//! Markdown memory blocks: a single-line-scalar YAML header plus a body.
//!
//! The header is kept as raw lines, so an edit rewrites only what it changes
//! and keys this crate does not model (`expires`, `provenance`, ...) survive.

use crate::repo::RepoError;

/// Longest accepted `description`, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// A parsed memory block.
/// The line endings of the original header, so a body-only edit keeps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Separators {
    nl: &'static str,
    crlf: bool,
}

impl Separators {
    fn of(header_text: &str) -> Self {
        Self {
            nl: if header_text.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            },
            crlf: header_text.starts_with("---\r\n"),
        }
    }

    fn default_lf() -> Self {
        Self {
            nl: "\n",
            crlf: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    header: Vec<String>,
    separators: Separators,
    /// Everything after the closing `---` line.
    pub body: String,
    /// The decoded `description` value.
    pub description: String,
    /// `true` when `read_only` is the literal `true` (quoted or not).
    pub read_only: bool,
}

impl Block {
    /// Parses a block.
    ///
    /// # Errors
    /// The frontmatter is missing or has no non-empty `description`.
    pub fn parse(content: &str) -> Result<Self, RepoError> {
        let (header, body, separators) = split_frontmatter(content)
            .ok_or_else(|| RepoError::new("target file is missing required frontmatter"))?;
        let mut description = None;
        let mut read_only = false;
        for line in &header {
            let Some((key, value)) = top_level_entry(line) else {
                continue;
            };
            match key.as_str() {
                "description" => description = Some(decode_scalar(value)),
                "read_only" => read_only = locks(value),
                _ => {}
            }
        }
        let description = description
            .filter(|d| !d.trim().is_empty())
            .ok_or_else(|| RepoError::new("target file frontmatter is missing 'description'"))?;
        Ok(Self {
            header,
            separators,
            body: body.to_owned(),
            description,
            read_only,
        })
    }

    /// A new block holding only `description` and `body`.
    ///
    /// # Errors
    /// The description is empty, too long, or does not survive a round trip.
    pub fn new(description: &str, body: &str) -> Result<Self, RepoError> {
        let description = sanitize_description(description);
        check_description(&description).map_err(RepoError::new)?;
        let rendered = format!(
            "---\ndescription: {}\n---\n{body}",
            render_scalar(&description)
        );
        let mut block = Self::checked(&rendered, &description)?;
        block.separators = Separators::default_lf();
        Ok(block)
    }

    /// This block with `description` replaced, every other header line kept.
    ///
    /// # Errors
    /// The description is empty, too long, or does not survive a round trip.
    pub fn with_description(&self, description: &str) -> Result<Self, RepoError> {
        let description = sanitize_description(description);
        check_description(&description).map_err(RepoError::new)?;
        let mut header = Vec::with_capacity(self.header.len());
        let mut skipping_continuation = false;
        for line in &self.header {
            if skipping_continuation && (line.trim().is_empty() || line.starts_with([' ', '\t'])) {
                continue;
            }
            skipping_continuation = false;
            match top_level_entry(line) {
                Some((key, value)) if key == "description" => {
                    header.push(format!("description: {}", render_scalar(&description)));
                    skipping_continuation = value.starts_with(['|', '>']);
                }
                _ => header.push(line.clone()),
            }
        }
        let next = Self {
            header,
            separators: self.separators,
            body: self.body.clone(),
            description: description.clone(),
            read_only: self.read_only,
        };
        Self::checked(&next.render(), &description)
    }

    /// This block with a different body.
    #[must_use]
    pub fn with_body(&self, body: String) -> Self {
        Self {
            body,
            ..self.clone()
        }
    }

    /// The decoded value of a top-level frontmatter key, when present.
    #[must_use]
    pub fn extra(&self, target_key: &str) -> Option<String> {
        for line in &self.header {
            let Some((key, value)) = top_level_entry(line) else {
                continue;
            };
            if key == target_key {
                return Some(decode_scalar(value));
            }
        }
        None
    }

    /// The file text.
    #[must_use]
    pub fn render(&self) -> String {
        let nl = self.separators.nl;
        format!("---{nl}{}{nl}---{nl}{}", self.header.join(nl), self.body)
    }

    fn checked(rendered: &str, description: &str) -> Result<Self, RepoError> {
        match Self::parse(rendered) {
            Ok(block) if block.description == description => Ok(block),
            _ => Err(RepoError::new("rendered header does not round-trip")),
        }
    }
}

/// Collapses newline runs to one space and trims, so a description is one line.
#[must_use]
pub fn sanitize_description(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_break = false;
    for ch in raw.chars() {
        if ch == '\r' || ch == '\n' {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(ch);
            in_break = false;
        }
    }
    out.trim().to_owned()
}

/// The reason a description is not acceptable, if any.
///
/// # Errors
/// Empty, multi-line, or longer than [`MAX_DESCRIPTION_CHARS`].
pub fn check_description(description: &str) -> Result<(), String> {
    if description.trim().is_empty() {
        return Err("'description' must not be empty".to_owned());
    }
    if description.contains(['\r', '\n']) {
        return Err("'description' must be a single line".to_owned());
    }
    let chars = description.chars().count();
    if chars > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "'description' exceeds {MAX_DESCRIPTION_CHARS} characters ({chars})"
        ));
    }
    Ok(())
}

