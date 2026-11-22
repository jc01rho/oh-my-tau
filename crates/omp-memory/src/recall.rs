//! Four-voice recall, reciprocal-rank fusion, deduplication, and scoped
//! fallback.
//!
//! Each voice ranks candidates independently. Fusion combines
//! `weight / (RRF_K + rank)` with a small bounded native-score term so that a
//! strong single-voice hit still beats a weak multi-voice coincidence, then
//! deduplicates by normalized content and applies the caller's row/token
//! budget.

use std::collections::{hash_map::Entry, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    bank::BankId,
    store::{BankStore, MemoryRecord, RankedCandidate},
    Result,
};

/// Reciprocal-rank constant from the published RRF formulation.
pub const RRF_K: f64 = 60.0;
/// Weight applied to the vector voice.
pub const VECTOR_WEIGHT: f64 = 0.35;
/// Weight applied to the graph voice.
pub const GRAPH_WEIGHT: f64 = 0.25;
/// Weight applied to the episodic voice.
pub const EPISODIC_WEIGHT: f64 = 0.25;
/// Weight applied to the working-memory voice.
pub const WORKING_WEIGHT: f64 = 0.15;
/// Approximate bytes per token used for budget estimation.
const BYTES_PER_TOKEN: usize = 4;

/// One independent recall voice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecallVoice {
    /// Dense semantic similarity.
    Vector,
    /// Entity/triple and associative-link traversal.
    Graph,
    /// Consolidated episodic lexical/importance recall.
    Episodic,
    /// Recent working-memory lexical recall.
    Working,
}

impl RecallVoice {
    /// Every voice in deterministic fusion order.
    pub const ALL: [Self; 4] = [Self::Vector, Self::Graph, Self::Episodic, Self::Working];

    /// Returns the fusion weight for this voice.
    pub const fn weight(self) -> f64 {
        match self {
            Self::Vector => VECTOR_WEIGHT,
            Self::Graph => GRAPH_WEIGHT,
            Self::Episodic => EPISODIC_WEIGHT,
            Self::Working => WORKING_WEIGHT,
        }
    }

    /// Returns the persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::Episodic => "episodic",
            Self::Working => "working",
        }
    }
}

impl std::fmt::Display for RecallVoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Per-voice fused contributions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VoiceScores {
    /// Vector contribution.
    pub vector: f64,
    /// Graph contribution.
    pub graph: f64,
    /// Episodic contribution.
    pub episodic: f64,
    /// Working-memory contribution.
    pub working: f64,
}

impl VoiceScores {
    fn add(&mut self, voice: RecallVoice, contribution: f64) {
        match voice {
            RecallVoice::Vector => self.vector += contribution,
            RecallVoice::Graph => self.graph += contribution,
            RecallVoice::Episodic => self.episodic += contribution,
            RecallVoice::Working => self.working += contribution,
        }
    }

    fn merge(&mut self, other: Self) {
        self.vector += other.vector;
        self.graph += other.graph;
        self.episodic += other.episodic;
        self.working += other.working;
    }

    /// Total fused contribution across every voice.
    pub fn total(self) -> f64 {
        self.vector + self.graph + self.episodic + self.working
    }
}

/// One deterministic fused recall result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    /// Full memory record.
    pub memory: MemoryRecord,
    /// RRF plus bounded native-score merge.
    pub score: f64,
    /// Individual voice contributions.
    pub voice_scores: VoiceScores,
    /// Whether the shared-bank broadening pass contributed evidence.
    pub broadened: bool,
}

/// Strict search bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecallBounds {
    /// Maximum returned rows.
    pub limit: usize,
    /// Approximate token ceiling across returned content.
    pub token_budget: usize,
    /// Maximum candidates accepted per voice before fusion.
    pub voice_limit: usize,
}

impl Default for RecallBounds {
    fn default() -> Self {
        Self {
            limit: 8,
            token_budget: 5000,
            voice_limit: 32,
        }
    }
}

impl RecallBounds {
    /// Applies documented floors and ceilings.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            limit: self.limit.clamp(1, 50),
            token_budget: self.token_budget.clamp(1, 32 * 1024),
            voice_limit: self.voice_limit.clamp(1, 100),
        }
    }
}

/// Stateless recall engine over ordered scoped banks.
pub struct RecallEngine<'a> {
    stores: &'a [BankStore],
    project_bank: &'a BankId,
    shared_bank: Option<&'a BankId>,
}

