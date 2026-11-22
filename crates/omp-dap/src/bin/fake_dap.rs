//! Fake debug adapter for `omp-dap` integration tests.
//!
//! Speaks DAP over stdio with `Content-Length` framing and answers a fixed
//! script, so a test can drive a real adapter process without a debugger
//! installed:
//!
//! | request | reply |
//! | --- | --- |
//! | `initialize` | capabilities `{supportsConfigurationDoneRequest: true}`, then an `initialized` event and one `output` event (`"fake dap ready\n"`, category `console`) |
//! | `launch` | success, no body; records `program`, `cwd`, `stopOnEntry` and `exitOnContinue` from the arguments |
//! | `setBreakpoints` | one verified breakpoint per requested line, replacing the previous set for that source path |
//! | `configurationDone` | success, then a `stopped` event on thread 1: `breakpoint` when a breakpoint is set and `stopOnEntry` is false, else `entry` |
//! | `threads` | one thread: `{id: 1, name: "main"}` |
//! | `stackTrace` | two frames (`main`, then `helper`) at the first breakpoint line, `totalFrames: 2` |
//! | `scopes` | `Locals` (variables reference 1) and `Globals` (reference 2) |
//! | `variables` | reference 1: `x: int 42`, `name: string "fake"`, `obj` (reference 3); reference 2: `g: int 7`; reference 3: `field: int 1`; any other reference: empty |
//! | `continue` | success (`allThreadsContinued`), a `continued` event, then `exited`+`terminated` when `launch` set `exitOnContinue`, else a fresh `stopped` event |
//! | `disconnect` | success, then the process exits cleanly |
//! | anything else | `success: false` with `Unsupported DAP request: <command>` |
//!
//! Replies are written in request order, echo the request `seq` as
//! `request_seq`, and carry their own increasing `seq`.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

/// The single thread the fake target pretends to run.
const THREAD_ID: i64 = 1;

/// One source breakpoint the fake adapter pretends to have set.
struct Breakpoint {
    id: i64,
    line: i64,
    /// `source.path` the breakpoint belongs to; `None` when the request had none.
    path: Option<String>,
}

/// Adapter state: reply sequence, the breakpoint table and the launch flags.
#[derive(Default)]
struct Server {
    seq: u64,
    next_breakpoint_id: i64,
    breakpoints: Vec<Breakpoint>,
    source: Option<Value>,
    program: Option<String>,
    cwd: Option<String>,
    stop_on_entry: bool,
    exit_on_continue: bool,
}

impl Server {
    /// Dispatch one request; `false` when the read loop should stop.
    fn handle(&mut self, request: &Value) -> bool {
        match command_of(request).as_str() {
            "initialize" => self.initialize(request),
            "launch" => self.launch(request),
            "setBreakpoints" => self.set_breakpoints(request),
            "configurationDone" => self.configuration_done(request),
            "threads" => self.threads(request),
            "stackTrace" => self.stack_trace(request),
            "scopes" => self.scopes(request),
            "variables" => self.variables(request),
            "continue" => self.resume(request),
            "disconnect" => {
                self.respond(request, None);
                return false;
            }
            other => {
                let message = format!("Unsupported DAP request: {other}");
                self.fail(request, &message);
            }
        }
        true
    }

    fn initialize(&mut self, request: &Value) {
        // `InitializeResponse.body` *is* the capabilities object, not a wrapper.
        self.respond(
            request,
            Some(json!({ "supportsConfigurationDoneRequest": true })),
        );
        self.emit("initialized", &json!({}));
        self.emit(
            "output",
            &json!({ "category": "console", "output": "fake dap ready\n" }),
        );
    }

    fn launch(&mut self, request: &Value) {
        let arguments = request.get("arguments").cloned().unwrap_or(Value::Null);
        self.program = string_arg(&arguments, "program");
        self.cwd = string_arg(&arguments, "cwd");
        self.stop_on_entry = bool_arg(&arguments, "stopOnEntry");
        self.exit_on_continue = bool_arg(&arguments, "exitOnContinue");
        self.respond(request, None);
    }

    fn set_breakpoints(&mut self, request: &Value) {
        let arguments = request.get("arguments").cloned().unwrap_or(Value::Null);
        let source = arguments.get("source").cloned().unwrap_or(Value::Null);
        let path = source
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        // A `setBreakpoints` call replaces the breakpoints of its own file only.
        self.breakpoints
            .retain(|breakpoint| breakpoint.path != path);
        let requested = arguments
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut verified = Vec::with_capacity(requested.len());
        for entry in requested {
            self.next_breakpoint_id += 1;
            let id = self.next_breakpoint_id;
            let line = entry.get("line").and_then(Value::as_i64).unwrap_or(1);
            self.breakpoints.push(Breakpoint {
                id,
                line,
                path: path.clone(),
            });
            let mut reply = json!({
                "id": id,
                "verified": true,
                "line": line,
            });
            if !source.is_null() {
                reply["source"] = source.clone();
            }
            verified.push(reply);
        }
        if !source.is_null() {
            self.source = Some(source);
        }
        self.respond(request, Some(json!({ "breakpoints": verified })));
    }

