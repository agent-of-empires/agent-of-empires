/// Tests for `default_attach_mode`, which decides whether Enter or a double-click on an
/// existing Structured-view row attaches to tmux or enters live-send.
use super::*;
use crate::session::config::{update_config, AttachMode};

fn add_session(view: &mut HomeView, title: &str) -> String {
    let mut inst = Instance::new(title, "/tmp/test");
    inst.source_profile = "test".to_string();
    let id = inst.id.clone();
    view.add_instance(inst);
    id
}

fn write_global_default_attach_mode(mode: AttachMode) {
    update_config(|config| {
        config.session.default_attach_mode = mode;
    })
    .unwrap();
}

/// Enter (and double-click) keep tmux attach by default so upgrades don't change muscle
/// memory; LiveSend moves Enter to live mode in every view and Tab becomes the tmux escape
/// hatch. Structured rows ignore the setting. The help overlay's Enter label follows.
#[test]
#[serial]
fn enter_and_tab_route_by_default_attach_mode() {
    use crate::tui::home::ViewMode;
    #[derive(Debug)]
    enum Expect {
        Attach,
        Live,
        AttachTerminal,
        Structured,
    }
    let cases = [
        (None, ViewMode::Structured, KeyCode::Enter, Expect::Attach),
        (
            Some(AttachMode::LiveSend),
            ViewMode::Structured,
            KeyCode::Enter,
            Expect::Live,
        ),
        (
            Some(AttachMode::LiveSend),
            ViewMode::Terminal,
            KeyCode::Enter,
            Expect::Live,
        ),
        (
            None,
            ViewMode::Terminal,
            KeyCode::Enter,
            Expect::AttachTerminal,
        ),
        (
            Some(AttachMode::LiveSend),
            ViewMode::Structured,
            KeyCode::Tab,
            Expect::Attach,
        ),
        (
            Some(AttachMode::LiveSend),
            ViewMode::Structured,
            KeyCode::Enter,
            Expect::Structured,
        ),
    ];
    for (mode, view_mode, code, expect) in cases {
        let mut env = create_test_env_empty();
        if let Some(mode) = mode {
            write_global_default_attach_mode(mode);
        }
        let id = add_session(&mut env.view, "session-one");
        let structured = matches!(expect, Expect::Structured);
        if structured {
            env.view.mutate_instance(&id, |inst| {
                inst.view = crate::session::View::Structured;
            });
        }
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        if !structured {
            assert_eq!(
                env.view.default_attach_mode(&id),
                Some(mode.unwrap_or(AttachMode::Tmux))
            );
            assert_eq!(
                env.view.help_live_on_enter(),
                Some(mode == Some(AttachMode::LiveSend))
            );
        }
        env.view.view_mode = view_mode.clone();
        let action = env.view.handle_key(key(code), None);
        let ok = match expect {
            Expect::Attach => action == Some(Action::AttachSession(id.clone())),
            Expect::Live => action == Some(Action::EnterLiveSend(id.clone())),
            Expect::AttachTerminal => {
                matches!(&action, Some(Action::AttachTerminal(returned, _)) if returned == &id)
            }
            Expect::Structured => {
                matches!(&action, Some(Action::OpenStructuredView(returned)) if returned == &id)
            }
        };
        assert!(
            ok,
            "{mode:?} {view_mode:?} {code:?}: expected {expect:?}, got {action:?}"
        );
    }
}

#[test]
#[serial]
fn tab_in_terminal_view_swaps_to_attach_terminal_when_default_is_live_send() {
    // Terminal-view counterpart of the swap: with Enter pinned to live-send, Tab attaches
    // the paired terminal pane rather than the agent pane.
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let action = env.view.handle_key(key(KeyCode::Tab), None);
    assert!(
        matches!(&action, Some(Action::AttachTerminal(returned_id, _)) if returned_id == &id),
        "Tab in Terminal view with LiveSend default must AttachTerminal, got {:?}",
        action
    );
}

#[test]
#[serial]
fn m_in_terminal_view_targets_terminal_pane() {
    // #1554: 'm' from Terminal view opened a compose dialog targeting the agent pane,
    // sending shell commands into the agent's input box. `pending_send_target` now reflects
    // view_mode at open time, so `execute_send_message` routes to the terminal pane.
    let mut env = create_test_env_empty();
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let _ = env.view.handle_key(key(KeyCode::Char('m')), None);
    assert!(
        env.view.send_message_dialog.is_some(),
        "Terminal view 'm' must open the compose dialog even when \
         the paired tmux pane hasn't spawned yet"
    );
    assert_eq!(
        env.view.pending_send_target,
        crate::tui::home::live_send::LiveSendTarget::Terminal,
        "compose dialog opened from Terminal view must target the terminal pane"
    );
}

