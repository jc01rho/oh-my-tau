//! Pure stable-Rust stand-in for the subset of [`xutf`](https://crates.io/crates/xutf)
//! that the vendored omp2 stack consumes.
//!
//! The real crate needs nightly (`core::simd`) and ships a generated Unicode
//! property trie. This shim keeps the same public surface for the call sites in
//! `omp-shell` and `omp-shell-builtins` (strict UTF-8 char iteration over
//! `BufRead`, `width_char`, `width_str`, `width_ansi_str`) and in the vendored
//! `pi-*` file/edit/predict crates (line splitting, transcoded lengths,
//! codepoint offsets, byte-set scans, substring search and NFC normalization)
//! and in the vendored `omp-utok` tokenizer (general category, script and
//! canonical combining class lookups, NFC checks over codepoints), while
//! delegating table lookups to `unicode-width`, `unicode-properties`,
//! `unicode-script` and `unicode-normalization` and cluster segmentation to
//! `unicode-segmentation`.
//!
//! Compatibility notes:
//!
//! - [`Chars`] yields `io::Result<char>`, exactly like upstream, so
//!   `Option<io::Result<char>>::transpose` feeding `From<io::Error>` still
//!   works. The decoder's [`ReadCharError`] is preserved as the `io::Error`
//!   payload (`err.get_ref().and_then(<dyn Error>::downcast_ref)`) and is
//!   yielded directly by [`CharsRaw`].
//! - Widths follow UAX #11 via `unicode-width` plus emoji-presentation rules
//!   (VS16, ZWJ, keycaps), which is what the `ls` grid test measures.
//! - [`lines_str`] with [`LineEnds::Lf`] reproduces `split_inclusive('\n')`
//!   through [`Line::with_end`] and [`str::lines`] through [`Line::text`],
//!   which is the contract the diff, edit, predict and VCS call sites rely on.
//! - The codepoint walk is the permissive decoder's, so malformed input gets a
//!   total answer instead of an error, matching upstream.

#![forbid(unsafe_code)]

mod convert;
mod encoding;
mod find;
mod lines;
mod normalize;
mod offset;
mod read;
mod set;
mod strip;
mod text;
mod ucd;
mod width;

pub use convert::transcoded_len;
pub use encoding::{Encoding, Kind, Unit};
pub use find::{FindIter, find_iter};
pub use lines::{Line, LineEnds, Lines, StrLines, lines, lines_str};
pub use normalize::{IntoUnicodeNormalized, ToUnicodeNormalized, is_nfc, is_nfc_codepoints};
pub use offset::unit_offset;
pub use read::{BufReadCharsExt, Chars, CharsRaw, ReadCharError};
pub use set::UnitSet;
pub use strip::{strip_ansi, width_ansi_str};
pub use text::Text;
pub use ucd::{
    GeneralCategory, GeneralCategoryGroup, Script, UCD_VERSION, Ucd, canonical_combining_class,
};
pub use width::{width_char, width_str};

/// UTF-8 encoding marker.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Utf8;

/// UTF-16 encoding marker. `BE` selects byte order opposite to native, i.e. a
/// byte-swapped stream; this shim never byte-swaps unless asked.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Utf16<const BE: bool = false>;

/// UTF-32 encoding marker. `BE` selects byte order opposite to native, i.e. a
/// byte-swapped stream; this shim never byte-swaps unless asked.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Utf32<const BE: bool = false>;
