//! Canonical session projection, including terminal and structured rows.
use super::*;
use crate::session::Status;
use crate::tui::session_feed::{SessionFeed, SessionFeedResult, SidebarSource};

fn structured_row(env: &mut TestEnv, status: Status) -> String {
    let mut inst = Instance::new("acp-session", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.view = crate::session::View::Structured;
    inst.status = status;
    let id = inst.id.clone();
    env.view.add_instance(inst);
    id
}

fn pending_daemon_approvals() -> Vec<crate::daemon::PendingApproval> {
    vec![crate::daemon::PendingApproval {
        nonce: format!("nonce-{}", uuid::Uuid::new_v4()),
        tool_name: "Bash".to_string(),
        target: "echo hi".to_string(),
        destructive: false,
        choice: false,
    }]
}

fn update(id: &str, status: Status) -> crate::daemon::SessionResponse {
    daemon_row(id, &format!("{status:?}"))
}

#[test]
#[serial]
fn daemon_status_moves_a_structured_row_off_idle() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    env.view
        .apply_daemon_status_update(&update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "a Running turn on the daemon must move the TUI's pill"
    );
}

#[test]
#[serial]
fn daemon_status_carries_the_waiting_state_for_a_pending_approval() {
    // `derive_acp_status` maps ApprovalRequested/ElicitationRequested to
    // Waiting; the whole point of the yellow pill is spotting a session
    // blocked on you from the home list without opening it.
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Running);

    env.view
        .apply_daemon_status_update(&update(&id, Status::Waiting));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Waiting)
    );
}

#[test]
#[serial]
fn daemon_status_clears_a_stale_error_message() {
    // The pre-fix sandbox-dead branch left sandboxed structured rows at
    // Idle with a phantom "Container is not running" hanging off them. The
    // daemon's own `last_error` is authoritative, so applying it clears the
    // leftover rather than letting it sit on the row for the session's life.
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Error);
    env.view.mutate_instance(&id, |inst| {
        inst.last_error = Some("Container is not running".to_string())
    });

    env.view
        .apply_daemon_status_update(&update(&id, Status::Idle));

    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Idle);
    assert_eq!(inst.last_error, None, "the phantom container error is gone");
}

#[test]
#[serial]
fn daemon_status_ignores_an_unknown_session_id() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    env.view
        .apply_daemon_status_update(&update("not-a-session", Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Idle)
    );
}

pub(super) fn daemon_row(id: &str, status: &str) -> crate::daemon::SessionResponse {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "status": status,
        "view": "structured",
    }))
    .unwrap()
}

fn daemon_row_in(profile: &str, id: &str, status: &str) -> crate::daemon::SessionResponse {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "profile": profile,
        "status": status,
        "view": "structured",
    }))
    .unwrap()
}

/// A `default`-profile row held by the view, returned whole so a test can also
/// persist it: the view's own model is loaded from storage.
fn local_row(env: &mut TestEnv, title: &str, path: &str) -> Instance {
    let mut inst = Instance::new(title, path);
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.view = crate::session::View::Structured;
    inst.status = Status::Idle;
    env.view.add_instance(inst.clone());
    inst
}

pub(super) fn daemon_snapshot(id: &str, status: &str) -> SessionFeedResult {
    snapshot_of(vec![daemon_row(id, status)])
}

pub(super) fn archived_daemon_snapshot(id: &str) -> SessionFeedResult {
    let mut row = daemon_row(id, "Stopped");
    row.archived_at = Some(chrono::Utc::now().to_rfc3339());
    snapshot_of(vec![row])
}

/// A snapshot whose rows carry their profile, which is what decides whether
/// this view may act on them.
fn snapshot_of(sessions: Vec<crate::daemon::SessionResponse>) -> SessionFeedResult {
    SessionFeedResult::Snapshot(std::sync::Arc::new(crate::daemon::RuntimeSnapshot {
        cursor: crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision: 1,
        },
        contents: crate::daemon::RuntimeContents {
            health: crate::daemon::RuntimeHealth::Healthy,
            capabilities: crate::daemon::RuntimeCapabilities {
                mutations: true,
                native_interaction: true,
            },
            default_profile: "test".into(),
            sessions,
            profiles: vec![],
            workspace_ordering: vec![],
            global_projects: vec![],
        },
    }))
}

