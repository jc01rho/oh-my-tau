//! `/plan`: plan mode (read-only planning, then execute on approval). TS: plan-mode/, slash-commands/builtin-modes.ts.
//!
//! `/plan [task]` enters plan mode: the agent is told it is planning and the
//! side-effecting tools (`bash`/`edit`/`write`/…) are blocked by a tool-call
//! gate while the mode is active. The produced plan is appended to the session
//! (a `custom` entry) and shown. `/plan approve` (or `/plan go`) leaves the
//! mode and sends the saved plan as the next prompt to execute; `/plan off`
//! leaves without executing; `/plan status` reports the mode.
//!
//! Plan state lives in a process-wide map keyed by session id (the gate is
//! process-wide and reflects the single live runtime/session).
//!
//! A captured plan opens a plan review (TS `interactive-mode.ts`
//! `handlePlanApproval`): extensions get `plan_review_requested`, may answer it
//! through `ctx.resolvePlanReview`, and see `plan_review_resolved` when it
//! closes (`/plan approve`, `/plan off`, a replacing review, a session switch,
//! shutdown).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use omp_ext::lifecycle::{PlanReviewBy, PlanReviewChoice, PlanReviewOption, PlanReviewRequest};
use pa_agent::agent_loop::BeforeToolCallFn;
use pa_agent::types::BeforeToolCallResult;
use serde_json::json;

use crate::rpc::slash::Outcome;
use crate::runtime::Runtime;

/// Tools allowed while planning. Everything else (write side effects: `bash`,
/// `edit`, `write`, `eval`, `ast_edit`, `task`, `checkpoint`, `rewind`,
/// `wait`, …) is refused with a clear error. `ast_grep` is the read-only
/// structural-search half of `ast`; `ast_edit` rewrites files and is blocked.
const ALLOWED_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "find",
    "ast_grep",
    "lsp",
    "web_search",
    "think",
    "todo",
    "ask",
];

/// Whether `name` may run during plan mode.
#[must_use]
pub fn is_plan_tool(name: &str) -> bool {
    ALLOWED_TOOLS.contains(&name)
}

/// Per-session plan state.
#[derive(Debug, Clone, Default)]
struct PlanSessionState {
    enabled: bool,
    task: String,
    plan: String,
    /// The open plan review awaiting an answer.
    review: Option<PlanReviewRequest>,
}

/// Receives plan-review transitions (the extension host; see `ext.rs`).
pub trait ReviewSink: Send + Sync {
    /// A review opened (`plan_review_requested`; becomes `ctx.getPendingPlanReview()`).
    fn opened(&self, request: &PlanReviewRequest);
    /// A review closed (`plan_review_resolved`; clears the pending review).
    fn closed(
        &self,
        review_id: &str,
        session_id: &str,
        choice: Option<PlanReviewChoice>,
        by: PlanReviewBy,
    );
}

fn sink() -> Option<&'static dyn ReviewSink> {
    crate::rpc::slash::extensions().map(|e| e.as_ref() as &dyn ReviewSink)
}

/// The choices tau's plan review offers, in TS order. `execute` (`/plan approve`) and
/// `refine` (feedback becomes the next planning prompt) are supported; tau has no
/// compact-then-execute, keep-context, or save-to-file approval, so those are disabled.
fn review_options() -> Vec<PlanReviewOption> {
    let opt = |id, label: &str, disabled| PlanReviewOption {
        id,
        label: label.to_string(),
        disabled,
    };
    vec![
        opt(PlanReviewChoice::Execute, "Approve and execute", false),
        opt(
            PlanReviewChoice::Compact,
            "Approve and compact context",
            true,
        ),
        opt(PlanReviewChoice::Keep, "Approve and keep context", true),
        opt(PlanReviewChoice::Refine, "Refine plan", false),
        opt(PlanReviewChoice::Save, "Save", true),
    ]
}

/// TS `normalizePlanTitle`: `None` when the title is unusable.
fn normalize_plan_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
    {
        return None;
    }
    let without_ext = if trimmed.to_ascii_lowercase().ends_with(".md") {
        &trimmed[..trimmed.len() - 3]
    } else {
        trimmed
    };
    let mut out = String::new();
    let mut in_space = false;
    for c in without_ext.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push('-');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    for c in out.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let sanitized = collapsed.trim_matches('-');
    (!sanitized.is_empty()).then(|| sanitized.to_string())
}

