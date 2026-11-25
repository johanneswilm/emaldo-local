// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Minimal civil-time helpers (no chrono/time crate). Everything is UTC here;
// Nordpool day-ahead days are keyed by *local* delivery date, so the module
// converts using a fixed offset supplied by config (default +1h, CET winter).

/// UNIX seconds now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a (year, month, day) civil date. Howard Hinnant's
/// algorithm; valid for the years we care about (positive).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Civil (year, month, day) from days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn unix_to_ymd(ts: i64) -> (i64, i64, i64) {
    civil_from_days(ts.div_euclid(86400))
}

pub fn ymd_to_unix_days(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d)
}

/// `YYYY-MM-DD` for the UTC instant `ts` shifted by `local_offset_secs`.
pub fn date_string(ts: i64, local_offset_secs: i64) -> String {
    let (y, m, d) = unix_to_ymd(ts + local_offset_secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// RFC-3339 `YYYY-MM-DDTHH:MM:SSZ` for a UTC instant (input is already UTC).
#[allow(dead_code)]
pub fn rfc3339_utc(ts: i64) -> String {
    let (y, m, d) = unix_to_ymd(ts);
    let sod = ts.rem_euclid(86400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Local (offset-shifted) clock `±HH:MM` for a UTC instant, for display.
pub fn local_hhmm(ts: i64, local_offset_secs: i64) -> String {
    let sod = (ts + local_offset_secs).rem_euclid(86400);
    format!("{:02}:{:02}", sod / 3600, (sod % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_dates() {
        assert_eq!(unix_to_ymd(0), (1970, 1, 1));
        // 2026-11-25T00:00:00Z
        assert_eq!(unix_to_ymd(1795564800), (2026, 11, 25));
        assert_eq!(ymd_to_unix_days(2026, 11, 25), 1795564800 / 86400);
        // round-trip a leap day
        let d = ymd_to_unix_days(2024, 2, 29);
        assert_eq!(unix_to_ymd(d * 86400), (2024, 2, 29));
    }

    #[test]
    fn strings() {
        assert_eq!(rfc3339_utc(1795564800 + 3661), "2026-11-25T01:01:01Z");
        // +1h shift rolls the date/time
        assert_eq!(date_string(1795564800 - 1800, 3600), "2026-11-25");
        assert_eq!(date_string(1795564800 + 86400 - 60, 3600), "2026-11-26");
        assert_eq!(local_hhmm(1795564800, 3600), "01:00");
    }
}