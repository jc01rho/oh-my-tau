//! End-to-end RPC conformance golden suite against the real `omt --mode rpc`
//! binary (`env!("CARGO_BIN_EXE_omt")`).
//!
//! Covers the commands, response envelopes (`code` + `errorCode`), and event
//! frames that the OmOMeow gateway (`omo-nyang/src`) sends and observes:
//! - `get_protocol_info`, `set_client_info`, `interrupt` (`stop` / `abort`),
//!   `reload`, `resume_session`, `extension_request`, `get_state`,
//!   `get_available_models`, `get_available_thinking_levels`,
//!   `get_session_stats`, `get_last_assistant_text`, `prompt` -> `agent_settled`
//! - `session_paused` / `session_resumed` lifecycle and error codes
//!
//! Uses an in-process mock OpenAI-compatible HTTP server and condition-variable
//! frame waiting (no `sleep`). Set `UPDATE_RPC_GOLDEN=1` to regenerate golden
//! files under `crates/omp-cli/tests/golden/rpc/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/rpc")
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn omt_bin() -> PathBuf {
    std::env::var_os("OMT_CONFORMANCE_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_omt")))
}

fn bun_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("bun");
        candidate.is_file()
    })
}

struct MockServer {
    base_url: String,
    hold_gate: Arc<(Mutex<bool>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on mock listener");
        let port = listener.local_addr().expect("local_addr").port();
        let hold_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let gate_bg = Arc::clone(&hold_gate);
        let shut_bg = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !shut_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let gate = Arc::clone(&gate_bg);
                        let shut = Arc::clone(&shut_bg);
                        std::thread::spawn(move || {
                            handle_http(stream, &gate, &shut);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::park_timeout(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            hold_gate,
            shutdown,
            handle: Some(handle),
        }
    }

    fn release_hold(&self) {
        let (lock, cv) = &*self.hold_gate;
        let mut released = lock.lock().expect("hold_gate lock");
        *released = true;
        cv.notify_all();
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.release_hold();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }

    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let mut first_line = headers.lines();
    let req_line = first_line.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body_bytes = &buf[header_end..buf.len().min(header_end + content_length)];
    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body_bytes).unwrap_or(Value::Null)
    };
    Some((method, path, body))
}

fn send_json(stream: &mut TcpStream, status: &str, body: &Value) {
    let payload = serde_json::to_string(body).expect("serialize json");
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.flush();
}

