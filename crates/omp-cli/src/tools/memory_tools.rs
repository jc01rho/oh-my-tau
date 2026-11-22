#![allow(dead_code, clippy::manual_noop_waker)]
//! `retain`, `recall`, `reflect`, `memory_edit`, `learn`, `manage_skill`:
//! omp's memory and self-improvement tools (`packages/coding-agent/src/tools/
//! memory-*.ts`, `learn.ts`, `manage-skill.ts`).
//!
//! Storage is split in two, exactly like the TS reference:
//!
//! * **Long-term memory** goes through [`MemoryToolBackend`]. The port has no
//!   `omp_memory` crate yet, so this module owns the contract instead of
//!   naming a concrete runtime: a session supplies its own implementation
//!   (hindsight/mnemopi client, SQLite store, …) and the tools stay identical.
//!   [`InMemoryBackend`] is a complete reference implementation, used by the
//!   tests and usable as the fallback backend.
//! * **Skills** are plain `SKILL.md` files on disk, in an isolated
//!   `~/.omp/agent/managed-skills/<name>/SKILL.md` tree so auto-management can
//!   never touch an authored skill (`~/.omp/agent/skills`,
//!   `<cwd>/.omp/skills`). A managed skill may not be created under a name an
//!   authored skill already claims, because authored skills always win in
//!   discovery and the write would never surface.
//!
//! Tools are plain `run(&self, &Value)` methods plus the thin `AgentTool`
//! adapter, mirroring `tools/todo.rs`: the async surface is a wrapper, so every
//! behavior below is unit-testable without a runtime.
//!
//! The `Env` link is intentionally narrow: the tools read `env.cwd` once at
//! construction (project skill dir, settings layer) and never hold the `Env`
//! itself, so a tool set can be built and exercised without a live session.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::module_name_repetitions
)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::approval::{Decision, Tier};
use crate::tools::Env;

// ---------------------------------------------------------------------------
// Shared limits
// ---------------------------------------------------------------------------

/// Characters of a recalled memory the model sees (`recall` preview clip).
pub const RECALL_PREVIEW_CHARS: usize = 500;

/// Characters per token used to turn `token_budget` into a character budget.
const CHARS_PER_TOKEN: usize = 4;

/// Default recall/reflect token budget when the caller passes none.
const DEFAULT_TOKEN_BUDGET: usize = 2_000;

/// Hard cap on a managed `SKILL.md` (TS `MAX_MANAGED_SKILL_BYTES`).
pub const MAX_SKILL_BYTES: usize = 64_000;

/// Managed-skill names: lowercase letters, digits and hyphens, 1-64 chars.
const SKILL_NAME_MAX: usize = 64;

/// Tags whose blocks must never be retained: a recalled `<memories>` block fed
/// back into the store is the feedback loop `stripMemoryTags` exists to break.
const MEMORY_TAGS: &[&str] = &[
    "memories",
    "mental_models",
    "hindsight_memories",
    "relevant_memories",
];

// ---------------------------------------------------------------------------
// Memory model
// ---------------------------------------------------------------------------

/// Where a memory lives. `Global` reaches every project's recall, so writing
/// one needs the same approval as a file write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Project,
    Global,
}

impl MemoryScope {
    fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Project => "project",
            MemoryScope::Global => "global",
        }
    }
}

/// Lifecycle of a stored memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Searchable and editable.
    Active,
    /// Superseded by another memory; kept for the audit trail, never recalled.
    Invalidated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_id: Option<String>,
    },
    /// Removed at the model's request; never recalled.
    Forgotten,
}

impl MemoryStatus {
    fn as_str(&self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Invalidated { .. } => "invalidated",
            MemoryStatus::Forgotten => "forgotten",
        }
    }

    fn is_active(&self) -> bool {
        matches!(self, MemoryStatus::Active)
    }
}

/// One stored memory.
#[derive(Debug, Clone, PartialEq)]
pub struct RetainedMemory {
    pub id: String,
    pub content: String,
    pub context: Option<String>,
    pub scope: MemoryScope,
    pub importance: f32,
    pub created_at: u64,
    pub updated_at: u64,
    pub status: MemoryStatus,
}

/// A write request handed to a backend.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryDraft {
    pub content: String,
    pub context: Option<String>,
    pub scope: MemoryScope,
    pub importance: f32,
    /// `retain` | `learn` — recorded so a store can tell tool writes apart.
    pub source: &'static str,
}

/// A ranked recall hit.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub id: String,
    pub content: String,
    pub context: Option<String>,
    pub importance: f32,
    pub score: f32,
    pub created_at: u64,
}

/// Outcome of `memory_edit`, named after the TS `editScopedMemory` statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    Updated,
    Forgotten,
    Invalidated,
    /// No memory carries that id in this scope.
    NotFound,
    /// Present but not editable (forgotten or invalidated): read it, do not edit it.
    NotEditable,
}

impl EditOutcome {
    fn as_str(&self) -> &'static str {
        match self {
            EditOutcome::Updated => "updated",
            EditOutcome::Forgotten => "forgotten",
            EditOutcome::Invalidated => "invalidated",
            EditOutcome::NotFound => "not_found",
            EditOutcome::NotEditable => "not_editable",
        }
    }
}

/// The storage contract the six tools need. Implementations own their
/// persistence; the tools own argument validation and rendering.
///
/// `reflect` has a default: synthesis is recall plus framing for stores that
/// have no dedicated reflection model, which is the honest behavior rather
/// than a fake "insight".
pub trait MemoryToolBackend: Send + Sync {
    /// Store one memory and return its id.
    fn retain(&self, draft: MemoryDraft) -> Result<String>;

    /// Rank `limit` memories against `query`; only active memories are eligible.
    fn recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>>;

    /// Answer `query` from memory. `context` sharpens the query.
    fn reflect(&self, query: &str, context: Option<&str>, limit: usize) -> Result<Vec<RecallHit>> {
        let composed = compose_reflect_query(query, context);
        self.recall(&composed, limit)
    }

    fn update(
        &self,
        id: &str,
        content: Option<&str>,
        importance: Option<f32>,
    ) -> Result<EditOutcome>;

    fn forget(&self, id: &str) -> Result<EditOutcome>;

    fn invalidate(&self, id: &str, replacement_id: Option<&str>) -> Result<EditOutcome>;

    /// Read one memory, whatever its status (used for messages and tests).
    fn get(&self, id: &str) -> Result<Option<RetainedMemory>>;
}

/// TS `composeReflectQuery`: the question plus, when given, an explicit
/// "Additional context" block.
fn compose_reflect_query(query: &str, context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!("{}\n\nAdditional context:\n{context}", query.trim()),
        None => query.trim().to_string(),
    }
}

/// Reference [`MemoryToolBackend`]: everything in memory, scored by token
/// overlap, importance and recency. Deterministic for a fixed clock.
#[derive(Debug, Default)]
pub struct InMemoryBackend {
    inner: Mutex<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    next_id: u64,
    items: Vec<RetainedMemory>,
}

impl InMemoryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of memories ever stored, including forgotten ones.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Seed a memory with an explicit id/clock (tests and fixtures).
    pub fn insert(&self, mut memory: RetainedMemory) -> String {
        let mut inner = self.lock();
        if memory.id.is_empty() {
            inner.next_id += 1;
            memory.id = format!("mem_{:04}", inner.next_id);
        }
        let id = memory.id.clone();
        inner.items.push(memory);
        id
    }
}

