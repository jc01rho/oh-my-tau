# 구현 조각 02: `crates/omp-cli/src/runtime.rs` 보류 상태 기계(`PauseState`, `pause_seq`) 및 `interrupt`(`intent: stop|abort`) 구현 명세 (impl-02-rpc-interrupt-pause-fsm)

## 1. 목적 및 코드 근거 (File & Line)

- `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`가 모든 RPC 명령의 단일 진입점이다. 본 조각은 여기에 `set_client_info`, `interrupt`, `get_state`, `resume_session` 분기를 추가하고, 미지원 명령 폴백(`rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`)에 도달하지 않도록 한다.
- `crates/omp-cli/src/rpc.rs:203-226` `response()`가 응답 프레임을 조립한다. 실패 시 `{type:"response", id, success:false, error, errorCode, data:{code}}` 형태로 `code`와 `errorCode`를 동시에 직렬화해야 한다. 근거: `omo-nyang/src/relay/move-host.ts:4-7,61`의 `MoveHost` 파서가 `{type, id, success, data?, error?, errorCode?}`만 읽고, 별도 경로에서 `code`를 참조하기 때문이다.
- 보류·재개 계약 원본: `omo-nyang/src/lane-hold.ts:29-76`. 순서는 `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`이며, `interactive_user` 미선언 상태의 stop은 `stop_requires_interactive_client`, capabilities 선언 후 역할 변경은 `connection_role_fixed`로 거부한다.
- 재개 계약: `resume_session()`은 보류를 해제하고 `session_resumed` 이벤트를 발행하며, 이후 `get_state`가 `paused:false, runPaused:false`를 반환해야 한다. `reload()`는 설정·스킬·확장 재로딩 후 `{cancelled:false}`를 반환한다(본 조각에서는 상태 기계와의 상호작용만 규정).
- 종료 신호 정합: `crates/omp-cli/src/runtime.rs:1641`은 현재 `{"type":"session_settled"}`만 stdout에 쓰고 `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`은 `agent_settled`를 대기하므로, 보류 상태 종료 시에도 동일 이벤트를 stdout으로 승격하는 훅을 여기서 정의한다.

## 2. 상세 계약 및 데이터/타입 명세

`crates/omp-cli/src/runtime.rs`에 추가할 타입:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseState { Running, PauseRequested, Paused, Aborting }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptIntent { Stop, Abort }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    pub width: Option<u16>,
    pub capabilities: Vec<String>, // "interactive_user" 포함 여부만 판정에 사용
    pub role: Option<String>,      // 최초 선언값 고정, 이후 변경 거부
}

pub struct PauseMachine {
    pub state: PauseState,
    pub pause_seq: u64,            // 단조 증가, 보류 진입 1회당 +1
    pub intent: Option<InterruptIntent>,
    pub client: ClientInfo,
    pub resume_generation: u64,    // resume_session 1회당 +1
}