fn sse_chunk(model: &str, delta: &Value, finish_reason: Option<&str>) -> String {
    let mut payload = json!({
        "id": "chatcmpl-mock-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    });
    if finish_reason.is_some() {
        payload["usage"] = json!({
            "prompt_tokens": 100,
            "completion_tokens": 4,
            "total_tokens": 104,
        });
    }
    format!("data: {payload}\n\n")
}

fn handle_http(mut stream: TcpStream, hold_gate: &(Mutex<bool>, Condvar), shutdown: &AtomicBool) {
    let Some((method, path, body)) = read_http_request(&mut stream) else {
        return;
    };
    if method == "GET" && path.ends_with("/models") {
        send_json(
            &mut stream,
            "200 OK",
            &json!({
                "object": "list",
                "data": [
                    {"id": "mock/text", "object": "model", "created": 0, "owned_by": "mock"},
                    {"id": "mock/hold", "object": "model", "created": 0, "owned_by": "mock"},
                ]
            }),
        );
        return;
    }
    if method != "POST" || !path.ends_with("/chat/completions") {
        send_json(&mut stream, "404 Not Found", &json!({"error": "not found"}));
        return;
    }

    let model = body["model"].as_str().unwrap_or("text").to_string();
    let model_id = model.strip_prefix("mock/").unwrap_or(&model);

    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }

    if model_id == "hold" {
        let first = sse_chunk(
            &model,
            &json!({"role": "assistant", "content": "HOLD-START "}),
            None,
        );
        if stream.write_all(first.as_bytes()).is_err() || stream.flush().is_err() {
            return;
        }
        let (lock, cv) = hold_gate;
        let guard = lock.lock().expect("hold_gate lock");
        let _ = cv.wait_timeout_while(guard, Duration::from_secs(15), |released| {
            !*released && !shutdown.load(Ordering::Relaxed)
        });
        let end = sse_chunk(&model, &json!({"content": "HOLD-END"}), Some("stop"));
        let _ = stream.write_all(end.as_bytes());
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
        return;
    }

    let c1 = sse_chunk(
        &model,
        &json!({"role": "assistant", "content": "MOCK-"}),
        None,
    );
    let c2 = sse_chunk(&model, &json!({"content": "RPC-OK"}), None);
    let c3 = sse_chunk(&model, &json!({}), Some("stop"));
    for chunk in [c1, c2, c3, "data: [DONE]\n\n".to_string()] {
        if stream.write_all(chunk.as_bytes()).is_err() {
            return;
        }
    }
    let _ = stream.flush();
}

struct FrameInbox {
    frames: Vec<Value>,
    closed: bool,
}

struct RpcHarness {
    _home: tempfile::TempDir,
    _mock: MockServer,
    child: Child,
    stdin: Option<ChildStdin>,
    inbox: Arc<(Mutex<FrameInbox>, Condvar)>,
    stderr: Arc<Mutex<String>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    transcript: Vec<Value>,
    seq: usize,
}

impl RpcHarness {
    fn spawn(extra_args: &[&str]) -> Self {
        let mock = MockServer::start();
        let home = tempfile::tempdir().expect("tempdir");
        let agent = home.path().join(".omp/agent");
        let work = home.path().join("work");
        std::fs::create_dir_all(&agent).expect("create agent dir");
        std::fs::create_dir_all(&work).expect("create work dir");
        std::fs::write(
            agent.join("models.yml"),
            format!(
                "providers:\n  mock:\n    baseUrl: {}\n    apiKey: e2e-not-a-secret\n    api: openai-completions\n    compat:\n      supportsDeveloperRole: true\n    models:\n      - id: text\n        name: Mock Text\n        contextWindow: 128000\n        maxTokens: 4096\n      - id: hold\n        name: Mock Hold\n        contextWindow: 128000\n        maxTokens: 4096\n",
                mock.base_url
            ),
        )
        .expect("write models.yml");
        std::fs::write(
            agent.join("config.yml"),
            "startup:\n  setupWizard: false\n  checkUpdate: false\n  changelogMode: hidden\n",
        )
        .expect("write config.yml");

        let session_path = home.path().join("lanes/lane.jsonl");
        let session_arg = session_path.display().to_string();

        let host_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../ext-host/host.ts")
            .canonicalize()
            .ok();

        let mut cmd = Command::new(omt_bin());
        cmd.env("HOME", home.path())
            .env_remove("OMP_AGENT_DIR")
            .env_remove("OMP_AUTH_BROKER_URL")
            .env_remove("OMP_AUTH_BROKER_TOKEN")
            .env_remove("OMO_PROFILE")
            .env_remove("OMP_PROFILE")
            .env_remove("XDG_RUNTIME_DIR")
            .env("OMP_RS_EXT_SHARED", "0")
            .env("OMP_RS_NO_TITLE", "1")
            .env("NO_COLOR", "1")
            .current_dir(&work)
            .args([
                "--mode",
                "rpc",
                "--session",
                &session_arg,
                "--model",
                "mock/text",
            ])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(script) = host_script {
            cmd.env("OMP_RS_EXT_HOST", script);
        }

        let mut child = cmd.spawn().expect("spawn omt --mode rpc");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr_pipe = child.stderr.take().expect("stderr");

        let inbox = Arc::new((
            Mutex::new(FrameInbox {
                frames: Vec::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let stderr = Arc::new(Mutex::new(String::new()));

        let inbox_bg = Arc::clone(&inbox);
        let stdout_thread = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let frame = serde_json::from_str::<Value>(trimmed).unwrap_or_else(
                    |e| json!({"type": "__unparsed", "raw": trimmed, "error": e.to_string()}),
                );
                let (lock, cv) = &*inbox_bg;
                let mut state = lock.lock().expect("inbox lock");
                state.frames.push(frame);
                cv.notify_all();
            }
            let (lock, cv) = &*inbox_bg;
            let mut state = lock.lock().expect("inbox lock");
            state.closed = true;
            cv.notify_all();
        });

        let stderr_bg = Arc::clone(&stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        stderr_bg.lock().expect("stderr lock").push_str(&line);
                    }
                }
            }
        });

        let h = Self {
            _home: home,
            _mock: mock,
            child,
            stdin: Some(stdin),
            inbox,
            stderr,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            transcript: Vec::new(),
            seq: 0,
        };
        let ready = h.expect_frame(0, |f| f["type"] == "ready", FRAME_TIMEOUT);
        assert_eq!(
            ready["protocolVersion"], 1,
            "unexpected ready frame: {ready}"
        );
        h
    }

    fn current_len(&self) -> usize {
        self.inbox.0.lock().expect("inbox lock").frames.len()
    }

    fn record_out(&mut self, frame: &Value) {
        self.transcript.push(json!({"dir": "out", "frame": frame}));
    }

    fn send(&mut self, mut cmd: Value) -> (String, usize) {
        self.seq += 1;
        let id = format!("gw-{}", self.seq);
        if cmd.get("id").is_none() {
            cmd["id"] = json!(id);
        }
        let assigned_id = cmd["id"].as_str().unwrap_or(&id).to_string();
        let from = self.current_len();
        self.record_out(&cmd);
        let line = format!("{cmd}\n");
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).expect("write cmd");
        stdin.flush().expect("flush cmd");
        (assigned_id, from)
    }

    fn send_raw_line(&mut self, raw: &str) -> usize {
        let from = self.current_len();
        self.record_out(&json!({"__raw": raw}));
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin
            .write_all(format!("{raw}\n").as_bytes())
            .expect("write raw");
        stdin.flush().expect("flush raw");
        from
    }

    fn expect_frame(&self, from: usize, pred: impl Fn(&Value) -> bool, timeout: Duration) -> Value {
        let (lock, cv) = &*self.inbox;
        let deadline = Instant::now() + timeout;
        let mut cursor = from;
        let mut guard = lock.lock().expect("inbox lock");
        loop {
            while cursor < guard.frames.len() {
                let frame = &guard.frames[cursor];
                cursor += 1;
                if pred(frame) {
                    return frame.clone();
                }
            }
            if guard.closed {
                let stderr = self.stderr.lock().expect("stderr lock").clone();
                panic!(
                    "rpc process exited before matching frame arrived.\nframes: {:#?}\nstderr: {stderr}",
                    guard.frames
                );
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                let stderr = self.stderr.lock().expect("stderr lock").clone();
                panic!(
                    "timed out waiting for frame after {timeout:?} (from={from}).\nframes: {:#?}\nstderr: {stderr}",
                    guard.frames
                );
            };
            let (next_guard, _) = cv.wait_timeout(guard, remaining).expect("condvar wait");
            guard = next_guard;
        }
    }

    fn expect_response(&self, id: &str, from: usize) -> Value {
        self.expect_frame(
            from,
            |f| f["type"] == "response" && f["id"] == id,
            FRAME_TIMEOUT,
        )
    }

    fn request(&mut self, cmd: Value) -> (Value, usize) {
        let (id, from) = self.send(cmd);
        let resp = self.expect_response(&id, from);
        self.transcript.push(json!({"dir": "in", "frame": resp}));
        (resp, from)
    }

    fn release_hold(&self) {
        self._mock.release_hold();
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    fn finish(mut self) -> Vec<Value> {
        self._mock.release_hold();
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait omt");
        if let Some(h) = self.stdout_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_thread.take() {
            let _ = h.join();
        }
        let stderr = self.stderr.lock().expect("stderr lock").clone();
        assert!(
            status.success(),
            "omt exited with {status}\nstderr: {stderr}"
        );
        self.inbox.0.lock().expect("inbox lock").frames.clone()
    }
}

