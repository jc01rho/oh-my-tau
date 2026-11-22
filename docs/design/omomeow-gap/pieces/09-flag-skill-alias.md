# tau `--skill` 별칭 도입 설계 (조각 09-flag-skill-alias)

## 1. 현재 상태 (근거)

`--skills` 하나만 있고, 그 값은 파싱 이후 어디에서도 소비되지 않는다.

- 플래그 정의: `crates/omp-cli/src/cli.rs:97` `STRING_FLAGS`, 그 안 `cli.rs:129` `"--skills"`. 값 처리 `cli.rs:309-311` 은 `a.skills = value.split(',').map(|s| s.trim().to_string()).collect()` 로 **대입**이므로 두 번째 `--skills` 가 첫 번째를 덮어쓴다.
- 필드: `cli.rs:61` `pub no_skills: bool`, `cli.rs:62` `pub skills: Vec<String>`. `Args` 는 `cli.rs:20` `#[derive(Debug, Default)]`, `cli.rs:22` `pub struct Args`.
- 소비처 없음: `a.skills` 를 읽는 곳은 `cli.rs:310`(쓰기)과 `cli.rs:747`(테스트 단언)뿐이고 `no_skills` 는 `cli.rs:61`, `cli.rs:410`, `cli.rs:868` 뿐이다. `sysprompt::Options`(`crates/omp-cli/src/sysprompt.rs:836-851`)에 스킬 필드가 없고 `crates/omp-cli/src/main.rs:380-388` 의 조립에도 없다.
- 자기 신고: `main.rs:62-63` 이 `--skills/--no-skills` 를 "subsystems are not in omt (no-op or reported)" 목록에 넣고, `main.rs:50-51` HELP 에는 `[--no-skills]` 만 노출한다.
- 실제 로딩은 디스커버리뿐: `sysprompt.rs:314` `pub fn load_skills(agent, cwd, enabled)`, `:320-322` 프로젝트 `.omp/skills` 상향 탐색, `:323` `agent/skills`, `:324` `hide` 제거, `:315-317` 은 `!enabled` 면 즉시 빈 벡터. `load_skills_dir`(`sysprompt.rs:277-310`)는 디렉터리만 받고 `SKILL.md` 없는 항목은 `continue`. 프롬프트 주입은 `sysprompt.rs:941`, `:968`, `:989`(`"skills": skill_values`), `hasSkillUriAccess` 는 `:942-943`.
- `skill://` 해석도 디스커버리만: `crates/omp-urls/src/schemes/skill.rs:19` `discover_skills`, `skill.rs:252-261` `active_skills` 가 `omp_providers::AgentDir::default().0` 와 `ctx.cwd` 로 매번 새로 스캔한다. `AgentDir` 기본값은 `crates/omp-providers/src/lib.rs:106-113`. `UrlCtx` 는 `crates/omp-urls/src/lib.rs:25-33`, 생성은 `crates/omp-cli/src/tools/read.rs:567-578`.
- `--skill` 은 파서에 아예 없다: `crates/omp-cli/src/cli.rs` 전체에 `"--skill"` 문자열이 없고, 미지 플래그는 `cli.rs:424-425` 에서 `unrecognized` 로 모여 `finish()`(`cli.rs:456`)에서 `unknown flag` 로 실패한다.

다른 5개 범주의 현재 상태(이 조각과 맞닿는 부분만):

| 범주 | tau 상태 | 근거 |
|---|---|---|
| RPC 8종 | `dispatch()` 에 없음 → `Unknown command` | `crates/omp-cli/src/rpc.rs:593`, `:1094`. `interrupt`/`reload`/`resume_session`/`release_session`/`set_client_info`/`get_protocol_info`/`extension_request`/`move_session` grep 0건, 인접 항목은 `set_interrupt_mode`(`rpc.rs:954`)뿐 |
| CLI 플래그 | `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`, `--skill` 모두 0건 | `cli.rs:97-160` 세 테이블 |
| 확장 호스트 | `api.rpc.handle`, `session_move_export/import` 없음. `ExtHost`(`crates/omp-ext/src/host.rs:202`)·`emit_nowait`(`:604`)만 | 노출 표면은 `ext-host/host.ts:341` `makeApi` |
| 도구 | `task` 만 있고 `task_send/task_output/task_cancel` 없음. `bash_output/kill_bash/monitor` 없음, `memory*` 없음, `workpool` 은 eval 커널 op | `crates/omp-cli/src/tools.rs:382-387`, `:682-700`, `tools/eval_agents.rs:16-17`, `:330` |
| 설정 루트 | `~/.omp/agent` (플릿 기대는 `~/.omo`) | `omp-providers/src/lib.rs:106-113` |
| 종료 신호 | `agent_end` 송신 후 `agent_settled` 는 확장 핸들러가 있을 때만 emit | `crates/omp-cli/src/events.rs:721-737`, `crates/omp-cli/src/ext.rs:1589-1598` |
| 세션 형식 | omp JSONL v3 | `crates/omp-session/src/lib.rs:1` |

