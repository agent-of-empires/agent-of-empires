/// Structured (ACP) rows get their status from the daemon, because nothing else
/// can tell you what an ACP session is doing: they have no tmux pane, the tmux
/// poller bails on them, and the daemon deliberately never persists their
/// status to `sessions.json` (see the durability contract on
/// `apply_acp_overlay_inplace`). Before this wiring existed the pill sat frozen
/// at whatever creation or an explicit start/stop wrote, for the whole life of
/// the session.
use super::*;
use crate::session::Status;
use crate::tui::daemon_status_poller::DaemonStatusUpdate;

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

fn update(id: &str, status: Status) -> DaemonStatusUpdate {
    DaemonStatusUpdate {
        id: id.to_string(),
        status,
        last_error: None,
        last_accessed_at: None,
        idle_entered_at: None,
    }
}

#[test]
#[serial]
fn daemon_status_moves_a_structured_row_off_idle() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    env.view
        .apply_daemon_status_update(update(&id, Status::Running));

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
        .apply_daemon_status_update(update(&id, Status::Waiting));

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
        .apply_daemon_status_update(update(&id, Status::Idle));

    let inst = env.view.get_instance(&id).expect("row still present");
    assert_eq!(inst.status, Status::Idle);
    assert_eq!(inst.last_error, None, "the phantom container error is gone");
}

#[test]
#[serial]
fn daemon_status_ignores_a_terminal_row() {
    // The tmux poller owns terminal rows. Letting the daemon's copy through
    // would give them two producers fighting on alternating cycles.
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("tmux-session", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.status = Status::Idle;
    let id = inst.id.clone();
    env.view.add_instance(inst);

    env.view
        .apply_daemon_status_update(update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Idle),
        "a terminal row must not be driven by the daemon overlay"
    );
}

#[test]
#[serial]
fn daemon_status_ignores_an_unknown_session_id() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);

    env.view
        .apply_daemon_status_update(update("not-a-session", Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Idle)
    );
}

#[test]
#[serial]
fn request_daemon_status_refresh_is_a_no_op_without_structured_rows() {
    // A terminal-only home view must never talk to the daemon; that would
    // be one HTTP round trip per second for nothing.
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("tmux-session", "/tmp/repo");
    inst.source_profile = "test".to_string();
    let _ = inst.id.clone();
    env.view.add_instance(inst);

    env.view.request_daemon_status_refresh();

    assert!(
        !env.view.pending_daemon_status_refresh,
        "no structured rows means no fetch is issued"
    );
}

/// The in-flight flag is the only thing stopping a slow daemon from
/// accumulating one queued request per tick, so assert the full cycle:
/// a first request arms it, a second while armed is dropped, and draining
/// the worker disarms it so the next tick can fetch again. Asserting only
/// that the flag is still true after the second call would pass even if
/// the second call had enqueued another request.
#[test]
#[serial]
fn request_daemon_status_refresh_arms_and_disarms_the_in_flight_flag() {
    let mut env = create_test_env_empty();
    let _id = structured_row(&mut env, Status::Idle);

    assert!(!env.view.pending_daemon_status_refresh, "starts disarmed");
    env.view.request_daemon_status_refresh();
    assert!(env.view.pending_daemon_status_refresh, "first request arms");

    // While armed, further ticks are dropped at the guard rather than
    // reaching the worker.
    env.view.pending_daemon_status_refresh = true;
    env.view.request_daemon_status_refresh();
    assert!(env.view.pending_daemon_status_refresh);

    // Draining the worker disarms, so the next tick can fetch again. The
    // fetch itself returns empty here (no daemon in the test env), which is
    // the same path a daemon-less TUI takes.
    while env.view.pending_daemon_status_refresh {
        if env.view.apply_daemon_status_updates() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !env.view.pending_daemon_status_refresh,
        "draining the worker disarms the flag"
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
        .apply_daemon_status_update(update(&id, Status::Running));

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
        .apply_daemon_status_update(update(&id, Status::Stopped));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Stopped)
    );
}

/// A row mid-restart has its post-cascade `Instance` delivered by
/// `apply_restart_results`; the daemon's copy landing inside that window
/// races it. `pollable_instances` excludes these rows from the tmux
/// producer, so this producer has to match.
#[test]
#[serial]
fn daemon_status_skips_a_row_mid_restart() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Starting);
    env.view.restart_in_flight.insert(id.clone());

    env.view
        .apply_daemon_status_update(update(&id, Status::Idle));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Starting),
        "the restart cascade owns this row until it reports back"
    );
}

