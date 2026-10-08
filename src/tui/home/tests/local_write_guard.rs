use super::super::GroupRenameContext;
use super::*;

fn rename_group(view: &mut HomeView) -> anyhow::Result<()> {
    view.group_rename_context = Some(GroupRenameContext {
        old_path: "work".into(),
        old_profile: "test".into(),
    });
    view.rename_selected_group(Some("renamed"), None)
}

#[tokio::test]
#[serial]
async fn native_group_rpc_admission_waits_for_real_commit_and_applied_snapshot() {
    for snapshot_first in [false, true] {
        let (_temp, _guard) = test_home();
        let mut row = instance_in("alpha", "/tmp/work", "work");
        row.source_profile = "test".into();
        row.status = Status::Stopped;
        seed_profile("test", std::slice::from_ref(&row));
        let state = native_state(&["test"]).await;
        let mut view = test_view(Some("test"));
        let mut drive = view.session_feed.namespace_driver_for_test();
        apply_published(&mut view, &state).await;
        let before = payload_bytes("test");
        rename_group(&mut view).unwrap();
        assert_eq!(payload_bytes("test"), before);
        assert_eq!(view.get_instance(&row.id).unwrap().group_path, "work");
        let (status, reply) = request(&state,"PATCH","/api/groups",serde_json::json!({"source":{"profile":"test","path":"work"},"target":{"profile":"test","path":"renamed"}})).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let committed = payload_bytes("test");
        assert_ne!(committed, before);
        assert_eq!(
            Storage::new_unwatched("test").unwrap().load().unwrap()[0].group_path,
            "renamed"
        );
        if snapshot_first {
            apply_published(&mut view, &state).await;
        }
        let submitted = drive(Ok(crate::daemon::MutationReceipt {
            cursor: serde_json::from_value(reply["cursor"].clone()).unwrap(),
            outcome: crate::daemon::NamespaceOutcome::Committed,
        }))
        .expect("admitted group move");
        let crate::daemon::NamespaceMutation::MoveGroup(body) = submitted else {
            panic!("expected group move")
        };
        assert_eq!(body.source.profile, "test");
        assert_eq!(body.source.path, "work");
        assert_eq!(body.target.profile, "test");
        assert_eq!(body.target.path, "renamed");
        view.apply_session_feed();
        if !snapshot_first {
            assert_eq!(view.get_instance(&row.id).unwrap().group_path, "work");
            apply_published(&mut view, &state).await;
        }
        assert_eq!(view.get_instance(&row.id).unwrap().group_path, "renamed");
        assert!(view.pending_namespace_intent.is_none());
        view.reload().unwrap();
        assert_eq!(payload_bytes("test"), committed);
    }
}

