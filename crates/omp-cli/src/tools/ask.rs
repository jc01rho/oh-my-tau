//! `ask`: ask the user a question during execution. Port of omp `tools/ask.ts`.
//!
//! omp shows questions through the UI's rich ask dialog. In RPC UI mode that is
//! one `extension_ui_request` frame with `method: "ask"` carrying the questions,
//! answered by an `extension_ui_response` with `answers` (or `cancelled`, with
//! `timedOut` when the deadline passed). The frame transport lives in the host
//! and is injected through [`AskUi`]. Without a UI, the tool fails with omp's
//! headless error (`Ask tool requires interactive mode`); omp's `-p` mode
//! never registers the tool, so this is only reached by a misconfigured host.
//!
//! Upstream `ask` takes one `html` string holding a `<form>` (`askSchema`
//! `{html: string}`) and returns the submitted form data as JSON. This port
//! accepts that shape: [`parse_html_form`] flattens the form's named controls
//! into the question list the dialog and renderer already speak (`<legend>` /
//! `<label>` text becomes the question, radio/checkbox labels become options, a
//! checkbox group is `multi`, `<textarea>` and text inputs become free-text
//! questions), and the dialog/result path below is unchanged. Arguments are
//! object-only (`{html}`), matching the schema; the pre-HTML `questions[]`
//! shape is kept as a fallback (omp `recoverAskHtml`).
//!
//! Divergences from upstream, all host-driven: an option answers with its label
//! (no `value`), per-field validation (`required`, `pattern`), image answers and
//! the JSON form-data result text are not ported, and the model-facing schema
//! advertises both shapes with no `required` list where omp requires `html` and
//! relies on lenient argument validation.
//!
//! Not ported yet: images in answers, and `chat` results.

use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use pa_agent::abort::{is_abort_error, race_with_abort, AbortSignal};
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolResultContent};
use pa_agent::BoxFut;
use serde::Deserialize;
use serde_json::{json, Map, Value};

const DESCRIPTION: &str = r#"Ask the user by sending one HTML `<form>` in `html`: named `<input>`, `<select>` and `<textarea>` controls, `type="radio"`/`checkbox` for choices, `<legend>`/`<label>` for the question text, `checked`/`selected` for preselecting. Legacy `questions[]` (`{id, question, options: [{label, description?}], multi?, recommended?}`) is still accepted.

<instruction>
- Several viable choices: pick conservative/standard, proceed, state choice.
- Batch related questions; 2–5 distinct options each; short labels, tradeoffs in `description`.
- `recommended` auto-adds " (Recommended)"; `multi: true` permits multiple selections.
- NEVER supply "Other": UI adds "Other (type your own)".
</instruction>"#;

/// Labels the runtime adds to every question; options may not reuse them.
const RESERVED_LABELS: [&str; 3] = ["Other (type your own)", "Chat about this", "Next →"];

/// Rows the runtime appends to the degraded select prompts (omp `OTHER_OPTION`
/// and `getDoneOptionLabel()` with the default theme's success symbol).
const OTHER_OPTION_LABEL: &str = "Other (type your own)";
const DONE_SELECTING_LABEL: &str = "\u{2714} Done selecting";
const RECOMMENDED_SUFFIX: &str = " (Recommended)";

/// One selectable row in the degraded `select` prompt. The host renders the
/// fields; the tool only forwards them.
#[allow(dead_code)] // consumed by the host UI implementation
#[derive(Debug, Clone)]
pub struct SelectOption {
    pub label: String,
    pub description: Option<String>,
}

fn select_option(label: &str) -> SelectOption {
    SelectOption {
        label: label.to_string(),
        description: None,
    }
}

/// Host transport for the rich ask dialog (omp `extensionUi`).
pub trait AskUi: Send + Sync {
    /// Present `questions` (the frame's `questions` array) with an optional
    /// deadline in milliseconds, and resolve with the answer frame: the
    /// `extension_ui_response` object (`answers`, or `cancelled` + `timedOut`).
    /// `Ok(None)` means the host could not deliver a response at all.
    fn ask(
        &self,
        questions: Value,
        timeout_ms: Option<u64>,
    ) -> BoxFut<'static, Result<Option<Value>>>;

    /// Whether the host provides the rich ask dialog (omp
    /// `extensionUi.askDialog`). When `false`, the tool degrades to the
    /// [`AskUi::select`]/[`AskUi::editor`] prompts.
    fn has_ask_dialog(&self) -> bool {
        true
    }

    /// Show a select prompt; resolves with the picked label, `Ok(None)` on
    /// cancel. `checked` are the pre-checked option indices (multi-select);
    /// `initial_index` is the cursor start.
    fn select(
        &self,
        prompt: String,
        options: Vec<SelectOption>,
        multi: bool,
        checked: Vec<usize>,
        initial_index: Option<usize>,
    ) -> BoxFut<'static, Result<Option<String>>> {
        let _ = (prompt, options, multi, checked, initial_index);
        Box::pin(async { Err(anyhow!("select is not supported by this host")) })
    }

    /// Show a single-line editor prompt; resolves with the typed text,
    /// `Ok(None)` on cancel.
    fn editor(&self, title: String) -> BoxFut<'static, Result<Option<String>>> {
        let _ = title;
        Box::pin(async { Err(anyhow!("editor is not supported by this host")) })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OptionItem {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct QuestionItem {
    id: String,
    question: String,
    #[serde(default)]
    header: Option<String>,
    options: Vec<OptionItem>,
    #[serde(default)]
    multi: Option<bool>,
    #[serde(default)]
    recommended: Option<f64>,
    /// Every pre-checked option index (multi-select defaults); `recommended`
    /// keeps carrying the first one for the single-select paths.
    #[serde(default)]
    checked: Vec<usize>,
}

/// Legacy pre-HTML argument shape (omp `recoverAskHtml`'s `questions` branch).
#[derive(Debug, Deserialize)]
struct Params {
    questions: Vec<QuestionItem>,
}

/// omp's invalid-arguments text.
const INVALID_ASK_ERROR: &str = "Error: ask takes one `html` string holding a <form>.";
/// omp's empty-form text (a result, not a tool failure).
const NO_NAMED_CONTROLS_ERROR: &str = "Error: the form has no named controls. Put at least one <input>, <select> or <textarea> with a `name` attribute inside the <form>.";

// =============================================================================
// HTML form parsing (omp `parseHtmlForm`, flattened onto the question model)
// =============================================================================

/// One token of the form markup: a text run or a tag.
enum Token {
    Text(String),
    Tag(HtmlTag),
}

/// A tag's lowercased name and its attributes (entity-decoded values).
#[derive(Debug, Default)]
struct HtmlTag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

impl HtmlTag {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn has(&self, key: &str) -> bool {
        self.attr(key).is_some()
    }
}

/// A form field being built: its name, question text and (for choice fields)
/// its options.
#[derive(Debug, Default)]
struct FieldDraft {
    name: String,
    question: Option<String>,
    multi: bool,
    choice: bool,
    options: Vec<OptDraft>,
}

#[derive(Debug, Default)]
struct OptDraft {
    label: String,
    value: String,
    checked: bool,
}

/// Text being collected for the tag whose content is still open.
enum Capture {
    Legend(String),
    Label {
        text: String,
        /// `for="..."` target element id, if the label names one.
        for_target: Option<String>,
    },
    Option {
        field: usize,
        index: usize,
        text: String,
    },
}

/// Index of the `>` that closes a tag, skipping quoted attribute values.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(index),
            None => {}
        }
    }
    None
}

