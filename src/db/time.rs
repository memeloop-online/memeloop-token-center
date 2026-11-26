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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
