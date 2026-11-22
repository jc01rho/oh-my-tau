# 구현 조각 06: `crates/omp-cli/src/rpc.rs`·`omp-session`의 `release_session` 펜스 기록(`.moved-away`) 및 핸들 해제 구현 명세 (impl-06-rpc-release-session)

## 1. 목적 및 코드 근거 (File & Line)

이 조각의 목적은 세션 이동(`move_session`)의 마지막 단계에서 `release_session({sessionId, reason:"move", moveId})` RPC를 수용하고, 대상 세션의 JSONL 기록을 flush/fsync한 뒤 세션 디렉터리에 `.moved-away` 펜스를 원자적으로 기록하고, 런타임의 세션 핸들을 해제하여 이후 쓰기를 차단하는 것이다. 성공 응답은 `{session_path}`를 반환해야 하며, 호출측 `omo-nyang/src/relay/move-operations.ts:104-225`가 이를 이동 완료 판정에 사용한다.

- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령 분기: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 현재 `release_session`은 이 분기로 빠지므로 새 match arm이 필요하다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226`의 `response()`. 실패 시 `error`와 `errorCode`를 함께 직렬화해야 하며, `MoveHost`는 `omo-nyang/src/relay/move-host.ts:4-7, 61`에서 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다.
- 호스트 실행 계약: `omo-nyang/src/relay/move-host.ts:37-39`는 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 호스트를 띄운다. 따라서 `release_session`은 해당 모드의 in-process 세션 런타임에서 동작해야 한다.
- 이동 오퍼레이션 계약: `omo-nyang/src/relay/move-operations.ts:104-225`의 `prepare|export|import|status|finish|cancel` 흐름 중 `finish` 직전 또는 `cancel` 복구 경로에서 `release_session`이 호출된다. `reason:"move"`일 때만 `.moved-away` 펜스를 기록한다.
- 확장 이벤트와의 분리: `omo-nyang/src/session-move-extension.ts:116-139`는 `session_move_export`/`session_move_import` 이벤트의 `event={dir, signal}`과 `ctx.sessionManager.getSessionFile()`를 사용한다. `release_session`은 이 이벤트를 대체하지 않으며, 펜스 기록과 핸들 해제만 담당한다.
- 세션 저장 형식: `crates/omp-session/src/entries.rs`의 omp JSONL v3(256B title slot + camelCase entries). 펜스는 JSONL 내부에 쓰지 않고 별도 파일로 두어 v3 레코드 무결성을 유지한다.
- 종료 신호와의 관계: `runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 보낸다. `release_session` 성공은 `agent_settled`를 대신하지 않으며, 핸들 해제 후에도 기존 종료 경로를 변경하지 않는다.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(JSON Lines, 1줄 1객체):

```json
{"type":"request","id":"req-42","command":"release_session","params":{"sessionId":"sess_01J...","reason":"move","moveId":"mv_01J..."}}
```

성공 응답:

```json
{"type":"response","id":"req-42","success":true,"data":{"session_path":"/home/u/.omp/sessions/sess_01J....jsonl"}}
```

실패 응답:

```json
{"type":"response","id":"req-42","success":false,"error":"session is busy","errorCode":"session_busy"}
```

Rust 파라미터 타입(`crates/omp-cli/src/rpc.rs` 내부 또는 `crates/omp-cli/src/rpc/release.rs`):

```rust
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseSessionParams {
    pub session_id: String,
    pub reason: ReleaseReason,
    #[serde(default)]
    pub move_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason { Move, Shutdown, User }

#[derive(Debug, serde::Serialize)]
pub struct ReleaseSessionData { pub session_path: String }
```

`omp-session` 펜스 타입(`crates/omp-session/src/fence.rs` 신규):

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveFence {
    pub version: u32,          // 1
    pub session_id: String,
    pub move_id: String,
    pub reason: String,        // "move"
    pub session_path: String,  // canonical absolute .jsonl path
    pub released_at: String,   // RFC3339 UTC
    pub pid: u32,
}

pub fn write_moved_away(session_dir: &std::path::Path, fence: &MoveFence)
    -> Result<std::path::PathBuf, omp_session::SessionError>;
pub fn read_moved_away(session_dir: &std::path::Path)
    -> Result<Option<MoveFence>, omp_session::SessionError>;
pub fn is_fenced(session_path: &std::path::Path) -> bool;
```

펜스 파일 경로는 `<session_dir>/.moved-away`로 고정한다. 내용은 위 `MoveFence`의 단일 JSON 객체이며, 파일명은 정확히 `.moved-away`여야 한다. 임시 파일은 `.moved-away.tmp.<pid>.<counter>`를 사용하고 `rename(2)`로 교체한다. `session_path`는 `std::fs::canonicalize` 결과를 사용하되 세션 루트 밖으로 벗어나면 `session_path_invalid`로 거부한다.

에러 코드 표: `invalid_params`, `session_not_found`, `session_busy`, `session_already_released`, `invalid_move_id`, `session_flush_failed`, `fence_write_failed`, `session_path_invalid`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`rpc.rs:1094`의 `other` arm 직전에 다음 arm을 추가한다.

```rust
"release_session" => crate::rpc::release::handle(rt, params).await,
```

`crates/omp-cli/src/rpc/release.rs` 핵심 로직:

```rust
pub async fn handle(rt: &Arc<Runtime>, params: &Value) -> CmdResult {
    let p: ReleaseSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| CmdError::with_code("invalid_params", e.to_string()))?;
    if p.reason == ReleaseReason::Move
        && p.move_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return Err(CmdError::with_code("invalid_move_id", "moveId is required for reason=move"));
    }
    let handle = rt.sessions().get(&p.session_id)
        .ok_or_else(|| CmdError::with_code("session_not_found", "unknown sessionId"))?;

    let _guard = handle.write_gate().lock().await; // append/release 직렬화
    if let Some(existing) = omp_session::fence::read_moved_away(handle.dir())? {
        if p.move_id.as_deref() == Some(existing.move_id.as_str()) {
            return Ok(json!({"session_path": existing.session_path})); // 멱등 성공
        }
        return Err(CmdError::with_code("session_already_released", "fence exists"));
    }
    if handle.is_running() && !handle
