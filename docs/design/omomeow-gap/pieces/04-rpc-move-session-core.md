# RPC move_session (장비 간 세션 이동) 핵심 설계

## 1. 현재 상태 (근거)

- RPC 디스패치는 `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt, cmd, kind) -> CmdResult` 의 단일 `match kind` 이고, 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` 로 떨어진다. `move_session` 분기는 없다.
- 응답 프레임은 `rpc.rs:203` `fn response(id, command, result)` 가 `{id?, type:"response", command, success, data?|error?, code?}` 순서로 만든다. 오류 코드는 `CmdError.code: Option<&'static str>` (`crates/omp-cli/src/runtime.rs:34`, `coded()` `runtime.rs:48`) 하나뿐이라 mhc의 `errorCode`/`errorData` 쌍은 표현되지 않는다.
- 명령은 도착 순서대로 워커 태스크에서 하나씩 처리된다(`rpc.rs:1436` 주석, 큐잉 루프 `rpc.rs:1490` 부근). move_session 안에서 긴 파일 복사를 돌리면 다른 명령이 전부 막힌다.
- 세션 파일은 omp JSONL v3: `crates/omp-session/src/lib.rs:25` `SESSION_VERSION: u32 = 3`, 헤더는 `lib.rs:225` `{"type":"session","version":3,"id":..,"timestamp":..,"cwd":..}`. 쓰기는 `SessionWriter`(`lib.rs:190`), append 진입점은 `lib.rs:598` `append_entry(kind, payload, timestamp_iso)`.
- 지연 인덱스는 `crates/omp-session/src/index.rs:90` `SessionIndex`, 로더 `index.rs:118` `SessionIndex::load(path)`, 활성 브랜치 `index.rs:161` `branch() -> Vec<u32>`, 각 레코드는 `index.rs:69` `EntryRef { line_no, offset, len, id, parent_id, timestamp, kind }` 로 바이트 구간을 이미 들고 있다.
- 런타임은 프로세스당 세션 1개다: `crates/omp-cli/src/runtime.rs:116` `struct Runtime` 에 `persist: Persistence`(`runtime.rs:120`) 하나. 경로는 `runtime.rs:1934` `session_file()`, `runtime.rs:1921` `session_id()`, `runtime.rs:1927` `persist_file() -> Option<PathBuf>`.
- 세션 전환은 `runtime.rs:2215` `new_session`, `runtime.rs:2255` `load_session`, 게이트 `runtime.rs:2570` `check_switch`, 전환 통지는 `runtime.rs:2615` `notify_session_switch`. `Persistence` 는 `crates/omp-cli/src/persist.rs:89`, `persist.rs:407` `start_new`, `persist.rs:436` `bind_existing(path, session_id, leaf)`.
- 사용 중 가드 선례는 `rpc.rs:752` `if rt.agent.is_streaming().await { ... CmdError::coded(..., "session_busy") }` (branch/fork).
- 확장 호스트는 `crates/omp-ext/src/host.rs:202` `ExtHost`, `host.rs:544` `emit(event, ctx)`, `host.rs:575` `has_handlers(&str)`, `host.rs:604` `emit_nowait`, 응답 API는 `host.rs:411` `respond`, `host.rs:463` `request`. `api.rpc.handle` 같은 확장→엔진 RPC 질의 경로는 없다(파일 내 `rpc` 언급 6건은 전부 JSON-RPC 봉투 문자열).
- `crates/omp-collab` 은 라이브 세션 미러링이다(`crates/omp-collab/src/host.rs:1-8`). move에 재사용할 것은 청크 패킹 `crates/omp-collab/src/replication.rs:631` `snapshot_chunks(&[String], &[usize])` 와 상한 `replication.rs:92` `SNAPSHOT_CHUNK_BYTES = 512 * 1024` 정도다.
- 설정 루트는 `~/.omp/agent` (`crates/omp-settings/src/settings.rs:461-469` `agent_dir()`), 세션 디렉터리는 `crates/omp-cli/src/persist.rs:63` `sessions_dir(cwd)`. 우리 플릿의 `~/.omo` 와는 다르다.
- 종료 신호는 `crates/omp-cli/src/runtime.rs:1641` `self.send(json!({"type":"session_settled"}))` 와 `runtime.rs:1631` `prompt_result.sessionSettled`. `agent_settled` 는 `crates/omp-cli/src/ext.rs:1593` 에서 `host.has_handlers("agent_settled")` 일 때만 확장 이벤트로 나간다.

