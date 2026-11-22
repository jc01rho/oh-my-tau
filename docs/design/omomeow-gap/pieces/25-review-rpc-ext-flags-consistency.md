# tau RPC 8종·실행 플래그·확장 API 교차 정합성 리뷰와 세션 상태 기계 통합 계약

## 1. 현재 상태 (근거)
기준 커밋: oh-my-tau `e7a5378` (main, depth 1 클론). 게이트웨이 근거는 `~/Work/omo-nyang/src` 기준이다.

- 디스패치: `rpc::handle()`(crates/omp-cli/src/rpc.rs:562)이 `dispatch()`(rpc.rs:593)를 부른다. 명령은 match 하나로 나뉘고, 처리되지 않은 명령은 rpc.rs:1094 `other => Err(CmdError::new(format!("Unknown command: {other}")))` 로 떨어지며 코드가 붙지 않는다. 8종(interrupt, set_client_info, get_protocol_info, reload, resume_session, release_session, extension_request, move_session)은 match 팔에 없다.
- 실행 모델: stdin 루프(rpc.rs:1434 부근)는 명령을 `cmd_tx`로 넘기고, 워커 태스크 하나가 도착 순서대로 직렬 처리한다(rpc.rs:1436-1450, `busy: AtomicBool`). 예외는 셋이다. `bash`는 `JoinSet`에서 따로 돈다. `extension_ui_response`는 루프가 바로 처리한다. `abort`는 `busy`일 때 큐를 건너뛴다(rpc.rs:1500-1506 부근).
- Runtime은 프로세스당 하나다. `rpc::run_with`가 `Runtime::start`를 한 번 부르고(rpc.rs:1383), `RUNTIME_TX` thread-local 슬롯이 있으며(rpc.rs:1382), 확장 호스트는 `slash::set_extensions` 전역으로 붙는다(main.rs:548). 다중 세션 개념은 없다.
- 응답 프레임: `response()`(rpc.rs:203-226)는 `{id, type:"response", command, success, data|error, code?}` 형식이다. `CmdError { message, code: Option<&'static str> }`(runtime.rs:34-58)이고, `CmdError::coded` 호출은 omp-cli/src 전체에 11곳뿐이다(예: `unknown_since` rpc.rs:800, `session_busy` rpc.rs:753 부근).
- 프로토콜: `ready` 프레임은 `protocolVersion:1, supportedProtocolVersions:[1,2]`를 보낸다(rpc.rs:1418 부근). `negotiate_protocol`은 2만 받는다(rpc.rs:595). 서버 버전을 알리는 경로는 없다.
- 중단: `"abort"`는 `Runtime::abort()`(rpc.rs:677 → runtime.rs:1844-1851)로 간다. 이 함수는 `user_interrupt` 플래그를 켜고 `abort_retry`, `abort_compaction`, `agent.abort`, `wait_for_idle`을 차례로 부른다. "paused" 상태는 없고 `set_interrupt_mode`는 `immediate|wait` 설정값일 뿐이다(rpc.rs:954-961).
- 상태 조회: `Runtime::state()`(runtime.rs:1953-2011)가 내는 키는 isStreaming, isSettled, sessionId, interruptMode, hasPendingAsyncWork, queuedMessages 등이다. `paused`, `runPaused`는 없다. `sessionFile`은 이 구간에서 찾지 못했다(확인 필요).
- 세션 전환 순서: `new_session`(runtime.rs:2215-2253)은 `check_switch` → `cancel_compaction_and_wait` → `cancel_retry_and_wait` → `agent.abort` → `wait_for_idle` → `reset` → `persist.start_new` → `bump_generation` → `notify_session_switch` 순서다. `open_session`은 `sessionDir`만 받고, 받은 디렉터리의 최신 파일을 연다(rpc.rs:730-764).
- 종료 신호: rpc 쪽은 `report_prompts_now`(runtime.rs:1611-1643)가 `prompt_result`와 `session_settled`를 보낸다. `agent_settled`는 확장 쪽에만 가고, 그것도 `has_handlers("agent_settled")`일 때만 `emit_nowait`로 나간다(ext.rs:1593-1598).
- 확장 호스트: `HostMessage::{Notification, Request}`(crates/omp-ext/src/host.rs:103-113), `ExtHost::request`(host.rs:463), `emit`(host.rs:544)가 있다. 코어가 받는 알림은 append_entry, send_message, log, ui_notify 등이다(ext.rs:1240-1266). ext-host/host.ts의 api 표면은 on, events.on, registerTool, registerCommand, sendMessage, exec이고(host.ts:356-445) `rpc`가 없다. 모르는 메서드는 JSON-RPC `-32601`(host.ts:1060), 모르는 도구·명령은 `-32004`(host.ts:949, 966)로 답한다. 호스트가 재연결되면 확장과 세션 ctx를 다시 올리는 경로가 이미 있다(ext.rs:994-1003).
- CLI: `STRING_FLAGS`(cli.rs:97)에 `--skills`가 있고(cli.rs:129, 309) 단수형 `--skill`은 없다. `OPTIONAL_FLAGS`(cli.rs:138)와 `VALUELESS_FLAGS`(cli.rs:142) 어디에도 4종 플래그가 없다. 모르는 플래그는 `unrecognized`에 쌓였다가(cli.rs:433) 파싱 오류가 된다(cli.rs:512-516). 따라서 게이트웨이 argv로는 tau가 기동조차 하지 않는다.
- 모델 폴백: `retry.fallbackChains`로 동작한다(crates/omp-cli/src/recovery.rs:1, 55, 256). 이를 끄는 CLI 경로는 없다. "recommended models" 개념은 omp-cli에서 찾지 못했다(확인 필요).