/// TS `resolvePlanTitle` without a supplied title: the first `# heading`, else `plan`.
fn plan_title(plan: &str) -> String {
    plan.lines()
        .find_map(|line| {
            let rest = line.trim_start_matches([' ', '\t']).strip_prefix('#')?;
            rest.starts_with([' ', '\t'])
                .then(|| rest.trim())
                .filter(|h| !h.is_empty())
        })
        .and_then(normalize_plan_title)
        .unwrap_or_else(|| "plan".to_string())
}

/// TS `planFileUrlForSlug`: `local://<slug>-plan.md` (a trailing `-plan` of the title is dropped).
fn plan_file_name(title: &str) -> String {
    let slug = if title.to_ascii_lowercase().ends_with("-plan") && title.len() > 5 {
        &title[..title.len() - 5]
    } else {
        title
    };
    format!("{slug}-plan.md")
}

/// A random RFC 4122 v4 id (TS `crypto.randomUUID()`).
fn review_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The `plan_review_requested` payload for a captured plan. tau has no execution-tier
/// slider, so `tiers` is omitted (fewer than two selectable tiers).
fn review_request(session_id: &str, plan: &str) -> PlanReviewRequest {
    let title = plan_title(plan);
    PlanReviewRequest {
        review_id: review_id(),
        session_id: session_id.to_string(),
        plan_file_path: format!("local://{}", plan_file_name(&title)),
        title,
        plan_content: plan.to_string(),
        options: review_options(),
        tiers: None,
    }
}

/// Open `request` as the session's review; a review it replaces closes first, unanswered.
fn open_review(sink: Option<&dyn ReviewSink>, request: PlanReviewRequest) {
    let replaced = {
        let mut guard = lock_state();
        let entry = guard.entry(request.session_id.clone()).or_default();
        entry.review.replace(request.clone())
    };
    if let Some(sink) = sink {
        if let Some(old) = replaced {
            sink.closed(&old.review_id, &old.session_id, None, PlanReviewBy::Local);
        }
        sink.opened(&request);
    }
}

/// Close the session's open review (if any) with `choice`.
fn close_review(
    sink: Option<&dyn ReviewSink>,
    session_id: &str,
    choice: Option<PlanReviewChoice>,
    by: PlanReviewBy,
) {
    let taken = lock_state()
        .get_mut(session_id)
        .and_then(|p| p.review.take());
    if let (Some(sink), Some(review)) = (sink, taken) {
        sink.closed(&review.review_id, &review.session_id, choice, by);
    }
}

/// Close every open review unanswered (session switch, shutdown).
pub fn cancel_reviews(sink: Option<&dyn ReviewSink>) {
    let taken: Vec<PlanReviewRequest> = lock_state()
        .values_mut()
        .filter_map(|p| p.review.take())
        .collect();
    if let Some(sink) = sink {
        for review in taken {
            sink.closed(
                &review.review_id,
                &review.session_id,
                None,
                PlanReviewBy::Local,
            );
        }
    }
}

/// The session's open review (the host serves `ctx.getPendingPlanReview()` from its mirror).
#[cfg(test)]
fn pending_review(session_id: &str) -> Option<PlanReviewRequest> {
    lock_state().get(session_id).and_then(|p| p.review.clone())
}

/// TS `resolvePlanReview` acceptance rules: the review must be the open one, the choice
/// offered and enabled, `refine` needs non-blank feedback, and `role` must name a selectable
/// tier of an approval (tau offers none, so any `role` is refused). Takes the review on success.
fn accept_answer(
    session_id: &str,
    review_id: &str,
    choice: &str,
    feedback: Option<&str>,
    role: Option<&str>,
) -> Option<(PlanReviewRequest, PlanReviewChoice)> {
    let mut guard = lock_state();
    let state = guard.get_mut(session_id)?;
    let review = state.review.as_ref().filter(|r| r.review_id == review_id)?;
    let choice = PlanReviewChoice::parse(choice)?;
    let option = review.options.iter().find(|o| o.id == choice)?;
    if choice == PlanReviewChoice::Refine && feedback.is_none_or(|f| f.trim().is_empty()) {
        return None;
    }
    if role.is_some() || option.disabled {
        return None;
    }
    state.review.take().map(|r| (r, choice))
}

