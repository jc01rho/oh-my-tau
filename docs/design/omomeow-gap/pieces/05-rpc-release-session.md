# RPC `release_session` 명령 추가 설계 (이동 후 원래 장비가 세션을 놓는 경로)

## 1. 현재 상태 (근거)

- RPC 디스패치는 단일 match다. `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`, 미지 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. `release_session` 분기가 없어 게이트웨이가 받는 응답은 `success:false, error:"Unknown command: release_session"`이다.
- 세션 잠금은 없다. `crates/omp-session/src/index.rs:90` `pub struct SessionIndex`는 `index.rs:118` `SessionIndex::load`에서 파일을 한 번 스캔해 메모리 링크만 만들고, `index.rs:161` `branch()`로 활성 가지만 계산한다. 디스크 인덱스 파일을 쓰지 않는다(`crates/omp-session/src/scan.rs:30` `FileBuf`는 `scan.rs:22` `MMAP_THRESHOLD` 초과 시 읽기 전용 mmap). 저장소 전체에서 파일 잠금은 `crates/omp-stats/src/aggregator/lock.rs:8`(통계 동기화)과 `crates/omp-ext/src/host.rs:891`(확장 데몬 기동 직렬화, `host.rs:962` `try_lock`)뿐이고, 세션 jsonl 옆에 `.lock`을 두는 코드는 없다.
- 세션 인덱스/레지스트리도 없다. `crates/omp-cli/src/persist.rs:63` `sessions_dir`는 `~/.omp-rs/sessions/<slug>` 경로만 계산하고, 목록은 `crates/omp-cli/src/subcmd/web/sessions.rs:119` `list_sessions`가 `read_dir`로 매번 훑는다. 소유자 테이블이나 세션 DB가 없다.
- 쓰기 핸들은 프로세스 안에만 있다. `persist.rs:89` `pub struct Persistence`가 `writer: Arc<Mutex<SessionWriter>>`를 들고, `crates/omp-session/src/lib.rs:190` `SessionWriter`가 append 모드 `File`을 소유한다(`lib.rs:208` `create`, `lib.rs:257` `open`, `lib.rs:765` `write_value`가 `write_all`). 닫기 공개 API는 없고 `sync_data`는 타이틀 슬롯 기록에만 쓰인다(`lib.rs:481`, `lib.rs:492`).
- 리스너가 writer 사본을 쥔다. `persist.rs:543-560`의 `agent.subscribe` 리스너가 writer 클론을 잡고 `MessageEnd`마다 append하며(`persist.rs:583`, `persist.rs:600`), `persist.rs:619-620` 주석대로 "Dropping a Subscription does not unsubscribe"라 프로세스 수명 내내 살아 있다(`vendor/pa-agent/src/agent.rs:284-294`의 `Subscription::unsubscribe`를 아무도 호출하지 않는다). 그래서 파일 핸들 정리는 Drop이 아니라 상태 플래그로 해야 한다.
- `Runtime`은 `persist: Persistence`를 소유하고(`crates/omp-cli/src/runtime.rs:120`), `Runtime` 자체가 `Arc`로 프로세스 종료까지 산다(`rpc.rs:1384` `Runtime::start`, `rpc.rs:1518` `drop(rt)`).
- 확장 상태는 이중으로 붙잡힌다. `Runtime`의 `ext_host: Mutex<Option<Arc<omp_ext::ExtHost>>>`(`runtime.rs:180`, 설정 `runtime.rs:2588` `set_ext_host`)와 프로세스 전역 `static EXTENSIONS: OnceLock<Arc<Ext>>`(`crates/omp-cli/src/slash.rs:65`, 등록 `slash.rs:71`). 종료 경로는 `crates/omp-cli/src/ext.rs:1209` `Ext::shutdown`이 `session_shutdown`을 emit하고 `host.shutdown()`을 부른다(`ext.rs:1210`, `ext.rs:1219`; `host.rs:696`). 공유 데몬에서도 이 shutdown은 세션 단위다(`ext-host/host.ts:1054-1057`, `ext-host/shard.ts:101`).
- 이동 관련 이벤트는 없다. `crates/` 전체에서 `session_move` 심볼 0건, `ext-host/`에 `api.rpc`/`session_move_export` 0건.
- mhc 쪽 계약 실측: 데몬 문자열 테이블에 `release_session`이 `open_session`/`unload_session`/`attach_session`/`reclaim_session`/`resume_session`과 나란히 있고, 오류 코드 `release_unsupported`, `invalid_release_reason`, `session_id_not_plain`, `release_failed`, 메시지 "release_session handed this session to another runtime; deliver there instead (admission is closed here)", 필드 `move_id`, `reason`, 프레임 `session_released`가 있다(mhc 바이너리 오프셋 17061069, 17145311, 17247154). 다만 인프로세스 `mhc --mode rpc`에 같은 명령을 보내면 `{"command":"release_session","success":false,"error":"Unknown command: release_session"}`가 돌아온다(직접 실행). 계약 표면이 데몬 소켓과 인프로세스 RPC로 갈린다.
- tau의 세션 id는 `uuid_v7()`(`persist.rs:39`, `persist.rs:421`)이고 파일명은 `<stamp>_<id>.jsonl`(`lib.rs:211`)이라 파일 stem과 id가 다르다. `omp-urls`의 `session_id()`는 stem을 쓴다(`crates/omp-urls/src/lib.rs:48-52`).

