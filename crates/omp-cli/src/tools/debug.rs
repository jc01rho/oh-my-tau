//! `debug`: Debug Adapter Protocol access. Port of omp `tools/debug.ts`:
//! launch/attach, source/function/instruction/data breakpoints, stepping,
//! evaluation, stack/scope/variable inspection, disassembly, memory access,
//! modules, loaded sources, raw requests, output, termination and the session
//! list. The DAP machinery lives in the `omp-dap` crate; this module is the
//! tool surface: schema, approval tier, timeout and per-action dispatch.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use omp_dap::session::{parse_access_type, SHARED};
use omp_dap::types::{
    DapAttachSessionOptions, DapAttachTarget, DapDataBreakpoint, DapFunctionBreakpoint,
    DapInstructionBreakpoint, DapLaunchSessionOptions, DapSourceBreakpoint, LaunchAdapterSelection,
};
use omp_dap::{config, format, Cancel};
use pa_agent::abort::{is_abort_error, AbortSignal};
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use pa_agent::BoxFut;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::Env;

/// omp `prompts/tools/debug.md`, byte-identical.
pub const DESCRIPTION: &str = r#"Debugger access. Prefer over bash for program state, breakpoints, stepping, or thread inspection.
Only one active session at a time. `program` is a target path, not a shell command.
Directories need a directory-capable adapter (e.g. `dlv`)."#;

/// Every action the tool accepts, in the order omp's schema declares them.
pub const ACTIONS: [&str; 28] = [
    "launch",
    "attach",
    "set_breakpoint",
    "remove_breakpoint",
    "set_instruction_breakpoint",
    "remove_instruction_breakpoint",
    "data_breakpoint_info",
    "set_data_breakpoint",
    "remove_data_breakpoint",
    "continue",
    "step_over",
    "step_in",
    "step_out",
    "pause",
    "evaluate",
    "stack_trace",
    "threads",
    "scopes",
    "variables",
    "disassemble",
    "read_memory",
    "write_memory",
    "modules",
    "loaded_sources",
    "custom_request",
    "output",
    "terminate",
    "sessions",
];

/// Actions that only read adapter state, so they run at the `read` tier.
///
/// Everything else mutates the debuggee or the adapter and runs at `exec`.
pub const READONLY_ACTIONS: [&str; 10] = [
    "output",
    "threads",
    "stack_trace",
    "scopes",
    "variables",
    "disassemble",
    "read_memory",
    "loaded_sources",
    "modules",
    "sessions",
];

/// Tool timeout config (omp `TOOL_TIMEOUTS.debug`).
const TIMEOUT_DEFAULT_SECS: u64 = 30;
const TIMEOUT_MIN_SECS: u64 = 5;
const TIMEOUT_MAX_SECS: u64 = 300;

/// Default `instructionCount` for `disassemble` (omp's `32`).
const DEFAULT_INSTRUCTION_COUNT: i64 = 32;

/// Default byte count for `read_memory` (omp's `64`).
const DEFAULT_READ_BYTES: i64 = 64;

/// Parameter schema, byte-equal (as JSON) to omp's `debugSchema`.
///
/// Unlike every other tool in this crate the schema has **no**
/// `additionalProperties: false`: omp leaves it open so adapter-specific
/// passthrough arguments survive validation.
#[must_use]
pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "action":{"enum":ACTIONS,"type":"string"},
        "program":{"type":"string","description":"debug target path; Delve accepts Go package directories"},
        "args":{"type":"array","items":{"type":"string"},"description":"program arguments"},
        "adapter":{"type":"string","description":"configured adapter id (gdb, lldb-dap, debugpy, dlv, rdbg, or dap.json entry)"},
        "cwd":{"type":"string"},
        "file":{"type":"string","description":"source file"},
        "line":{"type":"number","description":"source line"},
        "function":{"type":"string","description":"function name"},
        "name":{"type":"string","description":"variable or data name"},
        "condition":{"type":"string","description":"breakpoint condition"},
        "hit_condition":{"type":"string"},
        "expression":{"type":"string","description":"expression to evaluate"},
        "context":{"type":"string","description":"evaluate context: watch | repl | hover | variables | clipboard"},
        "frame_id":{"type":"number"},
        "scope_id":{"type":"number","description":"scope variables reference"},
        "variable_ref":{"type":"number","description":"variable reference"},
        "pid":{"type":"number","description":"process id for attach"},
        "port":{"type":"number","description":"remote attach port"},
        "host":{"type":"string","description":"remote attach host"},
        "levels":{"type":"number","description":"max stack frames"},
        "memory_reference":{"type":"string","description":"memory reference or address"},
        "instruction_reference":{"type":"string"},
        "instruction_count":{"type":"number"},
        "instruction_offset":{"type":"number"},
        "count":{"type":"number","description":"bytes to read"},
        "data":{"type":"string","description":"base64 memory payload"},
        "data_id":{"type":"string","description":"data breakpoint id"},
        "access_type":{"enum":["read","write","readWrite"],"type":"string"},
        "command":{"type":"string","description":"custom dap request command"},
        "arguments":{"type":"object","properties":{},"additionalProperties":{},"description":"custom request arguments"},
        "offset":{"type":"number"},
        "resolve_symbols":{"type":"boolean"},
        "allow_partial":{"type":"boolean"},
        "start_module":{"type":"number"},
        "module_count":{"type":"number"},
        "timeout":{"type":"number","description":"per-request timeout seconds"}
    },"required":["action"]})
}

