//! Live-send wiring at the home view level. Translation correctness
//! is covered by unit tests in src/tui/home/live_send.rs. Here we
//! verify the integration points: keys are captured while live mode
//! is active, Ctrl+q clears the state, the per-keystroke liveness
//! check auto-exits on drift, and the predicate plumbing treats
//! live mode like a modal capture so the rest of the TUI suspends
//! underneath it.

use super::super::live_send::LiveSendState;
use super::*;

/// Seed live-send state pointing at the first instance in the test
/// env, with a matching tmux_name so the drift check passes. Tests
/// that want to trigger drift either install pointing at a missing
/// id or mutate the instance's title after installing.
fn install_live_for_first_session(env: &mut TestEnv) -> String {
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has no sessions; use install_live_orphan instead");
    let inst = env.view.get_instance(&id).unwrap().clone();
    let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    // CI runs the e2e suite in the same `cargo test` invocation,
    // which populates the global tmux session cache. The drift
    // check then sees our fake test session name as "not in tmux"
    // (Some(false)) and clears live_send mid-test. Pre-inject the
    // name so the cache reports Some(true) for it; orphan tests
    // (install_live_orphan) deliberately skip this and let the
    // instance-missing branch fire instead.
    crate::tmux::test_inject_session_into_cache(&tmux_name);
    env.view.live_send = Some(LiveSendState {
        session_id: inst.id.clone(),
        title: inst.title,
        tmux_name,
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });
    id
}

/// Install live-send state pointing at a session id the env does
/// NOT contain — used to verify the drift check fires (auto-exit
/// + info dialog) when the underlying instance has vanished.
fn install_live_orphan(env: &mut TestEnv) {
    env.view.live_send = Some(LiveSendState {
        session_id: "missing-id".to_string(),
        title: "missing-title".to_string(),
        tmux_name: "missing-tmux".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });
}

/// A lock-loss flag from the live-send worker (another surface stole
/// the size-owner lock) exits live mode from the main-loop poll, with
/// no keystroke needed, drops the worker, and explains the takeover in
/// an info dialog. The sizing reset is skipped so the thief's grid
/// stands; only the shared teardown runs.
#[test]
#[serial]
fn poll_live_send_takeover_exits_live_mode_with_dialog() {
    use crate::tui::home::live_send::LiveSendWorker;
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    env.view.live_send_worker = Some(LiveSendWorker::spawn("fake".to_string(), None));

    // Flag not set: the poll is a no-op and live mode stays.
    assert!(!env.view.poll_live_send_takeover());
    assert!(env.view.live_send.is_some());
    assert!(env.view.info_dialog.is_none());

    env.view
        .live_send_worker
        .as_ref()
        .expect("worker mounted")
        .force_lock_lost_for_test();
    assert!(env.view.poll_live_send_takeover());
    assert!(env.view.live_send.is_none(), "live mode must exit");
    assert!(
        env.view.live_send_worker.is_none(),
        "worker must be dropped"
    );
    let dialog = env.view.info_dialog.as_ref().expect("info dialog shown");
    assert_eq!(dialog.title(), "Live send ended");
    assert!(
        dialog.message().contains("took over"),
        "dialog must explain the takeover, got: {}",
        dialog.message()
    );
}

#[test]
#[serial]
fn ctrl_q_exits_live_mode() {
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    assert!(env.view.live_send.is_some());

    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        None,
    );

    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn ctrl_q_exits_even_when_session_has_drifted() {
    // Ctrl+q is the safety chord: it must always exit cleanly,
    // even if the underlying session went away (so the user can
    // recover from a stuck live mode without an extra dialog).
    let mut env = create_test_env_empty();
    install_live_orphan(&mut env);
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.live_send.is_none());
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn arbitrary_key_in_live_mode_does_not_emit_action() {
    // Live-send swallows the key (forwards it to tmux). The tmux
    // call will quietly fail because the test env doesn't have a
    // real tmux pane, but the home view must NOT bubble an
    // Action::* out (otherwise the action would race with the
    // live state). Use bare `x` so the test doesn't collide with
    // the Ctrl+q exit chord.
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    let action = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
    assert!(action.is_none());
    // Still in live mode; only Ctrl+q exits.
    assert!(env.view.live_send.is_some());
}

#[test]
#[serial]
fn drift_check_auto_exits_when_instance_missing() {
    // If the session is deleted while live mode is active, the
    // very next keystroke should auto-exit and surface an info
    // dialog explaining why (so the user isn't typing into the
    // void with no feedback).
    let mut env = create_test_env_empty();
    install_live_orphan(&mut env);
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
    assert!(env.view.live_send.is_none());
    assert!(env.view.info_dialog.is_some());
}

