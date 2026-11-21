//! [`UnitSet`]: membership scans over small sets of code-unit values.
//!
//! A set holds up to [`UnitSet::MAX_RANGES`] disjoint inclusive ranges of
//! values in `0..=0xFF`. Values above `0xFF` are never members, so any
//! UTF-16/UTF-32 unit outside Latin-1 is a non-member. UTF-8 input is matched
//! as raw bytes: a member `>= 0x80` matches lead or continuation bytes with
//! that value. Malformed input is scanned as-is and never panics.

use core::ops::RangeInclusive;

use crate::encoding::{Encoding, Unit};

/// Most disjoint ranges a set can hold.
const MAX_RANGES: usize = 8;

/// A set of code-unit values in `0..=0xFF`.
///
/// Use it for the byte-class loops of tokenizers and parsers: skipping
/// whitespace, finding the next delimiter, trimming, or counting a class of
/// bytes.
///
/// # Example
///
/// ```
/// use xutf::{UnitSet, Utf8, Utf16};
///
/// const DELIMS: UnitSet = UnitSet::new(b",;:");
/// assert_eq!(DELIMS.find::<Utf8>(b"key: value"), Some(3));
///
/// let wide: Vec<u16> = "a\u{3000}b;c".encode_utf16().collect();
/// assert_eq!(DELIMS.find::<Utf16>(&wide), Some(3));
///
/// // Counting UTF-8 continuation bytes is `len - count`.
/// const CONTINUATIONS: UnitSet = UnitSet::from_ranges(&[0x80..=0xbf]);
/// assert_eq!(CONTINUATIONS.count::<Utf8>("caf\u{e9}".as_bytes()), 1);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitSet {
    /// Membership bitmap of `0..=0xFF`, for scalar probes.
    map: [u64; 4],
}

impl UnitSet {
    /// C `isspace` / Python `bytes.isspace` set: `\t \n \v \f \r` and space
    /// (note: includes `\v`, unlike [`u8::is_ascii_whitespace`]).
    pub const ASCII_WHITESPACE: Self = Self::new(b"\t\n\x0b\x0c\r ");

    /// Most disjoint ranges a set can hold.
    pub const MAX_RANGES: usize = MAX_RANGES;

    /// Builds the set of `members`; duplicate values merge.
    ///
    /// # Panics
    ///
    /// If the members form more than [`Self::MAX_RANGES`] disjoint ranges (a
    /// compile error in const context).
    ///
    /// # Example
    ///
    /// ```
    /// use xutf::UnitSet;
    ///
    /// const QUOTES: UnitSet = UnitSet::new(b"\"'`");
    /// assert!(QUOTES.contains(u32::from(b'`')));
    /// assert!(!QUOTES.contains(u32::from(b'a')));
    /// ```
    #[must_use]
    pub const fn new(members: &[u8]) -> Self {
        let mut map = [0u64; 4];
        let mut i = 0;
        while i < members.len() {
            let b = members[i] as usize;
            map[b >> 6] |= 1 << (b & 63);
            i += 1;
        }
        Self::from_bitmap(map)
    }

    /// Builds the set covering `ranges`; overlapping or adjacent ranges merge
    /// and empty ranges (`start > end`) are ignored.
    ///
    /// # Panics
    ///
    /// If the merged ranges number more than [`Self::MAX_RANGES`] (a compile
    /// error in const context).
    ///
    /// # Example
    ///
    /// ```
    /// use xutf::{UnitSet, Utf8};
    ///
    /// const IDENT: UnitSet =
    ///     UnitSet::from_ranges(&[b'0'..=b'9', b'A'..=b'Z', b'_'..=b'_', b'a'..=b'z']);
    /// assert_eq!(IDENT.find_not::<Utf8>(b"snake_case2 = 1"), Some(11));
    /// ```
    #[must_use]
    pub const fn from_ranges(ranges: &[RangeInclusive<u8>]) -> Self {
        let mut map = [0u64; 4];
        let mut i = 0;
        while i < ranges.len() {
            let mut b = *ranges[i].start() as usize;
            let end = *ranges[i].end() as usize;
            while b <= end {
                map[b >> 6] |= 1 << (b & 63);
                b += 1;
            }
            i += 1;
        }
        Self::from_bitmap(map)
    }

    /// Validates the bitmap's range count, which bounds the const constructors.
    const fn from_bitmap(map: [u64; 4]) -> Self {
        let mut set = Self { map };
        let mut b = 0;
        let mut runs = 0;
        while b < 256 {
            if (set.map[b >> 6] >> (b & 63)) & 1 == 0 {
                b += 1;
                continue;
            }
            while b < 256 && (set.map[b >> 6] >> (b & 63)) & 1 != 0 {
                b += 1;
            }
            runs += 1;
        }
        assert!(
            runs <= MAX_RANGES,
            "UnitSet holds at most MAX_RANGES (8) disjoint ranges"
        );
        set.map = map;
        set
    }

