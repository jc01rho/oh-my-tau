# 조사 조각 01 (1795326973 확인필요 1): `gateway.ts:1167` `--skill <경로>`와 `tau` `cli.rs:129` `--skills <CSV>`의 코드 대조 및 확정 결론 (inv-01-skill-flag-vs-skills-filter)

## 1. 목적 및 코드 근거 (File & Line)

목적: OmO 하네스(`omo-nyang`)가 호스트 `omt`를 spawn할 때 전달하는 `--skill <디렉터리경로>` 플래그가 tau CLI에 존재하지 않아 발생하는 인자 불일치를 확정하고, `--skills <CSV 필터>`와의 의미 충돌 없이 두 플래그를 공존시키는 계약을 고정한다.

| 위치 | 실측 내용 | 판정 |
| --- | --- | --- |
| `omo-nyang/src/gateway.ts:1167` | `--skill <디렉터리경로>`를 반복 전달 (스킬 루트 경로) | tau에 미구현 |
| `crates/omp-cli/src/cli.rs:129` | `--skills <CSV>` 정의 (스킬 이름 필터, 경로 아님) | 의미 상이 |
| `crates/omp-cli/src/cli.rs:309` | `--skills` 값이 `Args`로 전달되는 소비 지점 | 필터 전용 |
| `crates/omp-cli/src/cli.rs:97-166` | `Args` 구조체 및 clap 파싱 구간 | `skill_paths` 필드 부재 |
| `omo-nyang/src/model-routing.ts:654-658` | `--no-model-fallback`, `--no-recommended-models` 전달 | 본 조각 범위 외(교차 확인만) |
| `omo-nyang/src/relay/move-host.ts:37` | `--multi-session`, `--session-runtime in-process` | 본 조각 범위 외 |

확정 결론: `--skills`는 "이름 CSV 필터"로 유지하고, `--skill`은 "반복 가능한 추가 스킬 루트 경로"로 신규 도입한다. 두 플래그는 이름이 유사하지만 타입(`Vec<String>` vs `Vec<PathBuf>`)과 처리 단계(필터 vs 탐색 루트)가 다르므로 병합하지 않는다. clap의 접두사 매칭(`--skill` → `--skills`)은 `--skill`이 정확히 등록될 때만 안전하므로, 반드시 명시적 `long = "skill"`로 등록한다.

## 2. 상세 계약 및 데이터/타입 명세

Rust 측 타입 (crates/omp-cli/src/cli.rs, `Args` 내 추가):

```rust
/// 추가 스킬 루트 경로 (반복 가능, 디렉터리)
#[arg(long = "skill", value_name = "DIR", action = clap::ArgAction::Append)]
pub skill_paths: Vec<PathBuf>,

/// 스킬 이름 CSV 필터 (기존 유지)
#[arg(long = "skills", value_name = "CSV", value_delimiter = ',')]
pub skills: Option<Vec<String>>,
```

탐색 순서 및 우선순위(낮음 → 높음):

1. 내장 스킬 디렉터리 (바이너리 동봉)
2. 사용자 루트 `~/.omp/skills` (`crates/omp-settings/src/lib.rs`의 설정 루트 규약)
3. `--skill`로 전달된 경로들 (전달 순서 유지)
4. 프로젝트 루트 `<cwd>/.omp/skills`

스킬 디스크립터(직렬화 형태, RPC `get_protocol_info` 및 내부 목록 공용):

```json
{
  "name": "omo-fleet",
  "description": "fleet orchestration helpers",
  "path": "/abs/path/skills/omo-fleet",
  "source": "flag",
  "enabled": true
}
```

`source`는 `"builtin" | "user" | "flag" | "project"` 열거형. 동일 `name` 충돌 시 우선순위가 높은 쪽이 이기고, 패배한 항목은 `enabled:false`가 아니라 목록에서 제거하고 `shadowed_by`를 로그(JSONL)에만 기록한다.

RPC 계약: `get_protocol_info()` 응답은 `{protocolVersion:2, serverVersion: env!("CARGO_PKG_VERSION"), engine:"oh-my-tau", commands:[...]}`이며, 본 조각은 `commands`에 스킬 목록 조회 명령(예: `skills_list`)을 추가할 때의 인자 스키마만 확정한다. 미지원 명령은 `crates/omp-cli/src/rpc.rs:1094`의 `other => Err(CmdError::new(format!("Unknown command: {other}")))` 경로를 그대로 사용한다.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 또는 의사코드 포함)

```rust
fn resolve_skill_roots(args: &Args, settings_root: &Path, cwd: &Path) -> Vec<(SkillSource, PathBuf)> {
    let mut roots = vec![(SkillSource::Builtin, builtin_skills_dir())];
    roots.push((SkillSource::User, settings_root.join("skills")));
    for p in &args.skill_paths {
        let abs = if p.is_absolute() { p.clone() } else { cwd.join(p) };
        roots.push((SkillSource::Flag, abs));
    }
    roots.push((SkillSource::Project, cwd.join(".omp/skills")));
    roots
}

fn load_skills(roots: &[(SkillSource, PathBuf)], filter: Option<&[String]>) -> Vec<SkillDesc> {
    let mut seen: HashMap<String, SkillDesc> = HashMap::new();
    for (src, root) in roots {
        let canon = match root.canonicalize() { Ok(c) => c, Err(_) => { warn_skip(root, src); continue } };
        if !canon.is_dir() { warn_skip(root, src); continue; }
        for entry in read_dir_sorted(&canon) {
            let manifest = entry.join("SKILL.md");
            if !manifest.is_file() { continue; }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(f) = filter { if !f.iter().any(|x| x == &name) { continue; } }
            seen.insert(name.clone(), SkillDesc { name, path: canon.join(&entry), source: *src, enabled: true, .. });
        }
    }
    seen.into_values().collect()
}
```