/// `start_live_send` stages the pane the preview shows, so `prepare_live_send` dispatches
/// keystrokes to the paired terminal or the named tool rather than the agent.
#[test]
#[serial]
fn start_live_send_targets_the_previewed_pane() {
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::ViewMode;
    let cases = [
        (ViewMode::Terminal, LiveSendTarget::Terminal, false),
        (
            ViewMode::Tool("lazygit".to_string()),
            LiveSendTarget::Tool("lazygit".to_string()),
            true,
        ),
    ];
    for (view_mode, target, check_action) in cases {
        let mut env = create_test_env_empty();
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.view_mode = view_mode;
        let action = env.view.start_live_send();
        if check_action {
            assert_eq!(action, Some(Action::EnterLiveSend(id)));
        }
        assert_eq!(env.view.pending_live_send_target, target);
    }
}

#[test]
#[serial]
fn refresh_tool_preview_cache_resizes_live_pane_when_targeted() {
    // `refresh_tool_preview_cache_if_needed` must call `resize_live_pane_if_target` up
    // front like its Terminal siblings, so a resize while live-sent to a tool pane reflows
    // it instead of waiting for a re-enter. The dedup recorded in `live_send_last_resize`
    // is the observable signal without a spawned worker.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    let inst = env.view.get_instance(&id).unwrap().clone();
    let tmux_name = crate::tmux::ToolSession::new(&inst.id, &inst.title, "lazygit")
        .session_name()
        .to_string();
    env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
        session_id: id.clone(),
        title: inst.title.clone(),
        tmux_name,
        target: crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string()),
        exit_chords: Vec::new(),
        leader: None,
    });
    env.view.selected_session = Some(id);
    assert_eq!(env.view.live_send_last_resize, None);

    env.view
        .refresh_tool_preview_cache_if_needed(80, 24, "lazygit");

    assert_eq!(
        env.view.live_send_last_resize,
        Some((80, 24)),
        "resize_live_pane_if_target must fire for a targeted Tool pane"
    );
}

fn write_live_send_on_view_switch(mode: AttachMode, on_view_switch: bool) {
    update_config(|config| {
        config.session.default_attach_mode = mode;
        config.session.live_send_on_view_switch = on_view_switch;
    })
    .unwrap();
}

/// `live_send_on_view_switch` is the only gate for entering live-send on an explicit view
/// switch ('t' or a tool hotkey), whatever `default_attach_mode` says; off by default.
#[test]
#[serial]
fn view_switch_auto_starts_live_send_only_when_enabled() {
    use crate::tui::home::ViewMode;
    let tool_key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT);
    let cases = [
        (
            AttachMode::LiveSend,
            true,
            key(KeyCode::Char('t')),
            ViewMode::Terminal,
            true,
        ),
        (
            AttachMode::LiveSend,
            false,
            key(KeyCode::Char('t')),
            ViewMode::Terminal,
            false,
        ),
        (
            AttachMode::Tmux,
            true,
            key(KeyCode::Char('t')),
            ViewMode::Terminal,
            true,
        ),
        (
            AttachMode::LiveSend,
            true,
            tool_key,
            ViewMode::Tool("lazygit".to_string()),
            true,
        ),
    ];
    for (mode, on_switch, key_event, expected_view, live) in cases {
        let mut env = create_test_env_empty();
        write_live_send_on_view_switch(mode, on_switch);
        let id = add_session(&mut env.view, "session-one");
        env.view.flat_items = env.view.build_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        env.view.tool_hotkey_cache =
            vec![("lazygit".to_string(), KeyCode::Char('g'), KeyModifiers::ALT)];
        let action = env.view.handle_key(key_event, None);
        assert_eq!(env.view.view_mode, expected_view, "{mode:?} {on_switch}");
        if live {
            assert_eq!(
                action,
                Some(Action::EnterLiveSend(id)),
                "{mode:?} {on_switch}"
            );
        } else {
            assert_eq!(action, None, "{mode:?} {on_switch}");
            assert!(env.view.live_send.is_none());
        }
    }
}

