//! Moving a session between machines (senpi `core/session-move`, as the OmOMeow gateway drives it
//! through the `move_session` RPC command): the bundle a sender exports, the ledger a receiver
//! keeps, and the ownership a session file records about itself.
//!
//! Ownership lives in the session file as `custom` entries, folded in order: writable (no entry, a
//! `takeover` release, `session_moved_in`, `session_reclaimed`), then `moving` after a `move`
//! release (only that move may continue), then `moved` after `session_moved` (nothing reopens it).
//! An entry naming another session id was inherited by a copy or branch and is not a step here.
//!
//! The receiving machine keeps one ledger record per move id: `ready` (prepared, open for
//! [`READY_TTL_MS`]), `receiving` (a verified bundle is landing), then a final `accepted`,
//! `declined` or `failed`. Asking about a move this machine never heard of records it `declined`
//! first, so a late import of it is refused and the sender may safely take its session back.
//!
//! Every step is synchronous file I/O; the RPC layer runs it off the async workers.

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Bundle layout version written to `manifest.json`.
pub const MOVE_BUNDLE_FORMAT: u64 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";
/// Where the session file travels inside a bundle.
pub const SESSION_BUNDLE_PATH: &str = "files/session.jsonl";
/// How long a prepared move waits for its bundle before it is declined as expired.
pub const READY_TTL_MS: i64 = 10 * 60_000;

/// `customType` of the seal a `release_session` appends.
pub const SESSION_RELEASED: &str = "session_released";
pub const SESSION_MOVED_IN: &str = "session_moved_in";
pub const SESSION_MOVED: &str = "session_moved";
pub const SESSION_RECLAIMED: &str = "session_reclaimed";
/// Marks the moved extension state of one move as handed to the extensions (never redelivered).
pub const MOVED_STATE_RESTORED: &str = "session_move_state_restored";
/// The `session_released` field naming the release attempt whose staged state the bundle carries.
pub const STATE_ATTEMPT_FIELD: &str = "state_attempt";

const EXTENSION_STATE_PREFIX: &str = "state/";
const EXTENSION_STATE_PAYLOAD: &str = "extension-state";
const MOVE_STATE_DIR: &str = ".move-state";

/// A refused or failed move step. `code` is the wire `errorCode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveError {
    pub code: &'static str,
    pub detail: String,
}

impl MoveError {
    pub const INVALID_REQUEST: &'static str = "invalid_move_request";
    pub const NOT_RELEASED: &'static str = "move_not_released";
    pub const MISMATCH: &'static str = "move_mismatch";
    pub const BUNDLE_INVALID: &'static str = "move_bundle_invalid";
    pub const REPOSITORY_MISMATCH: &'static str = "move_repository_mismatch";
    pub const TARGET_OCCUPIED: &'static str = "move_target_occupied";
    pub const ALREADY_MOVED: &'static str = "move_already_moved";
    pub const FENCED: &'static str = "move_fenced";
    pub const FAILED: &'static str = "move_failed";

    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self::new(Self::FAILED, detail)
    }

    fn invalid_bundle(detail: impl std::fmt::Display) -> Self {
        Self::new(
            Self::BUNDLE_INVALID,
            format!(
                "The moved session could not be verified ({detail}); it was not imported. The original device keeps it."
            ),
        )
    }

    fn io(what: &str, path: &Path, error: &std::io::Error) -> Self {
        Self::failed(format!("{what} {}: {error}", path.display()))
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for MoveError {}

/// Move ids, session ids and release attempts become file and folder names, so they must be plain:
/// a letter or digit first (never `.` or `..`), then letters, digits, `.`, `_`, `-`, at most 128.
#[must_use]
pub fn is_plain_id(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn plain<'a>(value: &'a str, what: &str) -> Result<&'a str, MoveError> {
    if is_plain_id(value) {
        Ok(value)
    } else {
        Err(MoveError::new(
            MoveError::INVALID_REQUEST,
            format!("the {what} is not a plain id: {value:?}"),
        ))
    }
}

// ------------------------------------------------------------------------------ ownership

/// Who may write a session file, as the file itself records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    Writable,
    Moving { move_id: String, target: String },
    Moved { move_id: String, target: String },
}

impl Ownership {
    /// The `ownership` word `move_session finish` answers with.
    #[must_use]
    pub fn wire(&self) -> &'static str {
        match self {
            Self::Writable => "writable",
            Self::Moving { .. } => "moving",
            Self::Moved { .. } => "moved",
        }
    }
}

fn custom_type(entry: &Value) -> Option<&str> {
    if entry.get("type").and_then(Value::as_str) != Some("custom") {
        return None;
    }
    entry.get("customType").and_then(Value::as_str)
}

fn data_str<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get("data")?.get(key)?.as_str()
}

/// The `id` of the file's `session` header (a title slot line before it is skipped).
#[must_use]
pub fn header_session_id(entries: &[Value]) -> Option<&str> {
    header(entries)?.get("id")?.as_str()
}

fn header(entries: &[Value]) -> Option<&Value> {
    entries
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("session"))
}

/// Fold the file's ownership entries in order; a transition the current state does not allow is
/// ignored.
#[must_use]
pub fn session_ownership(entries: &[Value]) -> Ownership {
    let session_id = header_session_id(entries);
    let mut verdict = Ownership::Writable;
    for entry in entries {
        let Some(kind @ (SESSION_RELEASED | SESSION_MOVED_IN | SESSION_MOVED | SESSION_RECLAIMED)) =
            custom_type(entry)
        else {
            continue;
        };
        if let (Some(owner), Some(id)) = (data_str(entry, "session_id"), session_id) {
            if owner != id {
                continue;
            }
        }
        let move_id = data_str(entry, "move_id").unwrap_or_default().to_owned();
        let target = data_str(entry, "target").unwrap_or_default().to_owned();
        let next = match kind {
            SESSION_MOVED => Ownership::Moved {
                move_id: move_id.clone(),
                target,
            },
            SESSION_RELEASED if data_str(entry, "reason") == Some("move") => Ownership::Moving {
                move_id: move_id.clone(),
                target,
            },
            _ => Ownership::Writable,
        };
        verdict = match &verdict {
            Ownership::Writable => next,
            Ownership::Moved { .. } => continue,
            // Only the same move continues a moving session: it arrived, was given back, or (a
            // file imported over its own predecessor) moved in again.
            Ownership::Moving {
                move_id: current, ..
            } if *current == move_id && kind != SESSION_RELEASED => next,
            Ownership::Moving { .. } => continue,
        };
    }
    verdict
}

/// The `session` header of a session file (read up to it, a title slot line before it skipped).
///
/// # Errors
/// The file cannot be read or has no header.
pub fn read_header(path: &Path) -> Result<Value, MoveError> {
    use std::io::BufRead;
    let file = fs::File::open(path).map_err(|e| MoveError::io("open", path, &e))?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| MoveError::io("read", path, &e))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => return Ok(value),
            Some("title") => continue,
            _ => break,
        }
    }
    Err(MoveError::failed(format!(
        "{} has no session header",
        path.display()
    )))
}

fn entries(path: &Path) -> Result<Vec<Value>, MoveError> {
    crate::read_values(path)
        .map_err(|e| MoveError::failed(format!("read {}: {e:#}", path.display())))
}

/// Append one `custom` bookkeeping entry (the model never sees it) to a session file this process
/// does not hold open, flushed to disk: a move step that cannot record itself fails.
///
/// # Errors
/// The file cannot be opened, written or synced.
pub fn append_custom(path: &Path, kind: &str, data: Value) -> Result<(), MoveError> {
    let record =
        |e: anyhow::Error| MoveError::failed(format!("record {kind} in {}: {e:#}", path.display()));
    let mut writer = crate::SessionWriter::open(path, None).map_err(record)?;
    writer
        .append_entry(
            "custom",
            json!({"customType": kind, "data": data}),
            &now_iso(),
        )
        .map_err(record)?;
    writer.sync().map_err(record)
}

// ------------------------------------------------------------------------------ bundle

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundleFile {
    pub path: String,
    pub sha256: String,
}

/// A repository's identity (senpi `RepositoryIdentity`): its root commits and normalized origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryIdentity {
    pub root_commits: Vec<String>,
    pub origin_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MovePayload {
    pub kind: String,
    pub files: Vec<BundleFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveManifest {
    pub format: u64,
    pub move_id: String,
    pub session_id: String,
    pub session_file_name: String,
    pub source_cwd: String,
    pub repository: Option<RepositoryIdentity>,
    pub session: BundleFile,
    pub sidecars: Vec<BundleFile>,
    pub dropped_user_messages: Vec<String>,
    pub payloads: Vec<MovePayload>,
}

/// SHA-256 of a file, lowercase hex.
///
/// # Errors
/// The file cannot be read.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 65_536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A listed path must stay inside the bundle, whichever OS wrote it.
fn leaves_bundle(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    path.is_empty()
        || drive
        || path.starts_with(['/', '\\'])
        || path.split(['/', '\\']).any(|segment| segment == "..")
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
}

fn bundle_file(value: &Value) -> Result<BundleFile, MoveError> {
    let (Some(path), Some(sha256)) = (
        value.get("path").and_then(Value::as_str),
        value.get("sha256").and_then(Value::as_str),
    ) else {
        return Err(MoveError::invalid_bundle("a listed file is malformed"));
    };
    if leaves_bundle(path) {
        return Err(MoveError::invalid_bundle(format!(
            "a listed path leaves the bundle: {path}"
        )));
    }
    Ok(BundleFile {
        path: path.to_owned(),
        sha256: sha256.to_owned(),
    })
}

fn listed<T>(
    value: Option<&Value>,
    parse: impl Fn(&Value) -> Result<T, MoveError>,
) -> Result<Vec<T>, MoveError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(parse).collect(),
        Some(_) => Err(MoveError::invalid_bundle("a manifest list is malformed")),
    }
}