/// Whether `action` is one of the read-tier actions.
#[must_use]
pub fn is_readonly(action: &str) -> bool {
    READONLY_ACTIONS.contains(&action)
}

/// Whether `action` is dispatched at all.
#[must_use]
pub fn is_known_action(action: &str) -> bool {
    ACTIONS.contains(&action)
}

/// Clamp a caller timeout to omp's `5..=300` seconds, defaulting to 30.
#[must_use]
pub fn clamp_timeout(timeout: Option<u64>) -> u64 {
    timeout
        .unwrap_or(TIMEOUT_DEFAULT_SECS)
        .clamp(TIMEOUT_MIN_SECS, TIMEOUT_MAX_SECS)
}

/// The `debug` tool, bound to the session working directory.
#[must_use]
pub fn tool(env: Arc<Env>) -> Arc<dyn AgentTool> {
    Arc::new(DebugTool {
        schema: schema(),
        env,
    })
}

struct DebugTool {
    schema: Value,
    env: Arc<Env>,
}

/// Combined caller-cancel + wall-clock deadline, as `omp-dap` expects it.
struct DeadlineCancel<'a> {
    abort: &'a AbortSignal,
    deadline: Instant,
}

impl Cancel for DeadlineCancel<'_> {
    fn is_cancelled(&self) -> bool {
        self.abort.is_aborted() || Instant::now() >= self.deadline
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let sleep = tokio::time::sleep_until(self.deadline.into());
        Box::pin(async move {
            tokio::select! {
                () = self.abort.aborted() => {}
                () = sleep => {}
            }
        })
    }
}

impl AgentTool for DebugTool {
    fn name(&self) -> &'static str {
        "debug"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Debug"
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // omp `concurrency: "exclusive"`: one debugger mutation at a time.
        Some(ToolExecutionMode::Sequential)
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        Box::pin(async move { self.run(params, &signal).await })
    }
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    action: String,
    program: Option<String>,
    args: Option<Vec<String>>,
    adapter: Option<String>,
    cwd: Option<String>,
    file: Option<String>,
    line: Option<i64>,
    function: Option<String>,
    name: Option<String>,
    condition: Option<String>,
    hit_condition: Option<String>,
    expression: Option<String>,
    context: Option<String>,
    frame_id: Option<i64>,
    scope_id: Option<i64>,
    variable_ref: Option<i64>,
    pid: Option<u32>,
    port: Option<u32>,
    host: Option<String>,
    levels: Option<i64>,
    memory_reference: Option<String>,
    instruction_reference: Option<String>,
    instruction_count: Option<i64>,
    instruction_offset: Option<i64>,
    count: Option<i64>,
    data: Option<String>,
    data_id: Option<String>,
    access_type: Option<String>,
    command: Option<String>,
    arguments: Option<Value>,
    offset: Option<i64>,
    resolve_symbols: Option<bool>,
    allow_partial: Option<bool>,
    start_module: Option<i64>,
    module_count: Option<i64>,
    timeout: Option<u64>,
}

impl Params {
    fn request_json(&self) -> Value {
        json!({
            "action": self.action,
            "program": self.program,
            "args": self.args,
            "adapter": self.adapter,
            "cwd": self.cwd,
            "file": self.file,
            "line": self.line,
            "function": self.function,
            "name": self.name,
            "condition": self.condition,
            "hit_condition": self.hit_condition,
            "expression": self.expression,
            "context": self.context,
            "frame_id": self.frame_id,
            "scope_id": self.scope_id,
            "variable_ref": self.variable_ref,
            "pid": self.pid,
            "port": self.port,
            "host": self.host,
            "levels": self.levels,
            "memory_reference": self.memory_reference,
            "instruction_reference": self.instruction_reference,
            "instruction_count": self.instruction_count,
            "instruction_offset": self.instruction_offset,
            "count": self.count,
            "data": self.data,
            "data_id": self.data_id,
            "access_type": self.access_type,
            "command": self.command,
            "arguments": self.arguments,
            "offset": self.offset,
            "resolve_symbols": self.resolve_symbols,
            "allow_partial": self.allow_partial,
            "start_module": self.start_module,
            "module_count": self.module_count,
            "timeout": self.timeout,
        })
    }
}