#[test]
#[serial]
fn shift_page_up_scrolls_preview_instead_of_sending_to_agent() {
    // Terminal-emulator convention: Shift+PageUp scrolls the outer
    // scrollback, not the inner program. Live mode honors that so
    // users can read agent history without exiting.
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    env.view.preview_scroll_offset = 0;

    env.view
        .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT), None);

    assert!(
        env.view.preview_scroll_offset > 0,
        "Shift+PageUp should scroll the preview back into history"
    );
    // Still in live mode — the intercept doesn't exit.
    assert!(env.view.live_send.is_some());
}

#[test]
#[serial]
fn shift_page_down_scrolls_preview_forward() {
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    env.view.preview_scroll_offset = 50;

    env.view
        .handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT), None);

    assert!(
        env.view.preview_scroll_offset < 50,
        "Shift+PageDown should reduce the offset (scroll toward live)"
    );
    assert!(env.view.live_send.is_some());
}

#[test]
#[serial]
fn bare_page_up_still_passes_through_to_agent() {
    // Regression guard: only the Shift-modified Page chord is
    // intercepted. Bare PageUp must keep flowing to the agent so
    // agents that page their own UI (claude-code transcript, etc.)
    // keep responding.
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    env.view.preview_scroll_offset = 25;

    env.view
        .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), None);

    assert_eq!(
        env.view.preview_scroll_offset, 25,
        "bare PageUp must NOT change preview scroll offset"
    );
    assert!(env.view.live_send.is_some());
}

#[test]
#[serial]
fn drift_check_auto_exits_when_session_renamed() {
    // A rename that carried the tmux session with it: the worker now holds
    // a name tmux no longer has, so the next keystroke should auto-exit.
    // Force the cache to the post-rename state (only the new name live) so
    // the id-anchored resolution has nothing stale to adopt.
    let mut env = create_test_env_with_sessions(1);
    let id = install_live_for_first_session(&mut env);
    env.view.mutate_instance(&id, |inst| {
        inst.title = "renamed-after-entry".to_string();
    });
    let inst = env.view.get_instance(&id).unwrap().clone();
    let renamed = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let guard = crate::tmux::SessionCacheGuard::capture();
    guard.force_present(&[renamed.as_str()]);

    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
    assert!(env.view.live_send.is_none());
    assert!(env.view.info_dialog.is_some());
}

#[test]
#[serial]
fn drift_check_stays_when_retitle_did_not_rename_the_tmux_session() {
    // #3157: smart rename moves the title but the tmux session keeps the
    // name it was created under. The worker still holds THIS session's
    // pane, so that is not drift and live mode must survive; auto-exiting
    // here would kick the user out of a pane that is still correct.
    let mut env = create_test_env_with_sessions(1);
    let id = install_live_for_first_session(&mut env);
    let created = env.view.live_send.as_ref().unwrap().tmux_name.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.title = "Refactor billing module".to_string();
    });
    let guard = crate::tmux::SessionCacheGuard::capture();
    guard.force_present(&[created.as_str()]);

    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);
    assert!(
        env.view.live_send.is_some(),
        "a retitle that never reached tmux must not read as drift"
    );
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn drift_check_does_not_exit_for_tool_target_named_via_tool_session() {
    // Regression guard: the Tool arm of the drift check must resolve
    // the current name the same way `prepare_live_send` computed
    // `tmux_name` at entry (via `ToolSession::new(..).session_name()`).
    // A prior bug instead re-derived the Tool arm's "current name"
    // through `Session::generate_name`, the agent-pane naming scheme,
    // which never matches a tool's own tmux name. That mismatch made
    // every Tool-view live-send look "renamed" on its very first
    // keystroke and auto-exit immediately.
    let mut env = create_test_env_with_sessions(1);
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");
    let inst = env.view.get_instance(&id).unwrap().clone();
    let tmux_name = crate::tmux::ToolSession::new(&inst.id, &inst.title, "lazygit")
        .session_name()
        .to_string();
    crate::tmux::test_inject_session_into_cache(&tmux_name);
    env.view.live_send = Some(LiveSendState {
        session_id: inst.id.clone(),
        title: inst.title,
        tmux_name,
        target: crate::tui::home::live_send::LiveSendTarget::Tool("lazygit".to_string()),
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });

    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), None);

    assert!(
        env.view.live_send.is_some(),
        "first keystroke in a Tool-view live-send must not trip spurious drift"
    );
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn live_mode_makes_has_dialog_true() {
    // Every dialog-gating predicate that already inspects has_dialog()
    // (mouse swallow, list nav suspend, palette skip) inherits live
    // mode for free via this single addition.
    let mut env = create_test_env_empty();
    assert!(!env.view.has_dialog());
    install_live_orphan(&mut env);
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn live_mode_enables_paste_burst() {
    // wants_paste_burst is what tells the runtime to batch a stream
    // of Char events into a single Paste event when bracketed-paste
    // markers are missing (mosh, some SSH wrappers). Live mode wants
    // batching so a paste streams as one tmux call.
    let mut env = create_test_env_empty();
    install_live_orphan(&mut env);
    assert!(env.view.wants_paste_burst());
}

#[test]
#[serial]
fn tab_does_not_start_live_send_without_selection() {
    // No session selected (empty list, cursor on a group, etc.) →
    // Tab must silently no-op rather than emitting a deferred
    // action targeting nothing.
    let mut env = create_test_env_empty();
    let action = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
    assert!(action.is_none());
    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn tab_emits_enter_live_send_for_stopped_session() {
    // start_live_send is intentionally permissive: it accepts any
    // non-Creating instance and defers ensure_pane_ready to
    // prepare_live_send. Without this, Tab would silently no-op on
    // stopped/dead-but-recoverable rows because the tmux session
    // doesn't exist yet.
    let mut env = create_test_env_with_sessions(1);
    env.view.cursor = 0;
    env.view.update_selected();
    // Pin the status explicitly so this regression guard doesn't
    // rely on the implicit Instance::new default surviving future
    // refactors. A real stopped session is what we're modeling.
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Stopped;
    });
    let action = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
    assert!(
        matches!(action, Some(Action::EnterLiveSend(_))),
        "Tab on a stopped session should emit Action::EnterLiveSend, got {:?}",
        action
    );
}

