# Phase 6b 코어 확장 이벤트 방출 설계 (스웜 조각)

`docs/parity/ext-events.md` 원장 기준 업스트림 `on()` 이벤트 54종 가운데 14종은 `ext-host/host.ts`가 집계만 하고 Rust 코어(`crates/omp-cli`, `crates/omp-ext`)가 방출하지 않거나 결과를 버린다. 이 문서는 그 14종과 `runEphemeralTurn` 잔여 옵션을 조각 하나씩으로 나눈 작업 명세다. 조사 기준 업스트림은 `can1357/oh-my-pi` main `efcbef6`(2026-11-22)이고, 행 번호는 모두 그 커밋 기준이다.

통합 브랜치는 `task/phase6b-core-events`이다. 조각 브랜치 `task/phase6b-<slug>`는 이 브랜치에서 갈라지고, 통합자가 merge commit으로 합친 뒤 merge commit PR 하나로 `main`에 넣는다. 원장(`docs/parity/ext-events.md`)은 통합자만 갱신한다.

## 1. 공통 규칙

### 준비

1. `~/git/oh-my-tau`(원격 `jc01rho/oh-my-tau`, 없으면 그 경로로 클론)에서 `git fetch origin` 후 `git worktree add -b task/phase6b-<slug> ~/git/oh-my-tau-worktrees/phase6b-<slug> origin/task/phase6b-core-events`. 그 워크트리 안에서만 편집하고 `task-control worktree ROUTE <경로>`로 등록한다. 기본 체크아웃은 건드리지 않는다.
2. 업스트림 사본(읽기 전용): `git clone --depth 1 --filter=blob:none --sparse https://github.com/can1357/oh-my-pi ~/.cache/omt-phase6b-upstream` 후 `git -C ~/.cache/omt-phase6b-upstream sparse-checkout set packages/coding-agent packages/agent packages/ai`. main이 `efcbef6`에서 움직였으면 그대로 쓰되 보고에 SHA를 적는다.
3. `rustup toolchain install stable`(rustc 1.100 이상), `export CARGO_TARGET_DIR=$HOME/.cache/omt-target-phase6b-<slug>`(`/tmp` 금지). C 컴파일러(`cc`)가 없으면 즉시 보고한다. `bun`이 필요하다.

### 핵심 코드

- `crates/omp-ext/src/lifecycle.rs`: `LifecycleEvent`(serde tag `type`, payload는 업스트림 camelCase 그대로), 핸들러 결과 파서(`*Outcome`), `notify_helpers!` 방출 헬퍼, `HostResponder`(host→core 요청 처리, `answer`).
- `crates/omp-ext/src/host.rs`: `ExtHost`(`emit`, `emit_nowait`, `has_handlers`, `call`).
- `crates/omp-cli/src/ext.rs`: `Extensions`(`attach`, `subscribe_lifecycle`, `forward_event`, `notify()`, `RuntimeResponder`, `CompactBridge`), 테스트의 `sh_host` 가짜 호스트 패턴.
- `ext-host/host.ts`: `emit`의 이벤트별 집계(840~1000행), 테스트는 `ext-host/host.test.ts`(`bun test`).
- 업스트림: `packages/coding-agent/src/extensibility/extensions/{types.ts,runner.ts,loader.ts}`, `extensibility/shared-events.ts`, `session/agent-session.ts`, `modes/interactive-mode.ts`.

충돌을 줄이려고 새 `LifecycleEvent` variant는 enum 끝(`SessionShutdown` 바로 앞)에, 새 방출 헬퍼는 `notify_helpers!` 블록 끝에, `Extensions`에 더하는 메서드는 `impl Extensions` 블록 끝에 둔다.

### 규칙

