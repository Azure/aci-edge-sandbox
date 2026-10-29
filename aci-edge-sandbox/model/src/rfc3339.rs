// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! RFC 3339 timestamps in UTC, such as `2026-10-29T07:00:00Z`, which `humantime` writes and reads.
//!
//! A time is written as its date and time of day in UTC, with a `T` between them and the offset
//! `Z`, and with a fraction of a second only when it has one, in nine digits, such as
//! `2026-10-29T07:00:00.500000000Z`. It is read back in the same form, with one to nine digits of
//! fraction. The form covers the times from the Unix epoch through the end of the year 9999.

use std::time::{SystemTime, UNIX_EPOCH};

/// The last second of the form: 9999-12-31T23:59:59Z.
const MAX_SECONDS: u64 = 253_402_300_799;

/// What a time in this form looks like, for errors.
const EXPECTED: &str =
    "a time must be RFC 3339 in UTC, such as 2026-10-29T07:00:00Z, from 1970 through 9999";

/// Writes `time`, or returns `None` for a time before the Unix epoch or after the year 9999.
pub(crate) fn format(time: SystemTime) -> Option<String> {
    // `humantime` panics on a time before the epoch, and fails on one after the year 9999.
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    if seconds > MAX_SECONDS {
        return None;
    }
    Some(humantime::format_rfc3339(time).to_string())
}

/// Reads a time in this form, or returns `None` for any other text.
pub(crate) fn parse(text: &str) -> Option<SystemTime> {
    // `humantime` also reads the offset `+00:00`, a fraction with no digits or more than nine,
    // and second 60 as second 59, which this form refuses.
    let (whole_seconds, fraction) = text
        .strip_suffix('Z')?
        .split_at_checked("2026-10-29T07:00:00".len())?;
    if whole_seconds.ends_with(":60") || fraction == "." || fraction.len() > ".123456789".len() {
        return None;
    }
    humantime::parse_rfc3339(text).ok()
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
    use std::time::Duration;

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
            // `humantime` writes every fraction in nine digits.
            (1_793_257_200, 500_000_000, "2026-10-29T07:00:00.500000000Z"),
            (1_793_257_200, 123_456_700, "2026-10-29T07:00:00.123456700Z"),
            (MAX_SECONDS, 0, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(format(at(seconds, nanos)).as_deref(), Some(text));
            assert_eq!(parse(text), Some(at(seconds, nanos)), "{text}");
        }
        // Fractions of any length up to nanoseconds.
        for (text, nanos) in [
            ("2026-10-29T07:00:00.5Z", 500_000_000),
            ("2026-10-29T07:00:00.250Z", 250_000_000),
            ("2026-10-29T07:00:00.1234567Z", 123_456_700),
            ("2026-10-29T07:00:00.000000100Z", 100),
        ] {
            assert_eq!(parse(text), Some(at(1_793_257_200, nanos)), "{text}");
        }
    }

    #[test]
    fn other_forms_and_impossible_times_are_refused() {
        for text in [
            "",
            "Z",
            "2026-10-29T07:00:00",
            "2026-10-29t07:00:00Z",
            "2026-10-29T07:00:00z",
            "2026-10-29 07:00:00Z",
            "2026-10-29T07:00Z",
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
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn forms_that_humantime_also_reads_are_refused() {
        for text in [
            "2026-10-29T07:00:00+00:00",
            "2026-10-29T07:00:00.Z",
            "2026-10-29T07:00:00.1234567890Z",
            "2026-10-29T07:00:60Z",
        ] {
            assert!(humantime::parse_rfc3339(text).is_ok(), "{text}");
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
