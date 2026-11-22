//! VCS / worktree isolation PAL.
//!
//! - Read-only repository queries ([`discover`], [`head`], [`rev_parse`]) read
//!   `.git` directly (HEAD, loose refs, `packed-refs`, linked-worktree
//!   `commondir`) so they never spawn a process; reftable repositories fall
//!   back to `git rev-parse`. [`status`] needs the index and the work tree, so
//!   it runs `git status --porcelain` with bounded output.
//! - Managed worktrees live at `<base_dir>/<name>`, each with an owner record
//!   `<base_dir>/<name>.omt-owner.json` holding the owning PID (and on Linux
//!   its start time, so a recycled PID is not mistaken for the owner).
//!   [`gc_orphans`] removes worktrees whose owner is gone and never touches a
//!   live owner, unreadable records, or anything outside `base_dir`.
//! - [`keep_changes`] carries a worktree's committed and uncommitted changes
//!   (untracked files included) back to another checkout as a binary patch,
//!   checked before it is applied so a conflict leaves the target untouched.
//!
//! Every git invocation is non-interactive, has null stdin unless it is fed a
//! patch, and caps captured output at [`MAX_GIT_OUTPUT`].

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Upper bound on captured git stdout (patches, status listings).
pub const MAX_GIT_OUTPUT: usize = 32 * 1024 * 1024;
/// Upper bound on captured git stderr.
const MAX_GIT_STDERR: u64 = 64 * 1024;
/// Owner-record file suffix next to each managed worktree.
const OWNER_SUFFIX: &str = ".omt-owner.json";
/// Symbolic-ref hops followed before giving up (git itself stops at 5).
const MAX_SYMREF_DEPTH: usize = 5;

#[derive(Debug)]
pub enum VcsError {
    Io {
        context: String,
        source: io::Error,
    },
    NotARepo(PathBuf),
    Git {
        args: String,
        stderr: String,
    },
    OutputTooLarge {
        args: String,
    },
    InvalidName(String),
    AlreadyExists(PathBuf),
    OutsideBase(PathBuf),
    UnresolvedRev(String),
    /// `git apply --check` rejected the patch; the target was not modified.
    Conflict(String),
}

impl fmt::Display for VcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::NotARepo(p) => write!(f, "not inside a git repository: {}", p.display()),
            Self::Git { args, stderr } => write!(f, "git {args} failed: {stderr}"),
            Self::OutputTooLarge { args } => {
                write!(f, "git {args}: output exceeds {MAX_GIT_OUTPUT} bytes")
            }
            Self::InvalidName(n) => write!(f, "invalid worktree name: {n:?}"),
            Self::AlreadyExists(p) => write!(f, "worktree path already exists: {}", p.display()),
            Self::OutsideBase(p) => {
                write!(
                    f,
                    "refusing to touch path outside worktree base: {}",
                    p.display()
                )
            }
            Self::UnresolvedRev(r) => write!(f, "cannot resolve revision: {r}"),
            Self::Conflict(e) => write!(f, "changes do not apply cleanly: {e}"),
        }
    }
}

impl std::error::Error for VcsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> VcsError {
    let context = context.into();
    move |source| VcsError::Io { context, source }
}

pub type Result<T> = std::result::Result<T, VcsError>;

// ---------------------------------------------------------------------------
// Bounded git runner
// ---------------------------------------------------------------------------

