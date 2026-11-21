//! Codepoint-aligned offsets into encoded text: the inverse of
//! [`transcoded_len`](crate::transcoded_len).
//!
//! Callers use it to map a UTF-16 index (JavaScript, Cocoa, Win32) back to a
//! byte offset for slicing, and to truncate by codepoint count.

use crate::encoding::Encoding;

/// Offset in `src` (in its own units) of unit `n` of its transcoding to `T`,
/// rounded down to the start of the codepoint holding that unit; `src.len()`
/// when the transcoding is `n` units or shorter.
///
/// The inverse of [`transcoded_len`](crate::transcoded_len): the largest
/// codepoint boundary `k` with `transcoded_len::<F, T>(&src[..k]) <= n`.
/// `unit_offset::<Utf8, Utf32>` is the byte offset of `str::chars()` item `n`
/// (`char_indices().nth(n)`), `unit_offset::<Utf8, Utf16>` maps a UTF-16 index
/// to a byte offset, and `unit_offset::<Utf16, Utf8>` a byte offset to a UTF-16
/// index. Boundaries are the permissive decoder's, so malformed input gets the
/// same answer as walking the codepoints.
///
/// # Example
///
/// ```
/// use xutf::{Utf8, Utf16, Utf32, unit_offset};
///
/// let s = "a\u{1f600}b".as_bytes();
/// assert_eq!(unit_offset::<Utf8, Utf32>(s, 2), 5); // char 2, `b`, at byte 5
/// assert_eq!(unit_offset::<Utf8, Utf16>(s, 2), 1); // inside the pair: the emoji
/// assert_eq!(unit_offset::<Utf8, Utf16>(s, 4), s.len());
/// ```
#[must_use]
pub fn unit_offset<F: Encoding, T: Encoding>(src: &[F::Unit], n: usize) -> usize {
    if F::KIND == T::KIND && F::FOREIGN == T::FOREIGN {
        return n.min(src.len());
    }
    let mut at = 0usize;
    let mut left = n;
    let mut rest = src;
    while !rest.is_empty() {
        let before = rest.len();
        let cp = F::decode(&mut rest);
        let width = T::encoded_length(cp);
        if width > left {
            // `n` falls inside this codepoint: round down to its start.
            return at;
        }
        left -= width;
        at += before - rest.len();
    }
    at
}

#[cfg(test)]
mod tests {
    use super::unit_offset;
    use crate::{Utf8, Utf16, Utf32};

    #[test]
    fn utf8_to_utf32_matches_char_indices() {
        let text = "a\u{754c}\u{1f600}b";
        for (n, expected) in text
            .char_indices()
            .map(|(at, _)| at)
            .chain([text.len()])
            .enumerate()
        {
            assert_eq!(
                unit_offset::<Utf8, Utf32>(text.as_bytes(), n),
                expected,
                "char {n}"
            );
        }
    }

    #[test]
    fn utf8_to_utf16_rounds_down_inside_a_surrogate_pair() {
        let text = "a\u{1f600}b";
        let bytes = text.as_bytes();
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, 0), 0);
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, 1), 1);
        // Both units of the pair round down to the emoji's lead byte.
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, 2), 1);
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, 3), 5);
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, 4), bytes.len());
    }

    #[test]
    fn offset_past_the_end_is_the_length() {
        assert_eq!(unit_offset::<Utf8, Utf16>(b"abc", 99), 3);
        assert_eq!(unit_offset::<Utf8, Utf32>(b"", 0), 0);
        assert_eq!(unit_offset::<Utf8, Utf32>(b"", 3), 0);
    }

    #[test]
    fn same_encoding_offsets_are_unit_indices() {
        let wide: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        assert_eq!(unit_offset::<Utf16, Utf16>(&wide, 2), 2);
        assert_eq!(unit_offset::<Utf16, Utf16>(&wide, 9), 3);
    }

    #[test]
    fn utf16_index_slices_cleanly_at_a_surrogate_pair() {
        let text = "a\u{1f600}b";
        let end = unit_offset::<Utf8, Utf16>(text.as_bytes(), 3);
        assert_eq!(&text[..end], "a\u{1f600}");
    }

    #[test]
    fn is_the_inverse_of_transcoded_len_at_codepoint_boundaries() {
        let text = "caf\u{e9} \u{1f600}\u{754c}";
        let bytes = text.as_bytes();
        let mut utf16_index = 0usize;
        for (byte_at, ch) in text.char_indices() {
            assert_eq!(unit_offset::<Utf8, Utf16>(bytes, utf16_index), byte_at);
            utf16_index += ch.len_utf16();
        }
        assert_eq!(unit_offset::<Utf8, Utf16>(bytes, utf16_index), bytes.len());
    }
}
