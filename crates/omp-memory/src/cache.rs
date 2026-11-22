//! Exact and similar recall cache with bank-generation invalidation.
//!
//! Cached results are only valid while every participating bank keeps the
//! generation stamp it had at insertion, so a durable write anywhere in the
//! scope invalidates the entry instead of serving stale memories.

use std::collections::{HashSet, VecDeque};

use parking_lot::Mutex;

use crate::{
    recall::{RecallBounds, RecallResult},
    store::{BankStore, IndexGeneration},
    Result,
};

const DEFAULT_CAPACITY: usize = 128;
const MAX_CACHED_RESULT_BYTES: usize = 256 * 1024;
const MAX_CACHE_CAPACITY: usize = 1024;
const EMBEDDING_SIMILARITY_THRESHOLD: f32 = 0.95;
const TOKEN_SIMILARITY_THRESHOLD: f32 = 0.85;

/// Generation identity of one bank at cache insertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BankStamp {
    bank: String,
    generation: IndexGeneration,
}

impl BankStamp {
    /// Returns the bank this stamp covers.
    pub fn bank(&self) -> &str {
        self.bank.as_str()
    }

    /// Returns the generation snapshot.
    pub const fn generation(&self) -> IndexGeneration {
        self.generation
    }
}

/// Snapshot current generation fences for ordered recall banks.
pub fn stamps(stores: &[BankStore]) -> Result<Vec<BankStamp>> {
    stores
        .iter()
        .map(|store| {
            Ok(BankStamp {
                bank: store.bank().as_str().to_owned(),
                generation: store.generations()?,
            })
        })
        .collect()
}

#[derive(Clone)]
struct CacheEntry {
    query: String,
    tokens: HashSet<String>,
    embedding: Option<Vec<f32>>,
    stamps: Vec<BankStamp>,
    bounds: RecallBounds,
    results: Vec<RecallResult>,
}

/// Bounded recall cache.
///
/// Entries are evicted oldest-first and never survive an index generation
/// change.
pub struct RecallCache {
    entries: Mutex<VecDeque<CacheEntry>>,
    capacity: usize,
}

impl Default for RecallCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RecallCache {
    /// Creates the default 128-entry cache.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates a cache with a bounded capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            capacity: capacity.clamp(1, MAX_CACHE_CAPACITY),
        }
    }

    /// Returns the current number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns an exact query hit under identical bank generations.
    pub fn exact(
        &self,
        query: &str,
        current: &[BankStamp],
        bounds: RecallBounds,
    ) -> Option<Vec<RecallResult>> {
        let normalized = normalize_query(query);
        let mut entries = self.entries.lock();
        entries.retain(|entry| entry.stamps == current);
        let index = entries
            .iter()
            .position(|entry| entry.query == normalized && entry.bounds == bounds)?;
        let entry = entries.remove(index)?;
        let results = entry.results.clone();
        entries.push_back(entry);
        Some(results)
    }

    /// Returns a high-confidence similar-query hit.
    ///
    /// Dense cosine `>= 0.95` wins when both query embeddings exist; otherwise
    /// token Jaccard `>= 0.85` is required. Empty token sets never match.
    pub fn similar(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        current: &[BankStamp],
        bounds: RecallBounds,
    ) -> Option<Vec<RecallResult>> {
        let tokens = token_set(query);
        let embedding = embedding.and_then(normalize_embedding);
        let mut entries = self.entries.lock();
        entries.retain(|entry| entry.stamps == current);
        let mut best: Option<(usize, f32)> = None;
        for (index, entry) in
            entries.iter().enumerate().filter(|(_, entry)| entry.bounds == bounds)
        {
            let both_dense = embedding.is_some() && entry.embedding.is_some();
            let score = match (embedding.as_deref(), entry.embedding.as_deref()) {
                (Some(left), Some(right)) if left.len() == right.len() => cosine(left, right),
                _ => jaccard(&tokens, &entry.tokens),
            };
            let threshold = if both_dense {
                EMBEDDING_SIMILARITY_THRESHOLD
            } else {
                TOKEN_SIMILARITY_THRESHOLD
            };
            if score >= threshold && best.is_none_or(|(_, current_score)| score > current_score) {
                best = Some((index, score));
            }
        }
        let (index, _) = best?;
        let entry = entries.remove(index)?;
        let results = entry.results.clone();
        entries.push_back(entry);
        Some(results)
    }

    /// Inserts one bounded result set.
    pub fn insert(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        stamps: Vec<BankStamp>,
        bounds: RecallBounds,
        results: Vec<RecallResult>,
    ) {
        let bytes = results.iter().map(|result| result.memory.content.len()).sum::<usize>();
        if bytes > MAX_CACHED_RESULT_BYTES {
            return;
        }
        let query = normalize_query(query);
        let entry = CacheEntry {
            tokens: token_set(query.as_str()),
            query,
            embedding: embedding.and_then(normalize_embedding),
            stamps,
            bounds,
            results,
        };
        let mut entries = self.entries.lock();
        entries.retain(|existing| existing.query != entry.query || existing.bounds != entry.bounds);
        entries.push_back(entry);
        while entries.len() > self.capacity {
            entries.pop_front();
        }
    }

    /// Atomically invalidates every exact and similar tier.
    pub fn clear(&self) {
        self.entries.lock().clear();
    }
}

fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn token_set(query: &str) -> HashSet<String> {
    query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

fn normalize_embedding(vector: &[f32]) -> Option<Vec<f32>> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(vector.iter().map(|value| value / norm).collect())
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(left, right)| left * right).sum()
}

fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count();
    let union = left.len() + right.len() - intersection;
    intersection as f32 / union as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bank::BankId,
        recall::VoiceScores,
        store::{MemoryRecord, MemoryTier, NewMemory},
        test_support::temp_root,
    };

    fn store() -> BankStore {
        let root = temp_root("cache");
        BankStore::open(root.join("mnemopi.db"), BankId::configured("test").expect("bank"), root)
            .expect("store")
    }

    fn result(id: &str) -> RecallResult {
        RecallResult {
            memory: MemoryRecord {
                id: id.to_owned(),
                bank: BankId::configured("test").expect("bank"),
                tier: MemoryTier::Working,
                content: format!("content for {id}"),
                source: None,
                session_id: "session".to_owned(),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                importance: 0.5,
                veracity: "user".to_owned(),
                memory_type: "fact".to_owned(),
                metadata: serde_json::Value::Null,
                superseded_by: None,
            },
            score: 0.5,
            voice_scores: VoiceScores::default(),
            broadened: false,
        }
    }

    fn save(store: &BankStore, id: &str) {
        store
            .save(NewMemory {
                content: id,
                embed_text: Some(id),
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "fact",
                metadata: &serde_json::Value::Null,
                stable_id: Some(id),
            })
            .expect("save");
    }

    #[test]
    fn exact_hits_are_case_and_whitespace_insensitive() {
        let store = store();
        let current = stamps(std::slice::from_ref(&store)).expect("stamps");
        let cache = RecallCache::new();
        let bounds = RecallBounds::default();
        cache.insert(
            "User Prefers Rust",
            None,
            current.clone(),
            bounds,
            vec![result("mem-1")],
        );
        let hit = cache.exact("  user   prefers RUST ", &current, bounds).expect("hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].memory.id, "mem-1");
        assert!(cache.exact("unrelated", &current, bounds).is_none());
        assert!(
            cache
                .exact("user prefers rust", &current, RecallBounds { limit: 3, ..bounds })
                .is_none(),
            "bounds are part of the key"
        );
    }

    #[test]
    fn a_durable_write_invalidates_every_tier() {
        let store = store();
        let bounds = RecallBounds::default();
        let cache = RecallCache::new();
        let before = stamps(std::slice::from_ref(&store)).expect("stamps");
        cache.insert("user prefers rust", None, before.clone(), bounds, vec![result("mem-1")]);
        assert!(cache.exact("user prefers rust", &before, bounds).is_some());

        save(&store, "mem-2");
        let after = stamps(std::slice::from_ref(&store)).expect("stamps");
        assert_ne!(before, after);
        assert!(cache.exact("user prefers rust", &after, bounds).is_none());
        assert!(cache.similar("user prefers rust", None, &after, bounds).is_none());
        assert!(cache.is_empty(), "stale entries are dropped on lookup");
    }

    #[test]
    fn similar_hits_require_high_token_overlap() {
        let store = store();
        let current = stamps(std::slice::from_ref(&store)).expect("stamps");
        let bounds = RecallBounds::default();
        let cache = RecallCache::new();
        cache.insert(
            "user prefers rust for systems programming",
            None,
            current.clone(),
            bounds,
            vec![result("mem-1")],
        );
        assert!(
            cache
                .similar("user prefers rust for systems programming daily", None, &current, bounds)
                .is_some(),
            "six of seven shared tokens clears the Jaccard floor"
        );
        assert!(
            cache.similar("user prefers python for data pipelines", None, &current, bounds).is_none(),
            "a half-overlapping token set is below the floor"
        );
        assert!(
            cache.similar("completely different question", None, &current, bounds).is_none(),
            "unrelated token sets never hit"
        );
        assert!(cache.similar("", None, &current, bounds).is_none(), "empty token set never hits");
    }

    #[test]
    fn similar_hits_prefer_dense_embeddings_when_both_exist() {
        let store = store();
        let current = stamps(std::slice::from_ref(&store)).expect("stamps");
        let bounds = RecallBounds::default();
        let cache = RecallCache::new();
        cache.insert(
            "alpha beta",
            Some(&[1.0, 0.0]),
            current.clone(),
            bounds,
            vec![result("mem-dense")],
        );
        let hit = cache
            .similar("gamma delta", Some(&[0.999, 0.001]), &current, bounds)
            .expect("dense hit");
        assert_eq!(hit[0].memory.id, "mem-dense");
        assert!(
            cache.similar("gamma delta", Some(&[0.0, 1.0]), &current, bounds).is_none(),
            "an orthogonal embedding must not hit"
        );
        assert!(
            cache.similar("gamma delta", Some(&[f32::NAN, 0.0]), &current, bounds).is_none(),
            "non-finite embeddings are discarded"
        );
    }

    #[test]
    fn capacity_is_bounded_and_evicts_oldest_first() {
        let store = store();
        let current = stamps(std::slice::from_ref(&store)).expect("stamps");
        let bounds = RecallBounds::default();
        let cache = RecallCache::with_capacity(2);
        for index in 0..4 {
            cache.insert(
                &format!("query {index}"),
                None,
                current.clone(),
                bounds,
                vec![result(&format!("mem-{index}"))],
            );
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.exact("query 0", &current, bounds).is_none());
        assert!(cache.exact("query 3", &current, bounds).is_some());
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn oversized_result_sets_are_not_cached() {
        let store = store();
        let current = stamps(std::slice::from_ref(&store)).expect("stamps");
        let bounds = RecallBounds::default();
        let cache = RecallCache::new();
        let mut big = result("mem-big");
        big.memory.content = "x".repeat(MAX_CACHED_RESULT_BYTES + 1);
        cache.insert("big query", None, current.clone(), bounds, vec![big]);
        assert!(cache.exact("big query", &current, bounds).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn stamps_track_each_bank_generation_independently() {
        let left = store();
        let right = store();
        let current = stamps(&[left.clone(), right.clone()]).expect("stamps");
        assert_eq!(current.len(), 2);
        save(&left, "mem-left");
        let after = stamps(&[left, right]).expect("stamps");
        assert_ne!(after[0].generation().durable, current[0].generation().durable);
        assert_eq!(after[1].generation().durable, current[1].generation().durable);
    }
}
