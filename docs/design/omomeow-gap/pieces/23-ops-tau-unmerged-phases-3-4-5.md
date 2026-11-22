# 미머지 tau 계획 3·4·5단계 연계 설계: OmOMeow 엔진 전환 선행 조건

## 1. 현재 상태 (근거)

기준: `/data/home/git/oh-my-tau` HEAD `e7a5378`("phase2.5: omp-xfs·pi-vfs·pi-ast·pi-edit·pi-predict 업스트림 전환 및 pi-walker 제거 (#5)"). 즉 `docs/rust-migration-plan.md`가 전제한 Phase 0/1/2/2.5는 상당 부분 이미 머지되었고, 문서가 HEAD보다 낡았다.

- Phase 0 게이트는 실재한다. `justfile:82`의 `ci: fmt-check lint deny check test tools-parity golden`, `.github/workflows/ci.yml:16,26,43,44,52,68`의 fmt/clippy/deny/parity/test/tui-e2e. `vendor-src.lock`도 존재하며 `reference-commit = "b636e19d10…"`(main), `"2f92f3b5…"`(omp2), 크레이트별 `import-commit`을 기록한다.
- Phase 2(셸)는 머지됐다. `Cargo.toml` members에 `vendor/omp-shell`, `vendor/omp-shell-builtins`, `vendor/omp-core`, `vendor/omp-tool`, `vendor/omp-vocab`, `vendor/xutf`가 있고, `crates/omp-cli/src/tools/bash.rs:3-8`은 "Local finite commands run in-process via `omp-shell` and `omp-shell-builtins`(`tools/shell_host.rs`, `PathPolicy`, `SpawnWrapper`, `-pgid` teardown, `dyn` host bridge)"라고 기술한다. 계획 §4.2의 "`bash -c` 스폰, `brush` 오기"는 해소됐다.
- Phase 2.5도 머지됐다. `vendor/omp-walker`·`vendor/omp-xfs`가 있고 `crates/omp-cli/src/tools/{find,search,edit,ast}.rs`가 `omp_xfs::walk`/`glob`를 직접 쓴다(`find.rs:7,22-23`).
- Phase 1 원장은 존재한다: `docs/parity/{tools,slash,settings,shell,tui}.md`. `docs/parity/slash.md:113`은 `/handoff`를 `STUB`으로, `docs/parity/tools.md`는 visible 18/29를 기록한다.
- 3단계(기억·handoff·컨텍스트)는 미머지다.
  - `/handoff`는 스텁: `crates/omp-cli/src/slash.rs:887` → `handoff_command`(`slash.rs:3000-3011`)가 "not wired into the omt runtime" 통지만 낸다. RPC도 동일: `crates/omp-cli/src/rpc.rs:1022-1028`이 streaming 가드 후 `Err(unsupported("Handoff"))`. 라이브러리는 완비돼 있다 — `crates/omp-compact/src/handoff.rs`(183줄)의 `render_handoff_prompt`, `build_handoff_request`, `handoff_summary_from_document`, `handoff_compaction_entry`, `append_handoff`, `handoff_result`.
  - 기억 크레이트 부재: `crates/` 20종에 `omp-memory`가 없다. `/memory`는 `slash.rs:757-770`에서 "Memory payload is empty.", "Memory is not available for the off backend" 리터럴. 설정은 `crates/omp-settings/src/schema.rs:1765-1877`에 `memories.*` 17키 + `memory.backend`(enum `off|local|hindsight|mnemopi|sharpshooter`, 기본 `"off"`)가 선언만 되고 소비처가 없다. 계획 §4.3의 "74개 메모리 키"는 실측과 불일치(18키) — 문서 정정 필요.
  - `rule://`는 `crates/omp-urls/src/schemes/rule.rs:13-14`의 `NOT_IMPLEMENTED` 상수로 명시 오류화되어 있다(조용한 빈 벡터는 이미 제거됨).
  - 컨텍스트 파일은 `crates/omp-cli/src/sysprompt.rs:161`이 `["AGENTS.md","CLAUDE.md"]` 상향 탐색만 한다. `.omp/AGENTS.md`(우선순위 100)·`RULES.md`·`GEMINI.md` 없음(grep 0건).
  - `task.rs:34-37`은 "recall/reflect/retain/memory_edit … Re-add them with the Phase 3 memory crate"라고 스스로 미구현을 표시한다.
