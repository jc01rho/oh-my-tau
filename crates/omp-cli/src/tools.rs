//! Built-in tools with omp's names, parameter schemas and result formats:
//! `read`, `edit`, `write`, `grep`, `glob`, `bash`.
//!
//! The edit engine is the vendored `pi-edit` crate (omp's native edit
//! sessions, hashline mode included) and the file walks are `omp-xfs` over `pi-vfs`.
//! Failures are returned as `Err` so the agent loop reports `isError: true`.

pub(crate) mod aliases;
pub mod ask;
mod ask_goal_compat;
mod ast;
pub mod bash;
pub(crate) mod bash_output;
pub(crate) mod browser_prelude;
pub mod checkpoint;
pub(crate) mod computer_prelude;
pub(crate) mod context_host;
pub(crate) mod context_notes;
pub(crate) mod context_rollover;
pub(crate) mod debug;
mod edit;
mod eval;
pub(crate) mod eval_agents;
pub(crate) mod eval_bridge;
pub(crate) mod eval_journal;
mod find;
pub(crate) mod github;
pub(crate) mod github_cache;
pub(crate) mod image_gen;
mod jobs;
pub mod mcp;
pub(crate) mod memory_backend;
pub(crate) mod memory_tools;
mod read;
pub(crate) mod read_fetch;
pub(crate) mod read_formats;
pub(crate) mod relevance;
pub(crate) mod rlm;
pub(crate) mod rlm_combinators;
mod search;
pub(crate) mod security_scan;
pub mod shell_host;
mod task;
mod todo;
pub(crate) mod tts;
pub(crate) mod vibe;
pub mod wasm;
pub(crate) use read::url_registry;
pub use todo::{phases_from_entries, TodoPhase, TodoTool};
#[cfg(test)]
pub use todo::{Status, TodoItem};
mod goal;
mod lsp;
mod think;
mod tree;
mod util;
mod wait;
mod web_search;
mod yield_tool;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::Result;
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use pi_edit::{EditMode, EditStore};
use serde_json::{json, Value};

/// Job registry of the most recently built tool set (see [`take_last_jobs`]).
static LAST_JOBS: std::sync::Mutex<Option<jobs::JobRegistry>> = std::sync::Mutex::new(None);
/// The agent session's job registry (see [`set_session_jobs`]).
static SESSION_JOBS: std::sync::Mutex<Option<jobs::JobRegistry>> = std::sync::Mutex::new(None);
/// The session's tool [`Env`], held weakly: tools keep it alive through their own
/// `Arc<Env>`, so a strong link here would trap the pair in a cycle (see
/// [`set_session_env`]).
static SESSION_ENV: std::sync::Mutex<Option<Weak<Env>>> = std::sync::Mutex::new(None);

/// The registry of the tool set built last, for the caller that builds the session's tools to
/// capture right after (before anything else builds a throwaway set).
#[must_use]
pub fn take_last_jobs() -> Option<jobs::JobRegistry> {
    LAST_JOBS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Publish the agent session's job registry (`ctx.getAsyncJobSnapshot()`).
pub fn set_session_jobs(jobs: Option<jobs::JobRegistry>) {
    *SESSION_JOBS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = jobs;
}

/// The agent session's job registry, once published.
#[must_use]
pub fn session_jobs() -> Option<jobs::JobRegistry> {
    SESSION_JOBS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Publish the tool [`Env`] of the agent session, so callers that rebuild the
/// tool list can refresh its `dyn` snapshot (see [`republish_dyn_tools`]).
pub fn set_session_env(env: &Arc<Env>) {
    *SESSION_ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(env));
}

/// The agent session's tool [`Env`], while the session's tools keep it alive.
#[must_use]
pub fn session_env() -> Option<Arc<Env>> {
    SESSION_ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade)
}

/// Refresh the session `Env`'s `dyn` snapshot after the tool list changed (model
/// swap, `todo` rehydration, `clm-host` toggle, RPC `ask` swap): the in-process
/// shell must dispatch to the list the agent actually holds, gate included.
pub fn republish_dyn_tools(tools: &[Arc<dyn AgentTool>]) {
    if let Some(env) = session_env() {
        env.set_dyn_tools(tools);
    }
}

/// Per-session state shared by the tools: working directory, the hashline
/// snapshot store (`read` mints tags, `edit` validates them) and edit mode.
pub struct Env {
    pub cwd: PathBuf,
    pub store: EditStore,
    pub edit_mode: EditMode,
    /// Hashline anchors in `read`/`grep` output (hashline edit mode only).
    pub hashline: bool,
    /// Background `bash` jobs (async mode) and the `proc://` read surface.
    pub jobs: jobs::JobRegistry,
    /// Request text of the running tool call (latest user message + assistant
    /// text), set per call from [`relevance::take`]; ranks the inline view of
    /// spilled `bash`/`read` output.
    pub query: parking_lot::Mutex<String>,
    /// Weak snapshot of the registered host tools exposed to
    /// `omp-shell-builtins`' `dyn` builtin. Weak on purpose: the tools hold this
    /// `Env` strongly, so a strong list here would make the pair un-freeable.
    pub dyn_tools: parking_lot::RwLock<Option<shell_host::DynToolList>>,
}

impl Env {
    #[must_use]
    pub fn with_mode(cwd: &Path, edit_mode: EditMode) -> Self {
        Self::with_mode_and_jobs(cwd, edit_mode, jobs::JobRegistry::default())
    }

