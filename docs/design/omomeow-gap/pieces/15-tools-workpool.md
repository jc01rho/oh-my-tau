# tau 에이전트 도구: 키 기반 작업 큐(workpool) 설계

## 1. 현재 상태 (근거)

- 모델이 부르는 도구 목록에 workpool 이 없다. `crates/omp-cli/src/tools.rs:260` `builtin_tools` / `:279` `builtin_tools_with_env_and_jobs` 의 등록 배열(`:383` `"task"`, `:691` `"wait"`)에 항목이 없고, `crates/omp-cli/src/tools/` 에 `workpool.rs` 파일도 없다.
- workpool 은 eval 셀 안의 브리지 op 로만 존재한다. `crates/omp-cli/src/tools/eval_bridge.rs:164` 의 `AGENT_OPS` 에 `"__workpool__"` 이 있고, 구현은 `crates/omp-cli/src/tools/eval_agents.rs:2294` `run_workpool` → `:2298` `run_workpool_with` 다. 게이트웨이가 요구하는 "호스트 도구"가 아니라 커널 헬퍼다.
- 현 큐는 `crates/omp-cli/src/tools/eval_agents.rs:1824` `PoolState`, 항목 상태는 `:1773` `ItemStatus`(Queued/Running/Completed/Failed/Cancelled). push `:1856`, dispatch `:1885`, 워커 생성 `:1902`, 배치 프롬프트 `:1939`, 정산 `:1954`, status/peek/close `:1985`/`:2020`/`:2049`.
- 이 큐는 `freshAgents` 전용이다. `:1764-1770` 주석이 "omp-rs subagents exit when their prompt settles" 라며 항목당 새 워커 하나를 띄우는 모드만 구현한다고 명시한다. keep_alive 경로가 없다.
- 키 개념이 없다. `PoolItem` 은 `id: String`(`<pool>#<seq>`)만 갖고(`:1856-1875`), push 는 `items` 문자열 배열만 받는다(`:2320-2330`). 중복 키 판정·입력 해시·멱등 재전송이 전부 없다.
- 항목별 결과 보존이 없다. `finish_turn`(`:1954`)이 배치 단위 `output` 만 `PoolBatch` 에 남기고 항목 상태만 승격한다. 집계 전달(`aggregate`)은 "omp-rs 대응물이 없다"고 `:1768-1770` 에 적혀 있다.
- 동시성 한도는 프로세스 전역값 하나다. `:62` `subagent_concurrency()` = `TaskRunner::default_max_concurrency()`(`crates/omp-task/src/runner.rs:232`, `min(cpu,8)`), `create_workpool`(`:2353`)이 이 값을 그대로 `limit` 으로 쓴다. `task.maxConcurrency`(`crates/omp-cli/src/tools/task.rs:49` `effective_max_concurrency`)는 eval 런처에 반영되지 않는다(`:58-61` 주석). 풀별 `limit` 인자도 파싱하지 않는다.
- 워커 실행기는 재사용 가능한 형태로 분리돼 있다. `eval_agents.rs:484` `trait Launcher`, `:493` `impl Launcher for SubprocessLauncher` 가 `omp_task::TaskRunner::start(spec, ChildMode::Rpc)`(`crates/omp-task/src/runner/control.rs:307`)를 호출하고, `:504` `LAUNCHER` 가 프로세스 전역 세마포어를 공유한다. `RunControl::steer`(`control.rs:228`)/`cancel`(`:250`)도 있다.
- `tools`(워커 도구 허용 목록)는 사실상 거부된다. `create_workpool` 이 비어 있지 않은 `tools` 배열에 `"Eval-defined tools are unavailable in this runtime."` 를 반환한다(`:2353-2400`).
- 백그라운드 결과 자동 전달은 실제로 없다. `crates/omp-cli/src/tools/jobs.rs:221` 은 `"delivery": {"queued": 0, "delivering": false, "pendingJobIds": []}` 를 상수로 내보내고, `:291` `background_notice` 만 "auto-delivers on finish" 를 문구로 주장한다. 실제 주입 지점은 `crates/omp-cli/src/runtime.rs:1858` `Runtime::follow_up` 뿐인데 도구 계층은 `Env`(`tools.rs:115-131`)만 받아 `Runtime` 핸들이 없다.
- 영속 경로 선례는 있다. `crates/omp-cli/src/tools/eval_journal.rs:145` `journal_path()` 가 `omp_urls::artifacts_directory_for`(`crates/omp-urls/src/lib.rs:137`) 아래 JSONL 을 쓴다. workpool 저장소도 같은 규약을 쓸 수 있다.

