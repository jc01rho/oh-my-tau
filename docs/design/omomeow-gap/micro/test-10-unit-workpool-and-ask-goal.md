# 테스트 조각 10: `workpool` 키 큐 생명주기 및 `ask_user_question`·`create_goal`·`update_goal` 스키마 변환 골든 테스트 명세 (test-10-unit-workpool-and-ask-goal)

## 1. 목적 및 코드 근거 (File & Line)

OmOMeow 하네스가 호출하는 도구 4계열(`workpool`, `ask_user_question`, `create_goal`/`get_goal`/`update_goal`)에 대해 (a) RPC 디스패치 배선, (b) OmO 스키마에서 tau 내부 스키마로의 인자 변환, (c) 실패 시 `code`+`errorCode` 동시 직렬화를 골든 스냅샷으로 고정한다. 본 조각은 단위 테스트만 다루며 실제 프로세스 기동은 4번 항목의 선택적 통합 케이스로 한정한다.

| 근거 | 위치 | 요지 |
|---|---|---|
| 디스패치 진입점 | `crates/omp-cli/src/rpc.rs:593` (`async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`) | 모든 명령이 통과하는 단일 분기점. 테스트 대상 함수 |
| 미지원 명령 처리 | `crates/omp-cli/src/rpc.rs:1094` (`other => Err(CmdError::new(format!("Unknown command: {other}")))`) | 신규 명령 미배선 시 이 arm으로 흘러 실패함을 회귀 검증 |
| 응답 프레임 조립 | `crates/omp-cli/src/rpc.rs:203-226` (`response()`) | `{type,id,success,data?,error?,errorCode?}` 생성 지점 |
| 소비자 파서 | `omo-nyang/src/relay/move-host.ts:4-7, 61` | `errorCode`가 없으면 실패 원인이 소실됨 |
| 프로토콜 협상 | `omo-nyang/src/lane-hold.ts:29-76` (`get_protocol_info()` → `protocolVersion:2`, `commands:[...]`) | 신규 명령이 `commands` 배열에 노출되어야 클라이언트가 호출 가능 |
| 스텁 회피 근거 | `crates/omp-cli/src/rpc.rs:863` (`get_subagents` 빈 배열 스텁) | "필드 없는 성공"을 통과시키지 않도록 스냅샷에 필수 필드 강제 |
| 대조군(동기 배치) | `crates/omp-cli/src/tools/task.rs`, `crates/omp-task` | `context+tasks[]` 동기 실행만 존재. `workpool`은 이와 별개의 키 큐 의미론 |
| 종료 동기화 | `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71` | 통합 케이스에서 `{"type":"agent_settled"}` 대기 필요(현 tau는 `runtime.rs:1641`에서 `session_settled`만 송신) |
| 호스트 기동 | `omo-nyang/src/relay/move-host.ts:37` | `omt --mode rpc --multi-session --session-runtime in-process -e <ext>` |

현행 격차: `crates/omp-cli/src/tools/`에 `workpool`(create/push/close/inspect/cancel/yield) 도구가 없고, 질문은 `ask({html}|legacy questions[])` 단일 도구, 목표는 `goal({op})` 단일 도구만 존재한다. 따라서 OmO 스키마 4종을 받는 어댑터를 신규 추가하고, 그 변환 결과를 골든으로 고정한다.

## 2. 상세 계약 및