    /// Whether the native-order unit value `unit` is a member.
    #[inline]
    #[must_use]
    pub const fn contains(&self, unit: u32) -> bool {
        unit < 256 && (self.map[(unit >> 6) as usize] >> (unit & 63)) & 1 != 0
    }

    /// Whether every member is below `0x80`, so splitting UTF-8 at members
    /// keeps char boundaries (required by the `_str` methods).
    #[inline]
    #[must_use]
    pub const fn is_ascii(&self) -> bool {
        self.map[2] == 0 && self.map[3] == 0
    }

    /// Membership of a raw unit of encoding `E`.
    #[inline]
    fn has<E: Encoding>(&self, unit: E::Unit) -> bool {
        let unit = if E::FOREIGN { unit.swap_bytes() } else { unit };
        self.contains(unit.to_u32())
    }

    /// Index of the first member unit in `input`.
    ///
    /// # Example
    ///
    /// ```
    /// use xutf::{UnitSet, Utf8};
    ///
    /// const JSON_ESCAPE: UnitSet =
    ///     UnitSet::from_ranges(&[0..=0x1f, b'"'..=b'"', b'\\'..=b'\\']);
    /// assert_eq!(JSON_ESCAPE.find::<Utf8>(b"plain text \"quoted\""), Some(11));
    /// assert_eq!(JSON_ESCAPE.find::<Utf8>(b"plain text"), None);
    /// ```
    #[must_use]
    pub fn find<E: Encoding>(&self, input: &[E::Unit]) -> Option<usize> {
        input.iter().position(|&unit| self.has::<E>(unit))
    }

    /// Index of the first non-member unit in `input`; the leading member span
    /// is `find_not(input).unwrap_or(input.len())` units long.
    #[must_use]
    pub fn find_not<E: Encoding>(&self, input: &[E::Unit]) -> Option<usize> {
        input.iter().position(|&unit| !self.has::<E>(unit))
    }

    /// Index of the last member unit in `input`.
    #[must_use]
    pub fn rfind<E: Encoding>(&self, input: &[E::Unit]) -> Option<usize> {
        input.iter().rposition(|&unit| self.has::<E>(unit))
    }

    /// Index of the last non-member unit in `input`.
    #[must_use]
    pub fn rfind_not<E: Encoding>(&self, input: &[E::Unit]) -> Option<usize> {
        input.iter().rposition(|&unit| !self.has::<E>(unit))
    }

    /// Number of member units in `input`.
    ///
    /// # Example
    ///
    /// ```
    /// use xutf::{UnitSet, Utf16};
    ///
    /// let text: Vec<u16> = "a b\tc\n".encode_utf16().collect();
    /// assert_eq!(UnitSet::ASCII_WHITESPACE.count::<Utf16>(&text), 3);
    /// ```
    #[must_use]
    pub fn count<E: Encoding>(&self, input: &[E::Unit]) -> usize {
        input.iter().filter(|&&unit| self.has::<E>(unit)).count()
    }

    /// `input` without leading and trailing members.
    ///
    /// # Example
    ///
    /// ```
    /// use xutf::{UnitSet, Utf8};
    ///
    /// assert_eq!(UnitSet::ASCII_WHITESPACE.trim::<Utf8>(b" \x0bkey\r\n"), b"key");
    /// ```
    #[must_use]
    pub fn trim<'a, E: Encoding>(&self, input: &'a [E::Unit]) -> &'a [E::Unit] {
        self.trim_end::<E>(self.trim_start::<E>(input))
    }

    /// `input` without leading members.
    #[must_use]
    pub fn trim_start<'a, E: Encoding>(&self, input: &'a [E::Unit]) -> &'a [E::Unit] {
        let start = self.find_not::<E>(input).unwrap_or(input.len());
        &input[start..]
    }

    /// `input` without trailing members.
    #[must_use]
    pub fn trim_end<'a, E: Encoding>(&self, input: &'a [E::Unit]) -> &'a [E::Unit] {
        let end = self.rfind_not::<E>(input).map_or(0, |at| at + 1);
        &input[..end]
    }

    /// [`UnitSet::trim`] over UTF-8 `str`.
    ///
    /// # Panics
    ///
    /// Unless [`UnitSet::is_ascii`], since trimming a non-ASCII member could
    /// split a char.
    #[must_use]
    pub fn trim_str<'a>(&self, input: &'a str) -> &'a str {
        assert!(self.is_ascii(), "UnitSet::trim_str needs an ASCII-only set");
        let trimmed = self.trim::<crate::Utf8>(input.as_bytes());
        // Members are ASCII, so both ends sit on char boundaries.
        core::str::from_utf8(trimmed).expect("ASCII members split on char boundaries")
    }
}

