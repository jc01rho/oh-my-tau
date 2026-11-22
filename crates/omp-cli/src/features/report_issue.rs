#![allow(dead_code)]
//! `report_issue` — automated QA backend for tracking unexpected tool behavior.
//!
//! Port of `packages/coding-agent/src/tools/report-tool-issue.ts`. There is no
//! model-facing tool schema: the `write` tool dispatches plain text to
//! `xd://report_issue`, and the system prompt tells the model to write
//! `<tool>: <concise description>` there when auto-QA is enabled.
//!
//! Enabled by default (`dev.autoqa` defaults to true); `PI_AUTO_QA=0` or an
//! explicit `dev.autoqa: false` short-circuits injection entirely. When the
//! user is only enabled by default (never configured `dev.autoqa` themselves),
//! a persisted `dev.autoqaConsent: "denied"` also disables injection so a "No"
//! in the consent dialog fully turns the feature off.
//!
//! Grievances are recorded to a local SQLite database (`~/.omp/autoqa.db`) and
//! pushed to the bundled collector in batches. Nothing is written until consent
//! resolves; a denial (or dismissal) drops the pending report without touching
//! the database. `PI_AUTO_QA_PUSH=1` bypasses the dialog for headless
//! environments. Device execution never blocks on the network and never throws
//! from the dispatch path.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, FutureExt, Shared};
use rusqlite::{params, Connection, OptionalExtension};

/// Device name for `write xd://report_issue`.
pub const REPORT_ISSUE_DEVICE_NAME: &str = "report_issue";
/// Full URL of the auto-QA device.
pub const REPORT_ISSUE_DEVICE_PATH: &str = "xd://report_issue";

/// Maximum `tool` length the collector accepts, in UTF-8 bytes. An oversized
/// entry makes the collector reject the *whole* batch with HTTP 400, so
/// grievances are clamped at record time and again at send time — rows recorded
/// before the clamp existed still sit in users' databases.
pub const MAX_TOOL_BYTES: usize = 128;

/// Default collector endpoint, mirroring `dev.autoqaPush.endpoint`.
pub const DEFAULT_PUSH_ENDPOINT: &str = "https://qa.omp.sh";

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const FAILURE_COOLDOWN_MS: u64 = 30_000;
/// Per-request batch size. The worker loops until no unpushed rows remain.
const FLUSH_BATCH_SIZE: usize = 50;
const MAX_PUSH_ERROR_CHARS: usize = 300;

/// Usage text for `read xd://report_issue`.
pub fn report_issue_device_usage() -> String {
    format!(
        "Write `<tool>: <concise description>` as plain text to {REPORT_ISSUE_DEVICE_PATH}. \
         A two-line fallback also works: tool name on line 1, report body below."
    )
}

/// Whether a tool call writes to `xd://report_issue`.
pub fn is_report_issue_tool_call(tool_name: &str, path: Option<&str>) -> bool {
    if tool_name != "write" {
        return false;
    }
    match path {
        Some(p) => p == REPORT_ISSUE_DEVICE_PATH || p == "xd://report_issue/",
        None => false,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Body parsing
// ───────────────────────────────────────────────────────────────────────────

/// A parsed and normalized grievance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grievance {
    pub tool: String,
    pub report: String,
}

/// Parse failures surfaced to the model. Both variants carry the usage text,
/// matching `ToolError` in the TypeScript implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportIssueError {
    EmptyReport,
    InvalidFormat,
}

impl std::fmt::Display for ReportIssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyReport => write!(f, "Empty report. {}", report_issue_device_usage()),
            Self::InvalidFormat => write!(f, "Invalid report format. {}", report_issue_device_usage()),
        }
    }
}

impl std::error::Error for ReportIssueError {}

const JSON_TOOL_KEYS: &[&str] = &["tool", "toolName", "tool_name", "name"];
const JSON_REPORT_KEYS: &[&str] = &[
    "report",
    "description",
    "issue",
    "error",
    "message",
    "problem",
    "details",
    "body",
];

fn first_string(record: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(serde_json::Value::String(value)) = record.get(*key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// A JSON body (`{"tool": "read", "description": "…"}`), as agents write to
/// every other device. Returns `Ok(None)` when the text is not valid JSON, so
/// the caller can fall back to the line-oriented formats.
fn parse_json_report_body(body: &str) -> Result<Option<Grievance>, ReportIssueError> {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let record = match &parsed {
        serde_json::Value::Object(map) => map,
        _ => return Err(ReportIssueError::InvalidFormat),
    };
    let tool = first_string(record, JSON_TOOL_KEYS);
    let report = first_string(record, JSON_REPORT_KEYS);
    match (tool, report) {
        (Some(tool), Some(report)) => Ok(Some(normalize_grievance(&tool, &report))),
        _ => Err(ReportIssueError::InvalidFormat),
    }
}

/// Parse the plain-text body written to `xd://report_issue`. Accepts, in order:
/// a JSON object, tool name on the first line with the report below, or
/// `<tool>: <description>` on one line.
pub fn parse_report_issue_body(text: &str) -> Result<Grievance, ReportIssueError> {
    let body = text.trim();
    if body.is_empty() {
        return Err(ReportIssueError::EmptyReport);
    }
    if body.starts_with('{') || body.starts_with('[') {
        if let Some(json) = parse_json_report_body(body)? {
            return Ok(json);
        }
    }
    if let Some(first_newline) = body.find('\n') {
        let tool = body[..first_newline].trim();
        let report = body[first_newline + 1..].trim();
        if !tool.is_empty() && !report.is_empty() {
            return Ok(normalize_grievance(tool, report));
        }
    }
    // Only a colon followed by whitespace separates, so a URI (`xd://tui`) or an
    // unspaced argument (`{name:'x'}`) in the tool name stays in the tool name.
    if let Some(colon) = find_colon_space(body) {
        let tool = body[..colon].trim();
        let report = body[colon + 1..].trim();
        if !tool.is_empty() && !report.is_empty() {
            return Ok(normalize_grievance(tool, report));
        }
    }
    Err(ReportIssueError::InvalidFormat)
}

/// Byte index of the first `:` that is followed by whitespace and is not at
/// position 0 (a leading colon would leave an empty tool name).
fn find_colon_space(body: &str) -> Option<usize> {
    for (idx, ch) in body.char_indices() {
        if ch != ':' || idx == 0 {
            continue;
        }
        if let Some(next) = body[idx + 1..].chars().next() {
            if next.is_whitespace() {
                return Some(idx);
            }
        }
    }
    None
}

// ───────────────────────────────────────────────────────────────────────────
// Redaction + clamping
// ───────────────────────────────────────────────────────────────────────────

/// Longest prefix of `text` that is at most `max` bytes and ends on a char
/// boundary.
fn truncate_head_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn char_lower_eq(c: char, lower: char) -> bool {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(a), None) => a == lower,
        _ => false,
    }
}

/// Byte index just past a case-insensitive match of `needle_lower` at `start`.
fn ci_match_end(hay: &str, start: usize, needle_lower: &str) -> Option<usize> {
    if !hay.is_char_boundary(start) {
        return None;
    }
    let mut idx = start;
    for want in needle_lower.chars() {
        let c = hay[idx..].chars().next()?;
        if !char_lower_eq(c, want) {
            return None;
        }
        idx += c.len_utf8();
    }
    Some(idx)
}

fn prev_char(text: &str, at: usize) -> Option<char> {
    text[..at].chars().next_back()
}

