//! Key handling, cursor and page navigation, and watcher refresh.

use super::*;

#[test]
fn duplicate_session_ids_are_excluded_from_home_map() {
    let mut first = Instance::new("first", "/tmp/first");
    first.id = "duplicate-id".to_string();
    first.source_profile = "alpha".to_string();
    let mut second = first.clone();
    second.source_profile = "beta".to_string();
    let unique = Instance::new("unique", "/tmp/unique");
    let unique_id = unique.id.clone();

    let map = HomeView::build_instances_map(vec![first, unique, second]);

    assert!(!map.contains_key("duplicate-id"));
    assert!(map.contains_key(&unique_id));
}

// #1897 / CodeRabbit follow-up: `add_instance` is the funnel for both the
// `Creating` placeholder stub (async creation flow) and the finalized session
// row. The opt-in create-trend counter must bump only for finalized inserts, or
// a successful background create double-counts (stub + real) and a cancelled one
// counts a session that never existed. Asserts deltas (not absolutes) since the
// counter is a process-global shared with the `telemetry_creates` serial group.
#[test]
#[serial]
#[serial_test::serial(telemetry_creates)]
fn add_instance_counts_only_finalized_creates() {
    use crate::session::Status;
    let mut env = create_test_env_empty();
    let before = crate::tui::app::session_create_count_for_test();

    let mut stub = Instance::new("stub", "/tmp/test");
    stub.source_profile = "test".to_string();
    stub.status = Status::Creating;
    env.view.add_instance(stub);
    assert_eq!(
        crate::tui::app::session_create_count_for_test(),
        before,
        "a Creating placeholder stub must not bump the create counter"
    );

    let mut real = Instance::new("real", "/tmp/test");
    real.source_profile = "test".to_string();
    env.view.add_instance(real);
    assert_eq!(
        crate::tui::app::session_create_count_for_test(),
        before + 1,
        "a finalized session insert must bump the create counter exactly once"
    );
}

#[test]
#[serial]
fn rewire_disk_subscriptions_is_noop_without_tokio_runtime() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    let current = vec!["test".to_string()];

    assert!(
        view.disk_watch.handles.is_empty(),
        "construction outside a tokio runtime must not prewire subscriptions"
    );
    view.rewire_disk_subscriptions(&current);
    assert!(
        view.disk_watch.handles.is_empty(),
        "rewire outside a tokio runtime must stay a no-op for lib tests"
    );
    assert!(
        !view
            .disk_watch
            .dirty
            .load(std::sync::atomic::Ordering::Acquire),
        "the noop branch must leave disk_dirty clear outside a runtime"
    );
}

#[test]
#[serial]
fn watcher_refresh_does_not_reopen_hotkey_warning_dialog() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(
        &global_config,
        "[tools.alpha]\ncommand = \"alpha\"\nhotkey = \"Ctrl+g\"\n",
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["alpha"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.info_dialog.is_some(),
        "precondition: initial load shows warning dialog"
    );
    view.info_dialog = None;

    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Watcher);
    assert!(
        view.info_dialog.is_none(),
        "watcher-driven refresh must not reopen the hotkey warning dialog"
    );
}

#[test]
#[serial]
fn interactive_refresh_reopens_hotkey_warning_dialog() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(
        &global_config,
        "[tools.alpha]\ncommand = \"alpha\"\nhotkey = \"Ctrl+g\"\n",
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["alpha"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.info_dialog = None;

    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Interactive);
    assert!(
        view.info_dialog.is_some(),
        "interactive refresh must still surface the hotkey warning dialog"
    );
}

#[test]
#[serial]
fn watcher_refresh_stashes_pending_watcher_theme() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("dracula"),
        "watcher-driven refresh must stash the resolved theme name so the tick loop can dispatch App::set_theme"
    );
}

#[test]
#[serial]
fn interactive_refresh_does_not_stash_pending_watcher_theme() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Interactive);
    assert!(
        view.pending_watcher_theme.is_none(),
        "interactive refresh must not stash a pending theme; settings/intro input handlers dispatch Action::SetTheme directly"
    );
}

