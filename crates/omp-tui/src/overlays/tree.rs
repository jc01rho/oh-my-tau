//! Session tree navigator — port of `pi-tui/src/overlays/tree-selector.ts`
//! (`/tree`) over the `TreeView` projection (`components/tree-view.ts`),
//! plus the double-Esc rewind column (the MVP of `rewind-selector.ts`).
//!
//! `/tree` lists every persisted entry of the session forest (omp-session
//! `build_tree`, fed by the host's live journal). Linear chains keep their
//! branch head's depth; only real forks indent, drawing a three-cell gutter
//! (`├─`/`└─` at the fork, `│` while siblings continue below) that compresses
//! older levels behind a leading `…` when the tree is deep. The branch holding
//! the live leaf sorts first at every fork and its entries carry a `•`. Five
//! filter modes (`alt+d/t/u/l/a`, `ctrl+o` cycles) and a type-to-search query
//! narrow the rows without changing their geometry. Enter navigates (TS
//! `navigateTree`), Shift+Enter navigates with a branch summary, Shift+L edits
//! the selected entry's label, and the `app.tree.previousBranch` /
//! `app.tree.nextBranch` bindings jump between branches.
//!
//! The navigator never talks to the runtime: a confirmed decision queues as a
//! [`TreeOutcome`] that [`super::Overlays::take_actions`] drains.

use std::cell::Cell;
use std::collections::HashMap;

use omp_session::tree::{
    branch_path, build_tree, is_user_message_entry, user_message_text, TreeNode,
};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;

use super::models::{pad, truncate_spans};
use crate::autocomplete::fuzzy::FuzzyText;
use crate::component::{Component, EventOutcome};
use crate::keybindings::{format_key_hint, KeybindingsManager};
use crate::keys::{KeyCode, KeyEvent, KeyModifiers};
use crate::port::SessionJournal;
use crate::theme::Theme;

/// Characters of entry text kept for display and search (TS `SEARCH_TEXT_LIMIT`).
const SEARCH_TEXT_LIMIT: usize = 200;
/// Gutter budget (TS `#renderGutter`): entry text always keeps this many columns.
const MIN_CONTENT_COLS: usize = 24;
/// Cursor plus breathing room beside the gutter.
const GUTTER_OVERHEAD_COLS: usize = 4;
/// Bookkeeping kinds hidden outside the `all` filter (TS `isSettingsEntry`).
/// `session_info` is omp-rs's rename record (upstream `title_change`).
const SETTINGS_KINDS: &[&str] = &[
    "label",
    "custom",
    "model_change",
    "model_usage",
    "thinking_level_change",
    "service_tier_change",
    "title_change",
    "credential_pin",
    "session_init",
    "ttsr_injection",
    "mode_change",
    "reset_boundary",
    "session_info",
];

/// What the navigator shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeMode {
    /// User-message rewind column (double-Esc default action).
    #[default]
    Rewind,
    /// Full session tree (`/tree`).
    Tree,
}

/// `/tree` row filter (TS `TREE_FILTER_MODES`, in `ctrl+o` cycle order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Everything except bookkeeping entries.
    #[default]
    Default,
    /// Default minus tool results.
    NoTools,
    /// User requests only.
    UserOnly,
    /// Labeled entries only.
    LabeledOnly,
    /// Every entry.
    All,
}

impl FilterMode {
    /// Cycle order (TS `TREE_FILTER_MODES`).
    pub const CYCLE: [Self; 5] = [
        Self::Default,
        Self::NoTools,
        Self::UserOnly,
        Self::LabeledOnly,
        Self::All,
    ];

    /// Setting / wire id (`treeFilterMode`).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }

    /// Tab label (TS `FILTER_TABS`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::NoTools => "No tools",
            Self::UserOnly => "User only",
            Self::LabeledOnly => "Labeled",
            Self::All => "All",
        }
    }

    /// Parse a `treeFilterMode` setting value.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::CYCLE.into_iter().find(|m| m.id() == id)
    }

    fn cycled(self, forward: bool) -> Self {
        let n = Self::CYCLE.len();
        let i = Self::CYCLE.iter().position(|m| *m == self).unwrap_or(0);
        Self::CYCLE[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// How `/tree` opens (TS `showTreeSelector` inputs).
#[derive(Debug, Clone, Default)]
pub struct TreeOptions {
    /// Initial filter (`treeFilterMode` setting).
    pub filter: FilterMode,
    /// Ask "Summarize branch?" before navigating (`branchSummary.enabled`).
    pub summary_prompt: bool,
    /// Session name shown beside the title.
    pub session_name: Option<String>,
    /// Effective bindings: branch jumps and the hint row.
    pub keys: KeybindingsManager,
}

/// A decision the host must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOutcome {
    /// Navigate to `entry_id` (`summarize`: summarize the abandoned branch).
    Navigate {
        /// Target entry id.
        entry_id: String,
        /// Record a branch summary first.
        summarize: bool,
    },
    /// The pick is the live leaf already (TS "Already at this point").
    AlreadyHere,
    /// Set (`Some`) or clear (`None`) the label of `entry_id`; the tree stays
    /// open and reloads.
    Label {
        /// Labeled entry id.
        entry_id: String,
        /// New label.
        label: Option<String>,
    },
    /// Close without acting (Ctrl+C).
    Cancel,
}

/// One styled piece of an entry's row text (TS `EntryPart`).
#[derive(Debug, Clone, PartialEq)]
struct Part {
    text: String,
    token: &'static str,
    strong: bool,
}

fn part(text: impl Into<String>, token: &'static str) -> Part {
    Part {
        text: text.into(),
        token,
        strong: false,
    }
}

fn strong(text: impl Into<String>, token: &'static str) -> Part {
    Part {
        text: text.into(),
        token,
        strong: true,
    }
}

/// Display and filter facts of one entry (TS `#entryParts` + `#buildFilter`).
#[derive(Debug, Clone, Default, PartialEq)]
struct EntryView {
    parts: Vec<Part>,
    /// Searchable text: label, role, text content and kind-specific fields.
    search: String,
    /// Bookkeeping kind, hidden outside `all`.
    settings: bool,
    /// A `toolResult` message (hidden by `no-tools`).
    tool_result: bool,
    /// A user request (`user-only`).
    user_request: bool,
    /// An assistant turn with no text that stopped normally: hidden unless it
    /// is the live leaf.
    hollow_assistant: bool,
    /// A user or assistant message (Alt+Up / Alt+Down stops).
    turn: bool,
}

/// A gutter ancestor retained on a row (TS `TreeAncestor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ancestor {
    depth: usize,
    is_last: bool,
    sibling_count: usize,
    root: bool,
}

/// One link of the shared ancestor chain (TS `AncestorChain`): a linear chain
/// reuses its head's link, so memory stays linear in the entry count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    ancestor: Ancestor,
    parent: Option<usize>,
}

/// One structural row of the flattened forest (TS `TreeRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    /// Entry id.
    pub id: String,
    /// Display depth: forks indent, linear chains keep their head's depth.
    pub depth: usize,
    /// Row of the parent entry (`None` for a root).
    parent: Option<usize>,
    /// Child rows, the branch holding the live leaf first.
    children: Vec<usize>,
    sibling_count: usize,
    is_last: bool,
    chain: Option<usize>,
    /// On the root → live-leaf path (`•` marker).
    pub on_path: bool,
    /// The live leaf itself.
    pub current: bool,
    /// Resolved label (`[label]` badge).
    pub label: Option<String>,
    /// A user message's prompt text (the rewind column's row text).
    pub text: String,
    view: EntryView,
}

