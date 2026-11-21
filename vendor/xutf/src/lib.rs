//! Pure stable-Rust stand-in for the subset of [`xutf`](https://crates.io/crates/xutf)
//! that the vendored omp2 shell stack consumes.
//!
//! The real crate needs nightly (`core::simd`) and ships a generated Unicode
//! property trie. This shim keeps the same public surface for the call sites in
//! `omp-shell` (strict UTF-8 char iteration over `BufRead`) and
//! `omp-shell-builtins` (`width_char`, `width_str`, `width_ansi_str`) while
//! delegating table lookups to `unicode-width` and cluster segmentation to
//! `unicode-segmentation`.
//!
//! Compatibility notes:
//!
//! - [`Chars`] yields [`io::Result<char>`], exactly like upstream, so
//!   `Option<io::Result<char>>::transpose` feeding `From<io::Error>` still
//!   works. The decoder's [`ReadCharError`] is preserved as the `io::Error`
//!   payload (`err.get_ref().and_then(<dyn Error>::downcast_ref)`) and is
//!   yielded directly by [`CharsRaw`].
//! - Widths follow UAX #11 via `unicode-width` plus emoji-presentation rules
//!   (VS16, ZWJ, keycaps), which is what the `ls` grid test measures.

#![forbid(unsafe_code)]

mod read;
mod strip;
mod width;

pub use read::{BufReadCharsExt, Chars, CharsRaw, ReadCharError};
pub use strip::{strip_ansi, width_ansi_str};
pub use width::{width_char, width_str};

/// UTF-8 encoding marker (the only encoding this shim decodes).
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Utf8;

/// UTF-16 encoding marker, retained for signature compatibility. `BE` selects
/// big-endian byte order; this shim decodes neither order.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Utf16<const BE: bool = false>;