/// `ctx.resolvePlanReview(reviewId, choice, input)` for the live session: answers the open
/// review as if the operator picked `choice`. `execute` runs the plan like `/plan approve`;
/// `refine` sends `feedback` as the next planning prompt and reviews the revised plan.
pub fn resolve_review(
    rt: &Arc<Runtime>,
    review_id: &str,
    choice: &str,
    feedback: Option<&str>,
    role: Option<&str>,
) -> bool {
    let session_id = rt.session_id();
    let Some((review, choice)) = accept_answer(&session_id, review_id, choice, feedback, role)
    else {
        return false;
    };
    if let Some(sink) = sink() {
        sink.closed(
            &review.review_id,
            &review.session_id,
            Some(choice),
            PlanReviewBy::Extension,
        );
    }
    let rt = rt.clone();
    let feedback = feedback.unwrap_or_default().to_string();
    tokio::spawn(async move {
        if choice == PlanReviewChoice::Refine {
            let task = lock_state()
                .get(&session_id)
                .map(|p| p.task.clone())
                .unwrap_or_default();
            if let Err(message) = plan_turn(&rt, &session_id, &task, &feedback).await {
                output(&rt, &format!("plan: failed to refine plan: {message}"));
            }
        } else {
            let _ = approve(&rt).await;
        }
    });
    true
}

static PLAN_STATE: OnceLock<Mutex<HashMap<String, PlanSessionState>>> = OnceLock::new();

fn state() -> &'static Mutex<HashMap<String, PlanSessionState>> {
    PLAN_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_state() -> std::sync::MutexGuard<'static, HashMap<String, PlanSessionState>> {
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Is any session currently in plan mode? The gate (installed per agent via
/// [`gate`]) consults this so it never needs the session id itself.
fn is_any_plan_active() -> bool {
    lock_state().values().any(|p| p.enabled)
}

/// Wrap the agent's `before_tool_call` to refuse side-effecting tools while
/// plan mode is active. Composed after [`crate::tools::relevance::wrap_before`]
/// in `agent::build_agent`.
#[must_use]
pub fn gate(inner: Option<BeforeToolCallFn>) -> BeforeToolCallFn {
    Arc::new(move |ctx, signal| {
        if is_any_plan_active() && !is_plan_tool(&ctx.tool_call.name) {
            let reason = format!(
                "Plan mode is read-only: `{}` is blocked. Use the read-only tools \
                 (read/search/find/ast/lsp/web_search/think/todo/ask) to investigate, \
                 then run /plan approve to execute.",
                ctx.tool_call.name
            );
            let blocked = BeforeToolCallResult {
                block: true,
                reason: Some(reason),
                args: None,
                additional_context: None,
            };
            return Box::pin(async move { Ok(Some(blocked)) });
        }
        match &inner {
            Some(f) => f(ctx, signal),
            None => Box::pin(async { Ok(None) }),
        }
    })
}

/// Parsed `/plan` invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanCommand {
    /// Bare `/plan`: toggle the mode on (or report when already on).
    Toggle,
    /// `/plan approve` / `/plan go`: leave plan mode and run the saved plan.
    Approve,
    /// `/plan off`: leave plan mode without executing.
    Off,
    /// `/plan status`: report the mode.
    Status,
    /// `/plan <task>`: enter plan mode and plan this task.
    Task(String),
}

/// Parse `/plan` arguments (TS `handlePlanModeCommand` subcommand set). The first
/// whitespace-delimited token selects the subcommand; anything else is a task.
#[must_use]
pub fn parse_args(args: &str) -> PlanCommand {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return PlanCommand::Toggle;
    }
    match trimmed.split_whitespace().next().unwrap_or_default() {
        "approve" | "go" => PlanCommand::Approve,
        "off" => PlanCommand::Off,
        "status" | "show" => PlanCommand::Status,
        _ => PlanCommand::Task(trimmed.to_string()),
    }
}

