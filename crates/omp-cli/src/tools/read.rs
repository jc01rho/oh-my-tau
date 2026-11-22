//! `read`: one `path` parameter with inline selectors (`:50-200`, `:-60`,
//! `:raw`, `a-b,c-d`). Text files render as a `[path#TAG]` hashline snapshot
//! header followed by `N:line` rows (omp `read.ts`, hashline display mode);
//! directories render as omp's workspace tree.
//!
//! Dispatch: `proc://` and the internal URL registry first, then HTTP(S)/`www.`
//! URLs through [`super::read_fetch`], then the non-plain-text shapes through
//! [`super::read_formats`] (SQLite selectors, archives, JSON `?q=`, `.ipynb`,
//! PDF, binary metadata), and only then the plain text/image pipeline.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{LazyLock, OnceLock};
use std::time::SystemTime;

use omp_urls::spec::InternalResource;
use omp_urls::{Registry, UrlCtx, UrlTarget};

use anyhow::{bail, Context, Result};
use pa_agent::types::AgentToolResult;
use pi_edit::path_policy::canonical_key;
use pi_edit::store::seen_lines_from_body;
use pi_edit::text::{normalize_to_lf, strip_bom};
use regex::Regex;

use super::util::{
    format_age, format_bytes, format_path_relative_to_cwd, notice_with_artifact, resolve_to_cwd,
    save_artifact, shorten_path, truncate_head, SpillConfig, DEFAULT_MAX_BYTES,
};
use super::Env;

/// `read.defaultLimit` schema default.
const DEFAULT_LIMIT: usize = 300;
const MAX_LINES: usize = 3000;

/// Live `read.defaultLimit`, clamped to `[1, MAX_LINES]` (TS `#defaultLimit`);
/// [`DEFAULT_LIMIT`] when unset or not a finite number.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // clamped to 1..=3000 before the cast
fn default_limit(cwd: &Path) -> usize {
    omp_settings::Settings::load(cwd)
        .get_number("read.defaultLimit")
        .filter(|v| v.is_finite())
        .map_or(DEFAULT_LIMIT, |v| v.clamp(1.0, MAX_LINES as f64) as usize)
}
const LEADING_CONTEXT: usize = 1;
const TRAILING_CONTEXT: usize = 3;
const MAX_COLUMNS: usize = 768;
const DIR_MAX_DEPTH: usize = 2;
const DIR_CHILD_LIMIT: usize = 12;

/// Bytes sampled for format sniffing (magic bytes + text/binary sample).
const FORMAT_HEAD_BYTES: usize = 8192;
/// omp `MAX_SUMMARY_BYTES`: larger files skip the structural summary.
const MAX_SUMMARY_BYTES: usize = 2 * 1024 * 1024;
/// omp `MAX_SUMMARY_LINES`.
const MAX_SUMMARY_LINES: usize = 20_000;
/// `read.summarize.minTotalLines` schema default.
const DEFAULT_MIN_TOTAL_LINES: u32 = 100;
/// `read.summarize.unfoldUntil` schema default.
const DEFAULT_UNFOLD_UNTIL: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    None,
    Raw,
    Lines { ranges: Vec<LineRange>, raw: bool },
    Tail { count: usize, raw: bool },
}

static RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^L?(\d+)(?:(\.\.|[-+])L?(\d+)?)?$").expect("valid line range regex")
});
static TAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-(\d+)$").expect("valid tail regex"));

fn parse_chunk(chunk: &str, pin_bare: bool) -> Result<Option<LineRange>> {
    let Some(c) = RANGE_RE.captures(chunk) else {
        return Ok(None);
    };
    let start: usize = c[1].parse().unwrap_or(usize::MAX);
    if start < 1 {
        bail!("Line selector 0 is invalid; lines are 1-indexed. Use :1.");
    }
    let sep = c
        .get(2)
        .map(|m| if m.as_str() == ".." { "-" } else { m.as_str() });
    let rhs: Option<usize> = c.get(3).and_then(|m| m.as_str().parse().ok());
    let end = match sep {
        Some("+") => match rhs {
            // Checked math: `start` may be `usize::MAX` (parse-overflow
            // fallback) and TS's f64 grammar has no overflow — saturating
            // keeps the same benign empty-window behavior instead of
            // panicking (debug) or wrapping (release).
            Some(n) if n >= 1 => Some(start.saturating_add(n - 1)),
            _ => bail!(
                "Invalid range {start}+{}: count must be >= 1.",
                rhs.unwrap_or(0)
            ),
        },
        Some(_) => match rhs {
            Some(e) if e < start => bail!("Invalid range {start}-{e}: end must be >= start."),
            other => other,
        },
        None => pin_bare.then_some(start),
    };
    Ok(Some(LineRange { start, end }))
}

/// omp `parseLineRanges`: comma list, sorted and merged.
fn parse_ranges(sel: &str) -> Result<Option<Vec<LineRange>>> {
    let chunks: Vec<&str> = sel.split(',').collect();
    let pin = chunks.len() > 1;
    let mut parsed = Vec::new();
    for ch in chunks {
        match parse_chunk(ch, pin)? {
            Some(r) => parsed.push(r),
            None => return Ok(None),
        }
    }
    parsed.sort_by_key(|r| r.start);
    let mut merged: Vec<LineRange> = Vec::new();
    for r in parsed {
        match merged.last_mut() {
            None => merged.push(r),
            Some(last) => {
                let Some(last_end) = last.end else { continue };
                if r.start <= last_end.saturating_add(1) {
                    if r.end.is_none_or(|e| e > last_end) {
                        last.end = r.end;
                    }
                } else {
                    merged.push(r);
                }
            }
        }
    }
    Ok(Some(merged))
}

fn parse_range_or_tail(chunk: &str, raw: bool) -> Result<Option<Selector>> {
    if let Some(ranges) = parse_ranges(chunk)? {
        return Ok(Some(Selector::Lines { ranges, raw }));
    }
    if let Some(c) = TAIL_RE.captures(chunk) {
        let count: usize = c[1].parse().unwrap_or(0);
        if count < 1 {
            bail!("Tail selector -0 is invalid; use :-N with N >= 1 to read the last N lines.");
        }
        return Ok(Some(Selector::Tail { count, raw }));
    }
    Ok(None)
}

/// omp `parseSel` for the selector kinds this port serves.
///
/// # Errors
/// Invalid range bounds or a recognised-but-malformed compound selector.
pub fn parse_sel(sel: &str) -> Result<Selector> {
    if sel.is_empty() {
        return Ok(Selector::None);
    }
    let invalid = || {
        anyhow::anyhow!(
            "Invalid selector ':{sel}'. Use :N, :N-M, :N+K, :N- (open-ended), :-N (last N lines), a comma-separated list of ranges, :raw, or a range combined with raw (e.g. :raw:50-100)."
        )
    };
    let looks_read_like = |c: &str| {
        let l = c.to_lowercase();
        l == "raw"
            || l == "conflicts"
            || l == "img"
            || TAIL_RE.is_match(c)
            || matches!(parse_ranges(c), Ok(Some(_)))
    };
    if sel.contains(':') {
        let chunks: Vec<&str> = sel.split(':').collect();
        if let [a, b] = chunks[..] {
            let a_raw = a.eq_ignore_ascii_case("raw");
            let b_raw = b.eq_ignore_ascii_case("raw");
            let range_chunk = if a_raw {
                Some(b)
            } else if b_raw {
                Some(a)
            } else {
                None
            };
            if let Some(rc) = range_chunk {
                if let Some(parsed) = parse_range_or_tail(rc, true)? {
                    return Ok(parsed);
                }
            }
        }
        if chunks.iter().all(|c| looks_read_like(c)) {
            return Err(invalid());
        }
        return Ok(Selector::None);
    }
    if sel.eq_ignore_ascii_case("raw") {
        return Ok(Selector::Raw);
    }
    if sel.eq_ignore_ascii_case("conflicts") || sel.eq_ignore_ascii_case("img") {
        bail!("Selector ':{sel}' is not supported by this build; use :N-M, :-N or :raw.");
    }
    Ok(parse_range_or_tail(sel, false)?.unwrap_or(Selector::None))
}

