#![allow(dead_code)]
//! `/memory` slash command backend.
//!
//! The slash entry point is deliberately thin: [`handle_memory_command`] parses
//! the verb out of the raw argument string and routes it through the
//! [`MemorySlashBackend`] trait. The concrete implementation,
//! [`OmpMemoryBackend`], is backed by `omp_memory`'s runtime and the
//! `memory.*` settings domain.
//!
//! Wiring into `slash.rs`, replacing the stub at the `"memory"` arm:
//!
//! ```ignore
//! "memory" => {
//!     match crate::features::memory::handle_memory_command(rt.cwd(), args) {
//!         MemoryCommandOutput::Output(text) => output(rt, &text),
//!         MemoryCommandOutput::Usage(usage) => return Some(usage(rt, usage)),
//!     }
//!     Some(Handled { agent_invoked: false })
//! }
//! ```
//!
//! # Adapter surface
//!
//! Everything that names an `omp_memory` or `omp_settings` API lives in the
//! block marked "engine adapter" ([`backend_kind`], [`start_runtime`],
//! [`render_json`], [`OmpMemoryBackend`] and its trait impl). The verb table,
//! dispatch, support matrix and error rendering above it are pure and unit
//! tested without a database.
//!
//! The engine calls made by the adapter are, and must stay, exactly this set:
//!
//! * `omp_settings::Settings::{load, get_string, get_bool, get_number}`
//! * `omp_memory::MemoryRuntime::start(RuntimeStart) -> Result<Arc<MemoryRuntime>, _>`
//! * `MemoryRuntime::{prompt_snapshot, stats, diagnose, status, clear, enqueue}`
//! * `MemoryRuntime::mnemopi_settings`
//! * `omp_memory::INACTIVE_MESSAGE`
//!
//! Anything beyond that set is a compile-time break in this one file.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use omp_memory::runtime::{PromptSnapshot, RuntimeStart};
use omp_memory::{
    config::MemoryLlmMode, BankScoping, MemoryBackend, MemoryRuntime, MnemopiSettings,
    INACTIVE_MESSAGE,
};
use omp_settings::Settings;

/// Canonical usage line, identical to the string `slash.rs` printed for the stub.
pub const USAGE: &str =
    "Usage: /memory <view|stats|diagnose|clear|reset|enqueue|rebuild|queue|sync>";

/// `mm` notice for backends that keep mental models behind an HTTP API.
pub const MM_HINDSIGHT_NOTICE: &str = "Mental-model maintenance via /memory mm is unsupported in ACP mode; use the hindsight HTTP API directly.";

/// `mm` notice for every other backend.
pub const MM_ACP_NOTICE: &str =
    "Mental-model maintenance via /memory mm is unsupported in ACP mode.";

pub const VIEW_EMPTY: &str = "Memory payload is empty.";
pub const CLEARED: &str = "Memory cleared.";
pub const ENQUEUED: &str = "Memory consolidation enqueued.";
pub const SYNCED: &str = "Memory consolidation ran.";

/// What `slash.rs` should do with the result of a `/memory` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommandOutput {
    /// Print the text in the transcript.
    Output(String),
    /// Print the text and treat the invocation as a usage error.
    Usage(&'static str),
}

impl MemoryCommandOutput {
    pub fn text(&self) -> &str {
        match self {
            Self::Output(text) => text,
            Self::Usage(usage) => usage,
        }
    }

    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }

    pub fn into_text(self) -> String {
        match self {
            Self::Output(text) => text,
            Self::Usage(usage) => usage.to_string(),
        }
    }
}

/// A `/memory` subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryVerb {
    View,
    Stats,
    Diagnose,
    Clear,
    Reset,
    Enqueue,
    Rebuild,
    Queue,
    Sync,
    Mm,
}

impl MemoryVerb {
    pub const ALL: [Self; 10] = [
        Self::View,
        Self::Stats,
        Self::Diagnose,
        Self::Clear,
        Self::Reset,
        Self::Enqueue,
        Self::Rebuild,
        Self::Queue,
        Self::Sync,
        Self::Mm,
    ];

