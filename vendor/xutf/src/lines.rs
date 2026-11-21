//! Line splitting over UTF-8, UTF-16 and UTF-32.
//!
//! Which code points end a line is a [`LineEnds`] choice. A line never
//! includes its terminator in [`Line::text`], and a trailing terminator does
//! not start an extra empty line, so [`Line::with_end`] over [`LineEnds::Lf`]
//! is exactly `split_inclusive('\n')` and [`Line::text`] over the same mode is
//! exactly [`str::lines`].
//!
//! Splitting is unit-level and permissive: a unit that equals a line end still
//! ends a line even when it sits inside a malformed sequence.

use core::{
    fmt,
    iter::FusedIterator,
    marker::PhantomData,
    ops::{Index, RangeFrom, RangeTo},
};

use crate::{
    Utf8,
    encoding::{Encoding, Unit},
};

/// Line feed.
const LF: u32 = 0x0a;
/// Carriage return.
const CR: u32 = 0x0d;
/// Vertical tab.
const VT: u32 = 0x0b;
/// Form feed.
const FF: u32 = 0x0c;
/// File separator.
const FS: u32 = 0x1c;
/// Group separator.
const GS: u32 = 0x1d;
/// Record separator.
const RS: u32 = 0x1e;
/// Next line.
const NEL: u32 = 0x85;
/// Line separator.
const LS: u32 = 0x2028;
/// Paragraph separator.
const PS: u32 = 0x2029;

/// The code points that end a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineEnds {
    /// LF, with a CR directly before it joining the terminator; a lone CR is
    /// text. Matches [`str::lines`], `BufRead::lines`, and Git.
    Lf,
    /// LF, CRLF, and lone CR. Matches Python's `bytes.splitlines`, universal
    /// newlines, and terminal hard breaks.
    Universal,
    /// [`Universal`](LineEnds::Universal) plus VT, FF, FS, GS, RS, NEL
    /// (U+0085), LS (U+2028), and PS (U+2029). Matches Python's
    /// `str.splitlines`.
    Unicode,
}

/// One line from [`lines`] or [`lines_str`]: its text, its terminator, and
/// where it starts.
///
/// `T` is `str` for [`lines_str`] and the code-unit slice type for [`lines`].
#[derive(Debug, PartialEq, Eq)]
pub struct Line<'a, T: ?Sized> {
    /// Text followed by its terminator.
    full: &'a T,
    /// Units of `full` before the terminator.
    text_len: usize,
    /// Offset of `full` in the input.
    at: usize,
}

impl<T: ?Sized> Clone for Line<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Line<'_, T> {}

impl<'a, T> Line<'a, T>
where
    T: ?Sized + Index<RangeTo<usize>, Output = T> + Index<RangeFrom<usize>, Output = T>,
{
    /// The line without its terminator.
    #[inline]
    #[must_use]
    pub fn text(&self) -> &'a T {
        &self.full[..self.text_len]
    }

    /// The terminator (`"\n"`, `"\r\n"`, ...): empty only for a final line the
    /// input ends without terminating, which a stream reader should hold until
    /// more input arrives.
    #[inline]
    #[must_use]
    pub fn end(&self) -> &'a T {
        &self.full[self.text_len..]
    }

    /// The line with its terminator, as Python's `splitlines(keepends=True)`
    /// returns it.
    #[inline]
    #[must_use]
    pub const fn with_end(&self) -> &'a T {
        self.full
    }

    /// Offset of the line's first unit in the input: bytes for [`lines_str`],
    /// code units for [`lines`].
    #[inline]
    #[must_use]
    pub const fn at(&self) -> usize {
        self.at
    }
}

/// Iterator over the lines of encoded text, created by [`lines`].
pub struct Lines<'a, E: Encoding> {
    input: &'a [E::Unit],
    ends: LineEnds,
    /// Offset where the next line starts.
    start: usize,
    _encoding: PhantomData<E>,
}

impl<E: Encoding> Clone for Lines<'_, E> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            input: self.input,
            ends: self.ends,
            start: self.start,
            _encoding: PhantomData,
        }
    }
}

impl<E: Encoding> fmt::Debug for Lines<'_, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lines")
            .field("ends", &self.ends)
            .field("start", &self.start)
            .field("len", &self.input.len())
            .finish_non_exhaustive()
    }
}

/// Native value of `unit`, undoing a foreign byte order.
#[inline]
fn value<E: Encoding>(unit: E::Unit) -> u32 {
    if E::FOREIGN {
        unit.swap_bytes().to_u32()
    } else {
        unit.to_u32()
    }
}

/// Whether the codepoint starting at `at` is LF; `false` past the end.
#[inline]
fn is_lf_at<E: Encoding>(input: &[E::Unit], at: usize) -> bool {
    let mut rest = input.get(at..).unwrap_or_default();
    !rest.is_empty() && E::decode(&mut rest) == LF
}

