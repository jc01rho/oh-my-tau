# 구현 조각 09: `ext-host/host.ts`·`crates/omp-ext/src/lifecycle.rs`의 `api.rpc.handle`, `wake_source_state`, `session_move_export`/`import` 구현 명세 (impl-09-ext-host-rpc-handle-and-events)

## 1. 목적 및 코드 근거 (File & Line)

이 조각의 목적은 OmO 하네스(omo-nyang)가 tau RPC 호스트에 요구하는 확장 표면 4종을 실제로 제공하는 것이다. 즉 (a) `extension_request` → `api.rpc.handle` 라우팅, (b) `wake_source_state` 이벤트, (c) `session_move_export` / `session_move_import` 확장 이벤트, (d) 실패 응답의 `code` + `errorCode` 동시 직렬화다.

- `omo-nyang/src/relay/move-host.ts:4-7, 61` — 응답 프레임을 `{type:"response", id, success, data?, error?, errorCode?}`로 파싱한다. 따라서 실패 시 `errorCode`가 누락되면 하네스는 원인 분기(`stop_requires_interactive_client`, `connection_role_fixed` 등)를 잃는다.
- `omo-nyang/src/relay/move-host.ts:37-39` — 호스트 기동: `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`.
- `omo-nyang/src/move-operations.ts:104-225` — `move_session` op 집합: `prepare`|`export`|`import`|`status`|`finish`|`cancel`, 인자 `moveId, sessionPath, bundleDir, cwd, from, answer`.
- `omo-nyang/src/session-move-extension.ts:116-139` — 확장이 `session_move_export` / `session_move_import`를 구독하고 `event={dir, signal}`, `ctx.sessionManager.getSessionFile()`로 세션 파일을 얻는다.
- `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info` → `interrupt` → `get_state` 순서, `interactive_user` 미선언 시 `stop_requires_interactive_client`, 역할 변경 시 `connection_role_fixed`.
- `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`가 단일 진입점. `rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`가 미지원 명령 처리.
- `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 프레임을 조립하므로 `errorCode` 직렬화 지점은 여기다.
- `crates/omp-cli/src/rpc.rs:863` — `get_subagents`가 빈 배열 스텁. 확장 이벤트 경로와 동일한 레지스트리로 대체 검토 대상.
- `crates/omp-ext/src/lifecycle.rs` — 확장 수명주기(로드/활성/정지)와 이벤트 브로드캐스트 지점. `crates/omp-cli/src/ext.rs:1598`은 `agent_settled`를 확장 호스트 내부로만 보내므로, 본 조각은 그 내부 채널을 `wake_source_state`까지 확장한다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 Rust 측 (`crates/omp-ext/src/lifecycle.rs`)

```rust
pub trait RpcHandler: Send + Sync {
    fn handle(&self, ctx: &ExtCtx, data: Option<serde_json::Value>) -> Result<serde_json::Value, CmdError>;
}

pub struct RpcRegistry { handlers: std::collections::HashMap<String, std::sync::Arc<dyn RpcHandler>> }
impl RpcRegistry {
    pub fn register(&mut self, name: &str, h: std::sync::Arc<dyn RpcHandler>) -> Result<(), CmdError>;
    pub fn dispatch(&self, ctx: &ExtCtx, name: &str, data: Option<Value>) -> Result<Value, CmdError>;
}

pub enum WakeSource { Relay, Move, Feedback, User }
pub struct WakeSourceState { pub source: String, pub active_count: u32 }

pub struct ExtCtx {
    pub session_id: Option<String>,
    pub session_file: Option<std::path::PathBuf>, // ctx.sessionManager.getSessionFile() 대응
    pub lifecycle: Arc<LifecycleState>,
}
```

이벤트 페이로드(직렬화 형태 고정):

- `wake_source_state`: `{"type":"event","event":"wake_source_state","data":{"source":"relay|move|feedback|user","activeCount":0}}`
- `session_move_export`: `{"type":"event","event":"session_move_export","data":{"dir":"<bundleDir>","signal":"export_ready","sessionFile":"<path>"}}`
- `session_move_import`: 동일 스키마에 `"signal":"import_ready"`.

### 2.2 RPC 메서드 스키마 (`extension_request`로 도달)

| name | 요청 data | 성공 응답 data |
|---|---|---|
| `omomeow.move-readiness` | `{moveId, sessionPath}` | `{ready:boolean, blockers:[{code,detail}], sessionPath, pendingWrites:number}` |
| `omo.feedback.prepare` | `{moveId, cwd}` | `{ok:true, dir:"<bundleDir>", token:"<uuid>"}` |
| `omo.feedback.submit` | `{moveId, token, answer}` | `{ok:true, receipt:"<id>", answer}` |
| `move_session` | `{op, moveId, sessionPath?, bundleDir?, cwd?, from?, answer?}` | op별: `prepare`→`{moveId, staged:true}`, `export`→`{bundleDir, bytes}`, `import`→`{sessionPath}`, `status`→`{op,state}`, `finish`→`{moveId, done:true}`, `cancel`→`{moveId, cancelled:true}` |
| `release_session` | `{sessionId, reason:"move", moveId}` | `{session_path:"<abs path>"}` |
| `set_client_info` | `{width:80, capabilities:["interactive_user"]}` | `{ok:true}` |
| `interrupt` | `{intent:"stop"\|"abort"}` | `{ok:true, intent}` |
| `get_state` | `{}` | `{paused:boolean, runPaused:boolean, pauseSeq:number}` |
| `resume_session` | `{}` | `{ok:true}` + `session_resumed` 이벤트 |
| `get_protocol_info` | `{}` | `{protocolVersion:2, serverVersion:env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}` |
| `reload` | `{}` | `{cancelled:false}` |

### 2.3 TS 측 (`ext-host/host.ts`)

```ts
export interface ExtApi {
  rpc: { handle(name: string, h: (data?: unknown, ctx: ExtCtx) => Promise<unknown> | unknown): void };
  events: { on(name: string, cb: (data: unknown) => void): void };
}
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

