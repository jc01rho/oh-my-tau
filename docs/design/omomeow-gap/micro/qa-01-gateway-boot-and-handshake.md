# QA 시나리오 01: `gateway.ts` `workerArgv` 플래그로 `omt` 기동 및 `get_protocol_info`·`set_client_info` 초기 핸드셰이크 검증 (qa-01-gateway-boot-and-handshake)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: `omo-nyang` 게이트웨이가 조립한 `workerArgv`를 **한 글자도 변형하지 않고** `omt`에 전달해 프로세스를 기동하고, 최초 2개 RPC(`get_protocol_info`, `set_client_info`)로 핸드셰이크를 완료하는지 실측 검증한다. 인자 누락, 프로토콜 버전 불일치, 응답 프레임 스키마 불일치는 즉시 실패로 처리한다.
- 게이트웨이 인자 조립: `omo-nyang/src/gateway.ts:1167`이 `--skill <디렉터리경로>`를 전달하나, tau 쪽 `crates/omp-cli/src/cli.rs:129,309`에는 `--skills <CSV필터>`만 존재한다. `cli.rs:97-166`의 `Args` 정의에 반복 가능한 `skill_paths: Vec<PathBuf>`(`#[arg(long = "skill", value_name = "DIR")]`)를 신설해야 한다.
- 모델 폴백 인자: `omo-nyang/src/model-routing.ts:654-658`이 `--no-model-fallback`, `--no-recommended-models`를 붙인다. clap에 미정의면 exit code 2로 종료되어 핸드셰이크 이전에 죽는다.
- 멀티 세션 인자: `omo-nyang/src/relay/move-host.ts:37`이 `--multi-session --session-runtime in-process`를 전달한다.
- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 응답 프레임 조립: `rpc.rs:203-226`의 `response()`. 소비자 파서는 `omo-nyang/src/relay/move-host.ts:4-7,61`로 `{type:"response", id, success, data?, error?, errorCode?}`를 읽는다.
- 핸드셰이크 계약: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` 선언 후 `interrupt({intent})`, `get_state()` 순서. 역량 미선언 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(클라이언트 → `omt` stdout/stdin NDJSON):
```json
{"type":"request","id":1,"command":"get_protocol_info","payload":{}}
{"type":"request","id":2,"command":"set_client_info","payload":{"width":80,"capabilities":["interactive_user"]}}
```

`get_protocol_info` 성공 응답 `data`(고정 스키마):
```json
{"protocolVersion":2,"serverVersion":"<env!(CARGO_PKG_VERSION)>","engine":"oh-my-tau",
 "commands":["get_protocol_info","set_client_info","get_state","interrupt","reload",
             "resume_session","move_session","release_session","extension_request"]}
```

`set_client_info` 성공 응답: `{"type":"response","id":2,"success":true,"data":{"width":80,"capabilities":["interactive_user"]}}`

실패 프레임(두 필드 동시 직렬화 필수, `rpc.rs:203-226`):
```json
{"type":"response","id":2,"success":false,"error":"client role already fixed","errorCode":"connection_role_fixed"}
```

Rust 측 타입(`crates/omp-cli/src/rpc.rs`):
```rust
pub type CmdResult = Result<Value, CmdError>;
pub struct CmdError { pub code: &'static str, pub message: String }
impl CmdError { pub fn new(m: impl Into<String>) -> Self; pub fn with_code(m: impl Into<String>, c: &'static str) -> Self; }
```
TS 측 타입(`omo-nyang/src/relay/move-host.ts:4-7` 정합):
```ts
type RpcResponse<T> = { type: "response"; id: number; success: true; data: T }
  | { type: "response"; id: number; success: false; error: string; errorCode: string };
type ProtocolInfo = { protocolVersion: 2; serverVersion: string; engine: "oh-my-tau"; commands: string[] };
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

(1) `cli.rs` 인자 추가 — 기존 `--skills`(CSV 필터)와 병존:
```rust
// crates/omp-cli/src/cli.rs (Args, 97-166 구간)
#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
pub skill_paths: Vec<PathBuf>,          // gateway.ts:1167 반복 전달 대응
#[arg(long = "multi-session")] pub multi_session: bool,          // move-host.ts:37
#[arg(long = "session-runtime", value_name = "MODE")] pub session_runtime: Option<String>,
#[arg(long = "no-model-fallback")] pub no_model_fallback: bool,  // model-routing.ts:658
#[arg(long = "no-recommended-models")] pub no_recommended_models: bool,
```

(2) `rpc.rs` 핸들러: `set_client_info`가 `capabilities`에 `interactive_user` 포함 여부를 `ClientRole`에 1회 고정하고, 두 번째 호출에서 역할이 다르면 `CmdError::with_code(.., "connection_role_fixed")` 반환.

(3) QA 하네스(`omo-nyang/test/qa-01-boot.test.ts`) 의사코드:
```ts
const argv = ["--mode","rpc","--multi-session","--session-runtime","in-process",
  "--skill", skillRoot, "--no-model-fallback","--no-recommended-models"];
const child = spawn(omtBin, argv, { env: { ...process.env, OMP_HOME: tmpHome } });
const rl = readline.createInterface({ input: child.stdout });   // 청크 단위 파싱 금지
const pending = new Map<number, (r: RpcResponse<unknown>) => void>();
rl.on("line", l => { const f = JSON.parse(l); if (f.type === "response") pending.get(f.id)?.(f); });

const info = await rpc(1, "get_protocol_info", {});
expect(info.success && info.data.protocolVersion).toBe(2);
expect(info.data.engine).toBe("oh-my-tau");
expect(info.data.serverVersion).toMatch(/^\d+\.\d+\.\d+/);
expect(info.data.commands).toEqual(expect.arrayContaining(["set_client_info","get_state","interrupt"]));

const a = await rpc(2, "set_client_info", { width: 80, capabilities: ["interactive_user"] });
expect(a.success).toBe(true);
const b = await rpc(3, "set_client_info", { width: 80, capabilities: ["interactive_user","readonly"] });
expect(b.success).toBe(false); expect(b.errorCode).toBe("connection_role_fixed");

const u = await rpc(4, "definitely_unknown", {});   // rpc.rs