pub async fn command(rt: &Arc<Runtime>, args: &str) -> Outcome {
    match parse_args(args) {
        PlanCommand::Task(task) => enter_and_plan(rt, task).await,
        PlanCommand::Approve => approve(rt).await,
        PlanCommand::Off => leave(rt),
        PlanCommand::Status => status(rt),
        PlanCommand::Toggle => toggle(rt),
    }
}

fn output(rt: &Runtime, text: &str) {
    rt.send(json!({"type": "command_output", "text": text}));
}

fn handled() -> Outcome {
    Outcome::Handled {
        agent_invoked: false,
    }
}

fn set_session(session_id: &str, enabled: bool, task: &str) {
    let mut guard = lock_state();
    let entry = guard.entry(session_id.to_string()).or_default();
    entry.enabled = enabled;
    if !task.is_empty() {
        entry.task = task.to_string();
    }
}

/// Whether `session_id` is in plan mode (the `plan` half of `mode_changed`).
#[must_use]
pub fn is_active(session_id: &str) -> bool {
    lock_state().get(session_id).is_some_and(|p| p.enabled)
}

/// The TS plan-mode instruction, prefixing the submitted planning task.
fn instruction(task: &str) -> String {
    format!(
        "<critical>\nPlan mode active.\n- Working tree/system read-only: NEVER create, edit, \
         delete, or rename working-tree files; NEVER run state-changing commands (`git commit`, \
         `npm install`, migrations) or otherwise change the system.\n- Use only the read-only \
         investigation tools (read/search/find/ast/lsp/web_search/think/todo/ask).\n- Produce a \
         concrete implementation plan as your final message.\n</critical>\n\nPlanning task:\n{task}"
    )
}

/// `/plan <task>`: enable the gate, submit the task with the planning
/// instruction, wait for the planning turn, then save + show the plan.
async fn enter_and_plan(rt: &Arc<Runtime>, task: String) -> Outcome {
    let session_id = rt.session_id();
    if is_any_plan_active() {
        output(
            rt,
            "Plan mode is already active. Run /plan approve, /plan off, or /plan status.",
        );
        return handled();
    }
    set_session(&session_id, true, &task);
    crate::features::modes::notify(rt);
    if let Err(message) = plan_turn(rt, &session_id, &task, &instruction(&task)).await {
        set_session(&session_id, false, "");
        crate::features::modes::notify(rt);
        output(rt, &format!("plan: failed to start planning: {message}"));
    }
    handled()
}

/// Write the plan to its `local://` file (TS keeps plans there; the review reports the path).
fn write_plan_file(rt: &Runtime, request: &PlanReviewRequest) {
    let ctx = omp_urls::UrlCtx {
        session_file: rt.live_session_file(),
        ..Default::default()
    };
    let root = omp_urls::schemes::local::resolve_local_root(&ctx);
    let name = request
        .plan_file_path
        .strip_prefix("local://")
        .unwrap_or(&request.plan_file_path);
    let written = std::fs::create_dir_all(&root)
        .and_then(|()| std::fs::write(root.join(name), &request.plan_content));
    if let Err(e) = written {
        output(
            rt,
            &format!("plan: failed to write {}: {e}", request.plan_file_path),
        );
    }
}

/// One planning turn: submit `prompt` (plan mode stays on), wait for it, then
/// save + show the plan and open its review.
async fn plan_turn(
    rt: &Arc<Runtime>,
    session_id: &str,
    task: &str,
    prompt: &str,
) -> Result<(), String> {
    rt.prompt(None, prompt, Some("steer"), Vec::new())
        .await
        .map_err(|e| e.message)?;
    rt.wait_settled().await;
    let session_id = session_id.to_string();
    let task = task.to_string();
    let plan = rt.last_assistant_text().await.unwrap_or_default();
    {
        let mut guard = lock_state();
        if let Some(p) = guard.get_mut(&session_id) {
            p.plan.clone_from(&plan);
        }
    }
    let _ = rt.append_entry(
        "custom",
        json!({"customType": "plan", "data": {"task": task, "plan": plan}}),
    );
    output(rt, "─ Plan ─────────────────────────────");
    if plan.trim().is_empty() {
        output(rt, "(no plan text produced)");
    } else {
        output(rt, &plan);
    }
    output(rt, "─────────────────────────────────────");
    output(
        rt,
        "Plan mode: ON. Review the plan above, then /plan approve to execute or /plan off to cancel.",
    );
    if !plan.trim().is_empty() {
        let request = review_request(&session_id, &plan);
        write_plan_file(rt, &request);
        open_review(sink(), request);
    }
    Ok(())
}