/// Decode the handful of HTML entities a form label can carry.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let entity = [
            "&amp;", "&lt;", "&gt;", "&quot;", "&#39;", "&apos;", "&nbsp;",
        ]
        .into_iter()
        .find(|entity| tail.starts_with(*entity));
        match entity {
            Some("&amp;") => out.push('&'),
            Some("&lt;") => out.push('<'),
            Some("&gt;") => out.push('>'),
            Some("&quot;") => out.push('"'),
            Some("&#39;" | "&apos;") => out.push('\''),
            Some("&nbsp;") => out.push(' '),
            _ => out.push('&'),
        }
        rest = &tail[entity.map_or(1, str::len)..];
    }
    out.push_str(rest);
    out
}

fn tokenize(html: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        if pos > 0 {
            tokens.push(Token::Text(decode_entities(&rest[..pos])));
        }
        let after = &rest[pos..];
        if let Some(comment) = after.strip_prefix("<!--") {
            let end = comment.find("-->").map_or(after.len(), |i| i + 7);
            rest = &after[end.min(after.len())..];
            continue;
        }
        if after.starts_with("<!") || after.starts_with("<?") {
            let Some(end) = tag_end(after) else { break };
            rest = &after[end + 1..];
            continue;
        }
        let Some(end) = tag_end(after) else { break };
        let tag = parse_tag(&after[1..end]);
        // `<textarea>` content is raw text: no markup is recognized until
        // `</textarea>`, so a stray `<` in the body cannot become a control.
        let raw_text = matches!(&tag, Some(tag) if !tag.closing && tag.name == "textarea");
        if let Some(tag) = tag {
            tokens.push(Token::Tag(tag));
        }
        rest = &after[end + 1..];
        if raw_text {
            let lowered = rest.to_ascii_lowercase();
            let body_end = lowered.find("</textarea").unwrap_or(rest.len());
            rest = match rest[body_end..].find('>') {
                Some(close) => &rest[body_end + close + 1..],
                None => "",
            };
        }
    }
    if !rest.is_empty() {
        tokens.push(Token::Text(decode_entities(rest)));
    }
    tokens
}

fn parse_tag(inner: &str) -> Option<HtmlTag> {
    let inner = inner.trim();
    let closing = inner.starts_with('/');
    let mut body = inner.trim_start_matches('/').trim();
    if body.ends_with('/') {
        body = body[..body.len() - 1].trim_end();
    }
    let name_len = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = body[..name_len].to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let mut attrs = Vec::new();
    let mut rest = body[name_len..].trim_start();
    while !rest.is_empty() {
        let key_len = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let key = rest[..key_len].to_ascii_lowercase();
        rest = rest[key_len..].trim_start();
        let mut value = String::new();
        if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            match after_eq.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let quoted = &after_eq[1..];
                    let end = quoted.find(quote).unwrap_or(quoted.len());
                    value = decode_entities(&quoted[..end]);
                    rest = quoted[end..].strip_prefix(quote).unwrap_or("").trim_start();
                }
                _ => {
                    let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
                    value = decode_entities(&after_eq[..end]);
                    rest = after_eq[end..].trim_start();
                }
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    Some(HtmlTag {
        name,
        closing,
        attrs,
    })
}

/// Collapse whitespace in label/legend text (`spansText(...).replace(/\s+/g, " ")`).
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn field_index(
    drafts: &mut Vec<FieldDraft>,
    name: &str,
    legend: Option<&str>,
    choice: bool,
) -> usize {
    if let Some(index) = drafts.iter().position(|draft| draft.name == name) {
        if choice {
            drafts[index].choice = true;
        }
        if drafts[index].question.is_none() {
            drafts[index].question = legend.map(str::to_owned);
        }
        return index;
    }
    drafts.push(FieldDraft {
        name: name.to_owned(),
        question: legend.map(str::to_owned),
        multi: false,
        choice,
        options: Vec::new(),
    });
    drafts.len() - 1
}

/// Element `id`, used to resolve `<label for="...">`.
fn element_id(tag: &HtmlTag) -> Option<&str> {
    tag.attr("id").map(str::trim).filter(|id| !id.is_empty())
}

/// Record a legend on the innermost fieldset (index 0 is the root sentinel).
fn set_legend(legends: &mut [Option<String>], text: String) {
    if let Some(slot) = legends.last_mut() {
        *slot = Some(text);
    }
}

/// Finalize the open `<option>` body: an explicit `</option>` or the implicit
/// close before the next `<option>` / `</select>`.
fn close_option(drafts: &mut [FieldDraft], captures: &mut Vec<Capture>) {
    let pending = match captures.last() {
        Some(Capture::Option { field, index, text }) => Some((*field, *index, squash(text))),
        _ => None,
    };
    let Some((field, index, text)) = pending else {
        return;
    };
    if drafts[field].options[index].label.is_empty() {
        drafts[field].options[index].label = text;
    }
    captures.pop();
}

