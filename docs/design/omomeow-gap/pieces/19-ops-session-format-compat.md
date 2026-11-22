# 세션 파일 형식 호환 설계: omp JSONL v3에서 게이트웨이(senpi) 읽기까지

## 1. 현재 상태 (근거)

tau의 세션 파일은 omp TS의 JSONL v3를 바이트 단위로 재현하는 것이 계약이다.

- 포맷 버전 상수는 `crates/omp-session/src/lib.rs:27` `pub const SESSION_VERSION: u32 = 3`. 크레이트 문서 첫 줄이 "omp session files (JSONL, format v3)"이다(`lib.rs:1`).
- 헤더는 항상 1번 줄이고 형태가 고정이다: `{"type":"session","version":3,"id":…,"timestamp":…,"cwd":…}` (`lib.rs:225`, `lib.rs:246`). `SessionHeader`는 `vendor/pa-types/src/session.rs:29-50`에 있고 `parentSession`, `rlmDepth`, `git`이 선택 필드다.
- 파일명은 `{stamp}_{session_id}.jsonl`이고 stamp는 ISO의 `:`/`.`을 `-`로 치환한 값이다(`lib.rs:210-211`). `create_at`은 호출자가 경로를 지정하는 별도 진입점이다(`lib.rs:237-249`).
- 1번 줄 앞에 256바이트 고정폭 타이틀 슬롯 `{"type":"title","v":1,…}`이 붙을 수 있다(`entries.rs:604` `TITLE_SLOT_BYTES = 256`, `lib.rs:444-483` 제자리 패치). 로더는 첫 줄이 슬롯이면 헤더 검사를 2번 줄로 옮긴다(`lib.rs:120-129`).
- 엔트리 종류는 `entries.rs:463-482`의 `OmpEntry` 16종이다: `title`, `message`, `model_change`, `thinking_level_change`, `service_tier_change`, `model_usage`, `credential_pin`, `mode_change`, `title_change`, `ttsr_injection`, `reset_boundary`, `session_init`, `compaction`, `branch_summary`, `custom`, `custom_message`. 여기에 `pa_types::FileEntry`가 `label`, `session_info`, `session_state`, `git_state`, `child_usage_attributed`를 더한다(`vendor/pa-types/src/session.rs:329-415`).
- 트리 의미는 `parentId` 단일 연결이고, `label` 엔트리가 `targetId`의 라벨을 설정/해제한다(`tree.rs:56-72`, `tree.rs:80-120`). 지연 인덱스는 `(line, offset, len, id, parentId, kind)`만 유지한다(`index.rs:61-82`).
- 숫자는 JS `Number#toString` 형식으로 직렬화한다(`entries.rs:648-700`). 즉 파싱 후 재직렬화가 원문과 바이트 동일해야 한다는 불변식이 크레이트 전체에 걸려 있다(`entries.rs:1-9`).

게이트웨이가 세션 파일에서 실제로 읽는 지점은 네 곳뿐이다.

- 헤더 id: `omo-nyang/src/session-identity.ts:24-30`이 파일 앞 64KB의 첫 줄을 `JSON.parse`해 `type === "session"`과 문자열 `id`를 요구한다. 호출부는 `lane-sync.ts:69-70`.
- 꼬리 증거: `session-evidence.ts:14`의 zod 스키마가 `{type:"custom", customType, data, timestamp}`를 요구하고, `session-lifecycle`(`session-evidence.ts:70`, `:100`)과 `omo-claim-receipt:verdict`(`:105`)를 소비한다. 검증 통과 증거는 `type:"message"` + `message.role === "toolResult"`에서 뽑는다(`session-evidence.ts:74-76`, `:88-96`). 읽는 양은 꼬리 262144바이트(`session-evidence.ts:63`, `readSessionTail` `:137-145`).
- 파킹 판정: 같은 꼬리에서 `session-lifecycle.data.state === "waiting_human"` 및 `waitingOn.human[].kind === "interrupted_run"`을 본다(`session-evidence.ts:65-79`).
- 골 상태: `lane-pause.ts:59-70`의 파이썬 프로브가 첫 줄 `id`를 읽고 같은 디렉터리의 `extensions/goal/<id>.json`을 연다. 세션 파일 자체가 아니라 사이드카 경로 규칙에 의존한다.