impl<'a> RecallEngine<'a> {
    /// Creates recall over project-first scoped stores.
    pub const fn new(
        stores: &'a [BankStore],
        project_bank: &'a BankId,
        shared_bank: Option<&'a BankId>,
    ) -> Self {
        Self {
            stores,
            project_bank,
            shared_bank,
        }
    }

    /// Recalls through vector, graph, episodic, and working-memory voices.
    pub fn recall(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        bounds: RecallBounds,
    ) -> Result<Vec<RecallResult>> {
        let bounds = bounds.normalized();
        let terms = query_terms(query);
        let broadened = self
            .shared_bank
            .and_then(|shared| derive_shared_fallback(query, self.project_bank, shared));
        let mut voices = HashMap::<RecallVoice, Vec<VoiceCandidate>>::new();
        for store in self.stores {
            let is_shared = self.shared_bank.is_some_and(|bank| bank == store.bank());
            self.collect_store(
                store,
                query,
                &terms,
                query_embedding,
                bounds.voice_limit,
                false,
                &mut voices,
            )?;
            if is_shared {
                if let Some(fallback) = broadened.as_deref() {
                    let fallback_terms = query_terms(fallback);
                    self.collect_store(
                        store,
                        fallback,
                        &fallback_terms,
                        query_embedding,
                        bounds.voice_limit,
                        true,
                        &mut voices,
                    )?;
                }
            }
        }
        Ok(fuse(voices, bounds))
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_store(
        &self,
        store: &BankStore,
        query: &str,
        terms: &[String],
        query_embedding: Option<&[f32]>,
        limit: usize,
        broadened: bool,
        voices: &mut HashMap<RecallVoice, Vec<VoiceCandidate>>,
    ) -> Result<()> {
        if let Some(query_vector) = query_embedding.and_then(normalize) {
            let vector = vector_candidates(store, &query_vector, limit)?;
            voices.entry(RecallVoice::Vector).or_default().extend(
                vector
                    .into_iter()
                    .map(|candidate| VoiceCandidate::new(candidate, broadened)),
            );
        }
        {
            let graph = voices.entry(RecallVoice::Graph).or_default();
            graph.extend(
                store
                    .search_facts(query, limit)?
                    .into_iter()
                    .map(|candidate| VoiceCandidate::new(candidate, broadened)),
            );
            graph.extend(
                store
                    .graph_candidates(terms, limit)?
                    .into_iter()
                    .map(|candidate| VoiceCandidate::new(candidate, broadened)),
            );
        }
        voices.entry(RecallVoice::Episodic).or_default().extend(
            store
                .search_episodic(query, limit)?
                .into_iter()
                .map(|candidate| VoiceCandidate::new(candidate, broadened)),
        );
        {
            let working = voices.entry(RecallVoice::Working).or_default();
            working.extend(
                store
                    .search_working(query, limit)?
                    .into_iter()
                    .map(|candidate| VoiceCandidate::new(candidate, broadened)),
            );
            if looks_temporal(query) {
                working.extend(
                    store
                        .recent_working(limit)?
                        .into_iter()
                        .map(|candidate| VoiceCandidate::new(candidate, broadened)),
                );
            }
        }
        Ok(())
    }
}

struct VoiceCandidate {
    record: MemoryRecord,
    native: f64,
    broadened: bool,
}

impl VoiceCandidate {
    fn new(candidate: RankedCandidate, broadened: bool) -> Self {
        Self {
            record: candidate.record,
            native: candidate.score.clamp(0.0, 1.0),
            broadened,
        }
    }
}

fn vector_candidates(
    store: &BankStore,
    query: &[f32],
    limit: usize,
) -> Result<Vec<RankedCandidate>> {
    let mut output = Vec::new();
    for stored in store.vectors()? {
        if stored.vector.len() != query.len() {
            continue;
        }
        let cosine = stored
            .vector
            .iter()
            .zip(query)
            .map(|(left, right)| f64::from(*left) * f64::from(*right))
            .sum::<f64>();
        if let Some(record) = store.get(stored.memory_id.as_str())? {
            output.push(RankedCandidate {
                record,
                score: f64::midpoint(cosine, 1.0).clamp(0.0, 1.0),
            });
        }
    }
    output.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
    output.truncate(limit);
    Ok(output)
}

fn fuse(
    mut voices: HashMap<RecallVoice, Vec<VoiceCandidate>>,
    bounds: RecallBounds,
) -> Vec<RecallResult> {
    let mut fused = HashMap::<String, RecallResult>::new();
    for voice in RecallVoice::ALL {
        let Some(candidates) = voices.get_mut(&voice) else {
            continue;
        };
        candidates.sort_by(|left, right| {
            right
                .native
                .total_cmp(&left.native)
                .then_with(|| left.record.id.cmp(&right.record.id))
        });
        let mut seen = HashMap::<String, usize>::new();
        let mut unique: Vec<VoiceCandidate> = Vec::new();
        for candidate in candidates.drain(..) {
            match seen.get(&candidate.record.id) {
                Some(&index) => unique[index].broadened |= candidate.broadened,
                None => {
                    seen.insert(candidate.record.id.clone(), unique.len());
                    unique.push(candidate);
                }
            }
        }
        for (rank, candidate) in unique.iter().enumerate() {
            let contribution = f64::mul_add(
                voice.weight() * candidate.native,
                0.01,
                voice.weight() / (RRF_K + (rank + 1) as f64),
            );
            let result = fused
                .entry(candidate.record.id.clone())
                .or_insert_with(|| RecallResult {
                    memory: candidate.record.clone(),
                    score: 0.0,
                    voice_scores: VoiceScores::default(),
                    broadened: candidate.broadened,
                });
            result.score += contribution;
            result.voice_scores.add(voice, contribution);
            result.broadened |= candidate.broadened;
            if prefer_record(&candidate.record, &result.memory) {
                result.memory = candidate.record.clone();
            }
        }
    }

    let mut by_content = HashMap::<String, RecallResult>::new();
    for result in fused.into_values() {
        let key = normalized_content(result.memory.content.as_str());
        match by_content.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(result);
            }
            Entry::Occupied(mut entry) => {
                let current = entry.get_mut();
                current.score += result.score;
                current.voice_scores.merge(result.voice_scores);
                current.broadened |= result.broadened;
                if prefer(&result, current) {
                    current.memory = result.memory;
                }
            }
        }
    }
    let mut results = by_content.into_values().collect::<Vec<_>>();
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.memory.timestamp.cmp(&left.memory.timestamp))
            .then_with(|| left.memory.id.cmp(&right.memory.id))
    });
    let mut selected = Vec::new();
    let mut tokens = 0usize;
    for result in results {
        if selected.len() == bounds.limit {
            break;
        }
        let estimate = result.memory.content.len().div_ceil(BYTES_PER_TOKEN).max(1);
        if tokens.saturating_add(estimate) > bounds.token_budget {
            continue;
        }
        tokens += estimate;
        selected.push(result);
    }
    selected
}

