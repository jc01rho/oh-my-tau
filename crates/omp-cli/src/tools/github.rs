//! `github`: omp's `gh` op wrapper — repos/files, pull requests, search,
//! checkout, push, and Actions watch.
//!
//! Port of `src/tools/gh.ts` and its siblings (`gh-common`, `gh-view`,
//! `gh-search`, `gh-pr-checkout`, `gh-run-watch`, `glab-mr`) folded into one
//! module. Every network call goes through the sanctioned `gh` / `glab` CLI
//! runner ([`run_forge_cli`]): non-interactive env, bounded 8 MiB capture per
//! stream, five-minute deadline, abort-aware.
//!
//! Read issue/PR bodies are served by [`get_or_fetch_issue`] /
//! [`get_or_fetch_pr`], which share the SQLite rows in [`super::github_cache`]
//! with the `issue://` / `pr://` / `mr://` protocol handlers.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use super::github_cache::{
    self as cache, AuthKey, CacheKind, CacheLookupOptions, CacheStatus, FreshResult,
};

// ─── Descriptions ──────────────────────────────────────────────────────────

/// omp `prompts/tools/github.md`, rendered into the tool description.
pub const DESCRIPTION: &str = r#"`gh` op wrapper: repos/files, PRs, search, checkout, push, Actions watch. Read issue/PR: `issue://<N>`/`pr://<N>`. PR diffs: `pr://<N>/diff` (files); `pr://<N>/diff/<i>` (file slice, 1-indexed); `pr://<N>/diff/all` (full). `?at=<generation>` pins that cached snapshot.

<instruction>
Select via `op`.
- `repo`: `[host/]owner/repo`; qualify the host for a repo outside the checkout's own GitHub instance.
- `repo_view`: omit `repo` → current checkout.
- `file_read`: read `path` from `repo`; omit `repo` → current checkout, `branch` → default branch.
- `pr_create`: `head` defaults current branch.
- `pr_checkout`: PR(s) → dedicated git worktrees, never working tree; array `pr` batches multiple in one call.
- `pr_push`: requires prior `op: pr_checkout`.
- `search_issues`/`search_prs`/`search_commits`/`search_repos`: `query` optional with `since`/`until`; omit for date-only filter. `search_code`: `query` required; rejects `since`/`until`.
- `search_*`: `repo` defaults current checkout's `owner/repo`; search elsewhere with `repo:`/`org:`/`user:` in `query`. `search_repos`: ignores `repo`; scope via `org:`/`language:` in `query`.
- Boolean `AND`/`OR`/`NOT` combine text terms, not qualifiers; never place them between qualifiers.
- `since`/`until`: relative `<n>` + `m`/`h`/`d`/`w`/`mo`/`y` (e.g. `3d`, `2w`), ISO date `YYYY-MM-DD`, or ISO datetime. `dateField: "updated"`: update time (issues/PRs), push time (repos), never creation.
- `run_watch`: omit `run` → every run for current HEAD; `branch` defaults current. Fast-fails first job failure.
</instruction>

<output>
Concise summary per op. `run_watch` failures save full logs to a session artifact.
</output>

<critical>
GitHub-hosted repository file: MUST use `file_read`; NEVER `curl`/`wget`.
</critical>"#;

/// omp `summary`.
pub const SUMMARY: &str = "Interact with GitHub repositories, files, pull requests, and Actions";

/// omp `GITHUB_READONLY_OPS`: everything else is an `exec`-tier call.
pub const GITHUB_READONLY_OPS: [&str; 8] = [
    "repo_view",
    "file_read",
    "search_issues",
    "search_prs",
    "search_code",
    "search_commits",
    "search_repos",
    "run_watch",
];

/// omp `GH_COMMAND_TIMEOUT_MS`.
pub const GH_COMMAND_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// omp `OUTPUT_LIMIT_BYTES`.
pub const OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// omp `SEARCH_LIMIT_DEFAULT`.
pub const SEARCH_LIMIT_DEFAULT: u64 = 10;
/// omp `SEARCH_LIMIT_MAX`.
pub const SEARCH_LIMIT_MAX: u64 = 50;
/// omp `FILE_PREVIEW_LIMIT`.
pub const FILE_PREVIEW_LIMIT: usize = 50;
/// omp `REVIEW_COMMENTS_PAGE_SIZE`.
pub const REVIEW_COMMENTS_PAGE_SIZE: usize = 100;
/// omp `BINARY_SNIFF_BYTES`.
const BINARY_SNIFF_BYTES: usize = 8000;

const DEFAULT_BRANCH_LABEL: &str = "HEAD";

// ─── Wire schema ───────────────────────────────────────────────────────────

/// Parameter schema, byte-equal (as JSON) to omp's `githubSchema`.
///
/// `additionalProperties` is absent because omptype defaults `extras: "keep"`;
/// only `op` is required, and `pr` is the one union (`anyOf`).
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": [
                    "repo_view",
                    "file_read",
                    "pr_create",
                    "pr_checkout",
                    "pr_push",
                    "search_issues",
                    "search_prs",
                    "search_code",
                    "search_commits",
                    "search_repos",
                    "run_watch"
                ],
                "description": "github operation"
            },
            "repo": {"type": "string", "description": "owner/repo"},
            "branch": {"type": "string", "description": "branch"},
            "path": {"type": "string", "description": "repository-relative file path"},
            "pr": {
                "anyOf": [
                    {"type": "string"},
                    {"type": "array", "items": {"type": "string"}}
                ],
                "description": "pr number, url, or branch"
            },
            "force": {"type": "boolean", "description": "reset existing local branch"},
            "forceWithLease": {"type": "boolean", "description": "force-with-lease push"},
            "title": {"type": "string", "description": "pr title"},
            "body": {"type": "string", "description": "pr body markdown"},
            "base": {"type": "string", "description": "pr base branch"},
            "head": {"type": "string", "description": "pr head branch"},
            "draft": {"type": "boolean", "description": "open pr as draft"},
            "fill": {"type": "boolean", "description": "auto-fill pr title/body from commits"},
            "reviewer": {"type": "array", "items": {"type": "string"}, "description": "reviewers"},
            "assignee": {"type": "array", "items": {"type": "string"}, "description": "assignees"},
            "label": {"type": "array", "items": {"type": "string"}, "description": "labels"},
            "query": {"type": "string", "description": "search query"},
            "since": {"type": "string", "description": "lower-bound date filter"},
            "until": {"type": "string", "description": "upper-bound date filter"},
            "dateField": {"type": "string", "enum": ["created", "updated"], "description": "date field"},
            "limit": {"type": "number", "description": "max results"},
            "run": {"type": "string", "description": "actions run id or url"},
            "tail": {"type": "number", "description": "log lines per failed job"}
        },
        "required": ["op"]
    })
}

/// Approval tier for a call: the read-only ops are `read`, everything else `exec`.
#[must_use]
pub fn approval_for(args: &Value) -> &'static str {
    let op = args.get("op").and_then(Value::as_str).unwrap_or("");
    if GITHUB_READONLY_OPS.contains(&op) {
        "read"
    } else {
        "exec"
    }
}

// ─── Params ────────────────────────────────────────────────────────────────

/// `pr` is either one identifier or a batch of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PrArg {
    One(String),
    Many(Vec<String>),
}

/// Decoded `github` parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubParams {
    pub op: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub pr: Option<PrArg>,
    #[serde(default)]
    pub force: Option<bool>,
    #[serde(default, rename = "forceWithLease")]
    pub force_with_lease: Option<bool>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub fill: Option<bool>,
    #[serde(default)]
    pub reviewer: Option<Vec<String>>,
    #[serde(default)]
    pub assignee: Option<Vec<String>>,
    #[serde(default)]
    pub label: Option<Vec<String>>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default, rename = "dateField")]
    pub date_field: Option<String>,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub tail: Option<f64>,
}

impl GithubParams {
    /// Parse from the raw tool arguments.
    pub fn from_value(params: &Value) -> Result<Self> {
        serde_json::from_value(params.clone()).map_err(|e| anyhow!("invalid github parameters: {e}"))
    }
}

// ─── String helpers (omp `gh-common`) ──────────────────────────────────────

/// omp `normalizeOptionalString`: trim, and treat empty as absent.
#[must_use]
pub fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    let normalized = value?.trim();
    (!normalized.is_empty()).then(|| normalized.to_string())
}

/// omp `normalizeText`: CRLF/CR to LF, tabs to four spaces, trimmed.
#[must_use]
pub fn normalize_text(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .trim()
        .to_string()
}

/// omp `normalizeBlock`: like [`normalize_text`] but only trims the end.
#[must_use]
pub fn normalize_block(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .trim_end()
        .to_string()
}

/// omp `requireNonEmpty`.
pub fn require_non_empty(value: Option<&str>, label: &str) -> Result<String> {
    normalize_optional_string(value).ok_or_else(|| anyhow!("{label} must not be empty"))
}

/// omp `normalizePrIdentifierList`: trimmed, empties dropped, string or array.
#[must_use]
pub fn normalize_pr_identifier_list(value: Option<&PrArg>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let raw: Vec<&String> = match value {
        PrArg::One(single) => vec![single],
        PrArg::Many(many) => many.iter().collect(),
    };
    raw.into_iter()
        .filter_map(|entry| {
            let trimmed = entry.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
}

/// `[HOST/]OWNER/REPO`, split into its host (when named) and slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub host: Option<String>,
    pub slug: String,
}

/// omp `parseRepoRef`.
#[must_use]
pub fn parse_repo_ref(repo: &str) -> RepoRef {
    let (host, slug) = cache::parse_repo_ref(repo);
    RepoRef { host, slug }
}

/// omp `formatRepoRef`.
#[must_use]
pub fn format_repo_ref(host: Option<&str>, slug: &str) -> String {
    match host {
        Some(host) => format!("{host}/{slug}"),
        None => slug.to_string(),
    }
}

/// omp `ghApiHostArgs`: `gh api` endpoints carry no host, so name it with a flag.
#[must_use]
pub fn gh_api_host_args(reference: &RepoRef) -> Vec<String> {
    match &reference.host {
        Some(host) => vec!["--hostname".to_string(), host.clone()],
        None => Vec::new(),
    }
}

/// omp `appendRepoFlag`: a full-URL identifier already names host/repo/number,
/// and `gh` rejects a competing `--repo`.
pub fn append_repo_flag(args: &mut Vec<String>, repo: Option<&str>, identifier: Option<&str>) {
    if identifier.is_some_and(|id| id.starts_with("https://")) {
        return;
    }
    if let Some(repo) = normalize_optional_string(repo) {
        args.push("--repo".to_string());
        args.push(repo);
    }
}

/// The host `gh` assumes when a ref names none and `GH_HOST` is unset.
pub const GITHUB_HOST: &str = "github.com";

static REPO_URL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https?://([^/]+)/([^/]+)/([^/?#]+)").expect("repo url regex"));

/// omp `repoFromUrl`: `https://HOST/OWNER/REPO` → repository identity. The host
/// is dropped only when it is the one `gh` would have assumed anyway.
#[must_use]
pub fn repo_from_url(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    let captures = REPO_URL_PATTERN.captures(trimmed)?;
    let host = captures.get(1)?.as_str().to_lowercase();
    let slug = format!("{}/{}", captures.get(2)?.as_str(), captures.get(3)?.as_str());
    Some(if host == cache::default_gh_host() {
        slug
    } else {
        format!("{host}/{slug}")
    })
}

static PR_URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://([^/]+)/([^/]+/[^/]+)/pull/(\d+)(?:[/?#].*)?$").expect("pr url regex")
});
static ISSUE_URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://([^/]+)/([^/]+/[^/]+)/issues/(\d+)(?:[/?#].*)?$").expect("issue url regex")
});
static RUN_URL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/actions/runs/(\d+)").expect("run url regex"));

/// omp `parsePullRequestUrl`.
#[must_use]
pub fn parse_pull_request_url(value: Option<&str>) -> (Option<String>, Option<i64>) {
    let Some(normalized) = normalize_optional_string(value) else {
        return (None, None);
    };
    let Some(captures) = PR_URL_PATTERN.captures(&normalized) else {
        return (None, None);
    };
    (
        Some(format!(
            "{}/{}",
            captures.get(1).map_or("", |m| m.as_str()),
            captures.get(2).map_or("", |m| m.as_str())
        )),
        captures.get(3).and_then(|m| m.as_str().parse().ok()),
    )
}

/// omp `parseIssueUrl`.
#[must_use]
pub fn parse_issue_url(value: Option<&str>) -> (Option<String>, Option<i64>) {
    let Some(normalized) = normalize_optional_string(value) else {
        return (None, None);
    };
    let Some(captures) = ISSUE_URL_PATTERN.captures(&normalized) else {
        return (None, None);
    };
    (
        Some(format!(
            "{}/{}",
            captures.get(1).map_or("", |m| m.as_str()),
            captures.get(2).map_or("", |m| m.as_str())
        )),
        captures.get(3).and_then(|m| m.as_str().parse().ok()),
    )
}

/// omp `parsePositiveDecimalInt`: digit-only positive integers only. `1e2`,
/// `0x10`, `12.0`, and signed forms would otherwise key the cache against the
/// wrong row.
#[must_use]
pub fn parse_positive_decimal_int(value: Option<&str>) -> Option<i64> {
    let value = value?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<i64>().ok().filter(|number| *number > 0)
}

/// Parse a run id out of a bare id or an Actions run URL.
#[must_use]
pub fn parse_run_id(value: Option<&str>) -> Option<i64> {
    let value = normalize_optional_string(value)?;
    if let Some(captures) = RUN_URL_PATTERN.captures(&value) {
        return captures.get(1).and_then(|m| m.as_str().parse().ok());
    }
    parse_positive_decimal_int(Some(&value))
}

/// omp `formatAuthor`.
#[must_use]
pub fn format_author(author: &Value) -> Option<String> {
    let login = author.get("login").and_then(Value::as_str);
    if let Some(login) = login.filter(|login| !login.is_empty()) {
        return Some(format!("@{login}"));
    }
    author
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// omp `formatLabels`.
#[must_use]
pub fn format_labels(labels: Option<&Value>) -> Option<String> {
    let labels = labels?.as_array()?;
    let names: Vec<&str> = labels
        .iter()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// omp `formatShortSha`.
#[must_use]
pub fn format_short_sha(sha: Option<&str>) -> Option<String> {
    let sha = sha.filter(|sha| !sha.is_empty())?;
    Some(sha.chars().take(7).collect())
}

/// omp `formatBytes`.
#[must_use]
pub fn format_bytes(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn push_line<V: ToString>(lines: &mut Vec<String>, label: &str, value: Option<V>) {
    if let Some(value) = value {
        let rendered = value.to_string();
        if !rendered.is_empty() {
            lines.push(format!("{label}: {rendered}"));
        }
    }
}

// ─── Search semantics (omp `gh-search`) ────────────────────────────────────

static RELATIVE_DURATION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d+)\s*(m|h|d|w|mo|y)$").expect("relative duration regex"));
static ISO_DATE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("iso date regex"));

/// omp `resolveSearchLimit`: default 10, capped at 50, positive and finite.
pub fn resolve_search_limit(value: Option<f64>) -> Result<u64> {
    let Some(value) = value else {
        return Ok(SEARCH_LIMIT_DEFAULT);
    };
    if !value.is_finite() || value <= 0.0 {
        bail!("limit must be a positive number");
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((value.floor() as u64).min(SEARCH_LIMIT_MAX))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let adjusted_month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `YYYY-MM-DD` for an epoch-millisecond instant.
#[must_use]
pub fn format_iso_date(epoch_ms: i64) -> String {
    let (year, month, day) = civil_from_days(epoch_ms.div_euclid(86_400_000));
    format!("{year:04}-{month:02}-{day:02}")
}

/// omp `parseSearchDateBound`: relative `<n><unit>`, `YYYY-MM-DD`, or an ISO
/// datetime (milliseconds stripped — GitHub search takes seconds precision).
pub fn parse_search_date_bound(raw: &str, now_ms: i64) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("date bound must not be empty");
    }
    if let Some(captures) = RELATIVE_DURATION_PATTERN.captures(trimmed) {
        let count: i64 = captures
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let unit = captures.get(2).map_or("", |m| m.as_str()).to_lowercase();
        let fixed_ms = match unit.as_str() {
            "m" => Some(60_000_i64),
            "h" => Some(3_600_000),
            "d" => Some(86_400_000),
            "w" => Some(7 * 86_400_000),
            _ => None,
        };
        let bound_ms = if let Some(fixed_ms) = fixed_ms {
            now_ms.saturating_sub(count.saturating_mul(fixed_ms))
        } else {
            let (year, month, day) = civil_from_days(now_ms.div_euclid(86_400_000));
            let (year, month) = if unit == "mo" {
                let total = (year * 12 + month - 1).saturating_sub(count);
                (total.div_euclid(12), total.rem_euclid(12) + 1)
            } else {
                (year.saturating_sub(count), month)
            };
            days_from_civil(year, month, day).saturating_mul(86_400_000)
        };
        return Ok(format_iso_date(bound_ms));
    }
    if ISO_DATE_PATTERN.is_match(trimmed) {
        return Ok(trimmed.to_string());
    }
    // Any other parseable ISO datetime passes through with ms stripped.
    if is_iso_datetime(trimmed) {
        return Ok(strip_milliseconds(trimmed));
    }
    bail!(
        "invalid date bound: {raw}. Expected a relative duration like \"3d\", \"12h\", \"2w\", \
an ISO date \"YYYY-MM-DD\", or an ISO datetime."
    )
}

fn is_iso_datetime(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return false;
    }
    let all_digits = |range: std::ops::Range<usize>| {
        value
            .get(range)
            .is_some_and(|slice| slice.bytes().all(|byte| byte.is_ascii_digit()))
    };
    all_digits(0..4)
        && all_digits(5..7)
        && all_digits(8..10)
        && all_digits(11..13)
        && all_digits(14..16)
        && all_digits(17..19)
}