#[test]
#[serial]
fn native_attachment_never_survives_a_cancelled_context() {
    use crate::daemon::{MutationReceipt, RuntimeCursor, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PanePresence};
    #[derive(Debug, Clone, Copy)]
    enum Change {
        None,
        Selection,
        View,
        TerminalMode,
        Profile,
        Overlay,
        Escape,
        Rejected,
    }
    for change in [
        Change::None,
        Change::Selection,
        Change::View,
        Change::TerminalMode,
        Change::Profile,
        Change::Overlay,
        Change::Escape,
        Change::Rejected,
    ] {
        let mut env = create_test_env_with_sessions(2);
        let id = env.view.instance_at(0).id.clone();
        let other = env.view.instance_at(1).id.clone();
        env.view.select_session_by_id(&id);
        let mut respond = env.view.session_feed.terminal_driver_for_test();
        let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&id, "Stopped") else {
            unreachable!()
        };
        let target = AuxiliaryTarget::Host { index: 0 };
        std::sync::Arc::make_mut(&mut snapshot).contents.sessions[0]
            .auxiliary
            .push(AuxiliaryObservation {
                target: target.clone(),
                pane: crate::session::PaneObservation {
                    state: PanePresence::Alive,
                    tmux_session: Some("daemon-owned-target".into()),
                },
            });
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        env.view
            .prepare_native_attachment(
                &id,
                crate::tui::home::panes::NativePane::Auxiliary(target),
                None,
                crate::tui::home::panes::PaneIntent::Attach,
            )
            .unwrap();
        assert_eq!(env.view.get_instance(&id).unwrap().status, Status::Stopped);
        match change {
            Change::None | Change::Rejected => {}
            Change::Selection => {
                env.view.select_session_by_id(&other);
                env.view.select_session_by_id(&id);
            }
            Change::View => {
                env.view.handle_key(key(KeyCode::Char('t')), None);
                env.view.handle_key(key(KeyCode::Char('t')), None);
            }
            Change::TerminalMode => {
                env.view.toggle_terminal_mode(&id);
                env.view.toggle_terminal_mode(&id);
            }
            Change::Profile => {
                let original = env.view.active_profile.clone();
                env.view
                    .switch_profile(Some("native-other".into()))
                    .unwrap();
                env.view.switch_profile(original).unwrap();
                env.view.select_session_by_id(&id);
            }
            Change::Overlay => {
                env.view.info_dialog = Some(InfoDialog::new("Notice", "Context changed"));
                assert!(env.view.take_native_attachment().is_none());
                env.view.info_dialog = None;
            }
            Change::Escape => {
                env.view
                    .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None);
            }
        }
        std::sync::Arc::make_mut(&mut snapshot).cursor.revision = 2;
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        assert!(env.view.take_native_attachment().is_none());
        assert!(
            !env.view.session_feed.can_submit(&id),
            "cancelled UI intent freed an in-flight request: {change:?}"
        );
        respond(if matches!(change, Change::Rejected) {
            Err("readiness failed".into())
        } else {
            Ok(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: TerminalTarget {
                    tmux_session: "daemon-owned-target".into(),
                    status: TerminalTargetStatus::Exists,
                    lifecycle_generation: 0,
                    profile: String::new(),
                },
            })
        });
        env.view.apply_session_feed();
        assert_eq!(
            env.view.take_native_attachment().is_some(),
            matches!(change, Change::None),
            "{change:?}"
        );
        assert_eq!(
            env.view.session_feed.can_submit(&id),
            matches!(change, Change::None | Change::Rejected),
            "retry availability: {change:?}"
        );
        if matches!(change, Change::Rejected) {
            assert!(env.view.info_dialog.is_some());
        }
    }
}

/// A session another client created reaches disk before the daemon publishes
/// it. The view must gain it from the snapshot, loading from storage rather
/// than rebuilding a lossy row out of the wire projection.
#[test]
#[serial]
fn a_snapshot_row_this_view_never_saw_is_loaded_from_storage() {
    let mut env = create_test_env_empty();
    let local = local_row(&mut env, "local session", "/tmp/local");
    let local_id = local.id.clone();
    let mut peer = Instance::new("peer session", "/tmp/peer");
    peer.source_profile = "test".to_string();
    let peer_id = peer.id.clone();
    let storage = Storage::new_unwatched("test").unwrap();
    storage
        .update(|rows, _| {
            rows.push(local.clone());
            rows.push(peer.clone());
            Ok(())
        })
        .unwrap();

    env.view.session_feed.publish_for_test(snapshot_of(vec![
        daemon_row_in("test", &local_id, "Idle"),
        daemon_row_in("test", &peer_id, "Running"),
    ]));

    assert!(
        env.view.apply_session_feed(),
        "the new row asks for a redraw"
    );
    assert!(
        env.view.get_instance(&peer_id).is_some(),
        "a peer's session arrives from the snapshot"
    );
    assert!(
        env.view
            .flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &peer_id)),
        "the new row is listed"
    );
    assert!(
        env.view.get_instance(&local_id).is_some(),
        "rows already displayed survive the reload"
    );
}

