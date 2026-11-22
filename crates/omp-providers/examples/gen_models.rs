//! Regenerate the compiled fallback table (`vendor/pa-ai/src/models_generated.rs` `RAW_MODELS`)
//! from the committed catalog snapshots:
//!
//! * `crates/omp-cli/assets/tui-model-catalog.json` - per-model name / reasoning / vision /
//!   context / max tokens / input, output and cache-read cost;
//! * `vendor/pa-ai/data/compat/rules.json` - provider seeds (api, baseUrl, cache-write cost for
//!   `--add`) and the rules fingerprint stamped into `CATALOG_RULES_FINGERPRINT`;
//! * `vendor/pa-ai/data/compat/axes.json` - the known `api` ids every row must use (plus
//!   `NATIVE_APIS`).
//!
//! Existing rows are refreshed in place: catalog-owned fields are rewritten, the hand-authored
//! transport fields (api, base_url, thinking levels, headers, compat, featured) are kept, and rows
//! the catalog does not carry (e.g. the Prime Inference offline floor) are kept verbatim.
//!
//! ```text
//! cargo run -p omp-providers --example gen_models              # dry run: print the change summary
//! cargo run -p omp-providers --example gen_models -- --check   # exit 1 when the table is stale
//! cargo run -p omp-providers --example gen_models -- --write   # rewrite models_generated.rs
//! cargo run -p omp-providers --example gen_models -- --stdout  # print the regenerated file
//! cargo run -p omp-providers --example gen_models -- --write --add openai/gpt-5.5
//! ```

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde_json::Value;

const TABLE_START: &str = "static RAW_MODELS: &[RawModel] = &[\n";
const TABLE_END: &str = "\n];\n";
const ENTRY_START: &str = "    RawModel {\n";
const ENTRY_END: &str = "    },\n";
const FINGERPRINT_PREFIX: &str = "pub const CATALOG_RULES_FINGERPRINT: &str = ";
/// Apis pa-ai streams with a dedicated provider outside the compat axes
/// (`providers::mistral::API_MISTRAL_CONVERSATIONS`).
const NATIVE_APIS: &[&str] = &["mistral-conversations"];

/// One slim TUI catalog row: `[id, name, reasoning, vision, ctx, maxTokens, $in, $out, $cache, kind, priority]`.
struct CatalogRow {
    name: String,
    reasoning: bool,
    vision: bool,
    context_window: u64,
    max_tokens: u64,
    cost: (f64, f64, f64),
}

impl CatalogRow {
    fn parse(row: &Value) -> Option<(String, Self)> {
        let row = row.as_array()?;
        let flag = |i: usize| row.get(i).and_then(Value::as_u64).is_some_and(|v| v != 0);
        let num = |i: usize| row.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let id = row.first()?.as_str()?.to_owned();
        Some((
            id.clone(),
            Self {
                name: row.get(1).and_then(Value::as_str).unwrap_or(&id).to_owned(),
                reasoning: flag(2),
                vision: flag(3),
                context_window: row.get(4).and_then(Value::as_u64).unwrap_or(0),
                max_tokens: row.get(5).and_then(Value::as_u64).unwrap_or(0),
                cost: (num(6), num(7), num(8)),
            },
        ))
    }
}

struct Sources {
    catalog: HashMap<String, HashMap<String, CatalogRow>>,
    rules: Value,
    fingerprint: String,
    apis: BTreeSet<String>,
}

impl Sources {
    fn load(root: &Path) -> Result<Self> {
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).with_context(|| format!("reading {rel}"))
        };
        let rules_text = read("vendor/pa-ai/data/compat/rules.json")?;
        let fingerprint = omp_providers::catalog::rules_fingerprint(&rules_text)?;
        let rules: Value = serde_json::from_str(&rules_text)?;
        let axes: Value = serde_json::from_str(&read("vendor/pa-ai/data/compat/axes.json")?)?;
        let apis = axes["apiRecords"]
            .as_object()
            .context("axes.json has no apiRecords")?
            .keys()
            .cloned()
            .chain(NATIVE_APIS.iter().map(|api| (*api).to_owned()))
            .collect();
        let raw: serde_json::Map<String, Value> =
            serde_json::from_str(&read("crates/omp-cli/assets/tui-model-catalog.json")?)?;
        let catalog = raw
            .iter()
            .map(|(provider, rows)| {
                let rows = rows
                    .as_array()
                    .map(|rows| rows.iter().filter_map(CatalogRow::parse).collect())
                    .unwrap_or_default();
                (provider.clone(), rows)
            })
            .collect();
        Ok(Self {
            catalog,
            rules,
            fingerprint,
            apis,
        })
    }

    fn row(&self, provider: &str, id: &str) -> Option<&CatalogRow> {
        self.catalog.get(provider)?.get(id)
    }

    fn seed(&self, provider: &str, id: &str) -> Option<&Value> {
        self.rules["providers"][provider]["seed"]["models"]
            .as_array()?
            .iter()
            .find(|m| m["id"].as_str() == Some(id))
    }
}

