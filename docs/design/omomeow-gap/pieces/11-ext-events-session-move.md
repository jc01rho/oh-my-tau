# 확장 이벤트 · 세션 이동 설계 (11-ext-events-session-move)

## 1. 현재 상태 (근거)

- 이벤트 정의는 `crates/omp-ext/src/lifecycle.rs:87`의 `enum LifecycleEvent` 하나이고 `#[serde(tag = "type")]`로 직렬화된다. 세션 전환 관련 variant는 `SessionBeforeSwitch`(`lifecycle.rs:164`, 필드 `reason`, `target_session_file`)와 `SessionSwitch`(`lifecycle.rs:170`, `reason`, `previous_session_file`)뿐이다. `session_move_export`/`session_move_import`에 해당하는 variant는 없다.
- wire 변환은 `LifecycleEvent::to_value`(`lifecycle.rs:316`) → `ExtHost::emit_event`(`lifecycle.rs:462`) → `ExtHost::emit`(`crates/omp-ext/src/host.rs:544`) 경로다. 편의 함수는 `notify_helpers!` 매크로(`lifecycle.rs:470`)가 만들고, `emit_session_switch`(`lifecycle.rs:496`), `emit_session_before_switch`(`lifecycle.rs:651`)가 그 결과다. 취소형 이벤트는 `CancelOutcome::from_result`로 `result.cancel`을 읽는다(`lifecycle.rs:333` 부근).
- 호출 시점: `session_before_switch` 게이트는 `crates/omp-cli/src/ext.rs:1081`의 `set_switch_gate` 클로저가 `has_handlers("session_before_switch")`를 먼저 보고(`ext.rs:1084`) `emit_session_before_switch`(`ext.rs:1093`)를 await한다. `session_switch`는 `set_switch_listener`(`ext.rs:1062`)가 `push_session_ctx`(`ext.rs:1066`, 정의 `ext.rs:1162`) 후 emit한다(`ext.rs:1077`). 게이트 주입 지점은 `Runtime::check_switch`(`crates/omp-cli/src/runtime.rs:2570`)이고 `new_session`(`runtime.rs:2216`), `load_session_as`(`runtime.rs:2287`)가 부른다. 취소는 `SwitchCancelled`(`runtime.rs:259`)로 전파되고 `is_switch_cancelled`(`runtime.rs:271`)가 판정한다. 전환 완료 통지는 `notify_session_switch`(`runtime.rs:2615`) → `runtime.rs:2248`, `runtime.rs:2461`.
- 전송 계층: `ExtHost::emit`은 이벤트 타입을 화이트리스트로 거르지 않는다. `emit_skipping`(`host.rs:551`)이 `has_handlers`(`host.rs:575`)로 구독 여부만 보고 `request("emit", p)`(`host.rs:568`)를 보낸다. `emit_nowait`(`host.rs:604`)는 응답을 기다리지 않는 경로다. 즉 코어 쪽에 variant만 추가하면 Bun 호스트까지는 그대로 도달한다.
- Bun 호스트 `ext-host/host.ts`의 emit 디스패치는 `case "emit"`(`host.ts:1022`)에서 타입별 분기 없이 핸들러를 돌린다(`host.ts:892` 이후 일반 분기). 그래서 새 이벤트 타입 추가에 host.ts 수정은 필수가 아니다. 다만 `api.rpc.handle`은 없다. `makeApi`(`host.ts:342`)가 노출하는 것은 `logger`, `typebox`, `zod`, `events`(`host.ts:356`), `pi`, `on`(`host.ts:372`), `sessionManager`(`host.ts:237`), `modelRegistry`(`host.ts:238`) 등이고 `rpc` 멤버는 grep 0건이다. `api.events.on`은 있으므로 `wake_source_state` 구독은 가능하지만 `api.rpc.handle("omomeow.move-readiness", ...)`는 불가능하다.
- 세션 파일: 확장 상태는 `custom` 엔트리로 남는다. `crates/omp-session/src/entries.rs:241`의 `Custom { custom_type, data }`가 그 정의이고 `CustomMessage`(`entries.rs:251`)는 컨텍스트에 들어가는 별개 종류다. 기록 경로는 `SessionWriter::append_entry`(`crates/omp-session/src/lib.rs:598`)로, `type`/`id`/`parentId`/`timestamp`를 생성해 payload보다 우선한다(`lib.rs:604-620`). `STRICT_KINDS`(`entries.rs:522`)에 `custom`은 없으므로 스키마 검증 없이 `FileEntry`로 떨어진다. 상위 경로는 `Persistence::append_entry`(`crates/omp-cli/src/persist.rs:142`), `Runtime::append_entry`(`runtime.rs:4503`), 확장 알림 `append_entry` 처리(`ext.rs:1241-1248`)다.
- 종료 신호: `agent_settled`는 `crates/omp-cli/src/ext.rs:1593`에서 `has_handlers("agent_settled")` 확인 후 `emit_nowait(&json!({"type":"agent_settled"}))`(`ext.rs:1595`)로만 나간다. `LifecycleEvent`에는 없는 타입이다.
- 설정 루트: 확장 디렉터리는 `~/.omp/agent/extensions`(`ext.rs:83`, `ext.rs:91`), 세션 디렉터리는 `~/.omp-rs/sessions/<slug>`(`persist.rs:62-66`), 소켓은 `~/.omp-rs/run/ext-<key>.sock`(`ext.rs:152-168`)이다. `~/.omo`를 읽는 코드는 없다.
- RPC 트리거 없음: `dispatch`(`crates/omp-cli/src/rpc.rs:593`)의 arm 목록(`rpc.rs:595`-`rpc.rs:1083`)에 `release_session`, `move_session`이 없고 미지 명령은 `Unknown command: {other}`(`rpc.rs:1094`)로 떨어진다.

