//! `find`: behavior search — describe what code does, get files and line
//! ranges. omp's tool spends a judge (LLM) cascade over a lexical prior; omp-rs
//! has no judge provider yet, so this is the lexical stage alone: files are
//! ranked by case-insensitive keyword occurrences (omp `jfind/lexical.ts`),
//! scored as a 0–1 probability, and rendered in omp's exact report format.
//!
//! The walk is [`omp_xfs::walk`] over an [`omp_vfs::Fs`]: host roots take the
//! native fast path, `scheme://` roots are listed and read through the vfs
//! provider in place, and dropping the search future cancels the walk.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use futures::{stream, StreamExt};
use omp_urls::{Registry, UrlCtx, UrlTarget};
use omp_vfs::{
    BlockingFs, DirEntry, FileHandle, FileKind, FileSystem, Metadata, Permissions, ReadDir,
};
use omp_xfs::walk::{self, Action, Options};
use omp_xfs::{FileType, Fs};
use pa_agent::abort::{aborted_error, throw_if_aborted, AbortSignal};
use pa_agent::types::AgentToolResult;
use tokio_util::sync::CancellationToken;

use super::util::{format_bytes, format_path_relative_to_cwd, resolve_to_cwd, str_arg};
use super::Env;

/// Ranges shown per hit in the model-facing text (omp `RANGES_SHOWN`).
const RANGES_SHOWN: usize = 3;
/// Files listed in the report.
const FILES_SHOWN: usize = 10;
/// Below this score hits are weak evidence (omp's τ).
const THRESHOLD: f64 = 0.40;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const SNIPPET_CHARS: usize = 100;

struct Hit {
    rel: String,
    score: f64,
    lines_seen: usize,
    ranges: Vec<(u32, u32, String)>, // start, end (1-indexed), snippet
}

/// omp `jfind/text.ts` keyword extraction, applied to `grep_keywords` and the
/// plain-language query: lowercase, quoted phrases kept whole.
fn keywords(query: &str, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |kw: String| {
        let kw = kw.trim().to_lowercase();
        if !kw.is_empty() && !out.contains(&kw) {
            out.push(kw);
        }
    };
    for kw in extra {
        push(kw.clone());
    }
    // Quoted phrases match whole.
    let mut rest = query;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('"') {
            push(after[..end].to_string());
            rest = &after[end + 1..];
        } else {
            rest = after;
            break;
        }
    }
    for tok in rest.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.') {
        push(tok.to_string());
    }
    out
}

/// Score from occurrence count: 1 - 1/(1+n), a 0–1 probability-ish scale.
fn score(count: usize) -> f64 {
    1.0 - 1.0 / (1.0 + f64::from(u32::try_from(count).unwrap_or(u32::MAX)))
}

/// Display spelling of `path`: cwd-relative for host paths, the URL verbatim
/// for internal URLs (`omp://`, `local://`, …), which no host path describes.
fn display_path(path: &Path, cwd: &Path, trailing_slash: bool) -> String {
    if omp_vfs::is_virtual_path(path) {
        let mut display = path.display().to_string();
        if trailing_slash && !display.ends_with('/') {
            display.push('/');
        }
        display
    } else {
        format_path_relative_to_cwd(path, cwd, trailing_slash)
    }
}

// ------------------------------------------------------------ URL filesystem

/// What an internal URL names, once the registry has routed it.
enum UrlNode {
    /// A file-backed scheme's entry on the host (`local://`, `skill://`, …).
    Host(PathBuf),
    /// A virtual scheme's rendered resource (`omp://`, `cfg://`, `rule://`, …).
    Bytes(Vec<u8>),
    /// A virtual scheme's root, which lists its scheme's completions.
    Directory,
}

fn virtual_metadata(kind: FileKind, len: u64, mode: u32) -> Metadata {
    Metadata::new(
        omp_vfs::FileType::from_kind(kind),
        len,
        Permissions::from_mode(mode),
    )
}

fn host_error(error: &anyhow::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, error.to_string())
}

/// An [`omp_vfs::FileSystem`] over the internal URL registry, so a `scheme://`
/// scope is listed and read in place instead of being mistaken for a host path.
///
/// File-backed schemes resolve to their host backing entry per operation;
/// virtual schemes expose the resolved resource as one read-only file, and a
/// resource the provider reports as a directory lists that scheme's
/// completions as children (the docs corpus under `omp://` is flat, so every
/// such directory is a scheme root).
struct UrlFs {
    registry: &'static Registry,
    ctx: UrlCtx,
}

