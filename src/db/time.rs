use std::time::{SystemTime, UNIX_EPOCH};

/// Convert wall-clock time using floor milliseconds, including before epoch.
fn system_time_to_millis(at: SystemTime) -> Option<i64> {
    let nanos = match at.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).ok()?,
        Err(error) => -i128::try_from(error.duration().as_nanos()).ok()?,
    };
    i64::try_from(nanos.div_euclid(1_000_000)).ok()
}

pub fn unix_millis() -> i64 {
    system_time_to_millis(SystemTime::now()).expect("system clock milliseconds fit i64")
}

/// A UTC calendar day; never a database driver date parameter.
pub type UtcDay = ::time::Date;

pub(crate) fn utc_day_from_millis(millis: i64) -> Option<UtcDay> {
    ::time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .map(|value| value.date())
}

pub(crate) fn utc_midnight_millis(day: UtcDay) -> Option<i64> {
    i64::try_from(
        day.midnight()
            .assume_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .ok()
}

pub(crate) fn utc_day_add(day: UtcDay, days: i64) -> Option<UtcDay> {
    day.checked_add(::time::Duration::seconds(days.checked_mul(86_400)?))
}

pub(crate) fn format_utc_day(day: UtcDay, compact: bool) -> Option<String> {
    let pattern = if compact {
        "[year sign:automatic][month][day]"
    } else {
        "[year sign:automatic]-[month]-[day]"
    };
    let description = ::time::format_description::parse(pattern).ok()?;
    day.format(&description).ok()
}

// Match the prior chrono NaiveDate range, even with time's large-dates feature.
pub(crate) fn format_legacy_stats_day(epoch_day: i64) -> Option<String> {
    let day = utc_day_from_millis(epoch_day.checked_mul(86_400_000)?)?;
    if !(-262_143..=262_142).contains(&day.year()) {
        return None;
    }
    format_utc_day(day, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn utc_days_preserve_negative_epoch_midnight_and_leap_boundaries() {
        let day = utc_day_from_millis(-1).unwrap();
        assert_eq!(format_utc_day(day, false).as_deref(), Some("1969-12-31"));
        assert_eq!(utc_midnight_millis(day), Some(-86_400_000));
        assert_eq!(utc_day_from_millis(0), utc_day_add(day, 1));
        let leap = ::time::Date::from_calendar_date(2024, ::time::Month::February, 29).unwrap();
        assert_eq!(format_utc_day(leap, true).as_deref(), Some("20240229"));
        assert_eq!(
            format_utc_day(utc_day_add(leap, 1).unwrap(), false).as_deref(),
            Some("2024-03-01")
        );
        assert_eq!(
            utc_day_from_millis(utc_midnight_millis(leap).unwrap()),
            Some(leap)
        );
        assert!(utc_day_add(::time::Date::MAX, 1).is_none());
        assert!(utc_day_add(::time::Date::MIN, -1).is_none());
        assert!(utc_day_from_millis(i64::MAX).is_none());
    }

    #[test]
    fn legacy_stats_dates_preserve_extended_years_and_original_limits() {
        for (year, month, day, expected) in [
            (10_000, ::time::Month::January, 1, "+10000-01-01"),
            (-10_000, ::time::Month::January, 1, "-10000-01-01"),
            (-262_143, ::time::Month::January, 1, "-262143-01-01"),
            (262_142, ::time::Month::December, 31, "+262142-12-31"),
        ] {
            let date = ::time::Date::from_calendar_date(year, month, day).unwrap();
            let bucket = utc_midnight_millis(date).unwrap().div_euclid(86_400_000);
            assert_eq!(format_legacy_stats_day(bucket).as_deref(), Some(expected));
        }
        let min = ::time::Date::from_calendar_date(-262_143, ::time::Month::January, 1).unwrap();
        let max = ::time::Date::from_calendar_date(262_142, ::time::Month::December, 31).unwrap();
        assert!(
            format_legacy_stats_day(utc_midnight_millis(min).unwrap().div_euclid(86_400_000) - 1)
                .is_none()
        );
        assert!(
            format_legacy_stats_day(utc_midnight_millis(max).unwrap().div_euclid(86_400_000) + 1)
                .is_none()
        );
        assert!(format_legacy_stats_day(i64::MAX).is_none());
        assert!(format_legacy_stats_day(i64::MIN).is_none());
    }

    #[test]
    fn signed_milliseconds_floor_at_epoch_boundaries() {
        for (nanos, expected) in [(0, 0), (1, 0), (999_999, 0), (1_000_000, 1)] {
            assert_eq!(
                system_time_to_millis(UNIX_EPOCH + Duration::from_nanos(nanos)),
                Some(expected)
            );
        }
        for (nanos, expected) in [(1, -1), (999_999, -1), (1_000_000, -1), (1_000_001, -2)] {
            assert_eq!(
                system_time_to_millis(UNIX_EPOCH - Duration::from_nanos(nanos)),
                Some(expected)
            );
        }
    }

    #[test]
    fn signed_milliseconds_reject_overflow_instead_of_wrapping() {
        let max = Duration::from_millis(i64::MAX as u64);
        let at = UNIX_EPOCH
            .checked_add(max)
            .expect("platform supports i64 millisecond boundary");
        assert_eq!(system_time_to_millis(at), Some(i64::MAX));
        let at = UNIX_EPOCH
            .checked_add(max + Duration::from_millis(1))
            .expect("platform supports overflow boundary");
        assert_eq!(system_time_to_millis(at), None);
        let min_magnitude = Duration::from_millis(i64::MAX as u64 + 1);
        let at = UNIX_EPOCH
            .checked_sub(min_magnitude)
            .expect("platform supports negative i64 millisecond boundary");
        assert_eq!(system_time_to_millis(at), Some(i64::MIN));
        let at = UNIX_EPOCH
            .checked_sub(min_magnitude + Duration::from_nanos(1))
            .expect("platform supports negative overflow boundary");
        assert_eq!(system_time_to_millis(at), None);
    }
}