/// `/plan approve` / `/plan go`: leave plan mode and send the saved plan as the
/// next prompt.
async fn approve(rt: &Arc<Runtime>) -> Outcome {
    let session_id = rt.session_id();
    let (plan, task) = {
        let guard = lock_state();
        match guard.get(&session_id) {
            Some(p) if p.enabled => (p.plan.clone(), p.task.clone()),
            _ => (String::new(), String::new()),
        }
    };
    if plan.trim().is_empty() {
        output(rt, "No plan to approve yet. Run /plan <task> first.");
        return handled();
    }
    set_session(&session_id, false, &task);
    crate::features::modes::notify(rt);
    close_review(
        sink(),
        &session_id,
        Some(PlanReviewChoice::Execute),
        PlanReviewBy::Local,
    );
    if let Err(e) = rt.prompt(None, &plan, Some("steer"), Vec::new()).await {
        output(rt, &format!("plan: failed to submit plan: {}", e.message));
        return handled();
    }
    output(rt, "Plan approved. Plan mode off; executing the plan.");
    handled()
}

/// `/plan off`: leave plan mode without executing.
fn leave(rt: &Arc<Runtime>) -> Outcome {
    let session_id = rt.session_id();
    let was_active = {
        let mut guard = lock_state();
        let was = guard.get(&session_id).is_some_and(|p| p.enabled);
        if was {
            if let Some(p) = guard.get_mut(&session_id) {
                p.enabled = false;
            }
        }
        was
    };
    crate::features::modes::notify(rt);
    close_review(sink(), &session_id, None, PlanReviewBy::Local);
    output(
        rt,
        if was_active {
            "Plan mode disabled (plan not executed)."
        } else {
            "Plan mode is not active."
        },
    );
    handled()
}

/// `/plan status`.
fn status(rt: &Runtime) -> Outcome {
    let session_id = rt.session_id();
    let state = lock_state().get(&session_id).cloned();
    match state {
        Some(p) if p.enabled => {
            output(rt, "Plan mode: ON");
            if !p.task.is_empty() {
                output(rt, &format!("Task: {}", p.task));
            }
            if p.plan.trim().is_empty() {
                output(rt, "Plan: none captured yet");
            } else {
                output(
                    rt,
                    "Plan: captured (run /plan approve to execute, /plan off to cancel)",
                );
            }
        }
        _ => output(rt, "Plan mode: OFF"),
    }
    handled()
}

/// Bare `/plan`: enable plan mode (no task submitted; the next prompt still
/// plans because the gate stays active).
fn toggle(rt: &Arc<Runtime>) -> Outcome {
    let session_id = rt.session_id();
    if is_any_plan_active() {
        output(
            rt,
            "Plan mode is already active. Run /plan <task>, /plan approve, or /plan off.",
        );
        return handled();
    }
    set_session(&session_id, true, "");
    crate::features::modes::notify(rt);
    output(
        rt,
        "Plan mode: ON. Submit a task to plan, or /plan off to disable.",
    );
    handled()
}

#[cfg(test)]
mod review_tests {
    use super::*;

    /// Records sink calls as their wire events.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<serde_json::Value>>);

    impl ReviewSink for Recorder {
        fn opened(&self, request: &PlanReviewRequest) {
            let ev = omp_ext::lifecycle::LifecycleEvent::PlanReviewRequested(request.clone());
            self.0.lock().unwrap().push(ev.to_value());
        }
        fn closed(
            &self,
            review_id: &str,
            session_id: &str,
            choice: Option<PlanReviewChoice>,
            by: PlanReviewBy,
        ) {
            let ev = omp_ext::lifecycle::LifecycleEvent::PlanReviewResolved {
                review_id: review_id.into(),
                session_id: session_id.into(),
                choice,
                by,
            };
            self.0.lock().unwrap().push(ev.to_value());
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<serde_json::Value> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    /// `cancel_reviews` touches every session, so the review tests run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn open(rec: &Recorder, session: &str, plan: &str) -> PlanReviewRequest {
        set_session(session, true, "task");
        let request = review_request(session, plan);
        open_review(Some(rec), request.clone());
        request
    }

