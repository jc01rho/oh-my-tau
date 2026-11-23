//! Providers registered at runtime by extensions (`pi.registerProvider`).
//!
//! The extension host forwards each registration as a `provider_registered` /
//! `provider_unregistered` notification and reports the providers alive after a
//! load. The config has the `models.yml` provider shape (`baseUrl`, `apiKey`,
//! `api`, `headers`, `authHeader`, `models[]`), so [`crate::resolve`] and the
//! model listings overlay these entries on the `models.yml` providers: a
//! registration replaces a `models.yml` provider of the same name, and
//! unregistering it brings the file's definition back (TS model registry order).

use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use serde_json::{Map, Value};

#[derive(Debug, Clone)]
struct Registration {
    extension: String,
    config: Map<String, Value>,
}

static REGISTRY: LazyLock<RwLock<BTreeMap<String, Registration>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

fn write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<String, Registration>> {
    REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read() -> std::sync::RwLockReadGuard<'static, BTreeMap<String, Registration>> {
    REGISTRY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records (or replaces) the provider `name` that `extension` registered.
///
/// # Errors
/// An empty name or a config that is not a JSON object.
pub fn upsert(name: &str, extension: &str, config: &Value) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("extension provider without a name".to_string());
    }
    let Value::Object(config) = config else {
        return Err(format!(
            "extension provider '{name}': config is not an object"
        ));
    };
    write().insert(
        name.to_string(),
        Registration {
            extension: extension.to_string(),
            config: config.clone(),
        },
    );
    Ok(())
}

/// Drops the registration of `name`; `true` when one existed.
pub fn remove(name: &str) -> bool {
    write().remove(name.trim()).is_some()
}

/// Replaces every registration with `providers` (`(name, extension, config)`), the
/// set a finished extension load reports alive. Entries that fail [`upsert`]'s
/// checks are skipped and returned as messages.
pub fn replace_all<I>(providers: I) -> Vec<String>
where
    I: IntoIterator<Item = (String, String, Value)>,
{
    let mut next = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, extension, config) in providers {
        let name = name.trim().to_string();
        match config {
            Value::Object(config) if !name.is_empty() => {
                next.insert(name, Registration { extension, config });
            }
            _ => errors.push(format!("extension provider '{name}': invalid registration")),
        }
    }
    *write() = next;
    errors
}

/// `(name, config)` of every live registration, ordered by name.
#[must_use]
pub fn snapshot() -> Vec<(String, Map<String, Value>)> {
    read()
        .iter()
        .map(|(name, r)| (name.clone(), r.config.clone()))
        .collect()
}

/// The extension that registered `name`, when one did.
#[must_use]
pub fn owner(name: &str) -> Option<String> {
    read().get(name).map(|r| r.extension.clone())
}

/// Serializes tests that touch the process-wide registry.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn upsert_remove_and_replace_all() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        replace_all([]);
        upsert("fx", "ext-a", &json!({"baseUrl": "http://x"})).unwrap();
        assert_eq!(owner("fx").as_deref(), Some("ext-a"));
        assert!(upsert("", "ext-a", &json!({})).is_err());
        assert!(upsert("bad", "ext-a", &json!("nope")).is_err());
        assert_eq!(snapshot().len(), 1);
        assert!(remove("fx"));
        assert!(!remove("fx"));
        let errors = replace_all([
            (
                "a".to_string(),
                "e".to_string(),
                json!({"baseUrl": "http://a"}),
            ),
            ("b".to_string(), "e".to_string(), json!(1)),
        ]);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            snapshot().into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
            ["a"]
        );
        replace_all([]);
    }
}
