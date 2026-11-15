//! The in-flight creating stub is a sidebar placeholder: the creation belongs
//! to the daemon, and no canonical row or runtime exists yet. Every action that
//! needs a session behind the cursor must refuse it and say so, while actions
//! that do not touch a session stay live.

use super::*;
use crate::tui::dialogs::ContextMenuAction;
use crate::tui::home::CreationConfirmation;

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
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    view.request_creation(
        creation_data(&project_dir, "Pending", "test"),
        None,
        None,
        false,
    );
    let stub = view.creating_stub_id.clone().unwrap();
    let storage = Storage::new_unwatched("default").unwrap();
    let before = payload_bytes("default");
    let creations = crate::session::config::AppStateConfig::load()
        .unwrap()
        .sessions_created;
    view.reload().unwrap();
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
    view.reload().unwrap();
    assert_eq!(payload_bytes("default"), before);
    assert!(!storage.load().unwrap().iter().any(|row| row.id == stub));
    assert!(view.get_instance(&stub).is_none());
    assert_eq!(
        crate::session::config::AppStateConfig::load()
            .unwrap()
            .sessions_created,
        creations,
        "a cancelled display stub is not a committed creation"
    );
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
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    view.request_creation(
        creation_data(&project_dir, "Fenced", "fenced"),
        None,
        None,
        false,
    );
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
    assert!(view.status_flash_text().is_some());

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
    assert!(view.status_flash_text().is_some());

    // A menu whose every entry would refuse the row is never opened. The
    // sidebar geometry is only known once the list rects are set, exactly as
    // `right_click_context_menu` does.
    view.status_flash = None;
    view.list_inner_area = ratatui::layout::Rect::new(1, 1, 28, 10);
    view.list_area = ratatui::layout::Rect::new(0, 0, 30, 12);
    let row = 1 + view.cursor as u16;
    assert!(view.handle_right_click(5, row));
    assert!(view.context_menu.is_none(), "no menu for the creating stub");
    assert!(view.status_flash_text().is_some());

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
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    view.request_creation(
        creation_data(&project_dir, "Delayed", "test"),
        None,
        None,
        false,
    );
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
    let collision = crate::server::api::sessions::session_response_from_instance(&other, false);
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
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    view.request_creation(
        creation_data(&project_dir, "Delayed", "test"),
        None,
        None,
        false,
    );
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
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut driver = view.session_feed.creation_driver_for_test();
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    let mut data = creation_data(&project_dir, "Same title", "group");
    data.profile = "other".into();
    view.request_creation(data, None, None, false);
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
    view.session_feed.publish_for_test(creation_snapshot(
        vec![crate::server::api::sessions::session_response_from_instance(&committed, false)],
        "test",
        2,
    ));
    view.apply_session_feed();
    assert_eq!(view.apply_creation_results(), Some(id.clone()));
    assert_eq!(view.active_profile_display(), Some("other"));
    assert_eq!(view.selected_session.as_deref(), Some(id.as_str()));
    assert!(view.get_instance(&id).is_some());
}
#[tokio::test]
#[serial]
async fn z_on_a_parked_row_unarchives_through_the_feed() {
    let (_temp, _guard) = test_home();
    let mut parked = Instance::new("parked", "/tmp/parked");
    parked.source_profile = "test".into();
    parked.status = Status::Stopped;
    parked.archive();
    seed_profile("test", std::slice::from_ref(&parked));
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    let mut drive = view.session_feed.command_driver_for_test();
    apply_published(&mut view, &state).await;
    view.archived_section_collapsed = false;
    view.flat_items = view.build_flat_items();
    view.select_session_by_id(&parked.id);
    assert_eq!(view.selected_session.as_deref(), Some(parked.id.as_str()));
    let before = payload_bytes("test");
    view.handle_key(key(KeyCode::Char('z')), None);
    assert_eq!(payload_bytes("test"), before);
    assert!(view.get_instance(&parked.id).unwrap().is_archived());
    let (status, reply) = request(
        &state,
        "PATCH",
        &format!("/api/sessions/{}/archive", parked.id),
        serde_json::json!({"archived":false}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let submitted = drive(Ok(serde_json::from_value(reply["cursor"].clone()).unwrap())).unwrap();
    assert_eq!(submitted.0, parked.id);
    assert!(matches!(submitted.1, crate::daemon::SessionMutation::Archive(body) if !body.archived));
    let committed = payload_bytes("test");
    apply_published(&mut view, &state).await;
    assert!(!view.get_instance(&parked.id).unwrap().is_archived());
    assert!(!Storage::new_unwatched("test").unwrap().load().unwrap()[0].is_archived());
    assert_eq!(payload_bytes("test"), committed);
}

fn creation_snapshot(
    rows: Vec<crate::daemon::SessionResponse>,
    epoch: &str,
    revision: u64,
) -> crate::tui::session_feed::SessionFeedResult {
    crate::tui::session_feed::SessionFeedResult::Snapshot(Arc::new(fixture_snapshot(
        rows, "default", epoch, revision,
    )))
}

#[test]
#[serial]
fn creation_receipt_waits_for_applied_snapshot_and_retries_profile_loading_without_losing_confirmation(
) {
    for profile in ["default", "other"] {
        let CreationTestEnv {
            mut view,
            project_dir,
            _guard,
            _temp,
        } = setup_creation_test_env();
        let mut receipt_driver = view.session_feed.creation_receipt_driver_for_test();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        let mut data = creation_data(&project_dir, "Confirmed", "group");
        data.profile = profile.into();
        view.request_creation(data, None, None, false);
        let key = view.creating_stub_id.clone().unwrap();
        let mut committed = Instance::new("Confirmed", project_dir.to_str().unwrap());
        committed.source_profile = profile.into();
        committed.idempotency_key = Some(key.clone());
        committed.status = Status::Idle;
        let row = crate::server::api::sessions::session_response_from_instance(&committed, false);
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(committed.clone());
                Ok(())
            })
            .unwrap();
        // The daemon counted its commit before emitting the receipt; the consumer must not count it again.
        crate::tips::record_session_creations(1);
        let creations = crate::session::config::AppStateConfig::load()
            .unwrap()
            .sessions_created;
        let path = storage.sessions_path().to_path_buf();
        let valid = std::fs::read(&path).unwrap();
        assert_eq!(
            receipt_driver(crate::daemon::MutationReceipt {
                cursor: crate::daemon::RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2
                },
                outcome: row.clone(),
            }),
            key
        );
        assert!(
            view.apply_creation_results().is_none(),
            "receipt alone cannot return an attach id"
        );
        std::fs::write(&path, b"{ invalid json ]").unwrap();
        let unreadable_bytes = std::fs::read(&path).unwrap();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![row], "test", 2));
        view.apply_session_feed();
        assert_eq!(std::fs::read(&path).unwrap(), unreadable_bytes);
        assert!(view.apply_creation_results().is_none());
        assert_eq!(view.creating_stub_id.as_deref(), Some(key.as_str()));
        assert!(view.get_instance(&key).is_some());
        view.info_dialog = None;
        assert!(
            view.apply_creation_results().is_none(),
            "failed profile loads obey backoff"
        );
        assert!(view.info_dialog.is_none());
        std::fs::write(&path, valid).unwrap();
        let cursor = crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision: 2,
        };
        if !view.session_feed.receipt_applied(&cursor) {
            view.session_feed_reload_retry_at = Some(std::time::Instant::now());
            view.apply_session_feed();
        }
        assert!(view.session_feed.receipt_applied(&cursor));
        view.pending_creation.as_mut().unwrap().reload_retry_at = Some(std::time::Instant::now());
        assert_eq!(view.apply_creation_results(), Some(committed.id.clone()));
        assert_eq!(view.active_profile_display(), Some(profile));
        assert_eq!(
            view.selected_session.as_deref(),
            Some(committed.id.as_str())
        );
        assert!(view.get_instance(&key).is_none());
        assert!(view.pending_creation.is_none());
        assert!(view.get_instance(&committed.id).is_some());
        assert!(
            view.apply_creation_results().is_none(),
            "confirmation emits exactly one id"
        );
        assert_eq!(
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .sessions_created,
            creations,
            "receipt retries and repeat ticks never double-count the daemon commit"
        );
    }
}