/// Fields of the form's named controls, in document order (omp `parseHtmlForm`
/// flattened onto the TUI question model).
#[allow(clippy::cast_precision_loss)] // `recommended` is omp's number-typed option index
fn parse_html_form(html: &str) -> Vec<QuestionItem> {
    let mut drafts: Vec<FieldDraft> = Vec::new();
    // Innermost fieldset's legend; index 0 is the sentinel for markup outside
    // any fieldset, so closing the outermost fieldset clears the legend and
    // closing a nested one restores its parent's.
    let mut legends: Vec<Option<String>> = vec![None];
    let mut captures: Vec<Capture> = Vec::new();
    let mut label_depth = 0_usize;
    let mut label_options: Vec<(usize, usize)> = Vec::new();
    let mut label_fields: Vec<usize> = Vec::new();
    let mut select_field: Option<usize> = None;
    // `for="..."` targets: element id -> field, and element id -> option slot.
    let mut id_fields: Vec<(String, usize)> = Vec::new();
    let mut id_options: Vec<(String, usize, usize)> = Vec::new();
    // `<label for="...">` text, resolved once every element id is known (the
    // label may precede or follow the element it names).
    let mut for_labels: Vec<(String, String)> = Vec::new();
    let tokens = tokenize(html);
    // Only controls inside `<form>` count. A fragment with no `<form>` at all
    // is read as the form body, which omp's `recoverAskHtml` tolerates.
    // Detection goes through the parsed tokens: comments are dropped and
    // `<textarea>` bodies stay raw text, so a `<form` inside either is not a
    // real form boundary.
    let has_form = tokens
        .iter()
        .any(|token| matches!(token, Token::Tag(tag) if tag.name == "form"));
    let mut form_depth = 0_usize;

    for token in tokens {
        let tag = match token {
            Token::Text(text) => {
                if let Some(capture) = captures.last_mut() {
                    match capture {
                        Capture::Legend(buffer) | Capture::Label { text: buffer, .. } => {
                            buffer.push_str(&text);
                        }
                        Capture::Option { text: buffer, .. } => buffer.push_str(&text),
                    }
                }
                continue;
            }
            Token::Tag(tag) => tag,
        };

        // Form boundaries: controls outside a `<form>` are not collected.
        if tag.name == "form" {
            form_depth = if tag.closing {
                form_depth.saturating_sub(1)
            } else {
                form_depth + 1
            };
            continue;
        }
        if has_form && form_depth == 0 {
            continue;
        }

        if tag.closing {
            match tag.name.as_str() {
                "legend" => {
                    if let Some(Capture::Legend(text)) = captures.last() {
                        let text = squash(text);
                        captures.pop();
                        if !text.is_empty() {
                            set_legend(&mut legends, text);
                        }
                    }
                }
                // A legend titles only its own fieldset.
                "fieldset" => {
                    if legends.len() > 1 {
                        legends.pop();
                    } else if let Some(slot) = legends.last_mut() {
                        *slot = None;
                    }
                }
                "label" => {
                    label_depth = label_depth.saturating_sub(1);
                    let pending = match captures.last() {
                        Some(Capture::Label { text, for_target }) => {
                            Some((squash(text), for_target.clone()))
                        }
                        _ => None,
                    };
                    if let Some((text, for_target)) = pending {
                        captures.pop();
                        let question = text.trim_end_matches([':', '*']).trim().to_owned();
                        let mut targets: Vec<(usize, Option<usize>)> = label_options
                            .drain(..)
                            .map(|(field, index)| (field, Some(index)))
                            .collect();
                        targets.extend(label_fields.drain(..).map(|field| (field, None)));
                        if let Some(id) = for_target {
                            for_labels.push((id, text.clone()));
                        }
                        for (field, option) in targets {
                            match option {
                                Some(index) if drafts[field].options[index].label.is_empty() => {
                                    drafts[field].options[index].label = text.clone();
                                }
                                None if drafts[field].question.is_none()
                                    && !question.is_empty() =>
                                {
                                    drafts[field].question = Some(question.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "option" => close_option(&mut drafts, &mut captures),
                "select" => {
                    close_option(&mut drafts, &mut captures);
                    select_field = None;
                }
                _ => {}
            }
            continue;
        }

        let legend = legends.last().cloned().flatten();
        let legend = legend.as_deref();

        match tag.name.as_str() {
            "fieldset" => legends.push(None),
            "legend" => captures.push(Capture::Legend(String::new())),
            "label" => {
                label_depth += 1;
                captures.push(Capture::Label {
                    text: String::new(),
                    for_target: tag
                        .attr("for")
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned),
                });
            }
            "input" => {
                let Some(name) = tag.attr("name").map(str::trim).filter(|n| !n.is_empty()) else {
                    continue;
                };
                let kind = tag
                    .attr("type")
                    .map(|t| t.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "text".to_owned());
                match kind.as_str() {
                    "hidden" | "submit" | "button" | "reset" | "image" | "file" => {}
                    "radio" | "checkbox" => {
                        let field = field_index(&mut drafts, name, legend, true);
                        if kind == "checkbox" {
                            drafts[field].multi = true;
                        }
                        let index = drafts[field].options.len();
                        drafts[field].options.push(OptDraft {
                            label: String::new(),
                            value: tag.attr("value").unwrap_or(name).to_owned(),
                            checked: tag.has("checked"),
                        });
                        if label_depth > 0 {
                            label_options.push((field, index));
                        }
                        if let Some(id) = element_id(&tag) {
                            id_options.push((id.to_owned(), field, index));
                        }
                    }
                    _ => {
                        let field = field_index(&mut drafts, name, legend, false);
                        if label_depth > 0 {
                            label_fields.push(field);
                        }
                        if let Some(id) = element_id(&tag) {
                            id_fields.push((id.to_owned(), field));
                        }
                    }
                }
            }
            "textarea" => {
                if let Some(name) = tag.attr("name").map(str::trim).filter(|n| !n.is_empty()) {
                    let field = field_index(&mut drafts, name, legend, false);
                    if label_depth > 0 {
                        label_fields.push(field);
                    }
                    if let Some(id) = element_id(&tag) {
                        id_fields.push((id.to_owned(), field));
                    }
                }
            }
            "select" => {
                select_field = tag
                    .attr("name")
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(|name| {
                        let field = field_index(&mut drafts, name, legend, true);
                        if tag.has("multiple") {
                            drafts[field].multi = true;
                        }
                        if label_depth > 0 {
                            label_fields.push(field);
                        }
                        if let Some(id) = element_id(&tag) {
                            id_fields.push((id.to_owned(), field));
                        }
                        field
                    });
            }
            "option" => {
                close_option(&mut drafts, &mut captures);
                let Some(field) = select_field else { continue };
                if tag.has("disabled") {
                    continue;
                }
                let index = drafts[field].options.len();
                drafts[field].options.push(OptDraft {
                    label: String::new(),
                    value: tag.attr("value").unwrap_or("").to_owned(),
                    checked: tag.has("selected"),
                });
                captures.push(Capture::Option {
                    field,
                    index,
                    text: String::new(),
                });
            }
            _ => {}
        }
    }

    // Resolve `<label for="id">` against the collected element ids.
    for (id, text) in for_labels {
        let question = text.trim_end_matches([':', '*']).trim().to_owned();
        for (target, field, index) in &id_options {
            if *target == id && drafts[*field].options[*index].label.is_empty() {
                drafts[*field].options[*index].label = text.clone();
            }
        }
        for (target, field) in &id_fields {
            if *target == id && drafts[*field].question.is_none() && !question.is_empty() {
                drafts[*field].question = Some(question.clone());
            }
        }
    }

    drafts
        .into_iter()
        .map(|mut draft| {
            let question = draft.question.take().unwrap_or_else(|| draft.name.clone());
            // Every checked option, not just the first: multi-select defaults
            // keep all of them.
            let checked: Vec<usize> = draft
                .options
                .iter()
                .enumerate()
                .filter(|(_, option)| option.checked)
                .map(|(index, _)| index)
                .collect();
            let recommended = checked.first().copied();
            let options: Vec<OptionItem> = if draft.choice {
                draft
                    .options
                    .into_iter()
                    .map(|option| OptionItem {
                        label: if option.label.is_empty() {
                            option.value
                        } else {
                            option.label
                        },
                        description: None,
                        preview: None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            QuestionItem {
                id: draft.name,
                question,
                header: None,
                options,
                multi: draft.multi.then_some(true),
                recommended: recommended.map(|index| index as f64),
                checked,
            }
        })
        .collect()
}

// Question list for one call: `{html}`, a bare html string, or the legacy
// `questions[]` array (omp `recoverAskHtml`).

/// One answered question (omp `QuestionResult`).
#[derive(Debug, Clone, PartialEq)]
struct QuestionResult {
    id: String,
    question: String,
    options: Vec<String>,
    multi: bool,
    selected: Vec<String>,
    custom: Option<String>,
    timed_out: bool,
}

/// The `ask` tool.
pub struct AskTool {
    schema: Value,
    ui: Option<Arc<dyn AskUi>>,
}

/// The `ask` tool without a UI: every call fails with the headless error.
#[must_use]
pub fn tool() -> Arc<dyn AgentTool> {
    Arc::new(AskTool {
        schema: schema(),
        ui: None,
    })
}

/// The `ask` tool wired to a host dialog transport.
#[must_use]
#[allow(dead_code)] // host-transport variant, wired when a dialog UI backend is present
pub fn tool_with_ui(ui: Arc<dyn AskUi>) -> Arc<dyn AgentTool> {
    Arc::new(AskTool {
        schema: schema(),
        ui: Some(ui),
    })
}

fn parse_params(raw: Value) -> Result<Vec<QuestionItem>> {
    // Dispatch is object-only (`{html}` or `{questions[]}`), as the schema
    // declares; the harness validates arguments against it before the call, so
    // any other shape is rejected here instead of being coerced.
    let questions = if let Some(html) = raw.get("html").and_then(Value::as_str) {
        if html.trim().is_empty() {
            return Err(anyhow!(INVALID_ASK_ERROR));
        }
        parse_html_form(html)
    } else if raw.get("questions").is_some() {
        let params: Params = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Invalid ask arguments: {e}"))?;
        if params.questions.is_empty() {
            return Err(anyhow!(
                "Invalid ask arguments: questions must contain at least 1 item"
            ));
        }
        params.questions
    } else {
        return Err(anyhow!(INVALID_ASK_ERROR));
    };
    for question in &questions {
        if let Some(reserved) = question
            .options
            .iter()
            .find(|o| RESERVED_LABELS.contains(&o.label.as_str()))
        {
            return Err(anyhow!(
                "Invalid ask arguments: option labels must not collide with reserved runtime labels: {}",
                reserved.label
            ));
        }
    }
    Ok(questions)
}

/// Text-level validation omp reports as a non-error result (`Error: ...`).
fn validate_questions(questions: &[QuestionItem]) -> Option<String> {
    let mut seen_ids: Vec<&str> = Vec::new();
    for question in questions {
        if seen_ids.contains(&question.id.as_str()) {
            return Some(format!(
                "Error: question ids must be unique: {}",
                json!(question.id)
            ));
        }
        seen_ids.push(&question.id);
        let mut seen_labels: Vec<&str> = Vec::new();
        for option in &question.options {
            if seen_labels.contains(&option.label.as_str()) {
                return Some(format!(
                    "Error: option labels must be unique within a question: {}",
                    json!(option.label)
                ));
            }
            seen_labels.push(&option.label);
        }
    }
    None
}

fn trimmed_nonempty(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// One question in the `ask` frame (omp `ExtensionAskDialogQuestion`).
fn frame_question(question: &QuestionItem) -> Value {
    let options: Vec<Value> = question
        .options
        .iter()
        .map(|option| {
            let mut obj = Map::new();
            obj.insert("label".into(), json!(option.label));
            if let Some(description) = trimmed_nonempty(option.description.as_ref()) {
                obj.insert("description".into(), json!(description));
            }
            if let Some(preview) = trimmed_nonempty(option.preview.as_ref()) {
                obj.insert("preview".into(), json!(preview));
            }
            Value::Object(obj)
        })
        .collect();
    let mut obj = Map::new();
    obj.insert("id".into(), json!(question.id));
    obj.insert("question".into(), json!(question.question));
    if let Some(header) = trimmed_nonempty(question.header.as_ref()) {
        obj.insert("header".into(), json!(header));
    }
    obj.insert("options".into(), Value::Array(options));
    if let Some(multi) = question.multi {
        obj.insert("multi".into(), json!(multi));
    }
    if let Some(recommended) = question.recommended {
        obj.insert("recommended".into(), json!(recommended));
    }
    // Single-select defaults already travel as `recommended`; a multi-select
    // needs every pre-checked index.
    if question.multi == Some(true) && !question.checked.is_empty() {
        obj.insert("checked".into(), json!(question.checked));
    }
    Value::Object(obj)
}

/// omp `timedOutAskDialogResult`: each question takes its recommended option, else the first.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // index math on omp's clamped recommended option, values are small and range-validated
fn timed_out_results(questions: &[QuestionItem]) -> Vec<QuestionResult> {
    questions
        .iter()
        .map(|q| {
            let labels: Vec<String> = q.options.iter().map(|o| o.label.clone()).collect();
            let defaults: Vec<String> = if q.multi.unwrap_or(false) {
                q.checked
                    .iter()
                    .filter_map(|&index| labels.get(index).cloned())
                    .collect()
            } else {
                Vec::new()
            };
            let fallback = if labels.is_empty() {
                None
            } else {
                let last = labels.len() - 1;
                let index = q.recommended.unwrap_or(0.0).max(0.0).min(last as f64) as usize;
                labels.get(index).cloned()
            };
            QuestionResult {
                id: q.id.clone(),
                question: q.question.clone(),
                options: labels,
                multi: q.multi.unwrap_or(false),
                selected: if defaults.is_empty() {
                    fallback.into_iter().collect()
                } else {
                    defaults
                },
                custom: None,
                timed_out: true,
            }
        })
        .collect()
}

/// omp `parseAskDialogResponse`: validates each answer against its question.
fn parse_answers(questions: &[QuestionItem], answers: &Value) -> Result<Vec<QuestionResult>> {
    let answers = answers
        .as_array()
        .filter(|a| a.len() == questions.len())
        .ok_or_else(|| {
            anyhow!(
                "Ask dialog response must carry {} answers in question order",
                questions.len()
            )
        })?;
    questions
        .iter()
        .zip(answers)
        .enumerate()
        .map(|(index, (question, answer))| {
            let id_ok = answer.get("id").and_then(Value::as_str) == Some(question.id.as_str());
            if !answer.is_object() || !id_ok {
                return Err(anyhow!(
                    "Ask dialog answer {index} must have id {}",
                    json!(question.id)
                ));
            }
            let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            let multi = question.multi.unwrap_or(false);
            let selected_raw = answer
                .get("selectedOptions")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    anyhow!(
                        "Ask dialog answer {} must carry a selectedOptions array",
                        json!(question.id)
                    )
                })?;
            let mut selected: Vec<String> = Vec::new();
            for label in selected_raw {
                let known = label.as_str().filter(|l| labels.contains(l));
                let Some(label) = known else {
                    return Err(anyhow!(
                        "Ask dialog answer {} selected unknown option {}",
                        json!(question.id),
                        label
                    ));
                };
                if selected.iter().any(|s| s == label) {
                    return Err(anyhow!(
                        "Ask dialog answer {} selected {} twice",
                        json!(question.id),
                        json!(label)
                    ));
                }
                selected.push(label.to_string());
            }
            let custom_raw = answer.get("customInput").filter(|v| !v.is_null());
            let custom = match custom_raw {
                None => None,
                Some(Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
                Some(_) => {
                    return Err(anyhow!(
                        "Ask dialog answer {} customInput must be a string",
                        json!(question.id)
                    ))
                }
            };
            if !multi && (selected.len() > 1 || (!selected.is_empty() && custom.is_some())) {
                return Err(anyhow!(
                    "Ask dialog answer {} is single-select but carries more than one answer",
                    json!(question.id)
                ));
            }
            Ok(QuestionResult {
                id: question.id.clone(),
                question: question.question.clone(),
                options: labels
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                multi,
                selected,
                custom,
                timed_out: false,
            })
        })
        .collect()
}

/// Interprets the host's answer frame. `Ok(None)` = cancelled by the user.
fn results_from_response(
    questions: &[QuestionItem],
    response: Option<Value>,
) -> Result<Option<Vec<QuestionResult>>> {
    let Some(frame) = response else {
        return Ok(None);
    };
    if frame.get("cancelled").and_then(Value::as_bool) == Some(true) {
        if frame.get("timedOut").and_then(Value::as_bool) == Some(true) {
            return Ok(Some(timed_out_results(questions)));
        }
        return Ok(None);
    }
    let answers = frame.get("answers").cloned().unwrap_or(Value::Null);
    parse_answers(questions, &answers).map(Some)
}

fn format_question_result(result: &QuestionResult) -> String {
    if let Some(custom) = &result.custom {
        return format!("{}: \"{custom}\"", result.id);
    }
    if !result.selected.is_empty() {
        let suffix = if result.timed_out {
            " (auto-selected after timeout)"
        } else {
            ""
        };
        return if result.multi {
            format!("{}: [{}]{suffix}", result.id, result.selected.join(", "))
        } else {
            format!("{}: {}{suffix}", result.id, result.selected[0])
        };
    }
    if result.multi {
        format!("{}: []", result.id)
    } else {
        format!("{}: (cancelled)", result.id)
    }
}

fn indent_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text for a single question (omp `formatSingleQuestionResponse`).
fn format_single_response(result: &QuestionResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !result.selected.is_empty() {
        let text = if result.multi {
            format!("User selected: {}", result.selected.join(", "))
        } else {
            format!("User selected: {}", result.selected[0])
        };
        parts.push(if result.timed_out {
            format!("{text} (auto-selected after timeout)")
        } else {
            text
        });
    }
    if let Some(custom) = &result.custom {
        parts.push(if custom.contains('\n') {
            format!("User provided custom input:\n{}", indent_lines(custom))
        } else {
            format!("User provided custom input: {custom}")
        });
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    if result.multi {
        "User did not select any options".to_string()
    } else {
        "User cancelled the selection".to_string()
    }
}

fn result_json(result: &QuestionResult) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(result.id));
    obj.insert("question".into(), json!(result.question));
    obj.insert("options".into(), json!(result.options));
    obj.insert("multi".into(), json!(result.multi));
    obj.insert("selectedOptions".into(), json!(result.selected));
    if let Some(custom) = &result.custom {
        obj.insert("customInput".into(), json!(custom));
    }
    if result.timed_out {
        obj.insert("timedOut".into(), json!(true));
    }
    Value::Object(obj)
}

fn cancelled() -> anyhow::Error {
    anyhow!("Ask tool was cancelled by the user")
}

/// Navigation controls for the degraded prompts (omp `NavigationControls`).
/// Arrow-key back/forward navigation is not supported by the [`AskUi`]
/// `select` seam, so degraded multi-question runs proceed sequentially.
#[derive(Debug, Clone, Default)]
struct Navigation {
    allow_forward: bool,
    progress: Option<String>,
}

/// Result of one degraded-prompt question (omp `SelectionResult`).
#[derive(Debug, Clone, Default)]
struct Selection {
    selected: Vec<String>,
    custom: Option<String>,
    cancelled: bool,
}

/// omp's clamping of the `recommended` index onto the option list; the count
/// is tiny, so the casts cannot lose information.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn clamp_recommended(recommended: f64, option_count: usize) -> Option<usize> {
    let last = option_count.checked_sub(1)?;
    (recommended.is_finite() && recommended >= 0.0 && recommended <= last as f64)
        .then_some(recommended as usize)
}

/// The label omp shows for a fallback option (recommended index badged).
fn display_label(option: &OptionItem, recommended: Option<usize>, index: usize) -> String {
    if recommended == Some(index) && !option.label.ends_with(RECOMMENDED_SUFFIX) {
        format!("{}{RECOMMENDED_SUFFIX}", option.label)
    } else {
        option.label.clone()
    }
}

impl AskTool {
    /// omp `askSingleQuestion` on the degraded select/editor path. The
    /// `ask.timeout` setting is not ported, so `timedOut` stays false and the
    /// timeout auto-selection never triggers. Multi-select loops until the
    /// user picks the "Done selecting" row, custom input, or cancels.
    #[allow(clippy::too_many_lines)] // faithful port of omp's branching ask loop
    async fn ask_single(
        ui: &dyn AskUi,
        question: &QuestionItem,
        signal: &AbortSignal,
        previous: Option<&QuestionResult>,
        mut navigation: Navigation,
    ) -> Result<Selection> {
        let multi = question.multi.unwrap_or(false);
        let mut selected: Vec<String> = previous.map(|p| p.selected.clone()).unwrap_or_default();
        // First pass: multi-select defaults are the form's `checked`/`selected`
        // options, all of them.
        if multi && previous.is_none() {
            selected = question
                .checked
                .iter()
                .filter_map(|&index| question.options.get(index))
                .map(|option| option.label.clone())
                .collect();
        }
        let mut custom: Option<String> = previous.and_then(|p| p.custom.clone());
        let base_prompt = match navigation.progress.take() {
            Some(progress) => format!("{} ({progress})", question.question),
            None => question.question.clone(),
        };
        let recommended = question
            .recommended
            .and_then(|r| clamp_recommended(r, question.options.len()));
        loop {
            let prompt = if multi && !selected.is_empty() {
                format!("({} selected) {base_prompt}", selected.len())
            } else {
                base_prompt.clone()
            };
            if multi {
                let mut options: Vec<SelectOption> = question
                    .options
                    .iter()
                    .map(|o| SelectOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                    })
                    .collect();
                if !navigation.allow_forward && !selected.is_empty() {
                    options.push(select_option(DONE_SELECTING_LABEL));
                }
                options.push(select_option(OTHER_OPTION_LABEL));
                let checked: Vec<usize> = question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| selected.contains(&o.label))
                    .map(|(i, _)| i)
                    .collect();
                let fut = ui.select(prompt.clone(), options, true, checked, None);
                match Self::race_static(fut, signal).await? {
                    None => {
                        return Ok(Selection {
                            selected,
                            custom,
                            cancelled: true,
                        });
                    }
                    Some(choice) if choice == DONE_SELECTING_LABEL => break,
                    Some(choice) if choice == OTHER_OPTION_LABEL => {
                        let fut = ui.editor(prompt);
                        if let Some(input) = Self::race_static(fut, signal).await? {
                            custom = Some(input);
                            break;
                        }
                    }
                    Some(label) => {
                        if let Some(pos) = selected.iter().position(|s| *s == label) {
                            selected.remove(pos);
                        } else {
                            selected.push(label);
                        }
                    }
                }
            } else {
                let mut options: Vec<SelectOption> = question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| SelectOption {
                        label: display_label(o, recommended, i),
                        description: o.description.clone(),
                    })
                    .collect();
                let initial = selected
                    .first()
                    .and_then(|first| question.options.iter().position(|o| &o.label == first))
                    .or(if custom.is_some() {
                        Some(question.options.len())
                    } else {
                        recommended
                    });
                options.push(select_option(OTHER_OPTION_LABEL));
                let fut = ui.select(prompt, options, false, Vec::new(), initial);
                match Self::race_static(fut, signal).await? {
                    None => {
                        return Ok(Selection {
                            selected,
                            custom,
                            cancelled: true,
                        });
                    }
                    Some(choice) if choice == OTHER_OPTION_LABEL => {
                        let fut = ui.editor(base_prompt.clone());
                        if let Some(input) = Self::race_static(fut, signal).await? {
                            custom = Some(input);
                            selected.clear();
                            break;
                        }
                    }
                    Some(display) => {
                        let picked = question
                            .options
                            .iter()
                            .enumerate()
                            .find(|(i, o)| display_label(o, recommended, *i) == display)
                            .map(|(_, o)| o.label.clone())
                            .or_else(|| {
                                display.strip_suffix(RECOMMENDED_SUFFIX).map(str::to_string)
                            })
                            .unwrap_or(display);
                        selected = vec![picked];
                        custom = None;
                        break;
                    }
                }
            }
        }
        Ok(Selection {
            selected,
            custom,
            cancelled: false,
        })
    }

    /// Await `fut`, mapping an abort into omp's `ToolAbortError` message.
    async fn race_static<T>(
        fut: impl Future<Output = Result<T>>,
        signal: &AbortSignal,
    ) -> Result<T> {
        match race_with_abort(fut, signal).await {
            Ok(inner) => inner,
            Err(err) if is_abort_error(&err) => Err(anyhow!("Ask input was cancelled")),
            Err(err) => Err(err),
        }
    }

    /// Degraded path (omp's non-`askDialog` branch): sequential per-question
    /// select prompts, multi-select via the "Done selecting" loop.
    async fn run_fallback(
        &self,
        ui: &dyn AskUi,
        questions: &[QuestionItem],
        signal: &AbortSignal,
    ) -> Result<AgentToolResult> {
        let mut results: Vec<QuestionResult> = Vec::with_capacity(questions.len());
        for (index, question) in questions.iter().enumerate() {
            let selection = Self::ask_single(
                ui,
                question,
                signal,
                results.get(index),
                Navigation::default(),
            )
            .await?;
            if selection.cancelled {
                return Err(cancelled());
            }
            results.push(QuestionResult {
                id: question.id.clone(),
                question: question.question.clone(),
                options: question.options.iter().map(|o| o.label.clone()).collect(),
                multi: question.multi.unwrap_or(false),
                selected: selection.selected,
                custom: selection.custom,
                timed_out: false,
            });
        }
        render_results(&results)
    }

    async fn run(&self, raw: Value, signal: &AbortSignal) -> Result<AgentToolResult> {
        let Some(ui) = self.ui.clone() else {
            return Err(anyhow!("Ask tool requires interactive mode"));
        };
        let questions = parse_params(raw)?;
        // omp reports a form with no named controls as text, not as a failure.
        if questions.is_empty() {
            return Ok(AgentToolResult::text(NO_NAMED_CONTROLS_ERROR));
        }
        if let Some(text) = validate_questions(&questions) {
            return Ok(AgentToolResult::text(text));
        }
        // Hosts without the rich dialog degrade to the select/editor prompts
        // (omp checks `extensionUi.askDialog` presence).
        if !ui.has_ask_dialog() {
            return self.run_fallback(ui.as_ref(), &questions, signal).await;
        }
        let frame: Vec<Value> = questions.iter().map(frame_question).collect();
        let fut = ui.ask(Value::Array(frame), None);
        let response = Self::race_static(fut, signal).await?;
        let Some(results) = results_from_response(&questions, response)? else {
            return Err(cancelled());
        };
        render_results(&results)
    }
}

/// Text + details for a finished ask round (rich dialog and degraded path).
fn render_results(results: &[QuestionResult]) -> Result<AgentToolResult> {
    if let [single] = results {
        if !single.timed_out
            && !single.multi
            && single.selected.is_empty()
            && single.custom.is_none()
        {
            return Err(cancelled());
        }
        let text = format_single_response(single);
        let details = json!({
            "question": single.question,
            "options": single.options,
            "multi": single.multi,
            "selectedOptions": single.selected,
            "customInput": single.custom,
            "timedOut": single.timed_out.then_some(true),
        });
        return Ok(result_with_text(text, details));
    }
    let text = format!(
        "User answers:\n{}",
        results
            .iter()
            .map(format_question_result)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let details = json!({"results": results.iter().map(result_json).collect::<Vec<_>>()});
    Ok(result_with_text(text, details))
}

fn result_with_text(text: String, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::text(text)],
        details: strip_nulls(details),
        terminate: None,
        is_error: None,
    }
}

/// Drop `null` members, as JSON serialization of omitted TS `undefined` fields does.
fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}

impl AgentTool for AskTool {
    fn name(&self) -> &'static str {
        "ask"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Ask"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params, &signal).await })
    }
}

