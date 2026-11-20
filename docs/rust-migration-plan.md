# oh-my-tau Rust 전환 마이그레이션 계획

상태: 초안 (2026-11-20)
대상: `jc01rho/oh-my-tau` (binary `omt`, MIT)
기준 업스트림: `can1357/oh-my-pi` main (`b636e19d10`) + `omp2` 브랜치 (`2f92f3b`)
조사 근거: `docs/research/` (아래 §10 근거 목록)

---

## 1. 요약

oh-my-tau는 이미 "Rust 전환"을 끝낸 프로젝트다. 20개 `omp-*` 크레이트와 13개 벤더 크레이트로
`v0.12.0`까지 진행했고, `omp-cli` 7.0만 줄·`omp-tui` 6.6만 줄 규모의 Rust 코드가 있다.
따라서 이 계획의 대상은 "TS를 Rust로 옮기는 일"이 아니라 **이미 옮긴 Rust를 업스트림의
현재 설계와 다시 정렬하고, 남은 TS 의존(ext-host)을 걷어내는 일**이다.

업스트림은 두 트랙으로 움직인다.

| 트랙 | 위치 | 성격 | 규모 |
| --- | --- | --- | --- |
| main | `can1357/oh-my-pi` | TS 하네스 + 증분 Rust 네이티브(`crates/natives` N-API) | TS 2.37M줄 / Rust 0.86M줄 |
| omp2 | 같은 저장소의 `omp2` 브랜치 | TS를 버린 완전 Rust 재작성 | Rust 1.03M줄 / 46 크레이트 |

oh-my-tau는 main 트랙의 `pi-*` 네이티브 크레이트를 벤더링해 출발했고, 그 뒤 독자적으로
`omp-*` 크레이트를 쌓았다. 결과적으로 **세 계층이 서로 다른 시점의 설계를 섞고 있다**:
main의 `pi-*`(2025년 말 네이티브 API), omp2가 정리한 아키텍처 원칙(ADR 36건), 그리고
oh-my-tau 자체 확장. 이 계획은 그 셋을 한 방향으로 정렬한다.

핵심 판단 4가지:

1. **omp2는 "따라갈 대상"이 아니라 "설계 참조"다.** omp2의 크레이트 경계·ADR·규율은 이식
   가치가 있지만, omp2는 TS 생태계를 버린 대가로 확장 생태계를 처음부터 다시 시작했다
   (ADR 0036: pi의 TS 확장은 이식 불가). oh-my-tau는 omp 세션/설정 호환을 제품 가치로
   내걸었으므로 omp2를 그대로 복제하면 그 가치가 사라진다.
2. **가장 큰 실질 격차는 도구·셸·메모리다.** 도구 스키마 패리티 18/29(62%), 셸/빌트인
   패리티 0, 메모리 크레이트 부재, 컨텍스트 파일 탐색은 업스트림의 일부만 구현. 이 셋이
   사용자가 실제로 체감하는 격차다.
3. **ext-host는 유일한 남은 TS 런타임 의존이고, 가장 위험한 부분이다.** 1,482줄 Bun
   호스트가 `ExtensionAPI`를 흉내내지만 약 12개 메서드가 noop이고 `runEphemeralTurn`은
   throw한다. 확장이 "성공"을 보고하면서 아무 일도 하지 않는 조용한 열화다.
4. **검증 게이트가 업스트림보다 훨씬 약하다.** oh-my-tau CI는 `cargo test` + 3-OS 빌드 +
   musl 정적 빌드뿐이다. clippy 게이트도, 패리티 게이트도, 실PTY 증명도 없다. 마이그레이션
   이전에 게이트를 먼저 세우지 않으면 이후 모든 단계가 검증 불가능해진다.

---

## 2. 현재 상태 실측

### 2.1 oh-my-tau 코드 규모

| 영역 | 파일 | 줄 |
| --- | ---: | ---: |
| `crates/omp-cli` | 93 | 70,497 |
| `crates/omp-tui` | 103 | 66,466 |
| `crates/omp-stats` | 86 | 21,153 |
| `crates/omp-providers` | 28 | 17,867 |
| `crates/omp-collab` | 13 | 12,412 |
| `crates/omp-lsp` | 18 | 7,697 |
| `crates/omp-compact` | 22 | 6,890 |
| 나머지 13개 `omp-*` | 76 | 26,089 |
| **crates 합계** | **439** | **245,071** |
| `vendor/pa-*` | 180 | 78,381 |
| `vendor/pi-*` | 75 | 33,019 |
| `vendor/crossterm` | 55 | 11,584 |
| `vendor/laya-ort` | 2 | 539 |
| **vendor 합계** | **312** | **123,523** |
| `ext-host/` (TypeScript) | 4 | 1,482 |

### 2.2 커밋 이력

```
89fdbae  initial public release
549bd39  v0.10.0: faster RPC exit, fault handling, --session, install script
4b0f144  Brand assets
30fcd5e  features: scaffold feature command modules
95da100  features wave 1: /plan /wt /btw /loop /shake /restart /skills /marketplace
         /share /ssh /browser /computer /budget /sandbox, omt daemon, CI
dc68ab2  Portability: Windows/macOS/arm64 Linux
87edb60  Merge PR #1 (features-wave1)
1ccf7ab  v0.11.0
ac7bbf4  release: rebuild tag via workflow_dispatch
fb87954  release: Intel macOS without ONNX
2dc9ce1  wave 2: static musl, parallel session scan, WASM plugins, goose dedup
7261ecf  Merge PR #2 (features-wave2)
918731f  v0.12.0
4ad4208  musl: aarch64 erratum linker flag
d086e86  release: glibc assets on ubuntu-22.04
2140a41  release: glibc on ubuntu-24.04
63c950a  login: port remaining 14 provider flows
25af44a  Merge PR #3 (features-wave3)   ← HEAD
```

