//! Timestamps in the form `2026-09-25T14:03:37Z`, with no date library.

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time in UTC, to the second.
pub fn utc_now() -> String {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
    format_utc(seconds)
}

/// Formats seconds since 1970 as an ISO 8601 UTC timestamp.
///
/// The date arithmetic is Howard Hinnant's `civil_from_days`, which turns a
/// count of days into a year, month and day without a table of month lengths.
///
/// @param seconds - seconds since 1970-01-01T00:00:00Z
pub fn format_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", time / 3600, time % 3600 / 60, time % 60)
}

#[cfg(test)]
mod tests {
    use super::format_utc;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_790_345_017), "2026-09-25T14:03:37Z");
    }
}
