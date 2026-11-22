# 구현 조각 03: `crates/omp-cli/src/rpc.rs`의 `get_protocol_info` 응답(`serverVersion`, `protocolVersion`, `commands`) 및 `errorCode` 직렬화 구현 명세 (impl-03-rpc-get-protocol-info)

## 1. 목적 및 코드 근거 (File & Line)

- `omo-nyang/src/lane-hold.ts:29-76`의 보류·재개 계약은 `set_client_info` → `interrupt` → `get_state` 순서로 진행되며, 그 진입점에서 `get_protocol_info()`를 호출해 서버 능력을 확인한다. 응답은 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`여야 한다.
- 현재 `crates/omp-cli/src/rpc.rs:593`의 `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`는 `kind` 문자열로 분기하며, `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`가 미지원 명령을 처리한다. 이 fallback은 `code` 없이 메시지만 만들므로 클라이언트가 오류 종류를 식별할 수 없다.
- 응답 프레임은 `crates/omp-cli/src/rpc.rs:203-226`의 `response()`가 조립한다. `omo-nyang/src/relay/move-host.ts:4-7, 61`의 `MoveHost`는 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로, 실패 프레임에는 `error`와 `errorCode`가 동시에 직렬화되어야 한다.
- 따라서 본 조각의 범위는 (a) `get_protocol_info` 분기 신설, (b) `commands` 배열을 단일 진실 원천(single source of truth)으로 관리, (c) 모든 `CmdError`에 `code`를 부여하고 `response()`가 `errorCode`로 내보내도록 고정하는 것이다.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(클라이언트 → 서버):

```json
{"type":"request","id":"3f1c","command":"get_protocol_info"}
```

성공 응답 프레임(서버 → 클라이언트):

```json
{"type":"response","id":"3f1c","success":true,
 "data":{"protocolVersion":2,"serverVersion":"0.1.0","engine":"oh-my-tau",
         "commands":[{"name":"get_state","params":[]},{"name":"interrupt","params":["intent"]}]}}
```

실패 응답 프레임:

```json
{"type":"response","id":"3f1c","success":false,
 "error":"Unknown command: foo_bar","errorCode":"unknown_command"}
```

신규 타입(`crates/omp-cli/src/rpc.rs`):

```rust
pub const PROTOCOL_VERSION: u32 = 2;
pub const ENGINE: &str = "oh-my-tau";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CommandDescriptor {
    pub name: &'static str,
    pub params: &'static [&'static str],
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolInfo {
    pub protocol_version: u32,
    pub server_version: &'static str,
    pub engine: &'static str,
    pub commands: &'static [CommandDescriptor],
}

#[derive(Debug, Clone)]
pub struct CmdError {
    pub code: &'static str,   // snake_case, 예: "unknown_command"
    pub message: String,
}
```

`errorCode` 문자열 규약은 `lane-hold.ts`가 쓰는 snake_case(`stop_requires_interactive_client`, `connection_role_fixed`)와 동일 계열을 유지한다. `commands[].name`은 `dispatch`의 `kind` 매치 암과 1:1로 일치해야 하며, `params`는 인자 이름만 노출하고 값·기본값은 노출하지 않는다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

(1) `response()` 수정 — `crates/omp-cli/src/rpc.rs:203-226`:

```rust
fn response(id: &Value, result: CmdResult) -> Value {
    match result {
        Ok(data) => json!({"type":"response","id":id,"success":true,"data":data}),
        Err(e) => json!({
            "type":"response","id":id,"success":false,
            "error": e.message, "errorCode": e.code
        }),
    }
}
```

(2) 정적 명령 목록 — `dispatch` 위에 선언하고, `get_protocol_info`는 이 목록을 그대로 직렬화한다:

```rust
static COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor { name: "get_protocol_info", params: &[] },
    CommandDescriptor { name: "set_client_info",   params: &["width", "capabilities"] },
    CommandDescriptor { name: "interrupt",         params: &["intent"] },
    CommandDescriptor { name: "get_state",         params: &[] },
    CommandDescriptor { name: "reload",            params: &[] },
    CommandDescriptor { name: "resume_session",    params: &[] },
    CommandDescriptor { name: "release_session",   params: &["sessionId", "reason", "moveId"] },
    CommandDescriptor { name: "extension_request", params: &["sessionId", "name", "data"] },
];
```

(3) `dispatch` 분기 추가 — `rpc.rs:1094`의 `other =>` 암 직전:

```rust
"get_protocol_info" => Ok(serde_json::to_value(ProtocolInfo {
    protocol_version: PROTOCOL_VERSION,
    server_version: SERVER_VERSION,
    engine: ENGINE,
    commands: COMMANDS,
}).map_err(|e| CmdError { code: "serialize_failed", message: e.to_string() })?),