tau 쪽에 없는 것도 실측으로 확인했다. `session-lifecycle`, `omo-claim-receipt` 문자열은 `crates/` 전체 grep에서 0건이다. tau의 goal은 RPC `goal` 명령이 빈 응답을 돌려주는 스텁이다(`crates/omp-cli/src/rpc.rs:250-274`, 디스패치 `:776`).

게이트웨이 반대편(mhc)의 형식은 v4다.

- `mhc/crates/maho-agent/src/harness/session/jsonl/types.rs:6-7` `JSONL_FORMAT_VERSION = 4`, `JSONL_STORAGE_VERSION = 1`. 헤더는 `{v, kind:"header", id, storageVersion, createdAt(ms), cwd, parentSessionId?, legacyParentSessionPath?, nextSeq?}` (`types.rs:9-38`).
- v4 엔트리는 커밋 트랜잭션 레코드다: `{kind:"entry", id, parentId, seq, timestamp(ms), type, …}` (`jsonl/commit.rs:23-31`). `kind`는 `entry|usage|value|list` 넷이다(`commit.rs:10-19`).
- v4가 모델링하는 엔트리는 `Message`, `Compaction`, `BranchSummary`, `Custom` 넷뿐이다(`harness/session/types.rs:36-66`). `Message`에는 `terminate`가 붙고, `Compaction`은 `retainedTail`을 갖는다.
- mhc는 legacy v3도 읽는다. 헤더 판정은 `type=session`, `version=3`, `id`, `cwd`, 파싱 가능한 `timestamp`, 선택적 `parentSession`이다(`jsonl/codec.rs:25-40`). 엔트리는 `message|custom|custom_message|branch_summary|compaction|model_change|thinking_level_change|active_tools_change|session_info|label` 10종만 받는다(`jsonl/legacy_v3.rs:41-158`).
- 여기서 결정적 사실: legacy v3 경로는 관대하지 않다. 모르는 `type`은 `"Unsupported legacy v3 record type"` 하드 에러(`legacy_v3.rs:459-465`), 알려진 type이라도 필드가 안 맞으면 `"Invalid legacy v3 JSONL record"` 하드 에러다(`legacy_v3.rs:443-457`). tau의 `model_change`는 `model: "provider/modelId"` 단일 문자열(`entries.rs:88-99`)인데 legacy v3는 `provider`와 `modelId` 두 필드를 요구한다(`legacy_v3.rs:110-122`).

따라서 tau가 지금 쓰는 v3 파일은 mhc legacy 임포터로 읽히지 않는다. 1번 줄이 256바이트 타이틀 슬롯이면 헤더 판정에서 즉시 실패하고, 슬롯이 없어도 `service_tier_change`·`model_usage`·`reset_boundary` 같은 tau 고유 type에서 하드 에러가 난다. "확인 필요"로 남겨야 할 것은 실패 지점의 정확한 순서다. 실제 `~/.omp` 저널 표본으로 재현하지는 못했다.

## 2. 게이트웨이가 요구하는 계약

계약을 세 층으로 나눈다. tau가 맞춰야 하는 것은 1층과 2층이고, 3층은 이미 맞다.

1. 식별 계약: 세션 파일 첫 줄이 `{"type":"session","id":<문자열>}`. tau v3 헤더는 이 조건을 만족한다(`session-identity.ts:17-23` 대 `lib.rs:225`). 단 1번 줄이 타이틀 슬롯이면 깨진다.
2. 증거 계약: 꼬리 256KB 안에 `custom`/`session-lifecycle`과 `message`/`toolResult` 레코드가 senpi 키 이름으로 있어야 한다(`session-evidence.ts:14`, `:74-76`). tau는 `custom` 엔트리를 `customType`/`data`로 쓰므로(`entries.rs:243-253`) 키 이름은 맞고, `session-lifecycle`을 쓰지 않는 것이 결손이다.
3. 경로 계약: 게이트웨이는 세션 경로를 워커 실행 인자로 그대로 넘긴다(`omo-nyang/src/gateway.ts:1112`, `:1309`의 `workerArgv({omo, session})`). 즉 `--session <path>`로 tau가 다시 열어야 하는 파일과, 게이트웨이가 읽는 파일이 같은 경로다. `session.move` 스냅샷도 `${sessionFile}.omomeow-move.json`으로 같은 경로에서 파생된다(`session-move-extension.ts:17`).
4. 골 계약: `dirname(sessionFile)/extensions/goal/<header.id>.json` (`lane-pause.ts:59-70`). tau에는 이 사이드카 라이터가 없다.