## 2. 게이트웨이가 요구하는 계약
- Stop/재개(lane-hold.ts:29-76): `set_client_info{width, capabilities:["interactive_user"]}` 다음 `interrupt{intent:"stop"}`을 보내고, `get_state`에서 `paused:true`를 기대한다. 해제할 때는 `get_protocol_info`의 `serverVersion`을 읽는다. 엔진이 `2026.11.3-3` 이상이면 run 재개를 엔진에 맡긴다(`engineResumesRun`, lane-hold.ts:48-55). 아니면 `runPaused`일 때 `reload{}→{cancelled?}`를 먼저 보내고 `resume_session{}`을 보낸다. 이미 해제된 상태에서 `resume_session`을 보내면 `not_paused`로 실패해야 한다.
- pause seq: Stop이 run을 펜싱할 때 run의 pause는 `session_paused` seq로 찍히는데, 해제는 run 펜스가 올린 seq를 들고 와서 run이 계속 멈춰 있던 버그가 있었다(lane-hold.ts:37-47). 그래서 seq 체계는 하나여야 하고, 세션을 다시 시작할 때 seq를 다시 읽어야 한다.
- 이동(relay/move-operations.ts): 먼저 `open_session{sessionPath, cwd, attended:false}→{sessionId}`를 보낸다. 이 sessionId는 라우팅 id(`rpc-N`)이고 `state.sessionId`와 다르다(:206-210). 이어서 `extension_request{sessionId, name:"omomeow.move-readiness"}→{activeSources[]}`(:212), `release_session{sessionId, reason:"move", moveId, target, interrupt, force}→{session_path, dropped:{user_messages[]}}`(:216, :25)를 보낸다. `move_session`은 op 6개를 쓴다: `prepare|cancel`(:104, :109), `export{sessionPath, moveId, target, droppedUserMessages}→{move_id, session_id, bundle_dir, files}`(:219-222, :26), `import{bundleDir, cwd, from}→{session_id, session_path}`(:143, :19), `status`는 `{state: receiving|accepted|declined|failed, reason?}`로 답한다(:151, :17), `finish`는 `{ownership: moving|moved|writable}`로 답한다(:161-162).
- 에러: MoveHost는 `errorCode` 필드를 읽는다(relay/move-host.ts:6, 62). tau는 `code`를 보내므로 지금은 전부 `rpc_failed`로 뭉개진다.
- 기동 플래그는 `--mode rpc --multi-session --session-runtime in-process -e …`(relay/move-host.ts:37-38), `--no-model-fallback --no-recommended-models`(relay/split.ts:78, model-routing.ts:644), `--skill <dir>`(gateway.ts:1079)다.
- 확장: 게이트웨이는 `api.on("session_move_export"|"session_move_import", (event, ctx) => …)`을 등록하고 `ctx.sessionManager.getSessionFile()`을 쓴다(session-move-extension.ts:115-117). `api.rpc.handle`은 move-readiness 같은 `extension_request` 대상이다. 등록 쪽 코드는 이번 조사에서 직접 보지 못했다(확인 필요).
- 종료: 게이트웨이는 `agent_settled` rpc 프레임을 기다린다(gateway.ts, brain.ts, relay/agentd-sessions.ts).

