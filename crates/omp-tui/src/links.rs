//! Transcript links an extension claimed (`pi.registerLinkScheme`).
//!
//! TS reference: `modes/interactive-mode.ts` `startLinkHints` /
//! `openLinkBrowser` — the interactive mode collects the links of the
//! transcript whose scheme an extension registered and lets the user pick one,
//! which raises `link_activate` on the owning extension. Here the collection
//! half lives in this module (pure text work, no UI) and the picker in
//! [`crate::overlays::links`].

/// The scheme half of `url` (`cc-stretch:open/1` → `cc-stretch`), or `None`
/// when it has none. Case is preserved; callers compare lowercased.
#[must_use]
pub fn scheme_of(url: &str) -> Option<&str> {
    match url.split_once(':') {
        Some((scheme, _)) if !scheme.is_empty() => Some(scheme),
        _ => None,
    }
}

/// Whether `url`'s scheme is one of `schemes` (both lowercased, as the
/// extension host normalizes them).
#[must_use]
pub fn is_extension_link(url: &str, schemes: &[String]) -> bool {
    scheme_of(url).is_some_and(|scheme| {
        let scheme = scheme.to_ascii_lowercase();
        schemes.contains(&scheme)
    })
}

/// Characters that end a URL in running text: whitespace plus the delimiters
/// a markdown link, a quoted string or a sentence wraps it in.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`' | ',' | ';')
}

/// Every link in `text` whose scheme is one of `schemes`, in reading order and
/// without duplicates. Covers bare URLs and `[label](url)` markdown links
/// (the URL inside the parentheses is found the same way).
#[must_use]
pub fn link_urls(text: &str, schemes: &[String]) -> Vec<String> {
    if schemes.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for (colon, _) in text.match_indices(':') {
        // The scheme token immediately before the colon: letters, digits,
        // `+`, `-`, `.` (a URL scheme cannot start with a digit).
        let start = text[..colon]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let scheme = &text[start..colon];
        if scheme.is_empty() || !is_extension_link(&format!("{scheme}:"), schemes) {
            continue;
        }
        let end = text[colon + 1..]
            .char_indices()
            .find(|(_, c)| ends_url(*c))
            .map_or(text.len(), |(i, _)| colon + 1 + i);
        let url = &text[start..end];
        if !out.iter().any(|u| u == url) {
            out.push(url.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schemes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn finds_bare_and_markdown_links_in_reading_order() {
        let text = "open cc-stretch:open/1792/0/2 and [later](cc-stretch:open/7) or \
                    see https://example.com for the rest";
        assert_eq!(
            link_urls(text, &schemes(&["cc-stretch"])),
            ["cc-stretch:open/1792/0/2", "cc-stretch:open/7"]
        );
    }

    #[test]
    fn ignores_unclaimed_schemes_and_bare_words() {
        let text = "https://example.com and note: this, and cc-stretch:ok";
        assert_eq!(
            link_urls(text, &schemes(&["cc-stretch"])),
            ["cc-stretch:ok"]
        );
        // No registered scheme: nothing is offered.
        assert!(link_urls(text, &[]).is_empty());
    }

    #[test]
    fn trims_trailing_punctuation_and_dedupes() {
        let text = "(cc-stretch:open/1), then cc-stretch:open/1 again.";
        assert_eq!(
            link_urls(text, &schemes(&["cc-stretch"])),
            ["cc-stretch:open/1"]
        );
    }

    #[test]
    fn scheme_matching_ignores_case() {
        assert_eq!(
            link_urls("CC-Stretch:Open/1", &schemes(&["cc-stretch"])),
            ["CC-Stretch:Open/1"]
        );
        assert_eq!(scheme_of("cc-stretch:x"), Some("cc-stretch"));
        assert_eq!(scheme_of(":x"), None);
        assert_eq!(scheme_of("no-colon"), None);
    }
}
