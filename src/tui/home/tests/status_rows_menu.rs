//! Status updates, row decoration, and the context menu.

use super::*;

#[test]
#[serial]
fn wants_text_selection_tracks_copy_friendly_surfaces() {
    use crate::tui::dialogs::ChangelogDialog;

    let mut env = create_test_env_empty();

    // Fresh dashboard: mouse capture should stay on (wheel-scroll works).
    assert!(!env.view.wants_text_selection());

    // info_dialog (e.g. an error message the user might want to copy).
    env.view.info_dialog = Some(InfoDialog::new("Error", "something went wrong"));
    assert!(env.view.wants_text_selection());
    env.view.info_dialog = None;
    assert!(!env.view.wants_text_selection());

    // changelog_dialog (release notes).
    env.view.changelog_dialog = Some(ChangelogDialog::new(Some("1.0.0".to_string())));
    assert!(env.view.wants_text_selection());
    env.view.changelog_dialog = None;
    assert!(!env.view.wants_text_selection());

    use crate::tui::dialogs::ServeView;
    env.view.serve_view = Some(ServeView::new());
    assert!(env.view.wants_text_selection());
    env.view.serve_view = None;
    assert!(!env.view.wants_text_selection());
}

#[test]
#[serial]
fn archived_running_session_renders_stopped_icon_not_spinner() {
    // Regression for af711cb: pre-fix, archived/snoozed rows still cycled
    // through animated spinner frames driven by their underlying Running
    // status, making sunk rows read as "still alive" and pulling the eye
    // away from real attention items. Pin the icon to ICON_STOPPED for
    // archived rows even when status is Running.
    use crate::session::Status;
    use crate::tui::home::render::agent_row_icon;
    use crate::tui::home::ICON_STOPPED;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected one session"),
    };

    // Archive the session AND keep its underlying status as Running so the
    // spinner branch would fire in the absence of the override.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = Some(chrono::Utc::now());
    });

    let inst = env.view.get_instance(&id).expect("session present");
    let icon = agent_row_icon(inst);

    assert_eq!(
        icon, ICON_STOPPED,
        "archived row must render stopped icon, not animated spinner"
    );

    // Same expectation for snooze: a row snoozed into the future must not
    // animate even if it's also Running underneath.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = None;
        inst.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::minutes(15));
    });
    let inst = env.view.get_instance(&id).expect("session present");
    assert_eq!(
        agent_row_icon(inst),
        ICON_STOPPED,
        "snoozed row must render stopped icon, not animated spinner"
    );

    // Sanity: a plain Running row (no archive, no snooze) must NOT collapse
    // to ICON_STOPPED; otherwise the test would pass trivially because the
    // helper always returned the stopped glyph.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.archived_at = None;
        inst.snoozed_until = None;
    });
    let inst = env.view.get_instance(&id).expect("session present");
    assert_ne!(
        agent_row_icon(inst),
        ICON_STOPPED,
        "non-archived Running row should keep its spinner; helper would be a no-op otherwise"
    );
}

/// Regression: paste over a group header must stash to `pending_paste`,
/// never open a compose dialog targeted at "the first running session".
/// Earlier behavior fell through to the first-running fallback whenever
/// `selected_session` was None — silently misrouting voice/dictation
/// across groups. With cursor on a group, `selected_session` is None and
/// `resolve_send_target` must return None unconditionally.
#[test]
#[serial]
fn paste_on_group_header_stashes_instead_of_misrouting() {
    let mut env = create_test_env_with_groups();

    // Find the cursor index of the first group header in flat_items.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("fixture should produce at least one group header");
    env.view.cursor = group_idx;
    env.view.update_selected();

    // Cursor on a group sets selected_session to None.
    assert!(
        env.view.selected_session.is_none(),
        "cursor on a group header must clear selected_session"
    );

    env.view
        .handle_paste("voice dictation that must not misroute");

    assert!(
        env.view.send_message_dialog.is_none(),
        "paste over a group must NOT open a compose dialog against an unrelated session"
    );
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("voice dictation that must not misroute"),
        "paste over a group must stash to pending_paste"
    );
}

/// Regression: a transient status toast must render even when no aoe update
/// is pending. Before the fix, the update-bar row was only laid out when
/// `update_info.is_some()`, so toasts produced by paths like the
/// restart-during-attach failure or `Action::SendMessage`'s "Reviving
/// session..." were silently dropped on the floor for the common-case user
/// with no update available.
#[test]
#[serial]
fn update_bar_renders_status_toast_without_update_info() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let toast = "restart failed: tmux session unreachable";

    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, Some(toast), None);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("restart failed:"),
        "expected the toast to be rendered even when update_info is None.\n\
         Full buffer:\n{out}"
    );
    assert!(
        out.contains("[Ctrl+x] dismiss"),
        "expected the dismiss hint alongside the toast.\nFull buffer:\n{out}"
    );
}

