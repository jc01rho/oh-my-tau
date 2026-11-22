//! Bounded auxiliary-lane extraction and immutable fact persistence.
//!
//! Extraction output is a strict line protocol. Anything malformed is counted
//! and dropped rather than stored: facts are immutable once written, so a bad
//! parse can never be corrected by an update.

use std::future::Future;
use std::pin::Pin;

use sha2::{Digest as _, Sha256};

use crate::{
    store::{BankStore, NewFact},
    Error, Result,
};

/// Maximum transcript bytes sent to auxiliary inference.
pub const MAX_EXTRACTION_INPUT_BYTES: usize = 256 * 1024;
/// Maximum completion bytes parsed from auxiliary inference.
pub const MAX_EXTRACTION_OUTPUT_BYTES: usize = 256 * 1024;
/// Maximum jobs returned by one durable-queue read.
pub const MAX_EXTRACTION_BATCH_JOBS: usize = 16;
/// Maximum aggregate input bytes returned by one durable-queue read.
pub const MAX_EXTRACTION_BATCH_BYTES: usize = 1024 * 1024;

/// Maximum accepted subject bytes.
pub const MAX_SUBJECT_BYTES: usize = 512;
/// Maximum accepted predicate bytes.
pub const MAX_PREDICATE_BYTES: usize = 256;
/// Maximum accepted object bytes.
pub const MAX_OBJECT_BYTES: usize = 4096;

/// One memory-extraction completion request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractionRequest {
    /// User-authored framed transcript only.
    pub input: String,
    /// Authoring session.
    pub session_id: String,
    /// Durable source memory id.
    pub source_id: String,
}

/// App-owned auxiliary completion boundary.
///
/// The lane resolves the configured `none`/`smol`/`remote` model mode. The
/// memory crate never performs network egress itself, so credentials and
/// endpoints stay in the application.
pub trait ExtractionLane: Send + Sync {
    /// Completes one bounded extraction job.
    fn complete<'a>(
        &'a self,
        request: &'a ExtractionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

/// Extracted immutable fact.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedFact {
    /// Stable content-derived id.
    pub id: String,
    /// Subject.
    pub subject: String,
    /// Predicate.
    pub predicate: String,
    /// Object.
    pub object: String,
    /// Confidence in `[0, 1]`.
    pub confidence: f64,
}

/// Extraction persistence receipt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExtractionReport {
    /// Valid lines parsed.
    pub parsed: usize,
    /// New immutable facts inserted.
    pub inserted: usize,
    /// Malformed or unsafe lines ignored.
    pub rejected: usize,
}

/// Runs one bounded completion, parses the line protocol, and persists
/// immutable facts.
///
/// A successful completion always acknowledges its durable job, even when it
/// yields zero facts; only an inference failure leaves the job queued for retry.
pub async fn extract_and_store<L: ExtractionLane>(
    lane: &L,
    store: &BankStore,
    request: ExtractionRequest,
) -> Result<ExtractionReport> {
    if request.input.is_empty() || request.input.len() > MAX_EXTRACTION_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let completion = lane.complete(&request).await?;
    if completion.len() > MAX_EXTRACTION_OUTPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let (facts, rejected) = parse_facts(completion.as_str(), request.source_id.as_str());
    let borrowed = facts
        .iter()
        .map(|fact| NewFact {
            fact_id: fact.id.as_str(),
            session_id: request.session_id.as_str(),
            subject: fact.subject.as_str(),
            predicate: fact.predicate.as_str(),
            object: fact.object.as_str(),
            timestamp: None,
            source_memory_id: request.source_id.as_str(),
            confidence: fact.confidence,
        })
        .collect::<Vec<_>>();
    let inserted = store.complete_extraction(request.source_id.as_str(), &borrowed)?;
    Ok(ExtractionReport {
        parsed: facts.len(),
        inserted,
        rejected,
    })
}

