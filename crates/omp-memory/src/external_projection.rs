//! Bounded `<external_projection>` rendering for non-`system/` memory files.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;

use crate::projection::redact_secret_like_material;

const MEMORY_DIR: &str = "$MEMORY_DIR";
const OPEN: &str = "<external_projection>";
const CLOSE: &str = "</external_projection>";
const MARKDOWN: &str = ".md";
const MIN_SIBLINGS: usize = 3;
const MIN_HEX_FLIPS: usize = 4;
const MIN_NOISE: f64 = 0.25;
const MIN_RARE_PAIRS: f64 = 0.3;
const MIN_WORD_DENSITY: f64 = 0.4;
const MIN_RUN: usize = 3;
const ID: &str = "<id>";
const COMMON_PAIRS: &str = "th he in er an re on at en nd ti es or te of ed is it al ar st to nt ng se ha as ou io le ve co me de hi ri ro ic ne ea ra ce li ch ll be ma si om ur ca el ta la ns di fo ho pe ec pr no ct us ac ot il tr ly nc et ut ss so rs un lo wa ge ie wh ee wi em ad ol rt po we na ul ni ts mo ow pa im mi ai sh ir su id os iv ia am fi ci vi pl ig tu ev ld ry mp fe bl ab gh ty op wo sa ay ex ke fr oo av ag if ap gr od bo sp rd do uc bu ei ov by rm ep tt oc fa ef cu rn sc gi da yo cr cl du ga qu ue ff ba ey ls va um pp ua up lu go ht ru ug ds lt pi rc rr eg au ck ew mu br bi pt ak pu ui rg ib tl ny ki rk ys ob mm fu ph og ms ye ud mb ip ub oi rl gu dr hr cc tw ft wn nu af hu nn eo vo rv nf xp gn sm fl iz ok nl my gl aw sy oa";

pub const NAME_NOTE: &str = "Names drop \".md\" (a quoted name is exact).";
pub const NAME_LEGEND: &str = "Names drop \".md\" (a quoted name is exact); {a, b}-20261027 is a-20261027 and b-20261027; \"x … y (N files)\" and <id> stand for similar names: read the directory to list them.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ExternalProjectionLimits {
    pub max_entries_per_directory: usize,
    pub max_bytes: usize,
}

impl Default for ExternalProjectionLimits {
    fn default() -> Self {
        Self {
            max_entries_per_directory: 0,
            max_bytes: 40_960,
        }
    }
}

#[derive(Debug, Default)]
pub struct ExternalProjectionInput {
    pub times: BTreeMap<String, i64>,
    pub limits: ExternalProjectionLimits,
    pub recency_unavailable: bool,
    pub recency_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct NamedFile {
    name: String,
    time: f64,
    files: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct ProjectionItem {
    text: String,
    files: usize,
    time: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct DirectoryGroup {
    label: String,
    pointer: String,
    files: Vec<NamedFile>,
    grouped: bool,
}

struct SiblingGroup {
    members: Vec<String>,
    line: DirectoryGroup,
}

struct DirectoryListing {
    label: String,
    pointer: String,
    items: Vec<ProjectionItem>,
    item_bytes: Vec<usize>,
}

struct Layout {
    root_bare: bool,
    legend: bool,
    directories: Vec<DirectoryListing>,
    allowances: Vec<usize>,
    /// How many rows of `directories` the section keeps; the rest are omitted whole.
    kept: usize,
}

/// Locale-style case-insensitive comparison (`a.localeCompare(b)`).
#[must_use]
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let al = a.to_lowercase();
    let bl = b.to_lowercase();
    al.cmp(&bl).then_with(|| a.cmp(b))
}

fn natural_compare(a: &str, b: &str) -> Ordering {
    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();
    while let (Some(&(ap, ac)), Some(&(bp, bc))) = (ai.peek(), bi.peek()) {
        if ac.is_ascii_digit() && bc.is_ascii_digit() {
            let a_end = a[ap..]
                .find(|c: char| !c.is_ascii_digit())
                .map_or(a.len(), |i| ap + i);
            let b_end = b[bp..]
                .find(|c: char| !c.is_ascii_digit())
                .map_or(b.len(), |i| bp + i);
            let an = a[ap..a_end].trim_start_matches('0');
            let bn = b[bp..b_end].trim_start_matches('0');
            let cmp = an.len().cmp(&bn.len()).then_with(|| an.cmp(bn));
            if cmp != Ordering::Equal {
                return cmp;
            }
            while ai.peek().is_some_and(|&(idx, _)| idx < a_end) {
                ai.next();
            }
            while bi.peek().is_some_and(|&(idx, _)| idx < b_end) {
                bi.next();
            }
        } else {
            let cmp = ac
                .to_lowercase()
                .cmp(bc.to_lowercase())
                .then_with(|| ac.cmp(&bc));
            if cmp != Ordering::Equal {
                return cmp;
            }
            ai.next();
            bi.next();
        }
    }
    ai.next().is_some().cmp(&bi.next().is_some())
}

fn re(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("static pattern"))
}

fn date_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^(?:19|20)[0-9]{6}$")
}