#[tokio::test]
#[serial]
async fn local_unapplied_readonly_and_lost_authority_never_write_rows_or_groups() {
    for mode in ["local", "unapplied", "readonly", "lost"] {
        for action in ["group", "workdir", "attach", "trash"] {
            if mode == "unapplied" && action != "group" {
                continue;
            }
            let (_temp, _guard) = test_home();
            let mut row = instance_in("alpha", "/tmp/work", "work");
            row.source_profile = "test".into();
            row.status = Status::Stopped;
            seed_profile("test", std::slice::from_ref(&row));
            let state = native_state(&["test"]).await;
            let mut view = test_view(Some("test"));
            let mut namespace =
                (action == "group").then(|| view.session_feed.namespace_driver_for_test());
            let mut session =
                (action != "group").then(|| view.session_feed.request_driver_for_test());
            if mode != "local" && mode != "unapplied" {
                apply_published(&mut view, &state).await;
            }
            if mode == "unapplied" {
                let snapshot = state.runtime.publish(&state).await.unwrap();
                view.session_feed.publish_for_test(
                    crate::tui::session_feed::SessionFeedResult::Snapshot(std::sync::Arc::new(
                        snapshot.value.clone(),
                    )),
                );
            }
            if mode == "local" {
                view.session_feed.set_mutation_permission_for_test(false);
            }
            if mode == "readonly" {
                view.session_feed.set_mutation_permission_for_test(false);
                let snapshot = state.runtime.publish(&state).await.unwrap();
                let mut snapshot = snapshot.value.clone();
                snapshot.cursor.revision += 1;
                snapshot.contents.capabilities.mutations = false;
                view.session_feed.publish_for_test(
                    crate::tui::session_feed::SessionFeedResult::Snapshot(std::sync::Arc::new(
                        snapshot,
                    )),
                );
                view.apply_session_feed();
            }
            if mode == "lost" {
                view.session_feed.set_mutation_permission_for_test(false);
                view.session_feed.publish_for_test(
                    crate::tui::session_feed::SessionFeedResult::Unavailable("offline".into()),
                );
                view.apply_session_feed();
            }
            let before = payload_bytes("test");
            view.selected_session = Some(row.id.clone());
            match action {
                "group" => assert!(rename_group(&mut view).is_err(), "{mode}"),
                "workdir" => assert!(
                    view.set_worktree_name_for_selected("renamed", false)
                        .is_err(),
                    "{mode}"
                ),
                "attach" => assert!(
                    view.add_project_to_session(&row.id, std::path::Path::new("/tmp/other"))
                        .is_err(),
                    "{mode}"
                ),
                "trash" => view.trash_session_by_id(&row.id),
                _ => unreachable!(),
            }
            if let Some(driver) = session.as_mut() {
                assert!(
                    driver(Err("unexpected admission".into())).is_none(),
                    "{mode}:{action}"
                );
            }
            if let Some(driver) = namespace.as_mut() {
                assert!(
                    driver(Err("unexpected admission".into())).is_none(),
                    "{mode}:{action}"
                );
            }
            assert!(!view.get_instance(&row.id).unwrap().is_trashed());
            assert_eq!(view.get_instance(&row.id).unwrap().group_path, "work");
            assert_eq!(payload_bytes("test"), before, "{mode}");
            view.reload().unwrap();
            assert_eq!(payload_bytes("test"), before, "{mode}");
        }
    }
}

#[tokio::test]
#[serial]
async fn readonly_trash_restore_bulk_purge_and_force_remove_leave_persisted_owner_intact() {
    let (_temp, _guard) = test_home();
    let mut row = instance_in("alpha", "/tmp/work", "work");
    row.source_profile = "test".into();
    row.status = Status::Stopped;
    row.trash();
    row.lifecycle_generation = 9;
    row.lifecycle_reservation = Some(LifecycleReservation {
        op: LifecycleOperation::Purge,
        generation: 9,
        at: chrono::Utc::now(),
    });
    seed_profile("test", std::slice::from_ref(&row));
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    let mut drive = view.session_feed.request_driver_for_test();
    view.session_feed.set_mutation_permission_for_test(false);
    view.session_feed.set_native_permission_for_test(false);
    let mut snapshot = state.runtime.publish(&state).await.unwrap().value.clone();
    snapshot.contents.capabilities.mutations = false;
    view.session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
            std::sync::Arc::new(snapshot),
        ));
    view.apply_session_feed();
    view.selected_session = Some(row.id.clone());
    let before = payload_bytes("test");
    view.restore_selected_from_trash();
    view.empty_trash_all();
    assert!(view
        .force_remove_session(&row.id, std::num::NonZeroU64::new(9).unwrap())
        .is_err());
    assert!(drive(Err("unexpected admission".into())).is_none());
    assert!(view.get_instance(&row.id).unwrap().is_trashed());
    assert_eq!(
        view.get_instance(&row.id).unwrap().lifecycle_reservation,
        row.lifecycle_reservation
    );
    assert_eq!(payload_bytes("test"), before);
}

