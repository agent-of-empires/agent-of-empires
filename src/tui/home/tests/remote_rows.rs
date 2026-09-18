//! Remote daemon sessions listed inline, and "group by remote".

use super::*;
use crate::session::config::GroupByMode;
use crate::tui::remote_feed::RemoteSnapshot;

fn wire(id: &str, title: &str) -> crate::daemon::SessionResponse {
    serde_json::from_value(serde_json::json!({"id": id, "title": title})).unwrap()
}

fn with_remote(env: &mut TestEnv, rows: Vec<crate::daemon::SessionResponse>) {
    env.view.remote_snapshots = vec![RemoteSnapshot {
        name: "mini".into(),
        sessions: Some(Ok(rows)),
        meta: None,
    }];
    env.view.remotes_configured = true;
    env.view.rebuild_flat_items();
}

#[test]
#[serial]
fn a_defaulted_remote_grouping_renders_the_pre_remote_default_until_a_remote_exists() {
    let mut env = create_test_env_with_sessions(2);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = true;
    env.view.fallback_group_by = GroupByMode::Manual;
    env.view.remotes_configured = false;
    assert_eq!(env.view.effective_group_by(), GroupByMode::Manual);

    env.view.remotes_configured = true;
    assert_eq!(env.view.effective_group_by(), GroupByMode::Remote);

    // An explicit choice is honored even with nothing configured.
    env.view.remotes_configured = false;
    env.view.group_by_is_default = false;
    assert_eq!(env.view.effective_group_by(), GroupByMode::Remote);
}

#[test]
#[serial]
fn grouping_by_remote_lists_local_first_then_each_remote_one_indent_deep() {
    let mut env = create_test_env_with_sessions(2);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    with_remote(&mut env, vec![wire("r1", "remote one")]);

    let shape: Vec<(&str, usize)> = env
        .view
        .flat_items
        .iter()
        .map(|item| {
            let kind = match item {
                Item::LocalGroup { .. } => "local",
                Item::Session { .. } => "session",
                Item::RemoteGroup { .. } => "remote",
                Item::RemoteSession { .. } => "remote-session",
                Item::Group { .. } => "group",
            };
            (kind, item.depth())
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("local", 0),
            ("session", 1),
            ("session", 1),
            ("remote", 0),
            ("remote-session", 1),
        ]
    );
}

#[test]
#[serial]
fn collapsing_a_machine_header_hides_only_that_machine() {
    let mut env = create_test_env_with_sessions(2);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    with_remote(&mut env, vec![wire("r1", "remote one")]);

    env.view.cursor = 0;
    env.view.update_selected();
    assert!(env.view.toggle_machine_header_at_cursor());
    assert!(!env
        .view
        .flat_items
        .iter()
        .any(|item| matches!(item, Item::Session { .. })));
    assert!(env
        .view
        .flat_items
        .iter()
        .any(|item| matches!(item, Item::RemoteSession { .. })));
}

fn register_mini() {
    let mut registry = crate::daemon::remotes::Registry::default();
    registry.upsert(crate::daemon::remotes::Remote {
        name: "mini".into(),
        // Unroutable: the worker's connect fails in the background, which these
        // tests never drain; they assert the view's own state.
        url: "https://127.0.0.1:9".into(),
        enabled: true,
        token: Some("tok".into()),
        session: None,
        binding: None,
        insecure: false,
    });
    crate::daemon::remotes::save(&registry).unwrap();
}

#[test]
#[serial]
fn a_remote_row_never_selects_a_local_session_and_enter_live_sends_in_the_pane() {
    let mut env = create_test_env_with_sessions(1);
    register_mini();
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    with_remote(&mut env, vec![wire("r1", "remote one")]);

    let idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::RemoteSession { .. }))
        .expect("remote row listed");
    env.view.cursor = idx;
    env.view.update_selected();
    assert_eq!(env.view.selected_session, None);
    assert_eq!(env.view.selected_group, None);
    assert_eq!(
        env.view.remote_preview_key,
        Some(("mini".to_string(), "r1".to_string())),
        "selecting a remote row watches it for the preview"
    );

    assert!(env.view.activate_selected_session().is_none());
    let live = env.view.live_send.as_ref().expect("live-send in the pane");
    assert_eq!(
        live.remote_key(),
        Some(("mini".to_string(), "r1".to_string()))
    );
}

