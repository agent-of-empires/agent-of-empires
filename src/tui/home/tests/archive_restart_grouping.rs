//! Archive/trash flows, restart, creation env, and grouping.

use super::*;

/// Archiving in the default (non-Attention) sort advances the cursor to the
/// next active session below instead of following the archived row into the
/// Archived section. The section is NOT auto-revealed; its header count is
/// the feedback. The preview follows the new selection through the normal
/// per-frame retarget (cache gates on session id, worker drops stale frames).
#[test]
#[serial]
fn archive_advances_cursor_to_next_session() {
    let mut env = create_test_env_with_sessions(3);
    // Start with the Archived section collapsed (the default).
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    let next_id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a session row below the cursor, got {other:?}"),
    };

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(
        env.view.get_instance(&id).unwrap().is_archived(),
        "the session must be archived"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(next_id.as_str()),
        "selection must advance to the next active session"
    );
    assert!(
        env.view.archived_section_collapsed,
        "single-row archive must not auto-reveal the Archived section"
    );
    match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id: cur, .. }) => {
            assert_eq!(cur, &next_id, "cursor must sit on the next session's row")
        }
        _ => panic!("cursor should be on the next session row"),
    }
}

/// Archiving the bottom session has no row below to advance to, so the
/// cursor falls back to the nearest active session above.
#[test]
#[serial]
fn archive_bottom_row_falls_back_to_session_above() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = true;
    let last = env.view.flat_items.len() - 1;
    env.view.cursor = last;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    let above_id = match env.view.flat_items.get(last - 1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a session row above the cursor, got {other:?}"),
    };

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(above_id.as_str()),
        "with nothing below, selection must land on the session above"
    );
}

/// Archiving the only active session leaves nothing to advance to: the
/// cursor clamps into the remaining list (the Archived section header) and
/// the selection clears instead of pointing at a vanished row.
#[test]
#[serial]
fn archive_last_active_session_clears_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "no active session remains, so nothing should be selected"
    );
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must stay clamped inside the rebuilt list"
    );
}

/// The successor scan must skip rows already parked under an EXPANDED
/// Archived section: archiving the last active row with an archived row
/// visible below clears the selection instead of advancing into the section.
#[test]
#[serial]
fn archive_successor_skips_archived_rows() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;

    // Park the second session first, so an archived row sits below.
    let parked_id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        other => panic!("expected a second session row, got {other:?}"),
    };
    env.view.select_session_by_id(&parked_id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.get_instance(&parked_id).unwrap().is_archived());

    // Archive the remaining active session. The only session row left below
    // the cursor is the parked one, which must NOT become the selection.
    let id = env.view.selected_session.clone().unwrap();
    assert_ne!(
        id, parked_id,
        "selection must have fallen back to the active row"
    );
    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "the cursor must not advance onto a row inside the Archived section"
    );
}

/// Attention sort: archiving the only active session with the Archived
/// section collapsed leaves no session row for `select_top_attention` to
/// land on. Selection must clear (not stay pinned to the invisible archived
/// row) and the cursor must clamp into the shrunken list.
#[test]
#[serial]
fn archive_last_active_session_attention_sort_clears_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sort_order = crate::session::config::SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.archived_section_collapsed = true;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();

    assert!(env.view.get_instance(&id).unwrap().is_archived());
    assert_eq!(
        env.view.selected_session, None,
        "selection must not point at the archived row hidden in the collapsed section"
    );
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must stay clamped inside the rebuilt list"
    );
}

/// Restoring with `z` unarchives the row and keeps it selected, following it
/// back to its real tier. Unarchive does not restart the agent: the row stays
/// Stopped (archive killed its pane) and the user restarts with `e`.
#[test]
#[serial]
fn unarchive_keeps_selection() {
    let mut env = create_test_env_with_sessions(2);
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();

    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.get_instance(&id).unwrap().is_archived());

    // The archive advanced the cursor to the neighbor; navigate back onto
    // the archived row (visible because the section is expanded) to restore.
    env.view.select_session_by_id(&id);
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(
        !env.view.get_instance(&id).unwrap().is_archived(),
        "second toggle unarchives"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "unarchived row stays selected"
    );
    match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id: cur, .. }) => {
            assert_eq!(cur, &id, "cursor follows the unarchived row")
        }
        _ => panic!("cursor should be on the unarchived session row"),
    }
}

/// `restart_selected_session` must drop the press silently when nothing is
/// selected. No restart_with_size call, no save, no cooldown insertion.
#[test]
#[serial]
fn restart_selected_session_noop_with_no_selection() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(env.view.restart_cooldown_at.is_empty());
}

/// Sunk rows (`archived` / `snoozed` / `pane_dead_observed`) and transient
/// lifecycle states (`Creating` / `Deleting`) must skip the restart path.
/// Archive's contract is "don't auto-revive"; restart should respect that.
#[test]
#[serial]
fn restart_selected_session_skips_archived_row() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.mutate_instance(&id, |inst| inst.archive());

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instance_at(0).is_archived(),
        "archive bit should still be set: restart must not unarchive"
    );
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "cooldown should not be set on a skipped restart"
    );
}

#[test]
#[serial]
fn restart_selected_session_skips_snoozed_row_in_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Attention;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.instance_at(0).is_snoozed(),
        "Attention sort: snooze is the user's explicit `don't revive`; restart must not clear it"
    );
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "Attention sort: skipped restart should not set the cooldown"
    );
}

/// Outside Attention sort, the snooze badge / dim styling / `z ` prefix
/// are all invisible, so silently swallowing a restart press on a snoozed
/// row would leave the user staring at an apparently-restartable row that
/// doesn't restart. Wake the snooze and let the restart proceed instead.
#[test]
#[serial]
fn restart_selected_session_wakes_snooze_outside_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.sort_order = SortOrder::Newest;
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    assert!(env.view.instance_at(0).is_snoozed(), "pre-condition");

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        !env.view.instance_at(0).is_snoozed(),
        "Newest sort: restart on a snoozed row must clear the snooze so persisted state matches what's on screen"
    );
    // Restart cooldown gets set because the press wasn't dropped. Bare
    // `restart_selected_session` schedules the actual restart on a
    // worker; we only assert the synchronous bookkeeping here.
    assert!(
        env.view.restart_cooldown_at.contains_key(&id),
        "Newest sort: restart that proceeded must record the cooldown"
    );
}

#[test]
#[serial]
fn restart_selected_session_skips_creating_row() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Creating);

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(env.view.restart_cooldown_at.is_empty());
}

/// The cooldown map debounces rapid presses. A second press within the
/// cooldown window must be dropped before the restart_with_size call
/// would otherwise tear down a still-booting tmux pane.
///
/// We cannot exercise the full restart path under unit tests (no tmux),
/// so this test confirms the cooldown bookkeeping: after the first call
/// inserts an entry, a second call with the same id within the window
/// returns immediately and does not overwrite the timestamp.
#[test]
#[serial]
fn restart_selected_session_debounces_via_cooldown_map() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Seed the cooldown so the next press is debounced. This stands in
    // for the "first restart already ran" precondition: we cannot run
    // restart_with_size in a unit test (no tmux), but the debounce check
    // happens before that, on the cooldown map.
    let now = std::time::Instant::now();
    env.view.restart_cooldown_at.insert(id.clone(), now);

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    let stored = env.view.restart_cooldown_at.get(&id).copied().unwrap();
    assert_eq!(
        stored, now,
        "cooldown timestamp must not be overwritten on a debounced press"
    );
}

/// An engine swap must not carry the old agent's session state to the new
/// one, in memory OR on disk. Session ids are per-agent namespaces, so a
/// carried-over sid makes the next launch emit `--resume <foreign-sid>`; and
/// an in-memory-only reset is reverted by `reconcile_from_disk` (which is why
/// this asserts the disk row too). Follow-on to #3077, which is what made the
/// swap reach disk in the first place.
#[test]
#[serial]
fn restart_selected_session_tool_swap_clears_old_agent_session_state() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    let seed = |inst: &mut Instance| {
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("11111111-2222-3333-4444-555555555555".to_string());
        inst.acp_session_id = Some("acp-sess-1".to_string());
        inst.agent_name = Some("claude-code".to_string());
        inst.acp_effort = Some("high".to_string());
        inst.agent_model = Some("claude-opus-4-7".to_string());
        // The approval posture is deliberately NOT reset; see the comment in
        // `Instance::swap_tool`.
        inst.acp_mode_id = Some("plan".to_string());
    };
    env.view.mutate_instance(&id, seed);
    // Seed the disk row directly rather than through `save()`: `merge_from_tui`
    // syncs only status + launch config, so a `save()` here would leave these
    // fields absent on disk and the disk assertions below would pass
    // vacuously.
    env.view
        .storages
        .get("test")
        .unwrap()
        .update(|instances, _groups| {
            seed(instances.iter_mut().find(|i| i.id == id).unwrap());
            Ok(())
        })
        .unwrap();

    env.view
        .restart_selected_session(None, Some("codex"), None, None)
        .unwrap();

    let inst = env.view.instance_at(0);
    assert_eq!(inst.tool, "codex");
    assert_eq!(inst.agent_session_id, None, "in-memory sid must be dropped");
    assert_eq!(inst.acp_session_id, None);
    assert_eq!(inst.agent_name, None);
    assert_eq!(inst.acp_effort, None);
    assert_eq!(
        inst.agent_model, None,
        "the old agent's model must be dropped"
    );
    assert_eq!(
        inst.acp_mode_id.as_deref(),
        Some("plan"),
        "the approval posture must survive: clearing it resolves the adapter's \
         bypass mode on a yolo_mode row"
    );

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = disk.iter().find(|i| i.id == id).unwrap();
    assert_eq!(
        row.agent_session_id, None,
        "the old engine's sid must be gone from disk too, else reconcile_from_disk \
         restores it and the new engine launches with --resume <foreign-sid>"
    );
    assert_eq!(row.acp_session_id, None);
    assert_eq!(row.agent_name, None);
    assert_eq!(row.agent_model, None);
    assert_eq!(row.acp_mode_id.as_deref(), Some("plan"));
    // Parked, not discarded: the disk row is the one a swap back reads, so this
    // is what makes claude -> codex -> claude resumable. Round-trip mechanics
    // are covered by `swap_tool_parks_and_restores_per_tool_session_ids`.
    let parked = row.prior_tool_session_ids.get("claude").unwrap();
    assert_eq!(
        parked.agent_session_id.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
    assert_eq!(parked.acp_session_id.as_deref(), Some("acp-sess-1"));
}

/// The disk row a tool swap writes must resolve `agent_detect_as` against the
/// session's own profile. `source_profile` is `skip_serializing`, so a
/// storage-loaded row comes back blank and would key the default profile's
/// aliases instead; `detect_as` is not in `reconcile_from_disk`'s carry set,
/// so that wrong value is what the next launch reads.
#[test]
#[serial]
fn restart_selected_session_tool_swap_resolves_detect_as_for_the_row_profile() {
    // The registries are process-globals and every config resolve in this
    // test (env boot included) rewrites the touched profiles' entries, so
    // snapshot before anything runs and restore on the way out.
    let _registry_test = crate::tmux::status_rules::ProfileRegistryGuard::take("test");
    let _registry_other = crate::tmux::status_rules::ProfileRegistryGuard::take("other");

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // Pin the resolved default profile to something other than the row's own
    // profile, so a blank `source_profile` is observably the wrong key.
    let app_dir = crate::session::get_app_dir().expect("app dir");
    std::fs::create_dir_all(app_dir.join("profiles").join("other")).expect("other profile");
    std::fs::write(app_dir.join("config.toml"), "default_profile = \"other\"\n")
        .expect("global config");

    let mut config = crate::session::Config::default();
    config
        .session
        .agent_detect_as
        .insert("gjc".to_string(), "claude".to_string());
    crate::tmux::status_rules::install_from_config("test", &config);
    crate::tmux::status_rules::install_from_config("other", &crate::session::Config::default());

    env.view
        .restart_selected_session(None, Some("gjc"), None, None)
        .unwrap();

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = disk.iter().find(|i| i.id == id).unwrap();
    assert_eq!(row.tool, "gjc");
    assert_eq!(
        row.detect_as, "claude",
        "the swap must read profile 'test' aliases, not the default profile's"
    );
}