/// Parses the compact extraction line format:
/// `FACT<TAB>subject<TAB>predicate<TAB>object<TAB>confidence`.
///
/// Blank lines and `#` comments are ignored and not counted as rejections.
/// Every other non-conforming line increments the rejection count.
pub fn parse_facts(output: &str, source_id: &str) -> (Vec<ExtractedFact>, usize) {
    let mut facts = Vec::new();
    let mut rejected = 0usize;
    for (line_number, line) in output.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 || fields[0] != "FACT" {
            rejected += 1;
            continue;
        }
        let subject = fields[1].trim();
        let predicate = fields[2].trim();
        let object = fields[3].trim();
        let confidence = fields[4].trim().parse::<f64>().ok();
        if subject.is_empty()
            || predicate.is_empty()
            || object.is_empty()
            || subject.len() > MAX_SUBJECT_BYTES
            || predicate.len() > MAX_PREDICATE_BYTES
            || object.len() > MAX_OBJECT_BYTES
            || confidence.is_none_or(|value| !value.is_finite())
        {
            rejected += 1;
            continue;
        }
        let material = format!("{source_id}\0{line_number}\0{subject}\0{predicate}\0{object}");
        let hex = sha256_hex(material.as_bytes());
        facts.push(ExtractedFact {
            id: format!("fact_{}", &hex[..24]),
            subject: subject.to_owned(),
            predicate: predicate.to_owned(),
            object: object.to_owned(),
            confidence: confidence.unwrap_or(0.0).clamp(0.0, 1.0),
        });
    }
    (facts, rejected)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bank::BankId,
        store::{MemoryTier, NewMemory},
        test_support::temp_root,
    };

    struct FixedLane(&'static str);

    impl ExtractionLane for FixedLane {
        fn complete<'a>(
            &'a self,
            _request: &'a ExtractionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            let output = self.0.to_owned();
            Box::pin(async move { Ok(output) })
        }
    }

    struct FailingLane;

    impl ExtractionLane for FailingLane {
        fn complete<'a>(
            &'a self,
            _request: &'a ExtractionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move { Err(Error::InvalidIdentifier) })
        }
    }

    #[test]
    fn parse_facts_accepts_the_line_protocol_and_counts_rejections() {
        let output =
            "# comment\n\nFACT\tUser\tprefers\tRust\t0.9\nFACT\tUser\tprefers\tSQLite\t0.5\n\
                      BROKEN\tUser\tprefers\tRust\t0.9\nFACT\tonly\ttwo\n\
                      FACT\tUser\tprefers\tRust\tNaN\nFACT\t\tprefers\tRust\t0.5";
        let (facts, rejected) = parse_facts(output, "mem-source");
        assert_eq!(facts.len(), 2);
        assert_eq!(rejected, 4);
        assert_eq!(facts[0].subject, "User");
        assert_eq!(facts[0].predicate, "prefers");
        assert_eq!(facts[0].object, "Rust");
        assert!((facts[0].confidence - 0.9).abs() < f64::EPSILON);
        assert!(facts[0].id.starts_with("fact_"));
        assert_eq!(facts[0].id.len(), "fact_".len() + 24);
        assert_ne!(facts[0].id, facts[1].id);
    }

    #[test]
    fn parse_facts_rejects_oversized_fields() {
        let oversized = "x".repeat(MAX_OBJECT_BYTES + 1);
        let (facts, rejected) = parse_facts(&format!("FACT\ts\tp\t{oversized}\t0.5"), "src");
        assert!(facts.is_empty());
        assert_eq!(rejected, 1);
    }

    #[tokio::test]
    async fn extract_and_store_persists_facts_and_acks_the_job() {
        let root = temp_root("extract");
        let store = BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store");
        store
            .save(NewMemory {
                content: "durable transcript",
                embed_text: None,
                source: "test",
                session_id: "session",
                importance: 0.5,
                veracity: "user",
                memory_type: "episode",
                metadata: &serde_json::Value::Null,
                stable_id: Some("mem-source"),
            })
            .expect("save");
        store
            .connection()
            .expect("connection")
            .execute(
                "INSERT INTO extraction_jobs(source_memory_id, session_id, input, created_at) \
                 VALUES ('mem-source', 'session', 'input', '2026-01-01T00:00:00.000Z')",
                [],
            )
            .expect("queue job");

        let report = extract_and_store(
            &FixedLane("FACT\tUser\tprefers\tRust\t0.9"),
            &store,
            ExtractionRequest {
                input: "user text".to_owned(),
                session_id: "session".to_owned(),
                source_id: "mem-source".to_owned(),
            },
        )
        .await
        .expect("extract");

        assert_eq!(
            report,
            ExtractionReport {
                parsed: 1,
                inserted: 1,
                rejected: 0
            }
        );
        assert_eq!(store.counts().expect("counts").facts, 1);
        assert_eq!(store.pending_extraction_count().expect("pending"), 0);
        let facts = store.search_facts("User Rust", 8).expect("search");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].record.tier, MemoryTier::Fact);
    }

    #[tokio::test]
    async fn a_failed_completion_leaves_the_job_queued_for_retry() {
        let root = temp_root("extract-fail");
        let store = BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store");
        store
            .connection()
            .expect("connection")
            .execute(
                "INSERT INTO extraction_jobs(source_memory_id, session_id, input, created_at) \
                 VALUES ('mem-source', 'session', 'input', '2026-01-01T00:00:00.000Z')",
                [],
            )
            .expect("queue job");

        let error = extract_and_store(
            &FailingLane,
            &store,
            ExtractionRequest {
                input: "user text".to_owned(),
                session_id: "session".to_owned(),
                source_id: "mem-source".to_owned(),
            },
        )
        .await;
        assert!(error.is_err());
        assert_eq!(store.pending_extraction_count().expect("pending"), 1);
    }

    #[tokio::test]
    async fn empty_input_and_oversized_output_are_refused() {
        let root = temp_root("extract-bounds");
        let store = BankStore::open(
            root.join("mnemopi.db"),
            BankId::configured("test").expect("bank"),
            root,
        )
        .expect("store");
        let request = |input: String| ExtractionRequest {
            input,
            session_id: "session".to_owned(),
            source_id: "mem-source".to_owned(),
        };

        assert!(
            extract_and_store(&FixedLane(""), &store, request(String::new()))
                .await
                .is_err(),
            "empty input is refused before inference"
        );

        let oversized = "x".repeat(MAX_EXTRACTION_INPUT_BYTES + 1);
        assert!(
            extract_and_store(&FixedLane(""), &store, request(oversized))
                .await
                .is_err()
        );
    }
}