## 2. 게이트웨이가 요구하는 계약

mhc 바이너리 문자열 덤프(`/data/home/.local/share/mhc/versions/2026.11.20-omomeow1/mhc`)에서 확인했다. 소스가 아니므로 필드명·문구 수준까지만 신뢰한다.

- 도구 설명: "Create and inspect engine-owned keyed work queues. Push returns durable IDs without waiting for capacity. Omitted mode uses the approved keep_alive default; fresh remains selectable. Inspect reads durable keyed data or errors; close delivers one aggregate; uncertain delivery is never retried automatically."
- 셀 헬퍼 표면: `workpool(agent, name, mode?)` → `{ pool_id, push(items), close(), inspect(), cancel() }`. `pool_id` 는 `wp_` 접두이고 오류 문구는 "Expected a wp_ pool identifier."
- push 항목 스키마: `key`/`input`/`subagent_type`/`prompt`/`model` 계열, `additionalProperties` 거부.
- 중복 키: "The key already identifies different input." → 같은 키·같은 입력은 기존 durable id 반환, 같은 키·다른 입력은 `duplicate_key_conflict`.
- 워커 측 결과: `workpool_yield` / `WorkpoolYieldEvent { pool_id, generation, item_id, result, sha256 }`. 워커 프롬프트에 "Process the admitted workpool assignments above. Use workpool yield for each key; do not spawn or schedule tasks." 가 주입된다.
- 워커 오류: "Key is not assigned to this worker turn.", "This item already has a different terminal yield."(`yield_conflict`), "Worker completed without a keyed yield for this item."(`item_missing_yield`).
- 오류 코드: `invalid_input`, `invalid_pool_id`, `pool_not_found`, `scope_denied`, `pool_name_conflict`, `pool_closed`, `duplicate_key_conflict`, `admission_refused`, `spawn_failed`, `worker_not_continuable`, `worker_unassigned`, `yield_unavailable`, `store_corrupt`, `item_missing_yield`, `cwd_unavailable`, `config_generation_mismatch`.
- 소유권: "Only the owning parent session may access this pool."(= `scope_denied`), 이름 충돌은 "Pool name is already in use; choose a new explicit name."
- 영속: 풀 디렉터리(`workpools`), `baseline.json`, `pause.json`, 인덱스 `rpc/task-stores.json`, 레코드 필드 `parent_session_id`/`root_session_id`/`generation`/`agent`/`worker_spec`/`kernel_tool_names`/`aggregate`/`paused_workers`/`revisions`.
- 전달 상태: `delivery_uncertain` 이 별도 코드로 존재하고 재시도를 금지한다.
- tau 현 표면과의 차이: tau 는 `name` 기반 `create`/`push`/`status`/`peek`/`close`(`eval_agents.rs:2294-2350`), 게이트웨이는 `pool_id` 기반 `push`/`close`/`inspect`/`cancel` 이다. `status`+`peek` 를 `inspect` 로 합치고 `cancel` 을 추가해야 한다.

## 3. 설계

### 3.1 재사용 판단

`crates/omp-task` runner 는 재사용하고 큐는 새로 둔다. runner 는 이미 세마포어 동시성(`runner.rs:218-266`), 프로세스 그룹 kill·steer(`control.rs:179-266`), 출력 상한(`runner.rs:34-37`, 500KB/5000줄)을 갖고 있어 워커 실행을 다시 만들 이유가 없다. 반면 `PoolState`(`eval_agents.rs:1824`)는 키·영속·집계가 없어 확장이 아니라 대체 대상이다. eval 셀 헬퍼와 모델 도구는 같은 엔진을 공유해야 두 표면이 갈라지지 않는다.

