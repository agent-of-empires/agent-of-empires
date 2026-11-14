use super::*;
use crate::acp::{
    retained::invocation::Replay,
    state::{DiffPreview, Event, ToolCall, ToolOutputBlock},
};
use crate::tui::rich_text::window::{self, Index};
use std::sync::{atomic::AtomicBool, Arc};

fn project(
    events: impl IntoIterator<Item = Event>,
) -> (Rc<Context>, Projection<Disk>, Invocations) {
    let context = Context::new(Arc::new(AtomicBool::new(false))).unwrap();
    let mut model = Replay::new(Rc::clone(&context)).unwrap();
    for (position, event) in events.into_iter().enumerate() {
        context
            .store
            .with_transaction(|_| {
                let mut writer = context.spool.writer();
                serde_json::to_writer(&mut writer, &event).unwrap();
                let value = context.parse(writer.finish()?)?;
                model.apply(position as u64 + 1, context.event(value)?)?;
                Ok(())
            })
            .unwrap();
    }
    let (projection, invocations) = model.into_projection().unwrap();
    (context, projection, invocations)
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
    let (context, projection, invocations) = project([
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "t".into(),
                name: "RetainedTool".into(),
                kind: "edit".into(),
                args_preview: arguments.clone(),
                prompt: None,
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
                    document(
                        &context,
                        &projection,
                        &invocations,
                        &theme,
                        Some(&roots),
                        || true,
                    )
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
        let (context, projection, invocations) = project([
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
            .with_transaction(|_| {
                document(
                    &context,
                    &projection,
                    &invocations,
                    &Theme::default(),
                    None,
                    || true,
                )
            })
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
    let (context, projection, invocations) = project([
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
        .with_transaction(|_| {
            document(
                &context,
                &projection,
                &invocations,
                &Theme::default(),
                None,
                || true,
            )
        })
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

#[test]
fn reused_tool_ids_keep_invocations_but_replace_terminal_snapshots() {
    let started = |name: &str| Event::ToolCallStarted {
        tool_call: ToolCall {
            id: "reused".into(),
            name: name.into(),
            kind: "read".into(),
            args_preview: format!("{name}-arguments"),
            prompt: None,
            started_at: at(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        },
    };
    let completed = |text: &str| Event::ToolCallCompleted {
        tool_call_id: "reused".into(),
        is_error: false,
        content: text.into(),
        output: Vec::new(),
        completed_at: at(),
        async_subagent: false,
    };
    let (context, projection, invocations) = project([
        started("FIRST-INVOCATION"),
        completed("STALE-FIRST-SNAPSHOT"),
        completed("LATEST-FIRST-SNAPSHOT"),
        started("SECOND-INVOCATION"),
        Event::Stopped {
            reason: "done".into(),
        },
    ]);
    for width in [32, 80, 32] {
        context
            .spool
            .with_speculation::<()>(|| {
                let document = context.store.with_transaction(|_| {
                    document(
                        &context,
                        &projection,
                        &invocations,
                        &Theme::default(),
                        None,
                        || true,
                    )
                })?;
                let index = context
                    .store
                    .with_transaction(|_| Index::build(&document, width, || true))?;
                let mut copied = Vec::new();
                window::copy_all(&document, &mut copied, || true)?;
                let copied = String::from_utf8(copied).unwrap();
                for name in ["FIRST-INVOCATION", "SECOND-INVOCATION"] {
                    assert!(copied.contains(&format!("{name}-arguments")), "{copied}");
                }
                assert_eq!(copied.matches("LATEST-FIRST-SNAPSHOT").count(), 1);
                assert!(!copied.contains("STALE-FIRST-SNAPSHOT"));
                let painted = index
                    .window(&document, 0..index.len(), || true)?
                    .to_string();
                assert!(painted.contains("LATEST-FIRST-SNAPSHOT"));
                Ok(None)
            })
            .unwrap();
    }
}

#[test]
fn reused_tool_id_with_empty_metadata_does_not_inherit_prior_arguments_or_diffs() {
    let first: ToolCall = ToolCall {
        id: "reused".into(),
        name: "FIRST-CALL".into(),
        kind: "edit".into(),
        args_preview: "FIRST-ARGUMENTS".into(),
        prompt: None,
        started_at: at(),
        parent_tool_call_id: None,
        memory_recall: None,
        diffs: vec![DiffPreview {
            path: "first.txt".into(),
            old_text: Some("FIRST-BEFORE".into()),
            new_text: Some("FIRST-AFTER".into()),
            created_at: at(),
        }],
    };
    let mut second = first.clone();
    second.name = "SECOND-CALL".into();
    second.args_preview.clear();
    second.diffs.clear();
    let (context, projection, invocations) = project([
        Event::ToolCallStarted { tool_call: first },
        Event::ToolCallCompleted {
            tool_call_id: "reused".into(),
            is_error: false,
            content: "FIRST-OUTPUT".into(),
            output: Vec::new(),
            completed_at: at(),
            async_subagent: false,
        },
        Event::ToolCallStarted { tool_call: second },
        Event::Stopped {
            reason: "done".into(),
        },
    ]);
    let document = context
        .store
        .with_transaction(|_| {
            document(
                &context,
                &projection,
                &invocations,
                &Theme::default(),
                None,
                || true,
            )
        })
        .unwrap();
    let mut copied = Vec::new();
    window::copy_all(&document, &mut copied, || true).unwrap();
    let copied = String::from_utf8(copied).unwrap();
    for token in [
        "FIRST-ARGUMENTS",
        "FIRST-BEFORE",
        "FIRST-AFTER",
        "FIRST-OUTPUT",
    ] {
        assert_eq!(copied.matches(token).count(), 1, "{copied}");
        assert!(
            !copied.split("SECOND-CALL").nth(1).unwrap().contains(token),
            "{copied}"
        );
    }
    for width in [40, 80] {
        let index = context
            .store
            .with_transaction(|_| Index::build(&document, width, || true))
            .unwrap();
        let painted = index
            .window(&document, 0..index.len(), || true)
            .unwrap()
            .to_string();
        assert!(painted.contains("FIRST-ARGUMENTS"));
        assert!(painted.contains("FIRST-AFTER"));
        assert!(!painted
            .split("SECOND-CALL")
            .nth(1)
            .unwrap()
            .contains("FIRST-"));
    }
}
