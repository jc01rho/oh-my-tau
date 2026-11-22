# tau 게이트웨이 연동 QA 시나리오 설계 (조각 22-qa-gateway-e2e)

## 1. 현재 상태 (근거)

조사 기준: `~/git/oh-my-tau` (github.com/jc01rho/oh-my-tau, 방금 클론, 수정 없음), 게이트웨이 기준 소스 `~/Work/omo-nyang`.

- RPC 디스패치는 `crates/omp-cli/src/rpc.rs:593` `dispatch()` match 하나이고 미지 커맨드는 `rpc.rs:1094` `"Unknown command: {other}"` 로 떨어진다. 존재하는 커맨드: `negotiate_protocol:595`, `prompt:602`, `steer|follow_up:654`, `abort:677`, `abort_and_prompt:681`, `new_session:707`, `switch_session:714`, `open_session:730`, `branch|fork:746`, `get_state:766`, `set_model:883`, `set_thinking_level:918`, `compact:962`, `abort_bash:994`, `get_session_stats:998`, `get_messages:1030`, `get_subagents:863`, `cancel_subagent:865`, `steer_subagent:869`, `set_host_tools:824`.
- 없는 8종(`interrupt`, `set_client_info`, `get_protocol_info`, `reload`, `resume_session`, `release_session`, `extension_request`, `move_session`)은 전부 `rpc.rs:1094` 로 간다. `cancel_subagent` 는 `rpc.rs:865-868` 에서 `{"cancelled": false}` 만 돌려주고 실제 취소를 하지 않는다.
- `ready` 프레임은 `rpc.rs:1420-1426` 에서 `protocolVersion: 1`, `supportedProtocolVersions: [1,2]`; `negotiate_protocol` 성공 후 `FrameEncoder` 가 v2(`rpc_chunk`)로 전환된다(`rpc.rs` FrameEncoder `encode`, 약 1295-1360). 프레임 상한 `MAX_FRAME_BYTES = 1_048_576`(`rpc.rs:44`).
- 종료 신호: tau는 `agent_end`(`crates/omp-cli/src/events.rs:726`, `:737`)와 `prompt_result`/`session_settled`(`crates/omp-cli/src/runtime.rs:1629-1641`)를 보낸다. `agent_settled` 는 `crates/omp-cli/src/ext.rs:1593-1596` 에서 `host.has_handlers("agent_settled")` 일 때만 확장 이벤트로 emit 되므로, 확장을 하나도 안 실으면 게이트웨이가 기다리는 프레임이 영원히 오지 않는다(`crates/omp-ext/src/host.rs:575`).
- `get_state` 응답(`runtime.rs:1961-1990`)은 `isStreaming`, `isCompacting`, `queuedMessageCount`, `isSettled` 를 주지만 `isBashRunning`·`pendingMessageCount` 키는 없다.
- CLI 파서: `STRING_FLAGS`(`crates/omp-cli/src/cli.rs:97-136`), `OPTIONAL_FLAGS`(`:138`), `VALUELESS_FLAGS`(`:142-168`). 미지 플래그는 모아 두었다가 `cli.rs:513-520` 에서 `unknown flag(s): ...` 로 프로세스를 죽인다. 따라서 `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`, 게이트웨이가 쓰는 `--skill` 이 전부 기동 실패다.
- RPC 모드에서 `--session <path>` 는 파싱만 되고 쓰이지 않는다: `crates/omp-cli/src/main.rs:508-512` 가 `resume_target` 을 `None` 으로 두고("the rpc runtime owns session lifecycle"), 세션 파일은 `main.rs:487` → `crates/omp-cli/src/persist.rs:502-517` 경로로 `~/.omp-rs/sessions/<slug>`(`persist.rs:62-66`)에 만들어진다.
- 설정 루트는 `~/.omp/agent/config.yml` + `OMP_PROFILE` 프로필(`crates/omp-settings/src/settings.rs:101-130`, `:459-470`). `omo.jsonc` 를 읽는 코드는 없다. 이 장비의 `~/.omo/omo.jsonc` 에는 `fleet-small`/`fleet-medium` 동시 실행 프로필이 있다.
- 도구 등록은 `crates/omp-cli/src/tools.rs:322-410`: `read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask, todo, web_search, lsp, checkpoint, rewind`. `task_send/task_output/task_cancel`, `bash_output/kill_bash/monitor`, `memory/memory_apply_patch` 는 없다. `workpool` 은 도구가 아니라 eval 커널 내부 함수다(`tools/eval_agents.rs:330`, `:1759`).
- 백그라운드 셸은 `bash` 도구의 `async`/`name`/`ready` 인자와 `crates/omp-cli/src/tools/jobs.rs` 레지스트리로 존재한다(`tools.rs` bash 스키마, `crates/omp-cli/src/tools/bash.rs:10-12`). 폴링용 조회 도구만 없다.
- 하위 작업은 `crates/omp-task` 의 자식 프로세스다(`crates/omp-cli/src/tools/task.rs:5`, `crates/omp-task/src/runner.rs:253` `TaskRunner::new`, `:286` `spawn`, `:306` `spawn_all`). 동시성은 `task.maxConcurrency`(`tools/task.rs:47-49`)만 본다.
- 세션 형식은 omp JSONL v3(`crates/omp-session/src/lib.rs:1-25`, 헤더 `{"type":"session","version":3,...}` `:225`), 엔트리 종류는 `crates/omp-session/src/entries.rs:462-479`.
- 확장 호스트 `ext-host/host.ts` 에는 `api.rpc.handle` 표면이 없고, 미지 메서드는 `-32601 method not found`(`ext-host/host.ts:1060`). `session_move_export/import` 이벤트도 없다.
- 기존 e2e 자산: `tests-e2e/tui/run.py`(격리 HOME + 목 프로바이더, `make_home` 이 `models.yml` 생성 `run.py:41-60`), `tests-e2e/tui/mock_provider.py`(모델 `text/tool/tools/guards/fail/retry/full/slow`), `tests-e2e/ext/conformance.py`(rpc 모드 포함). 빌드 산출물 경로 `target/shared/release/omt`(`run.py:38`).

