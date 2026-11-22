# tau 설정 루트(~/.omo) 및 장비별 프로필(omo.jsonc) 지원 설계

## 1. 현재 상태 (근거)

tau의 설정 루트는 `~/.omp` 하나로 고정되어 있고, 프로필 개념은 CLI 파싱까지만 존재한다.

- 레이어 정의: `crates/omp-settings/src/lib.rs:1-13` — TS 기본값 → 전역 `~/.omp/agent/config.yml`(또는 `OMP_PROFILE` 프로필 agent dir) → 프로젝트 `<cwd>/.omp/config.yml`, `config.yaml` → env.
- 전역 경로 해석: `crates/omp-settings/src/settings.rs:461-471`의 `agent_dir()`가 `HOME/.omp`를 만들고 `OMP_PROFILE`이 있으면 `~/.omp/profiles/<name>/agent`가 `is_dir()`일 때만 그쪽을 쓴다. `crates/omp-settings/src/settings.rs:473-483`의 `agent_config_path()`는 그 디렉터리의 `config.yml` → `config.yaml` 순으로 고른다.
- 로더 진입점: `crates/omp-settings/src/settings.rs:108-113`의 `Settings::load(cwd)`가 `agent_config_path()`와 `cwd.join(".omp")`를 `load_from`에 넘긴다. `load_from`(`settings.rs:115-190`)이 4계층을 `deep_merge`로 합치고, `null`은 상위 레이어의 "해제"로 처리한다(`settings.rs:543-557`).
- 출처 추적: `Provenance`는 `Default|Global|Project|Env` 4종뿐이다(`settings.rs:75-80`, 판정은 `settings.rs:228-247`). 프로필 계층을 넣으면 이 enum을 확장해야 한다.
- 쓰기 경로: `write_global`(`settings.rs:407-457`)이 `agent_config_path()`에 YAML을 되쓴다. 즉 쓰기 대상도 `~/.omp`에 고정.
- 포맷: `crates/omp-settings/Cargo.toml:11`에 `serde_yaml_ng`만 있고 JSONC 파서 의존성은 없다. 저장소 전체에서 `jsonc` 문자열은 코드 하이라이트 언어 id(`crates/omp-tui/src/chat/code.rs:243`)뿐이다. mhc 쪽에는 자체 파서가 있다(`crates/maho-support/src/jsonc.rs:42 parse_jsonc_lenient`, 584줄).
- 프로필 CLI: `Args`에 `profile`/`alias` 필드는 있으나(`crates/omp-cli/src/cli.rs:55-56`) `STRING_FLAGS`에 등록만 되어 있고(`cli.rs:97-134`), `parse_from`(`cli.rs:331`)은 값을 담기만 한다. 실행 경로는 `crates/omp-cli/src/main.rs:303-310`에서 `--alias`는 `--profile`/`OMP_PROFILE`을 요구하고, `--profile`은 `"omt: --profile is not supported (no profile subsystem)"`로 즉시 실패한다. 테스트도 파싱만 검증한다(`cli.rs:897-905`).
- `.omp` 하드코딩 분포(워크스페이스 19개 파일): `crates/omp-cli/src/sysprompt.rs:23-30`, `crates/omp-cli/src/features/skills.rs:62-74`, `crates/omp-mcp/src/config.rs:95-99`, `crates/omp-stats/src/paths/mod.rs:16-29`, `crates/omp-collab/src/registry.rs:305-313`, `crates/omp-cli/src/ext.rs:89-91`, `crates/omp-providers/src/lib.rs:104-113`(`OMP_AGENT_DIR` 우선), `crates/omp-task/src/discovery.rs:86-95`, `crates/omp-cli/src/persist.rs:63-67`(`~/.omp-rs/sessions`).
- 동시성 설정: tau에는 `task.maxConcurrency`만 있고(`crates/omp-settings/src/schema.rs:3241-3248`, 기본 `0`=auto), 소비는 `crates/omp-cli/src/tools/task.rs:47-50` → `crates/omp-task/src/runner.rs:231-248`(`min(available_parallelism, 8)`). `global_concurrency`에 해당하는 설정 id도, 전역 세마포어도 없다(grep 0건). `task.maxRecursionDepth`는 스키마에만 있고(`schema.rs:3257-3262`) 실제 게이트는 env만 읽는다(`crates/omp-task/src/runner.rs:157-180`, `OMP_TASK_MAX_RECURSION_DEPTH`).
- 이 머신 실측: `~/.omp` 없음. `~/.omo/omo.jsonc` 존재 — 최상위 키 `$schema, profiles, model_profile, model_profiles, models, categories, memory, agents, task, teams, git_master, _migrations, [native], telemetry, provenance`. `profiles.fleet-small.task = {default_concurrency:2, global_concurrency:2}`, `profiles.fleet-medium.task = {4, 6}`, 베이스 `task = {default_concurrency:20, global_concurrency:24, max_depth:2, wait:{min_ms:5000, default_ms:60000, max_ms:600000}, residency_max_children:24, provider_concurrency, model_concurrency}`. 파일 내 `//` 주석은 없고 `$schema` URL의 `//`만 있다.
- `~/.omo/agent`에는 `settings.json`(17키), `mcp.json`, `auth.json`, `models.json`, `state.json`이 있다. tau는 `settings.json`을 읽지 않는다(grep 0건).
- 같은 슬랙 답변에서 지적된 나머지 4개 범주도 코드로 재확인했다: RPC 디스패치(`crates/omp-cli/src/rpc.rs:595-962`)에 `interrupt`/`set_client_info`/`get_protocol_info`/`reload`/`resume_session`/`release_session`/`extension_request`/`move_session` 없음, CLI의 `--multi-session`/`--session-runtime`/`--no-model-fallback`/`--no-recommended-models` 전무(grep 0건), 확장 호스트의 `api.rpc.handle`/`session_move_export|import` 없음, 도구 `task_send`/`task_output`/`task_cancel`/`bash_output`/`kill_bash`/`monitor`/`memory`/`workpool` 없음, 종료 신호는 `agent_settled`가 핸들러 있을 때만 emit된다(`crates/omp-cli/src/ext.rs:1594-1601`). 이 조각은 그중 설정 루트/프로필만 설계하고, 나머지는 7절에서 이 설계와의 접점만 적는다.

