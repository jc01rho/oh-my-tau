# RPC `get_protocol_info`·`reload` 명령 추가 설계

## 1. 현재 상태 (근거)

- 워크스페이스 버전은 `Cargo.toml:38`의 `version = "0.12.0"`, 바이너리는 `crates/omp-cli/Cargo.toml:7-9`의 `omt`(`path = "src/main.rs"`)다. 이번 조사 기준 HEAD는 `e7a5378`이다.
- RPC 명령 디스패치는 `crates/omp-cli/src/rpc.rs:593`의 `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` 하나의 `match kind`이며, 마지막 팔이 `rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`다. `get_protocol_info`와 `reload`는 이 match에 팔이 없어 그대로 `Unknown command`로 떨어진다.
- 응답 봉투는 `rpc.rs:203`의 `fn response(id: Option<&Value>, command: &str, result: CmdResult) -> Value`가 만들고 `{id?, type:"response", command, success, data?|error?, code?}` 형태다(`rpc.rs:1-5` 문서 주석). 성공 헬퍼는 `ok()`(`rpc.rs:435`), 데이터 없는 성공은 `none()`(`rpc.rs:440`), 오류는 `CmdError`(`crates/omp-cli/src/runtime.rs:34-39`, `code: Option<&'static str>`)와 `CmdResult = Result<Option<Value>, CmdError>`(`runtime.rs:63`)다. 미지원 기능용 `unsupported()`는 `runtime.rs:4534`에 있다.
- 프로토콜 협상은 `rpc.rs:595-601`이다. `protocolVersion == 2`만 받고 `{"protocolVersion": 2}`를 돌려주며, 그 밖의 값은 `Unsupported RPC protocol version: ...`로 거절한다.
- 시작 프레임은 `rpc.rs:1420-1426`의 `{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":...,"maxReassembledFrameBytes":...}`다. 여기의 `protocolVersion`은 **프레임 인코딩** 버전이다. `FrameEncoder`(`rpc.rs:1227-1351`)가 `version: u8`(`rpc.rs:1228`, 초기값 1 `rpc.rs:1236`)을 들고 있다가, 성공한 `negotiate_protocol` 응답을 관찰하면 2로 올린다(`rpc.rs:1345-1349`). 즉 v2는 초과 프레임을 `rpc_chunk`로 쪼개는 전송 계층 기능이다.
- `Runtime`은 `runtime.rs:116`에 정의되고 `ext_host: Mutex<Option<Arc<omp_ext::ExtHost>>>`(`runtime.rs:180`)를 들고 있으며 접근자는 `ext_host()`/`set_ext_host()`(`runtime.rs:2583-2607`)다. 슬래시 명령 카탈로그는 `announce_commands()`(`runtime.rs:3739-3744`), `set_available_commands()`(`runtime.rs:3731-3737`), `available_commands()`(`runtime.rs:3749-3754`)로 관리한다.
- 설정은 `omp_settings::Settings::load(cwd)`(`crates/omp-settings/src/settings.rs:108-112`)와 `load_from(global_yaml, project_dir)`(`settings.rs:115`)로 매 호출 새로 읽는다. 전역 경로는 `agent_config_path()`(`settings.rs:473-482`), 에이전트 디렉터리는 `omp_providers::AgentDir::default()`(`crates/omp-providers/src/lib.rs:104-114`, `$OMP_AGENT_DIR` 또는 `~/.omp/agent`)다.
- `Runtime::start`는 기동 시 설정을 한 번 스냅샷한다: `crate::compact::load_settings()`(`runtime.rs:436`), `omp_settings::Settings::load(&cwd)`로 todo 리마인더 설정(`runtime.rs:437`). 압축 설정 반영 경로는 `set_compaction_settings()`(`runtime.rs:3012-3015`)로 이미 존재한다.
- 도구 목록 전면 재구성의 기존 선례는 모델 전환 경로 `runtime.rs:3426-3461`이다. `session_builtin_tools_with_env_and_jobs()`(`crates/omp-cli/src/tools.rs:466`)로 목록을 만들고 `mcp_tools`/`wasm_tools`/`hidden_tools`를 더한 뒤 `env.set_dyn_tools`, `crate::tools::set_session_env`(`tools.rs:88`), `eval_bridge::set_session_tools`, `agent.set_tools`까지 수행한다. `republish_dyn_tools()`는 `tools.rs:107`에 있다.
- 확장 호스트는 `Extensions`(`crates/omp-cli/src/ext.rs:486-515`)가 `pub host: Arc<ExtHost>`(교체 불가 필드), `hooks`, `dir`, `extra`, `mode`, `rt`를 들고 있다. 기동은 `Extensions::start()`(`ext.rs:624-690`)가 `host_script()`(`ext.rs:113`)로 스크립트를 찾고 `HostConfig::new`(`crates/omp-ext/src/host.rs:89`)를 만든 뒤 `host.load(paths, dir, cwd, ctx)`를 호출한다. 기본 확장 디렉터리는 `~/.omp/agent/extensions`(`ext.rs:636-641`)다.
- 배선은 `attach_session()`(`ext.rs:1054-1103`)이 담당한다: `set_mode`(`ext.rs:966`), `rt.set_ext_host`, `attach`(`ext.rs:902`), `slash::set_extensions`, `set_name_listener`, `set_switch_listener`, `set_switch_gate`, `push_session_ctx`(`ext.rs:1162-1184`), `session_start`(`ext.rs:826-828`). RPC 모드 진입 훅은 `rpc_on_start()`(`ext.rs:1201-1207`)이고 `main.rs:555`가 `rpc::run_with(..., Some(e.rpc_on_start()))`로 넘긴다.
- 호스트 쪽 `load`는 `crates/omp-ext/src/host.rs:477-517`이 `request("load", p)`를 보내고 `LoadResult`(`host.rs:127-132`)를 받아 구독 집합을 갱신한다. `ExtHost::shutdown()`은 `host.rs:696-717`, 스폰은 `host.rs:342`다.
- 확장 재적재 선례는 `Extensions::recover()`(`ext.rs:1014-1040`)다. 호스트 재연결 시 `host.load` 재호출, `send_ctx`, `session_start` 재방출을 한다(`ext.rs:997-1008` 감시 루프).
- tau에는 `reload` 관련 RPC·확장 이벤트가 없다. `crates/` 전체에서 `session_before_reload` 문자열은 0건이고, `slash.rs`의 `reload`는 MCP 런타임 안내 문구(`slash.rs:2629-2630`)와 플러그인 안내(`slash.rs:778-779`)뿐이다. `LifecycleEvent::SessionShutdown`(`crates/omp-ext/src/lifecycle.rs:307`, 생성자 `lifecycle.rs:502`)에는 사유 필드가 없다.
- 게이트웨이가 기다리는 종료 신호 `agent_settled`는 확장 이벤트로만 나간다(`ext.rs:1593-1598`).

