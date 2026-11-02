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
    // `AcpState::sidebar_status` maps a pending approval or elicitation to Waiting; the
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
    row.last_error_evidence = None;
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

fn native_physical_fixture() -> Arc<crate::session::LegacyToolObservation> {
    let pid = std::process::id();
    Arc::new(crate::session::LegacyToolObservation {
        identity: crate::session::LegacyToolIdentity {
            session_id: "$1".into(),
            pane_id: "%1".into(),
            pane_pid: Some(pid),
        },
        server: Arc::new(crate::session::TmuxServerIdentity {
            socket: "/fixture-tmux.sock".into(),
            pid,
            birth: crate::process::process_birth_identity(pid)
                .unwrap()
                .unwrap(),
        }),
    })
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
                    physical: Some(native_physical_fixture()),
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
                    physical: Some(native_physical_fixture()),
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

#[test]
#[serial]
fn tool_preview_enter_upgrades_one_canonical_preparation() {
    use crate::daemon::{MutationReceipt, RuntimeCursor, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    use crate::tui::home::panes::PaneIntent;
    for observed in [
        None,
        Some(PanePresence::Absent),
        Some(PanePresence::Unknown),
    ] {
        let mut env = create_test_env_with_sessions(1);
        env.view.tool_configs.insert(
            "probe".into(),
            crate::session::config::ToolSessionConfig {
                command: "sleep 600".into(),
                ..Default::default()
            },
        );
        let id = env.view.instance_at(0).id.clone();
        env.view.select_session_by_id(&id);
        let mut respond = env.view.session_feed.terminal_driver_for_test();
        let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&env.view, &id, "Stopped")
        else {
            unreachable!()
        };
        if let Some(state) = observed {
            Arc::make_mut(&mut snapshot).contents.sessions[0]
                .auxiliary
                .push(AuxiliaryObservation {
                    target: AuxiliaryTarget::Tool {
                        tool_name: "probe".into(),
                    },
                    pane: PaneObservation {
                        physical: None,
                        state,
                        ..Default::default()
                    },
                });
        }
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        env.view.tool_hotkey_cache = vec![("probe".into(), KeyCode::Char('g'), KeyModifiers::ALT)];
        assert_eq!(
            env.view
                .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT), None),
            None
        );
        assert_eq!(env.view.view_mode, ViewMode::Tool("probe".into()));
        if observed == Some(PanePresence::Unknown) {
            assert!(env.view.info_dialog.is_some());
            assert!(env.view.pending_native_attachment.is_none());
            assert!(env.view.session_feed.can_submit(&id));
            continue;
        }
        assert!(env.view.info_dialog.is_none());
        assert!(!env.view.session_feed.can_submit(&id));
        assert_eq!(
            env.view.handle_key(key(KeyCode::Enter), None),
            Some(Action::AttachToolSession(id.clone(), "probe".into()))
        );
        let pane = crate::tui::home::panes::NativePane::Auxiliary(AuxiliaryTarget::Tool {
            tool_name: "probe".into(),
        });
        env.view
            .prepare_native_attachment(&id, pane, None, PaneIntent::Attach)
            .unwrap();
        respond(Ok(MutationReceipt {
            cursor: RuntimeCursor {
                epoch: "test".into(),
                revision: 2,
            },
            outcome: TerminalTarget {
                physical: Some(native_physical_fixture()),
                tmux_session: "canonical-tool".into(),
                status: TerminalTargetStatus::Created,
                lifecycle_generation: 0,
                profile: "test".into(),
            },
        }));
        env.view.apply_session_feed();
        assert!(env.view.take_native_attachment().is_none());
        assert!(!env.view.session_feed.can_submit(&id));
        let committed = Arc::make_mut(&mut snapshot);
        committed.cursor.revision = 2;
        committed.contents.sessions[0].auxiliary = vec![AuxiliaryObservation {
            target: AuxiliaryTarget::Tool {
                tool_name: "probe".into(),
            },
            pane: PaneObservation {
                physical: Some(native_physical_fixture()),
                state: PanePresence::Alive,
                tmux_session: Some("canonical-tool".into()),
                legacy_tool: None,
            },
        }];
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        let ready = env
            .view
            .take_native_attachment()
            .expect("same preparation must enter attach after canonical publication");
        assert!(matches!(ready.intent, PaneIntent::Attach));
        assert_eq!(ready.terminal.tmux_session, "canonical-tool");
        assert!(ready.lease.is_valid());
        assert!(env.view.session_feed.can_submit(&id));
    }
}

#[test]
#[serial]
fn deferred_tool_preview_keeps_its_live_upgrade_and_cancellation_context() {
    use crate::daemon::{RuntimeCursor, SessionMutation, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::{NativePane, PaneIntent};
    use crate::tui::session_feed::SessionRequest;
    for change in ["none", "selection", "overlay"] {
        let mut env = create_test_env_with_sessions(2);
        let id = env.view.instance_at(0).id.clone();
        let other = env.view.instance_at(1).id.clone();
        env.view.select_session_by_id(&id);
        let mut respond = env.view.session_feed.engagement_native_driver_for_test();
        let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&env.view, &id, "Stopped")
        else {
            unreachable!()
        };
        let target = AuxiliaryTarget::Tool {
            tool_name: "probe".into(),
        };
        Arc::make_mut(&mut snapshot).contents.sessions[0]
            .auxiliary
            .push(AuxiliaryObservation {
                target: target.clone(),
                pane: PaneObservation {
                    physical: None,
                    state: PanePresence::Absent,
                    ..Default::default()
                },
            });
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        env.view.view_mode = ViewMode::Tool("probe".into());
        env.view
            .session_feed
            .submit(id.clone(), SessionMutation::Access)
            .unwrap();
        env.view
            .prepare_native_attachment(
                &id,
                NativePane::Auxiliary(target.clone()),
                None,
                PaneIntent::Preview,
            )
            .unwrap();
        env.view
            .prepare_live_send_target(&id, LiveSendTarget::Tool("probe".into()), None)
            .unwrap();
        assert!(env.view.take_native_attachment().is_none());
        match change {
            "selection" => {
                env.view.select_session_by_id(&other);
                env.view.select_session_by_id(&id);
            }
            "overlay" => {
                env.view.info_dialog = Some(InfoDialog::new("Notice", "Context changed"));
                assert!(env.view.take_native_attachment().is_none());
                env.view.info_dialog = None;
            }
            _ => {}
        }
        let cursor = |revision| RuntimeCursor {
            epoch: "test".into(),
            revision,
        };
        assert!(matches!(
            respond(Ok(cursor(2)), None),
            Some(SessionRequest::Mutation(SessionMutation::Access))
        ));
        env.view.apply_session_feed();
        assert!(env.view.take_native_attachment().is_none());
        Arc::make_mut(&mut snapshot).cursor.revision = 2;
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        let prepared_target = || TerminalTarget {
            physical: Some(native_physical_fixture()),
            tmux_session: "canonical-tool".into(),
            status: TerminalTargetStatus::Created,
            lifecycle_generation: 0,
            profile: "test".into(),
        };
        let request = respond(Ok(cursor(3)), Some(prepared_target()));
        if change != "none" {
            assert!(
                request.is_none(),
                "{change}: cancelled context must not send ensure"
            );
            assert!(env.view.take_native_attachment().is_none());
            assert!(env.view.live_send.is_none());
            assert!(env.view.session_feed.can_submit(&id));
            continue;
        }
        assert!(matches!(
            request,
            Some(SessionRequest::EnsureAuxiliary { .. })
        ));
        env.view.apply_session_feed();
        assert!(env.view.take_native_attachment().is_none());
        let committed = Arc::make_mut(&mut snapshot);
        committed.cursor.revision = 3;
        committed.contents.sessions[0].auxiliary = vec![AuxiliaryObservation {
            target,
            pane: PaneObservation {
                physical: Some(native_physical_fixture()),
                state: PanePresence::Alive,
                tmux_session: Some("canonical-tool".into()),
                legacy_tool: None,
            },
        }];
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        let ready = env
            .view
            .take_native_attachment()
            .expect("same deferred preview must complete upgraded intent");
        assert!(
            matches!(&ready.intent, PaneIntent::LiveSend(LiveSendTarget::Tool(tool)) if tool == "probe")
        );
        assert_eq!(ready.terminal.tmux_session, "canonical-tool");
        assert!(ready.lease.is_valid());
        assert!(
            respond(Ok(cursor(3)), Some(prepared_target())).is_none(),
            "only one ensure is sent"
        );
    }
}