    /// Parse the first token of the argument string. An empty token selects
    /// `view`, matching the upstream default for a bare `/memory`.
    pub fn parse(token: &str) -> Option<Self> {
        Some(match token.trim() {
            "" | "view" => Self::View,
            "stats" => Self::Stats,
            "diagnose" => Self::Diagnose,
            "clear" => Self::Clear,
            "reset" => Self::Reset,
            "enqueue" => Self::Enqueue,
            "rebuild" => Self::Rebuild,
            "queue" => Self::Queue,
            "sync" => Self::Sync,
            "mm" => Self::Mm,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Stats => "stats",
            Self::Diagnose => "diagnose",
            Self::Clear => "clear",
            Self::Reset => "reset",
            Self::Enqueue => "enqueue",
            Self::Rebuild => "rebuild",
            Self::Queue => "queue",
            Self::Sync => "sync",
            Self::Mm => "mm",
        }
    }
}

/// Failure modes a backend can report for a verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySlashError {
    /// The configured backend has no implementation for this verb.
    Unavailable {
        backend: &'static str,
        verb: &'static str,
    },
    /// The backend exists but could not serve the request.
    Runtime {
        backend: &'static str,
        verb: &'static str,
        detail: String,
    },
    /// The settings store could not be read, so no backend could be chosen.
    Settings { detail: String },
}

impl MemorySlashError {
    pub fn unavailable(backend: &'static str, verb: &'static str) -> Self {
        Self::Unavailable { backend, verb }
    }

    pub fn runtime(backend: &'static str, verb: &'static str, detail: impl Into<String>) -> Self {
        Self::Runtime {
            backend,
            verb,
            detail: detail.into(),
        }
    }

    pub fn settings(detail: impl Into<String>) -> Self {
        Self::Settings {
            detail: detail.into(),
        }
    }

    /// The verb this failure belongs to, when it belongs to one.
    pub fn verb(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { verb, .. } | Self::Runtime { verb, .. } => Some(verb),
            Self::Settings { .. } => None,
        }
    }

    pub fn render(&self) -> String {
        match self {
            Self::Unavailable { backend, verb } => {
                format!("Memory is not available for the {backend} backend (/{verb}).")
            }
            Self::Runtime {
                backend,
                verb,
                detail,
            } => format!("Memory /{verb} failed for the {backend} backend: {detail}"),
            Self::Settings { detail } => format!("Memory settings could not be loaded: {detail}"),
        }
    }
}

impl fmt::Display for MemorySlashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl std::error::Error for MemorySlashError {}

/// The `memory.backend` setting domain, as a closed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Off,
    Local,
    Hindsight,
    Mnemopi,
    Sharpshooter,
}

impl BackendKind {
    /// Settings id this enum mirrors.
    pub const SETTING: &'static str = "memory.backend";

    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::Local,
        Self::Hindsight,
        Self::Mnemopi,
        Self::Sharpshooter,
    ];

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "off" => Self::Off,
            "local" => Self::Local,
            "hindsight" => Self::Hindsight,
            "mnemopi" => Self::Mnemopi,
            "sharpshooter" => Self::Sharpshooter,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
            Self::Hindsight => "hindsight",
            Self::Mnemopi => "mnemopi",
            Self::Sharpshooter => "sharpshooter",
        }
    }

    /// `true` when the backend owns an on-disk engine this crate drives through
    /// `omp_memory`. `hindsight` and `sharpshooter` are remote, and `off` has no
    /// store at all.
    pub fn is_engine_backed(self) -> bool {
        matches!(self, Self::Local | Self::Mnemopi)
    }

    /// Whether `verb` can be served by this backend.
    ///
    /// `view` and `mm` always answer (with the inactive payload and the ACP
    /// notice respectively); every mutating or inspecting verb needs a local
    /// engine.
    pub fn supports(self, verb: MemoryVerb) -> bool {
        match verb {
            MemoryVerb::View | MemoryVerb::Mm => true,
            MemoryVerb::Stats
            | MemoryVerb::Diagnose
            | MemoryVerb::Clear
            | MemoryVerb::Reset
            | MemoryVerb::Enqueue
            | MemoryVerb::Rebuild
            | MemoryVerb::Queue
            | MemoryVerb::Sync => self.is_engine_backed(),
        }
    }
}

