# 조사 조각 08: `mhc`(63 크레이트) 유지 대비 `oh-my-tau`(20 크레이트) 게이트웨이 엔진 전환의 크레이트별 변경 줄 수(LOC) 및 공수 정량 추정 (inv-08-cost-and-effort-mhc-vs-tau)

## 1. 목적 및 코드 근거 (File & Line)

목적: 게이트웨이 엔진을 `mhc`(63 크레이트) 유지로 갈 때와 `oh-my-tau`(20 크레이트)로 전환할 때의 크레이트별 변경 LOC와 공수를 동일 기준으로 계량한다. 계량 단위는 (a) Rust LOC, (b) TS 어댑터 LOC, (c) 계약 미충족 항목 수다.

- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 명령 분기는 `match`이며 미지원 분기는 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `fn response(...)`. 소비자는 `omo-nyang/src/relay/move-host.ts:4-7`의 타입과 `move-host.ts:61`의 파싱 지점이며 기대 스키마는 `{type:"response", id, success, data?, error?, errorCode?}`이다. 즉 실패 시 `code`와 `errorCode`를 동시 직렬화해야 한다.
- 보류·재개 계약: `omo-nyang/src/lane-hold.ts:29-76`. 순서는 `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. 위반 에러는 `stop_requires_interactive_client`, `connection_role_fixed`이며 `lane-hold.ts:29`의 초기 선언 이후 역할 고정이 전제다.
- 프로토콜/재개: `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`; `reload()` → `{cancelled:false}`; `resume_session()` → `session_resumed` 이벤트 발행 후 `get_state`에서 `paused:false, runPaused:false`.
- 세션 이동 계약: `omo-nyang/src/relay/move-host.ts:37-39`(호스트 실행 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`), `omo-nyang/src/relay/move-operations.ts:104-225`(`move_session` op 6종: `prepare|export|import|status|finish|cancel`), `omo-nyang/src/session-move-extension.ts:116-139`(`session_move_export`/`session_move_import`, `event={dir, signal}`, `ctx.sessionManager.getSessionFile()`, `api.events.on("wake_source_state", {source, activeCount})`).
- CLI 플래그 격차: `omo-nyang/src/gateway.ts:1167`은 `--skill <디렉터리경로>`를 전달하나 `crates/omp-cli/src/cli.rs:97-166`에는 `--skills <CSV필터>`(`cli.rs:129`, `cli.rs:309`)만 존재. `omo-nyang/src/model-routing.ts:654-658`은 `--no-model-fallback`, `--no-recommended-models`를 전달.
- 도구 격차: `crates/omp-cli/src/tools/task.rs` + `crates/omp-task`는 `context`+`tasks[]` 배치 동기 실행이고 `run_in_background`(`st_...` ID), `task_send`, `task_output`, `task_cancel`이 없으며 `crates/omp-cli/src/rpc.rs:863` `get_subagents`는 빈 배열 스텁이다. `shell_host.rs`, `jobs.rs`(`proc://`), `wait.rs`는 있으나 `bash_output`, `kill_bash`, `monitor`가 없다. `/memory`는 스텁이고 `crates/omp-memory`(Phase 3.3)의 `memory`/`memory_apply_patch`가 미배선이다. `workpool` 5종 도구 부재.
- 하네스 격차: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout 발행, `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 전송. 소비자는 `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`. 설정 루트는 `crates/omp-settings/src/lib.rs`의 `~/.omp` vs `~/.omo`(`omo.jsonc`, `fleet-small`/`fleet-medium`). 세션 기록은 `crates/omp-session/src/entries.rs`의 v3 포맷(256B title slot + camelCase) vs `omo-nyang/src/session-patterns.ts:47-76`의 `{timestamp, message:{role:"assistant"|"toolResult", content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}` 판독.

## 2. 상세 계약 및 데이터/타입 명세

RPC 응답 타입(직렬화 계약, `rpc.rs:203-226` 기준):

```rust
#[derive(Serialize)]
struct ResponseFrame<'a> { #[serde(rename="type")] kind: &'static str, id: Value,
  success: bool, #[serde(skip_serializing_if="Option::is_none")] data: Option<Value>,
  #[serde(skip_serializing_if="Option::is_none")] error: Option<&'a str>,
  #[serde(rename="errorCode", skip_serializing_if="Option::is_none")] error_code: Option<&'a str> }
```

- `CmdResult`는 `Result<Value, CmdError>`로 두고 `CmdError { code: &'static str, message: String }`에 `errorCode`를 추가한다. `move-host.ts:61` 파싱 실패 시 재시도 불가하므로 `success:false`에도 `errorCode` 필수.
- `get_protocol_info` 응답: `{protocolVersion:2, serverVersion:"<CARGO_PKG_VERSION>", engine:"oh-my-tau", commands:[&'static str]}`이며 `commands`는 `rpc.rs:593` dispatch 분기와 1:1로 일치해야 한다(문서-구현 드리프트 방지).
- `move_session` 요청 스키마: `{op:"prepare"|"export"|"import"|"status"|"finish"|"cancel", moveId:string, sessionPath:string, bundleDir:string, cwd:string, from?:string, answer?:Value}`.
- `release_session` 요청/응답: `{sessionId:string, reason:"move", moveId:string}` → `{session_path:string}`.
- `extension_request` 요청: `{sessionId?:string, name:string, data?:Value}` → `api.rpc.handle(name, handler)` 라우팅. 등록 이름 화이트리스트: `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`.
- 확장 이벤트: `session_move_export`, `session_move_import`(`event={dir:string, signal:string}`), `wake_source_state`(`{source:string, activeCount:number}`).
- CLI 타입: `Args::skill_paths: Vec<PathBuf>`(반복 가능, `--skill`), `skills: Option<String>`(CSV 필터, 기존 유지), `multi_session: bool`, `session_runtime: SessionRuntime{InProcess}`, `no_model_fallback: bool`, `no_recommended_models: bool`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

크레이트별 LOC 추정(전환 경로, Rust 기준, 괄호는 신규/수정):

| 크레이트 | 앵커 | 작업 | LOC |
|---|---|---|---|
| omp-cli (rpc) | `rpc.rs:593`, `rpc.rs:863`, `rpc.rs:1094` | 분기 9개 추가, `get_subagents` 실구현, `errorCode` 직렬화 | 520-700 |
| omp-cli (cli) | `cli.rs:97-166`, `cli.rs:129`, `cli.rs:309` | `--skill` 반복, `--multi-session`, `--session-runtime`, `--no-*` | 120-160 |
| omp-cli (tools) | `tools/task.rs`, `shell_host.rs`, `jobs.rs`, `wait.rs` | `bash_output`/`kill_bash`/`monitor`, `workpool` 5종, `ask_user_question`, `goal` 3종 | 1,600-2,100 |
| omp-task | `crates/omp-task` | `run_in_background`(`st_`), `task_send/output/cancel` | 420-560 |
| omp-memory | Phase 3.3 `crates/omp-memory` | `memory` 6 op + git 자동 커밋, `memory_apply_patch` | 700-950 |
| omp-settings | `omp-settings/src/lib.rs` | `~/.omo` + `omo.jsonc` 프로필 해석 | 240-320 |
| omp-session | `omp-session/src/entries.rs` | v3 → `session-patterns.ts:47-76` 호환 뷰 | 260-340 |
| omp-cli (ext) | `ext.rs:1598` | `agent_settled` 외부 노출, `wake_source_state` 브리지 | 90-130 |
| omp-cli (runtime) | `runtime.rs:1641` | `session_settled`/`agent_settled` 이중 발행 | 60-90 |
| omp-omo-bridge (신규) | `move-host.ts:4-7`, `lane-hold.ts:29-76` | RPC 어댑터·op 라우팅 | 500-700 |
| 기타 10개 크레이트 | - | 무변경(컴파일 영향만) | 0 |
| **합계** | | | **4,510-6,050** |

TS 측(`omo-nyang`) 수정: `move-host.ts`(프레임 파싱 강화) 80-120, `lane-hold.ts`(capability 선언 순서 고정) 60-90, `model-routing.ts:654-658` 플래그 정합 40-60, `session-patterns.ts:47-76` v3 어댑터 80-130 → **260-400**.

`mhc` 유지 경로: 63개 크레이트 중 계약 정합화 필요 크레이트 11개(추정), 동일 9개 계약 항목에 대해 **3,900-5,200 LOC**(추정, 가정: 기존 게이트웨이 추상화 재사