#[test]
#[serial]
fn auxiliary_receipt_never_outlives_its_row_generation_or_profile() {
    use crate::daemon::{MutationReceipt, RuntimeCursor, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    for (receipt_profile, snapshot_generation) in [("test", 1), ("other", 0)] {
        let mut env = create_test_env_with_sessions(1);
        let id = env.view.instance_at(0).id.clone();
        env.view.select_session_by_id(&id);
        let mut respond = env.view.session_feed.terminal_driver_for_test();
        let SessionFeedResult::Snapshot(mut snapshot) = daemon_snapshot(&env.view, &id, "Stopped")
        else {
            unreachable!()
        };
        Arc::make_mut(&mut snapshot).contents.sessions[0]
            .auxiliary
            .push(AuxiliaryObservation {
                target: AuxiliaryTarget::Host { index: 0 },
                pane: PaneObservation {
                    physical: Some(native_physical_fixture()),
                    state: PanePresence::Alive,
                    tmux_session: Some("same-name".into()),
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
                crate::tui::home::panes::NativePane::Auxiliary(AuxiliaryTarget::Host { index: 0 }),
                None,
                crate::tui::home::panes::PaneIntent::Attach,
            )
            .unwrap();
        respond(Ok(MutationReceipt {
            cursor: RuntimeCursor {
                epoch: "test".into(),
                revision: 2,
            },
            outcome: TerminalTarget {
                physical: Some(native_physical_fixture()),
                tmux_session: "same-name".into(),
                status: TerminalTargetStatus::Exists,
                lifecycle_generation: 0,
                profile: receipt_profile.into(),
            },
        }));
        let committed = Arc::make_mut(&mut snapshot);
        committed.cursor.revision = 2;
        committed.contents.sessions[0].lifecycle_generation = snapshot_generation;
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        assert!(env.view.take_native_attachment().is_none());
        assert!(
            env.view.pending_native_attachment.is_none(),
            "superseded continuation retired"
        );
        assert!(
            env.view.session_feed.can_submit(&id),
            "a confirmed receipt settles the mutation but must not attach a replacement row"
        );
    }
}

#[test]
#[serial]
fn queued_send_keeps_admitted_runtime_before_terminal_writer_dispatch() {
    use crate::daemon::{MutationReceipt, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::PaneIntent;

    let mut stale_continuations = Vec::new();
    for agent in [true, false] {
        for change in ["unchanged", "physical replacement", "same-PTY generation"] {
            let mut env = create_test_env_with_sessions(1);
            let id = env.view.instance_at(0).id.clone();
            env.view.select_session_by_id(&id);
            let mut respond = env.view.session_feed.terminal_driver_for_test();
            let SessionFeedResult::Snapshot(mut snapshot) =
                daemon_snapshot(&env.view, &id, "Stopped")
            else {
                unreachable!()
            };
            let physical = native_physical_fixture();
            let pane = PaneObservation {
                physical: Some(physical.clone()),
                state: PanePresence::Alive,
                tmux_session: Some("queued-original".into()),
                legacy_tool: None,
            };
            let row = &mut Arc::make_mut(&mut snapshot).contents.sessions[0];
            if agent {
                row.agent_pane = pane;
            } else {
                row.auxiliary = vec![AuxiliaryObservation {
                    target: AuxiliaryTarget::Host { index: 0 },
                    pane,
                }];
            }
            env.view
                .session_feed
                .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
            env.view.apply_session_feed();
            let target = if agent {
                LiveSendTarget::Agent
            } else {
                LiveSendTarget::Terminal
            };
            env.view
                .prepare_send_target(&id, target.clone(), "OLD".into(), None)
                .unwrap();
            assert!(env.view.session_feed.has_pending(&id));
            assert!(env.view.take_native_attachment().is_none());

            // This protocol fixture defers consuming the actual writer queue.
            // Physical input fencing is covered by the native input tests.
            let committed = Arc::make_mut(&mut snapshot);
            committed.cursor.revision += 1;
            let row = &mut committed.contents.sessions[0];
            if change != "unchanged" {
                row.lifecycle_generation += 1;
            }
            let pane = if agent {
                &mut row.agent_pane
            } else {
                &mut row.auxiliary[0].pane
            };
            if change == "physical replacement" {
                let mut replacement = (*physical).clone();
                replacement.identity.pane_id = "%2".into();
                pane.physical = Some(Arc::new(replacement));
                pane.tmux_session = Some("queued-replacement".into());
            }
            let terminal_physical = pane.physical.clone();
            let terminal_name = pane.tmux_session.clone().unwrap();
            let terminal_generation = row.lifecycle_generation;
            let terminal_profile = row.profile.clone();
            let terminal = || TerminalTarget {
                physical: terminal_physical.clone(),
                tmux_session: terminal_name.clone(),
                status: TerminalTargetStatus::Exists,
                lifecycle_generation: terminal_generation,
                profile: terminal_profile.clone(),
            };
            env.view
                .session_feed
                .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
            env.view.apply_session_feed();
            respond(Ok(MutationReceipt {
                cursor: snapshot.cursor.clone(),
                outcome: terminal(),
            }));
            env.view.apply_session_feed();
            let old = env.view.take_native_attachment();
            if change == "unchanged" {
                let old = old.expect("unchanged admitted runtime must remain usable");
                assert!(
                    matches!(old.intent, PaneIntent::Send { ref message, .. } if message == "OLD")
                );
                assert!(old.lease.is_valid());
            } else if let Some(old) = old {
                assert!(
                    matches!(old.intent, PaneIntent::Send { ref message, .. } if message == "OLD")
                );
                if old.lease.is_valid() {
                    stale_continuations.push((agent, change));
                }
                old.lease.cancel_continuation();
            }
            assert!(env.view.session_feed.can_submit(&id));
            env.view.info_dialog = None;

            env.view
                .prepare_send_target(&id, target, "FRESH".into(), None)
                .unwrap();
            respond(Ok(MutationReceipt {
                cursor: snapshot.cursor.clone(),
                outcome: terminal(),
            }));
            env.view.apply_session_feed();
            let fresh = env
                .view
                .take_native_attachment()
                .expect("a fresh explicit send may prepare the current runtime");
            assert!(
                matches!(fresh.intent, PaneIntent::Send { ref message, .. } if message == "FRESH")
            );
            assert_eq!(fresh.terminal.physical, terminal().physical);
            assert!(fresh.lease.is_valid());
        }
    }
    assert!(
        stale_continuations.is_empty(),
        "OLD acquired a continuation for a changed runtime: {stale_continuations:?}"
    );
}

#[test]
#[serial]
fn queued_send_real_writer_rejects_changed_scope_before_ensure_and_preserves_fresh_input() {
    use crate::session::{PaneObservation, PanePresence, SessionStore};
    use crate::tmux::test_helpers::{require_tmux, TmuxTestSession};
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::PaneIntent;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    require_tmux!();

    fn await_line(path: &std::path::Path, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let received = std::fs::read_to_string(path).unwrap_or_default();
            if received.lines().any(|line| line == expected) {
                return received;
            }
            assert!(
                Instant::now() < deadline,
                "producer did not acknowledge {expected}"
            );
            std::thread::yield_now();
        }
    }

    let mut violations = Vec::new();
    for replace_physical in [true, false] {
        let mut env = create_test_env_with_sessions(1);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let id = env.view.instance_at(0).id.clone();
        let storage = Storage::open_unwatched("test").unwrap();
        let row = storage
            .update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                row.source_profile = "test".into();
                row.tool = "bash".into();
                row.command = "/bin/sh".into();
                row.status = Status::Idle;
                row.lifecycle_generation = 7;
                Ok(row.clone())
            })
            .unwrap();
        let received = crate::session::get_app_dir()
            .unwrap()
            .join("queued-writer-native-receipts");
        let quoted = crate::session::environment::shell_escape(received.to_str().unwrap());
        let command = format!(
            "stty -echo; printf 'READY\\n' > {quoted}; while IFS= read -r line; do printf 'ACK:%s\\n' \"$line\" >> {quoted}; done"
        );
        let name = crate::tmux::Session::generate_name(&id, &row.title);
        let _pane = TmuxTestSession::from_name(&name);
        let session = crate::tmux::Session::from_name(&name);
        session.create("/tmp", Some(&command), "test").unwrap();
        await_line(&received, "READY");
        let (original, probe, _) =
            crate::tmux::utils::observe_pane(&name, &crate::tmux::TmuxCommandDeadline::new())
                .unwrap()
                .unwrap();
        assert_eq!(probe, crate::tmux::utils::PaneProbe::Alive);
        let original = Arc::new(original);
        let state = runtime.block_on(native_state(&["test"]));
        runtime.block_on(async {
            state.instances.write().await[0].agent_pane = PaneObservation {
                state: PanePresence::Alive,
                tmux_session: Some(name.clone()),
                physical: Some(original.clone()),
                legacy_tool: None,
            };
            test_support::refresh_canonical_metadata_for_test(&state).await;
        });
        runtime.block_on(apply_published(&mut env.view, &state));
        env.view.select_session_by_id(&id);
        let epoch = env
            .view
            .session_feed
            .applied_snapshot()
            .unwrap()
            .cursor
            .epoch
            .clone();

        let ensure_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = ensure_calls.clone();
        let app =
            test_support::build_router_for_test(state.clone()).layer(axum::middleware::from_fn(
                move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                    let observed_calls = observed_calls.clone();
                    async move {
                        // Private loopback fixture transport uses the existing owner-peer
                        // extension; the handler and native lifecycle are not substituted.
                        request.extensions_mut().insert(axum::extract::ConnectInfo(
                            crate::server::peer::ConnectionPeer::UnixOwner {
                                uid: nix::unistd::geteuid().as_raw(),
                            },
                        ));
                        if request.method() == axum::http::Method::POST
                            && request.uri().path().ends_with("/ensure")
                        {
                            observed_calls.fetch_add(1, Ordering::SeqCst);
                        }
                        next.run(request).await
                    }
                },
            ));
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            crate::daemon::DaemonClient::new(&format!("http://localhost:{port}"), None).unwrap();
        let write = env.view.session_feed.command_writer_for_test(client, epoch);
        let (entered, at_gate) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let writer = runtime.spawn(async move {
            entered.send(()).unwrap();
            if tokio::time::timeout(Duration::from_secs(10), released)
                .await
                .expect("release the writer before dispatch")
                .is_ok()
            {
                write.await;
            }
        });
        env.view
            .prepare_send_target(&id, LiveSendTarget::Agent, "OLD".into(), None)
            .unwrap();
        assert!(env.view.session_feed.has_pending(&id));
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), at_gate)
                .await
                .expect("the real writer reaches its pre-dispatch gate")
                .unwrap();
        });

        // The actual command is queued, but the real writer has not yet polled it.
        // Its NativeLease::is_valid check therefore runs after this native commit.
        let current = {
            let _runtime_context = runtime.enter();
            let native =
                crate::server::session_store::NativeSessionStore::open(state.clone(), "test", None)
                    .unwrap();
            let _lock = storage.acquire_instance_lifecycle_lock(&id).unwrap();
            if replace_physical {
                session.kill().unwrap();
                session.create("/tmp", Some(&command), "test").unwrap();
            }
            let current = (&native as &dyn SessionStore)
                .update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                    row.lifecycle_generation += 1;
                    row.status = Status::Running;
                    Ok(row.clone())
                })
                .unwrap();
            native.refresh_pane_observations(&current).unwrap();
            current
        };
        await_line(&received, "READY");
        let (physical, probe, _) =
            crate::tmux::utils::observe_pane(&name, &crate::tmux::TmuxCommandDeadline::new())
                .unwrap()
                .unwrap();
        assert_eq!(probe, crate::tmux::utils::PaneProbe::Alive);
        let physical = Arc::new(physical);
        assert_eq!(physical == original, !replace_physical);
        runtime.block_on(apply_published(&mut env.view, &state));
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let old = loop {
            runtime.block_on(apply_published(&mut env.view, &state));
            if let Some(ready) = env.view.take_native_attachment() {
                break Some(ready);
            }
            if !env.view.session_feed.has_pending(&id) {
                break None;
            }
            assert!(
                Instant::now() < deadline,
                "old writer request did not settle"
            );
            std::thread::yield_now();
        };
        let old_ensure_calls = ensure_calls.load(Ordering::SeqCst);
        if let Some(ready) = old {
            let PaneIntent::Send { message, target } = ready.intent else {
                panic!("expected the actual queued OLD send");
            };
            env.view
                .finish_send(&id, &ready.terminal, target, &message, &ready.lease);
        }
        while env.view.session_feed.has_pending(&id) {
            runtime.block_on(apply_published(&mut env.view, &state));
            assert!(
                Instant::now() < deadline,
                "post-send command did not settle"
            );
            std::thread::yield_now();
        }
        env.view.info_dialog = None;
        env.view
            .prepare_send_target(&id, LiveSendTarget::Agent, "FRESH".into(), None)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let fresh = loop {
            runtime.block_on(apply_published(&mut env.view, &state));
            if let Some(ready) = env.view.take_native_attachment() {
                break ready;
            }
            assert!(
                Instant::now() < deadline,
                "fresh writer request did not prepare"
            );
            std::thread::yield_now();
        };
        assert_eq!(
            fresh.terminal.lifecycle_generation,
            current.lifecycle_generation
        );
        assert_eq!(fresh.terminal.physical, Some(physical));
        let PaneIntent::Send { message, target } = fresh.intent else {
            panic!("expected the explicit FRESH send");
        };
        env.view
            .finish_send(&id, &fresh.terminal, target, &message, &fresh.lease);
        let received = await_line(&received, "ACK:FRESH");
        if old_ensure_calls != 0 || received.lines().any(|line| line == "ACK:OLD") {
            violations.push((replace_physical, old_ensure_calls, received));
        }
        writer.abort();
        server.abort();
        let _ = runtime.block_on(writer);
        let _ = runtime.block_on(server);
    }
    assert!(
        violations.is_empty(),
        "queued OLD crossed the native generation before real writer dispatch: {violations:?}"
    );
}

