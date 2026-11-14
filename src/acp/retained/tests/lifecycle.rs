use super::*;
use crate::acp::state::test_support::stopped;

fn content(id: &str, text: &str) -> Event {
    Event::ToolCallContent {
        tool_call_id: id.into(),
        content: text.into(),
    }
}
fn complete(id: &str, text: &str) -> Event {
    Event::ToolCallCompleted {
        tool_call_id: id.into(),
        is_error: false,
        content: text.into(),
        output: Vec::new(),
        completed_at: at(),
        async_subagent: false,
    }
}
fn question(id: &str) -> Event {
    Event::ElicitationRequested {
        elicitation: Elicitation {
            nonce: Nonce("n".into()),
            message: "Pick one".into(),
            title: None,
            description: None,
            tool_call_id: Some(id.into()),
            questions: Vec::new(),
            requested_at: at(),
            resolved: None,
        },
    }
}
fn fold(
    events: &[Event],
    split: usize,
) -> (
    Rc<Context>,
    crate::acp::transcript::reducer::Projection<Disk>,
    Vec<String>,
) {
    let context = Context::new(Arc::new(AtomicBool::new(false))).unwrap();
    let mut model = Core::new(Disk(Rc::clone(&context))).unwrap();
    let mut stopped = Vec::new();
    context
        .store
        .with_transaction(|_| {
            for (index, event) in events.iter().enumerate() {
                let mut writer = context.spool.writer();
                serde_json::to_writer(&mut writer, event).unwrap();
                let value = context.parse(writer.finish()?)?;
                let event = context.event(value)?;
                if index < split {
                    model.observe_prior(index as u64 + 1, event)?;
                } else {
                    for delta in model.apply(index as u64 + 1, event)?.into_iter() {
                        match delta? {
                            TranscriptDelta::Append(row) | TranscriptDelta::Patch { row, .. }
                                if row.kind == TranscriptRowKind::ToolStopped =>
                            {
                                stopped.push(text(&context, row.text));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    (context, model.into_projection().unwrap(), stopped)
}
fn unfinished(
    context: &Context,
    projection: &crate::acp::transcript::reducer::Projection<Disk>,
) -> Vec<(String, String)> {
    projection
        .unfinished
        .iter()
        .map(|output| {
            let output = output.unwrap();
            (
                text(context, output.tool_call_id),
                text(context, output.text),
            )
        })
        .collect()
}

#[test]
fn cursor_boundaries_preserve_latest_output_order_reuse_and_question_suppression() {
    let cases = [
        (
            vec![
                tool("a"),
                tool("b"),
                content("b", "old"),
                content("a", "second"),
                content("b", "latest"),
            ],
            vec![("b", "latest"), ("a", "second")],
        ),
        (
            vec![tool("a"), complete("a", "done"), content("a", "late")],
            vec![("a", "late")],
        ),
        (
            vec![
                complete("a", "done"),
                content("a", "old invocation"),
                tool("a"),
                content("b", "first"),
                content("a", "new invocation"),
            ],
            vec![("b", "first"), ("a", "new invocation")],
        ),
        (
            vec![
                content("q", "hidden"),
                question("q"),
                content("q", "still hidden"),
                complete("q", ""),
                content("q", "ordinary reuse"),
            ],
            vec![("q", "ordinary reuse")],
        ),
        (
            vec![
                content("a", "old"),
                content("b", "second"),
                content("a", "latest"),
            ],
            vec![("a", "latest"), ("b", "second")],
        ),
        (vec![content("a", "old"), content("a", "")], vec![]),
        (
            vec![
                content("q", "hidden"),
                question("q"),
                content("q", "still hidden"),
            ],
            vec![],
        ),
        (vec![content("a", "old"), complete("a", "final")], vec![]),
    ];
    for (events, expected) in cases {
        let expected = expected
            .iter()
            .map(|(id, text)| ((*id).to_owned(), (*text).to_owned()))
            .collect::<Vec<_>>();
        for split in 0..=events.len() {
            let (context, projection, _) = fold(&events, split);
            assert_eq!(
                unfinished(&context, &projection),
                expected,
                "split={split}, events={events:?}"
            );
        }
    }
}

#[test]
fn turn_endings_keep_orphans_separate_and_sweep_open_prefix_tools() {
    let closers = [
        stopped("cancelled"),
        Event::startup_error("boom"),
        Event::IncompatibleAgent {
            detail: crate::acp::state::StartupErrorDetail::UnsupportedProtocolVersion {
                expected: "1".into(),
                received: "2".into(),
            },
        },
        Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "rate_limit".into(),
        },
    ];
    for closer in closers {
        for split in 0..=2 {
            for completed in [false, true] {
                let mut events = vec![content("t", "orphan text"), closer.clone()];
                if completed {
                    events.push(complete("t", ""));
                }
                let (context, projection, stopped) = fold(&events, split);
                assert_eq!(
                    unfinished(&context, &projection),
                    [("t".into(), "orphan text".into())]
                );
                assert!(stopped.is_empty());
            }
        }
        for split in 0..=3 {
            let events = [
                tool("t"),
                content("t", "buffered before cursor"),
                closer.clone(),
                stopped("cancelled"),
            ];
            let (context, projection, stopped) = fold(&events, split);
            assert!(unfinished(&context, &projection).is_empty());
            assert_eq!(
                stopped,
                if split == 3 {
                    vec![]
                } else {
                    vec!["buffered before cursor".to_owned()]
                }
            );
        }
    }
}
