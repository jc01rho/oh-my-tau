//! Transcript retention formatting, durable cursors, and idempotent suffix
//! commits.
//!
//! Retention is driven by a durable per-session user-turn cursor stored in the
//! bank, so a restarted process never re-retains a suffix it already committed.

use serde::{Deserialize, Serialize};

use crate::{
    extract::MAX_EXTRACTION_INPUT_BYTES,
    store::{BankStore, RetainedWindow},
    Result,
};

/// Journal-settled message role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetentionRole {
    /// User-authored input.
    User,
    /// Assistant response.
    Assistant,
    /// Durable tool outcome.
    Tool,
    /// System-authored settled context.
    System,
}

impl RetentionRole {
    /// Returns the persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }
}

impl std::fmt::Display for RetentionRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Owned settled message used by bounded shutdown retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedRetentionMessage {
    /// Stable journal item id.
    pub stable_id: String,
    /// Message role.
    pub role: RetentionRole,
    /// Settled textual content.
    pub content: String,
}

/// One journal-durable message eligible for retention.
#[derive(Clone, Copy, Debug)]
pub struct RetentionMessage<'a> {
    /// Stable journal item id used for idempotency metadata.
    pub stable_id: &'a str,
    /// Message role.
    pub role: RetentionRole,
    /// Settled textual content.
    pub content: &'a str,
}

/// Result of one retention decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionOutcome {
    /// Whether a new durable episode was stored.
    pub stored_id: Option<String>,
    /// Highest covered user turn after the operation.
    pub retained_through: u64,
    /// Whether a new durable extraction job was enqueued atomically.
    pub extraction_enqueued: bool,
    /// Marker-free text supplied to embeddings, when substantive.
    pub embedding_text: Option<String>,
}

/// Per-session retention coordinator backed by a durable cursor in the bank.
pub struct Retainer<'a> {
    store: &'a BankStore,
    session_id: &'a str,
    canonical_root: &'a str,
    retain_every_n_turns: usize,
    extraction_enabled: bool,
}

impl<'a> Retainer<'a> {
    /// Creates a coordinator. The turn interval is clamped to at least one.
    pub fn new(
        store: &'a BankStore,
        session_id: &'a str,
        canonical_root: &'a str,
        retain_every_n_turns: usize,
        extraction_enabled: bool,
    ) -> Self {
        Self {
            store,
            session_id,
            canonical_root,
            retain_every_n_turns: retain_every_n_turns.max(1),
            extraction_enabled,
        }
    }

