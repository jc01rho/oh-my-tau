# tau(oh-my-tau) task_send / task_output / task_cancel 설계

기준 체크아웃: jc01rho/oh-my-tau HEAD e7a5378, version 0.12.0 (Cargo.toml:38). 아래 모든 인용은 이 트리에서 직접 읽은 파일:줄이다.

## 1. 현재 상태 (근거)

- 하위 실행 엔진은 이미 절반이 있다. `crates/omp-task/src/runner.rs:286` `TaskRunner::spawn`은 자식이 끝날 때까지 기다리고, `:306` `spawn_all`은 `JoinSet`으로 배치를 돌린 뒤 입력 순서로 모은다. 반환 타입은 `RunOutput`(runner.rs:53-68) 하나이고 스트리밍 채널은 없다.
- 제어 가능한 변형이 별도로 존재한다. `crates/omp-task/src/runner/control.rs:307` `TaskRunner::start(spec, mode) -> (RunControl, RunJoin)`이 백그라운드 실행의 실체다. `RunControl`(control.rs:179)은 `id/phase/steer/cancel/output_tokens`를 제공하고, `RunJoin`(control.rs:273)은 정산된 `RunOutput` 퓨처다.
- 단계 모델: `RunPhase { Queued, Running, Finishing, Settled }`(control.rs:96-105). 종료 사유는 `RunStatus { Completed, Failed, Cancelled }`(runner.rs:42-49)뿐이라 "왜 죽었는가"(사망·중단·취소)를 구분하지 못한다.
- steering은 이미 구현되어 있다. `RunControl::steer`(control.rs:228)는 `ChildMode::Rpc` 자식의 stdin에 `{"type":"steer","message":...}`(control.rs:641-643)를 쓰고, 실제 프레임이 기록된 뒤에야 ack한다. 실패는 `SteerError::{Unsupported,NotRunning}`(control.rs:129-136) 두 가지다. `Finishing` 진입 후 steer는 `NotRunning`으로 거절된다(control.rs:729-736).
- 취소는 CAS 전이다. `RunControl::cancel`(control.rs:250)은 `killable` 스왑에 성공한 호출만 `true`를 돌려주고, 자식 프로세스 그룹 전체를 SIGKILL한다(control.rs:424-432, 495-500). Queued 상태 취소는 스폰 없이 정산된다(테스트 control.rs:1050-1074). 취소 사유를 전달할 인자는 없다(control.rs:250 시그니처에 reason 없음).
- 자식 수명: `process_group(0)`(control.rs:481-482) + `GroupGuard` 드롭 시 그룹 킬(control.rs:403-421). 즉 부모 런타임이 사라지면 자식도 죽는다. 부모가 죽으면 자식의 종료 상태를 기록할 주체도 함께 사라진다.
- 결과 표면: `TaskResultEnvelope { agent, id, status, duration_ms, preview, agent_url, truncated, error }`(envelope.rs:18-33), 미리보기 5000자 상한( envelope.rs:14, 38-88), `agent://<id>` 포인터. `outputSchema` 검증은 `validate_output`(schema.rs:62).
- 기존 `task` 도구는 완전 블로킹이다. `crates/omp-cli/src/tools/task.rs:407` `task()`가 `:485`에서 `runner.spawn_all(specs).await`로 전부 끝날 때까지 붙잡고 `merge_outputs`(task.rs:541)로 한 덩어리 텍스트를 만든다. 호출마다 `TaskRunner::new`를 새로 만든다(task.rs:483-484) — 즉 동시성 상한이 호출 단위이고 프로세스 전역이 아니다.
- 레지스트리는 eval 쪽에만 있다. `crates/omp-cli/src/tools/eval_agents.rs:258` `AGENT_HANDLES`, `:151` `Handle`, `:121-127` `HandleStatus { Running, Completed, Failed, Cancelled }`, `:360` `deliver_message`, `:421` `steer_handle`. `:503` `LAUNCHER`가 프로세스 전역 `TaskRunner`를 하나만 쓴다. task 도구는 이 레지스트리를 공유하지 않는다.
- 이름 유사 도구는 별개다. `bash_output`/`kill_bash`/`monitor`는 없고 `crates/omp-cli/src/tools/jobs.rs:125` `JobRegistry`(insert :131, get :147, sorted :163)와 `wait` 도구(crates/omp-cli/src/tools/wait.rs, 30분 상한·100ms 폴)가 셸 잡용으로 존재한다. `task_send`/`task_output`/`task_cancel` 문자열은 crates/ 아래에 없다.
- RPC 표면도 비어 있다. `crates/omp-cli/src/rpc.rs:863` `get_subagents`는 항상 `{"subagents": []}`, `:865-868` `cancel_subagent`는 항상 `{"cancelled": false}`, `:869-878` `steer_subagent`는 항상 오류다. 미지원 명령은 `:1094` `Unknown command: {other}`.
- 종료 신호: `agent_settled`는 확장 핸들러가 있을 때만 emit된다(ext.rs:1593-1599). RPC 클라이언트가 보는 턴 경계는 `agent_end`(events.rs:721-737)와 `session_settled`(runtime.rs:1641)다.
- 참조 구현(mhc, /data/home/Work/mhc): `crates/omo/senpi-task/src/state/types.rs:9-58` 8상태(`pending/running/paused/completed/error/cancelled/interrupted/lost`)와 `is_terminal`, `:95-100` `Messageability { Steer, Revive, NotContinuable }`, `state/transitions.rs:40-72` 메시지 가능성 판정, `completion/routing.rs:6-11` `should_notify_status = Completed|Error|Lost`, 도구 등록은 `crates/omo/components/maho-omo-task/src/tools.rs:159,278,304,318`.

