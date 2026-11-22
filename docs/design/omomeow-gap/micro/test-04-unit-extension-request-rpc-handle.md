# 테스트 조각 04: `extension_request` ↔ `api.rpc.handle`(`omomeow.move-readiness`, 타임아웃, 미등록 핸들러) 통합 테스트 명세 (test-04-unit-extension-request-rpc-handle)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 RPC 클라이언트(omo-nyang `MoveHost`)가 보내는 `extension_request` 프레임이 확장 호스트의 `api.rpc.handle(name, handler)`로 정확히 라우팅되고, 그 결과가 `{type:"response", id, success, data?, error?, errorCode?}` 프레임으로 왕복하는지를 실제 프로세스 수준에서 고정한다. 대상 3경로는 (a) 정상 응답(`omomeow.move-readiness`), (b) 핸들러 타임아웃, (c) 미등록 핸들러/미지원 명령이다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. `extension_request` 분기는 여기 추가된다.
- 미지원 명령 폴백: `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 이 문자열은 그대로 회귀 기준선으로 사용한다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` — `response()`. 실패 시 `error`와 `errorCode`를 **동시에** 직렬화해야 한다.
- 클라이언트 파서: `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}`를 파싱. `errorCode` 누락 시 클라이언트 분기가 붕괴한다.
- 라우팅 계약: `omo-nyang/src/relay/session-move-extension.ts:116-139` — `extension_request({sessionId?, name, data?})` → `api.rpc.handle(name, handler)`. 등록 이름은 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`.
- 호스트 실행 형태: `omo-nyang/src/relay/move-host.ts:37-39` — `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`.
- 확장 이벤트 대조군: `session_move_export`/`session_move_import`(`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})` — 이 조각은 이벤트가 아니라 RPC 응답 경로만 검증한다.
- 정착 신호 참고: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고 `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 따라서 본 테스트는 종료 판정을 `session_settled`로 한다.

## 2. 상세 계약 및 데이터/타입 명세

요청 프레임(stdin, JSONL 1줄):

```json
{"type":"request","id":"req-1","command":"extension_request",
 "payload":{"sessionId":"sess-1","name":"omomeow.move-readiness",
            "data":{"sessionPath":"/tmp/ws/session.jsonl","cwd":"/tmp/ws","from":"local"},
            "timeoutMs":2000}}
```

성공 응답(stdout, JSONL 1줄) — `rpc.rs:203-226` 조립:

```json
{"type":"response","id":"req-1","success":true,
 "data":{"ready":true,"sessionPath":"/tmp/ws/session.jsonl","missing":[]}}
```

실패 응답 — `error`와 `errorCode` 동시 직렬화 필수:

```json
{"type":"response","id":"req-1","success":false,
 "error":"extension handler timed out after 2000ms","errorCode":"extension_request_timeout"}
```