/// Per-subcommand behaviour. Implementations may be partial: the default
/// methods define the aliases (`reset` -> `clear`, `rebuild` -> `enqueue`,
/// `sync` -> `enqueue` then confirm) so a backend only overrides what differs.
pub trait MemorySlashBackend {
    fn backend_id(&self) -> &'static str;

    fn view(&self) -> Result<String, MemorySlashError>;
    fn stats(&self) -> Result<String, MemorySlashError>;
    fn diagnose(&self) -> Result<String, MemorySlashError>;
    fn clear(&self) -> Result<String, MemorySlashError>;
    fn enqueue(&self) -> Result<String, MemorySlashError>;
    fn queue(&self) -> Result<String, MemorySlashError>;

    fn reset(&self) -> Result<String, MemorySlashError> {
        self.clear()
    }

    fn rebuild(&self) -> Result<String, MemorySlashError> {
        self.enqueue()
    }

    fn sync(&self) -> Result<String, MemorySlashError> {
        self.enqueue()?;
        Ok(SYNCED.to_string())
    }

    fn mm(&self) -> String {
        match self.backend_id() {
            "hindsight" => MM_HINDSIGHT_NOTICE.to_string(),
            _ => MM_ACP_NOTICE.to_string(),
        }
    }
}

/// Route one already-parsed verb to `backend`.
pub fn dispatch(backend: &dyn MemorySlashBackend, args: &str) -> MemoryCommandOutput {
    let Some(verb) = MemoryVerb::parse(first_token(args)) else {
        return MemoryCommandOutput::Usage(USAGE);
    };

    let result = match verb {
        MemoryVerb::View => backend.view(),
        MemoryVerb::Stats => backend.stats(),
        MemoryVerb::Diagnose => backend.diagnose(),
        MemoryVerb::Clear => backend.clear(),
        MemoryVerb::Reset => backend.reset(),
        MemoryVerb::Enqueue => backend.enqueue(),
        MemoryVerb::Rebuild => backend.rebuild(),
        MemoryVerb::Queue => backend.queue(),
        MemoryVerb::Sync => backend.sync(),
        MemoryVerb::Mm => Ok(backend.mm()),
    };

    MemoryCommandOutput::Output(match result {
        Ok(text) => text,
        Err(err) => err.render(),
    })
}

/// Entry point for `slash.rs`: resolve the configured backend for `cwd` and run
/// the requested verb.
///
/// The verb is validated before any settings or engine work so a typo never
/// pays for a runtime start.
pub fn handle_memory_command(cwd: &Path, args: &str) -> MemoryCommandOutput {
    if MemoryVerb::parse(first_token(args)).is_none() {
        return MemoryCommandOutput::Usage(USAGE);
    }
    match OmpMemoryBackend::open(cwd) {
        Ok(backend) => dispatch(&backend, args),
        Err(err) => MemoryCommandOutput::Output(err.render()),
    }
}

