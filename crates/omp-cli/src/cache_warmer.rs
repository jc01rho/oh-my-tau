//! Prompt-cache warmer: the port of omp's `session/cache-warmer.ts`.
//!
//! Bridges a long wait on one prompt-cache entry: it refreshes the entry before it expires while
//! renting it pays, and lets it expire once the refresh spend since the last real request would
//! pass the cost of losing it (ski rental). Every decision is priced and carried to extensions as
//! `cache_warming_decision`; a handler's `action` may override the warmer's own, and an override
//! the request cannot carry out leaves the warmer's decision standing ([`resolve_override`]).
//!
//! What tau can do that upstream also does, and what it cannot:
//!
//! * Only models whose cache lifetime this core can name are warmed: `anthropic-messages`, whose
//!   five-minute and one-hour ephemeral tiers the provider declares. Anything else is never touched,
//!   so a provider whose replay behavior is unvalidated never sees a warm request.
//! * A refresh is a one-token replay, cut off at its first generated block. The provider layer has
//!   no `max_tokens: 0` pre-warm, and a request whose reasoning budget would change with the output
//!   cap cannot be replayed without changing its cache key ([`is_replayable`]).
//! * Bridging a wait with compaction is not ported: no host compactor is wired, so `compact_cost`
//!   is never offered and a `compact` override is refused like any override the warmer cannot carry
//!   out. The rule itself is implemented and tested, so a bridge can be wired later.
//!
//! The decision engine is clock-injected ([`Warmer::decide_at`], [`Warmer::apply`],
//! [`Warmer::step_at`]) so tests drive it with an explicit `now` and never sleep; the driver task
//! owns the timer and the provider request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omp_ext::lifecycle::{
    CacheWarmingAction, CacheWarmingDecisionEvent, CacheWarmingOutcome, CacheWarmingRentMethod,
};
use pa_agent::stream::{
    AssistantMessageEvent, LlmContext, ModelStream, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{AssistantMessage, Model, StopReason, ThinkingLevel, Usage};

/// Anthropic's api id; the only prompt cache whose lifetime this core can name.
const ANTHROPIC_MESSAGES: &str = "anthropic-messages";
/// Idle warming uses a shorter horizon because continuation estimates age.
const MAX_IDLE_WARMING_AGE_MS: u64 = 30 * 60_000;
/// A refresh is sent only when it is expected to save at least this many dollars.
const MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// Chance that a real request arrives before the entry expires while the agent sits idle
/// (measured upstream over five-minute entries; per-session estimates were not better).
const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;
/// An extension `decide` call must answer inside the ten-second expiry margin.
const DECIDE_TIMEOUT: Duration = Duration::from_secs(2);
/// Billed output a replay is priced at before the session observed one for the model.
const REPLAY_OUTPUT_PRIOR_TOKENS: u64 = 512;

/// Warming mode (`providers.cacheWarming`): `off` disables it, `streaming` protects active runs,
/// `idle` also covers the gaps between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Streaming,
    Idle,
}

impl Mode {
    /// Parse a config or wire value; anything unknown is `off`, the safe direction.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "streaming" => Self::Streaming,
            "idle" => Self::Idle,
            _ => Self::Off,
        }
    }

    const fn wire(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Streaming => 1,
            Self::Idle => 2,
        }
    }

    const fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::Streaming,
            2 => Self::Idle,
            _ => Self::Off,
        }
    }
}

/// The mode new warmers start with, set from the session settings before the agent is assembled.
static MODE: AtomicU8 = AtomicU8::new(0);
/// The session's warmer, installed while the agent is assembled (one session per process).
static SESSION: Mutex<Option<Weak<Warmer>>> = Mutex::new(None);

#[must_use]
pub fn mode() -> Mode {
    Mode::from_wire(MODE.load(Ordering::Relaxed))
}

/// Set the warming mode and reconcile the session's active run.
pub fn set_mode(next: Mode) {
    MODE.store(next.wire(), Ordering::Relaxed);
    if let Some(warmer) = current() {
        warmer.set_mode(next);
    }
}

/// The session's warmer, when one was installed.
#[must_use]
pub fn current() -> Option<Arc<Warmer>> {
    SESSION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade)
}

/// Create the session's warmer. Its driver starts on the first warmed request, which always runs
/// inside the session's runtime.
pub fn install(emitter: Option<Arc<dyn DecisionEmitter>>) -> Arc<Warmer> {
    let warmer = Warmer::new(emitter);
    *SESSION.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(&warmer));
    warmer
}

/// Prompt-cache retention tier a request wrote its entry under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Short,
    Long,
}

impl Tier {
    const fn ttl_ms(self) -> u64 {
        match self {
            Self::Short => 300_000,
            Self::Long => 3_600_000,
        }
    }

    const fn duration(self) -> &'static str {
        match self {
            Self::Short => "5m",
            Self::Long => "1h",
        }
    }
}

/// Refresh at 90% of the TTL while preserving at least ten seconds of margin.
#[must_use]
pub fn get_cache_warming_delay_ms(ttl_ms: u64) -> Option<u64> {
    if ttl_ms <= 10_000 {
        return None;
    }
    Some((ttl_ms * 9 / 10).min(ttl_ms - 10_000).max(1))
}

/// Whether `api_key` is an Anthropic OAuth subscription token (`isAnthropicOAuthToken`).
fn is_oauth_token(api_key: Option<&str>) -> bool {
    api_key.is_some_and(|key| key.contains("sk-ant-oat"))
}