새 파일 `crates/omp-cli/src/tools/workpool.rs` 에 둘 것:

```
pub struct PoolId(String);                        // "wp_" + 32 hex
pub struct WorkpoolStore { root: PathBuf }        // <artifacts>/workpools
pub struct KeyedQueue { items: BTreeMap<String, QueueItem>, ... }
pub struct QueueItem { key, input_sha256, item_id, status, attempts, worker_id, result, error }
pub struct WorkpoolTool { env: Arc<Env>, store: WorkpoolStore, delivery: Arc<dyn WorkpoolDelivery> }
pub trait WorkpoolDelivery: Send + Sync { fn deliver(&self, aggregate: Value) -> DeliveryOutcome; }
```

저장소 경로는 `eval_journal.rs:145` 와 같은 `omp_urls::artifacts_directory_for` 규약을 쓴다. 세션 파일이 없으면(`--no-session`) 인메모리로 퇴화한다.

### 3.2 동시성 한도

두 층으로 나눈다.

- 풀 한도 `limit`: `KeyedQueue` 디스패치가 지킨다. 현 `dispatch`(`eval_agents.rs:1885`)의 `workers.len() < limit` 판정을 옮기되, `limit` 을 `create` 인자로 받고 없으면 `TaskRunner::resolve_max_concurrency(settings.get_number("task.maxConcurrency"))`(`runner.rs:244`, `task.rs:49`)로 해석한다. `subagent_concurrency()` 고정값을 쓰지 않는다.
- 프로세스 한도: `LAUNCHER`(`eval_agents.rs:504`)의 `TaskRunner` 세마포어가 담당한다. 풀 한도가 더 크면 항목은 `Queued` 로 남고 `RunPhase::Queued`(`control.rs:96`)가 `inspect` 에 그대로 노출된다. 풀 한도 초과는 거부가 아니라 대기다.
- `admission_refused` 는 프로세스 세마포어가 닫혔을 때만 낸다. 현재 `TaskRunner::spawn` 은 세마포어가 닫히지 않는다고 가정하고 `expect` 한다(`runner.rs:280-285`) — 이 `expect` 를 실패 `RunOutput` 으로 바꾸는 것이 선행 조건이다.

### 3.3 중복 키 처리

- 정규화: `input` 을 키 정렬 JSON 으로 직렬화한 뒤 sha256. 이 값이 `QueueItem.input_sha256` 이고 게이트웨이 `WorkpoolYieldEvent.sha256` 과 같은 의미다.
- 같은 `key` + 같은 `input_sha256`: 기존 `item_id` 반환, 상태 불변. push 응답은 `{"ids": [...], "deduped": [...]}`.
- 같은 `key` + 다른 `input_sha256`: `duplicate_key_conflict` 로 거부하고 기존 항목은 건드리지 않는다. 게이트웨이 문구와 1:1.
- 종결 항목도 키를 유지한다. 재push 는 `Completed`/`Failed` 결과를 그대로 돌려주고 재실행하지 않는다. 재실행은 `cancel` 후 새 풀이라는 명시적 경로만 허용한다.

### 3.4 실패 항목 보존

항목은 종결 후에도 삭제하지 않는다. `QueueItem` 은 `status`, `result: Option<Value>`, `error: Option<String>`, `exit_code`, `stderr`(절단 표시 포함), `duration_ms`, `attempts` 를 유지한다. `RunOutput`(`runner.rs:53`)의 `truncated`/`schema` 도 옮겨 "왜 실패했는지"가 집계 후에도 남게 한다. `cancel` 된 항목도 `Cancelled` 로 보존하고 `dropped` 목록에만 넣는다(현 `close` `:2049-2060` 동작 유지).

### 3.5 집계 전달 시점