impl Drop for RpcHarness {
    fn drop(&mut self) {
        self._mock.release_hold();
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.stdout_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_thread.take() {
            let _ = h.join();
        }
    }
}

fn normalize_frame(v: &Value) -> Value {
    let mut out = v.clone();
    normalize_in_place(&mut out);
    out
}

fn normalize_in_place(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let frame_type = map.get("type").and_then(Value::as_str).map(str::to_string);
            let command = map
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string);

            if frame_type.as_deref() == Some("available_commands_update") {
                if let Some(cmds) = map.get_mut("commands") {
                    *cmds = json!("<COMMANDS>");
                }
            }

            if frame_type.as_deref() == Some("response")
                && command.as_deref() == Some("get_protocol_info")
            {
                if let Some(Value::Object(data)) = map.get_mut("data") {
                    if data.contains_key("serverVersion") {
                        data.insert("serverVersion".into(), json!("<SERVER_VERSION>"));
                    }
                    for (key, placeholder) in
                        [("agentDir", "<AGENT_DIR>"), ("sessionId", "<SESSION_ID>")]
                    {
                        if data.contains_key(key) {
                            data.insert(key.into(), json!(placeholder));
                        }
                    }
                    if data.contains_key("capabilities") {
                        data.insert("capabilities".into(), json!("<RPC_COMMANDS>"));
                    }
                    if data.contains_key("commands") {
                        data.insert("commands".into(), json!("<RPC_COMMANDS>"));
                    }
                }
            }

            if frame_type.as_deref() == Some("response") && command.as_deref() == Some("get_state")
            {
                if let Some(Value::Object(data)) = map.get_mut("data") {
                    if data.contains_key("systemPrompt") {
                        data.insert("systemPrompt".into(), json!(["<SYSTEM_PROMPT>"]));
                    }
                    if data.contains_key("dumpTools") {
                        data.insert("dumpTools".into(), json!("<TOOLS_LIST>"));
                    }
                }
            }

            if frame_type.as_deref() == Some("response")
                && command.as_deref() == Some("get_available_models")
            {
                if let Some(Value::Object(data)) = map.get_mut("data") {
                    if let Some(arr) = data.get("models").and_then(Value::as_array) {
                        let mut mock: Vec<String> = arr
                            .iter()
                            .filter(|m| m["provider"] == "mock")
                            .map(|m| format!("mock/{}", m["id"].as_str().unwrap_or("?")))
                            .collect();
                        mock.sort();
                        data.insert("models".into(), json!(mock));
                    }
                }
            }

            for (k, val) in map.iter_mut() {
                match k.as_str() {
                    "sessionId" => *val = json!("<SESSION_ID>"),
                    "sessionFile" => *val = json!("<SESSION_FILE>"),
                    "baseUrl" => *val = json!("<MOCK_BASE_URL>"),
                    "contextUsage" => *val = json!("<CONTEXT_USAGE>"),
                    "at" | "since" | "timestamp" => *val = json!("<TIMESTAMP>"),
                    "duration" | "ttft" => *val = json!("<DURATION_MS>"),
                    "usage" if val.is_object() => {
                        *val = json!("<USAGE>");
                    }
                    _ => normalize_in_place(val),
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                normalize_in_place(item);
            }
        }
        _ => {}
    }
}