#[test]
#[serial]
fn tab_does_not_start_live_send_for_acp_session() {
    // Acp sessions are not tmux-backed, so live-send has no valid
    // target. Tab must refuse with a visible "no tmux pane" toast
    // (a silent no-op reads as a broken key) and must never
    // enqueue an Action::EnterLiveSend that would fail downstream.
    let mut env = create_test_env_with_sessions(1);
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");
    env.view.mutate_instance(&id, |inst| {
        inst.status = crate::session::Status::Stopped;
        inst.view = crate::session::View::Structured;
    });
    let action = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), None);
    assert!(
        matches!(
            &action,
            Some(Action::SetTransientStatus(msg)) if msg.contains("no tmux pane")
        ),
        "Tab on a structured row must surface the no-tmux-pane toast, got {action:?}"
    );
    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn has_non_live_send_overlay_false_in_pure_live_mode() {
    // Regression for the dead-fast-path bug: `has_dialog()` returns
    // true when live-send is active, which would gate off the
    // preview-only fast path (added in #1495) — the very thing it
    // was supposed to enable. `has_non_live_send_overlay()` is the
    // helper the fast-path gates use; in pure live mode with no
    // other dialog open, it must be false so the fast path can run.
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    assert!(env.view.has_dialog(), "has_dialog includes live_send");
    assert!(
        !env.view.has_non_live_send_overlay(),
        "live-send alone must NOT count as an overlay for the fast-path gate"
    );
}

#[test]
#[serial]
fn has_non_live_send_overlay_true_when_dialog_also_open() {
    // The fast path still has to bail when any non-live overlay is
    // on top of the home view (settings, diff, info dialog, etc.),
    // because the snapshot it repaints doesn't include them.
    let mut env = create_test_env_with_sessions(1);
    install_live_for_first_session(&mut env);
    env.view.info_dialog = Some(InfoDialog::new("title", "body"));
    assert!(env.view.has_non_live_send_overlay());
}

/// Regression for the preview-mode paste misroute: a rename dialog
/// opened on top of live-send (reachable via the right-click context
/// menu, which stays clickable while "attached") must receive pastes.
/// Before the fix, `handle_paste` gave live-send absolute priority,
/// so the clipboard streamed into the agent's pane while the user was
/// staring at a focused dialog input; the key path had already been
/// taught to route to overlays, but paste hadn't.
#[test]
#[serial]
fn paste_routes_to_rename_dialog_opened_over_live_send() {
    let mut env = create_test_env_with_sessions(1);
    env.view.update_selected();
    install_live_for_first_session(&mut env);
    env.view.open_rename_for_selected();
    assert!(env.view.rename_dialog.is_some());
    assert!(
        env.view.live_send.is_some(),
        "live-send must stay active underneath the dialog"
    );

    env.view.handle_paste("pasted-title");

    assert_eq!(
        env.view.rename_dialog.as_ref().unwrap().title_value(),
        "pasted-title",
        "paste must land in the dialog's focused input, not the pane behind it"
    );
}

