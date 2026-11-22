# `omt` 게이트웨이 카나리 운영 런북

`oh-my-tau`(`omt`)를 OmOMeow 게이트웨이(`omo-nyang`) 워커 장비 1대에 한해 시범 적용하고, 정상 동작을 확인하거나 문제 발생 시 `mhc`로 즉시 되돌리는 절차다.

기준 버전:
- `oh-my-tau`: `main` (PR `#18`, `#25`, `#26`, `#27`, `#29` 반영 시점)
- 게이트웨이 계약 원본: `omo-nyang/src` (`gateway.ts`, `brain.ts`, `lane-hold.ts`, `lane-idle.ts`, `worker-rpc.ts`, `config.ts`, `relay/agentd-sessions.ts`)

---

## 1. 사전 확인 (Preflight)

카나리 대상 워커 1대(예: `linux5` 또는 테스트 전용 워커)에서 아래를 모두 통과한 뒤에만 2절의 설정 전환을 진행한다.

1. **배포 바이너리 동일성 및 버전 확인**
   - `ubuntu-24.04` 이상(glibc >= 2.38)은 `omt-linux-x64` / `omt-linux-arm64`, EL9(glibc 2.34) 등 구형 glibc 장비는 정적 바이너리 `omt-linux-x64-musl`을 사용한다(musl 빌드는 ONNX 런타임 미포함).
   - 배포 대상 경로(예: `/home/<user>/.local/bin/omt`)의 바이너리 해시(`sha256sum /home/<user>/.local/bin/omt`)가 릴리즈 아티팩트 `.sha256`과 일치하는지 확인하고 실행한다:
     ```bash
     sha256sum /home/<user>/.local/bin/omt
     /home/<user>/.local/bin/omt --version
     /home/<user>/.local/bin/omt --mode rpc --help
     ```
2. **`bun` `PATH` 등록 및 확장 호스트 확인**
   - `omo-nyang`은 워커 기동 시 `-e <root>/src/model-provider.ts` 확장을 붙이고, 사전 적합성 테스트의 라이브 `extension_request` 검증 역시 `PATH`에서 `bun`을 찾는다. `config.json`의 `worker.bun` 설정만으로는 사전 검증 셸의 `PATH`가 채워지지 않으므로, 사전 확인 셸에서도 `bun`이 `PATH`에 있어야 한다:
     ```bash
     export PATH="/home/<user>/.bun/bin:$PATH"
     command -v bun && bun --version
     ```
   - `ext-host/host.ts`는 `omt` 실행 바이너리 상대/소스 경로(또는 `OMP_RS_EXT_HOST`)에서 로드된다.
3. **배포된 `omt` 바이너리 대상 RPC 적합성 골든 스위트 통과**
   - `CARGO_BIN_EXE_omt` 기본 빌드 대신 실제로 워커가 실행할 배포 바이너리 경로를 `OMT_CONFORMANCE_BIN`으로 지정해 스위트를 실행한다(또는 릴리즈 체크섬과 동일한 소스 빌드 바이너리임을 1단계에서 확인한다).
   - `--nocapture` 출력에 `skipping live extension_request`가 찍히지 않고 라이브 확장 검사까지 모두 실행·통과해야 한다:
     ```bash
     OMT_CONFORMANCE_BIN=/home/<user>/.local/bin/omt \
       cargo test -p omp-cli --test rpc_conformance --locked -- --nocapture
     ```
     (EL9/glibc 2.34 장비에서 소스 빌드가 필요할 때는 `--no-default-features` 추가)

---

## 2. 전환 설정 (단일 워커 카나리)

허브(`config.omoExecutable`)와 나머지 워커는 `mhc`로 두고, 대상 워커 1대의 `machines[].worker.omo`만 변경한다.

### 2.1 `omo-nyang` 설정 변경

허브의 게이트웨이 설정 파일(`config.json`, `OMOCAT_CONFIG`)에서 대상 장비 블록만 수정한다(`src/config.ts` 스키마 기준):

```json
{
  "omoExecutable": "/home/jc01rho/.local/bin/mhc",
  "machines": [
    {
      "name": "<canary-worker>",
      "host": "<canary-ssh-host>",
      "worker": {
        "userHome": "/home/<user>",
        "home": "/home/<user>/.local/share/omomeow",
        "bun": "/home/<user>/.bun/bin/bun",
        "omo": "/home/<user>/.local/bin/omt",
        "workDirectory": "/home/<user>/git"
      }
    }
  ]
}
```

