//! `bash`: one shell command (stderr merged into stdout), omp's result text:
//! output, `Wall time: N.NN seconds`, `Command exited with code N`.
//! Local finite commands run in-process via `omp-shell` and `omp-shell-builtins`
//! (`tools/shell_host.rs`, with `PathPolicy`, `SpawnWrapper`, `-pgid` teardown,
//! and the `dyn` host bridge); `/ssh` commands execute remotely over external
//! `ssh`.
//!
//! omp's `async: true` backgrounds the command in the session job manager and
//! returns `formatBackgroundNotice`; the job is inspected via `read proc://`
//! (`tools/jobs.rs`). Long-lived services (`name`/`ready`) stay supervised
//! with a dedicated external `Child` handle (`SUPERVISOR_TICK` + `start_kill`)
//! killed when the session's job registry is dropped.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::jobs::{self, JobRegistry, JobStatus, Service, ServiceState};
use super::util::{
    enforce_inline_byte_cap, notice_with_artifact, resolve_to_cwd, save_artifact, sink_window,
    str_arg, truncate_tail, SpillConfig, DEFAULT_MAX_BYTES,
};

const DEFAULT_TIMEOUT_SECS: f64 = 300.0;
const MIN_TIMEOUT_SECS: f64 = 1.0;
const MAX_TIMEOUT_SECS: f64 = 3600.0;
/// Bytes held in memory while the command runs (the rest is dropped from the middle).
const RETAIN_BYTES: usize = 8 * DEFAULT_MAX_BYTES;
/// Lines one `notify` report carries; further matches are only counted
/// (omp `NOTIFY_MAX_LINES`).
const NOTIFY_MAX_LINES: usize = 50;
/// Characters kept of one reported line (omp `NOTIFY_MAX_LINE_CHARS`).
const NOTIFY_MAX_LINE_CHARS: usize = 400;
/// Bytes of an unterminated line buffered while waiting for its newline.
/// Past it the line is judged once from its bounded prefix and the rest of
/// the line is skipped, so a command that prints a huge line without
/// newlines cannot grow the buffer or make every read rescan it (a reported
/// line is clamped to [`NOTIFY_MAX_LINE_CHARS`] characters either way).
const NOTIFY_MAX_PARTIAL_BYTES: usize = NOTIFY_MAX_LINE_CHARS * 4;

/// omp `formatWallTimeNotice`.
#[must_use]
pub fn wall_time_notice(elapsed: Duration) -> String {
    format!("Wall time: {:.2} seconds", elapsed.as_secs_f64())
}

/// Bound over-long output the way omp's bash does: the `OutputSink` stream
/// window (middle elision past `DEFAULT_MAX_BYTES`, head per
/// `tools.artifactHeadBytes`), then `enforceInlineByteCap` at
/// `tools.artifactSpillThreshold` + slack. Anything cut is saved as an
/// artifact through `save` (returns its `artifact://` URL); returns the
/// inline body and, when only the stream window cut, its notice.
///
/// Past the spill threshold with a non-empty `query` (the request the call
/// serves), the inline body is instead the BM25-selected chunks within
/// `artifactHeadBytes + artifactTailBytes` (see [`super::relevance`]);
/// `OMP_TRUNC_BM25=0` keeps the head/tail path.
fn bound_output(
    output: &str,
    cfg: &SpillConfig,
    query: &str,
    save: impl Fn(&str) -> Option<String>,
) -> (String, Option<String>) {
    if crate::methodology::raw_tool_output_requested() {
        return (output.to_string(), None);
    }
    if output.len() > cfg.threshold && !query.trim().is_empty() && super::relevance::enabled() {
        let budget = match cfg.head_bytes + cfg.tail_bytes {
            0 => cfg.threshold,
            b => b.min(cfg.threshold),
        };
        if let Some(p) = super::relevance::pick(
            output,
            query,
            budget,
            None,
            (cfg.head_bytes, cfg.tail_bytes),
        ) {
            let url = save(output);
            return (
                p.content,
                Some(notice_with_artifact(&p.notice, url.as_deref())),
            );
        }
    }
    let (body, notice, mut artifact) = match sink_window(output, cfg) {
        Some(e) => {
            let url = save(output);
            let notice = notice_with_artifact(&e.notice, url.as_deref());
            (e.content, Some(notice), url)
        }
        None => (output.to_string(), None, None),
    };
    if body.len() <= cfg.inline_cap() {
        return (body, notice);
    }
    // The cap's marker and `[raw output: …]` footer replace the stream
    // notice, whose line ranges no longer describe the shown text.
    let capped = enforce_inline_byte_cap(&body, cfg.inline_cap(), || {
        artifact.take().or_else(|| save(output))
    });
    (capped, None)
}

fn timeout_arg(args: &Value) -> Option<f64> {
    match args.get("timeout").and_then(Value::as_f64) {
        None => Some(DEFAULT_TIMEOUT_SECS),
        Some(0.0) => None,
        Some(t) => Some(t.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)),
    }
}

/// Run a command and return `(output, exit code, wall time, outcome)`.
///
/// `bash` in its own session (TS: brush runs every external command under
/// `setsid`): the command has no controlling terminal, so nothing it starts
/// can open `/dev/tty` and flip the TUI's raw mode, steal the foreground
/// group, or be stopped by SIGTTIN. The workspace forbids `unsafe`
/// (`pre_exec`), so util-linux `setsid` does it; the spawned child is not a
/// group leader, so `setsid` execs bash in place (same pid, kill works).
fn detached_bash() -> tokio::process::Command {
    match setsid_path().as_deref() {
        Some(setsid) => {
            let mut cmd = tokio::process::Command::new(setsid);
            cmd.arg("--wait").arg("bash");
            cmd
        }
        None => tokio::process::Command::new("bash"),
    }
}

/// util-linux `setsid` when installed (shared by [`detached_bash`] and
/// [`local_bash_argv`]).
fn setsid_path() -> &'static Option<std::path::PathBuf> {
    static SETSID: std::sync::LazyLock<Option<std::path::PathBuf>> =
        std::sync::LazyLock::new(|| {
            ["/usr/bin/setsid", "/bin/setsid"]
                .into_iter()
                .map(std::path::PathBuf::from)
                .find(|p| p.is_file())
        });
    &SETSID
}

/// The tool command's local argv for `script`: `setsid --wait bash -c` when
/// util-linux `setsid` exists (own session, see [`detached_bash`]), plain
/// `bash -c` otherwise. Shared by the plain and the sandboxed spawn paths so
/// both use the same launcher discovery.
fn local_bash_argv(script: &str) -> Vec<String> {
    match setsid_path().as_deref() {
        Some(setsid) => vec![
            setsid.to_string_lossy().into_owned(),
            "--wait".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        None => vec!["bash".to_string(), "-c".to_string(), script.to_string()],
    }
}

/// A `notify` regex on a running command: the lines it matches are collected
/// (bounded) and reported with the finished result.
///
/// omp streams these as follow-ups in 200 ms batches with a refilling
/// 30-batch allowance (`async/notify.ts` `OutputNotifier`). omp-rs has no
/// follow-up channel on the job surface, so the port keeps omp's per-batch
/// bounds — [`NOTIFY_MAX_LINES`] reported lines per batch, each cut to
/// [`NOTIFY_MAX_LINE_CHARS`], blank lines never counted, the rest only
/// counted — and emits one batch when the command settles; the timer and the
/// allowance therefore collapse into that single bounded report.
pub(super) struct Notify {
    re: regex::Regex,
    /// The regex as given, quoted back in the report.
    source: String,
    state: Mutex<NotifyState>,
    /// A `monitor` command watch: matching lines go to this sink as they arrive
    /// instead of being kept for the settle-time report.
    sink: Option<tokio::sync::mpsc::UnboundedSender<WatchMsg>>,
}

/// What a streaming [`Notify`] tells its `monitor` while the job runs.
pub(super) enum WatchMsg {
    /// One line that matched the filter (cut to [`NOTIFY_MAX_LINE_CHARS`]).
    Line(String),
    /// The job reached its final state; the registry already holds it.
    Settled(JobStatus),
}

#[derive(Default)]
struct NotifyState {
    /// An unterminated line carried between chunks (a match is a whole line).
    /// Bounded by [`NOTIFY_MAX_PARTIAL_BYTES`].
    partial: String,
    /// Skipping the remainder of an over-long line already judged from its
    /// bounded prefix (its newline has not arrived yet).
    skipping: bool,
    lines: Vec<String>,
    omitted: usize,
}

impl Notify {
    /// Compile a `notify` regex; the caller's bad pattern fails the call.
    fn new(source: &str) -> Result<Self> {
        let re = regex::Regex::new(source).map_err(|e| anyhow!("Invalid notify regex: {e}"))?;
        Ok(Self {
            re,
            source: source.to_string(),
            state: Mutex::new(NotifyState::default()),
            sink: None,
        })
    }

    /// A `notify` that streams every matching line to `sink` (a `monitor` command).
    pub(super) fn streaming(
        source: &str,
        sink: tokio::sync::mpsc::UnboundedSender<WatchMsg>,
    ) -> Result<Self> {
        Ok(Self {
            sink: Some(sink),
            ..Self::new(source)?
        })
    }

    /// Tell a streaming watcher the job is final (after the registry update).
    fn settled(&self, status: JobStatus) {
        if let Some(sink) = &self.sink {
            let _ = sink.send(WatchMsg::Settled(status));
        }
    }

    /// Match the complete lines in `text`; the remainder waits for the next
    /// chunk so a line split across reads is still matched whole.
    fn feed(&self, text: &str) {
        self.collect(text, false);
    }

    /// End of output: the trailing unterminated line counts as a line.
    fn finish(&self) {
        self.collect("", true);
    }

    fn collect(&self, text: &str, end: bool) {
        let mut st = self.state.lock().expect("notify poisoned");
        let mut lines: Vec<String> = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            if st.skipping {
                // Inside an over-long line: only its newline matters.
                match rest.find('\n') {
                    Some(i) => {
                        st.skipping = false;
                        rest = &rest[i + 1..];
                    }
                    None => rest = "",
                }
                continue;
            }
            if let Some(i) = rest.find('\n') {
                let mut line = std::mem::take(&mut st.partial);
                line.push_str(&rest[..i]);
                lines.push(line);
                rest = &rest[i + 1..];
            } else {
                st.partial.push_str(rest);
                rest = "";
                if st.partial.len() > NOTIFY_MAX_PARTIAL_BYTES {
                    let pending = std::mem::take(&mut st.partial);
                    if !pending.trim().is_empty() && self.re.is_match(&pending) {
                        lines.push(pending);
                    }
                    st.skipping = true;
                }
            }
        }
        if end {
            if !st.skipping && !st.partial.is_empty() {
                lines.push(std::mem::take(&mut st.partial));
            }
            st.partial.clear();
            st.skipping = false;
        }
        for line in lines {
            if line.trim().is_empty() || !self.re.is_match(&line) {
                continue;
            }
            if let Some(sink) = &self.sink {
                let _ = sink.send(WatchMsg::Line(clamp_chars(&line, NOTIFY_MAX_LINE_CHARS)));
                continue;
            }
            if st.lines.len() < NOTIFY_MAX_LINES {
                st.lines.push(clamp_chars(&line, NOTIFY_MAX_LINE_CHARS));
            } else {
                st.omitted += 1;
            }
        }
    }

    /// The report block, or `None` when nothing matched.
    fn report(&self) -> Option<String> {
        let st = self.state.lock().expect("notify poisoned");
        if st.lines.is_empty() {
            return None;
        }
        let plural = if st.lines.len() == 1 { "" } else { "s" };
        let mut out = format!(
            "[notify /{}/: {} matching line{plural}]",
            self.source,
            st.lines.len()
        );
        for line in &st.lines {
            out.push('\n');
            out.push_str(line);
        }
        if st.omitted > 0 {
            use std::fmt::Write as _;
            let _ = write!(out, "\n… {} more matching lines", st.omitted);
        }
        Some(out)
    }
}