/// Companion pin: with live-send active and NO overlay on top, paste
/// keeps streaming to the pane. The unit fixture has no worker
/// attached, so the observable contract is that the live-send branch
/// consumes the paste: nothing buffers into a compose dialog or
/// pending_paste.
#[test]
#[serial]
fn paste_in_pure_live_mode_is_consumed_by_live_send() {
    let mut env = create_test_env_with_sessions(1);
    env.view.update_selected();
    install_live_for_first_session(&mut env);

    env.view.handle_paste("streamed to pane");

    assert!(env.view.send_message_dialog.is_none());
    assert!(env.view.pending_paste.is_none());
}

/// A finalized preview highlight (installed via a mouse drag, which
/// never runs through `handle_key`) must be dropped when the user
/// pastes into a dialog opened over live-send. Before the clear was
/// hoisted to the top of `handle_paste`, only the pane-streaming
/// branch cleared it, so the highlight survived a dialog-routed paste
/// and kept repainting over stale cells after the dialog closed.
#[test]
#[serial]
fn paste_into_dialog_over_live_send_clears_preview_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view.update_selected();
    install_live_for_first_session(&mut env);
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, 0),
        extent: (4, 2),
        finalized: true,
    });
    env.view.open_rename_for_selected();
    assert!(env.view.rename_dialog.is_some());
    assert!(
        env.view.preview_selection.is_some(),
        "precondition: opening the dialog must not clear the selection"
    );

    env.view.handle_paste("pasted-title");

    assert!(
        env.view.preview_selection.is_none(),
        "a dialog-routed paste must still drop the finalized highlight"
    );
    assert_eq!(
        env.view.rename_dialog.as_ref().unwrap().title_value(),
        "pasted-title"
    );
}

#[test]
#[serial]
fn refresh_preserves_cache_when_live_capture_fails() {
    // Pin the kill-switch behavior (originally introduced in #1501,
    // re-implemented here against the fork-only capture path):
    // when live-send is active and the capture call fails (in this
    // unit fixture the backing tmux session doesn't exist, so the
    // fork returns Err), the previous capture's content must stay
    // in the cache. Pre-#1501 a single failed capture wiped
    // `preview_cache.content` to "" and the preview rendered
    // "No output available" until the user exited and re-entered
    // live mode.
    let mut env = create_test_env_with_sessions(1);
    let id = install_live_for_first_session(&mut env);
    env.view.selected_session = Some(id.clone());
    env.view.preview_cache.content = "hello from a successful capture".to_string();
    env.view.preview_cache.captured_lines = 1;
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.session_id = Some(id);

    env.view.refresh_preview_cache_if_needed(80, 24);

    assert_eq!(
        env.view.preview_cache.content, "hello from a successful capture",
        "cache must be preserved when the fork capture fails inside live mode"
    );
    assert_eq!(env.view.preview_cache.captured_lines, 1);
}

