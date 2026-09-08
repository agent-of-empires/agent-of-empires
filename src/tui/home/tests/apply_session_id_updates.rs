//! Post-CAS env publish: env mirrors the disk-confirmed sid
//! (Applied) or reloaded peer value (Skipped); filter paths
//! republish the memory mirror to clear `on_change`'s pre-CAS write.

use super::*;
use crate::session::poller::SessionPoller;
use crate::session::{ResumeIntent, View};
use std::sync::{Arc, Mutex};

const NEW_SID: &str = "019342ab-1111-7aaa-8bbb-cccdddeeefff";

struct TmuxSession(String);

impl TmuxSession {
    fn create(id: &str, title: &str) -> Self {
        Self::create_named(crate::tmux::Session::generate_name(id, title))
    }

    fn create_terminal(id: &str, title: &str) -> Self {
        Self::create_named(crate::tmux::TerminalSession::generate_name(id, title))
    }

    fn create_named(name: String) -> Self {
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &name])
            .output();
        let status = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name])
            .status()
            .expect("failed to spawn tmux");
        assert!(status.success(), "tmux new-session failed for {}", name);
        crate::tmux::refresh_session_cache();
        Self(name)
    }
    fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &self.0])
            .output();
        crate::tmux::refresh_session_cache();
    }
}

fn skip_if_no_tmux() -> bool {
    if crate::tmux::tmux_command().arg("-V").output().is_err() {
        eprintln!("Skipping: tmux not available");
        return true;
    }
    false
}

fn captured_env(name: &str) -> Option<String> {
    crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
}

fn build_view_with_inst(profile: &str, inst: &Instance) -> HomeView {
    use crate::session::config::GroupByMode;
    let storage = Storage::new_unwatched(profile).unwrap();
    storage
        .update(|i, g| {
            *i = vec![inst.clone()];
            *g = GroupTree::new_with_groups(std::slice::from_ref(inst), &[]).get_all_groups();
            Ok(())
        })
        .unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some(profile.to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();
    view
}

fn attach_poller_with_update(view: &mut HomeView, instance_id: &str, sid: &str) {
    let poller = SessionPoller::new("test-session".to_string());
    poller.inject_test_update(instance_id, sid);
    let arc = Arc::new(Mutex::new(poller));
    if let Some(i) = view.instances.get_mut(instance_id) {
        i.session_id_poller = Some(arc);
    }
}

fn fresh_instance(profile: &str, title: &str) -> Instance {
    let mut inst = Instance::new(title, "/tmp/x");
    inst.tool = "claude".to_string();
    inst.source_profile = profile.to_string();
    inst.agent_session_id = None;
    inst.resume_intent = ResumeIntent::Default;
    inst
}

#[test]
#[serial]
fn apply_session_id_updates_publishes_after_cas() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-publish";
    let inst = fresh_instance(profile, "apa");
    let mut view = build_view_with_inst(profile, &inst);

    let tmux = TmuxSession::create(&inst.id, &inst.title);

    attach_poller_with_update(&mut view, &inst.id, NEW_SID);

    let updated = view.apply_session_id_updates();
    assert!(updated, "Applied CAS must report a touch");
    assert_eq!(captured_env(tmux.name()).as_deref(), Some(NEW_SID));
}

#[test]
#[serial]
fn apply_session_id_updates_publishes_to_terminal_session() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-terminal-publish";
    let mut inst = fresh_instance(profile, "terminal-post-cas");
    inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
    let mut view = build_view_with_inst(profile, &inst);

    let tmux = TmuxSession::create_terminal(&inst.id, &inst.title);
    let agent_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);

    attach_poller_with_update(&mut view, &inst.id, NEW_SID);

    let updated = view.apply_session_id_updates();
    assert!(updated, "Applied CAS must report a touch");
    assert!(captured_env(&agent_name).is_none());
    assert_eq!(captured_env(tmux.name()).as_deref(), Some(NEW_SID));
}