fn split_frontmatter(content: &str) -> Option<(Vec<String>, &str, Separators)> {
    let mut lines = content.split_inclusive('\n');
    if lines.next()?.trim_end_matches(['\r', '\n']) != "---" {
        return None;
    }
    let mut offset = content.find('\n')? + 1;
    let mut header = Vec::new();
    for line in lines {
        offset += line.len();
        let text = line.trim_end_matches(['\r', '\n']);
        if text == "---" {
            let separators = Separators::of(&content[..offset]);
            return Some((header, &content[offset..], separators));
        }
        header.push(text.to_owned());
    }
    None
}

fn top_level_entry(line: &str) -> Option<(String, &str)> {
    if line.starts_with([' ', '\t', '#']) {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    let key = match key.strip_prefix('"').and_then(|k| k.strip_suffix('"')) {
        Some(unquoted) => unquoted.to_owned(),
        None => match key.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')) {
            Some(unquoted) => unquoted.to_owned(),
            None => key.to_owned(),
        },
    };
    Some((key, value.trim()))
}

fn decode_scalar(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('"') {
        if let Some(end) = closing_double_quote(inner) {
            if comment_only(&inner[end + 1..]) {
                return decode_double_quoted(&inner[..end])
                    .unwrap_or_else(|| inner[..end].to_owned());
            }
        }
    } else if let Some(inner) = value.strip_prefix('\'') {
        if let Some(end) = closing_single_quote(inner) {
            if comment_only(&inner[end + 1..]) {
                return inner[..end].replace("''", "'");
            }
        }
    }
    let unquoted = match unquoted_comment_at(value) {
        Some(at) => value[..at].trim_end(),
        None => value,
    };
    if unquoted.starts_with('#') || matches!(unquoted, "null" | "Null" | "NULL" | "~") {
        return String::new();
    }
    unquoted.to_owned()
}

/// Whether a `read_only` value locks the block. A double-quoted value whose
/// escapes cannot be decoded locks it too: an unreadable lock must not open.
fn locks(value: &str) -> bool {
    if let Some(inner) = value.strip_prefix('"') {
        if let Some(end) = closing_double_quote(inner) {
            if comment_only(&inner[end + 1..]) {
                return decode_double_quoted(&inner[..end]).is_none_or(|v| v == "true");
            }
        }
    }
    decode_scalar(value) == "true"
}

/// Decodes the body of a YAML double-quoted scalar (escapes per YAML 1.2).
fn decode_double_quoted(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let escaped = match chars.next()? {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            ' ' => ' ',
            '"' => '"',
            '/' => '/',
            '\\' => '\\',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            kind @ ('x' | 'u' | 'U') => {
                let width = match kind {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let hex: String = chars.by_ref().take(width).collect();
                if hex.len() != width {
                    return None;
                }
                char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
            }
            _ => return None,
        };
        out.push(escaped);
    }
    Some(out)
}

fn closing_double_quote(inner: &str) -> Option<usize> {
    let mut escaped = false;
    for (at, ch) in inner.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(at),
            _ => escaped = false,
        }
    }
    None
}

