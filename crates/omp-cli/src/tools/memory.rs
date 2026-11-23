//! `memory` and `memory_apply_patch`: the OmOMeow gateway's git-backed memory
//! tools, over `omp_memory`'s markdown repository.
//!
//! The repository is bound by the environment, not by `memory.backend` (that
//! setting selects the unrelated Mnemopi store): `MEMORY_DIR` names the git work
//! tree and `AGENT_ID` the commit identity (`<id>@omo.local`, default `omt`).
//! Without a `MEMORY_DIR` that is a git repository the tools are not advertised.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use omp_memory::{MemoryParams, MemoryRepo, WriteOutcome};
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde_json::{json, Value};

const MEMORY_DESCRIPTION: &str = "A convenience tool for memories stored in the omo memory repo that automatically commits changes. The harness syncs clean committed memory changes after the turn.\n\nMemory files are markdown documents with YAML frontmatter. Frontmatter carries a `description` (required on create; it is what the memory index shows) and may set `read_only: \"true\"` to block modification. Edits preserve existing frontmatter.\nTime-boxed content: its own system/ file with frontmatter expires: <UTC date or ISO instant with Z/offset>, written via memory_apply_patch; it stops being projected then.\n\nSupported operations on memory files:\n- `str_replace`\n- `insert`\n- `delete` (files, or directories recursively)\n- `rename` (path rename only)\n- `update_description`\n- `create`\nFor larger reorganizations, use memory_apply_patch instead.\n\nPath formats accepted:\n- relative memory file paths (e.g. `system/contacts.md`, `reference/project/team.md`)\n- absolute paths only when they are inside the memory repo\n\nNote: absolute paths outside the memory repo are rejected.\n\nWhen creating or deleting files, check for `[[path]]` references in other memory files that may need to be added or updated. Keeping references consistent ensures future discoverability.\n\nOn success the tool returns `Memory <command> committed locally (<sha>).`, or `Memory <command> committed (<sha>); harness will sync after the turn.` when a remote is configured. Commits are authored with the bound omo memory identity.";

const APPLY_PATCH_DESCRIPTION: &str = "Apply a codex-style patch to memory files in the omo memory repo, then automatically commit the change. The harness syncs clean committed memory changes after the turn.\n\nThis is similar to `apply_patch`, but scoped to the memory repo and with memory-aware guardrails.\n\n- Required args:\n  - `reason` — git commit message for the memory change\n  - `input` — patch text using the standard apply_patch format\n\nPatch format:\n- `*** Begin Patch`\n- `*** Add File: <path>`\n- `*** Update File: <path>`\n  - optional `*** Move to: <path>`\n  - one or more `@@` hunks with ` `, `-`, `+` lines\n- `*** Delete File: <path>`\n- `*** End Patch`\n\nPath rules:\n- Relative paths are interpreted inside the memory repo\n- Absolute paths are allowed only when under the memory repo\n- Paths outside the memory repo are rejected\n\nMemory rules:\n- Operates on markdown memory files (`.md`) with YAML frontmatter\n- Updated/deleted files must be valid memory files with frontmatter\n- `read_only: \"true\"` files cannot be modified\n- If adding a file without frontmatter, frontmatter is created automatically\n\nGit behavior:\n- Stages changed memory paths\n- Commits with `reason`, authored by the bound omo memory identity (`<identity>@omo.local`)\n- Sync to a configured remote is handled by the harness after the turn\n\nOn success the tool returns `memory_apply_patch committed locally (<sha>).`, or `memory_apply_patch committed (<sha>); harness will sync after the turn.` when a remote is configured.";

