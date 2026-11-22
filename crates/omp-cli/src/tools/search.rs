//! `grep` (ripgrep engine crates over the `omp-xfs` parallel walk) and `glob`
//! (`omp-xfs`, mtime-ranked), with omp's result formats and notices.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use futures::{stream, StreamExt};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use omp_xfs::glob::{glob as glob_walk, GlobOptions, Selector};
use omp_xfs::walk::{self, Action};
use omp_xfs::{FileType, Fs};
use pa_agent::abort::AbortSignal;
use pa_agent::types::AgentToolResult;
use pi_edit::path_policy::canonical_key;
use pi_edit::store::seen_lines_from_body;

use super::read::{parse_sel, LineRange, Selector as ReadSelector};
use super::tree::{events, format_grouped_paths, Kind};
use super::util::{
    cut_bytes, format_bytes, format_path_relative_to_cwd, path_list, resolve_to_cwd, truncate_head,
    DEFAULT_MAX_BYTES,
};
use super::Env;

const DEFAULT_FILE_LIMIT: usize = 20;
const MULTI_FILE_PER_FILE_MATCHES: usize = 20;
const SINGLE_FILE_MATCHES: usize = 200;
const INTERNAL_TOTAL_CAP: usize = 2000;
const MAX_COLUMN: usize = 512;
const CONTEXT_BEFORE: usize = 1;
const CONTEXT_AFTER: usize = 3;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Files read and searched at once (one `Searcher` each, since a `Searcher` is
/// neither `Sync` nor `Send`). Results are folded back in path order, so this
/// bounds memory and in-flight I/O without ever changing the output.
const SEARCH_CONCURRENCY: usize = 8;
const GLOB_DEFAULT_LIMIT: usize = 200;
const GLOB_MAX_LIMIT: usize = 200;

// -------------------------------------------------------------- patterns

fn has_glob_chars(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// omp `parseFindPattern`: base directory, glob and whether a glob is present.
pub struct FindPattern {
    pub base: String,
    pub glob: String,
    pub has_glob: bool,
}

#[must_use]
pub fn parse_find_pattern(pattern: &str) -> FindPattern {
    let normalized = pattern.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    let Some(first) = segments.iter().position(|s| has_glob_chars(s)) else {
        return FindPattern {
            base: normalized,
            glob: "**/*".into(),
            has_glob: false,
        };
    };
    if first == 0 {
        let glob = if normalized.starts_with("**/") {
            normalized
        } else {
            format!("**/{normalized}")
        };
        return FindPattern {
            base: ".".into(),
            glob,
            has_glob: true,
        };
    }
    FindPattern {
        base: segments[..first].join("/"),
        glob: segments[first..].join("/"),
        has_glob: true,
    }
}

/// Nanoseconds (the `omp-xfs` walker's unit) to the milliseconds `glob` ranks
/// by.
#[allow(clippy::cast_precision_loss)] // mtimes are far below f64's exact range
fn millis(nanos: i64) -> f64 {
    nanos as f64 / 1_000_000.0
}

// ------------------------------------------------------------------ glob

fn bool_arg(args: &serde_json::Value, key: &str) -> Option<bool> {
    args.get(key).and_then(serde_json::Value::as_bool)
}

fn with_details(text: String, details: serde_json::Value) -> AgentToolResult {
    let mut result = AgentToolResult::text(text);
    result.details = details;
    result
}

/// The `path` a call searches, from `path` plus the reference's runtime-only
/// `pattern` key (omp `globSearchPath`): each `;` entry of `pattern` is scoped
/// under each `;` entry of `path` unless it is absolute, `~`-rooted or a URL.
fn glob_search_path(args: &serde_json::Value) -> Option<String> {
    let base = args.get("path").and_then(serde_json::Value::as_str);
    let Some(pattern) = args.get("pattern").and_then(serde_json::Value::as_str) else {
        return base.map(ToOwned::to_owned);
    };
    let split = |value: &str| -> Vec<String> {
        value
            .split(';')
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty())
            .collect()
    };
    let patterns = split(pattern);
    let bases = base.map(split).unwrap_or_default();
    if bases.is_empty() {
        return Some(patterns.join(";"));
    }
    let mut scoped: Vec<String> = Vec::new();
    for dir in &bases {
        for pattern in &patterns {
            let standalone = pattern.starts_with('/')
                || pattern.starts_with('~')
                || is_url_like(pattern);
            let entry = if standalone {
                pattern.clone()
            } else {
                format!("{}/{pattern}", dir.trim_end_matches(['/', '\\']))
            };
            if !scoped.contains(&entry) {
                scoped.push(entry);
            }
        }
    }
    Some(scoped.join(";"))
}

/// `scheme://` prefix test (omp's `/^[a-z][a-z0-9+.-]*:\/\//i`).
fn is_url_like(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && scheme
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'+' | b'.' | b'-')))
}