/// The flattened session forest.
#[derive(Debug, Clone, Default)]
struct Forest {
    rows: Vec<TreeRow>,
    links: Vec<Link>,
    /// Root rows in display order.
    roots: Vec<usize>,
    multiple_roots: bool,
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// One display line: newlines and tabs become spaces (TS `normalize`).
fn one_line(s: &str) -> String {
    s.replace(['\n', '\t', '\r'], " ").trim().to_owned()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Text blocks joined, stopping once `limit` characters exist (TS
/// `#joinTextContent`).
fn join_text(content: Option<&Value>, limit: usize) -> String {
    match content {
        Some(Value::String(s)) => take_chars(s, limit),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for block in blocks {
                if str_at(block, "type") == Some("text") {
                    if let Some(text) = str_at(block, "text") {
                        out.push_str(text);
                        if out.chars().count() >= limit {
                            return take_chars(&out, limit);
                        }
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// `[tool: target]` for a tool result row (TS `#formatToolCall`).
fn format_tool_call(name: &str, args: &Value) -> String {
    let path_of = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| str_at(args, k))
            .map(crate::status_line::shorten_path)
            .unwrap_or_default()
    };
    let paths = |key: &str| -> Vec<String> {
        match args.get(key) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    };
    let scope = |list: Vec<String>| {
        if list.is_empty() {
            ".".to_owned()
        } else {
            crate::status_line::shorten_path(&list.join(", "))
        }
    };
    match name {
        "read" => {
            let mut display = path_of(&["path", "file_path"]);
            let offset = args.get("offset").and_then(Value::as_u64);
            let limit = args.get("limit").and_then(Value::as_u64);
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                display.push_str(&format!(":{start}"));
                if let Some(limit) = limit {
                    display.push_str(&format!(
                        "-{}",
                        start.saturating_add(limit.saturating_sub(1))
                    ));
                }
            }
            format!("[read: {display}]")
        }
        "write" => format!("[write: {}]", path_of(&["path", "file_path"])),
        "edit" => format!("[edit: {}]", path_of(&["path", "file_path"])),
        "bash" => {
            let raw = str_at(args, "command").unwrap_or_default();
            let cmd = take_chars(&one_line(raw), 50);
            let more = if raw.chars().count() > 50 { "..." } else { "" };
            format!("[bash: {cmd}{more}]")
        }
        "grep" => {
            let pattern = str_at(args, "pattern").unwrap_or_default();
            let list = if args.get("paths").is_some() {
                paths("paths")
            } else {
                paths("path")
            };
            format!("[grep: /{pattern}/ in {}]", scope(list))
        }
        "glob" => {
            let list = if args.get("path").is_some() {
                paths("path")
            } else {
                paths("paths")
            };
            format!("[glob: {}]", scope(list))
        }
        "ls" => {
            let path = str_at(args, "path").unwrap_or(".");
            format!("[ls: {}]", crate::status_line::shorten_path(path))
        }
        _ => {
            let json = args.to_string();
            let more = if json.chars().count() > 40 { "..." } else { "" };
            format!("[{name}: {}{more}]", take_chars(&json, 40))
        }
    }
}

/// Tool calls by id from every assistant message (TS `#toolCallMap`).
fn tool_calls(entries: &[Value]) -> HashMap<String, (String, Value)> {
    let mut calls = HashMap::new();
    for entry in entries {
        if str_at(entry, "type") != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        if str_at(message, "role") != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if str_at(block, "type") != Some("toolCall") {
                continue;
            }
            if let (Some(id), Some(name)) = (str_at(block, "id"), str_at(block, "name")) {
                let args = block.get("arguments").cloned().unwrap_or(Value::Null);
                calls.insert(id.to_owned(), (name.to_owned(), args));
            }
        }
    }
    calls
}

/// Row text and filter facts for one entry (TS `#entryParts`,
/// `#getSearchableText`, `#buildFilter`).
#[allow(clippy::too_many_lines)] // one arm per entry kind, mirroring the TS switch
fn entry_view(
    entry: &Value,
    label: Option<&str>,
    calls: &HashMap<String, (String, Value)>,
) -> EntryView {
    let kind = str_at(entry, "type").unwrap_or("entry");
    let mut search: Vec<String> = label.map(str::to_owned).into_iter().collect();
    let mut view = EntryView {
        settings: SETTINGS_KINDS.contains(&kind),
        ..EntryView::default()
    };
    let field = |key: &str| str_at(entry, key).map(one_line).unwrap_or_default();
    view.parts = match kind {
        "message" => {
            let message = entry.get("message").unwrap_or(&Value::Null);
            let role = str_at(message, "role").unwrap_or_default();
            let content = join_text(message.get("content"), SEARCH_TEXT_LIMIT);
            search.push(role.to_owned());
            search.push(content.clone());
            let text = one_line(&content);
            match role {
                "user" => {
                    view.user_request = true;
                    view.turn = true;
                    vec![strong("user: ", "accent"), strong(text, "accent")]
                }
                "developer" => vec![part("developer: ", "dim"), part(text, "text_muted")],
                "assistant" => {
                    view.turn = true;
                    let head = part("assistant: ", "success");
                    let stop = str_at(message, "stopReason");
                    if text.is_empty() {
                        let error_or_aborted = stop.is_some_and(|s| s != "stop" && s != "toolUse");
                        view.hollow_assistant = !error_or_aborted;
                        let error = str_at(message, "errorMessage")
                            .map(one_line)
                            .filter(|e| !e.is_empty());
                        match (stop, error) {
                            (Some("aborted"), _) => vec![head, part("(aborted)", "text_muted")],
                            (_, Some(error)) if error_or_aborted => {
                                vec![head, part(take_chars(&error, 80), "error")]
                            }
                            _ => vec![head, part("(no content)", "text_muted")],
                        }
                    } else {
                        vec![head, part(text, "text")]
                    }
                }
                "toolResult" => {
                    view.tool_result = true;
                    let call = str_at(message, "toolCallId").and_then(|id| calls.get(id));
                    let shown = call.map_or_else(
                        || format!("[{}]", str_at(message, "toolName").unwrap_or("tool")),
                        |(name, args)| format_tool_call(name, args),
                    );
                    vec![part(shown, "text_muted")]
                }
                "bashExecution" => {
                    let command = str_at(message, "command").unwrap_or_default();
                    search.push(command.to_owned());
                    vec![part(format!("[bash]: {}", one_line(command)), "dim")]
                }
                other => vec![part(format!("[{other}]"), "dim")],
            }
        }
        "custom_message" => {
            let custom = str_at(entry, "customType").unwrap_or("custom");
            let content = join_text(entry.get("content"), SEARCH_TEXT_LIMIT);
            search.push(custom.to_owned());
            search.push(content.clone());
            vec![
                part(format!("[{custom}]: "), "customMessageLabel"),
                part(one_line(&content), "text_muted"),
            ]
        }
        "compaction" => {
            search.push("compaction".into());
            let tokens = entry
                .get("tokensBefore")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            vec![part(
                format!("[compaction: {}k tokens]", (tokens / 1000.0).round()),
                "borderAccent",
            )]
        }
        "branch_summary" => {
            let summary = str_at(entry, "summary").unwrap_or_default();
            search.push(format!("branch summary {summary}"));
            vec![
                part("[branch summary]: ", "warning"),
                part(one_line(summary), "text"),
            ]
        }
        "model_change" => {
            search.push(format!("model {}", field("model")));
            vec![part(format!("[model: {}]", field("model")), "dim")]
        }
        "model_usage" => {
            let role = field("role");
            let role = if role.is_empty() {
                role
            } else {
                format!("{role} ")
            };
            let shown = format!(
                "{} {role}{}/{}",
                field("purpose"),
                field("provider"),
                field("model")
            );
            search.push(format!("model usage {shown}"));
            vec![part(format!("[model usage: {shown}]"), "dim")]
        }
        "thinking_level_change" => {
            let level = str_at(entry, "thinkingLevel").unwrap_or("off");
            search.push(format!("thinking {level}"));
            vec![part(format!("[thinking: {level}]"), "dim")]
        }
        "custom" => {
            search.push(format!("custom {}", field("customType")));
            vec![part(format!("[custom: {}]", field("customType")), "dim")]
        }
        "label" => {
            let shown = str_at(entry, "label")
                .filter(|l| !l.is_empty())
                .map_or_else(|| "(cleared)".to_owned(), one_line);
            search.push(format!("label {shown}"));
            vec![part(format!("[label: {shown}]"), "dim")]
        }
        "title_change" | "session_info" => {
            let title = if kind == "title_change" {
                field("title")
            } else {
                field("name")
            };
            search.push(format!("title {title}"));
            vec![part(format!("[title: {title}]"), "dim")]
        }
        "service_tier_change" => {
            let tiers = entry
                .get("serviceTier")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(family, tier)| tier.as_str().map(|t| format!("{family}:{t}")))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "(default)".to_owned());
            search.push(format!("service tier {tiers}"));
            vec![part(format!("[service tier: {tiers}]"), "dim")]
        }
        "mode_change" => {
            search.push(format!("mode {}", field("mode")));
            vec![part(format!("[mode: {}]", field("mode")), "dim")]
        }
        "credential_pin" => {
            search.push(format!("credential pin {}", field("provider")));
            vec![part(
                format!("[credential pin: {}]", field("provider")),
                "dim",
            )]
        }
        other => {
            let shown = other.replace('_', " ");
            search.push(shown.clone());
            vec![part(format!("[{shown}]"), "dim")]
        }
    };
    view.search = search.join(" ");
    view
}

impl Forest {
    /// Project `entries` (TS `TreeList#indexSession` + `projectTree`):
    /// active-branch-first ordering, linear chains at their head's depth,
    /// shared ancestor links. Iterative, so deep sessions cannot overflow the
    /// stack.
    fn build(entries: &[Value], leaf: Option<&str>) -> Self {
        struct Node {
            entry: Value,
            label: Option<String>,
            parent: Option<usize>,
            children: Vec<usize>,
        }
        struct Pending {
            node: usize,
            parent_row: Option<usize>,
            depth: usize,
            sibling_index: usize,
            sibling_count: usize,
            chain: Option<usize>,
        }

        // Arena out of the recursive omp-session forest; children are taken
        // before each node drops so a deep chain never drops recursively.
        let mut arena: Vec<Node> = Vec::with_capacity(entries.len());
        let mut roots = Vec::new();
        let mut stack: Vec<(TreeNode, Option<usize>)> = build_tree(entries)
            .into_iter()
            .rev()
            .map(|n| (n, None))
            .collect();
        while let Some((mut node, parent)) = stack.pop() {
            let idx = arena.len();
            let children = std::mem::take(&mut node.children);
            arena.push(Node {
                entry: node.entry,
                label: node.label,
                parent,
                children: Vec::new(),
            });
            match parent {
                Some(p) => arena[p].children.push(idx),
                None => roots.push(idx),
            }
            stack.extend(children.into_iter().rev().map(|c| (c, Some(idx))));
        }

        // Active-descendant membership: children always follow their parent.
        let leaf_idx =
            leaf.and_then(|l| arena.iter().position(|n| str_at(&n.entry, "id") == Some(l)));
        let mut active = vec![false; arena.len()];
        if let Some(l) = leaf_idx {
            active[l] = true;
        }
        for i in (0..arena.len()).rev() {
            if active[i] {
                if let Some(p) = arena[i].parent {
                    active[p] = true;
                }
            }
        }
        for node in &mut arena {
            node.children.sort_by_key(|c| !active[*c]);
        }
        roots.sort_by_key(|r| !active[*r]);
        let mut on_path = vec![false; arena.len()];
        let mut cursor = leaf_idx;
        while let Some(i) = cursor {
            on_path[i] = true;
            cursor = arena[i].parent;
        }

        let calls = tool_calls(entries);
        let multiple_roots = roots.len() > 1;
        let mut forest = Self {
            rows: Vec::with_capacity(arena.len()),
            links: Vec::new(),
            roots: Vec::with_capacity(roots.len()),
            multiple_roots,
        };
        let mut pending: Vec<Pending> = roots
            .iter()
            .enumerate()
            .rev()
            .map(|(i, &node)| Pending {
                node,
                parent_row: None,
                depth: 0,
                sibling_index: i,
                sibling_count: roots.len(),
                chain: None,
            })
            .collect();
        while let Some(p) = pending.pop() {
            let row_idx = forest.rows.len();
            let node = &arena[p.node];
            let is_last = p.sibling_index + 1 == p.sibling_count;
            let id = str_at(&node.entry, "id").unwrap_or_default().to_owned();
            forest.rows.push(TreeRow {
                current: leaf_idx == Some(p.node),
                on_path: on_path[p.node],
                label: node.label.clone(),
                text: user_message_text(&node.entry).unwrap_or_default(),
                view: entry_view(&node.entry, node.label.as_deref(), &calls),
                id,
                depth: p.depth,
                parent: p.parent_row,
                children: Vec::new(),
                sibling_count: p.sibling_count,
                is_last,
                chain: p.chain,
            });
            match p.parent_row {
                Some(parent) => forest.rows[parent].children.push(row_idx),
                None => forest.roots.push(row_idx),
            }
            let children = &node.children;
            if children.is_empty() {
                continue;
            }
            let child_depth = if children.len() > 1 || (multiple_roots && p.parent_row.is_none()) {
                p.depth + 1
            } else {
                p.depth
            };
            // `compactSameDepthAncestors`: a linear single child reuses the chain.
            let child_chain = if child_depth == p.depth && p.sibling_count == 1 {
                p.chain
            } else {
                forest.links.push(Link {
                    ancestor: Ancestor {
                        depth: p.depth,
                        is_last,
                        sibling_count: p.sibling_count,
                        root: p.parent_row.is_none(),
                    },
                    parent: p.chain,
                });
                Some(forest.links.len() - 1)
            };
            let count = children.len();
            for (i, &child) in children.iter().enumerate().rev() {
                pending.push(Pending {
                    node: child,
                    parent_row: Some(row_idx),
                    depth: child_depth,
                    sibling_index: i,
                    sibling_count: count,
                    chain: child_chain,
                });
            }
        }
        forest
    }

    /// The rewind column: user messages along the branch ending at `leaf`,
    /// oldest first (TS `rewind-selector.ts` targets, user turns only).
    fn rewind(entries: &[Value], leaf: Option<&str>) -> Self {
        let Some(leaf) = leaf else {
            return Self::default();
        };
        let rows: Vec<TreeRow> = branch_path(entries, leaf)
            .into_iter()
            .filter(|e| is_user_message_entry(e))
            .map(|e| TreeRow {
                id: str_at(e, "id").unwrap_or_default().to_owned(),
                depth: 0,
                parent: None,
                children: Vec::new(),
                sibling_count: 1,
                is_last: true,
                chain: None,
                on_path: true,
                current: false,
                label: None,
                text: user_message_text(e).unwrap_or_default(),
                view: EntryView {
                    turn: true,
                    user_request: true,
                    search: user_message_text(e).unwrap_or_default(),
                    ..EntryView::default()
                },
            })
            .collect();
        let roots = (0..rows.len()).collect();
        Self {
            rows,
            links: Vec::new(),
            roots,
            multiple_roots: false,
        }
    }

    fn ancestors(&self, row: &TreeRow) -> Vec<Ancestor> {
        let mut out = Vec::new();
        let mut link = row.chain;
        while let Some(i) = link {
            out.push(self.links[i].ancestor);
            link = self.links[i].parent;
        }
        out.reverse();
        out
    }

    /// Sibling rows of `row` (its parent's children, or the roots).
    fn siblings(&self, row: usize) -> &[usize] {
        match self.rows[row].parent {
            Some(parent) => &self.rows[parent].children,
            None => &self.roots,
        }
    }

    /// The three-cell gutter of `row` (TS `#renderGutter`): connectors only at
    /// real forks, `│` under an ancestor with siblings still below, levels
    /// older than the window's budget compressed behind a leading `…`.
    fn gutter(&self, row: &TreeRow, width: usize, deepest: usize, theme: &Theme) -> String {
        let reserve = MIN_CONTENT_COLS.max(width / 2);
        let max_levels = (width.saturating_sub(reserve + GUTTER_OVERHEAD_COLS) / 3).max(1);
        let scroll = deepest.saturating_sub(max_levels).min(row.depth);
        let rendered = row.depth - scroll;
        let has_connector = row.parent.is_some() && row.sibling_count > 1;
        let connector: Vec<char> = if has_connector {
            theme
                .symbol(if row.is_last {
                    "tree.last"
                } else {
                    "tree.branch"
                })
                .chars()
                .collect()
        } else {
            Vec::new()
        };
        let connector_level = if has_connector {
            rendered.checked_sub(1)
        } else {
            None
        };
        let mut by_level: Vec<Option<Ancestor>> = vec![None; rendered];
        for ancestor in self.ancestors(row) {
            if ancestor.sibling_count <= 1 || (self.multiple_roots && ancestor.root) {
                continue;
            }
            let Some(level) = ancestor.depth.checked_sub(1 + scroll) else {
                continue;
            };
            if level < rendered && by_level[level].is_none() {
                by_level[level] = Some(ancestor);
            }
        }
        let vertical = theme.symbol("tree.vertical");
        let horizontal = theme.symbol("tree.horizontal");
        let mut out = String::new();
        for (level, gutter) in by_level.iter().enumerate() {
            for pos in 0..3 {
                if let Some(ancestor) = gutter {
                    out.push_str(if pos == 0 && !ancestor.is_last {
                        vertical
                    } else {
                        " "
                    });
                } else if connector_level == Some(level) {
                    match connector.get(pos) {
                        Some(c) => out.push(*c),
                        None if pos == 1 => out.push_str(horizontal),
                        None => out.push(' '),
                    }
                } else {
                    out.push(' ');
                }
            }
        }
        if scroll > 0 && !out.is_empty() {
            let first = out.chars().next().map_or(0, char::len_utf8);
            out.replace_range(..first, "…");
        }
        out
    }
}

/// What the keys edit right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Stage {
    /// The entry list.
    #[default]
    List,
    /// "Summarize branch?" before navigating to `entry_id`
    /// (`branchSummary.enabled`).
    Summary { entry_id: String, cursor: usize },
    /// The Shift+L label editor for `entry_id`.
    Label { entry_id: String, buffer: String },
}

/// Interactive navigator: the `/tree` view or the double-Esc rewind column.
#[derive(Debug, Clone, Default)]
pub struct TreeNavigator {
    /// Which view the host asked for.
    pub mode: TreeMode,
    forest: Forest,
    leaf: Option<String>,
    session_name: Option<String>,
    filter: FilterMode,
    /// `/tree` search query, or the rewind column's `f` filter text.
    query: String,
    /// Rewind column: the `f` filter is open.
    rewind_filter: bool,
    /// Prepared fuzzy text per row, built on the first search.
    fuzzy: Vec<Option<FuzzyText>>,
    /// Visible rows in display order, and each row's position among them.
    visible: Vec<usize>,
    visible_pos: Vec<Option<usize>>,
    /// Selected row, and the anchor kept through empty filter results (TS
    /// `TreeView` `#selectionAnchor`).
    selected: Option<usize>,
    anchor: Option<usize>,
    summary_prompt: bool,
    keys: KeybindingsManager,
    stage: Stage,
    /// Rows per page from the last render (TS `maxVisibleLines`).
    page: Cell<usize>,
    outcome: Option<TreeOutcome>,
}

impl TreeNavigator {
    /// Load `journal` in `mode`; the selection starts on the live leaf (the
    /// newest prompt in the rewind column).
    pub fn open(&mut self, journal: &SessionJournal, mode: TreeMode, options: TreeOptions) {
        self.mode = mode;
        self.filter = options.filter;
        self.summary_prompt = options.summary_prompt;
        self.session_name = options.session_name;
        self.keys = options.keys;
        self.query.clear();
        self.rewind_filter = false;
        self.stage = Stage::List;
        self.outcome = None;
        self.load(journal);
        let start = match mode {
            TreeMode::Rewind => self.forest.rows.len().checked_sub(1),
            TreeMode::Tree => self.forest.rows.iter().position(|r| r.current),
        };
        // No leaf: TS selects the last row.
        self.anchor = start.or_else(|| self.forest.rows.len().checked_sub(1));
        self.refilter();
    }

