//! Multi-root workspace: `/add-dir`, `/remove-dir`, `/dirs` and `--add-dir`.
//!
//! Port of TS `session/session-workspace.ts` (normalization), the
//! `SessionManager` workspace-directory methods (`addWorkspaceDirectory`,
//! `removeWorkspaceDirectory`, `setAdditionalDirectories`: the list lives in the
//! session header's `additionalDirectories`) and the `builtin-lifecycle.ts`
//! command handlers. The live list feeds the system prompt's
//! `<workspace-roots>` block and its context-file discovery.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;

/// `--add-dir` values, kept for every session this process opens.
static CLI_DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Record the `--add-dir` flags (main, before any runtime starts).
pub fn set_cli_dirs(dirs: Vec<PathBuf>) {
    let _ = CLI_DIRS.set(dirs);
}

/// The session's additional workspace directories (absolute, normalized,
/// deduplicated, never the cwd), in order.
#[derive(Default)]
pub struct WorkspaceDirs(Mutex<Vec<String>>);

impl WorkspaceDirs {
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn replace(&self, dirs: Vec<String>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dirs;
    }
}

/// TS `normalizeWorkspaceDirectory`: expand a leading `~`/`~/`, resolve a
/// relative path against `base`, and normalize `.`/`..` lexically (like
/// `path.resolve`, symlinks are not followed).
#[must_use]
pub fn normalize(directory: &str, base: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let expanded = match (directory, home) {
        ("~", Some(home)) => home,
        (d, Some(home)) if d.starts_with("~/") => home.join(&d[2..]),
        (d, _) => PathBuf::from(d),
    };
    let joined = base.join(expanded);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// TS `normalizeSessionWorkspace` + `additionalWorkspaceDirectories`: every
/// entry normalized against `cwd`, deduplicated in order, the cwd dropped.
#[must_use]
pub fn normalize_list<'a>(
    cwd: &Path,
    directories: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let cwd = normalize(&cwd.to_string_lossy(), Path::new("/"));
    let mut out: Vec<String> = Vec::new();
    for directory in directories {
        let resolved = normalize(directory, &cwd);
        if resolved == cwd {
            continue;
        }
        let text = resolved.to_string_lossy().into_owned();
        if !out.contains(&text) {
            out.push(text);
        }
    }
    out
}

/// `workspace.additionalDirectories` from the settings layered for `cwd`.
fn settings_dirs(cwd: &Path) -> Vec<String> {
    match omp_settings::Settings::load(cwd).get("workspace.additionalDirectories") {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Roots a resumed or forked session merges into its header's list (TS main:
/// `--add-dir` ∪ `workspace.additionalDirectories`).
#[must_use]
pub fn configured(cwd: &Path) -> Vec<String> {
    let cli: Vec<String> = CLI_DIRS
        .get()
        .map(|dirs| {
            dirs.iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let settings = settings_dirs(cwd);
    normalize_list(cwd, cli.iter().chain(settings.iter()).map(String::as_str))
}

/// Roots a brand-new session starts with (TS `newSession`:
/// `workspace.additionalDirectories` only).
#[must_use]
pub fn configured_for_new_session(cwd: &Path) -> Vec<String> {
    let settings = settings_dirs(cwd);
    normalize_list(cwd, settings.iter().map(String::as_str))
}

/// TS `formatWorkspaceDirectories`.
#[must_use]
pub fn format(cwd: &Path, dirs: &[String], note: Option<&str>) -> String {
    let mut lines = vec![
        "Workspace directories:".to_owned(),
        format!("  {} (working directory)", cwd.display()),
    ];
    lines.extend(dirs.iter().map(|d| format!("  {d}")));
    match note {
        Some(note) => format!("{note}\n{}", lines.join("\n")),
        None => lines.join("\n"),
    }
}

/// What `/add-dir` resolved to against the current list.
#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    /// Appended; the new list.
    Added { resolved: String, dirs: Vec<String> },
    /// Already a workspace root.
    Already(String),
}

/// TS `addWorkspaceDirectory` over an already validated directory.
///
/// # Errors
/// `resolved` is the working directory itself.
pub fn add(cwd: &Path, dirs: &[String], resolved: &Path) -> Result<AddOutcome, String> {
    let resolved_text = resolved.to_string_lossy().into_owned();
    if resolved == normalize(&cwd.to_string_lossy(), Path::new("/")) {
        return Err("The current working directory is already the primary workspace root.".into());
    }
    if dirs.contains(&resolved_text) {
        return Ok(AddOutcome::Already(resolved_text));
    }
    let mut next = dirs.to_vec();
    next.push(resolved_text.clone());
    Ok(AddOutcome::Added {
        resolved: resolved_text,
        dirs: next,
    })
}

/// TS `removeWorkspaceDirectory`: the new list, or `None` when `resolved` is
/// not an additional root.
#[must_use]
pub fn remove(dirs: &[String], resolved: &Path) -> Option<Vec<String>> {
    let text = resolved.to_string_lossy();
    dirs.iter()
        .any(|d| *d == text)
        .then(|| dirs.iter().filter(|d| **d != text).cloned().collect())
}

fn notice(rt: &Runtime, text: &str) -> Outcome {
    rt.send(json!({"type": "command_output", "text": text}));
    Outcome::Handled {
        agent_invoked: false,
    }
}

async fn streaming(rt: &Runtime) -> bool {
    rt.agent.is_streaming().await
}

/// `/add-dir <path>` (TS `builtin-lifecycle.ts`).
pub async fn add_command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    if streaming(rt).await {
        return notice(rt, "Cannot add a directory while streaming.");
    }
    let dirs = rt.workspace_dirs.list();
    let arg = args.trim();
    if arg.is_empty() {
        return notice(rt, &format(&rt.cwd, &dirs, Some("Usage: /add-dir <path>")));
    }
    let resolved = normalize(arg, &rt.cwd);
    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return notice(rt, &format!("Not a directory: {}", resolved.display())),
        Err(_) => {
            return notice(
                rt,
                &format!("Directory does not exist: {}", resolved.display()),
            )
        }
    }
    match add(&rt.cwd, &dirs, &resolved) {
        Err(message) => notice(rt, &message),
        Ok(AddOutcome::Already(path)) => notice(rt, &format!("Already in the workspace: {path}")),
        Ok(AddOutcome::Added { resolved, dirs }) => {
            if let Err(e) = rt.set_workspace_directories(dirs.clone()).await {
                return notice(rt, &format!("{e:#}"));
            }
            notice(
                rt,
                &format(&rt.cwd, &dirs, Some(&format!("Added {resolved}."))),
            )
        }
    }
}

/// `/remove-dir <path>` (TS `builtin-lifecycle.ts`).
pub async fn remove_command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    if streaming(rt).await {
        return notice(rt, "Cannot remove a directory while streaming.");
    }
    let arg = args.trim();
    if arg.is_empty() {
        return notice(rt, "Usage: /remove-dir <path>");
    }
    let resolved = normalize(arg, &rt.cwd);
    if resolved == normalize(&rt.cwd.to_string_lossy(), Path::new("/")) {
        return notice(
            rt,
            "Cannot remove the working directory; use /move to change it.",
        );
    }
    let Some(dirs) = remove(&rt.workspace_dirs.list(), &resolved) else {
        return notice(
            rt,
            &format!("Not a workspace directory: {}", resolved.display()),
        );
    };
    if let Err(e) = rt.set_workspace_directories(dirs.clone()).await {
        return notice(rt, &format!("{e:#}"));
    }
    notice(
        rt,
        &format(
            &rt.cwd,
            &dirs,
            Some(&format!("Removed {}.", resolved.display())),
        ),
    )
}

