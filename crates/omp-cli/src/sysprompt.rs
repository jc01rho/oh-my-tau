#![allow(dead_code, clippy::unnecessary_sort_by)]
//! System prompt assembly, ported from omp's `system-prompt.ts` +
//! `prompts/system/*.md`: base prompt selection (user/project `SYSTEM.md`
//! override, `--system-prompt`/`--append-system-prompt` flags), project
//! context discovery (`.omp/AGENTS.md`, `AGENTS.md`/`CLAUDE.md` walk-up,
//! Copilot, Gemini, `contextFiles.extra`), skills listing, the rule buckets,
//! the tool inventory section, and the workstation environment block.
//!
//! Rendering uses a small Handlebars-subset interpreter that covers exactly
//! the helpers omp's bundled templates use (`if`, `unless`, `each`, `list`,
//! `ifAny`, `has`, `when`, `includes`, `not`, `else`). The templates ship in
//! `omp-cli/prompts/` (copied verbatim from the TS distribution).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};

use crate::rules;

const CUSTOM_TEMPLATE: &str = include_str!("../prompts/custom-system-prompt.md");
const BUNDLED_TEMPLATE: &str = include_str!("../prompts/system-prompt.md");
const COMPACT_TEMPLATE: &str = include_str!("../prompts/system-prompt-compact.md");
const PROJECT_TEMPLATE: &str = include_str!("../prompts/project-prompt.md");
const USER_APPEND_TEMPLATE: &str = include_str!("../prompts/user-append.md");

/// User home directory.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// omp's agent dir (`getAgentDir()` for the default profile).
fn agent_dir() -> PathBuf {
    omp_settings::resolve_agent_dir().unwrap_or_else(|| home_dir().join(".omp/agent"))
}

/// Settings the prompt renders from.
#[allow(clippy::struct_excessive_bools)] // mirrors omp's PromptConfig shape; one struct beats six params
struct PromptConfig {
    skills_enabled: bool,
    include_model_in_prompt: bool,
    inline_tool_descriptors: bool,
    reactions: bool,
    /// `systemPrompt.compact`: render the compact bundled template.
    compact: bool,
}

/// Prompt-rendered settings via `omp-settings` (TS registry layering over the
/// agent dir's `config.yml`). The port predates omp-settings and defaults
/// `includeModelInPrompt` / `inlineToolDescriptors` / `reactions` to off where
/// the TS registry defaults them on, so those three keep their port defaults
/// unless the user explicitly set the key (`Provenance::Default` = unset).
fn load_config(agent: &Path) -> PromptConfig {
    // No project layer: the prompt historically reads the agent dir only.
    let project_none = agent.join(".no-project-layer");
    let settings =
        omp_settings::Settings::load_from(Some(&agent.join("config.yml")), &project_none);
    let set = |id: &str| {
        settings
            .provenance(id)
            .is_some_and(|p| p != omp_settings::Provenance::Default)
    };
    let mut cfg = PromptConfig {
        skills_enabled: true,
        include_model_in_prompt: false,
        inline_tool_descriptors: false,
        reactions: false,
        compact: settings.get_bool("systemPrompt.compact").unwrap_or(false),
    };
    if set("skills.enabled") {
        cfg.skills_enabled = settings.get_bool("skills.enabled").unwrap_or(true);
    }
    if set("includeModelInPrompt") {
        cfg.include_model_in_prompt = settings.get_bool("includeModelInPrompt").unwrap_or(false);
    }
    if set("inlineToolDescriptors") {
        cfg.inline_tool_descriptors = match settings.get("inlineToolDescriptors") {
            serde_json::Value::String(v) => v == "on",
            serde_json::Value::Bool(true) => true, // YAML 1.1 `on` unquoted
            _ => false,
        };
    }
    if set("tui.reactions") {
        cfg.reactions = settings.get_bool("tui.reactions").unwrap_or(false);
    }
    cfg
}

/// Workstation facts, matching `getEnvironmentInfo()` (Node `os.platform()`
///`os.release()`/`os.arch()`).
pub fn environment_info() -> Vec<(String, String)> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let os = if release.is_empty() {
        std::env::consts::OS.to_string()
    } else {
        format!("{} {release}", std::env::consts::OS)
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    vec![("OS".into(), os), ("Arch".into(), arch.into())]
}

/// Discover the git worktree root by walking up for a `.git` entry
/// (equivalent of the repo root the TS discovery passes as boundary).
fn git_root(cwd: &Path, _home: &Path) -> Option<PathBuf> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Walk-up boundary for context-file discovery, mirroring
/// `loadStandaloneContextFiles`: `(boundary, include_boundary, exclude_home)`.
fn scan_boundary(cwd: &Path, home: &Path) -> (PathBuf, bool, bool) {
    let cwd_under_home = cwd.starts_with(home);
    let repo_root = git_root(cwd, home);
    let repo_is_home = repo_root.as_deref() == Some(home);
    let repo_under_home = repo_root
        .as_deref()
        .is_some_and(|r| r.starts_with(home) && r != home);
    let scan_to_home = repo_root.is_some() && cwd_under_home && repo_under_home;
    let boundary = if scan_to_home {
        home.to_path_buf()
    } else {
        match repo_root.clone() {
            Some(r) => r,
            None if cwd_under_home => home.to_path_buf(),
            None => cwd.ancestors().last().unwrap_or(cwd).to_path_buf(),
        }
    };
    let include_boundary = match repo_root {
        None => cwd_under_home,
        Some(_) => boundary != home || repo_is_home,
    };
    (boundary, include_boundary, scan_to_home)
}

/// One discovered project context file, ordered farthest-from-cwd first per
/// the TS depth-descending sort (`ContextFile`).
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
    pub depth: usize,
    /// `user` for user-scope files, `project` otherwise.
    pub level: Level,
    /// Additive files (`contextFiles.extra`) load beside a same-depth file
    /// instead of claiming its slot.
    pub additive: bool,
}

/// Which scope a context file came from (`SourceMeta.level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    User,
    Project,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::User => "user",
            Level::Project => "project",
        }
    }
}

/// `calculateDepth`: directory levels from `cwd` up to `dir` (0 = `cwd`).
fn calculate_depth(cwd: &Path, dir: &Path) -> usize {
    cwd.components()
        .count()
        .saturating_sub(dir.components().count())
}

/// `resolveCopilotHome`: `COPILOT_HOME` override, else `<home>/.copilot`.
fn copilot_home(home: &Path) -> PathBuf {
    match std::env::var("COPILOT_HOME") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => home.join(".copilot"),
    }
}

/// Directories from `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` (path-list separated by
/// the platform separator, as Copilot CLI documents).
fn copilot_custom_instruction_dirs() -> Vec<PathBuf> {
    let Ok(raw) = std::env::var("COPILOT_CUSTOM_INSTRUCTIONS_DIRS") else {
        return Vec::new();
    };
    std::env::split_paths(&raw)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// `loadStandaloneContextFiles` for `AGENTS.md` then `CLAUDE.md`: at each
/// directory from `cwd` up to the boundary, the first non-empty file claims
/// that depth scope. Hidden directories (`.foo/AGENTS.md`) are skipped.
pub fn standalone_context_files(cwd: &Path, home: &Path) -> Vec<ContextFile> {
    let (boundary, include_boundary, exclude_home) = scan_boundary(cwd, home);
    let mut out: Vec<ContextFile> = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        let at_boundary = current == boundary;
        let at_home = exclude_home && current == home;
        if !at_home && (!at_boundary || include_boundary) {
            for name in ["AGENTS.md", "CLAUDE.md"] {
                let candidate = current.join(name);
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    if !content.is_empty()
                        && !current
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                    {
                        out.push(ContextFile {
                            path: candidate,
                            content,
                            depth: calculate_depth(cwd, &current),
                            level: Level::Project,
                            additive: false,
                        });
                        break; // one file per depth scope
                    }
                }
            }
        }
        if at_boundary {
            break;
        }
        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }
    out
}

/// Push a context file unless an earlier, more authoritative provider already
/// claimed its dedupe key (`contextFileCapability` first-wins semantics).
fn push_scoped(out: &mut Vec<ContextFile>, claimed: &mut HashSet<String>, file: ContextFile) {
    let key = if file.additive {
        format!("additive:{}", file.path.display())
    } else {
        match file.level {
            Level::User => "user".to_string(),
            Level::Project => format!("project:{}", file.depth),
        }
    };
    if claimed.insert(key) {
        out.push(file);
    }
}

/// Read `path` as a context file, if it exists and is non-empty.
///
/// `scope_dir` is the directory whose depth the file competes at: a file inside
/// a config subdirectory (`.omp/`, `.github/`, `.gemini/`) is same-scope as the
/// project directory holding it, per the capability's dedupe key
/// ("files inside config subdirectories of an ancestor … are same-scope as the
/// ancestor itself").
fn read_context_file(
    path: &Path,
    scope_dir: &Path,
    cwd: &Path,
    level: Level,
    additive: bool,
) -> Option<ContextFile> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    Some(ContextFile {
        path: path.to_path_buf(),
        content,
        depth: calculate_depth(cwd, scope_dir),
        level,
        additive,
    })
}

/// Every context file for `cwd`, from every provider, in capability priority
/// order: `.omp/`-native (priority 100), standalone `AGENTS.md`/`CLAUDE.md`
/// (90), GitHub Copilot (30), Gemini CLI (60), then additive
/// `contextFiles.extra` (9).
///
/// Content is `@path`-expanded and the result is containment-deduped, matching
/// `loadProjectContextFiles`.
#[must_use]
pub fn context_files(cwd: &Path, home: &Path) -> Vec<ContextFile> {
    context_files_with(cwd, home, &agent_dir(), &[])
}

/// [`context_files`] with an explicit agent dir and `contextFiles.extra` names.
#[must_use]
pub fn context_files_with(
    cwd: &Path,
    home: &Path,
    agent: &Path,
    extra_names: &[String],
) -> Vec<ContextFile> {
    let mut out: Vec<ContextFile> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    // 1. `.omp/`-native: user `<agent>/AGENTS.md`, then the nearest project
    //    `.omp/AGENTS.md` (`discovery/builtin.ts`).
    if let Some(file) = read_context_file(&agent.join("AGENTS.md"), agent, cwd, Level::User, false)
    {
        push_scoped(&mut out, &mut claimed, file);
    }
    if let Some(config_dir) = nearest_project_config_dir(cwd, home) {
        let scope = config_dir.parent().unwrap_or(cwd);
        if let Some(file) = read_context_file(
            &config_dir.join("AGENTS.md"),
            scope,
            cwd,
            Level::Project,
            false,
        ) {
            push_scoped(&mut out, &mut claimed, file);
        }
    }

    // 2. Standalone walk-up providers.
    for file in standalone_context_files(cwd, home) {
        push_scoped(&mut out, &mut claimed, file);
    }

    // 3. GitHub Copilot: project `.github/copilot-instructions.md`, user
    //    `~/.copilot/copilot-instructions.md`, then each
    //    `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` entry's `AGENTS.md`.
    if let Some(file) = read_context_file(
        &cwd.join(".github/copilot-instructions.md"),
        cwd,
        cwd,
        Level::Project,
        false,
    ) {
        push_scoped(&mut out, &mut claimed, file);
    }
    if let Some(file) = read_context_file(
        &copilot_home(home).join("copilot-instructions.md"),
        home,
        cwd,
        Level::User,
        false,
    ) {
        push_scoped(&mut out, &mut claimed, file);
    }
    for dir in copilot_custom_instruction_dirs() {
        if let Some(file) = read_context_file(&dir.join("AGENTS.md"), &dir, cwd, Level::User, false)
        {
            push_scoped(&mut out, &mut claimed, file);
        }
    }

    // 4. Gemini CLI: project `.gemini/GEMINI.md`, user `~/.gemini/GEMINI.md`.
    if let Some(file) = read_context_file(
        &cwd.join(".gemini/GEMINI.md"),
        cwd,
        cwd,
        Level::Project,
        false,
    ) {
        push_scoped(&mut out, &mut claimed, file);
    }
    if let Some(file) = read_context_file(
        &home.join(".gemini/GEMINI.md"),
        home,
        cwd,
        Level::User,
        false,
    ) {
        push_scoped(&mut out, &mut claimed, file);
    }

    // 5. `contextFiles.extra`: additive names, never taking another file's slot.
    for name in extra_names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        for file in standalone_named_context_files(cwd, home, name) {
            push_scoped(&mut out, &mut claimed, file);
        }
        if let Some(file) = read_context_file(&agent.join(name), agent, cwd, Level::User, true) {
            push_scoped(&mut out, &mut claimed, file);
        }
    }

    // `@path` imports expand against the importing file's own directory.
    for file in &mut out {
        file.content = expand_at_imports(&file.content, &file.path, home);
    }

    // Depth-descending: farther from cwd first, closer last (more prominent).
    out.sort_by_key(|f| std::cmp::Reverse(f.depth));
    dedupe_contained_context_files(out)
}

