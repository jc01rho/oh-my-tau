# 워커 카나리 전환 블로커 추가 구현 설계 (2차)

기준 일자: 2026-11-23

기준 저장소:
- `oh-my-tau` `main` `9c49d86` (PR #37까지 머지, workspace version `0.12.0`)
- `omo-nyang` 게이트웨이 `main` `a080e5a` (`mac-mini:/home/jc01rho/Work/omo-nyang`)

근거: `#proj-omomeow` 스레드 `1795324158.334689`의 2026-11-23 재검증 보고. 1차 설계(`README.md`, Wave G1~G4)의 RPC 8종, 실행 플래그, 확장 API, 에이전트 도구, 하네스 항목은 PR #18~#37로 구현됐고, `rpc_conformance` 11/11이 통과했다. 그런데 실제 게이트웨이 인자로 띄우면 아래 블로커 때문에 워커 1대 카나리조차 시작할 수 없다. 이 문서는 남은 블로커를 코드 위치까지 확인하고 수정 설계, 테스트, 수용 기준, PR 분할을 정한다.

이 문서는 설계만 담는다. 코드는 바꾸지 않았다.

---

## 0. 요약

| ID | 쪽 | 블로커 | 결과 | 규모 |
| --- | --- | --- | --- | --- |
| B1 | tau | 확장이 등록한 공급자(`omomeow-opus`)를 코어가 모름 | 기동 즉시 `model ... not found`로 종료 | 중 |
| B2 | tau | `--append-system-prompt`가 단일 값 | 두 번째 값이 `OMOCAT.md`를 덮어씀 | 소 |
| B3 | tau | `get_state`·`abort(awaitIdle)` 응답 스키마 | 세션 이동·유휴 판정이 파싱 오류로 깨짐 | 소~중 |
| B4 | tau | `reload` 때 확장을 비우지 않고 다시 적재 | `duplicate extension rpc handler`로 재적재 실패 | 중 |
| B5 | tau | 기억 저장소 자동 연결·프롬프트 투영 없음 | `memory` 도구 미노출, 기억이 프롬프트에 없음 | 중~대 |
| B6 | tau | 배포 바이너리가 `ext-host/host.ts`를 못 찾음(이번에 새로 확인) | 설치본에서 `--extension` 사용 불가 | 소 |
| B7 | tau | `memory_report` RPC 없음 | 게이트웨이 메모리 측정 실패 | 소 |
| N1 | 게이트웨이 | 버전 문자열로 엔진 기능 판정 | tau `0.12.0`이 구버전으로 판정돼 이동이 막히고 매번 reload | 소 |
| N2 | 게이트웨이 | 프로세스 탐색 패턴에 `omt` 없음 | tau 레인을 못 찾음 | 소 |
| N3 | 게이트웨이 | 설치기·워커 배포가 mhc 전용 | tau를 장비에 설치할 수 없음 | 중 |
| N4 | 게이트웨이 | `omo.feedback.*` 확장 질의가 엔진 전제 | tau 레인에서 피드백 명령 실패 | 소 |

카나리 시작 조건은 B1~B4, B6, N1~N3이다. B5, B7, N4는 카나리와 병행해도 되지만 전면 전환 전에는 필요하다.

---

## 1. tau 쪽 블로커

### B1. 확장 공급자 등록이 코어에 연결되지 않음

**현상.** 게이트웨이 인자(`--model omomeow-opus/claude-opus-5-5 --extension src/model-provider.ts --no-model-fallback`)로 띄우면 `model 'omomeow-opus/claude-opus-5-5' not found`로 바로 끝난다.

**원인(코드 확인).**
1. 기동 순서가 반대다. `crates/omp-cli/src/main.rs`에서 `model::select(...)`(463행 부근)가 `ext::Extensions::start(...)`(491행 부근)보다 먼저 실행된다. 확장이 공급자를 등록하기 전에 모델 해석이 끝나 버린다.
2. 알림을 버린다. `ext-host/host.ts`의 `registerProvider`(601행)와 `rollbackLoad`(481행)는 `provider_registered`/`provider_unregistered` 알림을 보내지만, `crates/omp-cli/src/ext.rs`의 `handle_notification`(2135행)은 이 두 메서드를 `_ => {}`로 버린다.
3. 해석기가 모른다. `model.rs::from_providers`는 `omp_providers::resolve_async`만 부르고, 이 함수는 번들 카탈로그, `models.db`, `~/.omp/agent/models.yml`만 본다. `rpc.rs`의 `get_available_models`(1314행)도 `subcmd::models::browser_models`로 같은 출처만 읽고, `set_model`(`runtime.rs:4341` `select_model`)도 같은 해석기(`self.resolver`)를 쓴다.

**등록되는 설정(게이트웨이 `src/model-provider.ts:86-101`).**
```text
registerProvider("omomeow-opus", {
  baseUrl, apiKey, api: "openai-responses", authHeader: true,
  models: [{ id, name?, reasoning: true, input: ["text"], maxTokens: 128000,
             compat: { supportsForcedToolChoice: false }, cost: {0,0,0,0} }, ...]
})
```

**설계.**
1. `omp-providers`에 프로세스 전역 `ExtensionProviders` 저장소를 둔다. 키는 공급자 이름이고, 값은 `{ extension, config }`(위 형식의 파싱 결과)다. `upsert`, `remove`, `remove_by_extension`, `snapshot`을 제공한다. `models.yml`의 사용자 정의 공급자와 같은 내부 표현으로 바꿔 넣어서, 해석 경로를 새로 만들지 않고 같은 모델 객체(`pa_types::ai::Model`)와 자격증명(`apiKey`, `authHeader: true`이면 `Authorization: Bearer`)을 쓴다.
2. 우선순위는 업스트림 TS와 같게 한다. 확장 등록은 같은 이름의 `models.yml` 정의를 덮어쓰고, 등록이 해제되면 원래 정의가 다시 보인다.
3. `resolve_async`, `browser_models`, `select_model`이 이 저장소를 함께 본다. `get_available_models`에는 확장 공급자 모델도 `available: true`로 나온다.
4. 경쟁 조건을 피하려고 알림에만 기대지 않는다. `host.ts`의 `load` 결과(`loaded[]`, 701행)에 이번 적재로 살아 있는 공급자 목록 `providers: [{ name, config }]`를 넣고, Rust의 `ExtHost::load`(`crates/omp-ext/src/host.rs:530`)가 응답을 받으면 저장소를 그 목록으로 맞춘다. 적재 뒤 들어오는 `provider_registered`/`provider_unregistered` 알림은 `handle_notification`에서 같은 저장소에 반영한다.
5. 기동 순서를 바꾼다. `--extension`이나 확장 디렉터리가 있으면 `Extensions::start`와 첫 `load`를 끝낸 뒤 `model::select`를 부른다. 확장이 없으면 지금 순서 그대로다. 확장 시작이 실패하면 지금처럼 경고만 남기고 모델 해석으로 넘어간다(그 경우 확장 공급자 모델은 정상적으로 `not found`).
6. `--no-model-fallback`은 그대로 지킨다. 확장 공급자도 폴백 체인에서 일반 공급자와 똑같이 다룬다.
7. `reload`(B4) 때는 적재 결과로 저장소를 다시 맞춘다. 현재 모델의 공급자가 사라졌으면 세션 모델은 유지하되, 다음 요청에서 `No API key`/`Model not found` 오류를 정직하게 내고 조용히 다른 모델로 바꾸지 않는다.

**테스트.**
- `omp-cli` 통합 테스트: 테스트 확장(`tests/fixtures/provider_ext.ts`)이 목업 서버를 가리키는 `fx-prov`를 등록한다. `omt --mode rpc --model fx-prov/m1 --extension <fixture> --no-model-fallback`이 기동하고, `prompt`가 목업 서버로 가는지, `get_available_models`에 `fx-prov/m1`이 있는지, `set_model fx-prov/m2`가 성공하는지 확인한다.
- 같은 확장이 `unregisterProvider`를 부르면 `get_available_models`에서 빠지는지 확인한다.
- 골든 RPC 스위트에 `extension_provider_boot.jsonl`을 추가한다.

**수용 기준.** 게이트웨이의 실제 `model-provider.ts`와 실제 인자로 기동해서 `get_state.model.provider == "omomeow-opus"`가 나와야 한다.

### B2. `--append-system-prompt` 덮어쓰기

**현상.** 게이트웨이(`src/gateway.ts:1178`, `1181`)는 이 플래그를 두 번 넘긴다. 첫 번째는 `OMOCAT.md` 경로, 두 번째는 소유자 권한 문구다. 실측하면 시스템 프롬프트에 `OMOCAT.md`가 없다.

**원인.** `crates/omp-cli/src/cli.rs:53`이 `append_system_prompt: Option<String>`이고 300행에서 `Some(value)`로 덮어쓴다. `host.rs:101`, `main.rs:413`, `sysprompt.rs:1457`·`2090`도 단일 값으로 전달된다.

**설계.**
- 타입을 `Vec<String>`으로 바꾸고 300행은 `push`한다.
- `sysprompt.rs`는 항목마다 지금 규칙(파일이면 내용, 아니면 문자열 그대로)을 적용하고, 넘어온 순서대로 프로젝트 블록 끝에 빈 줄 하나를 사이에 두고 붙인다.
- `host.rs`(multi-session 호스트가 세션별로 넘기는 경로)와 세션 재개 경로도 목록 그대로 전달한다.

**테스트.**
- `cli.rs` 단위 테스트: 두 번 넘기면 두 값이 순서대로 남는지.
- `sysprompt.rs` 테스트: 파일 하나와 문자열 하나를 넘기면 둘 다, 같은 순서로 들어가는지.
- multi-session 호스트 경로의 회귀 테스트 하나.

**수용 기준.** 게이트웨이 인자로 기동한 뒤 `get_state.systemPrompt`에 `OMOCAT.md`의 첫 줄과 소유자 문구가 함께 있어야 한다.

### B3. `get_state`·`abort(awaitIdle)` 응답 스키마

**게이트웨이가 요구하는 것(코드 확인).**
- `src/relay/agentd-sessions.ts:12`: `pendingMessageCount: z.number()`, `sessionFile: z.string()`가 필수다. 없으면 파싱이 실패한다.
- `src/relay/agentd-sessions.ts:203`: 세션 이동 때 `{ type: "abort", awaitIdle: true, effectId }`를 보내고 `z.object({ idle: z.boolean() })`로 파싱한다.
- `src/lane-idle.ts:17`, `src/worker-rpc.ts:25`·`55`: `isBashRunning`, `pendingMessageCount`로 유휴를 판정한다(없으면 기본값을 쓴다).
- `src/worker-rpc.ts:216`, `src/brain.ts:18`: `usageTotals = { input, output, cacheRead, cacheWrite, cost }`.

**tau 현재 상태.** `runtime.rs::state`(2650~2700행)에는 `queuedMessageCount`만 있고 `pendingMessageCount`, `isBashRunning`, `usageTotals`가 없다. `sessionFile`은 파일이 있을 때만 넣는다. `rpc.rs:1048`의 `abort`는 `awaitIdle`을 무시하고 빈 `data`를 돌려준다.

**설계.**
1. `pendingMessageCount`: steering 큐와 follow-up 큐 길이의 합(`queuedMessageCount`와 같은 값)에, 아직 턴에 넣지 않은 확장 메시지 수를 더한다. `queuedMessageCount`는 기존 클라이언트(웹 UI, collab)를 위해 그대로 둔다.
2. `isBashRunning`: 포그라운드로 실행 중인 `bash` 도구 호출이 하나라도 있으면 `true`다. 백그라운드 작업(`bash_output`으로 읽는 세션)은 넣지 않는다. 지금은 이 값을 세는 곳이 없으므로, `tools/bash.rs`의 포그라운드 실행 시작과 종료(취소·오류 포함)에서 증감하는 런타임 카운터를 새로 둔다.
3. `usageTotals`: 세션 전체 누적값 `{ input, output, cacheRead, cacheWrite, cost }`다. 세션 통계(`get_session_stats`)가 이미 모으는 값을 그대로 쓰고, 하위 에이전트 사용량은 `subagents` 필드로 따로 준다(게이트웨이 `project-memory.ts:110`의 형식).
4. `sessionFile`: 파일이 없는 세션(`--no-session`)에서도 키를 넣는다. 다만 게이트웨이 스키마가 문자열을 요구하므로, 이동 대상이 될 수 없는 세션이라는 것을 `sessionFile: ""`와 `persistent: false`로 표시한다. 게이트웨이는 `persistent: false`인 세션을 이동 후보에서 뺀다(N1에서 함께 수정).
5. `abort`: `awaitIdle: true`이면 중단을 건 뒤, 에이전트 상태가 유휴(`!isStreaming && !isCompacting && !isBashRunning`)가 되는 이벤트를 구독해서 기다린다. 기본 상한은 30초이고 `awaitIdleTimeoutMs`로 바꿀 수 있다. 응답은 `{ idle: true|false }`다. `awaitIdle`이 없을 때도 `{ idle: <현재 상태> }`를 돌려주되, 기다리지는 않는다. 고정 sleep이나 폴링은 쓰지 않는다.
6. 게이트웨이는 `abort`에 `effectId`를 붙여 보낸다. tau `rpc.rs`에는 아직 `effectId` 처리가 없다. 이번에 같은 id의 재시도에는 처음 결과를 그대로 돌려주는 작은 원장(세션별, 최근 N개)을 추가한다.

**확인 필요.** mhc가 `usageTotals`에 비용을 어떤 단위로 넣는지(USD 소수인지)는 mhc `get_state`를 한 번 실측해서 맞춘다. 게이트웨이는 숫자인지만 검사한다.

**테스트.**
- 골든 RPC: 기존 `get_state` 프레임 7곳에 새 필드를 반영하고, `interrupt_abort_mid_turn.jsonl`에 `abort awaitIdle` → `{ idle: true }` 경우를 추가한다.
- 목업 `hold` 모델로 턴을 잡아 둔 상태에서 `abort awaitIdle`을 보내면 턴 종료 이벤트 뒤에 응답이 오는지 확인한다(이벤트 순서로 검증하고 시간으로 검증하지 않는다).
- 포그라운드 `bash` 실행 중에 `get_state.isBashRunning == true`인지 확인한다.

**수용 기준.** 게이트웨이의 `agentd-sessions.ts` 스키마를 그대로 옮긴 zod 검증 스크립트(`tests-e2e/gateway-schema.ts`)가 실제 응답을 통과시켜야 한다.

### B4. `reload` 때 확장 중복 등록

**현상.** 보류를 풀 때 게이트웨이가 `reload`를 보내면(`src/lane-hold.ts:64`) `duplicate extension rpc handler: omomeow.move-readiness`로 `model-provider.ts` 재적재가 실패한다.

**원인.** `rpc.rs:1100 reload_with_ext` → `ext.rs:1678 Extensions::reload` → `recover()`가 같은 호스트 프로세스에 `load`를 다시 보낸다. `host.ts`의 `load`(684~724행)는 기존 `exts`를 비우지 않고 `exts.push`만 하고, `rpcHandlers`(424행)도 남아 있다. 그래서 `rpc.handle`이 556행에서 던진다. RPC만 문제가 아니다. 도구, 이벤트 핸들러, 명령, 공급자, 플래그 값이 모두 두 벌이 된다. 지금은 RPC 등록만 예외를 던져서 드러났을 뿐이다.

**설계.**
1. 성공한 적재에도 `LoadTracker`를 `Ext`에 남긴다. 지금은 실패한 적재의 되돌리기(`rollbackLoad`, 451행)에서만 쓴다.
2. `host.ts`에 `unload_all` 요청을 추가한다. 각 확장에 `session_shutdown`(사유 `reload`)을 보낸 뒤, 그 확장의 추적 기록으로 구독을 풀고, RPC 이름, 공급자(`provider_unregistered` 알림 포함), 플래그 값, 링크 스킴을 정리하고 `exts`를 비운다. `rollbackLoad`와 같은 소유권 규칙(다른 확장이 나중에 쓴 값은 건드리지 않음)을 쓴다.
3. Rust `Extensions::reload`는 `recover()` 전에 `unload_all`을 보낸다. 투명 재연결(새 프로세스) 경로는 비어 있는 호스트에 `unload_all`을 보내게 되는데, 아무 일도 하지 않으므로 그대로 둔다.
4. 업스트림 TS가 재적재 때 `session_shutdown`을 보내는지는 구현 전에 `packages/coding-agent`의 reload 경로에서 확인하고, 다르면 업스트림을 따른다.
5. `reload`가 실패하면 지금처럼 `CmdError`로 돌려준다. 실패한 확장은 `rollbackLoad`로 정리되므로 다음 `reload`가 다시 깨지지 않는다.

**테스트.**
- `crates/omp-ext/tests/host.rs`: `rpc.handle`, `registerTool`, `registerProvider`, `on("turn_end")`를 쓰는 고정 확장을 적재하고 두 번 `reload`한다. RPC 응답이 되는지, `list_tools` 개수가 그대로인지, 이벤트 핸들러가 한 번만 불리는지, 공급자 알림이 해제 1회와 등록 1회인지 확인한다.
- 골든 `lane_hold_stop_reload_resume.jsonl`을 RPC 확장(`tests/fixtures/rpc_ext.ts`)을 붙인 형태로 하나 더 만든다.

**수용 기준.** 실제 `model-provider.ts`로 기동한 레인에서 `interrupt(stop)` → `reload` → `resume_session` 뒤 `extension_request omomeow.move-readiness`가 응답해야 한다.

### B5. 기억 저장소 자동 연결과 프롬프트 투영

**현상.** `crates/omp-cli/src/tools/memory.rs:199-203`은 `MEMORY_DIR`이 있을 때만 `memory`/`memory_apply_patch`를 노출한다. 게이트웨이는 이 변수를 넘기지 않는다. mhc는 작업 폴더로 기억 저장소를 스스로 찾아 열고, 기억을 시스템 프롬프트에 넣는다.

**경로 규칙(게이트웨이 `src/memory-sync.ts:30-48`과 같게).**
- `hash = sha256(resolve(cwd))` 16진수의 앞 8자
- `slug` = 경로 마지막 이름을 NFKD로 펴고, 결합 문자를 지우고, 소문자로 바꾸고, 문자·숫자 아닌 것을 `-`로 바꾸고, 연속 `-`를 줄이고, 양끝 `-`를 지우고, 40자로 자른 값(비면 `agent`)
- `legacy` = 같은 과정을 ASCII `[a-z0-9]`만 남겨 한 값
- 저장소 = `<기억 홈>/agents/<slug>-<hash>/repo`. 이게 git 저장소가 아니고 `<legacy>-<hash>/repo`가 git 저장소이면 legacy 쪽을 쓴다.
- 기억 홈 = `OMO_MEMORY_HOME`, 없으면 `~/.omo/memory`

검증값: `/home/whrho/git/oh-my-tau` → `oh-my-tau-3eb9c32a`. 이 작업 세션이 실제로 묶인 기억 id와 같다.

**설계.**
1. `MEMORY_DIR`이 있으면 지금처럼 그 값을 쓴다. 없으면 위 규칙으로 저장소를 찾는다. 저장소가 없을 때 mhc가 새로 만드는지는 구현 전에 mhc를 실측해 확인하고 같은 동작을 한다. 확인 전까지는 만들지 않고 도구도 노출하지 않으며, 그 사실을 기동 로그에 한 줄 남긴다.
2. `AGENT_ID`가 없으면 `<slug>-<hash>`를 커밋 작성자 id로 쓴다.
3. 프롬프트 투영. 세션이 시작될 때 저장소 HEAD를 고정(pin)하고 시스템 프롬프트에 기억 섹션을 만든다.
   - `system/` 아래 블록은 본문까지 넣는다. 앞머리 `expires`가 지난 블록은 뺀다.
   - 나머지 블록은 경로와 `description`만 목록으로 넣는다.
   - 섹션 머리에 고정한 커밋을 적는다.
4. 변경 알림. 턴을 시작할 때 HEAD가 고정값과 다르면, 프롬프트는 그대로 두고 바뀐 파일 목록만 짧은 알림으로 넣는다(고정한 프롬프트를 지키므로 캐시가 깨지지 않는다). 압축(compaction) 뒤에는 새 HEAD로 다시 고정한다.
5. 정확한 섹션 형식과 알림 문구는 mhc가 실제로 만드는 것을 한 세션 실측해서 따른다. 내용과 순서는 위 규칙대로 하고, 겉모양은 mhc 출력을 따른다.

**테스트.**
- 경로 규칙 단위 테스트: 위 검증값, 한글 폴더 이름, 기호만 있는 이름(`agent`), legacy 대체. 게이트웨이 구현과 같은 결과인지 몇 개 경로로 대조한다.
- 임시 기억 홈에 `system/a.md`, `system/old.md`(만료), `reference/b.md`를 만들고 `get_state.systemPrompt`에 a의 본문과 b의 설명이 있고 old는 없는지 확인한다.
- 턴 사이에 커밋을 하나 넣으면 다음 턴에 변경 알림이 들어가고 고정한 프롬프트는 그대로인지 확인한다.

**수용 기준.** `MEMORY_DIR` 없이 게이트웨이 인자로 기동한 레인에서 `memory` 도구가 보이고, 그 프로젝트의 `system/` 기억이 프롬프트에 있어야 한다.

### B6. 배포 바이너리가 확장 호스트를 못 찾음 (새로 확인)

**원인.** `ext.rs:121 host_script()`는 `OMP_RS_EXT_HOST` 환경변수가 없으면 바이너리 위치에서 위로 올라가며 `ext-host/host.ts`를 찾는다. 그런데 `.github/workflows/release.yml`은 바이너리 하나만(`omt-linux-x64` 등) 올린다. 설치본에서는 `--extension`을 쓰는 순간 `ext-host/host.ts not found`가 난다. 재검증은 저장소 체크아웃에서 빌드한 바이너리로 했기 때문에 이 문제가 드러나지 않았다.

**설계.**
- 릴리스 산출물을 `omt-<os>-<arch>.tar.gz`(`bin/omt`, `ext-host/` 전체, `LICENSE`, `NOTICE.md`)와 `.sha256`로 바꾼다. 맨 바이너리도 하위 호환을 위해 계속 올린다.
- `host_script()`의 탐색 순서에 설치 레이아웃 `<prefix>/share/omt/ext-host/host.ts`를 더한다.
- 설치 스크립트 `install.sh`를 저장소 루트에 둔다. 동작은 mhc 설치기와 같다: `~/.local/share/omt/versions/<v>/`에 풀고 `~/.local/bin/omt`를 가리키게 하며, 끝나면 `omt --version`으로 확인한다.
- 확장 호스트는 Bun이 필요하다. 워커에는 이미 Bun이 있지만, 설치기가 `bun --version`을 확인해서 없으면 실패하게 한다.

**수용 기준.** 저장소 밖의 빈 디렉터리에 tar.gz를 풀어 `--extension` 기동이 되어야 한다.

### B7. `memory_report` RPC

**게이트웨이 사용처.** `src/worker-rpc.ts:329`가 `{ type: "memory_report" }`를 보내고 `{ path }`를 받는다. `gateway.ts:2613`은 `footprintBytes`도 기록한다.

**설계.** 프로세스 메모리 보고서를 JSON 파일로 남기고 `{ path, footprintBytes }`를 돌려준다.
- Linux에서는 `/proc/self/status`(VmRSS, VmHWM)와 `/proc/self/smaps_rollup`을 읽는다.
- macOS에서는 `task_info(TASK_VM_INFO).phys_footprint`를 읽는다.
- 그 밖의 플랫폼에서는 `unsupported` 코드로 오류를 돌려준다. 가짜 숫자는 만들지 않는다.
- 같은 보고서에 세션 수, 메시지 수, 확장 호스트 프로세스의 RSS도 넣는다.

---

## 2. 게이트웨이(`omo-nyang`) 쪽

### N1. 버전 대신 기능으로 엔진 판정

- `src/session-move.ts:60 supportsSessionMove(version)`은 `omt --version`의 `0.12.0`을 major 0으로 읽어 이동을 막는다. 버전 문자열은 `session-move-runtime.ts:212`에서 `--version`으로 얻는다.
- `src/lane-hold.ts:48 engineResumesRun(serverVersion)`도 버전 패턴으로 판정해서, tau 레인은 보류를 풀 때마다 `reload`한다.

설계:
- `get_protocol_info`의 `engine == "oh-my-tau"`, `protocolVersion >= 2`, `capabilities`에 `move_session`이 있으면 이동을 지원한다고 판정한다. 기존 mhc/omo 판정은 그대로 둔다.
- 이동 판정이 `--version` 셸 호출에서 시작하므로, `omt`(`binary` 이름 또는 `--version` 출력이 semver 하나뿐인 경우)는 대상 장비에서 짧게 `get_protocol_info`를 묻는 경로를 추가한다.
- tau `get_protocol_info`에 `features: ["resume_run", ...]`을 더한다(tau 쪽 작은 변경). `resume_session`만으로 보류된 실행을 다시 잇는지를 골든 테스트로 증명한 뒤에 `resume_run`을 켠다. 그 전까지는 tau 레인도 `reload`를 하고, B4로 그 `reload`가 안전해진다.
- B3의 `persistent: false` 세션은 이동 후보에서 뺀다.

### N2. 프로세스 탐색 패턴

`src/gateway.ts:2376`의 패턴 `senpi/dist/bundle/cli[.]js|mh[c] --mode rpc`에 `om[t] --mode rpc`를 더한다. `src/runtime-install.ts:113 enrollFactsCommand`의 엔진 탐색에도 `~/.local/bin/omt`를 넣는다(우선순위는 설정에서 엔진을 고른 경우에만 omt).

### N3. 엔진 일반화한 설치·배포

`src/runtime-install.ts`는 저장소(`jc01rho/mhc`), 산출물 이름, 설치 경로, `mhc --version` 확인이 고정돼 있다.

설계:
- 엔진 기술자 `{ engine: "mhc" | "omt", repo, assetName(os, arch), installDir, binName }`을 두고, 지금 코드는 그 mhc 기술자로 동작하게 바꾼다.
- `--engine omt`이면 `jc01rho/oh-my-tau`의 B6 tar.gz와 `install.sh`를 받는다.
- `worker-deploy.ts`와 설정의 `worker.engine`(장비별 덮어쓰기 가능)으로 레인 엔진을 고른다. 기본값은 mhc다.
- 레인 실행 인자 생성부(`gateway.ts:1170` 부근)는 엔진마다 다른 플래그를 쓰지 않도록 맞춘다. tau는 1차 설계에서 mhc 플래그를 모두 받게 했으므로, 엔진 바이너리 경로만 바뀐다.

### N4. 엔진에 없는 확장 질의 처리

`src/omo-feedback.ts:41`·`50`은 `extension_request omo.feedback.prepare/submit`을 부르는데, 이 처리기는 omo 하네스가 등록한다. tau 레인에는 없다. 오류 코드 `unknown_extension_request`(tau `rpc.rs`가 이미 이 코드를 돌려준다)를 받으면 "이 레인의 엔진은 omo 피드백을 지원하지 않음"으로 정직하게 답하고, 재시도하지 않는다. `memory_report`도 tau가 B7 전이라 `Unknown command`를 돌려주면 같은 방식으로 기록만 하고 넘어간다.

---

## 3. 카나리 계획

**대상.** 워커 1대(linux5 권장: oh-my-tau 체크아웃과 빌드 도구가 있고 그동안 게이트를 돌려 온 장비). 설정에서 그 장비의 `worker.engine`만 `omt`로 바꾼다.

**시나리오.** 전부 실제 LLM 호출로 한다.
1. 새 작업 레인 기동: 모델이 `omomeow-opus`이고 시스템 프롬프트에 `OMOCAT.md`가 있는지 확인한다.
2. 도구 호출과 스트리밍이 있는 턴, `agent_settled`로 작업 종료를 판정한다.
3. 하위 에이전트(`task`, `task_send`, `task_output`), `workpool`, `monitor`, 백그라운드 셸.
4. 보류(`interrupt` stop) → `reload` → `resume_session`, 그 뒤 `extension_request omomeow.move-readiness`.
5. 기억: 자동 연결된 저장소에 쓰기 → 다음 턴 변경 알림 → 게이트웨이 기억 동기화.
6. 장비 간 세션 이동: tau 장비 두 대 사이, 그리고 `abort awaitIdle`이 포함된 경로.
7. `memory_report`, `get_state.usageTotals`가 게이트웨이 보고에 숫자로 남는지.

**판정.** 7개 시나리오가 각 3회 연속 통과하고, 같은 기간 mhc 레인과 비교해 작업 실패율이 높지 않으면 2대로 넓힌다.

**되돌리기.** 그 장비의 `worker.engine`을 `mhc`로 되돌리면 다음 레인부터 mhc로 돈다. 진행 중이던 tau 레인은 끝까지 두거나 세션 이동으로 mhc 장비에 넘긴다(JSONL v3 호환은 1차 설계에서 확인됨).

---

## 4. PR 분할과 순서

| 순서 | PR | 저장소 | 내용 | 의존 |
| --- | --- | --- | --- | --- |
| 1 | T1 | tau | B2 `--append-system-prompt` 목록화 | 없음 |
| 2 | T2 | tau | B3 `get_state` 필드, `abort awaitIdle`, `persistent` | 없음 |
| 3 | T3 | tau | B4 `unload_all`과 reload 정리 | 없음 |
| 4 | T4 | tau | B1 확장 공급자 저장소와 기동 순서 | T3(재적재 때 공급자 재정합) |
| 5 | T5 | tau | B6 tar.gz 산출물, `install.sh`, 설치 레이아웃 탐색 | 없음 |
| 6 | T6 | tau | B7 `memory_report`, `get_protocol_info.features` | 없음 |
| 7 | T7 | tau | B5 기억 자동 연결과 투영 | mhc 실측 2건(저장소 생성, 섹션 형식) |
| 8 | R1 | tau | `v0.13.0` 태그와 릴리스 | T1~T6 |
| 9 | G1 | 게이트웨이 | N1 기능 기반 엔진 판정 | T2, T6 |
| 10 | G2 | 게이트웨이 | N2 탐색 패턴, N4 미지원 처리 | 없음 |
| 11 | G3 | 게이트웨이 | N3 엔진 일반화한 설치·배포 | R1 |
| 12 | C1 | 운영 | 워커 1대 카나리 | R1, G1~G3 (T7은 병행 가능) |

T1, T2, T3, T5, T6과 G2는 서로 독립이라 동시에 진행할 수 있다. 모든 tau PR은 기존 머지 게이트(fmt, clippy 2종, 워크스페이스·omp-cli 테스트, `compare.ts`, `check-tools-parity.ts`, `check-golden.ts`)를 통과해야 하고, 골든 RPC 스위트를 갱신해야 한다.

---

## 5. 열린 질문 (구현 전에 실측으로 정할 것)

1. mhc는 `<기억 홈>/agents/<id>/repo`가 없을 때 새로 만드는가, 아니면 기억 없이 도는가? (B5)
2. mhc의 기억 섹션 형식과 변경 알림 문구. (B5)
3. mhc `get_state.usageTotals.cost`의 단위. (B3)
4. 업스트림 `packages/coding-agent`는 확장을 재적재할 때 `session_shutdown`을 보내는가? (B4)
5. tau `resume_session`은 `reload` 없이 보류된 실행을 다시 잇는가? (N1의 `resume_run`)
