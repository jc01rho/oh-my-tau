# 문서 조각 07: OmOMeow 워커에서 `omoExecutable`을 `mhc`에서 `omt`로 카나리 전환·검증·롤백하는 운영 런북 (doc-07-operator-canary-runbook)

## 1. 목적 및 코드 근거 (File & Line)

OmOMeow 워커가 spawn하는 에이전트 호스트 실행 파일(`omoExecutable`)을 기존 `mhc`에서 `omt`(oh-my-tau)로 전환하기 전에, 카나리 워커 1대에서 RPC 계약 호환성을 실측 검증하고, 실패 시 `mhc`로 원복한다. 검증 대상 계약은 아래 실측 지점에 1:1로 대응한다.

| 영역 | 파일:줄 | 확인 내용 |
|---|---|---|
| RPC 디스패치 | `crates/omp-cli/src/rpc.rs:593` | `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` |
| 미지원 명령 | `crates/omp-cli/src/rpc.rs:1094` | `other => Err(CmdError::new(format!("Unknown command: {other}")))` |
| 응답 프레임 조립 | `crates/omp-cli/src/rpc.rs:203-226` | `response()`가 `{type,id,success,data?,error?,errorCode?}` 직렬화 |
| 프레임 파싱(소비자) | `omo-nyang/src/relay/move-host.ts:4-7, 61` | `MoveHost`가 `success`/`error`/`errorCode`를 파싱 |
| 보류·재개 계약 | `omo-nyang/src/lane-hold.ts:29-76` | `set_client_info`→`interrupt`→`get_state`, `resume_session` |
| 세션 이동 spawn | `omo-nyang/src/relay/move-host.ts:37-39` | `--mode rpc --multi-session --session-runtime in-process -e <ext>` |
| 이동 오퍼레이션 | `move-operations.ts:104-225`, `session-move-extension.ts:116-139` | `move_session`, `release_session`, `extension_request` |
| 스킬 플래그 불일치 | `omo-nyang/src/gateway.ts:1167` vs `crates/omp-cli/src/cli.rs:129,309` | `--skill <dir>` 전달 vs `--skills <CSV>` 만 존재 |
| 모델 라우팅 플래그 | `omo-nyang/src/model-routing.ts:654-658` | `--no-model-fallback`, `--no-recommended-models` |
| 종료 신호 불일치 | `crates/omp-cli/src/runtime.rs:1641` vs `ext.rs:1598`, `omo-nyang/src/relay/runtime.ts:59` | tau는 `session_settled`만 stdout, 소비자는 `agent_settled` 대기 |
| 설정 루트 | `crates/omp-settings/src/lib.rs` vs `omo-nyang` `~/.omo/omo.jsonc` | `~/.omp` vs `~/.omo` 분리 |

## 2. 상세 계약 및 데이터/타입 명세

**2.1 응답 프레임 (필수 직렬화 형태)**
```json
{"type":"response","id":"1","success":true,"data":{"protocolVersion":2,"serverVersion":"<env!(CARGO_PKG_VERSION)>","engine":"oh-my-tau","commands":["get_protocol_info","set_client_info","interrupt","get_state","reload","resume_session","move_session","release_session","extension_request"]}}
{"type":"response","id":"7","success":false,"error":"Unknown command: bash_output","errorCode":"unknown_command"}
```
`MoveHost`는 `success===false`일 때 `errorCode`가 없으면 원인 분류가 불가하므로, 카나리 게이트에서 `errorCode` 존재를 별도 검사한다(`rpc.rs:203-226` 계약).

**2.2 보류·재개 순서 (`lane-hold.ts:29-76`)** — 순서 고정, 위반 시 게이트 실패:
1. `set_client_info({width:80, capabilities:["interactive_user"]})`
2. `interrupt({intent:"stop"})` 또는 `{intent:"abort"}`
3. `get_state()` → `{paused, runPaused, pauseSeq}` 에서 `paused===true` 확인
4. `reload()` → `{cancelled:false}`
5. `resume_session()` → `session_resumed` 이벤트 발행, `get_state()`에서 `paused===false && runPaused===false`

**2.3 세션 이동 (`move-operations.ts:104-225`)**
- `move_session` `op ∈ {"prepare","export","import","status","finish","cancel"}`, 인자 `{moveId, sessionPath, bundleDir, cwd, from, answer}`.
- `release_session({sessionId, reason:"move", moveId})` → `{session_path}`.
- `extension_request({sessionId?, name, data?})` → `api.rpc.handle(name, handler)`; 카나리에서 `omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit` 3종 라우팅 확인.
- 확장 이벤트: `session_move_export` / `session_move_import` (`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`), `api.events.on("wake_source_state", {source, activeCount})`.

**2.4 종료 신호 브리지 계약** — 카나리 기간 동안 `session_settled`(`runtime.rs:1641`)를 `agent_settled`로 승격하는 어댑터를 워커 측에 둔다(3.4절). 이 어댑터 없이는 워커가 영구 대기한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드)