#[test]
#[serial]
fn retrying_a_snapshot_preserves_one_creation_stub_and_never_resubmits_or_cancels() {
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut driver = view.session_feed.creation_driver_for_test();
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    let mut existing = Instance::new("before", project_dir.to_str().unwrap());
    existing.source_profile = "default".into();
    existing.status = Status::Idle;
    seed_profile("default", std::slice::from_ref(&existing));
    view.reload().unwrap();
    view.session_feed.publish_for_test(creation_snapshot(
        vec![crate::server::api::sessions::session_response_from_instance(&existing, false)],
        "test",
        1,
    ));
    view.apply_session_feed();
    view.request_creation(
        creation_data(&project_dir, "Pending", "group"),
        None,
        None,
        false,
    );
    let key = view.creating_stub_id.clone().unwrap();
    assert_eq!(driver(), vec![format!("create:{key}")]);
    let mut reservation = Instance::new("Pending", project_dir.to_str().unwrap());
    reservation.source_profile = "default".into();
    reservation.idempotency_key = Some(key.clone());
    reservation.status = Status::Creating;
    existing.title = "after".into();
    let storage = Storage::new_unwatched("default").unwrap();
    storage
        .update(|rows, _| {
            *rows = vec![existing.clone(), reservation.clone()];
            Ok(())
        })
        .unwrap();
    let path = storage.sessions_path().to_path_buf();
    let valid = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{ invalid json ]").unwrap();
    let unreadable_bytes = std::fs::read(&path).unwrap();
    view.session_feed.publish_for_test(creation_snapshot(
        vec![
            crate::server::api::sessions::session_response_from_instance(&existing, false),
            crate::server::api::sessions::session_response_from_instance(&reservation, false),
        ],
        "test",
        2,
    ));
    view.apply_session_feed();
    assert_eq!(std::fs::read(&path).unwrap(), unreadable_bytes);
    assert_eq!(view.in_flight_creation_id(), Some(reservation.id.as_str()));
    assert!(view.get_instance(&key).is_some());
    assert_eq!(view.session_feed.next_revision_for_test(), 2);
    std::fs::write(&path, valid).unwrap();
    view.session_feed_reload_retry_at = Some(std::time::Instant::now());
    view.apply_session_feed();
    assert_eq!(view.session_feed.next_revision_for_test(), 3);
    assert_eq!(view.get_instance(&existing.id).unwrap().title, "after");
    assert_eq!(view.creating_stub_id.as_deref(), Some(key.as_str()));
    assert_eq!(
        view.instances
            .values()
            .filter(|instance| instance.id == key)
            .count(),
        1
    );
    assert!(view.get_instance(&reservation.id).is_none());
    assert!(
        driver().is_empty(),
        "replay submits neither another creation nor cancellation"
    );
    assert!(!view.apply_session_feed());
}