fn assert_golden(name: &str, rows: &[Value]) {
    let normalized: Vec<Value> = rows.iter().map(normalize_frame).collect();
    let actual_text = normalized
        .iter()
        .map(|v| serde_json::to_string(v).expect("serialize row"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let path = golden_dir().join(format!("{name}.jsonl"));
    if std::env::var_os("UPDATE_RPC_GOLDEN").is_some_and(|v| v != "0") {
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        std::fs::write(&path, &actual_text).expect("write golden");
        return;
    }

    let expected_text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\nRun with UPDATE_RPC_GOLDEN=1 to generate.\nActual:\n{actual_text}",
            path.display()
        )
    });
    assert_eq!(
        actual_text,
        expected_text,
        "golden mismatch for {name} ({})",
        path.display()
    );
}

#[test]
fn golden_handshake_and_protocol_info() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);
    let ready = h.expect_frame(0, |f| f["type"] == "ready", FRAME_TIMEOUT);

    let (proto, _) = h.request(json!({"type": "get_protocol_info"}));
    assert_eq!(proto["success"], true);
    assert_eq!(proto["data"]["protocolVersion"], 2);
    assert_eq!(proto["data"]["engine"], "oh-my-tau");
    assert_eq!(proto["data"]["mode"], "classic");
    assert!(proto["data"]["serverVersion"].is_string());
    assert!(proto["data"]["agentDir"].is_string());
    assert!(proto["data"]["sessionId"].is_string());

    let cmds: Vec<&str> = proto["data"]["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in [
        "get_protocol_info",
        "set_client_info",
        "interrupt",
        "reload",
        "resume_session",
        "extension_request",
        "get_state",
        "prompt",
        "abort",
        "steer",
        "follow_up",
        "get_last_assistant_text",
        "get_session_stats",
        "get_available_models",
        "get_available_thinking_levels",
        "set_model",
        "set_thinking_level",
    ] {
        assert!(
            cmds.contains(&required),
            "missing command `{required}` in {cmds:?}"
        );
    }
    assert_eq!(proto["data"]["capabilities"], proto["data"]["commands"]);

    let (state, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state["success"], true);
    assert_eq!(state["data"]["isStreaming"], false);
    assert_eq!(state["data"]["isCompacting"], false);
    assert_eq!(state["data"]["paused"], false);
    assert_eq!(state["data"]["runPaused"], false);
    assert_eq!(state["data"]["pauseSeq"], 0);
    assert!(state["data"]["sessionId"].is_string());
    assert!(state["data"]["sessionFile"].is_string());

    let (models, _) = h.request(json!({"type": "get_available_models"}));
    assert_eq!(models["success"], true);

    let (levels, _) = h.request(json!({"type": "get_available_thinking_levels"}));
    assert_eq!(levels["success"], true);

    let (stats, _) = h.request(json!({"type": "get_session_stats"}));
    assert_eq!(stats["success"], true);

    let (last, _) = h.request(json!({"type": "get_last_assistant_text"}));
    assert_eq!(last["success"], true);
    assert_eq!(last["data"]["text"], Value::Null);

    let mut rows = vec![json!({"dir": "in", "frame": ready})];
    rows.extend(h.transcript.clone());
    h.finish();
    assert_golden("handshake_and_protocol", &rows);
}