#[cfg(test)]
// A one-range set is the documented `from_ranges` call shape, not a slip.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::UnitSet;
    use crate::{Utf8, Utf16, Utf32};

    const CONTINUATIONS: UnitSet = UnitSet::from_ranges(&[0x80..=0xbf]);

    #[test]
    fn counts_utf8_continuation_bytes() {
        // `len - continuations` is the char count the awk/wc kernels use.
        for text in ["", "ascii", "caf\u{e9}", "\u{754c}\u{1f600}", "a\u{301}"] {
            let bytes = text.as_bytes();
            let chars = bytes.len() - CONTINUATIONS.count::<Utf8>(bytes);
            assert_eq!(chars, text.chars().count(), "input {text:?}");
        }
    }

    #[test]
    fn count_matches_a_naive_scan_across_encodings() {
        let wide16: Vec<u16> = "a b\tc\n\u{3000}".encode_utf16().collect();
        assert_eq!(UnitSet::ASCII_WHITESPACE.count::<Utf16>(&wide16), 3);

        let wide32: Vec<u32> = "x y z".chars().map(u32::from).collect();
        assert_eq!(UnitSet::ASCII_WHITESPACE.count::<Utf32>(&wide32), 2);
    }

    #[test]
    fn find_and_rfind_agree_on_both_ends() {
        let bytes = b"  key\tvalue  ";
        let ws = UnitSet::ASCII_WHITESPACE;
        assert_eq!(ws.find::<Utf8>(bytes), Some(0));
        assert_eq!(ws.find_not::<Utf8>(bytes), Some(2));
        assert_eq!(ws.rfind::<Utf8>(bytes), Some(bytes.len() - 1));
        assert_eq!(ws.rfind_not::<Utf8>(bytes), Some(bytes.len() - 3));
    }

    #[test]
    fn trim_cuts_both_ends_and_handles_all_member_input() {
        let ws = UnitSet::ASCII_WHITESPACE;
        assert_eq!(ws.trim::<Utf8>(b" \x0bkey\r\n"), b"key");
        assert_eq!(ws.trim::<Utf8>(b"key"), b"key");
        assert_eq!(ws.trim::<Utf8>(b"   "), b"");
        assert_eq!(ws.trim::<Utf8>(b""), b"");
    }

    #[test]
    fn trim_str_keeps_multibyte_text_intact() {
        let ws = UnitSet::ASCII_WHITESPACE;
        assert_eq!(ws.trim_str("  \u{754c}\u{1f600}  "), "\u{754c}\u{1f600}");
        assert_eq!(ws.trim_str("plain"), "plain");
        assert_eq!(ws.trim_str(""), "");
    }

    #[test]
    fn membership_is_bounded_to_latin1() {
        assert!(UnitSet::new(b"\n").contains(0x0a));
        assert!(!UnitSet::new(b"\n").contains(0x100));
        assert!(!UnitSet::new(b"\n").contains(u32::MAX));
    }

    #[test]
    fn ascii_only_sets_report_themselves() {
        assert!(UnitSet::new(b" \t").is_ascii());
        assert!(!CONTINUATIONS.is_ascii());
    }

    #[test]
    fn wide_units_above_latin1_are_never_members() {
        // A UTF-16 unit outside Latin-1 must not match even a 0xFF member.
        let full = UnitSet::from_ranges(&[0..=0xff]);
        let wide = [0x3000u16];
        assert_eq!(full.count::<Utf16>(&wide), 0);
        assert_eq!(full.count::<Utf16>(&[0xff]), 1);
    }

    #[test]
    fn foreign_units_are_swapped_before_matching() {
        let lf = UnitSet::new(b"\n");
        let native = [u16::from(b'\n')];
        let foreign: Vec<u16> = native.iter().copied().map(u16::swap_bytes).collect();
        assert_eq!(lf.count::<Utf16<true>>(&foreign), 1);
        assert_eq!(lf.count::<Utf16<false>>(&native), 1);
    }

    #[test]
    fn const_construction_rejects_too_many_ranges() {
        // Nine isolated values are nine ranges, one past MAX_RANGES.
        let nine = [0x00u8, 0x02, 0x04, 0x06, 0x08, 0x0a, 0x0c, 0x0e, 0x10];
        let result = std::panic::catch_unwind(|| UnitSet::new(&nine));
        assert!(result.is_err());
        // Eight is the limit and must succeed.
        let eight = [0x00u8, 0x02, 0x04, 0x06, 0x08, 0x0a, 0x0c, 0x0e];
        assert_eq!(UnitSet::new(&eight).count::<Utf8>(&eight), 8);
    }
}