/// Parameter schema of `memory`.
#[must_use]
pub fn memory_schema() -> Value {
    json!({
        "type": "object",
        "required": ["command", "reason"],
        "properties": {
            "command": {"enum": ["create", "str_replace", "insert", "delete", "rename", "update_description"]},
            "reason": {"type": "string"},
            "file_path": {"type": "string"},
            "old_path": {"type": "string"},
            "new_path": {"type": "string"},
            "old_string": {"type": "string"},
            "new_string": {"type": "string"},
            "insert_line": {"type": "number"},
            "insert_text": {"type": "string"},
            "description": {"type": "string"},
            "file_text": {"type": "string"},
            "source": {"type": "string", "enum": ["user", "agent", "inferred"]},
            "user_quote": {"type": "string"},
        },
    })
}

/// Parameter schema of `memory_apply_patch`.
#[must_use]
pub fn apply_patch_schema() -> Value {
    json!({
        "type": "object",
        "required": ["reason", "input"],
        "properties": {
            "reason": {"type": "string"},
            "input": {"type": "string"},
            "source": {"type": "string", "enum": ["user", "agent", "inferred"]},
            "user_quote": {"type": "string"},
        },
    })
}

fn text_field(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[allow(clippy::cast_possible_truncation)]
fn memory_params(args: &Value) -> MemoryParams {
    MemoryParams {
        command: text_field(args, "command"),
        reason: text_field(args, "reason").unwrap_or_default(),
        file_path: text_field(args, "file_path"),
        file_text: text_field(args, "file_text"),
        description: text_field(args, "description"),
        old_string: text_field(args, "old_string"),
        new_string: text_field(args, "new_string"),
        insert_line: args
            .get("insert_line")
            .and_then(Value::as_f64)
            .map(|n| n.floor() as i64),
        insert_text: text_field(args, "insert_text"),
        old_path: text_field(args, "old_path"),
        new_path: text_field(args, "new_path"),
    }
}

fn outcome_result(outcome: &WriteOutcome) -> AgentToolResult {
    AgentToolResult::text(outcome.message.clone())
}

/// Which memory tool an instance serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Memory,
    ApplyPatch,
}

/// One of the two repository tools, bound to a [`MemoryRepo`].
pub struct MemoryRepoTool {
    kind: Kind,
    repo: MemoryRepo,
    schema: Value,
}

impl MemoryRepoTool {
    /// Runs the tool synchronously (the async adapter wraps this).
    ///
    /// # Errors
    /// The `memory:` / `memory_apply_patch:` failure text for a refused call.
    #[cfg(test)]
    pub fn run(&self, args: &Value) -> Result<AgentToolResult> {
        self.run_when(args, &|| false)
    }

    fn run_when(&self, args: &Value, cancelled: &dyn Fn() -> bool) -> Result<AgentToolResult> {
        let outcome = match self.kind {
            Kind::Memory => {
                omp_memory::ops::run_memory_when(&self.repo, &memory_params(args), cancelled)
            }
            Kind::ApplyPatch => omp_memory::patch::run_apply_patch_when(
                &self.repo,
                &text_field(args, "reason").unwrap_or_default(),
                &text_field(args, "input").unwrap_or_default(),
                cancelled,
            ),
        };
        outcome
            .map(|o| outcome_result(&o))
            .map_err(|e| anyhow!("{e}"))
    }
}

impl AgentTool for MemoryRepoTool {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "memory",
            Kind::ApplyPatch => "memory_apply_patch",
        }
    }
    fn description(&self) -> &str {
        match self.kind {
            Kind::Memory => MEMORY_DESCRIPTION,
            Kind::ApplyPatch => APPLY_PATCH_DESCRIPTION,
        }
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "Memory",
            Kind::ApplyPatch => "Memory Patch",
        }
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move {
            if signal.is_aborted() {
                return Err(pa_agent::abort::aborted_error());
            }
            let worker = signal.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                self.run_when(&params, &|| worker.is_aborted())
            })
            .await
            .map_err(|e| anyhow!("memory task failed: {e}"))?;
            // A finished commit is reported even if the call was cancelled late;
            // only a write the cancellation actually prevented becomes an abort.
            match outcome {
                Err(_) if signal.is_aborted() => Err(pa_agent::abort::aborted_error()),
                other => other,
            }
        })
    }
}