/// The snapshot can list rows this view must not adopt: another profile's, or
/// one the disk does not have. Neither may disturb the rows on screen.
#[test]
#[serial]
fn a_snapshot_row_outside_the_local_view_is_not_adopted() {
    let mut env = create_test_env_empty();
    let local = local_row(&mut env, "local session", "/tmp/local");
    let local_id = local.id.clone();
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            rows.push(local.clone());
            Ok(())
        })
        .unwrap();

    env.view.session_feed.publish_for_test(snapshot_of(vec![
        daemon_row_in("test", &local_id, "Running"),
        daemon_row_in("test", "phantom", "Running"),
        daemon_row_in("other-profile", "elsewhere", "Running"),
    ]));
    assert!(env.view.apply_session_feed());

    assert_eq!(
        env.view.get_instance(&local_id).map(|inst| inst.status),
        Some(Status::Running),
        "the applied status survives the reload a phantom row triggers"
    );
    assert!(
        env.view.get_instance("phantom").is_none(),
        "a row the disk does not have is never invented"
    );
    assert!(
        env.view.get_instance("elsewhere").is_none(),
        "another profile's row stays out of this view"
    );
}

/// A snapshot drives rows and marks the runtime as the sidebar source; a
/// disconnect is not an error the row surfaces: the source flips to
/// `Disconnected` (so the view drops the stale session content and offers
/// recovery) while the row keeps the runtime's last status. Either result
/// leaves the feed drained and ready for the next publication.
#[test]
#[serial]
fn session_feed_result_selects_the_sidebar_source() {
    let mut env = create_test_env_empty();
    env.view.sidebar_source = SidebarSource::Disconnected;
    let id = structured_row(&mut env, Status::Idle);
    assert_eq!(env.view.sidebar_source, SidebarSource::Disconnected);
    env.view.session_feed = SessionFeed::seeded_for_test(daemon_snapshot(&id, "Running"));

    assert!(
        env.view.apply_session_feed(),
        "an applied row asks for a redraw"
    );
    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running)
    );
    assert_eq!(env.view.sidebar_source, SidebarSource::Daemon);

    // Losing the runtime source is a visible transition, so the drain asks for
    // a redraw; the row itself keeps the last status the runtime gave it.
    env.view.session_feed =
        SessionFeed::seeded_for_test(SessionFeedResult::Unavailable("no daemon".to_string()));
    assert!(
        env.view.apply_session_feed(),
        "dropping the runtime source must ask for a redraw"
    );
    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running)
    );
    assert_eq!(env.view.sidebar_source, SidebarSource::Disconnected);

    // A repeat of the same disconnected source changes nothing to redraw, so
    // the redraw flag comes from the source transition and not from the drain
    // itself.
    env.view.session_feed =
        SessionFeed::seeded_for_test(SessionFeedResult::Unavailable("no daemon".to_string()));
    assert!(
        !env.view.apply_session_feed(),
        "a source that is already disconnected has nothing new to draw"
    );
}

#[test]
#[serial]
fn disconnected_runtime_hides_session_content_until_a_fresh_snapshot() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |instance| {
        instance.title = "offline-sensitive-session".into();
    });
    env.view
        .session_feed
        .publish_for_test(daemon_snapshot(&id, "Running"));
    env.view.apply_session_feed();
    assert!(render_home_to_string(&mut env.view, 120, 40).contains("offline-sensitive-session"));

    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Unavailable("disconnected".into()));
    env.view.apply_session_feed();
    let screen = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        !screen.contains("offline-sensitive-session"),
        "stale session content remains visible: {screen}"
    );
    assert!(
        screen.contains("Reconnect"),
        "offline view must offer recovery: {screen}"
    );

    env.view
        .session_feed
        .publish_for_test(daemon_snapshot(&id, "Stopped"));
    env.view.apply_session_feed();
    assert!(render_home_to_string(&mut env.view, 120, 40).contains("offline-sensitive-session"));
}

