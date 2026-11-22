# 리뷰 조각 02: 스트리밍·도구 실행 중 `interrupt:stop`과 `move_session:export` 경합 조건 및 락 순서(`Mutex`/`RwLock`) 감사 (rev-02-pause-and-move-concurrency-audit)

## 1. 목적 및 코드 근거 (File & Line)

- 대상 경합: (A) `interrupt({intent:"stop"})`가 진행 중인 스트리밍/도구 실행을 정지시키는 경로, (B) `move_session({op:"export"})`가 세션 파일 스냅샷을 뜨는 경로. 두 경로가 같은 세션 상태와 세션 JSONL writer를 동시에 만질 때 `paused`/`runPaused`/`pauseSeq` 관측값이 뒤집히거나, export 번들이 잘린 JSONL 라인을 담는 문제를 감사한다.
- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()`. 소비자 `omo-nyang/src/relay/move-host.ts:4-7, 61`은 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 **동시에** 직렬화해야 클라이언트 분기가 성립한다.
- 보류/재개 계약: `omo-nyang/src/lane-hold.ts:29-76`. `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`.
- 이동 계약: `omo-nyang/src/relay/move-host.ts:37-39`가 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 호스트를 띄우고, `move-operations.ts:104-225`가 `prepare|export|import|status|finish|cancel`을 순차 호출. 확장 측 `session-move-extension.ts:116-139`는 `session_move_export`/`session_move_import` 이벤트에서 `ctx.sessionManager.getSessionFile()`로 경로를 얻는다.
- 세션 기록 형식: `crates/omp-session/src/entries.rs`(JSONL v3, 256B title slot + camelCase entries) — export는 이 파일을 **라인 단위로 온전하게** 복사해야 한다.
- 종료 신호 불일치(감사 부수 항목): `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고, `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`은 `agent_settled`를 기다리므로 stop 이후 정지 확인이 타임아웃될 수 있다.

## 2. 상세 계약 및 데이터/타입 명세

요청/응답 스키마(모두 `id` 동반, `rpc.rs:203-226` 프레임):

```jsonc
{"id":7,"cmd":"set_client_info","args":{"width":80,"capabilities":["interactive_user"]}}
{"id":8,"cmd":"interrupt","args":{"intent":"stop"}}            // "stop" | "abort"
{"id":9,"cmd":"get_state"}   // data: {"paused":true,"runPaused":true,"pauseSeq":41}
{"id":10,"cmd":"move_session","args":{"op":"export","moveId":"mv-1","sessionPath":"/s/x.jsonl","bundleDir":"/b/mv-1","cwd":"/w","from":"omo-nyang","answer":null}}
{"id":11,"cmd":"release_session","args":{"sessionId":"s1","reason":"move","moveId":"mv-1"}} // data: {"session_path":"/s/x.jsonl"}
{"id":10,"cmd":"response","success":false,"error":"export in flight","code":"move_in_progress","errorCode":"move_in_progress"}
```

Rust 측 신규 타입(제안, `rpc.rs:593` 인근 모듈):

```rust
pub enum PauseIntent { Stop, Abort }
pub struct HoldState { pub paused: bool, pub run_paused: bool, pub pause_seq: u64 } // u64: AtomicU64
pub struct MoveEntry { pub move_id: String, pub phase: MovePhase, pub export_lease: Option<ExportLease> }
pub enum MovePhase { Prepare, Exporting, Exported, Importing, Finished, Cancelled }
pub struct ExportLease { pub seq_at_start: u64, pub guard: OwnedRwLockReadGuard<SessionStore> }
```

TS 측 대응(`move-host.ts:4-7`): `type RpcFrame = {type:"response"; id:number; success:boolean; data?:unknown; error?:string; errorCode?:string}`.

락 순서 불변식(위반 시 `debug_assert` + `deadlock_detection`):

