//! `edit` (`hashline` / `replace` / `patch` / `apply_patch` / `sloppy`, run by the
//! vendored `pi-edit` engine exactly as `omp`'s native `EditSession` does)
//! and `write`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use omp_xfs::{walk, Fs};
use pa_agent::types::AgentToolResult;
use pi_edit::engine::FileOp;
use pi_edit::error::{EditError, EditResult};
use pi_edit::path_policy::{PathPolicy, RecoveryGlob};
use pi_edit::session::{
    ApplyRequest, EditWriter, Session, SessionConfig, WriteRequest, WriteResponse,
};
use pi_edit::{EditMode, Resolved};
use serde_json::{json, Value};

use super::util::{format_path_relative_to_cwd, resolve_to_cwd, str_arg};
use super::Env;

/// Parameter schema of `edit` for `mode` (omp `edit/schemas.ts`).
#[must_use]
pub fn schema(mode: EditMode) -> Value {
    match mode {
        EditMode::Hashline | EditMode::ApplyPatch | EditMode::Sloppy => json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
            "additionalProperties": false
        }),
        EditMode::Replace => json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        EditMode::Patch => json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "op": {"type": "string", "enum": ["create", "delete", "update"]},
                        "rename": {"type": "string"},
                        "diff": {"type": "string"}
                    },
                    "additionalProperties": false
                }}
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    }
}

/// Narrowest per-line cap any display path applies, in UTF-8 bytes (`grep`'s
/// `MAX_COLUMN`; `read`'s 768 is wider). Without recorded read provenance,
/// hashline anchors on longer lines are rejected because their display may
/// have been cut (omp `displayMaxColumns`).
const DISPLAY_MAX_COLUMNS: usize = 512;

/// Longest a missing-target recovery searches the working tree (omp
/// `RECOVERY_TIMEOUT`).
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);

fn policy(cwd: &Path) -> PathPolicy {
    PathPolicy {
        cwd: cwd.to_path_buf(),
        home_dir: std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
        url_schemes: Vec::new(),
        url_alias_schemes: Vec::new(),
        plan_writable_roots: Vec::new(),
        plan_active: false,
        block_auto_generated: true,
    }
}

/// Host-side byte owner for the engine: plain filesystem writes.
struct FsWriter;

fn io_err(path: &Path, e: &std::io::Error) -> EditError {
    EditError::Writer(format!("{}: {e}", path.display()))
}

#[async_trait]
impl EditWriter for FsWriter {
    async fn write(&self, request: WriteRequest) -> EditResult<WriteResponse> {
        let target = request.move_to.as_deref().unwrap_or(&request.absolute);
        match request.op {
            FileOp::Delete => {
                std::fs::remove_file(&request.absolute)
                    .map_err(|e| io_err(&request.absolute, &e))?;
                Ok(WriteResponse::default())
            }
            FileOp::Noop => Ok(WriteResponse::default()),
            FileOp::Create | FileOp::Update => {
                let content = request.content.unwrap_or_default();
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
                }
                std::fs::write(target, &content).map_err(|e| io_err(target, &e))?;
                if request.move_to.is_some() && request.absolute != target {
                    std::fs::remove_file(&request.absolute)
                        .map_err(|e| io_err(&request.absolute, &e))?;
                }
                Ok(WriteResponse {
                    written: content,
                    diagnostics_json: None,
                })
            }
        }
    }
}

/// The session a tool call runs with.
fn session_config(env: &Env) -> SessionConfig {
    SessionConfig {
        mode: env.edit_mode,
        policy: policy(&env.cwd),
        allow_fuzzy: true,
        fuzzy_threshold: 0.95,
        enforce_seen_lines: true,
        max_columns: DISPLAY_MAX_COLUMNS,
        raw_input: false,
    }
}

