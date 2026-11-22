# 조사 조각 07: ~/.omo/omo.jsonc(omo-profile.ts fleet-small/fleet-medium)와 ~/.omp/agent/config.yml(omp-settings) 구조 비교 (inv-07-omo-vs-omp-config-and-profiles)

## 1. 목적 및 코드 근거 (File & Line)

목적: 두 하네스의 설정 루트, 프로필 해석, CLI 전달 경로를 1:1로 대조하여 omo-nyang이 `omt`를 구동할 때 `fleet-small`/`fleet-medium` 프로필이 tau 설정으로 손실 없이 투영되도록 매핑 계약을 확정한다. 본 조각은 설정 파일 비교에 한정하며, 도구 격차(§5의 task/memory/workpool)는 다른 조각의 소관이다.

근거:
- 설정 루트 이원화: tau는 `~/.omp`(crates/omp-settings/src/lib.rs), 플릿은 `~/.omo`(omo.jsonc, 프로필 `fleet-small`/`fleet-medium` 등). 근거 요약이 lib.rs를 파일 단위로만 제시하므로 라인 번호는 착수 시 `grep -n "\.omp" crates/omp-settings/src/lib.rs`로 고정하고, 확정 전에는 문서에 라인을 단정하지 않는다.
- CLI 표면: crates/omp-cli/src/cli.rs:97-166(플래그 정의), cli.rs:129,309(`--skills <CSV필터>`만 존재), omo-nyang/src/gateway.ts:1167(`--skill <디렉터리경로>` 전달), model-routing.ts:654-658(`--no-model-fallback`, `--no-recommended-models` 전달).
- 세션/이동 구동: move-host.ts:37-39(`omt --mode rpc --multi-session --session-runtime in-process -e <ext>`), move-host.ts:4-7,61(응답 프레임 파
