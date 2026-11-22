//! omomeow-compatible tool names beside tau's own `ask` and `goal`:
//! `ask_user_question` and `create_goal` / `get_goal` / `update_goal`.
//!
//! The model-facing schemas are the ones the OmOMeow engine (mhc) advertises, so a
//! prompt written for it runs unchanged. The tools reuse tau's machinery instead of
//! duplicating it: questions that the host cannot show as a gateway `question` dialog
//! run through the `ask` tool, and goals live in the same [`GoalRuntime`] the `goal`
//! tool uses.
//!
//! Gateway contract (omo-nyang `dialogs.ts`, `brain.ts`):
//! - dialog frames are `method: "question"` with `title` and
//!   `questions[{id, question, options[{label, description?}]}]`; any other key is dropped,
//!   and a `method` outside `confirm|select|input|editor|question` never reaches the owner;
//! - the answer is a flat `extension_ui_response` whose `answers` is a map from question id to
//!   `{selected: [label], text?}`; `cancelled` and `comment` ride beside it, and a
//!   multi-question dialog is answered with `comment` alone.
//!
//! Goal status words are the gateway's: `active | paused | blocked | complete`.

use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use anyhow::{anyhow, Result};
use pa_agent::abort::{race_with_abort, AbortSignal};
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::ask::{self, AskUi, CANCELLED_MESSAGE};
use super::goal::{completion_budget_report, Goal, GoalRuntime};

/// Delivers a message to the session as if the user had typed it.
pub type FollowUpSink = Arc<dyn Fn(String) + Send + Sync>;

const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_HEADER_CHARS: usize = 12;
const MAX_OBJECTIVE_CHARS: usize = 4000;
/// Rows the runtime or the gateway adds to a question; an option may not reuse them.
/// The last one is the gateway's free-text escape (`dialogs.ts` `freeTextOption`).
const RESERVED_LABELS: [&str; 4] = [
    "Other (type your own)",
    "Chat about this",
    "Next →",
    "Other (type an answer)",
];
/// The gateway has no multi-select widget, so the question text carries the hint.
const MULTI_SELECT_HINT: &str = " (복수 선택 가능)";

const ASK_DESCRIPTION: &str = "Ask the user one to four questions and get their answers back.\n\nEach question has a short `header`, the `question`, two to four `options` (`label`, optional `description`) and `multiSelect`. The user can always answer in their own words instead of picking an option, so do not add an \"Other\" option.\n\n`waitForAnswer: true` (default) pauses until the user answers. `waitForAnswer: false` returns at once and the answer arrives later as a user message; keep working on what does not depend on it.\n\nSet `required: true` when the answer decides whether you take an action: a dismissed question is then reported as an error, so the action is not taken.";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A rejected call: `isError` result carrying `data.errorCode` (impl-13 error table).
fn coded_error(code: &str, message: impl Into<String>) -> AgentToolResult {
    let mut result = AgentToolResult::error(message);
    result.details = json!({ "errorCode": code });
    result
}

#[derive(Debug, Clone, Deserialize)]
struct AskOption {
    label: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskQuestion {
    header: String,
    question: String,
    #[serde(default)]
    multi_select: bool,
    options: Vec<AskOption>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskArgs {
    questions: Vec<AskQuestion>,
    #[serde(default = "default_wait")]
    wait_for_answer: bool,
    #[serde(default)]
    required: bool,
}

fn default_wait() -> bool {
    true
}

/// Parameter schema of `ask_user_question`.
#[must_use]
pub fn ask_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_QUESTIONS,
                "description": "Questions to ask the user (1-4 questions)",
                "items": {
                    "type": "object",
                    "properties": {
                        "header": {"type": "string", "minLength": 1, "maxLength": MAX_HEADER_CHARS, "description": "Very short label displayed as a chip/tag (max 12 chars)."},
                        "question": {"type": "string", "minLength": 1, "description": "The complete question to ask the user, ending with a question mark."},
                        "multiSelect": {"type": "boolean", "description": "Allow selecting several options instead of one."},
                        "options": {
                            "type": "array",
                            "minItems": MIN_OPTIONS,
                            "maxItems": MAX_OPTIONS,
                            "description": "The available choices (2-4); distinct and mutually exclusive unless multiSelect is set.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string", "minLength": 1, "description": "The text the user sees and selects (1-5 words)."},
                                    "description": {"type": "string", "description": "What choosing this option means or what will happen."}
                                },
                                "required": ["label"]
                            }
                        }
                    },
                    "required": ["header", "question", "options"]
                }
            },
            "waitForAnswer": {"type": "boolean", "description": "true (default): pause until the user answers. false: keep working and receive the answer later as a user message."},
            "required": {"type": "boolean", "description": "Set true when the answer decides whether you take an action: with no answer, that action is not taken."}
        },
        "required": ["questions"]
    })
}

fn question_id(index: usize) -> String {
    format!("q{}", index + 1)
}

/// The first reason `args` cannot be asked, if any.
fn validate(args: &AskArgs) -> Option<String> {
    let questions = &args.questions;
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Some(format!(
            "questions must hold 1 to {MAX_QUESTIONS} items, got {}",
            questions.len()
        ));
    }
    for (index, question) in questions.iter().enumerate() {
        let at = |what: &str| format!("questions[{index}]: {what}");
        let header = question.header.trim();
        if header.is_empty() || header.chars().count() > MAX_HEADER_CHARS {
            return Some(at(&format!("header must be 1 to {MAX_HEADER_CHARS} characters")));
        }
        if questions[..index].iter().any(|q| q.header.trim() == header) {
            return Some(at(&format!("header {header:?} is used twice")));
        }
        if question.question.trim().is_empty() {
            return Some(at("question must not be empty"));
        }
        let options = &question.options;
        if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
            return Some(at(&format!(
                "options must hold {MIN_OPTIONS} to {MAX_OPTIONS} items, got {}",
                options.len()
            )));
        }
        for (position, option) in options.iter().enumerate() {
            let label = option.label.trim();
            if label.is_empty() {
                return Some(at("option labels must not be empty"));
            }
            if RESERVED_LABELS.contains(&label) {
                return Some(at(&format!(
                    "option label {label:?} is reserved; the user can already answer in their own words"
                )));
            }
            if options[..position].iter().any(|o| o.label.trim() == label) {
                return Some(at(&format!("option label {label:?} is used twice")));
            }
        }
    }
    None
}