## 2. 게이트웨이가 요구하는 계약

- 요청: `{"id":…, "type":"release_session", "sessionId":"<plain id>", "reason":"…"?, "move_id":"…"?}`. 경로 문자열은 `session_id_not_plain`로 거절한다(mhc 문자열 근거).
- 성공: `success:true` + `data`에 최소 `{"released":true,"sessionId":…,"sessionFile":<path|null>,"alreadyReleased":false}`. 이후 이 프로세스는 해당 세션에 대해 어떤 프레임도 더 내보내지 않는다.
- 실패 코드: `unknown_session`(id 불일치), `session_busy`(턴/큐/자식 작업 진행 중), `session_closing`(동시 해제 진행), `release_unsupported`(`--no-session` 등 파일 없는 세션 강제 해제), `release_failed`(flush/close 실패).
- 의미: 원 장비는 파일을 닫고 확장 상태를 정리하되, 같은 프로세스·같은 확장 데몬·같은 통계 락을 공유하는 다른 세션에는 아무 영향도 주지 않는다.

## 3. 설계

### 3.1 와이어 처리 (`crates/omp-cli/src/rpc.rs`)

`dispatch`의 `"set_session_name"` 근처(`rpc.rs:1014`)에 분기를 추가한다.

```rust
"release_session" => {
    let sid = str_field(cmd, "sessionId")?;
    if sid.contains('/') || sid.ends_with(".jsonl") {
        return Err(CmdError::coded("sessionId must be a plain session id", "session_id_not_plain"));
    }
    let reason = cmd.get("reason").and_then(Value::as_str).unwrap_or("handover");
    let move_id = cmd.get("move_id").and_then(Value::as_str);
    rt.release_session(sid, reason, move_id).await
}
```

`ok`/`none` 헬퍼(`rpc.rs:435`, `rpc.rs:440`)와 `response`(`rpc.rs:203`)는 그대로 쓴다. 응답 뒤에는 `agent_end`/`session_settled`를 포함해 아무 프레임도 보내지 않는다.

### 3.2 `SessionWriter::close` (`crates/omp-session/src/lib.rs`)

`file: File`을 `file: Option<File>`로 바꾸고 `lib.rs:305` `path()`는 유지, 추가:

```rust
pub fn sync(&mut self) -> Result<()>     // file.as_ref().sync_data()
pub fn close(&mut self) -> Result<()>    // sync 후 file = None
pub fn is_closed(&self) -> bool
```

`write_value`(`lib.rs:765`)는 `file`이 `None`이면 `anyhow::bail!("session released")`. 리스너가 쥔 writer 클론(`persist.rs:543-560`)이 살아 있어도 해제 후 append가 조용히 사라지지 않고 오류로 드러난다.

### 3.3 `Persistence::release` (`crates/omp-cli/src/persist.rs`)

```rust
pub struct ReleaseReport { pub session_id: String, pub session_file: Option<PathBuf>, pub already: bool }
pub fn release(&self, reason: &str) -> Result<ReleaseReport>
```

- `writer.lock()`으로 `sync()` → `close()`. 이미 닫혔으면 `already: true`로 성공(멱등).
- `ephemeral`이면 `session_file: None`을 돌려주고 scratch 디렉터리는 건드리지 않는다(Drop 정리 `persist.rs:111-116`은 유지).
- `persist.rs:121` `session_id()`(uuid_v7)와 비교한다. stem 기반 `omp-urls` 값과 섞지 않는다.
- `released: AtomicBool`을 두고 `append_*` 계열이 `Err`를 내게 한다.

### 3.4 `Runtime::release_session` (`crates/omp-cli/src/runtime.rs`)

순서를 고정한다.