#[test]
#[serial]
fn the_exit_chord_leaves_remote_live_send_and_moving_off_the_row_stops_watching() {
    let mut env = create_test_env_with_sessions(1);
    register_mini();
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    with_remote(&mut env, vec![wire("r1", "remote one")]);
    env.view.cursor = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::RemoteSession { .. }))
        .unwrap();
    env.view.update_selected();
    env.view.activate_selected_session();
    let chord = env.view.live_send.as_ref().unwrap().exit_chords[0];

    env.view.handle_key(KeyEvent::new(chord.0, chord.1), None);
    assert!(env.view.live_send.is_none());
    assert!(
        env.view.remote_preview_key.is_some(),
        "still previewing after live-send ends"
    );

    env.view.cursor = 0;
    env.view.update_selected();
    assert_eq!(env.view.remote_preview_key, None);
}

#[test]
#[serial]
fn remote_sections_sit_above_the_archived_shelf_in_other_groupings() {
    let mut env = create_test_env_with_sessions(2);
    env.view.group_by = GroupByMode::Manual;
    env.view.group_by_is_default = false;
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    with_remote(&mut env, vec![wire("r1", "remote one")]);

    let shelf = env.view.shelf_start().expect("archived shelf present");
    let remote = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::RemoteGroup { .. }))
        .expect("remote header listed");
    assert!(remote < shelf, "remote section must stay above the shelf");
}

fn shelved(id: &str, archived: bool, trashed: bool) -> crate::daemon::SessionResponse {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "title": id,
        "archived_at": archived.then_some("2026-01-01T00:00:00Z"),
        "trashed_at": trashed.then_some("2026-02-01T00:00:00Z"),
    }))
    .unwrap()
}

fn position_of_remote_row(env: &TestEnv, want: &str) -> Option<usize> {
    env.view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::RemoteSession { id, .. } if id == want))
}

fn section_count(env: &TestEnv, section: &str) -> Option<usize> {
    env.view.flat_items.iter().find_map(|item| match item {
        Item::Group {
            path,
            session_count,
            ..
        } if path == section => Some(*session_count),
        _ => None,
    })
}

#[test]
#[serial]
fn remote_archived_and_trashed_rows_go_to_the_shelf_not_the_machine_section() {
    let mut env = create_test_env_with_sessions(1);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    env.view.archived_section_collapsed = false;
    env.view.trashed_section_collapsed = false;
    with_remote(
        &mut env,
        vec![
            wire("live", "live"),
            shelved("old", true, false),
            shelved("gone", false, true),
        ],
    );

    let shelf = env
        .view
        .shelf_start()
        .expect("shelf created for remote rows");
    assert!(position_of_remote_row(&env, "live").unwrap() < shelf);
    assert!(position_of_remote_row(&env, "old").unwrap() > shelf);
    assert!(position_of_remote_row(&env, "gone").unwrap() > shelf);
    assert!(matches!(
        env.view
            .flat_items
            .iter()
            .find(|i| matches!(i, Item::RemoteGroup { depth: 0, .. })),
        Some(Item::RemoteGroup {
            session_count: 1,
            ..
        })
    ));
    assert_eq!(
        section_count(&env, crate::session::ARCHIVED_SECTION_PATH),
        Some(1)
    );
    assert_eq!(
        section_count(&env, crate::session::TRASH_SECTION_PATH),
        Some(1)
    );
}