impl MemoryToolBackend for InMemoryBackend {
    fn retain(&self, draft: MemoryDraft) -> Result<String> {
        let now = now_secs();
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = format!("mem_{:04}", inner.next_id);
        inner.items.push(RetainedMemory {
            id: id.clone(),
            content: draft.content,
            context: draft.context,
            scope: draft.scope,
            importance: draft.importance,
            created_at: now,
            updated_at: now,
            status: MemoryStatus::Active,
        });
        Ok(id)
    }

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
        let terms = query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let now = now_secs();
        let inner = self.lock();
        let mut hits: Vec<RecallHit> = inner
            .items
            .iter()
            .filter(|item| item.status.is_active())
            .filter_map(|item| {
                let mut haystack = item.content.to_lowercase();
                if let Some(context) = &item.context {
                    haystack.push('\n');
                    haystack.push_str(&context.to_lowercase());
                }
                let matched = terms.iter().filter(|t| haystack.contains(*t)).count();
                if matched == 0 {
                    return None;
                }
                let overlap = matched as f32 / terms.len() as f32;
                let age_days = now.saturating_sub(item.created_at) as f32 / 86_400.0;
                let recency = 1.0 - (age_days / 30.0).min(1.0);
                let score = 0.7 * overlap + 0.2 * item.importance.clamp(0.0, 1.0) + 0.1 * recency;
                Some(RecallHit {
                    id: item.id.clone(),
                    content: item.content.clone(),
                    context: item.context.clone(),
                    importance: item.importance,
                    score,
                    created_at: item.created_at,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn update(
        &self,
        id: &str,
        content: Option<&str>,
        importance: Option<f32>,
    ) -> Result<EditOutcome> {
        let mut inner = self.lock();
        let now = now_secs();
        let Some(item) = inner.items.iter_mut().find(|item| item.id == id) else {
            return Ok(EditOutcome::NotFound);
        };
        if !item.status.is_active() {
            return Ok(EditOutcome::NotEditable);
        }
        if let Some(content) = content {
            item.content = content.to_string();
        }
        if let Some(importance) = importance {
            item.importance = importance;
        }
        item.updated_at = now;
        Ok(EditOutcome::Updated)
    }

    fn forget(&self, id: &str) -> Result<EditOutcome> {
        self.transition(id, |item| {
            item.status = MemoryStatus::Forgotten;
            EditOutcome::Forgotten
        })
    }

    fn invalidate(&self, id: &str, replacement_id: Option<&str>) -> Result<EditOutcome> {
        let replacement = replacement_id.map(str::to_string);
        self.transition(id, move |item| {
            item.status = MemoryStatus::Invalidated {
                replacement_id: replacement,
            };
            EditOutcome::Invalidated
        })
    }

    fn get(&self, id: &str) -> Result<Option<RetainedMemory>> {
        Ok(self.lock().items.iter().find(|item| item.id == id).cloned())
    }
}

impl InMemoryBackend {
    fn transition(
        &self,
        id: &str,
        apply: impl FnOnce(&mut RetainedMemory) -> EditOutcome,
    ) -> Result<EditOutcome> {
        let mut inner = self.lock();
        let now = now_secs();
        let Some(item) = inner.items.iter_mut().find(|item| item.id == id) else {
            return Ok(EditOutcome::NotFound);
        };
        if !item.status.is_active() {
            return Ok(EditOutcome::NotEditable);
        }
        item.updated_at = now;
        Ok(apply(item))
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Lowercase alphanumeric terms of `query`, deduplicated, order preserved.
fn query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        let term = raw.to_lowercase();
        if term.len() >= 2 && seen.insert(term.clone()) {
            terms.push(term);
        }
    }
    terms
}

/// Clip `text` to `max` characters, marking the cut. Character (not byte)
/// based so multi-byte content never panics.
#[must_use]
pub fn clip(text: &str, max: usize) -> (String, bool) {
    if max == 0 {
        return (String::new(), !text.is_empty());
    }
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == max {
            out.push('…');
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

/// Remove `<tag>…</tag>` blocks (TS `stripMemoryTags`). An unterminated
/// opening tag is dropped rather than swallowing the rest of the text.
#[must_use]
pub fn strip_tag_blocks(input: &str, tags: &[&str]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let mut earliest: Option<(usize, &str)> = None;
        for tag in tags {
            let open = format!("<{tag}>");
            if let Some(index) = rest.find(&open) {
                if earliest.is_none_or(|(best, _)| index < best) {
                    earliest = Some((index, tag));
                }
            }
        }
        let Some((index, tag)) = earliest else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..index]);
        let open_len = tag.len() + 2;
        let close = format!("</{tag}>");
        match rest[index..].find(&close) {
            Some(end) => rest = &rest[index + end + close.len()..],
            None => rest = &rest[index + open_len..],
        }
    }
    out
}

/// Canonical memory/lesson text: tags stripped, trailing spaces removed,
/// surrounding blank lines trimmed.
#[must_use]
pub fn sanitize_memory_text(input: &str) -> String {
    let stripped = strip_tag_blocks(input, MEMORY_TAGS);
    stripped
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `YYYY-MM-DD HH:MM` in UTC (TS `formatCurrentTime`).
#[must_use]
pub fn format_utc(timestamp: u64) -> String {
    let days = (timestamp / 86_400) as i64;
    let secs = timestamp % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn scope_arg(args: &Value) -> Result<MemoryScope> {
    match args.get("scope").and_then(Value::as_str) {
        None => Ok(MemoryScope::Project),
        Some("project") => Ok(MemoryScope::Project),
        Some("global") => Ok(MemoryScope::Global),
        Some(other) => bail!("`scope` must be \"project\" or \"global\", got \"{other}\""),
    }
}

/// Turn a token budget into a character budget for rendering.
fn char_budget(tokens: Option<usize>) -> usize {
    tokens.unwrap_or(DEFAULT_TOKEN_BUDGET).max(1) * CHARS_PER_TOKEN
}

// ---------------------------------------------------------------------------
// Skills on disk
// ---------------------------------------------------------------------------

/// Where managed and authored skills live.
#[derive(Debug, Clone)]
pub struct SkillStore {
    managed: PathBuf,
    authored: Vec<PathBuf>,
}

impl SkillStore {
    /// Production layout: managed skills under `~/.omp/agent/managed-skills`,
    /// authored skills under `~/.omp/agent/skills` and `<cwd>/.omp/skills`.
    #[must_use]
    pub fn discover(cwd: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let agent = home.join(".omp").join("agent");
        Self {
            managed: agent.join("managed-skills"),
            authored: vec![agent.join("skills"), cwd.join(".omp").join("skills")],
        }
    }

    /// Explicit layout (tests, embedders).
    #[must_use]
    pub fn at(managed: PathBuf, authored: Vec<PathBuf>) -> Self {
        Self { managed, authored }
    }

    #[must_use]
    pub fn managed_root(&self) -> &Path {
        &self.managed
    }

    /// `SKILL.md` path a managed skill of `name` resolves to.
    #[must_use]
    pub fn path_for(&self, name: &str) -> PathBuf {
        self.managed.join(name).join("SKILL.md")
    }

    /// The authored skill claiming `name`, if any. Authored skills win in
    /// discovery, so a managed skill under a claimed name would never surface.
    #[must_use]
    pub fn authored_claim(&self, name: &str) -> Option<PathBuf> {
        for root in &self.authored {
            for candidate in [
                root.join(name).join("SKILL.md"),
                root.join(name).join("skill.md"),
                root.join(format!("{name}.md")),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Create or replace a managed `SKILL.md`, returning its path.
    pub fn write(
        &self,
        action: SkillAction,
        name: &str,
        description: &str,
        body: &str,
    ) -> Result<PathBuf> {
        let name = sanitize_skill_name(name)?;
        let description = sanitize_skill_description(description);
        if description.is_empty() {
            bail!("Managed skill \"{name}\" needs a non-empty description.");
        }
        let body = body.trim();
        if body.is_empty() {
            bail!("Managed skill \"{name}\" needs a non-empty body.");
        }
        let content = render_skill_file(&name, &description, body);
        let bytes = content.len();
        if bytes > MAX_SKILL_BYTES {
            bail!("Managed skill is {bytes} bytes; the limit is {MAX_SKILL_BYTES}. Trim the body or description.");
        }

        let dir = self.managed.join(&name);
        let file = dir.join("SKILL.md");
        self.assert_root_safe()?;
        if let Ok(meta) = std::fs::symlink_metadata(&dir) {
            if meta.file_type().is_symlink() {
                bail!("Managed skill \"{name}\" resolves through a symlink; refusing to write outside the managed directory.");
            }
        }

        match action {
            SkillAction::Create => {
                std::fs::create_dir_all(&dir)
                    .map_err(|e| anyhow!("could not create {}: {e}", dir.display()))?;
                // `create_new` is the atomic form: it fails when the file
                // already exists, so create/update cannot race each other.
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                let mut handle = match options.open(&file) {
                    Ok(handle) => handle,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        bail!("Managed skill \"{name}\" already exists. Use action \"update\" to change it.")
                    }
                    Err(err) => {
                        return Err(anyhow!("could not create {}: {err}", file.display()));
                    }
                };
                use std::io::Write as _;
                handle
                    .write_all(content.as_bytes())
                    .map_err(|e| anyhow!("could not write {}: {e}", file.display()))?;
            }
            SkillAction::Update => {
                let meta = std::fs::symlink_metadata(&file).map_err(|err| {
                    if err.kind() == std::io::ErrorKind::NotFound {
                        anyhow!("Managed skill \"{name}\" does not exist. Use action \"create\" to add it.")
                    } else {
                        anyhow!("could not stat {}: {err}", file.display())
                    }
                })?;
                if meta.file_type().is_symlink() {
                    bail!(
                        "Managed skill \"{name}\" SKILL.md is a symlink; refusing to overwrite it."
                    );
                }
                if !meta.is_file() {
                    bail!("Managed skill \"{name}\" SKILL.md is not a regular file; refusing to overwrite it.");
                }
                std::fs::write(&file, content.as_bytes())
                    .map_err(|e| anyhow!("could not write {}: {e}", file.display()))?;
            }
            // Deleting is `SkillStore::delete`; callers reject it before
            // reaching the write path, so this arm only guards the invariant.
            SkillAction::Delete => {
                bail!("Managed skill \"{name}\" cannot be written with action \"delete\".")
            }
        }
        Ok(file)
    }

    /// Delete a managed skill directory.
    pub fn delete(&self, name: &str) -> Result<()> {
        let name = sanitize_skill_name(name)?;
        self.assert_root_safe()?;
        let dir = self.managed.join(&name);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("Managed skill \"{name}\" is a symlink; refusing to delete outside the managed directory.");
            }
            Ok(meta) if !meta.is_dir() => {
                bail!("Managed skill \"{name}\" is not a directory; refusing to delete it.");
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("Managed skill \"{name}\" does not exist.");
            }
            Err(err) => return Err(anyhow!("could not stat {}: {err}", dir.display())),
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| anyhow!("could not delete {}: {e}", dir.display()))?;
        Ok(())
    }

    /// A symlinked managed root is allowed only when it resolves to a real
    /// directory; a broken link has no defined write target.
    fn assert_root_safe(&self) -> Result<()> {
        let Ok(meta) = std::fs::symlink_metadata(&self.managed) else {
            return Ok(());
        };
        if !meta.file_type().is_symlink() {
            return Ok(());
        }
        match std::fs::metadata(&self.managed) {
            Ok(target) if target.is_dir() => Ok(()),
            Ok(_) => bail!(
                "The managed-skills root {} links to a non-directory; refusing to write.",
                self.managed.display()
            ),
            Err(_) => bail!(
                "The managed-skills root {} is a broken symlink; refusing to write.",
                self.managed.display()
            ),
        }
    }
}

/// `create` | `update` | `delete`, shared by `manage_skill` and `learn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillAction {
    Create,
    Update,
    Delete,
}

impl SkillAction {
    fn verb(self) -> &'static str {
        match self {
            SkillAction::Create => "Created",
            SkillAction::Update => "Updated",
            SkillAction::Delete => "Deleted",
        }
    }
}

/// Validate and normalize a skill name: lowercased, with spaces and
/// underscores folded into single hyphens, so a human-written name
/// ("Toolchain Pinning") and its kebab-case form address the same skill.
/// Anything outside the allowlist is rejected so a name can never escape the
/// managed directory.
pub fn sanitize_skill_name(raw: &str) -> Result<String> {
    let mut name = String::with_capacity(raw.len());
    let mut last_was_hyphen = false;
    for ch in raw.trim().chars() {
        let ch = match ch.to_ascii_lowercase() {
            ' ' | '_' => '-',
            other => other,
        };
        if ch == '-' {
            if last_was_hyphen {
                continue;
            }
            last_was_hyphen = true;
        } else {
            last_was_hyphen = false;
        }
        name.push(ch);
    }
    let bytes = name.as_bytes();
    let valid_len = !name.is_empty() && name.len() <= SKILL_NAME_MAX;
    let valid_shape = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if !valid_len || !valid_shape {
        bail!(
            "Invalid skill name \"{raw}\". Use lowercase letters, digits, and hyphens (1-64 chars, starting with a letter or digit)."
        );
    }
    Ok(name)
}

/// Neutralize a machine-generated description so it cannot break out of the
/// system prompt's `<skills>` listing: markup and control characters become
/// word separators (a closing tag is dropped whole, since its name is markup
/// rather than prose), `~~~` collapses, and the result is a single line.
#[must_use]
pub fn sanitize_skill_description(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut last_was_tilde = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        index += 1;
        // A closing tag is markup, not prose: drop it whole rather than leaving
        // its name behind as words. An unterminated `</…` degrades to the plain
        // character arm below, so no text is swallowed.
        if ch == '<' && chars.get(index) == Some(&'/') {
            if let Some(close) = chars[index..].iter().position(|c| *c == '>') {
                index += close + 1;
                out.push(' ');
                last_was_tilde = false;
                continue;
            }
        }
        match ch {
            '<' | '>' | '`' => {
                out.push(' ');
                last_was_tilde = false;
            }
            ch if ch.is_control() => {
                out.push(' ');
                last_was_tilde = false;
            }
            '~' => {
                if !last_was_tilde {
                    last_was_tilde = true;
                    out.push('~');
                }
            }
            _ => {
                last_was_tilde = false;
                out.push(ch);
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Serialize a `SKILL.md`: minimal frontmatter plus the body.
fn render_skill_file(name: &str, description: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    let _ = writeln!(out, "name: {name}");
    let _ = writeln!(out, "description: {}", yaml_quote(description));
    out.push_str("---\n\n");
    out.push_str(body.trim());
    out.push('\n');
    out
}

/// Double-quoted YAML scalar with JSON escapes (a YAML double-quoted scalar is
/// JSON-compatible for the characters a sanitized description can hold).
fn yaml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Shared tool plumbing
// ---------------------------------------------------------------------------

/// Everything a memory tool needs, resolved once at construction.
#[derive(Clone)]
pub(crate) struct ToolContext {
    cwd: PathBuf,
    backend: Arc<dyn MemoryToolBackend>,
    skills: Arc<SkillStore>,
}

impl ToolContext {
    fn new(cwd: PathBuf, backend: Arc<dyn MemoryToolBackend>, skills: Arc<SkillStore>) -> Self {
        Self {
            cwd,
            backend,
            skills,
        }
    }
}

fn result_with(text: impl Into<String>, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details,
        terminate: None,
        is_error: None,
    }
}

fn soft_error(text: impl Into<String>, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details,
        terminate: None,
        is_error: Some(true),
    }
}

/// Build the async `AgentTool::execute` body from a sync `run`.
fn boxed_run<T>(
    tool: Arc<T>,
    signal: AbortSignal,
    run: impl FnOnce(&T) -> Result<AgentToolResult> + Send + 'static,
) -> BoxFut<'static, Result<AgentToolResult>>
where
    T: Send + Sync + 'static,
{
    Box::pin(async move {
        if signal.is_aborted() {
            return Err(pa_agent::abort::aborted_error());
        }
        run(&tool)
    })
}

// ---------------------------------------------------------------------------
// retain
// ---------------------------------------------------------------------------

const RETAIN_DESCRIPTION: &str = "Store durable facts in long-term memory. `items[]` each carry `content` (the fact, self-contained) and optional `context` (where it came from). Batch related facts in one call. Do not retain secrets, credentials, or text copied out of a `<memories>` block; recalled memory is never re-retained. `scope: \"global\"` reaches every project, so it needs write approval.";

#[derive(Debug, Clone, Deserialize)]
struct RetainItem {
    content: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RetainParams {
    items: Vec<RetainItem>,
}

/// `retain`: store one or more memories.
pub struct RetainTool {
    ctx: ToolContext,
    schema: Value,
}

impl RetainTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: retain_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: RetainParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid retain arguments: {e}"))?;
        if parsed.items.is_empty() {
            bail!("`items` must hold at least one memory.");
        }

        let mut stored: Vec<String> = Vec::new();
        let total = parsed.items.len();
        for (index, item) in parsed.items.iter().enumerate() {
            let scope = match item.scope.as_deref() {
                None | Some("project") => MemoryScope::Project,
                Some("global") => MemoryScope::Global,
                Some(other) => bail!("`scope` must be \"project\" or \"global\", got \"{other}\""),
            };
            let content = sanitize_memory_text(&item.content);
            if content.is_empty() {
                return Err(retain_failure(
                    index,
                    total,
                    "the content was empty after removing memory blocks",
                    &stored,
                ));
            }
            let context = item
                .context
                .as_deref()
                .map(sanitize_memory_text)
                .filter(|c| !c.is_empty());
            let draft = MemoryDraft {
                content,
                context,
                scope,
                importance: 0.75,
                source: "retain",
            };
            match self.ctx.backend.retain(draft) {
                Ok(id) => stored.push(id),
                Err(err) => return Err(retain_failure(index, total, &err.to_string(), &stored)),
            }
        }

        let count = stored.len();
        let noun = if count == 1 { "memory" } else { "memories" };
        Ok(result_with(
            format!("{count} {noun} stored."),
            json!({"count": count, "ids": stored, "cwd": self.ctx.cwd.display().to_string()}),
        ))
    }
}

/// A partial batch must say exactly what survived, so the caller retries only
/// what failed instead of trusting a success count.
fn retain_failure(index: usize, total: usize, reason: &str, stored: &[String]) -> anyhow::Error {
    let kept = if stored.is_empty() {
        "Nothing was stored.".to_string()
    } else {
        let listed = stored
            .iter()
            .enumerate()
            .map(|(i, id)| format!("item {} (id {id})", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Stored before the failure and kept: {listed}.")
    };
    let untried = if index + 1 < total {
        " Later items were not attempted."
    } else {
        ""
    };
    anyhow!(
        "Did not store item {} of {total}: {reason}. {kept}{untried}",
        index + 1
    )
}

impl AgentTool for RetainTool {
    fn name(&self) -> &'static str {
        "retain"
    }
    fn description(&self) -> &str {
        RETAIN_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Retain"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

/// Parameter schema (mirrors the TS `memoryRetainSchemaWithScope`).
#[must_use]
pub fn retain_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "description": "memories to retain",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "content": {"type": "string", "description": "information to remember"},
                        "context": {"type": "string", "description": "source context"},
                        "scope": {
                            "type": "string",
                            "enum": ["project", "global"],
                            "description": "storage scope; defaults to project, global is for durable cross-project knowledge"
                        }
                    },
                    "required": ["content"]
                }
            }
        },
        "required": ["items"]
    })
}