/// Run `git -C cwd args...`, feeding `input` on stdin, and return stdout.
fn run_git(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let joined = args.join(" ");
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().map_err(io_err("failed to run git"))?;

    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let data = bytes.to_vec();
            // A write error means git exited early; its exit status reports why.
            Some(std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            }))
        }
        _ => None,
    };
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut pipe).take(MAX_GIT_STDERR).read_to_end(&mut buf);
            let _ = io::copy(&mut pipe, &mut io::sink());
            buf
        })
    });

    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_GIT_OUTPUT as u64 + 1)
            .read_to_end(&mut out)
            .map_err(io_err(format!("reading git {joined} output")))?;
    }
    let overflow = out.len() > MAX_GIT_OUTPUT;
    if overflow {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(io_err(format!("waiting for git {joined}")))?;
    if let Some(handle) = writer {
        let _ = handle.join();
    }
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if overflow {
        return Err(VcsError::OutputTooLarge { args: joined });
    }
    if !status.success() {
        return Err(VcsError::Git {
            args: joined,
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Read-only repository queries
// ---------------------------------------------------------------------------

/// Locations of a (possibly linked) work tree's repository metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPaths {
    /// Top of the work tree.
    pub work_tree: PathBuf,
    /// Per-worktree git dir (`.git`, or `.git/worktrees/<id>` for linked ones).
    pub git_dir: PathBuf,
    /// Shared git dir holding objects and branch refs.
    pub common_dir: PathBuf,
}

/// Find the repository containing `start` by walking up to the nearest
/// `.git` directory or `.git` file (`gitdir:` pointer). Bare repositories and
/// `GIT_DIR` overrides are not considered.
///
/// # Errors
/// [`VcsError::NotARepo`] when no ancestor holds a usable `.git`.
pub fn discover(start: &Path) -> Result<RepoPaths> {
    let start =
        fs::canonicalize(start).map_err(io_err(format!("resolving {}", start.display())))?;
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let text = fs::read_to_string(&dot_git)
                .map_err(io_err(format!("reading {}", dot_git.display())))?;
            let Some(target) = text.trim_end().strip_prefix("gitdir: ") else {
                continue;
            };
            dir.join(target)
        } else {
            continue;
        };
        if !git_dir.join("HEAD").is_file() {
            continue;
        }
        let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
            Ok(rel) => git_dir.join(rel.trim_end()),
            Err(_) => git_dir.clone(),
        };
        let common_dir = fs::canonicalize(&common_dir).unwrap_or(common_dir);
        let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
        return Ok(RepoPaths {
            work_tree: dir.to_path_buf(),
            git_dir,
            common_dir,
        });
    }
    Err(VcsError::NotARepo(start))
}

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// Symbolic `HEAD` on a branch (short name; may be unborn).
    Branch(String),
    /// Detached at an object id.
    Detached(String),
}

/// Read the work tree's `HEAD`.
///
/// # Errors
/// I/O failure reading `HEAD`.
pub fn head(repo: &RepoPaths) -> Result<Head> {
    let path = repo.git_dir.join("HEAD");
    let text = fs::read_to_string(&path).map_err(io_err(format!("reading {}", path.display())))?;
    let text = text.trim_end();
    Ok(match text.strip_prefix("ref: ") {
        Some(target) => Head::Branch(
            target
                .strip_prefix("refs/heads/")
                .unwrap_or(target)
                .to_string(),
        ),
        None => Head::Detached(text.to_string()),
    })
}

/// Current branch short name, or `None` when `HEAD` is detached.
///
/// # Errors
/// See [`head`].
pub fn current_branch(repo: &RepoPaths) -> Result<Option<String>> {
    Ok(match head(repo)? {
        Head::Branch(name) => Some(name),
        Head::Detached(_) => None,
    })
}

fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Refs stored per worktree rather than in the common dir.
fn is_per_worktree_ref(name: &str) -> bool {
    !name.contains('/')
        || ["refs/worktree/", "refs/bisect/", "refs/rewritten/"]
            .iter()
            .any(|p| name.starts_with(p))
}

fn packed_ref(common_dir: &Path, name: &str) -> Result<Option<String>> {
    let path = common_dir.join("packed-refs");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(format!("reading {}", path.display()))(e)),
    };
    Ok(text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
        .filter_map(|l| l.split_once(' '))
        .find(|(_, r)| *r == name)
        .map(|(oid, _)| oid.to_string()))
}

