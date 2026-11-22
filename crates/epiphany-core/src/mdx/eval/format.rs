//! Numbers and dates written out as a pattern says, as VBA's `Format` writes them.
//!
//! A pattern with any of the letters for a part of a date writes a date: `yyyy` and `yy` the
//! year, `q` the quarter, `mmmm` and `mmm` the month's name, `mm` and `m` its number, `dddd`
//! and `ddd` the day's name, `dd` and `d` its number, `y` the day of the year, `ww` the week
//! of the year, `w` the day of the week, `hh` and `h` the hour, `nn` and `n` the minute (and
//! `mm` or `m` straight after an hour), `ss` and `s` the second, and `AM/PM`, `am/pm`, `A/P`
//! or `a/p` the half of the day, which makes the hours count to twelve. Names are English.
//!
//! Any other pattern writes a number: `0` for a digit always written and `#` for one written
//! only where the number has it, `.` before the decimals, `,` among the digits for thousands
//! written apart and at their end for thousands left off, and `%` for a percentage. Up to
//! three sections, apart by `;`, are for positive, negative and zero values; a section left
//! empty is the first.
//!
//! In either, text in quotes, a character after a backslash, and any character that means
//! nothing else are written as they stand.

use super::dates::{MONTHS, Parts, WEEKDAYS};
use super::{Evaluated, fail};

/// The most decimals a pattern may ask for.
const MOST_DECIMALS: usize = 20;

/// A piece of a pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece<'a> {
    /// Written as it stands.
    Text(&'a str),
    /// A run of one of the letters for the parts of a date, in lower case, and its length.
    Letters(char, usize),
    /// A mark for the half of the day, as the pattern writes it: `AM/PM` and the like.
    Half(&'a str),
    /// One of the marks of a number pattern, or the `;` between its sections.
    Mark(char),
}

/// A number written out as a pattern says: as a date where the pattern has letters for the
/// parts of one, and as briefly as it can be where the pattern is empty.
pub(super) fn write(number: f64, pattern: &str) -> Evaluated<String> {
    let pieces = pieces(pattern);
    match (pattern.is_empty(), for_dates(&pieces)) {
        (true, _) => Ok(number.to_string()),
        (false, true) => Ok(date(&Parts::of(number)?, &pieces)),
        (false, false) => self::number(number, &pieces),
    }
}

/// Whether a pattern writes out a date.
pub(super) fn writes_dates(pattern: &str) -> bool {
    for_dates(&pieces(pattern))
}

fn for_dates(pieces: &[Piece]) -> bool {
    let of_dates = |piece: &Piece| matches!(piece, Piece::Letters(..) | Piece::Half(_));
    pieces.iter().any(of_dates)
}

fn pieces(pattern: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = pattern;
    while let Some(first) = rest.chars().next() {
        let starts = |mark: &str| {
            rest.get(..mark.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(mark))
        };
        let (piece, length) = match first {
            '"' => match rest[1..].find('"') {
                Some(end) => (Piece::Text(&rest[1..=end]), end + 2),
                None => (Piece::Text(&rest[1..]), rest.len()),
            },
            '\\' => {
                let next = rest[1..].chars().next().map_or(0, char::len_utf8);
                (Piece::Text(&rest[1..1 + next]), 1 + next)
            }
            _ if starts("am/pm") => (Piece::Half(&rest[..5]), 5),
            _ if starts("a/p") => (Piece::Half(&rest[..3]), 3),
            _ if "ymdhnsqw".contains(first.to_ascii_lowercase()) => {
                let run = rest
                    .chars()
                    .take_while(|c| c.eq_ignore_ascii_case(&first))
                    .count();
                (Piece::Letters(first.to_ascii_lowercase(), run), run)
            }
            '0' | '#' | '.' | ',' | '%' | ';' => (Piece::Mark(first), 1),
            _ => (Piece::Text(&rest[..first.len_utf8()]), first.len_utf8()),
        };
        pieces.push(piece);
        rest = &rest[length..];
    }
    pieces
}

/// A date written out as the pieces of a pattern say.
fn date(parts: &Parts, pieces: &[Piece]) -> String {
    let twelve = pieces.iter().any(|piece| matches!(piece, Piece::Half(_)));
    let hour = parts.seconds / 3_600;
    let (minute, second) = (parts.seconds / 60 % 60, parts.seconds % 60);
    let mut written = String::new();
    // An `m` straight after an hour is the minute, not the month.
    let mut after_hour = false;
    for &piece in pieces {
        let (letter, mut run) = match piece {
            Piece::Text(text) => {
                written.push_str(text);
                continue;
            }
            Piece::Mark(mark) => {
                written.push(mark);
                continue;
            }
            Piece::Half(half) => {
                let (morning, afternoon) = half.split_once('/').unwrap_or((half, half));
                written.push_str(if hour < 12 { morning } else { afternoon });
                continue;
            }
            Piece::Letters(letter, run) => (letter, run),
        };
        // A run longer than any part is taken as more than one part.
        while run > 0 {
            let lengths: &[usize] = match letter {
                'y' => &[4, 2, 1],
                'm' | 'd' => &[4, 3, 2, 1],
                'q' => &[1],
                _ => &[2, 1],
            };
            let taken = lengths.iter().copied().find(|&length| length <= run);
            let taken = taken.unwrap_or(1);
            run -= taken;
            let both = |number: i64, two: bool| match two {
                true => format!("{number:02}"),
                false => number.to_string(),
            };
            let month = MONTHS[parts.month as usize - 1];
            let weekday = WEEKDAYS[parts.weekday(1) as usize - 1];
            let part = match (letter, taken) {
                ('y', 4) => format!("{:04}", parts.year),
                ('y', 2) => both(parts.year.rem_euclid(100), true),
                ('y', _) => parts.day_of_year().to_string(),
                ('q', _) => ((parts.month - 1) / 3 + 1).to_string(),
                ('m', 1 | 2) if after_hour => both(minute, taken == 2),
                ('m', 4) => month.to_owned(),
                ('m', 3) => month[..3].to_owned(),
                ('m', _) => both(parts.month, taken == 2),
                ('d', 4) => weekday.to_owned(),
                ('d', 3) => weekday[..3].to_owned(),
                ('d', _) => both(parts.day, taken == 2),
                ('w', 2) => parts.week_of_year().to_string(),
                ('w', _) => parts.weekday(1).to_string(),
                ('h', _) if twelve => both((hour + 11) % 12 + 1, taken == 2),
                ('h', _) => both(hour, taken == 2),
                ('n', _) => both(minute, taken == 2),
                (_, _) => both(second, taken == 2),
            };
            written.push_str(&part);
            after_hour = letter == 'h';
        }
    }
    written
}

/// A number written out as the pieces of a pattern say.
fn number(number: f64, pieces: &[Piece]) -> Evaluated<String> {
    let sections: Vec<&[Piece]> = pieces.split(|&piece| piece == Piece::Mark(';')).collect();
    let chosen = |index: usize| sections.get(index).filter(|section| !section.is_empty());
    let (section, signed) = match (number < 0.0, number == 0.0) {
        (true, _) if chosen(1).is_some() => (chosen(1), false),
        (_, true) if chosen(2).is_some() => (chosen(2), false),
        (negative, _) => (sections.first(), negative),
    };
    let layout = Layout::of(section.copied().unwrap_or_default())?;
    let scaled = number.abs() * 100f64.powi(layout.percents) / 1_000f64.powi(layout.thousands);
    let units = (scaled * 10f64.powi(layout.decimals as i32)).round();
    if !units.is_finite() {
        return fail("the number is too large to write out");
    }
    let digits = format!("{units:0>width$.0}", width = layout.decimals + 1);
    let (whole, fraction) = digits.split_at(digits.len() - layout.decimals);
    let fraction = &fraction[..fraction.trim_end_matches('0').len().max(layout.least)];
    let whole = format!(
        "{:0>width$}",
        whole.trim_start_matches('0'),
        width = layout.zeros
    );
    let mut written = String::new();
    if signed && units != 0.0 {
        written.push('-');
    }
    written.push_str(&layout.before);
    if layout.digits {
        written.push_str(&match layout.grouped {
            true => grouped(&whole),
            false => whole,
        });
    }
    if layout.point {
        written.push('.');
        written.push_str(fraction);
    }
    written.push_str(&layout.after);
    Ok(written)
}

/// Digits with a comma before each three from the right.
fn grouped(digits: &str) -> String {
    let mut written = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            written.push(',');
        }
        written.push(digit);
    }
    written
}

