//! `ast_grep` (structural search) and `ast_edit` (AST-aware rewrite) on the
//! `pi-ast` 19.0.1 `search` engine, matching omp's TS tool semantics:
//! `pat`/`path`/`lang`/`skip` parameters, 50-match default limit with skip
//! pagination, parse-error normalization, and `ast_edit` op validation +
//! per-replacement diff preview lines. omp-rs has no staged-rewrite `resolve`
//! surface, so `ast_edit` applies rewrites in place instead of staging behind
//! `xd://resolve`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use omp_vfs::Fs;
use omp_xfs::glob::Selector;
use pa_agent::abort::{aborted_error, throw_if_aborted, AbortSignal};
use pa_agent::types::AgentToolResult;
use serde_json::{json, Value};

use pi_ast::language::SupportLang;
use pi_ast::ops::resolve_supported_lang;
use pi_ast::search::{
    discover, find_page, rewrite, Candidate, Discover, MatchStrictness, Mode, Page, Query, Rewrite,
};

use super::util::{format_path_relative_to_cwd, path_list, resolve_to_cwd, str_arg};
use super::Env;

pub(crate) const AST_GREP_DESCRIPTION: &str = "Structural code search via ast-grep. Use when syntax shape matters more than text (calls, declarations, language constructs).\n\n<instruction>\n- Narrow each call to one language. `pat` is ONE AST pattern; separate calls for unrelated patterns.\n- Set `lang` when extension inference is ambiguous (for example, `cpp` for `.h`); `.cu` and `.cuh` infer as C++.\n- `$NAME` captures one node; `$_` matches without binding; `$$$NAME` zero-or-more; `$$$` zero-or-more unbound.\n  - Use `$$$NAME`, NOT `$$NAME` (invalid). Names UPPERCASE, whole node — `prefix$VAR` fails.\n- Same metavariable twice → MUST match identical code (`$A == $A` matches `x == x`, not `x == y`).\n- Patterns MUST parse as single AST node. Non-standalone → wrap: `class $_ { … }`.\n- C++ expression-statement calls need trailing `;`: `ns::doThing($ARG);`, `$CALLEE($ARG);`.\n- TS: tolerate annotations — `async function $NAME($$$ARGS): $_ { $$$BODY }`.\n- Declaration forms are distinct — `function foo`, method `foo()`, `const foo = () => {}`; search the right form before concluding absence.\n- Loosest existence check: `pat: \"executeBash\"` with narrow `path`.\n</instruction>\n\n<critical>\n- AVOID repo-root scans — narrow `path` first.\n- Parse issues = query failure, not absence: fix pattern or tighten `path` before concluding \"no matches\".\n</critical>";
pub(crate) const AST_EDIT_DESCRIPTION: &str = "Structural AST-aware rewrites via ast-grep. Use for codemods where text replace is unsafe. Mixed-language paths are fine: each file is parsed in its own language, and a pattern only rewrites files it parses in.\n\n- Metavariables in `pat` (`$A`, `$$$ARGS`) substitute into `out`.\n- **Patterns match AST structure, not text.** `$NAME` = one node; `$_` = unbound; `$$$NAME` = zero-or-more.\n  - Use `$$$NAME`, NOT `$$NAME` (invalid). Names UPPERCASE, whole node — partial like `prefix$VAR` fails.\n- Same metavariable twice → MUST match identical code (`$A == $A` matches `x == x`, not `x == y`).\n- Rewrite patterns MUST parse as single AST node. Non-standalone → wrap: `class $_ { … }`.\n- TS: tolerate annotations — `async function $NAME($$$ARGS): $_ { $$$BODY }`. Delete with empty `out`: `{\"pat\":\"console.log($$$)\",\"out\":\"\"}`.\n- 1:1 substitution — no splitting/merging captures.\n- Changes are applied immediately.\n- Parse issues → malformed rewrite, not clean no-op. For one-off text edits, prefer the Edit tool.";

const DEFAULT_AST_LIMIT: usize = 50;
const MAX_AST_FILES: usize = 1000;
const MAX_PARSE_ERRORS: usize = 5;
const DIFF_LINE_MAX: usize = 120;