#[test]
#[serial]
fn take_pending_watcher_theme_clears_the_field() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.pending_watcher_theme = Some("zinc".to_string());

    let first = view.take_pending_watcher_theme();
    let second = view.take_pending_watcher_theme();
    assert_eq!(first.as_deref(), Some("zinc"));
    assert!(
        second.is_none(),
        "take must drain the pending field so a single watcher refresh dispatches at most one set_theme"
    );
}

#[test]
#[serial]
fn watcher_refresh_stashes_global_theme_not_profile_override() {
    use crate::session::config::profile_config::{save_profile_config, ProfileConfig};
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();

    let profile_overrides: ProfileConfig =
        serde_json::from_value(serde_json::json!({"theme": {"name": "empire"}}))
            .expect("legacy hand-edited overrides may carry a theme key even though theme is global by contract");
    save_profile_config("test", &profile_overrides).unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert!(
        view.pending_watcher_theme.is_none(),
        "precondition: HomeView::new must not stash a pending watcher theme"
    );

    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("dracula"),
        "watcher path must stash the global theme name via resolve_theme_name; a stale per-profile theme override (legacy or hand-edited) must not mask the global value"
    );
}

#[test]
#[serial]
fn second_watcher_refresh_overwrites_stale_stash() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    let global_config = crate::session::get_app_dir().unwrap().join("config.toml");
    std::fs::write(&global_config, "[theme]\nname = \"dracula\"\n").unwrap();
    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Watcher);
    assert_eq!(view.pending_watcher_theme.as_deref(), Some("dracula"));

    std::fs::write(&global_config, "[theme]\nname = \"empire\"\n").unwrap();
    view.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Watcher);
    assert_eq!(
        view.pending_watcher_theme.as_deref(),
        Some("empire"),
        "second watcher refresh must overwrite the stale stash; first-write-wins would silently drop the latest theme change"
    );
}

#[test]
#[serial]
fn test_initial_cursor_position() {
    let env = create_test_env_with_sessions(3);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn preview_info_follows_flag_and_never_auto_shows_in_live() {
    // Info-header visibility is purely the persisted `show_preview_info` toggle
    // (driven by `i` in the TUI). Live mode must NOT change it: if the user
    // hid the header, it stays hidden when they go live, and a shown header
    // stays shown. Nothing magically re-shows it.
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    let live_state = || LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    };

    // Hidden via the toggle: gone outside live...
    env.view.show_preview_info = false;
    let hidden_not_live = render_to_string(&mut env.view);
    assert!(
        !hidden_not_live.contains("Profile:"),
        "header must be hidden when the flag is off.\n{hidden_not_live}"
    );
    // ...and STILL gone after going live (the regression the user reported:
    // it must never magically re-show).
    env.view.live_send = Some(live_state());
    let hidden_live = render_to_string(&mut env.view);
    assert!(
        !hidden_live.contains("Profile:"),
        "a hidden header must not re-appear in live mode.\n{hidden_live}"
    );

    // Shown via the toggle: present both outside and inside live mode.
    env.view.live_send = None;
    env.view.show_preview_info = true;
    let shown_not_live = render_to_string(&mut env.view);
    assert!(
        shown_not_live.contains("Profile:"),
        "header must render when the flag is on.\n{shown_not_live}"
    );
    env.view.live_send = Some(live_state());
    let shown_live = render_to_string(&mut env.view);
    assert!(
        shown_live.contains("Profile:"),
        "a shown header stays shown in live mode (flag, not mode, governs it).\n{shown_live}"
    );
}

/// The app-level global keybindings defer Ctrl+C to live-send via this
/// predicate (#2894). It is true only while live mode owns the keyboard: an
/// overlay opened on top of live mode takes focus back, and Ctrl+C should
/// then behave normally.
#[test]
#[serial]
fn is_live_send_capturing_tracks_state_and_overlays() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();

    assert!(
        !env.view.is_live_send_capturing(),
        "no live-send means the keyboard is not captured"
    );

    env.view.live_send = Some(LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
    assert!(
        env.view.is_live_send_capturing(),
        "live-send with no overlay captures the keyboard"
    );

    // An overlay over live mode hands focus back to the overlay, so Ctrl+C
    // must stop being routed to the agent.
    env.view.info_dialog = Some(InfoDialog::new("t", "b"));
    assert!(
        !env.view.is_live_send_capturing(),
        "an overlay over live-send releases the capture"
    );
}