impl std::fmt::Debug for UrlFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlFs").finish_non_exhaustive()
    }
}

impl UrlFs {
    fn resolve(&self, path: &Path) -> std::io::Result<UrlNode> {
        let text = path.to_str().ok_or_else(omp_vfs::invalid_argument)?;
        let target = self
            .registry
            .target(text, &self.ctx)
            .map_err(|error| host_error(&error))?
            .ok_or_else(|| host_error(&anyhow::anyhow!("not an internal URL: {text}")))?;
        match target {
            UrlTarget::File { path, .. } => Ok(UrlNode::Host(path)),
            UrlTarget::Resource { url, .. } => {
                let resource = self
                    .registry
                    .resolve(&url.raw_href, &self.ctx)
                    .map_err(|error| host_error(&error))?;
                if resource.is_directory {
                    Ok(UrlNode::Directory)
                } else {
                    Ok(UrlNode::Bytes(resource.content.into_bytes()))
                }
            }
        }
    }
}

/// A read-only in-memory [`FileHandle`] over one virtual resource.
#[derive(Debug)]
struct ResourceFile {
    bytes: Vec<u8>,
    pos: parking_lot::Mutex<u64>,
}

impl ResourceFile {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }
}

#[async_trait]
impl FileHandle for ResourceFile {
    async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(*self.pos.lock())
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        let len = buf.len().min(self.bytes.len() - start);
        buf[..len].copy_from_slice(&self.bytes[start..start + len]);
        *self.pos.lock() += u64::try_from(len).unwrap_or(u64::MAX);
        Ok(len)
    }

    async fn write(&self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(omp_vfs::unsupported("write"))
    }

    async fn seek(&self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let end = i64::try_from(self.bytes.len()).unwrap_or(i64::MAX);
        let mut cursor = self.pos.lock();
        let current = i64::try_from(*cursor).unwrap_or(i64::MAX);
        let next = match pos {
            std::io::SeekFrom::Start(n) => {
                i64::try_from(n).map_err(|_| omp_vfs::invalid_argument())?
            }
            std::io::SeekFrom::End(n) => end.saturating_add(n),
            std::io::SeekFrom::Current(n) => current.saturating_add(n),
        };
        *cursor = u64::try_from(next).map_err(|_| omp_vfs::invalid_argument())?;
        Ok(*cursor)
    }

    async fn metadata(&self) -> std::io::Result<Metadata> {
        Ok(virtual_metadata(FileKind::File, self.len(), 0o444))
    }
}

#[async_trait]
impl FileSystem for UrlFs {
    async fn open(
        &self,
        path: &Path,
        _options: &omp_vfs::OpenOptions,
    ) -> std::io::Result<omp_vfs::File> {
        match self.resolve(path)? {
            UrlNode::Host(host) => std::fs::File::open(host).map(omp_vfs::File::from),
            UrlNode::Bytes(bytes) => Ok(omp_vfs::File::from_handle(ResourceFile {
                bytes,
                pos: parking_lot::Mutex::new(0),
            })),
            UrlNode::Directory => Err(omp_vfs::is_a_directory()),
        }
    }

    async fn metadata(&self, path: &Path) -> std::io::Result<Metadata> {
        match self.resolve(path)? {
            UrlNode::Host(host) => std::fs::metadata(host).map(Metadata::from),
            UrlNode::Bytes(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                Ok(virtual_metadata(FileKind::File, len, 0o444))
            }
            UrlNode::Directory => Ok(virtual_metadata(FileKind::Dir, 0, 0o555)),
        }
    }

