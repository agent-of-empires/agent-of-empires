use super::*;
use crate::acp::state::{Event, WorkflowLaunch};

#[tokio::test]
async fn retained_source_snapshot_does_not_block_publication_and_rejects_stale_identity() {
    use crate::acp::event_store::test_support::{gate_retained_read, RetainedReadStage};
    use crate::acp::supervisor::{BroadcastSink, ChannelSink};
    use axum::{body::Body, http::Request, routing::get, Router};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use tower::ServiceExt;

    let instance = crate::session::Instance::new("concurrent retained source", "/tmp");
    let id = instance.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![instance]);
    let store = Arc::clone(&state.acp_event_store);
    let event = Event::AgentMessageChunk {
        text: "old snapshot bytes".into(),
        message_id: None,
    };
    store.record(&id, 1, &event).unwrap();
    let expected = serde_json::to_vec(&event).unwrap();
    let (entered, release) = gate_retained_read(&store, &id, RetainedReadStage::Snapshot);
    let reader_store = Arc::clone(&store);
    let reader_id = id.clone();
    let reader = std::thread::spawn(move || {
        reader_store.retained_source(&reader_id, &RetainedSourceQuery::default())
    });
    let deadline = Duration::from_secs(5);
    entered
        .recv_timeout(deadline)
        .expect("source reader must establish its snapshot");
    let (tx, mut broadcasts) = tokio::sync::broadcast::channel(4);
    let sink = ChannelSink::new(
        tx,
        Arc::clone(&store),
        Arc::new(crate::acp::control_cache::ControlStateCache::new()),
    );
    let (done, completed) = mpsc::channel();
    let writer_id = id.clone();
    let writer = std::thread::spawn(move || {
        let persisted = sink.publish_persisted("unrelated", 1, &Event::ThinkingStarted)
            && sink.publish_persisted(&writer_id, 2, &Event::ThinkingEnded);
        done.send(persisted).unwrap();
    });
    let publication = completed.recv_timeout(deadline);
    release.send(()).unwrap();
    let result = reader.join().unwrap().unwrap();
    writer.join().unwrap();
    assert!(publication.expect("source snapshot must not block live publication"));
    assert_eq!(broadcasts.try_recv().unwrap().session_id, "unrelated");
    let published = broadcasts.try_recv().unwrap();
    assert_eq!(
        (published.session_id.as_str(), published.seq),
        (id.as_str(), 2)
    );
    assert!(matches!(published.event.as_ref(), Event::ThinkingEnded));
    let RetainedSourceRead::Block { meta, bytes } = result else {
        panic!("coherent source block expected");
    };
    assert_eq!(bytes, expected);
    assert!(bytes.len() <= RETAINED_SOURCE_BLOCK_BYTES);
    assert_eq!(
        (meta.highest_seq, meta.lowest_seq, meta.seq),
        (1, Some(1), Some(1))
    );
    assert!(!meta.omitted && !meta.invalid);
    let RetainedSourceRead::Block { meta: latest, .. } = store
        .retained_source(&id, &RetainedSourceQuery::default())
        .unwrap()
    else {
        panic!("new source snapshot expected");
    };
    assert_eq!(latest.highest_seq, 2);
    assert_ne!(latest.history_identity, meta.history_identity);
    let app = Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            get(crate::server::api::acp_retained_source),
        )
        .with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/{id}/acp/retained-source?history_identity={}",
                    meta.history_identity
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
}

#[test]
fn retained_source_reads_the_written_private_database() {
    for name in [":memory:", ""] {
        let store = EventStore::open(std::path::Path::new(name), 0).unwrap();
        let event = Event::AgentMessageChunk {
            text: "private source".into(),
            message_id: None,
        };
        store.record("private", 1, &event).unwrap();
        let RetainedSourceRead::Block { meta, bytes } = store
            .retained_source("private", &RetainedSourceQuery::default())
            .unwrap()
        else {
            panic!("private source block expected");
        };
        assert_eq!(bytes, serde_json::to_vec(&event).unwrap());
        assert_eq!(
            (meta.highest_seq, meta.lowest_seq, meta.seq),
            (1, Some(1), Some(1))
        );
    }
}

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
                started_at: None,
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