/// Decode `bytes` as UTF-8, carrying an incomplete trailing sequence over to
/// the next read instead of turning it into U+FFFD: a character split across
/// two `read`s (e.g. `0xC3` then `0xA9 0x0A`) arrives whole. Genuinely invalid
/// bytes still degrade to one U+FFFD per bad sequence, like `from_utf8_lossy`.
fn decode_chunk(bytes: &[u8], carry: &mut Vec<u8>) -> String {
    carry.extend_from_slice(bytes);
    let mut out = String::with_capacity(carry.len());
    let mut start = 0;
    loop {
        match std::str::from_utf8(&carry[start..]) {
            Ok(s) => {
                out.push_str(s);
                start = carry.len();
                break;
            }
            Err(e) => {
                let valid = start + e.valid_up_to();
                out.push_str(std::str::from_utf8(&carry[start..valid]).expect("valid prefix"));
                match e.error_len() {
                    // A truncated sequence at the end of the chunk: wait.
                    None => {
                        start = valid;
                        break;
                    }
                    Some(len) => {
                        out.push('\u{FFFD}');
                        start = valid + len;
                    }
                }
            }
        }
    }
    carry.drain(..start);
    out
}

/// Cut `s` to `max` characters (not bytes).
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// `tap`, when given, receives every output chunk as it arrives (the
/// background-job surface shows it via `read proc://<id>` while running).
/// `notify`, when given, matches every chunk as it arrives (see [`Notify`]).
async fn run(
    cwd: &std::path::Path,
    command: &str,
    timeout: Option<f64>,
    signal: &AbortSignal,
    notify: Option<&Notify>,
    tap: Option<Arc<Mutex<String>>>,
) -> Result<(String, Option<i32>, Duration, Option<&'static str>)> {
    run_with_dyn(cwd, command, timeout, signal, notify, tap, None).await
}

