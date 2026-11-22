# 구현 조각 01: `crates/omp-cli/src/rpc.rs`·`runtime.rs`의 `set_client_info` 상태 저장 및 역할 고정(`connection_role_fixed`) 구현 명세 (impl-01-rpc-set-client-info)

## 1. 목적 및 코드 근거 (File & Line)

- `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. `cmd`의 `"cmd"`/`"op"` 문자열로 분기하는 단일 진입점이며, 본 조각은 여기에 `set_client_info`/`get_state`/`interrupt` arm을 추가·보강한다.
- `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 현재 `CmdError`는 코드 필드가 없어 `errorCode`를 실을 수 없다.
- `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?}` 프레임을 조립한다. 여기에 `errorCode?`를 추가해야 한다.
- `omo-nyang/src/relay/move-host.ts:4-7, 61` — `MoveHost`가 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 응답은 `code`와 `errorCode`를 동시에 직렬화해야 한다(둘 중 하나만 있으면 클라이언트가 원인 분기를 못 한다).
- `omo-nyang/src/lane-hold.ts:29-76` — 보류·재개 계약: `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 상태의 `stop`은 `stop_requires_interactive_client`, 선언 후 역할이 바뀌면 `connection_role_fixed`.
- `crates/omp-cli/src/runtime.rs:1641` — `{"type":"session_settled"}` stdout 방출 지점. 클라이언트 정보는 세션 수명과 무관하게 연결 수명에 귀속되므로 `reload()`/`resume_session()`에서 초기화하지 않는다는 규칙을 여기서 교차 확인한다.

목적: 클라이언트가 자기 능력을 1회 선언하면 그 연결의 역할(Observer/InteractiveUser)이 고정되고, 이후 능력 집합이 역할을 바꾸는 방향으로 변이할 때 원자적으로 거부하며, 그 실패를 `errorCode:"connection_role_fixed"`로 전달한다.

## 2. 상세 계약 및 데이터/타입 명세

요청 `set_client_info`:

```json
{"type":"request","id":"r1","cmd":"set_client_info","width":80,"capabilities":["interactive_user"]}
```

성공 응답 `data`:

```json
{"width":80,"capabilities":["interactive_user"],"role":"interactive_user","roleFixed":true}
```

실패 응답(역할 고정 위반):

```json
{"type":"response","id":"r1","success":false,
 "error":"connection role already fixed to interactive_user",
 "errorCode":"connection_role_fixed"}
```

`interrupt` 요청 `{"intent":"stop"|"abort"}` — `intent`는 두 값만 허용, 그 외는 `invalid_intent`. `get_state` 응답은 기존 필드에 `role`을 추가한다: `{paused, runPaused, pauseSeq, role}`.

Rust 타입(`crates/omp-cli/src/runtime.rs`):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo { pub width: u16, pub capabilities: Vec<String> }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRole { Observer, InteractiveUser }

impl ConnectionRole {
    pub fn as_str(self) -> &'static str { /* "observer" | "interactive_user" */ }
    pub fn from_capabilities(caps: &[String]) -> Self {
        if caps.iter().any(|c| c == "interactive_user") { Self::InteractiveUser } else { Self::Observer }
    }
}

#[derive(Debug, Default)]
pub struct ClientState { pub info: Option<ClientInfo>, pub role: Option<ConnectionRole> }
```

`Runtime`에 단일 락 필드 `client: std::sync::Mutex<ClientState>`를 추가한다(정보와 역할을 분리된 락 2개로 두면 검사-쓰기 사이 TOCTOU가 생기므로 반드시 하나의 `Mutex`).

`CmdError`(`crates/omp-cli/src/rpc.rs`)는 기존 `CmdError::new(msg)` 시그니처를 유지하고 `CmdError::coded(code: &'static str, msg: impl Into<String>)`를 신설한다(1094 포함 기존 호출부 다수 무수정). `code: Option<&'static str>` 필드를 갖고, `response()`는 `code`가 `Some`일 때만 `errorCode`를 넣는다.

오류 코드 표: `connection_role_fixed`, `stop_requires_interactive_client`, `invalid_intent`, `invalid_capabilities`, `invalid_width`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`runtime.rs`:

```rust
impl Runtime {
    pub fn set_client_info(&self, info: ClientInfo) -> Result<(ClientInfo, ConnectionRole), CmdError> {
        if info.width == 0 { return Err(CmdError::coded("invalid_width", "width must be > 0")); }
        let mut st = self.client.lock().unwrap();
        let next = ConnectionRole::from_capabilities(&info.capabilities);
        if let Some(fixed) = st.role {
            if fixed != next {
                return Err(CmdError::coded("connection_role_fixed",
                    format!("connection role already fixed to {}", fixed.as_str())));
            }
        }
        st.role = Some(next);
        st.info = Some(info.clone());
        Ok((info, next))
    }

    pub fn connection_role(&self) -> ConnectionRole {
        self.client.lock().unwrap().role.unwrap_or(ConnectionRole::Observer)
    }

    pub fn require_interactive(&self) -> Result<(), CmdError> {
        if self.connection_role() == ConnectionRole::InteractiveUser { Ok(()) }
        else { Err(CmdError::coded("stop_requires_interactive_client",
              "client must declare capability interactive_user")) }
    }
}
```

`rpc.rs` dispatch arm:

```rust
"set_client_info" => {
    let width = cmd.get("width").and_then(Value::as_u64).unwrap_or(80);
    let width = u16::try_from(width).map_err(|_| CmdError::coded("invalid_width", "width overflow"))?;
    let capabilities = cmd.get("capabilities").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    if capabilities.iter().any(|c| c.trim().is_empty()) {
        return Err(CmdError::coded("invalid_capabilities", "empty capability string"));
    }
    let (info, role) = rt.set_client_info(ClientInfo { width, capabilities })?;
    Ok(json!({"width": info.width, "capabilities": info.capabilities,
              "role": role.as_str(), "roleFixed": true}))
}
"interrupt" => {
    let intent = cmd.get("intent").and_then(Value::as_str).unwrap_or("stop");
    if intent != "stop" && intent != "abort" {
        return Err(CmdError::coded("invalid_intent", format!("unknown intent: {intent}")));
    }
    rt.require_interactive()?;   // stop/abort 모두 인터랙티브 클라이언트 요구
    /* 기존 보류 로직: pauseSeq 증가, paused=true */
}
```

`response()`(203-226) 수정: `if let Some(code) = err.code { obj.insert("errorCode", code.into()); }`.

TS 소비측(`omo-nyang/src/lane-hold.ts:29-76`) 정합:

```ts
const r = await rpc({ cmd: "set_client_info", width: 80, capabilities: ["interactive_user"] });
if (!r.success && r.errorCode === "connection_role_fixed") throw new LaneHoldError("role_fixed", r.error);
const s = await rpc({ cmd: "get_state" });        // s.data.role === "interactive_user"
```

`reload()`, `resume_session()` 경로는 `self.client`를 건드리지 않는다(역할은 연결 수명 귀속). 새 프로세스/새 `Runtime`에서만 `ClientState::default()`로 리셋된다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `set_client_info` 1회 호출 후 `get_state`가 `role:"interactive_user"`를 반환한다.
2. 동일 역할 재선언(width만 변경, capabilities 순서 변경)은 성공하고 `width`가 갱신된다(멱등).
3. 역할을 바꾸는 재선언(예: `capabilities:[]` 또는 `["observer"]`)은 `success:false`, `errorCode:"connection_role_fixed"`이며 `get_state`의 이전 상태가 보존된다.
4. `set_client_info` 없이 `interrupt({intent:"stop"})` → `errorCode:"stop_requires_interactive_client"`.
5. `interrupt({intent:"bogus"})` → `errorCode:"invalid_intent"`.
6. 모든 실패 프레임에 `error`(문자열)와 `errorCode`가 동시에 존재한다.

검증 명령:

```
cargo test -p omp-cli rpc::client_info -- --nocapture
cargo test -p omp-cli --test rpc_frames response_carries_error_code
cargo clippy -p omp-cli --all-targets -- -D warnings
grep -rn "connection_role_fixed\|stop_requires_interactive_client" crates/ omo-nyang/src/
printf '%s\n' '{"type":"request","id":"1","cmd":"set_client_info","width":80,"capabilities":["interactive_user"]}' \
  '{"type":"request","id":"2","cmd":"get_state"}' | omt --mode rpc --multi-session --session-runtime in-process
bun test omo-nyang/src/lane-hold.test.ts
```

수동 트랜스크립트에서 2번째 응답이 `