#[test]
#[serial]
fn terminal_status_follows_canonical_snapshots_across_stop_and_disconnect() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view
        .mutate_instance(&id, |instance| instance.status = Status::Stopped);
    let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&id, "Running") else {
        unreachable!()
    };
    std::sync::Arc::make_mut(&mut snapshot).contents.sessions[0].view =
        crate::session::View::Terminal;
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
    env.view.apply_session_feed();
    assert_eq!(env.view.get_instance(&id).unwrap().status, Status::Running);

    let next = std::sync::Arc::make_mut(&mut snapshot);
    next.cursor.revision += 1;
    next.contents.sessions[0].status = "Stopped".into();
    next.contents.sessions[0].pane_dead_observed = true;
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(snapshot));
    env.view.apply_session_feed();
    let instance = env.view.get_instance(&id).unwrap();
    assert_eq!(instance.status, Status::Stopped);
    assert!(instance.pane_dead_observed);

    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Unavailable("disconnected".into()));
    env.view.apply_session_feed();
    assert_eq!(env.view.get_instance(&id).unwrap().status, Status::Stopped);
}
#[test]
#[serial]
fn terminal_rows_take_status_and_observations_from_the_daemon_only() {
    use crate::session::{AuxiliaryTarget, PaneObservation, PanePresence};
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |instance| {
        instance.view = crate::session::View::Terminal;
        instance.status = Status::Running;
        instance.last_error =
            Some("tmux session is gone. The agent process may have exited or been killed.".into());
        instance.pane_dead_observed = false;
        instance.agent_pane = PaneObservation::default();
        instance.auxiliary = vec![crate::session::AuxiliaryObservation {
            target: AuxiliaryTarget::Host { index: 0 },
            pane: PaneObservation::default(),
        }];
    });
    let SessionFeedResult::Snapshot(snapshot) = daemon_snapshot(&id, "Idle") else {
        unreachable!()
    };
    let snapshot = {
        let mut snapshot =
            std::sync::Arc::try_unwrap(snapshot).unwrap_or_else(|snapshot| (*snapshot).clone());
        let row = &mut snapshot.contents.sessions[0];
        row.view = crate::session::View::Terminal;
        row.agent_pane = PaneObservation {
            state: PanePresence::Alive,
            ..PaneObservation::default()
        };
        row.auxiliary = vec![crate::session::AuxiliaryObservation {
            target: AuxiliaryTarget::Host { index: 0 },
            pane: PaneObservation {
                state: PanePresence::Alive,
                ..PaneObservation::default()
            },
        }];
        std::sync::Arc::new(snapshot)
    };
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(snapshot));
    env.view.apply_session_feed();
    let instance = env.view.get_instance(&id).unwrap();
    assert_eq!(instance.status, Status::Idle);
    assert_eq!(instance.last_error, None);
    assert_eq!(instance.agent_pane.state, PanePresence::Alive);
    assert_eq!(
        instance.auxiliary_presence(&AuxiliaryTarget::Host { index: 0 }),
        PanePresence::Alive
    );
}

#[test]
#[serial]
fn canonical_favorite_reorder_keeps_cursor_on_selected_session() {
    let mut env = create_test_env_with_sessions(2);
    env.view.sort_order = crate::session::config::SortOrder::Attention;
    for instance in env.view.instances.values_mut() {
        instance.status = Status::Stopped;
    }
    env.view.rebuild_flat_items();
    let ids: Vec<_> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let selected = ids.last().unwrap().clone();
    env.view.select_session_by_id(&selected);
    let old_cursor = env.view.cursor;
    let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&selected, "Stopped") else {
        unreachable!()
    };
    std::sync::Arc::make_mut(&mut snapshot).contents.sessions = ids
        .iter()
        .map(|id| {
            let mut row = daemon_row(id, "Stopped");
            row.view = crate::session::View::Terminal;
            if id == &selected {
                row.favorited = true;
                row.favorited_at = Some("2026-09-13T00:00:00Z".into());
            }
            row
        })
        .collect();
    env.view.session_feed = SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot));
    assert!(env.view.apply_session_feed());
    let selected_position = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if id == &selected))
        .unwrap();
    assert_ne!(
        selected_position, old_cursor,
        "fixture must reorder the selected row"
    );
    assert_eq!(
        env.view.cursor, selected_position,
        "highlight must follow the same session"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(selected.as_str())
    );
}

/// A published result is applied exactly once: a tick with no new publication
/// is a no-op, and a fresh publication after a completed apply is picked up.
#[test]
#[serial]
fn applied_feed_result_is_consumed_exactly_once() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    // No publication yet: nothing to apply, no redraw.
    assert!(!env.view.apply_session_feed());
    assert!(!env.view.apply_session_feed());

    env.view
        .session_feed
        .publish_for_test(daemon_snapshot(&id, "Running"));
    assert!(
        env.view.apply_session_feed(),
        "a fresh publication asks for a redraw"
    );
    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running)
    );
    assert_eq!(env.view.sidebar_source, SidebarSource::Daemon);

    // The completed apply is consumed: a tick with no new publication is a no-op.
    assert!(
        !env.view.apply_session_feed(),
        "a consumed result must not re-apply on the next tick"
    );

    // A new publication after the completed apply is picked up again.
    env.view
        .session_feed
        .publish_for_test(daemon_snapshot(&id, "Stopped"));
    assert!(env.view.apply_session_feed());
    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Stopped)
    );
}

