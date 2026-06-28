//! Trash retention helpers.
//!
//! A trashed session (see [`Instance::trash`](crate::session::Instance::trash))
//! stays recoverable until the user purges it or its retention window
//! elapses. Retention auto-purge is enforced by the serve daemon only (a
//! startup pass plus an hourly tick), routed through the same purge path the
//! `DELETE /api/sessions/{id}` handler uses, so ACP teardown, event-store
//! deletion, sidecar cleanup, and the storage row removal all stay
//! consistent and there is no multi-process purge race. Without a running
//! daemon, expired trash is purged on the next daemon start or by an explicit
//! manual purge / empty-trash. This module owns the pure "which rows are
//! expired" decision so it can be unit-tested in isolation.

use chrono::{DateTime, Utc};

use crate::session::Instance;

/// True when a trashed session is past its retention window and should be
/// auto-purged. `retention_days == 0` means "keep forever" (manual purge
/// only), so it never expires. A non-trashed session never expires.
pub fn is_expired(instance: &Instance, retention_days: u32, now: DateTime<Utc>) -> bool {
    if retention_days == 0 {
        return false;
    }
    match instance.trashed_at {
        Some(trashed_at) => now >= trashed_at + chrono::Duration::days(retention_days as i64),
        None => false,
    }
}

/// Ids of every trashed session whose retention window has elapsed, in the
/// order they appear in `instances`. Empty when retention is disabled
/// (`retention_days == 0`) or nothing has expired.
pub fn expired_trashed_ids(
    instances: &[Instance],
    retention_days: u32,
    now: DateTime<Utc>,
) -> Vec<String> {
    instances
        .iter()
        .filter(|i| is_expired(i, retention_days, now))
        .map(|i| i.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trashed_days_ago(days: i64) -> Instance {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.trashed_at = Some(Utc::now() - chrono::Duration::days(days));
        inst
    }

    #[test]
    fn not_expired_when_retention_zero() {
        let inst = trashed_days_ago(9999);
        assert!(!is_expired(&inst, 0, Utc::now()), "0 days = keep forever");
    }

    #[test]
    fn not_expired_when_not_trashed() {
        let inst = Instance::new("s", "/tmp/x");
        assert!(!is_expired(&inst, 30, Utc::now()));
    }

    #[test]
    fn expires_exactly_at_window() {
        let now = Utc::now();
        let mut inst = Instance::new("s", "/tmp/x");
        inst.trashed_at = Some(now - chrono::Duration::days(30));
        assert!(
            is_expired(&inst, 30, now),
            "trashed >= retention => expired"
        );

        inst.trashed_at = Some(now - chrono::Duration::days(29));
        assert!(!is_expired(&inst, 30, now), "still within window");
    }

    #[test]
    fn expired_ids_filters_and_preserves_order() {
        let fresh = trashed_days_ago(1);
        let old_a = trashed_days_ago(40);
        let live = Instance::new("s", "/tmp/x");
        let old_b = trashed_days_ago(31);
        let instances = vec![fresh, old_a.clone(), live, old_b.clone()];

        let ids = expired_trashed_ids(&instances, 30, Utc::now());
        assert_eq!(ids, vec![old_a.id, old_b.id]);
    }
}