패턴: **릴리스 → 기능 웨이브 → 이식성/릴리스 수정**의 반복. 각 웨이브는 기능을 넓게
추가했고, 그 뒤에 패리티 보강은 없었다. wave 3는 로그인 프로바이더 14종 추가가 전부다.

### 2.3 현재 CI (`\.github/workflows/ci.yml`)

| 잡 | 내용 |
| --- | --- |
| `test` | ubuntu-latest, `cargo test --workspace` |
| `build` | macOS / ubuntu-24.04-arm / windows(continue-on-error), `cargo build --release -p omp-cli` |
| `musl` | zig로 x86_64-musl 정적 빌드 후 `--version` 실행 |

없는 것: clippy, rustfmt 검사, `cargo deny`, 골든/패리티 실행, 실PTY TUI 증명,
E2E, 문서 테스트. `tests-golden`·`tests-e2e`·`tests-tools`는 저장소에 있으나 CI에서 돌지 않는다.

### 2.4 omp2 CI (참조 기준)

`format` → `runtime_spec`(`cargo -Zscript scripts/check-spec.rs`) → `licenses`(`cargo deny`)
→ `rust`(macos-15 arm64: clippy --workspace, nextest, doctests, P1–P8) → `p7_linux`(실PTY).
명령의 단일 출처는 `justfile`이고 raw cargo는 금지된다.

---

## 3. 업스트림 두 트랙 분석

### 3.1 main 트랙 — 증분 네이티브

main은 TS를 유지한 채 뜨거운 경로만 Rust로 내린다. `docs/porting-to-natives.md`가 그 절차서다.

- 경계: `@oh-my-pi/pi-natives` (N-API) → `omp-natives` → `omp-{ast,diff,edit,iso,predict,vcs,voice}`,
  `omp-xfs`/`omp-vfs`, `omp-shell`(bash 5.3 엔진 + `omp-builtins`)
- 판단 기준: "CPU·블로킹 I/O·할당·플랫폼 통합 비용이 실측으로 제거되고 경계가 데이터 지향일 때만"
- 금지: JS 객체 정체성·동적 import·애플리케이션 상태 콜백에 의존하는 작업은 JS에 남긴다
- 크레이트 규모: `shell` 350,765줄 / `builtins` 215,916줄 / `natives` 141,977줄

oh-my-tau가 벤더링한 `pi-*` 6종은 이 트랙의 산출물이다. 즉 oh-my-tau는 main 트랙의
"네이티브 부분"만 가져와 나머지를 독자 Rust로 채운 혼합형이다.

### 3.2 omp2 트랙 — 완전 재작성

`PHILOSOPHY.md` 한 줄: *"build a coding harness so good that the model is the only
replaceable part."*

두 개의 베팅:
- **하네스가 제품이다** — 모델은 수렴하고, 하네스가 모델의 몇 %를 실제로 쓸 수 있는지를 정한다
- **에이전트는 빌더이자 사용자다** — 언어 선택은 "관용구 수렴"으로 결정한다.
  TS는 모든 코드베이스가 자기 방언을 만들게 하고, Rust는 불건전한 버전을 실행 전에 거부한다

6개 교리: 구조적 진실을 투영된 뷰로 / 전체 메커니즘을 소유하거나 내보내지 않거나 /
추측 후 효과 / 편의보다 정직 / 코드보다 데이터 / 무게 없는 기계장치는 삭제.

`omp2`가 실제로 삭제한 것들: 병렬성 플래너, 승인 게이트, 취소 분류 체계, `loadMode`.
이들은 "만들어서 측정한 뒤 지운" 목록이다.

ADR 36건 중 이 계획에 직접 관련된 것:

| ADR | 결정 | oh-my-tau 함의 |
| --- | --- | --- |
| 0003 | 하나의 권위 세션 트리, 저널은 그 패치 스트림 | JSONL v3 선형 로그와 근본적으로 다름 |
| 0004 | rewind/fork/resume/replication이 트리에서 파생 | 분기를 파일 재작성 없이 1급으로 |
| 0007 | 서브에이전트는 CoW 뷰를 받고 diff를 반환 | `/wt`·샌드박스 설계에 직접 영향 |
| 0009 | 출력은 중앙에서 한 번만 경계 짓고, 전체는 아티팩트로 | 도구 출력 처리 재설계 |
| 0011 | 취소는 런타임 보장, 협조적 예절이 아님 | ext-host 수명 관리의 근거 |
| 0017 | 모델 호환성은 컴파일된 지식, 명시적 우선순위 | `omp-providers` 방향 |
| 0018 | 프로바이더는 `stream` 함수가 아니라 공유 인프라 | `pa-ai` 대체 방향 |
| 0024 | 영구 도구 로스터는 작고 고정 | 도구 추가 정책 |
| 0028 | `Bash`는 정책 인지형 in-process 인터프리터 | 가장 큰 단일 격차 |
| 0035 | 언어 선택은 아키텍처다, 엔진은 Rust | 이미 달성 |
| 0036 | 확장은 임베디드 CPython, JS/TS 런타임 없음 | ext-host 제거의 최종 목표 |