fn year_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^(?:19|20)[0-9]{2}$")
}

fn month_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^(?:0[1-9]|1[0-2])$")
}

fn hint_free_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(
        &CELL,
        r"(?i)^(?:(?:[01][0-9]|2[0-3])[0-5][0-9](?:[0-5][0-9])?(?:z|utc|gmt|kst|jst|pst|pdt|est|edt|cet|cest|bst|ist)?|[0-9]{1,2}|p[0-9]+)$",
    )
}

fn date_suffix_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^(.+)-((?:19|20)[0-9]{6})$")
}

fn uuid_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"(?i)^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$")
}

fn hex_digest_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"^(?:[0-9a-f]{16,}|[0-9A-F]{16,})$")
}

fn brace_or_comma_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    re(&CELL, r"[{}]|, ")
}

fn common_pairs() -> &'static HashSet<&'static str> {
    static CELL: OnceLock<HashSet<&'static str>> = OnceLock::new();
    CELL.get_or_init(|| COMMON_PAIRS.split(' ').collect())
}

fn newest_first(a: &ProjectionItem, b: &ProjectionItem) -> Ordering {
    let delta = b.time - a.time;
    if delta != 0.0 && !delta.is_nan() {
        return delta.partial_cmp(&0.0).unwrap_or(Ordering::Equal);
    }
    locale_compare(&a.text, &b.text)
}

fn generated_ids<S: AsRef<str>>(names: &[S]) -> HashSet<String> {
    names
        .iter()
        .map(AsRef::as_ref)
        .filter(|name| uuid_re().is_match(name) || is_hex_digest(name) || reads_as_random(name))
        .map(str::to_string)
        .collect()
}

fn is_hex_digest(name: &str) -> bool {
    if !hex_digest_re().is_match(name) {
        return false;
    }
    let bytes = name.as_bytes();
    let flips = bytes
        .windows(2)
        .filter(|pair| pair[0].is_ascii_digit() != pair[1].is_ascii_digit())
        .count();
    flips >= MIN_HEX_FLIPS
}

fn words(name: &str) -> Vec<&str> {
    let bytes = name.as_bytes();
    let mut words = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let run = |from: usize, test: fn(&u8) -> bool| {
            from + bytes[from..].iter().take_while(|b| test(b)).count()
        };
        let end = if byte.is_ascii_uppercase() {
            let upper_end = run(at, u8::is_ascii_uppercase);
            let next_lower = bytes.get(upper_end).is_some_and(u8::is_ascii_lowercase);
            if upper_end - at >= 2 && next_lower {
                upper_end - 1
            } else if upper_end - at == 1 && next_lower {
                run(at + 1, u8::is_ascii_lowercase)
            } else {
                upper_end
            }
        } else if byte.is_ascii_lowercase() {
            run(at, u8::is_ascii_lowercase)
        } else if byte.is_ascii_digit() {
            run(at, u8::is_ascii_digit)
        } else {
            at + 1
        };
        if byte.is_ascii_alphanumeric() {
            words.push(&name[at..end]);
        }
        at = end;
    }
    words
}

