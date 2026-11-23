//! OmOMeow memory repository discovery and identity derivation.
//!
//! Mirrors the gateway's `src/memory-sync.ts:30-48` (`memoryAgentIds` /
//! `memoryRepos`) and `mhc`'s `MAHO_MEMORY_HOME` lookup, falling back to
//! `OMO_MEMORY_HOME` and `~/.omo/memory`. Discovery never creates directories
//! or initializes repositories on disk.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::repo::MemoryRepo;

pub const MAHO_MEMORY_HOME_ENV: &str = "MAHO_MEMORY_HOME";
pub const OMO_MEMORY_HOME_ENV: &str = "OMO_MEMORY_HOME";
pub const MEMORY_DIR_ENV: &str = "MEMORY_DIR";
pub const AGENT_ID_ENV: &str = "AGENT_ID";

pub const AGENTS_DIRNAME: &str = "agents";
pub const REPO_DIRNAME: &str = "repo";
pub const FALLBACK_SLUG: &str = "agent";
pub const MAX_SLUG_CHARS: usize = 40;
pub const SHORT_HASH_LEN: usize = 8;

/// Primary (`<slug>-<hash8>`) and ASCII legacy (`<ascii>-<hash8>`) agent ids
/// derived from a workspace path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryAgentIds {
    pub id: String,
    pub legacy: String,
    pub slug: String,
    pub hash: String,
}

/// Bound OmOMeow memory repository for a session.
#[derive(Debug, Clone)]
pub struct BoundMemoryRepo {
    pub repo: MemoryRepo,
    pub root: PathBuf,
    pub agent_id: String,
    pub from_env: bool,
    pub used_legacy: bool,
}

/// Outcome of probing for a bound memory repository (never creates files).
#[derive(Debug, Clone)]
pub enum MemoryRepoLookup {
    Found(BoundMemoryRepo),
    NotFound {
        primary: PathBuf,
        legacy: PathBuf,
        reason: String,
    },
}

/// Lexically resolve `path` against `base` (`node:path.resolve` semantics).
#[must_use]
pub fn resolve_from(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_lexical(&joined)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn non_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^\p{L}\p{M}\p{N}]+").expect("valid unicode slug regex"))
}

fn collapse_and_trim_dashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_dash = false;
    for ch in value.chars() {
        if ch == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

fn fold_base_name(name: &str) -> String {
    name.nfkd()
        .filter(|&ch| !('\u{0300}'..='\u{036f}').contains(&ch))
        .collect::<String>()
        .to_lowercase()
}

/// Unicode-preserving slug matching `omo-nyang/src/memory-sync.ts:34`.
#[must_use]
pub fn slug_from_base(base: &str) -> String {
    let folded = fold_base_name(base);
    let replaced = non_word_re().replace_all(&folded, "-");
    let nfc: String = replaced.nfc().collect();
    let trimmed = collapse_and_trim_dashes(&nfc);
    let capped: String = trimmed.chars().take(MAX_SLUG_CHARS).collect();
    let final_slug = capped.trim_end_matches('-');
    if final_slug.is_empty() {
        FALLBACK_SLUG.to_string()
    } else {
        final_slug.to_string()
    }
}

/// ASCII-only legacy slug matching `omo-nyang/src/memory-sync.ts:35`.
#[must_use]
pub fn legacy_slug_from_base(base: &str) -> String {
    let folded = fold_base_name(base);
    let mut dashed = String::with_capacity(folded.len());
    let mut in_sep = false;
    for ch in folded.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            dashed.push(ch);
            in_sep = false;
        } else if !in_sep {
            dashed.push('-');
            in_sep = true;
        }
    }
    let trimmed = dashed.trim_matches('-');
    let capped: String = trimmed.chars().take(MAX_SLUG_CHARS).collect();
    let final_slug = capped.trim_end_matches('-');
    if final_slug.is_empty() {
        FALLBACK_SLUG.to_string()
    } else {
        final_slug.to_string()
    }
}

/// First 8 lowercase hex characters of `sha256(input)`.
#[must_use]
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(SHORT_HASH_LEN);
    for byte in &digest[..SHORT_HASH_LEN / 2] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Compute `{ id, legacy }` from `cwd`, matching `omo-nyang/src/memory-sync.ts:29-37`.
#[must_use]
pub fn memory_agent_ids(cwd: &Path) -> MemoryAgentIds {
    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    memory_agent_ids_from(&base_dir, cwd)
}