1. `sid != self.persist.session_id()` → `CmdError::coded(…, "unknown_session")`.
2. 이미 해제됨(`released` 플래그) → `ok({"released":true,"alreadyReleased":true,…})`.
3. 진행 중 표시(`releasing: AtomicBool` compare_exchange) 실패 → `session_closing`.
4. 바쁨 검사: `self.agent.is_streaming().await`(`runtime.rs:1670`), `self.queued_count() != 0`(`runtime.rs:570`), `lock(&self.post_turn_task).is_some()`(`runtime.rs:138`), `lock(&self.settings).running_bash.is_some()`(`runtime.rs:3805` `abort_bash`가 쓰는 슬롯) → `session_busy`. 강제 옵션은 두지 않는다(게이트웨이는 정상 종료 뒤에만 부른다).
5. `self.cancel_compaction_and_wait().await`(`runtime.rs:1821`), `self.cancel_retry_and_wait().await`(`runtime.rs:1798`) — 해제 뒤 늦은 쓰기를 막는다.
6. 확장 정리: `if let Some(ext) = crate::slash::extensions()` → `ext.shutdown().await`(`ext.rs:1209`). `session_shutdown`(`crates/omp-ext/src/lifecycle.rs:306`, `lifecycle.rs:501-502`)을 한 번 emit하고 `ExtHost::shutdown`(`host.rs:696`)으로 이 세션의 데몬 연결만 닫는다. 이어서 `self.set_ext_host(None)`(`runtime.rs:2588`).
7. `self.persist.release(reason)?` — flush/close. 실패 시 `release_failed`.
8. `omp_urls::set_global_session_file(None)`(`crates/omp-urls/src/lib.rs:58`) — 해제된 파일을 URL 해석 기본값으로 남기지 않는다(`publish_session_file` `runtime.rs:538`의 반대 동작).
9. `self.released.store(true)`; `ok(ReleaseReport)`.

### 3.5 확장 전역 슬롯 (필수 선행 리팩터)

`static EXTENSIONS: OnceLock<Arc<Ext>>`(`slash.rs:65`)는 지울 수 없어 `Ext`(= `ExtHost` 포함)가 프로세스 종료까지 살아남고 공유 데몬 쪽 Worker도 살아남는다. `RwLock<Option<Arc<Ext>>>`로 바꾸고 `slash::clear_extensions()`를 추가해 6단계 뒤에 호출한다. `slash::extensions()`(`slash.rs:75`) 시그니처는 유지한다.

### 3.6 다른 레인에 영향 없음 보장

- 프로세스 전역 자원은 건드리지 않는다: LSP 레지스트리 정리는 종료 경로 전용 `shutdown_lsp`(`crates/omp-cli/src/main.rs:702`)만 하고 release 경로에서는 호출하지 않는다. 통계 락(`omp-stats/src/aggregator/lock.rs:8`), 확장 데몬 기동 락(`host.rs:891`)도 그대로 둔다.
- 확장 데몬 shutdown은 세션 단위다(`ext-host/host.ts:1054-1057`, `ext-host/shard.ts:101`) — 같은 소켓의 다른 Worker는 유지된다.
- 세션 잠금이 없으므로 "잠금 해제" 단계 자체가 없다(1절 근거). 대신 해제 후 append를 거부해 이중 쓰기를 막는다.
- 해제된 런타임은 이후 `prompt`(`runtime.rs:1649`), `get_state`(`runtime.rs:1953`)에서 `released`를 보고 각각 `session_released`, `"released": true`를 낸다.

## 4. 구현 단계 (커밋 단위)

1. `omp-session`: `SessionWriter.file: Option<File>`, `sync`/`close`/`is_closed`, `write_value` 가드, 단위 테스트. (`crates/omp-session/src/lib.rs`)
2. `omp-cli/persist`: `released` 플래그, `ReleaseReport`, `Persistence::release`, append 계열 가드.
3. `omp-cli/runtime`: `released`/`releasing` 필드, `Runtime::release_session`, `prompt`·`get_state` 가드.
4. `omp-cli/slash`: `EXTENSIONS`를 `RwLock<Option<_>>`로, `clear_extensions()` 추가.
5. `omp-cli/rpc`: `release_session` 분기 + 오류 코드 상수(`session_busy`, `unknown_session`, `session_closing`, `release_unsupported`, `release_failed`, `session_id_not_plain`).
6. 문서/CLI: `--mode rpc` 명령 목록에 추가, `agent_settled`(`ext.rs:1593`) 이후 release 순서를 게이트웨이 문서에 명시.

## 5. 테스트

- `crates/omp-session/src/lib.rs` `mod tests`: `close()` 후 `append_message`가 오류, `path()` 유지, `close()` 두 번 호출 멱등.
- `crates/omp-cli/src/persist.rs` 테스트: `attach_in`으로 만든 `Persistence`에 append → `release()` → 파일 마지막 줄이 방금 메시지(동기화 확인), 두 번째 `release()`는 `already: true`, scratch 세션은 `session_file: None`.
- `crates/omp-cli/src/runtime.rs` `Harness`(`runtime.rs:5200`): 잘못된 id → `unknown_session`; 스트리밍 중 release → `session_busy`; 정착 후 release → 성공하고 `get_state()["released"] == true`; 해제 후 `prompt` → `session_released`. 대기는 고정 sleep이 아니라 채널 수신 + `tokio::time::timeout`.
- `crates/omp-cli/src/rpc.rs` `mod tests`(`rpc.rs:1533`): `dispatch`에 `{"type":"release_session","sessionId":"<path>"}` → `session_id_not_plain`, 미지 id → `unknown_session`.
- 확장: `crates/omp-ext/tests/fixtures/lifecycle.ts`에 `session_shutdown` 핸들러를 두고 release 1회당 정확히 1회 emit, 같은 데몬의 두 번째 세션은 살아 있음을 확인.