- 트리거는 `close` 하나뿐이다. `close` 는 새 push 를 막고(`PoolClosed`), 대기 항목을 `Cancelled` 로 정리하고, 실행 중 항목은 끝까지 둔다.
- 집계는 마지막 항목이 종결된 시점에 한 번 만든다. 키 정렬 항목 배열 + 상태 카운트 + 실패 항목의 `error`/`exit_code`. 실패·취소를 빼지 않는다.
- 전달 상태: `pending` → `delivering` → `delivered` | `uncertain`. `delivered` 이후 재전달 없음, `uncertain` 은 재시도 없음(게이트웨이 규약). 세션 종료로 `delivering` 이 끊기면 `uncertain` 으로 낙착하고 `inspect` 에 남긴다.
- 전달 경로: `WorkpoolDelivery` 를 `main.rs:489` 부근(`set_runtime_hooks` 옆)에서 설치하고, 구현은 `Runtime::follow_up`(`runtime.rs:1858`)으로 부모 세션에 한 통을 넣는다. `agent_settled`(`crates/omp-cli/src/ext.rs:1593`)는 확장이 핸들러를 등록했을 때만 나가므로 전달 트리거로 쓰지 않는다.
- `inspect` 는 전달 후에도 항목별 결과를 계속 읽을 수 있어야 한다. 집계는 사본이고 원본은 저장소에 남는다.

### 3.6 워커 결과 수거(키 기반 yield)

현 구조에서 자식의 최종 출력은 `RunOutput.output` 하나뿐이다(`runner.rs:53-71`). 키별 yield 는 3단계다.

- 1단계(단일 항목 워커): 항목 1개 = 워커 1개. 워커 프롬프트에 `key` 를 넣고, 자식이 `yield`(`crates/omp-cli/src/tools/yield_tool.rs:29`)로 낸 최종 텍스트를 그 키의 결과로 귀속한다. 키가 여럿인 배치는 만들지 않는다. 이 단계만으로 push/inspect/close/aggregate 흐름은 성립한다.
- 2단계(키 기반 yield): `RunJoin`(`control.rs:273`)이 `RunOutput` 하나로 끝나는 한 불가능하므로 진행 중 이벤트 채널을 `RunJoin` 에 추가해야 한다. 자식 측은 `yield_tool` 에 `key` 필드를 받아 `WorkpoolYieldEvent`(`pool_id`, `generation`, `item_id`, `result`, `sha256`)를 내보내야 한다. `yield_tool.rs:5` 가 "no workpool" 이라고 명시하므로 실제 코드 수정이다.
- 3단계(keep_alive): `ChildMode`(`control.rs:85`)는 `Print`/`Rpc` 뿐이고 `Rpc` 도 정산 후 `RPC_FINISHING_DEADLINE`(15초, `control.rs:64-72`) 안에 최종 답과 세션 통계를 받고 끝난다. 두 번째 프롬프트를 받아 계속 사는 `Resident` 모드를 새로 넣어야 한다. 게이트웨이 기본값이 keep_alive 라는 점이 이 조각의 최대 격차다.

## 4. 구현 단계 (커밋 단위)

1. `omp-task`: `TaskRunner::spawn` 의 세마포어 `expect`(`runner.rs:280-285`)를 실패 `RunOutput` 으로 바꾸고 `RunJoin` 에 진행 이벤트 수신자를 추가한다. 동작 변화 없는 준비 커밋.
2. `omp-cli/tools/workpool.rs`: `PoolId`, `QueueItem`, `KeyedQueue`(push/dedupe/conflict/dispatch/finish/close/inspect/cancel) + 단위 테스트. 런처는 기존 `Launcher` 트레이트(`eval_agents.rs:484`)를 주입받아 목으로 시험한다.
3. `omp-cli/tools/workpool.rs`: `WorkpoolStore` 영속(`<artifacts>/workpools/<pool_id>.json` + `index.json`), 손상 시 `store_corrupt`, 소유 세션 검증(`scope_denied`), 이름 충돌(`pool_name_conflict`).
4. `omp-cli/tools.rs`: `workpool` 도구 등록(`:383` 옆) + 스키마 + 설명. `tools` 허용 목록은 이번 커밋에서 계속 거부하되 `invalid_input` 으로 명시한다.
5. `omp-cli/tools/workpool.rs` + `main.rs`: `WorkpoolDelivery` 설치, `close` 집계 전달, `pending`/`delivered`/`uncertain` 상태 기계.
6. `omp-cli/tools/eval_agents.rs`: `__workpool__` 을 새 엔진 위임으로 바꾸고 op 이름을 `inspect`/`cancel`/`pool_id` 로 정렬한다(`:2294-2350` 의 `status`/`peek`/`name` 제거 또는 별칭 유지).
7. (별도 조각) `omp-task` `ChildMode::Resident` + `yield` 키 전달로 keep_alive 와 키 기반 yield 를 완성한다.