fn reads_as_random(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 12
        || !bytes.iter().all(u8::is_ascii_alphanumeric)
        || !bytes.iter().any(|b| b.is_ascii_lowercase())
        || !bytes.iter().any(|b| b.is_ascii_uppercase())
    {
        return false;
    }
    let words = words(name);
    let (mut marks, mut pairs, mut rare) = (0usize, 0usize, 0usize);
    for word in &words {
        if word.bytes().all(|b| b.is_ascii_digit()) {
            marks += usize::from(word.len() <= 2);
            continue;
        }
        if word.len() == 1 {
            marks += 1;
        } else if !word.bytes().any(|b| b"aeiouyAEIOUY".contains(&b)) {
            marks += if word.len() >= 4 { 2 } else { 1 };
        }
        let lower = word.to_ascii_lowercase();
        for index in 1..lower.len() {
            pairs += 1;
            if !common_pairs().contains(&lower[index - 1..=index]) {
                rare += 1;
            }
        }
    }
    let length = name.len() as f64;
    marks as f64 / length >= MIN_NOISE
        && pairs > 0
        && rare as f64 / pairs as f64 >= MIN_RARE_PAIRS
        && words.len() as f64 / length >= MIN_WORD_DENSITY
}

fn display_name(name: &str) -> String {
    let safe = redact_secret_like_material(name);
    match safe.strip_suffix(MARKDOWN) {
        Some(stem) => stem.to_string(),
        None => format!("\"{safe}\""),
    }
}

struct Groups<T> {
    index: HashMap<String, usize>,
    rows: Vec<(String, Vec<T>)>,
}

impl<T> Groups<T> {
    fn new() -> Self {
        Self {
            index: HashMap::new(),
            rows: Vec::new(),
        }
    }

    fn push(&mut self, key: &str, value: T) {
        match self.index.get(key) {
            Some(at) => self.rows[*at].1.push(value),
            None => {
                self.index.insert(key.to_string(), self.rows.len());
                self.rows.push((key.to_string(), vec![value]));
            }
        }
    }
}

fn directory_items(files: &[NamedFile]) -> Vec<ProjectionItem> {
    let merged = merge_by_name(files);
    let stems: Vec<&str> = merged
        .iter()
        .filter(|file| is_markdown(file))
        .map(|file| stem(&file.name))
        .collect();
    let ids = generated_ids(&stems);
    let mut runs: Groups<NamedFile> = Groups::new();
    let mut plain: Vec<NamedFile> = Vec::new();
    for file in &merged {
        match run_key(&file.name, &ids) {
            Some(key) => runs.push(&key, file.clone()),
            None => plain.push(file.clone()),
        }
    }
    let mut items = Vec::new();
    for (key, members) in runs.rows {
        match fold_run(&key, &members) {
            Some(folded) => items.push(folded),
            None => plain.extend(members),
        }
    }
    items.extend(date_groups(&plain));
    items.sort_by(newest_first);
    items
}

fn merge_by_name(files: &[NamedFile]) -> Vec<NamedFile> {
    let mut merged: Vec<NamedFile> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for file in files {
        let name = redact_secret_like_material(&file.name);
        match seen.get(&name) {
            Some(at) => {
                let row = &mut merged[*at];
                row.time = row.time.max(file.time);
                row.files += file.files;
            }
            None => {
                seen.insert(name.clone(), merged.len());
                merged.push(NamedFile {
                    name,
                    time: file.time,
                    files: file.files,
                });
            }
        }
    }
    merged
}

fn is_markdown(file: &NamedFile) -> bool {
    file.name.ends_with(MARKDOWN)
}

fn stem(name: &str) -> &str {
    &name[..name.len() - MARKDOWN.len()]
}

fn item(file: &NamedFile) -> ProjectionItem {
    ProjectionItem {
        text: display_name(&file.name),
        files: file.files,
        time: file.time,
    }
}

fn newest(files: &[NamedFile]) -> f64 {
    files
        .iter()
        .map(|file| file.time)
        .fold(f64::NEG_INFINITY, f64::max)
}

fn count(files: &[NamedFile]) -> usize {
    files.iter().map(|file| file.files).sum()
}

fn run_key(name: &str, ids: &HashSet<String>) -> Option<String> {
    if !name.ends_with(MARKDOWN) {
        return None;
    }
    let stem = stem(name);
    if ids.contains(stem) {
        return Some(ID.to_string());
    }
    let tokens: Vec<&str> = stem.split('-').collect();
    for (index, token) in tokens.iter().enumerate() {
        let date_length = if date_re().is_match(token) {
            1
        } else if year_re().is_match(token)
            && month_re().is_match(tokens.get(index + 1).copied().unwrap_or(""))
        {
            2
        } else {
            0
        };
        if date_length == 0 {
            continue;
        }
        let tail = &tokens[index + date_length..];
        if !tail
            .iter()
            .all(|part| hint_free_re().is_match(part) || date_re().is_match(part))
        {
            return None;
        }
        return Some(format!("{}-", tokens[..index].join("-")));
    }
    None
}