- 진짜 동작만 넣는다. 성공을 거짓으로 알리는 스텁, 조용한 no-op, TODO 자리표시는 금지다. 정말 불가능한 부분은 그 부분만 멈추고 이유를 보고한다.
- 업스트림 payload 필드명·모양과 방출 시점을 그대로 따르고, 핸들러 결과도 업스트림처럼 반영한다. 구독자가 있을 때만 방출(`host.has_handlers`)하는 기존 방식을 따른다.
- 이 조각의 결정적 테스트를 추가한다. sleep 금지, 이벤트를 먼저 구독한 뒤 트리거한다. 새 동작마다 변경 없이는 실패하는 테스트가 있어야 하고, 기존 테스트를 지우거나 건너뛰지 않는다.
- 최소 diff, 기존 스타일, 무관한 리팩터링 금지. `vendor/`를 고치면 같은 변경에서 `vendor-src.lock`의 `modifications`를 갱신한다.
- 푸시 전 전부 통과: `cargo fmt --all -- --check`; `cargo clippy -p omp-ext -p omp-cli --no-default-features --all-targets --locked -- -A clippy::pedantic -D warnings`(건드린 다른 크레이트도 같은 clippy); `cargo test -p omp-ext --locked`; `cargo test -p omp-cli --no-default-features --locked`; `ext-host`를 건드렸으면 `(cd ext-host && bun test)`.
- Conventional Commits로 커밋하고 `git push -u origin task/phase6b-<slug>`. PR 생성과 `main` 머지는 하지 않는다. 워크트리도 지우지 않는다.

### 보고

최종 답에 브랜치, head SHA, 바뀐 파일, 방출 위치(file:line), 결과 반영 방식, 추가한 테스트 이름과 실행 명령·통과 수, 못 한 것과 이유를 쓴다. 그 뒤 `task-control status ROUTE running "통합 대기: task/phase6b-<slug> 푸시 완료"`로 선언하고 멈춘다. 통합 PR이 머지됐다는 알림을 받으면 그때 done을 선언한다.

## 2. 조각

| slug | 이벤트 | tau 쪽 지점 |
|---|---|---|
| `clear` | `session_clear` | `/clear` 컨텍스트 리셋(현재는 새 세션) |
| `compacting` | `session.compacting` | `compact.rs` 요약 직전 |
| `cache-warming` | `cache_warming_decision` | 캐시 워머(현재 설정만 있고 동작 없음) |
| `before-shutdown` | `session_before_shutdown` | TUI·RPC 종료 경로 |
| `provider-request` | `before_provider_request` | `adapter.rs` `on_payload` |
| `provider-response` | `after_provider_response` | `pa-agent` `OnResponseHook` |
| `subagent-spawn` | `before_subagent_spawn` | task 도구 자식 실행 |
| `credential-disabled` | `credential_disabled` | `omp-providers` 자동 소프트 비활성화 |
| `plan-review` | `plan_review_requested`, `plan_review_resolved` | `features/plan.rs` |
| `mode-changed` | `mode_changed` | plan·loop·goal·vibe 모드 상태 |
| `mcp-notification` | `mcp_notification` | `omp-mcp` 알림 콜백 |
| `link-activate` | `link_activate` | `registerLinkScheme`·소유자 라우팅·TUI 활성화 |
| `resources-discover` | `resources_discover` | 스킬·프롬프트·테마 로더 |
| `ephemeral` | `runEphemeralTurn` 잔여 옵션 | `ext.rs` `RuntimeResponder::run_ephemeral_turn` |

### 2.1 `clear`: `session_clear`와 `/clear` 컨텍스트 리셋

- 업스트림: `slash-commands/builtin-lifecycle.ts` 281행(`/clear` "Clear the conversation context in place, keeping the session") → `handleResetContextCommand` → `modes/controllers/command-controller.ts` 1410행 → `session/agent-session.ts` `resetSessionContext()` 7448~7537행. 성공 뒤 `{type:"session_clear"}`를 방출한다. `sessionManager.appendResetBoundary()`로 영속 경계를 남기고, 모델 컨텍스트 재구성은 마지막 경계 이후부터 한다. 이벤트 정의는 `shared-events.ts` 51~55행.
- tau: `crates/omp-cli/src/slash.rs` 555행 근처 `/clear`는 옛 동작(새 세션 시작, `rt.new_session`)이다.
- 할 일: 업스트림 `resetSessionContext` 의미대로 이식한다. 세션 id·파일은 유지하고, 라이브 컨텍스트를 비우고, 업스트림 모양의 리셋 경계 엔트리를 기록하고, 재개·재로드 때 경계 이후만 컨텍스트로 복원한다. 스트리밍 중에는 거부하고, 성공하면 `session_clear`를 방출한다. 업스트림 RPC에 같은 동작 명령이 있으면 `rpc.rs`에도 연결한다. `docs/parity/slash.md`는 고치지 말고 차이만 보고한다.
- 테스트: `/clear` 뒤 세션 id 유지·메시지 비움·경계 기록, 재로드 시 경계 이후만 복원, `session_clear` 방출(가짜 호스트 수신), 스트리밍 중 거부.

