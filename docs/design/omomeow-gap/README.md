# `oh-my-tau` (`omt`) — OmOMeow 게이트웨이 엔진 전환 부족 기능 추가 구현 종합 설계

기준 일자: 2026-11-22
기준 저장소 및 커밋:
- `oh-my-tau`: `e7a5378` (`main`, workspace version `0.12.0`, Phase 2.5 머지 완료)
- `omo-nyang` (OmOMeow 게이트웨이·릴레이): `mac-mini:/home/jc01rho/Work/omo-nyang`
- `mhc` (현행 게이트웨이 엔진 참조 구현): `v2026.11.20-omomeow1` (`f0f3571c5`)

참조 슬랙 스레드:
- 원문 스레드: `#proj-omomeow` `1795324158.334689` (`mhc` → `tau` 전환 가능성 검토)
- 추가 반영 메시지: `#proj-omomeow` `1795326973.474349` (2026-11-22 14:56, 부족 기능 5대 범주 열거 및 "확인 필요" 5개 항목)

---

## 1. 개요 및 스웜 분해 구조

본 설계는 `oh-my-tau`(`omt`)를 OmOMeow 게이트웨이(`omo-nyang`) 및 워커 데몬(`agentd`)의 실행 엔진으로 사용하기 위해 부족한 기능 전수를 코드 실측 기반으로 설계하고, 모든 작업을 가장 작은 원자 단위로 분해하여 병렬 스웜(`oc-deepseek-v4.1-flash-free` 기본 + 아키텍처 교차 리뷰 강한 모델 라우팅)으로 실행·검증한 결과물이다.

### 1.1 문서 디렉터리 구성 (`docs/design/omomeow-gap/`)

- `README.md` (본 문서): 전체 아키텍처, 통합 상태 기계, 5대 범주별 상세 설계, 12개 PR 실행 로드맵(Wave G1~G4), 86개 조각 전체 색인
- `00-gap-list.md`: 슬랙 `1795324158.334689` 및 `1795326973.474349`에서 도출된 부족 기능 5대 범주 원본 정의
- `01-confirm-needed-verification.md`: 슬랙 `1795326973.474349`에서 "확인 필요"로 표시된 5개 항목과 미머지 계획 단계(Phase 3·4·5)를 `oh-my-tau`·`omo-nyang`·`mhc` 실제 코드(파일:줄)로 전수 대조해 확정한 보고서
- `pieces/01-*.md` ~ `26-*.md` (**26개 도메인 설계·조사·테스트·QA·교차 리뷰 조각**):
  - `01`~`06`: 게이트웨이·`agentd` 필수 RPC 명령 8종 설계
  - `07`~`09`: CLI 실행 플래그 5종 설계
  - `10`~`11`: 확장 호스트(`ext-host` / `omp-ext`) API 및 세션 이동 이벤트 설계
  - `12`~`16`: OmOMeow 에이전트 도구 10종 설계
  - `17`~`20`, `23`: 하네스·운영 계층 5종 및 미머지 Phase 3·4·5 연계 설계
  - `21`~`22`: RPC 적합성 골든 테스트 하네스 및 게이트웨이 연동 7대 축 E2E QA 시나리오 설계
  - `24`: 슬랙 `1795326973.474349` "확인 필요" 5개 항목 코드 대조 조사 조각
  - `25`~`26`: RPC·플래그·확장 상태 기계 교차 정합성 리뷰(25번) 및 도구·하네스 Wave 1~4 로드맵 교차 리뷰(26번)
- `micro/` (**60개 초세분화 원자 조각** — `oc-deepseek-v4.1-flash-free` 스웜):
  - `impl-01` ~ `impl-15` (15개): 개별 구현 변경(커밋 단위) 명세
  - `test-01` ~ `test-12` (12개): 개별 단위·통합·골든 테스트 명세
  - `qa-01` ~ `qa-10` (10개): 개별 E2E QA 시나리오 명세
  - `rev-01` ~ `rev-08` (8개): 개별 와이어·동시성·격리·교착·샌드박스·보안·이식성 감사 명세
  - `inv-01` ~ `inv-08` (8개): 개별 코드 대조 조사 및 정량 공수 추정 명세
  - `doc-01` ~ `doc-07` (7개): 개별 패리티 원장·ADR·마이그레이션 계획 개정·WBS·카나리 런북 명세

---

## 2. 슬랙 `1795326973.474349` "확인 필요" 5개 항목 실측 확정 요약

상세 코드 대조는 `01-confirm-needed-verification.md` 및 `pieces/24-inv-confirm-needed-audit.md`, `micro/inv-01`~`inv-06`에 수록되어 있으며 핵심 결론은 다음과 같다.

