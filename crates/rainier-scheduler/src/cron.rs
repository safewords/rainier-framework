//! Cron expressions — [`CronExpression`].
//!
//! The five-field form, as `crontab(5)` has it:
//!
//! ```text
//! ┌───────── minute        0-59
//! │ ┌─────── hour          0-23
//! │ │ ┌───── day of month  1-31
//! │ │ │ ┌─── month         1-12 or JAN-DEC
//! │ │ │ │ ┌─ day of week   0-6  or SUN-SAT (7 is also Sunday)
//! │ │ │ │ │
//! * * * * *
//! ```
//!
//! Each field takes `*`, a number, a `a-b` range, a `a,b,c` list, and a `/n`
//! step on any of those. The usual `@hourly`, `@daily`, `@weekly`, `@monthly`,
//! `@yearly` and `@midnight` shorthands are accepted too.
//!
//! ```
//! # use rainier_scheduler::CronExpression;
//! # use chrono::{TimeZone, Utc};
//! let weekdays_at_nine = CronExpression::parse("0 9 * * 1-5").unwrap();
//!
//! // Monday 09:00.
//! assert!(weekdays_at_nine.matches(Utc.with_ymd_and_hms(2026, 7, 27, 9, 0, 0).unwrap()));
//! // Sunday 09:00.
//! assert!(!weekdays_at_nine.matches(Utc.with_ymd_and_hms(2026, 7, 26, 9, 0, 0).unwrap()));
//! ```
//!
//! ## Seconds are not a field
//!
//! Five fields, not six. The scheduler is driven by a process that wakes once a
//! minute, so a sub-minute expression would be a promise it cannot keep — and
//! the failure mode of accepting one is a task that looks scheduled every ten
//! seconds and runs once a minute.
//!
//! ## Day-of-month and day-of-week are OR, not AND
//!
//! `0 0 13 * 5` is "the 13th, **and also** every Friday", not "Friday the
//! 13th". That is what cron does, it surprises everybody once, and matching it
//! is more useful than being quietly different.
//!
//! The rule only applies when both are restricted. If either is `*`, the other
//! decides alone — otherwise `0 0 13 * *` would fire every day.

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, TimeZone, Timelike};
use rainier_support::{Error, Result};

/// A parsed cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpression {
    source: String,
    minutes: Field,
    hours: Field,
    days_of_month: Field,
    months: Field,
    days_of_week: Field,
}

/// One field: the set of values it matches, and whether it was `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    values: BTreeSet<u32>,
    /// `*` — needed because day-of-month and day-of-week combine differently
    /// when one of them is unrestricted.
    wildcard: bool,
}

impl Field {
    fn matches(&self, value: u32) -> bool {
        self.values.contains(&value)
    }
}

impl CronExpression {
    /// Parse a five-field expression, or one of the `@` shorthands.
    pub fn parse(expression: &str) -> Result<Self> {
        let trimmed = expression.trim();

        if let Some(shorthand) = expand_shorthand(trimmed) {
            return Self::parse_fields(trimmed, shorthand);
        }

        Self::parse_fields(trimmed, trimmed)
    }

    fn parse_fields(source: &str, expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();

        if fields.len() != 5 {
            return Err(Error::internal(format!(
                "`{source}` has {} field(s); a cron expression has 5 \
                 (minute hour day-of-month month day-of-week)",
                fields.len()
            )));
        }

        Ok(Self {
            source: source.to_string(),
            minutes: parse_field(source, fields[0], 0, 59, &[])?,
            hours: parse_field(source, fields[1], 0, 23, &[])?,
            days_of_month: parse_field(source, fields[2], 1, 31, &[])?,
            months: parse_field(source, fields[3], 1, 12, MONTHS)?,
            days_of_week: parse_day_of_week(source, fields[4])?,
        })
    }

