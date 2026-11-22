# 리뷰 조각 08: 신규 크레이트·기능 추가 시 `linux8`(glibc 2.34, `omp-judge` `--no-default-features`) 및 정적 `musl` 빌드 호환성 감사 (rev-08-glibc-musl-ci-portability-audit)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: Phase 2~3.3에서 추가되는 크레이트(`omp-task`, `omp-memory`, `omp-judge`, 셸 호스트/작업 큐, 신규 CLI 플래그)가 `linux8`(glibc 2.34) 및 `x86_64-unknown-linux-musl` 정적 링크에서 빌드·실행 가능함을 CI에서 상시 감사한다.
- RPC 표면 불변성: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` 및 응답 조립 `rpc.rs:203-226` `response()`. 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`.
- 실패 프레임 계약: `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 `code`와 `errorCode`를 동시 직렬화해야 하며, 이 스키마는 타깃(glibc/musl)에 무관하게 동일해야 한다.
- 보류·재개 계약: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 역할 변경 시 `connection_role_fixed`.
- 프로토콜 고정: `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`. 정적 빌드에서도 `protocolVersion`/`engine` 값은 동일해야 한다.
- 이동 계약: `omo-nyang/src/relay/move-host.ts:37-39`가 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 호스트를 띄우고, `move-operations.ts:104-225`가 `move_session` op(`prepare|export|import|status|finish|cancel`)을, `session-move-extension.ts:116-139`가 `session_move_export`/`session_move_import` 이벤트와 `api.events.on("wake_source_state",
