# 리뷰 조각 07: 게이트웨이 호환 기능 추가 시 `oh-my-tau` 고유 자산(`/ultra`, `omp-judge`/`laya-ort`, `omp-goose`, `omp-sot`, `omt web`) 비파괴 감사 (rev-07-tau-unique-assets-preservation-audit)

## 1. 목적 및 코드 근거 (File & Line)

게이트웨이(OmOMeow) 호환을 위해 RPC 명령, CLI 플래그, 도구, 이벤트를 추가하는 작업은 `oh-my-tau` 고유 자산을 조용히 잠식할 위험이 있다. 본 조각은 "추가만 하고, 기존 표면은 줄이지 않는다"를 기계적으로 증명하는 감사 하네스를 정의한다.

- RPC 단일 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 신규 명령은 반드시 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))` **앞**에 추가한다. 이 폴백의 메시지 포맷은 계약이므로 변경 금지.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226` `response()`가 조립하고, `omo-nyang/src/relay/move-host.ts:4-7, 61`이 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱한다. 실패 시 `code`와 `errorCode`를 동시 직렬화해야 하므로 감사 대상에 포함.
- 프로토콜 표면: `get_protocol_info()`는 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}` (`omo-nyang/src/lane-hold.ts:29-76`). `commands` 배열은 게이트웨이가 기능 탐지에 쓰는 **공개 ABI**다.
- CLI 표면: `crates/omp-cli/src/cli.rs:97-166`. 현재 `--skills <CSV필터>`만 존재(`cli.rs:129,309`)하며 `omo-nyang/src/gateway.ts:1167`은 `--skill <디렉터리경로>`를 보낸다. `omt web` 서브커맨드도 같은 enum에 있다.
- 설정/세션 자산: 설정 루트는 `crates/omp-settings/src/lib.rs`의 `~/.omp`, 세션은 `crates/omp-session/src/entries.rs`의 JSONL v3(256B title slot + camelCase). `~/.omo`/`omo.jsonc`로의 무단 이관은 금지.
- 종료 신호: `crates/omp-cli/src/runtime.rs:1641`은 `{"type":"session_settled"}`만 stdout에 쓰고, `agent_settled`는 `ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 게이트웨이 대응(`omo-nyang/src/relay/runtime.ts:59`)은 **추가**여야 하며 `session_settled` 제거는 불가.

감사 대상 고유 자산 5종의 좌표는 실행 시점에 고정한다: `rg -n 'ultra' crates/omp-cli/src`, `rg -n 'laya|judge' crates/`, `rg -n 'goose' crates/`, `rg -n 'sot' crates/`, `rg -n 'Web' crates/omp-cli/src/cli.rs`. 발견 좌표는 아래 매니페스트에 커밋한다.

## 2. 상세 계약 및 데이터/타입 명세

```rust
// crates/omp-cli/tests/unique_assets.rs (신규)
pub struct UniqueAsset {
    pub name: &'static str,          // "ultra" | "judge-laya-ort" | "goose" | "sot" | "web"
    pub entry: AssetEntry,           // Rpc(&'static str) | Cli(&'static str) | Slash(&'static str)
    pub probe: &'static str,         // 실행 가능한 스모크 명령
    pub feature: Option<&'static str>, // cargo feature (없으면 항상 기대)
}
pub struct AssetEntry { /* kind, symbol, source: (file, line) */ }
```

불변식(INV):

- INV-1 (명령 상위집합): `get_protocol_info().commands`는 기준 스냅샷의 모든 원소를 순서 보존 부분수열로 포함한다. 삭제·개명 금지, 신규는 뒤에 append.
- INV-2 (폴백 불변): 미지원 명령 응답은 `success:false`, `error == format!("Unknown command: {other}")`, `errorCode` 존재.
- INV-3 (프로토콜): `protocolVersion == 2`, `engine == "oh-my-tau"`, `serverVersion == CARGO_PKG_VERSION`.
- INV-4 (CLI): `--skills`(CSV 필터) 유지 + `--skill`(반복 가능 `Vec<PathBuf>`) 추가. `omt web` 및 `omt --mode rpc --multi-session --session-runtime in-process` 조합 유지.
- INV-5 (설정): 기본 설정 루트는 `~/.omp`. `~/.omo`는 명시적 `--config`/env가 있을 때만 읽는다.
- INV-6 (세션): JSONL v3 라인 스키마 불변. `omo-nyang/src/session-patterns.ts:47-76`이 읽는 `{timestamp, message:{role, content:[{type:"toolCall",name,arguments,id}], toolCallId, isError}` 필드명 유지.
- INV-7 (이벤트): `session_settled` 유지, `agent_settled`는 additive. 한 턴에 정확히 1회씩.
- INV-8 (자산 도달성): 5종 자산 각각 `probe`가 exit 0.

감사 리포트 스키마(JSON, `target/unique-assets-audit.json`):

```json
{"schemaVersion":1,"commit":"<sha>","protocol":{"version":2,"engine":"oh-my-tau","commands":["..."]},
 "assets":[{"name":"ultra","entry":{"kind":"slash","symbol":"ultra"},"status":"pass","probe":"..."}],
 "invariants":[{"id":"INV-1","status":"pass"}],"removed":[],"added":["bash_output","memory","workpool"]}
```

`removed`가 비어 있지 않으면 실패. `added`는 허용 목록과 대조한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

(1) 디스패치 추가 규칙 — 기존 arm 무수정, 폴백 직전에 삽입:

```rust
// rpc.rs: dispatch 내부, 1094 폴백 바로 위
"memory" | "memory_apply_patch" | "bash_output" | "kill_bash"
| "task_send" | "task_output" | "task_cancel" | "workpool"
| "ask_user_question" | "create_goal" | "get_goal" | "update_goal" => {
    crate::compat::dispatch_compat(rt, cmd, kind, other)
}
other => Err(CmdError::new(format!("Unknown command: {other}"))),
```

`dispatch_compat`는 미등록 이름을 다시 `Unknown command`로 반환해 INV-2를 보존한다.

(2) CLI 플래그 추가 — `cli.rs`의 `Args`에 필드追加, 기존 `skills: Option<String>` 유지:

```rust
#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
pub skill_paths: Vec<std::path::PathBuf>,   // 추가 루트, 우선순위: builtin < --skill < (--skills는 필터만)
```

스킬 해석 순서를 명시적으로 고정: 내장 스킬 디렉터리 → `skill_paths` 순서대로 → `--skills` CSV는 **이름 필터**로만 작동(루트 추가 아님). `/ultra` 부트스트랩 스킬이 `--skill` 루트에 의해 가려지면 이름 충돌 시 내장 우선.

(3) 자산 스모크 하네스(의사코드):

```
for a in MANIFEST:
  match a.entry:
    Slash(s) => rpc("prompt", {text: format!("/{s} --dry-run")}) ; assert !is_unknown_command
    Rpc(c)   => rpc(c, {}) ; assert resp.type=="response" && resp.id==sent_id
    Cli(c)   => run([exe, c, "--help"]) ; assert exit==0
  assert a.feature.is_none() || cfg!(feature=a.feature)
```

(4) 프로토콜 스냅샷 비교: 기준 파일 `crates/omp-cli/tests/golden/protocol_commands.json`을 읽어 `is_subsequence(golden, live)` 검사. `reload()`(`lane-hold.ts` 계약) 이후 재조회하여 스킬 재스캔이 `commands`를 흔들지 않는지 2회 측정.

(5) 종료 신호 이중화: `runtime.rs:1641` 근처에서 `session_settled` 직후 `agent_settled`를 같은 turn id로 1회 emit
