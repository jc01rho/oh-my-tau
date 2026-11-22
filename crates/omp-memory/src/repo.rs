//! Git-backed markdown memory repository: path confinement, locking and commits.
//!
//! This is the on-disk contract the `OmOMeow` gateway's `memory` and
//! `memory_apply_patch` tools share: a plain git work tree of `.md` blocks,
//! one commit per tool call, authored as `<identity>@omo.local`. Git is driven
//! as a subprocess so the crate gains no native dependency.

use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A memory repository failure. The message is the tool-facing text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RepoError(String);

impl RepoError {
    /// Wraps a tool-facing message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The tool-facing message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// How a commit attempt ended without a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// Staging the paths left nothing to commit.
    NoEffectiveChanges,
    /// Git refused or failed.
    Failed(RepoError),
}

/// What kind of path a caller wants resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// A memory block: must be a `.md` file (an omitted extension is added).
    Block,
    /// Any file or directory inside the repository.
    Any,
}

/// A finished commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// Full commit id.
    pub sha: String,
}

impl Committed {
    /// The seven-character id the tools report.
    pub fn short(&self) -> &str {
        self.sha.get(..7).unwrap_or(&self.sha)
    }
}

/// An opened memory repository bound to a commit identity.
#[derive(Debug, Clone)]
pub struct MemoryRepo {
    root: PathBuf,
    real_root: PathBuf,
    git_dir: PathBuf,
    identity: String,
}

/// Holds the repository's cross-process write lock until dropped.
#[derive(Debug)]
pub struct RepoLock {
    _file: File,
}

impl MemoryRepo {
    /// Opens the git work tree at `root` and binds `identity` as the author.
    ///
    /// # Errors
    /// `root` is not a directory containing a git repository, or `identity`
    /// is empty.
    pub fn open(
        root: impl AsRef<Path>,
        identity: impl Into<String>,
    ) -> std::result::Result<Self, RepoError> {
        let identity = identity.into();
        if identity.trim().is_empty() || identity.contains(['\n', '<', '>']) {
            return Err(RepoError::new("memory identity is invalid"));
        }
        let root = std::path::absolute(root.as_ref())
            .map_err(|e| RepoError::new(format!("memory repository path is invalid: {e}")))?;
        let real_root = fs::canonicalize(&root).map_err(|e| {
            RepoError::new(format!(
                "memory repository {} is unavailable: {e}",
                root.display()
            ))
        })?;
        if !real_root.join(".git").exists() {
            return Err(RepoError::new(format!(
                "{} is not a git repository",
                root.display()
            )));
        }
        let repo = Self {
            root,
            real_root,
            git_dir: PathBuf::new(),
            identity,
        };
        let top = repo.git_ok(&["rev-parse", "--show-toplevel"])?;
        let top = fs::canonicalize(top.trim_end_matches(['\n', '\r'])).map_err(|e| {
            RepoError::new(format!("memory repository top level is unavailable: {e}"))
        })?;
        if top != repo.real_root {
            return Err(RepoError::new(format!(
                "{} is not the top of its git work tree ({})",
                repo.root.display(),
                top.display()
            )));
        }
        let git_dir = repo.git_ok(&["rev-parse", "--absolute-git-dir"])?;
        Ok(Self {
            git_dir: PathBuf::from(git_dir.trim_end_matches(['\n', '\r'])),
            ..repo
        })
    }

