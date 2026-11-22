# tau(oh-my-tau) 종료 신호 정합 설계 — agent_end 와 agent_settled

## 1. 현재 상태 (근거)

tau 가 지금 내보내는 종료 관련 신호는 세 종류이고, 그중 게이트웨이가 기다리는 이름은 하나도 RPC 스트림에 없다.

- `agent_end`: 에이전트 리스너가 받은 `AgentEvent::AgentEnd` 를 `Runtime::on_event` 가 잡아 `on_agent_end` 로 넘긴다 (crates/omp-cli/src/runtime.rs:614-663). 프레임 본문은 crates/omp-cli/src/events.rs:719-739 에서 만들어지고, 같은 파일의 프레이머가 `isTerminal: true`, `yielded: true` 를 무조건 찍는다 (crates/omp-cli/src/events.rs:704-710). 예외는 todo 리마인더가 인계한 경우로, 그때는 `isTerminal=false` 로 덮어쓴다 (crates/omp-cli/src/runtime.rs:1073-1140, 특히 1140).
- `prompt_result` / `session_settled`: `settle_terminal` (crates/omp-cli/src/runtime.rs:1007-1071) 이 `report_prompts` 를 부르고, 그 안의 스폰된 태스크가 `wait_for_idle` 뒤 2ms 쉬고 `report_prompts_now` 를 실행한다 (crates/omp-cli/src/runtime.rs:1578-1612). `report_prompts_now` 는 티켓마다 `prompt_result` 를 보내고, `p.active && drained && p.open.is_empty()` 일 때만 `{"type":"session_settled"}` 를 한 번 보낸 뒤 `settled.notify_waiters()` 한다 (crates/omp-cli/src/runtime.rs:1612-1645).
- `agent_settled`: 확장 호스트로만 나간다. `forward_event` 의 `AgentEvent::AgentEnd` 갈래에서 `host.has_handlers("agent_settled")` 를 확인하고 `emit_nowait(&json!({"type":"agent_settled"}))` 를 호출한다 (crates/omp-cli/src/ext.rs:1589-1601). 이건 `LifecycleEvent` 열거형의 변형이 아니라 날 JSON 이고 (crates/omp-ext/src/lifecycle.rs:87-111 에 `AgentSettled` 변형 없음), RPC 채널(`Runtime::send` → stdout writer)과는 다른 파이프다. 게다가 `ExtMode::Print | ExtMode::Json` 에서는 `agent_end` 자체가 보류되므로 (crates/omp-cli/src/ext.rs:1405-1412) 이 경로의 `agent_settled` 도 그 모드에선 나가지 않는다.
- RPC 디스패치에 `agent_settled` 를 내보내는 명령이나 프레임 생성 지점은 없다. `dispatch` 는 crates/omp-cli/src/rpc.rs:593 에서 시작해 crates/omp-cli/src/rpc.rs:1094 의 `Unknown command` 로 끝나고, 이벤트 프레임은 `Framer`(events.rs)와 `Runtime::send`(crates/omp-cli/src/runtime.rs:518-533) 두 곳에서만 만들어진다.
- 종료 대기: stdin 이 닫히면 `agent.wait_for_idle()` 다음 `rt.wait_settled()` 를 기다린다 (crates/omp-cli/src/rpc.rs:1514-1519, 같은 취지 주석이 crates/omp-cli/src/runtime.rs:1578-1580). `wait_settled` 는 `prompts.open` 이 비고 `!p.active` 이고 재시도 사가가 닫혔는지만 본다 (crates/omp-cli/src/runtime.rs:3701-3718). 즉 tau 의 "정지" 판정 기준은 티켓 장부이지 하위 작업이 아니다.
- 상태 조회: `get_state` 는 crates/omp-cli/src/rpc.rs:766 에서 `Runtime::state` 를 부르고, 그 응답은 `hasPendingAsyncWork: false` 를 하드코딩하며 `isSettled` 를 `!is_streaming && queued == 0 && !retrying` 로 계산한다 (crates/omp-cli/src/runtime.rs:1953-1998, 특히 1970-1971). 백그라운드 잡 레지스트리(crates/omp-cli/src/tools/jobs.rs:125 `JobRegistry`)는 이 계산에 들어가지 않는다.
- 하위 작업의 실체: `task` 도구는 crates/omp-cli/src/tools/task.rs:407 의 단일 async 함수이고, 자식들은 `TaskRunner::spawn_all` 의 `JoinSet` 안에서 회수된다 (crates/omp-task/src/runner.rs:306-340). 도구 호출이 반환되기 전에 자식이 끝나므로 tau 의 `agent_end` 는 이미 "자식까지 끝난 턴 종료"에 가깝다. 반면 셸 백그라운드 잡과 서비스는 도구 반환 뒤에도 살아 있다 (crates/omp-cli/src/tools/wait.rs:1-11 이 그 잡들을 기다리는 도구다).
- 중단 경로: `abort` 는 `user_interrupt` 를 세우고 `agent.abort()` 뒤 `wait_for_idle()` 한다 (crates/omp-cli/src/runtime.rs:1844-1851). 중단된 턴도 `settle_terminal` 을 지나므로 `agent_end` 와 `session_settled` 는 나가지만 `agent_settled` 는 나가지 않는다.

