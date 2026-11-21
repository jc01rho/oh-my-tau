//! In-process `omp-shell` and `omp-shell-builtins` host integration for `omp-cli`.
//!
//! Replaces external `setsid bash -c` spawning for local commands with
//! `omp_shell::Shell`, enforcing workspace write boundaries via [`CliPathPolicy`],
//! optional Landlock/seccomp external process confinement via [`CliSpawnWrapper`],
//! `-pgid` process-group teardown via [`SpawnBook`], static AST inspection via
//! [`analyze_script`], and dynamic host tool invocation via [`CliDynHost`].

use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    future::Future,
    io::{self, Read},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use omp_core::Str;
use omp_proto::omp::policy::v1::BashIr;
use omp_shell::{
    analysis::{self, ScriptIr},
    builtins::{self, Registration},
    openfiles::{OpenFile, OpenFiles},
    parser::{Parser, ParserOptions},
    ExecutionParameters, PathAccess, PathDenied, PathPolicy, ProcessScope, ProfileLoadBehavior,
    RcLoadBehavior, Shell, ShellVariable, SourceInfo, SpawnObserver, SpawnWrapper,
};
use omp_shell_builtins::{
    dyn_builtin, process_builtins, utility_builtins, DynCallOutput, DynDevice, DynFault, DynFuture,
    DynHost, DynOutput, DynSchema,
};
use pa_agent::{
    abort::{AbortController, AbortSignal},
    types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolResultContent},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Default shell extensions type used by `omp-cli`.
pub type OmpExt = omp_shell::extensions::DefaultShellExtensions;

/// Named builtin registration pairs.
pub type BuiltinPairs = Vec<(&'static str, Registration<OmpExt>)>;

/// Non-destructive and filesystem-mutation builtin groups returned by [`split_utility_builtins`].
pub type SplitUtilityBuiltins = (BuiltinPairs, BuiltinPairs);

/// Streaming UTF-8 chunk callback passed to [`ShellExecRequest`].
pub type ChunkCallback<'a> = Box<dyn FnMut(&str) + Send + 'a>;

/// Split utility builtins into non-destructive utilities and filesystem-mutation
/// utilities (`rm`, `mv`, `ln`) so mutation builtins can be audited, gated, or
/// registered as an independent registry tier.
#[must_use]
pub fn split_utility_builtins() -> SplitUtilityBuiltins {
    let mut safe = Vec::new();
    let mut fs_mutation = Vec::new();
    for (name, reg) in utility_builtins::<OmpExt>() {
        if matches!(name, "rm" | "mv" | "ln") {
            fs_mutation.push((name, reg));
        } else {
            safe.push((name, reg));
        }
    }
    (safe, fs_mutation)
}

/// Canonicalize `path` or, if it does not exist yet, its deepest existing ancestor
/// with the remaining relative tail re-attached. Prevents `..` / symlink escape.
fn normalize_for_policy(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(canon) = fs::canonicalize(parent) {
            let mut out = canon;
            for seg in tail.into_iter().rev() {
                out.push(seg);
            }
            return out;
        }
        cur = parent;
    }
    path.to_path_buf()
}

fn is_null_device(path: &Path) -> bool {
    path == Path::new("/dev/null")
}

fn is_controlling_tty(path: &Path) -> bool {
    path == Path::new("/dev/tty")
}

/// File access policy enforcing workspace write boundaries when sandboxing is
/// active (`sandbox_enabled = true`).
#[derive(Debug, Clone)]
pub struct CliPathPolicy {
    write_root: PathBuf,
    sandbox_enabled: bool,
}

impl CliPathPolicy {
    /// Creates a policy permitting writes under `write_root` when
    /// `sandbox_enabled` is true (or unrestricted when false).
    #[must_use]
    pub fn new(write_root: &Path, sandbox_enabled: bool) -> Self {
        Self {
            write_root: normalize_for_policy(write_root),
            sandbox_enabled,
        }
    }

