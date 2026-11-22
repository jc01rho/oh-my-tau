# tau 에이전트 도구 10종·하네스 운영 5종 교차 리뷰와 Wave 1~4 구현 로드맵

## 1. 현재 상태 (근거)
기준: main e7a5378, 워크스페이스 버전 0.12.0(Cargo.toml `[workspace.package]`), 멤버 `crates/*`와 `vendor/*`.

크레이트 의존성 그래프 (각 crates/*/Cargo.toml에서 추출, 내부 의존만)
- omp-cli -> omp-collab, omp-compact, omp-eval, omp-ext, omp-goose, omp-judge, omp-lsp, omp-mcp, omp-providers, omp-sandbox, omp-session, omp-settings, omp-stats, omp-task, omp-tui, omp-urls, omp-wasm, omp-shell(-builtins), omp-tool, omp-xfs, pa-agent/pa-ai/pa-types
- omp-task, omp-settings, omp-collab, omp-eval, omp-mcp, omp-lsp, omp-sandbox: 내부 의존 없음 (잎 노드)
- omp-session -> pa-types / omp-compact, omp-judge -> omp-session / omp-ext -> pa-agent / omp-tui -> omp-session, omp-settings / omp-providers -> omp-settings, omp-stats
- 결론: 도구와 RPC는 모두 omp-cli에 모인다. 새 상태를 잎 크레이트(omp-task, omp-session, omp-settings)에 두고 omp-cli에서 배선하면 순환 없이 넣을 수 있다.

에이전트 도구 10종
- 등록 지점: `builtin_tools_with_env`의 `vec![mk(...)]` (crates/omp-cli/src/tools.rs:321-409). 숨김 도구는 think/yield/goal 세 가지뿐이다 (tools.rs:510-516).
- task: 동기 일괄 실행이다. `TaskRunner::new(binary, ...)` 다음 `runner.spawn_all(specs).await`가 끝나야 반환한다 (crates/omp-cli/src/tools/task.rs:483-485). `TaskRunner::spawn`/`spawn_all` (crates/omp-task/src/runner.rs:286, 306)에는 실행 중 핸들을 들고 있는 레지스트리가 없다.
- 서브에이전트 RPC는 스텁이다. `get_subagents`는 빈 배열 (crates/omp-cli/src/rpc.rs:863), `cancel_subagent`는 `{"cancelled": false}` (rpc.rs:865-868), `steer_subagent`는 항상 "Subagent not running" (rpc.rs:869-879). 따라서 task_send/task_output/task_cancel의 기반이 없다.
- bash: `async`/`notify`/`name`/`ready` 파라미터가 있다 (tools.rs:328-347). 백그라운드 판정은 bash.rs:672-735, 레지스트리는 `JobRegistry` (crates/omp-cli/src/tools/jobs.rs:125)이고 `insert`/`get`/`update`/`sorted`/`async_snapshot` (jobs.rs:131-177)과 `kill_requested` 필드(jobs.rs:98)를 갖는다. 조회 경로는 `read proc://` (jobs.rs:308)와 `wait` 도구 (crates/omp-cli/src/tools/wait.rs:44)뿐이다. 잡 id 형식은 `bg_N`이다 (jobs.rs:130, 291-299).
- bash.rs:174-175 주석은 "omp-rs has no follow-up channel on the job surface"라고 적는다. 잡이 끝났을 때 턴을 깨우는 통로가 없다는 뜻이고, monitor에는 이 통로가 필요하다.
- RPC `abort_bash`는 전체 중단만 한다 (rpc.rs:994-997). 특정 잡을 지정해 죽이는 kill_bash 수준 API는 없다.
- memory: `memory.backend` 설정이 있고 값은 off|local|hindsight|mnemopi|sharpshooter, 기본 off (crates/omp-settings/src/schema.rs:1877-1879). 도구 등록은 없다 (tools.rs:321-409). 계획 문서는 `crates/omp-memory`를 Phase 3.3으로 미뤄 두었다 (docs/rust-migration-plan.md:341).
- workpool: tau 전체에서 대응 코드를 찾지 못했다. `workpool|task_send|bash_output|kill_bash|memory_apply_patch`로 grep하면 crates/omp-eval prelude, omp-tui, settings 스키마 문자열만 나온다. 의미가 같은지는 확인 필요.
- ask: html `<form>`을 기본으로 받고 레거시 `questions[]`(`multi`, `recommended`)를 함께 받는다 (crates/omp-cli/src/tools/ask.rs:39-44, 748). RPC 모드에서는 `RpcAskUi`로 교체된다 (rpc.rs:1390-1397).
- goal: `op` enum이 create|get|complete|pause|resume|drop이고 (crates/omp-cli/src/tools/goal.rs:25) `goal_mode_active`일 때만 도구 목록에 들어간다 (tools.rs:501-502).

하네스·운영 5종
- 설정 루트: `agent_dir()`가 `~/.omp`로 고정되어 있고 `OMP_PROFILE`이 있으면 `~/.omp/profiles/<p>/agent`를 쓴다 (crates/omp-settings/src/settings.rs:459-470). CLI는 `--profile`을 받는다 (crates/omp-cli/src/cli.rs:131, 322; main.rs:303). `~/.omo`나 `omo.jsonc`를 읽는 코드는 없다(grep 0건). 동시 실행 수는 `task.maxConcurrency` 하나다 (task.rs:46-49, runner.rs:244).
- 종료 신호: RPC 스트림에는 `session_settled`만 나간다 (crates/omp-cli/src/runtime.rs:1611, 1641). `agent_settled`는 확장 핸들러가 있을 때 `emit_nowait`로 확장에만 보낸다 (crates/omp-cli/src/ext.rs:1593-1597). RPC writer는 `agent_end`/`message_end`만 특별 처리한다 (rpc.rs:1335-1342).
- 세션: `SESSION_VERSION: u32 = 3` (crates/omp-session/src/lib.rs:25). 헤더는 `{"type":"session","version":3,...}` (lib.rs:225, 246). 쓰기 측은 `SessionWriter::create/open` (lib.rs:208, 257)이다. senpi 변환기는 없다.
- ultrawork/ulw-loop: crates 아래에서 `ultrawork|ulw` grep 결과가 0건이다. 해당 하네스는 없다.
- 미머지 3·4·5단계: PR #1~#5(phase0~2.5)는 모두 머지됐다(GitHub API, 2026-11-20~21 merged_at). Phase 3(docs/rust-migration-plan.md:335), Phase 4(:345), Phase 5(:354)는 PR이 없다.

## 2. 게이트웨이가 요구하는 계약
mhc 기준이며, 아래 항목의 필드 단위 스키마는 모두 mhc 소스와 대조해야 한다(확인 필요).
- 도구: `task_send(to, message)`, `task_output(task_id|name, mode=status|tail|full)`, `task_cancel`, `bash_output(bash_id, filter, view)`, `kill_bash(bash_id|all)`, `monitor(command|path|target, filter, timeout_ms, persistent)`, `memory(command=create|str_replace|insert|delete|rename|update_description)`, `memory_apply_patch(reason, input)`, `workpool(op=create|push|close|inspect|cancel|yield)`, `ask_user_question(questions[header,options,multiSelect], waitForAnswer)`, `create_goal`/`get_goal`/`update_goal(status=complete|blocked, reason)`.
- 백그라운드 완료는 다음 도구 경계나 유휴 시점에 턴으로 자동 주입돼야 한다(폴링 금지 모델).
- 운영: 설정 루트 `~/.omo`, omo.jsonc 프로필로 장비별 동시 실행 수 지정, 턴 종료 신호 `agent_settled`, 세션 파일 senpi 형식.
- 슬랙 1795326973.474349의 5개 범주는 원문을 보지 못했다. 이 문서는 브리프의 5개 범주(RPC 8종, CLI 플래그 4종, 확장 호스트, 도구 10종, 하네스·운영 5종)를 모두 Wave에 배치했다. 원문과의 일치는 확인 필요.

## 3. 설계
원칙: 상태는 잎 크레이트에 두고 omp-cli는 배선만 한다. 기존 omp 동작은 바꾸지 않고 게이트웨이용 이름은 별칭으로 추가한다.

- D1 종료 신호: `Runtime`의 settle 경로(runtime.rs:1636-1643)에서 `session_settled` 바로 다음에 `{"type":"agent_settled"}`를 `self.send`한다. 켜는 조건은 `set_client_info`로 받은 클라이언트 종류나 `--session-runtime` 플래그다. omp 클라이언트의 바이트 호환은 그대로 둔다. ext.rs:1593 경로는 손대지 않는다.
- D2 잡 도구: `crates/omp-cli/src/tools/jobs.rs`에 `JobRegistry::read_since(id, offset, filter)`, `kill(id)`(`kill_requested` 설정 + 프로세스 그룹 종료), `kill_all()`을 추가한다. 새 파일 `tools/bash_output.rs`, `tools/kill_bash.rs`, `tools/monitor.rs`는 이 API의 얇은 래퍼다. bash 스키마에 `run_in_background`를 `async`의 별칭으로 넣고, 반환 텍스트에 `bash_id`(=`bg_N`)를 싣는다.
- D3 후속 알림 통로: Env에 `FollowUpTx`(mpsc)를 두고, 잡 완료와 monitor 매치를 `Runtime`의 follow_up 큐(rpc.rs:654 `"follow_up"`와 같은 큐)로 넣는다. bash.rs:174의 공백을 메우는 것으로 monitor·task·workpool이 모두 이 통로를 공유한다.
- D4 태스크 레지스트리: `crates/omp-task/src/registry.rs`에 `TaskRegistry { id -> TaskHandle{child, status, transcript_path, stdin_tx} }`를 신설한다. `TaskRunner::spawn`이 핸들을 등록하고 `RunOutput`을 비동기로 채우게 바꾼다. task 도구에 `run_in_background`를 추가하고 기본값은 기존과 같은 동기로 둔다. `task_send`는 자식 stdin에 RPC `steer`를 쓰는데, 자식이 RPC 모드로 떠야 하므로 runner/control.rs의 자식 실행 모드를 확인해야 한다(확인 필요). `task_output`/`task_cancel`과 RPC `get_subagents`/`cancel_subagent`/`steer_subagent`(rpc.rs:863-879)도 같은 레지스트리로 실구현한다.
- D5 workpool: D4 위에 `crates/omp-task/src/workpool.rs`를 둔다. 키별 큐, keep_alive/fresh, close 시 집계 1건을 D3 통로로 전달한다.
- D6 메모리: 신규 잎 크레이트 `crates/omp-memory`를 만든다. 내용은 YAML frontmatter 파일 저장소, apply_patch 파서, git 커밋이다. `memory.backend`(schema.rs:1877)에 값 `omo`를 추가해 켠다. Phase 3.3의 SQLite/FTS 설계와는 백엔드 trait 하나로 합친다. git 호출은 Phase 5.1의 gix 방침과 충돌하므로 처음에는 `Command`로 하고 gix 전환 대상으로 표시한다.
- D7 ask/goal 호환: `tools/ask.rs`에 `ask_user_question` 별칭 스키마를 추가하고 `header`/`multiSelect`를 기존 `SelectOption`/`multi`로 매핑한다. `waitForAnswer=false` 처리는 D3 통로로 넘긴다. `tools/goal.rs`에 `create_goal`/`get_goal`/`update_goal` 래퍼를 추가하고 `blocked`를 `GoalStatus`(goal.rs:36)에 넣는다.
- D8 설정 루트: `agent_dir()`(settings.rs:459)가 `OMT_HOME`/`--config-root`를 우선하게 한다. 새 모듈 `crates/omp-settings/src/omo.rs`가 `~/.omo/omo.jsonc`의 프로필에서 동시 실행 수를 읽어 `task.maxConcurrency` 기본값으로 쓴다. JSONC 키 이름은 확인 필요.
- D9 세션 호환: `crates/omp-session/src/senpi.rs`에 v3를 senpi로 내보내고 senpi를 v3로 읽어오는 양방향 변환을 둔다. 쓰기 경로(lib.rs:225)는 v3로 유지하고, 게이트웨이용 senpi 사본은 `--session-runtime` 모드에서만 미러링한다. senpi 스키마는 확인 필요.
- D10 ultrawork: 프롬프트에서 키워드를 감지해 부트스트랩을 주입한다. 후보 위치는 `crates/omp-cli/src/methodology.rs` 또는 `sysprompt.rs`인데, 두 파일의 현재 역할은 확인 필요. ulw-loop는 D7의 goal 래퍼 위에서 동작한다.

## 4. 구현 단계 (커밋 단위)
Wave 1 — 신호·프로토콜. PR-A, 저위험, Phase 4.4와 같은 파일을 만지므로 먼저 머지한다.
1. rpc.rs `dispatch`(rpc.rs:593)에 `get_protocol_info`, `set_client_info`, `interrupt`(=`abort`, rpc.rs:677 경로 재사용) 추가
2. runtime.rs settle 경로에 `agent_settled` 조건부 송신 (D1)
3. cli.rs `VALUELESS_FLAGS`(cli.rs:142)에 `--no-model-fallback`, `--no-recommended-models`, `--multi-session` 추가. `STRING_FLAGS`(cli.rs:97)에 `--session-runtime` 추가. 처음에는 파싱과 `Args` 필드만 넣는다
4. settings.rs `agent_dir()` 루트 오버라이드와 `omo.rs` 프로필 로더 추가 (D8)

Wave 2 — 잡 도구와 호환 별칭. PR-B
5. jobs.rs `read_since`/`kill`/`kill_all` 추가 + 단위 테스트
6. FollowUpTx 통로 (D3)와 bash 완료 시 주입
7. bash_output·kill_bash·monitor 도구 등록 (tools.rs:404 부근), bash `run_in_background` 별칭
8. ask_user_question·goal 래퍼 (D7)

Wave 3 — 태스크·메모리. PR-C(task/workpool), PR-D(memory)로 나눈다
9. omp-task `registry.rs` + `TaskRunner::spawn` 핸들 등록
10. task `run_in_background`, task_send/task_output/task_cancel, rpc.rs:863-879 실구현
11. workpool.rs + 도구 등록
12. crates/omp-memory 신설 + memory/memory_apply_patch 도구 (Phase 3.3과 합류)

Wave 4 — 세션·멀티세션·확장. PR-E, PR-F
13. omp-session senpi.rs 변환기와 골든 픽스처
14. `resume_session`/`release_session`/`move_session` + `--multi-session` 배선(세션별 `Runtime` 맵)
15. ext-host/host.ts에 `api.rpc.handle` 추가, `session_move_export/import` 이벤트, `extension_request`/`reload` RPC
16. ultrawork 부트스트랩 (D10)

## 5. 테스트
- 단위: `JobRegistry::read_since`의 오프셋/필터, `kill` 후 `JobStatus::is_terminal`(jobs.rs:74) 확인. `TaskRegistry` 등록과 해제. `omo.rs` JSONC 파싱. senpi 왕복 변환(v3 -> senpi -> v3 동등).
- RPC 통합: recovery.rs:1205-1213이 `session_settled`까지 프레임을 모으는 패턴을 그대로 써서 `agent_settled`가 `session_settled` 바로 뒤에 정확히 1회 나오는지 단언한다.
- cli.rs 기존 `parse(&[...])` 테스트 스타일(cli.rs:571, 655)로 신규 플래그를 검증한다.
- 비동기 단언은 이벤트를 먼저 구독한 뒤 트리거하고 bounded timeout으로 기다린다. 고정 sleep은 쓰지 않는다.
- 회귀: task.rs:869 `schema_matches_ts_reference`, tools.rs:662 도구 목록 테스트를 별칭 추가 후 갱신한다.

## 6. QA 시나리오 (게이트웨이 카나리 전환 체크리스트)
1. `omt --mode rpc --session-runtime gateway`로 띄운 뒤 `get_protocol_info`/`set_client_info`가 성공하고 "Unknown command"가 0건이다.
2. 짧은 프롬프트 1회에서 `agent_end` -> `session_settled` -> `agent_settled` 순서가 나오고 게이트웨이가 턴을 닫는다.
3. `bash run_in_background` -> `bash_output` 증분 -> `kill_bash` -> 완료 알림 자동 주입. 폴링 호출은 0회여야 한다.
4. 백그라운드 task 2개 생성 -> `task_send`로 조향 -> `task_output tail` -> `task_cancel`. `get_subagents`에 실제 목록이 보인다.
5. `memory create` 후 git 커밋 확인, `memory_apply_patch` 다중 헌크 적용.
6. 세션 종료 후 senpi 사본을 게이트웨이가 읽고 `resume_session`으로 이어 붙인다.
7. omo.jsonc 저사양 프로필에서 task 동시 실행이 프로필 값을 넘지 않는다.
8. 카나리 운영: 장비 1대, 작업 10%, 롤백은 엔진 스위치를 mhc로 되돌리는 것. 판정 지표는 미종료 턴 0, Unknown command 0, 세션 재개 실패 0.

## 7. 위험과 미확인
- 후속 통로(D3)는 Runtime 큐 설계를 건드린다. steer/follow_up 모드(rpc.rs:946-953)와의 우선순위는 확인 필요.
- task 자식이 RPC stdin을 받는지(runner/control.rs) 확인 필요. 받지 않으면 task_send를 넣기 전에 자식 실행 모드부터 바꿔야 한다.
- Phase 4.4(rpc.rs 청킹 정렬)와 Wave 1이 같은 파일에서 충돌하고, Phase 3.3과 D6의 메모리 설계가 겹치며, Phase 5 워크트리 격리와 task isolated 옵션이 겹친다. 계획 문서 소유자와 조율해야 한다.
- senpi 스키마, omo.jsonc 키, mhc 도구의 정확한 JSON 스키마, `agent_settled` 페이로드 필드는 모두 확인 필요.
- `--multi-session`은 `RUNTIME_TX` thread-local 단일 런타임 구조(rpc.rs:1383)와 정면으로 충돌한다. Wave 4에서 가장 큰 리팩터가 된다.
- ext.rs:1593의 확장용 `agent_settled`와 D1의 RPC `agent_settled`가 같은 이름으로 두 경로에 나간다. 의미가 갈라지지 않도록 문서화가 필요하다.
- 슬랙 원문(1795326973.474349)을 직접 읽지 못했으므로 5개 범주의 일치는 확인 필요.
