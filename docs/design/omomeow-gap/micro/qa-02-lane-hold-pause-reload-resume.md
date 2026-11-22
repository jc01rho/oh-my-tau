# QA 시나리오 02: 슬랙 `/pause` → `lane-hold.ts` 보류(`interrupt:stop`) → `reload` → `resume_session` 재개 E2E 검증 (qa-02-lane-hold-pause-reload-resume)

## 1. 목적 및 코드 근거 (File & Line)

슬랙 `/pause` 명령이 게이트웨이를 거쳐 oh-my-tau RPC로 도달했을 때, `lane-hold.ts`의 보류 계약(`set_client_info` → `interrupt({intent:"stop"})` → `get_state`)이 성립하고, 이어지는 `reload()`가 보류 상태를 깨지 않으며, 최종 `resume_session()`이 `paused:false, runPaused:false`로 복귀시키는 전 구간을 하나의 E2E 시나리오로 고정한다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. `cmd["cmd"]` 문자열로 분기하며, 본 시나리오가 요구하는 5개 명령(`set_client_info`, `interrupt`, `get_state`, `reload`, `resume_session`)과 `get_protocol_info`가 모두 여기 등록되어야 한다.
- 미지원 명령 처리: `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 새 명령을 추가하지 않으면 이 arm으로 떨어져 시나리오가 즉시 실패한다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 만든다. 실패 시 `error`와 `errorCode`를 동시에 직렬화해야 클라이언트가 코드 기반 분기를 할 수 있다.
- 클라이언트 파싱 계약: `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 `errorCode` 누락은 `stop_requires_interactive_client` / `connection_role_fixed` 판별 불가로 이어진다.
- 보류·재개 상태 기계: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info` → `interrupt` → `get_state` 순서와 `paused`, `runPaused`, `pauseSeq` 필드 정의, `session_resumed` 이벤트 발행 지점.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(클라이언트 → RPC stdout/stdin):

```json
{"type":"request","id":"qa02-1","cmd":"set_client_info","args":{"width":80,"capabilities":["interactive_user"]}}
```

응답 프레임(`rpc.rs:203-226`):

```json
{"type":"response","id":"qa02-1","success":true,"data":{}}
```

실패 프레임:

```json
{"type":"response","id":"qa02-2","success":false,"error":"stop requires an interactive client","errorCode":"stop_requires_interactive_client"}
```

Rust 측 타입 명세(기존 시그니처 유지, 신규 arm만 추가):

- `type CmdResult = Result<Value, CmdError>;`
- `struct CmdError { code: &'static str, message: String }` — `response()`가 `errorCode: err.code`, `error: err.message`로 매핑.
- `dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` 내 신규 분기 6개: `"set_client_info" | "interrupt" | "get_state" | "get_protocol_info" | "reload" | "resume_session"`.

명령별 `data` 스키마:

- `set_client_info` → `{}` (성공). `capabilities`에 `"interactive_user"`가 없으면 `errorCode:"stop_requires_interactive_client"`를 반환할 준비만 하고, 이 호출 자체는 성공해야 한다. 이미 역할이 고정된 커넥션에서 다른 역할을 선언하면 `errorCode:"connection_role_fixed"`.
- `interrupt` args `{"intent":"stop"}` 또는 `{"intent":"abort"}` → `{"accepted":true,"intent":"stop","pauseSeq":<u64>}`. `intent`가 두 값이 아니면 `errorCode:"invalid_intent"`.
- `get_state` → `{"paused":true,"runPaused":true,"pauseSeq":<u64>}` (보류 중), 재개 후 `{"paused":false,"runPaused":false,"pauseSeq":<u64>}`.
- `get_protocol_info` → `{"protocolVersion":2,"serverVersion":env!("CARGO_PKG_VERSION"),"engine":"oh-my-tau","commands":["set_client_info","interrupt","get_state","get_protocol_info","reload","resume_session", ...]}`.
- `reload` → `{"cancelled":false}`. 설정·스킬·확장 재로딩을 수행하되 보류 상태(`paused`)를 변경하지 않는다.
- `resume_session` → `{"resumed":true}` + `session_resumed` 이벤트 1회 발행.

`pauseSeq`는 단조 증가 `u64`로, `interrupt` 성공 시 +1, `resume_session` 성공 시 증가하지 않는다(재개는 시퀀스를 소비하지 않음). 이 불변식이 `lane-hold.ts:29-76`의 재개 판정과 일치해야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust: `dispatch`에 arm 추가(기존 `other =>` arm 앞, `rpc.rs:1094` 이전).

```rust
"set_client_info" => {
    let caps = cmd["args"]["capabilities"].as_array().cloned().unwrap_or_default();
    rt.lane.set_client_capabilities(caps)?;   // Err => CmdError{code:"connection_role_fixed"}
    Ok(json!({}))
}
"interrupt" => {
    let intent = cmd["args"]["intent"].as_str().unwrap_or("");
    match intent {
        "stop" | "abort" => Ok(json!({"accepted":true,"intent":intent,"pauseSeq":rt.lane.pause(intent)?})),
        _ => Err(CmdError::new_code("invalid_intent", "intent must be stop|abort")),
    }
}
"resume_session" => { rt.lane.resume()?; rt.events.emit("session_resumed", json!({})); Ok(json!({"resumed":true})) }
```

TS 드라이버(테스트 하네스, `omo-nyang` 측):

```ts
await rpc("set_client_info", { width: 80, capabilities: ["interactive_user"] });
const i = await rpc("interrupt", { intent: "stop" });
assert.equal(i.success, true);
let s = await rpc("get_state", {});
assert.deepEqual([s.data.paused, s.data.runPaused], [true, true]);
const seqBefore = s.data.pauseSeq;
const r = await rpc("reload", {});
assert.equal(r.data.cancelled, false);
s = await rpc("get_state", {});
assert.equal(s.data.paused, true, "reload must not clear pause");
assert.equal(s.data.pauseSeq, seqBefore, "reload must not bump pauseSeq");
await rpc("resume_session", {});
s = await rpc("get_state", {});
assert.deepEqual([s.data.paused, s.data.runPaused], [false, false]);
```

슬랙 `/pause` 어댑터는 위 3단계(`set_client_info` → `interrupt:stop` → `get_state`)를 한 트랜잭션으로 묶고, `get_state.paused === true` 확인 후에만 사용자에게 "일시정지됨"을 응답한다. `reload`는 별도 슬래시 명령(`/reload`)으로, `resume_session`은 `/resume`으로 매핑한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `get_protocol_info` 응답의 `commands` 배열에 6개 명령이 모두 포함되고 `protocolVersion == 2`, `engine == "oh-my-tau"`이다.
2. `set_client_info` 없이 `interrupt:stop` 호출 시 `success:false, errorCode:"stop_requires_interactive_client"`가 반환된다.
3. 정상 순서 호출 시 `get_state`가 `paused:true, runPaused:true`를 반환하고 `pauseSeq`가 1 증가한다.
4. `reload`가 `{cancelled:false}`를 반환하고, 직후 `get_state`에서 `paused:true`와 동일 `pauseSeq`가 유지된다.
5. `resume_session` 후 `session_resumed` 이벤트가 정확히 1회 관측되고 `get_state`가 `paused:false, runPaused:false`다.
6. 모든 실패 응답이 `error`와 `errorCode`를 동시에 포함한다(`move-host.ts:4-7` 파싱 성공).

검증 명령어:

```
cargo test -p omp-cli rpc::tests::lane_hold_pause_reload_resume
cargo run -p omp-cli -- --mode rpc --multi-session --session-runtime in-process
bun test omo-nyang/src/lane-hold.test.ts -t "pause reload resume"
```

## 5. 엣지 케이스 및 실패 복구

- `capabilities` 미선언 후 `interrupt:stop`: `stop_requires_interactive_client` 반환, 보류 상태 진입 금지, `pauseSeq` 불변. 클라이언트는 `set_client_info` 재호출 후 재시도.
- 역할 재선언: 최초 `capabilities` 확정 후 다른 역할 선언 시 `connection_role_fixed`. 복구는 커넥션 재수립만 허용(동일 커넥션 내 우회 금지).
- `reload` 중 보류 유지 실패: `reload`가 `cancelled:true`를 반환하거나 `pauseSeq`가 변하면 회귀. `reload`는 확장 재로딩 실패 시에도 `cancelled:false`와 함께 부분 성공을 보고하고, 실패 상세는 별도 이벤트로 남긴다.
- `resume_session` 이중 호출: 두 번째 호출은 `{resumed:false}`를 반환하고 `session_resumed`를 재발행하지 않는다(멱등).
- RPC 프로세스 사망/타임아웃: 클라이언트는 `success` 미수신 시 `get_state`로 실제 상태를 재확인하고, `paused:true`가 남아 있으면 `/resume`을 재전송한다. `errorCode`가 없는 구버전 응답은 `Unknown command` 문자열 매칭으로 폴백하되, `rpc.rs:1094` arm이 계속 존재하는지 회귀 테스트로 감시한다.
- 다중 세션 모드에서 다른 세션의 `pauseSeq`와 혼선 방지: `get_state`는 세션 스코프 상태만 반환하고, 교차 세션 `resume_session`은 `errorCode:"session_mismatch"`로 거부한다.
