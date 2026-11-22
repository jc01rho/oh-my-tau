# RPC `interrupt`·`set_client_info` 명령 추가 설계 (tau 0.12.0)

## 1. 현재 상태 (근거)

- RPC 디스패치는 단일 match 다. `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`, 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` 로 떨어진다. `interrupt`, `set_client_info` 는 이 arm 에 걸려 거절된다.
- 프레임 조립은 `rpc.rs:203-226 response()` 가 전담한다. 성공은 `{id?, type:"response", command, success:true, data?}`, 실패는 `error`(문자열)와 선택적 `code`(`CmdError.code: Option<&'static str>`, `runtime.rs:34-54`).
- 기존 abort 경로는 `rpc.rs:677-679` `"abort" => { rt.abort().await; none() }` 하나뿐이고, `rt.abort()` 는 `runtime.rs:1844-1851` 에서 `user_interrupt.store(true)`, `abort_retry()`, `abort_compaction()`, `agent.abort()`(`vendor/pa-agent/src/agent.rs:1180-1186`), `agent.wait_for_idle()`(`agent.rs:1191`) 순으로 돌린다. 즉 "턴 중단 후 idle 까지 대기"는 이미 구현돼 있다.
- `abort_and_prompt` 는 `rpc.rs:681-707` 에서 `rt.abort().await` 뒤 별도 태스크로 `rt.prompt(...)` 를 띄운다.
- 읽기 루프는 명령을 워커 태스크에 순서대로 넘기고(`rpc.rs:1436-1445`), `busy` 플래그가 켜져 있을 때만 `abort` 를 워커를 건너뛰어 즉시 처리한다(`rpc.rs:1502-1508`). 다이얼로그 대기 중 교착을 피하려는 장치다.
- `get_state` 는 `rpc.rs:766` `ok(rt.state().await)` 이고, `runtime.rs:1953-2009` 의 json 에는 `isStreaming`, `isCompacting`, `queuedMessageCount`, `isSettled`, `goal` 등만 있고 `paused`/`runPaused`/`pauseSeq` 가 없다.
- 런타임 상태 그릇은 두 개다. `runtime.rs:93-114 struct Settings`(플래그·큐·`running_bash: Option<AbortController>`), `runtime.rs:79-90 struct PromptState`. `Runtime`(`runtime.rs:116-197`)은 프로세스당 1개이고 `user_interrupt: AtomicBool`(`runtime.rs:155`)이 "사용자가 방금 멈췄다"의 유일한 표식이다. 이 플래그는 `runtime.rs:1701`(새 prompt 진입 시 해제), `runtime.rs:1187-1196`(`drain_stranded_queue` 억제), `runtime.rs:1049-1062`(todo 리마인더 억제), `runtime.rs:3091`(압축 후 자동 재개 억제)에서 읽힌다.
- 셸 도구 중단은 별도 컨트롤러다. `rpc.rs:537-539` 가 `SettingsView::running_bash`(`runtime.rs:3802-3803`)에 `AbortController`(`vendor/pa-agent/src/abort.rs:97-125`)를 꽂고, `rpc.rs:994-997` `abort_bash` 가 `abort_bash()`(`runtime.rs:3805-3808`)로 취소한다.
- 세션 영속은 `crates/omp-cli/src/persist.rs:142` `append_entry(kind: &str, payload: Value)` 로 임의 종류를 남길 수 있고, 프리앰블 판정 허용 목록에 `"custom"` 이 포함된다(`persist.rs:474`).
- `agent_settled` 는 확장 이벤트로만 나간다(`crates/omp-cli/src/ext.rs:1593-1598`, `host.has_handlers("agent_settled")` 조건). RPC 프레임으로는 `session_settled`(`runtime.rs:1641`)만 있다.
- 게이트웨이 쪽 계약은 `omo-nyang` 의 `src/lane-hold.ts:29-34` 다: `set_client_info`(`width:80`, `capabilities:["interactive_user"]`) → `interrupt`(`intent:"stop"`) → `get_state` 로 `paused` 확인, `paused` 가 아니면 `LaneHoldError`. `holdState` 는 `paused`/`runPaused` 만 파싱한다(`lane-hold.ts:22-23`). 순서 고정은 `tests/lane-hold.test.ts:44-50` 이 `["set_client_info","interrupt:stop","get_state"]` 로 못박는다.
- 실 엔진(mhc 2026.11.20-omomeow1) stdio 실측:
  - `interrupt` 무인자 → `success:true, data:{"interrupted":false}`, 보류 없음.
  - `interrupt intent:"bogus"` → `error:"intent must be \"stop\" or \"abort\"."`.
  - `interactive_user` 미선언 상태의 `intent:"stop"` → `error:"stop_requires_interactive_client: declare interactive_user with set_client_info first"`.
  - `set_client_info` 에 `width` 누락 → `error:"missing field \`width\`"`.
  - `capabilities` 키를 아예 안 보낸 뒤 `interactive_user` 선언 → 성공. `capabilities:[]` 를 한 번 보낸 뒤 `interactive_user` 선언 → `error:"connection_role_fixed: interactive_user and task_parent cannot change after they are declared"`. 즉 역할 고정은 `capabilities` 키가 존재할 때 발동한다.
  - `intent:"stop"` 성공 시 `session_paused` 프레임(`pauseSeq:1, kind:"session_hold", persisted:true`)과 `custom`/`customType:"session-hold"` 엔트리가 함께 나가고, 이어지는 `get_state` 는 `paused:true, runPaused:false, pause:{pauseSeq,records}, pauseSeq, pausedHeldInputCount, holds` 를 준다.
  - 보류 중 `prompt` 를 보내면 `customType:"session-hold", op:"release", cause:"input"` 엔트리와 `session_resumed`(`cause:"input"`)가 나가고 보류가 풀린다.
  - `intent:"stop"` 을 두 번 보내면 둘 다 `success:true` 이고 `pauseSeq` 는 1로 유지된다(멱등).

