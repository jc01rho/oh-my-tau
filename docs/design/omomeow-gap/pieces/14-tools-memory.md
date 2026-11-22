# tau 기억 저장소(omp-memory) 설계: 마크다운 블록 저장소, 블록 연산, git 자동 커밋, 프롬프트 투영

## 1. 현재 상태 (근거)

- tau 워크스페이스에 기억 크레이트가 없다. `Cargo.toml` 멤버는 `crates/*` 이고 실제 크레이트는 `omp-auth-broker omp-cli omp-collab omp-compact omp-eval omp-ext omp-goose omp-judge omp-lsp omp-mcp omp-providers omp-sandbox omp-session omp-settings omp-sot omp-stats omp-task omp-tui omp-urls omp-wasm` 이며 `omp-memory` 는 없다.
- `/memory` 는 껍데기다. `crates/omp-cli/src/slash.rs:757` 의 `"memory"` arm 이 하위 명령을 받아 고정 문자열만 돌려준다: `view`/빈 인자 → `"Memory payload is empty."` (`slash.rs:760`), `clear|reset` → `"Memory cleared."`, `enqueue|rebuild` → `"Memory consolidation enqueued."`, `sync` → `"Memory consolidation ran."`, `queue|stats|diagnose` → `"Memory is not available for the off backend (/{verb})."` (`slash.rs:766`), `mm` → `"Mental-model maintenance via /memory mm is unsupported in ACP mode..."` (`slash.rs:770`). usage 문자열은 `slash.rs:773`.
- 명령 메타데이터는 실제 백엔드가 있는 것처럼 선언돼 있다. `crates/omp-cli/src/builtin_commands.json:653` 의 `memory` 항목과 하위 명령 `view/stats/diagnose/queue/sync/clear/reset/enqueue/rebuild`, `mm list`(`builtin_commands.json:696`)가 존재한다. 즉 표면은 있고 구현이 없다.
- 설정 스키마에는 백엔드 선택만 있다. `crates/omp-settings/src/schema.rs:1877` `memory.backend` 는 enum `off|local|hindsight|mnemopi|sharpshooter`, 기본값 `"off"`. 관련 키 `memories.summaryInjectionTokenLimit`(기본 5000), `memories.threadScanLimit`(기본 300)가 `schema.rs:1863`·`schema.rs:1870` 부근에 있고, `hindsight.*` 키군이 `schema.rs:1230`부터 이어진다. TUI 쪽 선택지 정의는 `crates/omp-tui/src/overlays/settings_defs.rs:1855`, 조건 평가는 `crates/omp-tui/src/overlays/settings.rs:734`.
- 설정 경로 해석기는 이미 프로필을 안다. `crates/omp-settings/src/settings.rs:461` 의 `agent_dir()` 는 `OMP_PROFILE` 이 있으면 `~/.omp/profiles/<profile>/agent`, 없으면 `~/.omp/agent` 를 돌려준다. `Settings::load` 는 `settings.rs:108`, `get_string`/`get_bool` 은 `settings.rs:177`·`settings.rs:183`.
- 시스템 프롬프트 조립 지점은 두 곳이다. `crates/omp-cli/src/sysprompt.rs:28` 의 `agent_dir()`(=`~/.omp/agent`), `sysprompt.rs:153` 의 `context_files()`, `sysprompt.rs:949` 에서 `files: Vec<Value>` 로 직렬화되어 `sysprompt.rs:984` 의 `"contextFiles": files` 로 템플릿 데이터에 들어간다. 렌더 결과는 `sysprompt.rs:855` `pub struct Built { pub blocks: Vec<String>, pub tool_names: Vec<String> }`.
- 템플릿은 이미 파일 주입 슬롯을 갖는다. `crates/omp-cli/prompts/project-prompt.md:7-15` 가 `{{#if contextFiles.length}}` → `<repo-rules>` → `{{#each contextFiles}}<file path="{{path}}">{{content}}</file>{{/each}}` 를 렌더한다. 기억 투영을 여기에 얹을 수 있다.
- 프롬프트 소비 지점: `crates/omp-cli/src/agent.rs:95` `system_prompt()` 가 `sysprompt::build(...).blocks.join("\n\n")`(`agent.rs:115`)로 합치고, `agent.rs:190` 에서 세션 프롬프트로 넘긴다. 재설정 경로는 `agent.set_system_prompt` (`crates/omp-cli/src/ext.rs:855`, `ext.rs:868`).
- frontmatter 파서가 두 벌 있고 둘 다 라인 단위 근사다. `crates/omp-cli/src/sysprompt.rs:256` 은 `:` 분할만 하는 단순 파서(skills용), `crates/omp-task/src/agent.rs:153` 은 `serde_yaml_ng` 로 진짜 YAML 파싱을 한다. 후자가 기억 블록에 맞는 선례다(`crates/omp-task/Cargo.toml` 의 `serde_yaml_ng = "0.10"`).
- 도구 등록은 단일 테이블이다. `crates/omp-cli/src/tools.rs:279` `builtin_tools_with_env_and_jobs`, `tools.rs:321` 의 `let tools = vec![...]`, 설정 게이트는 `tools.rs:420` `enabled_by_settings` + `tools.rs:473` `retain`. 도구 트레이트 구현 선례는 `tools.rs:219` `struct FnTool`, 상태를 가진 도구 선례는 `crates/omp-cli/src/tools/todo.rs:936` `pub fn tool()` + `todo.rs:942` `schema()`.
- 패치 파서는 이미 벤더에 있다. `vendor/pi-edit/src/modes/apply_patch.rs:208` `parse_apply_patch()`, 엔트리 타입은 `apply_patch.rs:54` `ApplyPatchEntry { path, op, rename, diff }`, `omp-cli` 는 `crates/omp-cli/Cargo.toml:47` 로 `pi-edit` 에 이미 의존한다.
- git 호출 선례: `crates/omp-cli/src/features/worktree.rs:123` `fn git(cwd, args)` 가 `Command::new("git").arg("-C").arg(cwd)` 로 서브프로세스를 돌리고 stdout 을 trim 한다. git2/gix 의존성은 워크스페이스에 없다.
- RPC 디스패치(`crates/omp-cli/src/rpc.rs:593`)의 관측 구간(`rpc.rs:595`-`rpc.rs:962`)에 기억 전용 명령은 없다. 전 구간 완전 열거는 확인 필요.
- 게이트웨이 기준 구현(mhc)의 기억 계약은 바이너리 문자열로 확인된다: 연산 `create/str_replace/insert/delete/rename/update_description`, 인자 `reason,file_path,file_text,description,old_string,new_string,insert_line,insert_text,old_path,new_path`, 별도 `memory_apply_patch(reason, input)` 가 `*** Begin Patch`/`*** Add File:`/`*** Update File:`/`*** Move to:`/`*** Delete File:`/`*** End Patch` 포맷을 받고, frontmatter 의 `description` 은 create 시 필수, `read_only: "true"` 는 수정 차단, `expires:` 는 투영 만료, 성공 응답은 `Memory <command> committed locally (<sha>).`, 커밋 author 는 `<identity>@omo.local`, 실패 문구는 `'reason' must be a non-empty string`, `str_replace: old_string was not found in the target memory block`, `Memory repo has uncommitted changes. ...`. 구현 위치 문자열은 `crates/omo/memory-core/src/tools/memory_apply_patch.rs`, `crates/omo/memory-core/src/memfs/frontmatter_entries.rs`, `crates/omo/memory-core/src/compile/stored_block.rs`.
- 실물 저장소 형태(이 머신): `/data/home/.omo/memory/agents/<agent-id>/repo` 가 git 저장소이고 `system/`, `reference/`, `skills/` 를 가지며, `system/persona.md` 는 `--- description: ... ---` frontmatter 로 시작한다. 시스템 파일만 프롬프트에 본문이 투영되고 비시스템 경로는 이름만 노출된다.