## 2. 게이트웨이가 요구하는 계약

mhc 기준 계약은 다음으로 확인했다.

- 이벤트 이름: `SessionMoveExport => "session_move_export"`, `SessionMoveImport => "session_move_import"`(`/data/home/git/mhc/crates/maho-ext-api/src/types.rs:978-979`).
- export 페이로드: `{ move_id, dir, signal }`, import 페이로드: `{ move_id, dir, redelivered, signal }`(`maho-ext-api/src/types.rs:3310-3321`, 구조체 정의 `maho-core/src/extensions/types.rs:25-38`). `signal`은 `AbortSignal`이고 wire에는 실리지 않는다.
- 방출 지점: `AgentSession::emit_session_move(move_id, dir, redelivered: Option<bool>)`(`maho-core/src/agent_session.rs:6026-6048`)가 `None`이면 export, `Some`이면 import로 분기한다.
- export 시점: `release_session` 처리 중, 런타임이 살아 있을 때. `prepare_outgoing_state`로 스테이징 폴더를 만든 직후 `emit_session_move(move_id, &dir)`를 호출하고(`maho-rpc/src/session_release.rs:512-525`), 실패는 `ReleaseWriteError::Other`로 릴리스 자체를 실패시킨다(`session_release.rs:522-524`).
- 스테이징 경로: `<session_file 부모>/.move-state/out/<move_id>/<attempt>`(`session_release.rs:53`, `session_release.rs:71`, attempt는 16자 uuid `session_release.rs:110-113`). 이전 attempt 중 seal되지 않은 폴더는 제거된다(`session_release.rs:100-107`).
- seal: `session_released` custom 엔트리에 `data.state_attempt`를 넣어 봉인한다(`session_release.rs:50`, `session_release.rs:588-601`). 이후 export 번들은 이 attempt의 폴더만 싣는다.
- import 시점: 세션 open 시 `session_start` 방출 뒤 `restore_moved_state_with_mark`로 `emit_session_move(..., Some(redelivered))`를 호출하고(`agent_session.rs:18986-19010`), 실패는 `ContinuationError`로 보고하되 open을 막지 않는다. `redelivered`는 `delivery/<name>/delivered` 마커 존재 여부로 정해진다(`maho-core/src/session_move/extension_state.rs:161-178`).
- 핸들러 예산: `run_session_move_handlers`가 warn/timeout을 두고 초과 시 abort 후 실패를 모은다(`maho-core/src/extensions/session_move_emit.rs:22-46`). 실패가 하나라도 있으면 `emit_session_move`가 `Err`를 돌려준다(`agent_session.rs:6053-6060`).
- 트리거 wire: 게이트웨이는 `{"type":"release_session","sessionId","reason","interrupt","force"}`를 보내고(`mhc/crates/omo/components/maho-omo-gateway/src/thread/live_surface.rs:455`), 파서에는 `moveId`/`target`도 있다(`maho-rpc/src/rpc_types.rs:600-607`). 확인 필요: 위 게이트웨이 호출에 `moveId`가 실리지 않으므로 move 경로가 어느 요청으로 시작되는지는 추가 확인이 필요하다.