fn strip_milliseconds(value: &str) -> String {
    static FRACTION: LazyLock<Regex> = LazyLock::new(|| {
        // The `regex` crate has no look-ahead, so the zone suffix is captured and
        // put back instead of being asserted.
        Regex::new(r"\.\d+(Z|[+-]\d{2}:?\d{2}$)").expect("fraction regex")
    });
    FRACTION.replace(value, "${1}").into_owned()
}

/// omp `buildSearchDateQualifier`: `field:>=X`, `field:<=X`, or `field:X..Y`.
pub fn build_search_date_qualifier(
    field: &str,
    since: Option<&str>,
    until: Option<&str>,
    now_ms: i64,
) -> Result<Option<String>> {
    let since = match since.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => Some(parse_search_date_bound(value, now_ms)?),
        None => None,
    };
    let until = match until.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => Some(parse_search_date_bound(value, now_ms)?),
        None => None,
    };
    Ok(match (since, until) {
        (Some(since), Some(until)) => Some(format!("{field}:{since}..{until}")),
        (Some(since), None) => Some(format!("{field}:>={since}")),
        (None, Some(until)) => Some(format!("{field}:<={until}")),
        (None, None) => None,
    })
}

/// omp `resolveSearchDateField`: commits use `committer-date`; repos with
/// `updated` mean push time; everything else is `created`.
#[must_use]
pub fn resolve_search_date_field(command: &str, requested: Option<&str>) -> String {
    if command == "commits" {
        return "committer-date".to_string();
    }
    let date_field = requested.unwrap_or("created");
    if command == "repos" && date_field == "updated" {
        return "pushed".to_string();
    }
    date_field.to_string()
}

/// omp `composeSearchQuery`.
pub fn compose_search_query(parts: &[Option<&str>]) -> Result<String> {
    let cleaned: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.map(str::trim))
        .filter(|part| !part.is_empty())
        .collect();
    if cleaned.is_empty() {
        bail!("query is required (or pass since/until to filter by date)");
    }
    Ok(cleaned.join(" "))
}

/// omp `buildGhApiSearchArgs`.
#[must_use]
pub fn build_gh_api_search_args(
    endpoint: &str,
    query: &str,
    limit: u64,
    host: Option<&str>,
    extra_headers: &[&str],
) -> Vec<String> {
    let mut args = vec!["api".to_string()];
    if let Some(host) = host {
        args.push("--hostname".to_string());
        args.push(host.to_string());
    }
    args.extend([
        "-X".to_string(),
        "GET".to_string(),
        format!("/search/{endpoint}"),
        "-f".to_string(),
        format!("q={query}"),
        "-F".to_string(),
        format!("per_page={limit}"),
    ]);
    for header in extra_headers {
        args.push("-H".to_string());
        args.push((*header).to_string());
    }
    args
}

static REPO_SCOPE_QUALIFIER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|\s)-?(?:repo|org|user|owner):\S").expect("repo scope regex")
});

/// Whether the query already scopes itself, so no default `repo:` belongs on top.
#[must_use]
pub fn query_carries_repo_scope(query: Option<&str>) -> bool {
    query.is_some_and(|query| REPO_SCOPE_QUALIFIER_PATTERN.is_match(query))
}

/// Split a resolved scope into the `repo:` qualifier and the host to search.
/// GitHub's search qualifiers take a bare `owner/repo`, so an enterprise host
/// travels as `--hostname` instead of inside the query.
#[must_use]
pub fn search_scope(repo: Option<&str>) -> (Option<String>, Option<String>) {
    match repo {
        None => (None, None),
        Some(repo) => {
            let reference = parse_repo_ref(repo);
            (Some(format!("repo:{}", reference.slug)), reference.host)
        }
    }
}

// ─── CLI runner (omp `utils/github` + `utils/forge-cli`) ───────────────────

/// Captured result of a completed forge CLI invocation.
#[derive(Debug, Clone)]
pub struct ForgeCliResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
}

/// Which forge binary a call targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    Gh,
    Glab,
}

impl Forge {
    #[must_use]
    pub fn bin(self) -> &'static str {
        match self {
            Self::Gh => "gh",
            Self::Glab => "glab",
        }
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Gh => "GitHub CLI",
            Self::Glab => "GitLab CLI",
        }
    }

    fn install_message(self) -> &'static str {
        match self {
            Self::Gh => "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/.",
            Self::Glab => "GitLab CLI (glab) is not installed. Install it from https://gitlab.com/gitlab-org/cli.",
        }
    }
}

/// Whether `bin` resolves on `PATH` (omp `$which`).
#[must_use]
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(candidate).is_ok_and(|meta| {
                    meta.is_file() && meta.permissions().mode() & 0o111 != 0
                })
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
}

/// Whether the `gh` CLI is installed (omp `github.available()`).
#[must_use]
pub fn available() -> bool {
    which("gh").is_some()
}

async fn read_capped<R>(mut reader: R, bin: &'static str) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut captured: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut total = 0usize;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                total += read;
                if captured.len() < OUTPUT_LIMIT_BYTES {
                    let take = read.min(OUTPUT_LIMIT_BYTES - captured.len());
                    captured.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    let truncated = total > OUTPUT_LIMIT_BYTES;
    let mut text = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        text.push_str(&format!(
            "\n[{bin} subprocess output truncated after 8 MiB]\n"
        ));
    }
    (text, truncated)
}

fn forge_env(command: &mut tokio::process::Command, forge: Forge) {
    command
        .env("GIT_ASKPASS", "true")
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_MESSAGES", "C")
        .env_remove("LC_ALL")
        .env("SSH_ASKPASS", "echo non-interactive");
    match forge {
        Forge::Gh => {
            command.env("GH_PROMPT_DISABLED", "1");
        }
        Forge::Glab => {
            command
                .env("GLAB_NO_PROMPT", "1")
                .env("GLAB_CHECK_UPDATE", "false")
                .env("NO_COLOR", "1");
        }
    }
}

/// Run a forge CLI command with bounded capture and a deadline. Does not throw
/// on a non-zero exit — the caller decides what a failure means.
pub async fn run_forge_cli(
    forge: Forge,
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    trim_output: bool,
) -> Result<ForgeCliResult> {
    if signal.is_aborted() {
        bail!("aborted");
    }
    if which(forge.bin()).is_none() {
        bail!("{}", forge.install_message());
    }
    let mut command = tokio::process::Command::new(forge.bin());
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    forge_env(&mut command, forge);

    let mut child = command
        .spawn()
        .map_err(|e| anyhow!("failed to start {}: {e}", forge.display_name()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture {} output.", forge.display_name()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture {} output.", forge.display_name()))?;

    let deadline = Duration::from_millis(GH_COMMAND_TIMEOUT_MS);
    let bin = forge.bin();
    // One future owns the child, so the abort/timeout arms cannot race it for
    // the handle; dropping it on either path triggers `kill_on_drop`.
    let work = async move {
        let (stdout, stdout_truncated) = read_capped(stdout, bin).await;
        let (stderr, _) = read_capped(stderr, bin).await;
        let status = child.wait().await;
        (stdout, stdout_truncated, stderr, status)
    };
    tokio::pin!(work);
    let (stdout, stdout_truncated, stderr, status) = tokio::select! {
        () = signal.aborted() => bail!("aborted"),
        () = tokio::time::sleep(deadline) => bail!(
            "{} command timed out: {bin} {}",
            forge.display_name(),
            args.join(" ")
        ),
        collected = &mut work => collected,
    };
    if signal.is_aborted() {
        bail!("aborted");
    }
    let exit_code = status.map_or(0, |status| status.code().unwrap_or(-1));
    let (stdout, stderr) = if trim_output {
        (stdout.trim().to_string(), stderr.trim().to_string())
    } else {
        (stdout, stderr)
    };
    Ok(ForgeCliResult {
        exit_code,
        stdout,
        stderr,
        stdout_truncated,
    })
}

/// Options shaping `gh` failure messages and output handling.
#[derive(Debug, Clone, Copy, Default)]
pub struct GhCommandOptions {
    /// Caller passed an explicit repo; suppresses "run inside a checkout" hints.
    pub repo_provided: bool,
    /// Trim captured output (default true).
    pub trim_output: Option<bool>,
}

impl GhCommandOptions {
    fn trim(&self) -> bool {
        self.trim_output.unwrap_or(true)
    }
}