fn next_char(text: &str, at: usize) -> Option<char> {
    text[at..].chars().next()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

fn ends_with_ci(hay: &str, pat: &str) -> bool {
    if pat.is_empty() || hay.len() < pat.len() {
        return false;
    }
    let lower = pat.to_lowercase();
    let mut start = hay.len() - pat.len();
    while start > 0 && !hay.is_char_boundary(start) {
        start -= 1;
    }
    ci_match_end(hay, start, &lower) == Some(hay.len())
}

/// Rewrite every occurrence of `needle` (case-insensitive) whose surrounding
/// characters mark it as a standalone path, not a substring of a longer word.
fn replace_home_form(hay: &str, needle: &str, replacement: &str) -> String {
    let lower = needle.to_lowercase();
    let mut out = String::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if let Some(end) = ci_match_end(hay, i, &lower) {
            let prefix_ok = match prev_char(hay, i) {
                None => true,
                Some(c) => !(is_word_char(c) || c == '_' || c == '.' || c == '-'),
            };
            let suffix_ok = match next_char(hay, end) {
                None => true,
                Some(c) if is_word_char(c) || c == '_' || c == '-' => false,
                Some('.') => match next_char(hay, end + 1) {
                    Some(after) => !is_word_char(after),
                    None => true,
                },
                Some(_) => true,
            };
            if prefix_ok && suffix_ok {
                out.push_str(replacement);
                i = end;
                continue;
            }
        }
        let c = hay[i..].chars().next().expect("index is a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Whether the username occurrence `[start, end)` sits in a position that names
/// the user: a home path (`/home/<user>`, `-Users-<user>-`, `~<user>`), a
/// reverse-DNS label (`com.<user>.app`) or `<user>@host`.
fn user_occurrence_matches(hay: &str, start: usize, end: usize) -> bool {
    let before = &hay[..start];
    let after = next_char(hay, end);

    let prefixes = [
        "Users/", "Users\\", "Users-", "home/", "home\\", "home-", "~", "com.", "org.", "net.", "io.",
        "dev.",
    ];
    for pat in prefixes {
        if !ends_with_ci(before, pat) {
            continue;
        }
        if pat == "~" {
            if matches!(after, Some(c) if !is_word_char(c)) {
                return true;
            }
            continue;
        }
        let keyword_start = before.len().saturating_sub(pat.len());
        if !before.is_char_boundary(keyword_start) {
            continue;
        }
        let guard_ok = match prev_char(before, keyword_start) {
            None => true,
            Some(c) => !is_word_char(c),
        };
        if !guard_ok {
            continue;
        }
        if matches!(after, Some(c) if !is_word_char(c)) {
            return true;
        }
    }

    let start_ok = match prev_char(hay, start) {
        None => true,
        Some(c) => !is_word_char(c),
    };
    start_ok && after == Some('@')
}

/// Remove the user's identity from grievance text. Reports quote paths and
/// commands verbatim and leave the machine, so the home directory becomes `~`
/// and the login name becomes `<user>` in the forms that name the user. Logins
/// such as `will`, `node`, `bin` or `test` are also words, so elsewhere the name
/// is left alone. `root` names no one, so only its home is rewritten.
pub fn redact_identity(text: &str, home: Option<&str>, user: Option<&str>) -> String {
    let mut redacted = text.to_string();
    if let Some(home) = home {
        if home.chars().count() > 1 {
            let mut forms: Vec<String> = vec![home.to_string()];
            let slashed = home.replace('\\', "/");
            if slashed != home {
                forms.push(slashed);
            }
            for form in forms {
                redacted = replace_home_form(&redacted, &form, "~");
            }
        }
    }
    if let Some(user) = user {
        if !user.is_empty() && user != "root" {
            let lower = user.to_lowercase();
            let mut out = String::with_capacity(redacted.len());
            let mut i = 0;
            while i < redacted.len() {
                if let Some(end) = ci_match_end(&redacted, i, &lower) {
                    if user_occurrence_matches(&redacted, i, end) {
                        out.push_str("<user>");
                        i = end;
                        continue;
                    }
                }
                let c = redacted[i..].chars().next().expect("index is a char boundary");
                out.push(c);
                i += c.len_utf8();
            }
            redacted = out;
        }
    }
    redacted
}

fn env_string(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Redact the current process's home directory and login name.
pub fn redact_user_identity(text: &str) -> String {
    let home = env_string("HOME").or_else(|| env_string("USERPROFILE"));
    let user = env_string("USER").or_else(|| env_string("USERNAME")).or_else(|| {
        env_string("HOME").and_then(|h| {
            h.rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
    });
    redact_identity(text, home.as_deref(), user.as_deref())
}

/// Prepare a grievance for storage and for the collector: redact the user's
/// identity, then clamp to the collector's field limits.
///
/// An over-long `tool` is prose the model put on line 1 instead of a tool name,
/// so the original line is kept at the head of the report and only the stored
/// name is truncated: nothing is lost and the row stops being a poison pill.
pub fn normalize_grievance(raw_tool: &str, raw_report: &str) -> Grievance {
    let tool = redact_user_identity(raw_tool);
    let report = redact_user_identity(raw_report);
    if tool.len() <= MAX_TOOL_BYTES {
        return Grievance { tool, report };
    }
    Grievance {
        tool: truncate_head_bytes(&tool, MAX_TOOL_BYTES).to_string(),
        report: format!("{tool}\n{report}"),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tool provenance
// ───────────────────────────────────────────────────────────────────────────

/// Built-in tool IDs, mirroring `BUILTIN_TOOL_NAMES`.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "edit",
    "ast_grep",
    "ast_edit",
    "ask",
    "debug",
    "eval",
    "github",
    "glob",
    "grep",
    "find",
    "lsp",
    "checkpoint",
    "rewind",
    "context_notes",
    "new_context",
    "security_scan",
    "task",
    "wait",
    "todo",
    "web_search",
    "write",
    "memory_edit",
    "retain",
    "recall",
    "reflect",
    "learn",
    "manage_skill",
];

/// Hidden built-in tool IDs, mirroring `HIDDEN_TOOL_NAMES`.
pub const HIDDEN_TOOL_NAMES: &[&str] = &["yield", "goal", "think"];

/// MCP tool names carry the `mcp__<server>_<tool>` prefix.
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn is_builtin_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name) || HIDDEN_TOOL_NAMES.contains(&name)
}

/// Registry snapshot needed to attribute a report. Hosts fill this from the
/// live tool registry; tests build it directly.
#[derive(Debug, Default, Clone)]
pub struct ToolCatalog {
    /// `tool name -> mcp server name` for tools registered by an MCP server.
    pub mcp_servers: HashMap<String, String>,
    /// Every registered tool name (built-in, extension or MCP).
    pub registered: HashSet<String>,
    /// Namespaces advertised by the eval tool's preludes.
    pub eval_namespaces: Vec<String>,
}

/// Where the reported tool comes from, so the collector can route third-party
/// reports: `builtin`, `eval:<namespace>`, `mcp:<server>`, `extension`, or
/// `unknown` (CLIs, guesses).
pub fn resolve_tool_source(catalog: &ToolCatalog, tool: &str) -> String {
    let name = tool.strip_prefix("xd://").unwrap_or(tool);
    let name = name.strip_prefix("proxy_").unwrap_or(name);
    if let Some(server) = catalog.mcp_servers.get(name) {
        return format!("mcp:{server}");
    }
    if is_mcp_tool_name(name) {
        return "mcp".to_string();
    }
    if is_builtin_tool_name(name) {
        return "builtin".to_string();
    }
    let namespace = name.split('.').next().unwrap_or(name);
    if catalog.eval_namespaces.iter().any(|ns| ns == namespace) {
        return format!("eval:{namespace}");
    }
    if catalog.registered.contains(name) {
        return "extension".to_string();
    }
    "unknown".to_string()
}

// ───────────────────────────────────────────────────────────────────────────
// Settings + consent gate
// ───────────────────────────────────────────────────────────────────────────

/// Persisted `dev.autoqaConsent` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Consent {
    #[default]
    Unset,
    Granted,
    Denied,
}

impl Consent {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "granted" | "grant" | "yes" | "true" | "1" => Self::Granted,
            "denied" | "deny" | "no" | "false" | "0" => Self::Denied,
            _ => Self::Unset,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Granted => "granted",
            Self::Denied => "denied",
        }
    }

    fn as_bool(self) -> Option<bool> {
        match self {
            Self::Unset => None,
            Self::Granted => Some(true),
            Self::Denied => Some(false),
        }
    }
}

/// The subset of settings the auto-QA pipeline reads.
#[derive(Debug, Clone, Default)]
pub struct AutoQaSettings {
    /// `dev.autoqa`; `None` means "not configured" (defaults on).
    pub autoqa: Option<bool>,
    /// `dev.autoqaConsent`.
    pub autoqa_consent: Consent,
    /// `dev.autoqaPush.endpoint`.
    pub push_endpoint: Option<String>,
    /// `dev.autoqaPush.token`.
    pub push_token: Option<String>,
}

/// Truthiness of an env flag: unset/empty is `None`, falsy spellings are
/// `Some(false)`, anything else is `Some(true)`.
pub fn env_flag_value(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        _ => Some(true),
    }
}

fn push_flag() -> bool {
    env_flag_value("PI_AUTO_QA_PUSH").unwrap_or(false)
}

/// Pure precedence rule, kept separate so it is testable without touching the
/// process environment.
///
/// Explicit `dev.autoqa` (env `PI_AUTO_QA` or any settings layer) wins; the
/// default is on unless the user previously denied consent, and that denial veto
/// only applies to the default.
pub fn auto_qa_enabled_pure(settings: Option<&AutoQaSettings>, env_value: Option<bool>) -> bool {
    let Some(settings) = settings else {
        return env_value.unwrap_or(false);
    };
    if let Some(env) = env_value {
        return env;
    }
    if let Some(explicit) = settings.autoqa {
        return explicit;
    }
    settings.autoqa_consent != Consent::Denied
}

/// Whether Auto-QA is active for this session.
pub fn is_auto_qa_enabled(settings: Option<&AutoQaSettings>) -> bool {
    auto_qa_enabled_pure(settings, env_flag_value("PI_AUTO_QA"))
}

/// Async resolver for the user's "share grievances?" consent.
///
/// Return values:
///   - `Some(true)`  — user agreed; record + ship for this run and persist.
///   - `Some(false)` — user declined; suppress for this run and persist.
///   - `None`        — user dismissed the dialog without picking an option. The
///                     decision is not cached, so the next invocation re-prompts.
pub type ConsentFuture = Pin<Box<dyn Future<Output = Option<bool>> + Send>>;
/// Handler wired by an interactive host (Yes/No popup).
pub type ConsentHandler = Arc<dyn Fn() -> ConsentFuture + Send + Sync>;
/// Persistence hook: the tool writes the decision, the host stores it.
pub type ConsentSink = Arc<dyn Fn(Consent) + Send + Sync>;

#[derive(Default)]
struct ConsentState {
    handler: Option<ConsentHandler>,
    sink: Option<ConsentSink>,
    /// Authoritative decision from the host's disk-backed settings, so a
    /// subagent's stale snapshot cannot re-prompt.
    persisted: Consent,
}

fn consent_state() -> &'static Mutex<ConsentState> {
    static STATE: OnceLock<Mutex<ConsentState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ConsentState::default()))
}

