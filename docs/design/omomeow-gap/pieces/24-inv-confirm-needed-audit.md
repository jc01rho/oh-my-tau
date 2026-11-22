# tau(oh-my-tau) 엔진의 OmOMeow 게이트웨이 계약 적합성 — '확인 필요' 5개 항목 실측 확정

## 1. 현재 상태 (근거)

- 조사 기준 리비전: `~/git/oh-my-tau` HEAD `e7a5378` (grafted, origin/main). 워크스페이스는 `crates/omp-*` 24개 크레이트.
- RPC 표면은 `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` 의 단일 `match kind` 다. 마지막 arm 이 `other => Err(CmdError::new(format!("Unknown command: {other}")))` (`rpc.rs:1094`) 이므로, 표에 없는 명령은 전부 이 한 줄로 떨어진다.
- 응답 봉투는 `fn response` (`rpc.rs:203-223`): `{id?, type:"response", command, success, data?}` 또는 `{..., success:false, error, code?}`. `id` 는 `Option<&Value>` 라 없으면 프레임에 키 자체가 들어가지 않는다 (`rpc.rs:205-207`).
- CLI 파서는 `parse_from` (`crates/omp-cli/src/cli.rs:331`) 이고 플래그는 세 테이블로만 판정한다: `STRING_FLAGS` (`cli.rs:97-136`), `OPTIONAL_FLAGS` (`cli.rs:138`, `--resume|-r|--session`), `VALUELESS_FLAGS` (`cli.rs:142-163`). 테이블에 없는 `--foo` 는 `finish` 에서 `unknown flag{plural}` 로 즉시 실패한다 (`cli.rs:513-518`).
- 확장 호스트는 `crates/omp-ext/src/host.rs` (1362줄) + `ext-host/host.ts`. 이벤트는 `emit`/`emit_nowait` (`host.rs:604-628`) 로만 나가고, 확장→코어 역방향은 `host.rs:1045` 의 JSON-RPC method 디스패치다. `api.rpc.handle` 에 해당하는 등록 표면은 코어 쪽에 없다.
- 도구 등록은 `crates/omp-cli/src/tools.rs:322-400` 의 `vec![read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask, todo, web_search, lsp, checkpoint, rewind]` 가 전부다.
- 종료 신호는 비대칭이다: stdout 프레임은 `session_settled` (`crates/omp-cli/src/runtime.rs:1641`), `agent_settled` 는 확장 이벤트로만 `crates/omp-cli/src/ext.rs:1593-1597` 에서 나간다.
- 세션 파일은 `crates/omp-session` 의 omp JSONL v3: `pub const SESSION_VERSION: u32 = 3` (`crates/omp-session/src/lib.rs:25`), 헤더 `{"type":"session","version":3,"id":…,"timestamp":…,"cwd":…}` (`lib.rs:225`, `lib.rs:246`).

## 2. 게이트웨이가 요구하는 계약

- 기동 argv: `--mode rpc --multi-session --session-runtime in-process -e <ext>` (`omo-nyang/src/relay/move-host.ts:37-38`), `--mode rpc --no-session … --no-model-fallback --no-recommended-models` (`src/relay/split.ts:77-78`), `--mode rpc --session <path> --append-system-prompt … --skill <dir>` (`src/gateway.ts:1077-1078`).
- 응답 봉투: `z.object({type: z.literal("response"), id: z.string(), success: z.boolean(), data: z.unknown().optional(), error: z.string().optional(), errorCode: z.string().optional()})` (`src/relay/move-host.ts:4-7`). 매칭 실패 프레임은 버린다 (`move-host.ts:52-57`).
- 요청/이벤트 상관: 게이트웨이는 `gatewayRequestId` 를 이벤트에 실어 보내고 (`src/relay/runtime.ts:235`), `agent_settled` 를 턴 종료 신호로 소비한다 (`src/brain.ts:330-337`, `src/brain.ts:351`, `src/relay/slice-run.ts:71`, `src/relay/fleet.ts:245`). `agent_end` 는 중단 소스 판정에 쓴다 (`src/brain.ts:326-329`).
- RPC 표면: `interrupt`/`set_client_info`/`get_protocol_info`/`reload`/`resume_session`/`release_session` (`src/lane-hold.ts:29,60,66`, `src/worker-control.ts:89`), `extension_request` (`src/relay/agentd-sessions.ts:141`, `src/relay/move-operations.ts:212`), `move_session` 5개 op (`src/relay/move-operations.ts:104-220`).
- 확장 계약: `api.rpc.handle("omomeow.move-readiness", …)` (`src/session-move-extension.ts:139`), `api.on("session_move_export"|"session_move_import", …)` (`src/session-move-extension.ts:116-117`).
- 세션 신원: 첫 줄이 `type=="session"` 이고 `id` 가 문자열이면 그 값을 sessionId 로 쓴다 (`src/session-identity.ts:13-22`, `24-31`).
- task 수명주기는 엔진이 아니라 허브가 소유한다: `task_output` 테이블 (`src/bridge-state.ts:39`), `decision-inbox.ts:79`, `worker-control.ts:25` 의 `no_progress`.