## 2. 게이트웨이가 요구하는 계약

mhc 기준 계약이며, 설정과 직접 맞닿는 부분은 다음 넷이다.

- 루트: `~/.omo/omo.jsonc`(없으면 `omo.json`)를 사용자 레이어로 읽는다(`crates/maho-config/src/paths.rs:34 user_candidate`). 프로젝트 레이어는 `<dir>/.omo/omo.jsonc`를 cwd에서 위로 올라가며 먼 조상부터 적용한다(`paths.rs:72 project_file_in`, `paths.rs:92 project_paths_farthest_first`, `paths.rs:125 resolve_paths`).
- 프로필: `profiles.<name>` 본문을 베이스 뷰 위에 덮는다(`crates/maho-config/src/load.rs:217-240`). 이름 결정은 명시값 > `OMO_PROFILE` > `OCX_PROFILE` > `OPENCODE_CONFIG_DIR`의 `profiles/<name>` 꼬리(`crates/maho-config/src/env.rs:114-140`). 없는 프로필은 `DiagnosticKind::Profile`로 보고하고 베이스를 쓴다(`load.rs:221-235`).
- 제어 키: `profiles`와 `[senpi]`/`[native]` 같은 하네스 블록은 뷰에서 제거되고(`crates/maho-config/src/merge.rs:46 is_control_key`, `merge.rs:51`), 하네스 오버레이는 `[senpi]` 다음 `[native]` 순으로 덮는다(`merge.rs:64 native_overlay`). 스키마에서 `profiles`는 `record(profile_body)`(`crates/maho-config/src/schema.rs:744`), 프로필 본문은 `task`/`agents`/`models`/`[native]` 등을 갖는 strict object다(`crates/omo/omo-config-core/src/schema/config.rs:72-96`).
- 소비: `task.default_concurrency`(0=무제한, 기본 5)와 `global_concurrency`(기본 8)를 `TaskSettings::from_view`가 읽는다(`crates/maho-task/src/settings.rs:125-135`, `crates/omo/omo-config-core/src/schema/task.rs:40-42`).