| # | 슬랙 `1795326973.474349` 확인 필요 항목 | 코드 실측 근거 (`oh-my-tau` vs `omo-nyang`) | 확정 결론 |
|---|---|---|---|
| 1 | `--skill <경로>`: tau에는 `--skills` 형태만 보임 | `omo-nyang/src/gateway.ts:1167`은 `--skill <스킬 루트 디렉터리>`를 전달함. 반면 `crates/omp-cli/src/cli.rs:129, 309`의 `--skills`는 쉼표 구분 스킬 이름 글롭 필터(`a.skills: Vec<String>`)임. | 단순 별칭이 아님. 반복 지정 가능한 추가 스킬 탐색 디렉터리/파일 플래그(`--skill <경로>` → `Args::skill_paths: Vec<PathBuf>`)로 신설해야 함. |
| 2 | `task` 도구가 있으나 동작 방식이 같은지 확인 필요 | `crates/omp-cli/src/tools/task.rs:94-116, 483`은 `{context, tasks:[{task, solutionSpace}]}` 동기 배치 실행이며 `rpc.rs:863` `get_subagents`는 빈 배열 스텁임. OmOMeow는 `{category\|subagent_type, prompt\|tasks, run_in_background}` 비동기 `st_...` 세션 및 `task_send`·`task_output`·`task_cancel`을 요구함. | 스키마와 실행 모델(동기 배치 vs 비동기 상주 세션)이 모두 다름. `SubagentRegistry`(`st_...`)와 Dual-Schema 정규화 및 제어 도구 3종 신설 필수. |
| 3 | `ask`·`goal`이 `ask_user_question`·`create_goal`과 입력 형식이 맞는지 확인 필요 | `crates/omp-cli/src/tools/ask.rs:36`은 `ask({html}\|{questions:[{multi}]})`, `goal.rs:25`는 숨김 도구 `goal({op})`임. OmOMeow는 `ask_user_question({questions:[{header, question, multiSelect, options}], waitForAnswer, required})`와 상시 노출 `create_goal`·`get_goal`·`update_goal`을 호출함. | 도구명과 파라미터 스키마가 불일치함. `ask_user_question` 및 `create_goal`·`get_goal`·`update_goal` 호환 래퍼 도구를 추가 등록해야 함. |
| 4 | 게이트웨이는 `agent_settled`를 기다리고 tau는 `agent_end`를 보냄(`agent_settled` 1회 출현 의미 확인 필요) | `omo-nyang/src/relay/{runtime.ts:59, slice-run.ts:71, fleet.ts:258, agentd-sessions.ts:186}`은 RPC stdout에서 `{"type":"agent_settled"}`를 대기함. 반면 `tau`는 `ext.rs:1598`에서 확장 호스트 내부 파이프로만 `agent_settled`를 보내고 RPC stdout에는 `runtime.rs:1641`에서 `{"type":"session_settled"}`만 내보냄. | `runtime.rs:1641`의 정착 지점(`p.active && drained && p.open.is_empty()`)에서 RPC stdout으로 `{"type":"agent_settled"}` 프레임을 정식 발행해야 함. |
| 5 | 세션 기록 파일 형식(`omp JSONL v3` vs `senpi`)이 게이트웨이 기록 읽기에 영향을 주는지 확인 필요 | `omo-nyang/src/session-patterns.ts:47-76`은 `.jsonl`에서 `"message"` 포함 줄만 골라 `{timestamp, message:{role, content, toolCallId, isError}}`를 읽음. `crates/omp-session/src/entries.rs`의 `Entry::Message`는 동일 구조를 가짐. 단, `gateway.ts:1165`의 `--session <신규파일경로>` 기동 시 `tau`가 기존 파일 부재 오류를 냄. | 게이트웨이 JSONL 리더(`session-patterns.ts`)는 즉시 호환됨. 단 `--session <경로>`가 미존재 파일일 때 해당 경로에 새 세션을 생성·바인딩하도록 `persist.rs`를 보강하고 `mhc ↔ omt` 이동용 헤더/엔트리 어댑터를 추가해야 함. |

---

## 3. 통합 세션 상태 기계 및 와이어 계약 (Cross-Cutting Architecture)

`pieces/25-review-rpc-ext-flags-consistency.md` 및 `micro/rev-01`~`rev-04`에서 확정한 단일·다중 세션 통합 계약이다.

### 3.1 응답 봉투 및 에러 코드 직렬화 (`crates/omp-cli/src/rpc.rs:203-226`)

- 현재 `rpc.rs:203` `response()`는 실패 시 `{"type":"response","command":...,"success":false,"error":...,"code":...}`만 직렬화한다.
- 그러나 게이트웨이의 `MoveHost`(`omo-nyang/src/relay/move-host.ts:4-7, 61`)는 `errorCode` 필드를 파싱한다(`value.errorCode ?? "rpc_failed"`).
- **설계**: `CmdError`가 `code: Some(c)`를 가질 때 `response()`가 `"code": c`와 `"errorCode": c`를 **동시에 직렬화**한다. 미지원 명령 폴백(`rpc.rs:1094`)은 `CmdError::coded(format!("Unknown command: {other}"), "unknown_command")`를 반환하고, 필수 파라미터 누락(`str_field`, `bool_field`)은 `"invalid_params"` 코드를 부여한다.

### 3.2 세션 제어 상태 기계 (`SessionCtlState`)