## 2. 게이트웨이가 요구하는 계약

- 워커 기동: `[<engine>, "--mode","rpc","--session",<path>, "--append-system-prompt",..., "--skill",<dir>, ...roleFlags]` (`src/gateway.ts:1077-1078`). `roleFlags` 는 `--model`, `--thinking`, `--extension`, `--no-model-fallback`, `--no-recommended-models` 를 덧붙인다(`src/model-routing.ts:640-645`).
- 정착 판정: `agent_settled` 수신 시 턴 종료(`src/brain.ts:330-351`), `agent_end` 는 abort 소스 해소용(`src/brain.ts:322-326`). 턴 결과는 `prompt_result` 성격의 이벤트와 `get_state` 로 보조 판정.
- 유휴 판정: `get_state` 가 `isStreaming=false`, `isCompacting=false`, `isBashRunning=false`, `pendingMessageCount=0` (`src/lane-idle.ts:17,23-26`).
- 보류·재개: `set_client_info{capabilities:["interactive_user"]}` → `interrupt{intent:"stop"}` → `get_state.paused` 확인(`src/lane-hold.ts:29-33`), 재개는 `get_protocol_info.serverVersion` 으로 엔진 세대 판단 후 `reload`/`resume_session`(`src/lane-hold.ts:56-70`).
- 확장 질의: `{type:"extension_request", name:"omomeow.move-readiness"}`(`src/relay/agentd-sessions.ts:141`, `src/relay/move-operations.ts:212`), 피드백은 `omo.feedback.prepare/submit`(`src/omo-feedback.ts:41,50`).
- 세션 이동: 별도 호스트를 `--multi-session --session-runtime in-process -e <ext>` 로 띄우고(`src/relay/move-host.ts:37-38`) `move_session` op `prepare/cancel/export/import/status/finish`(`src/relay/move-operations.ts:97-220`), 확장 쪽은 `session_move_export/import` + `api.rpc.handle("omomeow.move-readiness")`(`src/session-move-extension.ts:116-139`).
- 세션 파일: 게이트웨이는 `--session` 으로 준 경로의 JSONL 을 senpi 형식으로 읽고 쓴다(`src/session-evidence.ts:7`, `src/session-move.ts:41`).

## 3. 설계

QA 하네스는 tau를 게이트웨이에 얹기 전에 계약만 따로 검증하는 3층 구조로 만든다.

