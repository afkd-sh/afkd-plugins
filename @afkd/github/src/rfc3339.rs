//! Dependency-free RFC-3339 / ISO-8601 decoding and encoding: the reader every comment
//! stamp GitHub sends goes through, and the writer the `comments` reply's `at` comes out of.
//!
//! The reader is afkd's own (`afkd_forge::rfc3339`), so a watermark reads the same instant
//! here as in the built-in trigger. The writer is this plugin's: afkd parses each `at` with
//! that same reader, so what it writes is exactly the shape the reader accepts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parse an RFC-3339 / ISO-8601 timestamp into a [`SystemTime`], dependency-free.
///
/// Handles the forms a forge emits: `2026-06-27T12:34:56Z`, a numeric offset
/// `…+02:00`/`…-05:00`, and fractional seconds `…12:34:56.789Z`. Returns `None`
/// for anything it cannot read, so a malformed timestamp degrades to a default
/// rather than panicking. Only times at or after the Unix epoch are represented.
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
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

/// Render `t` as `YYYY-MM-DDTHH:MM:SSZ`, in UTC and to the second — the granularity
/// [`parse_rfc3339`] reads back, so a round trip is exact. A time before the epoch
/// (which nothing decoded here can be) renders as the epoch.
pub(crate) fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
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

/// The inverse of [`days_from_civil`]: the civil `(year, month, day)` of a day count
/// since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
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

    /// The writer's edges, each read straight back by the reader afkd itself parses an
    /// `at` with: the epoch (an absent GitHub `created_at` decodes to it), a leap day, the
    /// last second of a day and of a year, and far-future stamps past 2100 (a century
    /// that is not a leap year) and at the four-digit ceiling.
    #[test]
    fn format_utc_round_trips_through_the_reader() {
        for (secs, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (1_735_689_599, "2024-12-31T23:59:59Z"),
            (4_107_542_400, "2100-03-01T00:00:00Z"),
            (253_402_300_799, "9999-12-31T23:59:59Z"),
        ] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(format_utc(t), text, "{secs}");
            assert_eq!(parse_rfc3339(text), Some(t), "{text} reads back");
        }
        // Sub-second precision is dropped, exactly as the reader drops it.
        let t = UNIX_EPOCH + Duration::from_millis(1_609_459_200_999);
        assert_eq!(format_utc(t), "2021-01-01T00:00:00Z");
    }
}
