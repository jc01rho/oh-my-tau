# 에이전트 도구: 백그라운드 셸 세션 관리와 관찰(bash_output·kill_bash·monitor) 설계

## 1. 현재 상태 (근거)

- 도구 등록부는 `crates/omp-cli/src/tools.rs:321`의 `let tools = vec![...]` 하나이고, 이름 목록은 `tools.rs:677`의 `tool_names_and_schemas_match_omp` 테스트가 `read, bash, edit, glob, grep, write, find, task, wait, eval, ast_grep, ast_edit, ask, todo, web_search, lsp, checkpoint, rewind`로 고정한다. `bash_output`/`kill_bash`/`monitor`는 없다(`crates/omp-cli/src` 전체 grep에서 `bash_id`·`kill_bash`·`bash_output` 심볼 0건).
- 백그라운드 실행은 `crates/omp-cli/src/tools/bash.rs:1040` `start_background`. `registry.insert(job)`(`jobs.rs:131`)가 `bg_N` id를 만들고 `tokio::spawn` 안에서 `run_with_dyn(..., tap: Some(output), ...)`를 돌린 뒤 `bash.rs:1113`에서 `registry.update(&task_job_id, ...)`로 `status`/`result`/`error`/`ended`를 채운다.
- 세션 레코드 `jobs::Job`(`jobs.rs:23`)의 필드는 `command, started, status, output, result, error, ended`뿐이다. **pid도, 취소 핸들도, id 자기 참조도 없다.** `kill_requested`는 `Service`에만 있다(`jobs.rs:96`).
- 그래서 `start_background`의 `_signal: &AbortSignal`(`bash.rs:1050`)은 버려지고 내부에서 새 `AbortController`를 만든다(`bash.rs:1077`). 잡을 밖에서 중단할 경로가 없다.
- `jobs::background_notice`(`jobs.rs:291`)는 "output auto-delivers on finish; don't poll"이라고 약속하지만, 잡 완료 지점에서 `follow_up`을 부르는 코드는 없다. `follow_up(` 호출처는 `tui.rs:478`, `runtime.rs:1691`/`1858`, `rpc.rs:660`, `slash_extra.rs:488`뿐이다. 즉 자동 전달은 문구만 있고 구현이 없다. 실제 관찰 수단은 `wait` 도구(`wait.rs:104`)와 `read proc://`(`jobs.rs:308`) 둘이다.
- `wait`는 `POLL_INTERVAL = 100ms`(`wait.rs:28`)로 `tokio::time::sleep`을 도는 폴링 루프다. 규칙 위반이며 이 조각에서 함께 없앨 대상이다.
- 출력 보관은 `Job.output: Arc<Mutex<String>>` 하나이고 `RETAIN_BYTES = 8 * DEFAULT_MAX_BYTES`(`bash.rs:36`)를 넘으면 앞을 버린다(`shell_host.rs`의 tap drain, `bash.rs:427` 원격 경로). **소비 커서가 없어 증분 읽기가 불가능하다.**
- 프로세스 트리 종료 기구는 이미 있다. `shell_host::SpawnBook`(`shell_host.rs:526`)이 `signal_groups`(`shell_host.rs:544`)로 `killpg`를 쏘고, `execute_shell`(`shell_host.rs:848`)이 취소·타임아웃에서 `SIGKILL`을 그룹에 보낸다(`shell_host.rs:984`, `:991`). 그룹 정책은 `NewProcessGroup`(`shell_host.rs:879`).
- 감시 관련 기구는 `Notify`(`bash.rs:180`)가 유일하다. 청크마다 정규식 매칭을 하되 보고는 정산 시 한 번(`append_notify_report`, `bash.rs:1023`)이다. 스트리밍 깨우기는 없다.
- `proc://<id>/kill`, `/mode`는 "writable only"로 거절되고(`jobs.rs:323`), 쓰기 경로 자체가 없다(`edit.rs`에 `proc` 처리 없음). 서비스 종료는 `kill_requested` + `SUPERVISOR_TICK 100ms`(`bash.rs:812`) + `child.start_kill()`(`bash.rs:940`)이다.
- `pty`는 스키마에만 있다(`tools.rs:335`). `args.get("pty")`를 읽는 코드가 없으므로 PTY 세션·화면 스냅샷은 미구현이다.
- `notify` 크레이트 8.2.0은 워크스페이스에 있으나(`Cargo.toml:125`) `omp-cli` 의존성에는 없다(`crates/omp-cli/Cargo.toml`).