## 3. 설계

### 3.1 항목 1 — `--multi-session`/`--session-runtime` vs tau 단일 세션 (확정: 미지원)

- 확정 결론: tau에는 다중 세션 런타임이 없다. 세 테이블(`cli.rs:97-163`) 어디에도 두 플래그가 없고, `parse_from`→`finish` 가 `unknown flags` 로 죽인다(`cli.rs:513-518`). `Runtime` 구조체(`runtime.rs:116-…`)는 `agent: Arc<Agent>` 하나, `persist: Persistence` 하나, `out: UnboundedSender<Value>` 하나로 프로세스=세션 1개다. `--session` 은 resume 별칭(`cli.rs:138`)이며 값이 경로면 그 경로에 새 세션을 만든다(`main.rs:135` `is_session_path`, `main.rs:158-170`).
- 설계: (a) 장기 — `Runtime` 을 `SessionRegistry { HashMap<SessionId, Arc<Runtime>> }` 로 감싸고 `move_session`/`release_session` 이 세션 단위로 동작하게 한다. (b) 단기 — 게이트웨이 쪽 `MoveHost` 를 프로세스 1개/세션 1개로 낮추고 `move_session` 경로를 비활성화한다(`session-move.ts:17-45` 가 이미 "move_session 없으면 엔진 이동 불가"로 처리하는 경로를 그대로 탄다). 단기 (b)를 권고한다. (a)는 `Runtime` 의 `Mutex` 필드 20여 개를 세션별로 재배치해야 해서 별도 에픽이다.

### 3.2 항목 2 — task 도구 동작 방식 차이 (확정: 층이 다름 + tau 결손)

- 확정 결론: tau `task` 는 배치 팬아웃 수집기다. 스키마는 `context` + `tasks[]` 필수, 항목 필수 키는 `task`/`solutionSpace`, 최상위 `model` 은 `{"not": {}}` 로 금지된다(`crates/omp-cli/src/tools/task.rs:96-125`). 항목 타입은 `TaskItem`(`crates/omp-task/src/params.rs:26-50`, `solution_space` 는 기본값 없는 `String`). 자식은 `omp-rs -p` 서브프로세스(`tools/task.rs:5-6`, `crates/omp-task/src/runner.rs:212`, `:357`)이고 결과는 `<task-result>` + `agent://<id>`(`crates/omp-task/src/envelope.rs:38-70`). 반면 게이트웨이의 task 는 허브 SQLite 와 별도 agentd 세션이며(`bridge-state.ts:39`), 엔진에는 자식 조회·중단·조향 RPC를 요구한다. tau의 그 표면은 전부 스텁이다: `rpc.rs:863` `get_subagents → {"subagents": []}`, `rpc.rs:865-868` `cancel_subagent → {"cancelled": false}`, `rpc.rs:869-877` `steer_subagent → Err("Subagent not running: {id}")`. 도구 목록(`tools.rs:322-400`)에 `task_send`/`task_output`/`task_cancel` 은 없다.
- 설계: `TaskRunner` 에 실행 레지스트리(`run_id → RunHandle{ child, log_sink, cancel }`)를 추가하고 이를 `rpc.rs` 의 `get_subagents`/`cancel_subagent`/`steer_subagent` 가 읽게 한다. 도구 3종은 같은 레지스트리의 얇은 래퍼로 등록한다. tau `task` 의 배치 스키마는 유지한다(게이트웨이가 `task` 도구를 직접 호출하지 않으므로 계약 충돌은 없다).