| 순위 | 락 | 위치 |
|---|---|---|
| L0 | `AtomicU64 pause_seq` (무락, CAS) | HoldState |
| L1 | `Mutex<HoldState>` | 보류 상태 |
| L2 | `RwLock<SessionStore>` (write=append/flush, read=snapshot) | omp-session |
| L3 | `Mutex<HashMap<String, MoveEntry>>` | MoveRegistry |
| L4 | `Mutex<JobTable>` (proc://, jobs.rs) | 셸 잡 |

규칙: 항상 L1→L2→L3→L4 방향으로만 획득하고, L0은 어떤 락도 잡지 않은 상태에서 먼저 갱신한다. `export`는 L2 read → L3 write 순서를 지키고, `interrupt`는 L1 → L0 → L2 write 순서를 지킨다(L0는 L1 보유 중 갱신하되 다른 락을 요구하지 않음).

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// rpc.rs: interrupt 핸들러 (stop/abort)
async fn on_interrupt(rt: &Arc<Runtime>, intent: PauseIntent) -> CmdResult {
    let hold = rt.hold.lock();                       // L1
    if !hold.client_caps.contains("interactive_user") {
        return err("stop_requires_interactive_client");   // lane-hold.ts:29-76
    }
    let seq = rt.pause_seq.fetch_add(1, SeqCst) + 1;  // L0, 락 없이 먼저
    drop(hold);
    rt.turn.cancel(intent);                          // 스트리밍 취소 토큰
    if matches!(intent, PauseIntent::Abort) { rt.jobs.kill_all().await; } // L4, 단독 획득
    let mut store = rt.session.write().await;         // L2 write
    store.flush_partial_line().await;                 // 잘린 라인 0 보장
    drop(store);
    let mut h = rt.hold.lock(); h.paused = true; h.run_paused = true; h.pause_seq = seq;
    rt.emit_settled_if_idle().await;                  // runtime.rs:1641 + agent_settled 보강
    ok(json!({"paused":true,"runPaused":true,"pauseSeq":seq}))
}

// rpc.rs: move_session op=export
async fn on_export(rt: &Arc<Runtime>, mv: MoveArgs) -> CmdResult {
    let mut reg = rt.moves.lock();                    // L3
    if reg.values().any(|e| e.phase == MovePhase::Exporting) { return err_code("move_in_progress"); }
    let seq = rt.pause_seq.load(SeqCst);
    let entry = reg.entry(mv.move_id.clone()).or_insert(MoveEntry::new(&mv.move_id));
    entry.phase = MovePhase::Exporting; drop(reg);
    rt.turn.wait_idle_or_hold(Duration::from_secs(5)).await?; // 진행 턴 종료 또는 hold 대기
    let store = rt.session.read().await;              // L2 read (스냅샷)
    store.copy_jsonl_atomic(&mv.session_path, &mv.bundle_dir).await?; // tmp+rename
    drop(store);
    let mut reg = rt.moves.lock();                    // L3 재획득, 순서 유지
    reg.get_mut(&mv.move_id).unwrap().phase = MovePhase::Exported;
    ok(json!({"moveId":mv.move_id,"phase":"exported","pauseSeq":seq}))
}
```

TS 측(`move-host.ts`): `interrupt` 응답 후 반드시 `get_state`로 `pauseSeq` 증가를 확인하고, `export` 응답의 `pauseSeq`가 `get_state` 값과 같을 때만 번들을 신뢰한다. `errorCode === "move_in_progress"`면 지수 백오프(50ms→800ms, 최대 5회) 재시도. `session_move_export` 이벤트(`session-move-extension.ts:116-139`)의 `event.signal`은 export 완료 후에만 발행.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. 1,000회 반복에서 `interrupt(stop)`과 `move_session(export)` 동시 주입 시 데드락 0건(5초 내 전부 응답), export 번들 JSONL의 모든 라인이 `serde_json` 파싱 성공(잘린 라인 0건).
2. `get_state`의 `pauseSeq`가 단조 증가하며, export 응답의 `pauseSeq`와 불일치하는 번들은 생성되지 않음.
3. 실패 응답은 `code`와 `errorCode`를 모두 포함(`move_in_progress`, `stop_requires_interactive_client`, `connection_role_fixed`, `session_busy`).
4. stop 이후 2초 내 `session_settled` **및** `agent_settled`가 stdout에 각 1회 방출(`runtime.rs:1641`/`ext.rs:1598` 정합).
5. `resume_session` 후 `get_state`가 `{paused:false, runPaused:false}`이고 `session_resumed` 이벤트 1회.

```bash
cargo test -p omp-cli --test pause_move_race -- --test-threads=1
cargo test -p omp-cli --features deadlock_detection
RUSTFLAGS="--cfg loom" cargo test -p omp-session --test export_snapshot_loom
cargo run -p omp-cli -- --mode rpc --multi-session --session-runtime in-process -e ./ext/move.js
(cd omo-nyang && vitest run src/relay/move-host.test.ts src/lane-hold.test.ts)
```

## 5. 엣지 케이스 및 실패 복구

- export 중 `interrupt(abort)`: L4 잡 종료가 L2 flush보다 먼저 완료되도록 abort 경로에서 `kill_all` → `flush_partial_line` 순서 고정. export는 `wait_idle_or_hold` 타임아웃 시 `move_in_progress`로 실패하고 `phase`를 `Prepare`로 되돌린다.
- 보류 중 재-interrupt: `pauseSeq`만 증가, `paused` 유지, 이벤