/// The sandbox-image update banner renders (with its `[u] pull` /
/// `[Ctrl+x] dismiss` hints) when an `ImageUpdate` is present and no
/// higher-priority banner is up. Guards the lowest-priority slot in
/// `render_update_bar`.
#[test]
#[serial]
fn update_bar_renders_sandbox_image_banner() {
    use crate::containers::image_update::ImageUpdate;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let image_update = ImageUpdate {
        image: "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string(),
        remote_digest: "sha256:abc".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, None, None, Some(&image_update));
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("sandbox image update available"),
        "expected the sandbox image banner to render.\nFull buffer:\n{out}"
    );
    assert!(
        out.contains("[u] pull") && out.contains("[Ctrl+x] dismiss"),
        "expected the pull/dismiss hints alongside the image banner.\nFull buffer:\n{out}"
    );
}

/// The app-update banner wins the shared bottom row over a pending
/// sandbox-image update: only one shows at a time, so the lower-priority
/// image banner must stay hidden (and its `[u] pull` hint absent) while an
/// app update is up. This is what keeps the `u` / Ctrl+x keys unambiguous.
#[test]
#[serial]
fn app_update_banner_takes_precedence_over_image_banner() {
    use crate::containers::image_update::ImageUpdate;
    use crate::tui::styles::load_theme;
    use crate::update::UpdateInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let update_info = UpdateInfo {
        available: true,
        current_version: "1.0.0".to_string(),
        latest_version: "1.1.0".to_string(),
    };
    let image_update = ImageUpdate {
        image: "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string(),
        remote_digest: "sha256:abc".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(
                f,
                area,
                &theme,
                Some(&update_info),
                None,
                Some(&image_update),
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("update available 1.0.0"),
        "expected the app update banner to win the row.\nFull buffer:\n{out}"
    );
    assert!(
        !out.contains("sandbox image update available"),
        "image banner must stay hidden while an app update is shown.\nFull buffer:\n{out}"
    );
}

/// Issue #2220: the app-update banner reassures users that updating is safe
/// for running sessions. The reassurance must render alongside the version and
/// action keys so users know `u` won't tear down their work.
#[test]
#[serial]
fn app_update_banner_reassures_running_sessions_are_safe() {
    use crate::tui::styles::load_theme;
    use crate::update::UpdateInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    let update_info = UpdateInfo {
        available: true,
        current_version: "1.0.0".to_string(),
        latest_version: "1.1.0".to_string(),
    };

    terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, Some(&update_info), None, None);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }

    assert!(
        out.contains("running sessions stay safe"),
        "expected the update banner to reassure that running sessions are safe.\nFull buffer:\n{out}"
    );
    assert!(
        out.contains("[u] update"),
        "the action key must still render alongside the reassurance.\nFull buffer:\n{out}"
    );

    // Narrow-terminal contract: the reassurance is appended after the keys
    // precisely so the action hints survive when the line is too narrow to
    // hold everything. At 72 columns the keys fit but the reassurance clips.
    let narrow = TestBackend::new(72, 30);
    let mut narrow_terminal = Terminal::new(narrow).unwrap();
    narrow_terminal
        .draw(|f| {
            let area = f.area();
            env.view
                .render(f, area, &theme, Some(&update_info), None, None);
        })
        .unwrap();

    let nbuf = narrow_terminal.backend().buffer();
    let mut nout = String::new();
    for y in 0..nbuf.area.height {
        for x in 0..nbuf.area.width {
            nout.push_str(nbuf[(x, y)].symbol());
        }
        nout.push('\n');
    }

    assert!(
        nout.contains("[u] update") && nout.contains("[Ctrl+x] dismiss"),
        "the action keys must survive clipping on a narrow terminal.\nFull buffer:\n{nout}"
    );
    assert!(
        !nout.contains("running sessions stay safe"),
        "the trailing reassurance is expected to clip first on a narrow terminal.\nFull buffer:\n{nout}"
    );
}

/// Regression for the e2e CI failure (job 76034901940):
/// `test_command_palette_fuzzy_search_settings` and
/// `test_profile_picker_create_new_profile` failed because the harness types
/// fast enough to trip the paste-burst detector, and the resulting "paste"
/// got stashed in `pending_paste` instead of reaching the dialog's input.
/// `wants_paste_burst` must be false for dialogs that capture keys via
/// `handle_key` but do not implement `handle_paste`.
#[test]
#[serial]
fn wants_paste_burst_only_for_paste_aware_dialogs() {
    let mut env = create_test_env_empty();

    // No dialog open: burst is needed (home shortcuts at risk).
    assert!(
        env.view.wants_paste_burst(),
        "burst must be enabled when no dialog is open"
    );

    // Command palette: captures keys, no handle_paste. Burst would
    // strand input in pending_paste — must be disabled.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.command_palette.is_some(),
        "Ctrl+K must open the command palette"
    );
    assert!(
        !env.view.wants_paste_burst(),
        "burst must be disabled when command palette is open"
    );
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.command_palette.is_none());
    assert!(
        env.view.wants_paste_burst(),
        "burst should re-enable after dialog closes"
    );
}

