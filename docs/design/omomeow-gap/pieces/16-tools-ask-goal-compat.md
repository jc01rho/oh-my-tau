# tau(oh-my-tau) 도구·입력 호환 설계 — ask·goal RPC 정규화 계층

## 1. 현재 상태 (근거)

### 1.1 tau의 ask 경로

- 도구 정의는 `crates/omp-cli/src/tools/ask.rs`. 트레이트는 `AskUi`(`ask.rs:74`), 메서드는 `ask(questions: Value, timeout_ms: Option<u64>) -> BoxFut<Option<Value>>`(`ask.rs:79-83`), `has_ask_dialog()`(`ask.rs:88`), `select(...)`(`ask.rs:95`), `editor(...)`(`ask.rs:109`).
- 모델 대면 스키마는 `ask.rs:1409-1430`의 `schema()`로 `{"html": string}`와 레거시 `{"questions": [...]}` 두 형태를 광고한다. 인자 파싱은 `ask.rs:699-760` `parse_params`가 `html`이면 `parse_html_form`(`ask.rs:412`)으로, 아니면 `questions[]`로 푼다.
- 실행 경로는 `ask.rs:1300-1320`의 `run()`: `ui`가 없으면 `"Ask tool requires interactive mode"`, `has_ask_dialog()`가 false면 `run_fallback`(`ask.rs:1267-1296`, 질문별 순차 `select`/`editor`)로 내려간다.
- RPC 송신은 `crates/omp-cli/src/rpc.rs:86-136` `RpcAskUi::request`. `id`는 `ui-{nanos}`(`rpc.rs:96-101`), 프레임은 `{type:"extension_ui_request", id, method, ...}`이고 `timeout`은 있을 때만 들어간다(`rpc.rs:104-110`). `ask`는 `{questions:[...], timeout?}`를 **프레임 최상위**에 넣고 `method:"ask"`로 보낸다(`rpc.rs:148-161`).
- 질문 프레임 항목은 `ask.rs:805-836` `frame_question`: `id`, `question`, `header?`, `options:[{label, description?, preview?}]`, `multi?`, `recommended?`(0-based 숫자), multi일 때 `checked[]`.
- 응답 파싱은 `ask.rs:968-990` `results_from_response` → `ask.rs:886-960` `parse_answers`. 계약이 엄격하다: `answers`가 **배열**이고 길이가 질문 수와 정확히 같아야 하며, 각 원소는 `id` 일치 + `selectedOptions: [label...]` 배열 필수, 선택지는 질문의 label 집합에 속해야 하고, 자유 입력은 `customInput` 문자열. 어긋나면 `"Ask dialog response must carry N answers in question order"`(`ask.rs:890-896`)로 실패한다. 취소는 `{cancelled:true}`, 타임아웃은 `{cancelled:true, timedOut:true}`(`ask.rs:973-984`).
- `set_ask_dialog`는 `rpc.rs:777-785`에서 처리하고 `ASK_DIALOG_ENABLED`(`rpc.rs:83-84`)와 `Runtime` 상태(`runtime.rs:104`, `runtime.rs:464` 기본 false, setter `runtime.rs:3788`)에 반영한다. RPC 루프는 `ask` 도구만 `tool_with_ui(Arc::new(RpcAskUi))`로 교체한다(`rpc.rs:1389-1398`). 응답 라우팅은 `rpc.rs:1474-1480`이 `resolve_pending_ui_request(id, cmd)`로 처리한다.
- TUI 경로는 `crates/omp-tui/src/extension_ui.rs`. `PendingRequest{id, method, params, timeout}`(`:172-181`), `"ask"` 분기가 `params.questions`를 `Question::from_value`로 읽어 `DialogKind::Ask`를 만든다(`:338-360`). `payload_of`는 `params`가 있으면 그것을, 없으면 `type/id/method/extension`을 뺀 프레임 전체를 쓴다(`:665-680`). 알 수 없는 method는 `input`으로 강등된다(`:407-420`). 응답 봉투는 `crates/omp-cli/src/tui.rs:588-604`가 `answers`/`value`/`cancelled`를 최상위에 두고 `resolve_pending_ui_request`로 되돌린다.

