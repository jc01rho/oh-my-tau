# 구현 조각 11: `crates/omp-cli/src/tools/bash.rs`·`jobs.rs` 기반 `bash_output`·`kill_bash`·`monitor`(`command`/`path`/`target`) 구현 명세 (impl-11-tools-bash-session-and-monitor)

## 1. 목적 및 코드 근거 (File & Line)

OmOMeow 하네스는 백그라운드 셸을 띄운 뒤 `bash_output`으로 증분 출력을 읽고, `kill_bash`로 회수하며, `monitor`로 `command`/`path`/`target` 조건을 감시한다. tau는 Phase 2에서 `crates/omp-cli/src/tools/shell_host.rs`(프로세스 수명), `crates/omp-cli/src/tools/jobs.rs`(`proc://` 잡 레지스트리), `crates/omp-cli/src/tools/wait.rs`(대기 프리미티브)를 이미 도입했으나 위 3개 도구가 노출되지 않아 격차가 존재한다(근거 요약 5항).

- 디스패치 진입점: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`. 새 명령은 이 `match`에 암을 추가해야 하며, 누락 시 `crates/omp-cli/src/rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`로 떨어진다.
- 응답 프레임 조립: `crates/omp-cli/src/rpc.rs:203-226` `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 만든다. `MoveHost`(`omo-nyang/src/relay/move-host.ts:4-7, 61`)가 `errorCode`를 파싱하므로 실패 시 `CmdError`의 `code`와 `errorCode`를 동시에 직렬화해야 한다.
- 재사용 대상: `jobs.rs`의 `proc://<id>` 키 공간과 `shell_host.rs`의 자식 프로세스 그룹 종료 경로. 신규 프로세스 스포너를 만들지 않는다.
- 도구 표면 등록: `crates/omp-cli/src/tools/mod.rs`의 도구 레지스트리에 `bash_output`, `kill_bash`, `monitor` 3종을 추가하고, `get_protocol_info()`의 `commands:[...]`(`omo-nyang/src/lane-hold.ts:29-76` 계약)에도 반영한다.

## 2. 상세 계약 및 데이터/타입 명세

```rust
// crates/omp-cli/src/tools/bash.rs
pub type BashId = String;                       // "bash_" + 12 hex

pub struct OutputRing {                          // 1 MiB 링 버퍼, seq 기반 커서
    buf: std::collections::VecDeque<u8>,
    pub base_seq: u64, pub next_seq: u64, pub dropped_bytes: u64, cap: usize,
}

pub struct JobHandle {
    pub id: BashId,
    pub cmd: String,
    pub cwd: std::path::PathBuf,
    pub child: tokio::sync::Mutex<tokio::process::Child>,
    pub stdout: Arc<OutputRing>,
    pub stderr: Arc<OutputRing>,
    pub status: Arc<std::sync::atomic::AtomicU8>, // 0 running, 1 exited, 2 killed
    pub exit_code: Arc<std::sync::atomic::AtomicI32>,
    pub started_at: std::time::Instant,
}

pub async fn bash_output(jobs: &JobRegistry, args: &Value) -> CmdResult;
pub async fn kill_bash(jobs: &JobRegistry, args: &Value) -> CmdResult;
pub async fn monitor(jobs: &JobRegistry, args: &Value) -> CmdResult;
```

`bash_output` 요청/응답(JSON):

```json
{"bash_id":"bash_7f3a91c2","after_seq":0,"wait_ms":0,"max_bytes":65536,"filter":null}
{"bash_id":"bash_7f3a91c2","chunks":[{"seq":0,"stream":"stdout","data":"..."}],
 "next_seq":1234,"dropped_bytes":0,"running":true,"exit_code":null,"truncated":false}
```

`kill_bash` 요청/응답:

```json
{"bash_id":"bash_7f3a91c2","signal":"TERM","group":true}
{"bash_id":"bash_7f3a91c2","killed":true,"exit_code":143,"escalated":false}
```

`monitor` 요청/응답 — 조건은 `command` | `path` | `target` 중 정확히 1개:

```json
{"bash_id":"bash_7f3a91c2","timeout_ms":600000,"interval_ms":250,
 "command":{"stream":"both","mode":"regex","pattern":"(listening on|ready in)","count":1}}
{"path":{"path":"target/debug/app","event":"create","glob":null}}
{"target":{"kind":"port","value":8080,"expect":"open"}}
{"matched":true,"condition":"command","evidence":{"seq":918,"line":"ready in 412ms"},
 "elapsed_ms":1830,"timed_out":false}
```

