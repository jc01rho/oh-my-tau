#![allow(dead_code)]
//! The `security_scan` tool (omp `SecurityScanTool`): OMP-native repository
//! security scans — plan, start, inspect, cancel, validate, plus the Codex
//! Security cloud actions.
//!
//! The tool is a thin, schema-faithful shell: it validates `action`, charges
//! the `exec` approval tier, and hands the call to a
//! [`SecurityScanBackend`]. The concrete backend is the security coordinator
//! (plan store, background jobs, canonical `security://` store); it is wired in
//! by the host through [`tool`] rather than named here, so this module owns the
//! tool contract only.
//!
//! Schema is byte-equal (as JSON) to omp's `securityScanSchema`
//! (`tests-tools/ts_tools.json`): nine `action` values, `required: ["action"]`,
//! and no `additionalProperties` key.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde_json::{json, Value};

const DESCRIPTION: &str = "OMP-native repository security scans: plan, start, inspect, cancel, validate.\n`preflight`: immutable plan pinned to repository snapshot, model, exact OAuth credential.\n`start`: plan → background OMP job.\n`status`, `cancel`: returned operation ID.\n`cloud_scans`: Codex Security cloud configurations for exact selected ChatGPT OAuth account.\n`cloud_start`: creates/enables configuration using `repository_id`, `repository_url`, `environment_id`; consumes account's separate Codex Security cloud allowance; NEVER native-scan fallback.\n`cloud_status`: cloud progress.\n`cloud_pull`: cloud findings → canonical OMP security store, available through `security://`.\nCloud actions: `cloud_configuration_id` required; `credential_id` MAY pin account.\nSecurity MUST be enabled in settings.";

/// Parameter schema, byte-equal (as JSON) to omp's `securityScanSchema`.
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "enum": [
                    "preflight",
                    "start",
                    "status",
                    "cancel",
                    "validate",
                    "cloud_scans",
                    "cloud_start",
                    "cloud_status",
                    "cloud_pull"
                ],
                "type": "string"
            },
            "plan_id": {"type": "string"},
            "operation_id": {"type": "string"},
            "target_kind": {
                "enum": ["repository", "scoped_path", "ref_diff", "working_tree"],
                "type": "string"
            },
            "include_paths": {"type": "array", "items": {"type": "string"}},
            "exclude_paths": {"type": "array", "items": {"type": "string"}},
            "base_revision": {"type": "string"},
            "head_revision": {"type": "string"},
            "knowledge_base_paths": {"type": "array", "items": {"type": "string"}},
            "output_root": {"type": "string"},
            "archive_existing": {"type": "boolean"},
            "credential_id": {"type": "integer", "minimum": 1},
            "scan_id": {"type": "string"},
            "finding_id": {"type": "string"},
            "validation_status": {
                "enum": ["unvalidated", "validated", "rejected", "partial", "error"],
                "type": "string"
            },
            "validation_summary": {"type": "string"},
            "validation_evidence": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": {"type": "string", "minLength": 1},
                        "explanation": {"type": "string"}
                    },
                    "required": ["label", "explanation"]
                }
            },
            "cloud_configuration_id": {"type": "string"},
            "repository_id": {"type": "string"},
            "repository_url": {"type": "string"},
            "environment_id": {"type": "string"},
            "lookback_days": {
                "anyOf": [
                    {"type": "integer", "minimum": 1},
                    {"const": "all"}
                ]
            }
        },
        "required": ["action"]
    })
}

/// The nine `action` values, in schema order.
pub const ACTIONS: [&str; 9] = [
    "preflight",
    "start",
    "status",
    "cancel",
    "validate",
    "cloud_scans",
    "cloud_start",
    "cloud_status",
    "cloud_pull",
];

/// One validated `security_scan` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Preflight,
    Start,
    Status,
    Cancel,
    Validate,
    CloudScans,
    CloudStart,
    CloudStatus,
    CloudPull,
}

impl Action {
    /// Parse the raw `action` parameter. A missing or non-string value is the
    /// schema's `action` requirement; an unrecognized string is rejected rather
    /// than silently mapped.
    pub fn parse(params: &Value) -> Result<Self> {
        let raw = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing string argument `action`"))?;
        match raw {
            "preflight" => Ok(Self::Preflight),
            "start" => Ok(Self::Start),
            "status" => Ok(Self::Status),
            "cancel" => Ok(Self::Cancel),
            "validate" => Ok(Self::Validate),
            "cloud_scans" => Ok(Self::CloudScans),
            "cloud_start" => Ok(Self::CloudStart),
            "cloud_status" => Ok(Self::CloudStatus),
            "cloud_pull" => Ok(Self::CloudPull),
            other => Err(anyhow!("unknown security_scan action `{other}`")),
        }
    }

    /// The wire name, as it appears in the schema enum.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Start => "start",
            Self::Status => "status",
            Self::Cancel => "cancel",
            Self::Validate => "validate",
            Self::CloudScans => "cloud_scans",
            Self::CloudStart => "cloud_start",
            Self::CloudStatus => "cloud_status",
            Self::CloudPull => "cloud_pull",
        }
    }
}

/// The security coordinator, as the tool sees it: one call per action, with
/// the whole parameter object (the coordinator reads the fields its action
/// needs, exactly like the reference implementation).
///
/// Implemented by the security feature; kept as a trait here so the tool module
/// depends on the contract and not on the coordinator's internals.
pub trait SecurityScanBackend: Send + Sync {
    /// Run one action. `action` is the parsed value; `params` is the raw
    /// parameter object.
    fn run(&self, action: Action, params: &Value) -> Result<String>;
}