/// Repro for the open CodeRabbit thread on #3509: the tool-swap test above
/// mutates the process-global `agent_detect_as` registry through
/// `install_from_config` without restoring prior entries, and the registry
/// outlives the test, so any later reader of those profiles observes state
/// its config never contained. Sentinel aliases stand in for entries an
/// earlier test installed; both must survive the swap test unchanged.
#[test]
#[serial]
fn tool_swap_test_restores_the_detect_as_registry() {
    // (profile, sentinel agent, target)
    let sentinels = [
        ("test", "zz-sentinel-test", "codex"),
        ("other", "zz-sentinel-other", "claude"),
    ];
    // The probe's own seeds must not leak either: restore the pre-probe
    // entries once the assertion below has run.
    let _registry_test = crate::tmux::status_rules::ProfileRegistryGuard::take("test");
    let _registry_other = crate::tmux::status_rules::ProfileRegistryGuard::take("other");
    for (profile, agent, target) in sentinels {
        let mut seeded = crate::session::Config::default();
        seeded
            .session
            .agent_detect_as
            .insert(agent.to_string(), target.to_string());
        crate::tmux::status_rules::install_from_config(profile, &seeded);
    }

    // serial_test 4's default-key lock is reentrant, so this serialized test
    // can be called directly and observed after its nested guards have dropped.
    restart_selected_session_tool_swap_resolves_detect_as_for_the_row_profile();

    for (profile, agent, target) in sentinels {
        assert_eq!(
            crate::tmux::status_rules::effective_detect_as(profile, agent, ""),
            target,
            "the tool-swap test clobbered pre-existing alias {agent} in profile '{profile}'"
        );
    }
    assert_eq!(
        crate::tmux::status_rules::effective_detect_as("test", "gjc", ""),
        "",
        "the tool-swap test leaked `gjc -> claude` into profile 'test'"
    );
}

#[test]
#[serial]
fn restart_selected_session_surfaces_resume_failed_after_async_restart() {
    if crate::tmux::tmux_command().arg("-V").output().is_err() {
        eprintln!("Skipping: tmux not available");
        return;
    }

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    // The transcript-existence gate (`claude_host_transcript_confirmed_absent`)
    // resolves the Claude home via CLAUDE_CONFIG_DIR before falling back to
    // $HOME/.claude. If the var is set in the invoking environment (running
    // `cargo test` from inside a Claude Code session sets it), the lookup
    // points outside this test's temp home, the seeded transcript reads as
    // absent, and the restart launches fresh-pinned (`--session-id`) instead
    // of driving the --resume cascade this test exercises: no probe, no
    // ResumeFailed, no dialog. Pin the var to the temp home for the duration.
    let claude_home = temp.path().join(".claude");
    let _claude_config_guard =
        crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", claude_home.clone())]);
    crate::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })
    .unwrap();
    let profile = "restart-resume-failed";
    let storage = Storage::new_unwatched(profile).unwrap();
    let stale_sid = "11111111-2222-3333-4444-555555555555";
    // Use an exact built-in binary so the production resume gate passes while
    // the login-shell-safe fake rejects the stale id.
    let _path_guard = crate::session::test_support::install_login_shell_path_command(
        temp.path(),
        "claude",
        "#!/bin/sh\nexit 1\n",
    );
    // The instance workdir is a created tempdir path, not a shared global like
    // /tmp/x: tmux new-session -c on a nonexistent dir fails outright, and a
    // pre-existing /tmp/x on a dev machine would change the launch behavior.
    let workdir = temp.path().join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();
    let workdir_str = workdir.to_str().unwrap().to_string();

    let mut inst = Instance::new("restart-resume-failed", &workdir_str);
    inst.source_profile = profile.to_string();
    inst.tool = "claude".to_string();
    inst.command = "claude".to_string();
    inst.agent_session_id = Some(stale_sid.to_string());
    let id = inst.id.clone();
    let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
    let _ = crate::tmux::tmux_command()
        .args(["kill-session", "-t", &tmux_name])
        .output();

    storage
        .update(|instances, groups| {
            *instances = vec![inst.clone()];
            *groups = GroupTree::new_with_groups(std::slice::from_ref(&inst), &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // A real prior conversation on disk so the restart drives the --resume
    // cascade (and its ResumeFailed path). A stored sid with no transcript now
    // launches fresh-pinned (`--session-id`), which would not surface here.
    // The transcript lookup canonicalizes the project path, so encode the
    // canonical form (the tempdir may sit behind a symlink, e.g. /tmp on
    // macOS).
    let canonical_workdir = std::fs::canonicalize(&workdir).unwrap();
    let claude_dir =
        claude_home
            .join("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &canonical_workdir.to_string_lossy(),
            ));
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(format!("{stale_sid}.jsonl")), "seed\n").unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some(profile.to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.update_selected();
    view.selected_session = Some(id.clone());

    let result = view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());

    let mut applied = false;
    for _ in 0..120 {
        if view.apply_restart_results() {
            applied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = crate::tmux::tmux_command()
        .args(["kill-session", "-t", &tmux_name])
        .output();

    assert!(applied, "timed out waiting for async restart result");
    let row_dbg = view.get_instance(&id).cloned();
    let dialog =
        view.info_dialog.as_ref().unwrap_or_else(|| {
            panic!(
            "resume failure dialog missing; row status={:?} last_error={:?} sid={:?} marker={:?}",
            row_dbg.as_ref().map(|r| r.status),
            row_dbg.as_ref().and_then(|r| r.last_error.clone()),
            row_dbg.as_ref().and_then(|r| r.agent_session_id.clone()),
            row_dbg.as_ref().and_then(|r| r.resume_probe_failed_sid.clone()),
        )
        });
    assert_eq!(dialog.title(), "Restart Failed");
    assert!(
        dialog.message().contains(stale_sid),
        "dialog message was: {}",
        dialog.message()
    );
    let row = view.get_instance(&id).expect("instance remains visible");
    assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid));
    assert_eq!(row.resume_probe_failed_sid.as_deref(), Some(stale_sid));
    assert_eq!(row.status, crate::session::Status::Error);
    assert!(row.last_accessed_at.is_some());
}

#[test]
#[serial]
fn apply_restart_results_preserves_peer_sid_and_marker() {
    use crate::session::StartOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.restart_in_flight.insert(id.clone());
    env.view.instance_at_mut(0).agent_session_id = Some("peer-fresh-sid".to_string());
    env.view.instance_at_mut(0).resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut worker = env.view.instance_at(0).clone();
    worker.status = crate::session::Status::Error;
    worker.agent_session_id = Some("phase1-stale-sid".to_string());
    worker.resume_probe_failed_sid = Some("phase1-stale-sid".to_string());
    worker.last_error =
        Some("resume failed for sid phase1-stale-sid; preserved for explicit retry".to_string());

    env.view.restart_poller = crate::tui::restart_poller::RestartPoller::with_result_for_test(
        crate::session::restart::RestartResult {
            session_id: id.clone(),
            before: Box::new(worker.clone()),
            instance: Box::new(worker),
            outcome: Ok(StartOutcome::ResumeFailed {
                sid: "phase1-stale-sid".to_string(),
            }),
        },
    );

    assert!(env.view.apply_restart_results());

    let row = env
        .view
        .get_instance(&id)
        .expect("instance remains visible");
    assert_eq!(row.status, crate::session::Status::Error);
    assert_eq!(row.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        row.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
    assert!(env.view.restart_in_flight.is_empty());
    let dialog = env
        .view
        .info_dialog
        .as_ref()
        .expect("resume failure dialog");
    assert!(dialog.message().contains("phase1-stale-sid"));
}

#[test]
#[serial]
fn apply_restart_results_propagates_worker_sid_without_peer_write() {
    use crate::session::StartOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.restart_in_flight.insert(id.clone());
    env.view.instance_at_mut(0).agent_session_id = Some("sid-before".to_string());

    let before = env.view.instance_at(0).clone();
    let mut worker = before.clone();
    worker.agent_session_id = Some("sid-after".to_string());
    worker.status = crate::session::Status::Running;

    env.view.restart_poller = crate::tui::restart_poller::RestartPoller::with_result_for_test(
        crate::session::restart::RestartResult {
            session_id: id.clone(),
            before: Box::new(before),
            instance: Box::new(worker),
            outcome: Ok(StartOutcome::Resumed),
        },
    );

    assert!(env.view.apply_restart_results());

    let row = env
        .view
        .get_instance(&id)
        .expect("instance remains visible");
    assert_eq!(row.status, crate::session::Status::Running);
    assert_eq!(row.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(row.resume_probe_failed_sid, None);
    assert!(env.view.restart_in_flight.is_empty());
}

#[test]
#[serial]
fn execute_send_message_missing_session_shows_send_failed() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.instances.shift_remove(&id);

    env.view.execute_send_message(&id, "hello");

    let dialog = env.view.info_dialog.as_ref().expect("send failure dialog");
    assert_eq!(dialog.title(), "Send Failed");
    assert_eq!(
        dialog.message(),
        "Session disappeared before the message could be sent."
    );
}

/// A second restart press while the first cascade is still running on the
/// poller worker must be dropped. The cascade is off the event loop, so the
/// 1.5s keyboard-repeat debounce does not cover a deliberate press during a
/// multi-second pull; without the in-flight guard the worker would enqueue a
/// duplicate request and restart the row twice.
#[test]
#[serial]
fn restart_selected_session_skips_when_already_in_flight() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.restart_in_flight.insert(id.clone());

    let result = env.view.restart_selected_session(None, None, None, None);
    assert!(result.is_ok());
    assert!(
        env.view.restart_cooldown_at.is_empty(),
        "an in-flight restart must drop the press before any bookkeeping"
    );
    assert_ne!(
        env.view.instance_at(0).status,
        crate::session::Status::Starting,
        "the row must not be re-flipped to Starting by a dropped duplicate press"
    );
}

/// Deleting a row whose restart cascade is still running would fire docker
/// commands against the container the worker is mid-creating. The delete must
/// be refused (and surfaced) rather than racing the restart worker.
#[test]
#[serial]
fn delete_selected_refused_during_restart() {
    use crate::tui::dialogs::{DeleteOptions, GroupDeleteOptions};

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.restart_in_flight.insert(id.clone());

    let result = env.view.delete_selected(&DeleteOptions::default());
    assert!(result.is_ok());
    assert_ne!(
        env.view.instance_at(0).status,
        crate::session::Status::Deleting,
        "delete must be refused while a restart is in flight"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "the refused delete must surface a dialog, not silently no-op"
    );
    {
        let storage = env.view.storages.get("test").unwrap();
        storage
            .update(|instances, groups| {
                instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                    .unwrap()
                    .group_path = "work".to_string();
                groups.push(Group::new("work", "work"));
                Ok(())
            })
            .unwrap();
    }
    env.view.selected_session = None;
    env.view.selected_group = Some("work".to_string());
    env.view.selected_group_profile = Some("test".to_string());
    env.view.info_dialog = None;

    env.view
        .delete_group_with_sessions(&GroupDeleteOptions {
            delete_sessions: true,
            delete_worktrees: false,
            delete_branches: false,
            delete_containers: false,
            force_delete_worktrees: false,
        })
        .unwrap();

    assert_eq!(env.view.selected_group.as_deref(), Some("work"));
    assert_eq!(
        env.view.info_dialog.as_ref().map(InfoDialog::title),
        Some("Restart in progress")
    );
    let (instances, groups) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(instances[0].group_path, "work");
    assert!(groups.iter().any(|group| group.path == "work"));
    env.view.restart_in_flight.remove(&id);
    {
        let storage = env.view.storages.get("test").unwrap();
        storage
            .update(|instances, _groups| {
                instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                    .unwrap()
                    .status = crate::session::Status::Creating;
                Ok(())
            })
            .unwrap();
    }
    env.view.info_dialog = None;

    env.view
        .delete_group_with_sessions(&GroupDeleteOptions {
            delete_sessions: true,
            delete_worktrees: false,
            delete_branches: false,
            delete_containers: false,
            force_delete_worktrees: false,
        })
        .unwrap();

    assert_eq!(env.view.selected_group.as_deref(), Some("work"));
    assert_eq!(
        env.view.info_dialog.as_ref().map(InfoDialog::title),
        Some("Creation in progress")
    );
    let (instances, groups) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(instances[0].group_path, "work");
    assert_eq!(instances[0].status, crate::session::Status::Creating);
    assert!(groups.iter().any(|group| group.path == "work"));
}