/// The regression that made this producer necessary in the first place,
/// surviving in a reachable path: stopping a structured session persists
/// `Stopped`, `open_structured_view` does not clear it, and
/// `apply_status_update` drops every update whose row is `Stopped`. Without
/// the explicit lift, the pill stays grey through the entire next turn.
#[test]
#[serial]
fn daemon_status_lifts_a_locally_stopped_structured_row() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Stopped);

    env.view
        .apply_daemon_status_update(&update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "a fresh worker epoch on the daemon must wake a locally-Stopped row"
    );
}

/// The other side of that lift: a daemon still reporting `Stopped` must not
/// be turned into a wake-up. Only a non-Stopped reading, which the daemon
/// emits only after `AcpSessionAssigned` heals its own row, counts.
#[test]
#[serial]
fn daemon_status_stopped_leaves_a_stopped_row_alone() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Stopped);

    env.view
        .apply_daemon_status_update(&update(&id, Status::Stopped));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Stopped)
    );
}

#[test]
#[serial]
fn canonical_status_changes_never_persist_from_the_tui() {
    for view in [
        crate::session::View::Terminal,
        crate::session::View::Structured,
    ] {
        let mut env = create_test_env_empty();
        let id = structured_row(&mut env, Status::Idle);
        env.view.mutate_instance(&id, |row| row.view = view);
        env.view.save().expect("seed the durable row");
        let mut observed = update(&id, Status::Running);
        observed.view = view;
        env.view.apply_daemon_status_update(&observed);
        assert_eq!(
            env.view.get_instance(&id).map(|row| row.status),
            Some(Status::Running)
        );
        let rows = env.view.storages.get("test").unwrap().load().unwrap();
        let disk = rows
            .iter()
            .find(|row| row.id == id)
            .expect("disk row present");
        assert_eq!(
            disk.status,
            Status::Idle,
            "canonical {view:?} updates must not write from the TUI"
        );
    }
}

/// A structured row's turn-end is the daemon's to record, both halves of it,
/// so the TUI writes neither field: the status is a daemon-side overlay with
/// no durable owner (#3201), and the unread mark is written durably by the
/// live ACP turn-end path (`should_mark_acp_unread`, #3181).
///
/// The mark still reaches this row, from disk on the next reload;
/// `merge_from_tui` has no `unread` arm, so a TUI save cannot clobber it.
#[test]
#[serial]
fn tui_persists_neither_status_nor_unread_for_a_structured_turn_end() {
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Running);
    env.view
        .save()
        .expect("seed the structured row on disk as read/Running");

    // A finished turn (Running -> Idle).
    env.view
        .apply_daemon_status_update(&update(&id, Status::Idle));

    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Idle, "the turn-end still applies");
    assert!(
        !inst.is_unread(),
        "the structured turn-end mark is the daemon's to write, not ours"
    );

    let rows = env.view.storages.get("test").unwrap().load().unwrap();
    let disk = rows.iter().find(|i| i.id == id).expect("disk row present");
    assert_eq!(
        disk.status,
        Status::Running,
        "structured status must not be passively persisted (#3201)"
    );
    assert!(
        !disk.is_unread(),
        "structured unread must not be passively persisted either (#3181)"
    );
}

// Canonical errors replace the previous value even without a status change.
#[test]
#[serial]
fn daemon_status_reconciles_last_error_on_a_same_status_tick() {
    // (row status, seeded local error, incoming daemon error, expected)
    let cases = [
        (Status::Running, "delete failed: worktree busy", None, None),
        // A present incoming Some replaces it even with no status change.
        (
            Status::Error,
            "agent failed to start",
            Some("rate limit exceeded"),
            Some("rate limit exceeded"),
        ),
    ];
    for (status, seeded, incoming, expected) in cases {
        let mut env = create_test_env_empty();
        let id = structured_row(&mut env, status);
        env.view
            .mutate_instance(&id, |inst| inst.last_error = Some(seeded.to_string()));

        let mut u = update(&id, status);
        u.last_error = incoming.map(str::to_string);
        env.view.apply_daemon_status_update(&u);

        assert_eq!(
            env.view
                .get_instance(&id)
                .and_then(|i| i.last_error.clone()),
            expected.map(str::to_string),
            "status={status:?} incoming={incoming:?}"
        );
    }
}

#[test]
#[serial]
fn daemon_status_applies_to_a_snoozed_structured_row() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    env.view
        .apply_daemon_status_update(&update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "a snoozed row is live triage, not a sink; the daemon overlay must still drive its status (#3201)"
    );
}

#[test]
#[serial]
fn daemon_update_clears_cached_approvals_when_a_row_is_sunk() {
    for label in ["archived", "trashed"] {
        let mut env = create_test_env_empty();
        let id = structured_row(&mut env, Status::Waiting);
        // Cache a pending approval the way the live-refresh path does.
        env.view
            .structured_pending_approvals
            .insert(id.clone(), pending_daemon_approvals());
        let now = chrono::Utc::now();
        env.view.mutate_instance(&id, |inst| {
            if label == "archived" {
                inst.archived_at = Some(now);
            } else {
                inst.trashed_at = Some(now);
            }
        });

        env.view
            .apply_daemon_status_update(&update(&id, Status::Idle));

        assert!(
            !env.view.structured_pending_approvals.contains_key(&id),
            "a {label} row must not keep cached approvals after the transition"
        );
    }
}