fn is_raw(sel: &Selector) -> bool {
    matches!(
        sel,
        Selector::Raw | Selector::Lines { raw: true, .. } | Selector::Tail { raw: true, .. }
    )
}

/// Split `raw` into a filesystem path (existing) plus selector. A literal path
/// that exists always wins (omp `splitPathAndSelPreferringLiteral`).
fn split_path_and_sel(raw: &str, cwd: &Path) -> Result<(String, Selector)> {
    if resolve_to_cwd(raw, cwd).exists() {
        return Ok((raw.to_string(), Selector::None));
    }
    let chunks: Vec<&str> = raw.split(':').collect();
    for take in 1..=2usize.min(chunks.len().saturating_sub(1)) {
        let path = chunks[..chunks.len() - take].join(":");
        let sel = chunks[chunks.len() - take..].join(":");
        if path.is_empty() {
            continue;
        }
        let parsed = parse_sel(&sel)?;
        if parsed != Selector::None && resolve_to_cwd(&path, cwd).exists() {
            return Ok((path, parsed));
        }
    }
    Ok((raw.to_string(), Selector::None))
}

/// Lines addressable by hashline anchors (omp `splitAddressableFileLines`).
fn addressable_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn truncate_columns(line: &str) -> String {
    let mut chars = line.char_indices();
    match chars.nth(MAX_COLUMNS) {
        Some((idx, _)) => format!("{}…", &line[..idx]),
        None => line.to_string(),
    }
}

/// Run the `read` tool.
///
/// # Errors
/// Missing path, unreadable file, binary content or invalid selector.
// Layout mirrors omp's `read` flow top-to-bottom; splitting it would obscure that parity.
#[allow(clippy::too_many_lines)]
pub fn read(env: &Env, args: &serde_json::Value) -> Result<AgentToolResult> {
    let raw_path = super::util::str_arg(args, "path")?;
    if let Some(rest) = raw_path.strip_prefix("proc://") {
        return super::jobs::read_proc(env, rest);
    }
    if let Some(result) = read_internal_url(env, raw_path)? {
        return Ok(result);
    }
    // omp `read.ts` fetches HTTP(S)/`www.` targets instead of opening them.
    if super::read_fetch::is_fetch_url(raw_path) {
        return read_fetch_url(raw_path);
    }
    // Shapes whose target is not a plain path (`db.sqlite:table`, `a.zip:memb`,
    // `file.json?q=…`) peel before selector splitting would eat their suffix.
    if let Some(result) = read_peeled_format(env, raw_path)? {
        return Ok(result);
    }
    let (path_part, sel) = split_path_and_sel(raw_path, &env.cwd)?;
    let absolute = resolve_to_cwd(&path_part, &env.cwd);
    let meta = std::fs::metadata(&absolute)
        .map_err(|_| anyhow::anyhow!("Path '{path_part}' not found"))?;
    if meta.is_dir() {
        return read_directory(&absolute, &sel);
    }
    if !meta.is_file() {
        bail!("Cannot read '{path_part}': it is a special file, not a regular file or directory.");
    }
    if matches!(sel, Selector::None) && crate::images::looks_like_image(&absolute) {
        let (image, note) = crate::images::read_image_detailed(
            &absolute,
            &crate::images::ImageResizeOptions::default(),
        )
        .map_err(|e| anyhow::anyhow!("Cannot read '{path_part}': {e}"))?;
        let mut text = format!(
            "Read image file [{}].",
            format_path_relative_to_cwd(&absolute, &env.cwd, false)
        );
        if let Some(note) = note {
            text.push('\n');
            text.push_str(&note);
        }
        return Ok(AgentToolResult {
            content: vec![
                pa_agent::types::ToolResultContent::Text(pa_agent::types::TextContent {
                    text,
                    text_signature: None,
                }),
                pa_agent::types::ToolResultContent::Image(image),
            ],
            details: serde_json::Value::Null,
            terminate: None,
            is_error: None,
        });
    }
    // Non-plain-text formats (SQLite, archives, notebooks, PDF) render through
    // `read_formats` before the text pipeline sees their bytes.
    let head = read_head(&absolute, FORMAT_HEAD_BYTES);
    let format = super::read_formats::detect_format(&absolute, &head);
    if let Some(result) = read_detected_format(env, &absolute, &path_part, format, &sel)? {
        return Ok(result);
    }
    let bytes = std::fs::read(&absolute).with_context(|| format!("cannot read {path_part}"))?;
    if bytes.contains(&0) {
        // Binary content: on-disk metadata/strings/hexdump (omp `read-binary`)
        // instead of a bare "binary file" error. `:raw` still refuses to shape
        // the bytes, keeping that escape hatch byte-verbatim.
        if is_raw(&sel) {
            bail!(
                "Cannot read '{path_part}': binary file ({}).",
                format_bytes(bytes.len())
            );
        }
        let display = display_path(&absolute, &env.cwd);
        let text = super::read_formats::binary_read(&display, &bytes, "")?;
        return format_converted(env, &text, &path_part, &display, &sel);
    }
    let text = String::from_utf8_lossy(&bytes);
    let (_, without_bom) = strip_bom(&text);
    let normalized = normalize_to_lf(without_bom);
    let selector_base = path_part;
    let display = display_path(&absolute, &env.cwd);
    // `read.summarize.*`: a pi-ast structural summary may replace a whole-file
    // code read (opt-in; see `summarize_enabled`).
    if let Some(summary) = maybe_summarize(env, &absolute, &normalized, &display, &sel) {
        return Ok(summary);
    }
    let result = format_read_output(
        env,
        &normalized,
        &sel,
        &selector_base,
        &display,
        env.hashline,
        Some(&absolute),
    )?;
    let result = with_block_context(env, result, &normalized, &absolute, &sel);
    Ok(relevance_spill(env, result, save_artifact))
}

/// A file read past `tools.artifactSpillThreshold` with a known request
/// (`env.query`): the full text goes to an artifact (via `save`) and the
/// inline view keeps head 1/9 + tail 2/9 of the
/// `artifactHeadBytes + artifactTailBytes` budget plus the BM25-best chunks
/// (see [`super::relevance`]). `OMP_TRUNC_BM25=0`, an empty request or a
/// short read leave the result unchanged.
fn relevance_spill(
    env: &Env,
    mut result: AgentToolResult,
    save: impl Fn(&str) -> Option<String>,
) -> AgentToolResult {
    use pa_agent::types::ToolResultContent;
    let [ToolResultContent::Text(block)] = result.content.as_mut_slice() else {
        return result;
    };
    if block.text.len() <= 1024 || !super::relevance::enabled() {
        return result;
    }
    let query = env.query();
    if query.trim().is_empty() {
        return result;
    }
    let cfg = SpillConfig::load(&env.cwd);
    if block.text.len() <= cfg.threshold {
        return result;
    }
    let budget = match cfg.head_bytes + cfg.tail_bytes {
        0 => cfg.threshold,
        b => b.min(cfg.threshold),
    };
    let hybrid = (budget / 9, budget * 2 / 9);
    let Some(p) = super::relevance::pick(&block.text, &query, budget, Some(hybrid), hybrid) else {
        return result;
    };
    let url = save(&block.text);
    block.text = format!(
        "{}\n\n[{}]",
        p.content,
        notice_with_artifact(&p.notice, url.as_deref())
    );
    result
}