/// `build_flat_items_by_org` must group sessions by each repo's resolved
/// remote owner (any hosted git remote, not just GitHub), and fall a
/// session with no resolvable owner into the synthetic "No organization"
/// bucket.
#[test]
#[serial]
fn build_flat_items_by_org_groups_by_resolved_owner() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_orgs();
    env.view.group_by = GroupByMode::Org;
    env.view.flat_items = env.view.build_flat_items();

    let mut membership: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut current_group: Option<String> = None;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => current_group = Some(name.clone()),
            Item::Session { id, .. } => {
                if let Some(inst) = env.view.instances().find(|i| &i.id == id) {
                    membership.insert(inst.title.clone(), current_group.clone().unwrap());
                }
            }
        }
    }

    assert_eq!(membership.get("a-session"), Some(&"org-a".to_string()));
    assert_eq!(membership.get("b-session"), Some(&"org-b".to_string()));
    assert_eq!(
        membership.get("no-remote-session"),
        Some(&"No organization".to_string())
    );
}

/// Required-fix regression (#3284 review): two repos owned by the same
/// login on different hosts (GitHub "acme" vs GitLab "acme") must render as
/// two separate org headers, both displayed "acme", and a bulk operation
/// scoped to one must never pull in the other's session.
#[test]
#[serial]
fn build_flat_items_by_org_scopes_same_named_owners_by_host() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_same_owner_two_hosts();
    env.view.group_by = GroupByMode::Org;
    env.view.flat_items = env.view.build_flat_items();

    let group_paths: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Group { path, name, .. } => {
                assert_eq!(name, "acme", "both headers should display the bare owner");
                Some(path.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        group_paths.len(),
        2,
        "same-named owners on different hosts must render as two separate headers, got {group_paths:?}"
    );
    assert_ne!(
        group_paths[0], group_paths[1],
        "the two org headers must have distinct identity keys"
    );

    // Selecting one host's group must not pull in the other host's session.
    let gh_id = env
        .view
        .instances()
        .find(|i| i.title == "gh-session")
        .map(|i| i.id.clone())
        .expect("gh-session instance must exist");
    let gl_id = env
        .view
        .instances()
        .find(|i| i.title == "gl-session")
        .map(|i| i.id.clone())
        .expect("gl-session instance must exist");
    let gh_inst = env.view.get_instance(&gh_id).unwrap();
    let gh_key = env.view.org_group_key(gh_inst);
    env.view.selected_group = Some(gh_key);
    let scoped_ids = env.view.active_sessions_in_selected_group();
    assert_eq!(
        scoped_ids,
        vec![gh_id],
        "archiving the GitHub org header must not include the GitLab session ({gl_id})"
    );
}

/// Project grouping must survive Attention sort. Previously `build_flat_items`
/// short-circuited on `SortOrder::Attention` before checking `GroupByMode`,
/// flattening the list and dropping project headers. The headers are the
/// whole point of project mode; users want attention triage WITHIN their
/// project boundaries, not a flat firehose across projects.
#[test]
#[serial]
fn project_grouping_survives_attention_sort() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_count = env
        .view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Group { .. }))
        .count();
    assert_eq!(
        group_count, 2,
        "Project + Attention must keep both project headers (alpha, beta), \
         got flat_items: {:?}",
        env.view.flat_items
    );

    let group_names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        group_names.iter().any(|n| n == "alpha") && group_names.iter().any(|n| n == "beta"),
        "expected alpha and beta project headers, got {group_names:?}"
    );
}

/// Within a project group under Attention sort, sessions must order by
/// attention tier: Waiting (tier 0) above Running (tier 4). Confirms that
/// the existing `sort_sessions` helper, already reached by the project
/// flatten path via `flatten_tree`, is doing its job once we stopped
/// short-circuiting it.
#[test]
#[serial]
fn project_grouping_sorts_sessions_by_attention_within_group() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let mut current_group: Option<String> = None;
    let mut alpha_session_order: Vec<String> = Vec::new();
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => current_group = Some(name.clone()),
            Item::Session { id, .. } => {
                if current_group.as_deref() == Some("alpha") {
                    if let Some(inst) = env.view.instances.get(id) {
                        alpha_session_order.push(inst.title.clone());
                    }
                }
            }
        }
    }
    assert_eq!(
        alpha_session_order,
        vec!["alpha-waiting".to_string(), "alpha-running".to_string()],
        "Waiting session must rank above Running within the alpha group"
    );
}

/// The most-attention-urgent project floats to the top. `attention_group_key`
/// scores groups by their best member's tier; beta has an Error (tier 1)
/// while alpha's best is Waiting (tier 0), so alpha sorts first. This
/// confirms that the existing group-sort path is reached for project mode
/// under Attention sort.
#[test]
#[serial]
fn project_groups_sort_by_top_attention_member() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_order: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        group_order,
        vec!["alpha".to_string(), "beta".to_string()],
        "alpha (Waiting=tier 0) must sort above beta (Error=tier 1)"
    );
}

/// Archiving a project header while in Attention sort must remove the project
/// from the main flow once all of its live sessions are archived. The archived
/// rows still appear under the synthetic Archived section's project sub-header.
#[test]
#[serial]
fn project_attention_archive_selected_group_removes_empty_main_header() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_within_archived_section,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Attention;
    env.view.archived_section_collapsed = true;
    env.view.flat_items = env.view.build_flat_items();

    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| {
            matches!(
                i,
                Item::Group { name, path, .. }
                    if name == "beta" && !is_within_archived_section(path)
            )
        })
        .expect("beta project header present");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    assert_eq!(env.view.selected_group.as_deref(), Some("beta"));

    env.view.archive_selected_group().unwrap();

    assert!(
        env.view
            .instances
            .values()
            .filter(|i| crate::tui::home::project_group_key(i) == "beta")
            .all(|i| i.is_archived()),
        "all beta sessions must be archived"
    );
    assert!(
        !env.view.flat_items.iter().any(|item| matches!(
            item,
            Item::Group { name, path, .. }
                if name == "beta" && !is_within_archived_section(path)
        )),
        "archived-only beta must not leave a main-flow project header; got flat_items: {:?}",
        env.view.flat_items
    );
    let archived_beta = archived_project_sub_path("beta");
    assert!(
        env.view.flat_items.iter().any(|item| matches!(
            item,
            Item::Group { path, name, session_count, .. }
                if path == &archived_beta && name == "beta" && *session_count == 2
        )),
        "archived beta sessions must stay reachable under the Archived section; got flat_items: {:?}",
        env.view.flat_items
    );
}

/// A registered (pinned) project with no sessions surfaces as an empty
/// header in project view, mirroring the WebUI where an empty project is just
/// a registry entry decoupled from sessions. This is the core of #2047.
#[test]
#[serial]
fn pinned_project_without_sessions_shows_empty_header() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_two_projects_mixed_attention();
    // Register a project that has no sessions at all. The path need not exist;
    // `add` falls back to the literal path when canonicalization fails.
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("gamma", "/repos/gamma", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let group_names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        group_names.iter().any(|n| n == "gamma"),
        "pinned empty project gamma must show as a header, got {group_names:?}"
    );
    assert!(env.view.is_project_label_pinned("gamma"));
}

/// Pressing `p` on a project header pins it (registers the repo) instead of
/// opening the projects dialog; the pin toggle binding wins the shared chord
/// because a project header is selected.
#[test]
#[serial]
fn p_key_pins_project_on_header() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    let alpha_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header present");
    env.view.cursor = alpha_idx;
    env.view.update_selected();

    assert!(!env.view.is_project_label_pinned("alpha"));
    env.view.handle_key(key(KeyCode::Char('p')), None);
    assert!(
        env.view.is_project_label_pinned("alpha"),
        "p on a project header should pin it"
    );
    // The pin path must not open the projects dialog (the chord is shared).
    assert!(env.view.projects_dialog.is_none());
    // A successful pin stays quiet: the header's pin icon is feedback enough,
    // so there is no dialog to dismiss.
    assert!(
        env.view.info_dialog.is_none(),
        "a successful pin must not raise an info dialog"
    );

    // Unpinning (a second toggle) clears the pin but KEEPS the saved project,
    // so the entry stays in the registry (only an explicit remove deletes it).
    // See #2208.
    env.view.toggle_project_pin_at_cursor();
    assert!(!env.view.is_project_label_pinned("alpha"));
    // A successful unpin is likewise quiet.
    assert!(
        env.view.info_dialog.is_none(),
        "a successful unpin must not raise an info dialog"
    );
    // The specific entry is kept (not just "registry non-empty") with its pin
    // flag cleared: unpin keeps the saved project, only Remove deletes it.
    let after = crate::session::projects::load_global().unwrap();
    assert_eq!(after.len(), 1, "unpin must keep the registry entry");
    assert_eq!(after[0].name, "alpha");
    assert!(!after[0].pinned, "unpin must clear the pin flag");
}

/// Off a project header (here: in Manual grouping), `p` keeps its original
/// meaning and opens the projects dialog, so the overload doesn't shadow the
/// global binding.
#[test]
#[serial]
fn p_key_opens_projects_dialog_off_project_header() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.flat_items = env.view.build_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('p')), None);
    assert!(
        env.view.projects_dialog.is_some(),
        "p off a project header should open the projects dialog"
    );
}

/// A user's own repo whose basename is `scratch` must be pinnable, while the
/// synthetic scratch bucket (sessions with no repo, living under
/// `<app_dir>/scratch/<id>`) stays excluded. The gate used to reject the header
/// by its display LABEL, which collapsed both cases together and left `p` on a
/// real `~/scratch` repo falling through to the Projects dialog. See #3133; #3237
/// then gave the synthetic bucket its own sentinel identity, so the real repo
/// and the bucket render as two separate headers, keyed here by path.
#[test]
#[serial]
fn scratch_label_pin_gate_keys_on_backing_repo_not_label() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::canonical_key;
    use crate::session::SCRATCH_GROUP_PATH;

    // (case, has a real repo named `scratch`, has a synthetic scratch session,
    //  a pre-existing registry entry for `/repos/scratch` and its pin flag,
    //  the path of the header this case targets, the pin gate opens on it, and
    //  whether it is pinned after `p`)
    let cases = [
        // The reporter's setup: a plain repo at `~/scratch`, no scratch sessions.
        ("real repo only", true, false, None, "scratch", true, true),
        // Nothing but the synthetic bucket: no repo exists to register.
        (
            "synthetic bucket only",
            false,
            true,
            None,
            SCRATCH_GROUP_PATH,
            false,
            false,
        ),
        // The real repo and the synthetic bucket now render as two separate
        // headers (#3237). This case targets the real repo header, which backs a
        // pinnable project; the bucket's own header is covered by
        // `synthetic_scratch_bucket_is_distinct_from_real_repo`.
        (
            "real repo plus scratch session",
            true,
            true,
            None,
            "scratch",
            true,
            true,
        ),
        // A saved-but-unpinned repo named `scratch` surfaces no header of its
        // own (only pinned empties do), so the synthetic bucket is the only
        // `scratch` header and `p` must keep its global meaning.
        (
            "saved unpinned repo plus scratch session",
            false,
            true,
            Some(false),
            SCRATCH_GROUP_PATH,
            false,
            false,
        ),
        // A pinned registry entry surfaces its own empty header even with no live
        // session, so `p` must reach the unpin path on that header.
        (
            "pinned empty repo plus scratch session",
            false,
            true,
            Some(true),
            "scratch",
            true,
            false,
        ),
    ];

    for (case, has_repo, has_scratch, saved, target_path, gate_open, pinned_after) in cases {
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);
        let storage = Storage::new_unwatched("test").unwrap();

        let mut instances = Vec::new();
        if has_repo {
            instances.push(Instance::new("work", "/repos/scratch"));
        }
        if has_scratch {
            let mut throwaway = Instance::new("throwaway", "/app-dir/scratch/abc123");
            throwaway.scratch = true;
            instances.push(throwaway);
        }
        storage
            .update(|i, g| {
                *i = instances.to_vec();
                *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        if let Some(pinned) = saved {
            crate::session::projects::add(
                "test",
                crate::session::ProjectScope::Global,
                crate::session::Project::new(
                    "scratch",
                    "/repos/scratch",
                    crate::session::ProjectScope::Global,
                )
                .with_pinned(pinned),
                false,
            )
            .unwrap();
        }

        let mut view = HomeView::new(
            Some("test".to_string()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        view.group_by = GroupByMode::Project;
        view.flat_items = view.build_flat_items();

        let idx = view
            .flat_items
            .iter()
            .position(|i| matches!(i, Item::Group { path, .. } if path == target_path))
            .unwrap_or_else(|| panic!("{case}: header at path {target_path} must be present"));
        view.cursor = idx;
        view.update_selected();

        assert_eq!(
            view.project_group_at_cursor().is_some(),
            gate_open,
            "{case}: pin gate"
        );

        view.handle_key(key(KeyCode::Char('p')), None);

        assert_eq!(
            view.is_project_label_pinned("scratch"),
            pinned_after,
            "{case}: pin state after pressing p"
        );
        // The chord is shared: when the gate is closed, `p` keeps its global
        // meaning and opens the Projects dialog instead.
        assert_eq!(
            view.projects_dialog.is_some(),
            !gate_open,
            "{case}: projects dialog fallthrough"
        );

        // A registry entry exists iff one was seeded or the toggle created one;
        // the synthetic bucket on its own must never register anything.
        let registered = crate::session::projects::load_global().unwrap();
        assert_eq!(
            registered.len(),
            usize::from(gate_open || saved.is_some()),
            "{case}: registry entries, got {registered:?}"
        );
        if let Some(entry) = registered.first() {
            assert_eq!(
                canonical_key(&entry.path),
                canonical_key("/repos/scratch"),
                "{case}: the pin must target the real repo, not the app scratch dir"
            );
            assert_eq!(
                entry.pinned, pinned_after,
                "{case}: registry pin flag must track the header, got {registered:?}"
            );
        }
    }
}

/// #3237: a real repo named `scratch` and the synthetic scratch bucket must
/// render as two separate project headers with distinct identity paths, one
/// session each (not a pooled count), and independent pin/scope state. Mirrors
/// the org same-owner-two-hosts separation test above.
#[test]
#[serial]
fn synthetic_scratch_bucket_is_distinct_from_real_repo() {
    use crate::session::config::GroupByMode;
    use crate::session::{SCRATCH_GROUP_NAME, SCRATCH_GROUP_PATH};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let real = Instance::new("work", "/repos/scratch");
    let mut throwaway = Instance::new("throwaway", "/app-dir/scratch/abc123");
    throwaway.scratch = true;
    let scratch_id = throwaway.id.clone();
    let instances = vec![real, throwaway];
    storage
        .update(|i, g| {
            *i = instances.clone();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    // Two headers on distinct identity paths, one session each rather than a
    // pooled count of two. The repo header keeps its basename; the bucket
    // renders the capitalized system label.
    let scratch_headers: Vec<(&str, &str, usize)> = view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group {
                path,
                name,
                session_count,
                ..
            } if name.eq_ignore_ascii_case("scratch") => {
                Some((path.as_str(), name.as_str(), *session_count))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        scratch_headers.len(),
        2,
        "real repo and synthetic bucket must be two headers, got {scratch_headers:?}"
    );
    assert!(scratch_headers.contains(&("scratch", "scratch", 1)));
    assert!(scratch_headers.contains(&(SCRATCH_GROUP_PATH, SCRATCH_GROUP_NAME, 1)));

    // The synthetic bucket is not a pinnable project; the real repo is.
    let real_idx = view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { path, .. } if path == "scratch"))
        .unwrap();
    view.cursor = real_idx;
    assert_eq!(view.project_group_at_cursor().as_deref(), Some("scratch"));
    let bucket_idx = view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { path, .. } if path == SCRATCH_GROUP_PATH))
        .unwrap();
    view.cursor = bucket_idx;
    assert_eq!(view.project_group_at_cursor(), None);

    // Bulk-archive scope on the synthetic bucket touches only the scratch
    // session, never the real repo's session.
    view.selected_group = Some(SCRATCH_GROUP_PATH.to_string());
    assert_eq!(view.active_sessions_in_selected_group(), vec![scratch_id]);
}

