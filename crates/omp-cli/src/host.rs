//! `omt --mode rpc --multi-session --session-runtime in-process`: one process hosting several RPC
//! sessions on one stdin/stdout connection. The OmOMeow gateway's `MoveHost` moves sessions
//! through it (`open_session`, `extension_request`, `release_session`, `move_session`).
//!
//! Every hosted session runs on its own thread with its own current-thread runtime and
//! [`SessionScope`], serving exactly the command loop a single-session `--mode rpc` process
//! serves. The host owns stdin and stdout: it answers the host commands itself, routes every other
//! command to the session its `sessionId` names, and stamps that `sessionId` on every frame a
//! session sends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{bail, Result};
use omp_session::move_bundle;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::cli::Args;
use crate::rpc::{response, StartOptions};
use crate::runtime::{CmdError, CmdResult, Runtime};
use crate::scope::SessionScope;

/// Commands the host answers itself.
const HOST_COMMANDS: &[&str] = &[
    "open_session",
    "close_session",
    "list_sessions",
    "release_session",
    "move_session",
];
/// The frame a session's forwarder stops at once everything queued before it went out.
const FORWARD_DONE: &str = "__omt_session_done";
/// How long an interrupting release waits for the session's commands to finish.
const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What every hosted session is built from: the launch flags.
struct Launch {
    cwd: PathBuf,
    model: Option<String>,
    api_key: Option<String>,
    thinking: Option<String>,
    extensions: Vec<PathBuf>,
    no_extensions: bool,
    no_session: bool,
    no_ui: bool,
    prompt: crate::sysprompt::Options,
}

impl Launch {
    /// Read the flags and install what stays process-wide in a host: the approval gate, the context
    /// strategy, the sandbox and the extra skill roots.
    fn install(args: Args) -> Result<Self> {
        let cwd = match &args.cwd {
            Some(c) => c.canonicalize()?,
            None => std::env::current_dir()?,
        };
        crate::sysprompt::set_skill_paths(args.skill_paths.clone());
        let settings = omp_settings::Settings::load(&cwd);
        let strategy = crate::context::Kind::resolve(args.context_strategy.as_deref(), &settings)
            .map_err(|e| anyhow::anyhow!(e))?;
        crate::context::set_active(strategy);
        if args.sandbox || settings.get_bool("sandbox.enabled") == Some(true) {
            crate::features::sandbox::set_enabled(true);
        }
        crate::approval::Gate::install(
            args.approval_mode
                .as_deref()
                .and_then(crate::approval::parse_mode),
            args.auto_approve,
            &cwd,
            None,
        );
        if args.no_model_fallback {
            crate::runtime::MODEL_FALLBACK_DISABLED.store(true, Ordering::Relaxed);
        }
        if !args.no_ui {
            crate::slash_extra::mark_interactive_ui();
        }
        let model = match (&args.model, &args.provider) {
            (Some(m), Some(p)) if !m.contains('/') => Some(format!("{p}/{m}")),
            (Some(m), _) => Some(m.clone()),
            (None, Some(p)) => Some(p.clone()),
            (None, None) => None,
        };
        let tools_csv = if args.no_tools {
            Some("__none__".to_string())
        } else {
            args.tools_csv.clone()
        };
        Ok(Self {
            prompt: crate::sysprompt::Options {
                system_prompt: args.system_prompt.clone(),
                append_system_prompt: args.append_system_prompt.clone(),
                tools_csv,
                subagent: false,
                compact_prompt: args.compact_prompt,
                model: args.model.clone().unwrap_or_default(),
                generated_append: Vec::new(),
                prompt: args.prompt.clone(),
            },
            extensions: args
                .extension
                .iter()
                .cloned()
                .chain(args.hooks.iter().map(PathBuf::from))
                .collect(),
            cwd,
            model,
            api_key: args.api_key,
            thinking: args.thinking,
            no_extensions: args.no_extensions,
            no_session: args.no_session,
            no_ui: args.no_ui,
        })
    }
}

/// The command and release channels of a live session; dropping them starts its teardown.
struct Channels {
    commands: mpsc::UnboundedSender<Value>,
    releases: mpsc::UnboundedSender<Value>,
}

