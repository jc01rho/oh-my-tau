# QA 시나리오 03: `MoveHost`(`--multi-session --session-runtime in-process`) 기반 장비 간 세션·첨부파일·워크트리 이동 E2E 검증 (qa-03-cross-machine-session-move)

## 1. 목적 및 코드 근거 (File & Line)

장비 A에서 장비 B로 세션 JSONL, 첨부파일, git 워크트리를 `MoveHost` 경유로 이동시키는 전 과정을 RPC 계약 수준에서 검증한다. 검증 대상은 (1) 호스트 기동 플래그, (2) `move_session` 상태기계, (3) `release_session`/`extension_request` 라우팅, (4) 확장 이벤트 발행, (5) 종료 신호 정합성이다.

- 호스트 기동: `omo-nyang/src/relay/move-host.ts:37-39`가 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`를 spawn. 인자 파싱은 `crates/omp-cli/src/cli.rs:97-166`에서 수용해야 하며 현재 `--multi-session`, `--session-runtime in-process`, `-e`만 존재.
- 디스패치: `crates/omp-cli/src/rpc.rs:593 async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094 other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 응답 프레임: `rpc.rs:203-226 response()`가 조립. `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 동시 직렬화해야 파서가 깨지지 않는다.
- 이동 오퍼레이션: `omo-nyang/src/relay/move-operations.ts:104-225`가 `prepare|export|import|status|finish|cancel`을 순차 호출. 확장 측 수신은 `omo-nyang/src/session-move-extension.ts:116-139`.
- 보류/재개: `omo-nyang/src/lane-hold.ts:29-76`의 `set_client_info` → `interrupt({intent})` → `get_state` → `resume_session` 순서.
- 종료 신호 격차: `omo-nyang/src/relay/runtime.ts:59`는 stdout에서 `{"type":"agent_settled"}`를 대기하나 tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `session_settled`만 stdout에 쓰고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 발행. 본 시나리오는 이 격차를 회귀 조건으로 고정한다.
- 설정 루트 격차: tau는 `~/.omp`(`crates/omp-settings/src/lib.rs`), 플릿은 `~/.omo`(`omo.jsonc`, 프로필 `fleet-small`/`fleet-medium`). 이동 번들에는 `~/.omp` 기준 상대경로만 기록한다.

## 2. 상세 계약 및 데이터/타입 명세

응답 프레임(모든 RPC 공통, `rpc.rs:203-226`):

```json
{"type":"response","id":"<uuid>","success":true,"data":{}}
{"type":"response","id":"<uuid>","success":false,"error":"...","errorCode":"unknown_command"}
```

`move_session` 요청/응답 (`move-operations.ts:104-225`):

```json
{"cmd":"move_session","op":"prepare","moveId":"mv_01","sessionPath":"/home/u/.omp/sessions/x.jsonl","bundleDir":"/tmp/mv_01","cwd":"/work/repo","from":"host-a","answer":null}
{"cmd":"move_session","op":"status","moveId":"mv_01"} -> {"phase":"exported|imported|finished","bytes":12345,"files":7}
```

- `op="export"`: `bundleDir`에 `session.jsonl`, `attachments/`, `worktree/`, `manifest.json`(각 파일 sha256, `sessionId`, `from`) 생성.
- `op="import"`: `manifest.json` 검증 후 세션 재구성, `worktree/`를 `cwd`에 복원.
- `op="finish"`: 원본 측 정리, `op="cancel"`: 부분 산출물 삭제 후 `{cancelled:true}`.

릴리스/확장 라우팅:

```json
{"cmd":"release_session","sessionId":"s_1","reason":"move","moveId":"mv_01"} -> {"session_path":"/home/u/.omp/sessions/x.jsonl"}
{"cmd":"extension_request","sessionId":"s_1","name":"omomeow.move-readiness","data":{}}
```

`extension_request`는 `api.rpc.handle(name, handler)`로 라우팅되며 허용 이름은 `omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit` 3종으로 화이트리스트 검증한다. 미등록 이름은 `errorCode:"extension_not_found"`.

확장 이벤트(`session-move-extension.ts:116-139`): `session_move_export`/`session_move_import`는 `event={dir, signal}`을 받고 `ctx.sessionManager.getSessionFile()`로 대상 파일을 확정한다. `api.events.on("wake_source_state", {source, activeCount})`도 수신측에서 관측 가능해야 한다.

프로토콜/보류 계약(`lane-hold.ts:29-76`):

```json
{"cmd":"set_client_info","width":80,"capabilities":["interactive_user"]}
{"cmd":"interrupt","intent":"stop"}  // 또는 "abort"
{"cmd":"get_state"} -> {"paused":true,"runPaused":true,"pauseSeq":3}
{"cmd":"resume_session"} -> 이벤트 session_resumed 발행, 이후 get_state는 paused:false, runPaused:false
{"cmd":"get_protocol_info"} -> {"protocolVersion":2,"serverVersion":"<CARGO_PKG_VERSION>","engine":"oh-my-tau","commands":[...]}
```

세션 기록 정합: tau는 `crates/omp-session/src/entries.rs`의 JSONL v3(256B title slot + camelCase entries)를 쓰고, `omo-nyang/src/session-patterns.ts:47-76`은 각 줄에서 `{timestamp, message:{role, content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}`를 읽는다. 이동 후에도 두 파서가 동일 라인 수를 보아야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust 측 변경:

```rust
// crates/omp-cli/src/cli.rs:97-166
pub struct Args { /* 기존 */ pub skill_paths: Vec<PathBuf>, pub multi_session: bool, pub session_runtime: String }
// --skill <DIR> 반복 허용(gateway.ts:1167이 디렉터리 경로 전달), 기존 --skills <CSV필터>(cli.rs:129,309)와 별개 필드
```

```rust
// crates/omp-cli/src/rpc.rs:593 dispatch 내 신규 arm
"move_session"     => handle_move_session(rt, cmd).await,
"release_session"  => handle_release_session(rt, cmd).await,
"extension_request"=> handle_extension_request(rt, cmd).await,
// rpc.rs:203-226 response(): 실패 시 {"error": msg, "errorCode": code} 동시 기록
```

```rust
// crates/omp-cli/src/runtime.rs:1641 부근: session_settled 직후 agent_settled도 stdout에 발행
emit_json(&json!({"type":"session_settled"}));
emit_json(&json!({"type":"agent_settled"})); // move-host.ts / relay/runtime.ts:59 대기 신호
```

TS 하네스 의사코드:

```ts
const host = spawnMoveHost({ ext, args: ["--mode","rpc","--multi-session","--session-runtime","in-process","-e",ext] });
await rpc("set_client_info", { width: 80, capabilities: ["interactive_user"] });
await rpc("interrupt", { intent: "stop" });
const st = await rpc("get_state"); assert(st.paused && st.runPaused);
await rpc("move_session", { op: "prepare", moveId, sessionPath, bundleDir, cwd, from: "host-a" });
await rpc("move_session", { op: "export", moveId });
assertManifest(bundleDir);            // sha256, sessionId, attachments, worktree
await rpc("release_session", { sessionId, reason: "move", moveId });
await rpc("extension_request", { sessionId, name: "omomeow.move-readiness" });
await rpc("move_session", { op: "import", moveId });   // 장비 B 측
await rpc("move_session", { op: "finish", moveId });
await rpc("resume_session"); assert((await rpc("get_state")).paused === false);
assert(await waitForStdout('{"type":"agent_settled"}'));
```

검증 순서: `get_protocol_info`로 `protocolVersion:2`, `engine:"oh-my-tau"` 확인 → export 번들 무결성 → import 후 `session-patterns.ts:47-76` 파서로 assistant/toolResult 라인 재파싱 → worktree `git status --porcelain` 동일 → `reload()`가 `{cancelled:false}` 반환하는지 확인.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-cli --test rpc_move` 전 케이스 통과: `move_session` 6개 op, `release_session`, `extension_request` 화이트리스트 3종.
2. `omt --mode rpc --multi-session --session-runtime in-process -e ./ext/dist/index.js`가 exit code 0으로 기동하고 `get_protocol_info` 응답의 `commands`에 `move_session`, `release_session`, `extension_request`, `resume_session`, `reload`, `set_client_info`, `interrupt`, `get_state`가 모두 포함.
3. `pnpm -C omo-nyang test -- move-host move-operations` 통과, stdout에서 `agent_settled` 1회 이상 관측.
4. 번들 검증: `jq -e '.files|length>0' $BUNDLE/manifest.json`, `sha256sum -c` 전 항목 OK, `diff <(wc -l < $BUNDLE/session.jsonl) <(wc -l < $RESTORED/session.jsonl)` 무차이.
5. 실패 프레임 계약: 미지원 명령에 대해 `success:false`와 `errorCode`가 동시에 존재(`jq -e '.errorCode!=null'`)해야 하며 `error`는 `Unknown command: ` 접두사를 유지.
6. `--skill ./skills/extra` 반복 지정 시 `Args::skill_paths.len()==N`이고 스킬 로딩 목록에 N개 루트가 반영, 기존 `--skills a,b` CSV 필터 동작 불변.

## 5. 엣지 케이스 및 실패 복구

- 미지원/오타 op: `move_session`의 `op`가 6종 밖이면 `errorCode:"invalid_move_op"`, 상태기계 위반(예: `import` 전 `finish`)은 `errorCode:"invalid_move_phase"`. `cancel`은 어느 단계에서든 허용하고 부분 산출물을 삭제한 뒤 `{cancelled:true}`.
- 권한/역할 고정: `interactive_user` 미선언 상태의 `interrupt`는 `stop_requires_interactive_client`, 선언 후 역할 변경 시도는 `connection_role_fixed`로 실패해야 하며 두 경우 모두 `errorCode`가 프레임에 실려야 한다.
- 종료 신호 타임아웃: `agent_settled`가 오지 않으면 하네스는 `session_settled` 수신 후 유예 시간을 두고 재확인하고, 그래도 없으면 실패로 기록(무한 대기 금지).
- 번들 손상: `manifest.json` 부재/해시 불일치 시 `import`는 `errorCode:"bundle_corrupt"`로 중단하고 기존 세션 파일을 건드리지 않는다(원자적 rename 후 교체).
- 경로 이탈: `bundleDir`/`cwd`가 허용 루트(`~/.omp`) 밖이거나 `..` 포함 시 `errorCode:"path_escape"` 거부.
- 세션 파일 부재: `release_session` 시 `sessionPath`가 없으면 `{session_path:null}`과 경고를 반환하고 `move_session`은 `cancel`로 정리 가능해야 한다