## 2. 게이트웨이가 요구하는 계약

- 명령 형태(mhc 기준, `mhc/crates/maho-rpc/src/session_move_commands.rs:93` `move_session(agent_dir, hosted_paths, command)`): `{"type":"move_session","id":..,"op":..,"moveId":..,"sessionPath":..,"target":..,"bundleDir":..,"cwd":..,"from":..,"droppedUserMessages":[..],"answer":{..}}`.
- op 집합은 `session_move_commands.rs:44-88` 의 `prepare|cancel|status|export|finish|import|revoke`. 이 조각은 export / import / status / finish / cancel 5개만 다루고 prepare·revoke 는 별도 조각으로 넘긴다.
- `moveId` 는 plain id(첫 글자 영숫자, 이후 `._-`, 128자) — `mhc/crates/maho-core/src/session_move/bundle.rs:154` `is_move_id`, `bundle.rs:158` `parse_plain_id`.
- 상태 어휘: `ready | receiving | accepted | declined{reason} | failed{reason}` (`move_ledger.rs:73-90`), reason 집합 `no_record|expired|cancelled|chunk_without_ready|repository` / `bundle_invalid|import_failed` (`session_move_commands.rs:61-85`). `ready` TTL 은 10분(`move_ledger.rs:130` `READY_TTL_MS`).
- 오류 코드 집합(`session_move_commands.rs:145-160`): `invalid_move_request, session_open_here, move_not_released, move_mismatch, move_bundle_invalid, move_repository_mismatch, move_target_occupied, move_already_moved, move_fenced`, 기본값 `move_failed`.
- 번들 레이아웃(`bundle.rs:8-10`): `manifest.json`, `files/session.jsonl`, sidecar, `state/` payload. 파일마다 `{path, sha256}` (`bundle.rs:20` `BundleFile`), 매니페스트는 `bundle.rs:41` `MoveManifest { format, moveId, sessionId, sessionFileName, sourceCwd, repository, session, sidecars, droppedUserMessages, payloads }`.
- 능력 광고 문자열은 `move_session` (`mhc/crates/maho-rpc/src/session_move_wire.rs:4`), 광고 지점 `mhc/crates/maho-rpc/src/host_capabilities.rs:165`.

## 3. 설계

### 3.1 배치와 타입

새 크레이트를 만들지 않는다. 직렬화·원장은 `crates/omp-session/src/move.rs`(신규 모듈, `lib.rs` 에 `pub mod move;`), RPC 동작은 `crates/omp-cli/src/move_cmd.rs`(신규)에 두고 `rpc.rs:593` 의 `match kind` 에 `"move_session" => move_cmd::handle(rt, cmd).await,` 한 줄을 넣는다.

```rust
pub struct MoveId(String);                       // is_plain_id 검증 통과값
pub struct BundleFile { pub path: String, pub sha256: String }
pub struct MoveManifest { pub format: u64, pub move_id: MoveId, pub session_id: String,
    pub session_file_name: String, pub source_cwd: String, pub resume_leaf: Option<String>,
    pub session: BundleFile, pub sidecars: Vec<BundleFile>, pub payloads: Vec<MovePayload>,
    pub dropped_user_messages: Vec<String> }
pub enum MoveState { Ready { expires_at: i64 }, Receiving, Accepted, Declined(DeclineReason), Failed(FailReason) }
pub struct MoveError { pub code: &'static str, pub detail: String }   // CmdError 로 변환
pub struct MoveLedger { dir: PathBuf }                                 // <agent>/session-moves/ledger/<moveId>.json
```

