//! DAP wire and session types (port of omp `src/dap/types.ts`).
//!
//! Every struct mirrors the protocol field names verbatim (`camelCase`), so a
//! `serde_json` round-trip of a request/response body is byte-compatible with
//! the TS client. Optional fields are `Option` and skipped when absent: the
//! protocol treats "absent" and "null" differently for several fields
//! (`dataId`, `accessTypes`), so nothing is serialized as an explicit `null`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// =============================================================================
// Protocol messages
// =============================================================================

/// One framed DAP message: request, response or event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DapMessage {
    /// Adapter-bound request (`seq`, `command`, `arguments`).
    Request {
        seq: u64,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
    },
    /// Adapter-bound response (`request_seq`, `success`, `command`, `body`).
    Response {
        seq: u64,
        request_seq: u64,
        success: bool,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
    },
    /// Adapter-bound event (`event`, `body`).
    Event {
        seq: u64,
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
    },
}

impl DapMessage {
    /// The protocol `type` discriminator.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Request { .. } => "request",
            Self::Response { .. } => "response",
            Self::Event { .. } => "event",
        }
    }
}

/// `seq` of a decoded message, whichever kind it is.
#[must_use]
pub fn message_seq(message: &Value) -> Option<u64> {
    message.get("seq").and_then(Value::as_u64)
}

/// Protocol `type` of a raw message value.
#[must_use]
pub fn message_type(message: &Value) -> Option<&str> {
    message.get("type").and_then(Value::as_str)
}

// =============================================================================
// Session-facing enums
// =============================================================================

/// Lifecycle of one debug session (omp `DapSessionStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DapSessionStatus {
    /// Adapter spawned, `initialize` in flight.
    Launching,
    /// `initialized` seen; breakpoints being set before `configurationDone`.
    Configuring,
    /// Paused at a stop event.
    Stopped,
    /// Target is executing.
    Running,
    /// Target or adapter is gone.
    Terminated,
}

impl DapSessionStatus {
    /// Lowercase wire name (omp's status strings).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Configuring => "configuring",
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Terminated => "terminated",
        }
    }
}

impl std::fmt::Display for DapSessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a `continue`/step settled (omp `DapContinueOutcome["state"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DapRunState {
    /// Still executing when the budget ran out.
    Running,
    /// Paused at a stop.
    Stopped,
    /// Program exited.
    Terminated,
}

impl DapRunState {
    /// Lowercase wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Terminated => "terminated",
        }
    }
}

/// Data breakpoint access mode (omp `DapDataBreakpoint["accessType"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DapAccessType {
    /// Break on reads.
    Read,
    /// Break on writes.
    Write,
    /// Break on both.
    ReadWrite,
}

impl DapAccessType {
    /// Parse the tool-facing spelling (`read`/`write`/`readWrite`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "readWrite" => Some(Self::ReadWrite),
            _ => None,
        }
    }

    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "readWrite",
        }
    }
}

/// Evaluate context (omp `DapEvaluateArguments["context"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DapEvaluateContext {
    /// Watch pane expression.
    Watch,
    /// Debug console / repl.
    Repl,
    /// Hover tooltip.
    Hover,
    /// Clipboard evaluation.
    Clipboard,
    /// Variables pane filter.
    Variables,
}

impl DapEvaluateContext {
    /// Parse the tool-facing spelling; `None` for an unknown context.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "watch" => Some(Self::Watch),
            "repl" => Some(Self::Repl),
            "hover" => Some(Self::Hover),
            "clipboard" => Some(Self::Clipboard),
            "variables" => Some(Self::Variables),
            _ => None,
        }
    }

    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Repl => "repl",
            Self::Hover => "hover",
            Self::Clipboard => "clipboard",
            Self::Variables => "variables",
        }
    }
}

/// Transport an adapter speaks (omp `connectMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DapConnectMode {
    /// DAP over the adapter's stdin/stdout pipes.
    #[default]
    Stdio,
    /// Adapter opens a unix socket / dials back (Delve).
    Socket,
    /// Adapter listens on a TCP port substituted for `${port}` in `args`.
    Tcp,
}

/// How the launch `program` resolves on disk (omp `LaunchProgramKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchProgramKind {
    /// A regular file.
    File,
    /// A directory (Go package path for `dlv`).
    Directory,
    /// Does not exist; treated like a file.
    Missing,
}