1. 계약 드라이버 `tests-e2e/gateway/driver.py`: 게이트웨이의 `Brain` 대신 stdio JSONL 만 주고받는 최소 클라이언트. `gateway.ts:1077` 의 argv 를 그대로 조립하고, 프레임을 `frames.jsonl` 로 남기며, 각 시나리오는 "기대 프레임 순서 + 종료 조건"만 선언한다. 종료 조건은 시간이 아니라 프레임 술어로 쓴다(예: `type=="agent_settled"` 또는 `type=="session_settled"`).
2. 프로바이더는 `tests-e2e/tui/mock_provider.py` 를 재사용하고 `make_home`(`run.py:41-60`)으로 `~/.omp` 를 격리한다. 모델 고정 검증을 위해 `fail`/`retry` 모델을 쓴다.
3. 어댑터 판정기: tau가 아직 못 주는 프레임은 "미구현"으로 분류하되, 하네스 자체가 틀리지 않았는지 보려고 같은 시나리오를 `omo`(있으면 `mhc`)로 한 번 돌려 기준선을 만든다. `conformance.py` 의 EXPLAINED 방식(허용 차이 목록 + 사유)을 그대로 차용한다.

합격 기준은 두 단계로 둔다. 게이트 A(계약 최소선): 워커 기동 성공, `prompt` 1회 정착, `get_state` 유휴 판정 성립. 게이트 B(게이트웨이 실사용): 7축 전부 통과. A 없이는 어떤 축도 측정 불가이므로 A를 먼저 잠근다.

프레임 판정 규칙: `agent_settled` 는 확장이 없으면 나오지 않으므로(`ext.rs:1593`), 게이트 A에서는 `session_settled`(`runtime.rs:1641`)를 임시 정착 신호로 인정하되 "게이트웨이 계약 미충족"으로 별도 표기한다.

## 4. 구현 단계 (커밋 단위)

1. `test(gateway): stdio 계약 드라이버와 격리 HOME 하네스` — `tests-e2e/gateway/driver.py`, `frames.jsonl` 스키마, 기준선 실행 옵션.
2. `test(gateway): 정상 흐름·유휴 판정 시나리오` — 게이트 A. tau는 기동 실패(`--skill`, `cli.rs:513-520`)로 즉시 red 가 되는 것이 정상이다.
3. `feat(cli): 게이트웨이 기동 플래그 수용` — `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`, `--skill` 을 `cli.rs` 테이블에 넣고 `--skill` 은 `--skills` 로 정규화. rpc 모드에서 `--session <path>` 를 실제 세션 파일로 바인딩(`main.rs:508-512` 수정).
4. `feat(rpc): interrupt/set_client_info/get_protocol_info/reload/resume_session/release_session` — `rpc.rs` dispatch 확장, `get_state` 에 `paused`/`runPaused`/`isBashRunning`/`pendingMessageCount` 추가(`runtime.rs:1961-1990`).
5. `feat(rpc): agent_settled 를 확장 없이도 emit` — `ext.rs:1593` 의 `has_handlers` 조건 밖으로 분리, `gatewayRequestId` 스탬프 경로 정리.
6. `feat(ext): extension_request 와 api.rpc.handle` — `ext-host/host.ts` 에 rpc 핸들 테이블, `crates/omp-ext/src/host.rs` 에 요청/응답 브리지.
7. `feat(rpc): move_session 6 op + session_move_export/import` — 이동 축. 앞 단계 5·6 이 선행 조건.
8. `feat(tools): task_send/task_output/task_cancel, bash_output/kill_bash/monitor` — 하위 작업·셸 축. `cancel_subagent`(`rpc.rs:865`)의 무동작 반환도 여기서 고친다.

## 5. 테스트

- 단위: `cli.rs` 파서 테이블(기존 `resume_optional_value` 류 테스트 옆), `rpc.rs` dispatch 각 커맨드의 응답 형태, `runtime.rs` `state()` 키 집합.
- 계약 회귀: `cargo test -p omp-cli --locked` + `just test`, `just lint`(CI 게이트).
- 하네스: `python3 tests-e2e/gateway/driver.py --rs target/shared/release/omt --only <축>`; 기준선은 `--ts $(which omo)`.
- 금지: 고정 sleep 으로 정착을 기다리는 테스트. 정착은 프레임 술어 + 상한 timeout 으로만 판정한다(하네스의 `wait_for`/`settle` 패턴을 따르되 폴링 간격은 이벤트 구동).
- 골든: 도구 스키마 변경이 있으므로 `just tools-parity`(`scripts/check-tools-parity.ts`)를 같은 커밋에서 갱신.