fn payload(value: &Value) -> Result<MovePayload, MoveError> {
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return Err(MoveError::invalid_bundle("a payload is malformed"));
    };
    if kind != EXTENSION_STATE_PAYLOAD {
        return Err(MoveError::invalid_bundle(
            "it carries a payload this version cannot import",
        ));
    }
    let files = listed(value.get("files"), bundle_file)?;
    if let Some(stray) = files.iter().find(|f| {
        // `state//etc/passwd` passes the whole-path check but leaves the folder once the prefix is
        // stripped, and `join` of an absolute path discards the destination.
        !f.path
            .strip_prefix(EXTENSION_STATE_PREFIX)
            .is_some_and(|rest| !leaves_bundle(rest))
    }) {
        return Err(MoveError::invalid_bundle(format!(
            "an extension-state file is listed outside state/: {}",
            stray.path
        )));
    }
    Ok(MovePayload {
        kind: kind.to_owned(),
        files,
    })
}

fn commit_hash(value: &str) -> bool {
    (40..=64).contains(&value.len())
        && value
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// `repository` of a manifest; `None` when it names neither a root commit nor an origin.
fn repository(value: &Value) -> Option<RepositoryIdentity> {
    let roots = value.get("rootCommits")?.as_array()?;
    let origin_url = match value.get("originUrl") {
        None | Some(Value::Null) => None,
        Some(Value::String(url)) => Some(url.clone()),
        Some(_) => return None,
    };
    let mut root_commits: Vec<String> = roots
        .iter()
        .filter_map(Value::as_str)
        .filter(|c| commit_hash(c))
        .map(str::to_owned)
        .collect();
    root_commits.sort();
    (!root_commits.is_empty() || origin_url.is_some()).then_some(RepositoryIdentity {
        root_commits,
        origin_url,
    })
}

/// Read and validate a bundle's manifest.
///
/// # Errors
/// `move_bundle_invalid` for an unreadable or malformed manifest.
pub fn read_manifest(bundle_dir: &Path) -> Result<MoveManifest, MoveError> {
    let raw: Value = fs::read_to_string(bundle_dir.join(MANIFEST_FILE))
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|e| MoveError::invalid_bundle(format!("manifest unreadable: {e}")))?;
    if raw.get("format").and_then(Value::as_u64) != Some(MOVE_BUNDLE_FORMAT) {
        return Err(MoveError::invalid_bundle(format!(
            "unknown bundle format {}",
            raw.get("format").unwrap_or(&Value::Null)
        )));
    }
    let text = |key: &str| {
        raw.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| MoveError::invalid_bundle(format!("missing {key}")))
    };
    let id = |key: &str, what: &str| -> Result<String, MoveError> {
        let value = text(key)?;
        if is_plain_id(value) {
            Ok(value.to_owned())
        } else {
            Err(MoveError::invalid_bundle(format!(
                "the {what} is not a plain id: {value:?}"
            )))
        }
    };
    let session_file_name = text("sessionFileName")?;
    // The importer joins this name onto its session folder: a plain `<id>.jsonl` and nothing else.
    if !session_file_name
        .strip_suffix(".jsonl")
        .is_some_and(is_plain_id)
        || session_file_name.contains("..")
    {
        return Err(MoveError::invalid_bundle(format!(
            "the session file name is not a plain file name: {session_file_name}"
        )));
    }
    let dropped_user_messages = listed(raw.get("droppedUserMessages"), |v| {
        v.as_str()
            .map(str::to_owned)
            .ok_or_else(|| MoveError::invalid_bundle("malformed queued input"))
    })?;
    Ok(MoveManifest {
        format: MOVE_BUNDLE_FORMAT,
        move_id: id("moveId", "move id")?,
        session_id: id("sessionId", "session id")?,
        session_file_name: session_file_name.to_owned(),
        source_cwd: text("sourceCwd")?.to_owned(),
        repository: raw.get("repository").and_then(repository),
        session: bundle_file(raw.get("session").unwrap_or(&Value::Null))?,
        sidecars: listed(raw.get("sidecars"), bundle_file)?,
        dropped_user_messages,
        payloads: listed(raw.get("payloads"), payload)?,
    })
}

/// Every listed file hashes as listed, and the session file's own id is the bundle's session id.
///
/// # Errors
/// `move_bundle_invalid` naming the first file that is missing or changed.
pub fn verify_bundle(bundle_dir: &Path, manifest: &MoveManifest) -> Result<(), MoveError> {
    let listed = std::iter::once(&manifest.session)
        .chain(&manifest.sidecars)
        .chain(manifest.payloads.iter().flat_map(|p| &p.files));
    for file in listed {
        let actual = sha256_file(&bundle_dir.join(&file.path))
            .map_err(|_| MoveError::invalid_bundle(format!("missing {}", file.path)))?;
        if actual != file.sha256 {
            return Err(MoveError::invalid_bundle(format!(
                "{} changed in transit",
                file.path
            )));
        }
    }
    let session = crate::read_values(&bundle_dir.join(&manifest.session.path)).unwrap_or_default();
    if header_session_id(&session) != Some(manifest.session_id.as_str()) {
        return Err(MoveError::invalid_bundle(
            "the session file's own id does not match the bundle's session id",
        ));
    }
    Ok(())
}

/// senpi `normalizeRemoteUrl`: `git@host:owner/repo.git`, `https://user@host:443/owner/repo/` and
/// `ssh://host/owner/repo` all read `host/owner/repo`.
#[must_use]
pub fn normalize_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let mut value = if trimmed.contains("://") {
        let bare = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
        let bare = bare.split_once('@').map_or(bare, |(_, rest)| rest);
        let (host, path) = bare.split_once('/').unwrap_or((bare, ""));
        let host = host
            .split_once(':')
            .filter(|(_, port)| port.bytes().all(|c| c.is_ascii_digit()))
            .map_or(host, |(host, _)| host);
        format!("{host}/{path}")
    } else {
        let bare = trimmed.rsplit_once('@').map_or(trimmed, |(_, rest)| rest);
        bare.split_once(':')
            .filter(|(_, path)| !path.starts_with('/'))
            .map_or_else(|| bare.to_owned(), |(host, path)| format!("{host}/{path}"))
    }
    .trim_end_matches('/')
    .to_lowercase();
    if value.ends_with(".git") {
        value.truncate(value.len() - 4);
    }
    value
}

/// A repository identity from `git rev-list --max-parents=0 HEAD` and the origin URL; `None`
/// when neither names anything (not a checkout).
#[must_use]
pub fn repository_identity(
    root_commits: &str,
    origin_url: Option<&str>,
) -> Option<RepositoryIdentity> {
    let mut roots: Vec<String> = root_commits
        .split_whitespace()
        .filter(|c| commit_hash(c))
        .map(str::to_owned)
        .collect();
    roots.sort();
    let origin_url = origin_url
        .filter(|url| !url.trim().is_empty())
        .map(normalize_remote_url);
    (!roots.is_empty() || origin_url.is_some()).then_some(RepositoryIdentity {
        root_commits: roots,
        origin_url,
    })
}

/// senpi `sameRepository`: a shared root commit, else the same origin. A side without an identity
/// (not a checkout) never matches.
#[must_use]
pub fn same_repository(a: Option<&RepositoryIdentity>, b: Option<&RepositoryIdentity>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if !a.root_commits.is_empty() && !b.root_commits.is_empty() {
        return a
            .root_commits
            .iter()
            .any(|root| b.root_commits.contains(root));
    }
    matches!((&a.origin_url, &b.origin_url), (Some(a), Some(b)) if a == b)
}

// ------------------------------------------------------------------------ extension state

fn session_parent(session_file: &Path) -> PathBuf {
    session_file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Every release attempt of a move stages under this folder beside the session file.
#[must_use]
pub fn outgoing_state_dir(session_file: &Path, move_id: &str) -> PathBuf {
    session_parent(session_file)
        .join(MOVE_STATE_DIR)
        .join("out")
        .join(move_id)
}

/// Where an import places a move's extension state until the session's extensions take it.
#[must_use]
pub fn moved_state_dir(session_file: &Path, move_id: &str) -> PathBuf {
    let stem = session_file
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    session_parent(session_file)
        .join(MOVE_STATE_DIR)
        .join("in")
        .join(format!("{stem}.{move_id}"))
}

/// The `state_attempt` of the last seal of this file for `move_id`, if that release staged one.
fn sealed_attempt(entries: &[Value], move_id: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| {
            custom_type(e) == Some(SESSION_RELEASED) && data_str(e, "move_id") == Some(move_id)
        })
        .and_then(|e| data_str(e, STATE_ATTEMPT_FIELD))
        .filter(|attempt| is_plain_id(attempt))
        .map(str::to_owned)
}

/// One release attempt's staging folder: the attempt id the seal records and the folder the
/// `session_move_export` handlers write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedState {
    pub attempt: String,
    pub dir: PathBuf,
}

