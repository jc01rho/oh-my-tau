# tau RPC 적합성 테스트 하네스 설계 (조각 21)

## 1. 현재 상태 (근거)

- 저장소는 `~/git/oh-my-tau`(depth 1 클론, version 0.12.0, `crates/*` 22개). `crates/omp-cli`는 `[[bin]] name = "omt"`만 있고 `[lib]`이 없다(`crates/omp-cli/Cargo.toml:6-8`). 따라서 통합 테스트는 라이브러리를 링크하지 못하고 `env!("CARGO_BIN_EXE_omt")`로 바이너리를 띄우는 방식만 가능하다(`crates/omp-cli/tests/print_slash.rs:19`, `crates/omp-cli/tests/login_e2e.rs:42`).
- RPC 진입점은 `run_with`(`crates/omp-cli/src/rpc.rs:1374`) 하나이고, 명령 해석은 `handle`(`rpc.rs:562`) → `dispatch`(`rpc.rs:593`)의 단일 `match kind` 다. 문자열 arm은 실측 57개, 미지 명령은 `rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어진다.
- 응답 프레임은 `response()`(`rpc.rs:203`)가 `id?, type:"response", command, success, data?|error?, code?` 순서로 만든다. 오류 타입은 `CmdError { message: String, code: Option<&'static str> }`(`crates/omp-cli/src/runtime.rs:34-37`), 코드 부여는 `CmdError::coded`(`runtime.rs:48`)뿐이고 실제 사용처는 `session_busy`(`rpc.rs:753`, `rpc.rs:1033`)와 `unknown_since`(`rpc.rs:800`) 세 곳이다.
- 기동 프레임은 `{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}`(`rpc.rs:1421-1426`, 상수 `rpc.rs:50`, `rpc.rs:199`). 이어 `announce_commands`(`runtime.rs:3739`)가 `available_commands_update`를 보내고, 동적 명령이 있으면 `set_available_commands`(`runtime.rs:3731`)가 다시 announce 한다(`rpc.rs:1428-1435`).
- 실행 모델: 읽기 루프가 `cmd_tx`로 워커에 넘기고 워커는 한 번에 하나씩 순서대로 처리한다(`rpc.rs:1441-1452`). 예외 셋만 읽기 루프가 직접 처리한다 — `extension_ui_response`(`rpc.rs:1478`), `bash`(백그라운드 `JoinSet`으로 분기, `rpc.rs:1491`), `abort`(busy일 때 인라인, `rpc.rs:1502`). `host_tool_result|host_tool_update|host_uri_result`는 버린다(`rpc.rs:1485-1490`).
- 전송 계층: `FrameEncoder`(`rpc.rs:1227`)가 v1에서 1 MiB 초과 응답을 `"RPC response exceeded the transport limit"` 오류 프레임으로 바꾸고(`rpc.rs:1542-1554`), `negotiate_protocol`의 `protocolVersion:2` 성공 응답을 본 뒤 `version = 2`로 전환(`rpc.rs:1346-1350`)해 256 KiB `rpc_chunk`로 쪼갠다(`rpc.rs:1098`, `rpc.rs:1310-1336`). `agent_end`는 이미 스트리밍된 메시지를 빼고 `messageCount`를 붙인다(`rpc.rs:1247-1265`).
- 종료 정산: stdin이 닫히면 워커 대기 → `background.join_next()` → `agent.wait_for_idle()` → `rt.wait_settled()`(`runtime.rs:3702`) → `FLUSH_MARKER`(`rpc.rs:135`)로 writer를 끝낸다(`rpc.rs:1508-1521`).
- 기존 테스트 자산: ① `rpc.rs` 내부 `mod tests`(`rpc.rs:1533`, 5개 테스트)와 `mod ask_ui_tests`(`rpc.rs:1701`, 2개) — 전부 순수 함수/`FrameEncoder`/`RpcAskUi` 단위다. ② `tests-golden/capture.ts`(시나리오 `Step[]`을 spawn으로 재생, `capture.ts:14-88`)와 `compare.ts`(shape 붕괴 후 커맨드별 응답 diff, 이벤트 타입 순서 비교, `compare.ts:60-106`). ③ `scripts/check-golden.ts`가 baseline 카운트로 게이트하고(`justfile:55-56`, `.github/workflows/ci.yml:44`). ④ `tests-e2e/ext/conformance.py`(rpc 모드 포함 419줄), `tests-e2e/tui/run.py`(`SCENARIOS` 딕셔너리 `run.py:786`, mock provider `tests-e2e/tui/mock_provider.py:188`).
- 결정적 결함 셋. 첫째, `tests-golden/capture.ts`는 `sleep(1500)`/`sleep(200)`/`sleep(500)`(`capture.ts:145,155,158`)로 프레임을 기다린다 — 고정 대기라 부하에 따라 골든이 흔들린다. 둘째, `tests-e2e/ext/conformance.py`는 `stderr=STDOUT`으로 합쳐(`conformance.py:140,198`) 파싱 오염 위험이 있고, `pump(until substring)`(`conformance.py:206-215`)도 시간 기반이다. 셋째, 두 하네스 모두 `justfile`/CI 게이트에 없다(`tests-e2e/ext` 미등록 확인). 즉 RPC 계약을 상시 검증하는 장치는 지금 없다.

## 2. 게이트웨이가 요구하는 계약

게이트웨이(omo-nyang)는 mhc를 stdio JSON-lines로 구동하며, 계약의 기준은 mhc다. 실측한 요구와 tau 현재 상태:

| 명령/신호 | 게이트웨이 사용처 | mhc 응답 | tau 현재 |
|---|---|---|---|
| `set_client_info` | `src/lane-hold.ts:29` (`width:80, capabilities:["interactive_user"]`) | `Success{data:None}` (`maho-rpc/src/connection_handler.rs:1543`) | 없음 → `Unknown command` |
| `interrupt` | `src/lane-hold.ts:30` (`intent:"stop"`) | `{"interrupted":bool}` (`connection_handler.rs:1296-1297`) | 없음. `abort`(`rpc.rs:677`)는 데이터 없는 `none()` |
| `get_protocol_info` | `src/lane-hold.ts:60` (`serverVersion` 파싱) | `protocolVersion`,`serverVersion`,`capabilities`,`mode:"classic"` (`connection_handler.rs:1546-1561`, `CLASSIC_CAPABILITIES` 17종 `:723-741`) | 없음. `ready`의 `protocolVersion`만 존재 |
| `reload` | `src/lane-hold.ts:64` (`{cancelled}`) | — | 없음 |
| `resume_session` | `src/lane-hold.ts:66-70` | `not_paused` 사유(`maho-core/src/agent_session.rs:6461`) | 없음. `get_state`에 `paused`/`runPaused` 필드 자체가 없다(`runtime.rs:1961-1983`) |
| `release_session` | 게이트웨이 미호출 | mhc도 `Unknown command: release_session` (`connection_handler.rs:1565`) | 없음(동일 동작) |
| `extension_request` | `src/worker-rpc.ts:147` (`memory_report`) | 이름/페이로드 검증 후 위임 (`connection_handler.rs:1532-1540`) | 없음 |
| `move_session` | `src/session-move.ts:41` (omo 5.1.44+) | — | 없음. `ext-host/host.ts`·`crates/omp-ext/src/host.rs`에 `session_move_export/import` 없음(grep 0) |
| `agent_settled` | `src/gateway.ts:2332-2334` (`seen.type==="agent_settled"`) | 확장 이벤트로 전달 | stdout 프레임 아님. `rpc.rs` 내 발생 0회, 확장 이벤트로만 `ext.rs:1593-1597`에서 `has_handlers("agent_settled")`일 때 emit |
| 응답 봉투 | 게이트웨이 파서 | `RpcResponse{id?, type:"response", command, session_id?, flatten Success{data}|Error{error,error_code,error_data}}` (`rpc_types.rs:1054-1079`) | `session_id` 없음, 코드 키가 `code`(`rpc.rs:222`) |

부족 기능 5개 범주를 이 조각의 검증 축으로 고정한다. (1) RPC 명령 8종 부재, (2) CLI 플래그 — `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`가 `STRING_FLAGS`/`OPTIONAL_FLAGS`/`VALUELESS_FLAGS`(`cli.rs:97-165`)에서 보이지 않음(직접 확인한 구간은 `97-122`, `138`, `142-165`), (3) 확장 호스트 `api.rpc.handle`·`session_move_*` 부재, (4) 도구 표면(`task_send/task_output/task_cancel`, `bash_output/kill_bash/monitor`, `memory/memory_apply_patch`, workpool) 부재, (5) 설정 루트(`~/.omo` vs `~/.omp`)·정착 신호(`agent_settled` vs `agent_end`)·세션 파일 형식(omp JSONL v3 vs senpi) 불일치. 하네스는 이 다섯 축을 각각 별도 스위트로 검증한다.

## 3. 설계

새 통합 테스트 한 파일 `crates/omp-cli/tests/rpc_conformance.rs`와 픽스처/골든 디렉터리 두 개로 구성한다. 라이브러리가 없으므로 하네스는 전부 프로세스 밖에서 돈다.

```
crates/omp-cli/tests/rpc_conformance.rs   # 하네스 + 스위트 5개
crates/omp-cli/tests/fixtures/rpc/*.json  # 시나리오(전송 프레임 시퀀스)
tests-rpc-golden/rs/*.jsonl               # 정규화된 골든(수신 프레임)
tests-rpc-golden/baseline.txt             # 허용 diff 카운트
scripts/check-rpc-golden.ts               # baseline 게이트
```

하네스 코어(모든 스위트가 공유):

```rust
struct Harness { child: std::process::Child, stdin: std::process::ChildStdin,
                 rx: std::sync::mpsc::Receiver<Value>, seq: usize }
impl Harness {
    fn spawn(home: &Path, args: &[&str]) -> Self;          // CARGO_BIN_EXE_omt, --mode rpc, --no-session
    fn send(&mut self, frame: Value) -> (String, usize);   // id 부여, 전송 직전 인덱스 반환
    fn expect_response(&mut self, id: &str, ms: u64) -> Value;
    fn expect_frame(&mut self, from: usize, pred: impl Fn(&Value) -> bool, ms: u64) -> Value;
    fn close_and_drain(self, ms: u64) -> Vec<Value>;
}
```

핵심 규율 셋. 첫째, **구독 선행**: `send`가 전송 전에 현재 `seq`를 돌려주고, `expect_frame`은 `from` 이후 프레임만 본다. `capture.ts:145`의 `sleep(1500)` 같은 고정 대기는 쓰지 않고, 조건 술어 + 상한 타임아웃만 쓴다. 둘째, **stdout/stderr 분리**: stdout만 파싱하고 stderr는 실패 메시지에만 싣는다(`conformance.py:140`의 합침을 따르지 않는다). 셋째, **정규화 후 비교**: `sessionId`, `sessionFile`, `cwd`, 타임스탬프, 임시 HOME 경로를 `<ID>`/`<HOME>`으로 치환하고(`conformance.py:265-270` 방식), 남은 값은 `compare.ts:15-33`의 shape 붕괴로 타입만 비교한다.

스위트 5개.

**S1 계약표(ratchet).** `const REQUIRED: &[(&str, &str)]`에 게이트웨이가 보내는 명령과 최소 유효 프레임을 나열하고, 각각에 대해 "성공 응답" 또는 "`Unknown command: <name>`" 중 하나를 기대값으로 고정한다. 지금은 8종이 후자로 통과하고, 하나를 구현하는 커밋이 그 줄을 전자로 뒤집는다. `agent_settled`는 별도 단언으로 "RPC 스트림에 나타나지 않는다"를 잠근다 — 현재 `rpc.rs` 발생 0회이므로 이 단언이 곧 게이트웨이 미충족의 회귀 방지선이다.

**S2 와이어 골든 왕복.** 픽스처 JSON을 순서대로 보내고 수신 프레임 전부를 정규화해 `tests-rpc-golden/rs/<name>.jsonl`과 비교한다. 골든은 tau 자기 자신을 오라클로 삼는다(mhc 오라클은 없다). `check-rpc-golden.ts`는 `check-golden.ts`와 같은 방식으로 "differing item(s)" 카운트가 baseline을 넘으면 실패하고, `response missing`/`missing capture`는 카운트와 무관하게 하드 실패로 둔다.

**S3 순서·동시성.** 벽시계가 아니라 프레임 인덱스로 단언한다. (a) `prompt` 응답이 `prompt_result`보다 앞선다(`rpc.rs:602-651`의 `none()` + spawn 순서). (b) busy 중 `abort`가 큐를 건너뛰고 인라인 응답한다(`rpc.rs:1502`) — `prompt` 전송 직후 `agent_start`를 기다렸다가 `abort`를 보내고, `abort` 응답 인덱스가 `agent_end`보다 작음을 확인한다. (c) `bash`는 워커를 막지 않는다(`rpc.rs:1491`): `prompt`(느린 mock) 진행 중 `bash`를 보내 두 응답이 모두 오고 `bash` 응답이 `agent_end`보다 앞설 수 있음을 허용하되, `get_state` 같은 워커 명령은 `bash` 뒤에 도착해도 `bash` 응답을 기다리지 않음을 확인한다. (d) 워커 직렬성: 서로 다른 두 `get_state`를 연속 전송하면 응답 인덱스 순서가 전송 순서와 같다(`rpc.rs:1441-1452`).

**S4 프레임 한계·청킹.** (a) v1에서 1 MiB 초과 응답은 `success:false` + `"RPC response exceeded the transport limit"`(`rpc.rs:1542-1554`). (b) `negotiate_protocol{protocolVersion:2}` 성공 후 같은 요청이 `rpc_chunk` 여러 줄로 오고, `index`/`count`/`byteLength`가 일관되며 base64를 이어붙인 바이트가 원 프레임과 같다(`rpc.rs:1310-1336`). (c) `agent_end` 압축: `message_end`로 이미 보낸 메시지는 빠지고 `messageCount`는 전체 수다(`rpc.rs:1247-1265`). (d) stdin 종료 시 모든 프레임이 flush되고 exit code 0(`rpc.rs:1508-1521`).

**S5 실패 코드 매트릭스.** 명령별로 (입력, 기대 `error` 문자열, 기대 `code`)를 표로 고정한다: `parse` 실패는 `command:"parse"`에 `id` 없음(`rpc.rs:1460-1475`), `negotiate_protocol` 버전 불일치 문구(`rpc.rs:595-601`), `set_event_filter`의 `messageUpdates` 검증(`rpc.rs:846-856`), `goal` 검증 4종(`rpc.rs:257-278`), `set_host_tools`/`set_host_uri_schemes` 검증(`rpc.rs:281-341`), `session_busy`(`rpc.rs:753`,`1033`), `unknown_since`(`rpc.rs:800`). 코드가 붙지 않아야 하는 일반 `CmdError`는 `code` 키 부재를 단언한다. 봉투 차이(`session_id` 부재, `code` vs `error_code`)는 실패가 아니라 `EXPLAINED` 목록(`conformance.py:34-52` 방식)에 사유와 함께 등록해 게이트웨이 어댑터가 흡수할 항목임을 명시한다.

## 4. 구현 단계 (커밋 단위)

1. `test(rpc): conformance harness skeleton` — `Harness`(spawn/send/expect_response/expect_frame/close_and_drain), 정규화·shape 유틸, `state` 시나리오 1개. 제품 코드 변경 없음.
2. `test(rpc): lock the gateway command contract` — S1 계약표와 `agent_settled` 부재 단언. 8종 누락이 "현재 상태"로 고정된다.
3. `test(rpc): golden replay and baseline gate` — S2 + `tests-rpc-golden/`, `scripts/check-rpc-golden.ts`, `justfile`의 `rpc-conformance` 레시피, `ci.yml` parity job 연결.
4. `test(rpc): ordering and concurrency assertions` — S3 4개 시나리오(prompt/abort, bash 병행, 워커 직렬성, prompt_result 순서).
5. `test(rpc): frame limit and chunk reassembly` — S4 4개 시나리오.
6. `test(rpc): failure code matrix` — S5 표 + `EXPLAINED` 목록 + baseline 갱신.

각 커밋은 `cargo test -p omp-cli --test rpc_conformance`가 단독으로 통과해야 하고, 3번 커밋부터 `just rpc-conformance`가 CI 게이트가 된다.

## 5. 테스트

- 단위(`rpc.rs` `mod tests`, `mod ask_ui_tests`): `response()` 키 순서, `FrameEncoder` v1/v2, `compact_terminal`, 검증 헬퍼(`goal`, `set_host_tools`, `text_cursor`). 이미 있는 5+2개를 유지하고 새 하네스는 여기에 넣지 않는다 — 프로세스가 필요하기 때문이다.
- 통합(`crates/omp-cli/tests/rpc_conformance.rs`): S1~S5. 각 테스트는 임시 HOME(`tempfile::tempdir`)에 `models.yml`을 쓰고 `baseUrl: http://127.0.0.1:9/v1`로 두어 실제 공급자 호출을 차단한다(`print_slash.rs:8-17` 방식). 프롬프트가 필요한 시나리오만 `mock_provider.serve`(`mock_provider.py:188`)를 띄운 로컬 포트를 쓰고, 스크립트 응답은 `mock_provider.script`(`mock_provider.py:73`)의 `mock/text`·`mock/tool` 규약을 그대로 재사용한다.
- 비결정성 금지: 고정 sleep 없음. 모든 대기는 "전송 전 인덱스 기록 → 술어 대기 → 상한 타임아웃"이며, 실패 시 수신 프레임 전체를 덤프해 원인을 남긴다. 타임아웃은 시나리오 파일의 `timeoutMs`로 명시하고 기본값을 두지 않는다.
- 실행: `cargo test -p omp-cli --test rpc_conformance`(단일 실행 통과가 조건), 게이트는 `just rpc-conformance`.

## 6. QA 시나리오

| # | 시나리오 | 입력 | 기대 |
|---|---|---|---|
| Q1 | 기동 | 없음 | `ready` 1건(`protocolVersion:1`, `supportedProtocolVersions:[1,2]`) 후 `available_commands_update` 1건 이상, 그 뒤 `get_state` 응답 |
| Q2 | 미지 명령 | `{"type":"no_such_command","id":"x"}` | `success:false`, `error:"Unknown command: no_such_command"`, `code` 없음 |
| Q3 | 게이트웨이 8종 | `interrupt`/`set_client_info`/`get_protocol_info`/`reload`/`resume_session`/`extension_request`/`move_session`/`release_session` | 현재 전부 `Unknown command: <name>` (S1 ratchet) |
| Q4 | 정착 신호 | `prompt` → `agent_end`까지 | `agent_end`와 `session_settled`는 오고 `agent_settled`는 오지 않음 |
| Q5 | 순서 | `prompt`(mock) 직후 `abort` | `prompt` 응답 → `abort` 응답 → `agent_end` 순, `abort` 응답이 `agent_end`보다 앞 |
| Q6 | 병행 bash | `prompt`(느린 mock) + `bash: echo probe` | 두 응답 모두 도착, 워커 명령 응답이 `bash` 응답을 기다리지 않음 |
| Q7 | v1 한계 | 1 MiB 초과 응답 유발 | `"RPC response exceeded the transport limit"` |
| Q8 | v2 청킹 | `negotiate_protocol{2}` 후 같은 요청 | `rpc_chunk` 다중 줄, 재조립 결과가 원 프레임과 동일 |
| Q9 | 파싱 실패 | 비JSON 줄, JSON 배열 | `command:"parse"`, `id` 없음, 프로세스 생존 |
| Q10 | 종료 정산 | 시나리오 후 stdin close | 남은 프레임 전부 flush, exit code 0, 유실 없음 |

## 7. 위험과 미확인

- **오라클 부재**: 골든은 tau 자기 출력만 비교한다. mhc 계약과의 차이(`session_id` 부재, `code` vs `error_code`, `get_state`의 `paused`/`runPaused` 부재)는 S5의 `EXPLAINED`로 문서화할 뿐 자동 판정하지 못한다. mhc를 오라클로 세우려면 mhc 바이너리와 동일 시나리오 재생이 필요하고, 이는 이 조각 범위 밖이다.
- **mock 의존**: 프롬프트 시나리오는 `tests-e2e/tui/mock_provider.py`에 의존한다. 이 파일은 현재 `justfile`/CI 게이트에 없어 드리프트 시 하네스가 조용히 깨질 수 있다. 하네스 도입 커밋에서 mock을 고정하거나 Rust 쪽 최소 HTTP 스텁으로 대체해야 한다(권고: 후자, Python 의존 제거).
- **확장 호스트 잡음**: 확장이 켜지면 `available_commands_update`가 중복 송신된다(`tests-golden/rs/state.jsonl`에 3회 관측). 정규화에서 중복 제거 규칙을 정해야 하며, `--no-extensions`로 끄는 것을 기본으로 하고 확장 시나리오만 켠다.
- **CLI 플래그 축 미검증**: `--multi-session` 등 4종의 부재는 `cli.rs:97-165` 중 `97-122`, `138`, `142-165`만 직접 읽어 확인했다. `STRING_FLAGS`의 잔여 구간(`123-136`)은 확인하지 못했으므로 "없다"를 확정하지 않는다. 이 축은 S1과 별도 스위트(플래그 파싱 표 기반)로 분리해야 한다.
- **도구·설정·세션 형식 축**: 도구 표면 4종 부재, `~/.omo` vs `~/.omp`, omp JSONL v3 vs senpi 형식은 이 조각에서 코드 근거를 확보하지 못했다. 하네스 골격만 공유하고 각 축의 검증 내용은 해당 조각의 설계를 따른다. **확인 필요.**
- **슬랙 1795326973.474349의 5개 범주 원문**: 이 세션에서 해당 메시지를 열람하지 못했다. 위 5축은 공통 맥락에 적힌 항목으로 재구성한 것이며, 원문과 항목이 어긋나면 이 절과 2절 표를 그 기준으로 고쳐야 한다. **확인 필요.**
- **미실행**: `cargo test`, `cargo build`, `bun scripts/check-golden.ts`를 실행하지 않았다. 저장소는 읽기만 했고 수정·커밋하지 않았다. 따라서 본 문서의 모든 실행 관련 서술은 코드 정독 근거이며 실행 증거는 없다.