### 2.2 `~/.omo` 프로필 및 하네스 연동 확인 (`#26` 반영)

- `omt`는 `OMP_AGENT_DIR`이 비어 있을 때 `~/.omp/agent`를 우선 보고, 없으면 `~/.omo/agent`로 폴백한다.
- `~/.omo/omo.jsonc`(또는 `omo.json`)가 존재하면 `OMO_PROFILE` / `OMP_PROFILE` / `--profile` 기준 프로필의 `categories`, `task.maxConcurrency` 등을 읽어 반영한다.
- `gateway.ts`가 넘기는 기동 인자(`--mode rpc --session <path> --append-system-prompt ... --skill <dir> --model ... --thinking ... --extension ... --no-model-fallback --no-recommended-models`)가 모두 파싱되는지 확인한다.

### 2.3 카나리 트래픽 격리

- 초기 시범 운영에서는 테스트 저장소 또는 특정 작업에만 `<canary-worker>에서` 핀을 주거나 `lanes.projectMachines`로 대상 프로젝트를 카나리 장비에 묶어 영향 범위를 제한한다.
- 설정 변경 후 게이트웨이를 재시작하거나 설정을 반영하고, 카나리 장비에서 새 태스크 1건을 시작한다.

---

## 3. 확인할 신호

카나리 태스크 실행 중 아래 신호를 순서대로 확인한다.

### 3.1 프로세스 기동 및 핸드셰이크
- `agentd` 세션 로그에서 프로세스가 즉시 종료되지 않고 `{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],...}` 프레임을 출력해야 한다.
- `get_protocol_info` 응답:
  - `protocolVersion == 2`
  - `engine == "oh-my-tau"`
  - `serverVersion`이 `omt --version` 출력의 `omt/` 뒤 버전 문자열과 일치(예: `omt/0.12.0` → `0.12.0`)
  - `commands`에 `set_client_info`, `interrupt`, `reload`, `resume_session`, `extension_request`, `get_state`, `prompt`, `abort`, `get_last_assistant_text`, `get_session_stats`, `get_subagents`, `cancel_subagent`, `steer_subagent` 포함

### 3.2 세션 파일 바인딩 (`#29` 반영)
- `get_state` 응답의 `sessionFile`이 `gateway.ts`가 `--session`으로 넘긴 `<worker.home>/tasks/<taskId>.jsonl` 경로와 정확히 일치해야 한다(`~/.omp-rs` 기본 경로로 새 파일이 열리면 실패).

### 3.3 턴 정착 (`agent_settled`)
- 프롬프트 전송(`prompt` + `streamingBehavior: "followUp"`) 후 이벤트 순서:
  1. `agent_start`
  2. `message_start` / `message_update` / `message_end`
  3. `agent_end`
  4. `prompt_result` (`status: "completed"`, `sessionSettled: true`)
  5. `session_settled`
  6. `agent_settled` (`runtime.rs` stdout 방출)
- 게이트웨이 로그/스레드에서 턴 종료 후 스레드가 멈추지 않고 응답이 게시되며, `get_state`가 `isStreaming: false`, `isSettled: true`로 돌아오는지 확인한다.

### 3.4 보류·재개 (`lane-hold.ts`)
- "기다려/보류" 흐름 시뮬레이션(`task-control pause` / `lane-hold.ts`):
  1. `set_client_info({width: 80, capabilities: ["interactive_user"]})` → `success: true`
  2. `interrupt({intent: "stop"})` → `session_paused` 이벤트 방출, `get_state.paused == true`
  3. `reload` → `{"cancelled": false}`, 보류 상태(`paused: true`) 유지
  4. `resume_session` → `session_resumed` 이벤트 방출, `get_state.paused == false`

### 3.5 오류 응답 형식
- 실패 응답 프레임에 `code`와 `errorCode`가 **동시에** 포함되어야 한다(`MoveHost`/`brain.ts` 파서 호환).
- 게이트웨이 로그(`~/.local/share/omomeow/...`)에 `Unknown command`, `prompt_refused`, `session_exit`, `relay_link` 오류가 반복되지 않아야 한다.