// ---------------------------------------------------------------------------
// recall
// ---------------------------------------------------------------------------

const RECALL_DESCRIPTION: &str = "Search long-term memory for relevant prior context. `query` is natural language, not a regex. Results are ranked; each memory shows its id (usable with `memory_edit`) and a preview clipped at 500 characters. Empty results mean nothing relevant is stored: do not retry the same query.";

#[derive(Debug, Deserialize)]
struct RecallParams {
    query: String,
    #[serde(default)]
    token_budget: Option<usize>,
}

/// `recall`: ranked search with clipped previews.
pub struct RecallTool {
    ctx: ToolContext,
    schema: Value,
}

impl RecallTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: recall_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: RecallParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid recall arguments: {e}"))?;
        let query = parsed.query.trim();
        if query.is_empty() {
            bail!("`query` must not be empty.");
        }
        let budget = char_budget(parsed.token_budget);
        let hits = self.ctx.backend.recall(query, 50)?;
        Ok(render_recall(&hits, budget))
    }
}

/// Render ranked hits under a character budget, clipping each preview at
/// [`RECALL_PREVIEW_CHARS`].
fn render_recall(hits: &[RecallHit], budget: usize) -> AgentToolResult {
    if hits.is_empty() {
        return result_with("No relevant memories found.", json!({}));
    }
    let mut rendered: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut cut = false;
    let mut ids: Vec<String> = Vec::new();
    for hit in hits {
        let (preview, preview_cut) = clip(&hit.content, RECALL_PREVIEW_CHARS);
        let mut line = format!("- [{}] {preview}", hit.id);
        if let Some(context) = hit.context.as_deref() {
            let (context, _) = clip(context, 120);
            let _ = write!(line, " (context: {context})");
        }
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            cut = true;
            break;
        }
        used += cost;
        cut |= preview_cut;
        ids.push(hit.id.clone());
        rendered.push(line);
    }
    if rendered.is_empty() {
        // A budget too small for even one line still answers with one clipped
        // hit: silence would read as "nothing found".
        let hit = &hits[0];
        let (preview, _) = clip(&hit.content, RECALL_PREVIEW_CHARS.min(budget.max(1)));
        rendered.push(format!("- [{}] {preview}", hit.id));
        ids.push(hit.id.clone());
        cut = true;
    }
    let count = rendered.len();
    let noun = if count == 1 { "memory" } else { "memories" };
    let body = rendered.join("\n");
    result_with(
        format!(
            "Found {count} relevant {noun} (as of {} UTC):\n\n{body}",
            format_utc(now_secs())
        ),
        json!({"count": count, "ids": ids, "cut": cut}),
    )
}

