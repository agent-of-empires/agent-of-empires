//! The in-flight creating stub is a sidebar placeholder: the creation belongs
//! to the daemon, and no canonical row or runtime exists yet. Every action that
//! needs a session behind the cursor must refuse it and say so, while actions
//! that do not touch a session stay live.

use super::*;
use crate::tui::dialogs::ContextMenuAction;

const REFUSAL: &str = "Still creating this session; it has no runtime yet";

#[test]
#[serial]
fn saving_during_creation_never_persists_the_display_placeholder() {
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let _driver = view.session_feed.command_driver_for_test();
    view.request_creation(creation_data(&project_dir, "Pending", "test"), None);
    let stub = view.creating_stub_id.clone().unwrap();
    view.save().unwrap();
    let storage = Storage::new_unwatched("default").unwrap();
    assert!(!storage.load().unwrap().iter().any(|row| row.id == stub));
    assert!(
        view.get_instance(&stub).is_some(),
        "saving must preserve the pending display"
    );
    view.reload().unwrap();
    assert!(
        view.get_instance(&stub).is_some(),
        "reload must preserve the pending display"
    );
    view.cancel_creation();
    view.save().unwrap();
    assert!(!storage.load().unwrap().iter().any(|row| row.id == stub));
    assert!(view.get_instance(&stub).is_none());
}

#[test]
#[serial]
fn session_actions_refuse_the_creating_stub() {
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    // Structured view so a delete would otherwise open its dialog here, making
    // the refusal the fence's doing rather than the view-mode guard's.
    view.view_mode = ViewMode::Structured;
    // The wizard refuses without a runtime that can take the creation, so drive
    // the feed's command lane the way a connected daemon would.
    let _driver = view.session_feed.command_driver_for_test();
    view.request_creation(creation_data(&project_dir, "Fenced", "fenced"), None);
    assert!(
        view.creating_stub_id.is_some(),
        "request_creation installs the stub"
    );
    let stub = view.creating_stub_id.clone().expect("stub id");
    assert!(view.is_creating_stub_selected(), "the stub is selected");

    // The keyboard path: 'd' and 'r' resolve to Delete and Rename, and both
    // must refuse the stub. The binding is only live once the list has focus,
    // which it does here.
    view.status_flash = None;
    assert!(view.handle_key(key(KeyCode::Char('d')), None).is_none());
    assert!(
        view.unified_delete_dialog.is_none(),
        "delete opened for a session the daemon has not published"
    );
    assert!(view.handle_key(key(KeyCode::Char('r')), None).is_none());
    assert!(
        view.rename_dialog.is_none(),
        "rename opened for a session the daemon has not published"
    );
    assert_eq!(view.status_flash_text(), Some(REFUSAL));

    // Non-session actions stay live, and the stub remains selectable so its
    // own preview keeps working.
    view.handle_key(key(KeyCode::Char('?')), None);
    assert!(
        view.show_help,
        "a non-session action still runs on the stub"
    );
    assert!(view.is_creating_stub_selected());
    view.show_help = false;

    // The context menu is the second dispatcher for the same actions.
    view.status_flash = None;
    view.dispatch_context_menu_action(ContextMenuAction::Rename);
    assert!(view.rename_dialog.is_none());
    assert_eq!(view.status_flash_text(), Some(REFUSAL));

    // A menu whose every entry would refuse the row is never opened. The
    // sidebar geometry is only known once the list rects are set, exactly as
    // `right_click_context_menu` does.
    view.status_flash = None;
    view.list_inner_area = ratatui::layout::Rect::new(1, 1, 28, 10);
    view.list_area = ratatui::layout::Rect::new(0, 0, 30, 12);
    let row = 1 + view.cursor as u16;
    assert!(view.handle_right_click(5, row));
    assert!(view.context_menu.is_none(), "no menu for the creating stub");
    assert_eq!(view.status_flash_text(), Some(REFUSAL));

    // The placeholder is the daemon's; cancelling drops it at once, while the
    // daemon still owes the creation its rollback.
    assert!(
        view.is_creation_pending(),
        "the creation is still in flight"
    );
    view.cancel_creation();
    assert!(view.creating_stub_id.is_none());
    assert!(
        view.get_instance(&stub).is_none(),
        "the placeholder is gone"
    );
    assert!(
        view.pending_creation
            .as_ref()
            .is_some_and(|pending| pending.cancel_requested),
        "the cancellation is recorded while the daemon finishes the phase"
    );
}