    /// The repository work tree.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bound commit identity (the author name; the email is
    /// `<identity>@omo.local`).
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Resolves a tool-supplied path to an absolute path inside the repository.
    ///
    /// Relative paths are repository-relative; absolute paths must already be
    /// inside it. `field` names the argument in error messages.
    ///
    /// # Errors
    /// The path is empty, escapes the repository (`..`, a symlink, an absolute
    /// path elsewhere), names `.git`, or - for [`PathKind::Block`] - is not a
    /// `.md` file.
    pub fn resolve(
        &self,
        input: &str,
        field: &str,
        kind: PathKind,
    ) -> std::result::Result<PathBuf, RepoError> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(path_error(format!("'{field}' must be a non-empty string")));
        }
        if raw.contains('\0') {
            return Err(path_error(format!("'{field}' contains invalid null bytes")));
        }
        if raw.starts_with("~/") || raw.starts_with("$HOME/") {
            return Err(path_error(format!(
                "'{field}' must be a memory-relative path, not a home-relative filesystem path"
            )));
        }
        let absolute = Path::new(raw).is_absolute();
        let relative = if absolute {
            let abs = Path::new(raw);
            let inside = abs
                .strip_prefix(&self.root)
                .or_else(|_| abs.strip_prefix(&self.real_root))
                .map_or_else(|_| String::new(), |p| p.to_string_lossy().into_owned());
            if inside.is_empty() {
                return Err(path_error(format!(
                    "The memory tool can only be used to modify files in {{{}}} or provided as a relative path",
                    self.root.display()
                )));
            }
            inside
        } else {
            raw.to_owned()
        };
        let target = self
            .root
            .join(normalize_relative(&relative, field, kind, !absolute)?);
        let mut walked = self.root.clone();
        for part in target
            .strip_prefix(&self.root)
            .unwrap_or(&target)
            .components()
        {
            walked.push(part);
            if fs::symlink_metadata(&walked).is_ok_and(|m| m.is_symlink()) {
                return Err(path_error(format!(
                    "'{field}' goes through a symlink ({}); name the real path instead",
                    self.relative(&walked)
                )));
            }
        }
        self.assert_confined(&target)?;
        Ok(target)
    }

    /// The repository-relative, `/`-separated form of an absolute path.
    pub fn relative(&self, abs: &Path) -> String {
        let rel = abs.strip_prefix(&self.root).unwrap_or(abs);
        rel.components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn assert_confined(&self, target: &Path) -> std::result::Result<(), RepoError> {
        let mut candidate = target;
        loop {
            match fs::symlink_metadata(candidate) {
                Ok(meta) => {
                    let real = fs::canonicalize(candidate).map_err(|_| {
                        path_error(if meta.is_symlink() {
                            "path is a dangling symlink"
                        } else {
                            "path cannot be resolved"
                        })
                    })?;
                    let rest = target.strip_prefix(candidate).unwrap_or(Path::new(""));
                    return self.assert_real_path_allowed(&real.join(rest));
                }
                Err(_) => match candidate.parent() {
                    Some(parent) if parent.starts_with(&self.root) => candidate = parent,
                    _ => return Ok(()),
                },
            }
        }
    }

    fn assert_real_path_allowed(&self, real: &Path) -> std::result::Result<(), RepoError> {
        let Ok(inside) = real.strip_prefix(&self.real_root) else {
            return Err(path_error("path escapes memory root through a symlink"));
        };
        let hits_git = inside.components().any(|c| {
            matches!(c, Component::Normal(part) if part.to_string_lossy().eq_ignore_ascii_case(".git"))
        });
        if hits_git {
            return Err(path_error(
                "path cannot target .git administration paths (also through a symlink)",
            ));
        }
        Ok(())
    }

    /// Fails when git ignores `rel`, because an ignored file can be neither
    /// committed nor rolled back.
    ///
    /// # Errors
    /// `rel` matches an ignore rule, or git failed.
    pub fn reject_ignored(&self, rel: &str) -> std::result::Result<(), RepoError> {
        match self
            .git_with(false, &["check-ignore", "-q", "--", rel])?
            .status
            .code()
        {
            Some(1) => Ok(()),
            Some(0) => Err(RepoError::new(format!(
                "{rel} is ignored by git, so it cannot be committed; choose another path"
            ))),
            _ => Err(RepoError::new("git could not check the ignore rules")),
        }
    }

    /// The tracked files under `rel` (a directory), repository-relative.
    ///
    /// # Errors
    /// Git failed.
    pub fn tracked_under(&self, rel: &str) -> std::result::Result<Vec<String>, RepoError> {
        let listing = self.git_ok(&["ls-files", "-z", "--", rel])?;
        Ok(listing
            .split('\0')
            .filter(|f| !f.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Takes the repository-wide write lock (blocking). It is an advisory file
    /// lock inside the git directory, so it also serialises other processes
    /// and is released if the holder dies.
    ///
    /// # Errors
    /// The lock file cannot be created or locked.
    pub fn lock(&self) -> std::result::Result<RepoLock, RepoError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.git_dir.join("omp-memory.lock"))
            .map_err(|e| RepoError::new(format!("cannot open the memory write lock: {e}")))?;
        file.lock()
            .map_err(|e| RepoError::new(format!("cannot take the memory write lock: {e}")))?;
        Ok(RepoLock { _file: file })
    }

    /// Fails when the work tree has any uncommitted change, so a write never
    /// sweeps unrelated edits into its commit.
    ///
    /// # Errors
    /// The tree is dirty or git failed.
    pub fn clean_check(&self) -> std::result::Result<(), RepoError> {
        let porcelain = self.git_ok(&["status", "--porcelain", "--untracked-files=all"])?;
        if porcelain.trim().is_empty() {
            Ok(())
        } else {
            Err(RepoError::new(format!(
                "memory repository has uncommitted changes; commit or discard them first:\n{}",
                porcelain.trim_end()
            )))
        }
    }

    /// Commits exactly `paths` with `reason` as the message.
    ///
    /// The paths are staged into an index owned by this call, seeded from HEAD,
    /// and that snapshot is what gets committed: unrelated staged content never
    /// joins the commit, and a later edit of the work tree cannot change it.
    /// Hooks still run. The shared index is then brought in line for `paths`.
    ///
    /// # Errors
    /// [`CommitError::NoEffectiveChanges`] when nothing differs from HEAD;
    /// [`CommitError::Failed`] for any git failure (including a rejecting hook).
    pub fn commit_paths(
        &self,
        paths: &[String],
        reason: &str,
    ) -> std::result::Result<Committed, CommitError> {
        if paths.is_empty() {
            return Err(CommitError::NoEffectiveChanges);
        }
        let index = self
            .git_dir
            .join(format!("omp-memory-index-{}", std::process::id()));
        let _ = fs::remove_file(&index);
        let result = self.commit_with_index(&index, paths, reason);
        let _ = fs::remove_file(&index);
        let committed = result?;
        let mut sync = vec!["reset", "-q", "--"];
        sync.extend(paths.iter().map(String::as_str));
        self.git_ok(&sync).map_err(CommitError::Failed)?;
        Ok(committed)
    }

    fn commit_with_index(
        &self,
        index: &Path,
        paths: &[String],
        reason: &str,
    ) -> std::result::Result<Committed, CommitError> {
        let run = |args: &[&str]| -> std::result::Result<Output, CommitError> {
            self.git_full(true, Some(index), args)
                .map_err(CommitError::Failed)
        };
        let ok = |args: &[&str]| -> std::result::Result<String, CommitError> {
            Self::succeeded(args, &run(args)?).map_err(CommitError::Failed)
        };
        let has_head = run(&["rev-parse", "-q", "--verify", "HEAD"])?
            .status
            .success();
        if has_head {
            ok(&["read-tree", "HEAD"])?;
        } else {
            ok(&["read-tree", "--empty"])?;
        }
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        ok(&add)?;
        let staged_differs = if has_head {
            let mut diff = vec!["diff", "--cached", "--quiet", "--"];
            diff.extend(paths.iter().map(String::as_str));
            match run(&diff)?.status.code() {
                Some(0) => false,
                Some(1) => true,
                _ => {
                    return Err(CommitError::Failed(RepoError::new(
                        "git could not compare the staged memory paths",
                    )))
                }
            }
        } else {
            let mut listed = vec!["ls-files", "--"];
            listed.extend(paths.iter().map(String::as_str));
            !ok(&listed)?.trim().is_empty()
        };
        if !staged_differs {
            return Err(CommitError::NoEffectiveChanges);
        }
        ok(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", reason])?;
        let sha = ok(&["rev-parse", "HEAD"])?;
        Ok(Committed {
            sha: sha.trim().to_owned(),
        })
    }

    /// Puts `paths` back to HEAD: unstages them, restores tracked content and
    /// removes untracked files, so a refused write leaves the tree clean.
    ///
    /// # Errors
    /// The tree is still dirty afterwards.
    pub fn restore_paths(&self, paths: &[String]) -> std::result::Result<(), RepoError> {
        for path in paths {
            // Each step may legitimately match nothing (an untracked path has
            // nothing to check out, a tracked one nothing to clean).
            let _ = self.git(&["reset", "-q", "--", path]);
            let _ = self.git(&["checkout", "-q", "--", path]);
            let _ = self.git(&["clean", "-fdq", "--", path]);
        }
        self.clean_check()
            .map_err(|e| RepoError::new(format!("rolling back the written paths failed: {e}")))
    }

    /// Whether any git remote is configured (the harness then syncs after the turn).
    pub fn has_remote(&self) -> bool {
        self.git_ok(&["remote"])
            .is_ok_and(|remotes| !remotes.trim().is_empty())
    }

    fn git(&self, args: &[&str]) -> std::result::Result<Output, RepoError> {
        self.git_with(true, args)
    }

    fn git_with(&self, literal: bool, args: &[&str]) -> std::result::Result<Output, RepoError> {
        self.git_full(literal, None, args)
    }

    fn git_full(
        &self,
        literal: bool,
        index: Option<&Path>,
        args: &[&str],
    ) -> std::result::Result<Output, RepoError> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .args(["-c", "core.quotePath=false"]);
        if literal {
            command.arg("--literal-pathspecs");
        }
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", &self.identity)
            .env("GIT_AUTHOR_EMAIL", format!("{}@omo.local", self.identity))
            .env("GIT_COMMITTER_NAME", &self.identity)
            .env(
                "GIT_COMMITTER_EMAIL",
                format!("{}@omo.local", self.identity),
            )
            .stdin(Stdio::null());
        for override_name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ] {
            command.env_remove(override_name);
        }
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
            .output()
            .map_err(|e| RepoError::new(format!("git is unavailable: {e}")))
    }

    fn git_ok(&self, args: &[&str]) -> std::result::Result<String, RepoError> {
        Self::succeeded(args, &self.git(args)?)
    }

    fn succeeded(args: &[&str], out: &Output) -> std::result::Result<String, RepoError> {
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            Err(RepoError::new(format!(
                "git {} failed: {}",
                args.iter()
                    .find(|a| !a.starts_with('-') && !a.contains('='))
                    .copied()
                    .unwrap_or("command"),
                detail.trim()
            )))
        }
    }
}

