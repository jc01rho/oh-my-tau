# 슬랙 `1795326973.474349` "확인 필요" 5개 항목 및 미머지 단계 코드 대조 확정 보고서

기준 시각: 2026-11-22
기준 커밋:
- `oh-my-tau`: `e7a5378` (`main`, workspace version `0.12.0`, Phase 2.5 머지 직후)
- `omo-nyang` (게이트웨이): `mac-mini:/home/jc01rho/Work/omo-nyang` 현재 운영 본
- `mhc` (현행 엔진): `v2026.11.20-omomeow1` (`f0f3571c5`)

참조 스레드 메시지(`https://jc01rho.slack.com/archives/C0C8H66FH5W/p1795326973474349?thread_ts=1795324158.334689&cid=C0C8H66FH5W`, 2026-11-22 14:56 답변)에서 직접 확인하지 못해 **"확인 필요"**로 남겨 두었던 5개 항목과 미머지 계획 단계(3·4·5단계)를 세 저장소의 실제 소스 코드와 대조하여 아래와 같이 확정한다.

---

## 1. 실행 플래그 `--skill <경로>` vs `--skills` (확인 필요 항목 1)

### 1.1 코드 실측 근거
- **게이트웨이 호출부 (`omo-nyang/src/gateway.ts:1165-1168`)**:
  ```ts
  const workerArgv = (paths, role) => [
    paths.omo, "--mode", "rpc", "--session", paths.session,
    "--append-system-prompt", `${paths.root}/OMOCAT.md`,
    "--skill", `${paths.root}/skills`,
    ...roleFlags(config.runtime, role, paths.root),
  ```
  게이트웨이는 워커·코디네이터 세션을 띄울 때 항상 `--skill <스킬 루트 디렉터리 절대경로>`(예: `/Users/jc01rho/.local/share/omomeow/skills`)를 전달한다.
- **`oh-my-tau` 플래그 파서 (`crates/omp-cli/src/cli.rs:97-136, 309-311, 433, 512-516`)**:
  - `STRING_FLAGS`(`cli.rs:129`)에는 `"--skills"`만 있고 `"--skill"`은 없다.
  - `"--skills"` 처리(`cli.rs:309-310`)는 `a.skills = value.split(',').map(|s| s.trim().to_string()).collect();`로 **쉼표로 구분된 스킬 이름/글롭 필터 목록**(예: `git-*`, `cli.rs:741-747` 단위 테스트 참조)을 파싱한다.
  - 따라서 `omt`에 `--skill /path/to/skills`를 넘기면 `cli.rs:433` `unrecognized`로 빠져 기동 즉시 `Unrecognized flag: --skill` 오류로 종료된다.

### 1.2 확정 결론 및 설계 반영
- `--skill <경로>`는 `--skills <CSV 필터>`의 단순 이름 별칭이 **아니다**.
  - `--skills <CSV>`는 이미 탐색된 스킬 중 활성화할 스킬 이름 패턴 필터다.
  - `--skill <경로>`는 `SKILL.md` 또는 하위 `<name>/SKILL.md`를 탐색할 **추가 파일시스템 경로(디렉터리 또는 단일 파일)**를 등록하는 플래그이며 여러 번 지정될 수 있다.
- **설계**:
  1. `STRING_FLAGS`(`crates/omp-cli/src/cli.rs:129`)에 `"--skill"`을 추가하고 `Args`(`cli.rs:62`)에 `pub skill_paths: Vec<PathBuf>`를 신설한다.
  2. `set_string`(`cli.rs:309`)에서 `"--skill" => a.skill_paths.push(PathBuf::from(value))`로 누적한다(빈 문자열은 무시하지 않고 `invalid`에 기록, 중복 경로는 정규화 후 `IndexSet`으로 중복 제거).
  3. 시스템 프롬프트 스킬 로더(`crates/omp-cli/src/sysprompt.rs` 및 `features/skills.rs`)에서 기본 `~/.omp/agent/skills`·`<cwd>/.omp/skills` 외에 `args.skill_paths`를 순서대로 스캔해 `<Available_Skills>`에 병합한다. `--no-skills`(`cli.rs:158`)가 켜진 경우에는 `--skill`로 전달된 경로도 모두 비활성화한다.

---

## 2. 에이전트 도구 `task`의 동작 방식과 `task_send`·`task_output`·`task_cancel` (확인 필요 항목 2)

