# CLI 플래그 `--multi-session`·`--session-runtime in-process` 설계

## 1. 현재 상태 (근거)

- 플래그 테이블은 세 배열뿐이다: `STRING_FLAGS`(crates/omp-cli/src/cli.rs:97), `OPTIONAL_FLAGS`(:138), `VALUELESS_FLAGS`(:142). `--multi-session`·`--session-runtime`·`--listen`은 세 배열 어디에도 없고, `set_string`(:233)의 match에도 없어 `other => bail!("unknown flag: {other}")`로 떨어진다(cli.rs:433, :512-516).
- `Args` 구조체(cli.rs:22)에 세션 호스트 관련 필드가 없다. `--mode`는 `MODE_VALUES`(cli.rs:177)로 text/json/rpc/acp/rpc-ui만 허용하고(cli.rs:238-248), 값 검증 실패는 `invalid` 벡터에 모여 `finish`(:456)에서 한 번에 보고된다. 새 열거형 값 검증도 이 경로를 그대로 쓸 수 있다.
- 진입점은 `main`(crates/omp-cli/src/main.rs:249) → `async_main`(:281) 하나뿐이고, `--mode`가 있으면 current-thread 런타임을 고른다(main.rs:263-278, `sequential` 판정 :265-267).
- 세션 하나를 만드는 순서가 `async_main` 안에 직선으로 박혀 있다: cwd 확정(:335-348) → 확장 호스트 기동(:438-455) → `context::set_active`(:465) → `approval::Gate::install`(:472) → `agent::build_agent`(:478) → eval 브리지 주입(:488-489) → `persist::attach_with`(:491) → `omp_urls::set_global_session_file`(:494-496) → `Extensions::set_session_ctx`(:497-506) → resume 대상 해석(:510-537) → rpc 분기(:545-562).
- rpc 분기는 `rpc::run_with(agent, target, cwd, persistence, Some(e.rpc_on_start()))`(main.rs:554-557) 한 줄이고, 루프는 `stdin`을 직접 읽는다(rpc.rs:1434, :1452). 프레임 라우팅 키가 없다: `response()`(rpc.rs:203-224)는 id/type/command만 넣고 `sessionId`를 붙이지 않는다. 유일하게 `sessionId`가 나오는 곳은 `open_session` 응답 데이터(rpc.rs:742)와 `get_state`(runtime.rs:1967)다.
- 디스패치 미지원 분기는 `other => Err(CmdError::new(format!("Unknown command: {other}")))`(rpc.rs:1094) 하나다.
- 세션 상태는 `Runtime`(crates/omp-cli/src/runtime.rs:116)에 있고 `cwd`, `persist`, `out: UnboundedSender<Value>`를 필드로 갖는다. 즉 프레임 스트림 자체는 이미 세션 단위로 분리 가능한 형태다(runtime.rs:589 `emit_event` → `self.send`).
- 그러나 프로세스 전역 싱글턴이 다수 남아 있다: `approval::GATE`(crates/omp-cli/src/approval.rs:837, `install` :841이 `OnceLock::set`), `context::ACTIVE`(crates/omp-cli/src/context.rs:36, `set_active` :40), `slash::EXTENSIONS`(crates/omp-cli/src/slash.rs:65, :71), `tools::SESSION_JOBS`/`SESSION_ENV`(crates/omp-cli/src/tools.rs:54, :58), `eval_bridge::SESSION_TOOLS`(crates/omp-cli/src/tools/eval_bridge.rs:51), `eval_agents::HOOKS`(crates/omp-cli/src/tools/eval_agents.rs:89), `omp_urls::SESSION_FILE`(crates/omp-urls/src/lib.rs:54), `rpc::RUNTIME_TX` thread_local(crates/omp-cli/src/rpc.rs:137-141), `runtime::MODEL_PINNED`/`THINKING_PINNED`/`LAUNCH_FALLBACKS`(runtime.rs:4197-4211).
- 설정 루트는 `~/.omp/agent`로 고정이다(crates/omp-settings/src/settings.rs:461-471 `agent_dir`). `PI_CONFIG_DIR`를 읽는 곳은 omp-stats 쪽(crates/omp-stats/src/paths/mod.rs:20-27)뿐이고 omp-settings는 읽지 않는다. 확장 디렉터리도 `~/.omp/agent/extensions`로 두 곳에 하드코딩돼 있다(crates/omp-cli/src/ext.rs:91, :638). 세션 저장소는 `~/.omp-rs/sessions/<slug>`(crates/omp-cli/src/persist.rs:63-67).
- 확장 호스트는 이미 세션 단위로 띄울 수 있다: `Extensions::start(cwd, extra)`(ext.rs:624)가 `cwd`를 받아 `load`에 넘기고, 공유 데몬 모드에서도 세션마다 별도 bun Worker를 쓴다(ext.rs:630-633 주석, `shared_socket_path` :156-170). `attach_session`(:1054)과 `shutdown`(:1209)도 인스턴스 메서드다.
- 기존에 프로세스 하나가 여러 RPC 세션을 받는 구현은 `omt daemon` 서브커맨드다(crates/omp-cli/src/subcmd/daemon.rs:1-14): 연결마다 현재 바이너리를 `--mode rpc`로 spawn하고 소켓 바이트를 파이프한다(:378-379). in-process가 아니라 프로세스 격리 방식이다.