    /// Retains only when the configured user-turn interval has elapsed.
    pub fn retain_periodic(&self, messages: &[RetentionMessage<'_>]) -> Result<RetentionOutcome> {
        self.retain(messages, false)
    }

    /// Force-retains the unprocessed suffix, including a short final session
    /// window.
    pub fn retain_force(&self, messages: &[RetentionMessage<'_>]) -> Result<RetentionOutcome> {
        self.retain(messages, true)
    }

    #[tracing::instrument(
        level = "debug",
        name = "memory_retention",
        skip_all,
        fields(force = force, message_count = messages.len())
    )]
    fn retain(&self, messages: &[RetentionMessage<'_>], force: bool) -> Result<RetentionOutcome> {
        let cursor = self.store.retention_cursor(self.session_id)?;
        let user_turns = messages
            .iter()
            .filter(|message| message.role == RetentionRole::User)
            .count() as u64;
        let due = user_turns > cursor
            && (force || user_turns - cursor >= self.retain_every_n_turns as u64);
        tracing::debug!(
            force,
            user_turns,
            retained_through = cursor,
            interval = self.retain_every_n_turns,
            due,
            "memory retention evaluated"
        );
        if !due {
            return Ok(RetentionOutcome {
                retained_through: cursor,
                ..RetentionOutcome::default()
            });
        }
        let suffix = slice_unretained(messages, cursor);
        let Some(transcript) = format_durable_transcript(suffix) else {
            return Ok(RetentionOutcome {
                retained_through: cursor,
                ..RetentionOutcome::default()
            });
        };
        let extraction_text = if self.extraction_enabled {
            format_extraction_text(suffix)
        } else {
            None
        };
        let embedding_text = format_embedding_text(suffix);
        let ids = suffix
            .iter()
            .map(|message| message.stable_id)
            .collect::<Vec<_>>();
        let metadata = serde_json::json!({
            "session_id": self.session_id,
            "source_ids": ids,
            "message_count": suffix.len(),
            "retained_through_user_turn": user_turns,
            "primary_root": self.canonical_root,
        });
        let stored_id = self.store.retain_window(RetainedWindow {
            session_id: self.session_id,
            transcript: transcript.as_str(),
            embed_text: embedding_text.as_deref().unwrap_or(transcript.as_str()),
            extraction_text: extraction_text.as_deref(),
            metadata: &metadata,
            retained_through_user_turn: user_turns,
        })?;
        let extraction_enqueued = stored_id.is_some() && extraction_text.is_some();
        tracing::debug!(
            stored = stored_id.is_some(),
            retained_through = user_turns,
            extraction_enqueued,
            "memory retention completed"
        );
        Ok(RetentionOutcome {
            stored_id,
            retained_through: user_turns,
            extraction_enqueued,
            embedding_text,
        })
    }
}

/// Frames all substantive messages with explicit role/end markers.
pub fn format_durable_transcript(messages: &[RetentionMessage<'_>]) -> Option<String> {
    format_messages(messages.iter().copied(), true)
}

/// Frames only user-authored messages for fact/entity extraction.
pub fn format_extraction_text(messages: &[RetentionMessage<'_>]) -> Option<String> {
    let text = format_messages(
        messages
            .iter()
            .copied()
            .filter(|message| message.role == RetentionRole::User),
        true,
    )?;
    Some(bound_extraction_text(text))
}

fn bound_extraction_text(text: String) -> String {
    const CLOSING_MARKER: &str = "\n[user:end]";
    if text.len() <= MAX_EXTRACTION_INPUT_BYTES {
        return text;
    }
    let mut boundary = MAX_EXTRACTION_INPUT_BYTES - CLOSING_MARKER.len();
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let prefix = text[..boundary].trim_end();
    let mut bounded = String::with_capacity(prefix.len() + CLOSING_MARKER.len());
    bounded.push_str(prefix);
    bounded.push_str(CLOSING_MARKER);
    bounded
}

/// Formats every substantive message without protocol markers for embedding and
/// FTS.
pub fn format_embedding_text(messages: &[RetentionMessage<'_>]) -> Option<String> {
    format_messages(messages.iter().copied(), false)
}

/// Removes retention protocol markers from recalled episode content.
pub fn strip_protocol_markers(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        let marker = (trimmed.starts_with("[role: ") && trimmed.ends_with(']'))
            || trimmed.starts_with("[user:end]")
            || trimmed.starts_with("[assistant:end]")
            || trimmed.starts_with("[tool:end]")
            || trimmed.starts_with("[system:end]");
        if marker {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
    }
    output.trim().to_owned()
}

fn slice_unretained<'a>(
    messages: &'a [RetentionMessage<'a>],
    cursor: u64,
) -> &'a [RetentionMessage<'a>] {
    if cursor == 0 {
        return messages;
    }
    let mut users = 0u64;
    for (index, message) in messages.iter().enumerate() {
        if message.role != RetentionRole::User {
            continue;
        }
        users += 1;
        if users > cursor {
            return &messages[index..];
        }
    }
    &[]
}

fn format_messages<'a>(
    messages: impl Iterator<Item = RetentionMessage<'a>>,
    markers: bool,
) -> Option<String> {
    let mut output = String::new();
    for message in messages {
        let stripped = strip_memory_blocks(message.content);
        let content = stripped.trim();
        if !substantive(content) {
            continue;
        }
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        if markers {
            let role = message.role.as_str();
            output.push_str("[role: ");
            output.push_str(role);
            output.push_str("]\n");
            output.push_str(content);
            output.push('\n');
            output.push('[');
            output.push_str(role);
            output.push_str(":end]");
        } else {
            output.push_str(content);
        }
    }
    if output.trim().len() < 10 {
        None
    } else {
        Some(output)
    }
}

fn strip_memory_blocks(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut inside = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<memories>") {
            inside = true;
            continue;
        }
        if trimmed.ends_with("</memories>") {
            inside = false;
            continue;
        }
        if inside {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
    }
    output
}

