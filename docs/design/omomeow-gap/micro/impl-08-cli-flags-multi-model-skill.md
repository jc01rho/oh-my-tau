# 구현 조각 08: `crates/omp-cli/src/cli.rs`의 `--multi-session`, `--session-runtime`, `--no-model-fallback`, `--no-recommended-models`, `--skill <경로>` 파서 및 스킬 루트 로딩 구현 명세 (impl-08-cli-flags-multi-model-skill)

## 1. 목적 및 코드 근거 (File & Line)

OmO 하네스(`omo-nyang`)는 tau를 자식 프로세스로 띄울 때 다음 인자를 전달하는데, 현재 tau CLI 파서가 이를 수용하지 못해 즉시 종료되거나 무시된다.

- `omo-nyang/src/gateway.ts:1167` — `--skill <디렉터리경로>`를 전달. 그러나 `crates/omp-cli/src/cli.rs:129,309`에는 `--skills <CSV필터>`만 존재하여 `--skill`은 `error: unexpected argument`로 실패한다.
- `omo-nyang/src/model-routing.ts:654-658` — `--no-model-fallback`, `--no-recommended-models`를 전달. 대응 필드가 `Args`(`crates/omp-cli/src/cli.rs:97-166`)에 없다.
- `omo-nyang/src/relay/move-host.ts:37` — `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`로 호스트를 실행. `--multi-session`/`--session-runtime` 미지원 시 세션 이동 경로 전체가 붕괴한다.
- 실패는 RPC 응답으로 되돌아가야 한다. `crates/omp-cli/src/rpc.rs:593 async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`가 명령을 분기하고 `rpc.rs:1094 other => Err(CmdError::new(format!("Unknown command: {other}")))`로 미지원을 처리하며, `rpc.rs:203-226 response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립한다. `move-host.ts:4-7,61`이 `errorCode`를 파싱하므로 신규 검증 실패도 `code`와 `errorCode`를 동시에 직렬화해야 한다.
- 계약 정합: `omo-nyang/src/lane-hold.ts:29-76`의 `get_protocol_info()`는 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`를 기대하므로, 신규 플래그는 `commands` 배열 계약을 깨지 않아야 한다.

## 2. 상세 계약 및 데이터/타입 명세

`crates/omp-cli/src/cli.rs`의 `Args`(97-166)에 다음 필드를 추가한다. 기존 `--skills`(129)는 CSV 이름 필터로 그대로 유지한다.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SessionRuntime { InProcess, External }

#[arg(long, action = clap::ArgAction::SetTrue)]
pub multi_session: bool,

#[arg(long, value_name = "MODE", value_enum)]
pub session_runtime: Option<SessionRuntime>,

#[arg(long = "no-model-fallback", action = clap::ArgAction::SetTrue)]
pub no_model_fallback: bool,

#[arg(long = "no-recommended-models", action = clap::ArgAction::SetTrue)]
pub no_recommended_models: bool,

#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
pub skill_paths: Vec<PathBuf>,
```

정규화 결과는 단일 구조체로 고정한다(런타임/확장/RPC가 동일 값을 참조).

```rust
pub struct RuntimeCliConfig {
    pub multi_session: bool,
    pub session_runtime: SessionRuntime, // 기본 InProcess
    pub model_fallback: bool,            // 기본 true
    pub recommended_models: bool,        // 기본 true
    pub skill_roots: Vec<PathBuf>,       // 정규화·중복제거·존재검증 완료
}