fn text_result(text: impl Into<String>, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![pa_agent::types::ToolResultContent::text(text.into())],
        details,
        terminate: None,
        is_error: None,
    }
}

/// Breakpoints the tool last installed, per target.
///
/// The session manager keeps its own records but exposes counts, not the
/// request list a `setBreakpoints` replacement needs, so the tool mirrors what
/// it installed: removing one breakpoint means resending the rest. Only one
/// session is active at a time (`debug.md`), so a per-target key is enough.
#[derive(Debug, Default)]
struct Mirrors {
    sources: HashMap<String, Vec<DapSourceBreakpoint>>,
    functions: Vec<DapFunctionBreakpoint>,
    instructions: Vec<DapInstructionBreakpoint>,
    data: Vec<DapDataBreakpoint>,
}

static MIRROR: LazyLock<Mutex<Mirrors>> = LazyLock::new(|| Mutex::new(Mirrors::default()));

fn with_mirror<T>(f: impl FnOnce(&mut Mirrors) -> T) -> T {
    let mut guard = MIRROR.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// Resolve `path` against `cwd` unless it is already absolute.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

fn required<T>(value: Option<T>, name: &str, action: &str) -> Result<T> {
    value.ok_or_else(|| anyhow!("`{name}` is required for `{action}`."))
}

impl DebugTool {
    async fn run(self: &Arc<Self>, raw: Value, signal: &AbortSignal) -> Result<AgentToolResult> {
        let params: Params =
            serde_json::from_value(raw).map_err(|e| anyhow!("Invalid debug arguments: {e}"))?;
        if !is_known_action(&params.action) {
            return Err(anyhow!("Unknown debug action `{}`.", params.action));
        }

        let timeout_secs = clamp_timeout(params.timeout);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let cancel = DeadlineCancel {
            abort: signal,
            deadline,
        };

        match self.dispatch(&params, &cancel).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if is_abort_error(&e) || signal.is_aborted() {
                    return Err(e);
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // TS-parity per-action dispatch
    async fn dispatch(
        &self,
        params: &Params,
        cancel: &DeadlineCancel<'_>,
    ) -> Result<AgentToolResult> {
        let action = params.action.as_str();
        let cwd = self.cwd(params);
        let cwd_display = cwd.to_string_lossy().into_owned();
        let details = |extra: Value| {
            let mut map = Map::new();
            map.insert("action".to_string(), json!(action));
            map.insert("success".to_string(), json!(true));
            map.insert("request".to_string(), params.request_json());
            if let Value::Object(extra) = extra {
                for (key, value) in extra {
                    map.insert(key, value);
                }
            }
            Value::Object(map)
        };

        match action {
            // -----------------------------------------------------------------
            // Session lifecycle
            // -----------------------------------------------------------------
            "launch" => {
                let program = required(params.program.as_deref(), "program", action)?;
                let program_path = resolve(&cwd, program);
                let kind = config::classify_launch_program(&program_path);
                let adapter = match config::select_launch_adapter(
                    &program_path,
                    &cwd,
                    params.adapter.as_deref(),
                    kind,
                ) {
                    LaunchAdapterSelection::Adapter(adapter) => adapter,
                    LaunchAdapterSelection::Unavailable {
                        adapter_name,
                        command,
                    } => {
                        return Err(anyhow!(
                            "Debug adapter `{adapter_name}` is configured but `{command}` is not on PATH."
                        ));
                    }
                    LaunchAdapterSelection::None => {
                        return Err(anyhow!(
                            "No debug adapter matched `{}`. Pass `adapter` or add one to dap.json.",
                            program_path.display()
                        ));
                    }
                };
                let extra_launch_arguments =
                    config::resolve_launch_overrides(&adapter, &program_path, kind);
                let outcome = SHARED
                    .launch(
                        DapLaunchSessionOptions {
                            adapter,
                            program: program_path.to_string_lossy().into_owned(),
                            args: params.args.clone(),
                            cwd: cwd_display,
                            extra_launch_arguments,
                        },
                        Some(cancel),
                    )
                    .await?;
                let output = SHARED
                    .output(Some(&outcome.snapshot.id))
                    .ok()
                    .map(|snapshot| snapshot.output);
                Ok(text_result(
                    format::build_outcome_text(&outcome, output.as_deref()),
                    details(json!({
                        "state": outcome.state.as_str(),
                        "timed_out": outcome.timed_out,
                        "session": outcome.snapshot.id,
                    })),
                ))
            }
            "attach" => {
                let port = params.port.and_then(|port| u16::try_from(port).ok());
                if params.pid.is_none() && port.is_none() {
                    return Err(anyhow!("`pid` or `port` is required for `attach`."));
                }
                let adapter = config::select_attach_adapter(
                    &cwd,
                    params.adapter.as_deref(),
                    DapAttachTarget {
                        pid: params.pid,
                        port,
                        host: None,
                    },
                )
                .await
                .ok_or_else(|| anyhow!("No debug adapter is available for this attach target."))?;
                let outcome = SHARED
                    .attach(
                        DapAttachSessionOptions {
                            adapter,
                            cwd: cwd_display,
                            pid: params.pid,
                            port,
                            host: params.host.clone(),
                        },
                        Some(cancel),
                    )
                    .await?;
                let output = SHARED
                    .output(Some(&outcome.snapshot.id))
                    .ok()
                    .map(|snapshot| snapshot.output);
                Ok(text_result(
                    format::build_outcome_text(&outcome, output.as_deref()),
                    details(json!({
                        "state": outcome.state.as_str(),
                        "timed_out": outcome.timed_out,
                        "session": outcome.snapshot.id,
                    })),
                ))
            }
            "terminate" => {
                let summary = SHARED.disconnect(None, true, Some(cancel)).await?;
                Ok(text_result(
                    format::format_session_snapshot(&summary),
                    details(json!({"session": summary.id})),
                ))
            }
            "sessions" => {
                let summaries = SHARED.session_summaries();
                let active = SHARED.active_session_id();
                Ok(text_result(
                    format::format_sessions(&summaries, active.as_deref()),
                    details(json!({"count": summaries.len()})),
                ))
            }

            // -----------------------------------------------------------------
            // Source / function breakpoints
            // -----------------------------------------------------------------
            "set_breakpoint" => {
                if let Some(function) = params.function.clone() {
                    if params.file.is_none() {
                        let mut functions = with_mirror(|m| m.functions.clone());
                        functions.retain(|entry| entry.name != function);
                        functions.push(DapFunctionBreakpoint {
                            name: function.clone(),
                            condition: params.condition.clone(),
                            hit_condition: params.hit_condition.clone(),
                        });
                        let mutation = SHARED
                            .set_function_breakpoints(None, &functions, Some(cancel))
                            .await?;
                        with_mirror(|m| m.functions = functions);
                        return Ok(text_result(
                            render_function_breakpoints(&mutation.breakpoints),
                            details(json!({"session": mutation.snapshot.id})),
                        ));
                    }
                }

                let file = required(params.file.as_deref(), "file", action)?;
                let line = required(params.line, "line", action)?;
                let path = resolve(&cwd, file);
                let key = path.to_string_lossy().into_owned();
                let mut breakpoints =
                    with_mirror(|m| m.sources.get(&key).cloned().unwrap_or_default());
                breakpoints.retain(|entry| entry.line != line);
                breakpoints.push(DapSourceBreakpoint {
                    line,
                    column: None,
                    condition: params.condition.clone(),
                    hit_condition: params.hit_condition.clone(),
                    log_message: None,
                });
                breakpoints.sort_by_key(|entry| entry.line);
                let mutation = SHARED
                    .set_breakpoints(None, &key, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| {
                    m.sources.insert(key, breakpoints);
                });
                Ok(text_result(
                    format::format_breakpoints(&mutation.source_path, &mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }
            "remove_breakpoint" => {
                if let Some(function) = params.function.clone() {
                    let mut functions = with_mirror(|m| m.functions.clone());
                    functions.retain(|entry| entry.name != function);
                    let mutation = SHARED
                        .set_function_breakpoints(None, &functions, Some(cancel))
                        .await?;
                    with_mirror(|m| m.functions = functions);
                    return Ok(text_result(
                        render_function_breakpoints(&mutation.breakpoints),
                        details(json!({"session": mutation.snapshot.id})),
                    ));
                }

                let file = required(params.file.as_deref(), "file", action)?;
                let path = resolve(&cwd, file);
                let key = path.to_string_lossy().into_owned();
                let mut breakpoints =
                    with_mirror(|m| m.sources.get(&key).cloned().unwrap_or_default());
                match params.line {
                    Some(line) => breakpoints.retain(|entry| entry.line != line),
                    None => breakpoints.clear(),
                }
                let mutation = SHARED
                    .set_breakpoints(None, &key, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| {
                    m.sources.insert(key, breakpoints);
                });
                Ok(text_result(
                    format::format_breakpoints(&mutation.source_path, &mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }

            // -----------------------------------------------------------------
            // Instruction breakpoints
            // -----------------------------------------------------------------
            "set_instruction_breakpoint" => {
                let reference = required(
                    params.instruction_reference.as_deref(),
                    "instruction_reference",
                    action,
                )?;
                let mut breakpoints = with_mirror(|m| m.instructions.clone());
                breakpoints.retain(|entry| entry.instruction_reference != reference);
                breakpoints.push(DapInstructionBreakpoint {
                    instruction_reference: reference.to_string(),
                    offset: params.offset,
                    condition: params.condition.clone(),
                    hit_condition: params.hit_condition.clone(),
                });
                let mutation = SHARED
                    .set_instruction_breakpoints(None, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| m.instructions = breakpoints);
                Ok(text_result(
                    render_instruction_breakpoints(&mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }
            "remove_instruction_breakpoint" => {
                let reference = required(
                    params.instruction_reference.as_deref(),
                    "instruction_reference",
                    action,
                )?;
                let mut breakpoints = with_mirror(|m| m.instructions.clone());
                breakpoints.retain(|entry| entry.instruction_reference != reference);
                let mutation = SHARED
                    .set_instruction_breakpoints(None, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| m.instructions = breakpoints);
                Ok(text_result(
                    render_instruction_breakpoints(&mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }

            // -----------------------------------------------------------------
            // Data breakpoints
            // -----------------------------------------------------------------
            "data_breakpoint_info" => {
                let name = required(params.name.as_deref(), "name", action)?;
                let response = SHARED
                    .data_breakpoint_info(
                        None,
                        name,
                        params.variable_ref,
                        params.frame_id,
                        Some(cancel),
                    )
                    .await?;
                let mut lines = vec![format!(
                    "dataId: {}",
                    response.data_id.as_deref().unwrap_or("<none>")
                )];
                if !response.description.is_empty() {
                    lines.push(format!("description: {}", response.description));
                }
                if let Some(access_types) = &response.access_types {
                    let names: Vec<&str> = access_types.iter().map(|t| t.as_str()).collect();
                    lines.push(format!("accessTypes: {}", names.join(", ")));
                }
                if let Some(can_persist) = response.can_persist {
                    lines.push(format!("canPersist: {can_persist}"));
                }
                Ok(text_result(
                    lines.join("\n"),
                    details(json!({"data_id": response.data_id})),
                ))
            }
            "set_data_breakpoint" => {
                let data_id = required(params.data_id.as_deref(), "data_id", action)?;
                let access_type = match params.access_type.as_deref() {
                    Some(raw) => Some(parse_access_type(raw).ok_or_else(|| {
                        anyhow!("`access_type` must be read, write or readWrite.")
                    })?),
                    None => None,
                };
                let mut breakpoints = with_mirror(|m| m.data.clone());
                breakpoints.retain(|entry| entry.data_id != data_id);
                breakpoints.push(DapDataBreakpoint {
                    data_id: data_id.to_string(),
                    access_type,
                    condition: params.condition.clone(),
                    hit_condition: params.hit_condition.clone(),
                });
                let mutation = SHARED
                    .set_data_breakpoints(None, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| m.data = breakpoints);
                Ok(text_result(
                    render_data_breakpoints(&mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }
            "remove_data_breakpoint" => {
                let data_id = required(params.data_id.as_deref(), "data_id", action)?;
                let mut breakpoints = with_mirror(|m| m.data.clone());
                breakpoints.retain(|entry| entry.data_id != data_id);
                let mutation = SHARED
                    .set_data_breakpoints(None, &breakpoints, Some(cancel))
                    .await?;
                with_mirror(|m| m.data = breakpoints);
                Ok(text_result(
                    render_data_breakpoints(&mutation.breakpoints),
                    details(json!({"session": mutation.snapshot.id})),
                ))
            }

            // -----------------------------------------------------------------
            // Execution control
            // -----------------------------------------------------------------
            "continue" => {
                let outcome = SHARED
                    .continue_exec(None, None, false, Some(cancel))
                    .await?;
                let output = SHARED
                    .output(Some(&outcome.snapshot.id))
                    .ok()
                    .map(|snapshot| snapshot.output);
                Ok(text_result(
                    format::build_outcome_text(&outcome, output.as_deref()),
                    details(json!({
                        "state": outcome.state.as_str(),
                        "timed_out": outcome.timed_out,
                        "session": outcome.snapshot.id,
                    })),
                ))
            }
            "step_over" | "step_in" | "step_out" => {
                let outcome = match action {
                    "step_in" => SHARED.step_in(None, None, None, Some(cancel)).await?,
                    "step_out" => SHARED.step_out(None, None, None, Some(cancel)).await?,
                    _ => SHARED.next(None, None, None, Some(cancel)).await?,
                };
                let output = SHARED
                    .output(Some(&outcome.snapshot.id))
                    .ok()
                    .map(|snapshot| snapshot.output);
                Ok(text_result(
                    format::build_outcome_text(&outcome, output.as_deref()),
                    details(json!({
                        "state": outcome.state.as_str(),
                        "timed_out": outcome.timed_out,
                        "session": outcome.snapshot.id,
                    })),
                ))
            }
            "pause" => {
                let summary = SHARED.pause(None, None, Some(cancel)).await?;
                Ok(text_result(
                    format::format_session_snapshot(&summary),
                    details(json!({"session": summary.id})),
                ))
            }

            // -----------------------------------------------------------------
            // Inspection
            // -----------------------------------------------------------------
            "evaluate" => {
                let expression = required(params.expression.as_deref(), "expression", action)?;
                let response = SHARED
                    .evaluate(
                        None,
                        expression,
                        params.frame_id,
                        params.context.as_deref(),
                        Some(cancel),
                    )
                    .await?;
                Ok(text_result(
                    format::format_evaluation(&response),
                    details(json!({"type": response.result_type})),
                ))
            }
            "stack_trace" => {
                let frames = SHARED
                    .stack_trace(None, None, None, params.levels, Some(cancel))
                    .await?;
                Ok(text_result(
                    format::format_stack_frames(&frames),
                    details(json!({"count": frames.len()})),
                ))
            }
            "threads" => {
                let threads = SHARED.threads(None, Some(cancel)).await?;
                let current = SHARED.status().ok().and_then(|summary| summary.thread_id);
                Ok(text_result(
                    format::format_threads(&threads, current),
                    details(json!({"count": threads.len()})),
                ))
            }
            "scopes" => {
                let scopes = SHARED.scopes(None, params.frame_id, Some(cancel)).await?;
                Ok(text_result(
                    format::format_scopes(&scopes),
                    details(json!({"count": scopes.len()})),
                ))
            }
            "variables" => {
                let reference = params.variable_ref.or(params.scope_id).ok_or_else(|| {
                    anyhow!("`variable_ref` or `scope_id` is required for `variables`.")
                })?;
                let variables = SHARED
                    .variables(None, reference, None, None, Some(cancel))
                    .await?;
                Ok(text_result(
                    format::format_variables(&variables),
                    details(json!({"count": variables.len()})),
                ))
            }
            "disassemble" => {
                let reference = params
                    .memory_reference
                    .as_deref()
                    .or(params.instruction_reference.as_deref())
                    .ok_or_else(|| {
                        anyhow!("`memory_reference` or `instruction_reference` is required for `disassemble`.")
                    })?;
                let instructions = SHARED
                    .disassemble(
                        None,
                        reference,
                        params.offset,
                        params.instruction_offset,
                        params
                            .instruction_count
                            .unwrap_or(DEFAULT_INSTRUCTION_COUNT),
                        params.resolve_symbols.unwrap_or(true),
                        Some(cancel),
                    )
                    .await?;
                Ok(text_result(
                    format::format_disassembly(&instructions),
                    details(json!({"count": instructions.len()})),
                ))
            }
            "read_memory" => {
                let reference = required(
                    params.memory_reference.as_deref(),
                    "memory_reference",
                    action,
                )?;
                let response = SHARED
                    .read_memory(
                        None,
                        reference,
                        params.offset,
                        params.count.unwrap_or(DEFAULT_READ_BYTES),
                        Some(cancel),
                    )
                    .await?;
                Ok(text_result(
                    format::format_memory_read(&response),
                    details(json!({"address": response.address})),
                ))
            }
            "write_memory" => {
                let reference = required(
                    params.memory_reference.as_deref(),
                    "memory_reference",
                    action,
                )?;
                let data = required(params.data.as_deref(), "data", action)?;
                let response = SHARED
                    .write_memory(
                        None,
                        reference,
                        params.offset,
                        data,
                        params.allow_partial.unwrap_or(false),
                        Some(cancel),
                    )
                    .await?;
                let mut lines = vec![format!("wrote: {}", response.bytes_written.unwrap_or(0))];
                if let Some(offset) = response.offset {
                    lines.push(format!("offset: {offset}"));
                }
                Ok(text_result(
                    lines.join("\n"),
                    details(json!({"bytes_written": response.bytes_written})),
                ))
            }
            "modules" => {
                let modules = SHARED
                    .modules(None, params.start_module, params.module_count, Some(cancel))
                    .await?;
                Ok(text_result(
                    format::format_modules(&modules),
                    details(json!({"count": modules.len()})),
                ))
            }
            "loaded_sources" => {
                let sources = SHARED.loaded_sources(None, Some(cancel)).await?;
                Ok(text_result(
                    format::format_loaded_sources(&sources),
                    details(json!({"count": sources.len()})),
                ))
            }
            "custom_request" => {
                let command = required(params.command.as_deref(), "command", action)?;
                let body = SHARED
                    .custom_request(None, command, params.arguments.clone(), Some(cancel))
                    .await?;
                Ok(text_result(
                    format::format_custom_response(&body),
                    details(json!({"command": command})),
                ))
            }
            "output" => {
                let snapshot = SHARED.output(None)?;
                let mut text = format::format_session_snapshot(&snapshot.snapshot);
                if !snapshot.output.is_empty() {
                    text.push('\n');
                    text.push_str(&snapshot.output);
                }
                Ok(text_result(
                    text,
                    details(json!({"session": snapshot.snapshot.id})),
                ))
            }
            other => Err(anyhow!("Unknown debug action `{other}`.")),
        }
    }

    /// Working directory for the call: `cwd` when given, else the session cwd.
    fn cwd(&self, params: &Params) -> PathBuf {
        match params.cwd.as_deref() {
            Some(cwd) => resolve(&self.env.cwd, cwd),
            None => self.env.cwd.clone(),
        }
    }
}

// =============================================================================
// Renders the `omp-dap` format module does not own
// =============================================================================

/// One line per function breakpoint; the session keeps no renderer for these.
fn render_function_breakpoints(
    breakpoints: &[omp_dap::types::DapFunctionBreakpointRecord],
) -> String {
    let mut lines = vec![format!("function breakpoints: {}", breakpoints.len())];
    if breakpoints.is_empty() {
        lines.push("  none".to_string());
        return lines.join("\n");
    }
    for entry in breakpoints {
        lines.push(format!(
            "  {} {}{}",
            if entry.verified { "[x]" } else { "[ ]" },
            entry.name,
            entry
                .message
                .as_deref()
                .map(|message| format!(" ({message})"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

/// One line per instruction breakpoint.
fn render_instruction_breakpoints(
    breakpoints: &[omp_dap::types::DapInstructionBreakpointRecord],
) -> String {
    let mut lines = vec![format!("instruction breakpoints: {}", breakpoints.len())];
    if breakpoints.is_empty() {
        lines.push("  none".to_string());
        return lines.join("\n");
    }
    for entry in breakpoints {
        lines.push(format!(
            "  {} {}{}",
            if entry.verified { "[x]" } else { "[ ]" },
            entry.instruction_reference,
            entry
                .offset
                .map(|offset| format!("+{offset}"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

/// One line per data breakpoint.
fn render_data_breakpoints(breakpoints: &[omp_dap::types::DapDataBreakpointRecord]) -> String {
    let mut lines = vec![format!("data breakpoints: {}", breakpoints.len())];
    if breakpoints.is_empty() {
        lines.push("  none".to_string());
        return lines.join("\n");
    }
    for entry in breakpoints {
        lines.push(format!(
            "  {} {}{}",
            if entry.verified { "[x]" } else { "[ ]" },
            entry.data_id,
            entry
                .access_type
                .map(|access| format!(" ({})", access.as_str()))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 35 optional parameters omp's schema declares, in order.
    const OPTIONAL_PARAMS: [&str; 35] = [
        "program",
        "args",
        "adapter",
        "cwd",
        "file",
        "line",
        "function",
        "name",
        "condition",
        "hit_condition",
        "expression",
        "context",
        "frame_id",
        "scope_id",
        "variable_ref",
        "pid",
        "port",
        "host",
        "levels",
        "memory_reference",
        "instruction_reference",
        "instruction_count",
        "instruction_offset",
        "count",
        "data",
        "data_id",
        "access_type",
        "command",
        "arguments",
        "offset",
        "resolve_symbols",
        "allow_partial",
        "start_module",
        "module_count",
        "timeout",
    ];

    fn properties() -> Map<String, Value> {
        schema()
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .expect("schema has properties")
    }

    #[test]
    fn schema_matches_omp_shape() {
        let schema = schema();
        assert_eq!(schema.get("type"), Some(&json!("object")));
        assert_eq!(schema.get("required"), Some(&json!(["action"])));
        assert!(
            schema.get("additionalProperties").is_none(),
            "omp leaves the debug schema open"
        );

        let properties = properties();
        assert_eq!(properties.len(), 36, "action + 35 optional parameters");
        assert!(properties.contains_key("action"));
        for name in OPTIONAL_PARAMS {
            assert!(properties.contains_key(name), "missing parameter `{name}`");
        }

        let action = &properties["action"];
        assert_eq!(action.get("type"), Some(&json!("string")));
        let values = action
            .get("enum")
            .and_then(Value::as_array)
            .expect("action enum");
        let names: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, ACTIONS, "enum order is omp's declaration order");
        assert_eq!(names.len(), 28);

        // Nested shapes omp spells out verbatim.
        assert_eq!(
            properties["arguments"],
            json!({"type":"object","properties":{},"additionalProperties":{},"description":"custom request arguments"})
        );
        assert_eq!(properties["args"]["items"], json!({"type":"string"}));
        assert_eq!(
            properties["access_type"]["enum"],
            json!(["read", "write", "readWrite"])
        );
        assert_eq!(
            properties["line"],
            json!({"type":"number","description":"source line"})
        );
    }

    #[test]
    fn description_is_verbatim() {
        assert_eq!(DESCRIPTION.lines().count(), 3);
        assert!(DESCRIPTION.starts_with("Debugger access. Prefer over bash for program state"));
        assert!(DESCRIPTION.ends_with("Directories need a directory-capable adapter (e.g. `dlv`)."));
        assert!(!DESCRIPTION.ends_with('\n'));
    }

    #[test]
    fn approval_tiers_split_read_and_exec() {
        assert_eq!(READONLY_ACTIONS.len(), 10);
        for action in READONLY_ACTIONS {
            assert!(is_known_action(action), "`{action}` is not a real action");
            assert!(is_readonly(action));
        }
        // Every mutating action runs at the exec tier.
        for action in ACTIONS {
            if READONLY_ACTIONS.contains(&action) {
                continue;
            }
            assert!(!is_readonly(action), "`{action}` must require exec");
        }
        assert!(!is_readonly("launch"));
        assert!(!is_readonly("continue"));
        assert!(!is_readonly("write_memory"));
        assert!(is_readonly("read_memory"));
        assert!(is_readonly("sessions"));
        assert!(!is_readonly("not-an-action"));
    }

    #[test]
    fn timeout_clamps_to_omp_bounds() {
        assert_eq!(clamp_timeout(None), 30);
        assert_eq!(clamp_timeout(Some(0)), 5);
        assert_eq!(clamp_timeout(Some(1)), 5);
        assert_eq!(clamp_timeout(Some(4)), 5);
        assert_eq!(clamp_timeout(Some(5)), 5);
        assert_eq!(clamp_timeout(Some(45)), 45);
        assert_eq!(clamp_timeout(Some(300)), 300);
        assert_eq!(clamp_timeout(Some(301)), 300);
        assert_eq!(clamp_timeout(Some(u64::MAX)), 300);
    }

    #[test]
    fn every_action_is_dispatchable() {
        // The dispatch `match` ends in a catch-all error; the guard in `run`
        // rejects anything outside ACTIONS, so the two sets must agree.
        for action in ACTIONS {
            assert!(is_known_action(action));
        }
        assert!(!is_known_action(""));
        assert!(!is_known_action("step"));
        assert_eq!(ACTIONS.len(), 28);
        // No duplicates: a duplicate would make one enum entry unreachable.
        let mut sorted = ACTIONS;
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate action in ACTIONS");
        }
    }

    #[test]
    fn params_round_trip_and_defaults() {
        let params: Params = serde_json::from_value(json!({
            "action": "set_breakpoint",
            "file": "src/main.rs",
            "line": 12,
            "condition": "i == 3",
            "hit_condition": "5",
        }))
        .expect("params parse");
        assert_eq!(params.action, "set_breakpoint");
        assert_eq!(params.line, Some(12));
        assert_eq!(params.condition.as_deref(), Some("i == 3"));
        assert!(params.args.is_none());
        assert_eq!(clamp_timeout(params.timeout), 30);

        // Unknown keys survive (the schema is open), action is required.
        assert!(serde_json::from_value::<Params>(json!({"timeout": 10})).is_err());
        let request = params.request_json();
        assert_eq!(request["action"], json!("set_breakpoint"));
    }

    #[test]
    fn mirror_keeps_one_entry_per_target() {
        with_mirror(|m| {
            m.sources.clear();
            m.functions.clear();
            m.instructions.clear();
            m.data.clear();
        });

        let key = "/tmp/mirror-test.rs";
        with_mirror(|m| {
            m.sources.insert(
                key.to_string(),
                vec![DapSourceBreakpoint {
                    line: 3,
                    column: None,
                    condition: None,
                    hit_condition: None,
                    log_message: None,
                }],
            );
        });
        let stored = with_mirror(|m| m.sources.get(key).cloned().unwrap_or_default());
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].line, 3);
        with_mirror(|m| {
            m.sources.remove(key);
        });
        assert!(with_mirror(|m| m.sources.is_empty()));
    }

    #[test]
    fn renders_empty_breakpoint_lists() {
        assert_eq!(
            render_function_breakpoints(&[]),
            "function breakpoints: 0\n  none"
        );
        assert_eq!(
            render_instruction_breakpoints(&[]),
            "instruction breakpoints: 0\n  none"
        );
        assert_eq!(render_data_breakpoints(&[]), "data breakpoints: 0\n  none");
    }
}