fn describe_github_api_error(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    let Some(object) = value.as_object() else {
        return None;
    };
    if let Some(message) = object.get("message").and_then(Value::as_str) {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let resource = object
        .get("resource")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let field = object.get("field").and_then(Value::as_str).unwrap_or("").trim();
    let target = [resource, field]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(".");
    let code = object.get("code").and_then(Value::as_str).unwrap_or("").trim();
    match (target.is_empty(), code.is_empty()) {
        (false, false) => Some(format!("{target}: {code}")),
        (false, true) => Some(target),
        (true, false) => Some(code.to_string()),
        (true, true) => None,
    }
}

fn parse_github_api_error_messages(stdout: &str) -> Vec<String> {
    let Ok(payload) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let Some(object) = payload.as_object() else {
        return Vec::new();
    };
    let mut messages: Vec<String> = Vec::new();
    let mut push = |message: Option<String>| {
        if let Some(message) = message {
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
    };
    push(object.get("message").and_then(describe_github_api_error));
    if let Some(errors) = object.get("errors").and_then(Value::as_array) {
        for error in errors {
            push(describe_github_api_error(error));
        }
    }
    messages
}

fn format_gh_failure(args: &[String], stdout: &str, stderr: &str, options: GhCommandOptions) -> String {
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if message.contains("gh auth login") || message.contains("not logged into any GitHub hosts") {
        return "GitHub CLI is not authenticated. Run `gh auth login`.".to_string();
    }
    if !options.repo_provided
        && (message.contains("not a git repository")
            || message.contains("no git remotes found")
            || message.contains("unable to determine current repository"))
    {
        return "GitHub repository context is unavailable. Pass `repo` explicitly or run the tool inside a GitHub checkout.".to_string();
    }
    if !message.is_empty() {
        return message.to_string();
    }
    format!("GitHub CLI command failed: gh {}", args.join(" "))
}

fn format_gh_json_failure(
    args: &[String],
    stdout: &str,
    stderr: &str,
    options: GhCommandOptions,
) -> String {
    let raw_message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let fallback = format_gh_failure(args, stdout, stderr, options);
    if fallback != raw_message {
        return fallback;
    }
    let details: Vec<String> = parse_github_api_error_messages(stdout)
        .into_iter()
        .filter(|message| !fallback.contains(message.as_str()))
        .collect();
    if details.is_empty() {
        return fallback;
    }
    let rendered: Vec<String> = details.iter().map(|message| format!("- {message}")).collect();
    format!("{fallback}\nGitHub details:\n{}", rendered.join("\n"))
}

/// The sanctioned `gh` runner: `run` never throws on a non-zero exit, `json`
/// and `text` do.
pub async fn gh_run(
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    options: GhCommandOptions,
) -> Result<ForgeCliResult> {
    run_forge_cli(Forge::Gh, cwd, args, signal, options.trim()).await
}

/// Run `gh` and parse stdout as JSON.
pub async fn gh_json<T: serde::de::DeserializeOwned>(
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    options: GhCommandOptions,
) -> Result<T> {
    let result = gh_run(cwd, args, signal, options).await?;
    if result.exit_code != 0 {
        bail!(
            "{}",
            format_gh_json_failure(args, &result.stdout, &result.stderr, options)
        );
    }
    if result.stdout.is_empty() {
        bail!("GitHub CLI returned empty output.");
    }
    serde_json::from_str(&result.stdout).map_err(|_| anyhow!("GitHub CLI returned invalid JSON output."))
}

/// Run `gh` and return stdout as text.
pub async fn gh_text(
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    options: GhCommandOptions,
) -> Result<String> {
    let result = gh_run(cwd, args, signal, options).await?;
    if result.exit_code != 0 {
        bail!(
            "{}",
            format_gh_failure(args, &result.stdout, &result.stderr, options)
        );
    }
    Ok(result.stdout)
}

/// The `glab` runner, mirroring [`gh_text`].
pub async fn glab_text(
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    trim_output: bool,
) -> Result<String> {
    let result = run_forge_cli(Forge::Glab, cwd, args, signal, trim_output).await?;
    if result.exit_code != 0 {
        let message = if result.stderr.trim().is_empty() {
            result.stdout.trim()
        } else {
            result.stderr.trim()
        };
        bail!(
            "{}",
            if message.is_empty() {
                format!("GitLab CLI command failed: glab {}", args.join(" "))
            } else {
                message.to_string()
            }
        );
    }
    Ok(result.stdout)
}

// ─── git helpers ───────────────────────────────────────────────────────────

async fn git(
    cwd: &Path,
    args: &[&str],
    signal: &AbortSignal,
) -> Result<std::process::Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_EDITOR", "true")
        .kill_on_drop(true);
    let child = command.spawn().map_err(|e| anyhow!("failed to start git: {e}"))?;
    tokio::select! {
        () = signal.aborted() => bail!("aborted"),
        output = child.wait_with_output() => output.map_err(|e| anyhow!("git failed: {e}")),
    }
}

async fn git_stdout(cwd: &Path, args: &[&str], signal: &AbortSignal) -> Result<String> {
    let output = git(cwd, args, signal).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "{}",
            if stderr.is_empty() {
                format!("git {} failed", args.join(" "))
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_config_get(cwd: &Path, key: &str, signal: &AbortSignal) -> Option<String> {
    let output = git(cwd, &["config", "--get", key], signal).await.ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

async fn git_config_set(cwd: &Path, key: &str, value: &str, signal: &AbortSignal) -> Result<()> {
    git_stdout(cwd, &["config", key, value], signal).await?;
    Ok(())
}

/// The repository root of `cwd`, or an error when it is not a checkout.
pub async fn require_git_repo_root(cwd: &Path, signal: &AbortSignal) -> Result<PathBuf> {
    let root = git_stdout(cwd, &["rev-parse", "--show-toplevel"], signal).await?;
    if root.is_empty() {
        bail!("Current git repository is unavailable.");
    }
    Ok(PathBuf::from(root))
}

/// The primary checkout's root (the common dir's parent) for a linked worktree.
pub async fn require_primary_git_repo_root(cwd: &Path, signal: &AbortSignal) -> Result<PathBuf> {
    let common = git_stdout(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"], signal)
        .await
        .unwrap_or_default();
    if let Some(root) = Path::new(&common).parent() {
        if !common.is_empty() {
            return Ok(root.to_path_buf());
        }
    }
    require_git_repo_root(cwd, signal).await
}

/// Current branch name, or an error naming what to pass instead.
pub async fn require_current_git_branch(cwd: &Path, signal: &AbortSignal) -> Result<String> {
    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], signal)
        .await
        .unwrap_or_default();
    if branch.is_empty() || branch == "HEAD" {
        bail!("Current git branch is unavailable. Pass `branch` or `run` explicitly.");
    }
    Ok(branch)
}

/// Current HEAD sha, or an error naming what to pass instead.
pub async fn require_current_git_head(cwd: &Path, signal: &AbortSignal) -> Result<String> {
    let head = git_stdout(cwd, &["rev-parse", "HEAD"], signal)
        .await
        .unwrap_or_default();
    if head.is_empty() {
        bail!("Current git HEAD is unavailable. Pass `run` explicitly.");
    }
    Ok(head)
}

async fn ref_exists(cwd: &Path, reference: &str, signal: &AbortSignal) -> bool {
    git(cwd, &["rev-parse", "--verify", "--quiet", reference], signal)
        .await
        .is_ok_and(|output| output.status.success())
}

fn short_hash(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().take(6).map(|byte| format!("{byte:02x}")).collect()
}

/// omp `getWorktreeDir`: the per-session worktree root for `name`.
#[must_use]
pub fn worktree_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("OMP_WORKTREE_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                std::env::var_os("HOME")
                    .map_or_else(|| PathBuf::from("."), PathBuf::from)
                    .join(".omp")
                    .join("worktrees")
            },
            PathBuf::from,
        );
    base.join(name)
}

// ─── Repo resolution ───────────────────────────────────────────────────────

/// Ask `gh` which repository the checkout points at, as `[HOST/]OWNER/REPO`.
///
/// `nameWithOwner` alone would drop the host, and `gh` resolves a host-less
/// `--repo` against `GH_HOST` — so an enterprise checkout would silently be
/// looked up on github.com. The repo URL carries the host `gh` itself resolved.
async fn resolve_repo_from_cwd(cwd: &Path, signal: &AbortSignal) -> Result<String> {
    let args = vec![
        "repo".to_string(),
        "view".to_string(),
        "--json".to_string(),
        "url".to_string(),
        "-q".to_string(),
        ".url".to_string(),
    ];
    let url = gh_text(cwd, &args, signal, GhCommandOptions::default()).await?;
    let repo = repo_from_url(Some(&url))
        .ok_or_else(|| anyhow!("GitHub CLI returned an unrecognized repository URL: {url}"))?;
    Ok(repo)
}

/// Resolve the repository for a call: an explicit `repo` wins, then a run URL's
/// repository, then the checkout's own.
pub async fn resolve_github_repo(
    cwd: &Path,
    repo: Option<&str>,
    run_repo: Option<&str>,
    signal: &AbortSignal,
) -> Result<String> {
    if let (Some(repo), Some(run_repo)) = (repo, run_repo) {
        if !repo_refs_equal(repo, run_repo) {
            bail!("run URL repository does not match the provided repo");
        }
    }
    if let Some(repo) = normalize_optional_string(repo) {
        return Ok(repo);
    }
    if let Some(run_repo) = normalize_optional_string(run_repo) {
        return Ok(run_repo);
    }
    resolve_repo_from_cwd(cwd, signal).await
}

fn effective_host(reference: &RepoRef) -> String {
    reference
        .host
        .as_ref()
        .map_or_else(cache::default_gh_host, |host| host.to_lowercase())
}

/// Case-insensitive repo comparison over the instance each ref resolves to. A
/// host-less ref is not a wildcard: `gh` sends it to `GH_HOST`.
#[must_use]
pub fn repo_refs_equal(left: &str, right: &str) -> bool {
    let left = parse_repo_ref(left);
    let right = parse_repo_ref(right);
    effective_host(&left) == effective_host(&right)
        && left.slug.eq_ignore_ascii_case(&right.slug)
}

/// Best-effort cached cwd → repo resolution that swallows any failure.
pub async fn try_resolve_current_repo(cwd: &Path, signal: &AbortSignal) -> Option<String> {
    resolve_github_repo(cwd, None, None, signal).await.ok()
}

// ─── Formatters (omp `gh-view` + `gh-search`) ──────────────────────────────

/// omp `GH_REPO_FIELDS`.
pub const GH_REPO_FIELDS: [&str; 14] = [
    "nameWithOwner",
    "description",
    "url",
    "defaultBranchRef",
    "homepageUrl",
    "forkCount",
    "isArchived",
    "isFork",
    "primaryLanguage",
    "repositoryTopics",
    "stargazerCount",
    "updatedAt",
    "viewerPermission",
    "visibility",
];

/// omp `GH_ISSUE_FIELDS`.
pub const GH_ISSUE_FIELDS: [&str; 11] = [
    "author",
    "body",
    "comments",
    "createdAt",
    "labels",
    "number",
    "state",
    "stateReason",
    "title",
    "updatedAt",
    "url",
];

/// omp `GH_ISSUE_FIELDS_NO_COMMENTS`.
pub const GH_ISSUE_FIELDS_NO_COMMENTS: [&str; 10] = [
    "author",
    "body",
    "createdAt",
    "labels",
    "number",
    "state",
    "stateReason",
    "title",
    "updatedAt",
    "url",
];

/// omp `GH_PR_FIELDS`.
pub const GH_PR_FIELDS: [&str; 17] = [
    "author",
    "baseRefName",
    "body",
    "comments",
    "createdAt",
    "files",
    "headRefName",
    "isDraft",
    "labels",
    "mergeStateStatus",
    "number",
    "reviews",
    "reviewDecision",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// omp `GH_PR_FIELDS_NO_COMMENTS`.
pub const GH_PR_FIELDS_NO_COMMENTS: [&str; 16] = [
    "author",
    "baseRefName",
    "body",
    "createdAt",
    "files",
    "headRefName",
    "isDraft",
    "labels",
    "mergeStateStatus",
    "number",
    "reviews",
    "reviewDecision",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// omp `GH_PR_CREATE_FIELDS`.
pub const GH_PR_CREATE_FIELDS: [&str; 10] = [
    "author",
    "baseRefName",
    "createdAt",
    "headRefName",
    "isDraft",
    "labels",
    "number",
    "state",
    "title",
    "url",
];

/// omp `GH_PR_CHECKOUT_FIELDS`.
pub const GH_PR_CHECKOUT_FIELDS: [&str; 10] = [
    "baseRefName",
    "headRefName",
    "headRefOid",
    "headRepository",
    "headRepositoryOwner",
    "isCrossRepository",
    "maintainerCanModify",
    "number",
    "title",
    "url",
];

/// omp `GH_REPO_CLONE_FIELDS`.
pub const GH_REPO_CLONE_FIELDS: [&str; 3] = ["nameWithOwner", "sshUrl", "url"];

fn json_string<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

fn json_i64(data: &Value, key: &str) -> Option<i64> {
    data.get(key).and_then(Value::as_i64)
}

fn json_bool(data: &Value, key: &str) -> Option<bool> {
    data.get(key).and_then(Value::as_bool)
}

/// omp `formatRepoView`.
#[must_use]
pub fn format_repo_view(data: &Value, repo: Option<&str>, branch: Option<&str>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let name = json_string(data, "nameWithOwner")
        .or(repo)
        .unwrap_or("GitHub Repository");
    lines.push(format!("# {name}"));
    lines.push(String::new());
    let description = normalize_text(json_string(data, "description"));
    lines.push(if description.is_empty() {
        "No description provided.".to_string()
    } else {
        description
    });
    lines.push(String::new());
    push_line(&mut lines, "URL", json_string(data, "url"));
    push_line(
        &mut lines,
        "Default branch",
        data.get("defaultBranchRef")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str),
    );
    push_line(&mut lines, "Branch", normalize_optional_string(branch));
    push_line(&mut lines, "Visibility", json_string(data, "visibility"));
    push_line(
        &mut lines,
        "Viewer permission",
        json_string(data, "viewerPermission"),
    );
    push_line(
        &mut lines,
        "Primary language",
        data.get("primaryLanguage")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str),
    );
    push_line(&mut lines, "Stars", json_i64(data, "stargazerCount"));
    push_line(&mut lines, "Forks", json_i64(data, "forkCount"));
    push_line(&mut lines, "Archived", json_bool(data, "isArchived"));
    push_line(&mut lines, "Fork", json_bool(data, "isFork"));
    push_line(&mut lines, "Updated", json_string(data, "updatedAt"));
    push_line(&mut lines, "Homepage", json_string(data, "homepageUrl"));
    let topics = data
        .get("repositoryTopics")
        .and_then(Value::as_array)
        .map(|topics| {
            topics
                .iter()
                .filter_map(|topic| {
                    topic
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            topic
                                .get("topic")
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                        })
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|topics| !topics.is_empty());
    push_line(&mut lines, "Topics", topics);
    lines.join("\n").trim().to_string()
}

fn format_comments_section(comments: Option<&Value>) -> Vec<String> {
    let Some(comments) = comments.and_then(Value::as_array).filter(|list| !list.is_empty()) else {
        return Vec::new();
    };
    let visible: Vec<&Value> = comments
        .iter()
        .filter(|comment| !json_bool(comment, "isMinimized").unwrap_or(false))
        .collect();
    let hidden_count = comments.len() - visible.len();
    let mut lines = vec!["## Comments".to_string(), String::new()];
    if visible.is_empty() {
        lines.push(format!(
            "No visible comments. Minimized comments omitted: {hidden_count}."
        ));
        return lines;
    }
    lines[0] = format!("## Comments ({})", visible.len());
    for comment in &visible {
        let author = format_author(comment.get("author").unwrap_or(&Value::Null))
            .unwrap_or_else(|| "unknown".to_string());
        let created_at = json_string(comment, "createdAt")
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        lines.push(format!("### {author}{created_at}"));
        lines.push(String::new());
        let body = normalize_text(json_string(comment, "body"));
        lines.push(if body.is_empty() {
            "No comment body.".to_string()
        } else {
            body
        });
        if let Some(url) = json_string(comment, "url") {
            lines.push(String::new());
            lines.push(format!("URL: {url}"));
        }
        lines.push(String::new());
    }
    if hidden_count > 0 {
        lines.push(format!("Minimized comments omitted: {hidden_count}."));
    }
    lines
}

fn format_reviews_section(reviews: Option<&Value>) -> Vec<String> {
    let Some(reviews) = reviews.and_then(Value::as_array).filter(|list| !list.is_empty()) else {
        return Vec::new();
    };
    let mut lines = vec![format!("## Reviews ({})", reviews.len()), String::new()];
    for review in reviews {
        let author = format_author(review.get("author").unwrap_or(&Value::Null))
            .unwrap_or_else(|| "unknown".to_string());
        let submitted_at = json_string(review, "submittedAt")
            .map(|value| format!(" - {value}"))
            .unwrap_or_default();
        let state = json_string(review, "state")
            .map(|value| format!(" [{value}]"))
            .unwrap_or_default();
        lines.push(format!("### {author}{submitted_at}{state}"));
        if let Some(oid) = review
            .get("commit")
            .and_then(|value| value.get("oid"))
            .and_then(Value::as_str)
        {
            if let Some(short) = format_short_sha(Some(oid)) {
                lines.push(String::new());
                lines.push(format!("Commit: {short}"));
            }
        }
        lines.push(String::new());
        let body = normalize_text(json_string(review, "body"));
        lines.push(if body.is_empty() {
            "No review body.".to_string()
        } else {
            body
        });
        lines.push(String::new());
    }
    lines
}

fn format_review_comments_section(comments: Option<&Value>) -> Vec<String> {
    let Some(comments) = comments.and_then(Value::as_array).filter(|list| !list.is_empty()) else {
        return Vec::new();
    };
    let mut lines = vec![
        format!("## Review Comments ({})", comments.len()),
        String::new(),
    ];
    for comment in comments {
        let author = format_author(comment.get("author").unwrap_or(&Value::Null))
            .unwrap_or_else(|| "unknown".to_string());
        let created_at = json_string(comment, "createdAt")
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        lines.push(format!("### {author}{created_at}"));
        lines.push(String::new());
        let location = json_string(comment, "path").map(|path| match json_i64(comment, "line")
            .or_else(|| json_i64(comment, "originalLine"))
        {
            Some(line) => format!("{path}:{line}"),
            None => path.to_string(),
        });
        push_line(&mut lines, "Location", location);
        push_line(&mut lines, "Side", json_string(comment, "side"));
        push_line(&mut lines, "Reply to", json_i64(comment, "inReplyToId"));
        push_line(&mut lines, "URL", json_string(comment, "url"));
        lines.push(String::new());
        let body = normalize_text(json_string(comment, "body"));
        lines.push(if body.is_empty() {
            "No review comment body.".to_string()
        } else {
            body
        });
        lines.push(String::new());
    }
    lines
}

fn format_pr_files(files: Option<&Value>) -> Vec<String> {
    let Some(files) = files.and_then(Value::as_array).filter(|list| !list.is_empty()) else {
        return Vec::new();
    };
    let mut lines = vec![format!("## Files ({})", files.len()), String::new()];
    for file in files.iter().take(FILE_PREVIEW_LIMIT) {
        let path = json_string(file, "path").unwrap_or("(unknown file)");
        let change_type = json_string(file, "changeType").unwrap_or("CHANGED");
        let additions = json_i64(file, "additions").unwrap_or(0);
        let deletions = json_i64(file, "deletions").unwrap_or(0);
        lines.push(format!(
            "- {path} [{change_type}] (+{additions} -{deletions})"
        ));
    }
    if files.len() > FILE_PREVIEW_LIMIT {
        lines.push(format!(
            "[…{} files elided…]",
            files.len() - FILE_PREVIEW_LIMIT
        ));
    }
    lines
}

/// omp `formatIssueView`.
#[must_use]
pub fn format_issue_view(data: &Value, issue: &str, comments: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let number = json_i64(data, "number").map_or_else(|| issue.to_string(), |n| n.to_string());
    let title = json_string(data, "title").unwrap_or("Untitled");
    lines.push(format!("# Issue #{number}: {title}"));
    lines.push(String::new());
    push_line(&mut lines, "State", json_string(data, "state"));
    push_line(&mut lines, "State reason", json_string(data, "stateReason"));
    push_line(
        &mut lines,
        "Author",
        format_author(data.get("author").unwrap_or(&Value::Null)),
    );
    push_line(&mut lines, "Created", json_string(data, "createdAt"));
    push_line(&mut lines, "Updated", json_string(data, "updatedAt"));
    push_line(&mut lines, "Labels", format_labels(data.get("labels")));
    push_line(&mut lines, "URL", json_string(data, "url"));
    lines.push(String::new());
    lines.push("## Body".to_string());
    lines.push(String::new());
    let body = normalize_text(json_string(data, "body"));
    lines.push(if body.is_empty() {
        "No description provided.".to_string()
    } else {
        body
    });
    if comments {
        let section = format_comments_section(data.get("comments"));
        if !section.is_empty() {
            lines.push(String::new());
            lines.extend(section);
        }
    }
    lines.join("\n").trim().to_string()
}

/// omp `formatPrView`.
#[must_use]
pub fn format_pr_view(data: &Value, pr: Option<&str>, comments: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let identifier = json_i64(data, "number")
        .map(|n| n.to_string())
        .or_else(|| pr.map(str::to_string))
        .unwrap_or_else(|| "current".to_string());
    let title = json_string(data, "title").unwrap_or("Untitled");
    lines.push(format!("# Pull Request #{identifier}: {title}"));
    lines.push(String::new());
    push_line(&mut lines, "State", json_string(data, "state"));
    push_line(&mut lines, "Draft", json_bool(data, "isDraft"));
    push_line(
        &mut lines,
        "Author",
        format_author(data.get("author").unwrap_or(&Value::Null)),
    );
    push_line(&mut lines, "Base", json_string(data, "baseRefName"));
    push_line(&mut lines, "Head", json_string(data, "headRefName"));
    push_line(&mut lines, "Review decision", json_string(data, "reviewDecision"));
    push_line(&mut lines, "Merge state", json_string(data, "mergeStateStatus"));
    push_line(&mut lines, "Created", json_string(data, "createdAt"));
    push_line(&mut lines, "Updated", json_string(data, "updatedAt"));
    push_line(&mut lines, "Labels", format_labels(data.get("labels")));
    push_line(&mut lines, "URL", json_string(data, "url"));
    lines.push(String::new());
    lines.push("## Body".to_string());
    lines.push(String::new());
    let body = normalize_text(json_string(data, "body"));
    lines.push(if body.is_empty() {
        "No description provided.".to_string()
    } else {
        body
    });

    let files = format_pr_files(data.get("files"));
    if !files.is_empty() {
        lines.push(String::new());
        lines.extend(files);
    }
    if comments {
        for section in [
            format_reviews_section(data.get("reviews")),
            format_review_comments_section(data.get("reviewComments")),
            format_comments_section(data.get("comments")),
        ] {
            if !section.is_empty() {
                lines.push(String::new());
                lines.extend(section);
            }
        }
    }
    lines.join("\n").trim().to_string()
}

/// omp `formatSearchResults`.
#[must_use]
pub fn format_search_results(kind: &str, query: &str, repo: Option<&str>, items: &[Value]) -> String {
    let mut lines = vec![
        format!("# GitHub {kind} search"),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push(format!("No {kind} found."));
        return lines.join("\n").trim().to_string();
    }
    for item in items {
        lines.push(String::new());
        let number = json_i64(item, "number")
            .map(|n| format!("#{n}"))
            .unwrap_or_else(|| "#?".to_string());
        let title = json_string(item, "title").unwrap_or("Untitled");
        lines.push(format!("- {number} {title}"));
        push_line(
            &mut lines,
            "  Repo",
            item.get("repository")
                .and_then(|value| value.get("nameWithOwner"))
                .and_then(Value::as_str),
        );
        push_line(&mut lines, "  State", json_string(item, "state"));
        push_line(
            &mut lines,
            "  Author",
            format_author(item.get("author").unwrap_or(&Value::Null)),
        );
        push_line(&mut lines, "  Labels", format_labels(item.get("labels")));
        push_line(&mut lines, "  Created", json_string(item, "createdAt"));
        push_line(&mut lines, "  Updated", json_string(item, "updatedAt"));
        push_line(&mut lines, "  URL", json_string(item, "url"));
    }
    lines.join("\n").trim().to_string()
}

/// omp `formatSearchCodeResults`.
#[must_use]
pub fn format_search_code_results(query: &str, repo: Option<&str>, items: &[Value]) -> String {
    let mut lines = vec![
        "# GitHub code search".to_string(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No code matches found.".to_string());
        return lines.join("\n").trim().to_string();
    }
    for item in items {
        lines.push(String::new());
        lines.push(format!(
            "- {}",
            json_string(item, "path").unwrap_or("(unknown path)")
        ));
        push_line(
            &mut lines,
            "  Repo",
            item.get("repository")
                .and_then(|value| value.get("nameWithOwner"))
                .and_then(Value::as_str),
        );
        push_line(&mut lines, "  Commit", format_short_sha(json_string(item, "sha")));
        push_line(&mut lines, "  URL", json_string(item, "url"));
        let fragment = item
            .get("textMatches")
            .and_then(Value::as_array)
            .and_then(|matches| {
                matches
                    .iter()
                    .filter_map(|m| m.get("fragment").and_then(Value::as_str))
                    .find(|fragment| !fragment.is_empty())
            });
        if let Some(fragment) = fragment {
            let first_line = normalize_text(Some(fragment))
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            push_line(&mut lines, "  Match", Some(first_line));
        }
    }
    lines.join("\n").trim().to_string()
}

/// omp `formatSearchCommitsResults`.
#[must_use]
pub fn format_search_commits_results(query: &str, repo: Option<&str>, items: &[Value]) -> String {
    let mut lines = vec![
        "# GitHub commits search".to_string(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No commits found.".to_string());
        return lines.join("\n").trim().to_string();
    }
    for item in items {
        lines.push(String::new());
        let sha = format_short_sha(json_string(item, "sha")).unwrap_or_else(|| "(unknown sha)".to_string());
        let subject = item
            .get("commit")
            .and_then(|commit| commit.get("message"))
            .and_then(Value::as_str)
            .map(|message| {
                normalize_text(Some(message))
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| "(no commit message)".to_string());
        lines.push(format!("- {sha} {subject}"));
        push_line(
            &mut lines,
            "  Repo",
            item.get("repository")
                .and_then(|value| value.get("nameWithOwner"))
                .and_then(Value::as_str),
        );
        let author = format_author(item.get("author").unwrap_or(&Value::Null)).or_else(|| {
            item.get("commit")
                .and_then(|commit| commit.get("author"))
                .and_then(|author| author.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        push_line(&mut lines, "  Author", author);
        let date = item
            .get("commit")
            .and_then(|commit| commit.get("author"))
            .and_then(|author| author.get("date"))
            .and_then(Value::as_str)
            .or_else(|| {
                item.get("commit")
                    .and_then(|commit| commit.get("committer"))
                    .and_then(|committer| committer.get("date"))
                    .and_then(Value::as_str)
            });
        push_line(&mut lines, "  Date", date);
        push_line(&mut lines, "  URL", json_string(item, "url"));
    }
    lines.join("\n").trim().to_string()
}

/// omp `formatSearchReposResults`.
#[must_use]
pub fn format_search_repos_results(query: &str, items: &[Value]) -> String {
    let mut lines = vec![
        "# GitHub repositories search".to_string(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No repositories found.".to_string());
        return lines.join("\n").trim().to_string();
    }
    for item in items {
        lines.push(String::new());
        lines.push(format!(
            "- {}",
            json_string(item, "fullName").unwrap_or("(unknown repository)")
        ));
        let description = normalize_text(json_string(item, "description"))
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        if !description.is_empty() {
            push_line(&mut lines, "  Description", Some(description));
        }
        push_line(&mut lines, "  Language", json_string(item, "language"));
        push_line(&mut lines, "  Stars", json_i64(item, "stargazersCount"));
        push_line(&mut lines, "  Forks", json_i64(item, "forksCount"));
        push_line(&mut lines, "  Open issues", json_i64(item, "openIssuesCount"));
        push_line(&mut lines, "  Visibility", json_string(item, "visibility"));
        push_line(&mut lines, "  Archived", json_bool(item, "isArchived"));
        push_line(&mut lines, "  Fork", json_bool(item, "isFork"));
        push_line(&mut lines, "  Updated", json_string(item, "updatedAt"));
        push_line(&mut lines, "  URL", json_string(item, "url"));
    }
    lines.join("\n").trim().to_string()
}

// ─── REST → normalized projections (omp `gh-search`) ───────────────────────

fn api_user(value: Option<&Value>) -> Value {
    let Some(user) = value else {
        return Value::Null;
    };
    let login = user.get("login").and_then(Value::as_str);
    let name = user.get("name").and_then(Value::as_str);
    if login.is_none() && name.is_none() {
        return Value::Null;
    }
    json!({"login": login, "name": name})
}

fn api_labels(value: Option<&Value>) -> Value {
    let labels: Vec<Value> = value
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .map(|label| json!({"name": label.get("name").and_then(Value::as_str)}))
                .collect()
        })
        .unwrap_or_default();
    Value::Array(labels)
}

static REPOSITORY_API_URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/repos/([^/]+/[^/]+)$").expect("repository api url regex")
});

/// Enterprise API roots prefix the path with `/api/v3`, so match the tail.
#[must_use]
pub fn repo_from_repository_url(value: Option<&str>) -> Option<String> {
    let value = value?;
    REPOSITORY_API_URL_PATTERN
        .captures(value)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_string())
}

/// Project a REST issue/PR item into the normalized search shape.
#[must_use]
pub fn api_issue_to_search_result(item: &Value) -> Value {
    let merged = item
        .get("pull_request")
        .and_then(|value| value.get("merged_at"))
        .is_some_and(|value| !value.is_null());
    let state = if merged {
        Some("merged".to_string())
    } else {
        item.get("state").and_then(Value::as_str).map(str::to_string)
    };
    json!({
        "author": api_user(item.get("user")),
        "createdAt": item.get("created_at").cloned().unwrap_or(Value::Null),
        "labels": api_labels(item.get("labels")),
        "number": item.get("number").cloned().unwrap_or(Value::Null),
        "repository": {"nameWithOwner": repo_from_repository_url(item.get("repository_url").and_then(Value::as_str))},
        "state": state,
        "title": item.get("title").cloned().unwrap_or(Value::Null),
        "updatedAt": item.get("updated_at").cloned().unwrap_or(Value::Null),
        "url": item.get("html_url").cloned().unwrap_or(Value::Null),
    })
}

/// Project a REST code-search item into the normalized search shape.
#[must_use]
pub fn api_code_to_search_result(item: &Value) -> Value {
    let text_matches: Vec<Value> = item
        .get("text_matches")
        .and_then(Value::as_array)
        .map(|matches| {
            matches
                .iter()
                .map(|m| {
                    json!({
                        "fragment": m.get("fragment").cloned().unwrap_or(Value::Null),
                        "property": m.get("property").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "path": item.get("path").cloned().unwrap_or(Value::Null),
        "repository": {"nameWithOwner": item.get("repository").and_then(|r| r.get("full_name")).cloned().unwrap_or(Value::Null)},
        "sha": item.get("sha").cloned().unwrap_or(Value::Null),
        "textMatches": text_matches,
        "url": item.get("html_url").cloned().unwrap_or(Value::Null),
    })
}

/// Project a REST commit-search item into the normalized search shape.
#[must_use]
pub fn api_commit_to_search_result(item: &Value) -> Value {
    let commit = item.get("commit");
    json!({
        "author": api_user(item.get("author")),
        "commit": commit.map_or(Value::Null, |commit| json!({
            "author": commit.get("author").cloned().unwrap_or(Value::Null),
            "committer": commit.get("committer").cloned().unwrap_or(Value::Null),
            "message": commit.get("message").cloned().unwrap_or(Value::Null),
        })),
        "committer": api_user(item.get("committer")),
        "id": item.get("node_id").cloned().unwrap_or(Value::Null),
        "repository": {"nameWithOwner": item.get("repository").and_then(|r| r.get("full_name")).cloned().unwrap_or(Value::Null)},
        "sha": item.get("sha").cloned().unwrap_or(Value::Null),
        "url": item.get("html_url").cloned().unwrap_or(Value::Null),
    })
}

/// Project a REST repository-search item into the normalized search shape.
#[must_use]
pub fn api_repo_to_search_result(item: &Value) -> Value {
    json!({
        "createdAt": item.get("created_at").cloned().unwrap_or(Value::Null),
        "description": item.get("description").cloned().unwrap_or(Value::Null),
        "forksCount": item.get("forks_count").cloned().unwrap_or(Value::Null),
        "fullName": item.get("full_name").cloned().unwrap_or(Value::Null),
        "isArchived": item.get("archived").cloned().unwrap_or(Value::Null),
        "isFork": item.get("fork").cloned().unwrap_or(Value::Null),
        "isPrivate": item.get("private").cloned().unwrap_or(Value::Null),
        "language": item.get("language").cloned().unwrap_or(Value::Null),
        "openIssuesCount": item.get("open_issues_count").cloned().unwrap_or(Value::Null),
        "owner": api_user(item.get("owner")),
        "stargazersCount": item.get("stargazers_count").cloned().unwrap_or(Value::Null),
        "updatedAt": item.get("updated_at").cloned().unwrap_or(Value::Null),
        "url": item.get("html_url").cloned().unwrap_or(Value::Null),
        "visibility": item.get("visibility").cloned().unwrap_or(Value::Null),
    })
}

fn search_items(response: &Value) -> Vec<Value> {
    response
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ─── Issue / PR view fetchers (shared with `issue://` / `pr://`) ───────────

/// omp `GH_ISSUE_STATE_REASON_FIELD`.
pub const GH_ISSUE_STATE_REASON_FIELD: &str = "stateReason";

fn drop_json_field(args: &[String], field: &str) -> Option<Vec<String>> {
    let mut next = args.to_vec();
    let json_index = next.iter().position(|arg| arg == "--json")?;
    let fields = next.get(json_index + 1)?.clone();
    let kept: Vec<&str> = fields
        .split(',')
        .filter(|candidate| *candidate != field)
        .collect();
    if kept.len() == fields.split(',').count() {
        return None;
    }
    next[json_index + 1] = kept.join(",");
    Some(next)
}

/// `gh --json` for issue data, retrying without the optional `stateReason`
/// field on gh releases that predate it.
async fn gh_issue_json_with_fallback<T: serde::de::DeserializeOwned>(
    cwd: &Path,
    args: &[String],
    signal: &AbortSignal,
    options: GhCommandOptions,
) -> Result<T> {
    match gh_json::<T>(cwd, args, signal, options).await {
        Ok(value) => Ok(value),
        Err(err) => {
            let message = err.to_string();
            if !message.contains("Unknown JSON field")
                || !(message.contains(GH_ISSUE_STATE_REASON_FIELD)
                    || message.contains(&format!("\"{GH_ISSUE_STATE_REASON_FIELD}\"")))
            {
                return Err(err);
            }
            let retry = drop_json_field(args, GH_ISSUE_STATE_REASON_FIELD)
                .ok_or_else(|| anyhow!("{message}"))?;
            gh_json::<T>(cwd, &retry, signal, options).await
        }
    }
}

/// Fetch review comments for a PR, page by page.
pub async fn fetch_pr_review_comments(
    cwd: &Path,
    repo: &str,
    pr_number: i64,
    signal: &AbortSignal,
) -> Result<Vec<Value>> {
    let reference = parse_repo_ref(repo);
    let mut comments: Vec<Value> = Vec::new();
    let mut page = 1_u64;
    loop {
        let mut args = vec!["api".to_string()];
        args.extend(gh_api_host_args(&reference));
        args.extend([
            "--method".to_string(),
            "GET".to_string(),
            format!("/repos/{}/pulls/{pr_number}/comments", reference.slug),
            "-F".to_string(),
            format!("per_page={REVIEW_COMMENTS_PAGE_SIZE}"),
            "-F".to_string(),
            format!("page={page}"),
        ]);
        let response: Vec<Value> = gh_json(
            cwd,
            &args,
            signal,
            GhCommandOptions {
                repo_provided: true,
                trim_output: None,
            },
        )
        .await?;
        let page_len = response.len();
        for comment in response {
            let Some(id) = comment.get("id").and_then(Value::as_i64) else {
                continue;
            };
            comments.push(json!({
                "author": api_user(comment.get("user")),
                "body": comment.get("body").cloned().unwrap_or(Value::Null),
                "createdAt": comment.get("created_at").cloned().unwrap_or(Value::Null),
                "id": id,
                "inReplyToId": comment.get("in_reply_to_id").cloned().unwrap_or(Value::Null),
                "line": comment.get("line").cloned().unwrap_or(Value::Null),
                "originalLine": comment.get("original_line").cloned().unwrap_or(Value::Null),
                "path": comment.get("path").cloned().unwrap_or(Value::Null),
                "side": comment.get("side").cloned().unwrap_or(Value::Null),
                "url": comment.get("html_url").cloned().unwrap_or(Value::Null),
            }));
        }
        // Compare the raw page length: a dropped malformed item must not end
        // pagination early and silently lose the remaining pages.
        if page_len < REVIEW_COMMENTS_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(comments)
}

/// Fetch an issue view, live.
pub async fn fetch_issue_view_fresh(
    cwd: &Path,
    repo: Option<&str>,
    identifier: &str,
    include_comments: bool,
    signal: &AbortSignal,
) -> Result<FreshResult> {
    let mut args = vec!["issue".to_string(), "view".to_string(), identifier.to_string()];
    append_repo_flag(&mut args, repo, Some(identifier));
    let fields = if include_comments {
        GH_ISSUE_FIELDS.join(",")
    } else {
        GH_ISSUE_FIELDS_NO_COMMENTS.join(",")
    };
    args.push("--json".to_string());
    args.push(fields);
    let data: Value = gh_issue_json_with_fallback(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: repo.is_some(),
            trim_output: None,
        },
    )
    .await?;
    let rendered = format_issue_view(&data, identifier, include_comments);
    let source_url = json_string(&data, "url").map(str::to_string);
    Ok(FreshResult {
        rendered,
        source_url,
        payload: data,
    })
}

/// Fetch a PR view, live.
pub async fn fetch_pr_view_fresh(
    cwd: &Path,
    repo: &str,
    number: i64,
    include_comments: bool,
    signal: &AbortSignal,
) -> Result<FreshResult> {
    let mut args = vec!["pr".to_string(), "view".to_string(), number.to_string()];
    append_repo_flag(&mut args, Some(repo), Some(&number.to_string()));
    let fields = if include_comments {
        GH_PR_FIELDS.join(",")
    } else {
        GH_PR_FIELDS_NO_COMMENTS.join(",")
    };
    args.push("--json".to_string());
    args.push(fields);
    let mut data: Value = gh_json(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: true,
            trim_output: None,
        },
    )
    .await?;
    if include_comments {
        if let Some(pr_number) = data.get("number").and_then(Value::as_i64) {
            let comments = fetch_pr_review_comments(cwd, repo, pr_number, signal).await?;
            if let Some(object) = data.as_object_mut() {
                object.insert("reviewComments".to_string(), Value::Array(comments));
            }
        }
    }
    let rendered = format_pr_view(&data, Some(&number.to_string()), include_comments);
    let source_url = json_string(&data, "url").map(str::to_string);
    Ok(FreshResult {
        rendered,
        source_url,
        payload: data,
    })
}

/// Cache-aware issue view. Shared by the `issue://` handler and the tool.
pub async fn get_or_fetch_issue(
    cwd: &Path,
    repo: Option<&str>,
    issue: &str,
    include_comments: bool,
    signal: &AbortSignal,
    settings: Option<&omp_settings::Settings>,
    cache_auth_key: Option<AuthKey>,
) -> Result<cache::CacheLookupResult> {
    let identifier = require_non_empty(Some(issue), "issue")?;
    if identifier.starts_with('-') {
        bail!("invalid issue identifier: {identifier}. Pass an issue number or URL.");
    }
    let auth_key = cache_auth_key.unwrap_or_else(|| {
        cache::resolve_github_cache_auth_key(None).map_or(AuthKey::Bypass, AuthKey::Key)
    });
    let (url_repo, url_number) = parse_issue_url(Some(&identifier));
    let mut resolved_repo = url_repo.or_else(|| normalize_optional_string(repo));
    let cache_number = url_number.or_else(|| parse_positive_decimal_int(Some(&identifier)));
    if cache_number.is_some() && resolved_repo.is_none() {
        // A resolution failure leaves the repo unknown: fall through to a
        // direct fetch so gh produces its own error instead of a friendlier one.
        resolved_repo = try_resolve_current_repo(cwd, signal).await;
    }

    let cwd = cwd.to_path_buf();
    let identifier_owned = identifier.clone();
    let fetcher: Arc<dyn Fn() -> cache::BoxFut<'static, Result<FreshResult>> + Send + Sync> = {
        let repo_for_fetch = resolved_repo.clone();
        let identifier = identifier_owned.clone();
        Arc::new(move || {
            let repo = repo_for_fetch.clone();
            let identifier = identifier.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                // A fresh signal is derived from the caller's; abort propagation
                // happens at the surrounding call, so an already-aborted turn
                // never reaches here.
                let signal = pa_agent::abort::AbortController::new().signal();
                fetch_issue_view_fresh(&cwd, repo.as_deref(), &identifier, include_comments, &signal).await
            })
        })
    };

    let (Some(repo), Some(number)) = (resolved_repo.clone(), cache_number) else {
        let fresh = (fetcher)().await?;
        return Ok(cache::CacheLookupResult {
            rendered: fresh.rendered,
            source_url: fresh.source_url,
            payload: fresh.payload,
            status: CacheStatus::Miss,
            fetched_at: cache::now_ms(),
        });
    };
    cache::get_or_fetch_view(CacheLookupOptions {
        repo,
        kind: CacheKind::Issue,
        number,
        include_comments,
        auth_key,
        fetch_fresh: fetcher,
        ttl: cache::resolve_cache_ttl(settings),
        now: None,
    })
    .await
}

/// Cache-aware PR view. Callers must supply a numeric PR number; branch-name
/// lookups bypass the cache.
pub async fn get_or_fetch_pr(
    cwd: &Path,
    repo: &str,
    number: i64,
    include_comments: bool,
    signal: &AbortSignal,
    settings: Option<&omp_settings::Settings>,
    cache_auth_key: Option<AuthKey>,
) -> Result<cache::CacheLookupResult> {
    let auth_key = cache_auth_key.unwrap_or_else(|| {
        cache::resolve_github_cache_auth_key(parse_repo_ref(repo).host.as_deref())
            .map_or(AuthKey::Bypass, AuthKey::Key)
    });
    if signal.is_aborted() {
        bail!("aborted");
    }
    let cwd = cwd.to_path_buf();
    let repo_owned = repo.to_string();
    let fetcher: Arc<dyn Fn() -> cache::BoxFut<'static, Result<FreshResult>> + Send + Sync> = Arc::new(move || {
        let repo = repo_owned.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            let signal = pa_agent::abort::AbortController::new().signal();
            fetch_pr_view_fresh(&cwd, &repo, number, include_comments, &signal).await
        })
    });
    cache::get_or_fetch_view(CacheLookupOptions {
        repo: repo.to_string(),
        kind: CacheKind::Pr,
        number,
        include_comments,
        auth_key,
        fetch_fresh: fetcher,
        ttl: cache::resolve_cache_ttl(settings),
        now: None,
    })
    .await
}

// ─── The tool ──────────────────────────────────────────────────────────────

/// The `github` tool, bound to a working directory.
///
/// Cache TTLs are read by the cache-aware view fetchers ([`get_or_fetch_issue`]
/// / [`get_or_fetch_pr`]), which take the session settings explicitly; the 11
/// ops themselves are stateless.
pub struct GithubTool {
    schema: Value,
    cwd: PathBuf,
}

/// Register the `github` tool for `cwd`.
#[must_use]
pub fn tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(GithubTool {
        schema: schema(),
        cwd: cwd.into(),
    })
}