impl AgentTool for RecallTool {
    fn name(&self) -> &'static str {
        "recall"
    }
    fn description(&self) -> &str {
        RECALL_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Recall"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

#[must_use]
pub fn recall_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {"type": "string", "description": "natural language search query"},
            "token_budget": {
                "type": "integer",
                "minimum": 1,
                "description": "approximate token budget for the rendered result"
            }
        },
        "required": ["query"]
    })
}

// ---------------------------------------------------------------------------
// reflect
// ---------------------------------------------------------------------------

const REFLECT_DESCRIPTION: &str = "Synthesize an answer to a question from long-term memory. `query` is the question; `context` sharpens it. Returns the memories the answer must be built from, ranked and clipped; when nothing relevant is stored the call says so instead of inventing an answer.";

#[derive(Debug, Deserialize)]
struct ReflectParams {
    query: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    token_budget: Option<usize>,
}

/// `reflect`: question-shaped recall with optional context.
pub struct ReflectTool {
    ctx: ToolContext,
    schema: Value,
}

impl ReflectTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: reflect_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: ReflectParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid reflect arguments: {e}"))?;
        let query = parsed.query.trim();
        if query.is_empty() {
            bail!("`query` must not be empty.");
        }
        let budget = char_budget(parsed.token_budget);
        let hits = self
            .ctx
            .backend
            .reflect(query, parsed.context.as_deref(), 50)?;
        if hits.is_empty() {
            return Ok(result_with(
                "No relevant information found to reflect on.",
                json!({"count": 0}),
            ));
        }
        let recalled = render_recall(&hits, budget);
        let body = recalled
            .content
            .iter()
            .filter_map(|content| match content {
                pa_agent::types::ToolResultContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(result_with(
            format!("Based on recalled memories:\n\n{body}"),
            json!({"count": hits.len(), "query": query}),
        ))
    }
}

impl AgentTool for ReflectTool {
    fn name(&self) -> &'static str {
        "reflect"
    }
    fn description(&self) -> &str {
        REFLECT_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Reflect"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

#[must_use]
pub fn reflect_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {"type": "string", "description": "question to answer"},
            "context": {"type": "string", "description": "optional context"},
            "token_budget": {
                "type": "integer",
                "minimum": 1,
                "description": "approximate token budget for the rendered result"
            }
        },
        "required": ["query"]
    })
}

// ---------------------------------------------------------------------------
// memory_edit
// ---------------------------------------------------------------------------

const MEMORY_EDIT_DESCRIPTION: &str = "Correct long-term memory by id (ids come from `recall`). `update` rewrites content and/or importance; `forget` removes a wrong or obsolete memory; `invalidate` marks a memory superseded, optionally naming its `replacement_id`. `update` needs `content` or `importance`. Prefer `invalidate` over `forget` when a replacement exists.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EditOp {
    Update,
    Forget,
    Invalidate,
}

#[derive(Debug, Deserialize)]
struct MemoryEditParams {
    op: EditOp,
    id: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default)]
    replacement_id: Option<String>,
}

/// `memory_edit`: update, forget or invalidate one memory.
pub struct MemoryEditTool {
    ctx: ToolContext,
    schema: Value,
}

impl MemoryEditTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: memory_edit_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: MemoryEditParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid memory_edit arguments: {e}"))?;
        let id = parsed.id.trim();
        if id.is_empty() {
            bail!("`id` must not be empty.");
        }
        let content = parsed
            .content
            .as_deref()
            .map(sanitize_memory_text)
            .filter(|c| !c.is_empty());
        if parsed.op == EditOp::Update && content.is_none() && parsed.importance.is_none() {
            bail!("memory_edit update requires content or importance.");
        }
        // TS clamps instead of rejecting: a model that passes 1.5 means "very
        // important", not "fail the call".
        let importance = parsed.importance.map(|value| value.clamp(0.0, 1.0) as f32);

        let outcome = match parsed.op {
            EditOp::Update => self
                .ctx
                .backend
                .update(id, content.as_deref(), importance)?,
            EditOp::Forget => self.ctx.backend.forget(id)?,
            EditOp::Invalidate => self
                .ctx
                .backend
                .invalidate(id, parsed.replacement_id.as_deref())?,
        };

        let text = match outcome {
            EditOutcome::NotFound => format!("Memory {id} was not found."),
            EditOutcome::NotEditable => format!(
                "Memory {id} is a read-only fact; it cannot be edited. Read it with memory://{id}."
            ),
            _ => format!("Memory {id} {}.", outcome.as_str()),
        };
        let details = json!({
            "op": match parsed.op { EditOp::Update => "update", EditOp::Forget => "forget", EditOp::Invalidate => "invalidate" },
            "id": id,
            "status": outcome.as_str(),
        });
        Ok(match outcome {
            EditOutcome::NotFound | EditOutcome::NotEditable => soft_error(text, details),
            _ => result_with(text, details),
        })
    }
}

impl AgentTool for MemoryEditTool {
    fn name(&self) -> &'static str {
        "memory_edit"
    }
    fn description(&self) -> &str {
        MEMORY_EDIT_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Memory Edit"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

#[must_use]
pub fn memory_edit_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "op": {
                "type": "string",
                "enum": ["update", "forget", "invalidate"],
                "description": "memory edit operation"
            },
            "id": {"type": "string", "description": "memory id from recall output"},
            "content": {"type": "string", "description": "replacement content for update"},
            "importance": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "replacement importance for update (0-1)"
            },
            "replacement_id": {"type": "string", "description": "replacement memory id for invalidate"}
        },
        "required": ["op", "id"]
    })
}

// ---------------------------------------------------------------------------
// learn
// ---------------------------------------------------------------------------

const LEARN_DESCRIPTION: &str = "Capture a reusable lesson: `memory` is the durable, self-contained statement (what, when, why), `context` where it came from. With `skill`, the same call also creates or enhances a managed skill (`name` kebab-case, `description` one line, `body` the SKILL.md markdown without frontmatter). Storing the lesson always happens first; a skill failure is reported as a partial outcome.";

#[derive(Debug, Clone, Deserialize)]
struct LearnSkill {
    action: SkillAction,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LearnParams {
    memory: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    skill: Option<LearnSkill>,
}

/// `learn`: persist a lesson, optionally minting a managed skill.
pub struct LearnTool {
    ctx: ToolContext,
    schema: Value,
}

impl LearnTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: learn_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: LearnParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid learn arguments: {e}"))?;
        let memory = sanitize_memory_text(&parsed.memory);
        if memory.is_empty() {
            bail!("Lesson was empty after sanitization; nothing stored.");
        }
        let scope = match parsed.scope.as_deref() {
            None | Some("project") => MemoryScope::Project,
            Some("global") => MemoryScope::Global,
            Some(other) => bail!("`scope` must be \"project\" or \"global\", got \"{other}\""),
        };
        let context = parsed
            .context
            .as_deref()
            .map(sanitize_memory_text)
            .filter(|c| !c.is_empty());

        // 1) The lesson is stored before any skill work, so a skill failure can
        //    never lose it (TS `LearnTool.execute` order).
        let memory_id = self.ctx.backend.retain(MemoryDraft {
            content: memory,
            context,
            scope,
            importance: 0.8,
            source: "learn",
        })?;