#[test]
#[serial]
fn profile_default_attach_mode_cache_refreshes_with_config() {
    // The render path falls back to `profile_default_attach_mode` with no selection, so the
    // cache must track the saved config without re-reading from disk per paint: saving a
    // mode plus `refresh_from_config` updates it.
    let mut env = create_test_env_empty();
    // No selected row: the help overlay defers to this cached default.
    assert_eq!(env.view.help_live_on_enter(), None);
    assert_eq!(
        env.view.profile_default_attach_mode,
        AttachMode::Tmux,
        "cache should initialize to the historical Tmux default"
    );
    write_global_default_attach_mode(AttachMode::LiveSend);
    env.view
        .refresh_from_config(ConfigRefreshOrigin::Interactive);
    assert_eq!(
        env.view.profile_default_attach_mode,
        AttachMode::LiveSend,
        "refresh_from_config must pick up the saved LiveSend default"
    );
}

/// Render the whole home screen into a string for placeholder /
/// badge assertions.
fn render_home(env: &mut TestEnv) -> String {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let theme = load_theme("empire");
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
}

fn structured_session_env() -> (TestEnv, String) {
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    (env, id)
}

/// A selected structured session has no agent tmux pane, so the preview shows the
/// explanatory placeholder; in the Terminal layout the `[structured]` badge marks the row,
/// so Enter opening the structured view is never a surprise.
#[test]
#[serial]
fn structured_session_renders_placeholder_and_terminal_badge() {
    let (mut env, _id) = structured_session_env();
    let screen = render_home(&mut env);
    assert!(
        screen.contains("Structured view"),
        "placeholder heading missing:\n{screen}"
    );
    assert!(
        screen.contains("structured transcript"),
        "placeholder body missing:\n{screen}"
    );
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let screen = render_home(&mut env);
    assert!(
        screen.contains("[structured]"),
        "badge missing in Terminal view mode:\n{screen}"
    );
}

/// The switch-view context entry offers the opposite view: terminal for a structured row,
/// structured for an ACP-capable terminal row when the opt-in is on, and nothing for rows
/// mid-lifecycle. Accepting the switch confirm dispatches it for the stashed id.
#[test]
#[serial]
fn switch_view_target_gates_by_view_and_state() {
    use crate::session::config::update_config;
    let (mut env, id) = structured_session_env();
    env.view.prompt_switch_view_for_selected();
    assert!(
        env.view.confirm_dialog.is_some(),
        "switch must confirm first (history is destroyed)"
    );
    let action = env.view.dispatch_confirm_submit("switch_view");
    assert!(
        matches!(action, Some(Action::SwitchSessionView(ref sid)) if *sid == id),
        "expected SwitchSessionView({id}), got {action:?}"
    );
    update_config(|config| {
        config.acp.offer_structured_in_new_session = true;
    })
    .unwrap();
    assert_eq!(env.view.session_switch_view_target(&id), Some(true));
    // Terminal row with an ACP-capable tool (claude): offer structured.
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Terminal;
    });
    assert_eq!(env.view.session_switch_view_target(&id), Some(false));
    // Mid-lifecycle rows are excluded.
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Creating;
    });
    assert_eq!(env.view.session_switch_view_target(&id), None);
}

/// Switching a terminal session into structured view is gated on the
/// `offer_structured_in_new_session` opt-in, so with it off an ACP-capable row offers no
/// switch. A structured row can always switch back, so no session is stranded.
#[test]
#[serial]
fn switch_view_target_gated_on_structured_opt_in() {
    use crate::session::config::update_config;
    let (mut env, id) = structured_session_env();
    update_config(|config| {
        config.acp.offer_structured_in_new_session = false;
    })
    .unwrap();
    // Structured -> terminal is always available (escape hatch).
    assert_eq!(env.view.session_switch_view_target(&id), Some(true));
    // Terminal -> structured is suppressed while the opt-in is off.
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Terminal;
    });
    assert_eq!(env.view.session_switch_view_target(&id), None);
}