type SharedConsent = Shared<BoxFuture<'static, bool>>;

fn consent_in_flight() -> &'static Mutex<Option<SharedConsent>> {
    static IN_FLIGHT: OnceLock<Mutex<Option<SharedConsent>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(None))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Register the consent handler and the persistence hook. Passing `None` for
/// the handler clears it (e.g. on interactive-mode teardown). Re-registration is
/// authoritative.
pub fn set_auto_qa_consent_handler(handler: Option<ConsentHandler>, sink: Option<ConsentSink>) {
    let mut state = lock(consent_state());
    state.handler = handler;
    state.sink = sink;
}

/// Record the decision the host loaded from disk, so subagent reports see it
/// without prompting.
pub fn set_persisted_consent(consent: Consent) {
    lock(consent_state()).persisted = consent;
}

/// Consent, handler and flush state are process-global, so every test that
/// touches them (here or in another module) has to run alone. Without this lock
/// a concurrent test's `set_persisted_consent(Granted)` lands between another
/// test's reset and its assertion.
#[cfg(test)]
static AUTO_QA_STATE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn auto_qa_state_guard() -> std::sync::MutexGuard<'static, ()> {
    lock(&AUTO_QA_STATE_LOCK)
}

/// Test-only: clear consent cache, handler and in-flight state.
pub fn reset_auto_qa_consent_for_tests() {
    *lock(consent_state()) = ConsentState::default();
    *lock(consent_in_flight()) = None;
}

fn persist_consent(granted: bool) {
    let value = if granted {
        Consent::Granted
    } else {
        Consent::Denied
    };
    let sink = {
        let mut state = lock(consent_state());
        state.persisted = value;
        state.sink.clone()
    };
    if let Some(sink) = sink {
        sink(value);
    }
}

/// Resolve the user's consent for Auto-QA grievances.
///
/// Read live on every call so a `/settings` or config edit applies to the next
/// report. Priority: persisted host setting, the caller's `Settings`, the
/// registered UI handler (single-flight), then default `false`.
pub async fn resolve_auto_qa_consent(settings: Option<&AutoQaSettings>) -> bool {
    let persisted = lock(consent_state()).persisted;
    if let Some(value) = persisted.as_bool() {
        return value;
    }
    if let Some(value) = settings.and_then(|s| s.autoqa_consent.as_bool()) {
        return value;
    }
    let handler = lock(consent_state()).handler.clone();
    let Some(handler) = handler else {
        return false;
    };

    // Single-flight: while the dialog is open every concurrent report awaits the
    // same future instead of stacking duplicate popups.
    let shared = {
        let mut slot = lock(consent_in_flight());
        match slot.as_ref() {
            Some(existing) => existing.clone(),
            None => {
                let future: SharedConsent = async move {
                    match handler().await {
                        None => false,
                        Some(granted) => {
                            persist_consent(granted);
                            granted
                        }
                    }
                }
                .boxed()
                .shared();
                *slot = Some(future.clone());
                future
            }
        }
    };
    let decision = shared.await;
    *lock(consent_in_flight()) = None;
    decision
}

// ───────────────────────────────────────────────────────────────────────────
// SQLite store
// ───────────────────────────────────────────────────────────────────────────

/// `~/.omp/autoqa.db` (XDG: `$XDG_DATA_HOME/omp/autoqa.db`), overridable with
/// `OMP_AUTOQA_DB`.
pub fn auto_qa_db_path() -> Option<PathBuf> {
    if let Some(explicit) = env_string("OMP_AUTOQA_DB") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = env_string("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("omp").join("autoqa.db"));
    }
    env_string("HOME")
        .or_else(|| env_string("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".omp").join("autoqa.db"))
}

fn ensure_column(conn: &Connection, column: &str, ddl: &str) -> rusqlite::Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM pragma_table_info('grievances') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        conn.execute_batch(ddl)?;
    }
    Ok(())
}

