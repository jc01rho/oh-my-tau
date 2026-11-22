# tau CLI 플래그 `--no-model-fallback`·`--no-recommended-models` 설계

## 1. 현재 상태 (근거)

- 플래그가 없다. `crates/omp-cli/src/cli.rs:142` 의 `VALUELESS_FLAGS` 에 두 이름이 없고, 파서는 `cli.rs:392` 에서 이 목록에 있는 토큰만 boolean 으로 받은 뒤 `cli.rs:515` 에서 `unknown flag` 로 bail 한다. `Args`(`cli.rs:24`)에도 대응 필드가 없어 게이트웨이 argv 는 기동 즉시 죽는다.
- tau 에는 확장 플래그 통로가 없다. `crates/omp-ext/src/host.rs` 전체에서 flag 관련 코드는 `host.rs:278` 의 `OMP_EXT_NO_SMOL` 주석 하나뿐이다. mhc 는 `crates/maho-cli/src/cli/args.rs:377-400` 에서 미지의 `--x` 를 `unknown_flags` 로 흘려 확장이 `register_flag`(`crates/builtins/maho-ext-model-fallback/src/lib.rs:37`)로 받지만, tau 는 그 경로가 없으므로 코어 CLI 표에 직접 넣어야 한다.
- 시작 모델 선택은 `crates/omp-cli/src/main.rs:410-425` 에 모여 있다. `--model`/`--provider` 로 reference 를 만들고(`main.rs:410`), `MODEL_PINNED` 를 세우고(`main.rs:417`), `model::select`(`main.rs:422`)로 해석한 뒤 남은 후보를 `LAUNCH_FALLBACKS` 에 넣는다(`main.rs:425`).
- 쉼표 체인 자체가 폴백 설치다. `crates/omp-cli/src/model.rs:66` → `model.rs:132` → `model.rs:138` `select_chain_with` 가 후보를 순서대로 풀어 자격증명이 있는 첫 후보를 고르고 나머지를 `selected.fallbacks` 로 넘긴다(`model.rs:150`). `--model a,b` 는 `a` 가 키가 없으면 조용히 `b` 로 대체된다.
- 폴백 설정 조립은 `crates/omp-cli/src/runtime.rs:409` `start_with` 의 `runtime.rs:421-431` 이다. resolver 로 live 모델을 다시 풀어 `apply_launch_fallbacks`(`runtime.rs:4214`)를 적용하고 `RecoveryConfig::from_resolved`(`runtime.rs:426`)로 `TurnRecovery`(`runtime.rs:431`)를 만든다. `RecoveryConfig`(`crates/omp-cli/src/recovery.rs:57`)는 `primary.fallbacks` 를 `FallbackConfig.chains` 로 옮긴다(`recovery.rs:69-91`).
- 폴백 발동은 `runtime.rs:969` 의 `is_retryable` 게이트 → `recover`(`runtime.rs:1209`) → `handle_retryable`(`runtime.rs:1385`) 이다. 계정 로테이션(`runtime.rs:1282`)을 먼저 시도하고, `begin_attempt`/`consult`(`runtime.rs:1396-1397`)로 후보를 받아 `apply_fallback`(`runtime.rs:1525`)이 모델을 바꾼다. 다음 프롬프트에서 `maybe_restore_primary`(`runtime.rs:1553`, 호출부 `runtime.rs:1711`)가 원래 모델로 되돌린다.
- 게이트가 두 겹이고 둘 다 부적합하다. `is_retryable` 은 `self.enabled && is_transient(..)`(`recovery.rs:490-491`)이고 `enabled` 는 `cfg.retry.enabled`(`recovery.rs:426`)다. `retry.enabled=false` 는 백오프 재시도와 계정 로테이션까지 함께 끄므로(`recovery.rs:498`) `--no-model-fallback` 구현으로 쓸 수 없다. `consult` 는 `cfg.fallback.chains` 가 비면 빈 목록을 돌려준다(`recovery.rs:552-557`) — 체인을 비우는 방식은 `retry.fallbackChains` 설정과 구분되지 않는다.
- 설정 키는 있는데 소비자가 없다. `retry.modelFallback` 은 스키마(`crates/omp-settings/src/schema.rs:2543`)와 TUI 정의(`crates/omp-tui/src/overlays/settings_defs.rs:799`)에만 있고 `crates/` 어디에서도 읽지 않는다. `RetryConfig`(`crates/omp-providers/src/fallback.rs:31`)에도 필드가 없고 `from_settings`(`fallback.rs:86`)는 enabled/maxRetries/baseDelayMs/maxDelayMs/fallbackRevertPolicy 만 읽는다.
- 추천 모델 주입은 애초에 없다. `recommendedModels`·`offRecommendedModel` 키가 스키마에 없고(대조: `modelRoles` 는 `schema.rs:2115`), 카탈로그의 `featured`(`vendor/pa-ai/src/models_generated.rs:47`)는 `model_info` 에서 제거된다(`runtime.rs:4167`). 세션 시작에 모델을 바꾸는 경로는 저장 세션 모델 복원(`restore_session_model`, `runtime.rs:2477`)뿐이고 이는 추천이 아니다. `retry.fallbackOn`(`schema.rs:2515`), `retry.usageAwareFallback`(`schema.rs:2550`)도 스키마에만 있다.