/// #3237: New Session from the synthetic scratch bucket must not prefill
/// another scratch session's throwaway `<app_dir>/scratch/<id>` directory as
/// the working cwd; the dialog should fall through to the default cwd instead
/// of tying the new session's lifetime to an unrelated scratch dir. Mirrors
/// the same invariant `project_header_repo_path` enforces for the pin action.
#[test]
#[serial]
fn scratch_bucket_lends_no_repo_path_for_new_session_prefill() {
    use crate::session::config::GroupByMode;
    use crate::session::SCRATCH_GROUP_PATH;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut throwaway = Instance::new("throwaway", "/app-dir/scratch/abc123");
    throwaway.scratch = true;
    let instances = vec![throwaway];
    storage
        .update(|i, g| {
            *i = instances.clone();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(view.group_by, GroupByMode::Project);
    assert_eq!(view.group_repo_path(SCRATCH_GROUP_PATH), None);
}

/// #3237 pitfall guard: when the only scratch session is archived, the seed
/// must not create a phantom empty `scratch` header in the main flow (an
/// archived-only project header is undeletable in project mode). The bucket
/// appears only nested under the Archived section.
#[test]
#[serial]
fn scratch_bucket_absent_from_main_flow_when_only_scratch_is_archived() {
    use crate::session::config::GroupByMode;
    use crate::session::{is_within_archived_section, SCRATCH_GROUP_NAME, SCRATCH_GROUP_PATH};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut throwaway = Instance::new("throwaway", "/app-dir/scratch/abc123");
    throwaway.scratch = true;
    throwaway.archive();
    let instances = vec![throwaway];
    storage
        .update(|i, g| {
            *i = instances.clone();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = GroupByMode::Project;
    // Expand the Archived shelf so its per-project sub-headers are emitted.
    view.archived_section_collapsed = false;
    view.flat_items = view.build_flat_items();

    assert!(
        !view
            .flat_items
            .iter()
            .any(|i| matches!(i, Item::Group { path, .. } if path == SCRATCH_GROUP_PATH)),
        "an archived-only scratch session must not seed a phantom main-flow bucket"
    );
    assert!(
        view.flat_items.iter().any(|i| matches!(
            i,
            Item::Group { path, name, .. }
                if is_within_archived_section(path) && name == SCRATCH_GROUP_NAME
        )),
        "the archived scratch session should still render under the Archived section"
    );
}

/// Pin a project, archive its only session, then unpin: the empty header must
/// leave the main flow (the archived session stays under the Archived section).
#[test]
#[serial]
fn unpin_archived_only_project_leaves_main_flow() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // Pin beta.
    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "beta"))
        .expect("beta header present");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(env.view.is_project_label_pinned("beta"));

    // Archive both beta sessions.
    let beta_ids: Vec<String> = env
        .view
        .instances
        .values()
        .filter(|i| crate::tui::home::project_group_key(i) == "beta")
        .map(|i| i.id.clone())
        .collect();
    for id in &beta_ids {
        env.view
            .apply_user_action(id, |inst| inst.archive())
            .unwrap();
    }
    env.view.flat_items = env.view.build_flat_items();

    // Now unpin via the cursor on the empty main-flow beta header.
    let beta_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, path, .. } if name == "beta" && !is_within_archived_section(path)))
        .expect("empty beta header present in main flow after archiving");
    env.view.cursor = beta_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();

    assert!(
        !env.view.is_project_label_pinned("beta"),
        "beta must read as unpinned after the toggle; registry still has it"
    );

    // Count beta headers OUTSIDE the Archived section.
    let mut in_archived = false;
    let mut main_beta = 0;
    for item in &env.view.flat_items {
        if let Item::Group { path, name, .. } = item {
            if is_within_archived_section(path) {
                in_archived = true;
            } else if name == "beta" && !in_archived {
                main_beta += 1;
            }
        }
    }
    assert_eq!(
        main_beta, 0,
        "unpinned archived-only beta must not render in the main flow; got: {:?}",
        env.view.flat_items
    );
}

/// A registry entry whose path differs from an archived session's repo path
/// sharing the same basename must still read as pinned and be unpinnable.
/// The empty header is surfaced by LABEL match (`unpopulated_projects`), so
/// pin state and the unpin toggle must resolve by the same rule. Previously
/// `project_header_repo_path` let the archived row lend the header its path:
/// the path comparison failed (repo gone from disk, `canonical_key` compares
/// raw strings), the header read as unpinned, and `p` routed to the pin
/// branch and died on the name conflict, leaving a phantom header the user
/// could not clear.
#[test]
#[serial]
fn stale_registry_entry_with_mismatched_archived_path_stays_pinned_and_unpinnable() {
    use crate::session::config::GroupByMode;
    use crate::session::is_within_archived_section;
    use crate::session::projects::{self, Project, ProjectScope};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    // A live session in another project, plus an ARCHIVED session whose repo
    // basename is "otari" but whose recorded path differs from the registry
    // entry below (repo deleted/moved, so neither canonicalizes).
    let mut alpha = Instance::new("alpha-running", "/repos/alpha");
    alpha.status = Status::Running;
    let mut orphan = Instance::new("otari-old", "/old/home/otari");
    orphan.status = Status::Stopped;
    orphan.archive();

    let instances = vec![alpha, orphan];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // Stale registry entry: same basename, different (nonexistent) path.
    // Pinned, so it surfaces as an empty header (#2208).
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("otari", "/repos/otari", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    view.group_by = GroupByMode::Project;
    view.refresh_registered_projects();
    view.flat_items = view.build_flat_items();

    // 1. The phantom: an empty otari header renders in the main flow.
    let otari_idx = view.flat_items.iter().position(|i| {
        matches!(i, Item::Group { name, path, .. }
            if name == "otari" && !is_within_archived_section(path))
    });
    assert!(
        otari_idx.is_some(),
        "stale registry entry must surface an empty otari header; got {:?}",
        view.flat_items
    );

    // 2. With no live session populating the label, pin state resolves by
    //    label, the same rule that injected the header.
    assert!(
        view.is_project_label_pinned("otari"),
        "registry-backed empty header must read as pinned even when an \
         archived session recorded a different path for the same basename"
    );

    // 3. `p` on the header routes to the unpin branch and clears it.
    view.cursor = otari_idx.unwrap();
    view.update_selected();
    view.toggle_project_pin_at_cursor();

    let still_there = view.flat_items.iter().any(|i| {
        matches!(i, Item::Group { name, path, .. }
            if name == "otari" && !is_within_archived_section(path))
    });
    assert!(
        !still_there,
        "unpin must drop the empty header from the main flow; got {:?}",
        view.flat_items
    );
    // Unpin clears the flag but KEEPS the saved project (#2208): the entry
    // survives, now unpinned, so it stays in the Projects view / wizard.
    let after = projects::load_global().unwrap();
    assert_eq!(after.len(), 1, "unpin must keep the registry entry");
    assert!(!after[0].pinned, "unpin must clear the pin flag");
    // The archived session itself is untouched; it stays under Archived.
    assert!(
        view.flat_items
            .iter()
            .any(|i| { matches!(i, Item::Group { path, .. } if is_within_archived_section(path)) }),
        "archived section still present"
    );
}

/// The pin must persist a project across its last session leaving the view:
/// once pinned, the header remains even when no sessions reference it. This
/// is the user-visible promise of #2047.
#[test]
#[serial]
fn pinned_project_survives_losing_last_session() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    let alpha_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header present");
    env.view.cursor = alpha_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(env.view.is_project_label_pinned("alpha"));

    // Drop every alpha session, then rebuild as a reload would. The registry
    // entry keeps the header alive even with zero members.
    env.view
        .instances
        .retain(|_, i| crate::tui::home::project_group_key(i) != "alpha");
    env.view.flat_items = env.view.build_flat_items();

    let alpha_header = env.view.flat_items.iter().find_map(|i| match i {
        Item::Group {
            name,
            session_count,
            ..
        } if name == "alpha" => Some(*session_count),
        _ => None,
    });
    assert_eq!(
        alpha_header,
        Some(0),
        "pinned alpha must remain as an empty (0) header after losing its sessions"
    );
}

/// Two repos that share a basename are judged independently for pinning: a
/// header whose own repo is not registered must read as unpinned even when a
/// different same-basename repo is in the registry. Guards the path-keyed pin
/// identity (CodeRabbit #2055).
#[test]
#[serial]
fn same_basename_repos_pin_independently() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    // Register `/work/api`, but the visible header's repo is `/other/api`.
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("api", "/work/api", ProjectScope::Global),
        false,
    )
    .unwrap();
    let mut sess = Instance::new("api-sess", "/other/api");
    sess.source_profile = "test".to_string();
    env.view.instances.insert(sess.id.clone(), sess);

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let api_idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "api"))
        .expect("api header present");
    // The header's repo (/other/api) is not registered, so it is NOT pinned,
    // even though a same-basename repo (/work/api) is. The old basename match
    // would have reported pinned here.
    assert!(!env.view.is_project_label_pinned("api"));

    // Pinning this header would register under the basename "api", which the
    // registry already holds for /work/api, so the registry's name-uniqueness
    // surfaces a conflict rather than silently toggling the unrelated entry.
    env.view.cursor = api_idx;
    env.view.update_selected();
    env.view.toggle_project_pin_at_cursor();
    assert!(
        !env.view.is_project_label_pinned("api"),
        "the unrelated /work/api entry must not make this header read as pinned"
    );
    // The conflicting pin did not register the header's repo.
    let paths: Vec<String> = projects::load_global()
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert_eq!(paths, vec!["/work/api".to_string()]);
}