## 2. 게이트웨이가 요구하는 계약

- `set_client_info` 요청: `{type:"set_client_info", width:80, capabilities:["interactive_user"]}`. 응답은 `success:true` 면 충분하고 `data` 는 읽지 않는다(`lane-hold.ts:29`).
- `interrupt` 요청: `{type:"interrupt", intent:"stop"}`. 응답 `success` 가 false 면 `holdSession` 이 그대로 실패한다(`lane-hold.ts:30`). 게이트웨이의 `worker-control` 은 `intent` 없이 `{type:"interrupt"}` 도 보낸다(`omo-nyang/src/worker-control.ts:89`) — 무인자는 abort 로 해석돼야 한다.
- `get_state` 응답: 최소 `paused`, `runPaused` 불리언(`lane-hold.ts:22-23`). `resume_session`/`reload` 조각이 `pauseSeq` 를 읽으므로 `pauseSeq` 도 함께 내보내는 것이 안전하다(`lane-hold.ts:64-79` 가 `get_state` 를 반복 폴링).
- 순서 계약: 역할 선언 → Stop → 상태 확인. tau 는 이 순서를 강제해야 하고, 선언 없이 온 Stop 은 거절해야 한다(게이트웨이 테스트가 거절 케이스를 별도로 둔다, `tests/lane-hold.test.ts` 의 `FAKE_REFUSE_STOP` 경로).

## 3. 설계

### 3.1 보류 상태 기계의 위치

- 새 모듈 `crates/omp-cli/src/pause.rs` 를 만들고 `main.rs:3-29` 의 `mod` 목록에 `mod pause;` 를 추가한다. 순수 로직(의도 파싱, 역할 고정 검증, seq 전이)을 여기 모아 단위 테스트가 `Runtime` 없이 돌게 한다.
- 타입:
  - `pub enum InterruptIntent { Abort, Stop }` + `pub fn parse_intent(cmd: &Value) -> Result<InterruptIntent, CmdError>`
  - `pub struct ClientInfo { width: u16, capabilities: Vec<String>, client_name: Option<String>, session_id: Option<String>, protocol_version: Option<i64> }`
  - `pub struct HoldRecord { kind: String, key: String, since: String }`
  - `pub struct PauseState { seq: u64, records: Vec<HoldRecord>, run_paused: bool }` — 기본값 seq 0, 빈 records.