#[test]
fn golden_lane_hold_stop_reload_resume() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);

    let (stop_unauth, _) = h.request(json!({"type": "interrupt", "intent": "stop"}));
    assert_eq!(stop_unauth["success"], false);
    assert_eq!(stop_unauth["code"], "stop_requires_interactive_client");
    assert_eq!(stop_unauth["errorCode"], "stop_requires_interactive_client");

    let (sci, _) = h.request(json!({
        "type": "set_client_info",
        "width": 80,
        "capabilities": ["interactive_user"],
    }));
    assert_eq!(sci["success"], true);

    let (role_flip, _) = h.request(json!({
        "type": "set_client_info",
        "width": 120,
        "capabilities": ["task_parent"],
    }));
    assert_eq!(role_flip["success"], false);
    assert_eq!(role_flip["code"], "invalid_params");
    assert_eq!(role_flip["errorCode"], "invalid_params");
    assert!(
        role_flip["error"]
            .as_str()
            .is_some_and(|s| s.starts_with("connection_role_fixed")),
        "{role_flip}"
    );

    let (stop_ok, from_stop) = h.request(json!({"type": "interrupt", "intent": "stop"}));
    assert_eq!(stop_ok["success"], true);
    let paused_evt = h.expect_frame(from_stop, |f| f["type"] == "session_paused", FRAME_TIMEOUT);
    assert_eq!(paused_evt["pauseSeq"], 1);
    assert_eq!(paused_evt["kind"], "session_hold");
    assert_eq!(paused_evt["persisted"], true);
    h.transcript.push(json!({"dir": "in", "frame": paused_evt}));

    let (state_paused, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state_paused["data"]["paused"], true);
    assert_eq!(state_paused["data"]["runPaused"], false);
    assert_eq!(state_paused["data"]["pauseSeq"], 1);

    let (reloaded, _) = h.request(json!({"type": "reload"}));
    assert_eq!(reloaded["success"], true);
    assert_eq!(reloaded["data"]["cancelled"], false);

    let (state_after_reload, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state_after_reload["data"]["paused"], true);
    assert_eq!(state_after_reload["data"]["pauseSeq"], 1);

    let (resumed, from_resume) = h.request(json!({"type": "resume_session"}));
    assert_eq!(resumed["success"], true);
    let resumed_evt = h.expect_frame(
        from_resume,
        |f| f["type"] == "session_resumed",
        FRAME_TIMEOUT,
    );
    assert_eq!(resumed_evt["pauseSeq"], 2);
    assert_eq!(resumed_evt["cause"], "resume_session");
    h.transcript
        .push(json!({"dir": "in", "frame": resumed_evt}));

    let (state_resumed, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state_resumed["data"]["paused"], false);
    assert_eq!(state_resumed["data"]["runPaused"], false);
    assert_eq!(state_resumed["data"]["pauseSeq"], 2);

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("lane_hold_stop_reload_resume", &rows);
}

