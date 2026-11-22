# 문서 조각 04: ADR 초안 — `session_settled`/`agent_settled` 이원화 해소 및 `pauseSeq` 보류 상태 기계 아키텍처 결정 기록 (doc-04-adr-agent-settled-and-pause-fsm)

## 1. 목적 및 코드 근거 (File & Line)

- 결정 대상 1: 종료 신호 이원화. tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout으로 방출하고, `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 반면 소비자측 `omo-nyang`은 `omo-nyang/src/relay/runtime.ts:59`, `omo-nyang/src/slice-run.ts:71`, `omo-nyang/src/fleet.ts:258`, `omo-nyang/src/agentd-sessions.ts:186`에서 RPC stdout의 `{"type":"agent_settled"}`를 대기하므로, 현 상태는 구조적 교착(정상 종료 후에도 상위 러너가 타임아웃까지 대기)이다.
- 결정 대상 2: 보류·재개 상태 기계. 계약은 `omo-nyang/src/lane-hold.ts:29-76`에 존재한다. `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`, 그리고 `resume_session()`, `reload()`, `get_protocol_info()`.
- 결정 대상 3: RPC 디스패치/에러 직렬화. 진입점은 `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`, 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 응답 프레임은 `crates/omp-cli/src/rpc.rs:203-226` `response()`가 조립하고, `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 동시 직렬화해야 한다.
- 결정 대상 4: 세션 이동 연동. 호스트 실행 플래그는 `omo-nyang/src/relay/move-host.ts:37-39`(`--mode rpc --multi-session --session-runtime in-process`), 해제 계약은 `release_session({sessionId, reason:"move", moveId})` → `{session_path}`이며 `move-operations.ts:104-225`, `session-move-extension.ts:116-139`가 소비한다.
- 결론: (A) `agent_settled`를 정본(canonical) 종료 신호로 승격하고 `session_settled`를 별칭으로 유지하는 이중 방출, (B) `pauseSeq` 단조 증가 + 4상태 FSM 도입, (C) 모든 실패 응답에 안정적 `errorCode` 부여.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 종료 신호 (정본 + 별칭)

stdout 1줄 JSON, `type` 필드로 판별. 동일 settle에 대해 두 프레임은 `settleSeq`를 공유한다.

```json
{"type":"agent_settled","settleSeq":7,"sessionId":"ses_01H...","runId":"run_01H...","reason":"completed","paused":false,"ts":"2026-11-22T00:00:00Z"}
{"type":"session_settled","settleSeq":7,"sessionId":"ses_01H...","runId":"run_01H...","reason":"completed","paused":false,"ts":"2026-11-22T00:00:00Z"}
```

```rust
// crates/omp-cli/src/runtime.rs (신규)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettleReason { Completed, Aborted, Paused, Error, MoveReleased }

#[derive(Clone, Debug, serde::Serialize)]
pub struct SettleFrame {
    #[serde(rename = "type")] pub kind: &'static str, // "agent_settled" | "session_settled"
    #[serde(rename = "settleSeq")] pub settle_seq: u64,
    #[serde(rename = "sessionId")] pub session_id: String,
    #[serde(rename = "runId")] pub run_id: String,
    pub reason: SettleReason,
    pub paused: bool,
    pub ts: String,
}

pub fn emit_settle(rt: &Runtime, reason: SettleReason) -> Result<u64, EmitError>; // settle_seq 반환, 두 프레임 연속 기록
```

`ext.rs:1598`의 확장 호스트 내부 방출도 `emit_settle`을 경유하도록 통일하여, stdout/확장 채널이 동일 `settleSeq`를 관측하게 한다.

### 2.2 보류 FSM (`pauseSeq`)

```rust
// crates/omp-cli/src/pause.rs (신규)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausePhase { Running, PauseRequested, Paused, Resuming }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopIntent { Stop, Abort }

#[derive(Clone, Debug, Default)]
pub struct PauseFsm {
    pub phase: PausePhase,          // 기본 Running
    pub pause_seq: u64,             // PauseRequested 진입 시 +1, 단조 증가
    pub intent: Option<StopIntent>,
    pub capabilities: Vec<String>,  // "interactive_user" 포함 여부로 게이트
    pub role_fixed: bool,           // set_client_info 최초 1회 이후 true
    pub move_id: Option<String>,    // release_session(reason:"move") 기록
}
```

`get_state` 응답 data: `{paused: bool, runPaused: bool, pauseSeq: u64, phase: PausePhase, intent: Option<StopIntent>, moveId: Option<String>}`. `paused = phase ∈ {PauseRequested, Paused}`, `runPaused = phase == Paused`.

### 2.3 에러 코드 (response 프레임)

`CmdError`에 `code: &'static str`을 추가하고 `rpc.rs:203-226` `response()`에서 `error`(사람용)와 `errorCode`(기계용, 동일 값)를 모두 채운다. 최소 집합: `unknown_command`, `stop_requires_interactive_client`, `connection_role_fixed`, `pause_not_active`, `move_in_progress`, `invalid_params`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

### 3.1 `dispatch` 분기 추가 (`crates/omp-cli/src/rpc.rs:593`)