#[test]
#[serial]
fn pollable_instances_excludes_recovery_in_flight() {
    let mut env = create_test_env_with_sessions(3);
    let id_skipped = env.view.instance_at(1).id.clone();
    env.view.recovery_in_flight.insert(id_skipped.clone());

    let pollable = env.view.pollable_instances();

    assert_eq!(pollable.len(), 2);
    assert!(pollable.iter().all(|i| i.id != id_skipped));
}

#[test]
#[serial]
fn pollable_instances_recovers_after_inflight_clear() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.recovery_in_flight.insert(id.clone());
    assert!(env.view.pollable_instances().is_empty());

    env.view.recovery_in_flight.remove(&id);

    assert_eq!(env.view.pollable_instances().len(), 1);
}

#[test]
#[serial]
fn system_health_survives_refresh_but_closes_on_selection_change() {
    let mut env = create_test_env_with_sessions(2);
    env.view.update_selected();
    env.view.open_system_health();

    env.view.update_selected();
    assert!(env.view.system_health_open);

    env.view.cursor = 1;
    env.view.update_selected();
    assert!(!env.view.system_health_open);
}

#[test]
#[serial]
fn system_health_tip_requires_three_six_agent_samples() {
    let mut env = create_test_env_empty();
    env.view.metrics.counts.agents = 6;

    env.view.observe_system_health_tip_load();
    env.view.observe_system_health_tip_load();
    assert!(!env.view.system_health_tip_earned);
    assert!(env.view.pending_tip_pop.is_none());

    env.view.metrics.counts.agents = 5;
    env.view.observe_system_health_tip_load();
    assert_eq!(env.view.system_health_tip_high_samples, 0);

    env.view.metrics.counts.agents = 6;
    for _ in 0..3 {
        env.view.observe_system_health_tip_load();
    }
    assert!(env.view.system_health_tip_earned);
    assert_eq!(
        env.view.pending_tip_pop.map(|tip| tip.id),
        Some("system-health")
    );

    env.view.open_system_health();
    assert!(env.view.pending_tip_pop.is_none());
    let config = crate::session::Config::load().unwrap();
    assert!(config.app_state.system_health_tip_earned);
    assert!(config.app_state.used_system_health);
}

/// Footer discoverability hints track where each key actually does something.
/// Archive/Snooze are Attention-only. Fav follows its keybind's own gate
/// (`Context::FavoritesUsable`): usable in Attention, or in any sort order
/// while `favorites_first` is on. The underlying keybinds are unchanged; only
/// the footer adapts so it doesn't waste width on a shortcut that would not
/// visibly do anything.
#[test]
#[serial]
fn footer_hides_attention_workflow_hints_outside_attention_sort() {
    use crate::session::config::SortOrder;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let original = crate::session::favorites_first();
    let theme = load_theme("empire");

    let render_footer = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Newest sort with favorites-first OFF: no attention-workflow shortcuts,
    // Fav excluded, because `f` is inert here.
    crate::session::set_favorites_first(false);
    env.view.sort_order = SortOrder::Newest;
    let newest_off = render_footer(&mut env);
    for hint in ["Snooze", "Fav", "Archive"] {
        assert!(
            !newest_off.contains(hint),
            "{hint} hint should be hidden in Newest sort with favorites-first off.\n{newest_off}"
        );
    }

    // Newest sort with favorites-first ON: Fav is advertised because `f` pins
    // here, but Archive/Snooze stay Attention-only.
    crate::session::set_favorites_first(true);
    let newest_on = render_footer(&mut env);
    assert!(
        newest_on.contains("Fav"),
        "Fav hint should appear in Newest sort with favorites-first on.\n{newest_on}"
    );
    for hint in ["Snooze", "Archive"] {
        assert!(
            !newest_on.contains(hint),
            "{hint} hint should stay hidden outside Attention sort.\n{newest_on}"
        );
    }

    // Attention sort: footer advertises all three regardless of the flag.
    env.view.sort_order = SortOrder::Attention;
    let attention_out = render_footer(&mut env);
    for hint in ["Snooze", "Fav", "Archive"] {
        assert!(
            attention_out.contains(hint),
            "{hint} hint should appear in Attention sort.\n{attention_out}"
        );
    }

    crate::session::set_favorites_first(original);
}

#[test]
#[serial]
fn favorite_without_runtime_cannot_change_local_state() {
    let mut env = create_test_env_with_sessions(1);
    env.view.selected_session = Some(env.view.instance_at(0).id.clone());
    assert!(env.view.toggle_favorite_at_cursor().is_err());
    assert!(!env.view.instance_at(0).is_favorited());
}