### 1.2 tau의 goal 경로

- RPC는 `rpc.rs:776` `"goal" => goal(cmd)`, 본문은 `rpc.rs:250-281`. `get|pause|drop`은 항상 `{"goal":null,"state":null}`, `create`는 `objective`·`token_budget`만 검증한 뒤 `Err(unsupported("Goal mode"))`, `resume`은 `"No paused goal to resume."`, 그 외는 `"Unknown goal op: {}"`. 즉 **RPC goal은 상태를 갖지 않는다**.
- 도구 쪽은 `crates/omp-cli/src/tools/goal.rs`. 스키마 `op` enum은 `create|get|complete|pause|resume|drop`(`goal.rs:18-28`), `GoalRuntime`은 프로세스 내부 상태만 가진다(`goal.rs:131-136`). 직렬화 `Goal::to_json`은 `id/objective/status/tokenBudget/tokensUsed/timeUsedSeconds/createdAt/updatedAt`(`goal.rs:92-105`), `GoalStatus`는 `active|paused|budget-limited|complete|dropped`(`goal.rs:38-62`). 영속 저장소가 없다.
- 도구 등록 자체가 꺼져 있다. `hidden_tools(..., goal_mode_active)`가 true일 때만 `goal::tool()`을 넣고(`tools.rs:484-505`), 호출부는 `runtime.rs:3442`와 `agent.rs:167` 모두 `false`를 넘긴다. 그래서 `goal` 도구는 RPC/에이전트 경로에 존재하지 않는다.
- `/goal` 슬래시는 `slash_extra.rs:474-483`에서 `"/goal: goal mode is not enabled in this session; ..."`만 출력한다.

### 1.3 게이트웨이 측 (저장소 `/data/home/Work/omo-nyang`)

- `src/dialogs.ts:9-26` `dialogSchema`: `method`는 `confirm|select|input|editor|question`만 허용. `questions[]` 항목은 `{id, question, options?: [{label, description?}]}`만 정의되어 `multi`/`checked`/`recommended`/`preview`는 스키마에 없다. `timeout: null`은 `withoutNulls`(`dialogs.ts:5-8`)가 제거한다.
- 수신 분기는 `src/brain.ts:320-323`: `["confirm","select","input","editor","question"]`에 포함될 때만 `onDialog`로 넘긴다. tau가 보내는 `method:"ask"`는 **여기서 걸러져 폐기**된다.
- 응답은 `brain.ts:468`, `brain.ts:484`에서 `{type:"extension_ui_response", id, ...answer}`로 최상위 평면 전송. `DialogAnswer`는 `{value?, confirmed?, cancelled?, comment?, answers?: Record<id,{selected:[], text?}>}`(`dialogs.ts:17-23`), `question` 답변 생성은 `dialogs.ts:80-90`에서 `answers: { [question.id]: { selected, text } }` 맵을 만든다.
- goal은 RPC를 쓰지 않는다. `src/lane-pause.ts:21-32` `goalCommand`가 `"/goal pause"` / `"/goal resume"`을 **프롬프트 텍스트**로 보내고, `lane-pause.ts:60-70`의 `goalProbe`가 `<session dir>/extensions/goal/<sid>.json` 파일을 읽어 `goal` 상태를 확인한다. 상태 집합은 `active|paused|blocked|complete|none`(`lane-pause.ts:9`).
- mhc 바이너리 문자열(`/data/home/.local/share/mhc/versions/2026.11.20-omomeow1/mhc`) 기준으로 mhc는 `ask_user_question`/`request_user_input` 도구와 `questions/header/waitForAnswer/id/label/description/multiSelect` 필드, 오류 코드 `invalid_answer_id`/`question_incomplete`/`question_already_resolved`/`unknown_extension_ui_request`, 이벤트 `question_resolved`를 갖는다. goal은 확장 크레이트(`crates/extensions/maho-ext-pi-goal/src/goal/{store,tool_registration,command_registration}.rs`)와 도구 `create_goal`/`get_goal`/`update_goal`로 구현된다.

