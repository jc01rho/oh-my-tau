# RPC `extension_request` 명령 추가 설계

## 1. 현재 상태 (근거)

- RPC 진입점은 `crates/omp-cli/src/rpc.rs` 하나다. `handle()`(`rpc.rs:562`)가 `cmd.type`을 꺼내 `dispatch()`(`rpc.rs:593`)로 넘기고, `dispatch`의 마지막 `other =>`(`rpc.rs:1094`)가 `Unknown command: {other}`를 만든다. `extension_request`는 여기 걸린다.
- 응답 프레임은 `response()`(`rpc.rs:203`)가 만든다: `{id, type:"response", command, success, data?}` 또는 `{..., success:false, error, code?}`. 게이트웨이가 기대하는 프레임과 키 순서가 이미 같다.
- 명령 실패 타입은 `CmdError { message: String, code: Option<&'static str> }`(`crates/omp-cli/src/runtime.rs:34`)이고 `CmdError::coded`(`runtime.rs:45`)로 코드를 붙인다. 코드는 정적 문자열만 가능하다.
- RPC 수신 루프(`rpc.rs:1435` 이후)는 `bash`만 `background: JoinSet`으로 분리하고(`rpc.rs:1491`), 나머지는 `cmd_tx`로 보내 워커가 **한 번에 하나씩** 처리한다(`rpc.rs:1439`~`1450`). 즉 `dispatch` 안에서 오래 기다리는 명령은 뒤에 큐잉된 모든 명령을 막는다.
- 확장 호스트 전송 계층은 `crates/omp-ext/src/host.rs`다. `send_request()`(`host.rs:431`)가 `PendingTable`(`host.rs:23`)에 oneshot 웨이터를 등록하고 id를 증가시켜 보내며, `request()`(`host.rs:463`)는 `rx.await`만 한다. **타임아웃이 없다.** 호스트가 죽으면 `spawn_writer`(`host.rs:238`)의 `fail_all("extension host stdin closed")`(`host.rs:251`)와 `spawn_reader`(`host.rs:309`)의 `fail_all("extension host exited")`(`host.rs:322`)만 웨이터를 깨운다.
- `call_command()`(`host.rs:678`)는 `request("call_command", {name, args})` 한 줄이다. 즉 현재 확장 호출 표면은 명령/도구 두 개뿐이고, 이름 있는 임의 질의 표면은 없다.
- `has_handlers()`(`host.rs:575`)는 `load` 결과의 이벤트 구독 집합(`host.rs:505`~`513`)만 본다. 질의 이름 단위 게이트는 없다.
- 확장 쪽 API는 `ext-host/host.ts:341`의 `const api = {...}`이고 항목은 `on`(`host.ts:372`), `registerTool`(`host.ts:377`), `registerCommand`(`host.ts:380`), `registerFlag`(`host.ts:386`) 등이다. **`api.rpc` 키가 없다**(`grep 'api.rpc' ext-host/host.ts` = 0건). 게이트웨이 확장은 `api.rpc.handle("omomeow.move-readiness", ...)`를 호출한다(`~/Work/omo-nyang/src/session-move-extension.ts:139`). tau에서 이 확장을 로드하면 `loadOne`의 팩토리 try/catch(`host.ts:493`~`499`)가 TypeError를 잡아 `errors[]`로 돌려보내고, 확장은 조용히 미로드 상태가 된다.
- 호스트 디스패치는 `dispatch()`(`host.ts:996`), 미지원 메서드는 `-32601 method not found`(`host.ts:1060`), 미지원 명령은 `-32004`(`host.ts:961`~`966`)다.
- 호스트의 핸들러 예산은 `TIMEOUT_MS = OMP_EXT_TIMEOUT_MS || 30_000`(`host.ts:35`), `emit`은 요청별 `timeoutMs`를 받아 `runHandler`의 `Promise.race`(`host.ts:538`~`549`)로 자른다. **`call_command`/`call_tool` 경로에는 이 예산이 적용되지 않는다**(`host.ts:1021`~`1036` switch).
- tau는 `~/.omp`를 기본 확장 디렉터리로 쓴다(`host.ts:512`: `~/.omp/agent/extensions`). 우리 플릿의 확장은 `~/.omo` 아래에 있다.
- `Extensions`는 `pub host: Arc<ExtHost>`를 들고 있고(`crates/omp-cli/src/ext.rs:486`), `Runtime::ext_host()`(`runtime.rs:2583`)로 꺼낸다. RPC 모드는 `rpc_on_start()`(`ext.rs:1201`)로 `attach_session(rt, ExtMode::Rpc)`를 건다(`ext.rs:1054`).
- `session_move_export`/`session_move_import` 이벤트와 `wake_source_state` 이벤트는 tau에 없다(`grep -rn 'session_move\|wake_source' crates` = 0건).