impl AgentTool for GithubTool {
    fn name(&self) -> &'static str {
        "github"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "GitHub"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move {
            pa_agent::abort::throw_if_aborted(&signal)?;
            let params = GithubParams::from_value(&params)?;
            let result = self.run(&params, &signal).await?;
            if signal.is_aborted() {
                return Err(pa_agent::abort::aborted_error());
            }
            Ok(result)
        })
    }
}

impl GithubTool {
    async fn run(&self, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
        let cwd = self.cwd.as_path();
        match params.op.as_str() {
            "repo_view" => repo_view(cwd, params, signal).await,
            "file_read" => file_read(cwd, params, signal).await,
            "pr_create" => pr_create(cwd, params, signal).await,
            "pr_checkout" => pr_checkout(cwd, params, signal).await,
            "pr_push" => pr_push(cwd, params, signal).await,
            "search_issues" => search_issues(cwd, params, signal).await,
            "search_prs" => search_prs(cwd, params, signal).await,
            "search_code" => search_code(cwd, params, signal).await,
            "search_commits" => search_commits(cwd, params, signal).await,
            "search_repos" => search_repos(cwd, params, signal).await,
            "run_watch" => run_watch(cwd, params, signal).await,
            other => bail!("unknown github op: {other}"),
        }
    }
}

// ─── Ops ───────────────────────────────────────────────────────────────────

