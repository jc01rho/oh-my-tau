//! Structured model-family classification for model ids: replaces substring probes such as
//! `model_id.contains("opus-4-6")` with a parsed `(family, tier, revision)` and ordered
//! comparisons. Ids are matched case-insensitively and may carry namespace / region prefixes
//! (`anthropic/`, `us.anthropic.`) and date or `-v1:0` suffixes.

/// A `major[.minor]` model revision; a missing minor sorts before `.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Revision {
    pub major: u32,
    pub minor: Option<u32>,
}

impl Revision {
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self {
            major,
            minor: Some(minor),
        }
    }

    /// Whether this revision is `>= major.minor` (a missing minor counts as `.0`).
    #[must_use]
    pub fn at_least(self, major: u32, minor: u32) -> bool {
        (self.major, self.minor.unwrap_or(0)) >= (major, minor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeTier {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiVariant {
    Pro,
    Flash,
    FlashLite,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFamily {
    /// `revision` is `None` for legacy `claude-3-5-sonnet` ordering and for `mythos-preview`
    /// (`preview` set).
    Claude {
        tier: ClaudeTier,
        revision: Option<Revision>,
        preview: bool,
    },
    Gemini {
        revision: Revision,
        variant: GeminiVariant,
    },
    Gemma {
        revision: Revision,
    },
    /// `gpt-5.6-mini` -> revision 5.6, variant `Some("mini")`; `gpt-oss-*` is not a `Gpt`.
    Gpt {
        revision: Revision,
        variant: Option<String>,
    },
    Other,
}

/// Parse `major[(-|.)minor]` at the start of `s`; a date-like minor (`-20250514`) is not a minor.
fn parse_revision(s: &str) -> Option<(Revision, &str)> {
    let major_len = s.bytes().take_while(u8::is_ascii_digit).count();
    let major = s[..major_len].parse().ok()?;
    let rest = &s[major_len..];
    if let Some(after) = rest.strip_prefix(['-', '.']) {
        let minor_len = after.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=3).contains(&minor_len) {
            let minor = after[..minor_len].parse().ok()?;
            return Some((
                Revision {
                    major,
                    minor: Some(minor),
                },
                &after[minor_len..],
            ));
        }
    }
    Some((Revision { major, minor: None }, rest))
}

fn classify_claude(id: &str) -> Option<ModelFamily> {
    const TIERS: [(&str, ClaudeTier); 5] = [
        ("opus", ClaudeTier::Opus),
        ("sonnet", ClaudeTier::Sonnet),
        ("haiku", ClaudeTier::Haiku),
        ("fable", ClaudeTier::Fable),
        ("mythos", ClaudeTier::Mythos),
    ];
    for (name, tier) in TIERS {
        for (at, _) in id.match_indices(name) {
            let rest = &id[at + name.len()..];
            let Some(rest) = rest.strip_prefix(['-', '.']) else {
                continue;
            };
            if rest.starts_with("preview") {
                return Some(ModelFamily::Claude {
                    tier,
                    revision: None,
                    preview: true,
                });
            }
            if let Some((revision, _)) = parse_revision(rest) {
                return Some(ModelFamily::Claude {
                    tier,
                    revision: Some(revision),
                    preview: false,
                });
            }
        }
        if id.contains(name) && id.contains("claude") {
            return Some(ModelFamily::Claude {
                tier,
                revision: None,
                preview: false,
            });
        }
    }
    None
}

fn classify_gemini(id: &str) -> Option<ModelFamily> {
    let at = id.find("gemini-")?;
    let (revision, rest) = parse_revision(&id[at + "gemini-".len()..])?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let variant = if rest.starts_with("pro") {
        GeminiVariant::Pro
    } else if rest.starts_with("flash-lite") {
        GeminiVariant::FlashLite
    } else if rest.starts_with("flash") {
        GeminiVariant::Flash
    } else {
        GeminiVariant::Other
    };
    Some(ModelFamily::Gemini { revision, variant })
}

fn classify_gemma(id: &str) -> Option<ModelFamily> {
    let at = id.find("gemma")?;
    let rest = &id[at + "gemma".len()..];
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let (revision, _) = parse_revision(rest)?;
    Some(ModelFamily::Gemma { revision })
}

fn classify_gpt(id: &str) -> Option<ModelFamily> {
    let rest = id.strip_prefix("gpt-")?;
    let (revision, rest) = parse_revision(rest)?;
    let variant = rest.strip_prefix('-').map(str::to_owned);
    Some(ModelFamily::Gpt { revision, variant })
}

/// Classify a model id (or display name) into its family.
#[must_use]
pub fn classify(model_id: &str) -> ModelFamily {
    let id = model_id.to_ascii_lowercase();
    classify_claude(&id)
        .or_else(|| classify_gemini(&id))
        .or_else(|| classify_gemma(&id))
        .or_else(|| classify_gpt(&id))
        .unwrap_or(ModelFamily::Other)
}

impl ModelFamily {
    /// Claude adaptive thinking: Opus/Sonnet 4.6+, Fable/Mythos 5+ and Mythos preview.
    #[must_use]
    pub fn supports_adaptive_thinking(&self) -> bool {
        match *self {
            Self::Claude { preview: true, .. } => true,
            Self::Claude {
                tier: ClaudeTier::Opus | ClaudeTier::Sonnet,
                revision: Some(r),
                ..
            } => r.at_least(4, 6),
            Self::Claude {
                tier: ClaudeTier::Fable | ClaudeTier::Mythos,
                revision: Some(r),
                ..
            } => r.at_least(5, 0),
            _ => false,
        }
    }

    /// Claude models that think every turn and reject sampling params: Fable/Mythos and Opus 5.5+.
    #[must_use]
    pub fn supports_always_on_adaptive_thinking(&self) -> bool {
        match *self {
            Self::Claude { preview: true, .. } => true,
            Self::Claude {
                tier: ClaudeTier::Fable | ClaudeTier::Mythos,
                revision: Some(r),
                ..
            } => r.at_least(5, 0),
            Self::Claude {
                tier: ClaudeTier::Opus,
                revision: Some(r),
                ..
            } => r.at_least(5, 5),
            _ => false,
        }
    }

    #[must_use]
    pub fn is_gemma(&self, major: u32) -> bool {
        matches!(self, Self::Gemma { revision } if revision.major == major)
    }

    /// OpenAI service-tier (flex/priority) eligible ids: gpt-5.4, gpt-5.5, gpt-5.6[-*], gpt-6-astra.
    #[must_use]
    pub fn is_service_tier_eligible(&self) -> bool {
        let Self::Gpt { revision, variant } = self else {
            return false;
        };
        match (revision.major, revision.minor, variant.as_deref()) {
            (5, Some(4 | 5), None) | (5, Some(6), _) | (6, None, Some("astra")) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adaptive(id: &str) -> bool {
        classify(id).supports_adaptive_thinking()
    }

    #[test]
    fn parses_claude_ids_across_naming_schemes() {
        assert_eq!(
            classify("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            ModelFamily::Claude {
                tier: ClaudeTier::Sonnet,
                revision: Some(Revision::new(4, 5)),
                preview: false
            }
        );
        assert_eq!(
            classify("claude-sonnet-4-20250514"),
            ModelFamily::Claude {
                tier: ClaudeTier::Sonnet,
                revision: Some(Revision {
                    major: 4,
                    minor: None
                }),
                preview: false
            }
        );
        assert_eq!(
            classify("claude-3-5-sonnet-latest"),
            ModelFamily::Claude {
                tier: ClaudeTier::Sonnet,
                revision: None,
                preview: false
            }
        );
        assert!(matches!(
            classify("anthropic/claude-opus-4.6"),
            ModelFamily::Claude {
                tier: ClaudeTier::Opus,
                revision: Some(Revision {
                    major: 4,
                    minor: Some(6)
                }),
                ..
            }
        ));
    }

    #[test]
    fn adaptive_thinking_matches_the_former_substring_list() {
        for id in [
            "claude-opus-4-6",
            "claude-opus-4.6",
            "claude-opus-4-7",
            "claude-opus-4.7",
            "claude-opus-4-8",
            "claude-opus-4.8",
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4.6",
            "claude-sonnet-5",
            "claude-fable-5",
            "eu.anthropic.claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "~anthropic/claude-sonnet-5-5",
        ] {
            assert!(adaptive(id), "{id}");
        }
        for id in [
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5",
            "claude-sonnet-4-20250514",
            "claude-haiku-4-5-20251001",
            "claude-3-7-sonnet-latest",
            "gpt-5.5",
            "gemini-2.5-pro",
        ] {
            assert!(!adaptive(id), "{id}");
        }
    }

    #[test]
    fn always_on_adaptive_thinking() {
        for id in [
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "claude-opus-5-5",
            "claude-opus-5.5",
        ] {
            assert!(classify(id).supports_always_on_adaptive_thinking(), "{id}");
        }
        for id in ["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5"] {
            assert!(!classify(id).supports_always_on_adaptive_thinking(), "{id}");
        }
    }

    #[test]
    fn gemini_and_gemma() {
        assert_eq!(
            classify("gemini-2.5-flash-lite-preview-06-17"),
            ModelFamily::Gemini {
                revision: Revision::new(2, 5),
                variant: GeminiVariant::FlashLite
            }
        );
        assert_eq!(
            classify("models/gemini-2.5-pro"),
            ModelFamily::Gemini {
                revision: Revision::new(2, 5),
                variant: GeminiVariant::Pro
            }
        );
        assert!(matches!(
            classify("gemini-3-flash-preview"),
            ModelFamily::Gemini {
                variant: GeminiVariant::Flash,
                revision: Revision {
                    major: 3,
                    minor: None
                }
            }
        ));
        assert!(classify("google/gemma-4-31b-it").is_gemma(4));
        assert!(classify("Gemma4-E4B").is_gemma(4));
        assert!(!classify("gemma-3-27b-it").is_gemma(4));
    }

    #[test]
    fn service_tier_eligibility() {
        for id in [
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.6-luna",
            "gpt-6-astra",
        ] {
            assert!(classify(id).is_service_tier_eligible(), "{id}");
        }
        for id in ["gpt-5.4-mini", "gpt-5", "gpt-6", "gpt-oss-120b", "o3"] {
            assert!(!classify(id).is_service_tier_eligible(), "{id}");
        }
    }
}