fn prefer_record(left: &MemoryRecord, right: &MemoryRecord) -> bool {
    left.importance > right.importance
        || (left.importance == right.importance
            && (left.timestamp > right.timestamp
                || (left.timestamp == right.timestamp && left.bank < right.bank)))
}

fn prefer(left: &RecallResult, right: &RecallResult) -> bool {
    left.score > right.score
        || (left.score == right.score
            && (left.memory.timestamp > right.memory.timestamp
                || (left.memory.timestamp == right.memory.timestamp
                    && left.memory.id < right.memory.id)))
}

fn normalize(vector: &[f32]) -> Option<Vec<f32>> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(vector.iter().map(|value| value / norm).collect())
}

/// Removes literal project-bank terms for a second shared-bank recall pass.
///
/// Project bank names embed a repository basename and digest; those tokens
/// appear in queries only by accident and would otherwise dominate the shared
/// bank's lexical ranking.
pub fn derive_shared_fallback(query: &str, project: &BankId, shared: &BankId) -> Option<String> {
    if project == shared {
        return None;
    }
    let bank_terms = project
        .as_str()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<HashSet<_>>();
    if bank_terms.is_empty() {
        return None;
    }
    let mut kept = Vec::new();
    for token in query.split_whitespace() {
        let normalized = token
            .trim_matches(|character: char| !character.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        if !normalized.is_empty() && !bank_terms.contains(&normalized) {
            kept.push(token);
        }
    }
    let broadened = kept.join(" ").trim().to_owned();
    if broadened.is_empty() || normalized_content(&broadened) == normalized_content(query) {
        None
    } else {
        Some(broadened)
    }
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= 3)
        .map(str::to_lowercase)
        .take(32)
        .collect::<Vec<_>>();
    terms.sort_unstable();
    terms.dedup();
    terms
}

