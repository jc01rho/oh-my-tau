# 구현 조각 07: `crates/omp-cli/src/rpc.rs`·`crates/omp-ext/src/host.rs`의 `extension_request` 브리지 및 타임아웃 구현 명세 (impl-07-rpc-extension-request-bridge)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: OmOMeow 세션 이동 확장이 `extension_request({sessionId?, name, data?})`로 확장 핸들러를 호출하는 경로를 tau RPC에 개통한다. 현재 `crates/omp-cli/src/rpc.rs:593`의 `dispatch`에는 해당 명령 분기가 없어 `rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어지고, `omo-nyang/src/relay/move-host.ts:61`은 `errorCode`를 읽으므로 code만으로는 복구 분기가 불가능하다.
- 근거 1: 응답 조립은 `crates/omp-cli/src/rpc.rs:203-226`의 `response()`가 담당하며 스키마는 `{type:"response", id, success, data?, error?, errorCode?}`이다(`omo-nyang/src/relay/move-host.ts:4-7, 61`가 동일 스키마를 파싱).
- 근거 2: 확장 라우팅 계약은 `omo-nyang/src/session-move-extension.ts:116-139`의 `api.rpc.handle(name, handler)`이며 대상 이름은 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`이다(`omo-nyang/src/move-operations.ts:104-225`).
- 근거 3: 호스트 실행 형태는 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`(`omo-nyang/src/relay/move-host.ts:37-39`)이며 `--session-runtime in-process`는 확장이 같은 프로세스에 로드됨을 의미한다.
- 근거 4: 기능 탐지용 `get_protocol_info()`는 `{protocolVersion:2, serverVersion, engine:"oh-my-tau", commands:[...]}`를 반환하므로(`omo-nyang/src/lane-hold.ts:29-76`), `commands`에 `extension_request`를 추가해야 클라이언트가 분기할 수 있다.
- 근거 5: 확장 이벤트(`session_move_export`/`session_move_import`, `wake_source_state`)는 `api.events.on`으로 구독되므로, 요청 브리지는 이벤트 버스와 동일한 쓰기 경로(단일 라인 JSON 락)를 공유해야 프레임이 섞이지 않는다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 요청 파라미터 (`params`)

```json
{"sessionId":"<string|null>","name":"omomeow.move-readiness","data":{},"timeoutMs":30000}
```

- `name`: 필수, `^[a-zA-Z0-9_.-]{1,128}$`.
- `sessionId`: 선택. 지정 시 `release_session`/`move_session`이 다루는 살아있는 세션 ID와 일치해야 한다.
- `data`: 선택, 임의 JSON. 직렬화 후 1 MiB 초과 시 거부.
- `timeoutMs`: 선택, 기본 30000, 허용 범위 100..=600000으로 클램프.

### 2.2 성공/실패 응답 (rpc.rs:203-226 경유)

```json
{"type":"response","id":17,"success":true,"data":{"name":"...","result":{}}}
{"type":"response","id":18,"success":false,"code":"extension_timeout","errorCode":"extension_timeout","error":"extension request timed out after 30000ms"}
```

### 2.3 확장 호스트 왕복 프레임 (stdin/stdout JSONL, `host.rs`)

```json
{"type":"extension_request","requestId":42,"sessionId":null,"name":"omo.feedback.prepare","data":{}}
{"type":"extension_response","requestId":42,"success":true,"data":{}}
{"type":"extension_response","requestId":42,"success":false,"error":"boom","errorCode":"extension_handler_error"}
```

### 2.4 오류 코드 표

| errorCode | 발생 조건 | 재시도 |
|---|---|---|
| `invalid_params` | name 누락/정규식 위반, data 1 MiB 초과 | 불가 |
| `extension_not_found` | `api.rpc.handle` 미등록 이름 | 불가(등록 대기 후 1회 재시도 허용) |
| `extension_timeout` | timeoutMs 초과 | 가능 |
| `extension_handler_error` | 핸들러 throw/거부 | 조건부 |
| `extension_host_unavailable` | 확장 호스트 미기동/재시작 중 | 가능 |
| `session_not_found` | sessionId 불일치 | 불가 |

### 2.5 Rust 타입 (`crates/omp-ext/src/host.rs`)

