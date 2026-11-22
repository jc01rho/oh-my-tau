# 조사 조각 02 (1795326973 확인필요 2): `tau` `crates/omp-task`(`context`+`tasks[]` 동기)와 OmOMeow `task`(`category`/`subagent_type`+`st_...` 비동기) 정밀 대조 (inv-02-task-tool-contract-diff)

## 1. 목적 및 코드 근거 (File & Line)

목적: tau의 동기 배치 `task` 계약과 OmOMeow가 실제로 호출하는 비동기 핸들 계약의 차이를 확정하고, 양립 계층의 최소 구현 범위와 완료 판정을 고정한다.

- `crates/omp-cli/src/tools/task.rs`: tau `task` 도구 정의. 입력은 `{context: String, tasks: TaskSpec[]}`, `TaskSpec`은 `{description, prompt, subagent_type?, model?}`. 배치 전체를 동기 실행하고 `Vec<TaskResult>`를 반환하며 `run_in_background` 필드가 없다.
- `crates/omp-task/src/lib.rs`: 배치 실행기. `context`를 공통 프리앰블로 각 프롬프트 앞에 결합하고 실행 완료 후 결과를 모은다. 세션 핸들(`st_...`)을 발급하지 않는다.
- `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`: 명령 라우팅 진입점. task 계열 명령은 여기 match arm에 추가한다.
- `crates/omp-cli/src/rpc.rs:863` `get_subagents`: 현재 빈 배열 스텁. OmOMeow는 이 응답으로 진행 중 task 목록을 조회하므로 스텁이면 항상 "실행 중 없음"으로 오판한다.
- `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`: 미등록 명령 실패 경로. 신규 명령 누락 시 여기로 떨어진다.
- `crates/omp-cli/src/rpc.rs:203-226` `response()`: `{type:"response", id, success, data?, error?, errorCode?}` 조립. 실패 시 `error`와 `errorCode`를 함께 직렬화해야 한다.
- `omo-nyang/src/relay/move-host.ts:4-7,61`: 위 프레임의 소비자. `success:false`에서 `error`/`errorCode`가 비면 원인 미상 실패로 처리된다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 tau 현재 계약 (동기 배치)
```json
{"context":"repo: ...", "tasks":[{"description":"scan","prompt":"...","subagent_type":"general"}]}
```
응답: `{"results":[{"description":"scan","output":"...","is_error":false}]}` — RPC 호출이 전 배치 종료까지 블로킹된다.

### 2.2 OmOMeow 기대 계약 (비동기 핸들)
- 호출: `task({description, prompt, category?, subagent_type?, run_in_background:true, context?})`
- 즉시 반환: `{task_id:"st_<24hex>", status:"running"}`
- 후속 도구: `task_send({task_id, message})`, `task_output({task_id, block?, timeout_ms?})`, `task_cancel({task_id})`, `task_list({})`

### 2.3 필드 매핑 규칙
| OmOMeow 필드 | tau 대응 | 처리 |
| --- | --- | --- |
| `category` | 없음 | `subagent_type`으로 승격. 둘 다 있으면 `subagent_type` 우선, `category`는 경고 로그 후 무시 |
| `subagent_type` | `tasks[].subagent_type` | 그대로 전달, 미지정 시 `"general"` |
| `run_in_background` | 없음 | `true`면 비동기 레지스트리 경로, 미지정/`false`면 기존 배치 경로 유지 |
| `context` | `context` | 비동기 단건에서는 프롬프트 프리앰블로 주입 |
| `st_...` | 없음 | 신규 발급, `^st_[0-9a-f]{24}$` |

### 2.4 상태 전이
`running -> completed | failed | cancelled`. 종결 상태에서 `task_send`는 `task_not_running`, `task_cancel`은 멱등 성공(`cancelled` 재반환)으로 고정한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/tools/task.rs (확장)
pub struct TaskToolArgs {
    pub description: String,
    pub prompt: String,
    pub context: Option<String>,
    pub category: Option<String>,
    pub subagent_type: Option<String>,
    pub run_in_background: Option<bool>,
}

// crates/omp-task/src/registry.rs (신규)
pub enum TaskStatus { Running, Completed, Failed, Cancelled }
pub struct TaskHandle {
    pub id: String, pub status: TaskStatus,
    pub tx: mpsc::Sender<TaskInput>,
    pub out: Arc<Mutex<RingBuffer>>,   // 8 MiB 상한
    pub cancel: CancellationToken,
}
impl TaskRegistry {
    pub fn spawn(&self, spec: TaskSpec) -> Result<String, TaskError>; // "st_" + 24 hex, 3회 재시도
    pub fn output(&self, id: &str, block: bool, timeout: Duration) -> Result<TaskOutput, TaskError>;
    pub fn cancel(&self, id: &str) -> Result<TaskStatus, TaskError>;
    pub fn list(&self) -> Vec<TaskSummary>;
}
```

```rust
// crates/omp-cli/src/rpc.rs:593 dispatch 내 추가 arm (rpc.rs:1094 이전에 배치)
"task_start"  => tools::task::rpc_start(rt, cmd).await,
"task_send"   => tools::task::rpc_send(rt, cmd).await,
"task_output" => tools::task::rpc_output(rt, cmd).await,
"task_cancel" => tools::task::rpc_cancel(rt, cmd).await,
"task_list"   => tools::task::rpc_list(rt, cmd).await,
"get_subagents" => tools::task::rpc_subagents(rt, cmd).await, // rpc.rs:863 스텁 대체
```
모든 실패는 `CmdError::new(msg).with_code("task_not_found")` 형태로 만들어 `response()`(rpc.rs:203-226)가 `error`+`errorCode`를 동시에 내보내게 한다.

```ts
// omo-nyang 측 어댑터 의사코드
if (args.run_in_background) {
  const r = await rpc("task_start", { ...args, subagent_type: args.subagent_type ?? args.category ?? "general" });
  if (!r.success) throw new Error(`${r.errorCode}: ${
