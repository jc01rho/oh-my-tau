//! `rule://` protocol handler (TS `internal-urls/rule-protocol.ts`).
//!
//! URL form: `rule://<name>` — reads rule content (frontmatter stripped).
//! Virtual backing: resolved content is the rule body, not the file's bytes.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::{InternalResource, InternalUrl, UrlCompletion, UrlCtx, UrlProvider};

use super::RuleEntry;

/// Prompt doc line (TS `prompts/internal-urls/rule.md`, trimmed).
pub const PROMPT_DOC: &str = "rule://<name>: details.";

static ACTIVE_RULES: RwLock<Option<Arc<Vec<RuleEntry>>>> = RwLock::new(None);

/// Install the process-global rulebook the active session loaded
/// (TS `setActiveRules`). Read by `rule://` when the caller threads no
/// [`UrlCtx::rules`].
pub fn set_active_rules(rules: Vec<RuleEntry>) {
    *ACTIVE_RULES.write().expect("active rules lock") = Some(Arc::new(rules));
}

/// The installed rulebook, or an empty book when no session installed one
/// (TS `getActiveRules`).
pub fn active_rules() -> Arc<Vec<RuleEntry>> {
    ACTIVE_RULES
        .read()
        .expect("active rules lock")
        .clone()
        .unwrap_or_default()
}

/// Rules addressable by one caller: the context's book when it threaded one,
/// else the process-global book.
fn rules_for(ctx: &UrlCtx) -> Arc<Vec<RuleEntry>> {
    ctx.rules.clone().unwrap_or_else(active_rules)
}

/// Rule name addressed by `url` (TS `url.rawHost || url.hostname`).
fn rule_name(url: &InternalUrl) -> Result<&str, String> {
    if url.host.is_empty() {
        return Err("rule:// URL requires a rule name: rule://<name>".to_string());
    }
    Ok(&url.host)
}

/// Look up a rule by name; errors on a nameless URL (TS `findRule`).
fn find_rule<'a>(url: &InternalUrl, rules: &'a [RuleEntry]) -> Result<&'a RuleEntry, String> {
    let name = rule_name(url)?;
    match rules.iter().find(|r| r.name == name) {
        Some(rule) => Ok(rule),
        None => {
            // TS preserves discovery order here; it does not sort.
            let available: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
            let available_str = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            Err(format!("Unknown rule: {name}\nAvailable: {available_str}"))
        }
    }
}

/// Resolve a `rule://` URL to the rule body resource.
pub fn resolve(url: &InternalUrl, rules: &[RuleEntry]) -> Result<InternalResource, String> {
    let rule = find_rule(url, rules)?;
    Ok(InternalResource {
        content: rule.content.clone(),
        is_directory: false,
        immutable: Some(true),
        path: Some(rule.path.clone()),
    })
}

/// Rule source file (with frontmatter); `None` for unknown rules and rules with no file on disk.
pub fn locate(url: &InternalUrl, rules: &[RuleEntry]) -> Result<Option<PathBuf>, String> {
    let name = rule_name(url)?;
    Ok(rules
        .iter()
        .find(|r| r.name == name)
        .filter(|r| !r.path.is_empty() && PathBuf::from(&r.path).exists())
        .map(|r| PathBuf::from(&r.path)))
}

/// Autocomplete candidates over rule names + descriptions.
pub fn complete(rules: &[RuleEntry]) -> Vec<UrlCompletion> {
    rules
        .iter()
        .map(|r| UrlCompletion {
            label: r.name.clone(),
            detail: (!r.description.is_empty()).then(|| r.description.clone()),
        })
        .collect()
}

/// Read-only `rule://` provider over the session rulebook.
#[derive(Default)]
pub struct RuleProvider;