/// Run the `glob` tool.
///
/// # Errors
/// Missing paths, invalid glob or limit.
// Layout mirrors omp's `glob` flow top-to-bottom; splitting it would obscure that parity.
// The limit casts below are lossless: both limits are small fixed constants (200).
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub async fn glob(
    env: Arc<Env>,
    args: serde_json::Value,
    signal: AbortSignal,
) -> Result<AgentToolResult> {
    let fs = Fs::native();
    let mut patterns = path_list(glob_search_path(&args).as_deref());
    if patterns.is_empty() {
        patterns.push(".".into());
    }
    let hidden = bool_arg(&args, "hidden").unwrap_or(true);
    let gitignore = bool_arg(&args, "gitignore").unwrap_or(true);
    let requested = args
        .get("limit")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(GLOB_DEFAULT_LIMIT as f64);
    if !requested.is_finite() || requested <= 0.0 {
        bail!("Limit must be a positive number");
    }
    #[allow(clippy::cast_possible_truncation)]
    let limit = (requested.floor() as usize).clamp(1, GLOB_MAX_LIMIT);
    let clamp_notice = (requested > GLOB_MAX_LIMIT as f64)
        .then(|| format!("Requested limit {requested} clamped to the max of {GLOB_MAX_LIMIT}"));

    let single = patterns.len() == 1;
    let mut missing: Vec<String> = Vec::new();
    let mut merged: Vec<(String, f64)> = Vec::new();
    let mut scope = String::new();
    for pat in &patterns {
        pa_agent::abort::throw_if_aborted(&signal)?;
        let parsed = parse_find_pattern(pat.trim().trim_matches('"'));
        let base = resolve_to_cwd(&parsed.base, &env.cwd);
        if base == Path::new("/") {
            bail!("Searching from root directory '/' is not allowed");
        }
        if scope.is_empty() {
            scope = format_path_relative_to_cwd(&base, &env.cwd, false);
        }
        let Ok(info) = fs.session().stat(&base, true).await else {
            if single {
                bail!("Path not found: {scope}");
            }
            missing.push(pat.clone());
            continue;
        };
        if !parsed.has_glob && info.file_type == Some(FileType::File) {
            merged.push((
                format_path_relative_to_cwd(&base, &env.cwd, false),
                millis(info.stamp.mtime),
            ));
            continue;
        }
        if info.file_type != Some(FileType::Dir) {
            if single {
                bail!("Path is not a directory: {}", base.display());
            }
            continue;
        }
        // `parse_find_pattern` already made a bare glob recursive (`**/*.ts`),
        // so the remaining glob keeps the depth its own segments spell out.
        let opts = GlobOptions {
            pattern: parsed.glob.clone(),
            recursive: false,
            max_results: Some(limit),
            sort_by_mtime: true,
            walk: walk::Options {
                hidden,
                gitignore,
                ..walk::Options::default()
            },
            ..GlobOptions::default()
        };
        let found = tokio::select! {
            () = signal.aborted() => return Err(pa_agent::abort::aborted_error()),
            found = glob_walk(&fs, &base, &opts) => {
                found.map_err(|e| anyhow::anyhow!("{e}"))?
            }
        };
        for entry in found {
            let abs = fs.join(&base, entry.path.as_bytes());
            let is_dir = entry.file_type == FileType::Dir;
            let shown = format_path_relative_to_cwd(&abs, &env.cwd, is_dir);
            merged.push((shown, entry.mtime.map_or(0.0, millis)));
        }
    }
    if patterns.len() > 1 && merged.is_empty() && missing.len() == patterns.len() {
        bail!("Path not found: {}", missing.join(", "));
    }
    let mut seen = BTreeSet::new();
    merged.retain(|(p, _)| seen.insert(p.clone()));
    merged.sort_by(|a, b| b.1.total_cmp(&a.1));
    let missing_note =
        (!missing.is_empty()).then(|| format!("Skipped missing paths: {}", missing.join(", ")));
    if merged.is_empty() {
        let mut parts = vec!["No files found matching pattern".to_string()];
        parts.extend(missing_note);
        let mut details = serde_json::json!({
            "scopePath": scope,
            "fileCount": 0,
            "files": [],
            "truncated": false,
            "cwd": env.cwd.display().to_string(),
        });
        if !missing.is_empty() {
            details["missingPaths"] = serde_json::json!(missing);
        }
        return Ok(with_details(parts.join("\n"), details));
    }
    let files: Vec<String> = merged.into_iter().map(|(p, _)| p).collect();
    let reached = files.len() >= limit;
    let limited = &files[..files.len().min(limit)];
    let mut out = format_grouped_paths(limited);
    let mut trailing: Vec<String> = Vec::new();
    trailing.extend(clamp_notice);
    trailing.extend(missing_note);
    if !trailing.is_empty() {
        let _ = write!(out, "\n\n{}", trailing.join("\n"));
    }
    let t = truncate_head(
        &out,
        if crate::methodology::raw_tool_output_requested() {
            usize::MAX
        } else {
            DEFAULT_MAX_BYTES
        },
    );
    let mut text = t.content.clone();
    let mut notices: Vec<String> = Vec::new();
    if t.truncated {
        notices.push(format!(
            "Showing lines 1-{} of {} ({} limit)",
            t.output_lines,
            t.total_lines,
            format_bytes(t.max_bytes)
        ));
    }
    if reached {
        let suggestion = (limit * 2).min(GLOB_MAX_LIMIT);
        notices.push(if suggestion > limit {
            format!("{limit} results limit reached. Use limit={suggestion} for more")
        } else {
            format!("{limit} results limit reached")
        });
    }
    if !notices.is_empty() {
        let _ = write!(text, "\n\n[{}]", notices.join(". "));
    }
    let mut details = serde_json::json!({
        "scopePath": scope,
        "fileCount": limited.len(),
        "files": limited,
        "truncated": reached || t.truncated,
        "cwd": env.cwd.display().to_string(),
    });
    if reached {
        details["resultLimitReached"] = serde_json::json!(limit);
    }
    if !missing.is_empty() {
        details["missingPaths"] = serde_json::json!(missing);
    }
    Ok(with_details(text, details))
}