## 2. 게이트웨이가 요구하는 계약

1. 대화상자 프레임: `method ∈ {confirm, select, input, editor, question}`, `id: string` 필수, `questions[]`는 `{id, question, options?: [{label, description?}]}`만 의미 있고 나머지 키는 zod 기본 동작으로 **조용히 버려진다**(`dialogs.ts:9-26`).
2. 대화상자 응답: `extension_ui_response` 평면 봉투에 `answers`가 **id 키 맵**이고 값은 `{selected: string[], text?: string}`. 자유 텍스트는 `text`, 취소는 `cancelled:true`, 추가 의견은 `comment`(`dialogs.ts:17-23`, `:80-90`).
3. 자유 입력 이탈구: 게이트웨이는 `"Other (type an answer)"`(`dialogs.ts:29`)를 select의 마지막 옵션으로 가정한다. tau는 `"Other (type your own)"`(`ask.rs:44`)를 쓴다. 문자열이 다르다.
4. goal: RPC 명령이 아니라 `/goal pause|resume` 프롬프트 + `extensions/goal/<sid>.json` 파일 상태. 상태 어휘는 `active|paused|blocked|complete|none`.
5. `set_ask_dialog`를 게이트웨이가 보내지 않는다(`src` 전체 grep에서 송신 0건). 따라서 tau는 항상 `has_ask_dialog()==false`로 판단하고 강등 경로(`ask.rs:1315-1317`)로 간다.

## 3. 설계

핵심은 **tau 내부 표현(omp 계약)을 유지하고, 경계에서만 senpi/mhc 표현으로 정규화**하는 것이다. 새 모듈 `crates/omp-cli/src/rpc/compat.rs`에 순수 함수만 두고, `rpc.rs`·`omp-tui`·`tools/goal.rs`는 이 함수를 호출하도록 최소 수정한다.

### 3.1 모드 결정

`crates/omp-cli/src/cli.rs`의 `STRING_FLAGS`에 `--rpc-compat <omp|senpi>`를 추가하고, `~/.omo/omo.jsonc`의 `rpc.compat`으로도 설정 가능하게 한다. 기본값은 `omp`(현행 동작 보존). `senpi` 모드에서만 아래 변환이 켜진다. 판별 근거는 `get_protocol_info` 부재이므로 **플래그가 유일한 스위치**다.

### 3.2 송신 정규화: `normalize_dialog_request(method: &str, frame: Map<String,Value>) -> Map<String,Value>`

- `method == "ask"` → `method = "question"`. 게이트웨이 enum에 `ask`가 없으므로 이 변환이 없으면 대화상자가 통째로 사라진다(`brain.ts:320-323`).
- `questions[]` 각 항목을 게이트웨이 스키마 투영으로 재작성: `id`, `question`, `options:[{label, description?}]`만 남기고 `multi`/`checked`/`recommended`/`preview`는 제거. 대신 원본은 `x-omp-questions` 같은 별도 키에 보존하지 않는다(게이트웨이가 버리므로 무의미). multi 정보는 **질문 텍스트에 접미**로 흡수한다(`" (복수 선택 가능)"`).
- `title`을 첫 질문의 `header` 또는 `question`으로 채운다. 게이트웨이는 `title`을 Slack 메시지 제목으로 쓴다(`dialogs.ts:showDialog`).
- `options`에 `"Other (type an answer)"`가 없으면 추가해 게이트웨이의 자유 입력 경로(`freeTextOf`)를 살린다. tau의 예약 라벨 검사(`ask.rs:757-772`)는 도구 입력 단계에서 이미 끝났으므로 프레임 단계 추가는 안전하다.

