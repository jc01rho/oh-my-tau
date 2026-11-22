# 조사 조각 04 (1795326973 확인필요 4): `omo-nyang` `agent_settled` 대기 로직과 `tau` `ext.rs:1598`(`agent_settled`)·`runtime.rs:1641`(`session_settled`)·`agent_end` 의미 대조 (inv-04-agent-settled-vs-agent-end)

## 1. 목적 및 코드 근거 (File & Line)

목적: `omo-nyang` 하네스가 RPC stdout에서 기다리는 `{"type":"agent_settled"}` 프레임이 `tau` 쪽에서 실제로 stdout에 나오지 않는 불일치를 확정하고, `agent_end`(턴 단위) / `agent_settled`(세션 정지 단위) / `session_settled`(프로세스 집계 단위)의 의미를 분리해 브리지 계약을 고정한다.

- `omo-nyang/src/relay/runtime.ts:59` — RPC stdout 라인 파서가 `agent_settled` 수신 시 대기 Promise를 resolve. `tau`가 이 프레임을 안 보내면 여기서 영구 대기.
- `omo-nyang/src/slice-run.ts:71` — 슬라이스 실행 종료 판정도 동일 프레임 의존.
- `omo-nyang/src/fleet.ts:258` — 플릿 단위 실행에서 동일 대기.
- `omo-nyang/src/agentd-sessions.ts:186` — 세션 데몬 경로에서 동일 대기. 즉 4곳이 같은 계약을 공유하므로 한 곳만 고쳐서는 해결되지 않음.
- `crates/omp-cli/src/runtime.rs:1641` — `tau` 메인 런타임이 stdout으로 내보내는 유일한 정지 프레임은 `{"type":"session_settled"}`.
- `crates/omp-cli/src/ext.rs:1598` — `agent_settled`는 확장 호스트 내부 이벤트로만 발행되고 호스트 stdout으로 승격되지 않음.
- `crates/omp-cli/src/rpc.rs:203-226` — `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립. 실패 시 `code`와 `errorCode`를 함께 직렬화해야 `omo-nyang/src/relay/move-host.ts:4-7, 61` 파서가 오류를 복원할 수 있음.
- `crates/omp-cli/src/rpc.rs:593` — `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 정지 상태 조회/대기 명령을 추가할 진입점.
- `crates/omp-cli/src/rpc.rs:1094` — `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 신규 명령 미등록 시 이 경로로 떨어짐.
- `agent_end` — 근거 요약에 파일:줄이 확정되지 않은 항목(확인필요). 1차 검증 대상이며, 아래 4장의 grep 명령으로 위치를 먼저 고정한다.

## 2. 상세 계약 및 데이터/타입 명세

의미 분리(이 조각의 결론 계약):

| 이벤트 | 발생 단위 | 발생 횟수 | 소비자 | 현재 상태 |
|---|---|---|---|---|
| `agent_end` | 단일 에이전트 턴/런 종료 | 세션당 N회 | 확장, UI, 훅 | `tau` 내부 이벤트(위치 확인필요) |
| `agent_settled` | 세션 정지(대기 큐·스트리밍·툴콜 0) | 정지 1회당 1회 | `omo-nyang` 4개 지점 | `ext.rs:1598` 내부에만 존재 |
| `session_settled` | 프로세스 내 전체 세션 집계 | 프로세스 종료/전체 정지 시 | `tau` 호스트 | `runtime.rs:1641` stdout |

`agent_end`는 "턴이 끝났다"이지 "더 할 일이 없다"가 아니다. 백그라운드 툴콜·큐잉된 다음 턴이 남아 있으면 `agent_end` 직후 새 턴이 시작되므로, `omo-nyang`이 기다리는 것은 `agent_end`가 아니라 `agent_settled`여야 한다. `session_settled`는 `--multi-session`에서 세션 N개를 모두 포함하므로 단일 세션 대기 조건으로 쓰면 과도하게 늦게 풀린다.

stdout 프레임 스키마(신규, `runtime.rs:1641` writer 재사용):

```json
{"type":"agent_settled","sessionId":"st_01H...","runId":"run_7","seq":12,
 "reason":"idle|stop|abort|error|exit","pendingTurns":0,"pendingToolCalls":0}