#[test]
#[serial]
fn captured_live_lease_and_agent_readiness_survive_native_rename() {
    use crate::session::{PaneObservation, PanePresence};
    use crate::tmux::test_helpers::{require_tmux, TmuxTestSession};
    use std::time::{Duration, Instant};

    require_tmux!();
    fn await_line(path: &std::path::Path, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .any(|line| line == text)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native actor did not acknowledge {text}"
            );
            std::thread::yield_now();
        }
    }
    let mut env = create_test_env_with_sessions(1);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let id = env.view.instance_at(0).id.clone();
    let storage = Storage::open_unwatched("test").unwrap();
    let row = storage
        .update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == id).unwrap();
            row.source_profile = "test".into();
            row.tool = "bash".into();
            row.command = "/bin/sh".into();
            row.status = Status::Idle;
            row.lifecycle_generation = 7;
            Ok(row.clone())
        })
        .unwrap();
    let received = crate::session::get_app_dir()
        .unwrap()
        .join("rename-native-receipts");
    let quoted = crate::session::environment::shell_escape(received.to_str().unwrap());
    let command = format!(
        "stty -echo; printf 'NATIVE_RENAME_READY_OUTPUT_LONG_ENOUGH\\n'; printf 'READY\\n' > {quoted}; \
         while IFS= read -r line; do printf 'ACK:%s\\n' \"$line\" >> {quoted}; done"
    );
    let name = crate::tmux::Session::generate_name(&id, &row.title);
    let _pane = TmuxTestSession::from_name(&name);
    crate::tmux::Session::from_name(&name)
        .create("/tmp", Some(&command), "test")
        .unwrap();
    await_line(&received, "READY");
    let (physical, probe, _) =
        crate::tmux::utils::observe_pane(&name, &crate::tmux::TmuxCommandDeadline::new())
            .unwrap()
            .unwrap();
    assert_eq!(probe, crate::tmux::utils::PaneProbe::Alive);
    let physical = Arc::new(physical);
    let state = runtime.block_on(native_state(&["test"]));
    runtime.block_on(async {
        state.instances.write().await[0].agent_pane = PaneObservation {
            state: PanePresence::Alive,
            tmux_session: Some(name.clone()),
            physical: Some(physical.clone()),
            legacy_tool: None,
        };
        test_support::refresh_canonical_metadata_for_test(&state).await;
    });
    // The isolated Home fixture has no daemon command lane until installed.
    // Retain a live receiver, as a connected TUI does; use the normal physical lease.
    let _commands = env.view.session_feed.terminal_driver_for_test();
    runtime.block_on(apply_published(&mut env.view, &state));
    assert!(env.view.session_feed.current_snapshot_applied());
    assert!(env.view.session_feed.native_interaction_available());
    let lease = env
        .view
        .session_feed
        .preview_input_lease(&id, &name)
        .unwrap();
    let raw = lease.input_session().unwrap();

    let (status, body) = runtime.block_on(request(
        &state,
        "PATCH",
        &format!("/api/sessions/{id}"),
        serde_json::json!({"title": "renamed native owner"}),
    ));
    assert_eq!(status, StatusCode::OK, "{body}");
    let renamed = runtime.block_on(async { state.instances.read().await[0].clone() });
    let renamed_name = crate::tmux::Session::generate_name(&id, &renamed.title);
    let _renamed_pane = TmuxTestSession::from_name(&renamed_name);
    assert_ne!(renamed_name, name);
    assert_eq!(renamed.lifecycle_generation, row.lifecycle_generation);
    let observed =
        crate::tmux::utils::observe_pane(&renamed_name, &crate::tmux::TmuxCommandDeadline::new())
            .unwrap()
            .unwrap()
            .0;
    assert_eq!(&observed, physical.as_ref());
    runtime.block_on(async {
        let worker_state = state.clone();
        let owner = renamed.clone();
        tokio::task::spawn_blocking(move || {
            let native =
                crate::server::session_store::NativeSessionStore::open(worker_state, "test", None)?;
            native.refresh_pane_observations(&owner)
        })
        .await
        .unwrap()
        .unwrap();
    });
    runtime.block_on(apply_published(&mut env.view, &state));
    assert!(
        lease.is_valid(),
        "display-name change revoked a living physical owner"
    );
    assert!(raw.capture_pane(5).unwrap().contains("NATIVE_RENAME_READY"));
    raw.send_live_raw_bytes(b"RAW\n").unwrap();
    await_line(&received, "ACK:RAW");
    let programmatic = renamed
        .agent_input_session(
            &name,
            physical.clone(),
            true,
            Some(lease.input_fence(true).unwrap()),
        )
        .unwrap();
    programmatic.send_keys("PROGRAM").unwrap();
    await_line(&received, "ACK:PROGRAM");
    assert_eq!(
        storage.load_instances_complete().unwrap()[0].lifecycle_generation,
        7
    );
}

