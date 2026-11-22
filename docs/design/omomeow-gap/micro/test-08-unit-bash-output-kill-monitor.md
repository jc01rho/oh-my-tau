# 테스트 조각 08: `bash_output`·`kill_bash`(프로세스 그룹 트리 종료)·`monitor`(`command`/`path`/`persistent`) 비폴링 이벤트 테스트 명세 (test-08-unit-bash-output-kill-monitor)

## 1. 목적 및 코드 근거 (File & Line)

- `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` 가 RPC 명령의 단일 진입점이다. 신규 3개 명령(`bash_output`, `kill_bash`, `monitor`)은 반드시 이 `match`에 arm을 추가해야 하며, 추가하지 않으면 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` 로 낙하한다.
- 응답 프레임 조립은 `crates/omp-cli/src/rpc.rs:203-226` `response()` 이며, 소비자 `omo-nyang/src/relay/move-host.ts:4-7, 61` 은 `{type:"response", id, success, data?, error?, errorCode?}` 를 파싱한다. 따라서 실패 시 `code`(내부 enum)와 `errorCode`(문자열)를 **동시에** 직렬화해야 한다.
- 격차 근거: tau는 Phase 2에서 `crates/omp-cli/src/tools/shell_host.rs`, `crates/omp-cli/src/tools/jobs.rs`(`proc://` 스킴), `crates/omp-cli/src/tools/wait.rs` 를 도입했으나 OmOMeow가 호출하는 `bash_output`, `kill_bash`, `monitor(command/path/target)` 도구는 존재하지 않는다. `crates/omp-cli/src/rpc.rs:863` `get_subagents` 가 빈 배열 스텁인 것과 동일한 "스텁 잔존" 유형의 결함이므로, 본 조각은 스텁 미검출 자체를 실패로 간주한다.
- 세션 이동/보류 계약과의 접점: `omo-nyang/src/relay/move-host.ts:37-39` 는 `--multi-session --session-runtime in-process` 로 호스트를 띄우고 `move-operations.ts:104-225` 의 `prepare|export|import|status|finish|cancel` 을 수행한다. `release_session({sessionId, reason:"move", moveId})` 시점에 해당 세션의 job/monitor가 누수되면 이동 후 좀비 PG가 남으므로, 본 조각은 세션 스코프 정리를 계약으로 고정한다.
- `omo-nyang/src/lane-hold.ts:29-76` 의 `set_client_info`/`interrupt`/`get_state`/`resume_session` 흐름과 무관하게, `interrupt({intent:"abort"})` 는 진행 중 job을 종료시키지 않고 보류만 하므로, job 수명주기는 `session_settled`(`crates/omp-cli/src/runtime.rs:1641`) 및 `agent_settled`(`crates/omp-cli/src/ext.rs:1598`) 경계에서 별도 검증한다.

## 2. 상세 계약 및 데이터/타입 명세

```rust
// crates/omp-cli/src/tools/jobs.rs (신규/확장)
pub struct JobId(pub String);                       // "job_" + 12 hex
pub struct Pgid(pub i32);
pub struct JobRecord {
    pub id: JobId, pub pgid: Pgid, pub cmd: String, pub cwd: PathBuf,
    pub state: JobState, pub started_ms: u64,
    pub out: OutputRing,                            // seq 기반 링 버퍼
    pub exit: Option<ExitInfo>,
}
pub enum JobState { Running, Exited(i32), Signaled(i32), Killed, TimedOut }
pub struct ExitInfo { pub code: Option<i32>, pub signal: Option<i32> }
pub struct OutputChunk { pub seq: u64, pub stream: Stream, pub data: String }
pub enum Stream { Stdout, Stderr }
pub enum JobEvent {
    Started { id: JobId, pgid: Pgid },
    Output  { id: JobId, chunk: OutputChunk },
    Exited  { id: JobId, exit: ExitInfo },
}
```