/// Both tools over `repo`.
#[must_use]
pub fn tools_for(repo: &MemoryRepo) -> Vec<Arc<dyn AgentTool>> {
    [
        (Kind::Memory, memory_schema()),
        (Kind::ApplyPatch, apply_patch_schema()),
    ]
    .into_iter()
    .map(|(kind, schema)| -> Arc<dyn AgentTool> {
        Arc::new(MemoryRepoTool {
            kind,
            repo: repo.clone(),
            schema,
        })
    })
    .collect()
}

/// Discover the memory repository for `cwd` (`MEMORY_DIR` or `<memory_home>/agents/<slug>-<hash>/repo`)
/// without creating directories on disk. When absent and `log_missing` is set, logs one startup line.
#[must_use]
pub fn session_tools_for(cwd: &Path, log_missing: bool) -> Vec<Arc<dyn AgentTool>> {
    session_tools_for_with(cwd, log_missing, &|key| std::env::var(key).ok())
}

pub(crate) fn session_tools_for_with<F>(
    cwd: &Path,
    log_missing: bool,
    env: &F,
) -> Vec<Arc<dyn AgentTool>>
where
    F: Fn(&str) -> Option<String>,
{
    match omp_memory::identity::discover_memory_repo_with(cwd, env) {
        omp_memory::identity::MemoryRepoLookup::Found(bound) => tools_for(&bound.repo),
        omp_memory::identity::MemoryRepoLookup::NotFound {
            primary,
            legacy,
            reason,
        } => {
            if log_missing {
                eprintln!(
                    "omt: memory repository not found for {} (checked {}, legacy {}); memory tools disabled ({reason})",
                    cwd.display(),
                    primary.display(),
                    legacy.display()
                );
                tracing::info!(
                    cwd = %cwd.display(),
                    primary = %primary.display(),
                    legacy = %legacy.display(),
                    %reason,
                    "memory repo tools unavailable"
                );
            }
            Vec::new()
        }
    }
}