/// `toggle_archive_at_cursor` flips the cursor's instance archived state
/// and persists the change. No toast: the row sinks to tier 99 and that
/// visible reordering is the feedback.
#[test]
#[serial]
fn toggle_archive_at_cursor_round_trip() {
    let mut env = create_test_env_with_sessions(1);
    // Keep the Archived section expanded so the archived row stays reachable.
    env.view.archived_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Initial state: not archived.
    assert!(!env.view.instance_at(0).is_archived());

    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    assert!(env.view.instance_at(0).is_archived());

    // Archiving moved the selection off the row (it advances to the next
    // active session; here there is none). Navigate back onto the archived
    // row, as a user would, before toggling it back.
    env.view.select_session_by_id(&id);
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    assert!(!env.view.instance_at(0).is_archived());
}

/// Trashing a session hides it from the active list and surfaces it under
/// the synthetic Trash section; the shelve/unshelve key (`z`) restores it.
#[test]
#[serial]
fn trash_then_restore_round_trip() {
    let mut env = create_test_env_with_sessions(2);
    // Keep the Trash section expanded so the trashed row stays reachable.
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    assert!(!env.view.instance_at(0).is_trashed());

    env.view.trash_session_by_id(&id);
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be trashed"
    );

    // The Trash section header is present, and the trashed row is not in the
    // active flow (it renders under that header).
    let items = env.view.build_flat_items();
    assert!(
        items.iter().any(|it| matches!(
            it,
            Item::Group { path, .. } if crate::session::is_trash_section_path(path)
        )),
        "Trash section header must be present after trashing"
    );

    // Restore via the shelve/unshelve key.
    env.view.select_session_by_id(&id);
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be restored out of trash"
    );
}

/// Regression for #2489: trashing a session must not re-expand a Trash
/// section the user has collapsed. Like single-row archive, the section
/// header's count is the feedback; the collapse state is left untouched.
#[test]
#[serial]
fn trashing_leaves_collapsed_trash_section_collapsed() {
    let mut env = create_test_env_with_sessions(2);
    assert!(
        env.view.trashed_section_collapsed,
        "Trash section defaults to collapsed"
    );
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);

    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "session must be trashed"
    );
    assert!(
        env.view.trashed_section_collapsed,
        "trashing must not re-expand a collapsed Trash section"
    );
}

/// Regression: trashing must offload the blocking teardown (tmux kill, the
/// ~10s `docker stop`, and the worktree relocation) to the `TrashPoller`
/// instead of running it on the input thread, which froze the TUI while the
/// sandbox container stopped. The durable trash marker is still written inline
/// so the row flips to Trashed instantly; the teardown is merely queued.
#[test]
#[serial]
fn trash_offloads_blocking_teardown_to_poller() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);

    // Inline: the row is durably trashed the instant the key is handled.
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "trash marker must be written inline for instant feedback"
    );
    // Off-thread: the blocking teardown is in flight on the worker, tracked in
    // its pending set until a result is drained. If trashing had run the
    // teardown inline (the frozen-TUI bug), nothing would be queued here.
    let pending = env.view.trash_poller.take_pending();
    assert_eq!(
        pending,
        vec![id],
        "trash teardown must be queued on the TrashPoller, not run on the input thread"
    );
}

/// Trashing reserves a durable lifecycle generation before queueing teardown.
/// The worker may already have completed and released the lease by the time the
/// test reloads, but the monotonic generation proves ownership was acquired.
#[test]
#[serial]
fn trash_reserves_durable_lifecycle_generation() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);

    let rows = env.view.storages.get("test").unwrap().load().unwrap();
    let row = rows.iter().find(|instance| instance.id == id).unwrap();
    assert!(row.is_trashed());
    assert_eq!(row.lifecycle_generation, 1);
}

/// A plain session's no-relocation teardown releases its durable Trash reservation
/// before the worker publishes completion.
#[test]
#[serial]
fn trash_teardown_release_clears_durable_claim() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.trash_session_by_id(&id);
    let row = |view: &HomeView| {
        view.storages
            .get("test")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap()
    };

    // Drain the worker's completed transition.
    let mut drained = false;
    for _ in 0..100 {
        env.view.apply_trash_results();
        if !env.view.trash_poller.is_pending(&id) {
            drained = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(drained, "teardown result never drained");
    let final_row = row(&env.view);
    assert!(final_row.is_trashed(), "row stays trashed");
    assert_eq!(
        final_row.lifecycle_reservation, None,
        "Skipped teardown must release the Trash claim"
    );
}

/// Restore takes over a fresh Trash reservation before the queued teardown starts.
#[test]
#[serial]
fn trash_then_immediate_restore_hands_off_cleanly() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    env.view.trash_poller =
        crate::tui::trash_poller::TrashPoller::with_handler_for_test(move |request| {
            entered_tx.send(()).unwrap();
            release_rx.recv().expect("release teardown");
            crate::session::trash::perform_trash(&request)
        });
    let row = |view: &HomeView| {
        view.storages
            .get("test")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap()
    };

    env.view.selected_session = Some(id.clone());
    env.view.trash_session_by_id(&id);
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("teardown entered");
    let trashed = row(&env.view);
    assert!(trashed.is_trashed());
    assert!(trashed.lifecycle_reservation_is_owned(
        crate::session::LifecycleOperation::Trash,
        trashed.lifecycle_generation,
    ));

    env.view.selected_session = Some(id.clone());
    env.view.restore_selected_from_trash();
    let restored = row(&env.view);
    assert!(
        !restored.is_trashed(),
        "restore must seize the held Trash claim"
    );
    assert!(restored.lifecycle_generation > trashed.lifecycle_generation);
    assert_eq!(restored.lifecycle_reservation, None);

    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while env.view.trash_poller.is_pending(&id) {
        assert!(Instant::now() < deadline, "teardown result never drained");
        env.view.apply_trash_results();
        std::thread::yield_now();
    }
    let final_row = row(&env.view);
    assert!(
        !final_row.is_trashed(),
        "stale teardown must not undo restore"
    );
    assert_eq!(
        final_row.lifecycle_generation,
        restored.lifecycle_generation
    );
    assert_eq!(final_row.lifecycle_reservation, None);
}