```

```json
{"type":"session_settled","sessionCount":2,"sessions":["st_a","st_b"],"seq":13}
```

Rust 측 제안 타입(`crates/omp-cli/src/runtime.rs`):

```rust
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleReason { Idle, Stop, Abort, Error, Exit }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettledFrame<'a> {
    #[serde(rename = "type")] pub kind: &'static str, // "agent_settled"
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub seq: u64,
    pub reason: SettleReason,
    pub pending_turns: usize,
    pub pending_tool_calls: usize,
}
```

RPC 보조 명령(선택, `rpc.rs:593` dispatch에 추가): `wait_settled({sessionId?, timeoutMs})` → `data: {settled:true, seq, reason}`. 미지원 명령 폴백은 `rpc.rs:1094` 그대로 유지.

TS 측 대기 계약(`omo-nyang/src/relay/runtime.ts:59` 등 4개 지점 공통화):

```ts
type SettleFrame =
  | { type: "agent_settled"; sessionId: string; runId: string; seq: number; reason: string }
  | { type: "session_settled"; sessionCount: number; sessions: string[]; seq: number };

function isSettleFor(f: SettleFrame, sessionId?: string): boolean
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

Rust — 정지 판정을 `agent_end` 처리 지점에서 파생시키고 stdout으로 승격:

```rust
// runtime.rs: agent_end 처리 직후
fn on_agent_end(&mut self, session_id: &str, run_id: &str) {
    self.sessions.get_mut(session_id).map(|s| s.finish_turn(run_id));
    self.maybe_emit_agent_settled(session_id, SettleReason::Idle);
}

fn maybe_emit_agent_settled(&mut self, sid: &str, reason: SettleReason) {
    let s = &self.sessions[sid];
    if s.pending_turns > 0 || s.pending_tool_calls > 0 || s.streaming { return; } // 조기 발행 금지
    if s.last_settled_seq == s.seq { return; }                                        // 중복 억제
    self.seq += 1;
    s.last_settled_seq = self.seq;
    self.write_stdout_line(&AgentSettledFrame { kind: "agent_settled", session_id: sid,
        run_id: &s.run_id, seq: self.seq, reason, pending_turns: 0, pending_tool_calls: 0 });
    self.write_stdout_line(&json!({"type":"agent_settled", /* 동일 필드 */})); // flush 필수
    self.maybe_emit_session_settled();
}
```

`ext.rs:1598`은 내부 브로드캐스트를 유지하되, 동일 페이로드를 런타임 채널로 전달해 위 `maybe_emit_agent_settled`가 단일 발행자가 되게 한다(이중 발행 금지).

TS — 4개 지점을 공통 헬퍼로 교체:

```ts
export function waitForSettle(stream, sessionId?: string, timeoutMs = 120_000) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error("settle_timeout")), timeoutMs);
    const onLine = (line: string) => {
      const f = JSON.parse(line);
      if (f.type === "agent_settled" && (!sessionId || f.sessionId === sessionId)) {
        clearTimeout(t); resolve(f);                       // 1순위
      } else if (f.type === "session_settled") {
        clearTimeout(t); resolve({ ...f, degraded: true }); // 하위호환 폴백
      }
    };
  });
}
```

검증 절차: ① `rg` 로 `agent_end` 실제 정의 위치 확정 ② 단일 세션 RPC 스모크에서 프레임 순서 관찰(`agent_end`* → `agent_settled` → `session_settled`) ③ `--multi-session`에서 세션 2개 정지 시 `agent_settled` 2회 + `session_settled` 1회 확인 ④ `omo-nyang` 4개 지점 회귀.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `rg -n '"agent_settled"|"session_settled"|"agent_end"' crates/omp-cli/src` 결과에서 세 이벤트의 발행 지점이 각각 1곳으로 정리되어 있고, `agent_settled` 발행 지점이 `ext.rs` 내부가 아닌 stdout writer 경로임.
2. 단일 세션 실행 시 stdout에 `{"type":"agent_settled",...}`가 최소 1회, `session_settled`보다 먼저 출력됨.
3. `--multi-session` 세션 2개 실행 시 `agent_settled` 2회, `session_settled` 1회, `seq` 단조 증가.
4. `omo-nyang`