/// Run one edit call.
///
/// # Errors
/// The engine's model-facing message (parse, locate, apply or write failure).
pub async fn edit(env: &Env, args: &Value) -> Result<AgentToolResult> {
    let mut session = Session::new(session_config(env), env.store.clone());
    session.set_args_json(&args.to_string());
    session.finish();
    recover_missing_paths(&mut session, &env.cwd).await;
    let outcome = session
        .apply(ApplyRequest::default(), &FsWriter)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut result = AgentToolResult::text(outcome.text.clone());
    result.details = edit_details(&outcome);
    Ok(result)
}

/// Answer every missing target the finished payload names with the one file
/// under `cwd` whose path ends with it, so a path relative to another
/// directory still edits (omp's apply-phase recovery). The engine itself never
/// walks: it records misses and reruns once the host answers them.
async fn recover_missing_paths(session: &mut Session, cwd: &Path) {
    let mut answered: HashSet<String> = HashSet::new();
    loop {
        let mut missing = session.apply_recovery_misses();
        missing.retain(|authored| answered.insert(authored.clone()));
        if missing.is_empty() {
            return;
        }
        for authored in missing {
            let answer = recover_one(cwd, &authored).await;
            session.provide_recovery(authored, answer);
        }
    }
}

/// The single file or directory below `cwd` whose path ends with the missing
/// `authored` path (hidden entries included, ignore files honoured, `.git`
/// pruned). `None` when there is no match, more than one, or the search
/// outlives [`RECOVERY_TIMEOUT`]; the walk stops at the second match.
async fn recover_one(cwd: &Path, authored: &str) -> Option<Resolved> {
    let glob = RecoveryGlob::new(authored)?;
    let matches = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&matches);
    let root = cwd.to_path_buf();
    let opts = walk::Options {
        hidden: true,
        gitignore: true,
        skip_git: true,
        ..walk::Options::default()
    };
    let fs = Fs::native();
    let search = walk::collect(&fs, &root, &opts, false, move |_, entry| {
        if !glob.matches(entry.path, entry.name(), entry.file_type) {
            return walk::Action::Skip;
        }
        if seen.fetch_add(1, Ordering::AcqRel) == 0 {
            walk::Action::Continue
        } else {
            walk::Action::Stop
        }
    });
    // Dropping the timed-out future cancels the walk.
    let found = tokio::time::timeout(RECOVERY_TIMEOUT, search)
        .await
        .ok()?
        .ok()?;
    if matches.load(Ordering::Acquire) != 1 {
        return None;
    }
    let display = found.iter().next()?.path_lossy().into_owned();
    Some(Resolved {
        absolute: cwd.join(&display),
        display,
    })
}

/// omp `EditToolDetails`: the numbered `diff` (+ `firstChangedLine`, `op`,
/// `path`) the transcript card renders; several files add `perFileResults`.
fn edit_details(outcome: &pi_edit::session::ApplyOutcome) -> Value {
    let one = |f: &pi_edit::session::FileOutcome| {
        let mut d = serde_json::Map::new();
        d.insert("path".into(), Value::from(f.display.clone()));
        d.insert("diff".into(), Value::from(f.diff.clone()));
        d.insert(
            "op".into(),
            Value::from(match f.op {
                FileOp::Create => "create",
                FileOp::Delete => "delete",
                FileOp::Update | FileOp::Noop => "update",
            }),
        );
        if let Some(line) = f.first_changed_line {
            d.insert("firstChangedLine".into(), Value::from(line));
        }
        if let Some(to) = &f.move_to {
            d.insert("move".into(), Value::from(to.display().to_string()));
        }
        Value::Object(d)
    };
    match outcome.files.as_slice() {
        [] => Value::Null,
        [only] => one(only),
        files => {
            let mut d = one(&files[0]);
            d["perFileResults"] = Value::Array(files.iter().map(one).collect());
            d
        }
    }
}