/// Right-clicking the synthetic Trash section header opens the bulk menu
/// (Empty Trash / Restore All / Collapse), not the meaningless "Rename Group /
/// Delete Group" a real group would show.
#[test]
#[serial]
fn right_click_trash_header_shows_bulk_menu() {
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);

    let header_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| {
            matches!(it, Item::Group { path, .. }
                if crate::session::is_trash_section_path(path))
        })
        .expect("Trash header must render");
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, header_idx);
    assert!(env.view.handle_right_click(5, row));

    let labels: Vec<&str> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(labels, vec!["Empty Trash", "Restore All", "Collapse"]);
}

/// Right-clicking the synthetic Archived section header offers Restore All and
/// the collapse toggle, but no destructive "empty" action (archived rows are
/// never purged from there).
#[test]
#[serial]
fn right_click_archived_header_shows_restore_menu() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });

    let header_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| {
            matches!(it, Item::Group { path, .. }
                if crate::session::is_archived_section_path(path))
        })
        .expect("Archived header must render");
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, header_idx);
    assert!(env.view.handle_right_click(5, row));

    let labels: Vec<&str> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(labels, vec!["Restore All", "Collapse"]);
}

/// "Empty Trash" routes through a destructive confirm carrying the count; the
/// confirmed action queues every trashed row without taking a flock.
#[test]
#[serial]
fn empty_trash_confirm_purges_every_trashed_row() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(3);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    env.view.trash_session_by_id(&a);
    env.view.trash_session_by_id(&b);

    env.view.prompt_empty_trash();
    let dialog = env
        .view
        .confirm_dialog
        .as_ref()
        .expect("Empty Trash must open a confirm dialog");
    assert_eq!(dialog.action(), "empty_trash");

    env.view.dispatch_confirm_submit("empty_trash");
    for id in [&a, &b] {
        let inst = env
            .view
            .get_instance(id)
            .expect("row kept until purge lands");
        assert_eq!(
            inst.status,
            Status::Deleting,
            "each trashed row must be marked Deleting"
        );
    }
}

/// "Empty Trash" on an already-empty trash shows an info dialog instead of a
/// confirm that would delete nothing.
#[test]
#[serial]
fn empty_trash_on_empty_trash_is_a_noop_info() {
    let mut env = create_test_env_with_sessions(2);
    env.view.prompt_empty_trash();
    assert!(env.view.confirm_dialog.is_none());
    assert_eq!(
        env.view.info_dialog.as_ref().map(|d| d.title()),
        Some("Trash is empty")
    );
}

/// "Restore All" pulls every trashed session back out of the trash in one go.
#[test]
#[serial]
fn restore_all_from_trash_restores_every_row() {
    let mut env = create_test_env_with_sessions(3);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    env.view.trash_session_by_id(&a);
    env.view.trash_session_by_id(&b);
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .count(),
        2
    );

    env.view.restore_all_from_trash();
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .count(),
        0,
        "Restore All must un-trash every row"
    );
}

/// "Restore All" on the Archived section unarchives every archived session.
#[test]
#[serial]
fn unarchive_all_unarchives_every_row() {
    let mut env = create_test_env_with_sessions(3);
    for i in 0..2 {
        env.view.cursor = i;
        env.view.update_selected();
        with_canonical_archive(&mut env, |env| {
            env.view.toggle_archive_at_cursor().unwrap();
        });
    }
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_archived())
            .count(),
        2
    );

    env.view.unarchive_all();
    assert_eq!(
        env.view
            .instances
            .values()
            .filter(|i| i.is_archived())
            .count(),
        0,
        "Restore All (archived) must unarchive every row"
    );
}

/// The Trash section renders in the pinned shelf with its distinct type glyph,
/// and the sort indicator moves onto the divider above it.
#[test]
#[serial]
fn shelf_renders_trash_with_glyph_and_divider_sort() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);

    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut screen = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            screen.push_str(buf[(x, y)].symbol());
        }
        screen.push('\n');
    }

    assert!(
        screen.contains(crate::tui::home::ICON_TRASH_SECTION),
        "shelf must show the Trash type glyph"
    );
    assert!(
        screen.contains("Trash (1)"),
        "shelf must show the Trash count"
    );
    assert!(
        !screen.contains("sort:"),
        "the shelf divider must not duplicate the header's sort indicator"
    );
    assert!(
        env.view.shelf_inner_area.height > 0,
        "a shelf rect must be populated when trash is present"
    );
}