`sha2` 는 `omp-session/Cargo.toml` 에 없다(의존성은 `pa-types, serde, serde_json, anyhow, memchr, memmap2, rayon`). `crates/omp-cli/Cargo.toml:40` 에는 `sha2 = "0.10"` 이 있으므로 해시 계산은 `omp-cli` 에 두고 `omp-session::move` 는 해시 문자열만 받는다(의존성 추가 없음).

### 3.2 이동 단위

이동 단위는 세션 파일 1개 + 그 세션의 부속물이다. tau에는 다중 세션 호스트가 없으므로 `hosted_paths` 는 `vec![rt.session_file()]` 로 유도하고, `--no-session` 이면 `persist_file()` 이 `None`(`runtime.rs:1927`)이라 `invalid_move_request` 로 거절한다. fork로 파생된 파일(`runtime.rs:2269` `fork_session`)은 별개 세션으로 취급해 각각 moveId를 받는다. 다른 장비에서 같은 세션을 동시에 옮기는 경합은 moveId 단위 원장과 `move_fenced`(`move_ledger.rs:134`)로만 막고, 세션 파일 자체의 배타 락은 이 조각 범위 밖이다.

### 3.3 전송 포맷

- 원본 바이트 보존이 원칙이다. `SessionIndex` 의 `EntryRef.offset/len`(`index.rs:69-88`)으로 줄을 그대로 슬라이스해 `files/session.jsonl` 에 쓴다. 재직렬화(`entries::to_line`, `lib.rs:492`)는 쓰지 않는다 — `to_js_string` 의 숫자 포맷 차이로 게이트웨이 쪽 비교가 흔들리기 때문이다.
- 예외는 헤더 한 줄이다. import 시 첫 레코드만 파싱해 `cwd` 를 대상 cwd로 바꾸고 다시 쓴다(`lib.rs:225` 헤더 형태 유지). 나머지 줄은 바이트 그대로.
- 매니페스트는 `manifest.json` 하나, `format: 1`. `repository` 는 tau에 git root commit/origin 리더가 없어 `null` 로 기록한다(7절 확인 필요).
- 전송 자체는 게이트웨이 몫이다. tau는 번들 디렉터리 경로만 만들고 반환한다. 원격 전송이 필요하면 `replication.rs:631` `snapshot_chunks` 로 512KB 프레임에 실어 보내는 별도 조각이 붙는다.

### 3.4 5개 동작

- export: (1) `rt.agent.is_streaming().await` 이면 `session_busy`(`rpc.rs:752` 선례). (2) `SessionIndex::load` 로 헤더 `id` 와 활성 leaf(`index.rs:161` `branch()` 마지막)를 얻는다. (3) 원본에 `custom` 엔트리 `session_released` 를 `Persistence::append_entry`(`persist.rs:142` → `lib.rs:598`)로 남긴다 — `{"reason":"move","move_id","session_id","target"}`. (4) `bundleDir.partial-<pid>` 에 복사, 파일별 sha256 계산, `manifest.json` 작성, `rename` 으로 확정. (5) 응답 `data: {move_id, session_id, bundle_dir, files}`.
- import: (1) `bundleDir`·`cwd` 절대경로 검증. (2) 매니페스트 파싱 + 전 파일 sha256 재계산, 불일치면 `move_bundle_invalid`. (3) `moveId` 가 `Accepted` 면 `move_already_moved`, 기기 기록 충돌이면 `move_mismatch`. (4) 원장 `Receiving`. (5) 세션 디렉터리(`persist.rs:63`) 안 staging 에 풀고 헤더 `cwd` 재작성. (6) 대상 경로에 쓸 수 있는 세션이 이미 있으면 `move_target_occupied`. (7) `rename` 후 `session_moved_in` custom 엔트리 append, 원장 `Accepted`. (8) 응답 `{session_id, session_path}`.
- status: 원장 상태와 파일 실측을 함께 본다. 대상 파일에 해당 moveId의 `session_moved_in` 이 있으면 파일 우선으로 `accepted`, 없으면 원장값. 원본에 `session_released` 만 있으면 `ready`, 기록이 없으면 `no_record`.
- finish: 수신자의 `answer{state,reason}` 를 검증(`session_move_commands.rs:61-85` 와 동일 집합)한 뒤 원본에 `session_moved`(accepted) 또는 `session_reclaimed`(declined/failed) 를 append 하고 accepted 면 번들 디렉터리를 지운다. 응답 `{ownership: "writable"|"moved"|"moving"}`.
- cancel: `Ready` 만 취소 가능. `Declined{cancelled}` 로 전이, `bundleDir` 삭제, 원본에 `session_reclaimed` append 로 쓰기 가능 상태 복귀. `Receiving` 이후는 `move_fenced`.