        // 2) Optional managed skill.
        let Some(skill) = parsed.skill else {
            return Ok(result_with(
                "Lesson stored.",
                json!({"memory_id": memory_id, "skill": Value::Null}),
            ));
        };
        if skill.action == SkillAction::Delete {
            bail!("`skill.action` for learn must be \"create\" or \"update\".");
        }
        let Some(description) = skill.description.as_deref() else {
            bail!(
                "\"{}\" requires both \"description\" and \"body\".",
                skill.action.verb()
            );
        };
        let Some(body) = skill.body.as_deref() else {
            bail!(
                "\"{}\" requires both \"description\" and \"body\".",
                skill.action.verb()
            );
        };
        let safe_name = match sanitize_skill_name(&skill.name) {
            Ok(name) => name,
            Err(err) => {
                return Ok(soft_error(
                    format!(
                        "Lesson stored. Did not write managed skill \"{}\": {err}",
                        skill.name
                    ),
                    json!({"memory_id": memory_id, "skill": Value::Null}),
                ));
            }
        };
        // A managed skill resolves below any authored skill of the same name, so
        // creating one under a claimed name writes a file that never surfaces.
        if skill.action == SkillAction::Create {
            if let Some(claim) = self.ctx.skills.authored_claim(&safe_name) {
                return Ok(soft_error(
                    format!(
                        "Lesson stored. Did not create managed skill \"{}\": an authored skill of that name already exists ({}), and managed skills cannot override authored ones. Choose a different name.",
                        skill.name,
                        claim.display()
                    ),
                    json!({"memory_id": memory_id, "skill": Value::Null, "shadowed": true}),
                ));
            }
        }
        match self
            .ctx
            .skills
            .write(skill.action, &safe_name, description, body)
        {
            Ok(path) => Ok(result_with(
                format!(
                    "Lesson stored. {} managed skill \"{safe_name}\" ({}).",
                    skill.action.verb(),
                    path.display()
                ),
                json!({
                    "memory_id": memory_id,
                    "skill": safe_name,
                    "path": path.display().to_string(),
                }),
            )),
            // The lesson is already durable: report the partial outcome instead
            // of pretending the whole call failed.
            Err(err) => Ok(soft_error(
                format!("Lesson stored, but the managed skill could not be written: {err}"),
                json!({"memory_id": memory_id, "skill": Value::Null}),
            )),
        }
    }
}

impl AgentTool for LearnTool {
    fn name(&self) -> &'static str {
        "learn"
    }
    fn description(&self) -> &str {
        LEARN_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Learn"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

#[must_use]
pub fn learn_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "memory": {
                "type": "string",
                "description": "the durable, self-contained lesson to remember (what, when, why)"
            },
            "context": {"type": "string", "description": "optional source context for the lesson"},
            "scope": {
                "type": "string",
                "enum": ["project", "global"],
                "description": "storage scope; defaults to project, global is for durable cross-project knowledge"
            },
            "skill": {
                "type": "object",
                "additionalProperties": false,
                "description": "also create or enhance a managed skill in the same call",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "update"]},
                    "name": {"type": "string", "description": "kebab-case skill name"},
                    "description": {
                        "type": "string",
                        "description": "one-line description of when to use the skill"
                    },
                    "body": {
                        "type": "string",
                        "description": "the SKILL.md body in markdown (no frontmatter)"
                    }
                },
                "required": ["action", "name", "description", "body"]
            }
        },
        "required": ["memory"]
    })
}

// ---------------------------------------------------------------------------
// manage_skill
// ---------------------------------------------------------------------------

const MANAGE_SKILL_DESCRIPTION: &str = "Create, update, or delete an isolated managed skill (`~/.omp/agent/managed-skills/<name>/SKILL.md`). `create`/`update` need both `description` and `body`; `delete` needs only `name`. Managed skills never touch authored skills and cannot be created under a name an authored skill already claims. Names are lowercase letters, digits and hyphens.";

#[derive(Debug, Deserialize)]
struct ManageSkillParams {
    action: SkillAction,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// `manage_skill`: direct managed-skill lifecycle.
pub struct ManageSkillTool {
    ctx: ToolContext,
    schema: Value,
}

impl ManageSkillTool {
    #[must_use]
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            schema: manage_skill_schema(),
        }
    }

    fn run(&self, params: &Value) -> Result<AgentToolResult> {
        let parsed: ManageSkillParams = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid manage_skill arguments: {e}"))?;
        let name = sanitize_skill_name(&parsed.name)?;

        if parsed.action == SkillAction::Delete {
            self.ctx.skills.delete(&name)?;
            return Ok(result_with(
                format!("Deleted managed skill \"{name}\"."),
                json!({"action": "delete", "name": name}),
            ));
        }

        let Some(description) = parsed.description.as_deref() else {
            bail!(
                "\"{}\" requires both \"description\" and \"body\".",
                parsed.action.verb()
            );
        };
        let Some(body) = parsed.body.as_deref() else {
            bail!(
                "\"{}\" requires both \"description\" and \"body\".",
                parsed.action.verb()
            );
        };

        if parsed.action == SkillAction::Create {
            if let Some(claim) = self.ctx.skills.authored_claim(&name) {
                return Ok(soft_error(
                    format!(
                        "Cannot create managed skill \"{name}\": an authored skill of that name already exists ({}), and managed skills cannot override authored ones. Choose a different name.",
                        claim.display()
                    ),
                    json!({"action": "create", "name": name, "shadowed": true}),
                ));
            }
        }

        let path = self
            .ctx
            .skills
            .write(parsed.action, &name, description, body)?;
        Ok(result_with(
            format!(
                "{} managed skill \"{name}\" ({}).",
                parsed.action.verb(),
                path.display()
            ),
            json!({
                "action": match parsed.action { SkillAction::Create => "create", SkillAction::Update => "update", SkillAction::Delete => "delete" },
                "name": name,
                "path": path.display().to_string(),
            }),
        ))
    }
}

impl AgentTool for ManageSkillTool {
    fn name(&self) -> &'static str {
        "manage_skill"
    }
    fn description(&self) -> &str {
        MANAGE_SKILL_DESCRIPTION
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn label(&self) -> &'static str {
        "Manage Skill"
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        boxed_run(self, signal, move |tool| tool.run(&params))
    }
}

#[must_use]
pub fn manage_skill_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["create", "update", "delete"]},
            "name": {"type": "string", "description": "kebab-case skill name"},
            "description": {
                "type": "string",
                "description": "one-line description of when to use the skill (required for create/update)"
            },
            "body": {
                "type": "string",
                "description": "the SKILL.md body in markdown, no frontmatter (required for create/update)"
            }
        },
        "required": ["action", "name"]
    })
}

// ---------------------------------------------------------------------------
// Registry, gating and approval
// ---------------------------------------------------------------------------

/// The six tools, ready to register. `settings` gates availability the way
/// `tools::enabled_by_settings` gates the plain built-ins.
#[must_use]
pub fn tools(
    env: &Arc<Env>,
    backend: Arc<dyn MemoryToolBackend>,
    settings: &omp_settings::Settings,
) -> Vec<Arc<dyn AgentTool>> {
    let ctx = ToolContext::new(
        env.cwd.clone(),
        backend,
        Arc::new(SkillStore::discover(&env.cwd)),
    );
    let mut out: Vec<Arc<dyn AgentTool>> = Vec::new();
    let memory_on = memory_enabled(settings);
    if memory_on {
        out.push(Arc::new(RetainTool::new(ctx.clone())));
        out.push(Arc::new(RecallTool::new(ctx.clone())));
        out.push(Arc::new(ReflectTool::new(ctx.clone())));
        out.push(Arc::new(MemoryEditTool::new(ctx.clone())));
    }
    let autolearn_on = settings.get_bool("autolearn.enabled").unwrap_or(false);
    // Auto-learn only ever feeds the memory store and its managed skills, so it
    // registers nothing while no backend is live.
    if autolearn_on && memory_on {
        out.push(Arc::new(LearnTool::new(ctx.clone())));
        out.push(Arc::new(ManageSkillTool::new(ctx)));
    }
    out
}

/// Memory tools exist only with a live backend: `memory.backend` defaults to
/// `"off"`, and the TS `createIf` constructors return `None` for it.
#[must_use]
pub fn memory_enabled(settings: &omp_settings::Settings) -> bool {
    matches!(
        settings.get_string("memory.backend").as_deref(),
        Some("local" | "hindsight" | "mnemopi" | "sharpshooter")
    )
}

/// Approval tier for the memory tools, for the gate in `crate::approval`
/// (`tool_decision`). Writes that reach outside this project cost a write:
/// a `global` retain/learn, any skill mutation, and a `learn` on the `local`
/// file backend. Everything else is a read.
#[must_use]
pub fn approval(name: &str, args: &Value, settings: &omp_settings::Settings) -> Decision {
    let global_scope = |args: &Value| -> bool {
        if args.get("scope").and_then(Value::as_str) == Some("global") {
            return true;
        }
        args.get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("scope").and_then(Value::as_str) == Some("global"))
            })
    };
    match name {
        "retain" => Decision::tier(if global_scope(args) {
            Tier::Write
        } else {
            Tier::Read
        }),
        "learn" => {
            let skill = args.get("skill").is_some_and(|v| !v.is_null());
            let local_backend = settings.get_string("memory.backend").as_deref() == Some("local");
            Decision::tier(if skill || global_scope(args) || local_backend {
                Tier::Write
            } else {
                Tier::Read
            })
        }
        "manage_skill" => Decision::tier(Tier::Write),
        // recall/reflect/memory_edit read and rewrite stored memory only; the
        // store itself is not a file the user edits, so these stay reads.
        _ => Decision::tier(Tier::Read),
    }
}