    fn resolve_under_root(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            normalize_for_policy(path)
        } else {
            normalize_for_policy(&self.write_root.join(path))
        }
    }

    fn write_allowed(&self, path: &Path) -> bool {
        if !self.sandbox_enabled || is_null_device(path) {
            return true;
        }
        let target = self.resolve_under_root(path);
        target.starts_with(&self.write_root)
    }
}

impl PathPolicy for CliPathPolicy {
    fn check_read(&self, path: &Path) -> std::result::Result<(), PathDenied> {
        if is_controlling_tty(path) {
            return Err(PathDenied {
                path: path.to_path_buf(),
                access: PathAccess::Read,
            });
        }
        Ok(())
    }

    fn check_write(&self, path: &Path) -> std::result::Result<(), PathDenied> {
        if is_controlling_tty(path) || !self.write_allowed(path) {
            Err(PathDenied {
                path: path.to_path_buf(),
                access: PathAccess::Write,
            })
        } else {
            Ok(())
        }
    }

    fn open(
        &self,
        path: &Path,
        request: omp_shell::OpenRequest,
    ) -> std::result::Result<fs::File, PathDenied> {
        if is_controlling_tty(path) {
            return Err(PathDenied {
                path: path.to_path_buf(),
                access: request.access,
            });
        }
        let is_write = !matches!(request.access, PathAccess::Read);
        if is_write && !self.write_allowed(path) {
            return Err(PathDenied {
                path: path.to_path_buf(),
                access: request.access,
            });
        }
        let mut opts = OpenOptions::new();
        match request.access {
            PathAccess::Read => {
                opts.read(true);
            }
            PathAccess::CreateNew => {
                opts.write(true).create_new(true);
            }
            PathAccess::Truncate => {
                opts.write(true).create(true).truncate(true);
            }
            PathAccess::Append => {
                opts.write(true).create(true).append(true);
            }
            PathAccess::ReadWrite => {
                opts.read(true).write(true).create(true);
            }
            PathAccess::Write => {
                opts.write(true).create(true);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if is_write && request.create_mode != 0 {
                opts.mode(request.create_mode);
            }
        }
        opts.open(path).map_err(|_| PathDenied {
            path: path.to_path_buf(),
            access: request.access,
        })
    }
}

/// External command spawn wrapper adapting [`crate::features::sandbox::wrap_inner`]
/// so external binaries invoked from the in-process shell still run under
/// Landlock/seccomp confinement when `/sandbox` is active.
#[derive(Debug, Clone)]
pub struct CliSpawnWrapper {
    launcher_prog: Option<OsString>,
    launcher_args: Vec<OsString>,
}

impl CliSpawnWrapper {
    /// Builds a wrapper from the current sandbox toggle for `write_root`.
    #[must_use]
    pub fn from_sandbox(write_root: &Path) -> Self {
        match crate::features::sandbox::wrap_inner(write_root, &[]) {
            Some((prog, args)) => Self {
                launcher_prog: Some(prog.into_os_string()),
                launcher_args: args,
            },
            None => Self {
                launcher_prog: None,
                launcher_args: Vec::new(),
            },
        }
    }
}

impl SpawnWrapper for CliSpawnWrapper {
    fn launcher(&self) -> Option<(&OsStr, &[OsString])> {
        self.launcher_prog
            .as_deref()
            .map(|prog| (prog, self.launcher_args.as_slice()))
    }