### 2.1 코드 실측 근거
- **`oh-my-tau`의 `task` 도구 (`crates/omp-cli/src/tools/task.rs:1-125, 480-495`, `crates/omp-task/src/params.rs`, `runner.rs`)**:
  - 입력 스키마(`task.rs:94-116`)가 `{ context: string, tasks: [{ task: string, solutionSpace: string, name?, agent?, model?, outputSchema?, schemaMode?, tools? }] }`로 고정되어 있으며, 최상위 `model`은 `"model": {"not": {}}`로 명시 거부한다.
  - 실행 방식(`task.rs:483-485`)은 `TaskRunner::spawn_all(specs).await`로 자식 `omt -p <prompt>` 프로세스 배치를 띄우고 **배치 전체가 종료될 때까지 블로킹 대기**한 뒤 `<task-result>`(`agent://<id>`) 봉투를 한 번에 반환한다.
  - 실행 중인 자식 세션을 붙잡아 두는 프로세스 내 레지스트리가 없다. 그 결과 `crates/omp-cli/src/rpc.rs:863-879`의 `"get_subagents"`는 항상 `{"subagents": []}`를 반환하고, `"cancel_subagent"`는 `{"cancelled": false}`, `"steer_subagent"`는 항상 `"Subagent not running"` 오류를 내는 스텁 상태다.
- **OmOMeow(`mhc`/`senpi`)의 `task` 계약**:
  - 단일 호출(`prompt`)과 배치(`tasks[]`)를 모두 허용하고, `category`(예: `quick`, `deep`, `ultrabrain`, `architect` 등 `omo.json` 카테고리 라우팅) 또는 `subagent_type`(`explore`, `librarian` 등)으로 대상을 지정한다.
  - `run_in_background: true`일 때 자식 세션 ID(`st_...`)를 즉시 반환하고 백그라운드에서 실행하며, 완료 시 부모 세션에 알림을 주입한다.
  - 실행 중이거나 완료된 자식 세션을 `task_send({ to, message })`(실행 중 조향 또는 완료 세션 재개), `task_output({ task_id | name, mode: "status" | "tail" | "full", tail_lines? })`(블로킹 없는 스냅샷/트랜스크립트 조회), `task_cancel({ task_id | name, reason? })`(프로세스 트리 종료 및 리소스 해제)로 제어한다.
  - 또한 자식이 비정상 사망(died/lost)한 상태로 남아 있으면 `task_cancel`로 정리하기 전까지 부모 작업의 완료 주장(`done`)을 차단한다.

### 2.2 확정 결론 및 설계 반영
- `oh-my-tau`의 현재 `task` 도구는 OmOMeow 지침이 기대하는 `task`와 **입력 스키마 및 실행 생명주기(동기 배치 vs 비동기 상주 세션)가 모두 다르다**.
- **설계**:
  1. `crates/omp-task/src/registry.rs`에 세션 단위 `SubagentRegistry`를 신설하여 각 자식을 `omt --mode rpc` 인프로세스/서브프로세스 채널로 관리하고 고유 ID(`st_<hex>`)와 `name` 핸들을 부여한다.
  2. `crates/omp-cli/src/tools/task.rs`의 입력 정규화 계층에서 기존 omp 배치 스키마(`{context, tasks:[{task, solutionSpace}]}`)와 OmOMeow 스키마(`{category|subagent_type, prompt|tasks, run_in_background, load_skills, task_summary, isolated, merge, apply, model}`)를 모두 수용(Dual-Schema Normalization)한다.
  3. `task_send`, `task_output`, `task_cancel` 3개 도구를 `crates/omp-cli/src/tools/`에 추가 등록하고, `crates/omp-cli/src/rpc.rs:863-879`의 `get_subagents`·`cancel_subagent`·`steer_subagent`를 같은 `SubagentRegistry`에 직결한다.

---

## 3. 질문·목표 도구 `ask`·`goal` vs `ask_user_question`·`create_goal`·`get_goal`·`update_goal` (확인 필요 항목 3)

### 3.1 코드 실측 근거
- **`oh-my-tau`의 `ask` (`crates/omp-cli/src/tools/ask.rs:10-48, 740-780`)**:
  - 도구 이름이 `"ask"`이며 기본 입력은 `{ html: "<form>...</form>" }`이고 폴백으로 `{ questions: [{ id, question, options: [{ label, description? }], multi?, recommended? }] }`를 받는다.
  - RPC 모드(`rpc.rs:1390-1397` `RpcAskUi`)에서는 `{"type": "extension_ui_request", "id": "...", "method": "ask", "questions": [...]}` 프레임을 내보내고 `extension_ui_response`를 기다린다.
