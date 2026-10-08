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
    seed_profile("test", &env.view.cloned_instances());
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

fn update(view: &HomeView, id: &str, status: Status) -> crate::daemon::SessionResponse {
    daemon_row(
        view.get_instance(id).expect("fixture row"),
        &format!("{status:?}"),
    )
}

#[test]
#[serial]
fn daemon_status_moves_a_structured_row_off_idle() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    env.view
        .apply_daemon_status_update(&update(&env.view, &id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "a Running turn on the daemon must move the TUI's pill"
    );
}

#[test]
#[serial]
fn daemon_status_carries_the_waiting_state_for_a_pending_approval() {
    // `derive_acp_status` maps ApprovalRequested/ElicitationRequested to Waiting; the
    // yellow pill exists to spot a session blocked on you from the home list.
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Running);

    env.view
        .apply_daemon_status_update(&update(&env.view, &id, Status::Waiting));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Waiting)
    );
}

#[test]
#[serial]
fn daemon_status_clears_a_stale_error_message() {
    // Canonical last_error replaces stale pane diagnostics.
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Error);
    env.view.mutate_instance(&id, |inst| {
        inst.last_error = Some("Container is not running".to_string())
    });

    env.view
        .apply_daemon_status_update(&update(&env.view, &id, Status::Idle));

    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Idle);
    assert_eq!(inst.last_error, None, "the phantom container error is gone");
}

#[test]
#[serial]
fn daemon_status_ignores_an_unknown_session_id() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    let mut unknown = update(&env.view, &id, Status::Running);
    unknown.id = "not-a-session".into();
    env.view.apply_daemon_status_update(&unknown);

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Idle)
    );
}