    /// [`Env::with_mode`] over an existing job registry: a tool-list rebuild
    /// (model swap, edit-mode change) passes the session's registry so the
    /// background jobs and services started before the rebuild stay visible in
    /// `/jobs`, `asyncJobs` and `proc://`.
    #[must_use]
    pub fn with_mode_and_jobs(cwd: &Path, edit_mode: EditMode, jobs: jobs::JobRegistry) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            store: EditStore::new(),
            edit_mode,
            hashline: edit_mode == EditMode::Hashline,
            jobs,
            query: parking_lot::Mutex::default(),
            dyn_tools: parking_lot::RwLock::default(),
        }
    }

    /// Snapshot of host tools available to the in-process `dyn` builtin.
    /// Entries whose owner has since dropped are skipped by the host adapter.
    #[must_use]
    pub fn dyn_tools(&self) -> Option<shell_host::DynToolList> {
        self.dyn_tools.read().clone()
    }

    /// Publish the tool list the `dyn` builtin dispatches to. Stored weakly, so
    /// the list owning these tools decides their lifetime (an empty list
    /// publishes nothing).
    pub fn set_dyn_tools(&self, tools: &[Arc<dyn AgentTool>]) {
        *self.dyn_tools.write() = (!tools.is_empty())
            .then(|| Arc::new(tools.iter().map(Arc::downgrade).collect::<Vec<_>>()));
    }

    /// The current call's request text (see [`Env::query`]).
    #[must_use]
    pub fn query(&self) -> String {
        self.query.lock().clone()
    }

    #[cfg(test)]
    #[must_use]
    pub fn new(cwd: &Path, hashline: bool) -> Self {
        Self::with_mode(
            cwd,
            if hashline {
                EditMode::Hashline
            } else {
                EditMode::Replace
            },
        )
    }
}

/// omp `resolveEditMode`: `PI_EDIT_VARIANT` pins the mode; otherwise hashline,
/// except for model families whose hashline output is unreliable (omp's
/// per-model fallback to `replace`).
#[must_use]
pub fn edit_mode_for(model_id: &str) -> EditMode {
    if let Some(mode) = std::env::var("PI_EDIT_VARIANT")
        .ok()
        .as_deref()
        .and_then(EditMode::parse)
    {
        return mode;
    }
    let id = model_id.to_lowercase();
    let glm_flash_53 = id.contains("glm") && id.contains("flash") && id.contains("5.3");
    let replace_family = ["kimi", "mimo", "minimax", "deepseek", "step", "codex-spark"]
        .iter()
        .any(|f| id.contains(f));
    if glm_flash_53 || replace_family {
        EditMode::Replace
    } else {
        EditMode::Hashline
    }
}

type Handler = Arc<
    dyn Fn(Arc<Env>, Value, AbortSignal) -> BoxFut<'static, Result<AgentToolResult>> + Send + Sync,
>;

struct FnTool {
    name: &'static str,
    description: String,
    schema: Value,
    env: Arc<Env>,
    run: Handler,
}

impl AgentTool for FnTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn execute(
        self: Arc<Self>,
        tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        if let Some(q) = relevance::take(&tool_call_id) {
            *self.env.query.lock() = q;
        }
        (self.run)(self.env.clone(), params, signal)
    }
}

const READ_DESCRIPTION: &str = "Use `read` for static web; browser only if needed.\n\nPath suffixes: :50 or :50- starts at line 50; :50-200 inclusive; :50+150 counts lines; :-60 last 60; commas join ranges (:5-16,960-973) or individual lines (:19,59). :raw verbatim without anchors/prefixes; combine :2-4:raw or :raw:2-4.\n\nSources:\n- Selected file: `[foo.ts#1A2B]` snapshot+lines. Copy `[FILENAME#TAG]` for anchored edits; NEVER invent tag.\n- Directory: complete root; child listings cap at 12 (`… N more`), read child; page via :N-M/:-N.";
const BASH_DESCRIPTION: &str = "Persistent shell: one fact command/pipeline; dependencies use `&&`.\nScripts/heredocs/`$(…)`/complex pipelines → `eval`.\n`cwd`, not `cd`; `pty` only interactive.\nInternal URIs work as paths for builtins/coreutils, redirects, globs.\n`async` defers finite results but keeps the deadline (default 300s); `timeout: 0` for watchers and long jobs.\nNo `head`/`tail`/redirection; output trunc by default, full result at `artifact://<id>`.\nLong-lived services: unique name; ready requires name; no async/timeout; pty defaults true. ready needs log regex or port (both if given); host defaults 127.0.0.1, ready.timeout 30s.\nBackground results auto-deliver.";
const GLOB_DESCRIPTION: &str = "Glob files/dirs: `;`-separated paths or internal URLs (local://*.md, omp://**/*.md); default workspace root.\n`gitignore` and `hidden` default true; ignored dotfiles need `gitignore: false`. Newest-first by directory; dirs end `/`.\nBehavior search → `find`.";
const GREP_DESCRIPTION: &str = "Regex: Rust, then PCRE2. `path`: `;`-separated file/dir/glob/URL; default `.`. Default case-sensitive, gitignore respected; `skip` paginates files.\nFile-only selector: `src/foo.ts:50-100`. Literal `\\n`/`\\\\n` enables cross-line.\nBare glob `*.ts` matches any depth; `dir/*.ts` only `dir`'s direct children (`dir/**/*.ts` recurses).";
const WRITE_DESCRIPTION: &str = "SHOULD `edit` existing files; `write` for required new files or whole-file replacement. NEVER create docs or emojis unless requested.";
const FIND_DESCRIPTION: &str = "Describe behavior, get implementing files and line ranges. MUST use first for unknown locations; known strings/regex/symbols → `grep`, names → `glob`.\n`query`: plain language, not regex; quoted phrases match whole. `grep_keywords`: likely verbatim terms, `[]` if unsure.\n`path`: one directory or file, host path or internal URL (`omp://`, `omp://<file>.md`, `skill://<name>`, `local://…`); omitted = workspace root. Scope known subsystem; batch related questions. Searches live files, no index; no `:start-end` selector (judges whole files).\nHits strongest first: `path:start-end score snippet` (workspace-relative, or URL under URL scope); read returned ranges. Scores: absolute comparable 0–1 probability; below ~0.4 = weak evidence, so widen query or use `grep` before concluding absence.";