/// Parameter schema: omp's `askSchema` (`{html: string}`) plus the legacy
/// `questions[]` shape it still recovers. omp marks `html` required and leans on
/// lenient argument validation for the legacy call; this crate validates
/// strictly, so neither shape is required here.
#[must_use]
pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "html":{"type":"string","description":"one HTML <form>"},
        "questions":{"type":"array","minItems":1,"items":{"type":"object","properties":{
            "id":{"type":"string"},
            "question":{"type":"string"},
            "header":{"type":"string","description":"display chip"},
            "options":{"type":"array","items":{"type":"object","properties":{
                "label":{"type":"string"},
                "description":{"type":"string"},
                "preview":{"type":"string","description":"rich preview"}
            },"required":["label"]}},
            "multi":{"type":"boolean"},
            "recommended":{"type":"number","description":"0-based default index"}
        },"required":["id","question","options"]}}
    }})
}
#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use pa_agent::abort::AbortController;

    use super::*;

    /// Host transport double: records the frame and answers with a fixed response.
    struct FakeUi {
        response: Option<Value>,
        frames: Mutex<Vec<(Value, Option<u64>)>>,
    }

    impl AskUi for FakeUi {
        fn ask(
            &self,
            questions: Value,
            timeout_ms: Option<u64>,
        ) -> BoxFut<'static, Result<Option<Value>>> {
            self.frames.lock().push((questions, timeout_ms));
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }
    }

    fn fake(response: Option<Value>) -> (Arc<FakeUi>, AskTool) {
        let ui = Arc::new(FakeUi {
            response,
            frames: Mutex::new(Vec::new()),
        });
        let tool = AskTool {
            schema: schema(),
            ui: Some(ui.clone()),
        };
        (ui, tool)
    }

    fn storage_args() -> Value {
        json!({"questions": [{
            "id": "storage", "question": "Database?",
            "options": [{"label": "SQLite"}, {"label": "Postgres", "description": "  shared  "}],
            "recommended": 0
        }]})
    }

    #[tokio::test]
    async fn headless_without_ui_fails_like_omp() {
        let err = AskTool {
            schema: schema(),
            ui: None,
        }
        .run(storage_args(), &AbortSignal::never())
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "Ask tool requires interactive mode");
    }

    #[tokio::test]
    async fn single_answer_sends_ask_frame_and_formats_result() {
        let (ui, tool) = fake(Some(
            json!({"answers": [{"id": "storage", "selectedOptions": ["SQLite"]}]}),
        ));
        let result = tool
            .run(storage_args(), &AbortSignal::never())
            .await
            .unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text("User selected: SQLite")]
        );
        assert_eq!(result.details["selectedOptions"], json!(["SQLite"]));
        assert!(result.details.get("timedOut").is_none());
        let frames = ui.frames.lock();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].0,
            json!([{
                "id": "storage", "question": "Database?",
                "options": [{"label": "SQLite"}, {"label": "Postgres", "description": "shared"}],
                "recommended": 0.0
            }])
        );
    }

    #[tokio::test]
    async fn cancel_and_empty_single_select_are_cancellations() {
        let (_, tool) = fake(Some(json!({"cancelled": true})));
        assert_eq!(
            tool.run(storage_args(), &AbortSignal::never())
                .await
                .unwrap_err()
                .to_string(),
            "Ask tool was cancelled by the user"
        );
        let (_, tool) = fake(Some(
            json!({"answers": [{"id": "storage", "selectedOptions": []}]}),
        ));
        assert_eq!(
            tool.run(storage_args(), &AbortSignal::never())
                .await
                .unwrap_err()
                .to_string(),
            "Ask tool was cancelled by the user"
        );
    }

    #[tokio::test]
    async fn timeout_auto_selects_recommended_option() {
        let (_, tool) = fake(Some(json!({"cancelled": true, "timedOut": true})));
        let mut args = storage_args();
        args["questions"][0]["recommended"] = json!(1);
        let result = tool.run(args, &AbortSignal::never()).await.unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(
                "User selected: Postgres (auto-selected after timeout)"
            )]
        );
        assert_eq!(result.details["timedOut"], json!(true));
    }

    #[tokio::test]
    async fn multi_question_answers_use_result_lines() {
        let args = json!({"questions": [
            {"id": "a", "question": "A?", "options": [{"label": "x"}, {"label": "y"}]},
            {"id": "b", "question": "B?", "multi": true, "options": [{"label": "p"}, {"label": "q"}]}
        ]});
        let response = json!({"answers": [
            {"id": "a", "selectedOptions": [], "customInput": "  mine  "},
            {"id": "b", "selectedOptions": ["p", "q"]}
        ]});
        let (_, tool) = fake(Some(response));
        let result = tool.run(args, &AbortSignal::never()).await.unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(
                "User answers:\na: \"mine\"\nb: [p, q]"
            )]
        );
        assert_eq!(result.details["results"][0]["customInput"], json!("mine"));
    }

    #[tokio::test]
    async fn invalid_answers_fail_closed() {
        let (_, tool) = fake(Some(
            json!({"answers": [{"id": "storage", "selectedOptions": ["Nope"]}]}),
        ));
        let err = tool
            .run(storage_args(), &AbortSignal::never())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Ask dialog answer \"storage\" selected unknown option \"Nope\""
        );
        let (_, tool) = fake(Some(json!({"answers": []})));
        assert!(tool
            .run(storage_args(), &AbortSignal::never())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reserved_labels_and_duplicate_ids_are_rejected() {
        let (_, tool) = fake(None);
        let reserved =
            json!({"questions": [{"id": "a", "question": "?", "options": [{"label": "Next →"}]}]});
        assert!(tool.run(reserved, &AbortSignal::never()).await.is_err());
        let dup = json!({"questions": [
            {"id": "a", "question": "?", "options": [{"label": "x"}]},
            {"id": "a", "question": "?", "options": [{"label": "y"}]}
        ]});
        let result = tool.run(dup, &AbortSignal::never()).await.unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(
                "Error: question ids must be unique: \"a\""
            )]
        );
    }

    const FORM_HTML: &str = r#"<form>