/// Create a fresh, empty folder for one release attempt. Earlier attempts of the move that never
/// sealed the session are dead and removed.
///
/// # Errors
/// `move_id` is not plain, or the folder cannot be prepared.
pub fn prepare_outgoing_state(
    session_file: &Path,
    move_id: &str,
) -> Result<StagedState, MoveError> {
    plain(move_id, "move id")?;
    let root = outgoing_state_dir(session_file, move_id);
    let sealed = sealed_attempt(&entries(session_file)?, move_id);
    match fs::read_dir(&root) {
        Ok(listing) => {
            for entry in listing.flatten() {
                if Some(entry.file_name().to_string_lossy().as_ref()) != sealed.as_deref() {
                    remove(&entry.path())?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(MoveError::io("list", &root, &e)),
    }
    let attempt = new_attempt();
    let dir = root.join(&attempt);
    fs::create_dir_all(&dir).map_err(|e| MoveError::io("create", &dir, &e))?;
    Ok(StagedState { attempt, dir })
}

/// Abandon one attempt's staging folder: what it holds is never bundled.
pub fn discard_outgoing_state(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// One delivery of moved extension state to a freshly opened session: `dir` is a private copy the
/// handlers may consume, `redelivered` says an earlier delivery of the same state was attempted.
#[derive(Debug)]
pub struct MovedStateDelivery {
    pub move_id: String,
    pub dir: PathBuf,
    pub redelivered: bool,
    pending: PathBuf,
    bookkeeping: PathBuf,
}

/// The extension state an import placed beside `session_file` and nobody restored yet, copied for
/// one delivery; `None` when nothing is pending.
///
/// # Errors
/// The session file cannot be read or the copy cannot be made.
pub fn begin_moved_state_delivery(
    session_file: &Path,
) -> Result<Option<MovedStateDelivery>, MoveError> {
    // Every open of every session asks; only a file with imported state beside it is read.
    let stem = session_file
        .file_stem()
        .map(|s| format!("{}.", s.to_string_lossy()));
    let incoming = session_parent(session_file).join(MOVE_STATE_DIR).join("in");
    let pending_here = fs::read_dir(&incoming).is_ok_and(|listing| {
        listing.flatten().any(|e| {
            stem.as_deref()
                .is_some_and(|s| e.file_name().to_string_lossy().starts_with(s))
        })
    });
    if !pending_here || !session_file.is_file() {
        return Ok(None);
    }
    let entries = entries(session_file)?;
    let Some(move_id) = entries
        .iter()
        .rev()
        .find(|e| custom_type(e) == Some(SESSION_MOVED_IN))
        .and_then(|e| data_str(e, "move_id"))
        .filter(|id| is_plain_id(id))
    else {
        return Ok(None);
    };
    if entries.iter().any(|e| {
        custom_type(e) == Some(MOVED_STATE_RESTORED) && data_str(e, "move_id") == Some(move_id)
    }) {
        return Ok(None);
    }
    let pending = moved_state_dir(session_file, move_id);
    if !pending.is_dir() {
        return Ok(None);
    }
    let bookkeeping = session_parent(session_file)
        .join(MOVE_STATE_DIR)
        .join("delivery")
        .join(pending.file_name().unwrap_or_default());
    let marker = bookkeeping.join("delivered");
    let redelivered = marker.exists();
    if let Ok(listing) = fs::read_dir(&bookkeeping) {
        for entry in listing.flatten() {
            if entry.file_name().to_string_lossy().starts_with("attempt-") {
                remove(&entry.path())?;
            }
        }
    }
    fs::create_dir_all(&bookkeeping).map_err(|e| MoveError::io("create", &bookkeeping, &e))?;
    fs::write(&marker, "").map_err(|e| MoveError::io("write", &marker, &e))?;
    let dir = bookkeeping.join(format!("attempt-{}", new_attempt()));
    for from in files_under(&pending)? {
        let relative = from.strip_prefix(&pending).unwrap_or(&from);
        copy_file(&from, &dir.join(relative))?;
    }
    fs::create_dir_all(&dir).map_err(|e| MoveError::io("create", &dir, &e))?;
    Ok(Some(MovedStateDelivery {
        move_id: move_id.to_owned(),
        dir,
        redelivered,
        pending,
        bookkeeping,
    }))
}

impl MovedStateDelivery {
    /// End the delivery: its copy goes, and once `delivered` (the caller recorded
    /// [`MOVED_STATE_RESTORED`]) the pending state and its bookkeeping go too.
    ///
    /// # Errors
    /// A folder cannot be removed.
    pub fn finish(self, delivered: bool) -> Result<(), MoveError> {
        remove(&self.dir)?;
        if delivered {
            remove(&self.pending)?;
            remove(&self.bookkeeping)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ sender

/// What [`export_session`] bundles.
pub struct ExportOptions<'a> {
    pub session_file: &'a Path,
    pub move_id: &'a str,
    /// User input the release took out of the session; the receiver owes it a turn.
    pub dropped_user_messages: &'a [String],
    pub bundle_dir: &'a Path,
    /// The repository the session was recorded in (`None` outside a checkout).
    pub repository: Option<RepositoryIdentity>,
}

/// Write the move bundle of a session sealed for this move: the session file byte for byte, the
/// extension state the sealing release staged, and a manifest of their hashes. The bundle is built
/// beside `bundle_dir` and renamed into place.
///
/// # Errors
/// `move_not_released` unless the file is moving for this move, or I/O failure.
pub fn export_session(o: &ExportOptions<'_>) -> Result<MoveManifest, MoveError> {
    plain(o.move_id, "move id")?;
    let entries = entries(o.session_file)?;
    match session_ownership(&entries) {
        Ownership::Moving { move_id, .. } if move_id == o.move_id => {}
        _ => {
            return Err(MoveError::new(
                MoveError::NOT_RELEASED,
                "release the session for this move first",
            ))
        }
    }
    let session_id = header_session_id(&entries)
        .filter(|id| is_plain_id(id))
        .ok_or_else(|| {
            MoveError::new(
                MoveError::INVALID_REQUEST,
                "the session id is not a plain id",
            )
        })?
        .to_owned();
    let state = sealed_attempt(&entries, o.move_id)
        .map(|attempt| outgoing_state_dir(o.session_file, o.move_id).join(attempt));
    if let Some(state) = state.as_ref().filter(|s| !s.is_dir()) {
        return Err(MoveError::failed(format!(
            "The extension state the move release staged is missing ({}); the session was not exported without it.",
            state.display()
        )));
    }
    let staging = PathBuf::from(format!(
        "{}.partial-{}",
        o.bundle_dir.display(),
        std::process::id()
    ));
    remove(&staging)?;
    let session_copy = staging.join(SESSION_BUNDLE_PATH);
    copy_session_for_export(o.session_file, &session_copy)?;
    let mut state_files = Vec::new();
    if let Some(state) = &state {
        for from in files_under(state)? {
            let relative = from
                .strip_prefix(state)
                .unwrap_or(&from)
                .to_string_lossy()
                .replace('\\', "/");
            let path = format!("{EXTENSION_STATE_PREFIX}{relative}");
            let to = staging.join(&path);
            copy_file(&from, &to)?;
            state_files.push(BundleFile {
                sha256: sha256_file(&to).map_err(|e| MoveError::io("hash", &to, &e))?,
                path,
            });
        }
    }
    let manifest = MoveManifest {
        format: MOVE_BUNDLE_FORMAT,
        move_id: o.move_id.to_owned(),
        session_id,
        session_file_name: o
            .session_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        source_cwd: header(&entries)
            .and_then(|h| h.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        repository: o.repository.clone(),
        session: BundleFile {
            path: SESSION_BUNDLE_PATH.to_owned(),
            sha256: sha256_file(&session_copy)
                .map_err(|e| MoveError::io("hash", &session_copy, &e))?,
        },
        sidecars: Vec::new(),
        dropped_user_messages: o.dropped_user_messages.to_vec(),
        payloads: if state_files.is_empty() {
            Vec::new()
        } else {
            vec![MovePayload {
                kind: EXTENSION_STATE_PAYLOAD.to_owned(),
                files: state_files,
            }]
        },
    };
    let text =
        serde_json::to_string_pretty(&manifest).map_err(|e| MoveError::failed(e.to_string()))?;
    let manifest_path = staging.join(MANIFEST_FILE);
    fs::write(&manifest_path, format!("{text}\n"))
        .map_err(|e| MoveError::io("write", &manifest_path, &e))?;
    remove(o.bundle_dir)?;
    fs::rename(&staging, o.bundle_dir).map_err(|e| MoveError::io("rename", o.bundle_dir, &e))?;
    Ok(manifest)
}

/// Settle the sender's side once the receiver answered: `accepted` records `session_moved`, a
/// declined or failed move takes the session back (`session_reclaimed`), `receiving` changes
/// nothing. A settled move's bundle and staged state are removed. Returns the file's ownership.
///
/// # Errors
/// `move_mismatch` when the file or the kept bundle belongs to another move,
/// `move_already_moved` for a revoked receiver of a session already marked moved, or I/O failure.
pub fn finish_move(
    session_file: &Path,
    bundle_dir: &Path,
    move_id: &str,
    answer: &MoveStatus,
) -> Result<Ownership, MoveError> {
    plain(move_id, "move id")?;
    if bundle_dir.exists() {
        let kept = read_manifest(bundle_dir)?;
        if kept.move_id != move_id {
            return Err(MoveError::new(
                MoveError::MISMATCH,
                format!("the kept bundle belongs to {} not {move_id}", kept.move_id),
            ));
        }
    }
    let entries = entries(session_file)?;
    let session_id = header_session_id(&entries).unwrap_or_default().to_owned();
    match session_ownership(&entries) {
        Ownership::Writable => {}
        Ownership::Moving { move_id: owner, .. } | Ownership::Moved { move_id: owner, .. }
            if owner != move_id =>
        {
            return Err(MoveError::new(
                MoveError::MISMATCH,
                format!("session belongs to {owner} not {move_id}"),
            ));
        }
        Ownership::Moved { .. } => {
            if *answer == MoveStatus::AcceptedRevoked {
                return Err(MoveError::new(
                    MoveError::ALREADY_MOVED,
                    format!("receiver revoked {move_id}, but this session was already marked moved; no copy is writable"),
                ));
            }
        }
        Ownership::Moving { target, .. } => {
            let at = now_iso();
            let reclaimed = |reason: &str, detail: Option<&str>| {
                let mut data =
                    json!({"move_id": move_id, "session_id": session_id, "reason": reason});
                if let Some(detail) = detail {
                    data["detail"] = json!(detail);
                }
                data["reclaimed_at"] = json!(at);
                append_custom(session_file, SESSION_RECLAIMED, data)
            };
            match answer {
                MoveStatus::Receiving => return Ok(session_ownership(&entries)),
                MoveStatus::Accepted => append_custom(
                    session_file,
                    SESSION_MOVED,
                    json!({"move_id": move_id, "session_id": session_id, "target": target, "moved_at": at}),
                )?,
                MoveStatus::AcceptedRevoked => reclaimed("revoked", None)?,
                MoveStatus::Declined(reason) => reclaimed("declined", Some(reason.as_str()))?,
                MoveStatus::Failed(reason) => reclaimed("failed", Some(reason.as_str()))?,
            }
        }
    }
    remove(bundle_dir)?;
    remove(&outgoing_state_dir(session_file, move_id))?;
    Ok(session_ownership(&self::entries(session_file)?))
}

// ------------------------------------------------------------------------------ receiver

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineReason {
    NoRecord,
    Expired,
    Cancelled,
    ChunkWithoutReady,
    Repository,
}

impl DeclineReason {
    const ALL: [Self; 5] = [
        Self::NoRecord,
        Self::Expired,
        Self::Cancelled,
        Self::ChunkWithoutReady,
        Self::Repository,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoRecord => "no_record",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::ChunkWithoutReady => "chunk_without_ready",
            Self::Repository => "repository",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    BundleInvalid,
    ImportFailed,
}

impl FailReason {
    const ALL: [Self; 2] = [Self::BundleInvalid, Self::ImportFailed];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BundleInvalid => "bundle_invalid",
            Self::ImportFailed => "import_failed",
        }
    }
}

/// The answers a sender can hear (`move_session status`) and hand back to `finish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveStatus {
    Receiving,
    Accepted,
    /// An accepted move the receiver later revoked (`{state:"accepted", revoked:true}`).
    AcceptedRevoked,
    Declined(DeclineReason),
    Failed(FailReason),
}

impl MoveStatus {
    #[must_use]
    pub fn to_wire(self) -> Value {
        match self {
            Self::Receiving => json!({"state": "receiving"}),
            Self::Accepted => json!({"state": "accepted"}),
            Self::AcceptedRevoked => json!({"state": "accepted", "revoked": true}),
            Self::Declined(reason) => json!({"state": "declined", "reason": reason.as_str()}),
            Self::Failed(reason) => json!({"state": "failed", "reason": reason.as_str()}),
        }
    }

    /// A receiver's answer as `finish` takes it; `None` for anything else (a `ready` record
    /// included).
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        let reason = value.get("reason").and_then(Value::as_str);
        match value.get("state")?.as_str()? {
            "receiving" => Some(Self::Receiving),
            "accepted" if value.get("revoked").and_then(Value::as_bool) == Some(true) => {
                Some(Self::AcceptedRevoked)
            }
            "accepted" => Some(Self::Accepted),
            "declined" => DeclineReason::ALL
                .into_iter()
                .find(|r| Some(r.as_str()) == reason)
                .map(Self::Declined),
            "failed" => FailReason::ALL
                .into_iter()
                .find(|r| Some(r.as_str()) == reason)
                .map(Self::Failed),
            _ => None,
        }
    }
}

/// One ledger record: `ready` until a verified bundle starts landing, then an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveRecord {
    Ready { expires_at_ms: i64 },
    Status(MoveStatus),
}

impl MoveRecord {
    fn state(self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Status(MoveStatus::Receiving) => "receiving",
            Self::Status(MoveStatus::Accepted | MoveStatus::AcceptedRevoked) => "accepted",
            Self::Status(MoveStatus::Declined(_)) => "declined",
            Self::Status(MoveStatus::Failed(_)) => "failed",
        }
    }

    fn is_final(self) -> bool {
        !matches!(
            self,
            Self::Ready { .. } | Self::Status(MoveStatus::Receiving)
        )
    }

    /// The record as `prepare`/`cancel` answer it.
    #[must_use]
    pub fn to_wire(self) -> Value {
        match self {
            Self::Ready { expires_at_ms } => {
                json!({"state": "ready", "expires_at": iso_from_ms(expires_at_ms)})
            }
            Self::Status(status) => status.to_wire(),
        }
    }
}

/// Where an accepted import lands, and whether its final rename was under way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Landing {
    target: Option<String>,
    committing: bool,
}

/// Whether the session file at a landing target already records the move. An unreadable file is
/// an error, never "not landed": a transient read failure must not settle a landed move as failed.
fn moved_in(file: &Path, move_id: &str) -> Result<bool, MoveError> {
    // A missing file is not landed; any other metadata failure (a permission problem, an I/O
    // error) is an error, never "not landed": settling a landed move as failed lets the sender
    // reclaim a session that lives here now.
    match fs::metadata(file) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(MoveError::io("stat", file, &e)),
    }
    let entries = entries(file)?;
    Ok(entries.iter().any(|e| {
        custom_type(e) == Some(SESSION_MOVED_IN) && data_str(e, "move_id") == Some(move_id)
    }))
}

