use super::*;
use crate::acp::state::{DiffPreview, Event, ToolCall};
use axum::{middleware, response::IntoResponse, routing::get, Router};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

fn painted(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
async fn wait_for(
    view: &mut RetainedView,
    terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    offset: usize,
    predicate: impl Fn(&RetainedView, &str) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            view.tick();
            terminal
                .draw(|frame| {
                    view.render(frame, frame.area(), &Theme::default(), offset, true);
                })
                .unwrap();
            if predicate(view, &painted(terminal)) {
                break;
            }
            if let Some(worker) = view.worker.as_ref() {
                let mut signal = worker.results.clone();
                let _ = signal.changed().await;
            } else {
                tokio::time::sleep_until(view.next_read).await;
            }
        }
    })
    .await
    .expect("retained reader did not reach the requested visible state");
}
fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(100, 0).unwrap()
}
fn tool(id: &str) -> Event {
    Event::ToolCallStarted {
        tool_call: ToolCall {
            id: id.into(),
            name: "RetainedTool".into(),
            kind: "edit".into(),
            args_preview: "{}".into(),
            started_at: at(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: vec![DiffPreview {
                path: "/repo/src/test.rs".into(),
                old_text: Some("before".into()),
                new_text: Some("after".into()),
                created_at: at(),
            }],
        },
    }
}
async fn serve(router: Router) -> (DaemonEndpoint, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = DaemonEndpoint::new(
        format!("http://{}", listener.local_addr().unwrap()),
        None,
        crate::daemon::Source::Env,
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (endpoint, server)
}

#[tokio::test]
#[serial_test::serial]
async fn retained_reader_recovers_discovery_and_uses_pre_trash_roots() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _discovery = crate::session::test_support::EnvGuard::unset(&[
        "AOE_DAEMON_URL",
        "AOE_DAEMON_TOKEN",
        "AOE_DAEMON_PASSPHRASE",
    ]);
    let mut inst = crate::session::Instance::new("retained paths", "/trash/missing");
    inst.trash();
    inst.pre_trash_project_path = Some("/repo".into());
    let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
    state
        .acp_event_store
        .record(&inst.id, 1, &tool("edit"))
        .unwrap();
    state
        .acp_event_store
        .record(
            &inst.id,
            2,
            &Event::ToolCallCompleted {
                tool_call_id: "edit".into(),
                is_error: false,
                content: "READ-ONLY-CONTENT".into(),
                output: Vec::new(),
                completed_at: at(),
                async_subagent: false,
            },
        )
        .unwrap();
    let failing = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            get(crate::server::api::acp_retained_source),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            Arc::clone(&failing),
            |axum::extract::State(failing): axum::extract::State<Arc<AtomicBool>>,
             request: axum::extract::Request,
             next: middleware::Next| async move {
                assert_eq!(request.method(), axum::http::Method::GET);
                if failing.load(Ordering::SeqCst) {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    next.run(request).await
                }
            },
        ));
    let (endpoint, server) = serve(app).await;
    let _endpoint =
        crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_URL", &endpoint.base_url)]);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    for initial in [
        None,
        Some(DaemonEndpoint::new(
            "not a URL".into(),
            None,
            crate::daemon::Source::Env,
        )),
        Some(endpoint.clone()),
    ] {
        let mut view = RetainedView::new(initial, &inst);
        wait_for(&mut view, &mut terminal, usize::MAX, |view, text| {
            view.effective_offset().is_some() && text.contains("READ-ONLY-CONTENT")
        })
        .await;
        let text = painted(&terminal);
        assert!(text.contains("src/test.rs"));
        assert!(!text.contains("/repo/"));
        assert!(!text.contains("/trash/missing"));
    }
    let mut view = RetainedView::new(Some(endpoint), &inst);
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("READ-ONLY-CONTENT")
    })
    .await;
    failing.store(true, Ordering::SeqCst);
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.worker.is_none() && text.contains("503")
    })
    .await;
    assert!(painted(&terminal).contains("READ-ONLY-CONTENT"));
    failing.store(false, Ordering::SeqCst);
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.worker.is_some() && view.effective_offset().is_some() && !text.contains("503")
    })
    .await;
    drop(view);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mounted_reader_acquires_backfilled_stop_and_same_bounds_replacement() {
    let mut inst = crate::session::Instance::new("retained transitions", "/repo");
    inst.trash();
    let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
    state
        .acp_event_store
        .record(&inst.id, 1, &tool("pending"))
        .unwrap();
    state
        .acp_event_store
        .record(
            &inst.id,
            2,
            &Event::ToolCallContent {
                tool_call_id: "pending".into(),
                content: "BEFORE-TEARDOWN".into(),
            },
        )
        .unwrap();
    state
        .acp_event_store
        .record(
            &inst.id,
            4,
            &Event::ConversationSummary {
                text: "CONCURRENT-SUMMARY".into(),
                summarized_until_seq: 2,
            },
        )
        .unwrap();
    let (endpoint, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state.clone()),
    )
    .await;
    let mut view = RetainedView::new(Some(endpoint), &inst);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some()
            && text.contains("BEFORE-TEARDOWN")
            && text.contains("CONCURRENT-SUMMARY")
    })
    .await;
    assert!(!painted(&terminal).contains('◼'));
    state
        .acp_event_store
        .record(
            &inst.id,
            3,
            &Event::Stopped {
                reason: "teardown complete".into(),
            },
        )
        .unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains('◼')
    })
    .await;
    let text = painted(&terminal);
    assert!(text.contains("BEFORE-TEARDOWN"));
    assert!(text.contains("CONCURRENT-SUMMARY"));
    state.acp_event_store.delete_session(&inst.id);
    for sequence in 1..=4 {
        state
            .acp_event_store
            .record(
                &inst.id,
                sequence,
                &Event::AgentMessageChunk {
                    text: "REPLACEMENT-CONTENT\n\n".into(),
                    message_id: None,
                },
            )
            .unwrap();
    }
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("REPLACEMENT-CONTENT")
    })
    .await;
    assert!(!painted(&terminal).contains("BEFORE-TEARDOWN"));
    let identity = view
        .result
        .as_deref()
        .unwrap()
        .as_ref()
        .unwrap()
        .identity
        .clone();
    let result = view
        .worker
        .as_ref()
        .unwrap()
        .copy(identity, super::super::retained::worker::CopyRange::All);
    let mut file = tokio::time::timeout(Duration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut copied = String::new();
    file.read_to_string(&mut copied).unwrap();
    assert_eq!(copied.matches("REPLACEMENT-CONTENT").count(), 4);
    assert!(!copied.contains("BEFORE-TEARDOWN"));
    drop(view);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn obsolete_initial_read_is_cancelled_without_publishing_a_snapshot() {
    let started = Arc::new(tokio::sync::Notify::new());
    let app = Router::new().route(
        "/api/sessions/{id}/acp/retained-source",
        get({
            let started = Arc::clone(&started);
            move || {
                let started = Arc::clone(&started);
                async move {
                    started.notify_one();
                    std::future::pending::<axum::response::Response>().await
                }
            }
        }),
    );
    let (endpoint, server) = serve(app).await;
    let mut inst = crate::session::Instance::new("obsolete read", "/tmp");
    inst.trash();
    let view = RetainedView::new(Some(endpoint), &inst);
    let mut results = view.worker.as_ref().unwrap().results.clone();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    drop(view);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), results.changed())
            .await
            .unwrap()
            .is_err()
    );
    assert!(results.borrow().is_none());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn selection_copy_rebuilds_an_evicted_width_without_losing_offscreen_content() {
    let text = format!(
        "```text\nSTART-MARKER\n{}END-MARKER\n```",
        "é e\u{301} 界 👩‍👩‍👧‍👧 code payload with several words and a long suffix\n".repeat(1024)
    );
    let event = Event::AgentMessageChunk {
        text,
        message_id: None,
    };
    let mut inst = crate::session::Instance::new("evicted width copy", "/repo");
    inst.trash();
    let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
    state.acp_event_store.record(&inst.id, 1, &event).unwrap();
    let (endpoint, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state),
    )
    .await;
    let mut model = crate::acp::transcript::TranscriptModel::new();
    model.apply_event(1, &event);
    let http = crate::acp::client::HttpClient::new(endpoint.clone()).unwrap();
    let mut memory = StructuredViewState::new(inst.id.clone(), endpoint.clone(), http, None);
    memory.transcript.server_rows = model.into_rows();
    let reference = render::wrapped_transcript(&memory, &Theme::default(), 32);
    let expected = reference
        .lines
        .iter()
        .map(|line| {
            crate::tui::components::text::line_columns(line, 32)
                .slice(0, 32)
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut view = RetainedView::new(Some(endpoint), &inst);
    let mut original_total = 0;
    for width in [32, 47, 80] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 20)).unwrap();
        wait_for(&mut view, &mut terminal, 0, |view, text| {
            view.effective_offset().is_some() && text.contains("END-MARKER")
        })
        .await;
        if width == 32 {
            terminal
                .draw(|frame| {
                    original_total = view
                        .render(frame, frame.area(), &Theme::default(), 0, true)
                        .0
                        .total_lines;
                })
                .unwrap();
        }
    }
    assert_eq!(original_total, reference.lines.len());
    let identity = view
        .result
        .as_deref()
        .unwrap()
        .as_ref()
        .unwrap()
        .identity
        .clone();
    let selection = crate::tui::rich_text::window::Selection {
        start: (0, 0),
        end: (31, original_total as u64 - 1),
    };
    let reply = view.worker.as_ref().unwrap().copy(
        identity,
        super::super::retained::worker::CopyRange::Selection {
            width: 32,
            selection,
        },
    );
    let mut file = tokio::time::timeout(Duration::from_secs(12), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut copied = String::new();
    file.read_to_string(&mut copied).unwrap();
    assert_eq!(copied, expected);
    drop(view);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn stable_gaps_and_unrelated_writes_do_not_reload_retained_content() {
    use std::sync::atomic::AtomicUsize;
    let mut inst = crate::session::Instance::new("retained gap probes", "/repo");
    inst.trash();
    let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
    for (sequence, text) in [(1, "FIRST"), (3, "LAST")] {
        state
            .acp_event_store
            .record(
                &inst.id,
                sequence,
                &Event::AgentMessageChunk {
                    text: text.into(),
                    message_id: Some(text.into()),
                },
            )
            .unwrap();
    }
    let full_reads = Arc::new(AtomicUsize::new(0));
    let (probes, mut observed_probes) = tokio::sync::mpsc::channel(4);
    let app = Router::new().route("/api/sessions/{id}/acp/retained-source", get(crate::server::api::acp_retained_source)).with_state(state.clone()).layer(middleware::from_fn_with_state(
        (Arc::clone(&full_reads), probes),
        |axum::extract::State((reads, probes)): axum::extract::State<(Arc<AtomicUsize>, tokio::sync::mpsc::Sender<()>)>, request: axum::extract::Request, next: middleware::Next| async move {
            let query = axum::extract::Query::<crate::acp::protocol::RetainedSourceQuery>::try_from_uri(request.uri()).unwrap().0;
            if query.after == 0 && query.seq.is_none() { reads.fetch_add(1, Ordering::SeqCst); }
            let response = next.run(request).await;
            if query.after == u64::MAX { let _ = probes.try_send(()); }
            response
        },
    ));
    let (endpoint, server) = serve(app).await;
    let mut view = RetainedView::new(Some(endpoint), &inst);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("FIRST") && text.contains("LAST")
    })
    .await;
    let initial_reads = full_reads.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(12), async {
        observed_probes.recv().await.unwrap();
        state
            .acp_event_store
            .record(
                "unrelated",
                1,
                &Event::Stopped {
                    reason: "done".into(),
                },
            )
            .unwrap();
        observed_probes.recv().await.unwrap();
        observed_probes.recv().await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(full_reads.load(Ordering::SeqCst), initial_reads);
    state
        .acp_event_store
        .record(
            &inst.id,
            2,
            &Event::AgentMessageChunk {
                text: "BACKFILLED".into(),
                message_id: Some("backfill".into()),
            },
        )
        .unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("BACKFILLED")
    })
    .await;
    assert_eq!(full_reads.load(Ordering::SeqCst), initial_reads + 1);
    drop(view);
    server.abort();
    let _ = server.await;
}