### 2.2 `compacting`: `session.compacting`

- 업스트림: `session/session-maintenance.ts` 4155~4200행. 확장 `session_before_compact`가 요약을 대신 주지 않았고(`hookCompaction` 없음) `session.compacting` 구독자가 있으면 `{type:"session.compacting", sessionId, messages}`를 방출한다. 결과 `SessionCompactingResult`(`shared-events.ts` 513~521행)의 `context`(요약 입력에 덧붙일 줄), `prompt`(기본 요약 프롬프트 대체), `preserveData`(compaction 엔트리에 저장)를 반영한다. 호스트는 마지막 결과를 쓴다.
- tau: `crates/omp-cli/src/compact.rs`(요약 파라미터 290~680행), `runtime.rs` `compact_inner`(3240~3420행), `ext.rs` `CompactBridge`(`CompactionHooks` before/after, 619~666행).
- 테스트: `context`·`prompt`가 실제 요약 요청에 들어가고 `preserveData`가 compaction 엔트리에 남는지, 구독자가 없으면 요청이 그대로인지.

### 2.3 `cache-warming`: 캐시 워머와 `cache_warming_decision`

- 업스트림: `session/cache-warmer.ts`(1278행: 모드 `off|streaming|idle`, 경제성 계산, `prewarm|replay` 방식, `warm|compact|stop` 행동), 방출 844행, `sdk.ts` 5070행(`decide: event => extensionRunner.emitCacheWarmingDecision(event)`), `runner.ts` 1132행, payload와 결과는 `shared-events.ts` 440~480행.
- tau: 설정만 있다(`runtime.rs` 103·501·4006행 `cache_warming`, `rpc.rs` 1178행 `set_cache_warming`, `CACHE_WARMING_MODES`). 값을 저장할 뿐 아무 일도 하지 않는 조용한 no-op이다.
- 할 일: tau가 지원하는 프롬프트 캐시 공급자에 대해 워머를 이식하고 설정에 연결한다(`off`면 동작 없음). 결정마다 워머의 경제성 값을 담아 `cache_warming_decision`을 방출하고, 업스트림 규칙대로 덮어쓴다: `warm`은 `method`가 있어야 하고, `compact`는 `compactCost`가 있고 만료 1분 이상 전이어야 하며, 수행할 수 없는 덮어쓰기는 워머 자신의 결정을 유지하고, `stop`은 다음 실제 요청까지 워밍을 멈춘다.
- 테스트: 경제성 계산(업스트림 테스트가 있으면 그 사례 이식), 덮어쓰기 적용 규칙, 구독자 있을 때만 방출, 조절 가능한 시계로 스케줄 검증(sleep 금지).

### 2.4 `before-shutdown`: `session_before_shutdown`

- 업스트림: `session/agent-session.ts` 7000~7020행(구독자가 없으면 바로 통과, 있으면 `{type:"session_before_shutdown", signal}` 방출 후 `cancel`이면 종료 취소), `modes/interactive-mode.ts` 1898·8484행(운영자가 닫는 경로 중 취소 가능한 것), 정의 `shared-events.ts` 108~113·523~526행. 호스트는 `session_before_*` 규칙(`cancel`이 단락)으로 집계한다.
- tau: TUI 종료 경로(`crates/omp-cli/src/tui.rs`, `crates/omp-tui`; `/exit`, `/quit`, Ctrl+D, Ctrl+C 두 번 등), 업스트림 RPC 모드에 같은 게이트가 있으면 `rpc.rs`도.
- 할 일: 업스트림과 같은 종료 경로에서 방출하고 `cancel`이면 닫지 않는다. `signal`은 닫기가 더 기다리지 않을 때(다른 닫기 요청, 시그널, UI 중지) abort되어야 한다. 호스트가 `session_before_compact`의 `signal`을 어떻게 주는지 확인하고, 없으면 core가 대기를 끊을 때 호스트 쪽 signal을 abort하는 경로를 만든다.
- 테스트: `cancel` 시 유지, 아니면 종료, 구독자 없으면 미방출, 대기 중단 시 signal abort.

