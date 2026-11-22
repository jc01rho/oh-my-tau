# QA 시나리오 09: `agent_settled` 수신을 통한 게이트웨이 턴 종료 판정 및 주간 `session-patterns.ts` JSONL 스캔 무오류 검증 (qa-09-agent-settled-and-session-survey)

## 1. 목적 및 코드 근거 (File & Line)

본 조각은 두 가지 회귀를 동시에 차단한다.

- (A) 게이트웨이 턴 종료 판정 불일치: `omo-nyang`은 RPC stdout에서 `{"type":"agent_settled"}` 프레임을 기다려 턴을 종료 처리하지만(`omo-nyang/src/relay/runtime.ts:59`, `omo-nyang/src/relay/slice-run.ts:71`, `omo-nyang/src/fleet.ts:258`, `omo-nyang/src/agentd-sessions.ts:186`), tau 엔진은 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout으로 내보내고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부 채널로만 발행한다. 즉 프로세스 경계를 넘는 프레임 이름이 다르다.
- (B) 주간 세션 스캔 크래시: `omo-nyang/src/session-patterns.ts:47-76`은 JSONL 각 줄에서 `{timestamp, message:{role, content:[{type:"toolCall", name, arguments, id}], toolCallId, isError}}` 형태를 가정해 파싱한다. tau의 세션 파일은 `crates/omp-session/src/entries.rs`의 omp JSONL v3 포맷(첫 줄 256B title slot + camelCase entries)이므로, 헤더/부분 줄/비객체 라인에서 예외가 나면 주간 배치 전체가 중단된다.
- 참고 계약: RPC 응답 프레임 조립은 `crates/omp-cli/src/rpc.rs:203-226`(`response()`), 디스패치 진입점은 `crates/omp-cli/src/rpc.rs:593`, 미지원 명령 처리 `crates/omp-cli/src/rpc.rs:1094`. `MoveHost`는 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다(`omo-nyang/src/relay/move-host.ts:4-7, 61`).

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 종료 이벤트 프레임 (stdout, 1줄 1 JSON)

```json
{"type":"agent_settled","agentId":"st_01H...","sessionId":"01J...","turnId":7,"reason":"completed","ts":1732...}
{"type":"session_settled","sessionId":"01J...","turnId":7,"reason":"completed","ts":1732...}
```

- 규칙: 한 턴 종료 시 두 프레임을 **동일 `turnId`/`reason`으로 연속 발행**한다(하위 호환). `agent_settled`는 서브에이전트 단위 종료를, `session_settled`는 세션 단위 정지를 의미하며, 단일 에이전트 턴에서는 1:1로 대응한다.
- `reason` 허용값: `"completed" | "aborted" | "error" | "timeout"`.
- Rust 측 타입:

```rust
#[derive(serde::Serialize)]
pub struct AgentSettledEvent<'a> { pub agent_id: &'a str, pub session_id: &'a str, pub turn_id: u64, pub reason: &'a str, pub ts: i64 }
```

- TS 측 수신 타입(`omo-nyang/src/relay/runtime.ts` 인근):

```ts
type SettledFrame = { type: "agent_settled"; agentId: string; sessionId: string; turnId: number; reason: "completed"|"aborted"|"error"|"timeout"; ts: number };
```

### 2.2 세션 JSONL v3 라인 계약 (`crates/omp-session/src/entries.rs`)

- 라인 1: 256B title slot(고정 길이 패딩, JSON 아님) → 스캐너는 **첫 줄을 무조건 스킵**.
- 라인 2..n: camelCase 엔트리 객체. `session-patterns.ts`가 요구하는 최소 필드는 `timestamp`, `message.role`, `message.content[]`, 선택적 `toolCallId`, `isError`.
- 정규화 후 반환 타입:

```ts
type ToolCallRecord = { timestamp: string; role: "assistant"|"toolResult"; name: string; arguments: unknown; id: string; isError: boolean };
```

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

### 3.1 Rust: `runtime.rs:1641` 인근 이중 발행

```rust
fn emit_turn_end(&self, turn_id: u64, reason: SettleReason) {
    let ts = now_millis();
    for agent_id in self.active_agent_ids() {          // 단일 턴이면 1개
        let ev = AgentSettledEvent { agent_id: &agent_id, session_id: self.session_id(), turn_id, reason: reason.as_str(), ts };
        self.stdout_frame(&ev);                        // {"type":"agent_settled", ...}
    }
    self.stdout_frame(&SessionSettled { session_id: self.session_id(), turn_id, reason: reason.as_str(), ts });
}
```

- `ext.rs:1598`의 내부 발행은 유지하되, stdout 발행과 **동일 헬퍼(`emit_turn_end`)를 공유**해 필드 드리프트를 막는다.