### 3.3 oh-my-tau가 취할 위치

omp2를 복제하지 않는다. 이유는 셋이다.

1. **호환 계약**: oh-my-tau는 `~/.omp/agent`의 설정·세션 파일을 omp와 공유한다고 README에
   명시했다. omp2의 `.oms`/DOM/저널 모델로 갈아타면 이 계약이 깨진다.
2. **확장 생태계**: omp2는 ADR 0036에서 pi의 TS 확장을 포기했다. oh-my-tau의 ext-host는
   조악하지만 실제로 omp 확장을 실행한다. 이건 자산이지 부채만은 아니다.
3. **규모**: omp2는 103만 줄이다. oh-my-tau 24.5만 줄이 그 설계를 전부 소화할 수 없다.

따라서 취할 것은 **원칙과 경계**이고, 버릴 것은 **형식**이다. 구체적으로:
ADR 0028(셸)·0024(도구 로스터)·0017/0018(프로바이더)·0011(취소)·0009(출력 경계)는
oh-my-tau의 기존 구조에 맞춰 흡수 가능하다. ADR 0003/0004(저널/DOM)는 흡수하지 않고
JSONL v3 위에서 "분기 1급화"라는 목표만 가져온다.

---

## 4. 격차 분석

### 4.1 도구 패리티 — 18/29 (62%)

공유 18종 중 스키마가 정확히 일치: `find`, `write`, `wait`, `checkpoint`, `rewind`,
`ast_grep`, `ast_edit`, `think`, `yield`, `goal`, `grep`(wire shape).

실질 불일치 4종:

| 도구 | 격차 |
| --- | --- |
| `ask` | 대상은 폐기된 `{questions:[…]}` 형태, 참조는 `{html: string}` |
| `todo` | `task`가 `string`(참조는 `string\|string[]`), 참조에 없는 `reason` 필드 추가 |
| `web_search` | `max_tokens`·`temperature`·`num_search_results` 누락 |
| `bash` | `notify` 누락, 설정 의존 스키마 6종을 정적 1종으로 붕괴, `name` `maxLength:48` vs 문자클래스 정규식 |

누락 11종: `debug`, `github`, `context_notes`, `new_context`, `security_scan`,
`memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill`.
레지스트리 밖 참조 도구 7종도 없음: `computer`·`browser`(eval prelude), `generate_image`·`tts`,
`vibe_*` 5종, `report_tool_issue`.

설정 게이트도 좁다: 대상 11개 vs 참조 22개.

**끊긴 선언 3건**(선언은 있는데 구현이 없음):
- `tools/task.rs`가 구성 불가능한 메모리 도구 4종을 화이트리스트에 넣음
- `builtin_commands.json`이 `security_scan` 도구 없이 `security` 슬래시 명령 + 11개 서브명령 광고
- `settings_defs.rs`가 구현 없는 `github` 도구 설정 광고

omp2의 도구 계약과 비교하면 결손이 더 분명해진다. omp2 `omp-tool`은 도구를 다음 셋으로 규정한다.

1. **의도 파라미터 `i`**: 모든 모델 노출 스키마에 `i`(현재분사 의도)가 첫 필수 속성으로 강제 주입되고,
   `decode_params`가 디코드 전에 걷어낸다. 저널·디스패치 정책에는 남고 도메인 타입은 오염되지 않는다.
2. **버전 정체성**: `Rev { family, n }` + `ToolIdentity`. 과거 개정은 `lift(from: &Rev, RecordedCall)`
   순수 프롬프트/리프트 코드로만 살아남고 광고는 현행 개정만. `projection_code: [u8;32]`로
   투영 결과를 캐시한다.
3. **단일 스트림 수명**: `Tool::call(IncomingParams) -> Stream<Ev>`. 인자 스트림을 선형으로 당기고
   명시적 커밋 프레임이 있어야 효과가 승인된다. 스트림이 터미널 없이 끝나면 `Abort::MissingOutcome`을
   합성한다. 효과(`Effects`)는 deny-all 기본 봉투에 `narrow()` 부분집합 검사다.

oh-my-tau 도구는 `{name, description, input_schema}` + `fn(input) -> Result<output>` 형태다.
의도 파라미터도, 개정도, 스트리밍 커밋 프레임도, 효과 봉투도 없다. 이 중 **지금 당장 필요한 것은
`i` 주입과 효과 봉투**이고, 스트리밍 수명은 Phase 7로 미룬다(프로바이더 스트리밍과 함께 재설계해야
일관된다). 개정 정체성은 외부 호스트 호환(RPC)을 할 때 의미가 생긴다.

### 4.2 셸·빌트인 — 패리티 0

| 항목 | 업스트림 | oh-my-tau |
| --- | --- | --- |
| bash 엔진 | bash 5.3 IR + VM + safepoint, zsh 방언, ZLE 위젯 193개 | 호스트 `bash -c` 스폰 |
| in-process 유틸리티 | 97개(`rg`·`awk`·`jq`·`sed`·`fd`·`rsync`·`tar`·`watchexec` 등) | 없음 |
| 프로세스 빌트인 | 7개(`ps`·`top`·`pgrep`·`pkill`·`pidwait`·`lsof`·`sleep`) | 없음 |
| 출력 최소화 | ~60개 정의 | 없음 |