/// The receiving machine's record of every move it was offered, one JSON file per move id under
/// `dir`. Every step runs under an exclusive lock file per move, across processes.
pub struct MoveLedger {
    dir: PathBuf,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl MoveLedger {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_clock(dir, now_ms)
    }

    /// A ledger reading time (epoch milliseconds) from `now`.
    #[must_use]
    pub fn with_clock(
        dir: impl Into<PathBuf>,
        now: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            dir: dir.into(),
            now: Box::new(now),
        }
    }

    fn path(&self, move_id: &str) -> Result<PathBuf, MoveError> {
        Ok(self
            .dir
            .join(format!("{}.json", plain(move_id, "move id")?)))
    }

    fn locked<T>(
        &self,
        move_id: &str,
        work: impl FnOnce(&Path) -> Result<T, MoveError>,
    ) -> Result<T, MoveError> {
        let path = self.path(move_id)?;
        fs::create_dir_all(&self.dir).map_err(|e| MoveError::io("create", &self.dir, &e))?;
        let lock_path = path.with_extension("lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| MoveError::io("open", &lock_path, &e))?;
        lock.lock()
            .map_err(|e| MoveError::io("lock", &lock_path, &e))?;
        work(&path)
    }

    fn read(path: &Path) -> Result<(Option<MoveRecord>, Landing), MoveError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((None, Landing::default()))
            }
            Err(e) => return Err(MoveError::io("read", path, &e)),
        };
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| MoveError::failed(format!("move record {}: {e}", path.display())))?;
        let record = if value.get("state").and_then(Value::as_str) == Some("ready") {
            value
                .get("expiresAtMs")
                .and_then(Value::as_i64)
                .map(|expires_at_ms| MoveRecord::Ready { expires_at_ms })
        } else {
            MoveStatus::parse(&value).map(MoveRecord::Status)
        };
        let Some(record) = record else {
            return Err(MoveError::failed(format!(
                "Move record {} is not a move record",
                path.display()
            )));
        };
        let landing = Landing {
            target: value
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_owned),
            committing: value.get("committing").and_then(Value::as_bool) == Some(true),
        };
        Ok((Some(record), landing))
    }

    fn write(path: &Path, record: MoveRecord, landing: &Landing) -> Result<(), MoveError> {
        let mut body = match record {
            MoveRecord::Ready { expires_at_ms } => json!({
                "state": "ready", "expiresAt": iso_from_ms(expires_at_ms), "expiresAtMs": expires_at_ms,
            }),
            MoveRecord::Status(status) => status.to_wire(),
        };
        if let Some(target) = &landing.target {
            body["target"] = json!(target);
        }
        if landing.committing {
            body["committing"] = json!(true);
        }
        body["at"] = json!(now_iso());
        let temp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
        write_synced(&temp, format!("{body}\n").as_bytes())?;
        fs::rename(&temp, path).map_err(|e| MoveError::io("rename", path, &e))
    }

    /// The record, with a ready move whose window passed declined as expired first (on disk).
    fn current(&self, path: &Path) -> Result<(Option<MoveRecord>, Landing), MoveError> {
        let (record, landing) = Self::read(path)?;
        match record {
            Some(MoveRecord::Ready { expires_at_ms }) if (self.now)() >= expires_at_ms => {
                let expired = MoveRecord::Status(MoveStatus::Declined(DeclineReason::Expired));
                Self::write(path, expired, &Landing::default())?;
                Ok((Some(expired), Landing::default()))
            }
            _ => Ok((record, landing)),
        }
    }

    fn fenced(move_id: &str, record: Option<MoveRecord>) -> MoveError {
        let state = record.map_or("no record", MoveRecord::state);
        MoveError::new(
            MoveError::FENCED,
            format!(
                "Move {move_id} can no longer be imported here: it was already answered ({state})."
            ),
        )
    }

    /// Say this machine can take the move, for [`READY_TTL_MS`]. Preparing again keeps the first
    /// window; a move already receiving or accepted stays so.
    ///
    /// # Errors
    /// `move_fenced` for a move already declined or failed here.
    pub fn prepare(&self, move_id: &str) -> Result<MoveRecord, MoveError> {
        self.locked(move_id, |path| match self.current(path)?.0 {
            None => {
                let ready = MoveRecord::Ready {
                    expires_at_ms: (self.now)() + READY_TTL_MS,
                };
                Self::write(path, ready, &Landing::default())?;
                Ok(ready)
            }
            Some(
                record @ (MoveRecord::Ready { .. }
                | MoveRecord::Status(
                    MoveStatus::Receiving | MoveStatus::Accepted | MoveStatus::AcceptedRevoked,
                )),
            ) => Ok(record),
            other => Err(Self::fenced(move_id, other)),
        })
    }

    /// Start receiving a verified bundle at `target`. A bundle for a move this machine never readied
    /// is declined for good (`chunk_without_ready`).
    fn begin_receive(&self, move_id: &str, target: &str) -> Result<(), MoveError> {
        self.locked(move_id, |path| {
            let (record, landing) = self.current(path)?;
            match record {
                Some(MoveRecord::Ready { .. }) => Self::write(
                    path,
                    MoveRecord::Status(MoveStatus::Receiving),
                    &Landing {
                        target: Some(target.to_owned()),
                        committing: false,
                    },
                ),
                Some(MoveRecord::Status(MoveStatus::Receiving))
                    if !landing.committing && landing.target.as_deref() == Some(target) =>
                {
                    Ok(())
                }
                None => {
                    let declined =
                        MoveRecord::Status(MoveStatus::Declined(DeclineReason::ChunkWithoutReady));
                    Self::write(path, declined, &Landing::default())?;
                    Err(Self::fenced(move_id, Some(declined)))
                }
                other => Err(Self::fenced(move_id, other)),
            }
        })
    }

    /// Record a final answer unless one is there already: once a sender may have heard it, it
    /// never changes. `keeps` names the open states this transition leaves alone.
    fn settle(
        &self,
        move_id: &str,
        next: MoveRecord,
        keeps: &[&str],
    ) -> Result<MoveRecord, MoveError> {
        self.locked(move_id, |path| {
            let (record, landing) = self.current(path)?;
            if let Some(record) = record {
                if record.is_final() || keeps.contains(&record.state()) || landing.committing {
                    return Ok(record);
                }
            }
            Self::write(path, next, &Landing::default())?;
            Ok(next)
        })
    }

    /// Cancel or refuse a move that has not started receiving.
    ///
    /// # Errors
    /// I/O failure.
    pub fn decline(&self, move_id: &str, reason: DeclineReason) -> Result<MoveRecord, MoveError> {
        self.settle(
            move_id,
            MoveRecord::Status(MoveStatus::Declined(reason)),
            &["receiving"],
        )
    }

    fn fail_unless_landed(
        &self,
        move_id: &str,
        reason: FailReason,
        landed: bool,
    ) -> Result<(), MoveError> {
        if !landed {
            self.settle(move_id, MoveRecord::Status(MoveStatus::Failed(reason)), &[])?;
        }
        Ok(())
    }

    /// A file that already records this move landed at `target`: answer it accepted.
    fn confirm_landed(&self, move_id: &str, target: &str) -> Result<(), MoveError> {
        self.locked(move_id, |path| {
            let (record, landing) = Self::read(path)?;
            let open = matches!(
                record,
                None | Some(MoveRecord::Status(
                    MoveStatus::Receiving | MoveStatus::Accepted | MoveStatus::AcceptedRevoked
                ))
            );
            if !open || landing.target.as_deref().is_some_and(|t| t != target) {
                return Err(Self::fenced(move_id, record));
            }
            Self::write(
                path,
                MoveRecord::Status(MoveStatus::Accepted),
                &Landing {
                    target: Some(target.to_owned()),
                    committing: false,
                },
            )
        })
    }

    /// Run the import's commit under the move's lock: `prepare` (nothing lands if it refuses), the
    /// `committing` stamp, the final `land`, then `accepted`.
    fn commit<T>(
        &self,
        move_id: &str,
        target: &str,
        prepare: impl FnOnce() -> Result<(), MoveError>,
        land: impl FnOnce() -> Result<T, MoveError>,
    ) -> Result<T, MoveError> {
        self.locked(move_id, |path| {
            let (record, landing) = Self::read(path)?;
            let lands_here = record == Some(MoveRecord::Status(MoveStatus::Receiving))
                && !landing.committing
                && landing.target.as_deref() == Some(target);
            if !lands_here {
                return Err(Self::fenced(move_id, record));
            }
            prepare()?;
            let at = |committing| Landing {
                target: Some(target.to_owned()),
                committing,
            };
            Self::write(path, MoveRecord::Status(MoveStatus::Receiving), &at(true))?;
            let landed = land()?;
            Self::write(path, MoveRecord::Status(MoveStatus::Accepted), &at(false))?;
            Ok(landed)
        })
    }

    /// The signed answer a sender gets. `moved_in`: the session file the sender named already
    /// records the move. A move this machine never heard of is declined (`no_record`) before the
    /// answer leaves; an import that stopped mid-commit is settled from its landing file.
    fn answer(&self, move_id: &str, moved_in_here: bool) -> Result<MoveStatus, MoveError> {
        self.locked(move_id, |path| {
            let (mut record, landing) = self.current(path)?;
            if let (Some(MoveRecord::Status(MoveStatus::Receiving)), true, Some(target)) =
                (record, landing.committing, landing.target.as_deref())
            {
                let settled = match moved_in(Path::new(target), move_id) {
                    Ok(true) => MoveStatus::Accepted,
                    Ok(false) => MoveStatus::Failed(FailReason::ImportFailed),
                    Err(e) => return Err(e),
                };
                Self::write(
                    path,
                    MoveRecord::Status(settled),
                    &Landing {
                        target: Some(target.to_owned()),
                        committing: false,
                    },
                )?;
                record = Some(MoveRecord::Status(settled));
            }
            match record {
                Some(MoveRecord::Status(
                    status @ (MoveStatus::Accepted | MoveStatus::AcceptedRevoked),
                )) => Ok(status),
                _ if moved_in_here => Ok(MoveStatus::Accepted),
                Some(MoveRecord::Ready { .. }) => Ok(MoveStatus::Receiving),
                Some(MoveRecord::Status(status)) => Ok(status),
                None => {
                    let declined = MoveStatus::Declined(DeclineReason::NoRecord);
                    Self::write(path, MoveRecord::Status(declined), &Landing::default())?;
                    Ok(declined)
                }
            }
        })
    }
}

