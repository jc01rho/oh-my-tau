# 리뷰 조각 01: rpc.rs CmdError 직렬화(code vs errorCode)와 omo-nyang Zod 스키마(move-host.ts, lane-hold.ts) 정합성 감사 (rev-01-rpc-wire-error-code-audit)

## 1. 목적 및 코드 근거 (File & Line)

이 조각의 목적은 RPC 실패 프레임의 와이어 표현을 단일 계약으로 고정하는 것이다. 현재 서버측 오류는 `CmdError` 하나로 표현되지만, 클라이언트측 파서는 `errorCode`를 선택 필드로 느슨하게 받아들이고 있어 실패 원인 판별이 문자열 `error` 매칭에 의존할 위험이 있다. 감사 대상은 다음 세 지점이다.

| # | 위치 | 실측 내용 | 감사 포인트 |
|---|------|-----------|-------------|
| A | `crates/omp-cli/src/rpc.rs:593` | `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` | 모든 명령이 동일한 `CmdResult` 반환 경로를 타는지 |
| B | `crates/omp-cli/src/rpc.rs:1094` | `other => Err(CmdError::new(format!("Unknown command: {other}")))` | 코드 없는 생성자 사용 → `code`/`errorCode` 누락 |
| C | `crates/omp-cli/src/rpc.rs:203-226` | `response()`가 `{type,id,success,data?,error?}` 조립 | `errorCode` 및 `code` 직렬화 여부 |
| D | `omo-nyang/src/relay/move-host.ts:4-7, 61` | `{type:"response", id, success, data?, error?, errorCode?}` 파싱 | Zod 스키마와 서버 직렬화의 필드 집합 일치 |
| E | `omo-nyang/src/lane-hold.ts:29-76` | `set_client_info` → `interrupt` → `get_state`, `get_protocol_info`, `reload`, `resume_session` | 명령별 실패 코드가 클라이언트 분기에 매핑되는지 |

계약상 반드시 성립해야 하는 사실은 세 가지다. 첫째, `success:false`인 모든 프레임은 `errorCode`를 반드시 포함한다. 둘째, 하위 호환을 위해 `code`와 `errorCode`는 동일한 snake_case 값을 함께 직렬화한다. 셋째, `lane-hold.ts`가 정의한 `stop_requires_interactive_client`, `connection_role_fixed`는 문자열 메시지가 아니라 코드로 판별된다.

## 2. 상세 계약 및 데이터/타입 명세

성공 프레임은 `data`만, 실패 프레임은 `error` + `errorCode` + `code`만 갖는다. `data`가 `null`인 경우 필드를 생략하고, 실패 시 `data`는 절대 직렬화하지 않는다.

```jsonc
// 실패
{"type":"response","id":17,"success":false,
 "error":"Unknown command: foo","errorCode":"unknown_command","code":"unknown_command"}
// 성공 (reload)
{"type":"response","id":18,"success":true,"data":{"cancelled":false}}
```

오류 코드 택소노미(초기 고정 집합, 이후 추가만 허용):

| code | 발생 지점 | 클라이언트 분기 |
|------|-----------|-----------------|
| `unknown_command` | `rpc.rs:1094` | 프로토콜 불일치로 간주, `get_protocol_info` 재조회 |
| `invalid_params` | `dispatch` 진입부 검증 | 사용자 입력 오류, 재시도 금지 |
| `stop_requires_interactive_client` | `lane-hold.ts` interrupt 경로 | `set_client_info` 선행 후 1회 재시도 |
| `connection_role_fixed` | `lane-hold.ts` 역할 변경 | 재시도 금지, 세션 재연결 |
| `not_paused` | `resume_session` | `get_state`로 상태 재동기화 |
| `move_in_progress` | `move_session` | `status` 폴링 |
| `session_not_found` | `release_session`, `move_session` | 즉시 실패 처리 |

`get_protocol_info()` 응답은 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`이며, `commands` 배열에는 `set_client_info`, `interrupt`, `get_state`, `get_protocol_info`, `reload`, `resume_session`, `move_session`, `release_session`, `extension_request`가 모두 포함되어야 한다. `commands`에 없는 명령을 클라이언트가 보내면 서버는 `unknown_command`를 반환하고, 클라이언트는 `protocolVersion`을 재확인한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 포함)

Rust: `CmdError`에 코드를 필수 필드로 승격하고, 코드 없는 생성자를 제거한다.

```rust
// crates/omp-cli/src/rpc.rs
#[derive(Debug, Clone)]
pub struct CmdError { pub code: &'static str, pub message: String }

impl CmdError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn unknown_command(other: &str) -> Self {
        Self::new("unknown_command", format!("Unknown command: {}", sanitize(other, 512)))
    }
}
```

`rpc.rs:203-226`의 `response()`는 실패 분기에서 두 필드를 함께 쓴다.

```rust
Err(e) => json!({
    "type": "response", "id": id, "success": false,
    "error": e.message, "errorCode": e.code, "code": e.code
}),
Ok(data) => json!({ "type":"response", "id": id, "success": true, "data": data }),
```

`rpc.rs:1094`는 `Err(CmdError::unknown_command(other))`로 교체한다. `resume_session` 성공 시 `session_resumed` 이벤트 발행 후 `get_state`가 `paused:false, runPaused:false`를 반환하는지도 같은 테스트에서 확인한다.

TS: 스키마를 조이되 구버전 서버는 수용한다.

```ts
const ResponseFrame = z.object({
  type: z.literal("response"),
  id: z.union([z.string(), z.number()]),
  success: z.boolean(),
  data: z.unknown().optional(),
  error: z.string().optional(),
  errorCode: z.string().optional(),
  code: z.string().optional(),
}).passthrough().superRefine((f, ctx) => {
  if (f.success) return;
  const c = f.errorCode ?? f.code;
  if (!c) ctx.addIssue({ code: "custom", message: "errorCode missing on failure" });
  if (f.code && f.errorCode && f.code !== f.errorCode)
    ctx.addIssue({ code: "custom", message: "code/errorCode mismatch" });
});
```

클라이언트 정규화는 `const code = f.errorCode ?? f.code ?? "unknown_error"`로 두고, `move-host.ts:61`과 `lane-hold.ts`의 분기 모두 이 정규화 값만 참조하도록 통일한다.

## 4. 완료 판정
