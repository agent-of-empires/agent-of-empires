//! Cross-session aggregates for `aoe usage`.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{is_context_boundary, summarize, UsageEvent, UsageKind};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageReport {
    pub instances: u32,
    pub contexts: u32,
    pub clears: u32,
    pub compactions: u32,
    pub compactions_auto: u32,
    pub compactions_manual: u32,
    pub resumes: u32,
    pub prompts: u32,
    pub turns: u32,
    pub turn_errors: u32,
    pub median_prompts_per_context: Option<f64>,
    pub median_context_minutes: Option<f64>,
    pub median_resets_per_instance: Option<f64>,
    pub max_resets_per_instance: u32,
    /// Instance ids with the most resets, highest first, at most ten.
    pub top: Vec<(String, u32)>,
}

pub fn build_report(events: &[UsageEvent]) -> UsageReport {
    let mut by_instance: BTreeMap<&str, Vec<&UsageEvent>> = BTreeMap::new();
    for event in events {
        by_instance
            .entry(&event.instance_id)
            .or_default()
            .push(event);
    }
    let mut report = UsageReport::default();
    let mut prompts_per_context = Vec::new();
    let mut context_minutes = Vec::new();
    let mut resets = Vec::new();
    for (instance_id, events) in &by_instance {
        let owned: Vec<UsageEvent> = events.iter().map(|e| (*e).clone()).collect();
        let summary = summarize(&owned);
        report.instances += 1;
        report.clears += summary.clears;
        report.compactions += summary.compactions;
        report.compactions_auto += summary.compactions_auto;
        report.compactions_manual += summary.compactions_manual;
        report.resumes += summary.resumes;
        report.prompts += summary.prompts;
        report.turns += summary.turns;
        report.turn_errors += summary.turn_errors;
        resets.push((instance_id.to_string(), summary.resets));

        let last_at = owned.last().map(|e| e.occurred_at);
        let mut open: Option<(chrono::DateTime<chrono::Utc>, u32)> = None;
        for event in &owned {
            if is_context_boundary(event) {
                if let Some((started, prompts)) = open.take() {
                    prompts_per_context.push(f64::from(prompts));
                    context_minutes.push((event.occurred_at - started).num_seconds() as f64 / 60.0);
                }
                open = Some((event.occurred_at, 0));
                report.contexts += 1;
            } else if event.kind == UsageKind::Prompt {
                if let Some((_, prompts)) = open.as_mut() {
                    *prompts += 1;
                }
            }
        }
        if let (Some((started, prompts)), Some(last_at)) = (open, last_at) {
            prompts_per_context.push(f64::from(prompts));
            context_minutes.push((last_at - started).num_seconds() as f64 / 60.0);
        }
    }
    report.median_prompts_per_context = median(prompts_per_context);
    report.median_context_minutes = median(context_minutes);
    report.median_resets_per_instance = median(resets.iter().map(|(_, n)| f64::from(*n)).collect());
    report.max_resets_per_instance = resets.iter().map(|(_, n)| *n).max().unwrap_or(0);
    resets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    resets.truncate(10);
    report.top = resets;
    report
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::summary::tests::ev;

    fn on(instance: &str, mut event: UsageEvent) -> UsageEvent {
        event.instance_id = instance.into();
        event
    }

    #[test]
    fn aggregates_contexts_and_instances() {
        use UsageKind::*;
        let events = vec![
            on("a", ev(0, ContextStart, Some("startup"))),
            on("a", ev(1, Prompt, None)),
            on("a", ev(2, Prompt, None)),
            on("a", ev(10, ContextStart, Some("clear"))),
            on("a", ev(11, Prompt, None)),
            on("a", ev(20, Compact, Some("auto"))),
            on("a", ev(20, ContextStart, Some("compact"))),
            on("a", ev(26, TurnEnd, None)),
            on("b", ev(0, ContextStart, Some("startup"))),
            on("b", ev(4, Prompt, None)),
        ];
        let r = build_report(&events);
        assert_eq!((r.instances, r.contexts), (2, 4));
        assert_eq!(
            (r.clears, r.compactions, r.compactions_auto, r.resumes),
            (1, 1, 1, 0)
        );
        assert_eq!((r.prompts, r.turns), (4, 1));
        // Contexts: a[2 prompts, 10m], a[1, 10m], a[0, 6m], b[1, 4m].
        assert_eq!(r.median_prompts_per_context, Some(1.0));
        assert_eq!(r.median_context_minutes, Some(8.0));
        assert_eq!(r.median_resets_per_instance, Some(1.0));
        assert_eq!(r.max_resets_per_instance, 2);
        assert_eq!(r.top, vec![("a".to_string(), 2), ("b".to_string(), 0)]);
    }

    #[test]
    fn median_handles_even_odd_and_empty() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }
}