async fn run_with_dyn(
    cwd: &std::path::Path,
    command: &str,
    timeout: Option<f64>,
    signal: &AbortSignal,
    notify: Option<&Notify>,
    tap: Option<Arc<Mutex<String>>>,
    dyn_tools: Option<super::shell_host::DynToolList>,
) -> Result<(String, Option<i32>, Duration, Option<&'static str>)> {
    let script = format!("exec 2>&1\n{command}");
    if let Some(argv) = crate::features::ssh::remote_argv(&script) {
        return run_remote_ssh(cwd, &argv, timeout, signal, notify, tap).await;
    }
    let on_chunk: Option<super::shell_host::ChunkCallback<'_>> = notify
        .map(|n| Box::new(move |text: &str| n.feed(text)) as super::shell_host::ChunkCallback<'_>);
    let res = super::shell_host::execute_shell(super::shell_host::ShellExecRequest {
        cwd,
        command,
        timeout,
        signal,
        dyn_tools,
        retain_bytes: RETAIN_BYTES,
        on_chunk,
        tap,
    })
    .await?;
    Ok((res.output, res.exit_code, res.elapsed, res.outcome))
}

async fn run_remote_ssh(
    cwd: &std::path::Path,
    argv: &[String],
    timeout: Option<f64>,
    signal: &AbortSignal,
    notify: Option<&Notify>,
    tap: Option<Arc<Mutex<String>>>,
) -> Result<(String, Option<i32>, Duration, Option<&'static str>)> {
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new("ssh");
    for arg in argv {
        cmd.arg(arg);
    }
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn bash in {}", cwd.display()))?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    // Bytes of a multi-byte character still split across reads; decoded once
    // the rest arrives so a split sequence is never mangled into U+FFFD.
    let mut carry: Vec<u8> = Vec::new();
    let deadline = async {
        match timeout {
            Some(t) => tokio::time::sleep(Duration::from_secs_f64(t)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);
    let mut outcome = None;
    loop {
        tokio::select! {
            n = stdout.read(&mut chunk) => {
                let n = n.context("reading command output")?;
                if n == 0 { break; }
                if notify.is_some() || tap.is_some() {
                    let text = decode_chunk(&chunk[..n], &mut carry);
                    if let Some(notify) = notify {
                        notify.feed(&text);
                    }
                    if let Some(tap) = &tap {
                        let mut t = tap.lock().expect("job output tap poisoned");
                        t.push_str(&text);
                        // Bound the tap: proc reads only need the recent window.
                        if t.len() > RETAIN_BYTES {
                            let cut = super::util::floor_boundary(&t, t.len() - RETAIN_BYTES / 2);
                            t.drain(..cut);
                        }
                    }
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > RETAIN_BYTES * 2 {
                    // Keep the head and tail windows; drop the middle.
                    let tail = buf.split_off(buf.len() - RETAIN_BYTES / 2);
                    buf.truncate(RETAIN_BYTES / 2);
                    buf.extend_from_slice(&tail);
                }
            }
            () = &mut deadline => { outcome = Some("timeout"); break; }
            () = signal.aborted() => { outcome = Some("aborted"); break; }
        }
    }
    let code = if outcome.is_some() {
        let _ = child.kill().await;
        None
    } else {
        child.wait().await.context("waiting for command")?.code()
    };
    if !carry.is_empty() {
        // End of output: a truncated sequence now really is invalid text.
        let tail = String::from_utf8_lossy(&carry).into_owned();
        if let Some(notify) = notify {
            notify.feed(&tail);
        }
        if let Some(tap) = &tap {
            tap.lock().expect("job output tap poisoned").push_str(&tail);
        }
        carry.clear();
    }
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        code,
        started.elapsed(),
        outcome,
    ))
}

/// Outcome of a host-issued shell command (rpc `bash`), as omp's `BashResult`.
pub struct Executed {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub truncated: bool,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
}

/// Run `command` in `cwd` for the rpc `bash` command.
///
/// # Errors
/// Spawn or read failures.
pub async fn execute(
    cwd: &std::path::Path,
    command: &str,
    signal: &AbortSignal,
) -> Result<Executed> {
    let (raw, code, _, outcome) =
        run(cwd, command, Some(DEFAULT_TIMEOUT_SECS), signal, None, None).await?;
    let total_bytes = raw.len();
    let total_lines = raw.lines().count();
    let t = truncate_tail(&raw, DEFAULT_MAX_BYTES);
    Ok(Executed {
        output_bytes: t.content.len(),
        output_lines: t.content.lines().count(),
        truncated: t.truncated,
        output: t.content,
        exit_code: code,
        cancelled: outcome == Some("aborted"),
        timed_out: outcome == Some("timeout"),
        total_lines,
        total_bytes,
    })
}

/// Format a finished command as omp's bash result text (without error propagation).
fn finished_text(
    output: &str,
    elapsed: Duration,
    outcome: Option<&str>,
    timeout: Option<f64>,
    code: Option<i32>,
    cfg: &SpillConfig,
    query: &str,
) -> String {
    use std::fmt::Write as _;
    let (shown, elision) = bound_output(output.trim_end_matches('\n'), cfg, query, save_artifact);
    let body = if shown.is_empty() {
        "(no output)".to_string()
    } else {
        shown
    };
    let mut text = format!("{body}\n\n{}", wall_time_notice(elapsed));
    if let Some(n) = elision {
        let _ = write!(text, "\n\n[{n}]");
    }
    match outcome {
        Some("timeout") => {
            let secs = timeout.unwrap_or(0.0);
            let _ = write!(text, "\n\n[Command timed out after {secs} seconds]");
        }
        Some(_) => text.push_str("\n\n[Command aborted]"),
        None => {}
    }
    if outcome.is_none() {
        if let Some(c) = code {
            if c != 0 {
                let _ = write!(text, "\n\nCommand exited with code {c}");
            }
        } else {
            text.push_str("\n\nCommand failed: missing exit status");
        }
    }
    text
}

/// omp `ServiceReady` after `normalizeReady`: blank strings are dropped and a
/// spec with no surviving condition is `None` entirely.
#[derive(Default, Clone)]
struct ReadySpec {
    log: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    timeout: Option<f64>,
}

fn blank(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Parse `args["ready"]` (omp `normalizeReady`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // port fraction already validated 1..=65535 before the cast
fn parse_ready(args: &Value) -> Option<ReadySpec> {
    let r = &args["ready"];
    if !r.is_object() {
        return None;
    }
    let raw_port = r["port"].as_f64().filter(|p| p.is_finite());
    let port = raw_port
        .filter(|p| p.fract() == 0.0 && *p >= 1.0 && *p <= 65_535.0)
        .map(|p| p as u16);
    let spec = ReadySpec {
        log: blank(r["log"].as_str()),
        host: blank(r["host"].as_str()),
        port,
        timeout: r["timeout"].as_f64().filter(|t| t.is_finite()),
    };
    if spec.log.is_none() && spec.host.is_none() && spec.port.is_none() && spec.timeout.is_none() {
        return None;
    }
    Some(spec)
}

/// Whether `ready` carries any service-only field (omp `normalizeReady`'s
/// "is this a service request" test, used for the no-name notice).
fn ready_given(args: &Value) -> bool {
    parse_ready(args).is_some()
}

/// omp `startService` validation; `bail!`s with omp's `ToolError` texts.
fn validate_service(
    name: &str,
    ready: Option<&ReadySpec>,
    args: &Value,
    async_requested: bool,
) -> Result<()> {
    if !valid_service_name(name) {
        bail!("Service name must be 1-48 letters, numbers, dots, underscores, or hyphens");
    }
    if async_requested || args.get("timeout").is_some() {
        bail!("Service mode does not accept async or timeout; use ready.timeout for readiness.");
    }
    if let Some(ready) = ready {
        if let Some(port) = args["ready"]["port"].as_f64() {
            if ready.port.is_none() || port.fract() != 0.0 {
                bail!("ready.port must be an integer from 1 to 65535");
            }
        }
        if ready.log.is_none() && ready.port.is_none() {
            bail!("ready requires log or port");
        }
        if let Some(log) = &ready.log {
            if let Err(e) = regex::Regex::new(log) {
                bail!("Invalid readiness regex: {e}");
            }
        }
    }
    Ok(())
}

/// omp's service-name rule: `^[A-Za-z0-9][A-Za-z0-9._-]{0,47}$`.
fn valid_service_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() <= 48
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(*c, b'.' | b'_' | b'-'))
}

/// Run the `bash` tool.
///
/// # Errors
/// Spawn failure, timeout, abort or a non-zero exit (carrying the output);
/// disabled service mode (omp's error text).
pub async fn bash(
    env: std::sync::Arc<super::Env>,
    args: Value,
    signal: AbortSignal,
) -> Result<AgentToolResult> {
    let cwd = env.cwd.clone();
    let command = str_arg(&args, "command")?.to_string();
    let dir = args
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| cwd.clone(), |d| resolve_to_cwd(d, &cwd));
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let notify_source = args
        .get("notify")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Compiled before anything runs: a bad regex fails the call instead of a
    // job that never reports (omp `compileNotifyPattern`).
    let notify = notify_source.map(Notify::new).transpose()?.map(Arc::new);
    // omp: `async: true` or a `notify` regex on a plain command is an async
    // request; with a service `name` the captured log already shows every
    // line, so `notify` there only validates.
    let async_requested = args.get("async").and_then(Value::as_bool) == Some(true)
        || (notify.is_some() && name.is_none());
    let timeout = timeout_arg(&args);
    let mut notices: Vec<String> = Vec::new();
    if let Some(name) = name {
        let ready = parse_ready(&args);
        validate_service(name, ready.as_ref(), &args, async_requested)?;
        let timeout_secs = ready
            .as_ref()
            .and_then(|r| r.timeout)
            .unwrap_or(30.0)
            .clamp(0.05, 3_600.0);
        let outcome = start_service(
            &env.jobs,
            name,
            &dir,
            &command,
            ready.as_ref(),
            timeout_secs,
            &signal,
        )
        .await?;
        let mut lines = vec![format!(
            "{name}: {}{}{}",
            outcome.state.as_str(),
            outcome.pid.map(|p| format!(" pid={p}")).unwrap_or_default(),
            if outcome.ready { " ready" } else { "" }
        )];
        if outcome.timed_out {
            lines.push(format!(
                "NOT ready — readiness timed out after {timeout_secs}s; process state: {}.",
                outcome.state.as_str()
            ));
        }
        if let Some(reason) = &outcome.exit_reason {
            lines.push(format!("Reason: {reason}"));
        }
        if !outcome.log.is_empty() {
            lines.push(outcome.log);
        }
        // A service's captured log already carries every line, so `notify`
        // only tells the model where to look (omp `service.notifyArmed`).
        if let Some(notify) = &notify {
            lines.push(format!(
                "Output lines matching /{}/ arrive in proc://{name} while it runs.",
                notify.source
            ));
        }
        return Ok(AgentToolResult::text(lines.join("\n")));
    }
    if ready_given(&args) {
        // Nothing can honour ready without a service to attach it to (omp).
        notices.push("Ignored ready: service-only, and no service name was given.".to_string());
    }
    let spill = SpillConfig::load(&cwd);
    let query = env.query();
    if async_requested {
        return Ok(start_background(
            &env.jobs,
            dir,
            command,
            timeout,
            notices,
            spill,
            query,
            notify.as_ref(),
            env.dyn_tools(),
            &signal,
        ));
    }
    let (output, code, elapsed, outcome) = run_with_dyn(
        &dir,
        &command,
        timeout,
        &signal,
        None,
        None,
        env.dyn_tools(),
    )
    .await?;
    let mut text = finished_text(&output, elapsed, outcome, timeout, code, &spill, &query);
    if !notices.is_empty() {
        text = format!("{}\n{text}", notices.join("\n"));
    }
    if outcome.is_some() || code != Some(0) {
        bail!("{text}");
    }
    let mut result = AgentToolResult::text(text);
    // Transcript card facts (`⟦Wall: … | Timeout: …⟧`); failures keep the
    // error-text path, which carries no details.
    result.details = run_details(timeout, &args, elapsed);
    Ok(result)
}

/// omp `BashToolDetails` for a finished foreground run.
fn run_details(timeout: Option<f64>, args: &Value, elapsed: Duration) -> Value {
    let mut d = serde_json::Map::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // ms fit u64; whole seconds fit u64
    {
        d.insert(
            "wallTimeMs".into(),
            Value::from(elapsed.as_millis().min(u128::from(u64::MAX)) as u64),
        );
        match timeout {
            Some(t) => {
                d.insert("timeoutSeconds".into(), Value::from(t.round() as u64));
            }
            None => {
                d.insert("timeoutDisabled".into(), Value::Bool(true));
            }
        }
        if let (Some(req), Some(t)) = (args.get("timeout").and_then(Value::as_f64), timeout) {
            if req != 0.0 && (req - t).abs() > f64::EPSILON {
                d.insert(
                    "requestedTimeoutSeconds".into(),
                    Value::from(req.round() as u64),
                );
            }
        }
    }
    Value::Object(d)
}

/// What the bash tool reports after a service start attempt.
struct ServiceOutcome {
    state: ServiceState,
    pid: Option<u32>,
    ready: bool,
    timed_out: bool,
    exit_reason: Option<String>,
    log: String,
}

/// Readiness buffer window (omp broker `READINESS_BUFFER_CHARS`).
const READINESS_BUFFER_CHARS: usize = 64 * 1024;
/// Supervisor liveness tick: checks kill requests and registry liveness.
const SUPERVISOR_TICK: Duration = Duration::from_millis(100);

/// omp `startService`, in-proc: spawn the child piped, supervise it in a
/// background task, and wait for readiness (log regex and/or TCP port probe)
/// up to `timeout_secs`. Both conditions must hold when both are given (omp
/// `#markReady`). Returns once ready, once the process settles, or on timeout
/// (the service keeps running; `timed_out` is set, omp's `readyTimedOut`).
///
/// The supervisor owns the child and keeps draining its output after this
/// call returns (a full pipe would otherwise block the service); it kills the
/// child when the job registry is dropped (session end) or `kill_requested`
/// is set (`proc://<name>/kill`, tool-call abort).
///
/// # Errors
/// Spawn failure, or abort while waiting.
#[allow(clippy::too_many_lines)] // one supervisor task: spawn + readiness poll + liveness loop; mirrors omp's service supervisor
async fn start_service(
    registry: &JobRegistry,
    name: &str,
    dir: &std::path::Path,
    command: &str,
    ready: Option<&ReadySpec>,
    timeout_secs: f64,
    signal: &AbortSignal,
) -> Result<ServiceOutcome> {
    let script = format!("exec 2>&1\n{command}");
    let mut cmd = match crate::features::sandbox::wrap_inner(dir, &local_bash_argv(&script)) {
        Some((launcher, argv)) => {
            let mut c = tokio::process::Command::new(launcher);
            c.args(argv);
            c
        }
        None => {
            let mut c = detached_bash();
            c.arg("-c").arg(&script);
            c
        }
    };
    let mut child = cmd
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn bash in {}", dir.display()))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let pattern = ready
        .and_then(|r| r.log.as_ref())
        .map(|p| regex::Regex::new(p).expect("readiness regex validated"));
    let host = ready
        .and_then(|r| r.host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = ready.and_then(|r| r.port);
    let has_ready = ready.is_some();

    let service = Service {
        name: name.to_string(),
        command: command.to_string(),
        started: Instant::now(),
        state: if has_ready {
            ServiceState::Starting
        } else {
            ServiceState::Running
        },
        pid,
        ready_at: None,
        exited_at: None,
        exit_code: None,
        exit_reason: None,
        output: Arc::new(Mutex::new(String::new())),
        kill_requested: false,
    };
    let service = registry.insert_service(service);
    let inner = registry.inner.clone();
    let registry_liveness: std::sync::Weak<std::sync::Mutex<super::jobs::JobsInner>> =
        Arc::downgrade(&inner);

    // Supervisor: drains output (log capture + readiness buffer), polls the
    // port, watches for kill requests and registry death, settles on exit.
    let (notify_tx, mut notify_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut child = child;
        let mut notify = Some(notify_tx);
        let mut log_ready = pattern.is_none();
        let mut port_ready = port.is_none();
        let mut readiness_buffer = String::new();
        let mut chunk = [0u8; 8192];
        let mut tick = tokio::time::interval(SUPERVISOR_TICK);
        tick.tick().await; // first tick is immediate; skip it.
        loop {
            tokio::select! {
                n = stdout.read(&mut chunk) => {
                    match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&chunk[..n]).into_owned();
                            {
                                let svc = service.lock().expect("service poisoned");
                                let mut out = svc.output.lock().expect("service output poisoned");
                                out.push_str(&text);
                                if out.len() > RETAIN_BYTES {
                                    let cut =
                                        super::util::floor_boundary(&out, out.len() - RETAIN_BYTES / 2);
                                    out.drain(..cut);
                                }
                            }
                            if !log_ready {
                                readiness_buffer.push_str(&text);
                                if readiness_buffer.len() > READINESS_BUFFER_CHARS {
                                    let drop_to = super::util::floor_boundary(
                                        &readiness_buffer,
                                        readiness_buffer.len() - READINESS_BUFFER_CHARS,
                                    );
                                    readiness_buffer.drain(..drop_to);
                                }
                                if pattern.as_ref().is_some_and(|re| re.is_match(&readiness_buffer)) {
                                    log_ready = true;
                                }
                            }
                        }
                    }
                }
                _ = tick.tick() => {
                    {
                        let svc = service.lock().expect("service poisoned");
                        if svc.kill_requested || registry_liveness.upgrade().is_none() {
                            let _ = child.start_kill();
                        }
                    }
                    if !port_ready {
                        let Some(port) = port else { continue };
                        let addr = format!("{host}:{port}");
                        if TcpStream::connect(&addr).await.is_ok() {
                            port_ready = true;
                        }
                    }
                }
            }
            let ready_now = log_ready && port_ready;
            if ready_now {
                let mut svc = service.lock().expect("service poisoned");
                if svc.state == ServiceState::Starting {
                    svc.state = ServiceState::Ready;
                    svc.ready_at = Some(Instant::now());
                }
                if let Some(tx) = notify.take() {
                    let _ = tx.send(());
                }
            }
        }
        // EOF or a fatal read error: settle from the child's exit status.
        let status = child.wait().await;
        let mut svc = service.lock().expect("service poisoned");
        svc.exited_at = Some(Instant::now());
        svc.pid = None;
        match status {
            Ok(st) => {
                svc.exit_code = st.code();
                svc.state = if st.success() {
                    ServiceState::Exited
                } else {
                    ServiceState::Failed
                };
                if let Some(code) = st.code() {
                    if code != 0 {
                        svc.exit_reason = Some(format!("exited with code {code}"));
                    }
                } else {
                    svc.exit_reason = Some("killed by signal".to_string());
                }
            }
            Err(e) => {
                svc.state = ServiceState::Failed;
                svc.exit_reason = Some(e.to_string());
            }
        }
        if let Some(tx) = notify.take() {
            let _ = tx.send(());
        }
    });

    // Wait for readiness or terminal settlement, the timeout, or abort.
    let outcome = tokio::select! {
        _ = &mut notify_rx => false,
        () = tokio::time::sleep(Duration::from_secs_f64(timeout_secs)) => true,
        () = signal.aborted() => {
            if let Some(svc) = registry.service(name) {
                svc.lock().expect("service poisoned").kill_requested = true;
            }
            bail!("Command aborted");
        }
    };
    let svc_arc = registry.service(name).expect("service registered");
    let svc = svc_arc.lock().expect("service poisoned");
    Ok(ServiceOutcome {
        state: svc.state,
        pid: svc.pid,
        ready: svc.is_ready(),
        timed_out: outcome,
        exit_reason: svc.exit_reason.clone(),
        log: svc.log(),
    })
}

