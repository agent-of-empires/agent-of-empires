use super::*;
use crate::acp::{
    approvals::Nonce,
    elicitations::Elicitation,
    state::{Event, ToolCall},
    transcript::{storage::Rows, TranscriptRowKind},
};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Router,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct Snapshot {
    rows: Vec<(TranscriptRowKind, String, String, Option<String>)>,
    unfinished: Vec<(String, String)>,
    incomplete: bool,
    highest: u64,
}
fn string(context: &Context, text: pulldown_cmark::disk::TextRef) -> String {
    let mut bytes = Vec::new();
    context.spool.copy_to(text, &mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}
async fn snapshot(client: HttpClient, session: String) -> anyhow::Result<Snapshot> {
    let runtime = Handle::current();
    tokio::task::spawn_blocking(move || {
        let loaded = read(&client, &session, &runtime, &Cancellation::default())?;
        let mut rows = Vec::new();
        for position in 0..loaded.projection.rows.len() {
            let row = loaded.projection.rows.read(position)?;
            rows.push((
                row.kind,
                string(&loaded.context, row.text),
                string(&loaded.context, row.group_id),
                row.tool_call_id.map(|id| string(&loaded.context, id)),
            ));
        }
        let unfinished = loaded
            .projection
            .unfinished
            .iter()
            .map(|output| {
                let output = output?;
                Ok((
                    string(&loaded.context, output.tool_call_id),
                    string(&loaded.context, output.text),
                ))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Snapshot {
            rows,
            unfinished,
            incomplete: loaded.incomplete,
            highest: loaded.highest_seq,
        })
    })
    .await?
}
async fn serve(router: Router) -> (HttpClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = crate::daemon::DaemonEndpoint::new(
        format!("http://{}", listener.local_addr().unwrap()),
        None,
        crate::daemon::Source::Env,
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (HttpClient::new(endpoint).unwrap(), server)
}

#[tokio::test]
async fn retained_blocks_preserve_groups_output_precedence_and_elicitation_removal() {
    let inst = crate::session::Instance::new("retained blocks", "/tmp");
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let at = chrono::DateTime::from_timestamp(100, 0).unwrap();
    let first = format!("first{}", "é🦀".repeat(16_384));
    let completed = |id: &str| Event::ToolCallCompleted {
        tool_call_id: id.into(),
        is_error: false,
        content: String::new(),
        output: Vec::new(),
        completed_at: at,
        async_subagent: false,
    };
    let events = [
        Event::AgentMessageChunk {
            text: first.clone(),
            message_id: Some("a".into()),
        },
        Event::AgentMessageChunk {
            text: " continuation".into(),
            message_id: Some("a".into()),
        },
        Event::AgentMessageChunk {
            text: "second".into(),
            message_id: Some("b".into()),
        },
        Event::ToolCallContent {
            tool_call_id: "output".into(),
            content: "retained output".into(),
        },
        completed("output"),
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "question".into(),
                name: "Ask".into(),
                kind: "execute".into(),
                args_preview: "{}".into(),
                started_at: at,
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
        },
        Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: Nonce("n".into()),
                message: "Pick one".into(),
                title: None,
                description: None,
                tool_call_id: Some("question".into()),
                questions: Vec::new(),
                requested_at: at,
                resolved: None,
            },
        },
        completed("question"),
        Event::ToolCallContent {
            tool_call_id: "orphan".into(),
            content: "old orphan output".into(),
        },
        Event::ToolCallContent {
            tool_call_id: "orphan".into(),
            content: "latest orphan output".into(),
        },
    ];
    for (index, event) in events.iter().enumerate() {
        state
            .acp_event_store
            .record(&id, index as u64 + 1, event)
            .unwrap();
    }
    let (client, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state),
    )
    .await;
    let result = snapshot(client, id).await.unwrap();
    assert!(!result.incomplete);
    let messages = result
        .rows
        .iter()
        .filter(|row| row.0 == TranscriptRowKind::Message)
        .collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .map(|row| row.1.as_str())
            .collect::<Vec<_>>(),
        [first.as_str(), " continuation", "second"]
    );
    assert_eq!(messages[0].2, messages[1].2);
    assert_ne!(messages[1].2, messages[2].2);
    assert_eq!(
        result
            .rows
            .iter()
            .find(|row| row.0 == TranscriptRowKind::ToolComplete)
            .unwrap()
            .1,
        "retained output"
    );
    assert!(!result
        .rows
        .iter()
        .any(|row| row.3.as_deref() == Some("question")));
    assert_eq!(
        result.unfinished,
        [("orphan".into(), "latest orphan output".into())]
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn retained_read_rejects_content_changes_between_byte_blocks() {
    let inst = crate::session::Instance::new("concurrent retained blocks", "/tmp");
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    state
        .acp_event_store
        .record(
            &id,
            1,
            &Event::AgentMessageChunk {
                text: "retained-1\n".repeat(16_384),
                message_id: None,
            },
        )
        .unwrap();
    let appended = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            get({
                let appended = Arc::clone(&appended);
                move |State(state): State<Arc<crate::server::AppState>>,
                      Path(id): Path<String>,
                      Query(query): Query<RetainedSourceQuery>| {
                    let appended = Arc::clone(&appended);
                    async move {
                        if query.offset != 0 && !appended.swap(true, Ordering::SeqCst) {
                            state
                                .acp_event_store
                                .record(
                                    &id,
                                    2,
                                    &Event::AgentMessageChunk {
                                        text: "retained-2".into(),
                                        message_id: None,
                                    },
                                )
                                .unwrap();
                        }
                        crate::server::api::acp_retained_source(
                            State(state),
                            Path(id),
                            Query(query),
                        )
                        .await
                    }
                }
            }),
        )
        .with_state(state);
    let (client, server) = serve(app).await;
    let error = snapshot(client.clone(), id.clone()).await.err().unwrap();
    assert!(appended.load(Ordering::SeqCst));
    assert!(
        matches!(error.downcast_ref::<crate::acp::client::HttpError>(), Some(crate::acp::client::HttpError::Server { status, .. }) if *status == axum::http::StatusCode::CONFLICT)
    );
    let next = snapshot(client, id).await.unwrap();
    assert_eq!(next.highest, 2);
    assert!(next.rows.iter().any(|row| row.1 == "retained-2"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn retained_read_preserves_later_content_after_pinned_sequence_gaps() {
    use crate::acp::event_store::EventStore;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(EventStore::open(&directory.path().join("events.db"), 3).unwrap());
    let inst = crate::session::Instance::new("pinned retained blocks", "/tmp");
    let id = inst.id.clone();
    store
        .record(
            &id,
            1,
            &Event::AvailableCommandsUpdated {
                commands: Vec::new(),
            },
        )
        .unwrap();
    for sequence in 2..=20 {
        store
            .record(
                &id,
                sequence,
                &Event::AgentMessageChunk {
                    text: format!("message-{sequence}"),
                    message_id: None,
                },
            )
            .unwrap();
    }
    let state = crate::server::test_support::build_test_app_state_impl_with_event_store(
        vec![inst],
        Vec::new(),
        Vec::new(),
        None,
        Some(store),
        |_| {},
        std::convert::identity,
    );
    let (client, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state),
    )
    .await;
    let result = snapshot(client, id).await.unwrap();
    assert!(result.incomplete);
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row.1.as_str())
            .collect::<Vec<_>>(),
        ["message-18", "message-19", "message-20"]
    );
    server.abort();
    let _ = server.await;
}
