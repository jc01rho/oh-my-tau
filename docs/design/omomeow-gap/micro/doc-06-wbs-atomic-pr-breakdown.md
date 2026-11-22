# 문서 조각 06: 26개 도메인 조각과 60개 마이크로 조각을 12개 독립 머지 가능 PR(Wave G1~G4)로 묶는 실행 WBS 표 (doc-06-wbs-atomic-pr-breakdown)

## 1. 목적 및 코드 근거 (File & Line)

이 조각은 tau(oh-my-tau) 코어를 OmOMeow 하네스가 실제로 호출하는 계약에 맞춰 12개 PR로 분할 실행하기 위한 WBS를 확정한다. 각 PR은 단독으로 빌드·테스트·머지 가능해야 하며, 선행 PR이 없으면 컴파일이 깨지는 경우에만 Wave 순서를 강제한다.

- RPC 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 미지원 분기는 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 code 없이 실패한다.
- 응답 조립: `rpc.rs:203-226`의 `response()`. 소비자 `omo-nyang/src/relay/move-host.ts:4-7, 61`은 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `errorCode`가 없으면 원인 분기가 불가능하다.
- 보류·재개: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`.
- 프로토콜/재개: `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`, `reload()` → `{cancelled:false}`, `resume_session()` → `session_resumed` 이벤트 + `get_state`에서 `paused:false, runPaused:false`.
- 세션 이동: `omo-nyang/src/relay/move-host.ts:37-39`(스폰 인자), `move-operations.ts:104-225`(`move_session` op 세트), `session-move-extension.ts:116-139`(확장 이벤트).
- CLI 플래그: `omo-nyang/src/gateway.ts:1167`은 `--skill <디렉터리경로>`를 전달하나 `crates/omp