#[test]
fn golden_prompt_settled_and_read() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);

    let (_prompt_resp, from_prompt) = h.request(json!({
        "type": "prompt",
        "message": "Reply deterministically",
        "streamingBehavior": "followUp",
        "clientMessageId": "owner-msg-1",
    }));

    let settled = h.expect_frame(from_prompt, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);

    let frames_snapshot = h.inbox.0.lock().expect("inbox lock").frames.clone();
    let mut saw_agent_start = false;
    let mut saw_agent_end = false;
    let mut saw_prompt_result = false;
    let mut saw_session_settled = false;
    for f in frames_snapshot.iter().skip(from_prompt) {
        let ty = f["type"].as_str().unwrap_or_default();
        if matches!(
            ty,
            "agent_start"
                | "turn_start"
                | "turn_end"
                | "agent_end"
                | "prompt_result"
                | "session_settled"
                | "agent_settled"
        ) {
            h.transcript.push(json!({"dir": "in", "frame": f}));
        }
        match ty {
            "agent_start" => saw_agent_start = true,
            "agent_end" => saw_agent_end = true,
            "prompt_result" => {
                saw_prompt_result = true;
                assert_eq!(f["status"], "completed");
                assert_eq!(f["sessionSettled"], true);
            }
            "session_settled" => saw_session_settled = true,
            _ => {}
        }
    }
    assert!(saw_agent_start, "missing agent_start");
    assert!(saw_agent_end, "missing agent_end");
    assert!(saw_prompt_result, "missing prompt_result");
    assert!(saw_session_settled, "missing session_settled");
    assert_eq!(settled["type"], "agent_settled");

    let (last, _) = h.request(json!({"type": "get_last_assistant_text"}));
    assert_eq!(last["success"], true);
    assert_eq!(last["data"]["text"], "MOCK-RPC-OK");

    let (stats, _) = h.request(json!({"type": "get_session_stats"}));
    assert_eq!(stats["success"], true);
    assert_eq!(stats["data"]["totalMessages"], 2);

    let (state, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state["data"]["isStreaming"], false);
    assert_eq!(state["data"]["isSettled"], true);

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("prompt_settled_and_read", &rows);
}