## 2. 게이트웨이가 요구하는 계약

- 도구 표면: 이름 `memory`, `memory_apply_patch` 두 개. `memory` 는 `command` enum 6종 + `reason`(비어 있으면 거부) + 경로/텍스트 인자, `memory_apply_patch` 는 `reason` + codex 패치 `input`.
- 경로 규칙: 상대 경로는 기억 저장소 루트 기준, 절대 경로는 저장소 내부일 때만 허용, 외부는 거부.
- 파일 규칙: `.md` + YAML frontmatter, create 시 `description` 필수, `read_only: "true"` 수정 차단, 수정·삭제 대상은 유효한 기억 파일이어야 함, frontmatter 없는 추가는 자동 생성.
- 커밋 규칙: 변경 경로만 stage, `reason` 을 커밋 메시지로 사용, author 는 바운드된 기억 아이덴티티, 원격이 설정된 경우 하네스가 턴 종료 후 동기화. 응답 문자열에 로컬/원격 구분.
- 삭제는 파일과 디렉터리 재귀, rename 은 경로 이동만(내용 불변).
- `[[path]]` 상호 참조 일관성: 생성·삭제 시 다른 블록의 참조 갱신을 상기시켜야 한다.
- 프롬프트 투영: `system/` 아래 블록만 본문 투영, 나머지는 이름만. `expires` 가 지난 블록은 투영 제외.
- 백엔드 선택: 기존 `memory.backend` enum 에 로컬 저장소 백엔드가 대응해야 한다. tau 쪽 `off` 기본값은 `schema.rs:1877` 그대로 두고, 게이트웨이는 `local` 상당 값을 지정한다.
- 세션/이벤트 계약(정산 신호, RPC 명령명)은 이 조각 범위 밖이며 다른 조각 소관이다. 기억 도구가 `agent_settled`(`crates/omp-cli/src/ext.rs:1593`) 와 어떻게 상호작용해야 하는지는 확인 필요.