## 2. 게이트웨이가 요구하는 계약

계약 기준은 mhc(oh-my-openagent Rust 포트)다. `--skill` 계약은 네 가지다.

1. **반복 지정 누적**: `crates/maho-cli/src/cli/args.rs:375` `"--skill" => result.skills.push(value.to_owned())`. 문자열 플래그 테이블 등록은 `args.rs:315`, 문서는 `crates/maho-cli/src/cli/help.txt:67` "Load a skill file or directory (can be used multiple times)".
2. **경로는 디렉터리 또는 파일**: `crates/maho-core/src/skills.rs:604-623` 이 `Path::new(path).is_dir()` 로 분기해 디렉터리는 내부 로더, 아니면 단일 파일 로더를 쓴다.
3. **정규화 후 중복 제거, 이름 충돌은 진단**: `crates/maho-core/src/resource_loader.rs:312-329` `merge_paths` 가 `canonicalize_path` 로 dedupe. 파일 단위 중복은 `skills.rs:626-628`, 이름 충돌은 `skills.rs:629-636` `name "…" collision` 진단.
4. **`--no-skills` 는 디스커버리만 끄고 명시 경로는 살린다**: `resource_loader.rs:559-563` `if inputs.no_skills { cli_skills } else { [cli_skills, enabled_skills].concat() }`, `:602-604` `no_skills && skill_paths.is_empty()` 일 때만 빈 결과. 없는 경로는 실패가 아니라 진단(`:626-644` "Skill path does not exist"), 진단 출력은 `crates/maho-cli/src/cli/runtime.rs:917-927`. 경로는 cwd 기준 해석(`crates/maho-cli/src/cli/host_runtime.rs:259-264`, `:642`).

게이트웨이는 이 argv 를 그대로 넘기므로, tau 가 `unknown flag` 로 죽는 현재 동작이 곧 기동 실패다.

## 3. 설계

`--skills`(CSV) 의미는 건드리지 않고 `--skill`(경로, 반복)을 추가한다. 기존 `skills: Vec<String>` 는 TS 패리티 필드로 남기고 새 필드를 분리한다.

**3.1 파서** (`crates/omp-cli/src/cli.rs`)

- `Args` 에 `pub skill_paths: Vec<PathBuf>` 추가(`cli.rs:62` 인근). `PathBuf` 인 이유는 `--extension`(`cli.rs:302`)이 이미 그렇게 하고 경로 해석 지점을 한 곳으로 모으기 위해서다.
- `STRING_FLAGS`(`cli.rs:129`) 뒤에 `"--skill"` 추가. 그러면 `cli.rs:355-361` 의 `--skill=path` 분리와 `cli.rs:363-382` 의 다음 토큰 무조건 소비가 그대로 적용된다. 플래그처럼 보이는 토큰도 경로로 소비되는데 이는 `--extension`/`-e` 와 같은 기존 규약이다.
- `set_string`(`cli.rs:233`)에 `"--skill" => { if value.is_empty() { invalid.push("--skill requires a path.".to_string()) } else { a.skill_paths.push(PathBuf::from(value)) } }`. `--skills`(`cli.rs:309-311`)처럼 대입하지 않고 push 해서 반복 누적을 만든다.
- 빈 값 처리는 `cli.rs:379-381` 의 "missing value ⇒ `\"\"`" 규약과 어긋나지만, mhc 는 값이 없으면 플래그를 소비하지 않으므로(`args.rs:375` 의 `if let Some(value) = next` 아래) 빈 경로를 오류로 올리는 쪽이 계약에 가깝다. 이 편차는 주석으로 남긴다.
- 단형 별칭 `-s` 는 넣지 않는다. mhc 도 장형만 있고(`help.txt:67`), tau 의 단형 네임스페이스는 `-e`/`-r`/`-c`/`-p`/`-h`/`-v` 로 이미 좁다.

