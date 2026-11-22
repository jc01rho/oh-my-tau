//! `memory://` protocol handler (TS `internal-urls/memory-protocol.ts`).
//!
//! URL forms:
//! - `memory://root` — the project memory summary (`memory_summary.md`)
//! - `memory://root/<path>` — any relative file under the project memory root
//! - `memory://<id>` — one stored memory row, rendered with YAML frontmatter
//!
//! The file-backed half needs nothing but the filesystem; the row half reads
//! the book installed by [`set_memory_records`], which is what a memory backend
//! publishes once it has opened its banks.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::{InternalResource, InternalUrl, UrlCompletion, UrlCtx, UrlProvider};

use super::{content_type_for_path, format_directory_listing, validate_relative_path};

/// Namespace owning the file-backed project memory root.
pub const MEMORY_NAMESPACE: &str = "root";

/// File read by a bare `memory://root`.
pub const DEFAULT_MEMORY_FILE: &str = "memory_summary.md";

/// Prompt doc line (TS `prompts/internal-urls/memory.md`, trimmed).
pub const PROMPT_DOC: &str = "memory://root: project-memory summary.";

/// Error for a project whose memory root was never produced.
pub const ROOT_UNAVAILABLE: &str =
    "Memory artifacts are not available for this project yet. Run a session with memories enabled first.";

/// One stored memory row addressable as `memory://<id>` (TS `MemoryRow`).
#[derive(Debug, Clone, Default)]
pub struct MemoryRecord {
    pub id: String,
    /// Bank the row was stored in.
    pub bank: String,
    /// `working` or `episodic` (TS `MemoryTier`).
    pub store: String,
    pub content: String,
    pub memory_type: Option<String>,
    pub source: Option<String>,
    pub timestamp: Option<String>,
    pub created_at: Option<String>,
    pub importance: Option<f64>,
    pub veracity: Option<String>,
    pub session_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Fact-tier rows are immutable (TS `immutable: true` for extracted facts).
    pub immutable: bool,
}

static MEMORY_RECORDS: RwLock<Option<Arc<Vec<MemoryRecord>>>> = RwLock::new(None);

/// Publish the memory rows reachable as `memory://<id>`. A memory backend calls
/// this after opening its banks; the empty book resolves no id.
pub fn set_memory_records(records: Vec<MemoryRecord>) {
    *MEMORY_RECORDS.write().expect("memory records lock") = Some(Arc::new(records));
}

/// The installed memory rows (empty when no backend published any).
pub fn memory_records() -> Arc<Vec<MemoryRecord>> {
    MEMORY_RECORDS
        .read()
        .expect("memory records lock")
        .clone()
        .unwrap_or_default()
}

/// Encode a cwd into its memory-root directory name (TS `encodeProjectPath`):
/// `/home/me/repo` → `--home-me-repo--`.
pub fn encode_project_path(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let stripped = raw.trim_start_matches(['/', '\\']);
    let slug: String = stripped
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{slug}--")
}

/// The project memory root for `cwd` under `agent_dir` (TS `getMemoryRoot`).
pub fn memory_root_for(agent_dir: &Path, cwd: &Path) -> PathBuf {
    agent_dir.join("memories").join(encode_project_path(cwd))
}

/// The memory root of the calling session: the context's explicit root, else
/// the default agent dir joined with the caller's cwd.
pub fn resolve_memory_root(ctx: &UrlCtx) -> PathBuf {
    if let Some(root) = &ctx.memory_root {
        return root.clone();
    }
    let agent_dir = omp_providers::AgentDir::default().0;
    let cwd = ctx
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    memory_root_for(&agent_dir, &cwd)
}

/// Strict percent-decoding: errors on a `%` that is not a `%XX` escape
/// (TS `decodeURIComponent` throwing).
fn decode_strict(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw
                .get(i + 1..i + 3)
                .ok_or_else(|| "truncated".to_string())?;
            let v = u8::from_str_radix(hex, 16).map_err(|_| "invalid".to_string())?;
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| "invalid".to_string())
}