/// "New Session" on an empty pinned project (no member sessions) must prefill
/// the registered repo path, so the pin->launch loop works: the path can only
/// come from the registry fallback in `group_repo_path`.
#[test]
#[serial]
fn empty_pinned_project_new_session_uses_registered_path() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("lonely", "/repos/lonely", ProjectScope::Global),
        false,
    )
    .unwrap();
    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    assert_eq!(
        env.view.group_repo_path("lonely"),
        Some("/repos/lonely".to_string()),
        "empty pinned project must source its new-session path from the registry"
    );
}

/// In all-profiles mode the pin registry must include every loaded profile's
/// projects, not just the default profile's, so a profile-scoped pin keeps its
/// empty header (CodeRabbit #2055).
#[test]
#[serial]
fn all_profiles_view_includes_profile_scoped_pins() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Two discoverable profiles, each with a session.
    for (profile, title, path) in [
        ("alpha", "Alpha Session", "/tmp/a"),
        ("beta", "Beta Session", "/tmp/b"),
    ] {
        let storage = Storage::new_unwatched(profile).unwrap();
        let xs = vec![Instance::new(title, path)];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }
    // A profile-scoped pin in `beta` with no sessions of its own.
    projects::add(
        "beta",
        ProjectScope::Profile,
        Project::new("lonely", "/repos/lonely", ProjectScope::Profile).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    let names: Vec<String> = view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|n| n == "lonely"),
        "beta's profile-scoped pin must show in all-profiles project view, got {names:?}"
    );
    assert!(view.is_project_label_pinned("lonely"));
}

/// Unpinning a PROFILE-scoped pin from all-profiles mode must actually clear
/// it. Regression for #2055: the empty header surfaced from a non-default
/// profile's registry, but the unpin removed against `config_profile()` (the
/// default profile) rather than the profile that owned the entry, so the
/// header never disappeared.
#[test]
#[serial]
fn unpin_profile_scoped_pin_from_all_profiles_clears_header() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Two discoverable profiles, each with a session, so all-profiles mode
    // loads both registries.
    for (profile, title, path) in [
        ("alpha", "Alpha Session", "/tmp/a"),
        ("beta", "Beta Session", "/tmp/b"),
    ] {
        let storage = Storage::new_unwatched(profile).unwrap();
        let xs = vec![Instance::new(title, path)];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }
    // A profile-scoped pin in `beta` (NOT the default profile) with no sessions.
    projects::add(
        "beta",
        ProjectScope::Profile,
        Project::new("lonely", "/repos/lonely", ProjectScope::Profile).with_pinned(true),
        false,
    )
    .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    let idx = view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "lonely"))
        .expect("lonely header present in all-profiles project view");
    view.cursor = idx;
    view.update_selected();
    view.toggle_project_pin_at_cursor();

    assert!(
        !view.is_project_label_pinned("lonely"),
        "lonely must read as unpinned after the toggle"
    );
    // The unpin must clear the flag on beta's on-disk entry (not the default
    // profile's), but KEEP the entry: it stays a saved project. See #2208.
    let beta_after = projects::load_profile("beta").unwrap();
    assert_eq!(beta_after.len(), 1, "the profile-scoped entry must be kept");
    assert!(
        !beta_after[0].pinned,
        "its pin flag must be cleared on disk"
    );
    let still: Vec<String> = view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !still.iter().any(|n| n == "lonely"),
        "unpinned lonely must drop from the view, got {still:?}"
    );
}

/// A repo pinned in BOTH scopes (a profile entry shadowing a global one via
/// `--allow-override`) must fully unpin in a single press. `load_merged` only
/// surfaces the shadowing profile entry, so clearing just that one would
/// re-surface the global pin and leave the header pinned after a "success"
/// dialog. Unpin sweeps every scope for the path, clearing the flag while
/// keeping each entry. See #2208.
#[test]
#[serial]
fn unpin_clears_both_global_and_profile_entries_for_a_path() {
    use crate::session::config::GroupByMode;
    use crate::session::projects::{self, Project, ProjectScope};

    let mut env = create_test_env_empty();
    // Same path pinned globally and profile-scoped (override allows the shadow).
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("dual-global", "/repos/dual", ProjectScope::Global).with_pinned(true),
        false,
    )
    .unwrap();
    projects::add(
        "test",
        ProjectScope::Profile,
        Project::new("dual-profile", "/repos/dual", ProjectScope::Profile).with_pinned(true),
        true,
    )
    .unwrap();

    env.view.group_by = GroupByMode::Project;
    env.view.refresh_registered_projects();
    env.view.flat_items = env.view.build_flat_items();

    let idx = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "dual"))
        .expect("dual header present");
    env.view.cursor = idx;
    env.view.update_selected();

    // One press must fully unpin.
    env.view.toggle_project_pin_at_cursor();

    assert!(
        !env.view.is_project_label_pinned("dual"),
        "dual must read as unpinned after a single press"
    );
    // Both entries are kept (saved projects), with the pin flag cleared in
    // each scope so the merged view no longer shows a pinned header. See #2208.
    let global_after = projects::load_global().unwrap();
    assert_eq!(global_after.len(), 1, "global entry must be kept");
    assert!(!global_after[0].pinned, "global pin flag must be cleared");
    let profile_after = projects::load_profile("test").unwrap();
    assert_eq!(profile_after.len(), 1, "profile entry must be kept");
    assert!(!profile_after[0].pinned, "profile pin flag must be cleared");
    let names: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !names.iter().any(|n| n == "dual"),
        "unpinned dual must drop from the view, got {names:?}"
    );
}

/// Pressing `g` to flip `group_by` keeps the cursor on the previously
/// selected session, even when the list reshapes (Manual flat list →
/// Project grouped list). Previously `apply_group_by` clamped by index,
/// which landed the cursor on whatever row slid into the old slot once
/// project headers got inserted. The fix seeks `selected_session` by id
/// after the rebuild.
#[test]
#[serial]
fn group_by_toggle_preserves_selected_session() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.sort_order = crate::session::config::SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();

    // Pick the last session in the Manual flat list; that's the row whose
    // index is most likely to be invalidated when project headers get
    // inserted in front of it.
    let target_id = env
        .view
        .flat_items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("manual flat list should contain at least one session");
    env.view.select_session_by_id(&target_id);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str())
    );

    env.view.handle_key(key(KeyCode::Char('g')), None);
    // 'g' opens the picker; pick Project to apply the flip.
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.group_by, GroupByMode::Project);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str()),
        "cursor must stay on the same session after group_by flip"
    );
    let cursor_item = env
        .view
        .flat_items
        .get(env.view.cursor)
        .expect("cursor must point into flat_items");
    match cursor_item {
        Item::Session { id, .. } => assert_eq!(id, &target_id),
        Item::Group { .. } => panic!("cursor landed on a group header, not the session"),
    }
}

/// Pressing `o` to flip `sort_order` keeps the cursor on the previously
/// selected session. Most visible when going Newest → Attention with
/// Project grouping on, since Attention reorders both groups (by top
/// member) and sessions within each group, so the target session is very
/// unlikely to keep its index across the rebuild.
#[test]
#[serial]
fn sort_order_toggle_preserves_selected_session() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();

    // Pin the Running session inside alpha. Under Attention sort it sinks
    // below alpha-waiting, so its index will shift on the rebuild.
    let target_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .expect("fixture provides alpha-running");
    env.view.select_session_by_id(&target_id);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str())
    );

    // Open the sort picker and pick Attention (one down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::Attention);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(target_id.as_str()),
        "cursor must stay on the same session after sort_order flip"
    );
}

/// `reseat_cursor_after_rebuild` falls back to index clamping when there
/// is no prior session selection. Guards against the helper accidentally
/// regressing the empty-or-group-only path, where the original clamp
/// logic was correct.
#[test]
#[serial]
fn reseat_cursor_clamps_when_no_session_selected() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();
    env.view.selected_session = None;
    env.view.cursor = env.view.flat_items.len() + 50; // intentionally out of range

    env.view.reseat_cursor_after_rebuild();
    assert!(
        env.view.cursor < env.view.flat_items.len(),
        "cursor must be clamped into the flat_items range"
    );
}

/// Manual grouping + Attention sort must still flatten. The cross-cutting
/// flat priority view is the original Attention design and is the right
/// behavior when the user has not opted into project grouping. Guards
/// against an over-eager refactor flipping both modes to grouped.
#[test]
#[serial]
fn manual_grouping_attention_sort_stays_flat() {
    use crate::session::config::{GroupByMode, SortOrder};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Manual;
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();

    let group_count = env
        .view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Group { .. }))
        .count();
    assert_eq!(
        group_count, 0,
        "Manual + Attention should produce a flat list, no group headers"
    );
}

/// A profile move commits the source-group removal with the row transfer, so
/// reloading cannot resurrect metadata from the source profile.
#[test]
#[serial]
fn profile_move_group_metadata_survives_reload() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _ = Storage::new_unwatched("alpha").unwrap();
    let _ = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);

    {
        let mut view = HomeView::new(
            None,
            tools.clone(),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let moved = {
            let mut inst = Instance::new("moved", "/tmp/moved");
            inst.id = "moved".to_string();
            inst.source_profile = "alpha".to_string();
            inst.group_path = "work".to_string();
            inst
        };
        view.instances.insert(moved.id.clone(), moved);
        view.pending_added
            .entry("alpha".to_string())
            .or_default()
            .insert("moved".to_string());
        view.group_trees.insert(
            "alpha".to_string(),
            GroupTree::new_with_groups(&view.cloned_instances(), &[]),
        );
        view.save().unwrap();

        view.group_trees
            .entry("beta".to_string())
            .or_insert_with(|| GroupTree::new_with_groups(&[], &[]));
        let requested = view.instances["moved"].clone();
        view.move_to_profile("moved", "beta", requested, None)
            .unwrap();
    }

    let reloaded = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    assert!(
        reloaded.group_trees.contains_key("alpha"),
        "alpha tree must still load after the move"
    );
    assert!(
        !reloaded.group_trees["alpha"].group_exists("work"),
        "pruned 'work' must stay gone after save+reload, not get re-seeded from disk"
    );
    assert!(
        reloaded.group_trees["beta"].group_exists("work"),
        "target group metadata must be committed with the moved row"
    );
    let (_, source_groups) = Storage::new_unwatched("alpha")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let (_, target_groups) = Storage::new_unwatched("beta")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert!(!source_groups.iter().any(|group| group.path == "work"));
    assert!(target_groups.iter().any(|group| group.path == "work"));
}

/// Favorite, snooze, and urgent decorations only render in Attention sort.
/// With `session.favorites_first` off, the star is Attention-only: in Newest
/// (or any other sort) the row paints with its plain title and status-driven
/// color even when the flag is set, so users who don't triage in Attention
/// don't see decoration for state they didn't opt into managing.
///
/// The flag-on case is `favorite_decoration_shows_outside_attention_when_favorites_first`.
#[test]
#[serial]
fn favorite_decoration_gated_to_attention_sort() {
    use crate::session::config::SortOrder;

    let original = crate::session::favorites_first();

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let title = env.view.instance_at(0).title.clone();
    env.view.mutate_instance(&id, |inst| inst.favorite());

    // After the env is built: constructing it applies config, which resets the
    // process-wide flag to the shipped default (on).
    crate::session::set_favorites_first(false);

    // In Newest: row should NOT have the `* ` prefix or the bold/
    // underlined favorite styling.
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let item = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Newest sort");
    let text_newest = rendered_row_text(&env.view, &item);
    assert!(
        !text_newest.contains("* "),
        "favorite prefix must be hidden outside Attention sort; got: {:?}",
        text_newest
    );
    assert!(
        text_newest.contains(&title),
        "row title must still render; got: {:?}",
        text_newest
    );

    // Flip to Attention: the prefix returns.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let item_attention = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Attention sort");
    let text_attention = rendered_row_text(&env.view, &item_attention);
    assert!(
        text_attention.contains("* "),
        "favorite prefix must surface in Attention sort; got: {:?}",
        text_attention
    );

    crate::session::set_favorites_first(original);
}

