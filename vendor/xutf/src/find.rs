//! Substring search over encoded text, reporting unit offsets.
//!
//! The needle is compared unit for unit, so a UTF-8 needle matches bytes and a
//! UTF-16 needle matches code units. Callers use [`find_iter`] to walk every
//! non-overlapping occurrence of a literal in a large document.

use core::{fmt, iter::FusedIterator, marker::PhantomData};

use crate::encoding::Encoding;

/// Iterator over the unit offsets of non-overlapping occurrences of a needle,
/// left to right.
///
/// Created by [`find_iter`]. It remembers the resume offset, so each call
/// continues where the last match ended.
pub struct FindIter<'a, E: Encoding> {
    haystack: &'a [E::Unit],
    needle: &'a [E::Unit],
    /// Next haystack offset to test.
    next: usize,
    _encoding: PhantomData<E>,
}

impl<E: Encoding> Clone for FindIter<'_, E> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            haystack: self.haystack,
            needle: self.needle,
            next: self.next,
            _encoding: PhantomData,
        }
    }
}

impl<E: Encoding> fmt::Debug for FindIter<'_, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FindIter")
            .field("needle_len", &self.needle.len())
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

/// Iterates the unit offsets of the non-overlapping occurrences of `needle` in
/// `haystack`, left to right.
///
/// Use it to count or replace every occurrence; `"aaaa"` searched for `"aa"`
/// yields `0` and `2`. An empty needle yields every offset `0..=len`.
///
/// # Example
///
/// ```
/// use xutf::{Utf8, find_iter};
///
/// let hits: Vec<usize> = find_iter::<Utf8>(b"a,b,,c", b",").collect();
/// assert_eq!(hits, [1, 3, 4]);
/// assert_eq!(find_iter::<Utf8>(b"aaaa", b"aa").collect::<Vec<_>>(), [0, 2]);
/// ```
#[inline]
#[must_use]
pub fn find_iter<'a, E: Encoding>(
    haystack: &'a [E::Unit],
    needle: &'a [E::Unit],
) -> FindIter<'a, E> {
    FindIter {
        haystack,
        needle,
        next: 0,
        _encoding: PhantomData,
    }
}

impl<E: Encoding> Iterator for FindIter<'_, E> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let haystack = self.haystack;
        let needle = self.needle;
        if needle.is_empty() {
            // Every boundary including the end is an empty-needle match.
            let at = self.next;
            if at > haystack.len() {
                return None;
            }
            self.next = at + 1;
            return Some(at);
        }
        let last_start = haystack.len().checked_sub(needle.len())?;
        while self.next <= last_start {
            let at = self.next;
            if haystack[at..at + needle.len()] == *needle {
                // Non-overlapping: resume past the whole match.
                self.next = at + needle.len();
                return Some(at);
            }
            self.next = at + 1;
        }
        // Exhausted: park past the last possible start so repeats are `None`.
        self.next = haystack.len();
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Upper bound: one match per remaining start position.
        let remaining = self.haystack.len().saturating_sub(self.next);
        (0, Some(remaining))
    }
}

impl<E: Encoding> FusedIterator for FindIter<'_, E> {}

#[cfg(test)]
mod tests {
    use super::find_iter;
    use crate::{Utf8, Utf16};

    fn hits(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
        find_iter::<Utf8>(haystack, needle).collect()
    }

    #[test]
    fn finds_every_non_overlapping_occurrence() {
        assert_eq!(hits(b"a,b,,c", b","), [1, 3, 4]);
        assert_eq!(hits(b"aaaa", b"aa"), [0, 2]);
        // Non-overlapping: the match at 0 consumes "aba", leaving "bab".
        assert_eq!(hits(b"ababab", b"aba"), [0]);
        assert_eq!(hits(b"abababa", b"aba"), [0, 4]);
    }

    #[test]
    fn matches_at_the_very_end() {
        assert_eq!(hits(b"xxabc", b"abc"), [2]);
        assert_eq!(hits(b"abc", b"abc"), [0]);
    }

    #[test]
    fn reports_nothing_when_the_needle_is_longer() {
        assert_eq!(hits(b"ab", b"abc"), []);
        assert_eq!(hits(b"", b"a"), []);
        assert_eq!(hits(b"abc", b"abcd"), []);
    }

    #[test]
    fn empty_needle_yields_every_boundary() {
        assert_eq!(hits(b"", b""), [0]);
        assert_eq!(hits(b"ab", b""), [0, 1, 2]);
    }

    #[test]
    fn multibyte_literals_match_byte_for_byte() {
        let text = "caf\u{e9} caf\u{e9}";
        assert_eq!(
            find_iter::<Utf8>(text.as_bytes(), "caf\u{e9}".as_bytes()).collect::<Vec<_>>(),
            [0, 6]
        );
    }

    #[test]
    fn works_over_utf16_code_units() {
        let haystack: Vec<u16> = "a\u{1f600}b\u{1f600}".encode_utf16().collect();
        let needle: Vec<u16> = "\u{1f600}".encode_utf16().collect();
        assert_eq!(
            find_iter::<Utf16>(&haystack, &needle).collect::<Vec<_>>(),
            [1, 4]
        );
    }

    #[test]
    fn iteration_is_fused_and_cloneable() {
        let mut it = find_iter::<Utf8>(b"aXa", b"a");
        let mut copy = it.clone();
        assert_eq!(it.next(), Some(0));
        assert_eq!(copy.next(), Some(0));
        assert_eq!(it.next(), Some(2));
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);
    }

    #[test]
    fn size_hint_bounds_the_matches() {
        let it = find_iter::<Utf8>(b"abc", b"a");
        let (low, high) = it.size_hint();
        assert_eq!(low, 0);
        assert_eq!(high, Some(3));
    }
}