/// The URL's namespace: `root` for the file-backed root, else a memory id.
fn namespace(url: &InternalUrl) -> Result<&str, String> {
    if url.host.is_empty() {
        return Err(
            "memory:// URL requires a namespace: memory://root or memory://<memory-id>".to_string(),
        );
    }
    Ok(&url.host)
}

/// True for `memory://root` with no path, which reads the default summary file.
fn is_bare(url: &InternalUrl) -> bool {
    url.pathname.is_empty() || url.pathname == "/"
}

/// Resolve a `memory://root[/<path>]` URL to an absolute path under `root`.
pub fn resolve_root_path(url: &InternalUrl, root: &Path) -> Result<PathBuf, String> {
    let raw_pathname = &url.pathname;
    if is_bare(url) {
        return Ok(root.join(DEFAULT_MEMORY_FILE));
    }
    let relative = decode_strict(raw_pathname.trim_start_matches('/'))
        .map_err(|_| format!("Invalid URL encoding in memory:// path: {}", url.raw_href))?;
    validate_relative_path(&relative, "memory")?;
    Ok(root.join(relative))
}

/// Look up one memory row by id; the first hit wins (TS first-hit-wins lookup).
pub fn find_record<'a>(id: &str, records: &'a [MemoryRecord]) -> Option<&'a MemoryRecord> {
    records.iter().find(|r| r.id == id)
}

/// Render a memory row as markdown with a YAML frontmatter header
/// (TS `renderMnemopiMemory`; the resource URL is not carried by
/// [`InternalResource`], so only the body is rendered).
pub fn render_record(record: &MemoryRecord) -> InternalResource {
    let mut header = String::from("---\n");
    header.push_str(&format!("id: {}\n", record.id));
    header.push_str(&format!("bank: {}\n", record.bank));
    header.push_str(&format!("store: {}\n", record.store));
    for (key, value) in [
        ("memory_type", record.memory_type.as_ref()),
        ("source", record.source.as_ref()),
        ("timestamp", record.timestamp.as_ref()),
        ("created_at", record.created_at.as_ref()),
        ("veracity", record.veracity.as_ref()),
        ("session_id", record.session_id.as_ref()),
    ] {
        if let Some(value) = value {
            header.push_str(&format!("{key}: {value}\n"));
        }
    }
    if let Some(importance) = record.importance {
        header.push_str(&format!("importance: {importance}\n"));
    }
    if let Some(metadata) = &record.metadata {
        header.push_str(&format!("metadata: {metadata}\n"));
    }
    header.push_str("---\n\n");
    InternalResource {
        content: format!("{header}{}", record.content),
        is_directory: false,
        immutable: Some(true),
        path: None,
    }
}

/// Resolve a `memory://` URL to a resource (TS `MemoryProtocolHandler.resolve`).
pub fn resolve(url: &InternalUrl, ctx: &UrlCtx) -> Result<InternalResource, String> {
    let namespace = namespace(url)?;
    if namespace != MEMORY_NAMESPACE {
        let records = memory_records();
        let Some(record) = find_record(namespace, &records) else {
            return Err(if records.is_empty() {
                format!(
                    "Unknown memory namespace: {namespace}. Supported: {MEMORY_NAMESPACE} (file-backed memory summary), or a memory id when a memory backend is active."
                )
            } else {
                format!(
                    "Mnemopi memory {namespace} not found in any scoped bank. Use `recall` to list available ids."
                )
            });
        };
        return Ok(render_record(record));
    }

    let root = resolve_memory_root(ctx);
    if !root.is_dir() {
        return Err(ROOT_UNAVAILABLE.to_string());
    }
    let target = resolve_root_path(url, &root)?;
    let meta = std::fs::metadata(&target)
        .map_err(|_| format!("Memory file not found: {}", url.raw_href))?;
    let path_str = target.to_string_lossy().into_owned();
    if meta.is_dir() {
        let entries: Vec<(String, bool)> = std::fs::read_dir(&target)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.path().is_dir(),
                )
            })
            .collect();
        return Ok(InternalResource {
            content: format_directory_listing(entries),
            is_directory: true,
            immutable: Some(true),
            path: Some(path_str),
        });
    }
    if !meta.is_file() {
        return Err(format!(
            "memory:// URL must resolve to a file or directory: {}",
            url.raw_href
        ));
    }
    let content = std::fs::read_to_string(&target).map_err(|e| e.to_string())?;
    let _ = content_type_for_path(&path_str);
    Ok(InternalResource {
        content,
        is_directory: false,
        immutable: Some(true),
        path: Some(path_str),
    })
}

