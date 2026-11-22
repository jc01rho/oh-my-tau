# 테스트 조각 02: `get_protocol_info`·`reload`·`resume_session`(`lane-hold.ts` 시퀀스 재현) 단위·통합 테스트 명세 (test-02-unit-protocol-info-reload-resume)

## 1. 목적 및 코드 근거 (File & Line)

`omo-nyang/src/lane-hold.ts:29-76`이 구현하는 보류·재개 시퀀스(`set_client_info` → `interrupt` → `get_state` → `resume_session`)와, 그 사이에 끼어드는 `get_protocol_info`/`reload`를 oh-my-tau RPC 서버가 계약대로 처리하는지 단위(Rust)와 통합(TS, 실제 호스트 프로세스) 양쪽에서 고정한다. 목표는 세 가지다.

- `get_protocol_info` 응답이 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 만족하고, `commands`에 나열된 모든 이름이 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))` 분기에 걸리지 않음을 증명한다.
- `reload()`가 `{cancelled:false}`를 반환하고 보류 상태를 파괴하지 않음을 증명한다.
- `resume_session()`이 보류를 해제하고 `session_resumed` 이벤트를 발행한 뒤 `get_state`에서 `paused:false, runPaused:false`가 관측됨을 증명한다.

근거 좌표:

| 계약 | 위치 |
| --- | --- |
| 디스패치 진입점 `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` | `crates/omp-cli/src/rpc.rs:593` |
| 미지원 명령 처리 | `crates/omp-cli/src/rpc.rs:1094` |
| 응답 프레임 조립 `response()` | `crates/omp-cli/src/rpc.rs:203-226` |
| 클라이언트 측 프레임 파싱 `{type:"response", id, success, data?, error?, errorCode?}` | `omo-nyang/src/relay/move-host.ts:4-7, 61` |
| 보류·재개 시퀀스 및 두 에러(`stop_requires_interactive_client`, `connection_role_fixed`) | `omo-nyang/src/lane-hold.ts:29-76` |
| 호스트 실행 인자 `--mode rpc --multi-session --session-runtime in-process -e <ext>` | `omo-nyang/src/relay/move-host.ts:37-39` |
| `get_subagents` 스텁(회귀 감시 대상, 본 조각 범위 외) | `crates/omp-cli/src/rpc.rs:863` |

## 2. 상세 계약 및 데이터/타입 명세

Rust 측 타입은 `dispatch`의 반환 타입을 그대로 쓴다: `type CmdResult = Result<serde_json::Value, CmdError>`이며 `CmdError::new(String)`으로 생성되고, `response()`(`rpc.rs:203-226`)가 이를 `success:false` + `error` + `errorCode`로 직렬화한다. `errorCode`는 `code`와 **동시에** 존재해야 한다(`move-host.ts:4-7`이 두 필드를 모두 optional로 파싱하므로, 하나만 있으면 클라이언트 분기가 죽는다).

요청/응답 스키마(NDJSON 1줄 = 1프레임):

```jsonc
// 요청
{"type":"get_protocol_info","id":"p1"}
{"type":"set_client_info","id":"c1","width":80,"capabilities":["interactive_user"]}
{"type":"interrupt","id":"i1","intent":"stop"}          // "stop" | "abort"
{"type":"get_state","id":"s1"}
{"type":"reload","id":"r1"}
{"type":"resume_session","id":"u1"}

// 응답(성공)
{"type":"response","id":"p1","success":true,
 "data":{"protocolVersion":2,"serverVersion":"<CARGO_PKG_VERSION>",
         "engine":"oh-my-tau",
         "commands":["get_protocol_info","set_client_info","interrupt","get_state","reload","resume_session", "..."]}}

// 응답(실패)
{"type":"response","id":"i1","success":false,"error":"...","errorCode":"stop_requires_interactive_client"}

// get_state data
{"paused":true,"runPaused":true,"pauseSeq":3}

// reload data
{"cancelled":false}

