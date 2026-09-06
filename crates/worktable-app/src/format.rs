//! Small formatting helpers for the UI.

use std::time::{SystemTime, UNIX_EPOCH};

/// Format a Unix-epoch millisecond timestamp as a friendly relative time.
pub fn relative_time(created_at_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let delta = (now - created_at_ms).max(0) / 1000;

    if delta < 10 {
        return "just now".to_owned();
    }
    if delta < 60 {
        return format!("{}s ago", delta);
    }
    if delta < 3600 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h ago", delta / 3600);
    }
    if delta < 7 * 86_400 {
        let days = delta / 86_400;
        return if days == 1 {
            "yesterday".to_owned()
        } else {
            format!("{days}d ago")
        };
    }
    if delta < 30 * 86_400 {
        return format!("{}w ago", delta / (7 * 86_400));
    }

    // Fall back to a plain date for anything older.
    let seconds = created_at_ms / 1000;
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days);
    format!("{year}-{month:02}-{day:02}")
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Section label for an entry's creation day: "Today", "Yesterday", the
/// weekday for the past week, then "Mon D" (with the year when different).
/// Buckets are computed in whole UTC days — good enough for visual grouping.
pub fn day_bucket(created_at_ms: i64) -> String {
    let days_now = now_ms().div_euclid(86_400_000);
    let days_then = created_at_ms.div_euclid(86_400_000);
    let diff = days_now - days_then;

    if diff <= 0 {
        return "Today".to_owned();
    }
    if diff == 1 {
        return "Yesterday".to_owned();
    }
    let (year, month, day) = civil_from_days(days_then);
    if diff < 7 {
        // 1970-01-01 (day 0) was a Thursday.
        let weekday = (days_then.rem_euclid(7) + 4) % 7;
        return WEEKDAYS[weekday as usize].to_owned();
    }
    let (now_year, _, _) = civil_from_days(days_now);
    let month_name = MONTHS[(month - 1).clamp(0, 11) as usize];
    if year == now_year {
        format!("{month_name} {day}")
    } else {
        format!("{month_name} {day}, {year}")
    }
}

/// Convert days since the Unix epoch to a (year, month, day) civil date.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    // Howard Hinnant's civil_from_days algorithm.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as i64, d as i64)
}

/// Days since the Unix epoch for a civil date (inverse of `civil_from_days`,
/// Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse a UTC ISO-8601 timestamp like `2024-08-21T17:49:07Z` (GitHub's
/// format) to Unix epoch milliseconds. Fractional seconds are tolerated and
/// ignored. Returns `None` for anything unparseable.
pub fn iso8601_to_epoch_ms(input: &str) -> Option<i64> {
    let input = input.trim();
    let bytes = input.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let sep = bytes[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let year: i64 = input.get(0..4)?.parse().ok()?;
    let month: i64 = input.get(5..7)?.parse().ok()?;
    let day: i64 = input.get(8..10)?.parse().ok()?;
    let hour: i64 = input.get(11..13)?.parse().ok()?;
    let minute: i64 = input.get(14..16)?.parse().ok()?;
    let second: i64 = input.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + minute) * 60 * 1000 + second * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 86_400_000;

    fn now() -> i64 {
        now_ms()
    }

    #[test]
    fn relative_time_buckets() {
        let n = now();
        assert_eq!(relative_time(n), "just now");
        assert_eq!(relative_time(n - 30_000), "30s ago");
        assert_eq!(relative_time(n - 5 * 60_000), "5m ago");
        assert_eq!(relative_time(n - 3 * 3_600_000), "3h ago");
        assert_eq!(relative_time(n - DAY_MS), "yesterday");
        assert_eq!(relative_time(n - 3 * DAY_MS), "3d ago");
        assert_eq!(relative_time(n - 14 * DAY_MS), "2w ago");
        // Older than 30 days → an absolute date.
        let old = relative_time(n - 400 * DAY_MS);
        assert!(
            old.chars().filter(|c| *c == '-').count() == 2 && old.len() == 10,
            "expected YYYY-MM-DD, got {old}"
        );
        // Future timestamps clamp to "just now".
        assert_eq!(relative_time(n + DAY_MS), "just now");
    }

    #[test]
    fn day_bucket_labels() {
        let n = now();
        assert_eq!(day_bucket(n), "Today");
        assert_eq!(day_bucket(n - DAY_MS), "Yesterday");
        let weekday = day_bucket(n - 3 * DAY_MS);
        assert!(
            WEEKDAYS.contains(&weekday.as_str()),
            "3 days ago should be a weekday name, got {weekday}"
        );
        // 30 days ago → "Mon D" (same year) — no weekday, no comma+year.
        let recent = day_bucket(n - 30 * DAY_MS);
        assert!(
            MONTHS.iter().any(|m| recent.starts_with(m)) && !recent.contains(','),
            "expected 'Mon D', got {recent}"
        );
        // Over a year ago → includes the year.
        let ancient = day_bucket(n - 400 * DAY_MS);
        assert!(
            ancient.contains(','),
            "expected 'Mon D, YYYY', got {ancient}"
        );
    }

    #[test]
    fn iso8601_round_trips_through_civil_days() {
        // Epoch + known instants.
        assert_eq!(iso8601_to_epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso8601_to_epoch_ms("2024-01-01T00:00:00Z"),
            Some(1_704_067_200_000)
        );
        // Fractional seconds + space separator tolerated.
        assert_eq!(
            iso8601_to_epoch_ms("2024-08-21 17:49:07.123Z"),
            iso8601_to_epoch_ms("2024-08-21T17:49:07Z")
        );
        // Round-trip with the civil decoder.
        let ms = iso8601_to_epoch_ms("2026-08-22T09:30:00Z").unwrap();
        let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
        assert_eq!((y, m, d), (2026, 8, 22));
        assert_eq!(ms.rem_euclid(86_400_000), (9 * 3600 + 30 * 60) * 1000);
        // Junk rejected.
        assert_eq!(iso8601_to_epoch_ms("not a date"), None);
        assert_eq!(iso8601_to_epoch_ms("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [0i64, 18_993, 19_723, 20_628, -1, -365] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip {days}");
        }
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
