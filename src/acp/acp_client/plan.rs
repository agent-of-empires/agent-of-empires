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

/// `None` for text with no list items.
pub(super) fn plan_from_markdown(plan_text: &str) -> Option<Plan> {
    let steps = parse_plan_steps(plan_text);
    if steps.is_empty() {
        return None;
    }
    Some(Plan {
        plan_id: format!("plan-{}", crate::time::Timestamp::now().as_millis()),
        version: 1,
        steps,
    })
}

/// Each list item's first line becomes a step; code blocks never do.
/// Sub-bullets flatten in, since `PlanEntry` has no nesting field.
pub(super) fn parse_plan_steps(text: &str) -> Vec<PlanStep> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let mut titles = Vec::new();
    // One entry per open list item: its title so far, or `None` once the
    // first line has ended.
    let mut items: Vec<Option<String>> = Vec::new();
    for event in Parser::new(text) {
        let current = items.last_mut();
        match event {
            Event::Start(Tag::Item) => items.push(Some(String::new())),
            Event::End(TagEnd::Item) => {
                if let Some(Some(title)) = items.pop() {
                    titles.push(title);
                }
            }
            // `Vec<String>` parses as inline HTML; keep it as written.
            Event::Text(t) | Event::InlineHtml(t) => {
                if let Some(Some(title)) = current {
                    title.push_str(&t);
                }
            }
            Event::Code(t) => {
                if let Some(Some(title)) = current {
                    title.push_str(&format!("`{t}`"));
                }
            }
            // A heading that opens the item (`- ### Step`) is its title.
            Event::Start(Tag::Heading { .. }) if matches!(&current, Some(Some(t)) if t.is_empty()) =>
                {}
            // Inline markup keeps the line going; a break or any nested
            // block (sub-list, code block, second paragraph) ends it.
            Event::Start(
                Tag::Paragraph | Tag::Emphasis | Tag::Strong | Tag::Link { .. } | Tag::Image { .. },
            )
            | Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Link | TagEnd::Image) => {}
            _ => {
                if let Some(slot @ Some(_)) = current {
                    titles.push(slot.take().unwrap_or_default());
                }
            }
        }
    }
    titles
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .enumerate()
        .map(|(i, title)| PlanStep {
            id: format!("step-{i}"),
            title,
            detail: None,
            status: PlanStepStatus::Pending,
        })
        .collect()
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

        // Code blocks, fenced or indented, never yield steps.
        for (md, want) in [
            ("~~~\n~~~literal\n- hidden\n~~~  \n- after", &["after"][..]),
            ("````\n```\n- hidden\n````\n- after", &["after"]),
            ("```rust\n- hidden\n~~~\n`````\n- after", &["after"]),
            ("~~~\n- hidden\n```\n- after", &[]),
            ("````\n- hidden\n```\n- after", &[]),
            (
                "# Plan\n\n    ```text\n    example\n\n- Run tests",
                &["Run tests"],
            ),
            ("   ~~~\n- hidden\n   ~~~\n- after", &["after"]),
            ("```\n- hidden\n    ```\n```\n- after", &["after"]),
            (
                "- step\n  ```\n  - hidden\n  ```\n- after",
                &["step", "after"],
            ),
            (
                "1. step\n       ```\n   - sub\n- after",
                &["step", "sub", "after"],
            ),
            (
                "- parent\n    - child\n\n    ```\n    - hidden\n    ```\n- after",
                &["parent", "child", "after"],
            ),
            ("- step\n  ```\n  - hidden\n- after", &["step", "after"]),
            (
                "- step\n  ```\n  - hidden\n\nprose\n- after",
                &["step", "after"],
            ),
        ] {
            let titles: Vec<String> = parse_plan_steps(md).into_iter().map(|s| s.title).collect();
            assert_eq!(titles, want, "{md:?}");
        }

        // A heading opening the item is its title; one after text is not.
        for (md, want) in [
            (
                "- ### Inspect the logs\n- ### Verify the fix",
                &["Inspect the logs", "Verify the fix"][..],
            ),
            ("- text\n  ### Heading", &["text"]),
            (
                "- ### Inspect logs\n\n  Search the service logs",
                &["Inspect logs"],
            ),
            (
                "- ### Inspect logs\n  Search the service logs",
                &["Inspect logs"],
            ),
        ] {
            let titles: Vec<String> = parse_plan_steps(md).into_iter().map(|s| s.title).collect();
            assert_eq!(titles, want, "{md:?}");
        }

        // Titles take the item's first line with Markdown emphasis removed.
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
            ("_a_ _b_", "a b"),
            ("Run `cargo test`", "Run `cargo test`"),
            (
                "Return Vec<String> from parse",
                "Return Vec<String> from parse",
            ),
            ("Add ![badge](u) to README", "Add badge to README"),
            ("first line\nlazy continuation", "first line"),
            ("first paragraph\n\n  second paragraph", "first paragraph"),
        ] {
            let titles: Vec<String> = parse_plan_steps(&format!("- {input}"))
                .into_iter()
                .map(|s| s.title)
                .collect();
            assert_eq!(titles, [want], "input: {input:?}");
        }
    }
}