tau와의 차이(설계가 메워야 할 것): 이벤트 2종 부재, 스테이징/seal 개념 부재, `release_session` RPC 부재, `~/.omo` 미지원, `api.rpc.handle` 부재, 종료 신호 이름 불일치(`agent_end` vs `agent_settled`), 세션 포맷 차이(omp JSONL v3 vs senpi).

## 3. 설계

### 3.1 이벤트 variant와 emit 헬퍼

`LifecycleEvent`에 두 variant를 추가한다. `session_switch` 계열과 같은 `#[serde(rename_all = "camelCase")]`를 쓴다.

```rust
#[serde(rename = "session_move_export", rename_all = "camelCase")]
SessionMoveExport { move_id: String, dir: String },
#[serde(rename = "session_move_import", rename_all = "camelCase")]
SessionMoveImport { move_id: String, dir: String, redelivered: bool },
```

`AbortSignal`은 mhc도 wire에 싣지 않으므로 필드로 두지 않는다(`lifecycle.rs` 모듈 주석의 "AbortSignal members are not serializable" 규칙과 동일). 헬퍼는 `notify_helpers!`(`lifecycle.rs:470`)에 넣지 않고 별도 async fn 두 개로 둔다. export/import는 결과의 `errors`를 호출자가 판정해야 하므로 `emit_event`의 `EmitResult`를 그대로 돌려주는 편이 맞다.

```rust
pub async fn emit_session_move_export(&self, move_id: String, dir: String) -> Result<EmitResult>
pub async fn emit_session_move_import(&self, move_id: String, dir: String, redelivered: bool) -> Result<EmitResult>
```

### 3.2 페이로드 스키마 (wire)

export: `{"type":"session_move_export","moveId":"<16 hex>","dir":"<절대경로>"}`
import: `{"type":"session_move_import","moveId":"<16 hex>","dir":"<절대경로>","redelivered":false}`

ctx는 `ExtHost::set_ctx_provider`(`host.rs:520`)가 붙이는 라이브 ctx를 그대로 쓴다. 핸들러가 `ctx.sessionManager.getSessionFile()`을 읽어야 하므로(`omomeow/src/session-move-extension.ts`의 `registerSessionMoveExtension`), emit 전에 `push_session_ctx`(`ext.rs:1162`)가 보장한 `sessionFile`이 유효해야 한다. export는 이동 전 세션, import는 열린 세션의 파일이어야 한다.

### 3.3 호출 시점

export(이동 직전, 런타임 생존 중):
1. 세션 busy 판정(`Runtime::has_pending_messages` `runtime.rs:2022`, `agent.is_streaming()`).
2. 스테이징 폴더 생성 `<session_dir>/.move-state/out/<move_id>/<attempt>`.
3. `emit_session_move_export` await. 여기서 실패하면 2의 폴더를 지우고 이동 중단.
4. `session_released` seal 엔트리를 `Runtime::append_entry("custom", ...)`(`runtime.rs:4503`)로 기록. `data = {move_id, state_attempt, target, session_id}`.
5. 그 뒤에야 세션 teardown/전환을 진행한다.

import(세션 open 직후):
1. `session_start` 방출(`ext.rs:826`) 완료 후.
2. `pending_moved_state` 판정: 대상 세션 디렉터리에 `.move-state/in/<stem>.<move_id>`가 있으면 import 대상.
3. `emit_session_move_import(..., redelivered)` await. 실패는 `ContinuationError` 성격의 프레임으로 보고하고 세션은 계속 연다.
4. 성공 시 `session_move_state_restored` custom 엔트리 기록(중복 배달 방지 마커).

### 3.4 확장 상태 저장 형식

