# 테스트 조각 07: `task`(`run_in_background:true`) 및 `task_send`·`task_output`·`task_cancel`, 사망 자식 보유 시 완료 차단 테스트 명세 (test-07-unit-task-send-output-cancel)

## 1. 목적 및 코드 근거 (File & Line)

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어지므로, `task_send`/`task_output`/`task_cancel`이 이 분기에 걸리지 않음을 테스트로 고정한다.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226` `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립한다. 실패 시 `error`와 `errorCode`를 동시 직렬화해야 `omo-nyang/src/relay/move-host.ts:4-7, 61`의 `MoveHost` 파서가 `errorCode`를 잃지 않는다.
- 현행 격차: `crates/omp-cli/src/tools/task.rs`(+`crates/omp-task`)는 `context` + `tasks[]` 배치 **동기** 실행만 제공하며 `run_in_background`, `st_...` ID, `task_send`, `task_output`, `task_cancel`이 없다. 또한 `crates/omp-cli/src/rpc.rs:863`의 `get_subagents`는 빈 배열 스텁이다.
- 백그라운드 인프라: Phase 2의 `crates/omp-cli/src/shell_host.rs`, `jobs.rs`(`proc://` 핸들), `wait.rs`가 존재하나 `bash_output`/`kill_bash`/`monitor` 계열 도구가 없어 재사용 지점만 확보된 상태다.
- 종료 신호: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고 `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 본 조각은 "자식이 살아있거나 사망 후 미수거 상태면 settle을 보내지 않는다"는 게이트를 단위 테스트로 고정한다.

## 2. 상세 계약 및 데이터/타입 명세

도구 스키마(신규, `crates/omp-cli/src/tools/task.rs` 확장):

```rust
pub struct TaskArgs {
    pub context: Option<String>,
    pub tasks: Vec<TaskSpec>,          // 기존 동기 경로 유지
    pub run_in_background: Option<bool>, // true면 tasks[0]만 허용
}
pub struct TaskSpec { pub prompt: String, pub description: Option<String>, pub subagent: Option<String> }

// 신규 3종
pub struct TaskSendArgs   { pub task_id: String, pub input: String }
pub struct TaskOutputArgs { pub task_id: String, pub since: Option<u64>, pub wait_ms: Option<u64> }
pub struct TaskCancelArgs { pub task_id: String, pub reason: Option<String> }
```

응답 페이로드(`data`):

```json
{"task_id":"st_01J8Z9K3Q7","status":"running","pid":48211}
{"task_id":"st_01J8Z9K3Q7","accepted":true,"seq":7}
{"task_id":"st_01J8Z9K3Q7","status":"exited","exit_code":1,"chunks":[{"seq":8,"stream":"stderr","text":"..."}],"next_since":9}
{"task_id":"st_01J8Z9K3Q7","cancelled":true}
```

상태 기계: `pending -> running -> exited -> reaped`, 종료 계열은 `completed | failed | cancelled`. `exited`(자식 사망, stdout 미수거)는 **완료 차단 상태**로 취급한다. settle 게이트는 `live_children + exited_unreaped == 0`일 때만 열린다.

에러 코드(`CmdError` + `errorCode` 동시 기록): `task_not_found`, `task_id_invalid`, `task_already_terminal`, `task_output_timeout`, `settle_blocked_by_children`, `background_task_limit`(동시 8개).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/tools/task.rs
pub async fn task(args: TaskArgs, rt: &Arc<Runtime>) -> Result<Value, CmdError> {
    if args.run_in_background == Some(true) {
        if args.tasks.len() != 1 {
            return Err(CmdError::new("background task accepts exactly one task")
                .with_code("background_task_limit"));
        }
        let id = format!("st_{}", ulid::Ulid::new());
        let handle = rt.jobs.spawn_proc(&id, &args.tasks[0])?; // jobs.rs proc:// 재사용
        rt.task_registry.insert(id.clone(), handle);
        return Ok(json!({"task_id": id, "status": "running", "pid": handle.pid()}));
    }
    run_batch_sync(args.context, args.tasks).await // 기존 동기 경로 불변
}

// settle 게이트 (runtime.rs:1641 직전 삽입)
fn may_settle(reg: &TaskRegistry) -> bool {
    reg.iter().all(|t| t.state() == TaskState::Reaped)
}
```