`Runtime`(`crates/omp-cli/src/runtime.rs:116`) 내부에 `hold: Mutex<HoldLedger>`(`crates/omp-cli/src/hold.rs`)와 `client_info: Mutex<ClientInfo>`를 둔다.

```
[Active] ──(interrupt intent="stop", interactive_user 선언됨)──► [Paused (pause_seq += 1)]
   ▲                                                                  │
   └──────────────(resume_session / 보류 중 새 prompt)────────────────┘

[Active / Paused] ──(release_session)──► [Released] ──(move_session export/finish)──► [MovedAway]
```

- **역할 고정 불변식 (`set_client_info`)**:
  - `capabilities` 배열이 최초로 전달된 호출에서 클라이언트 역할(`interactive_user`, `task_parent`)을 고정한다.
  - 이후 호출에서 역할 집합을 바꾸려 하면 `CmdError::coded("connection_role_fixed: interactive_user and task_parent cannot change after they are declared", "connection_role_fixed")`로 거절한다.
  - `interactive_user`가 선언되지 않은 연결이 `interrupt { intent: "stop" }`을 보내면 `CmdError::coded("stop_requires_interactive_client: declare interactive_user with set_client_info first", "stop_requires_interactive_client")`로 거절한다(`omo-nyang/tests/lane-hold.test.ts` 계약).
- **`pauseSeq` 단조 증가 및 저널 영속화**:
  - `interrupt { intent: "stop" }`이 실행되면 진행 중 턴을 중단(`rt.abort().await`)하기 직전에 대기 큐(`steering`, `followUp`)를 스냅샷하고, `pause_seq`를 1 증가시킨 뒤 세션 JSONL에 `custom` 엔트리(`customType: "session-hold", data: { op: "add", kind: "session_hold", pauseSeq, queue }`)를 기록한다.
  - 이미 `Paused` 상태일 때 다시 `interrupt { intent: "stop" }`이 오면 `pause_seq`를 올리지 않고 현재 `pause_seq`로 멱등 성공한다.
  - `get_state`(`runtime.rs:1953`)는 `paused: bool`, `runPaused: bool`, `pauseSeq: u64`를 항상 포함한다.
- **락 획득 순서 불변식**:
  1. `SessionRegistry.slots` (`tokio::sync::Mutex`, 다중 세션 라우팅 테이블)
  2. `Runtime.hold` / `Runtime.client_info` (`std::sync::Mutex`, `.await` 경계 이전 즉시 해제)
  3. `Runtime.prompts` / `Runtime.settings` (`std::sync::Mutex`)
  4. `ExtHost` 비동기 요청 (`ExtHost::request` / `emit` 호출 시 어떠한 `std::sync::Mutex`도 보유하지 않음)

---

## 4. 5대 범주별 상세 구현 설계 요약

### 4.1 범주 1 — 게이트웨이·`agentd` 필수 RPC 명령 8종 (`crates/omp-cli/src/rpc.rs`)

| 명령 | 요청 페이로드 | 성공 응답 `data` 및 부수효과 | 상세 설계 조각 |
|---|---|---|---|
| `set_client_info` | `{ width: u16, capabilities?: string[] }` | `{ width, capabilities }`. `width` 누락 시 `invalid_params`, 역할 변경 시 `connection_role_fixed`. | `pieces/01`, `micro/impl-01`, `test-01` |
| `interrupt` | `{ intent?: "stop" \| "abort" }` | 무인자 또는 `"abort"`는 턴 중단 후 `{ interrupted: bool }`. `"stop"`은 `HoldLedger`에 보류 기록 후 `session_paused` 이벤트 방출. `busy`일 때 직렬 워커 큐를 우회(`rpc.rs:1502`). | `pieces/01`, `micro/impl-02`, `test-01` |
| `get_protocol_info` | `{ observe?: bool }` | `{ protocolVersion: 2, serverVersion: "0.12.0", engine: "omt", mode: "classic"\|"multi_session", capabilities: [...], commands: RPC_COMMANDS, agentDir }`. | `pieces/02`, `micro/impl-03`, `test-02` |
| `reload` | `{}` | 스트리밍/대기 메시지 중이면 `{ cancelled: true, reason }`. 유휴 시 `Settings::load` 재적용, 도구·스킬·확장 재적재(`Extensions::recover` 경로 재사용) 후 `{ cancelled: false }`. | `pieces/02`, `micro/impl-04`, `test-02` |
| `resume_session` | `{ expectedSeq?: u64, mode?: "continue"\|"release", message?: string, effectId?: string }` | 미보류 시 `{ resumed: false, reason: "not_paused", pauseSeq }`. `expectedSeq` 불일치 시 `reason: "stale"`. 보류 중이면 스냅샷된 큐 복원 후 `session-hold { op: "release" }` 기록, `session_resumed` 방출, `{ resumed: true, pauseSeq, ... }`. | `pieces/03`, `micro/impl-04`, `test-02` |
| `move_session` | `{ op: "prepare"\|"export"\|"import"\|"status"\|"finish"\|"cancel", moveId, sessionPath?, bundleDir?, cwd?, from?, answer? }` | `crates/omp-session/src/move_bundle.rs` + `crates/omp-cli/src/move_cmd.rs`. `SessionIndex` 바이트 슬라이스 무손실 추출, SHA-256 매니페스트 검증, `session_move_export`/`import` 확장 이벤트 호출, 원자적 스테이징 및 롤백. | `pieces/04`, `micro/impl-05`, `test-03` |
| `release_session` | `{ sessionId, reason?: "move"\|"handover", moveId? }` | `SessionWriter::sync` 및 `close()`로 파일 디스크립터 해제, `session_released` 엔트리 기록, 세션 단위 확장 컨텍스트 분리, `{ released: true, sessionId, session_path }`. | `pieces/05`, `micro/impl-06`, `test-03` |
| `extension_request` | `{ sessionId?, name, data? }` | `background: JoinSet`에서 비동기 실행(직렬 워커 블로킹 방지). `ExtHost::request_extension(name, data, 10s)` 호출. 미등록 시 `unknown_extension_request`, 타임아웃 시 `extension_timeout`. | `pieces/06`, `micro/impl-07`, `test-04` |