other => Err(CmdError {
    code: "unknown_command",
    message: format!("Unknown command: {other}"),
}),
```

(4) 회귀 방지 테스트 — 매치 암과 `COMMANDS`의 표류(drift)를 막는다:

```rust
#[test]
fn protocol_info_shape_and_command_coverage() {
    let v = serde_json::to_value(protocol_info()).unwrap();
    assert_eq!(v["protocolVersion"], 2);
    assert_eq!(v["engine"], "oh-my-tau");
    assert_eq!(v["serverVersion"], env!("CARGO_PKG_VERSION"));
    for c in v["commands"].as_array().unwrap() {
        assert!(dispatch_is_known(c["name"].as_str().unwrap()));
    }
}
```

(5) 클라이언트 측 게이팅(참고 구현, `lane-hold.ts` 소비부): `protocolVersion !== 2`이면 보류·이동 기능을 비활성화하고, `commands`에 `release_session`이 없으면 `move_session` 흐름을 시작하지 않는다. `success:false`이고 `errorCode === "unknown_command"`이면 구버전 서버로 판정해 기능 탐지를 `commands` 기반으로만 수행한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `get_protocol_info` 응답의 `data.protocolVersion == 2`, `data.engine == "oh-my-tau"`, `data.serverVersion`이 `CARGO_PKG_VERSION`과 일치한다.
- AC2: `data.commands`가 비어 있지 않고, 각 원소에 `name`(문자열)과 `params`(문자열 배열)가 존재하며 중복 `name`이 없다.
- AC3: 미지원 명령 응답이 `success:false`이며 `errorCode == "unknown_command"`, `error`에 원문 명령 문자열이 포함된다. 즉 `rpc.rs:1094`의 기존 `CmdError::new` 호출이 모두 `code` 지정 형태로 대체된다.
- AC4: `response()`가 내보내는 모든 실패 프레임에 `errorCode` 키가 존재한다(옵셔널 누락 금지).
- 검증 명령:
  - `cargo test -p omp-cli rpc::`
  - `cargo clippy -p omp-cli --all-targets -- -D warnings`
  - `printf '{"type":"request","id":"1","command":"get_protocol_info"}\n' | omt --mode rpc --multi-session --session-runtime in-process | jq -e '.success and (.data.protocolVersion==2) and (.data.commands|length>0)'`
  - `printf '{"type":"request","id":"2","command":"foo_bar"}\n' | omt --mode rpc | jq -e '.success==false and .errorCode=="unknown_command"'`

## 5. 엣지 케이스 및 실패 복구

- `id` 타입 변동: `id`는 문자열·숫자·`null` 모두 가능하므로 `Value`를 그대로 에코하고 절대 재생성하지 않는다.
- 버전 드리프트: `protocolVersion`은 호환성이 깨질 때만 증가시킨다. 필드 추가는 `commands`/`params` 확장으로 흡수하고 버전을 올리지 않는다.
- `commands` 정렬: 선언 순서를 그대로 유지해 스냅샷 테스트를 안정화하고, 클라이언트는 순서에 의존하지 않도록 집합으로 해석한다(문서화 필요).
- `env!("CARGO_PKG_VERSION")`은 컴파일 시점 상수이므로 릴리스 태그와 불일치할 수 있다. CI에서 `cargo metadata`의 버전과 응답 `serverVersion`을 비교하는 검사를 추가한다.
- 레거시 오류 코드 부재: `code`가 없는 기존 `CmdError` 생성 지점은 `internal_error`로 기본값을 채우되, `unknown_command`·`stop_requires_interactive_client`·`connection_role_fixed`는 명시 코드를 유지한다.
- 직렬화 실패: `ProtocolInfo` 직렬화가 실패하면 `serialize_failed` 코드로 실패 프레임을 반환하고 프로세스는 종료하지 않는다(패닉 금지, `dispatch`는 `Result` 유지).
- 동시성: `get_protocol_info`는 정적 데이터만 읽으므로 `Runtime` 락을 잡지 않는다. 락 획득 실패로 인한 교착을 원천 차단한다.
- 정보 노출: `commands`에는 이름과 파라미터 이름만 포함하고, 파일 경로·설정 값·토큰 등 런타임 상태는 절대 포함하지 않는다.
