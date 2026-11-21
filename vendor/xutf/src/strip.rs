//! ANSI/VT escape stripping and visible-width measurement.
//!
//! The scanner follows the ECMA-48 control strings (`CSI`, `OSC`, `DCS`, `SOS`,
//! `PM`, `APC`) plus the short `ESC <intermediate> <final>` forms, in both their
//! `ESC`-prefixed and C1 codepoint spellings. Everything it removes is invisible
//! to a terminal, so what remains is what the cell measurement must count.

use std::borrow::Cow;

use crate::width_str;

/// ESC introduces the 7-bit escape forms.
const ESC: char = '\u{1b}';
/// BEL terminates an OSC string.
const BEL: char = '\u{07}';

/// `true` for the C1 introducers: DCS, SOS, CSI, OSC, PM, APC.
const fn is_c1_introducer(ch: char) -> bool {
    matches!(
        ch,
        '\u{90}' | '\u{98}' | '\u{9b}' | '\u{9d}' | '\u{9e}' | '\u{9f}'
    )
}

/// CSI parameters and intermediates end at the first byte in this range.
fn is_csi_final(ch: char) -> bool {
    matches!(ch, '\u{40}'..='\u{7e}')
}

/// Charset designators take exactly one byte after the intermediate.
const fn is_charset_intermediate(ch: char) -> bool {
    matches!(ch, '(' | ')' | '*' | '+' | '#' | '%')
}

/// Index just past the string terminator starting at `start` (BEL or `ESC \`).
fn skip_string_terminated(chars: &[char], start: usize) -> usize {
    let mut at = start;
    while at < chars.len() {
        match chars[at] {
            BEL => return at + 1,
            ESC if chars.get(at + 1) == Some(&'\\') => return at + 2,
            _ => at += 1,
        }
    }
    chars.len()
}

/// Index just past the CSI sequence whose parameter body starts at `start`.
fn skip_csi(chars: &[char], start: usize) -> usize {
    let mut at = start;
    while at < chars.len() {
        if is_csi_final(chars[at]) {
            return at + 1;
        }
        at += 1;
    }
    chars.len()
}

/// Index just past the escape sequence introduced by `chars[esc]` (an `ESC`).
fn skip_escape(chars: &[char], esc: usize) -> usize {
    let Some(&introducer) = chars.get(esc + 1) else {
        return chars.len();
    };
    match introducer {
        '[' => skip_csi(chars, esc + 2),
        ']' | 'P' | 'X' | '^' | '_' => skip_string_terminated(chars, esc + 2),
        intermediate if is_charset_intermediate(intermediate) => (esc + 3).min(chars.len()),
        // Two-char escape (e.g. `ESC c`, `ESC =`).
        _ => esc + 2,
    }
}

/// Removes terminal escape sequences, borrowing when `s` holds none.
#[must_use]
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|ch| ch == ESC || is_c1_introducer(ch)) {
        return Cow::Borrowed(s);
    }

    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut at = 0usize;
    while at < chars.len() {
        let ch = chars[at];
        match ch {
            ESC => at = skip_escape(&chars, at),
            '\u{9b}' => at = skip_csi(&chars, at + 1),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                at = skip_string_terminated(&chars, at + 1);
            }
            _ => {
                out.push(ch);
                at += 1;
            }
        }
    }
    Cow::Owned(out)
}

/// Visible width of `s` after removing terminal escape sequences.
#[must_use]
pub fn width_ansi_str(s: &str) -> usize {
    match strip_ansi(s) {
        Cow::Borrowed(plain) => width_str(plain),
        Cow::Owned(plain) => width_str(&plain),
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_ansi, width_ansi_str};

    #[test]
    fn plain_text_is_borrowed_untouched() {
        assert!(matches!(
            strip_ansi("plain \u{754c}"),
            std::borrow::Cow::Borrowed("plain \u{754c}")
        ));
        assert_eq!(width_ansi_str("plain"), 5);
    }

    #[test]
    fn strips_csi_sgr_and_cursor_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}[?25lhide\u{1b}[?25h"), "hide");
        assert_eq!(strip_ansi("\u{1b}[1;38;5;196mx\u{1b}[m"), "x");
        // C1 spelling of CSI.
        assert_eq!(strip_ansi("\u{9b}1mwide\u{9b}0m"), "wide");
    }

    #[test]
    fn strips_osc_strings_in_both_terminators() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}body"), "body");
        assert_eq!(
            strip_ansi("\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\"),
            "link"
        );
    }

    #[test]
    fn strips_short_escape_and_charset_forms() {
        assert_eq!(strip_ansi("a\u{1b}cb"), "ab");
        assert_eq!(strip_ansi("\u{1b}(Btext"), "text");
    }

    #[test]
    fn unterminated_sequences_consume_the_rest() {
        assert_eq!(strip_ansi("tail\u{1b}[38;5"), "tail");
        assert_eq!(strip_ansi("tail\u{1b}]0;never"), "tail");
    }

    #[test]
    fn measures_the_cells_the_ls_grid_test_depends_on() {
        // Inputs of omp-shell-builtins' `measures_ansi_and_unicode_cells_with_xutf`.
        assert_eq!(width_ansi_str("\u{1b}[31m\u{754c}\u{1b}[0m"), 2);
        assert_eq!(width_ansi_str("a"), 1);
        assert_eq!(width_ansi_str("xx"), 2);
        assert_eq!(width_ansi_str("b"), 1);
    }

    #[test]
    fn measures_visible_cells_around_escapes() {
        assert_eq!(width_ansi_str("\u{1b}[31m\u{754c}\u{1b}[0m"), 2);
        assert_eq!(width_ansi_str("\u{1b}[1mabc\u{1b}[0m"), 3);
        assert_eq!(width_ansi_str("\u{1b}[31m\u{2764}\u{fe0f}\u{1b}[0m"), 2);
    }
}
