use super::*;
use crate::tui::terminal_output::{
    install_test_output,
    tests::{
        block_terminal, drain_terminal, private_terminal, read_terminal_until, resume_terminal,
    },
};
use std::fs::File;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Trash,
    Permanent,
    Group,
}

#[derive(Clone, Copy, Debug)]
enum Stale {
    Unseen,
    Replaced,
    ChangedOptions,
    Covered,
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn dialog_open(app: &App, kind: Kind) -> bool {
    match kind {
        Kind::Trash => app.home.confirm_dialog.is_some(),
        Kind::Permanent => app.home.unified_delete_dialog.is_some(),
        Kind::Group => app.home.group_delete_options_dialog.is_some(),
    }
}

fn open(app: &mut App, kind: Kind) {
    app.home.handle_key(key(KeyCode::Char('d')), None);
    assert!(
        dialog_open(app, kind),
        "the real delete gesture must open {kind:?}"
    );
}

fn present(
    app: &mut App,
    terminal: &mut Terminal<TuiBackend>,
    master: &mut File,
    bytes: &mut Vec<u8>,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        drain_terminal(master, bytes);
        app.draw(terminal).unwrap();
        if app.home.presentation_ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "private App frame did not finish"
        );
    }
    drain_terminal(master, bytes);
}

async fn exercise(kind: Kind, stale: Stale) {
    let root = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(root.path());
    crate::session::get_profile_dir("test").unwrap();
    crate::session::config::update_config(|config| {
        config.session.delete_to_trash = kind != Kind::Permanent;
        config.session.confirm_delete = true;
    })
    .unwrap();
    if kind == Kind::Group {
        crate::session::config::update_app_state(|state| {
            state.group_by = Some(crate::session::config::GroupByMode::Manual);
            state.sort_order = Some(crate::session::config::SortOrder::Newest);
        })
        .unwrap();
    }
    let project = root.path().join("private-project");
    std::fs::create_dir(&project).unwrap();
    let marker = project.join("keep.txt");
    std::fs::write(&marker, b"private resources must survive unseen consent").unwrap();
    let mut row =
        crate::session::Instance::new("Private consent target", project.to_str().unwrap());
    row.source_profile = "test".into();
    row.group_path = "Review".into();
    row.status = crate::session::Status::Stopped;
    // A real scratch option makes a changed permanent-delete choice observable.
    row.scratch = kind == Kind::Permanent;
    let storage = crate::session::Storage::new_unwatched("test").unwrap();
    storage
        .update(|rows, groups| {
            *rows = vec![row.clone()];
            *groups = crate::session::GroupTree::new_with_groups(rows, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
    let snapshot = state.runtime.publish(&state).await.unwrap().value.clone();
    let mut app = App::new(
        "test",
        AvailableTools::with_tools(&["claude"]),
        true,
        false,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    app.home
        .session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
            std::sync::Arc::new(snapshot),
        ));
    app.home.apply_session_feed();
    app.home.selected_session = (kind != Kind::Group).then(|| row.id.clone());
    app.home.selected_group = (kind == Kind::Group).then(|| "Review".into());
    if kind == Kind::Group {
        let (cursor, profile) = app
            .home
            .flat_items
            .iter()
            .enumerate()
            .find_map(|(cursor, item)| match item {
                crate::session::Item::Group { path, profile, .. } if path == "Review" => {
                    Some((cursor, profile.clone()))
                }
                _ => None,
            })
            .expect("manual grouping must expose the stored Review group");
        app.home.cursor = cursor;
        app.home.selected_group_profile = profile;
    }
    let mut dispatched: Box<dyn FnMut() -> bool> = if kind == Kind::Group {
        let mut driver = app.home.session_feed.namespace_driver_for_test();
        Box::new(move || driver(Err("observe private namespace command".into())).is_some())
    } else {
        let mut driver = app.home.session_feed.request_driver_for_test();
        Box::new(move || driver(Err("observe private session command".into())).is_some())
    };
    let before = serde_json::to_value(storage.load().unwrap()).unwrap();
    let config_path = crate::session::config::config_path().unwrap();
    let config_before = std::fs::read(&config_path).unwrap();
    let (mut master, slave) = private_terminal();
    let mut control = slave.try_clone().unwrap();
    let (_output_guard, _) = install_test_output(slave);
    let mut terminal = output_test_terminal();
    let mut emitted = Vec::new();
    present(&mut app, &mut terminal, &mut master, &mut emitted);

    open(&mut app, kind);
    if !matches!(stale, Stale::Unseen) {
        present(&mut app, &mut terminal, &mut master, &mut emitted);
        let title: &[u8] = match kind {
            Kind::Trash => b"Confirm Delete",
            Kind::Permanent => b"Delete Session",
            Kind::Group => b"Delete Group",
        };
        read_terminal_until(&mut master, &mut emitted, title);
    }
    block_terminal(&mut control);
    crate::tui::terminal_output::request_redraw();
    app.draw(&mut terminal).unwrap();
    assert!(app.pending_presentation.is_some());
    assert!(!app.home.presentation_ready);
    match stale {
        Stale::Unseen => {}
        Stale::Replaced => {
            app.home.handle_key(key(KeyCode::Esc), None);
            assert!(
                !dialog_open(&app, kind),
                "Cancel must work while output is blocked"
            );
            open(&mut app, kind);
        }
        Stale::ChangedOptions => {
            if kind == Kind::Trash {
                app.home.handle_key(key(KeyCode::Down), None);
            }
            app.home.handle_key(key(KeyCode::Char(' ')), None);
        }
        Stale::Covered => {
            app.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                "Covering notice",
                "This notice, not a destructive confirmation, is the visible foreground.",
            ));
        }
    }
    if !matches!(stale, Stale::Unseen) {
        resume_terminal(&control);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.pending_presentation.is_some() {
            assert!(std::time::Instant::now() < deadline);
            drain_terminal(&mut master, &mut emitted);
            app.draw(&mut terminal).unwrap();
        }
        if matches!(stale, Stale::Covered) {
            present(&mut app, &mut terminal, &mut master, &mut emitted);
            read_terminal_until(&mut master, &mut emitted, b"Covering notice");
            app.home.handle_key(key(KeyCode::Esc), None);
            assert!(app.home.info_dialog.is_none());
        }
    }
    let submit = if kind == Kind::Group {
        KeyCode::Enter
    } else {
        KeyCode::Char('y')
    };
    app.home.handle_key(key(submit), None);
    assert!(
        !dispatched(),
        "{kind:?}/{stale:?}: unseen consent dispatched a mutation"
    );
    assert_eq!(
        serde_json::to_value(storage.load().unwrap()).unwrap(),
        before
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    assert_eq!(
        std::fs::read(&marker).unwrap(),
        b"private resources must survive unseen consent"
    );
    assert!(
        dialog_open(&app, kind),
        "refused consent must remain safely cancellable"
    );
    app.home.handle_key(key(KeyCode::Esc), None);
    assert!(!dialog_open(&app, kind));
    assert!(!dispatched());

    // A newly opened, actually emitted confirmation must still work normally.
    resume_terminal(&control);
    present(&mut app, &mut terminal, &mut master, &mut emitted);
    open(&mut app, kind);
    crate::tui::terminal_output::request_redraw();
    present(&mut app, &mut terminal, &mut master, &mut emitted);
    if kind == Kind::Trash {
        let yes = app
            .home
            .confirm_dialog
            .as_ref()
            .unwrap()
            .yes_button_area_for_test();
        assert!(app
            .home
            .handle_dialog_click(yes.x + yes.width / 2, yes.y + yes.height / 2));
    } else {
        app.home.handle_key(key(submit), None);
    }
    assert!(
        dispatched(),
        "{kind:?}/{stale:?}: visible confirmation became a dead end"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn home_destructive_consent_trash_requires_presented_semantics() {
    for stale in [
        Stale::Unseen,
        Stale::Replaced,
        Stale::ChangedOptions,
        Stale::Covered,
    ] {
        exercise(Kind::Trash, stale).await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn home_destructive_consent_permanent_delete_requires_presented_semantics() {
    for stale in [
        Stale::Unseen,
        Stale::Replaced,
        Stale::ChangedOptions,
        Stale::Covered,
    ] {
        exercise(Kind::Permanent, stale).await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn home_destructive_consent_group_delete_requires_presented_semantics() {
    for stale in [
        Stale::Unseen,
        Stale::Replaced,
        Stale::ChangedOptions,
        Stale::Covered,
    ] {
        exercise(Kind::Group, stale).await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn home_visible_mouse_discard_restores_the_actual_theme_preview() {
    use crate::tui::settings::SettingsFocus;

    let root = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(root.path());
    crate::session::get_profile_dir("test").unwrap();
    crate::session::config::update_config(|_| {}).unwrap();
    let mut app = App::new(
        "test",
        AvailableTools::with_tools(&["claude"]),
        true,
        false,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    let saved_theme = app.theme_name.clone();
    let config_path = crate::session::config::config_path().unwrap();
    let config_before = std::fs::read(&config_path).unwrap();
    let (mut master, slave) = private_terminal();
    let (_output_guard, _) = install_test_output(slave);
    let mut terminal = output_test_terminal();
    let mut emitted = Vec::new();
    present(&mut app, &mut terminal, &mut master, &mut emitted);
    app.handle_key(key(KeyCode::Char('s')), &mut terminal)
        .await
        .unwrap();

    let categories = app.home.settings_view.as_ref().unwrap().categories.len();
    for _ in 0..categories {
        if app
            .home
            .settings_view
            .as_ref()
            .unwrap()
            .fields
            .iter()
            .any(|f| f.is_theme_name())
        {
            break;
        }
        app.handle_key(key(KeyCode::Down), &mut terminal)
            .await
            .unwrap();
    }
    assert!(app
        .home
        .settings_view
        .as_ref()
        .unwrap()
        .fields
        .iter()
        .any(|f| f.is_theme_name()));
    app.handle_key(key(KeyCode::Enter), &mut terminal)
        .await
        .unwrap();
    assert_eq!(
        app.home.settings_view.as_ref().unwrap().focus,
        SettingsFocus::Fields
    );
    let fields = app.home.settings_view.as_ref().unwrap().fields.len();
    for _ in 0..fields {
        let settings = app.home.settings_view.as_ref().unwrap();
        if settings.fields[settings.selected_field].is_theme_name() {
            break;
        }
        app.handle_key(key(KeyCode::Down), &mut terminal)
            .await
            .unwrap();
    }
    let settings = app.home.settings_view.as_ref().unwrap();
    assert!(settings.fields[settings.selected_field].is_theme_name());
    app.handle_key(key(KeyCode::Enter), &mut terminal)
        .await
        .unwrap();
    assert_ne!(
        app.theme_name, saved_theme,
        "the real theme edit must apply its live preview"
    );
    assert!(app.home.settings_view.as_ref().unwrap().has_changes);
    // Esc leaves field focus first; only Esc from categories requests dirty-settings consent.
    app.handle_key(key(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();
    assert_eq!(
        app.home.settings_view.as_ref().unwrap().focus,
        SettingsFocus::Categories
    );
    assert!(app.home.settings_view.as_ref().unwrap().has_changes);
    app.handle_key(key(KeyCode::Esc), &mut terminal)
        .await
        .unwrap();
    assert!(app.home.settings_close_confirm);
    present(&mut app, &mut terminal, &mut master, &mut emitted);
    read_terminal_until(&mut master, &mut emitted, b"Unsaved Changes");

    let yes = app
        .home
        .confirm_dialog
        .as_ref()
        .unwrap()
        .yes_button_area_for_test();
    assert!(yes.width > 0);
    assert!(app
        .home
        .handle_dialog_click(yes.x + yes.width / 2, yes.y + yes.height / 2));
    assert!(app.home.settings_view.is_none());
    assert!(!app.home.settings_close_confirm);
    let action = app
        .home
        .pending_dialog_click_action
        .take()
        .expect("visible mouse Discard action");
    assert!(matches!(&action, Action::SetTheme(name) if name == &saved_theme));
    app.execute_action(action, &mut terminal).unwrap();
    assert_eq!(app.theme_name, saved_theme);
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
}