## 2. 게이트웨이가 요구하는 계약

게이트웨이(omo-nyang) 쪽 근거로 확인되는 계약은 다음과 같다.

- 완료 판정은 `agent_settled` 하나다. `brain.ts` 의 이벤트 루프는 `agent_end` 를 abort 출처 해석에만 쓰고 (`abortSource` 파싱), `agent_settled` 에서 `onSettled(gatewayRequestId)` 를 부른다 (src/brain.ts:322-337). 회귀 테스트 이름도 "only agent_settled signals completion, not agent_end" 다 (tests/brain.test.ts:203).
- 레인 유휴 판정도 `agent_settled` 만 본다. 릴레이 활동 캐시에서 본 마지막 이벤트가 `agent_settled` 가 아니면 그 레인은 유휴 후보가 아니다 (src/gateway.ts:2320-2334).
- 요청 식별자가 필요하다. 릴레이는 명령의 requestId 를 모든 이벤트에 `gatewayRequestId` 로 붙여 넘기지만 (src/relay/runtime.ts:235), 게이트웨이 주석은 mhc 워커가 `agent_settled` 에 요청 id 를 찍지 않아 23/23 플랜이 10분 복구 틱까지 대기했다고 기록한다 (src/gateway.ts:1177-1181). 즉 계약은 "이름이 `agent_settled` 인 프레임" + "가능하면 요청 id 동봉"이다.
- mhc 기준 의미: mhc 는 `agent_end` 와 별개로 `AgentSettled` 세션 이벤트를 따로 emit 하고 (crates/maho-core/src/agent_session.rs:18231-18299), RPC 로는 `{"type":"agent_settled"}` 로 직렬화한다 (crates/maho-rpc/src/session_binding.rs:92). 그 emit 은 `wait_for_idle` 후 실행 세대(`settlement_epoch`, `agent_run_active`)를 확인하고, `settling_with_background_work` 플래그로 배경 작업 유무를 표시한 뒤 (crates/maho-core/src/agent_session.rs:18245-18249) `agent_settled` 를 보내고, 이후 배경 작업이 비면 `agent_idle` 을 보낸다 (crates/maho-core/src/agent_session.rs:18290-18296). 대기 유틸도 `agent_settled` 와 `agent_idle` 을 모두 Settled 로 분류한다 (crates/maho-rpc/src/host_session_wait.rs:38-43).
- mhc 에는 `session_settled` / `prompt_result` 프레임이 없다 (crates 전체 grep 무결과). 따라서 tau 의 `session_settled` 는 게이트웨이 계약의 일부가 아니고, 게이트웨이는 이 프레임을 모르는 채 흘려보낸다.
- 정리: 의미 차이는 "턴 종료(agent_end) vs 실행 정지(agent_settled) vs 완전 유휴(agent_idle)"의 3단이다. tau 는 1단만 RPC 로 내보내고 2단을 확장 전용으로, 3단을 아예 갖고 있지 않다.

## 3. 설계

목표는 RPC 스트림에 `agent_settled` 를 정식 프레임으로 넣고, 중복 0회와 순서 고정을 코드로 보장하는 것이다.

### 3.1 단일 정지 지점

`Runtime` 에 정지 지점을 하나 만든다. 이름은 `emit_agent_settled` 이고 `report_prompts_now` 안에서 `session_settled` 를 보낸 직후, `settled.notify_waiters()` 직전에 호출한다 (현재 crates/omp-cli/src/runtime.rs:1641-1643 사이).

```
// runtime.rs: report_prompts_now 끝부분
if settle {
    self.send(json!({"type":"session_settled"}));
    self.emit_agent_settled(settle_meta);   // 신규
}
self.settled.notify_waiters();
```

`emit_agent_settled` 는 다음 필드를 담는다.