- `proc://` URI: `proc://<job_id>/stdout?seq=<u64>` — `jobs.rs` 의 기존 스킴을 재사용하고 `seq`를 커서로 쓴다.
- `bash_output` 요청: `{"cmd":"bash_output","id":"job_ab12cd34ef56","seq":0,"max_bytes":65536,"wait_ms":0}` → `data = {"id","chunks":[{"seq","stream","data"}],"next_seq","state","truncated":bool}`.
- `kill_bash` 요청: `{"cmd":"kill_bash","id":"job_...","signal":"TERM","tree":true,"grace_ms":2000}` → `data = {"id","killed_pgids":[12345,12346],"escalated":true,"state":"Killed"}`.
- `monitor` 요청: `{"cmd":"monitor","op":"add","spec":{"kind":"command","pattern":"cargo","persistent":true,"debounce_ms":50}}` 또는 `{"kind":"path","path":"/work/src","persistent":false}`. `op ∈ {add, list, remove}`.
- 비폴링 이벤트 프레임(stdout push): `{"type":"monitor_event","id":"mon_...","kind":"command"|"path","matched":true,"pid":4242,"path":"/work/src/main.rs","seq":7}`. 클라이언트는 `wait_ms` 재호출 없이 이 프레임만으로 통지받아야 한다(테스트의 핵심 단언).
- 오류 코드: `JOB_NOT_FOUND`, `JOB_ALREADY_EXITED`, `MONITOR_PATH_MISSING`, `MONITOR_LIMIT`, `SIGNAL_DENIED`. 모두 `error`(메시지) + `errorCode`(위 문자열) 동시 직렬화.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/rpc.rs:593 dispatch 내 arm 추가
"bash_output" => tools::jobs::rpc_bash_output(rt, cmd).await,
"kill_bash"   => tools::jobs::rpc_kill_bash(rt, cmd).await,
"monitor"     => tools::monitor::rpc_monitor(rt, cmd).await,
```

```rust
// 프로세스 그룹 트리 종료 (setsid 후 killpg)
let pgid = child_pgid(&job)?;                       // setsid()로 pgid == child pid 보장
libc::kill(-pgid.0, libc::SIGTERM);                 // 그룹 전체
for _ in 0..(grace_ms / 50) { if group_empty(pgid) { break } sleep(50ms) }
if !group_empty(pgid) {
    libc::kill(-pgid.0, libc::SIGKILL); escalated = true;
}
assert!(scan_proc_children(pgid).is_empty());       // /proc/*/stat pgrp 스캔으로 손자 프로세스 0 확인
```

```rust
// monitor: 폴링 금지 검증 — notify 크레이트 inotify + /proc 워처 이벤트 구동
let (tx, mut rx) = broadcast::channel::<MonitorEvent>(256);
// path: notify::recommended_watcher → EventKind::Create|Modify → tx.send(...)
// command: procfs 폴링 대신 eBPF 불가 환경에서는 inotify(/proc 미지원) 대체로
//          pidfd + 재기동 시에만 1회 스캔. 폴링 카운터를 계측해 상한을 단언.
static PROC_SCAN_COUNT: AtomicUsize;                // 테스트에서 상한 단언용
```

```ts
// omo-nyang/src/relay/move-host.ts 확장: 비폴링 수신 단언
const ev = await nextFrame(host.stdout, (f) => f.type === "monitor_event", 2000);
expect(ev.matched).toBe(true);
expect(await countCalls("bash_output")).toBe(0);     // 이벤트만으로 통지됨
```

테스트 파일: `crates/omp-cli/tests/unit_bash_output.rs`, `crates/omp-cli/tests/unit_kill_tree.rs`, `crates/omp-cli/tests/unit_monitor_events.rs`.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-cli --test unit_bash_output` — seq 커서 재개, `truncated` 플래그, `next_seq` 단조 증가 통과.
2. `cargo test -p omp-cli --test unit_kill_tree` — `sh -c 'sleep 300 & sleep 300'` 기동 후 `kill_bash` 1회로 PG 내 전 프로세스 소멸, `escalated`가 2초 초과 시에만 true.
3. `cargo test -p omp-cli --test unit_monitor_events` — 파일 생성 1회에 `monitor_event` 1건 push, `PROC_SCAN_COUNT <= 1`, `persistent:false`는 1회 후 자동 제거.
4. `cargo test -p omp-cli rpc::tests::unknown_command` — 신규 3개 명령이 `rpc.rs:1094` 기본 arm에 도달하지 않음(`Unknown command: bash_output` 미발생) 확인.
5. `pnpm --filter omo-nyang test -t "monitor_event"` — `move-host.ts:4-7,61` 파서가 `errorCode` 포함 실패 프레임을 정상 해석.
6. `cargo test -p omp-cli --test unit_kill_tree session_release_cleanup` — `release_session({reason:"move"})` 후 PG 잔존 0.

## 5. 엣지 케이스 및 실패 복구

- 미존재 id: `bash_output`/`kill_bash` 모두 `JOB_NOT_FOUND` + `errorCode` 동시 반환, 프로세스 신호 미발생.
- 이미 종료된 job에 `kill_bash`: 멱등 성공(`state:"Exited"`, `killed_pgids:[]`), `JOB_ALREADY_EXITED`는 `strict:true`일 때만.
- PGID 재사용 경합: 종료 후 `killpg` 재호출 금지 — `JobRecord.exit` 확정 시 PGID를 무효화하고 `(pid, starttime)` 쌍으로 검증.
- 출력 링 버퍼 오버플로: `seq` 공백 발생 시 `truncated:true` + `next_seq`로 리셋, 클라이언트는 `seq` 재요청.
- path monitor: 경로 부재 시 `MONITOR_PATH_MISSING`, 감시 중 rename/재생성은 inode 교체로 재등록, 심볼릭 링크 루프는 1회 해석 후 실패 처리.
- command monitor 오탐: 자기 프로세스(`omp` 자신) 및 좀비는 `matched`에서 제외, debounce로 중복 이벤트 병합.
- monitor 개수 상한 초과: `MONITOR_LIMIT`, 기존 감시는 유지(원자적 거부).
- 세션 종료/이동: `session_settled`(`runtime.rs:1641`) 및 `release_session` 시 해당 세션의 job 전부 `SIGTERM→SIGKILL`, monitor watcher drop. `interrupt({intent:"abort"})`는 job을 죽이지 않음을 별도 단언.
- 확장 호스트 경계: `agent_settled`는 `ext.rs:1598` 내부 전용이므로 monitor 정리 트리거로 사용하지 않고, RPC 세션 스코프만 신뢰한다.
- 런타임 drop/패닉: `Drop for JobRegistry`에서 잔존 PG 일괄 종료, 실패 시 `tracing::error!` 후 계속(누수 리포트만 남김).