fn path_error(message: impl AsRef<str>) -> RepoError {
    RepoError::new(message.as_ref())
}

/// Validates and normalises a repository-relative path.
fn normalize_relative(
    input: &str,
    field: &str,
    kind: PathKind,
    relative_label: bool,
) -> std::result::Result<String, RepoError> {
    let mut normalized = input.trim().to_owned();
    if relative_label {
        normalized = normalized.replace('\\', "/");
    }
    if normalized.starts_with('/') {
        return Err(path_error(format!(
            "'{field}' must be a relative path like system/contacts.md"
        )));
    }
    if relative_label {
        if let Some(stripped) = normalized.strip_prefix("memory/") {
            normalized = stripped.to_owned();
        }
    }
    if Path::new(&normalized)
        .components()
        .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Err(path_error(format!(
            "'{field}' must be a relative path like system/contacts.md"
        )));
    }
    let mut segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(path_error(format!(
            "'{field}' resolves to an empty memory label"
        )));
    }
    for segment in &segments {
        if matches!(*segment, "." | "..") {
            return Err(path_error(format!(
                "'{field}' contains invalid path traversal segment"
            )));
        }
        if segment.eq_ignore_ascii_case(".git") {
            return Err(path_error(format!(
                "'{field}' cannot target .git administration paths"
            )));
        }
    }
    let with_extension;
    if kind == PathKind::Block {
        let last = segments.pop().unwrap_or_default();
        match last.rsplit_once('.') {
            Some((stem, "md")) if !stem.is_empty() => segments.push(last),
            Some((_, _)) => {
                return Err(path_error(format!(
                    "'{field}' must target a lowercase .md markdown file"
                )))
            }
            None => {
                with_extension = format!("{last}.md");
                segments.push(&with_extension);
            }
        }
    }
    Ok(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_repo;

    fn repo(tag: &str) -> MemoryRepo {
        MemoryRepo::open(init_repo(tag), "omt").unwrap()
    }

    fn resolved(repo: &MemoryRepo, input: &str) -> Result<String, String> {
        repo.resolve(input, "file_path", PathKind::Block)
            .map(|p| repo.relative(&p))
            .map_err(|e| e.message().to_owned())
    }

    #[test]
    fn open_requires_a_git_repository_and_an_identity() {
        let plain = crate::test_support::temp_root("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(MemoryRepo::open(&plain, "omt").is_err());
        assert!(MemoryRepo::open(init_repo("ident"), "  ").is_err());
        assert!(MemoryRepo::open(init_repo("ident2"), "a<b").is_err());
    }

    #[test]
    fn relative_and_inside_absolute_paths_resolve() {
        let repo = repo("resolve");
        assert_eq!(
            resolved(&repo, "system/human.md").unwrap(),
            "system/human.md"
        );
        assert_eq!(resolved(&repo, "system/human").unwrap(), "system/human.md");
        assert_eq!(resolved(&repo, "memory/a/b.md").unwrap(), "a/b.md");
        let inside = repo.root().join("reference/x.md");
        assert_eq!(
            resolved(&repo, inside.to_str().unwrap()).unwrap(),
            "reference/x.md"
        );
    }

    #[test]
    fn escapes_and_foreign_paths_are_rejected() {
        let repo = repo("escape");
        for bad in [
            "",
            "   ",
            "../outside.md",
            "a/../../outside.md",
            "/etc/passwd",
            "~/notes.md",
            "$HOME/notes.md",
            ".git/config",
            "sub/.GIT/hooks/pre-commit.md",
            "notes.txt",
            "a\0b.md",
        ] {
            assert!(resolved(&repo, bad).is_err(), "{bad:?} must be rejected");
        }
        let root_itself = repo.root().to_str().unwrap().to_owned();
        assert!(resolved(&repo, &root_itself).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_may_not_lead_out_of_the_repository() {
        let repo = repo("symlink");
        let outside = crate::test_support::temp_root("symlink-outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.root().join("link")).unwrap();
        let err = resolved(&repo, "link/x.md").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn dirty_trees_are_refused_before_any_write() {
        let repo = repo("dirty");
        repo.clean_check().unwrap();
        fs::write(repo.root().join("stray.md"), "x").unwrap();
        let err = repo.clean_check().unwrap_err();
        assert!(err.message().contains("stray.md"), "{err}");
    }

    #[test]
    fn commits_only_the_named_paths_under_the_bound_identity() {
        let repo = repo("commit");
        fs::write(repo.root().join("a.md"), "a").unwrap();
        let done = repo.commit_paths(&["a.md".to_owned()], "add a").unwrap();
        assert_eq!(
            done.sha,
            repo.git_ok(&["rev-parse", "HEAD"]).unwrap().trim()
        );
        assert_eq!(done.short().len(), 7);
        let log = repo.git_ok(&["log", "-1", "--format=%an|%ae|%s"]).unwrap();
        assert_eq!(log.trim(), "omt|omt@omo.local|add a");
        assert_eq!(
            repo.commit_paths(&["a.md".to_owned()], "again"),
            Err(CommitError::NoEffectiveChanges)
        );
        assert_eq!(
            repo.commit_paths(&[], "nothing"),
            Err(CommitError::NoEffectiveChanges)
        );
    }

    #[test]
    fn the_memory_prefix_shorthand_is_for_relative_inputs_only() {
        let repo = repo("shorthand");
        fs::create_dir_all(repo.root().join("memory")).unwrap();
        let inside = repo.root().join("memory/a.md");
        assert_eq!(
            resolved(&repo, inside.to_str().unwrap()).unwrap(),
            "memory/a.md"
        );
        assert_eq!(resolved(&repo, "memory/a.md").unwrap(), "a.md");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_reach_git_or_dangle() {
        let repo = repo("gitlink");
        std::os::unix::fs::symlink(repo.root().join(".git"), repo.root().join("admin")).unwrap();
        let err = resolved(&repo, "admin/hooks/pre-commit.md").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        std::os::unix::fs::symlink("/nonexistent-target", repo.root().join("dangling.md")).unwrap();
        let err = resolved(&repo, "dangling.md").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn ignored_paths_are_refused_up_front() {
        let repo = repo("ignored");
        fs::write(repo.root().join(".gitignore"), "*.bak\nscratch/\n").unwrap();
        repo.commit_paths(&[".gitignore".to_owned()], "ignore")
            .unwrap();
        assert!(repo.reject_ignored("a.md").is_ok());
        assert!(repo.reject_ignored("notes.bak").is_err());
        assert!(repo.reject_ignored("scratch/x.md").is_err());
    }

    #[test]
    fn a_commit_never_takes_unrelated_staged_content() {
        let repo = repo("only");
        fs::write(repo.root().join("mine.md"), "m").unwrap();
        fs::write(repo.root().join("other.md"), "o").unwrap();
        repo.git_ok(&["add", "other.md"]).unwrap();
        repo.commit_paths(&["mine.md".to_owned()], "mine").unwrap();
        let committed = repo
            .git_ok(&["show", "--name-only", "--format=", "HEAD"])
            .unwrap();
        assert_eq!(committed.trim(), "mine.md");
        assert!(repo
            .git_ok(&["diff", "--cached", "--name-only"])
            .unwrap()
            .contains("other.md"));
    }

    #[cfg(unix)]
    #[test]
    fn the_commit_records_the_staged_snapshot_not_a_later_worktree_read() {
        use std::os::unix::fs::PermissionsExt;
        let repo = repo("snapshot");
        fs::write(repo.root().join("s.md"), "staged").unwrap();
        let hook = repo.root().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\nprintf edited > s.md\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        repo.commit_paths(&["s.md".to_owned()], "snap").unwrap();
        assert_eq!(repo.git_ok(&["show", "HEAD:s.md"]).unwrap(), "staged");
    }

    #[test]
    fn remote_detection_follows_git_config() {
        let repo = repo("remote");
        assert!(!repo.has_remote());
        repo.git_ok(&["remote", "add", "origin", "https://example.invalid/m.git"])
            .unwrap();
        assert!(repo.has_remote());
    }

    #[test]
    fn restore_returns_the_tree_to_head() {
        let repo = repo("restore");
        fs::write(repo.root().join("keep.md"), "v1").unwrap();
        repo.commit_paths(&["keep.md".to_owned()], "seed").unwrap();
        fs::write(repo.root().join("keep.md"), "v2").unwrap();
        fs::write(repo.root().join("new.md"), "n").unwrap();
        repo.restore_paths(&["keep.md".to_owned(), "new.md".to_owned()])
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.root().join("keep.md")).unwrap(),
            "v1"
        );
        assert!(!repo.root().join("new.md").exists());
    }

    #[test]
    fn concurrent_writers_serialise_on_the_lock() {
        let repo = repo("lock");
        let first = repo.lock().unwrap();
        let handle = std::thread::spawn(move || repo.lock().map(|_| ()));
        drop(first);
        handle.join().unwrap().unwrap();
    }
}