#[test]
fn golden_interrupt_abort_and_stop_mid_turn() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);

    let (sci, _) = h.request(json!({
        "type": "set_client_info",
        "width": 80,
        "capabilities": ["interactive_user"],
    }));
    assert_eq!(sci["success"], true);

    let (sm, _) = h.request(json!({
        "type": "set_model",
        "provider": "mock",
        "modelId": "hold",
    }));
    assert_eq!(sm["success"], true);

    let (_p1, from_p1) = h.request(json!({
        "type": "prompt",
        "message": "Hold turn 1",
        "streamingBehavior": "followUp",
    }));
    let _start1 = h.expect_frame(from_p1, |f| f["type"] == "agent_start", FRAME_TIMEOUT);

    let (abort_resp, from_abort) = h.request(json!({"type": "interrupt"}));
    assert_eq!(abort_resp["success"], true);
    let end1 = h.expect_frame(from_abort, |f| f["type"] == "agent_end", FRAME_TIMEOUT);
    let settled1 = h.expect_frame(from_abort, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    h.transcript.push(json!({"dir": "in", "frame": end1}));
    h.transcript.push(json!({"dir": "in", "frame": settled1}));

    let (state_after_abort, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state_after_abort["data"]["paused"], false);
    assert_eq!(state_after_abort["data"]["isStreaming"], false);

    let (_p2, from_p2) = h.request(json!({
        "type": "prompt",
        "message": "Hold turn 2",
        "streamingBehavior": "followUp",
    }));
    let _start2 = h.expect_frame(from_p2, |f| f["type"] == "agent_start", FRAME_TIMEOUT);

    let (stop_resp, from_stop) = h.request(json!({"type": "interrupt", "intent": "stop"}));
    assert_eq!(stop_resp["success"], true);
    let paused = h.expect_frame(from_stop, |f| f["type"] == "session_paused", FRAME_TIMEOUT);
    let settled2 = h.expect_frame(from_stop, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    h.transcript.push(json!({"dir": "in", "frame": paused}));
    h.transcript.push(json!({"dir": "in", "frame": settled2}));

    let (state_after_stop, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(state_after_stop["data"]["paused"], true);
    assert_eq!(state_after_stop["data"]["pauseSeq"], 1);

    let (resumed, from_res) = h.request(json!({"type": "resume_session"}));
    assert_eq!(resumed["success"], true);
    let resumed_evt = h.expect_frame(from_res, |f| f["type"] == "session_resumed", FRAME_TIMEOUT);
    h.transcript
        .push(json!({"dir": "in", "frame": resumed_evt}));

    h.release_hold();
    let rows = h.transcript.clone();
    h.finish();
    assert_golden("interrupt_abort_mid_turn", &rows);
}

#[test]
fn golden_error_codes_and_extension_request() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);

    let (unk, _) = h.request(json!({"type": "no_such_gateway_cmd"}));
    assert_eq!(unk["success"], false);
    assert_eq!(unk["code"], "unknown_command");
    assert_eq!(unk["errorCode"], "unknown_command");

    let (bad_sci, _) = h.request(json!({"type": "set_client_info", "capabilities": []}));
    assert_eq!(bad_sci["success"], false);
    assert_eq!(bad_sci["code"], "invalid_params");
    assert_eq!(bad_sci["errorCode"], "invalid_params");

    let (bad_int, _) = h.request(json!({"type": "interrupt", "intent": "bogus"}));
    assert_eq!(bad_int["success"], false);
    assert_eq!(bad_int["code"], "invalid_params");
    assert_eq!(bad_int["errorCode"], "invalid_params");

    let (empty_ext, _) = h.request(json!({"type": "extension_request", "name": "   "}));
    assert_eq!(empty_ext["success"], false);
    assert_eq!(empty_ext["code"], "invalid_params");
    assert_eq!(empty_ext["errorCode"], "invalid_params");

    let (no_ext, _) = h.request(json!({
        "type": "extension_request",
        "name": "omomeow.move-readiness",
    }));
    assert_eq!(no_ext["success"], false);
    assert_eq!(no_ext["code"], "extension_unavailable");
    assert_eq!(no_ext["errorCode"], "extension_unavailable");

    let from_bad = h.send_raw_line("{not-json");
    let parse_err = h.expect_frame(
        from_bad,
        |f| f["type"] == "response" && f["command"] == "parse",
        FRAME_TIMEOUT,
    );
    assert_eq!(parse_err["success"], false);
    assert_eq!(parse_err["code"], "rpc_failed");
    assert_eq!(parse_err["errorCode"], "rpc_failed");
    h.transcript.push(json!({"dir": "in", "frame": parse_err}));

    let (alive, _) = h.request(json!({"type": "get_state"}));
    assert_eq!(alive["success"], true);

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("error_codes_and_no_ext", &rows);

    if !bun_on_path() {
        eprintln!("skipping live extension_request golden check: bun not found on PATH");
        return;
    }
    let ext_file = fixture_path("rpc_ext.ts");
    let ext_arg = ext_file.display().to_string();
    let mut h_ext = RpcHarness::spawn(&["-e", &ext_arg]);

    let (move_ready, _) = h_ext.request(json!({
        "type": "extension_request",
        "name": "omomeow.move-readiness",
    }));
    let ext_stderr = h_ext.stderr_text();
    assert_eq!(
        move_ready["success"], true,
        "{move_ready}\nstderr: {ext_stderr}"
    );
    assert_eq!(move_ready["data"], json!({"activeSources": []}));

    let (feedback, _) = h_ext.request(json!({
        "type": "extension_request",
        "name": "omo.feedback.prepare",
        "data": {},
    }));
    assert_eq!(feedback["success"], true, "{feedback}");
    assert_eq!(feedback["data"]["kind"], "unavailable");

    let (unknown_ext, _) = h_ext.request(json!({
        "type": "extension_request",
        "name": "omomeow.missing",
    }));
    assert_eq!(unknown_ext["success"], false);
    assert_eq!(unknown_ext["code"], "unknown_extension_request");
    assert_eq!(unknown_ext["errorCode"], "unknown_extension_request");

    let ext_rows = h_ext.transcript.clone();
    h_ext.finish();
    assert_golden("extension_request_live", &ext_rows);
}

#[test]
fn golden_subagent_rpc_contract() {
    let mut h = RpcHarness::spawn(&["--no-extensions"]);

    let (list, _) = h.request(json!({"type": "get_subagents"}));
    assert_eq!(list["success"], true);
    assert_eq!(list["data"]["subagents"], json!([]));

    let (cancel_missing, _) = h.request(json!({
        "type": "cancel_subagent",
        "subagentId": "st_000000000000",
    }));
    assert_eq!(cancel_missing["success"], false);
    assert_eq!(cancel_missing["code"], "subagent_not_found");
    assert_eq!(cancel_missing["errorCode"], "subagent_not_found");

    let (steer_missing, _) = h.request(json!({
        "type": "steer_subagent",
        "subagentId": "st_000000000000",
        "message": "ping",
    }));
    assert_eq!(steer_missing["success"], false);
    assert_eq!(steer_missing["code"], "subagent_not_found");
    assert_eq!(steer_missing["errorCode"], "subagent_not_found");

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("subagent_rpc_contract", &rows);
}