**Rust: 응답 프레임에 errorCode 강제 포함 (`rpc.rs:203-226` 수정)**

```rust
fn response(id: Value, r: CmdResult) -> Value {
    match r {
        Ok(data) => json!({"type":"response","id":id,"success":true,"data":data}),
        Err(e)  => json!({"type":"response","id":id,"success":false,
                          "error":e.message, "errorCode":e.code}),   // 둘 다 직렬화
    }
}
```

`CmdError::new`는 기존 호출부 호환을 위해 `code`를 선택 인자로 두고, 미지정 시 `"unknown_command"`(`rpc.rs:1094` 경로) 또는 `"internal"`로 채운다.

**Rust: `extension_request` 분기 (`rpc.rs:593` dispatch 내 추가)**

```rust
"extension_request" => {
    let name = cmd["name"].as_str().ok_or(CmdError::new_code("bad_request","name required"))?;
    let ctx = rt.ext_ctx(cmd["sessionId"].as_str());        // sessionId 없으면 활성 세션
    rt.ext.rpc_registry().dispatch(&ctx, name, cmd.get("data").cloned())
        .map_err(|e| e.with_session(ctx.session_id.clone()))
}
```

**Rust: `release_session` (`reason:"move"` 고정 검증)**

```rust
"release_session" => {
    if cmd["reason"].as_str() != Some("move") { return Err(CmdError::new_code("bad_request","reason must be move")); }
    let path = rt.sessions.release(cmd["sessionId"].as_str().unwrap(), cmd["moveId"].as_str())?;
    Ok(json!({"session_path": path.to_string_lossy()}))
}
```

**Rust: 이벤트 브로드캐스트 (`lifecycle.rs`)**

```rust
pub fn emit_move(&self, kind: MoveEvent, dir: &Path, signal: &str) -> Result<(), CmdError> {
    let name = match kind { MoveEvent::Export => "session_move_export", MoveEvent::Import => "session_move_import" };
    self.broadcast(name, json!({"dir": dir, "signal": signal, "sessionFile": self.session_file()}))
}
pub fn set_wake_source(&self, src: WakeSource, active: u32) {
    self.broadcast("wake_source_state", json!({"source": src.as_str(), "activeCount": active}));
}
```

`set_wake_source`는 (i) `interrupt` 수신 시 `Move/1`, (ii) `move_session` op 시작 시 `Move/1`, (iii) `finish`/`cancel` 시 `activeCount:0`으로 호출한다. 디바운스는 50ms, 동일 페이로드 중복 억제.

**TS: 확장 측 등록 (`ext-host/host.ts`)**

```ts
api.rpc.handle("omomeow.move-readiness", async (data, ctx) => {
  const sessionPath = ctx.sessionManager.getSessionFile();
  const blockers = await collectBlockers(sessionPath, data?.moveId);
  return { ready: blockers.length === 0, blockers, sessionPath, pendingWrites: await pendingWrites(sessionPath) };
});
api.events.on("session_move_export", async ({ dir, signal }) => {
  await writeBundle(dir, ctx.sessionManager.getSessionFile(), signal);
});
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-ext lifecycle::tests::rpc_handle_roundtrip` 통과: 등록되지 않은 이름 호출 시 `success:false`, `errorCode:"unknown_rpc_handler"`.
2. `cargo test -p omp-cli rpc::tests::error_code_serialized` 통과: 실패 프레임에 `error`와 `errorCode`가 동시에 존재.
3. `cargo test -p omp-cli rpc::tests::release_session_move` 통과: 응답이 `{"session_path": "<abs>"}`.
4. `bun test omo-nyang/src/relay/move-host.test.ts` 통과: prepare→export→import→finish 전 구간에서 `success:true` 프레임 수신.
5. 수동: `omt --mode rpc --multi-session --session-runtime in-process -e ./ext/host.ts` 실행 후 stdin에 `{"id":1,"cmd":"get_protocol_info"}` 주입 → `protocolVersion:2`, `engine:"oh-my-tau"` 확인.
6. 수동: `extension_request`로 `omomeow.move-readiness` 호출 시 `ready`, `blockers`, `sessionPath` 3필드 존재.
7. `wake_source_state` 수신 로그에 `{"source":"move","activeCount":1}` → `{"source":"move","activeCount":0}` 순서 기록.
8. `cargo clippy -p omp-ext -p omp-cli -- -D warnings` 무경고.

## 5. 엣지 케이스 및 실패 복구

- **핸들러 미등록**: `dispatch`가 `unknown_rpc_handler` 코드로 실패. 하네스는 `errorCode`로 분기하고 재시도하지 않는다(`move-host.ts:61` 계약).
- **확장 로드 실패/미기동**: `extension_request`는 `extension_unavailable`로 즉시 실패해야 하며, `in-process` 런타임이면 5초 타임아웃 후 `extension_timeout`.
- **`sessionId` 불일치**: `release_session`의 `sessionId`가 활성 세션과 다르면 `session_mismatch`, 세션 파일은 건드리지 않는다.
- **번들 디렉터리 쓰기 실패**: `export`는 부분 산출물을 `bundleDir/.partial`에 두고 실패 시 삭제, `status`는 `state:"failed"` 반환. `finish`는 `.partial` 잔존 시 `in