/// `/dirs` (TS `builtin-lifecycle.ts`).
pub fn dirs_command(rt: &Runtime) -> Outcome {
    notice(rt, &format(&rt.cwd, &rt.workspace_dirs.list(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_relative_dot_segments_and_tilde() {
        let base = Path::new("/work/repo");
        assert_eq!(normalize("../lib", base), PathBuf::from("/work/lib"));
        assert_eq!(normalize("./a/./b/..", base), PathBuf::from("/work/repo/a"));
        assert_eq!(normalize("/abs/x/", base), PathBuf::from("/abs/x"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(normalize("~/proj", base), PathBuf::from(home).join("proj"));
        }
    }

    #[test]
    fn normalize_list_dedupes_and_drops_the_cwd() {
        let cwd = Path::new("/work/repo");
        let list = normalize_list(cwd, ["../lib", "/work/lib", ".", "/work/repo/", "/srv"]);
        assert_eq!(list, vec!["/work/lib".to_owned(), "/srv".to_owned()]);
    }

    #[test]
    fn add_appends_once_and_refuses_the_cwd() {
        let cwd = Path::new("/work/repo");
        let first = add(cwd, &[], Path::new("/srv/a")).unwrap();
        let AddOutcome::Added { resolved, dirs } = first else {
            panic!("expected Added");
        };
        assert_eq!(resolved, "/srv/a");
        assert_eq!(dirs, vec!["/srv/a".to_owned()]);
        assert_eq!(
            add(cwd, &dirs, Path::new("/srv/a")).unwrap(),
            AddOutcome::Already("/srv/a".into())
        );
        assert!(add(cwd, &dirs, Path::new("/work/repo")).is_err());
    }

    #[test]
    fn remove_drops_only_a_known_root() {
        let dirs = vec!["/srv/a".to_owned(), "/srv/b".to_owned()];
        assert_eq!(
            remove(&dirs, Path::new("/srv/a")),
            Some(vec!["/srv/b".to_owned()])
        );
        assert_eq!(remove(&dirs, Path::new("/srv/c")), None);
    }

    #[test]
    fn format_lists_the_cwd_first_then_the_roots() {
        let text = format(
            Path::new("/work/repo"),
            &["/srv/a".to_owned()],
            Some("Added /srv/a."),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[2].contains("/work/repo"));
        assert_eq!(lines[3].trim(), "/srv/a");
    }
}
