# 조사 조각 06 (1795326973 5번 항목): `docs/rust-migration-plan.md` 미머지 Phase 3·4·5와 OmOMeow 전환 요구사항 간의 중첩·선후관계 분석 (inv-06-tau-phase3-4-5-overlap)

## 1. 목적 및 코드 근거 (File & Line)

`docs/rust-migration-plan.md`의 미머지 Phase 3(장기 기억·작업 큐), Phase 4(백그라운드 셸/프로세스 호스팅), Phase 5(비동기 하위 에이전트)가 OmOMeow 하네스가 이미 호출하는 도구·RPC 계약과 어디서 겹치고, 어떤 순서로 머지되어야 하는지를 확정한다. 목적은 "tau가 자체 로드맵대로 완성된 뒤 전환"이 아니라 "OmOMeow가 요구하는 최소 계약을 만족하는 순서로 Phase를 재배열"하는 것이다.

- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어지므로, 신규 도구/명령은 반드시 이 match에 arm을 추가해야 한다.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226`의 `response()`. `omo-nyang/src/relay/move-host.ts:4-7,61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 동시 직렬화해야 한다.
- 하위 에이전트 스텁: `crates/omp-cli/src/rpc.rs:863` `get_subagents`는 빈 배열 반환. `crates/omp-cli/src/tools/task.rs` + `crates/omp-task`는 `context` + `tasks[]` 배치 동기 실행만 지원.
- 백그라운드 셸: Phase 2에서 `shell_host.rs`, `jobs.rs`(`proc://`), `wait.rs`가 이미 도입됨(도구 표면 미노출).
- 장기 기억: `/memory`는 스텁, Phase 3.3 `crates/omp-memory` 미통합.
- 종료 신호: tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout 출력, `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 발행. 소비자는 `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`.
- CLI 표면: `omo-nyang/src/gateway.ts:1167`은 `--skill <디렉터리경로>`를, `omo-nyang/src/model-routing.ts:654-658`은 `--no-model-fallback`, `--no-recommended-models`를 전달. tau는 `crates/omp-cli/src/cli.rs:97-166`에 `--skills <CSV필터>`(129, 309)만 존재.
- 세션 이동: `omo-nyang/src/relay/move-host.ts:37-39`, `move-operations.ts:104-225`, `session-move-extension.ts:116-139`.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 중첩 매트릭스 (Phase ↔ OmOMeow 요구)

| Phase | tau 산출물 | OmOMeow 호출 표면 | 중첩 판정 |
|---|---|---|---|
| Phase 2 (기머지) | `shell_host.rs`, `jobs.rs`, `wait.rs` | `bash_output`, `kill_bash`, `monitor(command/path/target)` | 부분 중첩: 엔진 있음, 도구 없음 |
| Phase 3.3 | `crates/omp-memory` | `memory(create/str_replace/insert/delete/rename/update_description)`, `memory_apply_patch`, `/memory` | 완전 중첩(신규 노출) |
| Phase 3.x | workpool | `workpool(create/push/close/inspect/cancel/yield)` | 완전 중첩(미착수) |
| Phase 5 | `crates/omp-task` 비동기 | `run_in_background`(`st_...`), `task_send`, `task_output`, `task_cancel`, `get_subagents` | 부분 중첩: 동기 배치만 존재 |
| 공통 | `ask`, `goal` 단일 도구 | `ask_user_question`, `create_goal`/`get_goal`/`update_goal` | 표면 불일치 |

### 2.2 선후관계(DAG)

1. `jobs.rs`(`proc://`) → `bash_output`/`kill_bash`/`monitor`: job 핸들 ID와 출력 버퍼 수명 계약이 먼저 고정되어야 한다.
2. `crates/omp-memory` → `memory`/`memory_apply_patch`: git 자동 커밋 훅이 도구 계층이 아니라 스토어 계층에 있어야 재시도가 안전하다.
3. `jobs.rs` → `workpool`: `push`/`yield`가 프로세스 슬롯을 점유하므로 `proc://` 회수 규약 공유.
4. Phase 5 비동기 러너 → `get_subagents` 채움(`rpc.rs:863`): 스텁을 실제 레지스트리 조회로 교체.
5. `agent_settled` stdout 발행 → fleet 전환: 이것이 없으면 `fleet.ts:258`이 영구 대기한다.
6. `--skill`(Vec<PathBuf>) / `--no-model-fallback` / `--no-recommended-models` → `gateway.ts` 스폰 경로 전환.
7. `--multi-session` + `--session-runtime in-process` → `move_session` op(`prepare|export|import|status|finish|cancel`), `release_session({sessionId, reason:"move", moveId}) → {session_path}`, `extension_request` 라우팅(`omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit`).

