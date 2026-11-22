# QA 시나리오 05: `extension_request`(`omomeow.move-readiness`, `omo.feedback.prepare`/`submit`) 왕복 및 타임아웃 복구 검증 (qa-05-extension-readiness-and-feedback)

## 1. 목적 및 코드 근거 (File & Line)

`extension_request` RPC가 확장 호스트의 `api.rpc.handle(name, handler)` 라우팅을 통해 3개 핸들러(`omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit`)로 정확히 전달되고, 응답 프레임 스키마가 `MoveHost` 파서와 일치하며, 무응답/미등록 핸들러에서 타임아웃·에러코드로 복구되는지를 실측 검증한다.

- 디스패치 진입: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`
- 미지원 명령 폴백: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()` → `{type:"response", id, success, data?, error?, errorCode?}`
- 클라이언트 파서 계약: `omo-nyang/src/relay/move-host.ts:4-7, 61` (`{type:"response", id, success, data?, error?, errorCode?}`)
- 확장 라우팅 계약: `omo-nyang/src/relay/move-operations.ts:104-225`, `omo-nyang/src/relay/session-move-extension.ts:116-139`
- 확장 이벤트: `session_move_export` / `session_move_import` (`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})`
- 프로토콜 확인: `omo-nyang/src/lane-hold.ts:29-76` `get_protocol_info() → {protocolVersion:2, serverVersion, engine:"oh-my-tau", commands:[...]}`
- 호스트 실행 형태: `omt --mode rpc --multi-session --session-runtime in-process -e <ext>` (`omo-nyang/src/relay/move-host.ts:37-39`)

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(클라이언트 → tau):
```json
{"type":"request","id":"r-17","cmd":"extension_request","args":{"sessionId":"s-1","name":"omomeow.move-readiness","data":{"sessionPath":"/abs/s.jsonl","target":"fleet-small"}}}
```

Rust 측 타입:
```rust
// crates/omp-cli/src/rpc.rs
async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult;
struct CmdError { code: String, message: String } // 직렬화 시 code + errorCode 동시 노출
```

핸들러별 응답 `data` 스키마:
- `omomeow.move-readiness` → `{ready: bool, blockers: string[], sessionPath: string}`
- `omo.feedback.prepare` → `{feedbackId: string, prompt: string, expiresAt: number}`
- `omo.feedback.submit` → `{accepted: bool, feedbackId: string, storedAt: number}`

성공/실패 프레임:
```json
{"type":"response","id":"r-17","success":true,"data":{"ready":true,"blockers":[],"sessionPath":"/abs/s.jsonl"}}
{"type":"response","id":"r-17","success":false,"error":"handler timeout: omo.feedback.prepare","errorCode":"extension_request_timeout"}
```

에러코드 집합(고정): `extension_request_timeout`, `extension_handler_not_found`, `extension_handler_error`, `session_not_found`, `connection_role_fixed`(역할 고정 위반 시, `lane-hold.ts:29-76`).

타임아웃 기본값: 5000ms(핸들러 실행), 30000ms(`submit` 저장 경로). 재시도 없음(멱등 키 `feedbackId`로 중복 방지).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust 디스패치 분기 추가(`rpc.rs:593` 내, `1094` 폴백 이전):
```rust
"extension_request" => {
    let name = cmd["args"]["name"].as_str().ok_or_else(|| CmdError::new_code("invalid_args"))?;
    let data = cmd["args"].get("data").cloned().unwrap_or(Value::Null);
    match rt.extension_host().call(name, data, Duration::from_millis(5_000)).await {
        Ok(v) => Ok(v),
        Err(ExtErr::NotFound) => Err(CmdError::new_code("extension_handler_not_found")),
        Err(ExtErr::Timeout)  => Err(CmdError::new_code("extension_request_timeout")),
        Err(ExtErr::Handler(e)) => Err(CmdError::new_code("extension_handler_error").with_detail(e)),
    }
}
```

TS 확장 등록(`session-move-extension.ts:116-139` 패턴):
```ts
api.rpc.handle("omomeow.move-readiness", async ({data}) => {
  const f = ctx.sessionManager.getSessionFile(data.sessionPath);
  return { ready: !!f, blockers: f ? [] : ["session_missing"], sessionPath: data.sessionPath };
});
api.rpc.handle("omo.feedback.prepare", async () => ({ feedbackId: crypto.randomUUID(), prompt: "feedback?", expiresAt: Date.now()+30000 }));
api.rpc.handle("omo.feedback.submit", async ({data}) => ({ accepted: true, feedbackId: data.feedbackId, storedAt: Date.now() }));
```

검증 절차(하네스):
1. `get_protocol_info`로 `protocolVersion===2`, `commands`에 `extension_request` 존재 확인.
2. `set_client_info({width:80, capabilities:["interactive_user"]})` 후 3개 핸들러 순차 왕복, 각 응답 `id` 일치 및 스키마 필드 존재 단언.
3. 미등록 핸들러(`omo.feedback.unknown`) 호출 → `success:false`, `errorCode==="extension_handler_not_found"`.
4. 지연 핸들러(6초 sleep 주입) 호출 → `extension_request_timeout`, 이후 정상 핸들러 재호출 성공(복구 확인).
5. `release_session({sessionId, reason:"move", moveId})` → `{session_path}` 확인 후 `resume_session()` → `get_state`에서 `paused:false, runPaused:false`.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: 3개 핸들러 왕복 성공률 100%, `id`·`data` 스키마 100% 일치.
- AC2: 미등록 핸들러가 `extension_handler_not_found`로 1회 내 실패(폴백 `Unknown command` 미발생).
- AC3: 타임아웃 케이스가 5초±0.5초 내 `extension_request_timeout` 반환, 프로세스 생존.
- AC4: 타임아웃 후 정상 요청 성공(상태 오염 없음), `get_state` `paused/runPaused` 정상.
- AC5: 실패 프레임에 `error`와 `errorCode` 동시 존재(`rpc.rs:203-226` 계약).

```bash
cargo test -p omp-cli --test rpc_extension_request
bun test omo-nyang/src/relay/move-host.extension.test.ts
omt --mode rpc --multi-session --session-runtime in-process -e ./ext.js < fixtures/ext_req.jsonl | jq -c 'select(.type=="response")'
```

## 5. 엣지 케이스 및 실패 복구

- 무응답 핸들러: 타임아웃 후 요청 슬롯 정리, `pending` 맵 누수 없음 검증.
- 핸들러 내부 예외: `extension_handler_error` + 상세 메시지, 스택 미노출.
- `sessionId` 불일치/부재: `session_not_found`, `release_session` 선행 필요 안내.
- 역할 고정 위반(`capabilities` 변경 시도): `connection_role_fixed` 반환, 연결 유지.
- 동시 다발 요청(>16): 큐잉 후 순차 처리, `id` 교차 응답 없음.
- 확장 크래시: 호스트 재기동 후 미완료 `extension_request` 전부 `extension_handler_error`로 종결, 클라이언트는 `moveId` 기준 재시도.
- 복구 순서: `interrupt({intent:"abort"})` → `resume_session()` → `get_state` 확인 → 재시도. 실패 시 `reload()`로 설정·확장 재로딩 후 1회 재시도.