/// Whether the provider emits a real one-hour cache marker for `model`. Unsupported long retention
/// is emitted as the five-minute default, so the warmer must use that actual tier.
fn supports_long_cache_retention(model: &pa_types::ai::Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.raw.get("supportsLongCacheRetention"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// The prompt-cache tier and lifetime a request for `model` writes, or `None` when caching is off
/// or this core cannot name a lifetime for it.
///
/// Mirrors the Anthropic provider's `getCacheControl`: an OAuth subscriber seat writes one-hour
/// entries where the model supports them, and `PI_CACHE_RETENTION` wins over that default.
#[must_use]
pub fn prompt_cache_ttl(model: &pa_types::ai::Model, api_key: Option<&str>) -> Option<(Tier, u64)> {
    if model.api != ANTHROPIC_MESSAGES {
        return None;
    }
    let long_ok = supports_long_cache_retention(model);
    let default = if is_oauth_token(api_key) && long_ok {
        Tier::Long
    } else {
        Tier::Short
    };
    let tier = match std::env::var("PI_CACHE_RETENTION").as_deref() {
        Ok("none") => return None,
        Ok("long") => Tier::Long,
        Ok("short") => Tier::Short,
        _ => default,
    };
    let tier = if tier == Tier::Long && !long_ok {
        Tier::Short
    } else {
        tier
    };
    Some((tier, tier.ttl_ms()))
}

/// Whether replaying the request leaves its cache entry untouched.
///
/// Anthropic Messages derives a reasoning budget from `max_tokens`, so lowering the cap can change
/// that budget and the cache key. Adaptive-thinking models take an effort instead of a budget and
/// stay replayable.
#[must_use]
pub fn is_replayable(model: &Model, options: &StreamRequestOptions) -> bool {
    if model.api != ANTHROPIC_MESSAGES {
        return true;
    }
    let reasoning_requested = model.reasoning && options.reasoning != ThinkingLevel::Off;
    !reasoning_requested
        || pa_types::ai::model_family::classify(&model.id).supports_adaptive_thinking()
}

/// Prompt size of a response: what its request sent, cached or not.
#[must_use]
pub fn prompt_tokens_of(usage: &Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

/// Price of `tokens` at `rate` ($ per million tokens).
fn price(tokens: u64, rate: f64) -> f64 {
    // Token counts sit far below f64's 2^53 exact-integer range; the cost math is f64 by design.
    #[allow(clippy::cast_precision_loss)]
    let tokens = tokens as f64;
    tokens / 1_000_000.0 * rate
}

/// Price of writing `tokens` into the cache under `tier`.
///
/// Anthropic bills a one-hour write at twice the input price and a five-minute write at 1.25x, so
/// the tier's own multiplier is used where the catalog carries standard Anthropic pricing; other
/// models fall back to the catalog's single write price.
fn cache_write_price(model: &Model, tier: Tier, tokens: u64) -> f64 {
    if model.cost.cache_write <= 0.0 {
        return price(tokens, model.cost.input);
    }
    // Standard Anthropic pricing writes five-minute entries at 1.25x the input price; anything
    // else (a discounted seat, a reseller) is priced at the catalog's own write rate.
    let standard_anthropic = model.api == ANTHROPIC_MESSAGES
        && (model.cost.cache_write - model.cost.input * 1.25).abs() <= 1e-9;
    if standard_anthropic {
        let rate = pa_ai::get_anthropic_cache_write_cost(model.cost.input, tier.duration(), None);
        return price(tokens, rate);
    }
    price(tokens, model.cost.cache_write)
}

fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// "streaming" while the agent run that sent the request is still active, "idle" after it settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Streaming,
    Idle,
}

/// One priced warming decision, as `cache_warming_decision` carries it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub phase: Phase,
    /// How a refresh would be sent; `None` when the request cannot be refreshed safely.
    pub method: Option<CacheWarmingRentMethod>,
    /// Price of this refresh: a cache read of the prompt, plus the expected billed output of a replay.
    pub warm_cost: f64,
    /// Extra price of the next real request if the entry is lost.
    pub miss_cost: f64,
    /// Price of bridging with compaction; `None` when compaction is not an option here.
    pub compact_cost: Option<f64>,
    /// Refresh spend since the last real request.
    pub spent_cost: f64,
    /// Estimated chance that a real request arrives before the entry expires.
    pub continuation_probability: f64,
    /// `continuation_probability * miss_cost - warm_cost`.
    pub expected_savings: f64,
    /// False when the prompt size or the model's prices are unknown.
    pub economics_available: bool,
    pub action: CacheWarmingAction,
}

impl Decision {
    /// The `cache_warming_decision` payload for this decision.
    #[must_use]
    pub fn event(&self) -> CacheWarmingDecisionEvent {
        CacheWarmingDecisionEvent {
            method: self.method,
            warm_cost: self.warm_cost,
            miss_cost: self.miss_cost,
            compact_cost: self.compact_cost,
            spent_cost: self.spent_cost,
            continuation_probability: self.continuation_probability,
            action: self.action,
        }
    }
}

/// What the warmer is doing, for hosts that surface it.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// `false` when nothing is armed: warming is off, stopped, or waiting for the first request.
    pub scheduled: bool,
    pub reason: Option<String>,
    pub next_warm_at: Option<u64>,
    pub decision: Option<Decision>,
    pub extension_override: bool,
}

impl Status {
    fn inactive(reason: impl Into<String>) -> Self {
        Self {
            scheduled: false,
            reason: Some(reason.into()),
            next_warm_at: None,
            decision: None,
            extension_override: false,
        }
    }
}

/// The request whose prompt cache entry should be kept warm, exactly as it was sent.
pub struct Request {
    /// The provider seam the real turn used, so a refresh lands on the same cache key.
    pub base: StreamFn,
    pub model: Model,
    pub context: LlmContext,
    pub options: StreamRequestOptions,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("model", &model_key(&self.model))
            .finish_non_exhaustive()
    }
}

/// Asks extensions to override a warming decision.
pub trait DecisionEmitter: Send + Sync {
    /// Whether any loaded extension subscribes to `cache_warming_decision`.
    fn has_handlers(&self) -> bool;
    /// Emit the decision; the last handler's `action` wins.
    fn decide(
        &self,
        event: CacheWarmingDecisionEvent,
    ) -> pa_agent::BoxFut<'static, CacheWarmingOutcome>;
}

/// Bridges the session's extension host to the warmer.
pub struct ExtDecisionEmitter(pub Arc<crate::ext::Extensions>);

impl DecisionEmitter for ExtDecisionEmitter {
    fn has_handlers(&self) -> bool {
        self.0.cache_warming_handlers()
    }

