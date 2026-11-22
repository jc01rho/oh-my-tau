//! DAP transport client (port of omp `src/dap/client.ts`).
//!
//! Three transports, chosen by the adapter's `connectMode`:
//! - `stdio` (default): spawn the adapter, speak DAP over its stdin/stdout.
//! - `socket`: the adapter opens a unix socket (Linux, Delve) or dials back into
//!   a listener we own (other platforms).
//! - `tcp`: reserve a port, spawn the adapter with `${port}` substituted in its
//!   args, then connect to it.
//!
//! Messages are `Content-Length` framed with resync on junk bytes, requests are
//! routed through a `seq → oneshot` map, events fan out to registered handlers,
//! and adapter-initiated requests (`runInTerminal`, `startDebugging`) are
//! answered through registered reverse handlers.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::DAP_PORT_ARGUMENT;
use crate::types::{DapCapabilities, DapConnectMode, DapResolvedAdapter};
use crate::{Cancel, DapError, DapResult};

/// Default budget for one request or event wait.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on a single framed write, so a wedged adapter stdin cannot hang the
/// client forever (TS `WRITE_MESSAGE_TIMEOUT_MS`).
const WRITE_MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long socket-mode helpers wait for the adapter to become reachable.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Read chunk size for the transport reader.
const READ_CHUNK: usize = 64 * 1024;