/// One hosted session, as the router sees it.
struct Hosted {
    /// Read live: a `switch_session` rebinds the runtime to another file.
    rt: std::sync::Weak<Runtime>,
    cwd: PathBuf,
    /// `None` once the session tore down.
    channels: Mutex<Option<Channels>>,
    /// Set when a release starts: commands routed meanwhile are refused (`session_closing`), so
    /// nothing reaches a session after its seal. The command channel stays open until then, since
    /// the session's own loop must live to run the release.
    closing: std::sync::atomic::AtomicBool,
    /// Commands accepted and not yet answered; the host counts them as it routes.
    inflight: Arc<watch::Sender<usize>>,
    exited: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Hosted {
    fn file(&self) -> Option<PathBuf> {
        self.rt.upgrade().and_then(|rt| rt.persist_file())
    }
}

struct Host {
    launch: Launch,
    out: mpsc::UnboundedSender<Value>,
    sessions: Mutex<HashMap<String, Arc<Hosted>>>,
    next: AtomicU64,
    /// Session-less `set_client_info`: applied to every session opened afterwards.
    client_info: Mutex<Option<Value>>,
    opening: tokio::sync::Mutex<()>,
    moving: tokio::sync::Mutex<()>,
    /// Every session thread's exit, awaited before the last frame is flushed.
    exits: Mutex<Vec<oneshot::Receiver<()>>>,
}

/// Serve stdin/stdout as a multi-session host until stdin closes.
///
/// # Errors
/// An unsupported `--session-runtime`, invalid launch flags, or a stdin read error.
pub async fn run(args: Args) -> Result<()> {
    match args.session_runtime.as_deref() {
        None | Some("in-process") => {}
        Some(other) => bail!(
            "omt: --session-runtime {other} is not supported; use --session-runtime in-process"
        ),
    }
    let launch = Launch::install(args)?;
    crate::scope::install_url_scope();
    let (out, frames) = mpsc::unbounded_channel::<Value>();
    let writer = crate::rpc::spawn_writer(frames);
    let host = Arc::new(Host {
        launch,
        out: out.clone(),
        sessions: Mutex::new(HashMap::new()),
        next: AtomicU64::new(0),
        client_info: Mutex::new(None),
        opening: tokio::sync::Mutex::new(()),
        moving: tokio::sync::Mutex::new(()),
        exits: Mutex::new(Vec::new()),
    });
    let _ = out.send(json!({
        "type": "ready",
        "protocolVersion": 1,
        "supportedProtocolVersions": [1, 2],
        "maxFrameBytes": crate::rpc::MAX_FRAME_BYTES,
        "maxReassembledFrameBytes": crate::rpc::MAX_REASSEMBLED_BYTES,
        "multiSession": true,
        "capabilities": capabilities(),
        "launchProfile": launch_profile(),
    }));
    let mut work = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match crate::rpc::parse_command(&line) {
                Ok(cmd) => host.route(cmd, &mut work),
                Err(frame) => {
                    let _ = host.out.send(frame);
                }
            }
            // Finished commands and closed sessions leave no bookkeeping behind while stdin
            // stays open.
            while work.try_join_next().is_some() {}
            lock(&host.exits).retain_mut(|exited| exited.try_recv().is_err());
        }
        anyhow::Ok(())
    }
    .await;
    // stdin closed: host tool calls can no longer be answered; every session finishes its
    // accepted work, then the host flushes the frames they sent.
    crate::rpc::close_host_tools();
    crate::rpc::close_ui_requests();
    while work.join_next().await.is_some() {}
    lock(&host.sessions).clear();
    let exits = std::mem::take(&mut *lock(&host.exits));
    for exited in exits {
        let _ = exited.await;
    }
    crate::rpc::finish_writer(&out, writer).await;
    read
}

fn capabilities() -> Vec<&'static str> {
    let mut all = vec!["multi_session"];
    all.extend(HOST_COMMANDS);
    all.extend(crate::rpc::RPC_COMMANDS);
    all
}

fn launch_profile() -> Value {
    json!({"multi_session": true, "session_runtime": "in-process"})
}

fn tagged(mut frame: Value, session_id: Option<&str>) -> Value {
    if let (Some(id), Some(obj)) = (session_id, frame.as_object_mut()) {
        obj.insert("sessionId".into(), json!(id));
    }
    frame
}