## 2. 게이트웨이가 요구하는 계약

- mhc의 응답 데이터는 `protocolVersion`, `serverVersion`, `capabilities`, `mode`를 담고(`crates/maho-rpc/src/connection_handler.rs:1598-1616`), 여기에 `instanceId`/`engineVersion`/`engineOrdinal`/`launch_profile`/`agentDir`가 얹힌다(`crates/maho-rpc/src/protocol_identity.rs:150-157`). 파서 `parse_host_protocol_info`는 `serverVersion`(문자열)과 `capabilities`(문자열 배열)를 **필수**로 요구한다(`crates/maho-rpc/src/host_protocol_info.rs:49-57`).
- 호스트 프로토콜 버전 상수는 `HOST_PROTOCOL_VERSION: u32 = 1`(`crates/maho-rpc/src/host_decision.rs:8`)이고, 판정은 호스트가 보고한 `protocol_version`과 클라이언트 값을 `total_cmp`로 비교한다(`host_decision.rs:248-249`).
- 호스트로 인정받는 필수 capability는 `multi_session`, `extension_events`, `session_context`, `session_kind`(`host_decision.rs:11-16`)다.
- 게이트웨이는 `get_protocol_info`의 `data.commands` 배열로 TUI 엔드포인트가 받아들일 제어를 계산한다(`crates/omo/components/maho-omo-gateway/src/thread/endpoint_controls.rs:47-60`). 없으면 `LEGACY_TUI_COMMANDS`(`endpoint_controls.rs:24-33`)로 폴백해 send/read/rename만 허용한다. 제어별 요구 명령은 `control_commands()`(`endpoint_controls.rs:36-45`)에 있다: `interrupt`는 `"interrupt"`, `set_model`은 `get_available_models`+`set_model`.
- 호출 형태는 `{"type":"get_protocol_info","observe":true}`(`live_surface.rs:687`, `close_ladder.rs:141`)이고, 응답의 `capabilities`에서 `unload_refuse_deliveries`를 찾는다(`close_ladder.rs:141-152`). `get_protocol_info`와 `check_reload_veto`는 세션 첨부 없이 보낼 수 있는 예외 목록에 있다(`attachment_fence.rs:16-54`).
- `reload`는 mhc에서 `RpcCommandBody::Reload => session.reload()`로 처리되고 응답 데이터는 `{cancelled, reason?}`이다(`connection_handler.rs:1247`, 직렬화는 `connection_handler.rs:1020-1026`). veto는 프롬프트 승인 중·세션 시작 중·`session_before_reload` 핸들러 취소의 세 갈래다(`crates/maho-core/src/reload_veto.rs:45-70`). 실제 재적재는 `agent_session.rs:14331-14337`의 admission 락 + veto + `rebuild_runtime_for_reload`, 본체는 `agent_session.rs:14352-14395`(shutdown(Reload) → invalidate → `settings_manager.reload()` → `model_runtime.reload_config()` → 리소스 재로드 → 시스템 프롬프트 재구성 → 확장 런타임 갱신)다. 클라이언트는 `{"type":"reload"}`(`rpc_client.rs:1644`)를 보낸다.