## 3. 설계

### 3.1 루트 선택 (`crates/omp-settings/src/root.rs` 신설)

```
pub enum SettingsRoot { Omp(PathBuf), Omo(PathBuf) }
pub fn resolve_root(env: &dyn Fn(&str)->Option<String>, home: &Path) -> SettingsRoot
pub fn root_config_path(root: &SettingsRoot) -> PathBuf
pub fn agent_dir_of(root: &SettingsRoot) -> PathBuf
```

우선순위(위가 강함):
1. `TAU_CONFIG_ROOT=<절대경로>` — 루트 디렉터리 자체를 지정. fleet 프로비저닝/테스트용.
2. `--settings-root omp|omo|<path>` — `crates/omp-cli/src/cli.rs:97-134`의 `STRING_FLAGS`에 추가, `main.rs`에서 `Settings::set_root(...)`로 주입.
3. 마커 파일 존재: `~/.omo/omo.jsonc` 또는 `~/.omo/omo.json`이 정규 파일이면 `Omo(~/.omo)`.
4. 그 외 `Omp(~/.omp)`.

`~/.omp`만 있는 기존 사용자는 3번이 성립하지 않으므로 동작이 한 글자도 바뀌지 않는다. `~/.omo`와 `~/.omp`가 동시에 있는 장비는 `Omo`가 이기는데, 이는 `omo.jsonc`가 플릿 도구가 만든 마커이기 때문이다. 이 한 가지 반전만 의도된 변경이며 `--settings-root omp`로 즉시 되돌릴 수 있고, 선택된 루트와 이유를 기동 시 stderr 한 줄 + `get_state`의 `settingsRoot` 필드로 노출한다.

### 3.2 계층 (낮음 → 높음)

1. TS 기본값 — 기존 `defaults_layer()`(`settings.rs:485-501`) 그대로.
2. 루트 베이스 파일 — `Omp`면 `agent/config.yml|yaml`, `Omo`면 `omo.jsonc|omo.json`. 파싱은 `parse_layer_text(&str) -> Option<Value>` 하나로 통일하고 YAML을 먼저 시도한 뒤 실패하면 JSONC 스캐너를 쓴다.
3. 하네스 오버레이 — `[senpi]` 다음 `[native]`(mhc `merge.rs:64`와 동일 순서). tau는 자기 자신이 native이므로 `[native]`가 최종 승자.
4. 프로필 본문 — `profiles.<name>`을 3번과 같은 규칙(본문, 본문의 하네스 오버레이)으로 덮는다. 이름은 `--profile` > `OMO_PROFILE` > `OMP_PROFILE`(기존 tau env 유지) > 없음. 없는 이름은 진단 1건 + 베이스 사용.
5. tau 전용 오버레이 — `Omo` 모드에서만 `<root>/agent/omp/config.yml`. `/modelpreset`, 설정 패널 저장이 여기로 간다(플릿 파일 불가침).
6. 프로젝트 — `Omo` 모드에서는 조상별 `<dir>/.omo/omo.jsonc`(먼 조상 먼저) 다음 `<cwd>/.omp/config.yml|yaml`. `Omp` 모드는 기존과 동일.
7. env 오버라이드 — 기존 `parse_env_value`(`settings.rs:690-740` 부근) 유지.

`profiles`와 하네스 블록은 병합 후 뷰에서 제거한다(현재 `unknown_keys()`가 이들을 미지 키로 보고하므로 `collect_unknown` 호출 전에 제어 키 집합을 걸러야 한다).