`omp-cli/src/tools/bash.rs` 헤더는 "omp embeds a brush-based shell"이라고 적고 있다. main 트랙 기준으로는
틀린 서술이지만(그쪽은 bash 5.3 자체 엔진), **omp2 기준으로는 맞다**: omp2의 `crates/shell`은
`brush-core` 0.5.0 + `brush-parser` 0.4.0 포트이고 `crates/shell-builtins`는 `pi-builtins` 포트다.

이 구분이 이식 전략을 바꾼다. main의 셸(350k줄)은 zsh 방언·ZLE·minimizer·testkit을 포함한
"제품으로서의 셸"이고, omp2의 셸(48.5k + 90.4k줄)은 임베더블 라이브러리다. oh-my-tau에 필요한 것은
후자다. omp2 `shell-builtins`는 레지스트리를 3개로 분리해 임베더가 `rm`/`mv`/`ln`을 뺄 수 있게 했고,
`PathPolicy`·`ProcessGroupPolicy`·`SpawnWrapper`·`SpawnObserver` 정책 훅과 `analysis.rs`
정적 AST 분석을 제공한다. 샌드박스와 spawn 가로채기가 나중에 덧붙인 것이 아니라 1급이다.

### 4.3 메모리 — 크레이트 부재

- `crates/memory` 상당 없음. omp2의 `crates/memory`(6,428줄, SQLite 6테이블, 4-voice RRF)와
  TS `packages/mnemopi`(20,970줄) 모두 대응물 없음
- `omp-settings/src/schema.rs`에 메모리 도메인 74개 id가 **선언만** 되어 있고 소비자가 없음
  (74개 중 44개가 schema 외 참조 0)
- `/memory` 슬래시는 `slash.rs:757-776`에서 리터럴 문자열 반환 — 백엔드 디스패치 없음
- `recall`·`retain`·`reflect`·`memory_edit`·`learn` 도구 없음
- `memory://` 스킴 없음(`schemes/mod.rs`에 미등록)
- `rule://`는 등록됐지만 `active_rules()`가 빈 벡터 반환 → 모든 `rule://` 읽기가 "Unknown rule"

### 4.3b 코어 프리미티브 — 격차 후보

off2 `omp-core`가 제공하는 것 중 oh-my-tau에 대응물이 확인되지 않는 항목:

| 프리미티브 | 역할 | oh-my-tau 상태 |
| --- | --- | --- |
| `slopjson` | 관대한 JSON 문법(작은따옴표·미인용 키·주석·후행 쉼표) + 스트리밍 + 증분 커서 + 접두 분류기 + 복구 | 확인 안 됨 |
| `Str` | 24바이트 / 23바이트 인라인 SSO + `StrExt` 무할당 변형 | 확인 안 됨 |
| `Hash32` | SHA-256 다이제스트 통화, aarch64 런타임 SHA 디스패치 | `pa-*` 내부에 유사 기능 가능 |
| `AppendVec` | lock-free 세그먼트 append 저장소 | 확인 안 됨 |
| `SparseMap`/`SparseSet` | dense index + sparse key | 확인 안 됨 |
| `SecretString`/`ct_eq` | zeroize + 상수시간 비교 | 확인 안 됨 |
| `location`/`principal`/`phase` | `AgentUrl`·`WorkspaceUri`·`LifecyclePhase`·`Principal` | 부분(`omp-urls`) |

이들은 **지금 당장 옮길 대상이 아니다**. omp2 전체가 이 어휘 위에 서 있어 개별 이식은
이음매가 맞지 않는다. 다만 `slopjson`(모델 출력 파싱)과 `Str`(핫패스 할당)은 독립 가치가
있어 Phase 4·7에서 선택적으로 검토한다.

### 4.3c oh-my-tau 고유 자산 — 이식 중 삭제 금지

off2와 main 어느 쪽에도 대응물이 없는 항목:

| 크레이트 | 줄 | 성격 |
| --- | ---: | --- |
| `omp-sandbox` | 377 | Landlock ABI V9 + 선택적 seccomp + `exec_confined` pid 보존 |
| `omp-judge` | 1,710 | Laya 턴 판정기(shadow worker, 오프라인 채점) |
| `omp-sot` | 2,347 | State-of-Thought 컨트롤러(arXiv 2609.16055) |
| `omp-wasm` | 741 | WASM 플러그인 호스트(wasmi, 연료 계량) |
| `omp-stats` | 21,153 | 세션 로그 → SQLite → 대시보드 |
| `omp-collab` | 12,412 | 협업 릴레이 |
| `omp-auth-broker` | 2,582 | 자격증명 보관소 데몬 |

### 4.4 컨텍스트 파일 — 부분 구현

`sysprompt.rs:153-192`는 독립 `AGENTS.md`/`CLAUDE.md` 상향 탐색만 구현했다. 누락:
- 프로젝트 `.omp/AGENTS.md`(우선순위 100, 나머지를 가림)
- 사용자 스코프 `~/.omp/agent/AGENTS.md`
- `RULES.md` 끈끈한 규칙(항상 적용, `/clear` 시 재발견)
- `GEMINI.md`, `.github/copilot-instructions.md`, `.claude/CLAUDE.md`, `.codex/AGENTS.md`

`sysprompt.rs:990-991`은 `"rules": []`·`"alwaysApplyRules": []`를 하드코딩한다.

### 4.5 압축(snapcompact) — 세 구현이 서로 다른 것

