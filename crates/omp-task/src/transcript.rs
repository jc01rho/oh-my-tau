//! Background-task transcripts: a child's session file (omp JSONL v3)
//! rendered as plain lines for `task_output` (`mode: "tail" | "full"`).
//!
//! Only `message` entries are rendered: user text, assistant text and tool
//! calls, tool results. Every block is capped so one huge tool result cannot
//! flood the view, and `full` keeps the head and the tail of a long session
//! around one elision marker.

use serde_json::Value;

/// `tail_lines` when the caller gives none.
pub const DEFAULT_TAIL_LINES: usize = 60;
/// `full` keeps at most this many lines.
pub const FULL_MAX_LINES: usize = 2_000;
/// Lines `full` keeps from the start of an over-long transcript.
const FULL_HEAD_LINES: usize = 200;
/// Lines one message block may take (the rest is summarized).
const BLOCK_MAX_LINES: usize = 40;
/// Characters one message block may take.
const BLOCK_MAX_CHARS: usize = 4_000;
/// Characters of a tool call's arguments shown inline.
const ARGS_MAX_CHARS: usize = 300;

/// Which part of the transcript to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptMode {
    /// The last `n` lines (at least one).
    Tail(usize),
    /// Everything, capped at [`FULL_MAX_LINES`] with a head/tail elision marker.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// Lines were left out (tail window or the `full` cap).
    pub truncated: bool,
    /// Lines in the whole rendered transcript.
    pub total_lines: usize,
}

#[must_use]
pub fn render_transcript(jsonl: &str, mode: TranscriptMode) -> Transcript {
    let lines = transcript_lines(jsonl);
    let total = lines.len();
    match mode {
        TranscriptMode::Tail(n) => {
            let start = total.saturating_sub(n.max(1));
            Transcript {
                text: lines[start..].join("\n"),
                truncated: start > 0,
                total_lines: total,
            }
        }
        TranscriptMode::Full if total <= FULL_MAX_LINES => Transcript {
            text: lines.join("\n"),
            truncated: false,
            total_lines: total,
        },
        TranscriptMode::Full => {
            let tail = FULL_MAX_LINES - FULL_HEAD_LINES;
            let mut kept: Vec<String> = lines[..FULL_HEAD_LINES].to_vec();
            kept.push(format!("… {} lines elided …", total - FULL_MAX_LINES));
            kept.extend_from_slice(&lines[total - tail..]);
            Transcript {
                text: kept.join("\n"),
                truncated: true,
                total_lines: total,
            }
        }
    }
}

/// Every rendered line of a session file, oldest first.
#[must_use]
pub fn transcript_lines(jsonl: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if let Some(message) = entry.get("message") {
            render_message(message, &mut lines);
        }
    }
    lines
}

fn render_message(message: &Value, out: &mut Vec<String>) {
    match message.get("role").and_then(Value::as_str) {
        Some("user") => push_block(out, "user: ", &content_text(message)),
        Some("assistant") => {
            for block in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => push_block(
                        out,
                        "assistant: ",
                        block.get("text").and_then(Value::as_str).unwrap_or(""),
                    ),
                    Some("toolCall") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                        let args = block
                            .get("arguments")
                            .map(|a| clip(&a.to_string(), ARGS_MAX_CHARS))
                            .unwrap_or_default();
                        out.push(format!("tool call: {name} {args}").trim_end().to_string());
                    }
                    _ => {}
                }
            }
            if let Some(error) = message.get("errorMessage").and_then(Value::as_str) {
                push_block(out, "assistant error: ", error);
            }
        }
        Some("toolResult") => {
            let name = message
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let label = if message.get("isError") == Some(&Value::Bool(true)) {
                format!("tool error ({name}): ")
            } else {
                format!("tool result ({name}): ")
            };
            push_block(out, &label, &content_text(message));
        }
        _ => {}
    }
}

fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Push one capped block: the first line carries `prefix`, the rest are
/// indented.
fn push_block(out: &mut Vec<String>, prefix: &str, text: &str) {
    let text = clip(text.trim(), BLOCK_MAX_CHARS);
    if text.is_empty() {
        return;
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = if lines.len() > BLOCK_MAX_LINES {
        BLOCK_MAX_LINES - 1
    } else {
        lines.len()
    };
    for (i, line) in lines[..shown].iter().enumerate() {
        if i == 0 {
            out.push(format!("{prefix}{line}"));
        } else {
            out.push(format!("  {line}"));
        }
    }
    if shown < lines.len() {
        out.push(format!("  … ({} more lines)", lines.len() - shown));
    }
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(message: &Value) -> String {
        json!({"type": "message", "id": "e", "message": message}).to_string()
    }

    fn session() -> String {
        [
            json!({"type": "session", "version": 3, "id": "s"}).to_string(),
            entry(&json!({"role": "user", "content": [{"type": "text", "text": "find the bug"}]})),
            entry(&json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "text", "text": "looking\nat it"},
                {"type": "toolCall", "id": "c1", "name": "grep", "arguments": {"pattern": "fn main"}}
            ]})),
            entry(&json!({"role": "toolResult", "toolName": "grep", "isError": true, "content": [{"type": "text", "text": "no match"}]})),
            "not json".to_string(),
            entry(&json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]})),
        ]
        .join("\n")
    }

    #[test]
    fn renders_roles_tool_calls_and_results() {
        assert_eq!(
            transcript_lines(&session()),
            [
                "user: find the bug",
                "assistant: looking",
                "  at it",
                r#"tool call: grep {"pattern":"fn main"}"#,
                "tool error (grep): no match",
                "assistant: done",
            ]
        );
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let tail = render_transcript(&session(), TranscriptMode::Tail(2));
        assert_eq!(tail.text, "tool error (grep): no match\nassistant: done");
        assert!(tail.truncated);
        assert_eq!(tail.total_lines, 6);
        let all = render_transcript(&session(), TranscriptMode::Tail(100));
        assert!(!all.truncated);
        assert_eq!(
            render_transcript(&session(), TranscriptMode::Tail(0)).text,
            "assistant: done"
        );
    }

    #[test]
    fn full_elides_the_middle_of_a_long_transcript() {
        let long: String = (0..FULL_MAX_LINES + 10)
            .map(|i| entry(&json!({"role": "user", "content": format!("line {i}")})))
            .collect::<Vec<_>>()
            .join("\n");
        let full = render_transcript(&long, TranscriptMode::Full);
        assert!(full.truncated);
        let lines: Vec<&str> = full.text.lines().collect();
        assert_eq!(lines.len(), FULL_MAX_LINES + 1);
        assert_eq!(lines[0], "user: line 0");
        assert_eq!(lines[FULL_HEAD_LINES], "… 10 lines elided …");
        assert_eq!(
            *lines.last().unwrap(),
            format!("user: line {}", FULL_MAX_LINES + 9)
        );
    }

    #[test]
    fn blocks_are_capped() {
        let big = (0..100)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = transcript_lines(&entry(&json!({"role": "user", "content": big})));
        assert_eq!(lines.len(), BLOCK_MAX_LINES);
        assert_eq!(lines.last().unwrap(), "  … (61 more lines)");
        assert_eq!(clip("한국어입니다", 3), "한국어…");
    }
}
