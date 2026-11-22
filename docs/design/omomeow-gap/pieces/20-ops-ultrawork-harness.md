# OmO 하네스 기능(ultrawork·ulw-loop·스킬 묶음) 이식 설계

## 1. 현재 상태 (근거)

tau 저장소 HEAD `25af44a`, 워크스페이스 version 0.12.0, 바이너리 `omt`(`crates/omp-cli/Cargo.toml:6-8`), 멤버는 `crates/*` 전량(`Cargo.toml:3-16`). 작업 트리에는 이번 플릿 실행이 만든 미추적 `.omo/`만 있고 코드 변경은 없다.

하네스 계층에서 tau가 이미 가진 것:

- 시스템 프롬프트 조립은 `crates/omp-cli/src/sysprompt.rs` 한 곳이다. `build`(`sysprompt.rs:1052`) → `build_with`(`sysprompt.rs:888`)가 템플릿 `prompts/system-prompt.md`(13115바이트)와 `system-prompt-compact.md`, `custom-system-prompt.md`, `project-prompt.md`, `user-append.md`를 Handlebars 부분집합 인터프리터로 렌더한다(`sysprompt.rs:1-14`). 옵션 구조체는 `Options`(`sysprompt.rs:840-855`)이고 `generated_append: Vec<String>`(`sysprompt.rs:851`)이 `compose_append`(`sysprompt.rs:868`)로 합쳐진다. 실제 호출부는 `main.rs:384-402`이고 `generated_append`는 항상 빈 벡터다(`main.rs:391`).
- 프롬프트 안의 스킬 섹션은 `system-prompt.md:25-34`의 `<skills>` 블록이며 "Matching skill → MUST read `skill://<name>` first"로만 지시한다. 렌더 데이터는 `skill_values`(`sysprompt.rs:968-972`)와 `hasSkillUriAccess`(`sysprompt.rs:942-943`)다.
- 스킬 디스커버리는 `load_skills`(`sysprompt.rs:314`)가 `cwd` 조상의 `.omp/skills`를 가까운 순으로 훑고(`sysprompt.rs:320-322`) `~/.omp/agent/skills`(`sysprompt.rs:28-29`, `:323`)를 더한다. 엔트리는 `Skill { name, description, hide }`(`sysprompt.rs:249-253`)뿐이고 본문은 프롬프트에 들어가지 않는다. 같은 로직이 `omp-urls`에 중복 구현되어 있다(`crates/omp-urls/src/schemes/skill.rs:16-25`, `:28`) — `skill://` 해석은 `skill_target_path`(`skill.rs:75` 부근)가 담당하고 `SkillEntry`는 `crates/omp-urls/src/schemes/mod.rs:34-44`다.
- `ultra*` 계열은 이미 부분적으로 있다. 매직 키워드 테이블 `KEYWORDS`가 `ultrathink`, `orchestrate`, `workflowz`, `jevify`, `ultracode` 5종을 정의하고(`crates/omp-tui/src/magic.rs:33-66`), 판정은 `present`(`magic.rs:101`)·`enabled`(`magic.rs:70`)다. 초월 모드 열거형 `Ultra`(`magic.rs:363`)는 `ultracode/ultradeep/ultralong/hypercode`(`crates/omp-cli/src/runtime.rs:333-337`)이고 세션 상태로 유지된다(`runtime.rs:3521` `set_ultra`).
- 주입 경로는 숨은 custom 메시지다. `notices::build`(`crates/omp-cli/src/notices.rs:138`)가 `{customType: "<id>-notice", display: false}` 메시지를 만들고(`notices.rs:120` 부근 `message`), `prompt_notices`(`runtime.rs:1746`)가 프롬프트 진입 때마다 이를 붙인다(호출 `runtime.rs:1681`, `:1730`; 본문 `runtime.rs:1769-1770`). `ultracode`는 모드가 켜져 있으면 매 프롬프트마다 다시 나가고(`notices.rs:97` `ultracode_notice`, `notices.rs:160-164`), 본문은 `prompts/notices/ultracode-notice.md`(103줄)이며 `workpool()` 사용을 지시한다(`ultracode-notice.md:2`, `:19`).
- 확장 이벤트 표면은 `crates/omp-ext/src/lifecycle.rs`의 `LifecycleEvent`다. `BeforeAgentStart { prompt, images, system_prompt }`(`lifecycle.rs:91-97`), `Input`(`lifecycle.rs:99`), `SessionCompact`(`lifecycle.rs:159`), `SessionStop`, `SessionShutdown`이 있다. 결과 타입 `BeforeAgentStartOutcome { messages, system_prompt }`(`lifecycle.rs:323`)로 시스템 프롬프트 교체와 메시지 주입이 모두 가능하고, 실제 적용은 `Extensions::before_agent_start`(`crates/omp-cli/src/ext.rs:837-880`)가 `agent.set_system_prompt`로, 입력 게이트는 `input_gate`(`ext.rs:886`)가 담당한다. 배선은 `attach`(`ext.rs:913-930`) → `Runtime::set_before_start_hook`(`runtime.rs:2625`)이다.
- 하네스가 쓸 도구는 `todo`(`crates/omp-cli/src/tools/todo.rs:25`, 스키마 `:784`), 숨은 `goal`(`tools/goal.rs:1-30`), `task` 배치(`tools/task.rs:83-117`), `eval`의 `workpool`(`tools/eval_agents.rs:335`, 항목마다 새 에이전트 `:16-19`)가 있다.
- 이미 있는 모드 슬래시 명령은 `/plan`(`crates/omp-cli/src/features/plan.rs:1-12`, 372줄), `/loop`(`features/loop_mode.rs:1-14`, 345줄), `/skills`·`/marketplace`(`features/skills.rs:1-30`, 2064줄), `/ultra`(`slash_extra.rs:395`)이며 디스패치는 `slash_extra.rs:472/482/495`다.