**3.2 로더** (`crates/omp-cli/src/sysprompt.rs`)

- `Options`(`sysprompt.rs:836-851`)에 `pub skill_paths: Vec<PathBuf>` 추가, `main.rs:380-388` 에서 `skill_paths: args.skill_paths.clone()` 로 채운다. 경로는 여기서 cwd 기준으로 절대화한다(`main.rs` 는 `sp_opts` 앞에서 `cwd` 를 이미 계산한다).
- `load_skills_dir`(`sysprompt.rs:277`) 옆에 `fn load_skill_path(path: &Path, skills: &mut Vec<Skill>)`: 디렉터리면 기존 `load_skills_dir` 로 위임, 파일이면 `parse_frontmatter`(`sysprompt.rs:256`)로 `name`(없으면 부모 디렉터리명) / `description` / `hide` 를 읽어 `Skill`(`sysprompt.rs:248-253`) 하나를 push. mhc 의 파일 분기(`skills.rs:604-623`)와 같은 모양이다.
- 시그니처를 `pub fn load_skills(agent: &Path, cwd: &Path, enabled: bool, explicit: &[PathBuf]) -> Vec<Skill>` 로 넓히고 조기 반환을 `if !enabled && explicit.is_empty()` 로 바꾼다(`sysprompt.rs:315-317`). 본문은 `explicit` 를 먼저, 디스커버리를 뒤에 쌓는다. mhc 의 `[cli_skills, enabled_skills].concat()`(`resource_loader.rs:559-563`)과 같은 순서다.
- 정규화 dedupe: `canonicalize()` 성공 시 그 경로, 실패 시 lexical 절대화 결과를 키로 첫 항목만 유지. `merge_paths`(`resource_loader.rs:312-329`)와 동등한 규칙이며, `Skill` 에 `file_path` 가 없으므로 이름이 아니라 경로로 dedupe 한다.
- 없는 경로: 파서에서 실패시키지 않고 로더에서 `eprintln!("Skill path does not exist: {}", p.display())` 한 줄 후 계속. mhc 가 진단 후 진행하는 것과 같고(`resource_loader.rs:634-644`), RPC 모드에서 세션이 죽지 않는다.
- 호출부 갱신: `sysprompt.rs:941`, `crates/omp-cli/src/agent.rs:96-114`(`system_prompt` → `build`), `crates/omp-cli/src/features/skills.rs:874`(목록 표시는 `&[]`).

**3.3 `--no-skills` 상호작용**

`enabled` 인자는 계속 `cfg.skills_enabled`(`sysprompt.rs:59`, `:65-66`)에서 오고, 호출부에서 `enabled = cfg.skills_enabled && !args.no_skills` 로 결합한다. `args.no_skills` 를 `Options` 에 넣지 않는 이유는 `PromptConfig`(`sysprompt.rs:35`)가 config.yml 전용이라 CLI 플래그를 섞으면 테스트가 설정 파일과 플래그를 구분할 수 없기 때문이다. 결과 의미: `--no-skills` 는 디스커버리만 끄고 `--skill` 경로는 그대로 로드한다. `--no-skills --skill X` 와 `--skill X` 는 같은 스킬 집합을 낳는다.

**3.4 `skill://` 정합** (`crates/omp-urls`)

`active_skills`(`skill.rs:252-261`)가 디스커버리만 하므로 프롬프트에는 보이는데 `skill://<name>` 은 404 가 되는 불일치가 생긴다. `crates/omp-urls/src/lib.rs:58-65` 의 `set_global_session_file`/`global_session_file` 과 같은 모양의 `set_global_skill_paths(Vec<PathBuf>)` / `global_skill_paths()` 를 추가하고, `skill.rs:252-261` 에서 디스커버리 앞에 전역 목록을 얹는다. 호출은 `main.rs:495` 의 세션 파일 등록 옆 한 번. 이 항목을 미루면 3.2 만으로 프롬프트 목록은 동작하지만 `skill://` 해석은 실패한다(7절).