### 3.5 재개 지점

- 매니페스트 `resume_leaf` 에 export 시점의 활성 leaf id를 적는다. 값이 없으면 파일 마지막 엔트리가 leaf다(`index.rs:161` 규칙과 동일).
- import 후 재개는 `SessionWriter::open(path, Some(leaf))`(`lib.rs:257`)로 그 지점에 이어 쓴다. landed 파일을 그대로 쓰고 새 파일을 만들지 않는다(`Persistence::bind_existing`, `persist.rs:436`).
- 런타임 재개는 `Runtime::load_session`(`runtime.rs:2255`) → `load_session_as`(`runtime.rs:2279`)가 인덱스를 다시 스캔해 브랜치만 materialize 한다. move 후 첫 기동은 이 경로를 그대로 탄다.
- 확장 상태 복원은 재개 직후 `session_move_state_restored` custom 엔트리 1회로 표시해 중복 복원을 막는다.

### 3.6 부분 실패 롤백

- 파일 원자성: 항상 staging 디렉터리에 완성한 뒤 `rename`. 실패 시 staging 만 지운다. `SessionWriter` 의 fsync 정책은 확인 필요 — 미보장이면 import 커밋 전 `sync_all` 을 명시 호출한다.
- 원장 전이는 `ready → receiving → accepted | declined | failed` 단방향이고 되돌리지 않는다(`move_ledger.rs:134` `MoveFencedError`). 원장 파일도 temp+rename 으로 쓴다.
- export 실패: staging 삭제, `session_released` 마커는 남겨 `ready` 유지(재시도) 또는 `cancel` 로 정리. 원본 데이터는 손대지 않는다.
- import 실패: staging 삭제, 원장 `failed{bundle_invalid|import_failed}`, 대상 디렉터리 무변경. staging 은 `.move-staging/<moveId>` 아래에 둔다.
- finish 실패: 원본 마커를 쓰지 못했으면 다음 `status` 가 파일 실측으로 상태를 복구한다(원장 신뢰 금지).
- 크래시 복구: 기동 시 `session-moves/ledger` 를 훑어 `ready` 가 `READY_TTL_MS`(10분)를 넘겼으면 `declined{expired}`, `receiving` 이 남아 있으면 staging 존재 여부로 `accepted`/`failed` 를 판정한다.

### 3.7 확장 상태

tau 확장은 별도 프로세스(`ext-host/host.ts`)이고 `api.rpc.handle` 이 없으므로, export 는 `host.emit(json!({"type":"session_move_export","moveId":..,"dir":..}), None)`(`host.rs:544`, 게이트 `host.rs:575`)로 알리고 핸들러가 `dir` 에 쓴 파일을 `payloads[{kind:"extension-state", files}]` 로 묶는다. import 는 `session_move_import` 로 같은 디렉터리를 넘긴다. 핸들러가 없으면 payload 없이 진행한다. `emit` 이 핸들러의 쓰기 완료까지 기다리는지는 확인 필요(`EmitResult` 정의 `host.rs:152`).