## 2. 게이트웨이가 요구하는 계약

mhc 기준 계약(실측 소스):

- 요청: `{"type":"extension_request","name":"<name>","data":<any|null>,"sessionId?":"..."}`. mhc 타입은 `RpcCommandBody::ExtensionRequest { name: String, data: Option<Value> }`(`~/Work/mhc/crates/maho-rpc/src/rpc_types.rs:489`~`494`), 클라이언트는 `{"type":"extension_request","name":name,"data":data}`를 보낸다(`~/Work/mhc/crates/maho-rpc/src/rpc_client.rs:1219`~`1225`).
- 이름 검증: 앞뒤 공백 제거 후 빈 이름은 오류(`connection_handler.rs:1533`~`1534`, 문자열 `Extension RPC request name cannot be empty`).
- 핸들러 없음: `Unknown extension RPC request: {name}`(`~/Work/mhc/crates/maho-ext-host/src/runner.rs:1492`~`1494`). 중복 등록: `Multiple extension RPC request handlers registered`(`runner.rs:1495`~`1498`). 실패 타입 `ExtensionFailure`는 `message/stack/class/code`를 갖는다(`~/Work/mhc/crates/maho-ext-api/src/types.rs:793`~`797`).
- 광고되었지만 미서비스인 이름은 타입화된 코드로 응답한다(`connection_handler.rs:1535`~`1538`).
- 응답: `{id, type:"response", command:"extension_request", success, data}` 또는 `error`+`code`(`connection_handler.rs:1540`~`1542`).
- 게이트웨이 호출 지점과 기대값:
  - 세션 이동 준비 상태: `{activeSources: string[]}` (`~/Work/omo-nyang/src/relay/agentd-sessions.ts:141`, `src/relay/move-operations.ts:212`).
  - omo 피드백: `omo.feedback.prepare`는 초안 객체 또는 `{kind:"unavailable", reason}`(`src/omo-feedback.ts:41`, `:24`~`27`), `omo.feedback.submit`은 결과 객체(`:50`).
- 게이트웨이 자체 타임아웃: `request(session, command, timeoutMs = 30_000)`(`agentd-sessions.ts:105`), 초과 시 `MoveError("session_timeout")`(`:117`).

따라서 tau가 지켜야 할 것: (1) 위 프레임 모양, (2) 30초 안에 반드시 응답, (3) 확장 부재를 오류로 즉시 알리되 게이트웨이가 해석 가능한 코드를 줄 것, (4) 응답 대기 중 다른 RPC 명령이 막히지 않을 것.

## 3. 설계

### 3.1 확장 표면 (`ext-host/host.ts`)

- `makeApi`(`host.ts:340`)에 `rpc: { handle(name: string, fn: (data: Json) => Promise<unknown> | unknown) }`를 추가한다. 등록은 `ext.rpcRequests: Map<string, fn>`에 저장하고, `Ext` 인터페이스(`host.ts:21`~`27`)에 `rpcRequests` 필드를 더한다. `registerFlag`(`host.ts:386`)와 같은 로드 트랜잭션 규약을 따른다: 팩토리가 던지면 `rollbackLoad`(`host.ts:499` 인근)가 등록을 되돌린다.
- `load` 응답(`host.ts:519`~`529`)에 `rpcRequests: string[]`를 추가해 코어가 이름 단위로 게이트할 수 있게 한다. 이는 `handlers`/`tools`/`commands`와 같은 자리의 증분 필드라 구코어는 무시한다.
- `dispatch`(`host.ts:996`)에 `case "rpc_request"`를 넣는다. 의미론은 mhc와 동일하게: 이름 trim, 빈 이름 `-32602`, 핸들러 없음 `-32004 unknown extension rpc request: {name}`, 2개 이상 등록 시 `-32600`(등록 시점에 이름 충돌을 잡는 편이 낫다: 두 번째 `handle` 호출에서 throw).
- 핸들러 실행은 `runHandler`(`host.ts:538`)와 같은 `Promise.race` 예산을 쓴다. 요청의 `timeoutMs`가 있으면 그 값, 없으면 `TIMEOUT_MS`(`host.ts:35`). 초과 시 `-32001 extension rpc request timed out after {n}ms`. 이렇게 해야 호스트가 코어에 반드시 프레임을 돌려주고, 미응답 promise가 남지 않는다.