/// The gateway `question` dialog: `(title, questions)`. `header` and `multi` are extra
/// keys the gateway drops; the multi-select hint therefore also rides in the text.
fn gateway_dialog(questions: &[AskQuestion]) -> (String, Value) {
    let title = questions
        .iter()
        .map(|q| q.header.trim())
        .collect::<Vec<_>>()
        .join(" / ");
    let items = questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| {
                    let mut option = Map::new();
                    option.insert("label".into(), json!(o.label.trim()));
                    if let Some(d) = o.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                        option.insert("description".into(), json!(d));
                    }
                    Value::Object(option)
                })
                .collect();
            let hint = if q.multi_select { MULTI_SELECT_HINT } else { "" };
            json!({
                "id": question_id(index),
                "header": q.header.trim(),
                "question": format!("{}{hint}", q.question.trim()),
                "options": options,
                "multi": q.multi_select,
            })
        })
        .collect();
    (title, Value::Array(items))
}

/// What one question came back with.
#[derive(Debug, Clone, Default, PartialEq)]
struct Answer {
    selected: Vec<String>,
    other: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    /// Dismissed or timed out: the user gave nothing.
    Cancelled,
    Answered {
        answers: Vec<Answer>,
        /// Free text the gateway sends beside (or instead of) `answers`.
        comment: Option<String>,
    },
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Picks the labels of `question` out of `picked`; anything else is the user's own words.
fn split_picked(question: &AskQuestion, picked: Vec<String>, text: Option<String>) -> Answer {
    let (known, unknown): (Vec<String>, Vec<String>) = picked
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .partition(|p| question.options.iter().any(|o| o.label.trim() == p));
    let mut selected: Vec<String> = Vec::new();
    for label in known {
        if !selected.contains(&label) {
            selected.push(label);
        }
    }
    if !question.multi_select {
        selected.truncate(1);
    }
    let other = [unknown.join(", ").trim().to_string()]
        .into_iter()
        .chain(text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Answer {
        selected,
        other: (!other.is_empty()).then_some(other),
    }
}

/// Reads the `extension_ui_response` of a gateway `question` dialog. `answers` is the
/// gateway's id map (`{selected, text}`) or, from hosts speaking tau's own dialog, an
/// array of `{id, selectedOptions, customInput}`; a top-level `value` is a degraded
/// select answer to a single question.
fn parse_response(questions: &[AskQuestion], response: Option<Value>) -> Outcome {
    let Some(frame) = response else {
        return Outcome::Cancelled;
    };
    if frame.get("cancelled").and_then(Value::as_bool) == Some(true) {
        return Outcome::Cancelled;
    }
    let comment = nonempty(frame.get("comment"));
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let entry_of = |id: &str| -> Option<Value> {
        match frame.get("answers") {
            Some(Value::Object(map)) => map.get(id).cloned(),
            Some(Value::Array(list)) => list
                .iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
                .cloned(),
            _ => None,
        }
    };
    let single_value = (questions.len() == 1).then(|| nonempty(frame.get("value"))).flatten();
    let answers: Vec<Answer> = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            if let Some(entry) = entry_of(&question_id(index)) {
                let picked = if entry.get("selected").is_some() {
                    strings(entry.get("selected"))
                } else {
                    strings(entry.get("selectedOptions"))
                };
                let text = nonempty(entry.get("text").or_else(|| entry.get("customInput")));
                return split_picked(question, picked, text);
            }
            match &single_value {
                Some(value) => split_picked(question, vec![value.clone()], None),
                None => Answer::default(),
            }
        })
        .collect();
    let empty = answers.iter().all(|a| a.selected.is_empty() && a.other.is_none());
    if empty && comment.is_none() {
        return Outcome::Cancelled;
    }
    Outcome::Answered { answers, comment }
}

/// Reads what the `ask` tool reports in `details`: one question carries its fields
/// directly, several carry `results[]` in question order.
fn outcome_from_ask_details(questions: &[AskQuestion], details: &Value) -> Outcome {
    let one = |entry: &Value| Answer {
        selected: entry
            .get("selectedOptions")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        other: nonempty(entry.get("customInput")),
    };
    let answers = match details.get("results").and_then(Value::as_array) {
        Some(results) => results.iter().map(one).collect(),
        None => vec![one(details)],
    };
    debug_assert_eq!(answers.len(), questions.len());
    Outcome::Answered {
        answers,
        comment: None,
    }
}