    fn decide(
        &self,
        event: CacheWarmingDecisionEvent,
    ) -> pa_agent::BoxFut<'static, CacheWarmingOutcome> {
        let ext = Arc::clone(&self.0);
        Box::pin(async move {
            match ext.cache_warming_decision(&event).await {
                Ok(outcome) => outcome,
                // A failed emit leaves the warmer's own decision standing, as a throwing
                // handler does upstream.
                Err(e) => {
                    eprintln!("omt: extension cache_warming_decision: {e:#}");
                    CacheWarmingOutcome::default()
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Evaluation {
    run_id: u64,
    decision: Decision,
    stop_reason: &'static str,
}

struct Run {
    id: u64,
    request: Arc<Request>,
    tier: Tier,
    delay_ms: u64,
    started_at: u64,
    /// When the entry was last written or read.
    touched_at: u64,
    phase: Phase,
    next_warm_at: u64,
    extension_override: bool,
    replayable: bool,
    spent: f64,
    controller: pa_agent::abort::AbortController,
}

/// Idle warming's horizon: a decision past it, or a refresh scheduled past it, stops warming.
fn idle_stop_reason(run: &Run, now: u64) -> Option<&'static str> {
    let deadline = run.started_at + MAX_IDLE_WARMING_AGE_MS;
    if now >= deadline {
        return Some("30-minute idle safety limit reached");
    }
    if run.next_warm_at <= deadline {
        return None;
    }
    Some(if run.delay_ms >= MAX_IDLE_WARMING_AGE_MS {
        "cache entry outlives the 30-minute idle window"
    } else {
        "30-minute idle safety limit reached"
    })
}

/// Why the run must stop whatever the schedule says, from the live mode and its phase.
fn mode_stop_reason(mode: Mode, run: &Run) -> Option<&'static str> {
    match mode {
        Mode::Off => Some("cache warming disabled"),
        Mode::Streaming if run.phase == Phase::Idle => Some("agent run settled"),
        Mode::Streaming | Mode::Idle => None,
    }
}

/// What the driver should do after a decision was taken and possibly overridden.
enum Instruction {
    /// Send this refresh, then report its outcome with [`Warmer::on_refresh_result`].
    Refresh {
        request: Arc<Request>,
        method: CacheWarmingRentMethod,
        run_id: u64,
        signal: pa_agent::abort::AbortSignal,
    },
    /// Nothing to do: the run stopped, or its timer was re-armed.
    Idle,
}

/// Outcome of one refresh, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The entry was read from the cache: its lifetime restarts at the send.
    Hit,
    /// The refresh re-wrote the prefix, or read nothing.
    Miss,
    Error,
    /// The run was replaced before the provider answered.
    Aborted,
}

struct State {
    run: Option<Run>,
    entry: Option<TrackedEntry>,
    /// The entry the in-flight request will write, published once its response confirms it.
    pending: Option<TrackedEntry>,
    /// Billed output of this session's replays per model: the running mean prices the next replay.
    replay_output: HashMap<String, (u64, u64)>,
    inactive: Status,
    prompt_tokens: u64,
    next_run_id: u64,
}

#[derive(Debug, Clone)]
struct TrackedEntry {
    model_key: String,
    touched_at: u64,
    prompt_tokens: u64,
}

/// Keeps one prompt cache entry alive by refreshing its request before the entry expires.
pub struct Warmer {
    state: Mutex<State>,
    emitter: Option<Arc<dyn DecisionEmitter>>,
    /// This warmer's mode: the session default at install time, then whatever settings say.
    mode: AtomicU8,
    driving: AtomicBool,
    /// Bumped whenever the schedule changes, so a parked driver re-reads it at once.
    pulse: tokio::sync::watch::Sender<u64>,
    pulse_rx: tokio::sync::watch::Receiver<u64>,
}

impl std::fmt::Debug for Warmer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Warmer")
            .field("mode", &self.mode())
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Warmer {
    /// A warmer that starts in `off`: nothing is warmed until the session's settings say so.
    #[must_use]
    pub fn new(emitter: Option<Arc<dyn DecisionEmitter>>) -> Arc<Self> {
        let (pulse, pulse_rx) = tokio::sync::watch::channel(0);
        Arc::new(Self {
            state: Mutex::new(State {
                run: None,
                entry: None,
                pending: None,
                replay_output: HashMap::new(),
                inactive: Status::inactive("waiting for first request"),
                prompt_tokens: 0,
                next_run_id: 1,
            }),
            emitter,
            mode: AtomicU8::new(mode().wire()),
            driving: AtomicBool::new(false),
            pulse,
            pulse_rx,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::from_wire(self.mode.load(Ordering::Relaxed))
    }

    /// Change this warmer's mode and reconcile an active run (`onModeChanged`).
    pub fn set_mode(&self, next: Mode) {
        self.mode.store(next.wire(), Ordering::Relaxed);
        let mut st = self.lock();
        let Some(run) = st.run.as_ref() else { return };
        let reason = mode_stop_reason(next, run);
        if let Some(reason) = reason {
            stop(&mut st, reason);
        }
    }

    /// Keep the prompt cache entry written by `request` warm. Replaces any previous run.
    pub fn start(self: &Arc<Self>, request: Arc<Request>, cache: Option<(Tier, u64)>, now: u64) {
        let mut st = self.lock();
        if let Some(old) = st.run.take() {
            old.controller.abort();
        }
        st.pending = None;
        if self.mode() == Mode::Off {
            st.inactive = Status::inactive("cache warming disabled");
            return;
        }
        let Some((tier, ttl_ms)) = cache else {
            st.inactive = Status::inactive("request disabled prompt caching");
            return;
        };
        let Some(delay_ms) = get_cache_warming_delay_ms(ttl_ms) else {
            st.inactive = Status::inactive("cache lifetime unavailable");
            return;
        };
        let replayable = is_replayable(&request.model, &request.options);
        let id = st.next_run_id;
        st.next_run_id += 1;
        st.pending = Some(TrackedEntry {
            model_key: model_key(&request.model),
            touched_at: now,
            prompt_tokens: 0,
        });
        let mut run = Run {
            id,
            request,
            tier,
            delay_ms,
            started_at: now,
            touched_at: now,
            phase: Phase::Streaming,
            next_warm_at: 0,
            extension_override: false,
            replayable,
            spent: 0.0,
            controller: pa_agent::abort::AbortController::new(),
        };
        if schedule(&mut st, &mut run, now) {
            st.run = Some(run);
        }
        drop(st);
        self.ensure_driver();
        self.pulse();
    }

    /// Confirm the in-flight request's cache entry from its response (`onResponse`). A failed
    /// response, or one that neither read nor wrote the cache, never publishes its entry.
    pub fn on_response(&self, message: &AssistantMessage, _now: u64) {
        let mut st = self.lock();
        if message.stop_reason != StopReason::Error {
            st.prompt_tokens = prompt_tokens_of(&message.usage);
        }
        let Some(pending) = st.pending.take() else {
            return;
        };
        if message.stop_reason == StopReason::Error
            || model_key_of(message) != pending.model_key
            || (message.usage.cache_read == 0 && message.usage.cache_write == 0)
        {
            return;
        }
        let mut entry = pending;
        entry.prompt_tokens = prompt_tokens_of(&message.usage);
        st.entry = Some(entry);
    }

    /// The agent run settled: `streaming` stops, `idle` takes over the schedule.
    pub fn on_agent_settled(&self, now: u64) {
        let mut st = self.lock();
        let Some(run) = st.run.as_mut() else { return };
        if self.mode() == Mode::Streaming {
            stop(&mut st, "agent run settled");
            return;
        }
        run.phase = Phase::Idle;
        let reason = idle_stop_reason(run, now);
        if let Some(reason) = reason {
            stop(&mut st, reason);
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        let mut st = self.lock();
        if self.mode() == Mode::Off {
            return Status::inactive("cache warming disabled");
        }
        let Some(run) = st.run.take() else {
            return st.inactive.clone();
        };
        let decision = evaluate(&st, &run, run.next_warm_at);
        let status = Status {
            scheduled: true,
            reason: None,
            next_warm_at: Some(run.next_warm_at),
            decision: Some(decision),
            extension_override: run.extension_override,
        };
        st.run = Some(run);
        status
    }

    /// When the driver should wake; `None` while nothing is armed.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        self.lock().run.as_ref().map(|run| run.next_warm_at)
    }

    /// Take the decision due at `now` (`#decideAt`), or `None` when no run is armed.
    fn decide_at(&self, now: u64) -> Option<Evaluation> {
        let mut st = self.lock();
        let run = st.run.take()?;
        if let Some(reason) = mode_stop_reason(self.mode(), &run) {
            run.controller.abort();
            stop(&mut st, reason);
            return None;
        }
        if run.phase == Phase::Idle {
            if let Some(reason) = idle_stop_reason(&run, now) {
                run.controller.abort();
                stop(&mut st, reason);
                return None;
            }
        }
        let decision = evaluate(&st, &run, run.next_warm_at);
        let evaluation = Evaluation {
            run_id: run.id,
            decision,
            stop_reason: stop_reason(&decision),
        };
        st.run = Some(run);
        Some(evaluation)
    }

    /// Apply the decision, and an extension's override, at `now`.
    fn apply(
        &self,
        now: u64,
        evaluation: &Evaluation,
        override_action: Option<CacheWarmingAction>,
    ) -> Instruction {
        let mut st = self.lock();
        let Some(mut run) = st.run.take() else {
            return Instruction::Idle;
        };
        if run.id != evaluation.run_id {
            st.run = Some(run);
            return Instruction::Idle;
        }
        if let Some(reason) = mode_stop_reason(self.mode(), &run) {
            run.controller.abort();
            stop(&mut st, reason);
            return Instruction::Idle;
        }
        let action = resolve_override(&evaluation.decision, false, override_action);
        let extension_override = action != evaluation.decision.action;
        if action == CacheWarmingAction::Stop {
            run.controller.abort();
            let reason = if extension_override {
                "stopped by extension"
            } else {
                evaluation.stop_reason
            };
            stop_with(
                &mut st,
                reason,
                Some(evaluation.decision),
                extension_override,
            );
            return Instruction::Idle;
        }
        if action == CacheWarmingAction::Compact {
            run.controller.abort();
            stop_with(
                &mut st,
                "bridge compaction unavailable",
                Some(evaluation.decision),
                extension_override,
            );
            return Instruction::Idle;
        }
        let Some(method) = evaluation.decision.method else {
            run.controller.abort();
            stop_with(
                &mut st,
                "request cannot be replayed safely",
                Some(evaluation.decision),
                extension_override,
            );
            return Instruction::Idle;
        };
        run.extension_override = extension_override;
        let signal = run.controller.signal();
        let instruction = Instruction::Refresh {
            request: Arc::clone(&run.request),
            method,
            run_id: run.id,
            signal,
        };
        let _ = now;
        st.run = Some(run);
        instruction
    }

    /// Settle the run on a refresh outcome (`#send`).
    pub fn on_refresh_result(
        &self,
        run_id: u64,
        method: CacheWarmingRentMethod,
        outcome: RefreshOutcome,
        message: Option<&AssistantMessage>,
        now: u64,
    ) {
        let mut st = self.lock();
        let Some(current) = st.run.as_ref() else {
            return;
        };
        if current.id != run_id {
            return;
        }
        let key = model_key(&current.request.model);
        if let Some(usage) = message
            .map(|m| m.usage.clone())
            .filter(|u| u.total_tokens > 0)
        {
            if let Some(run) = st.run.as_mut() {
                run.spent += usage.cost.total;
            }
            if method == CacheWarmingRentMethod::Replay {
                let observed = st.replay_output.entry(key).or_insert((0, 0));
                observed.0 += 1;
                observed.1 += usage.output;
            }
        }
        match outcome {
            RefreshOutcome::Aborted => {}
            RefreshOutcome::Error => stop(&mut st, "refresh failed"),
            RefreshOutcome::Miss => {
                let wrote = message.is_some_and(|m| m.usage.cache_write > 0);
                if wrote {
                    // What it wrote, the next request reads: a new lifetime from the refresh's send.
                    if let Some(run) = st.run.as_mut() {
                        run.touched_at = now;
                    }
                    if let Some(entry) = st.entry.as_mut() {
                        entry.touched_at = now;
                    }
                }
                stop(&mut st, "refresh missed the cache");
            }
            RefreshOutcome::Hit => {
                let Some(mut run) = st.run.take() else { return };
                run.touched_at = now;
                if let Some(entry) = st.entry.as_mut() {
                    entry.touched_at = now;
                }
                if schedule(&mut st, &mut run, now) {
                    st.run = Some(run);
                }
                drop(st);
                self.pulse();
            }
        }
    }

    /// One driver step: take the due decision, ask extensions, act.
    pub async fn step_at(self: &Arc<Self>, now: u64) {
        let Some(evaluation) = self.decide_at(now) else {
            return;
        };
        let override_action = match &self.emitter {
            Some(emitter) if emitter.has_handlers() => {
                let decide = emitter.decide(evaluation.decision.event());
                match tokio::time::timeout(DECIDE_TIMEOUT, decide).await {
                    // A slow or failing extension leaves the warmer's own decision standing.
                    Ok(outcome) => outcome.action,
                    Err(_) => None,
                }
            }
            _ => None,
        };
        if let Instruction::Refresh {
            request,
            method,
            run_id,
            signal,
        } = self.apply(now, &evaluation, override_action)
        {
            let (outcome, message) = send_refresh(&request, method, signal).await;
            self.on_refresh_result(run_id, method, outcome, message.as_ref(), now_ms());
        }
    }

    fn pulse(&self) {
        self.pulse.send_modify(|tick| *tick = tick.wrapping_add(1));
    }

    /// Start the driver on the first warmed request: it always runs inside the session's runtime.
    fn ensure_driver(self: &Arc<Self>) {
        if self.driving.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.driving.store(false, Ordering::SeqCst);
            return;
        };
        let warmer = Arc::downgrade(self);
        handle.spawn(drive(warmer));
    }
}

/// Drive decisions on their own schedule until the warmer is dropped.
async fn drive(warmer: Weak<Warmer>) {
    let mut pulse = match warmer.upgrade() {
        Some(warmer) => warmer.pulse_rx.clone(),
        None => return,
    };
    loop {
        let Some(current) = warmer.upgrade() else {
            return;
        };
        let wait = match current.next_deadline() {
            Some(at) => at.saturating_sub(now_ms()),
            // Nothing armed: sleep out the idle window and re-check, so a dropped warmer's task ends.
            None => MAX_IDLE_WARMING_AGE_MS,
        };
        drop(current);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(wait)) => {}
            result = pulse.changed() => {
                if result.is_err() {
                    return;
                }
                continue;
            }
        }
        let Some(current) = warmer.upgrade() else {
            return;
        };
        current.step_at(now_ms()).await;
    }
}

/// `#decide`'s override rule: an override the request cannot carry out leaves the warmer's own
/// decision standing. `warm` needs a `method`; `compact` needs a `compactCost` and a decision at
/// least a minute before the entry expires; `stop` always applies.
#[must_use]
pub fn resolve_override(
    decision: &Decision,
    compact_now: bool,
    override_action: Option<CacheWarmingAction>,
) -> CacheWarmingAction {
    let Some(action) = override_action else {
        return decision.action;
    };
    match action {
        CacheWarmingAction::Warm if decision.method.is_none() => decision.action,
        CacheWarmingAction::Compact if !compact_now || decision.compact_cost.is_none() => {
            decision.action
        }
        CacheWarmingAction::Warm | CacheWarmingAction::Compact | CacheWarmingAction::Stop => action,
    }
}

fn model_key_of(message: &AssistantMessage) -> String {
    format!("{}/{}", message.provider, message.model)
}

/// Stop the run, keeping the decision behind the stop for the status surface.
fn stop_with(
    st: &mut State,
    reason: impl Into<String>,
    decision: Option<Decision>,
    extension_override: bool,
) {
    let reason = reason.into();
    if let Some(run) = st.run.take() {
        run.controller.abort();
    }
    st.inactive = Status {
        scheduled: false,
        reason: Some(reason),
        next_warm_at: None,
        decision,
        extension_override,
    };
}

fn stop(st: &mut State, reason: impl Into<String>) {
    stop_with(st, reason, None, false);
}

/// Arm the next decision (`#schedule`); false when the run stopped instead.
fn schedule(st: &mut State, run: &mut Run, now: u64) -> bool {
    run.extension_override = false;
    run.next_warm_at = now + run.delay_ms;
    if run.phase == Phase::Idle {
        if let Some(reason) = idle_stop_reason(run, now) {
            stop(st, reason);
            return false;
        }
    } else if !run.replayable {
        stop(st, "request cannot be replayed safely");
        return false;
    }
    true
}

/// Billed output a replay of `model` is expected to cost: this session's running mean, before that
/// the upstream prior.
fn expected_replay_output(st: &State, model: &Model) -> u64 {
    match st.replay_output.get(&model_key(model)) {
        Some((replays, tokens)) if *replays > 0 => tokens / replays,
        _ => REPLAY_OUTPUT_PRIOR_TOKENS,
    }
}

/// Prices the decision taken at `decision_at` and makes it (`#evaluate` + `#act`).
fn evaluate(st: &State, run: &Run, decision_at: u64) -> Decision {
    let model = &run.request.model;
    let prompt_tokens = st.prompt_tokens;
    let cache_hit_cost = price(prompt_tokens, model.cost.cache_read);
    let cache_miss_cost = cache_write_price(model, run.tier, prompt_tokens);
    let method = run.replayable.then_some(CacheWarmingRentMethod::Replay);
    let warm_cost = match method {
        None => 0.0,
        // A pre-warm reads the cache without generating; a replay is billed for its output too.
        Some(CacheWarmingRentMethod::Prewarm) => cache_hit_cost,
        Some(CacheWarmingRentMethod::Replay) => {
            cache_hit_cost + price(expected_replay_output(st, model), model.cost.output)
        }
    };
    let miss_cost = (cache_miss_cost - cache_hit_cost).max(0.0);
    let continuation_probability = if run.phase == Phase::Idle {
        IDLE_CONTINUATION_PROBABILITY
    } else {
        1.0
    };
    let economics_available = prompt_tokens > 0 && (cache_hit_cost > 0.0 || cache_miss_cost > 0.0);
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    // No host compactor is wired, so bridging a wait is never priced at this decision.
    let compact_cost: Option<f64> = None;
    let mut decision = Decision {
        phase: run.phase,
        method,
        warm_cost,
        miss_cost,
        compact_cost,
        spent_cost: run.spent,
        continuation_probability,
        expected_savings,
        economics_available,
        action: CacheWarmingAction::Stop,
    };
    let _ = decision_at;
    decision.action = act(&decision);
    decision
}

/// Stop reason behind a priced decision (`#evaluate`'s `stopReason`).
fn stop_reason(decision: &Decision) -> &'static str {
    if !decision.economics_available {
        "cache economics unavailable"
    } else if decision.method.is_none() {
        "request cannot be replayed safely"
    } else if decision.expected_savings < MINIMUM_EXPECTED_SAVINGS {
        "expected savings below threshold"
    } else {
        "refresh spend reached the cost of losing the cache"
    }
}