## 6. QA 시나리오

1. A 장비가 턴 종료(`agent_end` → `session_settled`) 후 `release_session`을 보내 `released:true`를 받는다. A의 jsonl 마지막 줄이 직전 어시스턴트 메시지이고 추가 append가 없다.
2. B 장비가 같은 파일을 `open_session`/`resume_session`으로 열어 이어 쓴다. A에서 늦게 도착한 프레임이 파일을 오염시키지 않는다.
3. 턴 진행 중 release → `session_busy`, 게이트웨이는 정착 후 재시도.
4. 동시 release 두 건 → 하나는 `released:true`, 다른 하나는 `alreadyReleased:true` 또는 `session_closing`.
5. 존재하지 않는 sessionId, 경로 문자열 → 각각 `unknown_session`, `session_id_not_plain`.
6. `--no-session` 세션 → `released:true`, `sessionFile:null`, 프로세스는 계속 동작.
7. 같은 확장 데몬 소켓을 쓰는 두 세션 중 하나만 release → 다른 세션의 확장 툴 호출과 `session_*` 이벤트가 정상.
8. release 후 같은 프로세스에 `get_state`/`prompt` → `released:true`, `session_released`.
9. 확장 `session_shutdown` 핸들러가 release당 1회 실행되고, 데몬 프로세스는 남은 세션이 있으면 종료되지 않는다.

## 7. 위험과 미확인

- 계약 표면 불일치(가장 큰 미확인): `release_session`은 mhc 데몬 문자열 테이블에만 있고 인프로세스 `mhc --mode rpc`는 Unknown command로 거절한다(실측). 데몬을 띄워 성공 응답 스키마를 확인하려 했으나 `mhc host ensure`가 "RPC socket host exited before ready (exit status: 1)"로 실패해 데몬 경로는 검증하지 못했다. 따라서 2절의 성공 응답 필드 집합은 문자열 테이블의 `session_released`, `move_id`, `reason`에서 역산한 것이며 실제 스키마 확인이 필요하다.
- 게이트웨이가 tau를 stdio로 띄우는지 데몬 소켓으로 쓰는지 확인하지 못했다. 데몬 경로라면 tau에도 세션 레지스트리와 소켓 계층이 별도로 필요하다.
- 교차 프로세스 잠금 부재: tau에 세션 파일 잠금이 없어(1절) release가 원자적 소유권 이전을 보장하지 못한다. A가 close하기 전 B가 열면 동시 append가 가능하다. `sessionPath.lock` + `File::try_lock`(`host.rs:962`에서 이미 사용) 도입은 후속 과제로 남겼다.
- `EXTENSIONS` 리팩터 파급: `slash.rs:65`를 `RwLock`으로 바꾸면 `extensions()` 호출부(예: `rpc.rs` `input_gate`)의 수명 가정을 다시 봐야 한다. 반환 타입을 `Option<Arc<Ext>>`로 바꾸는 편이 안전하나 호출부 시그니처 변경이 따른다. 호출부 전수 조사는 하지 못했다.
- 해제 후 프로세스 수명: `rpc.rs:1452-1510` 루프는 stdin이 닫혀야 끝난다. 게이트웨이가 release 후 프로세스를 회수하지 않으면 `wait_settled`(`runtime.rs:3702`)와 writer 태스크가 남는다. `"exitAfter"` 힌트나 SIGTERM 규약을 정해야 한다.
- 리스너 잔존: `persist.rs:619-620`대로 구독이 해제되지 않아 해제된 런타임이 `AgentEvent`를 계속 받아 `write_value` 오류를 `eprintln`할 수 있다(`persist.rs:583-605`). 오류 1회 억제 플래그가 필요하다.
- 이동 이벤트 부재: `session_move_export`/`import`가 tau에 없어(1절) 확장이 이동을 관찰할 수 없다. release는 `session_shutdown`만 emit하므로, 확장이 이동을 구분해야 한다면 별도 이벤트 타입 추가가 선행돼야 한다.
- 코드 작성·커밋은 하지 않았다(작업 규칙). 위 커밋 단위는 계획이며 실제 컴파일·테스트 실행은 없다.
