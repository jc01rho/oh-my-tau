//! Codec traits: the [`Encoding`] family and its [`Unit`] code units.
//!
//! Every operation is permissive, matching upstream `xutf`: decoding never
//! fails, a truncated UTF-8 sequence decodes to `0` (consuming the remainder),
//! lone UTF-16 surrogates pass through, and garbage-in produces garbage-out.
//! Values are raw `u32` codepoints, not [`char`].

use crate::{Utf8, Utf16, Utf32};

/// Seals [`Encoding`] to this crate's three markers.
pub mod sealed {
    /// Implemented only by [`Utf8`](crate::Utf8), [`Utf16`](crate::Utf16) and
    /// [`Utf32`](crate::Utf32).
    pub trait Sealed {}

    impl Sealed for crate::Utf8 {}
    impl<const FOREIGN: bool> Sealed for crate::Utf16<FOREIGN> {}
    impl<const FOREIGN: bool> Sealed for crate::Utf32<FOREIGN> {}
}

/// Encoding family, used to detect same-encoding fast paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// UTF-8.
    Utf8,
    /// UTF-16, either byte order.
    Utf16,
    /// UTF-32, either byte order.
    Utf32,
}

/// A UTF code unit: `u8` (UTF-8), `u16` (UTF-16), or `u32` (UTF-32).
pub trait Unit: Copy + Eq {
    /// Native-endian encoding whose [`Encoding::Unit`] is `Self`.
    type Native: Encoding<Unit = Self>;

    /// Zero-extending conversion to a codepoint value.
    #[must_use]
    fn to_u32(self) -> u32;

    /// Reverses the byte order; the identity for `u8`.
    #[must_use]
    fn swap_bytes(self) -> Self;
}

impl Unit for u8 {
    type Native = Utf8;

    #[inline]
    fn to_u32(self) -> u32 {
        u32::from(self)
    }

    #[inline]
    fn swap_bytes(self) -> Self {
        self
    }
}

impl Unit for u16 {
    type Native = Utf16<false>;

    #[inline]
    fn to_u32(self) -> u32 {
        u32::from(self)
    }

    #[inline]
    fn swap_bytes(self) -> Self {
        u16::swap_bytes(self)
    }
}

impl Unit for u32 {
    type Native = Utf32<false>;

    #[inline]
    fn to_u32(self) -> u32 {
        self
    }

    #[inline]
    fn swap_bytes(self) -> Self {
        u32::swap_bytes(self)
    }
}

/// A UTF encoding: a stateless codec between raw `u32` codepoints and code
/// units.
///
/// [`FOREIGN`](Encoding::FOREIGN) selects byte order opposite to native, so
/// [`Utf16<true>`](crate::Utf16) is byte-swapped relative to the host. The
/// trait is sealed: it is implemented only by the three encoding markers in
/// this crate, so each encoding family has exactly one code-unit type.
pub trait Encoding: sealed::Sealed {
    /// Code-unit type.
    type Unit: Unit;

    /// Encoding family.
    const KIND: Kind;

    /// `true` when code units are byte-swapped relative to native byte order.
    const FOREIGN: bool;

    /// Length in units of `cp` once encoded.
    ///
    /// Accepts any `u32`, including surrogate halves and values past
    /// `U+10FFFF`, which the permissive decoder can produce from malformed
    /// input.
    #[must_use]
    fn encoded_length(cp: u32) -> usize;

    /// Decodes one codepoint from the front of `input`, advancing it by the
    /// consumed unit count.
    ///
    /// # Panics
    ///
    /// If `input` is empty.
    fn decode(input: &mut &[Self::Unit]) -> u32;
}

impl Encoding for Utf8 {
    type Unit = u8;

    const KIND: Kind = Kind::Utf8;
    const FOREIGN: bool = false;

    #[inline]
    fn encoded_length(cp: u32) -> usize {
        1 + usize::from(cp >> 7 != 0)
            + usize::from(cp >> (5 + 6) != 0)
            + usize::from(cp >> (4 + 6 + 6) != 0)
    }

    #[inline]
    fn decode(input: &mut &[u8]) -> u32 {
        let bytes = *input;
        let front = bytes[0];
        if front < 0x80 {
            *input = &bytes[1..];
            return u32::from(front);
        }
        let len = if front < 0xe0 {
            2
        } else if front < 0xf0 {
            3
        } else {
            4
        };
        if bytes.len() < len {
            // Truncated tail: consume it and decode to `0`, like upstream.
            *input = &[];
            return 0;
        }
        // 5, 4 or 3 payload bits in the lead byte, then 6 per continuation.
        let mut cp = u32::from(front & (0x7f >> len)) << (6 * (len - 1));
        for (k, &byte) in bytes[1..len].iter().enumerate() {
            cp |= u32::from(byte & 0x3f) << (6 * (len - 2 - k));
        }
        *input = &bytes[len..];
        cp
    }
}

impl<const FOREIGN: bool> Encoding for Utf16<FOREIGN> {
    type Unit = u16;

    const KIND: Kind = Kind::Utf16;
    const FOREIGN: bool = FOREIGN;