/// I5 freshness contract: a terminal row converges to the daemon snapshot
/// within one feed apply, including the Running->Stopped transition that used
/// to need the local 500ms poller. No tmux probe runs between publish and
/// apply; the feed alone must carry status, last_error, pane_dead_observed,
/// agent_pane, and auxiliary (status.rs feed-overwrite lines).
#[test]
#[serial]
fn terminal_status_converges_in_one_feed_apply_without_a_local_probe() {
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    // Seed the row stale the way a live Running terminal looks before the
    // daemon reports the stop.
    env.view.mutate_instance(&id, |instance| {
        instance.view = crate::session::View::Terminal;
        instance.status = Status::Running;
        instance.last_error = Some("stale local probe error".into());
        instance.pane_dead_observed = false;
        instance.agent_pane = PaneObservation {
            state: PanePresence::Alive,
            ..PaneObservation::default()
        };
        instance.auxiliary = vec![AuxiliaryObservation {
            target: AuxiliaryTarget::Host { index: 0 },
            pane: PaneObservation {
                state: PanePresence::Alive,
                ..PaneObservation::default()
            },
        }];
    });
    // The daemon publishes the stop: Stopped + pane_dead_observed with both
    // pane seeds flipped to Dead.
    let SessionFeedResult::Snapshot(snapshot) = daemon_snapshot(&id, "Stopped") else {
        unreachable!()
    };
    let snapshot = {
        let mut snapshot =
            std::sync::Arc::try_unwrap(snapshot).unwrap_or_else(|snapshot| (*snapshot).clone());
        let row = &mut snapshot.contents.sessions[0];
        row.view = crate::session::View::Terminal;
        row.pane_dead_observed = true;
        row.last_error = Some("agent process exited".into());
        row.agent_pane = PaneObservation {
            state: PanePresence::Dead,
            ..PaneObservation::default()
        };
        row.auxiliary = vec![AuxiliaryObservation {
            target: AuxiliaryTarget::Host { index: 0 },
            pane: PaneObservation {
                state: PanePresence::Dead,
                ..PaneObservation::default()
            },
        }];
        std::sync::Arc::new(snapshot)
    };
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(snapshot));
    // No local tmux probe runs between publish and apply: one feed apply must
    // carry the whole transition.
    env.view.apply_session_feed();

    let instance = env.view.get_instance(&id).unwrap();
    assert_eq!(instance.status, Status::Stopped);
    assert_eq!(instance.last_error.as_deref(), Some("agent process exited"));
    assert!(instance.pane_dead_observed);
    assert_eq!(instance.agent_pane.state, PanePresence::Dead);
    assert_eq!(
        instance.auxiliary_presence(&AuxiliaryTarget::Host { index: 0 }),
        PanePresence::Dead
    );
    // The Terminal row seed reads through auxiliary_presence_for_view, so it
    // must flip from spinner to ICON_STOPPED input on the same apply.
    env.view.view_mode = crate::tui::home::ViewMode::Terminal;
    let seed = {
        let instance = env.view.get_instance(&id).unwrap();
        env.view.auxiliary_presence_for_view(instance)
    };
    assert_eq!(seed, PanePresence::Dead);
}

/// Stop through the feed lands the daemon's Stopped row with a cleared
/// `last_error`, the same end state the local path used to write, and no
/// local status write happens on either side of the submit.
#[test]
#[serial]
fn daemon_stop_result_lands_stopped_without_a_local_write() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.last_error = Some("stale local probe error".into());
    });

    let mut respond = env.view.session_feed.command_driver_for_test();
    let admitted = env
        .view
        .submit_daemon_stop(&id)
        .expect("stop submit returns its admission");
    assert!(admitted, "an available runtime admits the stop");
    // Admit-side: the mutation is queued, the row is untouched.
    assert_eq!(
        env.view.get_instance(&id).map(|inst| inst.status),
        Some(Status::Running)
    );
    assert_eq!(
        env.view
            .get_instance(&id)
            .and_then(|inst| inst.last_error.clone()),
        Some("stale local probe error".to_string())
    );
    let submitted = respond(Ok(crate::daemon::RuntimeCursor {
        epoch: "test".into(),
        revision: 2,
    }));
    assert!(
        submitted.is_some_and(|(target, mutation)| target == id
            && matches!(mutation, crate::daemon::SessionMutation::Stop)),
        "stop must submit SessionMutation::Stop for the row"
    );
    assert!(
        env.view.info_dialog.is_none(),
        "an admitted stop shows no dialog"
    );

    // Outcome-side: the canonical snapshot, not a local write, flips the row.
    let mut stopped = daemon_row(&id, "Stopped");
    stopped.last_error = None;
    env.view.apply_daemon_status_update(&stopped);
    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Stopped);
    assert_eq!(inst.last_error, None);
}