/// [`memory_agent_ids`] with an explicit base directory for relative `cwd`s.
#[must_use]
pub fn memory_agent_ids_from(base_dir: &Path, cwd: &Path) -> MemoryAgentIds {
    let resolved = resolve_from(base_dir, cwd);
    let resolved_str = resolved.to_string_lossy();
    let hash = short_hash(&resolved_str);
    let base_name = resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug = slug_from_base(&base_name);
    let legacy_slug = legacy_slug_from_base(&base_name);
    MemoryAgentIds {
        id: format!("{slug}-{hash}"),
        legacy: format!("{legacy_slug}-{hash}"),
        slug,
        hash,
    }
}

/// `~/.omo/memory` using the process home environment (`HOME` -> `USERPROFILE`).
#[must_use]
pub fn default_memory_home_with<F>(env: &F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = env(key) {
            if !value.trim().is_empty() {
                return PathBuf::from(value).join(".omo").join("memory");
            }
        }
    }
    if let (Some(drive), Some(path)) = (env("HOMEDRIVE"), env("HOMEPATH")) {
        if !path.trim().is_empty() {
            return PathBuf::from(format!("{drive}{path}"))
                .join(".omo")
                .join("memory");
        }
    }
    PathBuf::from(".").join(".omo").join("memory")
}

/// Resolve the memory home: `MAHO_MEMORY_HOME`, else `OMO_MEMORY_HOME`, else `~/.omo/memory`.
#[must_use]
pub fn resolve_memory_home_with<F>(cwd: &Path, env: &F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    for key in [MAHO_MEMORY_HOME_ENV, OMO_MEMORY_HOME_ENV] {
        if let Some(value) = env(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return resolve_from(cwd, Path::new(trimmed));
            }
        }
    }
    default_memory_home_with(env)
}

/// Discover and open the memory repository for `cwd` without creating anything on disk.
#[must_use]
pub fn discover_memory_repo(cwd: &Path) -> MemoryRepoLookup {
    discover_memory_repo_with(cwd, &|key| std::env::var(key).ok())
}

