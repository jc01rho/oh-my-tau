# 문서 조각 05: `docs/rust-migration-plan.md`에 추가할 'Phase G (OmOMeow Gateway Engine Compatibility)' 섹션 개정안 (doc-05-migration-plan-phase-gateway)

## 1. 목적 및 코드 근거 (File & Line)

Phase G의 목적은 oh-my-tau(`omt`) RPC 엔진이 OmOMeow 게이트웨이(`omo-nyang`)가 이미 소비하는 계약을 **바이너리 재작성 없이** 충족하도록 만드는 것이다. 기존 Phase A~F가 엔진 내부 정합성에 집중했다면, Phase G는 "OmOMeow가 기대하는 프레임/플래그/도구/신호"를 tau 쪽에서 제공하는 호환 계층이다.

| 영역 | tau 현행 근거 | OmOMeow 소비 근거 | 격차 |
|---|---|---|---|
| RPC 디스패치 | `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`; 미지원 분기 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` | `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}` 파싱 | 실패 시 `errorCode` 미직렬화 |
| 응답 프레임 | `rpc.rs:203-226` `response()` | 동일 | `code` + `errorCode` 동시 필요 |
| 보류·재개 | `rpc.rs` interrupt/get_state/resume_session | `omo-nyang/src/lane-hold.ts:29-76` | `connection_role_fixed`, `stop_requires_interactive_client` 코드 정합 |
| 프로토콜 정보 | `get_protocol_info` | `lane-hold.ts` — `{protocolVersion:2, serverVersion, engine:"oh-my-tau", commands:[...]}` | `commands` 목록 완전성 |
| 세션 이동 | `release_session`, `extension_request`, `move_session` | `move-host.ts:37-39`, `move-operations.ts:104-225`, `session-move-extension.ts:116-139` | `moveId`/`session_path` 스키마 |
| CLI 플래그 | `crates/omp-cli/src/cli.rs:97-166` — `--skills <CSV>`만 존재(`cli.rs:129,309`) | `omo-nyang/src/gateway.ts:1167` `--skill <dir>` 반복, `model-routing.ts:654-658` `--no-model-fallback`, `--no-recommended-models`, `move-host.ts:37` `--multi-session --session-runtime in-process` | `--skill` 미지원 |
| 하위 에이전트 | `tools/task.rs`(동기 배치), `rpc.rs:863` `get_subagents` 빈 배열 스텁 | `run_in_background`(`st_...`), `task_send`, `task_output`, `task_cancel` | 비동기 태스크 전무 |
| 셸/모니터 | `shell_host.rs`, `jobs.rs`(`proc://`), `wait.rs` | `bash_output`, `kill_bash`, `monitor(command/path/target)` | 3종 도구 부재 |
| 장기 기억 | `/memory` 스텁 | `memory(create/str_replace/insert/delete/rename/update_description + git 커밋)`, `memory_apply_patch` | `crates/omp-memory` 미통합 |
| 작업 큐 | 없음 | `workpool(create/push/close/inspect/cancel/yield)` | 부재 |
| 질문·목표 | `ask({html}|questions[])`, `goal({op})` 단일 | `ask_user_question({questions:[{header,question,multiSelect,options}],waitForAnswer,required})`, `create_goal/get_goal/update_goal` | 도구 형태 불일치 |
| 종료 신호 | `runtime.rs:1641` `{"type":"session_settled"}` 만 stdout; `ext.rs:1598` `agent_settled`는 확장 호스트 내부 한정 | `relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186` — stdout `{"type":"agent_settled"}` 대기 | 신호명 불일치 |
| 설정 루트 | `~/.omp`(`crates/omp-settings/src/lib.rs`) | `~/.omo`(`omo.jsonc`, 프로필 `fleet-small`/`fleet-medium`) | 루트 이원화 |
| 세션 기록 | omp JSONL v3(`crates/omp-session/src/entries.rs`, 256B title slot + camelCase) | `session-patterns.ts:47-76` — `{timestamp, message:{role,content:[{type:"toolCall",name,arguments,id}],toolCallId,isError}}` | 필드 매핑 |

## 2. 상세 계약 및 데이터/타입 명세

**2.1 RPC 오류 프레임 (필수 직렬화)**

```rust
// rpc.rs:203-226 response() 확장
#[derive(Serialize)]
struct ResponseFrame<'a> {
    #[serde(rename = "type")] kind: &'static str, // "response"
    id: &'a Value,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")] data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")] error: Option<String>,
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    error_code: Option<String>, // CmdError::code 와 동일 값
}
```

