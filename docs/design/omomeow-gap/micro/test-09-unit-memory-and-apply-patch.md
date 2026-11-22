# 테스트 조각 09: memory 6개 연산·read_only 보호·[[path]] 링크·memory_apply_patch Git 자동 커밋 테스트 명세 (test-09-unit-memory-and-apply-patch)

## 1. 목적 및 코드 근거 (File & Line)

`memory/memory_apply_patch`의 계약을 단위 테스트로 고정한다. 대상은 (a) YAML frontmatter 파싱과 `description` 필수 규칙, (b) `read_only: "true"` 문서의 모든 변이 차단, (c) 6개 연산(create, str_replace, insert, delete, rename, update_description)의 순차 적용, (d) `[[path]]` 링크 정규화/해석, (e) 호출 1회당 Git 커밋 1개 생성과 실패 시 무커밋 원자성이다.

근거 좌표:
- `memory/memory_apply_patch`: frontmatter(description 필수, `read_only:"true"` 수정 차단, `expires`), 마크다운 본문 블록, 6개 연산, `[[path]]` 링크, Git 자동 커밋.
- `ext-host/host.ts:340` `makeApi` 노출 표면(on, events.on, registerTool, registerCommand, registerFlag). `api.rpc.handle`은 **존재하지 않음**.
- `omo-nyang/src/session-move-extension.ts:130-140`은 `api.rpc.handle("omomeow.move-readiness", ...)`를 호출 → 표면 불일치. 본 조각은 이 불일치를 테스트 대상에서 분리하고 `extension_request` 경로만 검증한다.
- `omo-nyang/src/omo-feedback.ts:41,50` `extension_request({name, data})` 호출 형태 = memory 도구 호출의 기준 형태.
- `crates/omp-ext/src/host.rs:127` `LoadResult`, `:431` `send_request`, `:463` `request(method, params)`.
- `crates/omp-cli/src/rpc.rs:593` `dispatch`, `:203` `code` + `errorCode` 동시 직렬화(오류 판별은 `errorCode` 우선).
- `crates/omp-session/src/entries.rs:66-77` `TitleSlot`(256B 1행), `lib.rs:25` `SESSION_VERSION=3`, `lib.rs:225` 세션 헤더, `Entry::Message(id, parentId, timestamp, message{role, content, toolCallId, isError})` — memory 도구 호출/결과가 세션 엔트리로 남는지 교차 확인용.

## 2. 상세 계약 및 데이터/타입 명세

```rust
pub struct MemoryDoc { pub path: String, pub description: String,
                       pub read_only: bool, pub expires: Option<String>, pub body: String }

pub enum PatchOp {
  Create { path: String, description: String, body: String },
  StrReplace { path: String, old: String, new: String, expected_count: Option<usize> },
  Insert { path: String, after_line: usize, text: String },
  Delete { path: String },
  Rename { from: String, to: String },
  UpdateDescription { path: String, description: String },
}

pub struct AppliedOp { pub index: usize, pub op: &'static str, pub path: String, pub bytes_written: usize }
pub struct PatchResult { pub applied: Vec<AppliedOp>, pub links: Vec<LinkEdge>, pub commit: Option<CommitInfo> }
pub struct CommitInfo { pub sha: String, pub message: String, pub files: Vec<String> }
```

계약:
- frontmatter는 문서 선두 `---` 블록만 인정. `description` 부재/공백이면 `MEM_DESC_REQUIRED`로 전체 patch 거부(부분 적용 금지).
- `read_only: "true"`(문자열)와 `read_only: true`(불리언) 모두 보호로 간주. 보호 문서에 Create 이외 변이 연산이 닿으면 `MEM_READ_ONLY`.
- `expires`는 RFC3339 파싱 실패 시 `MEM_FRONTMATTER_INVALID`, 과거 시각이면 경고 필드만 세팅(차단 아님).
- `[[path]]`는 상대경로 기준, `.md` 생략 허용, `..` 탈출 금지. 해석 실패는 `MEM_LINK_BROKEN`(기본 수집만, 적용은 계속).
- Git: patch 1회 = 커밋 1개, 메시지 `memory: apply patch (<n> ops)`. 스테이징은 변경 파일만. 어떤 연산이든 실패하면 워킹트리 원복 + 커밋 0개.
- 오류 응답은 `rpc.rs:203` 규약대로 `{ code, errorCode, message }`를 함께 직렬화하고, 클라이언트 판별은 `errorCode`를 사용한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 포함)