/// Resolve a full ref name (or `HEAD`) to an object id, following symrefs.
fn resolve_ref(repo: &RepoPaths, name: &str) -> Result<Option<String>> {
    let mut name = name.to_string();
    for _ in 0..MAX_SYMREF_DEPTH {
        let dir = if is_per_worktree_ref(&name) {
            &repo.git_dir
        } else {
            &repo.common_dir
        };
        let loose = dir.join(&name);
        let content = match fs::read_to_string(&loose) {
            Ok(t) => t,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return packed_ref(&repo.common_dir, &name);
            }
            // A directory such as `refs/heads/feature/` is not a ref.
            Err(_) if loose.is_dir() => return packed_ref(&repo.common_dir, &name),
            Err(e) => return Err(io_err(format!("reading {}", loose.display()))(e)),
        };
        let content = content.trim_end();
        match content.strip_prefix("ref: ") {
            Some(target) => name = target.to_string(),
            None if is_oid(content) => return Ok(Some(content.to_ascii_lowercase())),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Resolve `rev` (full object id, `HEAD`, full ref, or short branch/tag/remote
/// name, in git's lookup order) to an object id without spawning git. Returns
/// `None` when nothing matches. Annotated tags resolve to the tag object, as
/// `git rev-parse <tag>` does.
///
/// # Errors
/// I/O failure reading refs, or a failing `git rev-parse` fallback for
/// reftable repositories.
pub fn rev_parse(repo: &RepoPaths, rev: &str) -> Result<Option<String>> {
    if is_oid(rev) {
        return Ok(Some(rev.to_ascii_lowercase()));
    }
    if rev.is_empty() || rev.starts_with('-') || rev.contains("..") {
        return Ok(None);
    }
    if repo.common_dir.join("reftable").is_dir() {
        return match run_git(
            &repo.work_tree,
            &["rev-parse", "--verify", "--quiet", "--end-of-options", rev],
            &[],
            None,
        ) {
            Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_string())),
            Err(VcsError::Git { .. }) => Ok(None),
            Err(e) => Err(e),
        };
    }
    let candidates = [
        rev.to_string(),
        format!("refs/{rev}"),
        format!("refs/tags/{rev}"),
        format!("refs/heads/{rev}"),
        format!("refs/remotes/{rev}"),
        format!("refs/remotes/{rev}/HEAD"),
    ];
    for name in &candidates {
        if name == "HEAD" || name.starts_with("refs/") {
            if let Some(oid) = resolve_ref(repo, name)? {
                return Ok(Some(oid));
            }
        }
    }
    Ok(None)
}

/// One `git status --porcelain=v1` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Index (staged) status column, e.g. `M`, `A`, `R`, `?`.
    pub index: char,
    /// Work-tree status column.
    pub worktree: char,
    pub path: String,
    /// Source path of a rename or copy.
    pub orig_path: Option<String>,
}

fn parse_porcelain_z(raw: &[u8]) -> Vec<StatusEntry> {
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let (index, worktree) = (char::from(field[0]), char::from(field[1]));
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        let orig_path = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned())
        } else {
            None
        };
        entries.push(StatusEntry {
            index,
            worktree,
            path,
            orig_path,
        });
    }
    entries
}

/// Work-tree status (untracked files listed individually). Takes no
/// optional locks, so it never rewrites the index.
///
/// # Errors
/// git failure or output above [`MAX_GIT_OUTPUT`].
pub fn status(work_tree: &Path) -> Result<Vec<StatusEntry>> {
    let out = run_git(
        work_tree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &[],
        None,
    )?;
    Ok(parse_porcelain_z(&out))
}

// ---------------------------------------------------------------------------
// Managed worktrees
// ---------------------------------------------------------------------------

/// Owner record persisted next to a managed worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub version: u32,
    /// Owning process.
    pub pid: u32,
    /// Owner start time (Linux `/proc/<pid>/stat` field 22) to detect PID reuse.
    pub start_time: Option<u64>,
    /// Work tree of the repository the worktree belongs to.
    pub repo: PathBuf,
    /// Worktree directory (`<base_dir>/<name>`).
    pub path: PathBuf,
    /// Commit the worktree was created at.
    pub base: String,
}

/// A worktree created by [`worktree_add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub name: String,
    pub path: PathBuf,
    pub record_path: PathBuf,
    pub owner: OwnerRecord,
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && !name.ends_with(".lock")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(VcsError::InvalidName(name.to_string()))
    }
}

fn record_path(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join(format!("{name}{OWNER_SUFFIX}"))
}

