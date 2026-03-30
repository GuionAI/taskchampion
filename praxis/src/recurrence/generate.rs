use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};

use super::spec::RecurrenceSpec;

/// The result of `generate_due_dates`.
pub struct GeneratedDates {
    /// All generated due dates (past + up to `future_limit` future instances).
    pub dates: Vec<DateTime<Utc>>,
    /// True if the `until` boundary was reached during generation, meaning the
    /// template can be considered expired once all slots are filled.
    pub until_reached: bool,
    /// True if generation stopped at the `MAX_ITERATIONS` safety cap rather than
    /// by a normal termination condition. Indicates a potential data issue.
    pub hit_limit: bool,
}

/// Compute the next due date after `current` for the given spec.
///
/// Returns `None` on arithmetic overflow or if the spec cannot advance.
///
/// Note: all date arithmetic uses UTC. Weekday-skip logic operates on UTC
/// day-of-week — this is intentional.
pub fn next_due_date(spec: &RecurrenceSpec, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
    use RecurrenceSpec::*;

    match spec {
        Daily => current.checked_add_signed(Duration::days(1)),

        Weekdays => {
            // Skip weekend: Fri→Mon (+3d), Sat→Mon (+2d), Sun→Mon (+1d), else +1d
            let days = match current.weekday() {
                Weekday::Fri => 3,
                Weekday::Sat => 2,
                Weekday::Sun => 1,
                _ => 1,
            };
            current.checked_add_signed(Duration::days(days))
        }

        Weekly => current.checked_add_signed(Duration::days(7)),
        Biweekly => current.checked_add_signed(Duration::days(14)),

        Monthly => add_months(current, 1),
        Bimonthly => add_months(current, 2),
        Quarterly => add_months(current, 3),
        Semiannual => add_months(current, 6),
        Annual => add_years(current, 1),
        Biannual => add_years(current, 2),

        NMonths(n) => add_months(current, *n),
        NQuarters(n) => add_months(current, n * 3),
        NYears(n) => add_years(current, *n),
        NDays(n) => current.checked_add_signed(Duration::days(*n as i64)),
        NWeeks(n) => current.checked_add_signed(Duration::weeks(*n as i64)),

        IsoMonths(n) => add_months(current, *n),
        IsoYears(n) => add_years(current, *n),
        IsoDays(n) => current.checked_add_signed(Duration::days(*n as i64)),
        IsoWeeks(n) => current.checked_add_signed(Duration::weeks(*n as i64)),

        Seconds(secs) => {
            let delta = Duration::try_seconds(*secs)?;
            current.checked_add_signed(delta)
        }
    }
}

/// Add `n` calendar months to `dt`, rolling back to the last valid day of the
/// month if the original day doesn't exist in the target month.
///
/// Examples:
/// - Jan 31 + 1 month → Feb 28 (or Feb 29 in leap year)
/// - Mar 31 + 1 month → Apr 30
fn add_months(dt: DateTime<Utc>, n: u32) -> Option<DateTime<Utc>> {
    let year = dt.year();
    let month = dt.month() as i32;
    let day = dt.day();

    let total_months = month + n as i32 - 1;
    let new_year = year.checked_add(total_months / 12)?;
    let new_month = (total_months % 12 + 1) as u32;

    // Clamp day to the last valid day of the target month.
    let max_day = days_in_month(new_year, new_month)?;
    let new_day = day.min(max_day);

    // Build via NaiveDate to avoid intermediate invalid states (e.g. day 31 in February).
    let naive_date = NaiveDate::from_ymd_opt(new_year, new_month, new_day)?;
    let naive_dt = naive_date.and_time(dt.time());
    Some(DateTime::from_naive_utc_and_offset(naive_dt, Utc))
}

/// Add `n` calendar years to `dt`, rolling back Feb 29 to Feb 28 in non-leap years.
fn add_years(dt: DateTime<Utc>, n: u32) -> Option<DateTime<Utc>> {
    let new_year = dt.year().checked_add(n as i32)?;
    let month = dt.month();
    let day = dt.day();

    // Feb 29 in a non-leap year → Feb 28.
    let new_day = if month == 2 && day == 29 && !is_leap_year(new_year) {
        28
    } else {
        day
    };

    let naive_date = NaiveDate::from_ymd_opt(new_year, month, new_day)?;
    let naive_dt = naive_date.and_time(dt.time());
    Some(DateTime::from_naive_utc_and_offset(naive_dt, Utc))
}