## 2. 게이트웨이가 요구하는 계약

- `task_send`: `{ to (필수), message: string | {type:"shutdown_request"|"shutdown_response",...}, team_run_id?, summary?, all_scope? }` (senpi-task/src/tools/control/send_schema.rs, `task_send_params_schema`). 게이트웨이가 실제로 쓰는 부분은 `to` + 평문 `message`이고, 구조화 메시지는 팀 종료 협상용이다.
- `task_output`: `{ task_id?, name?, mode?: "status"|"tail"|"full", tail_lines? }` (senpi-task/src/tools/output/output.rs, `TaskOutputMode`, `task_output_params_schema`). 기본 `status`, `tail` 기본 60줄. `block`/`timeout_ms`는 레거시로 거절해야 한다.
- `task_cancel`: `{ task_id?, name?, reason? }` (senpi-task/src/tools/control/cancel.rs). 취소는 종료이며 재개 불가, 실행 중이 아닌 대상 취소는 무시하고 현 상태를 보고한다(cancel.rs 설명 문자열).
- 상태 어휘: 최소 `pending|running|completed|error|cancelled|lost` 6종이 필요하다. `lost`는 "부모가 사라져 결과를 아무도 확정하지 못한 자식"이고, mhc는 이를 완료 알림 대상에 포함한다(routing.rs:6-11).
- tau가 맞춰야 할 결론: 세 도구는 모델 대면 표면이고, 게이트웨이가 실제로 호출하는 것은 `rpc.rs`의 `get_subagents`/`cancel_subagent`/`steer_subagent`다. 두 표면이 같은 레지스트리를 봐야 한다.

## 3. 설계

### 3.1 레지스트리 (신규 `crates/omp-task/src/registry.rs`)

```rust
pub struct TaskId(String);                 // "st_<monotonic>", 기존 agent://<id>와 별개 축
pub enum TaskState { Pending, Running, Finishing, Completed, Error, Cancelled, Lost }
pub struct TaskRecord {
    pub id: TaskId, pub name: Option<String>, pub agent: String,
    pub state: TaskState, pub phase_hint: RunPhase,
    pub owner_session: String,             // 부모 세션 id
    pub started_at: Instant, pub settled_at: Option<Instant>,
    pub output_tokens: u64, pub error: Option<String>,
    pub cancel_reason: Option<String>,
    pub envelope: Option<TaskResultEnvelope>, // 정산 시 1회 기록
    pub transcript: TranscriptTap,            // 3.4
}
pub struct TaskRegistry { inner: Mutex<RegistryInner>, runner: TaskRunner, next_id: AtomicU64 }
```