fn substantive(content: &str) -> bool {
    content.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bank::BankId, test_support::temp_root};

    fn store() -> BankStore {
        let root = temp_root("retain");
        BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store")
    }

    fn messages() -> Vec<OwnedRetentionMessage> {
        vec![
            OwnedRetentionMessage {
                stable_id: "m1".to_owned(),
                role: RetentionRole::User,
                content: "please remember the WAL pragma".to_owned(),
            },
            OwnedRetentionMessage {
                stable_id: "m2".to_owned(),
                role: RetentionRole::Assistant,
                content: "noted the WAL pragma".to_owned(),
            },
            OwnedRetentionMessage {
                stable_id: "m3".to_owned(),
                role: RetentionRole::User,
                content: "and the busy timeout".to_owned(),
            },
        ]
    }

    fn borrow(owned: &[OwnedRetentionMessage]) -> Vec<RetentionMessage<'_>> {
        owned
            .iter()
            .map(|message| RetentionMessage {
                stable_id: message.stable_id.as_str(),
                role: message.role,
                content: message.content.as_str(),
            })
            .collect()
    }

    #[test]
    fn durable_transcript_frames_every_role_and_strips_memory_blocks() {
        let owned = vec![OwnedRetentionMessage {
            stable_id: "m1".to_owned(),
            role: RetentionRole::User,
            content: "<memories>\nold recall\n</memories>\nreal content here".to_owned(),
        }];
        let transcript = format_durable_transcript(&borrow(&owned)).expect("transcript");
        assert!(transcript.starts_with("[role: user]\n"));
        assert!(transcript.ends_with("[user:end]"));
        assert!(transcript.contains("real content here"));
        assert!(!transcript.contains("<memories>"));
        assert!(!transcript.contains("old recall"));
    }

    #[test]
    fn extraction_text_keeps_only_user_turns() {
        let owned = messages();
        let extraction = format_extraction_text(&borrow(&owned)).expect("extraction");
        assert!(extraction.contains("please remember the WAL pragma"));
        assert!(extraction.contains("and the busy timeout"));
        assert!(!extraction.contains("noted the WAL pragma"));
    }

    #[test]
    fn extraction_text_is_utf8_safe_and_hard_bounded() {
        let content = "é".repeat(MAX_EXTRACTION_INPUT_BYTES);
        let owned = vec![OwnedRetentionMessage {
            stable_id: "m1".to_owned(),
            role: RetentionRole::User,
            content,
        }];
        let extraction = format_extraction_text(&borrow(&owned)).expect("substantive extraction");
        assert!(extraction.len() <= MAX_EXTRACTION_INPUT_BYTES);
        assert!(extraction.ends_with("[user:end]"));
        assert!(extraction.is_char_boundary(extraction.len()));
    }

    #[test]
    fn embedding_text_has_no_protocol_markers() {
        let owned = messages();
        let embedding = format_embedding_text(&borrow(&owned)).expect("embedding");
        assert!(!embedding.contains("[role: "));
        assert!(!embedding.contains(":end]"));
        assert!(embedding.contains("noted the WAL pragma"));
    }

    #[test]
    fn trivial_content_is_not_retained() {
        let owned = vec![OwnedRetentionMessage {
            stable_id: "m1".to_owned(),
            role: RetentionRole::User,
            content: "  ...  ".to_owned(),
        }];
        assert!(format_durable_transcript(&borrow(&owned)).is_none());
    }

    #[test]
    fn strip_protocol_markers_returns_plain_text() {
        let stripped = strip_protocol_markers(
            "[role: user]\nremember this\n[user:end]\n\n[role: assistant]\ndone\n[assistant:end]",
        );
        assert_eq!(stripped, "remember this\n\ndone");
    }

    #[test]
    fn periodic_retention_waits_for_the_configured_interval() {
        let store = store();
        let owned = messages();
        let retainer = Retainer::new(&store, "session", "/root", 3, true);
        let skipped = retainer.retain_periodic(&borrow(&owned)).expect("retain");
        assert!(
            skipped.stored_id.is_none(),
            "two user turns are below the interval of three"
        );
        assert_eq!(skipped.retained_through, 0);
        assert_eq!(store.counts().expect("counts").working, 0);

        let forced = retainer.retain_force(&borrow(&owned)).expect("retain");
        assert!(forced.stored_id.is_some());
        assert_eq!(forced.retained_through, 2);
        assert!(forced.extraction_enqueued);
        assert_eq!(store.counts().expect("counts").working, 1);
        assert_eq!(store.pending_extraction_count().expect("pending"), 1);
    }

    #[test]
    fn periodic_retention_stores_once_the_interval_is_reached() {
        let store = store();
        let owned = messages();
        let retainer = Retainer::new(&store, "session", "/root", 1, true);
        let first = retainer.retain_periodic(&borrow(&owned)).expect("retain");
        assert!(first.stored_id.is_some());
        let second = retainer
            .retain_periodic(&borrow(&owned))
            .expect("retain again");
        assert!(
            second.stored_id.is_none(),
            "the durable cursor prevents a second write"
        );
        assert_eq!(store.counts().expect("counts").working, 1);
    }

    #[test]
    fn only_the_unretained_suffix_is_stored_on_the_next_window() {
        let store = store();
        let retainer = Retainer::new(&store, "session", "/root", 1, false);
        let first = messages();
        retainer.retain_force(&borrow(&first)).expect("retain");
        assert_eq!(store.counts().expect("counts").working, 1);

        let mut second = first;
        second.push(OwnedRetentionMessage {
            stable_id: "m4".to_owned(),
            role: RetentionRole::User,
            content: "one more durable preference".to_owned(),
        });
        let outcome = retainer
            .retain_force(&borrow(&second))
            .expect("retain suffix");
        assert!(outcome.stored_id.is_some());
        assert_eq!(outcome.retained_through, 3);
        assert_eq!(store.counts().expect("counts").working, 2);
        let newest = store
            .list(1)
            .expect("list")
            .into_iter()
            .next()
            .expect("row");
        assert!(
            !newest.content.contains("WAL pragma"),
            "the retained prefix is not re-stored"
        );
        assert!(newest.content.contains("one more durable preference"));
    }

    #[test]
    fn disabled_extraction_retains_without_enqueuing() {
        let store = store();
        let retainer = Retainer::new(&store, "session", "/root", 1, false);
        let outcome = retainer.retain_force(&borrow(&messages())).expect("retain");
        assert!(outcome.stored_id.is_some());
        assert!(!outcome.extraction_enqueued);
        assert_eq!(store.pending_extraction_count().expect("pending"), 0);
    }
}