/// Returns the number of days in the given month, or `None` for invalid months.
fn days_in_month(year: i32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Generate all due dates from `base_due` forward.
///
/// - `future_limit`: max number of future instances (dates where `date > now`)
///   to generate. Matches tw's `recurrence.limit` config (default 1).
/// - `until`: optional expiry boundary — generate dates on or before `until`.
/// - Returns a `GeneratedDates` with:
///   - `dates`: all generated due dates (past + up to `future_limit` future)
///   - `until_reached`: true if the `until` boundary was passed during generation
///   - `hit_limit`: true if generation stopped at the `MAX_ITERATIONS` safety cap
pub fn generate_due_dates(
    spec: &RecurrenceSpec,
    base_due: DateTime<Utc>,
    now: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    future_limit: usize,
) -> GeneratedDates {
    let mut dates = Vec::new();
    let mut current = base_due;
    let mut future_count = 0usize;
    let mut until_reached = false;
    let mut hit_limit = false;

    // Safety cap to prevent infinite loops on bad input.
    const MAX_ITERATIONS: usize = 10_000;
    let mut iterations = 0;

    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            hit_limit = true;
            break;
        }

        // Check until boundary (inclusive).
        if let Some(until_dt) = until {
            if current > until_dt {
                until_reached = true;
                break;
            }
        }

        // Check future limit before pushing: stop if we've already consumed the quota.
        if current > now {
            if future_count >= future_limit {
                break;
            }
            future_count += 1;
        }

        dates.push(current);

        // Advance to next date.
        match next_due_date(spec, current) {
            Some(next) => {
                if next <= current {
                    // Overflow or no-op — stop.
                    break;
                }
                current = next;
            }
            None => break,
        }
    }

    GeneratedDates {
        dates,
        until_reached,
        hit_limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use pretty_assertions::assert_eq;

    fn dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn monthly_day_rollback() {
        // Jan 31 → Feb 28 → Mar 31 → Apr 30
        let spec = RecurrenceSpec::Monthly;
        let d1 = dt(2024, 1, 31);
        let d2 = next_due_date(&spec, d1).unwrap();
        assert_eq!(d2, dt(2024, 2, 29)); // 2024 is leap year
        let d3 = next_due_date(&spec, d2).unwrap();
        assert_eq!(d3, dt(2024, 3, 29)); // from Feb 29
                                         // Start fresh from Jan 31 2025 (non-leap)
        let d4 = next_due_date(&spec, dt(2025, 1, 31)).unwrap();
        assert_eq!(d4, dt(2025, 2, 28));
        let d5 = next_due_date(&spec, d4).unwrap();
        assert_eq!(d5, dt(2025, 3, 28)); // from Feb 28
    }

    #[test]
    fn monthly_apr_30() {
        let spec = RecurrenceSpec::Monthly;
        let d = next_due_date(&spec, dt(2024, 3, 31)).unwrap();
        assert_eq!(d, dt(2024, 4, 30));
    }

    #[test]
    fn annual_leap_to_nonleap() {
        let spec = RecurrenceSpec::Annual;
        let d = next_due_date(&spec, dt(2024, 2, 29)).unwrap();
        assert_eq!(d, dt(2025, 2, 28));
        let d2 = next_due_date(&spec, dt(2025, 2, 28)).unwrap();
        assert_eq!(d2, dt(2026, 2, 28));
        // Leap year: 2028
        let d3 = next_due_date(&spec, dt(2027, 2, 28)).unwrap();
        assert_eq!(d3, dt(2028, 2, 28));
    }

    #[test]
    fn weekdays_skip() {
        // Friday → Monday (+3)
        let fri = dt(2024, 3, 1); // 2024-03-01 is Friday
        assert_eq!(fri.weekday(), Weekday::Fri);
        let mon = next_due_date(&RecurrenceSpec::Weekdays, fri).unwrap();
        assert_eq!(mon, dt(2024, 3, 4));
        assert_eq!(mon.weekday(), Weekday::Mon);

        // Mon → Tue
        let tue = next_due_date(&RecurrenceSpec::Weekdays, mon).unwrap();
        assert_eq!(tue, dt(2024, 3, 5));
    }

    #[test]
    fn nmonths_quarterly() {
        let spec = RecurrenceSpec::NMonths(3);
        let d = next_due_date(&spec, dt(2024, 1, 15)).unwrap();
        assert_eq!(d, dt(2024, 4, 15));
    }

    #[test]
    fn generate_until_boundary_inclusive() {
        let spec = RecurrenceSpec::Monthly;
        let base = dt(2024, 1, 1);
        let now = dt(2023, 1, 1); // all dates are future
        let until = dt(2024, 3, 1); // inclusive
        let result = generate_due_dates(&spec, base, now, Some(until), 10);
        assert_eq!(
            result.dates,
            vec![dt(2024, 1, 1), dt(2024, 2, 1), dt(2024, 3, 1)]
        );
        assert!(result.until_reached);
        assert!(!result.hit_limit);
    }

    #[test]
    fn generate_future_limit() {
        let spec = RecurrenceSpec::Monthly;
        let base = dt(2024, 1, 1);
        let now = dt(2024, 1, 1); // base is exactly now, not future
                                  // future_limit=1: only generate up to 1 future date
        let result = generate_due_dates(&spec, base, now, None, 1);
        // base_due is not > now, so first future is Feb 1
        assert_eq!(result.dates.len(), 2); // Jan 1 (= now, not future) + Feb 1 (1 future)
    }

    #[test]
    fn generate_future_limit_zero() {
        let spec = RecurrenceSpec::Monthly;
        let base = dt(2024, 1, 1);
        let now = dt(2024, 6, 1);
        // future_limit=0: only past/current dates
        let result = generate_due_dates(&spec, base, now, None, 0);
        // Jan through Jun are all <= now
        for d in &result.dates {
            assert!(*d <= now);
        }
        assert!(!result.hit_limit);
    }

    #[test]
    fn generate_base_in_future() {
        // All dates are future — should generate exactly future_limit instances.
        let spec = RecurrenceSpec::Monthly;
        let base = dt(2025, 1, 1);
        let now = dt(2024, 1, 1); // base is well in the future
        let result = generate_due_dates(&spec, base, now, None, 2);
        assert_eq!(result.dates.len(), 2);
        assert!(result.dates.iter().all(|d| *d > now));
    }

    #[test]
    fn overflow_safety() {
        // i64::MAX seconds cannot fit in a Duration — must return None without panicking.
        let spec = RecurrenceSpec::Seconds(i64::MAX);
        let result = next_due_date(&spec, dt(2024, 1, 1));
        assert!(result.is_none());
    }
}