## 2. 게이트웨이가 요구하는 계약

mhc 기준 계약(참조 구현: `maho-ext-terminal`).

- `bash` 도구가 `run_in_background`를 받고 `details.bash_id`를 즉시 돌려준다. id 규칙은 셸 접두사 + 카운터(`manager.rs:228`, `:1082`의 `bash_1`, `bash_Ab12_1`).
- `bash_output { bash_id, filter?, view?: "log"|"screen" }` (`tools/bash_output.rs`의 `bash_output_schema`). 응답은 `status: running` 또는 `status: exited exit_code: N` 머리에 **직전 읽기 이후의 증분 텍스트**만 붙는다. 앞이 잘렸으면 `[N earlier chars dropped]`. `filter`는 정규식 라인 필터이고 잘못된 패턴은 무시(비필터)한다.
- `kill_bash { bash_id?, all? }`. `bash_id`도 `all`도 없으면 오류, `all`이면 세션 수를 세어 `Killed N session(s).`, 개별이면 `Killed <id>.` 또는 `No terminal session found with id: <id>`.
- `monitor { action?: create|rearm, type?: command|path|rpc, description(필수), command|path(XOR), event?: create|modify, filter?, timeout_ms?(기본 300000, 최대 3600000), persistent?, bash_id?, wakes_per_hour?, human_input? }` (`tools/monitor_schema.rs`). 스키마는 평면이어야 한다(루트 `anyOf` 금지).
- 감시 이벤트는 `MonitorEvent::Line`/`Summary`, 종료 사유는 `Exit|Timeout|Killed|Disposed`(`monitor_registry.rs`). 깨우기 본문은 `Monitor event(<description>): <line>` 형태의 `<system-reminder>` 묶음(`monitor_notify_format.rs`).
- 감시가 예산 초과로 스스로 멈추면 `bash_output` 결과 머리에 `monitor muted ... rearm` 안내가 붙고 `details.monitorMuted`가 실린다(`tools/bash_output.rs`의 `with_monitor_state`).
- 종료 신호는 게이트웨이가 `agent_settled`를 기다린다. tau는 `agent_end`만 항상 보내고 `agent_settled`는 확장 핸들러가 등록된 경우에만 `emit_nowait`로 보낸다(`crates/omp-cli/src/ext.rs:1593`, `crates/omp-ext/src/host.rs:575`/`:604`).

## 3. 설계

### 3.1 지속 세션 id

- 정본 id는 기존 `bg_N`(`jobs.rs:131`)을 유지한다. 세션 파일·`proc://`·`/jobs`가 이미 이 id를 쓴다. 계약 호환을 위해 도구 결과 `details`에 `bash_id: "bg_N"`을 추가하고, 입력 파서는 `bg_7`, `7`, `bash_7`을 모두 `bg_7`로 정규화한다(`resolve_session_id(&str) -> Option<String>`).
- `Job`에 `bash_id: String`을 넣어 자기 id를 갖게 한다. `JobRegistry::insert`가 id를 만든 뒤 역주입한다(현재는 `insert`가 id만 반환, `jobs.rs:131`).
- `JobStatus`에 `Killed`를 추가한다(`jobs.rs:39`). `as_str`는 `"killed"`.

### 3.2 증분 출력 읽기

- `Job`에 두 필드를 더한다: `dropped: Arc<AtomicUsize>`(앞에서 버린 절대 바이트), `cursor: Arc<AtomicUsize>`(절대 소비 오프셋). tap을 `String`에서 새 구조체로 바꾸지 않는다. 드레인 지점(`bash.rs:427`의 원격 tap, `shell_host.rs`의 tap 드레인)에서 `dropped.fetch_add(cut, Relaxed)`만 추가한다.
- `bash_output`은 `cursor`와 `dropped`를 읽어 `start = max(cursor, dropped)`, `text = output[(start - dropped)..]`, `cursor = dropped + output.len()`로 계산한다. mhc의 `read_delta`(`runtime_session.rs:144`)와 같은 산식이다. `dropped > cursor`면 `[N earlier chars dropped]`를 머리에 넣는다.
- 잡이 정산되면 `result`(포맷된 최종 텍스트, `bash.rs:1113`)가 정본이다. `bash_output`은 `Running`이면 델타, 정산 후 첫 읽기면 `status:` + `result` 전문을 주고 커서를 끝으로 민다. 두 번 읽으면 `(no new output)`.
- `filter`는 `regex::Regex`로 컴파일해 라인 단위 적용. 컴파일 실패는 mhc와 같이 무시하고 전문을 준다(도구 오류로 승격하지 않음).
- `view: "screen"`은 PTY가 없으므로 `Err("view \"screen\" requires a PTY session; tau has no PTY runtime")`로 명시 거절한다. 스키마에는 넣지 않는다(§7 참조).

