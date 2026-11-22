# 테스트 조각 03: `move_session` 6개 op(`prepare`~`cancel`) 및 `release_session` 원자성·롤백 테스트 명세 (test-03-unit-move-and-release-session)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: `move_session`의 6개 op(`prepare`, `export`, `import`, `status`, `finish`, `cancel`)와 `release_session`이 (a) 계약된 JSON 스키마를 정확히 주고받고 (b) 어느 단계에서 실패해도 세션 파일·번들 디렉터리·move 상태가 원자적으로 롤백됨을 단위/통합 테스트로 고정한다.
- `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 테스트 진입점이며, `cmd["op"]` 분기 누락 시 아래 1094 라인으로 낙하한다.
- `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 미지원 op가 성공으로 위장하지 않는다는 기준선(negative test 근거).
- `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립. 실패 시 `code`와 `errorCode`를 동시 직렬화해야 한다.
- `omo-nyang/src/relay/move-host.ts:4-7, 61` — `MoveHost`가 위 프레임을 파싱. `success:false`에서 `error`/`errorCode` 부재 시 파서가 예외를 던지므로 두 필드 동시 존재를 테스트로 강제한다.
- `omo-nyang/src/relay/move-operations.ts:104-225` — 6개 op의 상태 전이 및 `moveId`/`sessionPath`/`bundleDir`/`cwd`/`from`/`answer` 필드 소비 지점.
- `omo-nyang/src/relay/session-move-extension.ts:116-139` — 확장 이벤트 `session_move_export`/`session_move_import`(`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`) 발행 지점. 롤백 시 이벤트 미발행/취소 신호 검증 대상.
- `omo-nyang/src/relay/move-host.ts:37-39` — 호스트 기동 인자 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`.
- `omo-nyang/src/lane-hold.ts:29-76` — `interrupt({intent})`/`get_state({paused, runPaused, pauseSeq})`/`resume_session()` 계약. move 진행 중 보류 상태와의 상호배제를 교차 검증하는 근거로만 사용한다(이 조각의 1차 범위 아님).

## 2. 상세 계약 및 데이터/타입 명세

요청/응답 프레임(고정):

```json
{"type":"request","id":"t-1","command":"move_session","args":{"op":"prepare","moveId":"m-1","sessionPath":"/s/a.jsonl","bundleDir":"/b/m-1","cwd":"/w","from":"local"}}
{"type":"response","id":"t-1","success":true,"data":{"moveId":"m-1","op":"prepare","state":"prepared","sessionPath":"/s/a.jsonl","bundleDir":"/b/m-1"}}
{"type":"response","id":"t-2","success":false,"error":"move not prepared","errorCode":"move_not_prepared","code":"move_not_prepared"}
```

- Rust 측 시그니처(기존 유지, 테스트가 직접 호출): `dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`, `CmdError::new(impl Into<String>)`.
- op별 `data` 계약:
  - `prepare` → `{moveId, op:"prepare", state:"prepared", sessionPath, bundleDir}`
  - `export` → `{moveId, op:"export", state:"exported", bundlePath, bytes}` (`session_move_export` 1회 발행)
  - `import` → `{moveId, op:"import", state:"imported", sessionPath}` (`session_move_import` 1회 발행)
  - `status` → `{moveId, op:"status", state, sessionPath, bundleDir}` (부작용 없음, 멱등)
  - `finish` → `{moveId, op:"finish", state:"finished"}`
  - `cancel` → `{moveId, op:"cancel", state:"cancelled"}`
- `release_session` 요청: `{"sessionId":"s-1","reason":"move","moveId":"m-1"}` → 응답 `{session_path:"/s/a.jsonl"}`. `reason`이 `"move"`가 아니면 거부.
- 상태 머신: `idle → prepared → exported → imported → finished`, 각 상태에서 `cancel`만 허용(terminal `finished`/`cancelled`에서는 `status` 외 전부 거부).
- 오류 코드(신규 정의, `code`와 `errorCode` 동일 값으로 이중 직렬화): `move_not_prepared`, `move_already_exists`, `move_state_invalid`, `move_id_mismatch`, `session_not_found`, `bundle_path_escape`, `release_reason_invalid`, `release_in_progress`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

신규 파일 2개: `crates/omp-cli/tests/move_session_ops.rs`, `omo-nyang/src/relay/__tests__/move-host.move-session.test.ts`.

Rust 통합 테스트 하네스(의사코드):

