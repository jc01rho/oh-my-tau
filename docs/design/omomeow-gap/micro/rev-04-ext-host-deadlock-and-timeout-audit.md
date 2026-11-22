# 리뷰 조각 04: ext-host/host.ts의 inNowait 배리어와 extension_request 양방향 JSON-RPC 호출 시 교착(Deadlock) 방지 감사 (rev-04-ext-host-deadlock-and-timeout-audit)

## 1. 목적 및 코드 근거 (File & Line)

목적은 ext-host/host.ts가 `extension_request`를 처리하는 동안 수신 루프가 `inNowait` 배리어에 의해 멈춰 양방향 JSON-RPC가 서로의 응답을 기다리는 교착을 제거하고, 모든 대기 호출에 상한 시간과 복구 경로를 부여하는 것이다.

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 종결된다.
- 응답 프레임 조립: `rpc.rs:203-226`의 `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 만든다. `omo-nyang/src/relay/move-host.ts:4-7, 61`이 같은 형태를 파싱하므로 실패 시 `code`와 `errorCode`를 동시에 직렬화해야 한다.
- 확장 라우팅 계약: `extension_request({sessionId?, name, data?})`는 `api.rpc.handle(name, handler)`로 전달되며 `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"`이 등록 대상이다(`omo-nyang/src/relay/move-operations.ts:104-225`, `session-move-extension.ts:116-139`).
- 이벤트 경로: `session_move_export`/`session_move_import`는 `event={dir, signal}`, `ctx.sessionManager.getSessionFile()`를 사용하고, `api.events.on("wake_source_state", {source, activeCount})`가 별도로 흐른다.
- 종료 신호 불일치: `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`은 `{"type":"agent_settled"}`를 기다리지만 tau는 `crates/omp-cli/src/runtime.rs:1641`에서 `session_settled`만 stdout에 쓰고 `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 이 불일치도 동일한 "영구 대기" 계열 결함으로 함께 감사한다.
- 보류·재개 계약: `omo-nyang/src/lane-hold.ts:29-76`의 `set_client_info` → `interrupt` → `get_state` 순서와 `stop_requires_interactive_client`, `connection_role_fixed`, `resume_session`의 `paused:false`/`runPaused:false`가 교착 감사 중에도 보존되어야 한다.

## 2. 상세 계약 및 데이터/타입 명세

```ts
type RpcId = string | number;
interface RpcRequest  { type: "request";  id: RpcId; method: string; params?: unknown; }
interface RpcResponse { type: "response"; id: RpcId; success: boolean;
                        data?: unknown; error?: string; code?: string; errorCode?: string; }
interface PendingEntry { method: string; resolve(v: unknown): void;
                         reject(e: Error): void; timer: ReturnType<typeof setTimeout>; }
```

배리어 계약은 다음 다섯 조항으로 고정한다.

1. 수신 루프는 `inNowait` 값과 무관하게 항상 읽고 분배한다. 배리어는 송신 측 대기만 제한하며 수신을 절대 정지시키지 않는다.
2. `inNowait > 0`인 구간에서 `request()`를 호출하면 기본적으로 `nowait_reentrancy_forbidden`으로 즉시 거부한다. 예외는 `allowReentrant:true`를 명시하고 상대 피어가 동시 디스패치를 지원한다고 선언한 경우뿐이다.
3. 모든 `request()`는 `timeoutMs`(기본 30000, 이동 작업은 120000)를 가지며 만료 시 `pending`에서 제거한 뒤 `extension_request_timeout`으로 거부한다.
4. 실패 응답은 `success:false`, `error