<fieldset><legend>Database?</legend>
<label><input type="radio" name="storage" value="sqlite" checked> SQLite</label>
<label><input type="radio" name="storage" value="postgres"> Postgres</label>
</fieldset>
<label>Notes: <textarea name="notes"></textarea></label>
</form>"#;

    #[tokio::test]
    async fn html_form_parses_into_ask_questions() {
        let (ui, tool) = fake(Some(json!({"answers": [
            {"id": "storage", "selectedOptions": ["SQLite"]},
            {"id": "notes", "selectedOptions": [], "customInput": "  hi  "}
        ]})));
        let result = tool
            .run(json!({"html": FORM_HTML}), &AbortSignal::never())
            .await
            .unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(
                "User answers:\nstorage: SQLite\nnotes: \"hi\""
            )]
        );
        let frames = ui.frames.lock();
        assert_eq!(
            frames[0].0,
            json!([
                {"id": "storage", "question": "Database?",
                 "options": [{"label": "SQLite"}, {"label": "Postgres"}],
                 "recommended": 0.0},
                {"id": "notes", "question": "Notes", "options": []}
            ])
        );
    }

    #[tokio::test]
    async fn html_checkbox_group_is_multi() {
        let html = r#"<form><fieldset><legend>Tags</legend>
            <label><input type="checkbox" name="tags" value="a"> Alpha</label>
            <label><input type="checkbox" name="tags" value="b"> Beta</label>
        </fieldset></form>"#;
        let (ui, tool) = fake(Some(json!({"answers": [
            {"id": "tags", "selectedOptions": ["Alpha", "Beta"]}
        ]})));
        let result = tool
            .run(json!({"html": html}), &AbortSignal::never())
            .await
            .unwrap();
        assert_eq!(result.details["selectedOptions"], json!(["Alpha", "Beta"]));
        assert_eq!(
            ui.frames.lock()[0].0,
            json!([{
                "id": "tags", "question": "Tags", "multi": true,
                "options": [{"label": "Alpha"}, {"label": "Beta"}]
            }])
        );
    }

    #[tokio::test]
    async fn html_without_named_controls_and_missing_html_are_reported() {
        let (_, tool) = fake(None);
        let result = tool
            .run(
                json!({"html": "<form><p>hello</p></form>"}),
                &AbortSignal::never(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(NO_NAMED_CONTROLS_ERROR)]
        );
        let err = tool
            .run(json!({"text": "no form"}), &AbortSignal::never())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), INVALID_ASK_ERROR);
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let (_, tool) = fake(None);
        let err = tool
            .run(
                Value::String("<form><input name=\"a\"></form>".to_owned()),
                &AbortSignal::never(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), INVALID_ASK_ERROR);
    }

    fn parsed(html: &str) -> Vec<QuestionItem> {
        parse_html_form(html)
    }

    fn ids(questions: &[QuestionItem]) -> Vec<&str> {
        questions.iter().map(|q| q.id.as_str()).collect()
    }

    fn labels(question: &QuestionItem) -> Vec<&str> {
        question.options.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn controls_outside_the_form_are_not_collected() {
        let html = r#"<input name="before"><form><input name="inside"></form><input name="after">"#;
        assert_eq!(ids(&parsed(html)), vec!["inside"]);
    }

    #[test]
    fn form_tags_in_comments_and_textarea_text_do_not_bound_the_fragment() {
        let html = r#"<!-- <form> --><input name="a"><textarea name="t"><form></textarea><input name="b">"#;
        assert_eq!(ids(&parsed(html)), vec!["a", "t", "b"]);
    }

    #[test]
    fn textarea_body_is_raw_text() {
        let html = r#"<form><label>Notes <textarea name="notes"><input name="trap"> & <b>x</b></textarea></label><input name="next"></form>"#;
        let questions = parsed(html);
        assert_eq!(ids(&questions), vec!["notes", "next"]);
        assert_eq!(questions[0].question, "Notes");
    }

    #[test]
    fn disabled_options_are_skipped_and_missing_close_ends_the_option() {
        let html = r#"<form><select name="s">
            <option value="a">Alpha
            <option value="b" disabled>Beta
            <option value="c">Gamma
        </select></form>"#;
        let questions = parsed(html);
        assert_eq!(ids(&questions), vec!["s"]);
        assert_eq!(labels(&questions[0]), vec!["Alpha", "Gamma"]);
    }

    #[test]
    fn nested_fieldset_legends_restore_the_parent() {
        let html = r#"<form>
            <fieldset><legend>Outer</legend>
                <fieldset><legend>Inner</legend><input name="a"></fieldset>
                <input name="b">
            </fieldset>
            <input name="c">
        </form>"#;
        let questions = parsed(html);
        assert_eq!(questions[0].question, "Inner");
        assert_eq!(questions[1].question, "Outer");
        assert_eq!(questions[2].question, "c");
    }

    #[test]
    fn labels_associate_with_selects_by_for_and_by_wrapping() {
        let html = r#"<form>
            <label for="region">Region</label>
            <select name="region" id="region"><option>EU</option><option>US</option></select>
            <label>Size: <select name="size"><option>S</option><option>M</option></select></label>
        </form>"#;
        let questions = parsed(html);
        assert_eq!(questions[0].question, "Region");
        assert_eq!(questions[1].question, "Size");
        assert_eq!(labels(&questions[1]), vec!["S", "M"]);
    }

    #[test]
    fn label_for_a_radio_sets_that_option_label() {
        let html = r#"<form><fieldset><legend>Pick</legend>
            <input type="radio" name="r" id="ra" value="a"><label for="ra">Alpha</label>
            <input type="radio" name="r" id="rb" value="b"><label for="rb">Beta</label>
        </fieldset></form>"#;
        let questions = parsed(html);
        assert_eq!(questions[0].question, "Pick");
        assert_eq!(labels(&questions[0]), vec!["Alpha", "Beta"]);
    }

    #[test]
    fn multi_select_defaults_carry_every_checked_index() {
        let html = r#"<form><select name="tags" multiple>
            <option selected>a</option><option>b</option><option selected>c</option>
        </select></form>"#;
        let questions = parsed(html);
        assert_eq!(questions[0].checked, vec![0, 2]);
        assert_eq!(questions[0].recommended, Some(0.0));
        assert_eq!(frame_question(&questions[0])["checked"], json!([0, 2]));
    }

    /// Degraded-path double: scripted select/editor answers, no rich dialog.
    struct ScriptedUi {
        selects: Mutex<Vec<Option<&'static str>>>,
        editors: Mutex<Vec<Option<&'static str>>>,
        calls: Mutex<Vec<String>>,
    }

    impl AskUi for ScriptedUi {
        fn ask(
            &self,
            _questions: Value,
            _timeout_ms: Option<u64>,
        ) -> BoxFut<'static, Result<Option<Value>>> {
            Box::pin(async { Err(anyhow!("rich dialog not expected")) })
        }
        fn has_ask_dialog(&self) -> bool {
            false
        }
        fn select(
            &self,
            prompt: String,
            _options: Vec<SelectOption>,
            _multi: bool,
            _checked: Vec<usize>,
            _initial_index: Option<usize>,
        ) -> BoxFut<'static, Result<Option<String>>> {
            self.calls.lock().push(prompt);
            let next = self.selects.lock().pop();
            Box::pin(async move { Ok(next.flatten().map(str::to_string)) })
        }
        fn editor(&self, title: String) -> BoxFut<'static, Result<Option<String>>> {
            self.calls.lock().push(title);
            let next = self.editors.lock().pop();
            Box::pin(async move { Ok(next.flatten().map(str::to_string)) })
        }
    }

    fn scripted(selects: Vec<Option<&'static str>>, editors: Vec<Option<&'static str>>) -> AskTool {
        AskTool {
            schema: schema(),
            ui: Some(Arc::new(ScriptedUi {
                selects: Mutex::new(selects.into_iter().rev().collect()),
                editors: Mutex::new(editors.into_iter().rev().collect()),
                calls: Mutex::new(Vec::new()),
            })),
        }
    }

    fn multi_args() -> Value {
        json!({"questions": [{
            "id": "m", "question": "Pick", "multi": true,
            "options": [{"label": "a"}, {"label": "b"}]
        }]})
    }

    #[tokio::test]
    async fn degraded_multi_select_loops_until_done() {
        let tool = scripted(
            vec![Some("a"), Some("b"), Some("\u{2714} Done selecting")],
            vec![],
        );
        let result = tool.run(multi_args(), &AbortSignal::never()).await.unwrap();
        assert_eq!(
            result.content,
            vec![ToolResultContent::text("User selected: a, b")]
        );
        assert_eq!(result.details["selectedOptions"], json!(["a", "b"]));
    }

    #[tokio::test]
    async fn degraded_multi_select_custom_input_ends_loop() {
        let tool = scripted(vec![Some("Other (type your own)")], vec![Some("write it")]);
        let result = tool.run(multi_args(), &AbortSignal::never()).await.unwrap();
        assert_eq!(result.details["customInput"], json!("write it"));
    }

    #[tokio::test]
    async fn degraded_select_cancel_is_error() {
        let tool = scripted(vec![None], vec![]);
        let err = tool
            .run(multi_args(), &AbortSignal::never())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Ask tool was cancelled by the user");
    }

    #[tokio::test]
    async fn degraded_single_select_maps_recommended_suffix_back() {
        let tool = AskTool {
            schema: schema(),
            ui: Some(Arc::new(ScriptedUi {
                selects: Mutex::new(vec![Some("b (Recommended)")]),
                editors: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            })),
        };
        let args = json!({"questions": [{
            "id": "s", "question": "Pick", "recommended": 1,
            "options": [{"label": "a"}, {"label": "b"}]
        }]});
        let result = tool.run(args, &AbortSignal::never()).await.unwrap();
        assert_eq!(result.details["selectedOptions"], json!(["b"]));
    }

    #[tokio::test]
    async fn abort_while_waiting_is_reported_as_cancelled_input() {
        let controller = AbortController::new();
        controller.abort();
        let tool = scripted(vec![Some("a")], vec![]);
        let err = tool
            .run(multi_args(), &controller.signal())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Ask input was cancelled");
    }
}