- `TaskRegistry::spawn(&self, spec: TaskSpec, mode: ChildMode) -> TaskId`가 `TaskRunner::start`(control.rs:307)를 감싸고, `RunJoin`을 드라이버 태스크에 넘겨 정산 시 `TaskRecord`를 갱신한다.
- **러너는 레지스트리가 하나만 소유한다.** task.rs:483-484의 호출별 `TaskRunner::new`를 없애고 `LazyLock<TaskRegistry>`(eval_agents.rs:503 `LAUNCHER`와 동일 패턴)를 쓴다. 그래야 `task.maxConcurrency`(omp-settings schema.rs:3241)가 프로세스 전역 상한이 된다. eval 서브에이전트와는 세마포어를 공유하지 않고 각자 상한을 갖되, 합산 상한은 별도 설정으로 둔다(확인 필요: 게이트웨이가 두 계열 합산 상한을 요구하는지).
- 지속화: 세션 디렉터리에 `task-registry.jsonl`로 `TaskRecord` 스냅샷을 append한다. 재기동 시 `reconcile()`이 비종료 레코드를 `Lost`로 전이한다(mhc `mark_record_lost_for_reconciliation`, state/transitions.rs:298-334와 동일 규칙: 이미 종료면 무시, `Lost` 재기록은 `KeepExisting`이면 무시).

### 3.2 `task` 도구 확장 (백그라운드 생성)

- 스키마에 `tasks[].background: boolean`(기본 false)과 `context` 유지를 추가한다. true면 `spawn_all` 대신 레지스트리 경유 `start`를 쓰고 즉시 `{task_id, name, state:"running", agent_url}` 목록을 돌려준다. false면 현행 블로킹 경로 그대로다.
- `task.rs:83` `schema()`는 TS `getTaskSchema`와 바이트 일치를 목표로 하고 있다(주석 task.rs:78-84). 필드 추가는 파리티 계약을 깨므로, 별도 도구 `task_async`로 분리할지 여부를 결정해야 한다(확인 필요: 파리티 테스트 이름과 게이트웨이의 스키마 비교 여부).
- 재귀 게이트는 유지한다: `recursion_gate()`(runner.rs:196, 기본 상한 2, runner.rs:161)를 백그라운드 생성에도 동일하게 적용한다.

### 3.3 `task_send` 입력과 steering 의미

```json
{ "type":"object",
  "properties": {
    "to": {"type":"string","description":"Child task id (st_...) or name."},
    "message": {"anyOf":[{"type":"string"},
      {"type":"object","properties":{"type":{"enum":["shutdown_request","shutdown_response"]},
        "reason":{"type":"string"},"approve":{"type":"boolean"},"request_id":{"type":"string"}},
        "required":["type"]}]},
    "summary": {"type":"string"},
    "all_scope": {"type":"boolean","description":"다른 세션이 소유한 자식 허용. 기본 false."}
  },
  "required":["to"] }
```

- 해석 순서: `to`가 `st_`로 시작하면 id, 아니면 `name` 조회. 둘 다 없으면 `{task_id,name}` 규약상 오류 텍스트를 그대로 돌려준다(mhc cancel.rs `MISSING_TARGET_REASON`).
- 전달 판정은 레지스트리가 아니라 실행 상태가 결정한다. `TaskState::Pending | Running`이면 `RunControl::steer`(control.rs:228)로 위임하고 성공 시 `"Delivered to <id>."`를 반환한다. `Finishing`이면 `SteerError::NotRunning`(control.rs:729-736)을 그대로 표면화한다.
- **tau에는 Revive가 없다.** 자식은 프롬프트가 정산되면 종료되는 원샷 서브프로세스이고(eval_agents.rs:360-371 주석의 "cannot be revived"와 동일 근거), `Messageability`는 `Steer | NotContinuable` 두 값으로 축소된다. 종료된 자식에 대한 send는 mhc 문구 규약에 맞춰 `"Failed: <id> is not running. The child settled as <state>; resend to a new task."`로 실패한다. 이 축소는 게이트웨이 계약 위반이 아니라 기능 부재이므로 문서화가 필요하다.
- 구조화 `shutdown_request`: 자식에게 steer 프레임으로 전달하지 않고 `cancel_with_reason` 경로로 매핑한다(자식이 종료 협상을 구현하지 않으므로). `shutdown_response`는 tau에서 무의미하므로 `NotContinuable`로 거절한다.
- `all_scope=false`면 `record.owner_session == caller_session`만 허용한다.

### 3.4 `task_output` 상태 조회 모델

