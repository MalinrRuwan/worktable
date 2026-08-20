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