#[test]
#[serial]
fn apply_session_id_updates_skips_retroactive_excludes() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-excludes";
    let inst = fresh_instance(profile, "aer");
    let mut view = build_view_with_inst(profile, &inst);
    if let Some(i) = view.instances.get_mut(&inst.id) {
        i.retroactive_capture_excludes.insert(NEW_SID.to_string());
    }

    let tmux = TmuxSession::create(&inst.id, &inst.title);
    crate::tmux::env::set_hidden_env(
        tmux.name(),
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
        "stale-untouched",
    )
    .unwrap();

    attach_poller_with_update(&mut view, &inst.id, NEW_SID);

    let updated = view.apply_session_id_updates();
    assert!(
        !updated,
        "filtered sid must not propagate to memory (returned bool tracks memory)"
    );
    let mem_sid = view
        .instances
        .get(&inst.id)
        .and_then(|i| i.agent_session_id.clone());
    assert!(
        mem_sid.is_none(),
        "filtered sid must not enter in-memory mirror"
    );
    assert!(
        captured_env(tmux.name()).is_none(),
        "filtered sid must not survive in tmux env: env converges on disk (None)"
    );
}

#[test]
#[serial]
fn apply_session_id_updates_skipped_publishes_disk_value() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-skipped";
    let peer_sid = "019342aa-3333-7eee-8fff-aaaabbbbcccc";
    let other_peer = "019342bb-4444-7fff-8000-111122223333";

    let mut inst = fresh_instance(profile, "ase");
    inst.agent_session_id = Some(peer_sid.to_string());
    let mut view = build_view_with_inst(profile, &inst);

    let storage = Storage::new_unwatched(profile).unwrap();
    storage
        .update(|i, _g| {
            i[0].agent_session_id = Some(other_peer.to_string());
            Ok(())
        })
        .unwrap();

    let tmux = TmuxSession::create(&inst.id, &inst.title);
    crate::tmux::env::set_hidden_env(
        tmux.name(),
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
        NEW_SID,
    )
    .unwrap();

    attach_poller_with_update(&mut view, &inst.id, NEW_SID);

    let updated = view.apply_session_id_updates();
    assert!(updated, "Skipped path still touches state");

    let mem_sid = view
        .instances
        .get(&inst.id)
        .and_then(|i| i.agent_session_id.clone());
    assert_eq!(
        mem_sid.as_deref(),
        Some(other_peer),
        "memory rolls back to disk after CAS skip"
    );
    assert_eq!(
        captured_env(tmux.name()).as_deref(),
        Some(other_peer),
        "env converges from poller's pre-published NEW_SID to disk's other_peer"
    );
}

#[test]
#[serial]
fn apply_session_id_updates_invalid_sid_corrects_env() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-invalid";
    let inst = fresh_instance(profile, "aiv");
    let mut view = build_view_with_inst(profile, &inst);

    let tmux = TmuxSession::create(&inst.id, &inst.title);
    crate::tmux::env::set_hidden_env(
        tmux.name(),
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
        "bad sid!",
    )
    .unwrap();

    attach_poller_with_update(&mut view, &inst.id, "bad sid!");

    let updated = view.apply_session_id_updates();
    assert!(
        !updated,
        "validation-filtered sid must not propagate to memory"
    );
    assert!(
        captured_env(tmux.name()).is_none(),
        "env converges to disk-backed memory mirror (None) after validation failure"
    );
}

#[test]
#[serial]
fn apply_session_id_updates_no_tmux_session_skips_publish() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-pane-dead";
    let inst = fresh_instance(profile, "apds");
    let mut view = build_view_with_inst(profile, &inst);

    attach_poller_with_update(&mut view, &inst.id, NEW_SID);

    let updated = view.apply_session_id_updates();
    assert!(
        updated,
        "CAS still applies even when no tmux session exists"
    );
    let mem_sid = view
        .instances
        .get(&inst.id)
        .and_then(|i| i.agent_session_id.clone());
    assert_eq!(
        mem_sid.as_deref(),
        Some(NEW_SID),
        "memory still mirrors the CAS-applied sid",
    );
    let expected_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    assert!(
        captured_env(&expected_name).is_none(),
        "no tmux session means no publish target"
    );
}