// =============================================================================
// Capabilities
// =============================================================================

/// Subset of the adapter's `initialize` capabilities the tool acts on.
///
/// Unknown keys are preserved in [`DapCapabilities::extra`] so a `capabilities`
/// dump stays faithful; the typed fields are the ones with behaviour attached.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapCapabilities {
    pub supports_configuration_done_request: Option<bool>,
    pub supports_function_breakpoints: Option<bool>,
    pub supports_conditional_breakpoints: Option<bool>,
    pub supports_terminate_request: Option<bool>,
    pub supports_terminate_threads_request: Option<bool>,
    pub supports_evaluate_for_hovers: Option<bool>,
    pub supports_set_variable: Option<bool>,
    pub supports_restart_request: Option<bool>,
    pub supports_completions_request: Option<bool>,
    pub supports_log_points: Option<bool>,
    pub supports_disassemble_request: Option<bool>,
    pub supports_read_memory_request: Option<bool>,
    pub supports_write_memory_request: Option<bool>,
    pub supports_modules_request: Option<bool>,
    pub supports_loaded_sources_request: Option<bool>,
    pub supports_exception_info_request: Option<bool>,
    pub supports_instruction_breakpoints: Option<bool>,
    pub supports_data_breakpoints: Option<bool>,
    pub supports_stepping_granularity: Option<bool>,
    pub supports_clipboard_context: Option<bool>,
    /// Every key not modeled above, verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A capability the tool gates an action on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DapCapability {
    /// `supportsInstructionBreakpoints`.
    InstructionBreakpoints,
    /// `supportsDataBreakpoints`.
    DataBreakpoints,
    /// `supportsDisassembleRequest`.
    Disassemble,
    /// `supportsReadMemoryRequest`.
    ReadMemory,
    /// `supportsWriteMemoryRequest`.
    WriteMemory,
    /// `supportsModulesRequest`.
    Modules,
    /// `supportsLoadedSourcesRequest`.
    LoadedSources,
    /// `supportsTerminateRequest`.
    Terminate,
}

impl DapCapability {
    /// Human wording used in the refusal message (omp `requireCapability`).
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::InstructionBreakpoints => "instruction breakpoints",
            Self::DataBreakpoints => "data breakpoints",
            Self::Disassemble => "disassembly",
            Self::ReadMemory => "memory reads",
            Self::WriteMemory => "memory writes",
            Self::Modules => "module introspection",
            Self::LoadedSources => "loaded sources",
            Self::Terminate => "terminate requests",
        }
    }
}

impl DapCapabilities {
    /// Whether `capability` is advertised as supported.
    #[must_use]
    pub fn supports(&self, capability: DapCapability) -> bool {
        let flag = match capability {
            DapCapability::InstructionBreakpoints => self.supports_instruction_breakpoints,
            DapCapability::DataBreakpoints => self.supports_data_breakpoints,
            DapCapability::Disassemble => self.supports_disassemble_request,
            DapCapability::ReadMemory => self.supports_read_memory_request,
            DapCapability::WriteMemory => self.supports_write_memory_request,
            DapCapability::Modules => self.supports_modules_request,
            DapCapability::LoadedSources => self.supports_loaded_sources_request,
            DapCapability::Terminate => self.supports_terminate_request,
        };
        flag == Some(true)
    }

    /// `supportsConfigurationDoneRequest` (gates the `configurationDone` handshake).
    #[must_use]
    pub fn needs_configuration_done(&self) -> bool {
        self.supports_configuration_done_request == Some(true)
    }
}

// =============================================================================
// Wire payload types
// =============================================================================

/// A source file reference (`Source`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reference: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_data: Option<Value>,
}

impl DapSource {
    /// `path` when present, else `name`.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.path.as_deref().or(self.name.as_deref())
    }

    /// `label` with `:line[:column]` appended when a line is known.
    #[must_use]
    pub fn location(&self, line: Option<i64>, column: Option<i64>) -> Option<String> {
        let base = self.label()?;
        let Some(line) = line else {
            return Some(base.to_string());
        };
        match column {
            Some(column) => Some(format!("{base}:{line}:{column}")),
            None => Some(format!("{base}:{line}")),
        }
    }
}