## 3. 설계

- 새 크레이트 `crates/omp-memory` (lib 이름 `omp_memory`). 의존성은 `serde`, `serde_json`, `serde_yaml_ng 0.10`, `thiserror`, `anyhow`, `tempfile`(dev). git 은 서브프로세스로만 호출해 새 네이티브 의존성을 만들지 않는다(`worktree.rs:123` 선례). 패치 파싱은 `pi-edit` 의 `parse_apply_patch`(`vendor/pi-edit/src/modes/apply_patch.rs:208`)를 그대로 쓴다.
- 모듈 배치:
  - `src/lib.rs`: `pub use` 재수출, `MemoryError`.
  - `src/repo.rs`: `pub struct MemoryRepo { root: PathBuf, identity: MemoryIdentity }`, `MemoryRepo::open(root)`, `init(root)`(`git init` + 초기 커밋), `resolve(&self, raw: &str) -> Result<PathBuf>`(상대/절대 검증, `..` 탈출 거부), `lock()`(저장소 단위 파일 락).
  - `src/block.rs`: `pub struct BlockFront { description: String, read_only: bool, expires: Option<String> }`, `pub struct MemoryBlock { path: PathBuf, front: BlockFront, body: String, raw_front: String }`, `MemoryBlock::parse(&str)`, `render(&self)`. 파싱은 `omp_task::agent` 의 `split_frontmatter`(`crates/omp-task/src/agent.rs:153`)와 같은 규칙(첫 `---` 줄, 다음 `---` 줄까지 YAML, 나머지 본문)을 쓰되 크레이트 경계를 위해 `omp-memory` 안에 동일 구현을 둔다.
  - `src/ops.rs`: `pub enum MemoryOp { Create{..}, StrReplace{..}, Insert{..}, Delete{..}, Rename{..}, UpdateDescription{..} }`, `pub fn apply_op(repo: &MemoryRepo, op: &MemoryOp) -> Result<OpOutcome>`. `OpOutcome { committed: bool, sha: Option<String>, message: String }`.
  - `src/patch.rs`: `pub fn apply_patch(repo: &MemoryRepo, reason: &str, input: &str) -> Result<OpOutcome>`. `parse_apply_patch` 결과 `ApplyPatchEntry` 를 경로 검증 → frontmatter 검증 → 디스크 반영 순으로 처리하고, 한 엔트리라도 실패하면 전체를 롤백(원본 바이트 스냅샷 후 복원).
  - `src/git.rs`: `fn git(root: &Path, args: &[&str]) -> Result<String>`, `fn commit_paths(root, paths, reason, identity) -> Result<String>`. `git add -- <paths>` 후 `git -c user.name=... -c user.email=<identity>@omo.local commit -m <reason>`; `--no-verify` 는 쓰지 않는다. 변경이 없으면 커밋하지 않고 `made no effective changes` 계열 결과를 돌려준다.
  - `src/index.rs`: `pub fn render_index(repo: &MemoryRepo, max_entries: usize) -> String`. 각 블록의 `path` + `description` 만 나열(비시스템 경로는 이름만).
  - `src/project.rs`: `pub fn projection(repo: &MemoryRepo, now_ms: i64, budget_chars: usize) -> Projection`, `Projection { system_blocks: Vec<(String, String)>, external_names: Vec<String>, truncated: bool }`. `expires` 비교는 UTC ISO-8601 문자열 파싱 후 `now_ms` 와 비교. 예산 초과 시 뒤에서부터 자르고 `truncated=true`.
  - `src/tool.rs` 는 두지 않는다. 도구 표면은 `crates/omp-cli/src/tools/memory.rs` 에 두고 `omp_memory` 는 순수 저장소 로직만 갖는다(기존 `tools/todo.rs` 가 상태를 도구 쪽에 두는 구조와 동일).