## 3. 설계
**3.1 세션 상태 기계.** 새 모듈 `crates/omp-cli/src/session_ctl.rs`에 `SessionCtl { state: Mutex<CtlState>, pause_seq: AtomicU64 }`를 두고 `Runtime` 필드로 붙인다(runtime.rs:116).

`enum CtlState { Active, Paused { seq: u64, run_paused: bool }, Moving { move_id: String, phase: MovePhase }, Released { session_path: PathBuf } }`

| 현재 | 명령 | 다음 | 실패 코드 |
|---|---|---|---|
| Active | interrupt{intent:stop} | Paused{seq+1} | - |
| Active | interrupt{intent:abort 또는 생략} | Active (`Runtime::abort`와 같음) | - |
| Paused | resume_session{seq?} | Active | seq 불일치 `stale_pause_seq` |
| Active | resume_session | 변화 없음 | `not_paused` |
| Active, Paused | release_session{force} | Released | 실행 중이고 force가 없으면 `session_busy` |
| Active | move_session prepare | Moving{Prepared} | 다른 moveId면 `move_in_progress` |
| Moving | move_session cancel | Active | moveId 불일치 `move_unknown` |
| Released | prompt, steer 등 모든 변경 명령 | 변화 없음 | `session_released` |
| Moving | prompt 등 | 변화 없음 | `move_in_progress` |

- 상태 검사는 `dispatch()` 맨 앞에서 한다. `CtlState::admits(kind)`가 `CmdError`를 돌려준다. `get_state`, `get_protocol_info`, `move_session status`, `get_messages*`는 어느 상태에서나 허용한다.

**3.2 pauseSeq.** 세션마다 `pause_seq` 하나만 쓰는 단조 증가 u64다.
- `interrupt stop`은 seq를 올리고 그 값으로 run 펜스와 세션 pause에 같은 숫자를 찍는다. 그다음 `session_paused{seq}` 프레임을 보낸다. 두 번째 카운터는 두지 않는다. lane-hold.ts:37-47의 버그를 구조적으로 막기 위해서다.
- seq는 `persist` JSONL v3에 custom entry `ctl_pause{seq, run_paused}`로 남긴다. `reload`와 `load_session_as`는 이 entry를 다시 읽어 상태를 복원한다.
- `get_state`에 `paused`, `runPaused`, `pauseSeq`, `ctlState`, `sessionFile`을 추가한다(runtime.rs:1953).

**3.3 신규 RPC 응답 계약.**
- `set_client_info`: `Settings`에 `client_width`, `client_capabilities`를 저장하고 `none()`을 돌려준다.
- `get_protocol_info`: `{serverName:"omt", serverVersion:env!("CARGO_PKG_VERSION"), protocolVersion, supportedProtocolVersions:[1,2], capabilities:[…8종, "multi_session"]}`.
- `reload`: Paused에서만 허용한다. 확장 재적재는 ext.rs:994-1003 경로를 재사용하고, 그다음 `ctl_pause`를 다시 읽는다. 응답은 `{cancelled:false}`다. `session_before_switch` gate가 거부하면 `{cancelled:true}`를 돌려준다.
- `interrupt`가 `busy` 상태라면 `abort`처럼 직렬 워커를 우회한다(rpc.rs:1500 조건에 추가).