tau에 없는 것(이 조각의 핵심):

- `ultrawork`/`ulw`/`ulw-loop` 문자열이 `*.rs`/`*.md`/`*.toml` 전체에서 0건이다(저장소 전역 grep 결과 없음). 즉 ultrawork 지시 주입 엔진, 계약 리비전, 재무장 상태가 전부 없다.
- `.omo` 경로를 참조하는 Rust 코드가 0건이다(전역 grep 없음). ulw-loop 상태 디렉터리, boulder 상태, 플랜 파일 규약이 없다.
- `agent_settled`는 확장 이벤트로만 emit되고(`crates/omp-cli/src/ext.rs:1598` 부근), 종료 계약은 `session_stop`(`ext.rs:1432` `session_stop_continuation`) + `agent_end`(`lifecycle.rs:106` 부근)다.
- RPC 디스패치는 `rpc.rs`의 단일 `match`이고 미지원 명령은 `Unknown command`로 떨어진다(`crates/omp-cli/src/rpc.rs:1094`). `interrupt`, `set_client_info`, `get_protocol_info`, `reload`, `resume_session`, `release_session`, `extension_request`, `move_session` 8종이 없다.
- CLI 플래그 `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`가 없다(`crates/omp-cli/src/cli.rs:97` `STRING_FLAGS`, `:138` `OPTIONAL_FLAGS`, `:142` `VALUELESS_FLAGS` 전수 확인).
- 도구 `task_send`/`task_output`/`task_cancel`, `bash_output`/`kill_bash`/`monitor`, `memory`/`memory_apply_patch`, `skill`이 없다. 셸 백그라운드는 `bash` 도구의 `async`/`name`/`ready` 인자와 `tools/jobs.rs`로만 존재한다.
- 설정 루트가 `~/.omp/agent/config.yml` + `<cwd>/.omp/config.yml`(`crates/omp-settings/src/lib.rs:7-10`)이라 플릿의 `~/.omo`/`omo.jsonc`와 다르다.
- 세션 파일은 omp JSONL v3(`crates/omp-session`)이고 `session_move_export`/`session_move_import` 이벤트가 없다(전역 grep 결과 없음, `vendor-src`의 SQL 테스트 문자열만 히트).

## 2. 게이트웨이가 요구하는 계약

mhc 기준 계약 중 하네스 조각이 직접 걸리는 항목만 정리한다.

