//! The [`Text`] extension trait: encoding-generic helpers as methods on `str`
//! and on native code-unit slices.
//!
//! Callers reach for it when the same arithmetic must work on UTF-8 and on
//! UTF-16/32 units without naming the encoding at each site:
//! `text.transcoded_len::<u16>()` sizes a UTF-16 buffer, and
//! `line.unit_offset::<u16>(n)` turns a UTF-16 index back into a byte offset.

use crate::{
    Utf8, Utf16, Utf32,
    convert::transcoded_len,
    encoding::{Encoding, Unit},
    offset::unit_offset,
};

/// Encoding-generic text primitives as methods on `str` and native unit
/// slices.
///
/// Implemented for `str` (UTF-8 bytes) and for `[u8]`, `[u16]` and `[u32]`
/// (native-endian UTF-8/16/32). Foreign-endian data goes through the free
/// functions with an explicit encoding instead.
///
/// # Example
///
/// ```
/// use xutf::Text;
///
/// let text = "a\u{1f600}b";
/// assert_eq!(text.transcoded_len::<u16>(), 4);
/// assert_eq!(text.transcoded_len::<u32>(), 3);
/// assert_eq!(text.unit_offset::<u16>(3), 5);
/// ```
pub trait Text {
    /// Native encoding of this text.
    type Encoding: Encoding;

    /// Raw code units backing this text.
    #[must_use]
    fn as_units(&self) -> &[<Self::Encoding as Encoding>::Unit];

    /// Unit count this text occupies once transcoded to unit type `U` (see
    /// [`transcoded_len`](crate::transcoded_len)).
    #[inline]
    #[must_use]
    fn transcoded_len<U: Unit>(&self) -> usize {
        transcoded_len::<Self::Encoding, U::Native>(self.as_units())
    }

    /// Offset of unit `n` of this text's transcoding to `U`, rounded down to
    /// the start of the codepoint holding it (see
    /// [`unit_offset`](crate::unit_offset)).
    #[inline]
    #[must_use]
    fn unit_offset<U: Unit>(&self, n: usize) -> usize {
        unit_offset::<Self::Encoding, U::Native>(self.as_units(), n)
    }
}

impl Text for str {
    type Encoding = Utf8;

    #[inline]
    fn as_units(&self) -> &[u8] {
        self.as_bytes()
    }
}

macro_rules! impl_text_for_units {
    ($unit:ty, $encoding:ty) => {
        impl Text for [$unit] {
            type Encoding = $encoding;

            #[inline]
            fn as_units(&self) -> &[$unit] {
                self
            }
        }
    };
}

impl_text_for_units!(u8, Utf8);
impl_text_for_units!(u16, Utf16);
impl_text_for_units!(u32, Utf32);

#[cfg(test)]
mod tests {
    use super::Text;

    #[test]
    fn str_counts_utf16_units() {
        assert_eq!("".transcoded_len::<u16>(), 0);
        assert_eq!("ascii".transcoded_len::<u16>(), 5);
        assert_eq!("a\u{1f600}".transcoded_len::<u16>(), 3);
        assert_eq!("a\u{1f600}".transcoded_len::<u32>(), 2);
    }

    #[test]
    fn str_offsets_map_utf16_indices_to_bytes() {
        let text = "a\u{1f600}b";
        assert_eq!(text.unit_offset::<u16>(0), 0);
        assert_eq!(text.unit_offset::<u16>(1), 1);
        assert_eq!(text.unit_offset::<u16>(2), 1);
        assert_eq!(text.unit_offset::<u16>(3), 5);
        assert_eq!(text.unit_offset::<u32>(2), 5);
    }

    #[test]
    fn unit_slices_report_their_own_encoding() {
        let bytes = "caf\u{e9}".as_bytes();
        assert_eq!(bytes.transcoded_len::<u32>(), 4);
        assert_eq!(bytes.transcoded_len::<u16>(), 4);

        let wide: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        assert_eq!(wide.as_slice().transcoded_len::<u8>(), 5);
        assert_eq!(wide.as_slice().unit_offset::<u8>(1), 1);
    }

    #[test]
    fn as_units_exposes_the_backing_slice() {
        assert_eq!("ab".as_units(), b"ab");
        assert_eq!(Text::as_units(&[1u16, 2][..]), [1u16, 2]);
    }

    #[test]
    fn a_sliced_prefix_counts_only_itself() {
        let text = "a\u{1f600}b";
        let start = 5;
        assert_eq!(text[..start].transcoded_len::<u32>(), 2);
        assert_eq!(text[..start].transcoded_len::<u16>(), 3);
        // `xfs` derives a 1-based column from exactly this.
        assert_eq!(text[..start].transcoded_len::<u32>() + 1, 3);
    }
}