fn write_record(path: &Path, record: &OwnerRecord) -> Result<()> {
    let body =
        serde_json::to_vec_pretty(record).map_err(|e| io_err("encoding owner record")(e.into()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body).map_err(io_err(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_err(format!("renaming {}", tmp.display())))
}

/// Start time of `pid` in clock ticks since boot (Linux only).
fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `comm` (field 2) may contain spaces and parens; fields resume after the last ')'.
        let rest = &stat[stat.rfind(')')? + 1..];
        // Field 3 (`state`) is index 0 here, so field 22 (`starttime`) is index 19.
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pid` is still the process that wrote a record with `start_time`.
/// Errs toward "alive": only a definite "no such process" or a start-time
/// mismatch (the PID was recycled) reports dead.
#[must_use]
pub fn pid_alive(pid: u32, start_time: Option<u64>) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        // PID 0 / negative would address process groups, never a single owner.
        let Ok(raw) = i32::try_from(pid) else {
            return true;
        };
        if raw <= 0 {
            return true;
        }
        if let Err(Errno::ESRCH) = kill(Pid::from_raw(raw), None) {
            return false;
        }
        match (start_time, process_start_time(pid)) {
            (Some(recorded), Some(current)) => recorded == current,
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, start_time);
        true
    }
}

/// Create a detached worktree at `<base_dir>/<name>` from `rev` in `repo`,
/// owned by `owner_pid`. The owner record is written before the worktree so a
/// crash in between still leaves GC something to collect.
///
/// # Errors
/// Invalid name, existing path, unresolvable `rev`, I/O or git failure.
pub fn worktree_add(
    repo: &RepoPaths,
    base_dir: &Path,
    name: &str,
    rev: &str,
    owner_pid: u32,
) -> Result<ManagedWorktree> {
    validate_name(name)?;
    fs::create_dir_all(base_dir).map_err(io_err(format!("creating {}", base_dir.display())))?;
    let base_dir =
        fs::canonicalize(base_dir).map_err(io_err(format!("resolving {}", base_dir.display())))?;
    let path = base_dir.join(name);
    let record_path = record_path(&base_dir, name);
    if path.symlink_metadata().is_ok() || record_path.symlink_metadata().is_ok() {
        return Err(VcsError::AlreadyExists(path));
    }
    let base = rev_parse(repo, rev)?.ok_or_else(|| VcsError::UnresolvedRev(rev.to_string()))?;
    let owner = OwnerRecord {
        version: 1,
        pid: owner_pid,
        start_time: process_start_time(owner_pid),
        repo: repo.work_tree.clone(),
        path: path.clone(),
        base: base.clone(),
    };
    write_record(&record_path, &owner)?;
    let path_str = path.to_string_lossy().into_owned();
    if let Err(e) = run_git(
        &repo.work_tree,
        &["worktree", "add", "--detach", "--quiet", &path_str, &base],
        &[],
        None,
    ) {
        let _ = fs::remove_file(&record_path);
        return Err(e);
    }
    Ok(ManagedWorktree {
        name: name.to_string(),
        path,
        record_path,
        owner,
    })
}

/// Remove a managed worktree: unregister it from git, delete its directory
/// (only when it is a real directory directly under `base_dir`), prune stale
/// registrations, and drop the owner record.
fn remove_managed(base_dir: &Path, name: &str, owner: &OwnerRecord) -> Result<()> {
    validate_name(name)?;
    let path = base_dir.join(name);
    if owner.path != path {
        return Err(VcsError::OutsideBase(owner.path.clone()));
    }
    let repo_present = owner.repo.join(".git").exists();
    let path_str = path.to_string_lossy().into_owned();
    if repo_present && path.is_dir() {
        // Falls through to the filesystem removal below when git refuses
        // (e.g. the directory is no longer a registered worktree).
        let _ = run_git(
            &owner.repo,
            &["worktree", "remove", "--force", &path_str],
            &[],
            None,
        );
    }
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => return Err(VcsError::OutsideBase(path)),
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(&path).map_err(io_err(format!("removing {}", path.display())))?;
        }
        Ok(_) => return Err(VcsError::OutsideBase(path)),
        Err(_) => {}
    }
    if repo_present {
        run_git(&owner.repo, &["worktree", "prune"], &[], None)?;
    }
    let record = record_path(base_dir, name);
    match fs::remove_file(&record) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(format!("removing {}", record.display()))(e)),
    }
}

/// Remove a worktree created by [`worktree_add`].
///
/// # Errors
/// I/O or git failure, or a record that points outside its base directory.
pub fn worktree_remove(wt: &ManagedWorktree) -> Result<()> {
    let base_dir = wt
        .path
        .parent()
        .ok_or_else(|| VcsError::OutsideBase(wt.path.clone()))?;
    remove_managed(base_dir, &wt.name, &wt.owner)
}

/// Outcome of [`gc_orphans`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Worktree names removed because their owner is gone.
    pub removed: Vec<String>,
    /// Worktree names kept because their owner is alive.
    pub kept: Vec<String>,
    /// Records left alone (unreadable, mismatched path, or removal failed), with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Remove managed worktrees under `base_dir` whose owner process is gone.
///
/// # Errors
/// Failure listing `base_dir`; per-worktree problems land in
/// [`GcReport::skipped`].
pub fn gc_orphans(base_dir: &Path) -> Result<GcReport> {
    gc_orphans_with(base_dir, |rec| pid_alive(rec.pid, rec.start_time))
}

/// [`gc_orphans`] with an injectable liveness check. The current process
/// always counts as alive.
///
/// # Errors
/// See [`gc_orphans`].
pub fn gc_orphans_with(
    base_dir: &Path,
    is_alive: impl Fn(&OwnerRecord) -> bool,
) -> Result<GcReport> {
    let mut report = GcReport::default();
    let base_dir = match fs::canonicalize(base_dir) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(io_err(format!("resolving {}", base_dir.display()))(e)),
    };
    let entries =
        fs::read_dir(&base_dir).map_err(io_err(format!("listing {}", base_dir.display())))?;
    let mut records: Vec<(String, PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            let name = file_name.strip_suffix(OWNER_SUFFIX)?.to_string();
            Some((name, entry.path()))
        })
        .collect();
    records.sort();
    let me = std::process::id();
    for (name, path) in records {
        let owner = match fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<OwnerRecord>(&b).map_err(|e| e.to_string()))
        {
            Ok(owner) => owner,
            Err(e) => {
                report.skipped.push((name, e));
                continue;
            }
        };
        if validate_name(&name).is_err() || owner.path != base_dir.join(&name) {
            report
                .skipped
                .push((name, "record path does not match its location".to_string()));
            continue;
        }
        if owner.pid == me || is_alive(&owner) {
            report.kept.push(name);
            continue;
        }
        match remove_managed(&base_dir, &name, &owner) {
            Ok(()) => report.removed.push(name),
            Err(e) => report.skipped.push((name, e.to_string())),
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Merge-back
// ---------------------------------------------------------------------------

/// Binary patch from `base` to the worktree's current content: commits made
/// in the worktree, staged and unstaged edits, and untracked (non-ignored)
/// files. Uses a throwaway index, so the worktree's own index is untouched.
///
/// # Errors
/// I/O or git failure.
pub fn worktree_patch(worktree: &Path, base: &str) -> Result<Vec<u8>> {
    let repo = discover(worktree)?;
    let index = repo
        .git_dir
        .join(format!("omt-keep-index-{}", std::process::id()));
    let env = [("GIT_INDEX_FILE", index.as_path())];
    let result = run_git(&repo.work_tree, &["read-tree", "HEAD"], &env, None)
        .and_then(|_| run_git(&repo.work_tree, &["add", "--all"], &env, None))
        .and_then(|_| {
            run_git(
                &repo.work_tree,
                &[
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-renames",
                    base,
                    "--",
                ],
                &env,
                None,
            )
        });
    let _ = fs::remove_file(&index);
    result
}

/// Result of [`keep_changes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepOutcome {
    NoChanges,
    Applied { files: usize },
}

/// Carry the worktree's changes since `base` into the `target` work tree.
/// The patch is checked first; on conflict `target` is left untouched.
///
/// # Errors
/// [`VcsError::Conflict`] when the patch does not apply; otherwise I/O or git
/// failure.
pub fn keep_changes(worktree: &Path, base: &str, target: &Path) -> Result<KeepOutcome> {
    let patch = worktree_patch(worktree, base)?;
    if patch.is_empty() {
        return Ok(KeepOutcome::NoChanges);
    }
    match run_git(
        target,
        &["apply", "--binary", "--check", "-"],
        &[],
        Some(&patch),
    ) {
        Ok(_) => {}
        Err(VcsError::Git { stderr, .. }) => return Err(VcsError::Conflict(stderr)),
        Err(e) => return Err(e),
    }
    run_git(target, &["apply", "--binary", "-"], &[], Some(&patch))?;
    let files = patch
        .split(|b| *b == b'\n')
        .filter(|l| l.starts_with(b"diff --git "))
        .count();
    Ok(KeepOutcome::Applied { files })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run git for test setup with user/system config isolated.
    fn sh(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Fresh repo on `main` with one commit containing `a.txt`.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canon").join("repo");
        fs::create_dir(&root).expect("mkdir");
        sh(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "one\n").expect("write");
        sh(&root, &["add", "a.txt"]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        (dir, root)
    }

    fn base_dir(tmp: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(tmp.path()).expect("canon").join("wt")
    }

    #[test]
    fn discover_head_and_rev_parse_match_git() {
        let (_tmp, root) = init_repo();
        fs::create_dir(root.join("sub")).expect("mkdir");
        let repo = discover(&root.join("sub")).expect("discover");
        assert_eq!(repo.work_tree, root);
        assert_eq!(repo.git_dir, root.join(".git"));
        assert_eq!(repo.common_dir, root.join(".git"));
        assert_eq!(head(&repo).expect("head"), Head::Branch("main".into()));
        assert_eq!(
            current_branch(&repo).expect("branch").as_deref(),
            Some("main")
        );
        let oid = sh(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            rev_parse(&repo, "HEAD").expect("rev").as_deref(),
            Some(oid.as_str())
        );
        assert_eq!(
            rev_parse(&repo, "main").expect("rev").as_deref(),
            Some(oid.as_str())
        );
        assert_eq!(rev_parse(&repo, "no-such-branch").expect("rev"), None);
    }

    #[test]
    fn discover_outside_repo_is_not_a_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // The temp dir itself may sit inside a repo on some hosts; only assert
        // when it does not.
        if let Err(e) = discover(tmp.path()) {
            assert!(matches!(e, VcsError::NotARepo(_)), "{e}");
        }
    }

    #[test]
    fn rev_parse_reads_packed_refs_and_tags() {
        let (_tmp, root) = init_repo();
        sh(&root, &["branch", "feature/x"]);
        sh(&root, &["tag", "-a", "-m", "v", "v1"]);
        sh(&root, &["pack-refs", "--all"]);
        let repo = discover(&root).expect("discover");
        let commit = sh(&root, &["rev-parse", "HEAD"]);
        let tag_obj = sh(&root, &["rev-parse", "v1"]);
        assert!(!root.join(".git/refs/heads/feature/x").exists());
        assert_eq!(
            rev_parse(&repo, "feature/x").expect("rev").as_deref(),
            Some(commit.as_str())
        );
        assert_eq!(
            rev_parse(&repo, "refs/heads/feature/x")
                .expect("rev")
                .as_deref(),
            Some(commit.as_str())
        );
        assert_eq!(
            rev_parse(&repo, "v1").expect("rev").as_deref(),
            Some(tag_obj.as_str())
        );
    }

    #[test]
    fn detached_head_reports_oid() {
        let (_tmp, root) = init_repo();
        sh(&root, &["checkout", "-q", "--detach"]);
        let repo = discover(&root).expect("discover");
        let oid = sh(&root, &["rev-parse", "HEAD"]);
        assert_eq!(head(&repo).expect("head"), Head::Detached(oid.clone()));
        assert_eq!(current_branch(&repo).expect("branch"), None);
        assert_eq!(rev_parse(&repo, "HEAD").expect("rev"), Some(oid));
    }

    #[test]
    fn status_lists_modified_untracked_and_renamed() {
        let (_tmp, root) = init_repo();
        fs::write(root.join("a.txt"), "two\n").expect("write");
        fs::create_dir(root.join("d")).expect("mkdir");
        fs::write(root.join("d/new.txt"), "n\n").expect("write");
        fs::write(root.join("b.txt"), "b\n").expect("write");
        sh(&root, &["add", "b.txt"]);
        sh(&root, &["commit", "-q", "-m", "b"]);
        sh(&root, &["mv", "b.txt", "c.txt"]);
        let mut entries = status(&root).expect("status");
        entries.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(
            entries,
            vec![
                StatusEntry {
                    index: ' ',
                    worktree: 'M',
                    path: "a.txt".into(),
                    orig_path: None
                },
                StatusEntry {
                    index: 'R',
                    worktree: ' ',
                    path: "c.txt".into(),
                    orig_path: Some("b.txt".into())
                },
                StatusEntry {
                    index: '?',
                    worktree: '?',
                    path: "d/new.txt".into(),
                    orig_path: None
                },
            ]
        );
    }

    #[test]
    fn worktree_add_creates_linked_checkout_and_owner_record() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let base = base_dir(&tmp);
        let wt = worktree_add(&repo, &base, "lane-1", "HEAD", 4242).expect("add");
        assert_eq!(wt.path, base.join("lane-1"));
        assert_eq!(
            fs::read_to_string(wt.path.join("a.txt")).expect("read"),
            "one\n"
        );
        let stored: OwnerRecord =
            serde_json::from_slice(&fs::read(&wt.record_path).expect("record")).expect("json");
        assert_eq!(stored, wt.owner);
        assert_eq!(stored.pid, 4242);
        assert_eq!(stored.base, sh(&root, &["rev-parse", "HEAD"]));

        let linked = discover(&wt.path).expect("discover linked");
        assert_eq!(linked.work_tree, wt.path);
        assert_eq!(linked.common_dir, root.join(".git"));
        assert_ne!(linked.git_dir, linked.common_dir);
        assert_eq!(
            head(&linked).expect("head"),
            Head::Detached(stored.base.clone())
        );
        assert_eq!(rev_parse(&linked, "main").expect("rev"), Some(stored.base));
        assert!(status(&wt.path).expect("status").is_empty());
    }

    #[test]
    fn worktree_add_rejects_bad_names_existing_paths_and_unknown_revs() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let base = base_dir(&tmp);
        for bad in ["", "../x", "a/b", ".hidden", "-opt", "x.lock"] {
            assert!(
                matches!(
                    worktree_add(&repo, &base, bad, "HEAD", 1),
                    Err(VcsError::InvalidName(_))
                ),
                "{bad}"
            );
        }
        worktree_add(&repo, &base, "dup", "HEAD", 1).expect("add");
        assert!(matches!(
            worktree_add(&repo, &base, "dup", "HEAD", 1),
            Err(VcsError::AlreadyExists(_))
        ));
        assert!(matches!(
            worktree_add(&repo, &base, "other", "nope", 1),
            Err(VcsError::UnresolvedRev(_))
        ));
        assert!(!record_path(&base, "other").exists());
    }

    #[test]
    fn worktree_remove_unregisters_and_deletes() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let wt = worktree_add(&repo, &base_dir(&tmp), "gone", "HEAD", 1).expect("add");
        fs::write(wt.path.join("dirty.txt"), "x").expect("write");
        worktree_remove(&wt).expect("remove");
        assert!(!wt.path.exists());
        assert!(!wt.record_path.exists());
        let list = sh(&root, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains("gone"), "{list}");
    }

    #[test]
    fn keep_changes_carries_commits_edits_and_untracked_files() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let wt = worktree_add(&repo, &base_dir(&tmp), "keep", "HEAD", 1).expect("add");
        fs::write(wt.path.join("committed.txt"), "c\n").expect("write");
        sh(&wt.path, &["add", "committed.txt"]);
        sh(&wt.path, &["commit", "-q", "-m", "wt"]);
        fs::write(wt.path.join("a.txt"), "edited\n").expect("write");
        fs::write(wt.path.join("bin.dat"), [0_u8, 159, 146, 150, 0, 1]).expect("write");
        let wt_status_before = status(&wt.path).expect("status");

        let outcome = keep_changes(&wt.path, &wt.owner.base, &root).expect("keep");
        assert_eq!(outcome, KeepOutcome::Applied { files: 3 });
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).expect("read"),
            "edited\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("committed.txt")).expect("read"),
            "c\n"
        );
        assert_eq!(
            fs::read(root.join("bin.dat")).expect("read"),
            [0_u8, 159, 146, 150, 0, 1]
        );
        // The worktree's own index is untouched by the throwaway-index patch.
        assert_eq!(status(&wt.path).expect("status"), wt_status_before);
    }

    #[test]
    fn keep_changes_without_changes_is_noop() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let wt = worktree_add(&repo, &base_dir(&tmp), "idle", "HEAD", 1).expect("add");
        assert_eq!(
            keep_changes(&wt.path, &wt.owner.base, &root).expect("keep"),
            KeepOutcome::NoChanges
        );
        assert!(status(&root).expect("status").is_empty());
    }

    #[test]
    fn keep_changes_conflict_leaves_target_untouched() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let wt = worktree_add(&repo, &base_dir(&tmp), "clash", "HEAD", 1).expect("add");
        fs::write(wt.path.join("a.txt"), "from worktree\n").expect("write");
        fs::write(wt.path.join("extra.txt"), "e\n").expect("write");
        fs::write(root.join("a.txt"), "from main\n").expect("write");
        let err = keep_changes(&wt.path, &wt.owner.base, &root).expect_err("conflict");
        assert!(matches!(err, VcsError::Conflict(_)), "{err}");
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).expect("read"),
            "from main\n"
        );
        assert!(!root.join("extra.txt").exists());
    }

    #[test]
    fn gc_removes_dead_owners_and_keeps_live_ones() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let base = base_dir(&tmp);
        let dead = worktree_add(&repo, &base, "dead", "HEAD", 1001).expect("add");
        let live = worktree_add(&repo, &base, "live", "HEAD", 1002).expect("add");
        let mine = worktree_add(&repo, &base, "mine", "HEAD", std::process::id()).expect("add");

        let report = gc_orphans_with(&base, |rec| rec.pid == 1002).expect("gc");
        assert_eq!(report.removed, vec!["dead".to_string()]);
        assert_eq!(report.kept, vec!["live".to_string(), "mine".to_string()]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(!dead.path.exists() && !dead.record_path.exists());
        assert!(live.path.is_dir() && live.record_path.is_file());
        assert!(mine.path.is_dir() && mine.record_path.is_file());
        let list = sh(&root, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains("/dead") && list.contains("/live"), "{list}");
    }

    #[test]
    fn gc_collects_record_whose_worktree_dir_is_missing() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let base = base_dir(&tmp);
        let wt = worktree_add(&repo, &base, "half", "HEAD", 1001).expect("add");
        fs::remove_dir_all(&wt.path).expect("rm");
        let report = gc_orphans_with(&base, |_| false).expect("gc");
        assert_eq!(report.removed, vec!["half".to_string()]);
        assert!(!wt.record_path.exists());
        assert!(!sh(&root, &["worktree", "list", "--porcelain"]).contains("/half"));
    }

    #[test]
    fn gc_skips_corrupt_and_mismatched_records() {
        let (tmp, root) = init_repo();
        let repo = discover(&root).expect("discover");
        let base = base_dir(&tmp);
        let wt = worktree_add(&repo, &base, "real", "HEAD", 1001).expect("add");
        fs::write(record_path(&base, "broken"), "{not json").expect("write");
        // A record that claims another directory must not be acted on.
        let mut foreign = wt.owner.clone();
        foreign.path = root.clone();
        write_record(&record_path(&base, "foreign"), &foreign).expect("write");

        let report = gc_orphans_with(&base, |rec| rec.path == wt.path).expect("gc");
        assert_eq!(report.kept, vec!["real".to_string()]);
        assert!(report.removed.is_empty());
        let skipped: Vec<_> = report.skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(skipped, ["broken", "foreign"]);
        assert!(root.join("a.txt").is_file());
        assert!(record_path(&base, "broken").is_file());
    }

    #[test]
    fn gc_on_missing_base_dir_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = gc_orphans(&tmp.path().join("absent")).expect("gc");
        assert_eq!(report, GcReport::default());
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_distinguishes_live_reaped_and_recycled() {
        let me = std::process::id();
        assert!(pid_alive(me, process_start_time(me)));
        let mut child = Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(!pid_alive(pid, None));
        if let Some(start) = process_start_time(me) {
            assert!(
                !pid_alive(me, Some(start + 1)),
                "start-time mismatch means a recycled PID"
            );
        }
        assert!(pid_alive(0, None), "PID 0 is never treated as a dead owner");
    }

    #[test]
    fn porcelain_parser_handles_copies_and_short_fields() {
        let raw = b"C  new.rs\0old.rs\0 D gone.rs\0x\0";
        assert_eq!(
            parse_porcelain_z(raw),
            vec![
                StatusEntry {
                    index: 'C',
                    worktree: ' ',
                    path: "new.rs".into(),
                    orig_path: Some("old.rs".into())
                },
                StatusEntry {
                    index: ' ',
                    worktree: 'D',
                    path: "gone.rs".into(),
                    orig_path: None
                },
            ]
        );
    }
}