/// #2894: in live mode Ctrl+C is forwarded to the agent (an interrupt), not
/// treated as a quit, and each forward arms the footer reminder. Drives the
/// real `handle_key` routing with the default `C-q` exit chord present so the
/// test proves Ctrl+C is distinct from exiting.
#[test]
#[serial]
fn ctrl_c_in_live_mode_forwards_to_agent_and_flashes() {
    use crate::tui::home::live_send::{parse_chord_list, LiveSendState, LiveSendTarget};

    let mut env = create_test_env_with_sessions(1);
    let inst = env.view.instance_at(0).clone();

    // Match the generated tmux name so the drift guard doesn't tear live mode
    // down before the key is translated.
    let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    env.view.live_send = Some(LiveSendState {
        session_id: inst.id.clone(),
        title: inst.title.clone(),
        tmux_name,
        target: LiveSendTarget::Agent,
        exit_chords: parse_chord_list("C-q"),
        leader: None,
    });

    assert!(!env.view.live_send_ctrl_c_flash_active());

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let action = env.view.handle_key(ctrl_c, None);

    assert!(
        action.is_none(),
        "Ctrl+C in live mode produces no home-view action"
    );
    assert!(
        env.view.live_send.is_some(),
        "Ctrl+C reaches the agent; it must not exit live mode"
    );
    assert!(
        env.view.live_send_ctrl_c_flash_active(),
        "forwarding Ctrl+C arms the footer reminder"
    );
}

/// The live-send footer renders the "Ctrl+C sent to agent" reminder only
/// while the flash window is open (#2894).
#[test]
#[serial]
fn ctrl_c_flash_renders_in_live_footer() {
    use crate::tui::home::live_send::{parse_chord_list, LiveSendState, LiveSendTarget};
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    env.view.live_send = Some(LiveSendState {
        session_id: id.clone(),
        title: "session0".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: parse_chord_list("C-q"),
        leader: None,
    });

    let without = render_to_string(&mut env.view);
    assert!(
        !without.contains("Ctrl+C sent to agent"),
        "the reminder must be absent before any Ctrl+C\n{without}"
    );

    env.view.flash_ctrl_c_hint();
    let with = render_to_string(&mut env.view);
    assert!(
        with.contains("Ctrl+C sent to agent"),
        "the reminder must render while the flash window is open\n{with}"
    );
}

#[test]
#[serial]
fn preview_visible_rows_equal_output_area_with_info_shown() {
    // With the info header shown, the Agent branch sizes the pane to
    // `PreviewLayout::compute(..).output` (header + banner removed once) and the
    // renderer paints into the same rect. `preview_visible_rows` must equal
    // `preview_pane_area.height`; the historical bugs all came from a second,
    // drifting derivation of this number, now consolidated into one layout.
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Structured;
    env.view.show_preview_info = true;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    assert!(
        env.view.preview_pane_area.height > 0,
        "expected a non-empty output sub-rect at 120x40 (non-compact)"
    );
    assert_eq!(
        env.view.preview_visible_rows, env.view.preview_pane_area.height as usize,
        "visible rows must match the output area height, not be a row short"
    );
}

/// Precedence: unread paints only on resting (Idle/Unknown) rows. A live
/// status supersedes it, keeping its own spinner — so a Running session that
/// also carries an unread marker must NOT show the solid unread dot. See the
/// #2088 review note about jumbled precedence.
#[test]
#[serial]
fn unread_dot_yields_to_a_running_status() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    // Idle + unread: the row shows the solid unread dot.
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        render(&mut env).contains('●'),
        "an idle unread row should paint the unread dot"
    );

    // Running + still unread: the live status wins; no unread dot.
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Running);
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a running row must keep its spinner, not the unread dot"
    );
}