/// A trashed row whose permanent delete failed carries `Status::Error` +
/// `last_error` (set by `apply_deletion_results`). The preview must surface
/// that failure instead of the calm "Trash" placeholder, and the shelf row
/// must show the error glyph instead of the uniform muted one.
#[test]
#[serial]
fn trashed_preview_surfaces_delete_failure() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);

    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Error;
        inst.last_error = Some("worktree removal failed: directory locked".to_string());
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("worktree removal failed"),
        "a failed delete's error must show in the trashed preview.\n{screen}"
    );
    assert!(
        screen.contains(crate::tui::home::ICON_ERROR),
        "the shelf row must show the error glyph after a failed delete.\n{screen}"
    );
}

/// While a trashed row's permanent delete is running (`Status::Deleting`),
/// the preview placeholder must say so instead of advertising restore/delete
/// keys that would race the in-flight purge.
#[test]
#[serial]
fn trashed_preview_shows_deleting_status() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Deleting;
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("Deleting"),
        "an in-flight delete must be visible in the trashed preview.\n{screen}"
    );
}

/// An archived row can also be deleted; a failed delete must surface in the
/// archived preview the same way.
#[test]
#[serial]
fn archived_preview_surfaces_delete_failure() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Error;
        inst.last_error = Some("container teardown failed".to_string());
    });

    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        screen.contains("container teardown failed"),
        "a failed delete's error must show in the archived preview.\n{screen}"
    );
}

/// Restart (`e`) on an archived or trashed row is intentionally refused, but
/// the refusal must be visible: an info dialog, not a silent no-op.
#[test]
#[serial]
fn restart_on_trashed_row_surfaces_refusal() {
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);

    env.view
        .restart_selected_session(None, None, None, None)
        .unwrap();
    assert!(
        env.view.info_dialog.is_some(),
        "restarting a trashed row must explain why nothing happened"
    );
}

/// While a trashed row's permanent delete is in flight (`Status::Deleting`),
/// restart must stay a silent drop: the "press z to restore it first" dialog
/// would race the purge (same rationale as the Deleting preview body, which
/// drops the restore/delete hints).
#[test]
#[serial]
fn restart_on_deleting_trashed_row_stays_silent() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Deleting;
    });

    env.view
        .restart_selected_session(None, None, None, None)
        .unwrap();
    assert!(
        env.view.info_dialog.is_none(),
        "a mid-purge row must not get a restore hint that races the delete"
    );
}

/// In compact layouts (< 80 cols) the preview hoists the session's status
/// icon into the block title. A trashed row must mask a stale persisted
/// Running status there (no spinner above the "Trash" placeholder body),
/// matching the archived treatment.
#[test]
#[serial]
fn compact_title_masks_stale_spinner_on_trashed_row() {
    use crate::session::Status;
    let mut env = create_test_env_with_sessions(2);
    env.view.trashed_section_collapsed = false;
    let id = env.view.instance_at(0).id.clone();
    env.view.trash_session_by_id(&id);
    env.view.select_session_by_id(&id);
    // Stale persisted live status; the pane was killed on trash.
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
    });

    let screen = render_home_to_string(&mut env.view, 70, 40);
    assert!(
        screen.contains("Trash"),
        "trashed placeholder should render.\n{screen}"
    );
    // The hoisted preview title starts at the block's top-left corner. With
    // the mask it carries ICON_STOPPED; unmasked, Running would paint a
    // time-varying `dots()` spinner frame there instead (a frame set that
    // never includes ICON_STOPPED, so this pin cannot pass by accident).
    let masked_title = format!("\u{256d} {} session0", crate::tui::home::ICON_STOPPED);
    assert!(
        screen.contains(&masked_title),
        "a trashed row's compact title must show the stopped icon, not a stale spinner.\n{screen}"
    );
}

/// Regression for #2489: `w` (jump to next needing-attention) must skip
/// trashed rows even when a stale unread flag survived the trash. A trashed
/// session is stopped and only lives under the Trash section, so it never
/// "needs attention".
#[test]
#[serial]
fn w_skips_unread_trashed_session() {
    use crate::session::Status;
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    // Non-strict so bare `w` routes to the jump handler, not the typing guard.
    env.view.strict_hotkeys = false;
    // Keep the Trash section expanded so the trashed row lands in `flat_items`;
    // that is the only way `w`'s walk could reach it.
    env.view.trashed_section_collapsed = false;

    let trashed = env.view.instance_at(0).id.clone();
    let active = env.view.instance_at(1).id.clone();
    // The surviving active row is a plain idle session (the pass-2 fallback);
    // the trashed row carries an unread flag, as it would after being trashed
    // while unread.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&trashed, |inst| inst.mark_unread());
    env.view.trash_session_by_id(&trashed);
    assert!(env.view.get_instance(&trashed).unwrap().is_trashed());
    assert!(
        env.view.get_instance(&trashed).unwrap().is_unread(),
        "the trashed row must still carry the unread flag for this regression"
    );

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(trashed.as_str()),
        "`w` must not land on a trashed session even when it is unread"
    );
}

