//! RPC `move_session`: the six steps the OmOMeow gateway drives to move a session between machines
//! (`relay/move-operations.ts`): `prepare`, `status` and `cancel` on the receiver, `export` and
//! `finish` on the sender, `import` on the receiver. File custody lives in
//! [`omp_session::move_bundle`]; this layer checks the wire command, reads repository identities,
//! and runs the file work off the RPC task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omp_session::move_bundle::{
    self as bundle, DeclineReason, ExportOptions, ImportOptions, MoveError, MoveLedger, MoveRecord,
    MoveStatus, RepositoryIdentity,
};
use serde_json::{json, Value};

use crate::runtime::{CmdError, CmdResult};

const SESSION_OPEN_HERE: &str = "session_open_here";
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The engine's session folder for a checkout.
type SessionsDir = Arc<dyn Fn(&Path) -> Option<PathBuf> + Send + Sync>;

/// Where moves keep their engine-side files.
#[derive(Clone)]
pub struct MoveRoots {
    /// `ledger/` (the receiver's records) and `outgoing/<moveId>` (the sender's bundles).
    pub root: PathBuf,
    /// Where an import lands for a checkout.
    pub sessions_dir: SessionsDir,
}

impl MoveRoots {
    /// `<agent dir>/session-moves`, and the `~/.omp-rs/sessions/<cwd>` folders sessions live in.
    pub fn engine() -> Self {
        Self {
            root: omp_providers::AgentDir::default().0.join("session-moves"),
            sessions_dir: Arc::new(crate::persist::sessions_dir),
        }
    }

    fn ledger(&self) -> MoveLedger {
        MoveLedger::new(self.root.join("ledger"))
    }

    fn outgoing(&self, move_id: &str) -> PathBuf {
        self.root.join("outgoing").join(move_id)
    }
}

fn refusal(error: MoveError) -> CmdError {
    CmdError::coded(error.to_string(), error.code)
}

fn invalid(detail: impl Into<String>) -> MoveError {
    MoveError::new(MoveError::INVALID_REQUEST, detail)
}

fn text<'a>(cmd: &'a Value, key: &str) -> Result<&'a str, MoveError> {
    cmd.get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| invalid(format!("{key} must be a non-empty string")))
}

/// senpi `validateMoveRequest`: the shape every op needs before anything is touched.
fn validate(cmd: &Value) -> Result<&str, MoveError> {
    let op = cmd.get("op").and_then(Value::as_str).unwrap_or_default();
    if op == "import" {
        let (dir, cwd) = (text(cmd, "bundleDir")?, text(cmd, "cwd")?);
        if !Path::new(dir).is_absolute() || !Path::new(cwd).is_absolute() {
            return Err(invalid("import needs bundleDir and cwd as absolute paths."));
        }
        if cmd.get("from").is_some_and(|v| !v.is_string()) {
            return Err(invalid("from must be a string."));
        }
        return Ok(op);
    }
    if !cmd
        .get("moveId")
        .and_then(Value::as_str)
        .is_some_and(bundle::is_plain_id)
    {
        return Err(invalid(
            "moveId must be a plain id: a letter or digit, then letters, digits, '.', '_' or '-', at most 128 characters.",
        ));
    }
    match op {
        "prepare" | "cancel" => {}
        "status" => {
            if cmd.get("sessionPath").is_some() {
                text(cmd, "sessionPath")?;
            }
        }
        "export" => {
            text(cmd, "sessionPath")?;
            text(cmd, "target")?;
            let dropped = cmd.get("droppedUserMessages");
            if dropped.is_some_and(|v| {
                v.as_array()
                    .is_none_or(|items| items.iter().any(|i| !i.is_string()))
            }) {
                return Err(invalid("droppedUserMessages must contain strings."));
            }
        }
        "finish" => {
            text(cmd, "sessionPath")?;
            if MoveStatus::parse(&cmd["answer"]).is_none() {
                return Err(invalid("finish needs the receiver's answer from status."));
            }
        }
        _ => return Err(invalid("invalid move operation")),
    }
    Ok(op)
}

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A session this process still hosts cannot be exported or settled under it.
fn closed(hosted: &[PathBuf], path: &Path) -> Result<(), MoveError> {
    if hosted.iter().any(|h| real(h) == path) {
        Err(MoveError::new(
            SESSION_OPEN_HERE,
            "close the session here before moving it",
        ))
    } else {
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, MoveError> + Send + 'static,
) -> Result<T, MoveError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| MoveError::new(MoveError::FAILED, e.to_string()))?
}