#[test]
#[serial]
fn a_collapsed_shelf_section_counts_remote_rows_without_listing_them() {
    let mut env = create_test_env_with_sessions(1);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    env.view.trashed_section_collapsed = true;
    with_remote(&mut env, vec![shelved("gone", false, true)]);

    assert_eq!(
        section_count(&env, crate::session::TRASH_SECTION_PATH),
        Some(1)
    );
    assert_eq!(position_of_remote_row(&env, "gone"), None);
}

#[test]
#[serial]
fn enter_on_a_trashed_remote_row_says_where_to_restore_it_instead_of_opening() {
    let mut env = create_test_env_with_sessions(1);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    env.view.trashed_section_collapsed = false;
    with_remote(&mut env, vec![shelved("gone", false, true)]);

    env.view.cursor = position_of_remote_row(&env, "gone").expect("listed in the shelf");
    env.view.update_selected();
    match env.view.activate_selected_session() {
        Some(Action::SetTransientStatus(message)) => assert!(message.contains("mini"), "{message}"),
        other => panic!("expected a restore hint, got {other:?}"),
    }
}

#[test]
#[serial]
fn the_dialog_lists_every_enabled_remote_with_why_it_cannot_take_a_session() {
    use crate::tui::dialogs::RemoteUnavailable;
    use crate::tui::remote_feed::{RemoteMeta, RemoteProfile};
    let mut env = create_test_env_with_sessions(1);
    register_mini();
    let meta = RemoteMeta {
        profiles: vec![
            RemoteProfile {
                name: "work".into(),
                is_default: false,
            },
            RemoteProfile {
                name: "main".into(),
                is_default: true,
            },
        ],
        ..RemoteMeta::default()
    };
    let cases = [
        (None, None, Err(RemoteUnavailable::Connecting)),
        (
            Some(Err("refused".to_string())),
            Some(meta.clone()),
            Err(RemoteUnavailable::Unreachable),
        ),
        (
            Some(Ok(Vec::new())),
            None,
            Err(RemoteUnavailable::Unreachable),
        ),
        (Some(Ok(Vec::new())), Some(meta), Ok(vec!["main", "work"])),
    ];
    for (sessions, meta, expected) in cases {
        env.view.remote_snapshots = vec![
            RemoteSnapshot {
                name: "mini".into(),
                sessions,
                meta,
            },
            RemoteSnapshot {
                name: "unregistered".into(),
                sessions: None,
                meta: None,
            },
        ];
        let targets = env.view.remote_dialog_targets();
        let got: Vec<_> = targets
            .iter()
            .map(|t| {
                let machine = t.machine.as_ref().map(|m| m.profiles.clone());
                (t.name.as_str(), machine.map_err(|e| *e))
            })
            .collect();
        let expected = expected.map(|p| p.iter().map(|s| s.to_string()).collect());
        assert_eq!(got, [("mini", expected)]);
    }
}

#[test]
#[serial]
fn a_remote_submit_closes_the_dialog_and_hands_off_to_the_remote() {
    let mut env = create_test_env_with_sessions(1);
    register_mini();
    let target = crate::tui::dialogs::RemoteTarget {
        name: "mini".into(),
        machine: Ok(crate::tui::dialogs::RemoteMachine {
            home: Some("/Users/remote".into()),
            profiles: vec!["default".into()],
            tools: vec!["claude".into()],
            docker_available: false,
            client: crate::daemon::DaemonClient::new("https://127.0.0.1:9", Some("tok")).unwrap(),
        }),
    };
    let mut dialog = NewSessionDialog::new(
        AvailableTools::with_tools(&["claude"]),
        Vec::new(),
        "default",
        vec!["default".to_string()],
    )
    .with_remotes(vec![target]);
    dialog.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    env.view.new_dialog = Some(dialog);

    env.view
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(env.view.new_dialog.is_none());
    let flash = env.view.status_flash_text().unwrap_or_default().to_string();
    assert!(flash.contains("Creating session on mini"), "{flash}");
    assert_eq!(
        env.view.instances().count(),
        1,
        "nothing is created locally"
    );
}