- **OmOMeow의 `ask_user_question`**:
  - 도구 이름이 `"ask_user_question"`이며 입력 스키마는 `{ questions: [{ header, question, multiSelect, options: [{ label, description? }] }], waitForAnswer: boolean, required?: boolean }`이다.
- **`oh-my-tau`의 `goal` (`crates/omp-cli/src/tools/goal.rs:25-60`, `tools.rs:501-516`)**:
  - 단일 숨김 도구 `"goal"` 하나만 있으며 입력은 `{ op: "create" | "get" | "complete" | "pause" | "resume" | "drop", objective?, token_budget?, report? }`이다. 게다가 `goal_mode_active`일 때만 도구 목록에 노출된다(`tools.rs:501`).
- **OmOMeow의 `create_goal` / `get_goal` / `update_goal`**:
  - 상시 노출되는 3개의 개별 도구로 분리되어 있다:
    - `create_goal({ objective: string })`
    - `get_goal({})`
    - `update_goal({ status: "complete" | "blocked", reason?: string })`

### 3.2 확정 결론 및 설계 반영
- 도구 이름과 JSON 파라미터 필드명(`multi` vs `multiSelect`, 단일 `goal(op)` vs 3개 분리 도구, `goal_mode_active` 게이팅)이 **불일치**하므로 호환 래퍼가 필수다.
- **설계**:
  1. `ask_user_question` 도구를 `ask.rs` 위에 별칭 도구로 등록한다. 입력의 `questions[].multiSelect`를 `multi`로, `questions[].header`를 질문 `id`/칩 레이블로 매핑하여 기존 `AskUi::ask` 파이프라인을 그대로 타게 하고, `waitForAnswer: false`일 때는 비동기 질문 티켓을 발급한 뒤 즉시 반환한다.
  2. `create_goal`, `get_goal`, `update_goal` 3개 도구를 `goal.rs`의 `GoalRuntime` 위에 상시 등록한다(`create_goal` → `op:"create"`, `get_goal` → `op:"get"`, `update_goal(status:"complete")` → 열린 `todo` 항목이 0개인지 감사 후 `op:"complete"`, `update_goal(status:"blocked", reason)` → `op:"pause"` + 차단 사유 기록).

---

## 4. 작업 종료 신호 `agent_settled` vs `agent_end` vs `session_settled` (확인 필요 항목 4)

### 4.1 코드 실측 근거
- **게이트웨이(`omo-nyang`)가 기다리는 신호**:
  - `omo-nyang/src/relay/runtime.ts:59`: `if (type === "agent_settled")`에서 `settledAt`을 기록한다.
  - `omo-nyang/src/relay/slice-run.ts:71`: `if (event.type === "agent_settled") resolve("settled");`
  - `omo-nyang/src/relay/fleet.ts:258`: `if (event.type === "agent_settled") { clearTimeout(settled); settled = setTimeout(resolve, deps.settleGraceMs ?? 5_000); }`
  - `omo-nyang/src/relay/agentd-sessions.ts:186`: `["agent_end", "agent_settled", "queue_update", "compaction_end", "session_exit"]`를 감시한다.
  - 즉 게이트웨이는 RPC 표준 출력(stdout) 이벤트 스트림에서 **`{"type": "agent_settled"}` 프레임이 나와야** 한 턴과 후속 큐가 완전히 정착(settled)된 것으로 판정한다.
- **`oh-my-tau`의 현재 동작**:
  - `crates/omp-cli/src/ext.rs:1585-1603`: `AgentEvent::AgentEnd`가 발생할 때 RPC stdout에는 `{"type": "agent_end", ...}`만 내보내고, `{"type": "agent_settled"}`는 오직 Bun 확장 호스트에 `host.has_handlers("agent_settled")`가 등록되어 있을 때만 **확장 호스트 내부 파이프로만**(`host.emit_nowait(&json!({"type":"agent_settled"}))`) 보낸다. RPC stdout으로는 절대 나가지 않는다.
  - 게다가 `ext.rs:1593`의 `AgentEvent::AgentEnd` 시점은 단일 에이전트 루프가 끝난 시점일 뿐, 대기열(steering/follow-up)이나 압축 후 자동 재개가 남아 있는지를 검사하지 않는다.
  - 반면 `crates/omp-cli/src/runtime.rs:1630-1643`의 `report_prompts_now`는 실제로 모든 프롬프트와 큐가 비워졌을 때(`let settle = p.active && drained && p.open.is_empty();`) RPC stdout으로 `self.send(json!({"type": "session_settled"}));`를 보낸다.