/// Nearest ancestor whose `.omp` directory exists, never the home directory
/// (the user config root is not a project).
fn nearest_project_config_dir(cwd: &Path, home: &Path) -> Option<PathBuf> {
    let root = git_root(cwd, home);
    for dir in cwd.ancestors() {
        if dir == home {
            break;
        }
        let config_dir = dir.join(".omp");
        if config_dir.is_dir() {
            return Some(config_dir);
        }
        if root.as_deref() == Some(dir) {
            break;
        }
    }
    None
}

/// Walk-up discovery for one extra filename (`loadStandaloneContextFiles`
/// reused by `contextFiles.extra`), marking every hit additive.
fn standalone_named_context_files(cwd: &Path, home: &Path, name: &str) -> Vec<ContextFile> {
    let (boundary, include_boundary, exclude_home) = scan_boundary(cwd, home);
    let mut out = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        let at_boundary = current == boundary;
        let at_home = exclude_home && current == home;
        if !at_home && (!at_boundary || include_boundary) {
            if let Some(file) =
                read_context_file(&current.join(name), &current, cwd, Level::Project, true)
            {
                out.push(file);
            }
        }
        if at_boundary {
            break;
        }
        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Context-file content transforms (`at-imports.ts`, `system-prompt.ts`)
// ---------------------------------------------------------------------------

/// Maximum recursive `@`-import hops (Claude Code's documented cap).
pub const MAX_AT_IMPORT_DEPTH: usize = 5;

/// Maximum expanded imports in one top-level document.
pub const MAX_AT_IMPORT_EXPANSIONS: usize = 256;

/// Expand `@path/to/file` includes in `content`, resolving relative paths
/// against `file_path`'s directory (`expandAtImports`).
///
/// `@` must sit at the start of a line or after whitespace, so `git@github.com`
/// survives; imports inside fenced blocks and inline code spans stay verbatim;
/// `\ ` is a literal space; `~/` resolves to `home`; an unreadable target leaves
/// the original token untouched; cycles in the current chain are broken.
#[must_use]
pub fn expand_at_imports(content: &str, file_path: &Path, home: &Path) -> String {
    let base = file_path.parent().unwrap_or(Path::new("."));
    let chain = vec![file_path.to_path_buf()];
    let mut budget = MAX_AT_IMPORT_EXPANSIONS;
    expand_at_depth(content, base, 0, home, &chain, &mut budget)
}

fn expand_at_depth(
    content: &str,
    base_dir: &Path,
    depth: usize,
    home: &Path,
    chain: &[PathBuf],
    budget: &mut usize,
) -> String {
    if depth >= MAX_AT_IMPORT_DEPTH {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    let mut in_fence = false;
    for line in content.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if fence {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        let expanded = expand_line(line, base_dir, depth, home, chain, budget);
        out.push_str(&expanded);
    }
    out
}

/// Expand the `@`-imports of one line, leaving inline code spans alone.
fn expand_line(
    line: &str,
    base_dir: &Path,
    depth: usize,
    home: &Path,
    chain: &[PathBuf],
    budget: &mut usize,
) -> String {
    if !line.contains('@') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_inline_code = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '`' {
            in_inline_code = !in_inline_code;
            out.push(c);
            i += 1;
            continue;
        }
        let boundary = i == 0 || matches!(c_prev(bytes, i), Some(' ') | Some('\t'));
        if c == '@' && boundary && !in_inline_code {
            let rest = &line[i + 1..];
            let mut token_len = 0;
            let mut chars = rest.char_indices().peekable();
            while let Some((idx, ch)) = chars.next() {
                match ch {
                    // `\ ` is an escaped space inside the path.
                    '\\' if chars.peek().is_some_and(|(_, next)| *next == ' ') => {
                        token_len = idx + 2;
                        chars.next();
                    }
                    ch if ch.is_whitespace() => break,
                    _ => token_len = idx + ch.len_utf8(),
                }
            }
            let raw = &rest[..token_len];
            let token =
                raw.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
            let path_like = token.starts_with('/')
                || token.starts_with('.')
                || token.starts_with('~')
                || token
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric());
            if !token.is_empty() && path_like {
                if let Some(expanded) =
                    resolve_at_import(token, base_dir, depth, home, chain, budget)
                {
                    out.push_str(&expanded);
                    i += 1 + token.len();
                    continue;
                }
            }
        }
        // Copy one whole char (the byte index may sit mid-codepoint).
        let ch_len = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn c_prev(bytes: &[u8], i: usize) -> Option<char> {
    (i > 0).then(|| bytes[i - 1] as char)
}

/// Resolve and recursively expand one import; `None` keeps the token verbatim.
fn resolve_at_import(
    token: &str,
    base_dir: &Path,
    depth: usize,
    home: &Path,
    chain: &[PathBuf],
    budget: &mut usize,
) -> Option<String> {
    if *budget == 0 {
        return None;
    }
    let unescaped = token.replace("\\ ", " ");
    let resolved = if unescaped == "~" {
        home.to_path_buf()
    } else if let Some(rest) = unescaped.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(&unescaped).is_absolute() {
        PathBuf::from(&unescaped)
    } else {
        base_dir.join(&unescaped)
    };
    // Only an ancestor in the current chain is a cycle; a file imported twice
    // from unrelated places expands twice.
    if chain.contains(&resolved) {
        return None;
    }
    let content = std::fs::read_to_string(&resolved).ok()?;
    *budget -= 1;
    let mut next_chain = chain.to_vec();
    next_chain.push(resolved.clone());
    let next_base = resolved.parent().unwrap_or(base_dir).to_path_buf();
    Some(expand_at_depth(
        &content,
        &next_base,
        depth + 1,
        home,
        &next_chain,
        budget,
    ))
}

/// `prompt.format(content, {renderPhase: "post-render"}).trim()`.
fn normalize_prompt_block(content: &str) -> String {
    format_post_render(content).trim().to_string()
}

/// Split content into comparable paragraphs, keeping fenced blocks intact
/// (`splitComparablePromptBlocks`).
fn split_comparable_prompt_blocks(content: &str) -> Vec<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let rendered = normalize_prompt_block(trimmed);
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in rendered.split('\n') {
        let fence = {
            let t = line.trim_start();
            t.starts_with("```") || t.starts_with("~~~")
        };
        if fence {
            in_fence = !in_fence;
            current.push(line);
            continue;
        }
        let ends_block = !in_fence
            && line.trim().is_empty()
            && !current.is_empty()
            && current.last().is_some_and(|last| !last.trim().is_empty());
        if ends_block {
            let block = current.join("\n");
            let block = block.trim();
            if !block.is_empty() {
                blocks.push(block.to_string());
            }
            current.clear();
            continue;
        }
        current.push(line);
    }
    let tail = current.join("\n");
    let tail = tail.trim();
    if !tail.is_empty() {
        blocks.push(tail.to_string());
    }
    blocks
}

/// Whether `rule_blocks` is a contiguous run inside `source_blocks`.
fn prompt_blocks_contain(source_blocks: &[String], rule_blocks: &[String]) -> bool {
    if source_blocks.is_empty() || rule_blocks.is_empty() || rule_blocks.len() > source_blocks.len()
    {
        return false;
    }
    (0..=source_blocks.len() - rule_blocks.len()).any(|start| {
        rule_blocks
            .iter()
            .enumerate()
            .all(|(offset, block)| &source_blocks[start + offset] == block)
    })
}

/// Drop a context file whose whole paragraph sequence already appears in a
/// closer-to-cwd file (`dedupeContainedContextFiles`).
///
/// Containment is exact after normalization, not fuzzy: paraphrased or
/// interleaved content is kept.
#[must_use]
pub fn dedupe_contained_context_files(mut files: Vec<ContextFile>) -> Vec<ContextFile> {
    // Depth-descending so a file is only omitted when a more authoritative
    // (closer-to-cwd) file contains it. Stable, so equal depths keep caller order.
    files.sort_by(|a, b| b.depth.cmp(&a.depth));
    let blocks: Vec<Vec<String>> = files
        .iter()
        .map(|file| split_comparable_prompt_blocks(&file.content))
        .collect();
    let mut kept = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        let contained = blocks
            .iter()
            .enumerate()
            .any(|(candidate, candidate_blocks)| {
                candidate > index && prompt_blocks_contain(candidate_blocks, &blocks[index])
            });
        if !contained {
            kept.push(file);
        }
    }
    kept
}

/// Drop context files named `AGENTS.md` (any case): task subagents never
/// receive them (`withoutAgentsMd`).
#[must_use]
pub fn without_agents_md(files: Vec<ContextFile>) -> Vec<ContextFile> {
    files
        .into_iter()
        .filter(|file| {
            !file
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().to_lowercase() == "agents.md")
        })
        .collect()
}

/// Close a code fence a context file leaves open, so it never leaves the rest
/// of the prompt unformatted (`prompt.closeCodeFence`).
#[must_use]
pub fn close_code_fence(content: &str) -> String {
    let mut fence: Option<(char, usize)> = None;
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        let Some(marker) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) else {
            continue;
        };
        let len = trimmed.chars().take_while(|c| *c == marker).count();
        if len < 3 {
            continue;
        }
        match fence {
            None => fence = Some((marker, len)),
            Some((open, open_len)) if open == marker && len >= open_len => fence = None,
            Some(_) => {}
        }
    }
    match fence {
        Some((marker, len)) => {
            let mut out = content.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&marker.to_string().repeat(len));
            out
        }
        None => content.to_string(),
    }
}

/// A discovered system prompt override (SYSTEM.md text or `SYSTEM_TEMPLATE.md`
/// template), project scope beating user scope as in
/// `discoverSystemPromptOverride`.
#[derive(Debug)]
pub enum Override {
    Text(String),
    Template(String),
}

/// Walk from `cwd` up to the git root (or filesystem root) for SYSTEM.md /
/// `SYSTEM_TEMPLATE.md` (`getProjectPathCandidates` analog), falling back to the
/// user-level files in the agent dir.
pub fn discover_override(cwd: &Path, agent: &Path) -> Option<Override> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    if let Some(root) = git_root(cwd, &home_dir()) {
        if !dirs.contains(&root) {
            dirs.push(root.clone());
        }
        dirs.retain(|d| root.starts_with(d) || d.starts_with(&root));
    }
    for dir in dirs {
        for (name, template) in [("SYSTEM.md", false), ("SYSTEM_TEMPLATE.md", true)] {
            if let Ok(content) = std::fs::read_to_string(dir.join(name)) {
                if content.is_empty() {
                    continue;
                }
                if template {
                    if content.trim().is_empty() {
                        continue;
                    }
                    return Some(Override::Template(content));
                }
                return Some(Override::Text(content));
            }
        }
    }
    for (name, template) in [("SYSTEM.md", false), ("SYSTEM_TEMPLATE.md", true)] {
        if let Ok(content) = std::fs::read_to_string(agent.join(name)) {
            if content.is_empty() {
                continue;
            }
            if template {
                if content.trim().is_empty() {
                    continue;
                }
                return Some(Override::Template(content));
            }
            return Some(Override::Text(content));
        }
    }
    None
}