// ------------------------------------------------------------------ grep

#[derive(Debug, Clone)]
struct Match {
    line_number: u64,
    line: String,
    matched: bool,
}

struct Collector {
    rows: Vec<Match>,
    matches: usize,
    cap: usize,
    truncated_lines: bool,
}

impl Collector {
    fn clip(&mut self, bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.len() > MAX_COLUMN {
            self.truncated_lines = true;
            return format!("{}...", cut_bytes(text, MAX_COLUMN - 3));
        }
        text.to_string()
    }
}

impl Sink for Collector {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let first = m.line_number().unwrap_or(0);
        // A multi-line match yields one row per line.
        for (i, raw) in m.lines().enumerate() {
            let line = self.clip(raw);
            self.rows.push(Match {
                line_number: first + i as u64,
                line,
                matched: true,
            });
        }
        self.matches += 1;
        Ok(self.matches <= self.cap)
    }

    fn context(&mut self, _s: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if matches!(c.kind(), SinkContextKind::Other) && self.matches > self.cap {
            return Ok(false);
        }
        let line = self.clip(c.bytes());
        self.rows.push(Match {
            line_number: c.line_number().unwrap_or(0),
            line,
            matched: false,
        });
        Ok(true)
    }
}

fn escape_unescaped_parens(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 4);
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn build_matcher(pattern: &str, ignore_case: bool, multiline: bool) -> Result<RegexMatcher> {
    let build = |p: &str| -> Result<RegexMatcher, grep_regex::Error> {
        let make = |line_terminated: bool| {
            let mut b = RegexMatcherBuilder::new();
            b.case_insensitive(ignore_case).multi_line(multiline);
            if line_terminated {
                b.line_terminator(Some(b'\n'));
            }
            b.build(p)
        };
        if !multiline {
            if let Ok(m) = make(true) {
                return Ok(m);
            }
        }
        make(false)
    };
    let err = match build(pattern) {
        Ok(m) => return Ok(m),
        Err(e) => e,
    };
    let message = err.to_string();
    if message.contains("unclosed group") || message.contains("unopened group") {
        let escaped = escape_unescaped_parens(pattern);
        if escaped != pattern {
            if let Ok(m) = build(&escaped) {
                return Ok(m);
            }
        }
    }
    build(&regex::escape(pattern)).map_err(|_| anyhow::anyhow!("Invalid regex: {message}"))
}

#[derive(Debug, Clone)]
struct Target {
    base: PathBuf,
    glob: Option<String>,
    ranges: Option<Vec<LineRange>>,
    /// The base is a regular file, not a directory to walk.
    file: bool,
}

struct FileHits {
    shown: String,
    absolute: PathBuf,
    matches: Vec<Match>,
    match_count: usize,
}

fn in_ranges(line: u64, ranges: &[LineRange]) -> bool {
    let line = usize::try_from(line).unwrap_or(usize::MAX);
    ranges
        .iter()
        .any(|r| line >= r.start && r.end.is_none_or(|e| line <= e))
}

/// Reads `path` through `fs` (never past [`MAX_FILE_BYTES`]) and searches the
/// bytes with the caller's `Searcher`, so file I/O stays on the filesystem
/// layer while the scan keeps grep-searcher's exact row semantics.
async fn search_file(
    fs: &Fs,
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    per_file_cap: usize,
    ranges: Option<&[LineRange]>,
) -> Option<(Vec<Match>, usize, bool)> {
    let (mut file, _) = fs.session().open_file(path).await.ok()?;
    let mut bytes = Vec::new();
    file.read_into(&mut bytes, MAX_FILE_BYTES).await.ok()?;
    let mut sink = Collector {
        rows: Vec::new(),
        matches: 0,
        cap: per_file_cap,
        truncated_lines: false,
    };
    searcher.search_slice(matcher, &bytes, &mut sink).ok()?;
    let mut rows = sink.rows;
    let mut count = sink.matches;
    if let Some(r) = ranges {
        rows.retain(|m| in_ranges(m.line_number, r));
        count = rows.iter().filter(|m| m.matched).count();
    }
    if count == 0 {
        return None;
    }
    Some((rows, count.min(per_file_cap + 1), sink.truncated_lines))
}