```rust
fn boot() -> Child // omt --mode rpc --multi-session --session-runtime in-process -e <ext>
fn rpc(c: &mut Child, id: &str, args: Value) -> Value // 한 줄 write, 한 줄 read, 5s timeout
#[test] fn happy_path_six_ops() {
    let mut c = boot();
    assert_eq!(rpc(&mut c,"1",prepare("m-1")).data.state, "prepared");
    assert_eq!(rpc(&mut c,"2",status("m-1")).data.state, "prepared");
    assert_eq!(rpc(&mut c,"3",export("m-1")).data.state, "exported");
    assert_eq!(rpc(&mut c,"4",import("m-1")).data.state, "imported");
    assert_eq!(rpc(&mut c,"5",finish("m-1")).data.state, "finished");
    let rel = rpc_release(&mut c,"s-1","move","m-1");
    assert!(rel.data["session_path"].as_str().unwrap().ends_with(".jsonl"));
}
```

원자성·롤백 케이스(각각 별도 `#[test]`):

```text
1) export 실패 주입(읽기전용 bundleDir) → state는 "prepared" 유지, bundleDir 내 부분 파일 0개, session_move_export 미발행
2) import 도중 손상 번들 → state는 "exported" 유지, 대상 sessionPath mtime/해시 불변, session_move_import 미발행
3) cancel 후 재status → state=="cancelled", bundleDir 제거, 이후 export 호출은 move_state_invalid
4) prepare 중복(moveId 재사용) → move_already_exists, 최초 move 상태 불변
5) moveId 불일치(status에 m-2) → move_id_mismatch
6) release_session(moveId 불일치) → release_reason_invalid/move_id_mismatch, 세션 파일 잔존
```

TS 측(`vitest`)은 가짜 RPC 서버로 프레임 형태를 검증한다:

```ts
// 실패 응답에 error와 errorCode가 모두 있어야 MoveHost 파싱이 성공한다(move-host.ts:4-7,61)
server.reply({ type: "response", id, success: false, error: "x", errorCode: "move_state_invalid", code: "move_state_invalid" });
await expect(host.moveSession({ op: "cancel", moveId: "m-1" })).rejects.toMatchObject({ errorCode: "move_state_invalid" });
```

각 테스트는 임시 디렉터리(`tempfile::tempdir`)를 사용하고 종료 시 자식 프로세스에 SIGTERM 후 1초 내 미종료 시 SIGKILL, 임시 경로 잔존 여부를 단정한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- 6개 op 각각 성공/실패 경로 최소 1개, 총 12케이스 이상 통과.
- 실패 응답 전 케이스에서 `success:false` + `error` + `errorCode` + `code` 동시 존재(누락 시 실패).
- 롤백 3종(export/import/cancel)에서 세션 파일 SHA-256 불변, 부분 번들 0개.
- 미지원 op는 `rpc.rs:1094` 경로로 `Unknown command:` 메시지 반환.
- 테스트 종료 후 임시 디렉터리 및 자식 프로세스 잔존 0.

```bash
cargo test -p omp-cli --test move_session_ops -- --nocapture
cargo test -p omp-cli rpc::dispatch
pnpm -C omo-nyang test -- move-host.move-session
pnpm -C omo-nyang exec tsc --noEmit
```

## 5. 엣지 케이스 및 실패 복구

- `op` 누락/비문자열: `Unknown command: ` 대신 `move_state_invalid`로 정규화하고 프레임은 정상 응답 형태 유지.
- `bundleDir`가 `sessionPath` 상위나 `..` 포함: `bundle_path_escape`로 거부, 파일 생성 전 차단.
- 동시 move 2건: 두 번째 `prepare`는 `move_already_exists`, 첫 move 상태 불변(뮤텍스는 `moveId` 단위).
- import 중 프로세스 강제 종료: 재기동 후 `status`가 `exported`를 보고하고 `cancel`로 정리 가능해야 함(저널 파일 기반 복구).
- `finish` 이후 `cancel`: `move_state_invalid`, 번들 삭제는 `finish` 성공 시에만 수행.
- `release_session`이 move 진행 중 호출: `release_in_progress`로 거부하고 세션 파일 미삭제, move는 계속 진행.
- 디스크 가득참/권한 오류: 부분 산출물 삭제 후 원 상태 유지, 오류 메시지에 경로 포함하되 `code`는 `move_state_invalid`로 통일.
- `cancel` 멱등성: 이미 `cancelled`면 성공 응답(`state:"cancelled"`)으로 재시도 안전.