/// [`session_tools_for_with`] given only `MEMORY_DIR` and `AGENT_ID`, with a home that
/// holds no memory repository.
#[cfg(test)]
fn session_tools_in(
    root: Option<std::path::PathBuf>,
    identity: Option<&str>,
) -> Vec<Arc<dyn AgentTool>> {
    let home = std::env::temp_dir().join(format!("omt-memory-no-home-{}", std::process::id()));
    let env = |key: &str| match key {
        "MEMORY_DIR" => root.as_ref().map(|r| r.display().to_string()),
        "AGENT_ID" => identity.map(str::to_owned),
        "HOME" => Some(home.display().to_string()),
        _ => None,
    };
    session_tools_for_with(&home, false, &env)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    fn init(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "omt-memory-tool-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "core.hooksPath", ".git/hooks"],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        root
    }

    fn by_name(tools: &[Arc<dyn AgentTool>], name: &str) -> Arc<dyn AgentTool> {
        tools.iter().find(|t| t.name() == name).unwrap().clone()
    }

    fn body(result: &AgentToolResult) -> String {
        serde_json::to_value(result).unwrap().to_string()
    }

    fn tool(root: &PathBuf, name: &str) -> MemoryRepoTool {
        let repo = MemoryRepo::open(root, "omt").unwrap();
        MemoryRepoTool {
            kind: if name == "memory" {
                Kind::Memory
            } else {
                Kind::ApplyPatch
            },
            repo,
            schema: Value::Null,
        }
    }

    #[test]
    fn schemas_pin_the_gateway_contract() {
        let memory = memory_schema();
        assert_eq!(memory["required"], json!(["command", "reason"]));
        assert_eq!(
            memory["properties"]["command"]["enum"],
            json!([
                "create",
                "str_replace",
                "insert",
                "delete",
                "rename",
                "update_description"
            ])
        );
        for field in [
            "file_path",
            "file_text",
            "description",
            "old_string",
            "new_string",
            "insert_line",
            "insert_text",
            "old_path",
            "new_path",
        ] {
            assert!(memory["properties"].get(field).is_some(), "{field}");
        }
        assert_eq!(memory["properties"]["insert_line"]["type"], "number");
        let patch = apply_patch_schema();
        assert_eq!(patch["required"], json!(["reason", "input"]));
    }

    #[test]
    fn tools_are_absent_without_a_git_memory_repo() {
        assert!(session_tools_in(None, None).is_empty());
        assert!(session_tools_in(Some(PathBuf::new()), None).is_empty());
        let plain = std::env::temp_dir().join(format!("omt-memory-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        assert!(session_tools_in(Some(plain), None).is_empty());
    }

    #[tokio::test]
    async fn a_git_repo_advertises_both_tools_with_the_agent_identity() {
        let root = init("advertise");
        let tools = session_tools_in(Some(root.clone()), Some("oh-my-tau-3eb9c32a"));
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["memory", "memory_apply_patch"]);
        assert!(by_name(&tools, "memory")
            .description()
            .contains("committed locally"));
        let args = json!({"command": "create", "reason": "r", "file_path": "system/x.md",
            "description": "d", "file_text": "t\n"});
        let registered = tools.iter().find(|t| t.name() == "memory").unwrap().clone();
        let result = registered
            .execute(
                "call".to_owned(),
                args,
                AbortSignal::never(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert!(body(&result).contains("Memory create committed locally ("));
        let author = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["log", "-1", "--format=%ae"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&author.stdout).trim(),
            "oh-my-tau-3eb9c32a@omo.local"
        );
    }

    #[test]
    fn memory_errors_surface_as_tool_errors() {
        let root = init("errors");
        let memory = tool(&root, "memory");
        let missing_reason = memory.run(&json!({"command": "create"})).unwrap_err();
        assert_eq!(
            missing_reason.to_string(),
            "memory: 'reason' must be a non-empty string"
        );
        let outside = memory
            .run(
                &json!({"command": "create", "reason": "r", "file_path": "../x.md",
                "description": "d"}),
            )
            .unwrap_err();
        assert!(outside.to_string().contains("traversal"), "{outside}");
    }

    #[test]
    fn apply_patch_runs_through_the_tool() {
        let root = init("patch");
        let patch = tool(&root, "memory_apply_patch");
        let input = "*** Begin Patch\n*** Add File: reference/p.md\n+---\n+description: p\n+---\n+body\n*** End Patch";
        let result = patch
            .run(&json!({"reason": "add p", "input": input}))
            .unwrap();
        assert!(body(&result).contains("memory_apply_patch committed locally ("));
        let err = patch
            .run(&json!({"reason": "bad", "input": "not a patch"}))
            .unwrap_err();
        assert!(err.to_string().starts_with("memory apply_patch: "), "{err}");
    }

    #[test]
    fn auto_discovers_tools_without_memory_dir_and_creates_nothing_when_missing() {
        let base = std::env::temp_dir().join(format!(
            "omt-memory-auto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mem_home = base.join("mem-home");
        let project = base.join("한글 프로젝트");
        std::fs::create_dir_all(&project).unwrap();
        let ids = omp_memory::identity::memory_agent_ids(&project);
        let env = |key: &str| match key {
            "MAHO_MEMORY_HOME" => Some(mem_home.display().to_string()),
            "HOME" => Some(base.display().to_string()),
            _ => None,
        };

        assert!(session_tools_for_with(&project, false, &env).is_empty());
        assert!(!mem_home.exists());

        let repo_dir = mem_home.join("agents").join(&ids.id).join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "core.hooksPath", ".git/hooks"],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let tools = session_tools_for_with(&project, false, &env);
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["memory", "memory_apply_patch"]);
        let _ = std::fs::remove_dir_all(&base);
    }
}
