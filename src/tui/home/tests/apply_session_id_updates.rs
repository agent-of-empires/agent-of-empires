use super::*;

#[tokio::test]
#[serial]
async fn canonical_identity_updates_never_install_a_tui_poller_or_write_payloads() {
    let (_temp, _guard) = test_home();
    let mut row = Instance::new("identity", "/tmp/identity");
    row.source_profile = "test".into();
    row.tool = "claude".into();
    row.status = Status::Idle;
    seed_profile("test", std::slice::from_ref(&row));
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    apply_published(&mut view, &state).await;
    {
        let mut rows = state.instances.write().await;
        crate::server::test_support::attach_session_id_update_for_test(
            &mut rows[0],
            "019342ab-1111-7aaa-8bbb-cccdddeeefff",
        );
    }
    crate::server::test_support::drain_session_id_updates_for_test(&state).await;
    let stored = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert_eq!(
        stored[0].agent_session_id.as_deref(),
        Some("019342ab-1111-7aaa-8bbb-cccdddeeefff")
    );
    let bytes = payload_bytes("test");
    apply_published(&mut view, &state).await;
    assert_eq!(
        view.get_instance(&row.id).unwrap().agent_session_id,
        stored[0].agent_session_id
    );
    assert!(view
        .get_instance(&row.id)
        .unwrap()
        .session_id_poller
        .is_none());
    view.reload().unwrap();
    assert_eq!(payload_bytes("test"), bytes);
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