## 6. QA 시나리오

### 축 1 정상 흐름
준비: 격리 HOME + mock `text` 모델, 워커 argv 는 `gateway.ts:1077-1078` 그대로(`--skill` 포함).
실행: 기동 → `ready` 수신 → `prompt{message:"hello", streamingBehavior:"followUp"}` → 정착 대기 → `get_state` → `abort`.
기대: 기동 exit 0(현재는 unknown flag 로 사망), `ready.protocolVersion==1`(`rpc.rs:1422`), `agent_start`→`message_end`→`agent_end`→`prompt_result`→`session_settled` 순서, `agent_settled` 1회, `get_state.isSettled==true`, 텍스트가 `E2E-REPLY-1`.
실패 판정: 기동 3초 내 exit, `ready` 부재, `agent_settled` 부재, `prompt` 응답 `success:false`, 정착 후에도 `isStreaming==true`.

### 축 2 보류·재개
준비: mock `slow` 모델로 턴을 길게 유지, 드라이버가 `set_client_info`→`interrupt{intent:"stop"}` 전송.
실행: 보류 → `get_state` → `get_protocol_info` → `reload`/`resume_session` → `get_state`.
기대: `interrupt` 후 `paused==true`(`lane-hold.ts:31` 요구), 재개 후 `paused==false`, 잘린 턴의 `agent_end.abortSource` 가 `resolveAbort` 로 해소(`brain.ts:322-326`), 재개 턴이 같은 requestId 로 이어진다.
실패 판정: `Unknown command: interrupt`(`rpc.rs:1094`), `paused` 키 부재로 `holdSession` 예외, 재개 후에도 `runPaused==true`(`lane-hold.ts:69-71`).

### 축 3 장비 간 세션 이동
준비: `MoveHost` argv(`move-host.ts:37-38`)로 별도 프로세스, 확장 `session-move-extension.ts` 로드.
실행: `move_session{op:"prepare"}` → `export` → 번들 전송 → 다른 cwd 에서 `import` → `status` → `finish`, 중간에 `extension_request{name:"omomeow.move-readiness"}`.
기대: 6 op 모두 `success:true`, export 번들에 `engine/manifest.json` 과 `omo-nyang/`(`session-move-extension.ts:64-70`), import 후 새 경로 세션이 같은 taskId 를 유지, readiness 가 `{activeSources:[...]}`.
실패 판정: `--multi-session`/`--session-runtime` unknown flag 로 기동 실패, `move_session`/`extension_request` 가 `Unknown command`, import 후 세션 파일 부재.

### 축 4 모델 고정
준비: `--model mock/text --thinking high --no-model-fallback --no-recommended-models`(`model-routing.ts:643-644`), mock 로그 `E2E_MOCK_LOG` 활성.
실행: 기동 → `get_state.model` 확인 → `set_model` 로 다른 모델 지정 → `fail` 마커 프롬프트로 실패 유도 → `get_state` 재확인.
기대: 최초 모델이 argv 와 일치, `set_model`(`rpc.rs:883-887`) 반영, 실패 후에도 다른 모델로 자동 전환되지 않음, mock 로그의 `model` 필드가 전 구간 동일.
실패 판정: 플래그 기동 실패, 실패 턴 뒤 mock 로그에 두 번째 모델 id 등장, `get_state.model` 이 argv 와 불일치. tau의 recovery 경로가 대체 모델을 시도하는지는 미확인(§7).

### 축 5 확장 질의
준비: `-e <omo-nyang>/src/session-move-extension.ts` 로드, `ext-host` 기동 확인.
실행: `extension_request{name:"omomeow.move-readiness"}` → `extension_request{name:"omo.feedback.prepare"}` → 미등록 이름 1회.
기대: 등록 이름은 확장 반환값이 `data` 로, 미등록 이름은 `success:false` + 명확한 오류. `api.rpc.handle` 로 등록된 이름만 응답.
실패 판정: `Unknown command: extension_request`, `-32601 method not found`(`ext-host/host.ts:1060`), 응답 없이 timeout.