## 2. 게이트웨이가 요구하는 계약

- 두 플래그가 argv 에 있어도 tau 가 기동해야 한다. 현재는 `cli.rs:515` 에서 종료한다.
- `--no-model-fallback`: 세션 수명 동안 provider/id 가 복구 로직 때문에 바뀌지 않는다. mhc 근거는 `crates/maho-core/src/agent_session.rs:4414-4422` 로, 플래그를 `retry.modelFallback=false` 오버라이드로 번역한다. 게이트웨이 모델 카탈로그도 "run with --no-model-fallback, so an id missing here is Model not found, not a fallback"(`crates/extensions/maho-ext-omomeow/src/model_provider.rs:67`)을 전제한다.
- 모델 선택 자체는 게이트웨이 몫이다. `set_model`(`crates/omp-cli/src/rpc.rs:883`)과 `cycle_model`(`rpc.rs:888`)은 계속 동작해야 하고, 그 뒤에도 폴백은 꺼진 채여야 한다.
- 백오프 재시도(`retry.enabled`)는 별개 축이다. mhc 는 `retry.modelFallback` 만 끄고 `retry.enabled` 는 건드리지 않는다(`agent_session.rs:4414-4422`).
- `--no-recommended-models`: 세션 시작 시 tau 가 모델을 자동 교체하지 않는다는 선언이다. mhc 는 이 플래그로 추천 모델 자동 전환을 끈다(`crates/builtins/maho-ext-recommended-models/src/lib.rs:192`, `:233`, 자동 전환 조건 `:156`).

## 3. 설계

### 3.1 플래그 표면

```rust
// crates/omp-cli/src/cli.rs:142
const VALUELESS_FLAGS: &[&str] = &[
    /* ... */ "--no-model-fallback", "--no-recommended-models",
];
// crates/omp-cli/src/cli.rs:24 Args
pub no_model_fallback: bool,
pub no_recommended_models: bool,
```

`cli.rs:392` 디스패치에 두 arm 을 추가하고 `main.rs:43` 의 `HELP` 문자열에 넣는다. 파서 규칙(`--flag=value` 분해, `--` 종결)은 손대지 않는다.

### 3.2 폴백 차단 지점: `RetryConfig.model_fallback`

```rust
// crates/omp-providers/src/fallback.rs:31
pub struct RetryConfig { /* ... */ pub model_fallback: bool } // Default: true
// from_settings (fallback.rs:86) — 기존 provenance 게이트와 같은 형태
if set("retry.modelFallback") { c.model_fallback = settings.get_bool("retry.modelFallback").unwrap_or(true); }
```

이렇게 하면 지금 죽어 있는 `retry.modelFallback` 키가 살아나고, 플래그는 그 키의 argv 오버라이드가 된다(mhc 와 같은 형태). `FallbackConfig`(`fallback.rs:402`)는 건드리지 않는다. 체인 목록은 사용자가 설정한 사실 그대로 두고, 발동만 막는다.

```rust
// crates/omp-cli/src/recovery.rs:57
impl RecoveryConfig {
    pub fn with_model_fallback(mut self, on: bool) -> Self { self.retry.model_fallback = on; self }
}
// consult (recovery.rs:552) 첫 줄
if !self.cfg.retry.model_fallback { return Vec::new(); }
// begin_attempt (recovery.rs:524) — 429 즉시 페일오버 제로 딜레이도 함께 막는다
let has_fallback = self.cfg.retry.model_fallback && /* 기존 조건 */;
```

`consult` 가 빈 목록을 돌려주면 `runtime.rs:1401` 후보 루프가 비고 `apply_fallback`(`runtime.rs:1525`)은 호출되지 않는다. `commit_switch`(`recovery.rs:579`)가 불리지 않으므로 `FallbackState.active` 가 서지 않고, `restore_plan`(`fallback.rs:1008`)도 `None` 이라 `maybe_restore_primary`(`runtime.rs:1553`)가 무해해진다. 즉 차단 지점은 `consult` 하나이고 나머지는 자동으로 죽는다.

### 3.3 프로세스 전역 핀과 재적용 지점