/// Open the auto-QA database at `path`, creating the schema lazily.
pub fn open_auto_qa_db_at(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    // Install the busy handler BEFORE any lock-taking statement.
    conn.busy_timeout(Duration::from_millis(5_000))?;
    // `pushed` is tri-state: 0 = queued, 1 = accepted by the collector,
    // -1 = permanently refused (`push_error` holds the collector's reason).
    // Refused rows are parked so one bad row can't block the queue forever.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS grievances (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            model TEXT NOT NULL,
            version TEXT NOT NULL,
            tool TEXT NOT NULL,
            report TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            pushed INTEGER NOT NULL DEFAULT 0,
            push_error TEXT,
            source TEXT
        );",
    )?;
    // Legacy DBs (May 2026) predate these columns. ALTER TABLE only accepts
    // constant defaults, so add `created_at` empty and backfill.
    ensure_column(
        &conn,
        "created_at",
        "ALTER TABLE grievances ADD COLUMN created_at TEXT NOT NULL DEFAULT '';",
    )?;
    conn.execute(
        "UPDATE grievances SET created_at = CURRENT_TIMESTAMP WHERE created_at = ''",
        [],
    )?;
    ensure_column(
        &conn,
        "push_error",
        "ALTER TABLE grievances ADD COLUMN push_error TEXT;",
    )?;
    ensure_column(&conn, "source", "ALTER TABLE grievances ADD COLUMN source TEXT;")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS grievances_pushed_created_at_idx
         ON grievances (pushed, created_at, id);",
    )?;
    Ok(conn)
}

/// Open (or reuse) the auto-QA database at the default path. Returns `None`
/// when the path cannot be resolved or opened.
pub fn open_auto_qa_db() -> Option<Arc<Mutex<Connection>>> {
    static CACHED: OnceLock<Option<Arc<Mutex<Connection>>>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let path = auto_qa_db_path()?;
            match open_auto_qa_db_at(&path) {
                Ok(conn) => Some(Arc::new(Mutex::new(conn))),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to open auto-QA database");
                    None
                }
            }
        })
        .clone()
}

fn db_guard(db: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    lock(db)
}