### 4.2 범주 2 — CLI 실행 플래그 5종 (`crates/omp-cli/src/cli.rs`, `main.rs`)

1. `--multi-session` (`VALUELESS_FLAGS`) & `--session-runtime <in-process|worker>` (`STRING_FLAGS`):
   - `omo-nyang/src/relay/move-host.ts:37`이 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 스폰한다.
   - `crates/omp-cli/src/host.rs`의 `SessionRegistry`가 단일 프로세스 안에서 `sessionId`별로 독립 `Runtime`·`SessionScope`(`tokio::task_local!`)를 관리하고 모든 응답·이벤트 프레임에 `sessionId`를 부착한다(`pieces/07`, `micro/impl-08`, `rev-03`).
2. `--no-model-fallback` & `--no-recommended-models` (`VALUELESS_FLAGS`):
   - `omo-nyang/src/model-routing.ts:658`이 전달한다.
   - `crates/omp-providers/src/fallback.rs:31` `RetryConfig`에 `pub model_fallback: bool`을 추가해 `retry.modelFallback` 설정과 연결하고, `--no-model-fallback` 지정 시 `TurnRecovery::consult`(`crates/omp-cli/src/recovery.rs:552`)가 항상 빈 체인을 반환하게 하여 백오프 재시도(`retry.enabled`)는 유지하되 모델 자동 변경만 차단한다. RPC `set_model`·`cycle_model`은 정상 허용하되 전환 후에도 폴백 차단 핀(`MODEL_FALLBACK_DISABLED`)을 유지한다(`pieces/08`, `micro/impl-08`, `test-06`).
3. `--skill <경로>` (`STRING_FLAGS`, 반복 가능):
   - `omo-nyang/src/gateway.ts:1167`이 `--skill ${paths.root}/skills`를 전달한다.
   - `Args::skill_paths: Vec<PathBuf>`에 누적하여 스킬 디스커버리 루트로 추가한다(`pieces/09`, `micro/inv-01`, `impl-08`).

### 4.3 범주 3 — 확장 API (`ext-host/host.ts`, `crates/omp-ext/`)

1. `api.rpc.handle(name, handler)` 및 `api.events.on("wake_source_state", ...)`:
   - `omo-nyang/src/session-move-extension.ts:130-140`의 `registerMoveReadiness`가 호출한다.
   - `ext-host/host.ts:340` `makeApi`에 `rpc: { handle(name, fn) }`를 추가하고 `LoadResult`(`crates/omp-ext/src/host.rs:127`)에 `rpc_requests: Vec<String>`를 실어 코어에 광고한다.
   - 백그라운드 잡(`jobs.rs`), 모니터(`monitor`), 비동기 서브에이전트(`omp-task`)의 활성 카운트가 바뀔 때마다 확장 버스로 `wake_source_state { source, activeCount }`를 발행한다(`pieces/10`, `micro/impl-09`, `rev-04`).
2. `session_move_export` · `session_move_import` 라이프사이클 이벤트:
   - `omo-nyang/src/session-move-extension.ts:116-117`이 구독한다.
   - `crates/omp-ext/src/lifecycle.rs`에 `SessionMoveExport { dir: String }` 및 `SessionMoveImport { dir: String }`를 추가하고, `ext-host/host.ts`에서 핸들러 호출 시 `AbortController`의 `signal`을 `event.signal`로 주입하며 `ctx.sessionManager.getSessionFile()`이 해당 세션의 `.jsonl` 절대경로를 반환하도록 보장한다(`pieces/11`, `micro/impl-09`, `test-05`).

### 4.4 범주 4 — OmOMeow 에이전트 도구 10종 (`crates/omp-cli/src/tools/`, `crates/omp-task`, `crates/omp-memory`)