```rust
// crates/omp-ext/tests/memory_apply_patch.rs
fn repo() -> (TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_path_buf();
    run(&r, &["git", "init", "-q"]);
    run(&r, &["git", "config", "user.email", "t@t"]);
    run(&r, &["git", "config", "user.name", "t"]);
    (d, r)
}

#[test]
fn six_ops_apply_in_order_with_single_commit() {
    let (_d, root) = repo();
    seed(&root, "a.md", "---\ndescription: d\n---\nhello world\n");
    commit_all(&root, "seed");
    let before = rev_count(&root);

    let ops = vec![
        Op::Create { path: "b.md".into(), description: "b".into(), body: "x\n".into() },
        Op::StrReplace { path: "a.md".into(), old: "hello".into(), new: "hi".into(), expected_count: Some(1) },
        Op::Insert { path: "a.md".into(), after_line: 1, text: "inserted\n".into() },
        Op::Delete { path: "b.md".into() },
        Op::Rename { from: "a.md".into(), to: "c.md".into() },
        Op::UpdateDescription { path: "c.md".into(), description: "c2".into() },
    ];
    let out = apply_patch(&root, &ops).unwrap();

    assert_eq!(out.applied.len(), 6);
    assert_eq!(out.applied[0].op, "create");
    assert_eq!(out.applied[5].op, "update_description");
    assert_eq!(rev_count(&root), before + 1);
    assert!(out.commit.unwrap().message.starts_with("memory: apply patch (6 ops)"));
    assert!(root.join("c.md").exists() && !root.join("a.md").exists());
}

#[test]
fn read_only_blocks_and_leaves_no_commit() {
    let (_d, root) = repo();
    seed(&root, "ro.md", "---\ndescription: d\nread_only: \"true\"\n---\nbody\n");
    commit_all(&root, "seed");
    let before = rev_count(&root);

    let err = apply_patch(&root, &[Op::StrReplace {
        path: "ro.md".into(), old: "body".into(), new: "x".into(), expected_count: None }]).unwrap_err();

    assert_eq!(err.error_code, "MEM_READ_ONLY");
    assert_eq!(rev_count(&root), before);
    assert_eq!(read(&root, "ro.md"), "---\ndescription: d\nread_only: \"true\"\n---\nbody\n");
}
```

```ts
// ext-host/__tests__/memory-patch.test.ts  (api.rpc.handle 부재 → extension_request 경로 사용)
it("[[path]] 링크를 수집하고 깨진 링크는 MEM_LINK_BROKEN 으로 보고한다", async () => {
  const res = await host.extensionRequest({
    name: "memory_apply_patch",
    data: { patch: [{ op: "create", path: "n.md", description: "n",
                      body: "see [[m]] and [[missing]]\n" }] },
  });
  const edges = res.links.map((e: any) => e.target);
  expect(edges).toContain("m.md");
  expect(res.warnings).toContainEqual(expect.objectContaining({ errorCode: "MEM_LINK_BROKEN" }));
  expect(res.applied[0].bytes_written).toBeGreaterThan(0);
});

it("description 누락 patch 는 전체 거부되고 세션 엔트리가 남지 않는다", async () => {
  const before = await countEntries(root);
  const res = await host.extensionRequest({ name: "memory_apply_patch",
    data: { patch: [{ op: "create", path: "z.md", description: "", body: "b" }] } });
  expect(res.errorCode).toBe("MEM_DESC_REQUIRED");
  expect(await countEntries(root)).toBe(before);
  expect(await gitLogCount(root)).toBe(await gitLogCount(root)); // 커밋 증가 없음
});
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. 6개 연산이 입력 순서대로 `applied`에 기록되고 최종 파일 상태가 기대값과 일치한다.
2. `read_only:"true"`/`true` 두 표기 모두 변이 시 `MEM_READ_ONLY`이며 파일 해시와 커밋 수가 불변이다.
3. `description` 누락·빈 문자열은 `MEM_DESC_REQUIRED`, frontmatter 파손은 `MEM_FRONTMATTER_INVALID`로 전체 patch 거부.
4. 성공 patch 1회당 커밋 정확히 1개, 실패 시 0개(부분 적용/부분 커밋 없음).
5. `[[path]]` 정규화(`.md` 생략, 상대경로)와 `..` 탈출 차단이 케이스별로 검증된다.
6. 오류 응답에 `code`와 `errorCode`가 동시에 존재하고 클라이언트 판별이 `errorCode`로 동작한다.
7. `cargo test -p omp-ext memory_apply_patch`, `cargo test -p omp-cli rpc_`, `bun test ext-host/__tests__/memory-patch.test.ts` 전부 통과.
8. 회귀 확인: `git log --oneline -n 5`로 커밋 메시지 규약, `cargo test -p omp-session`으로 세션 엔트리 직렬화 회귀 없음.

## 5. 엣지 케이스 및 실패 복구

- 빈 patch 배열: `applied=[]`, 커밋 생성 안 함(no-op), 오류 아님.
- 동일 경로에 Create 2회: 두 번째는 `MEM_ALREADY_EXISTS`로 전체 롤백.
- `str_replace` 다중 매치 + `expected_count` 불일치: `MEM_STR_COUNT_MISMATCH`, 파일 원복.
- `rename` 대상이 이미 존재: `MEM_TARGET_EXISTS`, 소스 보존.
- `read_only` 문서를 `rename`/`delete`로 우회 시도: `MEM_READ_ONLY`(경로 기준 보호, 연산 종류 무관).
- `[[path]]` 순환/자기 참조: 수집은 허용, 순회 깊이 8에서 절단하고 경고만 기록.
- Git 미초기화 또는 dirty 워킹트리: `MEM_GIT_UNAVAILABLE`/`MEM_GIT_DIRTY`로 사전 차단(부분 커밋 방지).
- 커밋 실패(권한/락): 인메모리 백업에서 파일 원복 후 오류 반환, `.git/index.lock` 잔존 시 1회 재시도.
- `expires` 만료 문서: 경고 필드 `expired: true`만 세팅, 적용은 계속(정책 변경 시 테스트로 고정).
- `api.rpc.handle` 부재로 `session-move-extension` 경로가 실패하는 문제는 본 조각 범위 밖이며, 별도 이슈로 분리해 `makeApi` 확장 여부를 결정한다.