async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(key);
    }
    let out = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository `dir` is a checkout of (`None` outside one).
async fn repository_identity(dir: &Path) -> Option<RepositoryIdentity> {
    let roots = git(dir, &["rev-list", "--max-parents=0", "HEAD"])
        .await
        .unwrap_or_default();
    let origin = git(dir, &["config", "--get", "remote.origin.url"]).await;
    bundle::repository_identity(&roots, origin.as_deref())
}

/// Answer one `move_session` command. `hosted`: the session files this process holds open.
pub async fn handle(cmd: &Value, hosted: Vec<PathBuf>, roots: MoveRoots) -> CmdResult {
    let op = validate(cmd).map_err(refusal)?;
    let move_id = cmd["moveId"].as_str().unwrap_or_default().to_owned();
    let data = match op {
        "prepare" => {
            blocking(move || roots.ledger().prepare(&move_id).map(MoveRecord::to_wire)).await
        }
        "cancel" => {
            blocking(move || {
                roots
                    .ledger()
                    .decline(&move_id, DeclineReason::Cancelled)
                    .map(MoveRecord::to_wire)
            })
            .await
        }
        "status" => {
            let file = cmd
                .get("sessionPath")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            blocking(move || {
                bundle::answer_move_status(&roots.ledger(), &move_id, file.as_deref())
                    .map(MoveStatus::to_wire)
            })
            .await
        }
        "export" => export(cmd, move_id, &hosted, roots).await,
        "finish" => {
            let path = real(Path::new(text(cmd, "sessionPath").map_err(refusal)?));
            closed(&hosted, &path).map_err(refusal)?;
            let answer = MoveStatus::parse(&cmd["answer"])
                .ok_or_else(|| refusal(invalid("invalid move answer")))?;
            blocking(move || {
                bundle::finish_move(&path, &roots.outgoing(&move_id), &move_id, &answer)
                    .map(|ownership| json!({"ownership": ownership.wire()}))
            })
            .await
        }
        _ => import(cmd, roots).await,
    };
    data.map(Some).map_err(refusal)
}