1. 비동기 하위 작업 (`task` 확장 + `task_send`, `task_output`, `task_cancel`):
   - `crates/omp-task/src/registry.rs`에 `SubagentRegistry`를 두어 `run_in_background: true` 시 `st_<12hex>` ID를 즉시 반환하고 자식 RPC 파이프를 유지한다.
   - `task_send`는 실행 중인 자식에게 `steer` 프레임을 보내고, 정상 종료된 상주 자식은 같은 세션 파일로 재기동해 이어간다.
   - `task_output`은 `mode: "status" | "tail" | "full"` 비블로킹 스냅샷을 제공한다.
   - `task_cancel`은 프로세스 그룹(`-pgid`)을 종료하고 상태를 `Cancelled`로 확정한다. 비정상 종료(`Failed`/`Lost`)된 자식이 취소되지 않고 남아 있으면 부모 세션의 완료를 차단한다(`pieces/12`, `micro/impl-10`, `test-07`, `qa-06`).
2. 백그라운드 셸 제어 (`bash_output`, `kill_bash`, `monitor`):
   - Phase 2에서 도입된 `JobRegistry`(`crates/omp-cli/src/tools/jobs.rs:125`)를 확장해 각 백그라운드 셸 세션에 `bash_id`(`bg_<N>`)와 읽기 오프셋 커서, 프로세스 그룹 PGID를 부여한다.
   - `bash` 도구 입력에서 기존 `async: true` 외에 `run_in_background: true`를 정식 지원한다.
   - `bash_output({ bash_id, filter?, view? })`은 마지막 읽기 이후의 증분 로그 또는 가상 터미널 화면 스냅샷을 비블로킹으로 반환한다.
   - `kill_bash({ bash_id?, all? })`는 `-pgid` 시그널(`SIGTERM` → 유예 후 `SIGKILL`)로 자식 프로세스 트리를 고아 없이 정리한다.
   - `monitor({ description, command? | path? | target?, filter?, event?, timeout_ms?, persistent? })`는 `tokio::process::Command`(줄 단위 정규식 매칭), `notify` 파일시스템 워처(`create`/`modify`), 또는 서브에이전트 RPC 상태 변경을 비폴링으로 감시하고 조건 달성 시 `Runtime`의 `FollowUpTx`로 `<system-reminder>[monitor_event]`를 주입해 턴을 깨운다(`pieces/13`, `micro/impl-11`, `test-08`, `qa-07`, `rev-05`).
3. 장기 기억 (`memory`, `memory_apply_patch` 및 `crates/omp-memory`):
   - Phase 3.3의 `crates/omp-memory`에 Git 버전 관리 마크다운(`YAML frontmatter: description, read_only, expires`) 저장소 엔진을 구현한다.
   - `memory` 도구는 `create`, `str_replace`, `insert`, `delete`, `rename`, `update_description` 6개 명령을 수행하고 `read_only: "true"` 보호 및 경로 이탈 차단 후 바인딩된 에이전트 신원으로 즉시 Git 커밋한다.
   - `memory_apply_patch`는 다중 파일/헌크 패치를 원자적으로 검증·적용·커밋한다.
   - 세션 시작 및 매 턴 프롬프트 조립 시 `system/*.md`를 `<memory>` 블록으로, 그 외 디렉터리는 `<external_projection>` 인덱스로 투영한다(`pieces/14`, `micro/impl-12`, `test-09`, `qa-08`, `rev-06`).
4. 키 기반 작업 큐 (`workpool`):
   - `crates/omp-cli/src/tools/workpool.rs`에 `op: "create" | "push" | "close" | "inspect" | "cancel" | "yield"` 엔진 소유 키 작업 큐를 구현한다(`pieces/15`, `micro/impl-13`, `test-10`).
5. 질문·목표 호환 (`ask_user_question`, `create_goal`, `get_goal`, `update_goal`):
   - `ask.rs`와 `goal.rs` 위에 OmOMeow 표준 스키마 래퍼 도구를 상시 등록한다(`pieces/16`, `micro/impl-13`, `inv-03`, `test-10`).

### 4.5 범주 5 — 하네스·운영 계층 (`~/.omo` 프로필, `agent_settled`, 세션 JSONL 호환, `ultrawork`, Phase 3·4·5)

1. `~/.omo` 설정 루트 및 `omo.jsonc` 프로필 로더 (`crates/omp-settings`):
   - 설정 탐색 우선순위를 `$OMP_AGENT_DIR` → `~/.omp/agent` → `~/.omo/agent` 폴백으로 확장하고, `~/.omo/omo.jsonc`의 `profiles.<name>`(`fleet-small`, `fleet-medium` 등 장비 메모리별 동시 실행 수 및 카테고리별 모델 매핑)을 파싱해 `task.maxConcurrency`와 카테고리 모델 라우팅에 반영한다(`pieces/17`, `micro/impl-15`, `inv-07`).
2. `agent_settled` RPC stdout 발행 (`crates/omp-cli/src/runtime.rs:1641`, `ext.rs:1598`):
   - `runtime.rs:1630`에서 `p.active && drained && p.open.is_empty()`로 세션이 정착할 때 `{"type": "session_settled"}` 직후 `{"type": "agent_settled"}`를 RPC stdout으로 내보낸다(`pieces/18`, `micro/impl-14`, `inv-04`, `test-11`).
