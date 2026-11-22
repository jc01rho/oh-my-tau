# 문서 조각 02: `docs/parity/gateway-tools.md` 신설용 OmOMeow 에이전트 도구 10종 패리티 및 스키마 매핑 원장 표 (doc-02-parity-ledger-agent-tools)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 `docs/parity/gateway-tools.md`를 신설하기 위한 설계 원장이다. OmOMeow 하네스가 호출하는 에이전트 도구 10종을 oh-my-tau RPC 표면에 매핑하고, 각 도구의 요청/응답 JSON 스키마와 실패 코드를 고정한다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령 폴백: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 10종 명령은 이 arm에 도달하면 실패로 간주한다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()`.
- 응답 소비자: `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 **동시에** 직렬화해야 한다.
- 하위 작업 현황: `crates/omp-cli/src/tools/task.rs`, `crates/omp-task` — `context` + `tasks[]` 배치 **동기** 실행만 존재. 비동기 `run_in_background`(`st_...` ID), `task_send`, `task_output`, `task_cancel` 부재. `crates/omp-cli/src/rpc.rs:863` `get_subagents`는 빈 배열 스텁.
- 백그라운드 셸 현황: Phase 2에서 `shell_host.rs`, `jobs.rs`(`proc://` 핸들), `wait.rs` 도입. `bash_output`, `kill_bash`, `monitor`(command/path/target 조건 감시) 도구는 부재.
- 장기 기억 현황: `/memory`는 스텁. `memory`(create/str_replace/insert/delete/rename/update_description + git 자동 커밋) 및 `memory_apply_patch` 부재. Phase 3.3 `crates/omp-memory`와 통합 필요.
- 작업 큐 현황: `workpool`(create/push/close/inspect/cancel/yield) 부재.
- 질문·목표 현황: tau는 `ask({html}|legacy questions[])`와 `goal({op})` 단일 도구. OmOMeow는 `ask_user_question({questions:[{header,question,multiSelect,options}],waitForAnswer,required})`와 `create_goal`/`get_goal`/`update_goal` 3종을 호출.
- 기능 탐지 근거: `omo-nyang/src/lane-hold.ts:29-76`의 `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`. 신규 명령은 `commands[]`에 노출되어야 클라이언트가 게이팅할 수 있다.
- 확장 라우팅 근거: `extension_request({sessionId?, name, data?})` → `api.rpc.handle(name, handler)`(`omo-nyang/src/relay/move-operations.ts:104-225`, `session-move-extension.ts:116-139`). 도구 중 확장 소유분은 이 경로로 위임한다.

## 2. 상세 계약 및 데이터/타입 명세

공통 응답 봉투(성공/실패 모두 `rpc.rs:203-226` 경유):

```json
{"type":"response","id":17,"success":false,"code":"unknown_command","errorCode":"unknown_command","error":"Unknown command: bash_output"}
```

Rust 측 타입 계약:

```rust
pub struct CmdError { pub code: &'static str, pub message: String } // code == errorCode 로 직렬화
type CmdResult = Result<Value, CmdError>;
```

패리티 원장 표(10종):

| # | OmOMeow 도구 | tau 현황 (file:line) | 제안 RPC 명령 | 요청 페이로드 | 응답 페이로드 | 판정 |
|---|---|---|---|---|---|---|
| 1 | `task` (run_in_background) | `tools/task.rs`, `crates/omp-task` 동기 배치 | `task_start` | `{"context":string,"tasks":[{"prompt":string,"agent"?:string}],"run_in_background":bool}` | 동기 `{"status":"completed","results":[{"taskIndex":n,"output":string}]}` / 비동기 `{"taskId":"st_01J...","status":"running"}` | 신규 |
| 2 | `task_send` / `task_output` / `task_cancel` | `rpc.rs:863` `get_subagents` 빈 배열 스텁 | `task_send`, `task_output`, `task_cancel` | `{"taskId":"st_...","input":string}` / `{"taskId":"st_...","waitMs":number}` / `{"taskId":"st_..."}` | `{"accepted":bool}` / `{"chunks":[{"seq":n,"data":string}],"settled":bool,"exitCode":number\|null}` / `{"cancelled":bool}` | 신규 |
| 3 | `bash_output` | `shell_host.rs`, `jobs.rs`(`proc://`), `wait.rs` | `bash_output` | `{"jobId":"proc://...","offset":number,"limit":number}` | `{"chunks":[{"seq":n,"stream":"stdout"\|"stderr","data":string}],"exited":bool,"exitCode":number\|null}` | 어댑터 |
| 4 | `kill_bash` | `jobs.rs` 핸들 회수만 존재 | `kill_bash` | `{"jobId":"proc://...","signal":"TERM"\|"KILL"}` | `{"killed":bool,"exitCode":number\|null}` | 어댑터 |
| 5 | `monitor` | `wait.rs` 조건 대기 프리미티브 | `monitor` | `{"condition":{"type":"command"\|"path"\|"target","command"?:string,"path"?:string,"target"?:string,"timeoutMs":number}}` | `{"monitorId":"mon_...","state":"armed"\|"fired"\|"timeout"}` | 신규 |
| 6 | `memory` | `/memory` 스텁, Phase 3.3 `crates/omp-memory` | `memory` | `{"op":"create"\|"str_replace"\|"insert"\|"delete"\|"rename"