## 2. 게이트웨이가 요구하는 계약

mhc(게이트웨이가 현재 쓰는 엔진)의 실물 계약은 다음과 같다.

- 플래그: `--multi-session`은 값 없는 불리언(crates/maho-cli/src/cli/args.rs:74, :193), `--session-runtime`은 `in-process`/`worker` 두 값만 받고 그 외는 진단으로 거절한다(maho-cli/src/cli/args.rs:261-268). 기본값 결정은 `resolve_session_runtime`(:100-107): 명시값 우선, 소켓 호스트(`--listen` != `stdio://`)면 `InProcess`, 그 외(stdio)는 `Worker`.
- 분기: `mode == AppMode::Rpc && parsed.multi_session`일 때만 다중 세션 호스트로 간다(crates/maho-cli/src/cli/runtime.rs:190-215). 그 경로는 `serve_multi_session_host(HostCore, listen)`이고, `Worker`면 `current_exe()`를 세션 워커 엔트리로 재실행한다.
- 런치 프로파일: `RpcLaunchProfileCore { extensions, multi_session, session_runtime }`(crates/maho-rpc/src/host_protocol_info.rs:13-19)가 `protocol_info` 응답의 `launch_profile`에 실린다. `multi_session`은 항상 true여야 하고, 스펙에서 false면 거절된다(crates/maho-rpc/src/host_launch_spec.rs:44-50, :96-101).
- 프레임 라우팅: 모든 응답/이벤트에 `sessionId`가 실린다(crates/maho-rpc/src/connection_handler.rs:455-500). 부착 계열 명령 이름은 `attach_session`/`release_attachment`/`reclaim_session`/`list_sessions`(crates/maho-rpc/src/connection_handler.rs:1636-1638, crates/maho-rpc/src/session_attachment_commands.rs)이다.
- 종료 신호: 호스트는 `agent_settled`와 `session_closed`를 터미널 신호로 본다(crates/maho-rpc/src/host_session_wait.rs:36-39). tau는 `agent_settled`를 확장 이벤트로만 내보낸다(ext.rs:1593-1597).

tau에 요구되는 최소 계약은 (a) `--mode rpc --multi-session --session-runtime in-process`로 기동해도 죽지 않을 것, (b) 한 프로세스에서 `sessionId`로 구분되는 N개 세션을 동시에 굴릴 것, (c) 응답과 이벤트에 `sessionId`를 붙일 것이다.

## 3. 설계

### 3.1 플래그 표면 (crates/omp-cli/src/cli.rs)

- `Args`에 두 필드를 추가한다(cli.rs:22 블록, `context_strategy`(:89) 뒤):
  - `pub multi_session: bool`
  - `pub session_runtime: Option<SessionRuntime>`
