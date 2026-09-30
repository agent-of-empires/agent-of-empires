//! Per-instance counts derived from its usage events in id order.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{UsageEvent, UsageKind};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    /// The headline: clears + compactions + resumes.
    pub resets: u32,
    pub clears: u32,
    pub compactions: u32,
    pub compactions_auto: u32,
    pub compactions_manual: u32,
    /// Context starts other than clear and compaction, minus the first one.
    pub resumes: u32,
    pub prompts: u32,
    pub turns: u32,
    pub turn_errors: u32,
    pub context_started_at: Option<DateTime<Utc>>,
    pub context_prompts: u32,
    pub context_turns: u32,
    pub tracked_since: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    /// True once any agent-sourced event (as opposed to lifecycle-only rows)
    /// has been seen, so lifecycle-only sessions don't show a permanent 0.
    pub tracked: bool,
}

/// A compaction logs both `compact` and `context_start/compact`; only the
/// former opens a context, so the pair counts once. A `context_start` with
/// detail `reload` is Pi reloading the same conversation, not a new one.
pub fn is_context_boundary(event: &UsageEvent) -> bool {
    match event.kind {
        UsageKind::Compact => true,
        UsageKind::ContextStart => !matches!(event.detail.as_deref(), Some("compact" | "reload")),
        _ => false,
    }
}

pub fn summarize(events: &[UsageEvent]) -> UsageSummary {
    let mut summary = UsageSummary::default();
    let mut starts = 0u32;
    for event in events {
        summary.tracked_since.get_or_insert(event.occurred_at);
        summary.last_event_at = Some(event.occurred_at);
        if matches!(
            event.kind,
            UsageKind::ContextStart
                | UsageKind::ContextEnd
                | UsageKind::Compact
                | UsageKind::Prompt
                | UsageKind::TurnEnd
        ) {
            summary.tracked = true;
        }
        if is_context_boundary(event) {
            summary.context_started_at = Some(event.occurred_at);
            summary.context_prompts = 0;
            summary.context_turns = 0;
        }
        match event.kind {
            UsageKind::ContextStart => match event.detail.as_deref() {
                Some("clear") => summary.clears += 1,
                Some("compact" | "reload") => {}
                _ => starts += 1,
            },
            UsageKind::Compact => {
                summary.compactions += 1;
                match event.detail.as_deref() {
                    Some("auto") => summary.compactions_auto += 1,
                    Some("manual") => summary.compactions_manual += 1,
                    _ => {}
                }
            }
            UsageKind::Prompt => {
                summary.prompts += 1;
                summary.context_prompts += 1;
            }
            UsageKind::TurnEnd => {
                summary.turns += 1;
                summary.context_turns += 1;
                if event.detail.as_deref() == Some("error") {
                    summary.turn_errors += 1;
                }
            }
            UsageKind::ContextEnd
            | UsageKind::InstanceCreated
            | UsageKind::InstanceRestarted
            | UsageKind::InstanceDeleted => {}
        }
    }
    summary.resumes = starts.saturating_sub(1);
    summary.resets = summary.clears + summary.compactions + summary.resumes;
    summary
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::TimeZone;

    pub(crate) fn ev(minute: u32, kind: UsageKind, detail: Option<&str>) -> UsageEvent {
        UsageEvent {
            id: 0,
            occurred_at: Utc.with_ymd_and_hms(2026, 9, 29, 10, minute, 0).unwrap(),
            instance_id: "inst".into(),
            profile: None,
            agent: Some("claude".into()),
            kind,
            detail: detail.map(str::to_string),
            agent_session_id: None,
        }
    }

    #[test]
    fn empty_log_is_all_zero() {
        assert_eq!(summarize(&[]), UsageSummary::default());
    }

    #[test]
    fn resets_count_clears_compactions_and_later_starts() {
        use UsageKind::*;
        let events = vec![
            ev(0, InstanceCreated, None),
            ev(1, ContextStart, Some("startup")),
            ev(2, Prompt, None),
            ev(3, TurnEnd, None),
            ev(4, ContextStart, Some("clear")),
            ev(5, Prompt, None),
            ev(6, TurnEnd, Some("error")),
            ev(7, Compact, Some("auto")),
            ev(7, ContextStart, Some("compact")),
            ev(8, Prompt, None),
            ev(9, InstanceRestarted, None),
            ev(10, ContextStart, Some("resume")),
            ev(11, Compact, Some("manual")),
            ev(12, Prompt, None),
            ev(13, Prompt, None),
            ev(14, TurnEnd, None),
        ];
        let s = summarize(&events);
        assert_eq!((s.clears, s.compactions, s.resumes, s.resets), (1, 2, 1, 4));
        assert_eq!((s.compactions_auto, s.compactions_manual), (1, 1));
        assert_eq!((s.prompts, s.turns, s.turn_errors), (5, 3, 1));
        assert_eq!(s.context_started_at, Some(events[12].occurred_at));
        assert_eq!((s.context_prompts, s.context_turns), (2, 1));
        assert_eq!(s.tracked_since, Some(events[0].occurred_at));
        assert_eq!(s.last_event_at, Some(events[15].occurred_at));
    }

    #[test]
    fn a_first_seen_resume_is_not_a_reset() {
        let s = summarize(&[ev(0, UsageKind::ContextStart, Some("resume"))]);
        assert_eq!((s.resumes, s.resets), (0, 0));
    }

    #[test]
    fn lifecycle_only_events_are_untracked() {
        use UsageKind::*;
        let s = summarize(&[
            ev(0, InstanceCreated, None),
            ev(1, InstanceRestarted, None),
            ev(2, InstanceDeleted, None),
        ]);
        assert!(!s.tracked);
        assert!(s.last_event_at.is_some());
    }

    #[test]
    fn one_agent_event_marks_the_session_tracked() {
        let s = summarize(&[
            ev(0, UsageKind::InstanceCreated, None),
            ev(1, UsageKind::Prompt, None),
        ]);
        assert!(s.tracked);
    }

    #[test]
    fn pi_reload_is_not_a_boundary_or_a_resume() {
        use UsageKind::*;
        let events = vec![
            ev(0, ContextStart, Some("startup")),
            ev(1, Prompt, None),
            ev(2, ContextStart, Some("reload")),
            ev(3, Prompt, None),
        ];
        let s = summarize(&events);
        assert_eq!((s.resumes, s.resets), (0, 0));
        assert_eq!(s.context_prompts, 2);
        assert_eq!(s.context_started_at, Some(events[0].occurred_at));
    }
}