- 4단계(MCP 공유·RPC v2)는 부분 미머지다.
  - RPC v2 프레임은 이미 있다: `rpc.rs:50` `MAX_FRAME_BYTES = 1_048_576`, `rpc.rs:1225-1325` `FrameEncoder` + `rpc_chunk`, `rpc.rs:1031` `get_messages_page`, `rpc.rs:595-597` `negotiate_protocol`이 version 2를 수용, `rpc.rs:1487` `host_tool_result|host_tool_update|host_uri_result`, `rpc.rs:318/825` `set_host_uri_schemes`(브리지 없음 응답).
  - 게이트웨이가 요구하는 8종은 전부 부재: `rpc.rs`에서 `interrupt`, `set_client_info`, `get_protocol_info`, `reload`, `resume_session`, `release_session`, `extension_request`, `move_session` 문자열 카운트 0(`attach_session`/`detach_connection`도 0).
  - MCP는 세션 간 공유가 없다. `crates/omp-mcp/src/lib.rs:9-12`가 "Not ported: OAuth flows, Smithery, tool caching across sessions, prompts/resources"라고 명시하고, `crates/omp-cli/src/tools/mcp.rs`의 `mcp_tools(cwd)` + `static CONNECTED_SERVERS`는 프로세스 지역 상태다. `crates/omp-mcp/src/manager.rs:55-60`의 `McpManager`는 per-server lazy connect만 한다.
- 5단계(VCS·격리)는 미머지다.
  - `gix`/`git2` 의존 0건(모든 `Cargo.toml` grep). `crates/omp-cli/src/features/worktree.rs:123-131`의 `fn git`이 `Command::new("git")`로 fork+exec하고 stderr 문자열을 판정하며, `list_worktrees`(`worktree.rs:155`)는 `git worktree list --porcelain`을 파싱한다. `sysprompt.rs:103`은 `.git` 상향 탐색을 중복 구현한다. crates 전체 `Command::new("git")` 3건.
  - `vendor/`에 `omp-vcs`·`omp-iso`가 없다(`vendor/` 목록: crossterm, laya-ort, omp-core, omp-macros, omp-proto, omp-shell, omp-shell-builtins, omp-tool, omp-vocab, omp-walker, omp-xfs, pa-*, pi-*, xutf).
  - `crates/omp-sandbox`(376줄)는 실동작한다: `kernel_status`(`lib.rs:33/56`), `apply`(64), `parse_sandbox_args`(204), `exec_confined`(252). 배선은 `crates/omp-cli/src/features/sandbox.rs:127-143` `wrap_inner` → `tools/bash.rs:838`. 단 Landlock+seccomp 쓰기/네트워크 차단뿐이고 CoW·워크트리 격리는 없다. `worktree.base|clone|onStart|onExit`(`schema.rs:3899-3927`)는 선언만.