/// Asks through tau's `ask` tool (rich ask dialog or select/editor prompts).
async fn ask_through_ask_tool(
    ui: Arc<dyn AskUi>,
    questions: &[AskQuestion],
    signal: &AbortSignal,
) -> Result<Outcome> {
    let legacy: Vec<Value> = questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| json!({"label": o.label.trim(), "description": o.description}))
                .collect();
            json!({
                "id": question_id(index),
                "header": q.header.trim(),
                "question": q.question.trim(),
                "options": options,
                "multi": q.multi_select,
            })
        })
        .collect();
    let no_update: AgentToolUpdateCallback = Arc::new(|_| {});
    match ask::tool_with_ui(ui)
        .execute(
            "ask_user_question".into(),
            json!({ "questions": legacy }),
            signal.clone(),
            no_update,
        )
        .await
    {
        Ok(result) => Ok(outcome_from_ask_details(questions, &result.details)),
        Err(e) if e.to_string() == CANCELLED_MESSAGE => Ok(Outcome::Cancelled),
        Err(e) => Err(e),
    }
}

async fn collect(
    ui: Arc<dyn AskUi>,
    questions: &[AskQuestion],
    signal: &AbortSignal,
) -> Result<Outcome> {
    if !ui.has_question_dialog() {
        return ask_through_ask_tool(ui, questions, signal).await;
    }
    let (title, items) = gateway_dialog(questions);
    let response = race_with_abort(ui.question(title, items, None), signal).await??;
    Ok(parse_response(questions, response))
}

/// The user-facing text of an outcome, with its structured twin.
struct Rendered {
    text: String,
    details: Value,
}

fn answer_line(question: &AskQuestion, answer: &Answer) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !answer.selected.is_empty() {
        parts.push(answer.selected.join(", "));
    }
    if let Some(other) = &answer.other {
        parts.push(format!("own answer: {other:?}"));
    }
    if parts.is_empty() {
        parts.push("(no answer)".to_string());
    }
    format!("- {}: {}", question.header.trim(), parts.join("; "))
}

fn render(id: &str, questions: &[AskQuestion], outcome: &Outcome) -> Rendered {
    match outcome {
        Outcome::Cancelled => Rendered {
            text: "The user dismissed the question without answering.".to_string(),
            details: json!({"questionId": id, "pending": false, "cancelled": true, "answers": []}),
        },
        Outcome::Answered { answers, comment } => {
            let mut lines = vec!["User answered:".to_string()];
            let mut details_answers: Vec<Value> = Vec::new();
            for (question, answer) in questions.iter().zip(answers) {
                lines.push(answer_line(question, answer));
                details_answers.push(json!({
                    "header": question.header.trim(),
                    "question": question.question.trim(),
                    "selected": answer.selected,
                    "other": answer.other,
                }));
            }
            if let Some(comment) = comment {
                lines.push(format!("User comment: {comment}"));
            }
            Rendered {
                text: lines.join("\n"),
                details: json!({
                    "questionId": id,
                    "pending": false,
                    "cancelled": false,
                    "answers": details_answers,
                    "comment": comment,
                }),
            }
        }
    }
}

fn finish(id: &str, required: bool, questions: &[AskQuestion], outcome: &Outcome) -> AgentToolResult {
    let rendered = render(id, questions, outcome);
    if required && *outcome == Outcome::Cancelled {
        let mut result = coded_error("question_cancelled", rendered.text);
        result.details = json!({
            "errorCode": "question_cancelled",
            "questionId": id,
            "cancelled": true,
        });
        return result;
    }
    AgentToolResult {
        details: rendered.details,
        ..AgentToolResult::text(rendered.text)
    }
}

fn next_question_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("askq-{nanos:x}")
}

/// The `ask_user_question` tool.
pub struct AskUserQuestionTool {
    schema: Value,
    ui: Option<Arc<dyn AskUi>>,
    follow_up: Option<FollowUpSink>,
}

/// `ask_user_question` without a host dialog: every call fails like `ask` does headless.
#[must_use]
pub fn ask_user_question_tool() -> Arc<dyn AgentTool> {
    Arc::new(AskUserQuestionTool {
        schema: ask_schema(),
        ui: None,
        follow_up: None,
    })
}

/// `ask_user_question` wired to a host dialog transport. With a `follow_up` sink,
/// `waitForAnswer: false` returns at once and the answer is delivered through it;
/// without one the call waits for the answer.
#[must_use]
pub fn ask_user_question_with_ui(
    ui: Arc<dyn AskUi>,
    follow_up: Option<FollowUpSink>,
) -> Arc<dyn AgentTool> {
    Arc::new(AskUserQuestionTool {
        schema: ask_schema(),
        ui: Some(ui),
        follow_up,
    })
}

impl AskUserQuestionTool {
    async fn run(&self, params: Value, signal: &AbortSignal) -> Result<AgentToolResult> {
        let Some(ui) = self.ui.clone() else {
            return Err(anyhow!("Ask tool requires interactive mode"));
        };
        let args: AskArgs = match serde_json::from_value(params) {
            Ok(args) => args,
            Err(e) => {
                return Ok(coded_error(
                    "invalid_arguments",
                    format!("Invalid ask_user_question arguments: {e}"),
                ))
            }
        };
        if let Some(problem) = validate(&args) {
            return Ok(coded_error(
                "invalid_arguments",
                format!("Invalid ask_user_question arguments: {problem}"),
            ));
        }
        let id = next_question_id();
        let sink = self.follow_up.clone().filter(|_| !args.wait_for_answer);
        let Some(sink) = sink else {
            let outcome = collect(ui, &args.questions, signal).await?;
            return Ok(finish(&id, args.required, &args.questions, &outcome));
        };
        // The turn may end before the user answers, so the dialog outlives this call.
        let pending_id = id.clone();
        tokio::spawn(async move {
            let message = match collect(ui, &args.questions, &AbortSignal::never()).await {
                Ok(outcome) => {
                    let result = finish(&pending_id, args.required, &args.questions, &outcome);
                    format!("[ask_user_question answer {pending_id}]\n{}", result_text(&result))
                }
                Err(e) => format!("[ask_user_question answer {pending_id}]\nThe question failed: {e}"),
            };
            sink(message);
        });
        Ok(AgentToolResult {
            details: json!({"questionId": id, "pending": true}),
            ..AgentToolResult::text(format!(
                "Question {id} was posted without waiting. The answer will arrive later as a user message starting with \"[ask_user_question answer {id}]\"; keep working on whatever does not depend on it."
            ))
        })
    }
}

fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            pa_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl AgentTool for AskUserQuestionTool {
    fn name(&self) -> &'static str {
        "ask_user_question"
    }
    fn description(&self) -> &str {
        ASK_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Ask User Question"
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

/// The goal state every compat goal tool of this process shares.
static SESSION_GOAL: LazyLock<Arc<Mutex<GoalRuntime>>> = LazyLock::new(Arc::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoalOp {
    Create,
    Get,
    Update,
}

impl GoalOp {
    fn name(self) -> &'static str {
        match self {
            Self::Create => "create_goal",
            Self::Get => "get_goal",
            Self::Update => "update_goal",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Create => "Start a goal for work that outlives this turn. Only when the session has no unfinished goal. `objective` is the outcome to reach (at most 4000 characters).",
            Self::Get => "Read the current goal: its objective, status (`active`, `paused`, `blocked` or `complete`) and token usage. Returns no goal when none exists.",
            Self::Update => "Set the goal's status. `complete`: only when the objective is met and every deliverable is verified against current evidence, never because budget is low or the turn is ending. `blocked`: you cannot continue without the user; `reason` is required and says what you are waiting for.",
        }
    }

    fn schema(self) -> Value {
        match self {
            Self::Create => json!({
                "type": "object",
                "properties": {
                    "objective": {"type": "string", "minLength": 1, "maxLength": MAX_OBJECTIVE_CHARS, "description": "The concrete objective to pursue."}
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
            Self::Get => json!({"type": "object", "properties": {}, "additionalProperties": false}),
            Self::Update => json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["complete", "blocked"], "description": "`complete` when achieved, `blocked` when you cannot continue without the user."},
                    "reason": {"type": "string", "description": "Required and non-empty when status is `blocked`; rejected when status is `complete`."}
                },
                "required": ["status"],
                "additionalProperties": false
            }),
        }
    }
}

/// The goal in the gateway's vocabulary.
fn goal_json(goal: &Goal) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(goal.id));
    obj.insert("objective".into(), json!(goal.objective));
    obj.insert("status".into(), json!(goal.wire_status()));
    if let Some(reason) = &goal.blocked_reason {
        obj.insert("reason".into(), json!(reason));
    }
    if let Some(budget) = goal.token_budget {
        obj.insert("tokenBudget".into(), json!(budget));
    }
    obj.insert("tokensUsed".into(), json!(goal.tokens_used));
    obj.insert("timeUsedSeconds".into(), json!(goal.time_used_seconds));
    obj.insert("createdAt".into(), json!(goal.created_at));
    obj.insert("updatedAt".into(), json!(goal.updated_at));
    Value::Object(obj)
}

fn goal_result(goal: Option<&Goal>, with_report: bool) -> AgentToolResult {
    let Some(goal) = goal else {
        return AgentToolResult {
            details: json!({"goal": null}),
            ..AgentToolResult::text("No active goal.")
        };
    };
    let remaining = goal.remaining_tokens();
    let report = if with_report {
        completion_budget_report(goal)
    } else {
        None
    };
    let mut lines = vec![
        format!("Goal: {}", goal.objective),
        format!("Status: {}", goal.wire_status()),
    ];
    if let Some(reason) = &goal.blocked_reason {
        lines.push(format!("Reason: {reason}"));
    }
    lines.push(match goal.token_budget {
        Some(budget) => format!("Tokens: {} used / {budget} budget", goal.tokens_used),
        None => format!("Tokens: {} used", goal.tokens_used),
    });
    if let Some(remaining) = remaining {
        lines.push(format!("Remaining tokens: {remaining}"));
    }
    let mut text = lines.join("\n");
    if let Some(report) = &report {
        text.push_str("\n\n");
        text.push_str(report);
    }
    AgentToolResult {
        details: json!({
            "goal": goal_json(goal),
            "remainingTokens": remaining,
            "completionBudgetReport": report,
        }),
        ..AgentToolResult::text(text)
    }
}

/// One of `create_goal`, `get_goal`, `update_goal`.
pub struct GoalCompatTool {
    op: GoalOp,
    schema: Value,
    runtime: Arc<Mutex<GoalRuntime>>,
}

/// The three goal tools over the session's shared goal state.
#[must_use]
pub fn goal_tools() -> Vec<Arc<dyn AgentTool>> {
    goal_tools_with(&SESSION_GOAL)
}

fn goal_tools_with(runtime: &Arc<Mutex<GoalRuntime>>) -> Vec<Arc<dyn AgentTool>> {
    [GoalOp::Create, GoalOp::Get, GoalOp::Update]
        .into_iter()
        .map(|op| {
            Arc::new(GoalCompatTool {
                op,
                schema: op.schema(),
                runtime: Arc::clone(runtime),
            }) as Arc<dyn AgentTool>
        })
        .collect()
}

