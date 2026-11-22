//! Cross-session MCP tool cache, a port of omp's tool-list caching.
//!
//! `tools/list` results are persisted to a JSON file (default
//! `~/.omp/agent/cache/mcp-tools.json`) keyed by server name. Every entry
//! records a hash of the connection-relevant server config and the cache
//! format version, so a changed command/url/header or a format bump
//! invalidates it automatically. Entries older than the TTL are ignored.
//! [`crate::McpManager`] serves cached schemas before any server is
//! connected and refreshes the entry on every successful connect.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::McpServerConfig;
use crate::ToolDefinition;

/// On-disk format version; bump to invalidate every existing entry.
pub const CACHE_VERSION: u32 = 1;

/// Default entry lifetime (7 days).
pub const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One cached tool definition (serializable mirror of [`ToolDefinition`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedTool {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    annotations: Option<Value>,
}

impl From<&ToolDefinition> for CachedTool {
    fn from(def: &ToolDefinition) -> Self {
        Self {
            name: def.name.clone(),
            title: def.title.clone(),
            description: def.description.clone(),
            input_schema: def.input_schema.clone(),
            output_schema: def.output_schema.clone(),
            annotations: def.annotations.clone(),
        }
    }
}

impl From<CachedTool> for ToolDefinition {
    fn from(tool: CachedTool) -> Self {
        Self {
            name: tool.name,
            title: tool.title,
            description: tool.description,
            input_schema: tool.input_schema,
            output_schema: tool.output_schema,
            annotations: tool.annotations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    config_hash: String,
    version: u32,
    saved_at_ms: u64,
    tools: Vec<CachedTool>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, CacheEntry>,
}

/// JSON-backed tool cache shared by every session on the machine.
#[derive(Debug, Clone)]
pub struct ToolCache {
    path: PathBuf,
    ttl: Duration,
}

/// Hash of the config fields that decide which server a config talks to.
/// Timeouts and `enabled` are excluded: they do not change the tool list.
#[must_use]
pub fn config_hash(config: &McpServerConfig) -> String {
    let kind = match config.kind {
        crate::ServerKind::Stdio => "stdio",
        crate::ServerKind::Http => "http",
        crate::ServerKind::Sse => "sse",
    };
    let identity = json!({
        "type": kind,
        "command": config.command,
        "args": config.args,
        "env": config.env,
        "cwd": config.cwd,
        "url": config.url,
        "headers": config.headers,
    });
    let digest = Sha256::digest(identity.to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl ToolCache {
    /// Cache stored at `path` with the given entry lifetime.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, ttl: Duration) -> Self {
        Self {
            path: path.into(),
            ttl,
        }
    }

    /// Standard location next to the user config:
    /// `~/.omp/agent/cache/mcp-tools.json`. `None` when no home is known or
    /// `OMP_MCP_TOOL_CACHE=0` disables caching.
    #[must_use]
    pub fn default_location() -> Option<Self> {
        if std::env::var("OMP_MCP_TOOL_CACHE").is_ok_and(|v| v == "0") {
            return None;
        }
        let agent_dir = crate::config::user_config_path()?.parent()?.to_path_buf();
        Some(Self::new(
            agent_dir.join("cache").join("mcp-tools.json"),
            DEFAULT_TTL,
        ))
    }

    /// Backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> CacheFile {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return CacheFile::default();
        };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(file) if file.version == CACHE_VERSION => file,
            Ok(_) => CacheFile::default(),
            Err(error) => {
                tracing::warn!(
                    "ignoring corrupt MCP tool cache {}: {error}",
                    self.path.display()
                );
                CacheFile::default()
            }
        }
    }

    fn store(&self, mut file: CacheFile) -> io::Result<()> {
        file.version = CACHE_VERSION;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
        // Write-then-rename so a concurrent reader never sees a torn file.
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Cached tools for `server`, if present, fresh, and built from the same
    /// config and cache version.
    #[must_use]
    pub fn get(&self, server: &str, config: &McpServerConfig) -> Option<Vec<ToolDefinition>> {
        self.get_at(server, config, now_ms())
    }

    fn get_at(
        &self,
        server: &str,
        config: &McpServerConfig,
        now: u64,
    ) -> Option<Vec<ToolDefinition>> {
        let entry = self.load().servers.remove(server)?;
        if entry.version != CACHE_VERSION || entry.config_hash != config_hash(config) {
            return None;
        }
        let ttl_ms = u64::try_from(self.ttl.as_millis()).unwrap_or(u64::MAX);
        if now.saturating_sub(entry.saved_at_ms) > ttl_ms {
            return None;
        }
        Some(entry.tools.into_iter().map(ToolDefinition::from).collect())
    }

    /// Replace the entry for `server` with a fresh `tools/list` result.
    pub fn put(
        &self,
        server: &str,
        config: &McpServerConfig,
        tools: &[ToolDefinition],
    ) -> io::Result<()> {
        self.put_at(server, config, tools, now_ms())
    }

    fn put_at(
        &self,
        server: &str,
        config: &McpServerConfig,
        tools: &[ToolDefinition],
        now: u64,
    ) -> io::Result<()> {
        let mut file = self.load();
        file.servers.insert(
            server.to_string(),
            CacheEntry {
                config_hash: config_hash(config),
                version: CACHE_VERSION,
                saved_at_ms: now,
                tools: tools.iter().map(CachedTool::from).collect(),
            },
        );
        self.store(file)
    }

    /// Drop the entry for `server`.
    pub fn invalidate(&self, server: &str) -> io::Result<()> {
        let mut file = self.load();
        if file.servers.remove(server).is_some() {
            self.store(file)?;
        }
        Ok(())
    }

    /// Drop every entry.
    pub fn clear(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_config(url: &str) -> McpServerConfig {
        serde_json::from_value(json!({ "type": "http", "url": url })).unwrap()
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            title: Some(format!("{name} title")),
            description: Some("does things".to_string()),
            input_schema: json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
            output_schema: Some(json!({ "type": "object" })),
            annotations: Some(json!({ "readOnlyHint": true })),
        }
    }

    fn cache(dir: &tempfile::TempDir) -> ToolCache {
        ToolCache::new(dir.path().join("cache").join("tools.json"), DEFAULT_TTL)
    }

    #[test]
    fn round_trips_tools_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let config = http_config("http://a/mcp");
        cache(&dir)
            .put("srv", &config, &[tool("one"), tool("two")])
            .unwrap();
        // A fresh instance (new session) reads the same file.
        let got = cache(&dir).get("srv", &config).unwrap();
        assert_eq!(got, vec![tool("one"), tool("two")]);
    }

    #[test]
    fn missing_file_and_unknown_server_miss() {
        let dir = tempfile::tempdir().unwrap();
        let config = http_config("http://a/mcp");
        assert!(cache(&dir).get("srv", &config).is_none());
        cache(&dir).put("srv", &config, &[tool("one")]).unwrap();
        assert!(cache(&dir).get("other", &config).is_none());
    }

    #[test]
    fn config_change_invalidates_entry() {
        let dir = tempfile::tempdir().unwrap();
        cache(&dir)
            .put("srv", &http_config("http://a/mcp"), &[tool("one")])
            .unwrap();
        assert!(cache(&dir)
            .get("srv", &http_config("http://b/mcp"))
            .is_none());
    }

    #[test]
    fn timeout_does_not_change_hash() {
        let a = http_config("http://a/mcp");
        let mut b = a.clone();
        b.timeout = Some(5);
        b.enabled = Some(true);
        assert_eq!(config_hash(&a), config_hash(&b));
        let mut c = a.clone();
        c.headers = Some(BTreeMap::from([("x".to_string(), "y".to_string())]));
        assert_ne!(config_hash(&a), config_hash(&c));
    }

    #[test]
    fn expired_entry_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let config = http_config("http://a/mcp");
        let c = ToolCache::new(dir.path().join("t.json"), Duration::from_secs(10));
        c.put_at("srv", &config, &[tool("one")], 1_000).unwrap();
        assert!(c.get_at("srv", &config, 11_000).is_some());
        assert!(c.get_at("srv", &config, 11_001).is_none());
    }

    #[test]
    fn version_mismatch_and_corrupt_file_miss() {
        let dir = tempfile::tempdir().unwrap();
        let config = http_config("http://a/mcp");
        let c = cache(&dir);
        c.put("srv", &config, &[tool("one")]).unwrap();
        let text = std::fs::read_to_string(c.path()).unwrap();
        let bumped = text.replace(
            &format!("\"version\": {CACHE_VERSION}"),
            &format!("\"version\": {}", CACHE_VERSION + 1),
        );
        std::fs::write(c.path(), bumped).unwrap();
        assert!(c.get("srv", &config).is_none());

        std::fs::write(c.path(), "{not json").unwrap();
        assert!(c.get("srv", &config).is_none());
        // A corrupt file is replaced on the next write.
        c.put("srv", &config, &[tool("one")]).unwrap();
        assert!(c.get("srv", &config).is_some());
    }

    #[test]
    fn invalidate_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let config = http_config("http://a/mcp");
        let c = cache(&dir);
        c.put("a", &config, &[tool("one")]).unwrap();
        c.put("b", &config, &[tool("two")]).unwrap();
        c.invalidate("a").unwrap();
        assert!(c.get("a", &config).is_none());
        assert!(c.get("b", &config).is_some());
        c.clear().unwrap();
        assert!(c.get("b", &config).is_none());
        c.clear().unwrap();
    }
}