- `type: "agent_settled"`
- `epoch`: `settlement_epoch: AtomicU64` 를 fetch_add 한 값. mhc 의 `settlement_epoch`(crates/maho-core/src/agent_session.rs:18239) 과 같은 역할이다.
- `requestId`: 방금 끝난 실행의 `Ticket.id` (crates/omp-cli/src/runtime.rs:71-76). 게이트웨이의 10분 대기 문제를 없애는 필드다.
- `hasPendingAsyncWork`: `JobRegistry` 스냅숏 기준 boolean.
- `reason`: `"settled"` 또는 `"aborted"`(`user_interrupt` 가 선 경우), `"session_closed"` 는 shutdown 경로용.

### 3.2 중복 금지

- `report_prompts_now` 의 `settle` 계산이 이미 "한 번만 참" 을 보장한다. `p.active` 를 false 로 내려놓기 때문이다 (crates/omp-cli/src/runtime.rs:1620-1626). 여기에 `settlement_epoch` 검사를 더해, 정지 이후 도착한 늦은 태스크가 같은 epoch 로 다시 보내지 못하게 한다.
- 비종결 `agent_end`(todo 리마인더 인계, crates/omp-cli/src/runtime.rs:1140)에서는 정지 지점을 지나지 않으므로 `agent_settled` 가 나가지 않는다. 이게 mhc 와의 핵심 차이다. tau 의 확장 경로는 지금 `AgentEnd` 마다 무조건 `agent_settled` 를 쏘므로 (crates/omp-cli/src/ext.rs:1593) 연속 턴에서 중복이 난다. 이 갈래는 제거하고 같은 정지 지점으로 옮긴다.
- `abort` 로 끝난 턴도 `settle_terminal` 을 지나므로 1회 나간다. abort 응답 프레임이 먼저 나가고 그 뒤 정지가 나가는 순서는 기존 `report_prompts` 의 2ms 지연 설계(같은 파일 1583-1595 주석)가 이미 보장한다.

### 3.3 순서 보장

정지 지점은 `Runtime::send` 한 곳으로만 나간다. `send` 는 단일 `mpsc::UnboundedSender` 로 밀어넣고 (crates/omp-cli/src/runtime.rs:518-533), stdout writer 는 그 채널을 FIFO 로 소비한다 (crates/omp-cli/src/rpc.rs:1396 부근 `writer` 태스크). 따라서 한 실행의 프레임 순서는 다음으로 고정된다.

```
agent_start → ... → turn_end → message_end* → agent_end(isTerminal:true)
            → prompt_result* → session_settled → agent_settled
```

`session_settled` 를 `agent_settled` 앞에 두는 이유는 게이트웨이가 `session_settled` 를 모르므로 무해하고, tau 자체 클라이언트(tests-golden/capture.ts:36 등 `agent_end` 대기, crates/omp-cli/src/runtime.rs:5270 `until_settled` 의 `session_settled` 대기)의 기존 계약을 깨지 않기 때문이다. 게이트웨이 기준으로는 `agent_settled` 가 그 실행의 마지막 프레임이 된다.

`FrameEncoder` 는 `agent_end` 만 특별 취급하고 (crates/omp-cli/src/rpc.rs:1183-1218 overflow, 1340 streamed 초기화) 나머지 타입은 크기 초과 시 `rpc_frame_error` 로 대체한다. `agent_settled` 는 수백 바이트라 이 경로를 타지 않지만, 회귀 방지로 `overflow_frame` 의 `agent_end` 갈래 옆에 `agent_settled` 축약 갈래를 추가한다(필드 3개만 유지).

### 3.4 3단 신호로 확장

- `agent_settled` = 실행 정지(현재 `session_settled` 지점).
- `agent_idle` = 하위 작업까지 정지. `JobRegistry` 의 실행 중 잡/서비스가 0 이 되는 시점에 한 번 보낸다. mhc 가 `agent_settled` 뒤 `agent_idle` 을 보내는 구조(crates/maho-core/src/agent_session.rs:18290-18296)와 같다.
- `get_state.hasPendingAsyncWork` 를 하드코딩 false 에서 `JobRegistry` 실값으로 바꾼다 (crates/omp-cli/src/runtime.rs:1970). 지금 값은 게이트웨이의 유휴 판정에 거짓말을 하고 있다.

### 3.5 확장 호스트 정합