### 3.3 수신 정규화: `normalize_dialog_response(questions: &[QuestionItem], frame: &Value) -> Result<Option<Value>>`

`ask.rs:968-990`을 이 함수로 대체 위임한다. 처리 순서:

1. `cancelled:true` → `timedOut` 유무로 `timed_out_results`(`ask.rs:844-884`) 또는 `Ok(None)`.
2. `answers`가 배열이면 omp 원형이므로 **그대로** `parse_answers`에 넘긴다(기존 테스트·TUI 경로 보존).
3. `answers`가 객체(맵)이면 senpi 형: 각 `id`에 대해 `selected`(또는 `selectedOptions`)를 label 배열로, `text`(또는 `customInput`)를 `customInput`으로 옮겨 `[{id, selectedOptions, customInput}]` 배열을 **질문 순서대로** 재구성한다. 맵에 없는 id는 `selectedOptions: []`로 채우고, 알 수 없는 id는 무시하고 경고 로그만 남긴다(`invalid_answer_id` 대응).
4. 최상위 `value: string`(select 강등 응답, `rpc.rs:196-199`)이면 단일 질문의 label 매칭으로 환산한다.
5. `comment`는 tau에 대응 필드가 없으므로, 단일 질문이면 `customInput` 뒤에 개행으로 붙이고, 다중 질문이면 결과 텍스트 말미에 별도 줄로 덧붙인다(`render_results`, `ask.rs:1326-1360` 수정).

### 3.4 `set_ask_dialog` 부재 보정

senpi 모드에서는 `ASK_DIALOG_ENABLED` 초기값을 true로 두고(`rpc.rs:83-84` 교체 지점), `set_ask_dialog`가 오면 그 값으로 덮는다. 이렇게 해야 `ask`가 강등 경로 대신 단일 `question` 프레임으로 나가고, 게이트웨이가 한 메시지에 모든 질문을 담아 보여줄 수 있다.

### 3.5 TUI 정합

`omp-tui/src/extension_ui.rs:338-360`의 분기에 `"question"`을 `"ask"`와 동일 처리로 추가한다(`params.questions` → `DialogKind::Ask`). 현재는 `:407-420`에서 `input`으로 강등되어 질문 폼이 사라진다. `ask_advance`(`:943-960`)가 만드는 배열 응답은 그대로 유효하다.

### 3.6 goal 정규화

- **RPC 상태화**: `goal(cmd)`(`rpc.rs:250-281`)를 `Runtime`이 보유한 `Arc<Mutex<GoalRuntime>>`에 위임한다. `create|get|pause|resume|complete|drop`을 실제 상태로 처리하고, 응답은 `{goal: Goal::to_json(...) | null, state: ...}`.
- **입력 별칭**: `normalize_goal_command(cmd) -> GoalOp`가 `{op}`(tau/omp), `{type:"create_goal"|"get_goal"|"update_goal"}`, `{codexObjective}`를 모두 받는다. mhc 문자열 기준 도구 이름이 `create_goal`/`get_goal`/`update_goal`이므로 이 별칭이 필요하다.
- **상태 어휘 사상**: `GoalStatus` → 게이트웨이 어휘 `active|paused|blocked|complete|none`. `budget-limited`는 `blocked`(사유 `budget`), `dropped`는 `none`, `Goal` 없음은 `none`. 게이트웨이가 `blocked`를 기대하므로(`lane-pause.ts:9`) 이 사상이 없으면 pause 확인 루프가 실패한다.
- **파일 저장**: `lane-pause.ts:60-70`이 읽는 `<session dir>/extensions/goal/<session id>.json`에 `{"goal": {...}}`를 기록한다. 쓰기 지점은 `GoalRuntime` 변경 직후 1곳, 경로 계산은 `crates/omp-cli/src/persist.rs`의 세션 디렉터리 헬퍼를 재사용한다(정확한 함수명 확인 필요).
- **슬래시**: `slash_extra.rs:474-483`의 고정 문자열을 goal 런타임 호출로 바꾼다. `/goal pause|resume`이 게이트웨이의 유일한 goal 조작 수단이므로 이것이 없으면 goal 기능 전체가 죽는다.
- **도구 등록**: `runtime.rs:3442`, `agent.rs:167`의 `goal_mode_active=false`를 설정(`goal.enabled` + 상태 존재)으로 바꾸고 `runtime_tool(Some(rt))`(`goal.rs:300`)를 넘긴다.

