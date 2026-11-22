# 리뷰 조각 06: `memory`·`memory_apply_patch`의 경로 이탈(Path Traversal) 차단, `read_only: "true"` 보호, Git 인덱스 잠금 경합 감사 (rev-06-memory-git-safety-and-readonly-audit)

## 1. 목적 및 코드 근거 (File & Line)

- 도구 격차의 실측 근거: `crates/omp-cli/src/tools/`에는 `task.rs`(배치 동기 실행)만 존재하고, OmOMeow가 호출하는 `memory`(create/str_replace/insert/delete/rename/update_description + git 자동 커밋)와 `memory_apply_patch`가 없다. `/memory`는 스텁이며 Phase 3.3의 `crates/omp-memory`와 통합이 필요하다(근거 5).
- 디스패치 계약: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 따라서 신규 `memory`/`memory_apply_patch` 명령을 여기 등록하지 않으면 클라이언트는 Unknown command만 받는다.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226`의 `response()`가 `{type, id, success, data?, error?, errorCode?}`를 조립한다. `omo-nyang/src/relay/move-host.ts:4-7, 61`의 `MoveHost`가 동일 스키마를 파싱하므로, 실패 시 `code`(문자열)와 `errorCode`를 **함께** 직렬화해야 UI가 분기할 수 있다.
- 설정 루트: `crates/omp-settings/src/lib.rs`가 `~/.omp`를 루트로 삼는다. memory 저장 루트는 이 루트 하위로 고정되어야 하며, `omo-nyang` 측 플릿 설정 루트(`~/.omo`, `omo.jsonc`)와 혼동하면 안 된다.
- Git 자동 커밋: memory 도구는 쓰기 성공 시 저장소에 커밋을 남긴다. 동일 `.git/index.lock`에 대해 병렬 세션(`crates/omp-session/src/entries.rs` 기반 JSONL v3 세션 다수)이 동시에 커밋하면 `index.lock exists` 실패가 발생한다. 본 조각은 이 경합을 재시도·직렬화로 흡수하고, `read_only: "true"` 호출에서는 쓰기·커밋을 원천 차단한다.

## 2. 상세 계약 및 데이터/타입 명세

RPC 명령(신규 2종):

```
memory            { "path": string, "command": "create"|"str_replace"|"insert"|"delete"|"rename"|"update_description",
                    "file_text"?: string, "old_str"?: string, "new_str"?: string,
                    "insert_line"?: number, "insert_text"?: string,
                    "old_path"?: string, "new_path"?: string, "description"?: string,
                    "read_only"?: "true" | boolean }
memory_apply_patch { "input"?: string, "patch"?: string, "read_only"?: "true" | boolean }
```

성공 응답 `data`:

```
{ "ok": true, "path": string, "bytes": number, "commit"?: string|null, "dryRun": false }
```

실패 응답: `{ "success": false, "error": string, "errorCode": MemoryErrorCode }` — `errorCode`는 다음 enum과 1:1.

```
MemoryErrorCode =
  | "memory_root_unresolved"     // 루트 canonicalize 실패
  | "memory_path_escape"         // 루트 밖(상대경로 .., 절대경로, 심볼릭 링크 탈출)
  | "memory_read_only_violation" // read_only="true"인데 쓰기/커밋 시도
  | "memory_not_found"           // old_str/old_path 불일치
  | "memory_ambiguous_match"     // old_str 다중 매치
  | "memory_git_lock_timeout"    // index.lock 재시도 소진
  | "memory_git_commit_failed"   // 커밋 자체 실패(권한, 훅)
  | "memory_patch_invalid"       // apply_patch 파싱 실패
```

Rust 타입(`crates/omp-memory/src/lib.rs` 신설):

```rust
pub struct MemoryRoot(PathBuf);                  // canonicalize 완료본만 보유
pub enum MemoryCommand { Create, StrReplace, Insert, Delete, Rename, UpdateDescription }
pub struct MemoryRequest { pub path: String, pub command: MemoryCommand,
    pub read_only: bool, /* ... */ }
pub struct MemoryOutcome { pub path: PathBuf, pub bytes: usize, pub commit: Option<String> }
pub enum MemoryError { RootUnresolved, PathEscape, ReadOnlyViolation, NotFound,
    AmbiguousMatch, GitLockTimeout, GitCommitFailed, PatchInvalid }