**3.4 종료 신호.** `report_prompts_now`에서 `settle`이 참일 때 `session_settled` 바로 뒤에 rpc 프레임 `{"type":"agent_settled"}`를 보낸다(runtime.rs:1641). 확장용 emit(ext.rs:1593)은 그대로 둔다. Paused로 갈 때는 `agent_end`, `session_paused{seq}`, `agent_settled` 순서로 보낸다.

**3.5 에러 코드 표준화.**
- `response()`가 `code`와 `errorCode`에 같은 값을 넣는다(rpc.rs:220).
- 코드 상수 표를 `crates/omp-cli/src/rpc/codes.rs`에 둔다: `unknown_command`, `invalid_params`, `session_busy`, `not_paused`, `stale_pause_seq`, `session_released`, `move_in_progress`, `move_unknown`, `unknown_session`, `extension_not_found`, `extension_failed`, `unsupported`, `unknown_since`.
- 확장 호스트의 JSON-RPC 숫자 코드는 이렇게 바꿔 쓴다: `-32601`과 `-32004`는 `extension_not_found`, 그 밖의 숫자는 `extension_failed`. `str_field` 실패는 `invalid_params`로 바꾼다(rpc.rs:228).

**3.6 다중 세션.**
- `Args.multi_session`이 켜지면 `rpc::run_with`가 `SessionRegistry { slots: tokio::sync::Mutex<HashMap<String, Arc<Slot>>>, next: AtomicU64 }`를 만든다. `Slot`은 `{ rt: Arc<Runtime>, cmd_tx, busy }`이고 세션마다 직렬 워커를 하나씩 둔다.
- 명령의 `sessionId`(라우팅 id `rpc-N`)로 slot을 찾는다. 없으면 `unknown_session`이다. 나가는 프레임에는 `sessionId`를 붙인다.
- `open_session`은 `sessionPath`, `cwd`, `attended`도 받아서 `{sessionId: "rpc-N", durableSessionId, sessionFile}`을 돌려준다. 기존 `sessionDir` 경로는 유지한다.
- `RUNTIME_TX`(rpc.rs:1382)와 `slash::set_extensions`(main.rs:548) 전역은 slot 단위로 내린다.
- `--session-runtime`은 `in-process`만 받고, 다른 값은 기동 시 `bail!`한다.

**3.7 확장 API.**
- `api.rpc.handle(name, fn)`: ext-host/host.ts의 api 객체(host.ts:356-445)에 추가한다. `LoadResult`(host.rs:127)에 `rpc_handlers: Vec<String>`을 더해서, 코어가 왕복 없이 `extension_not_found`를 판정할 수 있게 한다.
- 코어 `extension_request{sessionId, name, params}`는 `ExtHost::request("rpc_call", {name, params, ctx})`(host.rs:463)를 부르고, host.ts는 메서드 switch(host.ts:1060 앞)에서 받는다.
- `session_move_export` 이벤트는 `move_session export` 안에서, `session_move_import` 이벤트는 `import` 안에서 `ExtHost::emit(event, Some(ctx))`로 보낸다(host.rs:544). ctx에는 `sessionFile`을 넣어서 `ctx.sessionManager.getSessionFile()`이 동작하게 한다.

