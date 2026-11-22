//! `read_fetch`: the URL half of `read` — HTTP(S) fetches, content-type
//! dispatch, and omp's `URL:/Content-Type:/Method:` output header.
//!
//! Ported from pi-coding-agent `src/tools/fetch.ts` (`renderUrl`,
//! `tryLlmEndpoints`, `parseFeedToMarkdown`, `formatJson`, `buildUrlReadOutput`)
//! and `src/web/scrapers/types.ts` (`MAX_BYTES`, `looksLikeHtml`,
//! `finalizeOutput`). Scope is the text surface only: HTML -> markdown, RSS/Atom
//! -> markdown, JSON pretty-print, plain text, `llms.txt` probing, and a binary
//! notice. PDF/archive/sqlite/notebook/image payload routing and the remote
//! reader backends (trafilatura, lynx, Parallel, Firecrawl, Jina) are out of
//! scope for this module.
//!
//! Self-contained on purpose: only `reqwest`, `serde_json`, `regex` and `anyhow`
//! are used, all already dependencies of `omp-cli`.
#![allow(
    dead_code,
    clippy::pedantic,
    clippy::nursery,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions
)]

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;

// ─── Limits ────────────────────────────────────────────────────────────────

/// Network bound on a fetched body (omp `MAX_BYTES` in `web/scrapers/types.ts`).
pub const MAX_RESPONSE_BYTES: usize = 50 * 1024 * 1024;

/// Inline budget for the rendered content: omp's `DEFAULT_MAX_BYTES`.
pub const MAX_INLINE_BYTES: usize = 50 * 1024;

/// `FETCH_DEFAULT_MAX_LINES` upstream; kept for callers that page the output.
pub const FETCH_DEFAULT_MAX_LINES: usize = 300;

/// Items rendered from an RSS/Atom feed (omp `parseFeedToMarkdown` default).
pub const FEED_MAX_ITEMS: usize = 10;

/// Characters of item description/summary kept before `...`.
pub const FEED_DESCRIPTION_CHARS: usize = 500;

/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A rendered page shorter than this is treated as a render failure and
/// triggers the `llms.txt` fallback (omp `MIN_RENDERED_CHARS`).
pub const MIN_RENDERED_CHARS: usize = 100;

/// A probed `llms.txt`/`llms.md` body must clear this to be accepted.
pub const LLMS_MIN_CHARS: usize = 100;

/// Cap on a single `llms.txt` probe body.
pub const LLMS_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Deepest URL scope probed for `llms.txt` (bounds the number of probes).
pub const LLMS_MAX_SCOPE_DEPTH: usize = 3;

/// Bytes sampled when deciding whether a body is binary.
pub const BINARY_SAMPLE_BYTES: usize = 4096;

const USER_AGENT: &str = "Mozilla/5.0 (compatible; omp-read/1.0; +https://github.com/oh-my-pi)";

const ACCEPT_HTML: &str = "text/markdown, text/html;q=0.9, application/xhtml+xml;q=0.9, \
     application/json;q=0.9, application/rss+xml;q=0.8, application/atom+xml;q=0.8, \
     text/plain;q=0.8, */*;q=0.5";

const ACCEPT_TEXT: &str = "text/plain, text/markdown;q=0.9, */*;q=0.1";

// ─── URL detection ─────────────────────────────────────────────────────────

/// Whether `path` is a URL `read` should fetch instead of opening a file.
///
/// Accepts `http://`, `https://` (any case) and the bare `www.` shorthand.
pub fn is_fetch_url(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.")
}

/// omp `normalizeFetchUrl`: `www.` gains `https://`, everything else is kept.
pub fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        trimmed.to_string()
    } else if lower.starts_with("www.") {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Normalize and reject anything that is not an http(s) URL.
fn validate_target(input: &str) -> Result<String> {
    let url = normalize_url(input);
    if url.is_empty() {
        bail!("Empty URL");
    }
    if !is_fetch_url(&url) {
        bail!("Not an http(s) URL: {url}");
    }
    Ok(url)
}

/// Split `scheme://authority/path` into `(origin, path)`, dropping query and
/// fragment. `path` is `/` when the URL carries none.
fn split_origin(url: &str) -> Option<(String, String)> {
    let scheme_end = url.find("://")?;
    let scheme = url[..scheme_end].to_ascii_lowercase();
    let after = &url[scheme_end + 3..];
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..authority_end];
    if authority.is_empty() {
        return None;
    }
    let origin = format!("{scheme}://{authority}");
    let rest = &after[authority_end..];
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let path = &rest[..path_end];
    Some((
        origin,
        if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        },
    ))
}

/// omp `buildLlmEndpointCandidates`: `llms.txt`/`llms.md` locations for `url`,
/// deepest scope first, falling back to the site root for a bare origin.
pub fn llms_txt_candidates(url: &str) -> Vec<String> {
    let Some((origin, path)) = split_origin(url) else {
        return Vec::new();
    };
    if path == "/" {
        return vec![
            format!("{origin}/.well-known/llms.txt"),
            format!("{origin}/llms.txt"),
            format!("{origin}/llms.md"),
        ];
    }
    let trimmed = path.trim_end_matches('/');
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    let deepest = segments.len().min(LLMS_MAX_SCOPE_DEPTH);
    let mut out = Vec::with_capacity(deepest * 2);
    for depth in (1..=deepest).rev() {
        let scope = segments[..depth].join("/");
        out.push(format!("{origin}/{scope}/llms.txt"));
        out.push(format!("{origin}/{scope}/llms.md"));
    }
    out
}

// ─── Content-type classification ───────────────────────────────────────────