/// How a section of a number pattern lays a number out.
#[derive(Debug, Default)]
struct Layout {
    /// The text before the digits, and after them.
    before: String,
    after: String,
    /// Whether there are digits to write.
    digits: bool,
    /// How many digits are written before the point at least.
    zeros: usize,
    /// Whether thousands are written apart.
    grouped: bool,
    /// How many times the number is shown in thousands.
    thousands: i32,
    /// Whether there is a point, how many decimals at most, and how many always.
    point: bool,
    decimals: usize,
    least: usize,
    /// How many times the number is shown as a percentage.
    percents: i32,
}

/// Where in a section of a number pattern a piece stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Before,
    Whole,
    Fraction,
    After,
}

impl Layout {
    fn of(section: &[Piece]) -> Evaluated<Self> {
        let mut layout = Self::default();
        let mut at = At::Before;
        // Commas among the digits before the point write thousands apart; those at their end
        // leave thousands off.
        let mut commas = 0;
        for &piece in section {
            let mut mark = [0; 4];
            let text = match piece {
                Piece::Mark(digit @ ('0' | '#')) => {
                    match at {
                        At::Before | At::Whole => {
                            at = At::Whole;
                            layout.grouped |= std::mem::take(&mut commas) > 0;
                            layout.zeros += usize::from(digit == '0');
                        }
                        At::Fraction => {
                            layout.decimals += 1;
                            if digit == '0' {
                                layout.least = layout.decimals;
                            }
                        }
                        At::After => {
                            let why = "a number pattern has its text before or after its \
                                       digits, not among them";
                            return fail(why);
                        }
                    }
                    layout.digits = true;
                    continue;
                }
                Piece::Mark('.') if matches!(at, At::Before | At::Whole) => {
                    layout.thousands += std::mem::take(&mut commas);
                    layout.point = true;
                    at = At::Fraction;
                    continue;
                }
                Piece::Mark(',') if at == At::Whole => {
                    commas += 1;
                    continue;
                }
                Piece::Mark(other) => {
                    layout.percents += i32::from(other == '%');
                    &*other.encode_utf8(&mut mark)
                }
                Piece::Text(text) => text,
                // A pattern with letters for the parts of a date writes a date instead.
                Piece::Letters(..) | Piece::Half(_) => continue,
            };
            match at {
                At::Before => layout.before.push_str(text),
                _ => {
                    layout.thousands += std::mem::take(&mut commas);
                    at = At::After;
                    layout.after.push_str(text);
                }
            }
        }
        layout.thousands += commas;
        if layout.decimals > MOST_DECIMALS {
            return fail(format!(
                "a number pattern writes at most {MOST_DECIMALS} decimals"
            ));
        }
        Ok(layout)
    }
}
