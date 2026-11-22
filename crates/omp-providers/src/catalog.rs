//! Bundled-catalog fingerprint shared by `examples/gen_models.rs` and the
//! currentness test below.
//!
//! The fingerprint is the `rules-<version>-<hash>` suffix TS
//! `materializationPolicy()` builds: `Bun.hash(JSON.stringify(RULES)).toString(36)`
//! (wyhash seed 0, base-36), the same computation `build.rs` runs.

use anyhow::{Context as _, Result};

/// The committed rules snapshot `pa-ai` compiles against (relative to this crate).
pub const RULES_SNAPSHOT: &str = "../../vendor/pa-ai/data/compat/rules.json";
/// The committed slimmed TUI catalog (relative to this crate).
pub const TUI_CATALOG: &str = "../omp-cli/assets/tui-model-catalog.json";

/// `rules-<version>-<base36 wyhash>` for a `rules.json` document.
pub fn rules_fingerprint(rules_json: &str) -> Result<String> {
    let rules: serde_json::Value =
        serde_json::from_str(rules_json).context("parsing rules.json")?;
    let version = rules
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .context("rules.json has no numeric `version`")?;
    // `JSON.stringify(RULES)`: compact, insertion (file) key order (serde_json `preserve_order`).
    let serialized = serde_json::to_string(&rules)?;
    let hash = omp_stats::parser::wyhash::wyhash(serialized.as_bytes());
    Ok(format!("rules-{version}-{}", base36(hash)))
}

/// `bigint.toString(36)` for an unsigned 64-bit value.
fn base36(mut v: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if v == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while v > 0 {
        out.push(DIGITS[(v % 36) as usize]);
        v /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("ascii")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn snapshot(rel: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    #[test]
    fn base36_matches_bigint_to_string() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(u64::MAX), "3w5e11264sgsf");
    }

    /// `models_generated.rs` records the rules fingerprint it was generated against; a rules
    /// bump without re-running `cargo run -p omp-providers --example gen_models -- --write`
    /// fails here.
    #[test]
    fn models_generated_is_current_with_rules_snapshot() {
        let fingerprint = rules_fingerprint(&snapshot(RULES_SNAPSHOT)).expect("fingerprint");
        assert_eq!(
            pa_ai::models_generated::CATALOG_RULES_FINGERPRINT,
            fingerprint,
            "vendor/pa-ai/src/models_generated.rs is stale against {RULES_SNAPSHOT}; \
             regenerate with `cargo run -p omp-providers --example gen_models -- --write`"
        );
    }

    /// build.rs hashes the same snapshot on a clean clone, so the models.db policy and the
    /// compiled table agree. A local vendor-src checkout legitimately overrides build.rs.
    #[test]
    fn materialization_policy_uses_the_same_rules_fingerprint() {
        let vendor_src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor-src/oh-my-pi/packages/catalog/src/compat/rules.json");
        if vendor_src.is_file() {
            return;
        }
        let fingerprint = rules_fingerprint(&snapshot(RULES_SNAPSHOT)).expect("fingerprint");
        let policy = env!("OMP_MODEL_MATERIALIZATION_POLICY");
        assert!(
            policy.ends_with(&format!(":{fingerprint}")),
            "policy {policy} does not end with :{fingerprint}"
        );
    }

    #[test]
    fn tui_catalog_rows_have_the_slim_shape() {
        let catalog: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&snapshot(TUI_CATALOG)).expect("tui catalog parses");
        assert!(!catalog.is_empty());
        for (provider, rows) in &catalog {
            for row in rows.as_array().expect("provider rows array") {
                let row = row.as_array().expect("row array");
                assert_eq!(row.len(), 11, "{provider}: slim row has 11 columns");
                assert!(row[0].is_string(), "{provider}: id column");
            }
        }
    }
}
