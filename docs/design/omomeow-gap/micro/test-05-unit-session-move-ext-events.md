# 테스트 조각 05: `session_move_export`·`session_move_import` 이벤트와 첨부파일 매니페스트(`session-move-extension.ts`) 호환 테스트 명세 (test-05-unit-session-move-ext-events)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 OmOMeow 하네스가 RPC 확장 호스트로 세션 이동을 수행할 때, tau 측 `session-move-extension`이 발행하는 `session_move_export`/`session_move_import` 이벤트와 번들 첨부파일 매니페스트가 호스트 계약과 바이트 단위로 일치하는지 검증하는 단위 테스트 명세다. 검증 대상 계약은 다음 실측 지점에 고정한다.

- 이벤트 발행 지점: `omo-nyang/src/session-move-extension.ts:116-139` — 이벤트명 `session_move_export` / `session_move_import`, payload `event = { dir, signal }`, 세션 파일은 `ctx.sessionManager.getSessionFile()`로 획득.
- 오퍼레이션 계약: `omo-nyang/src/move-operations.ts:104-225` — `move_session` op `"prepare"|"export"|"import"|"status"|"finish"|"cancel"`, 필드 `moveId`, `sessionPath`, `bundleDir`, `cwd`, `from`, `answer`.
- 호스트 실행 형태: `omo-nyang/src/relay/move-host.ts:37-39` — `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`.
- 응답 프레임 파서: `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 응답은 `code`와 `errorCode`를 동시에 직렬화해야 한다.
- 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` (`response()`), 디스패치 진입 `crates/omp-cli/src/rpc.rs:593`, 미지원 명령 `crates/omp-cli/src/rpc.rs:1094`.
- 라우팅: `extension_request({sessionId?, name, data?})`가 `api.rpc.handle(name, handler)`로 전달되며, 본 조각의 대상 이름은 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`.
- 부가 이벤트: `api.events.on("wake_source_state", {source, activeCount})` 구독이 이동 중에도 유지되는지 확인.
- `release_session({sessionId, reason:"move", moveId}) → {session_path}` 반환 스키마 회귀 확인.

## 2. 상세 계약 및 데이터/타입 명세

테스트가 고정하는 타입/스키마는 아래와 같다. 새 필드를 임의 추가하지 않는다.

```ts
// 세션 이동 이벤트 payload (session-move-extension.ts:116-139 계약)
type MoveEventName = "session_move_export" | "session_move_import";
interface MoveEvent {
  dir: string;        // 번들 루트 = move_session.bundleDir와 동일 문자열
  signal: "ready" | "done" | "cancel" | "error"; // 이동 단계 신호
}
```

- `event.dir`은 `move_session` 요청의 `bundleDir`과 정규화 후 완전 일치해야 한다(`path.resolve` 기준).
- 이벤트 발행 시 `ctx.sessionManager.getSessionFile()`가 반환한 경로가 `move_session.sessionPath` 및 `release_session` 응답의 `session_path`와 동일해야 한다.
- 첨부파일 매니페스트: `bundleDir/manifest.json`에 대해 아래 필드 존재를 단언한다.
  - `moveId: string` (요청 값과 동일)
  - `sessionPath: string`
  - `attachments: Array<{ path: string; size: number; sha256: string }>` — `path`는 `bundleDir` 기준 상대 경로, `size`는 바이트, `sha256`은 소문자 hex 64자.
  - `createdAt: string` (ISO-8601)
- 응답 프레임 성공: `{type:"response", id, success:true, data}`. 실패: `{type:"response", id, success:false, error:string, errorCode:string}`이며 `errorCode`가 `code`와 별개로 존재해야 파서가 예외 없이 처리한다.
- 미지원 확장 이름은 `rpc.rs:1094` 경로를 타 `Unknown command: <name>` 메시지와 함께 `success:false`로 반환되어야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

TS 측 단위 테스트(가칭 `omo-nyang/src/__tests__/session-move-extension.events.test.ts`)는 가짜 확장 API를 주입해 핸들러 등록과 이벤트 발행 순서를 캡처한다.

```ts
// 의사코드
const calls: Array<{ name: string; data?: unknown }> = [];
const events: Array<{ ev: string; payload: unknown }> = [];
const sessionFile = "/tmp/sess/abc.jsonl";
const api = {
  rpc: { handle: (name, fn) => registry.set(name, fn) },
  events: { on: (ev, fn) => subs.set(ev, fn), emit: (ev, p) => events.push({ ev, payload: p }) },
};
const ctx = { sessionManager: { getSessionFile: () => sessionFile } };
registerSessionMoveExtension(api, ctx); // session-move-extension.ts:116-139

// 1) prepare -> export
await dispatchExt("move_session", { op: "prepare", moveId: "m1", sessionPath: sessionFile, bundleDir: "/tmp/bundle/m1", cwd: "/w" });
await dispatchExt("move_session", { op: "export", moveId: "m1" });
assert.deepEqual(events.at(-1), { ev: "session_move_export", payload: { dir: "/tmp/bundle/m1", signal: "done" } });