pub enum CliConfigError {
    SessionRuntimeRequiresMultiSession,
    SessionRuntimeUnsupported { requested: String },
    SkillRootNotFound { path: PathBuf },
    SkillRootNotDir { path: PathBuf },
    SkillRootUnreadable { path: PathBuf, source: std::io::ErrorKind },
}
```

우선순위는 `CLI 플래그 > ~/.omp 설정 > 기본값`이다. `--session-runtime` 미지정 + `--multi-session`이면 `InProcess`로 승격한다. `--no-*` 계열은 부정 플래그이므로 설정 파일 값보다 항상 우선한다.

`skill_roots` 해석 규칙: (1) `std::fs::canonicalize`로 절대경로화, (2) 입력 순서 보존하며 중복 제거, (3) 각 루트의 하위 1단계 디렉터리에서 `SKILL.md`를 탐색해 `name`(frontmatter) 또는 디렉터리명을 스킬 ID로 사용, (4) 동일 ID 충돌 시 먼저 등록된 루트가 승리하고 나머지는 경고 로그, (5) `--skills` CSV가 주어지면 루트 열거 결과에 대한 화이트리스트 필터로만 작동한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

`cli.rs`에 정규화 함수를 추가하고 `main`에서 `Runtime` 생성 직전에 호출한다.

```rust
pub fn resolve_runtime_cli_config(args: &Args, settings: &Settings) -> Result<RuntimeCliConfig, CliConfigError> {
    if args.session_runtime.is_some() && !args.multi_session && !settings.multi_session {
        return Err(CliConfigError::SessionRuntimeRequiresMultiSession);
    }
    let multi_session = args.multi_session || settings.multi_session;
    let session_runtime = match args.session_runtime {
        Some(SessionRuntime::External) => return Err(CliConfigError::SessionRuntimeUnsupported { requested: "external".into() }),
        Some(rt) => rt,
        None if multi_session => SessionRuntime::InProcess,
        None => SessionRuntime::InProcess,
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    for raw in &args.skill_paths {
        let p = std::fs::canonicalize(raw).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CliConfigError::SkillRootNotFound { path: raw.clone() },
            k => CliConfigError::SkillRootUnreadable { path: raw.clone(), source: k },
        })?;
        if !p.is_dir() { return Err(CliConfigError::SkillRootNotDir { path: p }); }
        if !roots.contains(&p) { roots.push(p); }
    }
    Ok(RuntimeCliConfig {
        multi_session,
        session_runtime,
        model_fallback: !(args.no_model_fallback || settings.no_model_fallback),
        recommended_models: !(args.no_recommended_models || settings.no_recommended_models),
        skill_roots: roots,
    })
}
```

RPC 경로에서는 `dispatch`(`rpc.rs:593`) 초입에서 동일 함수를 재사용하고, 실패 시 `CmdError`에 `code`를 부여해 `response()`(`rpc.rs:203-226`)가 `errorCode`까지 내보내도록 한다. 예: `CmdError::with_code("skill_root_not_found", msg)`. `get_protocol_info` 응답의 `commands` 배열에는 `"get_protocol_info"`, `"resume_session"`, `"reload"`, `"interrupt"`, `"set_client_info"`, `"move_session"`, `"release_session"`, `"extension_request"`가 유지되어야 하며 신규 CLI 플래그는 이 배열에 영향을 주지 않는다.

스킬 로딩은 `Runtime` 빌더에서 `skill_roots`를 순서대로 순회하며 `SkillRegistry::register_from_dir(root, &filter)`를 호출한다. `model_fallback=false`이면 모델 해석 실패 시 대체 모델로 넘어가지 않고 `model_unavailable` 오류를 즉시 반환하며, `recommended_models=false`이면 추천 모델 목록 주입을 생략한다.

TS 측은 변경하지 않는다. `gateway.ts:1167`, `model-routing.ts:654-658`, `move-host.ts:37`의 인자 문자열이 그대로 유효함을 회귀 테스트로 고정한다.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

1. `omt --help`에 `--multi-session`, `--session-runtime <MODE>`, `--no-model-fallback`, `--no-recommended-models`, `--skill <DIR>`가 모두 노출되고 `--skills`는 유지된다.
2. `omt --mode rpc --multi-session --session-runtime in-process -e <ext>`가 파싱 오류 없이 기동하고 `get_protocol_info`가 `protocolVersion:2`를 반환한다.
3. `--skill` 2회 반복 시 두 루트의 스킬이 모두 등록되고, 동일 경로 중복은 1회만 등록된다.
4. `--session-runtime in-process`를 `--multi-session` 없이 단독 지정하면 `session_runtime_requires_multi_session` 코드로 실패한다.

```bash
cargo test -p omp-cli cli_flags -- --nocapture
cargo run -p omp-cli -- --help | grep -E -- '--(multi-session|session-runtime|no-model-fallback|no-recommended-models|skill )'
cargo run -p omp-cli -- --mode rpc --multi-session --session-runtime in-process <<< '{"id":1,"command":"get_protocol_info"}'
cd omo-nyang && bun test src/gateway.test.ts src/relay/move-host.test.ts
```

## 5. 엣지 케이스 및 실패 복구

- `--session-runtime` 값 오타: clap `value_enum`이 `invalid value`로 거부하고 종료 코드 2를 반환. RPC 모드에서는 `invalid_argument` 코드로 응답.
- `--skill` 경로가 파일이거나 권한 없음: 각각 `skill_root_not_dir`, `skill_root_unreadable`로 실패하며 부분 등록을 남기지 않도록 루트 검증을 전부 통과한 뒤 등록 단계로 진입한다(2단계 커밋).
- 심볼릭 링크 순환: `canonicalize` 실패를 `skill_root_unreadable`로 매핑하고 무한 루프를 차단한다.
- `--skills` 필터가 어떤 스킬과도 매칭되지 않음: 치명 오류가 아니라 경고 후 빈 레지스트리로 기동(하네스 부팅 실패 방지).
- `--multi-session` 없이 `release_session`/`move_session` 호출: `rpc.rs:1094`의 미지원 분기로 흘러가지 않도록 `session_move_requires_multi_session` 코드로 명시 거부하고 `errorCode`를 함께 직렬화한다.
- `--session-runtime external` 요청: 현재 미구현이므로 `session_runtime_unsupported`로 즉시 실패시켜, 조용히 in-process로 폴백하여 세션 이동이 잘못 성공하는 상황을 막는다.
- 설정 파일에 `no_model_fallback:false`가 있어도 CLI `--no-model-fallback`이 항상 우선한다(부정 플래그 우선 원칙).
- Windows 경로 구분자: `PathBuf` 기반 처리로 통일하고 문자열 결합 금지.