#[derive(Debug, Serialize)]
pub struct PauseSnapshot {
    pub paused: bool,
    pub run_paused: bool,
    #[serde(rename = "pauseSeq")]
    pub pause_seq: u64,
    pub intent: Option<InterruptIntent>,
}
```

JSON 페이로드 계약:

```json
{"id":"c1","cmd":"set_client_info","params":{"width":80,"capabilities":["interactive_user"],"role":"interactive"}}
{"id":"c2","cmd":"interrupt","params":{"intent":"stop"}}
{"id":"c3","cmd":"get_state","params":{}}
{"id":"c4","cmd":"resume_session","params":{}}
```

성공 응답: `{"type":"response","id":"c3","success":true,"data":{"paused":true,"runPaused":true,"pauseSeq":1,"intent":"stop"}}`
실패 응답: `{"type":"response","id":"c2","success":false,"error":"stop requires interactive client","errorCode":"stop_requires_interactive_client","data":{"code":"stop_requires_interactive_client"}}`

오류 코드 집합(고정 문자열): `stop_requires_interactive_client`, `connection_role_fixed`, `invalid_intent`, `not_paused`, `interrupt_in_progress`, `pause_seq_overflow`.

상태 전이표:

| 현재 | 입력 | 다음 | 부수 효과 |
|---|---|---|---|
| Running | interrupt(stop), interactive_user 있음 | PauseRequested | pause_seq += 1 |
| Running | interrupt(stop), interactive_user 없음 | Running | 오류 `stop_requires_interactive_client` |
| Running | interrupt(abort) | Aborting | pause_seq += 1, 취소 토큰 발행 |
| PauseRequested | 워커 정지 확인 | Paused | `session_settled` stdout 발행 |
| Aborting | 워커 정지 확인 | Paused | `session_settled` stdout 발행 |
| Paused | resume_session | Running | `session_resumed` 이벤트, resume_generation += 1 |
| Paused | interrupt(*) | Paused | 멱등, pause_seq 불변 |

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`dispatch` 분기 추가( `rpc.rs:593` 함수 내 match 앞부분):

```rust
"set_client_info" => rt.with_pause(|p| {
    let next = ClientInfo::from_params(cmd.get("params"))?;
    if let (Some(old), Some(new)) = (p.client.role.as_deref(), next.role.as_deref()) {
        if old != new { return Err(CmdError::coded("connection_role_fixed")); }
    }
    p.client = next;
    Ok(json!({"ok": true, "role": p.client.role}))
}),
"interrupt" => rt.with_pause(|p| {
    let intent: InterruptIntent = serde_json::from_value(
        cmd.pointer("/params/intent").cloned().unwrap_or(json!("stop")))
        .map_err(|_| CmdError::coded("invalid_intent"))?;
    match (p.state, intent) {
        (PauseState::Paused, _) => return Ok(p.snapshot()), // 멱등
        (_, InterruptIntent::Stop) if !p.client.has("interactive_user")
            => return Err(CmdError::coded("stop_requires_interactive_client")),
        _ => {}
    }
    p.pause_seq = p.pause_seq.checked_add(1).ok_or(CmdError::coded("pause_seq_overflow"))?;
    p.intent = Some(intent);
    p.state = if intent == InterruptIntent::Abort { PauseState::Aborting } else { PauseState::PauseRequested };
    rt.request_worker_stop(intent);           // 기존 취소 토큰 경로 재사용
    Ok(p.snapshot())
}),
"get_state" => Ok(rt.pause_snapshot()),
"resume_session" => {
    let gen = rt.with_pause(|p| {
        if p.state != PauseState::Paused { return Err(CmdError::coded("not_paused")); }
        p.state = PauseState::Running; p.intent = None; p.resume_generation += 1;
        Ok(p.resume_generation)
    })?;
    rt.emit_event("session_resumed", json!({"generation": gen}));
    Ok(json!({"resumed": true, "generation": gen}))
}
```

정지 확인 지점(기존 `runtime.rs:1641` 인근)에서 상태를 `Paused`로 확정하고, `{"type":"session_settled","pauseSeq":N,"intent":"stop"}`을 stdout에 1회 기록한다. `resume_session` 이후에는 새 턴 시작 시 `pause_seq`를 초기화하지 않고 유지하여 `lane-hold.ts:47-60`의 단조성 가정을 보존한다.

검증: 상태 변경은 반드시 `PauseMachine` 단일 뮤텍스 안에서 수행하고, 스냅샷은 복사본으로 반환한다. `has()`는 `capabilities.iter().any(|c| c == "interactive_user")`로 구현한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `interrupt({intent:"stop"})`를 capabilities 없이 호출하면 `success:false`, `errorCode:"stop_requires_interactive_client"`, `data.code` 동일 문자열이 반환된다.
2. `set_client_info`로 `interactive_user` 선언 후 `role`을 다른 값으로 재선언하면 `connection_role_fixed`가 반환되고 기존 role이 유지된다.
3. `interrupt` 성공 직후 `get_state`가 `paused:true, runPaused:true, pauseSeq:1`을 반환하고, 두 번째 `interrupt`는 `pauseSeq`를 증가시키지 않는다.
4. `resume_session` 후 `get_state`가 `paused:false, runPaused:false`이며 `session_resumed` 이벤트가 정확히 1회 발행된다.
5. `reload()`가 `{cancelled:false}`를 반환하고 상태 기계를 `Running`으로 리셋하지 않는다(보류 중 reload는 보류 유지).
6. `cargo test -p omp-cli pause_fsm -- --nocapture`, `cargo test -p omp-cli rpc_interrupt`, `cargo build --release -p omp-cli`, `cargo clippy -p omp-cli -- -D warnings` 전부 통과.
7. `node --test omo-nyang/test/lane-hold.test.ts`(또는 동등 스위트)에서 `set_client_info → interrupt → get_state → resume_session` 시나리오 통과.

## 5. 엣지 케이스 및 실패 복구

- **중복 stop**: `Paused`에서의 재요청은 오류가 아니라 멱등 성공으로 처리하고 `pauseSeq`를 고정한다. 클라이언트 재시도 폭주를 흡수하기 위함이다.
- **abort 후 resume**: `Aborting`은 워커 정지 확인 전까지 `resume_session`을 `not_paused`로 거부한다. 정지 확인 실패(타임아웃) 시 `Paused`로 강제 전이하고 `session_settled`에 `intent:"abort"`를 실어 보낸다.
- **capabilities 철회**: `set_client_info`에서 `interactive_user`를 제거하는 요청은 허용하되, 이미 `Paused`인 경우 상태를 유지하고 이후 stop만 거부한다.
- **pause_seq 오버플로**: `u64::MAX` 도달 시 `pause_seq_overflow`로 거부하고 상태는 변경하지 않는다(부분 적용 금지).
- **클라이언트 연결 종료**: `kind`가 단발 연결이고 보류 중이면 `Paused`를 유지한 채 `session_settled`를 stdout에 남겨 `relay/runtime.ts:59` 감시자가 정상 종료로 판정하게 한다.
- **resume 경쟁**: `resume_session`과 워커 정지 콜백이 동시에 도착하면 `resume_generation` 비교로 늦은 콜백을 폐기하여 `paused:false` 상태가 `Paused`로 되돌아가지 않게 한다.
- **알 수 없는 intent**: 문자열 파싱 실패는 `invalid_intent`로 즉시 반환하며 `rpc.rs:1094`의 `Unknown command` 경로로 흘러가지 않게 한다.
- **응답 직렬화 실패**: `errorCode` 누락 시 `move-host.ts:61` 파서가 성공으로 오판할 수 있으므로, `CmdError::coded` 생성자에서 `errorCode`와 `data.code`를 항상 동일 값으로 채우는 불변식을 테스트로 고정한다.