async fn repo_view(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo = normalize_optional_string(params.repo.as_deref());
    let branch = normalize_optional_string(params.branch.as_deref());
    let mut args = vec!["repo".to_string(), "view".to_string()];
    if let Some(repo) = &repo {
        args.push(repo.clone());
    }
    if let Some(branch) = &branch {
        args.push("--branch".to_string());
        args.push(branch.clone());
    }
    args.push("--json".to_string());
    args.push(GH_REPO_FIELDS.join(","));

    let data: Value = gh_json(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: repo.is_some(),
            trim_output: None,
        },
    )
    .await?;
    let text = format_repo_view(&data, repo.as_deref(), branch.as_deref());
    Ok(text_result(
        text,
        json_string(&data, "url").map(str::to_string),
        json!({"repo": repo, "branch": branch}),
    ))
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let suspicious = sample
        .iter()
        .filter(|byte| **byte < 0x09 || (**byte > 0x0d && **byte < 0x20))
        .count();
    suspicious * 100 / sample.len() > 30
}

fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

async fn file_read(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo = resolve_github_repo(
        cwd,
        normalize_optional_string(params.repo.as_deref()).as_deref(),
        None,
        signal,
    )
    .await?;
    let file_path = require_non_empty(params.path.as_deref(), "path")?;
    if file_path.starts_with('/') {
        bail!("path must be repository-relative");
    }
    let branch = normalize_optional_string(params.branch.as_deref());
    let endpoint_path: Vec<String> = file_path
        .split('/')
        .map(|segment| urlencode_segment(segment))
        .collect();
    let endpoint_path = endpoint_path.join("/");
    let reference = parse_repo_ref(&repo);
    let mut args = vec!["api".to_string()];
    args.extend(gh_api_host_args(&reference));
    args.extend([
        format!("/repos/{}/contents/{endpoint_path}", reference.slug),
        "--method".to_string(),
        "GET".to_string(),
        "-H".to_string(),
        "Accept: application/vnd.github+json".to_string(),
        "-H".to_string(),
        "Accept-Encoding: identity".to_string(),
    ]);
    if let Some(branch) = &branch {
        args.push("-f".to_string());
        args.push(format!("ref={branch}"));
    }
    let response: Value = gh_json(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: true,
            trim_output: None,
        },
    )
    .await
    .map_err(|error| {
        // Contents API 404s conflate repository, revision, path, and access
        // failures; identify the request without guessing which one it was.
        let revision = branch.as_deref().unwrap_or(DEFAULT_BRANCH_LABEL);
        anyhow!("GitHub file read failed for '{repo}@{revision}:{file_path}': {error}")
    })?;

    if response.get("type").and_then(Value::as_str) != Some("file") {
        bail!("GitHub path '{file_path}' is not a file.");
    }
    let fallback_host = reference
        .host
        .clone()
        .unwrap_or_else(cache::default_gh_host);
    let fallback_source_url = format!(
        "https://{fallback_host}/{}/blob/{}/{endpoint_path}",
        reference.slug,
        urlencode_segment(branch.as_deref().unwrap_or(DEFAULT_BRANCH_LABEL))
    );
    let source_url = json_string(&response, "html_url")
        .filter(|url| !url.is_empty())
        .map_or(fallback_source_url, str::to_string);

    let size = response.get("size").and_then(Value::as_i64);
    let size_label = size
        .filter(|size| *size >= 0)
        .map_or_else(|| "unknown size".to_string(), |size| {
            format_bytes(usize::try_from(size).unwrap_or(usize::MAX))
        });
    let (Some(encoding), Some(content)) = (
        response.get("encoding").and_then(Value::as_str),
        response.get("content").and_then(Value::as_str),
    ) else {
        return Ok(text_result(
            format!(
                "[GitHub did not return file bytes for '{file_path}' ({size_label}). Open {source_url} to view it.]"
            ),
            Some(source_url),
            json!({"repo": repo, "branch": branch}),
        ));
    };
    if encoding != "base64" {
        return Ok(text_result(
            format!(
                "[GitHub did not return file bytes for '{file_path}' ({size_label}). Open {source_url} to view it.]"
            ),
            Some(source_url),
            json!({"repo": repo, "branch": branch}),
        ));
    }
    let encoded: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| anyhow!("GitHub returned undecodable base64 content for '{file_path}'."))?;

    if let Some(mime) = sniff_image_mime(&bytes) {
        return Ok(text_result(
            format!(
                "Image file: {file_path}\nMIME: {mime}\nSize: {}\nSource: {source_url}",
                format_bytes(bytes.len())
            ),
            Some(source_url),
            json!({"repo": repo, "branch": branch}),
        ));
    }
    if is_probably_binary(&bytes) {
        return Ok(text_result(
            format!(
                "[Cannot read binary file '{file_path}' ({}); not valid UTF-8 text. Open {source_url} to view it.]",
                format_bytes(bytes.len())
            ),
            Some(source_url),
            json!({"repo": repo, "branch": branch}),
        ));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text_result(
            text,
            Some(source_url),
            json!({"repo": repo, "branch": branch}),
        )),
        Err(err) => {
            let bytes = err.into_bytes();
            Ok(text_result(
                format!(
                    "[Cannot read binary file '{file_path}' ({}); not valid UTF-8 text. Open {source_url} to view it.]",
                    format_bytes(bytes.len())
                ),
                Some(source_url),
                json!({"repo": repo, "branch": branch}),
            ))
        }
    }
}

fn urlencode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

async fn pr_create(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo = normalize_optional_string(params.repo.as_deref());
    let title = normalize_optional_string(params.title.as_deref());
    let body = params.body.as_deref();
    let base = normalize_optional_string(params.base.as_deref());
    let head = normalize_optional_string(params.head.as_deref());
    let draft = params.draft.unwrap_or(false);
    let fill = params.fill.unwrap_or(false);
    let reviewers = normalize_pr_identifier_list(params.reviewer.clone().map(PrArg::Many).as_ref());
    let assignees = normalize_pr_identifier_list(params.assignee.clone().map(PrArg::Many).as_ref());
    let labels = normalize_pr_identifier_list(params.label.clone().map(PrArg::Many).as_ref());

    if !fill && title.is_none() {
        bail!("title is required unless fill is true");
    }
    if fill && (title.is_some() || body.is_some()) {
        bail!("fill is mutually exclusive with title and body");
    }

    let mut args = vec!["pr".to_string(), "create".to_string()];
    append_repo_flag(&mut args, repo.as_deref(), None);
    if let Some(title) = &title {
        args.push("--title".to_string());
        args.push(title.clone());
    }
    if let Some(base) = &base {
        args.push("--base".to_string());
        args.push(base.clone());
    }
    if let Some(head) = &head {
        args.push("--head".to_string());
        args.push(head.clone());
    }
    if draft {
        args.push("--draft".to_string());
    }
    if fill {
        args.push("--fill".to_string());
    }
    for reviewer in &reviewers {
        args.push("--reviewer".to_string());
        args.push(reviewer.clone());
    }
    for assignee in &assignees {
        args.push("--assignee".to_string());
        args.push(assignee.clone());
    }
    for label in &labels {
        args.push("--label".to_string());
        args.push(label.clone());
    }

    let mut body_file: Option<PathBuf> = None;
    let result = async {
        if !fill {
            match body {
                Some(body) if !body.is_empty() => {
                    // Route through a temp file so multi-KB bodies stay clear of
                    // argv-length limits and shell-quoting hazards.
                    let path = std::env::temp_dir().join(format!(
                        "gh-pr-body-{}-{}.md",
                        std::process::id(),
                        short_hash(body)
                    ));
                    tokio::fs::write(&path, body)
                        .await
                        .map_err(|e| anyhow!("failed to stage PR body: {e}"))?;
                    body_file = Some(path.clone());
                    args.push("--body-file".to_string());
                    args.push(path.display().to_string());
                }
                _ => {
                    // Avoid gh dropping into an interactive editor when no body is given.
                    args.push("--body".to_string());
                    args.push(String::new());
                }
            }
        }
        gh_text(
            cwd,
            &args,
            signal,
            GhCommandOptions {
                repo_provided: repo.is_some(),
                trim_output: None,
            },
        )
        .await
    }
    .await;
    if let Some(path) = &body_file {
        let _ = tokio::fs::remove_file(path).await;
    }
    let output = result?;

    let url = output
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("https://"))
        .map_or_else(|| output.trim().to_string(), str::to_string);
    let (parsed_repo, pr_number) = parse_pull_request_url(Some(&url));
    let resolved_repo = repo.clone().or(parsed_repo);

    let mut pr_view: Option<Value> = None;
    if let (Some(resolved_repo), Some(pr_number)) = (resolved_repo.as_deref(), pr_number) {
        let view_args = vec![
            "pr".to_string(),
            "view".to_string(),
            pr_number.to_string(),
            "--repo".to_string(),
            resolved_repo.to_string(),
            "--json".to_string(),
            GH_PR_CREATE_FIELDS.join(","),
        ];
        // Best-effort summary; PR creation already succeeded.
        pr_view = gh_json(
            cwd,
            &view_args,
            signal,
            GhCommandOptions {
                repo_provided: true,
                trim_output: None,
            },
        )
        .await
        .ok();
    }

    let text = format_pr_create_result(
        &url,
        pr_number,
        pr_view.as_ref(),
        title.as_deref(),
        base.as_deref(),
        head.as_deref(),
        draft,
    );
    Ok(text_result(text, Some(url), Value::Null))
}

fn format_pr_create_result(
    url: &str,
    pr_number: Option<i64>,
    data: Option<&Value>,
    title: Option<&str>,
    base: Option<&str>,
    head: Option<&str>,
    draft: bool,
) -> String {
    let number = pr_number.or_else(|| data.and_then(|d| json_i64(d, "number")));
    let header_title = data
        .and_then(|d| json_string(d, "title"))
        .or(title)
        .unwrap_or("Untitled");
    let header = match number {
        Some(number) => format!("# Created Pull Request #{number}: {header_title}"),
        None => format!("# Created Pull Request: {header_title}"),
    };
    let mut lines = vec![header, String::new()];
    let resolved_url = data
        .and_then(|d| json_string(d, "url"))
        .filter(|value| !value.is_empty())
        .unwrap_or(url);
    push_line(&mut lines, "URL", Some(resolved_url));
    push_line(&mut lines, "State", data.and_then(|d| json_string(d, "state")));
    push_line(
        &mut lines,
        "Draft",
        data.and_then(|d| json_bool(d, "isDraft")).or(Some(draft)),
    );
    push_line(
        &mut lines,
        "Base",
        data.and_then(|d| json_string(d, "baseRefName"))
            .or(base)
            .map(str::to_string),
    );
    push_line(
        &mut lines,
        "Head",
        data.and_then(|d| json_string(d, "headRefName"))
            .or(head)
            .map(str::to_string),
    );
    push_line(
        &mut lines,
        "Author",
        data.and_then(|d| format_author(d.get("author").unwrap_or(&Value::Null))),
    );
    push_line(
        &mut lines,
        "Created",
        data.and_then(|d| json_string(d, "createdAt")),
    );
    push_line(
        &mut lines,
        "Labels",
        data.and_then(|d| format_labels(d.get("labels"))),
    );
    lines.join("\n").trim().to_string()
}

async fn pr_checkout(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo = normalize_optional_string(params.repo.as_deref());
    let force = params.force.unwrap_or(false);
    let pr_list = normalize_pr_identifier_list(params.pr.as_ref());
    let pr_refs: Vec<Option<String>> = if pr_list.is_empty() {
        vec![None]
    } else {
        pr_list.into_iter().map(Some).collect()
    };
    let is_multi = pr_refs.len() > 1;

    let mut outcomes: Vec<Value> = Vec::new();
    let mut failures: Vec<(Option<String>, String)> = Vec::new();
    for pr_ref in &pr_refs {
        match checkout_pull_request(cwd, pr_ref.as_deref(), repo.as_deref(), force, signal).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => {
                if signal.is_aborted() {
                    return Err(error);
                }
                failures.push((pr_ref.clone(), error.to_string()));
            }
        }
    }
    if !failures.is_empty() {
        let failure_lines: Vec<String> = failures
            .iter()
            .map(|(pr_ref, reason)| {
                format!(
                    "- {}: {reason}",
                    pr_ref.as_deref().unwrap_or("(current branch)")
                )
            })
            .collect();
        if outcomes.is_empty() {
            if failures.len() == 1 {
                bail!("{}", failures[0].1);
            }
            bail!(
                "all {} PR checkouts failed:\n{}",
                failures.len(),
                failure_lines.join("\n")
            );
        }
        // Partial success: report the worktrees that did get created alongside
        // the failures so the agent does not lose track of them.
        let sections: Vec<String> = outcomes.iter().map(format_pr_checkout_result).collect();
        let header = format!(
            "# {}/{} Pull Request Worktrees checked out ({} failed)",
            outcomes.len(),
            pr_refs.len(),
            failures.len()
        );
        let mut text = format!("{header}\n\n{}", join_sections(&sections));
        text.push_str("\n\n## Failed\n");
        text.push_str(&failure_lines.join("\n"));
        return Ok(text_result(
            text.trim().to_string(),
            None,
            json!({"repo": repo, "checkouts": outcomes}),
        ));
    }

    if !is_multi {
        let outcome = outcomes.remove(0);
        let text = format_pr_checkout_result(&outcome);
        let source_url = json_string(&outcome, "url").map(str::to_string);
        let branch = json_string(&outcome, "localBranch").map(str::to_string);
        return Ok(text_result(
            text,
            source_url,
            json!({
                "repo": repo.or_else(|| outcome.get("headRepo").and_then(Value::as_str).map(str::to_string)),
                "branch": branch,
                "worktreePath": outcome.get("worktreePath").cloned().unwrap_or(Value::Null),
                "remote": outcome.get("remoteName").cloned().unwrap_or(Value::Null),
                "remoteBranch": outcome.get("headRefName").cloned().unwrap_or(Value::Null),
                "checkouts": [outcome],
            }),
        ));
    }

    let sections: Vec<String> = outcomes.iter().map(format_pr_checkout_result).collect();
    let reused_count = outcomes
        .iter()
        .filter(|outcome| json_bool(outcome, "reused").unwrap_or(false))
        .count();
    let new_count = outcomes.len() - reused_count;
    let mut header_parts: Vec<String> = Vec::new();
    if new_count > 0 {
        header_parts.push(format!("{new_count} checked out"));
    }
    if reused_count > 0 {
        header_parts.push(format!("{reused_count} reused"));
    }
    let header = format!(
        "# {} Pull Request Worktrees ({})",
        outcomes.len(),
        header_parts.join(", ")
    );
    let text = format!("{header}\n\n{}", join_sections(&sections));
    Ok(text_result(
        text.trim().to_string(),
        None,
        json!({"repo": repo, "checkouts": outcomes}),
    ))
}