- 설정 경로: `memory.root` 기본값 `~/.omp/agent/memory`. 프로필이 있으면 `crates/omp-settings/src/settings.rs:461` 의 `agent_dir()` 결과를 그대로 재사용해 `~/.omp/profiles/<p>/agent/memory` 가 된다. 새 설정 키 `memory.root`(String, 기본 `""` = agent_dir 기반), `memory.identity`(String, 기본 `omt`), `memory.remote`(String, 기본 `""`), `memory.projectionBudgetChars`(Number, 기본 8000), `memory.autoCommit`(Boolean, 기본 true)를 `crates/omp-settings/src/schema.rs` 에 추가하고 `memory.backend` enum(`schema.rs:1877`)에 `local` 이 이미 있으므로 그 값을 활성 조건으로 쓴다. 백엔드가 `off` 면 도구를 광고하지 않는다.
- 도구 표면(`crates/omp-cli/src/tools/memory.rs`):
  - `pub fn memory_tool(settings: &omp_settings::Settings) -> Option<Arc<dyn AgentTool>>` — `memory.backend != "off"` 일 때만 `Some`.
  - `memory` 스키마는 `command` enum 6종 + `reason` 필수 + `file_path`/`file_text`/`description`/`old_string`/`new_string`/`insert_line`/`insert_text`/`old_path`/`new_path`. `reason` 누락·공백은 즉시 오류.
  - `memory_apply_patch` 스키마는 `reason` + `input`.
  - 결과 텍스트는 게이트웨이 문구를 그대로: `Memory <command> committed locally (<sha>).` / 원격 설정 시 `... committed (<sha>); harness will sync after the turn.`. 도구 실패는 `Err` 로 반환해 `isError: true` 가 되게 한다(`crates/omp-cli/src/tools.rs` 상단 주석의 규약).
  - 등록은 `tools.rs:321` 의 `let tools = vec![...]` 에 조건부 push, 게이트는 `tools.rs:420` `enabled_by_settings` 에 `"memory" | "memory_apply_patch" => "memory.backend" != "off"` 분기를 추가.