/// [`discover_memory_repo`] with an injected environment lookup (for deterministic tests).
#[must_use]
pub fn discover_memory_repo_with<F>(cwd: &Path, env: &F) -> MemoryRepoLookup
where
    F: Fn(&str) -> Option<String>,
{
    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved_cwd = resolve_from(&base_dir, cwd);
    let ids = memory_agent_ids_from(&base_dir, &resolved_cwd);
    let agent_id = env(AGENT_ID_ENV)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ids.id.clone());

    if let Some(dir) = env(MEMORY_DIR_ENV) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            let root = resolve_from(&resolved_cwd, Path::new(trimmed));
            return match MemoryRepo::open(&root, &agent_id) {
                Ok(repo) => MemoryRepoLookup::Found(BoundMemoryRepo {
                    repo,
                    root,
                    agent_id,
                    from_env: true,
                    used_legacy: false,
                }),
                Err(err) => MemoryRepoLookup::NotFound {
                    primary: root.clone(),
                    legacy: root,
                    reason: err.to_string(),
                },
            };
        }
    }

    let memory_home = resolve_memory_home_with(&resolved_cwd, env);
    let agents_dir = memory_home.join(AGENTS_DIRNAME);
    let primary = agents_dir.join(&ids.id).join(REPO_DIRNAME);
    let legacy = agents_dir.join(&ids.legacy).join(REPO_DIRNAME);

    if primary.join(".git").exists() {
        match MemoryRepo::open(&primary, &agent_id) {
            Ok(repo) => {
                return MemoryRepoLookup::Found(BoundMemoryRepo {
                    repo,
                    root: primary,
                    agent_id,
                    from_env: false,
                    used_legacy: false,
                });
            }
            Err(err) => {
                if legacy == primary || !legacy.join(".git").exists() {
                    return MemoryRepoLookup::NotFound {
                        primary,
                        legacy,
                        reason: err.to_string(),
                    };
                }
            }
        }
    }

    if legacy != primary && legacy.join(".git").exists() {
        match MemoryRepo::open(&legacy, &agent_id) {
            Ok(repo) => {
                return MemoryRepoLookup::Found(BoundMemoryRepo {
                    repo,
                    root: legacy,
                    agent_id,
                    from_env: false,
                    used_legacy: true,
                });
            }
            Err(err) => {
                return MemoryRepoLookup::NotFound {
                    primary,
                    legacy,
                    reason: err.to_string(),
                };
            }
        }
    }

    MemoryRepoLookup::NotFound {
        primary,
        legacy,
        reason: "memory repository does not exist".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;

    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    fn init_git(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn known_gateway_path_rules_match_memory_sync() {
        let omt = memory_agent_ids(Path::new("/home/whrho/git/oh-my-tau"));
        assert_eq!(omt.id, "oh-my-tau-3eb9c32a");
        assert_eq!(omt.legacy, "oh-my-tau-3eb9c32a");

        assert_eq!(
            memory_agent_ids(Path::new("/home/whrho/git/oh-my-tau/")).id,
            "oh-my-tau-3eb9c32a"
        );
        assert_eq!(
            memory_agent_ids(Path::new("/home/whrho/git/./oh-my-tau")).id,
            "oh-my-tau-3eb9c32a"
        );

        let korean = memory_agent_ids(Path::new("/tmp/한글 폴더"));
        assert_eq!(korean.id, "한글-폴더-f318a4b4");
        assert_eq!(korean.legacy, "agent-f318a4b4");

        let symbols = memory_agent_ids(Path::new("/tmp/---!!!___"));
        assert_eq!(symbols.id, "agent-eedec2be");
        assert_eq!(symbols.legacy, "agent-eedec2be");

        assert_eq!(
            memory_agent_ids(Path::new("/tmp/Café Été 2026")).id,
            "cafe-ete-2026-5929204b"
        );
        let overlong = format!("/tmp/{}", "가".repeat(45));
        assert_eq!(
            memory_agent_ids(Path::new(&overlong)).id,
            format!("{}-65f10f3c", "가".repeat(40))
        );
        assert_eq!(memory_agent_ids(Path::new("/")).id, "agent-8a5edab2");
    }

    #[test]
    fn memory_home_prefers_maho_then_omo_then_home_dot_omo_memory() {
        let cwd = Path::new("/work/proj");
        assert_eq!(
            resolve_memory_home_with(
                cwd,
                &env_map(&[
                    ("MAHO_MEMORY_HOME", "/maho/mem"),
                    ("OMO_MEMORY_HOME", "/omo/mem"),
                    ("HOME", "/home/u"),
                ])
            ),
            PathBuf::from("/maho/mem")
        );
        assert_eq!(
            resolve_memory_home_with(
                cwd,
                &env_map(&[
                    ("MAHO_MEMORY_HOME", "   "),
                    ("OMO_MEMORY_HOME", "rel-mem"),
                    ("HOME", "/home/u"),
                ])
            ),
            PathBuf::from("/work/proj/rel-mem")
        );
        assert_eq!(
            resolve_memory_home_with(cwd, &env_map(&[("HOME", "/home/u")])),
            PathBuf::from("/home/u/.omo/memory")
        );
    }

    #[test]
    fn discovers_primary_or_legacy_repo_and_creates_nothing_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let mem_home = tmp.path().join("mem-home");
        let project = tmp.path().join("한글 프로젝트");
        std::fs::create_dir_all(&project).unwrap();
        let ids = memory_agent_ids(&project);
        assert_ne!(ids.id, ids.legacy);

        let env = env_map(&[
            ("MAHO_MEMORY_HOME", mem_home.to_str().unwrap()),
            ("HOME", tmp.path().to_str().unwrap()),
        ]);

        let missing = discover_memory_repo_with(&project, &env);
        assert!(matches!(missing, MemoryRepoLookup::NotFound { .. }));
        assert!(!mem_home.exists(), "discovery must not create memory home");

        let legacy_repo = mem_home.join("agents").join(&ids.legacy).join("repo");
        init_git(&legacy_repo);
        let MemoryRepoLookup::Found(found) = discover_memory_repo_with(&project, &env) else {
            panic!("expected legacy repo match");
        };
        assert!(found.used_legacy);
        assert!(!found.from_env);
        assert_eq!(found.agent_id, ids.id);
        assert_eq!(found.repo.identity(), ids.id);
        assert_eq!(
            std::fs::canonicalize(&found.root).unwrap(),
            std::fs::canonicalize(&legacy_repo).unwrap()
        );

        let primary_repo = mem_home.join("agents").join(&ids.id).join("repo");
        init_git(&primary_repo);
        let MemoryRepoLookup::Found(found_primary) = discover_memory_repo_with(&project, &env)
        else {
            panic!("expected primary repo match");
        };
        assert!(!found_primary.used_legacy);
        assert_eq!(
            std::fs::canonicalize(&found_primary.root).unwrap(),
            std::fs::canonicalize(&primary_repo).unwrap()
        );
    }

    #[test]
    fn explicit_memory_dir_and_agent_id_override_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("explicit-repo");
        init_git(&explicit);
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let env = env_map(&[
            ("MEMORY_DIR", explicit.to_str().unwrap()),
            ("AGENT_ID", "custom-agent"),
        ]);
        let MemoryRepoLookup::Found(found) = discover_memory_repo_with(&project, &env) else {
            panic!("expected explicit MEMORY_DIR match");
        };
        assert!(found.from_env);
        assert_eq!(found.agent_id, "custom-agent");
        assert_eq!(found.repo.identity(), "custom-agent");
    }
}