fn join_sections(sections: &[String]) -> String {
    sections
        .iter()
        .enumerate()
        .flat_map(|(idx, section)| {
            if idx == 0 {
                vec![section.clone()]
            } else {
                vec![String::new(), "---".to_string(), String::new(), section.clone()]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_pr_checkout_result(outcome: &Value) -> String {
    let reused = json_bool(outcome, "reused").unwrap_or(false);
    let number = json_i64(outcome, "number")
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
    let header = if reused {
        format!("# Pull Request #{number} Worktree")
    } else {
        format!("# Checked Out Pull Request #{number}")
    };
    let mut lines = vec![header, String::new()];
    push_line(&mut lines, "Title", json_string(outcome, "title"));
    push_line(&mut lines, "URL", json_string(outcome, "url"));
    push_line(&mut lines, "Base", json_string(outcome, "baseRefName"));
    push_line(&mut lines, "Head", json_string(outcome, "headRefName"));
    push_line(&mut lines, "Local branch", json_string(outcome, "localBranch"));
    push_line(&mut lines, "Worktree", json_string(outcome, "worktreePath"));
    push_line(&mut lines, "Remote", json_string(outcome, "remoteName"));
    push_line(&mut lines, "Remote URL", json_string(outcome, "remoteUrl"));
    push_line(
        &mut lines,
        "Cross repository",
        json_bool(outcome, "isCrossRepository"),
    );
    push_line(
        &mut lines,
        "Maintainer can modify",
        json_bool(outcome, "maintainerCanModify"),
    );
    lines.push(String::new());
    lines.push(if reused {
        "Reused the existing PR worktree.".to_string()
    } else {
        "Created a dedicated worktree for this PR and configured the local branch to push back to the PR head branch."
            .to_string()
    });
    lines.join("\n").trim().to_string()
}

fn sanitize_remote_name(value: &str) -> String {
    let slug: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "fork".to_string()
    } else {
        format!("fork-{trimmed}")
    }
}

async fn checkout_pull_request(
    cwd: &Path,
    pr_ref: Option<&str>,
    repo: Option<&str>,
    force: bool,
    signal: &AbortSignal,
) -> Result<Value> {
    if let Some(pr_ref) = pr_ref {
        if pr_ref.starts_with('-') {
            bail!("invalid PR identifier: {pr_ref}. Pass a PR number, URL, or branch name.");
        }
    }
    let mut args = vec!["pr".to_string(), "view".to_string()];
    if let Some(pr_ref) = pr_ref {
        args.push(pr_ref.to_string());
    }
    append_repo_flag(&mut args, repo, pr_ref);
    args.push("--json".to_string());
    args.push(GH_PR_CHECKOUT_FIELDS.join(","));
    let data: Value = gh_json(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: repo.is_some(),
            trim_output: None,
        },
    )
    .await?;

    let Some(pr_number) = json_i64(&data, "number") else {
        bail!("GitHub CLI did not return a pull request number.");
    };
    let head_ref_name = require_non_empty(json_string(&data, "headRefName"), "head branch")?;
    let head_ref_oid = require_non_empty(json_string(&data, "headRefOid"), "head commit")?;
    let repo_root = require_git_repo_root(cwd, signal).await?;
    let primary_root = require_primary_git_repo_root(&repo_root, signal).await?;
    let local_branch = format!("pr-{pr_number}");
    let worktree_path = worktree_dir(&format!("{pr_number}-{}", short_hash(&primary_root.display().to_string())));

    // Which remote holds the PR head: `origin` for a same-repo PR, else the fork.
    let is_cross_repository = json_bool(&data, "isCrossRepository").unwrap_or(false);
    let (remote_name, remote_url) = if is_cross_repository {
        let head_repository = require_non_empty(
            data.get("headRepository")
                .and_then(|value| value.get("nameWithOwner"))
                .and_then(Value::as_str),
            "head repository",
        )?;
        let pull_repo = parse_pull_request_url(json_string(&data, "url")).0;
        let host = pull_repo
            .as_deref()
            .map(|repo| parse_repo_ref(repo).host)
            .unwrap_or_default();
        let clone_args = vec![
            "repo".to_string(),
            "view".to_string(),
            format_repo_ref(host.as_deref(), &head_repository),
            "--json".to_string(),
            GH_REPO_CLONE_FIELDS.join(","),
        ];
        let clone_info: Value = gh_json(
            &repo_root,
            &clone_args,
            signal,
            GhCommandOptions {
                repo_provided: true,
                trim_output: None,
            },
        )
        .await?;
        let origin_url = git_config_get(&repo_root, "remote.origin.url", signal).await;
        let prefer_http = origin_url
            .as_deref()
            .is_some_and(|url| url.starts_with("http://") || url.starts_with("https://"));
        let clone_url = if prefer_http {
            json_string(&clone_info, "url")
                .or_else(|| json_string(&clone_info, "sshUrl"))
                .unwrap_or("")
                .to_string()
        } else {
            json_string(&clone_info, "sshUrl")
                .or_else(|| json_string(&clone_info, "url"))
                .unwrap_or("")
                .to_string()
        };
        if clone_url.is_empty() {
            bail!("Could not determine a clone URL for {head_repository}.");
        }
        let remotes = git_stdout(&repo_root, &["remote"], signal)
            .await
            .unwrap_or_default();
        let mut existing: Option<String> = None;
        for remote in remotes.lines().filter(|line| !line.is_empty()) {
            let url = git_config_get(&repo_root, &format!("remote.{remote}.url"), signal).await;
            if url.as_deref() == Some(clone_url.as_str()) {
                existing = Some(remote.to_string());
                break;
            }
        }
        match existing {
            Some(remote) => (remote, clone_url),
            None => {
                let owner = data
                    .get("headRepositoryOwner")
                    .and_then(|value| value.get("login"))
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| head_repository.split('/').next().unwrap_or("fork"));
                let preferred = sanitize_remote_name(owner);
                let mut name = preferred.clone();
                let mut suffix = 2;
                while git_config_get(&repo_root, &format!("remote.{name}.url"), signal)
                    .await
                    .is_some()
                {
                    name = format!("{preferred}-{suffix}");
                    suffix += 1;
                }
                git_stdout(&repo_root, &["remote", "add", &name, &clone_url], signal).await?;
                (name, clone_url)
            }
        }
    } else {
        let origin_url = git_config_get(&repo_root, "remote.origin.url", signal)
            .await
            .ok_or_else(|| anyhow!("origin remote is unavailable for this repository."))?;
        ("origin".to_string(), origin_url)
    };

    // Everything below mutates the shared `.git` of the primary checkout.
    // `worktree list --porcelain` emits blank-line-separated blocks of
    // `worktree PATH` / `HEAD sha` / `branch refs/heads/...`; find the block
    // whose branch line is this PR's local branch.
    let worktree_listing = git_stdout(&repo_root, &["worktree", "list", "--porcelain"], signal)
        .await
        .unwrap_or_default();
    let wanted_branch = format!("branch refs/heads/{local_branch}");
    let mut existing_worktree: Option<String> = None;
    let mut current_path: Option<String> = None;
    for line in worktree_listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if line.trim() == wanted_branch {
            existing_worktree = current_path.take();
            break;
        } else if line.trim().is_empty() {
            current_path = None;
        }
    }

    let fetch_refspec = format!("refs/heads/{head_ref_name}:refs/remotes/{remote_name}/{head_ref_name}");
    git_stdout(
        &repo_root,
        &["fetch", &remote_name, &fetch_refspec],
        signal,
    )
    .await?;

    if existing_worktree.is_none() {
        let local_ref = format!("refs/heads/{local_branch}");
        if ref_exists(&repo_root, &local_ref, signal).await {
            let existing_oid = git_stdout(&repo_root, &["rev-parse", &local_ref], signal)
                .await
                .unwrap_or_default();
            if existing_oid != head_ref_oid {
                if !force {
                    let short = format_short_sha(Some(&existing_oid)).unwrap_or_else(|| existing_oid.clone());
                    bail!(
                        "local branch {local_branch} already exists at {short}; pass force=true to reset it"
                    );
                }
                git_stdout(
                    &repo_root,
                    &["branch", "-f", &local_branch, &format!("refs/remotes/{remote_name}/{head_ref_name}")],
                    signal,
                )
                .await?;
            }
        } else {
            git_stdout(
                &repo_root,
                &["branch", &local_branch, &format!("refs/remotes/{remote_name}/{head_ref_name}")],
                signal,
            )
            .await?;
        }
    }

    let config_prefix = format!("branch.{local_branch}.");
    for (key, value) in [
        ("remote", remote_name.clone()),
        ("merge", format!("refs/heads/{head_ref_name}")),
        ("pushRemote", remote_name.clone()),
        ("ompPrHeadRef", head_ref_name.clone()),
        ("ompPrUrl", json_string(&data, "url").unwrap_or("").to_string()),
        (
            "ompPrIsCrossRepository",
            is_cross_repository.to_string(),
        ),
        (
            "ompPrMaintainerCanModify",
            json_bool(&data, "maintainerCanModify").unwrap_or(false).to_string(),
        ),
    ] {
        git_config_set(&repo_root, &format!("{config_prefix}{key}"), &value, signal).await?;
    }

    let mut final_worktree_path = existing_worktree.as_deref().map_or(worktree_path, PathBuf::from);
    if existing_worktree.is_none() {
        if let Some(parent) = final_worktree_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // A leftover directory would make `worktree add` refuse; pick the next
        // free suffix instead of reusing it.
        let base = final_worktree_path.clone();
        let mut suffix = 1;
        while final_worktree_path.exists() {
            suffix += 1;
            final_worktree_path = PathBuf::from(format!("{}-{suffix}", base.display()));
        }
        git_stdout(
            &repo_root,
            &[
                "worktree",
                "add",
                &final_worktree_path.display().to_string(),
                &local_branch,
            ],
            signal,
        )
        .await?;
    }
    let resolved = tokio::fs::canonicalize(&final_worktree_path)
        .await
        .unwrap_or(final_worktree_path);

    Ok(json!({
        "number": pr_number,
        "title": data.get("title").cloned().unwrap_or(Value::Null),
        "url": data.get("url").cloned().unwrap_or(Value::Null),
        "baseRefName": data.get("baseRefName").cloned().unwrap_or(Value::Null),
        "headRefName": head_ref_name,
        "localBranch": local_branch,
        "worktreePath": resolved.display().to_string(),
        "remoteName": remote_name,
        "remoteUrl": remote_url,
        "isCrossRepository": is_cross_repository,
        "maintainerCanModify": json_bool(&data, "maintainerCanModify"),
        "reused": existing_worktree.is_some(),
    }))
}

/// The local branch `pr_checkout` made for `pr` (a number, `#N`, or PR URL).
#[must_use]
pub fn pr_checkout_branch(pr: Option<&PrArg>) -> Option<String> {
    let refs = normalize_pr_identifier_list(pr);
    let first = refs.first()?;
    let number = Some(first.strip_prefix('#').unwrap_or(first))
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_string)
        .or_else(|| parse_pull_request_url(Some(first)).1.map(|n| n.to_string()));
    number.map(|number| format!("pr-{number}"))
}