impl UrlProvider for RuleProvider {
    fn scheme(&self) -> &'static str {
        "rule"
    }

    fn spec(&self) -> &crate::SchemeSpec {
        static SPEC: crate::SchemeSpec = crate::SchemeSpec {
            backing: crate::SchemeBacking::Virtual,
            selectors: crate::SchemeSelectors::Lines,
            immutable: true,
            single_slash_alias: false,
            linkable: true,
            port_authority: false,
            write: None,
        };
        &SPEC
    }

    fn read(&self, url: &InternalUrl, ctx: &UrlCtx) -> anyhow::Result<InternalResource> {
        resolve(url, &rules_for(ctx)).map_err(anyhow::Error::msg)
    }

    fn locate(&self, url: &InternalUrl, ctx: &UrlCtx) -> Option<PathBuf> {
        locate(url, &rules_for(ctx)).ok().flatten()
    }

    fn complete(&self, _query: &str, ctx: &UrlCtx) -> Vec<UrlCompletion> {
        complete(&rules_for(ctx))
    }

    fn prompt_doc(&self) -> Option<String> {
        // TS advertises the scheme only when the book has addressable rules;
        // `SchemeHost` is not threaded here, so the global book is the signal.
        (!active_rules().is_empty()).then(|| PROMPT_DOC.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_internal_url;

    /// Serializes the tests that publish the process-global rulebook.
    static GLOBAL_BOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_BOOK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rule(name: &str, content: &str) -> RuleEntry {
        RuleEntry {
            name: name.to_string(),
            path: format!("/tmp/rules/{name}.md"),
            content: content.to_string(),
            ..RuleEntry::default()
        }
    }

    #[test]
    fn resolves_rule_body() {
        let rules = [rule("tdd", "Write tests first.")];
        let url = parse_internal_url("rule://tdd").unwrap();
        let res = resolve(&url, &rules).unwrap();
        assert_eq!(res.content, "Write tests first.");
        assert!(!res.is_directory);
    }

    #[test]
    fn errors_match_ts() {
        let url = parse_internal_url("rule://").unwrap();
        assert_eq!(
            resolve(&url, &[]).unwrap_err(),
            "rule:// URL requires a rule name: rule://<name>"
        );
        let rules = [rule("a", "x")];
        let url = parse_internal_url("rule://zzz").unwrap();
        assert_eq!(
            resolve(&url, &rules).unwrap_err(),
            "Unknown rule: zzz\nAvailable: a"
        );
    }

    #[test]
    fn empty_rulebook_reports_unknown_rule() {
        let url = parse_internal_url("rule://tdd").unwrap();
        assert_eq!(
            resolve(&url, &[]).unwrap_err(),
            "Unknown rule: tdd\nAvailable: none"
        );
        // A nameless URL still keeps the TS parse error.
        let url = parse_internal_url("rule://").unwrap();
        assert_eq!(
            resolve(&url, &[]).unwrap_err(),
            "rule:// URL requires a rule name: rule://<name>"
        );
    }

    #[test]
    fn available_list_preserves_discovery_order() {
        let rules = [rule("zeta", "z"), rule("alpha", "a")];
        let url = parse_internal_url("rule://missing").unwrap();
        assert_eq!(
            resolve(&url, &rules).unwrap_err(),
            "Unknown rule: missing\nAvailable: zeta, alpha"
        );
    }

    #[test]
    fn locate_returns_existing_file_only() {
        let rules = [rule("ghost", "x")];
        let url = parse_internal_url("rule://ghost").unwrap();
        assert_eq!(locate(&url, &rules).unwrap(), None);
        let url = parse_internal_url("rule://").unwrap();
        assert_eq!(
            locate(&url, &rules).unwrap_err(),
            "rule:// URL requires a rule name: rule://<name>"
        );
    }

    #[test]
    fn context_rules_override_the_global_book() {
        let _guard = lock_globals();
        set_active_rules(vec![rule("global", "g")]);
        let ctx = UrlCtx {
            rules: Some(Arc::new(vec![rule("scoped", "s")])),
            ..UrlCtx::default()
        };
        let provider = RuleProvider;
        let url = parse_internal_url("rule://scoped").unwrap();
        assert_eq!(provider.read(&url, &ctx).unwrap().content, "s");
        let url = parse_internal_url("rule://global").unwrap();
        assert_eq!(
            provider.read(&url, &ctx).unwrap_err().to_string(),
            "Unknown rule: global\nAvailable: scoped"
        );
        // A context that threads no book falls back to the global one.
        let url = parse_internal_url("rule://global").unwrap();
        assert_eq!(
            provider.read(&url, &UrlCtx::default()).unwrap().content,
            "g"
        );
        set_active_rules(Vec::new());
    }
}