    fn env_allowed(&self, _key: &str) -> bool {
        true
    }
}

/// Tracks spawned external child PIDs and process groups (`-pgid`) for scoped
/// cancellation teardown.
#[derive(Debug, Default)]
pub struct SpawnBook {
    pids: Mutex<Vec<i32>>,
    groups: Mutex<Vec<i32>>,
}

impl SpawnBook {
    /// Returns tracked process group IDs.
    #[must_use]
    pub fn tracked_groups(&self) -> Vec<i32> {
        self.groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sends a POSIX signal to all tracked process groups (`-pgid`) and any
    /// ungrouped tracked PIDs.
    #[cfg(unix)]
    pub fn signal_groups(&self, signal: nix::sys::signal::Signal) {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;
        let groups = self.tracked_groups();
        for pgid in groups {
            if pgid > 0 {
                let _ = killpg(Pid::from_raw(pgid), signal);
            }
        }
        let pids = self
            .pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for pid in pids {
            if pid > 0 {
                let _ = nix::sys::signal::kill(Pid::from_raw(pid), signal);
            }
        }
    }

    #[cfg(not(unix))]
    pub fn signal_groups_best_effort(&self) {}
}

impl SpawnObserver for SpawnBook {
    fn on_spawn(&self, pid: i32, pgid: Option<i32>) {
        if pid > 0 {
            self.pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(pid);
        }
        if let Some(pgid) = pgid.filter(|&g| g > 0) {
            let mut g = self
                .groups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !g.contains(&pgid) {
                g.push(pgid);
            }
        }
    }
}

impl ProcessScope for SpawnBook {
    fn may_signal(&self, pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let pids = self
            .pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pids.contains(&pid) || groups.contains(&pid)
    }

    fn may_observe(&self, pid: i32) -> bool {
        self.may_signal(pid)
    }
}

/// Callback type stored on [`super::Env`] to expose host tools to the `dyn`
/// shell builtin without creating recursive `bash`/`dyn` loops.
pub type DynToolList = Arc<Vec<Arc<dyn AgentTool>>>;

/// Bridges registered host `AgentTool` instances to `omp-shell-builtins` `dyn`.
pub struct CliDynHost {
    tools: DynToolList,
}

impl CliDynHost {
    /// Creates a host adapter from a snapshot of registered `AgentTool`s.
    /// `"bash"` is excluded to prevent unbounded shell recursion.
    #[must_use]
    pub fn new(tools: DynToolList) -> Self {
        Self { tools }
    }

    fn visible_tools(&self) -> impl Iterator<Item = &Arc<dyn AgentTool>> {
        self.tools
            .iter()
            .filter(|t| !matches!(t.name(), "bash" | "dyn"))
    }

    fn find_tool(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.visible_tools().find(|t| t.name() == name).cloned()
    }
}

impl DynHost for CliDynHost {
    fn list(&self) -> DynFuture<'_, Vec<DynDevice>> {
        let devices = self
            .visible_tools()
            .map(|t| DynDevice {
                name: Str::new(t.name()),
                description: Some(Str::new(t.description())),
            })
            .collect();
        Box::pin(async move { Ok(devices) })
    }

    fn schema(&self, name: &str) -> DynFuture<'_, DynSchema> {
        let name_owned = name.to_owned();
        let found = self.find_tool(name);
        Box::pin(async move {
            let tool =
                found.ok_or_else(|| DynFault::new(format!("unknown dyn device: {name_owned}")))?;
            Ok(DynSchema {
                name: Str::new(tool.name()),
                description: Some(Str::new(tool.description())),
                schema: tool.parameters().clone(),
            })
        })
    }

    fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> DynFuture<'_, DynCallOutput> {
        let name_owned = name.to_owned();
        let found = self.find_tool(name);
        Box::pin(async move {
            let tool =
                found.ok_or_else(|| DynFault::new(format!("unknown dyn device: {name_owned}")))?;
            let controller = AbortController::new();
            let signal = controller.signal();
            let watch = tokio::spawn(async move {
                cancel.cancelled().await;
                controller.abort();
            });
            let id = format!("dyn-{name_owned}");
            let noop_cb: AgentToolUpdateCallback = Arc::new(|_| {});
            let res = tool.execute(id, args, signal, noop_cb).await;
            watch.abort();
            let result: AgentToolResult = res.map_err(|e| DynFault::new(e.to_string()))?;
            let output = agent_result_to_dyn_output(&result);
            Ok(DynCallOutput {
                output,
                diags: Vec::new(),
            })
        })
    }
}

