//! Allowlisted registry of window-scoped "surface was used" signals. Adding an entry to
//! [`USAGE_SIGNALS`] instruments a surface end to end.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

pub const USAGE_SIGNALS: &[&str] = &[
    "web",
    "structured_view",
    "diff_panel",
    "diff_comments",
    "web_terminal",
];

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

    pub fn record(&self, name: &str) -> bool {
        match self.counts.get(name) {
            Some(counter) => {
                counter.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn snapshot(&self) -> BTreeMap<String, u32> {
        self.counts
            .iter()
            .map(|(&name, counter)| (name.to_string(), counter.load(Ordering::Relaxed)))
            .collect()
    }

    /// Iterates the registry, not the untrusted map, and saturates; increments that landed
    /// mid-send survive.
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

/// The TUI reports the full zeroed key set so the wire shape matches the daemon's.
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
        assert!(!counters.record("bogus"));
        let snap = counters.snapshot();
        assert!(!snap.contains_key("bogus"));
    }

    #[test]
    fn feature_signals_are_registered_and_reported() {
        let counters = UsageSeenCounters::new();
        for name in ["diff_panel", "diff_comments", "web_terminal"] {
            assert!(counters.record(name), "{name} should be allowlisted");
        }
        let snap = counters.snapshot();
        assert_eq!(snap.get("diff_panel"), Some(&1));
        assert_eq!(snap.get("diff_comments"), Some(&1));
        assert_eq!(snap.get("web_terminal"), Some(&1));
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
        let keys: Vec<&str> = snap.keys().map(String::as_str).collect();
        let mut expected: Vec<&str> = USAGE_SIGNALS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(snap.get("web"), Some(&2));
        assert_eq!(snap.get("structured_view"), Some(&0));
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

        let reported = counters.snapshot();
        counters.record("web");

        counters.decrement(&reported);
        let after = counters.snapshot();
        assert_eq!(after.get("web"), Some(&1));
        assert_eq!(after.get("structured_view"), Some(&0));
    }

    #[test]
    fn decrement_ignores_unknown_reported_keys() {
        let counters = UsageSeenCounters::new();
        counters.record("web");
        let mut reported = counters.snapshot();
        reported.insert("phantom".to_string(), 99);
        counters.decrement(&reported);
        assert_eq!(counters.snapshot().get("web"), Some(&0));
    }

    #[test]
    fn decrement_saturates_instead_of_underflowing() {
        let counters = UsageSeenCounters::new();
        counters.record("web");
        let mut reported = BTreeMap::new();
        reported.insert("web".to_string(), 100);
        counters.decrement(&reported);
        assert_eq!(counters.snapshot().get("web"), Some(&0));
    }
}
