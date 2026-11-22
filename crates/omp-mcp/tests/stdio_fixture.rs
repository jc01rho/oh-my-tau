//! Integration test against a fake stdio MCP server: exercises config
//! loading, connect/init handshake, `tools/list` pagination-free listing,
//! `tools/call`, result formatting, and per-server failure isolation.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use omp_mcp::format::format_tool_result;
use omp_mcp::{Content, McpManager, McpServerConfig, ServerKind, ToolCallResult};

/// A minimal MCP stdio server speaking NDJSON JSON-RPC 2.0: answers
/// `initialize`, `tools/list` (two tools) and `tools/call` (echoes args as
/// text plus a structuredContent payload).
const FAKE_SERVER_PY: &str = r#"
import json, sys

TOOLS = [
    {"name": "echo", "description": "Echo back the arguments",
     "inputSchema": {"type": "object", "properties": {
         "text": {"type": "string", "description": "text to echo"}},
         "required": ["text"]}},
    {"name": "fail", "description": "Always returns an error",
     "inputSchema": {"type": "object", "properties": {}}},
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25",
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "0.1"}}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "fail":
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                "result": {"content": [{"type": "text", "text": "boom"}],
                           "isError": True}}), flush=True)
            continue
        args = msg["params"].get("arguments", {})
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
            "result": {"content": [{"type": "text", "text": json.dumps(args)}],
                       "structuredContent": {"echoed": args}}}), flush=True)
        continue
    elif method is not None:
        # notifications/initialized and anything else: no response
        continue
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

/// A server that pushes one server-custom notification right after the
/// `initialize` response, before it answers `tools/list`.
const NOTIFY_SERVER_PY: &str = r#"
import json, sys

TOOLS = [{"name": "echo", "description": "Echo back the arguments",
         "inputSchema": {"type": "object", "properties": {}}}]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25",
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "notify", "version": "0.1"}}
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/custom/ping",
                          "params": {"detail": "custom"}}), flush=True)
        continue
    if method == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                          "result": {"tools": TOOLS}}), flush=True)
        continue
    continue
"#;

/// A server whose tool list grows after a `tools/call`: the reply is followed by
/// `notifications/tools/list_changed`, and the next `tools/list` adds `added`.
const REFRESH_SERVER_PY: &str = r#"
import json, sys

ECHO = {"name": "echo", "description": "Echo back the arguments",
        "inputSchema": {"type": "object", "properties": {}}}
ADDED = {"name": "added", "description": "Appears after list_changed",
         "inputSchema": {"type": "object", "properties": {}}}

announced = False

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "refresh", "version": "0.1"}}}), flush=True)
        continue
    if method == "tools/list":
        tools = [ECHO, ADDED] if announced else [ECHO]
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                          "result": {"tools": tools}}), flush=True)
        continue
    if method == "tools/call":
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                          "result": {"content": [{"type": "text", "text": "ok"}]}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}), flush=True)
        announced = True
        continue
    continue
"#;

fn write_fake_server(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_mcp_server.py");
    let mut file = std::fs::File::create(&path).expect("create fake server script");
    file.write_all(FAKE_SERVER_PY.as_bytes())
        .expect("write fake server script");
    path
}