3번이 설계의 제약을 결정한다. 게이트웨이가 읽는 파일을 tau가 그대로 재개 파일로 열 수 있어야 한다.

## 3. 설계

결론: 어댑터는 tau 쪽에 둔다. 새 크레이트 `crates/omp-session-compat`(의존: `omp-session`, `pa-types`)로 분리하고, `omp-session`의 바이트 동일 불변식은 건드리지 않는다.

근거 네 가지.

- 게이트웨이 쪽에 두면 tau의 16종 엔트리와 JS 숫자 포맷(`entries.rs:648`)을 TS에서 다시 구현해야 한다. tau 쪽은 이미 타입 모델과 `to_js_string`(`entries.rs:703`)을 갖고 있어 변환이 순수 함수가 된다.
- 게이트웨이의 독자는 zod `safeParse`로 방어적이고 필드 4개만 본다(`session-evidence.ts:14`, `:74`). 읽기 계약이 좁아서 tau가 생산자를 맞추는 편이 표면적이 작다.
- mhc의 legacy v3 파서는 senpi 업스트림 포트다(`legacy_v3.rs:1`). tau 사정으로 관대하게 고치면 업스트림 파리티가 깨진다. 별도 PR로 "모르는 type은 건너뛴다"만 완화하는 것이 맞고, 이는 어댑터의 필요를 없애지 않는다(tau 고유 정보는 여전히 손실).
- tau의 v3 저널은 omp TS가 열 수 있어야 한다는 자체 계약이 있다(`lib.rs:188-190`). 저널을 senpi v4로 바꾸면 이 계약이 깨진다. 그래서 저널은 그대로 두고 별도 파일을 만든다.

산출물은 두 가지다.

- 미러 라이터 `GatewayMirror`: tau 저널에 엔트리가 append될 때 같은 엔트리를 senpi v4 레코드로 `<journal>.gateway.jsonl`에 append한다. 자체 `seq` 카운터(1부터), `timestamp`는 ms 정수, 헤더는 `{v:4, kind:"header", id, storageVersion:1, createdAt, cwd, nextSeq}`. 게이트웨이의 `sessionFile`은 이 미러를 가리킨다.
- 왕복 변환 `export_senpi(&[Line]) -> Vec<String>` / `import_senpi(&[Value]) -> Vec<Line>`: 콜드 변환과 재개용. `import_senpi`가 있어야 `--session <mirror>`로 tau가 열린다.

재개 경로 처리는 `crates/omp-cli/src/main.rs:157-175`를 확장한다. `resolve_session`이 실패하고 대상 파일 첫 줄이 `kind:"header"`인 v4면 `import_senpi`로 임시 v3 저널을 만들어 그쪽을 연다. `rpc` 모드에서는 `resume_target`이 `None`이고 세션 수명을 런타임이 소유하므로(`main.rs:511-513`), `switch_session`(`rpc.rs:714-728`)과 `open_session`(`rpc.rs:730-745`)에도 같은 폴백을 넣는다.

필드 매핑 표(tau v3에서 senpi v4):

| tau v3 | senpi v4 | 손실 |
|---|---|---|
| `{type:"session",version:3,id,timestamp,cwd,parentSession?}` | `{v:4,kind:"header",id,storageVersion:1,createdAt:<ms>,cwd,parentSessionId?}` | `rlmDepth`, `git` |
| `title` 256B 슬롯(1번 줄) | 없음. 첫 줄을 헤더로 옮기고 슬롯 폐기 | 제목, `source`, `updatedAt` |
| `message` | `{kind:"entry",seq,id,parentId,timestamp:<ms>,type:"message",message}` | `developer`/`pythonExecution` 롤은 `custom`으로 강등 |
| `custom` | `{…,type:"custom",customType,data}` | 없음(직접 대응) |
| `custom_message` | `{…,type:"custom",customType:"tau-custom-message",data:{content,display,details}}` | 컨텍스트 주입 여부, `attribution` |
| `compaction` | `{…,type:"compaction",summary,tokensBefore,details,usage,fromHook:false}` | `shortSummary`, `firstKeptEntryId`, `retainedTail`(생성 불가), `preserveData` |
| `branch_summary` | `{…,type:"branch_summary",fromId,summary,details,usage}` | `fromExtension` |
| `model_change{model:"p/m"}` | `custom` `customType:"tau-model-change"` | senpi v4에 대응 타입 없음 |
| `thinking_level_change`, `service_tier_change`, `mode_change`, `title_change`, `ttsr_injection`, `reset_boundary`, `session_init`, `model_usage`, `credential_pin` | `custom` `customType:"tau-<kind>"`, 원 payload를 `data`에 | 원 타입 판별은 customType 접두로만 |
| `label{targetId,label}` | `custom` `customType:"tau-label"` | senpi v4에 label 타입 없음 |
| `session_info`, `session_state`, `git_state`, `child_usage_attributed` | `custom` `customType:"tau-<kind>"` | 동일 |
| 없음 | `session-lifecycle` `custom` 신규 합성 필요 | tau 상태 기계에서 파생 |