- 새 열거형을 `Resume`(cli.rs:15-20) 옆에 둔다: `pub enum SessionRuntime { InProcess, Worker }`. 문자열 매핑은 `SessionRuntime::parse(&str) -> Option<Self>` 하나로 두고 mhc의 두 값만 받는다.
- `VALUELESS_FLAGS`(cli.rs:142-176)에 `"--multi-session"`을 넣고, `parse_from`의 valueless match(cli.rs:392-410)에 `"--multi-session" => a.multi_session = true,`를 추가한다. `--yolo`처럼 별도 검증이 없는 단순 불리언이라 이 경로가 맞다.
- `STRING_FLAGS`(cli.rs:97-136)에 `"--session-runtime"`을 넣고, `set_string`(cli.rs:233)에 `--mode`와 같은 모양의 검증 분기를 추가한다(cli.rs:238-248 참조): 잘못된 값은 `invalid.push(format!("Invalid --session-runtime value: {}. Expected one of: in-process, worker.", json_str(&value)))`로 모아 `finish`에서 한 번에 보고. `--mode`와 달리 값 집합 상수를 `MODE_VALUES`(cli.rs:177) 옆에 `SESSION_RUNTIME_VALUES: &[&str] = &["in-process", "worker"]`로 둔다.
- `finish`(cli.rs:456)에 교차 검증 두 개를 추가한다. `--session-runtime`만 주고 `--multi-session`이 없으면 `usage_err("--session-runtime requires --multi-session")`, `--multi-session`이 `--mode rpc`가 아니면 `usage_err("--multi-session requires --mode rpc")`. 기존 `--trusted-extension`/`--goal` 검증과 같은 자리·같은 헬퍼(cli.rs:185)를 쓴다.
- `resolve_session_runtime(&Args) -> SessionRuntime`를 cli.rs에 둔다(mhc의 동명 함수 대응): 명시값 우선, 없으면 `InProcess`. tau에는 `--listen`이 없으므로 mhc의 "stdio면 Worker" 기본은 채택하지 않는다. 게이트웨이가 요구하는 조합이 in-process이기 때문이다.
- `HELP` 문자열(main.rs:43-66)에 두 줄을 추가한다.

### 3.2 진입점 분기 (crates/omp-cli/src/main.rs)

- 런타임 플레이버: `sequential` 판정(main.rs:265-267)에 `&& !raw.iter().any(|a| a == "--multi-session")`을 더한다. 다중 세션 호스트는 세션 N개를 동시에 돌리므로 current-thread면 세션 하나의 툴 대기가 전체를 막는다.
- 단일 세션 경로는 한 줄도 바꾸지 않는다. 분기점은 `if args.mode.as_deref() == Some("rpc")`(main.rs:545) **앞**에 새 블록을 넣는 것이다:
  ```
  if args.mode.as_deref() == Some("rpc") && args.multi_session {
      return crate::host::run(args, args.session_runtime.unwrap_or(InProcess)).await;
  }
  ```
  이 지점을 고른 이유: `resume_target` 계산(main.rs:510-537)이 이미 `mode == rpc`에서 `None`으로 우회하므로, 그 이전에 빠져나가면 세션별 resume 해석을 호스트가 직접 소유할 수 있다.
- 그 앞의 단일 세션 준비 단계(cwd 확정 :335, 확장 기동 :438, `context::set_active` :465, `Gate::install` :472, `build_agent` :478, `attach_with` :491)는 다중 세션 경로에서 **실행되면 안 된다**. 첫 세션의 cwd·게이트·확장이 프로세스 전역에 박히기 때문이다. 따라서 위 분기 블록은 main.rs:328(`acp`/`rpc-ui` 거절) 직후, cwd 확정(:335) **이전**에 놓는다. 즉 다중 세션 경로는 `async_main`의 세션 준비 코드를 전혀 타지 않고 `host::run`이 전부 소유한다.

### 3.3 세션 스코프 추출 (신규 crates/omp-cli/src/session.rs)

main.rs:335-543을 `pub struct SessionSpec { cwd, model, provider, api_key, thinking, extension, hooks, no_extensions, session_dir, no_session, approval_mode, auto_approve, sandbox, context_strategy, tools_csv, no_tools }`와 `pub async fn open(spec: &SessionSpec) -> Result<OpenedSession>`로 뽑는다. `OpenedSession { rt: Arc<Runtime>, extensions: Option<Arc<Extensions>>, target, cwd, persist }`.

