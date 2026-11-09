use super::*;
use crate::acp::{
    state::{DiffPreview, Event, ToolCall, ToolOutputBlock},
    transcript::{reducer::Core, storage::Changes},
};
use crate::tui::rich_text::window::{self, Index};
use std::sync::{atomic::AtomicBool, Arc};

fn project(events: impl IntoIterator<Item = Event>) -> (Rc<Context>, Projection<Disk>) {
    let context = Context::new(Arc::new(AtomicBool::new(false))).unwrap();
    let mut model = Core::new(Disk(Rc::clone(&context))).unwrap();
    for (position, event) in events.into_iter().enumerate() {
        context
            .store
            .with_transaction(|_| {
                let mut writer = context.spool.writer();
                serde_json::to_writer(&mut writer, &event).unwrap();
                let value = context.parse(writer.finish()?)?;
                for delta in model
                    .apply(position as u64 + 1, context.event(value)?)?
                    .into_iter()
                {
                    delta?;
                }
                Ok(())
            })
            .unwrap();
    }
    (context, model.into_projection().unwrap())
}
fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(100, 0).unwrap()
}

#[test]
fn rich_payloads_survive_reflow_and_full_copy_without_live_preview_limits() {
    let arguments = "argument\n".repeat(40) + "ARGUMENT-END";
    let diff = "changed\n".repeat(40) + "DIFF-END";
    let output = (0..80)
        .map(|line| format!("output-{line:02}\n"))
        .collect::<String>()
        + "OUTPUT-END";
    let (context, projection) = project([
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "t".into(),
                name: "RetainedTool".into(),
                kind: "edit".into(),
                args_preview: arguments.clone(),
                started_at: at(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: vec![DiffPreview {
                    path: "/repo/src/test.rs".into(),
                    old_text: Some("before".into()),
                    new_text: Some(diff.clone()),
                    created_at: at(),
                }],
            },
        },
        Event::ToolCallCompleted {
            tool_call_id: "t".into(),
            is_error: false,
            content: "FALLBACK-NOT-DISPLAYED".into(),
            output: vec![
                ToolOutputBlock::Text {
                    text: output.clone(),
                },
                ToolOutputBlock::Image {
                    mime_type: "image/png".into(),
                    data: Some("BINARY-NOT-TEXT".into()),
                    uri: None,
                },
            ],
            completed_at: at(),
            async_subagent: false,
        },
    ]);
    let roots = SessionPathRoots {
        id: "session".into(),
        project_path: "/repo".into(),
        main_repo_path: None,
        workspace_repos: Vec::new(),
    };
    for color in [ratatui::style::Color::Red, ratatui::style::Color::Cyan] {
        context
            .spool
            .with_speculation::<()>(|| {
                let theme = Theme {
                    text: color,
                    ..Theme::default()
                };
                let document = context.store.with_transaction(|_| {
                    document(&context, &projection, &theme, Some(&roots), || true)
                })?;
                for width in [32, 80] {
                    let index = context
                        .store
                        .with_transaction(|_| Index::build(&document, width, || true))?;
                    let tail = index.window(
                        &document,
                        index.len().saturating_sub(10)..index.len(),
                        || true,
                    )?;
                    assert!(tail.to_string().contains("OUTPUT-END"));
                    let header = index.window(&document, 0..1, || true)?;
                    assert_eq!(header.lines[0].spans[0].style.fg, Some(color));
                }
                let mut copied = Vec::new();
                window::copy_all(&document, &mut copied, || true)?;
                let copied = String::from_utf8(copied).unwrap();
                for expected in [&arguments, &diff, &output] {
                    assert!(copied.contains(expected), "missing complete payload");
                }
                assert!(copied.contains("Before: src/test.rs\nbefore\nAfter: src/test.rs\n"));
                assert!(copied.contains("[image image/png]"));
                for hidden in ["BINARY-NOT-TEXT", "FALLBACK-NOT-DISPLAYED", "/repo/"] {
                    assert!(!copied.contains(hidden));
                }
                Ok(None)
            })
            .unwrap();
    }
}