/// A `setBreakpoints` reply entry (`Breakpoint`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapBreakpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}

/// One requested source breakpoint (`SourceBreakpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapSourceBreakpoint {
    pub line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

/// One requested function breakpoint (`FunctionBreakpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapFunctionBreakpoint {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

/// One requested instruction breakpoint (`InstructionBreakpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapInstructionBreakpoint {
    pub instruction_reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

/// One requested data breakpoint (`DataBreakpoint`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapDataBreakpoint {
    pub data_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_type: Option<DapAccessType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

/// `dataBreakpointInfo` reply.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapDataBreakpointInfoResponse {
    /// `None` when the adapter answered `dataId: null` (not available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_id: Option<String>,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_types: Option<Vec<DapAccessType>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_persist: Option<bool>,
}

/// A stack frame (`StackFrame`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapStackFrame {
    pub id: i64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    pub line: i64,
    pub column: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_pointer_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
}

/// A scope (`Scope`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapScope {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
    pub variables_reference: i64,
    pub expensive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<i64>,
}

/// A variable (`Variable`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub var_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluate_name: Option<String>,
    pub variables_reference: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_variables: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_variables: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_reference: Option<String>,
}

/// One disassembled instruction (`DisassembledInstruction`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapDisassembledInstruction {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_bytes: Option<String>,
    pub instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<i64>,
}

/// A loaded module (`Module`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapModule {
    pub id: Value,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_optimized: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_user_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_time_stamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_range: Option<String>,
}

/// A thread (`Thread`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapThread {
    pub id: i64,
    pub name: String,
}

/// An `evaluate` reply (`EvaluateResponse`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapEvaluateResponse {
    pub result: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<Value>,
    pub variables_reference: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_variables: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_variables: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_reference: Option<String>,
}

/// `readMemory` reply (`ReadMemoryResponse`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapReadMemoryResponse {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// `writeMemory` reply (`WriteMemoryResponse`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapWriteMemoryResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<i64>,
}

/// `output` event body (`OutputEvent`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapOutputEventBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables_reference: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// `stopped` event body (`StoppedEvent`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapStoppedEventBody {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_focus_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_threads_stopped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_breakpoint_ids: Option<Vec<i64>>,
}

/// `exited` event body (`ExitedEvent`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapExitedEventBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

/// `continued` event body (`ContinuedEvent`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapContinuedEventBody {
    #[serde(default)]
    pub thread_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_threads_continued: Option<bool>,
}

/// `runInTerminal` reverse-request arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapRunInTerminalArguments {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, Value>>,
}

/// `runInTerminal` reply body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapRunInTerminalResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_process_id: Option<u32>,
}

/// `startDebugging` reverse-request arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapStartDebuggingArguments {
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub configuration: Map<String, Value>,
}

// =============================================================================
// Session records
// =============================================================================

/// Where the debuggee is currently paused (omp `DapStopLocation`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapStopLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_pointer_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
}

impl DapStopLocation {
    /// `source:line[:column]` when the stop has a source path.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        self.source.as_ref()?.location(self.line, self.column)
    }
}

/// A source breakpoint the session holds (omp `DapBreakpointRecord`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapBreakpointRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub verified: bool,
    pub line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A function breakpoint the session holds (omp `DapFunctionBreakpointRecord`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapFunctionBreakpointRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub verified: bool,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An instruction breakpoint the session holds (omp `DapInstructionBreakpointRecord`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapInstructionBreakpointRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub verified: bool,
    pub instruction_reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A data breakpoint the session holds (omp `DapDataBreakpointRecord`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapDataBreakpointRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub verified: bool,
    pub data_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_type: Option<DapAccessType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Snapshot of one session, as every action reports it (omp `DapSessionSummary`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapSessionSummary {
    pub id: String,
    pub adapter: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub status: DapSessionStatus,
    pub launched_at: String,
    pub last_used_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_pointer_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DapSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    pub breakpoint_files: usize,
    pub breakpoint_count: usize,
    pub function_breakpoint_count: usize,
    pub output_bytes: u64,
    pub output_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub needs_configuration_done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session_ids: Option<Vec<String>>,
}

impl DapSessionSummary {
    /// `source:line[:column]`, when the stop has a source path.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        self.source.as_ref()?.location(self.line, self.column)
    }
}

/// How a `continue`/step settled (omp `DapContinueOutcome`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapContinueOutcome {
    pub snapshot: DapSessionSummary,
    pub state: DapRunState,
    pub timed_out: bool,
}

/// Captured `output` events for one session (omp `DapOutputSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapOutputSnapshot {
    pub snapshot: DapSessionSummary,
    pub output: String,
}

// =============================================================================
// Adapter config
// =============================================================================

/// One adapter as configured on disk (omp `DapAdapterConfig`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapAdapterConfig {
    pub command: String,
    pub args: Vec<String>,
    pub languages: Vec<String>,
    pub file_types: Vec<String>,
    pub root_markers: Vec<String>,
    pub launch_defaults: Map<String, Value>,
    pub attach_defaults: Map<String, Value>,
    pub connect_mode: Option<DapConnectMode>,
    pub accepts_directory_program: bool,
}