/// A skill entry as rendered into the prompt (`name` + `description`).
pub struct Skill {
    pub name: String,
    pub description: String,
    pub hide: bool,
}

/// Skill paths extensions contributed through `resources_discover` (absolute, discovery order).
/// Each entry is a skill directory (`SKILL.md` inside) or a skills root whose immediate children
/// hold `SKILL.md`, the same shapes `.omp/skills` accepts.
static EXTENSION_SKILL_PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Replace the extension-contributed skill paths (a reload re-sends the whole set).
pub fn set_extension_skill_paths(paths: Vec<PathBuf>) {
    *EXTENSION_SKILL_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = paths;
}

/// The extension-contributed skill paths currently registered.
#[must_use]
pub fn extension_skill_paths() -> Vec<PathBuf> {
    EXTENSION_SKILL_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Test-only: these registries (the extension skill paths and the `--skill` roots) are process-global,
/// so a test that writes them must not overlap one that reads them through [`load_skills`]/[`crate::rpc::slash::load_file_commands`]. Sync tests
/// take it with `blocking_lock()`, async ones with `lock().await` (a std guard would be held across
/// an await point).
#[cfg(test)]
pub(crate) fn extension_registry_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
}

/// Record the `--skill` roots shared with [`load_skills`] and `omp_urls` `skill://` resolution.
pub fn set_skill_paths(paths: Vec<PathBuf>) {
    omp_urls::schemes::skill::set_extra_skill_paths(paths);
}

/// The options the session's base prompt was assembled from (kept by agent
/// construction so `/reload-plugins` can reassemble it with fresh skills).
static SESSION_OPTIONS: std::sync::Mutex<Option<Options>> = std::sync::Mutex::new(None);

/// Remember the session's prompt options.
pub fn remember_session_options(opts: &Options) {
    *SESSION_OPTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(opts.clone());
}

/// The session's prompt options, once the agent was built.
pub fn session_options() -> Option<Options> {
    SESSION_OPTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The skills the prompt for `cwd` lists now (`skills.enabled` honoured).
pub fn visible_skills(cwd: &Path) -> Vec<Skill> {
    let enabled = omp_settings::Settings::load(cwd)
        .get_bool("skills.enabled")
        .unwrap_or(true);
    load_skills(&agent_dir(), cwd, enabled)
}

/// All visible skills for the prompt (TS: `loadSkills` from the agent dir's
/// `skills/` + project `.omp/skills` + `--skill` roots, `hide: true` and
/// `skills.disabled` dropped, closest priority winning on duplicate names).
pub fn load_skills(agent: &Path, cwd: &Path, enabled: bool) -> Vec<Skill> {
    load_skills_with(agent, cwd, enabled, &extension_skill_paths())
}

/// [`load_skills`] with explicit extension-contributed paths (the registry, or a test's own list).
pub fn load_skills_with(
    agent: &Path,
    cwd: &Path,
    enabled: bool,
    extension_paths: &[PathBuf],
) -> Vec<Skill> {
    if !enabled {
        return Vec::new();
    }
    // Extension-contributed paths come after project/user skills (a same-named project or user skill
    // wins) and before the `--skill` roots, so a gateway-provided root cannot shadow either.
    let mut roots = extension_paths.to_vec();
    roots.extend(omp_urls::schemes::skill::extra_skill_paths());
    let disabled = settings_string_array(agent, "skills.disabled");
    let mut seen = HashSet::new();
    omp_urls::schemes::skill::discover_skills_with(agent, cwd, &roots)
        .into_iter()
        .filter(|s| seen.insert(s.name.clone()) && !disabled.contains(&s.name))
        .map(|s| Skill {
            name: s.name,
            description: s.description,
            hide: false,
        })
        .collect()
}

/// `resolvePromptInput`: multi-line input is literal; otherwise try reading
/// the value as a file, falling back to the literal string.
pub fn resolve_prompt_input(input: &str) -> String {
    if input.contains('\n') {
        return input.to_string();
    }
    match std::fs::read_to_string(input) {
        Ok(content) => content,
        Err(_) => input.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Handlebars-subset renderer for omp's bundled templates.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Arg {
    Str(String),
    Num(f64),
    Path(String),
    Call(String, Vec<Arg>),
}

#[derive(Debug)]
enum Node {
    Text(String),
    Var(Arg),
    Block {
        helper: String,
        args: Vec<Arg>,
        hash: Vec<(String, String)>,
        body: Vec<Node>,
        else_body: Vec<Node>,
    },
}

fn tokenize(template: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let (before, tag_start) = rest.split_at(start);
        if !before.is_empty() {
            tags.push(format!("\0{before}"));
        }
        if let Some(end) = tag_start[2..].find("}}") {
            tags.push(tag_start[..end + 4].to_string());
            rest = &tag_start[end + 4..];
        } else {
            tags.push(tag_start.to_string());
            rest = "";
        }
    }
    if !rest.is_empty() {
        tags.push(format!("\0{rest}"));
    }
    tags
}

/// Split on whitespace outside quotes, keeping quoted segments whole.
fn split_tokens(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in rest.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Interpret the escape sequences template authors write inside `join=`/`prefix=`.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_arg(tok: &str) -> Arg {
    let t = tok.trim();
    if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.split_whitespace();
        let name = parts.next().unwrap_or("").to_string();
        return Arg::Call(name, parts.map(parse_arg).collect());
    }
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        return Arg::Str(t[1..t.len() - 1].to_string());
    }
    if let Ok(n) = t.parse::<f64>() {
        return Arg::Num(n);
    }
    Arg::Path(t.to_string())
}

fn parse_nodes(tags: &[String], pos: &mut usize) -> (Vec<Node>, Option<String>) {
    let mut nodes = Vec::new();
    while *pos < tags.len() {
        let tag = &tags[*pos];
        if let Some(text) = tag.strip_prefix('\0') {
            nodes.push(Node::Text(text.to_string()));
            *pos += 1;
            continue;
        }
        let inner = tag.trim_start_matches("{{").trim_end_matches("}}").trim();
        if let Some(close) = inner.strip_prefix('/') {
            return (nodes, Some(close.trim().to_string()));
        }
        if inner == "else" {
            return (nodes, Some("else".into()));
        }
        if let Some(rest) = inner.strip_prefix('#') {
            let toks = split_tokens(rest);
            let helper = toks.first().map_or("", String::as_str).to_string();
            let mut args = Vec::new();
            let mut hash = Vec::new();
            for t in &toks[1..] {
                // Quoted tokens may contain '=' (comparison operators like
                // "=="); only bare `key=value` tokens are hashes.
                let quoted = t.starts_with('"') || t.starts_with('\'');
                if !quoted && !t.starts_with('(') {
                    if let Some((k, v)) = t.split_once('=') {
                        hash.push((k.to_string(), unescape(v.trim_matches('"'))));
                        continue;
                    }
                }
                args.push(parse_arg(t));
            }
            *pos += 1;
            let (body, term) = parse_nodes(tags, pos);
            let mut else_body = Vec::new();
            match term.as_deref() {
                Some("else") => {
                    // Consume the {{else}} tag, then parse the inverse branch.
                    *pos += 1;
                    let (eb, t2) = parse_nodes(tags, pos);
                    else_body = eb;
                    if t2.is_some() {
                        // Consume the {{/helper}} close tag.
                        *pos += 1;
                    }
                }
                Some(name) => {
                    debug_assert_eq!(name, helper, "mismatched block close: {name} vs {helper}");
                    // Consume the {{/helper}} close tag.
                    *pos += 1;
                }
                None => {}
            }
            nodes.push(Node::Block {
                helper,
                args,
                hash,
                body,
                else_body,
            });
            continue;
        }
        // Inline variable or inline helper (subexpressions are handled by parse_arg).
        nodes.push(Node::Var(parse_arg(inner)));
        *pos += 1;
    }
    (nodes, None)
}

fn lookup(ctx: &[&Value], path: &str) -> Value {
    if path == "this" || path == "." || path.is_empty() {
        return ctx.last().copied().cloned().unwrap_or(Value::Null);
    }
    // Handlebars loop metadata inside `each`: `@first` / `@last`.
    if path == "@first" || path == "@last" {
        return LOOP_FLAGS.with(|flags| {
            flags
                .borrow()
                .last()
                .map_or(Value::Bool(false), |(first, last)| {
                    Value::Bool(if path == "@first" { *first } else { *last })
                })
        });
    }
    // `a.length` on arrays.
    if let Some((base, "length")) = path.rsplit_once('.') {
        if let Value::Array(a) = lookup(ctx, base) {
            return json!(a.len());
        }
    }
    for frame in ctx.iter().rev() {
        let mut cur = *frame;
        let mut ok = true;
        for part in path.split('.') {
            if let Value::Object(m) = cur {
                if let Some(v) = m.get(part) {
                    cur = v;
                } else {
                    ok = false;
                    break;
                }
            } else {
                ok = false;
                break;
            }
        }
        if ok {
            return cur.clone();
        }
    }
    Value::Null
}

thread_local! {
    /// `@first`/`@last` for the innermost active `each`/`list` iteration.
    static LOOP_FLAGS: std::cell::RefCell<Vec<(bool, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    }
}

fn eval_arg(arg: &Arg, ctx: &[&Value]) -> Value {
    match arg {
        Arg::Str(s) => Value::String(s.clone()),
        Arg::Num(n) => json!(n),
        Arg::Path(p) => lookup(ctx, p),
        Arg::Call(name, args) => eval_helper_value(name, args, ctx),
    }
}

fn eval_helper_value(name: &str, args: &[Arg], ctx: &[&Value]) -> Value {
    match name {
        "includes" | "has" => {
            let hay = eval_arg(&args[0], ctx);
            let needle = eval_arg(&args[1], ctx);
            Value::Bool(hay.as_array().is_some_and(|a| a.contains(&needle)))
        }
        "not" => Value::Bool(!truthy(&eval_arg(&args[0], ctx))),
        _ => Value::Null,
    }
}

fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_lines)] // template node walker; one loop per node kind, splitting adds indirection
fn render_nodes(nodes: &[Node], ctx: &mut Vec<&Value>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Var(arg) => {
                let v = eval_arg(arg, ctx);
                out.push_str(&value_string(&v));
            }
            Node::Block {
                helper,
                args,
                hash,
                body,
                else_body,
            } => match helper.as_str() {
                "if" => {
                    let cond = truthy(&eval_arg(&args[0], ctx));
                    render_nodes(if cond { body } else { else_body }, ctx, out);
                }
                "unless" => {
                    let cond = truthy(&eval_arg(&args[0], ctx));
                    render_nodes(if cond { else_body } else { body }, ctx, out);
                }
                "ifAny" => {
                    let any = args.iter().any(|a| truthy(&eval_arg(a, ctx)));
                    render_nodes(if any { body } else { else_body }, ctx, out);
                }
                "has" => {
                    let hay = eval_arg(&args[0], ctx);
                    let needle = eval_arg(&args[1], ctx);
                    let hit = hay.as_array().is_some_and(|a| a.contains(&needle));
                    render_nodes(if hit { body } else { else_body }, ctx, out);
                }
                "when" => {
                    // Guard malformed comparisons (TS renders the else branch).
                    if args.len() < 3 {
                        render_nodes(else_body, ctx, out);
                        continue;
                    }
                    let l = eval_arg(&args[0], ctx);
                    let op = eval_arg(&args[1], ctx);
                    let r = eval_arg(&args[2], ctx);
                    let lnum = l.as_f64().or_else(|| l.as_str()?.parse().ok());
                    let rnum = r.as_f64().or_else(|| r.as_str()?.parse().ok());
                    let eq = value_string(&l) == value_string(&r);
                    let cmp = match (lnum, rnum) {
                        (Some(a), Some(b)) => Some(a.partial_cmp(&b)),
                        _ => None,
                    };
                    let op = value_string(&op);
                    let pass = match op.as_str() {
                        "==" => eq,
                        "!=" => !eq,
                        ">" => cmp.is_some_and(|c| c == Some(std::cmp::Ordering::Greater)),
                        "<" => cmp.is_some_and(|c| c == Some(std::cmp::Ordering::Less)),
                        ">=" => cmp.is_some_and(|c| c != Some(std::cmp::Ordering::Less)),
                        "<=" => cmp.is_some_and(|c| c != Some(std::cmp::Ordering::Greater)),
                        _ => false,
                    };
                    render_nodes(if pass { body } else { else_body }, ctx, out);
                }
                "each" | "list" => {
                    let items = eval_arg(&args[0], ctx);
                    let Value::Array(items) = items else {
                        render_nodes(else_body, ctx, out);
                        continue;
                    };
                    if items.is_empty() {
                        render_nodes(else_body, ctx, out);
                        continue;
                    }
                    let join = hash
                        .iter()
                        .find(|(k, _)| k == "join")
                        .map_or("", |(_, v)| v.as_str());
                    let prefix = hash
                        .iter()
                        .find(|(k, _)| k == "prefix")
                        .map_or("", |(_, v)| v.as_str());
                    let rendered: Vec<String> = items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let mut s = String::new();
                            let mut sub: Vec<&Value> = ctx.clone();
                            sub.push(item);
                            let flags = (index == 0, index + 1 == items.len());
                            LOOP_FLAGS.with(|f| f.borrow_mut().push(flags));
                            render_nodes(body, &mut sub, &mut s);
                            LOOP_FLAGS.with(|f| {
                                f.borrow_mut().pop();
                            });
                            s
                        })
                        .collect();
                    if helper == "list" {
                        for (i, part) in rendered.iter().enumerate() {
                            if i > 0 {
                                out.push_str(join);
                            }
                            out.push_str(prefix);
                            out.push_str(part);
                        }
                    } else {
                        out.push_str(&rendered.concat());
                    }
                }
                other => {
                    // Unknown block helper: render body as-is (best effort).
                    let _ = other;
                    render_nodes(body, ctx, out);
                }
            },
        }
    }
}

