# 리뷰 조각 03: `--multi-session --session-runtime in-process`에서 전역 상태(`cwd`, 환경변수, 확장 호스트 싱글턴) 오염 위험 감사 (rev-03-multi-session-isolation-audit)

## 1. 목적 및 코드 근거 (File & Line)

`omo-nyang/src/relay/move-host.ts:37-39`는 호스트를 `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 기동한다. 즉 **하나의 OS 프로세스 안에 N개의 세션 런타임이 공존**하며, 프로세스 전역 상태는 세션 간에 구조적으로 공유된다. 본 조각의 목적은 그 공유 지점을 전수 열거하고, 각 지점이 세션 격리를 깨는지 판정하는 감사(audit) 계약과 자동 검증을 정의하는 것이다.

감사 대상 전역 상태와 근거:

- **단일 `Runtime` 공유**: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. `rt: &Arc<Runtime>`가 모든 세션 요청에 동일 인스턴스로 전달되면 세션별 상태(보류 시퀀스, 서브에이전트 목록, 잡 테이블)가 섞인다. `rpc.rs:863`의 `get_subagents`가 빈 배열 스텁인 것도 이 공유 테이블 부재의 방증이다.
- **디스패치 폴백**: `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 신규 감사 커맨드는 반드시 이 폴백 앞에 등록되어야 하며, 폴백 문자열/에러 형태를 변경해서는 안 된다.
- **응답 프레임**: `rpc.rs:203-226` `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립하고, `omo-nyang/src/relay/move-host.ts:4-7, 61`이 동일 스키마를 파싱한다. 실패 시 `code`와 `errorCode`를 **동시에** 직렬화해야 클라이언트가 분기 가능하다.
- **보류/재개 상태**: `omo-nyang/src/lane-hold.ts:29-76` — `set_client_info({width:80, capabilities:["interactive_user"]})` → `interrupt({intent:"stop"|"abort"})` → `get_state({paused, runPaused, pauseSeq})`. `pauseSeq`가 프로세스 전역 카운터면 A 세션의 resume이 B 세션의 hold를 해제한다.
- **확장 호스트 싱글턴**: `omo-nyang/src/session-move-extension.ts:116-139`의 `api.rpc.handle("omomeow.move-readiness"|"omo.feedback.prepare"|"omo.feedback.submit")`는 이름 기반 전역 레지스트리다. `-e <ext>`가 1회 로드되면 핸들러는 특정 세션에 바인딩되지 않는다.
- **이벤트 버스**: `api.events.on("wake_source_state", {source, activeCount})`, `session_move_export`/`session_move_import`(`event={dir, signal}`, `ctx.sessionManager.getSessionFile()`)는 세션 식별자 없이 발행되면 수신측이 오귀속한다.
- **종료 신호 불일치**: `crates/omp-cli/src/runtime.rs:1641`은 stdout으로 `{"type":"session_settled"}`만 보내고 `agent_settled`는 `crates/omp-cli/src/ext.rs:1598`에서 확장 호스트 내부로만 보낸다. 반면 `omo-nyang/src/relay/runtime.ts:59`, `slice-run.ts:71`, `fleet.ts:258`, `agentd-sessions.ts:186`은 stdout의 `agent_settled`를 기다린다. in-process 다중 세션에서는 어느 세션이 settled인지 식별자도 없다.
- **설정 루트 이원화**: `crates/omp-settings/src/lib.rs`는 `~/.omp`, 플릿은 `~/.omo`(`omo.jsonc`, 프로필 `fleet-small`/`fleet-medium`). `--skill <경로>`는 `omo-nyang/src/gateway.ts:1167`이 전달하지만 tau는 `crates/omp-cli/src/cli.rs:129, 309`의 `--skills <CSV필터>`만 갖는다. `Args::skill_paths: Vec<PathBuf>`(cli.rs:97-166)는 프로세스 전역이므로 세션별 스킬 집합이 다르면 오염된다.
- **세션 기록 경로**: `crates/omp-session/src/entries.rs`(JSONL v3, 256B title slot + camelCase entries)와 `omo-nyang/src/session-patterns.ts:47-76`의 파서 계약. cwd 상대 경로로 쓰면 두 세션이 같은 파일에 append한다.

## 2. 상세 계약 및 데이터/타입 명세

신규 RPC 커맨드 `get_isolation_report`를 추가하고, 다음 타입을 `crates/omp-cli/src/isolation.rs`에 둔다.

```rust
pub struct SessionScopeId(pub String);            // "st_..." 형태, 세션 런타임 ID

pub struct SessionScopeSnapshot {
    pub session_id: String,
    pub cwd: PathBuf,                              // 세션 논리 cwd (프로세스 cwd와 무관)
    pub env_overlay_keys: Vec<String>,
