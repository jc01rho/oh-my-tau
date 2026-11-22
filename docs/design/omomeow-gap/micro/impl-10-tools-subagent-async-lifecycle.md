# 구현 조각 10: `crates/omp-task`·`crates/omp-cli/src/tools/task.rs` 비동기 `st_...` 세션 레지스트리와 `task_send`·`task_output`·`task_cancel` 구현 명세 (impl-10-tools-subagent-async-lifecycle)

## 1. 목적 및 코드 근거 (File & Line)

현재 tau의 하위 작업 실행은 `crates/omp-cli/src/tools/task.rs` + `crates/omp-task`의 `context` + `tasks[]` 배치 동기 실행뿐이다. OmOMeow는 `run_in_background: true`가 돌려주는 `st_...` ID와 `task_send`/`task_output`/`task_cancel` 3종 도구를 호출하므로, 도구 표면과 RPC 표면을 동시에 확장한다.

- `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`
- `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`: 신규 명령 3종은 반드시 이 arm 앞에 추가
- `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립하며, `omo-nyang/src/relay/move-host.ts:4-7,61`이 동일 스키마를 파싱하므로 실패 시 `error`와 `errorCode`를 함께 직렬화해야 함
- `crates/omp-cli/src/rpc.rs:863` — `get_subagents`가 빈 배열 스텁: 레지스트리 스냅샷 조회로 교체
- `crates/omp-cli/src/tools/task.rs` — 기존 동기 배치 실행기(`TaskRunner::run_one`)를 백그라운드 태스크 본문으로 재사용하는 지점

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 세션 ID와 상태
`TaskSessionId(String)` 불변식: `starts_with("st_") && len() == 29` (`st_` + ULID 26자 Crockford Base32). 위반 시 `invalid_task_id`.

```rust
pub enum TaskSessionState { Queued, Running, InputRequired, Completed, Failed, Cancelled }
pub struct TaskSession {
    pub id: TaskSessionId,
    pub parent_session_id: String,
    pub subagent_type: String,
    pub state: TaskSessionState,
    pub seq: AtomicU64,                 // 출력 커서 단조 증가
    pub exit_code: Option<i32>,
    pub error: Option<TaskErrorInfo>,   // { code: &'static str, message: String }
}
```

### 2.2 레지스트리 (`crates/omp-task/src/session_registry.rs` 신설)

```rust
pub struct TaskSessionRegistry { /* DashMap<TaskSessionId, Arc<TaskSession>>, Semaphore, ring: RingBuffer, root: PathBuf */ }

impl TaskSessionRegistry {
    pub async fn spawn(&self, req: SpawnTaskRequest) -> Result<TaskSessionId,
