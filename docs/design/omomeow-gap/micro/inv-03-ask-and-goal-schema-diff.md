# 조사 조각 03 (1795326973 확인필요 3): tau ask.rs·goal.rs와 OmOMeow ask_user_question·create_goal·get_goal·update_goal 입출력 스키마 대조

- 조각 ID: `inv-03-ask-and-goal-schema-diff`
- 대상 코드: `crates/omp-cli/src/tools/ask.rs`, `crates/omp-cli/src/tools/goal.rs`, `crates/omp-cli/src/rpc.rs:593`, `rpc.rs:1094`, `rpc.rs:203-226`, `omo-nyang/src/relay/move-host.ts:4-7`, `move-host.ts:61`
- 비범위: 질문 UI 렌더링, 목표 저장소 마이그레이션, `lane-hold` 보류 정책 자체의 변경

## 1. 목적 및 코드 근거 (File & Line)

tau는 질문 도구를 `ask({html} | legacy questions[])` 단일 도구로, 목표를 `goal({op})` 단일 도구로 노출한다. 반면 OmOMeow 에이전트는 `ask_user_question`, `create_goal`, `get_goal`, `update_goal` 4개 이름으로 호출한다. 이름·인자·반환 스키마가 모두 다르므로, RPC 디스패치 계층에서 어댑터를 두어 두 계약을 모두 만족시킨다.

- 디스패치 진입: `rpc.rs:593` `async fn dispatch(rt: &Arc<Runtime>, cmd: &Value, kind: &str) -> CmdResult`.
- 미지원 명령 폴백: `rpc.rs:1094` `other => Err(CmdError::new(format!("Unknown command: {other}")))`. 따라서 신규 도구 이름은 반드시 이 catch-all 앞의 match arm으로 등록해야 하며, 등록을 빠뜨리면 OmOMeow 호출은 `Unknown command: ask_user_question`으로 즉시 실패한다.
- 응답 프레임: `rpc.rs:203-226` `response()`가 `{type:"response", id, success, data?, error?, errorCode?}`를 조립하고, `move-host.ts:4-7,61`의 `MoveHost`가 같은 형태를 파싱한다. 실패 시 `code`와 `errorCode`를 함께 직렬화하지 않으면 TS 측에서 오류 분기가 불가능하다.
- 도구 격차 근거: tau의 `ask`/`goal`은 단일 도구이나 OmOMeow는 `ask_user_question({questions:[{header,question,multiSelect,options}],waitForAnswer,required})`와 목표 3종 도구를 호출한다.

## 2. 상세 계약 및 데이터/타입 명세

### 2.1 이름 매핑표

| OmOMeow 도구 | tau 현행 | 브리지 | 성공 반환 |
|---|---|---|---|
| `ask_user_question` | `ask({html}\|questions[])` | `to_legacy_ask()` 정규화 후 기존 엔진 호출 | `{answers:[{header,question,values:[]}], pending:false, questionId}` |
| `create_goal` | `goal({op:"create"})` | op 고정 주입 | `{goal:{id,objective,status,createdAt,updatedAt}}` |
| `get_goal` | `goal({op:"get"})` | op 고정 주입 | `{goal: Goal\|null}` |
| `update_goal` | `goal({op:"update"})` | op 고정 주입 + 낙관적 버전 검사 | `{goal: Goal}` |

### 2.2 입력 스키마 (Rust, camelCase 고정)

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserQuestionArgs {
    pub questions: Vec<AskQuestion>,
    #[serde(default = "default_true")] pub wait_for_answer: bool,
    #[serde(default)] pub required: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    pub header: String,
    pub question: String,
    #[serde(default)] pub multi_select: bool,
    #[serde(default)] pub options: Vec<AskOption>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskOption { pub label: String, #[serde(default)] pub description: Option<String> }
```

### 2.3 반환·오류 계약

- `pending`는 `wait_for_answer == false`일 때만 `true`이며, 이때 `questionId`가 필수다. `waitForAnswer` 기본값은 `true`.
- `required == true`이고 응답이 비어 있으면 `code:"ask_unanswered"`, `errorCode:"ASK_REQUIRED_EMPTY"`.
- `questions`가 빈 배열이면 `code:"invalid_argument"`, `errorCode:"ASK_QUESTIONS_EMPTY"`.
- `header`는 1..=64자, 중복 금지(중복 시 `errorCode:"ASK_HEADER_DUPLICATE"`). `options[].label`은 질문당 중복 금지.
- 목표 `status` 허용값: `active|completed|cancelled`. 그 외는 `errorCode:"GOAL_STATUS_INVALID"`.
- `get_goal`은 목표 미존재 시 오류가 아니라 `{goal:null}`을 반환한다(OmOMeow가 최초 실행에서 오류로 오판하지 않도록).
- `update_goal`에 `expectedUpdatedAt`이 있으면 불일치 시 `code:"conflict"`, `errorCode:"GOAL_VERSION_CONFLICT"`.

## 3. 구체적 변경/검증 로직 (Rust/TS 코드 포함)

### 3.1 Rust: 정규화 + 디스패치 등록

```rust
// crates/omp-cli/src/tools/ask.rs
pub fn to_legacy_ask(args: &AskUserQuestionArgs) -> Result<LegacyAskArgs, CmdError> {
    if args.questions.is_empty() {
        return Err(CmdError::coded("invalid_argument", "ASK_QUESTIONS_EMPTY"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(args.questions.len());
    for q in &args.questions {
        if q.header.is_empty() || q.header.chars().count() > 64 {
            return Err(CmdError::coded("invalid_argument", "ASK_HEADER_INVALID"));
        }
        if !seen.insert(q.header.as_str()) {
            return Err(CmdError::coded("invalid_argument", "ASK_HEADER_DUPLICATE"));
        }
        out.push(LegacyQuestion {
            question: format!("[{}] {}", q.header, q.question),
            header: Some(q.header.clone()),
            multi_select: q.multi_select,
            options: q.options.iter().map(|o| o.label.clone()).collect(),
        });
    }
    Ok(LegacyAskArgs { questions: out, wait_for_answer: args.wait