// 2) 매니페스트 검증
const m = JSON.parse(fs.readFileSync("/tmp/bundle/m1/manifest.json", "utf8"));
assert.equal(m.moveId, "m1");
assert.equal(m.sessionPath, sessionFile);
for (const a of m.attachments) {
  assert.match(a.sha256, /^[0-9a-f]{64}$/);
  assert.equal(fs.statSync(path.join("/tmp/bundle/m1", a.path)).size, a.size);
}

// 3) import 이벤트 및 세션 파일 일치
await dispatchExt("move_session", { op: "import", moveId: "m1", bundleDir: "/tmp/bundle/m1" });
assert.deepEqual(events.at(-1), { ev: "session_move_import", payload: { dir: "/tmp/bundle/m1", signal: "done" } });
assert.equal(ctx.sessionManager.getSessionFile(), sessionFile);
```

Rust 측 회귀 테스트(가칭 `crates/omp-cli/tests/rpc_extension_response.rs`)는 `rpc.rs:593` 디스패치와 `rpc.rs:203-226` 프레임 조립을 직접 호출해 실패 프레임에 `errorCode`가 존재하는지 확인한다.

```rust
// 의사코드
let out = dispatch(&rt, &json!({"cmd":"extension_request","name":"omomeow.move-readiness","data":{"moveId":"m1"}}), "rpc").await;
let frame = response(&out); // rpc.rs:203-226
assert_eq!(frame["type"], "response");
assert_eq!(frame["success"], false);
assert!(frame["error"].is_string());
assert!(frame["errorCode"].is_string(), "MoveHost 파서는 errorCode를 요구한다");

let unknown = dispatch(&rt, &json!({"cmd":"extension_request","name":"no.such.handler"}), "rpc").await;
assert!(unknown.unwrap_err().message.contains("Unknown command:"));
```

순서 단언: `session_move_export`는 `release_session` 호출 이전에, `session_move_import`는 `get_state`의 `paused:false` 확인 이후에 관측되어야 한다. `wake_source_state` 구독은 이동 전후로 `{source, activeCount}` 형태를 유지해야 한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `session_move_export`/`session_move_import` 각 1회 이상 발행되고 payload가 정확히 `{dir, signal}` 2필드만 가진다.
- AC2: `bundleDir/manifest.json`의 `moveId`, `sessionPath`, `attachments[].{path,size,sha256}`, `createdAt`가 모두 존재하고 검증을 통과한다.
- AC3: `ctx.sessionManager.getSessionFile()` 값이 `move_session.sessionPath` 및 `release_session` 응답 `session_path`와 일치한다.
- AC4: 실패 응답 프레임이 `success:false` + `error` + `errorCode`를 동시에 포함하고 `move-host.ts:61` 파서가 throw하지 않는다.
- AC5: 미지원 확장 이름이 `Unknown command: ` 접두 메시지로 반환된다(`rpc.rs:1094`).
- AC6: `api.events.on("wake_source_state")` 구독이 이동 중 해제되지 않고 `{source, activeCount}` 수신을 유지한다.

```bash
pnpm vitest run omo-nyang/src/__tests__/session-move-extension.events.test.ts
cargo test -p omp-cli --test rpc_extension_response
cargo test -p omp-cli rpc::tests::response_frame
pnpm tsc -p omo-nyang --noEmit
```

모든 명령이 종료 코드 0이고 위 AC1~AC6이 각각 최소 1개 테스트 케이스로 매핑되어야 한다.

## 5. 엣지 케이스 및 실패 복구

- `op:"export"` 중 `op:"cancel"` 수신: `session_move_export`의 `signal`이 `"cancel"`로 발행되고 부분 생성된 `bundleDir`는 정리되며, 매니페스트는 남지 않아야 한다(테스트는 `manifest.json` 부재 단언).
- `bundleDir` 누락 또는 상대 경로: `success:false` + `errorCode` 반환, `move-host.ts:61` 파서가 예외 없이 실패를 표면화.
- 동일 `moveId` 재사용: 두 번째 `prepare`는 거부되고 기존 번들 무결성(sha256)이 변경되지 않아야 한다.
- `import` 시 매니페스트의 `attachments[].path`가 `bundleDir` 밖을 가리키는 경우(경로 탈출): 가져오기 거부 + `errorCode` 반환, 세션 파일 미변경.
- 매니페스트 `sha256` 불일치: `session_move_import`가 `signal:"error"`로 발행되고 `getSessionFile()` 값이 이동 전과 동일하게 유지된다.
- 확장 이름 미등록: `rpc.rs:1094` 경로로 `Unknown command: <name>` 반환, 호스트는 재시도 없이 실패 처리.
- `--multi-session --session-runtime in-process` 미적용 호스트(`move-host.ts:37` 위반): `release_session` 응답에 `session_path`가 없어 테스트가 즉시 실패하도록 단언.
- 비대화형 클라이언트에서 `interrupt({intent:"stop"})` 시도: `stop_requires_interactive_client`로 실패하고 이동 상태(`moveId`, `bundleDir`)는 보존되어 `op:"status"`로 복구 조회 가능해야 한다.
- capabilities 선언 후 역할 변경 시도: `connection_role_fixed`로 실패하며 이벤트 스트림에 중복 `session_move_export`가 발행되지 않는다.