#[tokio::test]
#[serial]
async fn cityhall_refuses_profile_creation_without_creating_a_directory() {
    let (_temp, _guard) = test_home();
    seed_profile("test", &[]);
    let state = crate::server::test_support::build_test_app_state_with_policy_configured(
        Vec::new(),
        vec!["localhost".into()],
        Vec::new(),
        None,
        |state| state.cityhall_mode = true,
    );
    crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
    let mut view = test_view(Some("test"));
    let mut drive = view.session_feed.namespace_driver_for_test();
    apply_published(&mut view, &state).await;
    view.session_feed.set_cityhall_for_test(true);
    view.show_profile_picker();
    view.handle_key(key(KeyCode::Char('n')), None);
    for character in "cityhall-target".chars() {
        view.handle_key(key(KeyCode::Char(character)), None);
    }
    view.handle_key(key(KeyCode::Enter), None);
    let path = crate::session::get_app_dir()
        .unwrap()
        .join("profiles/cityhall-target");
    assert!(!path.exists());
    let (status, reply) = request(
        &state,
        "POST",
        "/api/profiles",
        serde_json::json!({"name":"cityhall-target"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let submitted =
        drive(Err(reply.to_string())).expect("profile RPC admitted without local side effects");
    let crate::daemon::NamespaceMutation::Profile(crate::daemon::ProfileMutation::Create(body)) =
        submitted
    else {
        panic!("expected create profile")
    };
    assert_eq!(body.name, "cityhall-target");
    view.apply_session_feed();
    assert!(!path.exists());
    assert!(view.info_dialog.is_some());
}

#[tokio::test]
#[serial]
async fn cityhall_group_policy_rejects_nonstructured_or_cross_profile_but_allows_structured_same_profile(
) {
    for (structured, cross_profile, expected) in [
        (false, false, axum::http::StatusCode::FORBIDDEN),
        (true, true, axum::http::StatusCode::FORBIDDEN),
        (true, false, axum::http::StatusCode::OK),
    ] {
        let (_temp, _guard) = test_home();
        let mut row = instance_in("alpha", "/tmp/work", "work");
        row.source_profile = "test".into();
        row.status = Status::Stopped;
        if structured {
            row.view = crate::session::View::Structured;
        }
        seed_profile("test", std::slice::from_ref(&row));
        seed_profile("target", &[]);
        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            vec![row.clone()],
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| state.cityhall_mode = true,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut view = test_view(Some("test"));
        let mut drive = view.session_feed.namespace_driver_for_test();
        view.session_feed.set_cityhall_for_test(true);
        apply_published(&mut view, &state).await;
        let before = (payload_bytes("test"), payload_bytes("target"));
        let target = if cross_profile { "target" } else { "test" };
        view.group_rename_context = Some(GroupRenameContext {
            old_path: "work".into(),
            old_profile: "test".into(),
        });
        view.rename_selected_group(Some("renamed"), Some(target))
            .unwrap();
        assert_eq!((payload_bytes("test"), payload_bytes("target")), before);
        let (status, reply) = request(&state,"PATCH","/api/groups",serde_json::json!({"source":{"profile":"test","path":"work"},"target":{"profile":target,"path":"renamed"}})).await;
        assert_eq!(status, expected);
        let result = if status.is_success() {
            Ok(crate::daemon::MutationReceipt {
                cursor: serde_json::from_value(reply["cursor"].clone()).unwrap(),
                outcome: crate::daemon::NamespaceOutcome::Committed,
            })
        } else {
            Err(reply.to_string())
        };
        let submitted = drive(result).unwrap();
        let crate::daemon::NamespaceMutation::MoveGroup(body) = submitted else {
            panic!("expected group move")
        };
        assert_eq!(body.source.profile, "test");
        assert_eq!(body.source.path, "work");
        assert_eq!(body.target.profile, target);
        assert_eq!(body.target.path, "renamed");
        view.apply_session_feed();
        if status.is_success() {
            assert_eq!(
                Storage::new_unwatched("test").unwrap().load().unwrap()[0].group_path,
                "renamed"
            );
            let committed = (payload_bytes("test"), payload_bytes("target"));
            apply_published(&mut view, &state).await;
            assert_eq!(view.get_instance(&row.id).unwrap().group_path, "renamed");
            assert_eq!((payload_bytes("test"), payload_bytes("target")), committed);
        } else {
            assert_eq!((payload_bytes("test"), payload_bytes("target")), before);
            assert!(view.group_trees["test"].group_exists("work"));
            assert!(view.info_dialog.is_some());
        }
    }
}