    async fn read_dir(&self, path: &Path) -> std::io::Result<ReadDir> {
        match self.resolve(path)? {
            UrlNode::Host(host) => {
                let mut entries = Vec::new();
                for entry in std::fs::read_dir(host)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let file_type = entry.file_type()?;
                    let kind = if file_type.is_dir() {
                        FileKind::Dir
                    } else if file_type.is_symlink() {
                        FileKind::Symlink
                    } else {
                        FileKind::File
                    };
                    let len = match kind {
                        FileKind::File => entry.metadata().map_or(0, |m| m.len()),
                        _ => 0,
                    };
                    let child = omp_vfs::child_path(path, &name);
                    entries.push(Ok(DirEntry::new(
                        child,
                        name,
                        Some(omp_vfs::FileType::from_kind(kind)),
                    )
                    .with_metadata(virtual_metadata(kind, len, 0o644))));
                }
                Ok(ReadDir::from_entries(entries))
            }
            UrlNode::Directory => {
                let scheme = omp_vfs::url_scheme(path).unwrap_or_default();
                let Some(provider) = self.registry.get(scheme) else {
                    return Err(omp_vfs::unsupported("directory listing"));
                };
                let entries = provider
                    .complete("", &self.ctx)
                    .into_iter()
                    .map(|candidate| {
                        let name = std::ffi::OsString::from(&candidate.label);
                        let child = omp_vfs::child_path(path, &name);
                        Ok(DirEntry::new(
                            child,
                            name,
                            Some(omp_vfs::FileType::from_kind(FileKind::File)),
                        ))
                    })
                    .collect::<Vec<_>>();
                Ok(ReadDir::from_entries(entries))
            }
            UrlNode::Bytes(_) => Err(omp_vfs::not_a_directory()),
        }
    }
}

/// Every regular file below `root`, in path order: hidden entries, `.git`,
/// and `node_modules` pruned, `.ignore`/`.gitignore` honoured.
async fn walk_files(fs: &Fs, root: &Path) -> Result<Vec<PathBuf>> {
    let options = Options {
        hidden: false,
        gitignore: true,
        skip_git: true,
        skip_node_modules: true,
        ..Options::default()
    };
    let found = walk::collect(fs, root, &options, true, |_, entry| {
        if entry.file_type == FileType::File {
            Action::Continue
        } else {
            Action::Skip
        }
    })
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(found
        .iter()
        .map(|entry| fs.join(root, entry.path))
        .collect())
}

/// Judge one file against the keywords: `(hit, files_read, file_bytes)` —
/// counters are zero when the file is skipped (too large, binary, unreadable)
/// or cancelled mid-scan.
fn judge_file(
    fs: &BlockingFs,
    file: &Path,
    keywords: &[String],
    cwd: &Path,
    cancel: &CancellationToken,
) -> (Option<Hit>, usize, usize) {
    if cancel.is_cancelled() {
        return (None, 0, 0);
    }
    let Ok(meta) = fs.metadata(file) else {
        return (None, 0, 0);
    };
    if meta.len() > MAX_FILE_BYTES {
        return (None, 0, 0);
    }
    let Ok(bytes) = fs.read(file) else {
        return (None, 0, 0);
    };
    if bytes.contains(&0) {
        return (None, 0, 0);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut matched_lines: Vec<(u32, &str)> = Vec::new();
    let mut count = 0usize;
    for (i, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        let line_hits = keywords
            .iter()
            .filter(|kw| lower.contains(kw.as_str()))
            .count();
        if line_hits > 0 {
            count += line_hits;
            matched_lines.push((u32::try_from(i + 1).unwrap_or(u32::MAX), line.trim()));
        }
    }
    let file_score = score(count);
    if file_score < THRESHOLD {
        return (None, 1, bytes.len());
    }
    // Contiguous matched lines become ranges, strongest (most hits) first.
    let mut ranges: Vec<(u32, u32, usize)> = Vec::new();
    for (ln, _) in &matched_lines {
        if let Some(last) = ranges.last_mut() {
            if *ln == last.1 + 1 {
                last.1 = *ln;
                last.2 += 1;
                continue;
            }
        }
        ranges.push((*ln, *ln, 1));
    }
    ranges.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut hit_ranges = Vec::new();
    for (start, end, _) in ranges.into_iter().take(RANGES_SHOWN) {
        let snippet = matched_lines
            .iter()
            .find(|(ln, _)| *ln == start)
            .map_or("", |(_, l)| l)
            .chars()
            .take(SNIPPET_CHARS)
            .collect::<String>();
        hit_ranges.push((start, end, snippet));
    }
    (
        Some(Hit {
            rel: display_path(file, cwd, false),
            score: file_score,
            lines_seen: text.lines().count(),
            ranges: hit_ranges,
        }),
        1,
        bytes.len(),
    )
}

/// Run the `find` tool: the lexical stage of omp's judge cascade.
///
/// Cancellation is immediate on both edges: the `select!` below abandons the
/// search future on abort (dropping the [`omp_xfs::walk`] future cancels the
/// walk) while the watcher fires the token the per-file judges poll, so at
/// most the files already being read finish.
///
/// # Errors
/// Empty query, an unresolvable scope path, or an aborted call.
pub async fn find(
    env: Arc<Env>,
    args: serde_json::Value,
    signal: AbortSignal,
) -> Result<AgentToolResult> {
    throw_if_aborted(&signal)?;
    let cancel = CancellationToken::new();
    let watcher = {
        let cancel = cancel.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            signal.aborted().await;
            cancel.cancel();
        })
    };
    let outcome = tokio::select! {
        biased;
        () = signal.aborted() => Err(aborted_error()),
        result = search(env, args, cancel.clone()) => result,
    };
    watcher.abort();
    cancel.cancel();
    if signal.is_aborted() {
        return Err(aborted_error());
    }
    outcome
}

