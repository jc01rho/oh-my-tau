# 문서 조각 03: ADR 초안 — `oh-my-tau` In-Process Multi-Session Host 및 무손실 세션 이동(`move_session`) 아키텍처 결정 기록 (doc-03-adr-multi-session-and-move)

## 1. 목적 및 코드 근거 (File & Line)

상태: Proposed. 범위: RPC 디스패치 확장, 클라이언트 역할 고정, 보류·재개, 세션 이동, CLI 플래그 정합, 종료 신호 정합. 결정일: 2026-11-22.

| ID | 결정 요지 | 근거 파일:줄 |
| --- | --- | --- |
| D1 | 단일 프로세스가 N개 세션을 소유하는 in-process 멀티세션 호스트로 전환 | `crates/omp-cli/src/rpc.rs:593`, `omo-nyang/src/relay/move-host.ts:37` |
| D2 | 실패 응답은 `code`와 `errorCode`를 함께 직렬화해야 소비자가 파싱 가능 | `crates/omp-cli/src/rpc.rs:203-226`, `omo-nyang/src/relay/move-host.ts:4-7, 61` |
| D3 | 보류·재개는 `set_client_info` → `interrupt` → `get_state` 3단 계약 | `omo-nyang/src/lane-hold.ts:29-76` |
| D4 | 이동은 `prepare`/`export`/`import`/`status`/`finish`/`cancel` 상태 머신 | `omo-nyang/src/relay/move-operations.ts:104-225` |
| D5 | `--skill <dir>`(반복 가능 루트)와 `--skills <CSV>`(필터)를 분리 | `omo-nyang/src/gateway.ts:1167`, `crates/omp-cli/src/cli.rs:129, 309` |
| D6 | 종료 신호 `agent_settled`를 RPC stdout으로 승격 | `crates/omp-cli/src/runtime.rs:1641`, `crates/omp-cli/src/ext.rs:1598` |
| D7 | 도구 격차(`task_send`/`task_output`/`task_cancel`, `bash_output`/`kill_bash`, `memory*`, `workpool*`)를 동일 릴리스에 편입 | `crates/omp-cli/src/tools/task.rs`, `crates/omp-cli/src/rpc.rs:863` |

미지원 명령의 현재 동작은 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`이며, 이 경로는 `errorCode`를 채우지 않아 D2를 위반한다.

## 2. 상세 계약 및 데이터/타입 명세

디스패치 진입점: `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` (`crates/omp-cli/src/rpc.rs:593`). 응답 조립은 `rpc.rs:203-226`의 `response()`가 담당하며 `MoveHost`(`move-host.ts:4-7, 61`)는 아래 형태만 수용한다.

```json
{"type":"response","id":"42","success":false,"error":"stop_requires_interactive_client","errorCode":"stop_requires_interactive_client"}
```

프로토콜/역할 계약(`lane-hold.ts:29-76`):

```ts
set_client_info({ width: 80, capabilities: ["interactive_user"] })   // 역할 고정
interrupt({ intent: "stop" | "abort" })                              // abort는 runPaused 유지
get_state() -> { paused: boolean, runPaused: boolean, pauseSeq: number }
get_protocol_info() -> { protocolVersion: 2, serverVersion: env!("CARGO_PKG_VERSION"),
                         engine: "oh-my-tau", commands: string[] }
reload() -> { cancelled: false }
resume_session() -> { paused: false, runPaused: false }  // session_resumed 이벤트 발행
```

세션 이동 계약(`move-operations.ts:104-225`, `session-move-extension.ts:116-139`):

```ts
move_session({ op: "prepare"|"export"|"import"|"status"|"finish"|"cancel",
               moveId: string, sessionPath?: string, bundleDir?: string,
               cwd?: string, from?: string, answer?: string })
release_session({ sessionId: string, reason: "move", moveId: string }) -> { session_path: string }
extension_request({ sessionId?: string, name: string, data?: unknown })   // api.rpc.handle로 라우팅
```

라우팅 대상 이름은 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`으로 고정한다. 확장 이벤트는 `session_move_export` / `session_move_import`(`event = { dir, signal }`, 세션 파일은 `ctx.sessionManager.getSessionFile()`)와 `api.events.on("wake_source_state", { source, activeCount })`를 사용한다. 호스트 기동 인자는 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`(`move-host.ts:37`)이며, 추가로 `--no-model-fallback`, `--no-recommended-models`(`model-routing.ts:654-658`)를 수용해야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/cli.rs:97-166 확장
#[arg(long = "skill", value_name = "DIR", action = ArgAction::Append)]
skill_paths: Vec<PathBuf>,           // 반복 가능한 추가 스킬 루트
#[arg(long = "skills", value_delimiter = ',')]
skills: Vec<String>,                 // 기존 CSV 필터 유지
#[arg(long = "multi-session")] multi_session: bool,
#[arg(long = "session-runtime", value_name = "MODE")] session_runtime: Option<String>,
#[arg(long = "no-model-fallback")] no_model_fallback: bool,
#[arg(long = "no-recommended-models")] no_recommended_models: bool,
```