/// Format already-in-memory text the way omp's read pipeline does: selector
/// windows, optional hashline header + numbered rows, continuation hints.
/// `store_key` is the snapshot key for hashline recording (`None` for URLs
/// with no local backing).
///
/// # Errors
/// Never fails; the Result mirrors the tool surface.
#[allow(clippy::too_many_lines)] // selector resolution + hashline snapshot recording; one flow, mirrors omp's formatReadOutput
#[allow(clippy::unnecessary_wraps)] // Result mirrors the tool surface (see doc above)
fn format_read_output(
    env: &Env,
    normalized: &str,
    sel: &Selector,
    selector_base: &str,
    display: &str,
    hashline: bool,
    store_key: Option<&Path>,
) -> Result<AgentToolResult> {
    let lines = addressable_lines(normalized);
    let total = lines.len();
    let raw = is_raw(sel);
    let hash_lines = !raw && hashline;

    // Resolve the selector against the line count.
    let sel = match sel {
        Selector::Tail { count, raw } => {
            let (count, raw) = (*count, *raw);
            let start = total.saturating_sub(count) + 1;
            Selector::Lines {
                ranges: vec![LineRange {
                    start: start.max(1),
                    end: Some(total.max(start)),
                }],
                raw,
            }
        }
        other => other.clone(),
    };
    let ranges: Vec<LineRange> = match &sel {
        Selector::Lines { ranges, .. } => ranges.clone(),
        _ => Vec::new(),
    };
    let mut body = String::new();
    let mut rows = 0usize;
    let mut last_shown = 0usize; // 1-indexed last displayed line
    let mut empty_window = false;
    let first_start = ranges.first().map_or(1, |r| r.start);
    let multi = ranges.len() > 1;
    let default_lines = default_limit(&env.cwd);
    let windows: Vec<(usize, usize)> = if ranges.is_empty() {
        vec![(0, default_lines)]
    } else {
        ranges
            .iter()
            .map(|r| {
                let requested_start = r.start - 1;
                // Context expansion only feeds hashline anchors (TS
                // `expandRangeWithContext`); raw/immutable reads show the
                // exact requested window.
                let lead = if hash_lines && !raw && r.start > 1 {
                    LEADING_CONTEXT.min(requested_start)
                } else {
                    0
                };
                let trail = if hash_lines && !raw && r.end.is_some() {
                    TRAILING_CONTEXT
                } else {
                    0
                };
                let limit = r.end.map_or(default_lines, |e| e - r.start + 1);
                (
                    requested_start - lead,
                    (limit + lead + trail).min(MAX_LINES),
                )
            })
            .collect()
    };
    let mut truncated_by_bytes = false;
    let max_bytes = if crate::methodology::raw_tool_output_requested() {
        usize::MAX
    } else {
        DEFAULT_MAX_BYTES.max(windows.iter().map(|w| w.1).max().unwrap_or(0) * 512)
    };
    for (wi, (start, count)) in windows.iter().enumerate() {
        if *start >= total {
            if wi == 0 {
                empty_window = true;
            }
            break;
        }
        let end = (start + count).min(total);
        for (i, line) in lines.iter().enumerate().take(end).skip(*start) {
            let shown = if raw {
                (*line).to_string()
            } else {
                truncate_columns(line)
            };
            let row = if hash_lines {
                format!("{}:{shown}", i + 1)
            } else {
                shown
            };
            if body.len() + row.len() + 1 > max_bytes && rows > 0 {
                truncated_by_bytes = true;
                break;
            }
            if rows > 0 {
                body.push('\n');
            }
            body.push_str(&row);
            rows += 1;
            last_shown = i + 1;
        }
        if truncated_by_bytes {
            break;
        }
    }

    if empty_window {
        let requested = first_start;
        let suggestion = if total == 0 {
            "The file is empty.".to_string()
        } else {
            format!("Use {selector_base}:1 to read from the start, or {selector_base}:{total} to read the last line.")
        };
        return Ok(AgentToolResult::text(format!(
            "Line {requested} is beyond end of file ({total} lines total). {suggestion}"
        )));
    }

    let mut out = String::new();
    if hash_lines && rows > 0 {
        if let Some(absolute) = store_key {
            let key = canonical_key(absolute);
            let tag = env.store.record(&key, normalized, None);
            let seen = seen_lines_from_body(&body);
            env.store.record_seen_lines(&key, &tag, &seen);
            let _ = writeln!(out, "[{display}#{tag}]");
        } else {
            let _ = writeln!(out, "[{display}]");
        }
    } else if raw && rows > 0 {
        // A raw read emits no header, but records the displayed range.
        if let Some(absolute) = store_key {
            let key = canonical_key(absolute);
            let first = windows.first().map_or(0, |w| w.0) + 1;
            let seen: Vec<u32> = (first..=last_shown)
                .filter_map(|n| u32::try_from(n).ok())
                .collect();
            env.store.record(&key, normalized, Some(&seen));
        }
    }
    out.push_str(&body);

    if truncated_by_bytes {
        let t = truncate_head(&body, max_bytes);
        let _ = write!(
            out,
            "\n\n[Showing lines {first_start}-{last_shown} of {total} ({} limit). Use {selector_base}:{} to continue]",
            format_bytes(t.max_bytes),
            last_shown + 1
        );
    } else if last_shown < total && !multi {
        let _ = write!(
            out,
            "\n\n[{} more lines in file. Use {selector_base}:{} to continue]",
            total - last_shown,
            last_shown + 1
        );
    }
    Ok(AgentToolResult::text(out))
}

// --------------------------------------------------------- format dispatch

/// Workspace-relative display path, shortened when it falls outside `cwd`.
fn display_path(absolute: &Path, cwd: &Path) -> String {
    let display = format_path_relative_to_cwd(absolute, cwd, false);
    if Path::new(&display).is_absolute() {
        shorten_path(&display)
    } else {
        display
    }
}

/// Leading bytes for format sniffing.
fn read_head(path: &Path, limit: usize) -> Vec<u8> {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut head = vec![0u8; limit];
    let mut filled = 0usize;
    while filled < limit {
        match file.read(&mut head[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => filled += read,
        }
    }
    head.truncate(filled);
    head
}

/// Peel a trailing `:raw` from a URL (`https://x/y:raw`), case-insensitively.
fn split_url_raw(raw: &str) -> (&str, bool) {
    let trimmed = raw.trim_end();
    let bytes = trimmed.as_bytes();
    if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b":raw") {
        return (&trimmed[..trimmed.len() - 4], true);
    }
    (trimmed, false)
}

/// Fetch an HTTP(S)/`www.` read through [`super::read_fetch`].
///
/// `read` runs on the blocking pool (`tools.rs`), so a current-thread runtime
/// is built only when no reactor is installed (unit tests), like `edit`'s
/// `agent://` delivery.
fn read_fetch_url(raw_path: &str) -> Result<AgentToolResult> {
    let (target, raw) = split_url_raw(raw_path);
    let options = super::read_fetch::FetchOptions {
        raw,
        ..super::read_fetch::FetchOptions::default()
    };
    let fetched = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(super::read_fetch::fetch_url(target, &options)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("read URL runtime")?
            .block_on(super::read_fetch::fetch_url(target, &options)),
    }?;
    Ok(AgentToolResult::text(
        super::read_fetch::render_url_read_output(&fetched, raw),
    ))
}