/// `move_session status`: the receiver's answer for `move_id`. With `session_file` (the landed
/// copy the sender expects), that file must be this move's import.
///
/// # Errors
/// `move_mismatch` when `session_file` exists but is not this move's copy, or I/O failure.
pub fn answer_move_status(
    ledger: &MoveLedger,
    move_id: &str,
    session_file: Option<&Path>,
) -> Result<MoveStatus, MoveError> {
    plain(move_id, "move id")?;
    match session_file.filter(|f| f.is_file()) {
        Some(file) if !moved_in(file, move_id)? => Err(MoveError::new(
            MoveError::MISMATCH,
            format!(
                "{} is not the copy this device imported for move {move_id}; nothing was answered",
                file.display()
            ),
        )),
        Some(_) => ledger.answer(move_id, true),
        None => ledger.answer(move_id, false),
    }
}

/// What [`import_session`] lands.
pub struct ImportOptions<'a> {
    pub bundle_dir: &'a Path,
    /// The checkout the session continues in; its header `cwd` is rewritten to this.
    pub target_cwd: &'a Path,
    /// The engine's session folder for `target_cwd`; the file lands under its bundled name.
    pub session_dir: &'a Path,
    /// The sending machine, recorded in `session_moved_in`.
    pub from: &'a str,
    pub ledger: &'a MoveLedger,
    /// The repository `target_cwd` belongs to (`None` outside a checkout).
    pub target_repository: Option<RepositoryIdentity>,
}

/// Land a verified bundle as a session file of this machine: stage it with a `session_moved_in`
/// entry, place its extension state for the session's extensions, rebind the header `cwd`, and
/// rename it into place under the move's ledger commit. A file that already landed for this move is
/// confirmed, never imported twice.
///
/// # Errors
/// `move_bundle_invalid`, `move_repository_mismatch`, `move_fenced` (the move was not prepared or
/// was already answered), `move_target_occupied` (a writable session sits at the target), or I/O
/// failure; each failure is recorded in the ledger before it is returned.
pub fn import_session(o: &ImportOptions<'_>) -> Result<PathBuf, MoveError> {
    let manifest = read_manifest(o.bundle_dir)?;
    let target = o.session_dir.join(&manifest.session_file_name);
    let target_text = target.to_string_lossy().into_owned();
    let landed = || moved_in(&target, &manifest.move_id).unwrap_or(false);
    if landed() {
        o.ledger.confirm_landed(&manifest.move_id, &target_text)?;
        return Ok(target);
    }
    if let Err(error) = verify_bundle(o.bundle_dir, &manifest) {
        o.ledger
            .fail_unless_landed(&manifest.move_id, FailReason::BundleInvalid, landed())?;
        return Err(error);
    }
    if !same_repository(manifest.repository.as_ref(), o.target_repository.as_ref()) {
        o.ledger
            .decline(&manifest.move_id, DeclineReason::Repository)?;
        return Err(MoveError::new(
            MoveError::REPOSITORY_MISMATCH,
            format!(
                "{} is not the same repository the session was recorded in ({})",
                o.target_cwd.display(),
                manifest.source_cwd
            ),
        ));
    }
    o.ledger.begin_receive(&manifest.move_id, &target_text)?;
    // One importer per move and per destination file: a retry never stages into a folder another
    // import is copying into, and two moves never race for the same landing path. A lock that
    // cannot be taken fails the move here, through the ledger, like any other import failure.
    let staging_root = o.session_dir.join(".move-staging");
    let staged_locks =
        StagingLock::hold(&staging_root, "move", &manifest.move_id).and_then(|lock| {
            StagingLock::hold(&staging_root, "target", &manifest.session_file_name)
                .map(|target| (lock, target))
        });
    let (_move_lock, _target_lock) = match staged_locks {
        Ok(locks) => locks,
        Err(e) => {
            o.ledger
                .fail_unless_landed(&manifest.move_id, FailReason::ImportFailed, landed())?;
            return Err(e);
        }
    };
    let staging = staging_root.join(&manifest.move_id);
    let result = stage(o.bundle_dir, &manifest, &staging, o.from).and_then(|staged| {
        let landed_file = o.ledger.commit(
            &manifest.move_id,
            &target_text,
            || {
                check_predecessor(&target, &manifest.session_id)?;
                place_extension_state(
                    &staging,
                    &manifest,
                    &moved_state_dir(&target, &manifest.move_id),
                )
            },
            || land(&staged, &target, o.target_cwd),
        )?;
        remove(&staging)?;
        Ok(landed_file)
    });
    if let Err(error) = &result {
        if error.code != MoveError::FENCED {
            let reason = if error.code == MoveError::BUNDLE_INVALID {
                FailReason::BundleInvalid
            } else {
                FailReason::ImportFailed
            };
            o.ledger
                .fail_unless_landed(&manifest.move_id, reason, landed())?;
        }
    }
    result
}

