//! Small crate-internal utilities shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in whole seconds, saturating to 0 if the clock is before
/// the epoch (which should never happen on a sane system).
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whole milliseconds between `t` and the Unix epoch, saturating to 0 if `t`
/// predates the epoch.
pub(crate) fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current Unix time in whole milliseconds. Wall-clock (not a per-process
/// monotonic), so values are comparable across processes; saturating to 0 if
/// the clock is before the epoch.
pub(crate) fn now_ms() -> u64 {
    system_time_to_ms(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn system_time_to_ms_at_epoch_is_zero() {
        assert_eq!(system_time_to_ms(UNIX_EPOCH), 0);
    }

    #[test]
    fn system_time_to_ms_converts_offset() {
        let t = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(system_time_to_ms(t), 1_500);
    }

    #[test]
    fn pre_epoch_saturates_to_zero() {
        let before = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(system_time_to_ms(before), 0);
    }

    #[test]
    fn now_ms_matches_seconds_at_same_instant() {
        let t = SystemTime::now();
        let ms = system_time_to_ms(t);
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        assert_eq!(ms / 1_000, secs);
    }

    #[test]
    fn now_helpers_are_post_epoch() {
        assert!(now_secs() > 0);
        assert!(now_ms() > 0);
    }
}