```rust
pub struct ExtensionRequest { pub session_id: Option<String>, pub name: String, pub data: Option<serde_json::Value>, pub timeout: Duration }
pub struct ExtensionResponse { pub data: Option<serde_json::Value> }
#[derive(Debug)] pub enum ExtBridgeError { InvalidParams(String), NotFound(String), Timeout(u64), Handler(String), HostUnavailable, SessionNotFound(String) }
impl ExtBridgeError { pub fn error_code(&self) -> &'static str { /* 2.4 표 매핑 */ } }
pub struct ExtensionBridge { next_id: AtomicU64, pending: Mutex<HashMap<u64, oneshot::Sender<Result<ExtensionResponse, ExtBridgeError>>>>, writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>> }
impl ExtensionBridge { pub async fn call(&self, req: ExtensionRequest) -> Result<ExtensionResponse, ExtBridgeError>; pub fn complete(&self, request_id: u64, res: Result<ExtensionResponse, ExtBridgeError>) -> bool; pub fn fail_all(&self, err: ExtBridgeError) -> usize; }
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

### 3.1 `crates/omp-cli/src/rpc.rs` — dispatch 분기 추가 (`rpc.rs:1094` 앞)

```rust
"extension_request" => {
    let p: ExtensionRequestParams = serde_json::from_value(cmd.get("params").cloned().unwrap_or_default())
        .map_err(|e| CmdError::new(format!("invalid_params: {e}")))?;
    let req = p.into_request()?;                       // name 정규식/용량/timeout 클램프 검증
    if let Some(sid) = &req.session_id { rt.ensure_session(sid)?; }
    match rt.ext_bridge().call(req).await {
        Ok(r)  => Ok(json!({"name": p.name, "result": r.data})),
        Err(e) => Err(CmdError::with_code(e.error_code(), e.to_string())),
    }
}
```

- `CmdError::with_code`는 `response()`(`rpc.rs:203-226`)에서 `code`와 `errorCode`를 동시에 직렬화하도록 확장한다(두 필드 모두 동일 문자열).
- `get_protocol_info()`의 `commands` 배열에 `"extension_request"`를 추가한다.

### 3.2 `crates/omp-ext/src/host.rs` — 요청 등록/대기/정리

```rust
pub async fn call(&self, req: ExtensionRequest) -> Result<ExtensionResponse, ExtBridgeError> {
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    { let mut g = self.pending.lock().await;
      if g.len() >= 256 { return Err(ExtBridgeError::HostUnavailable); }   // 백프레셔
      g.insert(id, tx); }
    let frame = json!({"type":"extension_request","requestId":id,"sessionId":req.session_id,"name":req.name,"data":req.data});
    if let Err(e) = self.write_frame(&frame).await { self.pending.lock().await.remove(&id); return Err(ExtBridgeError::HostUnavailable); }
    match tokio::time::timeout(req.timeout, rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_))  => Err(ExtBridgeError::HostUnavailable),           // 채널 드롭 = 호스트 종료
        Err(_) => { self.pending.lock().await.remove(&id); Err(ExtBridgeError::Timeout(req.timeout.as_millis() as u64)) }
    }
}
```

- `write_frame`은 `writer` 뮤텍스 안에서 개행 1회 포함 전체 라인을 한 번에 `write_all` 한다(이벤트 프레임과 인터리브 방지).
- 호스트 stdout 리더는 `extension_response` 수신 시 `complete(request_id, ...)`를 호출하고, 미존재 ID면 `tracing::warn!` 후 폐기(늦은 응답).
- 호스트 종료/재시작 훅에서 `fail_all(ExtBridgeError::HostUnavailable)`로 대기자를 전부 깨운다.
- 클라이언트 연결 종료 시 해당 커넥션의 in-flight ID를 수집해 `pending`에서 제거(누수 방지).

### 3.3 확장(TS) 측 대응 (`session-move-extension.ts:116-139` 형태 유지)

```ts
api.rpc.handle("omomeow.move-readiness", async (data, ctx) => ({ ready: true, dir: ctx.sessionManager.getSessionFile() }));
// 브리지는 name을 그대로 전달하고, 반환값을 {"result": ...}로 감싼다.
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `extension_request`가 `rpc.rs:1094`의 Unknown command로 떨어지지 않고, `commands` 목록에 노출된다.
2. 정상 호출 시 `{type:"response", id, success:true, data:{name,result}}` 1프레임 반환, 3개 동시 호출 시 requestId별 응답이 교차하지 않는다.
3. 미등록 이름은 `success:false`, `code`와 `errorCode`가 모두 `extension_not_found`.
4. `timeoutMs:500`에서 500ms +200ms 이내 `extension_timeout` 반환, 반환 직후 `pending` 맵 크기 0.
5. 확장 호스트 kill 시 in-flight 요청이 `extension_host_unavailable`로 즉시 실패(행 없음).
6. `get_protocol_info()`가 `protocolVersion:2`, `engine:"oh-my-tau"`, `commands`에 `extension_request` 포함.

```bash
cargo build -p omp-cli -p omp-ext
cargo test -p omp-ext --test extension_bridge -- --nocapture
cargo test -p omp-cli --test rpc_dispatch extension_request
cargo clippy -p omp-cli -p omp-ext --all-targets -- -D warnings
npx tsx omo-nyang/scripts/probe-extension-request.ts \
  --bin ./target/debug/omt --ext ./dist/session-move-extension.js \
  --cases not_found,timeout,concurrent3,host_kill
```

## 5. 엣지 케이스 및 실패 복구

- 타임아웃 후 늦은 `extension_response`: `complete()`가 `false`를 반환하면 로그만 남기고 폐기. 패닉 금지.
- 확장이 동일 `requestId`를 두 번 응답: 첫 응답만 반영, 두 번째는 `extension_handler_error` 로그.
- 핸들러가 등록되기 전 요청(확장 로딩 경쟁): `extension_not_found` 반환 시 클라이언트가 최대 2회, 100ms 간격 재시도. 서버는 재시도 카운트를 요청에 싣지 않고 무상태 유지.
- `sessionId`가 `release_session({sessionId, reason:"move", moveId})`로 이미 해제된 세션: `session_not_found`로 즉시 실패하여
