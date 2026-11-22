//! Adapter discovery and config loading (omp `dap/config.ts`).
//!
//! Sources, lowest priority applied first (later overrides win):
//! 1. Built-in `defaults.json`
//! 2. User home root (`~/dap.json`, `~/.dap.json`, + yaml variants)
//! 3. User config dirs (`~/.omp`, `~/.pi`, `~/.claude` — visible and hidden files)
//! 4. Project config dirs (`<cwd>/.omp`, `<cwd>/.pi`, `<cwd>/.claude`)
//! 5. Project root files (highest priority)
//!
//! Adapters are then resolved against the filesystem: their command must exist
//! (project-local bin dirs first, then `$PATH`) before they are considered
//! available.
//!
//! Deviation from the TS original, deliberate and observable: adapters live in a
//! [`BTreeMap`] rather than a JS insertion-ordered object, so every
//! order-dependent choice (which unavailable candidate a launch reports) is
//! alphabetical instead of declaration-ordered. The final launch ordering is
//! unaffected — it tie-breaks on adapter name anyway.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};
use tracing::warn;

use crate::types::{
    DapAdapterConfig, DapAttachTarget, DapConnectMode, DapResolvedAdapter, LaunchAdapterSelection,
    LaunchProgramKind,
};

/// omp `dap/defaults.json`, byte-identical.
const DEFAULTS_JSON: &str = include_str!("defaults.json");

const CONFIG_FILENAMES: [&str; 6] = [
    "dap.json",
    ".dap.json",
    "dap.yaml",
    ".dap.yaml",
    "dap.yml",
    ".dap.yml",
];

/// Extensionless debuggers tried first when no file extension matched.
const EXTENSIONLESS_DEBUGGER_ORDER: [&str; 2] = ["gdb", "lldb-dap"];

/// Env var naming an explicit `dapDebugServer.js` for js-debug.
const JS_DEBUG_SERVER_ENV: &str = "JS_DEBUG_DAP_SERVER";

/// Placeholder in adapter args replaced with the reserved TCP port.
pub const DAP_PORT_ARGUMENT: &str = "${port}";

/// Timeout for the attach probes (`/proc/<pid>/comm`, V8 inspector HTTP).
const ATTACH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Timeout for the `ps` fallback that names a process image off Linux.
#[cfg(not(target_os = "linux"))]
const PS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

// =============================================================================
// Config File Parsing / Normalization
// =============================================================================

/// Parse a config file as YAML (`.yaml`/`.yml`) or JSON.
///
/// # Errors
///
/// Returns `Err` on unparsable content.
pub fn parse_config_content(content: &str, file_path: &Path) -> anyhow::Result<Value> {
    match file_path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("yaml" | "yml") => Ok(serde_yaml::from_str(content)?),
        _ => Ok(serde_json::from_str(content)?),
    }
}