    #[test]
    fn requested_payload_matches_ts_shape() {
        let _g = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rec = Recorder::default();
        let req = open(&rec, "pr-shape", "intro\n# Migrate the auth layer\n- step");
        let events = rec.take();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        let keys: Vec<&str> = ev.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "type",
                "reviewId",
                "sessionId",
                "title",
                "planFilePath",
                "planContent",
                "options"
            ],
            "field order of handlePlanApproval; tiers omitted"
        );
        assert_eq!(ev["type"], "plan_review_requested");
        assert_eq!(ev["reviewId"], req.review_id.as_str());
        let id = req.review_id.as_bytes();
        assert_eq!((req.review_id.len(), id[14], id[8]), (36, b'4', b'-'));
        assert_eq!(ev["sessionId"], "pr-shape");
        assert_eq!(ev["title"], "Migrate-the-auth-layer");
        assert_eq!(ev["planFilePath"], "local://Migrate-the-auth-layer-plan.md");
        assert_eq!(ev["planContent"], "intro\n# Migrate the auth layer\n- step");
        assert_eq!(
            ev["options"],
            json!([
                {"id": "execute", "label": "Approve and execute", "disabled": false},
                {"id": "compact", "label": "Approve and compact context", "disabled": true},
                {"id": "keep", "label": "Approve and keep context", "disabled": true},
                {"id": "refine", "label": "Refine plan", "disabled": false},
                {"id": "save", "label": "Save", "disabled": true},
            ])
        );
        assert_eq!(pending_review("pr-shape"), Some(req));
        close_review(Some(&rec), "pr-shape", None, PlanReviewBy::Local);
    }

    #[test]
    fn title_falls_back_like_resolve_plan_title() {
        assert_eq!(plan_title("no heading here"), "plan");
        assert_eq!(plan_title("## sub only\n#nospace"), "plan");
        assert_eq!(plan_title("# ../etc"), "plan");
        assert_eq!(plan_title("  # Auth Plan.md "), "Auth-Plan");
        assert_eq!(plan_file_name("Auth-Plan"), "Auth-plan.md");
        assert_eq!(plan_file_name("plan"), "plan-plan.md");
    }

    #[test]
    fn local_approve_and_off_resolve_locally() {
        let _g = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rec = Recorder::default();
        let req = open(&rec, "pr-local", "# A");
        rec.take();
        close_review(
            Some(&rec),
            "pr-local",
            Some(PlanReviewChoice::Execute),
            PlanReviewBy::Local,
        );
        assert_eq!(
            rec.take(),
            [
                json!({"type": "plan_review_resolved", "reviewId": req.review_id,
                    "sessionId": "pr-local", "choice": "execute", "by": "local"})
            ]
        );
        assert_eq!(pending_review("pr-local"), None);

        let req = open(&rec, "pr-local", "# B");
        rec.take();
        close_review(Some(&rec), "pr-local", None, PlanReviewBy::Local);
        assert_eq!(
            rec.take(),
            [
                json!({"type": "plan_review_resolved", "reviewId": req.review_id,
                    "sessionId": "pr-local", "by": "local"})
            ],
            "a dismissed review carries no choice"
        );
        close_review(Some(&rec), "pr-local", None, PlanReviewBy::Local);
        assert!(rec.take().is_empty(), "nothing open: nothing resolved");
    }

    #[test]
    fn replacing_review_and_cancel_close_unanswered() {
        let _g = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rec = Recorder::default();
        let first = open(&rec, "pr-replace", "# One");
        let second = open(&rec, "pr-replace", "# Two");
        let events = rec.take();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1]["type"], "plan_review_resolved");
        assert_eq!(events[1]["reviewId"], first.review_id.as_str());
        assert!(events[1].get("choice").is_none());
        assert_eq!(events[2]["reviewId"], second.review_id.as_str());

        cancel_reviews(Some(&rec));
        let events = rec.take();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reviewId"], second.review_id.as_str());
        assert_eq!(events[0]["by"], "local");
        assert!(events[0].get("choice").is_none());
        assert_eq!(pending_review("pr-replace"), None);
    }

    #[test]
    fn extension_answers_follow_resolve_plan_review_rules() {
        let _g = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rec = Recorder::default();
        let req = open(&rec, "pr-ext", "# Ext");
        let id = req.review_id.as_str();
        let refused = |choice: &str, feedback: Option<&str>, role: Option<&str>| {
            accept_answer("pr-ext", id, choice, feedback, role).is_none()
        };
        assert!(accept_answer("pr-ext", "other-id", "execute", None, None).is_none());
        assert!(accept_answer("pr-none", id, "execute", None, None).is_none());
        assert!(refused("bogus", None, None));
        assert!(refused("compact", None, None), "disabled choice");
        assert!(refused("keep", None, None), "disabled choice");
        assert!(refused("save", None, None), "disabled choice");
        assert!(refused("refine", None, None), "refine needs feedback");
        assert!(refused("refine", Some("  "), None), "blank feedback");
        assert!(refused("execute", None, Some("smol")), "no selectable tier");
        assert_eq!(
            pending_review("pr-ext").as_ref(),
            Some(&req),
            "refusals keep it open"
        );

        let (taken, choice) = accept_answer("pr-ext", id, "refine", Some("more tests"), None)
            .expect("refine with feedback is accepted");
        assert_eq!((taken, choice), (req.clone(), PlanReviewChoice::Refine));
        assert_eq!(pending_review("pr-ext"), None);
        assert!(
            accept_answer("pr-ext", id, "execute", None, None).is_none(),
            "an answered review takes no second answer"
        );

        let req = open(&rec, "pr-ext", "# Ext 2");
        let (_, choice) = accept_answer("pr-ext", &req.review_id, "execute", None, None).unwrap();
        assert_eq!(choice, PlanReviewChoice::Execute);
    }
}