/// `db.sqlite[:table[:key]]` as `(file, sub-path)`, when the left side of a
/// `:` names an existing SQLite file.
fn sqlite_target(raw: &str, cwd: &Path) -> Option<(std::path::PathBuf, String)> {
    for (index, _) in raw.match_indices(':') {
        // `C:/dir` is a drive letter, not a sub-path separator.
        if index == 1 {
            continue;
        }
        let (path, rest) = raw.split_at(index);
        let absolute = resolve_to_cwd(path, cwd);
        if !absolute.is_file() {
            continue;
        }
        let head = read_head(&absolute, FORMAT_HEAD_BYTES);
        if super::read_formats::detect_format(&absolute, &head)
            == super::read_formats::Format::Sqlite
        {
            return Some((absolute, rest[1..].to_string()));
        }
    }
    None
}

/// Render already-converted (non-source) text in the read display: paging and
/// continuation hints, never hashline anchors.
fn format_converted(
    env: &Env,
    text: &str,
    selector_base: &str,
    display: &str,
    sel: &Selector,
) -> Result<AgentToolResult> {
    format_read_output(env, text, sel, selector_base, display, false, None)
}

/// Non-plain-text targets whose path is not a plain filesystem path: SQLite
/// selectors, JSON `?q=` queries and archive members. `Ok(None)` leaves the
/// input to the path/selector pipeline.
///
/// # Errors
/// A malformed SQLite/JSON/archive selector or a read failure.
fn read_peeled_format(env: &Env, raw_path: &str) -> Result<Option<AgentToolResult>> {
    use super::read_formats;

    if let Some((absolute, sub_path)) = sqlite_target(raw_path, &env.cwd) {
        let text = read_formats::sqlite_read(&absolute, &sub_path, "")?;
        let display = display_path(&absolute, &env.cwd);
        return format_converted(env, &text, raw_path, &display, &Selector::None).map(Some);
    }

    if let Some((path, query)) = read_formats::split_json_query_target(raw_path) {
        let absolute = resolve_to_cwd(&path, &env.cwd);
        if absolute.is_file() && !crate::images::looks_like_image(&absolute) {
            let head = read_head(&absolute, FORMAT_HEAD_BYTES);
            let display = display_path(&absolute, &env.cwd);
            // `db.sqlite?q=SELECT …` addresses rows; `?q=` on JSON is a filter.
            if read_formats::detect_format(&absolute, &head) == read_formats::Format::Sqlite {
                let text = read_formats::sqlite_read(&absolute, "", &query)?;
                return format_converted(env, &text, raw_path, &display, &Selector::None).map(Some);
            }
            if let Some(selector) = read_formats::parse_json_selector(&query) {
                let text = read_formats::json_read(&absolute, &selector)?;
                return format_converted(env, &text, raw_path, &display, &Selector::None).map(Some);
            }
        }
    }

    if let Some((archive, member)) = read_formats::split_archive_member(raw_path) {
        let absolute = resolve_to_cwd(&archive, &env.cwd);
        if absolute.is_file() {
            if let Some(kind) = read_formats::archive_kind_for_path(&archive) {
                let text = read_formats::archive_read(&absolute, kind, Some(&member))?;
                let display = display_path(&absolute, &env.cwd);
                return format_converted(env, &text, raw_path, &display, &Selector::None).map(Some);
            }
        }
    }

    Ok(None)
}

/// Render one detected non-plain-text format, or `None` for the formats the
/// text/image pipeline owns (`Format::Binary` is only reachable here for a
/// future sniffer; NUL-containing files are caught after the byte read).
///
/// # Errors
/// Format-specific read failures (corrupt archive, unsafe SQL, bad notebook).
fn read_detected_format(
    env: &Env,
    absolute: &Path,
    selector_base: &str,
    format: super::read_formats::Format,
    sel: &Selector,
) -> Result<Option<AgentToolResult>> {
    use super::read_formats::{self, ArchiveKind, Format};

    let display = display_path(absolute, &env.cwd);
    let rendered = match format {
        Format::Text | Format::Json | Format::JsonLines | Format::Image => return Ok(None),
        Format::Sqlite => read_formats::sqlite_read(absolute, "", "")?,
        Format::Zip | Format::Tar | Format::TarGz => {
            let kind = match format {
                Format::Zip => ArchiveKind::Zip,
                Format::Tar => ArchiveKind::Tar,
                _ => ArchiveKind::TarGz,
            };
            read_formats::archive_read(absolute, kind, None)?
        }
        Format::Notebook => {
            let text = std::fs::read_to_string(absolute)
                .with_context(|| format!("cannot read {selector_base}"))?;
            read_formats::format_notebook(&text)?
        }
        Format::Pdf => read_formats::pdf_read(
            &std::fs::read(absolute).with_context(|| format!("cannot read {selector_base}"))?,
        ),
        Format::Binary | Format::Gzip => {
            let bytes =
                std::fs::read(absolute).with_context(|| format!("cannot read {selector_base}"))?;
            read_formats::binary_read(&display, &bytes, "")?
        }
    };
    format_converted(env, &rendered, selector_base, &display, sel).map(Some)
}

// ------------------------------------------------------- pi-ast summaries

/// Whether `read.summarize.*` applies for this workspace: the setting is
/// registered with a TS default of `true`, but a summary replaces the file body
/// — and with it the hashline anchors a code edit copies from the read. So the
/// summary runs only when the user actually opts in (`enabled: true` in the
/// global/project/env layer, not the registry default); unset, defaulted or
/// `false` keeps the plain text read.
fn summarize_enabled(settings: &omp_settings::Settings) -> bool {
    settings.get_bool("read.summarize.enabled") == Some(true)
        && !matches!(
            settings.provenance("read.summarize.enabled"),
            None | Some(omp_settings::Provenance::Default)
        )
}