### 3.2 전송 계층 (`crates/omp-ext/src/host.rs`)

- `send_request()`(`host.rs:431`)는 현재 `oneshot::Receiver`만 돌려준다. 타임아웃 후 웨이터를 회수하려면 id가 필요하므로 `fn send_request_tagged(&self, method, params) -> Result<(u64, oneshot::Receiver<...>)>`를 추가하고 기존 `send_request`는 그 위의 얇은 래퍼로 남긴다.
- `PendingTable`(`host.rs:23`)에 `fn cancel(&mut self, id: u64)`를 추가해 `waiters.remove(&id)`만 한다. `fail_all`(`host.rs:29`)은 건드리지 않는다.
- 새 공개 API:
  ```rust
  pub async fn request_extension(&self, name: &str, data: Value, budget: Duration) -> Result<Value>
  pub fn has_rpc_handler(&self, name: &str) -> bool
  ```
  `request_extension`은 `send_request_tagged("rpc_request", json!({"name":name,"data":data,"timeoutMs":budget_ms}))` 후 `tokio::time::timeout(budget, rx)`로 기다리고, 만료 시 `pending.cancel(id)` 후 `Err(anyhow!("extension rpc request '{name}' timed out after {}ms"))`를 돌려준다. `Err` 문자열에 접두사를 넣어 호출자가 코드로 승격하기 쉽게 한다.
- `has_rpc_handler`는 `subscribed`(`host.rs:206`) 옆에 `rpc_requests: Mutex<Option<HashSet<String>>>`를 두고 `load`(`host.rs:494` 인근)에서 `LoadResult.rpc_requests`로 채운다. `None`(미로드)일 때는 `true`를 돌려 `has_handlers`(`host.rs:575`)와 같은 관용 정책을 유지한다. `LoadedExtension`(`host.rs:115`)·`LoadResult`(`host.rs:127`)에 `#[serde(default)] pub rpc_requests: Vec<String>`를 추가한다.

### 3.3 RPC 명령 (`crates/omp-cli/src/rpc.rs`)

- `dispatch`에 `"extension_request"` arm을 추가한다. 처리 순서:
  1. `str_field(cmd, "name")`로 이름을 꺼내 `trim`; 비면 `CmdError::new("name must be a non-empty string")`.
  2. `sessionId`가 왔고 `rt.session_id()`(`runtime.rs:1921`)와 다르면 `CmdError::coded(..., "unknown_session")`. tau는 단일 세션 프로세스라 라우팅은 하지 않고 불일치만 거절한다.
  3. `rt.ext_host()`(`runtime.rs:2583`)가 `None`이면 `CmdError::coded("extension host is not attached", "extension_unavailable")`.
  4. `!host.has_rpc_handler(name)`이면 `CmdError::coded(format!("Unknown extension RPC request: {name}"), "unknown_extension_request")`.
  5. `request_extension(name, data, budget)`을 호출하고 결과를 `ok(value)`로 감싼다. 오류는 메시지 접두사로 분류해 `"extension_timeout"` / `"extension_failed"` 코드를 붙인다.