fn closing_single_quote(inner: &str) -> Option<usize> {
    let bytes = inner.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\'' {
            if bytes.get(at + 1) == Some(&b'\'') {
                at += 2;
                continue;
            }
            return Some(at);
        }
        at += 1;
    }
    None
}

fn comment_only(rest: &str) -> bool {
    let rest = rest.trim();
    rest.is_empty() || rest.starts_with('#')
}

fn unquoted_comment_at(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    (1..bytes.len()).find(|at| bytes[*at] == b'#' && matches!(bytes[at - 1], b' ' | b'\t'))
}

fn render_scalar(value: &str) -> String {
    const INDICATORS: &[char] = &[
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
        '`',
    ];
    let reserved = matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
    );
    let plain = !value.is_empty()
        && value == value.trim()
        && !value.starts_with(INDICATORS)
        && !value.ends_with(':')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.contains(char::is_control)
        && !reserved
        && !value.starts_with(|c: char| c.is_ascii_digit() || c == '+' || c == '.')
        && value.parse::<f64>().is_err();
    if plain {
        value.to_owned()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ndescription: Persona\nexpires: 2030-01-01T00:00:00Z\nread_only: \"true\"\n---\nline one\nline two\n";

    #[test]
    fn parses_header_and_body() {
        let block = Block::parse(SAMPLE).unwrap();
        assert_eq!(block.description, "Persona");
        assert!(block.read_only);
        assert_eq!(block.body, "line one\nline two\n");
        assert_eq!(block.render(), SAMPLE);
    }

    #[test]
    fn read_only_is_only_the_literal_true() {
        for (value, locked) in [
            ("true", true),
            ("\"true\"", true),
            ("'true'", true),
            ("false", false),
            ("\"false\"", false),
            ("yes", false),
        ] {
            let text = format!("---\ndescription: d\nread_only: {value}\n---\nb\n");
            assert_eq!(Block::parse(&text).unwrap().read_only, locked, "{value}");
        }
    }

    #[test]
    fn missing_or_blank_description_is_rejected() {
        for text in [
            "no frontmatter\n",
            "---\nread_only: true\n---\nb\n",
            "---\ndescription:   \n---\nb\n",
            "---\ndescription: d\nno closing fence\n",
        ] {
            assert!(Block::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn escaped_read_only_values_lock_and_undecodable_ones_fail_closed() {
        for value in [
            r#""\x74rue""#,
            r#""\u0074rue""#,
            r#""t\x72ue" # m"#,
            r#""\q""#,
        ] {
            let text = format!("---\ndescription: d\nread_only: {value}\n---\nb\n");
            assert!(Block::parse(&text).unwrap().read_only, "{value}");
        }
        let open = "---\ndescription: d\nread_only: \"f\\x61lse\"\n---\nb\n";
        assert!(!Block::parse(open).unwrap().read_only);
        let desc = "---\ndescription: \"tab\\there\"\n---\nb\n";
        assert_eq!(Block::parse(desc).unwrap().description, "tab\there");
    }

    #[test]
    fn a_block_scalar_description_is_replaced_across_blank_lines() {
        let text = "---\ndescription: |\n  first\n\n  second\nkind: note\n---\nb\n";
        let next = Block::parse(text).unwrap().with_description("new").unwrap();
        assert_eq!(next.render(), "---\ndescription: new\nkind: note\n---\nb\n");
    }

    #[test]
    fn null_descriptions_are_missing_and_numeric_ones_stay_text() {
        for value in ["null", "~", "NULL"] {
            let text = format!("---\ndescription: {value}\n---\nb\n");
            assert!(Block::parse(&text).is_err(), "{value}");
        }
        let quoted = Block::parse("---\ndescription: \"null\"\n---\nb\n").unwrap();
        assert_eq!(quoted.description, "null");
        for description in ["0x10", "1e3", ".5", "+1", "2026 notes"] {
            let rendered = Block::new(description, "b\n").unwrap().render();
            assert!(
                rendered.contains(&format!("description: \"{description}\"")),
                "{rendered}"
            );
        }
    }

    #[test]
    fn a_tab_before_a_comment_does_not_unlock_a_block() {
        let text = "---\ndescription: d\nread_only: true\t# managed\n---\nb\n";
        assert!(Block::parse(text).unwrap().read_only);
        let dashed = "---\ndescription: d\nread_only:\ttrue\n---\nb\n";
        assert!(Block::parse(dashed).unwrap().read_only);
    }

    #[test]
    fn a_body_only_edit_keeps_the_original_line_endings() {
        let text = "---\r\ndescription: d\r\nread_only: \"false\"\r\n---\r\nbody\r\n";
        let block = Block::parse(text).unwrap();
        assert_eq!(
            block.with_body("next\r\n".to_owned()).render(),
            "---\r\ndescription: d\r\nread_only: \"false\"\r\n---\r\nnext\r\n"
        );
    }

    #[test]
    fn quoted_keys_and_comment_only_values_are_understood() {
        let quoted =
            Block::parse("---\n\"read_only\": \"true\"\ndescription: d\n---\nb\n").unwrap();
        assert!(quoted.read_only);
        let commented = Block::parse("---\ndescription: # pending\n---\nb\n");
        assert!(commented.is_err());
        let literal = Block::parse("---\ndescription: \"# pending\"\n---\nb\n").unwrap();
        assert_eq!(literal.description, "# pending");
    }

    #[test]
    fn quoted_values_keep_hashes_and_ignore_trailing_comments() {
        let text = "---\ndescription: \"a # b\" # why\nread_only: \"true\" # managed\n---\nx\n";
        let block = Block::parse(text).unwrap();
        assert_eq!(block.description, "a # b");
        assert!(block.read_only);
        let single =
            Block::parse("---\ndescription: 'it''s' # note\nread_only: 'true' # m\n---\n").unwrap();
        assert_eq!(single.description, "it's");
        assert!(single.read_only);
        let plain = Block::parse("---\ndescription: plain # note\n---\n").unwrap();
        assert_eq!(plain.description, "plain");
    }

    #[test]
    fn replacing_a_description_keeps_following_comments_and_keys() {
        let text = "---\ndescription: old\n  # indented comment\nkind: note\n---\nb\n";
        let next = Block::parse(text).unwrap().with_description("new").unwrap();
        assert_eq!(
            next.render(),
            "---\ndescription: new\n  # indented comment\nkind: note\n---\nb\n"
        );
        let block_scalar = "---\ndescription: |\n  folded\n  text\nkind: note\n---\nb\n";
        let next = Block::parse(block_scalar)
            .unwrap()
            .with_description("new")
            .unwrap();
        assert_eq!(next.render(), "---\ndescription: new\nkind: note\n---\nb\n");
    }

    #[test]
    fn crlf_headers_parse() {
        let block = Block::parse("---\r\ndescription: d\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(block.description, "d");
        assert_eq!(block.body, "body\r\n");
    }

    #[test]
    fn update_description_keeps_every_other_header_line() {
        let block = Block::parse(SAMPLE).unwrap();
        let next = block
            .with_description("New: summary # not a comment")
            .unwrap();
        let text = next.render();
        assert!(text.contains("expires: 2030-01-01T00:00:00Z\n"), "{text}");
        assert!(text.contains("read_only: \"true\"\n"), "{text}");
        assert_eq!(
            Block::parse(&text).unwrap().description,
            "New: summary # not a comment"
        );
        assert_eq!(next.body, block.body);
    }

    #[test]
    fn descriptions_round_trip_through_the_scalar_renderer() {
        for description in [
            "plain words",
            "한국어 설명: 콜론 포함",
            "quote \" and ' inside",
            "- leading dash",
            "true",
            "42",
            "ends with colon:",
            "tab\tinside",
        ] {
            let block = Block::new(description, "b\n").unwrap();
            assert_eq!(
                Block::parse(&block.render()).unwrap().description,
                description
            );
        }
    }

    #[test]
    fn descriptions_are_validated() {
        assert!(Block::new("", "b").is_err());
        assert!(Block::new("   \n  ", "b").is_err());
        assert!(Block::new(&"x".repeat(MAX_DESCRIPTION_CHARS + 1), "b").is_err());
        assert_eq!(
            Block::new("two\nlines", "b").unwrap().description,
            "two lines"
        );
        assert!(check_description("a\nb").is_err());
    }
}
