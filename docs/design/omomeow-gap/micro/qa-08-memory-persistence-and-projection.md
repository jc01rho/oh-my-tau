# QA 시나리오 08: `memory`·`memory_apply_patch` 실행 후 Git 커밋 생성 및 다음 턴 `<memory>` 시스템 프롬프트 투영 검증 (qa-08-memory-persistence-and-projection)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: 에이전트가 `memory`(create/str_replace/insert/delete/rename/update_description) 및 `memory_apply_patch`를 호출했을 때 (a) `~/.omp/memory` Git 저장소에 커밋이 실제 생성되고, (b) 그 결과가 세션 JSONL v3에 `memory_write` 엔트리로 기록되며, (c) **다음 턴** 시스템 프롬프트에 `<memory>...</memory>` 블록으로 투영되는지를 원자적으로 검증한다.
- 현재 격차: `/memory`는 스텁이고 `memory`·`memory_apply_patch` 도구가 없다. Phase 3.3 `crates/omp-memory` 신설 및 `crates/omp-cli/src/tools/memory.rs` 등록이 선행 조건이다.
- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어지므로, QA는 `memory`/`memory_apply_patch`/`memory_status` arm이 1094 이전에 존재함을 회귀 조건으로 삼는다.
- 응답 프레임: `rpc.rs:203-226` `response()`가 조립하며 `MoveHost`(`omo-nyang/src/relay/move-host.ts:4-7, 61`)가 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다. 따라서 실패 시 `code`와 `errorCode`를 **동시에** 직렬화해야 한다(§2).
- 프로토콜 노출: `get_protocol_info()`는 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 반환하므로(`omo-nyang/src/lane-hold.ts:29-76`), `commands` 배열에 `memory`, `memory_apply_patch`가 포함되어야 한다. `reload()`는 `{cancelled:false}`를 반환하며 메모리 인덱스도 재로딩 대상이다.
- 설정 루트: `~/.omp`(`crates/omp-settings/src/lib.rs`). 플릿 측 `~/.omo`(`omo.jsonc`)와 혼동하지 않도록 QA는 `OMP_HOME` 오버라이드로 임시 디렉터리를 주입한다.
- 세션 기록: omp JSONL v3(`crates/omp-session/src/entries.rs`, 256B title slot + camelCase entries). `omo-nyang`의 `session-patterns.ts:47-76`은 assistant/toolResult 항목만 읽으므로 `memory_write` 엔트리는 **비파괴 추가**여야 한다.
- 종료 신호: QA 하네스는 `runtime.rs:1641`의 `{"type":"session_settled"}`를 기준으로 턴 종료를 판정한다(`agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부 전용이므로 사용 금지).

## 2. 상세 계약 및 데이터/타입 명세

저장소 레이아웃(신규 `crates/omp-memory/src/store.rs`):

```
~/.omp/memory/.git/
~/.omp/memory/MEMORY.md            # 인덱스(파일 목록 + description)
~/.omp/memory/user/<slug>.md       # type ∈ {user, project, reference}
~/.omp/memory/project/<slug>.md
~/.omp/memory/reference/<slug>.md
```

메모리 파일 포맷(frontmatter + 본문):

```markdown
---
name: user-tabs
description: 사용자는 탭 들여쓰기를 선호한다
type: user
createdAt: 2026-11-22T00:00:00Z
updatedAt: 2026-11-22T00:00:00Z
---
본문 텍스트
```

도구 파라미터 스키마(`crates/omp-cli/src/tools/memory.rs`, serde `#[serde(tag = "op", rename_all = "snake_case")]`):

```jsonc
{"op":"create","name":"user-tabs","description":"...","type":"user","content":"..."}
{"op":"str_replace","name":"user-tabs","oldText":"탭","newText":"스페이스"}
{"op":"insert","name":"user-tabs","afterLine":3,"content":"추가 라인"}
{"op":"delete","name":"user-tabs"}
{"op":"rename","name":"user-tabs","newName":"user-indent"}
{"op":"update_description","name":"user-tabs","description":"새 설명"}
```

`memory_apply_patch`는 `*** Begin Patch` 봉투(envelope) 형식을 계약으로 고정한다:

```jsonc
{"patch":"*** Begin Patch\n*** Update File: user/user-tabs.md\n@@\n-탭\n+스페이스\n*** Add File: reference/rpc.md\n+...\n*** Delete File: project/old.md\n*** End Patch"}
```

성공 응답 `data`(RPC/도구 공통):

```json
{"op":"create","name":"user-tabs","path":"/home/u/.omp/memory/user/user-tabs.md",
 "commit":"a1b2c3d4e5f6","filesChanged":2,"bytesWritten":214,"indexUpdated":true}
```

실패 응답(`rpc.rs:203-226` + `move-host.ts:4-7,61`):

```json
{"type":"response","id":"8","success":false,
 "error":"patch hunk 2 failed at line 11","code":"patch_apply_failed",
 "errorCode":"patch_apply_failed"}
```

에러 코드 집합: `memory_disabled`, `memory_not_found`, `memory_name_conflict`, `memory_index_corrupt`, `patch_parse_failed`, `patch_apply_failed`, `git_unavailable`, `git_commit_failed`, `memory_locked`.

Git 계약: 저장소 미존재 시 `git init -b main` 자동 수행, 커밋 메시지 `memory: <op> <name>` (다중 파일 시 `memory: apply_patch (N files)`), author/committer `omp-memory <omp@local>`, `--no-verify` 금지(훅 실행 허용). 커밋 SHA는 응답 `commit`과 세션 엔트리에 동일 값으로 기록.

세션 엔트리(`crates/omp-session/src/entries.rs`, camelCase):

```json
{"type":"memory_write","timestamp":"2026-11-22T00:00:01Z","op":"create",
 "name":"user-tabs","path":".../user/user-tabs.md","commit":"a1b2c3d4e5f6"}
```

시스템 프롬프트 투영(`crates/omp-memory/src/project.rs`):

```
<memory>
다음은 이전 세션에서 저장된 장기 기억이다. 사용자 요청과 충돌하면 사용자 요청을 우선한다.

## user-tabs (user)
사용자는 탭 들여쓰기를 선호한다
</memory>
```

예산: `maxMemoryTokens = 4000`(문자수/4 근사), `maxMemoryFiles = 50`, 정렬 `updatedAt DESC`, 초과 시 말미 절단 + `... (N files omitted)` 마커. 투영은 **매 턴 재계산**하며 턴 시작 시점 스냅샷을 사용한다(도중 쓰기는 다음 턴 반영).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust: 도구 핸들러(`crates/omp-cli/src/tools/memory.rs`)

```rust
pub async fn memory(store: &MemoryStore, args: MemoryArgs) -> Result<MemoryOk, MemoryErr> {
    let _lock = store.lock().await?;                 // ~/.omp/memory/.lock (fs2)
    let mut dirty = store.read_index()?;             // MEMORY.md 파싱, 실패 시 memory_index_corrupt
    let changed = match args { /* 6개 op 분기, 파일 쓰기 */ };
    store.write_files(&changed)?;                    // 원자적 rename
    let commit = store.git_commit(&format!("memory: {} {}", op, name))?; // 실패 시 워크트리 복원
    store.update_index(&mut dirty)?;
    Ok(MemoryOk { op, name, path, commit, files_changed: changed.len(), .. })
}

pub fn build_memory_block(store: &MemoryStore, budget: usize) -> Option<String> { /* §2 포맷 */ }
```

RPC 배선(`crates/omp-cli/src/rpc.rs`, 1094 이전):

```rust
"memory" => { let a: MemoryArgs = serde_json::from_value(cmd["params"].clone())?;
              ok_or_err(response(id, memory(&rt.memory, a).await)) }
"memory_apply_patch" => { /* patch 파싱 -> 동일 트랜잭션 경로 */ }
"memory_status" => { /* {enabled, repoPath, head, files, dirty} */ }
```

프롬프트 조립(`crates/omp-cli/src/prompt.rs` 턴 시작): `session_settled` 이전에 `build_memory_block()` 결과를 시스템 프롬프트 말미에 삽입하고, 삽입 여부를 `systemPromptHash`로 세션 엔트리에 남겨 캐시 무효화 근거로 사용.

TS 검증(`omo-nyang/test/qa-08-memory.test.ts`, vitest):

```ts
const home = mkdtempSync(join(tmpdir(), "omp-home-"));
const host = spawn("omt", ["--mode","rpc","--multi-session","--session-runtime","in-process"]);
await rpc(host, "set_client_info", {width:80, capabilities:["interactive_user"]});
const p = await rpc(host, "get_protocol_info");
expect(p.data.commands).toEqual(expect.arrayContaining(["memory","memory_apply_patch"]));
const