/// The warmer's action on a priced decision (`#act`): idle refreshes while the expected savings
/// clear the floor; streaming (ski rental) refreshes while the spend so far plus this refresh stays
/// within the cheapest buy-out, else stops.
fn act(decision: &Decision) -> CacheWarmingAction {
    if !decision.economics_available {
        return CacheWarmingAction::Stop;
    }
    let rent_pays =
        decision.method.is_some() && decision.expected_savings >= MINIMUM_EXPECTED_SAVINGS;
    if decision.phase == Phase::Idle {
        return if rent_pays {
            CacheWarmingAction::Warm
        } else {
            CacheWarmingAction::Stop
        };
    }
    let buy_cost = decision
        .compact_cost
        .map_or(decision.miss_cost, |cost| cost.min(decision.miss_cost));
    if rent_pays && decision.spent_cost + decision.warm_cost <= buy_cost {
        return CacheWarmingAction::Warm;
    }
    if let Some(cost) = decision.compact_cost {
        if decision.miss_cost - cost >= MINIMUM_EXPECTED_SAVINGS && rent_pays {
            return CacheWarmingAction::Warm;
        }
    }
    CacheWarmingAction::Stop
}

/// Whether `event` starts generating output: the replay's cutoff point.
fn is_generation_event(event: &AssistantMessageEvent) -> bool {
    match event {
        AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ToolCallStart { .. } => true,
        AssistantMessageEvent::TextDelta { delta, .. }
        | AssistantMessageEvent::ThinkingDelta { delta, .. }
        | AssistantMessageEvent::ToolCallDelta { delta, .. } => !delta.is_empty(),
        _ => false,
    }
}