### 3.3 항목 3 — rpc 핸들·도구 입력 형식 (확정: 봉투 2건 불일치, 명령 8종 부재)

- 확정 결론 1: 오류 코드 키가 다르다. tau는 `code`(`rpc.rs:217-222`), 게이트웨이는 `errorCode`(`move-host.ts:4-7`). 게이트웨이는 zod parse 실패 프레임을 버리므로(`move-host.ts:52-57`) 오류 응답이 통째로 사라진다.
- 확정 결론 2: tau는 `id` 없는 요청에 `id` 없는 응답을 쓴다(`rpc.rs:205-207`). 게이트웨이는 `id: z.string()` 필수이고 `pending.get(value.id)` 로만 매칭한다(`move-host.ts:52-57`).
- 확정 결론 3: `dispatch` 전체 arm 을 읽은 결과 `interrupt`, `set_client_info`, `get_protocol_info`, `reload`, `resume_session`, `release_session`, `extension_request`, `move_session` 8종이 없다(`rpc.rs:593-1094`). `negotiate_protocol` 은 버전 2만 수용한다(`rpc.rs:595-600`).
- 확정 결론 4: 도구 입력은 엔진 내부 등록(`tools.rs:322-400`)이고 셸은 `bash` 하나다. `async:true` 는 `bg_N` 잡 등록 후 안내문 반환(`crates/omp-cli/src/tools/bash.rs:1037-1040`)이고 조회는 `read proc://`(`crates/omp-cli/src/tools/jobs.rs:1-11`)다. `bash_output`/`kill_bash`/`monitor` 는 없다.
- 설계: `response()` 에서 실패 시 `code` 와 `errorCode` 를 함께 쓰고, `handle`(`rpc.rs:568-575`)에서 `id` 가 없으면 합성 id를 넣는다. 8종 명령은 `dispatch` 에 arm 추가로 끝난다(`interrupt` → 기존 `rt.abort()`(`rpc.rs:677-680`) 재사용, `resume_session`/`release_session` → `get_state`(`runtime.rs:1953`) 의 `paused`/`runPaused` 필드 추가, `reload` → `load_session` 재사용). `bash_output`/`kill_bash`/`monitor` 는 `JobRegistry`(`jobs.rs`) 위 래퍼 3개로 등록한다.

### 3.4 항목 4 — `agent_settled` vs `session_settled` (확정: 의미와 경로가 모두 다름)

- 확정 결론: tau의 `session_settled` 는 큐가 빈 뒤에만 나간다. `report_prompts_now`(`runtime.rs:1612`)가 `let settle = p.active && drained && p.open.is_empty();`(`runtime.rs:1622-1626`) 일 때만 `self.send(json!({"type": "session_settled"}))`(`runtime.rs:1641`) 를 보내고, 이는 terminal `agent_end` 1틱 뒤다(`runtime.rs:1578`). `agent_settled` 는 `AgentEvent::AgentEnd` arm 안에서 확장 호스트로만 나가고(`ext.rs:1585-1597`), `host.has_handlers("agent_settled")` 가 참일 때만이며 ctx 는 `None` 이라 `gatewayRequestId` 가 없다(`ext.rs:1593-1595`).
- 확정 결론: 게이트웨이는 `agent_settled` 를 턴 종료로 쓰므로(`brain.ts:330-337`), tau를 그대로 붙이면 매 턴 미정착 후 `command()` 타임아웃(`brain.ts:374-377`, 기본 30초)으로 떨어진다. 게다가 tau의 정상 `agent_end`(`runtime.rs:614-661`)가 게이트웨이에서 abort 소스 판정(`brain.ts:326-329`)으로 오독된다.
- 설계: `report_prompts_now` 가 `session_settled` 를 보내는 그 지점에서 `agent_settled` 도 stdout 에 함께 보내고, `prompt_result` 에 이미 실리는 `id`(`runtime.rs:1631-1637`)를 `gatewayRequestId` 로 복사한다. `willContinue` 로 이어지는 턴은 settle 로 치지 않도록 `drained` 조건을 그대로 쓴다.