### 4.2 확정 결론 및 설계 반영
- `oh-my-tau`의 `ext.rs:1598`에 있는 `agent_settled`는 확장 호스트 내부 알림일 뿐 RPC stdout으로 나가지 않으며, 게이트웨이가 기다리는 진짜 정착 조건(`p.active && drained && p.open.is_empty() && !has_running_background_turn`)은 `runtime.rs:1641`의 `session_settled` 지점이다.
- **설계**:
  1. `crates/omp-cli/src/runtime.rs:1641`에서 `settle == true`일 때 기존 `{"type": "session_settled"}`(하위 호환 유지) 직후에 `{"type": "agent_settled"}` 프레임을 RPC stdout으로 함께 방출한다.
  2. `ext.rs:1593-1598`의 확장 이벤트 `agent_settled`도 매 `AgentEnd`가 아니라 `runtime.rs`의 정착 시점에 호출되도록 일원화하여 `agent_end` → `session_settled` → `agent_settled` 순서를 전역 불변식으로 고정한다.

---

## 5. 세션 기록 파일 형식 `omp JSONL v3` vs `senpi` 형식과 게이트웨이 기록 읽기 (확인 필요 항목 5)

### 5.1 코드 실측 근거
- **게이트웨이(`omo-nyang`)가 세션 `.jsonl` 파일을 직접 읽는 위치**:
  1. `omo-nyang/src/gateway.ts:1192`: 워커 기동 시 `--session <stateDirectory>/tasks/<taskId>.jsonl` 경로를 인자로 넘긴다.
  2. `omo-nyang/src/session-move-extension.ts:116-126`: 세션 이동 확장이 `ctx.sessionManager.getSessionFile()` 경로를 기준으로 `<sessionFile>.omomeow-move.json` 사이드카를 읽고 쓴다(세션 JSONL 본문 파싱은 하지 않음).
  3. `omo-nyang/src/session-patterns.ts:47-76`: 주간 세션 습관 분석기가 `.jsonl` 파일의 각 줄을 순회하며 `if (!line.includes("\"message\"")) continue;`로 필터링한 뒤 `JSON.parse(line)`을 수행하고 다음 필드를 읽는다:
     - `entry.timestamp` (ISO-8601 문자열)
     - `entry.message.role === "assistant"`일 때 `entry.message.content[]` 배열 내 `{ type: "toolCall", id: string, name: string, arguments: object }`
     - `entry.message.role === "toolResult"`일 때 `entry.message.content` (문자열 또는 `{ type: "text", text: string }[]`), `entry.message.toolCallId`, `entry.message.isError`
- **`oh-my-tau`의 `omp JSONL v3` 형식 (`crates/omp-session/src/entries.rs:1-120`, `lib.rs:25, 225`)**:
  - 1행: `{"type":"title","v":1,...}` (256바이트 고정 폭 타이틀 슬롯, `"message"` 문자열을 포함하지 않으므로 `session-patterns.ts:48`이 자동으로 건너뜀)
  - 2행: `{"type":"session","version":3,"id":"...","timestamp":"...","cwd":"..."}`
  - 메시지 행(`Entry::Message`): `{"id":"...","parentId":"...","timestamp":"2026-...","type":"message","message":{"role":"assistant"|"user"|"toolResult","content":[...],...}}`
  - 즉 `omp JSONL v3`의 `message` 엔트리는 이미 최상위 `timestamp`와 `message.{role, content, toolCallId, isError}` 구조를 그대로 가지고 있어 `omo-nyang/src/session-patterns.ts`와 **100% 호환**된다!
  - **유일한 차이점 2가지**:
    1. `--session <경로>` CLI 플래그 처리(`crates/omp-cli/src/cli.rs:138, 377-385`): `omo-nyang/src/gateway.ts:1165`는 새 작업에서도 아직 파일이 존재하지 않는 `--session /path/to/tasks/<taskId>.jsonl`을 넘긴다. `tau`의 `OPTIONAL_FLAGS`에서 `--session`은 현재 `a.resume = Some(Resume::Id(val))`로 매핑되어 **기존 세션 파일이 없으면 resume 실패**가 날 수 있다(`main.rs:510-537`). 파일이 없을 때는 해당 경로에 새 세션 JSONL 파일을 생성(`Persistence::bind_or_create(path)`)해야 한다.
    2. `mhc`(`senpi`)와 `omt`(`oh-my-tau`) 사이의 이기종 장비 간 `move_session`: `mhc`는 1행 고정 폭 `title` 슬롯(256B)이 없는 `senpi` JSONL 헤더를 쓰고 `tau`는 `TitleSlot` + `version: 3` 헤더를 쓴다. `tau`의 `SessionIndex::load`(`crates/omp-session/src/index.rs:118`)는 `title` 슬롯이 없는 JSONL도 읽을 수 있으나, `mhc`가 `tau`의 `title` 슬롯이나 `omp` 전용 엔트리(`model_usage`, `credential_pin`, `ttsr_injection`)를 읽을 때는 어댑터가 필요하다.