/// Sunk rows never paint the unread dot. Archiving or snoozing an unread
/// row dismisses it; surfacing it as unread contradicts that. The snooze
/// case must hold in every sort mode, not just Attention (#2571).
#[test]
#[serial]
fn unread_dot_suppressed_on_archived_and_snoozed() {
    use crate::session::config::SortOrder;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    // Baseline: an idle unread row paints the dot.
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        render(&mut env).contains('●'),
        "an idle unread row should paint the unread dot"
    );

    // Snoozed, in a non-Attention sort: the dot must be gone even though the
    // snooze decoration itself is Attention-only.
    env.view.sort_order = SortOrder::Newest;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a snoozed unread row must not paint the unread dot outside Attention sort"
    );

    // Snoozed in Attention sort: still no dot.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "a snoozed unread row must not paint the unread dot in Attention sort"
    );

    // Archived: the archive override already mutes the glyph; guard it stays muted.
    env.view.mutate_instance(&id, |inst| {
        inst.unsnooze();
        inst.archive();
    });
    env.view.flat_items = env.view.build_flat_items();
    assert!(
        !render(&mut env).contains('●'),
        "an archived unread row must not paint the unread dot"
    );
}

/// Unread is an Agent-view concept: the dot marks agent output the user hasn't
/// seen. The paired terminal has no such notion, so Terminal view must never
/// paint the unread dot even when the underlying instance carries the flag.
#[test]
#[serial]
fn unread_dot_never_paints_in_terminal_view() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    };

    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();

    // Agent view: the idle unread row paints the dot (baseline sanity).
    env.view.view_mode = ViewMode::Structured;
    assert!(
        render(&mut env).contains('●'),
        "agent view should paint the unread dot for an idle unread row"
    );

    // Terminal view: same instance, no dot. The terminal pane isn't running
    // (no tmux session in the test env), so the row shows its idle glyph.
    env.view.view_mode = ViewMode::Terminal;
    assert!(
        !render(&mut env).contains('●'),
        "terminal view must not paint the unread dot"
    );
}

/// Stop targets what the user is looking at: Agent view arms the
/// `stop_session` confirm (agent + container stop), while Terminal and Tool
/// views route to their respective pane-kill paths and never arm an agent
/// stop. The routing decision lives in `stop_selected`.
#[test]
#[serial]
fn stop_in_terminal_view_does_not_target_agent_session() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Idle);
    env.view.selected_session = Some(id.clone());

    // Agent view: Stop arms the agent-session stop and opens its confirm.
    env.view.view_mode = ViewMode::Structured;
    env.view.stop_selected();
    assert_eq!(
        env.view.pending_stop_session.as_deref(),
        Some(id.as_str()),
        "agent view stop must arm the agent-session stop"
    );
    assert_eq!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "agent view stop must open the stop-session confirm"
    );

    env.view.confirm_dialog = None;
    env.view.pending_stop_session = None;

    // Terminal view: Stop must never touch the agent session. With no live
    // terminal in the test env the terminal-kill path no-ops, but the critical
    // invariant is that no agent stop was armed and the stop-session confirm
    // never opened.
    env.view.view_mode = ViewMode::Terminal;
    env.view.stop_selected();
    assert!(
        env.view.pending_stop_session.is_none(),
        "terminal view stop must not arm an agent-session stop"
    );
    assert_ne!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "terminal view stop must not open the stop-session confirm"
    );

    // Tool view: same invariant, Stop routes to the tool-kill path and never
    // arms an agent stop.
    env.view.view_mode = ViewMode::Tool("lazygit".to_string());
    env.view.stop_selected();
    assert!(
        env.view.pending_stop_session.is_none(),
        "tool view stop must not arm an agent-session stop"
    );
    assert_ne!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("stop_session"),
        "tool view stop must not open the stop-session confirm"
    );
}