#[tokio::test]
#[serial]
async fn unknown_creation_uses_current_canonical_authority_after_reconnection_not_cached_rows() {
    let CreationTestEnv {
        mut view,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let mut driver = view.session_feed.creation_driver_for_test();
    view.session_feed
        .publish_for_test(creation_snapshot(vec![], "test", 1));
    view.apply_session_feed();
    let mut data = creation_data(&project_dir, "Unknown", "group");
    data.profile = "other".into();
    view.request_creation(data, None, None, false);
    let key = view.creating_stub_id.clone().unwrap();
    assert_eq!(driver(), vec![format!("create:{key}")]);
    assert!(view.apply_creation_results().is_none());
    let mut committed = Instance::new("Unknown", project_dir.to_str().unwrap());
    committed.source_profile = "other".into();
    committed.idempotency_key = Some(key.clone());
    committed.status = Status::Idle;
    let storage = Storage::new_unwatched("other").unwrap();
    storage
        .update(|rows, _| {
            rows.push(committed.clone());
            Ok(())
        })
        .unwrap();
    let path = storage.sessions_path().to_path_buf();
    let valid = std::fs::read(&path).unwrap();
    let row = crate::server::api::sessions::session_response_from_instance(&committed, false);
    std::fs::write(&path, b"{ invalid json ]").unwrap();
    let unreadable_bytes = std::fs::read(&path).unwrap();
    view.session_feed
        .publish_for_test(creation_snapshot(vec![row.clone()], "old", 2));
    view.apply_session_feed();
    assert_eq!(std::fs::read(&path).unwrap(), unreadable_bytes);
    assert!(view.apply_creation_results().is_none());
    assert!(matches!(
        view.pending_creation.as_ref().unwrap().confirmation,
        Some(CreationConfirmation::Canonical(_))
    ));
    view.session_feed.set_mutation_permission_for_test(false);
    view.session_feed.set_native_permission_for_test(false);
    view.session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Unavailable(
            "disconnected".into(),
        ));
    view.apply_session_feed();
    std::fs::write(&path, valid).unwrap();
    view.pending_creation.as_mut().unwrap().reload_retry_at = Some(std::time::Instant::now());
    assert!(
        view.apply_creation_results().is_none(),
        "the old applied row is not authority while unavailable"
    );
    assert!(view.is_creation_pending());
    view.connect_runtime();
    // Replace the transport before yielding; all Home reconnection/reset behavior has run.
    view.session_feed = crate::tui::session_feed::SessionFeed::new();
    let mut reconnected = view.session_feed.creation_driver_for_test();
    view.session_feed
        .publish_for_test(creation_snapshot(vec![row], "new", 1));
    view.apply_session_feed();
    assert_eq!(view.apply_creation_results(), Some(committed.id.clone()));
    assert!(
        reconnected().is_empty(),
        "canonical reconciliation submits no create/cancel request"
    );
    assert!(view.apply_creation_results().is_none());
    assert!(view.get_instance(&key).is_none());
}

