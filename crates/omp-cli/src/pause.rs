//! Lane hold: the gateway's pause/resume contract (`pieces/01`, `pieces/03`).
//!
//! State lives here; the `Runtime` owns one [`PauseState`] and the RPC
//! dispatcher drives it.

use std::collections::BTreeSet;

use serde_json::{json, Value};

/// `interrupt`'s `intent` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptIntent {
    /// Pause the session and hold it for the gateway (`intent: "stop"`).
    Stop,
    /// Plain abort with no hold (`intent: "abort"`, and the no-argument form).
    Abort,
}

impl InterruptIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Abort => "abort",
        }
    }
}

/// The capabilities that pin a connection's role.
const ROLE_CAPABILITIES: [&str; 2] = ["interactive_user", "task_parent"];

/// What `set_client_info` told us about the peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub width: u16,
    /// `None` until a request carried the `capabilities` key; the role is fixed at that point.
    pub capabilities: Option<BTreeSet<String>>,
    pub client_name: Option<String>,
    pub session_id: Option<String>,
    pub protocol_version: Option<i64>,
}

impl ClientInfo {
    /// Whether the peer declared `interactive_user` (required for `intent: "stop"`).
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|c| c.contains("interactive_user"))
    }
}

/// One active hold. `seq` is the session-wide pause counter at which it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRecord {
    pub kind: &'static str,
    pub key: String,
    pub since: String,
}

/// Pause/resume bookkeeping for one session.
#[derive(Debug, Clone, Default)]
pub struct PauseState {
    /// Monotonic counter: bumped on every pause and every release.
    pub seq: u64,
    pub holds: Vec<HoldRecord>,
    /// The client info as last declared.
    pub client: ClientInfo,
}

impl PauseState {
    #[must_use]
    pub fn paused(&self) -> bool {
        !self.holds.is_empty()
    }
}

/// Parse `interrupt`'s `intent`; an absent field is [`InterruptIntent::Abort`].
///
/// # Errors
/// Any value other than `stop`/`abort`.
pub fn parse_intent(cmd: &Value) -> Result<InterruptIntent, String> {
    match cmd.get("intent") {
        None | Some(Value::Null) => Ok(InterruptIntent::Abort),
        Some(Value::String(s)) if s == "stop" => Ok(InterruptIntent::Stop),
        Some(Value::String(s)) if s == "abort" => Ok(InterruptIntent::Abort),
        Some(_) => Err("intent must be \"stop\" or \"abort\".".to_string()),
    }
}