fn agent_result_to_dyn_output(res: &AgentToolResult) -> DynOutput {
    if res.content.len() == 1 {
        if let Some(ToolResultContent::Text(t)) = res.content.first() {
            if let Ok(val) = serde_json::from_str::<Value>(&t.text) {
                if val.is_object() || val.is_array() {
                    return DynOutput::Json(val);
                }
            }
            return DynOutput::Text(Str::new(&t.text));
        }
    }
    let parts = res
        .content
        .iter()
        .map(|item| match item {
            ToolResultContent::Text(t) => DynOutput::Text(Str::new(&t.text)),
            ToolResultContent::Image(img) => DynOutput::Text(Str::new(format!(
                "[image {}, {} base64 chars]",
                img.mime_type,
                img.data.len()
            ))),
        })
        .collect();
    DynOutput::Parts(parts)
}

/// Static AST inspection over `script` via `omp-shell::analysis`, returning
/// both the native [`ScriptIr`] and [`BashIr`] policy projection.
///
/// # Errors
/// Returns a parse error string if `script` cannot be parsed as a shell program.
pub fn analyze_script(
    script: &str,
    cwd: &Path,
    root: &Path,
) -> std::result::Result<(ScriptIr, BashIr), String> {
    let reader = io::BufReader::new(script.as_bytes());
    let mut parser = Parser::new(reader, &ParserOptions::default());
    let program = parser.parse_program().map_err(|e| e.to_string())?;
    let cwd_s = cwd.to_string_lossy();
    let root_s = root.to_string_lossy();
    let ir = analysis::analyze(&program, &cwd_s, &root_s);
    let pb = BashIr::from(&ir);
    Ok((ir, pb))
}

/// Builds an embedded `omp-shell` instance configured for tool execution.
///
/// # Errors
/// Returns an error if shell construction fails.
pub async fn build_embedded_shell(
    cwd: &Path,
    dyn_tools: Option<DynToolList>,
    include_fs_mutations: bool,
) -> Result<Shell<OmpExt>> {
    let (safe_utils, fs_mutation_utils) = split_utility_builtins();
    let mut builder = Shell::builder()
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(cwd.to_path_buf())
        .do_not_inherit_env(true)
        .external_cmd_leads_session(true)
        .builtins(builtins::default_builtins())
        .builtins(safe_utils.into_iter().map(|(n, r)| (n.to_owned(), r)))
        .builtins(
            process_builtins::<OmpExt>()
                .into_iter()
                .map(|(n, r)| (n.to_owned(), r)),
        );
    if include_fs_mutations {
        builder = builder.builtins(
            fs_mutation_utils
                .into_iter()
                .map(|(n, r)| (n.to_owned(), r)),
        );
    }
    if let Some(tools) = dyn_tools {
        let host: Arc<dyn DynHost> = Arc::new(CliDynHost::new(tools));
        builder = builder.builtin("dyn", dyn_builtin(host));
    }
    for (name, value) in std::env::vars_os() {
        let (Some(k), Some(v)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        let mut var = ShellVariable::new(v);
        var.export();
        builder = builder.var(k, var);
    }
    builder
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("failed to initialize embedded shell: {e}"))
}

/// Parameters for one in-process shell execution.
pub struct ShellExecRequest<'a> {
    /// Working directory and workspace write boundary root.
    pub cwd: &'a Path,
    /// Shell command string to execute.
    pub command: &'a str,
    /// Optional execution deadline in seconds.
    pub timeout: Option<f64>,
    /// Abort signal cancelling execution and tearing down child process groups.
    pub signal: &'a AbortSignal,
    /// Snapshot of host tools exposed through the `dyn` builtin.
    pub dyn_tools: Option<DynToolList>,
    /// Byte cap per head/tail window retained in memory and tap.
    pub retain_bytes: usize,
    /// Optional streaming callback receiving decoded UTF-8 output chunks.
    pub on_chunk: Option<ChunkCallback<'a>>,
    /// Optional live output tap exposed via `proc://`.
    pub tap: Option<Arc<Mutex<String>>>,
}