/// A creation refused by its on_create hook keeps the submitted request behind a dialog:
/// Retry and Create without setup resend it under a new key (the daemon replays a failed
/// key), the latter with the hooks skipped, and Cancel drops it. Any other refusal is the
/// plain "Creation Failed" dialog.
#[test]
#[serial]
fn a_hook_failure_offers_retry_create_without_setup_or_cancel() {
    use crate::tui::session_feed::CommandFailure;
    let hook = || CommandFailure::CreateHookFailed("on_create hook failed: exit 3".into());
    // (first failure, key in the dialog, resubmitted with skip_on_create)
    for (failure, key_code, resubmit) in [
        (hook(), Some(KeyCode::Enter), Some(false)),
        (hook(), Some(KeyCode::Char('s')), Some(true)),
        (hook(), Some(KeyCode::Esc), None),
        (CommandFailure::Rejected("denied".into()), None, None),
    ] {
        let case = format!("{failure:?} {key_code:?}");
        let CreationTestEnv {
            mut view,
            project_dir,
            _guard,
            _temp,
        } = setup_creation_test_env();
        let mut fail = view.session_feed.creation_failure_driver_for_test();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        view.request_creation(
            creation_data(&project_dir, "Hooked", "test"),
            Some(true),
            None,
            false,
        );
        let first = fail(failure);
        assert!(view.apply_creation_results().is_none());
        assert!(view.creating_stub_id.is_none(), "{case}: the stub is gone");

        let Some(key_code) = key_code else {
            assert!(view.create_hook_failed.is_none(), "{case}");
            assert_eq!(
                view.info_dialog.as_ref().map(|d| d.title()),
                Some("Creation Failed")
            );
            continue;
        };
        assert!(view.info_dialog.is_none(), "{case}");
        let screen = render_home_to_string(&mut view, 100, 30);
        assert!(screen.contains("Session setup failed"), "{case}\n{screen}");
        assert!(
            screen.contains("on_create hook failed: exit 3"),
            "{case}\n{screen}"
        );
        assert!(
            screen.contains("[Create without setup (s)]"),
            "{case}\n{screen}"
        );

        view.handle_key(key(key_code), None);
        assert!(
            view.create_hook_failed.is_none(),
            "{case}: the dialog closes"
        );
        let Some(skip) = resubmit else {
            assert!(!view.is_creation_pending(), "{case}: Cancel sends nothing");
            assert!(view.creating_stub_id.is_none(), "{case}");
            continue;
        };
        assert!(view.creating_stub_id.is_some(), "{case}: a new stub");
        let second = fail(CommandFailure::Rejected("done".into()));
        assert_ne!(second.idempotency_key, first.idempotency_key, "{case}");
        assert_eq!(second.skip_on_create, skip, "{case}");
        let same = |body: &crate::daemon::CreateSessionBody| {
            let mut value = serde_json::to_value(body).unwrap();
            value["idempotency_key"] = serde_json::Value::Null;
            value["skip_on_create"] = serde_json::Value::Null;
            value
        };
        assert_eq!(same(&second), same(&first), "{case}: the same request");
        assert_eq!(second.trust_hooks, Some(true), "{case}: trust is kept");
    }
}

