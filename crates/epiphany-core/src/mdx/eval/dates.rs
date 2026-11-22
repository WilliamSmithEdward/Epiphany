//! Dates as Excel and VBA count them: days from 30 December 1899, with the time of day as the
//! fraction of a day. The functions that make, take apart, move and compare dates work on
//! such numbers, so that a date may be kept in any numeric cell, as a workbook keeps it.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{Evaluated, fail};

/// The first and the last day there may be dates of: 1 January 100 and 31 December 9999,
/// the days VBA allows.
const FIRST: i64 = -657_434;
const LAST: i64 = 2_958_465;
/// 1 January 1970, from which the system clock counts.
const EPOCH: i64 = 25_569;
const SECONDS_A_DAY: i64 = 86_400;

/// The names of the months, in English.
pub(super) const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The names of the days of the week from Sunday, in English.
pub(super) const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A date taken apart, to the nearest second.
#[derive(Debug, Clone, Copy)]
pub(super) struct Parts {
    /// Days from 30 December 1899.
    pub(super) days: i64,
    pub(super) year: i64,
    pub(super) month: i64,
    pub(super) day: i64,
    /// Seconds from midnight.
    pub(super) seconds: i64,
}

/// What `DateAdd` moves a date by and `DateDiff` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interval {
    Year,
    Quarter,
    Month,
    Day,
    /// To `DateAdd` a day, and to `DateDiff` seven of them.
    Weekday,
    /// A calendar week, beginning on a Sunday.
    Week,
    Hour,
    Minute,
    Second,
}

impl Interval {
    /// An interval by VBA's name for it: yyyy, q, m, y, d, w, ww, h, n or s.
    pub(super) fn named(name: &str) -> Evaluated<Self> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "yyyy" => Self::Year,
            "q" => Self::Quarter,
            "m" => Self::Month,
            "y" | "d" => Self::Day,
            "w" => Self::Weekday,
            "ww" => Self::Week,
            "h" => Self::Hour,
            "n" => Self::Minute,
            "s" => Self::Second,
            _ => {
                return fail(format!(
                    "'{name}' is no interval: yyyy, q, m, y, d, w, ww, h, n or s"
                ));
            }
        })
    }
}

impl Parts {
    /// A date taken apart, if it falls in the years 100 to 9999.
    pub(super) fn of(date: f64) -> Evaluated<Self> {
        // To the nearest second first, so that a moment before midnight may be the day after.
        let seconds = (date * SECONDS_A_DAY as f64).round();
        let range = (FIRST * SECONDS_A_DAY) as f64..((LAST + 1) * SECONDS_A_DAY) as f64;
        if !range.contains(&seconds) {
            return fail("a date falls in the years 100 to 9999");
        }
        let seconds = seconds as i64;
        let days = seconds.div_euclid(SECONDS_A_DAY);
        let (year, month, day) = civil(days - EPOCH);
        Ok(Self {
            days,
            year,
            month,
            day,
            seconds: seconds.rem_euclid(SECONDS_A_DAY),
        })
    }

    /// The date as a number.
    pub(super) fn value(&self) -> f64 {
        at(self.days, self.seconds)
    }

    /// The day of the week, from 1 for the day the week begins on: 1 for Sunday, 2 for
    /// Monday and so on to 7 for Saturday.
    pub(super) fn weekday(&self, first: i64) -> i64 {
        // Day 1, 31 December 1899, was a Sunday.
        (self.days - first).rem_euclid(7) + 1
    }

    /// The day of the year, from 1.
    pub(super) fn day_of_year(&self) -> i64 {
        self.days - days(self.year, 1, 1) + 1
    }

    /// The week of the year, from 1 for the week of 1 January, weeks beginning on Sundays.
    pub(super) fn week_of_year(&self) -> i64 {
        let new_year = days(self.year, 1, 1);
        (self.days - new_year + (new_year - 1).rem_euclid(7)) / 7 + 1
    }

    /// The date some intervals later, or earlier for a negative number. A month or a year
    /// on from the 31st is the last day of a shorter month.
    pub(super) fn add(&self, interval: Interval, number: i64) -> Evaluated<f64> {
        let months = |months: i64| {
            let total = self.year * 12 + self.month - 1 + months;
            let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            let day = self
                .day
                .min(days(year, month + 1, 1) - days(year, month, 1));
            at(days(year, month, day), self.seconds)
        };
        let seconds = |seconds: i64| {
            let total = self.days * SECONDS_A_DAY + self.seconds + seconds;
            at(
                total.div_euclid(SECONDS_A_DAY),
                total.rem_euclid(SECONDS_A_DAY),
            )
        };
        let moved = match interval {
            Interval::Year => months(number * 12),
            Interval::Quarter => months(number * 3),
            Interval::Month => months(number),
            Interval::Day | Interval::Weekday => at(self.days + number, self.seconds),
            Interval::Week => at(self.days + number * 7, self.seconds),
            Interval::Hour => seconds(number * 3_600),
            Interval::Minute => seconds(number * 60),
            Interval::Second => seconds(number),
        };
        Self::of(moved).map(|parts| parts.value())
    }

