#![allow(dead_code)]
//! Rule discovery and bucketing, ported from omp's `capability/rule.ts`,
//! `capability/rule-buckets.ts`, and the rule half of `discovery/builtin.ts`.
//!
//! Rules reach a session through three routes:
//!
//! * `RULES.md` (user `<agent>/RULES.md`, nearest project `.omp/RULES.md`)
//!   becomes an always-apply rule whose whole body is carried on every
//!   request, so it keeps its hold across long sessions.
//! * `.omp/rules/*.md|mdc` (and `<agent>/rules/*.md|mdc`) with a
//!   `description` become rulebook entries: listed in `<domain-rules>` and
//!   read on demand through `rule://<name>`.
//! * rules carrying a TTSR trigger (`condition`/`astCondition`/`question`)
//!   never reach the prompt, because oh-my-tau has no TTSR stream subsystem
//!   to register them with.
//!
//! Discovery order is dedupe order: the nearest project scope wins a name
//! over a farther ancestor, and every project scope wins over the user scope
//! (`getConfigDirs` project-first ordering plus first-wins capability dedupe).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A discovered rule, canonicalized from Cursor `.mdc` / plain Markdown.
#[derive(Debug, Clone, Default)]
pub struct Rule {
    /// Rule name: the file stem, or `RULES` / `RULES@project` for sticky files.
    pub name: String,
    /// Absolute path of the rule file on disk.
    pub path: PathBuf,
    /// Rule body with the frontmatter block stripped.
    pub content: String,
    /// File globs the rule applies to (Cursor `globs:`).
    pub globs: Vec<String>,
    /// `alwaysApply: true` in frontmatter, or a `RULES.md` sticky rule.
    pub always_apply: bool,
    /// Agent-requested description (`description:`); the rulebook listing key.
    pub description: String,
    /// Lowercased agent-name globs; empty applies to every agent.
    pub agents: Vec<String>,
    /// TTSR regex triggers (never surfaced by this port, see module docs).
    pub condition: Vec<String>,
    /// TTSR ast-grep triggers (never surfaced by this port).
    pub ast_condition: Vec<String>,
    /// TTSR judged-rule question (never surfaced by this port).
    pub question: Option<String>,
}

impl Rule {
    /// Rulebook entry shape: name, description, globs.
    #[must_use]
    pub fn to_rulebook_entry(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "globs": self.globs,
        })
    }

    /// Always-apply entry shape: name plus the carried body.
    #[must_use]
    pub fn to_always_apply_entry(&self) -> serde_json::Value {
        serde_json::json!({ "name": self.name, "content": self.content })
    }
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

/// Split a leading `---` fenced YAML block from the rule body.
fn split_frontmatter(content: &str) -> (Option<serde_yaml_ng::Value>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, content);
    };
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    (serde_yaml_ng::from_str(yaml).ok(), body)
}