/// Copy the bundled session into `staging` and record `session_moved_in` there (kept across
/// retries once written), with the extension-state payloads re-verified beside it.
fn stage(
    bundle_dir: &Path,
    manifest: &MoveManifest,
    staging: &Path,
    from: &str,
) -> Result<PathBuf, MoveError> {
    let file = staging.join(&manifest.session_file_name);
    if !moved_in(&file, &manifest.move_id).unwrap_or(false) {
        remove(staging)?;
        copy_file(&bundle_dir.join(&manifest.session.path), &file)?;
        // The bundle can change between its verification and this copy; the journal that lands is
        // the one the sender listed, digest and all.
        if sha256_file(&file).map_err(|e| MoveError::io("hash", &file, &e))?
            != manifest.session.sha256
        {
            return Err(MoveError::invalid_bundle(
                "the session file changed in transit",
            ));
        }
        for listed in manifest.payloads.iter().flat_map(|p| &p.files) {
            let to = staging.join(".payloads").join(&listed.path);
            copy_file(&bundle_dir.join(&listed.path), &to)?;
            if sha256_file(&to).map_err(|e| MoveError::io("hash", &to, &e))? != listed.sha256 {
                return Err(MoveError::invalid_bundle(format!(
                    "{} changed in transit",
                    listed.path
                )));
            }
        }
        append_custom(
            &file,
            SESSION_MOVED_IN,
            json!({
                "move_id": manifest.move_id, "session_id": manifest.session_id, "from": from,
                "bundle_sha256": manifest.session.sha256, "moved_in_at": now_iso(),
            }),
        )?;
    }
    Ok(file)
}

/// A file at the target may only be this session's own copy that already moved away: a writable
/// session there is never overwritten, and a moving one still belongs to its own move.
fn check_predecessor(target: &Path, session_id: &str) -> Result<(), MoveError> {
    if !target.exists() {
        return Ok(());
    }
    let entries = entries(target)?;
    let refused = header_session_id(&entries) != Some(session_id)
        || !matches!(session_ownership(&entries), Ownership::Moved { .. });
    if refused {
        return Err(MoveError::new(
            MoveError::TARGET_OCCUPIED,
            format!(
                "a session that is not a moved copy sits at {}",
                target.display()
            ),
        ));
    }
    Ok(())
}

fn place_extension_state(
    staging: &Path,
    manifest: &MoveManifest,
    dir: &Path,
) -> Result<(), MoveError> {
    let files: Vec<&BundleFile> = manifest
        .payloads
        .iter()
        .filter(|p| p.kind == EXTENSION_STATE_PAYLOAD)
        .flat_map(|p| &p.files)
        .collect();
    if files.is_empty() {
        return Ok(());
    }
    remove(dir)?;
    for file in files {
        let relative = file
            .path
            .strip_prefix(EXTENSION_STATE_PREFIX)
            .ok_or_else(|| MoveError::invalid_bundle("invalid extension state path"))?;
        copy_file(
            &staging.join(".payloads").join(&file.path),
            &dir.join(relative),
        )?;
    }
    Ok(())
}

/// Rebind the staged file to this machine's checkout (only the header's `cwd` changes; every
/// other line, a title slot included, stays byte for byte) and rename it into place.
fn land(staged: &Path, target: &Path, cwd: &Path) -> Result<PathBuf, MoveError> {
    let content = fs::read_to_string(staged).map_err(|e| MoveError::io("read", staged, &e))?;
    let mut out = String::with_capacity(content.len() + 64);
    let mut rebound = false;
    for line in content.split_inclusive('\n') {
        if !rebound {
            if let Ok(mut header) =
                serde_json::from_str::<Value>(line.trim_end_matches(['\n', '\r']))
            {
                if header.get("type").and_then(Value::as_str) == Some("session") {
                    header["cwd"] = json!(cwd.to_string_lossy());
                    out.push_str(&header.to_string());
                    out.push('\n');
                    rebound = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    if !rebound {
        return Err(MoveError::invalid_bundle("the session file has no header"));
    }
    let dir = target
        .parent()
        .ok_or_else(|| MoveError::failed("missing target directory"))?;
    fs::create_dir_all(dir).map_err(|e| MoveError::io("create", dir, &e))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.rebind-{}.tmp", std::process::id()));
    write_synced(&temp, out.as_bytes())?;
    // The predecessor goes only now, with the replacement complete: a failure above leaves the
    // local session as it was.
    if target.exists() {
        remove(target)?;
    }
    fs::rename(&temp, target).map_err(|e| MoveError::io("rename", target, &e))?;
    remove(staged)?;
    Ok(target.to_path_buf())
}

// ------------------------------------------------------------------------------ files & time

/// One importer's exclusive hold on a staging folder or a landing path (an OS file lock, released
/// when dropped).
struct StagingLock {
    _file: fs::File,
}

impl StagingLock {
    /// A lock file per name, in its own namespace: a move id can spell another move's target
    /// name (`s1.jsonl.target`), and the two holds must never share one lock file.
    fn hold(dir: &Path, namespace: &str, name: &str) -> Result<Self, MoveError> {
        if !is_plain_id(name) {
            return Err(MoveError::new(
                MoveError::INVALID_REQUEST,
                format!("the staging name is not plain: {name:?}"),
            ));
        }
        fs::create_dir_all(dir).map_err(|e| MoveError::io("create", dir, &e))?;
        let path = dir.join(format!("{namespace}-{name}.lock"));
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| MoveError::io("open", &path, &e))?;
        file.lock().map_err(|e| MoveError::io("lock", &path, &e))?;
        Ok(Self { _file: file })
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), MoveError> {
    let mut file = fs::File::create(path).map_err(|e| MoveError::io("create", path, &e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| MoveError::io("write", path, &e))
}

fn line_is_type(bytes: &[u8], expected: &str) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    serde_json::from_str::<Value>(text.trim_end_matches(['\r', '\n']))
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|t| t == expected)
}

/// Copy a sealed session file into a move bundle, dropping any leading `title` slot line so
/// line 1 is always the `session` header (`mhc`/`senpi` and `session-identity` require it).
fn copy_session_for_export(source: &Path, destination: &Path) -> Result<(), MoveError> {
    use std::io::BufRead;
    let file = fs::File::open(source).map_err(|e| MoveError::io("open", source, &e))?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = Vec::new();
    let n1 = reader
        .read_until(b'\n', &mut first)
        .map_err(|e| MoveError::io("read", source, &e))?;
    if n1 > 0 && line_is_type(&first, "title") {
        let mut second = Vec::new();
        let n2 = reader
            .read_until(b'\n', &mut second)
            .map_err(|e| MoveError::io("read", source, &e))?;
        if n2 > 0 && line_is_type(&second, "session") {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|e| MoveError::io("create", parent, &e))?;
            }
            let mut out = fs::File::create(destination)
                .map_err(|e| MoveError::io("create", destination, &e))?;
            out.write_all(&second)
                .and_then(|()| std::io::copy(&mut reader, &mut out).map(|_| ()))
                .map_err(|e| MoveError::io("write", destination, &e))?;
            return Ok(());
        }
    }
    drop(reader);
    copy_file(source, destination)
}

fn copy_file(from: &Path, to: &Path) -> Result<(), MoveError> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| MoveError::io("create", parent, &e))?;
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| MoveError::io("copy", from, &e))
}

/// Remove a file or a whole folder; a missing one is already removed.
fn remove(path: &Path) -> Result<(), MoveError> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(MoveError::io("remove", path, &e))
        }
        _ => Ok(()),
    }
}

