# 조사 원본 (2026-11-20)

`docs/rust-migration-plan.md`의 근거 자료. 각 파일은 `oc-deepseek-v4.1-flash-free` 병렬 조사
워커가 읽기 전용으로 수집한 결과를 그대로 보존한 것이다. 파일명만 주제별로 바꿨고 내용은
가공하지 않았다.

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

## 재현 방법

업스트림은 읽기 전용으로만 받는다.

```sh
git clone --depth=200 https://github.com/can1357/oh-my-pi.git /tmp/oh-my-pi-ro
git clone --branch omp2 --single-branch --depth=50 \
  https://github.com/can1357/oh-my-pi.git /tmp/oh-my-pi-omp2
```

조사 시점 커밋: main `b636e19d10`, omp2 `2f92f3b`.

## 주의

- 일부 워커는 도구 호출 예산을 초과해 서술이 중간에 잘렸다(특히 `05`, `12`, `13`).
  잘린 부분은 계획 본문에서 다른 경로로 확인했고, 원본은 가공하지 않고 그대로 둔다.
- `06`, `07`, `08`, `09`는 oh-my-tau 코드를 직접 읽지 않고 격차를 추론한 부분이 있다.
  계획 본문 §4에서 해당 주장을 실제 파일 경로로 재확인했다.