/// Snooze is the same "don't bother me" sink state as trash/archive for
/// every other subsystem (see `Instance::is_snoozed`); `w`'s forward walk
/// (pass 1) must skip a snoozed session even though it is otherwise
/// `Status::Waiting`, exactly the state `w` is looking for. A third,
/// eligible `Status::Waiting` control session proves the walk actually
/// found and landed on a real target rather than merely failing to land
/// on the snoozed one (with only two sessions "landed != snoozed" would
/// hold trivially even if `w` did nothing at all).
#[test]
#[serial]
fn w_skips_snoozed_waiting_session() {
    use crate::session::Status;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = false;

    let snoozed = env.view.instances[0].id.clone();
    let active = env.view.instances[1].id.clone();
    let control = env.view.instances[2].id.clone();
    // The active row is a plain idle session the cursor starts on; the
    // control row is a legitimate, non-dismissed Waiting session `w` should
    // land on; the snoozed row is otherwise Waiting too, as it would be if
    // it started waiting on input before being snoozed.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&control, |inst| inst.status = Status::Waiting);
    env.view.mutate_instance(&snoozed, |inst| {
        inst.status = Status::Waiting;
        inst.snooze(30);
    });
    assert!(env.view.get_instance(&snoozed).unwrap().is_snoozed());

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(snoozed.as_str()),
        "`w` must not land on a snoozed session even though it is Waiting"
    );
    assert_eq!(
        landed.as_deref(),
        Some(control.as_str()),
        "`w` must land on the eligible Waiting session, proving the forward walk actually ran"
    );
}

/// Same contract as `w_skips_snoozed_waiting_session`, but for the pass-2
/// idle fallback: with the only Idle candidate snoozed, `w` must not treat
/// it as the "most-recently-accessed Idle session" fallback target.
#[test]
#[serial]
fn w_skips_snoozed_idle_session_in_fallback() {
    let (mut env, running, idle) = attention_env_running_then_idle();
    let running_id = match env.view.flat_items.get(running) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the running row to be a session item"),
    };
    let idle_id = match env.view.flat_items.get(idle) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the idle row to be a session item"),
    };
    env.view.mutate_instance(&idle_id, |inst| inst.snooze(30));
    assert!(env.view.get_instance(&idle_id).unwrap().is_snoozed());

    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(idle_id.as_str()),
        "`w` must not fall back to a snoozed session even when it is the only Idle candidate"
    );
    assert_eq!(
        landed.as_deref(),
        Some(running_id.as_str()),
        "with no eligible target, `w` must leave the cursor on the Running session"
    );
}

/// Archive is the same "don't bother me" sink state as trash/snooze (see
/// `Instance::archive`'s mutual-exclusion doc comment: archive is "the
/// strongest dismiss"); `w`'s forward walk (pass 1) must skip an archived
/// session even though it is otherwise `Status::Waiting`, exactly the state
/// `w` is looking for. A third, eligible `Status::Waiting` control session
/// proves the walk actually found and landed on a real target rather than
/// merely failing to land on the archived one (with only two sessions
/// "landed != archived" would hold trivially even if `w` did nothing at
/// all).
#[test]
#[serial]
fn w_skips_archived_waiting_session() {
    use crate::session::Status;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = false;
    // Keep the Archived section expanded so the archived row lands in
    // `flat_items`; that is the only way `w`'s walk could reach it.
    env.view.archived_section_collapsed = false;

    let archived = env.view.instances[0].id.clone();
    let active = env.view.instances[1].id.clone();
    let control = env.view.instances[2].id.clone();
    // The active row is a plain idle session the cursor starts on; the
    // control row is a legitimate, non-dismissed Waiting session `w` should
    // land on; the archived row is otherwise Waiting too, as it would be if
    // it started waiting on input before being archived.
    env.view
        .mutate_instance(&active, |inst| inst.status = Status::Idle);
    env.view
        .mutate_instance(&control, |inst| inst.status = Status::Waiting);
    env.view.mutate_instance(&archived, |inst| {
        inst.status = Status::Waiting;
        inst.archive();
    });
    assert!(env.view.get_instance(&archived).unwrap().is_archived());

    env.view.select_session_by_id(&active);
    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(archived.as_str()),
        "`w` must not land on an archived session even though it is Waiting"
    );
    assert_eq!(
        landed.as_deref(),
        Some(control.as_str()),
        "`w` must land on the eligible Waiting session, proving the forward walk actually ran"
    );
}

