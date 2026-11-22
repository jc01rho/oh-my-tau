//! `~/.omo/omo.jsonc` JSONC loader, profile selector, and `omp-settings` translator.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const OMO_CONFIG_FILENAMES: &[&str] = &["omo.jsonc", "omo.json"];

pub const OMO_CONTROL_KEYS: &[&str] = &["$schema", "profiles", "[senpi]", "[native]"];

pub const OMO_KNOWN_NAMESPACES: &[&str] = &[
    "$schema",
    "profiles",
    "[senpi]",
    "[native]",
    "model_profile",
    "model_profiles",
    "models",
    "categories",
    "memory",
    "agents",
    "teams",
    "git_master",
    "_migrations",
    "telemetry",
    "provenance",
];

#[derive(Debug, Clone, Default)]
pub struct OmoLayer {
    pub path: PathBuf,
    pub translated: Map<String, Value>,
    pub raw_merged: Map<String, Value>,
    pub active_profile: Option<String>,
    pub diagnostics: Vec<String>,
}

#[must_use]
pub fn find_omo_config(omo_dir: &Path) -> Option<PathBuf> {
    for name in OMO_CONFIG_FILENAMES {
        let candidate = omo_dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[must_use]
pub fn resolve_profile_name(
    explicit: Option<&str>,
    from_omo_env: Option<&str>,
    from_legacy_env: Option<&str>,
) -> Option<String> {
    for candidate in [explicit, from_omo_env, from_legacy_env] {
        if let Some(trimmed) = candidate.map(str::trim).filter(|s| !s.is_empty()) {
            return Some(trimmed.to_owned());
        }
    }
    None
}

#[must_use]
pub fn active_profile_from_env() -> Option<String> {
    let omo = std::env::var("OMO_PROFILE").ok();
    let omp = std::env::var("OMP_PROFILE").ok();
    resolve_profile_name(None, omo.as_deref(), omp.as_deref())
}

/// Parse a JSONC document supporting line/block comments and trailing commas.
///
/// # Errors
///
/// Returns an error string when a block comment is unterminated or the JSON is invalid.
pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let no_comments = strip_jsonc_comments(text)?;
    let cleaned = strip_trailing_commas(&no_comments);
    serde_json::from_str::<Value>(&cleaned).map_err(|e| e.to_string())
}

fn strip_jsonc_comments(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    let _ = chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    let _ = chars.next();
                    let mut closed = false;
                    while let Some(next) = chars.next() {
                        if next == '\n' {
                            out.push('\n');
                        } else if next == '*' && chars.peek() == Some(&'/') {
                            let _ = chars.next();
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Err("unterminated block comment in JSONC".to_owned());
                    }
                    continue;
                }
                _ => {}
            }
        }

        out.push(ch);
    }

    Ok(out)
}

fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let mut in_string = false;

    while let Some((idx, ch)) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some((_, escaped)) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let rest = &text[idx + ch.len_utf8()..];
            let next_non_ws = rest.bytes().find(|b| !b.is_ascii_whitespace());
            if matches!(next_non_ws, Some(b'}' | b']')) {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

#[must_use]
pub fn load_omo_file(path: &Path, profile_name: Option<&str>) -> Option<OmoLayer> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_omo_str(&text, path, profile_name)
}

#[must_use]
pub fn parse_omo_str(text: &str, path: &Path, profile_name: Option<&str>) -> Option<OmoLayer> {
    let value = parse_jsonc(text).ok()?;
    let root = value.as_object()?;
    let mut diagnostics = Vec::new();

    let base_clean = strip_control_keys(root);
    let mut raw_merged = base_clean.clone();
    let mut translated = translate_omo_block(&base_clean, &mut diagnostics);

    let mut active_profile = None;
    let mut profile_obj_ref: Option<&Map<String, Value>> = None;
    if let Some(name) = profile_name.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(p_obj) = root
            .get("profiles")
            .and_then(Value::as_object)
            .and_then(|profiles| profiles.get(name))
            .and_then(Value::as_object)
        {
            let p_clean = strip_control_keys(p_obj);
            deep_merge_omo(&mut raw_merged, p_clean.clone());
            deep_merge_omo(
                &mut translated,
                translate_omo_block(&p_clean, &mut diagnostics),
            );
            active_profile = Some(name.to_owned());
            profile_obj_ref = Some(p_obj);
        } else {
            diagnostics.push(format!(
                "profiles.{name}: profile not found in {}; using base settings",
                path.display()
            ));
        }
    }

    for harness_key in ["[senpi]", "[native]"] {
        if let Some(top_overlay) = root.get(harness_key).and_then(Value::as_object) {
            let clean = strip_control_keys(top_overlay);
            deep_merge_omo(&mut raw_merged, clean.clone());
            deep_merge_omo(
                &mut translated,
                translate_omo_block(&clean, &mut diagnostics),
            );
        }
        if let Some(prof_overlay) =
            profile_obj_ref.and_then(|p| p.get(harness_key).and_then(Value::as_object))
        {
            let clean = strip_control_keys(prof_overlay);
            deep_merge_omo(&mut raw_merged, clean.clone());
            deep_merge_omo(
                &mut translated,
                translate_omo_block(&clean, &mut diagnostics),
            );
        }
    }

    Some(OmoLayer {
        path: path.to_path_buf(),
        translated,
        raw_merged,
        active_profile,
        diagnostics,
    })
}