### 3.3 omo.jsonc → tau 설정 id 매핑 (`crates/omp-settings/src/omo.rs` 신설)

| omo.jsonc | tau id | 비고 |
| --- | --- | --- |
| `task.default_concurrency` | `task.maxConcurrency` | 0 의미 충돌: omo 0=무제한, tau 0=auto. fleet 값은 2/4라 실사용 영향 없음. 0은 tau auto로 매핑하고 진단 1건 |
| `task.global_concurrency` | `task.globalConcurrency` (신규 SettingDef, 기본 0) | 전역 세마포어 구현 필요(3.4) |
| `task.max_depth` | `task.maxRecursionDepth` | 게이트가 env만 읽으므로 배선 필요(3.4) |
| `task.wait.{min_ms,default_ms,max_ms}` | `task.wait.minMs/defaultMs/maxMs` (신규 3종) | 현재 tau에 대응 id 없음 |
| `[native]`, `[senpi]` | 하네스 오버레이 | 3.2 3번 |
| `model_profile(s)`, `models`, `agents`, `categories`, `teams`, `memory`, `telemetry`, `provenance`, `git_master`, `_migrations` | 매핑하지 않음 | 모델/도구 조각의 몫. 단 "알려진 omo 네임스페이스"로 등록해 `unknown_keys()` 소음에서 제외 |

`omo.rs`는 `pub fn translate(base: &Map<String,Value>) -> Map<String,Value>`와 `pub const OMO_NAMESPACES: &[&str]`를 제공하고, 병합 2단계 직후에 적용한다.

### 3.4 소비 배선

- 전역 동시성: `TaskRunner`(`crates/omp-task/src/runner.rs:212-260`)에 `Arc<Semaphore>` 전역 permit을 추가하고 `resolve_global_concurrency(Option<f64>) -> usize`(0 또는 비유한 = 무제한)를 둔다. 호출부는 `crates/omp-cli/src/tools/task.rs:483`의 `TaskRunner::new(binary, effective_max_concurrency(...))` 자리에 전역 상한을 함께 넘긴다.
- 재귀 깊이: `crates/omp-task/src/runner.rs:175-180 max_recursion_depth()`가 env 우선, 없으면 `OnceLock<usize>`(기동 시 `Settings`에서 1회 주입)를 읽도록 바꾼다. 그러지 않으면 `profiles.*.task.max_depth`가 조용히 무시된다.
- 쓰기: `write_global`(`settings.rs:407`)은 `Omp` 모드에서 기존 동작, `Omo` 모드에서는 `<root>/agent/omp/config.yml`에 쓰고 `omo.jsonc`는 절대 열지 않는다. omo 스키마가 strict object(`omo-config-core/src/schema/config.rs:72-96`)이므로 tau 전용 키를 끼워 넣으면 mhc 검증에서 거부된다.
- 나머지 `.omp` 하드코딩 19곳은 `omp_settings::root()`를 호출하도록 치환하되, 1단계에서는 `sysprompt.rs:23-30`, `features/skills.rs:62-74`, `ext.rs:89-91`, `omp-providers`의 `AgentDir`(`lib.rs:104-113`), `omp-task/src/discovery.rs:86-95`만 바꾼다. `persist.rs:63-67`의 `~/.omp-rs/sessions`와 `omp-stats`는 세션 포맷 조각과 함께 다룬다.

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-settings): add SettingsRoot resolver` — `root.rs`(3.1), `Settings::load`가 `root::resolve_root` 사용, 기존 경로 테스트는 `load_from`으로 유지. 신규 테스트는 루트 선택 행렬만.
2. `feat(omp-settings): parse JSONC layers` — `parse_layer_text` + 최소 JSONC 스캐너(`//`, `/* */`, 후행 쉼표, 문자열 내부 무시). 의존성 추가 없음. mhc 구현은 SUL-1.0이라 복사하지 않는다.
3. `feat(omp-settings): merge omo profiles and harness blocks` — 3.2의 3·4단계, `Provenance`에 `Profile`·`OmoBase`·`TauOverlay` 추가, 제어 키 필터.
4. `feat(omp-settings): translate omo task settings` — `omo.rs`, 신규 SettingDef 4종(`task.globalConcurrency`, `task.wait.minMs|defaultMs|maxMs`).
5. `feat(omp-cli): accept --profile and --settings-root` — `cli.rs` 플래그 2종, `main.rs:309`의 bail 제거, `main.rs:303-307`의 alias 선행조건 유지, 기동 시 루트 진단 1줄, `get_state`에 `settingsRoot`/`profile`.
6. `feat(omp-task): honour globalConcurrency and settings recursion depth` — 3.4의 두 배선.
7. `feat(omp-settings): write tau overlay in omo mode` — `write_global` 분기, `/modelpreset` 경로 회귀 테스트.
8. `refactor: route agent-dir lookups through SettingsRoot` — 3.4의 5개 사이트 치환.