/// Every regular file under `dir`, sorted (missing folder: none).
fn files_under(dir: &Path) -> Result<Vec<PathBuf>, MoveError> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let listing = match fs::read_dir(&next) {
            Ok(listing) => listing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(MoveError::io("list", &next, &e)),
        };
        for entry in listing {
            let path = entry.map_err(|e| MoveError::io("list", &next, &e))?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn new_attempt() -> String {
    format!("{:08x}{:08x}", crate::rand_u32(), crate::rand_u32())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn now_iso() -> String {
    iso_from_ms(now_ms())
}

/// ISO-8601 UTC with milliseconds (`2026-11-23T01:02:03.456Z`).
fn iso_from_ms(ms: i64) -> String {
    // Days to civil date (Howard Hinnant's `civil_from_days`).
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = ms.rem_euclid(86_400_000);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    fn header_line(id: &str, cwd: &str) -> String {
        format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-11-23T00:00:00.000Z","cwd":"{cwd}"}}"#
        )
    }

    /// A session file with a title slot, a header and one message.
    fn session(dir: &Path, id: &str, cwd: &str) -> PathBuf {
        let path = dir.join(format!("2026-11-23T00-00-00-000Z_{id}.jsonl"));
        let mut text = format!("{}\n", header_line(id, cwd));
        text.push_str(r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-11-23T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}"#);
        text.push('\n');
        fs::create_dir_all(dir).unwrap();
        fs::write(&path, text).unwrap();
        let mut writer = crate::SessionWriter::open(&path, None).unwrap();
        writer
            .update_title_slot("moved session", None, "2026-11-23T00:00:02.000Z")
            .unwrap();
        path
    }

    fn seal(file: &Path, move_id: &str, state_attempt: Option<&str>) {
        let id = header_session_id(&entries(file).unwrap())
            .unwrap()
            .to_owned();
        let mut data =
            json!({"reason": "move", "move_id": move_id, "target": "laptop", "session_id": id});
        if let Some(attempt) = state_attempt {
            data[STATE_ATTEMPT_FIELD] = json!(attempt);
        }
        append_custom(file, SESSION_RELEASED, data).unwrap();
    }

    fn repo() -> Option<RepositoryIdentity> {
        Some(RepositoryIdentity {
            root_commits: vec!["a".repeat(40)],
            origin_url: None,
        })
    }

    fn export(file: &Path, move_id: &str, bundle: &Path) -> Result<MoveManifest, MoveError> {
        export_session(&ExportOptions {
            session_file: file,
            move_id,
            dropped_user_messages: &["owed".to_owned()],
            bundle_dir: bundle,
            repository: repo(),
        })
    }

    fn import(
        bundle: &Path,
        cwd: &Path,
        sessions: &Path,
        ledger: &MoveLedger,
    ) -> Result<PathBuf, MoveError> {
        import_session(&ImportOptions {
            bundle_dir: bundle,
            target_cwd: cwd,
            session_dir: sessions,
            from: "desk",
            ledger,
            target_repository: repo(),
        })
    }

    #[test]
    fn remotes_normalize_to_host_and_path() {
        for url in [
            "git@github.com:jc01rho/oh-my-tau.git",
            "https://token@github.com:443/jc01rho/oh-my-tau/",
            "ssh://git@GitHub.com/jc01rho/oh-my-tau.git",
        ] {
            assert_eq!(
                normalize_remote_url(url),
                "github.com/jc01rho/oh-my-tau",
                "{url}"
            );
        }
        let root = "b".repeat(40);
        let identity = repository_identity(&format!("{root}\nnot-a-hash\n"), Some("")).unwrap();
        assert_eq!(identity.root_commits, vec![root]);
        assert_eq!(identity.origin_url, None);
        assert_eq!(repository_identity("", None), None);
    }

    #[test]
    fn plain_ids_start_alphanumeric_and_stay_short() {
        assert!(is_plain_id("abc-1.2_x"));
        assert!(is_plain_id(&"a".repeat(128)));
        assert!(!is_plain_id(&"a".repeat(129)));
        for bad in ["", ".", "..", ".hidden", "-x", "a/b", "a b"] {
            assert!(!is_plain_id(bad), "{bad}");
        }
    }

    #[test]
    fn ownership_follows_only_the_same_move_and_ignores_inherited_entries() {
        let header = json!({"type": "session", "id": "s"});
        let custom =
            |kind: &str, data: Value| json!({"type": "custom", "customType": kind, "data": data});
        let moving = custom(
            SESSION_RELEASED,
            json!({"reason": "move", "move_id": "m1", "target": "laptop", "session_id": "s"}),
        );
        assert_eq!(
            session_ownership(std::slice::from_ref(&header)),
            Ownership::Writable
        );
        let takeover = custom(SESSION_RELEASED, json!({"reason": "takeover"}));
        assert_eq!(
            session_ownership(&[header.clone(), takeover]),
            Ownership::Writable
        );
        let state = session_ownership(&[header.clone(), moving.clone()]);
        assert_eq!(
            state,
            Ownership::Moving {
                move_id: "m1".into(),
                target: "laptop".into()
            }
        );
        // Another move cannot take a moving session, and a copy's inherited seal is not a step.
        let other = custom(SESSION_MOVED, json!({"move_id": "m2", "session_id": "s"}));
        let inherited = custom(
            SESSION_MOVED,
            json!({"move_id": "m1", "session_id": "copy"}),
        );
        assert_eq!(
            session_ownership(&[header.clone(), moving.clone(), other, inherited]),
            state
        );
        let reclaimed = custom(
            SESSION_RECLAIMED,
            json!({"move_id": "m1", "session_id": "s"}),
        );
        assert_eq!(
            session_ownership(&[header.clone(), moving.clone(), reclaimed]),
            Ownership::Writable
        );
        let moved = custom(
            SESSION_MOVED,
            json!({"move_id": "m1", "target": "laptop", "session_id": "s"}),
        );
        let after = custom(
            SESSION_RECLAIMED,
            json!({"move_id": "m1", "session_id": "s"}),
        );
        assert_eq!(
            session_ownership(&[header, moving, moved, after]),
            Ownership::Moved {
                move_id: "m1".into(),
                target: "laptop".into()
            }
        );
    }

    #[test]
    fn export_needs_the_seal_of_this_move() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        let bundle = dir.path().join("bundle");
        assert_eq!(
            export(&file, "m1", &bundle).unwrap_err().code,
            MoveError::NOT_RELEASED
        );
        seal(&file, "m0", None);
        assert_eq!(
            export(&file, "m1", &bundle).unwrap_err().code,
            MoveError::NOT_RELEASED
        );
        assert!(!bundle.exists());
    }

    #[test]
    fn a_bundle_verifies_and_a_changed_byte_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        seal(&file, "m1", None);
        let bundle = dir.path().join("bundle");
        let manifest = export(&file, "m1", &bundle).unwrap();
        assert_eq!(read_manifest(&bundle).unwrap(), manifest);
        assert_eq!(manifest.dropped_user_messages, vec!["owed".to_owned()]);
        assert_eq!(manifest.source_cwd, "/repo");
        let original = fs::read_to_string(&file).unwrap();
        let (_, expected_body) = original.split_once('\n').unwrap();
        assert_eq!(
            fs::read_to_string(bundle.join(SESSION_BUNDLE_PATH)).unwrap(),
            expected_body
        );
        verify_bundle(&bundle, &manifest).unwrap();
        let mut bytes = fs::read(bundle.join(SESSION_BUNDLE_PATH)).unwrap();
        let last = bytes.len() - 2;
        bytes[last] ^= 1;
        fs::write(bundle.join(SESSION_BUNDLE_PATH), bytes).unwrap();
        let error = verify_bundle(&bundle, &manifest).unwrap_err();
        assert_eq!(error.code, MoveError::BUNDLE_INVALID);
        assert!(error.detail.contains("changed in transit"), "{error}");
    }

    #[test]
    fn manifest_paths_and_payload_kinds_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let base = json!({"format": 1, "moveId": "m1", "sessionId": "s1", "sessionFileName": "s1.jsonl",
            "sourceCwd": "/x", "session": {"path": SESSION_BUNDLE_PATH, "sha256": "b"}});
        let refused = |manifest: Value, needle: &str| {
            fs::write(dir.path().join(MANIFEST_FILE), manifest.to_string()).unwrap();
            let error = read_manifest(dir.path()).unwrap_err();
            assert_eq!(error.code, MoveError::BUNDLE_INVALID);
            assert!(error.detail.contains(needle), "{error}");
        };
        let mut escape = base.clone();
        escape["session"]["path"] = json!("../etc/passwd");
        refused(escape, "leaves the bundle");
        let mut name = base.clone();
        name["sessionFileName"] = json!("../x.jsonl");
        refused(name, "not a plain file name");
        let mut kind = base.clone();
        kind["payloads"] = json!([{"kind": "folder", "files": []}]);
        refused(kind, "cannot import");
        let mut rooted = base.clone();
        rooted["payloads"] = json!([{"kind": "extension-state",
            "files": [{"path": "state//etc/passwd", "sha256": "b"}]}]);
        refused(rooted, "extension-state file");
        let mut sibling = base.clone();
        sibling["payloads"] = json!([{"kind": "extension-state",
            "files": [{"path": "state/../../x", "sha256": "b"}]}]);
        refused(sibling, "leaves the bundle");
        let mut format = base;
        format["format"] = json!(2);
        refused(format, "unknown bundle format");
    }

    #[test]
    fn a_move_lands_once_with_its_state_and_a_rebound_header() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/desk/repo");
        let staged = prepare_outgoing_state(&file, "m1").unwrap();
        fs::create_dir_all(staged.dir.join("omo-nyang")).unwrap();
        fs::write(staged.dir.join("omo-nyang/manifest.json"), "{}").unwrap();
        seal(&file, "m1", Some(&staged.attempt));
        let bundle = dir.path().join("bundle");
        let manifest = export(&file, "m1", &bundle).unwrap();
        assert_eq!(
            manifest.payloads[0].files[0].path,
            "state/omo-nyang/manifest.json"
        );

        let ledger = MoveLedger::new(dir.path().join("ledger"));
        assert_eq!(ledger.prepare("m1").unwrap().to_wire()["state"], "ready");
        let sessions = dir.path().join("b");
        let landed = import(&bundle, Path::new("/laptop/repo"), &sessions, &ledger).unwrap();
        assert_eq!(landed, sessions.join(&manifest.session_file_name));
        // The exported bundle drops the leading title slot so line 1 is the `session` header;
        // only the header's `cwd` changes on landing, and every later line stays byte for byte.
        let original = fs::read_to_string(&file).unwrap();
        let moved = fs::read_to_string(&landed).unwrap();
        let (sent, got): (Vec<&str>, Vec<&str>) =
            (original.lines().collect(), moved.lines().collect());
        assert_eq!(
            serde_json::from_str::<Value>(got[0]).unwrap()["cwd"],
            "/laptop/repo"
        );
        assert_eq!(sent[2..], got[1..sent.len() - 1]);
        let entries = entries(&landed).unwrap();
        assert_eq!(custom_type(entries.last().unwrap()), Some(SESSION_MOVED_IN));
        assert_eq!(session_ownership(&entries), Ownership::Writable);
        assert!(moved_state_dir(&landed, "m1")
            .join("omo-nyang/manifest.json")
            .is_file());
        assert!(!sessions.join(".move-staging/m1").exists());
        // The landed copy answers accepted, and the same bundle never lands twice.
        assert_eq!(
            answer_move_status(&ledger, "m1", Some(&landed)).unwrap(),
            MoveStatus::Accepted
        );
        assert_eq!(
            import(&bundle, Path::new("/laptop/repo"), &sessions, &ledger).unwrap(),
            landed
        );
        assert_eq!(fs::read_to_string(&landed).unwrap(), moved);
        let elsewhere = dir.path().join("c/x.jsonl");
        assert_eq!(
            answer_move_status(&ledger, "m1", Some(&landed.with_file_name("missing.jsonl")))
                .unwrap(),
            MoveStatus::Accepted
        );
        fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        fs::write(&elsewhere, format!("{}\n", header_line("s9", "/x"))).unwrap();
        assert_eq!(
            answer_move_status(&ledger, "m1", Some(&elsewhere))
                .unwrap_err()
                .code,
            MoveError::MISMATCH
        );
    }

    #[test]
    fn an_unprepared_or_foreign_move_is_refused_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        seal(&file, "m1", None);
        let bundle = dir.path().join("bundle");
        export(&file, "m1", &bundle).unwrap();
        let ledger = MoveLedger::new(dir.path().join("ledger"));
        let sessions = dir.path().join("b");
        let error = import(&bundle, Path::new("/repo"), &sessions, &ledger).unwrap_err();
        assert_eq!(error.code, MoveError::FENCED);
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Declined(DeclineReason::ChunkWithoutReady)
        );
        // Another repository is declined before anything lands.
        let ledger = MoveLedger::new(dir.path().join("ledger2"));
        ledger.prepare("m1").unwrap();
        let error = import_session(&ImportOptions {
            bundle_dir: &bundle,
            target_cwd: Path::new("/other"),
            session_dir: &sessions,
            from: "desk",
            ledger: &ledger,
            target_repository: None,
        })
        .unwrap_err();
        assert_eq!(error.code, MoveError::REPOSITORY_MISMATCH);
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Declined(DeclineReason::Repository)
        );
        assert!(
            !sessions.exists()
                || fs::read_dir(&sessions)
                    .unwrap()
                    .all(|e| e.unwrap().file_name() != "2026-11-23T00-00-00-000Z_s1.jsonl")
        );
    }

    #[test]
    fn a_landing_file_that_cannot_be_read_never_settles_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MoveLedger::new(dir.path().join("ledger"));
        ledger.prepare("m1").unwrap();
        let file = session(&dir.path().join("sessions"), "s1", "/repo");
        append_custom(
            &file,
            SESSION_MOVED_IN,
            json!({"move_id": "m1", "session_id": "s1"}),
        )
        .unwrap();
        // An import stopped mid-commit: the answer comes from the landed file, and a file that
        // cannot be read right now is an error, not a failed move the sender would reclaim.
        fs::write(
            dir.path().join("ledger/m1.json"),
            json!({"state": "receiving", "target": file.to_string_lossy(), "committing": true})
                .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        }
        assert!(answer_move_status(&ledger, "m1", None).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Accepted
        );
    }

    #[test]
    fn a_writable_session_at_the_target_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        seal(&file, "m1", None);
        let bundle = dir.path().join("bundle");
        export(&file, "m1", &bundle).unwrap();
        let sessions = dir.path().join("b");
        let occupant = session(&sessions, "s1", "/repo");
        let before = fs::read(&occupant).unwrap();
        let ledger = MoveLedger::new(dir.path().join("ledger"));
        ledger.prepare("m1").unwrap();
        let error = import(&bundle, Path::new("/repo"), &sessions, &ledger).unwrap_err();
        assert_eq!(error.code, MoveError::TARGET_OCCUPIED);
        assert_eq!(fs::read(&occupant).unwrap(), before);
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Failed(FailReason::ImportFailed)
        );
    }

    #[test]
    fn ledger_answers_follow_the_ready_window_and_never_change_once_heard() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = now.clone();
        let ledger = MoveLedger::with_clock(dir.path(), move || clock.load(Ordering::SeqCst));
        assert_eq!(
            answer_move_status(&ledger, "never", None).unwrap(),
            MoveStatus::Declined(DeclineReason::NoRecord)
        );
        assert_eq!(ledger.prepare("never").unwrap_err().code, MoveError::FENCED);
        let ready = ledger.prepare("m1").unwrap();
        assert_eq!(
            ready,
            MoveRecord::Ready {
                expires_at_ms: 1_000 + READY_TTL_MS
            }
        );
        assert_eq!(ready.to_wire()["expires_at"], "1970-01-01T00:10:01.000Z");
        now.store(5_000, Ordering::SeqCst);
        assert_eq!(
            ledger.prepare("m1").unwrap(),
            ready,
            "a second prepare keeps the first window"
        );
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Receiving
        );
        now.store(1_000 + READY_TTL_MS, Ordering::SeqCst);
        assert_eq!(
            answer_move_status(&ledger, "m1", None).unwrap(),
            MoveStatus::Declined(DeclineReason::Expired)
        );
        assert_eq!(
            ledger.decline("m1", DeclineReason::Cancelled).unwrap(),
            MoveRecord::Status(MoveStatus::Declined(DeclineReason::Expired))
        );
        ledger.prepare("m2").unwrap();
        assert_eq!(
            ledger
                .decline("m2", DeclineReason::Cancelled)
                .unwrap()
                .to_wire(),
            json!({"state": "declined", "reason": "cancelled"})
        );
        assert_eq!(ledger.prepare("m2").unwrap_err().code, MoveError::FENCED);
        assert_eq!(
            ledger.prepare("../x").unwrap_err().code,
            MoveError::INVALID_REQUEST
        );
    }

    #[test]
    fn finish_records_the_answer_and_clears_the_move_files() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        // Accepted: the sender's copy is marked moved and its bundle and staged state go.
        let file = session(&dir.path().join("a"), "s1", "/repo");
        let staged = prepare_outgoing_state(&file, "m1").unwrap();
        seal(&file, "m1", Some(&staged.attempt));
        export(&file, "m1", &bundle).unwrap();
        assert_eq!(
            finish_move(&file, &bundle, "m1", &MoveStatus::Receiving)
                .unwrap()
                .wire(),
            "moving"
        );
        assert!(bundle.exists());
        assert_eq!(
            finish_move(&file, &bundle, "m2", &MoveStatus::Accepted)
                .unwrap_err()
                .code,
            MoveError::MISMATCH
        );
        assert_eq!(
            finish_move(&file, &bundle, "m1", &MoveStatus::Accepted)
                .unwrap()
                .wire(),
            "moved"
        );
        assert!(!bundle.exists());
        assert!(!outgoing_state_dir(&file, "m1").exists());
        assert_eq!(
            finish_move(&file, &bundle, "m1", &MoveStatus::Accepted)
                .unwrap()
                .wire(),
            "moved"
        );
        assert_eq!(
            finish_move(&file, &bundle, "m1", &MoveStatus::AcceptedRevoked)
                .unwrap_err()
                .code,
            MoveError::ALREADY_MOVED
        );
        // Declined: the session is taken back and becomes writable again.
        let file = session(&dir.path().join("b"), "s2", "/repo");
        seal(&file, "m3", None);
        let declined = MoveStatus::Declined(DeclineReason::Cancelled);
        assert_eq!(
            finish_move(&file, &bundle, "m3", &declined).unwrap().wire(),
            "writable"
        );
        let last = entries(&file).unwrap().pop().unwrap();
        assert_eq!(custom_type(&last), Some(SESSION_RECLAIMED));
        assert_eq!(last["data"]["detail"], "cancelled");
        assert_eq!(MoveStatus::parse(&declined.to_wire()), Some(declined));
        assert_eq!(MoveStatus::parse(&json!({"state": "ready"})), None);
    }

    #[test]
    fn moved_state_is_redelivered_until_it_is_marked_restored() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        assert!(begin_moved_state_delivery(&file).unwrap().is_none());
        append_custom(
            &file,
            SESSION_MOVED_IN,
            json!({"move_id": "m1", "session_id": "s1"}),
        )
        .unwrap();
        let pending = moved_state_dir(&file, "m1");
        fs::create_dir_all(pending.join("omo-nyang")).unwrap();
        fs::write(pending.join("omo-nyang/manifest.json"), "{}").unwrap();
        let first = begin_moved_state_delivery(&file).unwrap().unwrap();
        assert!(!first.redelivered);
        assert!(first.dir.join("omo-nyang/manifest.json").is_file());
        first.finish(false).unwrap();
        let second = begin_moved_state_delivery(&file).unwrap().unwrap();
        assert!(second.redelivered);
        append_custom(&file, MOVED_STATE_RESTORED, json!({"move_id": "m1"})).unwrap();
        second.finish(true).unwrap();
        assert!(!pending.exists());
        assert!(begin_moved_state_delivery(&file).unwrap().is_none());
    }

    #[test]
    fn a_new_release_attempt_drops_unsealed_ones_and_keeps_the_seal() {
        let dir = tempfile::tempdir().unwrap();
        let file = session(&dir.path().join("a"), "s1", "/repo");
        let dead = prepare_outgoing_state(&file, "m1").unwrap();
        let sealed = prepare_outgoing_state(&file, "m1").unwrap();
        assert!(!dead.dir.exists());
        seal(&file, "m1", Some(&sealed.attempt));
        let retry = prepare_outgoing_state(&file, "m1").unwrap();
        assert!(sealed.dir.exists() && retry.dir.exists());
        assert_ne!(sealed.attempt, retry.attempt);
        assert_eq!(
            prepare_outgoing_state(&file, "../m").unwrap_err().code,
            MoveError::INVALID_REQUEST
        );
    }

    #[test]
    fn exported_bundle_puts_session_header_on_first_line_even_with_title_slot() {
        let dir = tempfile::tempdir().unwrap();
        let titled = session(&dir.path().join("titled"), "s1", "/repo");
        seal(&titled, "m1", None);
        let bundle = dir.path().join("bundle-titled");
        let manifest = export(&titled, "m1", &bundle).unwrap();
        let exported_text = fs::read_to_string(bundle.join(SESSION_BUNDLE_PATH)).unwrap();
        let first_line = exported_text.lines().next().expect("header line");
        let first: Value = serde_json::from_str(first_line).expect("json first line");
        assert_eq!(first["type"], "session", "line 1 was {first_line}");
        assert_eq!(first["id"], manifest.session_id);
        let original = fs::read_to_string(&titled).unwrap();
        let (_, expected_body) = original.split_once('\n').expect("title slot newline");
        assert_eq!(exported_text, expected_body);
        verify_bundle(&bundle, &manifest).unwrap();

        // A session without a leading title slot keeps an exact byte-for-byte copy.
        let untitled_dir = dir.path().join("untitled");
        let untitled = untitled_dir.join("2026-11-23T00-00-00-000Z_s2.jsonl");
        fs::create_dir_all(&untitled_dir).unwrap();
        fs::write(&untitled, format!("{}\n", header_line("s2", "/repo"))).unwrap();
        seal(&untitled, "m2", None);
        let bundle_untitled = dir.path().join("bundle-untitled");
        let manifest_untitled = export(&untitled, "m2", &bundle_untitled).unwrap();
        assert_eq!(
            fs::read(bundle_untitled.join(SESSION_BUNDLE_PATH)).unwrap(),
            fs::read(&untitled).unwrap()
        );
        verify_bundle(&bundle_untitled, &manifest_untitled).unwrap();
    }
}