- **예산**: `fn rpc_request_budget() -> Duration`을 `ext.rs:30`의 `tool_timeout()`과 같은 형태로 두되 기본값은 10초(`OMP_EXT_RPC_TIMEOUT_MS`로 조정). 게이트웨이 상한이 30초(`agentd-sessions.ts:105`)이므로 코어가 먼저 끊어 코드 있는 응답을 주는 편이 낫다. 호스트 측 `timeoutMs`에도 같은 값을 넘겨 양쪽 예산을 맞춘다.
- **직렬화 회피**: 이 arm은 `cmd_tx`(`rpc.rs:1509`)로 보내지 않고 `bash`와 같은 `background: JoinSet`(`rpc.rs:1491`) 분기로 보낸다. 그래야 확장이 예산을 다 써도 뒤따르는 `get_state`/`abort`/`prompt`가 막히지 않는다. 응답은 완료 시 `rt.send(response(cmd.get("id"), "extension_request", result))`로 보낸다(`runtime.rs:518`의 `send`).
- **정착(settle)과의 관계**: `agent_settled`(`ext.rs:1593`)는 확장 이벤트이고 이 명령과 무관하다. `extension_request`는 `input_gate`(`rpc.rs:576`)도, slash 디스패치도 타지 않으므로 턴 상태를 바꾸지 않는다. 대기 중 `abort`는 `busy` 우회 경로(`rpc.rs:1502`)로 즉시 처리된다.

### 3.4 확장 부재 시 응답

두 층으로 나눈다. 확장이 아예 없거나 이름을 등록하지 않았으면 코어가 `success:false, code:"unknown_extension_request"`를 즉시 준다(대기 없음). 확장이 있으나 스스로 "이 환경에서는 못 한다"고 답하는 경우는 정상 `data`로 `{kind:"unavailable", reason}`을 돌려주는 것을 규약으로 문서화한다. 게이트웨이는 이미 그 모양을 `FeedbackUnavailable`로 처리한다(`omo-feedback.ts:24`~`27`).

## 4. 구현 단계 (커밋 단위)

1. `ext-host/host.ts`: `Ext.rpcRequests`, `api.rpc.handle`, 이름 중복 throw, `dispatch`의 `rpc_request` + 예산, `load`의 `rpcRequests`. 픽스처 `crates/omp-ext/tests/fixtures/rpc.ts` 추가.
2. `crates/omp-ext/src/host.rs`: `PendingTable::cancel`, `send_request_tagged`, `request_extension`, `has_rpc_handler`, `LoadedExtension`/`LoadResult`의 `rpc_requests`.
3. `crates/omp-cli/src/ext.rs`: `rpc_request_budget()`와 코드 승격 헬퍼(`fn classify_extension_error(e: &anyhow::Error) -> CmdError`). `Extensions`에 얇은 위임 메서드를 두어 rpc.rs가 `omp_ext` 타입을 직접 다루지 않게 한다.
4. `crates/omp-cli/src/rpc.rs`: `dispatch`의 `"extension_request"` arm + `background` 분기. `CmdError` 코드 4종 고정.
5. 테스트(5절).
6. `docs/` 에 계약 문서 1편(`docs/parity/` 아래), `tests-e2e/ext/conformance.py`의 rpc 모드에 probe 추가.

## 5. 테스트

- `crates/omp-ext/tests/host.rs` 스타일(현재 `loaded()` 헬퍼가 `fixture.ts`/`broken.ts`/`nofactory.ts`를 로드, `host.rs:33`~`60`):
  - `rpc_request_roundtrip`: `rpc.ts`가 `pi.rpc.handle("fx.rpc", d => ({echo:d}))` 등록, `host.request_extension("fx.rpc", json!({"a":1}), Duration::from_secs(5))`가 `{"echo":{"a":1}}`.
  - `rpc_request_unknown_name`: 미등록 이름에 `has_rpc_handler` false, 호출은 `-32004` 메시지 포함 `Err`.
  - `rpc_request_timeout_releases_waiter`: 핸들러가 `new Promise(() => {})`인 픽스처(`fixture.ts:23`의 `pi.on("hang")` 패턴 재사용)로 200ms 예산, `Err`에 `timed out`, 이후 같은 호스트에서 정상 질의가 성공(웨이터 회수 검증). 고정 sleep 금지, `tokio::time::timeout`으로 상한만 건다.
  - `duplicate_rpc_name_is_rejected`: 두 확장이 같은 이름 등록 시 두 번째 `load`가 `errors[]`에 이름 충돌을 남긴다.
- `crates/omp-cli/src/rpc.rs`의 `#[cfg(test)] mod tests`(`rpc.rs:1535`)에서 `handle()` 직접 호출: 확장 호스트 미부착 상태에서 `{"type":"extension_request","name":"x","id":"e1"}` → `success:false`, `code:"extension_unavailable"`, `command:"extension_request"`. 빈 이름 → `success:false`.
- 회귀: 기존 `Unknown command` 경로가 남은 명령에 대해 그대로인지(`rpc.rs:1094`) 1건.