const BLOCK_HELPERS: &[&str] = &[
    "if", "unless", "each", "list", "ifAny", "has", "when", "with",
];

/// Handlebars "standalone" semantics (mirrors `stripStandalone` in
/// pi-utils/template.ts): a line consisting solely of one block/else/close
/// tag plus whitespace is replaced by the trimmed tag alone — the leading
/// whitespace and the trailing newline disappear, the tag stays.
fn strip_standalone(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    for line in template.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim();
        let decor = trimmed
            .strip_prefix("{{")
            .and_then(|l| l.strip_suffix("}}"))
            .filter(|inner| !inner.contains("{{") && !inner.contains("}}"))
            .is_some_and(|inner| {
                let name = inner
                    .trim_start_matches('#')
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                inner.trim() == "else" || BLOCK_HELPERS.contains(&name)
            });
        if decor {
            out.push_str(trimmed); // tag kept; line newline dropped
        } else {
            out.push_str(line);
        }
    }
    out
}

/// `prompt.format` with `renderPhase: "post-render"`: trimEnd lines, collapse
/// runs of blank lines (keep at most one, and only between content), pop
/// blank lines before closing XML tags, strip trailing blanks. Code fences
/// (`` ``` `` / `` ~~~ ``) disable blank handling inside them.
fn format_post_render(content: &str) -> String {
    fn is_closing_tag_line(line: &str) -> bool {
        let Some(rest) = line.trim_start().strip_prefix("</") else {
            return false;
        };
        let name_len = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-' || *c == '_')
            .count();
        name_len > 0 && rest[name_len..] == *">"
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_end();
        let indentless = line.trim_start();
        if indentless.starts_with("```") || indentless.starts_with("~~~") {
            in_code = !in_code;
            result.push(line.to_string());
            i += 1;
            continue;
        }
        if in_code {
            result.push(line.to_string());
            i += 1;
            continue;
        }
        if line.trim().is_empty() {
            let next_blank = i + 1 >= lines.len() || lines[i + 1].trim().is_empty();
            if next_blank {
                // Run of 2+ blanks (or trailing): strip entirely.
                while result.last().is_some_and(std::string::String::is_empty) {
                    result.pop();
                }
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                i = j;
                continue;
            }
            if result.last().is_none_or(std::string::String::is_empty) {
                i += 1;
                continue;
            }
            result.push(String::new());
            i += 1;
            continue;
        }
        if is_closing_tag_line(line) {
            while result.last().is_some_and(std::string::String::is_empty) {
                result.pop();
            }
        }
        result.push(line.to_string());
        i += 1;
    }
    while result.last().is_some_and(std::string::String::is_empty) {
        result.pop();
    }
    result.join("\n")
}

/// Render a template against a JSON data object: Handlebars-subset render
/// followed by the `prompt.format` post-render pass, matching
/// `prompt.render(template, context)`.
pub fn render(template: &str, data: &Value) -> String {
    let template = strip_standalone(template);
    let tags = tokenize(&template);
    let mut pos = 0;
    let (nodes, _) = parse_nodes(&tags, &mut pos);
    let mut out = String::new();
    let mut ctx: Vec<&Value> = vec![data];
    render_nodes(&nodes, &mut ctx, &mut out);
    format_post_render(&out)
}

// ---------------------------------------------------------------------------
// System prompt assembly (`buildSystemPrompt`).
// ---------------------------------------------------------------------------

/// Inputs the CLI gathers for the system prompt.
#[derive(Default, Clone)]
pub struct Options {
    /// `--system-prompt <path|text>` (replaces the default prompt).
    pub system_prompt: Option<String>,
    /// `--append-system-prompt <path|text>` (appended in the project block).
    /// Repeatable: every value lands in argv order.
    pub append_system_prompt: Vec<String>,
    /// `--tools <csv>`: restrict enabled tools (omp semantics).
    pub tools_csv: Option<String>,
    /// Prompt is for a subagent session.
    pub subagent: bool,
    /// `--compact-prompt`: select the compact bundled template.
    pub compact_prompt: bool,
    /// Active model identifier (surfaced only when the config enables it).
    pub model: String,
    /// Generated append blocks (e.g. MCP server instructions), joined like
    /// `composeAppendPrompt`.
    pub generated_append: Vec<String>,
    pub prompt: Option<String>,
    /// Workspace directories beyond `cwd` (TS `additionalWorkspaceRoots`:
    /// `--add-dir`, `workspace.additionalDirectories`, `/add-dir`): listed in
    /// `<workspace-roots>` and searched for context files like `cwd`.
    pub additional_workspace_roots: Vec<String>,
}

pub const ULTRAWORK_DIRECTIVE_OPEN_TAG: &str = "<omo-ultrawork-directive>";
pub const ULTRAWORK_DIRECTIVE_CLOSE_TAG: &str = "</omo-ultrawork-directive>";
pub const ULTRAWORK_BOOTSTRAP: &str = concat!(
    "<omo-ultrawork-directive>\n",
    "# Ultrawork Execution Contract\n\n",
    "Binding ultrawork mode directive. When the `ultrawork` skill (`skill://ultrawork` or `skills/ultrawork/SKILL.md`) is available, read the whole file and follow every rule in it for the rest of the task.\n\n",
    "- **Real Surface Verification**: Validate changes directly through their genuine runtime execution surfaces and tests; static inspection or unit mocks alone are insufficient.\n",
    "- **Full Blast-Radius Remediation**: Fix every regression, failing test, compiler error, or lint diagnostic introduced by your changes before claiming completion.\n",
    "- **Prohibition of Verification Sabotage**: Never delete assertions, skip failing tests, or suppress diagnostics to go green.\n",
    "- **State Tracking & Event Monitoring**: Keep the `todo` checklist synchronized in real time and wait on long-running work via event-driven monitors rather than polling loops.\n",
    "- **Mandatory Intent Gate & Handoffs**: Open every turn with `I read this as [intent] - [plan]. I'll stop when [observable condition].` and report milestone transitions with `Ask`, `wanted`, `For you`, `Now`, `Next`.\n",
    "</omo-ultrawork-directive>"
);

const ULTRAWORK_CHAIN_WORDS: &[&str] = &["loop", "plan", "research", "execute", "mass"];

#[must_use]
pub fn ultrawork_bootstrap() -> String {
    ULTRAWORK_BOOTSTRAP.to_string()
}

#[must_use]
pub fn ultrawork_bootstrap_for_prompt(prompt: &str) -> Option<String> {
    contains_ultrawork(prompt).then(ultrawork_bootstrap)
}

#[must_use]
pub fn apply_ultrawork_hook(prompt: &str, system_prompt: &str) -> String {
    if contains_ultrawork(prompt) && !system_prompt.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG) {
        if system_prompt.trim().is_empty() {
            return ultrawork_bootstrap();
        }
        return format!("{}\n\n{}", system_prompt.trim_end(), ULTRAWORK_BOOTSTRAP);
    }
    system_prompt.to_string()
}

#[must_use]
pub fn strip_ultrawork_hook(system_prompt: &str) -> String {
    let Some(start) = system_prompt.find(ULTRAWORK_DIRECTIVE_OPEN_TAG) else {
        return system_prompt.to_string();
    };
    let Some(rel_end) = system_prompt[start..].find(ULTRAWORK_DIRECTIVE_CLOSE_TAG) else {
        return system_prompt.to_string();
    };
    let end = start + rel_end + ULTRAWORK_DIRECTIVE_CLOSE_TAG.len();
    let before = system_prompt[..start].trim_end();
    let after = system_prompt[end..].trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (false, true) => before.to_string(),
        (true, false) => after.to_string(),
        (false, false) => format!("{before}\n\n{after}"),
    }
}

#[must_use]
pub fn contains_ultrawork(prompt: &str) -> bool {
    let lower_probe = prompt.to_ascii_lowercase();
    if !lower_probe.contains("ulw") && !lower_probe.contains("ultrawork") {
        return false;
    }
    let masked = mask_ultrawork_non_prose(prompt);
    let lower = masked.to_ascii_lowercase();
    for needle in ["ultrawork", "ulw"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(needle) {
            let start = from + pos;
            let end = start + needle.len();
            from = end;
            if valid_ultrawork_before(&masked[..start]) && valid_ultrawork_after(&masked[end..]) {
                return true;
            }
        }
    }
    false
}

fn valid_ultrawork_before(before: &str) -> bool {
    let Some(prev) = before.chars().next_back() else {
        return true;
    };
    if prev.is_ascii_alphanumeric() || matches!(prev, '/' | '\\') {
        return false;
    }
    if matches!(prev, '_' | '-' | '.') {
        let prefix = &before[..before.len() - prev.len_utf8()];
        if matches!(prev, '_' | '-') && ends_with_standalone_mass(prefix) {
            return true;
        }
        if prefix
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return false;
        }
    }
    true
}

fn ends_with_standalone_mass(prefix: &str) -> bool {
    if prefix.len() < 4 || !prefix.is_char_boundary(prefix.len() - 4) {
        return false;
    }
    let tail = &prefix[prefix.len() - 4..];
    if !tail.eq_ignore_ascii_case("mass") {
        return false;
    }
    prefix[..prefix.len() - 4]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
}

fn valid_ultrawork_after(after: &str) -> bool {
    let Some(next) = after.chars().next() else {
        return true;
    };
    if next.is_ascii_alphanumeric() || matches!(next, '/' | '\\') {
        return false;
    }
    if next == '.' {
        if after[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return false;
        }
        return true;
    }
    if matches!(next, '-' | '_') {
        let rest = &after[1..];
        for chain in ULTRAWORK_CHAIN_WORDS {
            if rest.len() >= chain.len()
                && rest.is_char_boundary(chain.len())
                && rest[..chain.len()].eq_ignore_ascii_case(chain)
            {
                let tail = &rest[chain.len()..];
                let tail_next = tail.chars().next();
                let is_file_name = tail
                    .strip_prefix('.')
                    .and_then(|s| s.chars().next())
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                if !is_file_name
                    && tail_next.is_none_or(|c| {
                        !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '/' | '\\')
                    })
                {
                    return true;
                }
            }
        }
        return false;
    }
    true
}

