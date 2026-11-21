//! Unicode normalization: the NFC surface the edit engine consumes.
//!
//! `into_nfc` reuses the string's own allocation when it is already normal,
//! which is the common case for source text. The table lookups delegate to
//! `unicode-normalization`.

use unicode_normalization::UnicodeNormalization as _;

/// Converts an owned string to NFC while reusing its allocation.
///
/// # Example
///
/// ```
/// use xutf::IntoUnicodeNormalized;
///
/// // `e` + combining acute composes into the single scalar `é`.
/// assert_eq!("cafe\u{301}".to_owned().into_nfc(), "caf\u{e9}");
/// assert_eq!("caf\u{e9}".to_owned().into_nfc(), "caf\u{e9}");
/// ```
pub trait IntoUnicodeNormalized {
    /// Returns this string in NFC, growing its allocation only when required.
    fn into_nfc(self) -> String;
}

impl IntoUnicodeNormalized for String {
    fn into_nfc(self) -> String {
        if is_nfc(&self) {
            return self;
        }
        self.chars().nfc().collect()
    }
}

/// Whether `text` is already in NFC, so normalizing it is a no-op.
///
/// # Example
///
/// ```
/// use xutf::is_nfc;
///
/// assert!(is_nfc("caf\u{e9}"));
/// assert!(!is_nfc("cafe\u{301}"));
/// ```
#[must_use]
pub fn is_nfc(text: &str) -> bool {
    unicode_normalization::is_nfc(text)
}

#[cfg(test)]
mod tests {
    use super::{IntoUnicodeNormalized, is_nfc};

    #[test]
    fn composes_combining_sequences() {
        assert_eq!("cafe\u{301}".to_owned().into_nfc(), "caf\u{e9}");
        assert_eq!("A\u{30a}".to_owned().into_nfc(), "\u{c5}");
    }

    #[test]
    fn leaves_already_nfc_text_unchanged() {
        for text in ["", "ascii", "caf\u{e9}", "\u{754c}", "\u{1f600}"] {
            assert!(is_nfc(text), "{text:?}");
            assert_eq!(text.to_owned().into_nfc(), text);
        }
    }

    #[test]
    fn reuses_the_allocation_when_already_normal() {
        let text = String::from("caf\u{e9}");
        let before = text.as_ptr();
        let normalized = text.into_nfc();
        assert_eq!(normalized, "caf\u{e9}");
        // Already-NFC input must not be reallocated.
        assert_eq!(normalized.as_ptr(), before);
    }

    #[test]
    fn is_nfc_agrees_with_normalizing() {
        for text in ["cafe\u{301}", "A\u{30a}", "\u{212b}"] {
            assert_eq!(is_nfc(text), text.to_owned().into_nfc() == text, "{text:?}");
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        for text in ["cafe\u{301}", "A\u{30a}", "\u{212b}\u{301}"] {
            let once = text.to_owned().into_nfc();
            let twice = once.clone().into_nfc();
            assert_eq!(once, twice, "{text:?}");
        }
    }
}
