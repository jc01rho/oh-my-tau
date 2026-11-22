# 테스트 조각 06: `--skill <경로>` 다중 지정, `--no-model-fallback`, `--no-recommended-models`, `--multi-session`, `--session-runtime in-process` 파서 테스트 명세 (test-06-unit-cli-flags-parser)

## 1. 목적 및 코드 근거 (File & Line)

OmO 하네스(omo-nyang)는 호스트 프로세스를 띄울 때 tau CLI에 아래 인자를 그대로 전달한다. 현재 tau 파서는 이 중 상당수를 알지 못하므로, 파서 단위에서 계약을 고정한다.

- `omo-nyang/src/gateway.ts:1167` — `--skill <디렉터리경로>`를 반복 전달. tau의 `crates/omp-cli/src/cli.rs:129`에는 `--skills <CSV필터>`만 존재하고, `cli.rs:309`의 해당 필드는 스킬 이름 필터다. 즉 경로를 받는 필드가 없다.
- `omo-nyang/src/model-routing.ts:654-658` — `--no-model-fallback`, `--no-recommended-models` 전달.
- `omo-nyang/src/relay/move-host.ts:37` — `omt --mode rpc --multi-session --session-runtime in-process -e <ext>` 형태로 스폰.
- `crates/omp-cli/src/cli.rs:97-166` — 최상위 `Args` 구조체. 여기에 필드를 추가하되, 기존 `--skills`(CSV 필터)의 의미를 절대 변경하지 않는다.
- 실패 경로 근거: 파서가 미지 인자를 거부하면 프로세스는 clap 종료코드 2로 즉시 죽고, `move-host.ts:4-7,61`의 `MoveHost`는 `{type:"response", ...}` 프레임을 한 번도 받지 못해 타임아웃으로 실패한다. 따라서 "조용히 무시되지 않음"과 "정확히 파싱됨" 양쪽을 테스트로 고정한다.

이 조각은 기능 구현이 아니라 **파서 계약 테스트**이며, 산출물은 `crates/omp-cli/tests/cli_flags.rs`(통합 테스트) 1개 파일과 `Args` 필드 추가에 대한 최소 패치다.

## 2. 상세 계약 및 데이터/타입 명세

`crates/omp-cli/src/cli.rs`의 `Args`에 다음을 추가한다(기존 필드 유지).

```rust
#[derive(Debug, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum SessionRuntime { InProcess, Subprocess }

// Args 내부
/// 추가 스킬 루트 경로. 반복 지정 가능. 순서 보존, 중복 제거 없음.
#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
pub skill_paths: Vec<std::path::PathBuf>,

#[arg(long = "no-model-fallback", action = clap::ArgAction::SetTrue, default_value_t = false)]
pub no_model_fallback: bool,

#[arg(long = "no-recommended-models", action = clap::ArgAction::SetTrue, default_value_t = false)]
pub no_recommended_models: bool,

#[arg(long = "multi-session", action = clap::ArgAction::SetTrue, default_value_t = false)]
pub multi_session: bool,

#[arg(long = "session-runtime", value_enum, value_name = "MODE")]
pub session_runtime: Option<SessionRuntime>,
```

계약:

1. `--skill`은 **경로**이며 `Vec<PathBuf>`로 수집된다. `--skills`(CSV 이름 필터, `cli.rs:129`)와 이름이 다르므로 clap의 접두사 충돌이 없어야 한다. 두 옵션은 동시 사용 가능하다.
2. `--skill`은 0회, 1회, N회 모두 유효하다. 순서는 argv 순서와 동일하게 보존된다. 파서 단계에서 존재 여부·디렉터리 여부를 검사하지 않는다(검증은 스킬 로딩 단계 책임).
3. `--no-model-fallback`, `--no-recommended-models`, `--multi-session`은 값 없는 불리언이다. 부재 시 `false`, 존재 시 `true`.
4. `--session-runtime`은 `value_enum`이며 허용 리터럴은 `in-process`, `subprocess`(kebab-case)뿐이다. 부재 시 `None`(런타임 기본값 결정은 호출부 책임).
5. `move-host.ts:37`의 argv 벡터 `["--mode","rpc","--multi-session","--session-runtime","in-process","-e","<ext>"]`는 **에러 없이** 파싱되어야 한다.
6. `gateway.ts:1167` + `model-routing.ts:658`의 argv 벡터 `["--skill","/a/skills","--skill","/b/skills","--no-model-fallback","--no-recommended-models"]`는 `skill_paths == [PathBuf::from("/a/skills"), PathBuf::from("/b/skills")]`, 두 불리언 모두 `true`여야 한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

테스트는 clap의 `try_parse_from`만 사용한다(프로세스 스폰 없음, 네트워크 없음).

