//! Timestamps and durations, in nanoseconds since the Unix epoch (UTC).
//!
//! Exchange data arrives in milliseconds or microseconds; nanoseconds hold
//! all of them without loss and let the engine model sub-millisecond
//! latency. An `i64` of nanoseconds covers roughly the years 1677 to 2262.

use std::fmt;
use std::ops::{Add, AddAssign, Sub};

use serde::{Deserialize, Serialize};

const NANOS_PER_MICRO: i64 = 1_000;
const NANOS_PER_MILLI: i64 = 1_000_000;
const NANOS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// A span of time in nanoseconds. Can be negative.
#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimeDelta(i64);

impl TimeDelta {
    pub const ZERO: Self = Self(0);

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros * NANOS_PER_MICRO)
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self(millis * NANOS_PER_MILLI)
    }

    pub const fn from_secs(secs: i64) -> Self {
        Self(secs * NANOS_PER_SEC)
    }

    pub const fn from_mins(mins: i64) -> Self {
        Self::from_secs(mins * 60)
    }

    pub const fn from_hours(hours: i64) -> Self {
        Self::from_secs(hours * 3_600)
    }

    pub const fn from_days(days: i64) -> Self {
        Self::from_secs(days * SECS_PER_DAY)
    }

    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / NANOS_PER_SEC as f64
    }
}

impl fmt::Debug for TimeDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TimeDelta({}ns)", self.0)
    }
}

/// A point in time: nanoseconds since 1970-01-01T00:00:00Z.
#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const EPOCH: Self = Self(0);
    pub const MIN: Self = Self(i64::MIN);
    pub const MAX: Self = Self(i64::MAX);

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros * NANOS_PER_MICRO)
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self(millis * NANOS_PER_MILLI)
    }

    pub const fn from_secs(secs: i64) -> Self {
        Self(secs * NANOS_PER_SEC)
    }

    /// Midnight UTC at the start of a calendar date. Panics on an invalid
    /// month or day.
    pub fn from_ymd(year: i64, month: u32, day: u32) -> Self {
        Self::from_ymd_hms(year, month, day, 0, 0, 0)
    }

    /// A UTC calendar date and time. Panics if any field is out of range.
    pub fn from_ymd_hms(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Self {
        assert!((1..=12).contains(&month), "month out of range: {month}");
        assert!(
            (1..=days_in_month(year, month)).contains(&day),
            "day out of range: {year}-{month:02}-{day:02}"
        );
        assert!(
            hour < 24 && min < 60 && sec < 60,
            "time out of range: {hour}:{min}:{sec}"
        );
        let days = days_from_civil(year, month, day);
        let secs = days * SECS_PER_DAY + i64::from(hour * 3_600 + min * 60 + sec);
        Self::from_secs(secs)
    }

    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    pub const fn as_millis(self) -> i64 {
        self.0.div_euclid(NANOS_PER_MILLI)
    }

    /// Year, month and day (UTC).
    pub fn date(self) -> (i64, u32, u32) {
        civil_from_days(self.0.div_euclid(NANOS_PER_SEC * SECS_PER_DAY))
    }
}

impl Add<TimeDelta> for Timestamp {
    type Output = Self;
    fn add(self, rhs: TimeDelta) -> Self {
        Self(self.0.checked_add(rhs.0).expect("timestamp overflow"))
    }
}

impl AddAssign<TimeDelta> for Timestamp {
    fn add_assign(&mut self, rhs: TimeDelta) {
        *self = *self + rhs;
    }
}

impl Sub<TimeDelta> for Timestamp {
    type Output = Self;
    fn sub(self, rhs: TimeDelta) -> Self {
        Self(self.0.checked_sub(rhs.0).expect("timestamp overflow"))
    }
}

impl Sub for Timestamp {
    type Output = TimeDelta;
    fn sub(self, rhs: Self) -> TimeDelta {
        TimeDelta(self.0.checked_sub(rhs.0).expect("timestamp overflow"))
    }
}

/// ISO 8601 in UTC, with a fractional part only when there is one:
/// `2024-03-01T09:30:00Z`, `2024-03-01T09:30:00.25Z`.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.div_euclid(NANOS_PER_SEC);
        let nanos = self.0.rem_euclid(NANOS_PER_SEC);
        let (year, month, day) = civil_from_days(secs.div_euclid(SECS_PER_DAY));
        let sod = secs.rem_euclid(SECS_PER_DAY);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            sod / 3_600,
            sod % 3_600 / 60,
            sod % 60
        )?;
        if nanos != 0 {
            let digits = format!("{nanos:09}");
            write!(f, ".{}", digits.trim_end_matches('0'))?;
        }
        f.write_str("Z")
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({self})")
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Calendar conversions from Howard Hinnant's "chrono-Compatible Low-Level
// Date Algorithms": proleptic Gregorian calendar, day 0 = 1970-01-01.

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (month, day) = (i64::from(month), i64::from(day));
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(Timestamp::from_ymd(1970, 1, 1), Timestamp::EPOCH);
        // Binance reports times in milliseconds: 1704067200000 is 2024-01-01.
        assert_eq!(
            Timestamp::from_ymd(2024, 1, 1),
            Timestamp::from_millis(1_704_067_200_000)
        );
        assert_eq!(
            Timestamp::from_ymd(2000, 3, 1).as_nanos(),
            951_868_800 * NANOS_PER_SEC
        );
        assert_eq!(
            Timestamp::from_ymd(1969, 12, 31),
            Timestamp::from_secs(-SECS_PER_DAY)
        );
    }

    #[test]
    fn round_trips_every_day_across_four_centuries() {
        let start = days_from_civil(1800, 1, 1);
        let end = days_from_civil(2201, 1, 1);
        for days in start..end {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
            assert!((1..=days_in_month(y, m)).contains(&d));
        }
    }

    #[test]
    fn leap_years() {
        assert_eq!(Timestamp::from_ymd(2024, 2, 29).date(), (2024, 2, 29));
        assert_eq!(Timestamp::from_ymd(2000, 2, 29).date(), (2000, 2, 29));
    }

    #[test]
    #[should_panic(expected = "day out of range")]
    fn rejects_feb_29_in_a_century_non_leap_year() {
        Timestamp::from_ymd(1900, 2, 29);
    }

    #[test]
    fn displays_iso_8601() {
        let t = Timestamp::from_ymd_hms(2024, 3, 1, 9, 30, 5);
        assert_eq!(t.to_string(), "2024-03-01T09:30:05Z");
        assert_eq!(
            (t + TimeDelta::from_millis(250)).to_string(),
            "2024-03-01T09:30:05.25Z"
        );
        assert_eq!(
            (t + TimeDelta::from_nanos(1)).to_string(),
            "2024-03-01T09:30:05.000000001Z"
        );
        assert_eq!(
            Timestamp::from_nanos(-1).to_string(),
            "1969-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn arithmetic() {
        let t = Timestamp::from_ymd(2024, 1, 1);
        let later = t + TimeDelta::from_days(31) + TimeDelta::from_hours(2);
        assert_eq!(later, Timestamp::from_ymd_hms(2024, 2, 1, 2, 0, 0));
        assert_eq!(later - t, TimeDelta::from_mins(31 * 24 * 60 + 120));
        assert_eq!(
            later - TimeDelta::from_hours(2),
            Timestamp::from_ymd(2024, 2, 1)
        );
        assert_eq!(Timestamp::from_nanos(-1).as_millis(), -1);
    }
}