```rust
// rpc.rs:1094 이전에 명시 arm 추가, CmdError에 error_code 필드 도입
"move_session" => move_session(rt, cmd).await.map_err(|e| e.with_code("move_failed")),
"release_session" => release_session(rt, cmd).await,
"extension_request" => route_extension(rt, cmd).await,   // omomeow.move-readiness 등
"resume_session" => resume(rt).await,                    // paused=false, runPaused=false, emit session_resumed
other => Err(CmdError::new(format!("Unknown command: {other}")).with_code("unknown_command")),
```

```rust
// 이동 상태 머신: prepare -> export -> import -> finish, 어느 단계에서든 cancel 허용
async fn move_session(rt: &Arc<Runtime>, cmd: &Value) -> Result<Value, CmdError> {
    let op = cmd["op"].as_str().ok_or(CmdError::bad_request("op"))?;
    let move_id = cmd["moveId"].as_str().ok_or(CmdError::bad_request("moveId"))?;
    let tx = rt.moves.begin(move_id, op)?;              // 멱등: 동일 (moveId, op) 재진입 시 기존 상태 반환
    match op {
        "export" => { let p = tx.export_bundle(cmd["bundleDir"].as_str())?;
                      rt.emit("session_move_export", json!({"dir": p, "signal": "exported"})); }
        "import" => { tx.import_bundle(cmd["bundleDir"].as_str())?;
                      rt.emit("session_move_import", json!({"dir": tx.dir(), "signal": "imported"})); }
        _ => tx.advance(op)?,
    }
    Ok(json!({ "moveId": move_id, "op": op, "state": tx.state() }))
}
```

종료 신호는 `runtime.rs:1641`의 `session_settled` 출력 지점에서 `agent_settled`를 추가 방출하고, `ext.rs:1598`의 확장 호스트 내부 전용 발행을 stdout 경로와 공유하도록 정리한다. 세션 기록은 `crates/omp-session/src/entries.rs`의 JSONL v3(256B title slot, camelCase 엔트리)를 유지하되 `session-patterns.ts:47-76`이 읽는 `{timestamp, message:{role, content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}` 필드를 보존한다. 설정 루트는 `~/.omp`(`crates/omp-settings/src/lib.rs`)를 기준으로 하고 `~/.omo/omo.jsonc` 프로필(`fleet-small`/`fleet-medium`)을 읽기 전용 오버레이로 병합한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-cli rpc::tests` 통과, 미지원 명령 응답에 `error`와 `errorCode`가 모두 존재.
2. `omt --mode rpc --multi-session --session-runtime in-process -e ./ext.js` 기동 후 3개 세션 동시 `get_state` 응답, 프로세스 수 1.
3. `interactive_user` 미선언 상태의 `interrupt({intent:"stop"})`가 `stop_requires_interactive_client`로 실패하고, 선언 후 역할 변경 시 `connection_role_fixed` 반환.
4. `move_session` 6개 op 왕복 후 `get_state`가 `paused:false, runPaused:false`, `release_session`이 `{session_path}` 반환.
5. `agent_settled` 1회 수신 후 하네스가 정상 종료(타임아웃 없음).

```bash
cargo test -p omp-cli -p omp-session -p omp-memory
cargo run -p omp-cli -- --mode rpc --multi-session --session-runtime in-process --skill ./skills/extra -e ./ext.js
bun test omo-nyang/src/relay/move-host.test.ts omo-nyang/src/lane-hold.test.ts
printf '%s\n' '{"id":"1","cmd":"get_protocol_info"}' | cargo run -p omp-cli -- --mode rpc | head -n1
```

## 5. 엣지 케이스 및 실패 복구

- 동일 `moveId` 재전송: `(moveId, op)` 멱등 키로 기존 상태를 반환하고 번들 재기록 금지. `import` 재실행 시 `sessionPath` 충돌이면 `move_conflict` + `errorCode`로 거부.
- `export` 도중 프로세스 종료: `bundleDir`에 `.partial` 접미사 잔존 → 다음 `status`가 `state:"incomplete"` 보고, `cancel`이 정리.
- `capabilities` 미