/// File backing a `memory://root[/<path>]` URL, when it exists on disk.
pub fn locate(url: &InternalUrl, ctx: &UrlCtx) -> Option<PathBuf> {
    if url.host != MEMORY_NAMESPACE {
        return None;
    }
    let root = resolve_memory_root(ctx);
    let target = resolve_root_path(url, &root).ok()?;
    target.exists().then_some(target)
}

/// Autocomplete candidates (TS `complete`).
pub fn complete() -> Vec<UrlCompletion> {
    let mut completions = vec![UrlCompletion {
        label: MEMORY_NAMESPACE.to_string(),
        detail: Some("Project memory summary".to_string()),
    }];
    if !memory_records().is_empty() {
        completions.push(UrlCompletion {
            label: "<memory-id>".to_string(),
            detail: Some("Full memory by id (from recall)".to_string()),
        });
    }
    completions
}

/// Read-only `memory://` provider over the project memory root and stored rows.
#[derive(Default)]
pub struct MemoryProvider;

impl UrlProvider for MemoryProvider {
    fn scheme(&self) -> &'static str {
        "memory"
    }

    fn spec(&self) -> &crate::SchemeSpec {
        static SPEC: crate::SchemeSpec = crate::SchemeSpec {
            backing: crate::SchemeBacking::File,
            selectors: crate::SchemeSelectors::Lines,
            immutable: true,
            single_slash_alias: false,
            linkable: true,
            port_authority: false,
            write: None,
        };
        &SPEC
    }

    fn read(&self, url: &InternalUrl, ctx: &UrlCtx) -> anyhow::Result<InternalResource> {
        resolve(url, ctx).map_err(anyhow::Error::msg)
    }

    fn locate(&self, url: &InternalUrl, ctx: &UrlCtx) -> Option<PathBuf> {
        locate(url, ctx)
    }

    fn complete(&self, _query: &str, _ctx: &UrlCtx) -> Vec<UrlCompletion> {
        complete()
    }

    fn prompt_doc(&self) -> Option<String> {
        // TS advertises the file-backed namespace only for a backend that owns
        // it; with no backend threaded here, an existing project root is the
        // equivalent signal.
        resolve_memory_root(&UrlCtx::default())
            .is_dir()
            .then(|| PROMPT_DOC.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_internal_url;

    fn fixture(root: &Path) {
        std::fs::create_dir_all(root.join("skills/demo")).unwrap();
        std::fs::write(root.join(DEFAULT_MEMORY_FILE), "Project summary.\n").unwrap();
        std::fs::write(root.join("MEMORY.md"), "# Memory\n\nFacts.\n").unwrap();
        std::fs::write(root.join("skills/demo/SKILL.md"), "Skill body").unwrap();
    }

    fn ctx(root: &Path) -> UrlCtx {
        UrlCtx {
            memory_root: Some(root.to_path_buf()),
            ..UrlCtx::default()
        }
    }

    /// Serializes the tests that publish the process-global memory book.
    static GLOBAL_BOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_BOOK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn bare_root_reads_summary() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let url = parse_internal_url("memory://root").unwrap();
        let res = resolve(&url, &ctx(dir.path())).unwrap();
        assert_eq!(res.content, "Project summary.\n");
        assert!(!res.is_directory);
        assert_eq!(res.immutable, Some(true));
        assert!(res.path.unwrap().ends_with(DEFAULT_MEMORY_FILE));
    }

    #[test]
    fn root_paths_read_and_list() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let url = parse_internal_url("memory://root/MEMORY.md").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap().content,
            "# Memory\n\nFacts.\n"
        );
        // A trailing slash is still the bare URL: it reads the summary file.
        let url = parse_internal_url("memory://root/").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap().content,
            "Project summary.\n"
        );
        // A subdirectory lists its own entries.
        let url = parse_internal_url("memory://root/skills/demo").unwrap();
        assert_eq!(resolve(&url, &ctx(dir.path())).unwrap().content, "SKILL.md");
    }

    #[test]
    fn root_errors_match_ts() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let url = parse_internal_url("memory://").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap_err(),
            "memory:// URL requires a namespace: memory://root or memory://<memory-id>"
        );
        let url = parse_internal_url("memory://root/missing.md").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap_err(),
            "Memory file not found: memory://root/missing.md"
        );
        let url = parse_internal_url("memory://root/../escape.md").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap_err(),
            "Path traversal is not allowed"
        );
        // A project that never produced a memory root.
        let empty = tempfile::tempdir().unwrap();
        let url = parse_internal_url("memory://root").unwrap();
        assert_eq!(
            resolve(&url, &ctx(&empty.path().join("absent"))).unwrap_err(),
            ROOT_UNAVAILABLE
        );
    }

    #[test]
    fn memory_id_renders_frontmatter() {
        let _guard = lock_globals();
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        set_memory_records(vec![MemoryRecord {
            id: "m-1".into(),
            bank: "proj".into(),
            store: "working".into(),
            content: "Body text.".into(),
            memory_type: Some("fact".into()),
            source: Some("coding-agent-retain".into()),
            timestamp: Some("2026-11-22T00:00:00Z".into()),
            importance: Some(0.75),
            metadata: Some(serde_json::json!({ "k": "v" })),
            ..MemoryRecord::default()
        }]);
        let url = parse_internal_url("memory://m-1").unwrap();
        let res = resolve(&url, &ctx(dir.path())).unwrap();
        assert_eq!(
            res.content,
            "---\nid: m-1\nbank: proj\nstore: working\nmemory_type: fact\n\
             source: coding-agent-retain\ntimestamp: 2026-11-22T00:00:00Z\n\
             importance: 0.75\nmetadata: {\"k\":\"v\"}\n---\n\nBody text."
        );
        // The provider reads the same book, and offers it for completion.
        let provider = MemoryProvider;
        assert_eq!(
            provider.read(&url, &ctx(dir.path())).unwrap().content,
            res.content
        );
        let labels: Vec<String> = provider
            .complete("", &ctx(dir.path()))
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(labels, vec!["root".to_string(), "<memory-id>".to_string()]);
        // An id the book does not hold is a miss, not a namespace error.
        let url = parse_internal_url("memory://nope").unwrap();
        assert_eq!(
            resolve(&url, &ctx(dir.path())).unwrap_err(),
            "Mnemopi memory nope not found in any scoped bank. Use `recall` to list available ids."
        );
        set_memory_records(Vec::new());
    }

    #[test]
    fn empty_book_reports_unknown_namespace() {
        let _guard = lock_globals();
        set_memory_records(Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let url = parse_internal_url("memory://anything").unwrap();
        let err = resolve(&url, &ctx(dir.path())).unwrap_err();
        assert!(
            err.starts_with("Unknown memory namespace: anything."),
            "{err}"
        );
    }

    #[test]
    fn encode_project_path_matches_ts() {
        assert_eq!(
            encode_project_path(Path::new("/home/me/repo")),
            "--home-me-repo--"
        );
        // Both the drive colon and the separator map to `-`, like TS.
        assert_eq!(
            encode_project_path(Path::new(r"C:\work\proj")),
            "--C--work-proj--"
        );
    }

    #[test]
    fn memory_root_for_joins_agent_memories() {
        assert_eq!(
            memory_root_for(Path::new("/agent"), Path::new("/home/me/repo")),
            PathBuf::from("/agent/memories/--home-me-repo--")
        );
    }

    #[test]
    fn locate_only_backs_the_root_namespace() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let url = parse_internal_url("memory://root/MEMORY.md").unwrap();
        assert_eq!(
            locate(&url, &ctx(dir.path())).unwrap(),
            dir.path().join("MEMORY.md")
        );
        let url = parse_internal_url("memory://root/absent.md").unwrap();
        assert_eq!(locate(&url, &ctx(dir.path())), None);
        let url = parse_internal_url("memory://m-1").unwrap();
        assert_eq!(locate(&url, &ctx(dir.path())), None);
    }
}