- 종료 신호: `rpc.rs:1202`가 `agent_end` 프레임을 내고, `agent_settled`는 `crates/omp-cli/src/ext.rs:1593-1595`에서 `host.has_handlers("agent_settled")`일 때만 확장 이벤트로 emit된다. 즉 확장 호스트가 없으면 게이트웨이는 영원히 settle을 못 본다.
- 확장 호스트: `crates/omp-ext/src/host.rs`(1,362줄) 공개면은 `spawn/request/respond/emit/emit_nowait/has_handlers/list_tools/call_tool/list_commands/call_command/set_ctx/shutdown`이고, `ext-host/host.ts:1022-1054`의 메서드 분기는 `emit|list_tools|call_tool|cancel_tool|list_commands|call_command|set_ctx|list_flags|set_flag_value|list_providers|bus_emit|shutdown`뿐이다. `api.rpc.handle`도 `session_move_export|import` 이벤트도 없다(grep 0).
- 다중 세션 기반은 부분적으로 있다: `crates/omp-cli/src/subcmd/daemon.rs:1-25`가 `$XDG_RUNTIME_DIR/omt.sock`에서 연결마다 `omt --mode rpc` 자식을 띄우고 첫 줄 `{"type":"hello","cwd":…}`를 받는다. 그러나 `cli.rs:177`의 `MODE_VALUES`는 `text|json|rpc|acp|rpc-ui`이고 `--multi-session`·`--session-runtime`·`--no-model-fallback`·`--no-recommended-models`·`--listen`은 `cli.rs`에 없다(grep 0).

## 2. 게이트웨이가 요구하는 계약

mhc(`~/Work/mhc`, `f0f3571c5`)를 계약 원본으로 실측한 결과다.

- 기동: `crates/maho-rpc/src/host_ensure.rs:724` `host_child_argv` = `["--mode","rpc","--multi-session"]` + 인자. `crates/maho-rpc/src/host_lifecycle_launch.rs:161`은 `["--mode","rpc","--multi-session","--listen", …]`. `crates/maho-rpc/src/host_launch_spec.rs:224`는 `--session-runtime <in-process|worker>`(값 검증 `:87`, 기본 `:258`)를 붙이고 `--extension <abs>`도 넘긴다(`:245`).
- 명령 봉투: `crates/maho-rpc/src/rpc_types.rs:121-135` `RpcCommand { id, session_id, #[serde(flatten)] body }`, 본문 태그는 `#[serde(tag="type", rename_all="snake_case", rename_all_fields="camelCase")]`(`:132-140`). 호스트 요청은 별도로 `#[serde(tag="method")]`(`:12`).
- 명령 집합(부재 8종의 원형): `Interrupt{turn_id,intent}`(`rpc_types.rs:363`), `Reload`(225), `CheckReloadVeto`(226), `ExtensionRequest`(489), `SetClientInfo`(519), `ReleaseSession`(593), `ResumeSession`(649). 세션 이동은 `crates/maho-rpc/src/session_move_wire.rs:4-36`의 `MOVE_SESSION_CAPABILITY="move_session"`, `MOVE_SESSION_REVOKE_CAPABILITY`, `RpcMoveSessionCommand::{Prepare,Cancel,Status,Export{…},Import{bundle_dir,cwd,from},Finish,Revoke}` + 오류코드 `invalid_move_request|session_open_here|move_not_released|move_failed`.
- 프로토콜 정보: `crates/maho-rpc/src/host_decision.rs:8` `HOST_PROTOCOL_VERSION = 1`, 응답은 `crates/maho-rpc/src/connection_handler.rs:1555-1560`에서 `data["protocolVersion"]` + `data["capabilities"]`(=`multi_session_host_capabilities`, `host_capabilities.rs:95-170`: `multi_session`, `session_lifecycle`, `move_session`, `resume_session`, `observe_reads`, `detach_connection`, `unload_session` 등).
- 종료: 게이트웨이는 `agent_settled`를 기다린다(`crates/maho-rpc/src/host_session_wait.rs:38-42`, `session_binding.rs:92`, `tests/rpc_event_stream.rs:193`).
- 게이트웨이 크레이트 자체는 엔진을 직접 스폰하지 않는다: `crates/maho-gateway/Cargo.toml` 의존성은 `maho-ext-api`, `maho-config`뿐이고 설명은 "delivery as a `session_control_delivery` custom message on the senpi engine contract"이다. 실제 엔진 기동 경로는 확인 필요.
- tau 대비 격차 5범주로 정리된다: (1) RPC 8종+봉투/sessionId, (2) CLI 4플래그+`--listen`, (3) 확장 `api.rpc.handle`·`session_move_*` 이벤트, (4) 도구 8종(`task_send/task_output/task_cancel`, `bash_output/kill_bash/monitor`, `memory/memory_apply_patch`, `workpool`), (5) 설정 루트(`~/.omp` vs `~/.omo`)+종료 신호(`agent_end` vs `agent_settled`)+세션 포맷(JSONL v3 vs senpi).