### 3.5 항목 5 — 세션 기록 형식 (확정: 헤더·경로 호환, 본문 차이는 허브 읽기에 무영향)

- 확정 결론 1: 헤더는 호환된다. tau 헤더는 `type:"session"`, `id` 문자열을 포함하고(`lib.rs:225`, `lib.rs:246`), senpi 판독기는 그 두 조건만 본다(`session-identity.ts:13-22`). 첫 64KB만 읽고 첫 줄을 파싱한다(`session-identity.ts:24-31`).
- 확정 결론 2: 경로도 호환된다. 게이트웨이는 `--session <path>` 로 파일을 지정하고(`gateway.ts:1077`), tau는 경로 값이면 `is_session_path`(`main.rs:135`) 판정 후 `SessionWriter::create_at` 로 그 경로에 헤더를 쓴다(`main.rs:158-170`).
- 확정 결론 3: 본문 v3 vs senpi 차이는 허브의 기록 읽기에 직접 영향이 없다. 허브는 파일을 파싱하지 않고 `get_messages`/`get_last_assistant_text`/`get_session_stats` 를 쓴다(`worker-rpc.ts:22-31`, `brain.ts:479`, tau 구현 `rpc.rs:998`, `rpc.rs:1030`). `messagesSchema` 는 `z.array(z.unknown())` 로 느슨하다(`brain.ts:31`).
- 확정 결론 4: 기본 저장 위치는 `~/.omp-rs/sessions/<slug>`(`persist.rs:62-67`)이고 `~/.omo` 가 아니다. 프로필/동시 실행 수를 `~/.omo` 에서 읽는 플릿 설정과는 분리되어 있다.
- 설계: 형식 변환은 필요 없다. 다만 (a) title 슬롯(`entries.rs:66-77`)이 첫 줄로 나가면 헤더 판독이 깨지므로 첫 줄 고정 여부를 먼저 확정해야 하고, (b) `move_session` 번들은 `session_move_export`/`import` 확장 이벤트(`session-move-extension.ts:116-117`)에 tau 세션 경로를 넘기는 방식으로 붙인다.

## 4. 구현 단계 (커밋 단위)

1. `cli: accept gateway flags` — `cli.rs:97-163` 테이블에 `--no-model-fallback`, `--no-recommended-models` 를 `VALUELESS_FLAGS` 로, `--session-runtime` 을 `STRING_FLAGS` 로, `--skill` 을 `--skills` 별칭으로 추가. `--multi-session` 은 수용하되 단일 세션이면 즉시 경고 후 종료하는 경로로. `cli.rs:600-640` 테스트에 케이스 추가.
2. `rpc: envelope compat` — `response()`(`rpc.rs:203-223`)에 `errorCode` 병기, `handle()`(`rpc.rs:568-575`)에 id 합성.
3. `rpc: lane commands` — `dispatch` 에 `interrupt`/`set_client_info`/`get_protocol_info`/`reload`/`resume_session`/`release_session` arm 추가, `state()`(`runtime.rs:1953-1991`)에 `paused`/`runPaused`/`serverVersion` 필드 추가.
4. `rpc: settled parity` — `report_prompts_now`(`runtime.rs:1612-1644`)에서 `agent_settled` + `gatewayRequestId` 동시 방출.
5. `ext: rpc.handle + extension_request` — `host.rs:1045` method 디스패치에 확장 등록 명령 라우팅, `rpc.rs` 에 `extension_request` arm.
6. `tools: background surface` — `JobRegistry`(`jobs.rs`) 위 `bash_output`/`kill_bash`/`monitor` 등록, `tools.rs:322-400` 목록 확장.
7. `tools: task control` — `omp-task` 실행 레지스트리 + `task_send`/`task_output`/`task_cancel`, `get_subagents`/`cancel_subagent`/`steer_subagent` 실구현(`rpc.rs:863-877` 대체).
8. `rpc: move_session` — `prepare`/`export`/`import`/`status`/`finish` 5 op + `session_move_export`/`session_move_import` 이벤트. 항목 1의 (a) 선행 필요.

## 5. 테스트