/// A table entry as source text plus the key it sorts by.
struct Entry {
    provider: String,
    id: String,
    text: String,
}

fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("        {name}: ");
    text.lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(|rest| rest.trim_end_matches(','))
}

fn unquote(literal: &str) -> Result<String> {
    serde_json::from_str(literal).with_context(|| format!("not a string literal: {literal}"))
}

fn parse_entries(body: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(ENTRY_START) {
        let end = rest[start..]
            .find(ENTRY_END)
            .map(|i| start + i + ENTRY_END.len())
            .context("unterminated RawModel entry")?;
        let text = rest[start..end].to_owned();
        let provider = unquote(field(&text, "provider").context("entry without provider")?)?;
        let id = unquote(field(&text, "id").context("entry without id")?)?;
        entries.push(Entry { provider, id, text });
        rest = &rest[end..];
    }
    Ok(entries)
}

/// `128_000`-style integer literal.
fn int_literal(v: u64) -> String {
    let digits = v.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

fn input_literal(vision: bool) -> &'static str {
    if vision {
        "&[crate::types::ModelInput::Text, crate::types::ModelInput::Image]"
    } else {
        "&[crate::types::ModelInput::Text]"
    }
}

fn cost_literal(row: &CatalogRow, cache_write: f64) -> String {
    format!(
        "({:?}, {:?}, {:?}, {:?})",
        row.cost.0, row.cost.1, row.cost.2, cache_write
    )
}