/// omp `normalizeMime`: lowercase, parameters dropped.
pub fn normalize_mime(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// omp `looksLikeHtml`: only the leading tag decides.
pub fn looks_like_html(content: &str) -> bool {
    let head: String = content
        .trim_start()
        .chars()
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype")
        || head.starts_with("<html")
        || head.starts_with("<head")
        || head.starts_with("<body")
}

/// Whether a body opens like an RSS/Atom/RDF feed, whatever its MIME type.
pub fn looks_like_feed(content: &str) -> bool {
    let head: String = content
        .trim_start()
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:rdf")
}

pub fn is_html_mime(mime: &str) -> bool {
    mime.contains("html") || mime.contains("xhtml")
}

pub fn is_json_mime(mime: &str) -> bool {
    mime.contains("json")
}

pub fn is_feed_mime(mime: &str) -> bool {
    mime.contains("rss") || mime.contains("atom") || mime.contains("feed")
}

/// Plain-text-ish MIME types: rendered verbatim, never markdown-converted.
pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("markdown")
        || mime.contains("yaml")
        || mime.contains("toml")
        || mime.contains("javascript")
        || mime.contains("x-sh")
}

/// MIME types whose payload is not text; the body is reported, not rendered.
pub fn is_binary_mime(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime.starts_with("font/")
        || mime == "application/pdf"
        || mime == "application/zip"
        || mime == "application/octet-stream"
        || mime.contains("gzip")
        || mime.contains("compressed")
        || mime.contains("msword")
        || mime.contains("officedocument")
        || mime.contains("epub")
}

/// Sample-based binary detection: a NUL byte in the leading window.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SAMPLE_BYTES)].contains(&0)
}

// ─── Small formatting helpers ──────────────────────────────────────────────

/// omp `formatBytes`.
pub fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes}B")
    } else if value < MB {
        format!("{:.1}KB", value / KB)
    } else {
        format!("{:.1}MB", value / MB)
    }
}

/// Truncate on a UTF-8 boundary; returns the head and whether it was cut.
pub fn truncate_bytes(content: &str, max_bytes: usize) -> (String, bool) {
    if content.len() <= max_bytes {
        return (content.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (content[..end].to_string(), true)
}

/// omp `finalizeOutput`: trim. URL reads page their own preview, so content is
/// never shortened here beyond surrounding whitespace.
pub fn finalize_output(content: &str) -> String {
    content.trim().to_string()
}

// ─── Entity decoding ───────────────────────────────────────────────────────

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "bull" => '\u{2022}',
        "middot" => '\u{00B7}',
        "copy" => '\u{00A9}',
        "reg" => '\u{00AE}',
        "trade" => '\u{2122}',
        "deg" => '\u{00B0}',
        "plusmn" => '\u{00B1}',
        "times" => '\u{00D7}',
        "divide" => '\u{00F7}',
        "euro" => '\u{20AC}',
        "pound" => '\u{00A3}',
        "yen" => '\u{00A5}',
        "cent" => '\u{00A2}',
        "sect" => '\u{00A7}',
        "para" => '\u{00B6}',
        "laquo" => '\u{00AB}',
        "raquo" => '\u{00BB}',
        "rarr" => '\u{2192}',
        "larr" => '\u{2190}',
        "harr" => '\u{2194}',
        "check" => '\u{2713}',
        "zwnj" => '\u{200C}',
        "zwj" => '\u{200D}',
        "shy" => '\u{00AD}',
        _ => return None,
    })
}