#[test]
#[serial]
fn successful_native_send_preserves_shelving_committed_after_final_enter_ack() {
    use crate::daemon::{MutationReceipt, TerminalTarget, TerminalTargetStatus};
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    use crate::tmux::test_helpers::{require_tmux, TmuxTestSession};
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::panes::PaneIntent;

    require_tmux!();
    fn wait_for_text(target: &str, text: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let output = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", target])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == text)
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "missing {text:?}");
            std::thread::yield_now();
        }
    }

    let mut lost_shelving = Vec::new();
    for shelving in ["archive", "snooze", "dormancy"] {
        let mut env = create_test_env_with_sessions(1);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let id = env.view.instance_at(0).id.clone();
        let storage = Storage::open_unwatched("test").unwrap();
        storage
            .update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                row.status = Status::Idle;
                row.lifecycle_generation = 7;
                row.last_accessed_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
                Ok(())
            })
            .unwrap();
        let state = runtime.block_on(native_state(&["test"]));
        runtime.block_on(apply_published(&mut env.view, &state));
        env.view.select_session_by_id(&id);

        let guard = TmuxTestSession::new("aoe_test_send_shelving");
        crate::tmux::Session::from_name(guard.name())
            .create(
                "/tmp",
                Some(
                    "stty -echo; printf 'SEND-READY\\n'; while IFS= read -r line; do printf 'SEND-ACK:%s\\n' \"$line\"; done",
                ),
                "test",
            )
            .unwrap();
        let (physical, presence, _) = crate::tmux::utils::observe_pane(
            guard.name(),
            &crate::tmux::TmuxCommandDeadline::new(),
        )
        .unwrap()
        .expect("native send pane");
        assert_eq!(presence, crate::tmux::utils::PaneProbe::Alive);
        let physical = Arc::new(physical);
        wait_for_text(&physical.identity.pane_id, "SEND-READY");

        let mut prepare = env.view.session_feed.terminal_driver_for_test();
        let mut snapshot = published_fixture_for_send(&env.view, &id);
        Arc::make_mut(&mut snapshot).contents.sessions[0].auxiliary = vec![AuxiliaryObservation {
            target: AuxiliaryTarget::Host { index: 0 },
            pane: PaneObservation {
                state: PanePresence::Alive,
                tmux_session: Some(guard.name().into()),
                physical: Some(physical.clone()),
                legacy_tool: None,
            },
        }];
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        env.view
            .prepare_send_target(&id, LiveSendTarget::Terminal, "COMPLETED".into(), None)
            .unwrap();
        prepare(Ok(MutationReceipt {
            cursor: snapshot.cursor.clone(),
            outcome: TerminalTarget {
                tmux_session: guard.name().into(),
                physical: Some(physical.clone()),
                status: TerminalTargetStatus::Exists,
                lifecycle_generation: 7,
                profile: "test".into(),
            },
        }));
        env.view.apply_session_feed();
        let ready = env
            .view
            .take_native_attachment()
            .expect("prepared real pane");
        let PaneIntent::Send { message, target } = ready.intent else {
            panic!("expected captured send intent");
        };
        let command_epoch = env
            .view
            .session_feed
            .applied_snapshot()
            .unwrap()
            .cursor
            .epoch
            .clone();
        let mut dispatch =
            env.view
                .session_feed
                .command_dispatch_driver_for_test(|submitted, mutation| {
                    use crate::daemon::SessionMutation;
                    let method = match mutation {
                        SessionMutation::Start(_)
                        | SessionMutation::Restart(_)
                        | SessionMutation::AbandonPurge(_)
                        | SessionMutation::StopAuxiliary(_)
                        | SessionMutation::Stop => "POST",
                        _ => "PATCH",
                    };
                    let (status, headers, body) = runtime.block_on(request_with_headers_at_epoch(
                        &state,
                        method,
                        &format!("/api/sessions/{submitted}/{}", mutation.route()),
                        serde_json::to_value(mutation).unwrap(),
                        &command_epoch,
                    ));
                    assert!(status.is_success(), "{shelving}: {status} {body}");
                    Ok(super::pickers_groups_sort::receipt_cursor(&headers))
                });
        let (committed, ack) = std::sync::mpsc::sync_channel(1);
        let handle = runtime.handle().clone();
        let hook_state = state.clone();
        let hook_id = id.clone();
        // Run only after real native delivery, including the final Enter ACK.
        // The hook commits metadata; it does not stand in for native input.
        let _hook = super::super::send::post_send_test_hook::install(&id, move || {
            let storage = Storage::open_unwatched("test").unwrap();
            storage
                .update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == hook_id).unwrap();
                    match shelving {
                        "archive" => row.archive(),
                        "snooze" => row.snooze(120),
                        "dormancy" => row.idle_dormant_since = Some(chrono::Utc::now()),
                        _ => unreachable!(),
                    }
                    Ok(())
                })
                .unwrap();
            let mut rows = storage.load().unwrap();
            for row in &mut rows {
                row.source_profile = "test".into();
            }
            let row = rows.iter().find(|row| row.id == hook_id).unwrap();
            let expected = (row.archived_at, row.snoozed_until, row.idle_dormant_since);
            handle.block_on(async {
                *hook_state.instances.write().await = rows;
                test_support::refresh_canonical_metadata_for_test(&hook_state).await;
                hook_state.runtime.publish(&hook_state).await.unwrap();
            });
            committed.send(expected).unwrap();
        });
        env.view
            .finish_send(&id, &ready.terminal, target, &message, &ready.lease);
        assert!(
            env.view.info_dialog.is_none(),
            "{shelving}: native send failed"
        );
        let expected = ack
            .try_recv()
            .expect("post-Enter shelving commit acknowledged");
        assert!(
            expected.0.is_some() || expected.1.is_some() || expected.2.is_some(),
            "{shelving}: shelving must commit before processing the old completion"
        );
        wait_for_text(&physical.identity.pane_id, "SEND-ACK:COMPLETED");

        if env.view.session_feed.has_pending(&id) {
            let submitted = dispatch().expect("process the actual queued post-send mutation");
            assert_eq!(submitted, id);
            runtime.block_on(apply_published(&mut env.view, &state));
            assert!(env.view.session_feed.can_submit(&id));
        }
        let rows = storage.load().unwrap();
        let row = rows.iter().find(|row| row.id == id).unwrap();
        if (row.archived_at, row.snoozed_until, row.idle_dormant_since) != expected {
            lost_shelving.push(shelving);
        }
    }
    assert!(
        lost_shelving.is_empty(),
        "the old send completion erased newer shelving: {lost_shelving:?}"
    );

    fn published_fixture_for_send(
        view: &HomeView,
        id: &str,
    ) -> Arc<crate::daemon::RuntimeSnapshot> {
        let SessionFeedResult::Snapshot(snapshot) = daemon_snapshot(view, id, "Idle") else {
            unreachable!()
        };
        let mut snapshot = (*snapshot).clone();
        let applied = view.session_feed.applied_snapshot().unwrap();
        snapshot.cursor.epoch = applied.cursor.epoch.clone();
        Arc::new(snapshot)
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
            physical: None,
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
    let mut evidence = crate::session::OperationErrorEvidence::new(
        crate::session::ErrorEvidenceSource::TmuxObservation,
        crate::session::ErrorOperation::TerminalObservation,
    );
    evidence.runtime = crate::session::RuntimeEvidence::PaneExited { exit_code: None };
    // Seed the row stale the way a live Running terminal looks before the
    // daemon reports the stop.
    env.view.mutate_instance(&id, |instance| {
        instance.view = crate::session::View::Terminal;
        instance.status = Status::Running;
        instance.last_error = Some("stale local probe error".into());
        instance.pane_dead_observed = false;
        instance.agent_pane = PaneObservation {
            physical: None,
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
        row.last_error_evidence = Some(evidence);
        row.agent_pane = PaneObservation {
            physical: None,
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
    assert_eq!(instance.last_error_evidence, Some(evidence));
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
fn native_preparation_waits_for_its_receipt_and_lifecycle_generation() {
    use crate::daemon::{MutationReceipt, RuntimeCursor, TerminalTarget, TerminalTargetStatus};
    use crate::session::{PaneObservation, PanePresence};
    use crate::tui::home::panes::{NativePane, PaneIntent};

    for (reflected_generation, target_name, changed_physical) in [
        (4, "prepared-agent", false),
        (5, "prepared-agent", false),
        (4, "renamed-agent", false),
        (4, "prepared-agent", true),
    ] {
        let mut env = create_test_env_with_sessions(1);
        let id = env.view.instance_at(0).id.clone();
        env.view.select_session_by_id(&id);
        let mut row = daemon_row(env.view.get_instance(&id).unwrap(), "Running");
        row.view = crate::session::View::Terminal;
        row.lifecycle_generation = 4;
        row.agent_pane = PaneObservation {
            physical: Some(native_physical_fixture()),
            state: PanePresence::Alive,
            tmux_session: Some("prepared-agent".into()),
            legacy_tool: None,
        };
        let SessionFeedResult::Snapshot(mut snapshot) = snapshot_of(vec![row]) else {
            unreachable!()
        };
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        let mut respond = env.view.session_feed.terminal_driver_for_test();
        env.view
            .prepare_native_attachment(&id, NativePane::Agent, None, PaneIntent::Attach)
            .unwrap();

        std::sync::Arc::make_mut(&mut snapshot).cursor.revision = 2;
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot.clone()));
        env.view.apply_session_feed();
        assert!(!env.view.session_feed.can_submit(&id));
        assert!(env.view.take_native_attachment().is_none());

        respond(Ok(MutationReceipt {
            cursor: RuntimeCursor {
                epoch: "test".into(),
                revision: 3,
            },
            outcome: TerminalTarget {
                physical: Some(native_physical_fixture()),
                tmux_session: "prepared-agent".into(),
                status: TerminalTargetStatus::Alive,
                lifecycle_generation: 4,
                profile: "test".into(),
            },
        }));
        env.view.apply_session_feed();
        assert!(!env.view.session_feed.can_submit(&id));
        assert!(env.view.take_native_attachment().is_none());

        let committed = std::sync::Arc::make_mut(&mut snapshot);
        committed.cursor.revision = 3;
        committed.contents.sessions[0].lifecycle_generation = reflected_generation;
        committed.contents.sessions[0].agent_pane.tmux_session = Some(target_name.into());
        if changed_physical {
            let physical = committed.contents.sessions[0]
                .agent_pane
                .physical
                .as_mut()
                .unwrap();
            Arc::make_mut(physical).identity.pane_id = "%999999999".into();
        }
        env.view
            .session_feed
            .publish_for_test(SessionFeedResult::Snapshot(snapshot));
        env.view.apply_session_feed();
        assert!(env.view.session_feed.can_submit(&id));
        let attachment = env.view.take_native_attachment();
        if reflected_generation == 4 && !changed_physical {
            let attachment = attachment.expect("committed preparation is attachable");
            assert_eq!(attachment.id, id);
            assert_eq!(attachment.terminal.tmux_session, "prepared-agent");
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
fn canonical_peer_addition_preserves_live_send_and_drafts_when_active_row_is_hidden() {
    let mut env = create_test_env_empty();
    let active = local_row(&mut env, "active-live", "/tmp/active-live");
    let active_id = active.id.clone();
    seed_profile("test", std::slice::from_ref(&active));
    let _native_driver = env.view.session_feed.terminal_driver_for_test();
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&active, serde_json::json!({}))],
        vec![],
        1,
    );
    let native_lease = env.view.session_feed.native_lease_for_test();
    assert!(native_lease.is_valid());
    env.view.select_session_by_id(&active_id);
    let tmux_name = crate::tmux::Session::resolve_name(&active_id, "active-live");
    env.view.live_send = Some(live_send_state(&active_id, "active-live", &tmux_name));
    env.view.pending_paste = Some("queued paste".into());
    env.view.preview_capture_target = Some(tmux_name.clone());
    env.view.live_send_last_resize = Some((100, 30));
    env.view.trashed_section_collapsed = true;
    env.view.search_active = true;
    env.view.search_query = Input::new("peer-added".into());

    let mut hidden = active.clone();
    hidden.trash();
    let mut peer = Instance::new("peer-added", "/tmp/peer-added");
    peer.source_profile = "test".into();
    seed_profile("test", &[hidden.clone(), peer.clone()]);
    publish_canonical_snapshot(
        &mut env,
        vec![
            canonical_row(&hidden, serde_json::json!({})),
            canonical_row(&peer, serde_json::json!({})),
        ],
        vec![],
        2,
    );

    let view = &env.view;
    assert!(
        native_lease.is_valid(),
        "unchanged native authority remains live"
    );
    assert!(view
        .get_instance(&active_id)
        .is_some_and(Instance::is_trashed));
    assert!(
        !view
            .flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &active_id)),
        "precondition: the active row moved under the collapsed trash shelf"
    );
    assert_eq!(view.selected_session.as_deref(), Some(active_id.as_str()));
    assert_eq!(view.pending_paste.as_deref(), Some("queued paste"));
    assert_eq!(
        view.preview_capture_target.as_deref(),
        Some(tmux_name.as_str())
    );
    assert_eq!(view.live_send_last_resize, Some((100, 30)));
    assert_eq!(
        view.displayed_pane_tmux_name().as_deref(),
        Some(tmux_name.as_str()),
        "preview capture must stay pinned to the live-send pane"
    );
    let peer_index = view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if id == &peer.id))
        .expect("peer row remains visible");
    assert!(view.search_matches.contains(&peer_index));
    assert!(
        !view.is_sidebar_item_selected(&view.flat_items[peer_index], peer_index),
        "cursor fallback must not visibly retarget selection during live-send"
    );
    let live = view.live_send.as_ref().expect("live-send remains active");
    assert_eq!(
        (
            live.session_id.as_str(),
            live.title.as_str(),
            live.tmux_name.as_str()
        ),
        (active_id.as_str(), "active-live", tmux_name.as_str())
    );
    assert_eq!(
        live.target,
        crate::tui::home::live_send::LiveSendTarget::Agent
    );
}