### 2.5 `provider-request`: `before_provider_request` payload 교체

- 업스트림: `runner.ts` 2741~2775행 `emitBeforeProviderRequest(payload, model, signal)`(핸들러마다 앞 핸들러의 결과를 받아 연쇄 교체, 최종 payload 반환), `session/session-maintenance.ts` 4077~4080행과 `sdk.ts` 4968행(구독자가 있을 때 스트림 옵션 `onPayload`로 연결), 정의 `types.ts` 1209~1212행.
- tau: `crates/omp-cli/src/adapter.rs`(`agent_payload_hook_to_ai` 57행, `chain_payload_hooks` 307행, `on_payload` 369행), `pa_ai::types::OnPayloadHook`, `ext.rs` `Extensions::before_provider_request`(829~838행: 빈 payload를 보내고 결과를 버림, 호출부 확인).
- 할 일: 실제 공급자 JSON payload를 보내고 돌아온 payload를 실제 요청에 반영한다. 업스트림이 이 훅을 거는 요청 종류(본 턴, 요약, 사이드 턴 등)와 같게 맞춘다.
- 테스트: 필드를 바꾸는 핸들러가 있으면 실제로 나가는 요청 본문에 반영되는지(가짜 공급자나 훅 체인으로 포착), 연쇄 교체, 구독자 없으면 payload 그대로이고 방출 없음.

### 2.6 `provider-response`: `after_provider_response`

- 업스트림: `types.ts` 1214~1217행 `AfterProviderResponseEvent extends ProviderResponseMetadata`(`packages/ai/src/types.ts` 422행: `status`, `headers: Record<string,string>`, `requestId?`, `metadata?`), `runner.ts` 2778~2810행 관찰 전용. coding-agent에서 `emitAfterProviderResponse` 호출부를 찾아 같은 지점(응답 수신 후, 스트림 본문 소비 전)에 건다.
- tau: `vendor/pa-agent/src/stream.rs` 115~131행 `ProviderResponse`, `OnResponseHook`, `adapter.rs`의 `on_response` 연결.
- 테스트: 가짜 공급자 응답의 status·headers·requestId가 이벤트로 호스트에 도착하는지, 구독자가 없으면 미방출.

### 2.7 `subagent-spawn`: `before_subagent_spawn`

- 업스트림: `task/structured-subagent.ts` 640~700행(실제 자식 실행 직전 방출: `agent`, `invocationKind: "task"|"eval"`, `modelRole?`, `patterns`, `spawnKey?`), 200~215행(모델 교체 라우팅 노트), `tools/index.ts` 455~470행, `sdk.ts` 2803행, `runner.ts` 2860~2900행(`block` 단락, 마지막 `model` 우선, `model`은 문자열 또는 배열로 패턴 교체, `note`), 정의 `types.ts` 1235~1248행과 결과 타입.
- tau: `crates/omp-task`(`runner.rs` 354행 `spawn_child`, `runner/control.rs` 452행, `agent.rs`), `crates/omp-cli/src/tools`의 task 도구와 모델 해석.
- 할 일: 자식 모델이 정해지기 전에 방출하고, `block`이면 업스트림처럼 이유와 함께 실행을 거부하고, `model`이면 그 패턴으로 실행하며 `note`를 업스트림처럼 다룬다.
- 테스트: block 거부, 모델 교체 반영, 구독자 없으면 기존 동작.

### 2.8 `credential-disabled`: `credential_disabled`

