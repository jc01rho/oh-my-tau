# 구현 조각 05: `crates/omp-cli/src/rpc.rs`·`omp-session`의 `move_session`(`prepare`/`export`/`import`/`status`/`finish`/`cancel`) 구현 명세 (impl-05-rpc-move-session-ops)

## 1. 목적 및 코드 근거 (File & Line)

`omo-nyang`의 세션 이동 호스트는 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 tau를 띄우고(`omo-nyang/src/relay/move-host.ts:37-39`), 확장이 아닌 RPC 명령 `move_session`을 `op`별로 호출한다(`omo-nyang/src/relay/move-operations.ts:104-225`). 현재 tau의 RPC 디스패치(`crates/omp-cli/src/rpc.rs:593 async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`)에는 해당 명령이 없어 `crates/omp-cli/src/rpc.rs:1094 other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어진다. 본 조각은 이 갈래를 제거하고 6개 op를 원자적으로 구현한다.

- 응답 프레임은 `crates/omp-cli/src/rpc.rs:203-226 response()`가 조립하며, 클라이언트 `MoveHost`(`omo-nyang/src/relay/move-host.ts:4-7, 61`)는 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다. 따라서 실패 시 `code`(문자열)와 `errorCode`(기계 판독용)를 **동시에** 직렬화해야 한다.
- 이동 확장 계약: `omo-nyang/src/relay/session-move-extension.ts:116-139`는 `event={dir, signal}`과 `ctx.sessionManager.getSessionFile()`를 사용하고, `api.rpc.handle(name, handler)`로 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`을 라우팅한다. RPC `extension_request({sessionId?, name, data?})`(`rpc.rs` 내 확장 브리지)와의 응답 스키마를 맞춰야 한다.
- 보류·재개 계약(`omo-nyang/src/lane-hold.ts:29-76`): `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `import`/`finish`는 `paused`/`runPaused`가 참인 구간에서만 허용하고, 완료 시 `resume_session()`으로 `session_resumed` 이벤트를 발행해 `paused:false, runPaused:false`를 만든다.
- 세션 파일 포맷: `crates/omp-session/src/entries.rs`의 omp JSONL v3(256B title slot + camelCase entries). `omo-nyang/src/session-patterns.ts:47-76`이 각 줄에서 `{timestamp, message:{role:"assistant"|"toolResult", content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}`를 읽으므로 export/import는 라인 단위 불변 복사를 보장해야 한다.
- `release_session({sessionId, reason:"move", moveId}) → {session_path}`(`move-host.ts` 계약)는 `export` 성공 직후에만 호출한다.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(클라이언트 → tau):
```json
{"type":"request","id":"r-17","command":"move_session","params":{"op":"prepare","moveId":"mv_01J...","sessionPath":"/h/.omp/sessions/s-9.jsonl","bundleDir":"/h/.omo/moves/mv_01J...","cwd":"/work/repo","from":"fleet-small","answer":null}}
```
op별 `data` 응답 스키마:

| op | 성공 `data` |
|---|---|
| `prepare` | `{moveId, sessionPath, bundleDir, phase:"prepared", entries, bytes, sessionId, manifestHash:"sha256:..."}` |
| `export` | `{moveId, bundleDir, archivePath, fileCount, bytes, checksum:"sha256:...", released:{session_path}}` |
| `import` | `{moveId, sessionPath, sessionId, importedEntries, bytes, checksum, phase:"imported"}` |
| `status` | `{moveId, phase, sessionPath, bundleDir, updatedAt, error?}` |
| `finish` | `{moveId, phase:"finished", sessionPath, resumed:true}` |
| `cancel` | `{moveId, phase:"cancelled", cleaned:true}` |

Rust 타입(`crates/omp-cli/src/rpc.rs` 상단, `omp-session` 재사용):
```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MoveOp { Prepare, Export, Import, Status, Finish, Cancel }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveSessionParams {
    pub op: MoveOp,
    pub move_id: Option<String>,      // op != prepare 이면 필수
    pub session_path: Option<PathBuf>,
    pub bundle_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub from: Option<String>,         // fleet 프로필 이름, 감사 로그용
    pub answer: Option<serde_json::Value>,
}

pub struct MoveEntry { pub move_id: String, pub phase: MovePhase,
    pub session_path: PathBuf, pub bundle_dir: PathBuf,
    pub manifest_hash: String, pub updated_at: u64 }
pub struct MoveRegistry { inner: tokio::sync::Mutex<HashMap<String, Arc<Mutex<MoveEntry>>>> }
```
`MovePhase` 전이: `prepared → exported → imported → finished`, 임의 지점에서 `cancelled`(terminal). `exported → imported` 외 전이는 `move_session_state_conflict`로 거부한다.

`CmdError`는 현재 메시지만 받으므로 `crates/omp-cli/src/rpc.rs`에 `pub fn with_code(msg: impl Into<String>, code: &'static str) -> Self`를 추가하고 `rpc.rs:203-226 response()`가 `error`와 `errorCode`를 함께 채우도록 확장한다. 오류 코드 집합: `move_session_invalid_op`, `move_session_missing_field`, `move_session_unknown_id`, `move_session_state_conflict`, `move_session_target_exists`, `move_session_checksum_mismatch`, `move_session_io_error`, `move_session_busy`, `move_session_not_paused`.

`get_protocol_info()`(`lane-hold.ts:29-76` 계약, `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`)의 `commands` 배열에 `"move_session"`, `"release_session"`, `"extension_request"`, `"resume_session"`을 추가한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`rpc.rs:1094` 직전에 디스패치 암을 삽입한다:
```rust
"move_session" => handle_move_session(rt, cmd).await,
"release_session" => handle_release_session(rt, cmd).await,
```
핸들러 개요:
```rust
async fn handle_move_session(rt: &Arc<Runtime>, cmd: &Value) -> CmdResult {
    let p: MoveSessionParams = serde_json::from_value(cmd.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| CmdError::with_code(e.to_string(), "move_session_missing_field"))?;
    let moves = rt.session.moves();               // Arc<MoveRegistry>
    match p.op {
        MoveOp::Prepare => { let id = p.move_id.clone().unwrap_or_else(new_move_id); // "mv_" + ulid
            let sp = canon_abs(p.session_path.ok_or_else(|| missing("sessionPath"))?)?;
            let bd = canon_abs(p.bundle_dir.ok_or_else(|| missing("bundleDir"))?)?;
            let m = omp_session::move::prepare(&sp, &bd).await?;   // manifest.json 원자 기록(tmp+rename)
            moves.insert(MoveEntry { move_id: id.clone(), phase: Prepared, session_path: sp, bundle_dir: bd,
                manifest_hash: m.hash.clone(), updated_at: now() }).await;
            Ok(json!({"moveId": id, "phase":"prepared", "entries": m.entries, "bytes": m.bytes,
                      "sessionId": m.session_id, "manifestHash": m.hash})) }
        MoveOp::Export => { let e = moves.lock(&p.move_id)?.ensure(Prepared, Exported)?;
            let out = omp_session::move::export(&e).await?;        // tar + sha256, 검증 후 phase 갱신
            let rel = rt.release_session(&e.session_path, "move", &e.move_id).await?;  // {session_path}
            rt.emit(json!({"type":"event","event":"session_move_export","data":{"dir": e.bundle_dir, "signal":"export"}})).await;
            Ok(json!({"moveId": e.move_id, "archivePath": out.path, "fileCount": out.files,
                      "bytes": out.bytes, "checksum": out
