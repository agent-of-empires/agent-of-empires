/// Tests for the `default_attach_mode` setting that drives whether
/// pressing Enter (or double-clicking) on an existing session row in
/// Structured view attaches to tmux or enters live-send mode.
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

#[test]
#[serial]
fn defaults_to_tmux_when_no_config_present() {
    // Default Enter / double-click stays on AttachSession; flipping
    // it to LiveSend silently changes every existing user's muscle
    // memory on upgrade.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    let mode = env.view.default_attach_mode(&id);
    assert_eq!(mode, Some(AttachMode::Tmux));
}

#[test]
#[serial]
fn enter_emits_attach_session_when_default_is_tmux() {
    // Sanity: with the historical Tmux default, Enter on a session
    // row produces Action::AttachSession.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.activate_selected_session();
    assert_eq!(action, Some(Action::AttachSession(id)));
}

#[test]
#[serial]
fn enter_emits_enter_live_send_when_default_is_live_send() {
    // User opted into "Enter = live mode": activating an Agent-view
    // row must dispatch Action::EnterLiveSend instead of AttachSession.
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.activate_selected_session();
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
}

#[test]
#[serial]
fn terminal_view_honors_default_attach_mode_live_send() {
    // The `default_attach_mode = LiveSend` setting applies to
    // Terminal view too: pressing Enter on a terminal-view row
    // dispatches `Action::EnterLiveSend` against the paired
    // terminal pane (the live-send target resolution happens in
    // `start_live_send` based on view_mode). Without this, the
    // user's "Enter = live mode" preference would silently flip
    // back to a full tmux attach whenever they were previewing a
    // terminal.
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let action = env.view.activate_selected_session();
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
}

#[test]
#[serial]
fn terminal_view_falls_back_to_attach_when_default_is_tmux() {
    // Inverse of the LiveSend case: with the historical Tmux
    // default, Enter on a terminal-view row keeps the historical
    // `Action::AttachTerminal` so users who haven't opted into
    // live mode see no change.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let action = env.view.activate_selected_session();
    assert!(
        matches!(&action, Some(Action::AttachTerminal(returned_id, _)) if returned_id == &id),
        "default Tmux mode must keep Terminal view on AttachTerminal, got {:?}",
        action
    );
}

#[test]
#[serial]
fn tab_swaps_to_attach_session_when_default_is_live_send() {
    // When `default_attach_mode = LiveSend`, Enter takes over the
    // live-send slot, so Tab swaps to a full tmux attach (the
    // escape hatch). Without this, the user would have no
    // single-key path to the underlying tmux session.
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Tab), None);
    assert_eq!(action, Some(Action::AttachSession(id)));
}

#[test]
#[serial]
fn tab_still_enters_live_send_when_default_is_tmux() {
    // With the historical Tmux default, Enter still attaches and
    // Tab keeps its historical live-send role.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Tab), None);
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
}

