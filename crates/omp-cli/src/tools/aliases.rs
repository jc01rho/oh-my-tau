//! Built-in tool name canonicalization, ported from omp
//! `packages/coding-agent/src/tools/builtin-names.ts`.
//!
//! `LEGACY_BUILTIN_TOOL_NAME_ALIASES` carries the renames omp still accepts on
//! the wire (`search` was renamed `grep`), [`normalize_tool_name`] applies them
//! and lowercases canonical IDs while leaving plugin names untouched, and
//! [`normalize_tool_names`] dedups a list preserving first-seen order.

/// Canonical built-in tool IDs, in omp's `BUILTIN_TOOL_NAMES` order.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "edit",
    "ast_grep",
    "ast_edit",
    "ask",
    "debug",
    "eval",
    "github",
    "glob",
    "grep",
    "find",
    "lsp",
    "checkpoint",
    "rewind",
    "context_notes",
    "new_context",
    "security_scan",
    "task",
    "wait",
    "todo",
    "web_search",
    "write",
    "memory_edit",
    "retain",
    "recall",
    "reflect",
    "learn",
    "manage_skill",
];

/// Hidden built-in tool IDs (omp `HIDDEN_TOOL_NAMES`): installed but never
/// listed in the model-facing tool catalog.
pub const HIDDEN_TOOL_NAMES: &[&str] = &["yield", "goal", "think"];

/// Legacy built-in ID → canonical ID (omp
/// `LEGACY_BUILTIN_TOOL_NAME_ALIASES`, a `Map([["search", "grep"]])`).
///
/// A slice rather than a map so the table stays `const`; [`legacy_alias`] is the
/// lookup.
pub const LEGACY_BUILTIN_TOOL_NAME_ALIASES: &[(&str, &str)] = &[("search", "grep")];

/// The canonical ID a legacy alias maps to, if `name` is one.
#[must_use]
pub fn legacy_alias(name: &str) -> Option<&'static str> {
    LEGACY_BUILTIN_TOOL_NAME_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
}

/// Whether `name` is a canonical built-in or hidden tool ID (lowercase form).
#[must_use]
pub fn is_canonical_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name) || HIDDEN_TOOL_NAMES.contains(&name)
}

/// Canonicalize a built-in ID or legacy alias; leave plugin names unchanged
/// (omp `normalizeToolName`).
///
/// The alias and canonical lookups run on the lowercased name, so `SEARCH`
/// becomes `grep` and `Read` becomes `read`; anything else is returned in its
/// original spelling, since a plugin may be case-sensitive.
#[must_use]
pub fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    if let Some(canonical) = legacy_alias(&lower) {
        return canonical.to_string();
    }
    if is_canonical_tool_name(&lower) {
        return lower;
    }
    name.to_string()
}

/// Normalize and deduplicate tool names, preserving first-seen order (omp
/// `normalizeToolNames`).
#[must_use]
pub fn normalize_tool_names<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in names {
        let normalized = normalize_tool_name(name.as_ref());
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_alias_table_matches_omp() {
        assert_eq!(LEGACY_BUILTIN_TOOL_NAME_ALIASES.len(), 1);
        assert_eq!(LEGACY_BUILTIN_TOOL_NAME_ALIASES[0], ("search", "grep"));
        assert_eq!(legacy_alias("search"), Some("grep"));
        assert_eq!(legacy_alias("grep"), None);
        assert_eq!(legacy_alias("SEARCH"), None, "lookup is on the caller's casing");
    }

    #[test]
    fn aliases_map_to_canonical_ids() {
        assert_eq!(normalize_tool_name("search"), "grep");
        assert_eq!(normalize_tool_name("SEARCH"), "grep");
        assert_eq!(normalize_tool_name("Search"), "grep");
    }

    #[test]
    fn canonical_ids_lowercase() {
        assert_eq!(normalize_tool_name("read"), "read");
        assert_eq!(normalize_tool_name("Read"), "read");
        assert_eq!(normalize_tool_name("WEB_SEARCH"), "web_search");
        // Hidden tools are canonical too.
        assert_eq!(normalize_tool_name("Think"), "think");
        assert_eq!(normalize_tool_name("goal"), "goal");
        assert_eq!(normalize_tool_name("yield"), "yield");
    }

    #[test]
    fn plugin_names_keep_their_spelling() {
        assert_eq!(normalize_tool_name("mcp__server_tool"), "mcp__server_tool");
        assert_eq!(normalize_tool_name("MyPlugin"), "MyPlugin");
        assert_eq!(normalize_tool_name("vibe_spawn"), "vibe_spawn");
    }

    #[test]
    fn empty_name_is_preserved() {
        assert_eq!(normalize_tool_name(""), "");
        assert!(normalize_tool_names(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn normalize_names_dedups_preserving_first_seen_order() {
        assert_eq!(
            normalize_tool_names(["bash", "read", "bash"]),
            vec!["bash", "read"]
        );
    }

    #[test]
    fn alias_and_canonical_collapse_to_one_entry() {
        assert_eq!(
            normalize_tool_names(["search", "grep", "SEARCH"]),
            vec!["grep"]
        );
    }

    #[test]
    fn normalize_names_keeps_distinct_plugin_names() {
        assert_eq!(
            normalize_tool_names(["Read", "mcp__a_b", "mcp__a_c", "read"]),
            vec!["read", "mcp__a_b", "mcp__a_c"]
        );
    }

    #[test]
    fn normalize_names_accepts_borrowed_and_owned() {
        let owned = vec!["Search".to_string(), "todo".to_string()];
        assert_eq!(normalize_tool_names(owned), vec!["grep", "todo"]);
        let borrowed: Vec<&str> = vec!["Search", "todo"];
        assert_eq!(normalize_tool_names(borrowed), vec!["grep", "todo"]);
    }
}
