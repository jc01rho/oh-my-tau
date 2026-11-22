# tau RPC `resume_session` 명령 추가 설계

## 1. 현재 상태 (근거)

- 저장소: `jc01rho/oh-my-tau`, HEAD `e7a5378`, workspace version `0.12.0` (`Cargo.toml:38`). 이 머신 `~/git/oh-my-tau`에 클론해 읽었다. 수정·커밋 없음.
- RPC 진입: `crates/omp-cli/src/rpc.rs:562` `handle()` → `rpc.rs:593` `dispatch(rt, cmd, kind)` 하나의 `match kind`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 따라서 `resume_session`은 현재 이 arm으로 떨어진다.
- 세션 3종은 이미 있다: `rpc.rs:707` `new_session` → `Runtime::new_session(None)` (`runtime.rs:2215`), `rpc.rs:714` `switch_session` → `Runtime::load_session(&path, None)` (`runtime.rs:2255`), `rpc.rs:730` `open_session` → `newest_session(&dir)`(`runtime.rs:4408`) 후 `load_session` 또는 `new_session`.
- 재개 본체: `runtime.rs:2279` `load_session_as(path, dir, fork, branched)`. 순서는 `check_switch("resume", Some(path))` (`runtime.rs:2287`, 정의 `2570`) → `cancel_compaction_and_wait` / `cancel_retry_and_wait` (`2296-2297`) → `SessionIndex::load` (`2325`) → `agent.abort(); wait_for_idle(); reset()` (`2392-2394`) → `persist.bind_existing(path, header_id, tip)` (`2422`, `persist.rs:436`) → `bump_generation()` (`2427`) → `swap_todo_tool(TodoTool::with_session(phases, …))` (`2441`) → `set_messages(messages)` (`2446`) → `restore_session_model` / `restore_session_thinking` (`2452-2457`) → `herdr_session("resume")` (`1373`, `2459`) → `prompts.open.clear()` (`2460`) → `restore_title_from(&index)` (`2461`, 정의 `2638`) → `notify_session_switch("resume", previous)` (`2462`, 정의 `2615`).
- 세션 저장: `persist.rs:89` `Persistence`. `session_id()` `121`, `path()` `127`, `file()` `133`, `append_entry(kind, payload)` `142`, `append_message` `235`, `append_raw_message` `254`, `start_new` `407`, `bind_existing` `436`, `discard_pristine` `458`.
- 저널 형식: `crates/omp-session/src/index.rs:90` `SessionIndex { refs, malformed, title_slot, invalid_header, size }`, `index.rs:26` `EntryKind`(Header/Title/Message/Compaction/ResetBoundary/BranchSummary/CustomMessage/Custom/Label/ThinkingLevelChange/ModelChange/…), `index.rs:118` `load`, `161` `branch()`, `253` `scan_lines`. 쓰기는 `lib.rs:257` `SessionWriter::open(path, leaf)`, `lib.rs:598` `append_entry` (type/id/parentId/timestamp를 자동 부여, `STRICT_KINDS` 9종 외는 `FileEntry` 경로 — `entries.rs:522-532`). 확장 상태는 `entries.rs:242` `Custom { custom_type, data }`로 이미 왕복한다.
- 저널 재수화 선례: `runtime.rs:2195` `rehydrate_todo_tool(&[])`, `runtime.rs:2808` `latest_todo_phases` — 브랜치를 역순 스캔해 `"customType":"user_todo_edit"`/`"toolName":"todo"` 최신 1건으로 상태를 복원한다. `resume_session`의 보류 장부도 같은 방식이 맞다.
- 종료 신호: tau는 `runtime.rs:1641`에서 `session_settled`를 보낸다. `agent_settled`는 `ext.rs:1593-1597`에서 확장이 `has_handlers("agent_settled")`일 때만 emit되는 별도 프레임이다.
- 확장 호스트: `crates/omp-ext/src/host.rs:544` `emit`, `575` `has_handlers`, `604` `emit_nowait`. `api.rpc.handle`에 해당하는 확장→호스트 RPC 질의와 `session_move_export/import`는 grep에서 나오지 않았다(부재로 판단, 전수 확인은 아님).
- 도구 상태의 수명: 백그라운드 잡 레지스트리는 세션 단위 `JobRegistry`(`tools/jobs.rs:123-127`, `Arc<Mutex<JobsInner>>`)이고 `tools.rs:52-88`의 `SESSION_JOBS` 전역으로 노출된다. `bash.rs:1040` `start_background`가 띄운 프로세스는 툴 목록 재구성에도 살아남는다(`runtime.rs:3435-3459`가 같은 레지스트리를 재사용).
- 중단 경로: `runtime.rs:1844` `abort()`는 `user_interrupt` 설정 + `abort_retry` + `abort_compaction` + `agent.abort()` + `wait_for_idle()`만 한다. 큐를 비우는 것은 `vendor/pa-agent/src/agent.rs:1211` `reset()`이며, `abort()`는 큐를 건드리지 않는다(`agent.rs:1180`).
- 결정적 공백: tau에는 보류(pause/hold) 개념 자체가 없다. `rpc.rs`·`runtime.rs`에서 `pause`/`hold`는 goal의 `pause`/`resume` 메시지(`rpc.rs:254,272`)와 `/mcp reload`(`slash.rs:2629`)뿐이다. `get_state`(`runtime.rs:1953`)에도 `paused` 필드가 없고 `"hasPendingAsyncWork": false`(`1970`)로 고정이다.

