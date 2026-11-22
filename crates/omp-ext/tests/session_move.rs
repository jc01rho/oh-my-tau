//! `session_move_export` / `session_move_import`: wire shape, the per-handler `AbortSignal` the
//! host adds, and handler failures reported to the core (which refuses the move on any).

use std::path::PathBuf;
use std::time::Duration;

use omp_ext::lifecycle::LifecycleEvent;
use omp_ext::{ExtHost, HostConfig, HostMessage};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

mod common;

async fn entry(rx: &mut UnboundedReceiver<HostMessage>, custom_type: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {custom_type} entry"))
            .expect("host closed");
        if let HostMessage::Notification { method, params } = msg {
            if method == "append_entry" && params["customType"] == custom_type {
                return serde_json::from_str(params["data"].as_str().unwrap()).unwrap();
            }
        }
    }
}

#[test]
fn move_events_serialize_like_senpi() {
    let export = LifecycleEvent::SessionMoveExport {
        move_id: "m1".into(),
        dir: "/s/.move-state/out/m1/a1".into(),
    };
    assert_eq!(
        serde_json::to_string(&export).unwrap(),
        r#"{"type":"session_move_export","moveId":"m1","dir":"/s/.move-state/out/m1/a1"}"#
    );
    let import = LifecycleEvent::SessionMoveImport {
        move_id: "m1".into(),
        dir: "/s/in".into(),
        redelivered: false,
    };
    assert_eq!(
        serde_json::to_string(&import).unwrap(),
        r#"{"type":"session_move_import","moveId":"m1","dir":"/s/in","redelivered":false}"#
    );
}

#[tokio::test]
async fn handlers_get_a_signal_write_the_staged_folder_and_report_failures() {
    if common::bun_missing("handlers_get_a_signal_write_the_staged_folder_and_report_failures") {
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cfg = HostConfig::new(root.join("../../ext-host/host.ts"));
    cfg.handler_timeout_ms = Some(2000);
    let host = ExtHost::spawn(&cfg).await.unwrap();
    let loaded = host
        .load(
            Some(&[root.join("tests/fixtures/session_move.ts")]),
            None,
            "/tmp",
            Some(json!({"sessionId": "s1"})),
        )
        .await
        .unwrap();
    assert!(loaded.errors.is_empty(), "{loaded:?}");
    let mut rx = host.take_incoming().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().to_string_lossy().into_owned();

    let r = host
        .emit_session_move_export("m1".into(), staged.clone(), None)
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        entry(&mut rx, "seen:session_move_export").await,
        json!({"type": "session_move_export", "moveId": "m1", "dir": staged, "signal": true})
    );
    assert!(dir
        .path()
        .join("fixture/session_move_export.json")
        .is_file());

    let r = host
        .emit_session_move_import("m1".into(), staged.clone(), true, None)
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        entry(&mut rx, "seen:session_move_import").await,
        json!({"type": "session_move_import", "moveId": "m1", "dir": staged, "redelivered": true, "signal": true})
    );

    let refused = host
        .emit_session_move_export("boom".into(), staged, None)
        .await
        .unwrap();
    assert_eq!(refused.errors.len(), 1, "{:?}", refused.errors);
    assert!(refused.errors[0]["error"]
        .as_str()
        .is_some_and(|e| e.contains("session_move_export refused")));
    host.shutdown().await.unwrap();
}