## 3. 설계

### 3.1 `get_protocol_info`

- 위치: `rpc.rs`의 `dispatch`에 `"get_protocol_info" => protocol_info(rt),` 팔을 `rpc.rs:1094` 앞에 넣는다. 구현은 `rpc.rs`가 이미 `#[path = "slash.rs"] pub mod slash;`(`rpc.rs:46-47`)로 파일 분할하는 관례를 따서 `crates/omp-cli/src/rpc/protocol_info.rs`를 새 모듈로 두고 `#[path = "protocol_info.rs"] pub mod protocol_info;`를 추가한다.
- 응답 타입은 `serde` 구조체가 아니라 `serde_json::json!` 값으로 만든다. `response()`가 이미 `Value`를 받고, mhc 파서도 `Value` 기반(`host_protocol_info.rs:49`)이라 구조체를 하나 더 둘 이유가 없다. 함수 시그니처는 `pub fn protocol_info(rt: &Runtime) -> Value`.
- 필드:

| 필드 | 값 | 근거 |
|---|---|---|
| `protocolVersion` | `1` (u32) | mhc `HOST_PROTOCOL_VERSION`과 맞춤 |
| `serverVersion` | `env!("CARGO_PKG_VERSION")` = `0.12.0` | `Cargo.toml:38`, mhc 필수 필드 |
| `engine` | `"omt"` | `crates/omp-cli/Cargo.toml:7-9` |
| `mode` | `"classic"` | mhc의 단일 세션 바인딩 표기(`connection_handler.rs:1613`) |
| `capabilities` | 정적 배열 | 아래 |
| `commands` | `RPC_COMMANDS` | 아래 3.2 |
| `agentDir` | `omp_providers::AgentDir::default().0` | `omp-providers/src/lib.rs:104-114` |
| `sessionId`/`sessionFile` | `rt.session_id()`, `rt.session_file()` | `runtime.rs:1921`, `runtime.rs:1934` |