1. 지시 주입: 프롬프트에 `ultrawork`/`ulw`가 있으면 세션당 1회 전체 계약을, 재언급에는 짧은 리마인더를, 컴팩션 뒤 첫 언급에는 계약을 다시 넣는다. mhc 구현은 `crates/maho-ultrawork/src/classify.rs:9-37`의 `Stage{Initial, Reminder, CompactRearm, None}`·`Route`·`Suppression`과 `engine.rs:55` `on_input`, `:107` `on_turn_start`, `:121` `on_compaction`, `:129` `on_tool_result`다. 상태는 `state.rs:8` `SessionArming`, `:31` `Sessions`, `:58` `mark_armed`, `:74` `mark_compact_rearm`, 계약 리비전은 본문 FNV-1a 해시(`maho-ultrawork/src/lib.rs:50`), 주입 텍스트는 `texts.rs:6-10`(`DIRECTIVE_OPEN_TAG`, `DIRECTIVE_MESSAGE_TYPE = "omo-ultrawork:directive"`, `REMINDER_MESSAGE_TYPE`)이다. 끄는 스위치는 런타임 플래그 `ultrawork-disabled`(`maho-ultrawork/src/extension.rs:14`).
2. 상태 파일: `.omo/ulw-loop`(`crates/omo/ulw-loop-core/src/constants.rs:3`) 아래 `brief.md`, `goals.json`, `ledger.jsonl`, `.state.lock`(`constants.rs:4-7`). 세션 스코프 없이는 CLI가 거부하고(`ULW_LOOP_SESSION_SCOPE_REQUIRED`), 증거는 트리 해시에 묶인다. 플랜 체크리스트는 `.omo/boulder.json`(`crates/omo/boulder-state/src/records.rs:75` `BoulderState`, `types.rs:148` `active_plan`)이다.
3. 스킬 묶음: 세션 시작 시 `<available_skills>` 목록을 프롬프트에 넣고, 필요할 때 `SKILL.md`를 읽어 워크플로를 수행한다. mhc는 번들 루트(`crates/maho-skills/src/root.rs:13`, `OMO_BUNDLED_SKILLS_DIR`)와 `disabled_skills` 거부목록(`crates/maho-skills/src/lib.rs:32`)으로 이를 확장 계층에서 처리하고, `ulw-plan`/`ulw-loop` 같은 스킬 이름이 프롬프트에 나오면 포인터 메시지를 주입한다(`crates/omo/components/maho-omo-skill-pointers/src/index.rs:8-12`, `SKILL_COMMAND_PREFIX = "/skill:"`).
4. 확인 필요: 게이트웨이가 실제로 ultrawork를 스스로 판정하는지, 아니면 에이전트 프롬프트에 맡기는지. mhc는 에이전트 측 엔진이 판정하므로 게이트웨이는 관여하지 않는다. tau 이식 시에도 같은 위치(에이전트 측)를 권한다.

## 3. 설계

원칙: 새 크레이트 하나(`crates/omp-harness`)와 기존 세 지점만 건드린다. 프롬프트 조립, 스킬 디스커버리, 확장 이벤트는 이미 있으므로 재작성하지 않는다.

### 3.1 ultrawork 주입 엔진

- `crates/omp-harness/src/ultrawork/{mod,classify,state,texts,engine}.rs` 신설. `texts.rs`는 `include_str!`로 `crates/omp-harness/contract/core.md`를 넣고 `DIRECTIVE_MESSAGE_TYPE = "omo-ultrawork:directive"`, `REMINDER_MESSAGE_TYPE = "omo-ultrawork:reminder"`를 노출한다(대응: `maho-ultrawork/src/texts.rs:6-10`).
- `state.rs`: `SessionArming { armed_revision: Option<u64>, compact_rearm: bool }`, 프로세스 전역 `Sessions`(세션 id → 상태). 세션 id는 이미 `Runtime::session_id`(`runtime.rs:1921`)가 있으므로 그대로 키로 쓴다.
- `classify.rs`: 입력은 `(text, source, snapshot, disabled)` 4필드로 축소한다. `Route`/`Suppression`은 mhc의 9종(`classify.rs:22-37`)을 그대로 옮기되 `SkillExpansion`/`SkillArguments`는 tau에 `/skill:` 명령이 없으므로 1차에서 `None`으로 둔다.
- `engine.rs`: `on_input`, `on_turn_start`, `on_compaction`, `on_tool_result(todo)` 4개 진입점만 둔다. 마스킹(코드블록·인용 안의 키워드 무시)은 mhc `mask.rs`(282줄)+`mask_phrases.rs`(156줄)가 가장 큰 이식 부품이므로 1차에서는 `omp-tui::magic::present`(`magic.rs:101`)의 단어 경계 판정만 재사용하고 마스킹은 후속 커밋으로 분리한다.
- 배선은 두 갈래다. (a) 프롬프트 주입: `notices::build`(`notices.rs:138`) 옆에 `harness::ultrawork::messages(...)`를 추가하고 `prompt_notices`(`runtime.rs:1746`)에서 합류시킨다 — 기존 `display: false` custom 메시지 규약(`notices.rs:120`)을 그대로 쓰므로 세션 영속·TUI 렌더가 자동으로 맞는다. (b) 컴팩션 재무장: `SessionCompact` 이벤트는 이미 emit되므로(`ext.rs:558`) `CompactBridge`(`ext.rs:511` 부근)에서 `mark_compact_rearm`을 호출한다. 확장 호스트(Bun)를 거치지 않고 Rust 안에서 끝낼 수 있는 것이 tau의 이점이다.

