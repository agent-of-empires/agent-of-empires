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
            prompt: None,
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
    let mut collapsed = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 2)).unwrap();
    collapsed
        .draw(|frame| {
            view.render(frame, frame.area(), &Theme::default(), 0, true);
        })
        .unwrap();
    state
        .acp_event_store
        .record(
            &inst.id,
            3,
            &Event::AgentMessageChunk {
                text: "REPLACEMENT-CONTENT".into(),
                message_id: None,
            },
        )
        .unwrap();
    let mut updates = view.worker.as_ref().unwrap().results.clone();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if updates.borrow().as_deref().is_some_and(|update| {
                update
                    .as_ref()
                    .is_ok_and(|snapshot| snapshot.highest_seq == 3 && snapshot.window.is_none())
            }) {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    view.tick();
    failing.store(true, Ordering::SeqCst);
    wait_for(&mut view, &mut collapsed, 0, |view, text| {
        view.worker.is_none() && text.contains("503")
    })
    .await;
    terminal
        .draw(|frame| {
            view.render(frame, frame.area(), &Theme::default(), 0, true);
        })
        .unwrap();
    let preserved = painted(&terminal);
    assert!(preserved.contains("READ-ONLY-CONTENT"), "{preserved}");
    assert!(!preserved.contains("REPLACEMENT-CONTENT"));
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
    assert!(view.message.contains("incomplete"));
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
    assert!(!view.message.contains("incomplete"));
    drop(view);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn persisted_attachment_forms_render_and_copy_the_same_prompt_and_count() {
    use crate::acp::event_store::EventStore;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.db");
    let store = Arc::new(EventStore::open(&path, 0).unwrap());
    let schema = crate::events::Schema::new("acp").unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let forms = [
        serde_json::json!([
            {"id":"image","mime_type":"image/png","name":null,"size":12,"kind":"image"},
            {"id":"audio","mime_type":"audio/wav","name":"voice.wav","size":34,"kind":"audio"}
        ]),
        serde_json::json!([
            ["image", "image", "image/png", null, 12],
            ["audio", "audio", "audio/wav", "voice.wav", 34]
        ]),
    ];
    let mut instances = Vec::new();
    for attachments in forms {
        let mut inst = crate::session::Instance::new("attachment prompt", "/tmp");
        inst.trash();
        let raw = serde_json::json!({"UserPromptSent":{"text":"ATTACHMENT-PROMPT","attachments":attachments}});
        crate::events::insert_event(&connection, &schema, &inst.id, 1, &raw.to_string(), 0)
            .unwrap();
        instances.push(inst);
    }
    let state = crate::server::test_support::build_test_app_state_impl_with_event_store(
        instances.clone(),
        Vec::new(),
        Vec::new(),
        None,
        Some(store),
        |_| {},
        std::convert::identity,
    );
    let (endpoint, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state),
    )
    .await;
    let mut copies = Vec::new();
    for inst in instances {
        let mut view = RetainedView::new(Some(endpoint.clone()), &inst);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        wait_for(&mut view, &mut terminal, 0, |view, text| {
            view.effective_offset().is_some()
                && text.contains("ATTACHMENT-PROMPT")
                && text.contains("2 attachment(s)")
        })
        .await;
        let snapshot = view.result.as_deref().unwrap().as_ref().unwrap();
        assert!(!snapshot.incomplete);
        let mut file = view
            .worker
            .as_ref()
            .unwrap()
            .copy(
                snapshot.identity.clone(),
                super::super::retained::worker::CopyRange::All,
            )
            .await
            .unwrap()
            .unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.contains("ATTACHMENT-PROMPT"));
        assert!(text.contains("2 attachment(s)"));
        copies.push(text);
    }
    assert_eq!(copies[0], copies[1]);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn loading_copy_is_refused_with_visible_retry_feedback() {
    let mut instance = crate::session::Instance::new("loading retained copy", "/tmp");
    instance.trash();
    let mut view = RetainedView::new(None, &instance);
    assert!(!view.copy_all());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    terminal
        .draw(|frame| {
            view.render(frame, frame.area(), &Theme::default(), 0, true);
        })
        .unwrap();
    let text = painted(&terminal);
    assert!(text.contains("Copy unavailable while retained content loads"));
    assert!(!text.contains("Copy queued"));
}