- `capabilities`는 **정직하게** 광고한다. tau가 실제로 가진 것만 넣는다: `extension_events`(확장 이벤트 전달 `ext.rs:1560-1660`), `session_context`(확장 ctx 푸시 `ext.rs:1162`), `frame_chunking`(v2 `rpc_chunk`, `rpc.rs:1309-1330`). `multi_session`은 넣지 않는다. `crates/omp-cli/src/cli.rs:22-60`의 `Args` 필드에 multi-session 류가 없고 `rpc.rs`에 `open_session`/`list_sessions` 팔이 없다.
- `observe: true`는 무시한다. tau의 `dispatch`는 미지 필드를 검증하지 않으므로 추가 코드가 필요 없고, 부수효과(세션 첨부·이벤트 방출)도 없다. 이 무부수효과 성질을 테스트로 고정한다.
- `launch_profile`은 넣지 않는다. 넣으려면 `--multi-session`/`--session-runtime` 플래그가 먼저 있어야 하고(현재 없음, `cli.rs:22-60`), mhc 파서는 `launch_profile`이 있으면 `profile_id`/`core.extensions`/`multi_session`/`session_runtime`을 전부 요구해 하나라도 빠지면 `None`이 된다(`host_protocol_info.rs:64-88`). 반쪽 프로필을 싣는 것보다 생략이 안전하다.

### 3.2 지원 명령 목록: 손 관리 + 드리프트 테스트

- 자동 생성은 불가능하다. 디스패치가 `match kind`(`rpc.rs:593-1094`)라 런타임 리플렉션이 없고, 팔마다 시그니처가 다른 async 본문이라 이름만 뽑아낼 방법이 없다.
- 전면 재구성(핸들러 테이블)은 이 조각의 범위를 넘는다. `pa_agent::BoxFut`(`rpc.rs:31`)로 `fn(&Arc<Runtime>, &Value) -> BoxFut<'_, CmdResult>` 테이블을 만들 수는 있지만 팔이 수십 개라 회귀 위험이 크다.
- 결정: `rpc.rs`에 `pub const RPC_COMMANDS: &[&str]`를 두고 **손으로 관리**하되, 테스트 두 개로 드리프트를 막는다.
  1. `advertised_commands_dispatch`: `RPC_COMMANDS`의 모든 이름에 대해 최소 프레임 `{"id":"t","type":name}`을 `dispatch`에 넣고, 결과가 `Unknown command: ...`가 아님을 확인한다. 인자 누락으로 인한 `must be a string` 류 오류는 통과로 본다.
  2. `gateway_gated_commands_are_advertised`: 게이트웨이가 제어 판정에 쓰는 이름 집합(`wake`, `get_messages`, `set_session_name`, `get_available_models`, `set_model`, `get_available_thinking_levels`, `set_thinking_level`, `interrupt` — `endpoint_controls.rs:36-45`) 중 tau가 실제로 구현한 것만 `RPC_COMMANDS`에 있음을 확인한다.
- `commands`에는 `get_protocol_info`와 `reload` 자신도 넣는다. 게이트웨이 폴백 목록이 `get_protocol_info`를 포함하므로(`endpoint_controls.rs:24-33`) 자기 자신을 빠뜨리면 모순된 광고가 된다.

### 3.3 `reload`

- 시그니처: `"reload" => reload(rt).await,` → `async fn reload(rt: &Arc<Runtime>) -> CmdResult`.
- 응답: 성공 `ok(json!({"cancelled": false, "settings": {...}, "extensions": {"loaded": n, "errors": [...]}}))`, 보류 `ok(json!({"cancelled": true, "reason": "..."}))`. mhc 클라이언트는 `cancelled`만 읽으므로(`connection_handler.rs:1020-1026`) 추가 필드는 순수 확장이다. 오류는 `CmdError::coded(msg, "reload_failed")`로 코드를 붙인다(`runtime.rs:34-39`).
- 1단계 보류 판정. mhc의 세 갈래(`reload_veto.rs:45-70`) 중 tau에 대응물이 있는 것만 쓴다.
  - 진행 중 턴: `rt.agent.is_streaming().await`(사용 선례 `ext.rs:1110`) → `{"cancelled": true, "reason": "A turn is in progress."}`
  - 대기 메시지: `rt.has_pending_messages()`(`runtime.rs:2022`) → `{"cancelled": true, "reason": "Queued messages are pending."}`
  - 확장 veto(`session_before_reload`)는 tau에 이벤트가 없다. 1차에서는 넣지 않고, 필요해지면 `LifecycleEvent`에 변형을 추가하고 `has_handlers` 게이트(`host.rs:575-590`)로 호출하는 2차 작업으로 분리한다.