// 이벤트(응답과 별개 프레임)
{"type":"event","event":"session_resumed","data":{...}}
```

불변식(invariant):
- `I1` `protocolVersion == 2`, `engine == "oh-my-tau"`, `serverVersion`은 `^\d+\.\d+\.\d+` (semver).
- `I2` `commands`는 중복 없는 문자열 배열이며 `set_client_info`, `interrupt`, `get_state`, `get_protocol_info`, `reload`, `resume_session`을 모두 포함한다.
- `I3` `pauseSeq`는 보류 진입마다 단조 증가하고 `resume_session` 이후에도 감소하지 않는다.
- `I4` `resume_session` 성공 시 `session_resumed` 이벤트가 같은 연결에서 최소 1회 관측되고, 그 이후 `get_state`는 `paused:false, runPaused:false`다.
- `I5` `reload`는 `cancelled:false`를 반환하며 `pauseSeq`와 `paused`를 변경하지 않는다(재로딩은 상태 기계를 리셋하지 않는다).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

(a) Rust 단위 테스트 — `crates/omp-cli/src/rpc.rs` 하단 `#[cfg(test)] mod lane_hold_tests`. `dispatch`를 직접 호출하고 `response()`로 프레임을 조립해 클라이언트 파서와 동일한 형태를 검사한다.

```rust
async fn call(rt: &Arc<Runtime>, id: &str, cmd: serde_json::Value) -> serde_json::Value {
    let kind = cmd["type"].as_str().unwrap().to_string();
    let res: CmdResult = dispatch(rt, &cmd, &kind).await;   // rpc.rs:593
    response(id, res)                                        // rpc.rs:203-226
}

#[tokio::test]
async fn protocol_info_contract() {
    let rt = test_runtime().await;
    let f = call(&rt, "p1", json!({"type":"get_protocol_info","id":"p1"})).await;
    assert_eq!(f["success"], true);
    let d = &f["data"];
    assert_eq!(d["protocolVersion"], 2);
    assert_eq!(d["engine"], "oh-my-tau");
    assert_eq!(d["serverVersion"], env!("CARGO_PKG_VERSION"));
    for c in ["set_client_info","interrupt","get_state","get_protocol_info","reload","resume_session"] {
        assert!(d["commands"].as_array().unwrap().iter().any(|v| v == c), "missing {c}");
    }
}

#[tokio::test]
async fn unknown_command_carries_code_and_error_code() {
    let rt = test_runtime().await;
    let f = call(&rt, "x1", json!({"type":"nope","id":"x1"})).await;
    assert_eq!(f["success"], false);
    assert!(f["error"].is_string());
    assert!(f["errorCode"].is_string(), "move-host.ts:4-7 requires errorCode");
    assert!(f["error"].as_str().unwrap().contains("Unknown command: nope")); // rpc.rs:1094
}
```

`reload`/`resume_session` 단위 테스트는 `get_state` 스냅샷을 전후로 비교한다.

```rust
let s0 = call(&rt, "s0", json!({"type":"get_state"})).await;
let r  = call(&rt, "r1", json!({"type":"reload"})).await;
assert_eq!(r["data"]["cancelled"], false);
let s1 = call(&rt, "s1", json!({"type":"get_state"})).await;
assert_eq!(s0["data"]["pauseSeq"], s1["data"]["pauseSeq"]);   // I5
let u = call(&rt, "u1", json!({"type":"resume_session"})).await;
assert_eq!(u["success"], true);
let s2 = call(&rt, "s2", json!({"type":"get_state"})).await;
assert_eq!(s2["data"]["paused"], false);
assert_eq!(s2["data"]["runPaused"], false);
```

(b) TS 통합 테스트 — `omo-nyang/test/lane-hold.sequence.test.ts`. `move-host.ts:37-39`와 동일 인자로 실제 호스트를 띄우고, `lane-hold.ts:29-76`의 시퀀스를 그대로 실행한다.

```ts
const host = spawn("omt", ["--mode","rpc","--multi-session","--session-runtime","in-process","-e",extPath]);
const rpc = makeRpc(host, {timeoutMs: 10_000});  // id 매칭, move-host.ts:4-7 스키마 검증

const p = await rpc("get_protocol_info");
expect(p.data.protocolVersion).toBe(2);
expect(p.data.engine).toBe("oh-my-tau");
expect(p.data.commands).toEqual(expect.arrayContaining(["reload","resume