- 파일: `<session_dir>/.move-state/out/<move_id>/<attempt>/` 아래 자유 트리. 확장이 `dir` 하위에 자기 네임스페이스 폴더를 만든다(예: `omo-nyang/manifest.json`). tau는 내용을 해석하지 않고 통째로 번들에 싣는다.
- seal: omp JSONL의 `custom` 엔트리. `entries.rs:241-249`의 `Custom { custom_type, data }`를 쓰고 `custom_type = "session_released"`, `data.state_attempt = "<attempt>"`. `append_entry`가 `id`/`parentId`/`timestamp`를 생성하므로(`lib.rs:604-620`) 필드 충돌은 없다.
- import 마커: `custom_type = "session_move_state_restored"`, `data = {move_id, attempt}`. 재배달 판정의 근거다.
- 확인 필요: `entry!` 매크로가 `custom_type`을 `customType`으로 직렬화하는지. mhc의 seal 판독은 `entry["customType"]`(`session_release.rs:74-84`)을 보므로 키 이름이 다르면 상호 호환이 깨진다.

### 3.5 실패 시 이동 중단 조건

이동을 중단(부분 상태를 남기지 않고 롤백)하는 조건:
- busy: 스트리밍 중이거나 대기 메시지가 있으면 스테이징 전에 거부. 코드는 `session_busy`(`rpc.rs:755`, `rpc.rs:1035`와 같은 coded error).
- 스테이징 생성 실패: `create_dir_all` 오류. 아무것도 쓰지 않았으므로 그대로 반환.
- export emit 전송 실패(`Err`): 폴더 삭제 후 중단.
- export 핸들러 오류(`EmitResult.errors` 비어 있지 않음): 폴더 삭제 후 중단. mhc가 실패를 릴리스 실패로 승격하는 것과 동일(`session_release.rs:522-524`).
- 핸들러 타임아웃: 예산 초과는 오류로 집계되어 위와 같이 중단. 예산 값은 `HostConfig`의 handler timeout을 재사용.
- seal 기록 실패: 이미 쓴 스테이징 폴더를 삭제하고 중단. seal 없는 폴더는 다음 시도에서 dead attempt로 제거된다(`session_release.rs:100-107`와 같은 규칙).
- 중단 후 재시도는 새 attempt로 시작한다. 같은 `move_id`의 이전 폴더는 seal에 없으므로 폐기 대상이다.

중단하지 않는 조건: import 실패. 열린 세션을 되돌릴 수 없으므로 오류 보고 후 진행하고 `redelivered=true` 재시도에 맡긴다.

### 3.6 트리거

`dispatch`(`rpc.rs:593`)에 `"release_session"` arm을 추가한다. `reason == "move"`이고 `moveId`가 유효할 때만 3.3의 export 시퀀스를 돌리고, 그 외는 teardown만 한다. 응답 형태는 mhc와 맞춰 `{released:true, state_attempt}` 또는 오류 코드(`session_busy`, `release_failed`)로 한다.

### 3.7 호스트 측