/// Parameter schema of `ast_grep` (matches the TS `astGrepSchema`).
#[must_use]
pub fn schema_ast_grep() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pat": {"type": "string", "description": "ast pattern"},
            "path": {"type": "string", "description": "file, directory, glob, or internal URL to search; pass several as a semicolon-delimited list (\"src; tests\"). Omitted -> searches the workspace root (\".\")"},
            "lang": {"type": "string", "description": "language override, e.g. cpp for ambiguous .h files"},
            "skip": {"type": "number", "description": "matches to skip"}
        },
        "required": ["pat"],
        "additionalProperties": false
    })
}

/// Parameter schema of `ast_edit` (matches the TS `astEditSchema`).
#[must_use]
pub fn schema_ast_edit() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ops": {
                "type": "array",
                "minItems": 1,
                "description": "rewrite ops",
                "items": {
                    "type": "object",
                    "properties": {
                        "pat": {"type": "string", "description": "ast pattern"},
                        "out": {"type": "string", "description": "replacement template"}
                    },
                    "required": ["pat", "out"],
                    "additionalProperties": false
                }
            },
            "paths": {
                "type": "array",
                "minItems": 1,
                "items": {"type": "string", "description": "file, directory, glob, or internal URL to rewrite"},
                "description": "files, directories, globs, or internal URLs to rewrite"
            }
        },
        "required": ["ops", "paths"],
        "additionalProperties": false
    })
}

/// Discover candidate source files under `entries` (files, directories, or
/// globs relative to `env.cwd`), normalizing display paths relative to `env.cwd`
/// and capping at [`MAX_AST_FILES`].
async fn scope_candidates(
    fs: &Fs,
    env: &Env,
    entries: &[String],
    lang: Option<SupportLang>,
) -> Result<(Vec<Candidate>, HashMap<String, PathBuf>)> {
    let mut by_path: HashMap<PathBuf, Candidate> = HashMap::new();
    for raw in entries {
        let resolved = resolve_to_cwd(raw, &env.cwd);
        let (root, include) = if resolved.exists() {
            (resolved, None)
        } else {
            let text = raw.replace('\\', "/");
            let cut = text.find(['*', '?', '[']).unwrap_or(text.len());
            let base_raw = text[..cut].trim_end_matches('/');
            let base = resolve_to_cwd(if base_raw.is_empty() { "." } else { base_raw }, &env.cwd);
            if !base.is_dir() {
                bail!("Path not found: {raw}");
            }
            let glob_part = &text[cut..];
            let selector = if glob_part.is_empty() {
                None
            } else {
                Some(
                    Selector::new(glob_part, false)
                        .map_err(|e| anyhow::anyhow!("Invalid glob {raw}: {e}"))?,
                )
            };
            (base, selector)
        };
        let opts = Discover {
            include,
            exclude: None,
            hidden: false,
            gitignore: true,
            node_modules: false,
            follow_links: false,
            workers: None,
            lang,
        };
        let found = discover(fs, &root, &opts)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for mut candidate in found {
            candidate.display = format_path_relative_to_cwd(&candidate.path, &env.cwd, false);
            by_path.entry(candidate.path.clone()).or_insert(candidate);
        }
    }
    let mut candidates: Vec<Candidate> = by_path.into_values().collect();
    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    candidates.truncate(MAX_AST_FILES);
    let mut display_to_path = HashMap::with_capacity(candidates.len());
    for c in &candidates {
        display_to_path.insert(c.display.clone(), c.path.clone());
    }
    Ok((candidates, display_to_path))
}

fn cap_warnings(warnings: Vec<String>) -> (Vec<String>, usize) {
    let total = warnings.len();
    let mut capped: Vec<String> = warnings.into_iter().take(MAX_PARSE_ERRORS).collect();
    if total > MAX_PARSE_ERRORS {
        capped.push(format!("… and {} more", total - MAX_PARSE_ERRORS));
    }
    (capped, total)
}

fn clamp_line(text: &str) -> String {
    text.split('\n')
        .next()
        .unwrap_or("")
        .chars()
        .take(DIFF_LINE_MAX)
        .collect()
}