#[test]
#[serial]
fn canonical_revision_reloads_renames_moves_and_view_changes() {
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "alpha", "/tmp/repo");
    let id = instance.id.clone();
    seed_profile("test", &env.view.cloned_instances());
    let _native_driver = env.view.session_feed.terminal_driver_for_test();
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&instance, serde_json::json!({}))],
        vec![],
        1,
    );
    env.view.select_session_by_id(&id);
    let tmux_name = crate::tmux::Session::resolve_name(&id, &instance.title);
    env.view.live_send = Some(live_send_state(&id, &instance.title, &tmux_name));
    env.view.pending_paste = Some("rename draft".into());

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
    publish_canonical_snapshot(&mut env, vec![row], vec![], 2);

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
    assert!(env.view.session_feed.native_interaction_available());
    assert!(
        env.view.live_send.is_none(),
        "canonical rename revokes the stale live target"
    );
    assert!(!env.view.is_live_send_capturing());
    assert!(env.view.info_dialog.is_some());
    assert_eq!(env.view.selected_session.as_deref(), Some(id.as_str()));
    assert_eq!(env.view.pending_paste.as_deref(), Some("rename draft"));
}

#[test]
#[serial]
fn canonical_revision_drops_a_row_the_daemon_removed() {
    let mut env = create_test_env_empty();
    let kept = local_row(&mut env, "kept", "/tmp/kept");
    let removed = local_row(&mut env, "removed", "/tmp/removed");
    seed_profile("test", &env.view.cloned_instances());
    let _native_driver = env.view.session_feed.terminal_driver_for_test();
    publish_canonical_snapshot(
        &mut env,
        vec![
            canonical_row(&kept, serde_json::json!({})),
            canonical_row(&removed, serde_json::json!({})),
        ],
        vec![],
        1,
    );
    env.view.select_session_by_id(&removed.id);
    let tmux_name = crate::tmux::Session::resolve_name(&removed.id, &removed.title);
    env.view.live_send = Some(live_send_state(&removed.id, &removed.title, &tmux_name));
    env.view.pending_paste = Some("removal draft".into());
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
        2,
    );

    assert!(env.view.get_instance(&kept.id).is_some());
    assert!(
        env.view.get_instance(&removed.id).is_none(),
        "a row absent from the canonical revision must not linger"
    );
    assert!(env.view.session_feed.native_interaction_available());
    assert!(
        env.view.live_send.is_none(),
        "canonical removal revokes the stale live target"
    );
    assert!(!env.view.is_live_send_capturing());
    assert!(env.view.info_dialog.is_some());
    assert_eq!(env.view.selected_session.as_deref(), Some(kept.id.as_str()));
    assert_eq!(env.view.pending_paste.as_deref(), Some("removal draft"));
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
        assert!(env.view.reload_failure_state.has_any_failure());
        env.view.try_present_reload_failure_dialog();
        assert_eq!(
            env.view.info_dialog.as_ref().map(InfoDialog::title),
            Some("Reload Failed")
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
            std::fs::write(&sessions_path, valid).unwrap();
            assert!(env
                .view
                .session_feed_reload_retry_at
                .is_some_and(|deadline| deadline > std::time::Instant::now()));
            publish_canonical_snapshot(
                &mut env,
                vec![canonical_row(&durable, serde_json::json!({}))],
                vec![],
                3,
            );
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
    env.view.tool_configs.insert(
        "probe".into(),
        crate::session::config::ToolSessionConfig {
            command: "sleep 600".into(),
            ..Default::default()
        },
    );
    let instance = local_row(&mut env, "Row", "/tmp/repo");
    let id = instance.id.clone();
    seed_profile("test", &env.view.cloned_instances());
    let target = AuxiliaryTarget::Tool {
        tool_name: "probe".into(),
    };
    let pane = PaneObservation {
        physical: None,
        state: PanePresence::Unknown,
        tmux_session: Some("captured-tool".into()),
        legacy_tool: Some(observed_legacy_tool(LegacyToolIdentity {
            session_id: "$42".into(),
            pane_id: "%42".into(),
            pane_pid: Some(4242),
        })),
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
        pane.legacy_tool.as_ref().unwrap().identity
    );
    assert_eq!(
        captured.adoption.as_ref().unwrap().server,
        pane.legacy_tool.as_ref().unwrap().server
    );
    assert_eq!(captured.adoption.as_ref().unwrap().profile, "test");
    assert_eq!(captured.adoption.as_ref().unwrap().lifecycle_generation, 7);
    let replacement = PaneObservation {
        physical: None,
        state: PanePresence::Unknown,
        tmux_session: Some("replacement-tool".into()),
        legacy_tool: Some(observed_legacy_tool(LegacyToolIdentity {
            session_id: "$43".into(),
            pane_id: "%43".into(),
            pane_pid: Some(4343),
        })),
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
                physical: None,
                state: PanePresence::Unknown,
                tmux_session: Some("captured-tool".into()),
                legacy_tool: Some(observed_legacy_tool(LegacyToolIdentity {
                    session_id: "$42".into(),
                    pane_id: "%42".into(),
                    pane_pid: Some(4242),
                })),
            };
            let expected = crate::session::LegacyToolAdoption {
                tmux_session: "captured-tool".into(),
                identity: pane.legacy_tool.as_ref().unwrap().identity.clone(),
                server: pane.legacy_tool.as_ref().unwrap().server.clone(),
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
                physical: None,
                state: PanePresence::Unknown,
                tmux_session: Some("replacement-tool".into()),
                legacy_tool: Some(observed_legacy_tool(LegacyToolIdentity {
                    session_id: "$43".into(),
                    pane_id: "%43".into(),
                    pane_pid: Some(4343),
                })),
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
                        physical: Some(native_physical_fixture()),
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
                    physical: Some(native_physical_fixture()),
                    tmux_session: "owned-pane".into(),
                    status: TerminalTargetStatus::Exists,
                    lifecycle_generation: 0,
                    profile: "test".into(),
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

#[test]
#[serial]
fn absent_unconfigured_tool_remains_recoverable_without_native_preparation() {
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PaneObservation, PanePresence};
    let mut env = create_test_env_empty();
    let instance = local_row(&mut env, "Row", "/tmp/repo");
    let id = instance.id.clone();
    seed_profile("test", &env.view.cloned_instances());
    env.view.tool_configs.clear();
    let target = AuxiliaryTarget::Tool {
        tool_name: "removed-raw-tool".into(),
    };
    let row = canonical_row(
        &instance,
        serde_json::json!({
            "auxiliary": [AuxiliaryObservation {
                target: target.clone(),
                pane: PaneObservation { state: PanePresence::Absent, tmux_session: None, legacy_tool: None, physical: None },
            }],
        }),
    );
    let mut respond = env.view.session_feed.command_driver_for_test();
    publish_canonical_snapshot(&mut env, vec![row], vec![], 1);
    env.view.select_session_by_id(&id);
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char(';'), KeyModifiers::NONE), None);
    assert!(
        env.view.tool_picker_dialog.is_some(),
        "pending raw name must remain discoverable after restart without config"
    );
    env.view
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(
        env.view.view_mode,
        ViewMode::Tool("removed-raw-tool".into())
    );
    assert!(env.view.pending_native_attachment.is_none());
    assert!(env.view.live_send.is_none());
    assert!(
        env.view.session_feed.can_submit(&id),
        "recovery selection must not submit Ensure or Send"
    );
    env.view.stop_selected();
    assert!(env.view.confirm_dialog.is_some());
    assert_eq!(
        env.view.pending_stop_auxiliary.as_ref().unwrap().1.target,
        target
    );
    env.view.dispatch_confirm_submit("stop_auxiliary");
    let (sent_id, mutation) = respond(Err("retained original debt".into())).unwrap();
    assert_eq!(sent_id, id);
    let crate::daemon::SessionMutation::StopAuxiliary(sent) = mutation else {
        panic!("wrong mutation")
    };
    assert_eq!(sent.target, target);
    assert!(sent.adoption.is_none());
}

#[test]
#[serial]
fn recovery_only_stop_never_adopts_current_legacy_replacement() {
    use crate::session::{
        AuxiliaryObservation, AuxiliaryTarget, LegacyToolIdentity, PaneObservation, PanePresence,
    };
    for configuration in ["missing", "background", "blank"] {
        let mut env = create_test_env_empty();
        let instance = local_row(&mut env, "Row", "/tmp/repo");
        let id = instance.id.clone();
        seed_profile("test", &env.view.cloned_instances());
        env.view.tool_configs.clear();
        if configuration != "missing" {
            env.view.tool_configs.insert(
                "probe".into(),
                crate::session::config::ToolSessionConfig {
                    command: if configuration == "blank" {
                        " \t".into()
                    } else {
                        "sleep 600".into()
                    },
                    background: configuration == "background",
                    ..Default::default()
                },
            );
        }
        let target = AuxiliaryTarget::Tool {
            tool_name: "probe".into(),
        };
        let row = canonical_row(
            &instance,
            serde_json::json!({
                "auxiliary": [AuxiliaryObservation {
                    target: target.clone(),
                    pane: PaneObservation {
                        state: PanePresence::Unknown,
                        tmux_session: Some("current-legacy-replacement".into()),
                        legacy_tool: Some(observed_legacy_tool(LegacyToolIdentity {
                            session_id: "$42".into(), pane_id: "%42".into(), pane_pid: Some(4242),
                        })),
                        physical: None,
                    },
                }],
            }),
        );
        let mut respond = env.view.session_feed.command_driver_for_test();
        publish_canonical_snapshot(&mut env, vec![row], vec![], 1);
        env.view.select_session_by_id(&id);
        env.view
            .handle_key(KeyEvent::new(KeyCode::Char(';'), KeyModifiers::NONE), None);
        assert!(env.view.tool_picker_dialog.is_some());
        env.view
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(env.view.view_mode, ViewMode::Tool("probe".into()));
        assert!(
            env.view.confirm_dialog.is_none(),
            "selection must not ask to adopt replacement B"
        );
        assert!(env.view.pending_legacy_tool_preparation.is_none());
        assert!(env.view.pending_native_attachment.is_none());
        assert!(env.view.live_send.is_none());
        assert!(
            env.view.session_feed.can_submit(&id),
            "selection must not submit Ensure or Send"
        );
        env.view.stop_selected();
        assert!(env.view.confirm_dialog.is_some());
        let pending = &env.view.pending_stop_auxiliary.as_ref().unwrap().1;
        assert_eq!(pending.target, target);
        assert!(
            pending.adoption.is_none(),
            "{configuration}: original-debt Stop must not capture replacement B"
        );
        env.view.dispatch_confirm_submit("stop_auxiliary");
        let (_, mutation) = respond(Err("original debt retained".into())).unwrap();
        let crate::daemon::SessionMutation::StopAuxiliary(sent) = mutation else {
            panic!("wrong mutation")
        };
        assert_eq!(sent.target, target);
        assert!(sent.adoption.is_none());
    }
}