#[tokio::test]
async fn malformed_retained_events_preserve_valid_output_and_report_incompleteness() {
    use crate::acp::{
        client::HttpClient,
        retained::load::{self, Cancellation},
        state::ToolCall,
        transcript::storage::Rows,
    };

    let mut instance = crate::session::Instance::new("invalid retained events", "/tmp");
    instance.trash();
    let session = instance.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![instance]);
    let at = chrono::DateTime::from_timestamp(100, 0).unwrap();
    let events = [
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "question".into(),
                name: "Ask".into(),
                kind: "execute".into(),
                args_preview: "{}".into(),
                prompt: None,
                started_at: at,
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
        },
        Event::ToolCallContent {
            tool_call_id: "question".into(),
            content: "KEEP-TOOL-OUTPUT".into(),
        },
        Event::ThinkingEnded,
        Event::Stopped {
            reason: "end_turn".into(),
        },
        Event::AgentMessageChunk {
            text: "LATER-CONTENT".into(),
            message_id: None,
        },
    ];
    for (index, event) in events.iter().enumerate() {
        state
            .acp_event_store
            .record(&session, index as u64 + 1, event)
            .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = crate::daemon::DaemonEndpoint::new(
        format!("http://{}", listener.local_addr().unwrap()),
        None,
        crate::daemon::Source::Env,
    );
    let router = axum::Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            axum::routing::get(crate::server::api::acp_retained_source),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = HttpClient::new(endpoint).unwrap();
    let mut observations = Vec::new();
    for malformed in [
        r#"{"Bogus":{}}"#,
        r#"{"ElicitationRequested":{"elicitation":{"tool_call_id":"question"}}}"#,
        r#"{"ElicitationRequested":{"elicitation":{"nonce":"n","message":"Pick","tool_call_id":"question","questions":[{"field_key":"q","required":true,"kind":"free_text"}],"requested_at":"1970-01-01T00:01:40Z"}}}"#,
        r#"{"ElicitationResolved":{"nonce":"n","answers":[{"question":"FORGED","answer":"FORGED"}]}}"#,
    ] {
        assert!(serde_json::from_str::<Event>(malformed).is_err());
        state
            .acp_event_store
            .conn()
            .execute(
                "UPDATE acp_events SET event_json = ?1 WHERE session_id = ?2 AND seq = 3",
                rusqlite::params![malformed, session],
            )
            .unwrap();
        let client = client.clone();
        let session = session.clone();
        let runtime = tokio::runtime::Handle::current();
        let (incomplete, text) = tokio::task::spawn_blocking(move || {
            let loaded = load::read(&client, &session, &runtime, &Cancellation::default())?;
            let mut bytes = Vec::new();
            for index in 0..loaded.invocations.rows.len() {
                if !loaded.invocations.visible(index)? {
                    continue;
                }
                let row = loaded.invocations.rows.read(index)?;
                loaded.context.spool.copy_to(row.text, &mut bytes)?;
            }
            for output in loaded.projection.unfinished.iter() {
                loaded.context.spool.copy_to(output?.text, &mut bytes)?;
            }
            anyhow::Ok((loaded.incomplete, String::from_utf8(bytes)?))
        })
        .await
        .unwrap()
        .unwrap();
        observations.push((
            malformed,
            incomplete,
            text.contains("KEEP-TOOL-OUTPUT"),
            text.contains("LATER-CONTENT"),
            !text.contains("FORGED"),
        ));
    }
    server.abort();
    let _ = server.await;
    assert!(
        observations
            .iter()
            .all(|(_, incomplete, tool, later, no_forged)| *incomplete
                && *tool
                && *later
                && *no_forged),
        "{observations:?}"
    );
}

#[test]
fn complete_retained_reads_have_subquadratic_sqlite_work() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn steps(count: u64, replay: bool) -> usize {
        let directory = tempfile::tempdir().unwrap();
        let store = EventStore::open(&directory.path().join("events.db"), 1024).unwrap();
        for sequence in 1..=count {
            store
                .record(
                    "s",
                    sequence,
                    &Event::AgentMessageChunk {
                        text: format!("part-{sequence}"),
                        message_id: None,
                    },
                )
                .unwrap();
        }
        let steps = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&steps);
        store
            .read_conn()
            .progress_handler(
                1,
                Some(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .unwrap();
        let mut after = 0;
        let mut identity = None;
        loop {
            let next = if replay {
                let (mut page, current) = store
                    .retained_page("s", after, None, 1, identity.as_deref())
                    .unwrap()
                    .unwrap();
                identity = Some(current);
                page.events.pop()
            } else {
                let query = RetainedSourceQuery {
                    after,
                    history_identity: identity.clone(),
                    ..Default::default()
                };
                let RetainedSourceRead::Block { meta, bytes } =
                    store.retained_source("s", &query).unwrap()
                else {
                    panic!("pinned source block expected")
                };
                identity = Some(meta.history_identity);
                meta.seq
                    .map(|sequence| (sequence, serde_json::from_slice::<Event>(&bytes).unwrap()))
            };
            let Some((sequence, event)) = next else { break };
            assert_eq!(sequence, after + 1);
            let Event::AgentMessageChunk { text, .. } = event else {
                panic!("message expected")
            };
            assert_eq!(text, format!("part-{sequence}"));
            after = sequence;
        }
        assert_eq!(after, count);
        store
            .read_conn()
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        steps.load(Ordering::Relaxed)
    }

    for replay in [false, true] {
        let small = steps(128, replay);
        let large = steps(256, replay);
        println!("retained replay={replay}: 128 events={small} VM steps, 256 events={large}");
        assert!(
            large < 3 * small,
            "quadratic retained read: {small} -> {large}"
        );
    }
}