/// Render suppression is cosmetic: archive/snooze leave the `unread` flag on
/// disk so unarchiving or unsnoozing brings the marker back (#2571).
#[test]
fn unread_flag_survives_sink_round_trip() {
    let mut inst = crate::session::Instance::new("rt", "/tmp/rt");
    inst.mark_unread();

    inst.archive();
    assert!(inst.is_unread(), "archive must not clear unread");
    inst.unarchive();
    assert!(inst.is_unread(), "unarchive must keep unread");

    inst.snooze(30);
    assert!(inst.is_unread(), "snooze must not clear unread");
    inst.unsnooze();
    assert!(inst.is_unread(), "unsnooze must keep unread");
}

/// Dwell-to-read: an unread row that stays selected past `UNREAD_DWELL`
/// (with the list in the foreground) is cleared, distinguishing "stopped to
/// read it" from "scrolled past."
#[test]
#[serial]
fn unread_dwell_clears_after_threshold() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
        inst.mark_unread();
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    assert!(env.view.get_instance(&id).unwrap().is_unread());

    let t0 = Instant::now();
    // First tick arms the dwell clock; nothing cleared yet.
    assert!(!env.view.tick_unread_dwell(t0));
    assert!(env.view.get_instance(&id).unwrap().is_unread());
    // Below the threshold: still unread (this is the "scrolled past" guard).
    assert!(!env.view.tick_unread_dwell(t0 + Duration::from_millis(500)));
    assert!(env.view.get_instance(&id).unwrap().is_unread());
    // Past the threshold: cleared.
    assert!(env
        .view
        .tick_unread_dwell(t0 + crate::tui::home::UNREAD_DWELL + Duration::from_millis(1)));
    assert!(!env.view.get_instance(&id).unwrap().is_unread());
}

/// A fresh manual flag (`u`) is held for the current visit: marking a session
/// unread and keeping the cursor on it must not let dwell-to-read undo the
/// mark. Regression for the bug where staying on the row past `UNREAD_DWELL`
/// silently re-cleared it.
#[test]
#[serial]
fn manual_unread_survives_same_visit_dwell() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    env.view
        .toggle_unread_at_cursor()
        .expect("manual toggle should succeed");
    assert!(env.view.get_instance(&id).unwrap().is_unread());

    let t0 = Instant::now();
    // Arm the clock, then sit well past the threshold without moving: the hold
    // keeps the mark.
    assert!(!env.view.tick_unread_dwell(t0));
    assert!(!env
        .view
        .tick_unread_dwell(t0 + crate::tui::home::UNREAD_DWELL + Duration::from_secs(5)));
    assert!(
        env.view.get_instance(&id).unwrap().is_unread(),
        "a freshly hand-flagged row must survive dwell while it stays selected"
    );
}

/// The manual hold is per-visit: after leaving a hand-flagged row and coming
/// back, dwelling on it clears the mark like any other unread row. This is the
/// behavior verified by hand (select A, mark unread, move to B, move back, sit
/// past the threshold -> it clears).
#[test]
#[serial]
fn manual_unread_clears_after_leave_and_return() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    for id in [&a, &b] {
        env.view.mutate_instance(id, |inst| {
            inst.status = crate::session::Status::Idle;
        });
    }
    env.view.flat_items = env.view.build_flat_items();

    // Select A and flag it unread by hand.
    env.view.select_session_by_id(&a);
    env.view.toggle_unread_at_cursor().expect("manual mark A");
    assert!(env.view.get_instance(&a).unwrap().is_unread());

    // Move to B with NO dwell tick in between (a quick hop, like real
    // navigation). The hold must release purely from the selection change;
    // otherwise returning to A would stay suppressed forever (the reported
    // bug, which an in-between tick would have masked).
    env.view.select_session_by_id(&b);
    assert!(
        env.view.manual_unread_hold.is_none(),
        "moving off the row must release the hold without needing a dwell tick"
    );

    // Come back to A: arm the clock, then sit past the threshold. Now it clears.
    let t0 = Instant::now();
    env.view.select_session_by_id(&a);
    assert!(!env.view.tick_unread_dwell(t0));
    let cleared = env
        .view
        .tick_unread_dwell(t0 + crate::tui::home::UNREAD_DWELL + Duration::from_secs(1));
    assert!(cleared, "revisiting and dwelling should clear the mark");
    assert!(
        !env.view.get_instance(&a).unwrap().is_unread(),
        "a hand-flagged row clears on revisit + dwell (per-visit hold)"
    );
}