### 2.3 페이로드/타입

- `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`; `set_client_info({width:80, capabilities:["interactive_user"]})` 선행 필수.
- `get_protocol_info() → {protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}` — 신규 명령은 `commands` 배열에도 반영되어야 클라이언트가 기능 탐지 가능.
- 확장 이벤트: `session_move_export`/`session_move_import` (`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})`.
- 세션 로그: `omo-nyang/src/session-patterns.ts:47-76`이 JSONL 각 줄에서 `{timestamp, message:{role:"assistant"|"toolResult", content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}`를 읽는다. tau의 `crates/omp-session/src/entries.rs` v3(256B title slot + camelCase entries)와 필드명이 다르므로 어댑터가 필요하다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/rpc.rs: dispatch() 내 신규 arm (1094 이전에 배치)
"bash_output" => tools::shell_host::output(rt, &cmd).await,   // {jobId, offset?} -> {chunk, nextOffset, eof}
"kill_bash"   => tools::jobs::kill(rt, &cmd).await,           // {jobId} -> {killed: bool}
"monitor"     => tools::wait::monitor(rt, &cmd).await,        // {command|path|target, timeoutMs} -> {matched, detail}
"memory"      => tools::memory::apply(rt, &cmd).await,        // {op:"create"|"str_replace"|...} -> {path, commit}
"memory_apply_patch" => tools::memory::patch(rt, &cmd).await,
"workpool"    => tools::workpool::dispatch(rt, &cmd).await,
"task_send"   => tools::task::send(rt, &cmd).await,
"task_output" => tools::task::output(rt, &cmd).await,
"task_cancel" => tools::task::cancel(rt, &cmd).await,
"get_subagents" => tools::task::list(rt, &cmd).await,         // rpc.rs:863 스텁 대체
```

```rust
// 실패 프레임: rpc.rs:203-226 response() 조립 규약 준수
Err(CmdError::new("stop_requires_interactive_client")
      .with_code("interactive_required"))   // -> {"success":false,"error":"...","errorCode":"interactive_required"}
```

```ts
// omo-nyang: 종료 신호 이중 수용 (runtime.ts:59, slice-run.ts:71, fleet.ts:258, agentd-sessions.ts:186)
const settled = (evt: any) => evt.type === "agent_settled" || evt.type === "session_settled";
```

```rust
// crates/omp-cli/src/cli.rs: --skills(129,309)는 유지, --skill은 반복 가능 경로로 분리
#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
skill_paths: Vec<PathBuf>,   // gateway.ts:1167 대응, 디렉터리 존재 검증 후 스킬 루트에 prepend
```

검증 순서: (1) `get_protocol_info().commands`에 신규 명령 포함 확인 → (2) 각 신규 arm에 대해 정상/실패 프레임 스냅샷 테스트 → (3) `agent_settled` stdout 1회 발행 확인 → (4) `move_session` 6개 op 왕복 → (5) JSONL v3 → session-patterns 어댑터 파싱 확인.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `get_protocol_info()`의 `commands`에 `bash_output, kill_bash, monitor, memory, memory_apply_patch, workpool, task_send, task_output, task_cancel`이 모두 존재.
- AC2: `rpc.rs:863` `get_subagents`가 `run_in_background`로 생성된 `st_...` ID를 최소 1건 반환.
- AC3: 실패 응답에 `error`와 `errorCode`가 동시 존재(move-host.ts:4-7 파싱 성공).
- AC4: RPC stdout에서 `agent_settled` 관측, `session_settled`와 순서 역전 없음.
- AC5: `--skill` 2회 반복 시 두 경로의 스킬이 모두 로드되고 `--skills` CSV 필터와 독립 동작.
- AC6: `move_session` `prepare→export→import→status→finish` 왕복 후 `get