### 3.2 ulw-loop 상태

- 새 파일 포맷을 만들지 않는다. mhc의 `goals.json`/`ledger.jsonl`/`.state.lock`(`ulw-loop-core/src/constants.rs:3-7`)은 9457줄 크레이트의 절반이 스키마 검증·조정 로직이므로, tau에는 **읽기 전용 미러**만 둔다. 즉 `crates/omp-harness/src/ulw/{state,brief}.rs`가 `.omo/ulw-loop/<session>/`의 `brief.md`·`goals.json`·`ledger.jsonl`을 serde로 파싱해 진행률과 다음 행동만 계산하고, 쓰기는 하지 않는다. 쓰기 주체는 mhc CLI 그대로 둔다.
- 진행 노출은 기존 두 도구로 접는다. 목표는 숨은 `goal` 도구(`tools/goal.rs`)의 `create/get/complete/drop`을 그대로 쓰고, 원자 단계는 `todo`(`tools/todo.rs:25`)에 미러한다. mhc가 `mhc ulw-loop` CLI로 하는 일을 tau는 `goal`+`todo`+파일 미러로 대체하는 셈이다.
- 플랜 영속은 `.omo/plans/<slug>.md` 경로만 규약으로 고정하고, 체크박스 파싱은 `boulder-state`의 `top_level_task`(`crates/omo/boulder-state/src/top_level_task.rs:6`) 규칙을 참고하되 코드는 새로 쓰지 않는다.

### 3.3 내장 스킬 묶음

- tau의 스킬 디스커버리(`sysprompt.rs:314-325`, `omp-urls` 중복 `skill.rs:16-28`)는 "디렉터리에 SKILL.md가 있으면 목록에 넣는다"이므로, 번들 스킬은 **디렉터리만 배포하면 된다**. 코드 변경은 우선순위 한 줄이다: 현재 순서가 프로젝트 → 에이전트이고(`sysprompt.rs:320-323`), 번들 루트를 마지막에 붙이면 사용자 스킬이 항상 이긴다. 번들 루트는 `include_dir`로 바이너리에 넣고 첫 실행 시 `~/.omp/agent/skills/`로 풀어 쓰는 방식(권장)과, `OMP_BUNDLED_SKILLS` 환경변수로 경로만 주입하는 방식 중 전자를 택한다 — 후자는 게이트웨이가 환경을 통제해야 한다.
- 거부목록은 `omp-settings`에 `skills.disabled` 배열 키를 추가해 `load_skills`(`sysprompt.rs:314`) 끝에서 `retain`한다(`skills.enabled`가 이미 같은 자리에서 처리된다, `sysprompt.rs:65-67`).
- 스킬 본문 로딩은 이미 `skill://`가 있으므로(`omp-urls/src/schemes/skill.rs`) 새 도구를 만들지 않는다. 프롬프트 문구(`system-prompt.md:29`)가 "MUST read `skill://<name>` first"로 이미 계약을 말한다.

### 3.4 비이식 선택지와 비교

| 선택지 | 비용 | 얻는 것 | 판단 |
|---|---|---|---|
| A. mhc `maho-ultrawork` 크레이트를 그대로 vendoring | 1418줄 + `maho_ext_api` 의존 | 검증된 마스킹·분류 파리티 | 기각. 확장 API 타입이 tau의 `omp-ext`와 다르고, `maho_ext_api::CustomMessage` 의존이 `notices.rs` 규약과 겹친다 |
| B. `ulw-loop-core` 전량 이식 | 9457줄, 파일 60여 개 | 완전한 CLI 파리티 | 기각. SUL-1.0 라이선스이고, tau에는 이미 `goal`+`todo`가 있어 쓰기 상태 기계가 중복된다 |
| C. 하네스 전부 미이식, 게이트웨이가 프롬프트로만 지시 | 0줄 | 즉시 동작 | 조건부. ultrawork는 이 방식으로도 대략 동작하지만 컴팩션 뒤 재무장과 계약 리비전 추적이 불가능하다 |
| D. (권장) `omp-harness` 신설 + 읽기 전용 ulw 미러 + 번들 스킬 디렉터리 | 약 1200줄 신규, 기존 3지점 수정 | 재무장·리비전·세션 스코프 확보, mhc CLI와 상태 호환 | 채택 |

