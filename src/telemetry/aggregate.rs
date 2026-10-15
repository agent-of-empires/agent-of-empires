//! Windowed aggregate of live session state for `aoe serve`, so sessions between
//! snapshot ticks still count toward the agent/model mix and concurrency peak.

use std::collections::BTreeMap;

use crate::session::Instance;

/// The window resets only on a confirmed send, so cap new ids while the endpoint is down.
const MAX_SEEN: usize = 10_000;

/// Reset only after a confirmed send so a failed send keeps the window.
#[derive(Default)]
pub struct UsageAggregator {
    peak_concurrent_sessions: u32,
    /// Latest buckets per session id, so a mid-window model change is not double-counted.
    seen: BTreeMap<String, (String, String)>,
}

impl UsageAggregator {
    /// Trash is excluded at sample time: a session seen live stays counted if trashed later.
    pub fn sample(&mut self, instances: &[Instance]) {
        let mut concurrent = 0u32;
        for inst in instances {
            if inst.is_trashed() {
                continue;
            }
            concurrent += 1;
            let buckets = super::instance_buckets(inst);
            if self.seen.contains_key(&inst.id) || self.seen.len() < MAX_SEEN {
                self.seen.insert(inst.id.clone(), buckets);
            }
        }
        self.peak_concurrent_sessions = self.peak_concurrent_sessions.max(concurrent);
    }

    pub fn peak_concurrent_sessions(&self) -> u32 {
        self.peak_concurrent_sessions
    }

    pub fn distinct_by_agent(&self) -> BTreeMap<String, u32> {
        let mut out: BTreeMap<String, u32> = BTreeMap::new();
        for (agent, _model) in self.seen.values() {
            *out.entry(agent.clone()).or_insert(0) += 1;
        }
        out
    }

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
    fn folds_distinct_sessions_and_peak_across_samples() {
        let mut agg = UsageAggregator::default();
        assert_eq!(agg.peak_concurrent_sessions(), 0);
        assert!(agg.distinct_by_agent().is_empty());
        assert!(agg.distinct_by_model().is_empty());

        agg.sample(&[
            inst("a", "claude", Status::Running),
            inst("b", "claude", Status::Idle),
            inst("d", "claude", Status::Running),
        ]);
        agg.sample(&[inst("c", "codex", Status::Running)]);
        // A session seen again under another agent counts once, in its latest bucket.
        agg.sample(&[inst("d", "codex", Status::Running)]);

        let by_agent = agg.distinct_by_agent();
        assert_eq!(by_agent.get("claude"), Some(&2));
        assert_eq!(by_agent.get("codex"), Some(&2));
        assert_eq!(by_agent.values().sum::<u32>(), 4);
        assert_eq!(
            agg.peak_concurrent_sessions(),
            3,
            "the window peak, not the last sample"
        );
    }

    #[test]
    fn caps_distinct_ids_but_keeps_tracking_peak() {
        let mut agg = UsageAggregator::default();
        for i in 0..(MAX_SEEN + 50) {
            agg.sample(&[inst(&format!("s{i}"), "claude", Status::Running)]);
        }
        assert_eq!(
            agg.seen.len(),
            MAX_SEEN,
            "new ids stop being added past cap"
        );

        let big: Vec<Instance> = (0..MAX_SEEN + 200)
            .map(|i| inst(&format!("s{i}"), "claude", Status::Running))
            .collect();
        agg.sample(&big);
        assert_eq!(agg.peak_concurrent_sessions(), (MAX_SEEN + 200) as u32);

        agg.sample(&[inst("s0", "codex", Status::Running)]);
        assert_eq!(
            agg.seen.get("s0"),
            Some(&("codex".to_string(), "unset".to_string()))
        );
    }

    #[test]
    fn trashed_sessions_are_excluded_at_sample_time() {
        let mut agg = UsageAggregator::default();
        let mut already_trashed = inst("gone", "codex", Status::Idle);
        already_trashed.trash();
        agg.sample(&[inst("live", "claude", Status::Running), already_trashed]);

        assert_eq!(
            agg.peak_concurrent_sessions(),
            1,
            "a trashed session must not lift the window peak"
        );
        assert_eq!(agg.distinct_by_agent().get("codex"), None);
        assert_eq!(agg.distinct_by_agent().get("claude"), Some(&1));

        let mut later_trashed = inst("live", "claude", Status::Running);
        later_trashed.trash();
        agg.sample(&[later_trashed]);
        assert_eq!(agg.distinct_by_agent().get("claude"), Some(&1));
        assert_eq!(agg.peak_concurrent_sessions(), 1);
    }
}