## 4. 구현 단계 (커밋 단위)

1. `feat(rpc): compat 모듈 추가` — `crates/omp-cli/src/rpc/compat.rs`에 `normalize_dialog_request`, `normalize_dialog_response`, `normalize_goal_command`, `goal_state_wire` 순수 함수와 단위 테스트만. 배선 없음. 회귀 위험 0.
2. `feat(cli): --rpc-compat 플래그` — `cli.rs`의 `STRING_FLAGS`/`parse_from`에 추가, `Runtime` 상태 필드 1개, 기본 `omp`.
3. `feat(rpc): ask 송신을 compat에 위임` — `rpc.rs:148-161`이 `normalize_dialog_request`를 거치게 하고, senpi 모드에서 `ASK_DIALOG_ENABLED` 초기값 true.
4. `feat(rpc): ask 응답 정규화` — `ask.rs:968-990`을 compat 경유로 바꾸고 맵/배열 양형 수용, `comment` 병합. 기존 `parse_answers` 엄격성은 배열 경로에 그대로 유지.
5. `feat(tui): question 대화상자` — `extension_ui.rs` 분기에 `"question"` 추가, 스냅샷 테스트.
6. `feat(goal): RPC 상태화와 파일 저장` — `GoalRuntime`을 `Runtime`에 배선, `goal(cmd)` 재작성, `extensions/goal/<sid>.json` 기록.
7. `feat(goal): /goal 슬래시와 도구 등록` — `slash_extra.rs:474-483` 교체, `hidden_tools` 게이트 활성화.
8. `docs(rpc): senpi 호환 계약` — `crates/omp-cli/src/rpc.rs` 모듈 doc에 두 표현의 차이와 모드 표.

## 5. 테스트

- `compat.rs` 단위: (a) `ask` → `question` 변환과 `multi`/`checked` 제거, (b) `Other (type an answer)` 자동 추가, (c) omp 배열 응답 통과, (d) senpi 맵 응답 → 배열 재구성(질문 순서 보존), (e) 맵에 빠진 id → 빈 선택, (f) `value` 단일 응답 환산, (g) `comment` 병합, (h) `cancelled+timedOut` → 권장 옵션 자동 선택(`ask.rs:844-884` 결과와 동일).
- `ask.rs` 통합: 기존 `FakeUi`(`ask.rs:1440`)를 senpi 형 응답을 돌려주는 변형으로 하나 추가해 `run()`이 같은 `User answers:` 텍스트를 내는지 확인. 기존 omp 형 테스트(`ask.rs:1549-1660`)는 손대지 않는다.
- `goal.rs`: `create → get → pause → resume → complete` 왕복, 파일 저장 후 재로드, 상태 어휘 사상(`budget-limited`→`blocked`, `dropped`→`none`) 표 기반 테스트.
- `extension_ui.rs`: `method:"question"` 프레임이 `DialogKind::Ask`로 열리고 `answers` 배열을 내는 스냅샷.
- `rpc.rs`: 기존 `goal` 테스트(`rpc.rs:1633-1652`)를 상태 있는 동작으로 갱신. `Unknown goal op` 케이스는 유지.

## 6. QA 시나리오

