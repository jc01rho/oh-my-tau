# 테스트 조각 01: `set_client_info` 역할 고정 및 `interrupt`(`stop`/`abort`/멱등성/`pauseSeq`) 단위 테스트 명세 (test-01-unit-set-client-info-interrupt)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 oh-my-tau RPC 계층에서 보류/재개 계약의 최소 단위를 고정한다. 대상은 `set_client_info`의 역할 고정 규칙, `interrupt({intent})`의 `stop`/`abort` 분기, 중복 호출 멱등성, `pauseSeq` 단조 증가, 그리고 실패 프레임의 `code`/`errorCode` 동시 직렬화다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`
- 미지원 명령 분기: `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?, errorCode?}` 생성
- 소비자 파싱 계약: `omo-nyang/src/relay/move-host.ts:4-7`, `:61` — `MoveHost`가 동일 스키마를 파싱하므로 실패 시 `code`와 `errorCode`가 함께 실려야 함
- 보류·재개 계약: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info` → `interrupt` → `get_state` 순서, `stop_requires_interactive_client` / `connection_role_fixed` 에러 코드
- 프로토콜 핸드셰이크: `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`
- 해제 경로: `resume_session()` → `paused:false`, `runPaused:false`, `session_resumed` 이벤트 발행

목적은 상위 하네스(omo-nyang)를 띄우지 않고도 `dispatch()` 수준에서 계약 위반을 즉시 검출하는 것이다.

## 2. 상세 계약 및 데이터/타입 명세

요청 페이로드(클라이언트 → tau, stdin JSONL 1줄):

```json
{"id":11,"cmd":"set_client_info","width":80,"capabilities":["interactive_user"]}
{"id":12,"cmd":"interrupt","intent":"stop"}
{"id":13,"cmd":"get_state"}
```

응답 페이로드(`rpc.rs:203-226` 조립 규칙):

```json
{"type":"response","id":12,"success":true,"data":{"paused":true,"runPaused":true,"pauseSeq":1}}
{"type":"response","id":12,"success":false,"error":"stop requires interactive client","errorCode":"stop_requires_interactive_client"}
```

필드 계약:

| 필드 | 타입 | 규칙 |
|---|---|---|
| `capabilities` | `string[]` | `"interactive_user"` 미선언 시 `stop` 거부 |
| 역할 고정 | 내부 상태 | 최초 선언 후 다른 capability 집합 전송 시 `connection_role_fixed` |
| `intent` | `"stop" \| "abort"` | 그 외 값은 `Unknown command` 계열이 아닌 인자 검증 에러 |
| `paused` / `runPaused` | `bool` | `interrupt` 성공 시 둘 다 `true`, `resume_session()` 후 둘 다 `false` |
| `pauseSeq` | `u64` | 성공한 `interrupt`에서만 +1, 거부/멱등 재호출에서는 불변 |

Rust 측 표현(신규 테스트 하네스에서 사용할 최소 래퍼):

```rust
// crates/omp-cli/tests/support/lane.rs
pub async fn call(rt: &Arc<Runtime>, cmd: Value) -> Value; // response() 스키마 그대로 반환
pub async fn state(rt: &Arc<Runtime>) -> (bool, bool, u64); // (paused, runPaused, pauseSeq)
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust 단위 테스트 파일 `crates/omp-cli/tests/rpc_lane_hold.rs`:

```rust
#[tokio::test]
async fn stop_without_capability_is_rejected() {
    let rt = test_runtime().await;                       // rpc.rs:593 dispatch 대상 Runtime
    call(&rt, json!({"id":1,"cmd":"set_client_info","width":80,"capabilities":[]})).await;
    let r = call(&rt, json!({"id":2,"cmd":"interrupt","intent":"stop"})).await;
    assert_eq!(r["success"], false);
    assert_eq!(r["errorCode"], "stop_requires_interactive_client");
    assert!(r.get("error").is_some());                   // code + errorCode 동시 직렬화
    assert_eq!(state(&rt).await, (false, false, 0));     // 거부는 pauseSeq 불변
}

#[tokio::test]
async fn role_is_fixed_after_first_declaration() {
    let rt = test_runtime().await;
    call(&rt, json!({"id":1,"cmd":"set_client_info","width":80,
                     "capabilities":["interactive_user"]})).await;
    let r = call(&rt, json!({"id":2,"cmd":"set_client_info","width":120,
                             "capabilities":[]})).await;
    assert_eq!(r["errorCode"], "connection_role_fixed");
}

#[tokio::test]
async fn stop_then_abort_is_idempotent_and_seq_monotonic() {
    let rt = interactive_runtime().await;                // interactive_user 선언 완료 상태
    let a = call(&rt, json!({"id":1,"cmd":"interrupt","intent":"stop"})).await;
    let s1 = state(&rt).await;
    let b = call(&rt, json!({"id":2,"cmd":"interrupt","intent":"stop"})).await;
    let s2 = state(&rt).await;
    assert_eq!(a["success"], true);
    assert_eq!(b["success"], true);                      // 이미 보류 중이어도 성공 응답
    assert_eq!(s1, s2);                                  // 멱등: paused/runPaused/pauseSeq 불변
    let c = call(&rt, json!({"id":3,"cmd":"interrupt","intent":"abort"})).await;
    assert_eq!(c["success"], true);
    assert!(state(&rt).await.2 > s2.2);                  // abort는 새 시퀀스 발급
}
```