## 5. 테스트

- `crates/omp-settings/tests/root.rs`: 홈 픽스처 4종(둘 다 없음/`~/.omp`만/`~/.omo`만/둘 다) × (`TAU_CONFIG_ROOT`, `--settings-root`, 무지정) 선택 결과. 기존 `Settings::load_from` 테스트(`crates/omp-settings/tests/settings.rs:45,105,171,317`)는 그대로 통과해야 한다.
- `crates/omp-settings/tests/omo_profile.rs`: 베이스 `task.default_concurrency=20` + `profiles.fleet-small.task.default_concurrency=2`에서 `get_number("task.maxConcurrency")==2.0`, 프로필 없음일 때 `20.0`, 없는 프로필 이름일 때 베이스 + 진단 1건, `[native].task.maxConcurrency`가 프로필 본문보다 우선.
- `crates/omp-settings/tests/jsonc.rs`: 주석/후행 쉼표/문자열 내 `//`(예: `$schema` URL) 보존, 깨진 JSONC는 `None` + 파일 경로 포함 오류.
- `crates/omp-settings/tests/omo_write.rs`: `Omo` 모드에서 `write_global("colorBlindMode", true)` 후 `omo.jsonc` 바이트 불변, `<root>/agent/omp/config.yml` 생성, 재로드 시 값 반영.
- `crates/omp-cli/tests/profile_flag.rs`: `HOME` 픽스처에서 `omt --print --profile fleet-small "..."`가 exit 0이고 `get_state.settingsRoot == "omo"`.
- `crates/omp-task/src/runner.rs` 단위: `resolve_global_concurrency`(None/0/NaN → 무제한, 2 → 2), 전역 상한이 배치 전체 동시 실행 수를 제한.
- 회귀: `cargo test -p omp-settings -p omp-task -p omp-cli` 및 `cargo test -p omp-tui`(`Settings::load` 호출부 `crates/omp-tui/src/app.rs:426-432`).

## 6. QA 시나리오

1. 기존 사용자 보존: `HOME`에 `~/.omp/agent/config.yml`만 두고 개조 전후 `get_state`의 `settings` 블록이 동일함을 diff.
2. 플릿 장비: 실제 `~/.omo/omo.jsonc` + `OMO_PROFILE=fleet-small`로 기동, `task` 도구로 5건 동시 스폰 → 동시 실행 2건 상한 관측(로그 타임스탬프), `globalConcurrency` 2도 동시 적용.
3. 두 루트 공존: `~/.omp`와 `~/.omo` 동시 존재 시 omo 선택 + stderr 한 줄, `--settings-root omp`로 재기동하면 omp 값.
4. 프로필 오타: `--profile fleet-smal` → 베이스(20)로 동작하고 진단에 `profiles.fleet-smal`이 찍힘, 종료 코드 0.
5. 깨진 파일: `omo.jsonc`에 후행 쉼표 없는 닫힘 오류를 넣으면 해당 레이어만 건너뛰고 나머지 레이어로 기동.
6. 쓰기 격리: `Omo` 모드에서 `/modelpreset save qa` 실행 후 `omo.jsonc` mtime 불변, `~/.omo/agent/omp/config.yml`에만 기록.
7. 하네스 블록: `[native]`에 `task.maxConcurrency: 1`, 베이스에 20을 두고 실제 fan-out이 1인지 확인.
8. 세션/확장 접점: omo 루트에서 확장 디렉터리 탐색 결과를 로그로 확인(기대 경로는 7절 미확인 항목).