/// A refused stop submit fails loudly with reconnect guidance and leaves the
/// row untouched: no local status write on failure, no replay.
#[test]
#[serial]
fn daemon_unreachable_stop_fails_without_a_local_write() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.status = Status::Running;
        inst.last_error = Some("stale local probe error".into());
    });

    // No command driver: the feed has no runtime, so the submit refuses.
    let admitted = env
        .view
        .submit_daemon_stop(&id)
        .expect("stop refusal still returns");
    assert!(!admitted, "a disconnected runtime refuses the stop");
    let dialog = env
        .view
        .info_dialog
        .as_ref()
        .expect("a refused stop surfaces a dialog");
    assert_eq!(dialog.title(), "Stop failed");
    assert!(
        dialog.message().contains("Reconnect the runtime"),
        "a refused stop points at reconnect, got: {}",
        dialog.message()
    );
    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Running, "the row is unchanged");
    assert_eq!(
        inst.last_error.as_deref(),
        Some("stale local probe error"),
        "the stale error is unchanged"
    );
}

#[test]
#[serial]
fn restart_attachment_waits_for_its_receipt_and_lifecycle_generation() {
    use crate::daemon::{
        MutationReceipt, RestartOutcome, RuntimeCursor, TerminalTarget, TerminalTargetStatus,
    };
    use crate::session::{PaneObservation, PanePresence};

    for (reflected_generation, target_name) in [
        (5, "restarted-agent"),
        (6, "restarted-agent"),
        (5, "renamed-agent"),
    ] {
        let mut env = create_test_env_with_sessions(1);
        let id = env.view.instance_at(0).id.clone();
        env.view.select_session_by_id(&id);
        let mut row = daemon_row_in("test", &id, "Running");
        row.view = crate::session::View::Terminal;
        row.lifecycle_generation = 4;
        row.agent_pane = PaneObservation {
            state: PanePresence::Alive,
            tmux_session: Some("restarted-agent".into()),
        };
        let SessionFeedResult::Snapshot(mut snapshot) = snapshot_of(vec![row]) else {
            unreachable!()
        };
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        let mut respond = env.view.session_feed.restart_driver_for_test();
        env.view.restart_then_attach(&id, None, false);

        // A still-Running snapshot predating the receipt cannot settle a restart.
        std::sync::Arc::make_mut(&mut snapshot).cursor.revision = 2;
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        env.view.apply_restart_results();
        assert!(env.view.restart_in_flight.contains(&id));
        assert!(env.view.take_native_attachment().is_none());

        assert!(respond(Ok(MutationReceipt {
            cursor: RuntimeCursor {
                epoch: "test".into(),
                revision: 3
            },
            outcome: RestartOutcome {
                lifecycle_generation: 5,
                profile: "test".into(),
                target: Some(TerminalTarget {
                    tmux_session: "restarted-agent".into(),
                    status: TerminalTargetStatus::Restarted,
                    lifecycle_generation: 0,
                    profile: String::new(),
                }),
            },
        }))
        .is_some_and(|(target, _)| target == id));
        env.view.apply_session_feed();
        env.view.apply_restart_results();
        assert!(env.view.restart_in_flight.contains(&id));
        assert!(env.view.take_native_attachment().is_none());

        let committed = std::sync::Arc::make_mut(&mut snapshot);
        committed.cursor.revision = 3;
        committed.contents.sessions[0].lifecycle_generation = reflected_generation;
        committed.contents.sessions[0].agent_pane.tmux_session = Some(target_name.into());
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        env.view.apply_restart_results();
        assert!(!env.view.restart_in_flight.contains(&id));
        let attachment = env.view.take_native_attachment();
        if reflected_generation == 5 && target_name == "restarted-agent" {
            let attachment = attachment.expect("committed restart is attachable");
            assert_eq!(attachment.id, id);
            assert_eq!(attachment.tmux_name, "restarted-agent");
        } else {
            assert!(
                attachment.is_none(),
                "another lifecycle change superseded this receipt"
            );
        }
        assert!(env.view.take_native_attachment().is_none());
    }
}

