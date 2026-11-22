//! Subagent task tool ported from omp (TS `@oh-my-pi/pi-coding-agent/src/task`).
//!
//! Two halves:
//! - [`agent`]/[`discovery`]: agent-definition loading from markdown files with
//!   YAML frontmatter (`~/.omp/agent/agents/*.md`, `.omp/agents/*.md`), mirroring
//!   `task/agents.ts` + `task/discovery.ts`.
//! - [`runner`]/[`params`]/[`schema`]: the task-spawning contract — task params
//!   (`tasks[]`, `context`, `name`, `agent`, `outputSchema`), child runs as
//!   subprocesses of an `omp-rs -p`-style binary, bounded concurrency, result
//!   envelopes with `agent://<id>` pointers, and outputSchema validation in
//!   permissive/strict modes.
//!   [`TaskRunner::start`] is the controllable variant (kill a running child's
//!   process group; steer an `--mode rpc` child through its stdin).
//! - [`registry`]/[`transcript`]: background subagents (`st_<12hex>` task ids)
//!   the parent steers, revives, reads and cancels after the spawning call
//!   returned (OmOMeow `task_send`/`task_output`/`task_cancel` contract).

pub mod agent;
pub mod discovery;
pub mod envelope;
pub mod params;
pub mod registry;
pub mod rubric;
pub mod runner;
pub mod schema;
pub mod transcript;

pub use agent::{parse_agent, AgentDefinition, AgentParseError, ParsedAgentFields, ThinkingLevel};
pub use discovery::{
    discover_agents_in_dir, discover_project_agents, discover_user_agents, AgentLoadError,
    DiscoveryResult,
};
pub use envelope::{format_task_result, TaskResultEnvelope};
pub use params::{SchemaMode, TaskItem, TaskParams};
pub use registry::{
    is_task_id, CancelOutcome, Continuation, Lookup, RegistryEvent, SendOutcome, SpawnRequest,
    SubagentRegistry, TaskRecord, TaskStatus, TaskView,
};
pub use runner::{
    current_task_depth, max_recursion_depth, recursion_gate, recursion_gate_at, ChildMode,
    RunControl, RunJoin, RunOutput, RunPhase, RunStatus, StartOptions, SteerError, TaskRunner,
    TaskSpec, DEFAULT_MAX_RECURSION_DEPTH, MAX_RECURSION_ENV, TASK_DEPTH_ENV,
};
pub use schema::SchemaValidation;