#[test]
#[serial]
fn accepted_capture_keeps_content_and_cursor_in_one_cache_frame() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.selected_session.clone().expect("selected session");
    env.view
        .sync_preview_capture_worker(Some("aoe_test_atomic_cursor".to_string()));
    let first = crate::tmux::PaneCursor {
        x: 1,
        y: 2,
        visible: true,
        pane_height: 24,
        history_size: 0,
        pane_width: 80,
        alternate_on: false,
        mouse_tracking: false,
        mouse_sgr: false,
        mouse_all: false,
        position_reliable: true,
        composite_pane0: None,
    };
    env.view
        .preview_capture_worker
        .as_ref()
        .expect("capture worker")
        .inject_frame_with_cursor_for_test(40, "stable content", Some(first));
    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(env.view.preview_cache.session_id, Some(id));
    assert_eq!(env.view.preview_cache.content, "stable content");
    assert_eq!(env.view.preview_cache.cursor, Some(first));

    let second = crate::tmux::PaneCursor { x: 7, ..first };
    env.view
        .preview_capture_worker
        .as_ref()
        .expect("capture worker")
        .inject_frame_with_cursor_for_test(40, "stable content", Some(second));
    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(env.view.preview_cache.cursor, Some(second));
    env.view
        .sync_preview_capture_worker(Some("aoe_test_atomic_other".to_string()));
    assert!(env.view.active_preview_cursor().is_none());
    env.view
        .sync_preview_capture_worker(Some("aoe_test_atomic_cursor".to_string()));
    // Simulate an old cache surviving an A -> B -> A switch. Matching the
    // target string is insufficient; the accepted generation must match.
    env.view.preview_cache.cursor = Some(second);
    assert!(env.view.active_preview_cursor().is_none());
}
#[test]
#[serial]
fn warm_predicates_stay_cold_without_a_live_pane() {
    // The EnterLiveSend / SendMessage handlers skip the "Reviving
    // session..." toast only when the target pane is provably warm; every
    // uncertain case must stay cold so a real revive keeps its feedback.
    // The unit fixture has no tmux server, so even a live-status row must
    // report cold (pane existence is the load-bearing half).
    let mut env = create_test_env_with_sessions(1);
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");

    // Unknown session id: cold.
    assert!(!env.view.agent_pane_is_warm("no-such-session"));
    assert!(!env.view.live_entry_is_warm("no-such-session"));

    // Live status but no tmux session behind it: cold.
    env.view
        .set_instance_status(&id, crate::session::Status::Idle);
    assert!(!env.view.agent_pane_is_warm(&id));

    // Non-live statuses are cold regardless of pane state.
    for status in [
        crate::session::Status::Stopped,
        crate::session::Status::Starting,
        crate::session::Status::Error,
        crate::session::Status::Unknown,
    ] {
        env.view.set_instance_status(&id, status);
        assert!(
            !env.view.agent_pane_is_warm(&id),
            "status {status:?} must not count as warm"
        );
    }

    // Terminal-target warmth is keyed on the paired terminal pane, which
    // the fixture also lacks: cold.
    env.view
        .set_instance_status(&id, crate::session::Status::Idle);
    env.view.pending_live_send_target = crate::tui::home::live_send::LiveSendTarget::Terminal;
    assert!(!env.view.live_entry_is_warm(&id));
}

#[test]
#[serial]
fn passive_preview_sync_ignores_one_frame_toast_geometry() {
    // The EnterLiveSend / SendMessage handlers draw exactly one frame with
    // a transient toast up; its bottom bar makes the preview output rect
    // one row shorter for that frame only. The passive sync must not chase
    // it: pre-debounce it resized the agent's pane down and back up ~30ms
    // apart, and the double SIGWINCH made claude's bottom-anchored input
    // box (and cursor) visibly jump right as live mode opened.
    let mut env = create_test_env_with_sessions(1);
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");
    env.view.selected_session = Some(id.clone());
    // Steady state: pane already synced to the toast-free geometry.
    let synced = crate::tui::home::PassiveSynced {
        cols: 141,
        rows: 43,
        window_rows: 43,
        adopted_at: std::time::Instant::now(),
    };
    env.view.passive_pane_synced.insert(id.clone(), synced);

    // Toast frame: one row shorter. Armed only; the synced geometry (and
    // with it the real pane) must stay untouched.
    env.view.refresh_preview_cache_if_needed(141, 42);
    assert_eq!(env.view.preview_pane_pending, Some((id.clone(), 141, 42)));
    assert_eq!(env.view.passive_pane_synced.get(&id), Some(&synced));

    // Post-toast frame: back in sync; the transient arm is dropped so a
    // later real change still needs two consecutive sightings.
    env.view.refresh_preview_cache_if_needed(141, 43);
    assert_eq!(env.view.preview_pane_pending, None);
    assert_eq!(env.view.passive_pane_synced.get(&id), Some(&synced));
}