#[test]
#[serial]
fn daemon_status_skips_a_row_mid_recovery() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Starting);
    env.view.recovery_in_flight.insert(id.clone());

    env.view
        .apply_daemon_status_update(update(&id, Status::Idle));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Starting)
    );
}

/// #3201: the daemon owns structured status and deliberately never
/// persists it (`decide_passive_transition` returns `patch: None` for
/// `is_structured()`). The TUI's passive writer must gate the same way, or
/// a `Running`/`Error` stamped mid-turn survives a daemon stop and a TUI
/// restart, with the tmux poller now bailing on structured rows so nothing
/// heals it. The in-memory pill must still move.
#[test]
#[serial]
fn daemon_status_does_not_persist_a_structured_row_to_disk() {
    // Pin the process-global so the assertion cannot depend on it; an
    // Idle -> Running apply never marks unread regardless.
    crate::session::set_unread_enabled(true);
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);
    // `add_instance` only stages the row; flush it to disk as Idle so the
    // passive writer has a durable row to (not) touch.
    env.view.save().expect("seed the structured row on disk");

    env.view
        .apply_daemon_status_update(update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "the daemon reading must still drive the in-memory pill"
    );

    let rows = env.view.storages.get("test").unwrap().load().unwrap();
    let disk = rows.iter().find(|i| i.id == id).expect("disk row present");
    assert_eq!(
        disk.status,
        Status::Idle,
        "structured status must not be passively persisted to sessions.json (#3201)"
    );
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
        .apply_daemon_status_update(update(&id, Status::Idle));

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

/// #3201: `last_error` reconciliation on a same-status daemon tick. An
/// incoming `Some` is authoritative and always replaces the message, even
/// without a status change (gating that write on a transition froze the
/// first error on the row). An incoming `None` is not symmetric: the daemon
/// tracks only ACP errors, so a same-status `None` tick must leave a
/// locally-set message (e.g. the delete-failure text from
/// `apply_deletion_results`) in place rather than wipe it. Clearing across a
/// genuine transition is locked by `daemon_status_clears_a_stale_error_message`.
#[test]
#[serial]
fn daemon_status_reconciles_last_error_on_a_same_status_tick() {
    // (row status, seeded local error, incoming daemon error, expected)
    let cases = [
        // A None tick on an unchanged status keeps the local message.
        (
            Status::Running,
            "delete failed: worktree busy",
            None,
            Some("delete failed: worktree busy"),
        ),
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
        env.view.apply_daemon_status_update(u);

        assert_eq!(
            env.view
                .get_instance(&id)
                .and_then(|i| i.last_error.clone()),
            expected.map(str::to_string),
            "status={status:?} incoming={incoming:?}"
        );
    }
}

/// #3201: a snoozed row must stay live on the daemon path. Snooze is a
/// user-facing triage marker, not a sink like archive or trash;
/// `daemon_status_applies_to` deliberately excludes only archived and
/// trashed rows, never snoozed. This locks against a future edit that adds
/// a symmetric `!is_snoozed()` exclusion and silently freezes snoozed pills.
#[test]
#[serial]
fn daemon_status_applies_to_a_snoozed_structured_row() {
    let mut env = create_test_env_empty();
    let id = structured_row(&mut env, Status::Idle);
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    env.view
        .apply_daemon_status_update(update(&id, Status::Running));

    assert_eq!(
        env.view.get_instance(&id).map(|i| i.status),
        Some(Status::Running),
        "a snoozed row is live triage, not a sink; the daemon overlay must still drive its status (#3201)"
    );
}

/// #3201, reintroducing the #1868 / #2206 guard on the daemon path:
/// `/api/sessions` returns archived and trashed rows, and the
/// `is_archived()` short-circuit that protects the tmux producer lives in
/// `update_status_with_metadata_inner`, a path the daemon overlay never
/// reaches. A sunk row must not be restamped by the daemon reading.
#[test]
#[serial]
fn daemon_status_skips_a_sunk_structured_row() {
    for label in ["archived", "trashed"] {
        let mut env = create_test_env_empty();
        let id = structured_row(&mut env, Status::Idle);
        let now = chrono::Utc::now();
        env.view.mutate_instance(&id, |inst| {
            if label == "archived" {
                inst.archived_at = Some(now);
            } else {
                inst.trashed_at = Some(now);
            }
        });

        env.view
            .apply_daemon_status_update(update(&id, Status::Running));

        assert_eq!(
            env.view.get_instance(&id).map(|i| i.status),
            Some(Status::Idle),
            "a {label} row is sunk; the daemon overlay must not drive its status (#3201)"
        );
    }
}