/// A frontmatter field as a list of trimmed, de-duplicated strings.
///
/// `split_commas` mirrors `normalizeRuleField` (a scalar is one token) versus
/// `parseRuleAgents`/scope parsing (a scalar is a comma-separated list).
fn string_list(value: Option<&serde_yaml_ng::Value>, split_commas: bool) -> Vec<String> {
    fn push_token(out: &mut Vec<String>, raw: &str) {
        let token = raw.trim();
        if !token.is_empty() && !out.iter().any(|seen| seen == token) {
            out.push(token.to_string());
        }
    }
    let mut out = Vec::new();
    match value {
        Some(serde_yaml_ng::Value::String(s)) => {
            if split_commas {
                for part in s.split(',') {
                    push_token(&mut out, part);
                }
            } else {
                push_token(&mut out, s);
            }
        }
        Some(serde_yaml_ng::Value::Sequence(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    if split_commas {
                        for part in s.split(',') {
                            push_token(&mut out, part);
                        }
                    } else {
                        push_token(&mut out, s);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Canonical rule from Markdown; `None` when `enabled: false` disables it.
fn rule_from_markdown(name: &str, content: &str, path: &Path) -> Option<Rule> {
    let (frontmatter, body) = split_frontmatter(content);
    let map = frontmatter
        .as_ref()
        .and_then(serde_yaml_ng::Value::as_mapping);
    let field = |key: &str| map.and_then(|m| m.get(serde_yaml_ng::Value::String(key.to_string())));
    if field("enabled")
        .and_then(serde_yaml_ng::Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return None;
    }
    Some(Rule {
        name: name.to_string(),
        path: path.to_path_buf(),
        content: body.trim_end().to_string(),
        globs: string_list(field("globs"), false),
        always_apply: field("alwaysApply")
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false),
        description: field("description")
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        agents: string_list(field("agents"), true)
            .into_iter()
            .map(|token| token.to_lowercase())
            .collect(),
        condition: string_list(field("condition"), true),
        ast_condition: string_list(field("astCondition"), true),
        question: field("question")
            .and_then(serde_yaml_ng::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(ToString::to_string),
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Repository root: the nearest ancestor with a `.git` entry.
fn repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Project config dirs (`<dir>/.omp`), nearest first, from `cwd` up to the repo
/// root. The home directory is the user config root and never a project
/// (`findNearestProjectConfigDir` / `getProjectAncestorDirs`).
fn project_config_dirs(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let root = repo_root(cwd);
    let mut out = Vec::new();
    for dir in cwd.ancestors() {
        if dir == home {
            break;
        }
        out.push(dir.join(".omp"));
        if root.as_deref() == Some(dir) {
            break;
        }
    }
    out
}

/// Nearest ancestor whose `.omp` directory exists.
fn nearest_project_config_dir(cwd: &Path, home: &Path) -> Option<PathBuf> {
    project_config_dirs(cwd, home)
        .into_iter()
        .find(|dir| dir.is_dir())
}

/// Load every `*.md` / `*.mdc` rule in `dir`, first name wins.
fn load_rules_dir(dir: &Path, out: &mut Vec<Rule>, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    for path in files {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(extension, "md" | "mdc") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(rule) = rule_from_markdown(stem, &content, &path) {
            if seen.insert(rule.name.clone()) {
                out.push(rule);
            }
        }
    }
}

/// Read a top-level `RULES.md` and synthesize a sticky always-apply rule
/// (`loadStickyRulesFile`). Empty files contribute nothing.
fn sticky_rule(path: &Path, name: &str) -> Option<Rule> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    let mut rule = rule_from_markdown("RULES.md", &content, path)?;
    rule.name = name.to_string();
    // Forced regardless of frontmatter: the point of RULES.md is that its body
    // is carried on every request instead of degrading to an on-demand entry.
    rule.always_apply = true;
    Some(rule)
}

/// Every rule a session can see, in dedupe order (see module docs).
#[must_use]
pub fn load_rules(cwd: &Path, agent_dir: &Path) -> Vec<Rule> {
    let home = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .and_then(|_| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    load_rules_with_home(cwd, agent_dir, &home)
}

/// [`load_rules`] with an explicit home directory (tests).
#[must_use]
pub fn load_rules_with_home(cwd: &Path, agent_dir: &Path, home: &Path) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Project rulebook, nearest scope first.
    for config_dir in project_config_dirs(cwd, home) {
        load_rules_dir(&config_dir.join("rules"), &mut rules, &mut seen);
    }
    // User rulebook.
    load_rules_dir(&agent_dir.join("rules"), &mut rules, &mut seen);

    // Sticky RULES.md: project scope first so `RULES@project` is registered
    // before the user `RULES` (names never collide, so order is cosmetic).
    if let Some(config_dir) = nearest_project_config_dir(cwd, home) {
        if let Some(rule) = sticky_rule(&config_dir.join("RULES.md"), "RULES@project") {
            if seen.insert(rule.name.clone()) {
                rules.push(rule);
            }
        }
    }
    if let Some(rule) = sticky_rule(&agent_dir.join("RULES.md"), "RULES") {
        if seen.insert(rule.name.clone()) {
            rules.push(rule);
        }
    }

    rules
}

// ---------------------------------------------------------------------------
// Bucketing
// ---------------------------------------------------------------------------

/// Rules split into the two prompt buckets (`RuleBuckets`).
#[derive(Debug, Default)]
pub struct RuleBuckets {
    /// Rules listed as `<domain-rules>` and read on demand via `rule://`.
    pub rulebook: Vec<Rule>,
    /// Rules whose body is carried on every request.
    pub always_apply: Vec<Rule>,
}

/// Levers applied before a rule reaches a bucket (`BucketRulesOptions`).
#[derive(Debug, Default)]
pub struct BucketOptions<'a> {
    /// Rule names dropped entirely (`ttsr.disabledRules`).
    pub disabled_rules: &'a [String],
    /// Session agent name: `"main"` top-level, otherwise the agent name.
    /// `None` disables agent scoping (CLI inventory listings).
    pub agent_name: Option<&'a str>,
}

/// Whether a rule's `agents` scope admits `agent_name` (`ruleAppliesToAgent`).
#[must_use]
pub fn rule_applies_to_agent(rule: &Rule, agent_name: Option<&str>) -> bool {
    if rule.agents.is_empty() {
        return true;
    }
    let Some(name) = agent_name else {
        return true;
    };
    let name = name.trim().to_lowercase();
    rule.agents.iter().any(|pattern| {
        pattern == &name
            || globset::Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(&name))
    })
}

/// Whether the rule carries a TTSR trigger, which this port cannot register.
fn has_ttsr_trigger(rule: &Rule) -> bool {
    !rule.condition.is_empty() || !rule.ast_condition.is_empty() || rule.question.is_some()
}

/// Filter and bucket a rule snapshot (`bucketRules`).
///
/// Precedence: `disabledRules` drop, then agent scope, then TTSR exclusion,
/// then `alwaysApply`, then the rulebook (which requires a description).
#[must_use]
pub fn bucket_rules(rules: Vec<Rule>, options: &BucketOptions<'_>) -> RuleBuckets {
    let mut buckets = RuleBuckets::default();
    for rule in rules {
        if options.disabled_rules.iter().any(|name| name == &rule.name) {
            continue;
        }
        if !rule_applies_to_agent(&rule, options.agent_name) {
            continue;
        }
        if has_ttsr_trigger(&rule) {
            continue;
        }
        if rule.always_apply {
            buckets.always_apply.push(rule);
            continue;
        }
        if !rule.description.is_empty() {
            buckets.rulebook.push(rule);
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omp-rs-rules-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// `RULES.md` is sticky: both scopes load, the project copy is named
    /// `RULES@project`, and `alwaysApply` is forced regardless of frontmatter.
    #[test]
    fn rules_md_is_sticky_always_apply() {
        let root = tempdir("sticky");
        let repo = root.join("repo");
        let agent = root.join("home/.omp/agent");
        write(&repo.join(".git/HEAD"), "");
        write(&repo.join(".omp/RULES.md"), "project rule body\n");
        write(&agent.join("RULES.md"), "user rule body\n");
        let rules = load_rules_with_home(&repo, &agent, &root.join("home"));
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["RULES@project", "RULES"]);
        assert!(rules.iter().all(|r| r.always_apply));
        assert_eq!(rules[0].content, "project rule body");
        // Forced even when the file asks to stay on-demand.
        write(
            &repo.join(".omp/RULES.md"),
            "---\nalwaysApply: false\n---\nbody\n",
        );
        let rules = load_rules_with_home(&repo, &agent, &root.join("home"));
        assert!(rules[0].always_apply);
        assert_eq!(rules[0].content, "body");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.omp/rules/*.md|mdc` load with frontmatter fields; `enabled: false`
    /// drops a rule and a nearer scope wins a name over a farther one.
    #[test]
    fn rulebook_discovery_and_precedence() {
        let root = tempdir("discovery");
        let repo = root.join("repo");
        let cwd = repo.join("pkg/app");
        let agent = root.join("home/.omp/agent");
        write(&repo.join(".git/HEAD"), "");
        write(
            &repo.join(".omp/rules/tdd.md"),
            "---\ndescription: Outer TDD.\nglobs:\n  - src/**/*.rs\nalwaysApply: false\n---\nouter\n",
        );
        write(
            &repo.join(".omp/rules/style.mdc"),
            "---\ndescription: Style guide.\n---\nstyle body\n",
        );
        write(
            &repo.join(".omp/rules/off.md"),
            "---\nenabled: false\ndescription: Disabled.\n---\noff\n",
        );
        write(
            &repo.join("pkg/.omp/rules/tdd.md"),
            "---\ndescription: Inner TDD.\n---\ninner\n",
        );
        write(
            &agent.join("rules/user-only.md"),
            "---\ndescription: User rule.\n---\nuser\n",
        );
        write(
            &agent.join("rules/tdd.md"),
            "---\ndescription: User TDD.\n---\nuser tdd\n",
        );

        let rules = load_rules_with_home(&cwd, &agent, &root.join("home"));
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["tdd", "style", "user-only"]);
        let tdd = &rules[0];
        assert_eq!(tdd.description, "Inner TDD.");
        assert_eq!(tdd.content, "inner");
        assert!(tdd.globs.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Frontmatter drives globs, agent scope, and TTSR trigger parsing.
    #[test]
    fn frontmatter_fields_parse() {
        let path = Path::new("/tmp/x/r.md");
        let rule = rule_from_markdown(
            "r",
            "---\ndescription: D\nglobs: src/**\nagents: Main, Reviewer*\ncondition:\n  - 'a.*'\n  - 'b.*'\n---\nbody\n",
            path,
        )
        .unwrap();
        assert_eq!(rule.globs, vec!["src/**"]);
        assert_eq!(rule.agents, vec!["main", "reviewer*"]);
        assert_eq!(rule.condition, vec!["a.*", "b.*"]);
        assert_eq!(rule.content, "body");
        assert!(rule_from_markdown("r", "---\nenabled: false\n---\nbody\n", path).is_none());
        // No frontmatter: whole file is the body.
        let plain = rule_from_markdown("p", "just text\n", path).unwrap();
        assert_eq!(plain.content, "just text");
        assert!(plain.globs.is_empty());
    }

    /// Bucketing: disabled names, agent scope, TTSR triggers, always-apply,
    /// and the description requirement for the rulebook.
    #[test]
    fn bucket_precedence() {
        let rule = |name: &str, description: &str, always_apply: bool| Rule {
            name: name.to_string(),
            description: description.to_string(),
            always_apply,
            ..Rule::default()
        };
        let mut ttsr = rule("ttsr", "Triggered.", false);
        ttsr.condition = vec![".*".to_string()];
        let mut scoped = rule("scoped", "Only reviewer.", false);
        scoped.agents = vec!["reviewer*".to_string()];
        let rules = vec![
            rule("always", "", true),
            rule("plain", "Described.", false),
            rule("undescribed", "", false),
            rule("disabled", "Dropped.", false),
            ttsr,
            scoped,
        ];
        let disabled = vec!["disabled".to_string()];
        let buckets = bucket_rules(
            rules,
            &BucketOptions {
                disabled_rules: &disabled,
                agent_name: Some("main"),
            },
        );
        let rulebook: Vec<&str> = buckets.rulebook.iter().map(|r| r.name.as_str()).collect();
        let always: Vec<&str> = buckets
            .always_apply
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(rulebook, vec!["plain"]);
        assert_eq!(always, vec!["always"]);

        // Reviewer session: the scoped rule joins the rulebook.
        let buckets = bucket_rules(
            vec![{
                let mut r = rule("scoped", "Only reviewer.", false);
                r.agents = vec!["reviewer*".to_string()];
                r
            }],
            &BucketOptions {
                disabled_rules: &[],
                agent_name: Some("reviewer-2"),
            },
        );
        assert_eq!(buckets.rulebook.len(), 1);
    }

    /// `agents: main` targets the top-level session only.
    #[test]
    fn agent_scope_matches_main() {
        let mut rule = Rule {
            name: "r".into(),
            description: "d".into(),
            ..Rule::default()
        };
        rule.agents = vec!["main".into()];
        assert!(rule_applies_to_agent(&rule, Some("main")));
        assert!(!rule_applies_to_agent(&rule, Some("task")));
        // `None` disables scoping (inventory listings).
        assert!(rule_applies_to_agent(&rule, None));
        rule.agents.clear();
        assert!(rule_applies_to_agent(&rule, Some("task")));
    }

    /// No `.omp` config dir anywhere: discovery stays empty and quiet.
    #[test]
    fn empty_project_has_no_rules() {
        let root = tempdir("empty");
        let repo = root.join("repo");
        write(&repo.join(".git/HEAD"), "");
        let rules = load_rules_with_home(&repo, &root.join("home/.omp/agent"), &root.join("home"));
        assert!(rules.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
