# 리뷰 조각 05: `kill_bash`·`task_cancel`·`monitor` 타임아웃 시 `NewProcessGroup`(`-pgid`) 시그널 전파 및 고아 프로세스 방지 감사 (rev-05-process-tree-and-sandbox-audit)

## 1. 목적 및 코드 근거 (File & Line)

- 감사 대상은 OmOMeow가 호출하지만 tau에 아직 없는 3계열 도구다(근거 5): `bash_output`/`kill_bash`/`monitor` 부재, `task_send`/`task_output`/`task_cancel` 부재, `crates/omp-cli/src/rpc.rs:863`의 `get_subagents`는 빈 배열 스텁이다.
- 기반 인프라는 Phase 2의 `crates/omp-cli/src/tools/shell_host.rs`, `crates/omp-cli/src/tools/jobs.rs`(`proc://` 핸들), `crates/omp-cli/src/tools/wait.rs`이다.
- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593 async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094 other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 응답 조립: `crates/omp-cli/src/rpc.rs:203-226 response()`. 파서 측은 `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 읽으므로 실패 시 `code`와 `errorCode`를 함께 직렬화해야 한다.
- 보류/재개 계약: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`.
- 종료 신호 정합: tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout에 쓰고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598` 확장 호스트 내부에만 보낸다. 반면 `omo-nyang/src/relay/runtime.ts:59`는 `agent_settled`를 기다린다. 프로세스 정리는 settled 이벤트 이전에 완료되어야 한다.
- 핵심 위험: `NewProcessGroup`(`-pgid`) 시그널이 리더 PID 단일 타깃으로 전파되면 `&`, `nohup`, `setsid` 우회 자식이 고아로 잔존하고, `monitor` 타임아웃 경로에서 이 누수가 반복 발생한다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 프로세스 그룹 표현 (기존 `shell_host.rs` 확장)
```rust
pub struct NewProcessGroup {
    pub pgid: i32,            // 리더 pid == pgid (setpgid(0,0) 결과)
    pub leader: u32,
    pub started_at_ms: u64,   // PID 재사용 판별용
}
```
spawn 시 Unix는 `std::os::unix::process::CommandExt::process_group(0)`로 그룹을 만들고, `pre_exec`에서 `prctl(PR_SET_PDEATHSIG, SIGKILL)`을 설정해 부모 사망 시 자식 자동 회수 경로를 확보한다.

### 2.2 도구 JSON 스키마
| 도구 | 요청 | 응답 `data` |
|---|---|---|
| `kill_bash` | `{"bash_id":"st_...","signal":"TERM"\|"KILL","grace_ms":2000}` | `{"killed":true,"escalated":false,"reaped":3}` |
| `bash_output` | `{"bash_id":"st_...","filter":null,"timeout_ms":30000}` | `{"output":"...","exitCode":null,"running":true}` |
| `monitor` | `{"command":"...","path":"...","target":"...","timeout_ms":60000,"interval_ms":500}` | `{"matched":true,"timedOut":false,"sample":"..."}` |
| `task_cancel` | `{"task_id":"st_...","wait":true}` | `{"cancelled":true,"state":"cancelled"}` |

### 2.3 오류 코드 (RPC `errorCode` + `code` 동시 직렬화)
`unknown_bash_id`, `process_group_missing`, `kill_escalation_failed`, `orphan_detected`, `task_not_found`, `task_cancel_race`, `monitor_timeout`. `monitor` 타임아웃은 오류가 아니라 `{"timedOut":true,"matched":false,"exit":"killed"}`로 성공 응답한다(그룹 종료 완료 후).

### 2.4 타임아웃 계약
`monitor.timeout_ms` 초과 시 조건 미충족이어도 `terminate_group`을 먼저 완료하고 응답한다. 즉 응답 시점에 `-pgid` 그룹은 비어 있어야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS)

```rust
// crates/omp-cli/src/tools/kill_bash.rs (신규)
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

pub async fn terminate_group(g: &NewProcessGroup, grace: Duration) -> Result<TermReport, CmdError> {
    let pg = Pid::from_raw(g.pgid);
    killpg(pg, Signal::SIGTERM)
        .map_err(|_| CmdError::new("process_group_missing").with_code("process_group_missing"))?;
    if wait_group_empty(g, grace).await { return Ok(TermReport { escalated: false, ..Default::default() }); }
    killpg(pg, Signal::SIGKILL)?;                       // 음수 pgid = 그룹 전체
    if !wait_group_empty(g, Duration::from_secs(5)).await {
        return Err(CmdError::new("orphan_detected").with_code("orphan_detected"));
    }
    reap_children(g.pgid)?;                             // waitpid(-pg