- 2단계 설정 재적용. `omp_settings::Settings::load(&rt.cwd)`(`settings.rs:108`)를 새로 읽어 기동 시 스냅샷한 값과 교체한다.
  - 압축: `crate::compact::load_settings()`(`runtime.rs:436`과 동일 경로) → `rt.set_compaction_settings(...)`(`runtime.rs:3012-3015`). 이 경로는 이미 공개 API라 추가 배선이 없다.
  - todo 리마인더: 현재 `Settings` 구조체 필드(`runtime.rs:99-113`)에 공개 setter가 없다. `Runtime`에 `apply_todo_settings(&self, &omp_settings::Settings)`를 추가해 `todo_reminders_enabled`/`todo_reminders_max`를 갱신한다. 초기값 계산식(`runtime.rs:437`, `reminders_max`)을 그대로 재사용해 기동과 재적재가 같은 규칙을 쓰게 한다.
- 3단계 도구 재구성. `runtime.rs:3426-3461` 블록을 `Runtime::rebuild_tools(&self, reason: &'static str)`로 추출해 모델 전환과 reload가 같은 함수를 부른다. 추출 시 동작 변화는 없어야 하며, `set_dyn_tools`/`set_session_env`/`eval_bridge::set_session_tools`/`agent.set_tools` 순서를 그대로 유지한다(`tools.rs:88`, `tools.rs:107`).
- 4단계 확장 재적재. **같은 호스트에 `load`를 다시 보내면 안 된다.** `ext-host/host.ts:82`의 `const exts: Ext[] = []`는 모듈 전역이고 `load()`는 `host.ts:521`에서 `exts.push(ext)`만 한다. 초기화 경로가 없어 재호출은 핸들러·도구·명령을 중복 등록한다. 따라서 호스트를 새로 띄운다.
  - 순서는 mhc를 따른다(`agent_session.rs:14352-14395`): `session_shutdown` 방출 → 기존 호스트 `shutdown()`(`ext.rs:1209-1225`, 내부 `host.shutdown()` `host.rs:696-717`) → `Extensions::start(&rt.cwd, &extra)`(`ext.rs:624`) → `attach_session(new, ExtMode::Rpc)`(`ext.rs:1054`).
  - 전제 변경: `slash::EXTENSIONS`가 `OnceLock<Arc<Ext>>`(`slash.rs:68-71`)라 한 번 꽂으면 교체가 불가능하다. `RwLock<Option<Arc<Ext>>>`로 바꾸고 `set_extensions`가 덮어쓰게 한다. `extensions()`(`slash.rs:75`)와 호출부는 반환형을 유지한다.
  - `Extensions`의 `host` 필드가 `pub host: Arc<ExtHost>`(`ext.rs:487`)라 인스턴스 교체가 유일한 길이다. `attach_session`이 `rt.set_ext_host`(`runtime.rs:2588`)와 훅 재설치(compaction/before_start/switch/name)를 모두 하므로, 새 인스턴스에 그대로 다시 호출하면 배선이 새 호스트를 가리킨다.
  - 마지막에 `rt.announce_commands()`(`runtime.rs:3739`)로 슬래시 카탈로그 변경을 클라이언트에 알린다. 확장 명령이 바뀌면 `get_available_commands`(`rpc.rs:786`)도 새 목록을 본다.