/// With favorites-first on (the default), the star follows the pin: a
/// favorited row shows it in Newest too, because it is pinned there.
/// A snoozed favorite is not pinned, so it must not be decorated either.
#[test]
#[serial]
fn favorite_decoration_shows_outside_attention_when_favorites_first() {
    use crate::session::config::SortOrder;

    let original = crate::session::favorites_first();

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let title = env.view.instance_at(0).title.clone();
    env.view.mutate_instance(&id, |inst| inst.favorite());

    // Set after the env is built: constructing it applies config, which would
    // overwrite the flag.
    crate::session::set_favorites_first(true);

    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let row = |view: &HomeView, id: &str| {
        let item = view
            .flat_items
            .iter()
            .find(|i| matches!(i, Item::Session { id: sid, .. } if sid == id))
            .cloned()
            .expect("session item present");
        rendered_row_text(view, &item)
    };

    let text = row(&env.view, &id);
    assert!(
        text.contains("* "),
        "favorite prefix must show in Newest when favorites-first is on; got: {:?}",
        text
    );
    assert!(
        text.contains(&title),
        "row title must still render; got: {:?}",
        text
    );

    // Snooze outranks the star: the row is no longer pinned, so it must not
    // be decorated as a favorite either.
    env.view.mutate_instance(&id, |inst| inst.snooze(30));
    env.view.flat_items = env.view.build_flat_items();
    let text_snoozed = row(&env.view, &id);
    assert!(
        !text_snoozed.contains("* "),
        "a snoozed favorite is not pinned, so it must not show the star; got: {:?}",
        text_snoozed
    );

    crate::session::set_favorites_first(original);
}

/// Snoozed rows: prefix and remaining-time column only appear in Attention
/// sort. Outside Attention, the snooze flag persists silently and the row
/// paints with its underlying status.
#[test]
#[serial]
fn snooze_decoration_gated_to_attention_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| inst.snooze(30));

    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let item_newest = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Newest sort");
    let text_newest = rendered_row_text(&env.view, &item_newest);
    assert!(
        !text_newest.contains("z "),
        "snooze prefix must be hidden outside Attention sort; got: {:?}",
        text_newest
    );

    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let item_attention = env
        .view
        .flat_items
        .iter()
        .find(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id))
        .cloned()
        .expect("session item present in Attention sort");
    let text_attention = rendered_row_text(&env.view, &item_attention);
    assert!(
        text_attention.contains("z "),
        "snooze prefix must surface in Attention sort; got: {:?}",
        text_attention
    );
}

/// Archived sessions live under the synthetic "Archived" section pinned to
/// the bottom of the sidebar in every sort mode, not inline at their
/// natural position. The section header carries the count; when collapsed
/// the archived rows themselves are hidden but the header still appears.
#[test]
#[serial]
fn archived_section_pinned_to_bottom_in_every_sort() {
    use crate::session::{config::SortOrder, is_archived_section_path, ARCHIVED_SECTION_NAME};

    let mut env = create_test_env_with_sessions(3);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| inst.archive());
    env.view.archived_section_collapsed = true;

    for sort in [SortOrder::Newest, SortOrder::Attention, SortOrder::AZ] {
        env.view.sort_order = sort;
        env.view.flat_items = env.view.build_flat_items();

        // Archived row must NOT appear inline among the active sessions.
        let archived_inline = env
            .view
            .flat_items
            .iter()
            .take_while(|i| {
                !matches!(
                    i,
                    Item::Group { path, .. } if is_archived_section_path(path)
                )
            })
            .any(|i| matches!(i, Item::Session { id: sid, .. } if *sid == id));
        assert!(
            !archived_inline,
            "[{:?}] archived row must not appear before the Archived section",
            sort
        );

        // The synthetic section must sit at the bottom of the list.
        let last = env
            .view
            .flat_items
            .last()
            .expect("flat_items should be non-empty");
        match last {
            Item::Group {
                path,
                name,
                session_count,
                collapsed,
                ..
            } => {
                assert!(
                    is_archived_section_path(path),
                    "[{:?}] last item must be the Archived section header; got path {:?}",
                    sort,
                    path
                );
                assert_eq!(name, ARCHIVED_SECTION_NAME, "[{:?}] section name", sort);
                assert_eq!(*session_count, 1, "[{:?}] one archived row", sort);
                assert!(*collapsed, "[{:?}] section must default collapsed", sort);
            }
            other => panic!(
                "[{:?}] expected Archived section header, got {:?}",
                sort, other
            ),
        }
    }
}

/// In Project grouping mode, archived sessions must nest under per-project
/// sub-headers inside the Archived section instead of forming one flat list.
/// Layout: Archived (depth 0) > <project> (depth 1) > sessions (depth 2).
/// Sessions inside a sub-folder still sort most-recently-archived first.
#[test]
#[serial]
fn archived_section_nests_by_project_in_project_mode() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_archived_section_path, ARCHIVED_SECTION_NAME,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    // Pin to AZ so this test asserts only the depth-0/1/2 layout shape,
    // not the sort-order behavior. Sort_order coverage lives in
    // `archived_sub_folders_honor_sort_order` below.
    env.view.sort_order = SortOrder::AZ;
    // Archive one session from each project so we expect two sub-folders.
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;
    env.view.flat_items = env.view.build_flat_items();

    // Find the Archived section header and walk forward.
    let arch_idx = env
        .view
        .flat_items
        .iter()
        .position(|it| matches!(it, Item::Group { path, .. } if is_archived_section_path(path)))
        .expect("Archived section header must be present");

    // Header sanity: depth 0, count = 2, name = Archived.
    match &env.view.flat_items[arch_idx] {
        Item::Group {
            depth,
            session_count,
            name,
            ..
        } => {
            assert_eq!(*depth, 0, "Archived header depth");
            assert_eq!(*session_count, 2, "two archived sessions across projects");
            assert_eq!(name, ARCHIVED_SECTION_NAME);
        }
        _ => unreachable!(),
    }

    // The next two non-session items should be sub-folder headers at depth 1,
    // one for "alpha" and one for "beta", in alphabetical order. Between them
    // and after the second, the sessions at depth 2 belong to that sub-folder.
    let tail = &env.view.flat_items[arch_idx + 1..];

    let sub_alpha_path = archived_project_sub_path("alpha");
    let sub_beta_path = archived_project_sub_path("beta");

    // First sub-header must be alpha (AZ sort orders by name).
    match &tail[0] {
        Item::Group {
            path,
            name,
            depth,
            session_count,
            ..
        } => {
            assert_eq!(path, &sub_alpha_path);
            assert_eq!(name, "alpha");
            assert_eq!(*depth, 1);
            assert_eq!(*session_count, 1);
        }
        other => panic!("expected alpha sub-header at depth 1, got {:?}", other),
    }
    // Then alpha's archived session at depth 2.
    match &tail[1] {
        Item::Session { id, depth } => {
            assert_eq!(
                id, &alpha_id,
                "alpha sub-folder should contain alpha-running"
            );
            assert_eq!(*depth, 2);
        }
        other => panic!("expected alpha-running session row, got {:?}", other),
    }
    // Then the beta sub-header at depth 1.
    match &tail[2] {
        Item::Group {
            path,
            name,
            depth,
            session_count,
            ..
        } => {
            assert_eq!(path, &sub_beta_path);
            assert_eq!(name, "beta");
            assert_eq!(*depth, 1);
            assert_eq!(*session_count, 1);
        }
        other => panic!("expected beta sub-header at depth 1, got {:?}", other),
    }
    // Then beta's archived session at depth 2.
    match &tail[3] {
        Item::Session { id, depth } => {
            assert_eq!(id, &beta_id, "beta sub-folder should contain beta-error");
            assert_eq!(*depth, 2);
        }
        other => panic!("expected beta-error session row, got {:?}", other),
    }
}

/// A project whose only remaining member is archived must NOT leave an empty
/// phantom header in the main (non-archived) flow. The archived session shows
/// under the Archived section instead; an empty project header would be
/// undeletable in project mode ("Project groups are automatic").
#[test]
#[serial]
fn archived_only_project_leaves_no_phantom_header() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;

    // Drain beta down to a single ARCHIVED member: archive beta-error, then
    // delete beta-running (the "last visible session in the group").
    let beta_error = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_running = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-running")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&beta_error, |inst| inst.archive())
        .unwrap();
    env.view.instances.shift_remove(&beta_running);
    env.view.flat_items = env.view.build_flat_items();

    // Count "beta" headers that live OUTSIDE the Archived section.
    let mut in_archived = false;
    let mut main_beta_headers = 0;
    for item in &env.view.flat_items {
        if let Item::Group { path, name, .. } = item {
            if is_within_archived_section(path) {
                in_archived = true;
            } else if name == "beta" && !in_archived {
                main_beta_headers += 1;
            }
        }
    }
    assert_eq!(
        main_beta_headers, 0,
        "archived-only project must not render a header in the main flow; got flat_items: {:?}",
        env.view.flat_items
    );
}

/// Collapsing the Archived umbrella in Project mode hides both sub-folder
/// headers and their session rows.
#[test]
#[serial]
fn archived_section_collapsed_hides_project_sub_folders() {
    use crate::session::{config::GroupByMode, is_within_archived_section};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = true;
    env.view.flat_items = env.view.build_flat_items();

    let within_archive_items: Vec<&Item> = env
        .view
        .flat_items
        .iter()
        .filter(|it| match it {
            Item::Group { path, .. } => is_within_archived_section(path),
            Item::Session { .. } => false,
        })
        .collect();
    assert_eq!(
        within_archive_items.len(),
        1,
        "collapsed Archived must render only its top-level header, got {:?}",
        within_archive_items
    );
}

/// Collapsing a single project sub-folder under Archived hides its session
/// rows but leaves the sub-header (and any other sub-folders) intact. Uses
/// the same `project_group_collapsed` map that drives regular project mode
/// collapse, keyed by the synthetic `archived_project_sub_path`.
#[test]
#[serial]
fn archived_project_sub_folder_collapse_hides_only_its_sessions() {
    use crate::session::{archived_project_sub_path, config::GroupByMode};

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;
    // Collapse only alpha's archived sub-folder.
    env.view
        .project_group_collapsed
        .insert(archived_project_sub_path("alpha"), true);
    env.view.flat_items = env.view.build_flat_items();

    // alpha sub-folder must still appear as a header but with no session row
    // following it; beta sub-folder must still emit its session row.
    let has_alpha_session = env
        .view
        .flat_items
        .iter()
        .any(|it| matches!(it, Item::Session { id, .. } if id == &alpha_id));
    let has_beta_session = env
        .view
        .flat_items
        .iter()
        .any(|it| matches!(it, Item::Session { id, .. } if id == &beta_id));
    assert!(
        !has_alpha_session,
        "collapsed alpha sub-folder must hide its archived session"
    );
    assert!(
        has_beta_session,
        "expanded beta sub-folder must still surface its archived session"
    );
    let alpha_sub_path = archived_project_sub_path("alpha");
    assert!(
        env.view.flat_items.iter().any(
            |it| matches!(it, Item::Group { path, collapsed, .. } if path == &alpha_sub_path && *collapsed)
        ),
        "alpha sub-folder header must remain visible with collapsed=true"
    );
}

/// Archived project sub-folders honor `sort_order`, mirroring how active
/// project headers order in `flatten_tree`. AZ/ZA sort by project name;
/// recency sorts (Newest, LastActivity, Attention) bring the most-
/// recently-archived project to the top; Oldest does the inverse. Probes
/// AZ, ZA, and Newest as representatives; the Oldest/LastActivity/Attention
/// branches share the same `sort_archived_project_buckets` machinery.
#[test]
#[serial]
fn archived_sub_folders_honor_sort_order() {
    use crate::session::{
        archived_project_sub_path,
        config::{GroupByMode, SortOrder},
        is_archived_section_path,
    };

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let alpha_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "alpha-running")
        .map(|i| i.id.clone())
        .unwrap();
    let beta_id = env
        .view
        .instances
        .values()
        .find(|i| i.title == "beta-error")
        .map(|i| i.id.clone())
        .unwrap();
    // Archive alpha first, then beta. archived_at is `Utc::now()` at the
    // moment of `archive()`, so beta is strictly more recent than alpha.
    env.view
        .apply_user_action(&alpha_id, |inst| inst.archive())
        .unwrap();
    env.view
        .apply_user_action(&beta_id, |inst| inst.archive())
        .unwrap();
    env.view.archived_section_collapsed = false;

    let first_sub_folder = |env: &TestEnv| -> Option<String> {
        let arch_idx = env.view.flat_items.iter().position(
            |it| matches!(it, Item::Group { path, .. } if is_archived_section_path(path)),
        )?;
        env.view
            .flat_items
            .get(arch_idx + 1)
            .and_then(|it| match it {
                Item::Group { path, .. } => Some(path.clone()),
                _ => None,
            })
    };

    let alpha_sub = archived_project_sub_path("alpha");
    let beta_sub = archived_project_sub_path("beta");

    env.view.sort_order = SortOrder::AZ;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(alpha_sub.as_str()),
        "AZ: alphabetical, alpha first"
    );

    env.view.sort_order = SortOrder::ZA;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(beta_sub.as_str()),
        "ZA: reverse alphabetical, beta first"
    );

    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    assert_eq!(
        first_sub_folder(&env).as_deref(),
        Some(beta_sub.as_str()),
        "Newest: most-recently-archived project first (beta archived after alpha)"
    );
}