- `Runtime`(`runtime.rs:116-197`)에 필드 두 개를 더한다: `client: Mutex<Option<ClientInfo>>`, `pause: Mutex<PauseState>`. 세션 파일이 아니라 런타임 메모리에 두는 이유는 `rpc.rs:1374-1382` 에서 프로세스당 `Runtime::start` 가 한 번만 호출되고, 세션 전환도 같은 `Runtime` 위에서 일어나기 때문이다. 세션 재개 시 복원은 `resume_session`/`reload` 조각의 몫으로 남긴다.
- 영속은 `persist.append_entry("custom", json!({"customType":"session-hold","op":"add", ...}))` 로 남긴다(`persist.rs:142`). mhc 의 관측 형태(`customType:"session-hold"`, `op:"add"|"release"`, `pauseSeq`, `at`)를 그대로 따른다.

### 3.2 `interrupt`

`rpc.rs:677` 의 `"abort"` arm 바로 뒤에 붙인다.

```
"interrupt" => {
    let intent = pause::parse_intent(cmd)?;          // None|"abort" -> Abort, "stop" -> Stop
    match intent {
        InterruptIntent::Abort => {
            let was_streaming = rt.agent.is_streaming().await;   // agent.rs:885
            rt.abort().await;                                     // runtime.rs:1844
            ok(json!({"interrupted": was_streaming}))
        }
        InterruptIntent::Stop => {
            rt.pause_for_stop().await?;                           // 신규
            ok(json!({"interrupted": false}))
        }
    }
}
```

- `Runtime::pause_for_stop`(`runtime.rs` 확장):
  1. `pause::require_interactive(&self.client_info())` — `interactive_user` 가 없으면 `CmdError::new("stop_requires_interactive_client: declare interactive_user with set_client_info first")`. 실측 문구를 그대로 쓴다.
  2. `self.abort().await` 재사용 — `user_interrupt` 세팅, retry/compaction 취소, `agent.abort()`, `wait_for_idle()` 이 한 번에 끝난다. 이 재사용이 이 조각의 핵심이다. 별도 중단 경로를 새로 만들지 않는다.
  3. `self.set(|s| s.abort_bash())`(`runtime.rs:3805`)로 진행 중 셸도 함께 멈춘다. `abort()` 는 에이전트만 멈추고 bash 컨트롤러는 건드리지 않는다.
  4. `pause` 락을 잡고 `seq += 1`, `HoldRecord{kind:"session_hold", key:format!("session_hold:{seq}"), since}` 추가. 이미 `paused` 면 seq 를 올리지 않고 성공 반환(실측 멱등).
  5. `append_entry("custom", ...)` 후 `self.send(json!({"type":"session_paused","pauseSeq":seq,"kind":"session_hold","at":..,"noticeCount":0,"persisted":true}))`. `send()`(`runtime.rs:518`)를 쓴다 — `emit_event()`(`runtime.rs:589-598`)는 `set_event_filter` 로 걸러질 수 있어 게이트웨이가 프레임을 놓친다.
- 읽기 루프 fast-path: `rpc.rs:1502` 의 조건을 `matches!(kind, "abort" | "interrupt")` 로 넓힌다. Stop 은 `wait_for_idle()` 을 await 하므로 워커 뒤에 큐잉되면 슬래시 다이얼로그가 열려 있을 때 게이트웨이가 응답 없이 매달린다.

### 3.3 `set_client_info`

`rpc.rs` 의 `"set_ask_dialog"` 근처(설정 계열 arm)에 붙인다.

```
"set_client_info" => {
    let width = cmd.get("width").and_then(Value::as_u64)
        .ok_or_else(|| CmdError::new("missing field `width`"))?;   // 실측 문구
    let caps = pause::parse_capabilities(cmd)?;                    // None = 키 없음
    rt.set_client_info(ClientInfo { width: width as u16, capabilities: caps.unwrap_or_default(),
        client_name: cmd.get("clientName").and_then(Value::as_str).map(str::to_string),
        session_id: cmd.get("sessionId").and_then(Value::as_str).map(str::to_string),
        protocol_version: cmd.get("protocolVersion").and_then(Value::as_i64) })?;
    none()
}
```