- 5단계 응답 조립. `extensions.loaded.len()`과 `errors`를 그대로 싣고(`host.rs:127-137`), 실패한 확장은 `eprintln!`만 하던 기존 동작(`ext.rs:652-654`)에 더해 응답에도 담는다.
- 동시성: RPC 명령은 워커 태스크에서 하나씩 순서대로 처리되므로(`rpc.rs:1436-1444`) `reload` 자체의 상호 배제는 이미 성립한다. 별도 admission 락(mhc의 `reload_admission`, `agent_session.rs:14331`)은 프롬프트 경로와 경합할 때만 필요하고, 1차에서는 스트리밍 보류 판정으로 대신한다.
- `agent_settled`(`ext.rs:1593-1598`)는 reload로 건드리지 않는다. 재적재 후에도 `has_handlers` 게이트(`host.rs:575-590`)를 통해 새 호스트 기준으로 다시 계산된다.

## 4. 구현 단계 (커밋 단위)

1. `feat(rpc): add get_protocol_info` — `crates/omp-cli/src/rpc/protocol_info.rs` 신규(`protocol_info()`, `capabilities()`, `RPC_COMMANDS`), `rpc.rs`에 `pub mod protocol_info`와 디스패치 팔. 단위 테스트 포함.
2. `test(rpc): guard the advertised command list` — `advertised_commands_dispatch`, `gateway_gated_commands_are_advertised` 두 테스트만 추가. 코드 변경 없음.
3. `refactor(runtime): extract rebuild_tools` — `runtime.rs:3426-3461`을 `Runtime::rebuild_tools()`로 추출, 모델 전환 호출부를 교체. 동작 무변경, 기존 테스트 통과가 완료 조건.
4. `refactor(slash): make the extension registry replaceable` — `slash.rs:68-76`의 `OnceLock`을 `RwLock<Option<...>>`로, `set_extensions`가 교체하도록. 호출부 시그니처 유지.
5. `feat(rpc): add reload` — `reload()` 구현, `Runtime::apply_todo_settings()` 추가, 디스패치 팔, `RPC_COMMANDS`에 `"reload"` 추가. 응답 형태 테스트.
6. `test(rpc): reload veto and effect` — 스트리밍/대기 보류, 설정 변경 반영, 확장 재적재 결과 검증.

## 5. 테스트

- `protocol_info_reports_identity`: `dispatch`에 `{"type":"get_protocol_info"}`를 넣어 `success == true`, `data.serverVersion == env!("CARGO_PKG_VERSION")`, `data.protocolVersion == 1`, `data.capabilities`가 문자열 배열임을 확인한다. mhc 파서가 요구하는 두 필수 필드(`host_protocol_info.rs:49-57`)를 직접 겨냥한다.
- `protocol_info_ignores_observe`: `{"observe":true}`를 붙여도 같은 값이 나오고, 반환 프레임이 `response` 하나임을 확인한다. 부수효과 없음의 회귀 방지.
- `advertised_commands_dispatch`: `RPC_COMMANDS` 전수에 대해 `Unknown command`가 아님을 확인한다.
- `gateway_gated_commands_are_advertised`: `endpoint_controls.rs:36-45`의 이름 집합과 `RPC_COMMANDS`의 교집합이 의도한 목록과 같은지 확인한다. tau가 `interrupt`를 구현하지 않으므로 이 테스트는 "광고하지 않음"을 고정한다.
- `reload_refuses_while_streaming`: 진행 중 턴을 만들 수 있는 기존 테스트 하네스 재사용이 필요하다. `runtime.rs`의 테스트 모듈에 `Runtime`을 세우는 헬퍼가 있는지 **확인 필요**. 없으면 `reload` 본체를 `fn reload_decision(streaming: bool, pending: bool) -> Option<Value>` 같은 순수 함수로 분리해 판정만 단위 테스트하고, 배선은 QA로 덮는다.
- `reload_reapplies_compaction_settings`: `crate::compact::load_settings_from(dir)`(`compact.rs:85`)가 이미 존재하므로, 설정 소스를 `AgentDir::default()`가 아니라 주입 가능한 경로로 받는 시임을 하나 둔다. 임시 디렉터리에 `config.yml`을 쓰고 `compaction.enabled`를 뒤집은 뒤 reload 후 `Runtime`의 압축 설정이 바뀌었는지 확인한다. 프로세스 전역 `OMP_AGENT_DIR`(`omp-providers/src/lib.rs:108`)를 테스트에서 만지지 않는다 — 병렬 테스트에서 경합한다.
- `reload_reports_extension_errors`: 확장 디렉터리에 문법 오류 `.ts`를 두고 reload 응답의 `extensions.errors`가 비어 있지 않고, 세션은 계속 `get_state`에 응답함을 확인한다. bun이 필요한 테스트라 `tests-e2e/ext/conformance.py` 계열로 분리한다.
- `reload_keeps_session`: reload 전후 `session_id`/`session_file`(`runtime.rs:1921`, `runtime.rs:1934`)이 같고 `get_messages`가 유지됨을 확인한다.