async fn split_targets(env: &Env, entries: &[String], fs: &Fs) -> (Vec<Target>, Vec<String>) {
    let mut targets = Vec::new();
    let mut missing = Vec::new();
    let mut session = fs.session();
    for entry in entries {
        let entry = entry.trim().trim_matches('"').to_string();
        // line-range selector on an existing file: `src/foo.ts:50-100`
        let mut ranges = None;
        let mut path_part = entry.clone();
        if session
            .stat(&resolve_to_cwd(&entry, &env.cwd), true)
            .await
            .is_err()
        {
            if let Some((p, sel)) = entry.rsplit_once(':') {
                if let Ok(ReadSelector::Lines { ranges: r, .. }) = parse_sel(sel) {
                    if session
                        .stat(&resolve_to_cwd(p, &env.cwd), true)
                        .await
                        .is_ok_and(|info| info.file_type == Some(FileType::File))
                    {
                        ranges = Some(r);
                        path_part = p.to_string();
                    }
                }
            }
        }
        let literal = resolve_to_cwd(&path_part, &env.cwd);
        if let Ok(info) = session.stat(&literal, true).await {
            targets.push(Target {
                base: literal,
                glob: None,
                ranges,
                file: info.file_type == Some(FileType::File),
            });
            continue;
        }
        let parsed = parse_find_pattern(&path_part);
        if parsed.has_glob {
            // A glob spelled without a directory prefix matches at any depth.
            let no_dir_prefix = !path_part.contains('/');
            let glob = if no_dir_prefix && !parsed.glob.starts_with("**") {
                format!("**/{}", parsed.glob)
            } else {
                parsed.glob
            };
            let dir = resolve_to_cwd(&parsed.base, &env.cwd);
            if session
                .stat(&dir, true)
                .await
                .is_ok_and(|info| info.file_type == Some(FileType::Dir))
            {
                targets.push(Target {
                    base: dir,
                    glob: Some(glob),
                    ranges: None,
                    file: false,
                });
                continue;
            }
        }
        missing.push(entry);
    }
    (targets, missing)
}