D를 택하는 이유는 셋이다. 첫째, tau에 이미 `display:false` custom 메시지 주입 경로가 완성돼 있어 ultrawork의 절반이 공짜다(`notices.rs:120`, `runtime.rs:1746`). 둘째, `SessionCompact`가 이미 Rust 쪽에서 emit되므로(`ext.rs:558`) 확장 호스트 없이 재무장이 가능하다. 셋째, ulw-loop 쓰기 상태를 mhc에 남기면 두 에이전트가 같은 `.omo`를 공유해도 충돌면이 읽기로만 좁혀진다.

## 4. 구현 단계 (커밋 단위)

1. `feat(harness): add omp-harness crate skeleton with ultrawork contract text` — `crates/omp-harness/{Cargo.toml,src/lib.rs,contract/core.md}`, 워크스페이스 멤버 자동 포함(`Cargo.toml:3-16`의 `crates/*`). 계약 본문은 mhc `maho-ultrawork/contract/core.md`를 한국어 지시문 없이 그대로 복사하고 출처 주석을 단다.
2. `feat(harness): classify ultrawork mentions and track per-session arming` — `classify.rs`, `state.rs`, `texts.rs`. 단위 테스트 포함, 배선 없음.
3. `feat(notices): inject ultrawork directive through prompt notices` — `notices.rs:138` 옆 합류 + `runtime.rs:1746`. 이 커밋에서 처음 사용자 표면에 나타난다.
4. `feat(harness): re-arm the contract after compaction` — `ext.rs` `CompactBridge`에서 `mark_compact_rearm`, `runtime.rs`의 컴팩션 완료 지점에서 `on_compaction`.
5. `feat(harness): mirror .omo/ulw-loop progress read-only` — `ulw/state.rs`, `goal`/`todo` 연동. 쓰기 없음.
6. `feat(skills): ship bundled skill root and disabled list` — 번들 압축 해제, `sysprompt.rs:314-325` 순서 조정, `omp-settings`에 `skills.disabled`.
7. `test(harness): cover keyword routing, rearm and bundled skill discovery` — 통합 테스트.

각 커밋은 `crates/omp-harness` 단독 또는 한 파일 수정으로 끝나야 하며, 3번과 6번만 기존 동작을 바꾼다.

## 5. 테스트

- `classify` 순수 함수: `ultrawork`/`ulw`/`mass ulw-loop` 검출, `ulw\u{d574}` 같은 비ASCII 경계, `mulw`·`method` 오검출 차단. mhc `keywords.rs:20-24`(`CHAIN_WORDS`), `:34-56`(`before`/`after` 경계)의 케이스를 그대로 벡터로 옮긴다.
- 상태 기계: `Initial → Reminder → CompactRearm → Reminder` 전이, 계약 리비전 변경 시 재무장. `state.rs` 단위 테스트로 충분하고 시간 의존이 없어야 한다.
- 주입 파리티: `notices::build` 결과가 `customType == "omo-ultrawork:directive"`, `display == false`인지 확인. 기존 `notices.rs` 테스트 패턴(`notices.rs:180-200` 부근 `custom_type`/`text` 헬퍼)을 재사용한다.
- 스킬 디스커버리: `sysprompt.rs:1113-1135`의 기존 `skills_respect_config_and_hide` 테스트에 번들 루트·거부목록 케이스를 추가한다.
- 마이그레이션 없음: 기존 프롬프트·세션 포맷을 바꾸지 않으므로 회귀 범위는 `cargo test -p omp-cli -p omp-harness`와 `cargo test -p omp-urls`다.

## 6. QA 시나리오

