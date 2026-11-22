# 구현 조각 14: `crates/omp-cli/src/runtime.rs:1641`·`ext.rs:1598`의 RPC stdout `agent_settled` 프레임 발행 및 하위 작업 대기 정합 구현 명세 (impl-14-ops-agent-settled-wire-frame)

## 1. 목적 및 코드 근거 (File & Line)

- 현재 tau는 턴 종료 시 `crates/omp-cli/src/runtime.rs:1641`에서 `{"type":"session_settled"}`만 stdout에 기록하고, `{"type":"agent_settled"}`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부 채널로만 발행한다. 반면 OmOMeow 하네스는 RPC stdout에서 `agent_settled`를 종료 신호로 대기한다: `omo-nyang/src/relay/runtime.ts:59`, `omo-nyang/src/relay/slice-run.ts:71`, `omo-nyang/src/fleet.ts:258`, `omo-nyang/src/agentd-sessions.ts:186`.
- 따라서 RPC 모드에서 `session_settled`와 `agent_settled`를 동일 stdout 뮤텍스로 직렬 발행하고, 하위 작업(백그라운드 태스크·셸·workpool)이 모두 정지한 뒤에만 settle이 나가도록 게이트를 둔다.
- 프레임 조립 규약은 `crates/omp-cli/src/rpc.rs:203-226`의 `response()`가 사용하는 writer와 동일해야 하며, 클라이언트 파서 `omo-nyang/src/relay/move-host.ts:4-7,61`은 `{type,id,success,data?,error?,errorCode?}`만 response로 취급하므로 `id` 없는 push 프레임은 response 매칭에서 제외되어야 한다.
- 프로토콜 협상은 `omo-nyang/src/lane-hold.ts:29-76`의 `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}` 계약을 유지한 채 가산 필드만 추가한다.
- 하위 작업 계약: `crates/omp-cli/src/tools/task.rs`(동기 배치), `crates/omp-cli/src/tools/shell_host.rs`·`jobs.rs`(proc://)·`wait.rs`(Phase 2), `crates/omp-cli/src/rpc.rs:863`의 `get_subagents` 빈 배열 스텁. 미지원 명령 처리 경로는 `crates/omp-cli/src/rpc.rs:1094`이며 본 조각은 새 RPC 명령을 추가하지 않는다(`agent_settled`는 서버 push 전용).

## 2. 상세 계약 및 데이터/타입 명세

- push 프레임(개행 1개, 단일 라인 JSON, 말미 공백 없음):

```json
{"type":"agent_settled","sessionId":"s-01H...","seq":42,"reason":"turn_end","runPaused":false,"pendingTasks":0,"pendingShells":0,"ts":1763779200123}
```

- `reason` 허용값: `"turn_end" | "task_complete" | "aborted" | "resumed" | "move_release" | "shutdown"`.
- 순서 보장: 동일 턴에서 `session_settled` → `agent_settled` 순으로 1회씩, `seq`는 세션 단위 단조 증가. `session_settled`는 하위 호환용으로 유지하되 `settle_frame_policy="session_only"`로 억제 가능.
- Rust 타입(신규 `crates/omp-cli/src/settle.rs`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleReason { TurnEnd, TaskComplete, Aborted, Resumed, MoveRelease, Shutdown }

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleNotice {
    pub session_id: String,
    pub seq: u64,
    pub reason: SettleReason,
    pub run_paused: bool,
    pub pending_tasks: usize,
    pub pending_shells: usize,
    pub ts_ms: u64,
}

pub struct SettleGate { pending_tasks: AtomicUsize, pending_shells: AtomicUsize,
                        run_paused: AtomicBool, last_seq: AtomicU64 }
impl SettleGate {
    pub fn try_emit(&self, reason: SettleReason) -> Option<SettleReason>; // 카운터 0 + run_paused 아님일 때만 Some
    pub fn note_task_started(&self, id: &str);   // st_... 등록
    pub fn note_task_finished(&self, id: &str);  // 종료 시 0이면 task_complete settle 유도
}
```

- `get_protocol_info` 가산 필드: `"settleFrameVersion":1, "settleFrames":["session_settled","agent_settled"], "settleGate":["tasks","shells","runPaused"]`.
- TS 측 타입(`omo-nyang/src/relay/settle-wire.ts` 신규):

```ts
export type SettleFrame = { type: "agent_settled"; sessionId: string; seq: number;
  reason: "turn_end"|"task_complete"|"aborted"|"resumed"|"move_release"|"shutdown";
  runPaused: boolean; pendingTasks: number; pendingShells: number; ts: number };
```

- `run_in_background` 태스크 ID(`st_...`)·`task_output`·`task_cancel`·`bash_output`·`kill_bash`·`monitor`는 모두 `SettleGate` 카운터에 등록/해제 의무를 진다. `wake_source_state`(`{source, activeCount}`, `omo-nyang/src/relay/move-host.ts` 확장 이벤트 계약)의 `activeCount > 0`이면 settle 유보.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`runtime.rs:1641` 대체:

```rust
// before: write {"type":"session_settled"} 직접 기록
// after:
if let Some(reason) = self.settle_gate.try_emit(SettleReason::TurnEnd) {
    let notice = SettleNotice {
        session_id: self.session_id.clone(),
        seq: self.settle_gate.next_seq(),
        reason,
        run_paused: self.lane_hold.run_paused(),
        pending_tasks: self.settle_gate.pending_tasks(),
        pending_shells: self.settle_gate.pending_shells(),
        ts_ms: now_ms(),
    };
    self.emit_settle(&notice)?;
}
```

`emit_settle` (rpc.rs:203-226 writer 재사용):

```rust