## 2. 게이트웨이가 요구하는 계약

게이트웨이 쪽 근거는 `omo-nyang`(TypeScript)과 그 엔진 `mhc`(`/data/home/Work/mhc`, HEAD `f0f3571c5`, tag `v2026.11.15-omomeow1`)다.

- 와이어 형태: `{"type":"resume_session"}`, 인자 없음이 기본(`crates/maho-rpc/src/rpc_types.rs:648-649` `ResumeSession`). 옵션 인자는 `expectedSeq`(0 이상 정수), `mode`(`"continue"|"release"`), `message`(문자열), `effectId`(1~256자)만 허용(`crates/maho-rpc/src/pause_command_validation.rs:21-36`).
- 의미: "보류된 wake를 푼다". `maho-core/src/agent_session.rs:6584` `resume_session()`은 `engine_turn_release.resume(...)`로 장부를 release하고, `dispatch_hold_changed("resumed")`, `deliver_held_inputs()`를 호출한다.
- 응답은 오류가 아니라 상태다(`agent_session.rs:6501`, `6557`): `{resumed:false, pauseSeq, turnStarted:false, deliveredNotices:0, deliveredHeldInputs:0}` + `reason: "not_paused"|"stale"|"resume_through_parent"`, 또는 `{resumed:true, pauseSeq, turnStarted, deliveredNotices, deliveredHeldInputs}`. `expectedSeq` 불일치는 `reason:"stale"`(`6507-6513`).
- 멱등: `effectId`가 있으면 `customType:"session-resume-effect"` 엔트리에 답을 저장하고 재요청 시 `replayed:true`로 재생한다(`6489-6500`).
- 장부 형식: `custom` 엔트리 `session-hold`(`{op:"add"|"release", kind, key, pauseSeq, at}`), `session-notice`, `session-notices-consumed`(`maho-core/src/engine_turn_hold.rs:20-24`, `400`). `HoldKind`는 `Unattended|SessionHold|RunFence|InheritedHold`(`engine_turn_hold.rs:24-29`), release cause 이름이 `"resume_session"`(`engine_turn_hold.rs:54-64`).
- 능력 광고: `custom_capability.rs:28` `RESUME_SESSION_CAPABILITY = "resume_session"`. `get_state`가 `paused`/`runPaused`를 보고한다(`connection_handler.rs:656-657`).
- 게이트웨이 실제 호출 순서(`omo-nyang/src/lane-hold.ts:29-77`): ① `set_client_info {capabilities:["interactive_user"]}` ② `interrupt {intent:"stop"}` ③ `get_state`의 `paused`가 true여야 통과 ④ `get_protocol_info.serverVersion`으로 `engineResumesRun` 판정(`49-56`) ⑤ 미달이면 `reload` ⑥ `resume_session` ⑦ `get_state`를 500ms 간격 최대 10회 폴링해 `paused`(그리고 최신 엔진이면 `runPaused`)가 풀렸는지 확인. 실패 시 `LaneHoldError`.
- 따라서 tau가 답해야 할 것은 명령 하나가 아니라 이 7단계다. `resume_session` 단독 구현으로는 게이트웨이가 ③에서 먼저 실패한다.

