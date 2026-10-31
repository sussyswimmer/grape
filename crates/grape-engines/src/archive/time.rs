//! Modification times: Unix seconds, ZIP's MS-DOS date and time, and
//! `SystemTime`, without a time crate.
//!
//! ZIP's DOS fields have no time zone. Std has no time zone API either, so
//! they are read and written as UTC (DECISIONS D27). The extended
//! timestamp field Grape writes next to them is exact UTC, so Grape's own
//! ZIPs keep the time to the second.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zip::DateTime;

/// Seconds from 1970-01-01 to 1980-01-01, the first moment ZIP can hold.
const ZIP_FIRST: i64 = 315_532_800;

/// Days from 1970-01-01 to `year-month-day` in the proleptic Gregorian
/// calendar (Howard Hinnant's `days_from_civil`).
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date `days` after 1970-01-01: `(year, month, day)`.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // `day` is 1..=31 and `month` 1..=12 by construction.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// A ZIP entry's DOS date and time as Unix seconds, read as UTC.
pub(crate) fn from_dos(time: DateTime) -> i64 {
    let days = days_from_civil(
        i64::from(time.year()),
        u32::from(time.month()),
        u32::from(time.day()),
    );
    days * 86_400
        + i64::from(time.hour()) * 3_600
        + i64::from(time.minute()) * 60
        + i64::from(time.second())
}

/// Unix seconds as a ZIP DOS date and time in UTC, clamped to what ZIP can
/// hold (1980-01-01 to 2107-12-31). DOS times have 2-second steps.
pub(crate) fn to_dos(secs: i64) -> DateTime {
    if secs < ZIP_FIRST {
        return DateTime::default();
    }
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let in_day = secs.rem_euclid(86_400);
    if year > 2107 {
        return DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58).unwrap_or_default();
    }
    let field = |v: i64| u8::try_from(v).unwrap_or(0);
    DateTime::from_date_and_time(
        u16::try_from(year).unwrap_or(1980),
        field(i64::from(month)),
        field(i64::from(day)),
        field(in_day / 3_600),
        field(in_day % 3_600 / 60),
        field(in_day % 60),
    )
    .unwrap_or_default()
}

/// NTFS FILETIME (100 ns steps since 1601-01-01) as Unix seconds.
pub(crate) fn from_filetime(filetime: u64) -> i64 {
    i64::try_from(filetime / 10_000_000).unwrap_or(i64::MAX) - 11_644_473_600
}

/// Unix seconds as a `SystemTime`, if the platform can hold it.
pub(crate) fn to_system(secs: i64) -> Option<SystemTime> {
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// A `SystemTime` as whole Unix seconds (rounded down), saturating.
pub(crate) fn from_system(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            // -0.5 s is second -1, not second 0.
            if before.subsec_nanos() > 0 {
                -secs - 1
            } else {
                -secs
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        // A leap day.
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn dos_times_are_utc_and_clamped() {
        // 2023-11-14 22:13:20 UTC.
        let secs = 1_700_000_000;
        let dos = to_dos(secs);
        assert_eq!(
            (dos.year(), dos.month(), dos.day()),
            (2023, 11, 14),
            "{dos:?}"
        );
        assert_eq!((dos.hour(), dos.minute(), dos.second()), (22, 13, 20));
        assert_eq!(from_dos(dos), secs);
        // Odd seconds round down to DOS's 2-second steps.
        assert_eq!(from_dos(to_dos(secs + 1)), secs);
        // Before 1980 and after 2107.
        assert_eq!(to_dos(0), DateTime::default());
        assert_eq!(to_dos(-5), DateTime::default());
        let last = to_dos(i64::from(u32::MAX) * 2);
        assert_eq!((last.year(), last.month(), last.day()), (2107, 12, 31));
    }

    #[test]
    fn system_times_and_filetimes() {
        assert_eq!(
            from_system(to_system(1_700_000_000).unwrap()),
            1_700_000_000
        );
        assert_eq!(from_system(to_system(-86_400).unwrap()), -86_400);
        let half_before = UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(from_system(half_before), -1);
        // 2023-11-14 22:13:20 UTC as a FILETIME.
        assert_eq!(from_filetime(133_444_736_000_000_000), 1_700_000_000);
    }
}
