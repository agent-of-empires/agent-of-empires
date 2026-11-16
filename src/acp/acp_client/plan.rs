//! Agent plans: parsing bullet text into steps and mapping ACP plan status.

use crate::acp::agent_profiles::AgentProfile;
use crate::acp::state::{Plan, PlanStep, PlanStepStatus};

/// The ExitPlanMode markdown for the approval's plan viewer. `args_preview`
/// caps at 16 KiB and truncated JSON no longer parses, so the plan is read
/// from the raw input instead.
pub(super) fn approval_plan_text(
    profile: &AgentProfile,
    raw_input: Option<&serde_json::Value>,
) -> Option<String> {
    if !profile.supports_exit_plan_mode {
        return None;
    }
    let plan = raw_input?.get("plan")?.as_str()?;
    if plan.trim().is_empty() {
        return None;
    }
    Some(plan.to_string())
}

/// `None` for text with no bullet lines.
pub(super) fn plan_from_markdown(plan_text: &str) -> Option<Plan> {
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

/// Every line starting with a Markdown list marker (`-`, `*`, `+`, `1.`, `1)`)
/// outside a fenced code block becomes a step.
/// Sub-bullets flatten in, since `PlanEntry` has no nesting field.
pub(super) fn parse_plan_steps(text: &str) -> Vec<PlanStep> {
    use std::sync::OnceLock;
    static BULLET: OnceLock<regex::Regex> = OnceLock::new();
    let bullet = BULLET.get_or_init(|| {
        regex::Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+(.+?)\s*$")
            .expect("static plan-step regex must compile")
    });

    let mut steps = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = ["```", "~~~"].into_iter().find(|f| trimmed.starts_with(f)) {
            fence = Some(open);
            continue;
        }
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
    status: agent_client_protocol::v1::schema::PlanEntryStatus,
) -> PlanStepStatus {
    use agent_client_protocol::v1::schema::PlanEntryStatus;
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
    status: &agent_client_protocol::v1::schema::PlanEntryStatus,
) -> &'static str {
    use agent_client_protocol::v1::schema::PlanEntryStatus;
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
    use crate::acp::agent_profiles::{CLAUDE, GEMINI};

    #[test]
    fn approval_plan_text_cases() {
        let plan = serde_json::json!({ "plan": "# Plan\n\nprose, no bullets" });
        assert_eq!(
            approval_plan_text(&CLAUDE, Some(&plan)).as_deref(),
            Some("# Plan\n\nprose, no bullets")
        );
        assert!(approval_plan_text(&GEMINI, Some(&plan)).is_none());
        for raw in [
            serde_json::json!({}),
            serde_json::json!({ "plan": 42 }),
            serde_json::json!({ "plan": "  \n" }),
        ] {
            assert!(approval_plan_text(&CLAUDE, Some(&raw)).is_none(), "{raw}");
        }
        assert!(approval_plan_text(&CLAUDE, None).is_none());

        // Past the 16 KiB args_preview cap the plan is still kept whole.
        let long = "\u{e9}".repeat(20_000);
        let kept = approval_plan_text(&CLAUDE, Some(&serde_json::json!({ "plan": long })));
        assert_eq!(kept.as_deref(), Some(long.as_str()));
    }

    /// A plan needs bullets.
    #[test]
    fn plan_extraction_cases() {
        for text in ["Just a paragraph with no list.", ""] {
            assert!(plan_from_markdown(text).is_none(), "{text}");
        }
        let plan = plan_from_markdown("- Step one\n- Step two\n- Step three").expect("plan");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].title, "Step one");

        // Every list marker counts; headings, prose and fenced code do not.
        let md = "# Plan\n\nHere's the plan:\n\n- First, **read** the file\n```diff\n+    let x = 1;\n- old\n~~~\n```\n+ Then patch it\n~~~\n1. not a step\n~~~\n1. Run tests\n2) Commit\n\nOther prose.";
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