/// Engaging with a hand-flagged row (open/attach, which clears it) also drops
/// the per-visit hold, so a *later* auto mark on that same still-selected row
/// is not wrongly suppressed and clears on dwell.
#[test]
#[serial]
fn manual_hold_released_on_engagement_lets_auto_clear() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Idle;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);

    // Hand-flag, then engage (the open/attach path), which clears it and ends
    // the hold even though the cursor never left the row.
    env.view.toggle_unread_at_cursor().expect("manual mark");
    env.view.clear_unread_on_view(&id);
    assert!(
        env.view.manual_unread_hold.is_none(),
        "engaging with the row must release the manual hold"
    );
    assert!(!env.view.get_instance(&id).unwrap().is_unread());

    // A later auto mark on the same (still-selected) row must clear on dwell.
    env.view.mutate_instance(&id, |inst| inst.mark_unread());
    let t0 = Instant::now();
    assert!(!env.view.tick_unread_dwell(t0));
    let cleared = env
        .view
        .tick_unread_dwell(t0 + crate::tui::home::UNREAD_DWELL + Duration::from_secs(1));
    assert!(
        cleared && !env.view.get_instance(&id).unwrap().is_unread(),
        "a later auto mark must not be suppressed by a stale hold"
    );
}

/// Moving the selection to a different row before the dwell completes spares
/// the first row: arrowing through a list doesn't read everything you pass.
#[test]
#[serial]
fn unread_dwell_resets_on_selection_change() {
    use std::time::{Duration, Instant};
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_with_sessions(2);
    let a = env.view.instance_at(0).id.clone();
    let b = env.view.instance_at(1).id.clone();
    for id in [&a, &b] {
        env.view.mutate_instance(id, |inst| {
            inst.status = crate::session::Status::Idle;
            inst.mark_unread();
        });
    }
    env.view.flat_items = env.view.build_flat_items();

    let t0 = Instant::now();
    // Arm the dwell clock on A.
    env.view.select_session_by_id(&a);
    assert!(!env.view.tick_unread_dwell(t0));
    // Move to B well before A's threshold; A's clock is dropped, B's arms.
    env.view.select_session_by_id(&b);
    assert!(!env.view.tick_unread_dwell(t0 + Duration::from_millis(500)));
    // Long after, B has now dwelled past the threshold and clears; A, which we
    // left early, is untouched.
    assert!(env
        .view
        .tick_unread_dwell(t0 + crate::tui::home::UNREAD_DWELL + Duration::from_secs(2)));
    assert!(
        env.view.get_instance(&a).unwrap().is_unread(),
        "row left before the threshold must stay unread"
    );
    assert!(
        !env.view.get_instance(&b).unwrap().is_unread(),
        "row dwelled past the threshold must be cleared"
    );
}

#[test]
#[serial]
fn test_q_returns_quit_action() {
    let mut env = create_test_env_empty();
    let action = env.view.handle_key(key(KeyCode::Char('q')), None);
    assert_eq!(action, Some(Action::Quit));
}

#[test]
#[serial]
fn test_ctrl_q_does_not_quit_home() {
    // #1569: Ctrl+Q is a live-mode-exit habit; on the home view it must
    // not quit aoe. (The app-level handler swallows it; the home view
    // itself must also never treat it as a quit.)
    let mut env = create_test_env_empty();
    let action = env.view.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        None,
    );
    assert_eq!(action, None);
}

#[test]
#[serial]
fn test_quit_confirm_dont_ask_again_persists_opt_out() {
    let mut env = create_test_env_empty();
    env.view.confirm_before_quit = true;

    env.view.show_quit_confirm();
    assert!(env.view.confirm_dialog.is_some());

    // Tick "don't warn me again", then confirm.
    env.view.handle_key(key(KeyCode::Char(' ')), None);
    let action = env.view.handle_key(key(KeyCode::Char('y')), None);

    assert_eq!(action, Some(Action::Quit));
    assert!(!env.view.confirm_before_quit);
    // The opt-out is persisted so it survives a restart.
    let saved = crate::session::config::load_config()
        .unwrap()
        .expect("config should have been written");
    assert!(!saved.session.confirm_before_quit);
}

