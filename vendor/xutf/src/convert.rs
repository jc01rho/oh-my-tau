//! Transcoding between encodings and the unit-length arithmetic that callers
//! use to size buffers.
//!
//! Only the length path is needed by the vendored call sites:
//! [`transcoded_len`] sizes a UTF-16 or UTF-32 view of a UTF-8 string, and
//! [`unit_offset`](crate::unit_offset) inverts it. Encoding itself lives in
//! [`crate::encoding`].

use crate::encoding::Encoding;

/// Unit count `src` occupies once transcoded to `T`.
///
/// Decoding is permissive, so malformed input counts the codepoints the
/// permissive decoder would produce. `transcoded_len::<Utf8, Utf32>` is the
/// codepoint count of a UTF-8 byte slice.
///
/// # Example
///
/// ```
/// use xutf::{Utf8, Utf16, Utf32, transcoded_len};
///
/// let text = "caf\u{e9} \u{1f600}".as_bytes();
/// assert_eq!(transcoded_len::<Utf8, Utf32>(text), 6);
/// // The emoji is a surrogate pair, so UTF-16 needs one unit more.
/// assert_eq!(transcoded_len::<Utf8, Utf16>(text), 7);
/// ```
#[must_use]
pub fn transcoded_len<F: Encoding, T: Encoding>(src: &[F::Unit]) -> usize {
    if F::KIND == T::KIND && F::FOREIGN == T::FOREIGN {
        return src.len();
    }
    let mut rest = src;
    let mut total = 0usize;
    while !rest.is_empty() {
        let cp = F::decode(&mut rest);
        total += T::encoded_length(cp);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::transcoded_len;
    use crate::{Utf8, Utf16, Utf32};

    #[test]
    fn utf8_to_utf32_counts_codepoints() {
        assert_eq!(transcoded_len::<Utf8, Utf32>(b""), 0);
        assert_eq!(transcoded_len::<Utf8, Utf32>(b"ascii"), 5);
        assert_eq!(transcoded_len::<Utf8, Utf32>("caf\u{e9}".as_bytes()), 4);
        assert_eq!(
            transcoded_len::<Utf8, Utf32>("\u{754c}\u{1f600}".as_bytes()),
            2
        );
    }

    #[test]
    fn utf8_to_utf16_counts_code_units() {
        let text = "caf\u{e9} \u{1f600}";
        assert_eq!(
            transcoded_len::<Utf8, Utf16>(text.as_bytes()),
            text.encode_utf16().count()
        );
    }

    #[test]
    fn same_encoding_is_the_identity() {
        assert_eq!(transcoded_len::<Utf8, Utf8>(b"caf\xc3\xa9"), 5);
        let wide: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        assert_eq!(transcoded_len::<Utf16, Utf16>(&wide), 3);
    }

    #[test]
    fn utf16_to_utf8_counts_bytes() {
        let wide: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        assert_eq!(transcoded_len::<Utf16, Utf8>(&wide), "a\u{1f600}".len());
    }
}