/// Append a `notify` report to a finished job's text, keeping the combined
/// inline result within the spill cap: the report is appended after
/// [`finished_text`] already capped the body, so without this second cap a
/// large report would push `job.result` (read verbatim through `proc://`) past
/// the inline budget.
fn append_notify_report(text: &str, report: &str, spill: &SpillConfig) -> String {
    let combined = format!("{text}\n\n{report}");
    // Raw-output mode is an explicit bypass: `bound_output` left the body
    // uncapped, so appending the report must not re-impose the inline cap
    // (that would silently drop output the caller asked to see verbatim).
    if crate::methodology::raw_tool_output_requested() {
        return combined;
    }
    if combined.len() <= spill.inline_cap() {
        return combined;
    }
    enforce_inline_byte_cap(&combined, spill.inline_cap(), || save_artifact(&combined))
}

/// omp `BashTool` async mode: register a `bg_N` job, run it detached, and
/// return `formatBackgroundNotice` (the result is read later via `proc://`).
#[allow(clippy::too_many_arguments)] // job plumbing: registry, command, limits, spill policy, request query
pub(super) fn start_background(
    registry: &JobRegistry,
    dir: std::path::PathBuf,
    command: String,
    timeout: Option<f64>,
    notices: Vec<String>,
    spill: SpillConfig,
    query: String,
    notify: Option<&Arc<Notify>>,
    dyn_tools: Option<super::shell_host::DynToolList>,
    _signal: &AbortSignal,
) -> AgentToolResult {
    let output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cancel = Arc::new(pa_agent::abort::AbortController::new());
    let job = jobs::Job {
        command: command.clone(),
        started: Instant::now(),
        status: JobStatus::Running,
        output: output.clone(),
        result: None,
        error: None,
        ended: None,
        read_at: Arc::new(Mutex::new(0)),
        cancel: cancel.clone(),
    };
    let job_id = registry.insert(job);
    let registry = registry.clone();
    let task_job_id = job_id.clone();
    let task_notify = notify.cloned();
    // A background job outlives the tool list that started it, and the `dyn`
    // bridge only holds weak handles, so the job's `dyn` devices would vanish
    // as soon as the caller rebuilt or dropped its list. Pin the live tools for
    // the job's lifetime only: they are dropped with the job task, so `Env`
    // never gains a strong link back to the tools that own it.
    let keep_alive: Vec<Arc<dyn AgentTool>> = dyn_tools.as_ref().map_or_else(Vec::new, |list| {
        list.iter().filter_map(std::sync::Weak::upgrade).collect()
    });
    tokio::spawn(async move {
        // Held for the whole job; released when this task ends.
        let _keep = keep_alive;
        let raw = run_with_dyn(
            &dir,
            &command,
            timeout,
            &cancel.signal(),
            task_notify.as_deref(),
            Some(output),
            dyn_tools,
        )
        .await;
        if let Some(notify) = &task_notify {
            notify.finish();
        }
        let (status, result, error) = match &raw {
            Ok((output, code, elapsed, outcome)) => {
                let mut text =
                    finished_text(output, *elapsed, *outcome, timeout, *code, &spill, &query);
                if let Some(report) = task_notify.as_ref().and_then(|n| n.report()) {
                    text = append_notify_report(&text, &report, &spill);
                }
                let status = if cancel.signal().is_aborted() {
                    JobStatus::Killed
                } else if outcome.is_none() && *code == Some(0) {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                let error = (status == JobStatus::Failed).then(|| {
                    if *outcome == Some("timeout") {
                        format!("Command timed out after {} seconds", timeout.unwrap_or(0.0))
                    } else if *outcome == Some("aborted") {
                        "[Command aborted]".to_string()
                    } else {
                        format!(
                            "Command exited with code {}",
                            code.map_or("?".to_string(), |c| c.to_string())
                        )
                    }
                });
                (status, Some(text), error)
            }
            Err(e) => (JobStatus::Failed, None, Some(e.to_string())),
        };
        registry.update(&task_job_id, |job| {
            job.status = status;
            job.result = result;
            job.error = error;
            job.ended = Some(job.started.elapsed());
        });
        if let Some(notify) = &task_notify {
            notify.settled(status);
        }
    });
    let mut lines: Vec<String> = Vec::new();
    lines.extend(notices);
    lines.push(jobs::background_notice(&job_id, timeout));
    if let Some(notify) = notify {
        lines.push(format!(
            "Output lines matching /{}/ are reported with the job's result; read proc://{job_id}.",
            notify.source
        ));
    }
    let mut result = AgentToolResult::text(lines.join("\n"));
    result.details = serde_json::json!({"bash_id": job_id, "status": "running"});
    result
}

/// Run `command` as a background job with no deadline of its own, its matching lines streaming
/// into `notify` (a `monitor` command watch). Returns the job's `bg_N` id.
pub(super) fn start_watch(env: &super::Env, command: String, notify: &Arc<Notify>) -> String {
    let started = start_background(
        &env.jobs,
        env.cwd.clone(),
        command,
        None,
        Vec::new(),
        SpillConfig::load(&env.cwd),
        env.query(),
        Some(notify),
        env.dyn_tools(),
        &AbortSignal::never(),
    );
    started.details["bash_id"]
        .as_str()
        .expect("start_background reports details.bash_id")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> AbortSignal {
        pa_agent::abort::AbortController::new().signal()
    }

    fn text_of(r: &AgentToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_env() -> std::sync::Arc<super::super::Env> {
        std::sync::Arc::new(super::super::Env::new(&std::env::temp_dir(), false))
    }

    /// Wait (bounded) until job `id` leaves the running state.
    async fn settled_job(env: &super::super::Env, id: &str) -> jobs::Job {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let job = env.jobs.get(id).expect("job registered");
            if job.status != JobStatus::Running {
                return job;
            }
            assert!(Instant::now() < deadline, "job {id} never settled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Regression (hung TUI menus): neither the in-process shell nor any
    /// external command it spawns may reach the TUI's controlling terminal —
    /// `CliPathPolicy` blocks `/dev/tty` redirections in-process, and external
    /// commands run in their own `setsid()` session (`external_cmd_leads_session`).
    #[tokio::test]
    async fn commands_have_no_controlling_terminal() {
        let in_proc = bash(
            test_env(),
            serde_json::json!({"command": "if (: </dev/tty) 2>/dev/null; then echo HAS_TTY; else echo NO_TTY; fi"}),
            sig(),
        )
        .await
        .unwrap();
        assert!(
            text_of(&in_proc).contains("NO_TTY"),
            "{}",
            text_of(&in_proc)
        );

        let r = bash(
            test_env(),
            serde_json::json!({"command": "sh -c 'if (: </dev/tty) 2>/dev/null; then echo HAS_TTY; else echo NO_TTY; fi; ps -o sid= -p $$ | tr -d \" \"; echo $$'"}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.contains("NO_TTY"), "{t}");
        let mut lines = t.lines().skip(1);
        let (sid, pid) = (
            lines.next().unwrap_or_default(),
            lines.next().unwrap_or_default(),
        );
        assert_eq!(sid, pid, "spawned child is its own session leader: {t}");
    }

    #[tokio::test]
    async fn merges_streams_and_appends_wall_time() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({"command": "echo out; echo err 1>&2"}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.starts_with("out\nerr\n\nWall time: "), "{t}");
        assert!(t.ends_with(" seconds"), "{t}");
    }

    #[tokio::test]
    async fn success_carries_wall_and_timeout_details_for_the_card() {
        let env = test_env();
        let r = bash(env.clone(), serde_json::json!({"command": "true"}), sig())
            .await
            .unwrap();
        assert!(r.details["wallTimeMs"].is_u64(), "{:?}", r.details);
        assert_eq!(r.details["timeoutSeconds"], 300);
        let r = bash(
            env,
            serde_json::json!({"command": "true", "timeout": 0}),
            sig(),
        )
        .await
        .unwrap();
        assert_eq!(r.details["timeoutDisabled"], true);
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_output_and_code() {
        let env = test_env();
        let e = bash(
            env.clone(),
            serde_json::json!({"command": "echo boom; exit 3"}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("boom\n\nWall time: "), "{e}");
        assert!(e.ends_with("\n\nCommand exited with code 3"), "{e}");
    }

    #[tokio::test]
    async fn empty_output_and_timeout() {
        let env = test_env();
        let t = text_of(
            &bash(env.clone(), serde_json::json!({"command": "true"}), sig())
                .await
                .unwrap(),
        );
        assert!(t.starts_with("(no output)\n\nWall time: "), "{t}");
        let e = bash(
            env.clone(),
            serde_json::json!({"command": "sleep 5", "timeout": 1}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.ends_with("[Command timed out after 1 seconds]"), "{e}");
    }

    #[tokio::test]
    async fn async_mode_backgrounds_job_with_proc_retrieval() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({"command": "echo first; sleep 0.2; echo done-marker", "async": true}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(
            t.starts_with("Backgrounded as job bg_1 (deadline 300s)"),
            "{t}"
        );

        // proc:// lists the running job.
        let listing = super::super::jobs::read_proc(&env, "").unwrap();
        let lt = text_of(&listing);
        assert!(lt.contains("bg_1 [bash] running up "), "{lt}");
        assert!(
            lt.contains("echo first; sleep 0.2; echo done-marker"),
            "{lt}"
        );

        // Wait for completion, then proc://<id> carries the formatted result.
        for _ in 0..50 {
            let j = env.jobs.get("bg_1").unwrap();
            if j.status != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let detail = super::super::jobs::read_proc(&env, "bg_1").unwrap();
        let dt = text_of(&detail);
        assert!(
            dt.starts_with("bg_1 [bash] — completed — echo first"),
            "{dt}"
        );
        assert!(dt.contains("first\ndone-marker\n\nWall time: "), "{dt}");
        assert!(dt.ends_with(" seconds"), "{dt}");

        let after = super::super::jobs::read_proc(&env, "").unwrap();
        assert!(
            text_of(&after).contains("bg_1 [bash] completed in "),
            "{}",
            text_of(&after)
        );
    }

    /// Regression: a background job outlives the tool list that started it,
    /// and the `dyn` bridge holds its tools weakly, so without a job-scoped
    /// strong hold a `dyn` call inside a still-running job would find no
    /// device once the caller rebuilt or dropped its list.
    #[tokio::test]
    async fn background_job_keeps_dyn_tools_alive_after_the_list_drops() {
        /// A `dyn` device; the gated one parks until the test releases it, so the
        /// caller can drop its tool list at a known point in the command.
        struct TestTool {
            name: &'static str,
            gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        }
        impl pa_agent::types::AgentTool for TestTool {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &'static str {
                "test device"
            }
            fn parameters(&self) -> &Value {
                static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                SCHEMA.get_or_init(|| {
                    serde_json::json!(
                        {"type":"object","properties":{"msg":{"type":"string"}}}
                    )
                })
            }
            fn execute(
                self: Arc<Self>,
                _id: String,
                params: Value,
                _signal: AbortSignal,
                _on_update: pa_agent::types::AgentToolUpdateCallback,
            ) -> pa_agent::BoxFut<'static, Result<AgentToolResult>> {
                Box::pin(async move {
                    if let Some((entered, release)) = &self.gate {
                        let release = release.notified();
                        entered.notify_one();
                        release.await;
                    }
                    let msg = params["msg"].as_str().unwrap_or_default();
                    Ok(AgentToolResult::text(format!("ran {}:{msg}", self.name)))
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let env = std::sync::Arc::new(super::super::Env::new(dir.path(), false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let gated: Arc<dyn pa_agent::types::AgentTool> = Arc::new(TestTool {
            name: "gate_device",
            gate: Some((entered.clone(), release.clone())),
        });
        let target: Arc<dyn pa_agent::types::AgentTool> = Arc::new(TestTool {
            name: "target_device",
            gate: None,
        });
        let weak_target = Arc::downgrade(&target);
        let tools = vec![gated, target];
        env.set_dyn_tools(&tools);

        // The gated device runs first; the target device runs only after the
        // caller has dropped the list, so its lookup cannot ride on the caller.
        let notice = start_background(
            &env.jobs,
            dir.path().to_path_buf(),
            "dyn gate_device --msg go; dyn target_device --msg hi".to_string(),
            Some(60.0),
            Vec::new(),
            SpillConfig::load(dir.path()),
            String::new(),
            None,
            env.dyn_tools(),
            &sig(),
        );
        let text = text_of(&notice);
        assert!(text.starts_with("Backgrounded as job bg_1"), "{text}");

        tokio::time::timeout(Duration::from_secs(60), entered.notified())
            .await
            .expect("the background job never reached the gated dyn device");

        // The caller drops the list it built. The env's snapshot is weak, so the
        // tools survive only because the running job holds them.
        drop(tools);
        assert!(
            weak_target.upgrade().is_some(),
            "the running job keeps its dyn tools alive after the list drops"
        );
        release.notify_one();
        let job = settled_job(&env, "bg_1").await;
        let result = job.result.unwrap_or_default();
        assert!(
            job.status == JobStatus::Completed,
            "the post-drop dyn call failed: {:?}\n{result}",
            job.error
        );
        assert!(
            result.contains("ran target_device:hi"),
            "the dyn call made after the list dropped ran: {result}"
        );

        // The hold is job-scoped: settling the job drops it, so the tool frees
        // instead of being pinned for the session's lifetime.
        let deadline = Instant::now() + Duration::from_secs(60);
        while weak_target.upgrade().is_some() {
            assert!(
                Instant::now() < deadline,
                "the finished job still pins its dyn tools"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn service_ready_by_log_regex() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({
                "command": "echo boot-banner; echo service-up-now; sleep 30",
                "name": "svc-log",
                "ready": {"log": "service-up-now"}
            }),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.starts_with("svc-log: ready pid="), "{t}");
        assert!(t.contains(" ready\n"), "{t}");
        assert!(t.contains("service-up-now"), "{t}");
        // proc:// lists the service as ready.
        let listing = super::super::jobs::read_proc(&env, "").unwrap();
        let lt = text_of(&listing);
        assert!(lt.contains("svc-log [service] — ready — up "), "{lt}");
        // proc://<name> shows the captured log.
        let detail = super::super::jobs::read_proc(&env, "svc-log").unwrap();
        assert!(
            text_of(&detail).contains("boot-banner"),
            "{}",
            text_of(&detail)
        );
    }

    #[tokio::test]
    async fn service_ready_by_port() {
        let env = test_env();
        // A listening socket makes the TCP probe succeed (omp `#pollPort`).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let r = bash(
            env.clone(),
            serde_json::json!({
                "command": "sleep 30",
                "name": "svc-port",
                "ready": {"port": port}
            }),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.starts_with("svc-port: ready pid="), "{t}");
    }

    #[tokio::test]
    async fn service_readiness_timeout_reports_not_ready() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({
                "command": "sleep 30",
                "name": "svc-slow",
                "ready": {"log": "a-marker-that-never-appears", "timeout": 0.2}
            }),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.starts_with("svc-slow: starting pid="), "{t}");
        assert!(!t.contains(" ready\n"), "{t}");
        assert!(
            t.contains("NOT ready — readiness timed out after 0.2s; process state: starting."),
            "{t}"
        );
        // The service keeps running after the timeout (omp returns readyTimedOut).
        let svc = env.jobs.service("svc-slow").unwrap();
        assert_eq!(
            svc.lock().unwrap().state,
            super::super::jobs::ServiceState::Starting
        );
        assert!(svc.lock().unwrap().pid.is_some());
    }

    #[tokio::test]
    async fn service_validation_matches_omp_errors() {
        let env = test_env();
        let err = bash(
            env.clone(),
            serde_json::json!({"command": "true", "name": "-bad", "ready": {"log": "x"}}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Service name must be 1-48 letters, numbers, dots, underscores, or hyphens"
        );
        let err = bash(
            env.clone(),
            serde_json::json!({"command": "true", "name": "svc", "timeout": 10}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Service mode does not accept async or timeout; use ready.timeout for readiness."
        );
        let err = bash(
            env.clone(),
            serde_json::json!({"command": "true", "name": "svc", "ready": {"log": "x", "port": 70000}}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(err, "ready.port must be an integer from 1 to 65535");
        let err = bash(
            env.clone(),
            serde_json::json!({"command": "true", "name": "svc", "ready": {"host": "127.0.0.1"}}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(err, "ready requires log or port");
        let err = bash(
            env.clone(),
            serde_json::json!({"command": "true", "name": "svc", "ready": {"log": "(unclosed"}}),
            sig(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("Invalid readiness regex: "), "{err}");
    }

    #[tokio::test]
    async fn service_exit_settles_and_captures_state() {
        let env = test_env();
        // Without a ready spec the tool returns immediately in "running" state
        // (omp start does not wait); the supervisor settles the exit afterwards.
        let r = bash(
            env.clone(),
            serde_json::json!({"command": "echo brief; exit 3", "name": "svc-exit"}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        // The supervisor may settle the fast exit before the tool returns
        // (omp's start RPC has the same race); both snapshots are valid.
        let running = t.starts_with("svc-exit: running pid=");
        let failed = t.starts_with("svc-exit: failed");
        assert!(running || failed, "{t}");
        assert!(!t.contains(" ready\n"), "{t}");
        if failed {
            assert!(t.contains("Reason: exited with code 3"), "{t}");
        }
        assert!(t.contains("brief"), "{t}");
        // proc://<name> settles to "failed" once the child exits.
        let mut settled = String::new();
        for _ in 0..50 {
            let detail = super::super::jobs::read_proc(&env, "svc-exit").unwrap();
            settled = text_of(&detail);
            if settled.contains("svc-exit [service] — failed — up ") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            settled.contains("svc-exit [service] — failed — up "),
            "{settled}"
        );
        assert!(settled.contains("brief"), "{settled}");
    }

    #[tokio::test]
    async fn service_ready_wait_observes_pre_ready_exit() {
        // The readiness wait must wake on a terminal state instead of blocking
        // for the full timeout (omp broker wake condition).
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({
                "command": "echo partial; exit 1",
                "name": "svc-die",
                "ready": {"log": "never-appears", "timeout": 30}
            }),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(t.starts_with("svc-die: failed"), "{t}");
        assert!(!t.contains("NOT ready"), "{t}");
        assert!(t.contains("Reason: exited with code 1"), "{t}");
        assert!(t.contains("partial"), "{t}");
    }

    #[tokio::test]
    async fn service_child_is_killed_when_registry_drops() {
        let pid;
        {
            let env = test_env();
            bash(
                env.clone(),
                serde_json::json!({"command": "sleep 60", "name": "svc-drop"}),
                sig(),
            )
            .await
            .unwrap();
            pid = env
                .jobs
                .service("svc-drop")
                .unwrap()
                .lock()
                .unwrap()
                .pid
                .unwrap();
            assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        } // registry dropped: the supervisor must kill the child.
        let mut gone = false;
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "service child {pid} survived registry drop");
    }

    #[tokio::test]
    async fn ready_without_name_is_ignored_with_notice() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({"command": "echo hi", "ready": {"port": 8080}}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(
            t.starts_with(
                "Ignored ready: service-only, and no service name was given.\nhi\n\nWall time: "
            ),
            "{t}"
        );
    }

    /// A line split across two reads still matches whole (omp `SourceLineReader`
    /// keeps the line boundary); non-matching and blank lines are never reported.
    #[test]
    fn notify_matches_whole_lines_across_chunks() {
        let n = Notify::new("^boom$").unwrap();
        n.feed("quiet\nbo");
        n.feed("om\n\n  \nlast-boom\n");
        // The split line "bo"+"om" is matched as one line; the trailing
        // `last-boom` and the blanks do not match `^boom$`.
        assert_eq!(
            n.report().unwrap(),
            "[notify /^boom$/: 1 matching line]\nboom"
        );
        // An unterminated last line is still a line once output ends.
        n.feed("boom");
        n.finish();
        let report = n.report().unwrap();
        assert_eq!(report, "[notify /^boom$/: 2 matching lines]\nboom\nboom");
    }

    /// A multi-byte character split across two reads survives: `0xC3` alone is
    /// not a character, so decoding per read would emit U+FFFD and lose it.
    #[test]
    fn decode_chunk_carries_split_utf8_across_reads() {
        let mut carry = Vec::new();
        assert_eq!(decode_chunk(&[b'a', 0xC3], &mut carry), "a");
        assert_eq!(carry, vec![0xC3]);
        assert_eq!(decode_chunk(&[0xA9, 0x0A], &mut carry), "\u{e9}\n");
        assert!(carry.is_empty());
        // A 3-byte character split three ways, then a 4-byte one.
        assert_eq!(decode_chunk(&[0xE2], &mut carry), "");
        assert_eq!(decode_chunk(&[0x82], &mut carry), "");
        assert_eq!(decode_chunk(&[0xAC, 0xF0], &mut carry), "\u{20ac}");
        assert_eq!(decode_chunk(&[0x9F, 0xA6, 0x80], &mut carry), "\u{1f980}");
        assert!(carry.is_empty());
        // Genuinely invalid bytes still degrade to one U+FFFD per sequence.
        assert_eq!(decode_chunk(&[0xFF, b'x'], &mut carry), "\u{FFFD}x");
        assert!(carry.is_empty());
    }

    /// A `notify` match on a character split across the read boundary: the
    /// line is matched on its decoded text, not on a replacement char.
    #[test]
    fn notify_matches_a_line_split_mid_character() {
        let n = Notify::new("caf\u{e9}").unwrap();
        let mut carry = Vec::new();
        n.feed(&decode_chunk(b"hit: caf\xC3", &mut carry));
        n.feed(&decode_chunk(b"\xA9 done\n", &mut carry));
        n.finish();
        assert_eq!(
            n.report().unwrap(),
            "[notify /caf\u{e9}/: 1 matching line]\nhit: caf\u{e9} done"
        );
    }

    /// A huge line without newlines stays bounded: the buffer never exceeds
    /// the partial cap, the line is judged once from its prefix, and the rest
    /// is skipped rather than rescanned on every read.
    #[test]
    fn notify_bounds_partial_line_without_newlines() {
        let n = Notify::new("needle").unwrap();
        n.feed("needle");
        for _ in 0..1000 {
            n.feed(&"x".repeat(4096));
            let len = n.state.lock().unwrap().partial.len();
            assert!(len <= NOTIFY_MAX_PARTIAL_BYTES, "partial grew to {len}");
        }
        // The newline ends the skipped remainder; a following line still counts.
        n.feed("\nneedle again\n");
        n.finish();
        let report = n.report().unwrap();
        let mut lines = report.lines();
        assert_eq!(lines.next().unwrap(), "[notify /needle/: 2 matching lines]");
        // The over-long line was judged once from its bounded prefix and is
        // reported clamped, not at its original (megabyte) length.
        let first = lines.next().unwrap();
        assert!(first.starts_with("needle"), "{first}");
        assert_eq!(first.chars().count(), NOTIFY_MAX_LINE_CHARS);
        assert_eq!(lines.next().unwrap(), "needle again");
        assert!(n.state.lock().unwrap().partial.is_empty());
    }

    /// A huge non-matching line is dropped without reaching the report.
    #[test]
    fn notify_drops_an_unmatched_unbounded_line() {
        let n = Notify::new("hit").unwrap();
        n.feed(&"y".repeat(NOTIFY_MAX_PARTIAL_BYTES * 4));
        n.finish();
        assert!(n.report().is_none());
        assert!(n.state.lock().unwrap().partial.is_empty());
    }

    /// The report is appended after the body was capped, so the combined job
    /// result must be capped too before it becomes `proc://` text.
    #[test]
    fn notify_report_append_respects_the_inline_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "tools:\n  artifactSpillThreshold: 8\n  artifactHeadBytes: 20\n  artifactTailBytes: 20\n  artifactTailLines: 500\n",
        )
        .unwrap();
        let spill = SpillConfig::load(dir.path());
        let n = Notify::new("hit").unwrap();
        for i in 0..NOTIFY_MAX_LINES {
            n.feed(&format!("hit {i} {}\n", "q".repeat(NOTIFY_MAX_LINE_CHARS)));
        }
        n.finish();
        let report = n.report().unwrap();
        assert!(
            report.len() > spill.inline_cap(),
            "report {} bytes",
            report.len()
        );
        let body = "line\n".repeat(4000);
        let text = append_notify_report(&body, &report, &spill);
        assert!(
            text.len() <= spill.inline_cap(),
            "result {} bytes",
            text.len()
        );
        assert!(text.contains("B elided…]"), "elision marker kept: {text}");
        // A small report passes through unchanged.
        let small = append_notify_report("out", "[notify /x/: 1 matching line]\nx", &spill);
        assert_eq!(small, "out\n\n[notify /x/: 1 matching line]\nx");
    }

    /// Regression: the background job's output tap keeps only a recent window,
    /// and the byte window (`RETAIN_BYTES / 2`) is not char-aligned. Draining it
    /// at the raw byte index split a 3-byte character, `String::drain` panicked,
    /// and the detached job never settled. Output here is well past
    /// `RETAIN_BYTES`, so several drains run mid-character.
    #[test]
    fn background_tap_drains_multibyte_output_on_char_boundaries() {
        let chunk = "\u{d55c}".repeat(1024); // U+D55C, 3 bytes on the wire
        let cmd = format!("for i in $(seq 1 140); do printf '%s' '{chunk}'; done");
        let tap: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (output, code, _, outcome) = rt
            .block_on(run(
                &std::env::temp_dir(),
                &cmd,
                Some(60.0),
                &sig(),
                None,
                Some(tap.clone()),
            ))
            .unwrap();
        assert_eq!((code, outcome), (Some(0), None));
        let text = tap.lock().unwrap().clone();
        assert!(text.len() <= RETAIN_BYTES, "tap {} bytes", text.len());
        assert!(!text.is_empty(), "tap drained away all output");
        assert_eq!(
            text.len() % 3,
            0,
            "tap starts mid-character: {} bytes",
            text.len()
        );
        assert!(
            text.chars().all(|c| c == '\u{d55c}'),
            "tap kept a broken char"
        );
        // The foreground buffer keeps raw bytes for the head+tail window; only
        // the tap must stay decodable here.
        assert!(!output.is_empty());
    }

    #[test]
    fn notify_caps_reported_lines_and_counts_the_rest() {
        let n = Notify::new("hit").unwrap();
        let mut text = String::new();
        for i in 0..(NOTIFY_MAX_LINES + 5) {
            use std::fmt::Write as _;
            let _ = writeln!(text, "hit {i} {}", "x".repeat(NOTIFY_MAX_LINE_CHARS + 10));
        }
        n.feed(&text);
        n.finish();
        let report = n.report().unwrap();
        assert!(
            report.starts_with(&format!(
                "[notify /hit/: {NOTIFY_MAX_LINES} matching lines]"
            )),
            "{}",
            report.lines().next().unwrap_or_default()
        );
        assert!(report.ends_with("\n… 5 more matching lines"), "{report}");
        let first = report.lines().nth(1).unwrap();
        assert_eq!(first.chars().count(), NOTIFY_MAX_LINE_CHARS);
    }

    /// `notify` matches from the run loop (deterministic: no job polling).
    #[test]
    fn notify_over_a_command_reports_only_matching_lines() {
        let n = Notify::new("ERROR|FATAL").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run(
            &std::env::temp_dir(),
            "echo 'ERROR one'; echo fine; echo 'FATAL two'",
            Some(60.0),
            &sig(),
            Some(&n),
            None,
        ))
        .unwrap();
        n.finish();
        let report = n.report().unwrap();
        assert!(
            report.starts_with("[notify /ERROR|FATAL/: 2 matching lines]\nERROR one\nFATAL two"),
            "{report}"
        );
        assert!(!report.contains("fine"), "{report}");
    }

    #[test]
    fn invalid_notify_regex_fails_the_call_before_running() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(bash(
                test_env(),
                serde_json::json!({"command": "touch never-runs", "notify": "(unclosed"}),
                sig(),
            ))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Invalid notify regex: "), "{err}");
    }

    /// omp: a plain command with `notify` starts in the background; the notice
    /// names the regex and where the matches are reported.
    #[tokio::test]
    async fn notify_without_name_backgrounds_the_command() {
        let env = test_env();
        let r = bash(
            env.clone(),
            serde_json::json!({"command": "echo ERROR boom; echo ok", "notify": "ERROR"}),
            sig(),
        )
        .await
        .unwrap();
        let t = text_of(&r);
        assert!(
            t.starts_with("Backgrounded as job bg_1 (deadline 300s)"),
            "{t}"
        );
        assert!(
            t.contains(
                "Output lines matching /ERROR/ are reported with the job's result; read proc://bg_1."
            ),
            "{t}"
        );
        assert!(env.jobs.get("bg_1").is_some(), "job registered");
    }

    #[test]
    fn long_output_is_elided_in_the_middle() {
        let mut long = String::new();
        for i in 1..=5000 {
            use std::fmt::Write as _;
            let _ = writeln!(long, "line number {i}");
        }
        let (shown, notice) = bound_output(long.trim_end(), &SpillConfig::default(), "", |_| None);
        assert!(shown.len() < long.len());
        assert!(shown.starts_with("line number 1\n"));
        assert!(shown.ends_with("line number 5000"));
        assert!(shown.contains("ln elided…]"));
        assert!(notice.unwrap().contains("middle lines"));
    }

    /// `tools.artifactSpillThreshold: 8` (project settings layer) caps a
    /// 100 KB command output near 8 KB (head + tail around a marker) and
    /// points at the artifact holding the full output. The artifact sink is
    /// injected, so the test never touches the process-global session file.
    #[test]
    fn lowered_spill_threshold_caps_bash_output_with_artifact_pointer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        // Project layer pins every key so the user's global config cannot leak in.
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "tools:\n  artifactSpillThreshold: 8\n  artifactHeadBytes: 20\n  artifactTailBytes: 20\n  artifactTailLines: 500\n",
        )
        .unwrap();
        let cfg = SpillConfig::load(dir.path());
        assert_eq!(cfg.threshold, 8 * 1024);
        // 1000 lines of 99 digits + newline = 100 000 bytes.
        let cmd = "for i in $(seq 1 1000); do printf '%099d\\n' \"$i\"; done";
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (output, code, _, outcome) = rt
            .block_on(run(dir.path(), cmd, Some(60.0), &sig(), None, None))
            .unwrap();
        assert_eq!((code, outcome), (Some(0), None));
        let saved = std::cell::RefCell::new(Vec::<String>::new());
        let save = |text: &str| {
            let mut saved = saved.borrow_mut();
            saved.push(text.to_string());
            Some(format!("artifact://{}", saved.len() - 1))
        };
        let (body, notice) = bound_output(output.trim_end_matches('\n'), &cfg, "", save);
        assert!(notice.is_none(), "{notice:?}");
        assert!(body.len() < 9 * 1024, "inline body is {} bytes", body.len());
        assert!(body.starts_with(&format!("{:099}\n", 1)), "{body}");
        assert!(body.contains("B elided…]\n"), "{body}");
        let shown = body
            .strip_suffix("[raw output: artifact://0]")
            .expect("artifact pointer footer");
        assert!(
            shown.trim_end().ends_with(&format!("{:099}", 1000)),
            "{body}"
        );
        let saved = saved.into_inner();
        assert_eq!(saved.len(), 1, "one artifact for the whole output");
        assert_eq!(
            saved[0].len(),
            99_999,
            "artifact holds the full (trimmed) output"
        );
    }

    /// A 100 KB output whose only relevant line sits in the middle: the
    /// relevance view keeps it when the request names it (and points at the
    /// artifact); plain head/tail (no request) drops it.
    #[test]
    fn relevance_view_keeps_mid_output_line_named_by_the_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omp")).unwrap();
        std::fs::write(
            dir.path().join(".omp/config.yml"),
            "tools:\n  artifactSpillThreshold: 12\n  artifactHeadBytes: 3\n  artifactTailBytes: 6\n  artifactTailLines: 150\n",
        )
        .unwrap();
        let marker = "deploy failed: QuorumLostError on shard_alpha7";
        // 1000 lines of 99 digits + newline = 100 000 bytes, marker at line 500.
        let cmd = format!(
            "for i in $(seq 1 1000); do if [ $i -eq 500 ]; then echo '{marker}'; else printf '%099d\\n' \"$i\"; fi; done"
        );
        let env = std::sync::Arc::new(super::super::Env::new(dir.path(), false));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let run_with = |query: &str| {
            *env.query.lock() = query.to_string();
            text_of(
                &rt.block_on(bash(
                    env.clone(),
                    serde_json::json!({"command": cmd}),
                    sig(),
                ))
                .unwrap(),
            )
        };
        let ranked =
            run_with("why did the deploy hit QuorumLostError?\nChecking the shard_alpha7 log.");
        assert!(ranked.contains(marker), "{ranked}");
        assert!(ranked.contains(" lines omitted] …"), "{ranked}");
        assert!(ranked.len() < 12 * 1024, "inline {} bytes", ranked.len());
        let plain = run_with("");
        assert!(!plain.contains(marker), "head/tail keeps only the edges");
        assert!(plain.contains("B elided…]"), "{plain}");
        // The relevance view always carries the artifact pointer.
        let output = rt
            .block_on(run(dir.path(), &cmd, Some(60.0), &sig(), None, None))
            .unwrap()
            .0;
        let cfg = SpillConfig::load(dir.path());
        let (body, notice) = bound_output(
            output.trim_end_matches('\n'),
            &cfg,
            "QuorumLostError",
            |_| Some("artifact://4".to_string()),
        );
        assert!(body.contains(marker));
        let notice = notice.unwrap();
        assert!(
            notice.ends_with("Read artifact://4 for full output"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn in_process_pipeline_heredoc_redirect_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let sig = AbortSignal::never();
        let (out, code, _, outcome) = run(
            dir.path(),
            "cat <<'EOF' | grep -n two > hit.txt\none\ntwo\nthree\nEOF\ncat hit.txt\nexit 7",
            Some(10.0),
            &sig,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, None);
        assert_eq!(code, Some(7));
        assert_eq!(out.trim(), "2:two");
    }

    #[tokio::test]
    async fn in_process_timeout_cancels_sleeping_command() {
        let dir = tempfile::tempdir().unwrap();
        let sig = AbortSignal::never();
        let (_, code, elapsed, outcome) = run(dir.path(), "sleep 30", Some(0.15), &sig, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, Some("timeout"));
        assert_eq!(code, None);
        assert!(elapsed < Duration::from_secs(5), "elapsed: {elapsed:?}");
    }

    /// QA: the in-process `omp-shell-builtins` `jq`, `grep` and `wc` really run
    /// end-to-end inside the `bash` tool (no external binaries involved).
    #[tokio::test]
    async fn in_process_builtins_grep_wc_and_jq_work_end_to_end() {
        let r = bash(
            test_env(),
            serde_json::json!({"command": "printf '{\"k\":1}\\n{\"k\":2}\\n' | jq -c '.k' | grep -c '[12]'"}),
            sig(),
        )
        .await
        .unwrap();
        let out = text_of(&r);
        assert_eq!(
            out.lines().next().unwrap_or_default().trim(),
            "2",
            "jq|grep -c pipeline: {out}"
        );

        let r = bash(
            test_env(),
            serde_json::json!({"command": "printf 'a\\nb\\nc\\n' | wc -l"}),
            sig(),
        )
        .await
        .unwrap();
        let out = text_of(&r);
        assert_eq!(
            out.lines().next().unwrap_or_default().trim(),
            "3",
            "wc -l: {out}"
        );
    }

    /// QA: the registered host tools reach the in-process `dyn` builtin through
    /// the live `Env::dyn_tools` bridge and the real `AgentTool` interface, and
    /// `bash` itself is not exposed (no unbounded shell recursion).
    #[tokio::test]
    async fn bash_tool_exposes_registered_tools_via_dyn_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let tools = super::super::builtin_tools(dir.path(), pi_edit::EditMode::Replace);
        assert!(tools.iter().any(|t| t.name() == "read"), "read registered");
        let bash_tool = tools
            .iter()
            .find(|t| t.name() == "bash")
            .cloned()
            .expect("bash tool is registered");
        let call = |command: &str| {
            bash_tool.clone().execute(
                String::new(),
                serde_json::json!({"command": command}),
                sig(),
                Arc::new(|_r: AgentToolResult| {}),
            )
        };

        let catalog = text_of(&call("dyn").await.unwrap());
        assert!(catalog.contains("  read"), "catalog lists read: {catalog}");
        assert!(
            !catalog
                .lines()
                .any(|l| l.trim_start().starts_with("bash —")),
            "bash must not be a dyn device (recursion): {catalog}"
        );

        let top = text_of(&call("dyn --help").await.unwrap());
        assert!(
            top.contains("dyn — discover and invoke live dynamic devices"),
            "{top}"
        );

        let help = text_of(&call("dyn read --help").await.unwrap());
        assert!(help.contains("dyn read"), "dyn read --help: {help}");
        assert!(help.contains("<path>"), "read path is a positional: {help}");
    }

    /// QA: the `cwd` parameter scopes the in-process shell without touching the
    /// host process's own working directory.
    #[tokio::test]
    async fn in_process_cwd_parameter_isolates_shell_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let before = std::env::current_dir().unwrap();
        let env = std::sync::Arc::new(super::super::Env::new(dir.path(), false));
        let r = bash(
            env,
            serde_json::json!({"cwd": "sub", "command": "pwd"}),
            sig(),
        )
        .await
        .unwrap();
        let out = text_of(&r);
        let want = std::fs::canonicalize(&sub).unwrap();
        let printed = out.lines().next().unwrap_or_default().trim();
        assert_eq!(
            std::fs::canonicalize(printed).unwrap(),
            want,
            "pwd is the cwd parameter: {out}"
        );
        assert_eq!(
            std::env::current_dir().unwrap(),
            before,
            "host cwd untouched"
        );
    }
    /// Whether `pid` no longer runs: already reaped, or a zombie whose parent
    /// has not collected it yet. A killed grandchild is briefly either, so both
    /// count as dead. Only the pgid teardown test probes pids, and that test is
    /// unix-only.
    #[cfg(unix)]
    fn pid_is_dead(pid: i32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        let alive = kill(Pid::from_raw(pid), None).is_ok();
        // A zombie still answers `kill(pid, 0)`; Linux exposes its state.
        #[cfg(target_os = "linux")]
        let alive = alive
            && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            });
        !alive
    }

    /// Regression: the `cwd` parameter defines `$PWD` as well as the shell's
    /// working directory. A `$PWD` inherited from the host process would make
    /// `pwd` and `$PWD` disagree, so every relative path built from `$PWD`
    /// would resolve against the host's directory instead of the workspace.
    #[tokio::test]
    async fn in_process_pwd_env_matches_cwd_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let env = std::sync::Arc::new(super::super::Env::new(dir.path(), false));
        let r = bash(
            env,
            serde_json::json!({
                "cwd": "sub",
                "command": r#"printf '%s|%s\n' "$(pwd)" "$PWD""#,
            }),
            sig(),
        )
        .await
        .unwrap();
        let out = text_of(&r);
        let first = out.lines().next().unwrap_or_default();
        let (builtin, var) = first.split_once('|').unwrap_or((first, ""));
        assert_eq!(builtin, var, "`pwd` and `$PWD` disagree: {out}");
        let want = std::fs::canonicalize(&sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(builtin).unwrap(),
            want,
            "`$PWD` follows the cwd parameter: {out}"
        );
        let host_pwd = std::env::var("PWD").unwrap_or_default();
        assert_ne!(
            var, host_pwd,
            "the shell must not inherit the host process's PWD: {out}"
        );
    }

    /// Regression (hung TUI menus): the `/dev/tty` denial holds for alias
    /// spellings too. `/dev/../dev/tty` is the same device node under a
    /// different `Path`, so a literal `path == "/dev/tty"` test admits it and
    /// lets the command take over the host terminal.
    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_tty_alias_is_denied() {
        let policy =
            super::super::shell_host::CliPathPolicy::new(std::path::Path::new("/tmp"), false);
        for alias in ["/dev/../dev/tty", "/dev/tty"] {
            assert!(
                omp_shell::PathPolicy::check_read(&policy, std::path::Path::new(alias)).is_err(),
                "the policy admits the controlling-tty path {alias}"
            );
        }

        let r = bash(
            test_env(),
            serde_json::json!({
                "command": ": </dev/../dev/tty; echo alias_rc=$?; : </dev/tty; echo plain_rc=$?",
            }),
            sig(),
        )
        .await
        .unwrap();
        let out = text_of(&r);
        assert!(
            out.contains("sandbox denied read to /dev/../dev/tty"),
            "the alias is refused by the sandbox, not opened: {out}"
        );
        assert!(out.contains("alias_rc=1"), "the alias fails: {out}");
        assert!(out.contains("plain_rc=1"), "the plain path fails: {out}");
    }

    /// Regression: an abort tears down the whole process group, not just the
    /// direct child. A grandchild forked into the background outlives the shell
    /// and keeps running, so the teardown is asserted on the grandchild's pid.
    ///
    /// Synchronization is the `child.pid` file the command writes once the
    /// grandchild exists: no fixed delay, and the abort is issued only after
    /// that pid is readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_timeout_kills_backgrounded_grandchild_via_pgid() {
        let dir = tempfile::tempdir().unwrap();
        let controller = pa_agent::abort::AbortController::new();
        let env = std::sync::Arc::new(super::super::Env::new(dir.path(), false));
        let running = tokio::spawn(bash(
            env,
            serde_json::json!({"command": "sh -c 'sleep 30 & echo $! > child.pid; wait'"}),
            controller.signal(),
        ));

        let pid_file = dir.path().join("child.pid");
        let deadline = Instant::now() + Duration::from_secs(30);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the grandchild pid never reached {}",
                pid_file.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        controller.abort();
        let err = running.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("[Command aborted]"), "{err}");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !pid_is_dead(pid) {
            assert!(
                Instant::now() < deadline,
                "grandchild {pid} survived the pgid teardown"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Regression: the `dyn` catalog is the settings-gated session list. A tool
    /// switched off by settings is neither advertised nor dispatchable through
    /// the in-process `dyn` builtin.
    #[tokio::test]
    async fn session_dyn_bridge_respects_settings_gate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yml");
        std::fs::write(
            &cfg,
            "find:\n  enabled: \"off\"\ncheckpoint:\n  enabled: false\n",
        )
        .unwrap();
        let settings = omp_settings::Settings::load_from(Some(&cfg), dir.path());
        let tools =
            super::super::session_builtin_tools(dir.path(), pi_edit::EditMode::Replace, &settings);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"), "read stays advertised: {names:?}");
        assert!(
            !names.contains(&"find"),
            "find.enabled off hides find: {names:?}"
        );
        assert!(
            !names.contains(&"checkpoint"),
            "checkpoint.enabled false hides checkpoint: {names:?}"
        );
        // Both are built-ins: the omissions are the settings gate, not a
        // missing tool.
        let ungated = super::super::builtin_tools(dir.path(), pi_edit::EditMode::Replace);
        for gated in ["find", "checkpoint"] {
            assert!(
                ungated.iter().any(|t| t.name() == gated),
                "{gated} is a built-in, only its setting hides it"
            );
        }

        let bash_tool = tools
            .iter()
            .find(|t| t.name() == "bash")
            .cloned()
            .expect("bash is advertised");
        let catalog = text_of(
            &bash_tool
                .execute(
                    String::new(),
                    serde_json::json!({"command": "dyn"}),
                    sig(),
                    std::sync::Arc::new(|_r: AgentToolResult| {}),
                )
                .await
                .unwrap(),
        );
        let listed = |name: &str| {
            catalog
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{name} —")))
        };
        assert!(listed("read"), "dyn lists read: {catalog}");
        assert!(!listed("find"), "dyn must omit find: {catalog}");
        assert!(!listed("checkpoint"), "dyn must omit checkpoint: {catalog}");
    }
}