fn normalized_content(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn looks_temporal(query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    [
        "yesterday",
        "today",
        "recent",
        "last",
        "latest",
        "this week",
        "this month",
        "ago",
        "before",
    ]
    .iter()
    .any(|term| query.contains(term))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bank::{BankId, BankScope, BankScopeInput},
        config::BankScoping,
        link,
        store::{NewMemory, VectorEntry},
        test_support::temp_root,
    };

    fn store(bank: &str) -> BankStore {
        let root = temp_root("recall");
        BankStore::open(
            root.join(bank).join("mnemopi.db"),
            BankId::configured(bank).expect("bank"),
            root,
        )
        .expect("store")
    }

    fn save(store: &BankStore, id: &str, content: &str, importance: f64) {
        store
            .save(NewMemory {
                content,
                embed_text: Some(content),
                source: "test",
                session_id: "session",
                importance,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::Value::Null,
                stable_id: Some(id),
            })
            .expect("save");
    }

    fn bank(name: &str) -> BankId {
        BankId::configured(name).expect("bank")
    }

    #[test]
    fn voice_weights_and_rrf_constant_match_the_documented_contract() {
        assert!((RecallVoice::Vector.weight() - 0.35).abs() < f64::EPSILON);
        assert!((RecallVoice::Graph.weight() - 0.25).abs() < f64::EPSILON);
        assert!((RecallVoice::Episodic.weight() - 0.25).abs() < f64::EPSILON);
        assert!((RecallVoice::Working.weight() - 0.15).abs() < f64::EPSILON);
        assert!((RRF_K - 60.0).abs() < f64::EPSILON);
        let sum = RecallVoice::ALL
            .iter()
            .map(|voice| voice.weight())
            .sum::<f64>();
        assert!(
            (sum - 1.0).abs() < 1e-12,
            "weights form a convex combination"
        );
        assert_eq!(RecallVoice::ALL.len(), 4);
    }

    #[test]
    fn a_working_hit_reports_only_its_own_voice_contribution() {
        let store = store("project");
        save(
            &store,
            "mem-working",
            "User prefers Rust for systems work",
            0.5,
        );
        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        let results = engine
            .recall("Rust systems", None, RecallBounds::default())
            .expect("recall");
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.memory.id, "mem-working");
        assert!(result.voice_scores.working > 0.0);
        assert!(result.voice_scores.vector.abs() < f64::EPSILON);
        assert!(result.voice_scores.graph.abs() < f64::EPSILON);
        assert!(result.voice_scores.episodic.abs() < f64::EPSILON);
        assert!((result.score - result.voice_scores.total()).abs() < 1e-12);
        assert!(!result.broadened);
    }

    #[test]
    fn four_voices_fuse_for_one_record_and_outrank_a_single_voice_hit() {
        let store = store("project");
        save(&store, "mem-both", "SQLite WAL checkpoint tuning", 0.5);
        save(&store, "mem-lexical", "SQLite WAL notes only", 0.5);
        let generation = store.generations().expect("generations").durable;
        store
            .replace_vectors(
                generation,
                "test-model",
                &[VectorEntry {
                    memory_id: "mem-both",
                    vector: &[1.0, 0.0],
                }],
            )
            .expect("vectors");

        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        let results = engine
            .recall(
                "SQLite WAL checkpoint",
                Some(&[1.0, 0.0]),
                RecallBounds::default(),
            )
            .expect("recall");

        assert_eq!(results[0].memory.id, "mem-both");
        assert!(
            results[0].voice_scores.vector > 0.0,
            "vector voice participated"
        );
        assert!(
            results[0].voice_scores.working > 0.0,
            "working voice participated"
        );
        assert!(results[0].voice_scores.total() > 0.0);

        let lexical_only = results
            .iter()
            .find(|result| result.memory.id == "mem-lexical")
            .expect("lexical hit present");
        assert!(lexical_only.voice_scores.vector.abs() < f64::EPSILON);
        assert!(
            results[0].score > lexical_only.score,
            "multi-voice evidence must outrank a single-voice hit"
        );
    }

    #[test]
    fn graph_and_fact_voices_contribute_after_reconciliation() {
        let store = store("project");
        save(
            &store,
            "mem-a",
            "the retention cursor advances on settle",
            0.6,
        );
        save(
            &store,
            "mem-b",
            "the retention cursor rewinds when evicted",
            0.6,
        );
        link::reconcile(&store).expect("reconcile");
        assert!(store.counts().expect("counts").triples > 0);

        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        let results = engine
            .recall("retention cursor", None, RecallBounds::default())
            .expect("recall");
        assert!(!results.is_empty());
        let graph_contribution = results.iter().map(|r| r.voice_scores.graph).sum::<f64>();
        assert!(graph_contribution > 0.0, "graph voice contributed");
    }

    #[test]
    fn duplicate_content_across_banks_is_deduplicated_and_summed() {
        let project = store("project");
        let shared = store("default");
        save(
            &project,
            "mem-project",
            "User prefers tabs over spaces",
            0.5,
        );
        save(&shared, "mem-shared", "user  prefers TABS over spaces", 0.5);

        let stores = [project.clone(), shared.clone()];
        let engine = RecallEngine::new(&stores, project.bank(), Some(shared.bank()));
        let results = engine
            .recall("tabs spaces", None, RecallBounds::default())
            .expect("recall");
        assert_eq!(results.len(), 1, "normalized content dedupes across banks");
        assert!(results[0].voice_scores.working > 0.0);
    }

    #[test]
    fn limit_and_token_budget_bound_the_projection() {
        let store = store("project");
        for index in 0..12 {
            save(
                &store,
                &format!("mem-{index:02}"),
                &format!("budget token note {index}"),
                0.5,
            );
        }
        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        let limited = engine
            .recall(
                "budget token note",
                None,
                RecallBounds {
                    limit: 3,
                    ..RecallBounds::default()
                },
            )
            .expect("recall");
        assert_eq!(limited.len(), 3);

        let tight = engine
            .recall(
                "budget token note",
                None,
                RecallBounds {
                    limit: 12,
                    token_budget: 1,
                    ..RecallBounds::default()
                },
            )
            .expect("recall");
        assert!(
            tight.len() <= 1,
            "a one-token budget cannot admit every row"
        );
    }

    #[test]
    fn temporal_queries_add_recent_rows_to_the_working_voice() {
        let store = store("project");
        save(&store, "mem-recent", "unrelated recent observation", 0.7);
        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        let results = engine
            .recall("what did we do recently", None, RecallBounds::default())
            .expect("recall");
        assert!(
            results
                .iter()
                .any(|result| result.memory.id == "mem-recent"),
            "temporal wording pulls in recent working rows"
        );
    }

    #[test]
    fn shared_fallback_strips_project_bank_tokens() {
        let project = bank("alpha-0123456789ab");
        let shared = bank("default");
        let fallback = derive_shared_fallback("alpha 0123456789ab prefers rust", &project, &shared)
            .expect("fallback");
        assert_eq!(fallback, "prefers rust");
        assert!(derive_shared_fallback("alpha", &project, &shared).is_none());
        assert!(derive_shared_fallback("anything", &shared, &shared).is_none());
    }

    #[test]
    fn broadened_shared_hits_are_marked() {
        let project = store("alpha-0123456789ab");
        let shared = store("default");
        save(
            &project,
            "mem-project",
            "project local note about rust",
            0.5,
        );
        save(&shared, "mem-shared", "shared note about rust tooling", 0.5);
        let stores = [project.clone(), shared.clone()];
        let engine = RecallEngine::new(&stores, project.bank(), Some(shared.bank()));
        let results = engine
            .recall("alpha 0123456789ab rust", None, RecallBounds::default())
            .expect("recall");
        let shared_hit = results
            .iter()
            .find(|result| result.memory.id == "mem-shared")
            .expect("shared hit");
        assert!(
            shared_hit.broadened,
            "shared-only evidence is marked broadened"
        );
        let project_hit = results
            .iter()
            .find(|result| result.memory.id == "mem-project")
            .expect("project hit");
        assert!(!project_hit.broadened);
    }

    #[test]
    fn empty_and_symbol_only_queries_return_nothing() {
        let store = store("project");
        save(&store, "mem-any", "some durable content", 0.5);
        let engine = RecallEngine::new(std::slice::from_ref(&store), store.bank(), None);
        assert!(engine
            .recall("", None, RecallBounds::default())
            .expect("recall")
            .is_empty());
        assert!(engine
            .recall("!!! ???", None, RecallBounds::default())
            .expect("recall")
            .is_empty());
    }

    #[test]
    fn scoped_resolution_orders_project_before_shared() {
        let root = temp_root("scope-order");
        let scope = BankScope::resolve(&BankScopeInput {
            canonical_primary_root: Some(&root),
            workspace_root: &root,
            configured_bank: None,
            scoping: BankScoping::PerProjectTagged,
        })
        .expect("scope");
        assert_eq!(scope.recall.first(), Some(&scope.retain));
        assert_eq!(scope.recall.last(), Some(&scope.global));
    }
}
