# 문서 조각 01: `docs/parity/gateway-rpc.md` 신설용 게이트웨이 RPC 8종·플래그 5종·확장 API 3종 패리티 원장 표 (doc-01-parity-ledger-gateway-rpc)

## 1. 목적 및 코드 근거 (File & Line)

`docs/parity/gateway-rpc.md`를 신설하여, OmO 하네스(`omo-nyang`)가 oh-my-tau에 대해 실제로 호출하는 게이트웨이 표면을 **RPC 8종 / CLI 플래그 5종 / 확장 API 3종**으로 고정하고, 각 항목의 tau 측 구현 상태(구현/스텁/부재)를 파일:줄 근거와 함께 원장으로 기록한다. 이 문서는 이후 조각(doc-02 이후)의 상위 색인 역할을 하며, "무엇이 이미 있고 무엇이 없는가"를 재조사 없이 판정 가능하게 만든다.

| 영역 | 근거 파일:줄 | 현재 실측 상태 |
|---|---|---|
| RPC 디스패치 진입 | `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` | 존재. 명령 문자열 매칭 구조 |
| 미지원 명령 처리 | `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))` | code 없이 message만 반환 → 계약 위반 |
| 응답 프레임 조립 | `crates/omp-cli/src/rpc.rs:203-226` `response()` | `{type:"response", id, success, data?, error?, errorCode?}` |
| 소비자 파싱 | `omo-nyang/src/relay/move-host.ts:4-7, 61` | 실패 시 `errorCode` 부재를 별도 분기로 처리 |
| 보류·재개 계약 | `omo-nyang/src/lane-hold.ts:29-76` | `set_client_info`→`interrupt`→`get_state`, `resume_session` |
| 세션 이동 | `omo-nyang/src/relay/move-host.ts:37-39`, `move-operations.ts:104-225`, `session-move-extension.ts:116-139` | `move_session`, `release_session`, `extension_request` |
| CLI 플래그 | `omo-nyang/src/gateway.ts:1167`, `model-routing.ts:654-658`, `crates/omp-cli/src/cli.rs:97-166` | `--skill` 부재(129, 309는 `--skills` CSV 필터만) |
| 종료 신호 | `crates/omp-cli/src/runtime.rs:1641` / `crates/omp-cli/src/ext.rs:1598` | stdout은 `session_settled`만, `agent_settled`는 확장 호스트 내부 한정 |
| 서브에이전트 스텁 | `crates/omp-cli/src/rpc.rs:863` `get_subagents` | 빈 배열 반환 스텁 |

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 RPC 8종 원장 (요구 계약)

| # | 명령 | 요청 페이로드 | 성공 응답 `data` | 실패 `code` / `errorCode` |
|---|---|---|---|---|
| 1 | `get_protocol_info` | `{}` | `{protocolVersion:2, serverVersion:env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}` | `unknown_command` |
| 2 | `set_client_info` | `{width:80, capabilities:["interactive_user"]}` | `{ok:true}` | `connection_role_fixed` |
| 3 | `interrupt` | `{intent:"stop"\|"abort"}` | `{ok:true}` | `stop_requires_interactive_client` |
| 4 | `get_state` | `{}` | `{paused:bool, runPaused:bool, pauseSeq:u64}` | - |
| 5 | `reload` | `{}` | `{cancelled:false}` (설정·스킬·확장 재로딩 완료 후) | `reload_failed` |
| 6 | `resume_session` | `{}` | `{paused:false, runPaused:false}` + `session_resumed` 이벤트 발행 | `no_paused_session` |
| 7 | `release_session` | `{sessionId, reason:"move", moveId}` | `{session_path}` | `session_not_found` |
| 8 | `extension_request` | `{sessionId?, name, data?}` | 핸들러 반환값 그대로 (`api.rpc.handle(name, handler)` 라우팅) | `unknown_extension`, `extension_error` |

`move_session`은 8종에 포함하지 않고 `extension_request` 경유 이동 경로의 하위 op 집합으로 문서 내 별도 표에 기록한다: `op="prepare"|"export"|"import"|"status"|"finish"|"cancel"`, 필드 `{moveId, sessionPath, bundleDir, cwd, from, answer}` (`move-operations.ts:104-225`).

### 2.2 플래그 5종

| # | 플래그 | 타입 | 의미 | 근거 |
|---|---|---|---|---|
| 1 | `--skill <PATH>` (반복 가능) | `Args::skill_paths: Vec<PathBuf>` | 추가 스킬 루트 디렉터리. tau의 `--skills`(CSV 필터, `cli.rs:129,309`)와 **직교** | `gateway.ts:1167` |
| 2 | `--multi-session` | `bool` | 다중 세션 호스트 모드 | `move-host.ts:37` |
| 3 | `--session-runtime <MODE>` | `in-process` 허용 | 세션 런타임 선택 | `move-host.ts:37` |
| 4 | `--no-model-fallback` | `bool` | 폴백 체인 비활성 | `model-routing.ts:658` |
| 5 | `--no-recommended-models` | `bool` | 추천 모델 주입 비활성 | `model-routing.ts:658` |

