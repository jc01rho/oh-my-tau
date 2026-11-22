# 구현 조각 12: `crates/omp-memory` 신설 및 YAML frontmatter + Git 자동 커밋 기반 `memory`·`memory_apply_patch` 구현 명세 (impl-12-tools-memory-git-repo)

## 1. 목적 및 코드 근거 (File & Line)

- 격차 근거: 현재 `/memory`는 스텁이고, `memory`(create/str_replace/insert/delete/rename/update_description + git 자동 커밋) 및 `memory_apply_patch` 도구가 없다. Phase 3.3에서 `crates/omp-memory`로 통합한다.
- 도구 등록 지점: `crates/omp-cli/src/tools/` (기존 `tools/task.rs`, `tools/shell_host.rs`, `jobs.rs`, `wait.rs`와 동일한 `Tool` 트레이트 등록 방식). 신규 파일 `crates/omp-cli/src/tools/memory.rs`.
- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령 폴백은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`이므로, 여기 도달하기 전에 `"memory"`, `"memory_apply_patch"` arm을 추가해야 한다.
- 응답 프레임: `rpc.rs:203-226` `response()`가 조립하며 `omo-nyang/src/relay/move-host.ts:4-7,61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다. 따라서 실패 시 `code`와 `errorCode`를 함께 직렬화해야 한다(`CmdError::new` 단일 인자로는 부족 → `CmdError::with_code` 추가).
- 프로토콜 노출: `get_protocol_info()`가 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 반환하므로 `commands` 배열에 두 명령을 추가해야 클라이언트가 기능 탐지할 수 있다.
- 설정 루트: `crates/omp-settings/src/lib.rs`의 `~/.omp` 규약을 재사용해 기본 메모리 루트를 `~/.omp/memory`로 둔다(플릿의 `~/.omo`와 혼용 금지).
- 세션 기록 규약(`crates/omp-session/src/entries.rs`, camelCase entries)과 충돌하지 않도록 메모리 파일은 세션 JSONL과 완전히 분리된 저장소에 쓴다.
- 서브에이전트/도구 격차 목록(하위 작업 `task_send`/`task_output`, `bash_output`/`kill_bash`, `workpool`, `ask_user_question`/`create_goal` 3종)과 동일한 "도구 부재" 계열이며, 본 조각은 그중 장기 기억 축만 담당한다.

## 2. 상세 계약 및 데이터/타입 명세

- 크레이트 구조
  - `crates/omp-memory/src/lib.rs` — 공개 API
  - `crates/omp-memory/src/frontmatter.rs` — YAML frontmatter 파서/직렬화(`serde_yaml`)
  - `crates/omp-memory/src/store.rs` — 파일 CRUD + 이름 인덱스
  - `crates/omp-memory/src/git.rs` — 자동 커밋
  - `crates/omp-memory/src/patch.rs` — `memory_apply_patch`
- 메모리 파일 포맷(확장자 `.md`, 경로 `<root>/<name>.md`)

```yaml
---
id: mem_01J8Z3K4
name: project-architecture        # ^[a-z0-9][a-z0-9._-]{0,63}$
description: "CLI 크레이트 구조 요약"   # <= 200 chars
scope: project                    # project | global
tags: [rust, cli]
created_at: 2026-11-22T10:00:00Z
updated_at: 2026-11-22T10:12:31Z
---
본문 마크다운...
```

- Rust 타입

```rust
pub struct MemoryMeta { pub id: String, pub name: String, pub description: String,
  pub scope: Scope, pub tags: Vec<String>, pub created_at: String, pub updated_at: String }
pub struct MemoryDoc { pub meta: MemoryMeta, pub body: String }
pub enum Scope { Project, Global }

pub trait MemoryStore {
    fn create(&self, name:&str, description:&str, body:&str, scope:Scope, tags:&[String]) -> Result<MemoryDoc, MemoryError>;
    fn str_replace(&self, name:&str, old:&str, new:&str) -> Result<usize, MemoryError>; // 교체 횟수
    fn insert(&self, name:&str, after_line:usize, text:&str) -> Result<(), MemoryError>;
    fn delete(&self, name:&str) -> Result<(), MemoryError>;
    fn rename(&self, from:&str, to:&str) -> Result<(), MemoryError>;
    fn update_description(&self, name:&str, description:&str) -> Result<(), MemoryError>;
    fn read(&self, name:&str) -> Result<MemoryDoc, MemoryError>;
    fn list(&self) -> Result<Vec<MemoryMeta>, MemoryError>;
}
pub struct GitCommitter { repo: PathBuf, enabled: bool }
impl GitCommitter { pub fn commit(&self, msg:&str, paths:&[&Path]) -> Result<Option<String>, MemoryError>; }
```