3. 세션 JSONL v3 ↔ 게이트웨이/senpi 호환 (`crates/omp-session`, `crates/omp-cli/src/persist.rs`):
   - `--session /abs/path.jsonl` 인자가 아직 존재하지 않는 파일일 때 해당 경로에 즉시 새 v3 세션을 생성·바인딩하고, 이기종 엔진 간 `move_session`을 위한 `senpi_compat.rs` 정규화기를 제공한다(`pieces/19`, `micro/inv-05`, `test-11`).
4. OmO 하네스(`ultrawork`·`ulw-loop`·내장 스킬 묶음) 및 미머지 Phase 3·4·5 통합:
   - 프롬프트에 `ultrawork`/`ulw` 키워드 감지 시 바인딩 지시문을 주입하고 `--skill`로 전달된 스킬 루트를 `<available_skills>`에 로드한다(`pieces/20`, `pieces/23`, `micro/impl-15`, `inv-06`).

---

## 5. 단계별 구현 로드맵 (Wave G1 ~ G4, 12개 독립 머지 가능 PR)

`pieces/26-review-tools-harness-roadmap.md` 및 `micro/doc-06-wbs-atomic-pr-breakdown.md`에서 확정한 12개 PR 분할 계획이다.

| Wave | PR 번호 및 제목 | 대상 크레이트 / 파일 | 포함 도메인·원자 조각 | 게이트 검증 명령 |
|---|---|---|---|---|
| **Wave G1**<br>기동·신호·보류 기반 | **PR-G1**: RPC `errorCode` 이중 직렬화 및 `get_protocol_info` | `crates/omp-cli/src/rpc.rs` | `pieces/02`, `impl-03`, `rev-01`, `test-02` | `cargo test -p omp-cli rpc::protocol_info` |
| **Wave G1** | **PR-G2**: 게이트웨이 기동 CLI 플래그 5종(`--skill`, `--no-model-fallback`, `--no-recommended-models`, `--multi-session`, `--session-runtime`) | `crates/omp-cli/src/cli.rs`, `recovery.rs`, `crates/omp-providers/src/fallback.rs` | `pieces/07..09`, `impl-08`, `inv-01`, `test-06`, `qa-01`, `qa-04` | `cargo test -p omp-cli cli::` |
| **Wave G1** | **PR-G3**: `agent_settled` RPC stdout 정식 발행 및 `--session <신규경로>` 바인딩 | `crates/omp-cli/src/runtime.rs`, `ext.rs`, `persist.rs` | `pieces/18..19`, `impl-14`, `inv-04..05`, `test-11`, `qa-09` | `cargo test -p omp-cli runtime::settled` |
| **Wave G1** | **PR-G4**: `set_client_info`, `interrupt`(`stop`/`abort`), `HoldLedger`(`pauseSeq`), `reload`, `resume_session` | `crates/omp-cli/src/hold.rs`, `runtime.rs`, `rpc.rs` | `pieces/01..03`, `impl-01..02`, `impl-04`, `test-01..02`, `qa-02` | `cargo test -p omp-cli rpc::lane_hold` |
| **Wave G2**<br>확장·셸·호환 도구 | **PR-G5**: 확장 호스트 `api.rpc.handle`, `wake_source_state`, `extension_request` 비동기 브리지 | `ext-host/host.ts`, `crates/omp-ext/src/host.rs`, `crates/omp-cli/src/rpc.rs` | `pieces/06`, `pieces/10`, `impl-07`, `impl-09`, `rev-04`, `test-04`, `qa-05` | `cargo test -p omp-ext && cargo test -p omp-cli rpc::ext_req` |
| **Wave G2** | **PR-G6**: 백그라운드 셸 `bash_output`, `kill_bash`(-pgid), 비폴링 `monitor`(`FollowUpTx`) | `crates/omp-cli/src/tools/{bash.rs, jobs.rs, monitor.rs}` | `pieces/13`, `impl-11`, `rev-05`, `test-08`, `qa-07` | `cargo test -p omp-cli tools::monitor` |
| **Wave G2** | **PR-G7**: `ask_user_question` 및 `create_goal`·`get_goal`·`update_goal` 호환 래퍼 | `crates/omp-cli/src/tools/{ask.rs, goal.rs}` | `pieces/16`, `impl-13`, `inv-03`, `test-10` | `cargo test -p omp-cli tools::ask_goal_compat` |
| **Wave G3**<br>태스크·큐·기억 | **PR-G8**: 비동기 `SubagentRegistry`(`st_...`) 및 `task_send`·`task_output`·`task_cancel` | `crates/omp-task/src/registry.rs`, `crates/omp-cli/src/tools/task.rs`, `rpc.rs` | `pieces/12`, `impl-10`, `inv-02`, `test-07`, `qa-06` | `cargo test -p omp-task && cargo test -p omp-cli tools::task` |
| **Wave G3** | **PR-G9**: 엔진 소유 키 작업 큐 `workpool` 도구 | `crates/omp-cli/src/tools/workpool.rs` | `pieces/15`, `impl-13`, `test-10` | `cargo test -p omp-cli tools::workpool` |
| **Wave G3** | **PR-G10**: `crates/omp-memory`(Git 마크다운 블록 + SQLite 하이브리드) 및 `memory`·`memory_apply_patch` (Phase 3.3 통합) | `crates/omp-memory`, `crates/omp-cli/src/tools/memory.rs` | `pieces/14`, `pieces/23`, `impl-12`, `rev-06`, `test-09`, `qa-08` | `cargo test -p omp-memory` |
| **Wave G4**<br>다중세션·이동·하네스 | **PR-G11**: `--multi-session --session-runtime in-process` `SessionScope` 격리, `release_session`, `move_session`(6 op), `session_move_export`/`import` | `crates/omp-cli/src/{host.rs, move_cmd.rs}`, `crates/omp-session/src/move_bundle.rs` | `pieces/04..05`, `pieces/07`, `pieces/11`, `impl-05..06`, `rev-02..03`, `test-03`, `test-05`, `qa-03` | `cargo test -p omp-session move_bundle && cargo test -p omp-cli move_cmd` |
| **Wave G4** | **PR-G12**: `~/.omo`(`omo.jsonc` 프로필) 로더, `ultrawork` 하네스 훅, RPC 적합성 골든 스위트 및 카나리 런북 | `crates/omp-settings`, `crates/omp-cli/src/sysprompt.rs`, `tests-e2e/` | `pieces/17`, `pieces/20..22`, `impl-15`, `test-12`, `qa-10`, `doc-01..07` | `just ci` |