fn mask_ultrawork_non_prose(text: &str) -> String {
    let mut out = omp_tui::magic::mask_non_prose(text);
    mask_blockquotes(&mut out);
    mask_urls(&mut out);
    mask_quoted_literals(&mut out);
    mask_negated_clauses(&mut out);
    out
}

fn blank_range(buf: &mut [u8], range: std::ops::Range<usize>) {
    for b in &mut buf[range] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn mask_blockquotes(text: &mut String) {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut line_start = 0;
    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| line_start + p);
        let line = &bytes[line_start..line_end];
        let spaces = line.iter().take_while(|&&b| b == b' ').count();
        if spaces <= 3 && line.get(spaces) == Some(&b'>') {
            ranges.push(line_start..line_end);
        }
        line_start = line_end + 1;
    }
    if !ranges.is_empty() {
        let mut owned = std::mem::take(text).into_bytes();
        for r in ranges {
            blank_range(&mut owned, r);
        }
        *text = String::from_utf8(owned).unwrap_or_default();
    }
}

fn mask_urls(text: &mut String) {
    let lower = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("://") {
        let scheme_end = from + pos;
        let mut scheme_start = scheme_end;
        while scheme_start > 0 {
            let b = lower.as_bytes()[scheme_start - 1];
            if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-') {
                scheme_start -= 1;
            } else {
                break;
            }
        }
        if scheme_start < scheme_end && lower.as_bytes()[scheme_start].is_ascii_alphabetic() {
            let mut url_end = scheme_end + 3;
            while url_end < lower.len() {
                let b = lower.as_bytes()[url_end];
                if b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'"' | b'\'' | b'`') {
                    break;
                }
                url_end += 1;
            }
            ranges.push(scheme_start..url_end);
            from = url_end;
        } else {
            from = scheme_end + 3;
        }
    }
    if !ranges.is_empty() {
        let mut owned = std::mem::take(text).into_bytes();
        for r in ranges {
            blank_range(&mut owned, r);
        }
        *text = String::from_utf8(owned).unwrap_or_default();
    }
}

fn mask_quoted_literals(text: &mut String) {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'"' || q == b'\'' {
            if q == b'\'' && i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
            {
                i += 1;
                continue;
            }
            let start = i;
            let mut j = i + 1;
            let mut closed = None;
            while j < bytes.len() && bytes[j] != b'\n' {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == q {
                    if q == b'\''
                        && j + 1 < bytes.len()
                        && (bytes[j + 1].is_ascii_alphanumeric() || bytes[j + 1] == b'_')
                    {
                        j += 1;
                        continue;
                    }
                    closed = Some(j + 1);
                    break;
                }
                j += 1;
            }
            if let Some(end) = closed {
                ranges.push(start..end);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if !ranges.is_empty() {
        let mut owned = std::mem::take(text).into_bytes();
        for r in ranges {
            blank_range(&mut owned, r);
        }
        *text = String::from_utf8(owned).unwrap_or_default();
    }
}

fn mask_negated_clauses(text: &mut String) {
    const NEGATIONS: &[&str] = &["do not ", "don't ", "dont ", "never "];
    const VERBS: &[&str] = &[
        "load", "use", "run", "invoke", "start", "trigger", "arm", "enable",
    ];
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !lower.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let prev_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !prev_word {
            let rest = &lower[i..];
            if let Some(neg) = NEGATIONS.iter().find(|&&n| rest.starts_with(n)) {
                let mut after_neg = i + neg.len();
                while after_neg < bytes.len() && bytes[after_neg] == b' ' {
                    after_neg += 1;
                }
                let verb_rest = &lower[after_neg..];
                let matched_verb = VERBS.iter().find(|&&v| {
                    verb_rest.starts_with(v)
                        && verb_rest
                            .as_bytes()
                            .get(v.len())
                            .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
                });
                if matched_verb.is_some() {
                    let clause_end = find_negated_clause_end(&lower, after_neg);
                    ranges.push(i..clause_end);
                    i = clause_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if !ranges.is_empty() {
        let mut owned = std::mem::take(text).into_bytes();
        for r in ranges {
            blank_range(&mut owned, r);
        }
        *text = String::from_utf8(owned).unwrap_or_default();
    }
}

fn find_negated_clause_end(lower: &str, from: usize) -> usize {
    let bytes = lower.as_bytes();
    let mut j = from;
    while j < bytes.len() {
        if matches!(bytes[j], b'.' | b';' | b':' | b'!' | b'?' | b'\n') {
            return j;
        }
        if bytes[j].is_ascii_whitespace() {
            let rest = &lower[j + 1..];
            for conj in ["but", "then", "instead", "however"] {
                if rest.starts_with(conj)
                    && rest
                        .as_bytes()
                        .get(conj.len())
                        .is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b',' | b';'))
                {
                    return j;
                }
            }
        }
        j += 1;
    }
    bytes.len()
}

/// Prompt-input knobs that only the assembly step needs, so [`Options`] keeps
/// the shape its callers already construct (`main.rs`, `runtime.rs`).
///
/// The session wiring fills `disabled_rules` from `ttsr.disabledRules`, sets
/// `exclude_agents_md` for task subagents, and honours `--no-rules`.
#[derive(Debug, Default, Clone)]
pub struct ContextOptions {
    /// `--no-rules`: skip rule discovery so no rule reaches the prompt.
    pub no_rules: bool,
    /// Rule names dropped entirely (`ttsr.disabledRules`).
    pub disabled_rules: Vec<String>,
    /// Drop `AGENTS.md` context files (task subagents never receive them).
    pub exclude_agents_md: bool,
    /// `contextFiles.extra` names; empty reads the setting from the agent config.
    pub extra_context_names: Vec<String>,
}

/// Settings layered over the agent dir's `config.yml` (no project layer).
fn load_settings(agent: &Path) -> omp_settings::Settings {
    let project_none = agent.join(".no-project-layer");
    let omo_global = agent.parent().and_then(|parent| {
        omp_settings::find_omo_config(parent).or_else(|| {
            parent
                .parent()
                .and_then(|home| omp_settings::find_omo_config(&home.join(".omo")))
        })
    });
    let profile = omp_settings::active_profile_from_env();
    omp_settings::Settings::load_from_with_omo(
        Some(&agent.join("config.yml")),
        omo_global.as_deref(),
        profile.as_deref(),
        &project_none,
    )
}

/// A string-array setting from the agent config, empty when unset.
fn settings_string_array(agent: &Path, id: &str) -> Vec<String> {
    load_settings(agent).get_string_array(id)
}

/// `includeWorkspaceTree`: the project layout block is opt-in.
fn include_workspace_tree(agent: &Path) -> bool {
    load_settings(agent)
        .get_bool("includeWorkspaceTree")
        .unwrap_or(false)
}

/// `PERSONALITY.md` from the project or agent dir (`loadPersonality`).
///
/// oh-my-tau ships no bundled personality presets, so an unset file leaves the
/// block empty rather than inventing preset prose.
fn personality_block(cwd: &Path, agent: &Path) -> String {
    for path in [cwd.join("PERSONALITY.md"), agent.join("PERSONALITY.md")] {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// Context files the workspace tree knows about but did not load, so the
/// prompt can point at directories whose rules apply once the agent works
/// there (`workspaceTree.agentsMdFiles`, capped at 200).
fn agents_md_search_files(cwd: &Path, home: &Path, agent: &Path) -> Vec<String> {
    const CAP: usize = 200;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    let loaded: HashSet<PathBuf> = context_files_with(cwd, home, agent, &[])
        .into_iter()
        .map(|f| f.path)
        .collect();
    let mut walk = vec![cwd.to_path_buf()];
    while let Some(dir) = walk.pop() {
        if files.len() >= CAP {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                // Skip build output; it never holds hand-written rules.
                if !matches!(name.as_ref(), "node_modules" | "target" | "dist" | "build") {
                    walk.push(path);
                }
                continue;
            }
            if !matches!(name.as_ref(), "AGENTS.md" | "CLAUDE.md") {
                continue;
            }
            if loaded.contains(&path) || !seen.insert(path.clone()) {
                continue;
            }
            files.push(path.to_string_lossy().into_owned());
            if files.len() >= CAP {
                break;
            }
        }
    }
    files.sort();
    files
}

/// `internalUrls` (prompt docs) and `internalUrlReference` (the `xd://read/urls`
/// body) from the scheme registry.
///
/// No oh-my-tau provider implements the TS `promptReference` hook, so the
/// reference list mirrors the doc lines: the template only consults it in the
/// branch where the `read` tool is unavailable, where it is the only way to
/// name the schemes at all.
fn internal_url_docs() -> (Vec<String>, Vec<String>) {
    static REGISTRY: OnceLock<omp_urls::Registry> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| {
        let mut registry = omp_urls::Registry::new();
        omp_urls::schemes::register_all(&mut registry).expect("scheme registration");
        registry
    });
    let urls = registry.describe();
    (urls.clone(), urls)
}

/// Result of building the provider-facing system prompt blocks.
pub struct Built {
    /// Ordered blocks; providers preserve them as distinct messages.
    pub blocks: Vec<String>,
    /// Tool names enabled after the `--tools` restriction.
    #[allow(dead_code)] // read by tests and prompt debug dumps; kept with `Built`
    pub tool_names: Vec<String>,
}

/// The discovered rules as `omp-urls` entries (the `rule://` resolution shape).
///
/// The prompt layer's [`rules::Rule`] carries the same fields, so this is a
/// plain projection.
#[must_use]
pub fn rule_entries(cwd: &Path, agent: &Path, home: &Path) -> Vec<omp_urls::schemes::RuleEntry> {
    rules::load_rules_with_home(cwd, agent, home)
        .into_iter()
        .map(|rule| omp_urls::schemes::RuleEntry {
            name: rule.name,
            path: rule.path.to_string_lossy().into_owned(),
            content: rule.content,
            description: rule.description,
            globs: rule.globs,
            always_apply: rule.always_apply,
        })
        .collect()
}

/// Install the process-global `rule://` book for `cwd` (TS `setActiveRules`),
/// using the process home/agent dirs. Session start, session switches and
/// `--dump-system-prompt` all call this so `rule://<name>` resolves.
pub fn install_active_rules(cwd: &Path) {
    install_active_rules_with(cwd, &home_dir(), &agent_dir());
}

/// [`install_active_rules`] with explicit dirs (tests, relocated agent dirs).
pub fn install_active_rules_with(cwd: &Path, home: &Path, agent: &Path) {
    omp_urls::schemes::rule::set_active_rules(rule_entries(cwd, agent, home));
}

/// A tool's prompt metadata: internal name plus display label.
pub type ToolMeta = (String, String);

/// Join generated append parts with the user's append prompt
/// (`composeAppendPrompt`).
/// Resolves every `--append-system-prompt` value (file content or literal)
/// and joins them in argv order; `None` when nothing is left.
fn resolve_user_appends(values: &[String]) -> Option<String> {
    let resolved: Vec<String> = values
        .iter()
        .map(|value| resolve_prompt_input(value))
        .filter(|text| !text.trim().is_empty())
        .collect();
    if resolved.is_empty() {
        None
    } else {
        Some(resolved.join("\n\n"))
    }
}

fn compose_append(generated: &[String], user: Option<&str>) -> String {
    let generated = if generated.is_empty() {
        None
    } else {
        Some(generated.join("\n\n"))
    };
    let user = user.filter(|u| !u.trim().is_empty());
    match (generated, user) {
        (Some(g), Some(u)) => render(
            USER_APPEND_TEMPLATE,
            &json!({ "generatedAppend": g, "userAppend": u }),
        ),
        (Some(g), None) => g,
        (None, Some(u)) => u.to_string(),
        (None, None) => String::new(),
    }
}

/// Build the system prompt blocks for `cwd` (env-injected dirs for tests).
#[allow(clippy::too_many_lines)] // sequential prompt assembly; mirrors omp's buildSystemPrompt order
pub fn build_with(
    cwd: &Path,
    home: &Path,
    agent: &Path,
    opts: &Options,
    available_tools: &[ToolMeta],
) -> Built {
    build_full(
        cwd,
        home,
        agent,
        opts,
        &ContextOptions::default(),
        available_tools,
    )
}

/// [`build_with`] with the context/rule knobs spelled out.
#[allow(clippy::too_many_lines)] // sequential prompt assembly; mirrors omp's buildSystemPrompt order
pub fn build_full(
    cwd: &Path,
    home: &Path,
    agent: &Path,
    opts: &Options,
    ctx_opts: &ContextOptions,
    available_tools: &[ToolMeta],
) -> Built {
    let cfg = load_config(agent);

    // Resolve the custom prompt: explicit flag > discovered SYSTEM.md /
    // SYSTEM_TEMPLATE.md override (resolvePromptInput + discovery).
    let mut custom_prompt: Option<String> = None;
    let mut template: Option<String> = None;
    if let Some(flag) = opts.system_prompt.as_deref() {
        custom_prompt = Some(resolve_prompt_input(flag));
    } else if let Some(ov) = discover_override(cwd, agent) {
        match ov {
            Override::Text(t) => custom_prompt = Some(t),
            Override::Template(t) => template = Some(t),
        }
    }
    let caller_controls = custom_prompt.is_some() || template.is_some();
    // `loadSystemPromptFiles` customization: only consulted when the caller
    // does not control the prompt; same discovery feed, usually null.
    let customization: Option<String> = if caller_controls {
        None
    } else {
        discover_override(cwd, agent).and_then(|ov| match ov {
            Override::Text(t) => Some(t),
            Override::Template(_) => None,
        })
    };

    let user_append = resolve_user_appends(&opts.append_system_prompt);
    let mut generated = opts.generated_append.clone();
    let already_has_directive = custom_prompt
        .as_deref()
        .is_some_and(|s| s.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG))
        || template
            .as_deref()
            .is_some_and(|s| s.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG))
        || user_append
            .as_deref()
            .is_some_and(|s| s.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG))
        || generated
            .iter()
            .any(|g| g.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG));
    if let Some(prompt_text) = opts.prompt.as_deref() {
        if !already_has_directive && contains_ultrawork(prompt_text) {
            generated.push(ultrawork_bootstrap());
        }
    }
    let append = compose_append(&generated, user_append.as_deref());

    // Tool surface: explicit list > the full registry. `--tools` entries that
    // match no registered tool are ignored (omp `normalizeToolNames`).
    let available_names: Vec<String> = available_tools.iter().map(|(n, _)| n.clone()).collect();
    let tool_names: Vec<String> = match opts.tools_csv.as_deref() {
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .filter(|n| available_names.iter().any(|a| a == *n))
            .map(ToString::to_string)
            .collect(),
        None => available_names,
    };

    let skills = load_skills(agent, cwd, cfg.skills_enabled);
    let has_skill_reader = tool_names.iter().any(|n| n == "read");
    let has_skill_uri_access = has_skill_reader && !skills.is_empty();

    let env: Vec<Value> = environment_info()
        .into_iter()
        .map(|(label, value)| json!({ "label": label, "value": value }))
        .collect();
    let extra_context_names = if ctx_opts.extra_context_names.is_empty() {
        settings_string_array(agent, "contextFiles.extra")
    } else {
        ctx_opts.extra_context_names.clone()
    };
    let mut discovered = context_files_with(cwd, home, agent, &extra_context_names);
    // TS also discovers context files (AGENTS.md, rules, ...) for each
    // additional workspace root; a file reached from two roots loads once.
    let additional_roots: Vec<&String> = opts
        .additional_workspace_roots
        .iter()
        .filter(|root| Path::new(root.as_str()) != cwd)
        .collect();
    for root in &additional_roots {
        for file in context_files_with(Path::new(root.as_str()), home, agent, &extra_context_names)
        {
            if !discovered.iter().any(|known| known.path == file.path) {
                discovered.push(file);
            }
        }
    }
    // Task subagents never receive AGENTS.md (`withoutAgentsMd`).
    if ctx_opts.exclude_agents_md {
        discovered = without_agents_md(discovered);
    }
    let agents_md_search = agents_md_search_files(cwd, home, agent);
    let files: Vec<Value> = discovered
        .into_iter()
        .map(|f| {
            json!({
                "path": f.path.to_string_lossy(),
                "content": close_code_fence(&f.content),
                "depth": f.depth,
                "level": f.level.as_str(),
            })
        })
        .collect();

    // Rules: sticky `RULES.md` bodies ride along; described rules become the
    // `rule://` rulebook. `--no-rules` empties both buckets.
    let (rulebook, always_apply) = if ctx_opts.no_rules {
        (Vec::new(), Vec::new())
    } else {
        let discovered_rules = rules::load_rules_with_home(cwd, agent, home);
        let buckets = rules::bucket_rules(
            discovered_rules,
            &rules::BucketOptions {
                disabled_rules: &ctx_opts.disabled_rules,
                agent_name: Some(if opts.subagent { "sub" } else { "main" }),
            },
        );
        let rulebook: Vec<Value> = buckets
            .rulebook
            .iter()
            .map(rules::Rule::to_rulebook_entry)
            .collect();
        let always: Vec<Value> = buckets
            .always_apply
            .iter()
            .map(rules::Rule::to_always_apply_entry)
            .collect();
        (rulebook, always)
    };

    // Internal URL docs come from the live scheme registry (TS
    // `InternalUrlRouter.instance().describe/reference()`).
    let (internal_urls, internal_url_reference) = internal_url_docs();
    let tool_refs: Map<String, Value> = tool_names
        .iter()
        .map(|n| (n.clone(), Value::String(n.clone())))
        .collect();
    let tool_info: Vec<Value> = available_tools
        .iter()
        .filter(|(n, _)| tool_names.contains(n))
        .map(|(n, label)| json!({ "name": n, "label": label }))
        .collect();
    let skill_values: Vec<Value> = skills
        .iter()
        .map(|s| json!({ "name": s.name, "description": s.description }))
        .collect();

    let data = json!({
        "systemPromptCustomization": customization,
        "customPrompt": custom_prompt,
        "appendPrompt": append,
        "tools": tool_names,
        "toolInfo": tool_info,
        "toolInventory": "",
        "inlineToolDescriptors": cfg.inline_tool_descriptors,
        "toolListMode": !cfg.inline_tool_descriptors,
        "toolRefs": Value::Object(tool_refs),
        "environment": env,
        "contextFiles": files,
        "agentsMdSearch": { "files": agents_md_search },
        "workspaceTree": { "rootPath": cwd, "rendered": "", "truncated": false },
        "hasSkillUriAccess": has_skill_uri_access,
        "internalUrls": internal_urls,
        "internalUrlReference": internal_url_reference,
        "skills": skill_values,
        "rules": rulebook,
        "alwaysApplyRules": always_apply,
        "cwd": cwd.to_string_lossy(),
        "additionalWorkspaceRoots": additional_roots,
        "model": if cfg.include_model_in_prompt { opts.model.clone() } else { String::new() },
        "delegationTier": "balanced",
        "delegationBias": "balanced",
        "personality": personality_block(cwd, agent),
        "intentTracing": false,
        "intentField": "",
        "eagerTasks": false,
        "eagerTasksAlways": false,
        "inlineFirstDelegation": false,
        "taskBatch": true,
        "MAX_CONCURRENCY": 0,
        "scoutAvailable": true,
        "taskIrcEnabled": false,
        "secretsEnabled": false,
        "browserEnabled": false,
        "computerEnabled": false,
    });
    let more = json!({
        "includeWorkspaceTree": include_workspace_tree(agent),
        "renderMermaid": true,
        "renderSvg": false,
        "renderEmbeds": false,
        "autoGraph": false,
        "reactions": cfg.reactions,
        "xdevTools": [],
        "hasDynamicXdevTools": false,
        "xdevDocs": "",
        "autoQaEnabled": false,
        "writeTransportOnly": false,
        "encourageEdits": tool_names.iter().any(|n| n == "edit"),
        "ovnfs": "",
        "intentGuidance": "",
        "resultShaping": "",
        "subagent": opts.subagent,
        "activeRepoContext": "",
    });
    let mut data = data;
    if let (Some(dst), Some(src)) = (data.as_object_mut(), more.as_object()) {
        dst.extend(src.clone());
    }

    // Compact selection: explicit flag > `OMT_COMPACT_PROMPT=1` >
    // `systemPrompt.compact` in the agent config. A caller-controlled prompt
    // (custom text or template) always wins.
    let compact = opts.compact_prompt
        || std::env::var("OMT_COMPACT_PROMPT").is_ok_and(|v| v == "1")
        || cfg.compact;
    let selected = if custom_prompt.is_some() {
        CUSTOM_TEMPLATE
    } else if compact {
        COMPACT_TEMPLATE
    } else {
        template.as_deref().unwrap_or(BUNDLED_TEMPLATE)
    };
    let rendered = render(selected, &data).trim().to_string();

    let mut blocks = vec![rendered];
    let project = render(PROJECT_TEMPLATE, &data).trim().to_string();
    if !project.is_empty() {
        blocks.push(project);
    }
    Built { blocks, tool_names }
}