/// Approval tier for a raw parameter object: `security_scan` starts jobs and
/// writes to the canonical store, so every action is `exec` — including
/// malformed input, which the schema rejects later.
#[must_use]
pub fn approval(_args: &Value) -> &'static str {
    "exec"
}

/// The `security.enabled` gate (`isToolAllowed`): security scanning is off by
/// default and the schema default is `false`.
#[must_use]
pub fn enabled(settings: &omp_settings::Settings) -> bool {
    settings.get_bool("security.enabled").unwrap_or(false)
}

/// Validate `action` and dispatch. Split out from [`AgentTool::execute`] so the
/// routing is testable without a runtime.
pub fn dispatch(backend: &dyn SecurityScanBackend, params: &Value) -> Result<String> {
    let action = Action::parse(params)?;
    backend.run(action, params)
}

/// The `security_scan` tool over `backend`.
#[must_use]
pub fn tool(backend: Arc<dyn SecurityScanBackend>) -> Arc<dyn AgentTool> {
    Arc::new(SecurityScanTool {
        schema: schema(),
        backend,
    })
}

/// The `security_scan` tool.
pub struct SecurityScanTool {
    schema: Value,
    backend: Arc<dyn SecurityScanBackend>,
}

impl AgentTool for SecurityScanTool {
    fn name(&self) -> &'static str {
        "security_scan"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Security Scan"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move {
            Ok(AgentToolResult::text(dispatch(
                self.backend.as_ref(),
                &params,
            )?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the actions it is handed and echoes a marker per action.
    struct Recorder {
        seen: Mutex<Vec<Action>>,
    }

    impl SecurityScanBackend for Recorder {
        fn run(&self, action: Action, _params: &Value) -> Result<String> {
            self.seen.lock().unwrap().push(action);
            Ok(format!("ran {}", action.as_str()))
        }
    }

    fn recorder() -> Arc<Recorder> {
        Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn schema_matches_the_reference_shape() {
        let schema = schema();
        let props = schema["properties"].as_object().unwrap();

        // Nine actions, in schema order.
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(ACTIONS),
            "action enum must list all nine actions in order"
        );
        assert_eq!(ACTIONS.len(), 9);

        // 22 parameters besides nothing else: action + 21.
        assert_eq!(props.len(), 22, "parameter count");
        assert!(props.contains_key("action"));

        // `action` is the only required parameter, and the reference schema
        // carries no `additionalProperties` key at all.
        assert_eq!(schema["required"], json!(["action"]));
        assert!(
            schema.get("additionalProperties").is_none(),
            "the reference schema has no additionalProperties key"
        );
        assert_eq!(schema["type"], "object");

        // Spot-check the shapes the reference pins.
        assert_eq!(
            schema["properties"]["target_kind"]["enum"],
            json!(["repository", "scoped_path", "ref_diff", "working_tree"])
        );
        assert_eq!(
            schema["properties"]["validation_status"]["enum"],
            json!(["unvalidated", "validated", "rejected", "partial", "error"])
        );
        assert_eq!(schema["properties"]["credential_id"]["minimum"], json!(1));
        assert_eq!(
            schema["properties"]["validation_evidence"]["items"]["required"],
            json!(["label", "explanation"])
        );
        assert_eq!(
            schema["properties"]["lookback_days"]["anyOf"],
            json!([{"type": "integer", "minimum": 1}, {"const": "all"}])
        );
        assert_eq!(
            schema["properties"]["include_paths"]["items"],
            json!({"type": "string"})
        );
        assert_eq!(schema["properties"]["archive_existing"]["type"], "boolean");
    }

    #[test]
    fn missing_action_is_an_error() {
        let err = dispatch(recorder().as_ref(), &json!({})).unwrap_err();
        assert_eq!(err.to_string(), "missing string argument `action`");

        // A non-string `action` fails the same way, not as an unknown action.
        let err = dispatch(recorder().as_ref(), &json!({"action": 7})).unwrap_err();
        assert_eq!(err.to_string(), "missing string argument `action`");
    }

    #[test]
    fn unknown_action_is_rejected() {
        let backend = recorder();
        let err = dispatch(backend.as_ref(), &json!({"action": "nope"})).unwrap_err();
        assert_eq!(err.to_string(), "unknown security_scan action `nope`");
        assert!(backend.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn approval_is_always_exec() {
        assert_eq!(approval(&json!({})), "exec");
        assert_eq!(approval(&json!({"action": "status"})), "exec");
        assert_eq!(approval(&json!({"action": "cloud_pull"})), "exec");
    }

    #[test]
    fn dispatch_routes_every_action_to_the_backend() {
        let backend = recorder();
        for name in ACTIONS {
            let out = dispatch(backend.as_ref(), &json!({"action": name})).unwrap();
            assert_eq!(out, format!("ran {name}"));
        }
        let seen = backend.seen.lock().unwrap().clone();
        let expected: Vec<Action> = ACTIONS
            .iter()
            .map(|a| Action::parse(&json!({"action": a})).unwrap())
            .collect();
        assert_eq!(
            seen, expected,
            "every action must reach the backend once, in order"
        );
    }

    #[test]
    fn tool_reports_the_reference_identity() {
        let tool = tool(recorder());
        assert_eq!(tool.name(), "security_scan");
        assert_eq!(tool.description(), DESCRIPTION);
        assert_eq!(tool.label(), "Security Scan");
        assert_eq!(tool.parameters(), &schema());
    }
}