#[test]
#[serial]
fn every_view_mode_paints_the_same_sunk_row_decoration() {
    // `render_item_line`'s three view arms each carried their own copy of the
    // archive / snooze / favorite block (Structured and Terminal had
    // byte-identical title blocks), and the Tool arm had none at all: an
    // archived or snoozed session in Tool view kept painting its live glyph
    // with no `z ` prefix. `decorate_row` owns the overlay for every mode now,
    // so the three must agree.
    //
    // The pane views are seeded live on purpose. `ICON_IDLE` and `ICON_STOPPED`
    // are the same glyph and an unseeded pane row is already dimmed, so a row
    // whose terminal is NOT running renders identically with and without the
    // sink override, and every assertion below would pass on a renderer that
    // dropped `decorate_row` entirely. Injecting the pane names into the shared
    // tmux snapshot makes the seed a bright animated spinner, which is what
    // gives the override something to actually override.
    use crate::session::Status;
    use crate::tui::home::{ViewMode, ICON_STOPPED};
    use ratatui::style::Modifier;

    let mut env = create_test_env_with_sessions(1);
    let id = match env.view.flat_items.first() {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected the fixture to seed a single Session item"),
    };
    let title = env
        .view
        .get_instance(&id)
        .expect("session present")
        .title
        .clone();
    // Snooze decoration is Attention-gated; archive is universal.
    env.view.sort_order = crate::session::config::SortOrder::Attention;
    let theme = crate::tui::styles::Theme::default();
    let item = Item::Session {
        id: id.clone(),
        depth: 0,
    };

    let seed_panes_live = || {
        crate::tmux::test_inject_session_into_cache(&crate::tmux::TerminalSession::generate_name(
            &id, &title,
        ));
        crate::tmux::test_inject_session_into_cache(&crate::tmux::ToolSession::generate_name(
            &id, &title, "lazygit",
        ));
    };

    // (label, archived, snoozed, expected title prefix, extra modifiers)
    let cases = [
        ("archived", true, false, "", Modifier::empty()),
        (
            "snoozed",
            false,
            true,
            "z ",
            Modifier::ITALIC | Modifier::DIM,
        ),
    ];

    for mode in [
        ViewMode::Structured,
        ViewMode::Terminal,
        ViewMode::Tool("lazygit".to_string()),
    ] {
        env.view.view_mode = mode.clone();

        // Anti-vacuity: a live, unsunk row must NOT already look sunk, or the
        // sink assertions below prove nothing about this mode.
        seed_panes_live();
        env.view.mutate_instance(&id, |inst| {
            inst.status = Status::Running;
            inst.archived_at = None;
            inst.snoozed_until = None;
        });
        let live = env.view.render_item_line(&item, false, false, &theme, 120);
        assert_ne!(
            live.spans[1].style.fg,
            Some(theme.dimmed),
            "{mode:?}: a live row must not already paint dimmed; \
             the sink assertions below would be vacuous"
        );

        for (label, archived, snoozed, prefix, extra) in cases {
            seed_panes_live();
            // Status stays Running so the live-glyph branch would fire in
            // every mode if the sink override were missing.
            env.view.mutate_instance(&id, |inst| {
                inst.status = Status::Running;
                inst.archived_at = archived.then(chrono::Utc::now);
                inst.snoozed_until =
                    snoozed.then(|| chrono::Utc::now() + chrono::Duration::minutes(15));
            });

            let line = env.view.render_item_line(&item, false, false, &theme, 120);
            let icon = line.spans[1].content.trim().to_string();
            let rendered = line.spans[2].content.to_string();

            assert_eq!(
                icon, ICON_STOPPED,
                "{mode:?}/{label}: sunk row must drop its live glyph"
            );
            assert_eq!(
                line.spans[1].style.fg,
                Some(theme.dimmed),
                "{mode:?}/{label}: sunk row must paint dimmed"
            );
            assert!(
                line.spans[1].style.add_modifier.contains(extra),
                "{mode:?}/{label}: expected {extra:?}, got {:?}",
                line.spans[1].style.add_modifier
            );
            assert_eq!(
                rendered,
                format!("{prefix}{title}"),
                "{mode:?}/{label}: wrong title decoration"
            );
        }

        // Error and Deleting punch through the sink mask in Structured only.
        // There the seed carries ICON_ERROR + theme.error, so a failed Empty
        // Trash stays distinguishable from a healthy trash row. The pane views
        // seed from terminal liveness and have no error affordance, so
        // punching through would paint a bright animated "still alive" row
        // inside the Archived shelf while signalling nothing about the failure.
        for status in [Status::Error, Status::Deleting] {
            seed_panes_live();
            env.view.mutate_instance(&id, |inst| {
                inst.status = status;
                inst.archived_at = Some(chrono::Utc::now());
                inst.snoozed_until = None;
            });
            let line = env.view.render_item_line(&item, false, false, &theme, 120);
            let sunk = line.spans[1].style.fg == Some(theme.dimmed);
            assert_eq!(
                sunk,
                !matches!(mode, ViewMode::Structured),
                "{mode:?}/archived+{status:?}: only Structured may punch through the sink mask"
            );
        }
    }
}

/// Regression: the TUI paint path must never fork tmux, no matter how cold
/// the shared snapshots are. Every tmux question a frame needs is answered
/// from `SESSION_CACHE` / `PANE_META_CACHE` by a background poller and from
/// `LiveCaptureWorker` frames; this walks Structured / Terminal / Tool view
/// modes across an empty cache, a fresh-but-absent snapshot, and an expired
/// one (the states that used to trigger synchronous refreshes from render)
/// and counts forks on the paint thread via the probe at
/// `tmux_command()`. Worker threads are NOT armed, so their legitimate
/// captures don't count.
#[test]
#[serial]
fn paint_never_forks_tmux_even_with_empty_absent_or_expired_caches() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    let theme = load_theme("empire");

    let session_guard = crate::tmux::SessionCacheGuard::capture();
    let pane_guard = crate::tmux::PaneMetaCacheGuard::capture();

    let modes = [
        ViewMode::Structured,
        ViewMode::Terminal,
        ViewMode::Tool("lazygit".to_string()),
    ];
    for mode in modes {
        env.view.view_mode = mode.clone();
        // Cache states: (session snapshot, pane snapshot). "Cold boot" is the
        // never-refreshed state; "absent" is fresh but without our session;
        // "expired" is populated but past CACHE_TTL. All three used to make
        // paint refresh synchronously.
        let states = [("cold-boot", 0), ("fresh-absent", 1), ("expired", 2)];
        for (label, state) in states {
            match state {
                0 => {
                    session_guard.force_unreachable();
                    session_guard.force_stale();
                    pane_guard.force_failed_refresh();
                    pane_guard.force_stale();
                }
                1 => {
                    session_guard.force_present(&["aoe_someone_elses_session"]);
                    pane_guard.force_failed_refresh();
                }
                _ => {
                    session_guard.force_present(&["aoe_someone_elses_session"]);
                    session_guard.force_stale();
                    pane_guard.force_failed_refresh();
                    pane_guard.force_stale();
                }
            }

            crate::tmux::fork_probe::take();
            let _armed = crate::tmux::fork_probe::arm();
            // Two frames: the first exercises cold paths (worker empty,
            // debounces arming), the second steady state.
            for _frame in 0..2 {
                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
                    .unwrap();
            }
            let forks = crate::tmux::fork_probe::take();
            assert_eq!(
                forks, 0,
                "{mode:?} with {label} caches: paint forked tmux {forks}x; \
                 display answers must come from poller-refreshed snapshots"
            );
        }
    }
}

/// Regression: a frozen (scrolled-back) preview must still be able to GROW
/// its capture. Only worker frames write the cache now, so the reading-depth
/// budget has to reach the worker while frozen and an adequate frame has to
/// be applied; otherwise scrollback reads hit a hard wall at the live-edge
/// window (~CAPTURE_BUFFER rows past the viewport). An inadequate frame is
/// skipped rather than applied: it would clamp the held offset against too
/// few lines and snap the view toward the live edge.
#[test]
#[serial]
fn frozen_preview_grows_only_on_coverage_extending_frames() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view
        .sync_preview_capture_worker(Some("aoe_test_frozen_read".to_string()));
    env.view.preview_scroll_offset = 40; // non-zero offset freezes the preview

    // A frame that does not cover viewport + offset is skipped while frozen.
    if let Some(worker) = env.view.preview_capture_worker.as_ref() {
        worker.inject_frame_for_test(100, &"line\n".repeat(30));
    }
    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(
        env.view.preview_cache.content.lines().count(),
        0,
        "an inadequate frame must not shift the held content under the reader"
    );
    // The skipped frame must be back in the mailbox: the worker's content
    // dedup would otherwise never republish it.
    assert!(
        env.view
            .preview_capture_worker
            .as_ref()
            .map(|w| w.take_latest().is_some())
            .unwrap_or(false),
        "a frame rejected while frozen must be restored for later consumption"
    );

    // A coverage-extending frame grows the cache even though frozen.
    if let Some(worker) = env.view.preview_capture_worker.as_ref() {
        worker.inject_frame_for_test(200, &"line\n".repeat(120));
    }
    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(
        env.view.preview_cache.content.lines().count(),
        120,
        "the frozen read must be able to grow its capture off-thread"
    );
}

/// Regression: a frame captured before the last retarget must never land
/// under the new view. The consumer-side `frame_is_current` guard is the
/// last line of defense against the race where the worker publishes an
/// old-generation frame after `set_target` cleared the mailbox; without
/// this test, deleting the guard reintroduces the previous pane's bytes
/// under the new header.
#[test]
#[serial]
fn preview_rejects_frames_from_previous_generation() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view
        .sync_preview_capture_worker(Some("aoe_test_new_target".to_string()));
    if let Some(worker) = env.view.preview_capture_worker.as_ref() {
        // Idle the real capture thread before injection, so it cannot
        // overwrite the synthetic stale frame and make the test pass by
        // accident even if the consumer guard is deleted.
        worker.set_target(String::new());
        worker.inject_stale_generation_frame_for_test(40, "previous pane bytes");
    }

    env.view.refresh_preview_cache_if_needed(80, 24);

    assert_ne!(
        env.view.preview_cache.content, "previous pane bytes",
        "a stale-generation frame must be dropped, never applied"
    );
}

/// Regression: entering live-send while a blocking capture is in flight must
/// revalidate the empty-frame policy on the consumer. An empty frame captured
/// just before the transition cannot blank the agent/tool pane under the
/// user's cursor (#1501); the same restored frame must clear stale content
/// after live-send exits.
#[test]
#[serial]
fn preview_revalidates_empty_policy_across_live_transition() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    let target = "aoe_test_live_transition".to_string();
    env.view.sync_preview_capture_worker(Some(target.clone()));
    env.view.preview_cache.content = "last good frame".to_string();
    env.view.preview_cache.captured_lines = 1;
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.session_id = Some(id);
    env.view.preview_cache.capture_target = Some(target);
    env.view.preview_cache.capture_generation = env
        .view
        .preview_capture_worker
        .as_ref()
        .expect("worker spawned")
        .current_generation_for_test();
    let worker = env
        .view
        .preview_capture_worker
        .as_ref()
        .expect("worker spawned");
    worker.inject_frame_for_test(40, "");
    worker.set_live(true);

    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(
        env.view.preview_cache.content, "last good frame",
        "live transition must preserve the last-good agent frame"
    );

    env.view
        .preview_capture_worker
        .as_ref()
        .expect("worker retained")
        .set_live(false);
    env.view.refresh_preview_cache_if_needed(80, 24);
    assert_eq!(
        env.view.preview_cache.content, "",
        "the restored empty frame must clear stale content after live exit"
    );
}

/// A passive completion that lands after live ownership must invalidate the
/// matching agent resize dedup, but never another session or target.
#[test]
fn passive_completion_invalidates_only_matching_agent_live_geometry() {
    use crate::tui::home::live_send::LiveSendTarget;
    use crate::tui::home::render::passive_resize_invalidates_live_geometry;

    let cases = [
        (
            Some(&LiveSendTarget::Agent),
            Some("selected"),
            "selected",
            true,
        ),
        (
            Some(&LiveSendTarget::Agent),
            Some("selected"),
            "other",
            false,
        ),
        (
            Some(&LiveSendTarget::Terminal),
            Some("selected"),
            "selected",
            false,
        ),
        (None, Some("selected"), "selected", false),
    ];
    for (target, selected, completed, expected) in cases {
        assert_eq!(
            passive_resize_invalidates_live_geometry(target, selected, completed),
            expected,
        );
    }
}