/// The files below `target` to search, in path order.
async fn target_files(fs: &Fs, target: &Target, gitignore: bool) -> Result<Vec<PathBuf>> {
    if target.file {
        return Ok(vec![target.base.clone()]);
    }
    let pattern = target.glob.clone().unwrap_or_else(|| "**/*".to_string());
    // Not recursive: `parse_find_pattern` already split off the base directory
    // and prefixed a bare glob with `**/`, so the remaining glob is relative to
    // `target.base` and `dir/*.ts` must stay inside `dir`.
    let selector =
        Selector::new(&pattern, false).map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;
    let opts = walk::Options {
        hidden: true,
        gitignore,
        skip_git: !selector.mentions_git(),
        skip_node_modules: !selector.mentions_node_modules(),
        ..walk::Options::default()
    };
    let found = walk::collect(fs, &target.base, &opts, true, move |_, entry| {
        if entry.file_type == FileType::File && selector.matches(entry.path) {
            return Action::Continue;
        }
        if entry.file_type == FileType::Dir && selector.prunes(entry.path) {
            Action::SkipDir
        } else {
            Action::Skip
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(found
        .iter()
        .map(|entry| fs.join(&target.base, entry.path))
        .collect())
}

/// Run the `grep` tool.
///
/// # Errors
/// Empty pattern, bad `skip`, or no path resolving.
#[allow(clippy::too_many_lines)]
pub async fn grep(
    env: Arc<Env>,
    args: serde_json::Value,
    signal: AbortSignal,
) -> Result<AgentToolResult> {
    let fs = Fs::native();
    let pattern = super::util::str_arg(&args, "pattern")?;
    if pattern.trim().is_empty() {
        bail!("Pattern must not be empty");
    }
    let skip = match args.get("skip") {
        None | Some(serde_json::Value::Null) => 0usize,
        Some(v) => match v.as_f64() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(n) if n.is_finite() && n >= 0.0 => n.floor() as usize,
            _ => bail!("Skip must be a non-negative number"),
        },
    };
    let mut entries = path_list(args.get("path").and_then(serde_json::Value::as_str));
    if entries.is_empty() {
        entries.push(".".into());
    }
    let ignore_case = !bool_arg(&args, "case").unwrap_or(true);
    let gitignore = bool_arg(&args, "gitignore").unwrap_or(true);
    let multiline = pattern.contains('\n') || pattern.contains("\\n");

    let (targets, missing) = split_targets(&env, &entries, &fs).await;
    if targets.is_empty() {
        bail!(
            "Path not found: {}; list each target in the semicolon-delimited `path`",
            missing.join(", ")
        );
    }
    let scope_path = targets
        .iter()
        .map(|t| format_path_relative_to_cwd(&t.base, &env.cwd, false))
        .collect::<Vec<_>>()
        .join(", ");
    let search_path = targets[0].base.display().to_string();
    let matcher = build_matcher(pattern, ignore_case, multiline)?;
    let new_searcher = || {
        SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .multi_line(multiline)
            .before_context(CONTEXT_BEFORE)
            .after_context(CONTEXT_AFTER)
            .build()
    };

    let single_file = targets.len() == 1 && targets[0].glob.is_none() && targets[0].file;
    let is_multi_scope = !single_file;
    let per_file_cap = if is_multi_scope {
        MULTI_FILE_PER_FILE_MATCHES
    } else {
        SINGLE_FILE_MATCHES
    };

    let mut hits: Vec<FileHits> = Vec::new();
    let mut seen_files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut total_matches = 0usize;
    let mut limit_reached = false;
    let mut lines_truncated = false;
    let mut oversized: Vec<String> = Vec::new();
    for target in &targets {
        if signal.is_aborted() {
            return Err(pa_agent::abort::aborted_error());
        }
        if target.file {
            if let Ok(info) = fs.session().stat(&target.base, true).await {
                if info.size > MAX_FILE_BYTES {
                    oversized.push(format_path_relative_to_cwd(&target.base, &env.cwd, false));
                }
            }
        }
        let files: Vec<PathBuf> = target_files(&fs, target, gitignore)
            .await?
            .into_iter()
            .filter(|f| seen_files.insert(f.clone()))
            .collect();
        // Files are searched concurrently but folded back in path order, so
        // output and the total-match cap stay identical to a serial scan.
        let mut searched = stream::iter(files)
            .map(|file| {
                // `Fs` is a cheap handle (native: a unit variant) and a
                // `RegexMatcher` clone shares its compiled `Regex`.
                let fs = fs.clone();
                let matcher = matcher.clone();
                async move {
                    let mut searcher = new_searcher();
                    let found = search_file(
                        &fs,
                        &mut searcher,
                        &matcher,
                        &file,
                        per_file_cap,
                        target.ranges.as_deref(),
                    )
                    .await;
                    (file, found)
                }
            })
            .buffered(SEARCH_CONCURRENCY);
        loop {
            let next = tokio::select! {
                () = signal.aborted() => return Err(pa_agent::abort::aborted_error()),
                next = searched.next() => next,
            };
            let Some((file, found)) = next else { break };
            let Some((rows, count, truncated)) = found else {
                continue;
            };
            lines_truncated |= truncated;
            total_matches += count;
            hits.push(FileHits {
                shown: format_path_relative_to_cwd(&file, &env.cwd, false),
                absolute: file,
                matches: rows,
                match_count: count,
            });
            if total_matches >= INTERNAL_TOTAL_CAP {
                limit_reached = true;
                break;
            }
        }
        if limit_reached {
            break;
        }
    }

    // Per-file cap (keep whole context rows up to the cap-th match).
    for h in &mut hits {
        if h.match_count > per_file_cap {
            let mut seen = 0usize;
            let mut cut = h.matches.len();
            for (i, m) in h.matches.iter().enumerate() {
                if m.matched {
                    seen += 1;
                    if seen > per_file_cap {
                        cut = i;
                        break;
                    }
                }
            }
            h.matches.truncate(cut);
        }
    }

    let total_files = hits.len();
    let total_files_label = if limit_reached {
        format!("{total_files}+")
    } else {
        total_files.to_string()
    };
    let can_paginate = is_multi_scope;
    let skip_files = if can_paginate {
        skip.min(total_files)
    } else {
        0
    };
    let window: Vec<&FileHits> = if can_paginate {
        hits.iter()
            .skip(skip_files)
            .take(DEFAULT_FILE_LIMIT)
            .collect()
    } else {
        hits.iter().collect()
    };
    let file_limit_reached = can_paginate && total_files > skip_files + DEFAULT_FILE_LIMIT;
    let next_skip = skip_files + window.len();
    let limit_message = file_limit_reached.then(|| {
        format!(
            "Showing files {}-{next_skip} of {total_files_label}. Use skip={next_skip} for the next page, or narrow paths/pattern.",
            skip_files + 1
        )
    });

    let missing_note =
        (!missing.is_empty()).then(|| format!("Skipped missing paths: {}", missing.join(", ")));
    let oversized_note = (!oversized.is_empty()).then(|| {
        format!(
            "Searched only the first {}MB of large files (matches past the {}MB window are not shown; use `read` for the rest): {}",
            MAX_FILE_BYTES / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024),
            oversized.join(", ")
        )
    });
    let warning = [missing_note, oversized_note]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");

    if window.is_empty() {
        let no_match = if can_paginate && skip > 0 && total_files > 0 && skip_files >= total_files {
            format!(
                "No more results ({total_files_label} files total; skip={skip} is past the end)"
            )
        } else {
            "No matches found".to_string()
        };
        let text = if warning.is_empty() {
            no_match
        } else {
            format!("{no_match}\n{warning}")
        };
        let mut details = serde_json::json!({
            "scopePath": scope_path,
            "searchPath": search_path,
            "cwd": env.cwd.display().to_string(),
            "matchCount": 0,
            "fileCount": 0,
            "files": [],
            "fileMatches": [],
            "truncated": false,
            "displayContent": "",
        });
        if !missing.is_empty() {
            details["missingPaths"] = serde_json::json!(missing);
        }
        return Ok(with_details(text, details));
    }

    // Render per file; hashline snapshot tags bind to the displayed rows.
    let render = |h: &FileHits| -> (Vec<String>, Vec<String>, Option<String>) {
        let key = canonical_key(&h.absolute);
        let tag = if env.hashline {
            env.store.record_file(&key, None)
        } else {
            None
        };
        let mut out: Vec<String> = Vec::new();
        let mut disp: Vec<String> = Vec::new();
        let mut last: Option<u64> = None;
        let gutter_w = h
            .matches
            .iter()
            .map(|m| m.line_number.to_string().len())
            .max()
            .unwrap_or(1)
            + 1;
        for m in &h.matches {
            if last.is_some_and(|l| m.line_number > l + 1) {
                out.push("...".into());
                disp.push(format!("{:gutter_w$}\u{2502}...", ""));
            }
            let marker = if m.matched { '*' } else { ' ' };
            // TS `formatCodeFrameLine`: `*2│beta` / ` 1│alpha`, width-padded.
            let gutter = if m.matched {
                format!("*{}", m.line_number)
            } else {
                m.line_number.to_string()
            };
            disp.push(format!("{gutter:>gutter_w$}\u{2502}{}", m.line));
            if tag.is_some() {
                out.push(format!("{marker}{}:{}", m.line_number, m.line));
            } else {
                out.push(format!("{marker}{}|{}", m.line_number, m.line));
            }
            last = Some(m.line_number);
        }
        if let Some(t) = &tag {
            env.store
                .record_seen_lines(&key, t, &seen_lines_from_body(&out.join("\n")));
        }
        (out, disp, tag)
    };
    let mut model: Vec<String> = Vec::new();
    let mut display: Vec<String> = Vec::new();
    if is_multi_scope {
        let entries: Vec<(String, bool)> =
            window.iter().map(|h| (h.shown.clone(), false)).collect();
        type RenderedBlock = (Vec<String>, Vec<String>, Option<String>);
        let rendered: HashMap<usize, RenderedBlock> = window
            .iter()
            .enumerate()
            .map(|(i, h)| (i, render(h)))
            .collect();
        let mut emitted = false;
        for e in events(&entries) {
            let hashes = "#".repeat(e.depth + 1);
            if emitted && (e.depth == 0 || e.kind == Kind::Dir) {
                model.push(String::new());
                display.push(String::new());
            }
            match e.kind {
                Kind::Dir => {
                    model.push(format!("{hashes} {}/", e.name));
                    display.push(format!("{hashes} {}/", e.name));
                    emitted = true;
                }
                Kind::File => {
                    let (lines, disp, tag) = &rendered[&e.key];
                    if lines.is_empty() {
                        // Skipped files leave no header; undo the separator.
                        if model.last().is_some_and(String::is_empty) {
                            model.pop();
                            display.pop();
                        }
                        continue;
                    }
                    emitted = true;
                    let suffix = tag.as_ref().map(|t| format!("#{t}")).unwrap_or_default();
                    model.push(format!("{hashes} {}{suffix}", e.name));
                    display.push(format!("{hashes} {}{suffix}", e.name));
                    model.extend(lines.iter().cloned());
                    display.extend(disp.iter().cloned());
                }
            }
        }
    } else {
        for h in &window {
            let (lines, disp, tag) = render(h);
            if lines.is_empty() {
                continue;
            }
            if !model.is_empty() {
                model.push(String::new());
                display.push(String::new());
            }
            if let Some(t) = tag {
                model.push(format!("[{}#{t}]", h.shown));
            }
            model.extend(lines);
            display.extend(disp);
        }
    }
    if let Some(m) = limit_message {
        model.push(String::new());
        model.push(m);
    }
    if !warning.is_empty() {
        model.push(String::new());
        model.push(warning);
    }
    let raw = model.join("\n");
    let t = truncate_head(
        &raw,
        if crate::methodology::raw_tool_output_requested() {
            usize::MAX
        } else {
            DEFAULT_MAX_BYTES
        },
    );
    let mut text = t.content.clone();
    let mut notices: Vec<String> = Vec::new();
    if t.truncated {
        notices.push(format!(
            "Showing lines 1-{} of {} ({} limit)",
            t.output_lines,
            t.total_lines,
            format_bytes(t.max_bytes)
        ));
    }
    if lines_truncated {
        notices.push(format!("Some lines truncated to {MAX_COLUMN} bytes"));
    }
    if !notices.is_empty() {
        let _ = write!(text, "\n\n[{}]", notices.join(". "));
    }
    let match_total: usize = window
        .iter()
        .map(|h| h.matches.iter().filter(|m| m.matched).count())
        .sum();
    let files: Vec<String> = window.iter().map(|h| h.shown.clone()).collect();
    let file_matches: Vec<serde_json::Value> = window
        .iter()
        .map(|h| {
            serde_json::json!({
                "path": h.shown,
                "count": h.matches.iter().filter(|m| m.matched).count(),
            })
        })
        .collect();
    let mut details = serde_json::json!({
        "scopePath": scope_path,
        "searchPath": search_path,
        "cwd": env.cwd.display().to_string(),
        "matchCount": match_total,
        "fileCount": files.len(),
        "files": files,
        "fileMatches": file_matches,
        "truncated": file_limit_reached || limit_reached || t.truncated || lines_truncated,
        "displayContent": display.join("\n"),
    });
    if file_limit_reached {
        details["fileLimitReached"] = serde_json::json!(DEFAULT_FILE_LIMIT);
    }
    if lines_truncated {
        details["linesTruncated"] = serde_json::json!(true);
    }
    if !missing.is_empty() {
        details["missingPaths"] = serde_json::json!(missing);
    }
    Ok(with_details(text, details))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    async fn run_glob(env: &Arc<Env>, args: serde_json::Value) -> Result<AgentToolResult> {
        glob(env.clone(), args, AbortSignal::never()).await
    }

    async fn run_grep(env: &Arc<Env>, args: serde_json::Value) -> Result<AgentToolResult> {
        grep(env.clone(), args, AbortSignal::never()).await
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src/inner")).unwrap();
        std::fs::create_dir_all(p.join("ignored")).unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(p.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(p.join("src/inner/b.rs"), "// beta here\nlet x = 1;\n").unwrap();
        std::fs::write(p.join("ignored/c.rs"), "fn beta_ignored() {}\n").unwrap();
        std::fs::write(p.join("notes.md"), "beta in notes\n").unwrap();
        dir
    }

    fn env_of(dir: &tempfile::TempDir) -> Arc<Env> {
        Arc::new(Env::new(dir.path(), true))
    }

    #[test]
    fn glob_search_path_scopes_pattern_under_path() {
        // The reference's runtime-only `pattern` key (`globSearchPath`).
        let args = serde_json::json!({"path": "src", "pattern": "**/*.ts"});
        assert_eq!(glob_search_path(&args).as_deref(), Some("src/**/*.ts"));
        // Several paths x several patterns, deduplicated.
        let args = serde_json::json!({"path": "a;b", "pattern": "*.rs;*.ts"});
        assert_eq!(
            glob_search_path(&args).as_deref(),
            Some("a/*.rs;a/*.ts;b/*.rs;b/*.ts")
        );
        // Absolute, `~`-rooted and URL patterns stay standalone.
        let args = serde_json::json!({"path": "src", "pattern": "/abs/*.ts;omp://**/*.md"});
        assert_eq!(
            glob_search_path(&args).as_deref(),
            Some("/abs/*.ts;omp://**/*.md")
        );
        // No `path`: the patterns are the path list, as the reference does.
        let args = serde_json::json!({"pattern": "*.ts"});
        assert_eq!(glob_search_path(&args).as_deref(), Some("*.ts"));
        // No `pattern`: plain `path` passthrough.
        let args = serde_json::json!({"path": "src"});
        assert_eq!(glob_search_path(&args).as_deref(), Some("src"));
    }

    #[test]
    fn find_pattern_parsing_matches_omp() {
        let p = parse_find_pattern("**/*.json");
        assert_eq!(
            (p.base.as_str(), p.glob.as_str(), p.has_glob),
            (".", "**/*.json", true)
        );
        let p = parse_find_pattern("*.ts");
        assert_eq!((p.base.as_str(), p.glob.as_str()), (".", "**/*.ts"));
        let p = parse_find_pattern("/abs/path/**/*.ts");
        assert_eq!((p.base.as_str(), p.glob.as_str()), ("/abs/path", "**/*.ts"));
        let p = parse_find_pattern("src/app");
        assert_eq!(
            (p.base.as_str(), p.glob.as_str(), p.has_glob),
            ("src/app", "**/*", false)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn glob_lists_grouped_respecting_gitignore() {
        let dir = fixture();
        let env = env_of(&dir);
        let out = text_of(
            &run_glob(&env, serde_json::json!({"path": "**/*.rs"}))
                .await
                .unwrap(),
        );
        assert!(out.contains("a.rs") && out.contains("b.rs"), "{out}");
        assert!(!out.contains("c.rs"), "gitignored file leaked: {out}");
        assert!(out.contains("# src/"), "{out}");
        let none = text_of(
            &run_glob(&env, serde_json::json!({"path": "*.zzz"}))
                .await
                .unwrap(),
        );
        assert_eq!(none, "No files found matching pattern");
        let err = run_glob(&env, serde_json::json!({"path": "missing/dir"}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Path not found: missing/dir");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn glob_and_grep_emit_card_details() {
        let dir = fixture();
        let env = env_of(&dir);
        let g = run_glob(&env, serde_json::json!({"path": "**/*.rs"}))
            .await
            .unwrap();
        assert_eq!(g.details["fileCount"], 2, "{:?}", g.details);
        assert_eq!(g.details["scopePath"], ".");
        assert_eq!(g.details["files"].as_array().unwrap().len(), 2);
        let r = run_grep(&env, serde_json::json!({"pattern": "beta", "path": "."}))
            .await
            .unwrap();
        assert_eq!(r.details["matchCount"], 3, "{:?}", r.details);
        assert_eq!(r.details["fileCount"], 3);
        let display = r.details["displayContent"].as_str().unwrap();
        assert!(display.contains("*2\u{2502}fn beta() {}"), "{display}");
        assert!(display.starts_with("# "), "{display}");
        let none = run_grep(&env, serde_json::json!({"pattern": "zzzz", "path": "."}))
            .await
            .unwrap();
        assert_eq!(none.details["matchCount"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn glob_limit_notice() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let env = env_of(&dir);
        let out = text_of(
            &run_glob(&env, serde_json::json!({"path": "*.txt", "limit": 3}))
                .await
                .unwrap(),
        );
        assert!(
            out.ends_with("[3 results limit reached. Use limit=6 for more]"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_directory_scope_groups_files_with_tags_and_context() {
        let dir = fixture();
        let env = env_of(&dir);
        let out = text_of(
            &run_grep(&env, serde_json::json!({"pattern": "beta"}))
                .await
                .unwrap(),
        );
        assert!(
            out.contains("## a.rs#") || out.contains("# a.rs#") || out.contains("a.rs#"),
            "{out}"
        );
        assert!(out.contains("*2:fn beta() {}"), "{out}");
        assert!(
            out.contains(" 1:fn alpha() {}"),
            "context row missing: {out}"
        );
        assert!(out.contains("*1:// beta here"), "{out}");
        assert!(!out.contains("beta_ignored"), "{out}");
        assert!(out.contains("notes.md"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_single_file_scope_flat_with_header() {
        let dir = fixture();
        let env = env_of(&dir);
        let out = text_of(
            &run_grep(
                &env,
                serde_json::json!({"pattern": "alpha", "path": "src/a.rs"}),
            )
            .await
            .unwrap(),
        );
        assert!(out.starts_with("[src/a.rs#"), "{out}");
        assert!(out.contains("*1:fn alpha() {}"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_case_flag_no_match_and_invalid_input() {
        let dir = fixture();
        let env = env_of(&dir);
        let none = text_of(
            &run_grep(&env, serde_json::json!({"pattern": "ALPHA"}))
                .await
                .unwrap(),
        );
        assert_eq!(none, "No matches found");
        let some = text_of(
            &run_grep(&env, serde_json::json!({"pattern": "ALPHA", "case": false}))
                .await
                .unwrap(),
        );
        assert!(some.contains("alpha"), "{some}");
        assert_eq!(
            run_grep(&env, serde_json::json!({"pattern": "  "}))
                .await
                .unwrap_err()
                .to_string(),
            "Pattern must not be empty"
        );
        // unbalanced paren is escaped instead of failing
        let paren = text_of(
            &run_grep(&env, serde_json::json!({"pattern": "alpha("}))
                .await
                .unwrap(),
        );
        assert!(paren.contains("No matches found") || paren.contains("alpha"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_line_range_selector_filters_matches() {
        let dir = fixture();
        let env = env_of(&dir);
        let out = text_of(
            &run_grep(
                &env,
                serde_json::json!({"pattern": "fn", "path": "src/a.rs:2-2"}),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("*2:fn beta() {}"), "{out}");
        assert!(!out.contains("*1:fn alpha"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_glob_scope_and_bare_glob_any_depth() {
        let dir = fixture();
        let env = env_of(&dir);
        let out = text_of(
            &run_grep(&env, serde_json::json!({"pattern": "beta", "path": "*.rs"}))
                .await
                .unwrap(),
        );
        assert!(out.contains("a.rs") && out.contains("b.rs"), "{out}");
        assert!(!out.contains("notes.md"), "{out}");
        let direct = text_of(
            &run_grep(
                &env,
                serde_json::json!({"pattern": "beta", "path": "src/*.rs"}),
            )
            .await
            .unwrap(),
        );
        assert!(
            direct.contains("a.rs") && !direct.contains("b.rs"),
            "{direct}"
        );
    }
}