#[test]
#[serial]
fn disk_ahead_reload_keeps_complete_model_and_newer_frame_applies_without_waiting() {
    let mut env = create_test_env_empty();
    let kept = local_row(&mut env, "kept", "/tmp/kept");
    let removed = local_row(&mut env, "removed", "/tmp/removed");
    seed_profile("test", &env.view.cloned_instances());
    let _transport = env.view.session_feed.command_driver_for_test();
    publish_canonical_snapshot(
        &mut env,
        vec![
            canonical_row(&kept, serde_json::json!({})),
            canonical_row(&removed, serde_json::json!({})),
        ],
        vec![],
        1,
    );
    let before = serde_json::to_value(env.view.cloned_instances()).unwrap();
    env.view.info_dialog = Some(InfoDialog::new("Draft Help", "keep this modal"));
    env.view.pending_paste = Some("draft must survive".into());
    Storage::new_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            rows.retain(|row| row.id != removed.id);
            Ok(())
        })
        .unwrap();
    env.view
        .reload_storage_only()
        .expect("routine disk-ahead reload is recoverable");
    assert_eq!(
        serde_json::to_value(env.view.cloned_instances()).unwrap(),
        before
    );
    assert_eq!(env.view.info_dialog.as_ref().unwrap().title(), "Draft Help");
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("draft must survive")
    );
    assert_eq!(
        env.view
            .session_feed
            .applied_snapshot()
            .unwrap()
            .cursor
            .revision,
        1
    );
    assert!(!env.view.try_present_reload_failure_dialog());
    assert!(env
        .view
        .session_feed_reload_retry_at
        .is_some_and(|deadline| deadline > std::time::Instant::now()));
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&kept, serde_json::json!({}))],
        vec![],
        2,
    );
    assert!(env.view.get_instance(&removed.id).is_none());
    assert_eq!(
        env.view
            .session_feed
            .applied_snapshot()
            .unwrap()
            .cursor
            .revision,
        2
    );
    assert_eq!(env.view.info_dialog.as_ref().unwrap().title(), "Draft Help");
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("draft must survive")
    );
    assert!(
        !env.view.apply_session_feed(),
        "complete cursor is acknowledged once"
    );
}

#[test]
#[serial]
fn persistent_metadata_lag_latches_across_revisions_without_focus_theft() {
    let mut env = create_test_env_empty();
    let row = local_row(&mut env, "row", "/tmp/repo");
    seed_profile("test", &env.view.cloned_instances());
    let _transport = env.view.session_feed.command_driver_for_test();
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&row, serde_json::json!({}))],
        vec![],
        1,
    );
    seed_profile("test", &[]);
    env.view.reload_storage_only().unwrap();
    let first = env.view.canonical_metadata_lag.as_mut().unwrap();
    first.first_seen = std::time::Instant::now() - HomeView::CANONICAL_RELOAD_RETRY_INTERVAL;
    let first_seen = first.first_seen;
    env.view.info_dialog = Some(InfoDialog::new("Other", "draft"));
    publish_canonical_snapshot(
        &mut env,
        vec![canonical_row(&row, serde_json::json!({}))],
        vec![],
        2,
    );
    assert_eq!(
        env.view.canonical_metadata_lag.as_ref().unwrap().first_seen,
        first_seen
    );
    assert_eq!(
        env.view
            .session_feed
            .applied_snapshot()
            .unwrap()
            .cursor
            .revision,
        1
    );
    assert!(!env.view.try_present_reload_failure_dialog());
    assert_eq!(env.view.info_dialog.as_ref().unwrap().title(), "Other");
    env.view.info_dialog = None;
    assert!(env.view.try_present_reload_failure_dialog());
    env.view.info_dialog = None;
    env.view.session_feed_reload_retry_at = Some(std::time::Instant::now());
    env.view.apply_session_feed();
    assert!(
        !env.view.try_present_reload_failure_dialog(),
        "same failure burst stays acknowledged"
    );
}

#[test]
#[serial]
fn managed_resume_refusal_keeps_recovery_visible_and_restart_available() {
    use crate::tui::home::panes::{NativePane, PaneIntent};
    for action in ["mutation", "ensure"] {
        let mut env = create_test_env_with_sessions(1);
        let id = env.view.instance_at(0).id.clone();
        env.view.select_session_by_id(&id);
        let mut respond = env.view.session_feed.request_driver_for_test();
        env.view
            .session_feed
            .publish_for_test(daemon_snapshot(&env.view, &id, "Stopped"));
        env.view.apply_session_feed();
        match action {
            "ensure" => env
                .view
                .prepare_native_attachment(&id, NativePane::Agent, None, PaneIntent::Attach)
                .unwrap(),
            _ => env
                .view
                .session_feed
                .submit(
                    id.clone(),
                    crate::daemon::SessionMutation::Start(
                        crate::daemon::StartSessionBody::default(),
                    ),
                )
                .unwrap(),
        }
        let refusal = crate::daemon::DaemonClientError::Committed {
            cursor: crate::daemon::RuntimeCursor {
                epoch: "test".into(),
                revision: 2,
            },
            code: crate::daemon::ApiErrorCode::ManagedResumeRefused(
                crate::session::ManagedResumeCause::WrapperContractRequired,
            ),
        };
        assert_eq!(respond(Err(refusal.to_string())).unwrap().0, id);
        env.view.apply_session_feed();
        env.view.apply_restart_results();
        if action != "mutation" {
            assert!(env.view.info_dialog.is_none());
            assert!(env.view.pending_native_attachment.is_some());
            assert!(env.view.take_native_attachment().is_none());
            assert!(env.view.pending_native_attachment.is_none());
            assert!(env.view.live_send.is_none());
        }
        assert!(env.view.session_feed.can_submit(&id));
        assert!(env.view.pending_indeterminate_resolution.is_none());
        for (width, height) in [(80, 24), (120, 40), (200, 60)] {
            let screen = render_home_to_string(&mut env.view, width, height);
            for instruction in [
                if action == "mutation" {
                    "Runtime change"
                } else {
                    "Attachment failed"
                },
                "set-session-id",
                "<session>",
                "<native-id>",
                "--store",
                "session.agent_execution_as",
                "session.agent_config_dir",
                "\"\"",
                "AoE association",
                "[OK]",
            ] {
                assert!(
                    screen.contains(instruction),
                    "{action}: {instruction} is clipped:\n{screen}"
                );
            }
            assert!(!screen.contains("Resolve Unknown Outcome"), "{screen}");
        }
        env.view.handle_key(key(KeyCode::Esc), None);
        assert!(env.view.info_dialog.is_none());
        env.view.strict_hotkeys = false;
        env.view.handle_key(key(KeyCode::Char('e')), None);
        assert!(env.view.restart_dialog.is_some());
        assert!(respond(Err("must not submit before confirmation".into())).is_none());
    }
}

#[test]
#[serial]
fn indeterminate_launch_refuses_attachment_without_resubmitting() {
    use crate::tui::home::panes::{NativePane, PaneIntent};
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.select_session_by_id(&id);
    let mut respond = env.view.session_feed.request_driver_for_test();
    env.view
        .session_feed
        .publish_for_test(daemon_snapshot(&env.view, &id, "Stopped"));
    env.view.apply_session_feed();
    env.view
        .prepare_native_attachment(&id, NativePane::Agent, None, PaneIntent::Attach)
        .unwrap();
    let error = crate::daemon::DaemonClientError::Committed {
        cursor: crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision: 2,
        },
        code: crate::daemon::ApiErrorCode::LaunchIndeterminate,
    };
    assert_eq!(respond(Err(error.to_string())).unwrap().0, id);
    env.view.apply_session_feed();
    env.view.apply_restart_results();
    assert!(env.view.take_native_attachment().is_none());
    assert!(env.view.pending_native_attachment.is_none());
    assert!(env.view.live_send.is_none());
    assert!(env.view.session_feed.can_submit(&id));
    assert!(env.view.pending_indeterminate_resolution.is_none());
    assert!(env.view.info_dialog.is_some());
    assert!(respond(Err("no automatic retry".into())).is_none());
}