### 축 6 하위 작업 취소
준비: mock `tool` 모델로 `task` 호출을 유도하거나 `--tools task` 로 강제, 자식은 `omp-rs -p`(`tools/task.rs:5`).
실행: `task` 실행 중 `cancel_subagent{subagentId}` → `get_subagents` → 자식 프로세스 잔존 확인(`pgrep -f 'omt -p'`) → `abort`.
기대: `cancel_subagent` 가 `{"cancelled": true}` 를 돌려주고 자식이 종료, `get_subagents` 가 빈 목록으로 복귀, 턴은 `agent_end` 로 정상 종료.
실패 판정: `{"cancelled": false}`(`rpc.rs:865-868`), 자식 프로세스 잔존, 취소 후에도 `tool_execution_end` 미도착.

### 축 7 세션 형식 읽기
준비: tau가 만든 세션 파일 1개(`~/.omp-rs/sessions/<slug>/*.jsonl`, `persist.rs:62-66`)와 게이트웨이가 기대하는 경로를 함께 준비.
실행: `--session <tau 파일>` 로 기동 → `get_state.sessionFile` 확인 → 게이트웨이 리더로 같은 파일 파싱 → `--resume <id>` 재개.
기대: `--session` 이 지정 경로에 바인딩되어 `get_state.sessionFile` 이 그 경로와 일치, 헤더가 `{"type":"session","version":3,...}`(`omp-session/src/lib.rs:225`), 재개 시 `model_change`/`thinking_level_change` 엔트리가 복원(`runtime.rs` `session_model_state` 테스트 참조).
실패 판정: `sessionFile` 이 `~/.omp-rs/...` 로 어긋남(현재 동작, `main.rs:508-512`), 게이트웨이 리더가 v3 헤더를 거부, 재개 후 모델이 설정 기본값으로 되돌아감.

## 7. 위험과 미확인

- 최대 위험은 기능 부족이 아니라 기동 실패다. 게이트웨이 argv 의 `--skill` 이 `cli.rs:513-520` 에서 프로세스를 죽이므로, 어떤 축도 측정 전에 끝난다. QA 하네스의 첫 red 는 이것이어야 한다.
- `agent_settled` 를 `session_settled` 로 대체하면 게이트웨이의 `requestId` 상관(`brain.ts:351`)이 끊긴다. 임시 우회는 게이트 A 한정이고, B 에서는 금지.
- 유휴 판정: `lane-idle.ts:17` 은 `pendingMessageCount` 기본값 0을 허용하므로, tau가 큐에 메시지를 쌓아도(`runtime.rs:1969` `queuedMessageCount`) 게이트웨이는 유휴로 볼 수 있다. 실제 오판 여부는 미확인.
- 축 3(이동)은 `api.rpc.handle` 과 `session_move_export/import` 를 동시에 요구해 가장 큰 작업이다. 이동 축만 별도 마일스톤으로 떼는 것을 권한다.
- tau의 모델 실패 처리(`crates/omp-cli/src/recovery.rs`)가 `--no-model-fallback` 없이 대체 모델을 시도하는지 확인하지 못했다. 축 4 의 기대값은 이 확인 후 확정해야 한다.
- 세션 이동 번들의 `omo-nyang/` 레이아웃(`session-move-extension.ts:64-70`)과 tau 확장 호스트의 `event.dir` 계약이 맞는지 미확인.
- 설정 루트: tau는 `~/.omp`(`settings.rs:459-470`)만 읽고 `~/.omo/omo.jsonc` 프로필은 무시한다. 장비별 동시 실행 수를 게이트웨이가 조절하려면 `task.maxConcurrency`(`tools/task.rs:47-49`)로 매핑하는 별도 설계가 필요하다.
- 이번 조사에서 다 못한 부분: (1) 실제 `omt` 바이너리를 빌드해 7축 중 어느 것도 실행하지 못했다(설계만). (2) `omo`/`mhc` 로 기준선 실행을 하지 않아 "게이트웨이가 실제로 이 프레임들을 요구한다"는 근거는 소스 판독뿐이다. (3) `crates/omp-cli/src/runtime.rs` 의 프롬프트 거절 코드(`compacting`/`model_missing`)와 게이트웨이 `promptRefusal`(`brain.ts:12-16`, `lane-notice.ts`)의 문자열 일치 여부는 확인하지 못했다. (4) `frame` v2 `rpc_chunk` 재조립을 게이트웨이가 처리하는지 미확인.