fn strip_control_keys(map: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in map {
        if OMO_CONTROL_KEYS.contains(&k.as_str()) {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

#[must_use]
pub fn translate_omo_block(
    block: &Map<String, Value>,
    diagnostics: &mut Vec<String>,
) -> Map<String, Value> {
    let mut out = Map::new();

    for (k, v) in block {
        if k == "task" || OMO_KNOWN_NAMESPACES.contains(&k.as_str()) {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }

    if let Some(task_obj) = block.get("task").and_then(Value::as_object) {
        let task_out = translate_task_section(task_obj, diagnostics);
        if !task_out.is_empty() {
            out.insert("task".to_owned(), Value::Object(task_out));
        }
    }

    let mut model_overrides = Map::new();
    for section in ["categories", "agents"] {
        if let Some(entries) = block.get(section).and_then(Value::as_object) {
            for (name, spec) in entries {
                if let Some(selector) = primary_model_selector(spec) {
                    model_overrides.insert(name.clone(), Value::String(selector));
                }
            }
        }
    }
    if !model_overrides.is_empty() {
        let task_entry = out
            .entry("task".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(task_map) = task_entry.as_object_mut() {
            let overrides_entry = task_map
                .entry("agentModelOverrides".to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(existing) = overrides_entry.as_object_mut() {
                for (k, v) in model_overrides {
                    existing.entry(k).or_insert(v);
                }
            }
        }
    }

    out
}

fn translate_task_section(
    task_obj: &Map<String, Value>,
    diagnostics: &mut Vec<String>,
) -> Map<String, Value> {
    let mut task_out = Map::new();

    for (tk, tv) in task_obj {
        if matches!(
            tk.as_str(),
            "default_concurrency"
                | "global_concurrency"
                | "max_depth"
                | "wait"
                | "default_execution_mode"
                | "provider_concurrency"
                | "model_concurrency"
                | "residency_max_children"
                | "dag"
                | "ttl_ms"
                | "team"
        ) {
            continue;
        }
        let candidate_id = format!("task.{tk}");
        if crate::lookup(&candidate_id).is_some() || tk == "isolation" {
            task_out.insert(tk.clone(), tv.clone());
        }
    }

    if let Some(dc) = task_obj.get("default_concurrency") {
        if dc.is_number() {
            if dc.as_f64() == Some(0.0) {
                diagnostics.push(
                    "task.default_concurrency=0 mapped to task.maxConcurrency=0 (auto)".to_owned(),
                );
            }
            if !task_obj.contains_key("maxConcurrency") {
                task_out.insert("maxConcurrency".to_owned(), dc.clone());
            }
        }
    }

    if let Some(gc) = task_obj.get("global_concurrency") {
        if gc.is_number() && !task_obj.contains_key("globalConcurrency") {
            task_out.insert("globalConcurrency".to_owned(), gc.clone());
        }
    }

    if let Some(md) = task_obj.get("max_depth") {
        if md.is_number() && !task_obj.contains_key("maxRecursionDepth") {
            task_out.insert("maxRecursionDepth".to_owned(), md.clone());
        }
    }

    if let Some(wait_obj) = task_obj.get("wait").and_then(Value::as_object) {
        let mut wait_out = Map::new();
        for (src, dst) in [
            ("min_ms", "minMs"),
            ("default_ms", "defaultMs"),
            ("max_ms", "maxMs"),
            ("minMs", "minMs"),
            ("defaultMs", "defaultMs"),
            ("maxMs", "maxMs"),
        ] {
            if let Some(val) = wait_obj.get(src) {
                if val.is_number() {
                    wait_out.insert(dst.to_owned(), val.clone());
                }
            }
        }
        if !wait_out.is_empty() {
            task_out.insert("wait".to_owned(), Value::Object(wait_out));
        }
    }

    task_out
}

fn primary_model_selector(spec: &Value) -> Option<String> {
    let obj = spec.as_object()?;
    if let Some(m) = obj.get("model").and_then(Value::as_str) {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    let first = obj.get("models")?.as_array()?.first()?.as_object()?;
    let model = first.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    Some(model.to_owned())
}

fn deep_merge_omo(base: &mut Map<String, Value>, over: Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Object(base_obj)), Value::Object(over_obj)) => {
                deep_merge_omo(base_obj, over_obj);
            }
            (_, Value::Null) => {}
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}
