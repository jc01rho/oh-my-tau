# 구현 조각 04: `crates/omp-cli/src/rpc.rs`·`runtime.rs`의 `reload` 및 `resume_session` 재개 시퀀스 구현 명세 (impl-04-rpc-reload-resume-session)

## 1. 목적 및 코드 근거 (File & Line)

`omo-nyang/src/lane-hold.ts:29-76`의 보류·재개 시퀀스는 `set_client_info` → `interrupt` → `get_state` → `resume_session` 순으로 진행되며, 현재 tau 측에는 `reload`·`resume_session` 커맨드가 없어 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어진다. 본 조각은 다음을 닫는다.

- 디스패치 추가: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`의 match에 `"reload"`, `"resume_session"` arm을 `rpc.rs:1094` fallback **앞에** 삽입.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226` `response()`가 조립하는 프레임을 `omo-nyang/src/relay/move-host.ts:4-7, 61`의 `MoveHostResponse`(`{type:"response", id, success, data?, error?, errorCode?}`)와 정합시킨다. 실패 시 `error`(문자열)와 `errorCode`(기계 판독 코드)를 **동시에** 직렬화하고, 하위 호환을 위해 `code` 필드도 같은 값으로 중복 기록한다.
- 재개 상태원: `crates/omp-cli/src/runtime.rs`의 `PauseState`(paused / runPaused / pauseSeq)를 단일 진실원으로 삼고, `runtime.rs:1641`의 `session_settled` stdout 방출 지점에 `agent_settled` 방출을 병기한다(`omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`이 `agent_settled`를 대기하므로 미방출 시 fleet이 영구 대기).
- 기능 탐지: `get_protocol_info()`가 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 반환할 때 `commands` 배열에 `reload`, `resume_session`을 포함시켜 `lane-hold.ts:29-76`이 사전 탐지할 수 있게 한다.

## 2. 상세 계약 및 데이터/타입 명세

요청(둘 다 `id` 필수, `params` 선택):

```jsonc
{"id":"r1","command":"reload","params":{"reloadSkills":true,"reloadExtensions":true}}
{"id":"r2","command":"resume_session","params":{"sessionId":"s-123"}}
```

성공 응답:

```jsonc
{"type":"response","id":"r1","success":true,"data":{"cancelled":false,"skills":42,"extensions":3}}
{"type":"response","id":"r2","success":true,"data":{"paused":false,"runPaused":false,"pauseSeq":7,"wasPaused":true}}
```

실패 응답(`rpc.rs:203-226` 확장):

```jsonc
{"type":"response","id":"r2","success":false,
 "error":"stop requires interactive client","errorCode":"stop_requires_interactive_client",
 "code":"stop_requires_interactive_client"}
```

Rust 타입:

```rust
// crates/omp-cli/src/rpc.rs
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadParams { #[serde(default)] pub reload_skills: bool,
                          #[serde(default)] pub reload_extensions: bool }

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionParams { pub session_id: Option<String> }

pub struct CmdError { pub code: &'static str, pub message: String } // 기존 new(msg)는 code="internal"
```

```rust
// crates/omp-cli/src/runtime.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseState { pub paused: bool, pub run_paused: bool, pub pause_seq: u64 }

pub enum HoldReason { ClientStop, ClientAbort, Move } // move-host.ts:37-39 release_session(reason:"move")
```

이벤트 프레임(개행 구분 JSON, stdout 오염 금지):

```jsonc
{"type":"event","event":"session_resumed","data":{"sessionId":"s-123","pauseSeq":7,"wasPaused":true}}
{"type":"agent_settled","sessionId":"s-123"}
```

의미 규약: `interrupt({intent:"stop"})` → `paused=true, runPaused=false`(턴 경계 정지), `intent:"abort"` → `paused=true, runPaused=true`(실행 중단). `resume_session`은 두 값을 모두 `false`로 만들고 `pauseSeq`는 **감소·초기화하지 않는다**(클라이언트가 단조 증가로 비교).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// rpc.rs:593 dispatch 내부, rpc