| 능력 | TS 참조 | omp2 | oh-my-tau |
| --- | --- | --- | --- |
| 절단점 선택 | ○ | ✗ | ○ |
| LLM 요약 요청 | ○ | ✗ | ○ |
| 모델별 방언 10종 | ○ | ✗ | ○ |
| 컨텍스트 재구성 | ○ | ✗ | ○ |
| 비트맵 렌더러 | ○ | ○ | ✗ |
| 텍스트 정규화 | ○ | ✗ | ✗ |
| 셀 인지 래핑/페이지네이션 | ○ | 부분 | ✗ |
| 포비에이티드 레이아웃 | ○ | ✗ | ✗ |
| 프레임 청구(catalog 규칙) | ○ | 부분 | 하드코딩 `FRAME_TOKEN_ESTIMATE = 5024` |
| 아카이브 영속 | ○ | 부분 | 재생만 |
| blob 프레임 해석 | ○ | ○ | ✗ (모두 "unavailable" 갭) |

토크나이저가 가장 날카로운 분기점이다. TS 참조는 내장 utok로 정확 카운트하고
`approximate`/`upperbound`/`strict` 3모드를 가지며 메시지별 메모이즈한다. oh-my-tau는
바이트/4 근사에 하드코딩 상수를 쓴다.

### 4.6 VCS·격리 — 방향이 반대

- **git**: 업스트림은 `omp-vcs`에서 gitoxide 기반 in-process `Repo`(Git/Jj 디스패치, ~60 연산).
  oh-my-tau는 `features/worktree.rs`의 `git()` 헬퍼로 모든 읽기마다 fork+exec하고
  stderr를 문자열 매칭한다. 워크스페이스에 `gix`/`git2` 의존이 전혀 없다.
- **워크트리**: 업스트림 `worktree_add`는 브랜치 검증·admin dir 등록·gitoxide 체크아웃·
  CoW 클론(`clone_holes`)·`keep_changes`(staged/unstaged/untracked 이월 + rebase된 인덱스)까지
  한다. oh-my-tau `/wt new`는 `git worktree add <path> -b omt/<name>` 한 줄이다.
- **격리 PAL**: 업스트림 `omp-iso`는 8개 백엔드(APFS clonefile, btrfs, ZFS, FICLONE,
  overlayfs, Windows block clone, ProjFS, rcopy) + `probe/start/stop/clone_tree/diff` 트레잇.
  oh-my-tau에는 대응 크레이트가 없다. `worktree.clone`·`isolation.backend` 설정은
  TUI 스키마에 선언만 되어 있다.
- **샌드박스**: 여기서는 oh-my-tau가 앞선다. `omp-sandbox`는 Landlock ABI V9 + 선택적
  seccomp(AF_INET/AF_INET6 → EACCES) + `exec_confined` 재실행으로 pid를 보존한다.
  업스트림 `iso`는 파일시스템 격리이지 syscall 구속이 아니다. **이식하지 말고 유지한다.**
- **jj**: 업스트림은 jj-lib 기반 `JjWorkspace`, oh-my-tau는 0줄.

### 4.7 확장 모델 — 세 모델이 서로 다른 축

| | oh-my-pi(TS) | omp2(Python) | oh-my-tau |
| --- | --- | --- | --- |
| 실행 | in-process TS API | 임베디드 CPython 3.14t, 확장당 killable child | Bun ext-host + WASM 샌드박스 |
| 권한 | 문서화된 미격리 | 서명된 매니페스트가 코드 실행 전에 권위 | Tier A 무변경 호환 / Tier B 도구·명령만 |
| 이벤트 | 45개 ad-hoc | 5-arm 타입 결정 어휘 | 손실 있는 호환 |
| 실패 모드 | 프로세스 전체 위험 | 확장별 격리 | **조용한 열화**(약 12개 메서드 noop, `runEphemeralTurn` throw) |

oh-my-tau의 Tier A는 소스 변경 없이 pi 확장을 돌리지만, 확장이 "성공"을 보고하면서
아무 일도 하지 않는다. 이건 기능 부재보다 나쁘다 — 사용자가 알 수 없기 때문이다.

### 4.8 검증 인프라

저장소에 있으나 CI에서 돌지 않는 것:
- `tests-golden/`: TS↔Rust JSONL 쌍 12개 시나리오(`prompt`, `compact`, `abort`,
  `steer_follow_up`, `branch_messages`, `thinking_state`, `slash`, `state`, `bash_command`,
  `set_model_prompt`, `new_session`, `abort_mid_tool`) + `compare.ts`
- `tests-tools/`: `ts_tools.json`·`todo_golden.json`·`ts_schemas.ts`
- `tests-e2e/`: auth refresh, ext conformance, TUI 드라이버(11개 프로브)
- `scripts/compat-oracle/`: 요청 본문 오라클

`tests-golden/compare-baseline.txt`는 현재 19개 차이 항목을 기록하고 있다. 이건
"패리티가 깨져 있다"는 사실이 저장소 안에 이미 적혀 있다는 뜻인데, CI가 돌지 않으니
아무도 보지 않는다.

---

## 5. 마이그레이션 원칙

1. **호환 계약을 먼저 못 박는다.** `~/.omp/agent` 설정·세션 파일 호환은 제품 계약이다.
   이 계약을 깨는 변경(저널/DOM 전환, 세션 포맷 변경)은 별도 결정 없이 하지 않는다.
2. **게이트 먼저, 기능 나중.** §6 Phase 0이 끝나기 전에는 어떤 기능 이식도 시작하지 않는다.
   게이트 없는 이식은 "했다"고 말할 수 없는 이식이다.
