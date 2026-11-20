# oh-my-tau Rust 전환 마이그레이션 계획

상태: 확정안 (2026-11-20)
대상: `jc01rho/oh-my-tau` (`omt`, MIT) — 기준 커밋 `25af44a` (`v0.12.0` + PR #3 `features-wave3`)
기준 업스트림: `can1357/oh-my-pi` `main` (`b636e19d10`, `v19.0.1+126`) + `omp2` 브랜치 (`2f92f3b`)
참고 선례: `mhc` (omo → Rust 전환 계획)
조사 근거: `docs/research/01~24` (아래 §10 근거 목록)

---

## 1. 요약

`oh-my-tau`는 이미 1차 Rust 전환을 마친 코드베이스다. 20개 `omp-*` 크레이트와 12개 벤더 코드
크레이트(총 778 `.rs` 파일, 372,230줄)로 `v0.12.0` 및 wave 1~3까지 진행됐고, `omp-cli`
70,497줄·`omp-tui` 66,466줄 규모의 Rust 구현이 동작한다. 따라서 이 계획의 목표는
"TypeScript를 처음부터 Rust로 옮기는 일"이 아니라 **이미 옮긴 Rust 코드베이스를 업스트림의
현재 설계와 재정렬하고, 벤더 API 드리프트·미구현 격차·남은 Bun 의존(`ext-host/`)을
단계적으로 해소하는 것**이다.

업스트림 `can1357/oh-my-pi` 자체는 두 트랙으로 분기되어 있다.

| 트랙 | 위치 | 성격 | 규모 |
| --- | --- | --- | --- |
| `main` | `can1357/oh-my-pi` (`main`) | TS 하네스 + 증분 Rust 네이티브(`crates/natives` N-API, `sdk/rust/omp-rpc`) | TS 2.37M줄 / Rust 0.86M줄 (13 크레이트) |
| `omp2` | 같은 저장소의 `omp2` 브랜치 | TS를 완전히 폐기한 순수 Rust 재작성 (임베디드 CPython 3.14t 확장) | Rust 1.03M줄 / 46 크레이트, ADR 36건 |

`oh-my-tau`는 `PrimeIntellect-ai/prime-agent` 포크(`vendor/pa-*`) 위에 `main` 트랙의
`pi-*` 네이티브 크레이트 6종(18.6.1~18.9.1 스냅샷)을 벤더링하고, 나머지를 독자 `omp-*`
크레이트와 wave 1~3 기능으로 채운 **3계층 혼합형**이다.

핵심 판단 5가지:

1. **`omp2`는 "복제 대상"이 아니라 "아키텍처·규율 참조"다.** `omp2`의 크레이트 경계·ADR 36건·`clippy`/`lintx` 규율은 흡수 가치가 크지만, `omp2`는 TS 생태계를 버리고 Python 확장과 `.oms`/DOM 저널로 갈아탔다(ADR 0003/0036). `oh-my-tau`는 `~/.omp/agent` 설정·JSONL v3 세션·TS 확장 호환을 제품 계약으로 내걸었으므로 `omp2`를 그대로 복제하면 호환 계약이 깨진다.
2. **mhc(omo 전환)의 클린룸 절차는 버리고, 핀·원장·델타 규율만 가져온다.** mhc가 클린룸과 `cargo feature` 격리(Phase 0~5)를 택한 이유는 원본 omo가 비오픈소스 라이선스(SUL-1.0)였기 때문이다. `oh-my-tau`·`oh-my-pi`·`prime-agent`·`crossterm`은 전부 MIT이므로 법적 오염 위험이 없다. 대신 mhc에서 가장 가치 있는 **업스트림 SHA 핀 고정(`vendor-src.lock`), 패리티 원장, 골든/오라클 테스트, 델타 노트**를 도입한다.
3. **가장 큰 체감·구조 격차는 4개 축이다**: (a) **셸·빌트인 패리티 0** (`bash -c` 스폰 vs `omp2` brush 포트), (b) **도구 18/29(62%) + 메모리 크레이트 부재**, (c) **`vendor/pi-*` 버전 드리프트** (`pi-ast` API 전면 파괴, `pi-walker` 삭제→`omp-xfs` 흡수), (d) **`omp-tui` 66.4k줄의 미완 표면**(약 64개 오버레이·통합 터미널 기능 행렬 부재, E2E PTY 하네스 CI 미연동).
4. **조용한 열화(silent degradation)가 가장 위험한 결함이다.** `ext-host/`(Bun, 1,486줄)는 `ExtensionAPI` 약 12개 메서드를 `noop`으로 흘려보내고 `runEphemeralTurn`은 throw한다. `omp-settings`는 메모리 74개 키를 선언만 하고 소비하지 않으며, `/memory`와 `rule://`은 더미/빈 벡터를 반환한다. `tests-tools/ts_tools.json` 자체도 최신 TS(`ask` HTML 폼 등)와 어긋나 있다.
5. **검증 게이트가 선행되어야 한다.** 현재 CI는 `cargo test --workspace` + 3-OS 빌드 + `musl`만 돈다. 저장소에 이미 있는 `tests-golden/`, `tests-tools/`, `tests-e2e/`, `scripts/compat-oracle/`가 CI에서 돌지 않아 `compare-baseline.txt`의 19개 불일치조차 방치되어 있다.

---

## 2. 현재 상태 실측

### 2.1 oh-my-tau 코드 규모 (2026-11-20 실측, `target/`·`.git/`·`.wsckpt/` 제외)

| 영역 | `.rs`/`.ts` 파일 | 줄 수 | `#[test]` 수 | 비고 |
| --- | ---: | ---: | ---: | --- |
| `crates/omp-cli` | 93 | 70,497 | 515 | `src/` 88파일 70,125줄 + `tests/` 2파일; `runtime.rs` 6,028줄, `slash.rs` 3,318줄, `web/` UI 포함 |
| `crates/omp-tui` | 103 | 66,466 | 761 | 전체 크레이트의 27.8%, `ratatui` 즉시 모드 |
| `crates/omp-stats` | 86 | 21,153 | 185 | `@oh-my-pi/omp-stats` 포트 + React 번들 서빙 |
| `crates/omp-providers` | 28 | 17,867 | - | 4계층 모델 조회, `models.db` wyhash, `agent.db` v8 CAS |
| `crates/omp-collab` | 13 | 12,412 | 127 | 호스트·게스트·릴레이·`serve-collab` |
| `crates/omp-lsp` | 18 | 7,697 | 9 | `mux/server.rs`(1,087줄) 세션 공유 데몬 이미 구현 |
| `crates/omp-compact` | 22 | 6,890 | 24+ | 요약·재구축·`handoff.rs` 완비(`snapcompact` 렌더러·`cli` 배선 없음) |
| 나머지 13개 `omp-*` | 103 | 35,725 | 380 | `settings`(4,955), `urls`(4,705), `ext`(4,336), `task`(4,052), `session`(3,729), `eval`(3,205), `mcp`(2,839), `auth-broker`(2,582), `sot`(2,347), `judge`(1,710), `wasm`(741), `sandbox`(377), `goose`(147) |
| **`crates/` 합계 (20개)** | **466** | **238,707** | - | |
| `vendor/pa-*` (4개) | 180 | 78,381 | - | `pa-ai`(52,276), `pa-types`(13,240), `pa-agent`(8,123), `pa-models`(4,742) |
| `vendor/pi-*` (6개) | 75 | 43,019 | - | `pi-edit`(22,529), `pi-vfs`(6,060), `pi-walker`(5,739), `pi-predict`(4,169), `pi-ast`(3,600), `pi-diff`(922) |
| `vendor/crossterm` | 55 | 11,584 | - | `0.28.1` + `modifyOtherKeys` 단일 패치(`parse.rs:642`) |
| `vendor/laya-ort` | 2 | 539 | - | Laya judge CPU ONNX 추론 (`laya-judge` CLI, MIT) |
| **`vendor/` 합계 (12개)** | **312** | **133,523** | - | |
| `ext-host/` (TS/Bun) | 4 | 1,486 | - | `host.ts`(1,014), `shard.ts`(190), `legacy-typebox.ts`(176), `session.worker.ts`(102) |

### 2.2 커밋 이력과 개발 패턴

```
89fdbae  initial public release (v0.9.x 계열)
549bd39  v0.10.0: faster RPC exit, fault handling, --session, install script
95da100  features wave 1 (PR #1): /plan /wt /btw /loop /shake /restart /skills
         /marketplace /share /ssh /browser /computer /budget /sandbox, daemon
dc68ab2  Portability: Windows/macOS/arm64 Linux
2dc9ce1  features wave 2 (PR #2): static musl, 3x session scan, WASM(wasmi),
         goose dedup, parallel/cancellable tools
918731f  v0.12.0 + glibc/musl 릴리스 파이프라인 수정
63c950a  features wave 3 (PR #3, 25af44a): 나머지 14개 프로바이더 /login 포팅
```

웨이브별로 폭넓게 기능을 추가해 왔으나, **각 웨이브 뒤 패리티 회귀 검증과 업스트림 벤더
핀 동기화는 수반되지 않았다**.

### 2.3 현재 CI vs omp2 CI

- **`oh-my-tau` (`.github/workflows/ci.yml`)**: `test`(`cargo test --workspace`),
  `build`(macOS / ubuntu-24.04-arm / windows continue-on-error), `musl`(x86_64 정적 빌드).
  `clippy`, `fmt --check`, `cargo-deny`, `tests-golden/`, `tests-tools/`, `tests-e2e/`,
  `scripts/compat-oracle/`가 **모두 CI에 미연동**이다.
- **`omp2` (`.github/workflows/ci.yml`, `justfile`)**: `justfile`을 단일 명령 출처로 삼아
  `format` → `runtime_spec`(`scripts/check-spec.rs`, 크레이트 경계 의존 린트) → `licenses`
  (`cargo deny check licenses sources`) → `rust`(`clippy`, `nextest`, `doctests`,
  P1~P6·P8) → `p7_linux`(실PTY TUI 증명)을 강제한다.

---

## 3. 업스트림 두 트랙 분석

### 3.1 `main` 트랙 — TS 코어 + 증분 Rust 네이티브 (`b636e19d10`)

`docs/porting-to-natives.md` 원칙에 따라 TS(`packages/coding-agent` 131만 줄 등)를 유지하면서
CPU·블로킹 I/O·할당 병목만 `crates/natives` N-API 뒤의 12개 Rust 크레이트로 내렸다.
- Rust 크레이트(86만 줄): `shell`(350k, bash 5.3 + zsh + ZLE), `builtins`(215k, 97 유틸리티),
  `natives`(142k), `edit`(35k), `xfs`(21k, 기존 `walker` 대체), `vcs`(16k, gitoxide + jj-lib),
  `ast`(9.2k, `ast::search` 재설계), `predict`(8.3k, ngram+smollm+apple), `vfs`(7.6k),
  `iso`(5.1k, 8종 격리 백엔드), `voice`(4k, Opus/WebRTC), `diff`(1.2k).
- 공식 Rust 클라이언트 `sdk/rust/omp-rpc`는 Protocol v2(1 MiB `rpc_chunk` 분할, 256건 페이징,
  `host_tool_call` / `xd://` 마운트 / `host_uri_request`, 프로세스 그룹 단위 종료)를 제공한다.

### 3.2 `omp2` 트랙 — 순수 Rust 재작성 (`2f92f3b`)

`PHILOSOPHY.md` 핵심 명제: *"build a coding harness so good that the model is the only replaceable part."*
- **TS 폐기 이유 4가지**: (1) 에이전트 관용구 수렴("Pick a language your agents can't ruin"),
  (2) 스로틀·GC·UTF-16 방어 코드 제거("ts is slow, rust isn't"), (3) TS 모양이 아닌 관측 가능한
  동작 이식, (4) 임베디드 free-threaded CPython 3.14t로 확장·Eval·`@remote` 통합(ADR 0036).
- **46개 크레이트 5계층 + `omp envd` 데몬 경계**: `app` → `driver::compose_kernel` → `agent` →
  `session`(`.oms` SSE 저널 + `omp-dom` 트리) → `ai`(Tower 스파인) / `catalog`(KDL 캐스케이드) /
  `envd`(파일·실행·문서 CAS·Python 워커 독점 소유).
- **강제 기계 규율 (`AGENTS.md`, `clippy.toml`, `tools/lintx`)**: `std::sync::Mutex`·`tokio::sync::mpsc`·
  `anyhow`·`unicode-*`·수동 enum↔string 매치·문자열 에러·`model_id.contains(...)` 분기 금지,
  `parking_lot`+`flume`+`thiserror`+`strum`+`xutf` 강제.
- **수용 증명 (`crates/e2e/tests/`, P1~P10)**: 문서 CAS 경합(P1), `CancelTree` 취소(P2), 단일 Job
  원형(P3), 라이브 스키마 격리(P4), 4밴드 프롬프트 캐시 안정성(P5), torn-tail 크래시 복구(P6),
  실PTY TUI(P7), 비게이팅 성능 기준선(P8), 격리 워크트리·확장 제어(P9), 도구 리프트 멱등성(P10).

### 3.3 `oh-my-tau`가 취할 위치

`omp2`의 **원칙·경계·규율**은 흡수하되 **스토리지·확장 형식**은 복제하지 않는다.
1. `~/.omp/agent` 설정·JSONL v3 세션 호환을 유지한다(ADR 0003/0004의 `.oms`/DOM 전환 보류).
2. `omp-tui`는 이미 66.4k줄 `ratatui` 즉시 모드로 완성도가 높으므로 `omp2` retained-mode TUI로
   전면 재작성하지 않고 필요한 모듈만 선별 차용한다.
3. `omp-cli` 중심의 결합을 `omp2` 식 경계(`env`/`envd` 정책 분리, `catalog` 데이터 분리, `clippy`/
   `lintx` 게이트)로 점진 리팩터링한다.

---

## 4. 전 영역 격차 분석

### 4.1 도구 패리티 — 18/29 (62%) + 비레지스트리 7종

| 구분 | 내용 |
| --- | --- |
| **일치 (11+3종)** | `find`, `write`, `wait`, `checkpoint`, `rewind`, `ast_grep`, `ast_edit`, `think`, `yield`, `goal`, `grep`(와이어 스키마) |
| **스키마 불일치 (4종)** | `ask`(구 `questions[]` vs 참조 `{html: string}`), `todo`(`task: string` + 독자 `reason` vs 참조 `string\|string[]`·`reason` 없음), `web_search`(3개 파라미터 누락), `bash`(`notify` 누락, 6종 설정 스키마를 정적 1종으로 축소) |
| **기능/범위 격차 (2종)** | `glob`(`limit` 상한 200·`pattern` 키 누락), `read`(아카이브·SQLite·PDF·JSON·바이너리·노트북·HTTP URL fetch 및 설명 누락) |
| **누락 (11종)** | `debug`, `github`, `context_notes`, `new_context`, `security_scan`, `memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill` |
| **레지스트리 외 누락 (7+종)** | `computer`·`browser`(eval prelude 포함 도구), `generate_image`, `tts`, `vibe_*` 5종, `report_tool_issue`(`xd://report_issue`), `search`→`grep` 별칭 |
| **Eval prelude** | `sh`, `console`, `browser`, `computer` 누락 (반면 `tau` 전용 `llm_query`, `llm_stats`, `rlm_context` 존재) |
| **설정 게이트** | 대상 11개 vs 참조 22개 (`edit`, `eval`, `think`, 메모리 4종, `task` 깊이 등 미게이트) |
| **끊긴 선언 3건** | `task.rs` `READ_ONLY_TOOLS` 유령 메모리 도구, `builtin_commands.json`의 구현 없는 `security` 11서브명령, `settings_defs.rs:2832`의 구현 없는 `github` 설정 |

또한 `omp2` `omp-tool`의 핵심 계약인 **의도 파라미터 `i` 중앙 주입**, **`Effects` deny-all 봉투**,
**개정 정체성(`Rev`)**, **단일 스트림 커밋 프레임**이 `oh-my-tau`에는 없다. 1단계로 `i` 주입과
효과 봉투부터 선별 도입한다.

### 4.2 셸·빌트인 — 패리티 0

`oh-my-tau`는 `crates/omp-cli/src/tools/bash.rs`에서 호스트 `bash -c`(`setsid --wait`)를 스폰하며
파일 주석(`bash.rs:3`)은 `brush` 기반이라 잘못 기술하고 있다. 업스트림에는 두 가지 참조가 있다:
- `main` `crates/shell`(350k) + `crates/builtins`(215k): bash 5.3 + zsh + 193 ZLE 위젯 + 97 유틸 + minimizer.
- `omp2` `crates/shell`(48.5k) + `crates/shell-builtins`(90.4k): `brush-core` 0.5.0 / `brush-parser` 0.4.0 기반 **임베더블 라이브러리** + 57 bash 빌트인 + 59 유틸 + 8 프로세스 빌트인 + `dyn` 도구 브리지 + `PathPolicy`/`ProcessGroupPolicy`/`analysis.rs` 정적 AST 검사.

**결론**: `oh-my-tau`에는 `omp2`의 `crates/shell` + `crates/shell-builtins` 쌍이 훨씬 적합하다.

### 4.2b 벤더 크레이트 (`vendor/pi-*`, `vendor/pa-*`) 드리프트

`vendor/pi-*` 6종은 `18.9.1`(predict `18.6.1`) 스냅샷으로, 최신 `main`(`19.0.1+126`, `b636e19d10`)
기준 단순 버전 범프가 아니라 **공개 API 재작성이 필요한 격차**가 있다(생 코드 diff의 2/3는
4-space vs hard-tab 들여쓰기 차이이므로 시맨틱 diff 기준으로 판단해야 함).

| 벤더 크레이트 | 현행 → `main` | 시맨틱 diff | 파괴/변경 내용 및 `oh-my-tau` 영향 |
| --- | --- | ---: | --- |
| `pi-walker` | `18.9.1` → **삭제** | - | 업스트림에서 삭제되고 `crates/xfs`(21,015줄)의 `walk` 모듈로 통합(`Tree`/`Walk`/`Batch` 신 API). `omp-cli/src/tools/{find,search}.rs` 재작성 필수 |
| `pi-ast` | `18.9.1` → `19.0.1` | 3,533줄 | `ops::AstMatch`/`collect_matches`/`compile_rewrite_rules` 전부 삭제, `ast::search`(`Rewrite`, `CancellationToken`, `SourceEdit`)로 전면 교체 + `outline`/`debug`/wasm 문법 추가. `omp-cli/src/tools/ast.rs` 재작성 필수 |
| `pi-predict` | `18.6.1` → `19.0.1` | 429줄(ngram) | 업스트림에 `smollm/`(4,257줄, candle/metal)·`apple/`(381줄) 추가. 현재 `omp-tui/src/word_completion/host.rs:34`가 모두 `Ngram`으로 조용히 폴백 |
| `pi-edit` | `18.9.1` → `19.0.1` | 10,301줄 | 호출 시그니처는 대부분 호환되나 `Rejection` 에러 분류, `max_columns`, 5개 프롬프트 전면 개편, 8,054줄 테스트 추가 |
| `pi-vfs` | `18.9.1` → `19.0.1` | 1,471줄 | `Scope` 기반 scoped open, 100ms `POLL_SLICE`, `read_until_cancelled`, `Fs::write/symlink` 추가 |
| `pi-diff` | `18.9.1` → `19.0.1` | 643줄 | `ComponentArena` + `xutf` 내부 메모리 최적화. 공개 API 호환으로 거의 무수정 이식 가능 |
| `pa-*` (4종) | `prime-agent` 포크 | 78,381줄 | `compaction_epoch`, `get_system_prompt`, `prepare_dispatch`, bedrock h2c 등 `omp`용 확장 다수. `NOTICE.md`에 수정 파일 목록 누락 |

### 4.3 메모리·컨텍스트 파일 — 부재 및 부분 구현

- **메모리**: `crates/memory` 없음 (`omp2` 6,428줄 SQLite+4-voice RRF / TS `mnemopi` 20,970줄).
  `omp-settings/src/schema.rs`에 74개 메모리 키가 dead config로만 존재하고, `/memory`는
  `slash.rs:757-776` 리터럴 스텁이며, `memory://` URL 스킴 미등록, `rule://`은 빈 벡터 반환.
### 4.4 컨텍스트 파일 — 부분 구현

`sysprompt.rs:153-192`는 `AGENTS.md`/`CLAUDE.md` 상향 탐색만 구현했다. `.omp/AGENTS.md`
(우선순위 100), 사용자 스코프 `~/.omp/agent/AGENTS.md`, `RULES.md` 끈끈한 규칙(`rules: []`
하드코딩), `GEMINI.md`, `.github/copilot-instructions.md` 등이 누락됐다.

### 4.3c `oh-my-tau` 고유 자산 (이식 중 절대 삭제 금지)

업스트림 `main`과 `omp2` 어느 쪽에도 없는 포크 고유 자산은 보존한다:
- **`/ultra code|deep|long|hyper|off`** & **`ultracode` 매직 키워드**: RLM(`llm_query`)·CLM(`clm-host`) 연동. (참고: 업스트림 `chillax` 키워드는 현재 누락되어 있어 복원 여부 확인 필요)
- **`omp-judge`(1,710줄) + `vendor/laya-ort`(539줄)**: Laya ONNX 턴 판정기(`OMP_LAYA_JUDGE=1` 섀도 워커, `laya-judge` CLI, `judge-sessions`). musl 빌드에서는 `onnx` 피처를 끄고 깔끔히 오류 처리.
- **`omp-goose`(147줄 Rust + 4,398줄 생성 C)**: SipHash-1-3 dedup 커널, `omp-cli`의 비기본 `goose` 피처(`unsafe_code = "allow"` 격리).
- **`omp-sot`(2,347줄)**: State-of-Thought 컨트롤러(단, 현재 `omp-cli` 런타임 미연결, 예제·단위 테스트만 존재).
- **`omp-sandbox`(377줄)**: Landlock ABI V9 + seccomp + `exec_confined` — 업스트림 `omp-iso`(파일시스템 CoW)와 목적이 다르므로 대체하지 않고 유지.
- **`omp-wasm`(741줄)**: `wasmi` 기반 무의존 샌드박스 플러그인 호스트.
- **`omt web` UI(`crates/omp-cli/src/subcmd/web/`, 포트 7467)** + wave 1~3 슬래시 커맨드(`/plan`, `/wt`, `/btw`, `/loop`, `/shake`, `/restart`, `/skills`, `/marketplace`, `/share`, `/ssh`, `/browser`, `/computer`, `/budget`, `/sandbox`, `omt daemon`) + `omp-collab`, `omp-auth-broker`.
- **`vendor/crossterm` 패치**: xterm `modifyOtherKeys` (`CSI 27;mods;code~`) 1건.

### 4.5 세션(`omp-session`)·압축(`omp-compact`)·토크나이징·Handoff

- `omp-session`(3,729줄)은 JSONL v3 바이트 호환성이 매우 높다: 16종 `OmpEntry` 정확한 키 순서, JS `Number#toString` 포맷터, 256B 고정 title 슬롯(`write_all_at(0)`), 1MB mmap + 4MB rayon 병렬 스캔, 바이트 스팬 기반 `append_seeded_line` 무파싱 재작성까지 갖췄다. 따라서 JSONL v3 계약을 유지하는 한 세션 저장소 자체는 재작성보다 보호 대상이다(단, `label` 쓰기·`session_info`/`child_usage_attributed` 모델링·세션 list/delete 부재 및 `tree_parity.rs`의 없는 `reports/SessionTree.md` 참조는 보완 필요).
- `omp-compact`(6,890줄)는 절단점·요약·방언 10종·`handoff.rs` 라이브러리를 갖췄으나(`methodOrder`에 `soft`/`context-full`이 없으면 자동 컴팩션이 조용히 `off` 되는 CLI 게이팅 포함):
  1. `/handoff` 슬래시(`slash.rs:2999`)와 RPC `"handoff"`(`rpc.rs:1022`)가 **미연결**,
  2. 토크나이저가 `ByteEstimate`(bytes/4)뿐이고 이미지 3-bucket 추측 + `FRAME_TOKEN_ESTIMATE = 5024` 하드코딩(TS는 `utok` + 카탈로그 이미지 패치 과금 + usage 앵커 테일 카운팅),
  3. `snapcompact` 비트맵 렌더러·정규화·셀 래핑·포비에이티드 레이아웃·`blob:sha256:` 프레임 해석이 없어 실제로는 항상 `soft` 컴팩션만 실행됨.
- **`omp2` `crates/snapcompact`(2,042줄)**가 순수 Rust PNG 렌더러(`fontdue`/내장 폰트)를 이미 제공하므로 이를 벤더링하면 렌더러 구현 비용을 크게 줄일 수 있다(단, 정규화와 래핑은 호출자 책임).

### 4.6 VCS·워크트리·격리

- `oh-my-tau`는 `features/worktree.rs`의 `git()` 헬퍼로 모든 조회를 subprocess fork+exec 및 stderr 문자열 매칭에 의존(`gix`/`git2` 없음). `omp-cli/src/sysprompt.rs`도 별도 `.git` 탐색을 중복 구현.
- 업스트림 `main` `crates/vcs`(16k) + `crates/iso`(5.1k)는 gitoxide 기반 ~60개 연산, `worktree_add` + CoW `clone_holes` + `keep_changes`, `jj-lib`를 제공. (`omp2`의 `vcs`보다 `main`의 `crates/vcs`가 `keep_changes`·`git/filter.rs`까지 있어 더 최신이다.) 여기에 `omp2` `crates/app/src/worktree_cmd.rs`의 `.omp-isolation-owner` PID 기반 고아 워크트리 GC를 결합한다.

### 4.7 확장 모델 — 조용한 열화

- **Tier A (`omp-ext` + `ext-host/` Bun)**: 무수정 pi 확장을 실행하지만 `registerShortcut`, `registerMessageRenderer`, `registerAssistantThinkingRenderer`, `registerComposerShape`, `registerFileWriteFallback`, `registerFileDeleteFallback`, `getServiceTiers`/`setServiceTier`, `ui.*` ~10종이 **silent noop**이고 `runEphemeralTurn`은 throw한다. `OMP_RS_EXT_SHARED=1`(`shard.ts`+`session.worker.ts`) 공유 데몬 모드가 있으나 여전히 Bun 런타임에 의존하며 샌드박스가 없다.
- **Tier B (`omp-wasm`)**: 완전 샌드박스이나 매니페스트가 `tools`/`commands`만 허용하고 훅·컨텍스트 변환·정책이 없다.
- **`omp2` (CPython 3.14t)**: `StaticDeclarations` 사전 봉인 + 2소켓 + `ExtHostSupervisor` kill 경계 + 5-arm 훅 결정 어휘로 모든 보안·격리 결함을 해결했으나 TS 확장과 호환 불가.

### 4.8 검증 인프라 격차

저장소에 `tests-golden/`(12시나리오, `compare-baseline.txt` 19개 차이 기록), `tests-tools/`
(`ts_tools.json` 21/32종만 포함 및 최신 TS와 어긋남), `tests-e2e/`(auth, ext `conformance.py`,
TUI 11개 프로브 `run.py`), `scripts/compat-oracle/`가 있으나 **전부 CI에서 실행되지 않는다**.

### 4.9 `omp-tui` 정밀 격차 (66,466줄, 전체 크레이트의 27.8%)

103 `.rs` 파일, 761 단위 테스트, 인라인 스크롤백 렌더러(DEC 2026·kitty 키보드·멀티플렉서 감지),
102 내장 테마 JSON, 70 키바인딩, kitty 유니코드 이미지 플레이스홀더, 에디터/vim 모드, 상태줄,
완전한 오버레이 12종까지 구현되었다. 그러나 `main`의 `packages/tui`(213k src + 102k test) 대비:

| 영역 | 업스트림 `main` | `oh-my-tau` (`omp-tui`) | 격차 및 권고 |
| --- | --- | --- | --- |
| **기능 행렬** | `terminal-capabilities.ts` (1,602줄) | `render.rs`·`images.rs`·`notify.rs`에 분산 | **최우선 통합**: DECRQM 런타임 프로브 포함 단일 `terminal_capabilities.rs` |
| **오버레이** | 76개 | 12개 | ~64개 누락. 빈도 높은 `model-hub`·`agents-hub`·`usage-dashboard`·`session-selector` 우선 이식 |
| **키바인딩** | 79개 (`32 tui + 47 app`) | 70개 | 11개 누락 (`app.session.pin/unpin`, `app.tree.next/previousBranch` 등) |
| **차트·HTML 폼·앱** | 차트 11.2k + 폼 3.2k + 앱 14개 | 0줄 | 사용자 회귀 적은 후순위 Phase로 분리 (`ask` HTML 폼 수요에 따라 선별) |
| **네이티브 테마/모션** | shimmer/gradient/831 심볼 | 기본 토큰 9종 / 809 심볼 / 4종 매직키워드 | `omp2` `theme.rs`(oklch/lab/lch)와 `mermaid`/`graphviz` 렌더러만 선별 차용 |
| **검증** | 386 테스트 파일 | `tests-tui/` 픽스처만 존재, E2E는 `tests-e2e/tui/run.py` | `tests-e2e/tui/run.py`를 CI에 즉시 연결 |

**아키텍처 결론**: `omp2`의 retained-mode `dom!` TUI(96.9k줄)는 `omp-tui`의 `ratatui` 즉시 모드와
비호환이므로 전면 교체하지 않고 색상 공간·컴포넌트 적합성 테스트 패턴·mermaid 렌더러만 가져온다.

### 4.10 `omp-providers` · `compat-oracle` · 인증

- 4계층 모델 조회(`models.yml` → `models.db` → `models_generated.rs` → 합성), `MATERIALIZATION_POLICY`
  (`wyhash` base36 TS 바이트 호환), `agent.db` 스키마 v8 15초 DB 리스+CAS OAuth 리프레시, wave 3에서
  80개 `/login` 흐름 완료.
- **실측 부채 3건**: (1) `vendor/pa-ai/src/models_generated.rs`(186 KB)에 생성기가 없어 수동 관리 중,
  (2) `scripts/sync-catalog.sh`가 존재하지 않는 `runs/omp-rust/compat-oracle/gen-data.ts`를 가리켜
  실패함(실제 경로 `scripts/compat-oracle/gen-data.ts`), (3) `verify-models-db.ts`와 `compat::oracle`이
  CI에 미연동이며 픽스처 부재 시 자동 skip됨.

### 4.11 잔여 크레이트 (`omp-lsp`, `omp-mcp`, `/browser`·`/computer`, `omp-rpc`)

- **`omp-lsp`**: `mux/server.rs`(1,087줄)에 세션 공유 데몬이 이미 구현되어 있음(이전 계획 오해 해소).
- **`omp-mcp`**: stdio·Streamable-HTTP는 완비되었으나 OAuth·크로스세션 도구 캐싱·prompts/resources·데몬 기반 공유(`main` `acdfffa248`)가 미구현.
- **`/browser`·`/computer`·`omp-eval`**: `/browser`는 로컬 Chrome CDP 실제 제어(단, 모델 도구 및 `browser-relay` MV3 확장 부재), `/computer`는 `grim`/`import`/`gnome-screenshot` 스크린샷 래퍼뿐(업스트림 `desktop` AX/입력 자동화 15.7k줄 부재), `omp-eval`은 외부 `python3` stdlib 포트.
- **`omp-settings`·`omp-urls`·`omp-cli` 세부 실측**: `omp-settings`는 548개 키를 선언했으나 `Settings::provenance()`(`settings.rs:210-229`)가 `Global`을 절대 반환하지 않는 결함이 있다. `omp-urls` 11개 스킴은 전부 읽기 전용(`fn write` 미구현)이다. `omp-cli`는 55개 RPC 메서드·12개 서브커맨드·4개 모드(TUI/print/json/rpc; `acp`·`rpc-ui`는 시작 시 명시 거부)를 제공한다.
- **RPC v2**: `main` `sdk/rust/omp-rpc`의 1 MiB `rpc_chunk` 분할·`get_messages_page`·`xd://` 호스트 도구 마운트를 `crates/omp-cli/src/rpc.rs`와 비교해 누락 프레임을 보완해야 한다.

---

## 5. 마이그레이션 원칙 (mhc 선례 대조 포함)

### 5.1 mhc 클린룸 절차 대비 적용/제외 표

| 항목 | mhc (omo → Rust) | oh-my-tau (oh-my-pi → Rust) | 결정 |
| --- | --- | --- | --- |
| 원본 라이선스 | SUL-1.0 (비오픈소스) | MIT (`oh-my-pi`, `prime-agent`, `crossterm`) | 법적 오염 위험 없음 |
| **클린룸 세션 분리 / `CLEANROOM.md` / SUL feature 격리** | 필수(법적 방화벽) | **불필요 — 제외** (직접 읽고 이식하는 것이 MIT 허용 경로) | **미적용** |
| **업스트림 커밋 SHA 핀 (`vendor-src.lock` / `PINS.md`)** | 적용 (`v5.1.52`) | 현재 버전만 적고 SHA 미기록 | **채택·신설** |
| **기계 판독 가능 패리티 원장 + 골든/오라클 테스트** | 적용 | `tests-golden/`·`compat-oracle` 이미 존재(CI 미연동) | **채택·강화** |
| **변경 델타 노트 (`vendor/pa-*/NOTICE`, `pi-*` 델타)** | 동작 델타 스펙 | `NOTICE.md`에 "modified"만 적고 파일 목록 누락 | **채택 (MIT 준수)** |
| **`deny.toml` / `about.toml` 라이선스 CI 게이트** | `THIRD_PARTY_LICENSES` | 부재 (`laya-ort` 런타임 라이선스 미명시 포함) | **채택 (`omp2` `deny.toml` 이식)** |

### 5.2 핵심 설계 원칙 7개

1. **호환 계약을 먼저 못 박는다**: `~/.omp/agent` 설정·JSONL v3 세션·기본 키/툴 호환은 별도 승인 없이 깨지 않는다.
2. **게이트 먼저, 기능 나중**: Phase 0 CI·패리티 게이트 없이 기능을 이식하지 않는다.
3. **패리티 원장을 단일 진실로**: 도구·슬래시·셸 빌트인·TUI 오버레이·설정의 상태를 원장으로 관리한다.
4. **`omp2`는 형식이 아니라 원칙·경계만 차용한다**: ADR 0009/0011/0017/0018/0024/0028 흡수, `.oms`·CPython 전면 전환은 보류.
5. **조용한 열화를 금지한다**: `noop`·빈 벡터·dead config를 명시적 오류나 경고로 전환한다.
6. **앞서 있는 고유 자산을 보호한다**: `omp-sandbox`, `omp-lsp` mux, `omp-judge`/`laya-ort`, `omp-goose`, `omp-sot`, `omp-wasm`, `/ultra`, `omt web`, `crossterm` 패치 보존.
7. **핀과 델타로 동기화한다**: 모든 `vendor/*`와 테스트 픽스처에 기준 업스트림 SHA를 기록한다.

---

## 6. 단계별 마이그레이션 계획 (Phase 0 ~ Phase 8)

### Phase 0 — 검증 기반 및 핀 고정 (선행 필수)

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 0.1 | `cargo clippy --workspace -- -D warnings` + `cargo fmt --check` CI 추가 | `lint`·`format` 잡 통과(예외는 `#[allow(..., reason = "...")]`) |
| 0.2 | `omp2`의 `deny.toml` 이식 + `cargo deny check licenses sources` CI 잡 추가 | 라이선스 게이트 통과, `laya-ort` ONNX 런타임 라이선스 `NOTICE.md` 명시 |
| 0.3 | `vendor-src.lock`(또는 `docs/vendor-pins.md`) + `vendor/pa-*/NOTICE` 신설 | `pi-*`/`pa-*`/골든 픽스처의 기준 업스트림 SHA 및 `pa-*` 수정 파일 명시 |
| 0.4 | `scripts/sync-catalog.sh` 경로 버그 수정(`runs/omp-rust/compat-oracle` → `scripts/compat-oracle`) | `vendor-src` 있을 때 재현 가능하게 동작 |
| 0.5 | `tests-golden/compare.ts` + `tests-tools/compare.ts` + `tests-e2e/tui/run.py` CI 연동 | 19개 기존 차이 원장화, TUI 실PTY 프로브 CI 실행 |
| 0.6 | `justfile` 도입 (`check`, `test`, `lint`, `golden`, `e2e`, `ci`) | 단일 명령 진입점 확립 |

### Phase 1 — 패리티 원장 및 조용한 열화 제거

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 1.1 | `docs/parity/{tools,slash,settings,tui,shell}.md` 원장 구축 | 전 항목 상태·근거 파일 매핑 |
| 1.2 | 스키마 불일치 4종 수정(`ask` HTML 폼 + 레거시 복구, `todo` `string\|string[]`, `web_search` 3파라미터, `bash.notify`/설정별 스키마) + `glob` 상한 | `tests-tools/compare.ts` diff 0 |
| 1.3 | `tests-tools/ts_tools.json`을 최신 `main`(`b636e19d10`) 기준으로 32종 전체 재추출 | 누락 11종 골든 스키마 확보 |
| 1.4 | 끊긴 선언 3건 정리 (`task.rs` 유령 메모리 도구, `builtin_commands.json` `security`, `settings_defs.rs` `github`) | 구현 없는 광고 0건 |
| 1.5 | `ext-host/host.ts`의 `noop` ~12개 메서드를 `UnsupportedExtensionApi` 명시 오류로 전환 | 거짓 성공 0건 |
| 1.6 | `rule://` 빈 구현 명시 오류화, `schema.rs` 미소비 74개 메모리 키 마킹, `bash.rs:3` 주석 수정 | 조용한 폴백 0건 |

### Phase 2 — 셸·빌트인 이식 (`omp2` `shell` + `shell-builtins`)

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 2.1 | `omp2`의 `crates/shell`(48.5k) + `crates/shell-builtins`(90.4k)를 `vendor/`에 벤더링 | 워크스페이스 빌드·musl 정적 빌드·바이너리 크기 측정 통과 |
| 2.2 | `omp-cli/src/tools/bash.rs`를 in-process `ShellBuilder` + `DynHost`로 배선 | `bash_command` 골든 통과, 파이프·리다이렉트·heredoc 동일 출력 |
| 2.3 | `rm`/`mv`/`ln` 레지스트리 분리 + `PathPolicy`/`ProcessGroupPolicy`/`SpawnWrapper`를 `omp-sandbox`와 결합 | 샌드박스·정책 경계 회귀 통과 |
| 2.4 | `analysis.rs` 정적 AST 검사 + 취소 경계(ADR 0011, `-pgid` 시그널) + `dyn` 빌트인 브리지 배선 | 취소·동적 도구 로스터 불변 검증 |

### Phase 2.5 — `vendor/pi-*` 동기화 및 `omp-xfs` 전환

의존 순서대로 진행해 컴파일 깨짐을 국소화한다:
1. **`pi-vfs` (18.9.1 → 19.0.1)**: `Scope` 기반 open + 취소(`read_until_cancelled`) + `Fs::write/symlink` 이식.
2. **`omp-xfs` 도입 & `pi-walker` 은퇴**: `main`의 `crates/xfs/src/walk` 이식 후 `omp-cli/src/tools/{find,search}.rs`를 `Tree`/`Walk`/`Batch` API로 재작성, `vendor/pi-walker` 제거.
3. **`pi-ast` (18.9.1 → 19.0.1)**: `ast::search`/`outline`/`debug` 이식 후 `omp-cli/src/tools/ast.rs` 재작성.
4. **`pi-diff` & `pi-edit` (18.9.1 → 19.0.1)**: `ComponentArena` + `xutf`, `Rejection` taxonomy + `max_columns` + 5개 프롬프트 반영.
5. **`pi-predict` (18.6.1 → 19.0.1)**: 우선 `ngram` 리팩터링 반영, `smollm`/`apple`은 feature 게이트(`predict-local`)로 선택 이식.

### Phase 3 — 도구·메모리·컨텍스트·Handoff 완성

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 3.1 | `/handoff` 슬래시(`slash.rs:2999`) 및 RPC `"handoff"`(`rpc.rs:1022`)를 기존 `omp-compact/src/handoff.rs`에 배선 | `handoff` E2E 통과 |
| 3.2 | `read` 도구 디스패치 확장(아카이브·SQLite·PDF·JSON·바이너리·노트북·HTTP URL fetch) 및 `READ_DESCRIPTION` 복구 | 유형별 골든 테스트 통과 |
| 3.3 | `memory://` 스킴(100레코드/1MiB) + SQLite/FTS 메모리 크레이트(`crates/omp-memory`) + `recall`/`retain`/`reflect`/`memory_edit`/`learn` 구현 | `/memory stats` 실데이터 반환, 데드 설정 활성화 |
| 3.4 | 컨텍스트 파일 탐색 확장(`.omp/AGENTS.md` 우선순위 100, 사용자 스코프, `RULES.md` 끈끈한 규칙, `GEMINI.md` 등) | `sysprompt` 단위·골든 테스트 통과 |
| 3.5 | `context_notes`/`new_context`·`search→grep` 별칭·설정 게이트 22종·`browser`/`computer` 모델 도구화 검토 | 원장 정합 |

### Phase 4 — 프로바이더·추론·MCP/RPC 정렬

- `models_generated.rs` 수동 관리 제거(카탈로그 생성 스크립트 자동화), `model_id.contains(...)` 분기
  추방(KDL/카탈로그 데이터화), `ByteEstimate` → `utok` 정밀 토크나이저 + 카탈로그 기반 이미지 패치 과금 +
  프레임 청구(5024 하드코딩 제거) 교체.
- `omp-mcp`에 OAuth·세션 간 도구 캐싱·데몬 공유(`main` `acdfffa248`), `omp-cli/src/rpc.rs`에
  `omp-rpc` v2 프레임 청킹·`xd://` 호스트 도구·`host_uri` 정렬.

### Phase 5 — VCS·워크트리·격리 PAL

- `gix`(gitoxide) 도입으로 `features/worktree.rs` 및 `sysprompt.rs`의 subprocess `git` 호출 제거, 구조화 에러화.
- `main`의 `crates/vcs` (`worktree_add` + `keep_changes` + `git/filter.rs`) + `crates/iso`(Linux 우선:
  `linux_reflink`/`overlayfs`/`btrfs`/`rcopy`) + `omp2` `worktree_cmd.rs`(`.omp-isolation-owner` PID 고아 GC) 결합.
- `omp-sandbox`(Landlock V9 + seccomp) 회귀 테스트 고정.

### Phase 6 — 확장 런타임 결정 및 고도화

- **단기(트랙 A)**: `ext-host` 45개 이벤트 커버리지 원장 작성, `noop` 제거 검증, `runEphemeralTurn` 구현 또는 명시적 오류화, `omp-wasm` 훅 ABI 확장 여부 검토.
- **장기(트랙 B — 주인 결정 필요)**: `omp2` 식 임베디드 CPython 3.14t 전환은 기존 TS 확장 생태계를 폐기하므로 별도 ADR 및 주인 승인 시에만 착수.

### Phase 7 — 압축(`snapcompact`)·세션 분기 심화

- `omp2` `crates/snapcompact`(2,042줄, 폰트 포함) 벤더링 + TS `normalize`/`wrap`/`planArchive` 이식 +
  `Archive` 쓰기 및 `blob:sha256:` 프레임 해석 구현. JSONL v3 유지 하에 분기 메타데이터 1급화 검토.

### Phase 8 — `omp-tui` 단계적 패리티 & 최종 정리

- **1단계**: `terminal_capabilities.rs` 단일 행렬 통합(DECRQM 프로브 포함) + 누락 키바인딩 11종 추가.
- **2단계**: 고빈도 오버레이 4종 우선 이식(`model-hub`, `agents-hub`, `usage-dashboard`, `session-selector`).
- **3단계**: `omp2` `theme.rs`(oklch/lab/lch)·`mermaid`/`graphviz` 렌더러·컴포넌트 적합성 테스트 패턴 선별 차용.
- **4단계**: 차트·HTML 폼·서브앱(14종)은 수요 확인 후 별도 트랙으로 처리, 전 원장·벤더 핀·릴리스 노트 정합.

---

## 6b. 초세분화 스웜 실행 작업 분해 (Micro-Task WBS)

실제 구현·테스트·QA·리뷰 시 한 번에 거대한 PR을 만들지 않고, **1 워커 = 1 파일/도구/테스트/QA 시나리오/문서**
단위로 잘게 쪼개 `oc-deepseek-v4.1-flash-free` 병렬 스웜에 동시 투입할 수 있도록 각 Phase를 마이크로 조각으로
분해한다. 설계 변경·파괴적 API 판단·통합 아키텍처 리뷰만 상위 모델(예외 라우팅)에 배정한다.

| Phase | 마이크로 조각 ID 및 단위 (각 1 워커 병렬 실행 가능) | 모델 라우팅 |
| --- | --- | --- |
| **P0** | `P0-1` `deny.toml` 이식, `P0-2` `vendor-src.lock` 작성, `P0-3` `vendor/pa-*/NOTICE` 4파일 각각 작성, `P0-4` `sync-catalog.sh` 수정, `P0-5` `ci.yml` lint/fmt 잡, `P0-6` `ci.yml` golden/tools 잡, `P0-7` `ci.yml` TUI PTY 잡, `P0-8` `justfile` 작성 | `oc-deepseek-v4.1-flash-free` (전부) |
| **P1** | `P1-1~5` `docs/parity/*.md` 5개 각각 작성, `P1-6` `ts_tools.json` 32종 재추출, `P1-7` `web_search` 스키마+테스트, `P1-8` `todo` 스키마+테스트, `P1-9` `bash.notify` 스키마+테스트, `P1-10` `glob` 스키마+테스트, `P1-11` `ask` HTML 폼+복구(설계 확인), `P1-12` 끊긴 선언 3건 각각 제거, `P1-13` `ext-host` noop 메서드별 throw 전환+테스트, `P1-14` `rule://`·`schema.rs`·`bash.rs:3` 수정 | `oc-deepseek-v4.1-flash-free` (`P1-11` 리뷰만 상위 모델) |
| **P2** | `P2-1` `vendor/omp-shell*` 편입 및 빌드 확인, `P2-2` `DynHost` 어댑터 배선, `P2-3` `PathPolicy`/`omp-sandbox` 연결, `P2-4` `-pgid` 취소 테스트, `P2-5` `analysis.rs` 정책 테스트, `P2-6` 파이프/heredoc 골든 QA 5종, `P2-7` `dyn` 브리지 통합 | `P2-1~2` 상위 모델 설계 검수, `P2-3~7` 스웜 병렬 |
| **P2.5** | `P2.5-1` `pi-vfs` 동기화+테스트, `P2.5-2` `omp-xfs/walk` 벤더링, `P2.5-3` `find.rs` 재작성+테스트, `P2.5-4` `search.rs` 재작성+테스트, `P2.5-5` `pi-ast` 동기화, `P2.5-6` `tools/ast.rs` 재작성+테스트, `P2.5-7` `pi-diff` 동기화, `P2.5-8` `pi-edit` 동기화+5프롬프트, `P2.5-9` `pi-predict` 반영 | `P2.5-3/4/6` API 재작성은 상위 모델 리뷰, 나머지 스웜 |
| **P3** | `P3-1` `/handoff` 슬래시+RPC 배선+E2E, `P3-2~8` `read` 핸들러 7종(archive/sqlite/pdf/json/binary/notebook/http) 각 1워커, `P3-9` `memory://` 스킴+테스트, `P3-10` `omp-memory` SQLite 스키마+FTS, `P3-11~15` `recall/retain/reflect/memory_edit/learn` 각 1워커, `P3-16` `.omp/AGENTS.md`·`RULES.md` 탐색+테스트, `P3-17` 설정 게이트 22종 테스트 | `P3-10` 설계만 상위 모델, 나머지 전부 스웜 병렬 |
| **P4** | `P4-1` `models_generated.rs` 생성기 스크립트, `P4-2` `model_id.contains` grep 제거, `P4-3` `utok` 토크나이저+3모드 테스트, `P4-4` 이미지 패치 과금 테이블, `P4-5` `omp-mcp` OAuth/공유 데몬, `P4-6` `rpc.rs` v2 청킹+테스트 | `P4-5/6` 설계 검수 상위 모델, 나머지 스웜 |
| **P5** | `P5-1` `gix` 읽기 경로(status/branch/log/diff) 각 1워커, `P5-2` `sysprompt.rs` 중복 제거, `P5-3` `omp-iso` 백엔드별(reflink/overlayfs/btrfs/rcopy) 각 1워커, `P5-4` `keep_changes` 이월+테스트, `P5-5` `.omp-isolation-owner` 고아 GC+테스트, `P5-6` `omp-sandbox` 회귀 테스트 | 전부 스웜 (동시 10+ 병렬) |
| **P6** | `P6-1` `ext-host` 45개 이벤트 커버리지 원장 작성, `P6-2` `runEphemeralTurn` 구현 또는 명시적 오류화+테스트, `P6-3` `omp-wasm` 훅 ABI 확장 설계·타당성 검토, `P6-4` 트랙 A(Bun 유지) vs 트랙 B(CPython 3.14t) 결정 ADR 작성 | `P6-3/4` 설계 상위 모델, `P6-1/2` 스웜 |
| **P7** | `P7-1` `omp-snapcompact` 벤더링, `P7-2` `normalize`+유닛테스트, `P7-3` `wrap`/`planArchive`+골든테스트, `P7-4` `Archive` 쓰기+`blob:sha256:` 해석 | 전부 스웜 병렬 |
| **P8** | `P8-1` `terminal_capabilities.rs` 통합+DECRQM, `P8-2` 누락 키바인딩 11종, `P8-3~6` 오버레이 4종(`model-hub`/`agents-hub`/`usage`/`sessions`) 각 1워커, `P8-7` PTY QA 프로브 4종 추가, `P8-8` `oklch`/mermaid 차용 | `P8-1` 상위 모델 검수, 나머지 스웜 |

---

## 7. 우선순위와 근거

| 순위 | 단계 | 근거 |
| --- | --- | --- |
| 1 | Phase 0 | 게이트 없이는 이후 모든 "완료"가 주장일 뿐이며 기존 19개 diff도 방치됨 |
| 2 | Phase 1 | `ext-host` noop, 유령 선언, `ts_tools.json` 불일치 등 조용한 열화 즉시 제거 |
| 3 | Phase 2 | 체감 격차 최대(셸·빌트인 패리티 0), `omp2` 크레이트 벤더링으로 비용 절감 |
| 4 | Phase 2.5 | `pi-ast`·`pi-walker` API 파괴를 조기 봉합해야 이후 도구/검색 수정이 헛수고 안 됨 |
| 5 | Phase 3 | `/handoff` 즉시 활성화, `read` 다형성, 메모리·`RULES.md`는 매 세션 체감 |
| 6 | Phase 5 | `git` fork+exec 제거와 `keep_changes`·고아 GC로 에이전트 워크트리 신뢰성 확보 |
| 7 | Phase 4 | 카탈로그/토크나이저/`omp-mcp`/`rpc` v2 부채 상환 |
| 8 | Phase 7 & 8 | `snapcompact` 압축 심화 및 `omp-tui` 64개 오버레이·기능 행렬 단계적 마감 |
| 9 | Phase 6 | 확장 런타임 전면 전환(트랙 B)은 제품 계약을 바꾸므로 사용자 결정 후 착수 |

---

## 8. 리스크 및 완화

| 리스크 | 영향 | 완화 |
| --- | --- | --- |
| `omp2` `shell` 벤더링이 빌드 시간·바이너리 크기·musl 호환에 영향 | 릴리스 파이프라인 | Phase 2.1에서 바이너리 크기·musl 빌드 먼저 실측, feature gate 분리 |
| `pi-ast`/`omp-xfs` 전환 중 `tools/ast.rs`·`find.rs`·`search.rs` 회귀 | 핵심 파일 탐색/편집 깨짐 | Phase 2.5 전 단계별 골든 출력·스모크 테스트 선행 |
| `ext-host` noop → 명시적 오류 전환 시 기존 사용자 확장 중단 | 사용자 확장 오류 | 에러 메시지에 누락 API와 대안 명시, 릴리스 노트 사전 고지 |
| `omp-tui` 즉시모드 유지 vs `omp2` retained-mode 차이 지속 | 장기 UI 유지보수 분기 | `terminal_capabilities.rs`와 `tests-e2e/tui/run.py` PTY 게이트로 동작 계약 고정 |
| `models_generated.rs`·`vendor-src` 미고정으로 인한 카탈로그 드리프트 | 프로바이더/모델 오작동 | `vendor-src.lock` + `sync-catalog.sh` 경로 수정 + `verify-models-db.ts` CI화 |
| musl 빌드에서 `laya-ort`(ONNX)·`omp-goose` 링크 충돌 | 정적 바이너리 실패 | 기존 `onnx` opt-out 및 `goose` feature 격리 유지, musl 스모크 테스트 게이트 |

---

## 9. 검증 방법

각 Phase·마이크로 작업은 다음 셋을 모두 충족해야 완료다.
1. **기계 검증**: `just ci` (`fmt-check`, `clippy -D warnings`, `cargo deny`, `tests-golden`, `tests-tools`, `tests-e2e/tui/run.py`) 통과.
2. **행동 검증**: TUI 변경은 실PTY(`tests-e2e/tui/run.py`), 도구는 골든 스키마+실행, 셸은 파이프·heredoc·프로세스 그룹 취소 시나리오 실행.
3. **원장·증거 기록**: 해당 `docs/parity/*.md` 행과 `vendor-src.lock`을 갱신하고 커밋에 실제 실행한 검증 명령 결과를 남긴다.

---

## 10. 근거

- **업스트림 (`can1357/oh-my-pi`, 읽기 전용)**:
  - `main` (`b636e19d10`): `docs/{native-crates,porting-to-natives,memory,context-files,handoff-generation-pipeline,extensions}.md`, `crates/{ast,builtins,diff,edit,iso,natives,predict,shell,vcs,vfs,voice,xfs}`, `sdk/rust/omp-rpc`, `packages/{coding-agent,ai,tui,catalog,mnemopi,snapcompact,browser-relay,stats}`
  - `omp2` (`2f92f3b`): `PHILOSOPHY.md`, `AGENTS.md`, `CONTRIBUTING.md`, `deny.toml`, `about.toml`, `THIRD-PARTY-NOTICES.txt`, `docs/adr/0001~0036`, `docs/architecture/{crates,agent-loop,processes,extensions}.md`, `docs/py/`, `crates/*` 46개, `crates/e2e/tests/p1~p10`
  - GitHub 이슈: `#18808`(TS 제거 근거), `#12102`(omp2 셸 스냅샷 드리프트), `#14637`(LSP mux)
- **`oh-my-tau` (`25af44a`)**: `Cargo.toml`, `LICENSE`, `NOTICE.md`, `README.md`, `docs/wasm-plugins.md`, `crates/omp-*` 20개, `vendor/*` 12개, `ext-host/*` 4개, `tests-golden/`, `tests-tools/`, `tests-e2e/`, `scripts/compat-oracle/`, `.github/workflows/{ci,release}.yml`
- **조사 원본 아카이브**: `docs/research/01~24` (`docs/research/README.md` 색인 참조)
