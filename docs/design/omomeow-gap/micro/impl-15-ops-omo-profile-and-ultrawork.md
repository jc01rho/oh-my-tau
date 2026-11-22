# 구현 조각 15: `crates/omp-settings`의 `~/.omo`(`omo.jsonc` 프로필) 폴백 로더 및 `ultrawork`·`ulw-loop` 시스템 프롬프트 훅 구현 명세 (impl-15-ops-omo-profile-and-ultrawork)

## 1. 목적 및 코드 근거 (File & Line)

- tau의 설정 루트는 `~/.omp`(`crates/omp-settings/src/lib.rs`)이나, OmO 하네스는 `~/.omo/omo.jsonc`의 프로필(`fleet-small`, `fleet-medium` 등)을 사용한다. 본 조각은 `~/.omp` 우선, `~/.omo` 폴백 로더와 프로필 선택을 추가한다.
- CLI 격차: `omo-nyang/src/gateway.ts:1167`은 `--skill <디렉터리경로>`를 전달하지만 `crates/omp-cli/src/cli.rs:129,309`에는 `--skills <CSV필터>`만 있다. `--skill`을 반복 가능한 `Args::skill_paths: Vec<PathBuf>`로 추가한다.
- `omo-nyang/src/model-routing.ts:654-658`은 `--no-model-fallback`, `--no-recommended-models`를 전달하므로 동일 플래그를 `cli.rs:97-166`에 추가한다.
- `ultrawork`/`ulw-loop` 부트스트랩 및 내장 스킬 디렉터리 탐색이 OmO 하네스 요구사항이나 tau에는 대응 훅이 없다. 시스템 프롬프트 조립 단계에 훅 레지스트리를 신설한다.
- 재로딩 계약: `omo-nyang/src/lane-hold.ts:29-76`의 `reload()`는 설정·스킬·확장 재로딩 후 `{cancelled:false}`를 반환하고, `resume_session()`은 `paused:false, runPaused:false`를 보장한다. 따라서 프로필/훅 재구성은 `reload()` 경로에 연결하되 보류 상태를 변형하지 않는다.
- RPC 표면: `crates/omp-cli/src/rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`, 미지원 명령은 `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 새 `profile_info`는 이 catch-all 앞에 매치