/// A worker that stops advancing is replaced after the tmux deadline, while a
/// normal retarget clears heartbeat history. Removing either reset leaves the
/// old worker or old target's liveness attached to the displayed pane.
#[test]
fn stalled_preview_worker_restarts_and_retarget_resets_heartbeat() {
    let mut env = create_test_env_empty();
    let first_target = "aoe_test_stalled_worker".to_string();
    env.view
        .sync_preview_capture_worker(Some(first_target.clone()));

    let worker = env
        .view
        .preview_capture_worker
        .as_mut()
        .expect("worker spawned");
    let old_worker_id = worker.id_for_test();
    worker.stop_for_test();
    worker.set_cycles_for_test(17);
    env.view.preview_worker_pulse = Some((
        17,
        std::time::Instant::now()
            - crate::tmux::TMUX_COMMAND_TIMEOUT
            - std::time::Duration::from_secs(3),
    ));

    env.view
        .sync_preview_capture_worker(Some(first_target.clone()));
    let replacement = env
        .view
        .preview_capture_worker
        .as_ref()
        .expect("stalled worker replaced");
    assert_ne!(
        replacement.id_for_test(),
        old_worker_id,
        "a stalled capture worker must be replaced"
    );
    assert_eq!(
        env.view.preview_capture_target.as_deref(),
        Some(first_target.as_str()),
        "replacement must retain the displayed target"
    );

    env.view.preview_worker_pulse = Some((replacement.cycles(), std::time::Instant::now()));
    env.view
        .sync_preview_capture_worker(Some("aoe_test_retarget".to_string()));
    assert!(
        env.view.preview_worker_pulse.is_none(),
        "a retarget must start a fresh heartbeat window"
    );
}

/// #3611: trashed-row healing must not run before the first frame. `HomeView::new`
/// hands it to `ReconcilePoller`, so the repair lands through
/// `apply_reconcile_results` instead. This row needs only a pointer repair, so
/// the sweep reaches durable state without git.
#[test]
#[serial]
fn trashed_row_healing_lands_through_the_reconcile_poller() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let project = TempDir::new().unwrap();
    let storage = Storage::new_unwatched("test").unwrap();

    let recorded = project.path().join("feat");
    let mut instance = Instance::new("trashed", recorded.to_str().unwrap());
    instance.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feat".to_string(),
        main_repo_path: project.path().to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    instance.trash();
    let id = instance.id.clone();
    let holding = crate::session::trash::trash_holding_path(&recorded, &id).unwrap();
    std::fs::create_dir_all(&holding).unwrap();
    storage
        .update(|instances, _groups| {
            instances.push(instance);
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    let mut applied = false;
    for _ in 0..100 {
        if view.apply_reconcile_results() {
            applied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(applied, "the reconcile poller never reported its sweep");
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        holding.to_string_lossy(),
        "the reload must publish the healed path"
    );
}

/// the reconcile sweep's reload must respect the same live-send
/// gate every other storage reload uses, and the worker's verdict must survive
/// being skipped rather than being drained and dropped.
#[test]
#[serial]
fn reconcile_reload_waits_for_live_send_to_finish() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};

    let mut env = create_test_env_empty();
    env.view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(true);
    env.view.live_send = Some(LiveSendState {
        session_id: "s".to_string(),
        title: "s".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    assert!(
        !env.view.apply_reconcile_results(),
        "a reload must not interrupt a paste in progress"
    );

    env.view.live_send = None;
    assert!(
        env.view.apply_reconcile_results(),
        "the skipped verdict must still be waiting once live-send ends"
    );
}

/// startup auto-recovery launches from `project_path` and records
/// each attempt in a boot-scoped ledger that is not retried, so it must not run
/// until the reconcile sweep has had its chance to repoint a row whose worktree
/// moved outside aoe (#2002). `HomeView::new` therefore arms the gate instead of
/// starting recovery, and `apply_reconcile_results` releases it exactly once,
/// whether or not the sweep changed anything.
#[test]
#[serial]
fn startup_recovery_waits_for_the_first_reconcile_sweep() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    assert!(
        view.startup_recovery_gate.is_some(),
        "construction must arm the gate rather than recover from unrepaired paths"
    );

    // An unchanged sweep still releases it: the paths are now known good.
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(false);
    assert!(
        !view.apply_reconcile_results(),
        "nothing changed, so no reload"
    );
    assert!(
        view.startup_recovery_gate.is_none(),
        "the sweep landing must release the recovery gate"
    );

    // Released exactly once, so later ticks cannot re-run recovery.
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(false);
    assert!(!view.apply_reconcile_results());
    assert!(view.startup_recovery_gate.is_none());
}

/// the gate cannot outlive its deadline.
/// `Storage::update` blocks on a contended profile flock with no timeout, so a
/// peer holding that lock leaves the sweep worker neither delivering a result
/// nor disconnecting. Gating recovery on that forever would trade "recovery
/// used a stale path" for "recovery never ran", which is the worse failure.
#[test]
#[serial]
fn startup_recovery_gate_expires_when_the_sweep_never_lands() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    // A poller that never reports, standing in for a sweep blocked on a flock.
    view.reconcile_poller = crate::tui::reconcile_poller::ReconcilePoller::new();

    assert!(!view.apply_reconcile_results());
    assert!(
        view.startup_recovery_gate.is_some(),
        "an un-landed sweep inside the deadline must still hold the gate"
    );

    view.startup_recovery_gate =
        Some(std::time::Instant::now() - HomeView::STARTUP_RECOVERY_GATE_TIMEOUT);
    assert!(!view.apply_reconcile_results());
    assert!(
        view.startup_recovery_gate.is_none(),
        "past the deadline recovery must start without the sweep"
    );
}

/// A failed reload must not be retried on every tick. `apply_reconcile_results`
/// runs ~30 times a second, so an unreadable store would spin on storage and
/// flood the log where every other reload in that loop is throttled.
#[test]
#[serial]
fn a_failed_reload_backs_off_instead_of_retrying_every_tick() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut seed = Instance::new("row", "/tmp/stale-path");
    seed.source_profile = "test".to_string();
    storage
        .update(|instances, _groups| {
            instances.push(seed);
            Ok(())
        })
        .unwrap();
    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(true);

    let groups = crate::session::get_app_dir()
        .unwrap()
        .join("profiles")
        .join("test")
        .join("groups.json");
    std::fs::remove_file(&groups).ok();
    std::fs::create_dir(&groups).unwrap();

    assert!(!view.apply_reconcile_results(), "the first attempt fails");
    let armed = view
        .reconcile_reload_retry_at
        .expect("a failed reload must arm the backoff");

    // Storage is readable again, but the backoff has not elapsed, so the next
    // tick must not touch it.
    std::fs::remove_dir(&groups).unwrap();
    std::fs::write(&groups, "[]").unwrap();
    assert!(!view.apply_reconcile_results(), "still inside the backoff");
    assert_eq!(
        view.reconcile_reload_retry_at,
        Some(armed),
        "a skipped attempt must not re-arm the backoff"
    );
    assert!(view.pending_reconcile_reload, "the repair is still pending");

    // Once it elapses the retry lands.
    view.reconcile_reload_retry_at = Some(std::time::Instant::now());
    assert!(view.apply_reconcile_results(), "the retry must land");
    assert!(view.reconcile_reload_retry_at.is_none());
    assert!(!view.pending_reconcile_reload);
}

/// The deadline is also checked while live-send holds the reload, since starting
/// recovery spawns workers rather than touching the terminal.
#[test]
#[serial]
fn startup_recovery_gate_expires_during_live_send() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("test").unwrap();
    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.live_send = Some(LiveSendState {
        session_id: "s".to_string(),
        title: "s".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
    view.startup_recovery_gate =
        Some(std::time::Instant::now() - HomeView::STARTUP_RECOVERY_GATE_TIMEOUT);

    assert!(!view.apply_reconcile_results());
    assert!(
        view.startup_recovery_gate.is_none(),
        "a long paste must not strand recovery either"
    );
}

/// a repair queued in the channel must be applied to `instances`
/// before the gate opens, deadline or not. Releasing first let startup recovery
/// clone a `project_path` the sweep had already fixed on disk and spend that
/// row's one boot-scoped attempt on it, which is what the gate exists to stop.
#[test]
#[serial]
fn a_queued_repair_is_applied_before_the_gate_opens_at_the_deadline() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut seed = Instance::new("row", "/tmp/stale-path");
    seed.source_profile = "test".to_string();
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed);
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        "/tmp/stale-path"
    );

    // The sweep repaired durable storage and reported the change.
    storage
        .update(|instances, _groups| {
            instances[0].project_path = "/tmp/repaired-path".to_string();
            Ok(())
        })
        .unwrap();
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(true);
    view.startup_recovery_gate =
        Some(std::time::Instant::now() - HomeView::STARTUP_RECOVERY_GATE_TIMEOUT);

    assert!(
        view.apply_reconcile_results(),
        "the queued repair must reload"
    );
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        "/tmp/repaired-path",
        "recovery must not be released against the stale in-memory path"
    );
    assert!(view.startup_recovery_gate.is_none());
}

/// The live-send case of the same rule: the reload is postponed, so the result
/// must be preserved and the gate must stay armed even past the deadline.
#[test]
#[serial]
fn a_queued_repair_keeps_the_gate_armed_while_live_send_holds_the_reload() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut seed = Instance::new("row", "/tmp/stale-path");
    seed.source_profile = "test".to_string();
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed);
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    storage
        .update(|instances, _groups| {
            instances[0].project_path = "/tmp/repaired-path".to_string();
            Ok(())
        })
        .unwrap();
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(true);
    view.startup_recovery_gate =
        Some(std::time::Instant::now() - HomeView::STARTUP_RECOVERY_GATE_TIMEOUT);
    view.live_send = Some(LiveSendState {
        session_id: "s".to_string(),
        title: "s".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    assert!(
        !view.apply_reconcile_results(),
        "the reload waits for live-send"
    );
    assert!(
        view.startup_recovery_gate.is_some(),
        "an unapplied repair must hold the gate shut past the deadline"
    );

    // The result is preserved, not dropped, and lands once the paste ends.
    view.live_send = None;
    assert!(view.apply_reconcile_results());
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        "/tmp/repaired-path"
    );
    assert!(view.startup_recovery_gate.is_none());
}

/// a failed reload must keep the repair pending and the gate shut.
/// Clearing the flag before the fallible call dropped the repair and released
/// recovery against stale rows, which is the failure the gate exists to prevent.
#[test]
#[serial]
fn a_failed_reload_keeps_the_repair_pending_and_the_gate_shut() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let mut seed = Instance::new("row", "/tmp/stale-path");
    seed.source_profile = "test".to_string();
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed);
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    storage
        .update(|instances, _groups| {
            instances[0].project_path = "/tmp/repaired-path".to_string();
            Ok(())
        })
        .unwrap();
    view.reconcile_poller =
        crate::tui::reconcile_poller::ReconcilePoller::with_result_for_test(true);

    // A groups.json that is a directory makes `load_with_groups` fail.
    let groups = crate::session::get_app_dir()
        .unwrap()
        .join("profiles")
        .join("test")
        .join("groups.json");
    std::fs::remove_file(&groups).ok();
    std::fs::create_dir(&groups).unwrap();

    assert!(
        !view.apply_reconcile_results(),
        "the reload failed, so no refresh"
    );
    assert!(
        view.startup_recovery_gate.is_some(),
        "a dropped repair must not open the gate onto stale rows"
    );
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        "/tmp/stale-path",
        "the in-memory row is still the stale one"
    );

    // The repair is retried, not lost, once storage is readable again. The
    // retry is throttled, so let the backoff elapse as a later tick would;
    // `a_failed_reload_backs_off_instead_of_retrying_every_tick` covers the
    // throttle itself.
    std::fs::remove_dir(&groups).unwrap();
    std::fs::write(&groups, "[]").unwrap();
    view.reconcile_reload_retry_at = Some(std::time::Instant::now());
    assert!(
        view.apply_reconcile_results(),
        "the retry must land the repair"
    );
    assert_eq!(
        view.get_instance(&id).unwrap().project_path,
        "/tmp/repaired-path"
    );
    assert!(view.startup_recovery_gate.is_none());
}