    /// How many intervals lie between this date and a later one, negative for an earlier one,
    /// counted as VBA counts them: the year, quarter, month, day, hour, minute or second
    /// boundaries passed, the Sundays passed for calendar weeks, and whole weeks of days.
    pub(super) fn until(&self, other: &Self, interval: Interval) -> i64 {
        let seconds = |parts: &Self| parts.days * SECONDS_A_DAY + parts.seconds;
        let (from, to) = (seconds(self), seconds(other));
        let quarter = |parts: &Self| parts.year * 4 + (parts.month - 1) / 3;
        match interval {
            Interval::Year => other.year - self.year,
            Interval::Quarter => quarter(other) - quarter(self),
            Interval::Month => (other.year - self.year) * 12 + other.month - self.month,
            Interval::Day => other.days - self.days,
            Interval::Weekday => (other.days - self.days) / 7,
            Interval::Week => (other.days - 1).div_euclid(7) - (self.days - 1).div_euclid(7),
            Interval::Hour => to.div_euclid(3_600) - from.div_euclid(3_600),
            Interval::Minute => to.div_euclid(60) - from.div_euclid(60),
            Interval::Second => to - from,
        }
    }
}

/// The date of a year, month and day. The month and the day may run over, as in VBA's
/// `DateSerial`: month 13 is January of the year after, and day 0 the last of the month
/// before.
pub(super) fn date(year: i64, month: i64, day: i64) -> Evaluated<f64> {
    Parts::of(days(year, month, day) as f64).map(|parts| parts.value())
}

/// The date and time now, to the second, by the server's clock in UTC.
pub(super) fn now() -> f64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let seconds = since.map_or(0, |since| since.as_secs()) as i64;
    at(EPOCH, seconds)
}

/// A date written as yyyy-mm-dd, alone or with a time of day as hh:mm or hh:mm:ss after a
/// space or a T.
pub(super) fn parse(text: &str) -> Evaluated<f64> {
    let wrong = || fail(format!("'{text}' is not a date written as yyyy-mm-dd"));
    let (day, time) = match text.trim().split_once([' ', 'T']) {
        Some((day, time)) => (day, Some(time.trim())),
        None => (text.trim(), None),
    };
    let mut parts = day.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return wrong();
    };
    let (Some(year), Some(month), Some(day)) = (
        digits(year, 4..=4),
        digits(month, 1..=2),
        digits(day, 1..=2),
    ) else {
        return wrong();
    };
    if !(1..=12).contains(&month)
        || !(1..=days(year, month + 1, 1) - days(year, month, 1)).contains(&day)
    {
        return wrong();
    }
    let seconds = match time {
        None => 0,
        Some(time) => {
            let mut parts = time.split(':');
            let (Some(hour), Some(minute), second, None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return wrong();
            };
            let second = second.map_or(Some(0), |second| digits(second, 2..=2));
            let (Some(hour), Some(minute), Some(second)) =
                (digits(hour, 1..=2), digits(minute, 2..=2), second)
            else {
                return wrong();
            };
            if hour > 23 || minute > 59 || second > 59 {
                return wrong();
            }
            hour * 3_600 + minute * 60 + second
        }
    };
    Parts::of(at(days(year, month, day), seconds)).map(|parts| parts.value())
}

/// The number some decimal digits make, if there are as many of them as allowed.
fn digits(text: &str, allowed: std::ops::RangeInclusive<usize>) -> Option<i64> {
    let all = allowed.contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    all.then(|| text.parse().ok()).flatten()
}

/// A day and a time of day as a date.
fn at(days: i64, seconds: i64) -> f64 {
    days as f64 + seconds as f64 / SECONDS_A_DAY as f64
}

/// The day of a year, month and day, counted from 30 December 1899, with the month and the
/// day running over into the years and months after or before.
fn days(year: i64, month: i64, day: i64) -> i64 {
    let months = year * 12 + month - 1;
    let (year, month) = (months.div_euclid(12), months.rem_euclid(12) + 1);
    // Howard Hinnant's count of days from 1 January 1970, in eras of 400 years, each year
    // taken to begin on 1 March so that a leap day is the last of its year.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468 + EPOCH + day - 1
}

/// The year, month and day of a day counted from 1 January 1970: the count of `days` turned
/// back.
fn civil(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months counted from March.
    let from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * from_march + 2) / 5 + 1;
    let month = if from_march < 10 {
        from_march + 3
    } else {
        from_march - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}