/// All built-in tools, rooted at `cwd`, without exposing their [`Env`].
#[must_use]
pub fn builtin_tools(cwd: &Path, edit_mode: EditMode) -> Vec<Arc<dyn AgentTool>> {
    builtin_tools_with_env(cwd, edit_mode).1
}

/// [`builtin_tools`] plus the [`Env`] they share, so the caller can publish the
/// final tool list to it once settings, extensions and the approval gate have
/// had their say (see [`Env::set_dyn_tools`]).
#[must_use]
pub fn builtin_tools_with_env(
    cwd: &Path,
    edit_mode: EditMode,
) -> (Arc<Env>, Vec<Arc<dyn AgentTool>>) {
    builtin_tools_with_env_and_jobs(cwd, edit_mode, jobs::JobRegistry::default())
}

/// [`builtin_tools_with_env`] over an existing job registry (see
/// [`Env::with_mode_and_jobs`]).
#[must_use]
#[allow(clippy::too_many_lines)] // the tool registry table; order mirrors omp's builtinTools
pub fn builtin_tools_with_env_and_jobs(
    cwd: &Path,
    edit_mode: EditMode,
    jobs: jobs::JobRegistry,
) -> (Arc<Env>, Vec<Arc<dyn AgentTool>>) {
    let env = Arc::new(Env::with_mode_and_jobs(cwd, edit_mode, jobs));
    *LAST_JOBS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(env.jobs.clone());
    let mk = |name: &'static str,
              description: String,
              schema: Value,
              run: Handler|
     -> Arc<dyn AgentTool> {
        Arc::new(FnTool {
            name,
            description,
            schema,
            env: env.clone(),
            run,
        })
    };
    // Blocking tools run on the `spawn_blocking` pool (safe under the
    // current_thread runtime). Abort is honored around the blocking section:
    // checked before dispatch, watched concurrently (flipping the tool's
    // `CancelFlag` so long walker loops can bail early), and checked again
    // after completion so a cancelled call does not surface stale output.
    let blocking = |f: fn(&Env, &Value) -> Result<AgentToolResult>| -> Handler {
        Arc::new(move |env, args, signal| {
            Box::pin(async move {
                pa_agent::abort::throw_if_aborted(&signal)?;
                let result = tokio::task::spawn_blocking(move || f(&env, &args))
                    .await
                    .map_err(|e| anyhow::anyhow!("tool task failed: {e}"))?;
                // A cancelled turn must not surface stale output.
                if signal.is_aborted() {
                    return Err(pa_agent::abort::aborted_error());
                }
                result
            })
        })
    };
    let mut tools = vec![
        mk(
            "read",
            READ_DESCRIPTION.to_string(),
            json!({"type":"object","properties":{"path":{"type":"string","description":"Local path, internal URI, or URL; selectors inline."}},"required":["path"],"additionalProperties":false}),
            blocking(read::read),
        ),
        mk(
            "bash",
            BASH_DESCRIPTION.to_string(),
            json!({"type":"object","properties":{
                "command":{"type":"string"},
                "timeout":{"type":"number","description":"timeout in seconds; 0 disables the command deadline; nonzero values are clamped to 1-3600"},
                "cwd":{"type":"string"},
                "pty":{"type":"boolean"},
                "async":{"type":"boolean"},
                "notify":{"type":"string"},
                "name":{"type":"string","maxLength":48,"pattern":"^(?:\\s*|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$","description":"starts a long-lived supervised service under this name; omit for finite commands, async jobs included. 1-48 of [A-Za-z0-9._-]"},
                "ready":{"type":"object","properties":{
                    "log":{"type":"string"},
                    "port":{"type":"number"},
                    "host":{"type":"string"},
                    "timeout":{"type":"number"}
                }}
            },"required":["command"],"additionalProperties":false}),
            Arc::new(|env, args, signal| Box::pin(bash::bash(env, args, signal))),
        ),
        mk(
            "edit",
            pi_edit::description(edit_mode).to_string(),
            edit::schema(edit_mode),
            Arc::new(|env, args, _| Box::pin(async move { edit::edit(&env, &args).await })),
        ),
        mk(
            "glob",
            GLOB_DESCRIPTION.to_string(),
            // Golden `findSchema` exactly: four properties, no `required`, no
            // `additionalProperties`. The reference's fifth accepted key
            // `pattern` (`GLOB_ARGUMENTS`) is read by `search::glob` as a
            // runtime-only extension, so it is not advertised here.
            json!({"type":"object","properties":{"path":{"type":"string"},"hidden":{"type":"boolean"},"gitignore":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":200}}}),
            Arc::new(|env, args, signal| Box::pin(search::glob(env, args, signal))),
        ),
        mk(
            "grep",
            GREP_DESCRIPTION.to_string(),
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case":{"type":"boolean"},"gitignore":{"type":"boolean"},"skip":{"anyOf":[{"type":"number"},{"type":"null"}]}},"required":["pattern"],"additionalProperties":false}),
            Arc::new(|env, args, signal| Box::pin(search::grep(env, args, signal))),
        ),
        mk(
            "write",
            WRITE_DESCRIPTION.to_string(),
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}),
            blocking(edit::write),
        ),
        mk(
            "find",
            FIND_DESCRIPTION.to_string(),
            json!({"type":"object","properties":{
                "query":{"type":"string"},
                "grep_keywords":{"type":"array","items":{"type":"string"}},
                "path":{"type":"string"}
            },"required":["query","grep_keywords"],"additionalProperties":false}),
            Arc::new(|env, args, signal| Box::pin(find::find(env, args, signal))),
        ),
        mk(
            "task",
            task::description(cwd),
            task::schema(),
            Arc::new(|env, args, _| Box::pin(task::task(env, args))),
        ),
        wait::tool(env.clone()),
        mk(
            "bash_output",
            bash_output::description().to_string(),
            bash_output::schema(),
            blocking(bash_output::bash_output),
        ),
        mk(
            "kill_bash",
            bash_output::kill_description().to_string(),
            bash_output::kill_schema(),
            blocking(bash_output::kill_bash),
        ),
        // Own AgentTool: eval needs the abort signal (cell cancel) and on_update
        // (bridge status events), which the `mk` closure signature drops.
        Arc::new(eval::EvalTool::new(env.clone())),
        mk(
            "ast_grep",
            ast::AST_GREP_DESCRIPTION.to_string(),
            ast::schema_ast_grep(),
            Arc::new(|env, args, signal| Box::pin(ast::ast_grep(env, args, signal))),
        ),
        mk(
            "ast_edit",
            ast::AST_EDIT_DESCRIPTION.to_string(),
            ast::schema_ast_edit(),
            Arc::new(|env, args, signal| Box::pin(ast::ast_edit(env, args, signal))),
        ),
        ask::tool(),
        ask_goal_compat::ask_user_question_tool(),
        todo::tool(),
        web_search::tool(),
        lsp::tool(env.clone()),
        checkpoint::checkpoint_tool(),
        checkpoint::rewind_tool(),
        // Own AgentTool (DAP sessions, per-action tiers), not a `FnTool`.
        debug::tool(env.clone()),
        // Bound to the session cwd; gated off by default (`github.enabled`).
        github::tool(cwd.to_path_buf()),
        // Gated off by default (`speechgen.enabled`).
        tts::tool(env.clone()),
        // Gated off by default (`generate_image.enabled`).
        image_gen::tool(env.clone()),
    ];
    tools.extend(ask_goal_compat::goal_tools());
    env.set_dyn_tools(&tools);
    (env, tools)
}

/// TS `isToolAllowed` setting gates (`tools/index.ts`), one explicit arm per
/// gated name — the same 22-name table, in the reference's order. `debug`,
/// `github` and `tts` are gated for real now that they are in the builtin
/// table. The remaining arms (`security_scan`, `context_notes`, `new_context`,
/// `memory_edit`, `retain`, `recall`, `reflect`, `learn`, `manage_skill`) are
/// kept so the table is complete and their gates go live with the tool, and are
/// marked *inert* below: each needs a session-owned backend or host
/// (`SecurityScanBackend`, `ContextNotesHost`, `MemoryToolBackend`) that this
/// registry does not own, so the tool cannot be constructed here yet.
///
/// Every other name returns `true`: the reference's `return true` fallthrough for
/// tools with no gate (`read`, `write`, `ast_grep`'s sibling rules, `yield`, ...).
///
/// Legs the reference evaluates against session state rather than settings cannot
/// be expressed here and are noted per arm: `taskDepth` (`checkpoint`/`rewind`,
/// `manage_skill`/`learn`), `requestedTools` (`restrictToolNames` for `goal`),
/// `prewalkArmed` (`todo`), `isFindEnabled`'s native-judge detection (`find`),
/// `allowEval`'s backend probe (`eval`), and `think`'s model support (applied by
/// [`hidden_tools`]).
#[must_use]
#[allow(clippy::too_many_lines)] // one arm per reference gate, kept in table order
pub fn enabled_by_settings(settings: &omp_settings::Settings, name: &str) -> bool {
    // Setting defaults come from the ported TS defaults layer, so `unwrap_or`
    // only fires for an unset key; the values here mirror `isToolAllowed`.
    let flag = |id: &str, default: bool| settings.get_bool(id).unwrap_or(default);
    let string = |id: &str| settings.get_string(id);
    match name {
        // `goal.enabled` and no goal record / enabled / dropped goal. The
        // `restrictToolNames` and goal-state legs need session state.
        "goal" | "create_goal" | "get_goal" | "update_goal" => flag("goal.enabled", true),
        // `enableLsp && lsp.enabled`; `enableLsp` is the host's LSP availability.
        "lsp" => flag("lsp.enabled", true),
        // `edit.mode === "none"` hides `edit`. The ported `edit.mode` enum has no
        // `none` member yet, so this stays true until one is added; the arm keeps
        // the gate in place rather than silently dropping it.
        "edit" => string("edit.mode").as_deref() != Some("none"),
        // `editDisabled || bash.enabled`: without an edit tool, bash is the only
        // way to modify existing files, so it stays on.
        "bash" => string("edit.mode").as_deref() == Some("none") || flag("bash.enabled", true),
        // `allowEval`: any eval backend reachable. The port always has one, so
        // this is the reference's truthy branch.
        "eval" => true,
        // `debug.enabled`; on by default, so the arm only hides `debug` when the
        // setting is off.
        "debug" => flag("debug.enabled", true),
        // `(!includeYield || prewalkArmed) && todo.enabled`: the prewalk leg needs
        // session state, so the setting is the whole gate here.
        "todo" => flag("todo.enabled", true),
        "glob" => flag("glob.enabled", true),
        "grep" => flag("grep.enabled", true),
        // `isFindEnabled`: `off` hides it; `auto` stays on (TS narrows `auto` to
        // native-judge models, which this host does not detect).
        "find" => string("find.enabled").as_deref() != Some("off"),
        // `github.enabled`; off by default (the `gh`/`glab` CLI must be present).
        "github" => flag("github.enabled", false),
        "ast_grep" => flag("astGrep.enabled", false),
        "ast_edit" => flag("astEdit.enabled", true),
        "web_search" => flag("web_search.enabled", true),
        // Inert until the security-scan mode controller lands (the tool needs a
        // session `SecurityScanBackend`).
        "security_scan" => flag("security.enabled", false),
        // `externalThinkingActive`: the setting half lives here; the model-support
        // half is [`hidden_tools`]'s `supports_external_thinking` check.
        "think" => flag("externalThinking", false),
        // `speechgen.enabled`; off by default.
        "tts" => flag("speechgen.enabled", false),
        // `generate_image.enabled`; off by default.
        "generate_image" => flag("generate_image.enabled", false),
        "ask" | "ask_user_question" => flag("ask.enabled", true),
        // Inert until the context-management tools land (they need the session
        // journal as a `ContextNotesHost`).
        "context_notes" | "new_context" => flag("compaction.experimentalContextManagement", false),
        // `checkpoint.enabled && (taskDepth == 0 || requestedTools !== undefined)`;
        // the depth leg needs session state, so the setting is the gate here.
        "checkpoint" | "rewind" => flag("checkpoint.enabled", false),
        // `async || (enableIrc !== false && isIrcEnabled) || launch.enabled`. This
        // port has no IRC/peer bus (see `docs/parity/tools.md` §4), so the IRC leg
        // is the reference's always-false branch and the two settings decide.
        "wait" => flag("async.enabled", true) || flag("launch.enabled", true),
        // Inert until a session `MemoryToolBackend` is supplied.
        "retain" | "recall" | "reflect" => matches!(
            string("memory.backend").as_deref(),
            Some("hindsight" | "mnemopi")
        ),
        "memory_edit" => string("memory.backend").as_deref() == Some("mnemopi"),
        // Inert until the autolearn tools land; the depth/requested legs need
        // session state.
        "manage_skill" => flag("autolearn.enabled", false),
        "learn" => {
            flag("autolearn.enabled", false)
                && matches!(
                    string("memory.backend").as_deref(),
                    Some("hindsight" | "mnemopi" | "local")
                )
        }
        // `canSpawnAtDepth(task.maxRecursionDepth, taskDepth)`: a non-positive
        // ceiling disables spawning everywhere (the depth leg is session state).
        "task" => settings.get_number("task.maxRecursionDepth").unwrap_or(2.0) > 0.0,
        _ => true,
    }
}

/// [`builtin_tools`] minus the ones switched off by settings: the surface a
/// session advertises.
#[must_use]
pub fn session_builtin_tools(
    cwd: &Path,
    edit_mode: EditMode,
    settings: &omp_settings::Settings,
) -> Vec<Arc<dyn AgentTool>> {
    session_builtin_tools_with_env(cwd, edit_mode, settings).1
}

/// [`session_builtin_tools`] plus their [`Env`], with the settings-filtered
/// list already published to it.
#[must_use]
pub fn session_builtin_tools_with_env(
    cwd: &Path,
    edit_mode: EditMode,
    settings: &omp_settings::Settings,
) -> (Arc<Env>, Vec<Arc<dyn AgentTool>>) {
    session_builtin_tools_with_env_and_jobs(cwd, edit_mode, settings, jobs::JobRegistry::default())
}

/// [`session_builtin_tools_with_env`] over an existing job registry, so a
/// rebuild keeps the session's live background jobs and services (see
/// [`Env::with_mode_and_jobs`]).
#[must_use]
pub fn session_builtin_tools_with_env_and_jobs(
    cwd: &Path,
    edit_mode: EditMode,
    settings: &omp_settings::Settings,
    jobs: jobs::JobRegistry,
) -> (Arc<Env>, Vec<Arc<dyn AgentTool>>) {
    let (env, mut tools) = builtin_tools_with_env_and_jobs(cwd, edit_mode, jobs);
    tools.retain(|t| enabled_by_settings(settings, t.name()));
    env.set_dyn_tools(&tools);
    (env, tools)
}

/// Active hidden tools for a session (TS `resolveBuiltinToolPlan`'s
/// `...(externalThinkingActive ? ["think"] : [])`, `...(includeYield ? ["yield"]
/// : [])`, `...(goalModeActive ? ["goal"] : [])` legs, in that order; never in
/// the visible built-in list). `think` needs the `externalThinking` setting and
/// a model whose transport supports external thinking; `yield` is included when
/// the session requires it (subagent runs); `goal` when goal mode is active
/// (`goal.enabled` setting + enabled goal-mode state).
#[must_use]
pub fn hidden_tools(
    settings: &omp_settings::Settings,
    model: Option<&pa_types::ai::Model>,
    require_yield_tool: bool,
    goal_mode_active: bool,
) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let external_thinking = settings.get_bool("externalThinking").unwrap_or(false)
        && model.is_some_and(think::supports_external_thinking);
    if external_thinking {
        tools.push(think::tool());
    }
    if require_yield_tool {
        tools.push(yield_tool::tool());
    }
    if goal_mode_active {
        tools.push(goal::tool());
    }
    tools
}

