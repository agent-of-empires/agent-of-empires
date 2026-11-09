use super::*;
use crate::acp::state::{Event, WorkflowLaunch};

#[test]
fn retained_source_preserves_byte_offsets_identity_and_private_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let store = EventStore::open(&directory.path().join("events.db"), 100).unwrap();
    let event = Event::AgentMessageChunk {
        text: "é🦀".repeat(40_000),
        message_id: Some("m".into()),
    };
    let expected = serde_json::to_vec(&event).unwrap();
    store.record("s", 1, &event).unwrap();
    store
        .record(
            "s",
            2,
            &Event::BackgroundAgentLaunched {
                agent_id: "a".into(),
                tool_call_id: "t".into(),
                description: "task".into(),
                prompt: "prompt".into(),
                model: "model".into(),
                output_file: "/private/agent/transcript.jsonl".into(),
                started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                workflow: None,
            },
        )
        .unwrap();
    store
        .record(
            "s",
            3,
            &Event::BackgroundTaskStarted {
                task_id: "workflow".into(),
                tool_call_id: Some("w".into()),
                description: "work".into(),
                workflow: Some(WorkflowLaunch {
                    run_id: "run".into(),
                    name: "name".into(),
                    summary: None,
                    transcript_dir: "/private/workflow/transcripts".into(),
                }),
            },
        )
        .unwrap();
    let mut query = RetainedSourceQuery::default();
    let mut actual = Vec::new();
    loop {
        let RetainedSourceRead::Block { meta, bytes } = store.retained_source("s", &query).unwrap()
        else {
            panic!("source block expected")
        };
        assert_eq!(
            (meta.highest_seq, meta.lowest_seq, meta.seq),
            (3, Some(1), Some(1))
        );
        assert_eq!(meta.offset, actual.len() as u64);
        assert_eq!(meta.total_bytes, expected.len() as u64);
        assert!(!meta.omitted && !meta.invalid);
        assert!(bytes.len() <= RETAINED_SOURCE_BLOCK_BYTES);
        if let Some(identity) = &query.history_identity {
            assert_eq!(&meta.history_identity, identity);
        }
        query.history_identity = Some(meta.history_identity);
        actual.extend(bytes);
        if actual.len() == expected.len() {
            break;
        }
        query.seq = meta.seq;
        query.offset = actual.len() as u64;
    }
    assert_eq!(actual, expected);
    for sequence in [2, 3] {
        query.after = sequence - 1;
        query.seq = None;
        query.offset = 0;
        let RetainedSourceRead::Block { meta, bytes } = store.retained_source("s", &query).unwrap()
        else {
            panic!("metadata boundary expected")
        };
        assert_eq!(
            (
                meta.seq,
                meta.total_bytes,
                meta.offset,
                meta.omitted,
                meta.invalid
            ),
            (Some(sequence), 0, 0, true, false)
        );
        assert_eq!(bytes, b"");
        query.seq = Some(sequence);
        query.offset = 1;
        assert!(matches!(
            store.retained_source("s", &query).unwrap(),
            RetainedSourceRead::InvalidCursor
        ));
    }
    query.after = 3;
    query.seq = None;
    query.offset = 0;
    let RetainedSourceRead::Block { meta, bytes } = store.retained_source("s", &query).unwrap()
    else {
        panic!("end of history expected")
    };
    assert_eq!(meta.seq, None);
    assert_eq!(bytes, b"");
    let replacement = serde_json::to_string(&Event::AgentMessageChunk {
        text: "è🍐".repeat(40_000),
        message_id: Some("m".into()),
    })
    .unwrap();
    assert_eq!(replacement.len(), expected.len());
    store
        .conn()
        .execute(
            "UPDATE acp_events SET event_json = ?1 WHERE session_id = 's' AND seq = 1",
            [replacement],
        )
        .unwrap();
    query.after = 0;
    assert!(matches!(
        store.retained_source("s", &query).unwrap(),
        RetainedSourceRead::Changed
    ));

    let json: String = store
        .conn()
        .query_row(
            "SELECT event_json FROM acp_events WHERE session_id = 's' AND seq = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let escaped = json.replacen(
        "BackgroundAgentLaunched",
        "\\u0042ackgroundAgentLaunched",
        1,
    );
    store
        .conn()
        .execute(
            "UPDATE acp_events SET event_json = ?1 WHERE session_id = 's' AND seq = 2",
            [escaped],
        )
        .unwrap();
    query.history_identity = None;
    query.after = 1;
    let RetainedSourceRead::Block { meta, bytes } = store.retained_source("s", &query).unwrap()
    else {
        panic!("private metadata boundary expected")
    };
    assert_eq!((meta.seq, meta.omitted), (Some(2), true));
    assert_eq!(bytes, b"");
}
