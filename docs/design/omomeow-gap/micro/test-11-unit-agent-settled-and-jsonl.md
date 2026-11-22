# 테스트 조각 11: `agent_settled` RPC stdout 발행 타이밍 및 `omp JSONL v3` → `session-patterns.ts` 파싱 호환 테스트 명세 (test-11-unit-agent-settled-and-jsonl)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 두 개의 독립 회귀 지점을 하나의 테스트 스위트로 묶는다.

**(A) `agent_settled` stdout 계약 불일치**
- 생산자: `crates/omp-cli/src/runtime.rs:1641` — 세션 종료 시 stdout으로 `{"type":"session_settled"}`만 방출.
- 확장 호스트 내부 경로: `crates/omp-cli/src/ext.rs:1598` — `agent_settled`를 확장 호스트 내부 채널로만 전달, 프로세스 stdout에는 미노출.
- 소비자: `omo-nyang/src/relay/runtime.ts:59`, `omo-nyang/src/relay/slice-run.ts:71`, `omo-nyang/src/fleet.ts:258`, `omo-nyang/src/agentd-sessions.ts:186` — RPC stdout 라인에서 `type === "agent_settled"` 프레임을 대기.
- RPC 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()`; 디스패치 진입점 `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령 폴백: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`.

**(B) `omp JSONL v3` → `session-patterns.ts` 파싱 호환**
- 생산자: `crates/omp-session/src/entries.rs` — 256B title slot + camelCase entries 직렬화.
- 소비자: `omo-nyang/src/session-patterns.ts:47-76` — JSONL 각 줄에서 `{timestamp, message:{role:"assistant"|"toolResult", content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}}` 형태를 기대.

목표는 (1) `agent_settled`가 RPC stdout에 계약된 타이밍(정확히 1회, 세션 정산 완료 후, `session_settled`와의 순서 보장)에 발행되는지, (2) 실제 `omp` JSONL v3 산출물이 `session-patterns.ts`의 추출 로직을 무손실 통과하는지를 단위/골든 테스트로 고정하는 것이다.

## 2. 상세 계약 및 데이터/타입 명세

**2.1 `agent_settled` stdout 프레임 스키마 (NDJSON, 1줄 1객체)**

```json
{"type":"agent_settled","sessionId":"<string>","seq":<u64>,"settledAt":"<RFC3339>"}
```

- `type`: 리터럴 `"agent_settled"`.
- `sessionId`: `Runtime` 세션 식별자, `release_session({sessionId,...})`에서 쓰는 값과 동일.
- `seq`: 단조 증가 `u64`. `session_settled` 프레임과 동일 시퀀스 공간 공유.
- `settledAt`: `chrono::DateTime<Utc>` 직렬화(RFC3339).

**2.2 순서/횟수 계약**
- 세션 1회 종료당 `agent_settled` 정확히 1프레임.
- `session_settled`가 먼저, `agent_settled`가 뒤따른다(정산 완료 후 통지).
- `--multi-session`에서 세션별 1프레임, `sessionId`로 구분.

**2.3 Rust 측 발행 지점(신규 헬퍼)**

```rust
// crates/omp-cli/src/runtime.rs
fn emit_agent_settled(out: &mut impl Write, session_id: &str, seq: u64) -> std::io::Result<()>;
```

`runtime.rs:1641`의 `session_settled` 방출 직후 호출. `ext.rs:1598` 내부 채널 경로는 유지(이중 발행 금지 가드 필요).

**2.4 JSONL v3 엔트리 → 소비자 필드 매핑**

| JSONL v3 필드 (entries.rs) | session-patterns.ts 기대 |
|---|---|
| `timestamp` | `timestamp` |
| `message.role` ∈ {`assistant`,`toolResult`} | `message.role` |
| `message.content[].type === "toolCall"` | `content[].type` |
| `message.content[].name` | `content[].name` |
| `message.content[].arguments` | `content[].arguments` |
| `message.content[].id` | `content[].id` |
| `toolCallId` | `toolCallId` |
| `isError` | `isError` |

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

**3.1 Rust 통합 테스트 (stdout 캡처)**

```rust
#[test]
fn agent_settled_emitted_once_after_session_settled() {
    let out = run_rpc_script(&[
        r#"{"id":1,"cmd":"set_client_info","params":{"width":80,"capabilities":["interactive_user"]}}"#,
        r#"{"id":2,"cmd":"get_state"}"#,
        r#"{"id":3,"cmd":"interrupt","params":{"intent":"stop"}}"#,
        r#"{"id":4,"cmd":"resume_session"}"#,
    ]);
    let frames: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let idx_s = frames.iter().position(|f| f["type"] == "session_settled").expect("session_settled");
    let idx_a = frames.iter().position(|f| f["type"] == "agent_settled").expect("agent_settled");
    assert!(idx_s < idx_a);
    assert_eq!(frames.iter().filter(|f| f["type"] == "agent_settled").count(), 1);
    assert!(frames[idx_a]["sessionId"].is_string());
    assert!(frames[idx_a]["seq"].as_u64().unwrap() > 0);
}
```

**3.2 TS 파서 골든 테스트 (session-patterns.ts 재사용)**

```ts
// omo-nyang/src/session-patterns.test.ts
import { parseSessionFile } from "./session-patterns";
test("omp JSONL v3 golden -> toolCall 추출 무손실", async () => {
  const got = await parseSessionFile("fixtures/omp-v3-sample.jsonl");
  expect(got).toContainEqual({
    timestamp: expect.any(String),
    message: { role: "assistant", content: [{ type: "toolCall", name: "bash", arguments: { cmd: "ls" }, id: "call_1" }] },
    toolCallId: "call_1",
    isError: false,
  });
});
```

**3.3 이중 발행 가드 의사코드**

```
emit_session_settled()
if !settled_emitted.swap(true):
    emit_agent_settled(session_id, seq)