- `parse_capabilities`: 키가 없으면 `Ok(None)`, 배열이면 `Ok(Some(vec))`, 그 밖의 타입이면 `Err`. 알 수 없는 문자열은 그대로 통과시킨다(실측: `"bogus"` 허용).
- 역할 고정은 `pause::set_client_info(state, next)` 안에서 판정한다. `state.role` 은 `Option<HashSet<String>>` 이고 `capabilities` 키가 처음 존재할 때 확정된다. 이미 확정된 뒤 다른 집합이 오면 `CmdError::new("connection_role_fixed: interactive_user and task_parent cannot change after they are declared")`. 같은 집합 재선언은 성공(실측 E). `capabilities` 키가 없는 호출은 역할을 건드리지 않는다(실측 D 대 E 의 차이).
- 장비명·세션 id·프로토콜 버전은 tau 동작에 쓰지 않고 보관만 한다. 게이트웨이는 이 필드들을 요청에 넣지 않으므로(`lane-hold.ts:29` 는 `width`/`capabilities` 만) 관용적으로 받아 두는 것이 맞다. 실측에서도 알 수 없는 필드가 붙어도 파싱 오류가 나지 않았다.

### 3.4 `get_state` 확장과 입력에 의한 자동 해제

- `runtime.rs:1961-1983` 의 json 에 `paused`, `runPaused`, `pause:{pauseSeq, records}`, `pauseSeq`, `pausedHeldInputCount:0`, `pausedNoticeCount:0`, `holds` 를 추가한다. 값은 `self.pause_view()` 한 번으로 만든다.
- `Runtime::prompt`(`runtime.rs:1649`) 진입부, `user_interrupt.store(false)`(`runtime.rs:1701`) 바로 앞에서 `self.release_pause("input")` 를 호출한다. 보류 중 입력은 보류를 풀고 턴을 시작한다(실측 `session_resumed cause:"input"`). 이걸 빼면 게이트웨이가 Stop 후 큐에 넣은 입력이 영원히 보류에 갇힌다.
- `release_pause(cause)` 는 `seq += 1`, records 비우기, `append_entry` 로 `op:"release"`, `send({"type":"session_resumed","pauseSeq":..,"cause":cause,"at":..})`. `resume_session` 조각도 같은 함수를 `cause:"resume_session"` 으로 호출하게 된다.

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-cli): pause state module` — `crates/omp-cli/src/pause.rs` 신규(`InterruptIntent`, `ClientInfo`, `HoldRecord`, `PauseState`, `parse_intent`, `parse_capabilities`, `require_interactive`, `set_client_info`), `main.rs` 에 `mod pause;`, `Runtime` 에 `client`/`pause` 필드와 `runtime.rs:4538` 근처 단위 테스트. RPC 배선 없음.
2. `feat(omp-cli): expose pause state in get_state` — `runtime.rs:1961-1983` 확장 + `pause_view()`. 이 커밋만으로 `get_state` 가 `paused:false, runPaused:false, pauseSeq:0` 을 내기 시작한다.
3. `feat(omp-cli): interrupt command` — `rpc.rs:677` 뒤 arm, `Runtime::pause_for_stop`(`abort()` 재사용 + `abort_bash()` + seq 전이 + `session_paused` 프레임 + `session-hold` 엔트리), `rpc.rs:1502` fast-path 확장.
4. `feat(omp-cli): set_client_info command` — `rpc.rs` 설정 계열 arm + 역할 고정 규칙 + `client_info()` 접근자.
5. `feat(omp-cli): release pause on new input` — `prompt()` 진입부 `release_pause("input")` + `session_resumed` 프레임. `resume_session` 조각이 이 함수를 재사용하도록 `pub(crate)` 로 노출.
6. `test(omp-cli): stop protocol parity tests` — 5절의 통합 테스트.

## 5. 테스트

- `pause.rs` 단위 테스트(순수 로직, `Runtime` 불필요):
  - `parse_intent`: 키 없음→Abort, `"abort"`→Abort, `"stop"`→Stop, `"bogus"`→`Err` 메시지 `intent must be "stop" or "abort".`
  - `set_client_info`: `capabilities` 미포함 후 `interactive_user` → 성공, `capabilities:[]` 후 `interactive_user` → `connection_role_fixed`, 같은 집합 재선언 → 성공, `width` 누락은 `parse` 단계에서 `missing field \`width\``.
  - `PauseState`: stop 두 번 → seq 1 유지, release 후 seq 2, release 는 보류가 없으면 `None`.