```

`read_only` 해석 규칙: 문자열 `"true"`, 불리언 `true`, 그 외(`"false"`, `false`, 부재)는 쓰기 허용. 문자열 `"false"`를 truthy로 오판하지 않도록 명시 파싱한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

경로 검증(모든 명령 진입점에서 최우선):

```rust
fn resolve(root: &MemoryRoot, rel: &str, follow_symlink: bool) -> Result<PathBuf, MemoryError> {
    if Path::new(rel).is_absolute() { return Err(MemoryError::PathEscape); }
    let mut out = root.0.clone();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => return Err(MemoryError::PathEscape), // ParentDir, RootDir, Prefix
        }
    }
    // 부모가 존재하면 심볼릭 링크 해소 후 루트 prefix 재확인
    if let Some(parent) = out.parent().filter(|p| p.exists()) {
        let real = parent.canonicalize().map_err(|_| MemoryError::RootUnresolved)?;
        if !real.starts_with(&root.0) { return Err(MemoryError::PathEscape); }
    }
    Ok(out)
}
```

- `rename`은 `old_path`와 `new_path` 양쪽 모두 `resolve()` 통과 필요. 한쪽만 검사하는 것이 전형적 우회 경로다.
- `memory_apply_patch`는 패치 헤더의 `---`/`+++`/`*** Move to:` 경로를 파싱해 각각 `resolve()`에 통과시킨 뒤 적용한다. 파싱 실패는 `memory_patch_invalid`.
- `str_replace`는 `old_str` 매치 수를 세어 0이면 `memory_not_found`, 2 이상이면 `memory_ambiguous_match`.

read_only 강제(도구 경계에서 1회, 커밋 직전 1회 이중 검사):

```rust
if req.read_only { return Err(MemoryError::ReadOnlyViolation); } // 쓰기 명령 전부
```

Git 커밋 경로(`commit_with_retry`):

```rust
fn commit_with_retry(repo: &Path, msg: &str) -> Result<String, MemoryError> {
    let lock = repo.join(".git/index.lock");
    for attempt in 0..6 {                       // 총 대기 약 3.1s (50,100,200,400,800,1600ms)
        match run_git(repo, &["commit", "-m", msg]) {
            Ok(id) => return Ok(id),
            Err(e) if e.contains("index.lock") => {
                if let Ok(meta) = fs::metadata(&lock) {
                    let stale = meta.modified().map(|m| m.elapsed().unwrap_or_default()
                        .as_secs() > 10).unwrap_or(false);
                    if stale { let _ = fs::remove_file(&lock); }  // pid 생존 확인 후에만
                }
                std::thread::sleep(Duration::from_millis(50 << attempt));
            }
            Err(e) => return Err(MemoryError::GitCommitFailed),
        }
    }
    Err(MemoryError::GitLockTimeout)
}
```

RPC 배선: `rpc.rs:593`의 `dispatch` match에 `"memory"` / `"memory_apply_patch"` arm 추가. arm 내부에서 `MemoryError`를 `CmdError::with_code(msg, code)`로 매핑해 `response()`(`rpc.rs:203-226`)가 `errorCode`를 채우도록 한다. `memory_git_lock_timeout`은 재시도 가능 오류로 표시해 TS 측이 1회 재호출할 수 있게 한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `memory` create → 파일 생성 + 커밋 해시 반환, `get_state` 계열 상태 변화 없음.
2. `path: "../../etc/passwd"`, `path: "/etc/passwd"`, 루트 내부에서 외부로 향하는 심볼릭 링크 3종 모두 `memory_path_escape` 반환, 파일 미변경.
3. `rename`에서 `old_path` 정상 + `new_path: "../x"` → 거부되고 원본 유지.
4. `read_only: "true"`로 create/delete/rename/apply_patch 호출 시 `memory_read_only_violation`, 그리고 `.git` 디렉터리에 새 커밋 0건(`git rev-list --count HEAD` 불변).
5. 병렬 8 프로세스가 같은 저장소에 create를 동시 호출해도 `memory_git_lock_timeout` 없이 전건 성공(또는 명시적 재시도 후 성공), 커밋 수 = 성공 수.
6. 응답 JSON에 실패 시 `error`와 `errorCode`가 동시 존재하며 `move-host.ts` 파서가 예외 없이 통과.

검증 명령:

```
cargo test -p omp-memory -- --nocapture
cargo test -p omp-cli rpc::tests::memory_dispatch
cargo clippy -p omp-memory -p omp-cli -- -D warnings
cargo run -p omp-cli -- --mode rpc -e /dev/null  # 수동: memory/apply_patch 요청 후 errorCode 확인
bun test omo-nyang/src/relay   # 응답 프레임 파싱 회귀
```

## 5. 엣지 케이스 및 실패 복구

- 루트 자체가 심볼릭 링크(`~/.omp` → 다른 볼륨): 기동 시 1회 canonicalize 후 `MemoryRoot`에 고정. 요청마다 재해소하지 않는다.
- 대소문자 비구분 파일시스템: `starts_with` 비교는 canonicalize 결과 기준으로만 수행하고, 원본 문자열 비교로 우회 판정하지 않는다.
- `read_only`가 `"true"` 문자열과 `true` 불리언으로 혼재: 두 표현 모두 차단, `"false"`는 허용으로 확정(테스트로 고정).
- `index.lock`이 남긴 stale 파일: mtime 10초 초과 + 소유 pid 미존재일 때만 삭제. pid 확인 불가 시 삭제하지 않고 `memory_git_lock_timeout` 반환.
- 커밋 성공 후 응답 유실(RPC 타임아웃): 파일은 이미 반영됨. 클라이언트 재호출 시 `create`는 `memory_not_found`가 아니라 "동일 내용 존재"로 판정해 idempotent하게 `ok:true` 반환하도록 한다.
- `apply_patch` 부분 적용 실패: 전체를 원자적으로 되돌리고(백업 파일 또는 `git stash` 없이 in-memory 롤백) `memory_patch_invalid` 반환.
- `memory_not_found`/`memory_ambiguous_match`는 재시도 무의미로 표시하고, `memory_git_lock_timeout`만 TS 측 1회 재시도 대상으로 분류한다.
- 세션 JSONL(`crates/omp-session/src/entries.rs`) 기록과 memory 커밋이 동시에 발생해도 세션 파일은 건드리지 않는다(도구는 `~/.omp` 하위 memory 루트만 수정).