## 5. 테스트

- `KeyedQueue` 상태 기계: push 5개 → `limit=2` 에서 2 Running/3 Queued, 하나 종결 시 다음 1개 디스패치. `eval_agents.rs:2573` `MockLauncher` 패턴을 `workpool.rs` 테스트 모듈로 옮겨 쓴다.
- 중복 키: 같은 키·같은 입력 2회 push → `ids` 동일, 실행 1회. 같은 키·다른 입력 → `duplicate_key_conflict`, 기존 항목 상태 불변.
- 실패 보존: 1개를 `RunStatus::Failed`(exit 3, stderr 있음)로 정산한 뒤 `inspect` 에 `error`/`exit_code`/`truncated` 가 남는지, 집계 문자열에 그 항목이 포함되는지.
- 집계 1회: `close` 후 `deliver` 호출 횟수가 정확히 1. `DeliveryOutcome::Uncertain` 목에서 재호출이 없고 상태가 `uncertain` 으로 남는지.
- 영속: 저장 → 새 `WorkpoolStore` 로 재로드 → `inspect` 동일. 파일을 잘라 `store_corrupt` 확인.
- 동시성 상호작용: 풀 2개가 `LAUNCHER` 세마포어를 공유할 때 총 실행 수가 `min(cpu,8)` 을 넘지 않는지(`runner.rs:344` `available_permits` 로 관측).
- 도구 표면: `tools.rs` 도구 이름 배열 테스트(`:680-700`)에 `workpool` 이 추가되고 스키마가 `additionalProperties: false` 인지.
- 실행 검증: `cargo test -p omp-cli -p omp-task` 1회 통과. 정산 대기는 `Notify`/oneshot 구독으로 하고 sleep 을 쓰지 않는다.

## 6. QA 시나리오

1. `omt --mode rpc` 세션에서 `workpool({op:"create", agent:"task"})` → `wp_` 로 시작하는 `pool_id` 확인.
2. 같은 풀에 키 `a,b,c` push. `b` 를 같은 입력으로 다시 push → 같은 id, 중복 실행 없음. `b` 를 다른 입력으로 push → `duplicate_key_conflict`.
3. `limit=1` 풀에 3개 push → `inspect` 에 running 1 / queued 2.
4. 실행 중 워커 하나 `cancel` → 해당 항목 `cancelled`, 나머지 계속. `inspect` 에 취소 사유.
5. 자식 하나를 실패로 유도(잘못된 모델명) → 항목 `failed` + `exit_code`, 집계에 포함.
6. `close` → 부모 세션에 집계 follow-up 1건 도착. 같은 풀에 다시 push → `pool_closed`.
7. 세션 재시작 후 `inspect` → 이전 항목 결과 그대로(영속 확인).
8. 전달 도중 세션 강제 종료 → 재시작 후 `inspect` 에 `delivery: uncertain`, 재전달 없음.
9. eval 셀에서 `workpool()` 호출 → 모델 도구와 같은 `pool_id`/항목을 보는지(두 표면 일치).

## 7. 위험과 미확인

