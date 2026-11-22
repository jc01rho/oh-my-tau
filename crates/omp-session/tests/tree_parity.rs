//! Parity with the TypeScript agent: the session tree computed from a real (redacted) branched
//! session file must match omp's `SessionManager.getTree()` exactly. The golden digest in
//! `branched-session.expectation.json` was produced by running the TS sources under bun over this
//! fixture (`SessionManager.open(path).getTree()` flattened by `node_digest` below); the companion `buildSessionContext` check lives in omp-cli
//! (which depends on omp-compact; a dev-dependency here would cycle).

use serde_json::{json, Value};

/// Fixture entries: every JSONL record except the fixed-width title slot and the v3 header.
fn fixture_entries() -> Vec<Value> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/branched-session.jsonl"
    ))
    .unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| {
            !matches!(
                e.get("type").and_then(Value::as_str),
                Some("title" | "session")
            )
        })
        .collect()
}

fn expectation() -> Value {
    serde_json::from_str(include_str!("fixtures/branched-session.expectation.json")).unwrap()
}

/// The TS digest is flat: one `{id, parentId, children (ids), label?}` record per node, in
/// pre-order. `build_tree` roots arrive in the same order the TS `getTree` returns them.
fn node_digest(n: &omp_session::tree::TreeNode, out: &mut Vec<Value>) {
    let mut node = json!({
        "id": n.entry.get("id").and_then(Value::as_str).unwrap(),
        "parentId": n.entry.get("parentId").and_then(Value::as_str).map_or(Value::Null, |p| Value::String(p.to_owned())),
        "children": n.children.iter().filter_map(|c| c.id()).collect::<Vec<_>>(),
    });
    if let Some(label) = &n.label {
        node["label"] = Value::String(label.clone());
    }
    out.push(node);
    for child in &n.children {
        node_digest(child, out);
    }
}

#[test]
fn tree_matches_ts_get_tree() {
    let entries = fixture_entries();
    let golden = expectation();
    assert_eq!(
        omp_session::tree::leaf_id(&entries),
        golden["leafId"].as_str(),
        "leaf id must be the last physical journal entry"
    );
    let tree = omp_session::tree::build_tree(&entries);
    let mut digest = Vec::new();
    for root in &tree {
        node_digest(root, &mut digest);
    }
    assert_eq!(
        json!(digest),
        golden["tree"],
        "tree differs from TS getTree()"
    );
}

/// Writer-produced `label` / `session_info` lines use omp's key order, resolve onto the tree like
/// the TS index, and survive a strict JSONL v3 reload.
#[test]
fn writer_label_and_session_info_round_trip_through_tree() {
    let dir = std::env::temp_dir().join(format!("omp-tree-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut w =
        omp_session::SessionWriter::create(&dir, "/w", "sid", "2026-10-01T00:00:00.000Z").unwrap();
    let root = w
        .append_entry(
            "custom",
            json!({"customType": "x"}),
            "2026-10-01T00:00:01.000Z",
        )
        .unwrap();
    let label = w
        .append_label(&root, Some("checkpoint"), "2026-10-01T00:00:02.000Z")
        .unwrap();
    w.append_session_info("  My session  ", "2026-10-01T00:00:03.000Z")
        .unwrap();
    assert!(w
        .append_label("missing", Some("x"), "2026-10-01T00:00:04.000Z")
        .is_err());

    let raw = std::fs::read_to_string(w.path()).unwrap();
    let lines: Vec<&str> = raw
        .lines()
        .filter(|l| l.contains("\"type\":\"label\"") || l.contains("session_info"))
        .collect();
    assert_eq!(
        lines[0],
        format!("{{\"type\":\"label\",\"id\":\"{label}\",\"parentId\":\"{root}\",\"timestamp\":\"2026-10-01T00:00:02.000Z\",\"targetId\":\"{root}\",\"label\":\"checkpoint\"}}")
    );
    assert!(lines[1].ends_with("\"name\":\"My session\"}"));

    let loaded = omp_session::load(w.path()).unwrap();
    assert!(loaded.malformed.is_empty());
    let values = omp_session::read_values(w.path()).unwrap();
    let entries: Vec<_> = values
        .into_iter()
        .filter(|e| !matches!(e["type"].as_str(), Some("title" | "session")))
        .collect();
    let tree = omp_session::tree::build_tree(&entries);
    assert_eq!(tree[0].label.as_deref(), Some("checkpoint"));
    assert_eq!(
        omp_session::tree::session_name(&entries).as_deref(),
        Some("My session")
    );

    w.append_label(&root, None, "2026-10-01T00:00:05.000Z")
        .unwrap();
    let entries: Vec<_> = omp_session::read_values(w.path())
        .unwrap()
        .into_iter()
        .filter(|e| !matches!(e["type"].as_str(), Some("title" | "session")))
        .collect();
    assert_eq!(omp_session::tree::build_tree(&entries)[0].label, None);
    let _ = std::fs::remove_dir_all(&dir);
}