/// `write`: create or overwrite a file.
///
/// # Errors
/// Missing arguments or filesystem failures.
pub fn write(env: &Env, args: &Value) -> Result<AgentToolResult> {
    let raw = str_arg(args, "path")?;
    let Some(content) = args.get("content").and_then(Value::as_str) else {
        bail!("`content` is required to write {raw}");
    };
    // Route internal URLs through the scheme registry (TS write tool
    // `writeTarget` + `peelWriteSelector`).
    let registry = super::read::url_registry();
    let normalized = registry.normalize(raw);
    if registry.can_handle(&normalized) {
        // `agent://<id>` / `agent://all` writes are peer messages (TS
        // `AgentProtocolHandler.write` → `executeSend`): steer a running eval
        // subagent instead of hitting the read-only output store.
        if let Some(to) = normalized.strip_prefix("agent://") {
            // Delivery receipts are async; `write` itself is a sync tool on a
            // `spawn_blocking` thread (see `tools.rs`), so drive the future to
            // completion on the current runtime (or a throwaway one, e.g. in
            // unit tests without a reactor).
            let delivered = match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle.block_on(super::eval_agents::deliver_message(to, content)),
                Err(_) => tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("write agent:// delivery runtime")
                    .block_on(super::eval_agents::deliver_message(to, content)),
            };
            return delivered
                .map(AgentToolResult::text)
                .map_err(|error| anyhow!(error));
        }
        let (url_path, sel) = registry.split(&normalized);
        if let Some(sel) = sel {
            if !sel.eq_ignore_ascii_case("raw") && !sel.eq_ignore_ascii_case("conflicts") {
                bail!(
                    "write does not accept the trailing selector \":{sel}\" \u{2014} it addresses a whole file. Remove \":{sel}\", or if the filename truly ends with it, percent-encode the \":\" as %3A."
                );
            }
        }
        registry.write(&url_path, content, &super::read::url_ctx(env))?;
        return Ok(AgentToolResult::text(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            url_path
        )));
    }
    let path = resolve_to_cwd(raw, &env.cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    env.store
        .invalidate(&pi_edit::path_policy::canonical_key(&path));
    Ok(AgentToolResult::text(format!(
        "Successfully wrote {} bytes to {}",
        content.len(),
        format_path_relative_to_cwd(&path, &env.cwd, false)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::read;

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

    fn tag_of(read_out: &str) -> String {
        let header = read_out.lines().next().unwrap();
        header
            .split('#')
            .nth(1)
            .unwrap()
            .trim_end_matches(']')
            .to_string()
    }

    #[tokio::test]
    async fn hashline_replace_range_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let env = Env::new(dir.path(), true);
        let out = text_of(&read(&env, &json!({"path": "f.txt"})).unwrap());
        let tag = tag_of(&out);
        let input = format!("[f.txt#{tag}]\nPUT 2.=2:\n+BETA\n+beta2\n");
        let res = edit(&env, &json!({"input": input})).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "alpha\nBETA\nbeta2\ngamma\n"
        );
        let text = text_of(&res);
        // result header carries the new snapshot tag
        assert!(text.starts_with("[f.txt#"), "{text}");
    }

    #[tokio::test]
    async fn hashline_insert_after_and_eof_append() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        let env = Env::new(dir.path(), true);
        let tag = tag_of(&text_of(&read(&env, &json!({"path": "f.txt"})).unwrap()));
        edit(
            &env,
            &json!({"input": format!("[f.txt#{tag}]\nPUT >1:\n+one-and-a-half\n")}),
        )
        .await
        .unwrap();
        let tag = tag_of(&text_of(&read(&env, &json!({"path": "f.txt"})).unwrap()));
        edit(
            &env,
            &json!({"input": format!("[f.txt#{tag}]\nPUT >$:\n+three\n")}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "one\none-and-a-half\ntwo\nthree\n"
        );
    }

    #[tokio::test]
    async fn hashline_cut_deletes_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let env = Env::new(dir.path(), true);
        let tag = tag_of(&text_of(&read(&env, &json!({"path": "f.txt"})).unwrap()));
        edit(
            &env,
            &json!({"input": format!("[f.txt#{tag}]\nCUT 2.=3\n")}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nd\n"
        );
    }

    #[tokio::test]
    async fn stale_tag_and_garbage_are_rejected_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let env = Env::new(dir.path(), true);
        let _ = read(&env, &json!({"path": "f.txt"})).unwrap();
        let err = edit(&env, &json!({"input": "[f.txt#0000]\nPUT 1.=1:\n+x\n"}))
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
        let err = edit(&env, &json!({"input": "this is not hashline"}))
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\n"
        );
    }

    #[tokio::test]
    async fn hashline_requires_lines_to_have_been_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 1..=50 {
            use std::fmt::Write as _;
            let _ = writeln!(body, "l{i}");
        }
        std::fs::write(dir.path().join("f.txt"), body).unwrap();
        let env = Env::new(dir.path(), true);
        let tag = tag_of(&text_of(
            &read(&env, &json!({"path": "f.txt:1-3"})).unwrap(),
        ));
        let err = edit(
            &env,
            &json!({"input": format!("[f.txt#{tag}]\nPUT 40.=40:\n+x\n")}),
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[tokio::test]
    async fn replace_mode_edits_by_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world\n").unwrap();
        let env = Env::with_mode(dir.path(), EditMode::Replace);
        edit(
            &env,
            &json!({"path": "f.txt", "old_string": "world", "new_string": "rust"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "hello rust\n"
        );
    }

    #[tokio::test]
    async fn missing_file_is_recovered_by_unique_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("deep/src")).unwrap();
        std::fs::write(dir.path().join("deep/src/a.ts"), "old\n").unwrap();
        std::fs::write(dir.path().join("deep/src/other.ts"), "keep\n").unwrap();
        let env = Env::with_mode(dir.path(), EditMode::Replace);
        edit(
            &env,
            &json!({"path": "src/a.ts", "old_string": "old", "new_string": "new"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("deep/src/a.ts")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("deep/src/other.ts")).unwrap(),
            "keep\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_suffix_is_not_recovered() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["deep", "other"] {
            std::fs::create_dir_all(dir.path().join(sub).join("src")).unwrap();
            std::fs::write(dir.path().join(sub).join("src/a.ts"), "old\n").unwrap();
        }
        let env = Env::with_mode(dir.path(), EditMode::Replace);
        let err = edit(
            &env,
            &json!({"path": "src/a.ts", "old_string": "old", "new_string": "new"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("src/a.ts"), "{err}");
        for sub in ["deep", "other"] {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(sub).join("src/a.ts")).unwrap(),
                "old\n"
            );
        }
    }

    #[tokio::test]
    async fn over_cap_anchor_without_read_provenance_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // `max_columns: 512` bounds the display: a longer line may have been
        // cut, so an anchor on it cannot be trusted from a stale store.
        let wide = format!("{}\n", "x".repeat(600));
        std::fs::write(dir.path().join("f.txt"), &wide).unwrap();
        let env = Env::new(dir.path(), true);
        let tag = pi_edit::store::file_hash(&wide);
        let err = edit(
            &env,
            &json!({"input": format!("[f.txt#{tag}]\nCUT 1.=1\n")}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("column cap"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            wide
        );
    }

    #[test]
    fn session_config_carries_the_display_column_cap() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::new(dir.path(), true);
        assert_eq!(session_config(&env).max_columns, 512);
    }

    #[test]
    fn write_creates_parents_and_reports_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::new(dir.path(), true);
        let r = write(&env, &json!({"path": "a/b/c.txt", "content": "héllo"})).unwrap();
        assert_eq!(text_of(&r), "Successfully wrote 6 bytes to a/b/c.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "héllo"
        );
    }

    #[test]
    fn write_agent_url_is_a_message_not_a_read_only_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::new(dir.path(), true);
        let err = write(
            &env,
            &json!({"path": "agent://nobody-here", "content": "hi"}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with("Failed: nobody-here is not running."),
            "{err}"
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
