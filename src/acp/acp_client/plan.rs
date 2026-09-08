//! Agent plans: parsing bullet text into steps and mapping ACP plan status.

use crate::acp::state::{Plan, PlanStep, PlanStepStatus};

/// Parse Claude's ExitPlanMode tool input into a structured `Plan`.
/// Claude ships the plan markdown in `raw_input.plan`; we extract its
/// bullet- or number-prefixed lines as `PlanStep`s with status=Pending,
/// matching the ACP `SessionUpdate::Plan` shape so the existing
/// PlanStrip renderer can consume it.
///
/// Returns `None` when the input has no `plan` key, the value isn't a
/// string, or the string has no recognisable list items; in which case
/// the generic tool card is still rendered so the user sees the raw
/// plan text. See #1059 for the upstream gap this works around.
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

/// Flatten plan markdown into `PlanStep`s. v1 heuristic: every line
/// starting with `-`, `*`, or `<digit>.` becomes one step. Sub-bullets
/// flatten into the parent list (PlanEntry has no nesting field in the
/// ACP spec). Strips bold/italic markers from the step title so the
/// PlanStrip doesn't render literal `**foo**`.
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

pub(super) fn strip_markdown_emphasis(s: &str) -> String {
    // Replace **bold**, __bold__, *italic*, _italic_ markers with their
    // inner text. Keep it permissive; the source is Claude's planning
    // markdown, which is usually well-formed but occasionally drops a
    // closing marker. Underscore markers are anchored on word boundaries
    // so `snake_case` identifiers survive intact.
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

/// Lowercase string form of a PlanEntryStatus for the synthetic
/// TodoWrite args payload. Matches the values
/// `web/src/components/acp/ToolCards.tsx::normaliseTodoStatus`
/// accepts so the TodoUpdateCard renders the right glyph.
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

    #[test]
    fn parse_plan_steps_extracts_dash_and_numbered_bullets() {
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
    }

    #[test]
    fn parse_plan_steps_returns_empty_when_no_bullets() {
        assert!(parse_plan_steps("Just a paragraph with no list.").is_empty());
        assert!(parse_plan_steps("").is_empty());
    }

    #[test]
    fn extract_plan_from_switch_mode_handles_missing_plan_field() {
        let v = serde_json::json!({});
        assert!(extract_plan_from_switch_mode(&v).is_none());
        let v = serde_json::json!({ "plan": 42 });
        assert!(extract_plan_from_switch_mode(&v).is_none());
    }

    #[test]
    fn extract_plan_from_switch_mode_builds_plan_when_input_has_bullets() {
        let v = serde_json::json!({
            "plan": "- Step one\n- Step two\n- Step three"
        });
        let plan = extract_plan_from_switch_mode(&v).expect("plan should parse");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].title, "Step one");
    }

    #[test]
    fn strip_markdown_emphasis_unwraps_bold_and_italic() {
        assert_eq!(strip_markdown_emphasis("**bold**"), "bold");
        assert_eq!(strip_markdown_emphasis("__bold__"), "bold");
        assert_eq!(strip_markdown_emphasis("*italic*"), "italic");
        assert_eq!(strip_markdown_emphasis("_italic_"), "italic");
        assert_eq!(
            strip_markdown_emphasis("mix of **bold** and *italic*"),
            "mix of bold and italic"
        );
        assert_eq!(strip_markdown_emphasis("plain"), "plain");
    }

    #[test]
    fn strip_markdown_emphasis_keeps_intraword_underscores() {
        for (input, want) in [
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