/// Every schema this module owns, for a registry that validates before
/// dispatch.
#[must_use]
pub fn schemas() -> Vec<(&'static str, Value)> {
    vec![
        ("retain", retain_schema()),
        ("recall", recall_schema()),
        ("reflect", reflect_schema()),
        ("memory_edit", memory_edit_schema()),
        ("learn", learn_schema()),
        ("manage_skill", manage_skill_schema()),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::types::ToolResultContent;
    use serde_json::json;

    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .map(|content| match content {
                ToolResultContent::Text(text) => text.text.clone(),
                ToolResultContent::Image(_) => "<image>".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    struct Harness {
        _dir: tempfile::TempDir,
        backend: Arc<InMemoryBackend>,
        skills: Arc<SkillStore>,
        ctx: ToolContext,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(InMemoryBackend::new());
        let skills = Arc::new(SkillStore::at(
            dir.path().join("managed-skills"),
            vec![
                dir.path().join("user-skills"),
                dir.path().join("project/.omp/skills"),
            ],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf(), backend.clone(), skills.clone());
        Harness {
            _dir: dir,
            backend,
            skills,
            ctx,
        }
    }

    /// Insert `value` at a dotted id, building the nested objects the real
    /// config file has (`memory.backend`, not a flat `memory.backend` key).
    fn insert_setting(node: &mut Value, segments: &[&str], value: Value) {
        let Some((head, rest)) = segments.split_first() else {
            return;
        };
        if rest.is_empty() {
            node[*head] = value;
            return;
        }
        if !node.is_object() {
            *node = json!({});
        }
        insert_setting(&mut node[*head], rest, value);
    }

    fn settings(yaml_pairs: &[(&str, &str)]) -> omp_settings::Settings {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yml");
        // JSON is valid YAML, so a nested object written here is read back as
        // the same dotted settings a hand-written config.yml would produce.
        let mut root = json!({});
        for (key, raw) in yaml_pairs {
            let value: Value = serde_json::from_str(raw).expect("test setting value is JSON");
            let segments: Vec<&str> = key.split('.').collect();
            insert_setting(&mut root, &segments, value);
        }
        std::fs::write(&global, root.to_string()).unwrap();
        let settings =
            omp_settings::Settings::load_from(Some(&global), &dir.path().join("project"));
        drop(dir);
        settings
    }

    // -- helpers ----------------------------------------------------------

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13");
        // Leap day, 2024-02-29T12:34Z.
        assert_eq!(format_utc(1_709_210_040), "2024-02-29 12:34");
    }

    #[test]
    fn clip_is_character_based_and_marks_cuts() {
        assert_eq!(clip("abc", 5), ("abc".to_string(), false));
        assert_eq!(clip("abcdef", 3), ("abc…".to_string(), true));
        // Multi-byte content must not panic or split a character.
        assert_eq!(clip("가나다라", 2), ("가나…".to_string(), true));
        assert_eq!(clip("", 0), (String::new(), false));
    }

    #[test]
    fn tag_blocks_are_stripped_and_unterminated_tags_do_not_eat_text() {
        let input = "keep <memories>drop me</memories> tail";
        assert_eq!(strip_tag_blocks(input, MEMORY_TAGS), "keep  tail");
        assert_eq!(
            strip_tag_blocks("<relevant_memories>a</relevant_memories>ok", MEMORY_TAGS),
            "ok"
        );
        // Unterminated: drop the tag, keep the words.
        assert_eq!(
            strip_tag_blocks("<memories>still here", MEMORY_TAGS),
            "still here"
        );
    }

    #[test]
    fn memory_text_is_canonicalized() {
        assert_eq!(
            sanitize_memory_text("  line one   \n\n\n  line two <memories>x</memories> "),
            "line one\n\n\n  line two"
        );
    }

    #[test]
    fn query_terms_dedupe_and_ignore_short_tokens() {
        assert_eq!(
            query_terms("Deploy the DEPLOY a b pipeline"),
            vec![
                "deploy".to_string(),
                "the".to_string(),
                "pipeline".to_string()
            ]
        );
    }

    // -- retain -----------------------------------------------------------

    #[test]
    fn retain_stores_every_item_and_reports_ids() {
        let h = harness();
        let tool = RetainTool::new(h.ctx.clone());
        let out = tool
            .run(&json!({"items": [
                {"content": "The fleet builds on linux5.", "context": "chat"},
                {"content": "Release tags are vX.Y.Z-seq."}
            ]}))
            .unwrap();
        assert_eq!(text_of(&out), "2 memories stored.");
        assert_eq!(out.details["count"], 2);
        assert_eq!(out.details["ids"].as_array().unwrap().len(), 2);
        assert_eq!(h.backend.len(), 2);
        assert!(out.is_error.is_none());
    }

    #[test]
    fn retain_rejects_empty_items_and_blank_content() {
        let h = harness();
        let tool = RetainTool::new(h.ctx.clone());
        let err = tool.run(&json!({"items": []})).unwrap_err().to_string();
        assert!(err.contains("at least one memory"), "{err}");

        let err = tool
            .run(&json!({"items": [{"content": "   "}]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("item 1 of 1"), "{err}");
        assert!(err.contains("Nothing was stored."), "{err}");
        assert_eq!(h.backend.len(), 0);
    }

    #[test]
    fn retain_never_stores_a_recalled_memories_block() {
        let h = harness();
        let tool = RetainTool::new(h.ctx.clone());
        tool.run(&json!({"items": [{"content": "<memories>prior fact</memories>"}]}))
            .unwrap_err();
        assert!(h.backend.is_empty());
    }

    #[test]
    fn retain_reports_the_kept_prefix_when_a_later_item_fails() {
        struct Failing(Arc<InMemoryBackend>);
        impl MemoryToolBackend for Failing {
            fn retain(&self, draft: MemoryDraft) -> Result<String> {
                if draft.content.contains("boom") {
                    bail!("disk full");
                }
                self.0.retain(draft)
            }
            fn recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
                self.0.recall(query, limit)
            }
            fn update(&self, id: &str, c: Option<&str>, i: Option<f32>) -> Result<EditOutcome> {
                self.0.update(id, c, i)
            }
            fn forget(&self, id: &str) -> Result<EditOutcome> {
                self.0.forget(id)
            }
            fn invalidate(&self, id: &str, r: Option<&str>) -> Result<EditOutcome> {
                self.0.invalidate(id, r)
            }
            fn get(&self, id: &str) -> Result<Option<RetainedMemory>> {
                self.0.get(id)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(InMemoryBackend::new());
        let ctx = ToolContext::new(
            dir.path().to_path_buf(),
            Arc::new(Failing(backend.clone())),
            Arc::new(SkillStore::at(dir.path().join("m"), vec![])),
        );
        let tool = RetainTool::new(ctx);
        let err = tool
            .run(&json!({"items": [
                {"content": "first is fine"},
                {"content": "boom"},
                {"content": "never attempted"}
            ]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("item 2 of 3"), "{err}");
        assert!(err.contains("disk full"), "{err}");
        assert!(err.contains("kept: item 1 (id mem_0001)"), "{err}");
        assert!(err.contains("Later items were not attempted."), "{err}");
        assert_eq!(backend.len(), 1);
    }

    // -- recall -----------------------------------------------------------

    #[test]
    fn recall_ranks_by_overlap_and_reports_ids() {
        let h = harness();
        h.backend
            .insert(memory("mem_a", "release tag workflow uses vX.Y.Z-seq", 0.5));
        h.backend
            .insert(memory("mem_b", "release notes are written in Korean", 0.5));
        let tool = RecallTool::new(h.ctx);
        let out = tool.run(&json!({"query": "release tag workflow"})).unwrap();
        let text = text_of(&out);
        assert!(
            text.starts_with("Found 2 relevant memories (as of "),
            "{text}"
        );
        // mem_a matches three terms, mem_b one.
        let first = text.find("[mem_a]").unwrap();
        let second = text.find("[mem_b]").unwrap();
        assert!(first < second, "{text}");
        assert!(text.contains("vX.Y.Z-seq"), "{text}");
    }

    #[test]
    fn recall_clips_each_preview_at_500_characters() {
        let h = harness();
        let long = "x".repeat(900);
        h.backend.insert(memory("mem_long", &long, 0.5));
        let tool = RecallTool::new(h.ctx);
        let out = tool.run(&json!({"query": "xxx"})).unwrap();
        let text = text_of(&out);
        let preview = text.split("[mem_long] ").nth(1).unwrap();
        // 500 chars of preview plus the ellipsis marker.
        assert_eq!(
            preview.chars().count(),
            RECALL_PREVIEW_CHARS + 1,
            "{preview}"
        );
        assert!(preview.ends_with('…'));
        assert_eq!(out.details["cut"], true);
    }

    #[test]
    fn recall_honours_the_token_budget() {
        let h = harness();
        for i in 0..5 {
            h.backend.insert(memory(
                &format!("mem_{i:04}"),
                &format!("budget filler number {i} {}", "y".repeat(200)),
                0.5,
            ));
        }
        let tool = RecallTool::new(h.ctx);
        let out = tool
            .run(&json!({"query": "budget filler", "token_budget": 100}))
            .unwrap();
        // 100 tokens ≈ 400 chars: fewer than all five fit.
        let rendered = out.details["count"].as_u64().unwrap();
        assert!(rendered < 5, "{}", text_of(&out));
        assert!(rendered >= 1);
        assert_eq!(out.details["cut"], true);
    }

    #[test]
    fn recall_says_so_when_nothing_matches() {
        let h = harness();
        let tool = RecallTool::new(h.ctx);
        let out = tool.run(&json!({"query": "unheard of topic"})).unwrap();
        assert_eq!(text_of(&out), "No relevant memories found.");
        assert_eq!(out.details, json!({}));
    }

    #[test]
    fn recall_rejects_a_blank_query() {
        let h = harness();
        let tool = RecallTool::new(h.ctx);
        assert!(tool.run(&json!({"query": "  "})).is_err());
    }

    #[test]
    fn forgotten_memories_stop_being_recalled() {
        let h = harness();
        let id = h
            .backend
            .insert(memory("mem_x", "temporary deploy detail", 0.5));
        assert!(!h.backend.recall("deploy detail", 10).unwrap().is_empty());
        h.backend.forget(&id).unwrap();
        assert!(h.backend.recall("deploy detail", 10).unwrap().is_empty());
    }

    // -- reflect ----------------------------------------------------------

    #[test]
    fn reflect_composes_context_into_the_query() {
        let h = harness();
        h.backend
            .insert(memory("mem_q", "the answer is forty two", 0.5));
        let tool = ReflectTool::new(h.ctx);
        let out = tool
            .run(&json!({"query": "what is the answer?", "context": "forty two"}))
            .unwrap();
        let text = text_of(&out);
        assert!(text.starts_with("Based on recalled memories:"), "{text}");
        assert!(text.contains("the answer is forty two"), "{text}");
    }

    #[test]
    fn reflect_says_so_when_memory_is_empty() {
        let h = harness();
        let tool = ReflectTool::new(h.ctx);
        let out = tool.run(&json!({"query": "anything at all"})).unwrap();
        assert_eq!(
            text_of(&out),
            "No relevant information found to reflect on."
        );
        assert_eq!(out.details["count"], 0);
    }

    #[test]
    fn reflect_query_composition_matches_the_reference() {
        assert_eq!(compose_reflect_query(" q ", None), "q");
        assert_eq!(compose_reflect_query("q", Some("   ")), "q");
        assert_eq!(
            compose_reflect_query("q", Some(" ctx ")),
            "q\n\nAdditional context:\nctx"
        );
    }

    // -- memory_edit ------------------------------------------------------

    #[test]
    fn memory_edit_updates_content_and_clamps_importance() {
        let h = harness();
        let id = h.backend.insert(memory("mem_u", "old text", 0.2));
        let tool = MemoryEditTool::new(h.ctx.clone());
        let out = tool
            .run(&json!({"op": "update", "id": id, "content": "new text", "importance": 1.8}))
            .unwrap();
        assert_eq!(text_of(&out), "Memory mem_u updated.");
        let stored = h.backend.get("mem_u").unwrap().unwrap();
        assert_eq!(stored.content, "new text");
        assert!((stored.importance - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn memory_edit_update_needs_content_or_importance() {
        let h = harness();
        h.backend.insert(memory("mem_u", "old text", 0.2));
        let tool = MemoryEditTool::new(h.ctx);
        let err = tool
            .run(&json!({"op": "update", "id": "mem_u"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires content or importance"), "{err}");
    }

    #[test]
    fn memory_edit_forget_and_invalidate_record_the_status() {
        let h = harness();
        h.backend.insert(memory("mem_f", "forget me", 0.5));
        h.backend.insert(memory("mem_i", "supersede me", 0.5));
        h.backend.insert(memory("mem_r", "replacement", 0.5));
        let tool = MemoryEditTool::new(h.ctx.clone());

        let out = tool.run(&json!({"op": "forget", "id": "mem_f"})).unwrap();
        assert_eq!(text_of(&out), "Memory mem_f forgotten.");
        assert_eq!(
            h.backend.get("mem_f").unwrap().unwrap().status,
            MemoryStatus::Forgotten
        );

        let out = tool
            .run(&json!({"op": "invalidate", "id": "mem_i", "replacement_id": "mem_r"}))
            .unwrap();
        assert_eq!(text_of(&out), "Memory mem_i invalidated.");
        assert_eq!(
            h.backend.get("mem_i").unwrap().unwrap().status,
            MemoryStatus::Invalidated {
                replacement_id: Some("mem_r".to_string())
            }
        );
    }

    #[test]
    fn memory_edit_reports_missing_and_read_only_memories_as_errors() {
        let h = harness();
        h.backend.insert(memory("mem_d", "dead", 0.5));
        h.backend.forget("mem_d").unwrap();
        let tool = MemoryEditTool::new(h.ctx);

        let missing = tool.run(&json!({"op": "forget", "id": "mem_zzz"})).unwrap();
        assert_eq!(text_of(&missing), "Memory mem_zzz was not found.");
        assert_eq!(missing.is_error, Some(true));

        let read_only = tool
            .run(&json!({"op": "update", "id": "mem_d", "content": "revive"}))
            .unwrap();
        assert!(text_of(&read_only).contains("read-only fact"));
        assert!(text_of(&read_only).contains("memory://mem_d"));
        assert_eq!(read_only.is_error, Some(true));
    }

    // -- learn ------------------------------------------------------------

    #[test]
    fn learn_stores_the_lesson_and_mints_a_skill() {
        let h = harness();
        let tool = LearnTool::new(h.ctx.clone());
        let out = tool
            .run(&json!({
                "memory": "Always pin the toolchain version.",
                "context": "retro",
                "skill": {
                    "action": "create",
                    "name": "Toolchain Pinning",
                    "description": "Use when a build breaks after an upgrade",
                    "body": "# Pinning\n\nPin the toolchain."
                }
            }))
            .unwrap();
        let text = text_of(&out);
        assert!(
            text.starts_with("Lesson stored. Created managed skill \"toolchain-pinning\""),
            "{text}"
        );
        let file = h.skills.path_for("toolchain-pinning");
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.starts_with("---\nname: toolchain-pinning\n"),
            "{content}"
        );
        assert!(content.contains("description: \"Use when a build breaks after an upgrade\""));
        assert!(content.contains("# Pinning"));
        assert_eq!(h.backend.len(), 1);
    }

    #[test]
    fn learn_stores_the_lesson_without_a_skill() {
        let h = harness();
        let tool = LearnTool::new(h.ctx.clone());
        let out = tool.run(&json!({"memory": "A lone lesson."})).unwrap();
        assert_eq!(text_of(&out), "Lesson stored.");
        assert_eq!(out.details["skill"], Value::Null);
        assert_eq!(h.backend.len(), 1);
    }

    #[test]
    fn learn_keeps_the_lesson_when_the_skill_name_is_claimed() {
        let h = harness();
        let claimed = h.skills.authored_claim("taken");
        assert!(claimed.is_none());
        let authored = h._dir.path().join("user-skills/taken/SKILL.md");
        std::fs::create_dir_all(authored.parent().unwrap()).unwrap();
        std::fs::write(&authored, "---\nname: taken\n---\nbody\n").unwrap();

        let tool = LearnTool::new(h.ctx.clone());
        let out = tool
            .run(&json!({
                "memory": "Lesson survives the skill refusal.",
                "skill": {
                    "action": "create",
                    "name": "taken",
                    "description": "d",
                    "body": "b"
                }
            }))
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("Lesson stored."), "{text}");
        assert!(text.contains("cannot override authored ones"), "{text}");
        assert_eq!(out.is_error, Some(true));
        assert_eq!(h.backend.len(), 1);
        assert!(!h.skills.path_for("taken").exists());
    }

    #[test]
    fn learn_rejects_an_empty_lesson_and_a_delete_skill_action() {
        let h = harness();
        let tool = LearnTool::new(h.ctx);
        assert!(tool
            .run(&json!({"memory": "<memories>x</memories>"}))
            .is_err());
        let err = tool
            .run(&json!({
                "memory": "lesson",
                "skill": {"action": "delete", "name": "x", "description": "d", "body": "b"}
            }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be \"create\" or \"update\""), "{err}");
    }

    // -- manage_skill -----------------------------------------------------

    #[test]
    fn manage_skill_creates_updates_and_deletes() {
        let h = harness();
        let tool = ManageSkillTool::new(h.ctx.clone());

        let out = tool
            .run(&json!({
                "action": "create",
                "name": "Release Checks",
                "description": "Use before tagging a release",
                "body": "# Checks\n\nRun the suite."
            }))
            .unwrap();
        assert!(text_of(&out).starts_with("Created managed skill \"release-checks\""));
        let file = h.skills.path_for("release-checks");
        assert!(file.is_file());

        let out = tool
            .run(&json!({
                "action": "update",
                "name": "release-checks",
                "description": "Use before tagging a release",
                "body": "# Checks\n\nRun the suite twice."
            }))
            .unwrap();
        assert!(text_of(&out).starts_with("Updated managed skill"));
        assert!(std::fs::read_to_string(&file).unwrap().contains("twice"));

        let out = tool
            .run(&json!({"action": "delete", "name": "release-checks"}))
            .unwrap();
        assert_eq!(text_of(&out), "Deleted managed skill \"release-checks\".");
        assert!(!file.exists());
    }

    #[test]
    fn manage_skill_requires_description_and_body_outside_delete() {
        let h = harness();
        let tool = ManageSkillTool::new(h.ctx);
        let err = tool
            .run(&json!({"action": "create", "name": "x", "description": "d"}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires both \"description\" and \"body\""),
            "{err}"
        );
        // delete needs only the name.
        assert!(tool
            .run(&json!({"action": "delete", "name": "absent"}))
            .is_err());
    }

    #[test]
    fn manage_skill_rejects_unsafe_names() {
        let h = harness();
        let tool = ManageSkillTool::new(h.ctx);
        // Names are lowercased and space-folded before the allowlist runs, so
        // only characters outside the allowlist are rejected.
        for name in [
            "../escape",
            "with/slash",
            "",
            "-leading",
            "a".repeat(65).as_str(),
        ] {
            let err = tool
                .run(&json!({"action": "delete", "name": name}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("Invalid skill name"), "{name}: {err}");
        }
        assert!(sanitize_skill_name("  Good-Name  ").is_ok_and(|n| n == "good-name"));
        assert!(sanitize_skill_name("UPPER").is_ok_and(|n| n == "upper"));
    }

    #[test]
    fn manage_skill_create_twice_is_an_error_and_update_of_a_missing_skill_too() {
        let h = harness();
        let tool = ManageSkillTool::new(h.ctx);
        let args = json!({
            "action": "create",
            "name": "dup",
            "description": "d",
            "body": "b"
        });
        tool.run(&args).unwrap();
        let err = tool.run(&args).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");

        let err = tool
            .run(&json!({"action": "update", "name": "nope", "description": "d", "body": "b"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn manage_skill_refuses_a_name_claimed_by_an_authored_skill() {
        let h = harness();
        let authored = h._dir.path().join("project/.omp/skills/claimed/SKILL.md");
        std::fs::create_dir_all(authored.parent().unwrap()).unwrap();
        std::fs::write(&authored, "---\nname: claimed\n---\nbody\n").unwrap();

        let tool = ManageSkillTool::new(h.ctx.clone());
        let out = tool
            .run(&json!({
                "action": "create",
                "name": "claimed",
                "description": "d",
                "body": "b"
            }))
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text_of(&out).contains("cannot override authored ones"));
        assert_eq!(out.details["shadowed"], true);
        assert!(!h.skills.path_for("claimed").exists());
    }

    #[test]
    fn manage_skill_caps_the_final_file_size() {
        let h = harness();
        let tool = ManageSkillTool::new(h.ctx.clone());
        let err = tool
            .run(&json!({
                "action": "create",
                "name": "huge",
                "description": "d",
                "body": "z".repeat(MAX_SKILL_BYTES)
            }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("the limit is 64000"), "{err}");
        assert!(!h.skills.path_for("huge").exists());
    }

    #[test]
    fn descriptions_are_sanitized_against_prompt_breakout() {
        assert_eq!(
            sanitize_skill_description("  a\n<b>`c`</b>   ~~d~~  "),
            "a b c ~d~"
        );
        assert_eq!(sanitize_skill_description("   "), "");
        // The rendered file quotes the description, so YAML stays valid.
        let rendered = render_skill_file("n", "say \"hi\": now # not a comment", "b");
        assert!(rendered.contains("description: \"say \\\"hi\\\": now # not a comment\""));
    }

    #[cfg(unix)]
    #[test]
    fn manage_skill_refuses_symlinked_skill_directories() {
        let h = harness();
        let outside = h._dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("SKILL.md"), "keep me").unwrap();
        std::fs::create_dir_all(h.skills.managed_root()).unwrap();
        std::os::unix::fs::symlink(&outside, h.skills.managed_root().join("linked")).unwrap();

        let tool = ManageSkillTool::new(h.ctx);
        let err = tool
            .run(&json!({"action": "delete", "name": "linked"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(outside.join("SKILL.md").is_file());
    }

    // -- registry, gating, approval ---------------------------------------

    #[test]
    fn schemas_declare_the_required_arguments() {
        let by_name: std::collections::HashMap<_, _> = schemas().into_iter().collect();
        assert_eq!(by_name.len(), 6);
        assert_eq!(by_name["retain"]["required"], json!(["items"]));
        assert_eq!(by_name["retain"]["properties"]["items"]["minItems"], 1);
        assert_eq!(by_name["recall"]["required"], json!(["query"]));
        assert_eq!(by_name["reflect"]["required"], json!(["query"]));
        assert_eq!(by_name["memory_edit"]["required"], json!(["op", "id"]));
        assert_eq!(
            by_name["memory_edit"]["properties"]["op"]["enum"],
            json!(["update", "forget", "invalidate"])
        );
        assert_eq!(by_name["learn"]["required"], json!(["memory"]));
        assert_eq!(
            by_name["manage_skill"]["required"],
            json!(["action", "name"])
        );
        assert_eq!(
            by_name["manage_skill"]["properties"]["action"]["enum"],
            json!(["create", "update", "delete"])
        );
    }

    #[test]
    fn memory_tools_exist_only_with_a_live_backend() {
        assert!(!memory_enabled(&settings(&[("memory.backend", "\"off\"")])));
        assert!(memory_enabled(&settings(&[(
            "memory.backend",
            "\"local\""
        )])));
        assert!(memory_enabled(&settings(&[(
            "memory.backend",
            "\"hindsight\""
        )])));
        // Default is off (the schema default), so an empty config hides them.
        assert!(!memory_enabled(&settings(&[])));
    }

    #[test]
    fn approval_charges_writes_only_where_they_escape_the_project() {
        let local = settings(&[("memory.backend", "\"mnemopi\"")]);
        let local_backend = settings(&[("memory.backend", "\"local\"")]);

        assert_eq!(
            approval("recall", &json!({"query": "x"}), &local).tier,
            Tier::Read
        );
        assert_eq!(
            approval("reflect", &json!({"query": "x"}), &local).tier,
            Tier::Read
        );
        assert_eq!(
            approval("memory_edit", &json!({"op": "forget", "id": "m"}), &local).tier,
            Tier::Read
        );
        assert_eq!(
            approval("retain", &json!({"items": [{"content": "c"}]}), &local).tier,
            Tier::Read
        );
        assert_eq!(
            approval(
                "retain",
                &json!({"items": [{"content": "c", "scope": "global"}]}),
                &local
            )
            .tier,
            Tier::Write
        );
        assert_eq!(
            approval("learn", &json!({"memory": "m"}), &local).tier,
            Tier::Read
        );
        assert_eq!(
            approval("learn", &json!({"memory": "m", "scope": "global"}), &local).tier,
            Tier::Write
        );
        assert_eq!(
            approval("learn", &json!({"memory": "m"}), &local_backend).tier,
            Tier::Write
        );
        assert_eq!(
            approval(
                "learn",
                &json!({"memory": "m", "skill": {"action": "create", "name": "n"}}),
                &local
            )
            .tier,
            Tier::Write
        );
        assert_eq!(
            approval(
                "manage_skill",
                &json!({"action": "delete", "name": "n"}),
                &local
            )
            .tier,
            Tier::Write
        );
    }

    #[test]
    fn tools_are_registered_under_their_omp_names() {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(Env::with_mode(dir.path(), pi_edit::EditMode::Replace));
        let backend: Arc<dyn MemoryToolBackend> = Arc::new(InMemoryBackend::new());
        let enabled = settings(&[
            ("memory.backend", "\"mnemopi\""),
            ("autolearn.enabled", "true"),
        ]);
        let names: Vec<String> = tools(&env, backend.clone(), &enabled)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "retain",
                "recall",
                "reflect",
                "memory_edit",
                "learn",
                "manage_skill"
            ]
        );

        let memory_only = settings(&[("memory.backend", "\"mnemopi\"")]);
        let names: Vec<String> = tools(&env, backend.clone(), &memory_only)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, vec!["retain", "recall", "reflect", "memory_edit"]);

        let off = settings(&[("autolearn.enabled", "true")]);
        assert!(tools(&env, backend, &off).is_empty());
    }

    #[test]
    fn tool_labels_and_descriptions_are_stable() {
        let h = harness();
        let retain = RetainTool::new(h.ctx.clone());
        assert_eq!(retain.name(), "retain");
        assert_eq!(retain.label(), "Retain");
        assert!(!retain.description().is_empty());
        assert_eq!(RecallTool::new(h.ctx.clone()).label(), "Recall");
        assert_eq!(ReflectTool::new(h.ctx.clone()).label(), "Reflect");
        assert_eq!(MemoryEditTool::new(h.ctx.clone()).label(), "Memory Edit");
        assert_eq!(LearnTool::new(h.ctx.clone()).label(), "Learn");
        assert_eq!(ManageSkillTool::new(h.ctx).label(), "Manage Skill");
    }

    #[test]
    fn abort_before_dispatch_fails_the_call() {
        let h = harness();
        let tool: Arc<RetainTool> = Arc::new(RetainTool::new(h.ctx.clone()));
        let controller = pa_agent::abort::AbortController::new();
        controller.abort();
        let fut = AgentTool::execute(
            tool,
            "call-1".to_string(),
            json!({"items": [{"content": "c"}]}),
            controller.signal(),
            Arc::new(|_| {}),
        );
        let err = futures_lite_block_on(fut).unwrap_err();
        assert!(pa_agent::abort::is_abort_error(&err));
        assert!(h.backend.is_empty());
    }

    /// Minimal executor for the one abort test: the future never awaits, so a
    /// single poll either resolves or the test is wrong. The future arrives
    /// already pinned (`BoxFut`), so no `Pin::new_unchecked` is needed, and the
    /// waker comes from the safe `Wake` trait rather than a raw vtable.
    fn futures_lite_block_on<T>(mut fut: pa_agent::BoxFut<'static, T>) -> T {
        use std::task::{Context, Poll, Wake, Waker};
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("tool future must resolve without awaiting"),
        }
    }

    // -- fixtures ---------------------------------------------------------

    fn memory(id: &str, content: &str, importance: f32) -> RetainedMemory {
        RetainedMemory {
            id: id.to_string(),
            content: content.to_string(),
            context: None,
            scope: MemoryScope::Project,
            importance,
            created_at: now_secs(),
            updated_at: now_secs(),
            status: MemoryStatus::Active,
        }
    }
}