### 3.3 프로세스 트리 종료

- `start_background`가 만드는 `AbortController`(`bash.rs:1077`)를 `Job.cancel: Option<Arc<AbortController>>`에 보관한다. `kill_bash`는 `cancel.abort()` 한 번으로 `execute_shell`의 `signal.aborted()` 분기(`shell_host.rs:984`)를 타서 `cancel_token.cancel()` + `spawn_book.signal_groups(SIGKILL)`이 실행된다. 새 킬 코드를 쓰지 않고 기존 트리 종료를 재사용한다.
- 정산 텍스트가 `[Command aborted]`면 `JobStatus::Killed`로 기록한다(`bash.rs:1113`의 status 계산에 분기 추가).
- 유예가 필요한 경우를 위해 `ShellExecRequest`에 `cancel_grace: Option<Duration>`을 더하고, 있으면 `SIGTERM` → 유예 → `SIGKILL` 2단으로 바꾼다(`shell_host.rs:984` 지점). 1단계에서는 `None`(즉시 SIGKILL)로 두고 동작을 바꾸지 않는다.
- `all: true`는 `JobRegistry`에 `kill_all(&self) -> usize`를 추가해 `Running` 잡 전부에 abort를 걸고 개수를 반환한다. `teardown`처럼 서비스도 함께 정리하되 `bash.rs:940`의 `kill_requested` 경로를 그대로 쓴다.
- `proc://<id>/kill` 쓰기도 같은 경로로 연결한다(§3.5).

### 3.4 감시

- 새 모듈 `crates/omp-cli/src/tools/monitor.rs` + `monitor_registry.rs`. 레지스트리는 `Arc<Mutex<MonitorRegistry>>`를 `Env`에 두고(`tools.rs:113`의 `Env`에 `monitors` 필드 추가), 항목은 `Monitor { id: "mon_N", description, source: Source, filter: Option<Regex>, persistent: bool, deadline: Option<Instant>, paused: bool, fire_window, wake_tx }`.
- command 분기: 감시는 `start_background`와 같은 실행기를 쓰되 `Notify`(`bash.rs:180`)에 싱크를 꽂는다. `Notify`에 `sink: Option<mpsc::UnboundedSender<String>>`을 추가하고, 매칭 라인이 생길 때마다 보낸다. 정산 보고 경로(`append_notify_report`)는 감시 잡에서는 끈다.
- 깨우기 합치기: 수신 태스크가 첫 이벤트에서 깨어나 `tokio::time::sleep_until(now + 200ms)` 한 번으로 같은 창의 라인을 모은 뒤 한 번만 전달한다. 고정 주기 폴링이 아니라 이벤트 구동 배칭이다. 상한은 `NOTIFY_MAX_LINES`(`bash.rs:40`)를 재사용한다.
- path 분기: `notify` 크레이트의 `RecommendedWatcher`로 부모 디렉터리를 감시하고 `event: create|modify`로 거른다. 폴링 없음. 부모 디렉터리가 없으면 생성 시점에 오류. persistent면 상태 디렉터리에 등록 정보를 남긴다.
- rpc 분기: tau에는 피어 버스가 없다(`wait.rs:1`의 모듈 주석이 명시). `type: "rpc"`는 `Err("monitor type \"rpc\" is unsupported: no peer bus in this build")`로 즉시 거절한다.
- 데드라인: `tokio::time::timeout(deadline, watch_future)`로 감싼다. `persistent: true`면 데드라인 없음. 종료 사유는 `Exit|Timeout|Killed` 셋만 쓴다(`Disposed`는 세션 종료).
- 예산·일시정지: 시간당 깨우기 수를 세어 초과하면 `paused = true`로 두고 "completion still wakes this session" 안내를 한 번 보낸다. `action: "rearm"`은 `bash_id`(또는 전체)의 `paused`를 풀고 `fire_window`를 리셋한다.
- 전달: `Runtime::follow_up`(`runtime.rs:1858`)에 `<system-reminder>` 본문을 넣는다. 본문 형식은 기존 리마인더(`runtime.rs:1125`)를 따른다. 전달 큐는 `herdr.rs:322`의 단일 전달 스레드 + condvar 병합 패턴을 그대로 본뜬다(같은 종류 보고 병합, 순서 보존).
- 잡 정산 깨우기: `JobRegistry`에 `Arc<tokio::sync::Notify>`를 넣고 `update()` 끝에서 `notify_waiters()`를 부른다. `wait`는 `POLL_INTERVAL` 루프(`wait.rs:28`)를 버리고 이 알림을 `tokio::time::timeout(WAIT_MAX, notified)`로 기다린다. 이 조각에서 폴링을 실제로 제거하는 지점이다.