/// How much adapter stderr is retained for error messages.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Environment applied to every spawned adapter and debuggee: an adapter that
/// believes it is interactive opens `/dev/tty` and can be suspended by job
/// control (TS `NON_INTERACTIVE_ENV`).
const NON_INTERACTIVE_ENV: [(&str, &str); 6] = [
    ("CI", "1"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// One framed message in flight.
struct PendingRequest {
    command: String,
    tx: oneshot::Sender<DapResult<Value>>,
}

type EventPredicate = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// A registered `wait_for_event` waiter.
struct EventWaiter {
    event: String,
    predicate: Option<EventPredicate>,
    tx: Option<oneshot::Sender<Option<Value>>>,
}

type EventHandler = Arc<dyn Fn(&str, Option<&Value>) + Send + Sync>;
type ReverseHandler =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = DapResult<Value>> + Send>> + Send + Sync>;

/// Shared client state, cloned into the reader task.
struct Inner {
    adapter_name: String,
    seq: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    event_handlers: Mutex<HashMap<String, Vec<EventHandler>>>,
    any_event_handlers: Mutex<Vec<EventHandler>>,
    reverse_handlers: Mutex<HashMap<String, ReverseHandler>>,
    waiters: Mutex<Vec<EventWaiter>>,
    write: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    capabilities: Mutex<Option<DapCapabilities>>,
    last_activity: AtomicU64,
    disposed: AtomicBool,
    closed: watch::Sender<Option<String>>,
    stderr_tail: Mutex<String>,
}

impl Inner {
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn touch(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        self.last_activity.store(now, Ordering::Relaxed);
    }

    /// Reject every in-flight request and wake every event waiter with `error`.
    fn fail_connection(&self, error: &str) {
        let pending: Vec<PendingRequest> = {
            let mut guard = self.pending.lock();
            guard.drain().map(|(_, p)| p).collect()
        };
        for entry in pending {
            let _ = entry.tx.send(Err(DapError::Transport(error.to_string())));
        }
        let waiters: Vec<EventWaiter> = {
            let mut guard = self.waiters.lock();
            guard.drain(..).collect()
        };
        for mut waiter in waiters {
            if let Some(tx) = waiter.tx.take() {
                let _ = tx.send(None);
            }
        }
        let _ = self.closed.send(Some(error.to_string()));
    }
}

/// A live DAP connection to one adapter process (or TCP endpoint).
pub struct DapClient {
    adapter: DapResolvedAdapter,
    cwd: PathBuf,
    inner: Arc<Inner>,
    child: Mutex<Option<tokio::process::Child>>,
    port: Option<u16>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
}

/// Options for [`DapClient::spawn`].
#[derive(Debug, Clone)]
pub struct DapSpawnOptions {
    /// Adapter to spawn.
    pub adapter: DapResolvedAdapter,
    /// Working directory of the adapter process.
    pub cwd: PathBuf,
    /// Override for how long socket-mode helpers wait for readiness.
    pub socket_ready_timeout: Option<Duration>,
}

impl DapClient {
    /// Spawn an adapter and connect to it over the transport its config selects.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Spawn`] when the process cannot be started or its
    /// transport never becomes reachable.
    pub async fn spawn(options: DapSpawnOptions) -> DapResult<Arc<Self>> {
        match options.adapter.connect_mode {
            DapConnectMode::Stdio => Self::spawn_stdio(&options).await,
            DapConnectMode::Socket => Self::spawn_socket(&options).await,
            DapConnectMode::Tcp => Self::spawn_tcp(&options).await,
        }
    }

    fn base_command(adapter: &DapResolvedAdapter, cwd: &PathBuf) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&adapter.resolved_command);
        command
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in NON_INTERACTIVE_ENV {
            command.env(key, value);
        }
        // A dedicated process group keeps the adapter's tree off our controlling
        // terminal: a debuggee that opens `/dev/tty` would otherwise take SIGTTIN
        // and suspend the harness under shell job control.
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    async fn spawn_stdio(options: &DapSpawnOptions) -> DapResult<Arc<Self>> {
        let mut command = Self::base_command(&options.adapter, &options.cwd);
        command.args(&options.adapter.args);
        let mut child = command.spawn().map_err(|e| {
            DapError::Spawn(format!(
                "failed to start DAP adapter '{}': {e}",
                options.adapter.name
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DapError::Spawn("adapter stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DapError::Spawn("adapter stdout unavailable".to_string()))?;
        let stderr = child.stderr.take();
        Ok(Self::assemble(
            &options.adapter,
            &options.cwd,
            Box::new(stdin),
            Box::new(stdout),
            stderr,
            Some(child),
            None,
        ))
    }

    async fn spawn_tcp(options: &DapSpawnOptions) -> DapResult<Arc<Self>> {
        let host = "127.0.0.1";
        // Reserve a port by binding, then release it for the adapter.
        let listener = tokio::net::TcpListener::bind((host, 0u16))
            .await
            .map_err(|e| DapError::Spawn(format!("cannot reserve a TCP port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| DapError::Spawn(format!("cannot read the reserved port: {e}")))?
            .port();
        drop(listener);
        let args: Vec<String> = options
            .adapter
            .args
            .iter()
            .map(|arg| arg.replace(DAP_PORT_ARGUMENT, &port.to_string()))
            .collect();
        let mut command = Self::base_command(&options.adapter, &options.cwd);
        command.args(&args);
        let mut child = command.spawn().map_err(|e| {
            DapError::Spawn(format!(
                "failed to start DAP adapter '{}': {e}",
                options.adapter.name
            ))
        })?;
        let stderr = child.stderr.take();
        let ready_timeout = options.socket_ready_timeout.unwrap_or(SOCKET_READY_TIMEOUT);
        let stream = match connect_tcp_with_retry(host, port, ready_timeout, &mut child).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = child.start_kill();
                return Err(e);
            }
        };
        let (read_half, write_half) = stream.into_split();
        Ok(Self::assemble(
            &options.adapter,
            &options.cwd,
            Box::new(write_half),
            Box::new(read_half),
            stderr,
            Some(child),
            Some(port),
        ))
    }

    #[cfg(unix)]
    async fn spawn_socket(options: &DapSpawnOptions) -> DapResult<Arc<Self>> {
        let socket_path = PathBuf::from(format!(
            "/tmp/dap-{}-{}-{}.sock",
            options.adapter.name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let mut command = Self::base_command(&options.adapter, &options.cwd);
        command
            .args(&options.adapter.args)
            .arg(format!("--listen=unix:{}", socket_path.display()));
        let mut child = command.spawn().map_err(|e| {
            DapError::Spawn(format!(
                "failed to start DAP adapter '{}': {e}",
                options.adapter.name
            ))
        })?;
        let stderr = child.stderr.take();
        let ready_timeout = options.socket_ready_timeout.unwrap_or(SOCKET_READY_TIMEOUT);
        let deadline = tokio::time::Instant::now() + ready_timeout;
        loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket_path).await {
                let (read_half, write_half) = stream.into_split();
                return Ok(Self::assemble(
                    &options.adapter,
                    &options.cwd,
                    Box::new(write_half),
                    Box::new(read_half),
                    stderr,
                    Some(child),
                    None,
                ));
            }
            if child.try_wait().ok().flatten().is_some() {
                let _ = std::fs::remove_file(&socket_path);
                return Err(DapError::Spawn(format!(
                    "DAP adapter '{}' exited before its socket was ready",
                    options.adapter.name
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&socket_path);
                return Err(DapError::Spawn(format!(
                    "socket for DAP adapter '{}' was not ready after {}ms",
                    options.adapter.name,
                    ready_timeout.as_millis()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(not(unix))]
    async fn spawn_socket(options: &DapSpawnOptions) -> DapResult<Arc<Self>> {
        // Non-Linux socket mode: listen on a random port and let the adapter dial
        // back with `--client-addr`.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0u16))
            .await
            .map_err(|e| DapError::Spawn(format!("cannot reserve a TCP port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| DapError::Spawn(format!("cannot read the reserved port: {e}")))?
            .port();
        let mut command = Self::base_command(&options.adapter, &options.cwd);
        command
            .args(&options.adapter.args)
            .arg(format!("--client-addr=127.0.0.1:{port}"));
        let mut child = command.spawn().map_err(|e| {
            DapError::Spawn(format!(
                "failed to start DAP adapter '{}': {e}",
                options.adapter.name
            ))
        })?;
        let stderr = child.stderr.take();
        let ready_timeout = options.socket_ready_timeout.unwrap_or(SOCKET_READY_TIMEOUT);
        let accepted = tokio::time::timeout(ready_timeout, listener.accept()).await;
        let stream = match accepted {
            Ok(Ok((stream, _))) => stream,
            Ok(Err(e)) => {
                let _ = child.start_kill();
                return Err(DapError::Spawn(format!("accept failed: {e}")));
            }
            Err(_) => {
                let _ = child.start_kill();
                return Err(DapError::Spawn(format!(
                    "DAP adapter '{}' did not connect within {}ms",
                    options.adapter.name,
                    ready_timeout.as_millis()
                )));
            }
        };
        let (read_half, write_half) = stream.into_split();
        Ok(Self::assemble(
            &options.adapter,
            &options.cwd,
            Box::new(write_half),
            Box::new(read_half),
            stderr,
            Some(child),
            Some(port),
        ))
    }

    /// Connect to another session on an existing TCP DAP server (child sessions).
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Spawn`] when the connection cannot be established.
    pub async fn connect(
        adapter: DapResolvedAdapter,
        cwd: PathBuf,
        host: &str,
        port: u16,
    ) -> DapResult<Arc<Self>> {
        let stream = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|e| DapError::Spawn(format!("cannot connect to {host}:{port}: {e}")))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self::assemble(
            &adapter,
            &cwd,
            Box::new(write_half),
            Box::new(read_half),
            None,
            None,
            Some(port),
        ))
    }

    fn assemble(
        adapter: &DapResolvedAdapter,
        cwd: &Path,
        write: Box<dyn AsyncWrite + Unpin + Send>,
        read: Box<dyn AsyncRead + Unpin + Send>,
        stderr: Option<tokio::process::ChildStderr>,
        child: Option<tokio::process::Child>,
        port: Option<u16>,
    ) -> Arc<Self> {
        let (closed_tx, _) = watch::channel(None::<String>);
        let inner = Arc::new(Inner {
            adapter_name: adapter.name.clone(),
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            event_handlers: Mutex::new(HashMap::new()),
            any_event_handlers: Mutex::new(Vec::new()),
            reverse_handlers: Mutex::new(HashMap::new()),
            waiters: Mutex::new(Vec::new()),
            write: tokio::sync::Mutex::new(write),
            capabilities: Mutex::new(None),
            last_activity: AtomicU64::new(0),
            disposed: AtomicBool::new(false),
            closed: closed_tx,
            stderr_tail: Mutex::new(String::new()),
        });
        let client = Arc::new(Self {
            adapter: adapter.clone(),
            cwd: cwd.to_path_buf(),
            inner: Arc::clone(&inner),
            child: Mutex::new(child),
            port,
            reader: Mutex::new(None),
            stderr_task: Mutex::new(None),
        });
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&inner);
            let handle = tokio::spawn(async move { drain_stderr(stderr, tail).await });
            *client.stderr_task.lock() = Some(handle);
        }
        let reader = Self::start_reader(&client, read);
        *client.reader.lock() = Some(reader);
        client
    }

    fn start_reader(
        client: &Arc<Self>,
        mut read: Box<dyn AsyncRead + Unpin + Send>,
    ) -> JoinHandle<()> {
        let inner = Arc::clone(&client.inner);
        tokio::spawn(async move {
            let mut framer = Framer::new();
            let mut chunk = vec![0u8; READ_CHUNK];
            loop {
                match read.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => framer.push(&chunk[..n]),
                }
                while let Some(text) = framer.next() {
                    inner.touch();
                    match serde_json::from_str::<Value>(&text) {
                        Ok(message) => route_message(&inner, message).await,
                        Err(error) => warn!(
                            adapter = %inner.adapter_name,
                            "DAP message decode failed: {error}"
                        ),
                    }
                }
            }
            let stderr = inner.stderr_tail.lock().trim().to_string();
            let name = &inner.adapter_name;
            let close_error = if stderr.is_empty() {
                format!("DAP connection closed: {name} transport ended")
            } else {
                format!("DAP adapter exited: {stderr}")
            };
            inner.fail_connection(&close_error);
        })
    }

    /// Adapter name this client speaks to.
    #[must_use]
    pub fn adapter_name(&self) -> &str {
        &self.adapter.name
    }

    /// Adapter config this client was spawned from.
    #[must_use]
    pub fn adapter(&self) -> &DapResolvedAdapter {
        &self.adapter
    }

    /// Working directory of the adapter process.
    #[must_use]
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// TCP port this client's adapter listens on, for child sessions.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Capabilities from the `initialize` reply, once received.
    #[must_use]
    pub fn capabilities(&self) -> Option<DapCapabilities> {
        self.inner.capabilities.lock().clone()
    }

    /// Whether the transport is still usable.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.inner.disposed.load(Ordering::SeqCst) && self.inner.closed.borrow().is_none()
    }

    /// Whether the adapter process is still running (stdio/tcp/socket modes).
    #[must_use]
    pub fn child_running(&self) -> bool {
        let mut guard = self.child.lock();
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => self.is_alive(),
        }
    }

    /// Perform the `initialize` handshake and cache the capabilities.
    ///
    /// # Errors
    ///
    /// Propagates the request failure.
    pub async fn initialize(
        &self,
        args: Value,
        cancel: Option<&dyn Cancel>,
        timeout: Option<Duration>,
    ) -> DapResult<DapCapabilities> {
        let body = self
            .send_request("initialize", Some(args), cancel, timeout)
            .await?;
        let capabilities: DapCapabilities = match body {
            Value::Null => DapCapabilities::default(),
            other => serde_json::from_value(other).unwrap_or_default(),
        };
        *self.inner.capabilities.lock() = Some(capabilities.clone());
        Ok(capabilities)
    }

    /// Register a handler for one event name; dropping the returned guard removes it.
    pub fn on_event<F>(&self, event: &str, handler: F) -> EventSubscription
    where
        F: Fn(&str, Option<&Value>) + Send + Sync + 'static,
    {
        let arc: EventHandler = Arc::new(handler);
        self.inner
            .event_handlers
            .lock()
            .entry(event.to_string())
            .or_default()
            .push(Arc::clone(&arc));
        EventSubscription {
            inner: Arc::clone(&self.inner),
            event: event.to_string(),
            handler: arc,
        }
    }

    /// Register a handler for every event.
    pub fn on_any_event<F>(&self, handler: F) -> AnyEventSubscription
    where
        F: Fn(&str, Option<&Value>) + Send + Sync + 'static,
    {
        let arc: EventHandler = Arc::new(handler);
        self.inner.any_event_handlers.lock().push(Arc::clone(&arc));
        AnyEventSubscription {
            inner: Arc::clone(&self.inner),
            handler: arc,
        }
    }

    /// Register a handler for an adapter-initiated request (`runInTerminal`, …).
    pub fn on_reverse_request<F, Fut>(&self, command: &str, handler: F)
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DapResult<Value>> + Send + 'static,
    {
        let arc: ReverseHandler = Arc::new(move |args| Box::pin(handler(args)));
        self.inner
            .reverse_handlers
            .lock()
            .insert(command.to_string(), arc);
    }

    /// Wait for one event, optionally filtered by `predicate`.
    ///
    /// Subscribe *before* sending the command that triggers the event: the
    /// adapter may emit it in the same read chunk as the command's response.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Timeout`] on budget expiry, [`DapError::Aborted`] on
    /// caller cancellation, and [`DapError::Transport`] when the connection dies.
    pub async fn wait_for_event(
        &self,
        event: &str,
        predicate: Option<EventPredicate>,
        cancel: Option<&dyn Cancel>,
        timeout: Option<Duration>,
    ) -> DapResult<Value> {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Err(DapError::Aborted);
        }
        let (tx, rx) = oneshot::channel();
        self.inner.waiters.lock().push(EventWaiter {
            event: event.to_string(),
            predicate,
            tx: Some(tx),
        });
        let mut closed = self.inner.closed.subscribe();
        let budget = timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let deadline = tokio::time::Instant::now() + budget;

        let mut rx = rx;
        let wait = async {
            tokio::select! {
                received = &mut rx => received.ok(),
                changed = closed.changed() => {
                    let _ = changed;
                    None
                }
                () = tokio::time::sleep_until(deadline) => None,
                () = cancel_future(cancel) => None,
            }
        };
        match wait.await {
            Some(Some(body)) => Ok(body),
            _ => {
                if cancel.is_some_and(Cancel::is_cancelled) {
                    return Err(DapError::Aborted);
                }
                if let Some(error) = closed.borrow().clone() {
                    return Err(DapError::Transport(error));
                }
                Err(DapError::Timeout(format!(
                    "DAP event {event} timed out after {}ms",
                    budget.as_millis()
                )))
            }
        }
    }

    /// Send one request and await its response body.
    ///
    /// # Errors
    ///
    /// Returns [`DapError::Request`] when the adapter answers `success: false`,
    /// [`DapError::Timeout`] on budget expiry, [`DapError::Aborted`] on caller
    /// cancellation, and [`DapError::Transport`] when the connection dies.
    pub async fn send_request(
        &self,
        command: &str,
        args: Option<Value>,
        cancel: Option<&dyn Cancel>,
        timeout: Option<Duration>,
    ) -> DapResult<Value> {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Err(DapError::Aborted);
        }
        if self.inner.disposed.load(Ordering::SeqCst) {
            return Err(DapError::Transport(format!(
                "DAP adapter {} is not running",
                self.inner.adapter_name
            )));
        }
        let seq = self.inner.next_seq();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(
            seq,
            PendingRequest {
                command: command.to_string(),
                tx,
            },
        );
        self.inner.touch();
        let mut message = Map::new();
        message.insert("seq".to_string(), json!(seq));
        message.insert("type".to_string(), json!("request"));
        message.insert("command".to_string(), json!(command));
        if let Some(args) = args {
            message.insert("arguments".to_string(), args);
        }
        let frame = encode_frame(&Value::Object(message));
        // Fire the write in the background: a wedged stdin flush must not consume
        // the caller's budget. A write failure resolves the pending entry instead.
        let inner = Arc::clone(&self.inner);
        let write_future = async move {
            if let Err(error) = write_frame(&inner, &frame).await {
                let entry = inner.pending.lock().remove(&seq);
                if let Some(entry) = entry {
                    let _ = entry.tx.send(Err(error));
                }
            }
        };
        tokio::spawn(write_future);

        let budget = timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let deadline = tokio::time::Instant::now() + budget;
        let mut closed = self.inner.closed.subscribe();
        let mut rx = rx;
        let outcome = tokio::select! {
            received = &mut rx => Some(received),
            changed = closed.changed() => {
                let _ = changed;
                None
            }
            () = tokio::time::sleep_until(deadline) => None,
            () = cancel_future(cancel) => None,
        };
        match outcome {
            Some(Ok(result)) => result,
            Some(Err(_)) => {
                self.inner.pending.lock().remove(&seq);
                Err(DapError::Transport(format!(
                    "DAP request {command} was dropped"
                )))
            }
            None => {
                self.inner.pending.lock().remove(&seq);
                if cancel.is_some_and(Cancel::is_cancelled) {
                    return Err(DapError::Aborted);
                }
                if let Some(error) = closed.borrow().clone() {
                    return Err(DapError::Transport(error));
                }
                Err(DapError::Timeout(format!(
                    "DAP request {command} timed out after {}ms",
                    budget.as_millis()
                )))
            }
        }
    }

    /// Answer an adapter-initiated request.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the frame cannot be written.
    pub async fn send_response(
        &self,
        request_seq: u64,
        command: &str,
        success: bool,
        body: Option<Value>,
        message: Option<&str>,
    ) -> DapResult<()> {
        let seq = self.inner.next_seq();
        let mut map = Map::new();
        map.insert("seq".to_string(), json!(seq));
        map.insert("type".to_string(), json!("response"));
        map.insert("request_seq".to_string(), json!(request_seq));
        map.insert("success".to_string(), json!(success));
        map.insert("command".to_string(), json!(command));
        if let Some(message) = message {
            map.insert("message".to_string(), json!(message));
        }
        if let Some(body) = body {
            map.insert("body".to_string(), body);
        }
        let frame = encode_frame(&Value::Object(map));
        write_frame(&self.inner, &frame).await
    }

    /// Close the transport and kill the adapter process.
    pub async fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.inner.closed.send(Some(format!(
            "DAP adapter {} disposed",
            self.inner.adapter_name
        )));
        {
            let pending: Vec<PendingRequest> = {
                let mut guard = self.inner.pending.lock();
                guard.drain().map(|(_, p)| p).collect()
            };
            for entry in pending {
                let _ = entry.tx.send(Err(DapError::Transport(format!(
                    "DAP adapter {} disposed",
                    self.inner.adapter_name
                ))));
            }
        }
        let mut writer = self.inner.write.lock().await;
        let _ = writer.shutdown().await;
        drop(writer);
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        if let Some(handle) = self.reader.lock().take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_task.lock().take() {
            handle.abort();
        }
        debug!(adapter = %self.inner.adapter_name, "DAP client disposed");
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        if let Some(child) = self.child.lock().take() {
            let mut child = child;
            let _ = child.start_kill();
        }
    }
}

/// Removes one event handler when dropped.
pub struct EventSubscription {
    inner: Arc<Inner>,
    event: String,
    handler: EventHandler,
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        let mut handlers = self.inner.event_handlers.lock();
        if let Some(list) = handlers.get_mut(&self.event) {
            list.retain(|h| !Arc::ptr_eq(h, &self.handler));
            if list.is_empty() {
                handlers.remove(&self.event);
            }
        }
    }
}

/// Removes one any-event handler when dropped.
pub struct AnyEventSubscription {
    inner: Arc<Inner>,
    handler: EventHandler,
}

impl Drop for AnyEventSubscription {
    fn drop(&mut self) {
        self.inner
            .any_event_handlers
            .lock()
            .retain(|h| !Arc::ptr_eq(h, &self.handler));
    }
}

async fn cancel_future(cancel: Option<&dyn Cancel>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn write_frame(inner: &Arc<Inner>, frame: &str) -> DapResult<()> {
    let write = async {
        let mut writer = inner.write.lock().await;
        writer
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| DapError::Transport(format!("DAP write failed: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| DapError::Transport(format!("DAP flush failed: {e}")))
    };
    match tokio::time::timeout(WRITE_MESSAGE_TIMEOUT, write).await {
        Ok(result) => result,
        Err(_) => Err(DapError::Transport(format!(
            "DAP adapter {} write timed out after {}ms",
            inner.adapter_name,
            WRITE_MESSAGE_TIMEOUT.as_millis()
        ))),
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, inner: Arc<Inner>) {
    let mut chunk = vec![0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&chunk[..n]).into_owned();
                let mut tail = inner.stderr_tail.lock();
                tail.push_str(&text);
                if tail.len() > STDERR_TAIL_BYTES {
                    let cut = tail.len() - STDERR_TAIL_BYTES;
                    let cut = (0..=cut)
                        .rev()
                        .find(|idx| tail.is_char_boundary(*idx))
                        .unwrap_or(0);
                    tail.drain(..cut);
                }
            }
        }
    }
}

async fn route_message(inner: &Arc<Inner>, message: Value) {
    match message.get("type").and_then(Value::as_str) {
        Some("response") => handle_response(inner, &message),
        Some("event") => {
            let event = message
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let body = message.get("body").cloned().unwrap_or(Value::Null);
            dispatch_event(inner, &event, body);
        }
        Some("request") => handle_adapter_request(inner, &message).await,
        _ => {}
    }
}

fn handle_response(inner: &Arc<Inner>, message: &Value) {
    let Some(request_seq) = message.get("request_seq").and_then(Value::as_u64) else {
        return;
    };
    let Some(pending) = inner.pending.lock().remove(&request_seq) else {
        return;
    };
    let success = message
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if success {
        let body = message.get("body").cloned().unwrap_or(Value::Null);
        let _ = pending.tx.send(Ok(body));
        return;
    }
    let message_text = message.get("message").and_then(Value::as_str).map_or_else(
        || format!("DAP request {} failed", pending.command),
        ToString::to_string,
    );
    let _ = pending.tx.send(Err(DapError::Request(message_text)));
}

fn dispatch_event(inner: &Arc<Inner>, event: &str, body: Value) {
    let handlers: Vec<EventHandler> = inner
        .event_handlers
        .lock()
        .get(event)
        .cloned()
        .unwrap_or_default();
    let any_handlers: Vec<EventHandler> = inner.any_event_handlers.lock().clone();
    for handler in handlers.iter().chain(any_handlers.iter()) {
        handler(event, Some(&body));
    }
    // Wake matching `wait_for_event` waiters. The predicate sees the body; a
    // waiter that does not match stays registered for a later event.
    let mut resolved: Vec<oneshot::Sender<Option<Value>>> = Vec::new();
    {
        let mut waiters = inner.waiters.lock();
        let mut index = 0;
        while index < waiters.len() {
            let matches = waiters[index].event == event
                && waiters[index]
                    .predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate(&body));
            if matches {
                let mut waiter = waiters.remove(index);
                if let Some(tx) = waiter.tx.take() {
                    resolved.push(tx);
                }
            } else {
                index += 1;
            }
        }
    }
    for tx in resolved {
        let _ = tx.send(Some(body.clone()));
    }
}

async fn handle_adapter_request(inner: &Arc<Inner>, message: &Value) {
    let Some(request_seq) = message.get("seq").and_then(Value::as_u64) else {
        return;
    };
    let command = message
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = message.get("arguments").cloned().unwrap_or(Value::Null);
    let handler = inner.reverse_handlers.lock().get(&command).cloned();
    let Some(handler) = handler else {
        let text = format!("Unsupported DAP request: {command}");
        let body = json!({"error": {"id": 1, "format": text}});
        let frame = encode_response(
            request_seq,
            inner.next_seq(),
            &command,
            false,
            Some(body),
            Some(&text),
        );
        if let Err(error) = write_frame(inner, &frame).await {
            warn!(adapter = %inner.adapter_name, "failed to refuse {command}: {error}");
        }
        return;
    };
    match handler(args).await {
        Ok(body) => {
            let frame = encode_response(
                request_seq,
                inner.next_seq(),
                &command,
                true,
                Some(body),
                None,
            );
            if let Err(error) = write_frame(inner, &frame).await {
                warn!(adapter = %inner.adapter_name, "failed to answer {command}: {error}");
            }
        }
        Err(error) => {
            let text = error.to_string();
            let body = json!({"error": {"id": 1, "format": text}});
            let frame = encode_response(
                request_seq,
                inner.next_seq(),
                &command,
                false,
                Some(body),
                Some(&text),
            );
            if let Err(write_error) = write_frame(inner, &frame).await {
                warn!(adapter = %inner.adapter_name, "failed to refuse {command}: {write_error}");
            }
        }
    }
}

fn encode_response(
    request_seq: u64,
    seq: u64,
    command: &str,
    success: bool,
    body: Option<Value>,
    message: Option<&str>,
) -> String {
    let mut map = Map::new();
    map.insert("seq".to_string(), json!(seq));
    map.insert("type".to_string(), json!("response"));
    map.insert("request_seq".to_string(), json!(request_seq));
    map.insert("success".to_string(), json!(success));
    map.insert("command".to_string(), json!(command));
    if let Some(message) = message {
        map.insert("message".to_string(), json!(message));
    }
    if let Some(body) = body {
        map.insert("body".to_string(), body);
    }
    encode_frame(&Value::Object(map))
}

/// Encode one DAP frame: `Content-Length: <bytes>\r\n\r\n<json>`.
#[must_use]
pub fn encode_frame(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

async fn connect_tcp_with_retry(
    host: &str,
    port: u16,
    timeout: Duration,
    child: &mut tokio::process::Child,
) -> DapResult<tokio::net::TcpStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::net::TcpStream::connect((host, port)).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if child.try_wait().ok().flatten().is_some() {
                    return Err(DapError::Spawn(format!(
                        "adapter exited before TCP port {host}:{port} was ready ({error})"
                    )));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(DapError::Spawn(format!(
                        "TCP port {host}:{port} was not ready after {}ms",
                        timeout.as_millis()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Frame parser: `Content-Length` headers + JSON body, with resync on junk.
///
/// Adapters occasionally print to stdout; a header block without a
/// `Content-Length` is dropped and parsing resumes after it instead of stalling
/// on the same junk forever.
pub struct Framer {
    buf: Vec<u8>,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    /// An empty framer.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append bytes read from the transport.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered past the last complete frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Pop the next complete message body; `None` when more bytes are needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<String> {
        loop {
            let header_end = find_subslice(&self.buf, b"\r\n\r\n")?;
            let header_bytes = self.buf[..header_end].to_vec();
            let mut content_length: Option<usize> = None;
            let mut valid = true;
            for line in header_bytes.split(|&b| b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some((name, value)) = split_header(line) {
                    if name.eq_ignore_ascii_case(b"content-length") {
                        content_length = std::str::from_utf8(value)
                            .ok()
                            .and_then(|v| v.trim().parse().ok());
                    }
                } else if !line.is_empty() {
                    valid = false;
                }
            }
            let Some(len) = content_length else {
                if !valid {
                    warn!("DAP framing resync: header block without Content-Length");
                }
                self.buf.drain(..header_end + 4);
                continue;
            };
            let total = header_end + 4 + len;
            if self.buf.len() < total {
                return None;
            }
            let body = self.buf[header_end + 4..total].to_vec();
            self.buf.drain(..total);
            return Some(String::from_utf8_lossy(&body).into_owned());
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn split_header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let idx = line.iter().position(|&b| b == b':')?;
    Some((&line[..idx], &line[idx + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_encodes_content_length_in_bytes() {
        let frame = encode_frame(&json!({"seq": 1, "type": "request", "command": "threads"}));
        let body = r#"{"seq":1,"type":"request","command":"threads"}"#;
        assert_eq!(
            frame,
            format!("Content-Length: {}\r\n\r\n{body}", body.len())
        );
        assert!(frame.starts_with("Content-Length: 46\r\n\r\n"));
    }

    #[test]
    fn framer_yields_complete_messages_and_holds_partials() {
        let mut framer = Framer::new();
        framer.push(b"Content-Length: 8\r\n\r\n{\"a\": 1}");
        assert_eq!(framer.next().as_deref(), Some("{\"a\": 1}"));
        assert_eq!(framer.next(), None);
        // A partial body stays buffered until the rest arrives.
        framer.push(b"Content-Length: 11\r\n\r\n{\"b\"");
        assert_eq!(framer.next(), None);
        // Header line + CRLFCRLF + the 4 body bytes that arrived so far.
        assert_eq!(framer.buffered(), "Content-Length: 11\r\n\r\n".len() + 4);
        assert_eq!(framer.buffered(), 26);
        framer.push(b": true}");
        assert_eq!(framer.next().as_deref(), Some("{\"b\": true}"));
    }

    #[test]
    fn framer_resyncs_past_junk_without_content_length() {
        let mut framer = Framer::new();
        framer.push(b"Debugger listening on port 1234\r\n\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(framer.next().as_deref(), Some("{}"));
        assert_eq!(framer.next(), None);
    }

    #[test]
    fn framer_accepts_case_insensitive_headers() {
        let mut framer = Framer::new();
        framer.push(b"content-length: 4\r\n\r\nnull");
        assert_eq!(framer.next().as_deref(), Some("null"));
    }

    #[test]
    fn framer_counts_content_length_in_bytes_not_chars() {
        let body = "{\"s\": \"héllo\"}";
        let mut framer = Framer::new();
        framer.push(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
        assert_eq!(framer.next().as_deref(), Some(body));
    }

    fn test_inner() -> Arc<Inner> {
        let (closed_tx, _) = watch::channel(None::<String>);
        Arc::new(Inner {
            adapter_name: "test".to_string(),
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            event_handlers: Mutex::new(HashMap::new()),
            any_event_handlers: Mutex::new(Vec::new()),
            reverse_handlers: Mutex::new(HashMap::new()),
            waiters: Mutex::new(Vec::new()),
            write: tokio::sync::Mutex::new(Box::new(tokio::io::sink())),
            capabilities: Mutex::new(None),
            last_activity: AtomicU64::new(0),
            disposed: AtomicBool::new(false),
            closed: closed_tx,
            stderr_tail: Mutex::new(String::new()),
        })
    }

    #[test]
    fn dispatch_event_wakes_only_matching_waiters() {
        let inner = test_inner();
        let (tx, mut rx) = oneshot::channel();
        inner.waiters.lock().push(EventWaiter {
            event: "stopped".to_string(),
            predicate: Some(Arc::new(|body: &Value| {
                body.get("reason").and_then(Value::as_str) == Some("breakpoint")
            })),
            tx: Some(tx),
        });
        // A different event name leaves the waiter registered.
        dispatch_event(&inner, "output", json!({"output": "x"}));
        assert!(rx.try_recv().is_err());
        assert_eq!(inner.waiters.lock().len(), 1);
        // Same name, predicate miss: still registered.
        dispatch_event(&inner, "stopped", json!({"reason": "step"}));
        assert!(rx.try_recv().is_err());
        assert_eq!(inner.waiters.lock().len(), 1);
        // Matching body resolves and removes the waiter.
        dispatch_event(&inner, "stopped", json!({"reason": "breakpoint"}));
        assert_eq!(
            rx.try_recv().expect("waiter resolved"),
            Some(json!({"reason": "breakpoint"}))
        );
        assert!(inner.waiters.lock().is_empty());
    }

    #[test]
    fn dispatch_event_calls_every_registered_handler() {
        let inner = test_inner();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let one = Arc::clone(&seen);
        inner.event_handlers.lock().insert(
            "output".to_string(),
            vec![Arc::new(move |event: &str, _body: Option<&Value>| {
                one.lock().push(event.to_string());
            })],
        );
        let two = Arc::clone(&seen);
        inner.any_event_handlers.lock().push(Arc::new(
            move |event: &str, _body: Option<&Value>| {
                two.lock().push(format!("any:{event}"));
            },
        ));
        dispatch_event(&inner, "output", json!({"output": "hello"}));
        assert_eq!(*seen.lock(), ["output", "any:output"]);
    }

    #[test]
    fn fail_connection_rejects_pending_and_wakes_waiters() {
        let inner = test_inner();
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().insert(
            1,
            PendingRequest {
                command: "threads".to_string(),
                tx,
            },
        );
        let (wtx, wrx) = oneshot::channel();
        inner.waiters.lock().push(EventWaiter {
            event: "stopped".to_string(),
            predicate: None,
            tx: Some(wtx),
        });
        inner.fail_connection("adapter died");
        assert!(matches!(
            rx.blocking_recv(),
            Ok(Err(DapError::Transport(message))) if message == "adapter died"
        ));
        assert_eq!(wrx.blocking_recv(), Ok(None));
        assert!(inner.pending.lock().is_empty());
        assert!(inner.waiters.lock().is_empty());
    }

    #[test]
    fn failed_response_carries_the_adapter_message() {
        let inner = test_inner();
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().insert(
            7,
            PendingRequest {
                command: "pause".to_string(),
                tx,
            },
        );
        handle_response(
            &inner,
            &json!({"seq": 3, "type": "response", "request_seq": 7,
                    "success": false, "command": "pause", "message": "notStopped"}),
        );
        assert!(matches!(
            rx.blocking_recv(),
            Ok(Err(DapError::Request(message))) if message == "notStopped"
        ));
    }

    #[test]
    fn successful_response_returns_the_body() {
        let inner = test_inner();
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().insert(
            4,
            PendingRequest {
                command: "threads".to_string(),
                tx,
            },
        );
        handle_response(
            &inner,
            &json!({"seq": 2, "type": "response", "request_seq": 4, "success": true,
                    "command": "threads", "body": {"threads": [{"id": 1, "name": "main"}]}}),
        );
        assert_eq!(
            rx.blocking_recv(),
            Ok(Ok(json!({"threads": [{"id": 1, "name": "main"}]})))
        );
    }

    #[test]
    fn response_without_body_resolves_to_null() {
        let inner = test_inner();
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().insert(
            5,
            PendingRequest {
                command: "configurationDone".to_string(),
                tx,
            },
        );
        handle_response(
            &inner,
            &json!({"seq": 2, "type": "response", "request_seq": 5, "success": true,
                    "command": "configurationDone"}),
        );
        assert_eq!(rx.blocking_recv(), Ok(Ok(Value::Null)));
    }

    #[test]
    fn unknown_request_seq_is_ignored() {
        let inner = test_inner();
        handle_response(
            &inner,
            &json!({"seq": 2, "type": "response", "request_seq": 99, "success": true,
                    "command": "threads"}),
        );
        assert!(inner.pending.lock().is_empty());
    }

    #[test]
    fn encode_response_keeps_protocol_spelling() {
        let frame = encode_response(
            3,
            8,
            "runInTerminal",
            true,
            Some(json!({"processId": 42})),
            None,
        );
        let body = r#"{"seq":8,"type":"response","request_seq":3,"success":true,"command":"runInTerminal","body":{"processId":42}}"#;
        assert_eq!(
            frame,
            format!("Content-Length: {}\r\n\r\n{body}", body.len())
        );
    }
}
