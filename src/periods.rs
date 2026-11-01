//! Reporting periods and the ones before them, for statements that set a period beside earlier
//! ones. A period of whole calendar months steps back by months (October is followed back by
//! September, a quarter by the quarter before it, whatever the days in them); any other period
//! steps back by its own length in days. A year back keeps the day, and keeps a month's end a
//! month's end (29 February 2024 is 28 February 2023).

use chrono::{Datelike, Duration, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    PreviousPeriod,
    PreviousYear,
}

impl Compare {
    pub fn parse(text: &str) -> Option<Compare> {
        match text.trim().to_lowercase().as_str() {
            "previous period" => Some(Compare::PreviousPeriod),
            "previous year" => Some(Compare::PreviousYear),
            _ => None,
        }
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

pub fn month_end(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    ymd(y, m, 1) - Duration::days(1)
}

fn is_month_end(d: NaiveDate) -> bool {
    month_end(d) == d
}

/// `d` moved by `months` (negative goes back); the day is kept, or the month's last when the
/// month is shorter. A month's last day stays a month's last day.
pub fn add_months(d: NaiveDate, months: i32) -> NaiveDate {
    let total = d.year() * 12 + d.month0() as i32 + months;
    let first = ymd(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1);
    if is_month_end(d) { month_end(first) } else { ymd(first.year(), first.month(), d.day().min(month_end(first).day())) }
}

/// How many whole calendar months `[from, to]` make, when it is exactly that.
fn whole_months(from: NaiveDate, to: NaiveDate) -> Option<i32> {
    (from.day() == 1 && is_month_end(to)).then(|| (to.year() - from.year()) * 12 + to.month() as i32 - from.month() as i32 + 1)
}

/// The period `back` steps before `[from, to]`.
pub fn before(from: NaiveDate, to: NaiveDate, how: Compare, back: u32) -> (NaiveDate, NaiveDate) {
    let back = back as i32;
    match how {
        Compare::PreviousYear => (add_months(from, -12 * back), add_months(to, -12 * back)),
        Compare::PreviousPeriod => match whole_months(from, to) {
            Some(n) => (add_months(from, -n * back), add_months(to, -n * back)),
            None => {
                let days = (to - from).num_days() + 1;
                (from - Duration::days(days * back as i64), to - Duration::days(days * back as i64))
            }
        },
    }
}

/// The date `back` steps before `on`, for a statement of one day: a month or a year earlier.
pub fn day_before(on: NaiveDate, how: Compare, back: u32) -> NaiveDate {
    add_months(on, -(if how == Compare::PreviousYear { 12 } else { 1 }) * back as i32)
}

/// A period in words: "Oct 2026", "Jul–Sep 2026", "Nov 2025 – Feb 2026", or its two dates.
pub fn label(from: NaiveDate, to: NaiveDate) -> String {
    match whole_months(from, to) {
        Some(1) => format!("{} {}", MONTHS[from.month0() as usize], from.year()),
        Some(_) if from.year() == to.year() => format!("{}–{} {}", MONTHS[from.month0() as usize], MONTHS[to.month0() as usize], to.year()),
        Some(_) => format!("{} {} – {} {}", MONTHS[from.month0() as usize], from.year(), MONTHS[to.month0() as usize], to.year()),
        None => format!("{from} to {to}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        ymd(y, m, day)
    }

    #[test]
    fn whole_months_step_back_by_months() {
        // October, then September; a quarter, then the quarter before it; a year, then the year before.
        assert_eq!(before(d(2026, 10, 1), d(2026, 10, 31), Compare::PreviousPeriod, 1), (d(2026, 9, 1), d(2026, 9, 30)));
        assert_eq!(before(d(2026, 7, 1), d(2026, 9, 30), Compare::PreviousPeriod, 1), (d(2026, 4, 1), d(2026, 6, 30)));
        assert_eq!(before(d(2026, 7, 1), d(2026, 9, 30), Compare::PreviousPeriod, 3), (d(2025, 10, 1), d(2025, 12, 31)));
        assert_eq!(before(d(2026, 4, 1), d(2027, 3, 31), Compare::PreviousPeriod, 1), (d(2025, 4, 1), d(2026, 3, 31)));
        // March follows February, which is shorter.
        assert_eq!(before(d(2026, 3, 1), d(2026, 3, 31), Compare::PreviousPeriod, 1), (d(2026, 2, 1), d(2026, 2, 28)));
    }

    #[test]
    fn other_periods_step_back_by_their_length_in_days() {
        // 10 to 19 October is ten days: 30 September to 9 October is the ten before.
        assert_eq!(before(d(2026, 10, 10), d(2026, 10, 19), Compare::PreviousPeriod, 1), (d(2026, 9, 30), d(2026, 10, 9)));
        assert_eq!(before(d(2026, 10, 10), d(2026, 10, 19), Compare::PreviousPeriod, 2), (d(2026, 9, 20), d(2026, 9, 29)));
    }

    #[test]
    fn a_year_back_keeps_the_day_and_a_months_end() {
        assert_eq!(before(d(2026, 10, 1), d(2026, 10, 31), Compare::PreviousYear, 1), (d(2025, 10, 1), d(2025, 10, 31)));
        assert_eq!(before(d(2024, 2, 1), d(2024, 2, 29), Compare::PreviousYear, 1), (d(2023, 2, 1), d(2023, 2, 28)));
        assert_eq!(before(d(2025, 2, 1), d(2025, 2, 28), Compare::PreviousYear, 1), (d(2024, 2, 1), d(2024, 2, 29)));
        assert_eq!(before(d(2026, 3, 15), d(2026, 6, 14), Compare::PreviousYear, 2), (d(2024, 3, 15), d(2024, 6, 14)));
    }

    #[test]
    fn a_day_steps_back_a_month_or_a_year() {
        assert_eq!(day_before(d(2026, 3, 31), Compare::PreviousPeriod, 1), d(2026, 2, 28));
        assert_eq!(day_before(d(2026, 3, 15), Compare::PreviousPeriod, 1), d(2026, 2, 15));
        assert_eq!(day_before(d(2026, 3, 31), Compare::PreviousYear, 1), d(2025, 3, 31));
        assert_eq!(day_before(d(2026, 1, 31), Compare::PreviousPeriod, 2), d(2025, 11, 30));
    }

    #[test]
    fn periods_are_named_for_people() {
        assert_eq!(label(d(2026, 10, 1), d(2026, 10, 31)), "Oct 2026");
        assert_eq!(label(d(2026, 7, 1), d(2026, 9, 30)), "Jul–Sep 2026");
        assert_eq!(label(d(2025, 11, 1), d(2026, 2, 28)), "Nov 2025 – Feb 2026");
        assert_eq!(label(d(2026, 10, 10), d(2026, 10, 19)), "2026-10-10 to 2026-10-19");
    }

    #[test]
    fn the_words_for_a_comparison_are_known() {
        assert_eq!(Compare::parse("Previous period"), Some(Compare::PreviousPeriod));
        assert_eq!(Compare::parse(" previous YEAR "), Some(Compare::PreviousYear));
        assert_eq!(Compare::parse("last quarter"), None);
    }
}