- `mode=status`: `TaskRecord` 스냅샷 + `envelope`(정산 전이면 `preview` 없이 상태/경과/토큰만). 반환 형태는 `{ task_id, name, agent, state, phase, duration_ms, output_tokens, error, agent_url }`.
- `mode=tail`/`full`: `TranscriptTap`에서 읽는다. 현재 tau는 중간 산출물을 보관하지 않는다 — `drive_rpc`는 `message_end`의 assistant 텍스트를 `fallback_text` 한 칸에 덮어쓰고(control.rs:885-908), print 모드는 `read_all`로 종료 시점에 전량을 받는다(control.rs:522-530). 따라서 `TranscriptTap`을 신설해 (a) RPC 모드의 `message_end`/`turn_end` 프레임, (b) print 모드의 stdout 스트림을 링버퍼(기본 5000줄, 초과분은 앞에서 버리고 `truncated=true`)로 적립한다.
- print 모드의 점진 적립은 `read_all`을 라인 스트리머로 바꿔야 성립한다(control.rs:522-543 교체). 이 변경은 `join_text`의 시간 상한(KILL_DRAIN 2s / PIPES_DRAIN 5s, control.rs:55-59) 의미를 보존해야 한다.
- `block`/`timeout_ms`가 오면 mhc와 같이 거절하고 `mode=status` 사용을 안내한다.
- `agent://<id>`는 그대로 둔다. 이미 `crates/omp-urls/src/schemes/agent.rs:1-10`이 아티팩트 디렉터리의 `<id>.md`를 읽고 JSON 경로 추출을 지원한다. `task_output mode=full`은 이 스킴과 역할이 겹치므로, 아티팩트가 있으면 `agent_url`을 함께 돌려주고 본문은 탭에서 읽는다.

### 3.5 `task_cancel` 전이

```text
Pending  --cancel--> Cancelled   (스폰 없음; control.rs:1050 테스트가 이미 보장)
Running  --cancel--> Cancelled   (프로세스 그룹 SIGKILL, control.rs:424-432)
Finishing--cancel--> Cancelled   (cancel()가 이기면 Completed 산출물을 Cancelled로 덮음, control.rs:388-395)
Completed|Error|Cancelled|Lost --cancel--> 무변경, 현 상태 보고 (no-op)
```

- 반환은 `CancelOutcome::{ Cancelled{task_id, previous_status, status}, NoOp{task_id, status}, NotFound }`로 두고, `RunControl::cancel()`의 `bool`(control.rs:250)을 그대로 믿지 않는다. `false`는 "이미 정산됨"이므로 레지스트리 상태를 다시 읽어 `NoOp`으로 보고한다.
- 사유 기록: 현 `cancel()`에는 사유 인자가 없다. `cancel_with_reason(&self, reason: &str) -> bool`을 control.rs에 추가하고, `cancelled_output`(control.rs:545-568)의 stderr 첫 줄을 `run cancelled: <reason>`으로 만든다. 기존 `cancel()`은 `cancel_with_reason("")`로 위임해 호출부 호환을 유지한다.
- 취소 후에도 레코드는 남긴다. mhc 설명대로 `task_output`이 취소 결과를 계속 읽을 수 있어야 한다(cancel.rs 설명 문자열).
- 대기 중인 steer는 취소 시 `drain_controls`(control.rs:514-520)가 `NotRunning`으로 정산하므로, `task_cancel` 응답에 `dropped_steers: n`을 포함한다(mhc `with_dropped_steering_notice` 대응).

### 3.6 부모 완료 주장 차단 규칙

핵심 규칙 4개.

