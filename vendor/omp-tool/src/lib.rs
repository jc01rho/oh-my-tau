//! Tool-facing harness protocol data.
//!
//! A [`Diag`] is the structured notice a tool attaches to its call instead of
//! appending prose to its output. Native tools and shell builtins both speak
//! this shape, so it lives in one small crate they can share without pulling
//! in a tool runtime.

mod diag;

pub use diag::{Diag, DiagEnvelope, DiagKind, Omitted, Severity, Unit, UnknownVariant};