#[test]
#[serial]
fn repair_session_id_pollers_skips_structured_and_repairs_live_terminal() {
    if skip_if_no_tmux() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let profile = "apply-poller-repair";
    let terminal = fresh_instance(profile, "repair-terminal");
    let mut structured = fresh_instance(profile, "repair-structured");
    structured.view = View::Structured;
    let mut view = build_view_with_inst(profile, &terminal);
    view.instances
        .insert(structured.id.clone(), structured.clone());
    let terminal_stopped = Arc::new(Mutex::new(SessionPoller::new("stopped".to_string())));
    let structured_stopped = Arc::new(Mutex::new(SessionPoller::new("stopped".to_string())));
    view.instances
        .get_mut(&terminal.id)
        .unwrap()
        .session_id_poller = Some(terminal_stopped.clone());
    view.instances
        .get_mut(&structured.id)
        .unwrap()
        .session_id_poller = Some(structured_stopped.clone());
    let _tmux = TmuxSession::create(&terminal.id, &terminal.title);

    assert!(!view.apply_session_id_updates());
    assert!(
        Arc::ptr_eq(
            &view
                .instances
                .get(&terminal.id)
                .and_then(|i| i.session_id_poller.clone())
                .expect("drain should retain the stopped poller"),
            &terminal_stopped,
        ),
        "the hot drain path must not repair pollers"
    );

    view.repair_session_id_pollers();
    let repaired = view
        .instances
        .get(&terminal.id)
        .and_then(|i| i.session_id_poller.clone())
        .expect("live pane should receive a replacement poller");
    assert!(!Arc::ptr_eq(&repaired, &terminal_stopped));
    assert!(repaired
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_running());
    assert!(
        Arc::ptr_eq(
            &view
                .instances
                .get(&structured.id)
                .and_then(|i| i.session_id_poller.clone())
                .expect("structured poller should be untouched"),
            &structured_stopped,
        ),
        "structured sessions must not probe tmux or start terminal pollers"
    );
    repaired
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop();
}

/// Discarding unsaved Settings changes via a mouse click on the
/// confirmation dialog's [Yes] button must revert a live theme preview,
/// exactly like the keyboard discard path. Regression for the
/// empire -> rose-pine flip where the click path closed Settings but
/// never dispatched `SetTheme`, leaving the previewed theme applied until
/// the next restart.
#[test]
#[serial]
fn settings_mouse_discard_reverts_theme_preview() {
    use crate::tui::dialogs::ConfirmDialog;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let view = &mut env.view;
    view.open_settings();
    assert!(view.settings_view.is_some(), "settings view should open");

    // Stand in the state reached after the user previewed a theme (so the
    // view has unsaved changes) and pressed Esc to close: the unsaved-
    // changes confirm dialog floats over the settings takeover.
    view.settings_close_confirm = true;
    view.confirm_dialog = Some(ConfirmDialog::new(
        "Unsaved Changes",
        "You have unsaved changes. Discard them?",
        "discard_settings",
    ));

    // Render once so the dialog's [Yes] button hit-rect is populated at the
    // exact coordinates it draws.
    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    let yes = view
        .confirm_dialog
        .as_ref()
        .unwrap()
        .yes_button_area_for_test();
    assert!(yes.width > 0, "render should populate the [Yes] hit-rect");

    // Click the center of [Yes] to discard.
    view.handle_dialog_click(yes.x + yes.width / 2, yes.y + yes.height / 2);

    // The click path must queue the same theme revert the keyboard path
    // returns. Before the fix this was `None` and the previewed theme stuck.
    assert!(
        matches!(view.pending_dialog_click_action, Some(Action::SetTheme(_))),
        "mouse discard should queue a SetTheme revert, got {:?}",
        view.pending_dialog_click_action
    );
    assert!(view.settings_view.is_none(), "settings should be closed");
    assert!(!view.settings_close_confirm, "confirm flag should reset");
}