`CmdError`에 `code: String` 필드를 추가하고 `rpc.rs:1094` 미지원 분기는 `CmdError::new("unknown_command", format!("Unknown command: {other}"))`로 변경한다.

**2.2 lane-hold 계약 (lane-hold.ts:29-76)**

- `set_client_info({width:80, capabilities:["interactive_user"]})`
- `interrupt({intent:"stop"|"abort"})`
- `get_state({paused, runPaused, pauseSeq})`
- 오류 코드: `stop_requires_interactive_client`(capabilities 미선언), `connection_role_fixed`(역할 변경 시)
- `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`
- `reload()` → `{cancelled:false}`
- `resume_session()` → `session_resumed` 이벤트 발행 후 `get_state`가 `paused:false, runPaused:false`

**2.3 세션 이동 계약**

- 호스트 기동: `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`
- `move_session {op: "prepare"|"export"|"import"|"status"|"finish"|"cancel", moveId, sessionPath, bundleDir, cwd, from, answer}`
- `release_session({sessionId, reason:"move", moveId})` → `{session_path}`
- `extension_request({sessionId?, name, data?})` → `api.rpc.handle(name, handler)` 라우팅 대상: `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`
- 확장 이벤트: `session_move_export` / `session_move_import` (`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})`

**2.4 CLI 타입**

```rust
// cli.rs:97-166 확장
#[arg(long = "skill", value_name = "DIR")]
skill_paths: Vec<PathBuf>,        // 반복 가능, gateway.ts:1167 대응
#[arg(long = "no-model-fallback")]
no_model_fallback: bool,          // model-routing.ts:658
#[arg(long = "no-recommended-models")]
no_recommended_models: bool,      // model-routing.ts:658
#[arg(long = "multi-session")]
multi_session: bool,              // move-host.ts:37
#[arg(long = "session-runtime", value_name = "KIND")]
session_runtime: Option<String>,  // "in-process"
```

기존 `--skills <CSV>`(`cli.rs:129,309`)는 유지하되 `--skill`(경로)과 병합한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

**3.1 디스패치 코드 확장 (rpc.rs:593)**

```rust
pub async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult {
    let name = cmd["command"].as_str().unwrap_or_default();
    match name {
        "set_client_info" => { /* capabilities 저장, role 고정 플래그 */ }
        "interrupt"       => { /* intent:"stop"|"abort" → pauseSeq++ */ }
        "get_state"       => Ok(json!({"paused":..,"runPaused":..,"pauseSeq":..})),
        "get_protocol_info" => Ok(json!({
            "protocolVersion":2, "serverVersion": env!("CARGO_PKG_VERSION"),
            "engine":"oh-my-tau", "commands": supported_commands()})),
        "reload"          => Ok(json!({"cancelled": false})),
        "resume_session"  => { emit_event("session_resumed", json!({})); Ok(json!({"ok":true})) }
        "move_session"    => move_session_op(rt, cmd).await,
        "release_session" => Ok(json!({"session_path": session_path})),
        "extension_request" => route_extension(rt, cmd).await,
        "task_send"|"task_output"|"task_cancel" => task_broker(rt, name, cmd).await,
        "bash_output"|"kill_bash"|"monitor" => shell_tools(rt, name, cmd).await,
        "workpool"        => workpool_op(rt, cmd).await,
        other => Err(CmdError::new("unknown_command", format!("Unknown command: {other}"))),
    }
}
```

**3.2 역할 고정 검증**

```rust
fn set_client_info(state: &mut LaneState, caps: &[String]) -> Result<(), CmdError> {
    if state.role_locked && state.role != derive_role(caps) {
        return Err(CmdError::new("connection_role_fixed", "role already fixed"));
    }
    if !caps.iter().any(|c| c == "interactive_user") {
        state.stop_blocked = true; // stop 시 stop_requires_interactive_client
    }
    state.caps = caps.to_vec(); state.role_locked = true; Ok(())
}
```

**3.3 종료 신호 이중 발행 (runtime.rs:1641)**

```rust
// 기존 session_settled 유지 + 호환 신호 추가
emit_stdout(json!({"type":"session_settled"}));
emit_stdout(json!({"type":"agent_settled"})); // relay/runtime.ts:59, fleet.ts:258 대응
```

**3.4 비동기 하위 태스크 (tools/task.rs 확장)**

```rust
// run_in_background: st_<ulid> ID 발급, tokio::spawn, 브로커 등록
struct TaskEntry { id: String, handle: JoinHandle<()>, out: Arc<Mutex<String>>, status: TaskStatus }
// task_send(id, message) / task_output(id) -> {status, output} / task_cancel(id)
```

`rpc.rs:863` `get_subagents`는 브로커의 `TaskEntry` 목록을 반환하도록 교체.

**3.5 셸 모니터 (jobs.rs `proc://` 재사용)**

```rust
enum MonitorCond { Command(String), Path(PathBuf), Target(String) }
// wait.rs 폴링 루프에 조건 평가 추가, bash_output/kill_bash는 shell_host.rs 핸들 조회
```

**3.6 memory/workpool/ask·goal**

- `memory` op: `create|str_replace|insert|delete|rename|update_description` → `crates/omp-memory` API 호출 후 `git add/commit`.
- `memory_apply_patch` → 패치 적용 후 커밋.
- `workpool` op: `create|push|close|inspect|cancel|yield`.
- `ask_user_question({questions:[{header,question,multiSelect,options}],waitForAnswer,required})`는 기존 `ask`의 `questions[]` 경로로 위임(어댑터). `create_goal/get_goal/update_goal`은 `goal({op})` 위임.

**3.7 설정 루트 브리지**

```rust
// omp-settings: OMP_HOME 미설정 시 ~/.omo/omo.jsonc 우선 탐색, 없으면 ~/.omp
let root = env::var("OMP_HOME").map(PathBuf::from)
    .unwrap_or_else(|_| pick_existing([dirs::home_dir()?.join(".omo"), dirs::home_dir()?.join(".omp")]));
```

**3.8 세션 JSONL 매핑 (session-patterns.ts:47-76 대응)**

omp v3 `entries.rs` 직렬화 시 `timestamp`, `message.role`, `message.content[].type:"toolCall"`, `name`, `arguments`, `id`, `toolCallId`, `isError` 필드를 보장. camelCase 유지, 256B title slot 불변.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

| # | 기준 | 검증 명령 |
|---|---|---|
| AC-1 | 미지원 명령이 `errorCode` 포함 응답 | `omt --mode rpc <<< '{"id":1,"command":"nope"}' \| jq '.errorCode=="unknown_command"'` |
| AC-2 | `get_protocol_info` 계약 일치 | `omt --mode rpc <<< '{"id":2,"command":"get_protocol_info"}' \| jq '.data \| {protocolVersion,engine,commands}'` |
| AC-3 | capabilities 미선언 stop 차단 | `lane-hold.ts` 시나리오 재생 → `stop_requires_interactive_client` 확인 |
| AC-4 | 역할 변경 차단 | `set_client_info` 2회 상이 caps → `connection_role_fixed` |
| AC-5 | resume 후 상태 | `get_state` → `paused:false && runPaused:false`, `session_resumed` 수신 |
| AC-6 | 이동 플로우 | `omt --mode rpc --multi-session --session-runtime in-process -e ext` 로 `prepare→export→import→finish` 완주, `release_session` → `session_path` |
| AC-7 | CLI 플래그 | `omt --help \| grep -E -- '--skill|--no-model-fallback|--no-recommended-models|--multi-session'` |
| AC-8 | 종료 신호 | `omt --mode rpc ... \| grep '"type":"agent_settled"'` |
| AC-9 | 비동기 태스크 | `run_in_background` → `st_` ID, `task_output`/`task_cancel` 응답 |
| AC-10 | 셸 도구 | `bash_output`, `kill_bash`, `monitor` 3종 RPC 응답 |
| AC-11 | memory | `memory` create→str_replace→git log 1커밋, `memory_apply_patch` 적용 |
| AC-12 | workpool | 6개 op 왕복 |
| AC-13 | 질문/목표 | `ask_user_question`, `create_goal/get_goal/update_goal` 응답 |
| AC-14 | 설정 루트 | `~/.omo/omo.jsonc` 존재 시 우선 로드 |
| AC-15 | 세션 JSONL | `session-patterns.ts` 파서로 파싱 성공 |

전체 회귀: `cargo test -p omp-cli -p omp-settings -p omp-memory -p omp-session && (cd omo-nyang && npm test)`.

## 5. 엣지 케이스 및 실패 복구

1. **`errorCode` 누락 회귀**: `response()`에서 `error_code`가 `None`이면 relay가 실패 원인을 못 읽음 → 단위 테스트로 `success:false` 시 `errorCode` 필수 assert.
2. **capabilities 선언 후 역할 변경**: `connection_role_fixed` 반환하되 상태는 원래 role 유지(부분 적용 금지).
3. **`stop` vs `abort`**: `intent:"stop"`은 pause만, `"abort"`는 run 취소 + `pauseSeq` 증가. `resume_session`은 두 경우 모두 `paused:false`로 정규화.
4. **이동 중 세션 잠금**: `move_session op="prepare"` 후 `release_session` 전 `get_state`는 `paused:true`