/// Construct one hidden tool by name (omp `HIDDEN_TOOLS` factories), for
/// explicit `--tools <csv>` requests that name a hidden tool.
#[must_use]
pub fn hidden_tool_by_name(name: &str) -> Option<Arc<dyn AgentTool>> {
    // Canonicalize first: a hidden tool named in an explicit `--tools` request
    // may arrive with the caller's casing (`Think`), which must still resolve.
    match aliases::normalize_tool_name(name).as_str() {
        "think" => Some(think::tool()),
        "yield" => Some(yield_tool::tool()),
        "goal" => Some(goal::tool()),
        _ => None,
    }
}

/// [`hidden_tool_by_name`] for the hidden tools that need the session [`Env`]:
/// the `vibe_*` worker family (omp registers them beside `yield`, out of the
/// visible catalog). Falls through to [`hidden_tool_by_name`].
///
/// Session wiring (`agent.rs`/`runtime.rs`) calls this from the `--tools <csv>`
/// branch; until that call site lands the factory is exercised by the tests in
/// this module.
#[allow(dead_code)]
#[must_use]
pub fn hidden_tool_with_env_by_name(name: &str, env: &Arc<Env>) -> Option<Arc<dyn AgentTool>> {
    match aliases::normalize_tool_name(name).as_str() {
        "vibe_spawn" => Some(Arc::new(vibe::VibeSpawnTool::new(env.clone()))),
        "vibe_send" => Some(Arc::new(vibe::VibeSendTool::new(env.clone()))),
        "vibe_wait" => Some(Arc::new(vibe::VibeWaitTool::new(env.clone()))),
        "vibe_kill" => Some(Arc::new(vibe::VibeKillTool::new(env.clone()))),
        "vibe_list" => Some(Arc::new(vibe::VibeListTool::new(env.clone()))),
        _ => hidden_tool_by_name(name),
    }
}