이 추출은 단일 세션 경로에도 그대로 쓰인다(main.rs가 `session::open`을 호출하고 결과를 기존 변수 자리에 넣는다). 동작 변경 없이 다중 세션이 같은 코드를 재사용하게 만드는 것이 목적이다.

### 3.4 프로세스 전역 → 세션 스코프

다중 세션의 실제 난점은 1장에 열거한 싱글턴이다. 우선순위대로 세 부류로 나눈다.

1. **세션 값으로 바꿔야 하는 것 (필수)**: `tools::SESSION_ENV`/`SESSION_JOBS`(tools.rs:54, :58)와 `eval_bridge::SESSION_TOOLS`(eval_bridge.rs:51), `omp_urls::SESSION_FILE`(omp-urls/src/lib.rs:54), `rpc::RUNTIME_TX`(rpc.rs:137-141), `runtime::MODEL_PINNED`/`THINKING_PINNED`/`LAUNCH_FALLBACKS`(runtime.rs:4197-4211). 설계: `SessionScope` 구조체를 만들어 이 값들을 필드로 옮기고, `Env`(tools.rs:115)와 `Runtime`이 `Arc<SessionScope>`를 들고 있게 한다. 기존 자유 함수(`set_session_env`, `session_env`, `set_global_session_file`)는 "현재 스코프"를 참조하는 얇은 래퍼로 남겨 호출부 200여 곳을 건드리지 않는다. 현재 스코프는 `tokio::task_local!`로 두고, 세션 태스크 진입 시 `SESSION_SCOPE.scope(scope, ...)`로 감싼다. thread_local이 아니라 task_local이어야 하는 이유: rpc 워커(rpc.rs:1441-1450)가 세션마다 별도 태스크이기 때문이다.
2. **세션마다 인스턴스를 만들면 되는 것**: 확장 호스트(`Extensions::start`, ext.rs:624)는 이미 `cwd`별로 뜨고 `attach_session`(:1054)이 `Arc<Runtime>`을 받으므로 세션마다 하나씩 띄우면 된다. 단 `slash::EXTENSIONS`(slash.rs:65)가 `OnceLock`이라 두 번째 세션의 `set_extensions`가 무시된다 → `SessionScope`의 필드로 옮기고 `slash::extensions()`를 스코프 조회로 바꾼다.
3. **단일 테넌트로 남겨도 되는 것 (문서화)**: `approval::GATE`(approval.rs:837)는 `bind(&rt, has_ui)`(:875-882)로 세션마다 재바인딩되므로 게이트 자체는 공유해도 되지만 `mode_override`/`auto_approve`가 프로세스 값이 된다 → 다중 세션에서는 `--approval-mode`/`--yolo`를 첫 세션 값으로 고정한다고 명시하고, 세션별 차이는 1차 범위에서 지원하지 않는다. `herdr::GLOBAL`(herdr.rs:353), `methodology::AUDIT`(methodology.rs:28), `title::LAST_TERMINAL_TITLE`(title.rs:569)는 다중 세션에서 의미가 없으므로 비활성으로 둔다.

### 3.5 세션별 격리 항목