    /// Whether the expression fires at `at`.
    ///
    /// Seconds are ignored: the expression has minute resolution, so anything
    /// within the matching minute matches.
    pub fn matches<Tz: TimeZone>(&self, at: DateTime<Tz>) -> bool {
        if !self.minutes.matches(at.minute())
            || !self.hours.matches(at.hour())
            || !self.months.matches(at.month())
        {
            return false;
        }

        let dom = self.days_of_month.matches(at.day());
        let dow = self.days_of_week.matches(at.weekday().num_days_from_sunday());

        // cron's one genuine oddity — see the module docs.
        match (self.days_of_month.wildcard, self.days_of_week.wildcard) {
            (true, _) | (_, true) => dom && dow,
            (false, false) => dom || dow,
        }
    }

    /// The expression as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The next time at or after `from` that this fires, if there is one within
    /// four years.
    ///
    /// Four years rather than forever because `0 0 30 2 *` — the 30th of
    /// February — parses fine and never fires, and a search with no bound would
    /// hang rather than say so. Four covers a leap year in every position.
    pub fn next_after<Tz: TimeZone>(&self, from: DateTime<Tz>) -> Option<DateTime<Tz>> {
        // Minute resolution, so start from the next whole minute.
        let mut candidate = from.with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);

        for _ in 0..(4 * 366 * 24 * 60) {
            if self.matches(candidate.clone()) {
                return Some(candidate);
            }
            candidate += chrono::Duration::minutes(1);
        }

        None
    }
}

impl std::fmt::Display for CronExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source)
    }
}

impl std::str::FromStr for CronExpression {
    type Err = Error;

    fn from_str(expression: &str) -> Result<Self> {
        Self::parse(expression)
    }
}

const MONTHS: &[&str] =
    &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

const DAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn expand_shorthand(expression: &str) -> Option<&'static str> {
    match expression.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => Some("0 0 1 1 *"),
        "@monthly" => Some("0 0 1 * *"),
        "@weekly" => Some("0 0 * * 0"),
        "@daily" | "@midnight" => Some("0 0 * * *"),
        "@hourly" => Some("0 * * * *"),
        _ => None,
    }
}

/// Parse one field into the set of values it matches.
fn parse_field(source: &str, field: &str, min: u32, max: u32, names: &[&str]) -> Result<Field> {
    let mut values = BTreeSet::new();
    let wildcard = field == "*";

    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(invalid(source, field, "an empty entry in a list"));
        }

        // `a-b/n`, `*/n`, or a plain value.
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .map_err(|_| invalid(source, field, &format!("`{step}` is not a step")))?;
                if step == 0 {
                    return Err(invalid(source, field, "a step of zero"));
                }
                (range, step)
            }
            None => (part, 1),
        };

        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            (value_of(source, field, start, names)?, value_of(source, field, end, names)?)
        } else {
            let single = value_of(source, field, range, names)?;
            // `5/15` means "from 5 onwards, every 15" — a bare value with a
            // step is a range to the end of the field, not one value.
            if step > 1 {
                (single, max)
            } else {
                (single, single)
            }
        };

        if from > to {
            return Err(invalid(source, field, &format!("`{from}-{to}` runs backwards")));
        }
        if from < min || to > max {
            return Err(invalid(source, field, &format!("`{from}-{to}` is outside {min}-{max}")));
        }

        values.extend((from..=to).step_by(step as usize));
    }

    if values.is_empty() {
        return Err(invalid(source, field, "it matches nothing"));
    }

    Ok(Field { values, wildcard })
}

/// Day-of-week, where 7 is another name for Sunday.
fn parse_day_of_week(source: &str, field: &str) -> Result<Field> {
    let parsed = parse_field(source, field, 0, 7, DAYS)?;

    // Normalise 7 to 0 so `matches` can compare against
    // `num_days_from_sunday()` without special-casing.
    let mut values: BTreeSet<u32> = parsed.values.iter().map(|d| d % 7).collect();
    values.retain(|d| *d <= 6);

    Ok(Field { values, wildcard: parsed.wildcard })
}