fn existing_cache_write(text: &str) -> f64 {
    field(text, "cost")
        .and_then(|c| {
            c.trim_matches(|ch| ch == '(' || ch == ')')
                .rsplit(',')
                .next()
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0)
}

/// Rewrite the catalog-owned lines of an existing entry; every other line is kept.
fn refresh(entry: &Entry, row: &CatalogRow) -> String {
    let cost = cost_literal(row, existing_cache_write(&entry.text));
    let replacements = [
        ("name", format!("{:?}", row.name)),
        ("reasoning", row.reasoning.to_string()),
        ("input", input_literal(row.vision).to_owned()),
        ("cost", cost),
        ("context_window", int_literal(row.context_window)),
        ("max_tokens", int_literal(row.max_tokens)),
    ];
    let mut out = String::with_capacity(entry.text.len());
    for line in entry.text.split_inclusive('\n') {
        let replaced = replacements.iter().find_map(|(name, value)| {
            line.starts_with(&format!("        {name}: "))
                .then(|| format!("        {name}: {value},\n"))
        });
        out.push_str(replaced.as_deref().unwrap_or(line));
    }
    out
}

/// A new entry for `provider/id`: catalog fields plus the seed (or same-provider) transport.
fn new_entry(sources: &Sources, entries: &[Entry], key: &str) -> Result<Entry> {
    let (provider, id) = key
        .split_once('/')
        .with_context(|| format!("--add expects provider/id, got {key}"))?;
    let row = sources
        .row(provider, id)
        .with_context(|| format!("{key} is not in tui-model-catalog.json"))?;
    let seed = sources.seed(provider, id);
    let sibling = entries.iter().find(|e| e.provider == provider);
    let (api, base_url) = match (seed, sibling) {
        (Some(seed), _) => (
            seed["api"].as_str().context("seed without api")?.to_owned(),
            seed["baseUrl"]
                .as_str()
                .context("seed without baseUrl")?
                .to_owned(),
        ),
        (None, Some(sibling)) => (
            unquote(field(&sibling.text, "api").context("sibling without api")?)?,
            unquote(field(&sibling.text, "base_url").context("sibling without base_url")?)?,
        ),
        (None, None) => bail!(
            "{key}: no rules.json seed and no compiled {provider} row to take api/baseUrl from"
        ),
    };
    let cache_write = seed
        .and_then(|s| s["cost"]["cacheWrite"].as_f64())
        .unwrap_or(0.0);
    let text = format!(
        "{ENTRY_START}        provider: {provider:?},\n        id: {id:?},\n        name: {name:?},\n        \
         api: {api:?},\n        base_url: {base_url:?},\n        reasoning: {reasoning},\n        \
         thinking_levels: &[],\n        input: {input},\n        cost: {cost},\n        \
         context_window: {ctx},\n        max_tokens: {max},\n        featured: None,\n        \
         headers: &[],\n        compat: None,\n{ENTRY_END}",
        name = row.name,
        reasoning = row.reasoning,
        input = input_literal(row.vision),
        cost = cost_literal(row, cache_write),
        ctx = int_literal(row.context_window),
        max = int_literal(row.max_tokens),
    );
    Ok(Entry {
        provider: provider.to_owned(),
        id: id.to_owned(),
        text,
    })
}

fn regenerate(source: &str, sources: &Sources, add: &[String]) -> Result<String> {
    let start = source
        .find(TABLE_START)
        .context("RAW_MODELS table not found")?
        + TABLE_START.len();
    let end = start
        + source[start..]
            .find(TABLE_END)
            .context("RAW_MODELS end not found")?;
    // Include the newline that starts TABLE_END so the last entry ends with ENTRY_END.
    let mut entries = parse_entries(&source[start..=end])?;

    let (mut refreshed, mut changed, mut kept) = (0, 0, Vec::new());
    for entry in &mut entries {
        match sources.row(&entry.provider, &entry.id) {
            Some(row) => {
                let text = refresh(entry, row);
                refreshed += 1;
                if text != entry.text {
                    changed += 1;
                    entry.text = text;
                }
            }
            None => kept.push(format!("{}/{}", entry.provider, entry.id)),
        }
    }
    for key in add {
        if entries
            .iter()
            .any(|e| format!("{}/{}", e.provider, e.id) == *key)
        {
            bail!("--add {key}: already in the table");
        }
        let entry = new_entry(sources, &entries, key)?;
        entries.push(entry);
    }
    entries.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));

    for entry in &entries {
        let api = unquote(field(&entry.text, "api").context("entry without api")?)?;
        if !sources.apis.contains(&api) {
            bail!(
                "{}/{}: api {api} is not in axes.json apiRecords",
                entry.provider,
                entry.id
            );
        }
    }
    eprintln!(
        "gen_models: {} rows ({refreshed} from the catalog, {changed} changed, {} kept verbatim, {} added); {}",
        entries.len(),
        kept.len(),
        add.len(),
        sources.fingerprint,
    );
    if !kept.is_empty() {
        eprintln!("  kept (not in tui catalog): {}", kept.join(", "));
    }

    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..start]);
    for entry in &entries {
        out.push_str(&entry.text);
    }
    // The body ends at the last entry's `},`; TABLE_END re-adds the newline before `];`.
    out.truncate(out.trim_end_matches('\n').len());
    out.push_str(&source[end..]);

    let stamp = format!("{FINGERPRINT_PREFIX}{:?};", sources.fingerprint);
    let line = out
        .lines()
        .find(|l| l.starts_with(FINGERPRINT_PREFIX))
        .context("CATALOG_RULES_FINGERPRINT constant not found")?
        .to_owned();
    Ok(out.replacen(&line, &stamp, 1))
}

fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (mut write, mut check, mut stdout, mut add) = (false, false, false, Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write" => write = true,
            "--check" => check = true,
            "--stdout" => stdout = true,
            "--add" => add.push(args.next().context("--add needs provider/id")?),
            other => bail!(
                "unknown argument {other} (use --write, --check, --stdout, --add provider/id)"
            ),
        }
    }
    let target = root.join("vendor/pa-ai/src/models_generated.rs");
    let source = std::fs::read_to_string(&target)
        .with_context(|| format!("reading {}", target.display()))?;
    let sources = Sources::load(&root)?;
    let generated = regenerate(&source, &sources, &add)?;

    if stdout {
        print!("{generated}");
    }
    if check && generated != source {
        bail!("models_generated.rs is stale; run `cargo run -p omp-providers --example gen_models -- --write`");
    }
    if write && generated != source {
        std::fs::write(&target, &generated)?;
        eprintln!("gen_models: wrote {}", target.display());
    }
    Ok(())
}