/// [`hidden_tool_by_name`] for `learn` / `manage_skill`: the autolearn tools need
/// a session [`memory_tools::MemoryToolBackend`], which the plain registry cannot
/// invent, so the caller supplies it. Both stay off unless `autolearn.enabled`
/// is set (`memory_tools::tools` applies the same gate), and `learn` additionally
/// needs a memory backend the settings name (`memory.backend`).
///
/// `None` for any other name, or when the setting gate is closed.
///
/// Same story as [`hidden_tool_with_env_by_name`]: provided for the session
/// wiring, exercised by this module's tests until then.
#[allow(dead_code)]
#[must_use]
pub fn hidden_memory_tool_by_name(
    name: &str,
    env: &Arc<Env>,
    backend: Arc<dyn memory_tools::MemoryToolBackend>,
    settings: &omp_settings::Settings,
) -> Option<Arc<dyn AgentTool>> {
    let wanted = aliases::normalize_tool_name(name);
    if !matches!(wanted.as_str(), "learn" | "manage_skill") {
        return hidden_tool_with_env_by_name(name, env);
    }
    memory_tools::tools(env, backend, settings)
        .into_iter()
        .find(|tool| tool.name() == wanted.as_str())
}

/// Canonicalize an explicit `--tools <csv>` request (omp `normalizeToolNames`):
/// legacy aliases collapse onto their canonical ID (`search` -> `grep`),
/// canonical IDs lowercase, and duplicates drop while first-seen order holds.
/// Explicit lists must route through this before matching tool names, or
/// `--tools search` would silently match nothing.
///
/// Provided for the session wiring, exercised by this module's tests until the
/// `agent.rs` `tools_csv` branch calls it.
#[allow(dead_code)]
#[must_use]
pub fn normalize_requested_tools<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    aliases::normalize_tool_names(names)
}

