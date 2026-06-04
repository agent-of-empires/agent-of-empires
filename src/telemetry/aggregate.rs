//! Windowed aggregate of live session state for `aoe serve`.
//!
//! A point-in-time `usage_snapshot` only sees sessions alive at the instant the
//! periodic tick fires, so a session that opens and closes between two ticks is
//! invisible in the agent/model attribution and never lifts the concurrency
//! peak. `aoe serve` folds a sample of the live session list into a
//! [`UsageAggregator`] every ~30 min and reports the window's peak concurrency
//! and distinct-sessions-seen maps at flush time, while keeping the send cadence
//! at one POST per send window (see `src/server/mod.rs`).
//!
//! Sampling is coarse on purpose (the issue, #1870, chose a ~30-min interval):
//! a session born and gone entirely between two samples is still missed, but
//! its raw count survives in `session_creates_since_last_snapshot`. What this
//! recovers is the agent/model mix and peak/typical concurrency of sessions
//! that live long enough to be sampled at least once.

use std::collections::BTreeMap;

use crate::session::Instance;

/// Soft cap on distinct session ids tracked in one window. The aggregate is
/// reset only on a confirmed send, so a daemon whose endpoint stays unreachable
/// for a long stretch would otherwise keep folding new ids forever. Past the
/// cap we stop adding new ids (peak concurrency and already-seen sessions still
/// update), bounding worst-case memory without touching the happy path. The
/// limit is far above any realistic distinct-session count for one send window.
const MAX_SEEN: usize = 10_000;

/// In-memory accumulator folded by the serve telemetry loop. Reset (dropped /
/// re-defaulted) only after a confirmed send so a failed send retains the
/// window for the next attempt.
#[derive(Default)]
pub struct UsageAggregator {
    peak_concurrent_sessions: u32,
    /// session id -> (agent bucket, model bucket), latest observed. Keyed by
    /// session id so a session sampled in multiple windows-internal ticks
    /// counts once; storing the *latest* buckets (rather than a per-bucket set)
    /// means a session whose model bucket changes mid-window is counted in one
    /// bucket, not double-counted.
    seen: BTreeMap<String, (String, String)>,
}

impl UsageAggregator {
    /// Fold one sample of the live session list into the running window.
    pub fn sample(&mut self, instances: &[Instance]) {
        self.peak_concurrent_sessions = self.peak_concurrent_sessions.max(instances.len() as u32);
        for inst in instances {
            let buckets = super::instance_buckets(inst);
            // Refresh an already-seen session's bucket regardless; only gate
            // *new* ids on the cap so a stuck-offline daemon can't grow `seen`
            // without bound.
            if self.seen.contains_key(&inst.id) || self.seen.len() < MAX_SEEN {
                self.seen.insert(inst.id.clone(), buckets);
            }
        }
    }

    /// Max concurrent `session_total` seen across the window.
    pub fn peak_concurrent_sessions(&self) -> u32 {
        self.peak_concurrent_sessions
    }

    /// Distinct sessions seen per agent bucket across the window.
    pub fn distinct_by_agent(&self) -> BTreeMap<String, u32> {
        let mut out: BTreeMap<String, u32> = BTreeMap::new();
        for (agent, _model) in self.seen.values() {
            *out.entry(agent.clone()).or_insert(0) += 1;
        }
        out
    }

    /// Distinct sessions seen per model bucket across the window.
    pub fn distinct_by_model(&self) -> BTreeMap<String, u32> {
        let mut out: BTreeMap<String, u32> = BTreeMap::new();
        for (_agent, model) in self.seen.values() {
            *out.entry(model.clone()).or_insert(0) += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, Status};

    fn inst(id: &str, tool: &str, status: Status) -> Instance {
        let mut i = Instance::new("t", "/p");
        i.id = id.to_string();
        i.tool = tool.to_string();
        i.status = status;
        i
    }

    #[test]
    fn folds_distinct_sessions_across_samples() {
        let mut agg = UsageAggregator::default();
        // Window: two sessions early, a different one later. None concurrent
        // with all the others, but all three are "seen" across the window.
        agg.sample(&[
            inst("a", "claude", Status::Running),
            inst("b", "claude", Status::Idle),
        ]);
        agg.sample(&[inst("c", "codex", Status::Running)]);

        let by_agent = agg.distinct_by_agent();
        assert_eq!(by_agent.get("claude"), Some(&2));
        assert_eq!(by_agent.get("codex"), Some(&1));
        // Distinct-seen total (3) exceeds any single concurrent count.
        let distinct_total: u32 = by_agent.values().sum();
        assert_eq!(distinct_total, 3);
    }

    #[test]
    fn tracks_peak_concurrency_not_the_last_sample() {
        let mut agg = UsageAggregator::default();
        agg.sample(&[inst("a", "claude", Status::Running)]); // 1
        agg.sample(&[
            inst("a", "claude", Status::Running),
            inst("b", "claude", Status::Running),
            inst("c", "claude", Status::Running),
        ]); // 3
        agg.sample(&[inst("a", "claude", Status::Idle)]); // 1
        assert_eq!(agg.peak_concurrent_sessions(), 3);
    }

    #[test]
    fn same_session_counts_once_with_latest_bucket() {
        let mut agg = UsageAggregator::default();
        // The same session id sampled twice must count once; a later sample's
        // bucket wins, so it is never double-counted across buckets.
        agg.sample(&[inst("a", "claude", Status::Running)]);
        agg.sample(&[inst("a", "codex", Status::Running)]);

        let by_agent = agg.distinct_by_agent();
        assert_eq!(by_agent.get("claude"), None);
        assert_eq!(by_agent.get("codex"), Some(&1));
        assert_eq!(by_agent.values().sum::<u32>(), 1);
    }

    #[test]
    fn caps_distinct_ids_but_keeps_tracking_peak() {
        let mut agg = UsageAggregator::default();
        // Fill `seen` past the cap with distinct ids across many samples.
        for i in 0..(MAX_SEEN + 50) {
            agg.sample(&[inst(&format!("s{i}"), "claude", Status::Running)]);
        }
        assert_eq!(
            agg.seen.len(),
            MAX_SEEN,
            "new ids stop being added past cap"
        );

        // A large concurrent sample still lifts the peak even when `seen` is full.
        let big: Vec<Instance> = (0..MAX_SEEN + 200)
            .map(|i| inst(&format!("s{i}"), "claude", Status::Running))
            .collect();
        agg.sample(&big);
        assert_eq!(agg.peak_concurrent_sessions(), (MAX_SEEN + 200) as u32);

        // An already-seen id still refreshes its bucket past the cap.
        agg.sample(&[inst("s0", "codex", Status::Running)]);
        assert_eq!(
            agg.seen.get("s0"),
            Some(&("codex".to_string(), "unset".to_string()))
        );
    }

    #[test]
    fn empty_window_reports_zero() {
        let agg = UsageAggregator::default();
        assert_eq!(agg.peak_concurrent_sessions(), 0);
        assert!(agg.distinct_by_agent().is_empty());
        assert!(agg.distinct_by_model().is_empty());
    }
}