`target.kind`는 `path` | `port` | `process` | `url` 4종으로 고정한다. `condition` 값은 `"command"|"path"|"target"`만 허용한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
// rpc.rs:593 dispatch 내부에 추가
"bash_output" => crate::tools::bash::bash_output(&rt.jobs, cmd).await,
"kill_bash"   => crate::tools::bash::kill_bash(&rt.jobs, cmd).await,
"monitor"     => crate::tools::bash::monitor(&rt.jobs, cmd).await,
```

```rust
// bash_output 핵심: 커서 기반 증분 읽기, seq 역행 거부
let h = jobs.get(&bash_id).ok_or_else(|| CmdError::with_code("bash_not_found", ..))?;
let after = args.get("after_seq").and_then(Value::as_u64).unwrap_or(0);
if after > h.stdout.next_seq { return Err(CmdError::with_code("cursor_ahead", ..)); }
if wait_ms > 0 && h.status.load(Relaxed) == 0 {
    tokio::time::timeout(Duration::from_millis(wait_ms), h.exit.notified()).await.ok();
}
let (chunks, next_seq) = h.stdout.read_from(after, max_bytes); // filter는 라인 단위 적용
```

```rust
// kill_bash: SIGTERM -> 3s 유예 -> SIGKILL, 프로세스 그룹 전체
shell_host::signal_group(&h, Signal::TERM).await?;
if tokio::time::timeout(Duration::from_secs(3), h.exit.notified()).await.is_err() {
    shell_host::signal_group(&h, Signal::KILL).await?; escalated = true;
}
```

```rust
// monitor: 조건 1개 강제 + 폴링/이벤트 혼합
match (has_command, has_path, has_target) {
    (true, false, false) => wait::until_command(&h, &cond, interval, deadline).await,
    (false, true, false) => wait::until_path(&cond, interval, deadline).await,
    (false, false, true) => wait::until_target(&cond, interval, deadline).await,
    _ => return Err(CmdError::with_code("monitor_invalid_condition", ..)),
}
```

`wait.rs`에 `until_command`(링 버퍼 신규 라인 스캔, `count` 도달 시 매치), `until_path`(`notify` 크레이트 워처, `event` 필터), `until_target`(`port`는 TcpStream connect, `process`는 `/proc` 스캔, `url`은 HEAD 요청)을 추가한다. 타임아웃은 오류가 아니라 `{matched:false, timed_out:true}`로 `success:true` 반환한다(하네스가 분기 처리).

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `cargo test -p omp-cli tools::bash::` — 커서 증분, 링 오버플로(`dropped_bytes` 증가), `cursor_ahead` 거부, TERM/KILL 승격, 3조건 monitor 각각 통과.
2. `cargo clippy -p omp-cli -- -D warnings` 및 `cargo fmt --check` 무경고.
3. 실제 RPC 왕복 검증:

```bash
omt --mode rpc --multi-session --session-runtime in-process -e ./ext.js
{"id":"1","command":"exec","args":{"command":"sleep 5 && echo ready","background":true}}
{"id":"2","command":"bash_output","args":{"bash_id":"bash_...","after_seq":0,"wait_ms":6000}}
{"id":"3","command":"monitor","args":{"bash_id":"bash_...","command":{"stream":"both","mode":"contains","pattern":"ready"}}}
{"id":"4","command":"kill_bash","args":{"bash_id":"bash_...","signal":"TERM"}}
```

4. 4번 응답이 `{type:"response", id:"4", success:true, data:{killed:true, exit_code:143}}`이고, 실패 케이스에서 `errorCode`가 `bash_not_found` 등으로 채워질 것.
5. `get_protocol_info()`의 `commands` 배열에 3개 도구명이 포함되고 `protocolVersion:2` 유지.
6. `crates/omp-cli/src/rpc.rs:1094`의 `Unknown command` 경로가 위 3개 명령에 대해 절대 도달하지 않음(단위 테스트로 고정).

## 5. 엣지 케이스 및 실패 복구

- 존재하지 않는 `bash_id`: `CmdError::with_code("bash_not_found")`, `success:false`. 프로세스 재시작 후 잡 레지스트리는 비어 있으므로 하네스는 `bash_output` 실패를 "세션 소실"로 간주하고 재실행해야 한다.
- 이미 종료된 잡에 `bash_output`: 오류가 아니라 `{running:false, exit_code:N}` + 잔여 청크 반환(드레인 후 종료). `kill_bash`는 `{killed:false, exit_code:N}`으로 멱등 처리.
- 링 버퍼 오버플로: `dropped_bytes > 0`과 `truncated:true`를 함께 반환해 유실 구간을 명시. `after_seq`가 `base_seq`보다 작으면 `dropped_bytes`만큼 건너뛰고 경고 필드를 채운다.
- `after_seq` 역행/선행: `cursor_ahead`는 400 계열 코드로 거부, 동일 `after_seq` 재요청은 빈 `chunks`로 성공(중복 소비 방지).
- `monitor` 조건 0개 또는 2개 이상: `monitor_invalid_condition` 즉시 실패. `timeout_ms` 상한(기본 600000, 최대 3600000) 초과는 `monitor_timeout_too_large`.
- 감시 중 대상 잡 종료: `command` 조건은 종료 전까지 스캔 후 `matched:false, reason:"process_exited"` 반환. `path`/`target`은 잡과 무관하게 타임아웃까지 계속 감시.
- `kill_bash` 권한/신호 미지원(예: Windows): `signal_unsupported` 코드로 실패하고 `group:false` 재시도 경로를 안내.
- 취소 전파: RPC 연결 종료 시 `dispatch` future 드롭 → `monitor`는 `Drop`에서 워처 핸들을 해제하고 잡은 유지(백그라운드 셸은 세션 수명에 종속, `
