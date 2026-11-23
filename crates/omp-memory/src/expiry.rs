//! `expires` frontmatter parsing and pinned-session expiry notice helpers.

use crate::block::Block;

pub const MEMORY_EXPIRED_METADATA_TOKEN: &str = "Expired since this prompt was pinned";
const MAX_LISTED_PATHS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryExpiry {
    pub path: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryWindow {
    pub pinned_at_ms: i64,
    pub noticed_through_ms: i64,
    pub now_ms: i64,
}

#[must_use]
pub fn memory_expires_at_ms(block: &Block) -> Option<i64> {
    let raw = block.extra("expires")?;
    parse_expiry_instant(raw.trim())
}

#[must_use]
pub fn is_memory_expired(expires_at_ms: Option<i64>, as_of_ms: i64) -> bool {
    expires_at_ms.is_some_and(|at| as_of_ms >= at)
}

#[must_use]
pub fn newly_expired(expiries: &[MemoryExpiry], window: ExpiryWindow) -> Vec<MemoryExpiry> {
    let after = window.pinned_at_ms.max(window.noticed_through_ms);
    expiries
        .iter()
        .filter(|expiry| expiry.expires_at_ms > after && expiry.expires_at_ms <= window.now_ms)
        .cloned()
        .collect()
}

#[must_use]
pub fn render_expired_line(expired: &[MemoryExpiry]) -> String {
    let hidden = expired.len().saturating_sub(MAX_LISTED_PATHS);
    let listed = expired
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(|expiry| expiry.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let more = if hidden > 0 {
        format!(", {hidden} more")
    } else {
        String::new()
    };
    format!("- {MEMORY_EXPIRED_METADATA_TOKEN} (no longer true): {listed}{more}.")
}

fn digits(bytes: &[u8], start: usize, count: usize) -> Option<i64> {
    let slice = bytes.get(start..start + count)?;
    slice.iter().all(u8::is_ascii_digit).then(|| {
        slice
            .iter()
            .fold(0, |sum, b| sum * 10 + i64::from(b - b'0'))
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[must_use]
pub fn parse_expiry_instant(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (
        digits(bytes, 0, 4)?,
        digits(bytes, 5, 2)?,
        digits(bytes, 8, 2)?,
    );
    let day_ms = days_from_civil(year, month, day)? * 86_400_000;
    if bytes.len() == 10 {
        return Some(day_ms);
    }
    if bytes[10] != b'T' || bytes.get(13) != Some(&b':') {
        return None;
    }
    let hour = digits(bytes, 11, 2).filter(|h| *h <= 23)?;
    let minute = digits(bytes, 14, 2).filter(|m| *m <= 59)?;
    let mut at = 16;
    let (mut second, mut millis) = (0, 0);
    if bytes.get(at) == Some(&b':') {
        second = digits(bytes, at + 1, 2).filter(|s| *s <= 59)?;
        at += 3;
        if bytes.get(at) == Some(&b'.') {
            let fraction = bytes[at + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if !(1..=9).contains(&fraction) {
                return None;
            }
            millis = (0..3).fold(0, |sum, i| {
                sum * 10
                    + bytes
                        .get(at + 1 + i)
                        .filter(|_| i < fraction)
                        .map_or(0, |b| i64::from(b - b'0'))
            });
            at += 1 + fraction;
        }
    }
    let offset_ms = match &bytes[at..] {
        [b'Z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let offset_hour = digits(rest, 0, 2).filter(|h| *h <= 23)?;
            let offset_minute = digits(rest, 3, 2).filter(|m| *m <= 59)?;
            let magnitude = (offset_hour * 60 + offset_minute) * 60_000;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };
    Some(day_ms + ((hour * 60 + minute) * 60 + second) * 1000 + millis - offset_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates_and_offsets_and_rejects_naive_datetimes() {
        let block = Block::parse("---\ndescription: x\nexpires: 2026-10-27T06:33Z\n---\nb").unwrap();
        let expected = 20_753 * 86_400_000 + (6 * 60 + 33) * 60_000;
        assert_eq!(memory_expires_at_ms(&block), Some(expected));
        assert!(is_memory_expired(Some(expected), expected));
        assert!(!is_memory_expired(Some(expected), expected - 1));

        assert_eq!(
            parse_expiry_instant("2026-10-27T15:33:00+09:00"),
            Some(expected)
        );
        assert_eq!(parse_expiry_instant("2026-10-27T06:33"), None);
        assert_eq!(parse_expiry_instant("2026-02-30"), None);
        assert_eq!(parse_expiry_instant("never"), None);
    }

    #[test]
    fn newly_expired_respects_watermark_and_renders_line() {
        let expiries = [
            MemoryExpiry {
                path: "system/a.md".into(),
                expires_at_ms: 5,
            },
            MemoryExpiry {
                path: "system/b.md".into(),
                expires_at_ms: 15,
            },
            MemoryExpiry {
                path: "system/c.md".into(),
                expires_at_ms: 25,
            },
            MemoryExpiry {
                path: "system/d.md".into(),
                expires_at_ms: 40,
            },
        ];
        let got = newly_expired(
            &expiries,
            ExpiryWindow {
                pinned_at_ms: 10,
                noticed_through_ms: 20,
                now_ms: 30,
            },
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "system/c.md");
        assert_eq!(
            render_expired_line(&got),
            "- Expired since this prompt was pinned (no longer true): system/c.md."
        );
    }
}
