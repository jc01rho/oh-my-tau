# QA 시나리오 07: 장기 빌드 백그라운드 셸(`bash_output`, `kill_bash`) 및 `monitor` 조건 달성 시 세션 자동 깨우기 검증 (qa-07-background-shell-and-monitor-wake)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: 10분 이상 걸리는 빌드/테스트를 백그라운드로 넘긴 뒤, 폴링(`bash_output`), 강제 종료(`kill_bash`), 조건 감시(`monitor`)로 세션을 자동으로 깨우는 경로를 end-to-end로 검증한다.
- 격차 근거: `crates/omp-cli/src/tools/`에 Phase 2 산출물인 `shell_host.rs`, `jobs.rs`(`proc://` 스킴), `wait.rs`는 존재하나 OmOMeow가 호출하는 `bash_output`, `kill_bash`, `monitor`(command/path/target 조건) 도구가 없다. 즉 셸 호스트 인프라는 있고 도구 표면만 비어 있다.
- RPC 표면 근거: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어지므로, 신규 도구는 반드시 이 match 이전에 arm을 추가해야 한다.
- 응답 프레임 근거: `rpc.rs:203-226`의 `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립하고, `omo-nyang/src/relay/move-host.ts:4-7, 61`의 `MoveHost`가 동일 스키마를 파싱한다. 실패 시 `code`와 `errorCode`를 함께 직렬화해야 클라이언트가 분기 가능하다.
- 깨우기 신호 근거: `omo-nyang`은 RPC stdout에서 `{"type":"agent_settled"}`를 기다린다(`relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`). 반면 tau는 `runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout에 쓰고 `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 이 불일치가 시나리오 07의 핵심 실패 지점이다.
- 상태 계약 근거: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`, `resume_session()` 후 `paused:false, runPaused:false`.
- 이벤트 계약 근거: `omo-nyang/src/relay/session-move-extension.ts:116-139`의 `api.events.on("wake_source_state", {source, activeCount})` 및 `extension_request({sessionId?, name, data?})` → `api.rpc.handle(name, handler)` 라우팅.

## 2. 상세 계약 및 데이터/타입 명세

신규 도구 3종은 `crates/omp-cli/src/tools/`에 배치하고 `dispatch` arm에서 `kind`를 보존한 채 라우팅한다.

```rust
// tools/bash_output.rs (신규)
pub struct BashOutputArgs {
    pub job_id: String,        // jobs.rs의 "proc://<uuid>" 형식
    pub tail_lines: Option<u32>,   // 기본 200
    pub filter: Option<String>,    // 정규식, stdout/stderr 동시 적용
    pub wait_ms: Option<u64>,      // 0이면 즉시 스냅샷, >0이면 최대 대기 후 반환
}
pub struct BashOutputResult {
    pub job_id: String,
    pub state: JobState,       // Running | Exited { code: Option<i32> } | Killed { signal: String }
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

// tools/kill_bash.rs (신규)
pub struct KillBashArgs { pub job_id: String, pub signal: Option<String> } // 기본 "SIGTERM", "SIGKILL" 허용
pub struct KillBashResult { pub job_id: String, pub killed: bool, pub exit_code: Option<i32> }

// tools/monitor.rs (신규)
pub enum MonitorCondition {
    Command { command: String, expect_exit: i32 },
    Path { path: PathBuf, event: PathEvent },      // Created | Modified | Removed
    Target { target: String, matcher: String },    // 로그 라인 정규식 매칭
}
pub struct MonitorArgs {
    pub job_id: Option<String>,
    pub condition: MonitorCondition,
    pub timeout_ms: u64,       // 기본 600_000, 0은 무제한 금지(상한 3_600_000)
    pub poll_interval_ms: u64, // 기본 500
}
pub struct MonitorResult { pub satisfied: bool, pub reason: String, pub observed_at_ms: u64 }
```

깨우기 페이로드는 기존 계약을 재사용한다. `monitor`가 조건을 만족하면 stdout에 아래 두 프레임을 순서대로 기록한다.

```json
{"type":"wake_source_state","source":"monitor","activeCount":0}
{"type":"agent_settled"}
```

`activeCount`는 해당 세션에서 아직 Running인 `proc://` job 수이며, 0이 될 때만 `agent_settled`를 발행한다(중복 settle 방지). RPC 응답은 `rpc.rs:203-226` 스키마를 그대로 따르고, 실패는 `CmdError::new(...)`에 `code`(`JOB_NOT_FOUND`, `MONITOR_TIMEOUT`, `MONITOR_ALREADY_SETTLED`, `SHELL_HOST_UNAVAILABLE`)와 `errorCode`를 함께 채운다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust 디스패치 추가(`rpc.rs:1094` 이전):

```rust
"bash_output" => tools::bash_output::handle(rt, cmd).await,
"kill_bash"   => tools::kill_bash::handle(rt, cmd).await,
"monitor"     => tools::monitor::handle(rt, cmd).await,
```

`monitor` 루프 의사코드:

```rust
let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
let mut tick = tokio::time::interval(Duration::from_millis(args.poll_interval_ms));
loop {
    if Instant::now() >= deadline { return Err(CmdError::with_code("MONITOR_TIMEOUT", ...)); }
    tick.tick().await;
    if let Some(job) = jobs::get(&args.job_id) {
        if job.state.is_terminal() && matches!(args.condition, MonitorCondition::Command{..}) { break; }
        if evaluate(&args.condition, &job).await? { break; }
    }
    if session_paused(rt).await {  // lane-hold.ts:29-76의 get_state와 동일 소스
        emit(stdout, json!({"type":"wake_source_state","source":"monitor","activeCount":running_count(rt)}));
        return Ok(paused_ack());   // runPaused 유지, settle 보류
    }
}
emit(stdout, json!({"type":"wake_source_state","source":"monitor","activeCount":running_count(rt)}));
if running_count(rt) == 0 { emit(stdout, json!({"type":"agent_settled"})); }
```

TS 하네스 검증(시나리오 러너):

```ts
// 1) 보류/개시 계약
await rpc("set_client_info", { width: 80, capabilities: ["interactive_user"] });
const { job_id } = await rpc("exec_shell", { cmd: "cargo build --release", background: true }); // proc://...
// 2) 폴링
const snap = await rpc("bash_output", { job_id, tail_lines: 100, wait_ms: 2000 });
assert(snap.state === "Running" || snap.state.startsWith("Exited"));
// 3) 조건 감시 → 자동 깨우기
const mon = rpc("monitor", { job_id, condition: { Path: { path: "target/release/app", event: "Created" } }, timeout_ms: 600000 });
const frame = await waitForFrame((f) => f.type === "agent_settled", 660000); // runtime.ts:59 계약
assert(frame.type === "agent_settled");
// 4) 강제 종료 경로
const k = await rpc("kill_bash", { job_id, signal: "SIGKILL" });
assert(k.killed === true);
const after = await rpc("get_state", {});
assert(after.paused === false && after.runPaused === false);
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `bash_output`, `kill_bash`, `monitor`가 `dispatch`에서 인식되고 `Unknown command`(`rpc.rs:1094`)가 발생하지 않는다.
- AC2: 실패 응답이 `{type:"response", id, success:false, error, code, errorCode}` 형태로 `move-host.ts:4-7` 파서를 통과한다.
- AC3: `monitor` 조건 만족 시 stdout에 `wake_source_state` 다음 `agent_settled`가 정확히 1회 기록된다(`runtime.rs:1641`의 `session_settled`와 병존해도 무해).
- AC4: `kill_bash` 후 5초 내 `state=Killed`, `get_state`가 `paused:false, runPaused:false`(`lane-hold.ts:29-76`).
- AC5: 보류 중(`interrupt({intent:"stop"})`) `monitor` 만족 시 `agent_settled` 미발행, `wake_source_state`만 발행.

```bash
cargo test -p omp-cli --test qa07_background_shell
cargo run -p omp-cli -- --mode rpc --multi-session --session-runtime in-process -e ./ext/monitor.js \
  < fixtures/qa07_long_build.jsonl | tee /tmp/qa07.out
grep -c '"type":"agent_settled"' /tmp/qa07.out        # 기대값 1
jq -e 'select(.type=="response" and .success==false) | has("code") and has("errorCode")' /tmp/qa07.out
jq -e 'select(.type=="wake_source_state") | .source=="monitor" and .activeCount>=0' /tmp/qa07.out
```

## 5. 엣지 케이스 및 실패 복구

- 존재하지 않는 `job_id`: `JOB_NOT_FOUND` 반환, 셸 호스트 상태 불변. `proc://` 접두사 누락도 동일 코드로 정규화.
- 이미 종료된 job에 `kill_bash`: `killed:false` + `exit_code` 실제값 반환(에러 아님). 재시도 루프 유발 방지를 위해 성공 프레임으로 처리.
- `monitor` 타임아웃: `MONITOR_TIMEOUT` + `observed_at_ms`, job은 계속 Running 유지하고 감시 태스크만 해제.
- 동시 `monitor` 2건: 두 번째는 `MONITOR_ALREADY_SETTLED`로 거부하거나 동일 조건이면 기존 핸들 반환. 중복 `agent_settled` 발행 금지.
- 보류 중 조건 만족: `lane-hold.ts` 계약상 `runPaused`가 참이면 settle을 보류하고 `resume_session()` 이후 재평가하여 1회만 settle.
- `connection_role_fixed`: `capabilities` 선언 후 역할 변경 시도 시 에러. 시나리오 러너는 세션당 `set_client_info` 1회만 호출.
- 셸 호스트 미기동(`shell_host.rs` 초기화 실패): `SHELL_HOST_UNAVAILABLE`로 즉시 실패, `wait.rs` 폴백 경로로 안내.
- 세션 이동 중(`move_session op="export"`) 감시 충돌: `release_session({reason:"move"})` 이후 `monitor` 태스크를 취소하고 `wake_source_state`의 `activeCount`를 0으로 재계산.
- 프로세스 고아화: `kill_bash` 실패 시 `proc://` 레지스트리에서 좀비 정리 후 `Killed`로 강제 마킹하고 사유를 `reason`에 기록.