fn daemon_row(instance: &Instance, status: &str) -> crate::daemon::SessionResponse {
    let mut row = crate::daemon::SessionResponse::from_instance(instance, false);
    row.status = status.into();
    row.last_error = None;
    row.pane_dead_observed = false;
    row
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

pub(super) fn daemon_snapshot(view: &HomeView, id: &str, status: &str) -> SessionFeedResult {
    let mut sessions = vec![daemon_row(
        view.get_instance(id).expect("fixture target"),
        status,
    )];
    sessions.extend(
        view.instances()
            .filter(|instance| instance.id != id)
            .map(|instance| crate::daemon::SessionResponse::from_instance(instance, false)),
    );
    SessionFeedResult::Snapshot(Arc::new(fixture_snapshot(
        sessions,
        "test",
        "test",
        view.session_feed.next_revision_for_test(),
    )))
}

pub(super) fn archived_daemon_snapshot(view: &HomeView, id: &str) -> SessionFeedResult {
    let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(view, id, "Stopped") else {
        unreachable!()
    };
    Arc::make_mut(&mut snapshot).contents.sessions[0].archived_at =
        Some(chrono::Utc::now().to_rfc3339());
    SessionFeedResult::Snapshot(snapshot)
}

/// Explicit membership snapshots, including peer-created and removed rows.
fn snapshot_of(sessions: Vec<crate::daemon::SessionResponse>) -> SessionFeedResult {
    SessionFeedResult::Snapshot(Arc::new(fixture_snapshot(sessions, "test", "test", 1)))
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
        let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&env.view, &id, "Stopped")
        else {
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
                    legacy_tool: None,
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
                seed_profile("native-other", &[]);
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
                    profile: "test".into(),
                },
            })
        });
        env.view.apply_session_feed();
        assert_eq!(
            env.view.take_native_attachment().is_some(),
            matches!(change, Change::None),
            "{change:?}"
        );
        assert!(
            env.view.session_feed.can_submit(&id),
            "known completion frees the row after UX cancellation: {change:?}"
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
        daemon_row(&local, "Idle"),
        daemon_row(&peer, "Running"),
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

/// Off-profile rows stay out of this filtered view; missing local metadata must keep the cursor unapplied.
#[test]
#[serial]
fn a_snapshot_row_outside_the_local_view_is_not_adopted() {
    let mut env = create_test_env_empty();
    let local = local_row(&mut env, "local session", "/tmp/local");
    seed_profile("test", std::slice::from_ref(&local));
    let mut elsewhere = Instance::new("elsewhere", "/tmp/elsewhere");
    elsewhere.source_profile = "other-profile".into();
    seed_profile("other-profile", std::slice::from_ref(&elsewhere));
    let _driver = env.view.session_feed.command_driver_for_test();
    env.view.session_feed.publish_for_test(snapshot_of(vec![
        daemon_row(&local, "Running"),
        daemon_row(&elsewhere, "Running"),
    ]));
    env.view.apply_session_feed();
    assert_eq!(
        env.view.get_instance(&local.id).unwrap().status,
        Status::Running
    );
    assert!(env.view.get_instance(&elsewhere.id).is_none());
    assert!(env
        .view
        .session_feed
        .receipt_applied(&crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision: 1
        }));

    let mut phantom = Instance::new("metadata unavailable", "/tmp/missing");
    phantom.source_profile = "test".into();
    // This complete daemon row has not reached the read-only local metadata mirror.
    let snapshot = fixture_snapshot(
        vec![
            daemon_row(&local, "Stopped"),
            daemon_row(&phantom, "Running"),
        ],
        "test",
        "test",
        2,
    );
    let cursor = snapshot.cursor.clone();
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(Arc::new(snapshot)));
    env.view.apply_session_feed();
    assert!(!env.view.session_feed.receipt_applied(&cursor));
    assert!(!env.view.session_feed.current_snapshot_applied());
    assert!(env.view.get_instance(&phantom.id).is_none());
    assert_eq!(
        env.view.get_instance(&local.id).unwrap().status,
        Status::Running,
        "a failed canonical apply cannot partially replace displayed rows"
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
    env.view.session_feed =
        SessionFeed::seeded_for_test(daemon_snapshot(&env.view, &id, "Running"));

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
        .publish_for_test(daemon_snapshot(&env.view, &id, "Running"));
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
        .publish_for_test(daemon_snapshot(&env.view, &id, "Stopped"));
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
    let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&env.view, &id, "Running")
    else {
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
    let SessionFeedResult::Snapshot(snapshot) = daemon_snapshot(&env.view, &id, "Idle") else {
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
    let SessionFeedResult::Snapshot(mut snapshot) =
        daemon_snapshot(&env.view, &selected, "Stopped")
    else {
        unreachable!()
    };
    std::sync::Arc::make_mut(&mut snapshot).contents.sessions = ids
        .iter()
        .map(|id| {
            let mut row = daemon_row(env.view.get_instance(id).unwrap(), "Stopped");
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
        .publish_for_test(daemon_snapshot(&env.view, &id, "Running"));
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
        .publish_for_test(daemon_snapshot(&env.view, &id, "Stopped"));
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
        .apply_daemon_status_update(&update(&env.view, &id, Status::Running));

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
        .apply_daemon_status_update(&update(&env.view, &id, Status::Stopped));

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
        seed_profile("test", &env.view.cloned_instances());
        let mut observed = update(&env.view, &id, Status::Running);
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
    seed_profile("test", &env.view.cloned_instances());

    // A finished turn (Running -> Idle).
    env.view
        .apply_daemon_status_update(&update(&env.view, &id, Status::Idle));

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

        let mut u = update(&env.view, &id, status);
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
        .apply_daemon_status_update(&update(&env.view, &id, Status::Running));

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
            .apply_daemon_status_update(&update(&env.view, &id, Status::Idle));

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
    let SessionFeedResult::Snapshot(snapshot) = daemon_snapshot(&env.view, &id, "Stopped") else {
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
    let mut stopped = daemon_row(env.view.get_instance(&id).unwrap(), "Stopped");
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
        let mut row = daemon_row(env.view.get_instance(&id).unwrap(), "Running");
        row.view = crate::session::View::Terminal;
        row.lifecycle_generation = 4;
        row.agent_pane = PaneObservation {
            state: PanePresence::Alive,
            tmux_session: Some("restarted-agent".into()),
            legacy_tool: None,
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
                    lifecycle_generation: 5,
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

/// A complete runtime row, with deliberate canonical changes for the scenario.
fn canonical_row(
    instance: &Instance,
    overrides: serde_json::Value,
) -> crate::daemon::SessionResponse {
    let mut value = serde_json::to_value(crate::daemon::SessionResponse::from_instance(
        instance, false,
    ))
    .unwrap();
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
    let mut snapshot = fixture_snapshot(rows, "test", "test", revision);
    snapshot.contents.workspace_ordering = ordering;
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(Arc::new(snapshot)));
    env.view.apply_session_feed();
}

#[test]
#[serial]
fn canonical_revision_reloads_renames_moves_and_view_changes() {
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "alpha", "/tmp/repo");
    let id = instance.id.clone();
    seed_profile("test", &env.view.cloned_instances());

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
    seed_profile("test", &env.view.cloned_instances());
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
    seed_profile("test", &env.view.cloned_instances());
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

#[test]
#[serial]
fn failed_snapshot_reload_retries_without_publication_and_acknowledges_order_only_on_success() {
    use crate::daemon::{RuntimeCursor, SessionMutation};
    for change in ["rename", "add", "remove", "ordering"] {
        let mut env = create_test_env_empty();
        let instance = local_row(&mut env, "alpha", "/tmp/repo");
        seed_profile("test", &env.view.cloned_instances());
        let mut respond = env.view.session_feed.command_driver_for_test();
        publish_canonical_snapshot(
            &mut env,
            vec![canonical_row(&instance, serde_json::json!({}))],
            vec![],
            1,
        );
        env.view
            .session_feed
            .submit(instance.id.clone(), SessionMutation::Stop)
            .unwrap();
        assert!(respond(Ok(RuntimeCursor {
            epoch: "test".into(),
            revision: 2
        }))
        .is_some());
        let before_order = env.view.observed_workspace_ordering.clone();
        let mut durable = instance.clone();
        if change == "rename" {
            durable.title = "renamed".into();
        }
        let mut durable_rows = vec![durable.clone()];
        if change == "add" {
            let mut peer = Instance::new("peer", "/tmp/peer");
            peer.source_profile = "test".into();
            peer.view = crate::session::View::Structured;
            durable_rows.push(peer);
        }
        if change == "remove" {
            durable_rows.clear();
        }
        let storage = Storage::new_unwatched("test").unwrap();
        storage
            .update(|rows, _| {
                *rows = durable_rows.clone();
                Ok(())
            })
            .unwrap();
        let order = vec![format!("/tmp/repo::retry-{change}")];
        crate::session::update_workspace_ordering(|ordering| {
            ordering.order = order.clone();
            Ok(())
        })
        .unwrap();
        let sessions_path = storage.sessions_path().to_path_buf();
        let valid = std::fs::read(&sessions_path).unwrap();
        std::fs::write(&sessions_path, b"{ invalid json ]").unwrap();
        publish_canonical_snapshot(
            &mut env,
            durable_rows
                .iter()
                .map(|row| canonical_row(row, serde_json::json!({})))
                .collect(),
            order.clone(),
            2,
        );
        assert_eq!(env.view.session_feed.next_revision_for_test(), 2);
        assert!(!env.view.session_feed.can_submit(&instance.id));
        assert_eq!(env.view.observed_workspace_ordering, before_order);
        assert_eq!(
            env.view.info_dialog.as_ref().map(InfoDialog::title),
            Some("Runtime state not applied")
        );
        env.view.info_dialog = None;
        assert!(
            !env.view.apply_session_feed(),
            "backoff does not read disk or reopen the error"
        );
        assert!(env.view.info_dialog.is_none());
        std::fs::write(&sessions_path, valid).unwrap();
        env.view.session_feed_reload_retry_at = Some(std::time::Instant::now());
        assert!(env.view.apply_session_feed());
        assert_eq!(env.view.instances.len(), durable_rows.len());
        for row in &durable_rows {
            assert_eq!(env.view.get_instance(&row.id).unwrap().title, row.title);
        }
        if change == "remove" {
            assert!(env.view.get_instance(&instance.id).is_none());
        }
        assert_eq!(env.view.observed_workspace_ordering, order);
        assert_eq!(env.view.session_feed.next_revision_for_test(), 3);
        assert!(env.view.session_feed.can_submit(&instance.id));
        assert!(
            !env.view.apply_session_feed(),
            "successful replay is acknowledged once"
        );
        assert!(
            respond(Ok(RuntimeCursor {
                epoch: "test".into(),
                revision: 3
            }))
            .is_none(),
            "retry sends no mutation"
        );
    }
}

#[test]
#[serial]
fn a_newer_snapshot_supersedes_a_failed_reload_and_unavailable_discards_its_retry() {
    for disconnect in [false, true] {
        let mut env = create_test_env_empty();
        let instance = local_row(&mut env, "original", "/tmp/repo");
        seed_profile("test", &env.view.cloned_instances());
        let _driver = env.view.session_feed.command_driver_for_test();
        publish_canonical_snapshot(
            &mut env,
            vec![canonical_row(&instance, serde_json::json!({}))],
            vec![],
            1,
        );
        let storage = Storage::new_unwatched("test").unwrap();
        let sessions_path = storage.sessions_path().to_path_buf();
        let mut durable = instance.clone();
        durable.title = "latest".into();
        storage
            .update(|rows, _| {
                *rows = vec![durable.clone()];
                Ok(())
            })
            .unwrap();
        let valid = std::fs::read(&sessions_path).unwrap();
        std::fs::write(&sessions_path, b"{ invalid json ]").unwrap();
        let mut superseded = durable.clone();
        superseded.title = "superseded".into();
        publish_canonical_snapshot(
            &mut env,
            vec![canonical_row(&superseded, serde_json::json!({}))],
            vec![],
            2,
        );
        assert_eq!(env.view.session_feed.next_revision_for_test(), 2);
        if disconnect {
            env.view
                .session_feed
                .publish_for_test(SessionFeedResult::Unavailable("lost".into()));
            env.view.apply_session_feed();
            assert!(env.view.session_feed_reload_retry_at.is_none());
            std::fs::write(&sessions_path, valid).unwrap();
            assert!(!env.view.apply_session_feed());
            assert_eq!(
                env.view.get_instance(&instance.id).unwrap().title,
                "original"
            );
            assert_eq!(env.view.session_feed.next_revision_for_test(), 2);
        } else {
            publish_canonical_snapshot(
                &mut env,
                vec![canonical_row(&durable, serde_json::json!({}))],
                vec![],
                3,
            );
            std::fs::write(&sessions_path, valid).unwrap();
            env.view.session_feed_reload_retry_at = Some(std::time::Instant::now());
            env.view.apply_session_feed();
            assert_eq!(env.view.get_instance(&instance.id).unwrap().title, "latest");
            assert_eq!(env.view.session_feed.next_revision_for_test(), 4);
            assert!(!env.view.apply_session_feed());
        }
    }
}

#[test]
#[serial]
fn legacy_stop_confirmation_retains_the_original_exact_row_and_pane() {
    use crate::session::{
        AuxiliaryObservation, AuxiliaryTarget, LegacyToolIdentity, PaneObservation, PanePresence,
    };
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "Row", "/tmp/repo");
    let id = instance.id.clone();
    seed_profile("test", &env.view.cloned_instances());
    let target = AuxiliaryTarget::Tool {
        tool_name: "probe".into(),
    };
    let pane = PaneObservation {
        state: PanePresence::Unknown,
        tmux_session: Some("captured-tool".into()),
        legacy_tool: Some(LegacyToolIdentity {
            session_id: "$42".into(),
            pane_id: "%42".into(),
            pane_pid: 4242,
        }),
    };
    let row = canonical_row(
        &instance,
        serde_json::json!({"auxiliary":[AuxiliaryObservation { target:target.clone(),pane:pane.clone() }],"lifecycle_generation":7}),
    );
    let mut respond = env.view.session_feed.command_driver_for_test();
    publish_canonical_snapshot(&mut env, vec![row], vec![], 1);
    env.view.select_session_by_id(&id);
    env.view.view_mode = ViewMode::Tool("probe".into());
    env.view.stop_selected();
    let captured = env.view.pending_stop_auxiliary.as_ref().unwrap().1.clone();
    assert_eq!(
        captured.adoption.as_ref().unwrap().identity,
        pane.legacy_tool.unwrap()
    );
    assert_eq!(captured.adoption.as_ref().unwrap().profile, "test");
    assert_eq!(captured.adoption.as_ref().unwrap().lifecycle_generation, 7);
    let replacement = PaneObservation {
        state: PanePresence::Unknown,
        tmux_session: Some("replacement-tool".into()),
        legacy_tool: Some(LegacyToolIdentity {
            session_id: "$43".into(),
            pane_id: "%43".into(),
            pane_pid: 4343,
        }),
    };
    let row = canonical_row(
        &instance,
        serde_json::json!({"auxiliary":[AuxiliaryObservation { target, pane:replacement }],"lifecycle_generation":7}),
    );
    publish_canonical_snapshot(&mut env, vec![row], vec![], 2);
    env.view.dispatch_confirm_submit("stop_auxiliary");
    let (sent_id, mutation) = respond(Err("captured identity changed".into())).unwrap();
    assert_eq!(sent_id, id);
    let crate::daemon::SessionMutation::StopAuxiliary(sent) = mutation else {
        panic!("wrong mutation")
    };
    assert_eq!(sent.adoption, captured.adoption);
}

#[test]
#[serial]
fn legacy_attach_live_send_and_send_require_confirmation_and_keep_its_capture() {
    use crate::session::{
        AuxiliaryObservation, AuxiliaryTarget, LegacyToolIdentity, PaneObservation, PanePresence,
    };
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::{NativePane, PaneIntent};
    for intent in 0..3 {
        for cancel in [false, true] {
            let mut env = create_test_env_empty();
            let instance = local_row(&mut env, "Row", "/tmp/repo");
            let id = instance.id.clone();
            seed_profile("test", &env.view.cloned_instances());
            let target = AuxiliaryTarget::Tool {
                tool_name: "probe".into(),
            };
            let pane = PaneObservation {
                state: PanePresence::Unknown,
                tmux_session: Some("captured-tool".into()),
                legacy_tool: Some(LegacyToolIdentity {
                    session_id: "$42".into(),
                    pane_id: "%42".into(),
                    pane_pid: 4242,
                }),
            };
            let expected = crate::session::LegacyToolAdoption {
                tmux_session: "captured-tool".into(),
                identity: pane.legacy_tool.clone().unwrap(),
                profile: "test".into(),
                lifecycle_generation: 7,
            };
            let row = canonical_row(
                &instance,
                serde_json::json!({"auxiliary":[AuxiliaryObservation { target:target.clone(),pane:pane.clone() }],"lifecycle_generation":7}),
            );
            let mut respond = env.view.session_feed.legacy_tool_driver_for_test();
            publish_canonical_snapshot(&mut env, vec![row], vec![], 1);
            env.view.select_session_by_id(&id);
            env.view.view_mode = ViewMode::Tool("probe".into());
            let continuation = match intent {
                0 => PaneIntent::Attach,
                1 => PaneIntent::LiveSend(LiveSendTarget::Tool("probe".into())),
                _ => PaneIntent::Send {
                    message: "must wait for confirmation".into(),
                    target: LiveSendTarget::Tool("probe".into()),
                },
            };
            env.view
                .prepare_native_attachment(
                    &id,
                    NativePane::Auxiliary(target.clone()),
                    None,
                    continuation,
                )
                .unwrap();
            assert!(env.view.confirm_dialog.is_some());
            assert!(env.view.pending_native_attachment.is_none());
            assert!(env.view.session_feed.can_submit(&id));
            if cancel {
                env.view
                    .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None);
                assert!(env.view.pending_legacy_tool_preparation.is_none());
                assert!(env.view.pending_native_attachment.is_none());
                assert!(env.view.session_feed.can_submit(&id));
                continue;
            }
            let replacement = PaneObservation {
                state: PanePresence::Unknown,
                tmux_session: Some("replacement-tool".into()),
                legacy_tool: Some(LegacyToolIdentity {
                    session_id: "$43".into(),
                    pane_id: "%43".into(),
                    pane_pid: 4343,
                }),
            };
            let row = canonical_row(
                &instance,
                serde_json::json!({"auxiliary":[AuxiliaryObservation { target,pane:replacement }],"lifecycle_generation":7}),
            );
            publish_canonical_snapshot(&mut env, vec![row], vec![], 2);
            env.view.confirm_dialog = None;
            env.view.dispatch_confirm_submit("adopt_legacy_tool");
            respond(&expected);
            env.view.apply_session_feed();
            assert!(env.view.take_native_attachment().is_none());
            assert!(env.view.live_send.is_none());
            assert!(
                env.view.info_dialog.is_some(),
                "stale identity rejection is visible"
            );
        }
    }
}

#[test]
#[serial]
fn cancelled_attach_live_send_and_send_settle_without_a_continuation_in_either_receipt_order() {
    use crate::daemon::{MutationReceipt, RuntimeCursor, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::{NativePane, PaneIntent};
    for intent in 0..3 {
        for receipt_first in [false, true] {
            let mut env = create_test_env_with_sessions(1);
            let id = env.view.instance_at(0).id.clone();
            env.view.select_session_by_id(&id);
            let mut respond = env.view.session_feed.terminal_driver_for_test();
            let SessionFeedResult::Snapshot(mut snapshot) =
                daemon_snapshot(&env.view, &id, "Stopped")
            else {
                unreachable!()
            };
            let target = AuxiliaryTarget::Host { index: 0 };
            std::sync::Arc::make_mut(&mut snapshot).contents.sessions[0].auxiliary =
                vec![AuxiliaryObservation {
                    target: target.clone(),
                    pane: PaneObservation {
                        state: PanePresence::Alive,
                        tmux_session: Some("owned-pane".into()),
                        legacy_tool: None,
                    },
                }];
            env.view
                .session_feed
                .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
            env.view.apply_session_feed();
            let continuation = match intent {
                0 => PaneIntent::Attach,
                1 => PaneIntent::LiveSend(LiveSendTarget::Terminal),
                _ => PaneIntent::Send {
                    message: "must not be sent".into(),
                    target: LiveSendTarget::Terminal,
                },
            };
            env.view
                .prepare_native_attachment(&id, NativePane::Auxiliary(target), None, continuation)
                .unwrap();
            env.view
                .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None);
            let mut receipt = Some(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: TerminalTarget {
                    tmux_session: "owned-pane".into(),
                    status: TerminalTargetStatus::Exists,
                    lifecycle_generation: 0,
                    profile: String::new(),
                },
            });
            if receipt_first {
                respond(Ok(receipt.take().unwrap()));
            }
            std::sync::Arc::make_mut(&mut snapshot).cursor.revision = 2;
            if receipt_first {
                env.view.apply_session_feed();
                assert!(!env.view.session_feed.can_submit(&id));
            }
            env.view
                .session_feed
                .publish_for_test(SessionFeedResult::Snapshot(snapshot));
            env.view.apply_session_feed();
            if !receipt_first {
                assert!(!env.view.session_feed.can_submit(&id));
                respond(Ok(receipt.take().unwrap()));
                env.view.apply_session_feed();
            }
            assert!(
                env.view.take_native_attachment().is_none(),
                "no attach/live/send callback: {intent}"
            );
            assert!(env.view.pending_native_attachment.is_none());
            assert!(env.view.live_send.is_none());
            assert!(env.view.session_feed.can_submit(&id));
            assert!(env.view.pending_indeterminate_queue.is_empty());
        }
    }
}
