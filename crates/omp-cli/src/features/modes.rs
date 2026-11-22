//! Built-in session mode tracking: `mode_changed`. TS: `modes/interactive-mode.ts`
//! (`#getSessionModeState`, `#notifyModeChanged`).
//!
//! Every source that can change a built-in mode (the `/plan` and `/loop` slash commands, a tool, RPC,
//! and session restore) reports through [`notify`] once its transition is committed. The state is
//! recomputed from the live sources and compared with the last state reported to extensions, so a
//! command that leaves every mode where it was emits nothing.
//!
//! Upstream's `SessionModeState` also carries `vibe` and `goal`; tau has no vibe mode (only the
//! `vibe_*` director tools) and no live goal runtime (`/goal` and RPC goal creation are unsupported),
//! so those two stay `inactive`, which is exactly what the session really is.

use std::sync::Arc;

use omp_ext::lifecycle::{
    GoalModeState, LifecycleEvent, LoopModeState, ModeStatus, PlanModeState, SessionModeState,
    VibeModeState,
};

use crate::runtime::Runtime;

/// The session's built-in modes as they stand right now.
#[must_use]
pub fn snapshot(session_id: &str) -> SessionModeState {
    SessionModeState {
        plan: PlanModeState {
            status: if crate::features::plan::is_active(session_id) {
                ModeStatus::Active
            } else {
                ModeStatus::Inactive
            },
            // tau keeps the captured plan in the session, not in a plan file (TS `planFilePath`).
            plan_file_path: None,
        },
        vibe: VibeModeState {
            status: ModeStatus::Inactive,
        },
        goal: GoalModeState {
            status: ModeStatus::Inactive,
            objective: None,
            token_budget: None,
        },
        r#loop: LoopModeState {
            status: if crate::features::loop_mode::is_running() {
                ModeStatus::Active
            } else {
                ModeStatus::Inactive
            },
        },
    }
}

/// Report a committed mode change: emit `mode_changed` when the state differs from the last reported
/// one (TS `#notifyModeChanged`). Observe-only, so the emit is queued without waiting for handlers.
pub fn notify(rt: &Arc<Runtime>) {
    let Some(host) = rt.ext_host() else {
        return;
    };
    let state = snapshot(&rt.session_id());
    let Some(previous) = rt.commit_mode_state(&state) else {
        return;
    };
    if !host.has_handlers("mode_changed") {
        return;
    }
    let event = LifecycleEvent::ModeChanged { previous, state };
    let sent = host.emit_nowait(&event.to_value(), None, |r| match r {
        Ok(out) => {
            for err in out.errors {
                eprintln!("omt: extension handler error: {err}");
            }
        }
        Err(e) => eprintln!("omt: extension mode_changed: {e:#}"),
    });
    if let Err(e) = sent {
        eprintln!("omt: extension mode_changed: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::{snapshot, ModeStatus, SessionModeState};

    /// The wire shape is upstream's `SessionModeState`: plan/vibe/goal/loop in that order, optional
    /// fields omitted, statuses lowercase.
    #[test]
    fn mode_state_wire_shape() {
        assert_eq!(
            serde_json::to_string(&SessionModeState::default()).unwrap(),
            r#"{"plan":{"status":"inactive"},"vibe":{"status":"inactive"},"goal":{"status":"inactive"},"loop":{"status":"inactive"}}"#
        );
        let mut state = SessionModeState::default();
        state.plan.status = ModeStatus::Active;
        state.plan.plan_file_path = Some("/tmp/plan.md".into());
        state.goal.status = ModeStatus::Paused;
        state.goal.objective = Some("ship it".into());
        state.goal.token_budget = Some(50_000);
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"plan":{"status":"active","planFilePath":"/tmp/plan.md"},"vibe":{"status":"inactive"},"goal":{"status":"paused","objective":"ship it","tokenBudget":50000},"loop":{"status":"inactive"}}"#
        );
    }

    /// A session with no plan entry and no running loop reports every mode inactive.
    #[test]
    fn snapshot_of_an_untouched_session_is_all_inactive() {
        assert_eq!(
            snapshot("session-with-no-modes"),
            SessionModeState::default()
        );
    }
}