/// Result returned by [`execute_shell`].
pub struct ShellExecResult {
    /// Captured combined stdout/stderr output (middle-elided past `2 * retain_bytes`).
    pub output: String,
    /// Process/shell exit code when execution settled normally.
    pub exit_code: Option<i32>,
    /// Total elapsed wall time for this invocation.
    pub elapsed: Duration,
    /// Early termination reason (`"timeout"` or `"aborted"`), if interrupted.
    pub outcome: Option<&'static str>,
}

/// Executes `req.command` inside `omp-shell` with `PathPolicy`, `SpawnWrapper`,
/// `SpawnBook` (`-pgid` cancellation teardown), and bounded streaming capture.
///
/// # Errors
/// Returns an error if shell setup, pipe creation, or output I/O fails.
pub async fn execute_shell(mut req: ShellExecRequest<'_>) -> Result<ShellExecResult> {
    let started = Instant::now();
    if let Ok((ir, pb)) = analyze_script(req.command, req.cwd, req.cwd) {
        let root_s = req.cwd.to_string_lossy();
        tracing::debug!(
            commands = ir.commands.len(),
            read_only = ir.is_read_only(),
            writes_outside = ir.writes_outside(&root_s),
            opaque_count = pb.opaque.len(),
            "analyzed shell command IR"
        );
    }
    let mut shell = build_embedded_shell(req.cwd, req.dyn_tools.take(), true).await?;

    let (pipe_read, pipe_write) = io::pipe().context("creating shell output pipe")?;
    let stderr_write = pipe_write
        .try_clone()
        .context("duplicating shell stderr pipe writer")?;
    let stdin_null =
        omp_shell::openfiles::null().map_err(|e| anyhow::anyhow!("opening null stdin: {e}"))?;

    let sandbox_on = crate::features::sandbox::enabled();
    let path_policy: Arc<dyn PathPolicy> = Arc::new(CliPathPolicy::new(req.cwd, sandbox_on));
    let spawn_wrapper: Arc<dyn SpawnWrapper> = Arc::new(CliSpawnWrapper::from_sandbox(req.cwd));
    let spawn_book = Arc::new(SpawnBook::default());
    let cancel_token = CancellationToken::new();

    let mut params: ExecutionParameters = shell.default_exec_params();
    params.set_fd(OpenFiles::STDIN_FD, stdin_null);
    params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(pipe_write));
    params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(stderr_write));
    params.set_path_policy(path_policy);
    params.set_spawn_wrapper(spawn_wrapper);
    params.set_spawn_observer(spawn_book.clone());
    params.set_process_scope(spawn_book.clone());
    params.set_cancel_token(cancel_token.clone());

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = pipe_read;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if chunk_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let source_info = SourceInfo::from("-c");
    let command = req.command.to_owned();
    let exec_fut: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin(async move {
        let res = shell.run_string(command, &source_info, &params).await;
        drop(params);
        drop(shell);
        res
    });
    tokio::pin!(exec_fut);

    let deadline = async {
        match req.timeout {
            Some(t) => tokio::time::sleep(Duration::from_secs_f64(t)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);

    let mut buf: Vec<u8> = Vec::new();
    let mut carry: Vec<u8> = Vec::new();
    let retain_bytes = req.retain_bytes;
    let mut on_chunk = req.on_chunk.take();
    let tap = req.tap.take();

    let mut drain_chunk = |bytes: &[u8]| {
        if on_chunk.is_some() || tap.is_some() {
            let text = decode_utf8_chunk(bytes, &mut carry);
            if let Some(cb) = on_chunk.as_mut() {
                cb(&text);
            }
            if let Some(tap) = &tap {
                let mut t = tap.lock().expect("job output tap poisoned");
                t.push_str(&text);
                if t.len() > retain_bytes {
                    let cut = super::util::floor_boundary(&t, t.len() - retain_bytes / 2);
                    t.drain(..cut);
                }
            }
        }
        buf.extend_from_slice(bytes);
        if buf.len() > retain_bytes * 2 {
            let tail = buf.split_off(buf.len() - retain_bytes / 2);
            buf.truncate(retain_bytes / 2);
            buf.extend_from_slice(&tail);
        }
    };

    let mut exec_done = false;
    let mut exit_code: Option<i32> = None;
    let mut outcome: Option<&'static str> = None;
    let mut pipe_open = true;

    loop {
        if exec_done && !pipe_open {
            break;
        }
        tokio::select! {
            res = &mut exec_fut, if !exec_done => {
                exec_done = true;
                match res {
                    Ok(er) => exit_code = Some(i32::from(u8::from(&er.exit_code))),
                    Err(err) => {
                        let _ = write!(io::sink(), "{err}");
                        exit_code = Some(1);
                    }
                }
            }
            maybe_chunk = chunk_rx.recv(), if pipe_open => {
                match maybe_chunk {
                    Some(bytes) => drain_chunk(&bytes),
                    None => pipe_open = false,
                }
            }
            () = &mut deadline, if outcome.is_none() => {
                outcome = Some("timeout");
                cancel_token.cancel();
                #[cfg(unix)]
                spawn_book.signal_groups(nix::sys::signal::Signal::SIGKILL);
                break;
            }
            () = req.signal.aborted(), if outcome.is_none() => {
                outcome = Some("aborted");
                cancel_token.cancel();
                #[cfg(unix)]
                spawn_book.signal_groups(nix::sys::signal::Signal::SIGKILL);
                break;
            }
        }
    }

    if outcome.is_some() {
        exit_code = None;
        while let Ok(bytes) = chunk_rx.try_recv() {
            drain_chunk(&bytes);
        }
    }
    reader_task.abort();

    if !carry.is_empty() {
        let tail = String::from_utf8_lossy(&carry).into_owned();
        if let Some(cb) = on_chunk.as_mut() {
            cb(&tail);
        }
        if let Some(tap) = &tap {
            tap.lock().expect("job output tap poisoned").push_str(&tail);
        }
    }

    Ok(ShellExecResult {
        output: String::from_utf8_lossy(&buf).into_owned(),
        exit_code,
        elapsed: started.elapsed(),
        outcome,
    })
}

fn decode_utf8_chunk(bytes: &[u8], carry: &mut Vec<u8>) -> String {
    if carry.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(s) => return s.to_owned(),
            Err(e) if e.error_len().is_none() => {
                let valid = e.valid_up_to();
                carry.extend_from_slice(&bytes[valid..]);
                return String::from_utf8_lossy(&bytes[..valid]).into_owned();
            }
            Err(_) => {}
        }
    }
    carry.extend_from_slice(bytes);
    let mut out = String::new();
    loop {
        match std::str::from_utf8(carry) {
            Ok(s) => {
                out.push_str(s);
                carry.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    out.push_str(&String::from_utf8_lossy(&carry[..valid]));
                }
                match e.error_len() {
                    Some(bad_len) => {
                        out.push('\u{FFFD}');
                        carry.drain(..valid + bad_len);
                    }
                    None => {
                        carry.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    out
}

use std::io::Write as _;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;
    impl AgentTool for EchoTool {
        fn name(&self) -> &'static str {
            "echo_device"
        }
        fn description(&self) -> &'static str {
            "Echo test device"
        }
        fn parameters(&self) -> &Value {
            static SCHEMA: std::sync::LazyLock<Value> = std::sync::LazyLock::new(
                || json!({"type":"object","properties":{"msg":{"type":"string"}},"required":["msg"]}),
            );
            &SCHEMA
        }
        fn execute(
            self: Arc<Self>,
            _tool_call_id: String,
            params: Value,
            _signal: AbortSignal,
            _on_update: AgentToolUpdateCallback,
        ) -> pa_agent::BoxFut<'static, Result<AgentToolResult>> {
            Box::pin(async move {
                let msg = params.get("msg").and_then(Value::as_str).unwrap_or("");
                Ok(AgentToolResult::text(format!("pong:{msg}")))
            })
        }
    }

    #[test]
    fn utility_split_separates_fs_mutations() {
        let (safe, muts) = split_utility_builtins();
        let mut_names: Vec<&str> = muts.iter().map(|(n, _)| *n).collect();
        assert_eq!(mut_names.len(), 3);
        assert!(mut_names.contains(&"rm"));
        assert!(mut_names.contains(&"mv"));
        assert!(mut_names.contains(&"ln"));
        assert!(!safe.iter().any(|(n, _)| matches!(*n, "rm" | "mv" | "ln")));
        assert!(safe.iter().any(|(n, _)| *n == "grep"));
    }

    #[test]
    fn path_policy_enforces_workspace_write_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        let outside = tmp.path().join("outside.txt");
        fs::create_dir_all(&root).unwrap();

        let policy = CliPathPolicy::new(&root, true);
        assert!(policy.check_read(&outside).is_ok());
        assert!(policy.check_write(&root.join("ok.txt")).is_ok());
        assert!(policy.check_write(Path::new("/dev/null")).is_ok());
        assert!(policy.check_write(&outside).is_err());
        assert!(policy.check_write(&root.join("../outside.txt")).is_err());
    }

    #[test]
    fn analysis_detects_readonly_vs_outside_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (ro, _) = analyze_script("grep foo bar.txt | wc -l", ws, ws).unwrap();
        assert!(ro.is_read_only());
        assert!(!ro.writes_outside(&ws.to_string_lossy()));

        let (rw, pb) = analyze_script("echo hi > /tmp/omp-outside-test.txt", ws, ws).unwrap();
        assert!(!rw.is_read_only());
        assert!(rw.writes_outside(&ws.to_string_lossy()));
        assert!(!pb.reads.is_empty() || !pb.writes.is_empty());
    }

    #[tokio::test]
    async fn execute_shell_runs_pipes_heredoc_and_dyn_host() {
        let tmp = tempfile::tempdir().unwrap();
        let sig = AbortSignal::never();
        let tools: DynToolList = Arc::new(vec![Arc::new(EchoTool)]);

        let res = execute_shell(ShellExecRequest {
            cwd: tmp.path(),
            command: "cat <<'EOF' | grep beta > out.txt\nalpha\nbeta\ngamma\nEOF\ncat out.txt",
            timeout: Some(10.0),
            signal: &sig,
            dyn_tools: Some(tools.clone()),
            retain_bytes: 65536,
            on_chunk: None,
            tap: None,
        })
        .await
        .unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert_eq!(res.output.trim(), "beta");

        let list_res = execute_shell(ShellExecRequest {
            cwd: tmp.path(),
            command: "dyn",
            timeout: Some(10.0),
            signal: &sig,
            dyn_tools: Some(tools.clone()),
            retain_bytes: 65536,
            on_chunk: None,
            tap: None,
        })
        .await
        .unwrap();
        assert_eq!(list_res.exit_code, Some(0));
        assert!(
            list_res.output.contains("echo_device"),
            "got: {}",
            list_res.output
        );

        let dyn_flag = execute_shell(ShellExecRequest {
            cwd: tmp.path(),
            command: "dyn echo_device --msg hi",
            timeout: Some(10.0),
            signal: &sig,
            dyn_tools: Some(tools.clone()),
            retain_bytes: 65536,
            on_chunk: None,
            tap: None,
        })
        .await
        .unwrap();
        assert_eq!(dyn_flag.exit_code, Some(0));
        assert!(
            dyn_flag.output.contains("pong:hi"),
            "got: {}",
            dyn_flag.output
        );

        let dyn_json = execute_shell(ShellExecRequest {
            cwd: tmp.path(),
            command: "dyn echo_device -j '{\"msg\":\"hi\"}'",
            timeout: Some(10.0),
            signal: &sig,
            dyn_tools: Some(tools),
            retain_bytes: 65536,
            on_chunk: None,
            tap: None,
        })
        .await
        .unwrap();
        assert_eq!(dyn_json.exit_code, Some(0));
        assert!(
            dyn_json.output.contains("pong:hi"),
            "got: {}",
            dyn_json.output
        );
    }

    #[tokio::test]
    async fn sandbox_path_policy_blocks_in_process_redirection_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let outside = tmp.path().join("escape.txt");

        let mut shell = build_embedded_shell(&ws, None, true).await.unwrap();
        let (out_read, out_write) = io::pipe().unwrap();
        let err_write = out_write.try_clone().unwrap();
        let mut params = shell.default_exec_params();
        params.set_fd(OpenFiles::STDIN_FD, omp_shell::openfiles::null().unwrap());
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(out_write));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(err_write));
        params.set_path_policy(Arc::new(CliPathPolicy::new(&ws, true)));

        let cmd = format!(
            "echo ok > inside.txt && echo secret > {}",
            outside.display()
        );
        let er = shell
            .run_string(cmd, &SourceInfo::from("-c"), &params)
            .await
            .unwrap();
        drop(params);
        drop(shell);
        let mut captured = String::new();
        let mut reader = out_read;
        let _ = reader.read_to_string(&mut captured);

        assert_ne!(
            u8::from(&er.exit_code),
            0,
            "expected outside write denial; output={captured}"
        );
        assert!(ws.join("inside.txt").exists());
        assert!(!outside.exists(), "outside file must not be created");
    }

    #[tokio::test]
    async fn builtin_registry_without_fs_mutations_omits_rm() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("keep.txt");
        fs::write(&target, "stay").unwrap();

        let mut shell = build_embedded_shell(tmp.path(), None, false).await.unwrap();
        let (out_read, out_write) = io::pipe().unwrap();
        let err_write = out_write.try_clone().unwrap();
        let mut params = shell.default_exec_params();
        params.set_fd(OpenFiles::STDIN_FD, omp_shell::openfiles::null().unwrap());
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(out_write));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(err_write));

        let er = shell
            .run_string("PATH='' command -v rm", &SourceInfo::from("-c"), &params)
            .await
            .unwrap();
        drop(params);
        drop(shell);
        let mut captured = String::new();
        let mut reader = out_read;
        let _ = reader.read_to_string(&mut captured);

        assert_ne!(
            u8::from(&er.exit_code),
            0,
            "rm must be absent when fs mutations excluded; output={captured}"
        );
        assert!(target.exists());
    }

    #[test]
    fn analysis_covers_pipes_heredocs_and_net_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let script =
            "cat <<'EOF' | grep foo\nfoo\nEOF\ncurl https://example.com/data.json > out.json";
        let (ir, pb) = analyze_script(script, ws, ws).unwrap();
        assert!(ir.commands.len() >= 3);
        assert!(!ir.is_read_only());
        assert!(!ir.writes_outside(&ws.to_string_lossy()));
        assert!(!pb.net.is_empty());
    }

    #[test]
    fn spawn_book_tracks_and_authorizes_pids() {
        let book = SpawnBook::default();
        book.on_spawn(4242, Some(4242));
        book.on_spawn(4243, Some(4242));
        assert_eq!(book.tracked_groups(), vec![4242]);
        assert!(book.may_signal(4242));
        assert!(book.may_observe(4243));
        assert!(!book.may_signal(9999));
    }

    #[tokio::test]
    async fn cancel_aborts_spawned_child_via_pgid() {
        let tmp = tempfile::tempdir().unwrap();
        let controller = AbortController::new();
        let sig = controller.signal();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            controller.abort();
        });
        let res = execute_shell(ShellExecRequest {
            cwd: tmp.path(),
            command: "sleep 30",
            timeout: Some(30.0),
            signal: &sig,
            dyn_tools: None,
            retain_bytes: 65536,
            on_chunk: None,
            tap: None,
        })
        .await
        .unwrap();
        assert_eq!(res.outcome, Some("aborted"));
        assert!(
            res.elapsed < Duration::from_secs(5),
            "elapsed: {:?}",
            res.elapsed
        );
    }
}
