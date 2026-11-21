//! Visible terminal width: UAX #11 table widths plus emoji-presentation rules.
//!
//! Width is measured per extended grapheme cluster. Combining marks, variation
//! selectors and skin-tone modifiers contribute no extra cells, and everything
//! after a ZWJ joins the preceding cluster instead of widening it. VS16 and
//! keycap sequences promote a promotable text base to two cells. Controls,
//! including `\t`, have width zero; callers expand tabs before measuring when
//! tab stops matter.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// ZWJ, which joins the following scalar into the current cluster.
const ZWJ: char = '\u{200D}';
/// Emoji presentation selector.
const VS16: char = '\u{FE0F}';
/// Keycap combining mark.
const KEYCAP: char = '\u{20E3}';

/// Width of a single scalar in terminal cells. Controls measure zero.
#[must_use]
pub fn width_char(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Width of one extended grapheme cluster in terminal cells.
fn cluster_width(cluster: &str) -> usize {
    // `base + VS16? + U+20E3` renders as a single two-cell keycap.
    if cluster.contains(KEYCAP) {
        return 2;
    }

    let mut total = 0usize;
    let mut started = false;
    let mut joined = false;
    let mut emoji_presentation = false;

    for ch in cluster.chars() {
        if ch == ZWJ {
            joined = true;
            continue;
        }
        if ch == VS16 {
            emoji_presentation = true;
            continue;
        }
        let width = width_char(ch);
        if width == 0 {
            // Combining mark, variation selector, skin tone, or control.
            started = true;
            continue;
        }
        if !started {
            total = width;
            started = true;
        } else if joined {
            // Content after a ZWJ is part of the same emoji: never additive.
            total = total.max(width);
            joined = false;
        } else {
            total += width;
        }
    }

    if emoji_presentation {
        total = total.max(2);
    }
    total
}

/// Visible width of `s` in terminal cells.
#[must_use]
pub fn width_str(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    // Printable ASCII is one cell per byte; nothing else can be as cheap.
    if s.is_ascii() {
        return s.bytes().filter(|byte| !byte.is_ascii_control()).count();
    }
    s.graphemes(true).map(cluster_width).sum()
}

#[cfg(test)]
mod tests {
    use super::{width_char, width_str};

    #[test]
    fn measures_ascii_and_wide_cells() {
        assert_eq!(width_str("abc"), 3);
        assert_eq!(width_str(""), 0);
        assert_eq!(width_str("\u{754c}"), 2);
        assert_eq!(width_str("a\u{754c}b"), 4);
    }

    #[test]
    fn controls_measure_zero() {
        for ch in ['\0', '\t', '\n', '\r', '\u{1b}', '\u{7f}', '\u{9b}'] {
            assert_eq!(width_char(ch), 0, "{ch:?}");
        }
        assert_eq!(width_str("a\tb"), 2);
    }

    #[test]
    fn combining_marks_and_joiners_add_nothing() {
        assert_eq!(width_str("e\u{301}"), 1);
        assert_eq!(width_str("e\u{301}\u{327}"), 1);
        // Family emoji: four pictographs joined by ZWJ stay one two-cell cluster.
        assert_eq!(width_str("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"), 2);
    }

    #[test]
    fn vs16_and_keycaps_promote_to_two_cells() {
        assert_eq!(width_str("\u{2764}"), 1);
        assert_eq!(width_str("\u{2764}\u{fe0f}"), 2);
        assert_eq!(width_str("1\u{fe0f}\u{20e3}"), 2);
        assert_eq!(width_str("#\u{20e3}"), 2);
    }
}