손실 정보 정리: (a) 타이틀 슬롯 전체, (b) `compaction.retainedTail`과 `shortSummary`, (c) `custom_message`의 컨텍스트 주입 의미, (d) tau 고유 9종의 타입 정체성(customType 접두로만 보존), (e) `label`의 트리 라벨 해석. 반대 방향 손실은 v4의 `kind:"usage"|"value"|"list"` 레코드다(`commit.rs:10-19`). 이걸 버리면 토큰 회계와 값 네임스페이스가 사라진다. `import_senpi`는 이 셋을 `custom` `customType:"senpi-usage"` 등으로 보존해야 한다.

`session-lifecycle` 합성은 이 조각의 범위 밖이지만 의존성이다. 게이트웨이의 done gate는 `waitingOn.children`과 `completion.kind`를 읽으므로(`session-evidence.ts:16-20`), 미러에 이 레코드가 없으면 `sessionEvidence`가 `undefined`를 돌려주고 증거 없음으로 판정된다.

## 4. 구현 단계 (커밋 단위)

1. `feat(omp-session-compat): scaffold crate with senpi v4 record types` — `SenpiHeader`, `SenpiWrite`(`kind` 태그 enum: `entry|usage|value|list`), `SeqCounter`. 직렬화는 `entries::to_js_string` 재사용.
2. `feat(omp-session-compat): export omp v3 lines to senpi v4` — `export_senpi(&[Line]) -> Result<Vec<String>>`, 위 표의 16종 분기. 단위 테스트에 표의 모든 행을 픽스처로.
3. `feat(omp-session-compat): import senpi v4 lines into omp v3` — `import_senpi`, `usage|value|list` 보존 규칙, `timestamp` ms에서 ISO 역변환.
4. `feat(omp-session): GatewayMirror appends a senpi v4 shadow journal` — `SessionWriter::append_entry` 옆에 훅. 미러 경로는 `<journal>.gateway.jsonl`, 실패는 저널 쓰기를 막지 않는다(best-effort, 경고 1회).
5. `feat(omp-cli): omt session export --format senpi-v4` — 콜드 변환 서브커맨드. `crates/omp-cli/src/subcmd/` 아래.
6. `feat(omp-cli): resume a senpi v4 session through import_senpi` — `main.rs:157-175` 폴백, `rpc.rs:714`/`:730` 폴백.
7. `feat(omp-cli): write the extensions/goal sidecar the gateway probes` — `dirname(session)/extensions/goal/<id>.json`, 골 상태 변화 시 갱신.

## 5. 테스트

- 왕복 항등: 실제 `~/.omp/agent/sessions` 표본 파일을 `export_senpi` 후 `import_senpi` 하면 메시지 시퀀스와 `parentId` 체인이 보존됨을 검증. tau가 이미 쓰는 round-trip 검사기(`lib.rs` `RoundTrip`)와 같은 형태로.
- 매핑 표 커버리지: `OmpEntry` 16종 각각에 대해 export 결과의 `type`/`customType`을 고정 문자열로 단언. 컴파일 타임 exhaustiveness는 `match`에 `_ =>`를 두지 않아 강제.
- 숫자 포맷: `createdAt`/`timestamp` ms 정수가 JS `Number#toString` 규칙과 일치(`entries.rs:648` 테스트 확장).
- 헤더 계약: 미러 첫 줄이 `session-identity.ts:17-23`의 조건을 만족하는지 Rust 쪽 동등 검사 + TS 쪽 실제 `headerSessionId` 호출 테스트(게이트웨이 저장소에 별도 PR).
- 증거 계약: 합성한 `session-lifecycle` custom 레코드가 `session-evidence.ts:82-110`의 `sessionEvidence`를 통과해 `children`과 `finishedRun`을 돌려주는 통합 테스트. 이건 게이트웨이 저장소 테스트로 두고 tau 픽스처를 입력으로 쓴다.
- 재개: v4 파일을 `--session`으로 열어 메시지 수와 마지막 leaf id가 원본과 같은지.
- 금지: 시각 의존 테스트 없음. `timestamp`는 픽스처 고정값.