#[test]
#[serial]
fn snooze_refusal_preserves_target_duration_and_requires_explicit_resubmit() {
    use crate::tui::dialogs::test_render::{draw, find};
    for mouse in [false, true] {
        let mut env = create_test_env_with_sessions(2);
        let original = env.view.instance_at(0).id.clone();
        let other = env.view.instance_at(1).id.clone();
        let mut respond = env.view.session_feed.command_driver_for_test();
        env.view.select_session_by_id(&original);
        env.view
            .session_feed
            .submit(original.clone(), crate::daemon::SessionMutation::Stop)
            .unwrap();
        env.view.toggle_snooze_at_cursor().unwrap();
        let bytes = payload_bytes("test");
        if mouse {
            let buffer = draw(110, 40, |frame, theme| {
                env.view
                    .render(frame, frame.area(), theme, None, None, None)
            });
            let (column, row) = find(&buffer, "2 hours");
            assert!(env.view.handle_dialog_click(column, row));
        } else {
            env.view.handle_key(key(KeyCode::Char('2')), None);
        }
        assert_eq!(
            env.view.pending_snooze_session.as_deref(),
            Some(original.as_str())
        );
        assert!(env.view.snooze_duration_dialog.is_some());
        assert_eq!(payload_bytes("test"), bytes);
        let buffer = draw(110, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        find(&buffer, "Snooze Not Changed");
        let (submitted, _) = respond(Err("held operation rejected".into())).unwrap();
        assert_eq!(submitted, original);
        env.view.apply_session_feed();
        env.view.select_session_by_id(&other);
        let buffer = draw(110, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        find(&buffer, "Runtime change");
        if mouse {
            let (column, row) = find(&buffer, "[OK]");
            assert!(env.view.handle_dialog_click(column, row));
        } else {
            env.view.handle_key(key(KeyCode::Enter), None);
        }
        assert!(
            env.view.info_dialog.is_none(),
            "visible held-command error was acknowledged through actual input"
        );
        assert!(env.view.snooze_duration_dialog.is_some());
        let buffer = draw(110, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        find(&buffer, "Snooze Not Changed");
        if mouse {
            let (column, row) = find(&buffer, "[OK]");
            assert!(env.view.handle_dialog_click(column, row));
        } else {
            env.view.handle_key(key(KeyCode::Enter), None);
        }
        assert!(
            env.view.snooze_duration_dialog.is_some(),
            "acknowledging refusal retains duration and target"
        );
        assert!(respond(Err("must not submit".into())).is_none());
        env.view.handle_key(key(KeyCode::Enter), None);
        let (submitted, mutation) = respond(Err("test completion".into())).unwrap();
        assert_eq!(submitted, original);
        assert!(
            matches!(mutation, crate::daemon::SessionMutation::Snooze(body) if body.minutes == Some(120))
        );
        assert!(env.view.snooze_duration_dialog.is_none());
    }
}

fn diff_admission_fixture() -> (TestEnv, String, tempfile::TempDir) {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let temporary = tempfile::tempdir().unwrap();
    let repository = git2::Repository::init(temporary.path()).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    std::fs::write(temporary.path().join("draft.txt"), "base\n").unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(std::path::Path::new("draft.txt")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let commit = repository
        .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
        .unwrap();
    repository
        .branch("review", &repository.find_commit(commit).unwrap(), false)
        .unwrap();
    std::fs::write(temporary.path().join("draft.txt"), "changed\n").unwrap();
    index.add_path(std::path::Path::new("draft.txt")).unwrap();
    let changed_tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repository.find_commit(commit).unwrap();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "main-only change",
            &changed_tree,
            &[&parent],
        )
        .unwrap();
    std::fs::write(
        temporary.path().join("draft.txt"),
        "unsaved comparison draft\n",
    )
    .unwrap();
    let rows = env
        .view
        .cloned_instances()
        .iter()
        .map(|row| canonical_row(row, serde_json::json!({})))
        .collect();
    publish_canonical_snapshot(&mut env, rows, vec![], 1);
    env.view.diff_view = Some(
        crate::tui::diff::DiffView::new_for_session(
            temporary.path().to_owned(),
            Some(id.clone()),
            "test".into(),
            Some("main".into()),
            None,
        )
        .unwrap(),
    );
    assert!(env
        .view
        .diff_view
        .as_mut()
        .unwrap()
        .get_current_diff()
        .unwrap()
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .any(|line| line.old_line_num.is_some() && line.content.trim() == "changed"));
    (env, id, temporary)
}

#[test]
#[serial]
fn diff_keyboard_and_mouse_refusal_are_visible_dismissible_and_keep_comparison() {
    use crate::tui::dialogs::test_render::{draw, find};
    for mouse in [false, true] {
        let (mut env, id, _repo) = diff_admission_fixture();
        let mut respond = env.view.session_feed.command_driver_for_test();
        env.view
            .session_feed
            .submit(id.clone(), crate::daemon::SessionMutation::Stop)
            .unwrap();
        let bytes = payload_bytes("test");
        env.view.handle_key(key(KeyCode::Char('b')), None);
        let target = env
            .view
            .diff_view
            .as_ref()
            .unwrap()
            .branch_select
            .as_ref()
            .unwrap()
            .branches
            .iter()
            .position(|branch| branch == "review")
            .unwrap();
        if mouse {
            draw(120, 40, |frame, theme| {
                env.view
                    .render(frame, frame.area(), theme, None, None, None)
            });
            let rect = env
                .view
                .diff_view
                .as_ref()
                .unwrap()
                .branch_mouse
                .rows
                .iter()
                .find(|(index, _)| *index == target)
                .unwrap()
                .1;
            assert!(env.view.handle_dialog_click(rect.x, rect.y));
        } else {
            for _ in 0..target {
                env.view.handle_key(key(KeyCode::Down), None);
            }
            env.view.handle_key(key(KeyCode::Enter), None);
        }
        let diff = env.view.diff_view.as_ref().unwrap();
        assert_eq!(diff.base_branch, "review");
        assert!(diff.base_unsaved);
        assert!(env
            .view
            .diff_view
            .as_mut()
            .unwrap()
            .get_current_diff()
            .unwrap()
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.old_line_num.is_some() && line.content.trim() == "base"));
        let buffer = draw(120, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        find(&buffer, "Diff Base Not Saved");
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(env
            .view
            .diff_view
            .as_ref()
            .unwrap()
            .warning_dialog
            .is_none());
        let buffer = draw(120, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        find(&buffer, "[not saved]");
        assert_eq!(payload_bytes("test"), bytes);
        assert!(respond(Err("original pending stop".into())).is_some());
        assert!(respond(Err("must not have submitted diff".into())).is_none());
    }
}

#[test]
#[serial]
fn diff_save_uses_its_own_receipt_and_surfaces_rejection_or_unknown_in_takeover() {
    use crate::tui::dialogs::test_render::{draw, find};
    for outcome in ["saved", "rejected", "unknown"] {
        let (mut env, id, _repo) = diff_admission_fixture();
        let mut respond = env.view.session_feed.command_driver_for_test();
        env.view
            .diff_view
            .as_mut()
            .unwrap()
            .select_branch("review".into());
        env.view.drain_pending_diff_override();
        assert!(env.view.diff_view.as_ref().unwrap().base_save.is_some());
        env.view.handle_key(key(KeyCode::Char('b')), None);
        assert!(env.view.diff_view.as_ref().unwrap().branch_select.is_some());
        if outcome == "unknown" {
            drop(respond);
            env.view.apply_session_feed();
        } else if outcome == "rejected" {
            let (_, mutation) = respond(Err("policy changed".into())).unwrap();
            assert!(matches!(
                mutation,
                crate::daemon::SessionMutation::DiffBase(_)
            ));
            env.view.apply_session_feed();
        } else {
            respond(Ok(crate::daemon::RuntimeCursor {
                epoch: "test".into(),
                revision: 2,
            }))
            .unwrap();
            env.view.apply_session_feed();
            assert!(
                env.view.diff_view.as_ref().unwrap().base_save.is_some(),
                "receipt alone is insufficient"
            );
            Storage::new_unwatched("test")
                .unwrap()
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .base_branch_override = Some("review".into());
                    Ok(())
                })
                .unwrap();
            let row = canonical_row(
                env.view.get_instance(&id).unwrap(),
                serde_json::json!({"base_branch_override":"review"}),
            );
            publish_canonical_snapshot(&mut env, vec![row], vec![], 2);
        }
        let diff = env.view.diff_view.as_ref().unwrap();
        assert_eq!(diff.base_branch, "review");
        assert!(diff.base_save.is_none());
        assert_eq!(diff.base_unsaved, outcome != "saved");
        let buffer = draw(120, 40, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        if outcome != "saved" {
            find(&buffer, "Diff Base Not Saved");
            if outcome == "unknown" {
                let (column, row) = find(&buffer, "[OK]");
                assert!(env.view.handle_dialog_click(column, row));
            } else {
                env.view.handle_key(key(KeyCode::Enter), None);
            }
            assert!(env
                .view
                .diff_view
                .as_ref()
                .unwrap()
                .warning_dialog
                .is_none());
            assert!(
                env.view.diff_view.as_ref().unwrap().branch_select.is_some(),
                "warning dismissal retains reopened picker"
            );
            assert_eq!(env.view.diff_view.as_ref().unwrap().base_branch, "review");
        }
        if outcome == "unknown" {
            assert!(env
                .view
                .pending_indeterminate_queue
                .iter()
                .any(|(target, _)| target == &id));
        }
    }
}

#[tokio::test]
#[serial]
async fn degraded_stream_revokes_grants_and_project_writes_while_owner_http_is_held() {
    use crate::server::principal::Principal;
    use crate::tui::dialogs::ProjectsDialog;
    use axum::{extract::Extension, routing::get, Router};
    let mut env = create_test_env_with_sessions(1);
    let tmux_socket = tokio::task::spawn_blocking(crate::tmux::native_socket_locator)
        .await
        .unwrap();
    let app = crate::session::get_app_dir()
        .unwrap()
        .canonicalize()
        .unwrap();
    let repo = app.join("admission-project");
    git2::Repository::init(&repo).unwrap();
    crate::session::projects::add(
        "test",
        crate::session::ProjectScope::Global,
        crate::session::Project::new(
            "seed",
            repo.to_string_lossy(),
            crate::session::ProjectScope::Global,
        ),
        false,
    )
    .unwrap();
    let original = native_state(&["test"]).await;
    let mut owned = Arc::try_unwrap(original)
        .ok()
        .expect("fixture has one strong state owner");
    let work = owned.runtime.work.clone();
    owned.runtime = crate::server::runtime::NativeRuntime::new(
        app.to_string_lossy().into_owned(),
        tmux_socket
            .as_deref()
            .and_then(std::path::Path::to_str)
            .map(str::to_owned),
        work,
    );
    let state = Arc::new(owned);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let router = Router::new()
        .route("/api/runtime/ws", get(crate::server::runtime::runtime_ws))
        .route(
            "/api/runtime",
            get({
                let entered = entered.clone();
                let release = release.clone();
                let hits = hits.clone();
                move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        entered.notify_one();
                        release.notified().await;
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        )
        .layer(Extension(Principal::LocalPeer {
            uid: nix::unistd::geteuid().as_raw(),
        }))
        .with_state(state.clone());
    let socket = app.join("daemon/api.sock");
    let server = crate::server::test_support::serve_owner_router_for_test(&socket, router).await;
    let endpoint = crate::acp::client::DaemonEndpoint::local_unix(socket.clone());
    let connection = crate::daemon::RuntimeConnection::connect(&endpoint, Some("test"))
        .await
        .unwrap();
    let client = crate::daemon::DaemonClient::new_unix(socket).unwrap();
    env.view
        .session_feed
        .connect_prepared("test".into(), Some((connection, client.clone())));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            env.view.apply_session_feed();
            if env.view.session_feed.current_snapshot_applied() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let native_lease = env.view.session_feed.native_lease_for_test();
    assert_eq!(
        native_lease.is_valid(),
        tmux_socket.is_some(),
        "native authority is exercised when the actual tmux locator is available"
    );
    let held_get = tokio::spawn(async move { client.runtime_info().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("control owner HTTP request reaches the held endpoint");
    *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
        code: crate::daemon::ReloadFailureCode::ProfileData,
        profiles: vec!["test".into()],
    };
    let degraded = state
        .runtime
        .publish(&state)
        .await
        .unwrap()
        .value
        .cursor
        .clone();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            env.view.apply_session_feed();
            if env.view.session_feed.receipt_applied(&degraded) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("received degraded state is not held behind owner HTTP");
    assert!(!held_get.is_finished());
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!env.view.session_feed.mutations_available());
    assert!(!env.view.session_feed.native_interaction_available());
    assert!(
        !native_lease.is_valid(),
        "queued native dispatch observes the exact revoked grant"
    );
    let profile = crate::session::get_profile_dir("test").unwrap();
    let groups = profile.join("groups.json");
    let profile_projects = profile.join("projects.json");
    std::fs::write(&groups, "[]").unwrap();
    std::fs::write(&profile_projects, "[]").unwrap();
    std::fs::write(&groups, "{ SECRET_SOURCE_A").unwrap();
    let failure = crate::server::reload::load_all_profiles(&state.file_watch)
        .err()
        .unwrap();
    *state.canonical_diagnostics.write().unwrap() = failure.owner_diagnostics();
    *state.canonical_health.write().await = failure.health;
    let first_source_cursor = state
        .runtime
        .publish(&state)
        .await
        .unwrap()
        .value
        .cursor
        .clone();
    assert_eq!(first_source_cursor, degraded);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            env.view.apply_session_feed();
            if env
                .view
                .runtime_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == groups.to_string_lossy())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner diagnostics reach Home without a new canonical revision or HTTP result");
    assert!(env.view.try_present_reload_failure_dialog());
    std::fs::write(&profile_projects, "{ SECRET_SOURCE_B").unwrap();
    std::fs::write(&groups, "[]").unwrap();
    let failure = crate::server::reload::load_all_profiles(&state.file_watch)
        .err()
        .unwrap();
    *state.canonical_diagnostics.write().unwrap() = failure.owner_diagnostics();
    *state.canonical_health.write().await = failure.health;
    assert_eq!(
        state.runtime.publish(&state).await.unwrap().value.cursor,
        degraded
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            env.view.apply_session_feed();
            if env
                .view
                .runtime_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == profile_projects.to_string_lossy())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same-cursor diagnostic-only source switch updates Home");
    assert!(env.view.try_present_reload_failure_dialog());
    let body = env.view.info_dialog.as_ref().unwrap().message();
    assert!(!body.contains("SECRET_SOURCE_A") && !body.contains("SECRET_SOURCE_B"));
    env.view.handle_key(key(KeyCode::Enter), None);
    std::fs::write(&profile_projects, "[]").unwrap();
    // Repair alone does not restore permission before a fresh healthy daemon frame.
    let registry = app.join("projects.json");
    let before = std::fs::read(&registry).unwrap();
    env.view.projects_dialog = Some(ProjectsDialog::new("test"));
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert_eq!(std::fs::read(&registry).unwrap(), before);
    env.view.projects_dialog = Some(ProjectsDialog::new_adding("test"));
    for character in repo.to_string_lossy().chars() {
        env.view.handle_key(key(KeyCode::Char(character)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(std::fs::read(&registry).unwrap(), before);
    release.notify_one();
    assert!(held_get.await.unwrap().is_err());
    state.shutdown.cancel();
    drop(env);
    server.abort();
    let _ = server.await;
}

#[test]
#[serial]
fn projects_home_dispatch_reads_current_policy_for_keyboard_mouse_and_auto_add() {
    use crate::tui::dialogs::test_render::{draw, find};
    for policy in ["read-only", "cityhall", "disconnected"] {
        for mouse in [false, true] {
            for automatic in [false, true] {
                for remove in [false, true] {
                    if automatic && remove {
                        continue;
                    }
                    let mut env = create_test_env_empty();
                    env.view.strict_hotkeys = false;
                    let repo = crate::session::get_app_dir()
                        .unwrap()
                        .join("home-policy-project");
                    git2::Repository::init(&repo).unwrap();
                    if remove {
                        crate::session::projects::add(
                            "test",
                            crate::session::ProjectScope::Global,
                            crate::session::Project::new(
                                "seed",
                                repo.to_string_lossy(),
                                crate::session::ProjectScope::Global,
                            ),
                            false,
                        )
                        .unwrap();
                    }
                    let _transport = env.view.session_feed.command_driver_for_test();
                    publish_canonical_snapshot(&mut env, vec![], vec![], 1);
                    if automatic {
                        env.view.open_project_session_picker();
                    } else {
                        env.view.handle_key(key(KeyCode::Char('p')), None);
                        if !remove {
                            env.view.handle_key(key(KeyCode::Char('a')), None);
                        }
                    }
                    assert!(env.view.projects_dialog.is_some());
                    if !remove {
                        for character in repo.to_string_lossy().chars() {
                            env.view.handle_key(key(KeyCode::Char(character)), None);
                        }
                        env.view.handle_key(key(KeyCode::Tab), None);
                        for character in "retained-base".chars() {
                            env.view.handle_key(key(KeyCode::Char(character)), None);
                        }
                    }
                    let before = draw(120, 42, |frame, theme| {
                        env.view
                            .render(frame, frame.area(), theme, None, None, None)
                    });
                    let gesture = find(&before, if remove { "d remove" } else { "Enter save" });
                    let registry = crate::session::get_app_dir().unwrap().join("projects.json");
                    let bytes = std::fs::read(&registry).ok();
                    match policy {
                        "cityhall" => env.view.session_feed.set_cityhall_for_test(true),
                        "read-only" => env
                            .view
                            .session_feed
                            .set_mutation_permission_for_test(false),
                        "disconnected" => {
                            env.view
                                .session_feed
                                .publish_for_test(SessionFeedResult::Unavailable(
                                    "controlled policy transition".into(),
                                ));
                            env.view.apply_session_feed();
                        }
                        _ => unreachable!(),
                    }
                    assert!(env.view.project_write_block().is_some());
                    if mouse {
                        assert!(env.view.handle_dialog_click(gesture.0, gesture.1));
                    } else {
                        env.view.handle_key(
                            key(if remove {
                                KeyCode::Char('d')
                            } else {
                                KeyCode::Enter
                            }),
                            None,
                        );
                    }
                    assert_eq!(
                        std::fs::read(&registry).ok(),
                        bytes,
                        "{policy} mouse={mouse} automatic={automatic} remove={remove}"
                    );
                    assert!(env.view.projects_dialog.is_some());
                    if policy != "disconnected" {
                        let buffer = draw(120, 42, |frame, theme| {
                            env.view
                                .render(frame, frame.area(), theme, None, None, None)
                        });
                        find(
                            &buffer,
                            if policy == "cityhall" {
                                "The attached runtime serves"
                            } else {
                                "The runtime is read-only"
                            },
                        );
                        if !remove {
                            find(&buffer, "retained-base");
                        }
                    }
                }
            }
        }
    }
}

#[test]
#[serial]
fn retained_serve_takeover_owns_keyboard_and_mouse_without_consuming_other_drafts() {
    use crate::tui::dialogs::test_render::{draw, find};
    for mouse in [false, true] {
        let (mut env, _id, _repo) = diff_admission_fixture();
        let retained_diff = env.view.diff_view.take();
        let _transport = env.view.session_feed.command_driver_for_test();
        env.view.strict_hotkeys = false;
        env.view.handle_key(key(KeyCode::Char('n')), None);
        assert!(env.view.new_dialog.is_some());
        for character in "retained-serve-form".chars() {
            env.view.handle_key(key(KeyCode::Char(character)), None);
        }
        let form = draw(120, 42, |frame, theme| {
            env.view
                .new_dialog
                .as_mut()
                .unwrap()
                .render(frame, frame.area(), theme)
        });
        find(&form, "retained-serve-form");
        env.view.diff_view = retained_diff;
        env.view.pending_paste = Some("retained-serve-paste".into());
        env.view.serve_view = Some(crate::tui::dialogs::ServeView::new());
        let foreground = draw(120, 42, |frame, theme| {
            env.view
                .render(frame, frame.area(), theme, None, None, None)
        });
        let close = find(&foreground, "[Esc] cancel");
        if mouse {
            assert!(env.view.handle_dialog_click(close.0, close.1));
        } else {
            env.view.handle_key(key(KeyCode::Esc), None);
        }
        assert!(
            env.view.serve_view.is_none(),
            "the painted Serve surface owns its dismissal"
        );
        assert!(env.view.diff_view.is_some());
        assert_eq!(env.view.diff_view.as_ref().unwrap().base_branch, "main");
        assert!(
            env.view.new_dialog.is_some(),
            "hidden new-session form did not consume Serve dismissal"
        );
        assert_eq!(
            env.view.pending_paste.as_deref(),
            Some("retained-serve-paste")
        );
        let form = draw(120, 42, |frame, theme| {
            env.view
                .new_dialog
                .as_mut()
                .unwrap()
                .render(frame, frame.area(), theme)
        });
        find(&form, "retained-serve-form");
    }
}

fn observed_legacy_tool(
    identity: crate::session::LegacyToolIdentity,
) -> crate::session::LegacyToolObservation {
    crate::session::LegacyToolObservation {
        identity,
        server: std::sync::Arc::new(crate::session::TmuxServerIdentity {
            socket: "/fixture-tmux.sock".into(),
            pid: 4242,
            birth: crate::process::ProcessBirthIdentity {
                boot_id: "fixture-boot".into(),
                start_id: "fixture-server".into(),
            },
        }),
    }
}