async fn pr_push(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo_root = require_git_repo_root(cwd, signal).await?;
    let local_branch = match normalize_optional_string(params.branch.as_deref()) {
        Some(branch) => branch,
        None => match pr_checkout_branch(params.pr.as_ref()) {
            Some(branch) => branch,
            None => require_current_git_branch(&repo_root, signal).await?,
        },
    };
    if !ref_exists(&repo_root, &format!("refs/heads/{local_branch}"), signal).await {
        bail!("local branch {local_branch} does not exist");
    }

    let config_prefix = format!("branch.{local_branch}.");
    let head_ref = git_config_get(&repo_root, &format!("{config_prefix}ompPrHeadRef"), signal).await;
    let Some(head_ref) = head_ref else {
        bail!("branch {local_branch} has no PR push metadata; check it out via op: pr_checkout first");
    };
    let push_remote = git_config_get(&repo_root, &format!("{config_prefix}pushRemote"), signal).await;
    let remote = match push_remote {
        Some(remote) => Some(remote),
        None => git_config_get(&repo_root, &format!("{config_prefix}remote"), signal).await,
    };
    let Some(remote_name) = remote else {
        bail!("branch {local_branch} has no configured push remote");
    };
    let remote_url = git_config_get(&repo_root, &format!("remote.{remote_name}.url"), signal).await;
    let pr_url = git_config_get(&repo_root, &format!("{config_prefix}ompPrUrl"), signal).await;

    let current_branch = git_stdout(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"], signal)
        .await
        .unwrap_or_default();
    let source_ref = if current_branch == local_branch {
        "HEAD".to_string()
    } else {
        format!("refs/heads/{local_branch}")
    };
    let refspec = format!("{source_ref}:refs/heads/{head_ref}");
    let force_with_lease = params.force_with_lease.unwrap_or(false);

    let mut push_args = vec!["push".to_string()];
    if force_with_lease {
        push_args.push("--force-with-lease".to_string());
    }
    push_args.push(remote_name.clone());
    push_args.push(refspec.clone());
    let push_arg_refs: Vec<&str> = push_args.iter().map(String::as_str).collect();
    git_stdout(&repo_root, &push_arg_refs, signal).await?;

    // A successful push changes what `pr://N` and `pr://N/diff` should show;
    // drop the cached rows so push → re-read diff sees fresh data.
    let (pushed_repo, pushed_pr) = parse_pull_request_url(pr_url.as_deref());
    if let Some(pushed_pr) = pushed_pr {
        cache::invalidate_all_for_number(pushed_pr, pushed_repo.as_deref());
    }

    let mut lines = vec!["# Pushed Pull Request Branch".to_string(), String::new()];
    push_line(&mut lines, "Local branch", Some(local_branch.clone()));
    push_line(&mut lines, "Remote", Some(remote_name.clone()));
    push_line(&mut lines, "Remote branch", Some(head_ref.clone()));
    push_line(&mut lines, "Remote URL", remote_url.clone());
    push_line(&mut lines, "PR", pr_url.clone());
    push_line(&mut lines, "Force with lease", Some(force_with_lease));
    lines.push(String::new());
    lines.push(format!(
        "Pushed {local_branch} to {remote_name}:{head_ref}."
    ));
    Ok(text_result(
        lines.join("\n").trim().to_string(),
        pr_url,
        json!({
            "branch": local_branch,
            "remote": remote_name,
            "remoteBranch": head_ref,
        }),
    ))
}

async fn search_issues(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    search_issue_like(cwd, params, signal, "issues", "is:issue").await
}

async fn search_prs(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    search_issue_like(cwd, params, signal, "pull requests", "is:pr").await
}

async fn search_issue_like(
    cwd: &Path,
    params: &GithubParams,
    signal: &AbortSignal,
    kind: &str,
    type_qualifier: &str,
) -> Result<AgentToolResult> {
    let limit = resolve_search_limit(params.limit)?;
    let date_field = resolve_search_date_field(
        if type_qualifier == "is:pr" { "prs" } else { "issues" },
        params.date_field.as_deref(),
    );
    let date_qualifier = build_search_date_qualifier(
        &date_field,
        params.since.as_deref(),
        params.until.as_deref(),
        cache::now_ms(),
    )?;
    let display_query = compose_search_query(&[params.query.as_deref(), date_qualifier.as_deref()])?;
    let repo = resolve_search_repo_scope(
        cwd,
        normalize_optional_string(params.repo.as_deref()).as_deref(),
        Some(&display_query),
        signal,
    )
    .await;
    let (qualifier, host) = search_scope(repo.as_deref());
    let api_query = compose_search_query(&[
        Some(display_query.as_str()),
        qualifier.as_deref(),
        Some(type_qualifier),
    ])?;
    let args = build_gh_api_search_args("issues", &api_query, limit, host.as_deref(), &[]);

    let response: Value = gh_json(cwd, &args, signal, GhCommandOptions::default()).await?;
    let items: Vec<Value> = search_items(&response)
        .iter()
        .map(api_issue_to_search_result)
        .collect();
    let text = format_search_results(kind, &display_query, repo.as_deref(), &items);
    Ok(text_result(
        text,
        None,
        json!({"results": items.len()}),
    ))
}

async fn resolve_search_repo_scope(
    cwd: &Path,
    repo: Option<&str>,
    query: Option<&str>,
    signal: &AbortSignal,
) -> Option<String> {
    if let Some(repo) = normalize_optional_string(repo) {
        return Some(repo);
    }
    if query_carries_repo_scope(query) {
        return None;
    }
    // Resolution failures fall back to `None` so the search proceeds across all
    // of GitHub instead of throwing.
    try_resolve_current_repo(cwd, signal).await
}

async fn search_code(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let query = require_non_empty(params.query.as_deref(), "query")?;
    let since = normalize_optional_string(params.since.as_deref());
    let until = normalize_optional_string(params.until.as_deref());
    if since.is_some() || until.is_some() {
        bail!("search_code does not support since/until; GitHub code search has no date qualifier.");
    }
    let limit = resolve_search_limit(params.limit)?;
    let repo = resolve_search_repo_scope(
        cwd,
        normalize_optional_string(params.repo.as_deref()).as_deref(),
        Some(&query),
        signal,
    )
    .await;
    let (qualifier, host) = search_scope(repo.as_deref());
    let api_query = compose_search_query(&[Some(query.as_str()), qualifier.as_deref()])?;
    let args = build_gh_api_search_args(
        "code",
        &api_query,
        limit,
        host.as_deref(),
        &["Accept: application/vnd.github.text-match+json"],
    );
    let response: Value = gh_json(cwd, &args, signal, GhCommandOptions::default()).await?;
    let items: Vec<Value> = search_items(&response)
        .iter()
        .map(api_code_to_search_result)
        .collect();
    let text = format_search_code_results(&query, repo.as_deref(), &items);
    Ok(text_result(text, None, json!({"results": items.len()})))
}

async fn search_commits(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let limit = resolve_search_limit(params.limit)?;
    let date_field = resolve_search_date_field("commits", params.date_field.as_deref());
    let date_qualifier = build_search_date_qualifier(
        &date_field,
        params.since.as_deref(),
        params.until.as_deref(),
        cache::now_ms(),
    )?;
    let display_query = compose_search_query(&[params.query.as_deref(), date_qualifier.as_deref()])?;
    let repo = resolve_search_repo_scope(
        cwd,
        normalize_optional_string(params.repo.as_deref()).as_deref(),
        Some(&display_query),
        signal,
    )
    .await;
    let (qualifier, host) = search_scope(repo.as_deref());
    let api_query = compose_search_query(&[Some(display_query.as_str()), qualifier.as_deref()])?;
    let args = build_gh_api_search_args("commits", &api_query, limit, host.as_deref(), &[]);
    let response: Value = gh_json(cwd, &args, signal, GhCommandOptions::default()).await?;
    let items: Vec<Value> = search_items(&response)
        .iter()
        .map(api_commit_to_search_result)
        .collect();
    let text = format_search_commits_results(&display_query, repo.as_deref(), &items);
    Ok(text_result(text, None, json!({"results": items.len()})))
}

async fn search_repos(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let limit = resolve_search_limit(params.limit)?;
    let date_field = resolve_search_date_field("repos", params.date_field.as_deref());
    let date_qualifier = build_search_date_qualifier(
        &date_field,
        params.since.as_deref(),
        params.until.as_deref(),
        cache::now_ms(),
    )?;
    let query = compose_search_query(&[params.query.as_deref(), date_qualifier.as_deref()])?;
    let args = build_gh_api_search_args("repositories", &query, limit, None, &[]);
    let response: Value = gh_json(cwd, &args, signal, GhCommandOptions::default()).await?;
    let items: Vec<Value> = search_items(&response)
        .iter()
        .map(api_repo_to_search_result)
        .collect();
    let text = format_search_repos_results(&query, &items);
    Ok(text_result(text, None, json!({"results": items.len()})))
}

// ─── run_watch ─────────────────────────────────────────────────────────────

/// Poll cadence for `run_watch`.
const RUN_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Upper bound for one `run_watch` call (omp bounds the watch by the tool
/// deadline; the Rust port caps it explicitly so a queued workflow cannot hang).
const RUN_WATCH_MAX: Duration = Duration::from_mins(15);
/// Default log lines per failed job (omp `tail`).
const RUN_TAIL_DEFAULT: usize = 80;

fn job_state(job: &Value) -> (&str, &str) {
    (
        job.get("status").and_then(Value::as_str).unwrap_or(""),
        job.get("conclusion").and_then(Value::as_str).unwrap_or(""),
    )
}

fn is_terminal_status(status: &str) -> bool {
    status == "completed"
}

fn is_failed_conclusion(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure"
    )
}

async fn fetch_run_jobs(
    cwd: &Path,
    slug: &str,
    host: Option<&str>,
    run_id: i64,
    signal: &AbortSignal,
) -> Result<Vec<Value>> {
    let mut jobs: Vec<Value> = Vec::new();
    let mut page = 1_u64;
    loop {
        let mut args = vec!["api".to_string()];
        if let Some(host) = host {
            args.push("--hostname".to_string());
            args.push(host.to_string());
        }
        args.push(format!("/repos/{slug}/actions/runs/{run_id}/jobs"));
        args.push("-F".to_string());
        args.push("per_page=100".to_string());
        args.push("-F".to_string());
        args.push(format!("page={page}"));
        let response: Value = gh_json(
            cwd,
            &args,
            signal,
            GhCommandOptions {
                repo_provided: true,
                trim_output: None,
            },
        )
        .await?;
        let page_jobs = response
            .get("jobs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let page_len = page_jobs.len();
        jobs.extend(page_jobs);
        let total = response.get("total_count").and_then(Value::as_i64);
        if page_len < 100 || total.is_some_and(|total| i64::try_from(jobs.len()).unwrap_or(i64::MAX) >= total) {
            break;
        }
        page += 1;
    }
    Ok(jobs)
}

async fn fetch_job_logs(
    cwd: &Path,
    slug: &str,
    host: Option<&str>,
    job_id: i64,
    signal: &AbortSignal,
) -> String {
    let mut args = vec!["api".to_string()];
    if let Some(host) = host {
        args.push("--hostname".to_string());
        args.push(host.to_string());
    }
    args.push(format!("/repos/{slug}/actions/jobs/{job_id}/logs"));
    args.push("--allow-escape-sequences".to_string());
    match gh_text(
        cwd,
        &args,
        signal,
        GhCommandOptions {
            repo_provided: true,
            trim_output: None,
        },
    )
    .await
    {
        Ok(text) => text,
        Err(error) => {
            let message = error.to_string();
            if !message.contains("unknown flag") {
                return format!("[log unavailable: {message}]");
            }
            // Older gh releases have no `--allow-escape-sequences`.
            let retry: Vec<String> = args
                .into_iter()
                .filter(|arg| arg != "--allow-escape-sequences")
                .collect();
            gh_text(
                cwd,
                &retry,
                signal,
                GhCommandOptions {
                    repo_provided: true,
                    trim_output: None,
                },
            )
            .await
            .unwrap_or_else(|error| format!("[log unavailable: {error}]"))
        }
    }
}

fn tail_lines(text: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

async fn run_watch(cwd: &Path, params: &GithubParams, signal: &AbortSignal) -> Result<AgentToolResult> {
    let repo = resolve_github_repo(
        cwd,
        normalize_optional_string(params.repo.as_deref()).as_deref(),
        None,
        signal,
    )
    .await?;
    let reference = parse_repo_ref(&repo);
    let tail = params
        .tail
        .filter(|tail| tail.is_finite() && *tail > 0.0)
        .map_or(RUN_TAIL_DEFAULT, |tail| {
            usize::try_from(tail.floor() as i64).unwrap_or(RUN_TAIL_DEFAULT)
        });

    let run_id = match parse_run_id(params.run.as_deref()) {
        Some(run_id) => Some(run_id),
        None if normalize_optional_string(params.run.as_deref()).is_some() => {
            bail!(
                "invalid run identifier: {}. Pass an Actions run id or URL.",
                params.run.as_deref().unwrap_or_default()
            )
        }
        None => None,
    };

    // Resolve the run(s) to watch: the named one, or every run for HEAD.
    let mut run_ids: Vec<i64> = Vec::new();
    if let Some(run_id) = run_id {
        run_ids.push(run_id);
    } else {
        let mut args = vec!["api".to_string()];
        if let Some(host) = &reference.host {
            args.push("--hostname".to_string());
            args.push(host.clone());
        }
        args.push(format!("/repos/{}/actions/runs", reference.slug));
        match normalize_optional_string(params.branch.as_deref()) {
            Some(branch) => {
                // The branch head sha keeps `run_watch` on the pushed commit,
                // not on whatever the branch moved to since.
                let head_args = {
                    let mut args = vec!["api".to_string()];
                    if let Some(host) = &reference.host {
                        args.push("--hostname".to_string());
                        args.push(host.clone());
                    }
                    args.push(format!("/repos/{}/branches/{branch}", reference.slug));
                    args
                };
                let head: Value = gh_json(
                    cwd,
                    &head_args,
                    signal,
                    GhCommandOptions {
                        repo_provided: true,
                        trim_output: None,
                    },
                )
                .await?;
                let sha = head
                    .get("commit")
                    .and_then(|value| value.get("sha"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if sha.is_empty() {
                    bail!("GitHub did not return a head commit for branch {branch}.");
                }
                args.push("-F".to_string());
                args.push(format!("head_sha={sha}"));
            }
            None => {
                let sha = require_current_git_head(cwd, signal).await?;
                args.push("-F".to_string());
                args.push(format!("head_sha={sha}"));
            }
        }
        args.push("-F".to_string());
        args.push("per_page=100".to_string());
        let response: Value = gh_json(
            cwd,
            &args,
            signal,
            GhCommandOptions {
                repo_provided: true,
                trim_output: None,
            },
        )
        .await?;
        for run in response
            .get("workflow_runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = run.get("id").and_then(Value::as_i64) {
                run_ids.push(id);
            }
        }
        if run_ids.is_empty() {
            bail!("No Actions runs found for the current HEAD. Pass `run` or `branch` explicitly.");
        }
    }

    let deadline = tokio::time::Instant::now() + RUN_WATCH_MAX;
    let mut reports: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for run_id in run_ids {
        // Poll the run and its jobs until the run reaches a terminal status.
        let mut run_status;
        let mut run_conclusion;
        let mut run_title;
        let mut run_url;
        let mut jobs: Vec<Value>;
        loop {
            if signal.is_aborted() {
                bail!("aborted");
            }
            let mut args = vec!["api".to_string()];
            if let Some(host) = &reference.host {
                args.push("--hostname".to_string());
                args.push(host.clone());
            }
            args.push(format!("/repos/{}/actions/runs/{run_id}", reference.slug));
            let run: Value = gh_json(
                cwd,
                &args,
                signal,
                GhCommandOptions {
                    repo_provided: true,
                    trim_output: None,
                },
            )
            .await?;
            run_status = run
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            run_conclusion = run
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            run_title = run
                .get("display_title")
                .and_then(Value::as_str)
                .or_else(|| run.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            run_url = run
                .get("html_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            jobs = fetch_run_jobs(cwd, &reference.slug, reference.host.as_deref(), run_id, signal).await?;

            if is_terminal_status(&run_status) {
                break;
            }
            // Fast-fail on a failed job instead of waiting for the whole matrix.
            if jobs
                .iter()
                .any(|job| is_failed_conclusion(job_state(job).1))
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                () = signal.aborted() => bail!("aborted"),
                () = tokio::time::sleep(RUN_POLL_INTERVAL) => {}
            }
        }

        let mut lines = vec![
            format!(
                "# Actions Run {run_id}: {}",
                if run_title.is_empty() { "(untitled)" } else { &run_title }
            ),
            String::new(),
        ];
        push_line(&mut lines, "URL", Some(run_url.clone()).filter(|url| !url.is_empty()));
        push_line(&mut lines, "Status", Some(run_status.clone()).filter(|s| !s.is_empty()));
        push_line(
            &mut lines,
            "Conclusion",
            Some(run_conclusion.clone()).filter(|s| !s.is_empty()),
        );
        push_line(&mut lines, "Jobs", Some(jobs.len()));

        let mut job_lines: Vec<String> = Vec::new();
        for job in &jobs {
            let (status, conclusion) = job_state(job);
            let name = job.get("name").and_then(Value::as_str).unwrap_or("(unnamed job)");
            let marker = if is_failed_conclusion(conclusion) {
                "FAIL"
            } else if conclusion == "success" {
                "ok"
            } else if status.is_empty() {
                "?"
            } else {
                status
            };
            job_lines.push(format!(
                "- [{marker}] {name}{}",
                if conclusion.is_empty() {
                    String::new()
                } else {
                    format!(" ({conclusion})")
                }
            ));
        }
        if !job_lines.is_empty() {
            lines.push(String::new());
            lines.push(format!("## Jobs ({})", jobs.len()));
            lines.push(String::new());
            lines.extend(job_lines);
        }

        for job in &jobs {
            let (_, conclusion) = job_state(job);
            if !is_failed_conclusion(conclusion) {
                continue;
            }
            let Some(job_id) = job.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let name = job.get("name").and_then(Value::as_str).unwrap_or("(unnamed job)");
            let logs = fetch_job_logs(cwd, &reference.slug, reference.host.as_deref(), job_id, signal).await;
            let tail = tail_lines(&logs, tail);
            failures.push(format!("### {name}\n\n```\n{tail}\n```"));
        }
        reports.push(lines.join("\n").trim().to_string());
    }

    let mut text = reports.join("\n\n---\n\n");
    if !failures.is_empty() {
        text.push_str("\n\n## Failed job logs\n\n");
        text.push_str(&failures.join("\n\n"));
    }
    Ok(text_result(
        text,
        None,
        json!({"runs": reports.len(), "failedJobs": failures.len()}),
    ))
}

// ─── Result helper ─────────────────────────────────────────────────────────

fn text_result(text: String, source_url: Option<String>, details: Value) -> AgentToolResult {
    let text = match &source_url {
        Some(url) if !text.contains(url.as_str()) => format!("{text}\n\nURL: {url}"),
        _ => text,
    };
    AgentToolResult {
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details,
        terminate: None,
        is_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_enum() -> Vec<String> {
        schema()["properties"]["op"]["enum"]
            .as_array()
            .expect("op enum")
            .iter()
            .map(|value| value.as_str().expect("string op").to_string())
            .collect()
    }

    #[test]
    fn schema_matches_ts_reference() {
        assert_eq!(
            schema(),
            json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": [
                            "repo_view", "file_read", "pr_create", "pr_checkout", "pr_push",
                            "search_issues", "search_prs", "search_code", "search_commits",
                            "search_repos", "run_watch"
                        ],
                        "description": "github operation"
                    },
                    "repo": {"type": "string", "description": "owner/repo"},
                    "branch": {"type": "string", "description": "branch"},
                    "path": {"type": "string", "description": "repository-relative file path"},
                    "pr": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}}
                        ],
                        "description": "pr number, url, or branch"
                    },
                    "force": {"type": "boolean", "description": "reset existing local branch"},
                    "forceWithLease": {"type": "boolean", "description": "force-with-lease push"},
                    "title": {"type": "string", "description": "pr title"},
                    "body": {"type": "string", "description": "pr body markdown"},
                    "base": {"type": "string", "description": "pr base branch"},
                    "head": {"type": "string", "description": "pr head branch"},
                    "draft": {"type": "boolean", "description": "open pr as draft"},
                    "fill": {"type": "boolean", "description": "auto-fill pr title/body from commits"},
                    "reviewer": {"type": "array", "items": {"type": "string"}, "description": "reviewers"},
                    "assignee": {"type": "array", "items": {"type": "string"}, "description": "assignees"},
                    "label": {"type": "array", "items": {"type": "string"}, "description": "labels"},
                    "query": {"type": "string", "description": "search query"},
                    "since": {"type": "string", "description": "lower-bound date filter"},
                    "until": {"type": "string", "description": "upper-bound date filter"},
                    "dateField": {"type": "string", "enum": ["created", "updated"], "description": "date field"},
                    "limit": {"type": "number", "description": "max results"},
                    "run": {"type": "string", "description": "actions run id or url"},
                    "tail": {"type": "number", "description": "log lines per failed job"}
                },
                "required": ["op"]
            })
        );
    }

    #[test]
    fn schema_requires_only_op_and_has_no_additional_properties() {
        let schema = schema();
        assert_eq!(schema["required"], json!(["op"]));
        assert!(schema.get("additionalProperties").is_none());
        assert_eq!(op_enum().len(), 11);
    }

    #[test]
    fn pr_is_the_only_union() {
        let schema = schema();
        let pr = &schema["properties"]["pr"];
        assert_eq!(pr["anyOf"][0], json!({"type": "string"}));
        assert_eq!(
            pr["anyOf"][1],
            json!({"type": "array", "items": {"type": "string"}})
        );
        // Every other property is a single-type schema.
        for (name, property) in schema["properties"].as_object().unwrap() {
            if name == "pr" {
                continue;
            }
            assert!(
                property.get("anyOf").is_none(),
                "{name} must not be a union"
            );
        }
    }

    #[test]
    fn approval_splits_read_only_ops_from_mutating_ones() {
        for op in GITHUB_READONLY_OPS {
            assert_eq!(approval_for(&json!({"op": op})), "read", "{op}");
        }
        for op in ["pr_create", "pr_checkout", "pr_push"] {
            assert_eq!(approval_for(&json!({"op": op})), "exec", "{op}");
        }
        assert_eq!(approval_for(&json!({})), "exec");
        assert_eq!(approval_for(&json!({"op": 7})), "exec");
    }

    #[test]
    fn params_accept_string_and_array_pr() {
        let single: GithubParams =
            serde_json::from_value(json!({"op": "pr_checkout", "pr": "42"})).unwrap();
        assert_eq!(
            normalize_pr_identifier_list(single.pr.as_ref()),
            vec!["42".to_string()]
        );
        let many: GithubParams =
            serde_json::from_value(json!({"op": "pr_checkout", "pr": ["1", " #2 ", ""]})).unwrap();
        assert_eq!(
            normalize_pr_identifier_list(many.pr.as_ref()),
            vec!["1".to_string(), "#2".to_string()]
        );
        let none: GithubParams = serde_json::from_value(json!({"op": "repo_view"})).unwrap();
        assert!(normalize_pr_identifier_list(none.pr.as_ref()).is_empty());
    }

    #[test]
    fn search_limit_defaults_and_clamps() {
        assert_eq!(resolve_search_limit(None).unwrap(), SEARCH_LIMIT_DEFAULT);
        assert_eq!(resolve_search_limit(Some(3.9)).unwrap(), 3);
        assert_eq!(resolve_search_limit(Some(500.0)).unwrap(), SEARCH_LIMIT_MAX);
        assert!(resolve_search_limit(Some(0.0)).is_err());
        assert!(resolve_search_limit(Some(-1.0)).is_err());
        assert!(resolve_search_limit(Some(f64::NAN)).is_err());
        assert!(resolve_search_limit(Some(f64::INFINITY)).is_err());
    }

    #[test]
    fn date_bound_relative_units() {
        // 2026-05-09T00:00:00Z
        let now = 1_778_284_800_000;
        assert_eq!(parse_search_date_bound("3d", now).unwrap(), "2026-05-06");
        assert_eq!(parse_search_date_bound("2w", now).unwrap(), "2026-04-25");
        assert_eq!(parse_search_date_bound("12h", now).unwrap(), "2026-05-08");
        assert_eq!(parse_search_date_bound("30m", now).unwrap(), "2026-05-08");
        assert_eq!(parse_search_date_bound("1mo", now).unwrap(), "2026-04-09");
        assert_eq!(parse_search_date_bound("1y", now).unwrap(), "2025-05-09");
    }

    #[test]
    fn date_bound_iso_date_and_datetime() {
        let now = 1_778_284_800_000;
        assert_eq!(parse_search_date_bound("2024-01-31", now).unwrap(), "2024-01-31");
        assert_eq!(
            parse_search_date_bound("2024-01-31T10:11:12.345Z", now).unwrap(),
            "2024-01-31T10:11:12Z"
        );
        assert_eq!(
            parse_search_date_bound(" 2024-01-31T10:11:12+09:00 ", now).unwrap(),
            "2024-01-31T10:11:12+09:00"
        );
        assert!(parse_search_date_bound("", now).is_err());
        assert!(parse_search_date_bound("yesterday", now).is_err());
        assert!(parse_search_date_bound("3 days", now).is_err());
    }

    #[test]
    fn date_qualifier_forms() {
        let now = 1_778_284_800_000;
        assert_eq!(
            build_search_date_qualifier("created", Some("2024-01-01"), Some("2024-02-01"), now)
                .unwrap(),
            Some("created:2024-01-01..2024-02-01".to_string())
        );
        assert_eq!(
            build_search_date_qualifier("created", Some("2024-01-01"), None, now).unwrap(),
            Some("created:>=2024-01-01".to_string())
        );
        assert_eq!(
            build_search_date_qualifier("pushed", None, Some("2024-02-01"), now).unwrap(),
            Some("pushed:<=2024-02-01".to_string())
        );
        assert_eq!(
            build_search_date_qualifier("created", None, None, now).unwrap(),
            None
        );
    }

    #[test]
    fn date_field_mapping_matches_ts() {
        assert_eq!(resolve_search_date_field("commits", None), "committer-date");
        assert_eq!(
            resolve_search_date_field("commits", Some("updated")),
            "committer-date"
        );
        assert_eq!(resolve_search_date_field("repos", Some("updated")), "pushed");
        assert_eq!(resolve_search_date_field("repos", Some("created")), "created");
        assert_eq!(resolve_search_date_field("issues", None), "created");
        assert_eq!(resolve_search_date_field("prs", Some("updated")), "updated");
    }

    #[test]
    fn query_composition_requires_a_term() {
        assert_eq!(
            compose_search_query(&[Some("fix bug"), Some("created:>=2024-01-01")]).unwrap(),
            "fix bug created:>=2024-01-01"
        );
        assert_eq!(
            compose_search_query(&[Some("  "), None, Some("is:issue")]).unwrap(),
            "is:issue"
        );
        assert!(compose_search_query(&[None, Some(" ")]).is_err());
    }

    #[test]
    fn search_argv_shapes() {
        assert_eq!(
            build_gh_api_search_args("issues", "fix is:issue", 10, None, &[]),
            vec![
                "api",
                "-X",
                "GET",
                "/search/issues",
                "-f",
                "q=fix is:issue",
                "-F",
                "per_page=10"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        let with_host = build_gh_api_search_args(
            "code",
            "needle",
            50,
            Some("ghe.example.com"),
            &["Accept: application/vnd.github.text-match+json"],
        );
        assert_eq!(
            &with_host[..5],
            ["api", "--hostname", "ghe.example.com", "-X", "GET"]
        );
        assert_eq!(with_host[5], "/search/code");
        assert_eq!(
            &with_host[with_host.len() - 2..],
            [
                "-H".to_string(),
                "Accept: application/vnd.github.text-match+json".to_string()
            ]
        );
    }

    #[test]
    fn repo_scope_qualifier_suppresses_default_repo() {
        assert!(query_carries_repo_scope(Some("bug repo:other/project")));
        assert!(query_carries_repo_scope(Some("org:rust-lang async")));
        assert!(query_carries_repo_scope(Some("-user:someone")));
        assert!(!query_carries_repo_scope(Some("repository setup")));
        assert!(!query_carries_repo_scope(None));
    }

    #[test]
    fn search_scope_splits_host_from_qualifier() {
        assert_eq!(
            search_scope(Some("github.com/owner/repo")),
            (Some("repo:owner/repo".to_string()), Some("github.com".to_string()))
        );
        assert_eq!(
            search_scope(Some("owner/repo")),
            (Some("repo:owner/repo".to_string()), None)
        );
        assert_eq!(search_scope(None), (None, None));
    }

    #[test]
    fn repo_ref_parsing_and_host_args() {
        let reference = parse_repo_ref("ghe.example.com/owner/repo");
        assert_eq!(reference.host.as_deref(), Some("ghe.example.com"));
        assert_eq!(reference.slug, "owner/repo");
        assert_eq!(
            gh_api_host_args(&reference),
            vec!["--hostname".to_string(), "ghe.example.com".to_string()]
        );
        let bare = parse_repo_ref("owner/repo");
        assert!(bare.host.is_none());
        assert!(gh_api_host_args(&bare).is_empty());
        assert_eq!(format_repo_ref(Some("ghe.example.com"), "owner/repo"), "ghe.example.com/owner/repo");
        assert_eq!(format_repo_ref(None, "owner/repo"), "owner/repo");
    }

    #[test]
    fn repo_url_drops_only_the_default_host() {
        let default_host = cache::default_gh_host();
        assert_eq!(
            repo_from_url(Some(&format!("https://{default_host}/owner/repo"))).unwrap(),
            "owner/repo"
        );
        assert_eq!(
            repo_from_url(Some("https://ghe.example.com/owner/repo")).unwrap(),
            "ghe.example.com/owner/repo"
        );
        assert_eq!(repo_from_url(Some("not a url")), None);
        assert_eq!(repo_from_url(None), None);
    }

    #[test]
    fn append_repo_flag_skips_url_identifiers() {
        let mut args = vec!["issue".to_string(), "view".to_string()];
        append_repo_flag(&mut args, Some("owner/repo"), Some("12"));
        assert_eq!(args, vec!["issue", "view", "--repo", "owner/repo"]);

        let mut url_args = vec!["issue".to_string(), "view".to_string()];
        append_repo_flag(
            &mut url_args,
            Some("owner/repo"),
            Some("https://github.com/owner/repo/issues/12"),
        );
        assert_eq!(url_args, vec!["issue", "view"]);

        let mut bare = vec!["repo".to_string(), "view".to_string()];
        append_repo_flag(&mut bare, None, None);
        assert_eq!(bare, vec!["repo", "view"]);
    }

    #[test]
    fn positive_int_and_run_id_parsing() {
        assert_eq!(parse_positive_decimal_int(Some("42")), Some(42));
        assert_eq!(parse_positive_decimal_int(Some("0")), None);
        assert_eq!(parse_positive_decimal_int(Some("1e2")), None);
        assert_eq!(parse_positive_decimal_int(Some("0x10")), None);
        assert_eq!(parse_positive_decimal_int(Some("12.0")), None);
        assert_eq!(parse_positive_decimal_int(Some("+12")), None);
        assert_eq!(parse_positive_decimal_int(Some("")), None);
        assert_eq!(parse_positive_decimal_int(None), None);

        assert_eq!(parse_run_id(Some("12345")), Some(12345));
        assert_eq!(
            parse_run_id(Some("https://github.com/owner/repo/actions/runs/999")),
            Some(999)
        );
        assert_eq!(parse_run_id(Some("https://github.com/owner/repo")), None);
    }

    #[test]
    fn pull_request_and_issue_url_parsing() {
        assert_eq!(
            parse_pull_request_url(Some("https://github.com/owner/repo/pull/17")),
            (Some("github.com/owner/repo".to_string()), Some(17))
        );
        assert_eq!(
            parse_pull_request_url(Some("https://github.com/owner/repo/pull/17/files")),
            (Some("github.com/owner/repo".to_string()), Some(17))
        );
        assert_eq!(parse_pull_request_url(Some("17")), (None, None));
        assert_eq!(
            parse_issue_url(Some("https://ghe.example.com/owner/repo/issues/3#issuecomment-1")),
            (Some("ghe.example.com/owner/repo".to_string()), Some(3))
        );
        assert_eq!(parse_issue_url(None), (None, None));
    }

    #[test]
    fn pr_checkout_branch_derives_pr_number() {
        assert_eq!(
            pr_checkout_branch(Some(&PrArg::One("42".to_string()))),
            Some("pr-42".to_string())
        );
        assert_eq!(
            pr_checkout_branch(Some(&PrArg::One("#7".to_string()))),
            Some("pr-7".to_string())
        );
        assert_eq!(
            pr_checkout_branch(Some(&PrArg::One(
                "https://github.com/owner/repo/pull/9".to_string()
            ))),
            Some("pr-9".to_string())
        );
        assert_eq!(
            pr_checkout_branch(Some(&PrArg::One("feature/x".to_string()))),
            None
        );
        assert_eq!(pr_checkout_branch(None), None);
    }

    #[test]
    fn repo_refs_compare_by_effective_host() {
        assert!(repo_refs_equal("owner/repo", "github.com/Owner/Repo"));
        assert!(!repo_refs_equal("owner/repo", "ghe.example.com/owner/repo"));
        assert!(repo_refs_equal(
            "ghe.example.com/owner/repo",
            "ghe.example.com/Owner/Repo"
        ));
    }

    #[test]
    fn text_helpers_normalize_like_ts() {
        assert_eq!(normalize_optional_string(Some("  x  ")), Some("x".to_string()));
        assert_eq!(normalize_optional_string(Some("   ")), None);
        assert_eq!(normalize_text(Some("a\r\nb\tc\r")), "a\nb    c");
        assert_eq!(normalize_block(Some("a\r\n\tb\n\n")), "a\n    b");
        assert!(require_non_empty(Some("  "), "path").is_err());
        assert_eq!(require_non_empty(Some(" p "), "path").unwrap(), "p");
    }

    #[test]
    fn byte_and_sha_formatting() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_short_sha(Some("abcdef1234567")), Some("abcdef1".to_string()));
        assert_eq!(format_short_sha(Some("")), None);
        assert_eq!(format_short_sha(None), None);
    }

    #[test]
    fn author_and_label_formatting() {
        assert_eq!(
            format_author(&json!({"login": "octocat"})),
            Some("@octocat".to_string())
        );
        assert_eq!(
            format_author(&json!({"login": "", "name": "Octo Cat"})),
            Some("Octo Cat".to_string())
        );
        assert_eq!(format_author(&json!({})), None);
        assert_eq!(
            format_labels(Some(&json!([{"name": "bug"}, {"name": ""}, {"other": 1}]))),
            Some("bug".to_string())
        );
        assert_eq!(format_labels(Some(&json!([]))), None);
        assert_eq!(format_labels(None), None);
    }

    #[test]
    fn repo_view_renders_ts_layout() {
        let data = json!({
            "nameWithOwner": "owner/repo",
            "description": "A repo\nwith newline",
            "url": "https://github.com/owner/repo",
            "defaultBranchRef": {"name": "main"},
            "homepageUrl": null,
            "forkCount": 4,
            "isArchived": false,
            "isFork": true,
            "primaryLanguage": {"name": "Rust"},
            "repositoryTopics": [{"name": "cli"}, {"topic": {"name": "tools"}}],
            "stargazerCount": 12,
            "updatedAt": "2026-01-01T00:00:00Z",
            "viewerPermission": "ADMIN",
            "visibility": "PUBLIC"
        });
        let text = format_repo_view(&data, Some("owner/repo"), Some("dev"));
        assert!(text.starts_with("# owner/repo\n\nA repo\nwith newline\n\n"));
        assert!(text.contains("URL: https://github.com/owner/repo"));
        assert!(text.contains("Default branch: main"));
        assert!(text.contains("Branch: dev"));
        assert!(text.contains("Primary language: Rust"));
        assert!(text.contains("Stars: 12"));
        assert!(text.contains("Topics: cli, tools"));
        assert!(text.contains("Fork: true"));
        assert!(!text.contains("Homepage"));
    }

    #[test]
    fn issue_view_renders_sections_and_hides_minimized_comments() {
        let data = json!({
            "number": 3,
            "title": "Bug",
            "state": "OPEN",
            "stateReason": null,
            "author": {"login": "octocat"},
            "createdAt": "2026-01-01T00:00:00Z",
            "labels": [{"name": "bug"}],
            "url": "https://github.com/owner/repo/issues/3",
            "body": "Body text",
            "comments": [
                {"author": {"login": "a"}, "body": "hi", "url": "https://x/1"},
                {"author": {"login": "b"}, "body": "hidden", "isMinimized": true}
            ]
        });
        let text = format_issue_view(&data, "3", true);
        assert!(text.starts_with("# Issue #3: Bug"));
        assert!(text.contains("Author: @octocat"));
        assert!(text.contains("## Body\n\nBody text"));
        assert!(text.contains("## Comments (1)"));
        assert!(text.contains("Minimized comments omitted: 1."));
        assert!(!text.contains("hidden"));
        let without = format_issue_view(&data, "3", false);
        assert!(!without.contains("## Comments"));
    }

    #[test]
    fn pr_view_elides_files_past_the_preview_limit() {
        let files: Vec<Value> = (0..(FILE_PREVIEW_LIMIT + 3))
            .map(|i| json!({"path": format!("src/f{i}.rs"), "additions": 1, "deletions": 2}))
            .collect();
        let data = json!({
            "number": 5,
            "title": "Feature",
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefName": "feat",
            "url": "https://github.com/owner/repo/pull/5",
            "files": files
        });
        let text = format_pr_view(&data, Some("5"), false);
        assert!(text.starts_with("# Pull Request #5: Feature"));
        assert!(text.contains(&format!("## Files ({})", FILE_PREVIEW_LIMIT + 3)));
        assert!(text.contains("- src/f0.rs [CHANGED] (+1 -2)"));
        assert!(text.contains("[…3 files elided…]"));
        assert!(!text.contains("## Reviews"));
    }

    #[test]
    fn search_results_render_empty_and_populated() {
        let empty = format_search_results("issues", "q", None, &[]);
        assert!(empty.contains("No issues found."));
        let items = vec![api_issue_to_search_result(&json!({
            "number": 1,
            "title": "T",
            "state": "open",
            "user": {"login": "u"},
            "labels": [{"name": "bug"}],
            "repository_url": "https://api.github.com/repos/owner/repo",
            "html_url": "https://github.com/owner/repo/issues/1"
        }))];
        let text = format_search_results("issues", "q", Some("owner/repo"), &items);
        assert!(text.contains("Repository: owner/repo"));
        assert!(text.contains("Results: 1"));
        assert!(text.contains("- #1 T"));
        assert!(text.contains("  Repo: owner/repo"));
        assert!(text.contains("  Author: @u"));
    }

    #[test]
    fn merged_pull_requests_project_as_merged_state() {
        let merged = api_issue_to_search_result(&json!({
            "state": "closed",
            "pull_request": {"merged_at": "2026-01-01T00:00:00Z"}
        }));
        assert_eq!(merged["state"], "merged");
        let closed = api_issue_to_search_result(&json!({
            "state": "closed",
            "pull_request": {"merged_at": null}
        }));
        assert_eq!(closed["state"], "closed");
    }

    #[test]
    fn code_and_commit_and_repo_projections() {
        let code = api_code_to_search_result(&json!({
            "path": "src/main.rs",
            "sha": "abcdef1234567",
            "html_url": "https://github.com/owner/repo/blob/x/src/main.rs",
            "repository": {"full_name": "owner/repo"},
            "text_matches": [{"fragment": "needle\nsecond", "property": "content"}]
        }));
        assert_eq!(code["path"], "src/main.rs");
        assert_eq!(code["repository"]["nameWithOwner"], "owner/repo");
        assert_eq!(code["textMatches"][0]["property"], "content");

        let commit = api_commit_to_search_result(&json!({
            "sha": "abcdef1234567",
            "node_id": "n1",
            "html_url": "https://x",
            "author": {"login": "u"},
            "commit": {"message": "subject\nbody", "author": {"name": "A", "date": "d"}}
        }));
        assert_eq!(commit["id"], "n1");
        assert_eq!(commit["commit"]["author"]["name"], "A");

        let repo = api_repo_to_search_result(&json!({
            "full_name": "owner/repo",
            "stargazers_count": 3,
            "forks_count": 1,
            "open_issues_count": 2,
            "archived": false,
            "fork": true,
            "visibility": "public"
        }));
        assert_eq!(repo["fullName"], "owner/repo");
        assert_eq!(repo["stargazersCount"], 3);
        assert_eq!(repo["visibility"], "public");
    }

    #[test]
    fn search_commits_formatting_uses_first_message_line() {
        let items = vec![api_commit_to_search_result(&json!({
            "sha": "abcdef1234567",
            "html_url": "https://x",
            "author": {"login": "u"},
            "commit": {"message": "subject line\n\nbody text", "author": {"date": "2026-01-01"}}
        }))];
        let text = format_search_commits_results("q", None, &items);
        assert!(text.contains("- abcdef1 subject line"));
        assert!(!text.contains("body text"));
    }

    #[test]
    fn repos_search_formatting_prints_counts() {
        let items = vec![api_repo_to_search_result(&json!({
            "full_name": "owner/repo",
            "description": "desc\nsecond",
            "stargazers_count": 9,
            "forks_count": 2,
            "open_issues_count": 1,
            "archived": true,
            "fork": false,
            "visibility": "public"
        }))];
        let text = format_search_repos_results("q", &items);
        assert!(text.contains("- owner/repo"));
        assert!(text.contains("  Description: desc"));
        assert!(text.contains("  Stars: 9"));
        assert!(text.contains("  Archived: true"));
    }

    #[test]
    fn json_field_dropping_rewrites_only_the_json_list() {
        let args = vec![
            "issue".to_string(),
            "view".to_string(),
            "--json".to_string(),
            "author,stateReason,title".to_string(),
        ];
        assert_eq!(
            drop_json_field(&args, "stateReason").unwrap(),
            vec!["issue", "view", "--json", "author,title"]
        );
        assert!(drop_json_field(&args, "missing").is_none());
        let no_json = vec!["issue".to_string(), "view".to_string()];
        assert!(drop_json_field(&no_json, "stateReason").is_none());
    }

    #[test]
    fn binary_and_image_sniffing() {
        assert!(is_probably_binary(&[0x00, 0x01, 0x02, 0x03]));
        assert!(!is_probably_binary(b"plain text\n"));
        assert!(!is_probably_binary(b""));
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]), Some("image/png"));
        assert_eq!(sniff_image_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"RIFF1234WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image_mime(b"hello"), None);
    }

    #[test]
    fn urlencode_segment_escapes_reserved_bytes() {
        assert_eq!(urlencode_segment("src/main.rs"), "src/main.rs");
        assert_eq!(urlencode_segment("a b"), "a%20b");
        assert_eq!(urlencode_segment("a+b&c"), "a%2Bb%26c");
    }

    #[test]
    fn remote_names_are_sanitized() {
        assert_eq!(sanitize_remote_name("Octo Cat"), "fork-octo-cat");
        assert_eq!(sanitize_remote_name("///"), "fork");
        assert_eq!(sanitize_remote_name("owner"), "fork-owner");
    }

    #[test]
    fn tail_lines_keeps_the_last_n() {
        let text = (1..=5)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(tail_lines(&text, 2), "4\n5");
        assert_eq!(tail_lines(&text, 99), text);
        assert_eq!(tail_lines(&text, 0), "");
    }

    #[test]
    fn sections_join_with_a_separator() {
        assert_eq!(
            join_sections(&["a".to_string(), "b".to_string()]),
            "a\n\n---\n\nb"
        );
    }

    #[test]
    fn iso_date_roundtrips_known_instants() {
        assert_eq!(format_iso_date(0), "1970-01-01");
        assert_eq!(format_iso_date(1_778_284_800_000), "2026-05-09");
        assert_eq!(format_iso_date(1_704_067_200_000), "2024-01-01");
    }

    #[test]
    fn job_state_helpers_classify_conclusions() {
        assert!(is_terminal_status("completed"));
        assert!(!is_terminal_status("in_progress"));
        for conclusion in ["failure", "cancelled", "timed_out", "action_required", "startup_failure"] {
            assert!(is_failed_conclusion(conclusion), "{conclusion}");
        }
        assert!(!is_failed_conclusion("success"));
        assert!(!is_failed_conclusion(""));
    }

    #[test]
    fn worktree_dir_honors_the_override() {
        // `OMP_WORKTREE_DIR` is process-global; assert the shape only.
        let dir = worktree_dir("42-deadbeef");
        assert!(dir.ends_with("42-deadbeef"));
    }

    #[test]
    fn description_is_the_ts_prompt() {
        assert!(DESCRIPTION.starts_with("`gh` op wrapper: repos/files, PRs, search, checkout, push, Actions watch."));
        assert!(DESCRIPTION.contains("<critical>\nGitHub-hosted repository file: MUST use `file_read`; NEVER `curl`/`wget`.\n</critical>"));
        assert_eq!(SUMMARY, "Interact with GitHub repositories, files, pull requests, and Actions");
    }

    #[test]
    fn constants_match_ts() {
        assert_eq!(GH_COMMAND_TIMEOUT_MS, 300_000);
        assert_eq!(OUTPUT_LIMIT_BYTES, 8 * 1024 * 1024);
        assert_eq!(SEARCH_LIMIT_DEFAULT, 10);
        assert_eq!(SEARCH_LIMIT_MAX, 50);
        assert_eq!(FILE_PREVIEW_LIMIT, 50);
        assert_eq!(REVIEW_COMMENTS_PAGE_SIZE, 100);
        assert_eq!(GITHUB_HOST, "github.com");
        assert_eq!(GH_REPO_FIELDS.len(), 14);
        assert_eq!(GH_ISSUE_FIELDS.len(), 11);
        assert_eq!(GH_ISSUE_FIELDS_NO_COMMENTS.len(), 10);
        assert_eq!(GH_PR_FIELDS.len(), 17);
        assert_eq!(GH_PR_FIELDS_NO_COMMENTS.len(), 16);
        assert_eq!(GH_PR_CREATE_FIELDS.len(), 10);
        assert_eq!(GH_PR_CHECKOUT_FIELDS.len(), 10);
        assert_eq!(GH_REPO_CLONE_FIELDS.len(), 3);
    }
}