#[test]
#[serial]
fn test_quit_confirm_without_opt_out_keeps_flag() {
    let mut env = create_test_env_empty();
    env.view.confirm_before_quit = true;

    env.view.show_quit_confirm();
    // Confirm without ticking the checkbox.
    let action = env.view.handle_key(key(KeyCode::Char('y')), None);

    assert_eq!(action, Some(Action::Quit));
    assert!(env.view.confirm_before_quit);
}

#[test]
#[serial]
fn test_question_mark_opens_help() {
    let mut env = create_test_env_empty();
    assert!(!env.view.show_help);
    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_esc() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_question_mark() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_q() {
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('q')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_help_closes_on_uppercase_q_for_strict_mode() {
    // Strict mode binds quit to uppercase Q; the help overlay must
    // accept it too so strict-mode users can dismiss the dialog with
    // the same key they use to quit.
    let mut env = create_test_env_empty();
    env.view.show_help = true;
    env.view.handle_key(key(KeyCode::Char('Q')), None);
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_help() {
    let mut env = create_test_env_empty();
    assert!(!env.view.has_dialog());
    env.view.show_help = true;
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_n_opens_new_dialog() {
    let mut env = create_test_env_empty();
    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_new_dialog() {
    let mut env = create_test_env_empty();
    env.view.new_dialog = Some(NewSessionDialog::new(
        AvailableTools::with_tools(&["claude"]),
        Vec::new(),
        "default",
        vec!["default".to_string()],
    ));
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_b_opens_project_session_picker_when_projects_exist() {
    use crate::session::projects::{self, Project, ProjectScope};
    let mut env = create_test_env_empty();
    let repo = env._temp.path().join("repoA");
    std::fs::create_dir_all(&repo).unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("repoA", repo.to_string_lossy(), ProjectScope::Profile),
        false,
    )
    .unwrap();

    assert!(env.view.project_session_picker_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_some());
    assert!(env.view.info_dialog.is_none());
    // The picker captures filter chars, so it must register as a modal: an
    // unregistered picker lets the global `q` shortcut quit the app and the
    // paste-burst detector fire mid-filter (text gets stranded in handle_paste).
    assert!(env.view.has_dialog());
    assert!(!env.view.wants_paste_burst());
}

#[test]
#[serial]
fn test_b_opens_project_add_flow_when_no_projects() {
    let mut env = create_test_env_empty();
    let project_dir = env._temp.path().join("plain-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let profile = env.view.config_profile();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_none());
    assert!(env.view.info_dialog.is_none());
    assert!(env.view.projects_dialog.is_some());

    for ch in project_dir.to_string_lossy().chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    let canonical = project_dir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let projects = crate::session::projects::load_merged(&profile).unwrap();
    assert!(
        projects.iter().any(|p| p.path == canonical),
        "b empty-state add flow should register the typed path"
    );
}

#[test]
#[serial]
fn test_b_empty_project_add_flow_escape_closes_dialog() {
    let mut env = create_test_env_empty();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.projects_dialog.is_some());

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.projects_dialog.is_none());
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn test_b_submit_opens_new_dialog_with_prefilled_path() {
    use crate::session::projects::{self, Project, ProjectScope};
    let mut env = create_test_env_empty();
    let repo = env._temp.path().join("repoB");
    std::fs::create_dir_all(&repo).unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("repoB", repo.to_string_lossy(), ProjectScope::Profile),
        false,
    )
    .unwrap();
    let expected = projects::load_merged("test").unwrap()[0].path.clone();

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(env.view.project_session_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.project_session_picker_dialog.is_none());
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("new session dialog should open after picking a project");
    assert_eq!(dialog.path_value(), expected);
}

#[test]
#[serial]
fn test_cursor_down_j() {
    let mut env = create_test_env_with_sessions(5);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Char('j')), None);
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn test_cursor_down_arrow() {
    let mut env = create_test_env_with_sessions(5);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn test_cursor_up_k() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn test_cursor_up_arrow() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn test_cursor_bounds_at_top() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_cursor_bounds_at_bottom() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 4;
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_page_down() {
    let mut env = create_test_env_with_sessions(20);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::PageDown), None);
    assert_eq!(env.view.cursor, 10);
}

#[test]
#[serial]
fn test_page_up() {
    let mut env = create_test_env_with_sessions(20);
    env.view.cursor = 15;
    env.view.handle_key(key(KeyCode::PageUp), None);
    assert_eq!(env.view.cursor, 5);
}

#[test]
#[serial]
fn test_page_down_clamps_to_end() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0;
    env.view.handle_key(key(KeyCode::PageDown), None);
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_page_up_clamps_to_start() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::PageUp), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_home_key() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 7;
    env.view.handle_key(key(KeyCode::Home), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_end_key() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::End), None);
    assert_eq!(env.view.cursor, 9);
}