/// Decode HTML entities (`&amp;`, `&#65;`, `&#x41;`) in one pass. Unknown
/// entities and bare `&` are left untouched.
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input.as_bytes()[i] == b'&' {
            if let Some(rel) = input[i..].find(';') {
                if rel <= 12 {
                    let entity = &input[i + 1..i + rel];
                    if let Some(ch) = named_entity(entity) {
                        out.push(ch);
                        i += rel + 1;
                        continue;
                    }
                    if let Some(rest) = entity.strip_prefix('#') {
                        let code = if rest.starts_with('x') || rest.starts_with('X') {
                            u32::from_str_radix(&rest[1..], 16).ok()
                        } else {
                            rest.parse::<u32>().ok()
                        };
                        if let Some(ch) = code.and_then(char::from_u32) {
                            out.push(ch);
                            i += rel + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Drop every `<...>` span, leaving text. A full parser is not worth it here:
/// feed fields are short and `<` in attribute values is vanishingly rare.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// omp `cleanFeedText`: unwrap CDATA, strip markup, decode entities, collapse
/// whitespace runs to single spaces.
pub fn clean_feed_text(input: &str) -> String {
    let trimmed = input.trim();
    let body = if let Some(inner) = trimmed.strip_prefix("<![CDATA[") {
        match inner.find("]]>") {
            Some(end) => &inner[..end],
            None => inner,
        }
    } else {
        trimmed
    };
    let stripped = strip_tags(body);
    let decoded = decode_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─── HTML -> markdown ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered,
}

#[derive(Default)]
struct MdState {
    /// Open lists, innermost last, with the ordered-list item counter.
    lists: Vec<(ListKind, usize)>,
    in_pre: bool,
    /// `href` of each open `<a>`, innermost last (`None` for an anchor without one).
    links: Vec<Option<String>>,
}

/// Elements whose contents are never prose, plus comments.
const IGNORED_BLOCKS: [&str; 6] = ["script", "style", "noscript", "svg", "template", "head"];

/// Remove comments and the elements whose contents are never prose.
fn strip_ignored_blocks(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        if lower.as_bytes()[i] == b'<' {
            if lower[i..].starts_with("<!--") {
                match lower[i + 4..].find("-->") {
                    Some(end) => {
                        i += 4 + end + 3;
                        continue;
                    }
                    None => break,
                }
            }
            let mut skipped = false;
            for tag in IGNORED_BLOCKS {
                let open = format!("<{tag}");
                if !lower[i..].starts_with(&open) {
                    continue;
                }
                let after_open = i + open.len();
                let boundary = lower
                    .as_bytes()
                    .get(after_open)
                    .is_some_and(|b| *b == b'>' || b.is_ascii_whitespace());
                if !boundary {
                    continue;
                }
                let close = format!("</{tag}");
                match lower[after_open..].find(&close) {
                    Some(rel) => {
                        let close_start = after_open + rel;
                        match lower[close_start..].find('>') {
                            Some(gt) => {
                                i = close_start + gt + 1;
                                skipped = true;
                            }
                            None => {
                                i = html.len();
                                skipped = true;
                            }
                        }
                    }
                    None => {
                        i = html.len();
                        skipped = true;
                    }
                }
                break;
            }
            if skipped {
                continue;
            }
        }
        let ch = html[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Read one attribute value out of a tag's attribute text.
fn read_attr_value(input: &str) -> String {
    match input.as_bytes().first() {
        Some(&quote @ (b'"' | b'\'')) => {
            let body = &input[1..];
            match body.find(quote as char) {
                Some(end) => body[..end].to_string(),
                None => body.to_string(),
            }
        }
        _ => {
            let end = input
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(input.len());
            input[..end].to_string()
        }
    }
}

/// `attr(attrs, "href")` over the post-tag-name text of a start tag.
fn attr(attrs: &str, name: &str) -> Option<String> {
    if attrs.is_empty() {
        return None;
    }
    let lower = attrs.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(name) {
        let at = search + rel;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = at + name.len();
        let after_ok = lower
            .as_bytes()
            .get(after)
            .is_some_and(|b| *b == b'=' || b.is_ascii_whitespace());
        if before_ok && after_ok {
            let rest = attrs[after..].trim_start();
            let Some(rest) = rest.strip_prefix('=') else {
                return Some(String::new());
            };
            return Some(decode_entities(&read_attr_value(rest.trim_start())));
        }
        search = at + name.len();
    }
    None
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

fn ensure_newline(out: &mut String) {
    trim_trailing_spaces(out);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    trim_trailing_spaces(out);
    if out.is_empty() {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn push_text(out: &mut String, text: &str, in_pre: bool) {
    if in_pre {
        out.push_str(text);
        return;
    }
    let mut last_space = out.ends_with(' ') || out.ends_with('\n');
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
}

fn handle_tag(out: &mut String, raw_tag: &str, state: &mut MdState) {
    let raw_tag = raw_tag.trim();
    if raw_tag.is_empty() {
        return;
    }
    let closing = raw_tag.starts_with('/');
    let body = if closing {
        raw_tag[1..].trim_start()
    } else {
        raw_tag
    };
    let body = body.trim_end_matches('/');
    if body.is_empty() || body.starts_with('!') || body.starts_with('?') {
        return;
    }
    let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let attrs = &body[name_end..];

    let heading = match name.as_str() {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    };

    if let Some(level) = heading {
        ensure_blank_line(out);
        if !closing {
            for _ in 0..level {
                out.push('#');
            }
            out.push(' ');
        }
        return;
    }

    match name.as_str() {
        "br" => out.push('\n'),
        "hr" => {
            ensure_blank_line(out);
            out.push_str("---");
            ensure_blank_line(out);
        }
        "p" | "div" | "section" | "article" | "main" | "aside" | "header" | "footer" | "nav"
        | "figure" | "figcaption" | "dl" | "table" => ensure_blank_line(out),
        "blockquote" => {
            ensure_blank_line(out);
            if !closing {
                out.push_str("> ");
            }
        }
        "tr" => ensure_newline(out),
        "td" | "th" => out.push_str(" | "),
        "ul" | "ol" => {
            if closing {
                state.lists.pop();
                ensure_blank_line(out);
            } else {
                // A nested list continues the item line; a top-level one starts
                // a new block.
                if state.lists.is_empty() {
                    ensure_blank_line(out);
                } else {
                    ensure_newline(out);
                }
                let kind = if name == "ol" {
                    ListKind::Ordered
                } else {
                    ListKind::Unordered
                };
                state.lists.push((kind, 0));
            }
        }
        "li" => {
            if !closing {
                ensure_newline(out);
                let depth = state.lists.len().saturating_sub(1);
                for _ in 0..depth {
                    out.push_str("  ");
                }
                let marker = match state.lists.last_mut() {
                    Some((ListKind::Ordered, count)) => {
                        *count += 1;
                        format!("{count}. ")
                    }
                    _ => "- ".to_string(),
                };
                out.push_str(&marker);
            }
        }
        "pre" => {
            state.in_pre = !closing;
            if closing {
                ensure_newline(out);
                out.push_str("```");
                ensure_blank_line(out);
            } else {
                ensure_blank_line(out);
                out.push_str("```\n");
            }
        }
        "code" => {
            if !state.in_pre {
                out.push('`');
            }
        }
        "strong" | "b" => out.push_str("**"),
        "em" | "i" => out.push('*'),
        "a" => {
            if closing {
                match state.links.pop() {
                    Some(Some(href)) if !href.is_empty() => {
                        out.push_str("](");
                        out.push_str(&href);
                        out.push(')');
                    }
                    _ => out.push(']'),
                }
            } else {
                let href = attr(attrs, "href").filter(|h| !h.is_empty());
                state.links.push(href);
                out.push('[');
            }
        }
        "img" => {
            if !closing {
                let alt = attr(attrs, "alt").unwrap_or_default();
                let src = attr(attrs, "src").unwrap_or_default();
                if !src.is_empty() {
                    out.push_str("![");
                    out.push_str(&alt);
                    out.push_str("](");
                    out.push_str(&src);
                    out.push(')');
                }
            }
        }
        _ => {}
    }
}

/// Collapse blank-line runs and trim, keeping at most one blank line between
/// blocks.
fn finalize_markdown(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_newlines = 0;
    for line in raw.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            pending_newlines += 1;
            continue;
        }
        if !out.is_empty() {
            // One blank line between blocks is preserved; longer runs and
            // back-to-back lines both collapse to at most one blank line.
            for _ in 0..(pending_newlines + 1).min(2) {
                out.push('\n');
            }
        }
        out.push_str(line);
        pending_newlines = 0;
    }
    out.trim().to_string()
}

/// Convert an HTML document to markdown-ish text: headings, paragraphs, lists,
/// links, images, inline emphasis, fenced code, and table cells. Script/style/
/// head content is dropped first. Unknown tags are ignored; whitespace runs
/// collapse outside `<pre>`.
pub fn html_to_markdown(html: &str) -> String {
    let cleaned = strip_ignored_blocks(html);
    let mut out = String::with_capacity(cleaned.len() / 2);
    let mut state = MdState::default();
    let mut i = 0;
    while i < cleaned.len() {
        if cleaned.as_bytes()[i] != b'<' {
            let start = i;
            while i < cleaned.len() && cleaned.as_bytes()[i] != b'<' {
                i += 1;
            }
            let text = decode_entities(&cleaned[start..i]);
            push_text(&mut out, &text, state.in_pre);
            continue;
        }
        if cleaned[i..].starts_with("<!--") {
            match cleaned[i + 4..].find("-->") {
                Some(end) => {
                    i += 4 + end + 3;
                    continue;
                }
                None => break,
            }
        }
        let Some(rel_end) = cleaned[i..].find('>') else {
            break;
        };
        let raw_tag = &cleaned[i + 1..i + rel_end];
        i += rel_end + 1;
        handle_tag(&mut out, raw_tag, &mut state);
    }
    finalize_markdown(&out)
}

/// The document `<title>`, decoded and whitespace-collapsed.
pub fn html_title(html: &str) -> Option<String> {
    static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").expect("valid title regex")
    });
    let caps = TITLE_RE.captures(html)?;
    let title = clean_feed_text(&caps[1]);
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

// ─── Feeds ─────────────────────────────────────────────────────────────────

static CHANNEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<channel\b[^>]*>(.*?)</channel\s*>").expect("valid channel regex")
});
static FEED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<feed\b[^>]*>(.*?)</feed\s*>").expect("valid feed regex"));
static ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<item\b[^>]*>(.*?)</item\s*>").expect("valid item regex"));
static ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<entry\b[^>]*>(.*?)</entry\s*>").expect("valid entry regex")
});
static ATOM_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<link\b[^>]*\bhref\s*=\s*["']([^"']*)["']"#).expect("valid atom link regex")
});

fn element_text(xml: &str, name: &str) -> Option<String> {
    let re = Regex::new(&format!(r"(?is)<{name}\b[^>]*>(.*?)</{name}\s*>")).ok()?;
    let caps = re.captures(xml)?;
    Some(clean_feed_text(&caps[1]))
}

fn element_text_or_empty(xml: &str, name: &str) -> String {
    element_text(xml, name).unwrap_or_default()
}

fn atom_link(xml: &str) -> String {
    ATOM_LINK_RE
        .captures(xml)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default()
}

fn push_feed_item(md: &mut String, title: &str, date: &str, summary: &str, link: &str) {
    md.push_str("## ");
    md.push_str(if title.is_empty() { "Untitled" } else { title });
    md.push('\n');
    if !date.is_empty() {
        md.push('*');
        md.push_str(date);
        md.push_str("*\n\n");
    }
    if !summary.is_empty() {
        let mut chars = summary.chars();
        let head: String = chars.by_ref().take(FEED_DESCRIPTION_CHARS).collect();
        md.push_str(&head);
        if chars.next().is_some() {
            md.push_str("...");
        }
        md.push_str("\n\n");
    }
    if !link.is_empty() {
        md.push_str("[Read more](");
        md.push_str(link);
        md.push_str(")\n\n");
    }
    md.push_str("---\n\n");
}

/// omp `parseFeedToMarkdown`: RSS (`channel`/`item`) or Atom (`feed`/`entry`)
/// to markdown. `None` when the body is neither.
pub fn format_feed(content: &str, max_items: usize) -> Option<String> {
    if let Some(channel) = CHANNEL_RE.captures(content) {
        let channel = &channel[1];
        let title = element_text(channel, "title").filter(|t| !t.is_empty());
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(title.as_deref().unwrap_or("RSS Feed"));
        md.push_str("\n\n");
        for item in ITEM_RE.captures_iter(channel).take(max_items) {
            let item = &item[1];
            let item_title = element_text_or_empty(item, "title");
            let link = element_text_or_empty(item, "link");
            let pub_date = element_text_or_empty(item, "pubDate");
            let mut description = element_text_or_empty(item, "description");
            if description.is_empty() {
                description = element_text_or_empty(item, "content:encoded");
            }
            push_feed_item(&mut md, &item_title, &pub_date, &description, &link);
        }
        return Some(md);
    }

    let feed = FEED_RE.captures(content)?;
    let feed = &feed[1];
    let title = element_text(feed, "title").filter(|t| !t.is_empty());
    let mut md = String::new();
    md.push_str("# ");
    md.push_str(title.as_deref().unwrap_or("Atom Feed"));
    md.push_str("\n\n");
    for entry in ENTRY_RE.captures_iter(feed).take(max_items) {
        let entry = &entry[1];
        let entry_title = element_text_or_empty(entry, "title");
        let link = atom_link(entry);
        let updated = {
            let updated = element_text_or_empty(entry, "updated");
            if updated.is_empty() {
                element_text_or_empty(entry, "published")
            } else {
                updated
            }
        };
        let mut summary = element_text_or_empty(entry, "summary");
        if summary.is_empty() {
            summary = element_text_or_empty(entry, "content");
        }
        push_feed_item(&mut md, &entry_title, &updated, &summary, &link);
    }
    Some(md)
}

// ─── JSON ──────────────────────────────────────────────────────────────────

/// omp `formatJson`: pretty-print, or `None` when the body is not JSON.
pub fn pretty_json(content: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

// ─── Response types ────────────────────────────────────────────────────────

/// Which rendering path produced the content (omp `FetchProvider` subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    /// HTML converted to markdown.
    Html,
    /// Markdown served directly (content negotiation).
    Markdown,
    /// RSS/Atom converted to markdown.
    Feed,
    /// JSON pretty-printed.
    Json,
    /// Plain text, verbatim.
    Text,
    /// Raw body, no shaping (`:raw` or a fallback).
    Raw,
    /// An `llms.txt`/`llms.md` probe answered for a page that rendered empty.
    LlmsTxt,
    /// Not text; only a notice is returned.
    Binary,
}

impl FetchMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Feed => "feed",
            Self::Json => "json",
            Self::Text => "text",
            Self::Raw => "raw",
            Self::LlmsTxt => "llms.txt",
            Self::Binary => "binary",
        }
    }
}

