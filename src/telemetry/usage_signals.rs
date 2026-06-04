//! Registry of usage "feature X was used this window" signals.
//!
//! This is the single, auditable place that decides which usage signals the
//! snapshot may carry. The result is a map keyed by a **fixed set of short
//! signal names** (the allowlist), so it can never carry a path, name, or
//! other free-form value, and the gateway forwards it as allowlisted
//! short-name -> count while dropping anything else.
//!
//! It mirrors [`super::features`] (install-level adoption) but tracks a
//! different lifecycle: `features` is rebuilt from config on every snapshot,
//! whereas these counters accumulate browser pings across a window and are
//! decremented by exactly what a confirmed snapshot reported. Tracking a newly
//! instrumented surface is one entry in [`USAGE_SIGNALS`]: the
//! `/api/telemetry/seen` endpoint, the daemon aggregate, and the snapshot all
//! derive from this slice, so no other code changes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// The fixed allowlist of usage-signal names a snapshot may report. Adding a
/// surface here is the only edit needed to instrument it end to end.
///
/// `web` / `structured_view` are whole-UI opens; `diff_panel` / `diff_comments` /
/// `web_terminal` (#1881) are feature-level opens within the dashboard, each
/// fired from its own frontend trigger point. Scratch-session usage is not
/// here: it is cross-surface session state, reported point-in-time by the
/// `sessions_by_substrate` census, not a browser-fired open.
pub const USAGE_SIGNALS: &[&str] = &[
    "web",
    "structured_view",
    "diff_panel",
    "diff_comments",
    "web_terminal",
];

/// Window-scoped "feature was used" counters, one per allowlisted signal in
/// [`USAGE_SIGNALS`]. Built once at daemon start and only ever borrowed, so the
/// atomics are never moved; increments and the decrement-on-confirmed-send are
/// lock-free. The browser pings `POST /api/telemetry/seen`, which folds the
/// count in here; the next opt-in snapshot reports the map and clears exactly
/// what it reported once the send is confirmed.
#[derive(Debug)]
pub struct UsageSeenCounters {
    counts: BTreeMap<&'static str, AtomicU32>,
}

impl Default for UsageSeenCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageSeenCounters {
    pub fn new() -> Self {
        Self {
            counts: USAGE_SIGNALS
                .iter()
                .map(|&name| (name, AtomicU32::new(0)))
                .collect(),
        }
    }

    /// Record one use of an allowlisted signal. Returns `false` for an
    /// unregistered name, which the endpoint turns into a 400; the count is
    /// never created, so an off-list name can never reach the snapshot.
    pub fn record(&self, name: &str) -> bool {
        match self.counts.get(name) {
            Some(counter) => {
                counter.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Current counts as `name -> count`, always the full allowlisted key set
    /// (zeros included) so the wire map has a stable shape regardless of which
    /// surfaces were touched. Mirrors how `features` always emits its full key
    /// set.
    pub fn snapshot(&self) -> BTreeMap<String, u32> {
        self.counts
            .iter()
            .map(|(&name, counter)| (name.to_string(), counter.load(Ordering::Relaxed)))
            .collect()
    }

    /// Subtract exactly the counts a confirmed snapshot reported. Iterates the
    /// registry (not the reported map, which is treated as untrusted) and uses
    /// a saturating subtract, so a count that exceeds the current value can
    /// never underflow and wrap. An increment that landed between the snapshot
    /// build and the confirmed send survives into the next window because only
    /// the reported amount is removed.
    pub fn decrement(&self, reported: &BTreeMap<String, u32>) {
        for (&name, counter) in &self.counts {
            let Some(&amount) = reported.get(name) else {
                continue;
            };
            if amount == 0 {
                continue;
            }
            let mut current = counter.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_sub(amount);
                match counter.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        }
    }
}

/// A full allowlisted key set with every count at zero. The TUI never hosts the
/// web dashboard or acp, so it reports this stable shape rather than an
/// empty (or absent) map, keeping the wire key set identical to the daemon's.
pub fn zeroed() -> BTreeMap<String, u32> {
    USAGE_SIGNALS
        .iter()
        .map(|&name| (name.to_string(), 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_rejects_unregistered_names() {
        let counters = UsageSeenCounters::new();
        assert!(counters.record("web"));
        assert!(counters.record("structured_view"));
        // An off-list name is rejected and never creates a key.
        assert!(!counters.record("bogus"));
        let snap = counters.snapshot();
        assert!(!snap.contains_key("bogus"));
    }

    #[test]
    fn feature_signals_are_registered_and_reported() {
        // The #1881 feature-level signals are allowlisted: they record and
        // show up in the snapshot key set alongside the whole-UI opens.
        let counters = UsageSeenCounters::new();
        for name in ["diff_panel", "diff_comments", "web_terminal"] {
            assert!(counters.record(name), "{name} should be allowlisted");
        }
        let snap = counters.snapshot();
        assert_eq!(snap.get("diff_panel"), Some(&1));
        assert_eq!(snap.get("diff_comments"), Some(&1));
        assert_eq!(snap.get("web_terminal"), Some(&1));
        // The TUI zero shape carries the same expanded key set.
        for name in ["diff_panel", "diff_comments", "web_terminal"] {
            assert_eq!(zeroed().get(name), Some(&0));
        }
    }

    #[test]
    fn snapshot_emits_the_full_allowlisted_key_set_with_zeros() {
        let counters = UsageSeenCounters::new();
        counters.record("web");
        counters.record("web");
        let snap = counters.snapshot();
        // Exactly the allowlist, no more, no less; untouched signals are zero.
        // `snap` is a BTreeMap, so its keys come out sorted; compare against the
        // registry sorted the same way rather than relying on its source order.
        let keys: Vec<&str> = snap.keys().map(String::as_str).collect();
        let mut expected: Vec<&str> = USAGE_SIGNALS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(snap.get("web"), Some(&2));
        assert_eq!(snap.get("structured_view"), Some(&0));
        // zeroed() (the TUI shape) matches the same key set.
        assert_eq!(
            zeroed().keys().collect::<Vec<_>>(),
            snap.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn decrement_subtracts_exactly_reported_known_keys() {
        let counters = UsageSeenCounters::new();
        for _ in 0..5 {
            counters.record("web");
        }
        counters.record("structured_view");

        // An open lands during the in-flight send (after the snapshot read).
        let reported = counters.snapshot();
        counters.record("web");

        counters.decrement(&reported);
        let after = counters.snapshot();
        // The web open that landed mid-send survives; acp is fully cleared.
        assert_eq!(after.get("web"), Some(&1));
        assert_eq!(after.get("structured_view"), Some(&0));
    }

    #[test]
    fn decrement_ignores_unknown_reported_keys() {
        let counters = UsageSeenCounters::new();
        counters.record("web");
        let mut reported = counters.snapshot();
        reported.insert("phantom".to_string(), 99);
        // The unknown key is ignored; known keys still clear.
        counters.decrement(&reported);
        assert_eq!(counters.snapshot().get("web"), Some(&0));
    }

    #[test]
    fn decrement_saturates_instead_of_underflowing() {
        let counters = UsageSeenCounters::new();
        counters.record("web");
        // A reported count larger than the current value (buggy/stale report)
        // saturates at zero rather than wrapping to ~4 billion.
        let mut reported = BTreeMap::new();
        reported.insert("web".to_string(), 100);
        counters.decrement(&reported);
        assert_eq!(counters.snapshot().get("web"), Some(&0));
    }
}