TS 측(`omo-nyang/src/gateway.ts:1167`)은 변경 없이 유지하되, 경로 인자를 항상 절대경로로 정규화해 전달하도록 assert를 추가한다. `--skills` CSV는 `value_delimiter = ','`로 파싱하고 공백 트리밍은 별도 후처리에서 수행한다.

검증 절차:
- `--skill`이 `--skills`로 오파싱되지 않는지: `omt --skill ./a --skill ./b --mode rpc` 후 `get_protocol_info`에서 `source:"flag"` 항목 2건 확인.
- `--skills omo-fleet` 단독 사용 시 탐색 루트는 기본값만 유지되고 결과가 1건 이하인지 확인.
- 두 플래그 동시 사용 시 `--skill ./x --skills omo-fleet` → 루트 확장 + 이름 필터 교집합.

## 4. 완료 판정 기준 (Acceptance Criteria & 명령어)

- AC1: `Args::skill_paths: Vec<PathBuf>` 필드가 존재하고 `--skill`이 `ArgAction::Append`로 등록된다.
- AC2: `--skill`과 `--skills` 동시 사용 시 clap 파싱 에러가 없고, 두 값이 서로 다른 필드로 전달된다.
- AC3: 존재하지 않는 `--skill` 경로는 하드 에러가 아니라 경고 후 스킵되며, 나머지 루트 탐색이 계속된다.
- AC4: `source` 우선순위(builtin < user < flag < project)에 따라 동일 이름 스킬이 결정적으로 1건만 노출된다.
- AC5: `get_protocol_info` 응답의 `protocolVersion`이 여전히 `2`이고 `engine`이 `"oh-my-tau"`다.

명령어:

```bash
cargo test -p omp-cli skill_flag -- --nocapture
cargo test -p omp-cli skill_precedence
cargo build -p omp-cli && ./target/debug/omt --skill ./skills/a --skill ./skills/b --mode rpc
grep -n 'long = "skill"' crates/omp-cli/src/cli.rs
grep -n '"skill"' omo-nyang/src/gateway.ts | sed -n '1,5p'
```

## 5. 엣지 케이스 및 실패 복구

- 경로 부재/권한 오류: `canonicalize()` 실패 또는 `is_dir()` 거짓이면 경고 로그 후 스킵. 전체 기동은 계속한다(하드 실패 금지). 이유: 하네스가 임시 디렉터리를 넘길 수 있음.
- 심볼릭 링크 순환: `canonicalize()` 결과를 `HashSet<PathBuf>`로 방문 집합 관리, 재방문 시 스킵.
- 동일 경로 중복 전달(`--skill ./a --skill ./a`): canonical 기준 dedup, `source`는 최초 등장 값 유지.
- 스킬 이름 충돌: 우선순위 승자만 노출, 패자는 로그에 `shadowed_by` 기록. 사용자에게는 조용히 1건만 보인다.
- `--skills` 필터가 아무것도 매칭하지 않음: 빈 목록을 정상 응답으로 반환(에러 아님), 기동은 계속.
- CSV 공백/빈 토큰(`--skills "a, b,,"`): 트리밍 후 빈 토큰 제거, 전부 제거되면 필터 미적용이 아니라 "빈 필터 = 0건"으로 명시 처리(의도 은닉 방지).
- 상대 경로 해석 기준: `--cwd`가 주어지면 그 값을 기준으로, 없으면 프로세스 cwd 기준. RPC `--multi-session` 하에서 세션별 cwd가 달라도 스킬 루트는 기동 시 1회 확정(재로딩은 `reload()` 경로에서만).
- `reload()` 호출 시(`omo-nyang/src/lane-hold.ts:29-76` 계약): 스킬 루트 재탐색 후 `{cancelled:false}` 반환. 재탐색 중 경로 소실은 스킵 처리하여 `reload`가 실패로 끝나지 않게 한다.
- clap 접두사 충돌 회귀: `--skill` 미등록 상태에서 `--skill x`는 "unexpected argument" 또는 `--skills` 오매칭 위험이 있으므로, 회귀 테스트에서 `Args::skill_paths == vec![PathBuf::from("x")]`를 직접 단언한다.
- 실패 복구 원칙: 파싱 단계 실패는 exit code 비0 + stderr 1줄, 탐색 단계 실패는 경고 + 부분 성공. RPC 모드에서는 실패를 `{type:"response", id, success:false, data?, error?, errorCode?}` 프레임(`crates/omp-cli/src/rpc.rs:203-226` 조립, `omo-nyang/src/relay/move-host.ts:4-7, 61` 파싱)으로 반환하고 `code`와 `errorCode`를 함께 직렬화한다.