- keep_alive 미구현이 최대 위험이다. 게이트웨이 기본이 keep_alive 인데 tau 는 fresh 만 가능하다(`eval_agents.rs:1764-1770`, `control.rs:85`). fresh 로 계약을 만족시키려면 게이트웨이 측 모드를 fresh 로 고정해야 한다.
- 자식이 정산 후 두 번째 prompt 를 받아들이는지 확인 필요. `RPC_FINISHING_DEADLINE`(`control.rs:64-72`)과 stdin EOF 처리를 보면 정산 후 종료가 전제로 보이지만 실제 자식 루프 수용 여부는 실행으로 확인해야 한다.
- 전달 경로 부재. `Runtime::follow_up`(`runtime.rs:1858`)은 `Runtime` 핸들을 요구하는데 도구는 `Env`(`tools.rs:115`)만 받는다. `set_runtime_hooks`(`eval_agents.rs:93`, `main.rs:489`) 같은 전역 설치가 필요하고, 세션 다중화(`--multi-session` 부재, `crates/omp-cli/src/cli.rs:97-160`) 시 어느 세션으로 전달할지 규칙이 없다. 확인 필요.
- `jobs.rs:221` 의 자동 전달 주장과 구현 불일치(`:291` 문구 vs 상수 스냅샷). workpool 집계가 이 표면을 재사용하면 같은 문제를 물려받는다.
- 세션 저장 형식 불일치. tau 는 omp JSONL v3(`crates/omp-session`), 게이트웨이는 senpi 형식이고 저장소 인덱스가 `rpc/task-stores.json` 이다. workpool 레코드를 게이트웨이가 직접 읽어야 하는지 확인 필요.
- 워커 도구 grant(`kernel_tool_names`)가 지금 거부된다. 실제 집행에는 `apply_child_policy`(`eval_agents.rs:2179`)의 `--tools` 경로와 정책 검증이 더 필요하다.
- 키 기반 yield 는 `yield_tool.rs:5` 가 명시적으로 배제한 영역이라 자식 프롬프트·스키마·전송 삼면을 동시에 바꿔야 한다. 부분 구현으로 배포하면 "Worker completed without a keyed yield" 가 상시 발생한다.
- 재귀 깊이: 워커도 `recursion_gate()`(`runner.rs:196`, 기본 2)를 통과해야 한다. 풀 안에서 다시 풀을 만드는 흐름은 거부되며 게이트웨이가 이를 기대하는지 확인 필요.
- 출력 상한(500KB/5000줄, `runner.rs:34-37`)으로 잘린 결과가 집계에 들어가면 조용히 축약된다. `truncated` 를 집계에 실어야 한다.
- 게이트웨이 계약은 바이너리 문자열 덤프로만 확인했다. `push` 항목의 정확한 필수 필드와 `close` 응답 형태는 mhc 소스로 재확인해야 한다(확인 필요).

---

완료하지 못한 부분을 명확히 적는다.

- 코드 작성·커밋·워크트리는 지시대로 하지 않았다. 저장소는 읽기만 했고 `git status` 도 깨끗한 상태로 확인했다(`e7a5378`).
- mhc 소스(`crates/omo/senpi-task/src/workpool/*`, `crates/omo/components/maho-omo-task/src/workpool_tool.rs`)는 이 머신에 없다. 계약은 바이너리 문자열에서만 추출했으므로 `push` 항목의 필수 필드, `close`/`inspect` 응답 JSON 형태, `mode` 기본값의 실제 해석은 검증하지 못했다.
- `cargo test` / `cargo check` 를 실행하지 않았다. 인용한 파일:줄은 전부 실제 읽기로 확인했지만 컴파일 가능성은 검증 밖이다.
- 자식 프로세스가 정산 후 두 번째 프롬프트를 받는지, `RPC_FINISHING_DEADLINE` 이후 stdin 이 어떻게 되는지는 실행 실험을 하지 못했다.
- `close` 이후 집계가 실제로 부모 턴에 주입되는 경로(`Runtime::follow_up` 연결)는 설계만 했고 프로토타입을 만들지 않았다.