- 업스트림: `packages/ai/src/auth/refresh.ts` 850행 근처(OAuth `invalid_grant` 등으로 행을 소프트 비활성화하고 이벤트 발생), `auth/pool.ts` 110~120·265~275·585~595행(구독자가 없을 때 버퍼, 구독 시 비움), `auth-storage.ts` 256행, `auth/types.ts` 367·862행. coding-agent `runner.ts` 700~710·1030~1100행(초기화 전 버퍼, `emitCredentialDisabled`). payload `types.ts` 1341~1356행 `{provider, disabledCause, credentialId?, email?, accountId?, orgId?, orgName?}`.
- tau: `crates/omp-providers/src/auth.rs`, `login/store.rs`(자동 소프트 비활성화 지점), `crates/omp-auth-broker`(별도 데몬의 `disabled_cause`).
- 할 일: 자동 소프트 비활성화 지점에서 리스너로 알리고 `Extensions`가 방출한다. 확장이 붙기 전 이벤트는 버퍼했다가 붙을 때 보낸다. 사용자가 직접 지운 경우·로그아웃·중복 정리는 업스트림처럼 방출하지 않는다.
- 테스트: 자동 비활성화 → 이벤트 payload, 붙기 전 버퍼 후 전달, 수동 삭제는 미방출.

### 2.9 `plan-review`: `plan_review_requested`, `plan_review_resolved`

- 업스트림: `modes/interactive-mode.ts` 6725~6730·8080~8180행(리뷰가 열릴 때 requested, 닫힐 때 resolved: 선택 또는 `undefined`, `by: "local"|"extension"`), `types.ts` 830~855행(`ctx.resolvePlanReview(reviewId, choice, input?) → boolean`, `ctx.getPendingPlanReview()`), 1460~1515행(payload), `runner.ts` 640~641·998~999·1977~1978행.
- tau: `crates/omp-cli/src/features/plan.rs`(계획 포착 후 "Review the plan above, then /plan approve" 215행, `/plan approve` 222행, `/plan off`).
- 할 일: 승인을 기다리는 계획이 생기면 `plan_review_requested`(`reviewId`, `sessionId`, `title`, `planFilePath`, `planContent`, tau가 지원하는 선택지를 `options`로, 지원하지 않는 선택지는 `disabled: true`, 해석되는 tier가 둘 미만이면 `tiers` 생략)를 방출한다. `/plan approve`는 `choice: "execute", by: "local"`, `/plan off`·새 계획·세션 전환·종료는 선택 없는 resolved로 방출한다. `ext-host/host.ts`에 `ctx.resolvePlanReview`·`ctx.getPendingPlanReview`가 없으면 host→core 요청으로 구현해 `by: "extension"` 경로를 실제로 만든다.
- 테스트: 열림·승인·취소·확장 해결 각각의 payload, 대기 중 리뷰 조회.

### 2.10 `mode-changed`: `mode_changed`

- 업스트림: `modes/interactive-mode.ts` 1484~1500행(마지막 보고 상태는 전부 inactive로 시작해 복원된 모드도 보고), 3880~3945행(`{previous, state}` 방출, 여러 단계 전환은 끝난 뒤 한 번), `types.ts` 1520~1562행(`SessionModeState`: `plan{status, planFilePath?}`, `vibe{status}`, `goal{status, objective?, tokenBudget?}`, `loop{status}`).
- tau: `features/plan.rs`, `features/loop_mode.rs`, goal 상태(`goal_updated` 방출부), vibe(tau에 vibe 모드가 있는지 확인. 없으면 늘 inactive).
- 할 일: 슬래시·도구·RPC·복원 어느 경로든 계산된 상태가 바뀌면 방출한다.
- 테스트: 각 모드 진입·이탈, 복원 시 보고, 변화 없으면 미방출.

### 2.11 `mcp-notification`: `mcp_notification`

- 업스트림: `types.ts` 1360~1380행(`{server, method, params}`, `server`는 설정의 원래 이름), `runner.ts` 450~455행(버퍼 상한), 705~712·1050~1065행(초기화 때 비움), 1108~1120행 `emitMcpNotification`, `sdk.ts` 6405~6412행(런타임 자체 처리 뒤 전달), `mcp/manager.ts` 600행 근처.
- tau: `crates/omp-mcp/src/transport.rs` 99~150행 `NotificationCallback`, `omp-mcp` 매니저, `crates/omp-cli/src/tools/mcp.rs` 51행 `McpManager::from_default_configs`.
- 할 일: 모든 서버 알림을 런타임 자체 처리(`tools/list_changed` 등) 뒤 `Extensions`로 방출하고, 확장이 붙기 전에는 업스트림 상한으로 버퍼한다.
- 테스트: 가짜 MCP 서버가 보낸 사용자 정의 알림이 원래 서버 이름으로 도착, 버퍼 후 전달.