## 3. 설계

### 3.1 보류 장부 (신규 `crates/omp-cli/src/hold.rs`)

```
pub struct Hold { pub kind: HoldKind, pub key: String, pub since: String,
                  pub data: Option<Value>, pub source_counts: BTreeMap<String, usize> }
pub enum HoldKind { Unattended, SessionHold, RunFence, InheritedHold }
pub struct HoldLedger { records: BTreeMap<String, Hold>, pause_seq: u64, released: BTreeSet<String> }
```
- 저장은 `Persistence::append_entry("custom", json!({"customType":"session-hold","data":{...}}))`(`persist.rs:142`)로만 한다. `omp-session` 변경은 필요 없다 — `Custom`(`entries.rs:242`)이 이미 왕복하고 `append_entry`가 `custom`을 `FileEntry`로 처리한다(`lib.rs:598-623`).
- 복원은 `latest_todo_phases`(`runtime.rs:2808-2843`)와 같은 형태로 `SessionIndex::scan_lines`(`index.rs:253`)를 브랜치 역순으로 훑어 `op:"add"`/`op:"release"`를 접는다. `load_session_as` 마지막(`runtime.rs:2460` 부근)에서 `prompts.open.clear()` 옆에 `hold.reset_from(&index, path, &branch)`를 둔다.
- `RunFence`/`InheritedHold`는 tau의 `omp-task`(`crates/omp-task`)가 mhc의 run fence와 다르므로 이번 범위에서 **수용만 하고 강제하지 않는다**. `get_state.runPaused`는 항상 `false`로 보고한다(§7 위험).

### 3.2 재개 시퀀스

`Runtime::resume_held(&self, cmd: &Value) -> Result<Value>` (`runtime.rs` 신규, `rpc.rs`에서 호출):

1. `let mut led = lock(&self.hold)`; `if !led.held()` → `ok(json!({"resumed":false,"pauseSeq":led.pause_seq,"turnStarted":false,"deliveredNotices":0,"deliveredHeldInputs":0,"reason":"not_paused"}))`. 오류로 만들지 않는다(게이트웨이 `lane-hold.ts:66-71`이 오류를 reload 트리거로 쓴다).
2. `expectedSeq` 검사 → 불일치면 `reason:"stale"`.
3. `effectId` 재생: `customType:"session-resume-effect"` 최신 엔트리의 `data.answer`를 `replayed:true`로 반환.
4. single-flight: `AtomicBool running` (mhc `engine_turn_release.rs:45-55`와 동일 규약). 이미 진행 중이면 대기하지 않고 `resumed:false, reason:"in_flight"`.
5. `self.agent.wait_for_idle().await` — 보류 중인 실행이 남아 있으면 여기서 정리된다. 절대 `new_session`/`start_new`/`bind_existing`을 부르지 않는다(파일을 바꾸지 않는 것이 이 명령의 정의다).
6. held item 전달: notice를 먼저, 이어 held input. 전달은 `agent.prompt`/`steer`(`runtime.rs:1853`)로 하고, 실패하면 **장부를 release하지 않고** `Ok(false)`로 남긴다(mhc `agent_session.rs:6612-6624`의 "the hold then keeps custody"와 같은 규약).
7. 전달이 모두 끝난 뒤에야 `session-hold {op:"release", cause:"resume_session", pauseSeq}` 엔트리를 append한다. 이 순서가 "실패해도 세션을 잃지 않는다"의 핵심이다: 도중에 프로세스가 죽으면 저널에는 `add`만 남아 재기동 후 다시 보류 상태로 복원되고, 이중 전달은 effectId로 막힌다.
8. `emit_queue_update()`(`runtime.rs:578`), `emit_event({"type":"session_resumed","pauseSeq":…,"cause":"resume_session"})`, 확장에는 `LifecycleEvent::SessionResumed`(신규)를 `notify()`(`ext.rs:1515`)로 보낸다.
9. 답: `{resumed:true, pauseSeq, turnStarted, deliveredNotices, deliveredHeldInputs}`.

### 3.3 도구·큐·확장 상태 복원