fn into_partial(event: AssistantMessageEvent) -> Option<AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// Send one refresh (`#replay`): a one-token cap, cut off at its first generated block.
async fn send_refresh(
    request: &Request,
    method: CacheWarmingRentMethod,
    signal: pa_agent::abort::AbortSignal,
) -> (RefreshOutcome, Option<AssistantMessage>) {
    debug_assert_eq!(
        method,
        CacheWarmingRentMethod::Replay,
        "prewarm is not wired"
    );
    let mut options = request.options.clone();
    options.max_tokens = Some(1);
    // The turn's own signal dies with the turn; a refresh outlives it and follows the run instead.
    options.signal = signal;
    let mut stream =
        match (request.base)(request.model.clone(), request.context.clone(), options).await {
            Ok(stream) => stream,
            Err(_) => return (RefreshOutcome::Error, None),
        };
    let mut partial: Option<AssistantMessage> = None;
    while let Some(event) = stream.next_event().await {
        match event {
            AssistantMessageEvent::Done { message, .. } => {
                return (cache_outcome(&message), Some(message));
            }
            AssistantMessageEvent::Error { error, .. } => {
                let outcome = if error.stop_reason == StopReason::Aborted {
                    RefreshOutcome::Aborted
                } else {
                    RefreshOutcome::Error
                };
                return (outcome, Some(error));
            }
            event => {
                let generated = is_generation_event(&event);
                partial = into_partial(event).or(partial);
                if generated {
                    stream.close();
                    break;
                }
            }
        }
    }
    let _ = stream.result().await;
    match partial {
        Some(message) => (cache_outcome(&message), Some(message)),
        None => (RefreshOutcome::Error, None),
    }
}

/// A refresh that read nothing from the cache missed it: the entry was already gone.
fn cache_outcome(message: &AssistantMessage) -> RefreshOutcome {
    if message.usage.cache_read > 0 {
        RefreshOutcome::Hit
    } else {
        RefreshOutcome::Miss
    }
}

/// Settle the session's run at `AgentEnd`: `streaming` stops there, `idle` takes over the
/// schedule. The subscription must outlive the session (dropping it unsubscribes).
pub async fn watch_settle(agent: &pa_agent::agent::Agent) -> Option<pa_agent::agent::Subscription> {
    let warmer = current()?;
    Some(
        agent
            .subscribe(move |event, _| {
                if matches!(event, pa_agent::types::AgentEvent::AgentEnd { .. }) {
                    warmer.on_agent_settled(now_ms());
                }
                Box::pin(async { Ok(()) })
            })
            .await,
    )
}

/// Milliseconds since the Unix epoch: the clock the schedule counts with.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Wrap the real provider seam so every request it sends is the one kept warm: the entry's lifetime
/// counts from the send, and its response publishes the entry. `base` is the seam below the warmer,
/// so a refresh never re-enters this wrapper. The wrapper owns the warmer, which is what keeps it
/// alive for the session.
pub fn warm_stream(base: StreamFn, target: crate::agent::Target, warmer: Arc<Warmer>) -> StreamFn {
    Arc::new(move |model, context, options| {
        let base = base.clone();
        let target = target.clone();
        let warmer = Arc::clone(&warmer);
        Box::pin(async move {
            if warmer.mode() == Mode::Off {
                return base(model, context, options).await;
            }
            let cache = target
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .and_then(|t| prompt_cache_ttl(&t.model, t.api_key.as_deref()));
            warmer.start(
                Arc::new(Request {
                    base: base.clone(),
                    model: model.clone(),
                    context: context.clone(),
                    options: options.clone(),
                }),
                cache,
                now_ms(),
            );
            let stream = base(model, context, options).await?;
            Ok(Box::new(ResponseTap {
                inner: stream,
                warmer,
            }) as Box<dyn ModelStream>)
        })
    })
}