## 6. QA 시나리오

1. `printf '%s\n' '{"id":"1","type":"get_protocol_info"}' | omt --mode rpc --no-extensions` → `response` 한 줄, `data.serverVersion == "0.12.0"`, `data.commands`에 `get_protocol_info`와 `reload` 포함.
2. 같은 입력에 `"observe":true` 추가 → 1번과 동일한 데이터, 추가 이벤트 프레임 없음.
3. `negotiate_protocol` 후 `get_protocol_info` → `data.protocolVersion`은 여전히 `1`. 프레임 인코딩만 v2로 바뀌고 호스트 프로토콜 버전은 불변임을 확인(두 `protocolVersion`의 의미가 다르다).
4. `reload`(유휴) → `{"cancelled":false,...}`. 이어서 `get_available_commands`가 정상 응답.
5. `config.yml`의 `compaction.enabled`를 뒤집고 `reload` → `get_state` 및 압축 동작이 새 값을 따름.
6. 프롬프트 진행 중 `reload` → `{"cancelled":true,"reason":...}`, 턴은 중단되지 않고 정상 종료.
7. 확장 1개를 깨뜨린 뒤 `reload` → `cancelled:false`, `extensions.errors` 1건, `agent_settled`(`ext.rs:1593`)는 턴 종료 시 여전히 1회.
8. `reload` 2회 연속 → 두 번째도 `cancelled:false`이고 확장 도구가 중복 등록되지 않음(`get_state`의 도구 목록 또는 `get_available_commands`로 확인). `host.ts:521` 중복 등록 회귀의 핵심 시나리오다.
9. 게이트웨이 경유 기동(센피 태스크 rpc 프로세스) 후 `get_protocol_info` 응답으로 제어 목록이 계산되는지 확인. 게이트웨이 쪽 소비 코드는 **확인 필요**.

## 7. 위험과 미확인