```rust
match name {
  "set_client_info" => {
      let caps: Vec<String> = parse(&cmd, "capabilities")?;
      let mut f = rt.pause.lock();
      if f.role_fixed && caps != f.capabilities { return Err(CmdError::coded("connection_role_fixed", ...)); }
      f.capabilities = caps; f.role_fixed = true;
      Ok(json!({"width": width, "capabilities": f.capabilities}))
  }
  "interrupt" => {
      let intent = StopIntent::parse(&cmd)?; // "stop" | "abort"
      let mut f = rt.pause.lock();
      if !f.capabilities.iter().any(|c| c == "interactive_user") {
          return Err(CmdError::coded("stop_requires_interactive_client", ...));
      }
      if f.phase != PausePhase::Running { return Ok(json!({"pauseSeq": f.pause_seq, "idempotent": true})); }
      f.phase = PausePhase::PauseRequested; f.pause_seq += 1; f.intent = Some(intent);
      rt.request_run_halt(intent); // 런타임 협조적 정지
      Ok(json!({"pauseSeq": f.pause_seq, "phase": "PauseRequested"}))
  }
  "get_state" => Ok(state_json(&rt.pause.lock())),
  "resume_session" => {
      let mut f = rt.pause.lock();
      if f.phase == PausePhase::Running { return Ok(json!({"paused": false, "runPaused": false, "pauseSeq": f.pause_seq})); }
      f.phase = PausePhase::Resuming;
      drop(f);
      rt.emit_event("session_resumed", json!({"pauseSeq": rt.pause.lock().pause_seq}));
      rt.pause.lock().phase = PausePhase::Running;
      Ok(json!({"paused": false, "runPaused": false}))
  }
  "reload" => { rt.reload_settings_skills_extensions()?; Ok(json!({"cancelled": false})) }
  "get_protocol_info" => Ok(json!({"protocolVersion": 2, "serverVersion": env!("CARGO_PKG_VERSION"), "engine": "oh-my-tau", "commands": rt.command_list()})),
  "release_session" => { /* reason=="move" → move_id 기록, emit_settle(MoveReleased), Ok(json!({"session_path": p})) */ }
  "extension_request" => { /* api.rpc.handle(name, handler) 라우팅: "omomeow.move-readiness" | "omo.feedback.prepare" | "omo.feedback.submit" */ }
  other => Err(CmdError::coded("unknown_command", format!("Unknown command: {other}"))), // rpc.rs:1094 대체
}
```

### 3.2 소비자측 정규화 (`omo-nyang`)

```ts
// omo-nyang/src/relay/runtime.ts:59 인근 공통 파서로 추출
function isSettleFrame(f: any): boolean {
  return f?.type === "agent_settled" || f?.type === "session_settled";
}
// 중복 억제: key = `${sessionId}:${settleSeq}`, 이미 본 key면 무시(이중 방출 대응)
```

`slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`은 위 술어를 공유 import하도록 치환한다.

### 3.3 검증 순서

1. `set_client_info` 미호출 상태에서 `interrupt` → `stop_requires_interactive_client`.
2. `set_client_info` 후 capability를 바꿔 재호출 → `connection_role_fixed`.
3. `interrupt` 2회 → 두 번째는 `idempotent:true`, `pauseSeq` 불변.
4. `resume_session` → `session_resumed` 이벤트 1회, `get_state`에서 `paused:false, runPaused:false`.
5. 정상 완료 시 stdout에서 `agent_settled`와 `session_settled`가 동일 `settleSeq`로 연속 관측.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `cargo test -p omp-cli --test rpc_settle` 통과. 테스트는 stdout 캡처로 `agent_settled` 존재, `settleSeq` 동일성, `session_settled` 병존을 단언.
- AC2: `cargo test -p omp-cli --test pause_fsm` 통과. 4상태 전이표와 3개 에러 코드(`stop_requires_interactive_client`, `connection_role_fixed`, `pause_not_active`) 케이스 포함.
- AC3: 수동 스모크 — `omt --mode rpc --multi-session --session-runtime in-process -e <ext>` 기동 후
  `printf '%s\n' '{"id":1,"cmd":"set_client_info","capabilities":["interactive_user"],"width":80}' '{"id":2,"cmd":"interrupt","intent":"stop"}' '{"id":3,"cmd":"get_state"}' '{"id":4,"cmd":"resume_session"}' | omt ...` 실행 시 `id:3` 응답에 `pauseSeq >= 1`, `id:4` 후 `paused:false`.
- AC4: `pnpm --filter omo-nyang test` 통과, 특히 `move-host.ts:61` 파서가 `{success:false, error, errorCode}`를 `errorCode`로 분기하는 단위 테스트.
- AC5: 회귀 방지 grep — `grep -rn '"session_settled"' crates/omp-cli/src` 결과가 `emit_settle` 내부 1곳으로 한정.
- AC6: `cargo clippy -p omp-cli -- -D warnings` 및 `pnpm -C omo-nyang exec tsc --noEmit` 무경고.

## 5. 엣지 케이스 및 실패 복구

- 클라이언트 미선언 `interrupt`: 상태 변경 없이 에러만 반환하고 `pauseSeq
