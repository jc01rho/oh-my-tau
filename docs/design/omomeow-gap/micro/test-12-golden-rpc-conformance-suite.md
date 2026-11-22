# 테스트 조각 12: `omo-nyang` 게이트웨이 RPC 트레이스 리플레이 적합성 골든 테스트 스위트 명세 (test-12-golden-rpc-conformance-suite)

## 1. 목적 및 코드 근거 (File & Line)

`omo-nyang` 게이트웨이가 실제로 발행한 RPC 요청/응답 트레이스를 캡처해 `oh-my-tau`의 RPC 디스패처에 그대로 재생(replay)하고, 프레임 스키마·에러 코드·상태 전이를 골든 파일과 비교한다. 게이트웨이 코드를 수정하지 않고 tau 측 회귀를 조기에 검출하는 것이 목적이다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령 분기: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` — unknown은 성공 케이스가 아니라 별도 분류 대상이다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()`.
- 게이트웨이 측 파서: `omo-nyang/src/relay/move-host.ts:4-7, 61` — `{type:"response", id, success, data?, error?, errorCode?}`. 실패 시 `code`와 `errorCode`를 **동시에** 직렬화해야 파서가 깨지지 않는다.
- 보류/재개 계약: `omo-nyang/src/lane-hold.ts:29-76`.
- 세션 이동 계약: `omo-nyang/src/relay/move-host.ts:37-39`, `move-operations.ts:104-225`, `session-move-extension.ts:116-139`.
- 종료 신호 격차: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고, `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 발행된다. `omo-nyang`은 `relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`에서 `agent_settled`를 기다린다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 트레이스 레코드 스키마 (JSONL, 1줄 1레코드)

```ts
interface RpcTraceRecord {
  seq: number;                 // 재생 순서, 재정렬 금지
  direction: "req" | "res";
  id: string;                  // 정규화 대상: "<id:N>"
  command?: string;            // direction==="req"일 때만. rpc.rs:1094의 other와 동일 문자열
  payload: unknown;            // req: 원본 cmd Value / res: response() 산출 프레임
}
```

### 2.2 골든 커버리지 매트릭스

| 명령 | 근거 | 요청 페이로드 | 기대 응답 |
|---|---|---|---|
| `set_client_info` | lane-hold.ts:29-76 | `{width:80, capabilities:["interactive_user"]}` | `success:true` |
| `interrupt` | lane-hold.ts | `{intent:"stop"\|"abort"}` | `success:true`, 이후 `get_state.paused:true` |
| `get_state` | lane-hold.ts | `{}` | `{paused, runPaused, pauseSeq}` |
| `get_protocol_info` | lane-hold.ts | `{}` | `{protocolVersion:2, serverVersion, engine:"oh-my-tau", commands:[...]}` |
| `reload` | lane-hold.ts | `{}` | `{cancelled:false}` |
| `resume_session` | lane-hold.ts | `{}` | `get_state.paused:false, runPaused:false` + `session_resumed` 이벤트 |
| `move_session` | move-operations.ts:104-225 | `{op:"prepare"\|"export"\|"import"\|"status"\|"finish"\|"cancel", moveId, sessionPath, bundleDir, cwd, from, answer}` | op별 결과 |
| `release_session` | move-host.ts:37-39 | `{sessionId, reason:"move", moveId}` | `{session_path}` |
| `extension_request` | session-move-extension.ts:116-139 | `{sessionId?, name, data?}` | `api.rpc.handle(name, handler)` 라우팅 결과 (`omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit`) |
| `get_subagents` | rpc.rs:863 | `{}` | 현재 `[]` 스텁 — **xfail(known-gap)** 등록 |

### 2.3 에러 프레임 계약

`Err(CmdError::new(msg))` 경로는 `success:false`, `error:msg`, `errorCode`를 모두 채운다. 골든에서 검증할 코드: `stop_requires_interactive_client`, `connection_role_fixed`, 그리고 `Unknown command: <other>` 계열.

### 2.4 정규화 규칙 (비교 전 적용)

세션 ID `sess_[0-9a-f]+` → `<sid>`, 절대경로 → `<cwd>`, 타임스탬프/소요시간 → `<ts>`, `serverVersion` → `<version>`, `commands` 배열은 사전순 정렬. 정규화로 제거되지 않는 차이는 실패로 간주한다.

## 3. 구체적 변경/검증 로직

### 3.1 캡처기 (omo-nyang)

```ts
// omo-nyang/src/relay/move-host.ts:61 응답 파싱 지점
const frame = JSON.parse(line);
if (frame.type === "response") {
  trace?.push({ seq: nextSeq(), direction: "res", id: frame.id, payload: frame });
  assertFrameShape(frame); // success===false 이면 error와 errorCode 둘 다 존재
}
```

### 3.2 재생 하네스 (Rust)

```rust
// crates/omp-cli/tests/golden_rpc.rs
#[tokio::test]
async fn replay_gateway_trace() {
    let rt = Arc::new(Runtime::test_new());
    let mut diffs = Vec::new();
    for rec in load_jsonl("tests/golden/rpc/*.jsonl") {
        if rec.direction != Direction::Req { continue; }
        let got = dispatch(&rt, &rec.payload, "gateway").await;   // rpc.rs:593
        let frame = response(rec.id.clone(), got);                // rpc.rs:203-226
        match diff(&normalize(rec.expected), &normalize(frame)) {
            Diff::None => {}
            d => diffs.push((rec.seq, rec.command.clone(), d)),
        }
    }
    assert!(diffs.iter().all(|(_, _, d)| d.is_known_gap()), "{diffs:#?}");
}
```

### 3.3 차이 분류

`STRUCT`(필드 누락/타입 불일치), `CODE`(errorCode 불일치), `TEXT`(메시지 문구만 상이), `STATE`(`paused`/`runPaused`/`pauseSeq` 불일치). `STRUCT`와 `CODE`만 빌드 실패로 처리하고, `TEXT`는 경고, `STATE`는 순서 위반 여부를 함께 보고한다.

### 3.4 실행 모드

단위 재생은 in-process `dispatch` 호출로, 통합 재생은 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`(move-host.ts:37)로 동일 트레이스를 stdin 주입한다.

## 4. 완료 판정 기준 (