## 4. 구현 단계 (커밋 단위)

1. **C1 파서**: `Args.skill_paths`, `STRING_FLAGS` 에 `"--skill"`, `set_string` arm, HELP(`main.rs:44-63`)에 `--skill` 추가 및 no-op 목록(`main.rs:62-63`)에서 `--skills/--no-skills` 제거. 테스트는 `cli.rs:561` 모듈에.
2. **C2 로더**: `load_skill_path`, `load_skills` 시그니처 확장, `Options.skill_paths`, `main.rs:380-388` 배선, `agent.rs:96`/`features/skills.rs:874` 호출부 정리, `--no-skills` 결합.
3. **C3 URL 계층**: `omp_urls::set_global_skill_paths`, `active_skills` 병합, `main.rs` 등록, `omp-urls` 단위 테스트.
4. **C4 문서·패리티**: `docs/` 플래그 표 갱신, HELP 의 "no-op" 문구 정정, `tests-e2e/` 에 CLI 수준 시나리오 1건.

각 커밋은 `cargo check -p omp-cli`(C3 는 `-p omp-urls` 포함)와 해당 크레이트 테스트로 닫는다.

## 5. 테스트

파서(`cli.rs:561` 모듈, 기존 `parse()` 헬퍼 `cli.rs:565`):

- `--skill a --skill b` → `skill_paths == [a, b]`, 순서 보존. 기존 `--skills` 테스트(`cli.rs:735-748`)와 별개로 둔다.
- `--skill=a` (등호 형식, `cli.rs:355-361`) → 1건.
- `--skill ""` → `invalid` 에 `--skill requires a path.`, 세션 미생성.
- `--skills git-* --skill a` → `skills == ["git-*"]` 유지, `skill_paths == [a]` (두 필드 독립).
- `--no-skills --skill a` → `no_skills == true`, `skill_paths == [a]`.

로더(`sysprompt.rs` 테스트 모듈, `skills_respect_config_and_hide`(`:1116`) 옆):

- 디렉터리 1건 + 단일 `.md` 1건을 `explicit` 로 주면 둘 다 목록에 오고, 파일 쪽 `name` 은 frontmatter 값, 없으면 부모 디렉터리명.
- `hide: true` 항목은 explicit 이어도 제외(`sysprompt.rs:324` 규칙 유지).
- 같은 디렉터리를 `a` 와 `a/` 로 두 번 주면 1건(정규화 dedupe).
- 없는 경로 1건 + 정상 경로 1건 → 정상 경로는 로드되고 panic 없음.
- `enabled=false, explicit=[]` → 빈 벡터(기존 `:1127` 단언 유지), `enabled=false, explicit=[X]` → X 1건.
- explicit 이 디스커버리보다 앞에 오는 순서 단언.

URL(`omp-urls`): `set_global_skill_paths` 후 `skill://<name>` resolve 성공, 전역 목록이 비면 기존 디스커버리만으로 동작. 전역 상태를 쓰므로 세션 파일 패턴(`lib.rs:58-65`)처럼 단일 락 안에서 설정·복원한다.

검증 명령: `cargo test -p omp-cli`, `cargo test -p omp-urls`, `cargo clippy -p omp-cli --all-targets -- -D warnings`. 본 문서 작성 시점에 이 명령들은 실행하지 않았다(코드 미변경).

## 6. QA 시나리오