/// An adapter with its binary resolved (omp `DapResolvedAdapter`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DapResolvedAdapter {
    pub name: String,
    /// The command as configured (may be project-relative).
    pub command: String,
    pub args: Vec<String>,
    /// Absolute path (or PATH-resolved name) of the executable to spawn.
    pub resolved_command: String,
    pub languages: Vec<String>,
    pub file_types: Vec<String>,
    pub root_markers: Vec<String>,
    pub launch_defaults: Map<String, Value>,
    pub attach_defaults: Map<String, Value>,
    pub connect_mode: DapConnectMode,
    pub accepts_directory_program: bool,
}

impl DapResolvedAdapter {
    /// `attachDefaults.skipAttachRequest`: the adapter is already attached.
    #[must_use]
    pub fn skip_attach_request(&self) -> bool {
        self.attach_defaults
            .get("skipAttachRequest")
            .and_then(Value::as_bool)
            == Some(true)
    }
}

/// Result of automatic launch-adapter selection (omp `LaunchAdapterSelection`).
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchAdapterSelection {
    /// A runnable adapter.
    Adapter(DapResolvedAdapter),
    /// Configuration matched but its binary does not resolve.
    Unavailable {
        /// Adapter id that matched.
        adapter_name: String,
        /// The command that failed to resolve.
        command: String,
    },
    /// Nothing matched.
    None,
}

/// An attach target as the tool received it (omp `DapAttachTarget`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DapAttachTarget {
    /// Process to attach to.
    pub pid: Option<u32>,
    /// TCP port to attach to.
    pub port: Option<u16>,
    /// Host for `port` (defaults to `127.0.0.1`).
    pub host: Option<&'static str>,
}

/// Options for [`crate::session::DapSessionManager::launch`].
#[derive(Debug, Clone)]
pub struct DapLaunchSessionOptions {
    /// Resolved adapter to spawn.
    pub adapter: DapResolvedAdapter,
    /// Target path (executable, or a package directory for `dlv`).
    pub program: String,
    /// Program arguments.
    pub args: Option<Vec<String>>,
    /// Working directory of both the adapter and the debuggee.
    pub cwd: String,
    /// Per-launch overrides merged over `launch_defaults`.
    pub extra_launch_arguments: Map<String, Value>,
}

/// Options for [`crate::session::DapSessionManager::attach`].
#[derive(Debug, Clone)]
pub struct DapAttachSessionOptions {
    /// Resolved adapter to spawn.
    pub adapter: DapResolvedAdapter,
    /// Working directory.
    pub cwd: String,
    /// Process to attach to.
    pub pid: Option<u32>,
    /// TCP port to attach to.
    pub port: Option<u16>,
    /// Host for `port`.
    pub host: Option<String>,
}

/// Session id of a `setBreakpoints`-style mutation reply.
#[derive(Debug, Clone, PartialEq)]
pub struct DapBreakpointMutation {
    /// Session state after the mutation.
    pub snapshot: DapSessionSummary,
    /// The breakpoints the session now holds for the mutated target.
    pub breakpoints: Vec<DapBreakpointRecord>,
    /// Absolute source path that was mutated.
    pub source_path: String,
}