호스트 실행 형태(불변): `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`.

### 2.3 확장 API 3종

| # | API | 시그니처/이벤트 | 근거 |
|---|---|---|---|
| 1 | RPC 라우팅 | `api.rpc.handle(name, handler)` — 이름 예: `"omomeow.move-readiness"`, `"omo.feedback.prepare"`, `"omo.feedback.submit"` | `session-move-extension.ts:116-139` |
| 2 | 확장 이벤트 | `api.events.on("wake_source_state", {source, activeCount})` | `session-move-extension.ts` |
| 3 | 세션 이동 훅 | `session_move_export` / `session_move_import`, `event={dir, signal}`, `ctx.sessionManager.getSessionFile()` | `session-move-extension.ts` |

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// crates/omp-cli/src/rpc.rs — dispatch() 내 신규 arm (rpc.rs:593 블록)
"get_protocol_info" => Ok(CmdResult::ok(json!({
    "protocolVersion": 2,
    "serverVersion": env!("CARGO_PKG_VERSION"),
    "engine": "oh-my-tau",
    "commands": COMMANDS, // 정적 슬라이스, 테스트에서 8종 일치 단언
}))),
"release_session" => {
    let p: ReleaseSession = serde_json::from_value(cmd.clone())
        .map_err(|e| CmdError::with_code("invalid_params", "release_session", e))?;
    let path = rt.sessions.release(&p.session_id, &p.reason, &p.move_id)?;
    Ok(CmdResult::ok(json!({ "session_path": path })))
}
// rpc.rs:1094 교체: 코드 병기 필수
other => Err(CmdError::with_code("unknown_command", "unknown_command",
                                  format!("Unknown command: {other}"))),
```

```rust
// crates/omp-cli/src/cli.rs — Args 확장 (97-166 범위)
#[arg(long = "skill", value_name = "PATH", action = ArgAction::Append)]
pub skill_paths: Vec<PathBuf>,
#[arg(long = "multi-session", default_value_t = false)]
pub multi_session: bool,
#[arg(long = "session-runtime", value_name = "MODE")]
pub session_runtime: Option<String>, // "in-process" 검증
#[arg(long = "no-model-fallback", default_value_t = false)]
pub no_model_fallback: bool,
#[arg(long = "no-recommended-models", default_value_t = false)]
pub no_recommended_models: bool,
```

```rust
// runtime.rs:1641 — 종료 프레임 이중 발행
emit(json!({"type":"session_settled"}));
emit(json!({"type":"agent_settled"})); // relay/runtime.ts:59, slice-run.ts:71, fleet.ts:258, agentd-sessions.ts:186 대응
```

```ts
// omo-nyang/src/relay/move-host.ts:61 — 실패 분기 강화
if (!msg.success) {
  throw new GatewayError(msg.errorCode ?? msg.code ?? "unknown_error", msg.error);
}
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `docs/parity/gateway-rpc.md` 존재, 3개 표(RPC 8행 / 플래그 5행 / 확장 3행) 포함, 총 70줄 이상.
2. 모든 표 행에 `파일:줄` 근거 1개 이상 기재(정규식 `\.[a-z]+:[0-9]+` 매칭).
3. `dispatch`가 8종을 모두 인식하고 미지원 명령은 `code` + `errorCode`를 함께 직렬화.
4. `get_protocol_info.commands` 길이 8, `engine=="oh-my-tau"`, `protocolVersion==2`.
5. `--skill` 2회 반복 시 두 경로가 모두 스킬 루트로 등록되고 `--skills` CSV 필터와 독립 동작.
6. `agent_settled`가 RPC stdout에서 관측됨.

```bash
cargo build -p omp-cli && cargo test -p omp-cli rpc_parity
cargo test -p omp-cli cli_skill_paths_are_appendable
printf '%s\n' '{"id":1,"cmd":"get_protocol_info"}' | omt --mode rpc --multi-session --session-runtime in-process -e ./ext.js
grep -cE '^#|\|' docs/parity/gateway-rpc.md
grep -n 'agent_settled' crates/omp-cli/src/runtime.rs
```

## 5. 엣지 케이스 및 실패 복구

- **capabilities 미선언 후 `interrupt`**: `stop_requires_interactive_client` 반환. `set_client_info` 선행 없이는 `intent:"stop"` 불가 — 원장 표에 "선행조건" 열로 명시.
- **역할 변경 시도**: `set_client_info` 재호출로 `interactive_user`를 제거하면 `connection_role_fixed`. 재연결(새 세션) 외 복구 경로 없음을 문서화.
- **`resume