/// Knobs for [`fetch_url`]; [`Default`] mirrors omp's constants.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Network bound on the body.
    pub max_bytes: usize,
    /// Inline bound on the rendered content.
    pub max_inline_bytes: usize,
    pub timeout: Duration,
    /// Skip every text-shaping branch and return the body as-is.
    pub raw: bool,
    /// Items rendered from a feed.
    pub max_feed_items: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            max_bytes: MAX_RESPONSE_BYTES,
            max_inline_bytes: MAX_INLINE_BYTES,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            raw: false,
            max_feed_items: FEED_MAX_ITEMS,
        }
    }
}

/// A rendered URL read.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// The URL as requested.
    pub url: String,
    /// The URL after redirects.
    pub final_url: String,
    /// The MIME type the content is rendered as.
    pub content_type: String,
    pub method: FetchMethod,
    pub content: String,
    /// The body or the inline content was cut.
    pub truncated: bool,
    pub notes: Vec<String>,
}

struct RawResponse {
    final_url: String,
    content_type: String,
    status: u16,
    body: Vec<u8>,
    content_length: Option<u64>,
    truncated: bool,
}

// ─── HTTP ──────────────────────────────────────────────────────────────────

fn build_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("build HTTP client")
}

async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    accept: &str,
) -> Result<RawResponse> {
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let content_length = response.content_length();

    let mut body: Vec<u8> = Vec::new();
    let mut truncated = content_length.is_some_and(|len| len > max_bytes as u64);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("read body of {url}"))?
    {
        if body.len() + chunk.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(RawResponse {
        final_url,
        content_type,
        status,
        body,
        content_length,
        truncated,
    })
}