---

## 6. 전체 86개 설계 조각 색인 (26 도메인 조각 + 60 초세분화 원자 조각)

### 6.1 도메인 설계·조사·리뷰 조각 (26개, `docs/design/omomeow-gap/pieces/`)

1. `pieces/01-rpc-interrupt-client-info.md` — RPC `interrupt` 와 `set_client_info` 명령 추가 설계
2. `pieces/02-rpc-protocol-info-reload.md` — RPC `get_protocol_info` 와 `reload` 명령 추가 설계
3. `pieces/03-rpc-resume-session.md` — RPC `resume_session` 명령 추가 설계
4. `pieces/04-rpc-move-session-core.md` — RPC `move_session` (장비 간 세션 이동) 핵심 설계
5. `pieces/05-rpc-release-session.md` — RPC `release_session` 명령 추가 설계
6. `pieces/06-rpc-extension-request.md` — RPC `extension_request` 명령 추가 설계
7. `pieces/07-flag-multi-session-runtime.md` — CLI 플래그 `--multi-session`·`--session-runtime in-process` 설계
8. `pieces/08-flag-model-pinning.md` — CLI 플래그 `--no-model-fallback`·`--no-recommended-models` 설계
9. `pieces/09-flag-skill-alias.md` — CLI 플래그 `--skill <경로>` 설계
10. `pieces/10-ext-api-rpc-handle.md` — 확장 API `api.rpc.handle` 설계
11. `pieces/11-ext-events-session-move.md` — 확장 이벤트 `session_move_export`·`session_move_import` 설계
12. `pieces/12-tools-task-send-output-cancel.md` — 에이전트 도구 `task_send`·`task_output`·`task_cancel` 설계
13. `pieces/13-tools-bash-output-kill-monitor.md` — 에이전트 도구 `bash_output`·`kill_bash`·`monitor` 설계
14. `pieces/14-tools-memory.md` — 에이전트 도구 `memory`·`memory_apply_patch` 와 기억 저장소 설계
15. `pieces/15-tools-workpool.md` — 에이전트 도구 `workpool` (키 기반 작업 큐) 설계
16. `pieces/16-tools-ask-goal-compat.md` — 도구 `ask_user_question`·`create_goal` 입력 호환 설계
17. `pieces/17-ops-omo-config-profile.md` — 설정 루트 `~/.omo` 프로필 체계 지원 설계
18. `pieces/18-ops-agent-settled.md` — 종료 신호 `agent_settled` 의미 정합 설계
19. `pieces/19-ops-session-format-compat.md` — 세션 파일 형식 호환 (`omp JSONL v3` ↔ 게이트웨이 읽기) 설계
20. `pieces/20-ops-ultrawork-harness.md` — OmO 하네스 기능(`ultrawork`·`ulw-loop`·스킬 묶음) 이식 설계
21. `pieces/21-test-rpc-conformance.md` — RPC 적합성 테스트 하네스 설계
22. `pieces/22-qa-gateway-e2e.md` — 게이트웨이 연동 7대 축 QA 시나리오 설계
23. `pieces/23-ops-tau-unmerged-phases-3-4-5.md` — 미머지 tau 계획 단계(3·4·5단계) 연계 설계
24. `pieces/24-inv-confirm-needed-audit.md` — 슬랙 `1795326973.474349` "확인 필요" 5개 항목 코드 대조 실측 조사
25. `pieces/25-review-rpc-ext-flags-consistency.md` — RPC 8종·실행 플래그·확장 API 교차 정합성 리뷰 및 상태 기계 통합 설계
26. `pieces/26-review-tools-harness-roadmap.md` — 에이전트 도구 10종·하네스 운영 5종 교차 리뷰 및 Wave 1~4 구현 로드맵 설계