impl GoalCompatTool {
    fn run(&self, params: &Value) -> AgentToolResult {
        let mut runtime = lock(&self.runtime);
        match self.op {
            GoalOp::Get => goal_result(runtime.state_goal(), false),
            GoalOp::Create => {
                let objective = params.get("objective").and_then(Value::as_str).unwrap_or("").trim();
                if objective.is_empty() || objective.chars().count() > MAX_OBJECTIVE_CHARS {
                    return coded_error(
                        "invalid_arguments",
                        format!("objective must be 1 to {MAX_OBJECTIVE_CHARS} characters"),
                    );
                }
                if runtime.state_goal().is_some_and(|g| g.wire_status() != "complete") {
                    return coded_error(
                        "goal_exists",
                        "cannot create a new goal because this session already has an unfinished goal",
                    );
                }
                match runtime.create_goal(objective, None) {
                    Ok(goal) => goal_result(Some(&goal), false),
                    Err(e) => coded_error("invalid_arguments", e.to_string()),
                }
            }
            GoalOp::Update => {
                let status = params.get("status").and_then(Value::as_str);
                let reason = params.get("reason").and_then(Value::as_str);
                let outcome = match (status, reason) {
                    (Some("complete"), Some(_)) => {
                        return coded_error("invalid_arguments", "reason is only allowed when status is blocked")
                    }
                    (Some("complete"), None) => runtime.complete_goal(),
                    (Some("blocked"), reason) => runtime.block_goal(reason.unwrap_or("")),
                    _ => {
                        return coded_error("invalid_arguments", "status must be \"complete\" or \"blocked\"")
                    }
                };
                match outcome {
                    Ok(goal) => goal_result(Some(&goal), status == Some("complete")),
                    Err(e) if runtime.state_goal().is_none() => coded_error("goal_not_found", e.to_string()),
                    Err(e) => coded_error("invalid_arguments", e.to_string()),
                }
            }
        }
    }
}

