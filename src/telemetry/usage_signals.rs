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
    fn record_counts_only_allowlisted_names_and_snapshot_reports_every_key() {
        let counters = UsageSeenCounters::new();
        for name in ["web", "web", "diff_panel", "diff_comments", "web_terminal"] {
            assert!(counters.record(name), "{name} should be allowlisted");
        }
        assert!(!counters.record("bogus"));
        let snap = counters.snapshot();
        let mut expected: Vec<&str> = USAGE_SIGNALS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            snap.keys().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            zeroed().keys().collect::<Vec<_>>(),
            snap.keys().collect::<Vec<_>>()
        );
        assert!(zeroed().values().all(|n| *n == 0));
        for (name, count) in [
            ("web", 2),
            ("diff_panel", 1),
            ("diff_comments", 1),
            ("web_terminal", 1),
            ("structured_view", 0),
        ] {
            assert_eq!(snap.get(name), Some(&count), "{name}");
        }
    }

    #[test]
    fn decrement_subtracts_reported_known_keys_and_saturates() {
        let counters = UsageSeenCounters::new();
        for _ in 0..5 {
            counters.record("web");
        }
        counters.record("structured_view");
        counters.record("diff_panel");

        let mut reported = counters.snapshot();
        counters.record("web");
        reported.insert("phantom".to_string(), 99);
        reported.insert("diff_panel".to_string(), 100);

        counters.decrement(&reported);
        let after = counters.snapshot();
        assert_eq!(after.get("web"), Some(&1));
        assert_eq!(after.get("structured_view"), Some(&0));
        assert_eq!(after.get("diff_panel"), Some(&0), "saturates at zero");
        assert!(!after.contains_key("phantom"));
    }
}