## 7. 위험과 미확인

- JSONC 파서 부재가 가장 큰 신규 코드다. tau에는 파서도 의존성도 없다(`crates/omp-settings/Cargo.toml:11`). 실측 `omo.jsonc`에는 주석이 없어 1단계에서는 "YAML 슈퍼셋 파싱으로 충분"할 가능성이 높지만, `serde_yaml_ng`가 이 파일을 실제로 파싱하는지는 실행해 보지 않았다 — 확인 필요.
- `Provenance`에 variant를 추가하면 exhaustive match 호출부가 깨진다. `provenance()` 호출 지점을 전수 조사하지 않았다 — 확인 필요.
- `task.globalConcurrency`는 tau에 구현 자체가 없다. `TaskRunner`에 전역 세마포어를 넣는 것이 이 조각에서 가장 침습적인 변경이며, 하위 태스크가 프로세스 외부에서 스폰될 때 상한이 실제로 성립하는지는 미검증.
- `task.maxRecursionDepth`가 스키마에만 있고 게이트는 env만 읽는다(`crates/omp-task/src/runner.rs:157-180`). 배선하지 않으면 프로필의 `max_depth`가 무효다.
- 0의 의미 충돌(omo 0=무제한, tau 0=auto). fleet 값은 2/4라 현재는 무해하지만 `default_concurrency: 0`을 쓰는 장비가 생기면 해석 차이가 난다.
- 확장 디렉터리: tau는 `~/.omp/agent/extensions`를 본다(`crates/omp-cli/src/ext.rs:89-91`). omo 루트에서 `~/.omo/agent/extensions`를 기대하는지, 플릿이 이미 그 경로에 확장을 두는지 확인하지 못했다 — 확인 필요.
- 게이트웨이가 엔진에 `OMO_PROFILE`을 실제로 주입하는지 확인하지 못했다(`crates/maho-gateway` 내 `OMO_PROFILE` grep 0건). 주입하지 않는다면 장비별 프로필은 `TAU_CONFIG_ROOT`나 래퍼 env로만 성립한다 — 확인 필요.
- `~/.omo/agent/settings.json`(17키)와 tau `config.yml`의 키 체계는 다르다. tau가 이 파일을 읽지 않으므로 `theme`, `defaultModel`, `retry` 같은 mhc 사용자 설정이 tau에서 무시된다. 별도 매핑 조각이 필요하다.
- omo `task.wait.*`, `residency_max_children`, `provider_concurrency`/`model_concurrency`, `team`, `dag`는 tau에 대응 구현이 없다. 매핑표에 넣지 않은 항목은 조용히 무시되며, 진단으로 "무시된 omo 키" 목록을 1회 출력하는 편이 낫다.
- 이 조각은 설정 루트/프로필만 다룬다. 같은 답변의 나머지 4범주(RPC 8종 부재 `crates/omp-cli/src/rpc.rs:595-962`, CLI 플래그 4종 부재, 확장 호스트 `api.rpc.handle`/`session_move_*` 부재, 도구 8종 부재)와 종료 신호(`agent_settled`가 핸들러 있을 때만 emit, `crates/omp-cli/src/ext.rs:1594-1601`)는 각 담당 조각에서 설계해야 하며, 이 문서는 그중 설정과 맞닿는 지점(`get_state`의 `settingsRoot`/`profile` 노출, 기동 진단 1줄)만 규정한다.