- 대화 컨텍스트·todo: 같은 프로세스가 세션을 유지하므로 `set_messages`/`swap_todo_tool`을 다시 하지 않는다. 게이트웨이가 `reload` 폴백으로 프로세스를 재기동한 경우는 이미 `load_session_as`가 `2441`/`2446`에서 복원한다.
- 대기 큐: `queued()`(`runtime.rs:556`)의 `(steering, followUp)` 프리뷰를 `op:"add"` 엔트리의 `data.queue`에 스냅샷한다. 스냅샷 시점은 `abort()`(`runtime.rs:1844`) 직전 — `agent.reset()`이 큐를 비우므로(`agent.rs:1211-1223`) 그 전에 떠야 한다. 재개 시 `steer`/`follow_up`으로 되돌리고 `last_queue`(`runtime.rs:112`)를 갱신해 `queue_update`가 중복 emit되지 않게 한다.
- 백그라운드 잡: `JobRegistry`는 `Arc`라 abort에도 살아 있다(`jobs.rs:123-127`, `tools.rs:52-88`). 다만 모델에게 다시 보이게 하는 경로가 없다(`jobs.rs:291` `background_notice`는 시작 시 1회). 재개 시 실행 중 잡 id를 `queue_update` 뒤에 시스템 리마인더 1건으로 재주입한다.
- 확장: `push_session_ctx`(`ext.rs:1162`)로 ctx(id/file/model/name)를 재전송하고 `session_resumed`를 새 lifecycle 이벤트로 추가한다(`omp-ext/src/lifecycle.rs:87` enum에 현재 없음). `session_switch`는 파일이 바뀌지 않으므로 보내지 않는다(`ext.rs:1077`는 스위치 전용).

### 3.4 함께 필요한 최소 조각 (이 조각의 경계)