**3.1 프로파일 전환 (카나리 1대 한정)**
```jsonc
// ~/.omo/omo.jsonc  → 프로파일 canary-tau (fleet-small 상속)
{ "profile": "canary-tau", "omoExecutable": "omt",
  "agentArgs": ["--mode","rpc","--multi-session","--session-runtime","in-process",
                "-e","<abs>/session-move-extension.ts",
                "--no-model-fallback","--no-recommended-models"] }
```
`--skill`이 `cli.rs:129,309`에 없으므로, 카나리에서는 `Args::skill_paths: Vec<PathBuf>`(반복 가능) 구현 여부를 preflight로 판정하고 미구현이면 해당 워커의 `--skill` 주입을 비활성(스킬 로딩 스킵)으로 낮춘다.

**3.2 Preflight (플래그 프로브)**
```bash
omt --help | grep -E -- '--skill\b|--skills|--multi-session|--session-runtime|--no-model-fallback'
omt --version   # env!("CARGO_PKG_VERSION") 와 대조
```

**3.3 핸드셰이크 게이트 (의사코드)**
```
send {"id":"1","command":"get_protocol_info"}
assert resp.type=="response" && resp.id=="1" && resp.success==true
assert resp.data.protocolVersion==2 && resp.data.engine=="oh-my-tau"
REQUIRED = {set_client_info,interrupt,get_state,reload,resume_session,
            move_session,release_session,extension_request}
missing = REQUIRED - set(resp.data.commands); assert missing.isEmpty()
send {"command":"set_client_info","params":{width:80,capabilities:["interactive_user"]}}
send {"command":"move_session","params":{op:"prepare",moveId:canaryId,sessionPath:p,cwd:wd}}
send {"command":"move_session","params":{op:"status",moveId:canaryId}}  # 상태 회귀 확인
send {"command":"move_session","params":{op:"cancel",moveId:canaryId}}
send {"command":"extension_request","params":{name:"omomeow.move-readiness",data:{}}}
# 실패 프레임이면 errorCode 필드 존재까지 assert (rpc.rs:203-226)
```
`get_subagents`(`rpc.rs:863`)는 빈 배열 스텁이므로 카나리 기대값을 `[]`로 고정하고 회귀 아님으로 처리한다.

**3.4 종료 신호 어댑터 (TS, 워커 측)**
```ts
// omo-nyang/src/relay/runtime.ts:59 대기 지점 앞단
onStdoutLine(line) {
  const f = JSON.parse(line);
  if (f.type === "session_settled") { emit({ type: "agent_settled" }); return; }
  handle(f);
}
```

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `get_protocol_info` 응답이 `protocolVersion=2`, `engine="oh-my-tau"`, `serverVersion`이 `omt --version`과 일치.
2. `REQUIRED` 8개 명령이 `commands[]`에 모두 존재하고, 미지원 호출 시 `success=false` + `errorCode` 비어있지 않음.
3. `set_client_info`→`interrupt`→`get_state(paused=true)`→`reload({cancelled:false})`→`resume_session`→`get_state(paused=false, runPaused=false)` 전 구간 통과.
4. `move_session` prepare/status/cancel 왕복 후 `release_session`이 `{session_path}` 반환, `bundleDir` 잔여물 0건.
5. `extension_request` 3종(`omomeow.move-readiness`, `omo.feedback.prepare`, `omo.feedback.submit`) 모두 라우팅 성공.
6. 카나리 워커 1대에서 24시간 무재시작, `agent_settled` 수신으로 태스크 종료 확인.
7. 검증 명령:
```bash
omo worker run --profile canary-tau --task "noop" --json | jq -e '.type=="agent_settled"'
ls "$BUNDLE_DIR" | wc -l          # 0
grep -c '"type":"session_settled"' canary.stdout.log   # >= 1
```

## 5. 엣지 케이스 및 실패 복구

- **`Unknown command` (`rpc.rs:1094`)**: 카나리 중 `bash_output`, `kill_bash`, `task_send`, `task_output`, `task_cancel`, `memory*`, `workpool*`, `ask_user_question`, `create_goal` 호출이 오면 즉시 게이트 실패. 해당 태스크는 `mhc` 워커로 라우팅하고 기능 플래그로 차단한다(도구 격차 5절).
- **`stop_requires_interactive_client`**: `capabilities:["interactive_user"]` 선언 누락. 재선언 후 재시도하며, 선언 후 역할 변경 시도 시 `connection_role_fixed`가 나오므로 세션당 1회만 선언한다.
- **종료 신호 미수신**: 3.4 어댑터 미적용 시 워커가 hang. 60초 타임아웃 시 stdout 마지막 프레임을 로그에 남기고 롤백 트리거.
- **`--skill` 미지원**: `gateway.ts:1167`이 `--skill <dir>`을 보내면 tau가 인자 파싱 실패로 즉시 종료할 수 있음. preflight 실패 시 스킬 주입 제거 후 재기동, 또는 `Args::skill_paths` 구현 머지 전까지 카나리 보류.
- **설정 루트 혼선**: `~/.omo/omo.jsonc`의 `omoExecutable`만 변경하고 `~/.omp`(설정·스킬·확장)는 건드리지 않는다. 롤백은 `omoExecutable: "mhc"`로 1줄 원복 + 워커 재시작.
- **세션 기록 파싱**: tau는 omp JSON