### 3.5 표면 정리

- `proc://<id>/kill`은 쓰기 스킴으로 등록해 `kill_bash`와 같은 함수를 호출한다(`jobs.rs:323`의 거절 문구를 유지하면서 `/kill`만 허용).
- rpc `abort_bash`(`rpc.rs:994`)는 현재 전역 단일 컨트롤러(`runtime.rs:3805`)만 끊는다. `bash_id` 필드가 오면 해당 잡만 끊도록 확장한다.
- `details`에 `bash_id`, `status`, `monitorMuted`를 싣는다.

## 4. 구현 단계 (커밋 단위)

1. `jobs`: `Job`에 `bash_id`, `cancel`, `dropped`, `cursor`, `kill_requested` 추가, `JobStatus::Killed`, `JobRegistry::notify`와 `update` 알림, `kill_all`. 동작 변화 없음. `bash.rs:1113`에서 `details.bash_id`만 노출.
2. `tools/bash_output.rs` 신설 + `tools.rs:321` 등록 + `tools.rs:677` 기대 목록 갱신. 델타 커서·필터·`(no new output)`·드롭 안내.
3. `tools/kill_bash.rs` 신설 + 등록. 단일/전체, `Killed` 상태 기록, 트리 종료는 `AbortController` 경유.
4. `tools/monitor.rs` + `monitor_registry.rs` command 분기. `Notify`에 싱크 추가, 배칭 전달, 데드라인, `rearm`, 예산 일시정지. `bash_output`에 mute 안내 연결.
5. path 분기(`notify` 크레이트를 `crates/omp-cli/Cargo.toml`에 추가) + persistent 상태 저장/복원.
6. `wait.rs` 폴링 제거(`JobRegistry::notify` 구독), `proc://<id>/kill` 쓰기, rpc `abort_bash`의 `bash_id` 확장.
7. 문서: `BASH_DESCRIPTION`(`tools.rs:252`)에서 "auto-deliver" 문구를 실제 동작(정산 시 `follow_up`)에 맞추고 `bash_output`/`kill_bash`/`monitor` 안내 추가.

## 5. 테스트

- `jobs`: `insert`가 `bash_id`를 채우는지, `update`가 `Notify`를 깨우는지(`notified()`를 먼저 등록하고 `timeout`으로 대기, sleep 없음), `kill_all`이 `Running`만 세는지.
- `bash_output`: 3청크를 tap에 밀어넣고 1회차가 전부, 2회차가 `(no new output)`, 드레인 후 1회차가 `[N earlier chars dropped]`를 내는지. `filter`가 라인 단위로 자르는지, 잘못된 정규식이 전문을 주는지.
- `kill_bash`: `sleep 60` 잡을 띄우고 `kill_bash` 후 `status`가 `killed`이고 손자 프로세스가 죽었는지(`ps -o pid= --ppid` 대신 `killpg` 대상 pgid 부재 확인). 대기는 `timeout(5s, settled)`로 상한.
- `monitor` command: `printf 'READY\n'; sleep 30`을 감시로 띄우고 깨우기 수신을 `mpsc`로 구독한 뒤 첫 이벤트를 `timeout(5s)`로 받는다. 구독 등록이 트리거보다 먼저다.
- `monitor` path: 임시 디렉터리에 파일을 만든 뒤 이벤트 1건을 `timeout`으로 받는다. `event: "modify"`는 기존 파일에 써서 확인.
- `monitor` rpc: 즉시 오류 문자열 회귀 테스트.
- `wait`: 잡 정산이 `wait`를 깨우는지, `POLL_INTERVAL` 상수가 사라졌는지.
- 스키마: `tools.rs:677`의 이름 목록과 각 `parameters()` 평면성(루트 `anyOf` 없음)을 고정.

## 6. QA 시나리오

