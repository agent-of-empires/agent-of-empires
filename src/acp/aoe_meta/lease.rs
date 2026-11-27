//! Expiring leases: a dead proxy's holds drain on their own.

use super::{LeaseKind, LeaseOp, LeaseVerb, RunState};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// More simultaneous leases than any real agent holds; a larger table is a
/// contract violation. Explicit and derived leases share it, and so do the
/// derived leases that lapsed and are not yet reported.
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
    /// Derived leases that expired and are not yet reported by `drain_expired`.
    /// Always locked after `held`.
    lapsed: Mutex<Vec<Key>>,
}

/// The lease op a `started` or `ended` frame derives: `started` acquires or
/// renews a task lease for `ttl_ms`; `ended` releases it.
pub fn derived_op(id: &str, state: RunState, ttl_ms: Option<u64>) -> LeaseOp {
    LeaseOp {
        id: id.to_string(),
        kind: LeaseKind::Task,
        op: match state {
            RunState::Started => LeaseVerb::Acquire,
            RunState::Ended => LeaseVerb::Release,
        },
        ttl_ms,
    }
}

impl LeaseTable {
    /// Applies the ops of one frame together: all, or none when the new keys do
    /// not fit beside the held leases and the unreported lapses. Returns per op whether its id was already open: held, or
    /// lapsed and not yet reported. A renew of an unknown or expired id
    /// re-acquires it: the renew can race the expiry.
    pub fn apply_all(
        &self,
        ops: &[(LeaseNamespace, LeaseOp)],
        now_ms: i64,
    ) -> Result<Vec<bool>, LeaseTableFull> {
        let mut held = self.held.lock().expect("lease table mutex poisoned");
        let mut lapsed = self.lapsed.lock().expect("lease table mutex poisoned");
        Self::expire(&mut held, &mut lapsed, now_ms);
        let mut added = HashSet::new();
        let mut freed = HashSet::new();
        for (ns, op) in ops {
            let key = (*ns, op.id.clone());
            match op.op {
                LeaseVerb::Release if held.contains_key(&key) || lapsed.contains(&key) => {
                    freed.insert(key);
                }
                LeaseVerb::Acquire | LeaseVerb::Renew
                    if !held.contains_key(&key) && !lapsed.contains(&key) =>
                {
                    added.insert(key);
                }
                _ => {}
            }
        }
        if held.len() + lapsed.len() + added.len() - freed.len() > MAX_LEASES {
            return Err(LeaseTableFull);
        }
        let mut open = Vec::with_capacity(ops.len());
        for (ns, op) in ops {
            let key = (*ns, op.id.clone());
            open.push(held.contains_key(&key) || lapsed.contains(&key));
            match op.op {
                LeaseVerb::Release => {
                    lapsed.retain(|k| *k != key);
                    held.remove(&key);
                }
                LeaseVerb::Acquire | LeaseVerb::Renew => {
                    // The frame itself reports this id's state, so a lapse is stale.
                    lapsed.retain(|k| *k != key);
                    let ttl = op.ttl_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
                    held.insert(key, (op.kind, now_ms.saturating_add(ttl)));
                }
            }
        }
        Ok(open)
    }

    fn expire(held: &mut HashMap<Key, (LeaseKind, i64)>, lapsed: &mut Vec<Key>, now_ms: i64) {
        held.retain(|key, (_, deadline)| {
            let live = *deadline > now_ms;
            if !live && key.0 != LeaseNamespace::Explicit {
                lapsed.push(key.clone());
            }
            live
        });
    }

    /// Derived leases that expired without an `ended`, each reported once.
    pub fn drain_expired(&self, now_ms: i64) -> Vec<(LeaseNamespace, String)> {
        let mut held = self.held.lock().expect("lease table mutex poisoned");
        let mut lapsed = self.lapsed.lock().expect("lease table mutex poisoned");
        Self::expire(&mut held, &mut lapsed, now_ms);
        std::mem::take(&mut *lapsed)
    }