- **작업 디렉터리**: `SessionSpec.cwd`가 세션마다 다르고, `tools::Env.cwd`(tools.rs:116)와 `bash` 툴의 실행 cwd가 그 값을 쓴다. 프로세스 `current_dir`은 건드리지 않는다(main.rs:337의 `std::env::current_dir()`은 단일 세션 경로 전용). `omp_settings::Settings::load(cwd)`(settings.rs:108)도 세션 cwd를 받으므로 설정 레이어는 이미 격리된다.
- **확장 인스턴스**: 세션당 `Extensions` 1개, `session_start`/`attach_session`/`shutdown`을 세션 수명에 묶는다. 공유 데몬 모드에서도 Worker가 세션별로 갈리므로(ext.rs:630-633) 확장 전역 상태는 격리된다. 확장 디렉터리는 3.6의 루트 해석기를 통해 세션 설정 루트를 따른다.
- **이벤트 스트림**: `Runtime.out`(runtime.rs:116)이 세션별 `UnboundedSender`이므로 이미 분리돼 있다. 호스트 경계에서 나갈 때 `frame["sessionId"] = scope.session_id`를 주입하고, 들어올 때 `cmd["sessionId"]`로 세션을 고른다. `rpc::response()`(rpc.rs:203)를 `response_for(id, command, result, session: Option<&str>)`로 확장해 한 곳에서만 태깅한다.
- **세션 파일**: `persist::attach_with`(persist.rs:502)가 세션마다 새 writer를 만든다. `--session-dir`는 세션 공통 기본값으로만 쓰고, 세션별 경로는 `new_session(Some(dir))`(runtime.rs:2215)로 지정한다.
- **설정 루트(`~/.omo`)**: `omp-settings::agent_dir`(settings.rs:461-471)에 `PI_CONFIG_DIR` 해석을 추가한다(omp-stats의 `config_root_dir`(paths/mod.rs:20-27)와 같은 규칙). `ext.rs:91`/`:638`의 하드코딩된 `~/.omp/agent/extensions`도 같은 해석기를 호출하게 바꾼다. 그러면 플릿은 `PI_CONFIG_DIR=.omo` 하나로 tau 전체를 `~/.omo`에 묶을 수 있고 세션별 `omo.jsonc` 프로필도 그 아래에 놓을 수 있다.

### 3.6 호스트 (신규 crates/omp-cli/src/host.rs)

```
pub struct SessionHost {
    sessions: tokio::sync::RwLock<HashMap<String, Arc<HostedSession>>>,
    spec: SessionSpec,            // 세션 공통 기본값
    runtime: SessionRuntime,
}
struct HostedSession { rt: Arc<Runtime>, ext: Option<Arc<Extensions>>, scope: Arc<SessionScope> }
```

- `run(args, runtime)`: `--session-runtime`에 따라 `run_in_process` 또는 `run_worker`로 나간다.
- `run_in_process`: stdin 라인 루프(rpc.rs:1452과 같은 모양)를 호스트가 소유하고, `sessionId`가 없는 명령은 "기본 세션"으로 보낸다(첫 `prompt`가 기본 세션을 lazy 생성). 세션 생성은 `session::open(&spec)` + `Runtime::start_with`(runtime.rs:409). 명령 실행은 세션별 `tokio::spawn`이 아니라 세션별 단일 워커 태스크로 직렬화한다(rpc.rs:1441-1450의 `busy` 패턴을 세션마다 복제).
- `run_worker`: 세션마다 `current_exe()`를 `--mode rpc`(플래그 없이)로 spawn하고 소켓/파이프로 중계한다. 이는 `subcmd/daemon.rs:378-379`가 이미 하는 일과 같으므로 그 헬퍼를 `host::spawn_session_worker`로 승격해 재사용한다.
- `ready` 프레임에 `"capabilities": ["multi_session"]`과 `"launchProfile": {"core": {"session_runtime": "in-process", "multi_session": true}}`를 싣는다(mhc의 `protocol_info` 형태, host_protocol_info.rs:13-19 대응).
- 전송 계층: `rpc::run_with`(rpc.rs:1374)가 `tokio::io::stdin()`/`stdout()`을 직접 쓰므로(rpc.rs:1399, :1434) `AsyncRead + AsyncWrite` 제네릭으로 뺀다. 호스트는 이 제네릭 루프를 stdio로, 테스트는 `tokio::io::duplex`로 같은 코드를 돌린다.

## 4. 구현 단계 (커밋 단위)

