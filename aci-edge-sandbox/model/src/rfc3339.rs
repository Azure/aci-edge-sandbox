// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! RFC 3339 timestamps in UTC, such as `2026-10-29T07:00:00Z`.
//!
//! A time is written as its date and time of day in UTC, with a fraction of a second only when it
//! has one, and read back in the same form: a `T` between the date and the time, one to nine
//! digits of fraction, and the offset `Z`. The form covers the times from the Unix epoch through
//! the end of the year 9999.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: u64 = 86_400;
/// Days from 0000-03-01 to the Unix epoch in the proleptic Gregorian calendar.
const EPOCH_DAYS: u64 = 719_468;
/// Days in each 400-year cycle of the Gregorian calendar.
const CYCLE_DAYS: u64 = 146_097;
/// The last second of the form: 9999-12-31T23:59:59Z.
const MAX_SECONDS: u64 = 253_402_300_799;

/// What a time in this form looks like, for errors.
const EXPECTED: &str =
    "a time must be RFC 3339 in UTC, such as 2026-10-29T07:00:00Z, from 1970 through 9999";

/// Writes `time`, or returns `None` for a time before the Unix epoch or after the year 9999.
pub(crate) fn format(time: SystemTime) -> Option<String> {
    let since_epoch = time.duration_since(UNIX_EPOCH).ok()?;
    let seconds = since_epoch.as_secs();
    if seconds > MAX_SECONDS {
        return None;
    }
    let (year, month, day) = civil_from_days(seconds / SECONDS_PER_DAY);
    let second_of_day = seconds % SECONDS_PER_DAY;
    let mut text = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60
    );
    let nanos = since_epoch.subsec_nanos();
    if nanos != 0 {
        text.push('.');
        text.push_str(format!("{nanos:09}").trim_end_matches('0'));
    }
    text.push('Z');
    Some(text)
}

/// Reads a time in this form, or returns `None` for any other text.
pub(crate) fn parse(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    if !text.is_ascii() || text.len() < 19 {
        return None;
    }
    let (date_time, fraction) = text.split_at(19);
    let separators = date_time.as_bytes();
    if [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')]
        .iter()
        .any(|&(index, separator)| separators[index] != separator)
    {
        return None;
    }
    let field = |range: std::ops::Range<usize>| {
        let digits = &date_time[range];
        if digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.parse::<u64>().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let nanos = match fraction.strip_prefix('.') {
        None if fraction.is_empty() => 0,
        Some(digits)
            if (1..=9).contains(&digits.len())
                && digits.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            format!("{digits:0<9}").parse::<u32>().ok()?
        }
        _ => return None,
    };
    if year < 1970 || !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // A day past the end of its month counts into the next month, so its date differs.
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let seconds = days * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

/// Days from the Unix epoch to a date in or after 1970 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    // Counting years from March puts each leap day at the end of its year.
    let year = if month <= 2 { year - 1 } else { year };
    let year_of_era = year % 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    year / 400 * CYCLE_DAYS + day_of_era - EPOCH_DAYS
}

/// The date of a day counted from the Unix epoch (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + EPOCH_DAYS;
    let day_of_era = days % CYCLE_DAYS;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months count from March.
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = days / CYCLE_DAYS * 400 + year_of_era + u64::from(month <= 2);
    (year, month, day)
}

/// Serde helper that writes an optional time in this form.
pub(crate) mod option {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        time: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match time {
            None => serializer.serialize_none(),
            Some(time) => serializer.serialize_some(
                &super::format(*time).ok_or_else(|| serde::ser::Error::custom(super::EXPECTED))?,
            ),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| {
                super::parse(&text).ok_or_else(|| serde::de::Error::custom(super::EXPECTED))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64, nanos: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(seconds, nanos)
    }

    #[test]
    fn times_are_written_in_utc_and_read_back() {
        for (seconds, nanos, text) in [
            (0, 0, "1970-01-01T00:00:00Z"),
            (946_684_799, 0, "1999-12-31T23:59:59Z"),
            (951_827_696, 0, "2000-02-29T12:34:56Z"),
            (1_709_251_199, 0, "2024-02-29T23:59:59Z"),
            (1_793_257_200, 0, "2026-10-29T07:00:00Z"),
            (1_793_257_200, 500_000_000, "2026-10-29T07:00:00.5Z"),
            (1_793_257_200, 123_456_700, "2026-10-29T07:00:00.1234567Z"),
            (MAX_SECONDS, 0, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(format(at(seconds, nanos)).as_deref(), Some(text));
            assert_eq!(parse(text), Some(at(seconds, nanos)), "{text}");
        }
        // Fractions of any length up to nanoseconds.
        assert_eq!(
            parse("2026-10-29T07:00:00.250Z"),
            Some(at(1_793_257_200, 250_000_000))
        );
        assert_eq!(
            parse("2026-10-29T07:00:00.000000100Z"),
            Some(at(1_793_257_200, 100))
        );
    }

    #[test]
    fn other_forms_and_impossible_times_are_refused() {
        for text in [
            "",
            "Z",
            "2026-10-29T07:00:00",
            "2026-10-29T07:00:00+00:00",
            "2026-10-29t07:00:00Z",
            "2026-10-29T07:00:00z",
            "2026-10-29 07:00:00Z",
            "2026-10-29T07:00Z",
            "2026-10-29T07:00:00.Z",
            "2026-10-29T07:00:00.1234567890Z",
            "2026-10-29T07:00:00,5Z",
            "2026-10-29T07:00:00.+5Z",
            "2026-10-29T07:00:00Zx",
            "+026-10-29T07:00:00Z",
            "2026-1-029T07:00:00Z",
            "２026-10-29T07:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-10-00T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-10-29T24:00:00Z",
            "2026-10-29T07:60:00Z",
            "2026-10-29T07:00:60Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn times_outside_the_form_are_not_written() {
        assert_eq!(format(at(MAX_SECONDS + 1, 0)), None);
        if let Some(before) = UNIX_EPOCH.checked_sub(Duration::from_secs(1)) {
            assert_eq!(format(before), None);
        }
    }
}