fn fold_run(key: &str, members: &[NamedFile]) -> Option<ProjectionItem> {
    if members.len() < MIN_RUN {
        return None;
    }
    let files = count(members);
    let mut names: Vec<String> = members
        .iter()
        .map(|member| display_name(&member.name))
        .collect();
    names.sort_by(|a, b| natural_compare(a, b));
    let text = if key == ID {
        format!("{ID} ({files} files)")
    } else {
        format!("{} … {} ({files} files)", names[0], names[names.len() - 1])
    };
    let listed = names.join(", ").len();
    (text.len() < listed).then(|| ProjectionItem {
        text,
        files,
        time: newest(members),
    })
}

fn date_groups(files: &[NamedFile]) -> Vec<ProjectionItem> {
    let mut groups: Groups<NamedFile> = Groups::new();
    let mut items = Vec::new();
    for file in files {
        let date = (is_markdown(file) && !brace_or_comma_re().is_match(&file.name))
            .then(|| date_suffix_re().captures(stem(&file.name)))
            .flatten()
            .map(|caps| caps[2].to_string());
        match date {
            Some(date) => groups.push(&date, file.clone()),
            None => items.push(item(file)),
        }
    }
    for (date, members) in groups.rows {
        if members.len() < 2 {
            items.extend(members.iter().map(item));
            continue;
        }
        let mut entries: Vec<ProjectionItem> = members.iter().map(item).collect();
        entries.sort_by(newest_first);
        let listed: Vec<&str> = entries
            .iter()
            .map(|entry| &entry.text[..entry.text.len() - date.len() - 1])
            .collect();
        items.push(ProjectionItem {
            text: format!("{{{}}}-{date}", listed.join(", ")),
            files: count(&members),
            time: newest(&members),
        });
    }
    items
}

fn group_directories(
    paths: &[String],
    times: Option<&BTreeMap<String, i64>>,
) -> Vec<DirectoryGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut by_directory: HashMap<String, Vec<NamedFile>> = HashMap::new();
    for path in paths {
        let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let directory = if parts.is_empty() {
            String::new()
        } else {
            format!("{}/", parts.join("/"))
        };
        let file = NamedFile {
            name: name.to_string(),
            time: times
                .and_then(|times| times.get(path))
                .map_or(f64::NEG_INFINITY, |time| *time as f64),
            files: 1,
        };
        match by_directory.get_mut(&directory) {
            Some(files) => files.push(file),
            None => {
                order.push(directory.clone());
                by_directory.insert(directory, vec![file]);
            }
        }
    }
    let leaves = leaf_directories(&order);
    let mut groups = id_families(&by_directory, &leaves);
    groups.extend(uniform_siblings(&by_directory, &leaves));
    let grouped: HashSet<&String> = groups.iter().flat_map(|group| &group.members).collect();
    let mut lines: Vec<DirectoryGroup> = order
        .iter()
        .filter(|directory| !grouped.contains(directory))
        .map(|directory| {
            let (label, pointer) = labelled(directory);
            DirectoryGroup {
                label,
                pointer,
                files: by_directory[directory].clone(),
                grouped: false,
            }
        })
        .collect();
    lines.extend(groups.into_iter().map(|group| group.line));
    let root_label = format!("{MEMORY_DIR}/");
    let (mut sorted, mut rest): (Vec<_>, Vec<_>) =
        lines.into_iter().partition(|line| line.label == root_label);
    rest.sort_by(|a, b| locale_compare(&a.label, &b.label));
    sorted.extend(rest);
    sorted
}

fn labelled(directory: &str) -> (String, String) {
    if directory.is_empty() {
        return (format!("{MEMORY_DIR}/"), format!("{MEMORY_DIR}/"));
    }
    let parts: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let readable = &parts[..first_redacted(&parts)];
    let label = format!(
        "{}/",
        parts
            .iter()
            .map(|part| redact_secret_like_material(part))
            .collect::<Vec<_>>()
            .join("/")
    );
    let pointer = if readable.is_empty() {
        format!("{MEMORY_DIR}/")
    } else {
        format!("{MEMORY_DIR}/{}/", readable.join("/"))
    };
    (label, pointer)
}

fn first_redacted(parts: &[&str]) -> usize {
    parts
        .iter()
        .position(|part| redact_secret_like_material(part) != *part)
        .unwrap_or(parts.len())
}

