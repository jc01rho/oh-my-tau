# 조사 조각 05 (1795326973 확인필요 5): tau omp-session JSONL v3 엔트리 구조와 omo-nyang session-patterns.ts·session-identity.ts 리더 호환성 실측 (inv-05-omp-jsonl-v3-vs-senpi-reader)

## 1. 목적 및 코드 근거 (File & Line)

목적: omp-session이 기록하는 JSONL v3 파일을 omo-nyang 리더가 무손실로 읽는지, 그리고 확장이 요구하는 RPC 표면(`api.rpc.handle`)이 실제로 노출되는지를 코드 라인 단위로 확정한다.

- `crates/omp-session/src/lib.rs:25` — `SESSION_VERSION = 3`. 파일 포맷 버전의 단일 진실.
- `crates/omp-session/src/lib.rs:225` — session 헤더 엔트리 기록 지점(첫 줄 계약의 발생원).
- `crates/omp-session/src/entries.rs:66-77` — `TitleSlot` 256B 1행 제약. 초과 시 절단/거부 정책이 여기서 결정된다.
- `crates/omp-session/src/entries.rs` — `Entry::Message(id, parentId, timestamp, message{role, content, toolCallId, isError})`.
- `crates/omp-ext/src/host.rs:127` `LoadResult`, `:431` `send_request`, `:463` `request(method, params)` — 확장→호스트 요청 경로.
- `crates/omp-cli/src/rpc.rs:593` `dispatch`, `:203` `code` + `errorCode` 동시 직렬화 — 응답 파싱 계약의 위험 지점.
- `omo-nyang/src/session-identity.ts:13-31` — 첫 줄에서 `type == "session"` 및 `id` 문자열만 사용. 버전 필드는 읽지 않음(무검증 통과 위험).
- `omo-nyang/src/session-patterns.ts:47-76` — `"message"` 부분문자열 포함 줄만 선택 후 `timestamp`와 `message.{role, content, toolCallId, isError}` 파싱.
- `ext-host/host.ts:340` — `makeApi`에 `on, events.on, registerTool, registerCommand, registerFlag`만 존재. `api.rpc.handle` 부재.
- `omo-nyang/src/session-move-extension.ts:130-140` — `api.events.on("wake_source_state", {source, activeCount})` 구독 + `api.rpc.handle("omomeow.move-readiness", ...)` 등록 시도. 현재 API로는 즉시 실패.
- `omo-nyang/src/omo-feedback.ts:41,50` — `extension_request({name:"omo.feedback.prepare"|"omo.feedback.submit", data})` 경로 사용 중.

## 2. 상세 계약 및 데이터/타입 명세

JSONL v3 물리 계약:
- 1행 = 1 JSON 객체, LF 종료. BOM 없음. 파일 첫 행은 반드시 session 헤더.
- 헤더: `{"type":"session","id":<string>,"version":3,"titleSlot":<string<=256B>}`. `titleSlot`은 256B 1행 상한.
- 메시지: `{"type":"message","id":<string>,"parentId":<string|null>,"timestamp":<number|string>,"message":{"role":<string>,"content":<string|array>,"toolCallId":<string|null>,"isError":<bool>}}`.
- `parentId`는 트리 연결용이며 리더는 미사용(무시 가능해야 함).

RPC 계약:
- 요청: `request(method: string, params: unknown)` → `send_request`. 응답은 `LoadResult` 형태로 감싸진다.
- 응답 직렬화는 `code`와 `errorCode`를 동시에 포함할 수 있다(rpc.rs:203). 소비자는 `code != 0` 우선, `errorCode`는 보조 식별자로만 사용한다.
- `dispatch`(rpc.rs:593)는 미등록 메서드에 대해 명시적 에러를 반환해야 하며, 침묵 성공 금지.

TS 리더 계약(현행, 변경 금지 대상):
- `session-identity`: 첫 줄 파싱 성공 + `type === "session"` + `typeof id === "string"` → identity 확정. 그 외는 `null`.
- `session-patterns`: 라인별 `includes("message")` 프리필터 → `JSON.parse` → `message` 객체 존재 시에만 채택. 필드 누락은 `undefined` 허용.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 포함)

변경 1 — `ext-host/host.ts:340` `makeApi`에 RPC 표면 추가:

```ts
// ext-host/host.ts (makeApi 내부)
rpc: {
  handle: (method: string, handler: (params: unknown) => Promise<unknown> | unknown) => {
    if (rpcTable.has(method)) throw new Error(`duplicate rpc method: ${method}`);
    rpcTable.set(method, handler);
    return () => rpcTable.delete(method); // dispose
  },
},
```

변경 2 — `session-move-extension.ts:130-140` 등록 실패 방어:

```ts
const off = api.rpc?.handle?.("omomeow.move-readiness",
  () => ({ activeSources: [...active] }));
if (!off) throw new Error("host does not expose api.rpc.handle (host.ts:340)");
```

검증 1 — Rust 왕복 직렬화 테스트(`crates/omp-session/tests/jsonl_v3.rs`):

```rust
#[test]
fn v3_header_and_message_roundtrip() {
    let e = Entry::Message("m1".into(), None, 1_700_000_000, Message{
        role: "user".into(), content: "hi".into(), tool_call_id: None, is_error: false });
    let line = serde_json::to_string(&e).unwrap();
    assert!(!line.contains('\n'));
    let back: Entry = serde_json::from_str(&line).unwrap();
    assert_eq!(back, e);
}
```

검증 2 — `TitleSlot` 256B 경계:

```rust
assert!(TitleSlot::new("x".repeat(256)).is_ok());
assert!(TitleSlot::new("x".repeat(257)).is_err());
```

검증 3 — TS 리더 호환 픽스처(`omo-nyang/test/jsonl-compat.test.ts`):

```ts
const lines = readFileSync(fixture, "utf8").split("\n").filter(Boolean);
const ident = parseIdentity(lines[0]);            // {id} 기대
const msgs  = parsePatterns(lines.join("\n"));    // message만, 길이 일치 기대
expect(msgs.every(m => typeof m.message?.role === "string")).toBe(true);
expect(parseIdentity('{"type":"message"}')).toBeNull(); // 헤더 오인 방지
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-session jsonl_v3` 통과(헤더/메시지 왕복, 256B 경계, CRLF 미포함).
2. `cargo test -p omp-ext` 및 `cargo test -p omp-cli rpc` 통과(`code`+`errorCode` 동시 응답 파싱, 미등록 메서드 에러).
3. `pnpm -C omo-nyang test jsonl-compat` 통과(정상/손상 픽스처 각 1개 이상).
4. 확장 로드 시 `omomeow.move-readiness` 등록 성공 로그 1회, `api.rpc.handle` 부재 시 명시적 throw 확인.
5. `node scripts/verify-jsonl-compat.ts fixtures/*.jsonl` 종료코드 0, 출력에 `header=ok messages=N skipped=M`.
6. 회귀: v2 파일 1건을 넣어도 리더가 예외 없이 `identity=null` 또는 명시적 `unsupported_version` 반환.

## 5. 엣지 케이스 및 실패 복구

- 첫 줄이 session이 아님/공백: `session-identity`는 `null`, `session-patterns`는 계속 진행(메시지 유실 없음). 상위에서 `unsupported_session_header` 경고 1회.
- 부분 기록(중단된 write)으로 마지막 줄이 잘림: 라인 단위 `try/catch`로 skip하고 `skipped` 카운터 증가. 파일 절단 금지.
- CRLF/BOM: 파서 진입 전 `replace(/^\uFEFF/, "")` 및 `\r` 제거. 원본 파일은 불변.
- `titleSlot` 257B 이상: 기록 측에서 거부(`is_err`)하고 세션 생성 실패로 전파. 읽기 측은 256B 절단 후 경고.
- 알 수 없는 `type` 값: 무시하되 `unknownEntry` 카운터로 집계(포워드 호환).
- `content`가 배열/문자열 혼재, `toolCallId: null`, `isError` 누락: 리더는 타입 강제 변환 없이 통과, 소비 측에서 기본값(`isError=false`) 적용.
- `code != 0`과 `errorCode` 동시 존재: `code`를 1차 분기, `errorCode`는 로그 태그로만 사용. 둘 다 없으면 `malformed_response`로 실패 처리.
- `api.rpc.handle` 미노출(host.ts:340 회귀): 확장 초기화 즉시 실패 + `extension_request` 폴백(omo-feedback.ts:41,50 경로) 사용, 사용자에게 기능 축소 고지.
- 손상 라인 다수(>10%): 파일을 `.bad`로 격리하고 원본 보존, 세션 로드는 읽기 전용 모드로 강등.