/// Splits encoded text into lines ending at `ends`.
///
/// Offsets are code units. Decoding is permissive: units inside malformed
/// sequences that equal a line end still end a line. Use [`lines_str`] for
/// `&str` lines.
///
/// # Example
///
/// ```
/// use xutf::{LineEnds, Utf16, lines};
///
/// let text: Vec<u16> = "one\r\ntwo\rthree".encode_utf16().collect();
/// let mut it = lines::<Utf16>(&text, LineEnds::Universal);
/// let first = it.next().unwrap();
/// assert_eq!(String::from_utf16_lossy(first.text()), "one");
/// assert_eq!(first.end(), [u16::from(b'\r'), u16::from(b'\n')]);
/// assert_eq!(it.map(|line| line.at()).collect::<Vec<_>>(), [5, 9]);
/// ```
#[inline]
#[must_use]
pub fn lines<E: Encoding>(input: &[E::Unit], ends: LineEnds) -> Lines<'_, E> {
    Lines {
        input,
        ends,
        start: 0,
        _encoding: PhantomData,
    }
}

impl<E: Encoding> Lines<'_, E> {
    /// Offset just past the terminator ending the line at `at`, or the text
    /// end of that line when no terminator starts there.
    ///
    /// `next` is the offset just past the codepoint at `at`.
    #[inline]
    fn terminator(&self, at: usize, cp: u32, next: usize, start: usize) -> Option<usize> {
        // A CR that closes a CRLF pair joins the terminator, so the line's
        // text stops before it.
        let cr_before = at > start && value::<E>(self.input[at - 1]) == CR;
        let text_end = match self.ends {
            LineEnds::Lf => {
                if cp != LF {
                    return None;
                }
                if cr_before { at - 1 } else { at }
            }
            LineEnds::Universal => match cp {
                LF => {
                    if cr_before {
                        at - 1
                    } else {
                        at
                    }
                }
                // CRLF terminates at the LF, which carries the CR with it.
                CR if is_lf_at::<E>(self.input, next) => return None,
                CR => at,
                _ => return None,
            },
            LineEnds::Unicode => match cp {
                LF => {
                    if cr_before {
                        at - 1
                    } else {
                        at
                    }
                }
                CR if is_lf_at::<E>(self.input, next) => return None,
                CR | VT | FF | FS | GS | RS | NEL | LS | PS => at,
                _ => return None,
            },
        };
        Some(text_end)
    }
}

impl<'a, E: Encoding> Iterator for Lines<'a, E> {
    type Item = Line<'a, [E::Unit]>;

    fn next(&mut self) -> Option<Self::Item> {
        let len = self.input.len();
        let start = self.start;
        if start >= len {
            return None;
        }
        let mut at = start;
        while at < len {
            let mut rest = &self.input[at..];
            let cp = E::decode(&mut rest);
            let next = len - rest.len();
            if let Some(text_end) = self.terminator(at, cp, next, start) {
                self.start = next;
                return Some(Line {
                    full: &self.input[start..next],
                    text_len: text_end - start,
                    at: start,
                });
            }
            at = next;
        }
        // Unterminated final line.
        self.start = len;
        Some(Line {
            full: &self.input[start..],
            text_len: len - start,
            at: start,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rest = self.input.len() - self.start;
        (usize::from(rest != 0), Some(rest))
    }
}

impl<E: Encoding> FusedIterator for Lines<'_, E> {}

/// Iterator over the lines of a `&str`, created by [`lines_str`].
#[derive(Clone, Debug)]
pub struct StrLines<'a>(Lines<'a, Utf8>);

/// Splits a string into lines ending at `ends`; offsets are bytes.
///
/// # Example
///
/// ```
/// use xutf::{LineEnds, lines_str};
///
/// let text = "fn main() {\r\n\tprintln!();\n}";
/// let lines: Vec<&str> = lines_str(text, LineEnds::Lf)
///     .map(|line| line.text())
///     .collect();
/// assert_eq!(lines, ["fn main() {", "\tprintln!();", "}"]);
///
/// // `with_end` matches `split_inclusive('\n')` exactly.
/// let kept: Vec<&str> = lines_str(text, LineEnds::Lf)
///     .map(|line| line.with_end())
///     .collect();
/// assert_eq!(kept, text.split_inclusive('\n').collect::<Vec<_>>());
/// assert_eq!(lines_str("a\nb\n", LineEnds::Lf).count(), 2);
/// ```
#[inline]
#[must_use]
pub fn lines_str(input: &str, ends: LineEnds) -> StrLines<'_> {
    StrLines(lines::<Utf8>(input.as_bytes(), ends))
}