## 3. 설계

전제: tau는 "게이트웨이가 mhc에 붙이던 senpi 계약"을 흉내내는 것이 아니라, **같은 계약을 tau 고유 자산(JSONL v3, `omp-sandbox`, `omp-xfs`)을 유지한 채 만족**시킨다. 계약 어댑터는 `crates/omp-cli/src/rpc.rs` 한 곳에 두고, 상위 계층은 tau 네이티브를 유지한다.

### 3.1 3단계 — 배선·기억 (선행: 없음, 즉시 착수 가능)

- `HandoffRunner`: `rpc.rs:1022`의 `unsupported("Handoff")`를 `omp_compact::handoff` 호출로 교체. 필요한 것은 `pa_types::ai::Context` + 현재 `Session` 핸들 + `FileOperations`이며, `handoff_compaction_entry`/`append_handoff`가 이미 compaction 엔트리 형식을 만든다. `slash.rs:3000`도 같은 함수를 공유해 이중 구현을 막는다. 스트리밍 가드(`rpc.rs:1024`)는 유지.
- `crates/omp-memory` 신설: `MemoryStore`(SQLite + FTS), `MemoryRecord`, `RecallQuery`, `RetainOutcome`. 스키마는 `schema.rs`의 `memories.*` 17키를 그대로 소비한다(`memories.phase1InputTokenLimit` 등은 이미 정의되어 있으므로 새 키를 만들지 않는다). `memory.backend` enum의 `local`만 구현하고 `hindsight|mnemopi|sharpshooter`는 명시 `unsupported` 오류.
- `memory://` 스킴: `crates/omp-urls/src/schemes/`에 `memory.rs` 추가, `Registry` 등록(`omp-urls/src/lib.rs:16`). `rule://`와 동일하게 백엔드 `off`면 `NOT_IMPLEMENTED` 계열 명시 오류(조용한 빈 결과 금지).
- 도구 등록: `tools.rs:260` `builtin_tools`에 `recall`/`retain`/`reflect`/`memory_edit`/`learn` 추가, `tools/task.rs:34-37`의 유령 주석 제거. `/memory`(`slash.rs:757`)는 `MemoryStore` 실데이터로 교체.
- 컨텍스트: `sysprompt.rs:161`의 이름 배열을 우선순위 테이블로 확장(`.omp/AGENTS.md` 100 > `AGENTS.md` > `RULES.md` 끈끈한 규칙 > `CLAUDE.md` > `GEMINI.md`), 사용자 스코프 `~/.omp/agent/AGENTS.md` 추가.
- 게이트웨이 도구 계약: `task_send`/`task_output`/`task_cancel`은 `crates/omp-task/src/runner.rs`의 `TaskRunner`+`TaskSpec{id,agent,prompt,cwd,output_schema,schema_mode,model,tools,depth,extra_args}`를 그대로 쓰고, `tools/task.rs`가 지금 동기 수집하는 결과를 `agent://<id>` 핸들 테이블로 바꾼다. `bash_output`/`kill_bash`/`monitor`는 `tools/jobs.rs`의 `JobRegistry`/`Service`/`ServiceState` 위에 얹는다(`bash.rs:24,838`). `workpool`은 `tools/eval_agents.rs:330,1759-1948`의 `run_workpool`을 top-level 도구로 승격한다.

