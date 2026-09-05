//! Wall-clock helpers. Timestamps are Unix seconds throughout.

use std::time::{SystemTime, UNIX_EPOCH};

pub const DAY: i64 = 86_400;
/// Rooms become read-only 14 days after creation (SPEC.md §3.1).
pub const LOCK_AFTER: i64 = 14 * DAY;
/// Rooms are deleted a year after creation (SPEC.md §3.1).
pub const DELETE_AFTER: i64 = 365 * DAY;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before 1970")
        .as_secs() as i64
}

/// Format Unix seconds as `YYYY-MM-DD HH:MM UTC`.
pub fn format_utc(ts: i64) -> String {
    let (secs_of_day, days) = (ts.rem_euclid(DAY), ts.div_euclid(DAY));
    let (h, m) = (secs_of_day / 3600, (secs_of_day % 3600) / 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

/// Days-since-epoch → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC"); // leap day
        assert_eq!(format_utc(1_756_684_800), "2025-09-01 00:00 UTC");
        assert_eq!(format_utc(1_756_712_345), "2025-09-01 07:39 UTC");
    }
}