#[cfg(test)]
mod tests {
    use super::{is_active, is_plan_tool, parse_args, set_session, PlanCommand};
    use omp_ext::lifecycle::ModeStatus;

    #[test]
    fn parses_subcommands() {
        assert_eq!(parse_args(""), PlanCommand::Toggle);
        assert_eq!(parse_args("   "), PlanCommand::Toggle);
        assert_eq!(parse_args("approve"), PlanCommand::Approve);
        assert_eq!(parse_args("go"), PlanCommand::Approve);
        assert_eq!(parse_args("off"), PlanCommand::Off);
        assert_eq!(parse_args("status"), PlanCommand::Status);
        assert_eq!(parse_args(" show "), PlanCommand::Status);
    }

    #[test]
    fn parses_task() {
        assert_eq!(
            parse_args("refactor X"),
            PlanCommand::Task("refactor X".into())
        );
        assert_eq!(
            parse_args("  plan the migration  "),
            PlanCommand::Task("plan the migration".into())
        );
    }

    #[test]
    fn allowlist_allows_read_only_tools() {
        for name in [
            "read",
            "glob",
            "grep",
            "find",
            "ast_grep",
            "lsp",
            "web_search",
            "think",
            "todo",
            "ask",
        ] {
            assert!(is_plan_tool(name), "{name} should be allowed in plan mode");
        }
    }

    #[test]
    fn allowlist_blocks_side_effecting_tools() {
        for name in [
            "bash",
            "edit",
            "write",
            "eval",
            "ast_edit",
            "task",
            "checkpoint",
            "rewind",
            "wait",
        ] {
            assert!(!is_plan_tool(name), "{name} should be blocked in plan mode");
        }
    }

    /// Plan mode is per session: entering it for one session reports `active` for that session and
    /// `inactive` for any other, so a restored or switched-to session reports its own modes.
    #[test]
    fn plan_mode_is_reported_per_session() {
        set_session("modes-test-entered", true, "");
        assert!(is_active("modes-test-entered"));
        assert!(!is_active("modes-test-other"));
        assert_eq!(
            crate::features::modes::snapshot("modes-test-entered")
                .plan
                .status,
            ModeStatus::Active
        );
        assert_eq!(
            crate::features::modes::snapshot("modes-test-other")
                .plan
                .status,
            ModeStatus::Inactive
        );
        set_session("modes-test-entered", false, "");
        assert!(!is_active("modes-test-entered"));
    }
}
