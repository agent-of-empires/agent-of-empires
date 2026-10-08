use super::*;

fn row(profile: &str, title: &str, path: &str) -> Instance {
    let mut row = Instance::new(title, path);
    row.source_profile = profile.into();
    row.status = Status::Stopped;
    row
}

#[tokio::test]
#[serial]
async fn canonical_archive_merges_peer_fields_rows_groups_and_timestamp() {
    let (_temp, _guard) = test_home();
    let victim = row("test", "victim", "/tmp/victim");
    seed_profile("test", std::slice::from_ref(&victim));
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    apply_published(&mut view, &state).await;
    let peer = row("test", "peer", "/tmp/peer");
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, groups| {
            rows[0].extra_args = "--peer".into();
            rows[0].snooze(30);
            rows.push(peer.clone());
            groups.push(Group::new("empty", "empty"));
            Ok(())
        })
        .unwrap();
    let (status, _) = request(
        &state,
        "PATCH",
        &format!("/api/sessions/{}/archive", victim.id),
        serde_json::json!({"archived":true,"kill_pane":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (disk, groups) = Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let stored = disk.iter().find(|r| r.id == victim.id).unwrap();
    assert_eq!(stored.extra_args, "--peer");
    assert!(stored.archived_at.is_some());
    assert!(stored.snoozed_until.is_none());
    assert!(stored.lifecycle_reservation.is_none());
    assert!(disk.iter().any(|r| r.id == peer.id));
    assert!(groups.iter().any(|g| g.path == "empty"));
    let committed_bytes = payload_bytes("test");
    apply_published(&mut view, &state).await;
    assert_eq!(
        view.get_instance(&victim.id).unwrap().archived_at,
        stored.archived_at
    );
    assert_eq!(view.get_instance(&victim.id).unwrap().extra_args, "--peer");
    assert!(view.get_instance(&peer.id).is_some());
    view.reload().unwrap();
    assert_eq!(payload_bytes("test"), committed_bytes);
}

#[tokio::test]
#[serial]
async fn canonical_access_unsinks_both_states_with_one_persisted_timestamp() {
    for archived in [true, false] {
        let (_temp, _guard) = test_home();
        let mut victim = row("test", "victim", "/tmp/access");
        if archived {
            victim.archive();
        } else {
            victim.snooze(30);
        }
        seed_profile("test", std::slice::from_ref(&victim));
        let state = native_state(&["test"]).await;
        let mut view = test_view(Some("test"));
        let mut drive = view.session_feed.command_driver_for_test();
        apply_published(&mut view, &state).await;
        let before = payload_bytes("test");
        view.stamp_last_accessed(&victim.id);
        assert_eq!(payload_bytes("test"), before);
        assert_eq!(
            view.get_instance(&victim.id).unwrap().is_archived(),
            archived
        );
        let (status, reply) = request(
            &state,
            "PATCH",
            &format!("/api/sessions/{}/access", victim.id),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let submitted =
            drive(Ok(serde_json::from_value(reply["cursor"].clone()).unwrap())).unwrap();
        assert_eq!(submitted.0, victim.id);
        assert!(matches!(
            submitted.1,
            crate::daemon::SessionMutation::Access
        ));
        let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        let stored = &disk[0];
        assert!(!stored.is_archived());
        assert!(stored.snoozed_until.is_none());
        assert!(stored.last_accessed_at.is_some());
        let bytes = payload_bytes("test");
        apply_published(&mut view, &state).await;
        let shown = view.get_instance(&victim.id).unwrap();
        assert_eq!(shown.last_accessed_at, stored.last_accessed_at);
        assert!(!shown.is_archived());
        assert!(shown.snoozed_until.is_none());
        assert_eq!(payload_bytes("test"), bytes);
    }
}

#[tokio::test]
#[serial]
async fn canonical_profile_move_regroups_then_moves_authoritative_peer_state() {
    let (_temp, _guard) = test_home();
    let victim = row("source", "before", "/tmp/move");
    let source_peer = row("source", "source peer", "/tmp/source-peer");
    let target_peer = row("target", "target peer", "/tmp/target-peer");
    seed_profile("source", &[victim.clone(), source_peer.clone()]);
    seed_profile("target", std::slice::from_ref(&target_peer));
    let state = native_state(&["source", "target"]).await;
    let (status, _) = request(
        &state,
        "PATCH",
        &format!("/api/sessions/{}", victim.id),
        serde_json::json!({"group":"regrouped"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        Storage::new_unwatched("source").unwrap().load().unwrap()[0].group_path,
        "regrouped"
    );
    Storage::new_unwatched("source")
        .unwrap()
        .update(|rows, groups| {
            let live = rows.iter_mut().find(|r| r.id == victim.id).unwrap();
            live.title = "peer title".into();
            live.extra_args = "--peer".into();
            live.lifecycle_generation = 7;
            live.agent_session_id = Some("peer-sid".into());
            groups.push(Group::new("source empty", "source empty"));
            Ok(())
        })
        .unwrap();
    Storage::new_unwatched("target")
        .unwrap()
        .update(|_, groups| {
            groups.push(Group::new("target empty", "target empty"));
            Ok(())
        })
        .unwrap();
    let (status, _) = request(
        &state,
        "PATCH",
        &format!("/api/sessions/{}", victim.id),
        serde_json::json!({"profile":"target","group":"moved"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (source, source_groups) = Storage::new_unwatched("source")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let (target, target_groups) = Storage::new_unwatched("target")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert!(!source.iter().any(|r| r.id == victim.id));
    assert!(source.iter().any(|r| r.id == source_peer.id));
    assert!(target.iter().any(|r| r.id == target_peer.id));
    let moved = target.iter().find(|r| r.id == victim.id).unwrap();
    assert_eq!(moved.title, "peer title");
    assert_eq!(moved.extra_args, "--peer");
    assert_eq!(moved.lifecycle_generation, 7);
    assert_eq!(moved.agent_session_id.as_deref(), Some("peer-sid"));
    assert_eq!(moved.group_path, "moved");
    assert!(source_groups.iter().any(|g| g.path == "source empty"));
    assert!(target_groups.iter().any(|g| g.path == "target empty"));
    let published = state.runtime.publish(&state).await.unwrap();
    let moved = published
        .value
        .contents
        .sessions
        .iter()
        .find(|r| r.id == victim.id)
        .unwrap();
    assert_eq!(moved.profile, "target");
    assert_eq!(moved.title, "peer title");
    assert_eq!(
        published
            .value
            .contents
            .sessions
            .iter()
            .filter(|r| r.id == victim.id)
            .count(),
        1
    );
}

#[tokio::test]
#[serial]
async fn canonical_profile_collision_rejects_before_any_effect_or_partial_commit() {
    for tied in [false, true] {
        let (_temp, _guard) = test_home();
        let repo = TempDir::new().unwrap();
        let old = repo.path().join("old-name");
        let new = repo.path().join("new-name");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("sentinel"), b"untouched").unwrap();
        crate::session::config::update_config(|config| config.session.tie_workdir_to_name = tied)
            .unwrap();
        let mut victim = row("source", "old-name", old.to_str().unwrap());
        if tied {
            crate::session::config::update_config(|config| {
                config.session.tie_workdir_to_name = true
            })
            .unwrap();
            victim.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "old-name".into(),
                main_repo_path: repo
                    .path()
                    .join("missing-repo")
                    .to_string_lossy()
                    .into_owned(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
        }
        let collision_path = if tied {
            new.to_str().unwrap()
        } else {
            old.to_str().unwrap()
        };
        let collision = row("target", "new-name", &format!("{collision_path}/"));
        seed_profile("source", std::slice::from_ref(&victim));
        seed_profile("target", std::slice::from_ref(&collision));
        let state = native_state(&["source", "target"]).await;
        let before = (payload_bytes("source"), payload_bytes("target"));
        let (status, body) = request(&state, "PATCH", &format!("/api/sessions/{}", victim.id), serde_json::json!({"title":"new-name","group":"changed/group","profile":"target","rename_branch":tied})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "duplicate_session");
        assert_eq!((payload_bytes("source"), payload_bytes("target")), before);
        assert_eq!(std::fs::read(old.join("sentinel")).unwrap(), b"untouched");
        assert!(!new.exists());
        let live = state.instances.read().await;
        let live = live.iter().find(|r| r.id == victim.id).unwrap();
        assert_eq!(live.title, "old-name");
        assert_eq!(live.source_profile, "source");
        assert!(live.group_path.is_empty());
    }
}

#[tokio::test]
#[serial]
async fn canonical_peer_deleted_rows_and_cleared_sid_are_not_resurrected() {
    let (_temp, _guard) = test_home();
    let mut victim = row("test", "victim", "/tmp/peer-delete");
    victim.agent_session_id = Some("old-sid".into());
    let peer = row("test", "peer", "/tmp/peer");
    seed_profile("test", &[victim.clone(), peer.clone()]);
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    apply_published(&mut view, &state).await;
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            rows.retain(|r| r.id != peer.id);
            rows.iter_mut()
                .find(|r| r.id == victim.id)
                .unwrap()
                .agent_session_id = None;
            Ok(())
        })
        .unwrap();
    let (status, _) = request(
        &state,
        "PATCH",
        &format!("/api/sessions/{}/group", victim.id),
        serde_json::json!({"group":"changed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bytes = payload_bytes("test");
    apply_published(&mut view, &state).await;
    assert!(view.get_instance(&peer.id).is_none());
    assert!(view
        .get_instance(&victim.id)
        .unwrap()
        .agent_session_id
        .is_none());
    assert_eq!(view.get_instance(&victim.id).unwrap().group_path, "changed");
    view.reload().unwrap();
    assert_eq!(payload_bytes("test"), bytes);
}

#[tokio::test]
#[serial]
async fn canonical_refused_restart_preserves_launch_fields_and_snooze() {
    let (_temp, _guard) = test_home();
    let mut victim = row("test", "victim", "/tmp/restart");
    victim.tool = "claude".into();
    victim.command = "original-wrapper".into();
    victim.extra_args = "--original".into();
    victim.agent_session_id = Some("source-durable-sid".into());
    victim.snooze(30);
    victim.lifecycle_generation = 1;
    victim.lifecycle_reservation = Some(LifecycleReservation {
        op: LifecycleOperation::Launch,
        generation: 1,
        at: chrono::Utc::now(),
    });
    seed_profile("test", std::slice::from_ref(&victim));
    let state = native_state(&["test"]).await;
    let before = payload_bytes("test");
    let (status, _) = request(
        &state,
        "POST",
        &format!("/api/sessions/{}/restart", victim.id),
        serde_json::json!({"tool":"codex","command_override":"codex-wrapper","extra_args":"--new"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload_bytes("test"), before);
    for live in [
        Storage::new_unwatched("test").unwrap().load().unwrap()[0].clone(),
        state.instances.read().await[0].clone(),
    ] {
        assert_eq!(live.tool, victim.tool);
        assert_eq!(live.command, victim.command);
        assert_eq!(live.extra_args, victim.extra_args);
        assert_eq!(live.snoozed_until, victim.snoozed_until);
        assert_eq!(live.agent_session_id, victim.agent_session_id);
        assert!(live.prior_tool_session_ids.is_empty());
        assert_eq!(live.lifecycle_generation, 1);
    }
}
#[tokio::test]
#[serial]
async fn native_delete_admission_does_not_take_lifecycle_flock_and_only_snapshot_removes_row() {
    for snapshot_first in [false, true] {
        let (_temp, _guard) = test_home();
        crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let victim = row("test", "delete", "/tmp/delete-lock");
        seed_profile("test", std::slice::from_ref(&victim));
        let state = native_state(&["test"]).await;
        let mut view = test_view(Some("test"));
        let mut drive = view.session_feed.request_driver_for_test();
        apply_published(&mut view, &state).await;
        view.selected_session = Some(victim.id.clone());
        let storage = Storage::new_unwatched("test").unwrap();
        let lock = storage.acquire_instance_lifecycle_lock(&victim.id).unwrap();
        let before = payload_bytes("test");
        view.delete_selected(&crate::tui::dialogs::DeleteOptions::default())
            .unwrap();
        assert!(view.get_instance(&victim.id).is_some());
        assert_eq!(payload_bytes("test"), before);
        drop(lock);
        let (status, reply) = request(
            &state,
            "DELETE",
            &format!("/api/sessions/{}", victim.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let outcome: crate::daemon::PurgeOutcome = serde_json::from_value(reply.clone()).unwrap();
        assert!(matches!(
            &outcome,
            crate::daemon::PurgeOutcome::Deleted { .. }
        ));
        assert!(storage.load().unwrap().is_empty());
        let committed = payload_bytes("test");
        if snapshot_first {
            apply_published(&mut view, &state).await;
        }
        let submitted = drive(Ok(crate::tui::session_feed::SessionCommandOutcome::Purged(
            crate::daemon::MutationReceipt {
                cursor: serde_json::from_value(reply["cursor"].clone()).unwrap(),
                outcome,
            },
        )))
        .unwrap();
        assert_eq!(submitted.0, victim.id);
        let crate::tui::session_feed::SessionRequest::Purge(body) = submitted.1 else {
            panic!("expected purge")
        };
        assert_eq!(
            serde_json::to_value(body).unwrap(),
            serde_json::to_value(crate::daemon::DeleteSessionBody::default()).unwrap()
        );
        view.apply_session_feed();
        if !snapshot_first {
            assert!(view.get_instance(&victim.id).is_some());
            apply_published(&mut view, &state).await;
        }
        assert!(view.get_instance(&victim.id).is_none());
        view.reload().unwrap();
        assert_eq!(payload_bytes("test"), committed);
    }
}