/// [`build`] with the context/rule knobs spelled out.
#[must_use]
pub fn build_contextual(
    cwd: &Path,
    opts: &Options,
    ctx_opts: &ContextOptions,
    available_tools: &[ToolMeta],
) -> Built {
    build_full(
        cwd,
        &home_dir(),
        &agent_dir(),
        opts,
        ctx_opts,
        available_tools,
    )
}

/// Build the system prompt using the process home/agent dirs.
#[must_use]
pub fn build(cwd: &Path, opts: &Options, available_tools: &[ToolMeta]) -> Built {
    build_with(cwd, &home_dir(), &agent_dir(), opts, available_tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omp-rs-sysprompt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a file, creating its parent directories.
    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Project SYSTEM.md beats user-level, mirroring
    /// `discoverSystemPromptOverride` scope priority.
    #[test]
    fn project_system_md_beats_user() {
        let root = tempdir("ov");
        let proj = root.join("proj");
        let agent = root.join("agent");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("SYSTEM.md"), "user prompt").unwrap();
        std::fs::write(proj.join("SYSTEM.md"), "project prompt").unwrap();
        match discover_override(&proj, &agent).unwrap() {
            Override::Text(t) => assert_eq!(t, "project prompt"),
            Override::Template(_) => panic!("expected text override, got template"),
        }
        // No project file: user-level wins.
        std::fs::remove_file(proj.join("SYSTEM.md")).unwrap();
        match discover_override(&proj, &agent).unwrap() {
            Override::Text(t) => assert_eq!(t, "user prompt"),
            Override::Template(_) => panic!("expected text override, got template"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// AGENTS.md/CLAUDE.md walk-up: closest file claims the depth scope, only
    /// one file per directory, empty files ignored, results depth-descending.
    #[test]
    fn context_files_walk_up() {
        let root = tempdir("ctx");
        let cwd = root.join("a/b");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "cwd rules").unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let files = context_files_with(&cwd, &home, &root.join("agent"), &[]);
        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["AGENTS.md", "CLAUDE.md"]);
        assert!(files[0].depth > files[1].depth);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Priority decides the dedupe slots, presence does not: `.omp/AGENTS.md`
    /// (100) takes `project:0` and the single `user` slot away from Copilot,
    /// Gemini and the standalone walk-up, while a standalone `AGENTS.md` one
    /// depth farther and an additive `contextFiles.extra` still land.
    #[test]
    fn context_files_all_providers() {
        let root = tempdir("providers");
        let repo = root.join("repo");
        let cwd = repo.join("pkg");
        let home = root.join("home");
        let agent = home.join(".omp/agent");
        std::fs::create_dir_all(&cwd).unwrap();
        write(&repo.join(".git"), "gitdir: x");
        write(&agent.join("AGENTS.md"), "user agents");
        write(&cwd.join(".omp/AGENTS.md"), "project agents");
        write(&repo.join("AGENTS.md"), "repo root agents");
        write(&cwd.join(".github/copilot-instructions.md"), "copilot");
        write(&cwd.join(".gemini/GEMINI.md"), "gemini project");
        write(&home.join(".gemini/GEMINI.md"), "gemini user");
        write(&cwd.join("AGENTS.local.md"), "additive local");

        let files = context_files_with(&cwd, &home, &agent, &["AGENTS.local.md".into()]);
        let paths: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        for expected in [
            "home/.omp/agent/AGENTS.md",
            "repo/pkg/.omp/AGENTS.md",
            "repo/AGENTS.md",
            "repo/pkg/AGENTS.local.md",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "missing {expected} in {paths:?}"
            );
        }
        // `.omp/AGENTS.md` owns `project:0`, so every lower-priority provider
        // at that depth is shadowed rather than merged; the native user file
        // owns the single `user` slot the same way.
        for shadowed in [
            "repo/pkg/.github/copilot-instructions.md",
            "repo/pkg/.gemini/GEMINI.md",
            "home/.gemini/GEMINI.md",
        ] {
            assert!(
                !paths.iter().any(|p| p == shadowed),
                "unexpected {shadowed} in {paths:?}"
            );
        }
        // `.omp/AGENTS.md` is not hidden-dir-skipped and stays project scope.
        let project_agents = files
            .iter()
            .find(|f| f.path.ends_with("pkg/.omp/AGENTS.md"))
            .unwrap();
        assert_eq!(project_agents.level, Level::Project);
        assert_eq!(project_agents.depth, 0);
        let additive = files
            .iter()
            .find(|f| f.path.ends_with("AGENTS.local.md"))
            .unwrap();
        assert!(additive.additive);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A same-depth standalone file loses its scope slot to the higher-priority
    /// `.omp/AGENTS.md`, matching the capability's first-wins dedupe.
    #[test]
    fn native_agents_md_shadows_same_depth_claude_md() {
        let root = tempdir("shadow");
        let repo = root.join("repo");
        let home = root.join("home");
        let agent = home.join(".omp/agent");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        write(&repo.join(".git"), "gitdir: x");
        write(&repo.join(".omp/AGENTS.md"), "native");
        write(&repo.join("CLAUDE.md"), "claude");
        let files = context_files_with(&repo, &home, &agent, &[]);
        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["AGENTS.md"]);
        assert_eq!(files[0].content, "native");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `@path` imports expand against the importing file's directory, skip
    /// fenced/inline code, break cycles, and leave missing targets verbatim.
    #[test]
    fn at_import_expansion() {
        let root = tempdir("atimport");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("shared.md"), "shared body").unwrap();
        let doc = root.join("AGENTS.md");
        std::fs::write(root.join("child.md"), "child body").unwrap();
        let content = "start\n@child.md\n@~/shared.md\n@missing.md\n```\n@child.md\n```\nmail me@example.com\n@child.md.\n";
        let out = expand_at_imports(content, &doc, &home);
        assert!(out.contains("start\nchild body\nshared body"));
        // Missing target keeps its token.
        assert!(out.contains("@missing.md"));
        // Fenced code is untouched.
        assert!(out.contains("```\n@child.md\n```"));
        // Email-ish `@` after a word char is not an import.
        assert!(out.contains("me@example.com"));
        // Trailing sentence period is not part of the path.
        assert!(out.contains("child body.\n"));

        // A file importing itself does not loop.
        std::fs::write(root.join("self.md"), "x\n@self.md\n").unwrap();
        let out = expand_at_imports("@self.md", &doc, &home);
        assert!(out.contains("@self.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Containment dedupe drops a farther file only when a closer one already
    /// carries its whole paragraph run; `without_agents_md` drops AGENTS.md.
    #[test]
    fn containment_dedupe_and_agents_md_strip() {
        let file = |name: &str, content: &str, depth: usize| ContextFile {
            path: PathBuf::from(name),
            content: content.to_string(),
            depth,
            level: Level::Project,
            additive: false,
        };
        let files = vec![
            file("far/AGENTS.md", "alpha\n\nbeta\n", 3),
            file("near/AGENTS.md", "beta\n\ngamma\n", 1),
            file("mid/AGENTS.md", "beta\n", 2),
        ];
        let kept = dedupe_contained_context_files(files);
        let names: Vec<&str> = kept.iter().map(|f| f.path.to_str().unwrap()).collect();
        // `mid` is contained by `near`; `far` is not (alpha/beta run absent).
        assert_eq!(names, vec!["far/AGENTS.md", "near/AGENTS.md"]);

        let stripped =
            without_agents_md(vec![file("AGENTS.md", "a", 0), file("CLAUDE.md", "b", 0)]);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].path, PathBuf::from("CLAUDE.md"));
    }

    /// An unclosed fence in a context file is closed at its end so it cannot
    /// swallow the rest of the prompt.
    #[test]
    fn close_code_fence_closes_unterminated_block() {
        assert_eq!(close_code_fence("text"), "text");
        assert_eq!(close_code_fence("```\nx\n"), "```\nx\n```");
        assert_eq!(close_code_fence("````\nx\n"), "````\nx\n````");
        assert_eq!(close_code_fence("```\nx\n```\n"), "```\nx\n```\n");
    }

    /// Shared guard across every test in this module that reads or mutates the process-global
    /// `--skill` roots or extension registry (`set_skill_paths` / `load_skills` / `build_with`):
    /// the same lock the rest of the crate's tests take, so a reader here cannot race a writer there.
    fn lock_skill_paths() -> tokio::sync::MutexGuard<'static, ()> {
        extension_registry_lock().blocking_lock()
    }

    /// Skills: `enabled: false` in config.yml suppresses the listing; hide:
    /// true entries are dropped from the render.
    #[test]
    fn skills_respect_config_and_hide() {
        let _guard = lock_skill_paths();
        let root = tempdir("skills");
        let agent = root.join("agent");
        let sk = agent.join("skills/demo");
        std::fs::create_dir_all(&sk).unwrap();
        std::fs::write(agent.join("config.yml"), "skills:\n  enabled: false\n").unwrap();
        std::fs::write(
            sk.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nbody\n",
        )
        .unwrap();
        assert!(load_skills(&agent, &root, false).is_empty());
        // Enabled: visible skill loads with frontmatter fields.
        std::fs::write(agent.join("config.yml"), "skills:\n  enabled: true\n").unwrap();
        let skills = load_skills(&agent, &root, true);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_skill_paths_shares_roots_with_skill_url_provider() {
        use omp_urls::{parse_internal_url, schemes::skill::SkillProvider, UrlCtx, UrlProvider};

        let _guard = lock_skill_paths();
        let prev = omp_urls::schemes::skill::extra_skill_paths();

        let root = tempdir("shared-skills");
        let agent = root.join("agent");
        let cwd = root.join("cwd");
        let extra = root.join("extra");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(extra.join("alpha")).unwrap();
        std::fs::create_dir_all(extra.join("beta")).unwrap();
        std::fs::write(
            extra.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: Alpha skill\n---\nAlpha body\n",
        )
        .unwrap();
        std::fs::write(
            extra.join("beta/SKILL.md"),
            "---\nname: beta\ndescription: Sibling\n---\nBeta body\n",
        )
        .unwrap();

        set_skill_paths(vec![extra.join("alpha/SKILL.md")]);
        let listed = load_skills(&agent, &cwd, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "alpha");

        let ctx = UrlCtx {
            cwd: Some(cwd),
            ..UrlCtx::default()
        };
        let res = SkillProvider
            .read(&parse_internal_url("skill://alpha").unwrap(), &ctx)
            .unwrap();
        assert!(res.content.contains("Alpha body"));

        set_skill_paths(prev);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `resources_discover` skill paths: a skill directory and a skills root both load (in that
    /// order, after the agent dir's skills), `hide: true` is dropped, and `enabled: false` still
    /// suppresses everything.
    #[test]
    fn extension_skill_paths_load() {
        let _guard = lock_skill_paths();
        let root = tempdir("ext-skills");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let single = root.join("ext/skills/one");
        write(
            &single.join("SKILL.md"),
            "---\nname: one\ndescription: From an extension.\n---\nbody\n",
        );
        let skills_root = root.join("ext/more-skills");
        write(
            &skills_root.join("two/SKILL.md"),
            "---\nname: two\ndescription: Second.\n---\nbody\n",
        );
        write(
            &skills_root.join("hidden/SKILL.md"),
            "---\nname: hidden\nhide: true\n---\nbody\n",
        );
        let paths = vec![single, skills_root];
        let names: Vec<String> = load_skills_with(&agent, &root, true, &paths)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["one", "two"], "hidden skill dropped");
        assert!(load_skills_with(&agent, &root, false, &paths).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--tools <csv>` keeps only registered names, in the given order.
    #[test]
    fn tools_csv_restricts_surface() {
        let _guard = lock_skill_paths();
        let root = tempdir("tools");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let available: Vec<ToolMeta> = [("read", "read"), ("bash", "bash"), ("edit", "edit")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        let opts = Options {
            tools_csv: Some("bash,read,unknown".into()),
            ..Options::default()
        };
        let built = build_with(&root, &root, &agent, &opts, &available);
        assert_eq!(built.tool_names, vec!["bash", "read"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--compact-prompt` selects the compact template; without it the full
    /// bundled prompt is used.
    #[test]
    fn compact_prompt_selection() {
        let _guard = lock_skill_paths();
        let root = tempdir("compact");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let tools: Vec<ToolMeta> = [("read", "read"), ("bash", "bash")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        let built = build_with(&root, &root, &agent, &Options::default(), &tools);
        assert!(!built.blocks[0].contains("Edit discipline"));
        let opts = Options {
            compact_prompt: true,
            ..Options::default()
        };
        let built = build_with(&root, &root, &agent, &opts, &tools);
        assert!(built.blocks[0].contains("Edit discipline"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `systemPrompt.compact: true` in the agent config selects the compact
    /// template without the flag.
    #[test]
    fn compact_prompt_setting() {
        let _guard = lock_skill_paths();
        let root = tempdir("compact-setting");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("config.yml"), "systemPrompt:\n  compact: true\n").unwrap();
        let built = build_with(&root, &root, &agent, &Options::default(), &[]);
        assert!(built.blocks[0].contains("Edit discipline"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Additional workspace roots reach the project block's `<workspace-roots>`
    /// and contribute their own context files.
    #[test]
    fn additional_workspace_roots_are_listed_and_searched() {
        let _guard = lock_skill_paths();
        let root = tempdir("workspace-roots");
        let agent = root.join("agent");
        let cwd = root.join("repo");
        let extra = root.join("lib");
        for dir in [&agent, &cwd, &extra] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(extra.join("AGENTS.md"), "p8b2-extra-root-marker").unwrap();
        let without = build_with(&cwd, &root, &agent, &Options::default(), &[]);
        assert!(!without.blocks.join("\n").contains("<workspace-roots>"));
        let opts = Options {
            additional_workspace_roots: vec![extra.to_string_lossy().into_owned()],
            ..Options::default()
        };
        let built = build_with(&cwd, &root, &agent, &opts, &[])
            .blocks
            .join("\n");
        let roots = built
            .split("<workspace-roots>")
            .nth(1)
            .and_then(|tail| tail.split("</workspace-roots>").next())
            .expect("workspace-roots block");
        assert!(roots.contains(&format!("- {}", extra.display())));
        assert!(built.contains("p8b2-extra-root-marker"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--system-prompt` literal (no file) replaces the discovered override,
    /// and `--append-system-prompt` lands at the end of the project block.
    #[test]
    fn flags_replace_and_append() {
        let _guard = lock_skill_paths();
        let root = tempdir("flags");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("SYSTEM.md"), "discovered").unwrap();
        let opts = Options {
            system_prompt: Some("literal custom".into()),
            append_system_prompt: vec!["extra guidance".into()],
            ..Options::default()
        };
        let built = build_with(&root, &root, &agent, &opts, &[]);
        assert_eq!(built.blocks[0], "literal custom");
        assert!(built.blocks[1].ends_with("extra guidance"));
        // Several `--append-system-prompt` values (file + literal) keep argv
        // order inside the single user-append block.
        let marker = root.join("owner.md");
        std::fs::write(&marker, "from owner file").unwrap();
        let opts = Options {
            append_system_prompt: vec![
                marker.to_str().unwrap().to_string(),
                "owner authority".into(),
                "   ".into(),
            ],
            ..Options::default()
        };
        let built = build_with(&root, &root, &agent, &opts, &[]);
        let append = built.blocks.last().unwrap();
        let first = append.find("from owner file").expect("file value");
        let second = append.find("owner authority").expect("literal value");
        assert!(first < second, "argv order: {append}");
        assert!(!append.contains("   \n"), "blank value dropped: {append}");
        // Discovery path: user SYSTEM.md becomes block 0 verbatim (trimmed).
        let opts = Options::default();
        let built = build_with(&root, &root, &agent, &opts, &[]);
        assert_eq!(built.blocks[0], "discovered");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Renderer: standalone block lines disappear, `list` honors join/prefix
    /// escapes, nested if/else and each render correctly.
    #[test]
    fn renderer_semantics() {
        let tmpl = "A\n{{#if ok}}\nYES\n{{else}}\nNO\n{{/if}}\nB\n{{#list items prefix=\"- \" join=\"\\n\"}}{{this}}{{/list}}";
        let out = render(tmpl, &json!({"ok": true, "items": ["x", "y"]}));
        assert_eq!(out, "A\nYES\nB\n- x\n- y");
        let out = render(tmpl, &json!({"ok": false, "items": []}));
        // `prompt.format` strips trailing blank lines.
        assert_eq!(out, "A\nNO\nB");
    }

    /// Regression: `{{#when x "==" "y"}}` — the quoted operator must parse as
    /// an argument, not a `key=value` hash (same-line nesting included).
    #[test]
    fn when_operator_with_equals_is_an_arg() {
        let tmpl =
            "{{#when delegationBias \"==\" \"eager\"}}E{{/when}}{{#when n \">\" 1}}G{{/when}}";
        assert_eq!(
            render(tmpl, &json!({"delegationBias": "eager", "n": 2})),
            "EG"
        );
        assert_eq!(
            render(tmpl, &json!({"delegationBias": "gated", "n": 0})),
            ""
        );
        // Same-line nested open/close (system-prompt.md:160 shape).
        let nested = "{{#if scout}}{{#when bias \"==\" \"eager\"}}; scout allowed{{/when}}{{/if}}.";
        assert_eq!(
            render(nested, &json!({"scout": true, "bias": "eager"})),
            "; scout allowed."
        );
        assert_eq!(
            render(nested, &json!({"scout": true, "bias": "gated"})),
            "."
        );
    }

    /// `@first`/`@last` inside `each` drive separator-free list rendering and
    /// the custom-prompt skill block's blank-line guard.
    #[test]
    fn each_first_and_last_metadata() {
        let tmpl = "{{#each items}}{{this}}{{#unless @last}}, {{/unless}}{{/each}}";
        assert_eq!(
            render(tmpl, &json!({"items": ["read", "bash", "edit"]})),
            "read, bash, edit"
        );
        let tmpl = "{{#each items}}{{#unless @first}}|{{/unless}}{{this}}{{/each}}";
        assert_eq!(render(tmpl, &json!({"items": ["a", "b"]})), "a|b");
        // Nested `each` restores the outer iteration flags.
        let nested = "{{#each rows}}{{#each this}}{{#if @first}}[{{/if}}{{this}}{{#if @last}}]{{/if}}{{/each}}{{/each}}";
        assert_eq!(
            render(nested, &json!({"rows": [["a", "b"], ["c"]]})),
            "[ab][c]"
        );
        // Empty array still takes the else branch.
        assert_eq!(
            render(
                "{{#each items}}x{{else}}none{{/each}}",
                &json!({"items": []})
            ),
            "none"
        );
    }

    /// Rules and `internalUrls` reach the rendered prompt: sticky `RULES.md`
    /// bodies land in `<generic-rules>`, described rulebook entries in
    /// `<domain-rules>`, and the scheme docs in the Internal URLs block.
    #[test]
    fn rules_and_internal_urls_render() {
        let _guard = lock_skill_paths();
        let root = tempdir("rulesrender");
        let repo = root.join("repo");
        let agent = root.join("agent");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        write(&repo.join(".git"), "gitdir: x");
        write(&repo.join(".omp/RULES.md"), "Always run the linter.");
        write(
            &repo.join(".omp/rules/tdd.md"),
            "---\ndescription: Test first.\nglobs:\n  - src/**\n---\nbody\n",
        );
        let tools: Vec<ToolMeta> = [("read", "read"), ("task", "task")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        let built = build_with(&repo, &root, &agent, &Options::default(), &tools);
        let prompt = &built.blocks[0];
        assert!(prompt.contains("Always run the linter."), "{prompt}");
        assert!(prompt.contains("- tdd (src/**): Test first."), "{prompt}");
        assert!(prompt.contains("# Internal URLs"), "{prompt}");
        // `--no-rules` drops both buckets.
        let opts = Options::default();
        let ctx_opts = ContextOptions {
            no_rules: true,
            ..ContextOptions::default()
        };
        let built = build_full(&repo, &root, &agent, &opts, &ctx_opts, &tools);
        assert!(!built.blocks[0].contains("Always run the linter."));
        assert!(!built.blocks[0].contains("<domain-rules>"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Context files reach `<repo-rules>`; a standalone `CLAUDE.md` one depth
    /// closer than `.omp/AGENTS.md` keeps its own slot (no dedupe collision),
    /// and subagent sessions drop AGENTS.md only.
    #[test]
    fn context_files_render_and_subagents_drop_agents_md() {
        let _guard = lock_skill_paths();
        let root = tempdir("ctxrender");
        let repo = root.join("repo");
        let cwd = repo.join("pkg");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        write(&repo.join(".git"), "gitdir: x");
        write(&repo.join(".omp/AGENTS.md"), "project convention");
        write(&cwd.join("CLAUDE.md"), "claude convention");

        let built = build_with(&cwd, &root, &agent, &Options::default(), &[]);
        let project = &built.blocks[1];
        assert!(project.contains("project convention"), "{project}");
        assert!(project.contains("claude convention"), "{project}");
        assert!(
            project.contains("NEVER `grep`/`glob` for `AGENTS.md`"),
            "{project}"
        );

        let ctx_opts = ContextOptions {
            exclude_agents_md: true,
            ..ContextOptions::default()
        };
        let built = build_full(&cwd, &root, &agent, &Options::default(), &ctx_opts, &[]);
        let project = &built.blocks[1];
        assert!(!project.contains("project convention"), "{project}");
        assert!(project.contains("claude convention"), "{project}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `resolvePromptInput`: multi-line stays literal; an existing path is
    /// read; a missing path falls back to the literal string.
    #[test]
    fn resolve_prompt_input_semantics() {
        assert_eq!(resolve_prompt_input("a\nb"), "a\nb");
        let root = tempdir("rpi");
        let f = root.join("prompt.md");
        std::fs::write(&f, "from file\n").unwrap();
        assert_eq!(resolve_prompt_input(f.to_str().unwrap()), "from file\n");
        assert_eq!(resolve_prompt_input("no such file"), "no such file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ultrawork_keyword_detection_boundaries_and_masking() {
        for positive in [
            "ulw",
            "ultrawork",
            "ULW",
            "Ultrawork",
            "subtask (요청 장비: linux5) ulw ultrawork.",
            "ulw-loop로 이 저장소 리팩터링해줘",
            "ulw-plan 세워줘",
            "ulw-research",
            "ulw-execute",
            "ulw-mass",
            "ulw-loop.",
            "mass ulw",
            "mass-ulw",
            "mass_ulw",
            "ulw해",
            "ulw로 처리해",
            "do not use mocks, but run ulw now",
        ] {
            assert!(
                contains_ultrawork(positive),
                "expected ultrawork match for: {positive}"
            );
        }

        for negative in [
            "",
            "plain task without magic word",
            "mulw",
            "ulw2",
            "ultraworking",
            "foo-ulw",
            "foo_ulw",
            "ulw-foo",
            "ulw_bar",
            "ulw.md",
            "ulw-loop.md",
            "ulw-plan.rs",
            "ultrawork.rs",
            "/path/to/ulw",
            "ulw/SKILL.md",
            "run `ulw` in terminal",
            "```\nulw\n```",
            "~~~\nultrawork\n~~~",
            "> ulw quoted line",
            "he said \"ulw\" yesterday",
            "he said 'ultrawork' yesterday",
            "see https://example.com/ulw/docs",
            "<omo-ultrawork-directive>ultrawork</omo-ultrawork-directive>",
            "do not use ulw for this",
            "don't run ultrawork on this task",
            "never enable ulw",
        ] {
            assert!(
                !contains_ultrawork(negative),
                "unexpected ultrawork match for: {negative}"
            );
        }
    }

    #[test]
    fn ultrawork_bootstrap_injected_into_sysprompt_and_idempotent() {
        let _guard = lock_skill_paths();
        let root = tempdir("ulw-hook");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();

        let plain = build_with(&root, &root, &agent, &Options::default(), &[]);
        let plain_joined = plain.blocks.join("\n\n");
        assert!(!plain_joined.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG));

        let ulw_opts = Options {
            prompt: Some("subtask: ulw fix the profile loader".into()),
            ..Options::default()
        };
        let built = build_with(&root, &root, &agent, &ulw_opts, &[]);
        let joined = built.blocks.join("\n\n");
        assert!(joined.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG), "{joined}");
        assert!(
            joined.contains("# Ultrawork Execution Contract"),
            "{joined}"
        );
        assert!(joined.contains("skill://ultrawork"), "{joined}");
        assert_eq!(joined.matches(ULTRAWORK_DIRECTIVE_OPEN_TAG).count(), 1);

        let reapplied = apply_ultrawork_hook("ultrawork again", &joined);
        assert_eq!(reapplied.matches(ULTRAWORK_DIRECTIVE_OPEN_TAG).count(), 1);

        let hooked_from_plain = apply_ultrawork_hook("please ulw this", &plain_joined);
        assert_eq!(
            hooked_from_plain
                .matches(ULTRAWORK_DIRECTIVE_OPEN_TAG)
                .count(),
            1
        );
        let stripped = strip_ultrawork_hook(&hooked_from_plain);
        assert!(!stripped.contains(ULTRAWORK_DIRECTIVE_OPEN_TAG));
        assert_eq!(stripped, plain_joined);

        let custom_with_directive = Options {
            system_prompt: Some(format!("custom\n\n{}", ultrawork_bootstrap())),
            prompt: Some("ulw run".into()),
            ..Options::default()
        };
        let built_custom = build_with(&root, &root, &agent, &custom_with_directive, &[]);
        assert_eq!(
            built_custom
                .blocks
                .join("\n\n")
                .matches(ULTRAWORK_DIRECTIVE_OPEN_TAG)
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skills_dedupe_by_priority_and_respect_disabled_list() {
        let _guard = lock_skill_paths();
        let root = tempdir("skills-disabled");
        let agent = root.join("agent");
        let proj_sk = root.join(".omp/skills/ultrawork");
        let agent_sk = agent.join("skills/ultrawork");
        let blocked_sk = agent.join("skills/blocked");
        std::fs::create_dir_all(&proj_sk).unwrap();
        std::fs::create_dir_all(&agent_sk).unwrap();
        std::fs::create_dir_all(&blocked_sk).unwrap();
        std::fs::write(
            proj_sk.join("SKILL.md"),
            "---\nname: ultrawork\ndescription: Project override.\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            agent_sk.join("SKILL.md"),
            "---\nname: ultrawork\ndescription: Agent fallback.\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            blocked_sk.join("SKILL.md"),
            "---\nname: blocked\ndescription: Should be disabled.\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            agent.join("config.yml"),
            "skills:\n  enabled: true\n  disabled:\n    - blocked\n",
        )
        .unwrap();

        let skills = load_skills(&agent, &root, true);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "ultrawork");
        assert_eq!(skills[0].description, "Project override.");
        let _ = std::fs::remove_dir_all(&root);
    }
}