/// Reply of a function/instruction/data breakpoint mutation.
#[derive(Debug, Clone, PartialEq)]
pub struct DapTypedBreakpointMutation<T> {
    /// Session state after the mutation.
    pub snapshot: DapSessionSummary,
    /// The breakpoints the session now holds.
    pub breakpoints: Vec<T>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_tag_round_trips() {
        let request = DapMessage::Request {
            seq: 1,
            command: "initialize".to_string(),
            arguments: Some(json!({"clientID": "omp"})),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({"seq": 1, "type": "request", "command": "initialize",
                   "arguments": {"clientID": "omp"}})
        );
        let back: DapMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn response_uses_request_seq_spelling() {
        let value = json!({"seq": 9, "type": "response", "request_seq": 3,
                           "success": true, "command": "threads",
                           "body": {"threads": []}});
        let message: DapMessage = serde_json::from_value(value).unwrap();
        match message {
            DapMessage::Response {
                request_seq, success, ..
            } => {
                assert_eq!(request_seq, 3);
                assert!(success);
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_preserve_unknown_keys() {
        let caps: DapCapabilities = serde_json::from_value(json!({
            "supportsConfigurationDoneRequest": true,
            "supportsReadMemoryRequest": true,
            "supportsSomethingBrandNew": true
        }))
        .unwrap();
        assert!(caps.needs_configuration_done());
        assert!(caps.supports(DapCapability::ReadMemory));
        assert!(!caps.supports(DapCapability::WriteMemory));
        let back = serde_json::to_value(&caps).unwrap();
        assert_eq!(back["supportsSomethingBrandNew"], json!(true));
    }

    #[test]
    fn variable_type_uses_protocol_spelling() {
        let variable: DapVariable = serde_json::from_value(json!({
            "name": "x", "value": "1", "type": "int", "variablesReference": 0
        }))
        .unwrap();
        assert_eq!(variable.var_type.as_deref(), Some("int"));
        let back = serde_json::to_value(&variable).unwrap();
        assert_eq!(back["type"], json!("int"));
        assert!(back.get("var_type").is_none());
    }

    #[test]
    fn session_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(DapSessionStatus::Configuring).unwrap(),
            json!("configuring")
        );
        assert_eq!(DapSessionStatus::Stopped.as_str(), "stopped");
        assert_eq!(DapSessionStatus::Stopped.to_string(), "stopped");
    }

    #[test]
    fn access_type_and_context_parse_the_tool_spelling() {
        assert_eq!(DapAccessType::parse("readWrite"), Some(DapAccessType::ReadWrite));
        assert_eq!(DapAccessType::ReadWrite.as_str(), "readWrite");
        assert_eq!(
            serde_json::to_value(DapAccessType::ReadWrite).unwrap(),
            json!("readWrite")
        );
        assert_eq!(DapAccessType::parse("nope"), None);
        assert_eq!(
            DapEvaluateContext::parse("clipboard"),
            Some(DapEvaluateContext::Clipboard)
        );
        assert_eq!(DapEvaluateContext::Repl.as_str(), "repl");
    }

    #[test]
    fn source_location_appends_line_and_column() {
        let source = DapSource {
            path: Some("/tmp/main.c".to_string()),
            ..DapSource::default()
        };
        assert_eq!(
            source.location(Some(42), Some(3)).as_deref(),
            Some("/tmp/main.c:42:3")
        );
        assert_eq!(source.location(Some(42), None).as_deref(), Some("/tmp/main.c:42"));
        assert_eq!(source.location(None, None).as_deref(), Some("/tmp/main.c"));
        assert_eq!(DapSource::default().location(Some(1), None), None);
    }

    #[test]
    fn stop_location_falls_back_through_description_and_text() {
        let stop = DapStopLocation {
            source: Some(DapSource {
                name: Some("main.py".to_string()),
                ..DapSource::default()
            }),
            line: Some(7),
            ..DapStopLocation::default()
        };
        assert_eq!(stop.location().as_deref(), Some("main.py:7"));
    }

    #[test]
    fn skip_attach_request_reads_attach_defaults() {
        let mut adapter = DapResolvedAdapter::default();
        assert!(!adapter.skip_attach_request());
        adapter
            .attach_defaults
            .insert("skipAttachRequest".to_string(), json!(true));
        assert!(adapter.skip_attach_request());
    }
}