/// Insert one grievance row. Returns the new row id.
pub fn record_grievance(
    db: &Mutex<Connection>,
    model: &str,
    version: &str,
    tool: &str,
    report: &str,
    source: &str,
) -> rusqlite::Result<i64> {
    let conn = db_guard(db);
    conn.execute(
        "INSERT INTO grievances (model, version, tool, report, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)",
        params![model, version, tool, report, source],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Number of rows still waiting to be shipped.
pub fn pending_count(db: &Mutex<Connection>) -> rusqlite::Result<i64> {
    let conn = db_guard(db);
    conn.query_row("SELECT COUNT(*) FROM grievances WHERE pushed = 0", [], |row| {
        row.get(0)
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Backend push
// ───────────────────────────────────────────────────────────────────────────

/// Outcome of one flush.
#[derive(Debug, Clone, Default)]
pub struct FlushResult {
    /// Rows the collector accepted this flush.
    pub pushed: usize,
    pub ok: bool,
    pub skipped: bool,
    /// Rows the collector permanently refused, parked as `pushed = -1`.
    pub rejected: usize,
    /// Last collector error (`HTTP <status>: <body>`).
    pub error: Option<String>,
}

/// Optional per-flush controls. `bypass_consent` is reserved for explicit
/// user-driven pushes (`omt grievances push`); the device's auto-flush path
/// never sets it.
#[derive(Clone, Default)]
pub struct FlushOptions {
    pub bypass_consent: bool,
    /// Fires once with the snapshot count of unpushed rows.
    pub on_start: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    /// Fires after every shipped batch with the running pushed count.
    pub on_progress: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl std::fmt::Debug for FlushOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlushOptions")
            .field("bypass_consent", &self.bypass_consent)
            .field("on_start", &self.on_start.is_some())
            .field("on_progress", &self.on_progress.is_some())
            .finish()
    }
}

#[derive(Debug, Clone)]
struct PushConfig {
    endpoint: String,
    token: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct PushInputs<'a> {
    enabled: bool,
    consented: bool,
    push_flag: bool,
    bypass_consent: bool,
    endpoint_env: Option<&'a str>,
    endpoint_settings: Option<&'a str>,
    token_env: Option<&'a str>,
    token_settings: Option<&'a str>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Consent IS the push opt-in for the auto-flush path; `bypass_consent` covers
/// explicit user-driven pushes, and `PI_AUTO_QA_PUSH` stays as a headless
/// override.
fn resolve_push_config_pure(inputs: PushInputs<'_>) -> Option<PushConfig> {
    if !inputs.enabled {
        return None;
    }
    if !inputs.bypass_consent && !inputs.consented && !inputs.push_flag {
        return None;
    }
    let endpoint = non_empty(inputs.endpoint_env)
        .or_else(|| non_empty(inputs.endpoint_settings))
        .unwrap_or(DEFAULT_PUSH_ENDPOINT)
        .trim()
        .to_string();
    if endpoint.is_empty() {
        return None;
    }
    let token = non_empty(inputs.token_env)
        .or_else(|| non_empty(inputs.token_settings))
        .map(|t| t.to_string());
    Some(PushConfig { endpoint, token })
}

fn push_config(settings: Option<&AutoQaSettings>, bypass_consent: bool) -> Option<PushConfig> {
    let persisted = lock(consent_state()).persisted;
    let consented = settings
        .map(|s| s.autoqa_consent == Consent::Granted)
        .unwrap_or(false)
        || persisted == Consent::Granted;
    let endpoint_env = env_string("PI_AUTO_QA_PUSH_URL");
    let token_env = env_string("PI_AUTO_QA_PUSH_TOKEN");
    resolve_push_config_pure(PushInputs {
        enabled: is_auto_qa_enabled(settings),
        consented,
        push_flag: push_flag(),
        bypass_consent,
        endpoint_env: endpoint_env.as_deref(),
        endpoint_settings: settings.and_then(|s| s.push_endpoint.as_deref()),
        token_env: token_env.as_deref(),
        token_settings: settings.and_then(|s| s.push_token.as_deref()),
    })
}

#[derive(Debug, Clone)]
struct GrievanceRow {
    id: i64,
    model: String,
    version: String,
    tool: String,
    report: String,
    /// Tool provenance; `None` for rows recorded before it existed.
    source: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static LAST_FAILURE_MS: AtomicU64 = AtomicU64::new(0);

type SharedFlush = Shared<BoxFuture<'static, FlushResult>>;

fn flush_in_flight() -> &'static Mutex<Option<SharedFlush>> {
    static IN_FLIGHT: OnceLock<Mutex<Option<SharedFlush>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(None))
}

/// Test-only: clear single-flight + cooldown state.
pub fn reset_auto_qa_flush_state_for_tests() {
    *lock(flush_in_flight()) = None;
    LAST_FAILURE_MS.store(0, Ordering::Relaxed);
}

fn clamp_error_text(mut detail: String) -> String {
    if detail.chars().count() > MAX_PUSH_ERROR_CHARS {
        detail = detail.chars().take(MAX_PUSH_ERROR_CHARS).collect::<String>() + "…";
    }
    detail
}

fn describe_error_response(status: u16, detail: String) -> String {
    let detail = clamp_error_text(detail.trim().to_string());
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

enum BatchOutcome {
    /// Collector accepted every entry.
    Sent,
    /// Collector refused the payload; retrying the same bytes is pointless.
    Rejected(String),
    /// Transient failure (network, 5xx, rate limit, auth) — try again later.
    Retry(String),
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FLUSH_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

fn install_id() -> String {
    static INSTALL_ID: OnceLock<String> = OnceLock::new();
    INSTALL_ID
        .get_or_init(|| {
            if let Some(home) = env_string("HOME").or_else(|| env_string("USERPROFILE")) {
                let path = PathBuf::from(home).join(".omp").join("install-id");
                if let Ok(existing) = std::fs::read_to_string(&path) {
                    let trimmed = existing.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
                let generated = format!("{:032x}", rand::random::<u128>());
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, &generated);
                return generated;
            }
            format!("{:032x}", rand::random::<u128>())
        })
        .clone()
}

fn entry_json(row: &GrievanceRow) -> serde_json::Value {
    // Normalized on the way out so rows recorded before record-time clamping and
    // redaction existed neither poison every batch nor ship the home directory.
    let normalized = normalize_grievance(&row.tool, &row.report);
    let mut entry = serde_json::json!({
        "id": row.id,
        "model": row.model,
        "version": row.version,
        "tool": normalized.tool,
        "report": normalized.report,
    });
    if let (Some(source), Some(map)) = (row.source.as_ref(), entry.as_object_mut()) {
        map.insert("source".to_string(), serde_json::Value::String(source.clone()));
    }
    entry
}

async fn post_batch(config: &PushConfig, batch: &[GrievanceRow]) -> BatchOutcome {
    let body = serde_json::json!({
        "agent": { "name": "omp", "version": env!("CARGO_PKG_VERSION") },
        "installId": install_id(),
        // Coarse host fingerprint for triage — no machine name.
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "entries": batch.iter().map(entry_json).collect::<Vec<_>>(),
    });
    let mut request = http_client()
        .post(&config.endpoint)
        .header("content-type", "application/json")
        .json(&body);
    if let Some(token) = config.token.as_deref() {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return BatchOutcome::Retry(error.to_string()),
    };
    let status = response.status();
    if status.is_success() {
        return BatchOutcome::Sent;
    }
    let detail = response.text().await.unwrap_or_default();
    let error = describe_error_response(status.as_u16(), detail);
    // 400/413/422 mean the payload itself is unacceptable — retrying the same
    // bytes can only fail again. 5xx, 408, 429 and auth failures (401/403) say
    // nothing about the rows, so they stay retryable.
    match status.as_u16() {
        400 | 413 | 422 => BatchOutcome::Rejected(error),
        _ => BatchOutcome::Retry(error),
    }
}

fn mark_pushed(db: &Mutex<Connection>, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let placeholders = std::iter::repeat("?")
        .take(ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("UPDATE grievances SET pushed = 1 WHERE id IN ({placeholders})");
    let conn = db_guard(db);
    let _ = conn.execute(&sql, rusqlite::params_from_iter(ids.iter()));
}

fn park_rejected(db: &Mutex<Connection>, id: i64, error: &str) {
    let conn = db_guard(db);
    let _ = conn.execute(
        "UPDATE grievances SET pushed = -1, push_error = ?1 WHERE id = ?2",
        params![error, id],
    );
}

async fn perform_flush(
    db: Arc<Mutex<Connection>>,
    config: PushConfig,
    options: FlushOptions,
) -> FlushResult {
    let mut total_pushed = 0usize;
    let mut total_rejected = 0usize;
    let mut last_error: Option<String> = None;

    // Planning snapshot — mid-flight inserts are not folded in.
    if let Some(on_start) = options.on_start.as_deref() {
        let total = pending_count(&db).unwrap_or(0);
        on_start(total.max(0) as usize);
    }

    let mut ok = true;
    loop {
        let rows: Vec<GrievanceRow> = {
            let conn = db_guard(&db);
            let mut stmt = match conn.prepare(
                "SELECT id, model, version, tool, report, source FROM grievances
                 WHERE pushed = 0 ORDER BY id ASC LIMIT ?1",
            ) {
                Ok(stmt) => stmt,
                Err(error) => {
                    last_error = Some(error.to_string());
                    ok = false;
                    break;
                }
            };
            let mapped = stmt.query_map(params![FLUSH_BATCH_SIZE as i64], |row| {
                Ok(GrievanceRow {
                    id: row.get(0)?,
                    model: row.get(1)?,
                    version: row.get(2)?,
                    tool: row.get(3)?,
                    report: row.get(4)?,
                    source: row.get(5)?,
                })
            });
            match mapped {
                Ok(iter) => iter.filter_map(Result::ok).collect(),
                Err(error) => {
                    last_error = Some(error.to_string());
                    ok = false;
                    break;
                }
            }
        };
        if rows.is_empty() {
            break;
        }

        // Ship the batch, bisecting on payload rejections until the offending
        // rows are isolated and parked. A transient failure stops the flush and
        // leaves everything queued for the next attempt.
        let mut stack: Vec<Vec<GrievanceRow>> = vec![rows];
        let mut transient: Option<String> = None;
        while let Some(batch) = stack.pop() {
            match post_batch(&config, &batch).await {
                BatchOutcome::Sent => {
                    let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
                    mark_pushed(&db, &ids);
                    total_pushed += batch.len();
                    if let Some(on_progress) = options.on_progress.as_deref() {
                        on_progress(total_pushed);
                    }
                }
                BatchOutcome::Retry(error) => {
                    last_error = Some(error.clone());
                    transient = Some(error);
                    break;
                }
                BatchOutcome::Rejected(error) => {
                    last_error = Some(error.clone());
                    if batch.len() == 1 {
                        let row = &batch[0];
                        park_rejected(&db, row.id, &error);
                        total_rejected += 1;
                        tracing::warn!(
                            endpoint = %config.endpoint,
                            id = row.id,
                            tool = %row.tool,
                            error = %error,
                            "autoqa grievance rejected"
                        );
                        continue;
                    }
                    let mid = batch.len() / 2;
                    let tail = batch[mid..].to_vec();
                    let head = batch[..mid].to_vec();
                    stack.push(tail);
                    stack.push(head);
                }
            }
        }
        if let Some(error) = transient {
            LAST_FAILURE_MS.store(now_ms(), Ordering::Relaxed);
            tracing::warn!(
                endpoint = %config.endpoint,
                error = %error,
                pushed_so_far = total_pushed,
                "autoqa push failed"
            );
            ok = false;
            break;
        }
    }

    FlushResult {
        pushed: total_pushed,
        ok,
        skipped: false,
        rejected: total_rejected,
        error: last_error,
    }
}

/// Flush queued grievances to the configured backend.
pub async fn flush_grievances(
    db: Arc<Mutex<Connection>>,
    settings: Option<&AutoQaSettings>,
    options: FlushOptions,
) -> FlushResult {
    let config = match push_config(settings, options.bypass_consent) {
        Some(config) => config,
        None => {
            return FlushResult {
                skipped: true,
                ..Default::default()
            }
        }
    };

    let bypass = options.bypass_consent;
    if !bypass {
        let existing = lock(flush_in_flight()).clone();
        if let Some(existing) = existing {
            return existing.await;
        }
        let last = LAST_FAILURE_MS.load(Ordering::Relaxed);
        if last > 0 && now_ms().saturating_sub(last) < FAILURE_COOLDOWN_MS {
            return FlushResult {
                skipped: true,
                ..Default::default()
            };
        }
    }

    let future: SharedFlush = async move {
        match tokio::spawn(perform_flush(db, config, options)).await {
            Ok(result) => result,
            Err(error) => {
                LAST_FAILURE_MS.store(now_ms(), Ordering::Relaxed);
                FlushResult {
                    error: Some(error.to_string()),
                    ..Default::default()
                }
            }
        }
    }
    .boxed()
    .shared();

    if !bypass {
        *lock(flush_in_flight()) = Some(future.clone());
    }
    let result = future.await;
    if !bypass {
        *lock(flush_in_flight()) = None;
    }
    result
}

// ───────────────────────────────────────────────────────────────────────────
// Session-facing facade
// ───────────────────────────────────────────────────────────────────────────

/// Result handed back to the `write` tool for `xd://report_issue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// Model-visible confirmation text.
    pub message: String,
    /// The trimmed report body, as dispatched to the device.
    pub report: String,
}

impl DispatchOutcome {
    pub fn new(report: &str) -> Self {
        Self {
            message: "Noted, thanks!".to_string(),
            report: report.trim().to_string(),
        }
    }
}

/// Auto-QA pipeline for one session: settings snapshot, active model, and the
/// lazily opened grievance database.
pub struct AutoQa {
    settings: AutoQaSettings,
    model: String,
    db: OnceLock<Option<Arc<Mutex<Connection>>>>,
}

impl AutoQa {
    pub fn new(settings: AutoQaSettings, model: impl Into<String>) -> Self {
        Self {
            settings,
            model: model.into(),
            db: OnceLock::new(),
        }
    }

    pub fn with_database(
        settings: AutoQaSettings,
        model: impl Into<String>,
        db: Arc<Mutex<Connection>>,
    ) -> Self {
        let this = Self::new(settings, model);
        let _ = this.db.set(Some(db));
        this
    }

    pub fn settings(&self) -> &AutoQaSettings {
        &self.settings
    }

    fn database(&self) -> Option<Arc<Mutex<Connection>>> {
        self.db.get_or_init(open_auto_qa_db).clone()
    }

    /// Queue a grievance for recording: consent → insert → flush. Never panics
    /// and never propagates storage errors.
    pub async fn record(&self, catalog: &ToolCatalog, tool: &str, report: &str) {
        let canonical = tool.strip_prefix("proxy_").unwrap_or(tool);
        let source = resolve_tool_source(catalog, canonical);
        if !push_flag() && !resolve_auto_qa_consent(Some(&self.settings)).await {
            return;
        }
        let Some(db) = self.database() else {
            return;
        };
        if let Err(error) = record_grievance(
            &db,
            &self.model,
            env!("CARGO_PKG_VERSION"),
            canonical,
            report,
            &source,
        ) {
            tracing::debug!(%error, "autoqa insert failed");
            return;
        }
        let result = flush_grievances(db, Some(&self.settings), FlushOptions::default()).await;
        if !result.ok && !result.skipped {
            tracing::debug!(error = ?result.error, "autoqa flush failed");
        }
    }

    /// Execute `write xd://report_issue`. `text` must be either
    /// `<tool>: <concise description>` on one line, or the tool name on the
    /// first line with the report body below.
    pub async fn dispatch(
        &self,
        catalog: &ToolCatalog,
        text: &str,
    ) -> Result<DispatchOutcome, ReportIssueError> {
        if is_auto_qa_enabled(Some(&self.settings)) {
            let grievance = parse_report_issue_body(text)?;
            self.record(catalog, &grievance.tool, &grievance.report).await;
        }
        Ok(DispatchOutcome::new(text))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn grievance(tool: &str, report: &str) -> Grievance {
        Grievance {
            tool: tool.to_string(),
            report: report.to_string(),
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    fn consent_future(value: Option<bool>) -> ConsentFuture {
        Box::pin(async move { value })
    }


    // ── parsing ────────────────────────────────────────────────────────────

    #[test]
    fn parses_colon_body() {
        let parsed = parse_report_issue_body("read: truncated a long file").unwrap();
        assert_eq!(parsed, grievance("read", "truncated a long file"));
    }

    #[test]
    fn parses_two_line_body() {
        let parsed = parse_report_issue_body("bash\ncwd was lost after cd\nsecond line").unwrap();
        assert_eq!(parsed, grievance("bash", "cwd was lost after cd\nsecond line"));
    }

    #[test]
    fn two_line_wins_over_colon() {
        let parsed = parse_report_issue_body("grep: noisy\nbody here").unwrap();
        assert_eq!(parsed, grievance("grep: noisy", "body here"));
    }

    #[test]
    fn parses_json_body() {
        let parsed =
            parse_report_issue_body(r#"{"tool":"edit","description":"patch did not apply"}"#).unwrap();
        assert_eq!(parsed, grievance("edit", "patch did not apply"));
    }

    #[test]
    fn parses_json_alternate_keys() {
        let parsed =
            parse_report_issue_body(r#"{"tool_name":"glob","message":"ignored .gitignore"}"#).unwrap();
        assert_eq!(parsed, grievance("glob", "ignored .gitignore"));
    }

    #[test]
    fn json_missing_fields_is_invalid() {
        assert_eq!(
            parse_report_issue_body(r#"{"tool":"read"}"#).unwrap_err(),
            ReportIssueError::InvalidFormat
        );
    }

    #[test]
    fn json_array_is_invalid() {
        assert_eq!(
            parse_report_issue_body(r#"["read"]"#).unwrap_err(),
            ReportIssueError::InvalidFormat
        );
    }

    #[test]
    fn broken_json_falls_back_to_text() {
        // Not valid JSON, so the line-oriented formats get a chance: the first
        // colon followed by whitespace splits the body.
        let parsed = parse_report_issue_body("{\"tool\": read: bad quote").unwrap();
        assert_eq!(parsed, grievance("{\"tool\"", "read: bad quote"));
    }

    #[test]
    fn colon_without_space_stays_in_tool_name() {
        // `xd://tui` must not split at the scheme colon.
        let parsed = parse_report_issue_body("xd://tui: blank screen").unwrap();
        assert_eq!(parsed, grievance("xd://tui", "blank screen"));
    }

    #[test]
    fn empty_body_is_empty_report() {
        assert_eq!(
            parse_report_issue_body("   \n\t ").unwrap_err(),
            ReportIssueError::EmptyReport
        );
    }

    #[test]
    fn unseparated_text_is_invalid() {
        assert_eq!(
            parse_report_issue_body("just some words").unwrap_err(),
            ReportIssueError::InvalidFormat
        );
    }

    #[test]
    fn error_display_includes_usage() {
        let message = ReportIssueError::InvalidFormat.to_string();
        assert!(message.starts_with("Invalid report format."));
        assert!(message.contains("xd://report_issue"));
    }

    // ── clamping ───────────────────────────────────────────────────────────

    #[test]
    fn long_tool_is_clamped_and_moved_into_report() {
        let tool = "x".repeat(MAX_TOOL_BYTES + 40);
        let normalized = normalize_grievance(&tool, "detail");
        assert!(normalized.tool.len() <= MAX_TOOL_BYTES);
        assert_eq!(normalized.tool, "x".repeat(MAX_TOOL_BYTES));
        assert!(normalized.report.starts_with(&tool));
        assert!(normalized.report.ends_with("\ndetail"));
    }

    #[test]
    fn clamp_respects_char_boundaries() {
        let tool = "é".repeat(MAX_TOOL_BYTES); // 2 bytes each
        let normalized = normalize_grievance(&tool, "detail");
        assert!(normalized.tool.len() <= MAX_TOOL_BYTES);
        assert!(normalized.tool.chars().all(|c| c == 'é'));
    }

    #[test]
    fn short_tool_is_untouched() {
        assert_eq!(normalize_grievance("read", "fine"), grievance("read", "fine"));
    }

    // ── redaction ──────────────────────────────────────────────────────────

    #[test]
    fn home_directory_becomes_tilde() {
        let out = redact_identity("/home/will/proj/src/main.rs", Some("/home/will"), Some("will"));
        assert_eq!(out, "~/proj/src/main.rs");
    }

    #[test]
    fn home_rewrite_is_case_insensitive() {
        let out = redact_identity(r"c:\Users\Will\proj", Some(r"C:\Users\Will"), None);
        assert_eq!(out, r"~\proj");
    }

    #[test]
    fn windows_home_form_with_forward_slashes() {
        let out = redact_identity("C:/Users/Will/proj", Some(r"C:\Users\Will"), None);
        assert_eq!(out, "~/proj");
    }

    #[test]
    fn username_in_windows_path_is_redacted() {
        let out = redact_identity(r"c:\Users\will\app", None, Some("will"));
        assert_eq!(out, r"c:\Users\<user>\app");
    }

    #[test]
    fn username_in_encoded_session_dir_is_redacted() {
        let out = redact_identity("-Users-will-proj-", None, Some("will"));
        assert_eq!(out, "-Users-<user>-proj-");
    }

    #[test]
    fn username_in_reverse_dns_label_is_redacted() {
        let out = redact_identity("com.will.app", None, Some("will"));
        assert_eq!(out, "com.<user>.app");
    }

    #[test]
    fn username_in_tilde_form_is_redacted() {
        let out = redact_identity("~will/notes", None, Some("will"));
        assert_eq!(out, "~<user>/notes");
    }

    #[test]
    fn username_before_at_sign_is_redacted() {
        let out = redact_identity("will@host", None, Some("will"));
        assert_eq!(out, "<user>@host");
    }

    #[test]
    fn bare_username_inside_words_is_left_alone() {
        let out = redact_identity("willing to node/bin test", None, Some("will"));
        assert_eq!(out, "willing to node/bin test");
    }

    #[test]
    fn root_username_only_rewrites_home() {
        let out = redact_identity("/root/x root@host", Some("/root"), Some("root"));
        assert_eq!(out, "~/x root@host");
    }

    #[test]
    fn single_char_home_is_ignored() {
        let out = redact_identity("/a/b", Some("/"), None);
        assert_eq!(out, "/a/b");
    }

    // ── provenance ─────────────────────────────────────────────────────────

    #[test]
    fn resolves_builtin() {
        let catalog = ToolCatalog::default();
        assert_eq!(resolve_tool_source(&catalog, "read"), "builtin");
        assert_eq!(resolve_tool_source(&catalog, "xd://grep"), "builtin");
        assert_eq!(resolve_tool_source(&catalog, "proxy_edit"), "builtin");
    }

    #[test]
    fn resolves_mcp_server() {
        let mut catalog = ToolCatalog::default();
        catalog
            .mcp_servers
            .insert("github_search".to_string(), "github".to_string());
        assert_eq!(resolve_tool_source(&catalog, "github_search"), "mcp:github");
        assert_eq!(resolve_tool_source(&catalog, "mcp__other_tool"), "mcp");
    }

    #[test]
    fn resolves_eval_namespace() {
        let catalog = ToolCatalog {
            eval_namespaces: vec!["computer".to_string()],
            ..Default::default()
        };
        assert_eq!(
            resolve_tool_source(&catalog, "computer.screenshot"),
            "eval:computer"
        );
    }

    #[test]
    fn resolves_extension_and_unknown() {
        let mut catalog = ToolCatalog::default();
        catalog.registered.insert("my_plugin".to_string());
        assert_eq!(resolve_tool_source(&catalog, "my_plugin"), "extension");
        assert_eq!(resolve_tool_source(&catalog, "mystery_tool"), "unknown");
    }

    // ── enablement + push config ───────────────────────────────────────────

    #[test]
    fn enabled_requires_env_without_settings() {
        assert!(!auto_qa_enabled_pure(None, None));
        assert!(auto_qa_enabled_pure(None, Some(true)));
        assert!(!auto_qa_enabled_pure(None, Some(false)));
    }

    #[test]
    fn explicit_env_wins_over_settings() {
        let settings = AutoQaSettings {
            autoqa: Some(false),
            ..Default::default()
        };
        assert!(auto_qa_enabled_pure(Some(&settings), Some(true)));
        assert!(!auto_qa_enabled_pure(Some(&settings), Some(false)));
    }

    #[test]
    fn denial_vetoes_default_but_not_explicit_enable() {
        let denied = AutoQaSettings {
            autoqa_consent: Consent::Denied,
            ..Default::default()
        };
        assert!(!auto_qa_enabled_pure(Some(&denied), None));

        let explicit = AutoQaSettings {
            autoqa: Some(true),
            autoqa_consent: Consent::Denied,
            ..Default::default()
        };
        assert!(auto_qa_enabled_pure(Some(&explicit), None));
    }

    fn push_inputs() -> PushInputs<'static> {
        PushInputs {
            enabled: true,
            consented: false,
            push_flag: false,
            bypass_consent: false,
            endpoint_env: None,
            endpoint_settings: None,
            token_env: None,
            token_settings: None,
        }
    }

    #[test]
    fn push_requires_consent_or_flag() {
        assert!(resolve_push_config_pure(push_inputs()).is_none());
        assert!(resolve_push_config_pure(PushInputs {
            consented: true,
            ..push_inputs()
        })
        .is_some());
        assert!(resolve_push_config_pure(PushInputs {
            push_flag: true,
            ..push_inputs()
        })
        .is_some());
        assert!(resolve_push_config_pure(PushInputs {
            bypass_consent: true,
            ..push_inputs()
        })
        .is_some());
    }

    #[test]
    fn push_disabled_when_autoqa_off() {
        let inputs = PushInputs {
            enabled: false,
            consented: true,
            ..push_inputs()
        };
        assert!(resolve_push_config_pure(inputs).is_none());
    }

    #[test]
    fn push_endpoint_and_token_precedence() {
        let config = resolve_push_config_pure(PushInputs {
            consented: true,
            endpoint_env: Some("  https://env.example  "),
            endpoint_settings: Some("https://settings.example"),
            token_env: Some("env-token"),
            token_settings: Some("settings-token"),
            ..push_inputs()
        })
        .unwrap();
        assert_eq!(config.endpoint, "https://env.example");
        assert_eq!(config.token.as_deref(), Some("env-token"));

        let config = resolve_push_config_pure(PushInputs {
            consented: true,
            endpoint_settings: Some("https://settings.example"),
            token_env: Some("   "),
            token_settings: Some("settings-token"),
            ..push_inputs()
        })
        .unwrap();
        assert_eq!(config.endpoint, "https://settings.example");
        assert_eq!(config.token.as_deref(), Some("settings-token"));
    }

    #[test]
    fn push_endpoint_falls_back_to_default() {
        let config = resolve_push_config_pure(PushInputs {
            consented: true,
            ..push_inputs()
        })
        .unwrap();
        assert_eq!(config.endpoint, DEFAULT_PUSH_ENDPOINT);
        assert!(config.token.is_none());
    }

    #[test]
    fn consent_parses_known_spellings() {
        assert_eq!(Consent::parse("granted"), Consent::Granted);
        assert_eq!(Consent::parse(" DENIED "), Consent::Denied);
        assert_eq!(Consent::parse("maybe"), Consent::Unset);
        assert_eq!(Consent::default(), Consent::Unset);
    }

    #[test]
    fn consent_without_handler_is_false() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        let settings = AutoQaSettings::default();
        assert!(!block_on(resolve_auto_qa_consent(Some(&settings))));
        reset_auto_qa_consent_for_tests();
    }

    #[test]
    fn persisted_consent_short_circuits_handler() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        set_persisted_consent(Consent::Granted);
        assert!(block_on(resolve_auto_qa_consent(None)));
        reset_auto_qa_consent_for_tests();
    }

    #[test]
    fn handler_grant_is_persisted_via_sink() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        let seen = Arc::new(Mutex::new(Vec::<Consent>::new()));
        let sink_seen = seen.clone();
        let handler: ConsentHandler = Arc::new(|| consent_future(Some(true)));
        let sink: ConsentSink = Arc::new(move |consent| {
            sink_seen.lock().unwrap().push(consent);
        });
        set_auto_qa_consent_handler(Some(handler), Some(sink));

        assert!(block_on(resolve_auto_qa_consent(None)));
        assert_eq!(*seen.lock().unwrap(), vec![Consent::Granted]);
        // The second call reads the persisted decision instead of prompting.
        assert!(block_on(resolve_auto_qa_consent(None)));
        assert_eq!(seen.lock().unwrap().len(), 1);
        reset_auto_qa_consent_for_tests();
    }

    #[test]
    fn handler_dismissal_does_not_persist() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let handler: ConsentHandler = Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            consent_future(None)
        });
        set_auto_qa_consent_handler(Some(handler), None);

        assert!(!block_on(resolve_auto_qa_consent(None)));
        assert_eq!(lock(consent_state()).persisted, Consent::Unset);
        assert!(!block_on(resolve_auto_qa_consent(None)));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        reset_auto_qa_consent_for_tests();
    }

    // ── device dispatch + store ────────────────────────────────────────────

    #[test]
    fn detects_report_issue_writes() {
        assert!(is_report_issue_tool_call("write", Some("xd://report_issue")));
        assert!(is_report_issue_tool_call("write", Some("xd://report_issue/")));
        assert!(!is_report_issue_tool_call("write", Some("xd://report_issue/x")));
        assert!(!is_report_issue_tool_call("edit", Some("xd://report_issue")));
        assert!(!is_report_issue_tool_call("write", None));
    }

    #[test]
    fn store_round_trip_and_pending_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autoqa.db");
        let conn = open_auto_qa_db_at(&path).unwrap();
        let db = Arc::new(Mutex::new(conn));

        assert_eq!(pending_count(&db).unwrap(), 0);
        let id = record_grievance(&db, "test-model", "9.9.9", "read", "body", "builtin").unwrap();
        assert!(id > 0);
        assert_eq!(pending_count(&db).unwrap(), 1);
        drop(db);

        // Re-opening keeps the row and does not duplicate the schema.
        let conn = open_auto_qa_db_at(&path).unwrap();
        let db = Arc::new(Mutex::new(conn));
        assert_eq!(pending_count(&db).unwrap(), 1);
        let row: (String, String, String) = {
            let conn = db_guard(&db);
            conn.query_row(
                "SELECT model, tool, source FROM grievances WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(
            row,
            (
                "test-model".to_string(),
                "read".to_string(),
                "builtin".to_string()
            )
        );
    }

    #[test]
    fn store_creates_nested_dir_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("autoqa.db");
        let conn = open_auto_qa_db_at(&path).unwrap();
        assert!(path.exists());
        let index: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index'
                 AND name = 'grievances_pushed_created_at_idx'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(index.as_deref(), Some("grievances_pushed_created_at_idx"));
        let created_at: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('grievances') WHERE name = 'created_at'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(created_at.as_deref(), Some("created_at"));
    }

    #[test]
    fn flush_skips_without_consent() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        reset_auto_qa_flush_state_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let conn = open_auto_qa_db_at(&dir.path().join("autoqa.db")).unwrap();
        let db = Arc::new(Mutex::new(conn));
        record_grievance(&db, "m", "1", "read", "body", "builtin").unwrap();

        let settings = AutoQaSettings::default();
        let result = block_on(flush_grievances(db.clone(), Some(&settings), FlushOptions::default()));
        assert!(result.skipped);
        assert_eq!(result.pushed, 0);
        assert_eq!(pending_count(&db).unwrap(), 1);
        reset_auto_qa_flush_state_for_tests();
    }

    #[test]
    fn dispatch_without_autoqa_does_not_store() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let conn = open_auto_qa_db_at(&dir.path().join("autoqa.db")).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let settings = AutoQaSettings {
            autoqa: Some(false),
            ..Default::default()
        };
        let auto = AutoQa::with_database(settings, "test-model", db.clone());

        let outcome = block_on(auto.dispatch(&ToolCatalog::default(), "read: broken")).unwrap();
        assert_eq!(outcome.message, "Noted, thanks!");
        assert_eq!(outcome.report, "read: broken");
        assert_eq!(pending_count(&db).unwrap(), 0);
    }

    #[test]
    fn record_stores_row_when_consent_granted() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        set_persisted_consent(Consent::Granted);
        let dir = tempfile::tempdir().unwrap();
        let conn = open_auto_qa_db_at(&dir.path().join("autoqa.db")).unwrap();
        let db = Arc::new(Mutex::new(conn));
        // `dev.autoqa: false` keeps push disabled, so this never hits the network.
        let settings = AutoQaSettings {
            autoqa: Some(false),
            ..Default::default()
        };
        let auto = AutoQa::with_database(settings, "test-model", db.clone());

        block_on(auto.record(&ToolCatalog::default(), "grep", "missed a match"));
        assert_eq!(pending_count(&db).unwrap(), 1);
        let (model, tool, source): (String, String, String) = {
            let conn = db_guard(&db);
            conn.query_row("SELECT model, tool, source FROM grievances", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
        };
        assert_eq!(model, "test-model");
        assert_eq!(tool, "grep");
        assert_eq!(source, "builtin");
        reset_auto_qa_consent_for_tests();
        reset_auto_qa_flush_state_for_tests();
    }

    #[test]
    fn record_drops_report_without_consent() {
        let _guard = auto_qa_state_guard();
        reset_auto_qa_consent_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let conn = open_auto_qa_db_at(&dir.path().join("autoqa.db")).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let auto = AutoQa::with_database(AutoQaSettings::default(), "test-model", db.clone());

        block_on(auto.record(&ToolCatalog::default(), "read", "silently dropped"));
        assert_eq!(pending_count(&db).unwrap(), 0);
        reset_auto_qa_consent_for_tests();
    }

    #[test]
    fn dispatch_propagates_parse_errors() {
        let settings = AutoQaSettings {
            autoqa: Some(true),
            ..Default::default()
        };
        let auto = AutoQa::new(settings, "test-model");
        let err = block_on(auto.dispatch(&ToolCatalog::default(), "   ")).unwrap_err();
        assert_eq!(err, ReportIssueError::EmptyReport);
    }

    #[test]
    fn error_text_is_clamped() {
        let long = "e".repeat(MAX_PUSH_ERROR_CHARS + 50);
        let described = describe_error_response(500, long);
        assert!(described.starts_with("HTTP 500: "));
        assert_eq!(
            described.chars().count(),
            "HTTP 500: ".chars().count() + MAX_PUSH_ERROR_CHARS + 1
        );
        assert_eq!(describe_error_response(503, "   ".to_string()), "HTTP 503");
    }

    #[test]
    fn entries_carry_normalized_fields_and_source() {
        let row = GrievanceRow {
            id: 7,
            model: "m".to_string(),
            version: "1".to_string(),
            tool: "x".repeat(MAX_TOOL_BYTES + 10),
            report: "detail".to_string(),
            source: Some("builtin".to_string()),
        };
        let entry = entry_json(&row);
        assert_eq!(entry["id"], 7);
        assert_eq!(entry["source"], "builtin");
        let tool = entry["tool"].as_str().unwrap();
        assert!(tool.len() <= MAX_TOOL_BYTES);
        assert!(entry["report"].as_str().unwrap().ends_with("\ndetail"));
    }

    #[test]
    fn entries_omit_absent_source() {
        let row = GrievanceRow {
            id: 1,
            model: "m".to_string(),
            version: "1".to_string(),
            tool: "read".to_string(),
            report: "detail".to_string(),
            source: None,
        };
        assert!(entry_json(&row).get("source").is_none());
    }
}