/// A prose file (Markdown/plain text) skips code summarization unless
/// `read.summarize.prose` opts in (omp `isProseSummaryPath`).
fn is_prose_path(path: &Path) -> bool {
    let extension = path
        .extension()
        .map_or_else(String::new, |e| e.to_string_lossy().to_ascii_lowercase());
    matches!(
        extension.as_str(),
        "md" | "markdown" | "mdx" | "txt" | "rst" | "adoc"
    )
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to u32::MAX
fn number_setting(settings: &omp_settings::Settings, id: &str, default: u32) -> u32 {
    settings
        .get_number(id)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or(default, |value| value.min(f64::from(u32::MAX)) as u32)
}

/// Structural summary for a whole-file code read (`read.summarize.*`), or
/// `None` for the plain text read: summarization off, not a whole-file read,
/// prose without `read.summarize.prose`, under `read.summarize.minTotalLines`,
/// over the summary caps, or nothing to elide.
fn maybe_summarize(
    env: &Env,
    absolute: &Path,
    normalized: &str,
    display: &str,
    sel: &Selector,
) -> Option<AgentToolResult> {
    if !matches!(sel, Selector::None) {
        return None;
    }
    if normalized.len() > MAX_SUMMARY_BYTES {
        return None;
    }
    let lines = normalized.lines().count();
    if lines > MAX_SUMMARY_LINES {
        return None;
    }
    let settings = omp_settings::Settings::load(&env.cwd);
    if !summarize_enabled(&settings) {
        return None;
    }
    if is_prose_path(absolute) && settings.get_bool("read.summarize.prose") != Some(true) {
        return None;
    }
    let total = u32::try_from(lines).unwrap_or(u32::MAX);
    if total
        < number_setting(
            &settings,
            "read.summarize.minTotalLines",
            DEFAULT_MIN_TOTAL_LINES,
        )
    {
        return None;
    }
    let unfold = number_setting(
        &settings,
        "read.summarize.unfoldUntil",
        DEFAULT_UNFOLD_UNTIL,
    );
    let summary =
        super::read_formats::summarize_source(normalized, absolute, Some(unfold)).ok()??;
    if summary.elided_lines == 0 {
        return None;
    }
    Some(AgentToolResult::text(format!(
        "[summary: {display} · {total} lines · {} elided · {}]\n{}",
        summary.elided_lines,
        summary.language.as_deref().unwrap_or("unknown"),
        summary.rendered,
    )))
}

/// Append the enclosing block boundary lines a single-range read leaves out
/// (`buildLineEntriesWithBlockContext`, gated like the summarizer).
fn with_block_context(
    env: &Env,
    result: AgentToolResult,
    normalized: &str,
    absolute: &Path,
    sel: &Selector,
) -> AgentToolResult {
    if !summarize_enabled(&omp_settings::Settings::load(&env.cwd)) || is_prose_path(absolute) {
        return result;
    }
    let ranges = match sel {
        Selector::Lines { ranges, raw } if !*raw => ranges,
        _ => return result,
    };
    let [range] = ranges.as_slice() else {
        return result;
    };
    let Some(end) = range.end else {
        return result;
    };
    let (Ok(start), Ok(end)) = (u32::try_from(range.start), u32::try_from(end)) else {
        return result;
    };
    let Ok(Some(boundaries)) =
        super::read_formats::enclosing_boundaries(normalized, absolute, &[(start, end)])
    else {
        return result;
    };
    let mut outside: Vec<u32> = boundaries
        .into_iter()
        .filter(|line| *line < start || *line > end)
        .collect();
    outside.sort_unstable();
    outside.dedup();
    if outside.is_empty() {
        return result;
    }
    let mut result = result;
    let [pa_agent::types::ToolResultContent::Text(block)] = result.content.as_mut_slice() else {
        return result;
    };
    let _ = write!(
        block.text,
        "\n\n[block boundaries: {}]",
        outside
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

// ------------------------------------------------------------- internal URLs

/// Process-global scheme registry (TS `InternalUrlRouter.instance()`).
pub(crate) fn url_registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| {
        let mut reg = Registry::new();
        omp_urls::schemes::register_all(&mut reg).expect("scheme registration");
        reg
    })
}

#[cfg(test)]
thread_local! {
    /// Per-test session file for [`url_ctx`]: the process-global one is also
    /// set by every test that builds a `Runtime`, which races URL tests.
    pub(crate) static TEST_SESSION_FILE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Per-call provider context pinned to the caller's session (TS
/// `sessionResolveContext`).
pub(crate) fn url_ctx(env: &Env) -> UrlCtx {
    #[cfg(test)]
    let session_file = TEST_SESSION_FILE
        .with(|f| f.borrow().clone())
        .or_else(omp_urls::global_session_file);
    #[cfg(not(test))]
    let session_file = omp_urls::global_session_file();
    UrlCtx {
        session_file,
        cwd: Some(env.cwd.clone()),
        skip_directory_listing: false,
        ..UrlCtx::default()
    }
}

/// TS read.ts invalid-selector message for internal URLs.
fn invalid_url_selector(sel: &str, path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Invalid selector ':{sel}' on '{path}'. Use :N, :N-M, :N+K, :N- (open-ended), :-N (last N lines), a comma-separated list of ranges, :raw, :img for SVG rendering, or a range combined with raw (e.g. :raw:50-100)."
    )
}

/// Route a `scheme://` read through the internal URL registry (TS read.ts
/// internal-URL dispatch). `Ok(None)` means the input is not a registered URL.
///
/// # Errors
/// Invalid selector, or provider resolve/format failures.
fn read_internal_url(env: &Env, raw_path: &str) -> Result<Option<AgentToolResult>> {
    let reg = url_registry();
    let normalized = reg.normalize(raw_path);
    if !reg.can_handle(&normalized) {
        return Ok(None);
    }
    let (bare, sel_str) = reg.split(&normalized);
    let parsed = match &sel_str {
        Some(sel) => match parse_sel(sel) {
            Ok(parsed) if !matches!(parsed, Selector::None) => parsed,
            _ => return Err(invalid_url_selector(sel, &bare)),
        },
        None => Selector::None,
    };
    let ctx = url_ctx(env);
    let target = reg.target(&normalized, &ctx)?;
    let Some(target) = target else {
        return Ok(None);
    };

    match target {
        UrlTarget::File {
            url, spec, path, ..
        } => {
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => read_directory(&path, &parsed).map(Some),
                Ok(meta) if meta.is_file() => {
                    let bytes = std::fs::read(&path)
                        .with_context(|| format!("cannot read {}", path.display()))?;
                    if bytes.contains(&0) {
                        bail!(
                            "Cannot read '{}': binary file ({}).",
                            url.raw_href,
                            format_bytes(bytes.len())
                        );
                    }
                    let text = String::from_utf8_lossy(&bytes);
                    let (_, without_bom) = strip_bom(&text);
                    let normalized_text = normalize_to_lf(without_bom);
                    // Immutable resources never mint hashline anchors.
                    let hashline = env.hashline && !spec.immutable;
                    format_read_output(
                        env,
                        &normalized_text,
                        &parsed,
                        &url.raw_href,
                        &url.raw_href,
                        hashline,
                        Some(&path),
                    )
                    .map(Some)
                }
                // The located file vanished: the provider owns the canonical error.
                _ => resolve_resource(env, reg, &bare, &ctx, &parsed),
            }
        }
        UrlTarget::Resource { url, .. } => resolve_resource(env, reg, &url.raw_href, &ctx, &parsed),
    }
}

/// Resolve a resource target and format it with the read selector (TS
/// `#handleInternalUrl` → `buildInMemorySelectorResult`).
fn resolve_resource(
    env: &Env,
    reg: &'static Registry,
    url: &str,
    ctx: &UrlCtx,
    parsed: &Selector,
) -> Result<Option<AgentToolResult>> {
    let resource: InternalResource = reg.resolve(url, ctx)?;
    let immutable = resource.immutable.unwrap_or(false);
    let hashline = env.hashline && !immutable;
    if resource.is_directory {
        return Ok(Some(slice_listing(&resource.content, parsed, url)));
    }
    format_read_output(env, &resource.content, parsed, url, url, hashline, None).map(Some)
}

/// Slice a rendered directory-style listing by a single-range or tail
/// selector (TS `#readDirectory`'s listing slice).
fn slice_listing(content: &str, sel: &Selector, selector_base: &str) -> AgentToolResult {
    let all: Vec<&str> = content.split('\n').collect();
    let (start, limit) = match sel {
        Selector::Lines { ranges, .. } => ranges.first().map_or((0, None), |r| {
            (r.start.saturating_sub(1), r.end.map(|e| e - r.start + 1))
        }),
        Selector::Tail { count, .. } => {
            let start = all.len().saturating_sub(*count);
            (start, Some(all.len() - start))
        }
        Selector::None | Selector::Raw => (0, None),
    };
    if start >= all.len() {
        let suggestion = if all.is_empty() {
            "The listing is empty.".to_string()
        } else {
            format!(
                "Use :1 to read from the start, or :{} to read the last line.",
                all.len()
            )
        };
        return AgentToolResult::text(format!(
            "Line {} is beyond end of listing ({} lines total). {suggestion}",
            start + 1,
            all.len()
        ));
    }
    let end = limit.map_or(all.len(), |l| (start + l).min(all.len()));
    let mut text = all[start..end].join("\n");
    if end < all.len() {
        let _ = write!(
            text,
            "\n\n[{} more lines in listing. Use :{} to continue]",
            all.len() - end,
            end + 1
        );
    }
    let _ = selector_base;
    AgentToolResult::text(text)
}