impl<'a> Iterator for StrLines<'a> {
    type Item = Line<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        let line = self.0.next()?;
        // Every terminator is a whole character (an ASCII byte, or a complete
        // NEL/LS/PS sequence), so both cuts land on char boundaries.
        let full = core::str::from_utf8(line.full).expect("terminators split on char boundaries");
        Some(Line {
            full,
            text_len: line.text_len,
            at: line.at,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl FusedIterator for StrLines<'_> {}

#[cfg(test)]
mod tests {
    use super::{LineEnds, lines, lines_str};
    use crate::{Utf8, Utf16, Utf32};

    fn kept(text: &str, ends: LineEnds) -> Vec<&str> {
        lines_str(text, ends).map(|line| line.with_end()).collect()
    }

    fn texts(text: &str, ends: LineEnds) -> Vec<&str> {
        lines_str(text, ends).map(|line| line.text()).collect()
    }

    #[test]
    fn lf_keeps_the_terminator_exactly_like_split_inclusive() {
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\nb\n",
            "\n",
            "\n\n",
            "a\r\nb\n",
            "a\r\n",
            "trailing\n\n",
        ] {
            assert_eq!(
                kept(text, LineEnds::Lf),
                text.split_inclusive('\n').collect::<Vec<_>>(),
                "input {text:?}"
            );
        }
    }

    #[test]
    fn lf_text_matches_str_lines() {
        for text in ["", "a", "a\n", "a\r\nb", "a\rb", "\r\n", "x\r\n\r\ny"] {
            assert_eq!(
                texts(text, LineEnds::Lf),
                text.lines().collect::<Vec<_>>(),
                "input {text:?}"
            );
        }
    }

    #[test]
    fn lf_treats_a_lone_cr_as_text() {
        let line = lines_str("a\rb", LineEnds::Lf).next().expect("one line");
        assert_eq!(line.text(), "a\rb");
        assert_eq!(line.end(), "");
        assert_eq!(line.at(), 0);
    }

    #[test]
    fn universal_splits_lone_cr_and_keeps_crlf_together() {
        assert_eq!(
            kept("one\r\ntwo\rthree", LineEnds::Universal),
            ["one\r\n", "two\r", "three"]
        );
        assert_eq!(
            texts("one\r\ntwo\rthree", LineEnds::Universal),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn unicode_splits_the_extra_separators() {
        assert_eq!(
            kept("a\u{2028}b\x0cc\u{85}d", LineEnds::Unicode),
            ["a\u{2028}", "b\x0c", "c\u{85}", "d"]
        );
        assert_eq!(
            texts("a\u{2029}b\u{0b}c", LineEnds::Unicode),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn offsets_are_code_units_of_the_input() {
        let offsets: Vec<usize> = lines_str("\u{754c}\na\nb", LineEnds::Lf)
            .map(|line| line.at())
            .collect();
        assert_eq!(offsets, [0, 4, 6]);

        let text: Vec<u16> = "\u{1f600}\na".encode_utf16().collect();
        let offsets: Vec<usize> = lines::<Utf16>(&text, LineEnds::Lf)
            .map(|line| line.at())
            .collect();
        assert_eq!(offsets, [0, 3]);
    }

    #[test]
    fn splits_utf16_and_utf32_input() {
        let wide: Vec<u16> = "one\r\ntwo".encode_utf16().collect();
        let kept: Vec<Vec<u16>> = lines::<Utf16>(&wide, LineEnds::Lf)
            .map(|line| line.with_end().to_vec())
            .collect();
        assert_eq!(kept, [vec![111, 110, 101, 13, 10], vec![116, 119, 111]]);

        let wide: Vec<u32> = "a\nb".chars().map(u32::from).collect();
        let texts: Vec<String> = lines::<Utf32>(&wide, LineEnds::Lf)
            .map(|line| {
                line.text()
                    .iter()
                    .filter_map(|&cp| char::from_u32(cp))
                    .collect()
            })
            .collect();
        assert_eq!(texts, ["a", "b"]);
    }

    #[test]
    fn utf8_unicode_separators_span_whole_sequences() {
        // NEL (C2 85), LS (E2 80 A8) and PS (E2 80 A9) must not split mid-char.
        for (text, sep) in [
            ("a\u{85}b", "\u{85}"),
            ("a\u{2028}b", "\u{2028}"),
            ("a\u{2029}b", "\u{2029}"),
        ] {
            let first = lines_str(text, LineEnds::Unicode).next().expect("a line");
            assert_eq!(first.text(), "a", "{text:?}");
            assert_eq!(first.end(), sep, "{text:?}");
        }
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert_eq!(lines_str("", LineEnds::Lf).count(), 0);
        assert_eq!(lines::<Utf8>(b"", LineEnds::Lf).count(), 0);
    }

    #[test]
    fn size_hint_bounds_the_remaining_lines() {
        let mut it = lines_str("a\nb\nc", LineEnds::Lf);
        assert_eq!(it.size_hint(), (1, Some(5)));
        it.next();
        assert_eq!(it.size_hint(), (1, Some(3)));
    }
}
