//! Agent plans: parsing bullet text into steps and mapping ACP plan status.

use crate::acp::state::{Plan, PlanStep, PlanStepStatus};

/// ExitPlanMode ships its markdown in `raw_input.plan`, which has no ACP
/// `SessionUpdate::Plan` of its own (#1059). `None` for a missing, non-string,
/// or bullet-less value, which still renders as a generic tool card.
pub(super) fn extract_plan_from_switch_mode(raw_input: &serde_json::Value) -> Option<Plan> {
    let plan_text = raw_input.get("plan")?.as_str()?;
    let steps = parse_plan_steps(plan_text);
    if steps.is_empty() {
        return None;
    }
    Some(Plan {
        plan_id: format!("plan-{}", chrono::Utc::now().timestamp_millis()),
        version: 1,
        steps,
    })
}

/// Every line starting with `-`, `*`, or `<digit>.` becomes a step.
/// Sub-bullets flatten in, since `PlanEntry` has no nesting field.
pub(super) fn parse_plan_steps(text: &str) -> Vec<PlanStep> {
    use std::sync::OnceLock;
    static BULLET: OnceLock<regex::Regex> = OnceLock::new();
    let bullet = BULLET.get_or_init(|| {
        regex::Regex::new(r"^\s*(?:[-*]|\d+\.)\s+(.+?)\s*$")
            .expect("static plan-step regex must compile")
    });

    let mut steps = Vec::new();
    for line in text.lines() {
        if let Some(caps) = bullet.captures(line) {
            let raw_title = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let title = strip_markdown_emphasis(raw_title);
            if title.is_empty() {
                continue;
            }
            steps.push(PlanStep {
                id: format!("step-{}", steps.len()),
                title,
                detail: None,
                status: PlanStepStatus::Pending,
            });
        }
    }
    steps
}

/// Unwraps `**bold**`, `__bold__`, `*italic*` and `_italic_` so the PlanStrip
/// renders no literal markers. Underscores anchor on word boundaries, leaving
/// `snake_case` identifiers intact.
pub(super) fn strip_markdown_emphasis(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\*\*(.+?)\*\*|\b__(.+?)__\b|\*([^*]+?)\*|\b_([^_]+?)_\b")
            .expect("static emphasis-strip regex must compile")
    });
    re.replace_all(s.trim(), |caps: &regex::Captures<'_>| {
        for i in 1..=4 {
            if let Some(m) = caps.get(i) {
                return m.as_str().to_string();
            }
        }
        String::new()
    })
    .into_owned()
}

pub(super) fn map_plan_status(
    status: agent_client_protocol::schema::v1::PlanEntryStatus,
) -> PlanStepStatus {
    use agent_client_protocol::schema::v1::PlanEntryStatus;
    match status {
        PlanEntryStatus::Pending => PlanStepStatus::Pending,
        PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
        PlanEntryStatus::Completed => PlanStepStatus::Done,
        // The schema is non-exhaustive; treat unknown variants as Pending.
        _ => PlanStepStatus::Pending,
    }
}

/// For the synthetic TodoWrite args payload. Matches what
/// `web/src/components/acp/ToolCards.tsx::normaliseTodoStatus` accepts.
pub(super) fn plan_status_to_str(
    status: &agent_client_protocol::schema::v1::PlanEntryStatus,
) -> &'static str {
    use agent_client_protocol::schema::v1::PlanEntryStatus;
    match status {
        PlanEntryStatus::Pending => "pending",
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan needs bullets; anything else renders as a generic tool card.
    #[test]
    fn plan_extraction_cases() {
        for raw in [
            serde_json::json!({}),
            serde_json::json!({ "plan": 42 }),
            serde_json::json!({ "plan": "Just a paragraph with no list." }),
            serde_json::json!({ "plan": "" }),
        ] {
            assert!(extract_plan_from_switch_mode(&raw).is_none(), "{raw}");
        }
        let plan = extract_plan_from_switch_mode(&serde_json::json!({
            "plan": "- Step one\n- Step two\n- Step three"
        }))
        .expect("plan should parse");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].title, "Step one");

        // Dash and numbered bullets both count; surrounding prose does not.
        let md = "Here's the plan:\n\n- First, **read** the file\n- Then patch it\n1. Run tests\n2. Commit\n\nOther prose.";
        let steps = parse_plan_steps(md);
        let titles: Vec<&str> = steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "First, read the file",
                "Then patch it",
                "Run tests",
                "Commit"
            ]
        );
        for s in &steps {
            assert!(matches!(s.status, PlanStepStatus::Pending));
        }

        {
            for (input, want) in [
                ("**bold**", "bold"),
                ("__bold__", "bold"),
                ("*italic*", "italic"),
                ("_italic_", "italic"),
                ("mix of **bold** and *italic*", "mix of bold and italic"),
                ("plain", "plain"),
                ("rename _foo_ now", "rename foo now"),
                ("foo_bar_baz", "foo_bar_baz"),
                ("rename foo_bar_baz", "rename foo_bar_baz"),
                ("call do_thing() then _stop_", "call do_thing() then stop"),
                // `\b` is zero-width, so adjacent emphasis still unwraps.
                ("_a_ _b_", "a b"),
            ] {
                assert_eq!(strip_markdown_emphasis(input), want, "input: {input}");
            }
        }
    }
}