`omp-ext` 에 `LifecycleEvent::AgentSettled` 변형을 추가하고 (crates/omp-ext/src/lifecycle.rs:87-111), `ext.rs` 의 `emit_nowait(json!({"type":"agent_settled"}))` 를 `notify(host, "agent_settled", ...)` 로 바꾼다 (crates/omp-cli/src/ext.rs:1511-1520 의 `notify` 는 구독자 없으면 직렬화 비용 0). 호출 위치는 `forward_event` 의 `AgentEnd` 갈래(1593)가 아니라 3.1 의 정지 지점에서 확장 호스트로 전달되는 훅이다. `ExtMode::Print | ExtMode::Json` 의 `agent_end` 보류(crates/omp-cli/src/ext.rs:1405-1412)와 `shutdown` 의 지연 플러시(crates/omp-cli/src/ext.rs:1210-1218)도 같은 정지 지점을 참조하도록 맞춘다.

## 4. 구현 단계 (커밋 단위)

1. **C1 — 정지 카운터와 프레임**: `Runtime` 에 `settlement_epoch: AtomicU64` 필드 추가, `emit_agent_settled(&self, request_id: Option<Value>, reason: &str)` 신설, `report_prompts_now` 에서 `session_settled` 직후 호출. 파일: crates/omp-cli/src/runtime.rs (필드 130-160 부근, 1612-1645).
2. **C2 — 요청 id 배선**: `Ticket.id` 를 정지 지점까지 전달. `report_prompts_now` 의 `claimed` 벡터에서 마지막 티켓 id 를 뽑아 `requestId` 로 넣는다. 파일: crates/omp-cli/src/runtime.rs:1578-1645.
3. **C3 — 확장 경로 중복 제거**: `AgentEnd` 갈래의 즉시 `agent_settled` 제거(crates/omp-cli/src/ext.rs:1593-1599), `LifecycleEvent::AgentSettled` 추가(crates/omp-ext/src/lifecycle.rs:87-111, 476 부근 매크로), 정지 지점에서 확장 호스트로 전달. 파일: crates/omp-cli/src/ext.rs, crates/omp-ext/src/lifecycle.rs.
4. **C4 — 백그라운드 유무**: `get_state.hasPendingAsyncWork` 실값화(crates/omp-cli/src/runtime.rs:1970), `agent_idle` 1회 발신, `JobRegistry`(crates/omp-cli/src/tools/jobs.rs:125) 조회 헬퍼 추가.
5. **C5 — 인코더 방어와 테스트**: `overflow_frame` 에 `agent_settled` 축약 갈래(crates/omp-cli/src/rpc.rs:1183-1218), 단위 테스트 4종과 e2e 시나리오.

## 5. 테스트

- `runtime.rs` 단위: `until_settled` 헬퍼(crates/omp-cli/src/runtime.rs:5270)를 확장해 프레임 시퀀스가 `prompt_result → session_settled → agent_settled` 인지, `agent_settled` 가 정확히 1개인지 검사. 기존 `until_settled` 는 `session_settled` 에서 멈추므로 새 헬퍼 `until_agent_settled` 를 따로 둔다.
- 비종결 턴: todo 리마인더가 인계한 턴(crates/omp-cli/src/runtime.rs:1073-1140)에서 `agent_settled` 가 0개인지. `isTerminal=false` 프레임만 나가야 한다.
- abort: `abort` 후 `agent_settled` 가 1개, `reason:"aborted"` 인지 (crates/omp-cli/src/runtime.rs:1844-1851 경로).
- 연속 프롬프트: `prompt` 두 번 연속에서 `agent_settled` 가 2개, epoch 가 증가하는지.
- `FrameEncoder` 단위: `agent_settled` 를 `MAX_FRAME_BYTES` 근처까지 부풀려도 `rpc_frame_error` 로 바뀌지 않고 축약 프레임으로 나가는지 (crates/omp-cli/src/rpc.rs:1591-1605 의 기존 `terminal_agent_end_drops_messages_already_streamed` 와 같은 형태).
- 확장 경로: `crates/omp-ext/tests/lifecycle.rs` 에 `agent_settled` 직렬화/역직렬화 왕복 1건 추가.
- e2e: `tests-e2e/ext/probe-all-events.ts:9` 의 이벤트 목록에 `agent_settled` 를 넣어 실제 프로세스에서 수신 확인.

## 6. QA 시나리오