## 6. QA 시나리오

1. tau로 짧은 세션을 돌리고 `<journal>.gateway.jsonl` 첫 줄을 `jq`로 읽어 `v=4, kind="header", id` 일치 확인.
2. 게이트웨이 `readSessionHeaderId`를 그 미러에 직접 호출해 id가 나오는지 확인(불일치 시 레인 legacy 강등).
3. 미러 꼬리 256KB에 `session-lifecycle` 레코드가 있고 `state:"done"`, `completion.kind:"run"`이 읽히는지 `sessionEvidence`로 확인.
4. 게이트웨이가 `--session <mirror>`로 tau를 띄우고 이어서 프롬프트를 보내, 컨텍스트가 살아 있는지(직전 턴 내용 인용) 확인.
5. 세션을 한 번 `/clear`(reset_boundary)하고, 미러에 그 경계가 남아 재개 시 이전 컨텍스트가 섞이지 않는지 확인.
6. `session.move` export/import 후 스냅샷 `*.omomeow-move.json`과 미러가 같이 이동했는지, 골 사이드카 경로가 새 디렉터리 기준으로 풀리는지 확인.
7. tau 세션을 중간에 SIGKILL하고 미러 마지막 줄이 잘렸을 때, 게이트웨이 꼬리 읽기가 예외 없이 넘어가는지 확인(부분 줄 내성).

## 7. 위험과 미확인

- 미확인: mhc v4 저널의 한 줄이 단일 write 객체인지 write 배열인지. `jsonl/storage.rs:279`가 `serialize_jsonl_transaction(&prepared.writes)`를 한 줄로 쓰지만 트랜잭션 경계 형태를 직접 열어보지 못했다. 미러 포맷을 확정하기 전에 실제 `~/.omo` v4 파일 한 줄을 봐야 한다.
- 미확인: `maho_ai::types::Usage` 필드 이름과 tau `usage` 객체(`entries.rs:415-430`)의 1:1 대응. `cacheRead`/`cacheWrite`/`cttl`이 senpi에 있는지 확인하지 못했다.
- 미확인: tau v3 파일을 mhc legacy 임포터에 실제로 넣었을 때의 첫 실패 지점. 코드상으로는 타이틀 슬롯 또는 `service_tier_change`에서 하드 에러가 예상되지만 실행 재현은 못 했다.
- 위험: 미러를 별도 파일로 두면 게이트웨이가 `session.move`로 옮기는 대상과 tau가 재개하는 대상이 갈라진다. `session-move-extension.ts:67-68`이 `tasks/<key>.jsonl` 이름 규칙에서 taskId를 역추론하므로 미러 이름을 `<key>.gateway.jsonl`로 두면 그 추론이 깨진다. 미러를 `<key>.jsonl`로 두고 tau 저널을 다른 이름으로 두는 역전이 안전할 수 있고, 이 선택은 move 조각과 함께 정해야 한다.
- 위험: 미러 append가 저널 append와 원자적이지 않다. 크래시 시 미러가 뒤처지고, 게이트웨이는 뒤처진 증거로 done gate를 판정한다. 미러에 `journalLineNo`를 남겨 재동기화 지점을 갖는 보완이 필요하다.
- 위험: `session-lifecycle`을 tau가 합성하면 mhc의 상태 기계(`session_lifecycle_reducer.rs`)와 의미가 어긋날 수 있다. tau의 상태 어휘를 senpi의 `waiting_human`/`done`으로 사상하는 표를 별도로 확정해야 한다.
- 위험: v4에는 `label` 타입이 없어 트리 라벨이 `custom`으로 강등된다. 게이트웨이가 라벨을 읽는 곳은 확인하지 못했으나, 읽게 되면 `tau-label` 접두를 모른다.

조사하지 못한 부분: 실제 `~/.omo` v4 세션 파일 표본 확인, mhc `serialize_jsonl_transaction`의 출력 형태, `Usage` 필드 대응, tau v3를 mhc legacy 임포터에 넣는 실행 재현. 이 넷은 위 미확인 항목에 그대로 남아 있고, 나머지 매핑 표와 계약 분석은 위에 적은 파일:줄 근거로 확인한 내용이다.