impl AgentTool for GoalCompatTool {
    fn name(&self) -> &'static str {
        self.op.name()
    }
    fn description(&self) -> &str {
        self.op.description()
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        match self.op {
            GoalOp::Create => "Create Goal",
            GoalOp::Get => "Get Goal",
            GoalOp::Update => "Update Goal",
        }
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { Ok(self.run(&params)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PlMutex;
    use pa_agent::types::ToolResultContent;

    /// Gateway-style host: records the `question` dialog, answers with a fixed response.
    struct GatewayUi {
        response: Option<Value>,
        dialogs: PlMutex<Vec<(String, Value)>>,
    }

    impl AskUi for GatewayUi {
        fn ask(&self, _: Value, _: Option<u64>) -> BoxFut<'static, Result<Option<Value>>> {
            Box::pin(async { Err(anyhow!("the gateway dialog must not use ask")) })
        }
        fn has_question_dialog(&self) -> bool {
            true
        }
        fn question(
            &self,
            title: String,
            questions: Value,
            _: Option<u64>,
        ) -> BoxFut<'static, Result<Option<Value>>> {
            self.dialogs.lock().push((title, questions));
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }
    }

    fn gateway(response: Option<Value>) -> (Arc<GatewayUi>, Arc<AskUserQuestionTool>) {
        let ui = Arc::new(GatewayUi {
            response,
            dialogs: PlMutex::new(Vec::new()),
        });
        let tool = Arc::new(AskUserQuestionTool {
            schema: ask_schema(),
            ui: Some(ui.clone()),
            follow_up: None,
        });
        (ui, tool)
    }

    /// tau's own rich dialog: answers the `ask` frame with an array.
    struct TauUi(Value);

    impl AskUi for TauUi {
        fn ask(&self, _: Value, _: Option<u64>) -> BoxFut<'static, Result<Option<Value>>> {
            let response = self.0.clone();
            Box::pin(async move { Ok(Some(response)) })
        }
    }

    /// A host with neither dialog: `ask` degrades to select prompts.
    struct SelectUi(&'static str);

    impl AskUi for SelectUi {
        fn ask(&self, _: Value, _: Option<u64>) -> BoxFut<'static, Result<Option<Value>>> {
            Box::pin(async { Err(anyhow!("no rich dialog")) })
        }
        fn has_ask_dialog(&self) -> bool {
            false
        }
        fn select(
            &self,
            _: String,
            _: Vec<ask::SelectOption>,
            _: bool,
            _: Vec<usize>,
            _: Option<usize>,
        ) -> BoxFut<'static, Result<Option<String>>> {
            let label = self.0.to_string();
            Box::pin(async move { Ok(Some(label)) })
        }
    }

    fn tool_with(ui: Arc<dyn AskUi>, follow_up: Option<FollowUpSink>) -> Arc<AskUserQuestionTool> {
        Arc::new(AskUserQuestionTool {
            schema: ask_schema(),
            ui: Some(ui),
            follow_up,
        })
    }

    async fn call(tool: &Arc<AskUserQuestionTool>, params: Value) -> Result<AgentToolResult> {
        tool.run(params, &AbortSignal::never()).await
    }

    fn storage() -> Value {
        json!({"questions": [{
            "header": "Storage",
            "question": "Which database?",
            "multiSelect": false,
            "options": [
                {"label": "SQLite", "description": "one file"},
                {"label": "Postgres"}
            ]
        }]})
    }

    fn two_questions() -> Value {
        json!({"questions": [
            {"header": "Storage", "question": "Which database?", "options": [{"label": "SQLite"}, {"label": "Postgres"}]},
            {"header": "Extras", "question": "Which extras?", "multiSelect": true, "options": [{"label": "Cache"}, {"label": "Queue"}, {"label": "Search"}]}
        ]})
    }

    fn text_of(result: &AgentToolResult) -> String {
        result_text(result)
    }

    #[test]
    fn schemas_match_the_omomeow_contract() {
        let ask = ask_schema();
        assert_eq!(ask["required"], json!(["questions"]));
        assert_eq!(ask["properties"]["questions"]["minItems"], 1);
        assert_eq!(ask["properties"]["questions"]["maxItems"], 4);
        let item = &ask["properties"]["questions"]["items"];
        assert_eq!(item["properties"]["header"]["maxLength"], 12);
        assert_eq!(item["properties"]["options"]["minItems"], 2);
        assert_eq!(item["properties"]["options"]["maxItems"], 4);
        assert_eq!(item["properties"]["options"]["items"]["required"], json!(["label"]));
        for key in ["header", "question", "multiSelect", "options"] {
            assert!(item["properties"].get(key).is_some(), "question key {key}");
        }
        assert!(ask["properties"].get("waitForAnswer").is_some());
        assert!(ask["properties"].get("required").is_some());

        let tools = goal_tools_with(&Arc::default());
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["create_goal", "get_goal", "update_goal"]);
        assert_eq!(tools[0].parameters()["required"], json!(["objective"]));
        assert_eq!(tools[1].parameters()["properties"], json!({}));
        let update = tools[2].parameters();
        assert_eq!(update["properties"]["status"]["enum"], json!(["complete", "blocked"]));
        assert_eq!(update["required"], json!(["status"]));
        assert_eq!(ask_user_question_tool().name(), "ask_user_question");
    }

    #[tokio::test]
    async fn without_a_host_dialog_the_tool_fails_like_ask() {
        let tool = Arc::new(AskUserQuestionTool {
            schema: ask_schema(),
            ui: None,
            follow_up: None,
        });
        let err = call(&tool, storage()).await.unwrap_err();
        assert_eq!(err.to_string(), "Ask tool requires interactive mode");
    }

    #[tokio::test]
    async fn invalid_arguments_are_reported_with_an_error_code() {
        let (ui, tool) = gateway(None);
        let one = |q: Value| json!({"questions": [q]});
        let opts2 = json!([{"label": "A"}, {"label": "B"}]);
        let cases = vec![
            json!({"questions": []}),
            json!({"questions": [1]}),
            json!({}),
            json!({"questions": vec![storage()["questions"][0].clone(); 5]}),
            one(json!({"header": "H", "question": "Q?", "options": [{"label": "A"}]})),
            one(json!({"header": "H", "question": "Q?", "options": [{"label": "A"}, {"label": "B"}, {"label": "C"}, {"label": "D"}, {"label": "E"}]})),
            one(json!({"header": "", "question": "Q?", "options": opts2})),
            one(json!({"header": "thirteen-chars", "question": "Q?", "options": opts2})),
            one(json!({"header": "H", "question": "  ", "options": opts2})),
            one(json!({"header": "H", "question": "Q?", "options": [{"label": "A"}, {"label": "A"}]})),
            one(json!({"header": "H", "question": "Q?", "options": [{"label": "A"}, {"label": "Other (type an answer)"}]})),
            one(json!({"header": "H", "question": "Q?", "options": [{"label": "A"}, {"label": "Chat about this"}]})),
            json!({"questions": [
                {"header": "Same", "question": "Q1?", "options": opts2},
                {"header": "Same", "question": "Q2?", "options": opts2}
            ]}),
        ];
        for case in cases {
            let result = call(&tool, case.clone()).await.unwrap();
            assert_eq!(result.is_error, Some(true), "{case}");
            assert_eq!(result.details["errorCode"], "invalid_arguments", "{case}");
        }
        assert!(ui.dialogs.lock().is_empty(), "an invalid call never opens a dialog");
    }

    #[tokio::test]
    async fn the_gateway_dialog_carries_question_ids_options_and_a_title() {
        let (ui, tool) = gateway(Some(json!({"answers": {"q1": {"selected": ["SQLite"]}}})));
        call(&tool, storage()).await.unwrap();
        let dialogs = ui.dialogs.lock();
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0].0, "Storage");
        assert_eq!(
            dialogs[0].1,
            json!([{
                "id": "q1", "header": "Storage", "question": "Which database?", "multi": false,
                "options": [{"label": "SQLite", "description": "one file"}, {"label": "Postgres"}]
            }])
        );
    }

    #[tokio::test]
    async fn multi_select_and_several_questions_shape_the_dialog() {
        let (ui, tool) = gateway(Some(json!({"answers": {}, "comment": "SQLite plus a cache"})));
        call(&tool, two_questions()).await.unwrap();
        let dialogs = ui.dialogs.lock();
        assert_eq!(dialogs[0].0, "Storage / Extras");
        let questions = dialogs[0].1.as_array().unwrap();
        assert_eq!(questions[0]["id"], "q1");
        assert_eq!(questions[1]["id"], "q2");
        assert_eq!(questions[1]["question"], "Which extras? (복수 선택 가능)");
        assert_eq!(questions[1]["multi"], true);
    }

    #[tokio::test]
    async fn a_gateway_answer_map_becomes_the_result() {
        let (_, tool) = gateway(Some(json!({"answers": {"q1": {"selected": ["SQLite"]}}})));
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(result.is_error, None);
        assert_eq!(text_of(&result), "User answered:\n- Storage: SQLite");
        assert_eq!(result.details["cancelled"], false);
        assert_eq!(result.details["pending"], false);
        assert_eq!(result.details["answers"][0]["header"], "Storage");
        assert_eq!(result.details["answers"][0]["selected"], json!(["SQLite"]));
        assert!(result.details["questionId"].as_str().is_some_and(|id| id.starts_with("askq-")));
    }

    #[tokio::test]
    async fn free_text_and_unknown_labels_are_the_users_own_answer() {
        let (_, tool) = gateway(Some(json!({"answers": {"q1": {"selected": [], "text": "DuckDB"}}})));
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(text_of(&result), "User answered:\n- Storage: own answer: \"DuckDB\"");
        assert_eq!(result.details["answers"][0]["selected"], json!([]));
        assert_eq!(result.details["answers"][0]["other"], "DuckDB");

        let (_, tool) = gateway(Some(json!({"answers": {"q1": {"selected": ["MySQL"]}}})));
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(result.details["answers"][0]["other"], "MySQL");
    }

    #[tokio::test]
    async fn a_multi_question_dialog_answered_by_comment_alone_keeps_the_comment() {
        let (_, tool) = gateway(Some(json!({"answers": {}, "comment": "SQLite plus a cache"})));
        let result = call(&tool, two_questions()).await.unwrap();
        assert_eq!(
            text_of(&result),
            "User answered:\n- Storage: (no answer)\n- Extras: (no answer)\nUser comment: SQLite plus a cache"
        );
        assert_eq!(result.details["comment"], "SQLite plus a cache");
    }

    #[tokio::test]
    async fn multi_select_keeps_every_known_label_in_order() {
        let (_, tool) = gateway(Some(json!({"answers": {
            "q1": {"selected": ["Postgres"]},
            "q2": {"selected": ["Queue", "Cache", "Queue"]}
        }})));
        let result = call(&tool, two_questions()).await.unwrap();
        assert_eq!(
            text_of(&result),
            "User answered:\n- Storage: Postgres\n- Extras: Queue, Cache"
        );
    }

    #[tokio::test]
    async fn a_degraded_select_value_answers_a_single_question() {
        let (_, tool) = gateway(Some(json!({"value": "Postgres"})));
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(result.details["answers"][0]["selected"], json!(["Postgres"]));
    }

    #[tokio::test]
    async fn a_dismissed_question_is_a_result_unless_required() {
        for response in [Some(json!({"cancelled": true})), None, Some(json!({"answers": {}}))] {
            let (_, tool) = gateway(response);
            let result = call(&tool, storage()).await.unwrap();
            assert_eq!(result.is_error, None);
            assert_eq!(result.details["cancelled"], true);
            assert_eq!(text_of(&result), "The user dismissed the question without answering.");
        }
        let (_, tool) = gateway(Some(json!({"cancelled": true})));
        let mut params = storage();
        params["required"] = json!(true);
        let result = call(&tool, params).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.details["errorCode"], "question_cancelled");
    }

    #[tokio::test]
    async fn hosts_without_the_gateway_dialog_ask_through_the_ask_tool() {
        let tool = tool_with(
            Arc::new(TauUi(json!({"answers": [{"id": "q1", "selectedOptions": ["Postgres"]}]}))),
            None,
        );
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(text_of(&result), "User answered:\n- Storage: Postgres");

        let tool = tool_with(
            Arc::new(TauUi(json!({"answers": [
                {"id": "q1", "selectedOptions": [], "customInput": "DuckDB"},
                {"id": "q2", "selectedOptions": ["Cache", "Search"]}
            ]}))),
            None,
        );
        let result = call(&tool, two_questions()).await.unwrap();
        assert_eq!(
            text_of(&result),
            "User answered:\n- Storage: own answer: \"DuckDB\"\n- Extras: Cache, Search"
        );

        let tool = tool_with(Arc::new(TauUi(json!({"cancelled": true}))), None);
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(result.details["cancelled"], true);
    }

    #[tokio::test]
    async fn hosts_with_only_select_prompts_still_answer() {
        let tool = tool_with(Arc::new(SelectUi("SQLite")), None);
        let result = call(&tool, storage()).await.unwrap();
        assert_eq!(text_of(&result), "User answered:\n- Storage: SQLite");
    }

    #[tokio::test]
    async fn wait_for_answer_false_returns_at_once_and_delivers_the_answer_later() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sink: FollowUpSink = Arc::new(move |message| {
            sent.send(message).ok();
        });
        let ui = Arc::new(GatewayUi {
            response: Some(json!({"answers": {"q1": {"selected": ["SQLite"]}}})),
            dialogs: PlMutex::new(Vec::new()),
        });
        let tool = tool_with(ui, Some(sink));
        let mut params = storage();
        params["waitForAnswer"] = json!(false);
        let result = call(&tool, params).await.unwrap();
        assert_eq!(result.details["pending"], true);
        let id = result.details["questionId"].as_str().unwrap().to_string();
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .expect("the answer is delivered")
            .expect("the sink is still open");
        assert_eq!(
            message,
            format!("[ask_user_question answer {id}]\nUser answered:\n- Storage: SQLite")
        );
    }

    #[tokio::test]
    async fn wait_for_answer_false_waits_when_the_host_cannot_deliver_later() {
        let (_, tool) = gateway(Some(json!({"answers": {"q1": {"selected": ["SQLite"]}}})));
        let mut params = storage();
        params["waitForAnswer"] = json!(false);
        let result = call(&tool, params).await.unwrap();
        assert_eq!(result.details["pending"], false);
        assert_eq!(text_of(&result), "User answered:\n- Storage: SQLite");
    }

    #[tokio::test]
    async fn an_abort_ends_the_wait() {
        struct Never;
        impl AskUi for Never {
            fn ask(&self, _: Value, _: Option<u64>) -> BoxFut<'static, Result<Option<Value>>> {
                Box::pin(std::future::pending())
            }
            fn has_question_dialog(&self) -> bool {
                true
            }
            fn question(&self, _: String, _: Value, _: Option<u64>) -> BoxFut<'static, Result<Option<Value>>> {
                Box::pin(std::future::pending())
            }
        }
        let controller = pa_agent::abort::AbortController::new();
        let tool = tool_with(Arc::new(Never), None);
        controller.abort();
        let err = tool.run(storage(), &controller.signal()).await.unwrap_err();
        assert!(pa_agent::abort::is_abort_error(&err), "{err:#}");
    }

    async fn goal_call(tools: &[Arc<dyn AgentTool>], name: &str, params: Value) -> AgentToolResult {
        let tool = tools.iter().find(|t| t.name() == name).unwrap().clone();
        tool.execute("call".into(), params, AbortSignal::never(), Arc::new(|_| {}))
            .await
            .unwrap()
    }

    fn fresh_goals() -> Vec<Arc<dyn AgentTool>> {
        goal_tools_with(&Arc::default())
    }

    #[tokio::test]
    async fn goal_lifecycle_create_get_complete() {
        let tools = fresh_goals();
        let none = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(none.is_error, None);
        assert_eq!(none.details["goal"], Value::Null);

        let created = goal_call(&tools, "create_goal", json!({"objective": "  ship it  "})).await;
        assert_eq!(created.is_error, None);
        assert_eq!(created.details["goal"]["objective"], "ship it");
        assert_eq!(created.details["goal"]["status"], "active");

        let got = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(got.details["goal"]["status"], "active");
        assert_eq!(got.details["goal"]["id"], created.details["goal"]["id"]);

        let done = goal_call(&tools, "update_goal", json!({"status": "complete"})).await;
        assert_eq!(done.is_error, None);
        assert_eq!(done.details["goal"]["status"], "complete");
        let got = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(got.details["goal"]["status"], "complete");

        // A finished goal does not block the next one.
        let next = goal_call(&tools, "create_goal", json!({"objective": "again"})).await;
        assert_eq!(next.is_error, None);
        assert_eq!(next.details["goal"]["status"], "active");
    }

    #[tokio::test]
    async fn a_blocked_goal_carries_its_reason_and_resumes_on_the_goal_runtime() {
        let runtime: Arc<Mutex<GoalRuntime>> = Arc::default();
        let tools = goal_tools_with(&runtime);
        goal_call(&tools, "create_goal", json!({"objective": "migrate"})).await;
        let blocked = goal_call(
            &tools,
            "update_goal",
            json!({"status": "blocked", "reason": "  need the prod password  "}),
        )
        .await;
        assert_eq!(blocked.is_error, None);
        assert_eq!(blocked.details["goal"]["status"], "blocked");
        assert_eq!(blocked.details["goal"]["reason"], "need the prod password");
        assert!(text_of(&blocked).contains("Status: blocked\nReason: need the prod password"));

        let got = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(got.details["goal"]["status"], "blocked");

        // The same runtime backs tau's own `goal` ops: resuming clears the block.
        lock(&runtime).resume_goal().unwrap();
        let got = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(got.details["goal"]["status"], "active");
        assert!(got.details["goal"].get("reason").is_none());
    }

    #[tokio::test]
    async fn goal_calls_are_rejected_with_an_error_code() {
        let tools = fresh_goals();
        let missing = goal_call(&tools, "update_goal", json!({"status": "complete"})).await;
        assert_eq!(missing.is_error, Some(true));
        assert_eq!(missing.details["errorCode"], "goal_not_found");
        let missing = goal_call(&tools, "update_goal", json!({"status": "blocked", "reason": "x"})).await;
        assert_eq!(missing.details["errorCode"], "goal_not_found");

        for bad in [json!({}), json!({"objective": "   "}), json!({"objective": "x".repeat(4001)})] {
            let result = goal_call(&tools, "create_goal", bad).await;
            assert_eq!(result.is_error, Some(true));
            assert_eq!(result.details["errorCode"], "invalid_arguments");
        }

        goal_call(&tools, "create_goal", json!({"objective": "first"})).await;
        let twice = goal_call(&tools, "create_goal", json!({"objective": "second"})).await;
        assert_eq!(twice.is_error, Some(true));
        assert_eq!(twice.details["errorCode"], "goal_exists");

        for bad in [
            json!({}),
            json!({"status": "paused"}),
            json!({"status": "blocked"}),
            json!({"status": "blocked", "reason": "  "}),
            json!({"status": "complete", "reason": "done"}),
        ] {
            let result = goal_call(&tools, "update_goal", bad.clone()).await;
            assert_eq!(result.is_error, Some(true), "{bad}");
            assert_eq!(result.details["errorCode"], "invalid_arguments", "{bad}");
        }
        // None of the rejected calls touched the goal.
        let got = goal_call(&tools, "get_goal", json!({})).await;
        assert_eq!(got.details["goal"]["status"], "active");
    }

    #[tokio::test]
    async fn a_finished_goal_cannot_be_blocked() {
        let tools = fresh_goals();
        goal_call(&tools, "create_goal", json!({"objective": "x"})).await;
        goal_call(&tools, "update_goal", json!({"status": "complete"})).await;
        let result = goal_call(&tools, "update_goal", json!({"status": "blocked", "reason": "late"})).await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.details["errorCode"], "invalid_arguments");
    }

    #[test]
    fn the_session_registry_lists_the_compat_tools() {
        let dir = tempfile::tempdir().unwrap();
        let settings = omp_settings::Settings::load(dir.path());
        let names: Vec<_> = crate::tools::session_builtin_tools(
            dir.path(),
            pi_edit::EditMode::Replace,
            &settings,
        )
        .iter()
        .map(|t| t.name())
        .collect();
        for name in ["ask_user_question", "create_goal", "get_goal", "update_goal"] {
            assert!(names.contains(&name), "{name} is registered");
        }
        // tau's own tools stay.
        assert!(names.contains(&"ask"));
    }

    #[test]
    fn content_text_helper_reads_text_blocks() {
        let result = AgentToolResult {
            content: vec![ToolResultContent::text("a"), ToolResultContent::text("b")],
            ..AgentToolResult::default()
        };
        assert_eq!(result_text(&result), "a\nb");
    }
}