### 3.2 4단계 — MCP 공유·RPC v2 (선행: 3.1의 도구 핸들 테이블)

- MCP 공유: `omp-mcp`에 `McpDaemon`(세션 간 서버 1개, 연결 참조계수) 추가. 전송은 `crates/omp-mcp/src/transport/{stdio,http}.rs`를 유지하고 데몬 경계만 `subcmd/daemon.rs`의 Unix 소켓 규약을 재사용한다. `McpManager`는 데몬 클라이언트로 축소하고 `tools/mcp.rs`의 `CONNECTED_SERVERS` 정적 상태를 데몬 조회로 대체. OAuth는 `omp-auth-broker`의 자격증명 저장을 재사용.
- RPC 8종 추가: `rpc.rs` `dispatch`에 `interrupt`(`rt.abort()` + `turn_id` 반향), `set_client_info`, `get_protocol_info`(`ready` 프레임의 `supportedProtocolVersions`/`maxFrameBytes`를 재사용하되 `capabilities` 배열 추가), `reload`, `resume_session`, `release_session`, `extension_request`, `move_session`(=`session_move_wire` 7개 op).
- 봉투 정합: `rpc.rs:203-214` `response()`는 `{id,type:"response",command,success,data}`인데 mhc는 `RpcCommand{sessionId}` + `RpcResponse{command,result}`다. **최소 변경 원칙**으로 `sessionId`를 요청/응답 양쪽에 additive로 넣고 `command` 필드를 유지한다(게이트웨이가 `command`를 읽는지 확인 필요 — 확인 전에는 두 필드 병기).
- 종료 신호: `ext.rs:1593`의 `has_handlers` 게이트를 없애고 `agent_settled`를 **항상** RPC 프레임으로도 emit(`rpc.rs:1202`의 `agent_end` 옆). 확장 이벤트는 기존 경로 유지. `willContinue`가 true인 중간 `agent_end`에서는 settle을 내지 않는다(`rpc.rs:1340`의 terminal 판정 재사용).
- 세션 이동: `Session`(`crates/omp-session/src/lib.rs:208-305`)은 경로 고정 writer라 `/move`가 거부된다(`slash.rs:2927-2960`). `move_session`의 `Export/Import`는 **프로세스 재기동 + 번들 복사**로 구현하고, `Prepare/Finish/Revoke`는 `subcmd/daemon.rs`의 소켓 핸드오프로 처리한다. senpi 번들 포맷과 JSONL v3의 매핑은 확인 필요.
- 다중 세션 호스트: `cli.rs:177`에 `--multi-session`/`--session-runtime`/`--no-model-fallback`/`--no-recommended-models`/`--listen` 추가. `subcmd/daemon.rs`를 흡수해 한 프로세스가 N개 `Session`을 `HashMap<SessionId, Arc<Runtime>>`로 들고 소켓 하나에서 라우팅한다(`--listen unix://…`).

### 3.3 5단계 — VCS·격리 (선행: 3.2의 `move_session` — 워크트리 이동이 세션 이동과 같은 경로를 쓴다)

- `vendor/omp-vcs`(main `crates/vcs`) 벤더링: `GitRepo::discover`(순수 FS walk), `worktree_add(path,ref,detach,clone,keep_changes)`, `clone_holes`, `seed_worktree_changes`, `detach_git_dir`, `status_summary`, `diff_text`. `features/worktree.rs:123`의 `fn git`과 `:155` `list_worktrees` porcelain 파싱을 제거하고 `sysprompt.rs:103`의 중복 탐색도 같은 `discover`로 통합.
- `vendor/omp-iso` 벤더링: `IsolationBackend` trait + Linux 백엔드(`linux_reflink`, `overlayfs`, `btrfs`, `rcopy`). `omp-sandbox`는 **대체하지 않는다** — `iso`는 파일시스템 CoW, `omp-sandbox`는 Landlock/seccomp 실행 격리로 목적이 다르다(`sandbox.rs:1-15`).
- 고아 워크트리 GC: omp2 `worktree_cmd.rs`의 `.omp-isolation-owner`(PID) 마커를 `crates/omp-cli/src/subcmd/gc.rs`에 결합. `schema.rs:3899-3927`의 `worktree.*` 5키를 실제 소비.
- `TaskSpec`에 격리 필드 추가: 현재 `runner.rs:84-108`에 워크트리 관련 필드가 없다. `TaskSpec.isolation: Option<WorktreeSpec>`을 넣고 `worktree_add(keep_changes=true)`로 자식 cwd를 만든다.