fn first_token(args: &str) -> &str {
    args.split_whitespace().next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// engine adapter: the only code that names `omp_memory` / `omp_settings` APIs
// ---------------------------------------------------------------------------

/// Read `memory.backend` and map it onto [`BackendKind`].
///
/// Unset or unrecognised values fall back to `off`, which is the schema default.
fn backend_kind(settings: &Settings) -> BackendKind {
    settings
        .get_string(BackendKind::SETTING)
        .and_then(|id| BackendKind::from_id(&id))
        .unwrap_or(BackendKind::Off)
}

/// Project the `mnemopi.*` settings domain onto the engine's typed config.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // finite, non-negative counts
fn mnemopi_settings(settings: &Settings) -> MnemopiSettings {
    let defaults = MnemopiSettings::default();
    let text = |id: &str| {
        settings
            .get_string(id)
            .filter(|value| !value.trim().is_empty())
    };
    let count = |id: &str, fallback: usize| {
        settings
            .get_number(id)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map_or(fallback, |value| value as usize)
    };
    MnemopiSettings {
        db_path: text("mnemopi.dbPath").map(PathBuf::from),
        bank: text("mnemopi.bank"),
        scoping: text("mnemopi.scoping")
            .and_then(|value| BankScoping::parse(&value))
            .unwrap_or(defaults.scoping),
        auto_recall: settings
            .get_bool("mnemopi.autoRecall")
            .unwrap_or(defaults.auto_recall),
        auto_retain: settings
            .get_bool("mnemopi.autoRetain")
            .unwrap_or(defaults.auto_retain),
        polyphonic_recall: settings
            .get_bool("mnemopi.polyphonicRecall")
            .unwrap_or(defaults.polyphonic_recall),
        enhanced_recall: settings
            .get_bool("mnemopi.enhancedRecall")
            .unwrap_or(defaults.enhanced_recall),
        proactive_linking: settings
            .get_bool("mnemopi.proactiveLinking")
            .unwrap_or(defaults.proactive_linking),
        retain_every_n_turns: count("mnemopi.retainEveryNTurns", defaults.retain_every_n_turns),
        recall_limit: count("mnemopi.recallLimit", defaults.recall_limit),
        recall_context_turns: count("mnemopi.recallContextTurns", defaults.recall_context_turns),
        recall_max_query_chars: count(
            "mnemopi.recallMaxQueryChars",
            defaults.recall_max_query_chars,
        ),
        injection_token_limit: count(
            "mnemopi.injectionTokenLimit",
            defaults.injection_token_limit,
        ),
        llm_mode: text("mnemopi.llmMode")
            .and_then(|value| MemoryLlmMode::parse(&value))
            .unwrap_or(defaults.llm_mode),
        debug: settings.get_bool("mnemopi.debug").unwrap_or(defaults.debug),
        ..defaults
    }
}

/// Session identity for a slash-command runtime (one per process).
fn slash_session_id() -> String {
    format!("slash-memory-{}", std::process::id())
}

/// Private memory data root (`$OMP_AGENT_DIR` or `~/.omp/agent`).
fn memory_data_dir() -> PathBuf {
    omp_providers::AgentDir::default().0.join("memory")
}

/// Open the on-disk engine for an engine-backed kind.
fn start_runtime(cwd: &Path, settings: &Settings) -> Result<Arc<MemoryRuntime>, String> {
    let input = RuntimeStart {
        session_id: slash_session_id(),
        data_dir: memory_data_dir(),
        workspace_root: cwd.to_path_buf(),
        canonical_primary_root: None,
        backend: MemoryBackend::Mnemopi,
        mnemopi: mnemopi_settings(settings),
    };
    MemoryRuntime::start(input).map_err(|err| err.to_string())
}

/// Flatten the non-empty prompt slots of one snapshot into displayable text.
fn snapshot_text(snapshot: &PromptSnapshot) -> String {
    [&snapshot.memory, &snapshot.standing, &snapshot.recall]
        .iter()
        .filter_map(|slot| slot.content.as_deref())
        .map(str::trim)
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| format!("<memory report could not be rendered: {err}>"))
}

/// `/memory` backend backed by `omp_memory` and the `memory.*` settings domain.
pub struct OmpMemoryBackend {
    kind: BackendKind,
    cwd: PathBuf,
    runtime: Option<Arc<MemoryRuntime>>,
    start_error: Option<String>,
}