impl Host {
    fn reply(&self, cmd: &Value, command: &str, result: CmdResult, session_id: Option<&str>) {
        let _ = self
            .out
            .send(tagged(response(cmd.get("id"), command, result), session_id));
    }

    fn get(&self, session_id: &str) -> Option<Arc<Hosted>> {
        lock(&self.sessions).get(session_id).cloned()
    }

    /// Paths hosted here: an export or finish of one of them is refused (`session_open_here`).
    fn hosted_files(&self) -> Vec<PathBuf> {
        lock(&self.sessions)
            .values()
            .filter_map(|hosted| hosted.file())
            .collect()
    }

    fn route(self: &Arc<Self>, cmd: Value, work: &mut tokio::task::JoinSet<()>) {
        let kind = cmd
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let session = cmd
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match kind.as_str() {
            "extension_ui_response" => {
                if let Some(id) = cmd.get("id").and_then(Value::as_str) {
                    crate::rpc::resolve_pending_ui_request(id, cmd.clone());
                }
            }
            "host_tool_result" | "host_tool_update" => {
                crate::rpc::route_host_tool_frame(cmd);
            }
            "host_uri_result" => {}
            "get_protocol_info" => {
                self.reply(&cmd, &kind, Ok(Some(protocol_info())), session.as_deref());
            }
            "negotiate_protocol" if session.is_none() => {
                let result = match cmd.get("protocolVersion").and_then(Value::as_i64) {
                    Some(2) => Ok(Some(json!({"protocolVersion": 2}))),
                    other => Err(CmdError::new(format!(
                        "Unsupported RPC protocol version: {}",
                        other.map_or_else(|| "undefined".to_string(), |v| v.to_string())
                    ))),
                };
                self.reply(&cmd, &kind, result, None);
            }
            "set_client_info" if session.is_none() => {
                let result = match cmd.get("width").and_then(Value::as_u64) {
                    Some(_) => {
                        *lock(&self.client_info) = Some(cmd.clone());
                        Ok(None)
                    }
                    None => Err(CmdError::coded_params("width must be a number")),
                };
                self.reply(&cmd, &kind, result, None);
            }
            "list_sessions" => {
                let sessions: Vec<Value> = lock(&self.sessions)
                    .iter()
                    .map(|(id, h)| {
                        json!({"sessionId": id, "sessionFile": h.file(), "cwd": h.cwd,
                               "busy": *h.inflight.borrow() > 0})
                    })
                    .collect();
                self.reply(&cmd, &kind, Ok(Some(json!({"sessions": sessions}))), None);
            }
            "open_session" => {
                let host = self.clone();
                work.spawn(async move {
                    let result = host.clone().open(&cmd).await;
                    host.reply(&cmd, "open_session", result, None);
                });
            }
            "close_session" => {
                let host = self.clone();
                work.spawn(async move { host.close(&cmd, session).await });
            }
            "release_session" => self.release(&cmd, session),
            "move_session" => {
                let host = self.clone();
                work.spawn(async move {
                    let _turn = host.moving.lock().await;
                    let roots = crate::move_cmd::MoveRoots::engine();
                    let result = crate::move_cmd::handle(&cmd, host.hosted_files(), roots).await;
                    host.reply(&cmd, "move_session", result, None);
                });
            }
            _ => match session {
                None => self.reply(
                    &cmd,
                    &kind,
                    Err(CmdError::coded(
                        format!("{kind} needs the sessionId of an open session"),
                        "missing_session_id",
                    )),
                    None,
                ),
                Some(id) => match self.get(&id) {
                    Some(hosted) => {
                        let sent = match lock(&hosted.channels).as_ref() {
                            Some(channels) if !hosted.closing.load(Ordering::SeqCst) => {
                                // Counted here, before the session can answer: a release routed
                                // next must see this command as work in flight.
                                hosted.inflight.send_modify(|n| *n += 1);
                                channels.commands.send(cmd.clone()).is_ok()
                            }
                            _ => false,
                        };
                        if !sent {
                            self.reply(
                                &cmd,
                                &kind,
                                Err(CmdError::coded("the session is closing", "session_closing")),
                                Some(&id),
                            );
                        }
                    }
                    None => self.reply(
                        &cmd,
                        &kind,
                        Err(CmdError::coded(
                            format!("no open session {id}"),
                            "unknown_session",
                        )),
                        Some(&id),
                    ),
                },
            },
        }
    }