`MODEL_PINNED`(`runtime.rs:4197`) 옆에 전용 static 을 둔다. `MODEL_PINNED` 를 재사용하면 안 된다 — RPC 모드에서 `resume_target` 이 `None` 이라 `main.rs:541` 이 핀을 곧바로 해제한다.

```rust
// crates/omp-cli/src/runtime.rs:4197 옆
pub static MODEL_FALLBACK_DISABLED: AtomicBool = AtomicBool::new(false);
```

`RecoveryConfig::from_resolved` 호출 지점은 세 곳이다: `runtime.rs:426`, `restore_session_model` 의 `runtime.rs:2508`, `select_model` 의 `runtime.rs:3364`. `select_model` 은 `set_model`/`cycle_model` 이 지나는 길이므로 여기서 다시 적용하지 않으면 게이트웨이의 `set_model` 한 번에 폴백이 되살아난다. 적용은 `TurnRecovery::new` 직전 한 곳(`runtime.rs:431`)과 두 reconfigure 지점에서 한다.

```rust
// runtime.rs:431
let recovery = TurnRecovery::new(
    recovery_cfg.with_model_fallback(!MODEL_FALLBACK_DISABLED.load(Ordering::Relaxed)));
// runtime.rs:2508, runtime.rs:3364
rec.reconfigure(RecoveryConfig::from_resolved(&resolved)
    .with_model_fallback(!MODEL_FALLBACK_DISABLED.load(Ordering::Relaxed)));
```

`main.rs` 는 플래그를 `main.rs:284` 부근(핀 해제보다 앞)에서 static 에 심고, `main.rs:410-421` 에서 reference 를 첫 쉼표에서 자른다. 그래야 `select_chain_with`(`model.rs:138`)가 뒤 후보로 조용히 대체하지 않는다. `LAUNCH_FALLBACKS`(`main.rs:425`)는 플래그가 켜져 있으면 설정하지 않는다.

### 3.4 우선순위

| 순위 | 입력 | 효과 |
|---|---|---|
| 1 | `--no-model-fallback` | 복구에 의한 모델 교체 전면 금지. 프로세스 수명. `set_model`/`cycle_model` 로 해제 불가 |
| 2 | `retry.modelFallback=false` | 같은 효과, 설정 레이어 |
| 3 | `retry.fallbackChains` 존재 | 1·2 가 꺼져 있을 때만 후보 공급 |
| 4 | RPC `set_model`/`cycle_model` | 어떤 모델이 live 인지만 결정. 폴백 허용 여부는 못 바꾼다 |
| 5 | RPC `set_auto_retry`(`rpc.rs:985` → `runtime.rs:3719`) | `retry.enabled` 만 토글. 백오프 재시도 축 |
| 6 | `--model a,b` | 1 이 켜져 있으면 첫 항목만 채택 |
| - | `retry.fallbackRevertPolicy` | 1 이 켜져 있으면 도달 불가 |

계정 로테이션(`runtime.rs:1282`)은 provider/id 를 바꾸지 않으므로 1 의 적용 대상이 아니다. 게이트웨이 계약이 "모델 id 고정"(`model_provider.rs:67`)이므로 유지한다.

### 3.5 `--no-recommended-models`

파싱만 하고 런타임 효과는 없다. tau 에 끌 대상이 없기 때문이다(1절 근거). 다음 세 가지는 명시적으로 건드리지 않는다: `get_available_models`(`rpc.rs:889`)의 목록, `/model` 브라우저(`crates/omp-cli/src/subcmd/models.rs:157`), 자동 제목의 `smol` 역할 호출(`runtime.rs:2940`, `runtime.rs:2943`). 제목까지 끄려면 게이트웨이는 `--no-title`(`cli.rs:160` → `main.rs:284-287`)을 쓴다.

## 4. 구현 단계 (커밋 단위)

1. `cli: accept --no-model-fallback and --no-recommended-models` — `cli.rs:24` 필드, `cli.rs:142` 목록, `cli.rs:392` arm, `main.rs:43` HELP, `cli.rs:846` `valueless_flags_accepted` 확장.
2. `providers: consume retry.modelFallback in RetryConfig` — `fallback.rs:31` 필드, `fallback.rs:86` 읽기, `fallback.rs` 단위 테스트.
3. `recovery: gate model fallback on RetryConfig::model_fallback` — `recovery.rs:57` `with_model_fallback`, `recovery.rs:552` 조기 반환, `recovery.rs:524` `has_fallback` 조건, `recovery.rs:733` 테스트.
4. `runtime: honor a process-wide model-fallback pin` — `runtime.rs:4197` static, `runtime.rs:431`/`2508`/`3364` 적용, `main.rs:284` 부근 주입.
5. `main: truncate --model chains under --no-model-fallback` — `main.rs:410-426`, `LAUNCH_FALLBACKS` 미설정.
6. `docs: record the flag contract` — `main.rs:43` HELP 문구와 `docs/` 한 절. `--no-recommended-models` 가 no-op 임을 명시.

