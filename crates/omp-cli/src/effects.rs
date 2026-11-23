//! Retry ledger for commands that carry an `effectId` (the gateway's idempotency key).
//!
//! A command with an `effectId` runs once per id: a retry (same id) gets the first run's
//! answer, and a retry that arrives while the first run is still going waits for it instead of
//! running again. The ledger keeps the most recent [`CAPACITY`] ids of one session.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::OnceCell;

/// Ids remembered per session; the oldest is forgotten first.
const CAPACITY: usize = 64;

#[derive(Default)]
pub struct EffectLedger {
    order: VecDeque<String>,
    cells: HashMap<String, Arc<OnceCell<Value>>>,
}

impl EffectLedger {
    /// The cell that holds `effect_id`'s answer, created (and the oldest id evicted) on first use.
    fn cell(&mut self, effect_id: &str) -> Arc<OnceCell<Value>> {
        if let Some(cell) = self.cells.get(effect_id) {
            return cell.clone();
        }
        if self.order.len() >= CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.cells.remove(&oldest);
            }
        }
        let cell = Arc::new(OnceCell::new());
        self.order.push_back(effect_id.to_owned());
        self.cells.insert(effect_id.to_owned(), cell.clone());
        cell
    }
}

/// Run `run` for `effect_id` unless the ledger already has (or is computing) its answer.
pub async fn once<F>(ledger: &parking_lot::Mutex<EffectLedger>, effect_id: &str, run: F) -> Value
where
    F: Future<Output = Value>,
{
    let cell = ledger.lock().cell(effect_id);
    cell.get_or_init(|| run).await.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_retry_gets_the_first_answer_without_running_again() {
        let ledger = parking_lot::Mutex::new(EffectLedger::default());
        let runs = AtomicUsize::new(0);
        let first = once(&ledger, "e1", async {
            runs.fetch_add(1, Ordering::SeqCst);
            json!({"idle": true})
        })
        .await;
        let retry = once(&ledger, "e1", async {
            runs.fetch_add(1, Ordering::SeqCst);
            json!({"idle": false})
        })
        .await;
        assert_eq!(first, json!({"idle": true}));
        assert_eq!(retry, first);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let other = once(&ledger, "e2", async { json!({"idle": false}) }).await;
        assert_eq!(other, json!({"idle": false}));
    }

    #[tokio::test]
    async fn a_retry_during_the_first_run_waits_for_its_answer() {
        let ledger = Arc::new(parking_lot::Mutex::new(EffectLedger::default()));
        let runs = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        let first = {
            let (ledger, runs) = (ledger.clone(), runs.clone());
            tokio::spawn(async move {
                once(&ledger, "e1", async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                    json!({"idle": true})
                })
                .await
            })
        };
        started_rx.await.expect("first run started");
        let retry = {
            let (ledger, runs) = (ledger.clone(), runs.clone());
            tokio::spawn(async move {
                once(&ledger, "e1", async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    json!({"idle": false})
                })
                .await
            })
        };
        // The retry has joined the first run's cell once it is no longer the cell's only holder.
        while Arc::strong_count(&ledger.lock().cell("e1")) < 4 {
            tokio::task::yield_now().await;
        }
        finish_tx.send(()).expect("first run still waiting");
        assert_eq!(first.await.unwrap(), json!({"idle": true}));
        assert_eq!(retry.await.unwrap(), json!({"idle": true}));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_oldest_id_is_forgotten_past_capacity() {
        let ledger = parking_lot::Mutex::new(EffectLedger::default());
        for i in 0..=CAPACITY {
            once(&ledger, &format!("e{i}"), async move { json!(i) }).await;
        }
        let runs = AtomicUsize::new(0);
        let again = once(&ledger, "e0", async {
            runs.fetch_add(1, Ordering::SeqCst);
            json!("again")
        })
        .await;
        assert_eq!(again, json!("again"));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let kept = once(&ledger, &format!("e{CAPACITY}"), async { json!("new") }).await;
        assert_eq!(kept, json!(CAPACITY));
    }
}