1. `omt --mode rpc --rpc-compat senpi`를 띄우고 `{"type":"prompt",...}`로 ask 도구를 호출하게 한다. 표준출력에 `method:"question"` 프레임이 `id`, `title`, `questions[].options[].label`과 함께 나오는지 확인.
2. 같은 프로세스에 `{"type":"extension_ui_response","id":"...","answers":{"q1":{"selected":["SQLite"]}}}`를 넣어 도구 결과가 `q1: SQLite`로 돌아오는지 확인.
3. `{"answers":{"q1":{"selected":[],"text":"직접 입력"}}}`로 자유 입력 경로 확인, `{"cancelled":true}`로 취소 오류(`ask.rs:1067`) 확인.
4. `--rpc-compat omp`에서 기존 프레임(`method:"ask"`, 배열 응답)이 그대로 나가고 기존 테스트가 통과하는지 확인(회귀).
5. goal: `{"type":"goal","op":"create","objective":"x"}` 후 `get`이 `status:"active"`, `pause` 후 `paused`, `extensions/goal/<sid>.json` 내용이 `{"goal":{"status":"paused"}}`인지 확인.
6. 게이트웨이 연동: `lane-pause.ts`의 `pauseGoal`을 실행해 `readBackTries` 안에 `confirmed`가 나오는지, 즉 `/goal pause` 프롬프트가 실제로 상태를 바꾸는지 확인.
7. TUI: 대화상자에서 다중 질문 폼이 열리고, `Esc`가 `{"cancelled":true}`를 보내는지 확인.

## 7. 위험과 미확인

- **확인 필요**: `/data/home/Work/omo-nyang`가 실제 배포된 OmOMeow 게이트웨이인지. 코드 형태(`brain.ts`가 `extension_ui_request`를 소비하고 mhc식 명령을 보냄)는 일치하지만 배포본 동일성은 확인하지 못했다.
- **확인 필요**: 게이트웨이 zod 객체가 미정의 키를 strip하는지 passthrough하는지. strip이면 `multi` 정보가 확실히 사라지므로 3.2의 텍스트 접미 방식이 유일한 해법이고, passthrough면 별도 키 보존이 더 낫다.
- **확인 필요**: `question_resolved` 이벤트를 게이트웨이가 요구하는지. tau는 이 이벤트를 emit하지 않는다(mhc 문자열에는 존재). `waitForAnswer` 의미론과 함께 미확인.
- **확인 필요**: `session dir` 계산에 쓸 `persist.rs`의 정확한 함수명과, `extensions/goal/<sid>.json`의 `sid`가 tau 세션 id와 같은 체계인지.
- 위험: `ask.rs:886-960`의 엄격 검증을 완화하면 omp 원형 계약이 무너진다. 배열 경로는 반드시 기존 코드를 그대로 통과시켜야 한다(테스트로 고정).
- 위험: goal을 실제 상태로 만들면 `create`가 `unsupported`를 반환하던 기존 호출자가 성공을 받는다. 의미 변화이므로 커밋 6에 명시적 변경 노트가 필요하다.
- 위험: senpi 모드에서 `ASK_DIALOG_ENABLED` 기본 true는 `--no-ui` 경로와 충돌할 수 있다. `--no-ui`일 때는 강제 false로 두어야 한다.
- 미확인: tau의 `id` 형식 `ui-{nanos}`(`rpc.rs:96-101`)가 게이트웨이의 `expiresAt`/`timeout` 추적과 충돌하지 않는지. 게이트웨이는 `timeout`을 기록만 하고 tau는 `timeout`을 보내지 않으므로 실질 영향은 없어 보이나 확인 필요.
- 이 조각은 ask·goal만 다룬다. `interrupt`, `set_client_info`, `get_protocol_info`, `reload`, `resume_session` 등 디스패치 부재 8종과 도구 부재 5종은 다른 조각 범위이며, 여기서 설계한 `compat.rs`가 그 정규화의 공통 착지점이 될 수 있다.
