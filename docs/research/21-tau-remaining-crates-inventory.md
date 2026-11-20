# 잔여 크레이트·업스트림 SDK·E2E 실측 요약 (2026-11-20)

`01~20` 조사에서 다루지 않았던 `oh-my-tau` 잔여 크레이트 14종과 `oh-my-pi` main의 Rust SDK(`omp-rpc`),
`omp2`의 E2E 증명 체계(`omp-e2e`), 그리고 wave 1 `/browser`·`/computer`·`/ssh`·`omp-eval` 구현을
직접 소스 대조로 확인한 기록이다.

---

## 1. oh-my-tau 잔여 크레이트 14종 실측

| 크레이트 | `.rs` 파일 | 줄 수 | `#[test]` 수 | 구현 현황 및 업스트림 대비 격차 |
| --- | ---: | ---: | ---: | --- |
| `omp-cli` | 93 | 70,497 | 515 | 메인 바이너리 `omt`. TUI·print·RPC·`web`·`daemon`·`judge-sessions`·`collab`·`login` 탑재. `runtime.rs` 6,029줄, `slash.rs` 3,319줄에 결합 집중 |
| `omp-lsp` | 18 | 7,697 | 9 | `src/lsp` v18.6.1 포트. **`mux/server.rs`(1,087줄)로 세션 간 LSP 공유 데몬 이미 구현** (`tests/shared_mux.rs` 736줄) |
| `omp-mcp` | 16 | 2,839 | 7 | stdio·Streamable-HTTP(SSE)·`mcp__<server>_<tool>` 네이밍 구현. **미구현**: OAuth 흐름, Smithery, 세션 간 도구 캐싱, prompts/resources, 데몬 기반 서버 공유(main `acdfffa248`) |
| `omp-session` | 7 | 3,729 | 34 | JSONL v3(`pa_types::session::FileEntry` + `OmpEntry`). `tree_parity.rs`로 트리 패리티 검증 |
| `omp-settings` | 4 | 4,955 | 8 | `schema.rs` 3,934줄에 대량 설정 선언(메모리 74개 등 다수 dead key 포함) |
| `omp-stats` | 86 | 21,153 | 185 | `@oh-my-pi/omp-stats` 1:1 포트 + Bun 빌드된 React `client/` 번들 서빙 |
| `omp-urls` | 18 | 4,705 | 57 | `agent/artifact/cfg/core/history/local/mcp/omp/rule/skill/xd` 등록. `memory://` 누락, `rule://`는 빈 벡터 반환 |
| `omp-eval` | 9 | 3,205 | 48 | `python3 -u` 외부 인터프리터 + `js.rs` Bun 워커, NDJSON `HostBridge`. 임베디드 CPython 아님 |
| `omp-task` | 11 | 4,052 | 59 | `.omp/agents/*.md` 탐색, `TaskRunner::start` (`--mode rpc` stdin 조향·프로세스 그룹 kill), `outputSchema` 검증, `rubric.rs` |
| `omp-ext` | 10 | 4,336 | 22 | Bun `ext-host` JSON-RPC 클라이언트, `pa-agent` 훅 매핑, ~25종 라이프사이클 이벤트 |
| `omp-wasm` | 5 | 741 | 8 | `wasmi` 기반 샌드박스 플러그인 호스트 (tools/commands 전용, hooks 없음) |
| `omp-auth-broker` | 3 | 2,582 | 8 | `agent.db` 위 REST vault 데몬 (`omp-providers` CAS 리프레시 경로 공유) |
| `omp-collab` | 13 | 12,412 | 127 | `collab` 호스트·게스트·릴레이·복제 축소 전체 이식 + `serve-collab` |
| `omp-sot` | 7 | 2,347 | 33 | State of Thought 컨트롤러(arXiv 2609.16055). 단위 테스트·예제만 존재, CLI 런타임 미연결 |

---

## 2. `/browser`, `/computer`, `omp-eval` 실측

- **`crates/omp-cli/src/features/browser.rs` (495줄)**:
  - `/browser`: 로컬 Chrome/Chromium을 `--remote-debugging-port`로 띄워 `tokio-tungstenite` CDP로 제어하는 **실제 구현**이나, 모델 도구가 아닌 슬래시 커맨드에만 노출되어 있고 `packages/browser-relay` MV3 확장 멀티플렉서는 없다.
  - `/computer`: `grim`, `import`, `gnome-screenshot` 외부 CLI를 호출하는 **단순 데스크톱 스크린샷 래퍼**다. 업스트림 `crates/natives/src/desktop/`(15,753줄)이나 `omp2` `crates/desktop`(11,731줄) 같은 접근성 트리(AX)·입력 자동화는 없다.
- **`crates/omp-cli/src/features/ssh.rs` (763줄)**: `tools/bash.rs`의 원격 실행 대상 관리.
- **`crates/omp-eval/src/runner.py`**: stdlib 전용 최소 포트로 IPython/셀 매직 미지원, 시스템 `python3` 바이너리에 의존한다.

---

## 3. 업스트림 참조 확인 (`sdk/rust/omp-rpc`, `omp2` `omp-e2e`)

- **`/tmp/oh-my-pi-ro/sdk/rust/omp-rpc`**:
  - main 브랜치의 공식 Rust RPC 클라이언트. `rpc-wire.schema.json`에서 `src/wire.rs` 자동 생성(`bun run gen:rpc`).
  - **Protocol v2**: 1 MiB 프레임 분할(`rpc_chunk`), 64 MiB 재조립, `get_messages_page` 256개 단위 페이징, `host_tool_call` / `xd://` 마운트 / `host_uri_request`, 프로세스 그룹 단위 종료 확인.
  - `oh-my-tau`의 `--mode rpc`는 아직 이 v2 청킹·`host_uri`·`xd://` 호스트 도구 표면을 완전히 추적하지 않는다.
- **`/tmp/oh-my-pi-omp2/crates/e2e/tests/`**:
  - 결합 시스템 수용 증명 11종: `P1`(문서 동시성/리베이스), `P2`(CancelTree 취소 경계), `P3`(단일 Job 원형), `P4`(라이브 스키마 격리), `P5`(4대 프롬프트 밴드 캐시 프리픽스), `P6`(크래시/torn-tail 복구), `P7`(실PTY TUI), `P8`(비게이팅 성능 기준선), `P9`(격리 워크트리 + 확장 제어), `P10`(도구 리프트 멱등성), `tool_sources`.
  - 모든 대기에 bounded timeout, 모든 프로세스/소켓에 RAII 소유권을 강제한다.