else:
    log_warn("agent_settled already emitted")  // ext.rs 경로와 충돌 방지
```

**3.4 픽스처 생성**
- `crates/omp-session` 직렬화로 `fixtures/omp-v3-sample.jsonl` 생성 후 리포지토리에 커밋. 256B title slot 헤더 라인 포함, 이후 camelCase 엔트리 라인.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- [ ] `agent_settled`가 세션당 정확히 1회, `session_settled` 이후 stdout에 발행된다.
- [ ] `--multi-session`에서 세션별 `sessionId` 구분 발행, 누락/중복 0건.
- [ ] `ext.rs:1598` 내부 경로와 stdout 경로 간 이중 발행 없음(가드 테스트 통과).
- [ ] `fixtures/omp-v3-sample.jsonl`이 `session-patterns.ts` 파서를 예외 없이 통과하고 `toolCall` 항목 수/필드가 기대값과 일치.
- [ ] 기존 소비자(`relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`)가 파싱 실패 0건.

```bash
cargo test -p omp-cli --test rpc_stdout agent_settled
cargo test -p omp-session jsonl_v3_roundtrip
pnpm -C omo-nyang test -- session-patterns
```

## 5. 엣지 케이스 및 실패 복구

- **순서 역전**: `agent_settled`가 `session_settled`보다 먼저 나오면 소비자 타임아웃 위험 → 테스트에서 인덱스 단언으로 차단.
- **중복 발행**: `ext.rs` 경로와 stdout 경로 동시 트리거 시 `settled_emitted` 원자 플래그로 1회 제한, 위반 시 `log_warn` + 테스트 실패.
- **`sessionId` 누락/빈 문자열**: 스키마 검증에서 `is_string()` + non-empty 단언.
- **JSONL 버전 혼재(v2/v3)**: `entries.rs` 버전 헤더 확인 후 v3만 골든 대상, v2는 skip + 명시적 경고.
- **필드명 스네이크/카멜 혼용**: `timestamp`,`toolCallId`,`isError` 카멜 케이스 강제, 스네이크 발견 시 파서 실패로 조기 감지.
- **파서 부분 실패**: 한 줄 파싱 예외가 전체 파일을 중단시키지 않도록 `session-patterns.ts`에서 줄 단위 try/catch 유지, 실패 줄 수 카운트 반환 검증.
- **복구**: 실패 시 픽스처 재생성(`cargo run -p omp-session --bin gen-fixture`)으로 기준선 재확립, 스키마 변경 시 골든 파일과 매핑 표를 동시 갱신.