#[test]
fn retained_media_keeps_text_fallback_without_exposing_binary_payloads() {
    let image = ToolOutputBlock::Image {
        mime_type: "image/png".into(),
        data: Some("BINARY-HIDDEN".into()),
        uri: None,
    };
    let resource = |text: Option<&str>| ToolOutputBlock::Resource {
        uri: "fixture-resource".into(),
        mime_type: None,
        text: text.map(str::to_owned),
        data: Some("BINARY-HIDDEN".into()),
    };
    let cases = [
        (vec![image.clone()], "", "BUFFER-TEXT"),
        (
            vec![
                image.clone(),
                ToolOutputBlock::Text {
                    text: String::new(),
                },
            ],
            "",
            "BUFFER-TEXT",
        ),
        (vec![resource(Some(""))], "", "BUFFER-TEXT"),
        (
            vec![
                resource(None),
                ToolOutputBlock::Audio {
                    mime_type: "audio/wav".into(),
                    data: Some("BINARY-HIDDEN".into()),
                },
                ToolOutputBlock::ResourceLink {
                    uri: "fixture-link".into(),
                    name: "link".into(),
                    mime_type: None,
                },
            ],
            "",
            "BUFFER-TEXT",
        ),
        (vec![image.clone()], "EXPLICIT-TEXT", "EXPLICIT-TEXT"),
        (
            vec![
                image,
                ToolOutputBlock::Text {
                    text: "FINAL-TEXT".into(),
                },
            ],
            "FINAL-TEXT",
            "FINAL-TEXT",
        ),
        (vec![resource(Some("RESOURCE-TEXT"))], "", "RESOURCE-TEXT"),
    ];
    for (blocks, final_text, expected) in cases {
        let (context, projection) = project([
            Event::ToolCallContent {
                tool_call_id: "media".into(),
                content: "BUFFER-TEXT".into(),
            },
            Event::ToolCallCompleted {
                tool_call_id: "media".into(),
                is_error: false,
                content: final_text.into(),
                output: blocks,
                completed_at: at(),
                async_subagent: false,
            },
        ]);
        let document = context
            .store
            .with_transaction(|_| document(&context, &projection, &Theme::default(), None, || true))
            .unwrap();
        let mut copied = Vec::new();
        window::copy_all(&document, &mut copied, || true).unwrap();
        let copied = String::from_utf8(copied).unwrap();
        assert_eq!(copied.matches(expected).count(), 1);
        if expected != "BUFFER-TEXT" {
            assert!(!copied.contains("BUFFER-TEXT"));
        }
        assert!(!copied.contains("BINARY-HIDDEN"));
    }
}

#[test]
fn markdown_groups_and_complete_thoughts_keep_their_content() {
    let thought = "detail\n".repeat(100) + "THOUGHT-END";
    let (context, projection) = project([
        crate::acp::state::test_support::prompt("user\ncontinuation"),
        Event::AgentMessageChunk {
            text: "**bo".into(),
            message_id: Some("one".into()),
        },
        Event::AgentMessageChunk {
            text: "ld**\n\n| key | value |\n|---|---|\n| x | y |".into(),
            message_id: Some("one".into()),
        },
        Event::AgentThoughtChunk {
            text: thought.clone(),
            message_id: Some("thought".into()),
        },
        Event::ElicitationResolved {
            nonce: crate::acp::approvals::Nonce("n".into()),
            outcome: crate::acp::elicitations::ElicitationOutcome::Accepted,
            answers: vec![crate::acp::elicitations::ElicitationAnswer {
                question: "Proceed?".into(),
                answer: "Yes".into(),
            }],
        },
    ]);
    let document = context
        .store
        .with_transaction(|_| document(&context, &projection, &Theme::default(), None, || true))
        .unwrap();
    let index = context
        .store
        .with_transaction(|_| Index::build(&document, 80, || true))
        .unwrap();
    let first = index.window(&document, 0..10, || true).unwrap();
    assert!(first
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .any(|span| span.content.contains("bold")
            && span.style.add_modifier.contains(Modifier::BOLD)));
    let mut copied = Vec::new();
    window::copy_all(&document, &mut copied, || true).unwrap();
    let copied = String::from_utf8(copied).unwrap();
    assert!(copied.contains("› user\n  continuation"));
    assert!(copied.contains("key │ value"));
    assert!(copied.contains("THOUGHT-END"));
    assert_eq!(copied.matches("detail").count(), 100);
    assert!(copied.contains("› Proceed?: Yes"));
}