## 4. 구현 단계 (커밋 단위)

3단계
1. `feat(compact): wire /handoff and rpc handoff to omp-compact::handoff` — `slash.rs:3000`, `rpc.rs:1022`, 테스트 `tests/handoff_e2e.rs`.
2. `feat(memory): add omp-memory crate with SQLite+FTS store` — 신규 크레이트, `schema.rs` 17키 소비, 단위 테스트.
3. `feat(urls): register memory:// scheme behind memory.backend` — `omp-urls/src/schemes/memory.rs`, `Registry` 등록.
4. `feat(tools): register recall/retain/reflect/memory_edit/learn` — `tools.rs:260`, `task.rs:34-37` 정리, `tests-tools/ts_tools.json`에서 `NOT_PORTED_YET` 5종 제거.
5. `feat(slash): /memory backed by MemoryStore` — `slash.rs:757-776`.
6. `feat(context): .omp/AGENTS.md priority, RULES.md, user scope` — `sysprompt.rs:161`.
7. `feat(tools): task_send/task_output/task_cancel handle table` — `omp-task::runner`, `tools/task.rs`.
8. `feat(tools): bash_output/kill_bash/monitor over JobRegistry` — `tools/jobs.rs`, `tools/bash.rs`.
9. `feat(eval): promote workpool to a registered tool` — `eval_agents.rs:1759`.

4단계
10. `feat(rpc): add interrupt/set_client_info/get_protocol_info` — `rpc.rs` dispatch.
11. `feat(rpc): add reload/resume_session/release_session/extension_request` — `rpc.rs`, `runtime.rs` 세션 수명.
12. `feat(rpc): emit agent_settled unconditionally next to agent_end` — `ext.rs:1593`, `rpc.rs:1202`.
13. `feat(rpc): sessionId on request/response envelopes` — `rpc.rs:203`.
14. `feat(cli): --multi-session/--session-runtime/--no-model-fallback/--no-recommended-models/--listen` — `cli.rs:97-177`.
15. `feat(cli): multi-session host loop folding subcmd/daemon.rs` — `subcmd/daemon.rs` → `host.rs`.
16. `feat(rpc): move_session prepare/export/import/finish/revoke` — `session_move_wire` 대응, `omp-session` 번들 export/import 신설.
17. `feat(ext): api.rpc.handle on the extension host` — `omp-ext/src/host.rs`, `ext-host/host.ts:1022`.
18. `feat(ext): session_move_export/session_move_import events` — `ext.rs` 이벤트 표.
19. `feat(mcp): cross-session server sharing via daemon` — `omp-mcp/src/manager.rs`, `tools/mcp.rs`.
20. `feat(mcp): OAuth for remote servers` — `omp-mcp` + `omp-auth-broker`.

5단계
21. `refactor(vcs): vendor omp-vcs and replace worktree git() with gix` — `worktree.rs:123`, `sysprompt.rs:103`, grep 게이트 `Command::new("git")` 0.
22. `feat(iso): vendor omp-iso Linux backends behind a feature` — `vendor/omp-iso`, `Cargo.toml`.
23. `feat(worktree): keep_changes carry + isolation owner GC` — `worktree.rs`, `subcmd/gc.rs`.
24. `feat(task): isolation field on TaskSpec` — `omp-task/src/runner.rs:84`.
25. `docs(parity): correct plan §4.2/§4.3/§4.6 against HEAD e7a5378` — `docs/rust-migration-plan.md`(74키→18키, bash in-process, pi-walker 은퇴 반영).