TS 측 교차 검증 `omo-nyang/src/lane-hold.test.ts` (실제 프로세스 스폰, `move-host.ts:4-7` 파서 재사용):

```ts
const host = spawnRpcHost();                 // omt --mode rpc --multi-session --session-runtime in-process
await host.call("get_protocol_info");        // protocolVersion === 2, engine === "oh-my-tau"
await host.call("set_client_info", { width: 80, capabilities: ["interactive_user"] });
const stop = await host.call("interrupt", { intent: "stop" });
expect(stop.success).toBe(true);
expect(await host.call("get_state")).toMatchObject({ paused: true, runPaused: true });
const resumed = await host.call("resume_session");
expect(resumed.data.cancelled).toBe(false);
expect(await host.call("get_state")).toMatchObject({ paused: false, runPaused: false });
```

의사코드 검증 순서: 1) capability 미선언 거부 → 2) 선언 후 역할 고정 → 3) `stop` 성공 및 seq 증가 → 4) 재호출 멱등 → 5) `abort` seq 증가 → 6) `resume_session` 해제 및 `session_resumed` 이벤트 수신 → 7) 재보류 시 seq가 이전 값보다 큼.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `set_client_info`에서 `interactive_user` 미선언 상태의 `interrupt({intent:"stop"})`가 `success:false`, `errorCode:"stop_requires_interactive_client"`, `error` 비어 있지 않음.
2. 최초 선언 이후 다른 capability 집합 전송이 `connection_role_fixed`로 실패하고, 기존 역할/`pauseSeq`가 보존됨.
3. 동일 `intent` 연속 호출 시 `paused`/`runPaused`/`pauseSeq`가 모두 불변이고 두 응답 모두 `success:true`.
4. `abort` 호출이 `pauseSeq`를 정확히 1 증가시킴(2회 호출 시 +1, +1).
5. `resume_session()` 후 `get_state`가 `paused:false, runPaused:false`이고 `session_resumed` 이벤트가 1회 발행됨.
6. 모든 실패 프레임이 `rpc.rs:203-226` 스키마를 만족하여 `move-host.ts:61` 파서가 `errorCode`를 읽을 수 있음.
7. 신규 테스트가 기존 `rpc.rs:1094` 미지원 명령 회귀를 깨지 않음.

명령어:

```
cargo test -p omp-cli --test rpc_lane_hold -- --nocapture
cargo test -p omp-cli --test rpc_response_frame
cargo clippy -p omp-cli --tests -- -D warnings
pnpm -C omo-nyang vitest run src/lane-hold.test.ts
```

## 5. 엣지 케이스 및 실패 복구

- `set_client_info` 이전에 `interrupt` 도달: capability 미선언으로 간주하여 `stop_requires_interactive_client` 반환, `pauseSeq`는 0 유지.
- `width` 누락 또는 0: capability 판정과 독립적으로 처리하되 역할 고정은 최초 유효 선언 시점에 확정.
- `intent` 미지정/오타: `rpc.rs:1094`의 `Unknown command` 경로로 흘러가지 않도록 `interrupt` 핸들러 내부 인자 검증에서 거부하고 `errorCode`를 부여.
- 동시 `interrupt` 2건: 락 획득 순서에 따라 seq가 1만 증가하고 두 응답 모두 `success:true`(동일 seq 관측) — `pauseSeq`는 단조 증가 불변식만 보장.
- `abort`를 보류 상태가 아닐 때 호출: 계약상 허용, seq 증가 및 `paused:true` 전이.
- `resume_session()` 중복 호출: 두 번째 호출은 `{cancelled:false}`로 성공하고 이벤트는 발행하지 않음.
- `reload()`가 보류 중 수행: `paused` 상태 유지, `pauseSeq` 불변, 확장 재로딩 실패 시 원자적 롤백 후 에러 프레임 반환.
- 프로세스 재시작: `pauseSeq`는 0으로 초기화되며 테스트는 절대값이 아닌 증가분만 단언.
- 프레임 직렬화 실패(`code`만 있고 `errorCode` 누락): `move-host.ts:61`이 `undefined`를 받아 상위 분기가 오작동하므로, 실패 케이스마다 두 필드 존재를 명시적으로 단언하여 회귀를 차단.