3. **패리티 원장을 단일 진실로.** 도구·슬래시·설정·셸 빌트인 각각에 기계 판독 가능한
   원장을 두고, 원장에 없는 항목은 "미이식"으로 간주한다. 사람 기억에 의존하지 않는다.
4. **omp2는 형식이 아니라 원칙을 가져온다.** ADR 번호를 인용해 결정을 기록하되,
   omp2의 크레이트 구조를 복제하지 않는다.
5. **조용한 열화를 금지한다.** 확장·도구·설정 중 미구현은 실행 시 명시적 오류 또는
   시작 시 경고로 드러낸다. noop 스텁은 버그다.
6. **앞서 있는 것은 유지한다.** `omp-sandbox`(Landlock+seccomp), WASM 플러그인,
   Laya judge(`omp-judge`), `omp-sot`은 업스트림에 없는 자산이다. 이식 과정에서 지우지 않는다.
7. **핀과 델타.** 업스트림 `pi-*`·`pa-*` 벤더는 커밋 핀과 델타 목록으로 관리한다.
   핀 없이 "최신"을 따라가지 않는다(omo의 PINS.md 방식).

---

## 6. 단계별 계획

각 단계는 독립적으로 완료·검증 가능하고, 앞 단계의 산출물을 입력으로 쓴다.

### Phase 0 — 검증 기반 (선행, 다른 모든 단계의 전제)

목표: "무엇이 깨졌는지"를 CI가 말하게 만든다. 기능 추가 없음.

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 0.1 | `cargo clippy --workspace -- -D warnings` 게이트 추가 | CI에 `lint` 잡, 현재 위반 0 또는 명시적 `#[allow(reason)]` 목록화 |
| 0.2 | `cargo fmt --check` 게이트 | CI `format` 잡 |
| 0.3 | `cargo deny check licenses sources` 게이트 | CI `licenses` 잡, `deny.toml` 신규 |
| 0.4 | `tests-golden/compare.ts`를 CI에서 실행 | 12개 시나리오 diff 0 또는 baseline에 명시 |
| 0.5 | `tests-tools/compare.ts` CI 실행 | 도구 스키마 diff가 원장과 일치 |
| 0.6 | 실PTY TUI 증명 잡 | `tests-e2e/tui/run.py`가 CI 러너에서 통과 |
| 0.7 | `justfile` 도입 | 명령 단일 출처, raw cargo 금지 |
| 0.8 | 벤더 핀 문서화 | `docs/vendor-pins.md`: 각 `pi-*`/`pa-*`의 업스트림 커밋 + 델타 목록 |

검증: 이 단계 종료 시 `just ci` 하나로 format+lint+license+golden+tools+e2e가 돈다.

### Phase 1 — 패리티 원장과 조용한 열화 제거

목표: "무엇이 없는지"를 기계가 말하게 하고, 거짓 성공을 없앤다.

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 1.1 | 도구 패리티 원장 `docs/parity/tools.md` 생성 | 29+7종 각각 상태(일치/불일치/누락)와 근거 |
| 1.2 | 스키마 불일치 4종 수정(`ask`, `todo`, `web_search`, `bash`) | golden diff 0 |
| 1.3 | 끊긴 선언 3건 제거 또는 구현 | `security` 명령·`github` 설정·task 화이트리스트 정합 |
| 1.4 | ext-host noop 메서드 목록화 → 명시적 오류로 전환 | noop 12개가 `throw new UnsupportedExtensionApi(...)` |
| 1.5 | `rule://` 빈 구현을 명시적 오류로 | "Unknown rule" 대신 "rulebook not implemented" |
| 1.6 | 설정 스키마의 미소비 항목 표시 | `schema.rs`에 `#[implemented = false]` 또는 별도 목록 |
| 1.7 | `bash` 도구 주석의 `brush` 오기 수정 | 주석이 실제 엔진을 가리킴 |

검증: 원장의 모든 행이 코드 참조를 갖고, noop/빈 구현이 0.

### Phase 2 — 셸·빌트인 이식 (가장 큰 체감 격차)

목표: `bash` 도구가 호스트 셸 스폰에서 in-process 인터프리터로 바뀐다.

전략: **omp2의 `crates/shell` + `crates/shell-builtins`를 벤더링한다**(MIT, brush 포트).
main의 `crates/shell`(350k줄, zsh/ZLE/minimizer/testkit 포함)은 oh-my-tau에 과하고,
omp2 쌍(48.5k + 90.4k줄)이 임베더블 라이브러리로 정확히 필요한 형태다.

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 2.1 | `vendor/omp-shell`, `vendor/omp-shell-builtins` 벤더링 | 워크스페이스 빌드 통과, 바이너리 크기 증가 측정 |
| 2.2 | `omp-cli` `bash` 도구를 in-process 엔진으로 배선 | 기존 `bash_command` golden 통과 |
| 2.3 | 셸 빌트인 패리티 원장 `docs/parity/shell.md` | 57 셸 빌트인 + 59 유틸 + 8 프로세스 빌트인 상태 |
| 2.4 | 레지스트리 분리 수용 | `rm`/`mv`/`ln`을 정책으로 withhold 가능 |
| 2.5 | 정책 훅 배선 | `PathPolicy`·`ProcessGroupPolicy`·`SpawnWrapper`를 `omp-sandbox`와 연결 |
| 2.6 | `analysis.rs` 정적 AST 검사 배선 | 실행 전 거부 결정, 규칙 원장 |
| 2.7 | 취소 경계(ADR 0011) 적용 | 도구 취소가 프로세스 그룹까지 도달 |
| 2.8 | 폴백 경로 유지 | 엔진 실패 시 `bash -c` 폴백 + 경고 |
| 2.9 | `dyn` 빌트인으로 MCP/동적 도구 연결 | 도구 로스터 불변 유지(ADR 0024) |