#[test]
#[serial]
fn enter_on_a_remote_structured_row_opens_it_and_tab_explains_there_is_no_pane() {
    let mut env = create_test_env_with_sessions(1);
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    let structured: crate::daemon::SessionResponse = serde_json::from_value(
        serde_json::json!({"id": "s1", "title": "structured", "view": "structured"}),
    )
    .unwrap();
    with_remote(&mut env, vec![structured]);

    env.view.cursor = position_of_remote_row(&env, "s1").expect("listed");
    env.view.update_selected();
    match env.view.activate_selected_session() {
        Some(Action::OpenRemoteStructuredView { remote, id }) => {
            assert_eq!((remote.as_str(), id.as_str()), ("mini", "s1"));
        }
        other => panic!("expected a remote structured open, got {other:?}"),
    }
    match env
        .view
        .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None)
    {
        Some(Action::SetTransientStatus(message)) => {
            assert!(message.contains("press Enter"), "{message}")
        }
        other => panic!("expected a hint, got {other:?}"),
    }
    assert!(env.view.live_send.is_none());
}

/// A remote row selected with its preview laid out like a local one.
fn remote_preview_env() -> TestEnv {
    let mut env = create_test_env_with_sessions(1);
    register_mini();
    env.view.group_by = GroupByMode::Remote;
    env.view.group_by_is_default = false;
    with_remote(&mut env, vec![wire("r1", "remote one")]);
    env.view.cursor = position_of_remote_row(&env, "r1").expect("listed");
    env.view.update_selected();
    env.view.list_area = ratatui::layout::Rect::new(0, 0, 30, 40);
    env.view.preview_area = ratatui::layout::Rect::new(30, 0, 100, 40);
    env
}

fn remote_frame(lines: usize, tag: &str) -> crate::tui::home::remote_pane::RemoteFrame {
    crate::tui::home::remote_pane::RemoteFrame {
        content: (0..lines).map(|i| format!("{tag} {i}\n")).collect(),
        cursor: crate::tmux::PaneCursor {
            x: 0,
            y: 0,
            visible: false,
            pane_height: 24,
            history_size: 0,
            pane_width: 80,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        },
        budget: 4000,
    }
}

#[test]
#[serial]
fn remote_live_send_lights_the_preview_and_footer_like_local_live_send() {
    let theme = crate::tui::styles::load_theme("empire");
    let mut env = remote_preview_env();
    for live in [false, true] {
        if live {
            assert!(env.view.activate_selected_session().is_none());
        }
        let screen = render_home_to_string(&mut env.view, 120, 30);
        let footer = screen.lines().last().unwrap_or_default().to_string();
        let border = {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal
                .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
                .unwrap();
            let outer = env.view.preview_outer_area;
            terminal.backend().buffer()[(outer.x + outer.width / 2, outer.y)].fg
        };
        assert_eq!(border == theme.accent, live, "border accent, live={live}");
        assert_eq!(footer.contains("LIVE"), live, "{footer}");
        assert_eq!(footer.contains("remote one @ mini"), live, "{footer}");
        assert_eq!(footer.contains("Ctrl+Q to exit"), live, "{footer}");
    }
}

#[test]
#[serial]
fn a_remote_preview_scrolls_like_a_local_one_in_preview_and_live_send() {
    for live in [false, true] {
        let mut env = remote_preview_env();
        if live {
            assert!(env.view.activate_selected_session().is_none());
        }
        env.view.remote_preview_cache.dimensions = (80, 24);
        env.view.remote_preview_cache.captured_lines = 200;

        assert!(env.view.handle_scroll_up(50, 10), "live={live}");
        let after_wheel = env.view.preview_scroll_offset;
        assert!(after_wheel > 0, "live={live}");
        assert!(env.view.handle_scroll_down(50, 10), "live={live}");
        assert!(env.view.preview_scroll_offset < after_wheel, "live={live}");

        if live {
            env.view
                .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT), None);
            assert!(env.view.preview_scroll_offset > 0);
            assert!(env.view.live_send.is_some(), "scrolling stays live");
        }
    }
}