### 5.2 확정 결론 및 설계 반영
- 게이트웨이 자체의 세션 파일 리더(`session-patterns.ts`, `session-move-extension.ts`)는 `tau`의 `omp JSONL v3`와 **즉시 호환**된다.
- 단, ① `--session /abs/path.jsonl`이 존재하지 않는 파일 경로를 가리킬 때 그 경로로 새 세션을 생성·바인딩하는 로직(`crates/omp-cli/src/main.rs:510-537`, `persist.rs`)과 ② `mhc ↔ omt` 혼합 플릿에서 `move_session`을 수행할 때 `TitleSlot` 및 `omp` 전용 커스텀 엔트리를 정규화하는 양방향 어댑터(`crates/omp-session/src/senpi_compat.rs`)를 구현해야 한다.

---

## 6. 미머지 `oh-my-tau` 계획 단계(Phase 3·4·5)와의 의존성 (슬랙 `1795326973.474349` 5번 항목)

`docs/rust-migration-plan.md`의 미머지 단계(Phase 3·4·5)와 OmOMeow 게이트웨이 전환 요구사항을 대조한 결과는 다음과 같다:

| `rust-migration-plan.md` 단계 | 세부 항목 | OmOMeow 게이트웨이 전환과의 관계 | 통합 설계 방침 |
|---|---|---|---|
| **Phase 3** (도구·메모리·컨텍스트·Handoff) | 3.1 `/handoff` 배선 (`omp-compact/src/handoff.rs`)<br>3.3 `crates/omp-memory` 신설<br>3.4 컨텍스트 파일 탐색 | **직접 중첩 (선행 필수)**: OmOMeow는 `memory`·`memory_apply_patch` 도구와 `<memory>` 시스템 프롬프트 투영을 상시 사용한다. | Phase 3.3의 `crates/omp-memory`에 SQLite/FTS 백엔드와 함께 **Git 버전 관리 마크다운(`YAML frontmatter`) 블록 백엔드(`omo` 메모리 저장소 호환)**를 통합 설계한다. |
| **Phase 4** (프로바이더·추론·MCP/RPC 정렬) | 4.3 `omp-mcp` 다중 세션 데몬 공유<br>4.4 `rpc.rs` `omp-rpc` v2 청킹 정렬 | **직접 중첩**: `--multi-session --session-runtime in-process`에서 MCP 서버 공유(4.3)와 `rpc.rs` 응답/에러 프레임 정렬(4.4)이 같은 코드(`crates/omp-cli/src/rpc.rs`)를 수정한다. | Phase 4.4와 게이트웨이 필수 RPC 8종(`interrupt`~`move_session`) 및 `errorCode` 표준화를 단일 RPC 계층 개편(Wave G1/G4)으로 합류시킨다. |
| **Phase 5** (VCS·워크트리·격리 PAL) | 5.1 `gix` 도입<br>5.2 `crates/vcs` + `crates/iso` 워크트리 격리<br>5.3 `.omp-isolation-owner` 고아 워크트리 GC | **직접 중첩**: OmOMeow의 `task(isolated: true, merge: "patch"|"branch", apply: bool)` 하위 작업 격리가 Phase 5.2의 `crates/vcs`·`crates/iso` 위에 올라간다. | 비동기 `task`(`st_...`)의 `isolated` 옵션을 Phase 5.2의 `crates/iso` CoW 워크트리 백엔드에 직접 연결한다. |
