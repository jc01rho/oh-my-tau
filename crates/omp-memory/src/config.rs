//! Native Mnemopi configuration and defaults.
//!
//! Every value here mirrors one `mnemopi.*` setting id in the application
//! settings schema. The crate performs no environment lookups: the composing
//! application resolves settings and passes them in already-typed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Selected memory implementation. Memory is globally inactive by default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryBackend {
    /// Perform no memory effects and advertise no memory capabilities.
    #[default]
    Off,
    /// Use the first-party durable Mnemopi implementation.
    Mnemopi,
}

impl MemoryBackend {
    /// Returns the persisted settings spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Mnemopi => "mnemopi",
        }
    }

    /// Parses the persisted settings spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "mnemopi" => Some(Self::Mnemopi),
            _ => None,
        }
    }
}

/// Project-bank scoping policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BankScoping {
    /// Read and write one shared bank.
    Global,
    /// Read and write one canonical-project bank.
    #[default]
    PerProject,
    /// Write the canonical-project bank and recall it together with the shared
    /// bank.
    PerProjectTagged,
}

impl BankScoping {
    /// Returns the persisted settings spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::PerProject => "per-project",
            Self::PerProjectTagged => "per-project-tagged",
        }
    }

    /// Parses the persisted settings spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "global" => Some(Self::Global),
            "per-project" | "perproject" => Some(Self::PerProject),
            "per-project-tagged" | "perprojecttagged" => Some(Self::PerProjectTagged),
            _ => None,
        }
    }
}

/// Auxiliary model lane used by extraction and consolidation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryLlmMode {
    /// Store episodes without model-extracted facts.
    None,
    /// Resolve the configured tiny/smol online role.
    #[default]
    Smol,
    /// Use an app-owned OpenAI-compatible completion endpoint.
    Remote,
}

impl MemoryLlmMode {
    /// Returns the persisted settings spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Smol => "smol",
            Self::Remote => "remote",
        }
    }

    /// Parses the persisted settings spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "smol" => Some(Self::Smol),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }

    /// Whether this lane enqueues durable extraction jobs.
    pub const fn extraction_enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Mnemopi settings. [`MnemopiSettings::normalize`] applies floors and hard
/// safety ceilings so a hand-edited config can never drive the store into an
/// unbounded state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MnemopiSettings {
    /// Optional primary database path; otherwise the caller supplies its memory
    /// data root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<PathBuf>,
    /// Optional shared bank base name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    /// Bank scoping policy.
    #[serde(default)]
    pub scoping: BankScoping,
    /// Recall automatically on the first top-level turn.
    #[serde(default = "default_true")]
    pub auto_recall: bool,
    /// Retain settled top-level turns automatically.
    #[serde(default = "default_true")]
    pub auto_retain: bool,
    /// Enable four-voice reciprocal-rank recall.
    #[serde(default)]
    pub polyphonic_recall: bool,
    /// Enable exact/similar tiered recall caching.
    #[serde(default)]
    pub enhanced_recall: bool,
    /// Derive graph links as memories enter the durable store.
    #[serde(default)]
    pub proactive_linking: bool,
    /// Maximum transient working rows retained per session. Zero disables
    /// count/TTL eviction.
    #[serde(default = "default_working_memory_limit")]
    pub working_memory_limit: usize,
    /// Maximum transient working-row age in hours.
    #[serde(default = "default_working_memory_ttl_hours")]
    pub working_memory_ttl_hours: u64,
    /// User-turn interval for periodic retention.
    #[serde(default = "default_retain_turns")]
    pub retain_every_n_turns: usize,
    /// Maximum recalled rows.
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    /// User-bounded turns included in an automatic recall query.
    #[serde(default = "default_recall_context_turns")]
    pub recall_context_turns: usize,
    /// Maximum recall-query characters.
    #[serde(default = "default_recall_query_chars")]
    pub recall_max_query_chars: usize,
    /// Maximum approximate tokens injected into prompts or projections.
    #[serde(default = "default_injection_tokens")]
    pub injection_token_limit: usize,
    /// Extraction and consolidation model lane.
    #[serde(default)]
    pub llm_mode: MemoryLlmMode,
    /// Emit memory debug diagnostics.
    #[serde(default)]
    pub debug: bool,
    /// Bounded shutdown drain in milliseconds.
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
}

const fn default_true() -> bool {
    true
}

const fn default_retain_turns() -> usize {
    4
}

const fn default_working_memory_limit() -> usize {
    1000
}

const fn default_working_memory_ttl_hours() -> u64 {
    24
}

const fn default_recall_limit() -> usize {
    8
}

const fn default_recall_context_turns() -> usize {
    3
}

const fn default_recall_query_chars() -> usize {
    4000
}

const fn default_injection_tokens() -> usize {
    5000
}

const fn default_shutdown_timeout_ms() -> u64 {
    30_000
}

impl Default for MnemopiSettings {
    fn default() -> Self {
        Self {
            db_path: None,
            bank: None,
            scoping: BankScoping::PerProject,
            auto_recall: true,
            auto_retain: true,
            polyphonic_recall: false,
            enhanced_recall: false,
            proactive_linking: false,
            working_memory_limit: default_working_memory_limit(),
            working_memory_ttl_hours: default_working_memory_ttl_hours(),
            retain_every_n_turns: default_retain_turns(),
            recall_limit: default_recall_limit(),
            recall_context_turns: default_recall_context_turns(),
            recall_max_query_chars: default_recall_query_chars(),
            injection_token_limit: default_injection_tokens(),
            llm_mode: MemoryLlmMode::Smol,
            debug: false,
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
        }
    }
}