    /// `open_session {sessionPath?, cwd?, attended?}`: start a session on its own thread; the
    /// answer names it by its routing id.
    async fn open(self: Arc<Self>, cmd: &Value) -> CmdResult {
        let path = |key: &str| -> Result<Option<PathBuf>, CmdError> {
            match cmd.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(p)) if Path::new(p).is_absolute() => Ok(Some(PathBuf::from(p))),
                Some(_) => Err(CmdError::coded(
                    format!("{key} must be an absolute path"),
                    "invalid_path",
                )),
            }
        };
        let cwd = path("cwd")?.unwrap_or_else(|| self.launch.cwd.clone());
        let cwd = cwd
            .canonicalize()
            .map_err(|e| CmdError::coded(format!("cwd {}: {e}", cwd.display()), "invalid_path"))?;
        let session_path = path("sessionPath")?;
        let attended = cmd.get("attended").and_then(Value::as_bool) != Some(false);
        let _turn = self.opening.lock().await;
        if let Some(p) = &session_path {
            let wanted = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            let open = self
                .hosted_files()
                .iter()
                .any(|f| std::fs::canonicalize(f).unwrap_or_else(|_| f.clone()) == wanted);
            if open {
                return Err(CmdError::coded(
                    format!("{} is already open here", p.display()),
                    "session_path_in_use",
                ));
            }
        }
        let id = format!("rpc-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (opened, answer) = oneshot::channel();
        let (exited, exit) = oneshot::channel();
        lock(&self.exits).push(exit);
        let request = OpenRequest {
            id,
            cwd,
            session_path,
            attended,
        };
        let host = self.clone();
        std::thread::Builder::new()
            .name(format!("omt-{}", request.id))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = opened.send(Err(CmdError::coded(e.to_string(), "open_failed")));
                        return;
                    }
                };
                crate::scope::install(Arc::new(SessionScope::default()));
                runtime.block_on(session_main(host, request, opened, exited));
            })
            .map_err(|e| CmdError::coded(e.to_string(), "open_failed"))?;
        answer
            .await
            .unwrap_or_else(|_| Err(CmdError::coded("the session thread stopped", "open_failed")))
    }

    async fn close(&self, cmd: &Value, session: Option<String>) {
        let Some(id) = session else {
            return self.reply(
                cmd,
                "close_session",
                Err(CmdError::coded(
                    "close_session needs a sessionId",
                    "missing_session_id",
                )),
                None,
            );
        };
        let Some(hosted) = self.get(&id) else {
            return self.reply(
                cmd,
                "close_session",
                Err(CmdError::coded(
                    format!("no open session {id}"),
                    "unknown_session",
                )),
                Some(&id),
            );
        };
        // The file stays reserved until the session drained its accepted commands and tore down:
        // only the close that starts the teardown owns it, a second one just waits it out.
        let Some(exited) = lock(&hosted.exited).take() else {
            return self.reply(
                cmd,
                "close_session",
                Err(CmdError::coded("the session is closing", "session_closing")),
                Some(&id),
            );
        };
        hosted.closing.store(true, Ordering::SeqCst);
        drop(lock(&hosted.channels).take());
        drop(hosted);
        let _ = exited.await;
        lock(&self.sessions).remove(&id);
        self.reply(cmd, "close_session", Ok(None), Some(&id));
    }

    /// `release_session`: checked here, then run on the session's own thread; commands routed to
    /// the session meanwhile are refused.
    fn release(&self, cmd: &Value, session: Option<String>) {
        let Some(id) = session else {
            return self.reply(
                cmd,
                "release_session",
                Err(CmdError::coded(
                    "release_session needs a sessionId",
                    "missing_session_id",
                )),
                None,
            );
        };
        let reason = cmd.get("reason").and_then(Value::as_str);
        let move_id = cmd.get("moveId").and_then(Value::as_str);
        let valid = match reason {
            Some("takeover") => true,
            Some("move") => move_id.is_some_and(move_bundle::is_plain_id),
            _ => false,
        };
        if !valid {
            return self.reply(
                cmd,
                "release_session",
                Err(CmdError::coded(
                    "reason must be \"takeover\", or \"move\" with a plain moveId",
                    "invalid_release_reason",
                )),
                Some(&id),
            );
        }
        let Some(hosted) = self.get(&id) else {
            return self.reply(
                cmd,
                "release_session",
                Err(CmdError::coded(
                    format!("no open session {id}"),
                    "unknown_session",
                )),
                Some(&id),
            );
        };
        // Closing starts here: later commands are refused, and the request goes to the session's
        // own thread (its command loop must stay alive to run it).
        if hosted.closing.swap(true, Ordering::SeqCst) {
            return self.reply(
                cmd,
                "release_session",
                Err(CmdError::coded("the session is closing", "session_closing")),
                Some(&id),
            );
        }
        let releases = lock(&hosted.channels)
            .as_ref()
            .map(|channels| channels.releases.clone());
        let sent = releases.is_some_and(|releases| releases.send(cmd.clone()).is_ok());
        if !sent {
            self.reply(
                cmd,
                "release_session",
                Err(CmdError::coded("the session is closing", "session_closing")),
                Some(&id),
            );
        }
    }
}

