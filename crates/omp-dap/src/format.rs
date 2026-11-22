//! Text renderers for DAP replies.
//!
//! Every `format_*` function turns one protocol payload into the plain text the
//! `debug` tool returns to the model. The output is deliberately line oriented
//! and stable: a header line with a count, then one indented line per item.
//! Absent optional fields are omitted instead of printed as `null`, so a reply
//! with no source never grows a `location:` line.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;

use crate::types::{
    DapBreakpointRecord, DapContinueOutcome, DapDisassembledInstruction, DapEvaluateResponse,
    DapModule, DapReadMemoryResponse, DapScope, DapSessionSummary, DapSource, DapStackFrame,
    DapThread, DapVariable,
};

/// Bytes rendered per hex-dump row.
const HEX_ROW_BYTES: usize = 16;

/// `""` for one, `"s"` otherwise (keeps count lines grammatical).
fn plural(count: u64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

// =============================================================================
// Session snapshot
// =============================================================================

/// Render one session summary: identity, status, stop location, bookkeeping.
///
/// This is the block every action ends with, so it stays compact and never
/// repeats the `output:` payload (that lives in [`build_outcome_text`]).
#[must_use]
pub fn format_session_snapshot(summary: &DapSessionSummary) -> String {
    let mut lines = vec![
        format!("session: {}", summary.id),
        format!("adapter: {}", summary.adapter),
        format!("status: {}", summary.status.as_str()),
    ];

    if let Some(program) = summary.program.as_deref() {
        lines.push(format!("program: {program}"));
    }
    lines.push(format!("cwd: {}", summary.cwd));

    if let Some(location) = summary.location() {
        lines.push(format!("location: {location}"));
    }
    if let Some(name) = summary.frame_name.as_deref() {
        let mut line = format!("frame: {name}");
        match (summary.frame_id, summary.thread_id) {
            (Some(frame), Some(thread)) => {
                line.push_str(&format!(" (frame {frame}, thread {thread})"));
            }
            (Some(frame), None) => line.push_str(&format!(" (frame {frame})")),
            (None, Some(thread)) => line.push_str(&format!(" (thread {thread})")),
            (None, None) => {}
        }
        lines.push(line);
    } else if let Some(thread) = summary.thread_id {
        lines.push(format!("thread: {thread}"));
    }
    if let Some(reference) = summary.instruction_pointer_reference.as_deref() {
        lines.push(format!("instruction pointer: {reference}"));
    }

    if let Some(reason) = summary.stop_reason.as_deref() {
        match summary.stop_description.as_deref() {
            Some(description) => lines.push(format!("stop: {reason} - {description}")),
            None => lines.push(format!("stop: {reason}")),
        }
    }

    let breakpoints = summary.breakpoint_count as u64;
    let files = summary.breakpoint_files as u64;
    let functions = summary.function_breakpoint_count as u64;
    lines.push(format!(
        "breakpoints: {breakpoints} in {files} file{}, {functions} function{}",
        plural(files),
        plural(functions)
    ));

    let mut output = format!(
        "output: {} byte{}",
        summary.output_bytes,
        plural(summary.output_bytes)
    );
    if summary.output_truncated {
        output.push_str(" (truncated)");
    }
    lines.push(output);

    if let Some(exit_code) = summary.exit_code {
        lines.push(format!("exit: {exit_code}"));
    }
    if summary.needs_configuration_done {
        lines.push("configuration: awaiting configurationDone".to_string());
    }
    if let Some(parent) = summary.parent_session_id.as_deref() {
        lines.push(format!("parent: {parent}"));
    }
    if let Some(children) = summary.child_session_ids.as_deref() {
        if !children.is_empty() {
            lines.push(format!("children: {}", children.join(", ")));
        }
    }

    lines.join("\n")
}

// =============================================================================
// Breakpoints
// =============================================================================

/// Render the breakpoints a session holds for one source file.
#[must_use]
pub fn format_breakpoints(source_path: &str, breakpoints: &[DapBreakpointRecord]) -> String {
    let mut lines = vec![format!(
        "breakpoints: {} in {source_path}",
        breakpoints.len()
    )];
    if breakpoints.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for breakpoint in breakpoints {
        let mut line = format!("  line {}", breakpoint.line);
        if let Some(id) = breakpoint.id {
            line.push_str(&format!(" (id {id})"));
        }
        line.push_str(if breakpoint.verified {
            " verified"
        } else {
            " pending"
        });
        if let Some(condition) = breakpoint.condition.as_deref() {
            line.push_str(&format!("  condition: {condition}"));
        }
        if let Some(message) = breakpoint.message.as_deref() {
            line.push_str(&format!("  {message}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

// =============================================================================
// Stack frames, threads, scopes, variables
// =============================================================================

/// Render a stack trace, innermost frame first (the adapter's order).
#[must_use]
pub fn format_stack_frames(frames: &[DapStackFrame]) -> String {
    let mut lines = vec![format!("stack frames: {}", frames.len())];
    if frames.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for (index, frame) in frames.iter().enumerate() {
        let location = frame
            .source
            .as_ref()
            .and_then(|source| source.location(Some(frame.line), Some(frame.column)))
            .unwrap_or_else(|| format!("line {}", frame.line));
        lines.push(format!(
            "  #{index} {} at {location} (frame {})",
            frame.name, frame.id
        ));
    }
    lines.join("\n")
}

/// Render the thread list, marking `current` when the caller knows it.
#[must_use]
pub fn format_threads(threads: &[DapThread], current: Option<i64>) -> String {
    let mut lines = vec![format!("threads: {}", threads.len())];
    if threads.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for thread in threads {
        let marker = if current == Some(thread.id) { '*' } else { ' ' };
        lines.push(format!("  {marker} {} {}", thread.id, thread.name));
    }
    lines.join("\n")
}

/// Render the scopes of one frame.
#[must_use]
pub fn format_scopes(scopes: &[DapScope]) -> String {
    let mut lines = vec![format!("scopes: {}", scopes.len())];
    if scopes.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for scope in scopes {
        let mut line = format!("  {} (ref {})", scope.name, scope.variables_reference);
        if let Some(hint) = scope.presentation_hint.as_deref() {
            line.push_str(&format!(" [{hint}]"));
        }
        if scope.expensive {
            line.push_str(" expensive");
        }
        if let Some(location) = scope
            .source
            .as_ref()
            .and_then(|source| source.location(scope.line, scope.column))
        {
            line.push_str(&format!(" at {location}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Render one `variables` page.
///
/// `[ref N, ...]` marks a variable the caller can expand with another
/// `variables` request on that reference.
#[must_use]
pub fn format_variables(variables: &[DapVariable]) -> String {
    let mut lines = vec![format!("variables: {}", variables.len())];
    if variables.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for variable in variables {
        let mut line = format!("  {} = {}", variable.name, variable.value);
        if let Some(var_type) = variable.var_type.as_deref() {
            line.push_str(&format!(" ({var_type})"));
        }
        if variable.variables_reference != 0 {
            line.push_str(&format!(" [ref {}", variable.variables_reference));
            if let Some(named) = variable.named_variables {
                line.push_str(&format!(", {named} named"));
            }
            if let Some(indexed) = variable.indexed_variables {
                line.push_str(&format!(", {indexed} indexed"));
            }
            line.push(']');
        }
        if let Some(memory) = variable.memory_reference.as_deref() {
            line.push_str(&format!(" @ {memory}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

// =============================================================================
// Disassembly
// =============================================================================

/// Render disassembled instructions, one per line, address first.
#[must_use]
pub fn format_disassembly(instructions: &[DapDisassembledInstruction]) -> String {
    let mut lines = vec![format!("instructions: {}", instructions.len())];
    if instructions.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for instruction in instructions {
        let mut line = format!("  {}: {}", instruction.address, instruction.instruction);
        if let Some(bytes) = instruction.instruction_bytes.as_deref() {
            line.push_str(&format!("  ; {bytes}"));
        }
        if let Some(symbol) = instruction.symbol.as_deref() {
            line.push_str(&format!("  <{symbol}>"));
        }
        if let Some(location) = instruction
            .location
            .as_ref()
            .and_then(|source| source.location(instruction.line, instruction.column))
        {
            line.push_str(&format!("  {location}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

// =============================================================================
// Memory
// =============================================================================

/// Render a `readMemory` reply as a classic 16-byte hex + ASCII dump.
#[must_use]
pub fn format_memory_read(response: &DapReadMemoryResponse) -> String {
    let decoded = response.data.as_deref().and_then(decode_memory);
    let mut lines = Vec::new();
    match decoded {
        Some(bytes) => {
            lines.push(format!(
                "memory {}: {} byte{}",
                response.address,
                bytes.len(),
                plural(bytes.len() as u64)
            ));
            for (index, chunk) in bytes.chunks(HEX_ROW_BYTES).enumerate() {
                lines.push(hex_row(&response.address, index * HEX_ROW_BYTES, chunk));
            }
        }
        None => {
            lines.push(format!("memory {}: no data", response.address));
            if let Some(raw) = response.data.as_deref() {
                lines.push(format!("  {raw}"));
            }
        }
    }
    if let Some(unreadable) = response.unreadable_bytes {
        lines.push(format!(
            "unreadable: {unreadable} byte{}",
            plural(unreadable.max(0) as u64)
        ));
    }
    lines.join("\n")
}

/// Base64 payload to bytes; `None` when the adapter sent something else.
fn decode_memory(data: &str) -> Option<Vec<u8>> {
    STANDARD.decode(data.trim()).ok()
}

/// `0x00001000  48 65 ...  |Hello...|` for one row of at most 16 bytes.
fn hex_row(address: &str, offset: usize, bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in bytes {
        hex.push_str(&format!("{byte:02x} "));
    }
    let ascii: String = bytes
        .iter()
        .map(|byte| {
            if (0x20..0x7f).contains(byte) {
                char::from(*byte)
            } else {
                '.'
            }
        })
        .collect();
    format!(
        "  {}  {:<width$} |{ascii}|",
        row_address(address, offset),
        hex,
        width = HEX_ROW_BYTES * 3
    )
}

/// Absolute address of a row, or `base+0x10` when the adapter's spelling is
/// not parseable as hex.
fn row_address(address: &str, offset: usize) -> String {
    match parse_address(address) {
        Some(base) => format!("0x{:08x}", base.saturating_add(offset as u64)),
        None if offset == 0 => address.to_string(),
        None => format!("{address}+{offset:#x}"),
    }
}

/// Parse `0x1000`, `0X1000` or bare `1000` as a hex address.
fn parse_address(address: &str) -> Option<u64> {
    let trimmed = address.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u64::from_str_radix(digits, 16).ok()
}

// =============================================================================
// Modules and sources
// =============================================================================

/// Render the loaded modules as an aligned table.
#[must_use]
pub fn format_modules(modules: &[DapModule]) -> String {
    let mut lines = vec![format!("modules: {}", modules.len())];
    if modules.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    let name_width = modules
        .iter()
        .map(|module| module.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("name".len());
    lines.push(format!(
        "  {:<name_width$}  {:<16}  {:<12}  path",
        "name", "address range", "symbols"
    ));
    for module in modules {
        let mut symbols = module
            .symbol_status
            .clone()
            .unwrap_or_else(|| "-".to_string());
        if module.is_optimized == Some(true) {
            symbols.push_str(", optimized");
        }
        if module.is_user_code == Some(true) {
            symbols.push_str(", user");
        }
        lines.push(format!(
            "  {:<name_width$}  {:<16}  {:<12}  {}",
            module.name,
            module.address_range.as_deref().unwrap_or("-"),
            symbols,
            module.path.as_deref().unwrap_or("-")
        ));
    }
    lines.join("\n")
}

/// Render the `loadedSources` list.
#[must_use]
pub fn format_loaded_sources(sources: &[DapSource]) -> String {
    let mut lines = vec![format!("loaded sources: {}", sources.len())];
    if sources.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for source in sources {
        let label = source.label().unwrap_or("<unnamed>");
        match source.name.as_deref() {
            Some(name) if source.path.is_some() && source.path.as_deref() != Some(name) => {
                lines.push(format!("  {label}  ({name})"));
            }
            _ => lines.push(format!("  {label}")),
        }
    }
    lines.join("\n")
}

// =============================================================================
// Generic replies
// =============================================================================

/// Pretty-print the body of an arbitrary adapter request.
///
/// Custom requests have no schema, so JSON is the only honest rendering.
#[must_use]
pub fn format_custom_response(body: &Value) -> String {
    serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string())
}

/// Render every live session, marking `active`.
#[must_use]
pub fn format_sessions(sessions: &[DapSessionSummary], active: Option<&str>) -> String {
    let mut lines = vec![format!("sessions: {}", sessions.len())];
    if sessions.is_empty() {
        lines.push("  (none)".to_string());
        return lines.join("\n");
    }
    for session in sessions {
        let marker = if active == Some(session.id.as_str()) {
            '*'
        } else {
            ' '
        };
        let mut line = format!(
            "  {marker} {} {} {}",
            session.id,
            session.adapter,
            session.status.as_str()
        );
        if let Some(program) = session.program.as_deref() {
            line.push_str(&format!("  {program}"));
        }
        if let Some(location) = session.location() {
            line.push_str(&format!("  {location}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Render an `evaluate` reply on one line.
#[must_use]
pub fn format_evaluation(response: &DapEvaluateResponse) -> String {
    let mut text = response.result.clone();
    if let Some(result_type) = response.result_type.as_deref() {
        text.push_str(&format!(" ({result_type})"));
    }
    if response.variables_reference != 0 {
        text.push_str(&format!(" [ref {}", response.variables_reference));
        if let Some(named) = response.named_variables {
            text.push_str(&format!(", {named} named"));
        }
        if let Some(indexed) = response.indexed_variables {
            text.push_str(&format!(", {indexed} indexed"));
        }
        text.push(']');
    }
    if let Some(memory) = response.memory_reference.as_deref() {
        text.push_str(&format!(" @ {memory}"));
    }
    text
}

/// Render the result of `continue`/`step`/`launch`: how it settled, the
/// resulting snapshot, and whatever the program wrote while running.
#[must_use]
pub fn build_outcome_text(outcome: &DapContinueOutcome, output: Option<&str>) -> String {
    let mut lines = vec![format!("state: {}", outcome.state.as_str())];
    if outcome.timed_out {
        lines.push("timed out: yes".to_string());
    }
    lines.push(format_session_snapshot(&outcome.snapshot));
    if let Some(output) = output {
        let output = output.trim_end_matches('\n');
        if !output.is_empty() {
            lines.push("output:".to_string());
            lines.push(output.to_string());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DapRunState, DapSessionStatus};
    use serde_json::json;

    fn stopped_summary() -> DapSessionSummary {
        DapSessionSummary {
            id: "s1".to_string(),
            adapter: "dlv".to_string(),
            cwd: "/tmp".to_string(),
            program: Some("/tmp/app".to_string()),
            status: DapSessionStatus::Stopped,
            thread_id: Some(1),
            frame_id: Some(3),
            stop_reason: Some("breakpoint".to_string()),
            stop_description: Some("breakpoint 1".to_string()),
            frame_name: Some("main.main".to_string()),
            source: Some(DapSource {
                name: Some("main.go".to_string()),
                path: Some("/tmp/main.go".to_string()),
                ..DapSource::default()
            }),
            line: Some(42),
            column: Some(5),
            breakpoint_files: 1,
            breakpoint_count: 2,
            function_breakpoint_count: 0,
            output_bytes: 7,
            ..DapSessionSummary::default()
        }
    }

    #[test]
    fn session_snapshot_reports_the_stop() {
        let text = format_session_snapshot(&stopped_summary());
        assert!(text.contains("session: s1"));
        assert!(text.contains("adapter: dlv"));
        assert!(text.contains("status: stopped"));
        assert!(text.contains("program: /tmp/app"));
        assert!(text.contains("location: /tmp/main.go:42:5"));
        assert!(text.contains("frame: main.main (frame 3, thread 1)"));
        assert!(text.contains("stop: breakpoint - breakpoint 1"));
        assert!(text.contains("breakpoints: 2 in 1 file, 0 functions"));
        assert!(text.contains("output: 7 bytes"));
    }

    #[test]
    fn session_snapshot_omits_absent_fields() {
        let summary = DapSessionSummary {
            id: "s2".to_string(),
            adapter: "debugpy".to_string(),
            cwd: "/work".to_string(),
            status: DapSessionStatus::Running,
            ..DapSessionSummary::default()
        };
        let text = format_session_snapshot(&summary);
        assert!(!text.contains("program:"));
        assert!(!text.contains("location:"));
        assert!(!text.contains("stop:"));
        assert!(!text.contains("exit:"));
        assert!(!text.contains("parent:"));
        assert!(text.contains("output: 0 bytes"));
    }

    #[test]
    fn session_snapshot_reports_exit_and_configuration() {
        let summary = DapSessionSummary {
            id: "s3".to_string(),
            adapter: "gdb".to_string(),
            cwd: "/tmp".to_string(),
            status: DapSessionStatus::Configuring,
            output_bytes: 1,
            output_truncated: true,
            exit_code: Some(2),
            needs_configuration_done: true,
            parent_session_id: Some("root".to_string()),
            child_session_ids: Some(vec!["c1".to_string(), "c2".to_string()]),
            ..DapSessionSummary::default()
        };
        let text = format_session_snapshot(&summary);
        assert!(text.contains("output: 1 byte (truncated)"));
        assert!(text.contains("exit: 2"));
        assert!(text.contains("configuration: awaiting configurationDone"));
        assert!(text.contains("parent: root"));
        assert!(text.contains("children: c1, c2"));
    }

    #[test]
    fn breakpoints_mark_verification_and_conditions() {
        let records = vec![
            DapBreakpointRecord {
                id: Some(1),
                verified: true,
                line: 42,
                condition: Some("x > 3".to_string()),
                message: None,
            },
            DapBreakpointRecord {
                id: Some(2),
                verified: false,
                line: 51,
                condition: None,
                message: Some("no executable code".to_string()),
            },
        ];
        let text = format_breakpoints("/tmp/main.go", &records);
        assert!(text.starts_with("breakpoints: 2 in /tmp/main.go"));
        assert!(text.contains("  line 42 (id 1) verified  condition: x > 3"));
        assert!(text.contains("  line 51 (id 2) pending  no executable code"));
    }

    #[test]
    fn breakpoints_render_an_empty_list() {
        let text = format_breakpoints("/tmp/main.go", &[]);
        assert_eq!(text, "breakpoints: 0 in /tmp/main.go\n  (none)");
    }

    #[test]
    fn stack_frames_include_location_and_id() {
        let frames = vec![
            DapStackFrame {
                id: 7,
                name: "main.main".to_string(),
                source: Some(DapSource {
                    path: Some("/tmp/main.go".to_string()),
                    ..DapSource::default()
                }),
                line: 42,
                column: 5,
                ..DapStackFrame::default()
            },
            DapStackFrame {
                id: 8,
                name: "runtime.main".to_string(),
                line: 250,
                column: 1,
                ..DapStackFrame::default()
            },
        ];
        let text = format_stack_frames(&frames);
        assert!(text.starts_with("stack frames: 2"));
        assert!(text.contains("  #0 main.main at /tmp/main.go:42:5 (frame 7)"));
        assert!(text.contains("  #1 runtime.main at line 250 (frame 8)"));
    }

    #[test]
    fn threads_mark_the_current_one() {
        let threads = vec![
            DapThread {
                id: 1,
                name: "main".to_string(),
            },
            DapThread {
                id: 2,
                name: "worker".to_string(),
            },
        ];
        let text = format_threads(&threads, Some(2));
        assert!(text.contains("  1 main"));
        assert!(text.contains("  * 2 worker"));
        assert_eq!(format_threads(&[], None), "threads: 0\n  (none)");
    }

    #[test]
    fn scopes_include_reference_hint_and_flags() {
        let scopes = vec![
            DapScope {
                name: "Locals".to_string(),
                presentation_hint: Some("locals".to_string()),
                variables_reference: 1,
                ..DapScope::default()
            },
            DapScope {
                name: "Globals".to_string(),
                variables_reference: 2,
                expensive: true,
                ..DapScope::default()
            },
        ];
        let text = format_scopes(&scopes);
        assert!(text.starts_with("scopes: 2"));
        assert!(text.contains("  Locals (ref 1) [locals]"));
        assert!(text.contains("  Globals (ref 2) expensive"));
    }

    #[test]
    fn variables_include_type_and_expandable_reference() {
        let variables = vec![
            DapVariable {
                name: "x".to_string(),
                value: "1".to_string(),
                var_type: Some("int".to_string()),
                ..DapVariable::default()
            },
            DapVariable {
                name: "items".to_string(),
                value: "[]int len: 3".to_string(),
                var_type: Some("[]int".to_string()),
                variables_reference: 5,
                named_variables: Some(0),
                indexed_variables: Some(3),
                memory_reference: Some("0xc000010000".to_string()),
                ..DapVariable::default()
            },
        ];
        let text = format_variables(&variables);
        assert!(text.contains("  x = 1 (int)"));
        assert!(text.contains(
            "  items = []int len: 3 ([]int) [ref 5, 0 named, 3 indexed] @ 0xc000010000"
        ));
    }

    #[test]
    fn disassembly_includes_bytes_symbol_and_location() {
        let instructions = vec![DapDisassembledInstruction {
            address: "0x401000".to_string(),
            instruction_bytes: Some("4889e5".to_string()),
            instruction: "mov rbp, rsp".to_string(),
            symbol: Some("main".to_string()),
            location: Some(DapSource {
                path: Some("/tmp/main.c".to_string()),
                ..DapSource::default()
            }),
            line: Some(5),
            column: Some(1),
            ..DapDisassembledInstruction::default()
        }];
        let text = format_disassembly(&instructions);
        assert!(text.starts_with("instructions: 1"));
        assert!(text.contains("  0x401000: mov rbp, rsp  ; 4889e5  <main>  /tmp/main.c:5:1"));
    }

    #[test]
    fn memory_read_renders_a_hex_dump() {
        let response = DapReadMemoryResponse {
            address: "0x1000".to_string(),
            unreadable_bytes: Some(2),
            data: Some(STANDARD.encode(b"Hello, world!")),
        };
        let text = format_memory_read(&response);
        assert!(text.starts_with("memory 0x1000: 13 bytes"));
        assert!(text.contains("0x00001000  48 65 6c 6c 6f 2c 20 77 6f 72 6c 64 21"));
        assert!(text.contains("|Hello, world!|"));
        assert!(text.contains("unreadable: 2 bytes"));
    }

    #[test]
    fn memory_read_advances_the_row_address() {
        let bytes: Vec<u8> = (0u8..20).collect();
        let response = DapReadMemoryResponse {
            address: "0x2000".to_string(),
            unreadable_bytes: None,
            data: Some(STANDARD.encode(&bytes)),
        };
        let text = format_memory_read(&response);
        assert!(text.starts_with("memory 0x2000: 20 bytes"));
        assert!(text.contains("0x00002000  00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f"));
        assert!(text.contains("0x00002010  10 11 12 13"));
        assert!(!text.contains("unreadable:"));
    }

    #[test]
    fn memory_read_handles_missing_and_unparseable_payloads() {
        let empty = DapReadMemoryResponse {
            address: "0x10".to_string(),
            ..DapReadMemoryResponse::default()
        };
        assert_eq!(format_memory_read(&empty), "memory 0x10: no data");

        let raw = DapReadMemoryResponse {
            address: "0xZZ".to_string(),
            data: Some("not base64!".to_string()),
            ..DapReadMemoryResponse::default()
        };
        let text = format_memory_read(&raw);
        assert!(text.starts_with("memory 0xZZ: no data"));
        assert!(text.contains("  not base64!"));
    }

    #[test]
    fn modules_render_an_aligned_table() {
        let modules = vec![
            DapModule {
                id: json!(1),
                name: "libc.so.6".to_string(),
                path: Some("/usr/lib/libc.so.6".to_string()),
                address_range: Some("0x7f00-0x7f40".to_string()),
                symbol_status: Some("symbolsLoaded".to_string()),
                is_optimized: Some(true),
                is_user_code: Some(false),
                ..DapModule::default()
            },
            DapModule {
                id: json!("app"),
                name: "app".to_string(),
                ..DapModule::default()
            },
        ];
        let text = format_modules(&modules);
        assert!(text.starts_with("modules: 2"));
        assert!(text.contains("address range"));
        assert!(text.contains("libc.so.6"));
        assert!(text.contains("symbolsLoaded, optimized"));
        assert!(text.contains("/usr/lib/libc.so.6"));
        assert!(text.contains("  app"));
    }

    #[test]
    fn loaded_sources_label_path_with_name() {
        let sources = vec![
            DapSource {
                name: Some("main.go".to_string()),
                path: Some("/tmp/main.go".to_string()),
                ..DapSource::default()
            },
            DapSource {
                name: Some("generated.py".to_string()),
                ..DapSource::default()
            },
            DapSource::default(),
        ];
        let text = format_loaded_sources(&sources);
        assert!(text.starts_with("loaded sources: 3"));
        assert!(text.contains("  /tmp/main.go  (main.go)"));
        assert!(text.contains("  generated.py"));
        assert!(text.contains("  <unnamed>"));
    }

    #[test]
    fn custom_response_pretty_prints_json() {
        let body = json!({"a": 1, "b": [true, null]});
        let text = format_custom_response(&body);
        assert!(text.starts_with('{'));
        assert!(text.contains("\"a\": 1"));
        assert!(text.contains("null"));
        assert_eq!(text.lines().count(), 7);
    }

    #[test]
    fn sessions_mark_the_active_one() {
        let sessions = vec![stopped_summary()];
        let text = format_sessions(&sessions, Some("s1"));
        assert!(text.starts_with("sessions: 1"));
        assert!(text.contains("  * s1 dlv stopped  /tmp/app  /tmp/main.go:42:5"));

        let inactive = format_sessions(&sessions, None);
        assert!(inactive.contains("    s1 dlv stopped"));
        assert_eq!(format_sessions(&[], None), "sessions: 0\n  (none)");
    }

    #[test]
    fn evaluation_appends_type_and_reference() {
        let plain = DapEvaluateResponse {
            result: "42".to_string(),
            result_type: Some("int".to_string()),
            ..DapEvaluateResponse::default()
        };
        assert_eq!(format_evaluation(&plain), "42 (int)");

        let expandable = DapEvaluateResponse {
            result: "map[a:1]".to_string(),
            variables_reference: 9,
            named_variables: Some(1),
            memory_reference: Some("0x1".to_string()),
            ..DapEvaluateResponse::default()
        };
        assert_eq!(
            format_evaluation(&expandable),
            "map[a:1] [ref 9, 1 named] @ 0x1"
        );
    }

    #[test]
    fn outcome_text_includes_state_snapshot_and_output() {
        let outcome = DapContinueOutcome {
            snapshot: stopped_summary(),
            state: DapRunState::Stopped,
            timed_out: true,
        };
        let text = build_outcome_text(&outcome, Some("hello\n"));
        assert!(text.starts_with("state: stopped\ntimed out: yes\nsession: s1"));
        assert!(text.contains("location: /tmp/main.go:42:5"));
        assert!(text.ends_with("output:\nhello"));
    }

    #[test]
    fn outcome_text_omits_empty_output() {
        let outcome = DapContinueOutcome {
            snapshot: stopped_summary(),
            state: DapRunState::Running,
            timed_out: false,
        };
        let text = build_outcome_text(&outcome, Some("\n\n"));
        assert!(text.starts_with("state: running\nsession: s1"));
        assert!(!text.contains("timed out:"));
        // The snapshot's `output: 0 bytes` line stays; the payload block does not.
        assert!(!text.contains("output:\n"));
        assert_eq!(text, build_outcome_text(&outcome, None));
    }
}
