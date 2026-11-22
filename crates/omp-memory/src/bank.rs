//! Canonical project-bank identity and conservative legacy-bank adoption.

use std::{
    collections::HashSet,
    fmt::{self, Display},
    fs, io,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{Error, Result, config::BankScoping};

/// Shared bank used when no bank is configured.
pub const DEFAULT_SHARED_BANK: &str = "default";
/// Maximum accepted bank-name bytes.
pub const MAX_BANK_BYTES: usize = 64;
const LEGACY_SCAN_LIMIT: usize = 64;
const BANK_HASH_CHARS: usize = 12;

/// Validated Mnemopi bank identifier.
///
/// Bank names are the only user-controlled path component in the memory data
/// directory, so they are restricted to `[A-Za-z0-9_-]` and bounded to
/// [`MAX_BANK_BYTES`]. Over-long names keep a content-derived digest suffix
/// instead of being truncated into a collision.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BankId(String);

impl BankId {
    /// Validates and normalizes a user-configured bank name.
    pub fn configured(value: &str) -> Result<Self> {
        sanitize(value).map(Self).ok_or(Error::InvalidIdentifier)
    }

    /// Wraps an already-validated bank name.
    ///
    /// Only for names this module produced (project banks, adopted banks).
    /// Prefer [`BankId::configured`] for anything user-supplied.
    pub(crate) fn trusted(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Display for BankId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for BankId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Caller-supplied identity facts consumed by bank selection.
///
/// Memory code never probes Git itself: linked worktrees must receive the same
/// canonical primary root so they share one bank.
pub struct BankScopeInput<'a> {
    /// Canonical primary repository root, when the workspace is a repository.
    pub canonical_primary_root: Option<&'a Path>,
    /// Canonical selected workspace root, used only outside a repository.
    pub workspace_root: &'a Path,
    /// Optional configured shared bank base.
    pub configured_bank: Option<&'a str>,
    /// Requested write/recall scoping.
    pub scoping: BankScoping,
}

/// Resolved durable write bank and ordered recall-bank fallback set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BankScope {
    /// Stable root identity that was hashed.
    pub identity_root: PathBuf,
    /// Shared bank.
    pub global: BankId,
    /// Bank receiving new durable rows.
    pub retain: BankId,
    /// Ordered banks searched by recall.
    pub recall: Vec<BankId>,
    /// Applied scoping mode.
    pub scoping: BankScoping,
}

impl BankScope {
    /// Resolves bank identity exclusively from caller-supplied roots.
    pub fn resolve(input: &BankScopeInput<'_>) -> Result<Self> {
        let identity_root =
            input.canonical_primary_root.unwrap_or(input.workspace_root).to_path_buf();
        let global = match input.configured_bank {
            Some(value) => BankId::configured(value)?,
            None => BankId::trusted(DEFAULT_SHARED_BANK),
        };
        let project = project_bank(&identity_root, input.configured_bank)?;
        let (retain, recall) = match input.scoping {
            BankScoping::Global => (global.clone(), vec![global.clone()]),
            BankScoping::PerProject => (project.clone(), vec![project]),
            BankScoping::PerProjectTagged if project == global => (project.clone(), vec![project]),
            BankScoping::PerProjectTagged => (project.clone(), vec![project, global.clone()]),
        };
        Ok(Self { identity_root, global, retain, recall, scoping: input.scoping })
    }

    /// Adds already-validated adopted legacy banks after the normal recall
    /// order, preserving the first occurrence of every bank.
    pub fn append_adopted(&mut self, adopted: impl IntoIterator<Item = BankId>) {
        let mut seen = self.recall.iter().cloned().collect::<HashSet<_>>();
        for bank in adopted {
            if seen.insert(bank.clone()) {
                self.recall.push(bank);
            }
        }
    }
}

/// Returns the isolated SQLite file for `bank`; the shared bank alone owns the
/// primary DB, every other bank gets its own directory.
pub fn database_path(db_dir: &Path, global: &BankId, bank: &BankId) -> PathBuf {
    if bank == global {
        db_dir.join("mnemopi.db")
    } else {
        db_dir.join("banks").join(bank.as_str()).join("mnemopi.db")
    }
}

/// Conservatively discovers legacy single-root banks.
///
/// Every durable row must name exactly the active canonical identity or
/// selected workspace root. Missing, mixed, unreadable, and corrupt banks are
/// ignored. Discovery is bounded to 64 bank directories and never adopts based
/// on a basename alone.
pub fn discover_legacy_banks(
    db_dir: &Path,
    resolved: &[BankId],
    identity_root: &Path,
    workspace_root: &Path,
) -> Result<Vec<BankId>> {
    let banks_dir = db_dir.join("banks");
    let entries = match fs::read_dir(banks_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let have = resolved.iter().map(BankId::as_str).collect::<HashSet<_>>();
    let identity = identity_root.to_string_lossy();
    let workspace = workspace_root.to_string_lossy();
    let mut adopted = Vec::new();
    let mut scanned = 0usize;
    for entry in entries {
        let entry = entry?;
        if scanned == LEGACY_SCAN_LIMIT {
            break;
        }
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if have.contains(name) {
            continue;
        }
        scanned += 1;
        let Ok(bank) = BankId::configured(name) else {
            continue;
        };
        let path = entry.path().join("mnemopi.db");
        if legacy_bank_matches(&path, identity.as_ref(), workspace.as_ref()) {
            adopted.push(bank);
        }
    }
    adopted.sort_unstable();
    Ok(adopted)
}

fn legacy_bank_matches(path: &Path, identity: &str, workspace: &str) -> bool {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let Ok(connection) = Connection::open_with_flags(path, flags) else {
        return false;
    };
    let recorded = connection
        .query_row("SELECT identity_root FROM bank_scope WHERE singleton = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional();
    if let Ok(Some(recorded)) = recorded {
        return recorded == identity;
    }
    let query = "SELECT COUNT(*) AS total,\nSUM(CASE WHEN json_extract(metadata_json, \
                 '$.primary_root') = ?1\nOR json_extract(metadata_json, '$.cwd') = ?2 THEN 1 ELSE \
                 0 END) AS matching\nFROM working_memory";
    connection
        .query_row(query, (identity, workspace), |row| {
            let total = row.get::<_, i64>(0)?;
            let matching = row.get::<_, i64>(1)?;
            Ok(total > 0 && matching == total)
        })
        .unwrap_or(false)
}

/// Derives the canonical project bank name for one root identity.
///
/// The name is `<basename>-<12 hex of SHA-256(root)>`, prefixed by the
/// configured bank base when present. Two checkouts with the same basename in
/// different locations therefore never share a bank.
pub fn project_bank(root: &Path, configured: Option<&str>) -> Result<BankId> {
    let project = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(sanitize)
        .unwrap_or_else(|| DEFAULT_SHARED_BANK.to_owned());
    let digest = short_hash(&root.to_string_lossy());
    let raw = match configured.and_then(sanitize) {
        Some(base) => format!("{base}-{project}-{digest}"),
        None => format!("{project}-{digest}"),
    };
    Ok(BankId::trusted(limit(&raw)))
}

fn short_hash(material: &str) -> String {
    let digest = Sha256::digest(material.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex.truncate(BANK_HASH_CHARS);
    hex
}

fn sanitize(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut output = String::with_capacity(value.len().min(MAX_BANK_BYTES));
    let mut last_dash = false;
    for byte in value.bytes() {
        let valid = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
        if valid {
            output.push(char::from(byte));
            last_dash = byte == b'-';
        } else if !last_dash && !output.is_empty() {
            output.push('-');
            last_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        None
    } else {
        Some(limit(&output))
    }
}

fn limit(name: &str) -> String {
    if name.len() <= MAX_BANK_BYTES {
        return name.to_owned();
    }
    let digest = short_hash(name);
    let prefix_bytes = MAX_BANK_BYTES - digest.len() - 1;
    let mut prefix = name[..prefix_bytes].trim_end_matches('-').to_owned();
    if prefix.is_empty() {
        prefix.push_str("bank");
    }
    format!("{prefix}-{digest}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scope(root: &str, configured: Option<&str>, scoping: BankScoping) -> BankScope {
        let root = PathBuf::from(root);
        BankScope::resolve(&BankScopeInput {
            canonical_primary_root: Some(&root),
            workspace_root: &root,
            configured_bank: configured,
            scoping,
        })
        .expect("scope")
    }

    #[test]
    fn per_project_scoping_retains_and_recalls_only_the_project_bank() {
        let resolved = scope("/srv/work/alpha", None, BankScoping::PerProject);
        assert_eq!(resolved.global.as_str(), DEFAULT_SHARED_BANK);
        assert_ne!(resolved.retain, resolved.global);
        assert_eq!(resolved.recall, vec![resolved.retain.clone()]);
        assert!(resolved.retain.as_str().starts_with("alpha-"));
        assert_eq!(resolved.identity_root, PathBuf::from("/srv/work/alpha"));
    }

    #[test]
    fn global_scoping_ignores_project_identity() {
        let resolved = scope("/srv/work/alpha", None, BankScoping::Global);
        assert_eq!(resolved.retain, resolved.global);
        assert_eq!(resolved.recall, vec![resolved.global.clone()]);
    }

    #[test]
    fn tagged_scoping_recalls_project_first_then_shared() {
        let resolved = scope("/srv/work/alpha", None, BankScoping::PerProjectTagged);
        assert_eq!(resolved.recall.len(), 2);
        assert_eq!(resolved.recall[0], resolved.retain);
        assert_eq!(resolved.recall[1], resolved.global);
    }

    #[test]
    fn distinct_roots_with_the_same_basename_never_share_a_bank() {
        let left = scope("/srv/one/alpha", None, BankScoping::PerProject);
        let right = scope("/srv/two/alpha", None, BankScoping::PerProject);
        assert_ne!(left.retain, right.retain);
    }

    #[test]
    fn configured_bank_prefixes_the_project_bank_and_names_the_shared_bank() {
        let resolved = scope("/srv/work/alpha", Some("team"), BankScoping::PerProjectTagged);
        assert_eq!(resolved.global.as_str(), "team");
        assert!(resolved.retain.as_str().starts_with("team-alpha-"));
        assert_eq!(resolved.recall[1].as_str(), "team");
    }

    #[test]
    fn workspace_root_is_the_identity_outside_a_repository() {
        let workspace = PathBuf::from("/srv/plain");
        let resolved = BankScope::resolve(&BankScopeInput {
            canonical_primary_root: None,
            workspace_root: &workspace,
            configured_bank: None,
            scoping: BankScoping::PerProject,
        })
        .expect("scope");
        assert_eq!(resolved.identity_root, workspace);
    }

    #[test]
    fn bank_names_are_sanitized_bounded_and_digest_suffixed() {
        assert_eq!(BankId::configured("  my bank!! ").expect("bank").as_str(), "my-bank");
        assert!(BankId::configured("   ").is_err());
        assert!(BankId::configured("...").is_err());
        let long = "a".repeat(200);
        let bank = BankId::configured(&long).expect("bank");
        assert_eq!(bank.as_str().len(), MAX_BANK_BYTES);
        assert_ne!(bank.as_str(), BankId::configured(&"a".repeat(199)).expect("bank").as_str());
    }

    #[test]
    fn shared_bank_owns_the_primary_database_file() {
        let dir = PathBuf::from("/data/mnemopi");
        let global = BankId::configured("default").expect("bank");
        let project = BankId::configured("alpha-0123456789ab").expect("bank");
        assert_eq!(database_path(&dir, &global, &global), dir.join("mnemopi.db"));
        assert_eq!(
            database_path(&dir, &global, &project),
            dir.join("banks").join("alpha-0123456789ab").join("mnemopi.db")
        );
    }

    #[test]
    fn adopted_banks_append_without_duplicating_recall_order() {
        let mut resolved = scope("/srv/work/alpha", None, BankScoping::PerProjectTagged);
        let extra = BankId::configured("legacy").expect("bank");
        resolved.append_adopted(vec![resolved.global.clone(), extra.clone()]);
        assert_eq!(resolved.recall.len(), 3);
        assert_eq!(resolved.recall[2], extra);
    }
}