fn protocol_info() -> Value {
    let mut commands: Vec<&str> = HOST_COMMANDS.to_vec();
    commands.extend(crate::rpc::RPC_COMMANDS);
    json!({
        "protocolVersion": crate::rpc::PROTOCOL_VERSION,
        "serverVersion": env!("CARGO_PKG_VERSION"),
        "engine": crate::rpc::ENGINE,
        "mode": "multi_session",
        "capabilities": capabilities(),
        "commands": commands,
        "agentDir": omp_providers::AgentDir::default().0.display().to_string(),
        "launchProfile": launch_profile(),
    })
}

struct OpenRequest {
    id: String,
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    attended: bool,
}

/// A live hosted session, on its own thread.
struct Session {
    rt: Arc<Runtime>,
    ext: Option<Arc<crate::ext::Extensions>>,
    /// It continues a session file that already existed.
    resumed: bool,
}

/// The session thread: open, serve until closed or released, tear down.
async fn session_main(
    host: Arc<Host>,
    request: OpenRequest,
    opened: oneshot::Sender<CmdResult>,
    exited: oneshot::Sender<()>,
) {
    if let Some(scope) = crate::scope::current() {
        *crate::scope::write(&scope.routing_id) = Some(request.id.clone());
    }
    let (frames, sent) = mpsc::unbounded_channel::<Value>();
    let forwarder = tokio::spawn(forward(sent, request.id.clone(), host.out.clone()));
    let session = match open_session(&host.launch, &request, frames.clone()).await {
        Ok(session) => session,
        Err(e) => {
            let _ = opened.send(Err(e));
            let _ = frames.send(json!({ FORWARD_DONE: true }));
            let _ = forwarder.await;
            let _ = exited.send(());
            return;
        }
    };
    if let Some(info) = lock(&host.client_info).clone() {
        if let Err(e) = session.rt.set_client_info(&info) {
            eprintln!("omt: {}: set_client_info: {}", request.id, e.message);
        }
    }
    let (commands, queued) = mpsc::unbounded_channel::<Value>();
    let (releases, mut release_rx) = mpsc::unbounded_channel::<Value>();
    let inflight = Arc::new(watch::channel(0usize).0);
    let (exit_tx, exit_rx) = oneshot::channel::<()>();
    let file = session.rt.persist_file();
    lock(&host.sessions).insert(
        request.id.clone(),
        Arc::new(Hosted {
            rt: Arc::downgrade(&session.rt),
            cwd: request.cwd.clone(),
            channels: Mutex::new(Some(Channels { commands, releases })),
            inflight: inflight.clone(),
            closing: std::sync::atomic::AtomicBool::new(false),
            exited: Mutex::new(Some(exit_rx)),
        }),
    );
    let _ = opened.send(Ok(Some(json!({
        "sessionId": request.id,
        "durableSessionId": session.rt.session_id(),
        "sessionFile": file,
        "cwd": request.cwd,
        "resumed": session.resumed,
    }))));
    let mut serving = tokio::spawn(crate::rpc::serve_session(
        session.rt.clone(),
        queued,
        inflight.clone(),
    ));
    let mut released = None;
    loop {
        tokio::select! {
            _ = &mut serving => break,
            Some(cmd) = release_rx.recv() => {
                match release(&session, &inflight, &cmd).await {
                    Ok(data) => {
                        released = Some((cmd, data));
                        break;
                    }
                    Err(e) => {
                        session.rt.send(response(cmd.get("id"), "release_session", Err(e)));
                    }
                }
            }
        }
    }
    // Closing: no more commands reach this session, and it leaves the registry only once its
    // accepted work drained and it tore down (its file stays reserved until then).
    if let Some(hosted) = host.get(&request.id) {
        drop(lock(&hosted.channels).take());
    }
    drop(release_rx);
    if !serving.is_finished() {
        let _ = serving.await;
    }
    if let Some(ext) = &session.ext {
        ext.shutdown().await;
    }
    if let Some((cmd, data)) = released {
        session
            .rt
            .send(response(cmd.get("id"), "release_session", Ok(Some(data))));
    }
    lock(&host.sessions).remove(&request.id);
    let _ = frames.send(json!({ FORWARD_DONE: true }));
    let _ = forwarder.await;
    let _ = exit_tx.send(());
    let _ = exited.send(());
}