## 5. 테스트

- RPC 계약: `crates/omp-cli/tests/rpc_contract.rs` 신설(현재 `tests/`에는 `login_e2e.rs`, `print_slash.rs`뿐). `agent_settled` 구독을 **먼저** 등록하고 prompt를 보낸 뒤 bounded timeout으로 그 프레임을 기다린다. 고정 sleep 금지. 케이스: 8종 명령 각각의 성공/`session_busy` 오류, `sessionId` 왕복, `get_protocol_info.capabilities`에 `multi_session`·`move_session` 포함.
- handoff: `tests-golden/rs/`에 `handoff.jsonl` 추가, `tests-golden/compare.ts` 비교. `/handoff` 후 세션에 compaction 엔트리가 남고 컨텍스트가 교체됐는지 단정.
- 기억: `omp-memory` 단위 테스트(색인·FTS·만료), `memory.backend=off`일 때 `memory://`가 명시 오류를 내는지, `local`일 때 `/memory stats`가 실데이터를 내는지.
- 도구 계약: `tests-tools/ts_tools.json`에서 5종 메모리 도구가 `NOT_PORTED_YET`에서 빠지고 스키마 diff 0.
- task/bash 비동기: `task_send` → `task_output` 완료 신호를 이벤트 구독으로 대기. `bash_output`은 `JobRegistry` 상태 전이를 관찰.
- 다중 세션: 한 소켓에 두 클라이언트, 서로 다른 `sessionId`로 동시 prompt, 각자 `agent_settled` 수신. 프로세스 1개 확인.
- move_session: Export 후 원본 프로세스 종료 → Import로 새 cwd에서 재개, 트랜스크립트 해시 동일.
- VCS: `worktree_clone_carry` 동등 테스트(더티 파일 이월), `keep_changes=false`일 때 미이월, 고아 GC가 죽은 PID 마커만 지우는지. `cargo test -p omp-cli --features iso` + 기본 빌드 양쪽.
- 회귀 게이트: `just ci`(fmt/clippy/deny/test/tools-parity/golden) + `just ci-all`(PTY TUI).

## 6. QA 시나리오

1. 게이트웨이 대역: `omt --mode rpc --multi-session --listen unix:///tmp/omt.sock --session-runtime in-process`를 띄우고 mhc `maho-rpc` 클라이언트로 `get_protocol_info` → `prompt` → `agent_settled` → `interrupt` → `agent_settled` 순서를 확인.
2. 확장 없이 settle: `~/.omp/agent/extensions`가 빈 상태에서 prompt 1회, `agent_settled` 프레임이 오는지(현재는 오지 않음이 결함).
3. 세션 이동: 세션 A에서 `move_session{op:"export"}` → 프로세스 종료 → `op:"import"` → 같은 트랜스크립트, `cwd` 변경 확인.
4. MCP 공유: 같은 MCP 서버를 쓰는 세션 2개를 띄우고 서버 프로세스가 1개인지(`ps` 카운트), 한쪽 서버 kill 후 다른 쪽 도구가 살아있는지(장애 격리).
5. handoff: 긴 세션에서 `/handoff` → 새 세션이 요약 컨텍스트로 시작, 원본 세션 파일에 compaction 엔트리 1건.
6. 기억: `memory.backend=local`로 `retain` → 프로세스 재시작 → `recall` 히트. `off`로 바꾸면 명시 오류.
7. 워크트리: `/wt run 3 "…"`가 `keep_changes`로 더티 파일을 이월하는지, 중간 kill 후 재실행 시 고아 워크트리가 GC되는지.
8. 격리: `/sandbox on` + `iso` 백엔드 활성 상태에서 쓰기 허용 디렉터리 밖 쓰기 차단(Landlock)과 CoW 클론이 동시에 성립하는지.
9. 설정 루트: `~/.omo` 기반 플릿 프로필로 실행했을 때 `~/.omp`와 충돌하지 않는지 — **이 항목은 설계 미완(아래 §7)**.
10. PTY: `python3 tests-e2e/tui/run.py --client rs` 전 프로브 통과, `tests-e2e/ext/conformance.py` 미설명 diff 0.