### 2.12 `link-activate`: `registerLinkScheme`, 소유자 라우팅, TUI 활성화

- 업스트림: `extensibility/extensions/loader.ts` 340~356행(`pi.registerLinkScheme` 검증: 두 글자 이상, 영문·숫자·`+-.`, 예약 스킴 거부), `runner.ts` 1700~1710행(`extensionRenderers.assistantText`가 꺼져 있으면 스킴 없음), 1839~1880행(`emitLinkActivate`: 로드 순서상 첫 소유 확장의 핸들러만, 핸들러 타임아웃 없음, 실패는 보고만, 소유 여부 반환), `types.ts` 405~415·1385~1398·2155~2166행, `modes/interactive-mode.ts` 4715~4730행(트랜스크립트에서 발생), `modes/types.ts` 267행(링크 힌트 모드).
- tau: `ext-host/host.ts`에는 `registerLinkScheme`이 없고 `link_activate`를 모든 확장에 관찰로 뿌린다. TUI(`crates/omp-tui`, `crates/omp-cli/src/tui.rs`)에는 링크 활성화 경로가 없다.
- 할 일: (1) 호스트에 `pi.registerLinkScheme`과 검증, core→host `link_activate {url}` 요청(소유자 라우팅, `{handled}` 반환)을 넣고 일반 브로드캐스트에서는 뺀다. 등록된 스킴을 core에 알린다. (2) `ExtHost`·`Extensions`에 링크 활성화 API. (3) TUI에서 등록 스킴 링크를 실제로 활성화하는 경로를 업스트림 링크 힌트 모드에 가깝게 만든다. tau 설정에 `extensionRenderers.assistantText`에 해당하는 게이트가 있는지 확인한다.
- 테스트: 호스트 bun 테스트(소유자만, 로드 순서 첫 소유자, 대소문자 무시, 모르는 스킴 false, 핸들러 오류 보고), Rust 라우팅, TUI 선택.

### 2.13 `resources-discover`: `resources_discover`

- 업스트림: `runner.ts` 2575~2620행 `emitResourcesDiscover(cwd, reason)`(`reason: "startup"|"reload"`), coding-agent의 호출부, 정의 `types.ts` 1150~1162행. 호스트는 이미 경로를 모으고 `extensionPath`를 붙인다.
- tau: 시작 때 `session_start` 뒤, 확장 재로드 때 방출할 지점(`ext.rs`), 스킬 `crates/omp-cli/src/sysprompt.rs` `load_skills`(847~895행), 프롬프트 템플릿 로더, `omp-tui` 테마 로더.
- 할 일: 업스트림 시점에 방출하고 돌려받은 `skillPaths`·`promptPaths`·`themePaths`를 해당 로더에 넣는다. 상대 경로 해석 기준은 업스트림을 따른다.
- 테스트: 반환 경로의 스킬·프롬프트·테마가 실제 로드되는지, reload 사유.

### 2.14 `ephemeral`: `runEphemeralTurn` 잔여 옵션과 스트리밍

- 업스트림: `session/agent-session-types.ts` 620~700행 `EphemeralTurnOptions`(`conversationKey`, `minimizeEffort`, `maxContextBytes`, `purpose`, `dedupeReply`, `replyMaxBytes`, `onTextDelta`, `onUsage`, `signal`), `session/agent-session.ts`의 `runEphemeralTurn` 구현, `runner.ts` 2000~2015행(`ephemeralTurnBlocker`).
- tau: `ext.rs` `RuntimeResponder::run_ephemeral_turn`(431~474행), `EphemeralTurn::parse`(482~524행), `features/btw.rs` `complete_side_turn`, `ext-host/host.ts`의 `runEphemeralTurn`. 지금은 위 옵션 여섯 개를 무시하고 `onTextDelta`에는 답 전체를 한 번에 준다.
- 할 일: 옵션마다 업스트림 의미를 구현하고, 텍스트 델타를 도착하는 대로 호스트의 `onTextDelta`로 흘려 보낸다. 취소 동작은 유지한다.
- 테스트: 옵션별 검증·효과, 델타 순서와 합이 최종 답과 같은지, 취소.
