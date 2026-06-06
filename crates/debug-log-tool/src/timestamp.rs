//! Hand-rolled parser for the two timestamp shapes Bitcoin Core emits in
//! `debug.log`:
//!
//! - `2026-05-15T00:06:31.149660Z` — RFC3339-ish with fractional seconds and a
//!   trailing `Z`.
//! - `2026-05-15T00:06:31` — whole seconds only, no suffix.
//!
//! Both must compare correctly against each other for the merge to be stable,
//! which is why we parse to a struct of integer fields and derive `Ord` over
//! it rather than comparing strings.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimestampKey {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub nanos: u32,
}

/// Parse a leading timestamp from `s`. Returns `None` if the prefix doesn't
/// match either supported shape. Anything after the timestamp is ignored, so
/// this is safe to call on a whole log line.
pub fn parse_timestamp(s: &str) -> Option<TimestampKey> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }

    let year = parse_int::<u16>(&b[0..4])?;
    let month = parse_int::<u8>(&b[5..7])?;
    let day = parse_int::<u8>(&b[8..10])?;
    let hour = parse_int::<u8>(&b[11..13])?;
    let minute = parse_int::<u8>(&b[14..16])?;
    let second = parse_int::<u8>(&b[17..19])?;

    let nanos = if b.len() > 19 && b[19] == b'.' {
        let mut end = 20;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        let digits = &b[20..end];
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        let mut n: u32 = 0;
        for &c in digits {
            n = n * 10 + (c - b'0') as u32;
        }
        n * 10u32.pow(9 - digits.len() as u32)
    } else {
        0
    };

    Some(TimestampKey {
        year,
        month,
        day,
        hour,
        minute,
        second,
        nanos,
    })
}

trait FromAsciiDigits: Sized {
    fn from_digits(digits: &[u8]) -> Option<Self>;
}

macro_rules! impl_from_ascii_digits {
    ($($t:ty),*) => {$(
        impl FromAsciiDigits for $t {
            fn from_digits(digits: &[u8]) -> Option<Self> {
                let mut n: Self = 0;
                for &c in digits {
                    if !c.is_ascii_digit() { return None; }
                    n = n.checked_mul(10)?.checked_add((c - b'0') as Self)?;
                }
                Some(n)
            }
        }
    )*};
}
impl_from_ascii_digits!(u8, u16);

fn parse_int<T: FromAsciiDigits>(digits: &[u8]) -> Option<T> {
    T::from_digits(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_only() {
        let k = parse_timestamp("2026-05-15T00:06:31 hello").unwrap();
        assert_eq!(k.year, 2026);
        assert_eq!(k.second, 31);
        assert_eq!(k.nanos, 0);
    }

    #[test]
    fn parses_with_fractional_and_z() {
        let k = parse_timestamp("2026-05-15T00:06:31.149660Z hello").unwrap();
        assert_eq!(k.second, 31);
        assert_eq!(k.nanos, 149_660_000);
    }

    #[test]
    fn fractional_compares_against_whole_seconds() {
        let whole = parse_timestamp("2026-05-15T00:06:31").unwrap();
        let frac = parse_timestamp("2026-05-15T00:06:31.000001Z").unwrap();
        assert!(whole < frac);

        let earlier_whole = parse_timestamp("2026-05-15T00:06:30").unwrap();
        let later_frac = parse_timestamp("2026-05-15T00:06:31.000000Z").unwrap();
        assert!(earlier_whole < later_frac);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_timestamp("").is_none());
        assert!(parse_timestamp("not a timestamp").is_none());
        assert!(parse_timestamp("2026/05/15T00:06:31").is_none());
        assert!(parse_timestamp("2026-05-15T00:06:31.").is_none());
    }
}
