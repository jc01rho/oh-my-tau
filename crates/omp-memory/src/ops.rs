//! The `memory` tool's six commands over a [`MemoryRepo`], one commit per call.

use std::fs;
use std::path::Path;

use crate::block::Block;
use crate::repo::{CommitError, Committed, MemoryRepo, PathKind, RepoError};

/// Arguments of one `memory` call, as the model supplies them.
#[derive(Debug, Clone, Default)]
pub struct MemoryParams {
    /// `create`, `str_replace`, `insert`, `delete`, `rename` or `update_description`.
    pub command: Option<String>,
    /// Commit message; required.
    pub reason: String,
    /// Target block for every command except `rename`.
    pub file_path: Option<String>,
    /// Body of a new block.
    pub file_text: Option<String>,
    /// Block description (`create`, `update_description`).
    pub description: Option<String>,
    /// Text to find (`str_replace`).
    pub old_string: Option<String>,
    /// Replacement text (`str_replace`).
    pub new_string: Option<String>,
    /// One-based body line to insert before (`insert`).
    pub insert_line: Option<i64>,
    /// Text to insert (`insert`).
    pub insert_text: Option<String>,
    /// Source path (`rename`).
    pub old_path: Option<String>,
    /// Destination path (`rename`).
    pub new_path: Option<String>,
}

/// A committed write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    /// The text the tool returns to the model.
    pub message: String,
    /// The commit that recorded the write.
    pub commit: Committed,
    /// Repository-relative paths the commit touched.
    pub paths: Vec<String>,
}

pub(crate) fn failure(prefix: &str, message: impl AsRef<str>) -> RepoError {
    RepoError::new(format!("{prefix}: {}", message.as_ref()))
}

fn err(message: impl AsRef<str>) -> RepoError {
    failure("memory", message)
}

fn required<'a>(
    command: &str,
    field: &str,
    value: Option<&'a String>,
) -> Result<&'a str, RepoError> {
    match value {
        Some(text) if !text.trim().is_empty() => Ok(text),
        _ => Err(err(format!(
            "{command}: '{field}' must be a non-empty string"
        ))),
    }
}

/// Runs one `memory` command and commits it.
///
/// # Errors
/// A malformed call, a path outside the repository, a `read_only` block, a
/// dirty work tree, or a git failure. A refused write leaves the tree clean.
pub fn run_memory(repo: &MemoryRepo, params: &MemoryParams) -> Result<WriteOutcome, RepoError> {
    run_memory_when(repo, params, &|| false)
}

/// [`run_memory`] that checks `cancelled` once the write lock is held, so a
/// call cancelled while waiting never mutates the repository.
///
/// # Errors
/// As [`run_memory`], or `memory: cancelled before writing`.
pub fn run_memory_when(
    repo: &MemoryRepo,
    params: &MemoryParams,
    cancelled: &dyn Fn() -> bool,
) -> Result<WriteOutcome, RepoError> {
    if params.reason.trim().is_empty() {
        return Err(err("'reason' must be a non-empty string"));
    }
    let command = params
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| err("'command' must be a non-empty string"))?;
    let _lock = repo.lock()?;
    if cancelled() {
        return Err(err("cancelled before writing"));
    }
    repo.clean_check().map_err(|e| err(e.message()))?;
    let mut affected = Vec::new();
    if let Err(error) = execute(repo, command, params, &mut affected) {
        return Err(rolled_back(repo, &affected, error));
    }
    commit(
        repo,
        &affected,
        &params.reason,
        &format!("Memory {command}"),
        "memory",
        command,
    )
}

pub(crate) fn rolled_back(repo: &MemoryRepo, paths: &[String], error: RepoError) -> RepoError {
    match repo.restore_paths(paths) {
        Ok(()) => error,
        Err(restore) => RepoError::new(format!("{}\n{restore}", error.message())),
    }
}