/// A resubmit refused before it is sent reopens the recovery dialog with the same request,
/// beneath the refusal, so Retry still works once the cause clears.
#[test]
#[serial]
fn a_refused_resubmit_keeps_the_recovery_dialog_and_its_request() {
    use crate::tui::session_feed::CommandFailure;
    for case in ["already pending", "daemon unavailable"] {
        let CreationTestEnv {
            mut view,
            project_dir,
            _guard,
            _temp,
        } = setup_creation_test_env();
        let mut fail = view.session_feed.creation_failure_driver_for_test();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        view.request_creation(
            creation_data(&project_dir, "Hooked", "test"),
            Some(true),
            None,
            false,
        );
        let first = fail(CommandFailure::CreateHookFailed("exit 3".into()));
        assert!(view.apply_creation_results().is_none());
        assert!(view.create_hook_failed.is_some(), "{case}");
        let kept_title = |view: &HomeView| {
            view.create_hook_failed
                .as_ref()
                .map(|(_, request)| request.data.title.clone())
        };

        if case == "already pending" {
            view.request_creation(
                creation_data(&project_dir, "Other", "test"),
                None,
                None,
                false,
            );
            assert!(view.is_creation_pending(), "{case}");
            let stub = view.creating_stub_id.clone();
            view.handle_key(key(KeyCode::Enter), None);
            assert_eq!(kept_title(&view).as_deref(), Some("Hooked"), "{case}");
            assert_eq!(
                view.status_flash_text(),
                Some("A session is already being created"),
                "{case}"
            );
            assert_eq!(view.creating_stub_id, stub, "{case}: nothing new sent");
            continue;
        }

        view.session_feed.set_mutation_permission_for_test(false);
        view.handle_key(key(KeyCode::Char('s')), None);
        assert!(view.creating_stub_id.is_none(), "{case}: nothing sent");
        assert_eq!(kept_title(&view).as_deref(), Some("Hooked"), "{case}");
        assert_eq!(
            view.info_dialog.as_ref().map(|d| d.title()),
            Some("Cannot Create Session"),
            "{case}"
        );
        let screen = render_home_to_string(&mut view, 100, 30);
        assert!(
            screen.contains("The local daemon is unavailable"),
            "{case}: the refusal is drawn on top\n{screen}"
        );
        // The refusal is read first, then the dialog takes keys again.
        view.handle_key(key(KeyCode::Esc), None);
        assert!(view.info_dialog.is_none(), "{case}");
        assert_eq!(kept_title(&view).as_deref(), Some("Hooked"), "{case}");
        view.session_feed.set_mutation_permission_for_test(true);
        view.handle_key(key(KeyCode::Char('s')), None);
        assert!(view.create_hook_failed.is_none(), "{case}");
        let second = fail(CommandFailure::Rejected("done".into()));
        assert!(second.skip_on_create, "{case}");
        assert_ne!(second.idempotency_key, first.idempotency_key, "{case}");
    }
}