- 1~4는 `crates/omp-cli/src/rpc.rs` 하단 기존 인라인 테스트(`rpc.rs:1560-1580` 의 `negotiate_protocol` 픽스처) 옆에 프레임 단위 골든 테스트로 붙인다: 요청 JSON 한 줄 → 응답/이벤트 프레임 배열 비교.
- settle 동작은 `crates/omp-task/src/runner/control.rs:1118-1121` 의 `session_settled` 픽스처 방식(파일 대신 `on_line` 직접 호출)을 재사용해 `agent_settled` 가 `prompt_result` 뒤 한 번만 나오는지 검증한다.
- CLI는 `cli.rs:600-640` 스타일로 `parse(&["--multi-session", "--session-runtime", "in-process"])` 가 `Err` 가 아님을 고정한다.
- 봉투 호환은 게이트웨이 쪽 `responseSchema`(`move-host.ts:4-7`)를 그대로 옮긴 zod 파서로 tau 응답을 파싱하는 교차 테스트로 둔다. 순수 산문이 아니라 파싱되는 필드만 고정한다.

## 6. QA 시나리오

1. `omt --mode rpc --multi-session --session-runtime in-process --session /tmp/q/s.jsonl` 기동 → `get_protocol_info` 응답 확인, 프로세스가 살아 있는지 확인.
2. `prompt` → `prompt_result` + `agent_settled`(같은 id) 수신. 30초 내 도착하지 않으면 항목 4 미구현으로 판정.
3. `interrupt` → `get_state.paused == true`, `resume_session` → `paused == false`, `release_session` → `runPaused == false`.
4. 턴 중 `bash async:true` 실행 → `bash_output` 으로 로그 회수, `kill_bash` 로 종료.
5. `task` 배치 1건 → `get_subagents` 에 run 이 보이고 `cancel_subagent` 가 `cancelled:true` 반환.
6. `/tmp/q/s.jsonl` 첫 줄을 `session-identity.ts:13-22` 로직으로 파싱해 sessionId 가 나오는지 확인(title 슬롯 회귀 검출).
7. 게이트웨이 `MoveHost`(`move-host.ts`)로 붙여 `move_session status` 1회 왕복.

## 7. 위험과 미확인

- 위험: `agent_settled` 를 `session_settled` 지점에 추가하면 게이트웨이가 이미 소비하는 `agent_end`(`brain.ts:326-329`)와 순서가 엇갈려 abort 오독이 재현될 수 있다. `agent_end` → `prompt_result` → `agent_settled` 순서를 고정해야 한다.
- 위험: `Runtime` 다중화(항목 1 (a))는 `runtime.rs:116-…` 의 `Mutex` 필드 대부분이 세션 스코프라 회귀면이 넓다. 단기 (b)로 게이트웨이를 낮추는 편이 안전하다.
- 위험: `--skill`(단수, `gateway.ts:1077-1078`)은 tau에 없고 `--skills`(복수, `cli.rs:129`)만 있다. 이대로면 워커 기동이 `unknown flags` 로 죽는다(`cli.rs:513-518`). 위 4장 1번에서 별칭으로 흡수해야 한다.
- 확인 필요: title 슬롯(`entries.rs:66-77`)이 실제 세션 파일 첫 줄로 기록되는지. 첫 줄이면 `session-identity.ts:24-31` 의 헤더 판독이 실패해 레인 신원이 legacy 로 떨어진다.
- 확인 필요: 게이트웨이가 `negotiate_protocol` 을 보내는지, 보낸다면 어떤 버전을 요구하는지(tau는 2만 수용, `rpc.rs:595-600`).
- 확인 필요: omo-nyang이 `set_host_tools`(`rpc.rs:281-315`)를 실제로 호출하는지. `src/` grep에서 호출부를 찾지 못했다.
- 확인 필요: senpi/mhc `task` 도구의 정확한 입력 스키마. 이번 조사에서는 게이트웨이의 task 수명주기가 허브 DB(`bridge-state.ts:39`)에 있음만 확인했고 엔진측 스키마 대조는 하지 못했다.
- 확인 필요: `move_session` export/import 번들 레이아웃(`move-operations.ts:143` 의 `join(bundleDir, "engine")`)과 tau 세션 디렉터리 매핑.
- 미완: 항목 3의 "도구 입력 형식" 중 호스트 도구 경로는 미확인으로 남았다. 항목 1~5의 확정 결론 자체는 모두 위 파일:줄 근거로 닫았다.