- `crates/omp-cli/tests/rpc_stop_protocol.rs` 신규 통합 테스트. `tests/print_slash.rs:1-40` 패턴을 그대로 쓴다: `tempfile::tempdir()`, `~/.omp/agent/models.yml` 에 mock provider, `env!("CARGO_BIN_EXE_omt")` 를 `--mode rpc` 로 띄우고 stdin 에 프레임을 쓰고 stdout 을 줄 단위로 파싱한다.
  - 시나리오 A(게이트웨이 순서 그대로): `set_client_info(width 80, capabilities ["interactive_user"])` → `interrupt(intent "stop")` → `get_state`. 마지막 응답의 `paused == true` 를 단정한다. `omo-nyang/tests/lane-hold.test.ts:44-50` 과 같은 순서·같은 단정이다.
  - 시나리오 B(역할 미선언): `interrupt(intent "stop")` 만 → `success:false`, `error` 문자열이 실측 문구와 완전 일치.
  - 시나리오 C(무인자): `interrupt` → `success:true`, `data.interrupted == false`, `get_state.paused == false`.
  - 시나리오 D(보류 중 입력): Stop 후 `prompt` → `session_resumed` 프레임이 `prompt` 응답보다 먼저 도착하고 `get_state.paused == false`.
  - 시나리오 E(프레임 필터): `set_event_filter(["agent_end"])` 후 Stop → `session_paused` 가 여전히 도착. `send()` 를 쓴다는 3.2 의 선택을 고정한다.
  - 모든 대기는 고정 sleep 이 아니라 "해당 프레임이 올 때까지 stdout 을 읽고, 상한 시간 초과 시 실패"로 쓴다.
- `cargo test -p omp-cli` 와 `cargo clippy -p omp-cli -- -D warnings` 를 통과 조건으로 둔다.

## 6. QA 시나리오

1. 게이트웨이 `holdSession` 을 tau 에 그대로 돌린다: `bun test tests/lane-hold.test.ts` 의 fake 세션 자리에 `omt --mode rpc` 를 넣어 `paused` 확인까지 통과하는지 본다.
2. 실제 턴 중 Stop: mock provider 로 긴 응답을 흘리는 중 `interrupt(intent "stop")` 을 보내고, `session_paused` 가 `agent_end`/`session_settled` 뒤에 오는지, 응답 시점에 `get_state.isStreaming == false` 인지 확인한다. 순서가 뒤집히면 `wait_for_idle` 누락이다.
3. 진행 중 `bash` 도구 + Stop: `bash` 명령이 `cancelled:true` 로 끝나고 보류가 걸리는지 확인한다(`abort_bash` 경로).
4. Stop 후 `prompt`: 보류가 `cause:"input"` 으로 풀리고 턴이 시작되는지, `user_interrupt` 가 새 턴을 막지 않는지(`runtime.rs:1701` 순서) 확인한다.
5. Stop 두 번, `resume_session` 두 번: seq 가 1→2 로만 오르고 두 번째 resume 이 오류를 내지 않는지. mhc 실측에서 두 번째 `resume_session` 응답은 수집하지 못했으므로 이 항목은 tau 자체 규칙으로 정한다(멱등 성공).
6. 세션 파일 검사: Stop 후 JSONL 에 `custom`/`session-hold` 엔트리가 남고, `discard_if_preamble_only`(`persist.rs:461-482`)가 이 파일을 프리앰블 전용으로 오판해 지우지 않는지 확인한다.
7. 역할 고정: 게이트웨이가 한 연결에서 `interactive_user` 를 선언한 뒤 `task_parent` 를 선언하면 두 번째가 거절되는지. 같은 목록 재선언은 통과하는지.