pub(crate) fn commit(
    repo: &MemoryRepo,
    paths: &[String],
    reason: &str,
    label: &str,
    prefix: &str,
    command: &str,
) -> Result<WriteOutcome, RepoError> {
    if paths.is_empty() {
        return Err(failure(prefix, format!("{command} made no changes")));
    }
    let commit = match repo.commit_paths(paths, reason) {
        Ok(commit) => commit,
        Err(CommitError::NoEffectiveChanges) => {
            return Err(rolled_back(
                repo,
                paths,
                failure(
                    prefix,
                    format!(
                        "{command} made no effective changes; nothing was committed. \
                         The new content matched what was already on disk."
                    ),
                ),
            ));
        }
        Err(CommitError::Failed(error)) => {
            return Err(rolled_back(repo, paths, failure(prefix, error.message())));
        }
    };
    let message = if repo.has_remote() {
        format!(
            "{label} committed ({}); harness will sync after the turn.",
            commit.short()
        )
    } else {
        format!("{label} committed locally ({}).", commit.short())
    };
    Ok(WriteOutcome {
        message,
        commit,
        paths: paths.to_vec(),
    })
}

fn execute(
    repo: &MemoryRepo,
    command: &str,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(), RepoError> {
    match command {
        "create" => create(repo, params, affected),
        "str_replace" => edit_body(repo, params, affected, |body| {
            let old = required("str_replace", "old_string", params.old_string.as_ref())?;
            let new = params
                .new_string
                .as_deref()
                .ok_or_else(|| err("str_replace: 'new_string' must be a non-empty string"))?;
            if !body.contains(old) {
                return Err(err(
                    "str_replace: old_string was not found in the target memory block",
                ));
            }
            Ok(body.replacen(old, new, 1))
        }),
        "insert" => {
            let line = params
                .insert_line
                .ok_or_else(|| err("insert: 'insert_line' must be a number"))?;
            let text = required("insert", "insert_text", params.insert_text.as_ref())?;
            edit_body(repo, params, affected, |body| {
                Ok(insert_lines(body, line, text))
            })
        }
        "delete" => delete(repo, params, affected),
        "rename" => rename(repo, params, affected),
        "update_description" => update_description(repo, params, affected),
        other => Err(err(format!("unknown command: {other}"))),
    }
}

fn target_for(
    repo: &MemoryRepo,
    command: &str,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(std::path::PathBuf, String), RepoError> {
    let raw = required(command, "file_path", params.file_path.as_ref())?;
    let path = repo
        .resolve(raw, "file_path", PathKind::Block)
        .map_err(|e| err(e.message()))?;
    let rel = repo.relative(&path);
    affected.push(rel.clone());
    Ok((path, rel))
}

fn create(
    repo: &MemoryRepo,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(), RepoError> {
    let description = required("create", "description", params.description.as_ref())?;
    let raw = required("create", "file_path", params.file_path.as_ref())?;
    let path = repo
        .resolve(raw, "file_path", PathKind::Block)
        .map_err(|e| err(e.message()))?;
    let rel = repo.relative(&path);
    if path.exists() {
        return Err(err(format!("create: block already exists at {rel}")));
    }
    repo.reject_ignored(&rel).map_err(|e| err(e.message()))?;
    affected.push(rel);
    let block = Block::from_file_text(description, params.file_text.as_deref().unwrap_or(""))
        .map_err(|e| err(e.message()))?;
    write_file(&path, &block.render())
}

fn edit_body(
    repo: &MemoryRepo,
    params: &MemoryParams,
    affected: &mut Vec<String>,
    change: impl FnOnce(&str) -> Result<String, RepoError>,
) -> Result<(), RepoError> {
    let command = params.command.as_deref().unwrap_or_default();
    let (path, rel) = target_for(repo, command, params, affected)?;
    let block = read_editable(&path, &rel)?;
    let body = change(&block.body)?;
    write_file(&path, &block.with_body(body).render())
}

fn update_description(
    repo: &MemoryRepo,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(), RepoError> {
    let description = required(
        "update_description",
        "description",
        params.description.as_ref(),
    )?;
    let (path, rel) = target_for(repo, "update_description", params, affected)?;
    let block = read_editable(&path, &rel)?;
    let next = block
        .with_description(description)
        .map_err(|e| err(e.message()))?;
    write_file(&path, &next.render())
}

fn delete(
    repo: &MemoryRepo,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(), RepoError> {
    let raw = required("delete", "file_path", params.file_path.as_ref())?;
    let any = repo
        .resolve(raw, "file_path", PathKind::Any)
        .map_err(|e| err(e.message()))?;
    if any.is_dir() && !fs::symlink_metadata(&any).is_ok_and(|m| m.is_symlink()) {
        assert_tree_editable(repo, &any)?;
        let mut files = Vec::new();
        collect_files(repo, &any, &mut files)?;
        if files.is_empty() {
            return Err(err(format!(
                "delete: {} holds no files, so removing it could not be undone",
                repo.relative(&any)
            )));
        }
        let tracked = repo
            .tracked_under(&repo.relative(&any))
            .map_err(|e| err(e.message()))?;
        let unrestorable: Vec<&str> = files
            .iter()
            .filter(|f| !tracked.contains(f))
            .map(String::as_str)
            .collect();
        if !unrestorable.is_empty() {
            return Err(err(format!(
                "delete: {} contains files git does not track (untracked or ignored), which a rollback could not restore: {}",
                repo.relative(&any),
                unrestorable.join(", ")
            )));
        }
        affected.extend(files);
        return fs::remove_dir_all(&any).map_err(|e| err(e.to_string()));
    }
    let (path, rel) = target_for(repo, "delete", params, affected)?;
    read_editable(&path, &rel)?;
    fs::remove_file(&path).map_err(|e| err(e.to_string()))
}

fn rename(
    repo: &MemoryRepo,
    params: &MemoryParams,
    affected: &mut Vec<String>,
) -> Result<(), RepoError> {
    let old = required("rename", "old_path", params.old_path.as_ref())?;
    let new = required("rename", "new_path", params.new_path.as_ref())?;
    let src = repo
        .resolve(old, "old_path", PathKind::Block)
        .map_err(|e| err(e.message()))?;
    let dst = repo
        .resolve(new, "new_path", PathKind::Block)
        .map_err(|e| err(e.message()))?;
    let (src_rel, dst_rel) = (repo.relative(&src), repo.relative(&dst));
    affected.push(src_rel.clone());
    affected.push(dst_rel.clone());
    if !src.exists() {
        return Err(err(format!("rename: source does not exist at {src_rel}")));
    }
    if dst.exists() {
        return Err(err(format!(
            "rename: destination already exists at {dst_rel}"
        )));
    }
    read_editable(&src, &src_rel)?;
    repo.reject_ignored(&dst_rel)
        .map_err(|e| err(e.message()))?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| err(e.to_string()))?;
    }
    fs::rename(&src, &dst).map_err(|e| err(e.to_string()))
}

fn insert_lines(body: &str, insert_line: i64, text: &str) -> String {
    let mut lines: Vec<&str> = body.split('\n').collect();
    let at = usize::try_from(insert_line.saturating_sub(1)).map_or(0, |i| i.min(lines.len()));
    lines.splice(at..at, text.split('\n'));
    lines.join("\n")
}

pub(crate) fn read_text(path: &Path, rel: &str, prefix: &str) -> Result<String, RepoError> {
    let bytes =
        fs::read(path).map_err(|e| failure(prefix, format!("failed to read {rel}: {e}")))?;
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return Err(failure(
            prefix,
            format!("{rel} is UTF-16 encoded; convert it to UTF-8 before using the memory tool"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        failure(
            prefix,
            format!("{rel} is not valid UTF-8; convert it to UTF-8 before using the memory tool"),
        )
    })
}

fn read_editable(path: &Path, rel: &str) -> Result<Block, RepoError> {
    let block = Block::parse(&read_text(path, rel, "memory")?).map_err(|e| err(e.message()))?;
    if block.read_only {
        return Err(err(format!("{rel} is read_only and cannot be modified")));
    }
    Ok(block)
}

fn assert_tree_editable(repo: &MemoryRepo, dir: &Path) -> Result<(), RepoError> {
    let mut files = Vec::new();
    collect_files(repo, dir, &mut files)?;
    for rel in files
        .iter()
        .filter(|rel| Path::new(rel).extension().is_some_and(|ext| ext == "md"))
    {
        let path = repo.root().join(rel);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if let Ok(block) = Block::parse(&read_text(&path, rel, "memory")?) {
            if block.read_only {
                return Err(err(format!("{rel} is read_only and cannot be modified")));
            }
        }
    }
    Ok(())
}

fn collect_files(repo: &MemoryRepo, dir: &Path, out: &mut Vec<String>) -> Result<(), RepoError> {
    for entry in fs::read_dir(dir).map_err(|e| err(e.to_string()))? {
        let entry = entry.map_err(|e| err(e.to_string()))?;
        if entry.file_name().to_str().is_none() {
            return Err(err(format!(
                "{} has a name that is not valid UTF-8; rename it before using the memory tool",
                entry.path().display()
            )));
        }
        let kind = entry.file_type().map_err(|e| err(e.to_string()))?;
        if kind.is_dir() {
            collect_files(repo, &entry.path(), out)?;
        } else {
            out.push(repo.relative(&entry.path()));
        }
    }
    out.sort();
    Ok(())
}

/// Writes `content` to a sibling temporary file and renames it over `path`,
/// so an existing name is replaced rather than rewritten in place: a block
/// hard-linked to a file outside the repository never changes that file.
pub(crate) fn write_file(path: &Path, content: &str) -> Result<(), RepoError> {
    let parent = path
        .parent()
        .ok_or_else(|| err(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(parent).map_err(|e| err(e.to_string()))?;
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let staging = parent.join(format!(".{name}.omp-memory-{}.tmp", std::process::id()));
    if let Err(e) = fs::write(&staging, content) {
        let _ = fs::remove_file(&staging);
        return Err(err(e.to_string()));
    }
    fs::rename(&staging, path).map_err(|e| {
        let _ = fs::remove_file(&staging);
        err(e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::test_support::init_repo;

    fn repo(tag: &str) -> MemoryRepo {
        MemoryRepo::open(init_repo(tag), "omt").unwrap()
    }

    fn call(command: &str) -> MemoryParams {
        MemoryParams {
            command: Some(command.to_owned()),
            reason: "test reason".to_owned(),
            ..MemoryParams::default()
        }
    }

    fn create_params(path: &str, body: &str) -> MemoryParams {
        MemoryParams {
            file_path: Some(path.to_owned()),
            description: Some("a block".to_owned()),
            file_text: Some(body.to_owned()),
            ..call("create")
        }
    }

    fn git(repo: &MemoryRepo, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo.root())
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commits(repo: &MemoryRepo) -> usize {
        git(repo, &["rev-list", "--count", "HEAD"]).parse().unwrap()
    }

    fn read(repo: &MemoryRepo, rel: &str) -> String {
        fs::read_to_string(repo.root().join(rel)).unwrap()
    }

    fn seeded(tag: &str) -> MemoryRepo {
        let repo = repo(tag);
        run_memory(&repo, &create_params("system/a.md", "one\ntwo\nthree\n")).unwrap();
        repo
    }

    fn is_hex_sha(text: &str) -> bool {
        text.len() == 7 && text.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn create_commits_with_the_reason_and_the_bound_identity() {
        let repo = repo("create");
        let before = commits(&repo);
        let out = run_memory(&repo, &create_params("system/human.md", "hello\n")).unwrap();
        assert!(
            out.message.starts_with("Memory create committed locally (")
                && out.message.ends_with(").")
        );
        assert!(is_hex_sha(
            &out.message[out.message.find('(').unwrap() + 1..out.message.len() - 2]
        ));
        assert_eq!(out.paths, ["system/human.md"]);
        assert_eq!(commits(&repo), before + 1);
        assert_eq!(
            git(&repo, &["log", "-1", "--format=%s|%ae"]),
            "test reason|omt@omo.local"
        );
        assert_eq!(
            read(&repo, "system/human.md"),
            "---\ndescription: a block\n---\nhello\n"
        );
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn create_merges_a_frontmatter_that_file_text_brings() {
        let repo = repo("create-fm");
        let text = "---\ndescription: from text\nkind: note\nexpires: 2030-01-01T00:00:00Z\n---\n- entry\n";
        run_memory(&repo, &create_params("notes/a.md", text)).unwrap();
        assert_eq!(
            read(&repo, "notes/a.md"),
            "---\ndescription: a block\nkind: note\nexpires: 2030-01-01T00:00:00Z\n---\n- entry\n"
        );
        let no_desc = "---\nkind: note\n---\nbody\n";
        run_memory(&repo, &create_params("notes/b.md", no_desc)).unwrap();
        assert_eq!(
            read(&repo, "notes/b.md"),
            "---\ndescription: a block\nkind: note\n---\nbody\n"
        );
        let locked = "---\ndescription: x\nread_only: \"true\"\n---\nbody\n";
        run_memory(&repo, &create_params("notes/c.md", locked)).unwrap();
        assert_eq!(
            read(&repo, "notes/c.md"),
            "---\ndescription: a block\nread_only: \"true\"\n---\nbody\n"
        );
    }

    #[test]
    fn create_keeps_an_unclosed_fence_as_body_text() {
        let repo = repo("create-fence");
        run_memory(&repo, &create_params("notes/a.md", "---\nnot a header\n")).unwrap();
        assert_eq!(
            read(&repo, "notes/a.md"),
            "---\ndescription: a block\n---\n---\nnot a header\n"
        );
    }

    #[test]
    fn create_requires_a_description_and_refuses_existing_blocks() {
        let repo = seeded("create-bad");
        let before = commits(&repo);
        let mut no_desc = create_params("system/b.md", "x");
        no_desc.description = None;
        assert!(run_memory(&repo, &no_desc)
            .unwrap_err()
            .message()
            .contains("'description' must be a non-empty string"));
        let dup = run_memory(&repo, &create_params("system/a.md", "x")).unwrap_err();
        assert!(dup
            .message()
            .contains("block already exists at system/a.md"));
        assert_eq!(commits(&repo), before);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn reason_and_command_are_required() {
        let repo = repo("reason");
        let mut params = create_params("a.md", "x");
        params.reason = "  ".to_owned();
        assert_eq!(
            run_memory(&repo, &params).unwrap_err().message(),
            "memory: 'reason' must be a non-empty string"
        );
        params.reason = "r".to_owned();
        params.command = None;
        assert!(run_memory(&repo, &params).is_err());
        params.command = Some("explode".to_owned());
        assert!(run_memory(&repo, &params)
            .unwrap_err()
            .message()
            .contains("unknown command: explode"));
    }

    #[test]
    fn str_replace_edits_the_body_only() {
        let repo = seeded("replace");
        let params = MemoryParams {
            file_path: Some("system/a.md".to_owned()),
            old_string: Some("two".to_owned()),
            new_string: Some("2".to_owned()),
            ..call("str_replace")
        };
        run_memory(&repo, &params).unwrap();
        assert_eq!(
            read(&repo, "system/a.md"),
            "---\ndescription: a block\n---\none\n2\nthree\n"
        );
        let missing = MemoryParams {
            old_string: Some("description".to_owned()),
            ..params
        };
        let err = run_memory(&repo, &missing).unwrap_err();
        assert!(err.message().contains("old_string was not found"), "{err}");
    }

    #[test]
    fn insert_places_text_before_the_numbered_body_line() {
        let repo = seeded("insert");
        let at = |line: i64, text: &str| MemoryParams {
            file_path: Some("system/a.md".to_owned()),
            insert_line: Some(line),
            insert_text: Some(text.to_owned()),
            ..call("insert")
        };
        run_memory(&repo, &at(2, "inserted")).unwrap();
        run_memory(&repo, &at(0, "top")).unwrap();
        run_memory(&repo, &at(99, "tail")).unwrap();
        assert_eq!(
            read(&repo, "system/a.md"),
            "---\ndescription: a block\n---\ntop\none\ninserted\ntwo\nthree\n\ntail"
        );
    }

    #[test]
    fn update_description_keeps_unknown_header_keys() {
        let repo = repo("describe");
        fs::write(
            repo.root().join("k.md"),
            "---\ndescription: old\nexpires: 2031-01-01T00:00:00Z\n---\nbody\n",
        )
        .unwrap();
        repo.commit_paths(&["k.md".to_owned()], "seed").unwrap();
        let params = MemoryParams {
            file_path: Some("k.md".to_owned()),
            description: Some("new text".to_owned()),
            ..call("update_description")
        };
        run_memory(&repo, &params).unwrap();
        assert_eq!(
            read(&repo, "k.md"),
            "---\ndescription: new text\nexpires: 2031-01-01T00:00:00Z\n---\nbody\n"
        );
    }

    #[test]
    fn rename_moves_content_unchanged_and_refuses_existing_targets() {
        let repo = seeded("rename");
        let original = read(&repo, "system/a.md");
        let params = MemoryParams {
            old_path: Some("system/a.md".to_owned()),
            new_path: Some("reference/deep/b.md".to_owned()),
            ..call("rename")
        };
        let out = run_memory(&repo, &params).unwrap();
        assert_eq!(read(&repo, "reference/deep/b.md"), original);
        assert!(!repo.root().join("system/a.md").exists());
        assert_eq!(out.paths, ["system/a.md", "reference/deep/b.md"]);
        run_memory(&repo, &create_params("c.md", "x")).unwrap();
        let clash = MemoryParams {
            old_path: Some("c.md".to_owned()),
            new_path: Some("reference/deep/b.md".to_owned()),
            ..call("rename")
        };
        assert!(run_memory(&repo, &clash)
            .unwrap_err()
            .message()
            .contains("destination already exists"));
        assert!(run_memory(
            &repo,
            &MemoryParams {
                new_path: Some("../escape.md".to_owned()),
                ..clash
            }
        )
        .is_err());
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn delete_removes_files_and_directories_recursively() {
        let repo = seeded("delete");
        run_memory(&repo, &create_params("tree/x.md", "x")).unwrap();
        run_memory(&repo, &create_params("tree/sub/y.md", "y")).unwrap();
        let file = MemoryParams {
            file_path: Some("system/a.md".to_owned()),
            ..call("delete")
        };
        run_memory(&repo, &file).unwrap();
        assert!(!repo.root().join("system/a.md").exists());
        let dir = MemoryParams {
            file_path: Some("tree".to_owned()),
            ..call("delete")
        };
        let out = run_memory(&repo, &dir).unwrap();
        assert_eq!(out.paths, ["tree/sub/y.md", "tree/x.md"]);
        assert!(!repo.root().join("tree").exists());
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn deleting_a_directory_with_untracked_or_ignored_files_is_refused() {
        let repo = seeded("deltree");
        fs::write(repo.root().join(".gitignore"), "*.bak\n").unwrap();
        repo.commit_paths(&[".gitignore".to_owned()], "ignore")
            .unwrap();
        run_memory(&repo, &create_params("tree/x.md", "x")).unwrap();
        fs::write(repo.root().join("tree/keep.bak"), "precious").unwrap();
        let dir = MemoryParams {
            file_path: Some("tree".to_owned()),
            ..call("delete")
        };
        let err = run_memory(&repo, &dir).unwrap_err();
        assert!(err.message().contains("tree/keep.bak"), "{err}");
        assert_eq!(read(&repo, "tree/keep.bak"), "precious");
        assert!(repo.root().join("tree/x.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_directory_does_not_follow_symlinks_out_of_it() {
        let repo = seeded("delsym");
        run_memory(&repo, &create_params("tree/x.md", "x")).unwrap();
        let outside = crate::test_support::temp_root("delsym-out");
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("locked.md"),
            "---\ndescription: l\nread_only: true\n---\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, repo.root().join("tree/link")).unwrap();
        repo.commit_paths(&["tree/link".to_owned()], "link")
            .unwrap();
        let dir = MemoryParams {
            file_path: Some("tree".to_owned()),
            ..call("delete")
        };
        run_memory(&repo, &dir).unwrap();
        assert!(!repo.root().join("tree").exists());
        assert!(outside.join("locked.md").exists());
    }

    #[test]
    fn ignored_destinations_are_refused_before_writing() {
        let repo = repo("ignoredst");
        fs::write(repo.root().join(".gitignore"), "*.md.bak\nscratch/\n").unwrap();
        repo.commit_paths(&[".gitignore".to_owned()], "ignore")
            .unwrap();
        let err = run_memory(&repo, &create_params("scratch/a.md", "x")).unwrap_err();
        assert!(err.message().contains("ignored by git"), "{err}");
        assert!(!repo.root().join("scratch").exists());
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_cancelled_call_writes_nothing() {
        let repo = repo("cancelled");
        let err = run_memory_when(&repo, &create_params("c.md", "x"), &|| true).unwrap_err();
        assert!(err.message().contains("cancelled"), "{err}");
        assert!(!repo.root().join("c.md").exists());
        let patch_err = crate::patch::run_apply_patch_when(
            &repo,
            "r",
            "*** Begin Patch\n*** Add File: d.md\n+x\n*** End Patch",
            &|| true,
        )
        .unwrap_err();
        assert!(patch_err.message().contains("cancelled"), "{patch_err}");
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[cfg(unix)]
    #[test]
    fn edits_through_a_directory_symlink_are_refused_before_writing() {
        let repo = seeded("dirlink");
        std::os::unix::fs::symlink(repo.root().join("system"), repo.root().join("alias")).unwrap();
        repo.commit_paths(&["alias".to_owned()], "alias").unwrap();
        let params = MemoryParams {
            file_path: Some("alias/a.md".to_owned()),
            old_string: Some("one".to_owned()),
            new_string: Some("1".to_owned()),
            ..call("str_replace")
        };
        let err = run_memory(&repo, &params).unwrap_err();
        assert!(err.message().contains("symlink"), "{err}");
        assert!(read(&repo, "system/a.md").contains("one"));
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_with_a_non_utf8_name_inside_is_not_deleted() {
        use std::os::unix::ffi::OsStrExt;
        let repo = seeded("nonutf8");
        run_memory(&repo, &create_params("tree/x.md", "x")).unwrap();
        let odd = repo
            .root()
            .join("tree")
            .join(std::ffi::OsStr::from_bytes(b"bad\xff.md"));
        fs::write(&odd, "x").unwrap();
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                "odd",
            ],
        );
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
        let dir = MemoryParams {
            file_path: Some("tree".to_owned()),
            ..call("delete")
        };
        let err = run_memory(&repo, &dir).unwrap_err();
        assert!(err.message().contains("UTF-8"), "{err}");
        assert!(odd.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_edit_never_writes_through_a_hard_link() {
        let repo = seeded("hardlink");
        let outside = crate::test_support::temp_root("hardlink-out");
        fs::create_dir_all(&outside).unwrap();
        let twin = outside.join("twin.md");
        fs::hard_link(repo.root().join("system/a.md"), &twin).unwrap();
        let before = fs::read_to_string(&twin).unwrap();
        let params = MemoryParams {
            file_path: Some("system/a.md".to_owned()),
            old_string: Some("two".to_owned()),
            new_string: Some("2".to_owned()),
            ..call("str_replace")
        };
        run_memory(&repo, &params).unwrap();
        assert!(read(&repo, "system/a.md").contains("\n2\n"));
        assert_eq!(fs::read_to_string(&twin).unwrap(), before);
    }

    #[test]
    fn an_empty_directory_is_not_removed() {
        let repo = seeded("emptydir");
        fs::create_dir_all(repo.root().join("hollow/inner")).unwrap();
        let dir = MemoryParams {
            file_path: Some("hollow".to_owned()),
            ..call("delete")
        };
        let err = run_memory(&repo, &dir).unwrap_err();
        assert!(err.message().contains("holds no files"), "{err}");
        assert!(repo.root().join("hollow/inner").is_dir());
    }

    #[test]
    fn a_refused_create_leaves_an_existing_directory_alone() {
        let repo = repo("refusedcreate");
        fs::create_dir_all(repo.root().join("taken.md")).unwrap();
        let err = run_memory(&repo, &create_params("taken.md", "x")).unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert!(repo.root().join("taken.md").is_dir());
    }

    #[test]
    fn read_only_blocks_every_mutating_command() {
        let repo = repo("readonly");
        let locked = "---\ndescription: locked\nread_only: \"true\"\n---\nbody\n";
        fs::write(repo.root().join("ro.md"), locked).unwrap();
        fs::create_dir_all(repo.root().join("dir")).unwrap();
        fs::write(repo.root().join("dir/ro2.md"), locked).unwrap();
        repo.commit_paths(&["ro.md".to_owned(), "dir/ro2.md".to_owned()], "seed")
            .unwrap();
        let before = commits(&repo);
        let on = |command: &str| MemoryParams {
            file_path: Some("ro.md".to_owned()),
            old_string: Some("body".to_owned()),
            new_string: Some("x".to_owned()),
            insert_line: Some(1),
            insert_text: Some("x".to_owned()),
            description: Some("d".to_owned()),
            old_path: Some("ro.md".to_owned()),
            new_path: Some("moved.md".to_owned()),
            ..call(command)
        };
        for command in [
            "str_replace",
            "insert",
            "delete",
            "rename",
            "update_description",
        ] {
            let err = run_memory(&repo, &on(command)).unwrap_err();
            assert!(err.message().contains("read_only"), "{command}: {err}");
        }
        let dir = MemoryParams {
            file_path: Some("dir".to_owned()),
            ..call("delete")
        };
        assert!(run_memory(&repo, &dir)
            .unwrap_err()
            .message()
            .contains("read_only"));
        assert_eq!(read(&repo, "ro.md"), locked);
        assert_eq!(commits(&repo), before);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn paths_outside_the_repository_are_refused_without_side_effects() {
        let repo = repo("outside");
        let before = commits(&repo);
        for bad in ["../x.md", "/etc/passwd", "notes.txt", ".git/config"] {
            assert!(
                run_memory(&repo, &create_params(bad, "x")).is_err(),
                "{bad}"
            );
        }
        assert_eq!(commits(&repo), before);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn unrelated_uncommitted_changes_refuse_the_write() {
        let repo = seeded("dirty");
        fs::write(repo.root().join("stray.md"), "x").unwrap();
        let before = commits(&repo);
        let err = run_memory(&repo, &create_params("new.md", "x")).unwrap_err();
        assert!(err.message().contains("uncommitted changes"), "{err}");
        assert!(!repo.root().join("new.md").exists());
        assert_eq!(commits(&repo), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_rejecting_hook_rolls_the_write_back() {
        use std::os::unix::fs::PermissionsExt;
        let repo = seeded("hook");
        let hook = repo.root().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho no >&2\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let before = commits(&repo);
        let err = run_memory(&repo, &create_params("system/b.md", "x")).unwrap_err();
        assert!(err.message().starts_with("memory: "), "{err}");
        assert!(!repo.root().join("system/b.md").exists());
        assert_eq!(commits(&repo), before);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_configured_remote_changes_the_success_line() {
        let repo = repo("remote");
        git(
            &repo,
            &["remote", "add", "origin", "https://example.invalid/m.git"],
        );
        let out = run_memory(&repo, &create_params("a.md", "x")).unwrap();
        assert!(
            out.message.starts_with("Memory create committed (")
                && out
                    .message
                    .ends_with("); harness will sync after the turn."),
            "{}",
            out.message
        );
    }

    #[test]
    fn concurrent_calls_serialise_into_separate_commits() {
        let repo = repo("concurrent");
        let before = commits(&repo);
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let repo = repo.clone();
                std::thread::spawn(move || {
                    run_memory(&repo, &create_params(&format!("n{i}.md"), "x")).map(|_| ())
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(commits(&repo), before + 4);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn rewriting_identical_content_reports_no_effective_change() {
        let repo = seeded("noop");
        let params = MemoryParams {
            file_path: Some("system/a.md".to_owned()),
            old_string: Some("one".to_owned()),
            new_string: Some("one".to_owned()),
            ..call("str_replace")
        };
        let err = run_memory(&repo, &params).unwrap_err();
        assert!(err.message().contains("made no effective changes"), "{err}");
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }
}