/// Stamp the session's routing id on each frame it sends, in order.
async fn forward(
    mut sent: mpsc::UnboundedReceiver<Value>,
    id: String,
    out: mpsc::UnboundedSender<Value>,
) {
    while let Some(frame) = sent.recv().await {
        if frame.get(FORWARD_DONE).is_some() {
            break;
        }
        let _ = out.send(tagged(frame, Some(&id)));
    }
}

/// Build one session the way a single-session `--mode rpc` process starts: model, extensions,
/// agent, session file, runtime; then bind `sessionPath` (a missing file starts there).
async fn open_session(
    launch: &Launch,
    request: &OpenRequest,
    frames: mpsc::UnboundedSender<Value>,
) -> Result<Session, CmdError> {
    let failed =
        |what: &str, e: anyhow::Error| CmdError::coded(format!("{what}: {e:#}"), "open_failed");
    let cwd = &request.cwd;
    let mut selected = match crate::model::select(launch.model.as_deref()).await {
        Ok(selected) => selected,
        // The gateway's move host launches without `--model`, and a move never prompts: a
        // machine with no default model still opens the session; `set_model` can pick one.
        Err(e) if launch.model.is_none() => {
            eprintln!(
                "omt: {}: no model resolved ({e:#}); set_model before prompting",
                request.id
            );
            unresolved_model().map_err(|e| failed("model", e))?
        }
        Err(e) => return Err(failed("model", e)),
    };
    if !selected.fallbacks.is_empty() {
        let primary = format!("{}/{}", selected.model.provider, selected.model.id);
        let _ = crate::runtime::LAUNCH_FALLBACKS
            .set((primary, std::mem::take(&mut selected.fallbacks)));
    }
    if let Some(key) = launch.api_key.clone() {
        selected.api_key = Some(key);
        selected.credential_id = None;
    }
    if let Some(level) = launch.thinking.as_deref().filter(|l| *l != "auto") {
        selected.thinking = crate::runtime::parse_level(level).ok_or_else(|| {
            CmdError::coded(format!("unknown thinking level: {level}"), "open_failed")
        })?;
    }
    let mut ext = None;
    if !launch.no_extensions && crate::ext::has_extensions(&launch.extensions) {
        match crate::ext::Extensions::start(cwd, &launch.extensions).await {
            Ok(e) => ext = Some(e),
            Err(e) => eprintln!("omt: {}: extensions disabled: {e:#}", request.id),
        }
    }
    let extra_tools = match ext.as_ref() {
        Some(e) => e.tools().await.unwrap_or_default(),
        None => Vec::new(),
    };
    let (agent, target) = crate::agent::build_agent(
        cwd,
        selected,
        extra_tools,
        ext.as_ref(),
        &launch.prompt,
        crate::context::active(),
    )
    .map_err(|e| failed("agent", e))?;
    crate::tools::eval_bridge::set_session_tools(agent.tools().await);
    crate::tools::eval_agents::set_runtime_hooks(target.clone());
    let persist = crate::persist::attach_with(&agent, cwd, launch.no_session)
        .await
        .map_err(|e| failed("session file", e))?;
    if let Some(e) = ext.as_ref() {
        let model = agent.model().await;
        if let Err(err) = e
            .set_session_ctx(
                &persist.session_id(),
                persist.file().as_deref(),
                &format!("{}/{}", model.provider, model.id),
            )
            .await
        {
            eprintln!("omt: {}: extension ctx: {err:#}", request.id);
        }
    }
    let resumed = request.session_path.as_ref().is_some_and(|p| p.exists());
    if let Some(path) = &request.session_path {
        if !path.exists() {
            omp_session::SessionWriter::create_at(
                path,
                &cwd.display().to_string(),
                &crate::persist::uuid_v7(),
                &crate::persist::now_iso(),
            )
            .map_err(|e| failed("session file", e))?;
        }
    }
    // The requested file is bound before the startup hooks, so the extension host's ctx already
    // names it when the session attaches.
    let rt = crate::rpc::start_session(
        Arc::new(agent),
        target,
        cwd.clone(),
        persist,
        frames,
        StartOptions {
            resume: request.session_path.clone(),
            on_start: ext.as_ref().map(|e| e.rpc_on_start()),
            has_ui: request.attended && !launch.no_ui,
            ready: false,
        },
    )
    .await
    .map_err(|e| failed("session file", e))?;
    Ok(Session { rt, ext, resumed })
}