검증: `tests-golden/rs/bash_command.jsonl`이 새 엔진에서 통과하고, PTY 없는 환경에서도
파이프·리다이렉트·heredoc 시나리오가 동일 출력.

### Phase 3 — 도구·메모리·컨텍스트

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 3.1 | `memory://` 스킴 등록 | `read memory://root` 동작, 100레코드/1MiB 경계 |
| 3.2 | 메모리 백엔드 최소 구현(SQLite + FTS) | `/memory stats`가 실제 수치 반환 |
| 3.3 | `recall`·`retain` 도구 구현 | 도구 원장 갱신, 서브에이전트 읽기전용 분류 정합 |
| 3.4 | `reflect`·`memory_edit`·`learn` | 또는 원장에서 "의도적 미지원"으로 확정 |
| 3.5 | 컨텍스트 파일 탐색 확장 | `.omp/AGENTS.md`, 사용자 스코프, `RULES.md` |
| 3.6 | `RULES.md` 끈끈한 규칙 | `rules`/`alwaysApplyRules`가 실제 배열 |
| 3.7 | 설정 게이트 22종으로 확대 | 원장 대조 |

검증: 메모리·컨텍스트 시나리오 골든 + `/memory` 슬래시가 상태를 실제로 바꿈.

### Phase 4 — 프로바이더·추론 정렬

ADR 0017/0018 흡수. `pa-ai`(52,276줄) 위에 얹힌 `omp-providers`(17,867줄)를 유지하되,
프로바이더별 코드를 카탈로그 데이터로 밀어낸다.

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 4.1 | 프로바이더 카탈로그 스키마 확정 | `scripts/sync-catalog.sh` 산출물이 단일 소스 |
| 4.2 | `model_id.contains(...)` 류 분기 제거 | grep 0건, 카탈로그 행으로 대체 |
| 4.3 | 호환성 규칙을 데이터로 | `compat-oracle` 결과와 카탈로그 정합 |
| 4.4 | 토크나이저 정확 카운트 | utok 또는 동등 엔진, 3모드 API |
| 4.5 | 프레임 청구를 카탈로그 규칙으로 | 하드코딩 `5024` 제거 |

검증: 프로바이더 추가가 Rust 코드 변경 없이 카탈로그 행만으로 가능.

### Phase 5 — VCS·워크트리

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 5.1 | `gix` 도입, 읽기 경로 in-process화 | branch/dirty/log/diff/status가 fork 없이 |
| 5.2 | `git()` 문자열 stderr 매칭 제거 | 구조화 에러 |
| 5.3 | 워크트리 검증·admin dir 등록 | `worktree_add` 동등 동작 |
| 5.4 | `keep_changes` 이월 | `/wt new`가 현재 변경을 이월 |
| 5.5 | CoW 클론(가능한 백엔드만) | FICLONE 우선, 실패 시 rcopy |
| 5.6 | jj 지원 | 또는 원장에서 "미지원" 확정 |
| 5.7 | `omp-sandbox` 유지 확인 | Landlock+seccomp 회귀 테스트 |

검증: `/wt` 시나리오 골든 + 대형 저장소에서 status 지연 측정.

### Phase 6 — 확장 런타임

가장 위험하고 가장 늦게. 두 갈래를 먼저 결정해야 한다.

**결정 분기 A — Bun ext-host 유지 + WASM 확대**
- 장점: 기존 omp TS 확장이 계속 돈다. 호환 계약 유지
- 단점: Bun 런타임 의존이 영구화, 격리 없음
- 비용: noop 제거(Phase 1.4) + 이벤트 커버리지 확대

**결정 분기 B — 임베디드 CPython(ADR 0036 방향)**
- 장점: 격리, kill 경계, 이식성, Eval 일관성
- 단점: 기존 TS 확장 전부 폐기, 생태계 재시작
- 비용: `omp-ext` 4,336줄 + `omp-wasm` 741줄을 Python 호스트로 재작성 + `omp-eval` 통합

이 계획은 **A를 먼저 완료하고 B를 별도 결정으로 미루는 것**을 권한다. 이유: A는
기존 자산을 살리면서 조용한 열화를 제거하는 유한 작업이고, B는 제품 계약(omp 확장 호환)을
깨는 결정이라 사용자 승인이 필요하다.

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 6.1 | ext-host 이벤트 커버리지 원장 | 45개 이벤트 중 구현/미구현 |
| 6.2 | noop → 명시적 오류 (Phase 1.4 완료분 검증) | 확장이 실패를 알 수 있음 |
| 6.3 | `runEphemeralTurn` 구현 또는 명시 거부 | throw 대신 계약된 동작 |
| 6.4 | WASM 플러그인 ABI 확장 | 도구·명령 외 훅 |
| 6.5 | 결정 분기 A/B 문서화 | `docs/adr/` 형식으로 기록 |