---

## 4. 되돌리는 법 (롤백)

이상 징후(기동 실패, `agent_settled` 미수신, `get_state` 파싱 실패, `Unknown command` 폭증)가 보이면 즉시 다음 순서로 되돌린다.

1. **설정 원복**
   - 허브의 `config.json`에서 카나리 장비의 `worker.omo`를 기존 `mhc` 경로로 복구한다:
     ```json
     "omo": "/home/<user>/.local/bin/mhc"
     ```
2. **게이트웨이 재기동 및 카나리 세션 중지**
   - 게이트웨이를 재시작해 새 워커 스폰이 `mhc`를 쓰도록 전환한다.
   - 이미 `omt`로 떠 있는 카나리 레인 세션만 `worker-control stop ROUTE` 또는 `agentd`의 `session.stop`으로 종료한다.
   - 기존 JSONL v3 세션 파일은 유지되므로, 다음 스레드 입력 시 `mhc`가 같은 세션 파일에서 이어서 재개한다.
3. **사후 점검**
   - `bun src/agent-status.ts`로 카나리 장비 레인 상태를 확인하고, 롤백 후 새 턴이 정상 정착하는지 1회 확인한다.

---

## 5. 알려진 공백 (`origin/main` `b3be985` 기준)

아래 항목은 현재 `main` 기준으로 미구현이거나 오픈 PR에서 작업 중이므로, 해당 PR 머지 전까지는 카나리 범위에서 제외하거나 게이트웨이 측 주의가 필요하다. (PR `#27` G8의 `SubagentRegistry`, `task_send`/`task_output`/`task_cancel`, 서브에이전트 RPC 4종은 `main`에 반영 완료됨.)

### 5.1 `get_state` 및 제어 응답 스키마 차이
- **`pendingMessageCount` 부재**: `main`의 `get_state`는 `queuedMessageCount`만 반환한다. `omo-nyang/src/relay/agentd-sessions.ts`의 `quiesce()`는 `pendingMessageCount: z.number()`(기본값 없음)를 필수로 파싱하므로, 세션 이동 대기 경로에서 파싱 오류가 날 수 있다(`brain.ts`/`lane-idle.ts`는 optional/default 처리됨).
- **`isBashRunning` / `usageTotals` 부재**: `main`의 `get_state`에 아직 없어 `worker-rpc.ts`의 `stopSession`은 백그라운드 셸 상태를 `not_supported`로 간주하고, `sessionUsage`는 세션 자체 `usageTotals`를 읽지 못한다.
- **`runPaused` 고정값**: `pause.rs`의 `view()`는 현재 `runPaused: false`를 반환한다.
- **`interrupt` 반환값**: `rpc.rs`의 `interrupt`는 중단 여부와 무관하게 `{"interrupted": false}`를 반환한다(`lane-hold.ts`/`worker-rpc.ts`는 반환 본문 대신 후속 `get_state`로 판정).

### 5.2 오픈 PR 반영 대기 항목
- **PR `#22` (G6b)**: 백그라운드 셸 제어(`bash_output`, `kill_bash`, `monitor` 비폴링 도구).
- **PR `#23` (G10)**: `omp-memory` 및 `memory`, `memory_apply_patch` 도구.
- **PR `#30` (G11)**: `--multi-session` / `--session-runtime in-process` 다중 세션 격리 라우팅, `release_session`, `move_session`(6 op), `session_move_export`/`import` 확장 이벤트. (현재 `main`에는 CLI 플래그만 수용되고 실제 다중 세션·이동 핸들러는 없음.)
- **PR `#31` (G1fix)**: PR `#18` 리뷰 반영(`reload` 실패 시 `CmdError` 반환, `stop` 보류 중 큐/프롬프트 차단, overflow 프레임 `code`/`errorCode` 포함, `--skill` 파일 직접 지정 등).

### 5.3 기타 미구현 기능
- **`workpool` 에이전트 도구** (PR-G9 대기 — 현재는 `eval` 커널 내부 헬퍼로만 존재).
- **`memory_report` RPC** (`SENPI_MEMORY_REPORT=1` / `worker-rpc.ts` 메모리 풋프린트 수집).
- **`gateway_resume` / `gateway_answer`** 등 `agentd`/릴레이 전용 확장 프레임.