Rust 측 신규 타입(`crates/omp-cli/src/rpc.rs` 인근, serde `camelCase`):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRequestParams {
    pub session_id: Option<String>,
    pub name: String,
    pub data: Option<serde_json::Value>,
    pub timeout_ms: Option<u64>, // 기본 30_000, 상한 120_000으로 클램프
}
```

디스패치 분기 계약(의사 시그니처):

```rust
"extension_request" => {
    let p: ExtensionRequestParams = parse(cmd)?;              // 실패 → invalid_params
    let ext = rt.extension_host().ok_or(CmdError::code("extension_host_unavailable"))?;
    match ext.call_rpc(&p.name, p.data, timeout).await {       // ext.rs 브리지
        Ok(v)  => Ok(v),
        Err(e) => Err(CmdError::with_code(e.code(), e.to_string())),
    }
}
```

`CmdError`는 `code`와 `message`를 보유하고, `response()`가 `errorCode`/`error`로 매핑한다. 오류 코드 집합은 닫힌 열거로 고정한다: `extension_handler_not_found`, `extension_request_timeout`, `extension_request_failed`, `extension_host_unavailable`, `invalid_params`.

TS 측 계약(테스트 픽스처가 등록하는 핸들러):

```ts
api.rpc.handle("omomeow.move-readiness", async (data: {sessionPath: string; cwd: string}) => ({
  ready: true, sessionPath: data.sessionPath, missing: [],
}));
```

클라이언트 검증은 `move-host.ts:61`의 파서를 그대로 통과시켜 `success:false` + `errorCode` 조합이 예외 객체의 `code`로 승격되는지까지 본다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

1. 픽스처 확장 작성: `crates/omp-cli/tests/fixtures/ext_rpc_probe.ts` — 3개 핸들러를 등록한다.
   - `omomeow.move-readiness`: 즉시 `{ready:true,...}` 반환.
   - `omomeow.slow`: `await new Promise(r => setTimeout(r, 5000))` 후 반환(타임아웃 유도, 이름은 테스트 전용).
   - `omomeow.boom`: `throw new Error("handler exploded")`.
2. Rust 통합 테스트 `crates/omp-cli/tests/extension_request_rpc.rs`:

```rust
#[tokio::test]
async fn extension_request_routes_to_registered_handler() {
    let mut child = spawn_omt(&["--mode","rpc","--multi-session",
                                "--session-runtime","in-process","-e",FIXTURE]);
    let (mut tx, mut rx) = attach_jsonl(&mut child);
    tx.send(json!({"type":"request","id":"r1","command":"extension_request",
        "payload":{"name":"omomeow.move-readiness",
                   "data":{"sessionPath":"/tmp/ws/session.jsonl","cwd":"/tmp/ws"},
                   "timeoutMs":2000}})).await;
    let resp = rx.next_response("r1", Duration::from_secs(5)).await;
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["ready"], true);
    assert!(resp.get("errorCode").is_none());
}
```

3. 타임아웃 케이스: `"omomeow.slow"` + `timeoutMs: 300` → `success:false`, `errorCode == "extension_request_timeout"`, `error` 문자열에 `300ms` 포함. 응답 도달 시간 상한 2초 단언(`Instant::now()` 측정).
4. 미등록 핸들러: `"name":"omomeow.not-registered"` → `success:false`, `errorCode == "extension_handler_not_found"`, 프로세스는 생존(이어서 `get_protocol_info` 호출 성공).
5. 핸들러 예외: `"omomeow.boom"` → `errorCode == "extension_request_failed"`, `error`에 `"handler exploded"` 포함.
6. 미지원 명령 회귀: `{"command":"definitely_not_a_command"}` → `error`가 정확히 `Unknown command: definitely_not_a_command`(`rpc.rs:1094`).
7. TS 측 `omo-nyang/src/relay/__tests__/move-host.rpc.spec.ts`: 위 4개 프레임을 순서대로 주입하고 `callExtensionRequest`의 reject/ resolve를 단언, `errorCode` → `err.code` 매핑 확인.
8. 종료: 각 테스트 종료 시 `session_settled`(`runtime.rs:1641`) 수신 후 `child.kill()`. 미수신 시 3초 후 강제 종료.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- 3개 시나리오(정상/타임아웃/미등록) + 예외 + 미지원 명령, 총 5개 케이스가 모두 통과한다.
- 모든 실패 응답이 `error`와 `errorCode`를 **동시에** 포함한다(`rpc.rs:203-226` 계약).
- `extension_request` 처리 중 RPC 루프가 블로킹되지 않는다: 타임아웃 케이스 도중 `get_protocol_info`가 500ms 내 응답.
- 픽스처 미등록 상태에서 `extension_handler_not_found`가 `Unknown command:`와 구분되어 반환된다.
- 명령어:
  - `cargo test -p omp-cli --test extension_request_rpc -- --nocapture`
  - `cargo test -p omp-cli rpc::tests::response_frame_shape`
  - `cargo clippy -p omp-cli --tests -- -D warnings`
  - `pnpm -C omo-nyang test -- move-host`
  - `pnpm -C omo-nyang exec tsc --noEmit`

## 5. 엣지 케이스 및 실패 복구

- 핸들러가 `undefined`/비직렬화 값 반환: `data`를 `null`로 정규화하고 `success:true` 유지, 경고 로그 1회.
- `timeoutMs: 0` 또는 음수: `invalid_params`로 즉시 거부(0을 "무한"으로 해석하지 않는다).
- `timeoutMs > 120_000`: 상한 클램프 후 정상 진행, 응답 `data`에 클램프 흔적을 남기지 않는다.
- 확장 호스트 미기동(`-e` 누락): `extension_host_unavailable`로 실패하되 프로세스는 종료하지 않는다.
- 중복 `id` 재사용: 마지막 요청 기준으로 응답하며, 테스트는 `id` 유일성을 강제한다.
- 핸들러가 stdout에 직접 로그 출력: JSONL 파서가 비-JSON 줄을 건너