#[test]
#[serial]
fn fleet_reconcile_presizes_open_sessions_once_per_epoch() {
    let mut env = create_test_env_with_sessions(3);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 3);
    let selected = ids[0].clone();
    env.view.selected_session = Some(selected.clone());
    let inner = ratatui::layout::Rect::new(0, 0, 141, 45);

    // First sighting of a fleet geometry arms only; nothing reaches the
    // worker until the same geometry holds for a second refresh.
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_fleet_armed.is_some());
    assert!(env.view.passive_pane_queued.is_empty());

    // Second sighting queues every open session except the excluded
    // (selected) one.
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(!env.view.passive_pane_queued.contains_key(&selected));
    for id in &ids[1..] {
        assert!(
            env.view.passive_pane_queued.contains_key(id),
            "open session {id} must be handed to the worker"
        );
    }

    // The fixture sessions have no tmux panes, so the worker declines
    // each intent. Once the declines are adopted, the same epoch must not
    // re-queue them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        env.view
            .reconcile_passive_fleet(inner, false, Some(&selected));
        if ids[1..]
            .iter()
            .all(|id| env.view.passive_pane_declined.contains_key(id))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "declined completions never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(
        ids[1..]
            .iter()
            .all(|id| !env.view.passive_pane_queued.contains_key(id)),
        "a declined geometry must not be retried within its epoch"
    );

    // A different fleet geometry is a new epoch: arming clears the
    // declines and the follow-up refresh retries each session once.
    let inner2 = ratatui::layout::Rect::new(0, 0, 120, 38);
    env.view
        .reconcile_passive_fleet(inner2, false, Some(&selected));
    assert!(env.view.passive_pane_declined.is_empty());
    env.view
        .reconcile_passive_fleet(inner2, false, Some(&selected));
    for id in &ids[1..] {
        assert!(
            env.view.passive_pane_queued.contains_key(id),
            "a new epoch must retry {id} once"
        );
    }

    // Moving the selection is NOT an epoch: the armed key is pure
    // geometry, so switching the excluded session must not re-arm, and
    // the previously excluded session fires on the same refresh.
    let armed_before = env.view.passive_fleet_armed.clone();
    env.view.selected_session = Some(ids[1].clone());
    env.view
        .reconcile_passive_fleet(inner2, false, Some(&ids[1]));
    assert_eq!(env.view.passive_fleet_armed, armed_before);
    assert!(
        env.view.passive_pane_queued.contains_key(&ids[0]),
        "the newly deselected session must be handed to the worker without re-arming"
    );
}

#[test]
#[serial]
fn fleet_reconcile_reasserts_after_external_resize() {
    let _cache_guard = crate::tmux::PaneMetaCacheGuard::capture();
    let mut env = create_test_env_with_sessions(2);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let selected = ids[0].clone();
    env.view.selected_session = Some(selected.clone());
    let inner = ratatui::layout::Rect::new(0, 0, 141, 45);
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));

    // Pretend ids[1]'s resize was applied at its wanted geometry (read
    // back from the armed epoch so the fixture can't drift from the real
    // layout math).
    let (cols, rows) = env
        .view
        .passive_fleet_armed
        .as_ref()
        .and_then(|armed| armed.iter().find(|(id, ..)| id == &ids[1]))
        .map(|&(_, cols, rows)| (cols, rows))
        .expect("armed epoch covers ids[1]");
    env.view.passive_pane_synced.insert(
        ids[1].clone(),
        crate::tui::home::PassiveSynced {
            cols,
            rows,
            window_rows: rows,
            adopted_at: std::time::Instant::now(),
        },
    );
    let title = env.view.get_instance(&ids[1]).unwrap().title.clone();
    let name = crate::tmux::Session::resolve_name_for_display(&ids[1], &title);

    // A fresher observation MATCHING the applied size is not a
    // contradiction: the session stays in sync, nothing queued.
    crate::tmux::test_inject_pane_window_size(&name, (cols, rows));
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_pane_synced.contains_key(&ids[1]));
    assert!(!env.view.passive_pane_queued.contains_key(&ids[1]));

    // Another client resizes the window (external attach, web live
    // view): the contradicted entry is dropped and the pane re-asserted.
    crate::tmux::test_inject_pane_window_size(&name, (cols + 10, rows));
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(!env.view.passive_pane_synced.contains_key(&ids[1]));
    assert!(env.view.passive_pane_queued.contains_key(&ids[1]));
}

#[test]
#[serial]
fn stale_observation_published_after_adoption_does_not_invalidate() {
    // The cache boundary of the timestamp race: a `list-panes` that read
    // the pane BEFORE our resize can finish publishing AFTER the resize's
    // adoption. The snapshot's time is the observation instant (captured
    // pre-fork), so the pre-resize sizes it carries must read as older
    // than the adoption and leave the synced entry alone. Re-stamping
    // `cache.time` at publication would make this observation look
    // fresher than the adoption and turns this test red (the injector
    // routes through the real publication path).
    let _cache_guard = crate::tmux::PaneMetaCacheGuard::capture();
    let mut env = create_test_env_with_sessions(2);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let selected = ids[0].clone();
    env.view.selected_session = Some(selected.clone());
    let inner = ratatui::layout::Rect::new(0, 0, 141, 45);
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    let (cols, rows) = env
        .view
        .passive_fleet_armed
        .as_ref()
        .and_then(|armed| armed.iter().find(|(id, ..)| id == &ids[1]))
        .map(|&(_, cols, rows)| (cols, rows))
        .expect("armed epoch covers ids[1]");

    // The listing reads the pane's pre-resize size...
    let listed_at = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(2));
    // ...then our resize applies and is adopted...
    env.view.passive_pane_synced.insert(
        ids[1].clone(),
        crate::tui::home::PassiveSynced {
            cols,
            rows,
            window_rows: rows,
            adopted_at: std::time::Instant::now(),
        },
    );
    // ...and only then does the stale listing get published.
    let title = env.view.get_instance(&ids[1]).unwrap().title.clone();
    let name = crate::tmux::Session::resolve_name_for_display(&ids[1], &title);
    crate::tmux::test_inject_pane_window_size_at(&name, (cols + 10, rows), listed_at);

    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(
        env.view.passive_pane_synced.contains_key(&ids[1]),
        "a pre-resize observation must not invalidate the adopted size"
    );
    assert!(!env.view.passive_pane_queued.contains_key(&ids[1]));
}