1. `omt --mode rpc`로 띄우고 `prompt`에 "ulw-loop로 이 저장소 리팩터링 계획 세워줘"를 보낸다. 기대: 첫 응답 컨텍스트에 `omo-ultrawork:directive` custom 메시지 1건, 이후 같은 세션 재언급에는 `omo-ultrawork:reminder` 1건.
2. 같은 세션에서 `/compact` 후 다시 `ulw`를 언급한다. 기대: 리마인더가 아니라 전체 계약이 다시 나간다.
3. `magicKeywords.ultracode: false`를 `~/.omp/agent/config.yml`에 넣고 재실행한다. 기대: ultrawork 주입만 남고 ultracode notice는 사라진다(기존 `notices.rs:148-152` 경로).
4. `.omo/ulw-loop/<session>/{brief.md,goals.json,ledger.jsonl}`을 손으로 놓고 세션을 시작한다. 기대: 진행률이 프롬프트 또는 상태 출력에 반영되고, tau가 그 파일을 **수정하지 않는다**(mtime 비교).
5. 번들 스킬 확인: 새 홈에서 `omt` 첫 실행 → `~/.omp/agent/skills/`에 `ultrawork`, `ulw-loop` 디렉터리가 생기고 `get_state` 또는 프롬프트의 `<skills>` 목록에 이름과 description이 나온다.
6. 사용자 스킬 우선: 같은 이름의 스킬을 `<cwd>/.omp/skills/ultrawork/`에 두면 프롬프트에 사용자 description이 실린다.
7. 게이트웨이 관점 회귀: `agent_settled`를 기다리는 게이트웨이가 여전히 타임아웃하는지 확인한다 — 이 조각은 종료 계약을 바꾸지 않으므로 **여전히 깨진다**. 별도 조각 담당.

## 7. 위험과 미확인

- 마스킹 부재 위험: 1차에서 코드블록·인용 안의 `ulw`를 걸러내지 않으면 계약이 과다 주입된다. mhc `mask.rs`(282줄) 이식은 후속 커밋으로 미뤘고, 그때까지 오탐률은 측정되지 않았다.
- 프롬프트 길이: 계약 본문은 mhc 기준 약 60줄이다. `ultracode-notice.md`가 103줄인 점을 감안하면 컴팩션 트리거가 앞당겨질 수 있다. `runtime.rs:1999-2033`의 `prompt_overhead_chars` 기반 계산에 실제 영향을 재야 한다(확인 필요).
- 설정 루트 불일치: tau는 `~/.omp`(`omp-settings/src/lib.rs:7-10`), 플릿은 `~/.omo`. 번들 스킬 해제 위치와 `skills.disabled` 키를 어느 루트에서 읽을지 미정. 게이트웨이가 `HOME`을 격리하면 문제 없지만 그렇지 않으면 두 루트를 모두 봐야 한다(확인 필요).
- 세션 파일 이중화: ulw-loop 미러는 세션 id를 키로 쓰는데 tau 세션 id(`runtime.rs:1921`)와 mhc의 `senpi:<session_id>` 규약(`ulw-execute/SKILL.md:69`)이 다르다. 매핑 규칙 미정.
- 라이선스: mhc `ulw-loop-core`는 SUL-1.0이다(`ulw-loop-core/src/lib.rs:3`). 이 조각은 해당 크레이트 코드를 복사하지 않고 스키마만 참조하지만, `maho-ultrawork`의 계약 텍스트를 그대로 가져오는 1번 커밋은 라이선스 검토가 필요하다(확인 필요).
- 확인 못 한 것: (a) 게이트웨이가 ultrawork 판정을 에이전트에 위임하는지, (b) `agent_settled` 요구가 하네스 주입과 상호작용하는지, (c) tau `todo` 도구가 `goal`과 동시 활성일 때 우선순위, (d) `ext-host/host.ts:686-698`의 `before_agent_start`/`input` 집계가 다중 확장에서 시스템 프롬프트를 마지막 하나로 덮는지 — 코드상 마지막 핸들러 승리로 보이나(`host.ts:686-697`) 실행 검증은 하지 않았다.
- 이 조각이 다루지 않은 것: RPC 8종 부재, CLI 플래그 4종 부재, `task_send`/`task_output`/`task_cancel`·`bash_output`/`kill_bash`/`monitor`·`memory` 도구 부재, `api.rpc.handle`, `session_move_export/import`, `agent_settled` 종료 신호. 하네스 주입은 이들이 없어도 동작하지만, 게이트웨이 계약 전체를 만족하려면 별도 조각이 필요하다.