// ------------------------------------------------------------- directories

struct Node {
    name: String,
    is_dir: bool,
    mtime: f64,
    size: u64,
    children: Vec<Node>,
    /// Entries cut from this directory's listing (`… N more`).
    dropped: usize,
}

fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

fn skip_dir(name: &str) -> bool {
    matches!(name, ".git" | "node_modules")
}

/// Children of `dir` (depth 1 = the root's entries), newest first. Below the
/// root a directory keeps `limit - 1` recent entries plus its oldest.
fn load_children(dir: &Path, depth: usize) -> (Vec<Node>, usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (Vec::new(), 0);
    };
    let mut nodes: Vec<Node> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            let is_dir = meta.is_dir();
            if is_dir && skip_dir(&name) {
                return None;
            }
            Some(Node {
                name,
                is_dir,
                mtime: mtime_ms(&meta),
                size: meta.len(),
                children: Vec::new(),
                dropped: 0,
            })
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.mtime
            .total_cmp(&a.mtime)
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut dropped = 0;
    if depth > 1 && nodes.len() > DIR_CHILD_LIMIT {
        dropped = nodes.len() - DIR_CHILD_LIMIT;
        let oldest = nodes.pop();
        nodes.truncate(DIR_CHILD_LIMIT - 1);
        nodes.extend(oldest);
    }
    for n in &mut nodes {
        if n.is_dir && depth < DIR_MAX_DEPTH {
            let (children, child_dropped) = load_children(&dir.join(&n.name), depth + 1);
            n.children = children;
            n.dropped = child_dropped;
        }
    }
    (nodes, dropped)
}

struct Line {
    label: String,
    size: Option<String>,
    age: Option<String>,
}

fn render(nodes: &[Node], depth: usize, dropped: usize, now_ms: f64, out: &mut Vec<Line>) {
    let indent = "  ".repeat(depth);
    let one = |n: &Node, out: &mut Vec<Line>| {
        let suffix = if n.is_dir { "/" } else { "" };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let age = format_age(((now_ms - n.mtime).max(0.0) / 1000.0) as u64);
        out.push(Line {
            label: format!("{indent}- {}{suffix}", n.name),
            size: (!n.is_dir).then(|| format_bytes(usize::try_from(n.size).unwrap_or(usize::MAX))),
            age: Some(age),
        });
        render(&n.children, depth + 1, n.dropped, now_ms, out);
    };
    if dropped == 0 {
        for n in nodes {
            one(n, out);
        }
        return;
    }
    // Layout: recent entries, the "… N more" marker, then the oldest entry.
    let (oldest, recent) = nodes
        .split_last()
        .map_or((None, nodes), |(l, r)| (Some(l), r));
    for n in recent {
        one(n, out);
    }
    out.push(Line {
        label: format!("{indent}- … {dropped} more"),
        size: None,
        age: None,
    });
    if let Some(n) = oldest {
        one(n, out);
    }
}