#[test]
#[serial]
fn tab_in_terminal_view_swaps_to_attach_terminal_when_default_is_live_send() {
    // Terminal-view counterpart of the swap: with Enter pinned to
    // live-send, Tab in Terminal view attaches the paired terminal
    // pane rather than the agent pane.
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
    // The 'm' bug from #1554: pressing 'm' from Terminal view used
    // to open a compose dialog that targeted the agent pane,
    // sending commands meant for the shell into the agent's input
    // box. The fix: `pending_send_target` reflects view_mode at
    // dialog open time so `execute_send_message` routes to the
    // paired terminal pane.
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

#[test]
#[serial]
fn start_live_send_in_terminal_view_targets_terminal_pane() {
    // Direct check on the live-send target resolution: in Terminal
    // view, `start_live_send` stages the host terminal as the
    // pending target so `prepare_live_send` will dispatch
    // keystrokes to the paired terminal tmux pane.
    let mut env = create_test_env_empty();
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let _ = env.view.start_live_send();
    assert_eq!(
        env.view.pending_live_send_target,
        crate::tui::home::live_send::LiveSendTarget::Terminal
    );
}

#[test]
#[serial]
fn refresh_tool_preview_cache_resizes_live_pane_when_targeted() {
    // Reviewer-requested fix (CodeRabbit + Seluj78 on #2777):
    // `refresh_tool_preview_cache_if_needed` must call
    // `resize_live_pane_if_target` up front, the same as the
    // Terminal/ContainerTerminal siblings, so a window resize while
    // live-sent to a Tool pane (lazygit, yazi) reflows it instead of
    // waiting for a live-mode re-enter. `resize_live_pane_if_target`
    // records the dedup in `live_send_last_resize` even without a
    // spawned worker, so that's the observable signal here.
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

#[test]
#[serial]
fn start_live_send_in_tool_view_targets_tool_pane() {
    // Tool-view counterpart of `start_live_send_in_terminal_view_targets_terminal_pane`:
    // when previewing a named tool (lazygit, yazi, etc.), `start_live_send`
    // must resolve to that tool's own paired pane, not fall back to the
    // agent or bail out entirely.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.view_mode = crate::tui::home::ViewMode::Tool("lazygit".to_string());
    let action = env.view.start_live_send();
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
    assert_eq!(
        env.view.pending_live_send_target,
        crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string())
    );
}

fn write_live_send_on_view_switch(mode: AttachMode, on_view_switch: bool) {
    update_config(|config| {
        config.session.default_attach_mode = mode;
        config.session.live_send_on_view_switch = on_view_switch;
    })
    .unwrap();
}

#[test]
#[serial]
fn toggle_view_auto_starts_live_send_when_setting_enabled_and_default_is_live_send() {
    // With `live_send_on_view_switch` on and `default_attach_mode =
    // LiveSend`, pressing 't' (ToggleView) from Structured view must
    // not just flip the preview to Terminal; it must also enter
    // live-send immediately, without a separate Enter/Tab/click.
    let mut env = create_test_env_empty();
    write_live_send_on_view_switch(AttachMode::LiveSend, true);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(
        env.view.view_mode,
        crate::tui::home::ViewMode::Terminal,
        "ToggleView must still flip the preview to Terminal"
    );
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
}

#[test]
#[serial]
fn toggle_view_does_not_auto_start_live_send_when_setting_disabled() {
    // The setting defaults to off: even with `default_attach_mode =
    // LiveSend`, ToggleView must leave live-send alone and only
    // change the preview.
    let mut env = create_test_env_empty();
    write_live_send_on_view_switch(AttachMode::LiveSend, false);
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(env.view.view_mode, crate::tui::home::ViewMode::Terminal);
    assert_eq!(
        action, None,
        "auto live-send must stay off when the setting is disabled"
    );
    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn toggle_view_auto_starts_live_send_regardless_of_default_attach_mode() {
    // The setting is the only gate: with the historical Tmux
    // default, ToggleView still auto-enters live-send when
    // `live_send_on_view_switch` is enabled.
    let mut env = create_test_env_empty();
    write_live_send_on_view_switch(AttachMode::Tmux, true);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(env.view.view_mode, crate::tui::home::ViewMode::Terminal);
    assert_eq!(
        action,
        Some(Action::EnterLiveSend(id)),
        "auto live-send must fire even when default_attach_mode is Tmux"
    );
}

#[test]
#[serial]
fn tool_hotkey_auto_starts_live_send_when_setting_enabled_and_default_is_live_send() {
    // Parallel case for the other explicit view-switch entry point:
    // opening a tool via its configured hotkey must apply the same
    // auto-entry check as ToggleView.
    let mut env = create_test_env_empty();
    write_live_send_on_view_switch(AttachMode::LiveSend, true);
    let id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.tool_hotkey_cache =
        vec![("lazygit".to_string(), KeyCode::Char('g'), KeyModifiers::ALT)];
    let action = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT), None);
    assert_eq!(
        env.view.view_mode,
        crate::tui::home::ViewMode::Tool("lazygit".to_string())
    );
    assert_eq!(action, Some(Action::EnterLiveSend(id)));
}

#[test]
#[serial]
fn help_live_on_enter_returns_none_when_no_session_selected() {
    // Cursor parked off any session row: the help overlay shouldn't
    // claim a session-attach behavior, so `help_live_on_enter`
    // signals "no row" with None and the render path falls back to
    // the cached profile default.
    let env = create_test_env_empty();
    assert!(
        env.view.selected_session.is_none(),
        "fresh empty view should have no session selected"
    );
    assert_eq!(env.view.help_live_on_enter(), None);
}

#[test]
#[serial]
fn help_live_on_enter_returns_some_for_selected_session() {
    // With the historical Tmux default, a selected session row maps
    // to Some(false): Enter goes to tmux attach, Tab to live mode.
    let mut env = create_test_env_empty();
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    assert_eq!(env.view.help_live_on_enter(), Some(false));
}

#[test]
#[serial]
fn help_live_on_enter_reflects_live_send_setting() {
    // Flipping the user's default to LiveSend must propagate to
    // help_live_on_enter so the help overlay relabels Enter as
    // live mode and Tab as tmux attach.
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    assert_eq!(env.view.help_live_on_enter(), Some(true));
}