#[test]
#[serial]
fn fleet_reconcile_retries_expired_declines() {
    let mut env = create_test_env_with_sessions(2);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let selected = ids[0].clone();
    env.view.selected_session = Some(selected.clone());
    let inner = ratatui::layout::Rect::new(0, 0, 141, 45);
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    let want = env
        .view
        .passive_fleet_armed
        .as_ref()
        .and_then(|armed| armed.iter().find(|(id, ..)| id == &ids[1]))
        .map(|&(_, cols, rows)| (cols, rows))
        .expect("armed epoch covers ids[1]");

    // A decline older than the retry window reads as absent, so the
    // session recovers once its blocking attach or size owner may have
    // gone away, instead of staying parked until a geometry change.
    let expired = std::time::Instant::now()
        .checked_sub(
            crate::tui::home::render::PASSIVE_DECLINE_RETRY + std::time::Duration::from_secs(1),
        )
        .expect("test clock predates the retry window");
    env.view
        .passive_pane_declined
        .insert(ids[1].clone(), (want, expired));
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_pane_queued.contains_key(&ids[1]));

    // A fresh decline parks it again.
    env.view.passive_pane_queued.clear();
    env.view
        .passive_pane_declined
        .insert(ids[1].clone(), (want, std::time::Instant::now()));
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(!env.view.passive_pane_queued.contains_key(&ids[1]));
}

#[test]
#[serial]
fn fleet_reconcile_is_single_tui_only() {
    // With two aoe TUIs alive, each would treat the other's fleet
    // resizes as external (observed-size invalidation) and re-assert its
    // own geometry, oscillating every open pane. The presence count gates
    // the whole fleet pass; only the selected-session sync stays on.
    let mut env = create_test_env_with_sessions(2);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let selected = ids[0].clone();
    env.view.selected_session = Some(selected.clone());
    let inner = ratatui::layout::Rect::new(0, 0, 141, 45);

    env.view.active_tui_count = 2;
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_fleet_armed.is_none());
    assert!(env.view.passive_pane_queued.is_empty());

    // Back down to one TUI: the fleet resumes on the usual two-sighting
    // debounce.
    env.view.active_tui_count = 1;
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_fleet_armed.is_some());
    env.view
        .reconcile_passive_fleet(inner, false, Some(&selected));
    assert!(env.view.passive_pane_queued.contains_key(&ids[1]));
}

#[test]
#[serial]
fn refresh_terminal_cache_overwrites_on_empty_capture() {
    // Counterpart to `refresh_preserves_cache_when_live_capture_fails`:
    // only the agent path carries the live-send kill switch. The terminal
    // path must overwrite to empty so the preview surfaces "session looks
    // gone". With the worker as the ONLY capture source, the empty frame
    // arrives through the mailbox (the worker forwards empties for
    // terminal panes); paint applies it without any synchronous fork.
    let mut env = create_test_env_with_sessions(1);
    let id = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("test env has one session");
    env.view.selected_session = Some(id.clone());
    env.view.terminal_preview_cache.content = "stale terminal output".to_string();
    env.view.terminal_preview_cache.captured_lines = 1;
    env.view.terminal_preview_cache.dimensions = (10, 10);
    env.view.terminal_preview_cache.session_id = Some(id.clone());

    env.view
        .sync_preview_capture_worker(Some("aoe_test_missing_terminal".to_string()));
    if let Some(worker) = env.view.preview_capture_worker.as_ref() {
        worker.inject_frame_for_test(40, "");
    }

    env.view.refresh_terminal_preview_cache_if_needed(80, 24);

    assert_eq!(
        env.view.terminal_preview_cache.content, "",
        "terminal cache must overwrite stale content (no kill switch outside the agent path)"
    );
    assert_eq!(env.view.terminal_preview_cache.dimensions, (80, 24));
    assert_eq!(env.view.terminal_preview_cache.session_id, Some(id));
}

