//! Expiring leases: a dead proxy's holds drain on their own.

use super::{LeaseKind, LeaseOp, LeaseVerb, RunState};
use std::collections::HashMap;
use std::sync::Mutex;

/// More simultaneous leases than any real agent holds; a larger table is a
/// contract violation.
pub const MAX_LEASES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("lease table is full ({MAX_LEASES})")]
pub struct LeaseTableFull;

/// Leases derived from a background task or workflow run live apart from
/// explicit ones, so an id cannot collide across them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseNamespace {
    Explicit,
    BackgroundTask,
    WorkflowRun,
}

type Key = (LeaseNamespace, String);

/// Time is injected as epoch milliseconds so expiry is testable.
#[derive(Debug, Default)]
pub struct LeaseTable {
    held: Mutex<HashMap<Key, (LeaseKind, i64)>>,
}

impl LeaseTable {
    /// A renew of an unknown or expired id re-acquires it: the renew can race
    /// the expiry.
    pub fn apply(&self, op: &LeaseOp, now_ms: i64) -> Result<(), LeaseTableFull> {
        self.apply_in(LeaseNamespace::Explicit, op, now_ms)
    }

    /// `started` acquires or renews a task lease for `ttl_ms`; `ended`
    /// releases it.
    pub fn apply_derived(
        &self,
        namespace: LeaseNamespace,
        id: &str,
        state: RunState,
        ttl_ms: Option<u64>,
        now_ms: i64,
    ) -> Result<(), LeaseTableFull> {
        let op = LeaseOp {
            id: id.to_string(),
            kind: LeaseKind::Task,
            op: match state {
                RunState::Started => LeaseVerb::Acquire,
                RunState::Ended => LeaseVerb::Release,
            },
            ttl_ms,
        };
        self.apply_in(namespace, &op, now_ms)
    }

    fn apply_in(
        &self,
        namespace: LeaseNamespace,
        op: &LeaseOp,
        now_ms: i64,
    ) -> Result<(), LeaseTableFull> {
        let key = (namespace, op.id.clone());
        let mut held = self.held.lock().expect("lease table mutex poisoned");
        held.retain(|_, (_, deadline)| *deadline > now_ms);
        match op.op {
            LeaseVerb::Release => {
                held.remove(&key);
            }
            LeaseVerb::Acquire | LeaseVerb::Renew => {
                if !held.contains_key(&key) && held.len() >= MAX_LEASES {
                    return Err(LeaseTableFull);
                }
                let ttl = op.ttl_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
                held.insert(key, (op.kind, now_ms.saturating_add(ttl)));
            }
        }
        Ok(())
    }

    pub fn clear(&self) {
        self.held
            .lock()
            .expect("lease table mutex poisoned")
            .clear();
    }

    /// Any lease of any kind.
    pub fn any_live(&self, now_ms: i64) -> bool {
        self.held
            .lock()
            .expect("lease table mutex poisoned")
            .values()
            .any(|(_, deadline)| *deadline > now_ms)
    }

    pub fn live(&self, kind: LeaseKind, now_ms: i64) -> bool {
        self.held
            .lock()
            .expect("lease table mutex poisoned")
            .values()
            .any(|(k, deadline)| *k == kind && *deadline > now_ms)
    }

    /// A busy or task lease: the agent is still working with no frames.
    pub fn working(&self, now_ms: i64) -> bool {
        self.live(LeaseKind::Busy, now_ms) || self.live(LeaseKind::Task, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: &str, kind: LeaseKind, verb: LeaseVerb, ttl_ms: Option<u64>) -> LeaseOp {
        LeaseOp {
            id: id.into(),
            kind,
            op: verb,
            ttl_ms,
        }
    }

    #[test]
    fn lease_lifecycle() {
        use LeaseKind::*;
        use LeaseVerb::*;
        let t = LeaseTable::default();
        t.apply(&op("a", Busy, Acquire, Some(1000)), 0).unwrap();
        assert!(t.working(999), "live before expiry");
        assert!(!t.working(1000), "expired with no further frames");

        t.apply(&op("a", Busy, Acquire, Some(1000)), 0).unwrap();
        t.apply(&op("a", Busy, Renew, Some(1000)), 900).unwrap();
        assert!(t.working(1800), "renew extends the deadline");
        assert!(!t.working(1900));

        t.apply(&op("b", Task, Acquire, Some(5000)), 0).unwrap();
        t.apply(&op("b", Task, Release, None), 10).unwrap();
        assert!(!t.live(Task, 11), "release drops it at once");

        t.apply(&op("c", Compaction, Renew, Some(100)), 0).unwrap();
        assert!(t.live(Compaction, 99), "renew of an unknown id re-acquires");
        assert!(!t.live(Task, 99), "compaction does not count as a task");
    }

    #[test]
    fn derived_leases_are_keyed_apart_from_explicit_ones() {
        use LeaseNamespace::*;
        let t = LeaseTable::default();
        let started = |ns, id: &str, ttl, now| {
            t.apply_derived(ns, id, RunState::Started, Some(ttl), now)
                .unwrap()
        };
        let ended =
            |ns, id: &str, now| t.apply_derived(ns, id, RunState::Ended, None, now).unwrap();
        t.apply(
            &op("x", LeaseKind::Compaction, LeaseVerb::Acquire, Some(5000)),
            0,
        )
        .unwrap();
        started(BackgroundTask, "x", 1000, 0);
        assert!(t.live(LeaseKind::Task, 999));
        ended(BackgroundTask, "x", 10);
        assert!(
            !t.live(LeaseKind::Task, 11),
            "ended releases the task lease"
        );
        assert!(
            t.live(LeaseKind::Compaction, 11),
            "the explicit lease is untouched"
        );

        started(WorkflowRun, "x", 1000, 0);
        ended(BackgroundTask, "x", 10);
        assert!(t.live(LeaseKind::Task, 11), "namespaces do not collide");

        started(WorkflowRun, "x", 1000, 900);
        assert!(t.working(1800), "a repeated started renews");
        assert!(t.live(LeaseKind::Task, 1899));
        assert!(!t.live(LeaseKind::Task, 1900), "and it expires on its own");
    }

    #[test]
    fn table_is_bounded() {
        let t = LeaseTable::default();
        for i in 0..MAX_LEASES {
            t.apply(
                &op(
                    &i.to_string(),
                    LeaseKind::Task,
                    LeaseVerb::Acquire,
                    Some(10),
                ),
                0,
            )
            .unwrap();
        }
        let extra = op("extra", LeaseKind::Task, LeaseVerb::Acquire, Some(10));
        assert!(t.apply(&extra, 0).is_err());
        assert!(t.apply(&extra, 10).is_ok(), "expired entries free room");
    }
}
