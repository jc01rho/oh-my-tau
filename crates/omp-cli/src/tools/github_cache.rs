//! SQLite cache for rendered `github` issue/PR/MR view output, plus the
//! cache-aware lookup wrapper the `github` tool shares with the `issue://` /
//! `pr://` / `mr://` internal-URL handlers.
//!
//! Port of omp `src/tools/github-cache.ts`. Storage is one process-wide
//! connection opened lazily on first hit; every helper swallows open/IO
//! failures and degrades to "no cache" so a corrupt or unreadable DB never
//! blocks a `gh` call.
//!
//! TTL contract (`get_or_fetch_view`):
//!   * within soft TTL -> return the cached row directly. An *open* issue/PR
//!     row stays fresh for at most [`OPEN_STATE_SOFT_TTL_MS`], because a merge
//!     or close can land at any moment.
//!   * issue/PR past soft TTL but within hard TTL -> refresh synchronously,
//!     falling back to the cached row when the live fetch fails.
//!   * `pr-diff` / `mr-diff` past soft TTL but within hard TTL -> return the
//!     cached row *and* schedule a single-flight background refresh.
//!   * past hard TTL -> treat as a miss and fetch fresh.
//!   * `?at=<generation>` pins an immutable `/review` snapshot, read through
//!     [`get_retained_generation`] with the same hard TTL counted from
//!     `retained_at`.
//!
//! The invalidation epoch is the non-obvious piece: every invalidation bumps a
//! counter, and a fetch that started before the bump returns its result but
//! does **not** store it, so a pre-mutation response cannot repopulate a row
//! the mutation just dropped. Fencing every in-flight fetch, not only the
//! matching rows, is deliberate: a skipped store costs one refetch.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A boxed `Send` future, so the cache stays independent of the agent loop.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ────────────────────────────────────────────────────────────────────────────
// Storage layer
// ────────────────────────────────────────────────────────────────────────────

/// Cache row kinds (`github_view_cache.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Issue,
    Pr,
    PrDiff,
    MrDiff,
}

impl CacheKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
            Self::PrDiff => "pr-diff",
            Self::MrDiff => "mr-diff",
        }
    }

    /// Only these kinds can be retained as a `/review` generation.
    #[must_use]
    pub fn is_generation_kind(self) -> bool {
        matches!(self, Self::Issue | Self::Pr | Self::PrDiff)
    }
}

impl std::str::FromStr for CacheKind {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, ()> {
        match value {
            "issue" => Ok(Self::Issue),
            "pr" => Ok(Self::Pr),
            "pr-diff" => Ok(Self::PrDiff),
            "mr-diff" => Ok(Self::MrDiff),
            _ => Err(()),
        }
    }
}

/// Rows written without an explicit credential namespace.
pub const DEFAULT_CACHE_AUTH_KEY: &str = "default";

/// One cached rendered view.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedView {
    pub auth_key: String,
    pub repo: String,
    pub kind: CacheKind,
    pub number: i64,
    pub include_comments: bool,
    pub fetched_at: i64,
    pub payload: Value,
    pub rendered: String,
    pub source_url: Option<String>,
}

/// A generation stored by [`retain_cached_generation`], with the time it was saved.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedGeneration {
    pub view: CachedView,
    pub retained_at: i64,
}

/// Fields accepted by [`put_cached`].
#[derive(Debug, Clone)]
pub struct PutCachedInput {
    pub auth_key: Option<String>,
    pub repo: String,
    pub kind: CacheKind,
    pub number: i64,
    pub include_comments: bool,
    pub payload: Value,
    pub rendered: String,
    pub source_url: Option<String>,
    pub fetched_at: Option<i64>,
}

