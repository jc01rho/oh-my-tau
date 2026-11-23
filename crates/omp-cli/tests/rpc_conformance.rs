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
    /// Every chat-completions body the provider received, in arrival order.
    requests: Arc<Mutex<Vec<Value>>>,
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
        let requests = Arc::new(Mutex::new(Vec::new()));

        let gate_bg = Arc::clone(&hold_gate);
        let shut_bg = Arc::clone(&shutdown);
        let requests_bg = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            while !shut_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let gate = Arc::clone(&gate_bg);
                        let shut = Arc::clone(&shut_bg);
                        let seen = Arc::clone(&requests_bg);
                        std::thread::spawn(move || {
                            handle_http(stream, &gate, &shut, &seen);
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
            requests,
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

/// The watcher the mock provider starts when a prompt asks for it: it ends once the test creates
/// the gate file named by the `G12C_GATE` environment variable of the `omt` process.
const MONITOR_TRIGGER: &str = "[[run-monitor]]";
const MONITOR_COMMAND: &str = "until [ -e \"$G12C_GATE\" ]; do sleep 0.05; done";

/// A request asks for the watcher while no tool result is in its history yet.
fn wants_monitor(body: &Value) -> bool {
    let messages = body["messages"].as_array().map_or(&[][..], Vec::as_slice);
    messages
        .iter()
        .any(|m| m["role"] == "user" && m["content"].to_string().contains(MONITOR_TRIGGER))
        && !messages.iter().any(|m| m["role"] == "tool")
}

fn handle_http(
    mut stream: TcpStream,
    hold_gate: &(Mutex<bool>, Condvar),
    shutdown: &AtomicBool,
    requests: &Mutex<Vec<Value>>,
) {
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

    requests.lock().expect("requests lock").push(body.clone());
    let model = body["model"].as_str().unwrap_or("text").to_string();
    let model_id = model.strip_prefix("mock/").unwrap_or(&model);

    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }

    if wants_monitor(&body) {
        let arguments = json!({
            "description": "g12c watcher",
            "command": MONITOR_COMMAND,
            "filter": "^NEVER-MATCHES$",
            "timeout_ms": 120_000,
        })
        .to_string();
        let call = sse_chunk(
            &model,
            &json!({"role": "assistant", "tool_calls": [{
                "index": 0,
                "id": "call_monitor_1",
                "type": "function",
                "function": {"name": "monitor", "arguments": arguments},
            }]}),
            None,
        );
        let end = sse_chunk(&model, &json!({}), Some("tool_calls"));
        for chunk in [call, end, "data: [DONE]\n\n".to_string()] {
            if stream.write_all(chunk.as_bytes()).is_err() {
                return;
            }
        }
        let _ = stream.flush();
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
    home_dir: PathBuf,
    work_dir: PathBuf,
    _mock: MockServer,
    child: Child,
    stdin: Option<ChildStdin>,
    inbox: Arc<(Mutex<FrameInbox>, Condvar)>,
    stderr: Arc<Mutex<String>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    transcript: Vec<Value>,
    seq: usize,
    quiet: usize,
}

/// Flags of a multi-session host the way the gateway's `MoveHost` starts one.
const HOST_ARGS: &[&str] = &[
    "--multi-session",
    "--session-runtime",
    "in-process",
    "--no-extensions",
];

impl RpcHarness {
    fn spawn(extra_args: &[&str]) -> Self {
        Self::spawn_with(true, extra_args)
    }

    /// A multi-session host (`--mode rpc --multi-session --session-runtime in-process`): every
    /// session is opened by the client, so there is no `--session` file.
    fn spawn_host() -> Self {
        Self::spawn_with(false, HOST_ARGS)
    }

    fn spawn_with(single_session: bool, extra_args: &[&str]) -> Self {
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
            .env("G12C_GATE", home.path().join("monitor.gate"))
            .current_dir(&work)
            .args(["--mode", "rpc"])
            .args(if single_session {
                vec!["--session", session_arg.as_str()]
            } else {
                Vec::new()
            })
            .args(["--model", "mock/text"])
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
            home_dir: home.path().to_path_buf(),
            work_dir: work,
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
            quiet: 0,
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
        if cmd.get("id").is_none() {
            self.seq += 1;
            cmd["id"] = json!(format!("gw-{}", self.seq));
        }
        let assigned_id = cmd["id"].as_str().unwrap_or_default().to_string();
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

    /// Add a received frame to the transcript.
    fn record_in(&mut self, frame: Value) {
        self.transcript.push(json!({"dir": "in", "frame": frame}));
    }

    /// `request` for a command routed to the hosted session `session_id`.
    fn request_in(&mut self, session_id: &str, mut cmd: Value) -> (Value, usize) {
        cmd["sessionId"] = json!(session_id);
        self.request(cmd)
    }

    /// Wait for a frame the host stamped with the routing id `session_id`.
    fn expect_tagged(&self, from: usize, session_id: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.expect_frame(
            from,
            |f| f["sessionId"] == session_id && pred(f),
            FRAME_TIMEOUT,
        )
    }

    /// Every frame received since `from` (the snapshot the transcripts filter).
    fn frames_since(&self, from: usize) -> Vec<Value> {
        self.inbox.0.lock().expect("inbox lock").frames[from..].to_vec()
    }

    /// Ask the host whether it still holds `session_id`, without a transcript row: a released
    /// session answers before it leaves the host's registry, and a move's export or finish is
    /// refused while the host still holds the file.
    fn is_open(&mut self, session_id: &str) -> bool {
        self.quiet += 1;
        let id = format!("quiet-{}", self.quiet);
        let line = format!("{}\n", json!({"type": "list_sessions", "id": id}));
        let from = self.current_len();
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin
            .write_all(line.as_bytes())
            .expect("write list_sessions");
        stdin.flush().expect("flush list_sessions");
        let listed = self.expect_frame(
            from,
            |f| f["type"] == "response" && f["id"] == id.as_str(),
            FRAME_TIMEOUT,
        );
        listed["data"]["sessions"]
            .as_array()
            .is_some_and(|all| all.iter().any(|s| s["sessionId"] == session_id))
    }

    /// Wait until the host dropped a released or closed session.
    fn wait_until_gone(&mut self, session_id: &str) {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        while self.is_open(session_id) {
            assert!(
                Instant::now() < deadline,
                "session {session_id} never left the host"
            );
        }
    }

    /// A session file under this host's temp home.
    fn lane_file(&self, name: &str) -> String {
        self.home_dir.join("lanes").join(name).display().to_string()
    }

    /// `open_session` the way the gateway's move host sends it; the `data` of the answer.
    fn open_session(&mut self, name: &str) -> Value {
        let (opened, _) = self.request(json!({
            "type": "open_session",
            "sessionPath": self.lane_file(name),
            "cwd": self.work_dir.display().to_string(),
            "attended": false,
        }));
        assert_eq!(opened["success"], true, "{opened}");
        opened["data"].clone()
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

            if frame_type.as_deref() == Some("ready") && map.contains_key("capabilities") {
                map.insert("capabilities".into(), json!("<RPC_COMMANDS>"));
            }

            if frame_type.as_deref() == Some("response")
                && command.as_deref() == Some("list_sessions")
            {
                // The host lists sessions in map order: sort so the golden is stable.
                if let Some(Value::Array(sessions)) =
                    map.get_mut("data").and_then(|d| d.get_mut("sessions"))
                {
                    sessions
                        .sort_by_key(|s| s["sessionId"].as_str().unwrap_or_default().to_owned());
                }
            }

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
                    // The host's routing ids (`rpc-N`) are deterministic and are what the
                    // routing goldens pin; the durable session ids are not.
                    "sessionId" if val.as_str().is_some_and(|id| id.starts_with("rpc-")) => {}
                    "sessionId" | "durableSessionId" | "session_id" => {
                        *val = json!("<SESSION_ID>");
                    }
                    "sessionPath" | "session_path" => *val = json!("<SESSION_PATH>"),
                    "bundleDir" | "bundle_dir" => *val = json!("<BUNDLE_DIR>"),
                    "cwd" => *val = json!("<CWD>"),
                    "released_at" => *val = json!("<TIMESTAMP>"),
                    "error" if val.is_string() => {
                        // Messages may name a temp path: keep the words, drop the path.
                        let mut text = val.as_str().unwrap_or_default().to_owned();
                        // The busy signals include "request" while the last reply is still being
                        // counted off; the refusal code already says what runs.
                        if let Some((head, rest)) = text.split_once("the session is busy (") {
                            if let Some((_, tail)) = rest.split_once(')') {
                                text = format!("{head}the session is busy (<SIGNALS>){tail}");
                            }
                        }
                        *val = json!(text
                            .split(' ')
                            .map(|w| if w.starts_with('/') { "<PATH>" } else { w })
                            .collect::<Vec<_>>()
                            .join(" "));
                    }
                    "sessionFile" => *val = json!("<SESSION_FILE>"),
                    "baseUrl" => *val = json!("<MOCK_BASE_URL>"),
                    "contextUsage" => *val = json!("<CONTEXT_USAGE>"),
                    "at" | "since" | "timestamp" | "expires_at" => *val = json!("<TIMESTAMP>"),
                    // A reply goes out before the host counts its command as answered, so a
                    // listing right after one may still show the session as busy.
                    "busy" if frame_type.is_none() => *val = json!("<BUSY>"),
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

/// `lane_hold_stop_reload_resume` with the RPC extension (`rpc_ext.ts`) loaded: it answers before
/// the stop, after each of two reloads and after the resume. A reload used to load the extension a
/// second time into the same host, where `rpc.handle` failed with `duplicate extension rpc handler`.
#[test]
fn golden_lane_hold_stop_reload_resume_rpc_ext() {
    if !bun_on_path() {
        eprintln!("skipping golden_lane_hold_stop_reload_resume_rpc_ext: bun not found on PATH");
        return;
    }
    let ext_arg = fixture_path("rpc_ext.ts").display().to_string();
    let mut h = RpcHarness::spawn(&["-e", &ext_arg]);
    let stderr = Arc::clone(&h.stderr);
    let move_ready = json!({"type": "extension_request", "name": "omomeow.move-readiness"});

    let (sci, _) = h.request(json!({
        "type": "set_client_info",
        "width": 80,
        "capabilities": ["interactive_user"],
    }));
    assert_eq!(sci["success"], true);

    let (ready, _) = h.request(move_ready.clone());
    assert_eq!(ready["data"], json!({"activeSources": []}), "{ready}");

    let (stop_ok, from_stop) = h.request(json!({"type": "interrupt", "intent": "stop"}));
    assert_eq!(stop_ok["success"], true);
    let paused_evt = h.expect_frame(from_stop, |f| f["type"] == "session_paused", FRAME_TIMEOUT);
    h.record_in(paused_evt);

    for _ in 0..2 {
        let (reloaded, _) = h.request(json!({"type": "reload"}));
        assert_eq!(reloaded["success"], true, "{reloaded}");
        assert_eq!(reloaded["data"]["cancelled"], false);
        let (ready, _) = h.request(move_ready.clone());
        assert_eq!(ready["data"], json!({"activeSources": []}), "{ready}");
    }

    let (resumed, from_resume) = h.request(json!({"type": "resume_session"}));
    assert_eq!(resumed["success"], true);
    let resumed_evt = h.expect_frame(
        from_resume,
        |f| f["type"] == "session_resumed",
        FRAME_TIMEOUT,
    );
    h.record_in(resumed_evt);

    let (ready, _) = h.request(move_ready);
    assert_eq!(ready["data"], json!({"activeSources": []}), "{ready}");

    let rows = h.transcript.clone();
    h.finish();
    let stderr = stderr.lock().expect("stderr lock").clone();
    assert!(
        !stderr.contains("failed to load"),
        "a reload must not load the extension on top of itself:\n{stderr}"
    );
    assert_golden("lane_hold_stop_reload_resume_rpc_ext", &rows);
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

/// Frames that make up one agent turn, as the gateway observes it.
const TURN_FRAMES: &[&str] = &[
    "agent_start",
    "turn_start",
    "turn_end",
    "agent_end",
    "prompt_result",
    "session_settled",
    "agent_settled",
];

/// Add the frames of the given kinds received since `from` to the transcript; with
/// `session`, only those the host stamped with that routing id.
fn record_frames(h: &mut RpcHarness, from: usize, kinds: &[&str], session: Option<&str>) {
    for frame in h.frames_since(from) {
        let kind = frame["type"].as_str().unwrap_or_default();
        // The watch's last count (zero) races with the turn its wake starts: only the
        // count of a watch that began is a fixed point in the transcript.
        let ended_count = kind == "extension_event" && frame["data"]["activeCount"] == 0;
        if kinds.contains(&kind)
            && !ended_count
            && session.is_none_or(|id| frame["sessionId"] == id)
        {
            h.record_in(frame);
        }
    }
}

fn assert_refused(resp: &Value, code: &str) {
    assert_eq!(resp["success"], false, "{resp}");
    assert_eq!(resp["code"], code, "{resp}");
    assert_eq!(resp["errorCode"], code, "{resp}");
}

/// The `session_released` seal a release appended to the session file.
fn seal_of(file: &str) -> Value {
    let text = std::fs::read_to_string(file).expect("read session file");
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry["customType"] == "session_released")
        .unwrap_or_else(|| panic!("no session_released entry in {file}:\n{text}"))
}

/// Make the host's work dir a checkout of one repository: a move refuses a landing in another.
fn init_checkout(h: &RpcHarness) {
    for args in [
        vec!["init", "-q"],
        vec![
            "remote",
            "add",
            "origin",
            "git@github.com:jc01rho/oh-my-tau.git",
        ],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&h.work_dir)
            .args(&args)
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?}");
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create copy target");
    for entry in std::fs::read_dir(from).expect("read bundle dir") {
        let entry = entry.expect("bundle entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy bundle file");
        }
    }
}

#[test]
fn golden_multi_session_open_route_close() {
    let mut h = RpcHarness::spawn_host();
    let ready = h.expect_frame(0, |f| f["type"] == "ready", FRAME_TIMEOUT);
    assert_eq!(ready["multiSession"], true);
    assert_eq!(
        ready["launchProfile"],
        json!({"multi_session": true, "session_runtime": "in-process"})
    );
    let mut rows = vec![json!({"dir": "in", "frame": ready})];

    let (info, _) = h.request(json!({"type": "get_protocol_info"}));
    assert_eq!(info["success"], true);
    assert_eq!(info["data"]["mode"], "multi_session");
    let cmds: Vec<&str> = info["data"]["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in [
        "open_session",
        "close_session",
        "list_sessions",
        "release_session",
        "move_session",
        "extension_request",
        "get_state",
        "prompt",
    ] {
        assert!(cmds.contains(&required), "missing {required} in {cmds:?}");
    }

    let (empty, _) = h.request(json!({"type": "list_sessions"}));
    assert_eq!(empty["data"]["sessions"], json!([]));
    let (no_id, _) = h.request(json!({"type": "get_state"}));
    assert_refused(&no_id, "missing_session_id");

    let first = h.open_session("a.jsonl");
    assert_eq!(first["sessionId"], "rpc-1");
    assert_eq!(first["resumed"], false);
    let second = h.open_session("b.jsonl");
    assert_eq!(second["sessionId"], "rpc-2");
    assert_ne!(first["durableSessionId"], second["durableSessionId"]);

    let (in_use, _) = h.request(json!({
        "type": "open_session",
        "sessionPath": h.lane_file("a.jsonl"),
        "cwd": h.work_dir.display().to_string(),
    }));
    assert_refused(&in_use, "session_path_in_use");
    let (relative, _) = h.request(json!({"type": "open_session", "sessionPath": "relative.jsonl"}));
    assert_refused(&relative, "invalid_path");
    let (no_cwd, _) = h.request(json!({
        "type": "open_session",
        "cwd": h.home_dir.join("no-such-dir").display().to_string(),
    }));
    assert_refused(&no_cwd, "invalid_path");

    let (listed, _) = h.request(json!({"type": "list_sessions"}));
    assert_eq!(listed["data"]["sessions"].as_array().map(Vec::len), Some(2));

    for (id, opened) in [("rpc-1", &first), ("rpc-2", &second)] {
        let (state, _) = h.request_in(id, json!({"type": "get_state"}));
        assert_eq!(state["success"], true, "{state}");
        assert_eq!(state["sessionId"], id, "the answer carries the routing id");
        assert_eq!(state["data"]["sessionId"], opened["durableSessionId"]);
    }

    // A turn on rpc-2 stays on rpc-2: rpc-1 sees none of it.
    let (_prompt, from_prompt) = h.request_in(
        "rpc-2",
        json!({
            "type": "prompt",
            "message": "Reply deterministically",
            "streamingBehavior": "followUp",
        }),
    );
    h.expect_tagged(from_prompt, "rpc-2", |f| f["type"] == "agent_settled");
    assert!(
        h.frames_since(from_prompt)
            .iter()
            .all(|f| f["sessionId"] != "rpc-1"),
        "rpc-1 received frames of rpc-2's turn"
    );
    record_frames(&mut h, from_prompt, TURN_FRAMES, Some("rpc-2"));
    let (last_1, _) = h.request_in("rpc-1", json!({"type": "get_last_assistant_text"}));
    assert_eq!(last_1["data"]["text"], Value::Null);
    let (last_2, _) = h.request_in("rpc-2", json!({"type": "get_last_assistant_text"}));
    assert_eq!(last_2["data"]["text"], "MOCK-RPC-OK");

    let (unknown, _) = h.request_in("rpc-9", json!({"type": "get_state"}));
    assert_refused(&unknown, "unknown_session");
    assert_eq!(unknown["sessionId"], "rpc-9");

    let (closed, _) = h.request_in("rpc-1", json!({"type": "close_session"}));
    assert_eq!(closed["success"], true, "{closed}");
    assert_eq!(closed["sessionId"], "rpc-1");
    let (after, _) = h.request(json!({"type": "list_sessions"}));
    let left: Vec<&Value> = after["data"]["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| &s["sessionId"])
        .collect();
    assert_eq!(left, [&json!("rpc-2")]);
    let (gone, _) = h.request_in("rpc-1", json!({"type": "get_state"}));
    assert_refused(&gone, "unknown_session");
    let (twice, _) = h.request_in("rpc-1", json!({"type": "close_session"}));
    assert_refused(&twice, "unknown_session");
    let (no_session, _) = h.request(json!({"type": "close_session"}));
    assert_refused(&no_session, "missing_session_id");

    rows.extend(h.transcript.clone());
    h.finish();
    assert_golden("multi_session_open_route_close", &rows);
}

#[test]
fn golden_release_session_refusals_and_seal() {
    let mut h = RpcHarness::spawn_host();
    let ready = h.expect_frame(0, |f| f["type"] == "ready", FRAME_TIMEOUT);
    let first = h.open_session("a.jsonl");
    let second = h.open_session("b.jsonl");
    let (file_1, file_2) = (
        first["sessionFile"].as_str().expect("file 1").to_owned(),
        second["sessionFile"].as_str().expect("file 2").to_owned(),
    );

    let (no_id, _) = h.request(json!({"type": "release_session", "reason": "takeover"}));
    assert_refused(&no_id, "missing_session_id");
    for bad in [
        json!({}),
        json!({"reason": "move"}),
        json!({"reason": "move", "moveId": "../escape"}),
        json!({"reason": "swap"}),
    ] {
        let mut cmd = json!({"type": "release_session"});
        for (k, v) in bad.as_object().expect("object") {
            cmd[k] = v.clone();
        }
        let (resp, _) = h.request_in("rpc-1", cmd);
        assert_refused(&resp, "invalid_release_reason");
    }
    let (unknown, _) = h.request_in(
        "rpc-9",
        json!({"type": "release_session", "reason": "takeover"}),
    );
    assert_refused(&unknown, "unknown_session");

    // A session with a turn in flight refuses a release that does not interrupt.
    let (set, _) = h.request_in(
        "rpc-1",
        json!({"type": "set_model", "provider": "mock", "modelId": "hold"}),
    );
    assert_eq!(set["success"], true, "{set}");
    let (_p, from_prompt) = h.request_in(
        "rpc-1",
        json!({"type": "prompt", "message": "Hold turn", "streamingBehavior": "followUp"}),
    );
    h.expect_tagged(from_prompt, "rpc-1", |f| f["type"] == "agent_start");
    let (busy, _) = h.request_in(
        "rpc-1",
        json!({"type": "release_session", "reason": "takeover"}),
    );
    assert_refused(&busy, "turn_active");
    assert!(
        busy["error"].as_str().is_some_and(|e| e.contains("turn")),
        "{busy}"
    );
    // The refusal changed nothing: the session still answers.
    let (still, _) = h.request_in("rpc-1", json!({"type": "get_state"}));
    assert_eq!(still["success"], true, "{still}");

    let (taken, _) = h.request_in(
        "rpc-1",
        json!({"type": "release_session", "reason": "takeover", "interrupt": true}),
    );
    assert_eq!(taken["success"], true, "{taken}");
    assert_eq!(taken["sessionId"], "rpc-1");
    assert_eq!(taken["data"]["released"], true);
    assert_eq!(taken["data"]["session_path"], json!(file_1));
    let seal = seal_of(&file_1);
    assert_eq!(seal["data"]["reason"], "takeover");
    assert_eq!(seal["data"]["interrupted"], true);

    // The gateway's move release, on an idle session.
    let (moved, _) = h.request_in(
        "rpc-2",
        json!({
            "type": "release_session",
            "reason": "move",
            "moveId": "m1",
            "target": "laptop",
            "interrupt": true,
            "force": true,
        }),
    );
    assert_eq!(moved["success"], true, "{moved}");
    assert_eq!(moved["data"]["released"], true);
    assert_eq!(
        moved["data"]["dropped"],
        json!({"deliveries": [], "user_messages": []})
    );
    let seal = seal_of(&file_2);
    assert_eq!(seal["data"]["reason"], "move");
    assert_eq!(seal["data"]["move_id"], "m1");
    assert_eq!(seal["data"]["target"], "laptop");
    assert_eq!(seal["data"]["interrupted"], false);

    h.wait_until_gone("rpc-2");
    let (gone, _) = h.request_in("rpc-2", json!({"type": "get_state"}));
    assert_refused(&gone, "unknown_session");

    h.release_hold();
    let mut rows = vec![json!({"dir": "in", "frame": ready})];
    rows.extend(h.transcript.clone());
    h.finish();
    assert_golden("release_session_refusals_and_seal", &rows);
}

#[test]
fn golden_move_session_between_two_hosts() {
    let mut desk = RpcHarness::spawn_host();
    let mut laptop = RpcHarness::spawn_host();
    init_checkout(&desk);
    init_checkout(&laptop);

    // The receiver readies the move, as the gateway's "prepare" step.
    let (prepared, _) =
        laptop.request(json!({"type": "move_session", "op": "prepare", "moveId": "m1"}));
    assert_eq!(prepared["success"], true, "{prepared}");
    assert_eq!(prepared["data"]["state"], "ready");

    // The sender: a session with a turn behind it.
    let opened = desk.open_session("a.jsonl");
    let (_p, from_prompt) = desk.request_in(
        "rpc-1",
        json!({"type": "prompt", "message": "Reply deterministically", "streamingBehavior": "followUp"}),
    );
    desk.expect_tagged(from_prompt, "rpc-1", |f| f["type"] == "agent_settled");
    let file = opened["sessionFile"]
        .as_str()
        .expect("session file")
        .to_owned();
    let export = |move_id: &str, file: &str, dropped: &Value| {
        json!({"type": "move_session", "op": "export", "moveId": move_id, "sessionPath": file,
               "target": "laptop", "droppedUserMessages": dropped})
    };
    let (hosted, _) = desk.request(export("m1", &file, &json!([])));
    assert_refused(&hosted, "session_open_here");

    let (released, _) = desk.request_in(
        "rpc-1",
        json!({"type": "release_session", "reason": "move", "moveId": "m1",
               "target": "laptop", "interrupt": true, "force": true}),
    );
    assert_eq!(released["success"], true, "{released}");
    desk.wait_until_gone("rpc-1");
    let (exported, _) = desk.request(export(
        "m1",
        &file,
        &released["data"]["dropped"]["user_messages"],
    ));
    assert_eq!(exported["success"], true, "{exported}");
    assert_eq!(exported["data"]["move_id"], "m1");
    assert!(exported["data"]["session_id"].is_string(), "{exported}");
    let bundle = PathBuf::from(exported["data"]["bundle_dir"].as_str().expect("bundle dir"));

    // The gateway carries the bundle to the receiver, which imports it and answers.
    let carried = laptop.home_dir.join("carried/engine");
    copy_dir(&bundle, &carried);
    let (imported, _) = laptop.request(json!({
        "type": "move_session", "op": "import", "bundleDir": carried.display().to_string(),
        "cwd": laptop.work_dir.display().to_string(), "from": "desk",
    }));
    assert_eq!(imported["success"], true, "{imported}");
    assert_eq!(
        imported["data"]["session_id"],
        exported["data"]["session_id"]
    );
    let landed = imported["data"]["session_path"]
        .as_str()
        .expect("landed path")
        .to_owned();
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
    let (answer, _) = laptop.request(
        json!({"type": "move_session", "op": "status", "moveId": "m1", "sessionPath": landed}),
    );
    assert_eq!(answer["data"], json!({"state": "accepted"}));
    let (finished, _) = desk.request(json!({
        "type": "move_session", "op": "finish", "moveId": "m1", "sessionPath": file, "answer": answer["data"],
    }));
    assert_eq!(finished["data"], json!({"ownership": "moved"}));
    assert!(!bundle.exists(), "finish removes the sender's bundle");

    // A move the receiver cancelled: the sender takes its session back.
    let (readied, _) =
        laptop.request(json!({"type": "move_session", "op": "prepare", "moveId": "m2"}));
    assert_eq!(readied["data"]["state"], "ready");
    let (cancelled, _) =
        laptop.request(json!({"type": "move_session", "op": "cancel", "moveId": "m2"}));
    assert_eq!(
        cancelled["data"],
        json!({"state": "declined", "reason": "cancelled"})
    );
    let (fenced, _) =
        laptop.request(json!({"type": "move_session", "op": "prepare", "moveId": "m2"}));
    assert_refused(&fenced, "move_fenced");
    let (unknown, _) =
        laptop.request(json!({"type": "move_session", "op": "status", "moveId": "m9"}));
    assert_eq!(
        unknown["data"],
        json!({"state": "declined", "reason": "no_record"})
    );

    let reopened = desk.open_session("b.jsonl");
    assert_eq!(reopened["sessionId"], "rpc-2");
    let (_p2, from_p2) = desk.request_in(
        "rpc-2",
        json!({"type": "prompt", "message": "Reply deterministically", "streamingBehavior": "followUp"}),
    );
    desk.expect_tagged(from_p2, "rpc-2", |f| f["type"] == "agent_settled");
    let file_2 = reopened["sessionFile"]
        .as_str()
        .expect("session file")
        .to_owned();
    let (closed, _) = desk.request_in("rpc-2", json!({"type": "close_session"}));
    assert_eq!(closed["success"], true, "{closed}");
    let (unreleased, _) = desk.request(export("m2", &file_2, &json!([])));
    assert_refused(&unreleased, "move_not_released");
    let again = desk.open_session("b.jsonl");
    assert_eq!(again["sessionId"], "rpc-3");
    assert_eq!(again["resumed"], true);
    let (released_2, _) = desk.request_in(
        "rpc-3",
        json!({"type": "release_session", "reason": "move", "moveId": "m2",
               "target": "laptop", "interrupt": true, "force": true}),
    );
    assert_eq!(released_2["success"], true, "{released_2}");
    desk.wait_until_gone("rpc-3");
    let (exported_2, _) = desk.request(export("m2", &file_2, &json!(["still owed"])));
    assert_eq!(exported_2["success"], true, "{exported_2}");
    let (declined, _) =
        laptop.request(json!({"type": "move_session", "op": "status", "moveId": "m2"}));
    assert_eq!(
        declined["data"],
        json!({"state": "declined", "reason": "cancelled"})
    );
    let (writable, _) = desk.request(json!({
        "type": "move_session", "op": "finish", "moveId": "m2", "sessionPath": file_2, "answer": declined["data"],
    }));
    assert_eq!(writable["data"], json!({"ownership": "writable"}));

    // Malformed requests are refused before anything is touched.
    for (cmd, code) in [
        (json!({"op": "prepare"}), "invalid_move_request"),
        (
            json!({"op": "prepare", "moveId": "../escape"}),
            "invalid_move_request",
        ),
        (
            json!({"op": "teleport", "moveId": "m3"}),
            "invalid_move_request",
        ),
        (
            json!({"op": "export", "moveId": "m3", "sessionPath": "/s.jsonl"}),
            "invalid_move_request",
        ),
        (
            json!({"op": "finish", "moveId": "m3", "sessionPath": "/s.jsonl", "answer": {"state": "ready"}}),
            "invalid_move_request",
        ),
        (
            json!({"op": "import", "bundleDir": "relative", "cwd": "/repo"}),
            "invalid_move_request",
        ),
    ] {
        let mut full = json!({"type": "move_session"});
        for (k, v) in cmd.as_object().expect("object") {
            full[k] = v.clone();
        }
        let (resp, _) = laptop.request(full);
        assert_refused(&resp, code);
    }

    let (desk_rows, laptop_rows) = (desk.transcript.clone(), laptop.transcript.clone());
    desk.finish();
    laptop.finish();
    assert_golden("move_session_sender", &desk_rows);
    assert_golden("move_session_receiver", &laptop_rows);
}

/// The watcher the mock provider starts ends once the gate file exists.
fn open_gate(h: &RpcHarness) {
    std::fs::write(h.home_dir.join("monitor.gate"), "open").expect("create gate file");
}

const WAKE_FRAMES: &[&str] = &[
    "extension_event",
    "queue_update",
    "agent_start",
    "turn_start",
    "turn_end",
    "agent_end",
    "agent_settled",
];

/// The user message the provider received last.
fn last_user_text(h: &RpcHarness) -> String {
    let requests = h._mock.requests.lock().expect("requests lock");
    let body = requests.last().expect("a provider request");
    let messages = body["messages"].as_array().expect("messages");
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("a user message");
    last["content"].to_string()
}

#[test]
fn golden_monitor_completion_wakes_an_idle_session() {
    if !cfg!(unix) {
        return;
    }
    let mut h = RpcHarness::spawn(&["--no-extensions", "--auto-approve"]);
    let (_p, from_prompt) = h.request(json!({
        "type": "prompt",
        "message": "Start the watcher [[run-monitor]]",
        "streamingBehavior": "followUp",
    }));
    h.expect_frame(from_prompt, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    record_frames(&mut h, from_prompt, WAKE_FRAMES, None);

    // The watched command ends: its completion wakes the idle session with a turn of its own.
    let from_gate = h.current_len();
    open_gate(&h);
    h.expect_frame(from_gate, |f| f["type"] == "agent_start", FRAME_TIMEOUT);
    h.expect_frame(from_gate, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    record_frames(&mut h, from_gate, WAKE_FRAMES, None);
    let wake = last_user_text(&h);
    assert!(wake.contains("<system-reminder>"), "{wake}");
    assert!(
        wake.contains("Monitor mon_1(g12c watcher) ended: the command exited with code 0."),
        "{wake}"
    );

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("monitor_wake_idle_turn", &rows);
}

#[test]
fn golden_monitor_completion_joins_the_next_turn_after_a_hold() {
    if !cfg!(unix) {
        return;
    }
    let mut h = RpcHarness::spawn(&["--no-extensions", "--auto-approve"]);
    let (sci, _) = h.request(
        json!({"type": "set_client_info", "width": 80, "capabilities": ["interactive_user"]}),
    );
    assert_eq!(sci["success"], true);
    let (_p, from_prompt) = h.request(json!({
        "type": "prompt",
        "message": "Start the watcher [[run-monitor]]",
        "streamingBehavior": "followUp",
    }));
    h.expect_frame(from_prompt, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    record_frames(&mut h, from_prompt, WAKE_FRAMES, None);

    let (stop, from_stop) = h.request(json!({"type": "interrupt", "intent": "stop"}));
    assert_eq!(stop["success"], true);
    let paused = h.expect_frame(from_stop, |f| f["type"] == "session_paused", FRAME_TIMEOUT);
    h.record_in(paused);

    // A held lane only queues the wake: a monitor never lifts a hold.
    let from_gate = h.current_len();
    open_gate(&h);
    let queued = h.expect_frame(
        from_gate,
        |f| f["type"] == "queue_update" && f["followUp"].to_string().contains("g12c watcher"),
        FRAME_TIMEOUT,
    );
    h.record_in(queued);
    assert!(
        h.frames_since(from_gate)
            .iter()
            .all(|f| f["type"] != "agent_start"),
        "the wake started a turn on a held lane"
    );

    let (resumed, from_resume) = h.request(json!({"type": "resume_session"}));
    assert_eq!(resumed["success"], true);
    let resumed_evt = h.expect_frame(
        from_resume,
        |f| f["type"] == "session_resumed",
        FRAME_TIMEOUT,
    );
    h.record_in(resumed_evt);

    // The next turn the gateway starts carries the queued wake.
    let (_next, from_next) = h.request(json!({
        "type": "prompt",
        "message": "Next turn",
        "streamingBehavior": "followUp",
    }));
    h.expect_frame(from_next, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    record_frames(&mut h, from_next, WAKE_FRAMES, None);
    let wake = last_user_text(&h);
    assert!(
        wake.contains("Monitor mon_1(g12c watcher) ended: the command exited with code 0."),
        "{wake}"
    );

    let rows = h.transcript.clone();
    h.finish();
    assert_golden("monitor_wake_held_next_turn", &rows);
}

/// B1: a provider an extension registers is resolvable at launch (`--model` names it before
/// any notification is served), listed by `get_available_models`, selectable with
/// `set_model`, and streams to its own base URL; an unregistered one stays gone.
#[test]
fn extension_provider_boot() {
    if !bun_on_path() {
        eprintln!("skipping extension_provider_boot: bun not found on PATH");
        return;
    }
    let ext_file = fixture_path("provider_ext.ts");
    let ext_arg = ext_file.display().to_string();
    let mut h = RpcHarness::spawn(&[
        "-e",
        &ext_arg,
        "--model",
        "fx-prov/m1",
        "--no-model-fallback",
    ]);

    let (state, _) = h.request(json!({"type": "get_state"}));
    let stderr = h.stderr_text();
    assert_eq!(state["success"], true, "{state}\nstderr: {stderr}");
    assert_eq!(state["data"]["model"]["provider"], "fx-prov", "{state}");
    assert_eq!(state["data"]["model"]["id"], "m1", "{state}");

    let (listed, _) = h.request(json!({"type": "get_available_models"}));
    let ids: Vec<String> = listed["data"]["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|m| {
            format!(
                "{}/{}",
                m["provider"].as_str().unwrap_or_default(),
                m["id"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert!(ids.iter().any(|i| i == "fx-prov/m1"), "{ids:?}");
    assert!(ids.iter().any(|i| i == "fx-prov/m2"), "{ids:?}");
    assert!(!ids.iter().any(|i| i.starts_with("fx-gone/")), "{ids:?}");

    let (_, from_prompt) = h.request(json!({"type": "prompt", "message": "hello"}));
    h.expect_frame(from_prompt, |f| f["type"] == "agent_settled", FRAME_TIMEOUT);
    let sent: Vec<String> = h
        ._mock
        .requests
        .lock()
        .expect("requests lock")
        .iter()
        .map(|b| b["model"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(sent.last().map(String::as_str), Some("m1"), "{sent:?}");

    let (switched, _) =
        h.request(json!({"type": "set_model", "provider": "fx-prov", "modelId": "m2"}));
    assert_eq!(switched["success"], true, "{switched}");
    let (gone, _) = h.request(json!({"type": "set_model", "provider": "fx-gone", "modelId": "g1"}));
    assert_eq!(gone["success"], false, "{gone}");

    h.finish();
}