### 3.8 응답과 동기화

- 응답은 기존 `response()`(`rpc.rs:203`)를 그대로 쓴다. 게이트웨이가 `errorCode` 를 요구하면 `CmdError.code`(`runtime.rs:34`)를 `code` 로 내보내고 `errorCode` 별칭은 게이트웨이 어댑터에서 매핑한다.
- move_session 은 세션을 전환하지 않으므로 `session_settled`(`runtime.rs:1641`)를 기다릴 필요가 없다. 이동 후 세션을 여는 것은 별도 `switch_session`(`rpc.rs:714`) 호출이고, 그 뒤 클라이언트는 `session_settled` 또는 `prompt_result.sessionSettled`(`runtime.rs:1631`)를 종료 신호로 쓴다.
- `agent_settled` 는 핸들러가 있을 때만 나가므로(`ext.rs:1593`) 이 조각에서 의존하지 않는다.

## 4. 구현 단계 (커밋 단위)

1. `omp-session`: `move.rs` 타입(`MoveId`, `BundleFile`, `MoveManifest`, `MoveState`, `MoveError`) + 매니페스트 파서/직렬화 + plain id 검증. `lib.rs` 모듈 등록.
2. `omp-session`: `MoveLedger`(prepare/begin_receive/commit/decline/fail/status, TTL, 펜싱) + temp+rename 쓰기.
3. `omp-session`: `export_session_for_move` — 인덱스 슬라이스 복사, sha256 주입, staging/rename, `session_released` 마커.
4. `omp-session`: `import_moved_session` — 검증, staging, 헤더 `cwd` 재작성, target 점유 검사, `session_moved_in` 마커.
5. `omp-cli`: `move_cmd::handle` + `rpc.rs:593` 에 `"move_session"` 분기 + `response()` 코드 매핑. `announce_commands`(`runtime.rs:3739`) 계열 능력 광고.
6. `omp-cli`: `finish`/`cancel` + `session_moved`/`session_reclaimed` 마커 + 번들 정리.
7. `omp-cli`: 확장 상태 emit 연결(`session_move_export`/`session_move_import`)과 payload 수집.
8. 문서: 게이트웨이 계약 표(`errorCode`, `agent_settled` 부재) 정합 노트.

각 커밋은 `cargo nextest run -p omp-session` 또는 `-p omp-cli` 로 닫고, 마지막에 `cargo clippy -p omp-session -p omp-cli --all-targets -- -D warnings`.

## 5. 테스트

- 단위(`omp-session`): 매니페스트 round-trip, `is_plain_id` 경계, sha256 불일치 → `move_bundle_invalid`, 원장 전이표와 `move_fenced`, TTL 만료 판정, 헤더만 바뀌고 나머지 줄 바이트가 동일함을 확인하는 골든 비교.
- 단위(`omp-session`): `resume_leaf` 지정 시 `SessionWriter::open(path, Some(leaf))` 가 그 지점에 이어 쓰는지 — `index.rs` 의 브랜치 케이스(a→b, c 분기) 재사용.
- 통합(`omp-cli`): tempdir 두 개로 `export → import → status → finish` 왕복, 대상에 같은 세션 파일이 있을 때 `move_target_occupied`, 스트리밍 중 `session_busy`, `--no-session` 거절.
- 실패 주입: staging 복사 중 오류, `manifest.json` 절단, import 직전 대상 파일 생성, `finish` 미호출 상태에서 `status` 복구.
- RPC 계층: 자식 프로세스에 stdin으로 `{"type":"move_session",...}` 를 넣고 stdout 프레임을 파싱. 대기는 고정 sleep 없이 프레임 수신과 파일 이벤트로만 한다(저장소 규칙).

## 6. QA 시나리오