#[test]
#[serial]
fn profile_default_attach_mode_cache_refreshes_with_config() {
    // The render path falls back to `profile_default_attach_mode`
    // when no session is selected, so it has to track the saved
    // config without re-reading from disk per paint. Saving a new
    // mode + calling `refresh_from_config` must update the cache.
    let mut env = create_test_env_empty();
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

/// Acp sessions short-circuit before the setting is consulted
/// (the structured view branch in `activate_selected_session` returns
/// `OpenStructuredView`/transient-status before we get to the view-mode
/// match), so the resolver also returns None for them; the setting
/// must not be able to misroute a structured view row into live mode.
#[test]
#[serial]
fn acp_session_ignores_default_attach_mode() {
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let action = env.view.activate_selected_session();
    assert!(
        matches!(&action, Some(Action::OpenStructuredView(returned_id)) if returned_id == &id),
        "structured view rows must route to OpenStructuredView regardless of default_attach_mode, got {:?}",
        action
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

/// A selected structured session has no agent tmux pane; the preview
/// must show the explanatory placeholder instead of a blank capture.
#[test]
#[serial]
fn structured_session_preview_shows_placeholder() {
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
}

/// The switch-view context entry offers the opposite view: terminal for
/// a structured row, structured for a terminal row whose tool is
/// ACP-capable (only when the structured-view opt-in is on), and nothing
/// for rows mid-lifecycle.
#[test]
#[serial]
fn switch_view_target_gates_by_view_and_state() {
    use crate::session::config::update_config;
    let (mut env, id) = structured_session_env();
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

/// Switching a terminal session INTO the structured view is gated on the
/// `offer_structured_in_new_session` opt-in, so with it off an ACP-capable
/// terminal row offers no switch. A structured row can always switch back
/// to terminal regardless, so a session is never stranded.
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

/// Accepting the switch-view confirm emits the action with the stashed
/// session id, mirroring the other confirm-carrying actions.
#[test]
#[serial]
fn switch_view_confirm_dispatches_action_with_stashed_id() {
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
}

/// The `[structured]` badge marks structured rows in the Terminal home
/// layout too (non-sandboxed rows have no container/host badge there),
/// so Enter opening the structured view is never a surprise.
#[test]
#[serial]
fn structured_badge_shows_in_terminal_view_mode() {
    let (mut env, _id) = structured_session_env();
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let screen = render_home(&mut env);
    assert!(
        screen.contains("[structured]"),
        "badge missing in Terminal view mode:\n{screen}"
    );
}

fn render_footer(env: &mut TestEnv) -> String {
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

/// Tab is Enter's complement on a session row: whichever of
/// live-send / tmux-attach `default_attach_mode` doesn't route Enter
/// to. The footer must surface that complement so it isn't only
/// discoverable by reading the source or the `?` help overlay.
#[test]
#[serial]
fn footer_advertises_tab_as_live_when_default_is_tmux() {
    let mut env = create_test_env_empty();
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let out = render_footer(&mut env);
    assert!(
        out.contains("↵  Attach"),
        "Enter hint should stay tmux attach under the default mode.\n{out}"
    );
    assert!(
        out.contains("⇥  Live"),
        "Tab hint should advertise Live mode when Enter is pinned to tmux attach.\n{out}"
    );
}

/// Inverse of the above: once `default_attach_mode = LiveSend` takes
/// over Enter, the two hints swap rather than both claiming "Attach".
/// Enter owns live-send and Tab becomes the tmux escape hatch, the
/// same swap the `?` overlay does for this pairing.
#[test]
#[serial]
fn footer_advertises_tab_as_attach_when_default_is_live_send() {
    let mut env = create_test_env_empty();
    write_global_default_attach_mode(AttachMode::LiveSend);
    let _id = add_session(&mut env.view, "session-one");
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let out = render_footer(&mut env);
    assert!(
        out.contains("↵  Live"),
        "Enter hint should say Live once it owns live-send.\n{out}"
    );
    assert!(
        out.contains("⇥  Attach"),
        "Tab hint should offer the tmux escape hatch once Enter owns live-send.\n{out}"
    );
}

/// Acp/structured rows ignore `default_attach_mode` entirely (Tab
/// either mirrors Enter or no-ops), so the footer must not advertise
/// a Tab complement that doesn't actually do anything different.
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
    let out = render_footer(&mut env);
    assert!(
        !out.contains("⇥"),
        "structured view rows must not show a Tab hint at all.\n{out}"
    );
    assert!(
        out.contains("↵  Attach"),
        "structured rows keep the plain Enter attach label.\n{out}"
    );
}