pub use ask::{tool_with_ui, AskUi, SelectOption};
pub use ask_goal_compat::{
    ask_user_question_with_ui, reset_session_goal, FollowUpDelivery, FollowUpSink,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_is_freeable_once_the_tool_list_drops() {
        // The tools hold the `Env` strongly and the `Env` holds them weakly, so
        // the pair must free with the tool list; a strong `dyn_tools` snapshot
        // here would trap both in a reference cycle.
        let (env, tools) = builtin_tools_with_env(Path::new("/tmp"), EditMode::Hashline);
        let weak_env = Arc::downgrade(&env);
        assert!(env.dyn_tools().is_some_and(|t| !t.is_empty()));
        drop(env);
        assert!(weak_env.upgrade().is_some(), "the tool list owns the env");
        drop(tools);
        assert!(
            weak_env.upgrade().is_none(),
            "env outlives the tool list that owns it"
        );
    }

    #[test]
    fn rebuild_keeps_the_session_job_registry() {
        // A model swap (or an edit-mode change) rebuilds the tool list; the
        // rebuild runs on the session's own registry, so a job backgrounded
        // before the swap is still listed by `asyncJobs` and `proc://` after.
        let dir = tempfile::tempdir().unwrap();
        let settings = omp_settings::Settings::load_from(None, dir.path());
        let registry = jobs::JobRegistry::default();
        let job_id = registry.insert(jobs::Job {
            command: "sleep 30".to_string(),
            started: std::time::Instant::now(),
            status: jobs::JobStatus::Running,
            output: Arc::new(std::sync::Mutex::new(String::new())),
            result: None,
            error: None,
            ended: None,
            read_at: Arc::new(std::sync::Mutex::new(0)),
            cancel: Arc::new(pa_agent::abort::AbortController::new()),
        });
        let (before, tools) = session_builtin_tools_with_env_and_jobs(
            dir.path(),
            EditMode::Hashline,
            &settings,
            registry.clone(),
        );
        drop(tools);
        // The rebuild passes the registry the session published before it.
        let (after, tools) = session_builtin_tools_with_env_and_jobs(
            dir.path(),
            EditMode::Replace,
            &settings,
            registry,
        );
        assert!(
            Arc::ptr_eq(&before.jobs.inner, &after.jobs.inner),
            "the rebuild runs on the session's registry instance"
        );
        assert!(
            after.jobs.get(&job_id).is_some(),
            "the rebuilt env still sees the job backgrounded before it"
        );
        let proc_listing = jobs::read_proc(&after, "").unwrap();
        let listing: String = proc_listing
            .content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            listing.contains(&format!("{job_id} [bash] running up ")),
            "proc:// lists the job after the rebuild: {listing}"
        );
        assert!(
            after.dyn_tools().is_some_and(|t| !t.is_empty()),
            "the rebuilt env publishes its tools"
        );
        drop(tools);
    }

    #[test]
    fn dyn_tools_follow_the_settings_filter() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yml");
        // `checkpoint`/`rewind` default off; `find.enabled: off` hides `find`.
        std::fs::write(&cfg, "find:\n  enabled: \"off\"\n").unwrap();
        let settings = omp_settings::Settings::load_from(Some(&cfg), dir.path());
        let (env, tools) =
            session_builtin_tools_with_env(dir.path(), EditMode::Hashline, &settings);
        let published: Vec<String> = env
            .dyn_tools()
            .expect("session env publishes its tools")
            .iter()
            .filter_map(Weak::upgrade)
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            published,
            tools
                .iter()
                .map(|t| t.name().to_string())
                .collect::<Vec<_>>()
        );
        for off in ["find", "checkpoint", "rewind"] {
            assert!(
                !published.iter().any(|n| n == off),
                "{off} published despite its setting"
            );
        }
    }

    #[test]
    fn edit_mode_follows_omp_model_rule() {
        // PI_EDIT_VARIANT may be set by the harness; only assert the rule when unset.
        if std::env::var_os("PI_EDIT_VARIANT").is_some() {
            return;
        }
        assert_eq!(edit_mode_for("glm-5.3-flash"), EditMode::Replace);
        assert_eq!(edit_mode_for("kimi-k2.5"), EditMode::Replace);
        assert_eq!(edit_mode_for("claude-opus-5-5"), EditMode::Hashline);
        assert_eq!(edit_mode_for("glm-5.2"), EditMode::Hashline);
    }

    #[test]
    fn settings_gate_the_advertised_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let names = |yaml: &str| -> Vec<String> {
            let cfg = dir.path().join("config.yml");
            std::fs::write(&cfg, yaml).unwrap();
            let settings = omp_settings::Settings::load_from(Some(&cfg), dir.path());
            session_builtin_tools(dir.path(), EditMode::Hashline, &settings)
                .iter()
                .map(|t| t.name().to_string())
                .collect()
        };
        // TS defaults: astGrep and checkpoint/rewind off, the rest on.
        let defaults = names("{}");
        for off in ["ast_grep", "checkpoint", "rewind"] {
            assert!(!defaults.iter().any(|n| n == off), "{off} on by default");
        }
        for on in ["read", "bash", "ast_edit", "lsp", "todo"] {
            assert!(defaults.iter().any(|n| n == on), "{on} off by default");
        }
        // Phase 3 gates: these stay on by default (`edit.mode` has no `none`
        // member, so the `edit` arm cannot hide it yet).
        for on in [
            "ask",
            "glob",
            "grep",
            "web_search",
            "task",
            "wait",
            "edit",
            "debug",
        ] {
            assert!(defaults.iter().any(|n| n == on), "{on} off by default");
        }
        // `github.enabled` is off by default (no `gh`/`glab` assumption).
        assert!(
            !defaults.iter().any(|n| n == "github"),
            "github on by default"
        );
        let with_github = names("github:\n  enabled: true\n");
        assert!(
            with_github.iter().any(|n| n == "github"),
            "github gate ignored"
        );
        let no_debug = names("debug:\n  enabled: false\n");
        assert!(!no_debug.iter().any(|n| n == "debug"), "debug gate ignored");
        // `speechgen.enabled` is off by default and turns `tts` on.
        assert!(!defaults.iter().any(|n| n == "tts"), "tts on by default");
        let with_tts = names("speechgen:\n  enabled: true\n");
        assert!(with_tts.iter().any(|n| n == "tts"), "tts gate ignored");
        // `wait` is gated by `async.enabled || launch.enabled`.
        let no_wait = names("async:\n  enabled: false\nlaunch:\n  enabled: false\n");
        assert!(!no_wait.iter().any(|n| n == "wait"), "wait gate ignored");
        // `task.maxRecursionDepth: 0` disables spawning everywhere.
        let no_task = names("task:\n  maxRecursionDepth: 0\n");
        assert!(
            !no_task.iter().any(|n| n == "task"),
            "task depth gate ignored"
        );
        // `ask.enabled: false` hides `ask`.
        let no_ask = names("ask:\n  enabled: false\n");
        assert!(!no_ask.iter().any(|n| n == "ask"), "ask gate ignored");
        let custom = names("lsp:\n  enabled: false\nastGrep:\n  enabled: true\ncheckpoint:\n  enabled: true\nfind:\n  enabled: \"off\"\n");
        assert!(!custom.iter().any(|n| n == "lsp" || n == "find"));
        assert!(
            defaults.iter().any(|n| n == "find"),
            "find.enabled auto keeps find"
        );
        for on in ["ast_grep", "checkpoint", "rewind"] {
            assert!(custom.iter().any(|n| n == on), "{on} not enabled by config");
        }
    }

    #[test]
    fn explicit_tool_lists_normalize_legacy_aliases() {
        // `--tools search` must resolve to `grep`, and duplicates collapse.
        assert_eq!(
            normalize_requested_tools(["search", "grep", "SEARCH", "read"]),
            vec!["grep", "read"]
        );
        // Plugin names keep their spelling (a plugin may be case-sensitive).
        assert_eq!(
            normalize_requested_tools(["mcp__a_b", "Read"]),
            vec!["mcp__a_b", "read"]
        );
        // Hidden tools resolve regardless of the caller's casing.
        assert!(hidden_tool_by_name("Think").is_some());
        assert!(hidden_tool_by_name("think").is_some());
        assert!(hidden_tool_by_name("not_a_tool").is_none());
    }

    #[test]
    fn hidden_vibe_tools_resolve_with_a_session_env() {
        let dir = tempfile::tempdir().unwrap();
        let (env, tools) = builtin_tools_with_env(dir.path(), EditMode::Hashline);
        for name in [
            "vibe_spawn",
            "vibe_send",
            "vibe_wait",
            "vibe_kill",
            "vibe_list",
        ] {
            let tool = hidden_tool_with_env_by_name(name, &env)
                .unwrap_or_else(|| panic!("{name} not constructible"));
            assert_eq!(tool.name(), name);
            assert!(
                !tools.iter().any(|t| t.name() == name),
                "{name} must not be advertised in the builtin table"
            );
        }
        // The env-free hidden tools still route through the same factory.
        assert!(hidden_tool_with_env_by_name("Think", &env).is_some());
        assert!(hidden_tool_with_env_by_name("nope", &env).is_none());
        drop(tools);
    }

    #[test]
    fn hidden_memory_tools_follow_the_autolearn_gate() {
        let dir = tempfile::tempdir().unwrap();
        let (env, tools) = builtin_tools_with_env(dir.path(), EditMode::Hashline);
        let backend: Arc<dyn memory_tools::MemoryToolBackend> =
            Arc::new(memory_tools::InMemoryBackend::new());

        let off = omp_settings::Settings::load_from(None, dir.path());
        for name in ["learn", "manage_skill"] {
            assert!(
                hidden_memory_tool_by_name(name, &env, backend.clone(), &off).is_none(),
                "{name} active with autolearn off"
            );
        }

        let cfg = dir.path().join("config.yml");
        std::fs::write(
            &cfg,
            "autolearn:\n  enabled: true\nmemory:\n  backend: mnemopi\n",
        )
        .unwrap();
        let on = omp_settings::Settings::load_from(Some(&cfg), dir.path());
        for name in ["learn", "manage_skill"] {
            let tool = hidden_memory_tool_by_name(name, &env, backend.clone(), &on)
                .unwrap_or_else(|| panic!("{name} not constructible with autolearn on"));
            assert_eq!(tool.name(), name);
            assert!(
                !tools.iter().any(|t| t.name() == name),
                "{name} must not be advertised in the builtin table"
            );
        }
        // Anything else falls through to the env factory rather than vanishing.
        assert!(hidden_memory_tool_by_name("think", &env, backend, &on).is_some());
        drop(tools);
    }

    #[test]
    fn tool_names_and_schemas_match_omp() {
        let tools = builtin_tools(Path::new("/tmp"), EditMode::Hashline);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            [
                "read",
                "bash",
                "edit",
                "glob",
                "grep",
                "write",
                "find",
                "task",
                "wait",
                "bash_output",
                "kill_bash",
                "eval",
                "ast_grep",
                "ast_edit",
                "ask",
                "ask_user_question",
                "todo",
                "web_search",
                "lsp",
                "checkpoint",
                "rewind",
                "debug",
                "github",
                "tts",
                "generate_image",
                "create_goal",
                "get_goal",
                "update_goal"
            ]
        );
        let edit = tools.iter().find(|t| t.name() == "edit").unwrap();
        assert_eq!(edit.parameters()["properties"]["input"]["type"], "string");
        assert!(edit
            .description()
            .starts_with("Hashline patches existing files"));
        let read = tools.iter().find(|t| t.name() == "read").unwrap();
        assert_eq!(read.parameters()["required"], json!(["path"]));
        // omp `writeSchema`: both `path` and `content` are required.
        let write = tools.iter().find(|t| t.name() == "write").unwrap();
        assert_eq!(write.parameters()["required"], json!(["path", "content"]));
        assert_eq!(
            write.parameters()["properties"],
            json!({"path": {"type": "string"}, "content": {"type": "string"}})
        );
    }

    #[test]
    fn bash_has_async_name_ready_params() {
        let tools = builtin_tools(Path::new("/tmp"), EditMode::Replace);
        let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
        let props = &bash.parameters()["properties"];
        assert_eq!(props["async"], json!({"type": "boolean"}));
        assert_eq!(props["notify"], json!({"type": "string"}));
        assert_eq!(
            props["name"],
            json!({"type": "string", "maxLength": 48,
                "pattern": "^(?:\\s*|[A-Za-z0-9][A-Za-z0-9._-]{0,47})$",
                "description": "starts a long-lived supervised service under this name; omit for finite commands, async jobs included. 1-48 of [A-Za-z0-9._-]"})
        );
        assert_eq!(
            props["ready"],
            json!({"type": "object", "properties": {
                "log": {"type": "string"},
                "port": {"type": "number"},
                "host": {"type": "string"},
                "timeout": {"type": "number"}
            }})
        );
        assert_eq!(
            bash.description().lines().next().unwrap(),
            "Persistent shell: one fact command/pipeline; dependencies use `&&`."
        );
    }

    #[test]
    fn grep_skip_uses_omp_anyof_encoding() {
        let tools = builtin_tools(Path::new("/tmp"), EditMode::Replace);
        let grep = tools.iter().find(|t| t.name() == "grep").unwrap();
        assert_eq!(
            grep.parameters()["properties"]["skip"],
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]})
        );
        assert_eq!(
            grep.description().lines().next().unwrap(),
            "Regex: Rust, then PCRE2. `path`: `;`-separated file/dir/glob/URL; default `.`. Default case-sensitive, gitignore respected; `skip` paginates files."
        );
        assert_eq!(
            tools.iter().find(|t| t.name() == "glob").unwrap().description().lines().next().unwrap(),
            "Glob files/dirs: `;`-separated paths or internal URLs (local://*.md, omp://**/*.md); default workspace root."
        );
        // omp `findSchema`: `limit?": "1 <= number.integer <= 200"`, and `pattern`
        // stays off the wire schema. NOTE: unlike upstream `GLOB_ARGUMENTS`,
        // `search::glob` does not read `pattern` yet - wiring it is deferred.
        assert_eq!(
            tools
                .iter()
                .find(|t| t.name() == "glob")
                .unwrap()
                .parameters()["properties"]["limit"],
            json!({"type": "integer", "minimum": 1, "maximum": 200})
        );
    }

    #[test]
    fn find_schema_matches_omp() {
        let tools = builtin_tools(Path::new("/tmp"), EditMode::Replace);
        let find = tools.iter().find(|t| t.name() == "find").unwrap();
        assert_eq!(
            find.parameters()["required"],
            json!(["query", "grep_keywords"])
        );
        let props = &find.parameters()["properties"];
        assert_eq!(props["query"], json!({"type": "string"}));
        assert_eq!(
            props["grep_keywords"],
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(props["path"], json!({"type": "string"}));
        assert_eq!(
            find.description().lines().next().unwrap(),
            "Describe behavior, get implementing files and line ranges. MUST use first for unknown locations; known strings/regex/symbols → `grep`, names → `glob`."
        );
    }
}