1. 단일 턴: `omt --mode rpc` 에 `prompt` 1회. stdout 라인을 시간순으로 저장해 `agent_end` < `session_settled` < `agent_settled` 순서와 각 1회를 확인.
2. 도구 사용 턴: 셸 도구가 있는 턴에서 `tool_execution_end` 뒤에 `agent_end` 가 오고 그 뒤 정지가 오는지.
3. abort: 턴 진행 중 `{"type":"abort"}` 를 보내고 `response(abort)` 가 `agent_settled` 보다 먼저 오는지 (crates/omp-cli/src/runtime.rs:1583-1595 의 2ms 지연 계약).
4. 백그라운드 잡: 백그라운드 셸을 띄운 턴에서 `agent_settled.hasPendingAsyncWork == true`, 잡 종료 후 `agent_idle` 1회.
5. 게이트웨이 통합: omo-nyang 릴레이로 tau 를 물려 `agent_settled` 수신 시 `onSettled` 가 불리고 10분 복구 틱을 기다리지 않는지 (src/gateway.ts:1177-1181 의 진단 로그 `handoff_settle_seen` 에 `matched:true`).
6. 스트림 종료: stdin 을 닫았을 때 `wait_settled` 가 정지 프레임을 모두 플러시한 뒤 종료하는지 (crates/omp-cli/src/rpc.rs:1514-1519).

## 7. 위험과 미확인

- **확인 필요**: 게이트웨이가 `agent_settled` 의 `requestId` 필드를 읽는지. 릴레이는 자기가 `gatewayRequestId` 를 주입하므로(src/relay/runtime.ts:235) tau 가 보낸 `requestId` 를 쓰는 코드는 찾지 못했다. 필드명을 `requestId` 로 할지 `gatewayRequestId` 로 맞출지는 게이트웨이 쪽 수정 여부에 달려 있다.
- **확인 필요**: `PromptState.active` 가 true 로 세워지는 정확한 줄. `report_prompts_now` 가 참으로 읽고 false 로 내리는 것만 확인했고(crates/omp-cli/src/runtime.rs:1620-1626), 승격 지점은 프롬프트 수락 경로 안이라 줄 번호를 특정하지 못했다.
- **확인 필요**: `agent_idle` 을 tau 가 새로 내보낼 때 게이트웨이가 이를 어떻게 처리하는지. 게이트웨이 소스에서 `agent_idle` 문자열 사용처를 찾지 못했다(무결과). mhc 는 보내지만 게이트웨이는 `agent_settled` 만 본다.
- 위험: `agent_settled` 를 `session_settled` 뒤에 두면, 게이트웨이가 `agent_settled` 만 완료로 보는 동안 tau 클라이언트는 `session_settled` 로 먼저 깨어난다. 두 신호를 동시에 구독하는 클라이언트가 생기면 중복 완료 처리가 가능하므로, 문서화된 계약은 "tau 네이티브는 `session_settled`, 게이트웨이는 `agent_settled`" 로 못박아야 한다.
- 위험: `agent_end` 프레임은 기본적으로 메시지 본문을 싣지 않고 카운트만 싣는다(crates/omp-cli/src/events.rs:719-739, `OMP_RPC_FULL_AGENT_END` 게이트). 게이트웨이가 `agent_end` 로 abort 출처를 해석하므로(brain.ts:322-326) `abortSource` 필드가 이 요약 프레임에 없다. 정지 프레임에 `reason` 을 넣는 것으로 우회하지만, 게이트웨이의 abort 해석 경로는 여전히 빈손이다.
- 위험: 하위 작업 의미 정합. `task` 는 동기 회수라(crates/omp-task/src/runner.rs:306-340) 자식이 끝나기 전에 `agent_end` 가 나갈 수 없지만, `bash` 백그라운드와 `wait` 도구가 보는 잡(crates/omp-cli/src/tools/wait.rs:1-11)은 그렇지 않다. `hasPendingAsyncWork` 를 실값으로 바꾸면 게이트웨이 유휴 판정이 지금보다 늦어질 수 있다.
- 미확인: 이 조각은 종료 신호만 다룬다. 같은 슬랙 답변(ts 1795326973.474349)의 나머지 범주 — RPC 명령 8종 부재(crates/omp-cli/src/rpc.rs:593-1094), CLI 플래그 4종 부재(crates/omp-cli/src/cli.rs), 확장 호스트 `api.rpc.handle`·`session_move_export/import` 부재(crates/omp-ext/src/host.rs), 도구 부재(task_send/task_output/task_cancel, bash_output/kill_bash/monitor, memory/memory_apply_patch, workpool), 설정 루트 `~/.omp` 대 `~/.omo`, 세션 파일 형식 차이(crates/omp-session 의 omp JSONL v3 대 senpi 형식) — 는 다른 조각의 몫이며 여기서는 정지 신호와 맞닿는 부분(`get_state` 필드, `workpool`/백그라운드 유무, 다중 세션 시 정지 프레임의 세션 구분자)만 3.4 와 7 에 적었다. 다중 세션에서 `agent_settled` 에 `sessionId` 를 넣을지는 `--multi-session` 플래그 설계가 확정된 뒤 정해야 하므로 확인 필요로 남긴다.