/// The stand-in model of a session opened without one: any provider call fails until `set_model`.
fn unresolved_model() -> Result<crate::model::Selected> {
    let model = serde_json::from_value(json!({
        "id": "unresolved",
        "name": "unresolved",
        "api": "openai-completions",
        "provider": "unresolved",
        "baseUrl": "",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128_000,
        "maxTokens": 16_384,
    }))?;
    Ok(crate::model::Selected {
        model,
        api_key: None,
        thinking: pa_agent::types::ThinkingLevel::Off,
        headers: None,
        credential_id: None,
        fallbacks: Vec::new(),
    })
}

/// What still runs in a session, by `release_session`'s signal names.
async fn busy(rt: &Runtime, inflight: &Arc<watch::Sender<usize>>) -> Vec<&'static str> {
    let mut signals = rt.busy_signals().await;
    if *inflight.borrow() > 0 {
        signals.push("request");
    }
    signals
}

fn busy_refusal(signals: &[&str]) -> CmdError {
    let code = if signals.iter().any(|s| matches!(*s, "turn" | "queued")) {
        "turn_active"
    } else {
        "session_busy"
    };
    CmdError::coded(
        format!(
            "{code}: the session is busy ({}); release with interrupt: true to stop it and take its queued input",
            signals.join(", ")
        ),
        code,
    )
}