fn normalize_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_object(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn normalize_connect_mode(value: Option<&Value>) -> Option<DapConnectMode> {
    match value.and_then(Value::as_str) {
        Some("socket") => Some(DapConnectMode::Socket),
        Some("tcp") => Some(DapConnectMode::Tcp),
        _ => None,
    }
}

/// Normalize one raw adapter entry; `None` when `command` is missing/empty.
fn normalize_adapter_config(config: &Value) -> Option<DapAdapterConfig> {
    if !config.is_object() {
        return None;
    }
    let command = config
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    Some(DapAdapterConfig {
        command,
        args: normalize_string_array(config.get("args")),
        languages: normalize_string_array(config.get("languages")),
        file_types: normalize_string_array(config.get("fileTypes"))
            .into_iter()
            .map(|entry| entry.to_lowercase())
            .collect(),
        root_markers: normalize_string_array(config.get("rootMarkers")),
        launch_defaults: normalize_object(config.get("launchDefaults")),
        attach_defaults: normalize_object(config.get("attachDefaults")),
        connect_mode: normalize_connect_mode(config.get("connectMode")),
        accepts_directory_program: config
            .get("acceptsDirectoryProgram")
            .and_then(Value::as_bool)
            == Some(true),
    })
}

/// The built-in adapters, normalized once.
fn default_adapters() -> BTreeMap<String, DapAdapterConfig> {
    let defaults: Value = serde_json::from_str(DEFAULTS_JSON).unwrap_or(Value::Null);
    let mut adapters = BTreeMap::new();
    if let Some(obj) = defaults.as_object() {
        for (name, raw) in obj {
            if let Some(cfg) = normalize_adapter_config(raw) {
                adapters.insert(name.clone(), cfg);
            }
        }
    }
    adapters
}

fn read_config_file(file_path: &Path) -> Option<BTreeMap<String, Value>> {
    let content = std::fs::read_to_string(file_path).ok()?;
    let parsed = parse_config_content(&content, file_path).ok()?;
    // `{ "adapters": { … } }` or a bare `{ "<name>": { … } }` map.
    let adapters = parsed
        .get("adapters")
        .filter(|v| v.is_object())
        .or(Some(&parsed))?;
    Some(
        adapters
            .as_object()?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// Shallow-merge one adapter entry over its existing config.
///
/// `launchDefaults`/`attachDefaults` merge key-by-key (TS spreads both sides);
/// every other field replaces.
fn merge_adapter(base: Option<&DapAdapterConfig>, raw: &Value) -> Option<DapAdapterConfig> {
    let Some(existing) = base else {
        return normalize_adapter_config(raw);
    };
    if !raw.is_object() {
        return None;
    }
    let mut merged = serde_json::to_value(existing).ok()?;
    let Some(merged_obj) = merged.as_object_mut() else {
        return None;
    };
    for key in ["launchDefaults", "attachDefaults"] {
        let base_defaults = merged_obj.get(key).cloned().unwrap_or(Value::Null);
        let raw_defaults = raw.get(key).cloned().unwrap_or(Value::Null);
        if base_defaults.is_object() || raw_defaults.is_object() {
            let mut combined = normalize_object(Some(&base_defaults));
            for (k, v) in normalize_object(Some(&raw_defaults)) {
                combined.insert(k, v);
            }
            merged_obj.insert(key.to_string(), Value::Object(combined));
        }
    }
    let Some(raw_obj) = raw.as_object() else {
        return None;
    };
    for (k, v) in raw_obj {
        if k == "launchDefaults" || k == "attachDefaults" {
            continue;
        }
        merged_obj.insert(k.clone(), v.clone());
    }
    normalize_adapter_config(&merged)
}

fn merge_adapters(
    base: &BTreeMap<String, DapAdapterConfig>,
    overrides: &BTreeMap<String, Value>,
) -> BTreeMap<String, DapAdapterConfig> {
    let mut merged = base.clone();
    for (name, raw) in overrides {
        match merge_adapter(merged.get(name), raw) {
            Some(cfg) => {
                merged.insert(name.clone(), cfg);
            }
            None => warn!("Ignoring invalid DAP adapter override (keeping previous config): {name}"),
        }
    }
    merged
}

fn config_dirs(base: &Path) -> Vec<PathBuf> {
    [".omp", ".pi", ".claude"]
        .iter()
        .map(|d| base.join(d))
        .collect()
}

fn source_files(cwd: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();
    // Highest priority first (reversed before application).
    for filename in CONFIG_FILENAMES {
        sources.push(cwd.join(filename));
    }
    for dir in config_dirs(cwd) {
        for filename in CONFIG_FILENAMES {
            sources.push(dir.join(filename));
        }
    }
    if let Some(home) = home_dir() {
        for dir in config_dirs(&home) {
            for filename in CONFIG_FILENAMES {
                sources.push(dir.join(filename));
            }
        }
        for filename in CONFIG_FILENAMES {
            sources.push(home.join(filename));
        }
    }
    sources
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Load and merge the adapter config for `cwd` (does not consult the cache).
#[must_use]
pub fn load_adapter_configs(cwd: &Path) -> BTreeMap<String, DapAdapterConfig> {
    let mut adapters = default_adapters();
    // TS reverses the source list so the lowest priority applies first.
    for file in source_files(cwd).iter().rev() {
        if let Some(overrides) = read_config_file(file) {
            adapters = merge_adapters(&adapters, &overrides);
        }
    }
    adapters
}

/// Adapter configs for `cwd`, cached per normalized path.
#[must_use]
pub fn get_adapter_configs(cwd: &Path) -> Arc<BTreeMap<String, DapAdapterConfig>> {
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, Arc<BTreeMap<String, DapAdapterConfig>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let key = normalize_path(cwd);
    if let Ok(guard) = cache.lock() {
        if let Some(cfg) = guard.get(&key) {
            return Arc::clone(cfg);
        }
    }
    let cfg = Arc::new(load_adapter_configs(&key));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, Arc::clone(&cfg));
    }
    cfg
}

/// Drop the cached config for `cwd` (tests, and `reload` of adapter settings).
pub fn invalidate_config(cwd: &Path) {
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, Arc<BTreeMap<String, DapAdapterConfig>>>>> =
        OnceLock::new();
    if let Some(cache) = CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            guard.remove(&normalize_path(cwd));
        }
    }
}

// =============================================================================
// Paths / Root Markers / Binary Resolution
// =============================================================================

/// Lexically normalize a path (no filesystem access, no symlink resolution).
#[must_use]
pub fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Resolve `file` against `cwd` and normalize it.
#[must_use]
pub fn resolve_to_cwd(file: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(file);
    if p.is_absolute() {
        normalize_path(&p)
    } else {
        normalize_path(&cwd.join(p))
    }
}