## 6. QA 시나리오

1. `omt --mode rpc`에 `{"type":"extension_request","name":"nope","id":"1"}` → 즉시 `success:false, code:"unknown_extension_request"`, 1초 이내 응답.
2. 같은 프로세스에서 확장이 등록한 이름으로 질의 → `data`가 픽스처 반환값과 일치.
3. 확장 핸들러가 무한 대기 → 예산 후 `code:"extension_timeout"`, 그리고 **직후** `{"type":"get_state"}`가 정상 응답(직렬화 회피 확인).
4. 확장 디렉터리를 비운 상태로 1번과 2번 반복 → 각각 `unknown_extension_request`, 호스트 미부착이면 `extension_unavailable`. 프로세스는 살아 있어야 한다.
5. `extension_request` 대기 중 `{"type":"abort"}` 전송 → abort 응답이 질의 응답보다 먼저 오고, 이후 질의 응답도 도착.
6. `tests-e2e/ext/conformance.py --mode rpc`(`:140`~`:235` 구동부)에 1·2번 probe를 넣고 omp(TS) 대비 diff가 `EXPLAINED` 외에 없음을 확인. TS omp는 `api.rpc`를 실제로 제공하므로 이름 등록 결과가 같아야 한다.

## 7. 위험과 미확인

- **확인 필요**: TS omp의 `api.rpc.handle` 정확한 시그니처와 `rpc_request` 와이어 이름. tau 저장소 안에 `api.rpc` 정의가 없어(`grep` 0건, `vendor/`에도 없음) mhc의 `rpc_handlers`(`maho-ext-api/src/types.rs:4894`)와 게이트웨이 호출 형태(`session-move-extension.ts:139`)로 역산했다. 실제 omp `ExtensionAPI` 타입과 다를 수 있다.
- **확인 필요**: `omomeow.move-readiness`가 요구하는 `activeSources`의 원천인 `wake_source_state` 이벤트가 tau에 없다. 이 명령을 추가해도 이동 준비 상태 질의는 `{activeSources: []}`만 답하게 되어, 게이트웨이는 "백그라운드 작업 없음"으로 오판할 수 있다. 이동 기능 전체를 쓰려면 `wake_source_state` 발행이 별도로 필요하다.
- 위험: `dispatch` arm을 `background`로 보내면 같은 세션에서 두 개의 `extension_request`가 동시에 확장에 들어간다. 확장 핸들러가 상태를 공유하면 경쟁이 생긴다. 1차 구현은 허용하고, 필요하면 이름별 직렬화 큐를 나중에 둔다.
- 위험: 호스트 측 예산(`timeoutMs`)과 코어 측 예산이 둘 다 걸리면 어느 쪽이 먼저 끊기느냐에 따라 `code`가 `extension_timeout` 또는 `extension_failed`로 갈린다. 코어 예산을 더 짧게 두는 규약을 테스트로 고정해야 한다.
- 위험: 확장 기본 경로가 `~/.omp/agent/extensions`(`host.ts:512`)다. `~/.omo` 확장을 쓰려면 `--extension-dir` 상당의 경로 지정이 필요한데, 이번 조각 범위 밖이며 CLI 플래그가 없다.
- **미확인**: mhc가 `extension_request`에 자체 타임아웃을 거는지. `handle_rpc_request`(`runner.rs:1476`~`1503`)에는 타임아웃이 보이지 않아 게이트웨이의 30초가 유일한 상한으로 보이나, 상위 호출부를 끝까지 읽지 못했다.
- **미확인**: 게이트웨이가 `success:false` + `code`를 `MoveError("session_rpc")`로만 뭉뚱그리는지(`agentd-sessions.ts:113`~`115`), 코드별 분기를 두는지. 위 설계는 코드가 보존된다는 가정 위에 있다.
- 이번에 하지 못한 것: 실제 코드 작성·컴파일·테스트 실행은 규칙상 수행하지 않았고, 위 줄 번호는 현재 체크아웃(`e7a5378`) 기준 읽기만으로 확인했다. `docs/` 문서 추가와 conformance probe는 설계만 했고 작성하지 않았다.
