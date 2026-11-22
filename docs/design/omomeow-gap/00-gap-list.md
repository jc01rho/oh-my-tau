# tau(oh-my-tau) — OmOMeow 엔진 전환용 부족 기능 목록

기준: Slack #proj-omomeow 스레드 1795324158.334689 (2026-11-22 14:56 답변), tau main e7a5378 (workspace 0.12.0).
"확인 필요"는 실측/코드 대조로 확정하지 못한 항목이다.

## 1. 게이트웨이·agentd RPC 명령 (tau v0.12.0이 "Unknown command"로 거절)

RPC 디스패치는 crates/omp-cli/src/rpc.rs 의 dispatch() 단일 match. 현재 존재하는 명령: negotiate_protocol, prompt, steer/follow_up, abort, abort_and_prompt, new_session, switch_session, open_session, branch/fork, get_state, goal, get_available_commands, get_entries, get_tree, set_todos, set_host_tools, get_subagents, cancel_subagent, steer_subagent, set_model, cycle_model, get_available_models, set_thinking_level, compact, get_session_stats, get_messages, get_messages_page, login, predict_word 등.

없는 명령:
- interrupt — 작업 중단·보류
- set_client_info — 보류된 레인에 클라이언트 정보 전달
- get_protocol_info — 엔진 버전 확인
- reload — 보류 해제 시 확장 재로딩
- resume_session — 보류 세션 재개
- release_session — 세션 이동 시 세션 해제
- extension_request — 확장에 직접 질의(세션 이동 준비 상태, omo 피드백)
- move_session — 장비 간 세션 이동(내보내기·가져오기·상태·마무리·취소)

## 2. 실행 플래그 (crates/omp-cli/src/cli.rs 에 없음)

- --multi-session, --session-runtime in-process — 세션 이동용 호스트
- --no-model-fallback, --no-recommended-models — 모델 고정·복구 체인 제어
- --skill <경로> — tau에는 --skills 형태만 존재(확인 필요)

## 3. 확장 API (crates/omp-ext)

- api.rpc.handle — 확장이 RPC 질의에 응답(세션 이동 준비 확인)
- session_move_export / session_move_import 이벤트 — 이동 시 확장 상태 이전
- 이미 있는 것: 모델 제공자 등록, 도구 등록, 이벤트 구독

## 4. 에이전트 도구

- 하위 작업: task_send, task_output, task_cancel 부재. task 는 crates/omp-task 에 있으나 동작 동일성 확인 필요
- 백그라운드 셸: bash_output, kill_bash, monitor 부재 (셸 도구는 crates/omp-cli/src/tools/bash.rs)
- 장기 기억: memory, memory_apply_patch 부재. 기억 저장소 자체가 없고 /memory 는 껍데기. 계획상 3단계 예정
- workpool (키 기반 작업 큐) 부재
- 질문·목표: ask·goal 은 있으나 ask_user_question·create_goal 입력 형식 호환 확인 필요

## 5. 하네스·운영

- 설정 루트: 플릿 ~/.omo (omo.jsonc 프로필로 장비별 동시 실행 수), tau ~/.omp — 프로필 체계 없음
- 종료 신호: 게이트웨이는 agent_settled 대기, tau는 agent_end 송신. agent_settled 는 crates/omp-cli/src/ext.rs:1598 부근에서 확장 이벤트로만 emit — 의미 동일성 확인 필요
- OmO 계층(ultrawork, ulw-loop, 내장 스킬 묶음) 대응물 없음
- 미머지 계획 단계: 3단계(/handoff 배선, 기억), 4단계(MCP 공유, RPC v2), 5단계(VCS·격리)
- 세션 파일: tau는 omp JSONL v3, 게이트웨이는 senpi 형식 — 기록 읽기 영향 확인 필요

## 규모 판단

1~3번은 tau 쪽 개발로 채울 수 있다. 4~5번은 사실상 OmO 하네스를 tau 위에 새로 옮기는 규모다.