/// Publishes the entry a real response wrote.
struct ResponseTap {
    inner: Box<dyn ModelStream>,
    warmer: Arc<Warmer>,
}

impl ModelStream for ResponseTap {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(async move {
            let event = self.inner.next_event().await;
            if let Some(message) = event
                .as_ref()
                .and_then(AssistantMessageEvent::terminal_message)
            {
                self.warmer.on_response(message, now_ms());
            }
            event
        })
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        self.inner.result()
    }

    fn close(&mut self) {
        self.inner.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_700_000_000_000;
    const SHORT_TTL: u64 = 300_000;
    const LONG_TTL: u64 = 3_600_000;

    fn model(api: &str, provider: &str, id: &str, input: f64, write: f64, read: f64) -> Model {
        serde_json::from_value(json!({
            "id": id, "name": id, "api": api, "provider": provider,
            "baseUrl": "http://127.0.0.1:9", "reasoning": false,
            "cost": {"input": input, "output": 15.0, "cacheRead": read, "cacheWrite": write},
            "contextWindow": 200_000, "maxTokens": 8192
        }))
        .expect("model json")
    }

    /// The model list prices: $3 in, $15 out, $0.30 cache read, $3.75 five-minute write.
    fn claude() -> Model {
        model("anthropic-messages", "anthropic", "", 3.0, 3.75, 0.3)
    }

    fn pa_claude() -> pa_types::ai::Model {
        serde_json::from_value(json!({
            "id": "claude-x", "name": "c", "api": "anthropic-messages", "provider": "anthropic",
            "baseUrl": "http://127.0.0.1:9", "reasoning": false, "input": ["text"],
            "cost": {"input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75},
            "contextWindow": 200_000, "maxTokens": 8192
        }))
        .expect("pa model json")
    }

    fn message(
        model: &Model,
        stop_reason: &str,
        cache_read: u64,
        cache_write: u64,
        output: u64,
        total_cost: f64,
    ) -> AssistantMessage {
        serde_json::from_value(json!({
            "content": [{"type": "text", "text": "ok"}],
            "api": model.api, "provider": model.provider, "model": model.id,
            "usage": {
                "input": 0, "output": output, "cacheRead": cache_read, "cacheWrite": cache_write,
                "totalTokens": cache_read + cache_write + output,
                "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": total_cost}
            },
            "stopReason": stop_reason, "timestamp": 0
        }))
        .expect("assistant json")
    }

    /// A provider seam that never answers: pure-decision tests must not reach it.
    fn unreachable_base() -> StreamFn {
        Arc::new(|_, _, _| Box::pin(async { anyhow::bail!("the warmer must not send here") }))
    }

    fn request(model: Model) -> Arc<Request> {
        Arc::new(Request {
            base: unreachable_base(),
            model,
            context: LlmContext::default(),
            options: StreamRequestOptions::default(),
        })
    }

    /// A warmer in `mode` whose request wrote a 100k-token entry the provider reported.
    fn warmed(mode: Mode, model: Model, tier: Tier, ttl: u64) -> Arc<Warmer> {
        let warmer = Warmer::new(None);
        warmer.set_mode(mode);
        warmer.start(request(model.clone()), Some((tier, ttl)), NOW);
        warmer.on_response(&message(&model, "stop", 100_000, 0, 20, 0.03), NOW);
        warmer
    }

    #[test]
    fn delay_keeps_ten_seconds_of_margin_and_floors_at_one_millisecond() {
        assert_eq!(get_cache_warming_delay_ms(SHORT_TTL), Some(270_000));
        assert_eq!(get_cache_warming_delay_ms(LONG_TTL), Some(3_240_000));
        assert_eq!(get_cache_warming_delay_ms(10_001), Some(1));
        assert_eq!(get_cache_warming_delay_ms(10_000), None);
        assert_eq!(get_cache_warming_delay_ms(0), None);
    }

    #[test]
    fn an_unknown_mode_is_off() {
        assert_eq!(Mode::parse("idle"), Mode::Idle);
        assert_eq!(Mode::parse("streaming"), Mode::Streaming);
        assert_eq!(Mode::parse("nonsense"), Mode::Off);
    }

    #[test]
    fn only_anthropic_models_have_a_lifetime_this_core_can_name() {
        assert_eq!(
            prompt_cache_ttl(&pa_claude(), None),
            Some((Tier::Short, SHORT_TTL))
        );
        let openai = model("openai-responses", "openai", "gpt-5", 1.0, 0.0, 0.1);
        assert_eq!(prompt_cache_ttl(&pa_claude_of(&openai), None), None);
    }

    /// The same catalog row in the pa-types shape, so one JSON builds both sides.
    fn pa_claude_of(model: &Model) -> pa_types::ai::Model {
        serde_json::from_value(json!({
            "id": model.id, "name": model.name, "api": model.api, "provider": model.provider,
            "baseUrl": "http://127.0.0.1:9", "reasoning": false, "input": ["text"],
            "cost": {"input": model.cost.input, "output": model.cost.output,
                     "cacheRead": model.cost.cache_read, "cacheWrite": model.cost.cache_write},
            "contextWindow": 200_000, "maxTokens": 8192
        }))
        .expect("pa model json")
    }

    #[test]
    fn an_oauth_seat_writes_one_hour_entries_where_the_model_supports_them() {
        let mut model = pa_claude();
        model.compat = Some(
            serde_json::from_value(json!({"supportsLongCacheRetention": true}))
                .expect("compat json"),
        );
        assert_eq!(
            prompt_cache_ttl(&model, Some("sk-ant-oat-123")),
            Some((Tier::Long, LONG_TTL))
        );
        // An api key on the same model stays on the cheap tier.
        assert_eq!(
            prompt_cache_ttl(&model, Some("sk-ant-api-123")),
            Some((Tier::Short, SHORT_TTL))
        );
    }

    #[test]
    fn a_reasoning_budget_that_follows_max_tokens_makes_the_request_unreplayable() {
        let mut model = claude();
        model.reasoning = true;
        let mut opts = StreamRequestOptions {
            reasoning: ThinkingLevel::High,
            ..StreamRequestOptions::default()
        };
        assert!(!is_replayable(&model, &opts));
        // No reasoning requested: the output cap cannot move the budget.
        opts.reasoning = ThinkingLevel::Off;
        assert!(is_replayable(&model, &opts));
        // Another provider's request is always replayable.
        let mut other = model_of("openai-responses", "openai", "gpt-5");
        other.reasoning = true;
        assert!(is_replayable(&other, &opts));
    }

    fn model_of(api: &str, provider: &str, id: &str) -> Model {
        model(api, provider, id, 1.0, 0.0, 0.1)
    }

    #[test]
    fn a_cheap_refresh_pays_while_streaming() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        let decision = warmer
            .decide_at(NOW + 270_000)
            .expect("a decision is due")
            .decision;
        assert_eq!(decision.method, Some(CacheWarmingRentMethod::Replay));
        assert_eq!(decision.continuation_probability, 1.0);
        // 100k cached tokens: $0.03 to read, $0.375 to rewrite, so $0.345 to lose.
        assert!((decision.miss_cost - 0.345).abs() < 1e-9, "{decision:?}");
        // A 512-token output prior at $15/M is $0.00768 on top of the cache read.
        assert!((decision.warm_cost - 0.037_68).abs() < 1e-9, "{decision:?}");
        assert_eq!(decision.action, CacheWarmingAction::Warm);
    }

    #[test]
    fn a_prompt_too_small_to_matter_stops() {
        let model = claude();
        let warmer = Warmer::new(None);
        warmer.set_mode(Mode::Streaming);
        warmer.start(request(model.clone()), Some((Tier::Short, SHORT_TTL)), NOW);
        // 1k tokens: the whole entry is worth $0.00345, below the $0.05 floor.
        warmer.on_response(&message(&model, "stop", 1_000, 0, 20, 0.0), NOW);
        let decision = warmer
            .decide_at(NOW + 270_000)
            .expect("a decision is due")
            .decision;
        assert!(decision.economics_available);
        assert!(decision.expected_savings < MINIMUM_EXPECTED_SAVINGS);
        assert_eq!(decision.action, CacheWarmingAction::Stop);
    }

    #[test]
    fn an_unknown_prompt_size_is_not_priced_at_all() {
        let warmer = Warmer::new(None);
        warmer.set_mode(Mode::Streaming);
        warmer.start(request(claude()), Some((Tier::Short, SHORT_TTL)), NOW);
        let decision = warmer
            .decide_at(NOW + 270_000)
            .expect("a decision is due")
            .decision;
        assert!(!decision.economics_available);
        assert_eq!(decision.action, CacheWarmingAction::Stop);
    }

    #[test]
    fn ski_rental_stops_once_the_spend_reaches_the_cost_of_losing_the_cache() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        let mut decision = warmer.decide_at(NOW + 270_000).expect("due").decision;
        assert_eq!(decision.action, CacheWarmingAction::Warm);
        // Refreshes at $0.03768 each: $0.34 spent already exceeds the $0.345 rewrite.
        decision.spent_cost = 0.34;
        assert_eq!(act(&decision), CacheWarmingAction::Stop);
        decision.spent_cost = 0.30;
        assert_eq!(act(&decision), CacheWarmingAction::Warm);
    }

    #[test]
    fn idle_warming_needs_the_continuation_odds_to_clear_the_floor() {
        let warmer = warmed(Mode::Idle, claude(), Tier::Short, SHORT_TTL);
        warmer.on_agent_settled(NOW + 1_000);
        let decision = warmer
            .decide_at(NOW + 270_000)
            .expect("idle warming keeps its schedule")
            .decision;
        assert_eq!(decision.phase, Phase::Idle);
        assert_eq!(
            decision.continuation_probability,
            IDLE_CONTINUATION_PROBABILITY
        );
        // 0.15 * $0.345 = $0.0518, under the $0.05 floor plus the $0.0377 refresh.
        assert!(
            decision.expected_savings < MINIMUM_EXPECTED_SAVINGS,
            "{decision:?}"
        );
        assert_eq!(decision.action, CacheWarmingAction::Stop);
    }

    #[test]
    fn streaming_mode_stops_when_the_agent_run_settles() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        warmer.on_agent_settled(NOW + 1_000);
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(status.reason.as_deref(), Some("agent run settled"));
        assert!(warmer.decide_at(NOW + 270_000).is_none());
    }

    #[test]
    fn idle_warming_ends_at_its_thirty_minute_horizon() {
        let warmer = warmed(Mode::Idle, claude(), Tier::Short, SHORT_TTL);
        warmer.on_agent_settled(NOW + 1_000);
        assert!(warmer
            .decide_at(NOW + MAX_IDLE_WARMING_AGE_MS - 1)
            .is_some());
        assert!(warmer.decide_at(NOW + MAX_IDLE_WARMING_AGE_MS).is_none());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("30-minute idle safety limit reached")
        );
    }

    #[test]
    fn a_one_hour_entry_outlives_the_idle_window() {
        let warmer = warmed(Mode::Idle, claude(), Tier::Long, LONG_TTL);
        warmer.on_agent_settled(NOW + 1_000);
        assert!(warmer.decide_at(NOW + 60_000).is_none());
        assert_eq!(
            warmer.status().reason.as_deref(),
            Some("cache entry outlives the 30-minute idle window")
        );
    }

    #[test]
    fn a_refresh_hit_restarts_the_lifetime_and_bills_the_spend() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        let evaluation = warmer.decide_at(NOW + 270_000).expect("due");
        let Instruction::Refresh { method, run_id, .. } =
            warmer.apply(NOW + 270_000, &evaluation, None)
        else {
            panic!("a paying refresh is sent");
        };
        let billed = message(&claude(), "stop", 100_000, 0, 1, 0.031);
        warmer.on_refresh_result(
            run_id,
            method,
            RefreshOutcome::Hit,
            Some(&billed),
            NOW + 270_100,
        );
        let status = warmer.status();
        assert!(status.scheduled);
        assert_eq!(status.next_warm_at, Some(NOW + 270_100 + 270_000));
        let decision = status.decision.expect("the next decision is priced");
        assert!((decision.spent_cost - 0.031).abs() < 1e-9, "{decision:?}");
    }

    #[test]
    fn a_refresh_miss_stops_warming() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        let evaluation = warmer.decide_at(NOW + 270_000).expect("due");
        let Instruction::Refresh { method, run_id, .. } =
            warmer.apply(NOW + 270_000, &evaluation, None)
        else {
            panic!("a paying refresh is sent");
        };
        warmer.on_refresh_result(run_id, method, RefreshOutcome::Miss, None, NOW + 270_100);
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(status.reason.as_deref(), Some("refresh missed the cache"));
    }

    fn priced(action: CacheWarmingAction, method: Option<CacheWarmingRentMethod>) -> Decision {
        Decision {
            phase: Phase::Streaming,
            method,
            warm_cost: 0.04,
            miss_cost: 0.35,
            compact_cost: None,
            spent_cost: 0.0,
            continuation_probability: 1.0,
            expected_savings: 0.31,
            economics_available: true,
            action,
        }
    }

    #[test]
    fn an_override_the_request_cannot_carry_out_leaves_the_warmer_decision_standing() {
        // `warm` without a method is refused.
        let unreplayable = priced(CacheWarmingAction::Stop, None);
        assert_eq!(
            resolve_override(&unreplayable, false, Some(CacheWarmingAction::Warm)),
            CacheWarmingAction::Stop
        );
        // `compact` without a priced bridge is refused, at a deadline or not.
        assert_eq!(
            resolve_override(&unreplayable, true, Some(CacheWarmingAction::Compact)),
            CacheWarmingAction::Stop
        );
        // A carryable `warm` overrides a stop, and `stop` always applies.
        let replayable = priced(
            CacheWarmingAction::Stop,
            Some(CacheWarmingRentMethod::Replay),
        );
        assert_eq!(
            resolve_override(&replayable, false, Some(CacheWarmingAction::Warm)),
            CacheWarmingAction::Warm
        );
        assert_eq!(
            resolve_override(&replayable, false, Some(CacheWarmingAction::Stop)),
            CacheWarmingAction::Stop
        );
        // A priced bridge is carried out only at its deadline.
        let mut bridged = priced(
            CacheWarmingAction::Stop,
            Some(CacheWarmingRentMethod::Replay),
        );
        bridged.compact_cost = Some(0.2);
        assert_eq!(
            resolve_override(&bridged, false, Some(CacheWarmingAction::Compact)),
            CacheWarmingAction::Stop
        );
        assert_eq!(
            resolve_override(&bridged, true, Some(CacheWarmingAction::Compact)),
            CacheWarmingAction::Compact
        );
        // No override keeps the warmer's own action.
        assert_eq!(
            resolve_override(&replayable, false, None),
            CacheWarmingAction::Stop
        );
    }

    #[test]
    fn the_payload_carries_every_priced_field() {
        let warmer = warmed(Mode::Streaming, claude(), Tier::Short, SHORT_TTL);
        let decision = warmer.decide_at(NOW + 270_000).expect("due").decision;
        let event = decision.event();
        assert_eq!(event.action, decision.action);
        assert_eq!(event.method, decision.method);
        assert_eq!(event.warm_cost, decision.warm_cost);
        assert_eq!(event.miss_cost, decision.miss_cost);
        assert_eq!(event.compact_cost, None);
        assert_eq!(event.spent_cost, 0.0);
        assert_eq!(event.continuation_probability, 1.0);
    }

    #[test]
    fn warming_off_never_arms() {
        let warmer = Warmer::new(None);
        warmer.set_mode(Mode::Off);
        warmer.start(request(claude()), Some((Tier::Short, SHORT_TTL)), NOW);
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(status.reason.as_deref(), Some("cache warming disabled"));
    }

    #[test]
    fn a_request_that_cannot_be_replayed_never_arms() {
        let mut model = claude();
        model.reasoning = true;
        let opts = StreamRequestOptions {
            reasoning: ThinkingLevel::High,
            ..StreamRequestOptions::default()
        };
        let warmer = Warmer::new(None);
        warmer.set_mode(Mode::Streaming);
        warmer.start(
            Arc::new(Request {
                base: unreachable_base(),
                model,
                context: LlmContext::default(),
                options: opts,
            }),
            Some((Tier::Short, SHORT_TTL)),
            NOW,
        );
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(
            status.reason.as_deref(),
            Some("request cannot be replayed safely")
        );
    }

    /// Counts the decisions an extension was asked to judge.
    struct CountingEmitter {
        handlers: bool,
        calls: Mutex<Vec<CacheWarmingDecisionEvent>>,
        answer: Option<CacheWarmingAction>,
    }

    impl DecisionEmitter for CountingEmitter {
        fn has_handlers(&self) -> bool {
            self.handlers
        }

        fn decide(
            &self,
            event: CacheWarmingDecisionEvent,
        ) -> pa_agent::BoxFut<'static, CacheWarmingOutcome> {
            self.calls.lock().expect("calls").push(event);
            let answer = self.answer;
            Box::pin(async move { CacheWarmingOutcome { action: answer } })
        }
    }

    /// A provider seam whose first event is the finished message.
    fn answering_base(reply: AssistantMessage) -> StreamFn {
        struct One {
            reply: Option<AssistantMessage>,
        }

        impl ModelStream for One {
            fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
                Box::pin(async move {
                    self.reply
                        .take()
                        .map(|message| AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message,
                        })
                })
            }

            fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
                Box::pin(async { anyhow::bail!("the scripted stream has no result") })
            }

            fn close(&mut self) {}
        }

        Arc::new(move |_, _, _| {
            let reply = reply.clone();
            Box::pin(
                async move { Ok(Box::new(One { reply: Some(reply) }) as Box<dyn ModelStream>) },
            )
        })
    }

    #[tokio::test]
    async fn no_decision_is_emitted_without_a_subscriber() {
        let emitter = Arc::new(CountingEmitter {
            handlers: false,
            calls: Mutex::new(Vec::new()),
            answer: None,
        });
        let warmer = Warmer::new(Some(Arc::clone(&emitter) as Arc<dyn DecisionEmitter>));
        warmer.set_mode(Mode::Streaming);
        warmer.start(request(claude()), Some((Tier::Short, SHORT_TTL)), NOW);
        warmer.on_response(&message(&claude(), "stop", 100_000, 0, 20, 0.0), NOW);
        warmer.step_at(NOW + 270_000).await;
        assert!(emitter.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn a_subscriber_judges_every_decision_and_can_stop_the_refresh() {
        let emitter = Arc::new(CountingEmitter {
            handlers: true,
            calls: Mutex::new(Vec::new()),
            answer: Some(CacheWarmingAction::Stop),
        });
        let model = claude();
        let warmer = Warmer::new(Some(Arc::clone(&emitter) as Arc<dyn DecisionEmitter>));
        warmer.set_mode(Mode::Streaming);
        warmer.start(request(model.clone()), Some((Tier::Short, SHORT_TTL)), NOW);
        warmer.on_response(&message(&model, "stop", 100_000, 0, 20, 0.0), NOW);
        warmer.step_at(NOW + 270_000).await;
        let calls = emitter.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].action, CacheWarmingAction::Warm);
        assert_eq!(calls[0].method, Some(CacheWarmingRentMethod::Replay));
        drop(calls);
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(status.reason.as_deref(), Some("stopped by extension"));
    }

    #[tokio::test]
    async fn a_warm_override_sends_the_refresh_the_warmer_would_have_skipped() {
        let emitter = Arc::new(CountingEmitter {
            handlers: true,
            calls: Mutex::new(Vec::new()),
            answer: Some(CacheWarmingAction::Warm),
        });
        let model = claude();
        let warmer = Warmer::new(Some(Arc::clone(&emitter) as Arc<dyn DecisionEmitter>));
        warmer.set_mode(Mode::Streaming);
        warmer.start(
            Arc::new(Request {
                base: answering_base(message(&model, "stop", 100_000, 0, 1, 0.031)),
                model: model.clone(),
                context: LlmContext::default(),
                options: StreamRequestOptions::default(),
            }),
            Some((Tier::Short, SHORT_TTL)),
            NOW,
        );
        // 1k tokens: the warmer's own decision is `stop` (savings under the floor).
        warmer.on_response(&message(&model, "stop", 1_000, 0, 20, 0.0), NOW);
        warmer.step_at(NOW + 270_000).await;
        let calls = emitter.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].action, CacheWarmingAction::Stop);
        drop(calls);
        // The override sent the refresh: it hit, was billed, and the next decision is armed.
        let status = warmer.status();
        assert!(status.scheduled, "{status:?}");
        let decision = status.decision.expect("the next decision is priced");
        assert!((decision.spent_cost - 0.031).abs() < 1e-9, "{decision:?}");
    }

    #[test]
    fn the_mode_switch_stops_an_active_run() {
        let warmer = warmed(Mode::Idle, claude(), Tier::Short, SHORT_TTL);
        warmer.set_mode(Mode::Off);
        let status = warmer.status();
        assert!(!status.scheduled);
        assert_eq!(status.reason.as_deref(), Some("cache warming disabled"));
    }
}