fn append_diagnostics(
    model: &mut Vec<String>,
    warnings: Vec<String>,
    missing_grammars: &[&'static str],
) {
    let (capped, total) = cap_warnings(warnings);
    if !capped.is_empty() {
        model.push(String::new());
        model.push(format!(
            "Parse errors (showing {} of {total}):",
            capped.len()
        ));
        model.extend(capped);
    }
    if !missing_grammars.is_empty() {
        model.push(String::new());
        model.push(format!(
            "Skipped missing grammars: {}",
            missing_grammars.join(", ")
        ));
    }
}

/// `ast_grep`: structural search.
///
/// # Errors
/// Empty `pat`, invalid `skip`, unknown `lang`, unresolvable paths, or
/// cancellation.
#[allow(clippy::too_many_lines)]
pub async fn ast_grep(env: Arc<Env>, args: Value, signal: AbortSignal) -> Result<AgentToolResult> {
    throw_if_aborted(&signal)?;
    let pattern = str_arg(&args, "pat")?.trim().to_string();
    if pattern.is_empty() {
        bail!("`pat` must be a non-empty pattern");
    }
    let skip = match args.get("skip") {
        None | Some(Value::Null) => 0usize,
        Some(v) => match v.as_f64() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(n) if n.is_finite() && n >= 0.0 => n.floor() as usize,
            _ => bail!("skip must be a non-negative number"),
        },
    };
    let lang_str = args.get("lang").and_then(Value::as_str);
    let explicit_lang = lang_str.map(resolve_supported_lang).transpose()?;
    let mut entries = path_list(args.get("path").and_then(Value::as_str));
    if entries.is_empty() {
        entries.push(".".into());
    }

    let fs = Fs::native();
    let run = async {
        let (candidates, display_to_path) =
            scope_candidates(&fs, &env, &entries, explicit_lang).await?;
        let patterns = [pattern];
        let query = Query {
            patterns: &patterns,
            lang: lang_str,
            selector: None,
            strictness: MatchStrictness::Smart,
            captures: false,
        };
        let page = Page {
            offset: skip,
            limit: DEFAULT_AST_LIMIT,
        };
        find_page(&fs, candidates, &query, page)
            .await
            .map(|fp| (fp, display_to_path))
            .map_err(|e| anyhow::anyhow!("{e}"))
    };
    let (find_res, display_to_path) = tokio::select! {
        res = run => res?,
        () = signal.aborted() => return Err(aborted_error()),
    };
    if signal.is_aborted() {
        return Err(aborted_error());
    }

    let (capped_warnings, warnings_total) = cap_warnings(find_res.report.warnings.clone());
    if find_res.hits.is_empty() {
        let mut text = if capped_warnings.is_empty() {
            "No matches found".to_string()
        } else {
            "No matches found. Parse issues mean the query may be mis-scoped; narrow `path` before concluding absence."
                .to_string()
        };
        if !capped_warnings.is_empty() {
            text.push_str("\n\nParse errors (showing ");
            text.push_str(&capped_warnings.len().to_string());
            text.push_str(" of ");
            text.push_str(&warnings_total.to_string());
            text.push(':');
            text.extend(capped_warnings.iter().map(|e| format!("\n{e}")));
        }
        if !find_res.report.missing_grammars.is_empty() {
            text.push_str("\n\nSkipped missing grammars: ");
            text.push_str(&find_res.report.missing_grammars.join(", "));
        }
        return Ok(AgentToolResult::text(text));
    }

    // Render per file, mirroring the `grep` tool: hashline snapshot tags when
    // active, `*line:text` match rows, continuation lines space-marked.
    let mut model: Vec<String> = Vec::new();
    let mut idx = 0usize;
    while idx < find_res.hits.len() {
        let relative = &find_res.hits[idx].display;
        let mut end = idx + 1;
        while end < find_res.hits.len() && find_res.hits[end].display == *relative {
            end += 1;
        }
        if !model.is_empty() {
            model.push(String::new());
        }
        let absolute = display_to_path.get(relative);
        let tag = match (env.hashline, absolute) {
            (true, Some(abs)) => env.store.record_file(abs, None),
            _ => None,
        };
        if let Some(t) = &tag {
            model.push(format!("[{relative}#{t}]"));
        } else {
            model.push(format!("{relative}:"));
        }
        let mut lines: Vec<String> = Vec::new();
        for hit in &find_res.hits[idx..end] {
            let start_row = hit.matched.span.start_pos.line + 1;
            for (offset, line) in hit.matched.text.split('\n').enumerate() {
                let marker = if offset == 0 { '*' } else { ' ' };
                let row = start_row + offset;
                if tag.is_some() {
                    lines.push(format!("{marker}{row}:{line}"));
                } else {
                    lines.push(format!("{marker}{row}|{line}"));
                }
            }
        }
        if let (Some(t), Some(abs)) = (&tag, absolute) {
            env.store.record_seen_lines(
                abs,
                t,
                &pi_edit::store::seen_lines_from_body(&lines.join("\n")),
            );
        }
        model.extend(lines);
        idx = end;
    }
    if find_res.limit_reached {
        model.push(String::new());
        model.push("Result limit reached; narrow path or increase limit.".into());
    }
    append_diagnostics(
        &mut model,
        find_res.report.warnings,
        &find_res.report.missing_grammars,
    );
    Ok(AgentToolResult::text(model.join("\n")))
}