```rust
use clap::Parser;
use omp_cli::cli::{Args, SessionRuntime};

fn parse(argv: &[&str]) -> Args {
    let mut v = vec!["omt"];
    v.extend_from_slice(argv);
    Args::try_parse_from(v).expect("argv must parse")
}

#[test]
fn skill_repeated_preserves_order() {
    let a = parse(&["--skill", "/a", "--skill", "/b", "--skill", "/a"]);
    assert_eq!(a.skill_paths.len(), 3);
    assert_eq!(a.skill_paths[2], std::path::PathBuf::from("/a"));
}

#[test]
fn skill_and_skills_coexist() {
    let a = parse(&["--skills", "review,test", "--skill", "/x"]);
    assert_eq!(a.skill_paths, vec![std::path::PathBuf::from("/x")]);
}

#[test]
fn boolean_flags_default_false_and_set_true() {
    let d = parse(&[]);
    assert!(!d.no_model_fallback && !d.no_recommended_models && !d.multi_session);
    let s = parse(&["--no-model-fallback", "--no-recommended-models", "--multi-session"]);
    assert!(s.no_model_fallback && s.no_recommended_models && s.multi_session);
}

#[test]
fn session_runtime_enum() {
    assert_eq!(parse(&["--session-runtime", "in-process"]).session_runtime,
               Some(SessionRuntime::InProcess));
    assert_eq!(parse(&[]).session_runtime, None);
}

#[test]
fn move_host_argv_matches() {           // move-host.ts:37 고정
    let a = parse(&["--mode","rpc","--multi-session","--session-runtime","in-process","-e","/tmp/ext.ts"]);
    assert!(a.multi_session);
    assert_eq!(a.session_runtime, Some(SessionRuntime::InProcess));
}

#[test]
fn gateway_argv_matches() {             // gateway.ts:1167 + model-routing.ts:658 고정
    let a = parse(&["--skill","/a/skills","--skill","/b/skills",
                    "--no-model-fallback","--no-recommended-models"]);
    assert_eq!(a.skill_paths.len(), 2);
    assert!(a.no_model_fallback && a.no_recommended_models);
}

#[test]
fn unknown_flag_is_rejected() {
    let e = Args::try_parse_from(["omt", "--no-model-fallbacks"]).unwrap_err();
    assert_eq!(e.kind(), clap::error::ErrorKind::UnknownArgument);
}

#[test]
fn boolean_flag_with_value_is_rejected() {
    let e = Args::try_parse_from(["omt", "--multi-session=true"]).unwrap_err();
    assert_eq!(e.kind(), clap::error::ErrorKind::TooManyValues);
}
```

추가로 `--skill`의 빈 문자열은 `value_parser`에서 거부하도록 한다: `|s: &str| if s.is_empty() { Err("empty skill dir".into()) } else { Ok(PathBuf::from(s)) }`. 이 검증은 테스트 `skill_empty_value_rejected`로 고정한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- `cargo test -p omp-cli --test cli_flags -- --nocapture` 전 케이스 통과, 실패 0.
- 위 8개 테스트 이름이 실제로 수집된다: `cargo test -p omp-cli --test cli_flags -- --list | grep -c '^'`가 8 이상.
- `cargo clippy -p omp-cli --all-targets -- -D warnings` 무경고.
- `cargo test -p omp-cli` 기존 테스트 회귀 0건(특히 `--skills` CSV 파싱 테스트 유지).
- 회귀 방지 확인: `--skill` 필드를 제거하면 `move_host_argv_matches`/`gateway_argv_matches`가 아니라 `gateway_argv_matches`가 실패해야 한다(테스트 유효성 자체 검증).
- `cargo fmt --check -p omp-cli` 통과.

## 5. 엣지 케이스 및 실패 복구

- `--skill` 값 누락(`["--skill"]`) → clap `InvalidValue`/`MissingRequiredArgument` 계열 오류, 종료코드 2. 조용히 빈 벡터가 되면 안 된다.
- `--skill=`(빈 문자열) → value_parser에서 거부. `PathBuf::from("")`이 스킬 루트로 흘러가 로더가 cwd 전체를 스캔하는 사고를 차단한다.
- `--skill` 중복 경로 → 파서는 그대로 3개 보존. 중복 제거는 스킬 로딩 단계에서 수행하며, 파서 테스트는 "제거하지 않음"을 계약으로 고정한다(순서 보존 회귀 방지).
- `--session-runtime InProcess`(대문자)·`in_process`(언더스코어) → `InvalidValue` 오류. `move-host.ts:37`은 하이픈 표기만 보내므로 케이스 민감성을 유지한다.
- `--no-model-fallback=true` → `TooManyValues` 오류. 불리언에 값이 붙는 형태를 허용하면 하네스의 오타가 무시된다.
- 공백 포함 경로(`--skill "/opt/my skills"`) → 단일 argv 원소이므로 정상 파싱, `PathBuf`에 공백 보존.
- 비UTF-8 경로 → `PathBuf`는 `OsString` 기반이므로 허용. 테스트는 `std::os::unix::ffi::OsStrExt`로 1건 추가 검증.
- 미지 플래그(`--no-model-fallbacks`) → `UnknownArgument`. 이 테스트가 깨지면 하네스가 조용히 기본 동작(폴백 활성)으로 떨어지는 위험이 재발한다.
- 복구 원칙: 파서 실패는 런타임 재시도로 감추지 않는다. 실패 시 `Args::try_parse_from`의 `ErrorKind`를 그대로 상위로 전파하고, 호스트 스폰 측은 stderr를 그대로 노출해 `gateway.ts:1167`/`move-host.ts:37`의 argv 불일치를 즉시 식별 가능하게 한다.