type Leaves = Vec<(String, Vec<String>)>;

fn leaf_directories(directories: &[String]) -> Leaves {
    let mut leaves: Leaves = Vec::new();
    for directory in directories {
        if directory.is_empty()
            || directories
                .iter()
                .any(|other| other != directory && other.starts_with(directory.as_str()))
        {
            continue;
        }
        let trimmed = &directory[..directory.len() - 1];
        let parent = &directory[..trimmed.rfind('/').map_or(0, |at| at + 1)];
        let name = segment(directory, parent);
        if redact_secret_like_material(name) != name {
            continue;
        }
        match leaves.iter_mut().find(|(key, _)| key == parent) {
            Some((_, siblings)) => siblings.push(directory.clone()),
            None => leaves.push((parent.to_string(), vec![directory.clone()])),
        }
    }
    leaves
}

fn segment<'a>(directory: &'a str, parent: &str) -> &'a str {
    &directory[parent.len()..directory.len() - 1]
}

fn files_of(by_directory: &HashMap<String, Vec<NamedFile>>, members: &[String]) -> Vec<NamedFile> {
    members
        .iter()
        .flat_map(|directory| by_directory.get(directory).cloned().unwrap_or_default())
        .collect()
}

fn parent_label(parent: &str) -> (String, String) {
    let (label, pointer) = labelled(parent);
    (
        if parent.is_empty() {
            String::new()
        } else {
            label
        },
        pointer,
    )
}

fn id_families(
    by_directory: &HashMap<String, Vec<NamedFile>>,
    leaves: &Leaves,
) -> Vec<SiblingGroup> {
    let mut groups = Vec::new();
    for (parent, siblings) in leaves {
        let names: Vec<&str> = siblings
            .iter()
            .map(|directory| segment(directory, parent))
            .collect();
        let ids = generated_ids(&names);
        let members: Vec<String> = siblings
            .iter()
            .filter(|directory| ids.contains(segment(directory, parent)))
            .cloned()
            .collect();
        if members.len() < MIN_SIBLINGS {
            continue;
        }
        let (label, pointer) = parent_label(parent);
        groups.push(SiblingGroup {
            line: DirectoryGroup {
                label: format!("{label}<id>/ ({} dirs)", members.len()),
                pointer,
                files: files_of(by_directory, &members),
                grouped: true,
            },
            members,
        });
    }
    groups
}

fn uniform_siblings(
    by_directory: &HashMap<String, Vec<NamedFile>>,
    leaves: &Leaves,
) -> Vec<SiblingGroup> {
    let mut groups = Vec::new();
    for (parent, siblings) in leaves {
        let names: Vec<&str> = siblings
            .iter()
            .map(|directory| segment(directory, parent))
            .collect();
        let ids = generated_ids(&names);
        let mut by_names: Vec<(String, Vec<String>)> = Vec::new();
        for directory in siblings {
            if ids.contains(segment(directory, parent)) {
                continue;
            }
            let mut file_names: Vec<&str> = by_directory
                .get(directory)
                .map(|files| files.iter().map(|file| file.name.as_str()).collect())
                .unwrap_or_default();
            file_names.sort_unstable();
            let key = file_names.join("\0");
            match by_names.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, same)) => same.push(directory.clone()),
                None => by_names.push((key, vec![directory.clone()])),
            }
        }
        for (_, members) in by_names {
            if members.len() < MIN_SIBLINGS
                || members.iter().any(|directory| {
                    let name = segment(directory, parent);
                    name.contains(['{', '}']) || name.contains(", ")
                })
            {
                continue;
            }
            let (label, pointer) = parent_label(parent);
            let mut names: Vec<&str> = members
                .iter()
                .map(|directory| segment(directory, parent))
                .collect();
            names.sort_by(|a, b| locale_compare(a, b));
            groups.push(SiblingGroup {
                line: DirectoryGroup {
                    label: format!("{label}{{{}}}/", names.join(", ")),
                    pointer,
                    files: files_of(by_directory, &members),
                    grouped: true,
                },
                members,
            });
        }
    }
    groups
}

fn files(items: &[ProjectionItem]) -> usize {
    items.iter().map(|item| item.files).sum()
}

fn marker(omitted: usize, pointer: &str) -> String {
    format!("(+{omitted} more; read {pointer} to list)")
}

