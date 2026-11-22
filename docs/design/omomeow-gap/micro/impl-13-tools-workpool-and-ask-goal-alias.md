# 구현 조각 13: `crates/omp-cli/src/tools/`의 `workpool` 엔진 큐 및 `ask_user_question`·`create_goal`·`get_goal`·`update_goal` 호환 래퍼 구현 명세 (impl-13-tools-workpool-and-ask-goal-alias)

## 1. 목적 및 코드 근거 (File & Line)

- 신설 파일: `crates/omp-cli/src/tools/workpool.rs` (엔진 큐), 수정 파일: `crates/omp-cli/src/tools/ask.rs`, `crates/omp-cli/src/tools/goal.rs`, `crates/omp-cli/src/tools/mod.rs`, `crates/omp-cli/src/rpc.rs`.
- 라우팅 진입점은 `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 여기에 5개 명령을 매치로 추가하지 않으면 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어져 OmOMeow 도구 호출이 전부 실패한다.
- 응답 프레임은 `crates/omp-cli/src/rpc.rs:203-226`의 `response()`가 조립하며 `{type:"response", id, success, data?, error?, errorCode?}` 형태다. `omo-nyang/src/relay/move-host.ts:4-7, 61`이 같은 스키마를 파싱하므로 실패 시 `error` 문자열과 `errorCode`를 **동시에** 직렬화해야 한다(하나만 채우면 클라이언트 분기 불가).
- 프로토콜 계약: `omo-nyang/src/lane-hold.ts:29-76`의 `get_protocol_info()`는 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 요구하므로 신규 5개 명령명이 `commands` 배열에 포함되어야 한다.
- 격차 근거(요약 5): tau의 `crates/omp-cli/src/tools/task.rs` + `crates/omp-task`는 `context`+`tasks[]` 배치 **동기** 실행이고, `rpc.rs:863`의 `get_subagents`는 빈 배열 스텁이다. `shell_host.rs`/`jobs.rs`(`proc://`)/`wait.rs`는 있으나 `bash_output`/`kill_bash`/`monitor`가 없고, `/memory`는 스텁이며, `workpool` 도구군은 아예 없다. 질문·목표는 `ask({html}|questions[])`와 `goal({op})` 단일 도구뿐이다.
- OmOMeow 호출 스키마(격차 대상): `ask_user_question({questions:[{header,question,multiSelect,options}], waitForAnswer, required})` 및 `create_goal`/`get_goal`/`update_goal` 3종.
- 상호작용 제약: `interactive_user` 미선언 상태에서 `interrupt({intent:"stop"|"abort"})` 호출 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`(`omo-nyang/src/lane-hold.ts:29-76`). `ask_user_question` 대기열은 이 capability와 결합해 검증한다.
- 정착(settle) 신호: tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout으로 보내고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 보낸다. `workpool yield` 해제 조건은 `session_settled` 기준으로 정의한다.

## 2. 상세 계약 및 데이터/타입 명세

```rust
// crates/omp-cli/src/tools/workpool.rs
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkpoolOp {
    Create { pool: String, #[serde(default)] capacity: Option<u32>, #[serde(default)] ttl_ms: Option<u64> },
    Push { pool: String, items: Vec<WorkpoolItem> },
    Close { pool: String, #[serde(default)] reason: Option<String> },
    Inspect { pool: String, #[serde(default)] limit: Option<u32> },
    Cancel { pool: String, #[serde(default)] item_id: Option<String> },
    Yield { pool: String, #[serde(default)] timeout_ms: Option<u64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkpoolItem { pub id: Option<String>, pub prompt: String, #[serde(default)] pub priority: i32, #[serde(default)] pub meta: Option<serde_json::Value> }

#[derive(Debug, Serialize)]
pub struct WorkpoolState { pub pool: String, pub closed: bool, pub capacity: Option<u32>, pub pending: Vec<WorkpoolItem>, pub running: Option<String>, pub done: u64, pub cancelled: u64 }
```

- `push`는 `id` 미지정 시 `wp_<8hex>`를 발급하고 `pool` 단위로 `priority DESC, seq ASC` 정렬. `yield`는 `running` 슬롯을 비우고 다음 항목을 반환하되, 대기 중 `session_settled`(`runtime.rs:1641`) 수신 또는 `timeout_ms` 초과 시 반환한다.
- `ask_user_question` 래퍼(`tools/ask.rs`): `questions[].header|question|multiSelect|options[{label,description?}]`를 내부 `ask` 도구의 `questions[]`로 1:1 정규화하고, `waitForAnswer`(기본 true) / `required`(기본 false)를 보존한다. 반환 `data`는 `{questionId, cancelled:bool, answers:[{header, selected:[label], other:Option<String>}]}`.
- `create_goal`/`get_goal`/`update_goal`(`tools/goal.rs`): 각각 `goal({op:"create", objective, ...})`, `goal({op:"get"})`, `goal({op:"update", status, note})`로 정규화하는 얇은 래퍼. 반환 `data`는 `goal` 도구의 `{goalId, objective, status, updatedAt}`를 그대로 통과시킨다.
- 오류 코드 표: `invalid_arguments`(serde 실패), `unknown_pool`, `pool_closed`, `duplicate_item_id`, `queue_full`, `not_interactive_client`(`stop_requires_interactive_client` 대응), `yield_timeout`, `goal_not_found`, `unknown_goal_op`. RPC 레벨에서는 `errorCode` 필드로, 도구 레벨에서는 `data.errorCode`로 동일 문자열을 노출한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// rpc.rs:593 dispatch 내부, rpc.rs:1094 unknown arm 직전에 삽입
"workpool" => tools::workpool::handle(rt, cmd).await.map_err(CmdError::from),
"ask_user_question" => tools::ask::handle_user_question(rt, cmd).await.map_err(CmdError::from),
"create_goal" => tools::goal::alias(rt, cmd, GoalOp::Create).await.map_err(CmdError::from),
"get_goal" => tools::goal::alias(rt, cmd, GoalOp::Get).await.map_err(CmdError::from),
"update_goal" => tools::goal::alias(rt, cmd, GoalOp::Update).await.map_err(CmdError::from),
```

```rust
// 실패 프레임 조립: rpc.rs:203-226 response() 계약 준수
fn fail(id: &Value, code: &str, msg: impl Into<String>) -> Value {
    json!({ "type":"response", "id": id, "success": false,
            "error": msg.into(), "errorCode": code })
}
```

1. `workpool::handle`는 `serde_json::from_value::<WorkpoolOp>`로 파싱하고 실패 시 `fail(id, "invalid_arguments", e)` 반환(패닉 금지).
2. 엔진 상태는 `Arc<Mutex<BTreeMap<String, PoolState>>>`를 `Runtime`에 1회 지연 초기화(`OnceLock`)하여 세션 간 공유. `create`는 중복 `pool`에 대해 멱등(기존 상태 반환, `closed`면 `pool_closed`).
3. `push`는 `closed` 검사 → `capacity` 검사(`queue_full`) → `duplicate_item_id` 검사 → 정렬 삽입 → `WorkpoolState` 반환.
4. `yield`는 `tokio::sync::Notify`와 `timeout_ms`(기본 30_000) 조합으로 대기. 정착 이벤트 수신 훅은 `runtime.rs:1641`의 `session_settled` 직렬화 지점에서 `notify_waiters()` 호출.
5. `ask_user_question`은 `set_client_info`의 `capabilities`에 `interactive_user`가 없으면 즉시 `not_interactive_client`로 실패(대기 진입 금지). `required=true`이고 `cancelled=true`면 `success:false` + `errorCode:"question_cancelled"`.
6. `goal` 별칭은 `op` 필드가 이미 존재하면 원본 값을 우선(충돌 시 `invalid_arguments`), 없으면 래퍼가 주입.
7. `get_protocol_info()`의 `commands` 배열에 `workpool`, `ask_user_question`, `create_goal`, `get_goal`, `update_goal` 추가(정렬 유지).

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- `cargo build -p omp-cli` 및 `cargo clippy -p omp-cli -- -D warnings` 통과.
- `cargo test -p omp-cli --test tools_workpool` :