- S1 정상 이동: A에서 export, 번들을 B의 같은 상대 경로로 복사, import, finish, B에서 `switch_session` 후 대화가 leaf까지 그대로 보이고 이어쓰기가 된다.
- S2 대상 점유: B에 같은 세션 파일이 이미 있으면 `move_target_occupied`, A 원본은 쓰기 가능 상태 유지.
- S3 손상 번들: `files/session.jsonl` 1바이트 변조 → `move_bundle_invalid`, B 디렉터리에 잔여 파일 없음.
- S4 만료: export 후 11분 방치 → `status` 가 `declined{expired}`, `cancel` 불가.
- S5 취소: export 후 `cancel` → 원본이 `writable`, 번들 삭제, `status` 가 `declined{cancelled}`.
- S6 이동 중 작업: 프롬프트 스트리밍 중 export → `session_busy`.
- S7 확장 상태: `session_move_export` 핸들러가 파일을 쓰는 확장이 로드된 상태에서 export → payload에 실리고 import 후 복원 1회.
- S8 게이트웨이 재시작: import 도중 프로세스 종료 → 재기동 후 `status` 가 `receiving`/`accepted` 를 파일 실측으로 정확히 보고.
- S9 경로 표기: 심볼릭 링크된 세션 디렉터리와 `~/.omp` 대 `~/.omo` 루트 차이에서 대상 경로가 어긋나지 않는다.
- S10 대용량: 수만 줄 세션에서 export/import 시간과 메모리(`index.rs` 의 `PAR_THRESHOLD` 병렬 경로) 확인.

## 7. 위험과 미확인

- tau는 프로세스당 세션 1개(`runtime.rs:116-120`)라 mhc의 `hosted_paths` 개념이 없다. 게이트웨이가 한 프로세스에서 여러 세션을 동시에 호스팅하길 기대하면 계약이 어긋난다 — `--multi-session` 플래그도 없다(`crates/omp-cli/src/cli.rs:97`, `138`, `142` 목록에 없음).
- repository identity: tau에 git root commit/origin 리더가 없어 `repository` 검사를 생략한다. 다른 저장소로 세션이 넘어가는 오이동을 막지 못한다 — 확인 필요.
- sidecar: tau에는 `session_sidecar_store` 가 없다(`crates/omp-cli` 내 grep 0건, `crates/omp-cli/src/subcmd/gc.rs:231` 주석만). `sidecars` 는 항상 빈 배열이 되어 mhc 번들과 형태가 다르다 — 확인 필요.
- `SessionWriter::push_line` 의 fsync 정책 미확인. 미보장이면 import 커밋 경로에 `sync_all` 을 넣어야 한다.
- `ExtHost::emit` 이 핸들러의 파일 쓰기 완료를 보장하는지 미확인(`host.rs:152` `EmitResult`). 보장하지 않으면 export 가 빈 payload를 담는다.
- 응답 스키마 차이: tau는 `code` 만 내보내고 `errorCode`/`errorData` 가 없다(`rpc.rs:203-226`). 게이트웨이가 mhc 형태를 강제하면 어댑터 또는 `response()` 확장이 필요하다.
- Windows에서 열린 파일 `rename` 실패, CRLF 혼입, 심볼릭 링크된 세션 디렉터리에서 대상 경로 정규화가 어긋날 수 있다.
- 대용량 세션에서 `SessionIndex::load` 가 전체를 매핑(`memmap2`)하므로 export 시 원본과 번들이 동시에 디스크를 점유한다. 여유 공간 사전 검사가 없다.
- `move_session` 은 명령 워커(`rpc.rs:1436`)에서 돌므로 동기 복사를 하면 다른 명령이 막힌다. mhc처럼 `spawn_blocking`(`mhc/crates/maho-rpc/src/multi_session_host.rs:1673-1687`)으로 분리해야 하며, 이때 `Arc<Runtime>` 만 캡처하고 파일 경로는 문자열로 넘겨야 한다.
- `prepare`·`revoke` op 는 배정 범위 밖이라 계약만 인용하고 설계하지 않았다.