## 7. 위험과 미확인

위험
- `agent_settled` 무조건 emit은 `agent_end`와 중복 프레임을 만들 수 있다. `willContinue` 판정(`rpc.rs:1340`)을 settle에도 적용하지 않으면 턴 중간 settle로 게이트웨이가 조기 종료한다.
- `sessionId` 병기 봉투는 두 계약을 동시에 만족시키는 대신 필드 충돌 위험이 있다. mhc `RpcCommand`는 `#[serde(flatten)] body`에 `type` 태그를 쓰므로 tau 응답에 `type:"response"`를 유지하면 게이트웨이 파서가 명령으로 오인할 수 있다.
- `move_session`은 tau의 경로 고정 writer(`slash.rs:2927-2960`)와 정면 충돌한다. 프로세스 재기동 방식은 게이트웨이의 attachment 참조계수와 어긋날 수 있다.
- `vendor/omp-vcs`·`omp-iso`는 아직 vendor에 없고, `iso`의 `btrfs`/`overlayfs` 백엔드는 CI 컨테이너에서 권한이 없어 테스트가 skip될 위험이 있다. skip을 통과로 세지 않도록 별도 표기 필요.
- MCP 데몬 공유는 `omp-mcp`의 per-server 실패 격리(`manager.rs:1-10`)를 데몬 경계로 옮기는 일이라, 데몬 다운이 전 세션 도구 상실로 번질 수 있다.
- `--multi-session` 도입은 `subcmd/daemon.rs`의 "연결=프로세스" 가정을 깨므로 `omp-collab`, `/share`, `omt web`의 세션 조회 경로 회귀 위험이 있다.

확인 필요
- mhc `handoff` 명령의 정확한 wire 형태(`crates/maho-rpc/src/host_handoff*.rs` 미독).
- 게이트웨이가 실제로 엔진을 스폰하는 경로. `maho-gateway`는 `maho-rpc`에 의존하지 않으므로 엔진 기동이 상위(omo-nyang 쪽)에 있을 가능성 — 미확인.
- senpi 세션 번들 포맷과 tau JSONL v3(`crates/omp-session/src/entries.rs`의 `OmpEntry` 16종)의 필드 매핑. `omp-session`에는 번들 export/import API가 없다(`lib.rs` 공개면: `create/create_at/open/append_*/branch_to/reserialize/round_trip`).
- `.github/workflows/ci.yml:43-44`의 `scripts/check-tools-parity.ts`·`scripts/check-golden.ts`가 `tests-tools/compare.ts`·`tests-golden/compare.ts`를 실제로 호출하는지(간접 래퍼 여부).
- `memory` 도구명이 이미 어딘가 등록돼 있는지(tools.rs+tools/*.rs에서 `"memory"` 4히트, 내용 미확인). `memory_apply_patch`는 0히트로 확정.
- 설정 루트 이원화(`~/.omp` vs `~/.omo` + `omo.jsonc` 프로필): tau의 `agent_dir()`(`sysprompt.rs:28`, `features/skills.rs:72`)가 `$OMP_AGENT_DIR`/`~/.omp/agent`만 보므로, `~/.omo` 프로필을 읽게 하는 설계는 이번 조각에서 작성하지 못했다.
- 5범주 중 (5) 설정 루트 항목과 4단계 MCP OAuth 세부는 조사만 하고 설계를 완성하지 못했다. 3·4·5단계 코드 변경은 전부 미착수(설계 문서만).