    /// Rebuild from a fresh `journal` (after a label edit), keeping the mode,
    /// filter, query and the selected entry.
    pub fn reload(&mut self, journal: &SessionJournal) {
        let selected_id = self.selected_row().map(|r| r.id.clone());
        self.load(journal);
        self.anchor = selected_id
            .and_then(|id| self.forest.rows.iter().position(|r| r.id == id))
            .or_else(|| self.forest.rows.iter().position(|r| r.current));
        self.refilter();
    }

    fn load(&mut self, journal: &SessionJournal) {
        self.leaf.clone_from(&journal.leaf);
        self.forest = match self.mode {
            TreeMode::Tree => Forest::build(&journal.entries, journal.leaf.as_deref()),
            TreeMode::Rewind => Forest::rewind(&journal.entries, journal.leaf.as_deref()),
        };
        self.fuzzy = vec![None; self.forest.rows.len()];
        self.selected = None;
    }

    /// The selected row, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.selected.map(|i| &self.forest.rows[i])
    }

    /// The active filter mode.
    #[must_use]
    pub fn filter_mode(&self) -> FilterMode {
        self.filter
    }

    /// Ids of the visible rows, in display order.
    #[must_use]
    pub fn visible_ids(&self) -> Vec<&str> {
        self.visible
            .iter()
            .map(|&i| self.forest.rows[i].id.as_str())
            .collect()
    }