1. **살아 있는 자식이 있는 동안 `agent_settled`를 내지 않는다.** 현재 emit 지점은 ext.rs:1593-1599이고 조건은 `host.has_handlers("agent_settled")` 하나다. 여기에 `registry.live_children(owner_session).is_empty()`를 AND로 넣는다. 자식이 살아 있으면 부모 턴은 `agent_end`(events.rs:721)까지만 가고 settle 신호는 보류된다.
2. **자식 종료는 부모에게 주입된다.** 자식이 종료 상태(`Completed|Error|Cancelled|Lost`)에 도달하면 드라이버가 부모 세션에 완료 메시지를 넣는다. 부모가 유휴면 깨우고, 스트리밍 중이면 다음 턴 경계에 steer로 넣는다(mhc `route_completion`, completion/routing.rs:15-25의 Wake/DeliverStreaming/Buffer 3분기 축소판; tau에는 compact/session-switch 버퍼가 없으므로 Buffer는 생략하고 확인 필요로 남긴다). 주입 문구는 `<task-result>` 엔벨로프 JSON을 그대로 쓴다(envelope.rs:38).
3. **`Lost` 자식은 성공으로 상쇄되지 않는다.** 부모의 정산 시점에 소유 자식 중 `Lost`가 하나라도 있으면, 부모 런의 종료 상태를 `Error`로 강등하고 `error`에 `"child <id> lost: no settlement was recorded"`를 넣는다. 자식이 `Cancelled`인 경우는 부모가 스스로 취소한 것이면 성공을 막지 않는다 — `cancel_reason`이 부모 세션에서 발행됐는지로 판별한다.
4. **차단은 유한하다.** 1번 때문에 settle이 보류된 상태에서 자식이 영원히 안 끝나면 교착이 된다. 따라서 `max_task_wait`(기본 30분, `wait` 도구의 `WAIT_MAX`와 동일 스케일)을 두고, 초과 시 살아 있는 자식을 `Interrupted` 상당으로 정산(그룹 킬)한 뒤 `Lost`가 아닌 `Cancelled`로 기록하고 부모 settle을 허용한다.

이 규칙이 없으면 게이트웨이는 자식이 죽은 뒤에도 부모의 `agent_settled`를 정상 완료로 읽는다. 그 경우가 바로 이 조각에서 막아야 하는 실패 모드다.

## 4. 구현 단계 (커밋 단위)

1. `omp-task`: `registry.rs` 신설 — `TaskId`, `TaskState`, `TaskRecord`, `TaskRegistry::spawn/status/cancel/reconcile`, `lib.rs` 재노출(lib.rs:23-35 블록에 추가). 테스트는 상태 전이표 단위 테스트만.
2. `omp-task`: `control.rs`에 `cancel_with_reason`과 `TranscriptTap` 적립 훅(`message_end`, `turn_end`), print 모드 스트리밍 리더 교체. 기존 `cancel()` 위임 유지.
3. `omp-cli`: `tools/task.rs`에 `background` 경로 — `TaskRegistry` 전역 인스턴스 사용, `spawn_all`(task.rs:485) 분기. `merge_outputs`(task.rs:541)는 블로킹 경로 전용으로 남긴다.
4. `omp-cli`: `tools/task_send.rs`, `tools/task_output.rs`, `tools/task_cancel.rs` 신설 + `tools.rs` 등록 목록(tools.rs:383 인근)에 3개 추가 + `enabled_by_settings`(tools.rs:420 부근) 게이트 추가.
5. `omp-cli`: `rpc.rs` `get_subagents`(863)/`cancel_subagent`(865)/`steer_subagent`(869)를 레지스트리 조회로 교체. `task_status` 구독 이벤트(자식 상태 전이 push) 추가.
6. `omp-cli`: `ext.rs:1593` settle 게이트 + `runtime.rs:1641` 정산 경로에 규칙 3(자식 Lost 강등) 연결.
7. 문서/파리티: `docs/parity` 아래에 task 제어 표면 차이(Revive 없음, 8상태 중 7상태) 기록, 스키마 파리티 테스트 갱신.

## 5. 테스트

- 단위(`crates/omp-task`): `reconcile`가 비종료 레코드를 `Lost`로 만들고 종료 레코드는 건드리지 않음. `cancel` 전이표 4행. `TranscriptTap` 링버퍼 상한과 `truncated` 표시. `cancel_with_reason`이 `cancelled_output` stderr 첫 줄에 사유를 넣음.
- 통합(`crates/omp-cli`): 기존 `fake_binary` 패턴(runner.rs:509-520, control.rs:970-979)을 재사용해 (a) 백그라운드 생성 후 `task_output mode=status`가 `running`을 보고, (b) `task_send`가 RPC 자식에 steer를 넣고 ack, (c) `task_cancel`이 그룹 킬 후 `cancelled`를 보고하고 `task_output`이 여전히 읽힘, (d) Queued 취소가 스폰 0회.
- 게이트 테스트: 살아 있는 자식이 있는 부모 런에서 `agent_settled` 프레임이 나오지 않고, 자식 종료 주입 후에만 나온다. 자식이 `Lost`면 부모 프레임의 `status`가 `error`다. 고정 sleep 대신 자식 종료 Notify와 프레임 수신 채널을 먼저 구독한 뒤 타임아웃으로 기다린다.
- RPC 테스트: `rpc.rs` 테스트 모듈에 `get_subagents`가 실제 레코드를 반환하고, `steer_subagent`가 종료된 자식에 `NotRunning` 계열 오류를 반환하는 케이스 추가.
- 회귀: `cargo test -p omp-task -p omp-cli` 1회 통과. `task` 블로킹 경로의 기존 테스트(task.rs 모듈)가 그대로 통과해야 한다.