/// A cancellation asked for before the daemon has named the creation must still
/// reach it: the placeholder disappears at once, and the request is delivered as
/// soon as progress identifies the daemon's session.
#[test]
#[serial]
fn a_cancellation_before_the_daemon_names_the_creation_is_delivered_later() {
    use crate::daemon::{CreationPhase, CreationProgress};

    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut driven = view.session_feed.creation_driver_for_test();
    view.request_creation(creation_data(&project_dir, "Delayed", "test"), None);
    let stub = view
        .creating_stub_id
        .clone()
        .expect("the placeholder is installed");
    assert_eq!(driven(), vec![format!("create:{stub}")]);
    assert!(view.is_creation_pending());

    // Cancel before the daemon publishes anything.
    view.cancel_creation();
    assert!(
        view.get_instance(&stub).is_none(),
        "the placeholder goes away immediately"
    );
    assert!(
        view.is_creation_pending(),
        "the daemon still owes this creation a cancellation"
    );
    assert!(
        driven().is_empty(),
        "nothing can be addressed until the daemon names the session"
    );

    let mut other = Instance::new("Delayed", project_dir.to_str().unwrap());
    other.source_profile = "default".into();
    let collision = crate::daemon::SessionResponse::from_instance(&other, false);
    assert!(!view.reconcile_in_flight_creation(&[collision]));
    assert!(view.pending_creation.as_ref().unwrap().daemon_id.is_none());
    let progress = CreationProgress {
        session_id: "daemon-session".into(),
        request_key: Some(stub.clone()),
        title: "Delayed".into(),
        profile: "default".into(),
        phase: CreationPhase::CreateHooks,
        command: None,
        output: Vec::new(),
        cancelled: false,
    };
    view.session_feed
        .publish_progress_for_test(vec![CreationProgress {
            session_id: "other-session".into(),
            request_key: Some("another-request".into()),
            ..progress.clone()
        }]);
    assert!(!view.apply_creation_progress());
    assert!(driven().is_empty());
    view.session_feed
        .publish_progress_for_test(vec![progress.clone()]);
    assert!(view.apply_creation_progress());
    assert_eq!(driven(), vec!["cancel:daemon-session".to_string()]);
    view.session_feed.publish_progress_for_test(vec![progress]);
    view.apply_creation_progress();
    assert!(
        driven().is_empty(),
        "a repeated progress frame must not retry cancellation"
    );
}

#[test]
#[serial]
fn refused_create_after_early_cancel_settles_without_a_stub_or_daemon_id() {
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut reject = view.session_feed.creation_rejection_driver_for_test();
    view.request_creation(creation_data(&project_dir, "Delayed", "test"), None);
    let key = view.creating_stub_id.clone().unwrap();
    view.cancel_creation();
    assert!(view.creating_stub_id.is_none());
    assert_eq!(reject(), key);
    assert!(view.apply_creation_results().is_none());
    assert!(
        !view.is_creation_pending(),
        "the refusal must settle the cancelled request"
    );
}

#[test]
#[serial]
fn unknown_create_outcome_waits_for_matching_canonical_row_across_profiles() {
    use crate::daemon::{
        RuntimeCapabilities, RuntimeContents, RuntimeCursor, RuntimeHealth, RuntimeSnapshot,
    };
    use crate::tui::session_feed::SessionFeedResult;
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut driver = view.session_feed.creation_driver_for_test();
    let mut data = creation_data(&project_dir, "Same title", "group");
    data.profile = "other".into();
    view.request_creation(data, None);
    let key = view.creating_stub_id.clone().unwrap();
    assert_eq!(driver(), vec![format!("create:{key}")]);
    assert!(view.apply_creation_results().is_none());
    assert!(view.pending_creation.as_ref().unwrap().outcome_unknown);
    assert!(
        view.get_instance(&key).is_some(),
        "an unknown outcome must keep its placeholder"
    );

    let mut committed = Instance::new("Same title", project_dir.to_str().unwrap());
    committed.source_profile = "other".into();
    committed.idempotency_key = Some(key);
    committed.status = Status::Idle;
    let id = committed.id.clone();
    Storage::new_unwatched("other")
        .unwrap()
        .update(|rows, _| {
            rows.push(committed.clone());
            Ok(())
        })
        .unwrap();
    let snapshot = RuntimeSnapshot {
        cursor: RuntimeCursor {
            epoch: "test".into(),
            revision: 2,
        },
        contents: RuntimeContents {
            health: RuntimeHealth::Healthy,
            capabilities: RuntimeCapabilities {
                mutations: true,
                native_interaction: true,
            },
            default_profile: "default".into(),
            sessions: vec![crate::daemon::SessionResponse::from_instance(
                &committed, false,
            )],
            profiles: Vec::new(),
            workspace_ordering: Vec::new(),
            global_projects: Vec::new(),
        },
    };
    view.session_feed
        .publish_for_test(SessionFeedResult::Snapshot(std::sync::Arc::new(snapshot)));
    view.apply_session_feed();
    assert_eq!(view.apply_creation_results(), Some(id.clone()));
    assert_eq!(view.active_profile_display(), Some("other"));
    assert_eq!(view.selected_session.as_deref(), Some(id.as_str()));
    assert!(view.get_instance(&id).is_some());
}
/// `z` on a row parked inside the expanded Archived section must submit the
/// unarchive through the native lane, exactly as it does for the active row.
#[test]
#[serial]
fn z_on_a_parked_row_unarchives_through_the_feed() {
    use crate::daemon::{RuntimeCursor, SessionMutation};

    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    let parked = env.view.instance_at(1).id.clone();
    env.view.select_session_by_id(&parked);
    with_canonical_archive(&mut env, |env| {
        env.view.toggle_archive_at_cursor().unwrap();
    });
    assert!(env.view.get_instance(&parked).unwrap().is_archived());

    // Navigate the way the e2e does: down to the section header, expand, down
    // onto the parked row.
    env.view.handle_key(key(KeyCode::Char('j')), None);
    env.view.handle_key(key(KeyCode::Char('l')), None);
    env.view.handle_key(key(KeyCode::Char('j')), None);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(parked.as_str()),
        "the parked row should be selected before pressing z"
    );

    let mut respond = env.view.session_feed.command_driver_for_test();
    env.view.handle_key(key(KeyCode::Char('z')), None);
    let submitted = respond(Ok(RuntimeCursor {
        epoch: "test".into(),
        revision: 3,
    }));
    match submitted {
        Some((id, SessionMutation::Archive(body))) => {
            assert_eq!(id, parked);
            assert!(!body.archived, "parked row must submit an unarchive");
        }
        _ => panic!("parked row must submit an unarchive through the feed"),
    }
}