/// `release_session` on the session's own thread (senpi `modes/rpc/session-release.ts`): refuse a
/// busy session unless `interrupt` (then stop it and take its queued input), stage and export the
/// extensions' state for a `move`, and seal the file with `session_released`. The caller tears
/// the session down once this succeeds.
async fn release(
    session: &Session,
    inflight: &Arc<watch::Sender<usize>>,
    cmd: &Value,
) -> Result<Value, CmdError> {
    let rt = &session.rt;
    let reason = cmd
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let move_id = (reason == "move")
        .then(|| cmd.get("moveId").and_then(Value::as_str))
        .flatten();
    // A session this host cannot hand over is refused before anything is taken out of it.
    if rt.persist_file().is_none() {
        return Err(CmdError::coded(
            "release_unsupported: the session has no file",
            "release_unsupported",
        ));
    }
    if move_id.is_some() && !move_bundle::is_plain_id(&rt.session_id()) {
        return Err(CmdError::coded(
            "release_unsupported: session_id_not_plain",
            "release_unsupported",
        ));
    }
    let signals = busy(rt, inflight).await;
    let interrupted = !signals.is_empty();
    let mut dropped = crate::runtime::QueuedInput::default();
    if interrupted {
        if cmd.get("interrupt").and_then(Value::as_bool) != Some(true) {
            return Err(busy_refusal(&signals));
        }
        dropped = rt.take_queued_input();
        rt.abort().await;
        rt.set(|s| s.abort_bash());
        // Admitted switches and turns finish first: the file (and durable id) this release
        // seals must be the one the session holds when it seals.
        let mut settled = inflight.subscribe();
        let _ = tokio::time::timeout(SETTLE_TIMEOUT, settled.wait_for(|n| *n == 0)).await;
    }
    let Some(file) = rt.persist_file() else {
        return Err(CmdError::coded(
            "release_unsupported: the session has no file",
            "release_unsupported",
        ));
    };
    let session_id = rt.session_id();
    if move_id.is_some() && !move_bundle::is_plain_id(&session_id) {
        return Err(CmdError::coded(
            "release_unsupported: session_id_not_plain",
            "release_unsupported",
        ));
    }
    // A refused release leaves the session as it found it: input it took goes back in its queue.
    let refuse = |error: CmdError, staged: Option<&move_bundle::StagedState>| {
        if let Some(staged) = staged {
            move_bundle::discard_outgoing_state(&staged.dir);
        }
        dropped.restore(rt);
        error
    };
    let staged = match move_id {
        Some(move_id) => Some(move_bundle::prepare_outgoing_state(&file, move_id).map_err(
            |e| {
                refuse(
                    CmdError::coded(format!("release_failed: {}", e.detail), "release_failed"),
                    None,
                )
            },
        )?),
        None => None,
    };
    if let (Some(move_id), Some(staged), Some(ext)) =
        (move_id, staged.as_ref(), session.ext.as_ref())
    {
        if let Err(e) = ext.export_moved_state(move_id, &staged.dir).await {
            return Err(refuse(
                CmdError::coded(format!("release_failed: {e}"), "release_failed"),
                Some(staged),
            ));
        }
    }
    let still = busy(rt, inflight).await;
    if !still.is_empty() {
        return Err(refuse(busy_refusal(&still), staged.as_ref()));
    }
    // The export waited on extensions, and a switch that outlived the settle timeout can still
    // have rebound the session meanwhile: the seal must name the file the session holds now.
    if rt.persist_file().as_deref() != Some(file.as_path()) {
        return Err(refuse(
            CmdError::coded(
                "release_failed: the session switched files during the release",
                "release_failed",
            ),
            staged.as_ref(),
        ));
    }
    let mut seal = json!({"reason": reason});
    if let (Some(move_id), Some(staged)) = (move_id, staged.as_ref()) {
        seal["move_id"] = json!(move_id);
        seal[move_bundle::STATE_ATTEMPT_FIELD] = json!(staged.attempt);
        seal["target"] = json!(cmd
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default());
        seal["session_id"] = json!(session_id);
    }
    seal["interrupted"] = json!(interrupted);
    seal["attachments"] = json!(0);
    seal["released_at"] = json!(crate::persist::now_iso());
    if let Err(e) = rt.append_entry(
        "custom",
        json!({"customType": move_bundle::SESSION_RELEASED, "data": seal}),
    ) {
        return Err(refuse(
            CmdError::coded(format!("release_failed: {e:#}"), "release_failed"),
            staged.as_ref(),
        ));
    }
    Ok(json!({
        "released": true,
        "session_path": file,
        "attachments": 0,
        "dropped": {"deliveries": [], "user_messages": dropped.user_messages()},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_the_routing_id_and_busy_refusals_name_their_signals() {
        let frame = tagged(
            json!({"type": "response", "command": "get_state"}),
            Some("rpc-2"),
        );
        assert_eq!(frame["sessionId"], "rpc-2");
        assert_eq!(
            tagged(json!({"type": "ready"}), None),
            json!({"type": "ready"})
        );
        let turn = busy_refusal(&["turn", "request"]);
        assert_eq!(turn.code, Some("turn_active"));
        assert!(turn.message.contains("turn, request"), "{}", turn.message);
        assert_eq!(busy_refusal(&["bash"]).code, Some("session_busy"));
    }

    #[test]
    fn protocol_info_names_the_host_commands_and_its_launch_profile() {
        let info = protocol_info();
        assert_eq!(info["mode"], "multi_session");
        assert_eq!(info["launchProfile"]["session_runtime"], "in-process");
        let commands: Vec<&str> = info["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for command in HOST_COMMANDS
            .iter()
            .chain(["extension_request", "get_state"].iter())
        {
            assert!(commands.contains(command), "{command}");
        }
    }
}