fn stdio_config(command: &str) -> McpServerConfig {
    McpServerConfig {
        enabled: None,
        timeout: Some(15_000),
        kind: ServerKind::Stdio,
        command: Some(command.to_string()),
        args: None,
        env: None,
        cwd: None,
        url: None,
        headers: None,
        extra: BTreeMap::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_server_roundtrip_and_failure_isolation() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not available; skipping stdio fixture test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_fake_server(dir.path());

    let mut configs = BTreeMap::new();
    configs.insert(
        "fake".to_string(),
        McpServerConfig {
            args: Some(vec![script.to_string_lossy().into_owned()]),
            ..stdio_config("python3")
        },
    );
    configs.insert(
        "dead".to_string(),
        stdio_config("/nonexistent/mcp-server-xyz"),
    );

    let manager = McpManager::new(Arc::new(dir.path().to_path_buf()), configs);

    // tools(): the dead server must not hide the live one's tools, and its
    // failure is recorded rather than propagated.
    let tools = manager.tools().await;
    let names: Vec<&str> = tools.iter().map(|t| t.registry_name.as_str()).collect();
    assert_eq!(names, ["mcp__fake_echo", "mcp__fake_fail"]);
    assert!(manager.failure("dead").await.is_some());
    assert_eq!(manager.failure("fake").await, None);

    // tools/call: echo returns text + structuredContent; the format layer
    // appends the fenced JSON block.
    let result = manager
        .call("fake", "echo", serde_json::json!({"text": "hello"}))
        .await
        .expect("echo call");
    let formatted = format_tool_result(&result);
    assert!(
        formatted.contains("\"text\": \"hello\""),
        "formatted: {formatted}"
    );
    assert!(
        formatted.contains("```json"),
        "structured content block: {formatted}"
    );
    assert!(formatted.contains("\"echoed\""));

    // tools/call: isError results get the `Error: ` prefix.
    let failed = manager
        .call("fake", "fail", serde_json::json!({}))
        .await
        .expect("fail call");
    assert!(failed.is_error);
    assert!(format_tool_result(&failed).starts_with("Error: boom"));

    // Unknown server surfaces a config error, not a panic.
    assert!(manager
        .call("nope", "echo", serde_json::json!({}))
        .await
        .is_err());
}

/// A server-custom notification reaches the listener tagged with the server's
/// raw configured name (not the `mcp__<sanitized>` tool prefix).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_notification_reaches_listener_under_raw_server_name() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not available; skipping notification fixture test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("notify_server.py");
    std::fs::write(&script, NOTIFY_SERVER_PY).expect("write notify server script");

    let mut configs = BTreeMap::new();
    configs.insert(
        "Fake Server.v1".to_string(),
        McpServerConfig {
            args: Some(vec![script.to_string_lossy().into_owned()]),
            ..stdio_config("python3")
        },
    );
    let manager = McpManager::new(Arc::new(dir.path().to_path_buf()), configs);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.add_notification_listener(Arc::new(move |frame: &omp_mcp::ServerNotification| {
        let _ = tx.send(frame.clone());
    }));

    // The listener is subscribed before connecting; the server pushes its
    // notification right after the handshake.
    let tools = manager.tools().await;
    assert_eq!(tools.len(), 1);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("notification delivered within the timeout")
        .expect("listener channel open");
    assert_eq!(frame.server, "Fake Server.v1");
    assert_eq!(frame.method, "notifications/custom/ping");
    assert_eq!(frame.params, serde_json::json!({"detail": "custom"}));
}

/// `notifications/tools/list_changed` is the manager's own handling first: the
/// server's tool list is re-read on the next snapshot, and the frame still
/// reaches listeners (omp `#triggerNotificationRefresh` ahead of the fanout).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_changed_refreshes_tools_before_the_next_snapshot() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not available; skipping list_changed fixture test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("refresh_server.py");
    std::fs::write(&script, REFRESH_SERVER_PY).expect("write refresh server script");

    let mut configs = BTreeMap::new();
    configs.insert(
        "refresh".to_string(),
        McpServerConfig {
            args: Some(vec![script.to_string_lossy().into_owned()]),
            ..stdio_config("python3")
        },
    );
    let manager = McpManager::new(Arc::new(dir.path().to_path_buf()), configs);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.add_notification_listener(Arc::new(move |frame: &omp_mcp::ServerNotification| {
        let _ = tx.send(frame.clone());
    }));

    let names = |tools: Vec<omp_mcp::DiscoveredTool>| -> Vec<String> {
        tools.into_iter().map(|t| t.definition.name).collect()
    };
    assert_eq!(names(manager.tools().await), ["echo"]);

    // The call's reply is followed by `list_changed`; the frame arriving on the
    // listener channel is the barrier, so the next snapshot is deterministic.
    manager
        .call("refresh", "echo", serde_json::json!({}))
        .await
        .expect("echo call");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("list_changed delivered within the timeout")
        .expect("listener channel open");
    assert_eq!(frame.server, "refresh");
    assert_eq!(frame.method, "notifications/tools/list_changed");

    assert_eq!(
        names(manager.tools().await),
        ["echo", "added"],
        "the manager re-listed the server after list_changed"
    );
}

#[test]
fn result_formatting_matches_omp_build_result() {
    // resource blocks and blank-line joining, port of formatMCPContent
    let result = ToolCallResult {
        content: vec![
            Content::Text {
                text: "one".to_string(),
            },
            Content::Resource {
                uri: "file:///a".to_string(),
                text: Some("body".to_string()),
            },
            Content::Text {
                text: "two".to_string(),
            },
        ],
        ..ToolCallResult::default()
    };
    assert_eq!(
        format_tool_result(&result),
        "one\n\n[Resource: file:///a]\nbody\n\ntwo"
    );

    // structuredContent already present verbatim in a text block is not
    // duplicated.
    let dup = ToolCallResult {
        content: vec![Content::Text {
            text: "{\"a\":1}".to_string(),
        }],
        structured_content: Some(serde_json::json!({"a": 1})),
        ..ToolCallResult::default()
    };
    assert_eq!(format_tool_result(&dup), "{\"a\":1}");

    // error without content still yields an `Error:` marker
    let bare = ToolCallResult {
        is_error: true,
        ..ToolCallResult::default()
    };
    assert_eq!(format_tool_result(&bare), "Error:");
}
