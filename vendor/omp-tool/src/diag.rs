//! Structured harness notices attached to a tool call.
//!
//! A [`Diag`] is protocol data on the call element: the fold materializes it
//! as a `<diag>` child, the model projection renders every diag as one
//! uniform trailing part, and cards present it beside the result. Tools yield
//! a diag instead of appending prose to their output; extension tools emit the
//! same JSON envelope (`{"diag": {…}}`) as an update.

use core::{fmt, str::FromStr};

use omp_core::Str;
use serde::{Deserialize, Serialize};

/// Returned when a string does not name a variant of the closed vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownVariant(&'static str);

impl fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown ")?;
        f.write_str(self.0)?;
        f.write_str(" name")
    }
}

impl std::error::Error for UnknownVariant {}

/// Generates the wire surface of a closed-vocabulary enum: the canonical name,
/// `Display`, `From<Enum> for &'static str`, and case-insensitive parsing.
macro_rules! wire_names {
	(
		$ty:ident,
		$what:literal,
		{
			$(
				$variant:ident => $name:literal $(| $alias:literal)*
			),+ $(,)?
		}
	) => {
		impl $ty {
			/// Canonical wire name of this variant.
			#[must_use]
			pub const fn as_str(self) -> &'static str {
				match self {
					$(Self::$variant => $name),+
				}
			}
		}

		impl fmt::Display for $ty {
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				f.write_str(self.as_str())
			}
		}

		impl From<$ty> for &'static str {
			fn from(value: $ty) -> Self {
				value.as_str()
			}
		}

		impl FromStr for $ty {
			type Err = UnknownVariant;

			fn from_str(input: &str) -> Result<Self, Self::Err> {
				$(
					$(
						if input.eq_ignore_ascii_case($alias) {
							return Ok(Self::$variant);
						}
					)*
					if input.eq_ignore_ascii_case($name) {
						return Ok(Self::$variant);
					}
				)+
				Err(UnknownVariant($what))
			}
		}
	};
}

/// Severity of a harness notice.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Context the model may use; no action needed.
    #[default]
    Info,
    /// The result is complete but degraded or surprising; the model should
    /// adapt.
    #[serde(alias = "warning")]
    Warn,
    /// The call faulted; carried by the terminal diag only.
    Error,
}

wire_names!(Severity, "severity", {
    Info => "info",
    Warn => "warn" | "warning",
    Error => "error",
});

/// Closed vocabulary of native harness notice kinds.
///
/// Extension tools may emit other kinds; the DOM stores the string. Native
/// tools MUST pick a variant so the model learns one stable vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagKind {
    /// Output exceeded the inline bound; the full result lives at `artifact`.
    OutputBounded,
    /// The result is one page of a larger set; `continuation` fetches the next.
    Pagination,
    /// A requested range or offset lies beyond the end of the resource.
    RangeOutOfBounds,
    /// A structural summary elided bodies; `continuation` names the ranges to
    /// re-read.
    SummaryElided,
    /// A fixed result cap was hit; `omitted` counts what was dropped.
    LimitReached,
    /// Work stopped at a deadline with partial results.
    Timeout,
    /// Requested paths did not exist and were skipped.
    MissingPaths,
    /// Inputs were skipped because the tool cannot process them.
    Skipped,
    /// Only a prefix of the input was examined.
    PartialScan,
    /// The authored path did not exist; the tool resolved a different one.
    PathRecovered,
    /// The resource holds unresolved merge conflicts.
    Conflicts,
    /// Where or how the content was obtained.
    Provenance,
    /// A degraded fallback source or renderer produced the content.
    Fallback,
    /// Fetching the resource failed; the result is a substitute.
    FetchFailed,
    /// Cached content was served because a live refresh failed.
    StaleCache,
    /// The tool rewrote the supplied content before applying it.
    ContentNormalized,
    /// Execute bits were added because the content starts with a shebang.
    MadeExecutable,
    /// An edit broke parsing and the harness repaired it automatically.
    SyntaxRepaired,
    /// An edit left the document unparseable.
    SyntaxBroken,
    /// Anchors drifted from the authored text and were remapped.
    AnchorDrift,
    /// Non-blocking advice from a structural tool.
    Advisory,
    /// A file failed to parse in its language.
    ParseIssue,
    /// A recovery snapshot was recorded for an applied change.
    Snapshot,
    /// The upstream provider attached a warning to its response.
    ProviderWarning,
    /// The host applied a sandbox policy to the command.
    Sandbox,
}

wire_names!(DiagKind, "diag kind", {
    OutputBounded => "output_bounded",
    Pagination => "pagination",
    RangeOutOfBounds => "range_out_of_bounds",
    SummaryElided => "summary_elided",
    LimitReached => "limit_reached",
    Timeout => "timeout",
    MissingPaths => "missing_paths",
    Skipped => "skipped",
    PartialScan => "partial_scan",
    PathRecovered => "path_recovered",
    Conflicts => "conflicts",
    Provenance => "provenance",
    Fallback => "fallback",
    FetchFailed => "fetch_failed",
    StaleCache => "stale_cache",
    ContentNormalized => "content_normalized",
    MadeExecutable => "made_executable",
    SyntaxRepaired => "syntax_repaired",
    SyntaxBroken => "syntax_broken",
    AnchorDrift => "anchor_drift",
    Advisory => "advisory",
    ParseIssue => "parse_issue",
    Snapshot => "snapshot",
    ProviderWarning => "provider_warning",
    Sandbox => "sandbox",
});