**3.8 락 순서.** 반드시 다음 순서로 잡는다: `SessionRegistry.slots`(tokio) → `SessionCtl.state`(std) → Runtime 내부 std Mutex(`prompts`, `settings`, `recovery` 등, runtime.rs:124-196) → `ExtHost.conn`(host.rs:212).
- std Mutex는 `.await`를 넘겨 들고 있지 않는다. 규칙은 "상태를 CAS로 전이하고 락을 놓은 뒤 await"다. registry 락은 조회와 삽입에만 쓴다.
- `ExtHost::request`와 `emit`은 어떤 std 락도 잡지 않은 채 부른다.
- 이 순서를 어기는 기존 코드가 있는지는 runtime.rs 전체를 검토하지 못했다(확인 필요).

**3.9 플래그.**
- `--multi-session`, `--no-model-fallback`, `--no-recommended-models`는 `VALUELESS_FLAGS`(cli.rs:142)에 넣는다.
- `--session-runtime`은 `STRING_FLAGS`에 넣는다.
- `--skill <path>`는 `--skills` 목록에 하나를 덧붙이는 별칭으로 처리한다(cli.rs:309). `--skills`의 구분자 의미는 확인 필요다.
- `--no-model-fallback`은 `recovery`의 fallbackChains를 비운다(recovery.rs:55).

## 4. 구현 단계 (커밋 단위)
1. `rpc: 에러 코드 표준화`. codes.rs를 추가하고, `response()`에서 `errorCode`를 함께 보내고, Unknown command에 `unknown_command`, `str_field` 실패에 `invalid_params`를 붙인다.
2. `cli: 게이트웨이 기동 플래그 5종 파싱`. `Args.{multi_session, session_runtime, no_model_fallback, no_recommended_models}`를 추가하고 `--skill` 별칭을 단다. 동작은 아직 연결하지 않고, `session_runtime != in-process`는 거부한다.
3. `runtime: SessionCtl 상태 기계와 pause_seq`. CtlState, `ctl_pause` entry, get_state 필드, `dispatch` 입장 검사를 넣는다.
4. `rpc: interrupt/resume_session/set_client_info/get_protocol_info/reload`. interrupt는 busy일 때 직렬 워커를 우회한다.
5. `rpc: agent_settled rpc 프레임`. runtime.rs:1641에서 보낸다.
6. `ext: api.rpc.handle과 extension_request`. host.ts, host.rs LoadResult, ext.rs를 고친다.
7. `rpc: 다중 세션 SessionRegistry와 open_session 라우팅 id`.
8. `rpc: release_session과 move_session 6 op, session_move_export/import 이벤트`.
9. `recovery: --no-model-fallback 연결, --no-recommended-models 동작 정의`. 동작 정의는 mhc 의미를 확인한 뒤 정한다.

## 5. 테스트
- runtime.rs의 프레임 수집 테스트(runtime.rs:5277, 5729에서 `session_settled`를 기다리는 방식)를 그대로 쓴다. 검증할 것: `interrupt stop` 뒤 `session_paused{seq:1}`, `agent_settled` 순서, `get_state.paused==true`. `resume_session{seq:0}`은 `stale_pause_seq`, 두 번째 `resume_session`은 `not_paused`여야 한다.
- 상태 표의 각 칸마다 단위 테스트 하나씩(`CtlState::admits`). Released 상태의 `prompt`가 `session_released`이고 `errorCode`와 `code`가 같은지 본다.
- reload 복원: `ctl_pause` entry를 쓴 세션 파일을 `load_session`하면 `paused`, `pauseSeq`가 복원돼야 한다.
- cli.rs 테스트(cli.rs:571 형식): 게이트웨이 argv 세 벌(move-host.ts:37, split.ts:78, gateway.ts:1079)이 오류 없이 파싱되는지, `--session-runtime other`가 오류인지 본다.
- 다중 세션: `open_session` 두 번에 `rpc-1`과 `rpc-2`가 나오고, 프레임의 `sessionId`가 분리되며, 모르는 id는 `unknown_session`이어야 한다.
- 확장: ext-host 테스트 확장이 `api.rpc.handle("x")`를 등록하면 `extension_request{name:"x"}`가 성공하고, `name:"y"`는 `extension_not_found`여야 한다.
- 비동기 대기는 프레임 구독 뒤에 트리거하고 시간 제한을 둔 await로 한다. sleep 기반 대기는 쓰지 않는다.

