# QA 시나리오 10: 단일 워커 장비(`instabox-dev26`) `omt` 카나리 배치, 스모크 작업 수행, 이상 시 `mhc` 즉시 롤백 검증 (qa-10-canary-worker-rollout-and-rollback)

## 1. 목적 및 코드 근거 (File & Line)

- 대상: 단일 워커 `instabox-dev26`의 1슬롯에 신규 `omt` 바이너리를 카나리로 배치하고, 계약 스모크 통과 시 승격, 임의 체크 실패 시 `mhc`로 직전 리비전으로 즉시 롤백한다.
- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령: `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 카나리 사전 점검은 이 문자열을 "구버전 바이너리" 신호로 판정한다.
- 응답 프레임: `crates/omp-cli/src/rpc.rs:203-226` `response()`. 소비자 `omo-nyang/src/relay/move-host.ts:4-7,61`가 `{type:"response", id, success, data?, error?, errorCode?}`를 파싱하므로 실패 시 `code`와 `errorCode`를 함께 직렬화해야 한다.
- 보류/재개 계약: `omo-nyang/src/lane-hold.ts:29-76`. `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `interactive_user` 미선언 시 `stop_requires_interactive_client`, 선언 후 역할 변경 시 `connection_role_fixed`. `resume_session()`은 `session_resumed` 이벤트 후 `paused:false, runPaused:false`.
- 프로토콜 정보: `get_protocol_info()` → `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`. `reload()` → `{cancelled:false}`.
- 세션 이동 계약: `omo-nyang/src/relay/move-host.ts:37-39`, `move-operations.ts:104-225`, `session-move-extension.ts:116-139`. 호스트 실행 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`; `move_session` op=`prepare|export|import|status|finish|cancel`; `release_session({sessionId, reason:"move", moveId})` → `{session_path}`; 확장 이벤트 `session_move_export`/`session_move_import`(`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})`.
- 종료 신호 격차: `crates/omp-cli/src/runtime.rs:1641`은 stdout에 `{"type":"session_settled"}`만 쓰고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 스모크 정착 판정은 두 신호를 모두 수용해야 한다.
- 설정/세션 루트: `crates/omp-settings/src/lib.rs`의 `~/.omp` vs 플릿 `~/.omo`(`omo.jsonc`), 세션 기록 `crates/omp-session/src/entries.rs`(JSONL v3, 256B title slot) 및 파서 `omo-nyang/src/session-patterns.ts:47-76`.
- CLI 플래그: `omo-nyang/src/gateway.ts:1167`(반복 가능한 `--skill <dir>`), `omo-nyang/src/model-routing.ts:654-658`(`--no-model-fallback`, `--no-recommended-models`), `crates/omp-cli/src/cli.rs:97-166`(`Args::skill_paths: Vec<PathBuf>` 신설 지점).

## 2. 상세 계약 및 데이터/타입 명세

```ts
// omo-nyang/src/qa/canary.ts (신규)
export type CanaryPhase = "preflight" | "deploy" | "smoke" | "verify" | "promote" | "rollback";

export interface CanaryPlan {
  id: "qa-10-canary-worker-rollout-and-rollback";
  worker: "instabox-dev26";
  slot: 0;                       // 단일 슬롯 장비
  binPath: string;               // /usr/local/bin/omt
  baselineVersion: string;       // 승격 전 serverVersion
  omoHome: string;               // ~/.omo
  rpc: { mode: "rpc"; multiSession: true; sessionRuntime: "in-process"; ext: string[] };
  smoke: { timeoutMs: number; cwd: string; bundleDir: string; moveId: string };
  rollback: { tool: "mhc"; args: string[]; timeoutMs: number };
}

export interface CanaryCheck { name: string; ok: boolean; detail?: string; code?: string; errorCode?: string; }
export interface CanaryResult {
  phase: CanaryPhase; ok: boolean; candidateVersion?: string;
  checks: CanaryCheck[]; rolledBack: boolean;
}
```

- RPC 요청/응답: 요청은 `{type:"request", id, command, ...args}`, 응답은 `rpc.rs:203-226` 스키마 고정. 실패 프레임 예: `{"type":"response","id":7,"success":false,"error":"Unknown command: move_session","code":"unknown_command","errorCode":"unknown_command"}`.
- `mhc` 계약(본 조각에서 정의, 준수 테스트 필수): `mhc rollback --worker instabox-dev26 --slot 0 --to <bas