impl OmpMemoryBackend {
    /// Build a backend around an already-started (or absent) runtime.
    pub fn new(
        kind: BackendKind,
        cwd: impl Into<PathBuf>,
        runtime: Option<Arc<MemoryRuntime>>,
    ) -> Self {
        Self {
            kind,
            cwd: cwd.into(),
            runtime,
            start_error: None,
        }
    }

    /// Load settings and start the engine when the configured backend needs one.
    pub fn open(cwd: &Path) -> Result<Self, MemorySlashError> {
        let settings = Settings::load(cwd);
        Ok(Self::from_settings(cwd, &settings))
    }

    /// Build from an already-loaded settings value.
    pub fn from_settings(cwd: &Path, settings: &Settings) -> Self {
        let kind = backend_kind(settings);
        let (runtime, start_error) = if kind.is_engine_backed() {
            match start_runtime(cwd, settings) {
                Ok(runtime) => (Some(runtime), None),
                Err(err) => (None, Some(err)),
            }
        } else {
            (None, None)
        };
        Self {
            kind,
            cwd: cwd.to_path_buf(),
            runtime,
            start_error,
        }
    }

    pub fn kind(&self) -> BackendKind {
        self.kind
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn runtime(&self) -> Option<&MemoryRuntime> {
        self.runtime.as_deref()
    }

    /// Resolve the runtime for an engine verb, reporting why it is missing.
    ///
    /// The support matrix is consulted first: a backend that does not serve
    /// `verb` at all reports `Unavailable`, whether or not a runtime exists.
    fn engine(&self, verb: MemoryVerb) -> Result<&MemoryRuntime, MemorySlashError> {
        if !self.kind.supports(verb) {
            return Err(MemorySlashError::unavailable(self.kind.id(), verb.as_str()));
        }
        if let Some(runtime) = self.runtime.as_deref() {
            return Ok(runtime);
        }
        let detail = self
            .start_error
            .clone()
            .unwrap_or_else(|| "no memory runtime is attached to this session".to_string());
        Err(MemorySlashError::runtime(
            self.kind.id(),
            verb.as_str(),
            detail,
        ))
    }
}

impl MemorySlashBackend for OmpMemoryBackend {
    fn backend_id(&self) -> &'static str {
        self.kind.id()
    }

    fn view(&self) -> Result<String, MemorySlashError> {
        if !self.kind.is_engine_backed() {
            return Ok(INACTIVE_MESSAGE.to_string());
        }
        let runtime = self.engine(MemoryVerb::View)?;
        let budget = runtime.mnemopi_settings().map_or_else(
            |_| MnemopiSettings::default().injection_token_limit,
            |settings| settings.injection_token_limit,
        );
        let snapshot = runtime
            .prompt_snapshot(None, None, budget)
            .map_err(|err| MemorySlashError::runtime(self.kind.id(), "view", err.to_string()))?;
        let payload = snapshot_text(&snapshot);
        if payload.trim().is_empty() {
            Ok(VIEW_EMPTY.to_string())
        } else {
            Ok(payload)
        }
    }

    fn stats(&self) -> Result<String, MemorySlashError> {
        let runtime = self.engine(MemoryVerb::Stats)?;
        let stats = runtime
            .stats()
            .map_err(|err| MemorySlashError::runtime(self.kind.id(), "stats", err.to_string()))?;
        Ok(render_json(&stats))
    }

    fn diagnose(&self) -> Result<String, MemorySlashError> {
        let runtime = self.engine(MemoryVerb::Diagnose)?;
        let report = runtime.diagnose().map_err(|err| {
            MemorySlashError::runtime(self.kind.id(), "diagnose", err.to_string())
        })?;
        Ok(render_json(&report))
    }

    fn clear(&self) -> Result<String, MemorySlashError> {
        let runtime = self.engine(MemoryVerb::Clear)?;
        runtime
            .clear()
            .map_err(|err| MemorySlashError::runtime(self.kind.id(), "clear", err.to_string()))?;
        Ok(CLEARED.to_string())
    }