    /// Take the pending decision, if any.
    pub fn take_outcome(&mut self) -> Option<TreeOutcome> {
        self.outcome.take()
    }

    fn passes(&self, i: usize) -> bool {
        let row = &self.forest.rows[i];
        if self.mode == TreeMode::Rewind {
            let q = self.query.to_lowercase();
            return !self.rewind_filter || row.text.to_lowercase().contains(&q);
        }
        let view = &row.view;
        if view.hollow_assistant && !row.current {
            return false;
        }
        let mode_ok = match self.filter {
            FilterMode::UserOnly => view.user_request,
            FilterMode::NoTools => !view.settings && !view.tool_result,
            FilterMode::LabeledOnly => row.label.is_some(),
            FilterMode::All => true,
            FilterMode::Default => !view.settings,
        };
        if !mode_ok {
            return false;
        }
        let query = self.query.trim();
        query.is_empty()
            || self.fuzzy[i]
                .as_ref()
                .is_some_and(|text| text.r#match(query).matches)
    }

    /// Recompute the visible rows and re-resolve the selection from the
    /// anchor (TS `TreeView#ensureRows`).
    fn refilter(&mut self) {
        if self.mode == TreeMode::Tree && !self.query.trim().is_empty() {
            for (i, slot) in self.fuzzy.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(FuzzyText::new(&self.forest.rows[i].view.search));
                }
            }
        }
        self.visible = (0..self.forest.rows.len())
            .filter(|&i| self.passes(i))
            .collect();
        self.visible_pos = vec![None; self.forest.rows.len()];
        for (pos, &row) in self.visible.iter().enumerate() {
            self.visible_pos[row] = Some(pos);
        }
        self.selected = self.nearest_visible(self.anchor);
        if self.selected.is_some() {
            self.anchor = self.selected;
        }
    }

    /// TS `#nearestVisibleKey`: the row itself, else its nearest visible
    /// ancestor, else the last visible row.
    fn nearest_visible(&self, row: Option<usize>) -> Option<usize> {
        let last = *self.visible.last()?;
        let Some(mut candidate) = row else {
            return self.visible.first().copied();
        };
        loop {
            if self.visible_pos[candidate].is_some() {
                return Some(candidate);
            }
            match self.forest.rows[candidate].parent {
                Some(parent) => candidate = parent,
                None => return Some(last),
            }
        }
    }

    fn select_row(&mut self, row: usize) {
        if let Some(resolved) = self.nearest_visible(Some(row)) {
            self.selected = Some(resolved);
            self.anchor = Some(resolved);
        }
    }

    fn selection_index(&self) -> Option<usize> {
        self.selected.and_then(|r| self.visible_pos[r])
    }

    fn select_index(&mut self, index: usize) {
        if let Some(&row) = self
            .visible
            .get(index.min(self.visible.len().saturating_sub(1)))
        {
            self.select_row(row);
        }
    }

    /// TS `moveSelection(delta, wrap)`.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)] // row counts are far below isize::MAX
    fn move_by(&mut self, delta: isize, wrap: bool) {
        let count = self.visible.len();
        if count == 0 {
            return;
        }
        let current = self.selection_index().unwrap_or(0) as isize;
        let mut next = current + delta;
        if wrap {
            next = next.rem_euclid(count as isize);
        }
        self.select_index(next.clamp(0, count as isize - 1) as usize);
    }

    /// Alt+Up / Alt+Down: the previous/next visible user or assistant message
    /// (TS `moveSelectionWhere`, no wrap).
    fn move_turn(&mut self, forward: bool) {
        let Some(start) = self.selection_index() else {
            return;
        };
        let hit = if forward {
            self.visible[start + 1..]
                .iter()
                .find(|&&r| self.forest.rows[r].view.turn)
                .copied()
        } else {
            self.visible[..start]
                .iter()
                .rev()
                .find(|&&r| self.forest.rows[r].view.turn)
                .copied()
        };
        if let Some(row) = hit {
            self.select_row(row);
        }
    }

    /// TS `#moveBranch`: every fork maps each branch to its first visible
    /// row; jump to the adjacent branch of the one holding the selection,
    /// wrapping around.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)] // branch counts are tiny
    fn move_branch(&mut self, forward: bool) {
        let Some(selected) = self.selected else {
            return;
        };
        let rows = &self.forest.rows;
        // Rows are in DFS order, so children follow their parent: one
        // bottom-up pass finds each subtree's first visible row.
        let mut first_visible: Vec<Option<usize>> = vec![None; rows.len()];
        for i in (0..rows.len()).rev() {
            first_visible[i] = if self.visible_pos[i].is_some() {
                Some(i)
            } else {
                rows[i].children.iter().find_map(|&c| first_visible[c])
            };
        }
        let mut targets: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut add_fork = |branches: &[usize]| {
            if branches.len() < 2 {
                return;
            }
            for &branch in branches {
                let Some(target) = first_visible[branch] else {
                    continue;
                };
                match targets.iter_mut().find(|(t, _)| *t == target) {
                    Some((_, heads)) => heads.push(branch),
                    None => targets.push((target, vec![branch])),
                }
            }
        };
        add_fork(&self.forest.roots);
        for row in rows {
            add_fork(&row.children);
        }
        if targets.is_empty() {
            return;
        }
        targets.sort_by_key(|(t, _)| self.visible_pos[*t]);
        // The branch head holding the selection at its nearest fork.
        let mut branch = selected;
        let current = loop {
            if self.forest.siblings(branch).len() > 1 {
                break Some(branch);
            }
            match rows[branch].parent {
                Some(parent) => branch = parent,
                None => break None,
            }
        };
        let index =
            current.and_then(|head| targets.iter().position(|(_, heads)| heads.contains(&head)));
        let n = targets.len() as isize;
        let next = match index {
            None if forward => 0,
            None => n - 1,
            Some(i) => (i as isize + if forward { 1 } else { -1 }).rem_euclid(n),
        };
        let target = targets[next as usize].0;
        self.select_row(target);
    }

    fn set_filter(&mut self, mode: FilterMode) {
        self.filter = mode;
        self.refilter();
    }

    /// Enter / Shift+Enter (TS `confirm(summarize)` + the host's guards).
    fn confirm(&mut self, summarize: bool) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let entry_id = row.id.clone();
        if self.mode == TreeMode::Tree && row.current {
            self.outcome = Some(TreeOutcome::AlreadyHere);
            return;
        }
        if self.mode == TreeMode::Tree && !summarize && self.summary_prompt {
            self.stage = Stage::Summary {
                entry_id,
                cursor: 0,
            };
            return;
        }
        self.outcome = Some(TreeOutcome::Navigate {
            entry_id,
            summarize,
        });
    }

    fn key_tree(&mut self, key: KeyEvent) -> EventOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = isize::try_from(self.page.get().max(1)).unwrap_or(1);
        match key.code {
            KeyCode::Up if alt => self.move_turn(false),
            KeyCode::Down if alt => self.move_turn(true),
            KeyCode::Up => self.move_by(-1, true),
            KeyCode::Down => self.move_by(1, true),
            _ if self.keys.matches_event(&key, "app.tree.previousBranch") => {
                self.move_branch(false)
            }
            _ if self.keys.matches_event(&key, "app.tree.nextBranch") => self.move_branch(true),
            KeyCode::Home => self.select_index(0),
            KeyCode::End => self.select_index(usize::MAX),
            KeyCode::PageUp | KeyCode::Left => self.move_by(-page, false),
            KeyCode::PageDown | KeyCode::Right => self.move_by(page, false),
            // Shift+Enter, or the bare LF some terminals send for it.
            KeyCode::Enter if shift => self.confirm(true),
            KeyCode::Char('j') if ctrl => self.confirm(true),
            KeyCode::Enter => self.confirm(false),
            KeyCode::Esc => {
                if self.query.is_empty() {
                    return EventOutcome::Ignored; // the host closes the overlay
                }
                self.query.clear();
                self.refilter();
            }
            KeyCode::Char('c') if ctrl => self.outcome = Some(TreeOutcome::Cancel),
            KeyCode::Char('o' | 'O') if ctrl => self.set_filter(self.filter.cycled(!shift)),
            KeyCode::Char(c) if alt && !ctrl => {
                let mode = match c {
                    'd' => Some(FilterMode::Default),
                    't' => Some(FilterMode::NoTools),
                    'u' => Some(FilterMode::UserOnly),
                    'l' => Some(FilterMode::LabeledOnly),
                    'a' => Some(FilterMode::All),
                    _ => None,
                };
                if let Some(mode) = mode {
                    self.set_filter(mode);
                }
            }
            KeyCode::Char('L') if self.query.is_empty() && !ctrl => {
                if let Some(row) = self.selected_row() {
                    self.stage = Stage::Label {
                        entry_id: row.id.clone(),
                        buffer: row.label.clone().unwrap_or_default(),
                    };
                }
            }
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.refilter();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        EventOutcome::Consumed
    }

    fn key_summary(&mut self, key: KeyEvent) -> EventOutcome {
        let Stage::Summary { entry_id, cursor } = &mut self.stage else {
            return EventOutcome::Ignored;
        };
        match key.code {
            KeyCode::Up | KeyCode::Down => *cursor = 1 - *cursor,
            KeyCode::Enter => {
                let outcome = TreeOutcome::Navigate {
                    entry_id: std::mem::take(entry_id),
                    summarize: *cursor == 1,
                };
                self.stage = Stage::List;
                self.outcome = Some(outcome);
            }
            // TS: Esc on the summary choice re-shows the tree.
            KeyCode::Esc => self.stage = Stage::List,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stage = Stage::List;
                self.outcome = Some(TreeOutcome::Cancel);
            }
            _ => {}
        }
        EventOutcome::Consumed
    }

    fn key_label(&mut self, key: KeyEvent) -> EventOutcome {
        let Stage::Label { entry_id, buffer } = &mut self.stage else {
            return EventOutcome::Ignored;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let label = Some(buffer.trim().to_owned()).filter(|l| !l.is_empty());
                let outcome = TreeOutcome::Label {
                    entry_id: std::mem::take(entry_id),
                    label,
                };
                self.stage = Stage::List;
                self.outcome = Some(outcome);
            }
            KeyCode::Esc => self.stage = Stage::List,
            KeyCode::Char('c') if ctrl => self.stage = Stage::List,
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) if !ctrl => buffer.push(c),
            _ => {}
        }
        EventOutcome::Consumed
    }

    fn key_rewind(&mut self, key: KeyEvent) -> EventOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.rewind_filter {
            match key.code {
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    self.refilter();
                    return EventOutcome::Consumed;
                }
                KeyCode::Backspace => {
                    if self.query.pop().is_none() {
                        self.rewind_filter = false;
                    }
                    self.refilter();
                    return EventOutcome::Consumed;
                }
                KeyCode::Esc => {
                    self.rewind_filter = false;
                    self.query.clear();
                    self.refilter();
                    return EventOutcome::Consumed;
                }
                _ => {}
            }
        }
        let page = isize::try_from(self.page.get().max(1)).unwrap_or(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1, false),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, false),
            KeyCode::PageUp => self.move_by(-page, false),
            KeyCode::PageDown => self.move_by(page, false),
            KeyCode::Home => self.select_index(0),
            KeyCode::End => self.select_index(usize::MAX),
            KeyCode::Char('c') if ctrl => self.outcome = Some(TreeOutcome::Cancel),
            KeyCode::Char('f') => {
                self.rewind_filter = true;
                self.query.clear();
                self.refilter();
            }
            KeyCode::Enter => self.confirm(false),
            KeyCode::Esc => return EventOutcome::Ignored, // the host closes the overlay
            _ => {}
        }
        EventOutcome::Consumed
    }

    /// One `/tree` row: cursor, gutter, active-path dot, label badge, entry
    /// text; the selected row bold on `selectedBg` (TS `renderLeading` +
    /// `#renderGutter` + `#renderTreeRow` + `styleSelected`).
    fn tree_row_line(
        &self,
        row: &TreeRow,
        selected: bool,
        width: usize,
        deepest: usize,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let fg = |token: &str| Style::default().fg(theme.color(token));
        let mut spans = vec![if selected {
            Span::styled("› ", fg("accent"))
        } else {
            Span::raw("  ")
        }];
        spans.push(Span::styled(
            self.forest.gutter(row, width, deepest, theme),
            fg("dim"),
        ));
        if row.on_path {
            spans.push(Span::styled(
                format!("{} ", theme.symbol("md.bullet")),
                fg("accent"),
            ));
        }
        if let Some(label) = &row.label {
            spans.push(Span::styled(
                format!("[{}] ", one_line(label)),
                fg("warning"),
            ));
        }
        for p in &row.view.parts {
            let mut style = fg(p.token);
            if p.strong {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(p.text.clone(), style));
        }
        let mut spans = truncate_spans(spans, width);
        if selected {
            spans = pad(spans, width);
            let bg = theme.color("selectedBg");
            for span in &mut spans {
                span.style = span.style.bg(bg).add_modifier(Modifier::BOLD);
            }
        }
        spans
    }

    /// The `/tree` body rows (list or empty state), `rows` tall, `width` wide.
    fn tree_body(&self, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let fg = |token: &str| Style::default().fg(theme.color(token));
        let total = self.visible.len();
        if total == 0 {
            let (head, tail) = if self.forest.rows.is_empty() {
                ("No entries found".to_owned(), "(0/0)".to_owned())
            } else if !self.query.trim().is_empty() {
                (
                    format!("No entries match search \"{}\"", self.query.trim()),
                    "Press backspace to clear the search".to_owned(),
                )
            } else {
                (
                    format!(
                        "{} entries hidden by the current filter [{}]",
                        self.forest.rows.len(),
                        self.filter.id()
                    ),
                    format!(
                        "Press {} to show all, {} for default",
                        format_key_hint("alt+a", theme),
                        format_key_hint("alt+d", theme)
                    ),
                )
            };
            return vec![
                Line::from(Span::styled(head, fg("text_muted"))),
                Line::from(Span::styled(tail, fg("dim"))),
            ];
        }
        let window = rows.min(total);
        let selected = self.selection_index().unwrap_or(0);
        let start = selected.saturating_sub(window / 2).min(total - window);
        let overflow = total > window;
        let row_width = if overflow {
            width.saturating_sub(1)
        } else {
            width
        };
        let deepest = self.visible[start..start + window]
            .iter()
            .map(|&r| self.forest.rows[r].depth)
            .max()
            .unwrap_or(0);
        // Scrollbar thumb (TS `scrollbarThumbRange`).
        let thumb = (window * window / total).clamp(1, window);
        let travel = window - thumb;
        let max_offset = total - window;
        let thumb_start = (start * travel + max_offset / 2)
            .checked_div(max_offset)
            .unwrap_or(0);
        self.visible[start..start + window]
            .iter()
            .enumerate()
            .map(|(i, &r)| {
                let row = &self.forest.rows[r];
                let mut spans =
                    self.tree_row_line(row, self.selected == Some(r), row_width, deepest, theme);
                if overflow {
                    spans = pad(spans, row_width);
                    let in_thumb = i >= thumb_start && i < thumb_start + thumb;
                    spans.push(if in_thumb {
                        Span::styled("█", fg("accent"))
                    } else {
                        Span::styled("│", fg("text_muted"))
                    });
                }
                Line::from(spans)
            })
            .collect()
    }

    /// The `/tree` key hints (TS hint row; unbound branch keys are left out).
    fn hint_text(&self, theme: &Theme) -> String {
        let k = |key: &str| format_key_hint(key, theme);
        let mut hints = vec![
            format!("{} switch", k("enter")),
            format!("{}/{} turn", k("alt+up"), k("alt+down")),
        ];
        let branch: Vec<String> = ["app.tree.previousBranch", "app.tree.nextBranch"]
            .iter()
            .map(|id| self.keys.display_string(id, theme))
            .filter(|s| !s.is_empty())
            .collect();
        if !branch.is_empty() {
            hints.push(format!("{} branch", branch.join("/")));
        }
        hints.extend([
            format!(
                "{}/{} ({}/{}) page",
                k("pageUp"),
                k("pageDown"),
                k("left"),
                k("right")
            ),
            format!("{}/{} first/last", k("home"), k("end")),
            format!("{} summarize & switch", k("shift+enter")),
            format!("{} label", k("shift+l")),
            format!("{} filter", k("ctrl+o")),
            "type to search".to_owned(),
        ]);
        hints.join(" · ")
    }

    fn render_tree(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let fg = |token: &str| Style::default().fg(theme.color(token));
        frame.render_widget(Clear, area);
        let title = match &self.session_name {
            Some(name) => format!(" Session Tree · {} ", one_line(name)),
            None => " Session Tree ".to_owned(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fg("border"))
            .title(Span::styled(title, fg("accent")));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        // Seven rows keep both "Summarize branch?" choices on screen.
        if width < 8 || height < 7 {
            return;
        }
        let divider = || Line::from(Span::styled("─".repeat(width), fg("border")));

        let mut lines: Vec<Line<'static>> = Vec::new();
        // Filter tabs (TS `FILTER_TABS`).
        let mut tabs: Vec<Span<'static>> = Vec::new();
        for (i, mode) in FilterMode::CYCLE.into_iter().enumerate() {
            if i > 0 {
                tabs.push(Span::raw("  "));
            }
            tabs.push(if mode == self.filter {
                Span::styled(
                    mode.label(),
                    fg("accent").add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )
            } else {
                Span::styled(mode.label(), fg("text_muted"))
            });
        }
        lines.push(Line::from(truncate_spans(tabs, width)));
        // Search line (TS `SearchLine`).
        let mut search = vec![Span::styled("Search: ", fg("text_muted"))];
        if self.query.is_empty() {
            search.push(Span::styled("Type to search", fg("dim")));
        } else {
            search.push(Span::styled(format!("{}▏", self.query), fg("text")));
        }
        lines.push(Line::from(truncate_spans(search, width)));
        lines.push(divider());

        let footer = if matches!(self.stage, Stage::Label { .. }) {
            3
        } else {
            2
        };
        let body_rows = height.saturating_sub(3 + footer).max(1);
        self.page.set(body_rows);
        let mut body = match &self.stage {
            Stage::Summary { cursor, .. } => {
                let mut rows = Vec::new();
                // Both choices stay visible; the heading only when it fits too.
                if body_rows >= 3 {
                    rows.push(Line::from(Span::styled(
                        "Summarize branch?",
                        fg("accent").add_modifier(Modifier::BOLD),
                    )));
                }
                for (i, label) in ["No summary", "Summarize"].into_iter().enumerate() {
                    rows.push(if i == *cursor {
                        Line::from(Span::styled(
                            format!("{} {label}", theme.glyph("prompt")),
                            fg("accent").add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        Line::from(Span::styled(format!("  {label}"), fg("text")))
                    });
                }
                rows
            }
            _ => self.tree_body(body_rows, width, theme),
        };
        body.truncate(body_rows);
        body.resize(body_rows, Line::default());
        lines.extend(body);
        lines.push(divider());
        let hint = match &self.stage {
            Stage::Label { buffer, .. } => {
                let editor = vec![
                    Span::styled("Label: ", fg("accent")),
                    Span::styled(format!("{buffer}▏"), fg("text")),
                ];
                lines.push(Line::from(truncate_spans(editor, width)));
                format!(
                    "{} save · {} cancel · empty clears the label",
                    format_key_hint("enter", theme),
                    format_key_hint("escape", theme)
                )
            }
            Stage::Summary { .. } => format!(
                "{}/{} choose · {} switch · {} back",
                format_key_hint("up", theme),
                format_key_hint("down", theme),
                format_key_hint("enter", theme),
                format_key_hint("escape", theme)
            ),
            Stage::List => self.hint_text(theme),
        };
        lines.push(Line::from(truncate_spans(
            vec![Span::styled(hint, fg("text_muted"))],
            width,
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_rewind(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let popup = super::centered(area, 70, 70);
        if popup.width < 6 || popup.height < 4 {
            return;
        }
        frame.render_widget(Clear, popup);
        let title = if self.rewind_filter {
            format!(" rewind — filter: {}▏ ", self.query)
        } else {
            " rewind ".to_owned()
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.color("border")));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if self.visible.is_empty() {
            let msg = Line::from(Span::styled(
                if self.rewind_filter {
                    "No turns match"
                } else {
                    "No turns yet"
                },
                Style::default().fg(theme.color("dim")),
            ));
            frame.render_widget(Paragraph::new(msg), inner);
            return;
        }
        let height = usize::from(inner.height);
        self.page.set(height.max(1));
        let body = self.visible.len().min(height);
        let cursor = self.selection_index().unwrap_or(0);
        let start = cursor
            .saturating_sub(body.saturating_sub(1))
            .min(self.visible.len() - body);
        let width = usize::from(inner.width);
        let lines: Vec<Line<'static>> = self.visible[start..start + body]
            .iter()
            .map(|&i| {
                let row = &self.forest.rows[i];
                let sel = self.selected == Some(i);
                let (marker, token) = if sel {
                    ("> ", "accent")
                } else {
                    ("  ", "text")
                };
                Line::from(truncate_spans(
                    vec![Span::styled(
                        format!("{marker}{}", one_line(&row.text)),
                        Style::default().fg(theme.color(token)),
                    )],
                    width,
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl Component for TreeNavigator {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        match self.mode {
            TreeMode::Tree => self.render_tree(frame, area, theme),
            TreeMode::Rewind => self.render_rewind(frame, area, theme),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        match (self.mode, &self.stage) {
            (TreeMode::Rewind, _) => self.key_rewind(key),
            (TreeMode::Tree, Stage::List) => self.key_tree(key),
            (TreeMode::Tree, Stage::Summary { .. }) => self.key_summary(key),
            (TreeMode::Tree, Stage::Label { .. }) => self.key_label(key),
        }
    }

    fn wants_focus(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn msg(id: &str, parent: Option<&str>, ts: &str, role: &str, content: Value) -> Value {
        json!({"type": "message", "id": id, "parentId": parent, "timestamp": ts,
               "message": {"role": role, "content": content}})
    }

    /// u1 → a1 → {u2 → a2, u3 → a3 (leaf)}: one fork under `a1`.
    fn forked() -> SessionJournal {
        SessionJournal {
            entries: vec![
                msg("u1", None, "01", "user", json!("first question")),
                msg("a1", Some("u1"), "02", "assistant", json!("answer one")),
                msg("u2", Some("a1"), "03", "user", json!("follow up A")),
                msg("a2", Some("u2"), "04", "assistant", json!("A done")),
                msg("u3", Some("a1"), "05", "user", json!("follow up B")),
                msg("a3", Some("u3"), "06", "assistant", json!("B done")),
            ],
            leaf: Some("a3".into()),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn tree(journal: &SessionJournal) -> TreeNavigator {
        let mut nav = TreeNavigator::default();
        nav.open(journal, TreeMode::Tree, TreeOptions::default());
        nav
    }

    fn render(nav: &TreeNavigator, width: u16, height: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| nav.render(f, f.area(), &Theme::default()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn active_branch_first_and_only_forks_indent() {
        let nav = tree(&forked());
        // The branch holding the leaf (u3) sorts before its older sibling.
        assert_eq!(nav.visible_ids(), ["u1", "a1", "u3", "a3", "u2", "a2"]);
        let depth: Vec<usize> = nav.forest.rows.iter().map(|r| r.depth).collect();
        assert_eq!(depth, [0, 0, 1, 1, 1, 1]);
        let on_path: Vec<&str> = nav
            .forest
            .rows
            .iter()
            .filter(|r| r.on_path)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(on_path, ["u1", "a1", "u3", "a3"]);
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("a3"));
    }

    #[test]
    fn gutter_draws_connectors_at_the_fork_and_continuations_below() {
        let nav = tree(&forked());
        let theme = Theme::default();
        let gutters: Vec<String> = nav
            .forest
            .rows
            .iter()
            .map(|r| nav.forest.gutter(r, 80, 1, &theme))
            .collect();
        assert_eq!(gutters, ["", "", "├─ ", "│  ", "└─ ", "   "]);
    }

    #[test]
    fn deep_gutters_compress_behind_an_ellipsis() {
        // A chain of forks: each assistant answer has two children.
        let mut entries = vec![msg("r", None, "00", "user", json!("root"))];
        let mut parent = "r".to_owned();
        for i in 0..12 {
            let keep = format!("k{i}");
            let side = format!("s{i}");
            entries.push(msg(
                &side,
                Some(&parent),
                &format!("{:02}a", i),
                "user",
                json!("side"),
            ));
            entries.push(msg(
                &keep,
                Some(&parent),
                &format!("{:02}b", i),
                "user",
                json!("keep"),
            ));
            parent = keep;
        }
        let journal = SessionJournal {
            leaf: Some(parent.clone()),
            entries,
        };
        let nav = tree(&journal);
        let theme = Theme::default();
        let deepest = nav.forest.rows.iter().map(|r| r.depth).max().unwrap();
        let row = nav.forest.rows.iter().find(|r| r.id == parent).unwrap();
        let gutter = nav.forest.gutter(row, 40, deepest, &theme);
        assert!(gutter.starts_with('…'), "{gutter:?}");
        // Budget: max(24, 40/2) content columns + 4 overhead leave 4 levels.
        assert_eq!(gutter.chars().count(), 4 * 3);
    }

    #[test]
    fn default_filter_hides_bookkeeping_and_hollow_assistant_turns() {
        let mut journal = forked();
        journal.entries.insert(
            2,
            json!({"type": "model_change", "id": "m1", "parentId": "a1", "timestamp": "02b", "model": "x/y"}),
        );
        journal.entries.push(
            json!({"type": "message", "id": "t1", "parentId": "a3", "timestamp": "07",
            "message": {"role": "assistant", "content": [], "stopReason": "toolUse"}}),
        );
        journal.entries.push(json!({"type": "message", "id": "r1", "parentId": "t1", "timestamp": "08",
            "message": {"role": "toolResult", "toolName": "bash", "toolCallId": "c1", "content": "ok"}}));
        journal.leaf = Some("r1".into());
        let mut nav = tree(&journal);
        let ids = nav.visible_ids();
        assert!(!ids.contains(&"m1") && !ids.contains(&"t1"), "{ids:?}");
        assert!(ids.contains(&"r1"));
        nav.handle_key(with(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(nav.filter_mode(), FilterMode::NoTools);
        assert!(!nav.visible_ids().contains(&"r1"));
        nav.handle_key(with(KeyCode::Char('u'), KeyModifiers::ALT));
        assert_eq!(nav.visible_ids(), ["u1", "u3", "u2"]);
        nav.handle_key(with(KeyCode::Char('a'), KeyModifiers::ALT));
        let all = nav.visible_ids();
        // TS hides text-less tool-call turns in every mode, `all` included.
        assert!(all.contains(&"m1") && !all.contains(&"t1"), "{all:?}");
        // ctrl+o cycles forward from `all` back to `default`, ctrl+shift+o back.
        nav.handle_key(with(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(nav.filter_mode(), FilterMode::Default);
        nav.handle_key(with(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(nav.filter_mode(), FilterMode::All);
    }

    #[test]
    fn search_narrows_rows_and_esc_clears_it_before_closing() {
        let mut nav = tree(&forked());
        for c in "follow".chars() {
            nav.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(nav.visible_ids(), ["u3", "u2"]);
        assert!(nav.handle_key(key(KeyCode::Esc)).consumed());
        assert_eq!(nav.visible_ids().len(), 6);
        // With no query, Esc bubbles so the host closes the overlay.
        assert!(!nav.handle_key(key(KeyCode::Esc)).consumed());
        assert_eq!(nav.take_outcome(), None);
    }

    #[test]
    fn navigation_keys_never_pick_and_enter_navigates() {
        let mut nav = tree(&forked());
        nav.handle_key(key(KeyCode::Up));
        nav.handle_key(key(KeyCode::Down));
        nav.handle_key(key(KeyCode::Up));
        assert_eq!(nav.take_outcome(), None, "moving the cursor is not a pick");
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u3"));
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(
            nav.take_outcome(),
            Some(TreeOutcome::Navigate {
                entry_id: "u3".into(),
                summarize: false
            })
        );
        nav.handle_key(with(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(
            nav.take_outcome(),
            Some(TreeOutcome::Navigate {
                entry_id: "u3".into(),
                summarize: true
            })
        );
    }

    #[test]
    fn enter_on_the_live_leaf_is_already_here() {
        let mut nav = tree(&forked());
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(nav.take_outcome(), Some(TreeOutcome::AlreadyHere));
    }

    #[test]
    fn up_down_wrap_and_alt_arrows_step_turns() {
        let mut nav = tree(&forked());
        nav.handle_key(key(KeyCode::Home));
        nav.handle_key(key(KeyCode::Up));
        assert_eq!(
            nav.selected_row().map(|r| r.id.as_str()),
            Some("a2"),
            "wraps"
        );
        nav.handle_key(with(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u2"));
        nav.handle_key(key(KeyCode::End));
        nav.handle_key(with(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(
            nav.selected_row().map(|r| r.id.as_str()),
            Some("a2"),
            "no wrap"
        );
    }

    #[test]
    fn bound_branch_keys_jump_between_branches() {
        let mut user = crate::keybindings::UserBindings::new();
        user.insert("app.tree.nextBranch".into(), vec!["shift+right".into()]);
        user.insert("app.tree.previousBranch".into(), vec!["shift+left".into()]);
        let mut nav = TreeNavigator::default();
        nav.open(
            &forked(),
            TreeMode::Tree,
            TreeOptions {
                keys: KeybindingsManager::new(user),
                ..TreeOptions::default()
            },
        );
        // From the leaf a3 (inside branch u3) the next branch is u2.
        nav.handle_key(with(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u2"));
        nav.handle_key(with(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(
            nav.selected_row().map(|r| r.id.as_str()),
            Some("u3"),
            "wraps"
        );
        nav.handle_key(with(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u2"));
        // Plain Right still pages.
        nav.handle_key(key(KeyCode::Home));
        nav.handle_key(key(KeyCode::Right));
        assert_ne!(nav.selected_row().map(|r| r.id.as_str()), Some("u1"));
    }

    #[test]
    fn summary_prompt_asks_before_navigating() {
        let mut nav = TreeNavigator::default();
        nav.open(
            &forked(),
            TreeMode::Tree,
            TreeOptions {
                summary_prompt: true,
                ..TreeOptions::default()
            },
        );
        nav.handle_key(key(KeyCode::Up));
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(nav.take_outcome(), None, "the summary choice comes first");
        let screen = render(&nav, 80, 20).join("\n");
        assert!(screen.contains("Summarize branch?"), "{screen}");
        nav.handle_key(key(KeyCode::Esc));
        assert_eq!(nav.stage, Stage::List, "esc returns to the tree");
        nav.handle_key(key(KeyCode::Enter));
        nav.handle_key(key(KeyCode::Down));
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(
            nav.take_outcome(),
            Some(TreeOutcome::Navigate {
                entry_id: "u3".into(),
                summarize: true
            })
        );
    }

    #[test]
    fn shift_l_edits_the_selected_label_and_reload_shows_it() {
        let mut journal = forked();
        let mut nav = tree(&journal);
        nav.handle_key(key(KeyCode::Up));
        nav.handle_key(with(KeyCode::Char('L'), KeyModifiers::SHIFT));
        for c in "keep".chars() {
            nav.handle_key(key(KeyCode::Char(c)));
        }
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(
            nav.take_outcome(),
            Some(TreeOutcome::Label {
                entry_id: "u3".into(),
                label: Some("keep".into())
            })
        );
        journal
            .entries
            .push(json!({"type": "label", "id": "l1", "parentId": "a3",
            "timestamp": "09", "targetId": "u3", "label": "keep"}));
        journal.leaf = Some("l1".into());
        nav.reload(&journal);
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u3"));
        nav.handle_key(with(KeyCode::Char('l'), KeyModifiers::ALT));
        assert_eq!(nav.visible_ids(), ["u3"]);
        let screen = render(&nav, 80, 16).join("\n");
        assert!(screen.contains("[keep] user: follow up B"), "{screen}");
    }

    #[test]
    fn renders_title_tabs_rows_and_markers() {
        let mut nav = TreeNavigator::default();
        nav.open(
            &forked(),
            TreeMode::Tree,
            TreeOptions {
                session_name: Some("demo".into()),
                ..TreeOptions::default()
            },
        );
        let screen = render(&nav, 90, 20);
        let text = screen.join("\n");
        assert!(text.contains("Session Tree · demo"), "{text}");
        assert!(
            text.contains("Default  No tools  User only  Labeled  All"),
            "{text}"
        );
        assert!(text.contains("Search: Type to search"), "{text}");
        assert!(text.contains("├─ • user: follow up B"), "{text}");
        assert!(text.contains("│  • assistant: B done"), "{text}");
        assert!(text.contains("└─ user: follow up A"), "{text}");
        // The selected row (the live leaf) carries the cursor.
        assert!(
            screen
                .iter()
                .any(|l| l.contains("› │  • assistant: B done")),
            "{text}"
        );
    }

    #[test]
    fn empty_states_explain_what_hides_the_rows() {
        let mut nav = tree(&forked());
        for c in "zzz".chars() {
            nav.handle_key(key(KeyCode::Char(c)));
        }
        let text = render(&nav, 80, 14).join("\n");
        assert!(text.contains("No entries match search \"zzz\""), "{text}");
        let mut nav = tree(&forked());
        nav.handle_key(with(KeyCode::Char('l'), KeyModifiers::ALT));
        let text = render(&nav, 80, 14).join("\n");
        assert!(
            text.contains("6 entries hidden by the current filter [labeled-only]"),
            "{text}"
        );
        let nav = tree(&SessionJournal::default());
        let text = render(&nav, 80, 14).join("\n");
        assert!(text.contains("No entries found"), "{text}");
    }

    #[test]
    fn tool_results_show_their_call() {
        let journal = SessionJournal {
            entries: vec![
                msg("u1", None, "01", "user", json!("go")),
                msg(
                    "a1",
                    Some("u1"),
                    "02",
                    "assistant",
                    json!([
                    {"type": "toolCall", "id": "c1", "name": "read",
                     "arguments": {"path": "/etc/hosts", "offset": 3, "limit": 5}}]),
                ),
                json!({"type": "message", "id": "r1", "parentId": "a1", "timestamp": "03",
                    "message": {"role": "toolResult", "toolCallId": "c1", "toolName": "read", "content": "x"}}),
            ],
            leaf: Some("r1".into()),
        };
        let nav = tree(&journal);
        let text = render(&nav, 80, 14).join("\n");
        assert!(text.contains("[read: /etc/hosts:3-7]"), "{text}");
    }

    #[test]
    fn rewind_column_moves_without_picking_and_enter_picks() {
        let mut nav = TreeNavigator::default();
        nav.open(&forked(), TreeMode::Rewind, TreeOptions::default());
        assert_eq!(nav.visible_ids(), ["u1", "u3"]);
        assert_eq!(nav.selected_row().map(|r| r.id.as_str()), Some("u3"));
        nav.handle_key(key(KeyCode::Up));
        nav.handle_key(key(KeyCode::Char('j')));
        nav.handle_key(key(KeyCode::Char('k')));
        assert_eq!(nav.take_outcome(), None);
        nav.handle_key(key(KeyCode::Char('f')));
        nav.handle_key(key(KeyCode::Char('z')));
        assert!(nav.visible_ids().is_empty());
        assert!(
            nav.handle_key(key(KeyCode::Esc)).consumed(),
            "esc closes the filter first"
        );
        assert_eq!(nav.visible_ids().len(), 2);
        nav.handle_key(key(KeyCode::Enter));
        assert_eq!(
            nav.take_outcome(),
            Some(TreeOutcome::Navigate {
                entry_id: "u1".into(),
                summarize: false
            })
        );
        let text = render(&nav, 60, 12).join("\n");
        assert!(
            text.contains("rewind") && text.contains("> first question"),
            "{text}"
        );
    }
}