fn value_of(source: &str, field: &str, raw: &str, names: &[&str]) -> Result<u32> {
    let raw = raw.trim();

    if let Ok(number) = raw.parse::<u32>() {
        return Ok(number);
    }

    let lower = raw.to_ascii_lowercase();
    match names.iter().position(|name| *name == lower) {
        // Month names are 1-based, day names 0-based, which is exactly the
        // offset each table already encodes.
        Some(index) if std::ptr::eq(names.as_ptr(), MONTHS.as_ptr()) => Ok(index as u32 + 1),
        Some(index) => Ok(index as u32),
        None => Err(invalid(source, field, &format!("`{raw}` is not a number or a name"))),
    }
}

fn invalid(source: &str, field: &str, why: &str) -> Error {
    Error::internal(format!("`{source}`: the field `{field}` has {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn fires(expression: &str, when: DateTime<Utc>) -> bool {
        CronExpression::parse(expression).unwrap().matches(when)
    }

    #[test]
    fn every_minute_matches_everything() {
        assert!(fires("* * * * *", at(2026, 7, 27, 13, 41)));
    }

    #[test]
    fn a_fixed_time_matches_only_that_minute() {
        assert!(fires("30 2 * * *", at(2026, 7, 27, 2, 30)));
        assert!(!fires("30 2 * * *", at(2026, 7, 27, 2, 31)));
        assert!(!fires("30 2 * * *", at(2026, 7, 27, 3, 30)));
    }

    #[test]
    fn a_step_matches_every_nth() {
        let every_fifteen = CronExpression::parse("*/15 * * * *").unwrap();

        for minute in [0, 15, 30, 45] {
            assert!(every_fifteen.matches(at(2026, 7, 27, 0, minute)), "{minute}");
        }
        for minute in [1, 14, 16, 44, 59] {
            assert!(!every_fifteen.matches(at(2026, 7, 27, 0, minute)), "{minute}");
        }
    }

    #[test]
    fn a_bare_value_with_a_step_runs_to_the_end_of_the_field() {
        // `5/20` is "from 5, every 20" — 5, 25, 45 — not just 5.
        let expression = CronExpression::parse("5/20 * * * *").unwrap();

        for minute in [5, 25, 45] {
            assert!(expression.matches(at(2026, 7, 27, 0, minute)), "{minute}");
        }
        assert!(!expression.matches(at(2026, 7, 27, 0, 6)));
    }

    #[test]
    fn a_range_is_inclusive_at_both_ends() {
        let nine_to_five = CronExpression::parse("0 9-17 * * *").unwrap();

        assert!(nine_to_five.matches(at(2026, 7, 27, 9, 0)));
        assert!(nine_to_five.matches(at(2026, 7, 27, 17, 0)));
        assert!(!nine_to_five.matches(at(2026, 7, 27, 18, 0)));
    }

    #[test]
    fn a_list_matches_any_entry() {
        let expression = CronExpression::parse("0 0,6,12,18 * * *").unwrap();

        for hour in [0, 6, 12, 18] {
            assert!(expression.matches(at(2026, 7, 27, hour, 0)), "{hour}");
        }
        assert!(!expression.matches(at(2026, 7, 27, 7, 0)));
    }

    #[test]
    fn month_and_day_names_work() {
        // 2026-12-25 is a Friday.
        assert!(fires("0 0 25 DEC *", at(2026, 12, 25, 0, 0)));
        assert!(fires("0 9 * * MON-FRI", at(2026, 7, 27, 9, 0)));
        assert!(!fires("0 9 * * MON-FRI", at(2026, 7, 26, 9, 0)));
    }

    #[test]
    fn seven_is_another_name_for_sunday() {
        // 2026-07-26 is a Sunday.
        assert!(fires("0 0 * * 7", at(2026, 7, 26, 0, 0)));
        assert!(fires("0 0 * * 0", at(2026, 7, 26, 0, 0)));
    }

    #[test]
    fn day_of_month_and_day_of_week_are_or_when_both_are_restricted() {
        // cron's famous oddity: `0 0 13 * 5` is the 13th *or* any Friday.
        // 2026-08-13 is a Thursday, 2026-08-14 a Friday.
        let expression = CronExpression::parse("0 0 13 * 5").unwrap();

        assert!(expression.matches(at(2026, 8, 13, 0, 0)), "the 13th, though a Thursday");
        assert!(expression.matches(at(2026, 8, 14, 0, 0)), "a Friday, though not the 13th");
        assert!(!expression.matches(at(2026, 8, 12, 0, 0)), "neither");
    }

    #[test]
    fn one_being_a_wildcard_makes_the_other_decide_alone() {
        // Otherwise `0 0 13 * *` would fire every day, which is the bug the OR
        // rule causes if applied unconditionally.
        let thirteenth = CronExpression::parse("0 0 13 * *").unwrap();

        assert!(thirteenth.matches(at(2026, 8, 13, 0, 0)));
        assert!(!thirteenth.matches(at(2026, 8, 14, 0, 0)));
    }

    #[test]
    fn the_shorthands_expand() {
        assert!(fires("@daily", at(2026, 7, 27, 0, 0)));
        assert!(!fires("@daily", at(2026, 7, 27, 0, 1)));

        assert!(fires("@hourly", at(2026, 7, 27, 13, 0)));
        // 2026-07-26 is a Sunday.
        assert!(fires("@weekly", at(2026, 7, 26, 0, 0)));
        assert!(fires("@monthly", at(2026, 8, 1, 0, 0)));
        assert!(fires("@yearly", at(2026, 1, 1, 0, 0)));
    }

    #[test]
    fn the_wrong_number_of_fields_says_how_many_it_found() {
        let err = CronExpression::parse("* * *").unwrap_err();

        assert!(err.message().contains("3 field(s)"), "{}", err.message());
        assert!(err.message().contains("has 5"), "{}", err.message());
    }

    #[test]
    fn an_out_of_range_value_names_the_field_and_the_range() {
        let err = CronExpression::parse("0 25 * * *").unwrap_err();

        assert!(
            err.message().contains("`25`") || err.message().contains("25-25"),
            "{}",
            err.message()
        );
        assert!(err.message().contains("0-23"), "{}", err.message());
    }

    #[test]
    fn nonsense_is_rejected_rather_than_silently_never_firing() {
        for expression in ["* * * * nope", "*/0 * * * *", "5-1 * * * *", "", "0 0 * *"] {
            assert!(CronExpression::parse(expression).is_err(), "`{expression}` should not parse");
        }
    }

    #[test]
    fn next_after_finds_the_following_occurrence() {
        let daily = CronExpression::parse("0 3 * * *").unwrap();

        let next = daily.next_after(at(2026, 7, 27, 4, 0)).unwrap();
        assert_eq!(next, at(2026, 7, 28, 3, 0));
    }

    #[test]
    fn next_after_skips_the_current_minute() {
        // Otherwise a scheduler asking "when next" while a task is running
        // would answer "now" forever.
        let daily = CronExpression::parse("0 3 * * *").unwrap();

        assert_eq!(daily.next_after(at(2026, 7, 27, 3, 0)).unwrap(), at(2026, 7, 28, 3, 0));
    }

    #[test]
    fn an_expression_that_can_never_fire_gives_up_rather_than_hanging() {
        // The 30th of February parses and matches nothing.
        let never = CronExpression::parse("0 0 30 2 *").unwrap();

        assert!(never.next_after(at(2026, 7, 27, 0, 0)).is_none());
    }

    #[test]
    fn it_round_trips_through_its_source() {
        let expression = CronExpression::parse("*/5 9-17 * * MON-FRI").unwrap();

        assert_eq!(expression.to_string(), "*/5 9-17 * * MON-FRI");
        assert_eq!(CronExpression::parse(&expression.to_string()).unwrap(), expression);
    }
}