### 3.2 TS: 게이트웨이 대기 로직(`relay/runtime.ts:59` 계열)

```ts
async function waitForAgentSettled(turnId: number, timeoutMs = 30_000): Promise<SettledFrame> {
  const seen = new Set<string>();
  return await withTimeout(new Promise((resolve, reject) => {
    onStdoutLine((line) => {
      const f = safeJson(line);
      if (!f || f.type !== "agent_settled" || f.turnId !== turnId) return;
      const key = `${f.agentId}:${f.turnId}`;
      if (seen.has(key)) return;               // 중복 settle 무시
      seen.add(key); resolve(f as SettledFrame);
    });
    onExit((code) => reject(new Error(`early_exit:${code}`)));
  }), timeoutMs, () => reject(new Error("settle_timeout")));
}
```

- `session_settled`만 도착하고 `agent_settled`가 없는 구버전 엔진 대비 폴백: 동일 `turnId`의 `session_settled` 수신 시 경고 로그 1회 후 종료 처리(단, QA에서는 폴백 미발동을 판정 기준으로 삼는다).

### 3.3 주간 스캔 하네스(`session-patterns.ts:47-76` 방어)

```ts
export function scanSession(file: string): { records: ToolCallRecord[]; skipped: number } {
  const lines = readFileSync(file, "utf8").split("\n");
  const records: ToolCallRecord[] = []; let skipped = 0;
  for (const [i, raw] of lines.entries()) {
    if (i === 0) continue;                        // 256B title slot
    if (!raw.trim()) continue;
    let e: any; try { e = JSON.parse(raw); } catch { skipped++; continue; }
    if (e === null || typeof e !== "object" || !e.message) { skipped++; continue; }
    const role = e.message.role; if (role !== "assistant" && role !== "toolResult") continue;
    for (const c of Array.isArray(e.message.content) ? e.message.content : []) {
      if (c?.type !== "toolCall") continue;
      records.push({ timestamp: String(e.timestamp ?? ""), role, name: String(c.name ?? "unknown"), arguments: c.arguments ?? {}, id: String(c.id ?? ""), isError: Boolean(e.isError) });
    }
  }
  return { records, skipped };
}
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. 단일 턴 실행 시 stdout에 `agent_settled`와 `session_settled`가 **같은 `turnId`로 각 1회 이상** 관측된다.
2. `agent_settled` 미수신 상태에서 게이트웨이가 30s 타임아웃 없이 정상 종료한다(폴백 미발동).
3. fixtures의 정상/손상 JSONL 6종에 대해 `scanSession`이 예외 없이 `{records, skipped}`를 반환한다.
4. 전체 파이프라인이 아래 명령으로 그린이어야 한다.

```bash
cargo build -p omp-cli
cargo test -p omp-cli runtime::settle -- --nocapture
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"get_protocol_info"}' | ./target/debug/omt --mode rpc | grep -c agent_settled
node --test omo-nyang/test/session-patterns.test.ts
npx tsx omo-nyang/scripts/weekly-scan.ts --fixtures omo-nyang/test/fixtures/sessions --strict
```

- `--strict` 모드는 `skipped > 0`인 파일 목록을 출력하되 exit code 0을 유지한다(주간 배치 비중단 원칙).

## 5. 엣지 케이스 및 실패 복구

- **중복 settle**: 동일 `agentId:turnId` 재수신은 `seen` 셋으로 무시. 재수신 비율은 카운터로만 기록.
- **조기 종료**: `agent_settled` 없이 프로세스 exit → `early_exit:<code>`로 실패 처리하고, 부분 stdout을 보존해 사후 분석에 사용.
- **부분 라인/잘린 JSONL**: 마지막 줄이 `JSON.parse` 실패하면 `skipped++` 후 계속. 단, 파일 크기가 0이면 스킵 카운트 없이 빈 결과.
- **title slot 오인**: 첫 줄이 우연히 `{`로 시작해도 인덱스 0은 무조건 스킵하므로 오탐 없음.
- **비 UTF-8 바이트**: `readFileSync` 실패 시 `latin1` 폴백으로 재시도하고, 그래도 실패하면 해당 파일을 `unreadable` 목록에 넣고 배치는 계속.
- **확장 호스트 내부 발행과의 불일치**: `ext.rs:1598` 경로와 stdout 경로가 다른 `reason` 문자열을 쓰면 QA가 실패하도록 단위 테스트에서 두 경로의 직렬화 결과를 비교한다.
- **RPC 오류 프레임 혼동**: `rpc.rs:1094`의 `Unknown command` 응답은 `success:false` + `errorCode`를 함께 직렬화해야 `move-host.ts:61` 파싱이 깨지지 않는다. QA에서 `get_protocol_info` 호출 후 `errorCode` 필드 존재를 확인한다.