/// Check if any root marker exists in `cwd` (glob markers match one readdir level).
#[must_use]
pub fn has_root_markers(cwd: &Path, markers: &[String]) -> bool {
    let mut entries: Option<Vec<String>> = None;
    for marker in markers {
        if marker.contains('*') {
            if entries.is_none() {
                entries = Some(
                    std::fs::read_dir(cwd)
                        .map(|rd| {
                            rd.filter_map(Result::ok)
                                .filter_map(|e| e.file_name().into_string().ok())
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            if let Some(list) = &entries {
                if list.iter().any(|e| glob_match(marker, e)) {
                    return true;
                }
            }
            continue;
        }
        if cwd.join(marker).exists() {
            return true;
        }
    }
    false
}

/// Minimal glob: `*` matches any run of characters, `?` one character.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => (0..=n.len()).any(|i| inner(&p[1..], &n[i..])),
            b'?' => !n.is_empty() && inner(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && inner(&p[1..], &n[1..]),
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

const PYTHON_ROOT_MARKERS: [&str; 9] = [
    "pyproject.toml",
    "ty.toml",
    "requirements.txt",
    "setup.py",
    "setup.cfg",
    "Pipfile",
    "pyrightconfig.json",
    "ruff.toml",
    ".ruff.toml",
];

const LOCAL_BIN_PATHS: [(&[&str], &str); 8] = [
    (
        &[
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
        ],
        "node_modules/.bin",
    ),
    (&PYTHON_ROOT_MARKERS, ".venv/bin"),
    (&PYTHON_ROOT_MARKERS, ".venv/Scripts"),
    (&PYTHON_ROOT_MARKERS, "venv/bin"),
    (&PYTHON_ROOT_MARKERS, "venv/Scripts"),
    (&PYTHON_ROOT_MARKERS, ".env/bin"),
    (&PYTHON_ROOT_MARKERS, ".env/Scripts"),
    (&["Gemfile", "Gemfile.lock"], "vendor/bundle/bin"),
];

fn markers_of(markers: &[&str]) -> Vec<String> {
    markers.iter().map(ToString::to_string).collect()
}

fn resolve_command_from_local_root(command: &str, root: &Path) -> Option<PathBuf> {
    for (markers, bin_dir) in LOCAL_BIN_PATHS {
        if !has_root_markers(root, &markers_of(markers)) {
            continue;
        }
        let candidate = root.join(bin_dir).join(command);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(windows)]
        for ext in [".exe", ".cmd", ".bat"] {
            let candidate = root.join(bin_dir).join(format!("{command}{ext}"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // Go-style project-local bin (markers go.mod/go.sum/go.work) shares `bin/`.
    if has_root_markers(root, &markers_of(&["go.mod", "go.sum", "go.work"])) {
        let candidate = root.join("bin").join(command);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a command to an executable path: `local_roots` bin dirs, `cwd`'s bin
/// dirs, then `$PATH`.
#[must_use]
pub fn resolve_command(command: &str, cwd: &Path, local_roots: &[PathBuf]) -> Option<String> {
    for root in local_roots {
        if let Some(local) = resolve_command_from_local_root(command, root) {
            return Some(local.to_string_lossy().into_owned());
        }
    }
    if let Some(local) = resolve_command_from_local_root(command, cwd) {
        return Some(local.to_string_lossy().into_owned());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(command);
        if is_executable(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
        #[cfg(windows)]
        for ext in [".exe", ".cmd", ".bat"] {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.exists() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn normalize_command_for_cwd(command: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(command);
    if p.is_absolute() {
        return p;
    }
    if command.starts_with("./")
        || command.starts_with("../")
        || command.starts_with(".\\")
        || command.starts_with("..\\")
    {
        return normalize_path(&cwd.join(command));
    }
    p
}

// =============================================================================
// js-debug-adapter: dapDebugServer.js special case
// =============================================================================

fn resolve_js_debug_server_path(cwd: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = std::env::var_os(JS_DEBUG_SERVER_ENV) {
        candidates.push(normalize_path(&cwd.join(configured)));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| {
        home_dir().map(|h| h.join(".local").join("share"))
    });
    if let Some(data_home) = data_home {
        candidates.push(
            data_home
                .join("nvim/mason/packages/js-debug-adapter/js-debug/src/dapDebugServer.js"),
        );
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".local/opt/js-debug/src/dapDebugServer.js"));
    }
    candidates.into_iter().find(|c| c.exists())
}

fn resolve_default_js_debug_adapter(
    adapter_name: &str,
    config: &DapAdapterConfig,
    cwd: &Path,
    local_roots: &[PathBuf],
) -> Option<Option<DapResolvedAdapter>> {
    if adapter_name != "js-debug-adapter" || config.command != "js-debug-adapter" {
        return None;
    }
    let server_path = resolve_js_debug_server_path(cwd)?;
    let node_command = resolve_command("node", cwd, local_roots);
    let resolved_command = node_command
        .clone()
        .or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))?;
    Some(Some(DapResolvedAdapter {
        name: adapter_name.to_string(),
        command: if node_command.is_some() { "node" } else { "bun" }.to_string(),
        args: vec![
            server_path.to_string_lossy().into_owned(),
            DAP_PORT_ARGUMENT.to_string(),
            "127.0.0.1".to_string(),
        ],
        resolved_command,
        languages: config.languages.clone(),
        file_types: config.file_types.clone(),
        root_markers: config.root_markers.clone(),
        launch_defaults: config.launch_defaults.clone(),
        attach_defaults: config.attach_defaults.clone(),
        connect_mode: DapConnectMode::Tcp,
        accepts_directory_program: config.accepts_directory_program,
    }))
}

// =============================================================================
// Adapter Resolution
// =============================================================================

fn resolve_adapter_from_config(
    adapter_name: &str,
    configs: &BTreeMap<String, DapAdapterConfig>,
    cwd: &Path,
    local_roots: &[PathBuf],
) -> Option<DapResolvedAdapter> {
    let config = configs.get(adapter_name)?;
    if let Some(js_debug) = resolve_default_js_debug_adapter(adapter_name, config, cwd, local_roots) {
        return js_debug;
    }
    let normalized_command = normalize_command_for_cwd(&config.command, cwd);
    let command_is_bare = !normalized_command.is_absolute()
        && !config.command.contains('/')
        && !config.command.contains('\\');
    let resolved_command = resolve_command(
        &normalized_command.to_string_lossy(),
        cwd,
        if command_is_bare { local_roots } else { &[] },
    )?;
    Some(DapResolvedAdapter {
        name: adapter_name.to_string(),
        command: config.command.clone(),
        args: config.args.clone(),
        resolved_command,
        languages: config.languages.clone(),
        file_types: config.file_types.clone(),
        root_markers: config.root_markers.clone(),
        launch_defaults: config.launch_defaults.clone(),
        attach_defaults: config.attach_defaults.clone(),
        connect_mode: config.connect_mode.unwrap_or_default(),
        accepts_directory_program: config.accepts_directory_program,
    })
}

/// Resolve one named adapter, or `None` when it is unknown or not installed.
#[must_use]
pub fn resolve_adapter(adapter_name: &str, cwd: &Path) -> Option<DapResolvedAdapter> {
    let configs = get_adapter_configs(cwd);
    resolve_adapter_from_config(adapter_name, &configs, cwd, &[])
}

/// Every adapter whose binary resolves for `cwd`.
#[must_use]
pub fn get_available_adapters(cwd: &Path) -> Vec<DapResolvedAdapter> {
    let configs = get_adapter_configs(cwd);
    configs
        .keys()
        .filter_map(|name| resolve_adapter_from_config(name, &configs, cwd, &[]))
        .collect()
}

/// First ancestor directory of `program` that carries any of `markers`.
fn find_root_marker_in_launch_ancestry(
    program: &Path,
    markers: &[String],
    program_kind: LaunchProgramKind,
) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    let mut dir = if program_kind == LaunchProgramKind::Directory {
        program.to_path_buf()
    } else {
        program.parent()?.to_path_buf()
    };
    loop {
        if has_root_markers(&dir, markers) {
            return Some(dir);
        }
        let parent = dir.parent()?.to_path_buf();
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

fn resolve_adapter_for_launch(
    adapter_name: &str,
    configs: &BTreeMap<String, DapAdapterConfig>,
    cwd: &Path,
    root_dir: Option<&Path>,
) -> Option<DapResolvedAdapter> {
    let local_roots: Vec<PathBuf> = match root_dir {
        Some(root) if root != cwd => vec![root.to_path_buf(), cwd.to_path_buf()],
        _ => Vec::new(),
    };
    resolve_adapter_from_config(adapter_name, configs, cwd, &local_roots)
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn unavailable_adapter(
    name: &str,
    configs: &BTreeMap<String, DapAdapterConfig>,
) -> LaunchAdapterSelection {
    match configs.get(name) {
        Some(config) => LaunchAdapterSelection::Unavailable {
            adapter_name: name.to_string(),
            command: config.command.clone(),
        },
        None => LaunchAdapterSelection::None,
    }
}

fn extensionless_rank(name: &str) -> usize {
    EXTENSIONLESS_DEBUGGER_ORDER
        .iter()
        .position(|n| *n == name)
        .unwrap_or(usize::MAX)
}

/// Extension match first, then root match, then gdb/lldb-dap, then name.
fn sort_adapters_for_launch(
    program: &Path,
    _cwd: &Path,
    program_kind: LaunchProgramKind,
    mut adapters: Vec<DapResolvedAdapter>,
) -> Vec<DapResolvedAdapter> {
    let extension = extension_of(program);
    adapters.sort_by(|left, right| {
        let left_ext = !extension.is_empty() && left.file_types.contains(&extension);
        let right_ext = !extension.is_empty() && right.file_types.contains(&extension);
        if left_ext != right_ext {
            return if left_ext {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        let left_root =
            find_root_marker_in_launch_ancestry(program, &left.root_markers, program_kind).is_some();
        let right_root =
            find_root_marker_in_launch_ancestry(program, &right.root_markers, program_kind).is_some();
        if left_root != right_root {
            return if left_root {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        extensionless_rank(&left.name)
            .cmp(&extensionless_rank(&right.name))
            .then_with(|| left.name.cmp(&right.name))
    });
    adapters
}

fn select_automatic_launch_adapter(
    program: &Path,
    cwd: &Path,
    program_kind: LaunchProgramKind,
    configs: &BTreeMap<String, DapAdapterConfig>,
) -> LaunchAdapterSelection {
    let extension = extension_of(program);
    if !extension.is_empty() {
        let mut configured: Vec<String> = Vec::new();
        let mut available: Vec<DapResolvedAdapter> = Vec::new();
        for (name, config) in configs {
            if !config.file_types.contains(&extension) {
                continue;
            }
            let root_dir = find_root_marker_in_launch_ancestry(program, &config.root_markers, program_kind);
            configured.push(name.clone());
            if let Some(adapter) =
                resolve_adapter_for_launch(name, configs, cwd, root_dir.as_deref())
            {
                available.push(adapter);
            }
        }
        if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, available).first()
        {
            return LaunchAdapterSelection::Adapter(selected.clone());
        }
        let root_match = configured
            .iter()
            .find(|name| {
                configs.get(*name).is_some_and(|config| {
                    find_root_marker_in_launch_ancestry(program, &config.root_markers, program_kind)
                        .is_some()
                })
            })
            .or_else(|| configured.first());
        if let Some(name) = root_match {
            return unavailable_adapter(name, configs);
        }
    }

    let mut available: Vec<DapResolvedAdapter> = Vec::new();
    let mut root_matches: Vec<String> = Vec::new();
    let mut directory_matches: Vec<String> = Vec::new();
    for (name, config) in configs {
        let root_dir = find_root_marker_in_launch_ancestry(program, &config.root_markers, program_kind);
        if root_dir.is_some() {
            root_matches.push(name.clone());
            if config.accepts_directory_program {
                directory_matches.push(name.clone());
            }
        }
        if extensionless_rank(name) == usize::MAX && root_dir.is_none() {
            continue;
        }
        if let Some(adapter) = resolve_adapter_for_launch(name, configs, cwd, root_dir.as_deref()) {
            available.push(adapter);
        }
    }

    if program_kind == LaunchProgramKind::Directory && !directory_matches.is_empty() {
        let directory_adapters: Vec<DapResolvedAdapter> = available
            .into_iter()
            .filter(|a| a.accepts_directory_program && directory_matches.contains(&a.name))
            .collect();
        if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, directory_adapters)
            .first()
        {
            return LaunchAdapterSelection::Adapter(selected.clone());
        }
        return directory_matches
            .first()
            .map_or(LaunchAdapterSelection::None, |name| {
                unavailable_adapter(name, configs)
            });
    }

    let candidates = if program_kind == LaunchProgramKind::Directory {
        let only: Vec<DapResolvedAdapter> = available
            .iter()
            .filter(|a| a.accepts_directory_program)
            .cloned()
            .collect();
        if only.is_empty() {
            available
        } else {
            only
        }
    } else {
        available
    };
    if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, candidates).first() {
        return LaunchAdapterSelection::Adapter(selected.clone());
    }
    root_matches
        .first()
        .map_or(LaunchAdapterSelection::None, |name| {
            unavailable_adapter(name, configs)
        })
}

/// Select a launch adapter, or report why the matching configuration cannot run.
#[must_use]
pub fn select_launch_adapter(
    program: &Path,
    cwd: &Path,
    adapter_name: Option<&str>,
    program_kind: LaunchProgramKind,
) -> LaunchAdapterSelection {
    let configs = get_adapter_configs(cwd);
    if let Some(name) = adapter_name {
        let Some(config) = configs.get(name) else {
            return LaunchAdapterSelection::None;
        };
        let root_dir = find_root_marker_in_launch_ancestry(program, &config.root_markers, program_kind);
        return match resolve_adapter_for_launch(name, &configs, cwd, root_dir.as_deref()) {
            Some(adapter) => LaunchAdapterSelection::Adapter(adapter),
            None => LaunchAdapterSelection::Unavailable {
                adapter_name: name.to_string(),
                command: config.command.clone(),
            },
        };
    }
    select_automatic_launch_adapter(program, cwd, program_kind, &configs)
}

// =============================================================================
// Attach Adapter Selection
// =============================================================================

/// Adapter for the runtime a process runs, keyed on its executable name.
const RUNTIME_ADAPTERS: [(&str, &str); 5] = [
    ("js-debug-adapter", r"^(node|nodejs|deno|electron)(\.exe)?$"),
    ("debugpy", r"^python(\d+(\.\d+)*)?(\.exe)?$"),
    ("netcoredbg", r"^dotnet(\.exe)?$"),
    ("rdbg", r"^ruby(\.exe)?$"),
    ("php-debug-adapter", r"^php(\.exe)?$"),
];

/// Executable name of a running process, or `None` when it cannot be read.
async fn process_image_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let comm = tokio::fs::read_to_string(format!("/proc/{pid}/comm")).await.ok()?;
        let name = comm.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = tokio::time::timeout(
            PS_PROBE_TIMEOUT,
            tokio::process::Command::new("ps")
                .args(["-o", "comm=", "-p", &pid.to_string()])
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        let image = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!image.is_empty()).then(|| {
            Path::new(&image)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(image)
        })
    }
}

/// Whether `host:port` serves a V8 inspector (`node --inspect`), which js-debug
/// attaches to. Speaks just enough HTTP/1.1 to read `/json/version`.
async fn is_v8_inspector(host: &str, port: u16) -> bool {
    let probe = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect((host, port)).await.ok()?;
        let request = format!(
            "GET /json/version HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.ok()?;
        let text = String::from_utf8_lossy(&body);
        let status_ok = text
            .split("\r\n")
            .next()
            .is_some_and(|line| line.contains(" 200"));
        Some(
            status_ok
                && (text.contains("\"V8-Version\"") || text.contains("\"Protocol-Version\"")),
        )
    };
    matches!(
        tokio::time::timeout(ATTACH_PROBE_TIMEOUT, probe).await,
        Ok(Some(true))
    )
}

/// Match a process image name against the runtime→adapter table.
fn runtime_adapter_for(image: &str) -> Option<&'static str> {
    RUNTIME_ADAPTERS.iter().find_map(|(adapter, pattern)| {
        // The patterns are a fixed, trusted table; `regex` is not a dependency
        // here, so match the five shapes directly.
        let matched = match *pattern {
            p if p.contains("node|nodejs") => {
                let stem = image.trim_end_matches(".exe");
                matches!(stem, "node" | "nodejs" | "deno" | "electron")
            }
            p if p.starts_with("^python") => {
                let stem = image.trim_end_matches(".exe");
                stem == "python"
                    || stem
                        .strip_prefix("python")
                        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
            }
            p if p.contains("dotnet") => image.trim_end_matches(".exe") == "dotnet",
            p if p.contains("ruby") => image.trim_end_matches(".exe") == "ruby",
            p if p.contains("php") => image.trim_end_matches(".exe") == "php",
            _ => false,
        };
        matched.then_some(*adapter)
    })
}

/// Adapter for an attach. Without an explicit adapter it follows the target: the
/// runtime of the process `pid` runs (node → js-debug, python → debugpy, …), or
/// for a port, js-debug when it serves a V8 inspector and debugpy otherwise.
/// Native targets fall back to gdb/lldb-dap.
#[must_use]
pub async fn select_attach_adapter(
    cwd: &Path,
    adapter_name: Option<&str>,
    target: DapAttachTarget,
) -> Option<DapResolvedAdapter> {
    if let Some(name) = adapter_name {
        return resolve_adapter(name, cwd);
    }
    let available = get_available_adapters(cwd);
    let by_name = |name: &str| available.iter().find(|a| a.name == name).cloned();
    if let Some(pid) = target.pid {
        if let Some(image) = process_image_name(pid).await {
            if let Some(name) = runtime_adapter_for(&image) {
                if let Some(found) = by_name(name) {
                    return Some(found);
                }
            }
        }
    }
    if let Some(port) = target.port {
        if let Some(js_debug) = by_name("js-debug-adapter") {
            if is_v8_inspector(target.host.unwrap_or("127.0.0.1"), port).await {
                return Some(js_debug);
            }
        }
        if let Some(debugpy) = by_name("debugpy") {
            return Some(debugpy);
        }
    }
    for preferred in EXTENSIONLESS_DEBUGGER_ORDER {
        if let Some(found) = available.iter().find(|a| a.name == preferred) {
            return Some(found.clone());
        }
    }
    available.first().cloned()
}

// =============================================================================
// Launch Overrides
// =============================================================================

/// Adapter-specific launch arguments that depend on the resolved program.
///
/// Returned values are spread over `launch_defaults` so they take precedence over
/// the static defaults but can still be overridden by the fields `launch` sets
/// explicitly (`program`, `cwd`, `args`). Scoped to `dlv`, where `mode` selects
/// how the program path is interpreted: directories and `.go` sources debug as a
/// Go package (`mode=debug`), anything else as a compiled binary (`mode=exec`).
#[must_use]
pub fn resolve_launch_overrides(
    adapter: &DapResolvedAdapter,
    program: &Path,
    program_kind: LaunchProgramKind,
) -> Map<String, Value> {
    let mut overrides = Map::new();
    if adapter.name != "dlv" {
        return overrides;
    }
    let extension = extension_of(program);
    if program_kind == LaunchProgramKind::Directory || extension == ".go" {
        overrides.insert("mode".to_string(), Value::String("debug".to_string()));
    } else if program_kind == LaunchProgramKind::File {
        overrides.insert("mode".to_string(), Value::String("exec".to_string()));
    }
    overrides
}

/// Classify a launch program path as file, directory or missing.
#[must_use]
pub fn classify_launch_program(program: &Path) -> LaunchProgramKind {
    match std::fs::metadata(program) {
        Ok(meta) if meta.is_dir() => LaunchProgramKind::Directory,
        Ok(_) => LaunchProgramKind::File,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LaunchProgramKind::Missing,
        Err(_) => LaunchProgramKind::File,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> BTreeMap<String, DapAdapterConfig> {
        default_adapters()
    }

    #[test]
    fn defaults_carry_every_upstream_adapter() {
        let adapters = defaults();
        let names: Vec<&str> = adapters.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "bash-debug-adapter",
                "codelldb",
                "dart-debug-adapter",
                "debugpy",
                "dlv",
                "elixir-ls-debugger",
                "flutter-debug-adapter",
                "gdb",
                "js-debug-adapter",
                "kotlin-debug-adapter",
                "lldb-dap",
                "netcoredbg",
                "php-debug-adapter",
                "rdbg",
            ]
        );
        assert_eq!(adapters["gdb"].command, "gdb");
        assert_eq!(adapters["gdb"].args, ["-i", "dap"]);
        assert!(adapters["dlv"].accepts_directory_program);
        assert_eq!(adapters["dlv"].connect_mode, Some(DapConnectMode::Socket));
        assert_eq!(
            adapters["debugpy"].launch_defaults.get("justMyCode"),
            Some(&json!(false))
        );
    }

    #[test]
    fn file_types_normalize_to_lowercase_with_dot() {
        let raw = json!({"command": "x", "fileTypes": [".PY", "Js"]});
        let cfg = normalize_adapter_config(&raw).unwrap();
        assert_eq!(cfg.file_types, [".py", "js"]);
    }

    #[test]
    fn invalid_adapter_without_command_is_rejected() {
        assert!(normalize_adapter_config(&json!({"args": []})).is_none());
        assert!(normalize_adapter_config(&json!({"command": ""})).is_none());
        assert!(normalize_adapter_config(&json!("nope")).is_none());
    }

    #[test]
    fn merge_keeps_defaults_and_overrides_scalars() {
        let base = defaults();
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "debugpy".to_string(),
            json!({
                "command": "python3",
                "launchDefaults": {"justMyCode": true},
                "attachDefaults": {"extra": 1}
            }),
        );
        let merged = merge_adapters(&base, &overrides);
        let debugpy = &merged["debugpy"];
        assert_eq!(debugpy.command, "python3");
        // Scalar replaced, sibling default kept.
        assert_eq!(debugpy.launch_defaults.get("justMyCode"), Some(&json!(true)));
        assert_eq!(debugpy.launch_defaults.get("stopOnEntry"), Some(&json!(true)));
        // Existing attach defaults survive, new key added.
        assert_eq!(debugpy.attach_defaults.get("request"), Some(&json!("attach")));
        assert_eq!(debugpy.attach_defaults.get("extra"), Some(&json!(1)));
        // Unrelated adapters untouched.
        assert_eq!(merged["gdb"].command, "gdb");
    }

    #[test]
    fn invalid_override_keeps_the_previous_config() {
        let base = defaults();
        let mut overrides = BTreeMap::new();
        overrides.insert("gdb".to_string(), json!({"command": ""}));
        let merged = merge_adapters(&base, &overrides);
        assert_eq!(merged["gdb"].command, "gdb");
    }

    #[test]
    fn yaml_and_json_configs_parse() {
        let yaml = parse_config_content(
            "gdb:\n  command: my-gdb\n",
            Path::new("/tmp/dap.yaml"),
        )
        .unwrap();
        assert_eq!(yaml["gdb"]["command"], json!("my-gdb"));
        let json = parse_config_content(r#"{"gdb": {"command": "g2"}}"#, Path::new("/tmp/dap.json"))
            .unwrap();
        assert_eq!(json["gdb"]["command"], json!("g2"));
    }

    #[test]
    fn config_files_layer_project_over_user() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(
            cwd.join("dap.json"),
            r#"{"adapters": {"gdb": {"command": "project-gdb"}}}"#,
        )
        .unwrap();
        let configs = load_adapter_configs(cwd);
        assert_eq!(configs["gdb"].command, "project-gdb");
        // Sibling fields from the built-in entry survive the override.
        assert_eq!(configs["gdb"].args, ["-i", "dap"]);
    }

    #[test]
    fn project_config_dirs_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join(".omp")).unwrap();
        std::fs::write(
            cwd.join(".omp").join("dap.yml"),
            "lldb-dap:\n  command: lldb-vscode\n",
        )
        .unwrap();
        let configs = load_adapter_configs(cwd);
        assert_eq!(configs["lldb-dap"].command, "lldb-vscode");
    }

    #[test]
    fn root_markers_see_globs_and_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.sln"), "").unwrap();
        assert!(has_root_markers(dir.path(), &markers_of(&["*.sln"])));
        assert!(!has_root_markers(dir.path(), &markers_of(&["*.csproj"])));
        assert!(has_root_markers(dir.path(), &markers_of(&["app.sln"])));
        assert!(!has_root_markers(dir.path(), &markers_of(&["Cargo.toml"])));
    }

    #[test]
    fn root_marker_ancestry_walks_up_from_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let program = nested.join("app");
        assert_eq!(
            find_root_marker_in_launch_ancestry(
                &program,
                &markers_of(&["Cargo.toml"]),
                LaunchProgramKind::File
            ),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(
            find_root_marker_in_launch_ancestry(
                &program,
                &markers_of(&["nope.toml"]),
                LaunchProgramKind::File
            ),
            None
        );
    }

    #[test]
    fn resolve_to_cwd_normalizes() {
        let cwd = Path::new("/work/project");
        assert_eq!(resolve_to_cwd("a/./b/../c", cwd), PathBuf::from("/work/project/a/c"));
        assert_eq!(resolve_to_cwd("/abs/x", cwd), PathBuf::from("/abs/x"));
    }

    #[test]
    fn classify_launch_program_separates_files_dirs_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bin");
        std::fs::write(&file, "").unwrap();
        assert_eq!(classify_launch_program(&file), LaunchProgramKind::File);
        assert_eq!(
            classify_launch_program(dir.path()),
            LaunchProgramKind::Directory
        );
        assert_eq!(
            classify_launch_program(&dir.path().join("absent")),
            LaunchProgramKind::Missing
        );
    }

    #[test]
    fn launch_overrides_switch_dlv_mode() {
        let mut adapter = DapResolvedAdapter {
            name: "dlv".to_string(),
            ..DapResolvedAdapter::default()
        };
        assert_eq!(
            resolve_launch_overrides(&adapter, Path::new("/src"), LaunchProgramKind::Directory)
                .get("mode"),
            Some(&json!("debug"))
        );
        assert_eq!(
            resolve_launch_overrides(&adapter, Path::new("/src/main.go"), LaunchProgramKind::File)
                .get("mode"),
            Some(&json!("debug"))
        );
        assert_eq!(
            resolve_launch_overrides(&adapter, Path::new("/src/app"), LaunchProgramKind::File).get("mode"),
            Some(&json!("exec"))
        );
        assert!(resolve_launch_overrides(&adapter, Path::new("/src/app"), LaunchProgramKind::Missing)
            .is_empty());
        adapter.name = "gdb".to_string();
        assert!(
            resolve_launch_overrides(&adapter, Path::new("/src/app"), LaunchProgramKind::File).is_empty()
        );
    }

    #[test]
    fn runtime_table_maps_process_images() {
        assert_eq!(runtime_adapter_for("node"), Some("js-debug-adapter"));
        assert_eq!(runtime_adapter_for("node.exe"), Some("js-debug-adapter"));
        assert_eq!(runtime_adapter_for("python3.12"), Some("debugpy"));
        assert_eq!(runtime_adapter_for("python"), Some("debugpy"));
        assert_eq!(runtime_adapter_for("dotnet"), Some("netcoredbg"));
        assert_eq!(runtime_adapter_for("ruby"), Some("rdbg"));
        assert_eq!(runtime_adapter_for("php"), Some("php-debug-adapter"));
        assert_eq!(runtime_adapter_for("pythonista"), None);
        assert_eq!(runtime_adapter_for("myapp"), None);
    }

    #[test]
    fn extensionless_rank_prefers_gdb_then_lldb() {
        assert!(extensionless_rank("gdb") < extensionless_rank("lldb-dap"));
        assert_eq!(extensionless_rank("debugpy"), usize::MAX);
    }

    #[test]
    fn launch_selection_reports_none_for_unknown_adapter() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            select_launch_adapter(
                &dir.path().join("main.c"),
                dir.path(),
                Some("no-such-adapter"),
                LaunchProgramKind::File
            ),
            LaunchAdapterSelection::None
        );
    }

    #[test]
    fn launch_selection_reports_unavailable_for_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let selection = select_launch_adapter(
            &dir.path().join("main.c"),
            dir.path(),
            Some("elixir-ls-debugger"),
            LaunchProgramKind::File,
        );
        match selection {
            LaunchAdapterSelection::Unavailable {
                adapter_name,
                command,
            } => {
                assert_eq!(adapter_name, "elixir-ls-debugger");
                assert_eq!(command, "elixir-ls-debugger");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn launch_selection_picks_the_configured_adapter_when_it_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-gdb");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            dir.path().join("dap.json"),
            serde_json::to_string(&json!({
                "adapters": {"gdb": {"command": fake.to_string_lossy()}}
            }))
            .unwrap(),
        )
        .unwrap();
        invalidate_config(dir.path());
        let selection = select_launch_adapter(
            &dir.path().join("main.c"),
            dir.path(),
            Some("gdb"),
            LaunchProgramKind::File,
        );
        match selection {
            LaunchAdapterSelection::Adapter(adapter) => {
                assert_eq!(adapter.name, "gdb");
                assert_eq!(adapter.resolved_command, fake.to_string_lossy());
                assert_eq!(adapter.args, ["-i", "dap"]);
            }
            other => panic!("expected an adapter, got {other:?}"),
        }
        invalidate_config(dir.path());
    }

    #[test]
    fn js_debug_server_path_honours_the_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("dapDebugServer.js");
        std::fs::write(&server, "").unwrap();
        // SAFETY-equivalent: the crate forbids `unsafe`, and `set_var` is the
        // only route to exercise the env branch. Serialized by the test harness
        // because it is the only test touching this variable.
        std::env::set_var(JS_DEBUG_SERVER_ENV, &server);
        assert_eq!(resolve_js_debug_server_path(dir.path()), Some(server));
        std::env::remove_var(JS_DEBUG_SERVER_ENV);
    }
}