### Phase 7 — 압축·세션 심화

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 7.1 | snapcompact 렌더러 이식 | omp2 `crates/snapcompact`(2,042줄) 벤더링 |
| 7.2 | 텍스트 정규화·셀 인지 래핑 | TS 참조 동작 골든 |
| 7.3 | 프레임 청구 카탈로그화 (4.5 완료분) | 하드코딩 제거 |
| 7.4 | 아카이브 영속 | `Archive.text/textHead/textTail` |
| 7.5 | blob 프레임 해석 | "unavailable" 갭 0 |
| 7.6 | 세션 분기 1급화(ADR 0004 목표만) | JSONL v3 유지하며 `prior` 개념 도입 검토 |

검증: `tests-golden` compact 시나리오 + 토큰 카운트 오차 측정.

### Phase 8 — 정리

| # | 작업 | 완료 조건 |
| --- | --- | --- |
| 8.1 | 벤더 핀 갱신 절차 문서화 | `docs/vendor-pins.md` 최신 |
| 8.2 | 패리티 원장 전수 재측정 | 모든 원장이 CI 산출물 |
| 8.3 | `ext-host` 잔여 표면 축소 | TS 줄 수 감소 또는 동결 사유 기록 |
| 8.4 | 릴리스 노트에 패리티 상태 포함 | 릴리스마다 원장 링크 |

---

## 7. 우선순위와 근거

| 순위 | 단계 | 근거 |
| --- | --- | --- |
| 1 | Phase 0 | 게이트 없이는 이후 모든 "완료"가 주장일 뿐 |
| 2 | Phase 1 | 조용한 열화 제거 — 사용자 피해가 가장 직접적 |
| 3 | Phase 2 | 체감 격차 최대(패리티 0), 그러나 벤더링으로 비용은 낮음 |
| 4 | Phase 3 | 메모리·컨텍스트는 매 세션 영향 |
| 5 | Phase 5 | git fork/exec은 성능·정확도 모두 손해 |
| 6 | Phase 4 | 카탈로그 정리는 부채 상환 |
| 7 | Phase 7 | 압축 심화는 상위 기능 |
| 8 | Phase 6 | 제품 계약을 건드리므로 사용자 결정 필요 |

---

## 8. 리스크

| 리스크 | 영향 | 완화 |
| --- | --- | --- |
| 셸 벤더링이 빌드 시간·바이너리 크기를 크게 늘림 | 릴리스 파이프라인 | Phase 2.1에서 먼저 측정, feature gate로 분리 |
| `pi-*` 벤더가 업스트림과 계속 벌어짐 | 패리티 재악화 | Phase 0.8 핀 문서 + 분기별 델타 재측정 |
| ext-host noop → 오류 전환이 기존 확장을 깨뜨림 | 사용자 확장 | 명시적 오류 + 마이그레이션 안내, 1릴리스 유예 |
| JSONL v3 유지 결정이 omp2 방향과 충돌 | 장기 구조 부채 | ADR로 명시적 기록, 재검토 조건 명시 |
| omp2가 main으로 머지되면 참조가 이동 | 계획 기준 변경 | 분기별 업스트림 재확인, 핀 갱신 |
| 메모리 백엔드 최소 구현이 mnemopi와 호환 안 됨 | 데이터 이전 | 스키마 버전 필드 + 마이그레이션 경로 |

---

## 9. 검증 방법

각 단계는 다음 셋을 모두 만족해야 완료다.

1. **기계 검증**: `just ci` 통과(Phase 0 이후). 패리티 원장 diff 0 또는 명시된 예외.
2. **행동 검증**: 해당 단계의 사용자 표면을 실제로 실행. TUI는 실PTY, 도구는 골든,
   셸은 파이프·리다이렉트·취소 시나리오.
3. **증거 기록**: 커밋 메시지에 원장 링크와 실행한 검증 명령을 남긴다.
   "통과했을 것"은 증거가 아니다.

---

## 10. 근거

### 업스트림 (읽기 전용 클론)

- `/tmp/oh-my-pi-ro` — main `b636e19d10`
  - `docs/porting-to-natives.md`, `docs/native-crates.md`, `docs/natives-architecture.md`
  - `crates/{shell,builtins,natives,edit,xfs,vcs,ast,predict,vfs,iso,voice,diff}`
  - `sdk/rust/omp-rpc`
  - `packages/{coding-agent,ai,tui,catalog,utils,agent,stats,mnemopi,snapcompact,...}`
  - 이슈 #18808 (omp²의 TS 제거 근거 질문), #12102 (omp2 셸 스냅샷 드리프트)
- `/tmp/oh-my-pi-omp2` — `omp2` `2f92f3b`
  - `PHILOSOPHY.md`, `AGENTS.md`, `README.md`, `CONTRIBUTING.md`
  - `docs/adr/0001-0036`, `docs/architecture/{crates,agent-loop,processes,extensions}.md`
  - `docs/py/00-17`, `docs/pyx/`
  - `crates/*` 46개

### oh-my-tau

- `/home/whrho/git/oh-my-tau` — `25af44a` (main)
- `Cargo.toml`, `NOTICE.md`, `README.md`, `docs/wasm-plugins.md`
- `crates/omp-*` 20개, `vendor/*` 13개, `ext-host/*` 4개
- `tests-golden/`, `tests-tools/`, `tests-e2e/`, `scripts/compat-oracle/`
- `.github/workflows/{ci,release}.yml`

### 정량 측정

파일 수·줄 수는 `bun`으로 `crates/*`, `packages/*`, `vendor/*`, `ext-host/`를 재귀 순회해
확장자별로 집계했다(2026-11-20). `target/`·`node_modules/`·`.git/` 제외.
