//! Unicode Character Database lookups: the general category, script and
//! canonical combining class surface the vendored `omp-utok` tokenizer
//! consumes.
//!
//! Tables come from `unicode-properties` and `unicode-script`, pinned to
//! UCD 16.0.0: the generation upstream xutf resolves by default and the one
//! behind the `regex-syntax` tables the tokenizer's reference engines use.
//! UCD 17.0.0 reclassifies thousands of scalars, which would move piece
//! boundaries and change token counts.

pub use unicode_properties::{GeneralCategory, GeneralCategoryGroup};
pub use unicode_script::Script;

/// UCD generation of the [`Ucd`] lookups.
pub const UCD_VERSION: (u64, u64, u64) = unicode_properties::UNICODE_VERSION;

/// Per-character property lookups.
pub trait Ucd {
    /// General category (`gc`).
    fn general_category(self) -> GeneralCategory;
    /// Major class of the general category (`L`, `M`, `N`, `P`, `S`, `Z`, `C`).
    fn general_category_group(self) -> GeneralCategoryGroup;
    /// Script property (`sc`).
    fn script(self) -> Script;
}

impl Ucd for char {
    fn general_category(self) -> GeneralCategory {
        unicode_properties::UnicodeGeneralCategory::general_category(self)
    }

    fn general_category_group(self) -> GeneralCategoryGroup {
        unicode_properties::UnicodeGeneralCategory::general_category_group(self)
    }

    fn script(self) -> Script {
        unicode_script::UnicodeScript::script(&self)
    }
}

/// Canonical combining class of the codepoint `cp`; 0 for values that are
/// not Unicode scalars.
#[must_use]
pub fn canonical_combining_class(cp: u32) -> u8 {
    char::from_u32(cp).map_or(0, unicode_normalization::char::canonical_combining_class)
}

#[cfg(test)]
mod tests {
    use super::{
        GeneralCategory, GeneralCategoryGroup, Script, UCD_VERSION, Ucd, canonical_combining_class,
    };

    #[test]
    fn tables_resolve_against_ucd_16() {
        assert_eq!(UCD_VERSION, (16, 0, 0));
        assert_eq!(unicode_script::UNICODE_VERSION, UCD_VERSION);
    }

    #[test]
    fn classifies_letters_marks_and_scripts() {
        assert_eq!('A'.general_category(), GeneralCategory::UppercaseLetter);
        assert_eq!(
            '\u{301}'.general_category_group(),
            GeneralCategoryGroup::Mark
        );
        assert_eq!('\u{754c}'.script(), Script::Han);
        assert_eq!('a'.script(), Script::Latin);
    }

    #[test]
    fn combining_class_of_marks_and_non_scalars() {
        assert_eq!(canonical_combining_class(0x301), 230);
        assert_eq!(canonical_combining_class(0x94d), 9);
        assert_eq!(canonical_combining_class(u32::from('a')), 0);
        assert_eq!(canonical_combining_class(0xd800), 0);
    }
}