### 6.2 초세분화 원자 조각 (60개, `docs/design/omomeow-gap/micro/`)

- **구현 변경 조각 (15개)**:
  - `micro/impl-01-rpc-set-client-info.md`
  - `micro/impl-02-rpc-interrupt-pause-fsm.md`
  - `micro/impl-03-rpc-get-protocol-info.md`
  - `micro/impl-04-rpc-reload-resume-session.md`
  - `micro/impl-05-rpc-move-session-ops.md`
  - `micro/impl-06-rpc-release-session.md`
  - `micro/impl-07-rpc-extension-request-bridge.md`
  - `micro/impl-08-cli-flags-multi-model-skill.md`
  - `micro/impl-09-ext-host-rpc-handle-and-events.md`
  - `micro/impl-10-tools-subagent-async-lifecycle.md`
  - `micro/impl-11-tools-bash-session-and-monitor.md`
  - `micro/impl-12-tools-memory-git-repo.md`
  - `micro/impl-13-tools-workpool-and-ask-goal-alias.md`
  - `micro/impl-14-ops-agent-settled-wire-frame.md`
  - `micro/impl-15-ops-omo-profile-and-ultrawork.md`
- **단위·통합·골든 테스트 조각 (12개)**:
  - `micro/test-01-unit-set-client-info-interrupt.md`
  - `micro/test-02-unit-protocol-info-reload-resume.md`
  - `micro/test-03-unit-move-and-release-session.md`
  - `micro/test-04-unit-extension-request-rpc-handle.md`
  - `micro/test-05-unit-session-move-ext-events.md`
  - `micro/test-06-unit-cli-flags-parser.md`
  - `micro/test-07-unit-task-send-output-cancel.md`
  - `micro/test-08-unit-bash-output-kill-monitor.md`
  - `micro/test-09-unit-memory-and-apply-patch.md`
  - `micro/test-10-unit-workpool-and-ask-goal.md`
  - `micro/test-11-unit-agent-settled-and-jsonl.md`
  - `micro/test-12-golden-rpc-conformance-suite.md`
- **E2E QA 시나리오 조각 (10개)**:
  - `micro/qa-01-gateway-boot-and-handshake.md`
  - `micro/qa-02-lane-hold-pause-reload-resume.md`
  - `micro/qa-03-cross-machine-session-move.md`
  - `micro/qa-04-model-pinning-and-recovery.md`
  - `micro/qa-05-extension-readiness-and-feedback.md`
  - `micro/qa-06-subagent-swarm-and-dead-child-gate.md`
  - `micro/qa-07-background-shell-and-monitor-wake.md`
  - `micro/qa-08-memory-persistence-and-projection.md`
  - `micro/qa-09-agent-settled-and-session-survey.md`
  - `micro/qa-10-canary-worker-rollout-and-rollback.md`
- **아키텍처·보안·동시성 리뷰 조각 (8개)**:
  - `micro/rev-01-rpc-wire-error-code-audit.md`
  - `micro/rev-02-pause-and-move-concurrency-audit.md`
  - `micro/rev-03-multi-session-isolation-audit.md`
  - `micro/rev-04-ext-host-deadlock-and-timeout-audit.md`
  - `micro/rev-05-process-tree-and-sandbox-audit.md`
  - `micro/rev-06-memory-git-safety-and-readonly-audit.md`
  - `micro/rev-07-tau-unique-assets-preservation-audit.md`
  - `micro/rev-08-glibc-musl-ci-portability-audit.md`
- **코드 대조 조사 조각 (8개)**:
  - `micro/inv-01-skill-flag-vs-skills-filter.md`
  - `micro/inv-02-task-tool-contract-diff.md`
  - `micro/inv-03-ask-and-goal-schema-diff.md`
  - `micro/inv-04-agent-settled-vs-agent-end.md`
  - `micro/inv-05-omp-jsonl-v3-vs-senpi-reader.md`
  - `micro/inv-06-tau-phase3-4-5-overlap.md`
  - `micro/inv-07-omo-vs-omp-config-and-profiles.md`
  - `micro/inv-08-cost-and-effort-mhc-vs-tau.md`
- **문서·패리티 원장·ADR·런북 조각 (7개)**:
  - `micro/doc-01-parity-ledger-gateway-rpc.md`
  - `micro/doc-02-parity-ledger-agent-tools.md`
  - `micro/doc-03-adr-multi-session-and-move.md`
  - `micro/doc-04-adr-agent-settled-and-pause-fsm.md`
  - `micro/doc-05-migration-plan-phase-gateway.md`
  - `micro/doc-06-wbs-atomic-pr-breakdown.md`
  - `micro/doc-07-operator-canary-runbook.md`