#[tokio::test]
async fn failed_reader_backs_off_until_a_usable_window_resets_retry_cadence() {
    use crate::acp::protocol::RetainedSourceQuery;
    use axum::extract::{Path, Query, State};
    let mut instance = crate::session::Instance::new("retained backoff", "/tmp");
    instance.trash();
    let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
    state
        .acp_event_store
        .record(
            &instance.id,
            1,
            &Event::AgentMessageChunk {
                text: "LAST-READABLE-WINDOW".into(),
                message_id: None,
            },
        )
        .unwrap();
    let failing = Arc::new(AtomicBool::new(true));
    let app = Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            get({
                let failing = Arc::clone(&failing);
                move |State(state): State<Arc<crate::server::AppState>>,
                      Path(id): Path<String>,
                      Query(query): Query<RetainedSourceQuery>| {
                    let failing = Arc::clone(&failing);
                    async move {
                        if failing.load(Ordering::SeqCst) {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            crate::server::api::acp_retained_source(
                                State(state),
                                Path(id),
                                Query(query),
                            )
                            .await
                        }
                    }
                }
            }),
        )
        .with_state(state);
    let (endpoint, server) = serve(app).await;
    let mut view = RetainedView::new(None, &instance);
    let mut prior_delay = Duration::ZERO;
    for failure in 1..=8 {
        view.connect(endpoint.clone()).unwrap();
        let mut updates = view.worker.as_ref().unwrap().results.clone();
        if updates.borrow().is_none() {
            tokio::time::timeout(Duration::from_secs(5), updates.changed())
                .await
                .unwrap()
                .unwrap();
        }
        view.tick();
        assert!(view.worker.is_none());
        assert_eq!(view.consecutive_failures, failure);
        let delay = view.next_read.saturating_duration_since(Instant::now());
        assert!(delay > prior_delay.min(Duration::from_secs(59)));
        assert!(delay <= Duration::from_secs(60));
        prior_delay = delay;
        assert!(
            !view.tick(),
            "backoff must not start a disconnected reader early"
        );
        assert!(view.worker.is_none());
    }
    failing.store(false, Ordering::SeqCst);
    view.connect(endpoint.clone()).unwrap();
    let mut updates = view.worker.as_ref().unwrap().results.clone();
    if updates.borrow().is_none() {
        tokio::time::timeout(Duration::from_secs(5), updates.changed())
            .await
            .unwrap()
            .unwrap();
    }
    view.tick();
    assert_eq!(
        view.consecutive_failures, 8,
        "metadata alone must not reset retry backoff"
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    wait_for(&mut view, &mut terminal, 0, |_, text| {
        text.contains("LAST-READABLE-WINDOW")
    })
    .await;
    assert_eq!(view.consecutive_failures, 0);
    failing.store(true, Ordering::SeqCst);
    view.worker = None;
    view.connect(endpoint).unwrap();
    let mut updates = view.worker.as_ref().unwrap().results.clone();
    if updates.borrow().is_none() {
        tokio::time::timeout(Duration::from_secs(5), updates.changed())
            .await
            .unwrap()
            .unwrap();
    }
    view.tick();
    assert_eq!(view.consecutive_failures, 1);
    terminal
        .draw(|frame| {
            view.render(frame, frame.area(), &Theme::default(), 0, true);
        })
        .unwrap();
    assert!(painted(&terminal).contains("LAST-READABLE-WINDOW"));
    assert!(view.message.contains("Transcript read failed"));
    server.abort();
    let _ = server.await;
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn dropping_retained_view_keeps_original_helper_owned_before_newer_live_copy() {
    use std::io::{Seek, Write};
    let temp = tempfile::tempdir().unwrap();
    let gate_path = temp.path().join("gate");
    nix::unistd::mkfifo(
        &gate_path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate_path)
        .unwrap();
    let output_path = temp.path().join("clipboard");
    let sink = output_path.clone();
    let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let mut first = true;
    let executor = crate::tui::clipboard::Executor::start(move |mut file, timeout, _| {
        file.rewind()?;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(if first {
                "cat >> \"$1\"; IFS= read -r release < \"$2\""
            } else {
                "cat >> \"$1\""
            })
            .arg("clipboard-helper")
            .arg(&sink)
            .arg(&gate_path)
            .stdin(std::process::Stdio::from(file.try_clone()?));
        let mut child = command.spawn()?;
        started.send(child.id()).unwrap();
        first = false;
        let status = crate::process::wait_with_timeout(&mut child, timeout)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "controlled helper deadline")
        })?;
        if !status.success() {
            return Err(std::io::Error::other("controlled clipboard helper failed"));
        }
        Ok(crate::tui::clipboard::CopyReport {
            bytes: file.metadata()?.len(),
            desktop: Some("test helper"),
            osc52: crate::tui::clipboard::Osc52Outcome::Failed(
                std::io::ErrorKind::Unsupported.into(),
            ),
        })
    })
    .unwrap();
    executor.activate_for_test();
    let mut instance = crate::session::Instance::new("owned retained clipboard", "/tmp");
    instance.trash();
    let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
    state
        .acp_event_store
        .record(
            &instance.id,
            1,
            &Event::AgentMessageChunk {
                text: "ORIGINAL-RETAINED-COPY".into(),
                message_id: None,
            },
        )
        .unwrap();
    let (endpoint, server) = serve(
        Router::new()
            .route(
                "/api/sessions/{id}/acp/retained-source",
                get(crate::server::api::acp_retained_source),
            )
            .with_state(state),
    )
    .await;
    let mut view = RetainedView::new(Some(endpoint), &instance);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("ORIGINAL-RETAINED-COPY")
    })
    .await;
    assert!(view.copy_all());
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            view.tick();
            if let Ok(pid) = observed.try_recv() {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(view);
    let original = nix::unistd::Pid::from_raw(pid as i32);
    assert!(
        nix::sys::signal::kill(original, None).is_ok(),
        "view drop must not detach or terminate active helper"
    );
    assert!(crate::tui::clipboard::copy_to_clipboard("NEWER-LIVE-COPY").starts_with("Copy queued"));
    writeln!(gate, "release").unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(message) = executor.feedback() {
                break message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(message.starts_with("Copied"), "{message}");
    drop(executor);
    assert!(
        nix::sys::signal::kill(original, None).is_err(),
        "executor reaps original helper"
    );
    let copied = std::fs::read_to_string(output_path).unwrap();
    assert_eq!(copied.matches("ORIGINAL-RETAINED-COPY").count(), 1);
    assert!(copied.ends_with("NEWER-LIVE-COPY"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
#[serial_test::serial]
async fn source_replacement_cancels_already_prepared_copy_before_clipboard_admission() {
    use std::io::{Seek, Write};
    let _app = crate::session::test_support::isolate_app_dir();
    let written = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&written);
    let executor = crate::tui::clipboard::Executor::start(move |mut file, _, _| {
        file.rewind()?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        sink.lock().unwrap().push(text);
        Ok(crate::tui::clipboard::CopyReport {
            bytes: file.metadata()?.len(),
            desktop: Some("controlled helper"),
            osc52: crate::tui::clipboard::Osc52Outcome::Failed(
                std::io::ErrorKind::Unsupported.into(),
            ),
        })
    })
    .unwrap();
    executor.activate_for_test();
    let mut instance = crate::session::Instance::new("copy source freshness", "/tmp");
    instance.trash();
    let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
    state
        .acp_event_store
        .record(
            &instance.id,
            1,
            &Event::AgentMessageChunk {
                text: "GENERATION-A-CONTENT".into(),
                message_id: None,
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
    let mut view = RetainedView::new(Some(endpoint), &instance);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("GENERATION-A-CONTENT")
    })
    .await;
    assert!(view.copy_all());
    view.wait_for_copy_preparation_for_test().await;
    state
        .acp_event_store
        .record(
            &instance.id,
            2,
            &Event::AgentMessageChunk {
                text: "GENERATION-B-CONTENT".into(),
                message_id: Some("next".into()),
            },
        )
        .unwrap();
    let mut updates = view.worker.as_ref().unwrap().results.clone();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if updates.borrow().as_deref().is_some_and(|update| {
                update
                    .as_ref()
                    .is_ok_and(|snapshot| snapshot.highest_seq == 2)
            }) {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    // A real A export is ready at the same time that the worker has published B.
    view.tick();
    if let Some(submitted) = view.clipboard_result.take() {
        tokio::time::timeout(Duration::from_secs(5), submitted)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    let mut barrier = tempfile::tempfile().unwrap();
    barrier.write_all(b"CLIPBOARD-BARRIER").unwrap();
    crate::tui::clipboard::reserve()
        .unwrap()
        .submit(barrier, Duration::from_secs(5))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*written.lock().unwrap(), ["CLIPBOARD-BARRIER"]);
    assert!(view
        .copy_message
        .as_deref()
        .is_some_and(|message| message.contains("retained history changed")));
    wait_for(&mut view, &mut terminal, 0, |view, text| {
        view.effective_offset().is_some() && text.contains("GENERATION-B-CONTENT")
    })
    .await;
    assert!(view.copy_all());
    view.wait_for_copy_preparation_for_test().await;
    view.tick();
    let submitted = view
        .clipboard_result
        .take()
        .expect("current source is admitted");
    tokio::time::timeout(Duration::from_secs(5), submitted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(written
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("GENERATION-B-CONTENT"));
    drop(view);
    server.abort();
    let _ = server.await;
}
