//! Proactive associative-link construction and generation-fenced
//! reconciliation.

use std::collections::{HashMap, HashSet};

use crate::{
    store::{BankStore, GraphTriple, MemoryLink},
    Result,
};

/// Maximum authoritative rows considered by one reconcile pass.
pub const MAX_RECONCILE_ROWS: usize = 1000;
/// Maximum outgoing links retained per memory.
pub const MAX_LINKS_PER_MEMORY: usize = 8;
/// Minimum Jaccard term overlap required for an associative link.
pub const MIN_LINK_WEIGHT: f64 = 0.2;
/// Maximum distinct content terms indexed per memory.
const MAX_TERMS_PER_MEMORY: usize = 128;
/// Minimum term length accepted as a graph term.
const MIN_TERM_CHARS: usize = 3;

struct OwnedTriple {
    subject: String,
    predicate: &'static str,
    object: String,
    source: String,
    confidence: f64,
}

struct OwnedLink {
    source: String,
    target: String,
    weight: f64,
}

/// Rebuilds graph terms and associative links against one stable durable
/// generation.
///
/// Rows are bounded, deterministic, and derived exclusively from authoritative
/// memory content. A concurrent durable write rejects the commit rather than
/// publishing a mixed generation.
pub fn reconcile(store: &BankStore) -> Result<usize> {
    let expected = store.generations()?.durable;
    let records = store.list(MAX_RECONCILE_ROWS)?;
    let mut terms_by_id = HashMap::<String, HashSet<String>>::new();
    let mut ids_by_term = HashMap::<String, Vec<String>>::new();
    let mut triples = Vec::<OwnedTriple>::new();
    for record in &records {
        let terms = content_terms(record.content.as_str());
        for term in &terms {
            ids_by_term.entry(term.clone()).or_default().push(record.id.clone());
            triples.push(OwnedTriple {
                subject: term.clone(),
                predicate: "mentioned_in",
                object: record.id.clone(),
                source: record.id.clone(),
                confidence: record.importance,
            });
        }
        terms_by_id.insert(record.id.clone(), terms);
    }
    let mut overlap = HashMap::<(String, String), usize>::new();
    for ids in ids_by_term.values_mut() {
        ids.sort_unstable();
        ids.dedup();
        for (index, left) in ids.iter().enumerate() {
            for right in ids.iter().skip(index + 1) {
                *overlap.entry((left.clone(), right.clone())).or_default() += 1;
            }
        }
    }
    let mut candidates = HashMap::<String, Vec<(String, f64)>>::new();
    for ((left, right), intersection) in overlap {
        let left_count = terms_by_id.get(&left).map_or(0, HashSet::len);
        let right_count = terms_by_id.get(&right).map_or(0, HashSet::len);
        let union = left_count + right_count - intersection;
        if union == 0 {
            continue;
        }
        let weight = intersection as f64 / union as f64;
        if weight < MIN_LINK_WEIGHT {
            continue;
        }
        candidates.entry(left.clone()).or_default().push((right.clone(), weight));
        candidates.entry(right).or_default().push((left, weight));
    }
    let mut links = Vec::<OwnedLink>::new();
    let mut sources = candidates.keys().cloned().collect::<Vec<_>>();
    sources.sort_unstable();
    for source in sources {
        let Some(targets) = candidates.get_mut(&source) else {
            continue;
        };
        targets.sort_by(|left, right| {
            right.1.total_cmp(&left.1).then_with(|| left.0.cmp(&right.0))
        });
        targets.truncate(MAX_LINKS_PER_MEMORY);
        for (target, weight) in targets.iter() {
            links.push(OwnedLink {
                source: source.clone(),
                target: target.clone(),
                weight: *weight,
            });
        }
    }
    let graph = triples
        .iter()
        .map(|triple| GraphTriple {
            subject: triple.subject.as_str(),
            predicate: triple.predicate,
            object: triple.object.as_str(),
            source_memory_id: triple.source.as_str(),
            confidence: triple.confidence,
        })
        .collect::<Vec<_>>();
    let associations = links
        .iter()
        .map(|link| MemoryLink {
            source_memory_id: link.source.as_str(),
            target_memory_id: link.target.as_str(),
            relation: "context",
            weight: link.weight,
        })
        .collect::<Vec<_>>();
    store.replace_graph(expected, &graph, &associations)?;
    Ok(associations.len())
}

fn content_terms(content: &str) -> HashSet<String> {
    content
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= MIN_TERM_CHARS)
        .map(str::to_lowercase)
        .take(MAX_TERMS_PER_MEMORY)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bank::BankId,
        store::NewMemory,
        test_support::temp_root,
    };

    fn store() -> BankStore {
        let root = temp_root("link");
        BankStore::open(root.join("mnemopi.db"), BankId::configured("test").expect("bank"), root)
            .expect("store")
    }

    fn save(store: &BankStore, id: &str, content: &str) {
        store
            .save(NewMemory {
                content,
                embed_text: Some(content),
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
    fn reconcile_builds_triples_and_links_from_content_overlap() {
        let store = store();
        save(&store, "mem-a", "sqlite wal checkpoint tuning notes");
        save(&store, "mem-b", "sqlite wal checkpoint durability notes");
        save(&store, "mem-c", "completely unrelated lunch menu");

        let links = reconcile(&store).expect("reconcile");
        assert!(links >= 2, "a/b link in both directions");
        let counts = store.counts().expect("counts");
        assert!(counts.triples > 0);
        let generations = store.generations().expect("generations");
        assert_eq!(generations.graph, generations.durable);
        assert!(store.integrity().expect("integrity").graph_current);
    }

    #[test]
    fn reconcile_is_deterministic_and_idempotent() {
        let store = store();
        save(&store, "mem-a", "sqlite wal checkpoint tuning notes");
        save(&store, "mem-b", "sqlite wal checkpoint durability notes");
        let first = reconcile(&store).expect("first");
        let first_counts = store.counts().expect("counts");
        let second = reconcile(&store).expect("second");
        assert_eq!(first, second);
        assert_eq!(store.counts().expect("counts"), first_counts);
    }

    #[test]
    fn reconcile_without_rows_publishes_an_empty_graph() {
        let store = store();
        let links = reconcile(&store).expect("reconcile");
        assert_eq!(links, 0);
        assert_eq!(store.counts().expect("counts").triples, 0);
        assert!(store.integrity().expect("integrity").graph_current);
    }

    #[test]
    fn a_concurrent_durable_write_rejects_the_stale_reconcile() {
        let store = store();
        save(&store, "mem-a", "sqlite wal checkpoint tuning notes");
        let stale = store.generations().expect("generations").durable;
        save(&store, "mem-b", "sqlite wal checkpoint durability notes");
        let result = store.replace_graph(stale, &[], &[]);
        assert!(matches!(result, Err(crate::Error::StaleGeneration)));
    }

    #[test]
    fn content_terms_are_bounded_lowercase_and_length_filtered() {
        let terms = content_terms("SQLite WAL a ab abc AB_C");
        assert!(terms.contains("sqlite"));
        assert!(terms.contains("wal"));
        assert!(terms.contains("abc"));
        assert!(!terms.contains("a"));
        assert!(!terms.contains("ab"));
        assert!(terms.len() <= MAX_TERMS_PER_MEMORY);
    }
}