## 5. 테스트

- `cli.rs:562` 모듈: 두 플래그가 `Args` 에 서고, 플래그 없이도 기존 `unknown_flag_errors_with_omp_message`(`cli.rs:601`)가 그대로 통과한다.
- `fallback.rs` 모듈: `Settings` 에 `retry.modelFallback: false` 를 Global provenance 로 넣으면 `RetryConfig::from_settings` 가 `model_fallback=false`, 미설정이면 `true`(`crates/omp-providers/src/lib.rs:1095-1100` 의 기존 픽스처 형식 재사용).
- `recovery.rs:733`: `cfg()` 헬퍼에 `model_fallback=false` 를 넣고 `consult` 가 빈 벡터인지, 429 + 체인 존재에서 `begin_attempt` 의 `delay_ms` 가 0 으로 깎이지 않는지. 둘 다 순수 함수라 타이밍 의존이 없다.
- `runtime.rs` 테스트: `h.rt.set_model("q","b")`(`recovery.rs:1577` 형식) 후 `TurnRecovery` 의 cfg 가 여전히 `model_fallback=false` 인지 — 3.3 의 재적용 회귀를 잡는 유일한 테스트다.
- 게이트: `just check`, `just lint`, `cargo test -p omp-cli -p omp-providers -p omp-settings --locked`.

## 6. QA 시나리오

`--mode rpc` 로 띄우고 mock provider(`tests-e2e` 가 쓰는 로컬 mock)로 429 와 정상 응답을 만든다.

1. `--model p/a --no-model-fallback`, 429 응답 → `retry_fallback_applied` 프레임 없음, `auto_retry_start`/`auto_retry_end` 만, live 모델 `p/a` 유지.
2. 같은 조건에 `retry.fallbackChains: {p/a: [p/b]}` 설정 → 여전히 전환 없음(1번 우선).
3. 1번 세션에 `set_model` 로 `p/c` 지정 → `model_changed` 발생, 이어 429 → 전환 없음(재적용 확인).
4. `cycle_model`(`rpc.rs:444`)로 다음 모델 이동 → 이후 429 에서도 전환 없음.
5. 플래그 없이 1번과 동일 → 기존대로 `p/b` 로 전환(회귀 없음).
6. `--model p/a,p/b --no-model-fallback`, `p/a` 키 없음 → `p/a` 로 해석되고 대체 없음. 기동 로그에 경고 1회.
7. `--no-recommended-models` 로 기동 후 첫 프롬프트까지 `model_changed` 0회, `get_available_models` 응답 목록이 플래그 없을 때와 동일.
8. `--no-recommended-models` + `--no-title` → 제목용 `smol` 호출 없음.

## 7. 위험과 미확인

- `retry.modelFallback` 를 실제로 읽게 되면 이 키를 이미 `false` 로 써 둔 사용자의 동작이 바뀐다. 현재 소비자가 없어 실사용자는 없을 것으로 보나 확인 필요.
- 계정 로테이션(`runtime.rs:1282`)을 유지하기로 한 결정은 게이트웨이가 자격증명까지 고정하는 경우 어긋난다. `credential_id` 핀 여부는 확인 필요.
- `MODEL_FALLBACK_DISABLED` 는 프로세스 전역이다. tau 의 RPC 런타임은 현재 단일(`rpc.rs:1384`)이라 문제가 없지만, `--multi-session` 류가 들어오면 세션별 플래그로 바꿔야 한다.
- `LAUNCH_FALLBACKS`(`runtime.rs:4210`)는 `OnceLock` 이라 두 번째 `set` 이 조용히 실패한다. 플래그 경로에서 아예 설정하지 않는 것으로 회피하지만, 향후 세션 재기동 시맨틱이 생기면 재검토 필요.
- `resume_session` RPC 가 tau 에 아직 없다. 그것이 들어오면 `restore_session_model`(`runtime.rs:2477`)이 저장 모델로 되돌리는 동작이 게이트웨이의 모델 고정과 충돌할 수 있다. 그때 `--no-recommended-models` 가 복원까지 막아야 하는지는 확인 필요.
- 3.4 의 5번(`set_auto_retry`)이 `retry.enabled` 만 건드린다는 결론은 `runtime.rs:3719` 와 `recovery.rs:459` 만 근거로 한다. 게이트웨이가 `set_auto_retry(false)` 를 보내는 경로가 있는지는 확인 필요.
