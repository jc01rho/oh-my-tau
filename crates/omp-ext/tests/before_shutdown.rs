//! `session_before_shutdown` gate against the real extension host: cancel keeps the session,
//! no subscriber emits nothing, and a close that stops waiting aborts the handlers' `signal`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omp_ext::lifecycle::LifecycleEvent;
use omp_ext::{ExtHost, HostConfig, HostMessage};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

mod common;

async fn host_with(dir: &Path, source: &str, trace: Option<PathBuf>) -> Arc<ExtHost> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cfg = HostConfig::new(root.join("../../ext-host/host.ts"));
    cfg.handler_timeout_ms = Some(5000);
    cfg.trace = trace;
    let ext = dir.join("gate.ts");
    std::fs::write(&ext, source).unwrap();
    let host = ExtHost::spawn(&cfg).await.unwrap();
    let r = host
        .load(
            Some(&[ext]),
            None,
            dir.to_str().unwrap(),
            Some(json!({"sessionId": "s1"})),
        )
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    host
}

async fn entry(rx: &mut UnboundedReceiver<HostMessage>, custom_type: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {custom_type} entry"))
            .expect("host closed");
        if let HostMessage::Notification { method, params } = msg {
            if method == "append_entry" && params["customType"] == custom_type {
                return params["data"].clone();
            }
        }
    }
}

#[test]
fn wire_shape_matches_ts_without_the_signal() {
    assert_eq!(
        serde_json::to_string(&LifecycleEvent::SessionBeforeShutdown).unwrap(),
        r#"{"type":"session_before_shutdown"}"#
    );
}

#[tokio::test]
async fn cancel_keeps_the_session_and_no_cancel_closes() {
    if common::bun_missing("cancel_keeps_the_session_and_no_cancel_closes") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_with(
        dir.path(),
        r#"let n = 0;
export default (pi) => pi.on("session_before_shutdown", (e) => {
  pi.appendEntry("seen", JSON.stringify({ type: e.type, live: e.signal instanceof AbortSignal && !e.signal.aborted }));
  return ++n === 1 ? { cancel: true } : undefined;
});"#,
        None,
    )
    .await;
    let mut rx = host.take_incoming().unwrap();
    let never = std::future::pending::<()>;
    assert!(!host
        .emit_session_before_shutdown(never(), None)
        .await
        .unwrap());
    assert_eq!(
        entry(&mut rx, "seen").await,
        json!(r#"{"type":"session_before_shutdown","live":true}"#)
    );
    assert!(host
        .emit_session_before_shutdown(never(), None)
        .await
        .unwrap());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_subscriber_emits_nothing() {
    if common::bun_missing("no_subscriber_emits_nothing") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace.jsonl");
    let host = host_with(
        dir.path(),
        r#"export default (pi) => pi.on("agent_start", () => {});"#,
        Some(trace.clone()),
    )
    .await;
    assert!(!host.has_handlers("session_before_shutdown"));
    assert!(host
        .emit_session_before_shutdown(std::future::pending(), None)
        .await
        .unwrap());
    host.emit_event(&LifecycleEvent::AgentStart, None)
        .await
        .unwrap();
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    assert!(text.contains("\"agent_start\""), "{text}");
    assert!(!text.contains("session_before_shutdown"), "{text}");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_close_that_stops_waiting_aborts_the_signal_and_drops_the_cancel() {
    if common::bun_missing("a_close_that_stops_waiting_aborts_the_signal_and_drops_the_cancel") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_with(
        dir.path(),
        r#"export default (pi) => pi.on("session_before_shutdown", async (e) => {
  pi.appendEntry("asked", "1");
  await new Promise((r) => e.signal.addEventListener("abort", r, { once: true }));
  pi.appendEntry("aborted", String(e.signal.aborted));
  return { cancel: true };
});"#,
        None,
    )
    .await;
    let mut rx = host.take_incoming().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let gate = {
        let host = host.clone();
        tokio::spawn(async move {
            host.emit_session_before_shutdown(
                async move {
                    let _ = stopped.await;
                },
                None,
            )
            .await
        })
    };
    // The handler is waiting (an operator prompt would be open now): the close stops waiting.
    assert_eq!(entry(&mut rx, "asked").await, json!("1"));
    stop.send(()).unwrap();
    assert!(gate.await.unwrap().unwrap(), "an aborted gate closes");
    assert_eq!(entry(&mut rx, "aborted").await, json!("true"));
    host.shutdown().await.unwrap();
}