- **게이트웨이가 `reload`를 실제로 보내는지 확인하지 못했다.** `crates/omo/components/maho-omo-gateway/src` 전체에서 `reload`는 `check_reload_veto` 한 줄(`attachment_fence.rs:51`)뿐이고, `reload` 전송 지점은 찾지 못했다. 보류 해제 시나리오의 실제 트리거는 **확인 필요**.
- **정직한 `capabilities`는 게이트웨이의 호스트 필수 조건을 만족하지 못한다.** `multi_session`을 광고할 수 없으므로(`cli.rs:22-60`에 플래그 없음, `open_session`/`list_sessions` 팔 없음) `host_decision.rs:11-16` 판정에서 거절되거나 폴백으로 떨어진다. tau를 호스트가 아니라 엔진 자식 프로세스로만 쓸 때는 영향이 없지만, 이 경계는 **확인 필요**.
- **`commands`를 정직하게 광고하면 게이트웨이 제어가 줄어든다.** `interrupt`가 없으므로 TUI 엔드포인트로 취급되면 interrupt/set_model 제어가 거절된다(`endpoint_controls.rs:36-45`). tau가 `RpcHost` 종류로 등록되는지에 따라 결과가 갈리며 이는 **확인 필요**.
- **`protocolVersion` 이름 충돌.** tau의 `ready.protocolVersion`은 프레임 인코딩 버전(`rpc.rs:1420-1426`, `FrameEncoder` `rpc.rs:1227-1351`)이고 mhc의 `protocolVersion`은 호스트 프로토콜 버전(`host_decision.rs:8`)이다. 지금은 둘 다 1이라 충돌이 드러나지 않지만, 한쪽이 2로 올라가면 게이트웨이가 오판할 수 있다. `get_protocol_info`에 `frameProtocolVersion` 같은 별도 키를 두는 방안을 검토해야 한다.
- **확장 호스트 in-place 재적재는 불가능하다.** `ext-host/host.ts:82`의 전역 `exts` 배열에 초기화 경로가 없고 `host.ts:521`이 push만 한다. 재적재는 호스트 재기동이어야 하며, 이는 진행 중인 확장 상태(플래그·provider 등록)를 전부 버린다. `host.ts`에 unload RPC를 추가하는 대안은 벤더 스크립트 계약을 바꾸므로 2차로 미룬다.
- **`slash::EXTENSIONS` 교체는 필수 선행 변경이다**(`slash.rs:68-71`). 이를 빼면 reload 후 슬래시 디스패치가 죽은 호스트를 가리킨다.
- **`Extensions::shutdown()`이 RPC 모드에서 `agent_end`를 중복 방출할 위험.** `deferred_agent_end`(`ext.rs:514`)를 꺼내 방출하는 경로가 `ext.rs:1211-1220`에 있고, 이 값이 RPC 모드에서도 채워지는지 **확인 필요**. 채워진다면 reload가 게이트웨이에 가짜 종료 신호를 보낸다.
- **`SessionShutdown`에 사유 필드가 없다**(`lifecycle.rs:307`, `lifecycle.rs:502`). mhc는 `SessionReason::Reload`를 구분하므로(`maho-ext-api/src/types.rs:864`), 확장이 reload를 구분해야 한다면 `LifecycleEvent` 확장이 필요하다. 1차에서는 미구현.
- **reload와 압축 가드의 상호작용 미확인.** `compaction_guard`/`compaction_done`(`runtime.rs:143-152`)이 도는 중 reload가 도구 목록을 갈아끼우면 진행 중 압축이 옛 도구 집합을 볼 수 있다. 스트리밍 보류 판정이 압축 중 상태까지 막는지 **확인 필요**.
- **`Runtime::rebuild_tools` 추출의 회귀 위험.** `runtime.rs:3426-3461`은 `env`/`jobs`/`eval_bridge`/`Gate`를 한 덩어리로 다룬다. 순서를 바꾸면 `/jobs`, `proc://`, eval 브리지가 옛 목록을 본다(주석 `runtime.rs:3430-3436`). 추출 커밋은 동작 무변경을 테스트로 고정한 뒤에만 진행한다.
- **디스패치 팔의 정확한 개수는 확정하지 못했다.** `rpc.rs:593-1094` 범위를 awk로 센 값이 57, 파일 전체 `grep -c`가 72로 서로 달랐다. `RPC_COMMANDS` 초기 목록은 이 숫자가 아니라 이름 전수 조사로 채워야 한다. **확인 필요**.
- **`reload` 응답의 추가 필드(`settings`, `extensions`)를 게이트웨이가 문제없이 무시하는지 확인 필요.** mhc 클라이언트는 `cancelled`만 읽지만(`connection_handler.rs:1020-1026`), 게이트웨이의 응답 파서가 닫힌 스키마인지 확인하지 못했다.
- **미완료 항목**: 게이트웨이의 센피 태스크 rpc 프로세스가 이 두 명령을 어떻게 소비하는지(`crates/omo/senpi-task/src/runners/rpc_process.rs`) 읽지 못했다. `get_protocol_info`의 `commands`를 실제로 읽는지, `reload`를 보내는지 이 파일에서 확인해야 한다.