/// [`find`]'s body, over a cancel token the caller fires when the call aborts.
///
/// Per-file work (read + keyword scan) is CPU-bound, so it runs on the
/// blocking pool, `workers` files at a time; hits are collected in input order
/// and sorted afterwards, so the report depends only on the multiset of
/// judgments, never on scheduling.
///
/// # Errors
/// Empty query or an unresolvable scope path.
#[allow(clippy::too_many_lines)]
async fn search(
    env: Arc<Env>,
    args: serde_json::Value,
    cancel: CancellationToken,
) -> Result<AgentToolResult> {
    let query = str_arg(&args, "query")?.trim().to_string();
    if query.is_empty() {
        bail!("`query` must be a non-empty description");
    }
    let extra: Vec<String> = args
        .get("grep_keywords")
        .and_then(serde_json::Value::as_array)
        .map_or_else(Vec::new, |a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        });
    let keywords: Arc<[String]> = Arc::from(keywords(&query, &extra));
    if keywords.is_empty() {
        bail!("`grep_keywords` must contain at least one term");
    }

    let scope_input = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    // omp `resolveSearchBase`: a registered internal URL is searched as spelled
    // (single-slash alias normalized) through the URL filesystem; anything else
    // resolves against the cwd as a host path.
    let registry = super::read::url_registry();
    let normalized = registry.normalize(scope_input.trim());
    let (root, vfs) = if scope_input.is_empty() {
        (env.cwd.clone(), omp_vfs::Fs::native())
    } else if registry.can_handle(&normalized) {
        (
            PathBuf::from(&normalized),
            omp_vfs::Fs::new(Arc::new(UrlFs {
                registry,
                ctx: super::read::url_ctx(&env),
            })),
        )
    } else {
        (resolve_to_cwd(scope_input, &env.cwd), omp_vfs::Fs::native())
    };
    let meta = vfs
        .metadata(&root)
        .await
        .map_err(|_| anyhow::anyhow!("Path not found: {scope_input}"))?;
    let scope_path = if root == env.cwd {
        None
    } else {
        Some(display_path(&root, &env.cwd, meta.is_dir()))
    };

    let started = std::time::Instant::now();
    let fs = Fs::new(&vfs, &root);
    let files: Vec<PathBuf> = if meta.is_file() {
        vec![root]
    } else {
        walk_files(&fs, &root).await?
    };
    let listed = files.len();

    // Spawned blocking judges run off the runtime's workers; `buffered` bounds
    // how many are in flight, and `collect` keeps them in input order.
    let blocking = vfs.blocking();
    let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let judged: Vec<(Option<Hit>, usize, usize)> = stream::iter(files)
        .map(|file| {
            let fs = blocking.clone();
            let keywords = Arc::clone(&keywords);
            let cwd = env.cwd.clone();
            let cancel = cancel.clone();
            tokio::task::spawn_blocking(move || {
                judge_file(&fs, &file, &keywords[..], &cwd, &cancel)
            })
        })
        .buffered(workers)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|joined| joined.map_err(|e| anyhow::anyhow!("find worker failed: {e}")))
        .collect::<Result<Vec<_>>>()?;

    let mut hits: Vec<Hit> = Vec::new();
    let mut files_read = 0usize;
    let mut file_bytes = 0usize;
    for (hit, read, bytes) in judged {
        files_read += read;
        file_bytes += bytes;
        if let Some(hit) = hit {
            hits.push(hit);
        }
    }

    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.rel.cmp(&b.rel)));
    hits.truncate(FILES_SHOWN);

    let where_ = scope_path
        .as_deref()
        .map_or_else(String::new, |p| format!(" in {p}"));
    let mut out = String::new();
    if hits.is_empty() {
        let _ = writeln!(out, "no hits for \"{query}\"{where_} (τ {THRESHOLD:.2})");
    } else {
        let _ = writeln!(
            out,
            "{} hit(s) for \"{query}\"{where_} (τ {THRESHOLD:.2}), strongest first",
            hits.len()
        );
        out.push('\n');
        for hit in &hits {
            let _ = writeln!(
                out,
                "{}  {:.2}  {} lines matched",
                hit.rel, hit.score, hit.lines_seen
            );
            for (start, end, snippet) in &hit.ranges {
                let span = if start == end {
                    format!("{start}")
                } else {
                    format!("{start}-{end}")
                };
                let _ = writeln!(out, "  {}:{span}  {:.2}  {snippet}", hit.rel, hit.score);
            }
        }
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "listed {listed} · judged 0 · read {files_read} files ({}) · 0 requests · 0 tokens · $0.0000 · {} wall / 0:00 api",
        format_bytes(file_bytes),
        super::jobs::format_duration(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
    );
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let hit_values: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "rel": h.rel,
                "contentScore": h.score,
                "linesSeen": h.lines_seen,
                "ranges": h.ranges.iter().map(|(start, end, snippet)| serde_json::json!({
                    "start": start,
                    "end": end,
                    "p": h.score,
                    "snippet": snippet,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut details = serde_json::json!({
        "query": query,
        "keywords": &keywords[..],
        "threshold": THRESHOLD,
        "cwd": env.cwd.display().to_string(),
        "elapsedMs": elapsed_ms,
        "hits": hit_values,
        "stats": {
            "filesRead": files_read,
            "inputTokens": 0,
            "cost": 0.0,
            "errors": 0,
            "failures": [],
        },
    });
    if let Some(p) = scope_path {
        details["scopePath"] = serde_json::json!(p);
    }
    let mut result = AgentToolResult::text(out.trim_end().to_string());
    result.details = details;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Env>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/alpha.rs"),
            "pub fn login_user() {\n    verify_password();\n    create_session();\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/beta.rs"),
            "pub fn logout() { destroy_session(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# demo\nnothing here\n").unwrap();
        let env = Arc::new(Env::new(dir.path(), false));
        (dir, env)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ranks_files_by_keyword_hits_in_omp_format() {
        let (_dir, env) = fixture();
        let r = find(
            env,
            serde_json::json!({"query": "user login session", "grep_keywords": ["session"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        let text = match &r.content[0] {
            pa_agent::types::ToolResultContent::Text(t) => t.text.clone(),
            pa_agent::types::ToolResultContent::Image(image) => {
                panic!("unexpected content: {image:?}")
            }
        };
        assert!(
            text.starts_with("2 hit(s) for \"user login session\" (τ 0.40), strongest first"),
            "{text}"
        );
        let alpha = text
            .lines()
            .find(|l| l.starts_with("src/alpha.rs"))
            .unwrap();
        assert!(alpha.contains("  0."), "{alpha}");
        // omp hit line format: `path:start-end score snippet`; matched lines
        // 1 and 3 are not contiguous, so they list as separate ranges.
        assert!(text.contains("  src/alpha.rs:1  "), "{text}");
        assert!(text.contains("  src/alpha.rs:3  "), "{text}");
        assert!(text.contains("pub fn login_user()"), "{text}");
        assert!(
            text.contains("listed 3 · judged 0 · read 3 files"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_search_reports_no_hits() {
        let (_dir, env) = fixture();
        let r = find(
            env,
            serde_json::json!({"query": "zzz absent topic", "grep_keywords": ["zznotthere"], "path": "src"}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        let text = match &r.content[0] {
            pa_agent::types::ToolResultContent::Text(t) => t.text.clone(),
            pa_agent::types::ToolResultContent::Image(image) => {
                panic!("unexpected content: {image:?}")
            }
        };
        assert!(
            text.starts_with("no hits for \"zzz absent topic\" in src/ (τ 0.40)"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_empty_query_and_scope() {
        let (_dir, env) = fixture();
        let e = find(
            Arc::clone(&env),
            serde_json::json!({"query": "  ", "grep_keywords": ["x"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "`query` must be a non-empty description");
        let e = find(
            env,
            serde_json::json!({"query": "x", "grep_keywords": ["x"], "path": "nope"}),
            AbortSignal::never(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "Path not found: nope");
    }
}