/// Same contract as `w_skips_archived_waiting_session`, but for the pass-2
/// idle fallback: with the only Idle candidate archived, `w` must not treat
/// it as the "most-recently-accessed Idle session" fallback target.
#[test]
#[serial]
fn w_skips_archived_idle_session_in_fallback() {
    let (mut env, running, idle) = attention_env_running_then_idle();
    let running_id = match env.view.flat_items.get(running) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the running row to be a session item"),
    };
    let idle_id = match env.view.flat_items.get(idle) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the idle row to be a session item"),
    };
    // `mutate_instance` updates the instance in place without rebuilding
    // `flat_items`, so the now-archived row stays at its original index
    // (pass 2 re-derives its dismissed state from the live instance, not
    // from `flat_items`, so this still exercises the real code path).
    env.view.mutate_instance(&idle_id, |inst| inst.archive());
    assert!(env.view.get_instance(&idle_id).unwrap().is_archived());

    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => Some(id.clone()),
        _ => None,
    };
    assert_ne!(
        landed.as_deref(),
        Some(idle_id.as_str()),
        "`w` must not fall back to an archived session even when it is the only Idle candidate"
    );
    assert_eq!(
        landed.as_deref(),
        Some(running_id.as_str()),
        "with no eligible target, `w` must leave the cursor on the Running session"
    );
}

/// The default gesture: `d` opens the confirm dialog and a second `d` accepts
/// it, trashing the session both in memory and on disk. See #3364.
#[test]
#[serial]
fn d_then_d_confirms_the_trash_and_persists_the_marker() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "the first d must only open the dialog"
    );
    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        env.view.confirm_dialog.is_none(),
        "the confirming d must close the dialog"
    );
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "a confirmed delete with default trash-first config must mark the row trashed in memory"
    );
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "trash-first d must not open the permanent-delete dialog"
    );
    let disk_row = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|inst| inst.id == id)
        .expect("disk row present");
    assert!(
        disk_row.is_trashed(),
        "confirming must persist trashed_at so a storage refresh cannot resurrect a killed session"
    );
}

/// Turning `session.confirm_delete` off restores the historical
/// one-keystroke trash. See #3364.
#[test]
#[serial]
fn d_with_confirm_delete_off_trashes_on_the_keystroke() {
    let mut env = create_test_env_with_sessions(2);
    disable_confirm_delete();
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        env.view.confirm_dialog.is_none(),
        "confirm_delete off must not open a dialog"
    );
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "confirm_delete off must trash on the keystroke"
    );
}

/// Ticking the dialog's "don't warn me again" checkbox trashes the session
/// and persists `confirm_delete = false`, so the next `d` goes back to the
/// one-keystroke trash without a trip through the settings pane. See #3364.
#[test]
#[serial]
fn confirm_delete_dont_ask_again_persists_the_opt_out() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);
    // Space ticks the checkbox, the second `d` accepts.
    env.view.handle_key(key(KeyCode::Char(' ')), None);
    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "accepting with the checkbox ticked must still trash the session"
    );
    assert!(
        !crate::session::config::Config::load()
            .unwrap()
            .session
            .confirm_delete,
        "the opt-out must be persisted to config"
    );
}

/// With `session.confirm_delete` on (the default), `d` opens a confirmation
/// dialog and does not trash until the dialog is accepted; accepting then runs
/// the same trash path as the instant flow. See #2583.
#[test]
#[serial]
fn d_with_confirm_delete_prompts_before_trashing() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);

    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "confirm_delete on must not trash the session on the keystroke"
    );
    let dialog = env
        .view
        .confirm_dialog
        .as_ref()
        .expect("confirm_delete on must open a confirmation dialog");
    assert_eq!(dialog.action(), "trash_session");
    assert_eq!(
        env.view.pending_trash_session.as_deref(),
        Some(id.as_str()),
        "the pending trash target must be the selected session"
    );

    // Accepting the dialog trashes via the same trash_session_by_id path.
    env.view.dispatch_confirm_submit("trash_session");
    assert!(
        env.view.get_instance(&id).unwrap().is_trashed(),
        "accepting the confirm dialog must trash the session"
    );
    assert!(
        env.view.pending_trash_session.is_none(),
        "the pending trash target must be cleared once consumed"
    );
}

/// Cancelling the `session.confirm_delete` dialog leaves the session untouched
/// and clears the pending target. See #2583.
#[test]
#[serial]
fn confirm_delete_dialog_cancel_leaves_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.selected_session.clone().unwrap();

    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.confirm_dialog.is_some());

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        env.view.confirm_dialog.is_none(),
        "Esc must dismiss the confirm dialog"
    );
    assert!(
        !env.view.get_instance(&id).unwrap().is_trashed(),
        "cancelling the confirm dialog must not trash the session"
    );
    assert!(
        env.view.pending_trash_session.is_none(),
        "cancelling must clear the pending trash target"
    );
}

/// When no session is selected, the toggle is a silent no-op.
#[test]
#[serial]
fn toggle_archive_at_cursor_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
}