    /// Drops every lease and returns the derived ones still open: held, or
    /// lapsed and not yet reported.
    pub fn clear(&self) -> Vec<(LeaseNamespace, String)> {
        let mut held = self.held.lock().expect("lease table mutex poisoned");
        let mut lapsed = self.lapsed.lock().expect("lease table mutex poisoned");
        let mut open = std::mem::take(&mut *lapsed);
        open.extend(held.drain().map(|(key, _)| key));
        open.retain(|(ns, _)| *ns != LeaseNamespace::Explicit);
        open
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

    fn apply(t: &LeaseTable, op: &LeaseOp, now_ms: i64) -> Result<(), LeaseTableFull> {
        let ops = [(LeaseNamespace::Explicit, op.clone())];
        t.apply_all(&ops, now_ms).map(|_| ())
    }

    fn apply_derived(
        t: &LeaseTable,
        ns: LeaseNamespace,
        id: &str,
        state: RunState,
        ttl_ms: Option<u64>,
        now_ms: i64,
    ) -> Result<bool, LeaseTableFull> {
        let ops = [(ns, derived_op(id, state, ttl_ms))];
        t.apply_all(&ops, now_ms).map(|open| open[0])
    }
    #[test]
    fn lease_lifecycle() {
        use LeaseKind::*;
        use LeaseVerb::*;
        let t = LeaseTable::default();
        apply(&t, &op("a", Busy, Acquire, Some(1000)), 0).unwrap();
        assert!(t.working(999), "live before expiry");
        assert!(!t.working(1000), "expired with no further frames");

        apply(&t, &op("a", Busy, Acquire, Some(1000)), 0).unwrap();
        apply(&t, &op("a", Busy, Renew, Some(1000)), 900).unwrap();
        assert!(t.working(1800), "renew extends the deadline");
        assert!(!t.working(1900));

        apply(&t, &op("b", Task, Acquire, Some(5000)), 0).unwrap();
        apply(&t, &op("b", Task, Release, None), 10).unwrap();
        assert!(!t.live(Task, 11), "release drops it at once");

        apply(&t, &op("c", Compaction, Renew, Some(100)), 0).unwrap();
        assert!(t.live(Compaction, 99), "renew of an unknown id re-acquires");
        assert!(!t.live(Task, 99), "compaction does not count as a task");
    }

    #[test]
    fn derived_leases_are_keyed_apart_from_explicit_ones() {
        use LeaseNamespace::*;
        let t = LeaseTable::default();
        let started = |ns, id: &str, ttl, now| {
            apply_derived(&t, ns, id, RunState::Started, Some(ttl), now).unwrap()
        };
        let ended =
            |ns, id: &str, now| apply_derived(&t, ns, id, RunState::Ended, None, now).unwrap();
        apply(
            &t,
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
    fn expired_derived_leases_are_reported_once() {
        use LeaseNamespace::*;
        let t = LeaseTable::default();
        let start = |ns, id: &str, now| {
            apply_derived(&t, ns, id, RunState::Started, Some(100), now).unwrap()
        };
        start(BackgroundTask, "t", 0);
        start(WorkflowRun, "w", 0);
        apply(
            &t,
            &op("e", LeaseKind::Busy, LeaseVerb::Acquire, Some(100)),
            0,
        )
        .unwrap();
        assert!(t.drain_expired(99).is_empty(), "nothing has expired yet");
        let mut got = t.drain_expired(100);
        got.sort_by_key(|(_, id)| id.clone());
        assert_eq!(
            got,
            [
                (BackgroundTask, "t".to_string()),
                (WorkflowRun, "w".to_string())
            ],
            "the explicit lease is not reported"
        );
        assert!(t.drain_expired(200).is_empty(), "reported once");
    }

    #[test]
    fn released_or_revived_derived_leases_are_not_reported() {
        use LeaseNamespace::*;
        let t = LeaseTable::default();
        let apply = |id: &str, state, now| {
            apply_derived(&t, BackgroundTask, id, state, Some(100), now).unwrap()
        };
        apply("rel", RunState::Started, 0);
        apply("rel", RunState::Ended, 50);
        assert!(t.drain_expired(500).is_empty(), "normal release");

        apply("pruned", RunState::Started, 0);
        apply("other", RunState::Started, 500);
        apply("pruned", RunState::Started, 500);
        assert!(
            t.drain_expired(550).is_empty(),
            "a lapse pruned by apply is dropped once the id is revived"
        );

        let t = LeaseTable::default();
        apply_derived(&t, BackgroundTask, "gone", RunState::Started, Some(100), 0).unwrap();
        apply_derived(
            &t,
            BackgroundTask,
            "next",
            RunState::Started,
            Some(100),
            200,
        )
        .unwrap();
        assert_eq!(
            t.drain_expired(250),
            [(BackgroundTask, "gone".to_string())],
            "a lapse pruned by apply is not lost"
        );
    }

    #[test]
    fn table_is_bounded() {
        let t = LeaseTable::default();
        for i in 0..MAX_LEASES {
            apply(
                &t,
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
        assert!(apply(&t, &extra, 0).is_err());
        assert!(apply(&t, &extra, 10).is_ok(), "expired entries free room");
    }

    #[test]
    fn a_refused_acquire_keeps_the_pending_lapse() {
        let t = LeaseTable::default();
        let ns = LeaseNamespace::BackgroundTask;
        apply_derived(&t, ns, "d", RunState::Started, Some(10), 0).unwrap();
        for i in 0..MAX_LEASES - 2 {
            let hold = op(
                &i.to_string(),
                LeaseKind::Busy,
                LeaseVerb::Acquire,
                Some(1000),
            );
            apply(&t, &hold, 0).unwrap();
        }
        // This apply expires "d" into the pending lapses and fills the table.
        let last = op("last", LeaseKind::Busy, LeaseVerb::Acquire, Some(1000));
        apply(&t, &last, 10).unwrap();
        let other = apply_derived(&t, ns, "other", RunState::Started, Some(10), 11);
        assert!(other.is_err(), "the table is full");
        assert_eq!(t.drain_expired(11), vec![(ns, "d".to_string())]);
    }

    #[test]
    fn unreported_lapses_count_against_the_bound() {
        let t = LeaseTable::default();
        let ns = LeaseNamespace::WorkflowRun;
        let admitted = (0..MAX_LEASES + 44)
            .filter(|i| {
                let id = i.to_string();
                apply_derived(&t, ns, &id, RunState::Started, Some(1), *i as i64).is_ok()
            })
            .count();
        assert_eq!(admitted, MAX_LEASES);
        assert!(t.lapsed.lock().unwrap().len() <= MAX_LEASES);
        assert_eq!(t.drain_expired(10_000).len(), MAX_LEASES);
        apply_derived(&t, ns, "next", RunState::Started, Some(1), 10_000).unwrap();
    }

    #[test]
    fn a_frame_that_does_not_fit_changes_nothing() {
        use LeaseNamespace::*;
        let t = LeaseTable::default();
        apply_derived(&t, BackgroundTask, "t", RunState::Started, Some(10), 0).unwrap();
        for i in 0..MAX_LEASES - 2 {
            let hold = op(
                &i.to_string(),
                LeaseKind::Busy,
                LeaseVerb::Acquire,
                Some(1000),
            );
            apply(&t, &hold, 0).unwrap();
        }
        // "t" lapses here: the batch needs three slots and two are free.
        let batch = [
            (BackgroundTask, derived_op("t", RunState::Started, Some(10))),
            (WorkflowRun, derived_op("w", RunState::Started, Some(10))),
            (
                Explicit,
                op("x", LeaseKind::Busy, LeaseVerb::Acquire, Some(10)),
            ),
        ];
        assert!(t.apply_all(&batch, 10).is_err());
        assert_eq!(t.drain_expired(10), [(BackgroundTask, "t".to_string())]);
        assert!(!t.live(LeaseKind::Task, 10), "nothing was admitted");
    }
}