- 프롬프트 투영: `sysprompt.rs:949` 의 `files` 조립 직후 `omp_memory::project::projection()` 결과를 별도 데이터 키 `memoryBlocks` 로 넣고, `crates/omp-cli/prompts/project-prompt.md` 에 `<memory>` 블록을 추가해 `{{#each memoryBlocks}}<block path="...">{{content}}</block>{{/each}}` 로 렌더한다. 기존 `<repo-rules>`(`project-prompt.md:8`)와 분리해 사용자 AGENTS.md 와 섞이지 않게 한다. `sysprompt::build` 시그니처는 건드리지 않고 `Options`(`sysprompt.rs:836`)에 `memory_root: Option<PathBuf>` 를 추가한다.
- `/memory` 슬래시 재배선: `slash.rs:757` 의 고정 문자열을 실제 호출로 바꾼다. `view` → `projection()` 렌더, `stats` → 블록 수·총 바이트·미투영 수, `diagnose` → frontmatter 오류·`read_only`·`expires` 만료 목록, `sync` → `git status` 요약과 미커밋 경고, `clear`/`reset` → 저장소 이동 없이 비활성화 플래그만(파괴적 삭제는 사용자 확인 필요), `enqueue`/`rebuild` → `off` 백엔드에서는 기존 문구 유지.
- 동시성: 저장소 단위 배타 락(`repo.lock()`, `std::fs` 기반 lockfile)으로 `memory`/`memory_apply_patch` 동시 호출을 직렬화한다. 락 획득 실패는 도구 오류로 반환.
- 안전 규칙: 저장소 밖 경로 거부, 심볼릭 링크는 `canonicalize` 후 루트 prefix 검사, `read_only: "true"` 블록 수정 거부, 커밋 전 `git status --porcelain` 으로 무관 변경이 섞이지 않았는지 확인(섞이면 stage 를 경로 지정으로만 수행).

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-memory): scaffold crate with repo root resolution and git plumbing` — `crates/omp-memory/Cargo.toml`, `src/lib.rs`, `src/repo.rs`, `src/git.rs`. `Cargo.toml` 워크스페이스 멤버는 `crates/*` 라 추가 불필요. 단위 테스트: `resolve()` 의 상대/절대/탈출 케이스, `commit_paths` 가 경로만 stage 하는지.
2. `feat(omp-memory): block frontmatter parse and render` — `src/block.rs`. 테스트: `description` 필수, `read_only: "true"` 파싱, `expires` 보존, 본문 개행 보존(라운드트립).
3. `feat(omp-memory): block operations create/str_replace/insert/delete/rename/update_description` — `src/ops.rs`. 각 연산 성공·실패(미존재 경로, `old_string` 불일치, 디렉터리 재귀 삭제) 테스트.
4. `feat(omp-memory): codex-style apply_patch over memory repo` — `src/patch.rs`, `pi-edit` 의존 추가. 테스트: Add/Update/Move/Delete 4종, 부분 실패 시 전체 롤백.
5. `feat(omp-memory): index and prompt projection with expires` — `src/index.rs`, `src/project.rs`. 테스트: 만료 블록 제외, 예산 초과 절단, 비시스템 경로 이름만 노출.
6. `feat(settings): memory.root/identity/remote/projectionBudgetChars/autoCommit` — `crates/omp-settings/src/schema.rs` 키 추가, 기본값·프로필 경로 해석 테스트.
7. `feat(tools): memory and memory_apply_patch tools behind memory.backend` — `crates/omp-cli/src/tools/memory.rs`, `tools.rs:321`/`tools.rs:420` 배선. `off` 에서 미광고 테스트.
8. `feat(sysprompt): project memory blocks into the project prompt` — `crates/omp-cli/src/sysprompt.rs`(`Options` + `memoryBlocks`), `crates/omp-cli/prompts/project-prompt.md`. 골든 렌더 테스트.
9. `feat(slash): back /memory subcommands with the local repo` — `crates/omp-cli/src/slash.rs:757`. `builtin_commands.json:653` 문구와 실제 동작 일치 테스트.
10. `docs: memory repo layout and commit policy` — `docs/memory.md`(경로 규칙, 커밋 규칙, 투영 규칙).

## 5. 테스트

- 단위: 위 1~9 각 커밋의 `#[cfg(test)]` 모듈. `tempfile::TempDir` 로 저장소를 만들고 `git init` 후 검증(`crates/omp-task` 의 dev-dependency 선례와 동일).
- 골든: `tests-tools/` 관례(`tests-tools/todo_golden.json`, `tests-tools/ts_tools.json`)를 따라 `memory`/`memory_apply_patch` 스키마 JSON 을 고정. 스키마 필드명·enum 값·필수 여부만 고정하고 설명 문구는 고정하지 않는다.
- 통합: `crates/omp-cli/tests/` 에 `memory_tools.rs` — 실제 `omt` 세션을 띄우지 않고 도구 실행 경로(`FnTool` 실행)를 직접 호출해 (a) create → 파일 존재 + 커밋 sha 반환, (b) str_replace 불일치 → `isError`, (c) `read_only` 차단, (d) `off` 백엔드에서 도구 부재를 확인.
- 프롬프트: `sysprompt` 골든에 기억 블록 2개(시스템 1, reference 1)를 넣어 `<memory>` 안에 시스템 본문만, `<external_projection>` 에 이름만 나오는지 비교.
- 회귀: `cargo test -p omp-memory -p omp-cli -p omp-settings`, `cargo clippy --workspace -- -D warnings`. nondeterminism 금지: 커밋 sha 는 40자 hex 패턴 매칭만, 시간 의존 `expires` 테스트는 고정 `now_ms` 주입.

## 6. QA 시나리오

1. `memory.backend: "off"` 상태에서 세션 시작 → 시스템 프롬프트에 `<memory>` 없음, 도구 목록에 `memory` 없음, `/memory view` 는 기존 문구(`slash.rs:760`) 유지.
2. `memory.backend: "local"` 로 바꾸고 새 세션 → `memory` 도구 광고됨. `create` 로 `system/human.md` 생성 시 `description` 누락 → 오류, 포함 → 파일 생성 + `Memory create committed locally (<sha>).`.
3. `str_replace` 로 기존 블록 한 줄 교체 → `git log -1 --format=%s` 가 `reason` 과 일치, author 가 `<identity>@omo.local`.
4. `memory_apply_patch` 로 3파일 동시 수정(Add 1, Update 1, Delete 1) → 단일 커밋, `git show --stat` 이 3경로만 포함.
5. 패치 중 2번째 파일의 `old_string` 불일치 → 전체 롤백, 작업 트리 clean, 오류 메시지에 해당 경로.
6. `read_only: "true"` 블록에 `str_replace` → 거부, 파일 mtime 불변.
7. 저장소 밖 절대 경로(`/etc/passwd`) 지정 → 거부, 어떤 파일도 변경되지 않음.
8. `expires` 가 과거인 블록 → 프롬프트에서 제외, 파일은 남아 있고 `diagnose` 에 만료로 표시.
9. 세션 재시작 후 `/memory view` → 이전 턴에 쓴 블록이 그대로 투영됨(영속성).
10. `git status` 에 무관한 사용자 변경이 있는 상태에서 `create` → 그 변경이 커밋에 섞이지 않음.
11. 동시에 두 도구 호출(같은 파일) → 하나는 락 대기 후 순차 적용, 커밋 2개, 최종 내용 일관.

## 7. 위험과 미확인

- 위험: 게이트웨이가 기억을 도구가 아니라 RPC 명령으로 구동할 가능성. `rpc.rs:593` 관측 구간에 기억 명령이 없으나 전 구간 열거는 확인 필요. RPC 경로가 필요하면 `dispatch` 에 `memory_*` arm 을 추가하는 별도 조각이 필요하다.
- 위험: 커밋 author/아이덴티티 바인딩 방식이 mhc 와 다르면 게이트웨이 측 동기화가 커밋을 거부할 수 있다. mhc 는 `<identity>@omo.local` 을 쓰지만 tau 의 아이덴티티 저장 위치는 확인 필요.
- 위험: frontmatter 파서 이원화. `sysprompt.rs:256` 의 단순 파서를 기억 블록에 재사용하면 중첩·따옴표·리스트에서 깨진다. `omp-memory` 는 `serde_yaml_ng` 경로만 쓰고 `sysprompt` 파서는 건드리지 않는다.
- 위험: 프롬프트 예산. `memories.summaryInjectionTokenLimit`(기본 5000, `schema.rs:1863` 부근)과 새 `memory.projectionBudgetChars` 의 관계가 정의되지 않았다. 토큰/문자 환산 규칙은 확인 필요.
- 위험: `git` 서브프로세스 의존은 git 미설치·`safe.directory` 거부에서 실패한다. 실패 시 도구 오류로 표면화하고 파일 변경은 롤백해야 한다.
- 위험: `expires` 시간대 처리. mhc 는 UTC date 또는 offset 포함 ISO 를 받는다. tau 에 기존 ISO 파서(`crates/omp-cli/src/persist.rs` 의 `iso_from_ms`/`now_iso`)가 있으나 역방향 파싱기는 확인 필요.
- 미확인: senpi 세션 파일에 기억 관련 필드가 있는지, 게이트웨이가 턴 종료 후 원격 동기화를 실제로 수행하는지.
- 미확인: `memory.backend` 의 `local` 이 mhc 의 로컬 저장소 백엔드와 의미상 동일한지, 게이트웨이가 `mnemopi`/`sharpshooter` 를 요구하는지.
- 미확인: `/memory clear|reset` 의 파괴 범위. 현재 tau 문구는 `"Memory cleared."`(`slash.rs:761`)만 있고 삭제 대상이 정의돼 있지 않다. 백업·복원(`Restore memory backup`) 요구가 게이트웨이 계약에 포함되는지 확인 필요.
- 미확인: 이 조각에서 다루지 않은 나머지 4개 범주(RPC 명령 8종, CLI 플래그 4종, 확장 호스트 `api.rpc.handle`/`session_move_*`, `task_send`/`bash_output`/`monitor`/`workpool` 도구)와의 상호작용. 특히 `memory` 도구가 서브에이전트(`crates/omp-task`) 세션에서도 노출돼야 하는지는 확인 필요.
