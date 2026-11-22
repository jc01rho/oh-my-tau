# QA 시나리오 04: `--no-model-fallback`·`--no-recommended-models` 적용 상태에서 모델 거절 시 게이트웨이 `ModelRecovery` 체인 전환 검증 (qa-04-model-pinning-and-recovery)

## 1. 목적 및 코드 근거 (File & Line)

게이트웨이가 자식 tau 프로세스에 `--no-model-fallback`, `--no-recommended-models`를 넘기는데(`omo-nyang/src/model-routing.ts:654-658`) tau CLI에는 해당 인자가 없어 clap 단계에서 기동이 실패한다. 본 조각은 (a) 두 플래그를 실제 `Args` 필드로 수용하고, (b) 모델 거절 시 `ModelRecovery` 체인이 "허용된 전환만" 수행하도록 고정하며, (c) 전환/차단/소진의 모든 판정을 RPC 프레임으로 관측 가능하게 만든다.

| 근거 | 파일:줄 | 실측 내용 | 본 조각에서의 사용 |
|---|---|---|---|
| 플래그 전달 | `omo-nyang/src/model-routing.ts:654-658` | `--no-model-fallback`, `--no-recommended-models`를 argv로 전달 | QA 입력 계약의 원천 |
| argv 조립 | `omo-nyang/src/gateway.ts:1167` | `--skill <디렉터리경로>` 반복 전달, 같은 지점에서 모델 플래그 조립 | argv 스냅샷 검증 대상 |
| CLI 정의 | `crates/omp-cli/src/cli.rs:97-166` | `Args` 구조체. 두 플래그 부재 | 필드 추가 위치 |
| 기존 유사 인자 | `crates/omp-cli/src/cli.rs:129,309` | `--skills <CSV필터>`만 존재(반복 경로 인자 아님) | 신규 인자 명명 규칙 참조 |
| RPC 진입 | `crates/omp-cli/src/rpc.rs:593` | `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult` | `model_policy` 명령 라우팅 추가 |
| 응답 조립 | `crates/omp-cli/src/rpc.rs:203-226` | `response()`가 `{type:"response", id, success, data?, error?, errorCode?}` 조립 | 실패 시 `code`+`errorCode` 동시 직렬화 |
| 미등록 명령 | `crates/omp-cli/src/rpc.rs:1094` | `other => Err(CmdError::new(format!("Unknown command: {other}")))` | 명령 미등록 시 QA 오탐 방지 |
| 클라이언트 파싱 | `omo-nyang/src/relay/move-host.ts:4-7, 61` | `{type,id,success,data?,error?,errorCode?}` 파싱 | `success` 필드 누락 금지 |
| 종료 동기화 | `omo-nyang/src/relay/runtime.ts:59`, `crates/omp-cli/src/runtime.rs:1641` | 게이트웨이는 `agent_settled` 대기, tau는 `session_settled`만 stdout 출력 | QA는 `session_settled` 기준 + 타임아웃 가드 |
| 설정 루트 | `crates/omp-settings/src/lib.rs` | `~/.omp` | 플래그보다 낮은 우선순위 소스 |

## 2. 상세 계약 및 데이터/타입 명세

**Rust CLI (`crates/omp-cli/src/cli.rs`, `Args` 내부)**

```rust
#[arg(long = "no-model-fallback", default_value_t = false)]
pub no_model_fallback: bool,          // 폴백 체인 전체 비활성
#[arg(long = "no-recommended-models", default_value_t = false)]
pub no_recommended_models: bool,      // 추천 모델 후보만 제외
```

**정책 해석 (우선순위: CLI > `~/.omp` 설정 > 기본값)**

```rust
pub struct ModelPolicy {
    pub pinned: Option<String