/// The daemon refuses a trust review that no longer matches, so a resubmit reviews again.
/// Unchanged, it resends; a repo hook changed since the refusal reopens the trust prompt on
/// the kept request; a changed global hook with no repo hooks is pinned without a prompt.
#[test]
#[serial]
fn a_resubmit_reviews_trust_again_once_the_hooks_change() {
    use crate::tui::home::input::creation_trust_review;
    use crate::tui::session_feed::CommandFailure;
    let write_repo_hook = |project: &std::path::Path, command: &str| {
        let dir = project.join(".agent-of-empires");
        std::fs::create_dir_all(&dir).unwrap();
        let body = format!("[hooks]\non_create = [\"{command}\"]\n");
        std::fs::write(dir.join("config.toml"), body).unwrap();
    };
    // (what changes after the refusal, the dialog key, whether the trust prompt reopens)
    for (change, key_code, prompted) in [
        ("nothing", KeyCode::Enter, false),
        ("repo hook", KeyCode::Char('s'), true),
        ("global hook", KeyCode::Enter, false),
    ] {
        let CreationTestEnv {
            mut view,
            project_dir,
            _guard,
            _temp,
        } = setup_creation_test_env();
        let mut fail = view.session_feed.creation_failure_driver_for_test();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        let repo = change != "global hook";
        if repo {
            write_repo_hook(&project_dir, "exit 3");
        }
        let data = || creation_data(&project_dir, "Hooked", "test");
        let (_, first_review) = creation_trust_review(&data()).unwrap();
        let trust_hooks = repo.then_some(true);
        view.request_creation(data(), trust_hooks, Some(first_review.clone()), false);
        let first = fail(CommandFailure::CreateHookFailed("exit 3".into()));
        assert!(view.apply_creation_results().is_none());
        assert!(view.create_hook_failed.is_some(), "{change}");

        match change {
            "repo hook" => write_repo_hook(&project_dir, "echo fixed"),
            "global hook" => crate::session::config::update_config(|config| {
                config.hooks.on_create = vec!["echo global".into()]
            })
            .unwrap(),
            _ => {}
        }
        if change == "global hook" {
            // A resend refused before it is sent keeps the dialog, now with the new review.
            view.session_feed.set_mutation_permission_for_test(false);
            view.handle_key(key(key_code), None);
            let kept = view
                .create_hook_failed
                .as_ref()
                .map(|(_, r)| r.trust_review.clone());
            let (_, now) = creation_trust_review(&data()).unwrap();
            assert_eq!(kept, Some(Some(now)), "{change}");
            view.handle_key(key(KeyCode::Esc), None);
            view.session_feed.set_mutation_permission_for_test(true);
        }
        view.handle_key(key(key_code), None);
        assert!(view.create_hook_failed.is_none(), "{change}");
        if prompted {
            assert!(
                view.creating_stub_id.is_none(),
                "{change}: nothing sent yet"
            );
            let screen = render_home_to_string(&mut view, 120, 40);
            assert!(
                screen.contains("echo fixed"),
                "{change}: the new hook\n{screen}"
            );
            let kept = view
                .pending_repo_trust_data
                .as_ref()
                .map(|d| d.title.as_str());
            assert_eq!(kept, Some("Hooked"), "{change}");
            view.handle_key(key(KeyCode::Char('y')), None);
        }
        let second = fail(CommandFailure::Rejected("done".into()));
        let (_, now) = creation_trust_review(&data()).unwrap();
        assert_eq!(second.trust_review.as_ref(), Some(&now), "{change}");
        assert_eq!(now != first_review, change != "nothing", "{change}");
        assert_eq!(second.trust_hooks, trust_hooks, "{change}");
        assert_eq!(
            second.skip_on_create,
            key_code == KeyCode::Char('s'),
            "{change}"
        );
        assert_ne!(second.idempotency_key, first.idempotency_key, "{change}");
    }
}

/// The session-setup dialog has no text input: a paste while it is open is dropped, not
/// routed into a compose dialog over it or saved for whichever dialog opens next. The
/// settings view takes keys before the dialog, so a paste into it still lands there.
#[test]
#[serial]
fn a_paste_over_the_setup_failed_dialog_goes_nowhere() {
    use crate::tui::session_feed::CommandFailure;
    for settings_on_top in [false, true] {
        let CreationTestEnv {
            mut view,
            project_dir,
            _guard,
            _temp,
        } = setup_creation_test_env();
        let mut fail = view.session_feed.creation_failure_driver_for_test();
        view.session_feed
            .publish_for_test(creation_snapshot(vec![], "test", 1));
        view.apply_session_feed();
        // A runnable session for a stray paste to land in.
        let mut running = Instance::new("Running", project_dir.to_str().unwrap());
        running.status = Status::Running;
        running.source_profile = "default".to_string();
        let running_id = running.id.clone();
        view.add_instance(running);
        view.request_creation(
            creation_data(&project_dir, "Hooked", "test"),
            None,
            None,
            false,
        );
        fail(CommandFailure::CreateHookFailed("exit 3".into()));
        assert!(view.apply_creation_results().is_none());
        assert!(view.create_hook_failed.is_some());
        view.flat_items = view.build_flat_items();
        view.cursor = view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &running_id))
            .expect("the running row");
        view.update_selected();
        // The terminal target needs no live tmux pane, so a stray paste would open compose.
        view.view_mode = crate::tui::home::ViewMode::Terminal;
        if settings_on_top {
            view.settings_view =
                Some(crate::tui::settings::SettingsView::new("default", None).unwrap());
            view.handle_key(key(KeyCode::Char('/')), None);
        }

        view.handle_paste("zqxpasted");
        let case = format!("settings on top: {settings_on_top}");
        assert!(
            view.send_message_dialog.is_none(),
            "{case}: no compose dialog opens"
        );
        assert_eq!(view.pending_paste, None, "{case}: nothing is kept");
        assert!(
            view.create_hook_failed.is_some(),
            "{case}: the dialog stays"
        );
        assert_eq!(
            render_home_to_string(&mut view, 120, 40).contains("zqxpasted"),
            settings_on_top,
            "{case}: only the settings search shows the paste"
        );
    }
}
