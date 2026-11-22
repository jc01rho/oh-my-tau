//! Port of omp's (pi-coding-agent TS v18.6.1) settings model.
//!
//! A layered, typed view over `config.yml`:
//!
//! 1. defaults from the TS setting registry (526 `register({...})` definitions,
//!    see [`schema`], generated from the TS source),
//! 2. the global config (`~/.omp/agent/config.yml`, or the active
//!    `OMP_PROFILE` profile's agent dir),
//! 3. project overrides (`<cwd>/.omp/config.yml`, then `config.yaml`),
//! 4. environment-variable overrides (each setting's `env:` name, highest precedence).
//!
//! Unknown keys are preserved verbatim in the merged raw map and reported via
//! [`Settings::unknown_keys`] — TS never drops config.yml keys it does not know.

pub mod omo;
mod schema;
mod settings;

pub use omo::{
    active_profile_from_env, find_omo_config, load_omo_file, parse_jsonc, parse_omo_str,
    resolve_profile_name, translate_omo_block, OmoLayer, OMO_CONFIG_FILENAMES, OMO_CONTROL_KEYS,
    OMO_KNOWN_NAMESPACES,
};
pub use schema::{SettingDef, SETTINGS};
pub use settings::{
    resolve_agent_dir, resolve_agent_dir_with_home, write_global, ModelPreset, ModelTagDef,
    Provenance, ServiceTier, Settings,
};

/// Value type of a setting, mirroring the TS registry `type:` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    Boolean,
    Number,
    String,
    /// Closed value set; `()` when TS derives the values at runtime (voice lists etc.).
    Enum(&'static [&'static str]),
    Array,
    Record,
}

impl SettingType {
    /// TS type name as written in `register({ type: ... })`.
    #[must_use]
    pub fn ts_name(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Enum(_) => "enum",
            Self::Array => "array",
            Self::Record => "record",
        }
    }
}

/// Look up a setting definition by dotted id (binary search over the sorted table).
#[must_use]
pub fn lookup(id: &str) -> Option<&'static SettingDef> {
    SETTINGS
        .binary_search_by(|def| def.id.cmp(id))
        .ok()
        .map(|idx| &SETTINGS[idx])
}