fn read_directory(absolute: &Path, sel: &Selector) -> Result<AgentToolResult> {
    let (top, _) = load_children(absolute, 1);
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let mut lines = vec![Line {
        label: ".".to_string(),
        size: None,
        age: None,
    }];
    render(&top, 1, 0, now_ms, &mut lines);
    let rendered = if lines.len() <= 1 {
        "(empty directory)".to_string()
    } else {
        let width = lines
            .iter()
            .map(|l| l.label.chars().count())
            .max()
            .unwrap_or(0);
        lines
            .iter()
            .map(|l| match &l.age {
                None => l.label.clone(),
                Some(age) => {
                    let pad = " ".repeat(width + 2 - l.label.chars().count());
                    format!(
                        "{}{pad}{:<8}  {:<4}",
                        l.label,
                        l.size.clone().unwrap_or_default(),
                        age
                    )
                    .trim_end()
                    .to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let sel = match sel {
        Selector::Tail { count, raw } => {
            let total = rendered.split('\n').count();
            let start = total.saturating_sub(*count) + 1;
            Selector::Lines {
                ranges: vec![LineRange {
                    start,
                    end: Some(total),
                }],
                raw: *raw,
            }
        }
        other => other.clone(),
    };
    if let Selector::Lines { ranges, .. } = &sel {
        if ranges.len() > 1 {
            bail!("Multi-range line selectors are not supported for directory listings.");
        }
        let all: Vec<&str> = rendered.split('\n').collect();
        let r = ranges[0];
        let start = r.start - 1;
        if start >= all.len() {
            return Ok(AgentToolResult::text(format!(
                "Line {} is beyond end of listing ({} lines total). Use :1 to read from the start, or :{} to read the last line.",
                start + 1,
                all.len(),
                all.len()
            )));
        }
        let end = r.end.map_or(all.len(), |e| e.min(all.len()));
        let mut text = all[start..end].join("\n");
        if end < all.len() {
            let _ = write!(
                text,
                "\n\n[{} more lines in listing. Use :{} to continue]",
                all.len() - end,
                end + 1
            );
        }
        return Ok(AgentToolResult::text(text));
    }
    let t = truncate_head(
        &rendered,
        if crate::methodology::raw_tool_output_requested() {
            usize::MAX
        } else {
            DEFAULT_MAX_BYTES
        },
    );
    let mut text = t.content;
    if t.truncated {
        let _ = write!(
            text,
            "\n\n[Showing lines 1-{} of {} ({} limit)]",
            t.output_lines,
            t.total_lines,
            format_bytes(t.max_bytes)
        );
    }
    Ok(AgentToolResult::text(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Env;
    // Shared with every test that installs the process-global session file.
    use super::super::util::SESSION_FILE_TEST_LOCK as URL_TEST_LOCK;

    /// Install a fake session store (`<tmp>/sess.jsonl` + artifacts dir
    /// `<tmp>/sess/`) and return the tempdir guard. URL tests share the
    /// process-global session file, so they take this lock to avoid races.
    fn session_store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let session_file = dir.path().join("sess.jsonl");
        std::fs::write(
            &session_file,
            "{\"type\":\"session\",\"version\":3,\"id\":\"sess\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("sess")).unwrap();
        omp_urls::set_global_session_file(Some(session_file.clone()));
        super::TEST_SESSION_FILE.with(|f| *f.borrow_mut() = Some(session_file));
        dir
    }

    fn env(dir: &Path) -> Env {
        Env::new(dir, true)
    }

    fn text_of(r: &AgentToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Review #4: absurd numeric selectors must not overflow (debug panic /
    /// release wrap); TS's f64 grammar turns them into benign empty windows,
    /// which the saturating arithmetic preserves.
    #[test]
    fn huge_numeric_selectors_do_not_overflow() {
        // Parse-overflow fallback start + count.
        let sel = parse_sel("99999999999999999999999+2").unwrap();
        match sel {
            Selector::Lines { ranges, .. } => {
                assert_eq!(ranges[0].start, usize::MAX);
                assert_eq!(ranges[0].end, Some(usize::MAX));
            }
            other => panic!("expected lines, got {other:?}"),
        }
        // Huge-but-parseable bounds merge without `last_end + 1` overflow.
        let sel =
            parse_sel("18446744073709551610-18446744073709551615,18446744073709551615").unwrap();
        match sel {
            Selector::Lines { ranges, .. } => {
                assert_eq!(ranges.len(), 1);
                assert_eq!(ranges[0].end, Some(usize::MAX));
            }
            other => panic!("expected lines, got {other:?}"),
        }
        // TS error texts are preserved verbatim.
        assert_eq!(
            parse_sel("0").unwrap_err().to_string(),
            "Line selector 0 is invalid; lines are 1-indexed. Use :1."
        );
        assert_eq!(
            parse_sel("5+0").unwrap_err().to_string(),
            "Invalid range 5+0: count must be >= 1."
        );
        assert_eq!(
            parse_sel("5-2").unwrap_err().to_string(),
            "Invalid range 5-2: end must be >= start."
        );
    }

    #[test]
    fn selectors_parse_like_omp() {
        assert_eq!(parse_sel("").unwrap(), Selector::None);
        assert_eq!(parse_sel("raw").unwrap(), Selector::Raw);
        assert_eq!(
            parse_sel("5-16,960-973").unwrap(),
            Selector::Lines {
                ranges: vec![
                    LineRange {
                        start: 5,
                        end: Some(16)
                    },
                    LineRange {
                        start: 960,
                        end: Some(973)
                    }
                ],
                raw: false
            }
        );
        assert_eq!(
            parse_sel("50+10").unwrap(),
            Selector::Lines {
                ranges: vec![LineRange {
                    start: 50,
                    end: Some(59)
                }],
                raw: false
            }
        );
        assert_eq!(
            parse_sel("-60").unwrap(),
            Selector::Tail {
                count: 60,
                raw: false
            }
        );
        assert_eq!(
            parse_sel("raw:2-4").unwrap(),
            Selector::Lines {
                ranges: vec![LineRange {
                    start: 2,
                    end: Some(4)
                }],
                raw: true
            }
        );
        assert!(parse_sel("0").is_err());
        assert!(parse_sel("9-3").is_err());
    }

    #[test]
    fn hashline_read_header_rows_and_footer() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 1..=5 {
            use std::fmt::Write as _;
            let _ = writeln!(body, "line {i}");
        }
        std::fs::write(dir.path().join("a.txt"), &body).unwrap();
        let e = env(dir.path());
        let out = text_of(&read(&e, &serde_json::json!({"path": "a.txt"})).unwrap());
        let mut it = out.lines();
        let header = it.next().unwrap();
        assert!(
            header.starts_with("[a.txt#") && header.ends_with(']'),
            "{header}"
        );
        assert_eq!(header.len(), "[a.txt#ABCD]".len());
        assert_eq!(it.next(), Some("1:line 1"));
        assert_eq!(out.lines().last(), Some("5:line 5"));
        // window with selector: one leading + three trailing context lines, footer
        let out = text_of(&read(&e, &serde_json::json!({"path": "a.txt:2-2"})).unwrap());
        assert!(out.contains("1:line 1\n2:line 2\n3:line 3"), "{out}");
        assert!(out.contains("5:line 5"), "{out}");
        let raw = text_of(&read(&e, &serde_json::json!({"path": "a.txt:raw:2-3"})).unwrap());
        assert_eq!(
            raw,
            "line 2\nline 3\n\n[2 more lines in file. Use a.txt:4 to continue]"
        );
        let tail = text_of(&read(&e, &serde_json::json!({"path": "a.txt:-2"})).unwrap());
        assert!(tail.contains("4:line 4\n5:line 5"), "{tail}");
    }

    #[test]
    fn hashline_output_matches_ts_natives() {
        // Same fixture and TS formatting functions (bun cross-check, 2026-10-12):
        //   hashlineFileHash → "0793", hashlineFormatHeader → "[a.txt#0793]",
        //   rows via formatNumberedLine → "N:line".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            "line 1\nline 2\nline 3\nline 4\nline 5\n",
        )
        .unwrap();
        let e = env(dir.path());
        let out = text_of(&read(&e, &serde_json::json!({"path": "a.txt"})).unwrap());
        let expected = "[a.txt#0793]\n1:line 1\n2:line 2\n3:line 3\n4:line 4\n5:line 5";
        assert_eq!(out, expected, "omt read output diverged from TS formatting");
    }

    #[test]
    fn read_reports_missing_and_beyond_eof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let e = env(dir.path());
        let err = read(&e, &serde_json::json!({"path": "nope.txt"}))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Path 'nope.txt' not found");
        let out = text_of(&read(&e, &serde_json::json!({"path": "a.txt:9"})).unwrap());
        assert!(
            out.starts_with("Line 9 is beyond end of file (1 lines total)."),
            "{out}"
        );
    }

    /// Past the spill threshold a file read keeps its header (head), the
    /// continue footer (tail) and the line the request names; everything
    /// else is elided behind markers plus the artifact pointer.
    #[test]
    fn spilled_read_keeps_edges_and_the_requested_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "tools:\n  artifactSpillThreshold: 12\n  artifactHeadBytes: 3\n  artifactTailBytes: 6\n  artifactTailLines: 150\nread:\n  defaultLimit: 300\n",
        )
        .unwrap();
        let body: String = (1..=400)
            .map(|i| {
                if i == 150 {
                    "fn reconcile_ledger_snapshot() { /* the one */ }\n".to_string()
                } else {
                    format!("let filler_{i} = compute_value({i}, \"padding padding padding padding padding\");\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("big.rs"), &body).unwrap();
        let env = Env::new(dir.path(), false);
        let full = read(&env, &serde_json::json!({"path": "big.rs"})).unwrap();
        let full_text = text_of(&full);
        assert!(full_text.len() > 12 * 1024, "{}", full_text.len());
        *env.query.lock() = "where is reconcile_ledger_snapshot defined?".to_string();
        let out = text_of(&relevance_spill(&env, full, |_| {
            Some("artifact://9".to_string())
        }));
        assert!(out.len() < 10 * 1024, "{}", out.len());
        assert!(out.contains("reconcile_ledger_snapshot"), "{out}");
        assert!(out.starts_with(full_text.lines().next().unwrap()), "{out}");
        assert!(out.contains("to continue]"), "{out}");
        assert!(out.contains(" lines omitted] …"), "{out}");
        assert!(out.ends_with("Read artifact://9 for full output]"), "{out}");
    }

    #[test]
    fn long_files_page_with_continue_footer() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 1..=400 {
            use std::fmt::Write as _;
            let _ = writeln!(body, "l{i}");
        }
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let out =
            text_of(&read(&env(dir.path()), &serde_json::json!({"path": "big.txt"})).unwrap());
        assert!(out.contains("300:l300"));
        assert!(!out.contains("301:l301"));
        assert!(
            out.ends_with("[100 more lines in file. Use big.txt:301 to continue]"),
            "{out}"
        );
    }
    #[test]
    fn reads_artifact_url_with_selector() {
        let _lock = URL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = session_store();
        let dir = tempfile::tempdir().unwrap();
        let artifacts = store.path().join("sess");
        std::fs::write(artifacts.join("3.log"), "l1\nl2\nl3\nl4\n").unwrap();
        let env = Env::new(dir.path(), false);
        let out = text_of(&read(&env, &serde_json::json!({"path": "artifact://3:2-3"})).unwrap());
        // Immutable scheme: raw display, exact window (no context expansion);
        // a range ending before EOF carries the continuation hint like TS.
        assert_eq!(
            out,
            "l2\nl3\n\n[1 more lines in file. Use artifact://3:4 to continue]"
        );
    }

    #[test]
    fn reads_agent_url_markdown() {
        let _lock = URL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = session_store();
        let dir = tempfile::tempdir().unwrap();
        let artifacts = store.path().join("sess");
        std::fs::write(artifacts.join("Worker.md"), "report body\n").unwrap();
        let env = Env::new(dir.path(), false);
        let out = text_of(&read(&env, &serde_json::json!({"path": "agent://Worker"})).unwrap());
        assert_eq!(out, "report body");
    }

    #[test]
    fn reads_history_index_and_local_file() {
        let _lock = URL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = session_store();
        let dir = tempfile::tempdir().unwrap();
        let artifacts = store.path().join("sess");
        std::fs::write(
            artifacts.join("Agent.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"a\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(artifacts.join("local")).unwrap();
        std::fs::write(artifacts.join("local/notes.txt"), "scratch\n").unwrap();
        let env = Env::new(dir.path(), false);

        let out = text_of(&read(&env, &serde_json::json!({"path": "history://"})).unwrap());
        assert!(out.starts_with("# Agents"));
        assert!(out.contains("| Agent | on disk |"));

        let out = text_of(&read(&env, &serde_json::json!({"path": "local://notes.txt"})).unwrap());
        assert_eq!(out, "scratch");
    }

    #[test]
    fn invalid_url_selector_names_the_url() {
        let _lock = URL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = session_store();
        let dir = tempfile::tempdir().unwrap();
        let artifacts = store.path().join("sess");
        std::fs::write(artifacts.join("1.log"), "x\n").unwrap();
        let env = Env::new(dir.path(), false);
        // "bogus" is not selector-shaped, so nothing peels and the numeric-ID
        // error surfaces — TS behavior for unregistered selector chunks.
        let err = read(&env, &serde_json::json!({"path": "artifact://1:bogus"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            "artifact:// ID must be numeric, got: 1:bogus"
        );
        // A selector-shaped but malformed chunk peels, then errors as a selector.
        let err = read(
            &env,
            &serde_json::json!({"path": "artifact://1:2-3:raw:-100-5"}),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid selector ':2-3:raw:-100-5' on 'artifact://1'. Use :N, :N-M, :N+K, :N- (open-ended), :-N (last N lines), a comma-separated list of ranges, :raw, :img for SVG rendering, or a range combined with raw (e.g. :raw:50-100)."
        );
    }

    #[test]
    fn unknown_scheme_falls_through_to_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "plain\n").unwrap();
        let env = Env::new(dir.path(), false);
        // Not a registered scheme: the existing path pipeline answers.
        let out = text_of(&read(&env, &serde_json::json!({"path": "f.txt"})).unwrap());
        assert!(out.contains("plain"));
    }

    #[test]
    fn url_dispatch_recognizes_http_and_www() {
        assert!(crate::tools::read_fetch::is_fetch_url(
            "https://example.com/x"
        ));
        assert!(crate::tools::read_fetch::is_fetch_url("www.example.com/x"));
        assert!(!crate::tools::read_fetch::is_fetch_url("/tmp/x.txt"));
        assert_eq!(
            split_url_raw("https://example.com/x:raw"),
            ("https://example.com/x", true)
        );
        assert_eq!(
            split_url_raw("https://example.com/x"),
            ("https://example.com/x", false)
        );
        assert_eq!(
            split_url_raw("https://example.com/x:RAW"),
            ("https://example.com/x", true)
        );
    }

    #[test]
    fn reads_notebook_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nb.ipynb"),
            r##"{"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"language":"python"}},"cells":[{"cell_type":"markdown","source":["# Title\n"]},{"cell_type":"code","source":["print(1)\n"],"outputs":[],"execution_count":1}]}"##,
        )
        .unwrap();
        let out =
            text_of(&read(&env(dir.path()), &serde_json::json!({"path": "nb.ipynb"})).unwrap());
        assert!(out.contains("# Notebook v4.5"), "{out}");
        assert!(out.contains("# Title"), "{out}");
        assert!(out.contains("print(1)"), "{out}");
    }

    #[test]
    fn reads_sqlite_listing_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n             INSERT INTO users (name) VALUES ('ada');",
        )
        .unwrap();
        drop(conn);
        let e = env(dir.path());
        let out = text_of(&read(&e, &serde_json::json!({"path": "app.sqlite"})).unwrap());
        assert!(out.contains("users"), "{out}");
        let out = text_of(&read(&e, &serde_json::json!({"path": "app.sqlite:users"})).unwrap());
        assert!(out.contains("ada"), "{out}");
    }

    #[test]
    fn reads_json_query_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("data.json"),
            r#"{"users":[{"name":"ada"},{"name":"lin"}]}"#,
        )
        .unwrap();
        let out = text_of(
            &read(
                &env(dir.path()),
                &serde_json::json!({"path": "data.json?q=.users"}),
            )
            .unwrap(),
        );
        assert!(out.contains("ada"), "{out}");
        // A query-less JSON read stays a plain text read.
        let out =
            text_of(&read(&env(dir.path()), &serde_json::json!({"path": "data.json"})).unwrap());
        assert!(out.contains("\"users\""), "{out}");
    }

    #[test]
    fn reads_pdf_and_binary_without_a_bare_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("doc.pdf"),
            b"%PDF-1.4\n1 0 obj\n<</Type/Page>>\nendobj\ntrailer\n%%EOF\n",
        )
        .unwrap();
        let out =
            text_of(&read(&env(dir.path()), &serde_json::json!({"path": "doc.pdf"})).unwrap());
        assert!(out.to_lowercase().contains("pdf"), "{out}");

        let mut elf = b"\x7fELF\x02\x01\x01\x00".to_vec();
        elf.extend_from_slice(&[0u8; 64]);
        elf.extend_from_slice(b"main\0");
        std::fs::write(dir.path().join("prog.bin"), &elf).unwrap();
        let out =
            text_of(&read(&env(dir.path()), &serde_json::json!({"path": "prog.bin"})).unwrap());
        assert!(!out.is_empty(), "binary read produced no content");
        assert!(!out.contains("binary file"), "{out}");
        // `:raw` still refuses to shape binary bytes.
        let err = read(
            &env(dir.path()),
            &serde_json::json!({"path": "prog.bin:raw"}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("binary file"), "{err}");
    }

    #[test]
    fn summarization_is_opt_in() {
        assert!(is_prose_path(Path::new("notes.md")));
        assert!(!is_prose_path(Path::new("main.rs")));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "read:\n  summarize:\n    enabled: true\n",
        )
        .unwrap();
        assert!(summarize_enabled(&omp_settings::Settings::load(dir.path())));
        assert!(!summarize_enabled(&omp_settings::Settings::load(
            tempfile::tempdir().unwrap().path()
        )));
    }

    #[test]
    fn whole_file_code_read_keeps_its_head_with_summarize_on() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..40)
            .map(|i| {
                format!(
                    "pub fn f{i}() {{\n    let a = {i};\n    let b = a + 1;\n    let c = a + b;\n    let d = c * 2;\n    println!(\"{{d}}\");\n}}\n"
                )
            })
            .collect();
        std::fs::write(dir.path().join("lib.rs"), &body).unwrap();
        let e = env(dir.path());
        // Default (unset): the plain hashline read.
        let out = text_of(&read(&e, &serde_json::json!({"path": "lib.rs"})).unwrap());
        assert!(out.starts_with("[lib.rs#"), "{out}");
        assert!(out.contains("pub fn f0()"), "{out}");
        // Summarization on: whatever pi-ast elides, the head survives.
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "read:\n  summarize:\n    enabled: true\n",
        )
        .unwrap();
        let out = text_of(&read(&e, &serde_json::json!({"path": "lib.rs"})).unwrap());
        assert!(out.contains("pub fn f0()"), "{out}");
    }
}