테스트 본문(요지): `crates/omp-cli/tests/task_background.rs`

1. `dispatch(&rt, json!({"cmd":"task","run_in_background":true,...}), "rpc")` → `success:true`, `data.task_id`가 `st_` 프리픽스, `get_subagents`(rpc.rs:863)가 더 이상 빈 배열이 아님을 확인.
2. `task_output`을 `since:0`으로 호출해 청크 수집, `next_since` 단조 증가 검증.
3. 자식이 `exit(1)`로 사망 후 `task_output` 미호출 상태에서 settle 프레임이 **부재**함을 `may_settle == false`로 확인하고, 수거 후 `true`로 전이.
4. `task_cancel` 후 `task_output`이 `status:"cancelled"`를 반환하고 재호출 시 `task_already_terminal`.
5. 미존재 ID는 `task_not_found` + `errorCode` 동시 직렬화(`rpc.rs:203-226` 경로) 검증.

TS 측(`omo-nyang/src/relay/move-host.ts:4-7` 파서 대응) 검증: 실패 프레임 `{"type":"response","id":"t1","success":false,"error":"...","errorCode":"task_not_found"}`를 파서에 통과시켜 `errorCode`가 보존되는지 단언한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `task` + `run_in_background:true`가 `st_` ID를 반환하고 프로세스가 실제로 병렬 실행된다(부모 블로킹 없음, 경과 < 200ms).
- AC2: `task_send` → `task_output` 왕복에서 입력이 자식 stdin에 도달하고 출력 seq가 누락 없이 1..N 연속.
- AC3: 자식 사망 + 미수거 상태에서 settle 프레임 0건, 수거 후 정확히 1건.
- AC4: `task_cancel` 후 좀비 프로세스 0건(`waitpid` 기준).
- AC5: 모든 실패 응답이 `error` + `errorCode` 동시 포함.

```bash
cargo test -p omp-cli --test task_background -- --nocapture
cargo test -p omp-task
cargo clippy -p omp-cli -- -D warnings
cd omo-nyang && npx vitest run src/relay/move-host.test.ts
```

## 5. 엣지 케이스 및 실패 복구

- 동시 백그라운드 8개 초과: `background_task_limit`, 기존 태스크는 영향 없음(부분 실패 롤백).
- `task_output`의 `wait_ms` 초과: `task_output_timeout`이되 상태는 `running` 유지, `next_since`는 불변(재시도 멱등).
- 자식이 출력 없이 즉시 사망: `chunks:[]`, `exit_code`만 반환, 게이트는 수거 전까지 닫힘.
- 부모 세션 `interrupt({intent:"abort"})`(`omo-nyang/src/lane-hold.ts:29-76`): 등록된 모든 `st_` 태스크에 SIGTERM 후 SIGKILL, `task_cancel`과 동일 경로로 정리, `paused:true` 유지.
- `reload()`/`resume_session()` 후 레지스트리 재구성: 살아있는 PID는 `proc://` 핸들로 재부착, 재부착 실패 시 `exited_unreaped`로 강등해 settle 차단 유지.
- RPC 클라이언트 조기 종료: stdout 파이프 EPIPE 시 태스크는 유지하되 60초 후 자동 cancel(누수 방지).
- 알 수 없는 `cmd` 문자열은 여전히 `rpc.rs:1094`의 `Unknown command` 경로를 타야 하며, 신규 3종은 해당 분기에 도달하지 않음을 회귀 테스트로 고정.