async fn export(
    cmd: &Value,
    move_id: String,
    hosted: &[PathBuf],
    roots: MoveRoots,
) -> Result<Value, MoveError> {
    let path = real(Path::new(text(cmd, "sessionPath")?));
    closed(hosted, &path)?;
    let dropped: Vec<String> = cmd
        .get("droppedUserMessages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let header = {
        let path = path.clone();
        blocking(move || bundle::read_header(&path)).await?
    };
    let cwd = PathBuf::from(
        header
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let repository = repository_identity(&cwd).await;
    let bundle_dir = roots.outgoing(&move_id);
    let out = bundle_dir.clone();
    let manifest = blocking(move || {
        bundle::export_session(&ExportOptions {
            session_file: &path,
            move_id: &move_id,
            dropped_user_messages: &dropped,
            bundle_dir: &out,
            repository,
        })
    })
    .await?;
    let files = 1
        + manifest.sidecars.len()
        + manifest
            .payloads
            .iter()
            .map(|p| p.files.len())
            .sum::<usize>();
    Ok(json!({
        "move_id": manifest.move_id,
        "session_id": manifest.session_id,
        "bundle_dir": bundle_dir,
        "files": files,
    }))
}

async fn import(cmd: &Value, roots: MoveRoots) -> Result<Value, MoveError> {
    let bundle_dir = PathBuf::from(text(cmd, "bundleDir")?);
    let cwd = real(Path::new(text(cmd, "cwd")?));
    let from = cmd
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let session_dir = (roots.sessions_dir)(&cwd)
        .ok_or_else(|| MoveError::new(MoveError::FAILED, "HOME is not set"))?;
    let target_repository = repository_identity(&cwd).await;
    blocking(move || {
        let landed = bundle::import_session(&ImportOptions {
            bundle_dir: &bundle_dir,
            target_cwd: &cwd,
            session_dir: &session_dir,
            from: &from,
            ledger: &roots.ledger(),
            target_repository,
        })?;
        let header = bundle::read_header(&landed)?;
        Ok(json!({"session_id": header.get("id"), "session_path": landed}))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use omp_session::move_bundle::{append_custom, SESSION_RELEASED};

    /// One machine: its engine move root and its session store, under `home`.
    fn machine(home: &Path) -> MoveRoots {
        let sessions = home.join("sessions");
        MoveRoots {
            root: home.join("session-moves"),
            sessions_dir: Arc::new(move |cwd: &Path| {
                Some(sessions.join(cwd.display().to_string().replace('/', "-")))
            }),
        }
    }

    /// A checkout with an origin: both machines read the same repository identity from it.
    fn checkout(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "git@github.com:jc01rho/oh-my-tau.git",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        real(dir)
    }

    fn session(roots: &MoveRoots, cwd: &Path, id: &str) -> PathBuf {
        let dir = (roots.sessions_dir)(cwd).unwrap();
        let mut writer = omp_session::SessionWriter::create(
            &dir,
            &cwd.display().to_string(),
            id,
            "2026-11-23T00:00:00.000Z",
        )
        .unwrap();
        writer
            .append_entry(
                "custom",
                json!({"customType": "note", "data": {"n": 1}}),
                "2026-11-23T00:00:01.000Z",
            )
            .unwrap();
        writer.path().to_path_buf()
    }

    async fn call(cmd: Value, hosted: &[PathBuf], roots: &MoveRoots) -> Result<Value, CmdError> {
        handle(&cmd, hosted.to_vec(), roots.clone())
            .await
            .map(|data| data.unwrap_or(Value::Null))
    }

    fn code(result: Result<Value, CmdError>) -> &'static str {
        result.expect_err("refused").code.expect("coded")
    }

    #[tokio::test]
    async fn malformed_commands_are_refused_before_anything_is_touched() {
        let home = tempfile::tempdir().unwrap();
        let roots = machine(home.path());
        for cmd in [
            json!({"op": "prepare"}),
            json!({"op": "prepare", "moveId": "../x"}),
            json!({"op": "teleport", "moveId": "m1"}),
            json!({"op": "export", "moveId": "m1", "sessionPath": "/s.jsonl"}),
            json!({"op": "export", "moveId": "m1", "sessionPath": "/s.jsonl", "target": "b", "droppedUserMessages": [1]}),
            json!({"op": "finish", "moveId": "m1", "sessionPath": "/s.jsonl", "answer": {"state": "ready"}}),
            json!({"op": "finish", "moveId": "m1", "sessionPath": "/s.jsonl", "answer": {"state": "declined", "reason": "typo"}}),
            json!({"op": "import", "bundleDir": "relative", "cwd": "/repo"}),
            json!({"op": "import", "bundleDir": "/b", "cwd": "/repo", "from": 7}),
        ] {
            assert_eq!(
                code(call(cmd.clone(), &[], &roots).await),
                "invalid_move_request",
                "{cmd}"
            );
        }
        assert!(!roots.root.exists());
    }

    #[tokio::test]
    async fn a_released_session_moves_to_another_machine_and_is_marked_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let (desk, laptop) = (
            machine(&tmp.path().join("desk")),
            machine(&tmp.path().join("laptop")),
        );
        let repo_desk = checkout(&tmp.path().join("desk-repo"));
        let repo_laptop = checkout(&tmp.path().join("laptop-repo"));
        let file = session(&desk, &repo_desk, "0193abcd-0000-7000-8000-000000000001");
        let session_path = file.display().to_string();
        // The release the gateway runs before `export`, as `release_session` seals the file.
        let export = json!({"op": "export", "moveId": "m1", "sessionPath": session_path, "target": "laptop",
            "droppedUserMessages": ["still owed"]});
        assert_eq!(
            code(call(export.clone(), &[], &desk).await),
            "move_not_released"
        );
        append_custom(
            &file,
            SESSION_RELEASED,
            json!({"reason": "move", "move_id": "m1", "target": "laptop",
            "session_id": "0193abcd-0000-7000-8000-000000000001"}),
        )
        .unwrap();
        assert_eq!(
            code(call(export.clone(), std::slice::from_ref(&file), &desk).await),
            SESSION_OPEN_HERE
        );
        let exported = call(export, &[], &desk).await.unwrap();
        assert_eq!(exported["move_id"], "m1");
        assert_eq!(exported["files"], 1);
        let bundle = PathBuf::from(exported["bundle_dir"].as_str().unwrap());
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(bundle.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["droppedUserMessages"], json!(["still owed"]));
        assert_eq!(
            manifest["repository"]["originUrl"],
            "github.com/jc01rho/oh-my-tau"
        );

        // The gateway carries the bundle; the receiver readies, imports and answers.
        let carried = tmp.path().join("carried/engine");
        std::fs::create_dir_all(carried.parent().unwrap()).unwrap();
        std::fs::rename(&bundle, &carried).unwrap();
        let ready = call(json!({"op": "prepare", "moveId": "m1"}), &[], &laptop)
            .await
            .unwrap();
        assert_eq!(ready["state"], "ready");
        let imported = call(
            json!({"op": "import", "bundleDir": carried, "cwd": repo_laptop, "from": "desk"}),
            &[],
            &laptop,
        )
        .await
        .unwrap();
        assert_eq!(
            imported["session_id"],
            "0193abcd-0000-7000-8000-000000000001"
        );
        let landed = imported["session_path"].as_str().unwrap().to_owned();
        let header = bundle::read_header(Path::new(&landed)).unwrap();
        assert_eq!(header["cwd"], repo_laptop.display().to_string());
        let answer = call(
            json!({"op": "status", "moveId": "m1", "sessionPath": landed}),
            &[],
            &laptop,
        )
        .await
        .unwrap();
        assert_eq!(answer, json!({"state": "accepted"}));

        let finish =
            json!({"op": "finish", "moveId": "m1", "sessionPath": session_path, "answer": answer});
        assert_eq!(
            call(finish, &[], &desk).await.unwrap(),
            json!({"ownership": "moved"})
        );
        assert!(!desk.root.join("outgoing/m1").exists());
    }

    #[tokio::test]
    async fn a_cancelled_move_is_declined_and_the_sender_takes_its_session_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (desk, laptop) = (
            machine(&tmp.path().join("desk")),
            machine(&tmp.path().join("laptop")),
        );
        call(json!({"op": "prepare", "moveId": "m2"}), &[], &laptop)
            .await
            .unwrap();
        let cancelled = call(json!({"op": "cancel", "moveId": "m2"}), &[], &laptop)
            .await
            .unwrap();
        assert_eq!(
            cancelled,
            json!({"state": "declined", "reason": "cancelled"})
        );
        let answer = call(json!({"op": "status", "moveId": "m2"}), &[], &laptop)
            .await
            .unwrap();
        assert_eq!(answer, cancelled);
        assert_eq!(
            code(call(json!({"op": "prepare", "moveId": "m2"}), &[], &laptop).await),
            "move_fenced"
        );
        // Asking about a move this machine never readied fences it before answering.
        let unknown = call(json!({"op": "status", "moveId": "m9"}), &[], &laptop)
            .await
            .unwrap();
        assert_eq!(unknown, json!({"state": "declined", "reason": "no_record"}));

        let repo = checkout(&tmp.path().join("repo"));
        let file = session(&desk, &repo, "0193abcd-0000-7000-8000-000000000002");
        append_custom(
            &file,
            SESSION_RELEASED,
            json!({"reason": "move", "move_id": "m2", "target": "laptop",
            "session_id": "0193abcd-0000-7000-8000-000000000002"}),
        )
        .unwrap();
        let finish = json!({"op": "finish", "moveId": "m2", "sessionPath": file, "answer": answer});
        assert_eq!(
            call(finish, &[], &desk).await.unwrap(),
            json!({"ownership": "writable"})
        );
    }
}