mod paste_splitting {
    //! `split_paste_for_live_send` decomposes a pasted string into
    //! tmux operations the live-send worker can actually deliver.
    //! Single-line pastes stay on the simple `Literal` + `Named("Tab")`
    //! path so raw shells and bracketed-paste-unaware agents keep
    //! working. Multi-line pastes are dispatched as one `TmuxKey::Paste`
    //! payload without escape markers; tmux delegates bracketed-paste
    //! handling to `paste-buffer -p`, so the agent sees one paste instead
    //! of one `Enter` per line.

    use crate::tui::home::input::split_paste_for_live_send;
    use crate::tui::home::live_send::TmuxKey;

    fn lit(s: &str) -> TmuxKey {
        TmuxKey::Literal(s.to_string())
    }
    fn named(name: &str) -> TmuxKey {
        TmuxKey::Named(name.to_string())
    }

    /// Expected shape for a multi-line paste: one `Paste` action
    /// carrying the payload verbatim. No bracketed-paste markers
    /// appear here on purpose. tmux adds them via `paste-buffer -p`
    /// only for panes that set DECSET 2004, so a raw shell or SQL
    /// REPL no longer receives markers it would render as literal
    /// `00~` / `01~` text. Interior newlines stay `\n`; tmux
    /// translates them to CR when it performs the paste.
    fn paste(body: &str) -> Vec<TmuxKey> {
        vec![TmuxKey::Paste(body.to_string())]
    }

    #[test]
    fn paste_shapes_are_normalized_and_dispatched() {
        let cases = [
            ("printable", "hello world", vec![lit("hello world")]),
            ("multiline", "first\nsecond", paste("first\nsecond")),
            ("trailing newline", "only line\n", paste("only line\n")),
            ("leading newline", "\nbody", paste("\nbody")),
            // Windows and legacy-Mac line endings normalize to LF in the
            // payload; tmux translates LF to CR while performing the paste.
            ("crlf to lf", "a\r\nb", paste("a\nb")),
            ("bare cr to lf", "a\rb", paste("a\nb")),
            (
                "single-line tab",
                "a\tb",
                vec![lit("a"), named("Tab"), lit("b")],
            ),
            ("multiline tab", "a\tb\nc", paste("a\tb\nc")),
            // BEL and ESC have no safe mapping and are dropped.
            (
                "single-line control bytes",
                "a\x07b\x1bc",
                vec![lit("a"), lit("b"), lit("c")],
            ),
            ("multiline control bytes", "a\x07b\x1bc\nd", paste("abc\nd")),
            (
                "drag-select multiline",
                "alpha beta\nsecond line\nthird",
                paste("alpha beta\nsecond line\nthird"),
            ),
            ("multiline utf8", "café\n🚀", paste("café\n🚀")),
        ];

        for (name, input, expected) in cases {
            assert_eq!(split_paste_for_live_send(input), expected, "{name}");
        }
    }

    /// The bug: we used to hand-roll `\e[200~` / `\e[201~` into the
    /// payload and ship it as raw bytes, so every pane got the markers
    /// whether or not it had set DECSET 2004. A raw shell or SQL REPL
    /// parses `\e[2` as a partial Insert-key sequence, discards it, and
    /// self-inserts the leftover `00~` / `01~` into the user's text.
    /// The payload must now carry no escape bytes at all; tmux decides.
    #[test]
    fn multiline_paste_carries_no_escape_markers() {
        let keys = split_paste_for_live_send("SELECT id\nFROM users;");
        assert_eq!(keys, paste("SELECT id\nFROM users;"));
        match &keys[0] {
            TmuxKey::Paste(body) => {
                assert!(
                    !body.contains('\x1b'),
                    "paste payload must not carry ESC: {body:?}"
                );
                assert!(!body.contains("200~") && !body.contains("201~"));
            }
            other => panic!("expected Paste, got {other:?}"),
        }
    }

    #[test]
    fn multiline_paste_dispatches_as_one_payload() {
        // Single-dispatch: the entire paste is one `Paste` action, so
        // the worker fires exactly one `load-buffer` + `paste-buffer`
        // pair. Verifies the length-of-1 invariant the worker relies
        // on for paste latency.
        let out = split_paste_for_live_send("a\nb\nc\nd");
        assert_eq!(out.len(), 1, "multiline paste must be one TmuxKey");
        match &out[0] {
            TmuxKey::Paste(_) => {}
            other => panic!("expected Paste, got {other:?}"),
        }
    }

    #[test]
    fn empty_paste_is_empty() {
        // An empty paste emits nothing at all: an empty tmux paste
        // would still flash through some agents' paste handlers.
        assert!(split_paste_for_live_send("").is_empty());
    }
}