fn layout(paths: &[String], input: Option<&ExternalProjectionInput>) -> Option<Layout> {
    let limits = input.map_or(
        ExternalProjectionLimits {
            max_entries_per_directory: 0,
            max_bytes: 0,
        },
        |input| input.limits,
    );
    let bounded = limits.max_entries_per_directory > 0 || limits.max_bytes > 0;
    let groups = group_directories(paths, input.filter(|_| bounded).map(|input| &input.times));
    let directories: Vec<DirectoryListing> = groups
        .iter()
        .map(|group| {
            let items = directory_items(&group.files);
            let item_bytes = items.iter().map(|item| item.text.len()).collect();
            DirectoryListing {
                label: group.label.clone(),
                pointer: group.pointer.clone(),
                items,
                item_bytes,
            }
        })
        .collect();
    let cap = limits.max_entries_per_directory;
    let mut allowances: Vec<usize> = directories
        .iter()
        .map(|directory| {
            if cap > 0 {
                cap.min(directory.items.len())
            } else {
                directory.items.len()
            }
        })
        .collect();
    let root_label = format!("{MEMORY_DIR}/");
    let root_bare = !groups.iter().any(|group| group.label == root_label);
    let legend = groups.iter().any(|group| group.grouped)
        || directories
            .iter()
            .any(|directory| directory.items.iter().any(|item| item.files > 1));
    if limits.max_bytes > 0 {
        fit_to_budget(
            &directories,
            &mut allowances,
            limits.max_bytes,
            root_bare,
            legend,
        );
    }
    // Directory labels alone can exceed the budget: omit whole rows (from the end of the
    // deterministic order) and report the count, rather than emitting an over-budget section.
    let mut kept = directories.len();
    if limits.max_bytes > 0 {
        let row_bytes: Vec<usize> = (0..directories.len())
            .map(|index| line_bytes(&directories, index, allowances[index]))
            .collect();
        let fixed_bytes = OPEN.len()
            + (if legend { NAME_LEGEND } else { NAME_NOTE }).len()
            + CLOSE.len()
            + if root_bare {
                format!("{MEMORY_DIR}/").len()
            } else {
                0
            }
            + 2
            + usize::from(root_bare);
        let root_pointer = format!("{MEMORY_DIR}/");
        let mut kept_bytes: usize = row_bytes.iter().map(|len| len + 1).sum();
        loop {
            let dropped = directories.len() - kept;
            let summary_bytes = if dropped > 0 {
                marker(dropped, &root_pointer).len() + 1
            } else {
                0
            };
            if fixed_bytes + kept_bytes + summary_bytes <= limits.max_bytes {
                break;
            }
            if kept == 0 {
                return None;
            }
            kept -= 1;
            kept_bytes -= row_bytes[kept] + 1;
        }
    }
    Some(Layout {
        root_bare,
        legend,
        directories,
        allowances,
        kept,
    })
}

fn line_bytes(directories: &[DirectoryListing], index: usize, allowance: usize) -> usize {
    let directory = &directories[index];
    let mut size = format!("{}: ", directory.label).len();
    for shown in 0..allowance {
        size +=
            directory.item_bytes.get(shown).copied().unwrap_or(0) + if shown > 0 { 2 } else { 0 };
    }
    let omitted = files(&directory.items[allowance.min(directory.items.len())..]);
    if omitted > 0 {
        size += usize::from(allowance > 0) + marker(omitted, &directory.pointer).len();
    }
    size
}

fn fit_to_budget(
    directories: &[DirectoryListing],
    allowances: &mut [usize],
    max_bytes: usize,
    root_bare: bool,
    legend: bool,
) {
    let start = allowances.to_vec();
    let mut lines: Vec<usize> = (0..directories.len())
        .map(|index| line_bytes(directories, index, allowances[index]))
        .collect();
    let fixed_lines = 3 + usize::from(root_bare);
    let fixed_bytes = OPEN.len()
        + (if legend { NAME_LEGEND } else { NAME_NOTE }).len()
        + CLOSE.len()
        + if root_bare {
            format!("{MEMORY_DIR}/").len()
        } else {
            0
        };
    let mut total = fixed_bytes
        + lines.iter().sum::<usize>()
        + (fixed_lines + directories.len().saturating_sub(1));
    while total > max_bytes {
        let mut widest: Option<usize> = None;
        for index in 0..allowances.len() {
            if allowances[index] > widest.map_or(0, |at| allowances[at]) {
                widest = Some(index);
            }
        }
        let Some(widest) = widest else {
            break;
        };
        allowances[widest] -= 1;
        let before = lines[widest];
        lines[widest] = line_bytes(directories, widest, allowances[widest]);
        total = total + lines[widest] - before;
    }
    if total <= max_bytes {
        return;
    }
    for (index, limit) in start.into_iter().enumerate() {
        let mut best = limit;
        for allowance in (0..limit).rev() {
            if line_bytes(directories, index, allowance) < line_bytes(directories, index, best) {
                best = allowance;
            }
        }
        allowances[index] = best;
    }
}

