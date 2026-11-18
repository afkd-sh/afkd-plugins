//! Dependency-free RFC-3339 / ISO-8601 decoding: the reader a card's `created_at` and
//! `updated_at` and a comment's `created_at` go through, to unix milliseconds, the way afkd
//! reads a handle's `time` field.
//!
//! The reader is afkd's own (`afkd_forge::rfc3339`), as `@afkd/linear` carries it. The board
//! writes every instant in UTC to the second, `2026-11-14T09:30:00Z`, and the reader takes
//! the other forms too.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The instant `s` names, in unix milliseconds, or `None` for one [`parse_rfc3339`] cannot
/// read.
pub(crate) fn unix_millis(s: &str) -> Option<u64> {
    let at = parse_rfc3339(s)?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(at.as_millis()).ok()
}

/// Parse an RFC-3339 / ISO-8601 timestamp into a [`SystemTime`], dependency-free.
///
/// Handles the forms a tracker or forge emits: `2026-06-27T12:34:56Z`, a numeric offset
/// `…+02:00`/`…-05:00`, and fractional seconds `…12:34:56.789Z`. Returns `None`
/// for anything it cannot read, so a malformed timestamp degrades to a default
/// rather than panicking. Only times at or after the Unix epoch are represented.
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    // Split date and time on the `T` (tolerate a space separator too).
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // The time runs up to the timezone designator (`Z`, `+`, or `-`). A bare time
    // with no designator is read as UTC.
    let (time, offset_secs) = split_offset(rest)?;
    let mut t = time.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let sec_field = t.next().unwrap_or("0");
    // Drop any fractional part — second granularity is all the watermark needs.
    let second: i64 = sec_field.split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    if total < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(total as u64))
}

/// Split a time string from its trailing timezone designator, returning the bare
/// `HH:MM:SS[.fff]` and the offset in seconds (positive east of UTC). A `Z` or an
/// absent designator is zero offset.
fn split_offset(time_and_tz: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = time_and_tz
        .strip_suffix('Z')
        .or_else(|| time_and_tz.strip_suffix('z'))
    {
        return Some((stripped, 0));
    }
    // Scan from the seconds onward for a `+`/`-` introducing the offset (the
    // date's hyphens are already consumed, so any `-` here is the tz sign).
    for (i, c) in time_and_tz.char_indices() {
        if (c == '+' || c == '-') && i > 0 {
            let (time, tz) = time_and_tz.split_at(i);
            let sign = if c == '+' { 1 } else { -1 };
            let body = &tz[1..];
            let (h, m) = body.split_once(':').unwrap_or((body, "0"));
            let h: i64 = h.parse().ok()?;
            let m: i64 = m.parse().ok()?;
            return Some((time, sign * (h * 3_600 + m * 60)));
        }
    }
    Some((time_and_tz, 0))
}

/// Days since 1970-01-01 for a civil `(year, month, day)` (Howard Hinnant's
/// `days_from_civil`, exact for the proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_handles_z_offset_and_fractional() {
        // 2021-01-01T00:00:00Z = 1609459200.
        let z = parse_rfc3339("2021-01-01T00:00:00Z").unwrap();
        assert_eq!(z, UNIX_EPOCH + Duration::from_secs(1_609_459_200));
        // The same instant expressed with a +02:00 offset reads two hours earlier
        // in UTC terms (02:00+02:00 == 00:00Z).
        let off = parse_rfc3339("2021-01-01T02:00:00+02:00").unwrap();
        assert_eq!(off, z);
        // A negative offset.
        let neg = parse_rfc3339("2020-12-31T19:00:00-05:00").unwrap();
        assert_eq!(neg, z);
        // Fractional seconds are accepted (and dropped to the second).
        let frac = parse_rfc3339("2021-01-01T00:00:00.123456Z").unwrap();
        assert_eq!(frac, z);
        // Garbage is rejected.
        assert!(parse_rfc3339("not a date").is_none());
        assert!(parse_rfc3339("2021-13-01T00:00:00Z").is_none());
    }

    #[test]
    fn parse_rfc3339_rejects_out_of_range_and_pre_epoch_times() {
        // An out-of-range clock field (hour 24) is refused.
        assert!(parse_rfc3339("2021-01-01T24:00:00Z").is_none());
        assert!(parse_rfc3339("2021-01-01T00:60:00Z").is_none());
        // A civil time before the Unix epoch has no representation here.
        assert!(parse_rfc3339("1969-12-31T23:00:00Z").is_none());
        // A positive offset that pushes a near-epoch time below zero is refused.
        assert!(parse_rfc3339("1970-01-01T00:00:00+01:00").is_none());
    }

    #[test]
    fn parse_rfc3339_reads_a_bare_time_with_no_designator_as_utc() {
        // No `Z`/`+`/`-` designator ⇒ zero offset (read as UTC), the
        // `split_offset` fall-through arm.
        let bare = parse_rfc3339("2021-01-01T00:00:00").unwrap();
        assert_eq!(bare, UNIX_EPOCH + Duration::from_secs(1_609_459_200));
    }

    /// The board's own form reads to the millisecond, a whole second, and what is no
    /// instant is none.
    #[test]
    fn unix_millis_reads_the_boards_instants() {
        assert_eq!(unix_millis("2026-11-14T09:30:00Z"), Some(1_794_648_600_000));
        assert_eq!(unix_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_millis("2024-02-29T23:59:59Z"), Some(1_709_251_199_000));
        assert_eq!(unix_millis(""), None);
        assert_eq!(unix_millis("2026-11-14"), None);
        assert_eq!(unix_millis("1969-12-31T23:59:59Z"), None);
    }
}