- RPC 명령 페이로드(`dispatch` arm 입력 `cmd: &Value`)
  - `{"cmd":"memory","op":"create","name":"...","description":"...","body":"...","scope":"project","tags":["rust"]}`
  - `{"cmd":"memory","op":"str_replace","name":"...","old":"...","new":"..."}` → `data:{replacements:1}`
  - `{"cmd":"memory","op":"insert","name":"...","after_line":12,"text":"..."}`
  - `{"cmd":"memory","op":"delete"|"rename"|"update_description"|"read"|"list", ...}`
  - `{"cmd":"memory_apply_patch","patch":"--- a/x.md\n+++ b/x.md\n@@ ...","dry_run":false}`
- 오류 코드(`errorCode`): `memory_not_found`, `memory_already_exists`, `memory_name_invalid`, `memory_str_replace_not_unique`, `memory_insert_out_of_range`, `memory_patch_invalid`, `memory_locked`, `memory_git_failed`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// rpc.rs dispatch 내부, rpc.rs:1094 폴백 이전에 삽입
"memory" => {
    let op = cmd.get("op").and_then(|v| v.as_str())
        .ok_or_else(|| CmdError::with_code("memory_name_invalid", "memory_op_missing", "op required"))?;
    let store = rt.memory_store(); // Arc<dyn MemoryStore>, root = settings.home().join("memory")
    match op {
        "create" => store.create(name, desc, body, scope, &tags).map(|d| json!({"id":d.meta.id,"name":d.meta.name})),
        "str_replace" => store.str_replace(name, old, new).map(|n| json!({"replacements": n})),
        "insert" => store.insert(name, after_line, text).map(|_| json!({"ok":true})),
        "delete" => store.delete(name).map(|_| json!({"ok":true})),
        "rename" => store.rename(from, to).map(|_| json!({"ok":true})),
        "update_description" => store.update_description(name, desc).map(|_| json!({"ok":true})),
        "read" => store.read(name).map(|d| json!({"meta":d.meta,"body":d.body})),
        "list" => store.list().map(|v| json!({"items": v})),
        other => Err(CmdError::with_code("memory_name_invalid","memory_op_unknown", format!("Unknown memory op: {other}"))),
    }
}
"memory_apply_patch" => patch::apply(rt.memory_store(), patch_str, dry_run),
```

- `str_replace` 검증: 본문에서 `old` 등장 횟수를 센다. 0회 → `memory_str_replace_not_unique`(detail `count:0`), 2회 이상 → 동일 코드(`count:n`). 1회일 때만 치환하고 `updated_at` 갱신 후 커밋.
- `insert` 검증: `after_line <= body.lines().count()` 아니면 `memory_insert_out_of_range`.
- `rename` 검증: `to`가 슬러그 정규식 불일치 시 `memory_name_invalid`, 대상 존재 시 `memory_already_exists`. 파일 이동은 `std::fs::rename` + frontmatter `name` 동시 갱신.
- 원자적 쓰기: `tempfile::NamedTempFile`로 같은 디렉터리에 쓰고 `persist()`로 교체. 실패 시 원본 보존.
- 동시성: `<root>/.lock`에 `fd-lock` 배타 락, 5회 재시도(20ms 백오프), 초과 시 `memory_locked`.
- Git 자동 커밋(`git.rs`): `git` CLI를 `std::process::Command`로 호출(의존성 최소화).
  1. `<root>/.git` 없으면 `git init -q` + `.gitignore`(`.lock`, `*.tmp`) 생성.
  2. `git -C <root> add -- <paths>`
  3. `git -C <root> -c user.name=omp -c user.email=omp@localhost commit -q --no-verify -m "memory(<op>): <name>"` (patch는 `memory(patch): <n> files`)
  4. 변경 없음(exit 1, "nothing to commit")은 성공으로 간주하고 `Ok(None)`.
- `memory_apply_patch`: `patch` 크레이트로 unified diff 파싱 → 대상 경로가 `<root>` 밖(`..` 포함)이면 `memory_patch_invalid`. 모든 hunk를 메모리상 적용해 frontmatter 유효성(`name`/`description` 필수) 검사 후, `dry_run=false`일 때만 일괄 쓰고 단일 커밋. `dry_run=true`면 `{"applied":n,"dryRun":true}`만 반환.
- 도구 래퍼 `tools/memory.rs`: `Tool::name() -> "memory" | "memory_apply_patch"`, JSON Schema는 위 페이로드와 1:1, 실행은 `dispatch`와 동일한 `MemoryStore` 경유(중복 구현 금지).

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- `cargo build -p omp-memory -p omp-cli` 및 `cargo clippy -- -D warnings` 통과.
- 단
