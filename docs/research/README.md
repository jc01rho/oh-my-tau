# 조사 원본 (2026-11-20)

`docs/rust-migration-plan.md`의 근거 자료. `01~24`는 `oc-deepseek-v4.1-flash-free` 병렬 조사(21번은 잔여 크레이트·SDK·E2E 직접 실측 포함)로 수집한 근거 자료다.

| 파일 | 주제 | 대응 절 |
| --- | --- | --- |
| `01-omp2-philosophy-and-readme.md` | omp2 철학·두 베팅·8교리·결정 절차, README 워크스페이스/프로세스/IPC | §3.2 |
| `02-omp2-philosophy-summary.md` | 위의 압축 요약, TS 제거 근거 4가지 | §3.2 |
| `03-omp2-agents-rules-and-gates.md` | AGENTS.md 규율 전수, 포팅 규칙, 금지 패턴과 집행 지점, QA 게이트 | §2.4, §5 |
| `04-omp2-architecture-docs.md` | 크레이트 의존 그래프, 프로세스 토폴로지, 에이전트 루프, 확장 모델 | §3.2 |
| `05-tool-and-builtin-parity.md` | 도구 스키마 18/29, 누락 11종, 셸/빌트인 패리티 0, 끊긴 선언 3건 | §4.1, §4.2 |
| `06-memory-context-gap.md` | 메모리 크레이트 부재, 설정 선언만, `/memory` 스텁, 컨텍스트 파일 부분 구현 | §4.3, §4.4 |
| `07-snapcompact-comparison.md` | TS/omp2/tau 세 구현 능력 매트릭스, 토크나이저 분기 | §4.5 |
| `08-vcs-isolation-comparison.md` | git in-process vs fork/exec, 워크트리, 격리 PAL, 샌드박스 역전 | §4.6 |
| `09-extension-model-comparison.md` | TS/CPython/tau 세 확장 모델, 조용한 열화 | §4.7 |
| `10-omp2-journal-dom-session.md` | 저널 우선 세션, `.oms` SSE 포맷, fold/projection, rewind/fork | §3.2 |
| `11-omp2-ai-and-catalog.md` | Tower 서비스 스파인, 고정 순서 미들웨어, `ChatEvent`, 프로바이더=데이터 | §3.2, Phase 4 |
| `12-omp2-shell-and-builtins.md` | brush-core 포트 3계층, 빌트인 3레지스트리, main/omp2/tau 비교 | §4.2, Phase 2 |
| `13-omp2-tool-contract.md` | 의도 파라미터 `i`, 버전 정체성 `Rev`, 단일 스트림 수명, 효과 봉투 | §4.1 |
| `14-tau-unique-features.md` | `/ultra`, `omp-sot`, `omp-judge`/`laya-ort`, `omp-goose`, `omt web` 등 tau 고유 기능 검증 | §4.3c |
| `15-mhc-cleanroom-vs-mit.md` | mhc(SUL-1.0 클린룸) vs `oh-my-tau`(MIT 직접 포크) 적용·제외 항목 비교 | §5 |
| `16-vendor-pi-crates-diff.md` | `vendor/pi-*` 6종(18.9.1) vs `oh-my-pi` main(19.0.1) 시맨틱 diff 및 `pi-walker`→`omp-xfs` 전환 | §4.2b, Phase 2.5 |
| `17-vendor-crossterm-laya-goose.md` | `vendor/crossterm` 패치, `laya-ort`, `omp-judge`, `omp-goose` 출처·feature 경계 | §4.3c |
| `18-omp-tui-gap-analysis.md` | `omp-tui` 66.4k줄 정밀 실측, 누락 64 오버레이/차트/HTML폼, `omp2`와 아키텍처 차이 | §4.9, Phase 8 |
| `19-omp-providers-compat-oracle.md` | 4계층 모델 조회, `models.db` wyhash 패리티, `agent.db` v8 CAS, `sync-catalog.sh` 버그 | §4.10, Phase 4 |
| `20-omp-ext-and-wasm.md` | `omp-ext` + `ext-host`(`shard.ts` 공유 데몬 포함) 및 `omp-wasm` 심층 대조 | §4.7, Phase 6 |
| `21-tau-remaining-crates-inventory.md` | 잔여 크레이트 14종(`omp-lsp` mux 포함), `/browser`·`/computer`, `sdk/rust/omp-rpc`, `omp-e2e` | §4.11, §3.2 |
| `22-omp-session-and-compact.md` | `omp-session` JSONL v3 바이트 호환(JS `Number#toString`, 256B title 슬롯, mmap/rayon) 및 `omp-compact` | §4.5 |
| `23-omp-cli-inventory.md` | `omp-cli` 88파일·12서브커맨드·4실행모드(`acp`/`rpc-ui` 거부)·55 RPC 메서드·`web`/`daemon` | §4.11 |
| `24-omp-settings-sot-stats-urls.md` | `omp-settings`(548키·`provenance` 결함), `omp-sot`, `omp-stats`, `omp-urls`(11스킴 읽기전용) | §4.3, §4.11 |

## 재현 방법

업스트림은 읽기 전용으로만 받는다.

```sh
git clone --depth=200 https://github.com/can1357/oh-my-pi.git /tmp/oh-my-pi-ro
git clone --branch omp2 --single-branch --depth=50 \
  https://github.com/can1357/oh-my-pi.git /tmp/oh-my-pi-omp2
```

조사 시점 커밋: main `b636e19d10` (`v19.0.1-126-gb636e19d10`), omp2 `2f92f3b`, oh-my-tau `25af44a`.

## 주의

- 일부 워커(`05`, `12`, `13`, `17` 앞부분)는 도구 호출 예산을 초과해 서술이 중간에 잘렸거나
  후속 보충이 필요했다. 잘린 부분은 후속 워커(`16`, `17`, `20`) 및 `21`에서 직접 소스를 읽어
  교차 검증했다.
- `06`, `07`, `08`, `09` 중 일부 비교 표기는 계획 본문 §4와 `16~21`에서 실제 `oh-my-tau`
  파일 경로를 직접 확인해 보정했다.