/// A runtime row carrying the full durable projection, as the daemon sends
/// it. Minimal test rows (id/status only) deliberately skip metadata
/// comparison in the view.
fn canonical_row(
    instance: &crate::session::Instance,
    overrides: serde_json::Value,
) -> crate::daemon::SessionResponse {
    let mut value = serde_json::json!({
        "id": instance.id,
        "title": instance.title,
        "project_path": instance.project_path,
        "profile": instance.source_profile,
        "group_path": instance.group_path,
        "tool": instance.tool,
        "view": instance.view,
        "status": format!("{:?}", instance.status),
        "has_managed_worktree": instance.worktree_info.is_some(),
        "branch": instance.worktree_info.as_ref().map(|worktree| worktree.branch.clone()),
        "base_branch_override": instance.base_branch_override,
    });
    let object = value.as_object_mut().expect("an object row");
    for (key, item) in overrides.as_object().expect("object overrides") {
        object.insert(key.clone(), item.clone());
    }
    serde_json::from_value(value).expect("a canonical row")
}

fn publish_canonical_snapshot(
    env: &mut TestEnv,
    rows: Vec<crate::daemon::SessionResponse>,
    ordering: Vec<String>,
    revision: u64,
) {
    let snapshot = crate::daemon::RuntimeSnapshot {
        cursor: crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision,
        },
        contents: crate::daemon::RuntimeContents {
            health: crate::daemon::RuntimeHealth::Healthy,
            capabilities: crate::daemon::RuntimeCapabilities {
                mutations: true,
                native_interaction: true,
            },
            default_profile: "test".into(),
            sessions: rows,
            profiles: vec![],
            workspace_ordering: ordering,
            global_projects: vec![],
        },
    };
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(std::sync::Arc::new(snapshot)));
    env.view.apply_session_feed();
}

#[test]
#[serial]
fn canonical_revision_reloads_renames_moves_and_view_changes() {
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "alpha", "/tmp/repo");
    let id = instance.id.clone();
    env.view.save().expect("seed the durable row");

    let mut renamed = instance.clone();
    renamed.title = "renamed".into();
    renamed.group_path = "moved/group".into();
    renamed.tool = "codex".into();
    renamed.view = crate::session::View::Terminal;
    renamed.base_branch_override = Some("upstream/main".into());
    renamed.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature".into(),
        main_repo_path: "/tmp/repo".into(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: Some("main".into()),
    });
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            *rows = vec![renamed.clone()];
            Ok(())
        })
        .expect("publish the canonical row on disk");

    let row = canonical_row(&renamed, serde_json::json!({}));
    publish_canonical_snapshot(&mut env, vec![row], vec![], 1);

    let applied = env.view.get_instance(&id).expect("row survives");
    assert_eq!(applied.title, "renamed");
    assert_eq!(applied.group_path, "moved/group");
    assert_eq!(applied.tool, "codex");
    assert!(applied.view.is_terminal(), "view change is absorbed");
    assert_eq!(
        applied.base_branch_override.as_deref(),
        Some("upstream/main")
    );
    assert_eq!(
        applied
            .worktree_info
            .as_ref()
            .map(|worktree| worktree.branch.as_str()),
        Some("feature")
    );
}

#[test]
#[serial]
fn canonical_revision_drops_a_row_the_daemon_removed() {
    let mut env = create_test_env_empty();
    let kept = local_row(&mut env, "kept", "/tmp/kept");
    let removed = local_row(&mut env, "removed", "/tmp/removed");
    env.view.save().expect("seed both rows");
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            rows.retain(|row| row.id != removed.id);
            Ok(())
        })
        .expect("delete the row a peer owns");

    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&kept, serde_json::json!({}))],
        vec![],
        1,
    );

    assert!(env.view.get_instance(&kept.id).is_some());
    assert!(
        env.view.get_instance(&removed.id).is_none(),
        "a row absent from the canonical revision must not linger"
    );
}

#[test]
#[serial]
fn canonical_revision_reconciles_metadata_across_workspace_reordering() {
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "alpha", "/tmp/repo");
    env.view.save().expect("seed the durable row");
    crate::session::update_workspace_ordering(|ordering| {
        ordering.order = vec!["/tmp/repo::feature".into()];
        Ok(())
    })
    .expect("persist the peer ordering");

    let stored_title = Storage::new_unwatched("test").unwrap().load().unwrap()[0]
        .title
        .clone();
    assert_eq!(stored_title, "alpha");
    let mut row = canonical_row(&instance, serde_json::json!({}));
    row.title = "ordering-arrival".into();
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            rows[0].title = row.title.clone();
            Ok(())
        })
        .expect("the ordering write landed together with a durable change");
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(
            &Storage::new_unwatched("test").unwrap().load().unwrap()[0].clone(),
            serde_json::json!({}),
        )],
        vec!["/tmp/repo::feature".into()],
        1,
    );

    assert!(env
        .view
        .get_instance(&instance.id)
        .is_some_and(|row| row.title == "ordering-arrival"));
}