1. **cli 플래그** — `Args` 두 필드, `SessionRuntime` 열거형, `SESSION_RUNTIME_VALUES`, `VALUELESS_FLAGS`/`STRING_FLAGS`/`set_string`/valueless match/`finish` 교차검증, `resolve_session_runtime`, `HELP` 두 줄. 동작 변화 없음(파싱만). 단위 테스트 포함.
2. **설정 루트 해석기** — `omp-settings::agent_dir`에 `PI_CONFIG_DIR` 반영, `ext.rs:91`/`:638` 하드코딩 제거. `PI_CONFIG_DIR` 미설정 시 기존 경로 그대로임을 테스트로 고정.
3. **`session.rs` 추출** — main.rs:335-543을 `SessionSpec`/`session::open`으로 이동, main.rs는 호출만. 단일 세션 동작 불변(기존 테스트 전부 통과가 이 커밋의 게이트).
4. **`SessionScope` 도입** — 3.4의 1번 항목을 스코프 필드로 옮기고 래퍼 함수 유지. `task_local` 설치 지점은 `session::open`과 rpc 워커.
5. **rpc 전송 제네릭화 + sessionId 태깅** — `run_with` 시그니처에 `R: AsyncRead + Unpin, W: AsyncWrite + Unpin`, `response_for`. 단일 세션 경로는 `sessionId`를 붙이지 않는 기존 동작 유지(플래그 off일 때 바이트 동일성 테스트).
6. **`host.rs` in-process** — `SessionHost`, 세션 lazy 생성, 세션별 워커 직렬화, `ready` 능력 광고, main.rs 분기(main.rs:328 직후)와 런타임 플레이버 수정(:265-267).
7. **`host.rs` worker** — `subcmd/daemon.rs`의 spawn 헬퍼 승격, `--session-runtime worker` 경로.

## 5. 테스트

- `cli.rs` tests 모듈(cli.rs:456 이후 기존 `#[test]` 블록)에: `--multi-session` 단독 파싱, `--session-runtime in-process`/`--session-runtime=worker`, 잘못된 값의 정확한 오류 문자열(`Invalid --session-runtime value: "threads". Expected one of: in-process, worker.`), `--session-runtime` 없이 `--multi-session`만 있을 때 허용, `--mode text --multi-session`일 때 `--multi-session requires --mode rpc`, `resolve_session_runtime` 기본값이 `InProcess`인지.
- `session.rs`: 같은 `SessionSpec`으로 `open`을 두 번 호출해 `rt.session_id()`가 다르고 `rt.cwd`가 각각인지 확인(파일시스템은 `tempfile::tempdir`).
- 스코프 격리: 두 세션을 열고 각각 `session_env()`가 서로 다른 `Arc<Env>`를 돌려주는지, `omp_urls::global_session_file()`이 세션 태스크 안에서 각자 값을 보는지. `tokio::task_local`이므로 `tokio::test`에서 두 태스크를 동시에 띄워 교차 오염이 없는지 본다.
- rpc 라우팅: `tokio::io::duplex`로 호스트를 띄우고, `sessionId: "a"`/`"b"`로 `prompt`를 보낸 뒤 각 세션의 응답·이벤트에 자기 `sessionId`만 실리는지. 세션 A의 `abort`가 B의 `isStreaming`을 건드리지 않는지.
- 회귀: 플래그 없는 `--mode rpc`의 stdout 프레임이 이전과 바이트 동일한지(고정 프레임 시퀀스 스냅샷, `sessionId` 키 부재 확인).
- 확장: `Extensions` 두 인스턴스를 띄워 `set_session_ctx`가 서로의 ctx를 덮지 않는지(공유 데몬 소켓은 `OMP_RS_EXT_HOST=private`로 고정해 테스트 결정성 확보, ext.rs:156-161).
- 금지: 고정 sleep으로 세션 준비를 기다리지 않는다. `ready` 프레임 수신과 `agent_settled`/`agent_end` 이벤트 구독을 먼저 걸고 그 신호를 bounded timeout으로 기다린다.

## 6. QA 시나리오