#[test]
#[serial]
fn a_wheel_over_a_full_screen_remote_pane_asks_the_daemon_unless_live_send_drives_it() {
    use crate::tui::remote_preview::{PendingWheel, PreviewCommand, RemotePreview};
    let mut env = remote_preview_env();
    let (preview, mut sent) = RemotePreview::recording();
    env.view.remote_preview = preview;
    env.view.preview_text_view.pane = ratatui::layout::Rect::new(30, 0, 100, 40);
    let mut cursor = remote_frame(0, "").cursor;
    cursor.alternate_on = true;
    cursor.mouse_tracking = true;
    cursor.mouse_sgr = true;
    env.view.remote_preview_cache.session_id = Some("r1".into());
    env.view.remote_preview_cache.cursor = Some(cursor);

    assert!(env.view.handle_scroll_up(50, 10));
    assert!(env.view.handle_scroll_up(50, 10));
    assert_eq!(env.view.preview_scroll_offset, 0);
    assert!(matches!(sent.try_recv(), Ok(PreviewCommand::Wheel)));
    assert!(sent.try_recv().is_err(), "a burst wakes the worker once");
    assert_eq!(
        env.view.remote_preview.slots.take_wheel(),
        Some(PendingWheel {
            col: 20,
            row: 10,
            notches: 2
        })
    );

    assert!(env.view.activate_selected_session().is_none());
    while sent.try_recv().is_ok() {}
    assert!(env.view.handle_scroll_down(50, 10));
    match sent.try_recv() {
        Ok(PreviewCommand::Input(bytes)) => assert_eq!(bytes, b"\x1b[<65;21;11M"),
        _ => panic!("live-send forwards the wheel as its own ordered input"),
    }
}

/// Another client taking the pane ends live mode with a notice naming it. The
/// notice also swallows the keys already typed for the pane, so none of them
/// reaches the session list as a shortcut.
#[test]
#[serial]
fn a_take_over_ends_remote_live_send_behind_a_notice_naming_the_taker() {
    use crate::tui::remote_preview::{PreviewEvent, RemotePreview};
    let mut env = remote_preview_env();
    let (preview, _commands) = RemotePreview::recording();
    env.view.remote_preview = preview;
    assert!(env.view.activate_selected_session().is_none());
    let key = env.view.remote_live_key().expect("remote live-send");
    env.view.remote_live_granted = true;

    env.view.remote_preview.emit(PreviewEvent::SizeOwner {
        key,
        is_owner: false,
        holder: Some("mac-mini (aoe)".into()),
    });
    assert!(env.view.apply_remote_preview());
    assert!(env.view.live_send.is_none(), "live mode ends");
    let dialog = env
        .view
        .info_dialog
        .as_ref()
        .expect("a notice guards input");
    assert!(
        dialog.message().contains("mac-mini (aoe) took over"),
        "{:?}",
        dialog.message()
    );
    // A key that would otherwise delete a session is consumed by the notice.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), None);
    assert!(env.view.confirm_dialog.is_none());
}

#[test]
#[serial]
fn a_scrolled_back_remote_preview_holds_frames_until_it_returns_to_the_live_edge() {
    let mut env = remote_preview_env();
    env.view.remote_preview_frame = Some(remote_frame(200, "old"));
    render_home_to_string(&mut env.view, 120, 40);
    assert!(env.view.remote_preview_cache.content.starts_with("old 0"));

    env.view.preview_scroll_offset = 10;
    env.view.remote_preview_frame = Some(remote_frame(200, "new"));
    render_home_to_string(&mut env.view, 120, 40);
    assert!(
        env.view.remote_preview_cache.content.starts_with("old 0"),
        "reading scrollback keeps the held text still"
    );
    assert!(env.view.remote_preview_frame.is_some(), "the frame waits");

    env.view.preview_scroll_offset = 0;
    render_home_to_string(&mut env.view, 120, 40);
    assert!(env.view.remote_preview_cache.content.starts_with("new 0"));
}