    fn enqueue(&self) -> Result<String, MemorySlashError> {
        let runtime = self.engine(MemoryVerb::Enqueue)?;
        runtime
            .enqueue()
            .map_err(|err| MemorySlashError::runtime(self.kind.id(), "enqueue", err.to_string()))?;
        Ok(ENQUEUED.to_string())
    }

    fn queue(&self) -> Result<String, MemorySlashError> {
        let runtime = self.engine(MemoryVerb::Queue)?;
        Ok(render_json(&runtime.status()))
    }

    // The aliases below re-check support under their own verb name so an
    // unsupported backend reports `/reset`, `/rebuild` and `/sync` rather than
    // the verb they delegate to.

    fn reset(&self) -> Result<String, MemorySlashError> {
        self.engine(MemoryVerb::Reset)?;
        self.clear()
    }

    fn rebuild(&self) -> Result<String, MemorySlashError> {
        self.engine(MemoryVerb::Rebuild)?;
        self.enqueue()
    }

    fn sync(&self) -> Result<String, MemorySlashError> {
        self.engine(MemoryVerb::Sync)?;
        self.enqueue()?;
        Ok(SYNCED.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct FakeBackend {
        kind: &'static str,
        calls: RefCell<Vec<&'static str>>,
        failure: Option<MemorySlashError>,
    }

    impl FakeBackend {
        fn new(kind: &'static str) -> Self {
            Self {
                kind,
                calls: RefCell::new(Vec::new()),
                failure: None,
            }
        }

        fn failing(kind: &'static str, failure: MemorySlashError) -> Self {
            Self {
                kind,
                calls: RefCell::new(Vec::new()),
                failure: Some(failure),
            }
        }

        fn record(&self, verb: &'static str) -> Result<String, MemorySlashError> {
            self.calls.borrow_mut().push(verb);
            match self.failure.clone() {
                Some(err) => Err(err),
                None => Ok(format!("{verb}:{}", self.kind)),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl MemorySlashBackend for FakeBackend {
        fn backend_id(&self) -> &'static str {
            self.kind
        }

        fn view(&self) -> Result<String, MemorySlashError> {
            self.record("view")
        }

        fn stats(&self) -> Result<String, MemorySlashError> {
            self.record("stats")
        }

        fn diagnose(&self) -> Result<String, MemorySlashError> {
            self.record("diagnose")
        }

        fn clear(&self) -> Result<String, MemorySlashError> {
            self.record("clear")
        }

        fn enqueue(&self) -> Result<String, MemorySlashError> {
            self.record("enqueue")
        }

        fn queue(&self) -> Result<String, MemorySlashError> {
            self.record("queue")
        }
    }

    fn output(backend: &dyn MemorySlashBackend, args: &str) -> String {
        let out = dispatch(backend, args);
        assert!(!out.is_usage(), "unexpected usage for args {args:?}");
        out.into_text()
    }

    #[test]
    fn every_verb_parses_from_its_own_token() {
        for verb in MemoryVerb::ALL {
            assert_eq!(MemoryVerb::parse(verb.as_str()), Some(verb));
        }
    }

    #[test]
    fn empty_token_defaults_to_view() {
        assert_eq!(MemoryVerb::parse(""), Some(MemoryVerb::View));
        assert_eq!(MemoryVerb::parse("   "), Some(MemoryVerb::View));
    }

    #[test]
    fn unknown_token_is_rejected() {
        assert_eq!(MemoryVerb::parse("nope"), None);
        assert_eq!(MemoryVerb::parse("VIEW"), None);
    }

    #[test]
    fn backend_kind_round_trips_every_id() {
        for kind in BackendKind::ALL {
            assert_eq!(BackendKind::from_id(kind.id()), Some(kind));
        }
        assert_eq!(BackendKind::from_id("mnemosyne"), None);
        assert_eq!(BackendKind::from_id(""), None);
    }

    #[test]
    fn only_local_and_mnemopi_are_engine_backed() {
        assert!(BackendKind::Local.is_engine_backed());
        assert!(BackendKind::Mnemopi.is_engine_backed());
        assert!(!BackendKind::Off.is_engine_backed());
        assert!(!BackendKind::Hindsight.is_engine_backed());
        assert!(!BackendKind::Sharpshooter.is_engine_backed());
    }

    #[test]
    fn support_matrix_matches_backend_capability() {
        for verb in MemoryVerb::ALL {
            assert!(
                BackendKind::Local.supports(verb),
                "local must serve {verb:?}"
            );
            assert!(
                BackendKind::Mnemopi.supports(verb),
                "mnemopi must serve {verb:?}"
            );
        }
        for kind in [
            BackendKind::Off,
            BackendKind::Hindsight,
            BackendKind::Sharpshooter,
        ] {
            assert!(kind.supports(MemoryVerb::View));
            assert!(kind.supports(MemoryVerb::Mm));
            for verb in [
                MemoryVerb::Stats,
                MemoryVerb::Diagnose,
                MemoryVerb::Clear,
                MemoryVerb::Reset,
                MemoryVerb::Enqueue,
                MemoryVerb::Rebuild,
                MemoryVerb::Queue,
                MemoryVerb::Sync,
            ] {
                assert!(
                    !kind.supports(verb),
                    "{} must not serve {verb:?}",
                    kind.id()
                );
            }
        }
    }

    #[test]
    fn every_subcommand_dispatches_to_its_own_hook() {
        let cases = [
            ("view", "view:mnemopi", "view"),
            ("stats", "stats:mnemopi", "stats"),
            ("diagnose", "diagnose:mnemopi", "diagnose"),
            ("clear", "clear:mnemopi", "clear"),
            ("reset", "clear:mnemopi", "clear"),
            ("enqueue", "enqueue:mnemopi", "enqueue"),
            ("rebuild", "enqueue:mnemopi", "enqueue"),
            ("queue", "queue:mnemopi", "queue"),
        ];
        for (args, expected, expected_call) in cases {
            let backend = FakeBackend::new("mnemopi");
            assert_eq!(output(&backend, args), expected, "args {args:?}");
            assert_eq!(backend.calls(), vec![expected_call], "args {args:?}");
        }
    }

    #[test]
    fn sync_enqueues_before_confirming() {
        let backend = FakeBackend::new("mnemopi");
        assert_eq!(output(&backend, "sync"), SYNCED);
        assert_eq!(backend.calls(), vec!["enqueue"]);
    }

    #[test]
    fn bare_memory_views_the_payload() {
        let backend = FakeBackend::new("local");
        assert_eq!(output(&backend, ""), "view:local");
        assert_eq!(backend.calls(), vec!["view"]);
    }

    #[test]
    fn trailing_arguments_are_ignored() {
        let backend = FakeBackend::new("local");
        assert_eq!(output(&backend, "  stats   extra words "), "stats:local");
        assert_eq!(backend.calls(), vec!["stats"]);
    }

    #[test]
    fn unknown_subcommand_returns_usage() {
        let backend = FakeBackend::new("local");
        let out = dispatch(&backend, "consolidate");
        assert_eq!(out, MemoryCommandOutput::Usage(USAGE));
        assert!(out.is_usage());
        assert_eq!(out.text(), USAGE);
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn handle_memory_command_rejects_unknown_verbs_before_loading_settings() {
        let out = handle_memory_command(Path::new("/nonexistent/project"), "consolidate");
        assert_eq!(out, MemoryCommandOutput::Usage(USAGE));
        assert_eq!(out.into_text(), USAGE);
    }

    #[test]
    fn unavailable_error_renders_the_backend_and_verb() {
        let backend = FakeBackend::failing(
            "hindsight",
            MemorySlashError::unavailable("hindsight", "stats"),
        );
        assert_eq!(
            output(&backend, "stats"),
            "Memory is not available for the hindsight backend (/stats)."
        );
    }

    #[test]
    fn runtime_error_renders_the_detail() {
        let backend = FakeBackend::failing(
            "mnemopi",
            MemorySlashError::runtime("mnemopi", "clear", "database is locked"),
        );
        assert_eq!(
            output(&backend, "clear"),
            "Memory /clear failed for the mnemopi backend: database is locked"
        );
    }

    #[test]
    fn settings_error_has_no_verb_and_names_the_cause() {
        let err = MemorySlashError::settings("permission denied");
        assert_eq!(err.verb(), None);
        assert_eq!(
            err.render(),
            "Memory settings could not be loaded: permission denied"
        );
        assert_eq!(err.to_string(), err.render());
    }

    #[test]
    fn error_verb_accessor_tracks_the_failing_verb() {
        assert_eq!(
            MemorySlashError::unavailable("off", "queue").verb(),
            Some("queue")
        );
        assert_eq!(
            MemorySlashError::runtime("local", "sync", "boom").verb(),
            Some("sync")
        );
    }

    #[test]
    fn mm_reports_the_backend_specific_notice() {
        assert_eq!(FakeBackend::new("hindsight").mm(), MM_HINDSIGHT_NOTICE);
        assert_eq!(FakeBackend::new("mnemopi").mm(), MM_ACP_NOTICE);
        assert_eq!(FakeBackend::new("off").mm(), MM_ACP_NOTICE);
        let out = dispatch(&FakeBackend::new("hindsight"), "mm");
        assert_eq!(out.into_text(), MM_HINDSIGHT_NOTICE);
    }

    #[test]
    fn off_backend_view_reports_inactive_without_a_runtime() {
        let backend = OmpMemoryBackend::new(BackendKind::Off, "/tmp/project", None);
        assert_eq!(output(&backend, "view"), INACTIVE_MESSAGE);
        assert_eq!(output(&backend, ""), INACTIVE_MESSAGE);
    }

    #[test]
    fn off_backend_refuses_engine_verbs_with_the_unavailable_message() {
        let backend = OmpMemoryBackend::new(BackendKind::Off, "/tmp/project", None);
        for verb in [
            "stats", "diagnose", "queue", "clear", "reset", "enqueue", "rebuild", "sync",
        ] {
            assert_eq!(
                output(&backend, verb),
                format!("Memory is not available for the off backend (/{verb})."),
                "verb {verb}"
            );
        }
    }

    #[test]
    fn hindsight_backend_refuses_engine_verbs_with_the_unavailable_message() {
        let backend = OmpMemoryBackend::new(BackendKind::Hindsight, "/tmp/project", None);
        assert_eq!(
            output(&backend, "stats"),
            "Memory is not available for the hindsight backend (/stats)."
        );
    }

    #[test]
    fn engine_backend_without_a_runtime_reports_the_reason() {
        let backend = OmpMemoryBackend::new(BackendKind::Mnemopi, "/tmp/project", None);
        assert_eq!(
            output(&backend, "stats"),
            "Memory /stats failed for the mnemopi backend: no memory runtime is attached to this session"
        );
        assert_eq!(backend.kind(), BackendKind::Mnemopi);
        assert_eq!(backend.cwd(), Path::new("/tmp/project"));
        assert!(backend.runtime().is_none());
    }

    #[test]
    fn backend_exposes_its_kind_and_runtime() {
        let backend = OmpMemoryBackend::new(BackendKind::Local, "/tmp/project", None);
        assert_eq!(backend.backend_id(), "local");
        assert_eq!(backend.kind(), BackendKind::Local);
        assert!(backend.runtime().is_none());
    }

    #[test]
    fn mm_on_an_engine_backend_never_touches_the_runtime() {
        let backend = OmpMemoryBackend::new(BackendKind::Mnemopi, "/tmp/project", None);
        assert_eq!(output(&backend, "mm"), MM_ACP_NOTICE);
    }
}