static DB: Mutex<Option<Connection>> = Mutex::new(None);
static OPEN_ATTEMPTED: AtomicBool = AtomicBool::new(false);
/// Bumped by every invalidation (see the module header).
static INVALIDATION_EPOCH: AtomicU64 = AtomicU64::new(0);
static LAST_SWEEP_MS: AtomicI64 = AtomicI64::new(0);
/// Test hook: overrides [`github_cache_db_path`].
static DB_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Current wall clock in epoch milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// omp `getGithubCacheDbPath`: `OMP_GITHUB_CACHE_DB`, else `<home>/.omp/cache/github-cache.db`.
#[must_use]
pub fn github_cache_db_path() -> PathBuf {
    if let Some(path) = DB_PATH_OVERRIDE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
    {
        return path;
    }
    if let Some(path) = std::env::var_os("OMP_GITHUB_CACHE_DB").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    home_dir()
        .join(".omp")
        .join("cache")
        .join("github-cache.db")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Point the cache at `path` (test hook). Pass `None` to restore the default.
pub fn set_db_path_override(path: Option<PathBuf>) {
    *DB_PATH_OVERRIDE
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = path;
}

fn ensure_parent_dir(file_path: &Path) {
    let Some(dir) = file_path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

fn protect_db_files(db_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut paths = vec![db_path.to_path_buf()];
        for suffix in ["-wal", "-shm"] {
            paths.push(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        for path in paths {
            if let Ok(meta) = std::fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db_path;
}

/// omp `openDb`: install the busy handler *before* any lock-taking statement,
/// migrate pre-v4 tables by dropping them (the cache is regenerable), then
/// create the current schema. No eviction on open — the configured hard TTL is
/// enforced per lookup by `sweep_if_due`, since user settings are not loaded
/// when the connection first opens.
fn open_db() -> Option<Connection> {
    let db_path = github_cache_db_path();
    ensure_parent_dir(&db_path);
    let conn = Connection::open(&db_path).ok()?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .ok()?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");

    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if user_version < 4 {
        let _ = conn.execute_batch("DROP TABLE IF EXISTS github_view_cache;");
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS github_view_cache (
            auth_key         TEXT    NOT NULL,
            repo             TEXT    NOT NULL,
            kind             TEXT    NOT NULL CHECK (kind IN ('issue','pr','pr-diff','mr-diff')),
            number           INTEGER NOT NULL,
            include_comments INTEGER NOT NULL,
            fetched_at       INTEGER NOT NULL,
            payload          TEXT    NOT NULL,
            rendered         TEXT    NOT NULL,
            source_url       TEXT,
            PRIMARY KEY (auth_key, repo, kind, number, include_comments)
        );
        CREATE TABLE IF NOT EXISTS github_view_cache_generation (
            auth_key         TEXT    NOT NULL,
            repo             TEXT    NOT NULL,
            kind             TEXT    NOT NULL CHECK (kind IN ('issue','pr','pr-diff')),
            number           INTEGER NOT NULL,
            include_comments INTEGER NOT NULL,
            fetched_at       INTEGER NOT NULL,
            retained_at      INTEGER NOT NULL,
            payload          TEXT    NOT NULL,
            rendered         TEXT    NOT NULL,
            source_url       TEXT,
            PRIMARY KEY (auth_key, repo, kind, number, include_comments, fetched_at)
        );
        CREATE INDEX IF NOT EXISTS idx_github_view_cache_fetched ON github_view_cache(fetched_at);
        CREATE INDEX IF NOT EXISTS idx_github_view_cache_generation_retained
            ON github_view_cache_generation(retained_at);
        PRAGMA user_version = 4;",
    )
    .ok()?;
    protect_db_files(&db_path);
    Some(conn)
}

/// Run `f` against the process-wide connection, opening it on first use.
fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> Option<T> {
    let mut guard = DB.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_none() {
        if OPEN_ATTEMPTED.swap(true, Ordering::SeqCst) {
            return None;
        }
        *guard = open_db();
    }
    guard.as_ref().map(f)
}

fn evict_expired(conn: &Connection, hard_ttl_ms: i64) {
    let cutoff = now_ms().saturating_sub(hard_ttl_ms);
    let _ = conn.execute(
        "DELETE FROM github_view_cache WHERE fetched_at < ?1",
        params![cutoff],
    );
    let _ = conn.execute(
        "DELETE FROM github_view_cache_generation WHERE retained_at < ?1",
        params![cutoff],
    );
}

/// Throttle for the per-lookup configured-TTL sweep: one `DELETE` per minute is
/// enough to cap the on-disk exposure window at `hard_ttl + SWEEP_INTERVAL`.
pub const SWEEP_INTERVAL_MS: i64 = 60_000;

fn sweep_if_due(hard_ttl_ms: i64) {
    let now = now_ms();
    let last = LAST_SWEEP_MS.load(Ordering::SeqCst);
    if now.saturating_sub(last) < SWEEP_INTERVAL_MS {
        return;
    }
    if LAST_SWEEP_MS
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let _ = with_db(|conn| evict_expired(conn, hard_ttl_ms));
}

fn get_gh_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GH_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("gh");
    }
    home_dir().join(".config").join("gh")
}

fn hash_cache_identity(parts: &[String]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
        hasher.update(b"|");
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(20);
    for byte in digest.iter().take(10) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// glab config file, following glab's search order.
#[must_use]
pub fn get_glab_config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("GLAB_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("config.yml");
    }
    let home = home_dir();
    let legacy = home.join(".config").join("glab-cli").join("config.yml");
    let mut candidates = vec![legacy.clone()];
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(xdg).join("glab-cli").join("config.yml"));
    } else if cfg!(target_os = "macos") {
        candidates.push(
            home.join("Library")
                .join("Application Support")
                .join("glab-cli")
                .join("config.yml"),
        );
    }
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .unwrap_or(legacy)
}

const AUTH_KEY_TOKEN_ENV_VARS: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// Env vars glab reads tokens from; they apply to every host glab talks to.
pub const GITLAB_TOKEN_ENV_VARS: [&str; 3] = ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN"];

/// Memo key -> (env signature, config path, config mtime, value).
type AuthKeyMemo = Mutex<Vec<(String, String, PathBuf, i64, Option<String>)>>;
static AUTH_KEY_MEMO: AuthKeyMemo = Mutex::new(Vec::new());

fn resolve_cli_cache_auth_key(
    memo_key: &str,
    host: &str,
    token_env_vars: &[&str],
    config_path: &Path,
    config_label: &str,
) -> Option<String> {
    let mut env_sig = String::new();
    for name in token_env_vars {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                env_sig.push_str(&format!("{name}={}:{value}\0", value.len()));
            }
        }
    }
    let config_mtime_ms = std::fs::metadata(config_path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(-1, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

    {
        let memo = AUTH_KEY_MEMO.lock().unwrap_or_else(PoisonError::into_inner);
        for (key, sig, path, mtime, value) in memo.iter() {
            if key == memo_key
                && sig == &env_sig
                && path == config_path
                && *mtime == config_mtime_ms
            {
                return value.clone();
            }
        }
    }

    let mut parts = vec![format!("host:{host}")];
    let mut has_credential_material = false;
    for name in token_env_vars {
        if let Ok(value) = std::env::var(name) {
            if value.is_empty() {
                continue;
            }
            has_credential_material = true;
            parts.push(format!("{name}:{value}"));
        }
    }
    if let Ok(contents) = std::fs::read_to_string(config_path) {
        has_credential_material = true;
        parts.push(format!("{config_label}:{contents}"));
    }
    let value = has_credential_material.then(|| format!("{host}:{}", hash_cache_identity(&parts)));
    let mut memo = AUTH_KEY_MEMO.lock().unwrap_or_else(PoisonError::into_inner);
    memo.retain(|(key, ..)| key != memo_key);
    memo.push((
        memo_key.to_string(),
        env_sig,
        config_path.to_path_buf(),
        config_mtime_ms,
        value.clone(),
    ));
    value
}

/// Best-effort local fingerprint for the active GitHub CLI credentials.
///
/// Cache hits must not cross account/token boundaries, but probing `gh api user`
/// before every cached read would defeat the soft-TTL contract. Rows are keyed
/// by the credential material `gh` itself consumes — token env vars and/or
/// `hosts.yml` — and only the hash is stored, never the token or file contents.
/// `None` means "no visible credential": callers must bypass the cache.
#[must_use]
pub fn resolve_github_cache_auth_key(host: Option<&str>) -> Option<String> {
    let host = host.map_or_else(default_gh_host, str::to_lowercase);
    resolve_cli_cache_auth_key(
        &format!("github:{host}"),
        &host,
        &AUTH_KEY_TOKEN_ENV_VARS,
        &get_gh_config_dir().join("hosts.yml"),
        "hosts",
    )
}

/// GitLab counterpart of [`resolve_github_cache_auth_key`].
#[must_use]
pub fn resolve_gitlab_cache_auth_key(host: &str) -> Option<String> {
    resolve_cli_cache_auth_key(
        &format!("gitlab:{host}"),
        host,
        &GITLAB_TOKEN_ENV_VARS,
        &get_glab_config_path(),
        "glab-config",
    )
}

/// The host `gh` falls back to for any ref that names none.
#[must_use]
pub fn default_gh_host() -> String {
    std::env::var("GH_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || "github.com".to_string(),
            |value| value.trim().to_lowercase(),
        )
}

/// Split `[HOST/]OWNER/REPO`; anything with another shape is taken as a slug.
#[must_use]
pub fn parse_repo_ref(repo: &str) -> (Option<String>, String) {
    let Some(first_slash) = repo.find('/') else {
        return (None, repo.to_string());
    };
    let Some(second_slash) = repo[first_slash + 1..]
        .find('/')
        .map(|i| i + first_slash + 1)
    else {
        return (None, repo.to_string());
    };
    if repo[second_slash + 1..].contains('/') {
        return (None, repo.to_string());
    }
    (
        Some(repo[..first_slash].to_string()),
        repo[first_slash + 1..].to_string(),
    )
}

/// Row identity for a repo, relative to the host `gh` defaults to. A prefix
/// naming that same host is dropped so `github.com/owner/repo` and
/// `owner/repo` share one row — but under `GH_HOST` the bare form means the
/// configured instance, so an explicit `github.com/` prefix then stays.
#[must_use]
pub fn normalize_repo(repo: &str) -> String {
    let (host, slug) = parse_repo_ref(&repo.to_lowercase());
    match host {
        Some(host) if host != default_gh_host() => format!("{host}/{slug}"),
        _ => slug,
    }
}

fn row_to_view(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedView> {
    let kind: String = row.get("kind")?;
    let payload: String = row.get("payload")?;
    Ok(CachedView {
        auth_key: row.get("auth_key")?,
        repo: row.get("repo")?,
        kind: kind.parse().unwrap_or(CacheKind::Issue),
        number: row.get("number")?,
        include_comments: row.get::<_, i64>("include_comments")? == 1,
        fetched_at: row.get("fetched_at")?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        rendered: row.get("rendered")?,
        source_url: row.get("source_url")?,
    })
}

/// Read one live cache row, or `None`.
#[must_use]
pub fn get_cached(
    repo: &str,
    kind: CacheKind,
    number: i64,
    include_comments: bool,
    auth_key: &str,
) -> Option<CachedView> {
    with_db(|conn| {
        conn.query_row(
            "SELECT auth_key, repo, kind, number, include_comments, fetched_at, payload, rendered, source_url
             FROM github_view_cache
             WHERE auth_key = ?1 AND repo = ?2 AND kind = ?3 AND number = ?4 AND include_comments = ?5",
            params![
                auth_key,
                normalize_repo(repo),
                kind.as_str(),
                number,
                i64::from(include_comments)
            ],
            row_to_view,
        )
        .optional()
        .ok()
        .flatten()
    })
    .flatten()
}

/// Write (or replace) the live cache row.
pub fn put_cached(input: &PutCachedInput) {
    let fetched_at = input.fetched_at.unwrap_or_else(now_ms);
    let payload = input.payload.to_string();
    let auth_key = input
        .auth_key
        .clone()
        .unwrap_or_else(|| DEFAULT_CACHE_AUTH_KEY.to_string());
    let repo = normalize_repo(&input.repo);
    let _ = with_db(|conn| {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO github_view_cache
                (auth_key, repo, kind, number, include_comments, fetched_at, payload, rendered, source_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                auth_key,
                repo,
                input.kind.as_str(),
                input.number,
                i64::from(input.include_comments),
                fetched_at,
                payload,
                input.rendered,
                input.source_url
            ],
        );
    });
    protect_db_files(&github_cache_db_path());
}

/// Drop a specific cache entry. Bumps the invalidation epoch.
pub fn invalidate(
    repo: &str,
    kind: CacheKind,
    number: i64,
    include_comments: Option<bool>,
    auth_key: &str,
) {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let repo = normalize_repo(repo);
    let _ = with_db(|conn| match include_comments {
        None => {
            let _ = conn.execute(
                "DELETE FROM github_view_cache WHERE auth_key = ?1 AND repo = ?2 AND kind = ?3 AND number = ?4",
                params![auth_key, repo, kind.as_str(), number],
            );
        }
        Some(include_comments) => {
            let _ = conn.execute(
                "DELETE FROM github_view_cache
                 WHERE auth_key = ?1 AND repo = ?2 AND kind = ?3 AND number = ?4 AND include_comments = ?5",
                params![auth_key, repo, kind.as_str(), number, i64::from(include_comments)],
            );
        }
    });
}

/// Drop every cached row for an issue/PR number, regardless of repo, auth key,
/// `include_comments`, or row kind.
///
/// Repo + auth-key narrowing is intentionally skipped: the bash-side detector
/// that reacts to `gh issue close` / `gh pr merge` often does not name the repo,
/// and resolving the current repo from `cwd` on every bash call would cost far
/// more than a write-amplified `DELETE`.
pub fn invalidate_all_for_number(number: i64, repo: Option<&str>) {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let normalized = repo.map(normalize_repo);
    let _ = with_db(|conn| match normalized {
        None => {
            let _ = conn.execute(
                "DELETE FROM github_view_cache WHERE number = ?1",
                params![number],
            );
        }
        Some(repo) => {
            let _ = conn.execute(
                "DELETE FROM github_view_cache WHERE number = ?1 AND repo = ?2",
                params![number, repo],
            );
        }
    });
}

/// Drop every cached row for a repo, or all rows when the repo is unknown.
/// Fallback for current-branch mutations that name no PR number or URL.
pub fn invalidate_all_for_repo(repo: Option<&str>) {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let normalized = repo.map(normalize_repo);
    let _ = with_db(|conn| match normalized {
        None => {
            let _ = conn.execute("DELETE FROM github_view_cache", []);
        }
        Some(repo) => {
            let _ = conn.execute(
                "DELETE FROM github_view_cache WHERE repo = ?1",
                params![repo],
            );
        }
    });
}

/// Drop live PR rows after a push, leaving issues and retained snapshots alone.
pub fn invalidate_pull_requests_for_repo(repo: &str) {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let repo = normalize_repo(repo);
    let _ = with_db(|conn| {
        let _ = conn.execute(
            "DELETE FROM github_view_cache WHERE repo = ?1 AND kind IN ('pr', 'pr-diff')",
            params![repo],
        );
    });
}

/// Drop every cached row. Test helper.
pub fn clear_all() {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let _ = with_db(|conn| {
        let _ = conn.execute("DELETE FROM github_view_cache", []);
        let _ = conn.execute("DELETE FROM github_view_cache_generation", []);
    });
}

/// Current invalidation epoch (see the module header).
#[must_use]
pub fn invalidation_epoch() -> u64 {
    INVALIDATION_EPOCH.load(Ordering::SeqCst)
}

/// Caps a pathological run of distinct snapshots claiming the same millisecond.
pub const MAX_GENERATION_ID_STEPS: i64 = 1000;

/// Keep one immutable generation (a `/review` snapshot).
///
/// A published id is never overwritten: storing the same snapshot again returns
/// its existing id and restarts its retention clock, while a *different*
/// snapshot that lands on the same millisecond gets the next free id, which the
/// caller must publish. Retention runs from `retained_at`, not `fetched_at`, so
/// a stale live row pinned by `/review` keeps its full hard TTL. Returns that id,
/// or `None` when the database is unavailable.
pub fn retain_cached_generation(input: &PutCachedInput) -> Option<i64> {
    let auth_key = input
        .auth_key
        .clone()
        .unwrap_or_else(|| DEFAULT_CACHE_AUTH_KEY.to_string());
    let repo = normalize_repo(&input.repo);
    let payload = input.payload.to_string();
    let retained_at = now_ms();
    let mut fetched_at = input.fetched_at.unwrap_or(retained_at);
    with_db(|conn| {
        for _ in 0..MAX_GENERATION_ID_STEPS {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT (payload = ?1 AND rendered = ?2) AS same
                     FROM github_view_cache_generation
                     WHERE auth_key = ?3 AND repo = ?4 AND kind = ?5 AND number = ?6
                       AND include_comments = ?7 AND fetched_at = ?8",
                    params![
                        payload,
                        input.rendered,
                        auth_key,
                        repo,
                        input.kind.as_str(),
                        input.number,
                        i64::from(input.include_comments),
                        fetched_at
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .ok()
                .flatten();
            match existing {
                Some(1) => {
                    let _ = conn.execute(
                        "UPDATE github_view_cache_generation SET retained_at = ?1
                         WHERE auth_key = ?2 AND repo = ?3 AND kind = ?4 AND number = ?5
                           AND include_comments = ?6 AND fetched_at = ?7",
                        params![
                            retained_at,
                            auth_key,
                            repo,
                            input.kind.as_str(),
                            input.number,
                            i64::from(input.include_comments),
                            fetched_at
                        ],
                    );
                    return Some(fetched_at);
                }
                Some(_) => fetched_at += 1,
                None => {
                    let inserted = conn.execute(
                        "INSERT INTO github_view_cache_generation
                            (auth_key, repo, kind, number, include_comments, fetched_at, retained_at,
                             payload, rendered, source_url)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        params![
                            auth_key,
                            repo,
                            input.kind.as_str(),
                            input.number,
                            i64::from(input.include_comments),
                            fetched_at,
                            retained_at,
                            payload,
                            input.rendered,
                            input.source_url
                        ],
                    );
                    match inserted {
                        Ok(_) => return Some(fetched_at),
                        Err(_) => fetched_at += 1,
                    }
                }
            }
        }
        None
    })
    .flatten()
}

fn delete_cached_generation(
    repo: &str,
    kind: CacheKind,
    number: i64,
    include_comments: bool,
    fetched_at: i64,
    auth_key: &str,
) {
    let repo = normalize_repo(repo);
    let _ = with_db(|conn| {
        let _ = conn.execute(
            "DELETE FROM github_view_cache_generation
             WHERE auth_key = ?1 AND repo = ?2 AND kind = ?3 AND number = ?4
               AND include_comments = ?5 AND fetched_at = ?6",
            params![
                auth_key,
                repo,
                kind.as_str(),
                number,
                i64::from(include_comments),
                fetched_at
            ],
        );
    });
}

/// Read a generation stored by [`retain_cached_generation`], honoring the cache
/// kill switch and hard TTL (counted from `retained_at`). An expired generation
/// is deleted. Does not read or refresh the live row.
#[must_use]
pub fn get_retained_generation(
    repo: &str,
    kind: CacheKind,
    number: i64,
    include_comments: bool,
    fetched_at: i64,
    auth_key: &str,
    ttl: CacheTtl,
) -> Option<CachedGeneration> {
    // Pinned-only sessions never enter `get_or_fetch_view`, so the sweep has to
    // run here too. Disabled lookups sweep as well; they just refuse the read.
    sweep_if_due(ttl.hard_ms);
    if !ttl.enabled {
        return None;
    }
    let normalized = normalize_repo(repo);
    let row = with_db(|conn| {
        conn.query_row(
            "SELECT auth_key, repo, kind, number, include_comments, fetched_at, payload, rendered,
                    source_url, retained_at
             FROM github_view_cache_generation
             WHERE auth_key = ?1 AND repo = ?2 AND kind = ?3 AND number = ?4
               AND include_comments = ?5 AND fetched_at = ?6",
            params![
                auth_key,
                normalized,
                kind.as_str(),
                number,
                i64::from(include_comments),
                fetched_at
            ],
            |row| {
                Ok(CachedGeneration {
                    view: row_to_view(row)?,
                    retained_at: row.get("retained_at")?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    })
    .flatten()?;
    if now_ms().saturating_sub(row.retained_at) > ttl.hard_ms {
        delete_cached_generation(repo, kind, number, include_comments, fetched_at, auth_key);
        return None;
    }
    Some(row)
}

/// Close and forget the cached connection so the next access reopens against
/// (possibly) a different DB path. Test/maintenance helper.
pub fn reset_for_tests() {
    INVALIDATION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let mut guard = DB.lock().unwrap_or_else(PoisonError::into_inner);
    *guard = None;
    OPEN_ATTEMPTED.store(false, Ordering::SeqCst);
    LAST_SWEEP_MS.store(0, Ordering::SeqCst);
    AUTH_KEY_MEMO
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

// ────────────────────────────────────────────────────────────────────────────
// Cache-aware lookup wrapper
// ────────────────────────────────────────────────────────────────────────────

/// What a fresh fetch produced.
#[derive(Debug, Clone)]
pub struct FreshResult {
    pub rendered: String,
    pub source_url: Option<String>,
    pub payload: Value,
}

/// How long a cached issue or PR that was still open stays fresh.
///
/// It can be merged, closed or reopened at any moment (often by the agent
/// itself, outside this tool), so it is revalidated well before the configured
/// soft TTL.
pub const OPEN_STATE_SOFT_TTL_MS: i64 = 15_000;

/// Resolved cache TTLs and kill switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTtl {
    pub soft_ms: i64,
    pub hard_ms: i64,
    pub enabled: bool,
}

impl Default for CacheTtl {
    fn default() -> Self {
        Self {
            soft_ms: 300_000,
            hard_ms: 604_800_000,
            enabled: true,
        }
    }
}

/// `github.cache.*` settings, floored at 0 and converted to milliseconds.
#[must_use]
pub fn resolve_cache_ttl(settings: Option<&omp_settings::Settings>) -> CacheTtl {
    let Some(settings) = settings else {
        return CacheTtl::default();
    };
    CacheTtl {
        soft_ms: settings
            .get_number("github.cache.softTtlSec")
            .unwrap_or(300.0)
            .max(0.0) as i64
            * 1000,
        hard_ms: settings
            .get_number("github.cache.hardTtlSec")
            .unwrap_or(604_800.0)
            .max(0.0) as i64
            * 1000,
        enabled: settings.get_bool("github.cache.enabled").unwrap_or(true),
    }
}

/// Credential namespace for a lookup: the row default, an explicit key, or
/// "no visible credential" (bypass the cache entirely).
#[derive(Debug, Clone)]
pub enum AuthKey {
    Default,
    Bypass,
    Key(String),
}

impl AuthKey {
    fn resolve(&self) -> Option<String> {
        match self {
            Self::Default => Some(DEFAULT_CACHE_AUTH_KEY.to_string()),
            Self::Bypass => None,
            Self::Key(key) => Some(key.clone()),
        }
    }
}

/// Outcome of [`get_or_fetch_view`], for freshness notes and protocol handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Miss,
    Fresh,
    Refreshed,
    Stale,
    Disabled,
}

impl CacheStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Miss => "miss",
            Self::Fresh => "fresh",
            Self::Refreshed => "refreshed",
            Self::Stale => "stale",
            Self::Disabled => "disabled",
        }
    }
}

/// A resolved lookup result.
#[derive(Debug, Clone)]
pub struct CacheLookupResult {
    pub rendered: String,
    pub source_url: Option<String>,
    pub payload: Value,
    pub status: CacheStatus,
    pub fetched_at: i64,
}

type Fetcher = Arc<dyn Fn() -> BoxFut<'static, Result<FreshResult>> + Send + Sync>;

/// Inputs for [`get_or_fetch_view`].
#[derive(Clone)]
pub struct CacheLookupOptions {
    pub repo: String,
    pub kind: CacheKind,
    pub number: i64,
    pub include_comments: bool,
    pub auth_key: AuthKey,
    pub fetch_fresh: Fetcher,
    pub ttl: CacheTtl,
    pub now: Option<i64>,
}

fn is_open_view(kind: CacheKind, payload: &Value) -> bool {
    if !matches!(kind, CacheKind::Issue | CacheKind::Pr) {
        return false;
    }
    payload
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| state.eq_ignore_ascii_case("OPEN"))
}

fn store_result(
    auth_key: &str,
    options: &CacheLookupOptions,
    fresh: &FreshResult,
    fetched_at: i64,
) {
    put_cached(&PutCachedInput {
        auth_key: Some(auth_key.to_string()),
        repo: options.repo.clone(),
        kind: options.kind,
        number: options.number,
        include_comments: options.include_comments,
        payload: fresh.payload.clone(),
        rendered: fresh.rendered.clone(),
        source_url: fresh.source_url.clone(),
        fetched_at: Some(fetched_at),
    });
}

async fn fetch_and_store(
    auth_key: String,
    options: CacheLookupOptions,
    status: CacheStatus,
) -> Result<CacheLookupResult> {
    let epoch = invalidation_epoch();
    let fresh = (options.fetch_fresh)().await?;
    let fetched_at = now_ms();
    if epoch == invalidation_epoch() {
        store_result(&auth_key, &options, &fresh, fetched_at);
    }
    Ok(CacheLookupResult {
        rendered: fresh.rendered,
        source_url: fresh.source_url,
        payload: fresh.payload,
        status,
        fetched_at,
    })
}

/// In-flight background refreshes keyed by row identity, so N concurrent stale
/// reads of one row spawn one `gh` subprocess rather than N identical ones.
static INFLIGHT_REFRESHES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn schedule_background_refresh(auth_key: String, options: CacheLookupOptions) {
    let key = format!(
        "{auth_key}|{}|{}|{}|{}",
        normalize_repo(&options.repo),
        options.kind.as_str(),
        options.number,
        i64::from(options.include_comments)
    );
    {
        let mut inflight = INFLIGHT_REFRESHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if inflight.iter().any(|existing| existing == &key) {
            return;
        }
        inflight.push(key.clone());
    }
    tokio::spawn(async move {
        let _ = fetch_and_store(auth_key, options, CacheStatus::Refreshed).await;
        let mut inflight = INFLIGHT_REFRESHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        inflight.retain(|existing| existing != &key);
    });
}

/// Cache-aware fetch: the one entry point shared by the `github` tool ops and
/// the `issue://` / `pr://` / `mr://` handlers.
pub async fn get_or_fetch_view(options: CacheLookupOptions) -> Result<CacheLookupResult> {
    let ttl = options.ttl;
    let now = options.now.unwrap_or_else(now_ms);
    let Some(auth_key) = options.auth_key.resolve() else {
        let fresh = (options.fetch_fresh)().await?;
        return Ok(CacheLookupResult {
            rendered: fresh.rendered,
            source_url: fresh.source_url,
            payload: fresh.payload,
            status: CacheStatus::Disabled,
            fetched_at: now,
        });
    };
    if !ttl.enabled {
        let fresh = (options.fetch_fresh)().await?;
        return Ok(CacheLookupResult {
            rendered: fresh.rendered,
            source_url: fresh.source_url,
            payload: fresh.payload,
            status: CacheStatus::Disabled,
            fetched_at: now,
        });
    }

    // Enforce the *configured* hard TTL against on-disk rows: this is what makes
    // `github.cache.hardTtlSec` a real retention cap rather than a soft
    // suggestion the next open eventually honors.
    sweep_if_due(ttl.hard_ms);

    let cached = get_cached(
        &options.repo,
        options.kind,
        options.number,
        options.include_comments,
        &auth_key,
    );

    if let Some(cached) = cached {
        let age = now.saturating_sub(cached.fetched_at);
        let soft_ms = if is_open_view(options.kind, &cached.payload) {
            ttl.soft_ms.min(OPEN_STATE_SOFT_TTL_MS)
        } else {
            ttl.soft_ms
        };
        if age > ttl.hard_ms {
            // Drop the row eagerly so the on-disk exposure window stays bounded
            // even if the fetch below fails and never overwrites it.
            invalidate(
                &options.repo,
                options.kind,
                options.number,
                Some(options.include_comments),
                &auth_key,
            );
        } else if age <= soft_ms {
            return Ok(CacheLookupResult {
                rendered: cached.rendered,
                source_url: cached.source_url,
                payload: cached.payload,
                status: CacheStatus::Fresh,
                fetched_at: cached.fetched_at,
            });
        } else if matches!(options.kind, CacheKind::PrDiff | CacheKind::MrDiff) {
            schedule_background_refresh(auth_key.clone(), options.clone());
            return Ok(CacheLookupResult {
                rendered: cached.rendered,
                source_url: cached.source_url,
                payload: cached.payload,
                status: CacheStatus::Stale,
                fetched_at: cached.fetched_at,
            });
        } else {
            match fetch_and_store(auth_key.clone(), options.clone(), CacheStatus::Refreshed).await {
                Ok(result) => return Ok(result),
                Err(_) => {
                    return Ok(CacheLookupResult {
                        rendered: cached.rendered,
                        source_url: cached.source_url,
                        payload: cached.payload,
                        status: CacheStatus::Stale,
                        fetched_at: cached.fetched_at,
                    });
                }
            }
        }
    }

    fetch_and_store(auth_key, options, CacheStatus::Miss).await
}

/// Human-friendly freshness note for protocol-handler `notes[]` rendering.
#[must_use]
pub fn format_freshness_note(status: CacheStatus, fetched_at_ms: i64, now: i64) -> String {
    match status {
        CacheStatus::Miss | CacheStatus::Refreshed => return "Fetched live".to_string(),
        CacheStatus::Disabled => return "Cache disabled; fetched live".to_string(),
        CacheStatus::Fresh | CacheStatus::Stale => {}
    }
    let age_sec = now.saturating_sub(fetched_at_ms).max(0) / 1000;
    let human = if age_sec < 60 {
        format!("{age_sec}s ago")
    } else if age_sec < 3600 {
        format!("{}m ago", (age_sec + 30) / 60)
    } else {
        format!("{}h ago", (age_sec + 1800) / 3600)
    };
    if status == CacheStatus::Stale {
        return format!(
            "WARNING: showing cached content from {human}; live refresh failed or is still running"
        );
    }
    format!("Cached: {human}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::OnceLock;

    /// The cache is process-global; DB tests must not interleave.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    struct TempDb {
        _dir: tempfile::TempDir,
    }

    fn temp_db() -> TempDb {
        let dir = tempfile::tempdir().expect("tempdir");
        set_db_path_override(Some(dir.path().join("github-cache.db")));
        reset_for_tests();
        TempDb { _dir: dir }
    }

    fn put_view(kind: CacheKind, number: i64, state: &str, fetched_at: i64) {
        put_cached(&PutCachedInput {
            auth_key: None,
            repo: "owner/repo".to_string(),
            kind,
            number,
            include_comments: true,
            payload: serde_json::json!({"number": number, "state": state}),
            rendered: format!("cached-{number}"),
            source_url: Some(format!("https://github.com/owner/repo/pull/{number}")),
            fetched_at: Some(fetched_at),
        });
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn counting_fetcher(rendered: &'static str) -> (Fetcher, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let fetcher: Fetcher = Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(FreshResult {
                    rendered: rendered.to_string(),
                    source_url: None,
                    payload: serde_json::json!({"state": "OPEN"}),
                })
            })
        });
        (fetcher, calls)
    }

    #[test]
    fn schema_is_user_version_4_with_both_tables() {
        let _guard = serial();
        let _db = temp_db();
        with_db(|conn| {
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, 4);
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .collect();
            assert!(tables.contains(&"github_view_cache".to_string()));
            assert!(tables.contains(&"github_view_cache_generation".to_string()));
        })
        .expect("db open");
    }

    #[test]
    fn put_and_get_roundtrip_normalizes_default_host() {
        let _guard = serial();
        let _db = temp_db();
        put_view(CacheKind::Pr, 1, "OPEN", now_ms());
        let row = get_cached(
            "github.com/owner/repo",
            CacheKind::Pr,
            1,
            true,
            DEFAULT_CACHE_AUTH_KEY,
        )
        .expect("row");
        assert_eq!(row.repo, "owner/repo");
        assert_eq!(row.payload["state"], "OPEN");
        assert!(get_cached("owner/repo", CacheKind::Pr, 2, true, DEFAULT_CACHE_AUTH_KEY).is_none());
    }

    #[test]
    fn hard_ttl_expiry_deletes_row_and_reports_miss() {
        let _guard = serial();
        let _db = temp_db();
        let now = now_ms();
        put_view(CacheKind::Pr, 1, "OPEN", now - 5_000);
        let (fetcher, calls) = counting_fetcher("fresh");
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Pr,
            number: 1,
            include_comments: true,
            auth_key: AuthKey::Default,
            fetch_fresh: fetcher,
            ttl: CacheTtl {
                soft_ms: 1_000,
                hard_ms: 2_000,
                enabled: true,
            },
            now: Some(now),
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Miss);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The expired row was dropped eagerly, then replaced by the fresh one.
        let row = get_cached("owner/repo", CacheKind::Pr, 1, true, DEFAULT_CACHE_AUTH_KEY).unwrap();
        assert_eq!(row.rendered, "fresh");
        assert!(row.fetched_at > now - 5_000);
    }

    #[test]
    fn open_state_soft_ttl_is_clamped_to_fifteen_seconds() {
        let _guard = serial();
        let _db = temp_db();
        let now = now_ms();
        // 20s old: inside the 300s configured soft TTL, past the 15s open clamp.
        put_view(CacheKind::Pr, 1, "OPEN", now - 20_000);
        let (fetcher, calls) = counting_fetcher("refreshed");
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Pr,
            number: 1,
            include_comments: true,
            auth_key: AuthKey::Default,
            fetch_fresh: fetcher,
            ttl: CacheTtl {
                soft_ms: 300_000,
                hard_ms: 604_800_000,
                enabled: true,
            },
            now: Some(now),
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Refreshed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.rendered, "refreshed");
    }

    #[test]
    fn closed_row_within_soft_ttl_is_served_fresh_without_a_fetch() {
        let _guard = serial();
        let _db = temp_db();
        let now = now_ms();
        put_view(CacheKind::Pr, 7, "MERGED", now - 20_000);
        let (fetcher, calls) = counting_fetcher("fresh");
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Pr,
            number: 7,
            include_comments: true,
            auth_key: AuthKey::Default,
            fetch_fresh: fetcher,
            ttl: CacheTtl {
                soft_ms: 300_000,
                hard_ms: 604_800_000,
                enabled: true,
            },
            now: Some(now),
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Fresh);
        assert_eq!(result.rendered, "cached-7");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn disabled_cache_bypasses_storage() {
        let _guard = serial();
        let _db = temp_db();
        let (fetcher, _calls) = counting_fetcher("live");
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Issue,
            number: 3,
            include_comments: true,
            auth_key: AuthKey::Bypass,
            fetch_fresh: fetcher,
            ttl: CacheTtl::default(),
            now: None,
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Disabled);
        assert!(get_cached(
            "owner/repo",
            CacheKind::Issue,
            3,
            true,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_none());
    }

    #[test]
    fn kill_switch_also_disables_storage() {
        let _guard = serial();
        let _db = temp_db();
        let (fetcher, _calls) = counting_fetcher("live");
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Issue,
            number: 4,
            include_comments: true,
            auth_key: AuthKey::Default,
            fetch_fresh: fetcher,
            ttl: CacheTtl {
                enabled: false,
                ..CacheTtl::default()
            },
            now: None,
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Disabled);
        assert!(get_cached(
            "owner/repo",
            CacheKind::Issue,
            4,
            true,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_none());
    }

    #[test]
    fn invalidation_epoch_fences_an_in_flight_store() {
        let _guard = serial();
        let _db = temp_db();
        let result = block_on(get_or_fetch_view(CacheLookupOptions {
            repo: "owner/repo".to_string(),
            kind: CacheKind::Pr,
            number: 9,
            include_comments: true,
            auth_key: AuthKey::Default,
            fetch_fresh: Arc::new(|| {
                Box::pin(async {
                    // A mutation lands mid-fetch.
                    invalidate_all_for_repo(None);
                    Ok(FreshResult {
                        rendered: "stale-write".to_string(),
                        source_url: None,
                        payload: serde_json::json!({"state": "OPEN"}),
                    })
                })
            }),
            ttl: CacheTtl::default(),
            now: None,
        }))
        .expect("lookup");
        assert_eq!(result.status, CacheStatus::Miss);
        assert_eq!(result.rendered, "stale-write");
        assert!(
            get_cached("owner/repo", CacheKind::Pr, 9, true, DEFAULT_CACHE_AUTH_KEY).is_none(),
            "a pre-invalidation fetch must not repopulate the row"
        );
    }

    #[test]
    fn invalidation_helpers_scope_their_deletes() {
        let _guard = serial();
        let _db = temp_db();
        for kind in [CacheKind::Pr, CacheKind::PrDiff, CacheKind::Issue] {
            put_view(kind, 1, "OPEN", now_ms());
        }
        invalidate_pull_requests_for_repo("owner/repo");
        assert!(get_cached("owner/repo", CacheKind::Pr, 1, true, DEFAULT_CACHE_AUTH_KEY).is_none());
        assert!(get_cached(
            "owner/repo",
            CacheKind::PrDiff,
            1,
            true,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_none());
        assert!(get_cached(
            "owner/repo",
            CacheKind::Issue,
            1,
            true,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_some());

        invalidate_all_for_number(1, None);
        assert!(get_cached(
            "owner/repo",
            CacheKind::Issue,
            1,
            true,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_none());
    }

    #[test]
    fn retained_generation_roundtrips_and_is_immutable() {
        let _guard = serial();
        let _db = temp_db();
        let input = PutCachedInput {
            auth_key: None,
            repo: "owner/repo".to_string(),
            kind: CacheKind::PrDiff,
            number: 5,
            include_comments: false,
            payload: serde_json::json!({"files": []}),
            rendered: "diff".to_string(),
            source_url: None,
            fetched_at: Some(1_000),
        };
        let id = retain_cached_generation(&input).expect("retained");
        assert_eq!(id, 1_000);
        // Same snapshot again: same id, retention clock restarted.
        assert_eq!(retain_cached_generation(&input), Some(1_000));
        // A different snapshot on the same millisecond gets the next id.
        let mut other = input.clone();
        other.rendered = "different".to_string();
        assert_eq!(retain_cached_generation(&other), Some(1_001));

        let row = get_retained_generation(
            "owner/repo",
            CacheKind::PrDiff,
            5,
            false,
            1_000,
            DEFAULT_CACHE_AUTH_KEY,
            CacheTtl::default(),
        )
        .expect("generation");
        assert_eq!(row.view.rendered, "diff");
        assert!(row.retained_at > 0);
        // The live row is untouched by retention.
        assert!(get_cached(
            "owner/repo",
            CacheKind::PrDiff,
            5,
            false,
            DEFAULT_CACHE_AUTH_KEY
        )
        .is_none());
    }

    #[test]
    fn retained_generation_honors_kill_switch_and_hard_ttl() {
        let _guard = serial();
        let _db = temp_db();
        let input = PutCachedInput {
            auth_key: None,
            repo: "owner/repo".to_string(),
            kind: CacheKind::PrDiff,
            number: 6,
            include_comments: false,
            payload: Value::Null,
            rendered: "diff".to_string(),
            source_url: None,
            fetched_at: Some(500),
        };
        assert_eq!(retain_cached_generation(&input), Some(500));
        let disabled = CacheTtl {
            enabled: false,
            ..CacheTtl::default()
        };
        assert!(get_retained_generation(
            "owner/repo",
            CacheKind::PrDiff,
            6,
            false,
            500,
            DEFAULT_CACHE_AUTH_KEY,
            disabled
        )
        .is_none());
        let expired = CacheTtl {
            hard_ms: 0,
            ..CacheTtl::default()
        };
        assert!(get_retained_generation(
            "owner/repo",
            CacheKind::PrDiff,
            6,
            false,
            500,
            DEFAULT_CACHE_AUTH_KEY,
            expired
        )
        .is_none());
    }

    #[test]
    fn freshness_note_matches_ts_wording() {
        let now = 1_000_000_000;
        assert_eq!(
            format_freshness_note(CacheStatus::Miss, now - 5_000, now),
            "Fetched live"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Refreshed, now - 5_000, now),
            "Fetched live"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Disabled, now - 5_000, now),
            "Cache disabled; fetched live"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Fresh, now - 12_000, now),
            "Cached: 12s ago"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Fresh, now - 120_000, now),
            "Cached: 2m ago"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Fresh, now - 7_200_000, now),
            "Cached: 2h ago"
        );
        assert_eq!(
            format_freshness_note(CacheStatus::Stale, now - 120_000, now),
            "WARNING: showing cached content from 2m ago; live refresh failed or is still running"
        );
    }

    #[test]
    fn normalize_repo_drops_only_the_default_host() {
        let default_host = default_gh_host();
        assert_eq!(
            normalize_repo(&format!("{default_host}/Owner/Repo")),
            "owner/repo"
        );
        assert_eq!(normalize_repo("Owner/Repo"), "owner/repo");
        if default_host != "ghe.example.com" {
            assert_eq!(
                normalize_repo("ghe.example.com/Owner/Repo"),
                "ghe.example.com/owner/repo"
            );
        }
    }

    #[test]
    fn parse_repo_ref_splits_host_only_for_three_segments() {
        assert_eq!(
            parse_repo_ref("github.com/owner/repo"),
            (Some("github.com".to_string()), "owner/repo".to_string())
        );
        assert_eq!(
            parse_repo_ref("owner/repo"),
            (None, "owner/repo".to_string())
        );
        assert_eq!(parse_repo_ref("repo"), (None, "repo".to_string()));
        assert_eq!(
            parse_repo_ref("host/owner/repo/extra"),
            (None, "host/owner/repo/extra".to_string())
        );
    }

    #[test]
    fn ttl_defaults_match_schema_defaults() {
        let ttl = resolve_cache_ttl(None);
        assert_eq!(ttl.soft_ms, 300_000);
        assert_eq!(ttl.hard_ms, 604_800_000);
        assert!(ttl.enabled);
        assert_eq!(CacheTtl::default(), ttl);
    }

    #[test]
    fn sweep_throttle_is_one_minute() {
        assert_eq!(SWEEP_INTERVAL_MS, 60_000);
        assert_eq!(OPEN_STATE_SOFT_TTL_MS, 15_000);
        assert_eq!(MAX_GENERATION_ID_STEPS, 1000);
    }
}