## 6. QA 시나리오
1. lane-hold 재현: `omt --mode rpc`에 lane-hold.ts:29-33의 순서(set_client_info, interrupt, get_state)를 보내고 `paused:true`를 확인한다. 이어서 releaseSession 흐름(get_protocol_info, reload, resume_session, get_state)에서 `paused`와 `runPaused`가 모두 false인지 본다.
2. task 자식이 도는 중에 Stop을 걸고 resume한다. 자식이 재개되는지 확인한다. omo 5.1.48 회귀 검사다. 다만 tau에는 task 제어 도구가 없어서 자식 재개 경로 자체가 확인 필요다.
3. 이동 전체 흐름: MoveHost argv로 기동해 open_session, extension_request(move-readiness), release_session, move_session export를 보내고, 받는 쪽에서 import, status, finish를 보낸다. `ownership:"moved"`가 나오는지, `session_move_export/import` 핸들러가 `getSessionFile()`로 실제 경로를 받는지 확인한다.
4. 에러 표면: Released 세션에 prompt를 보냈을 때 MoveHost가 `rpc_failed`가 아닌 `session_released`를 받는지 본다(move-host.ts:62).
5. 종료 신호: 일반 prompt 하나를 끝까지 돌리고 게이트웨이 브레인이 `agent_settled` 하나로 턴 종료를 인지하는지 본다.
6. 플래그: split.ts:78 argv로 기동했을 때 fallbackChains가 있어도 429에서 `retry_fallback_applied` 프레임이 나오지 않는지 본다.

## 7. 위험과 미확인
- 버전 판정: `serverVersion`을 `0.12.0`으로 내면 `engineResumesRun`이 false를 돌려서(lane-hold.ts:48-55) 게이트웨이가 항상 reload 경로를 탄다. 둘 중 하나를 정해야 한다. reload 경로를 tau에서 확실히 동작시키거나, 게이트웨이가 `serverName`별로 판정하게 바꾸거나. 권장은 전자이고, 결정은 확인 필요다.
- `--no-recommended-models`가 mhc에서 정확히 무엇을 끄는지 확인하지 못했다(확인 필요). tau에는 대응 개념이 없다.
- 다중 세션은 전역 상태 때문에 위험이 크다. `RUNTIME_TX`, `slash::extensions`, approval `Gate::get()`(rpc.rs:1384)이 걸리고, 확장 호스트 하나가 `ctx_provider`(host.rs:209)를 하나만 가진다. 세션별 ctx 분리 방식은 확인 필요다.
- 세션 형식: 이동 번들은 omp JSONL v3(crates/omp-session)이고 게이트웨이와 mhc 쪽은 senpi 형식이다. `import`가 형식 변환을 맡을지, 엔진끼리만 이동을 허용할지 확인 필요다.
- `open_session`의 `sessionId` 의미가 바뀐다(durable id에서 라우팅 id로). 기존 tau 클라이언트(`sessionDir` 사용)와의 호환은 응답에 `durableSessionId`를 함께 넣는 방법으로 완화한다.
- 직렬 워커: `release_session{force}`가 실행 중인 prompt 뒤에 줄 서면 교착이 생길 수 있다. `abort`처럼 직렬 워커를 우회시킬지 결정해야 한다(rpc.rs:1500).
- `api.rpc.handle`의 게이트웨이 쪽 등록 형태(이름 규칙 `omomeow.*`, params 스키마)와 `omomeow.move-readiness`의 `activeSources` 산출 기준은 tau에 대응 개념(bash_output, monitor, task 자식)이 없어서 확인 필요다.
- runtime.rs 전체(5천 줄 이상)를 놓고 락 순서 위반이 있는지 전수 검토하지 못했다. 3.8의 규칙이 기존 코드와 충돌하는지는 확인 필요다.
