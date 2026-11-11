use super::*;
use crate::acp::transcript::{storage::DeltaOf, TranscriptModel, TranscriptRow};

type ToolSignature = (String, String, String, String, DateTime<Utc>);
type RowSignature = (
    String,
    String,
    TranscriptRowKind,
    String,
    Option<String>,
    Option<ToolSignature>,
);
type DeltaSignature = (u8, String, Option<RowSignature>);

fn memory_row(row: &TranscriptRow) -> RowSignature {
    (
        row.id.clone(),
        row.group_id.clone(),
        row.kind.clone(),
        row.text.clone(),
        row.tool_call_id.clone(),
        row.tool.as_ref().map(|tool| {
            (
                tool.id.clone(),
                tool.name.clone(),
                tool.kind.clone(),
                tool.args_preview.clone(),
                tool.started_at,
            )
        }),
    )
}

fn memory_delta(delta: &TranscriptDelta) -> DeltaSignature {
    match delta {
        TranscriptDelta::Append(row) => (0, row.id.clone(), Some(memory_row(row))),
        TranscriptDelta::Patch { id, row } => (1, id.clone(), Some(memory_row(row))),
        TranscriptDelta::Remove(id) => (2, id.clone(), None),
    }
}

fn disk_delta(context: &Context, delta: &DeltaOf<Disk>) -> DeltaSignature {
    let row = match delta {
        TranscriptDelta::Append(row) | TranscriptDelta::Patch { row, .. } => row,
        TranscriptDelta::Remove(id) => return (2, text(context, *id), None),
    };
    let signature = (
        text(context, row.id),
        text(context, row.group_id),
        row.kind.clone(),
        text(context, row.text),
        row.tool_call_id.map(|id| text(context, id)),
        row.tool.as_ref().map(|tool| {
            (
                text(context, tool.id),
                text(context, tool.name),
                text(context, tool.kind),
                text(context, tool.args_preview),
                tool.started_at,
            )
        }),
    );
    match delta {
        TranscriptDelta::Append(_) => (0, signature.0.clone(), Some(signature)),
        TranscriptDelta::Patch { id, .. } => (1, text(context, *id), Some(signature)),
        TranscriptDelta::Remove(_) => unreachable!(),
    }
}

fn compare(events: &[Event], split: usize) {
    let context = Context::new(Arc::new(AtomicBool::new(false))).unwrap();
    let mut disk = Core::new(Disk(Rc::clone(&context))).unwrap();
    let mut memory = TranscriptModel::new();
    context
        .store
        .with_transaction(|_| {
            for (index, event) in events.iter().enumerate() {
                let seq = index as u64 + 1;
                let mut writer = context.spool.writer();
                serde_json::to_writer(&mut writer, event).unwrap();
                let value = context.parse(writer.finish()?)?;
                let input = context.event(value)?;
                if index < split {
                    disk.observe_prior(seq, input)?;
                    memory.observe_prior(seq, event);
                } else {
                    let actual = disk
                        .apply(seq, input)?
                        .into_iter()
                        .map(|delta| disk_delta(&context, &delta.unwrap()))
                        .collect::<Vec<_>>();
                    let expected = memory
                        .apply_event(seq, event)
                        .iter()
                        .map(memory_delta)
                        .collect::<Vec<_>>();
                    assert_eq!(actual, expected, "event={index}, split={split}: {event:?}");
                }
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn retained_distinct_tool_starts_have_linear_row_read_work() {
    let mut counts = Vec::new();
    for count in [32, 64] {
        let events = (0..count).map(|id| tool(&format!("tool-{id}"))).collect::<Vec<_>>();
        super::super::records::ROW_READS.with(|reads| reads.set(0));
        compare(&events, 0);
        counts.push(super::super::records::ROW_READS.with(|reads| reads.replace(0)));
    }
    assert!(
        counts[0] <= 8 * 32 && counts[1] <= 8 * 64,
        "distinct starts must not scan preceding disk rows: row reads for 32/64 starts = {counts:?}"
    );
    assert!(
        counts[1] <= counts[0].saturating_mul(3).max(64),
        "doubling distinct starts must not quadruple disk row reads: {counts:?}"
    );
}

#[test]
fn retained_indexed_tool_lifecycle_matches_memory_across_replay_boundaries() {
    let mut richer = tool("a");
    if let Event::ToolCallStarted { tool_call } = &mut richer {
        tool_call.name = "Richer duplicate".into();
        tool_call.args_preview = "{\"path\":\"kept\"}".into();
    }
    let update = |id: &str| Event::ToolCallUpdated {
        tool_call_id: id.into(),
        title: Some("Retitled".into()),
        args_preview: Some("{\"path\":\"updated\"}".into()),
        started_at: Some(at()),
        diffs: None,
    };
    let events = vec![
        tool("a"),
        tool("b"),
        richer,
        update("b"),
        Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: Nonce("question".into()),
                message: "Pick".into(),
                title: None,
                description: None,
                tool_call_id: Some("a".into()),
                questions: Vec::new(),
                requested_at: at(),
                resolved: None,
            },
        },
        tool("b"),
        Event::ToolCallCompleted {
            tool_call_id: "b".into(),
            is_error: false,
            content: "complete".into(),
            output: Vec::new(),
            completed_at: at(),
            async_subagent: false,
        },
        tool("b"),
        Event::SessionCleared,
        tool("a"),
        update("a"),
        Event::Stopped { reason: "done".into() },
        tool("a"),
    ];
    for split in 0..=events.len() {
        super::super::records::ROW_READS.with(|reads| reads.set(0));
        compare(&events, split);
        super::super::records::ROW_READS.with(|reads| reads.set(0));
    }
}
