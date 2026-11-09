use super::*;
use crate::acp::{
    approvals::Nonce,
    elicitations::Elicitation,
    state::{Event, ToolCall},
    transcript::{
        reducer::Core,
        storage::{Changes, Rows},
        TranscriptDelta, TranscriptRowKind,
    },
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

fn at() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}
fn tool(id: &str) -> Event {
    Event::ToolCallStarted {
        tool_call: ToolCall {
            id: id.into(),
            name: "Read".into(),
            kind: "read".into(),
            args_preview: "{}".into(),
            started_at: at(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        },
    }
}
fn feed(context: &Context, model: &mut Core<Disk>, seq: u64, event: Event) {
    let mut writer = context.spool.writer();
    serde_json::to_writer(&mut writer, &event).unwrap();
    let input = writer.finish().unwrap();
    let value = context.parse(input).unwrap();
    let event = context.event(value).unwrap();
    for delta in model.apply(seq, event).unwrap().into_iter() {
        delta.unwrap();
    }
}
fn text(context: &Context, value: TextRef) -> String {
    let mut bytes = Vec::new();
    context.spool.copy_to(value, &mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}
struct Hash(Sha256);
impl Write for Hash {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn retained_snapshots_survive_eviction_reuse_and_row_removal() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let context = Context::new(Arc::clone(&cancelled)).unwrap();
    let mut model = Core::new(Disk(Rc::clone(&context))).unwrap();
    feed(
        &context,
        &mut model,
        1,
        Event::ToolCallContent {
            tool_call_id: "reused".into(),
            content: "orphan".into(),
        },
    );
    feed(
        &context,
        &mut model,
        2,
        Event::Stopped {
            reason: "complete".into(),
        },
    );
    feed(&context, &mut model, 3, tool("reused"));
    feed(
        &context,
        &mut model,
        4,
        Event::ToolCallContent {
            tool_call_id: "reused".into(),
            content: "first snapshot".into(),
        },
    );
    feed(
        &context,
        &mut model,
        5,
        Event::ToolCallCompleted {
            tool_call_id: "reused".into(),
            is_error: false,
            content: String::new(),
            output: Vec::new(),
            completed_at: at(),
            async_subagent: false,
        },
    );
    feed(
        &context,
        &mut model,
        6,
        Event::ToolCallContent {
            tool_call_id: "reused".into(),
            content: "stale terminal output".into(),
        },
    );
    feed(&context, &mut model, 7, tool("reused"));
    feed(
        &context,
        &mut model,
        8,
        Event::Stopped {
            reason: "complete".into(),
        },
    );
    feed(&context, &mut model, 9, tool("ask"));
    feed(
        &context,
        &mut model,
        10,
        Event::ToolCallContent {
            tool_call_id: "ask".into(),
            content: "suppressed".into(),
        },
    );

    let mut writer = context.spool.writer();
    writer
        .write_all(br#"{"AgentMessageChunk":{"text":""#)
        .unwrap();
    let mut expected = Sha256::new();
    let chunk = "é🦀# hello ";
    for _ in 0..65_536 {
        writer.write_all(chunk.as_bytes()).unwrap();
        expected.update(chunk.as_bytes());
    }
    writer.write_all(br#"","message_id":"retained"}}"#).unwrap();
    let input = writer.finish().unwrap();
    let value = context.parse(input).unwrap();
    for delta in model
        .apply(11, context.event(value).unwrap())
        .unwrap()
        .into_iter()
    {
        delta.unwrap();
    }

    let elicitation = Elicitation {
        nonce: Nonce("n".into()),
        message: "Question".into(),
        title: None,
        description: None,
        tool_call_id: Some("ask".into()),
        questions: Vec::new(),
        requested_at: at(),
        resolved: None,
    };
    let event = Event::ElicitationRequested { elicitation };
    let mut writer = context.spool.writer();
    serde_json::to_writer(&mut writer, &event).unwrap();
    let value = context.parse(writer.finish().unwrap()).unwrap();
    let mut removed = Vec::new();
    for delta in model
        .apply(12, context.event(value).unwrap())
        .unwrap()
        .into_iter()
    {
        if let TranscriptDelta::Remove(id) = delta.unwrap() {
            removed.push(text(&context, id));
        }
    }
    assert_eq!(removed, ["start-ask"]);

    let mut eviction = context.spool.writer();
    for _ in 0..128 {
        eviction.write_all(&[b'x'; 8192]).unwrap();
    }
    let eviction = eviction.finish().unwrap();
    context.spool.copy_to(eviction, &mut io::sink()).unwrap();

    let projection = model.into_projection().unwrap();
    let mut ids = Vec::new();
    for index in 0..projection.rows.len() {
        let row = projection.rows.read(index).unwrap();
        let id = text(&context, row.id);
        match id.as_str() {
            "done-reused" => {
                assert_eq!(row.kind, TranscriptRowKind::ToolComplete);
                assert_eq!(text(&context, row.text), "first snapshot");
            }
            "stopped-reused-8" => {
                assert_eq!(row.kind, TranscriptRowKind::ToolStopped);
                assert_eq!(text(&context, row.text), "");
            }
            "msg-11" => {
                let mut hash = Hash(Sha256::new());
                let copied = context.spool.copy_to(row.text, &mut hash).unwrap();
                assert_eq!(copied, chunk.len() as u64 * 65_536);
                assert_eq!(hash.0.finalize(), expected.clone().finalize());
            }
            _ => {}
        }
        ids.push(id);
    }
    assert_eq!(
        ids,
        ["start-reused", "done-reused", "stopped-reused-8", "msg-11"]
    );
    let unfinished = projection
        .unfinished
        .iter()
        .map(|value| {
            let value = value.unwrap();
            (
                text(&context, value.tool_call_id),
                text(&context, value.text),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(unfinished, [("reused".into(), "orphan".into())]);
    cancelled.store(true, Ordering::Relaxed);
    assert_eq!(
        projection.rows.read(0).err().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn timestamps_preserve_chrono_grammar_without_payload_sized_strings() {
    let context = Context::new(Arc::new(AtomicBool::new(false))).unwrap();
    for value in [
        "2000-01-01T00:00:00Z",
        "2000-1-1 0:0:0.123456789987654321UTC",
        "+000000000002000-1-1T0:0:0+0000",
        "-0000-1-1T0:0:0Z",
        "2000\u{2002}-1-1 0 :0:0\tUTC",
        "2000-1-1\u{2002}0:0:0Z",
        "2000-1-1T0:0:0.Z",
        "2000-1-1T0:0:0. Z",
        "2000-001-1T0:0:0Z",
        "- 0001-1-1T0:0:0Z",
        "2000-1-1T0:0:0.123 456Z",
        "not a date",
    ] {
        let expected = value.parse::<DateTime<Utc>>().ok();
        let source = context.spool.append(value.as_bytes()).unwrap();
        assert_eq!(date::read(&context, source).ok(), expected, "{value:?}");
    }
    let mut writer = context.spool.writer();
    writer.write_all(b"+000").unwrap();
    for _ in 0..128 {
        writer.write_all(&[b'0'; 8192]).unwrap();
    }
    writer.write_all(b"2000-1-1T0:0:0.").unwrap();
    for _ in 0..128 {
        writer.write_all(&[b'1'; 8192]).unwrap();
    }
    writer.write_all(b"Z").unwrap();
    let source = writer.finish().unwrap();
    assert_eq!(
        date::read(&context, source).unwrap(),
        "2000-01-01T00:00:00.111111111Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
    );
}

mod lifecycle;