- `rpc.rs:677` `"abort"` arm에 `"interrupt"` 별칭을 추가하고 `intent`를 검증한다(`"stop"|"abort"`, `pause_command_validation.rs:5-11`). `intent:"stop"`이면 §3.2 5단계 전에 큐 스냅샷 + `SessionHold` add + `abort()`.
- `runtime.rs:1953` `state()`에 `"paused": led.held()`, `"runPaused": false`를 추가하고 `isSettled`(`1971`)에 `&& !paused`를 넣는다.
- `get_protocol_info`(`serverVersion` 포함)와 `reload`는 이 조각 밖이지만, 없으면 게이트웨이는 tau를 구엔진으로 보고 reload 폴백을 탄다(`lane-hold.ts:59-71`).

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-cli): 세션 보류 장부 hold.rs` — `HoldLedger`, `Hold`, `HoldKind`, `replay_from`, `append_add`/`append_release`, `pause_seq` 단조 증가. `Runtime`에 `hold: Mutex<HoldLedger>` 필드(`runtime.rs:116` struct) 추가, `load_session_as`(`2460`)·`new_session`(`2234` 부근)에서 리셋.
2. `feat(omp-cli): abort 직전 큐 스냅샷과 SessionHold 등록` — `abort()`(`1844`) 앞단에 `hold.snapshot_queues(queued())`, `rpc.rs:677` `"abort"|"interrupt"` 분기와 `intent` 검증.
3. `feat(omp-cli): resume_session RPC` — `rpc.rs:745` 뒤에 arm 추가, `Runtime::resume_held` 구현(§3.2 1~9), `effectId` 재생, `reason` 3종.
4. `feat(omp-cli): get_state paused/runPaused` — `state()`(`1953`) 필드 추가, `isSettled` 보정.
5. `feat(omp-ext): session_resumed 라이프사이클 이벤트` — `LifecycleEvent::SessionResumed { pause_seq, cause, turn_started }`(`lifecycle.rs:87`), `ext.rs`에서 `notify(host, "session_resumed", …)`.
6. `docs(rpc): resume_session 계약` — `rpc.rs` 모듈 doc(1-24행)의 커맨드 목록에 항목 추가.

## 5. 테스트

- `hold.rs` 단위: `add` 2건 + `release` 1건 저널 → `replay_from`이 남은 1건과 최신 `pause_seq`를 낸다. `release` 없는 저널은 `held()==true`.
- `resume_held` 단위(`runtime.rs:5210` `harness(tools, config, dir)` 재사용): 보류 없는 런타임에 `resume_session` → `resumed:false, reason:"not_paused"`; `expectedSeq` 불일치 → `reason:"stale"`.
- 멱등: 같은 `effectId`로 2회 호출 → 두 번째가 `replayed:true`, 저널에 `session-resume-effect` 1건, 전달 1회.
- 실패 보존: 세션 파일을 읽기 전용으로 만든 상태에서 `resume_session` → 오류 응답이고 `get_state.paused`가 여전히 `true`(장부 미해제).
- 게이트웨이 호환 시퀀스: `interrupt{intent:"stop"}` → `get_state.paused==true` → `resume_session` → `get_state.paused==false` 를 한 테스트에서 연속 검증(`lane-hold.ts:29-77`의 축소 재현).
- 큐 복원: `steer("a")` + `follow_up("b")` 후 stop → resume → `get_state.queuedMessages`가 `{steering:["a"],followUp:["b"]}`로 돌아온다. 타이머 대기 없이 `queue_update` 프레임을 구독해 확인한다.

## 6. QA 시나리오

1. 정상 보류·재개: 실행 중 `interrupt{intent:"stop"}`, `get_state.paused` true 확인, `resume_session`, `paused` false와 `resumed:true` 확인.
2. 보류 없이 재개: `resumed:false, reason:"not_paused"`, 오류 아님, 세션 파일 무변경(mtime 동일).
3. 낡은 seq: `expectedSeq`를 `pauseSeq-1`로 보내 `reason:"stale"`, 장부 유지.
4. 응답 유실 후 재시도: 같은 `effectId`로 재호출해 `replayed:true`, 모델에 중복 프롬프트가 가지 않음.
5. 재개 도중 SIGKILL: 재기동 후 `get_state.paused`가 true(장부가 add만 남음), 다시 `resume_session`으로 정상 재개.
6. 백그라운드 잡이 있는 세션: `bash {async:true}` 실행 중 stop → resume 후 잡이 여전히 running이고 리마인더 1건이 온다.
7. 확장이 붙은 세션: `session_resumed` 프레임 수신, ctx의 `sessionFile`/`sessionName`이 유지됨(`ext.rs:1162`).

## 7. 위험과 미확인

- **최대 위험**: tau에는 보류 의미론이 없다(§1 마지막 항목). 이 설계는 mhc의 `session-hold` 장부를 새로 들여오는 것이며, `RunFence`/task 자식 보류는 tau `omp-task`가 mhc와 달라 흉내만 낸다. 게이트웨이의 `runPaused` 확인(`lane-hold.ts:74-75`)은 tau에서 항상 false로 통과시키는 것이고, 이는 "자식이 실제로 멈췄다"는 보증이 아니다.
- `get_protocol_info`와 `reload`가 없으면 게이트웨이는 매 재개마다 reload 폴백을 시도하고 실패한다(`lane-hold.ts:59-71`). 이 조각만으로 게이트웨이 경로가 완성되지 않는다.
- `agent_settled`(`ext.rs:1593`)는 확장이 핸들러를 등록했을 때만 나가고, tau의 정착 신호는 `session_settled`(`runtime.rs:1641`)다. 재개가 turn을 시작한 경우 게이트웨이가 어느 프레임을 기다리는지는 확인 필요.
- 미확인: 스트리밍 중 `append_entry`로 `custom`을 쓰는 것이 안전한지(`SessionWriter`는 `append` 모드 `lib.rs:257-281`이나 동시 쓰기 규약은 읽지 않았다). 큐 프리뷰를 `data`에 넣는 형식이 `Custom.data`(`entries.rs:242-246`)의 `Option<Value>`로 무손실 왕복하는지는 테스트로만 확인 가능하다.
- 미확인: `crates/omp-cli/src/subcmd/daemon.rs`의 다중 세션 호스팅에서 `resume_session`이 어느 세션을 대상으로 하는지(라우팅 봉투가 tau에 없음). 이 조각은 단일 세션 프로세스 기준이다.
- 미확인: `omp-ext`에 확장→호스트 RPC 질의(`api.rpc.handle`)가 정말 없는지는 grep 결과에 근거한 판단이며 전수 감사는 하지 않았다.
- 이번 세션에서 컴파일·테스트는 실행하지 않았다. 위 테스트 목록은 설계이며 통과 증거가 아니다. `crates/omp-task` 내부, `omp-compact`의 `build_session_context_owned` 경로, `tests-e2e/ext/conformance.py`의 이벤트 목록은 읽지 못했다.