## 7. 위험과 미확인

- `interrupt` 응답의 `interrupted` 값 규칙: mhc 는 idle 상태에서 항상 `false` 를 줬다. 진행 중 턴에서 `true` 가 되는지는 실측하지 못했다. tau 설계는 `agent.is_streaming()` 스냅샷으로 정했으나 게이트웨이가 이 값을 읽지 않으므로(`lane-hold.ts:30` 은 성공 여부만 본다) 위험은 낮다. **확인 필요**.
- `runPaused`: mhc 의 `runPaused` 는 워크플로 런이 걸린 경우다. tau 에는 DAG 런 개념이 없어 항상 `false` 로 낸다. 게이트웨이의 `releaseSession` 은 `runPaused && engineResumesRun(...)` 분기를 타므로(`lane-hold.ts:59-70`) tau 는 그 분기를 영원히 안 탄다. 의도된 단순화지만 `resume_session` 조각과 합의가 필요하다.
- `get_protocol_info` 의 `serverVersion`: `releaseSession` 이 이 값으로 reload 필요 여부를 가른다(`lane-hold.ts:55-58`). tau 는 아직 이 명령이 없으므로(1절) 게이트웨이는 tau 를 "버전 미보고 엔진"으로 취급해 reload 를 건다. 이 조각 범위 밖이나, 두 조각이 같은 PR 계열에 있어야 동작한다.
- `pauseSeq` 의 영속 복원: 세션 파일을 다시 열었을 때 보류를 복원할지 여부를 정하지 않았다. mhc 는 `persisted:true` 를 내므로 복원하는 쪽이 맞지만, tau 의 세션 로더에 `custom` 엔트리 해석 지점이 있는지 확인하지 못했다. **확인 필요**.
- `abort()` 재사용의 부작용: `runtime.rs:1844-1851` 은 `user_interrupt` 를 세팅하고 `wait_for_idle()` 을 기다린다. Stop 이 이 경로를 타면 `drain_stranded_queue`(`runtime.rs:1187-1196`)가 억제되고 todo 리마인더도 꺼진다. Stop 의 의미와 일치하지만, 보류 중 `prompt` 로 풀 때 `user_interrupt` 해제 순서(3.4)가 틀리면 큐가 조용히 버려진다. 통합 테스트 D 가 이 회귀를 잡는다.
- 읽기 루프 fast-path 확장(`rpc.rs:1502`)은 `busy` 가 켜진 동안에만 유효하다. Stop 이 워커에서 처리 중일 때 두 번째 Stop 이 오면 두 경로가 동시에 `pause` 락을 잡는다. 락 안에서 멱등 처리하도록 설계했으나 동시성 테스트는 넣지 않았다.
- 게이트웨이의 `set_client_info` 재전송: `lane-hold.ts:26-27` 주석은 "역할은 한 번 선언되면 고정이므로 매번 같은 목록을 선언한다"고 적는다. tau 가 이 규칙을 그대로 구현하면 게이트웨이의 짧은 연결(레인마다 새 stdio)에서도 안전하다. 다만 같은 프로세스에 다른 클라이언트가 붙는 다중 연결 시나리오는 tau 에 없으므로(단일 stdio) 검증 대상이 아니다.
- CLI 플래그(`--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`)와 `resume_session`/`release_session`/`reload`/`get_protocol_info`/`extension_request`/`move_session` 은 이 조각 범위가 아니다. 이 문서는 `interrupt` 와 `set_client_info` 두 명령, 그에 필요한 `PauseState`·`get_state` 확장·`prompt` 자동 해제까지만 다룬다.
- 이 문서의 tau 근거는 `git clone --depth 1` 로 받은 `main`(`e7a5378`, workspace version 0.12.0, `Cargo.toml:38`) 기준이다. 게이트웨이 근거는 `~/Work/omo-nyang` 의 현재 체크아웃이고, mhc 근거는 이 장비에 설치된 `2026.11.20-omomeow1` 바이너리를 `--mode rpc` 로 직접 구동해 얻은 stdout 이다.