#[test]
#[serial]
fn test_g_key_opens_group_picker() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_with_sessions(3);
    env.view.group_by = GroupByMode::Manual;

    // 'g' opens the picker without changing the current mode.
    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(env.view.group_picker_dialog.is_some());
    assert_eq!(env.view.group_by, GroupByMode::Manual);

    // Down + Enter selects the next option (Project).
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.group_picker_dialog.is_none());
    assert_eq!(env.view.group_by, GroupByMode::Project);

    // 'g' again, Esc cancels without changing mode.
    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(env.view.group_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.group_picker_dialog.is_none());
    assert_eq!(env.view.group_by, GroupByMode::Project);

    // 'g' again, Down + Enter advances Project -> Org.
    env.view.handle_key(key(KeyCode::Char('g')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.group_picker_dialog.is_none());
    assert_eq!(env.view.group_by, GroupByMode::Org);
}

#[test]
#[serial]
fn test_uppercase_g_goes_to_end() {
    let mut env = create_test_env_with_sessions(10);
    env.view.cursor = 3;
    env.view.handle_key(key(KeyCode::Char('G')), None);
    assert_eq!(env.view.cursor, 9);
}

#[test]
#[serial]
fn test_cursor_movement_on_empty_list() {
    let mut env = create_test_env_empty();
    env.view.handle_key(key(KeyCode::Down), None);
    assert_eq!(env.view.cursor, 0);
    env.view.handle_key(key(KeyCode::Up), None);
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_enter_on_session_returns_attach_action() {
    let mut env = create_test_env_with_sessions(3);
    env.view.cursor = 1;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachSession(_))));
}

#[test]
#[serial]
fn test_enter_on_acp_session_opens_structured_view() {
    use crate::session::config::GroupByMode;
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut instances = vec![
        Instance::new("plain", "/tmp/0"),
        Instance::new("acp", "/tmp/1"),
        Instance::new("plain2", "/tmp/2"),
    ];
    instances[1].view = crate::session::View::Structured;
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.cursor = 1;
    view.update_selected();

    let action = view.handle_key(key(KeyCode::Enter), None);
    match action {
        Some(Action::OpenStructuredView(id)) => {
            // Should target the structured view instance, not the plain ones.
            assert!(
                id.contains("acp") || !id.is_empty(),
                "OpenStructuredView carried an empty session id"
            );
        }
        other => {
            panic!("expected Action::OpenStructuredView for structured view session, got {other:?}")
        }
    }
}

#[test]
#[serial]
fn test_slash_enters_search_mode() {
    let mut env = create_test_env_with_sessions(3);
    assert!(!env.view.search_active);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    assert!(env.view.search_active);
    assert!(env.view.search_query.value().is_empty());
}

#[test]
#[serial]
fn test_search_mode_captures_chars() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('t')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(env.view.search_query.value(), "test");
}

#[test]
#[serial]
fn test_search_mode_backspace() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('a')), None);
    env.view.handle_key(key(KeyCode::Char('b')), None);
    env.view.handle_key(key(KeyCode::Backspace), None);
    assert_eq!(env.view.search_query.value(), "a");
}