fn binary_notice(url: &str, mime: &str, size: u64) -> String {
    let kind = if mime.is_empty() {
        "unknown type"
    } else {
        mime
    };
    format!(
        "[{url} is a binary response ({kind}, {}); download it and read the file instead]",
        format_bytes(usize::try_from(size).unwrap_or(usize::MAX))
    )
}

/// Probe the `llms.txt`/`llms.md` candidates for `url`. Accepts the first
/// non-HTML body over [`LLMS_MIN_CHARS`], returning `(endpoint, body)`.
pub async fn probe_llms_txt(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> Option<(String, String)> {
    for endpoint in llms_txt_candidates(url) {
        let Ok(response) = fetch_bytes(
            client,
            &endpoint,
            max_bytes.min(LLMS_MAX_BYTES),
            ACCEPT_TEXT,
        )
        .await
        else {
            continue;
        };
        if !(200..300).contains(&response.status) {
            continue;
        }
        let text = String::from_utf8_lossy(&response.body).to_string();
        if text.trim().chars().count() > LLMS_MIN_CHARS && !looks_like_html(&text) {
            return Some((endpoint, text));
        }
    }
    None
}

/// Fetch `input` and render it for `read`.
///
/// Dispatch order: `raw` short-circuit, JSON, feed, HTML (with an `llms.txt`
/// fallback when the page renders to almost nothing), text, binary notice.
///
/// # Errors
/// Empty or non-http(s) input, transport failures, and non-2xx responses.
pub async fn fetch_url(input: &str, options: &FetchOptions) -> Result<FetchResult> {
    let url = validate_target(input)?;
    let client = build_client(options.timeout)?;
    let response = fetch_bytes(&client, &url, options.max_bytes, ACCEPT_HTML).await?;

    if response.status == 204 {
        return Ok(FetchResult {
            url,
            final_url: response.final_url,
            content_type: normalize_mime(&response.content_type),
            method: FetchMethod::Text,
            content: String::new(),
            truncated: false,
            notes: vec!["204 No Content".to_string()],
        });
    }
    if !(200..300).contains(&response.status) {
        let preview = String::from_utf8_lossy(&response.body);
        let (preview, _) = truncate_bytes(preview.trim(), 200);
        bail!(
            "HTTP {} for {}{}",
            response.status,
            response.final_url,
            if preview.is_empty() {
                String::new()
            } else {
                format!(": {preview}")
            }
        );
    }

    let mut notes: Vec<String> = Vec::new();
    if response.truncated {
        notes.push(format!(
            "Response body exceeded {} and was cut mid-stream; content is incomplete",
            format_bytes(options.max_bytes)
        ));
    }

    let mime = normalize_mime(&response.content_type);
    let body_text = String::from_utf8_lossy(&response.body).to_string();
    let total_bytes = response
        .content_length
        .unwrap_or(response.body.len() as u64);

    let (method, content, content_type) = if options.raw {
        (FetchMethod::Raw, body_text, mime)
    } else if is_json_mime(&mime) {
        match pretty_json(&body_text) {
            Some(pretty) => (FetchMethod::Json, pretty, mime),
            None => (FetchMethod::Json, body_text, mime),
        }
    } else if is_feed_mime(&mime) || looks_like_feed(&body_text) {
        match format_feed(&body_text, options.max_feed_items) {
            Some(markdown) => (FetchMethod::Feed, markdown, mime),
            None => (FetchMethod::Text, body_text, mime),
        }
    } else if is_html_mime(&mime) || looks_like_html(&body_text) {
        let markdown = html_to_markdown(&body_text);
        if markdown.trim().chars().count() >= MIN_RENDERED_CHARS {
            (FetchMethod::Html, markdown, "text/markdown".to_string())
        } else if let Some((endpoint, llm)) =
            probe_llms_txt(&client, &response.final_url, options.max_bytes).await
        {
            notes.push(format!("Used llms.txt fallback: {endpoint}"));
            (FetchMethod::LlmsTxt, llm, "text/plain".to_string())
        } else {
            notes.push("HTML rendered to little text; returning raw HTML".to_string());
            (FetchMethod::Raw, body_text, mime)
        }
    } else if is_binary_mime(&mime) && looks_binary(&response.body) {
        (
            FetchMethod::Binary,
            binary_notice(&response.final_url, &mime, total_bytes),
            mime,
        )
    } else if is_binary_mime(&mime) || is_text_mime(&mime) {
        // Declared binary but the sample is text (a mislabeled server type), or
        // a plain-text type: render verbatim.
        (FetchMethod::Text, body_text, mime)
    } else if looks_binary(&response.body) {
        (
            FetchMethod::Binary,
            binary_notice(&response.final_url, &mime, total_bytes),
            mime,
        )
    } else {
        (FetchMethod::Text, body_text, mime)
    };

    let (content, inline_truncated) = truncate_bytes(&content, options.max_inline_bytes);
    if inline_truncated {
        notes.push(format!(
            "Content truncated to {}; read the URL with :raw for the full body",
            format_bytes(options.max_inline_bytes)
        ));
    }

    Ok(FetchResult {
        url,
        final_url: response.final_url,
        content_type,
        method,
        content,
        truncated: response.truncated || inline_truncated,
        notes,
    })
}

/// omp `buildUrlReadOutput`: the `URL:/Content-Type:/Method:` header block.
pub fn render_url_read_output(result: &FetchResult, raw: bool) -> String {
    if raw {
        return result.content.clone();
    }
    let mut out = String::new();
    out.push_str("URL: ");
    out.push_str(&result.final_url);
    out.push('\n');
    out.push_str("Content-Type: ");
    out.push_str(&result.content_type);
    out.push('\n');
    out.push_str("Method: ");
    out.push_str(result.method.as_str());
    out.push('\n');
    if !result.notes.is_empty() {
        out.push_str("Notes: ");
        out.push_str(&result.notes.join("; "));
        out.push('\n');
    }
    out.push_str("\n---\n\n");
    out.push_str(&result.content);
    out
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_fetch_urls() {
        assert!(is_fetch_url("http://example.com"));
        assert!(is_fetch_url("https://example.com/a?b=1"));
        assert!(is_fetch_url("HTTPS://EXAMPLE.COM"));
        assert!(is_fetch_url("www.example.com/docs"));
        assert!(is_fetch_url("  https://example.com  "));
        assert!(!is_fetch_url("example.com"));
        assert!(!is_fetch_url("/tmp/file.txt"));
        assert!(!is_fetch_url("./README.md"));
        assert!(!is_fetch_url(""));
    }

    #[test]
    fn normalizes_www_shorthand() {
        assert_eq!(normalize_url("www.example.com"), "https://www.example.com");
        assert_eq!(
            normalize_url("  www.example.com/a  "),
            "https://www.example.com/a"
        );
        assert_eq!(
            normalize_url("https://example.com/a"),
            "https://example.com/a"
        );
        assert_eq!(normalize_url("http://example.com"), "http://example.com");
        assert_eq!(normalize_url("   "), "");
    }

    #[test]
    fn validates_targets_before_any_network_io() {
        assert_eq!(
            validate_target("www.example.com").expect("valid"),
            "https://www.example.com"
        );
        assert!(validate_target("").is_err());
        assert!(validate_target("   ").is_err());
        assert!(validate_target("/tmp/file.txt").is_err());
        assert!(validate_target("ftp://example.com/x").is_err());
        assert!(validate_target("example.com").is_err());
    }

    #[test]
    fn splits_origin_and_path() {
        assert_eq!(
            split_origin("https://example.com/docs/api?x=1"),
            Some(("https://example.com".to_string(), "/docs/api".to_string()))
        );
        assert_eq!(
            split_origin("http://localhost:8080"),
            Some(("http://localhost:8080".to_string(), "/".to_string()))
        );
        assert_eq!(split_origin("not a url"), None);
        assert_eq!(split_origin("https://"), None);
    }

    #[test]
    fn builds_llms_candidates_for_root() {
        let root = vec![
            "https://example.com/.well-known/llms.txt".to_string(),
            "https://example.com/llms.txt".to_string(),
            "https://example.com/llms.md".to_string(),
        ];
        assert_eq!(llms_txt_candidates("https://example.com/"), root);
        assert_eq!(llms_txt_candidates("https://example.com"), root);
    }

    #[test]
    fn builds_llms_candidates_deepest_first() {
        assert_eq!(
            llms_txt_candidates("https://example.com/docs/api"),
            vec![
                "https://example.com/docs/api/llms.txt".to_string(),
                "https://example.com/docs/api/llms.md".to_string(),
                "https://example.com/docs/llms.txt".to_string(),
                "https://example.com/docs/llms.md".to_string(),
            ]
        );
    }

    #[test]
    fn caps_llms_candidate_depth() {
        let candidates = llms_txt_candidates("https://example.com/a/b/c/d/e");
        assert_eq!(candidates.len(), LLMS_MAX_SCOPE_DEPTH * 2);
        assert_eq!(candidates[0], "https://example.com/a/b/c/llms.txt");
    }

    #[test]
    fn normalizes_mime_types() {
        assert_eq!(normalize_mime("text/html; charset=utf-8"), "text/html");
        assert_eq!(normalize_mime("  APPLICATION/JSON "), "application/json");
        assert_eq!(normalize_mime(""), "");
        assert_eq!(normalize_mime(";"), "");
    }

    #[test]
    fn classifies_mime_types() {
        assert!(is_html_mime("text/html"));
        assert!(is_html_mime("application/xhtml+xml"));
        assert!(!is_html_mime("text/plain"));

        assert!(is_json_mime("application/json"));
        assert!(is_json_mime("application/ld+json"));
        assert!(!is_json_mime("text/html"));

        assert!(is_feed_mime("application/rss+xml"));
        assert!(is_feed_mime("application/atom+xml"));
        assert!(is_feed_mime("application/feed+json"));
        assert!(!is_feed_mime("text/html"));

        assert!(is_text_mime("text/plain"));
        assert!(is_text_mime("text/markdown"));
        assert!(!is_text_mime("image/png"));

        assert!(is_binary_mime("image/png"));
        assert!(is_binary_mime("application/pdf"));
        assert!(is_binary_mime("application/zip"));
        assert!(is_binary_mime("application/octet-stream"));
        assert!(!is_binary_mime("text/html"));
    }

    #[test]
    fn detects_html_by_leading_tag() {
        assert!(looks_like_html("<!DOCTYPE html><html></html>"));
        assert!(looks_like_html("  \n<html lang=\"en\">"));
        assert!(looks_like_html("<head><title>x</title></head>"));
        assert!(looks_like_html("<body>hi</body>"));
        assert!(!looks_like_html("# markdown"));
        assert!(!looks_like_html("{\"a\":1}"));
    }

    #[test]
    fn detects_feeds_by_leading_tag() {
        assert!(looks_like_feed(
            "<?xml version=\"1.0\"?><rss version=\"2.0\">"
        ));
        assert!(looks_like_feed(
            "<feed xmlns=\"http://www.w3.org/2005/Atom\">"
        ));
        assert!(looks_like_feed("<rdf:RDF></rdf:RDF>"));
        assert!(!looks_like_feed("<?xml version=\"1.0\"?><root/>"));
    }

    #[test]
    fn detects_binary_by_nul_byte() {
        assert!(looks_binary(b"\x7fELF\x00\x01\x02"));
        assert!(!looks_binary(b"plain text body"));
        assert!(!looks_binary(b""));
        let mut sample = vec![b'a'; BINARY_SAMPLE_BYTES + 16];
        sample.push(0);
        assert!(
            !looks_binary(&sample),
            "a NUL past the sample window is ignored"
        );
    }

    #[test]
    fn formats_byte_counts() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0MB");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_bytes("hello", 10), ("hello".to_string(), false));
        assert_eq!(truncate_bytes("hello", 3), ("hel".to_string(), true));
        // 'e' with an acute accent occupies bytes 1..3, so a 2-byte cut backs
        // off to byte 1 instead of splitting the character.
        assert_eq!(truncate_bytes("h\u{e9}llo", 2), ("h".to_string(), true));
        assert_eq!(
            truncate_bytes("h\u{e9}llo", 3),
            ("h\u{e9}".to_string(), true)
        );
        assert_eq!(truncate_bytes("", 0), (String::new(), false));
    }

    #[test]
    fn decodes_named_and_numeric_entities() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&mdash;"), "\u{2014}");
        assert_eq!(decode_entities("no entities"), "no entities");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
        assert_eq!(decode_entities("100% & more"), "100% & more");
    }

    #[test]
    fn renders_headings_and_paragraphs() {
        let md = html_to_markdown("<h1>Hello</h1><p>World <b>bold</b>.</p>");
        assert_eq!(md, "# Hello\n\nWorld **bold**.");
    }

    #[test]
    fn renders_links_and_images() {
        let md = html_to_markdown(r#"<p>See <a href="https://example.com">the site</a> now.</p>"#);
        assert_eq!(md, "See [the site](https://example.com) now.");

        let md = html_to_markdown(r#"<p><img src="/a.png" alt="A picture"></p>"#);
        assert_eq!(md, "![A picture](/a.png)");
    }

    #[test]
    fn renders_unordered_and_ordered_lists() {
        assert_eq!(
            html_to_markdown("<ul><li>One</li><li>Two</li></ul>"),
            "- One\n- Two"
        );
        assert_eq!(
            html_to_markdown("<ol><li>A</li><li>B</li></ol>"),
            "1. A\n2. B"
        );
    }

    #[test]
    fn renders_nested_lists_with_indent() {
        let md = html_to_markdown("<ul><li>Outer</li><ul><li>Inner</li></ul></ul>");
        assert_eq!(md, "- Outer\n  - Inner");
    }

    #[test]
    fn drops_script_style_and_head() {
        let html = "<html><head><title>T</title></head><body><p>Hi</p>\
                    <script>alert(1)</script><style>p{color:red}</style><p>Bye</p></body></html>";
        assert_eq!(html_to_markdown(html), "Hi\n\nBye");
    }

    #[test]
    fn keeps_paragraph_text_around_inline_markup() {
        let md = html_to_markdown("<p>a <em>x</em> b <code>y</code> c</p>");
        assert_eq!(md, "a *x* b `y` c");
    }

    #[test]
    fn renders_fenced_code_blocks() {
        let md = html_to_markdown("<pre><code>fn main() {}</code></pre>");
        assert_eq!(md, "```\nfn main() {}\n```");
    }

    #[test]
    fn renders_horizontal_rules_and_breaks() {
        assert_eq!(html_to_markdown("<p>a</p><hr><p>b</p>"), "a\n\n---\n\nb");
        assert_eq!(html_to_markdown("<p>a<br>b</p>"), "a\nb");
    }

    #[test]
    fn collapses_whitespace_outside_pre() {
        let md = html_to_markdown("<p>a\n\n   b\t\tc</p>");
        assert_eq!(md, "a b c");
    }

    #[test]
    fn ignores_comments_and_unknown_tags() {
        let md = html_to_markdown("<p>a<!-- hidden --><span>b</span></p>");
        assert_eq!(md, "ab");
    }

    #[test]
    fn extracts_document_title() {
        assert_eq!(
            html_title("<html><head><title>  My &amp; Page </title></head></html>"),
            Some("My & Page".to_string())
        );
        assert_eq!(html_title("<html></html>"), None);
    }

    #[test]
    fn formats_rss_feed() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>My Feed</title>
<item><title>First</title><link>https://e.com/1</link><pubDate>Mon, 01 Jan 2024</pubDate><description>Hello &amp; welcome</description></item>
<item><title>Second</title><link>https://e.com/2</link><description>More</description></item>
</channel></rss>"#;
        let md = format_feed(rss, FEED_MAX_ITEMS).expect("rss parses");
        assert_eq!(
            md,
            "# My Feed\n\n\
             ## First\n\
             *Mon, 01 Jan 2024*\n\n\
             Hello & welcome\n\n\
             [Read more](https://e.com/1)\n\n\
             ---\n\n\
             ## Second\n\
             More\n\n\
             [Read more](https://e.com/2)\n\n\
             ---\n\n"
        );
    }

    #[test]
    fn formats_atom_feed() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>Atom Feed</title>
<entry><title>E1</title><link href="https://e.com/a"/><updated>2024-01-01T00:00:00Z</updated><summary>Summary one</summary></entry>
</feed>"#;
        let md = format_feed(atom, FEED_MAX_ITEMS).expect("atom parses");
        assert_eq!(
            md,
            "# Atom Feed\n\n\
             ## E1\n\
             *2024-01-01T00:00:00Z*\n\n\
             Summary one\n\n\
             [Read more](https://e.com/a)\n\n\
             ---\n\n"
        );
    }

    #[test]
    fn caps_feed_items() {
        let mut rss = String::from("<rss><channel><title>F</title>");
        for i in 0..5 {
            rss.push_str(&format!("<item><title>I{i}</title></item>"));
        }
        rss.push_str("</channel></rss>");
        let md = format_feed(&rss, 2).expect("rss parses");
        assert_eq!(md.matches("## ").count(), 2);
        assert!(md.contains("## I0"));
        assert!(md.contains("## I1"));
        assert!(!md.contains("## I2"));
    }

    #[test]
    fn truncates_long_feed_descriptions() {
        let long = "x".repeat(FEED_DESCRIPTION_CHARS + 10);
        let rss = format!(
            "<rss><channel><title>F</title><item><title>I</title><description>{long}</description></item></channel></rss>"
        );
        let md = format_feed(&rss, FEED_MAX_ITEMS).expect("rss parses");
        assert!(md.contains(&"x".repeat(FEED_DESCRIPTION_CHARS)));
        assert!(md.contains("..."));
    }

    #[test]
    fn cleans_feed_text_from_cdata_and_markup() {
        assert_eq!(clean_feed_text("<![CDATA[<p>hi   there</p>]]>"), "hi there");
        assert_eq!(clean_feed_text("  plain   text "), "plain text");
        assert_eq!(clean_feed_text("<b>bold</b> &amp; <i>it</i>"), "bold & it");
    }

    #[test]
    fn rejects_non_feed_documents() {
        assert!(format_feed("<html><body>nope</body></html>", FEED_MAX_ITEMS).is_none());
        assert!(format_feed("{\"a\":1}", FEED_MAX_ITEMS).is_none());
    }

    #[test]
    fn pretty_prints_json() {
        assert_eq!(
            pretty_json("{\"a\":1}").as_deref(),
            Some("{\n  \"a\": 1\n}")
        );
        assert_eq!(pretty_json("[1,2]").as_deref(), Some("[\n  1,\n  2\n]"));
        assert!(pretty_json("not json").is_none());
        assert!(pretty_json("").is_none());
    }

    #[test]
    fn renders_url_read_header() {
        let result = FetchResult {
            url: "https://example.com".to_string(),
            final_url: "https://example.com/final".to_string(),
            content_type: "text/markdown".to_string(),
            method: FetchMethod::Html,
            content: "# Hi".to_string(),
            truncated: false,
            notes: Vec::new(),
        };
        assert_eq!(
            render_url_read_output(&result, false),
            "URL: https://example.com/final\nContent-Type: text/markdown\nMethod: html\n\n---\n\n# Hi"
        );
        assert_eq!(render_url_read_output(&result, true), "# Hi");
    }

    #[test]
    fn renders_notes_in_header() {
        let result = FetchResult {
            url: "https://example.com".to_string(),
            final_url: "https://example.com".to_string(),
            content_type: "text/plain".to_string(),
            method: FetchMethod::LlmsTxt,
            content: "body".to_string(),
            truncated: true,
            notes: vec![
                "Used llms.txt fallback: https://example.com/llms.txt".to_string(),
                "second".to_string(),
            ],
        };
        let out = render_url_read_output(&result, false);
        assert!(
            out.contains("Notes: Used llms.txt fallback: https://example.com/llms.txt; second\n")
        );
        assert!(out.contains("Method: llms.txt"));
    }

    #[test]
    fn fetch_method_names_match_omp() {
        assert_eq!(FetchMethod::Html.as_str(), "html");
        assert_eq!(FetchMethod::Markdown.as_str(), "markdown");
        assert_eq!(FetchMethod::Feed.as_str(), "feed");
        assert_eq!(FetchMethod::Json.as_str(), "json");
        assert_eq!(FetchMethod::Text.as_str(), "text");
        assert_eq!(FetchMethod::Raw.as_str(), "raw");
        assert_eq!(FetchMethod::LlmsTxt.as_str(), "llms.txt");
        assert_eq!(FetchMethod::Binary.as_str(), "binary");
    }

    #[test]
    fn default_options_match_omp_limits() {
        let options = FetchOptions::default();
        assert_eq!(options.max_bytes, 50 * 1024 * 1024);
        assert_eq!(options.max_inline_bytes, 50 * 1024);
        assert_eq!(options.max_feed_items, FEED_MAX_ITEMS);
        assert!(!options.raw);
        assert_eq!(options.timeout, Duration::from_secs(30));
    }

    #[test]
    fn binary_notice_names_type_and_size() {
        let notice = binary_notice("https://e.com/a.pdf", "application/pdf", 2048);
        assert!(notice.contains("application/pdf"));
        assert!(notice.contains("2.0KB"));
        assert!(notice.contains("https://e.com/a.pdf"));
    }

    #[test]
    fn finalize_output_trims_only() {
        assert_eq!(finalize_output("  body  "), "body");
        assert_eq!(finalize_output("a\n\nb"), "a\n\nb");
    }
}
