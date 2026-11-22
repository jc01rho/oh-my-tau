# QA 시나리오 06: 백그라운드 하위 작업(`task`, `task_send`, `task_output`, `task_cancel`) 구동 및 사망 자식 취소 전 `done` 차단 검증 (qa-06-subagent-swarm-and-dead-child-gate)

## 1. 목적 및 코드 근거 (File & Line)

- 목적: `task`의 `run_in_background` 경로로 생성된 자식 세션이 비정상 종료했을 때, `task_cancel`로 사망 자식을 회수(reap)하기 전까지 부모의 `done` 및 `agent_settled` stdout 프레임이 발행되지 않음을 검증한다.
- 현재 tau의 `task`는 `crates/omp-cli/src/tools/task.rs` + `crates/omp-task`의 `context` + `tasks[]` 배치 동기 실행만 지원하며, `run_in_background`(`st_...` ID), `task_send`, `task_output`, `task_cancel`이 없다.
- `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`에 위 4개 명령 arm을 추가해야 한다.
- `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`이므로 미구현 상태에서는 QA가 즉시 실패한다.
- `crates/omp-cli/src/rpc.rs:203-226` `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립한다. `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `errorCode`를 파싱하므로 실패 시 `code`와 `errorCode`를 동일 값으로 직렬화한다.
- `crates/omp-cli/src/rpc.rs:863`의 `get_subagents`는 빈 배열 스텁이므로, 실제 자식 레지스트리 스냅샷을 반환하도록 교체한다.
- 종료 신호 불일치: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고, `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 보낸다. `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`은 RPC stdout의 `agent_settled`를 기다린다.
- 보류 계약은 `omo-nyang/src/lane-hold.ts:29-76`을 따른다: `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 역할 변경 시 `