1. `omt --mode rpc --multi-session --session-runtime in-process` 기동 → `ready` 프레임에 `multi_session` 능력과 `session_runtime: "in-process"`가 실린다.
2. 같은 프로세스에 `sessionId: "s1"`(cwd `/data/workspace/a`)과 `"s2"`(cwd `/data/workspace/b`)로 `prompt`를 연속 전송. 각 응답의 `sessionId`가 요청과 일치하고, `read`/`bash`가 각자의 cwd에서 동작한다(`pwd` 결과 비교).
3. 두 세션에 동시에 긴 턴을 걸어 한 세션의 `abort`가 다른 세션을 끊지 않는지 확인.
4. `PI_CONFIG_DIR=.omo`로 기동해 설정·확장·세션 디렉터리가 모두 `~/.omo` 아래에 생기고 `~/.omp`에 아무것도 쓰이지 않는지(`find ~/.omp -newer` 로 확인).
5. `--session-runtime worker`로 같은 시나리오 → `ps`에 세션 수만큼 `omt --mode rpc` 자식이 보이고, 부모를 죽이면 자식이 함께 정리된다.
6. 플래그 없이 `omt --mode rpc` → 기존 단일 세션 클라이언트(현재 게이트웨이)가 그대로 동작하고 프레임에 `sessionId`가 없다.
7. `--multi-session` 없이 `--session-runtime in-process` → 즉시 사용법 오류로 종료(코드 1), stdin을 읽지 않는다.

## 7. 위험과 미확인

- **싱글턴 전량 전환 비용**: 3.4의 1번 목록은 `tools.rs`/`eval_bridge.rs`/`omp-urls`/`rpc.rs`/`runtime.rs`에 걸쳐 있고, 래퍼 유지 전략을 택해도 `tokio::task_local`이 닿지 않는 호출 경로(툴 내부에서 spawn한 태스크, `rayon` 병렬 경로)가 있으면 값이 새는 세션 간 누출이 생긴다. 어느 호출 경로가 스코프 밖으로 나가는지는 **확인 필요**.
- **`context::ACTIVE`(context.rs:36)**: `OnceLock`이라 세션별 컨텍스트 전략을 다르게 줄 수 없다. `Runtime` 필드로 옮기는 것이 옳지만 `context::live()`/`context::tool()` 호출부가 전역을 전제하므로 4번 커밋 범위가 커진다. 1차에서는 다중 세션 시 `--context-strategy`를 첫 세션 값으로 고정한다.
- **`--approval-mode`/`--yolo` 세션별 차이 미지원**: `Gate::install`(approval.rs:841)이 `OnceLock::set`이라 첫 호출이 이긴다. 게이트웨이가 세션마다 승인 모드를 다르게 요구하면 별도 설계가 필요하다. 요구 여부 **확인 필요**.
- **`agent_settled` 부재**: tau는 확장 핸들러가 있을 때만 `agent_settled`를 낸다(ext.rs:1593). 확장 없이 뜬 다중 세션 호스트는 게이트웨이가 기다리는 종료 신호를 보내지 않는다. 이 조각의 범위 밖이지만, 다중 세션 호스트가 `agent_settled`를 무조건 emit하도록 바꾸지 않으면 2번 QA가 게이트웨이 쪽에서 실패한다.
- **세션 부착 명령 이름 불일치 가능성**: mhc는 `attach_session`/`release_attachment`/`reclaim_session`/`list_sessions`를 갖지만(connection_handler.rs:1636-1638), 게이트웨이가 tau에 실제로 보내는 프레임 목록은 `sessionId` 필드 라우팅만일 수 있다. 우리 게이트웨이가 어느 쪽을 쓰는지 **확인 필요**. 1차 구현은 `sessionId` 라우팅만 하고 부착 명령은 6번 커밋 이후로 미룬다.
- **in-process 격리의 한계**: 한 세션의 패닉이 프로세스 전체를 죽인다. mhc의 `Worker` 모드가 존재하는 이유이고, 그래서 7번 커밋(worker 경로)이 선택지가 아니라 안전판이다. 게이트웨이가 in-process를 강제하면 이 위험을 감수한다는 결정을 명시해야 한다.
- **`mimalloc` 재exec**(main.rs:224-246)은 `exec`로 프로세스를 교체하므로 다중 세션 호스트에서도 기동 시 한 번만 일어난다. 세션 생성 경로에서 `ensure_mimalloc_env`를 다시 부르지 않도록 주의(현재는 `async_main` 선두 1회, main.rs:282).
- **미확인**: `--multi-session`이 `--session-dir`/`--no-session`과 조합될 때 mhc의 의미(세션 공통 vs 세션별). tau 설계는 "공통 기본값 + 세션별 override"로 잡았으나 게이트웨이 기대와 일치하는지 **확인 필요**.