    fn configuration_done(&mut self, request: &Value) {
        self.respond(request, None);
        let reason = if self.stop_on_entry || self.breakpoints.is_empty() {
            "entry"
        } else {
            "breakpoint"
        };
        let mut body = json!({
            "reason": reason,
            "threadId": THREAD_ID,
            "allThreadsStopped": true,
        });
        if reason == "breakpoint" {
            body["hitBreakpointIds"] = json!(self.breakpoint_ids());
        }
        self.emit("stopped", &body);
    }

    fn threads(&mut self, request: &Value) {
        self.respond(
            request,
            Some(json!({ "threads": [{ "id": THREAD_ID, "name": "main" }] })),
        );
    }

    fn stack_trace(&mut self, request: &Value) {
        let line = self
            .breakpoints
            .first()
            .map_or(1, |breakpoint| breakpoint.line);
        let source = self.source_value();
        self.respond(
            request,
            Some(json!({
                "stackFrames": [
                    {
                        "id": 1,
                        "name": "main",
                        "source": source,
                        "line": line,
                        "column": 1,
                    },
                    {
                        "id": 2,
                        "name": "helper",
                        "source": source,
                        "line": line,
                        "column": 1,
                    },
                ],
                "totalFrames": 2,
            })),
        );
    }

    fn scopes(&mut self, request: &Value) {
        self.respond(
            request,
            Some(json!({
                "scopes": [
                    { "name": "Locals", "variablesReference": 1, "expensive": false },
                    { "name": "Globals", "variablesReference": 2, "expensive": false },
                ]
            })),
        );
    }

    fn variables(&mut self, request: &Value) {
        let reference = request
            .pointer("/arguments/variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let variables = match reference {
            1 => json!([
                { "name": "x", "value": "42", "type": "int", "variablesReference": 0 },
                { "name": "name", "value": "\"fake\"", "type": "string", "variablesReference": 0 },
                {
                    "name": "obj",
                    "value": "{field: 1}",
                    "type": "object",
                    "variablesReference": 3,
                },
            ]),
            2 => json!([
                { "name": "g", "value": "7", "type": "int", "variablesReference": 0 },
            ]),
            3 => json!([
                { "name": "field", "value": "1", "type": "int", "variablesReference": 0 },
            ]),
            _ => json!([]),
        };
        self.respond(request, Some(json!({ "variables": variables })));
    }

    fn resume(&mut self, request: &Value) {
        self.respond(request, Some(json!({ "allThreadsContinued": true })));
        self.emit(
            "continued",
            &json!({ "threadId": THREAD_ID, "allThreadsContinued": true }),
        );
        if self.exit_on_continue {
            self.emit("exited", &json!({ "exitCode": 0 }));
            self.emit("terminated", &json!({}));
            return;
        }
        // Default script: the target runs on and stops again.
        let reason = if self.breakpoints.is_empty() {
            "step"
        } else {
            "breakpoint"
        };
        let mut body = json!({
            "reason": reason,
            "threadId": THREAD_ID,
            "allThreadsStopped": true,
        });
        if reason == "breakpoint" {
            body["hitBreakpointIds"] = json!(self.breakpoint_ids());
        }
        self.emit("stopped", &body);
    }

    /// Breakpoint ids in the order they were set.
    fn breakpoint_ids(&self) -> Vec<i64> {
        self.breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect()
    }

    /// Source object for stack frames: the last `setBreakpoints` source, else
    /// the launched program resolved against `cwd`.
    fn source_value(&self) -> Value {
        if let Some(source) = &self.source {
            return source.clone();
        }
        let program = self.program.clone().unwrap_or_else(|| "fake".to_string());
        let path = match &self.cwd {
            Some(cwd) if !std::path::Path::new(&program).is_absolute() => std::path::Path::new(cwd)
                .join(&program)
                .display()
                .to_string(),
            _ => program,
        };
        json!({ "name": file_name(&path), "path": path })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn respond(&mut self, request: &Value, body: Option<Value>) {
        let seq = self.next_seq();
        let mut message = json!({
            "seq": seq,
            "type": "response",
            "request_seq": request_seq(request),
            "success": true,
            "command": command_of(request),
        });
        if let Some(body) = body {
            message["body"] = body;
        }
        write_message(&message);
    }

    fn fail(&mut self, request: &Value, reason: &str) {
        let seq = self.next_seq();
        write_message(&json!({
            "seq": seq,
            "type": "response",
            "request_seq": request_seq(request),
            "success": false,
            "command": command_of(request),
            "message": reason,
        }));
    }

    fn emit(&mut self, event: &str, body: &Value) {
        let seq = self.next_seq();
        write_message(&json!({ "seq": seq, "type": "event", "event": event, "body": body }));
    }
}

fn command_of(request: &Value) -> String {
    request
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn request_seq(request: &Value) -> u64 {
    request.get("seq").and_then(Value::as_u64).unwrap_or(0)
}

fn string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn bool_arg(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(
        || path.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(value: &Value) {
    let body = serde_json::to_string(value).expect("serialize message");
    let mut out = std::io::stdout().lock();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write message");
    out.flush().expect("flush message");
}

fn main() {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut server = Server::default();
    while let Some(message) = read_message(&mut reader) {
        // Adapter-initiated traffic (reverse responses) is ignored: this
        // fixture never sends requests of its own.
        if message.get("type").and_then(Value::as_str) != Some("request") {
            continue;
        }
        if !server.handle(&message) {
            break;
        }
    }
}