/// Unit of elided content reported by [`Diag::omitted`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Text lines.
    Lines,
    /// Table rows.
    Rows,
    /// Listing entries.
    Entries,
    /// Files.
    Files,
    /// Bytes.
    Bytes,
    /// Characters.
    Chars,
    /// Generic items.
    Items,
}

wire_names!(Unit, "unit", {
    Lines => "lines",
    Rows => "rows",
    Entries => "entries",
    Files => "files",
    Bytes => "bytes",
    Chars => "chars",
    Items => "items",
});

/// Count of content elided from the inline result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Omitted {
    /// Number of units not shown.
    pub count: u64,
    /// What was counted.
    pub unit: Unit,
}

/// One harness notice attached to a tool call.
///
/// `text` is the human/model sentence; the optional fields are the typed
/// facts a consumer acts on without parsing prose. Native tools construct
/// through [`Diag::info`]/[`Diag::warn`] and the builder methods.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diag {
    /// Notice severity.
    #[serde(default)]
    pub severity: Severity,
    /// Stable machine-readable kind; native tools use [`DiagKind`].
    #[serde(default = "default_kind")]
    pub kind: Str,
    /// Human-readable sentence.
    #[serde(default, alias = "message")]
    pub text: Str,
    /// Selector or argument that fetches the next slice of a bounded result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Str>,
    /// Full-result recovery address (`artifact://…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Str>,
    /// Content elided from the inline result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Omitted>,
}

const fn default_kind() -> Str {
    Str::new_static("notice")
}

impl Default for Diag {
    /// An informational `notice` with no text; the lenient decode baseline
    /// for extension-authored diags.
    fn default() -> Self {
        Self {
            severity: Severity::Info,
            kind: default_kind(),
            text: Str::new_static(""),
            continuation: None,
            artifact: None,
            omitted: None,
        }
    }
}

impl Diag {
    /// Creates a notice with the given severity.
    #[must_use]
    pub fn new(severity: Severity, kind: DiagKind, text: impl Into<Str>) -> Self {
        Self {
            severity,
            kind: Str::new_static(kind.into()),
            text: text.into(),
            continuation: None,
            artifact: None,
            omitted: None,
        }
    }

    /// Creates an informational notice.
    #[must_use]
    pub fn info(kind: DiagKind, text: impl Into<Str>) -> Self {
        Self::new(Severity::Info, kind, text)
    }

    /// Creates a warning.
    #[must_use]
    pub fn warn(kind: DiagKind, text: impl Into<Str>) -> Self {
        Self::new(Severity::Warn, kind, text)
    }

    /// Attaches the selector or argument that fetches the next slice.
    #[must_use]
    pub fn continuation(mut self, continuation: impl Into<Str>) -> Self {
        self.continuation = Some(continuation.into());
        self
    }

    /// Attaches the full-result recovery address.
    #[must_use]
    pub fn artifact(mut self, artifact: impl Into<Str>) -> Self {
        self.artifact = Some(artifact.into());
        self
    }

    /// Records how much content the inline result elides.
    #[must_use]
    pub const fn omitted(mut self, count: u64, unit: Unit) -> Self {
        self.omitted = Some(Omitted { count, unit });
        self
    }

    /// Parsed native kind, when the diag uses the closed vocabulary.
    #[must_use]
    pub fn native_kind(&self) -> Option<DiagKind> {
        self.kind.as_str().parse().ok()
    }
}

/// Update envelope carrying one diag: `{"diag": {…}}`.
///
/// This is the wire shape native erasure and extension tools both emit, and
/// the shape the session fold recognizes.
#[derive(Debug, Deserialize, Serialize)]
pub struct DiagEnvelope<D = Diag> {
    /// The notice.
    pub diag: D,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_extension_envelope_decodes_with_defaults() {
        let json = r#"{"diag":{"kind":"annotation","message":"hi"}}"#;
        let envelope: DiagEnvelope = serde_json::from_str(json).expect("decodes");
        assert_eq!(envelope.diag.severity, Severity::Info);
        assert_eq!(envelope.diag.kind.as_str(), "annotation");
        assert_eq!(envelope.diag.text.as_str(), "hi");
        assert_eq!(envelope.diag.native_kind(), None);
    }

    #[test]
    fn native_diag_round_trips_typed_facts() {
        let diag = Diag::info(DiagKind::Pagination, "45 more lines")
            .continuation(":16")
            .omitted(45, Unit::Lines);
        let json = serde_json::to_string(&DiagEnvelope { diag: &diag }).expect("encodes");
        let decoded: DiagEnvelope = serde_json::from_str(&json).expect("decodes");
        assert_eq!(decoded.diag, diag);
        assert_eq!(decoded.diag.native_kind(), Some(DiagKind::Pagination));
        assert!(!json.contains("artifact"));
    }

    #[test]
    fn warning_alias_parses_as_warn() {
        let diag: Diag = serde_json::from_str(r#"{"kind":"x","severity":"warning"}"#).expect("ok");
        assert_eq!(diag.severity, Severity::Warn);
    }
}