/// `ast_edit`: AST-aware rewrite. Applies matching rewrites per file (each file
/// is parsed in its own language; a pattern only rewrites files it parses in).
///
/// # Errors
/// Missing/empty/duplicate `ops` patterns, unresolvable paths, overlapping
/// replacements within one file, or cancellation.
#[allow(clippy::too_many_lines)]
pub async fn ast_edit(env: Arc<Env>, args: Value, signal: AbortSignal) -> Result<AgentToolResult> {
    throw_if_aborted(&signal)?;
    let ops = args
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("`ops` must include at least one op entry"))?;
    if ops.is_empty() {
        bail!("`ops` must include at least one op entry");
    }
    let mut rules: Vec<(String, String)> = Vec::with_capacity(ops.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (index, entry) in ops.iter().enumerate() {
        let pat = entry.get("pat").and_then(Value::as_str).unwrap_or_default();
        if pat.is_empty() {
            bail!("`ops[{index}].pat` must be a non-empty pattern");
        }
        let out = entry.get("out").and_then(Value::as_str).unwrap_or_default();
        if !seen.insert(pat.to_string()) {
            bail!("Duplicate rewrite pattern: {pat}");
        }
        rules.push((pat.to_string(), out.to_string()));
    }
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("`paths` must include at least one path"))?;
    let entries: Vec<String> = paths
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("`paths` entries must be strings"))
        })
        .collect::<Result<_>>()?;
    if entries.is_empty() {
        bail!("`paths` must include at least one path");
    }

    let fs = Fs::native();
    let run = async {
        let (candidates, _) = scope_candidates(&fs, &env, &entries, None).await?;
        let rw = Rewrite {
            rules: &rules,
            lang: None,
            selector: None,
            strictness: MatchStrictness::Smart,
            max_replacements: usize::MAX,
            max_files: MAX_AST_FILES,
            fail_on_parse_error: false,
            skip_overlaps: false,
            mode: Mode::Write,
        };
        rewrite(&fs, candidates, &rw)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    };
    let edited = tokio::select! {
        res = run => res?,
        () = signal.aborted() => return Err(aborted_error()),
    };
    if signal.is_aborted() {
        return Err(aborted_error());
    }

    let mut total_replacements = 0usize;
    let mut sections: Vec<String> = Vec::new();
    for file_edit in &edited.files {
        if file_edit.changes.is_empty() {
            continue;
        }
        total_replacements += file_edit.changes.len();
        sections.push(format!("{}:", file_edit.display));
        for change in &file_edit.changes {
            let line = change.span.start_pos.line + 1;
            let col = change.span.start_pos.column + 1;
            sections.push(format!("-{line}:{col} {}", clamp_line(&change.before)));
            sections.push(format!("+{line}:{col} {}", clamp_line(&change.after)));
        }
    }

    if total_replacements == 0 {
        let (capped, total) = cap_warnings(edited.report.warnings);
        let mut text = "No replacements made".to_string();
        if !capped.is_empty() {
            text.push_str("\n\nParse errors (showing ");
            text.push_str(&capped.len().to_string());
            text.push_str(" of ");
            text.push_str(&total.to_string());
            text.push(':');
            text.extend(capped.iter().map(|e| format!("\n{e}")));
        }
        if !edited.report.missing_grammars.is_empty() {
            text.push_str("\n\nSkipped missing grammars: ");
            text.push_str(&edited.report.missing_grammars.join(", "));
        }
        return Ok(AgentToolResult::text(text));
    }

    let files_touched = edited
        .files
        .iter()
        .filter(|f| !f.changes.is_empty())
        .count();
    let mut model = vec![format!(
        "Applied {total_replacements} replacement{} in {files_touched} file{}.",
        if total_replacements == 1 { "" } else { "s" },
        if files_touched == 1 { "" } else { "s" }
    )];
    model.extend(sections);
    append_diagnostics(
        &mut model,
        edited.report.warnings,
        &edited.report.missing_grammars,
    );
    Ok(AgentToolResult::text(model.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Env;

    #[allow(clippy::match_wildcard_for_single_variants)]
    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn env(dir: &std::path::Path) -> Arc<Env> {
        Arc::new(Env::new(dir, false))
    }

    /// Rust + TS fixtures: two functions in Rust, two calls in TS.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() { beta(1); }\nfn beta(x: i32) { x + 1 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.ts"),
            "function alpha() { beta(1); }\nfunction beta(x: number) { return x + 1 }\n",
        )
        .unwrap();
        dir
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_grep_finds_matches_across_languages() {
        let dir = fixture();
        let result = ast_grep(
            env(dir.path()),
            json!({"pat": "beta($A)", "path": "src"}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(text.contains("src/app.ts:"), "{text}");
        assert!(text.contains("src/lib.rs:"), "{text}");
        assert!(text.contains("*1|beta(1)"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_grep_lang_override_scopes_to_one_language() {
        let dir = fixture();
        let result = ast_grep(
            env(dir.path()),
            json!({"pat": "function alpha() { beta($A); }", "path": "src", "lang": "typescript"}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(text.contains("src/app.ts:"), "{text}");
        assert!(text.contains("*1|function alpha() { beta(1); }"), "{text}");
        assert!(!text.contains("*1|fn alpha"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_grep_skip_paginates_matches() {
        let dir = fixture();
        let e = env(dir.path());
        let all = text_of(
            &ast_grep(
                e.clone(),
                json!({"pat": "$F($A)", "path": "src"}),
                AbortSignal::never(),
            )
            .await
            .unwrap(),
        );
        let skipped = text_of(
            &ast_grep(
                e,
                json!({"pat": "$F($A)", "path": "src", "skip": 1}),
                AbortSignal::never(),
            )
            .await
            .unwrap(),
        );
        assert!(
            all.lines().count() > skipped.lines().count(),
            "{all}\n---\n{skipped}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_grep_no_match_and_empty_pattern() {
        let dir = fixture();
        let e = env(dir.path());
        let miss = text_of(
            &ast_grep(
                e.clone(),
                json!({"pat": "nonexistent($$$)"}),
                AbortSignal::never(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(miss, "No matches found");
        assert!(ast_grep(e, json!({"pat": "   "}), AbortSignal::never())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_edit_rewrites_rust_and_ts_fixtures() {
        let dir = fixture();
        let e = env(dir.path());
        let result = ast_edit(
            e,
            json!({"ops": [{"pat": "beta($A)", "out": "gamma($A)"}], "paths": ["src"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Applied 2 replacements in 2 files."),
            "{text}"
        );
        assert!(text.contains("-1:14 beta(1)"), "{text}");
        assert!(text.contains("+1:14 gamma(1)"), "{text}");
        assert!(text.contains("+1:20 gamma(1)"), "{text}");
        let rs = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        assert!(rs.contains("gamma(1)"), "{rs}");
        let ts = std::fs::read_to_string(dir.path().join("src/app.ts")).unwrap();
        assert!(ts.contains("gamma(1)"), "{ts}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_edit_delete_op_and_no_match() {
        let dir = fixture();
        let e = env(dir.path());
        let result = ast_edit(
            e.clone(),
            json!({"ops": [{"pat": "beta($A)", "out": ""}], "paths": ["src/lib.rs"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        assert!(text_of(&result).contains("Applied 1 replacement in 1 file."));
        let miss = ast_edit(
            e,
            json!({"ops": [{"pat": "nope($$$)", "out": "x"}], "paths": ["src"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap();
        assert_eq!(text_of(&miss), "No replacements made");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ast_edit_validates_ops() {
        let dir = fixture();
        let e = env(dir.path());
        let dup = ast_edit(
            e.clone(),
            json!({"ops": [
                {"pat": "beta($A)", "out": "x"},
                {"pat": "beta($A)", "out": "y"}
            ], "paths": ["src"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap_err();
        assert!(
            dup.to_string().contains("Duplicate rewrite pattern"),
            "{dup}"
        );
        let empty = ast_edit(
            e,
            json!({"ops": [{"pat": "", "out": "x"}], "paths": ["src"]}),
            AbortSignal::never(),
        )
        .await
        .unwrap_err();
        assert!(
            empty.to_string().contains("must be a non-empty pattern"),
            "{empty}"
        );
    }
}