    #[inline]
    fn encoded_length(cp: u32) -> usize {
        1 + usize::from(cp >> 16 != 0)
    }

    #[inline]
    fn decode(input: &mut &[u16]) -> u32 {
        let units = *input;
        let first = units[0];
        let lo = if FOREIGN {
            u16::swap_bytes(first)
        } else {
            first
        };
        if (lo & 0xfc00) == 0xd800 && units.len() > 1 {
            let second = units[1];
            let hi = if FOREIGN {
                u16::swap_bytes(second)
            } else {
                second
            };
            let cp = 0x1_0000u32
                .wrapping_add((u32::from(lo) - 0xd800) << 10)
                .wrapping_add(u32::from(hi).wrapping_sub(0xdc00));
            *input = &units[2..];
            cp
        } else {
            *input = &units[1..];
            u32::from(lo)
        }
    }
}

impl<const FOREIGN: bool> Encoding for Utf32<FOREIGN> {
    type Unit = u32;

    const KIND: Kind = Kind::Utf32;
    const FOREIGN: bool = FOREIGN;

    #[inline]
    fn encoded_length(_cp: u32) -> usize {
        1
    }

    #[inline]
    fn decode(input: &mut &[u32]) -> u32 {
        let cp = input[0];
        *input = &input[1..];
        if FOREIGN { u32::swap_bytes(cp) } else { cp }
    }
}

#[cfg(test)]
mod tests {
    use super::{Encoding, Kind, Unit};
    use crate::{Utf8, Utf16, Utf32};

    #[test]
    fn utf8_round_trips_scalars() {
        for text in ["", "a", "caf\u{e9}", "\u{754c}", "\u{1f600}", "\u{10ffff}"] {
            let mut rest = text.as_bytes();
            let mut decoded = String::new();
            while !rest.is_empty() {
                let cp = Utf8::decode(&mut rest);
                decoded.push(char::from_u32(cp).expect("valid scalar"));
            }
            assert_eq!(decoded, text);
        }
    }

    #[test]
    fn utf8_truncated_tail_decodes_to_zero_and_consumes_the_rest() {
        let mut rest = b"\xe4\xb8".as_slice();
        assert_eq!(Utf8::decode(&mut rest), 0);
        assert_eq!(rest, [] as [u8; 0]);
    }

    #[test]
    fn encoded_length_matches_the_std_scalar_lengths() {
        for ch in [
            'a',
            '\u{7f}',
            '\u{80}',
            '\u{7ff}',
            '\u{800}',
            '\u{ffff}',
            '\u{1_0000}',
        ] {
            let cp = u32::from(ch);
            assert_eq!(Utf8::encoded_length(cp), ch.len_utf8(), "{ch:?}");
            assert_eq!(Utf16::<false>::encoded_length(cp), ch.len_utf16(), "{ch:?}");
            assert_eq!(Utf32::<false>::encoded_length(cp), 1, "{ch:?}");
        }
    }

    #[test]
    fn utf16_pairs_surrogates_and_passes_lone_halves_through() {
        let text: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        let mut rest = text.as_slice();
        assert_eq!(Utf16::<false>::decode(&mut rest), u32::from('a'));
        assert_eq!(Utf16::<false>::decode(&mut rest), 0x1_f600);
        assert_eq!(rest, [] as [u16; 0]);

        // A lone high surrogate keeps its own value rather than failing.
        let lone = [0xd800u16];
        let mut rest = lone.as_slice();
        assert_eq!(Utf16::<false>::decode(&mut rest), 0xd800);
    }

    #[test]
    fn foreign_endianness_reads_byte_swapped_units() {
        let native: Vec<u16> = "ab".encode_utf16().collect();
        let foreign: Vec<u16> = native.iter().copied().map(u16::swap_bytes).collect();
        let mut rest = foreign.as_slice();
        assert_eq!(Utf16::<true>::decode(&mut rest), u32::from('a'));
        assert_eq!(Utf16::<true>::decode(&mut rest), u32::from('b'));

        let swapped = [0x41u32.swap_bytes()];
        let mut rest = swapped.as_slice();
        assert_eq!(Utf32::<true>::decode(&mut rest), u32::from('A'));
    }

    #[test]
    fn kinds_pin_one_unit_type_per_family() {
        assert_eq!(Utf8::KIND, Kind::Utf8);
        assert_eq!(Utf16::<false>::KIND, Kind::Utf16);
        assert_eq!(Utf16::<true>::KIND, Kind::Utf16);
        assert_eq!(Utf32::<false>::KIND, Kind::Utf32);
    }

    #[test]
    fn unit_conversions_are_lossless_and_swap_only_when_wider() {
        assert_eq!(Unit::to_u32(0x7fu8), 0x7f);
        assert_eq!(Unit::to_u32(0x1234u16), 0x1234);
        assert_eq!(Unit::swap_bytes(0x7fu8), 0x7f);
        assert_eq!(Unit::swap_bytes(0x1234u16), 0x3412);
        assert_eq!(Unit::swap_bytes(0x1234_5678u32), 0x7856_3412);
    }
}