/// Parse `capabilities`: `Ok(None)` when the key is absent (the role is not fixed),
/// `Ok(Some(..))` for an array, `Err` for any other type.
///
/// # Errors
/// `capabilities` is present but not an array of strings.
pub fn parse_capabilities(cmd: &Value) -> Result<Option<BTreeSet<String>>, String> {
    match cmd.get("capabilities") {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = BTreeSet::new();
            for item in items {
                match item.as_str() {
                    Some(s) => {
                        out.insert(s.to_string());
                    }
                    None => return Err("capabilities must be an array of strings".to_string()),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err("capabilities must be an array of strings".to_string()),
    }
}

/// Apply one `set_client_info` request to `state`.
///
/// The connection role is fixed the first time a request carries `capabilities`; a later
/// request may not change the role-defining subset. Same-set re-declarations succeed.
///
/// # Errors
/// A missing `width`, or a role change after it was fixed.
pub fn set_client_info(state: &mut PauseState, cmd: &Value) -> Result<(), String> {
    let width = cmd
        .get("width")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing field `width`".to_string())?;
    let next = parse_capabilities(cmd)?;
    if let Some(next) = next {
        if let Some(prev) = state.client.capabilities.as_ref() {
            let roles = |set: &BTreeSet<String>| -> BTreeSet<String> {
                ROLE_CAPABILITIES
                    .iter()
                    .filter(|r| set.contains(**r))
                    .map(|r| (*r).to_string())
                    .collect()
            };
            if roles(prev) != roles(&next) {
                return Err(
                    "connection_role_fixed: interactive_user and task_parent cannot change after they are declared"
                        .to_string(),
                );
            }
        }
        state.client.capabilities = Some(next);
    }
    #[allow(clippy::cast_possible_truncation)] // a terminal width above u16::MAX is clamped
    {
        state.client.width = width.min(u64::from(u16::MAX)) as u16;
    }
    state.client.client_name = cmd
        .get("clientName")
        .and_then(Value::as_str)
        .map(str::to_string);
    state.client.session_id = cmd
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    state.client.protocol_version = cmd.get("protocolVersion").and_then(Value::as_i64);
    Ok(())
}

/// `intent: "stop"` requires a declared `interactive_user`.
///
/// # Errors
/// The role was never declared.
pub fn require_interactive(state: &PauseState) -> Result<(), String> {
    if state.client.is_interactive() {
        Ok(())
    } else {
        Err(
            "stop_requires_interactive_client: declare interactive_user with set_client_info first"
                .to_string(),
        )
    }
}

/// Take a `session_hold`. Idempotent while already paused (the seq does not advance).
pub fn hold(state: &mut PauseState, at: &str) -> u64 {
    if !state.paused() {
        state.seq += 1;
        let key = format!("session_hold:{}", state.seq);
        state.holds.push(HoldRecord {
            kind: "session_hold",
            key,
            since: at.to_string(),
        });
    }
    state.seq
}

/// Release every hold, bumping the seq.
pub fn release(state: &mut PauseState) -> u64 {
    state.holds.clear();
    state.seq += 1;
    state.seq
}

/// The `session_paused` frame.
#[must_use]
pub fn paused_frame(seq: u64, at: &str) -> Value {
    json!({
        "type": "session_paused",
        "pauseSeq": seq,
        "kind": "session_hold",
        "at": at,
        "noticeCount": 0,
        "persisted": true,
    })
}

/// The `session_resumed` frame.
#[must_use]
pub fn resumed_frame(seq: u64, cause: &str, at: &str) -> Value {
    json!({
        "type": "session_resumed",
        "pauseSeq": seq,
        "cause": cause,
        "at": at,
    })
}

/// The pause fields `get_state` reports.
#[must_use]
pub fn view(state: &PauseState) -> Value {
    let records: Vec<Value> = state
        .holds
        .iter()
        .map(|h| json!({"kind": h.kind, "key": h.key, "since": h.since}))
        .collect();
    json!({
        "paused": state.paused(),
        "runPaused": false,
        "pauseSeq": state.seq,
        "pausedHeldInputCount": 0,
        "pausedNoticeCount": 0,
        "holds": records,
        "pause": {"pauseSeq": state.seq, "records": records},
    })
}

/// The `custom` session entry that journals a hold transition.
#[must_use]
pub fn journal(op: &str, seq: u64, cause: Option<&str>, at: &str) -> Value {
    let mut v = json!({
        "customType": "session-hold",
        "op": op,
        "pauseSeq": seq,
        "at": at,
    });
    if let Some(cause) = cause {
        v["cause"] = json!(cause);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: u64, caps: Option<&[&str]>) -> Value {
        let mut v = json!({"width": width});
        if let Some(caps) = caps {
            v["capabilities"] = json!(caps);
        }
        v
    }

    #[test]
    fn missing_width_is_rejected() {
        let mut s = PauseState::default();
        assert_eq!(
            set_client_info(&mut s, &json!({"capabilities": []})).unwrap_err(),
            "missing field `width`"
        );
    }

    #[test]
    fn stop_requires_interactive_user() {
        let mut s = PauseState::default();
        assert!(require_interactive(&s).is_err());
        set_client_info(&mut s, &info(80, Some(&["interactive_user"]))).unwrap();
        assert!(require_interactive(&s).is_ok());
    }

    #[test]
    fn role_is_fixed_once_declared_but_same_set_passes() {
        let mut s = PauseState::default();
        set_client_info(&mut s, &info(80, None)).unwrap();
        assert!(s.client.capabilities.is_none());
        set_client_info(&mut s, &info(80, Some(&[]))).unwrap();
        let err = set_client_info(&mut s, &info(80, Some(&["interactive_user"]))).unwrap_err();
        assert!(err.starts_with("connection_role_fixed"), "{err}");
        let mut t = PauseState::default();
        set_client_info(&mut t, &info(80, Some(&["interactive_user"]))).unwrap();
        set_client_info(&mut t, &info(120, Some(&["interactive_user"]))).unwrap();
        assert_eq!(t.client.width, 120);
    }

    #[test]
    fn hold_is_idempotent_and_release_advances() {
        let mut s = PauseState::default();
        assert_eq!(hold(&mut s, "t0"), 1);
        assert_eq!(hold(&mut s, "t1"), 1, "a second stop must not advance seq");
        assert!(s.paused());
        assert_eq!(release(&mut s), 2);
        assert!(!s.paused());
        assert_eq!(view(&s)["pauseSeq"], json!(2));
        assert_eq!(view(&s)["paused"], json!(false));
    }

    #[test]
    fn intent_parsing_matches_the_measured_engine() {
        assert_eq!(parse_intent(&json!({})), Ok(InterruptIntent::Abort));
        assert_eq!(
            parse_intent(&json!({"intent": "stop"})),
            Ok(InterruptIntent::Stop)
        );
        assert_eq!(
            parse_intent(&json!({"intent": "bogus"})),
            Err("intent must be \"stop\" or \"abort\".".to_string())
        );
    }

    #[test]
    fn unknown_capability_strings_pass_through() {
        let mut s = PauseState::default();
        set_client_info(&mut s, &info(80, Some(&["bogus"]))).unwrap();
        assert!(!s.client.is_interactive());
    }
}