`ext-host/host.ts`는 수정하지 않는다(3.1의 근거). 단 `api.rpc.handle`이 필요한 확장(예: `omomeow.move-readiness`)은 동작하지 않으므로, `extension_request` RPC와 `api.rpc` 브리지는 별도 조각으로 분리한다.

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-ext): add session_move_export/import lifecycle events` — `lifecycle.rs` variant 2개, emit 헬퍼 2개, 직렬화 골든 테스트.
2. `feat(omp-cli): stage extension move state beside the session file` — `crates/omp-cli/src/session_move.rs` 신규: `prepare_outgoing_state`, `discard_outgoing_state`, `sealed_attempt`, 경로 규칙. 단위 테스트 포함.
3. `feat(omp-cli): emit session_move_export before a move teardown` — export 시퀀스와 3.5 중단 규칙, `session_released` seal 기록.
4. `feat(omp-cli): restore moved extension state on session open` — `session_move_import` 방출, `session_move_state_restored` 마커, 재배달 판정.
5. `feat(omp-cli): add release_session rpc with move reason` — `rpc.rs` arm, busy 거부, 응답 스키마.
6. `test(e2e): cover session move export/import in the ext probe` — `tests-e2e/ext/probe-all-events.ts`의 `EVENTS`에 두 타입 추가.

## 5. 테스트

- 단위: `lifecycle.rs`에서 두 variant의 `to_value()` 결과가 `{"type":"session_move_export","moveId":...,"dir":...}`와 정확히 일치하는지. `redelivered:false`가 생략되지 않는지(생략 금지 필드) 확인.
- 단위: `session_move.rs`에서 스테이징 경로가 `.move-state/out/<move_id>/<attempt>`이고, seal이 없는 이전 attempt가 제거되며, seal된 attempt는 보존되는지.
- 단위: seal 엔트리 직렬화가 `type:"custom"`, `customType:"session_released"`, `data.state_attempt`를 갖는지(`lib.rs:598`의 생성 필드 우선 규칙 포함).
- 통합: export 핸들러가 오류를 내면 세션 파일에 seal이 남지 않고 스테이징 폴더가 사라지는지. 이동이 실제로 일어나지 않았는지.
- 통합: import 재배달에서 `redelivered`가 두 번째 호출에 `true`인지.
- e2e: `tests-e2e/ext/conformance.py`의 rpc 모드에 move 시나리오를 추가해 probe.jsonl의 이벤트 순서(`session_move_export` → `session_released` seal)를 검증.
- 금지: 고정 sleep으로 핸들러 완료를 기다리지 않는다. emit 응답 또는 파일 상태 변화를 구독해 기다린다.

## 6. QA 시나리오

1. 정상 이동: 이동 가능한 세션에서 release(move) → `session_move_export` 1회, 스테이징 폴더에 확장 산출물, seal 엔트리 1줄, 세션 파일 재개방 시 `session_move_import` 1회(`redelivered=false`), 마커 기록.
2. 스트리밍 중 이동: `session_busy` 거부, 세션 파일과 스테이징 디렉터리 모두 무변경.
3. export 핸들러 예외: 이동 실패 응답, 스테이징 폴더 없음, seal 없음, 원 세션이 그대로 사용 가능.
4. 핸들러 지연: 예산 초과 시 실패로 집계되고 이동 중단, 프로세스는 생존.
5. seal 기록 실패(디스크 읽기 전용): 이동 중단, 스테이징 폴더 정리.
6. import 실패 후 재시도: 첫 open에서 오류 보고, 두 번째 open에서 `redelivered=true`로 재시도되고 마커가 남는다.
7. 확장 미설치: `has_handlers`가 false라 emit이 생략되고 이동은 seal만 남기고 정상 완료.

## 7. 위험과 미확인

- 확인 필요: `entry!` 매크로의 필드명 직렬화(`custom_type` → `customType`). mhc seal 판독 키와 일치하지 않으면 상호 운용이 깨진다.
- 확인 필요: 게이트웨이가 실제로 `moveId`를 담아 `release_session`을 보내는지. `live_surface.rs:455`의 명령에는 `moveId`가 없다.
- 확인 필요: 슬랙 ts 1795326973.474349 본문. 로컬 relay DB(`/data/home/.local/share/omomeow/relay/agentd.db`)에서 해당 ts를 찾지 못해 5개 범주 목록 원문을 직접 확인하지 못했다. 이 문서는 프롬프트에 요약된 범주(RPC 디스패치, CLI 플래그, 확장 호스트 `api.rpc`/move 이벤트, 도구 5종, 설정 루트·종료 신호·세션 포맷)를 기준으로 작성했고, 이 조각의 범위(확장 이벤트·이동) 밖 항목은 다루지 않았다.
- 위험: tau 세션 파일은 `~/.omp-rs/sessions`(`persist.rs:62-66`)에 있고 게이트웨이는 senpi 포맷을 기대한다. 이동 번들에 세션 파일 자체를 넣는지, 확장 상태만 넣는지는 mhc도 확장 상태만 넣으므로(`session_release.rs:53`의 `.move-state`) tau도 같은 범위로 두었다. 세션 본문 변환은 별도 조각 소관이다.
- 위험: `session_switch` 리스너는 `tokio::spawn` 안에서 emit하므로(`ext.rs:1064-1079`) 순서 보장이 약하다. export는 이동 전에 완료를 기다려야 하므로 리스너 경로를 재사용하지 않고 동기 시퀀스로 둔다.
- 위험: `emit_nowait`(`host.rs:604`)는 실패를 콜백으로만 돌려주므로 export/import에는 쓰지 않는다. 반드시 await 경로(`emit`)를 쓴다.
- 미확인: `agent_settled`(`ext.rs:1593`)가 게이트웨이의 종료 대기와 맞물리는지. 이 조각에서는 이름만 확인했고 게이트웨이 대기 로직은 확인하지 않았다.
- 미확인: `api.rpc.handle` 부재가 move-readiness 확장에 미치는 영향의 정도. `api.events.on`은 있으므로(`host.ts:356`) 상태 구독 자체는 가능하다.