1. `bash {command:"sleep 30; echo done", async:true}` → `details.bash_id` 확인 → `bash_output` 2회(델타/무변화) → `kill_bash` → `bash_output`이 `status: killed`.
2. 대량 출력(`seq 1 200000`) 잡에서 `bash_output`을 3회 호출해 잘림 없이 이어붙인 결과가 원문과 같은지, 드롭 안내가 정확한지.
3. `monitor {command:"tail -F app.log", filter:"ERROR", description:"errors in app.log"}` 후 로그에 ERROR 3줄 추가 → `<system-reminder>` 묶음 1회, 3줄 포함.
4. `monitor {path:"build/out.bin", event:"create", persistent:true}` 후 파일 생성 → 1회 깨우기. `kill_bash {bash_id:"mon_1"}`로 종료되고 상태 디렉터리가 지워지는지.
5. `kill_bash {all:true}`가 `Killed N session(s).`를 내고 `/jobs`·`asyncJobs`·`proc://` 목록이 비는지.
6. 감시 폭주(1초에 1000줄)에서 배칭·예산 일시정지가 걸리고 `bash_output` 머리에 mute 안내와 `details.monitorMuted`가 오는지. 이때 세션은 폴링 없이 유휴로 돌아가는지.

## 7. 위험과 미확인

- **PTY 없음.** `pty`는 스키마에만 있고(`tools.rs:335`) 읽는 코드가 없다. `view: "screen"`, `bash_input`의 키 전송, 화면 스냅샷은 이 조각에서 제공할 수 없다. 계약의 `bash_input`은 범위 밖이며 별도 조각이 필요하다.
- **자동 전달 미구현.** `jobs.rs:291`의 문구와 달리 잡 완료 시 `follow_up` 호출이 없다. §3.4의 정산 깨우기를 넣기 전까지 게이트웨이는 `agent_settled`를 영원히 못 받을 수 있다.
- **`agent_settled`.** `ext.rs:1593`은 확장 핸들러가 등록된 경우에만 보낸다. 게이트웨이가 확장을 안 띄우면 이 신호는 나가지 않는다. 이 조각에서 고치지 않는다.
- **`follow_up`이 유휴 세션을 깨우는지 확인 필요.** `runtime.rs:1858`은 `pa_agent::Agent::follow_up`(`vendor/pa-agent/src/agent.rs:1079`)으로 큐에 넣기만 한다. 유휴 상태에서 턴이 실제로 시작되는지 이 세션에서 실행 확인하지 못했다. 안 되면 monitor 전달은 `rpc.rs:654`의 `follow_up` 명령을 게이트웨이가 당겨가는 방식으로 대체해야 한다.
- **id 형식 확인 필요.** 계약 참조 구현은 `bash_N`(`manager.rs:1082`), tau는 `bg_N`(`jobs.rs:131`). 게이트웨이가 id를 불투명 문자열로 다루는지 검증하지 못했다. 불투명하지 않으면 `bash_N`으로 정본을 바꾸는 편이 낫다.
- **`notify` 크레이트 추가.** 워크스페이스에만 있고(`Cargo.toml:125`) `omp-cli` 의존성에는 없다. path 감시 도입은 `crates/omp-cli/Cargo.toml` 변경을 동반한다.
- **rpc 감시 분기 미지원.** 피어 버스가 없어(`wait.rs:1`) `type: "rpc"`는 거절한다. 게이트웨이가 이 분기를 실제로 쓰면 별도 설계가 필요하다.
- **`all`의 범위.** `kill_bash {all:true}`가 서비스까지 포함하는지 계약 문구가 모호하다. 참조 구현은 파일·rpc 감시까지 세고 명령 감시는 세션으로 한 번만 센다. tau는 잡+서비스+감시를 각각 세되 명령 감시는 중복 계수하지 않는 쪽으로 맞춘다(검증 필요).
- **세션 파일·종료 신호·확장 호스트의 나머지 계약**(`interrupt`, `set_client_info`, `api.rpc.handle`, senpi 세션 형식, `~/.omo` 설정 루트)은 이 조각 범위 밖이라 조사하지 않았다. 슬랙 1795326973.474349의 5개 범주 중 이 조각은 "백그라운드 셸 세션 관리와 관찰" 하나만 다룬다.
- 코드·테스트는 작성·실행하지 않았다(규칙상 저장소 무수정). 위 테스트 목록은 실행 전 계획이며 통과 증거가 아니다.