fn render(layout: &Layout) -> String {
    let mut lines = vec![
        OPEN.to_owned(),
        (if layout.legend {
            NAME_LEGEND
        } else {
            NAME_NOTE
        })
        .to_owned(),
    ];
    if layout.root_bare {
        lines.push(format!("{MEMORY_DIR}/"));
    }
    for (directory, allowance) in layout
        .directories
        .iter()
        .zip(&layout.allowances)
        .take(layout.kept)
    {
        let allowance = (*allowance).min(directory.items.len());
        let shown = directory.items[..allowance]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let omitted = files(&directory.items[allowance..]);
        let suffix = if omitted > 0 {
            format!(
                "{}{}",
                if shown.is_empty() { "" } else { " " },
                marker(omitted, &directory.pointer)
            )
        } else {
            String::new()
        };
        lines.push(format!("{}: {shown}{suffix}", directory.label));
    }
    let dropped = layout.directories.len() - layout.kept;
    if dropped > 0 {
        lines.push(marker(dropped, &format!("{MEMORY_DIR}/")));
    }
    lines.push(CLOSE.to_owned());
    lines.join("\n")
}

#[must_use]
pub fn render_external_projection(paths: &[String], input: &ExternalProjectionInput) -> String {
    layout(paths, Some(input)).map_or(String::new(), |layout| render(&layout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max_bytes: usize) -> ExternalProjectionInput {
        ExternalProjectionInput {
            limits: ExternalProjectionLimits {
                max_entries_per_directory: 0,
                max_bytes,
            },
            ..Default::default()
        }
    }

    /// One note in each of `count` distinct directories whose names are `width` bytes long. The
    /// notes differ, so no two directories fold into one row.
    fn wide_directories(count: usize, width: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                let head = format!("dir-{index:04}-");
                let tail = "q".repeat(width - head.len());
                format!("{head}{tail}/note-{index}.md")
            })
            .collect()
    }

    fn kept_rows(rendered: &str) -> usize {
        rendered
            .lines()
            .filter(|line| line.starts_with("dir-"))
            .count()
    }

    #[test]
    fn directory_rows_alone_over_the_budget_are_omitted_whole_and_counted() {
        let paths = wide_directories(1000, 80);
        let rendered = render_external_projection(&paths, &budget(40_960));

        assert!(rendered.len() <= 40_960, "{} bytes", rendered.len());
        let kept = kept_rows(&rendered);
        assert!(kept > 0 && kept < paths.len(), "{kept} rows kept");
        let summary = marker(paths.len() - kept, &format!("{MEMORY_DIR}/"));
        assert!(rendered.contains(&summary), "missing {summary}");
        assert!(rendered.starts_with(OPEN) && rendered.ends_with(CLOSE));
        // Whole rows: every kept row is the full label and its one note.
        for line in rendered.lines().filter(|line| line.starts_with("dir-")) {
            assert!(line.ends_with(".md") || line.contains(": note-"), "{line}");
        }
        assert_eq!(
            rendered,
            render_external_projection(&paths, &budget(40_960)),
            "the same input renders the same section"
        );
    }

    #[test]
    fn budget_below_the_frame_and_summary_omits_the_section() {
        let paths = wide_directories(50, 80);
        assert_eq!(render_external_projection(&paths, &budget(10)), "");
    }

    #[test]
    fn projection_that_fits_keeps_its_layout() {
        let paths = wide_directories(5, 20);
        let unbounded = render_external_projection(&paths, &budget(0));
        assert_eq!(
            render_external_projection(&paths, &budget(unbounded.len())),
            unbounded
        );
        assert_eq!(kept_rows(&unbounded), 5);
        let one_less = render_external_projection(&paths, &budget(unbounded.len() - 1));
        assert!(one_less.len() < unbounded.len(), "{one_less}");
        assert!(kept_rows(&one_less) < 5, "{one_less}");
    }
}
