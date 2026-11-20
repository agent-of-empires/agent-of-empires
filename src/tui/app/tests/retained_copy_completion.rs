use super::*;
use crate::tui::terminal_output::{
    install_test_output,
    tests::{block_terminal, drain_terminal, private_terminal, resume_terminal, TerminalDrainer},
};
use std::io::Seek;

#[tokio::test]
#[serial_test::serial]
async fn retained_admitted_copy_reports_completion_after_presentation_invalidation() {
    const CONTENT: &str = "CLIPBOARD-COMPLETION";
    let root = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(root.path());
    crate::session::get_profile_dir("test").unwrap();
    let mut row =
        crate::session::Instance::new("retained completion", root.path().to_str().unwrap());
    row.source_profile = "test".into();
    row.trash();
    crate::session::Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, groups| {
            *rows = vec![row.clone()];
            *groups = crate::session::GroupTree::new_with_groups(rows, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    state
        .acp_event_store
        .record(
            &row.id,
            1,
            &crate::acp::state::Event::AgentMessageChunk {
                text: CONTENT.into(),
                message_id: None,
            },
        )
        .unwrap();
    let router = axum::Router::new()
        .route(
            "/api/sessions/{id}/acp/retained-source",
            axum::routing::get(crate::server::api::acp_retained_source),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = crate::daemon::DaemonEndpoint::new(
        format!("http://{}", listener.local_addr().unwrap()),
        Some("retained-copy-fixture-token".into()),
        crate::daemon::Source::Env,
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut app = App::new(
        "test",
        AvailableTools::with_tools(&["claude"]),
        true,
        false,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
    let snapshot = state.runtime.publish(&state).await.unwrap().value.clone();
    app.home
        .session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
            std::sync::Arc::new(snapshot),
        ));
    app.home.apply_session_feed();
    app.home.selected_session = Some(row.id.clone());
    app.home.retained_focus = Some((row.id.clone(), row.lifecycle_generation));
    app.home.retained_preview = Some(crate::tui::structured_view::embedded::RetainedView::new(
        Some(endpoint),
        &row,
    ));
    let sink = root.path().join("actual-clipboard-output");
    let output_path = sink.clone();
    let (started, mut observed) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let (feedback_tx, feedback) = tokio::sync::oneshot::channel();
    let mut feedback_tx = Some(feedback_tx);
    app._clipboard = crate::tui::clipboard::Executor::start(move |mut file, _, _| {
        file.rewind()?;
        let mut output = std::fs::File::create(&output_path)?;
        let bytes = std::io::copy(&mut file, &mut output)?;
        started.take().expect("one selected copy").send(()).unwrap();
        released
            .recv_timeout(Duration::from_secs(5))
            .map_err(std::io::Error::other)?;
        let report = crate::tui::clipboard::CopyReport {
            bytes,
            desktop: Some("private output file"),
            osc52: crate::tui::clipboard::Osc52Outcome::Failed(
                std::io::ErrorKind::Unsupported.into(),
            ),
        };
        feedback_tx
            .take()
            .expect("one actual copy report")
            .send(report.message())
            .unwrap();
        Ok(report)
    })
    .unwrap();
    app._clipboard.activate_for_test();
    struct Release(Option<std::sync::mpsc::SyncSender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }
    // Declared after App so a failed assertion releases its joined executor first.
    let mut release = Release(Some(release));
    let (mut master, slave) = private_terminal();
    let mut control = slave.try_clone().unwrap();
    let (_output_guard, _) = install_test_output(slave);
    let mut terminal = output_test_terminal();
    let mut screen = vt100::Parser::new(
        std::num::NonZeroU16::new(40).unwrap(),
        std::num::NonZeroU16::new(200).unwrap(),
        0,
    );
    let mut bytes = Vec::new();
    let mut parsed = 0;
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    let mut phase = "present-populated-window";
    let initial_scene = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            ticker.tick().await;
            app.reconcile_structured_preview().await;
            drain_terminal(&mut master, &mut bytes);
            app.draw(&mut terminal).unwrap();
            if app.home.presentation_ready
                && app.pending_presentation.is_none()
                && app
                    .home
                    .retained_preview
                    .as_ref()
                    .is_some_and(|view| view.effective_offset() == Some(0))
                && app.home.preview_text_view.total_lines > 0
                && !app.home.preview_text_view.pane.is_empty()
            {
                break;
            }
            // Let the current-thread HTTP producer progress between synchronous frame pumps.
            ticker.reset();
        }
        // Observe the acknowledged frame's bytes without capturing its replacement.
        phase = "observe-emitted-content";
        loop {
            drain_terminal(&mut master, &mut bytes);
            screen.process(&bytes[parsed..]);
            parsed = bytes.len();
            if screen.screen().contents().contains(CONTENT) {
                break;
            }
            ticker.tick().await;
        }
    })
    .await;
    if initial_scene.is_err() {
        drain_terminal(&mut master, &mut bytes);
        screen.process(&bytes[parsed..]);
        parsed = bytes.len();
        let content = screen.screen().contents();
        panic!(
            "initial retained scene timed out: phase={phase}; ready={}; pending={}; \
             window_offset={:?}; pane={:?}; total_lines={}; bytes={}; parsed={}; \
             content_present={}; content={:.2400}",
            app.home.presentation_ready,
            app.pending_presentation.is_some(),
            app.home
                .retained_preview
                .as_ref()
                .and_then(|view| view.effective_offset()),
            app.home.preview_text_view.pane,
            app.home.preview_text_view.total_lines,
            bytes.len(),
            parsed,
            content.contains(CONTENT),
            content
        );
    }
    let pane = app.home.preview_text_view.pane;
    let (x, y) = (pane.y..pane.bottom())
        .find_map(|y| {
            let line = (pane.x..pane.right())
                .map(|x| {
                    screen
                        .screen()
                        .cell(y, x)
                        .map_or("", |cell| cell.contents())
                })
                .collect::<String>();
            line.find(CONTENT).map(|offset| {
                (
                    pane.x + unicode_width::UnicodeWidthStr::width(&line[..offset]) as u16,
                    y,
                )
            })
        })
        .expect("copy target must be in the actually emitted preview");
    assert!(app.home.handle_drag_start(x, y));
    assert!(app.home.handle_drag_move(x + CONTENT.len() as u16 - 1, y));
    assert!(app.home.handle_drag_end());
    let drainer = TerminalDrainer::start(&master, std::mem::take(&mut bytes));
    // The production release path redraws before polling; that render admits the selection copy.
    app.draw(&mut terminal).unwrap();
    assert!(
        !app.home.preview_copy_pending,
        "the finalized selection must be consumed by its repaint"
    );
    assert!(
        app.home.presentation_ready,
        "the release repaint must be presented before waiting for copy admission"
    );
    assert!(
        app.pending_presentation.is_none(),
        "the release repaint must not leave an unacknowledged frame"
    );
    assert!(
        app.home.preview_selection.is_some(),
        "the real selection must survive its release repaint"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            ticker.reset();
            ticker.tick().await;
            app.reconcile_structured_preview().await;
            match observed.try_recv() {
                Ok(()) => break,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(error) => panic!("actual clipboard executor ended before writing: {error}"),
            }
        }
    })
    .await
    .expect("clipboard admission precondition: the real executor must write the sink before invalidation");
    bytes = drainer.finish();
    drain_terminal(&mut master, &mut bytes);
    screen.process(&bytes[parsed..]);
    parsed = bytes.len();
    assert_eq!(std::fs::read(&sink).unwrap(), CONTENT.as_bytes());
    block_terminal(&mut control);
    crate::tui::terminal_output::request_redraw();
    app.draw(&mut terminal).unwrap();
    assert!(!app.home.presentation_ready);
    assert!(app.home.preview_selection.is_none());
    release.0.take().unwrap().send(()).unwrap();
    resume_terminal(&control);
    let completed_caption = format!("Copied {} bytes", CONTENT.len());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            ticker.reset();
            ticker.tick().await;
            app.reconcile_structured_preview().await;
            drain_terminal(&mut master, &mut bytes);
            app.draw(&mut terminal).unwrap();
            drain_terminal(&mut master, &mut bytes);
            screen.process(&bytes[parsed..]);
            parsed = bytes.len();
            if screen.screen().contents().contains(&completed_caption) {
                break;
            }
        }
    })
    .await
    .expect(
        "an admitted copy must report actual completion, not leave a permanent Writing caption",
    );
    assert!(!screen
        .screen()
        .contents()
        .contains("Writing clipboard content"));
    assert_eq!(std::fs::read(&sink).unwrap(), CONTENT.as_bytes());
    let completed_feedback = feedback
        .await
        .expect("the actual executor produced its report");
    assert!(
        screen.screen().contents().contains(&completed_feedback),
        "the actual completed report must first be visible"
    );
    const UPDATED: &str = "RETAINED-SOURCE-AFTER-COPY";
    state
        .acp_event_store
        .record(
            &row.id,
            2,
            &crate::acp::state::Event::AgentMessageChunk {
                text: UPDATED.into(),
                message_id: Some("post-copy-source-update".into()),
            },
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            ticker.reset();
            ticker.tick().await;
            app.reconcile_structured_preview().await;
            drain_terminal(&mut master, &mut bytes);
            app.draw(&mut terminal).unwrap();
            drain_terminal(&mut master, &mut bytes);
            screen.process(&bytes[parsed..]);
            parsed = bytes.len();
            if app.home.presentation_ready && screen.screen().contents().contains(UPDATED) {
                break;
            }
        }
    })
    .await
    .expect("the real changed retained source must be repainted before comparing copy feedback");
    assert!(
        screen.screen().contents().contains(&completed_feedback),
        "a later source update must not relabel the actual confirmed copy result"
    );
    assert_eq!(std::fs::read(&sink).unwrap(), CONTENT.as_bytes());
    drop(app);
    server.abort();
    let _ = server.await;
}