1. 임시 디렉터리에 `sk/one/SKILL.md`(`name: one`)와 `two.md`(`name: two`)를 만들고 `omt --skill <sk/one> --skill <sk/two.md> --dump-system-prompt`(`main.rs:389-398`) 실행, `<skills>` 블록(`crates/omp-cli/prompts/system-prompt.md:28-34`)에 `one`, `two` 확인.
2. 같은 명령에 `--skill <sk/one>/` 를 한 번 더 붙여 출력에 중복이 없는지 확인.
3. `omt --no-skills --skill <sk/one> --dump-system-prompt` → `one` 만 나오고 `~/.omp/agent/skills` 의 다른 스킬은 사라졌는지 확인.
4. `omt --skill /nonexistent --mode rpc` 기동 후 stderr 에 `Skill path does not exist: /nonexistent` 한 줄, `negotiate_protocol`(`rpc.rs:595`) 응답 정상 확인.
5. `omt --mode rpc` 에서 `prompt` 를 보내 모델이 `skill://one` 을 read 로 여는지 확인(C3 반영 후).
6. 회귀: `omt --skills git-*` 가 여전히 파싱되는지(값 미소비, `unknown flag` 아님), `--extension`/`-e` 반복 의미(`cli.rs:302`)가 그대로인지.
7. 게이트웨이 측: OmOMeow 가 만드는 argv 를 그대로 넣어 기동되는지 확인(확인 필요 항목, 7절).

## 7. 위험과 미확인

- **TS omp 원본의 `--skill` 유무·의미 미확인**: `vendor/` 에 TS CLI 소스가 없어(`vendor/` 는 crossterm, ort 등만) 옮겨야 할 원본 계약을 직접 보지 못했다. mhc 계약만 근거로 설계했다. 확인 필요.
- **게이트웨이가 실제로 넘기는 argv 미확인**: mhc 는 `crates/omo/components/maho-omo-config-startup/src/metadata_launch.rs:39` 에서 `--skill` 을 쓰지만, OmOMeow 게이트웨이가 tau 용으로 같은 인자를 만드는지는 확인하지 못했다. 확인 필요.
- **`--skills` CSV 의 원래 의미 미확인**: 값이 소비되지 않아 "디스커버리 필터(glob)" 인지 "경로 목록" 인지 코드로 판정할 수 없다. 본 설계는 `--skills` 를 건드리지 않아 위험이 국소적이지만, 게이트웨이가 `--skills` 로 경로를 넘기면 별도 조치가 필요하다. 확인 필요.
- **이름 충돌 진단 부재**: tau `Skill`(`sysprompt.rs:248-253`)에 `file_path` 가 없어 mhc 의 `name "…" collision`(`skills.rs:629-636`)에 해당하는 진단을 낼 수 없다. C2 에서 `Skill` 에 `file_path` 를 더하지 않으면 충돌은 조용히 중복 노출된다. `Skill` 확장은 렌더(`:968`)와 `features/skills.rs:874` 두 곳만 건드리므로 비용은 작다.
- **`--no-skills` 단락 해석 차이**: mhc 는 `no_skills && skill_paths.is_empty()` 일 때만 빈 결과(`resource_loader.rs:602-604`)지만 tau 의 `enabled=false` 는 디스커버리까지 전부 끈다(`sysprompt.rs:315-317`). 본 설계는 후자에 explicit 예외만 두어 결과 집합은 mhc 와 같게 만들지만, `skills.enabled: false` 설정과 `--no-skills` 플래그를 같은 `enabled` 로 접는 구조는 유지된다. 설정으로 끈 경우에도 explicit 경로가 살아나는 것이 의도인지 확인 필요.
- **전역 상태로 인한 테스트 경쟁**: C3 의 `set_global_skill_paths` 는 `SESSION_FILE`(`omp-urls/src/lib.rs:7`, `:58-65`)과 같은 프로세스 전역이다. 병렬 테스트에서 세션 간 누출 위험이 있고 `read.rs:560-566` 의 `TEST_SESSION_FILE` 같은 우회가 필요할 수 있다.
- **경로 정규화 실패 케이스**: 없는 경로는 `canonicalize()` 가 실패하므로 lexical 절대화로만 dedupe 된다. `a/../a` 처럼 표기만 다른 존재 경로와 없는 경로가 섞이면 중복이 남을 수 있다. mhc 도 `canonicalize_path` 실패 시 같은 한계인지 확인 필요.
- **미착수**: 이 조각은 설계만이며 코드·테스트·문서를 수정하지 않았다. 저장소는 `e7a5378` 그대로다. 1절의 다른 범주 항목(RPC 8종, 나머지 CLI 플래그, 확장 호스트 `api.rpc.handle`, 도구 5종, 세션/종료 신호)은 조사 결과만 기록했고 각 조각의 설계는 이 문서 범위 밖이다.