/// Tab is Enter's complement on a session row: whichever of live-send and tmux-attach
/// `default_attach_mode` doesn't route Enter to. The footer must surface it so it isn't
/// discoverable only from the source or the `?` overlay.
#[test]
#[serial]
fn footer_advertises_tab_as_live_when_default_is_tmux() {
    let mut env = create_test_env_empty();
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let out = render_home(&mut env);
    assert!(
        out.contains("↵  Attach"),
        "Enter hint should stay tmux attach under the default mode.\n{out}"
    );
    assert!(
        out.contains("⇥  Live"),
        "Tab hint should advertise Live mode when Enter is pinned to tmux attach.\n{out}"
    );
}

/// Inverse: once `default_attach_mode = LiveSend` takes over Enter, the two hints swap
/// rather than both claiming "Attach", the same swap the `?` overlay does.
#[test]
#[serial]
fn footer_advertises_tab_as_attach_when_default_is_live_send() {
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let out = render_home(&mut env);
    assert!(
        out.contains("↵  Live"),
        "Enter hint should say Live once it owns live-send.\n{out}"
    );
    assert!(
        out.contains("⇥  Attach"),
        "Tab hint should offer the tmux escape hatch once Enter owns live-send.\n{out}"
    );
}

/// Acp rows ignore `default_attach_mode` entirely (Tab mirrors Enter or no-ops), so the
/// footer must not advertise a Tab complement that does nothing different.
#[test]
#[serial]
fn footer_hides_tab_hint_for_structured_sessions() {
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let out = render_home(&mut env);
    assert!(
        !out.contains("⇥"),
        "structured view rows must not show a Tab hint at all.\n{out}"
    );
    assert!(
        out.contains("↵  Attach"),
        "structured rows keep the plain Enter attach label.\n{out}"
    );
}

#[test]
#[serial]
fn send_message_opens_structured_view() {
    let (mut env, id) = structured_session_env();
    let action = env.view.handle_key(key(KeyCode::Char('m')), None);
    assert!(
        matches!(&action, Some(Action::OpenStructuredView(returned_id)) if returned_id == &id),
        "m must open the structured composer for the selected session, got {action:?}"
    );
}

/// 'm' on a structured session drains buffered paste into
/// `pending_paste_for_structured_view`, so the async open path forwards it into the composer
/// instead of losing it.
#[test]
#[serial]
fn send_message_drains_pending_paste_for_structured_view() {
    let (mut env, id) = structured_session_env();
    env.view.pending_paste = Some("cached text".to_string());
    env.view.handle_key(key(KeyCode::Char('m')), None);
    assert_eq!(
        env.view.pending_paste, None,
        "pending_paste must be drained when routing to structured view"
    );
    assert_eq!(
        env.view.pending_paste_for_structured_view.get(&id),
        Some(&"cached text".to_string()),
        "drained text must land in pending_paste_for_structured_view, bound to the selected session"
    );
}

/// A second buffered paste for the same structured session appends instead of replacing:
/// the earlier paste belongs to a failed activation still waiting to drain.
#[test]
#[serial]
fn send_message_merges_buffered_paste_for_same_session() {
    let (mut env, id) = structured_session_env();
    env.view.pending_paste = Some("first ".to_string());
    env.view.handle_key(key(KeyCode::Char('m')), None);
    env.view.pending_paste = Some("second".to_string());
    env.view.handle_key(key(KeyCode::Char('m')), None);
    assert_eq!(
        env.view.pending_paste_for_structured_view.get(&id),
        Some(&"first second".to_string()),
        "same-target paste must merge into the buffered text"
    );
}

/// A paste captured for another structured session gets its own entry, so the earlier
/// target's unsent draft survives and neither session's draft leaks into the other.
#[test]
#[serial]
fn send_message_keeps_buffered_paste_per_session() {
    let (mut env, id_a) = structured_session_env();
    env.view.pending_paste = Some("session a draft".to_string());
    env.view.handle_key(key(KeyCode::Char('m')), None);
    let other = add_session(&mut env.view, "acp-two");
    env.view.mutate_instance(&other, |inst| {
        inst.view = crate::session::View::Structured;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.pending_paste = Some("session b draft".to_string());
    // Select the other structured session and press 'm' again.
    env.view.select_session_by_id(&other);
    env.view.handle_key(key(KeyCode::Char('m')), None);
    assert_eq!(
        env.view.pending_paste_for_structured_view.get(&other),
        Some(&"session b draft".to_string()),
        "the new target owns its own draft"
    );
    assert_eq!(
        env.view.pending_paste_for_structured_view.get(&id_a),
        Some(&"session a draft".to_string()),
        "the previous target's unsent draft must survive the switch"
    );
}