impl MnemopiSettings {
    /// Applies parity floors and bounded operational ceilings.
    #[must_use]
    pub fn normalize(mut self) -> Self {
        self.working_memory_limit = self.working_memory_limit.min(1_000_000);
        self.working_memory_ttl_hours = self.working_memory_ttl_hours.clamp(1, 24 * 365 * 10);
        self.retain_every_n_turns = self.retain_every_n_turns.clamp(1, 10_000);
        self.recall_limit = self.recall_limit.clamp(1, 50);
        self.recall_context_turns = self.recall_context_turns.clamp(1, 64);
        self.recall_max_query_chars = self.recall_max_query_chars.clamp(256, 64 * 1024);
        self.injection_token_limit = self.injection_token_limit.clamp(256, 32 * 1024);
        self.shutdown_timeout_ms = self.shutdown_timeout_ms.clamp(1, 10 * 60 * 1000);
        self
    }
}

/// Persisted memory selector and backend settings.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemorySettings {
    /// Active backend; omitted means [`MemoryBackend::Off`].
    #[serde(default)]
    pub backend: MemoryBackend,
}

/// Automatic-learning settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutolearnSettings {
    /// Enables managed-skill guidance and capture eligibility.
    #[serde(default)]
    pub enabled: bool,
    /// Schedules a private capture turn when a substantive turn settles.
    #[serde(default)]
    pub auto_continue: bool,
    /// Minimum settled tool executions in the turn.
    #[serde(default = "default_min_tool_calls")]
    pub min_tool_calls: usize,
}

const fn default_min_tool_calls() -> usize {
    5
}

impl Default for AutolearnSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_continue: false,
            min_tool_calls: default_min_tool_calls(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_parity_values() {
        let settings = MnemopiSettings::default();
        assert_eq!(settings.scoping, BankScoping::PerProject);
        assert_eq!(settings.llm_mode, MemoryLlmMode::Smol);
        assert_eq!(settings.retain_every_n_turns, 4);
        assert_eq!(settings.recall_limit, 8);
        assert_eq!(settings.recall_context_turns, 3);
        assert_eq!(settings.injection_token_limit, 5000);
        assert!(settings.auto_recall);
        assert!(settings.auto_retain);
        assert!(!settings.polyphonic_recall);
        assert_eq!(MemorySettings::default().backend, MemoryBackend::Off);
    }

    #[test]
    fn normalize_clamps_every_bounded_field() {
        let normalized = MnemopiSettings {
            working_memory_limit: usize::MAX,
            working_memory_ttl_hours: 0,
            retain_every_n_turns: 0,
            recall_limit: 0,
            recall_context_turns: 0,
            recall_max_query_chars: 1,
            injection_token_limit: 1,
            shutdown_timeout_ms: 0,
            ..MnemopiSettings::default()
        }
        .normalize();

        assert_eq!(normalized.working_memory_limit, 1_000_000);
        assert_eq!(normalized.working_memory_ttl_hours, 1);
        assert_eq!(normalized.retain_every_n_turns, 1);
        assert_eq!(normalized.recall_limit, 1);
        assert_eq!(normalized.recall_context_turns, 1);
        assert_eq!(normalized.recall_max_query_chars, 256);
        assert_eq!(normalized.injection_token_limit, 256);
        assert_eq!(normalized.shutdown_timeout_ms, 1);

        let high = MnemopiSettings {
            recall_limit: 1_000,
            recall_context_turns: 1_000,
            injection_token_limit: 1_000_000,
            shutdown_timeout_ms: u64::MAX,
            ..MnemopiSettings::default()
        }
        .normalize();
        assert_eq!(high.recall_limit, 50);
        assert_eq!(high.recall_context_turns, 64);
        assert_eq!(high.injection_token_limit, 32 * 1024);
        assert_eq!(high.shutdown_timeout_ms, 10 * 60 * 1000);
    }

    #[test]
    fn settings_round_trip_through_serde() {
        let settings = MnemopiSettings {
            bank: Some("shared".to_owned()),
            scoping: BankScoping::PerProjectTagged,
            llm_mode: MemoryLlmMode::Remote,
            ..MnemopiSettings::default()
        };
        let encoded = serde_json::to_string(&settings).expect("encode");
        let decoded: MnemopiSettings = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, settings);
        assert!(encoded.contains("\"per-project-tagged\""));
        assert!(encoded.contains("\"remote\""));
    }

    #[test]
    fn omitted_fields_take_documented_defaults() {
        let decoded: MnemopiSettings = serde_json::from_str("{}").expect("decode");
        assert_eq!(decoded, MnemopiSettings::default());
        let memory: MemorySettings = serde_json::from_str("{}").expect("decode");
        assert_eq!(memory.backend, MemoryBackend::Off);
    }

    #[test]
    fn backend_and_scoping_parsers_accept_persisted_spellings() {
        assert_eq!(
            MemoryBackend::parse("MnemoPi"),
            Some(MemoryBackend::Mnemopi)
        );
        assert_eq!(MemoryBackend::parse("off"), Some(MemoryBackend::Off));
        assert_eq!(MemoryBackend::parse("hindsight"), None);
        assert_eq!(
            BankScoping::parse("per-project-tagged"),
            Some(BankScoping::PerProjectTagged)
        );
        assert_eq!(MemoryLlmMode::parse("none"), Some(MemoryLlmMode::None));
        assert!(!MemoryLlmMode::None.extraction_enabled());
        assert!(MemoryLlmMode::Smol.extraction_enabled());
    }
}