## 6. QA 시나리오

1. 백그라운드 생성: `omt --mode rpc`에 `{"type":"prompt","message":"task 도구로 background:true 3건 생성"}` → `get_subagents`가 3건, `pending/running` 혼재.
2. steer 전달: 1번 자식에 `steer_subagent` → 자식 `turn_end` 뒤 최종 텍스트에 지시가 반영됨. 종료된 자식에 재전송하면 `not running` 오류.
3. 취소: Queued 자식 `cancel_subagent` → 스폰 로그 0줄, 상태 `cancelled`. Running 자식 취소 → 5초 내 정산, 손자 프로세스 생존 마커 없음(control.rs:1024-1047 테스트와 동일 검증).
4. 사망 감지: 자식 프로세스를 외부에서 SIGKILL → 레지스트리 재기동 `reconcile` 후 `lost`, 부모 `agent_settled` 프레임이 `status=error`로 나옴.
5. 출력 조회: `task_output mode=tail tail_lines=20`이 마지막 20줄, `full`이 전체, 5000줄 초과 시 `truncated=true`.
6. 부모 완료 차단: 자식 2건 중 1건이 장기 실행일 때 부모 턴 종료 후 settle 프레임 부재 확인, 자식 종료 후 1회만 settle.

## 7. 위험과 미확인

- **스키마 파리티 충돌**: task.rs:83 `schema()`는 TS 스키마 바이트 일치를 목표로 한다. `background` 필드 추가는 이 계약을 깬다. 파리티 테스트의 실제 이름과 게이트웨이가 스키마를 비교하는지는 확인 필요.
- **전역 러너 전환의 파급**: task.rs:483-484의 호출별 러너를 전역으로 바꾸면 동시성 상한 의미가 달라진다(호출 단위 → 프로세스 단위). 기존 사용자 체감 동작 변경이므로 설정 기본값 결정이 필요하다.
- **print 모드 스트리밍**: `read_all`(control.rs:522-530) 교체는 파이프 드레인 시간 상한과 그룹 킬 경로에 영향을 준다. 교체 없이는 `task_output tail`이 RPC 모드에서만 정확하다.
- **Revive 부재**: 종료된 자식 재개는 이 설계에 없다. 게이트웨이가 keep-alive 자식을 전제하면 계약 미충족이며, 그 경우 `task` 도구의 자식 수명 모델 자체를 바꿔야 한다(확인 필요: 게이트웨이가 revive를 실제로 호출하는지).
- **settle 게이트 위치**: ext.rs:1593은 확장 핸들러가 있을 때만 도는 경로다. 핸들러가 없는 RPC 세션에서는 `agent_settled`가 아예 나오지 않으므로, 게이트를 `runtime.rs`의 `session_settled` 발행 지점(runtime.rs:1641)에 함께 걸어야 하는지 확인 필요. 해당 함수명은 읽지 못했다.
- **`Lost` 판정 권한**: 부모가 죽으면 자식도 그룹 킬된다(control.rs:403-421). 따라서 `Lost`는 "자식이 살아 있는데 결과를 못 받은" 경우가 아니라 "부모가 재기동했을 때 비종료로 남은" 경우에만 발생한다. 이 전제가 게이트웨이의 senpi 세션 재개 모델과 맞는지 확인 필요.
- **이번 실행에서 하지 못한 것**: 컴파일·테스트를 한 번도 돌리지 않았다(설계 문서만 요구되어 코드를 작성하지 않음). `docs/parity`의 기존 파리티 테스트 파일, `runtime.rs` 정산 함수 본문, 게이트웨이 쪽 task 도구 소비 코드(게이트웨이 저장소 미확인)는 읽지 못했다. mhc 참조는 /data/home/Work/mhc 체크아웃 기준이며 버전은 확인하지 않았다.
