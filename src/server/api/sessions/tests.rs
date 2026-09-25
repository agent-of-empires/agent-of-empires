use super::*;

#[tokio::test]
#[serial_test::serial]
async fn create_receipts_include_published_rows_and_idempotent_retries() -> anyhow::Result<()> {
    if !crate::tmux::is_tmux_available() {
        return Ok(());
    }
    let _home = crate::session::test_support::isolate_app_dir();
    crate::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    let project = tempfile::tempdir()?;
    let state = crate::server::test_support::build_test_app_state(Vec::new());
    let _storage = Storage::new("creation-receipt", state.file_watch.clone())?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let mut created_id = None;
    let mut panes = Vec::new();
    for expected_status in [StatusCode::CREATED, StatusCode::OK] {
        let body = serde_json::from_value(serde_json::json!({
            "title": "creation receipt", "path": project.path(),
            "tool": "claude", "command_override": "sleep 120",
            "profile": "creation-receipt", "idempotency_key": "one-creation",
            "group": "team/sub",
            "fork_session_id": created_id.is_some().then_some("missing-parent")
        }))?;
        let response = create_session(
            State(state.clone()),
            axum::extract::Query(create::CreateSessionQuery { wait: None }),
            None,
            Ok(Json(body)),
        )
        .await
        .into_response();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        for instance in _storage.load()? {
            panes.push(crate::tmux::test_helpers::TmuxTestSession::from_name(
                crate::tmux::Session::generate_name(&instance.id, &instance.title),
            ));
        }
        assert_eq!(
            status,
            expected_status,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let row: SessionResponse = serde_json::from_slice(&bytes)?;
        let epoch = headers
            .get(crate::daemon::RUNTIME_EPOCH_HEADER)
            .expect("creation must return its publication epoch")
            .to_str()?;
        let revision = headers
            .get(crate::daemon::RUNTIME_REVISION_HEADER)
            .expect("creation must return its publication revision")
            .to_str()?
            .parse::<u64>()?;
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(snapshot.value.cursor.epoch, epoch);
        assert!(snapshot.value.cursor.revision >= revision);
        let published = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|item| item.id == row.id)
            .expect("the receipt precedes the canonical row");
        assert_eq!(published.profile, "creation-receipt");
        let profile = snapshot
            .value
            .contents
            .profiles
            .iter()
            .find(|profile| profile.name == "creation-receipt")
            .unwrap();
        assert!(profile.groups.iter().any(|group| group.path == "team"));
        assert!(profile.groups.iter().any(|group| group.path == "team/sub"));
        if let Some(id) = &created_id {
            assert_eq!(&row.id, id, "retry created another session");
        }
        created_id = Some(row.id);
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn canonical_fork_refusals_do_not_create_sessions() -> anyhow::Result<()> {
    async fn request(state: Arc<AppState>, body: serde_json::Value) -> axum::response::Response {
        create_session(
            State(state),
            axum::extract::Query(create::CreateSessionQuery { wait: None }),
            None,
            Ok(Json(create_body_from_json(body))),
        )
        .await
        .into_response()
    }

    let _home = crate::session::test_support::isolate_app_dir();
    let project = tempfile::tempdir()?;
    let mut source = Instance::new("fork source", project.path().to_str().unwrap());
    source.source_profile = "fork-source".into();
    source.tool = "claude".into();
    source.status = Status::Stopped;
    source.agent_session_id = Some("11111111-1111-4111-8111-111111111111".into());
    let id = source.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![source.clone()]);
    let storage = Storage::new("fork-source", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(source.clone());
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    for (row, raw, tool, expected) in [
        (
            Some("missing-parent"),
            None,
            "claude",
            StatusCode::NOT_FOUND,
        ),
        (Some(id.as_str()), None, "codex", StatusCode::BAD_REQUEST),
        (
            Some(id.as_str()),
            Some("parent-id"),
            "claude",
            StatusCode::BAD_REQUEST,
        ),
        (None, Some("../escape"), "claude", StatusCode::BAD_REQUEST),
    ] {
        let response = request(
            state.clone(),
            serde_json::json!({
                "path": project.path(), "profile": "fork-source", "tool": tool,
                "fork_session_id": row, "fork_from": raw,
            }),
        )
        .await;
        assert_eq!(response.status(), expected);
    }
    state.instances.write().await[0].try_acquire_lifecycle_reservation(
        crate::session::LifecycleOperation::Launch,
        Instance::LIFECYCLE_RESERVATION_TTL,
        chrono::Utc::now(),
    )?;
    let body = serde_json::json!({
        "path": project.path(), "profile": "fork-source", "tool": "claude",
        "fork_session_id": id,
    });
    let response = request(state.clone(), body.clone()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        crate::daemon::ApiErrorCode::from_headers(response.status(), response.headers(), false),
        Some(crate::daemon::ApiErrorCode::LifecycleLocked)
    );
    {
        let mut rows = state.instances.write().await;
        rows[0].lifecycle_reservation = None;
        rows[0].agent_session_id = None;
    }
    assert_eq!(
        request(state.clone(), body).await.status(),
        StatusCode::BAD_REQUEST
    );
    let snapshot = state.runtime.snapshot(&state).await?;
    assert_eq!(
        snapshot
            .value
            .contents
            .sessions
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec![id.as_str()]
    );
    let cityhall = crate::server::test_support::build_test_app_state_cityhall(vec![source]);
    for parent in [id.as_str(), "missing-parent"] {
        let response = request(
            cityhall.clone(),
            serde_json::json!({
                "path": project.path(), "tool": "claude", "fork_session_id": parent,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            crate::daemon::ApiErrorCode::from_headers(response.status(), response.headers(), false),
            Some(crate::daemon::ApiErrorCode::CityhallMode)
        );
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn tool_ensure_refuses_restricted_or_degraded_runtime() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (read_only, cityhall, degraded, status, code) in [
        (
            true,
            false,
            false,
            StatusCode::FORBIDDEN,
            Some(crate::daemon::ApiErrorCode::ReadOnly),
        ),
        (
            false,
            true,
            false,
            StatusCode::FORBIDDEN,
            Some(crate::daemon::ApiErrorCode::CityhallMode),
        ),
        (false, false, true, StatusCode::SERVICE_UNAVAILABLE, None),
    ] {
        let row = Instance::new("denied tool", "/unused-tool-project");
        let id = row.id.clone();
        let state =
            crate::server::test_support::build_test_app_state_configured(vec![row], |state| {
                state.read_only = read_only;
                state.cityhall_mode = cityhall;
            });
        if degraded {
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
        }
        let response = ensure_tool(
            State(state),
            Path(id),
            Ok(Json(crate::daemon::EnsureToolBody {
                tool_name: "probe".into(),
                size: None,
            })),
        )
        .await
        .into_response();
        assert_eq!(response.status(), status);
        assert_eq!(
            crate::daemon::ApiErrorCode::from_headers(status, response.headers(), false),
            code
        );
        assert!(!response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn metadata_commit_rejects_partial_rows_without_rewriting_them() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut row = Instance::new("complete commit", "/tmp/complete-commit");
    row.source_profile = "complete".into();
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);

    let storage = Storage::new("complete", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let mut partial: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(storage.sessions_path())?)?;
    partial.push(serde_json::json!({"id": 5}));
    let damaged = serde_json::to_vec(&partial)?;
    std::fs::write(storage.sessions_path(), &damaged)?;
    let response = update_session_color(
        State(state.clone()),
        Path(id.clone()),
        Ok(Json(serde_json::from_value(
            serde_json::json!({"color": "red"}),
        )?)),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response
        .headers()
        .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
    assert_eq!(std::fs::read(storage.sessions_path())?, damaged);
    assert!(!storage
        .sessions_path()
        .with_file_name("sessions.corrupt.jsonl")
        .exists());
    assert_eq!(
        *state.canonical_health.read().await,
        crate::daemon::RuntimeHealth::Degraded {
            code: crate::daemon::ReloadFailureCode::ProfileData,
            profiles: vec!["complete".into()],
        }
    );
    let rows = state.instances.read().await;
    assert!(rows
        .iter()
        .find(|row| row.id == id)
        .unwrap()
        .color
        .is_none());
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn lifecycle_requests_reject_a_peer_reservation_without_acknowledging_it(
) -> anyhow::Result<()> {
    use crate::session::LifecycleOperation;
    for (action, operation, status) in [
        ("stop", LifecycleOperation::Stop, Status::Idle),
        ("start", LifecycleOperation::Launch, Status::Stopped),
        ("archive", LifecycleOperation::Stop, Status::Waiting),
        ("trash", LifecycleOperation::Purge, Status::Deleting),
        ("restore", LifecycleOperation::Purge, Status::Deleting),
    ] {
        let _guard = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("reserved lifecycle", "/tmp/reserved-lifecycle");
        row.source_profile = "receipt".into();
        row.status = status;
        if action == "restore" {
            row.trash();
        }
        row.try_acquire_lifecycle_reservation(
            operation,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        let id = row.id.clone();
        let generation = row.lifecycle_generation;
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);

        let storage = Storage::new("receipt", state.file_watch.clone())?;
        storage.update(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        let response = match action {
            "start" => start_session(State(state.clone()), Path(id.clone()), Ok(None))
                .await
                .into_response(),
            "stop" => stop_session(State(state.clone()), Path(id.clone()))
                .await
                .into_response(),
            "archive" => update_session_archive(
                State(state.clone()),
                Path(id.clone()),
                Ok(Json(UpdateArchiveBody {
                    archived: true,
                    kill_pane: false,
                })),
            )
            .await
            .into_response(),
            "trash" => trash_session(
                State(state.clone()),
                Path(id.clone()),
                Some(Json(TrashSessionBody { kill_pane: false })),
            )
            .await
            .into_response(),
            "restore" => restore_session(State(state.clone()), Path(id.clone()))
                .await
                .into_response(),
            _ => unreachable!(),
        };
        assert_eq!(response.status(), StatusCode::CONFLICT, "{action}");
        assert_eq!(
            response
                .headers()
                .get(crate::daemon::ERROR_CODE_HEADER)
                .unwrap(),
            crate::daemon::ApiErrorCode::LifecycleLocked.as_str()
        );
        assert!(!response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        assert_eq!(
            *state.canonical_health.read().await,
            crate::daemon::RuntimeHealth::Healthy
        );
        let row = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(row.status, status);
        assert!(row.lifecycle_reservation_is_owned(operation, generation));
        assert_eq!(
            state
                .instances
                .read()
                .await
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .status,
            status
        );
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn restart_refusal_preserves_authoritative_launch_fields() -> anyhow::Result<()> {
    for refusal in [
        "read_only",
        "cityhall",
        "degraded",
        "reserved",
        "profile_collision",
    ] {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("restart refusal", "/tmp/restart-refusal");
        row.source_profile = "receipt".into();
        row.tool = "claude".into();
        row.command = "original-wrapper".into();
        row.extra_args = "--original".into();
        row.agent_session_id = Some("original-session".into());
        row.snooze(30);
        row.status = Status::Running;
        if refusal == "reserved" {
            row.try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )?;
        }
        let before = row.clone();
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state_configured(
            vec![row.clone()],
            |state| {
                state.read_only = refusal == "read_only";
                state.cityhall_mode = refusal == "cityhall";
            },
        );
        let storage = Storage::new("receipt", state.file_watch.clone())?;
        storage.update(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        if refusal == "profile_collision" {
            Storage::new("target", state.file_watch.clone())?.update(|rows, _| {
                rows.push(Instance::new("restart refusal", "/tmp/restart-refusal/"));
                Ok(())
            })?;
        }
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        if refusal == "degraded" {
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
        }
        let response = restart_session(
            State(state.clone()),
            Path(id.clone()),
            Ok(Some(Json(crate::daemon::RestartSessionBody {
                profile: (refusal == "profile_collision").then(|| "target".into()),
                tool: Some("codex".into()),
                command_override: Some("replacement-wrapper".into()),
                extra_args: Some("--replacement".into()),
                unsnooze: true,
                ..Default::default()
            }))),
        )
        .await
        .into_response();
        assert!(!response.status().is_success(), "{refusal}");
        assert!(!response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        let stored = storage.load()?;
        let live = state.instances.read().await;
        for row in [
            stored.iter().find(|row| row.id == id).unwrap(),
            live.iter().find(|row| row.id == id).unwrap(),
        ] {
            assert_eq!(row.tool, before.tool, "{refusal}");
            assert_eq!(row.command, before.command, "{refusal}");
            assert_eq!(row.extra_args, before.extra_args, "{refusal}");
            assert_eq!(row.snoozed_until, before.snoozed_until, "{refusal}");
            assert_eq!(row.agent_session_id, before.agent_session_id, "{refusal}");
            assert_eq!(
                row.lifecycle_generation, before.lifecycle_generation,
                "{refusal}"
            );
            assert_eq!(row.status, Status::Running, "{refusal}");
        }
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn running_restart_respawns_while_start_remains_idempotent() -> anyhow::Result<()> {
    use crate::tmux::test_helpers::{pane_field, TmuxTestSession};
    use std::time::Duration;
    if !crate::tmux::is_tmux_available() {
        return Ok(());
    }
    let _home = crate::session::test_support::isolate_app_dir();
    crate::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    let project = tempfile::tempdir()?;
    let mut row = Instance::new("running restart", project.path().to_str().unwrap());
    row.source_profile = "receipt".into();
    row.command = "sleep 60".into();
    row.status = Status::Running;
    let id = row.id.clone();
    let generation = row.lifecycle_generation;
    let name = crate::tmux::Session::generate_name(&id, &row.title);
    let _pane = TmuxTestSession::from_name(name.clone());
    row.tmux_session()?.create(
        project.path().to_str().unwrap(),
        Some("sleep 60"),
        "receipt",
    )?;
    let original_pid = pane_field(&name, "#{pane_pid}");
    assert!(!original_pid.is_empty(), "created pane owns a live process");
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;

    let started = start_session(State(state.clone()), Path(id.clone()), Ok(None))
        .await
        .into_response();
    assert_eq!(started.status(), StatusCode::OK);
    assert_eq!(pane_field(&name, "#{pane_pid}"), original_pid);
    assert_eq!(
        storage
            .load()?
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .lifecycle_generation,
        generation
    );

    let restarted = restart_session(
        State(state.clone()),
        Path(id.clone()),
        Ok(Some(Json(crate::daemon::RestartSessionBody {
            wake_message: Some(String::new()),
            ..Default::default()
        }))),
    )
    .await
    .into_response();
    assert_eq!(restarted.status(), StatusCode::OK);
    let revision = restarted.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
        .to_str()?
        .parse::<u64>()?;
    let bytes = axum::body::to_bytes(restarted.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let outcome: crate::daemon::RestartOutcome = serde_json::from_value(body["outcome"].clone())?;
    assert!(outcome.lifecycle_generation > generation);
    assert_eq!(outcome.target.as_ref().unwrap().tmux_session, name);
    let respawn_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let respawned_pid = loop {
        let pid = pane_field(&name, "#{pane_pid}");
        if !pid.is_empty() && pid != original_pid {
            break pid;
        }
        assert!(
            std::time::Instant::now() < respawn_deadline,
            "pane never respawned after restart"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!respawned_pid.is_empty());
    let snapshot = state.runtime.snapshot(&state).await?;
    assert_eq!(snapshot.value.cursor.revision, revision);
    let published = snapshot
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(published.lifecycle_generation, outcome.lifecycle_generation);
    assert_eq!(
        storage
            .load()?
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .lifecycle_generation,
        outcome.lifecycle_generation
    );
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn restart_delivers_requested_wake_message_to_new_pane() -> anyhow::Result<()> {
    if !crate::tmux::is_tmux_available() {
        return Ok(());
    }
    let _home = crate::session::test_support::isolate_app_dir();
    crate::session::config::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    let project = tempfile::tempdir()?;
    let marker = project.path().join("wake-received");
    let mut row = Instance::new("wake-on-restart", project.path().to_str().unwrap());
    row.source_profile = "wake-profile".into();
    row.status = Status::Running;
    row.command = format!(
        r#"sh -c 'read message; printf "%s" "$message" > {}; sleep 60'"#,
        marker.display(),
    );
    let id = row.id.clone();
    let name = crate::tmux::Session::generate_name(&id, &row.title);
    let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name.clone());
    row.tmux_session()?.create(
        project.path().to_str().unwrap(),
        Some("sleep 60"),
        "wake-profile",
    )?;
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("wake-profile", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;

    let response = restart_session(
        State(state.clone()),
        Path(id),
        Ok(Some(Json(crate::daemon::RestartSessionBody {
            wake_message: Some("wake-message-verified".into()),
            ..Default::default()
        }))),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(&marker).ok().as_deref() == Some("wake-message-verified") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "restart wake did not reach the new pane; marker={:?}; pane={:?}",
            std::fs::read_to_string(&marker),
            crate::tmux::Session::from_name(&name).capture_pane(30),
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn archived_session_cannot_be_started_or_ensured() -> anyhow::Result<()> {
    let _home = crate::session::test_support::isolate_app_dir();
    let mut row = Instance::new("archived launch", "/tmp/archived-launch");
    row.source_profile = "archive-guard".into();
    row.status = Status::Stopped;
    row.archived_at = Some(chrono::Utc::now());
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("archive-guard", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;

    let start = start_session(State(state.clone()), Path(id.clone()), Ok(None))
        .await
        .into_response();
    let ensure = ensure_session(State(state.clone()), Path(id.clone()), Ok(None))
        .await
        .into_response();
    assert_eq!(start.status(), StatusCode::CONFLICT);
    assert_eq!(ensure.status(), StatusCode::CONFLICT);
    let saved = storage.load()?;
    let saved = saved.iter().find(|row| row.id == id).unwrap();
    assert!(saved.is_archived());
    assert_eq!(saved.status, Status::Stopped);
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn structured_stop_start_receipts_include_the_committed_peer_bundle() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut row = Instance::new("lifecycle receipt", "/tmp/lifecycle-receipt");
    row.source_profile = "receipt".into();
    row.view = crate::session::View::Structured;
    row.status = Status::Idle;
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);

    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let mut revision = state.runtime.publish(&state).await?.value.cursor.revision;
    let mut peer = Instance::new("peer", "/tmp/lifecycle-peer");
    peer.source_profile = "receipt".into();
    let peer_id = peer.id.clone();
    storage.update(|rows, _| {
        rows.push(peer);
        Ok(())
    })?;
    for stopped in [true, true, false, false] {
        let response = if stopped {
            stop_session(State(state.clone()), Path(id.clone()))
                .await
                .into_response()
        } else {
            start_session(State(state.clone()), Path(id.clone()), Ok(None))
                .await
                .into_response()
        };
        assert_eq!(response.status(), StatusCode::OK);
        let receipt = response
            .headers()
            .get(crate::daemon::RUNTIME_REVISION_HEADER)
            .expect("lifecycle response must carry its reflection cursor")
            .to_str()?
            .parse::<u64>()?;
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(receipt, snapshot.value.cursor.revision);
        assert!(receipt >= revision);
        revision = receipt;
        assert!(snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == peer_id));
        let row = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(
            row.status,
            if stopped {
                Status::Stopped
            } else {
                Status::Idle
            }
            .wire_str()
        );
        let stored = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(stored.status == Status::Stopped, stopped);
        assert_eq!(stored.idle_dormant_since.is_some(), stopped);
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn archive_receipts_publish_committed_status_and_peer_rows() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut row = Instance::new("archive receipt", "/tmp/archive-receipt");
    row.source_profile = "receipt".into();
    row.status = Status::Waiting;
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    state.runtime.publish(&state).await?;
    let peer = Instance::new("peer", "/tmp/archive-peer");
    let peer_id = peer.id.clone();
    storage.update(|rows, _| {
        rows.push(peer);
        Ok(())
    })?;
    for (archived, kill_pane, expected_status) in [
        (true, false, Status::Idle),
        (false, false, Status::Idle),
        (true, true, Status::Stopped),
        (false, false, Status::Stopped),
    ] {
        let response = update_session_archive(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(UpdateArchiveBody {
                archived,
                kill_pane,
            })),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let receipt = response
            .headers()
            .get(crate::daemon::RUNTIME_REVISION_HEADER)
            .expect("archive must acknowledge its published commit")
            .to_str()?
            .parse::<u64>()?;
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(receipt, snapshot.value.cursor.revision);
        assert!(snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == peer_id));
        let row = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(row.archived_at.is_some(), archived);
        assert_eq!(row.status, expected_status.wire_str());
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn engagement_unsinks_archived_and_snoozed_row_in_one_canonical_commit() -> anyhow::Result<()>
{
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut row = Instance::new("engagement", "/tmp/engagement");
    row.source_profile = "receipt".into();
    row.archive();
    row.snooze(30);
    row.last_accessed_at = Some(chrono::Utc::now() - chrono::Duration::days(1));
    let previous_access = row.last_accessed_at;
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    state.runtime.publish(&state).await?;
    let response = touch_session_access(State(state.clone()), Path(id.clone()))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let snapshot = state.runtime.snapshot(&state).await?;
    assert_eq!(
        response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
            .to_str()?
            .parse::<u64>()?,
        snapshot.value.cursor.revision
    );
    let disk = storage.load()?;
    let stored = disk.iter().find(|row| row.id == id).unwrap();
    assert!(!stored.is_archived());
    assert!(stored.snoozed_until.is_none());
    assert!(stored.last_accessed_at > previous_access);
    let published = snapshot
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert!(published.archived_at.is_none());
    assert!(published.snoozed_until.is_none());
    Ok(())
}
#[tokio::test]
#[serial_test::serial]
async fn archive_and_trash_receipts_discard_killed_auxiliary_observations() -> anyhow::Result<()> {
    use crate::session::{AuxiliaryTarget, PanePresence};
    if !crate::tmux::is_tmux_available() {
        return Ok(());
    }
    for archive in [true, false] {
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("auxiliary receipt", project.path().to_str().unwrap());
        row.source_profile = "receipt".into();
        row.status = Status::Stopped;
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        let storage = Storage::new("receipt", state.file_watch.clone())?;
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        let mut panes = Vec::new();
        for index in [0, 1] {
            let name =
                crate::server::pane::respawn_paired_if_dead(&state, &id, &row, index).await?;
            panes.push(crate::tmux::test_helpers::TmuxTestSession::from_name(name));
        }
        let response = if archive {
            update_session_archive(
                State(state.clone()),
                Path(id.clone()),
                Ok(Json(UpdateArchiveBody {
                    archived: true,
                    kill_pane: true,
                })),
            )
            .await
            .into_response()
        } else {
            trash_session(
                State(state.clone()),
                Path(id.clone()),
                Some(Json(TrashSessionBody { kill_pane: true })),
            )
            .await
            .into_response()
        };
        assert_eq!(response.status(), StatusCode::OK);
        let receipt = response
            .headers()
            .get(crate::daemon::RUNTIME_REVISION_HEADER)
            .unwrap()
            .to_str()?
            .parse::<u64>()?;
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(snapshot.value.cursor.revision, receipt);
        let published = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|item| item.id == id)
            .unwrap();
        assert_eq!(
            published
                .auxiliary
                .iter()
                .find(|item| item.target == AuxiliaryTarget::Host { index: 0 })
                .map(|item| item.pane.state),
            Some(PanePresence::Absent),
            "archive={archive}"
        );
        assert!(
            !published
                .auxiliary
                .iter()
                .any(|item| item.target == AuxiliaryTarget::Host { index: 1 }
                    && item.pane.state == PanePresence::Alive),
            "archive={archive}: {:?}",
            published.auxiliary
        );
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn trash_restore_receipts_publish_complete_peer_bundles() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    let root = tempfile::tempdir()?;
    let main_repo = root.path().join("main");
    let worktree = root.path().join("worktree");
    let repo = git2::Repository::init(&main_repo)?;
    let signature = git2::Signature::now("Test", "test@example.com")?;
    let tree = repo.find_tree(repo.index()?.write_tree()?)?;
    repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])?;
    crate::git::GitWorktree::new(main_repo.clone())?.create_worktree(
        "receipt-branch",
        &worktree,
        true,
        None,
    )?;
    std::fs::write(worktree.join("payload"), b"retained")?;
    let mut row = Instance::new("trash receipt", worktree.to_str().unwrap());
    row.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "receipt-branch".into(),
        main_repo_path: main_repo.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    row.source_profile = "receipt".into();
    row.status = Status::Stopped;
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    state.runtime.publish(&state).await?;
    for trashed in [true, false] {
        let peer = Instance::new("peer", "/tmp/trash-peer");
        let peer_id = peer.id.clone();
        storage.update(|rows, _| {
            rows.push(peer);
            Ok(())
        })?;
        let before_path = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
            .project_path;
        let identity = crate::session::acquire_session_identity_lock()?;
        let worker_state = state.clone();
        let worker_id = id.clone();
        let mut worker = tokio::spawn(async move {
            if trashed {
                trash_session(
                    State(worker_state),
                    Path(worker_id),
                    Some(Json(TrashSessionBody { kill_pane: false })),
                )
                .await
                .into_response()
            } else {
                restore_session(State(worker_state), Path(worker_id))
                    .await
                    .into_response()
            }
        });
        let premature = tokio::time::timeout(std::time::Duration::from_secs(2), &mut worker)
            .await
            .ok();
        let visible = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        let path_present = std::path::Path::new(&before_path).is_dir();
        drop(identity);
        let response = match premature {
            Some(response) => response?,
            None => worker.await?,
        };
        assert_eq!(
            visible.project_path, before_path,
            "resource references changed during cleanup exclusion"
        );
        assert!(path_present, "worktree moved during cleanup exclusion");
        assert_eq!(response.status(), StatusCode::OK);
        let receipt = response
            .headers()
            .get(crate::daemon::RUNTIME_REVISION_HEADER)
            .expect("trash and restore must acknowledge their published commit")
            .to_str()?
            .parse::<u64>()?;
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(receipt, snapshot.value.cursor.revision);
        assert!(snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == peer_id));
        let row = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(row.trashed_at.is_some(), trashed);
        assert_eq!(row.status, Status::Stopped.wire_str());
        let stored = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(stored.lifecycle_reservation.is_none());
        let expected = if trashed {
            crate::session::trash::trash_holding_path(&worktree, &id).unwrap()
        } else {
            worktree.clone()
        };
        assert_eq!(std::path::Path::new(&stored.project_path), expected);
        assert_eq!(std::fs::read(expected.join("payload"))?, b"retained");
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn purge_receipt_publishes_removal_and_committed_peer_rows() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    crate::session::purge_owners::initialize(&crate::session::get_app_dir()?)?;
    let mut row = Instance::new("purge receipt", "/tmp/purge-receipt");
    row.source_profile = "receipt".into();
    row.status = Status::Stopped;
    row.trash();
    let id = row.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
    let storage = Storage::new("receipt", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    state.runtime.publish(&state).await?;
    let peer = Instance::new("peer", "/tmp/purge-peer");
    let peer_id = peer.id.clone();
    storage.update(|rows, _| {
        rows.push(peer);
        Ok(())
    })?;
    let response = delete_session(State(state.clone()), Path(id.clone()), None)
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let receipt = response
        .headers()
        .get(crate::daemon::RUNTIME_REVISION_HEADER)
        .expect("purge must acknowledge its published removal")
        .to_str()?
        .parse::<u64>()?;
    let snapshot = state.runtime.snapshot(&state).await?;
    assert_eq!(receipt, snapshot.value.cursor.revision);
    assert!(!snapshot
        .value
        .contents
        .sessions
        .iter()
        .any(|row| row.id == id));
    assert!(snapshot
        .value
        .contents
        .sessions
        .iter()
        .any(|row| row.id == peer_id));
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn workspace_purge_retains_shared_files_when_structured_shutdown_is_unproven(
) -> anyhow::Result<()> {
    let _home = crate::session::test_support::isolate_app_dir();
    crate::session::purge_owners::initialize(&crate::session::get_app_dir()?)?;
    let mut owner = Instance::new("owner", "");
    let root = crate::session::scratch::provision_scratch_dir(&owner.id)?;
    owner.project_path = root.to_string_lossy().into_owned();
    owner.source_profile = "shutdown-proof".into();
    owner.status = Status::Stopped;
    owner.scratch = true;
    let mut sibling = Instance::new("sibling", &owner.project_path);
    sibling.source_profile = owner.source_profile.clone();
    sibling.status = Status::Stopped;
    sibling.view = crate::session::View::Structured;
    let owner_id = owner.id.clone();
    let sibling_id = sibling.id.clone();
    std::fs::write(root.join("payload"), b"live workspace")?;
    let state =
        crate::server::test_support::build_test_app_state(vec![owner.clone(), sibling.clone()]);
    let storage = Storage::new("shutdown-proof", state.file_watch.clone())?;
    storage.update(|rows, _| {
        rows.extend([owner, sibling]);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    std::fs::create_dir_all(crate::process::worker_registry::record_path(&sibling_id)?)?;
    let plan = vec![
        (sibling_id.clone(), DeleteSessionBody::default()),
        (owner_id.clone(), DeleteSessionBody::default()),
    ];
    let (deleted, _, failed, _, _) =
        purge_workspace_artifacts(&state, owner_id.clone(), plan, false).await;
    assert_eq!(std::fs::read(root.join("payload"))?, b"live workspace");
    assert_eq!(deleted, vec![sibling_id.clone()]);
    assert!(failed.iter().any(|failure| failure.id == sibling_id));
    let rows = storage.load()?;
    assert!(rows.iter().any(|row| row.id == owner_id));
    assert!(!rows.iter().any(|row| row.id == sibling_id));
    drop(state);
    let mut recovered = storage.load()?;
    for row in &mut recovered {
        row.source_profile = "shutdown-proof".into();
    }
    let state = crate::server::test_support::build_test_app_state(recovered);
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let response = delete_session(
        State(state),
        Path(owner_id),
        Some(Json(DeleteSessionBody::default())),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read(root.join("payload"))
            .expect("later purge after supervisor reconstruction deleted unresolved runtime files"),
        b"live workspace"
    );
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn workspace_purge_keeps_owner_when_a_sibling_was_restored() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut owner = Instance::new("owner", "/tmp/workspace-purge");
    owner.source_profile = "workspace-purge".into();
    owner.status = Status::Stopped;
    let mut sibling = Instance::new("sibling", "/tmp/workspace-purge");
    sibling.source_profile = owner.source_profile.clone();
    sibling.status = Status::Stopped;
    sibling.trash();
    let owner_id = owner.id.clone();
    let sibling_id = sibling.id.clone();
    let state =
        crate::server::test_support::build_test_app_state(vec![owner.clone(), sibling.clone()]);
    let storage = Storage::new("workspace-purge", state.file_watch.clone())?;
    sibling.untrash();
    storage.update(|rows, _| {
        rows.extend([owner, sibling]);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let response = delete_workspace(
        State(state.clone()),
        Some(Json(DeleteWorkspaceBody {
            session_ids: vec![owner_id.clone(), sibling_id.clone()],
            ..Default::default()
        })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let snapshot = state.runtime.snapshot(&state).await?;
    assert!(
        snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == owner_id),
        "a restored sibling still needs its workspace owner"
    );
    assert!(snapshot
        .value
        .contents
        .sessions
        .iter()
        .any(|row| row.id == sibling_id && row.trashed_at.is_none()));
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn overlapping_workspace_purges_complete_without_lock_inversion() -> anyhow::Result<()> {
    let _guard = crate::session::test_support::isolate_app_dir();
    crate::session::purge_owners::initialize(&crate::session::get_app_dir()?)?;
    let mut rows = vec![
        Instance::new("left", "/tmp/purge-overlap"),
        Instance::new("right", "/tmp/purge-overlap"),
    ];
    for row in &mut rows {
        row.source_profile = "purge-overlap".into();
        row.status = Status::Stopped;
    }
    let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    let state = crate::server::test_support::build_test_app_state(rows.clone());
    let storage = Storage::new("purge-overlap", state.file_watch.clone())?;
    storage.update(|stored, _| {
        stored.extend(rows);
        Ok(())
    })?;
    *state.canonical_metadata.write().await =
        crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
    let left_gate = state.session_service.prompt_submission(&ids[0]).await;
    let right_gate = state.session_service.prompt_submission(&ids[1]).await;
    let mut claims = state.session_service.watch_submission_claims();
    let launch = |owner: String, sibling: String| {
        let state = state.clone();
        tokio::spawn(async move {
            let plan = vec![
                (sibling, DeleteSessionBody::default()),
                (owner.clone(), DeleteSessionBody::default()),
            ];
            purge_workspace_artifacts(&state, owner, plan, false).await
        })
    };
    let mut left = launch(ids[0].clone(), ids[1].clone());
    let mut right = launch(ids[1].clone(), ids[0].clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        claims.recv().await.unwrap();
        claims.recv().await.unwrap();
    })
    .await?;
    drop(right_gate);
    drop(left_gate);
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(&mut left, &mut right)
    })
    .await;
    if completed.is_err() {
        left.abort();
        right.abort();
    }
    let (left, right) =
        completed.expect("overlapping purge commands must not hold each other indefinitely");
    let (mut deleted, _, left_failed, _, _) = left?;
    let (right_deleted, _, right_failed, _, _) = right?;
    assert!(left_failed.is_empty() && right_failed.is_empty());
    deleted.extend(right_deleted);
    deleted.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(deleted, expected);
    assert!(storage.load()?.is_empty());
    assert!(state.instances.read().await.is_empty());
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn pin_completion_is_reflected_in_the_canonical_snapshot() {
    use tower::ServiceExt;

    let _home = crate::session::test_support::isolate_app_dir();
    let profile = "pin-reflection";
    let seed = Instance::new("pin-target", "/tmp/repo");
    let id = seed.id.clone();
    Storage::new_unwatched(profile)
        .unwrap()
        .update(|rows, _| {
            rows.push(seed);
            Ok(())
        })
        .unwrap();
    let loaded =
        crate::server::reload::load_all_profiles(&crate::file_watch::FileWatchService::noop())
            .unwrap();
    let state = crate::server::test_support::build_test_app_state_with_policy(
        loaded.instances,
        vec!["localhost".into()],
        Vec::new(),
        None,
    );

    *state.canonical_metadata.write().await = loaded.metadata;
    let before = state.runtime.publish(&state).await.unwrap();
    let mut peer = Instance::new("peer-target", "/tmp/peer");
    let peer_id = peer.id.clone();
    peer.group_path = "peer/group".into();
    Storage::new_unwatched(profile)
        .unwrap()
        .update(|rows, groups| {
            rows.push(peer);
            let mut group = crate::session::Group::new("group", "peer/group");
            group.collapsed = true;
            groups.push(group);
            Ok(())
        })
        .unwrap();
    let app = crate::server::test_support::build_router_for_test(state.clone());
    let request = |epochs: &[&str]| {
        let mut request = axum::http::Request::builder()
            .method("PATCH")
            .uri(format!("/api/sessions/{id}/pin"))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .extension(axum::extract::ConnectInfo(
                crate::server::peer::ConnectionPeer::UnixOwner {
                    uid: nix::unistd::geteuid().as_raw(),
                },
            ));
        for epoch in epochs {
            request = request.header(crate::daemon::RUNTIME_EPOCH_HEADER, *epoch);
        }
        request
            .body(axum::body::Body::from(r#"{"pinned":true}"#))
            .unwrap()
    };
    let epoch = before.value.cursor.epoch.as_str();
    for invalid in [&["previous-daemon-lifetime"][..], &[epoch, epoch][..]] {
        let response = app.clone().oneshot(request(invalid)).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.headers()[crate::daemon::ERROR_CODE_HEADER],
            "runtime_epoch_mismatch"
        );
    }
    assert_eq!(
        state.runtime.snapshot(&state).await.unwrap().value.cursor,
        before.value.cursor
    );
    let response = app.oneshot(request(&[epoch])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let reflected = state.runtime.snapshot(&state).await.unwrap();
    let row = reflected
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert!(
        row.pinned_at.is_some(),
        "a completed pin must already be visible in the shared snapshot"
    );
    assert_eq!(reflected.value.cursor.epoch, before.value.cursor.epoch);
    assert!(reflected.value.cursor.revision > before.value.cursor.revision);
    assert_eq!(
        response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
        reflected.value.cursor.epoch.as_str()
    );
    assert_eq!(
        response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        reflected.value.cursor.revision
    );
    assert!(reflected
        .value
        .contents
        .sessions
        .iter()
        .any(|row| { row.id == peer_id && row.group_path == "peer/group" }));
    let profile = reflected
        .value
        .contents
        .profiles
        .iter()
        .find(|entry| entry.name == profile)
        .unwrap();
    assert!(profile
        .groups
        .iter()
        .any(|group| { group.path == "peer/group" && group.collapsed }));
}

#[tokio::test]
#[serial_test::serial]
async fn abandon_purge_requires_current_ownership_without_waiting_for_teardown(
) -> anyhow::Result<()> {
    use crate::session::LifecycleOperation;
    use tower::ServiceExt;

    let _home = crate::session::test_support::isolate_app_dir();
    let profile = "abandon-purge";
    let mut row = Instance::new("blocked purge", "/tmp/abandon-purge");
    row.status = Status::Stopped;
    let generation = row.try_acquire_lifecycle_reservation(
        LifecycleOperation::Purge,
        Instance::LIFECYCLE_RESERVATION_TTL,
        chrono::Utc::now(),
    )?;
    let id = row.id.clone();
    let storage = Storage::new_unwatched(profile)?;
    storage.update(|rows, _| {
        rows.push(row);
        Ok(())
    })?;
    let loaded =
        crate::server::reload::load_all_profiles(&crate::file_watch::FileWatchService::noop())?;
    let state = crate::server::test_support::build_test_app_state_with_policy(
        loaded.instances,
        vec!["localhost".into()],
        Vec::new(),
        None,
    );
    *state.canonical_metadata.write().await = loaded.metadata;
    let before = state.runtime.publish(&state).await?;
    let lifecycle = storage.acquire_instance_lifecycle_lock(&id)?;
    let submission = state.session_service.prompt_submission(&id).await;
    let instance_guard = state.instance_lock(&id).await.lock_owned().await;
    let mut background = tokio::task::JoinSet::new();
    let mut claims = state.session_service.watch_submission_claims();
    background.spawn({
        let state = state.clone();
        let id = id.clone();
        async move {
            let _ = delete_session(State(state), Path(id), None).await;
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), claims.recv())
        .await?
        .unwrap();
    let completed = delete_session(
        State(state.clone()),
        Path("missing-purge-reader".into()),
        None,
    )
    .await
    .into_response();
    assert_eq!(completed.status(), StatusCode::NOT_FOUND);
    let (queued, waiting) = tokio::sync::oneshot::channel();
    background.spawn({
        let namespace = state.profile_namespace.clone();
        async move {
            queued.send(()).unwrap();
            let _exclusive = namespace.write().await;
        }
    });
    waiting.await?;
    let app = crate::server::test_support::build_router_for_test(state.clone());
    let request = |expected_generation| {
        axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{id}/purge/abandon"))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header(
                crate::daemon::RUNTIME_EPOCH_HEADER,
                before.value.cursor.epoch.as_str(),
            )
            .extension(axum::extract::ConnectInfo(
                crate::server::peer::ConnectionPeer::UnixOwner {
                    uid: nix::unistd::geteuid().as_raw(),
                },
            ))
            .body(axum::body::Body::from(
                serde_json::json!({"expected_generation": expected_generation}).to_string(),
            ))
            .unwrap()
    };
    for (operation, expected) in [
        (LifecycleOperation::Purge, generation + 1),
        (LifecycleOperation::Stop, generation),
    ] {
        storage.update(|rows, _| {
            rows[0].lifecycle_reservation.as_mut().unwrap().op = operation;
            Ok(())
        })?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.clone().oneshot(request(expected)),
        )
        .await??;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(!response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        let stored = storage.load()?;
        assert!(stored
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .lifecycle_reservation_is_owned(operation, generation));
    }
    let peer = Instance::new("peer commit", "/tmp/abandon-purge-peer");
    let peer_id = peer.id.clone();
    storage.update(|rows, _| {
        rows[0].lifecycle_reservation.as_mut().unwrap().op = LifecycleOperation::Purge;
        rows.push(peer);
        Ok(())
    })?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.oneshot(request(generation)),
    )
    .await??;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let reflected = state.runtime.snapshot(&state).await?;
    assert_eq!(
        response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
        reflected.value.cursor.epoch.as_str()
    );
    assert_eq!(
        response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
            .to_str()?
            .parse::<u64>()?,
        reflected.value.cursor.revision
    );
    assert!(reflected.value.cursor.revision > before.value.cursor.revision);
    assert!(!reflected
        .value
        .contents
        .sessions
        .iter()
        .any(|row| row.id == id));
    assert!(reflected
        .value
        .contents
        .sessions
        .iter()
        .any(|row| row.id == peer_id));
    assert!(!storage.load()?.iter().any(|row| row.id == id));
    drop(instance_guard);
    drop(submission);
    drop(lifecycle);
    state.runtime.work.shutdown.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.runtime.work.drain(),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(result) = background.join_next().await {
            result.unwrap();
        }
    })
    .await?;
    assert!(!storage.load()?.iter().any(|row| row.id == id));
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn abandon_purge_preserves_read_only_and_cityhall_policy() -> anyhow::Result<()> {
    use crate::session::LifecycleOperation;
    use tower::ServiceExt;

    for (read_only, cityhall, structured, expected) in [
        (true, false, false, StatusCode::FORBIDDEN),
        (false, true, false, StatusCode::FORBIDDEN),
        (false, true, true, StatusCode::ACCEPTED),
    ] {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("policy purge", "/tmp/abandon-policy");
        row.status = Status::Stopped;
        if structured {
            row.view = crate::session::View::Structured;
        }
        let generation = row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Purge,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        let id = row.id.clone();
        let storage = Storage::new_unwatched("abandon-policy")?;
        storage.update(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        let loaded =
            crate::server::reload::load_all_profiles(&crate::file_watch::FileWatchService::noop())?;
        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            loaded.instances,
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| {
                state.read_only = read_only;
                state.cityhall_mode = cityhall;
            },
        );
        *state.canonical_metadata.write().await = loaded.metadata;
        let snapshot = state.runtime.publish(&state).await?;
        let app = crate::server::test_support::build_router_for_test(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{id}/purge/abandon"))
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .header(
                        crate::daemon::RUNTIME_EPOCH_HEADER,
                        snapshot.value.cursor.epoch.as_str(),
                    )
                    .extension(axum::extract::ConnectInfo(
                        crate::server::peer::ConnectionPeer::UnixOwner {
                            uid: nix::unistd::geteuid().as_raw(),
                        },
                    ))
                    .body(axum::body::Body::from(
                        serde_json::json!({"expected_generation": generation}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), expected);
        assert_eq!(
            storage.load()?.iter().any(|row| row.id == id),
            expected == StatusCode::FORBIDDEN
        );
        if expected == StatusCode::FORBIDDEN {
            assert!(storage
                .load()?
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
            assert!(!response
                .headers()
                .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        }
        state.runtime.work.shutdown.cancel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.runtime.work.drain(),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn abandon_purge_inherits_a_namespace_lease_published_after_it_started() -> anyhow::Result<()>
{
    let state = crate::server::test_support::build_test_app_state(Vec::new());
    let held = state.profile_namespace.write().await;
    let mut purge = Box::pin(
        state
            .runtime
            .purge_namespace_lease(&state.profile_namespace),
    );
    assert!(futures_util::poll!(purge.as_mut()).is_pending());
    let mut writer = Box::pin(state.profile_namespace.write());
    assert!(futures_util::poll!(writer.as_mut()).is_pending());
    let mut abandon = Box::pin(
        state
            .runtime
            .abandon_namespace_lease(&state.profile_namespace),
    );
    assert!(futures_util::poll!(abandon.as_mut()).is_pending());
    drop(held);
    let purge = purge.await;
    let abandon = tokio::time::timeout(std::time::Duration::from_secs(5), abandon).await?;
    drop(purge);
    assert!(
        futures_util::poll!(writer.as_mut()).is_pending(),
        "abandon cleanup must retain namespace exclusion after the original purge ends"
    );
    drop(abandon);
    drop(tokio::time::timeout(std::time::Duration::from_secs(5), writer).await?);
    Ok(())
}

#[test]
fn notification_patch_distinguishes_omitted_from_null() {
    let patch: UpdateNotificationsBody =
        serde_json::from_str(r#"{"notify_on_idle":null,"notify_on_error":false}"#).unwrap();
    assert!(matches!(patch.notify_on_waiting, Tristate::Unset));
    assert!(matches!(patch.notify_on_idle, Tristate::Clear));
    assert!(matches!(patch.notify_on_error, Tristate::Set(false)));
    assert_eq!(
        serde_json::to_value(patch).unwrap(),
        serde_json::json!({"notify_on_idle": null, "notify_on_error": false}),
    );
}

fn build_rename_test_state(
    persisted: Vec<Instance>,
    cached: Vec<Instance>,
) -> (Storage, std::sync::Arc<crate::server::AppState>) {
    let storage = Storage::new_unwatched("default").unwrap();
    storage
        .update(|instances, _groups| {
            *instances = persisted;
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(cached);
    (storage, state)
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_rejects_duplicate_and_preserves_newer_cache() {
    use axum::body::to_bytes;

    let _guard = crate::session::test_support::isolate_app_dir();
    let mut existing = Instance::new("main branch", "/tmp/repo/");
    existing.source_profile = "default".to_string();
    let mut target = Instance::new("throwaway", "/tmp/repo");
    target.source_profile = "default".to_string();
    let target_id = target.id.clone();
    let mut stale_existing = existing.clone();
    stale_existing.title = "previous title".to_string();
    let mut stale_target = target.clone();
    stale_target.project_path = "/tmp/stale".to_string();
    let (storage, state) =
        build_rename_test_state(vec![existing, target], vec![stale_existing, stale_target]);

    let response = rename_session(
        State(state.clone()),
        Path(target_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), 2048).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("duplicate_session"));
    assert_eq!(
        state
            .instances
            .read()
            .await
            .iter()
            .find(|instance| instance.id == target_id)
            .unwrap()
            .title,
        "throwaway"
    );

    storage
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|instance| instance.id != target_id)
                .unwrap()
                .title = "other".to_string();
            Ok(())
        })
        .unwrap();
    // A user action can advance the live cache while the disk snapshot the
    // rename will persist still has the older row. Publication must patch
    // only rename-owned identity fields, not replace this favorite.
    state
        .instances
        .write()
        .await
        .iter_mut()
        .find(|instance| instance.id == target_id)
        .unwrap()
        .favorite();
    let response = rename_session(
        State(state.clone()),
        Path(target_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let instances = state.instances.read().await;
    let target = instances
        .iter()
        .find(|instance| instance.id == target_id)
        .unwrap();
    assert_eq!(target.title, "main branch");
    assert_eq!(target.project_path, "/tmp/repo");
    assert_eq!(target.source_profile, "default");
    assert!(
        target.is_favorited(),
        "newer cached user action must survive rename publication"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_rejects_tied_drifted_path_collision() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let mut existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
    existing.source_profile = "default".to_string();
    let mut drifted = Instance::new("main branch", "/tmp/worktrees/drifted");
    drifted.source_profile = "default".to_string();
    drifted.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "main-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let drifted_id = drifted.id.clone();
    let (_storage, state) = build_rename_test_state(
        vec![existing.clone(), drifted.clone()],
        vec![existing, drifted],
    );

    let response = rename_session(
        State(state),
        Path(drifted_id),
        Ok(Json(RenameSessionBody {
            title: "main branch".to_string(),
            rename_branch: false,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
#[serial_test::serial]
async fn concurrent_renames_commit_only_one_same_identity_pair() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut first = Instance::new("first", "/tmp/shared");
    first.source_profile = "default".to_string();
    let mut second = Instance::new("second", "/tmp/shared/");
    second.source_profile = "default".to_string();
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let storage = Storage::new_unwatched("default").unwrap();
    storage
        .update(|instances, _groups| {
            *instances = vec![first.clone(), second.clone()];
            Ok(())
        })
        .unwrap();
    let state = crate::server::test_support::build_test_app_state(vec![first, second]);

    let first_rename = rename_session(
        State(state.clone()),
        Path(first_id),
        Ok(Json(RenameSessionBody {
            title: "shared title".to_string(),
            rename_branch: false,
        })),
    );
    let second_rename = rename_session(
        State(state.clone()),
        Path(second_id),
        Ok(Json(RenameSessionBody {
            title: "shared title".to_string(),
            rename_branch: false,
        })),
    );
    let (first_response, second_response) = tokio::join!(first_rename, second_rename);
    let statuses = [
        first_response.into_response().status(),
        second_response.into_response().status(),
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );
    assert_eq!(
        storage
            .load()
            .unwrap()
            .iter()
            .filter(|instance| {
                instance.title == "shared title"
                    && instance.project_path.trim_end_matches('/') == "/tmp/shared"
            })
            .count(),
        1
    );
}

// #2536: the workspace-delete order must tear down record-only siblings
// first and the shared-worktree owner last, so a sibling failure can never
// orphan a session against an already-removed worktree.
mod workspace_deletion {
    use super::*;

    fn body() -> DeleteWorkspaceBody {
        DeleteWorkspaceBody {
            session_ids: vec![],
            delete_worktree: true,
            delete_branch: true,
            delete_sandbox: true,
            force_delete: false,
            keep_scratch: false,
        }
    }

    #[test]
    fn owner_is_last_and_siblings_are_record_only() {
        let ids = vec!["owner".to_string(), "sib1".to_string(), "sib2".to_string()];
        let plan = order_workspace_deletion(&ids, &body());

        let order: Vec<&str> = plan.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            order,
            vec!["sib1", "sib2", "owner"],
            "siblings must precede the owner so the worktree owner is torn down last"
        );

        // Siblings never touch the shared worktree/branch.
        for (id, b) in &plan[..2] {
            assert!(
                !b.delete_worktree,
                "sibling {id} must not remove the worktree"
            );
            assert!(!b.delete_branch, "sibling {id} must not delete the branch");
            assert!(
                b.delete_sandbox,
                "sibling {id} still tears down its own sandbox"
            );
        }
        // The owner (last) carries the caller's worktree/branch flags.
        let (owner_id, owner_body) = plan.last().unwrap();
        assert_eq!(owner_id, "owner");
        assert!(owner_body.delete_worktree);
        assert!(owner_body.delete_branch);
    }

    #[test]
    fn single_session_is_owner_only_with_full_flags() {
        let ids = vec!["solo".to_string()];
        let plan = order_workspace_deletion(&ids, &body());
        assert_eq!(plan.len(), 1);
        let (id, b) = &plan[0];
        assert_eq!(id, "solo");
        assert!(
            b.delete_worktree,
            "the only session owns the worktree cleanup"
        );
        assert!(b.delete_branch);
    }

    #[test]
    fn empty_input_is_empty_plan() {
        assert!(order_workspace_deletion(&[], &body()).is_empty());
    }

    #[test]
    fn worktree_flags_off_stay_off_for_owner() {
        let mut b = body();
        b.delete_worktree = false;
        b.delete_branch = false;
        let ids = vec!["owner".to_string(), "sib".to_string()];
        let plan = order_workspace_deletion(&ids, &b);
        let (_, owner_body) = plan.last().unwrap();
        assert!(!owner_body.delete_worktree);
        assert!(!owner_body.delete_branch);
    }

    #[test]
    fn dedupe_drops_repeats_preserving_first_seen_order() {
        let ids = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
            "b".to_string(),
        ];
        assert_eq!(dedupe_session_ids(&ids), vec!["a", "b", "c"]);
    }

    #[test]
    fn duplicate_owner_still_removes_the_worktree() {
        // #2536 review: ["owner", "owner"] must not delete the owner with
        // sibling (record-only) flags and then skip the repeat. After
        // dedupe the single owner entry keeps the real worktree flags.
        let ids = dedupe_session_ids(&["owner".to_string(), "owner".to_string()]);
        assert_eq!(ids, vec!["owner"]);
        let plan = order_workspace_deletion(&ids, &body());
        assert_eq!(plan.len(), 1);
        let (id, b) = &plan[0];
        assert_eq!(id, "owner");
        assert!(
            b.delete_worktree,
            "the deduped owner must still own the worktree cleanup"
        );
        assert!(b.delete_branch);
    }
}

// CityHall create-time capability gate (#7): create_session rejects a
// non-ACP agent up front instead of downgrading to a hidden terminal view.
mod cityhall_capability {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use serial_test::serial;

    #[test]
    fn builtin_agent_is_acp_capable() {
        // Built-in ACP agents resolve via the registry without reading
        // config, so the gate accepts them regardless of the project path.
        assert!(agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "claude",
            None,
        ));
    }

    #[test]
    #[serial]
    fn an_explicit_agent_name_keys_the_custom_acp_cmd_lookup() {
        // An explicit `agent_name` can point at a different `agent_acp_cmd`
        // entry than `tool`, and `resolve_agent_spec` resolves the custom map
        // by that same name. Keying this lookup off `tool` reported
        // not-capable for an agent that spawns fine, which skipped the
        // up-front 403 in favor of a late refusal at spawn.
        let _tmp = isolate_app_dir();
        crate::session::config::update_config(|c| {
            c.session
                .agent_acp_cmd
                .insert("acp-helper".into(), "acp-helper --acp".into());
        })
        .unwrap();
        let path = std::path::Path::new("/nonexistent");
        assert!(agent_is_acp_capable(
            "default",
            path,
            "plain-tool",
            Some("acp-helper"),
        ));
        // Without the override there is nothing to resolve to, so the same
        // tool stays not-capable.
        assert!(!agent_is_acp_capable("default", path, "plain-tool", None));
    }

    #[test]
    #[serial]
    fn unknown_tool_is_not_acp_capable() {
        let _tmp = isolate_app_dir();
        assert!(!agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "definitely-not-a-real-tool",
            None,
        ));
    }

    /// Why `acp_enable` gates on this predicate and not on
    /// `pick_agent_for_tool`: the default-agent fallback always names a
    /// registry entry, so a post-fallback registry lookup reports every
    /// tool capable and would switch a terminal-only session into a
    /// structured one running some other agent.
    #[test]
    #[serial]
    fn the_default_agent_fallback_is_not_a_capability_signal() {
        let _tmp = isolate_app_dir();
        let fallback = crate::session::config::DEFAULT_ACP_AGENT;
        assert!(
            crate::acp::AgentRegistry::with_defaults()
                .get(fallback)
                .is_some(),
            "the fallback must be spawnable, which is what makes it useless as a gate"
        );
        assert!(!agent_is_acp_capable(
            "default",
            std::path::Path::new("/nonexistent"),
            "plain-tool",
            None,
        ));
    }
}

// #2587: the artifact route serves only canonicalized files confined to
// the session's artifact dir, sets nosniff, and never serves HTML inline.
mod artifact_route {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use axum::body::to_bytes;
    use axum::extract::Path as AxumPath;
    use axum::http::header;
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn serves_image_with_nosniff() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
        std::fs::write(dir.join("shot.png"), b"\x89PNG\r\n").unwrap();
        let resp = serve_session_artifact(AxumPath((id, "shot.png".to_string())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
    }

    #[tokio::test]
    #[serial]
    async fn rejects_traversal_with_empty_body() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        crate::session::artifacts::session_artifact_dir(&id).unwrap();
        let resp = serve_session_artifact(AxumPath((id, "../../../../etc/hosts".to_string())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(body.is_empty(), "unexpected body: {body:?}");
    }

    #[tokio::test]
    #[serial]
    async fn serves_svg_as_attachment() {
        // #2587: SVG can execute script as a top-level document, and the
        // frontend opens artifacts via a same-origin blob URL, so SVG must
        // download rather than render inline.
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
        std::fs::write(
            dir.join("d.svg"),
            b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
        )
        .unwrap();
        let resp = serve_session_artifact(AxumPath((id, "d.svg".to_string())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            resp.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment"
        );
    }

    #[tokio::test]
    #[serial]
    async fn serves_html_as_attachment() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
        std::fs::write(dir.join("status.html"), b"<h1>hi</h1>").unwrap();
        let resp = serve_session_artifact(AxumPath((id, "status.html".to_string())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            resp.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment"
        );
    }
}

fn make_test_instance() -> Instance {
    let mut inst = Instance::new("test-session", "/tmp/test-project");
    inst.tool = "claude".to_string();
    inst.status = Status::Running;
    inst.group_path = "work/projects".to_string();
    inst
}

#[tokio::test]
#[serial_test::serial]
async fn smart_rename_rejects_only_trusted_command_overrides() {
    async fn rejection_code(repo: &std::path::Path) -> String {
        let mut instance = Instance::new("Vikings", repo.to_str().unwrap());
        instance.tool = "claude".into();
        instance.source_profile = "default".into();
        instance.view = crate::session::View::Structured;
        let id = instance.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![instance]);
        let response = force_smart_rename(axum::extract::State(state), axum::extract::Path(id))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        body["error"].as_str().unwrap().to_owned()
    }
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::session::test_support::isolate_app_dir_at(home.path());
    let repo = tempfile::tempdir().unwrap();
    let config = "[session.agent_command_override]\nclaude = \"wrapper-3058\"\n";
    let repo_config = repo.path().join(".agent-of-empires");
    std::fs::create_dir_all(&repo_config).unwrap();
    std::fs::write(repo_config.join("config.toml"), config).unwrap();
    assert_eq!(rejection_code(repo.path()).await, "no_prompt");
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("config.toml"), config).unwrap();
    assert_eq!(rejection_code(repo.path()).await, "command_overridden");
}

// Manual regeneration bypasses the current-title gate but still requires a prompt.
#[tokio::test]
#[serial_test::serial]
async fn force_smart_rename_ignores_a_custom_name() {
    use axum::body::to_bytes;

    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());

    let mut inst = Instance::new("Vikings", "/tmp/custom-name-regen");
    inst.title = "Fix login bug".to_string();
    inst.tool = "claude".to_string();
    inst.source_profile = "default".to_string();
    inst.view = crate::session::View::Structured;
    let id = inst.id.clone();

    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let resp = force_smart_rename(axum::extract::State(state), axum::extract::Path(id))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let msg = String::from_utf8_lossy(&body);
    assert!(
        !msg.contains("custom name"),
        "manual regenerate must not refuse a custom-named session; got: {msg}"
    );
    assert!(
        msg.contains("No prompt to name this session from yet"),
        "must fall through to the next gate instead; got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn list_sessions_shares_config_resolution_across_overlays() {
    use std::sync::atomic::Ordering;

    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());

    let mk = |profile: &str, project_path: &str| {
        let mut inst = Instance::new("test-session", project_path);
        inst.tool = "custom-tool-2603".to_string();
        inst.source_profile = profile.to_string();
        inst
    };
    let a = mk("default", "/tmp/repo-a-2603");
    let a2 = mk("default", "/tmp/repo-a-2603");
    let b = mk("default", "/tmp/repo-b-2603");

    // The counter lives on this state, so no concurrent test can bump it.
    let state = crate::server::test_support::build_test_app_state(vec![a, a2, b]);

    let _envelope = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    let misses = state.list_sessions_resolver_misses.load(Ordering::Relaxed);

    assert_eq!(
        misses, 2,
        "shared cache must resolve once per unique (profile, project_path) across both overlays; got {misses}",
    );
}

#[tokio::test]
#[serial_test::serial]
async fn list_sessions_state_filter() {
    let _guard = crate::session::test_support::isolate_app_dir();
    let mut live = Instance::new("live", "/tmp/scope-live");
    live.id = "scope-live".to_string();
    let mut trashed = Instance::new("trashed", "/tmp/scope-trashed");
    trashed.id = "scope-trashed".to_string();
    trashed.trash();
    let mut archived = Instance::new("archived", "/tmp/scope-archived");
    archived.id = "scope-archived".to_string();
    archived.archived_at = Some(chrono::Utc::now());

    let state = crate::server::test_support::build_test_app_state(vec![
        live.clone(),
        trashed.clone(),
        archived.clone(),
    ]);

    async fn ids(response: impl IntoResponse) -> Vec<String> {
        let response = response.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body).unwrap();
        envelope.sessions.into_iter().map(|s| s.id).collect()
    }

    let all = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await;
    assert_eq!(
        ids(all).await,
        ["scope-live", "scope-trashed", "scope-archived"]
    );

    let live_only = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::Live),
        }),
    )
    .await;
    assert_eq!(ids(live_only).await, ["scope-live"]);

    let trashed_only = list_sessions(
        axum::extract::State(state.clone()),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::Trashed),
        }),
    )
    .await;
    assert_eq!(ids(trashed_only).await, ["scope-trashed"]);

    let explicit_all = list_sessions(
        axum::extract::State(state),
        axum::extract::Query(ListSessionsQuery {
            state: Some(crate::session::SessionScope::All),
        }),
    )
    .await;
    assert_eq!(
        ids(explicit_all).await,
        ["scope-live", "scope-trashed", "scope-archived"]
    );
}

#[tokio::test]
async fn wait_until_left_starting_returns_immediately_if_already_left() {
    let mut inst = Instance::new("already-running", "/tmp/wait-a");
    inst.id = "wait-already-left".to_string();
    inst.status = Status::Running;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let result = wait_until_left_starting(
        &state,
        "wait-already-left",
        std::time::Duration::from_secs(5),
    );
    tokio::pin!(result);
    let result = futures_util::poll!(&mut result).map(|value| value.map(|i| i.status));
    assert_eq!(result, std::task::Poll::Ready(Some(Status::Running)));
}

#[tokio::test(start_paused = true)]
async fn wait_until_left_starting_resolves_on_broadcast() {
    let mut inst = Instance::new("starting", "/tmp/wait-b");
    inst.id = "wait-resolves".to_string();
    inst.status = Status::Starting;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let waiter =
        wait_until_left_starting(&state, "wait-resolves", std::time::Duration::from_secs(5));
    tokio::pin!(waiter);
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    {
        let mut instances = state.instances.write().await;
        instances
            .iter_mut()
            .find(|i| i.id == "wait-resolves")
            .unwrap()
            .status = Status::Waiting;
    }
    state
        .status_tx
        .send(crate::server::push::StatusChange {
            instance_id: "wait-resolves".to_string(),
            instance_title: "starting".to_string(),
            old: Status::Starting,
            new: Status::Waiting,
            at: chrono::Utc::now(),
        })
        .expect("the waiter must be subscribed before the transition");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("the broadcast must resolve before the fallback timeout");
    assert_eq!(result.map(|i| i.status), Some(Status::Waiting));
}

#[tokio::test(start_paused = true)]
async fn wait_until_left_starting_times_out_with_current_status() {
    let mut inst = Instance::new("stuck", "/tmp/wait-c");
    inst.id = "wait-timeout".to_string();
    inst.status = Status::Starting;
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let timeout = std::time::Duration::from_millis(150);
    let waiter = wait_until_left_starting(&state, "wait-timeout", timeout);
    tokio::pin!(waiter);
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    state.instances.write().await[0].status = Status::Waiting;
    assert!(futures_util::poll!(waiter.as_mut()).is_pending());
    tokio::time::advance(timeout).await;
    assert_eq!(waiter.await.map(|i| i.status), Some(Status::Waiting));
}

#[tokio::test]
async fn wait_until_left_starting_returns_none_if_instance_vanished() {
    let state = crate::server::test_support::build_test_app_state(vec![]);
    let result = wait_until_left_starting(
        &state,
        "never-existed",
        std::time::Duration::from_millis(100),
    )
    .await;
    assert!(result.is_none());
}

#[test]
fn find_by_idempotency_key_matches_trashed_but_not_missing() {
    let mut with_key = Instance::new("has-key", "/tmp/idem-a");
    with_key.id = "idem-has-key".to_string();
    with_key.idempotency_key = Some("retry-token-1".to_string());
    with_key.trash(); // soft-deleted; a retry must still find it.

    let mut without_key = Instance::new("no-key", "/tmp/idem-b");
    without_key.id = "idem-no-key".to_string();

    let instances = vec![with_key, without_key];

    let found = find_by_idempotency_key(&instances, "retry-token-1");
    assert_eq!(found.map(|i| i.id.as_str()), Some("idem-has-key"));

    assert!(find_by_idempotency_key(&instances, "never-seen").is_none());
}

#[test]
fn fork_from_builds_terminal_seed_for_claude() {
    let parent_binding = crate::session::ConversationBinding {
        session_id: "parent-uuid".into(),
        execution: Some(crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec!["/tmp/claude-store".into()],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: "/tmp".into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
        provenance: crate::session::ConversationProvenance::Observed,
        transcript_path: None,
    };
    let mut parent = crate::session::Instance::new("parent", "/tmp");
    parent.agent_session_id = Some(parent_binding.session_id.clone());
    parent.agent_session_binding = Some(parent_binding.clone());

    let seed = resolve_create_fork_seed("parent-uuid", false, &[parent])
        .expect("claude terminal fork allowed");
    match seed {
        crate::session::ForkSeed::Terminal {
            parent,
            child_session_id,
        } => {
            assert_eq!(*parent, parent_binding);
            assert!(crate::session::capture::is_valid_session_id(
                &child_session_id
            ));
        }
        _ => panic!("expected Terminal seed"),
    }
}

#[test]
fn fork_from_builds_structured_seed_when_view_is_structured() {
    let seed = resolve_create_fork_seed("parent-acp-id", true, &[])
        .expect("structured fork seed is always allowed at create time");
    assert_eq!(
        seed,
        crate::session::ForkSeed::Structured {
            parent_acp_session_id: "parent-acp-id".into(),
        }
    );
}

#[test]
fn fork_from_rejects_ambiguous_parent_session_id() {
    let binding = |cwd: &str| crate::session::ConversationBinding {
        session_id: "shared-parent-id".into(),
        execution: Some(crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec!["/tmp/claude-store".into()],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: cwd.into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
        provenance: crate::session::ConversationProvenance::Observed,
        transcript_path: None,
    };
    let instance = |binding: crate::session::ConversationBinding| {
        let mut instance = crate::session::Instance::new("parent", "/tmp");
        instance.agent_session_id = Some(binding.session_id.clone());
        instance.agent_session_binding = Some(binding);
        instance
    };

    assert!(matches!(
        resolve_create_fork_seed(
            "shared-parent-id",
            false,
            &[instance(binding("/tmp/one")), instance(binding("/tmp/two"))],
        ),
        Err(crate::session::ForkDenied::NoParentSession)
    ));
}

fn create_body_from_json(value: serde_json::Value) -> CreateSessionBody {
    serde_json::from_value(value).expect("valid CreateSessionBody")
}

#[test]
fn worktree_enabled_true_opts_in_without_branch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_enabled": true,
    }));

    assert!(create_body_uses_worktree(&body));
    assert!(body.worktree_branch.is_none());
}

#[test]
fn worktree_branch_preserves_legacy_worktree_opt_in() {
    let explicit = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_branch": "feat/api",
    }));
    assert!(create_body_uses_worktree(&explicit));

    let empty = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
        "worktree_branch": "",
    }));
    assert!(create_body_uses_worktree(&empty));
}

#[test]
fn worktree_defaults_off_without_flag_or_branch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "/tmp/p",
        "tool": "claude",
    }));

    assert!(!create_body_uses_worktree(&body));
}

#[test]
fn worktree_enabled_conflicts_with_scratch() {
    let body = create_body_from_json(serde_json::json!({
        "path": "",
        "tool": "claude",
        "scratch": true,
        "worktree_enabled": true,
    }));

    assert!(create_body_combines_scratch_and_worktree(&body));
}

#[test]
fn create_sources_are_mutually_exclusive() {
    for (import, raw_fork, row_fork, conflicts) in [
        (Some("import"), Some("parent"), None, true),
        (Some("import"), None, Some("row"), true),
        (None, Some("parent"), Some("row"), true),
        (None, None, Some("row"), false),
        (Some("import"), Some(" "), Some(" "), false),
    ] {
        let body = create_body_from_json(serde_json::json!({
            "path": "/tmp/p", "tool": "claude", "import_acp_session_id": import,
            "fork_from": raw_fork, "fork_session_id": row_fork,
        }));
        assert_eq!(create_body_has_conflicting_sources(&body), conflicts);
    }
}

#[test]
fn acp_can_fork_tracks_acp_capable_and_fork_strategy() {
    // claude is ACP-capable AND declares a real fork strategy, so the web
    // gets a forkable signal.
    let mut claude = make_test_instance();
    claude.tool = "claude".to_string();
    assert!(SessionResponse::from_instance(&claude, false).acp_can_fork);

    // aoe-agent is ACP-capable (it is in the ACP registry) but declares no
    // fork strategy, so it is NOT forkable. Gating the web Fork action on
    // acp_session_id alone would offer a dead-end button for it; this is the
    // signal that suppresses that.
    let mut aoe_agent = make_test_instance();
    aoe_agent.tool = "aoe-agent".to_string();
    assert!(!SessionResponse::from_instance(&aoe_agent, false).acp_can_fork);

    // codex has a real terminal fork strategy but its ACP adapter is not
    // verified to implement `session/fork`, so the web signal must stay
    // false rather than offer a fork the live handshake would refuse.
    let mut codex = make_test_instance();
    codex.tool = "codex".to_string();
    assert!(!SessionResponse::from_instance(&codex, false).acp_can_fork);

    // A non-ACP agent is neither ACP-capable nor fork-capable.
    let mut other = make_test_instance();
    other.tool = "definitely-not-an-acp-agent".to_string();
    assert!(!SessionResponse::from_instance(&other, false).acp_can_fork);
}

// Regression for #2363: a multi-repo workspace session carries
// `workspace_info` and no `worktree_info`. The DTO must report
// `has_cleanable_worktree: true` so the web delete dialog shows the
// "Delete worktree" checkbox, while keeping `has_managed_worktree: false`
// so worktree-only actions (sidebar "Edit workdir name", tie overlay) stay
// hidden for workspace sessions.
#[test]
fn from_instance_reports_managed_worktree_for_workspace_session() {
    let mut inst = make_test_instance();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/abc".to_string(),
        workspace_dir: "/tmp/ws".to_string(),
        repos: vec![crate::session::WorkspaceRepo {
            name: "repo-a".to_string(),
            source_path: "/tmp/src/repo-a".to_string(),
            branch: "feature/abc".to_string(),
            worktree_path: "/tmp/ws/repo-a".to_string(),
            main_repo_path: "/tmp/src/repo-a".to_string(),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        }],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let resp = SessionResponse::from_instance(&inst, false);
    assert!(
        resp.has_cleanable_worktree,
        "workspace session must report a cleanable worktree so the delete checkbox shows"
    );
    assert!(
        !resp.has_managed_worktree,
        "workspace session must NOT report a single-repo managed worktree (keeps Edit-workdir hidden)"
    );
}

#[test]
#[serial_test::serial(hook_base)]
fn from_instance_surfaces_hook_urgent_flag() {
    // #1640: the web Attention sort needs `Instance::is_urgent()` on the
    // wire. Write the hook-side attention.json the agent would emit and
    // confirm it round-trips onto the response, then confirm a session
    // with no hook file reports urgent: false.
    let (_g, _, _tmp_base) = crate::hooks::test_support::BaseGuard::ready();
    let inst = make_test_instance();
    let dir = crate::hooks::ensure_instance_dir_path(&inst.id)
        .expect("guard must create instance subdir");
    std::fs::write(
        dir.join("attention.json"),
        r#"{"urgent":true,"urgent_reason":"needs input"}"#,
    )
    .unwrap();

    let urgent_resp = SessionResponse::from_instance(&inst, false);
    assert!(urgent_resp.urgent, "hook-flagged session must be urgent");

    crate::hooks::cleanup_hook_status_dir(&inst.id);
    let plain_resp = SessionResponse::from_instance(&inst, false);
    assert!(
        !plain_resp.urgent,
        "session with no hook file must not be urgent"
    );
}

#[tokio::test]
async fn create_hook_failure_details_are_local_owner_only() {
    let error = anyhow::Error::new(CreateHookFailed::new(
        anyhow::anyhow!("command exited with status 1"),
        Some("Defined in /tmp/project/.agent-of-empires.yml"),
    ));
    assert!(local_create_hook_error_response(&error, false).is_none());

    let response = local_create_hook_error_response(&error, true).expect("local response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(crate::daemon::ERROR_CODE_HEADER),
        Some(&axum::http::HeaderValue::from_static("create_hook_failed"))
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        body,
        "on_create hook failed: command exited with status 1\nDefined in /tmp/project/.agent-of-empires.yml"
    );
}

#[test]
fn public_create_session_error_forwards_whitelisted_git_errors() {
    let dup: anyhow::Error =
        GitError::WorktreeAlreadyExists(std::path::PathBuf::from("/tmp/repo-worktrees/foo")).into();
    assert_eq!(
        public_create_session_error(&dup),
        "Worktree already exists at /tmp/repo-worktrees/foo"
    );

    let in_use: anyhow::Error = GitError::BranchAlreadyCheckedOut("feature/foo".to_string()).into();
    assert_eq!(
        public_create_session_error(&in_use),
        "Branch 'feature/foo' is already in use by another worktree"
    );

    // Whitelisted variants survive an anyhow::Context wrapper too.
    let wrapped = anyhow::Error::from(GitError::BranchNotFound("nope".to_string()))
        .context("while creating worktree");
    assert_eq!(
        public_create_session_error(&wrapped),
        "Branch 'nope' not found"
    );
}

#[test]
fn public_create_session_error_hides_unsafe_messages() {
    // Raw git stderr (even already-sanitized) must not reach the client.
    let cmd: anyhow::Error = GitError::WorktreeCommandFailed(
        "fatal: unable to access 'https://<redacted>@host/repo.git'".to_string(),
    )
    .into();
    assert_eq!(
        public_create_session_error(&cmd),
        "Failed to create session"
    );

    let clone: anyhow::Error =
        GitError::CloneFailed("https://alice:supersecret@host/repo.git".to_string()).into();
    let msg = public_create_session_error(&clone);
    assert_eq!(msg, "Failed to create session");
    assert!(!msg.contains("supersecret"));

    // A non-GitError anyhow also stays generic.
    let other = anyhow::anyhow!("something internal at /home/user/.config/secret");
    assert_eq!(
        public_create_session_error(&other),
        "Failed to create session"
    );
}

#[test]
fn session_response_from_instance() {
    let inst = make_test_instance();
    let resp = SessionResponse::from_instance(&inst, false);

    assert_eq!(resp.id, inst.id);
    assert_eq!(resp.title, "test-session");
    assert_eq!(resp.project_path, "/tmp/test-project");
    assert_eq!(resp.tool, "claude");
    assert_eq!(resp.status, "Running");
    assert_eq!(resp.group_path, "work/projects");
    assert!(!resp.is_sandboxed);
    assert!(!resp.has_terminal);
}

#[test]
fn session_response_status_variants() {
    let mut inst = make_test_instance();

    for (status, expected) in [
        (Status::Running, "Running"),
        (Status::Waiting, "Waiting"),
        (Status::Error, "Error"),
        (Status::Stopped, "Stopped"),
        (Status::Idle, "Idle"),
        (Status::Starting, "Starting"),
    ] {
        inst.status = status;
        assert_eq!(
            SessionResponse::from_instance(&inst, false).status,
            expected
        );
    }
}

#[test]
fn session_response_dormant_reflects_shown_dormant() {
    let mut inst = make_test_instance();

    // Live idle: not dormant.
    inst.status = Status::Idle;
    assert!(!SessionResponse::from_instance(&inst, false).dormant);

    // Idle-reaped (marker set, status left Idle): dormant.
    inst.mark_idle_dormant();
    assert!(SessionResponse::from_instance(&inst, false).dormant);

    // Deliberate stop (marker set AND Stopped): reports NOT dormant so the
    // dashboard keeps the neutral Stopped dot. See #2250.
    inst.status = Status::Stopped;
    assert!(!SessionResponse::from_instance(&inst, false).dormant);
}

#[test]
fn session_response_branch_from_worktree() {
    let mut inst = make_test_instance();
    assert!(SessionResponse::from_instance(&inst, false)
        .branch
        .is_none());

    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/test".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    assert_eq!(
        SessionResponse::from_instance(&inst, false)
            .branch
            .as_deref(),
        Some("feature/test")
    );
}

#[test]
fn session_response_surfaces_base_branch_override() {
    let mut inst = make_test_instance();
    // Default: no override -> field omitted from JSON.
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(
        json.get("base_branch_override").is_none(),
        "base_branch_override should be omitted when None, got: {json}"
    );

    inst.base_branch_override = Some("upstream/main".to_string());
    let resp = SessionResponse::from_instance(&inst, false);
    assert_eq!(resp.base_branch_override.as_deref(), Some("upstream/main"));
}

#[test]
fn resolve_diff_base_prefers_override_then_worktree_then_config_then_auto() {
    let tmp = tempfile::tempdir().unwrap();
    // Override wins over everything.
    assert_eq!(
        resolve_diff_base(Some("release-1.2"), None, Some("develop"), tmp.path()),
        "release-1.2"
    );
    // Worktree base wins after override; whitespace override falls through.
    assert_eq!(
        resolve_diff_base(
            Some("   "),
            Some("worktree-base"),
            Some("develop"),
            tmp.path()
        ),
        "worktree-base"
    );
    // Config wins when no override and no worktree base.
    assert_eq!(
        resolve_diff_base(None, None, Some("develop"), tmp.path()),
        "develop"
    );
    // Auto-detect when nothing is set. The tmp dir is not a repo so
    // `get_default_base_ref` returns Err -> "main" fallback.
    assert_eq!(resolve_diff_base(None, None, None, tmp.path()), "main");
}

/// Each workspace member carries its own override and recorded base, and
/// the session-level `base_branch_override` does not leak into any of
/// them. That leak is what made a multi-repo diff compare every repo
/// against one ref. See #3329.
#[test]
fn diff_repos_of_scopes_bases_per_workspace_repo() {
    fn repo(name: &str, base: Option<&str>, over: Option<&str>) -> crate::session::WorkspaceRepo {
        crate::session::WorkspaceRepo {
            name: name.to_string(),
            source_path: format!("/src/{name}"),
            branch: "feature/x".to_string(),
            worktree_path: format!("/ws/{name}"),
            main_repo_path: format!("/src/{name}"),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: base.map(str::to_string),
            base_branch_override: over.map(str::to_string),
        }
    }

    let mut inst = make_test_instance();
    inst.base_branch_override = Some("session-wide".to_string());
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/x".to_string(),
        workspace_dir: "/ws".to_string(),
        repos: vec![
            repo("api", Some("develop"), None),
            repo("web", Some("develop"), Some("epic/checkout")),
            repo("infra", None, None),
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let repos = diff_repos_of(&inst);
    let seen: Vec<_> = repos
        .iter()
        .map(|r| {
            (
                r.name.as_deref(),
                r.base_override.as_deref(),
                r.recorded_base.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (Some("api"), None, Some("develop")),
            (Some("web"), Some("epic/checkout"), Some("develop")),
            (Some("infra"), None, None),
        ],
        "workspace members must not inherit the session-level override"
    );

    // A single-repo session is the other shape: one unnamed entry whose
    // override IS the session-level field.
    let mut single = make_test_instance();
    single.base_branch_override = Some("upstream/main".to_string());
    single.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/x".to_string(),
        main_repo_path: "/src/only".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: Some("develop".to_string()),
    });
    let repos = diff_repos_of(&single);
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, None);
    assert_eq!(repos[0].base_override.as_deref(), Some("upstream/main"));
    assert_eq!(repos[0].recorded_base.as_deref(), Some("develop"));
}

/// The PATCH write lands on exactly the named repo, and the unnamed
/// target still writes the session field. See #3329.
#[test]
fn apply_diff_base_override_writes_only_the_named_repo() {
    let mut inst = make_test_instance();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/x".to_string(),
        workspace_dir: "/ws".to_string(),
        repos: ["api", "web"]
            .iter()
            .map(|n| crate::session::WorkspaceRepo {
                name: n.to_string(),
                source_path: format!("/src/{n}"),
                branch: "feature/x".to_string(),
                worktree_path: format!("/ws/{n}"),
                main_repo_path: format!("/src/{n}"),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            })
            .collect(),
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    apply_diff_base_override(&mut inst, Some("web"), Some("epic/checkout".to_string()));
    let overrides: Vec<_> = inst
        .all_repos()
        .iter()
        .map(|r| (r.name.as_str(), r.base_branch_override.as_deref()))
        .collect();
    assert_eq!(
        overrides,
        vec![("api", None), ("web", Some("epic/checkout"))]
    );
    assert_eq!(
        inst.base_branch_override, None,
        "a per-repo write must not touch the session field"
    );

    // Clearing one repo leaves its sibling alone.
    apply_diff_base_override(&mut inst, Some("web"), None);
    assert_eq!(inst.all_repos()[1].base_branch_override, None);

    // The unnamed target is the session's own checkout.
    apply_diff_base_override(&mut inst, None, Some("develop".to_string()));
    assert_eq!(inst.base_branch_override.as_deref(), Some("develop"));
}

#[test]
fn session_response_surfaces_base_branch_when_set() {
    let mut inst = make_test_instance();
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/test".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: Some("release-1.2".to_string()),
    });
    let resp = SessionResponse::from_instance(&inst, false);
    assert_eq!(resp.base_branch.as_deref(), Some("release-1.2"));

    // Field is omitted from the wire JSON when None so old clients
    // don't see a flood of nulls.
    inst.worktree_info.as_mut().unwrap().base_branch = None;
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(
        json.get("base_branch").is_none(),
        "base_branch should be omitted when None, got: {json}"
    );
}

#[test]
fn session_response_serializes_to_json() {
    let inst = make_test_instance();
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();

    assert!(json.get("id").is_some());
    assert_eq!(json["tool"], "claude");
    assert_eq!(json["status"], "Running");
    assert_eq!(json["is_sandboxed"], false);
    assert_eq!(json["claude_fullscreen"], false);
}

#[test]
fn session_response_omits_empty_warnings() {
    let inst = make_test_instance();
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.warnings.is_empty());

    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.get("warnings").is_none(),
        "empty warnings should be omitted from the JSON body, got: {json}"
    );
}

#[test]
fn session_response_serializes_populated_warnings() {
    let inst = make_test_instance();
    let mut resp = SessionResponse::from_instance(&inst, false);
    resp.warnings = vec![
        "post-checkout hook failed for repo-a".to_string(),
        "post-checkout hook failed for repo-b".to_string(),
    ];

    let json = serde_json::to_value(&resp).unwrap();
    let warnings = json
        .get("warnings")
        .expect("warnings should appear in JSON when populated");
    let arr = warnings
        .as_array()
        .expect("warnings should serialize as a JSON array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0], "post-checkout hook failed for repo-a");
    assert_eq!(arr[1], "post-checkout hook failed for repo-b");
}

#[test]
fn claude_fullscreen_set_for_claude_when_enabled() {
    let resp = SessionResponse::from_instance(&make_test_instance(), true);
    assert_eq!(resp.tool, "claude");
    assert!(resp.claude_fullscreen);
}

#[test]
fn session_response_surfaces_pinned_at() {
    let mut inst = make_test_instance();

    // Default: no pin -> field omitted from the JSON body.
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(
        json.get("pinned_at").is_none(),
        "pinned_at should be omitted when None, got: {json}"
    );

    inst.pin();
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.pinned_at.is_some(), "pinned_at must surface when set");
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.get("pinned_at").is_some(),
        "pinned_at must appear in JSON when set"
    );
}

#[test]
fn session_response_surfaces_archived_at() {
    let mut inst = make_test_instance();
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(json.get("archived_at").is_none());

    inst.archive();
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.archived_at.is_some());
}

#[test]
fn session_response_gates_snoozed_until_on_active_snooze() {
    let mut inst = make_test_instance();

    // Not snoozed -> field omitted.
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.snoozed_until.is_none());

    // Active snooze -> field surfaced.
    inst.snooze(30);
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(resp.snoozed_until.is_some());

    // Expired snooze -> stays on disk for the next mutation to rewrite,
    // but the API gates on `is_snoozed()` so the wire value is None.
    // This prevents the web from rendering "snoozed 0m" on rows that
    // have already woken on the server.
    inst.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    let resp = SessionResponse::from_instance(&inst, false);
    assert!(
        resp.snoozed_until.is_none(),
        "expired snooze must be filtered out on the wire even though the persisted field stays set"
    );
}

#[test]
fn update_snooze_validates_against_shared_bounds() {
    // The handler uses `validate_snooze_duration` to reject 0 and >
    // SNOOZE_MAX_MINUTES. Mirror the assertions here so a regression in
    // the validator shape (or in the dialog presets at
    // src/tui/dialogs/snooze_duration.rs) is caught locally.
    assert!(crate::session::validate_snooze_duration(0).is_err());
    for &m in &[60u64, 120, 180, 240, 300, 360, 1440, 7 * 1440] {
        assert!(
            crate::session::validate_snooze_duration(m).is_ok(),
            "preset {m} min must pass validator (matches TUI dialog presets)"
        );
    }
}

#[test]
fn claude_fullscreen_unset_for_non_claude_even_when_enabled() {
    let mut inst = make_test_instance();
    inst.tool = "cursor".to_string();
    let resp = SessionResponse::from_instance(&inst, true);
    assert!(!resp.claude_fullscreen);
}

#[test]
fn claude_fullscreen_unset_when_setting_disabled() {
    let resp = SessionResponse::from_instance(&make_test_instance(), false);
    assert!(!resp.claude_fullscreen);
}

#[test]
fn rename_updates_title_without_changing_worktree_branch() {
    let mut inst = make_test_instance();
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/test".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    apply_session_title_rename(&mut inst, "Renamed Session".to_string());

    assert_eq!(inst.title, "Renamed Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("feature/test")
    );
}

#[test]
fn title_only_rename_cache_patch_preserves_newer_path_and_branch() {
    let mut cached = make_test_instance();
    cached.title = "Old title".to_string();
    cached.project_path = "/tmp/worktrees/concurrent".to_string();
    cached.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "concurrent-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    apply_session_rename_cache_patch(
        &mut cached,
        SessionRenameCachePatch {
            title: "New title",
            initial_path: "/tmp/worktrees/initial",
            initial_branch: Some("initial-branch"),
            authoritative_path: "/tmp/worktrees/earlier-snapshot",
            authoritative_branch: Some("earlier-snapshot-branch"),
            renamed_path: None,
            renamed_branch: None,
        },
    );

    assert_eq!(cached.title, "New title");
    assert_eq!(cached.project_path, "/tmp/worktrees/concurrent");
    assert_eq!(
        cached
            .worktree_info
            .as_ref()
            .map(|worktree| worktree.branch.as_str()),
        Some("concurrent-branch")
    );
    let response = SessionResponse::from_instance(&cached, false);
    assert_eq!(response.title, "New title");
}

#[test]
fn tied_rename_cache_patch_publishes_owned_path_and_branch() {
    let mut cached = make_test_instance();
    cached.project_path = "/tmp/worktrees/concurrent".to_string();
    cached.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "concurrent-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    apply_session_rename_cache_patch(
        &mut cached,
        SessionRenameCachePatch {
            title: "New title",
            initial_path: "/tmp/worktrees/initial",
            initial_branch: Some("initial-branch"),
            authoritative_path: "/tmp/worktrees/renamed",
            authoritative_branch: Some("renamed-branch"),
            renamed_path: Some("/tmp/worktrees/renamed"),
            renamed_branch: Some("renamed-branch"),
        },
    );

    assert_eq!(cached.title, "New title");
    assert_eq!(cached.project_path, "/tmp/worktrees/renamed");
    assert_eq!(
        cached
            .worktree_info
            .as_ref()
            .map(|worktree| worktree.branch.as_str()),
        Some("renamed-branch")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_distinguishes_cwd_stable_title_and_branch_changes() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let paths = tempfile::tempdir().unwrap();
    let title_path = paths.path().join("my-session");
    let branch_path = paths.path().join("branch-only");
    let title_id = "rename-title-only".to_string();
    let branch_id = "rename-branch-only".to_string();

    let mut title_only = Instance::new(
        "Original title",
        title_path.to_str().expect("UTF-8 temp path"),
    );
    title_only.id = title_id.clone();
    title_only.status = Status::Running;
    title_only.view = crate::session::View::Structured;
    title_only.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "my-session".to_string(),
        main_repo_path: paths
            .path()
            .join("missing-repo")
            .to_string_lossy()
            .into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let mut branch_only = Instance::new(
        "Branch Only",
        branch_path.to_str().expect("UTF-8 temp path"),
    );
    branch_only.id = branch_id.clone();
    branch_only.status = Status::Running;
    branch_only.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "existing-branch".to_string(),
        main_repo_path: paths
            .path()
            .join("missing-repo")
            .to_string_lossy()
            .into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let (_storage, state) = build_rename_test_state(
        vec![title_only.clone(), branch_only.clone()],
        vec![title_only, branch_only],
    );
    state.acp_supervisor.test_insert_worker(&title_id).await;

    // The title changes, but its slug already matches both the cwd leaf
    // and branch. Even with the branch toggle armed, this is title-only.
    let title_response = rename_session(
        State(state.clone()),
        Path(title_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "My Session!".to_string(),
            rename_branch: true,
        })),
    )
    .await
    .into_response();
    assert_eq!(title_response.status(), StatusCode::OK);
    let title_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(title_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(title_json["tie_workdir_to_name"], true);
    assert!(
        state.acp_supervisor.is_running(&title_id).await,
        "a cwd-stable title-only rename must not stop the structured worker"
    );

    {
        let instances = state.instances.read().await;
        let renamed = instances.iter().find(|inst| inst.id == title_id).unwrap();
        assert_eq!(renamed.title, "My Session!");
        assert_eq!(renamed.project_path, title_path.to_str().unwrap());
        assert_eq!(
            renamed.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("my-session")
        );
    }

    let branch_response = rename_session(
        State(state.clone()),
        Path(branch_id.clone()),
        Ok(Json(RenameSessionBody {
            title: "Branch Only".to_string(),
            rename_branch: true,
        })),
    )
    .await
    .into_response();
    assert_eq!(branch_response.status(), StatusCode::CONFLICT);
    let branch_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(branch_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(branch_json["error"], "session_running");

    let instances = state.instances.read().await;
    let rejected = instances.iter().find(|inst| inst.id == branch_id).unwrap();
    assert_eq!(rejected.title, "Branch Only");
    assert_eq!(rejected.project_path, branch_path.to_str().unwrap());
    assert_eq!(
        rejected.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("existing-branch"),
        "the active branch-only request must be rejected before git mutation"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rename_session_quiesces_structured_worker_only_when_its_cwd_moves() {
    // Invariant #2260: a live structured-view worker is pinned to its cwd,
    // so a tied rename that MOVES the worktree directory must stop the
    // worker first (else it crash-loops at the pulled-out path), while a
    // rename that leaves the cwd in place must NOT interrupt it. The
    // quiesce runs before the git edit, so the cwd-moving assertion holds
    // even though the edit itself then fails on a fixture with no real
    // worktree to move: what #2260 pins is that the worker is gone by then.
    let _app_dir = crate::session::test_support::isolate_app_dir();

    struct Case {
        id: &'static str,
        leaf: &'static str,
        new_title: &'static str,
        // Whether the new title's slug relocates the worktree directory.
        moves_cwd: bool,
    }
    // The cwd-stable row's slug ("my-session") equals the existing leaf, so
    // the edit is a no-op move; the cwd-moving row's slug differs, forcing
    // a relocation.
    let cases = [
        Case {
            id: "quiesce-cwd-stable",
            leaf: "my-session",
            new_title: "My Session!",
            moves_cwd: false,
        },
        Case {
            id: "quiesce-cwd-moving",
            leaf: "old-leaf",
            new_title: "A Brand New Name",
            moves_cwd: true,
        },
    ];

    for case in cases {
        let paths = tempfile::tempdir().unwrap();
        let project_path = paths.path().join(case.leaf);
        let mut inst = Instance::new(
            "Original title",
            project_path.to_str().expect("UTF-8 temp path"),
        );
        inst.id = case.id.to_string();
        // Idle, not Running: a structured session the user "stopped" sits
        // at Idle yet still owns a live worker, which is exactly the gap
        // `blocks_worktree_edit` misses and quiesce closes.
        inst.status = Status::Idle;
        inst.view = crate::session::View::Structured;
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: case.leaf.to_string(),
            main_repo_path: paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        let (_storage, state) = build_rename_test_state(vec![inst.clone()], vec![inst]);
        state.acp_supervisor.test_insert_worker(case.id).await;

        let _ = rename_session(
            State(state.clone()),
            Path(case.id.to_string()),
            Ok(Json(RenameSessionBody {
                title: case.new_title.to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(
            state.acp_supervisor.is_running(case.id).await,
            !case.moves_cwd,
            "{}: worker should be {} for moves_cwd={}",
            case.id,
            if case.moves_cwd {
                "stopped"
            } else {
                "preserved"
            },
            case.moves_cwd
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn set_worktree_name_quiesces_structured_worker_only_when_its_cwd_moves() {
    // The standalone-endpoint mirror of the rename_session gate above: both
    // stop a live structured-view worker only when the edit actually moves
    // the worktree cwd (#2260), never for a cwd-stable or branch-only edit.
    // The quiesce precedes the git edit, so the cwd-moving assertion holds
    // even though the edit itself then fails on a fixture with no real
    // worktree to move: what #2260 pins is that the worker is gone by then.
    let _app_dir = crate::session::test_support::isolate_app_dir();
    // set_worktree_name refuses a tied managed worktree (tied callers must
    // go through rename_session), so untie the profile to reach the worker
    // gate that this test exercises.
    let mut overrides = serde_json::Map::new();
    overrides.insert(
        "session".to_string(),
        serde_json::json!({ "tie_workdir_to_name": false }),
    );
    crate::session::config::profile_config::save_profile_config(
        "test",
        &crate::session::config::profile_config::ProfileConfig {
            description: None,
            overrides,
        },
    )
    .expect("write test profile override");

    struct Case {
        id: &'static str,
        leaf: &'static str,
        new_name: &'static str,
        // Whether the requested name relocates the worktree directory.
        moves_cwd: bool,
    }
    // The cwd-stable row's name equals the existing leaf (a no-op move); the
    // cwd-moving row's name differs, forcing a relocation.
    let cases = [
        Case {
            id: "sw-cwd-stable",
            leaf: "my-session",
            new_name: "my-session",
            moves_cwd: false,
        },
        Case {
            id: "sw-cwd-moving",
            leaf: "old-leaf",
            new_name: "new-leaf",
            moves_cwd: true,
        },
    ];

    for case in cases {
        let paths = tempfile::tempdir().unwrap();
        let project_path = paths.path().join(case.leaf);
        let mut inst = Instance::new(
            "Original title",
            project_path.to_str().expect("UTF-8 temp path"),
        );
        inst.id = case.id.to_string();
        inst.source_profile = "test".to_string();
        inst.status = Status::Idle;
        inst.view = crate::session::View::Structured;
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: case.leaf.to_string(),
            main_repo_path: paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        let storage = Storage::new_unwatched("test").unwrap();
        storage
            .update(|instances, _groups| {
                *instances = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        state.acp_supervisor.test_insert_worker(case.id).await;

        let _ = set_worktree_name(
            State(state.clone()),
            Path(case.id.to_string()),
            Ok(Json(SetWorktreeNameBody {
                name: case.new_name.to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(
            state.acp_supervisor.is_running(case.id).await,
            !case.moves_cwd,
            "{}: worker should be {} for moves_cwd={}",
            case.id,
            if case.moves_cwd {
                "stopped"
            } else {
                "preserved"
            },
            case.moves_cwd
        );
    }
}

#[test]
fn worktree_name_edit_updates_path_and_optionally_branch() {
    let mut inst = make_test_instance();
    inst.project_path = "/tmp/repo-worktrees/old".to_string();
    inst.title = "My Session".to_string();
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "old".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    // Path-only edit leaves the branch and title untouched.
    apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/new", None);
    assert_eq!(inst.project_path, "/tmp/repo-worktrees/new");
    assert_eq!(inst.title, "My Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("old")
    );

    // Branch rename also updates worktree_info.branch.
    apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/newer", Some("newer"));
    assert_eq!(inst.project_path, "/tmp/repo-worktrees/newer");
    assert_eq!(inst.title, "My Session");
    assert_eq!(
        inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
        Some("newer")
    );
}

#[test]
#[serial_test::serial]
fn apply_post_restart_sync_propagates_agent_session_id() {
    // Models the rapid double-restart case: in-memory state is stale
    // (agent_session_id = None) because the 2s status poller hasn't
    // refreshed yet, while the just-finished restart produced a Claude
    // UUID via acquire_session_id. The sync must propagate that ID so a
    // second ensure_session within the poller window doesn't generate a
    // fresh UUID and orphan the persisted Claude conversation.
    let mut live = make_test_instance();
    live.status = Status::Stopped;
    live.last_error = Some("prior failure".to_string());
    live.agent_session_id = None;
    live.last_start_time = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Starting;
    started.agent_session_id = Some("claude-uuid-restart".to_string());
    started.omp_capture_generation = Some("omp-generation-restart".to_string());
    let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
    assert_eq!(
        poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
        crate::session::poller::PollerSpawn::Spawned
    );
    let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
    started.session_id_poller = Some(restarted_poller.clone());
    started.last_start_time = Some(std::time::Instant::now());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Starting);
    assert!(live.last_error.is_none());
    assert_eq!(
        live.agent_session_id.as_deref(),
        Some("claude-uuid-restart")
    );
    assert_eq!(
        live.omp_capture_generation.as_deref(),
        Some("omp-generation-restart")
    );
    assert!(live.session_id_poller_is_running());
    assert_eq!(live.last_start_time, started.last_start_time);

    let mut generation_converged = before.clone();
    generation_converged.agent_session_id = Some("peer-sid".to_string());
    generation_converged.omp_capture_generation = Some("omp-generation-restart".to_string());
    apply_post_restart_identity_sync(&mut generation_converged, &before, &started);
    assert_eq!(
        generation_converged.agent_session_id.as_deref(),
        Some("peer-sid")
    );

    let mut peer_relaunched = before.clone();
    peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
    apply_post_restart_identity_sync(&mut peer_relaunched, &before, &started);
    assert_eq!(
        peer_relaunched.omp_capture_generation.as_deref(),
        Some("peer-generation")
    );
    let mut peer = before.clone();
    peer.pi_session_path = Some("/peer/transcript.jsonl".into());
    let expected = peer.conversation_state();
    apply_post_restart_identity_sync(&mut peer, &before, &started);
    assert_eq!(peer.conversation_state(), expected);
    restarted_poller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop();
}

#[test]
fn apply_post_restart_identity_sync_clears_repair_backoff_when_the_restart_cascade_runs() {
    let mut before = make_test_instance();
    before.omp_capture_generation = Some("generation-a".to_string());
    let now = std::time::Instant::now();
    before.poller_repair.defer(now);
    before.poller_repair.defer(now);
    assert_eq!(before.poller_repair.deferrals(), 2);

    let mut started = before.clone();
    started.omp_capture_generation = Some("generation-b".to_string());
    started.poller_repair.reset();
    let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
    assert_eq!(
        poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
        crate::session::poller::PollerSpawn::Spawned
    );
    let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
    started.session_id_poller = Some(restarted_poller.clone());

    let mut live = before.clone();
    apply_post_restart_identity_sync(&mut live, &before, &started);
    assert_eq!(
        live.poller_repair.deferrals(),
        0,
        "the merged live row must not keep the pre-restart backoff"
    );

    let mut peer_relaunched = before.clone();
    peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
    apply_post_restart_identity_sync(&mut peer_relaunched, &before, &started);
    assert_eq!(peer_relaunched.poller_repair.deferrals(), 0);

    let mut not_started = started.clone();
    not_started.session_id_poller = None;
    let mut live = before.clone();
    apply_post_restart_identity_sync(&mut live, &before, &not_started);
    assert_eq!(
        live.poller_repair.deferrals(),
        2,
        "a restart without a running poller leaves the schedule alone"
    );
    restarted_poller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop();
}

#[test]
fn apply_post_restart_sync_overwrites_stale_session_id() {
    // If somehow the in-memory ID was non-None and the start path
    // produced a different (newer) ID, the sync must use the newer one.
    // Belt-and-suspenders: in practice acquire_session_id reuses an
    // existing ID, but the contract here is "started wins."
    let mut live = make_test_instance();
    live.agent_session_id = Some("stale-id".to_string());
    let before = live.clone();

    let mut started = make_test_instance();
    started.agent_session_id = Some("fresh-id".to_string());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.agent_session_id.as_deref(), Some("fresh-id"));
}

#[test]
fn apply_post_restart_sync_propagates_resume_failed_marker_and_error() {
    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("prior failure".to_string());
    live.agent_session_id = Some("sid-before".to_string());
    live.resume_probe_failed_sid = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.agent_session_id = Some("sid-after".to_string());
    started.resume_probe_failed_sid = Some("sid-after".to_string());
    started.last_error =
        Some("resume failed for sid sid-after; preserved for explicit retry".to_string());
    started.last_error_check = Some(std::time::Instant::now());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Error);
    assert_eq!(
        live.last_error.as_deref(),
        Some("resume failed for sid sid-after; preserved for explicit retry")
    );
    assert!(live.last_error_check.is_some());
    assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
}

#[test]
fn apply_cascade_state_sync_propagates_marker_without_status() {
    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("keep me".to_string());
    live.agent_session_id = Some("sid-before".to_string());
    live.resume_probe_failed_sid = None;
    let before = live.clone();

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.last_error = Some("resume failed".to_string());
    started.agent_session_id = Some("sid-after".to_string());
    started.resume_probe_failed_sid = Some("sid-after".to_string());

    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Running);
    assert_eq!(live.last_error.as_deref(), Some("keep me"));
    assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
}

#[test]
fn apply_post_restart_sync_preserves_peer_sid_write() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("stale-restart-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = make_test_instance();
    live.agent_session_id = Some("peer-fresh-sid".to_string());
    live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());
    started.last_error = Some("resume failed".to_string());

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Error);
    assert_eq!(live.last_error.as_deref(), Some("resume failed"));
    assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        live.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
}

#[test]
fn restart_sync_rejects_an_older_lifecycle_generation() {
    let mut before = make_test_instance();
    before.lifecycle_generation = 4;

    let mut started = before.clone();
    started.status = Status::Error;
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.retroactive_capture_excludes = [crate::session::ConversationBinding::unknown(
        "stale-exclusion".to_string(),
    )]
    .into();

    let mut live = before.clone();
    live.lifecycle_generation = 5;
    live.status = Status::Running;
    live.agent_session_id = Some("newer-restart-sid".to_string());
    live.retroactive_capture_excludes = [crate::session::ConversationBinding::unknown(
        "newer-exclusion".to_string(),
    )]
    .into();

    assert!(!apply_post_restart_sync(&mut live, &before, &started));
    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.lifecycle_generation, 5);
    assert_eq!(live.status, Status::Running);
    assert_eq!(live.agent_session_id.as_deref(), Some("newer-restart-sid"));
    assert_eq!(
        live.retroactive_capture_excludes,
        [crate::session::ConversationBinding::unknown(
            "newer-exclusion".to_string()
        )]
        .into()
    );
}

#[test]
fn apply_post_restart_sync_preserves_peer_marker_for_same_sid() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("same-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = before.clone();
    live.resume_probe_failed_sid = Some("same-sid".to_string());

    let mut started = before.clone();
    started.status = Status::Starting;
    started.resume_probe_failed_sid = None;

    apply_post_restart_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Starting);
    assert_eq!(live.agent_session_id.as_deref(), Some("same-sid"));
    assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("same-sid"));
}

#[test]
fn apply_cascade_state_sync_preserves_peer_sid_write() {
    let mut before = make_test_instance();
    before.agent_session_id = Some("stale-restart-sid".to_string());
    before.resume_probe_failed_sid = None;

    let mut live = make_test_instance();
    live.status = Status::Running;
    live.last_error = Some("keep me".to_string());
    live.agent_session_id = Some("peer-fresh-sid".to_string());
    live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

    let mut started = make_test_instance();
    started.status = Status::Error;
    started.last_error = Some("resume failed".to_string());
    started.agent_session_id = Some("stale-restart-sid".to_string());
    started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());

    apply_cascade_state_sync(&mut live, &before, &started);

    assert_eq!(live.status, Status::Running);
    assert_eq!(live.last_error.as_deref(), Some("keep me"));
    assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        live.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
}

#[test]
#[serial_test::serial]
fn send_message_post_restart_save_preserves_peer_sid_write() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "send-post-restart-peer-sid";
    let storage = Storage::new_unwatched(profile).unwrap();
    let mut seed = make_test_instance();
    let id = seed.id.clone();
    seed.agent_session_id = Some("peer-fresh-sid".to_string());
    seed.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    let mut sync_base_for_save = make_test_instance();
    sync_base_for_save.id = id.clone();
    sync_base_for_save.agent_session_id = Some("stale-restart-sid".to_string());
    sync_base_for_save.resume_probe_failed_sid = None;

    let mut started_for_save = make_test_instance();
    started_for_save.id = id.clone();
    started_for_save.status = Status::Starting;
    started_for_save.agent_session_id = Some("stale-restart-sid".to_string());
    started_for_save.resume_probe_failed_sid = None;

    storage
        .update(|all, _groups| {
            if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(disk_inst, &sync_base_for_save, &started_for_save);
                disk_inst.touch_last_accessed();
            }
            Ok(())
        })
        .unwrap();

    let reloaded = storage.load().unwrap();
    let disk = reloaded.iter().find(|i| i.id == seed.id).unwrap();
    assert_eq!(disk.status, Status::Starting);
    assert_eq!(disk.agent_session_id.as_deref(), Some("peer-fresh-sid"));
    assert_eq!(
        disk.resume_probe_failed_sid.as_deref(),
        Some("peer-fresh-sid")
    );
    assert!(disk.last_accessed_at.is_some());
}

#[test]
#[serial_test::serial]
fn session_tool_identity_accepts_builtin_agent() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let project = tempfile::tempdir().unwrap();

    assert!(validate_session_tool_identity(
        "claude",
        "default",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_accepts_non_empty_configured_custom_agent() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            remote-claude = "ssh -t host claude"
        "#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();

    assert!(validate_session_tool_identity(
        "remote-claude",
        "default",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_rejects_unknown_agent() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let project = tempfile::tempdir().unwrap();

    assert!(!validate_session_tool_identity(
        "surprise-agent",
        "default",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_rejects_empty_custom_agent_command() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            remote-claude = ""
        "#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();

    assert!(!validate_session_tool_identity(
        "remote-claude",
        "default",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_rejects_whitespace_only_custom_agent_command() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            remote-claude = "   "
        "#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();

    assert!(!validate_session_tool_identity(
        "remote-claude",
        "default",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_uses_requested_profile() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let work_profile = app_dir.join("profiles").join("work");
    std::fs::create_dir_all(&work_profile).unwrap();
    std::fs::write(
        work_profile.join("config.toml"),
        r#"
            [session.custom_agents]
            work-agent = "ssh -t work claude"
        "#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();

    assert!(!validate_session_tool_identity(
        "work-agent",
        "default",
        project.path()
    ));
    assert!(validate_session_tool_identity(
        "work-agent",
        "work",
        project.path()
    ));
}

#[test]
#[serial_test::serial]
fn session_tool_identity_uses_repo_aware_config_but_not_repo_custom_agents() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            my-agent = "ssh -t lenovo claude"
        "#,
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    let repo_config_dir = project.path().join(".agent-of-empires");
    std::fs::create_dir_all(&repo_config_dir).unwrap();
    std::fs::write(
        repo_config_dir.join("config.toml"),
        r#"
            [session.custom_agents]
            repo-agent = "ssh -t repo claude"
        "#,
    )
    .unwrap();

    // The user's own custom agent resolves through the repo-aware path.
    assert!(validate_session_tool_identity(
        "my-agent",
        "default",
        project.path()
    ));
    // A repo-defined one does not exist as far as AoE is concerned (#3154).
    assert!(!validate_session_tool_identity(
        "repo-agent",
        "default",
        project.path()
    ));
}

/// Build one structured, idle session with an empty `source_profile`, so
/// `purge_session_artifacts` refuses on its first line. The teardown that
/// follows is what a delete must not start under an in-flight submission;
/// these tests only need to observe that it waits for one.
fn delete_race_state(id: &str) -> std::sync::Arc<crate::server::AppState> {
    delete_race_state_for(&[id])
}

/// [`delete_race_state`] for a workspace: every id shares the shape, so a
/// sibling teardown can be observed the same way the owner's is.
fn delete_race_state_for(ids: &[&str]) -> std::sync::Arc<crate::server::AppState> {
    let instances = ids
        .iter()
        .map(|id| {
            let mut inst = Instance::new("delete-3650", "/tmp/aoe-3650-delete");
            inst.id = (*id).to_string();
            inst.view = crate::session::View::Structured;
            inst.status = Status::Idle;
            inst
        })
        .collect();
    crate::server::test_support::build_test_app_state(instances)
}

/// #3650: prompt submission moved off `instance_lock`, so a permanent
/// delete that takes only `instance_lock` no longer excludes a queue drain
/// that snapshotted an idle turn and is on its way to `send_turn`. The
/// delete would then stop the worker, purge the transcript and remove the
/// worktree under a delivery already in flight.
///
/// Each permanent-delete path is checked the same way: hold the session's
/// submission guard (standing in for that drain) and assert the delete
/// parks before any teardown, then completes once the guard drops.
#[tokio::test]
async fn permanent_deletion_waits_for_an_in_flight_submission() {
    let _home = crate::session::test_support::isolate_app_dir();
    use std::time::Duration;

    // Direct delete.
    let state = delete_race_state("sess-3650-direct");
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-direct")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let delete = {
        let state = std::sync::Arc::clone(&state);
        async move {
            delete_session(
                State(state),
                Path("sess-3650-direct".to_string()),
                Some(Json(DeleteSessionBody::default())),
            )
            .await
            .into_response()
        }
    };
    tokio::pin!(delete);
    assert!(
        futures_util::poll!(&mut delete).is_pending(),
        "a delete must not tear a session down under an in-flight submission"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), claims.recv())
            .await
            .expect("contender must reach submission claim")
            .expect("submission claim watcher must remain open"),
        "sess-3650-direct"
    );
    assert_eq!(
        state.instances.read().await[0].status,
        Status::Idle,
        "the session must not even be marked Deleting yet"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), delete)
        .await
        .expect("the delete lands once the submission releases the session");

    // Workspace teardown, on the owner's own guard.
    let state = delete_race_state("sess-3650-owner");
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-owner")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let workspace = {
        let state = std::sync::Arc::clone(&state);
        async move {
            purge_workspace_artifacts(
                &state,
                "sess-3650-owner".to_string(),
                vec![("sess-3650-owner".to_string(), DeleteSessionBody::default())],
                false,
            )
            .await
        }
    };
    tokio::pin!(workspace);
    assert!(
        futures_util::poll!(&mut workspace).is_pending(),
        "a workspace teardown must wait for the owner's in-flight submission"
    );
    assert_eq!(
        claims
            .try_recv()
            .expect("contender reached submission claim"),
        "sess-3650-owner"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), workspace)
        .await
        .expect("the workspace teardown lands once the submission releases");

    // A workspace teardown must also wait on a sibling's live submission.
    let state = delete_race_state_for(&["sess-3650-sib", "sess-3650-ws-owner"]);
    let delivering = state
        .session_service
        .prompt_submission("sess-3650-sib")
        .await;
    let mut claims = state.session_service.watch_submission_claims();
    let workspace = {
        let state = std::sync::Arc::clone(&state);
        async move {
            purge_workspace_artifacts(
                &state,
                "sess-3650-ws-owner".to_string(),
                vec![
                    ("sess-3650-sib".to_string(), DeleteSessionBody::default()),
                    (
                        "sess-3650-ws-owner".to_string(),
                        DeleteSessionBody::default(),
                    ),
                ],
                false,
            )
            .await
        }
    };
    tokio::pin!(workspace);
    assert!(
        futures_util::poll!(&mut workspace).is_pending(),
        "a workspace teardown must wait for a sibling's in-flight submission"
    );
    assert!(
        std::iter::from_fn(|| claims.try_recv().ok()).any(|id| id == "sess-3650-sib"),
        "contender reached the sibling submission claim"
    );
    assert!(
        state
            .instances
            .read()
            .await
            .iter()
            .all(|i| i.status == Status::Idle),
        "no row may be marked Deleting while the sibling's submission is in flight"
    );
    drop(delivering);
    tokio::time::timeout(Duration::from_secs(10), workspace)
        .await
        .expect("the workspace teardown lands once the sibling submission releases");
}

/// Retention waits for submissions, then rechecks restores under the instance lock.
#[tokio::test]
async fn the_retention_purge_takes_submission_before_the_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    std::fs::write(
        crate::session::get_app_dir().unwrap().join("config.toml"),
        "[session]\ntrash_retention_days = 1\n",
    )
    .unwrap();
    let mut inst = make_test_instance();
    inst.trashed_at = Some(chrono::Utc::now() - chrono::Duration::days(2));
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let submission = state.session_service.prompt_submission(&id).await;
    let mut claims = state.session_service.watch_submission_claims();
    let purge = purge_expired_trash(&state);
    tokio::pin!(purge);
    assert!(futures_util::poll!(&mut purge).is_pending());
    assert_eq!(
        claims.try_recv().expect("purge reaches submission claim"),
        id
    );
    let lock = state.instance_lock(&id).await;
    let held = lock
        .try_lock()
        .expect("submission must precede instance lock");
    drop(submission);
    assert!(futures_util::poll!(&mut purge).is_pending());
    state.instances.write().await[0].trashed_at = None;
    drop(held);
    purge.await;
    assert_eq!(
        state.instances.read().await[0].id,
        id,
        "restore wins before purge snapshot"
    );
}

/// #3650's barrier applies to every handler that stops a worker, not just
/// the ones that delete a session. `drain_queued_prompts_once` reads the
/// status and the trashed/archived/snoozed flags once under the submission
/// guard and only then reaches `send_turn`, which respawns a worker it
/// finds gone. So a stop that lands inside that window is undone: the user
/// presses Stop and the session comes back running the queued prompt.
///
/// Before #3639 the drain held `instance_lock` across delivery and these
/// four handlers were excluded by it. They take the submission guard now
/// for the same reason `attach_project` and the tied renames do.
#[tokio::test]
async fn worker_stopping_handlers_wait_for_an_in_flight_submission() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    use std::time::Duration;

    async fn call(
        which: &str,
        state: std::sync::Arc<crate::server::AppState>,
        id: String,
    ) -> axum::response::Response {
        match which {
            "stop" => stop_session(State(state), Path(id)).await.into_response(),
            "trash" => trash_session(State(state), Path(id), None)
                .await
                .into_response(),
            "archive" => update_session_archive(
                State(state),
                Path(id),
                Ok(Json(UpdateArchiveBody {
                    archived: true,
                    kill_pane: true,
                })),
            )
            .await
            .into_response(),
            "snooze" => update_session_snooze(
                State(state),
                Path(id),
                Ok(Json(UpdateSnoozeBody { minutes: Some(30) })),
            )
            .await
            .into_response(),
            other => unreachable!("unknown handler {other}"),
        }
    }

    for which in ["stop", "trash", "archive", "snooze"] {
        let id = format!("sess-3650-{which}");
        let state = delete_race_state(&id);
        let delivering = state.session_service.prompt_submission(&id).await;
        let mut claims = state.session_service.watch_submission_claims();
        let handler = {
            let state = std::sync::Arc::clone(&state);
            let id = id.clone();
            async move { call(which, state, id).await }
        };
        tokio::pin!(handler);
        assert!(
            futures_util::poll!(&mut handler).is_pending(),
            "{which} must not quiesce a worker a submission is mid-delivery on"
        );
        assert_eq!(
            claims
                .try_recv()
                .expect("contender reached submission claim"),
            id
        );
        assert_eq!(
            state.instances.read().await[0].status,
            Status::Idle,
            "{which} must not have touched the session yet"
        );

        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), handler)
            .await
            .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"));
    }
}

/// #3651: `prompt_submission` auto-vivifies a registry entry for whatever
/// id it is handed and nothing prunes it, so every externally reachable
/// mutation that claims it must prove the session exists first. Otherwise
/// an authenticated client grows daemon memory with random ids.
#[tokio::test]
async fn session_mutations_allocate_no_prompt_lock_for_an_unknown_id() {
    let state = crate::server::test_support::build_test_app_state(Vec::new());
    let service = std::sync::Arc::clone(&state.session_service);

    for i in 0..3 {
        let id = format!("sess-gone-{i}");
        assert_eq!(
            rename_session(
                State(std::sync::Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(RenameSessionBody {
                    title: "new title".to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response()
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            set_worktree_name(
                State(std::sync::Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(SetWorktreeNameBody {
                    name: "new-dir".to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response()
            .status(),
            StatusCode::NOT_FOUND
        );
        assert!(matches!(
            crate::server::attach_project::attach_project(
                &state,
                &id,
                std::path::Path::new("/tmp"),
                crate::session::attach_project::ExistingBranch::Refuse,
            )
            .await,
            Err(crate::server::attach_project::AttachError::NotFound)
        ));
        assert!(matches!(
            service
                .edit_queued_prompt(&id, "q1".to_string(), "text".to_string())
                .await,
            crate::server::session_service::EditQueuedOutcome::NotFound
        ));
        assert!(!service.remove_queued_prompt(&id, "q1".to_string()).await);
        service.clear_queued_prompts(&id).await;
    }

    assert_eq!(
        service.prompt_locks_len().await,
        0,
        "an id that was never admitted must not leave a lock-registry entry behind"
    );
}

#[test]
fn create_session_validates_tool_before_builder_or_persistence() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions/create.rs"),
    )
    .unwrap();
    let create_start = source.find("pub async fn create_session").unwrap();
    let create_source = &source[create_start..];
    // Anchor on the call, not the bare name: a comment above mentions the
    // fn earlier in the handler and would satisfy a name-only find.
    let validation = create_source
        .find("if !validate_session_tool_identity(")
        .unwrap();
    let profile_default = create_source.find("body.profile.unwrap_or").unwrap();
    let spawn_blocking = create_source.find("tokio::task::spawn_blocking").unwrap();
    // Build and persistence both go through session_spawn.
    let session_spawn = create_source
        .find("crate::server::session_spawn::")
        .unwrap();

    assert!(validation < profile_default);
    assert!(validation < spawn_blocking);
    assert!(validation < session_spawn);
    assert!(create_source.contains("body.profile.as_deref().unwrap_or(&default_profile)"));
    assert!(create_source.contains("std::path::Path::new(&body.path)"));
    assert!(!create_source[validation..spawn_blocking].contains("command_override"));
}

#[tokio::test]
async fn ensure_session_refreshes_instance_after_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    let inst = make_test_instance();
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let lock = state.instance_lock(&id).await;
    let held = lock.lock().await;
    let handler = ensure_session(State(state.clone()), Path(id.clone()), Ok(None));
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    state.instances.write().await.clear();
    drop(held);
    assert_eq!(
        handler.await.into_response().status(),
        StatusCode::NOT_FOUND
    );
}

/// The three terminal handlers must take the per-session lock before
/// snapshotting the instance, like `ensure_session`; a read-then-lock order
/// lets a concurrent mutation land between the two and hands `spawn_blocking`
/// a stale clone.
#[tokio::test]
async fn terminal_handlers_take_instance_lock_before_snapshot() {
    let _home = crate::session::test_support::isolate_app_dir();
    for which in ["ensure", "container", "kill"] {
        let inst = make_test_instance();
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let lock = state.instance_lock(&id).await;
        let held = lock.lock().await;
        let handler = async {
            let query =
                axum::extract::Query(crate::server::live_ws::TerminalIndexQuery { index: 1 });
            match which {
                "ensure" => {
                    ensure_terminal(State(state.clone()), Path(id.clone()), query, Ok(None))
                        .await
                        .into_response()
                }
                "container" => ensure_container_terminal(
                    State(state.clone()),
                    Path(id.clone()),
                    query,
                    Ok(None),
                )
                .await
                .into_response(),
                "kill" => kill_terminal(State(state.clone()), Path(id.clone()), query)
                    .await
                    .into_response(),
                _ => unreachable!(),
            }
        };
        tokio::pin!(handler);
        assert!(futures_util::poll!(&mut handler).is_pending(), "{which}");
        state.instances.write().await.clear();
        drop(held);
        assert_eq!(handler.await.status(), StatusCode::NOT_FOUND, "{which}");
    }
}

/// A workspace row can persist with `repos: []`; a file-diff request that
/// omits `?repo=` must get a 400 for it, not a panic on the empty repo list.
#[tokio::test]
async fn diff_file_rejects_workspace_with_no_repos() {
    use axum::extract::Query;

    let mut inst = Instance::new("empty-ws", "/tmp/aoe-empty-ws");
    inst.id = "empty-ws".to_string();
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "main".to_string(),
        workspace_dir: "/tmp/aoe-empty-ws".to_string(),
        repos: Vec::new(),
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });
    let state = crate::server::test_support::build_test_app_state(vec![inst]);

    let resp = session_diff_file(
        State(state),
        Path("empty-ws".to_string()),
        Query(FileDiffQuery {
            path: "Cargo.toml".to_string(),
            repo: None,
        }),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// "Open file" in the diff list: the raw route serves the selected repo's
/// current worktree bytes, typed so passive files render in the tab while
/// scriptable or unrenderable ones download, and refuses whatever the confined
/// reader refuses.
mod diff_file_raw {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::Query;
    use axum::http::header;

    fn state_for(inst: Instance, cityhall: bool) -> Arc<crate::server::AppState> {
        if cityhall {
            crate::server::test_support::build_test_app_state_cityhall(vec![inst])
        } else {
            crate::server::test_support::build_test_app_state(vec![inst])
        }
    }

    fn single_repo(dir: &std::path::Path) -> Instance {
        let mut inst = Instance::new("raw", dir.to_str().unwrap());
        inst.id = "raw".to_string();
        inst
    }

    async fn get(
        state: &Arc<crate::server::AppState>,
        id: &str,
        path: &str,
        repo: Option<&str>,
    ) -> axum::response::Response {
        session_diff_file_raw(
            State(state.clone()),
            Path(id.to_string()),
            Query(FileDiffQuery {
                path: path.to_string(),
                repo: repo.map(str::to_string),
            }),
        )
        .await
        .into_response()
    }

    #[tokio::test]
    async fn renders_passive_types_and_downloads_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        // (file name, bytes, Content-Type, Content-Disposition)
        let cases: [(&str, &[u8], &str, Option<&str>); 10] = [
            (
                "report.pdf",
                b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n",
                "application/pdf",
                None,
            ),
            ("shot.png", b"\x89PNG\r\n\x1a\n\0\0", "image/png", None),
            ("notes.txt", b"hello\n", "text/plain; charset=utf-8", None),
            // mime_guess calls `.ts` a video type; the text shows as text.
            (
                "main.ts",
                b"export const a = 1;\n",
                "text/plain; charset=utf-8",
                None,
            ),
            (
                "page.html",
                b"<script>alert(1)</script>",
                "application/octet-stream",
                Some("attachment"),
            ),
            (
                "d.svg",
                b"<svg xmlns='http://www.w3.org/2000/svg'/>",
                "application/octet-stream",
                Some("attachment"),
            ),
            (
                "data.xml",
                b"<a/>",
                "application/octet-stream",
                Some("attachment"),
            ),
            (
                "feed.rss",
                b"<rss/>",
                "application/octet-stream",
                Some("attachment"),
            ),
            (
                "archive.zip",
                b"PK\x03\x04\0\0",
                "application/zip",
                Some("attachment"),
            ),
            (
                "blob.unknown",
                b"\0\x01\x02",
                "application/octet-stream",
                Some("attachment"),
            ),
        ];
        for (name, bytes, _, _) in cases {
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        let state = state_for(single_repo(dir.path()), false);

        for (name, bytes, content_type, disposition) in cases {
            let resp = get(&state, "raw", name, None).await;
            assert_eq!(resp.status(), StatusCode::OK, "{name}");
            let headers = resp.headers();
            assert_eq!(
                headers.get(header::CONTENT_TYPE).unwrap(),
                content_type,
                "{name}"
            );
            assert_eq!(
                headers
                    .get(header::CONTENT_DISPOSITION)
                    .map(|v| v.to_str().unwrap()),
                disposition,
                "{name}"
            );
            assert_eq!(
                headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
                "nosniff",
                "{name}"
            );
            assert_eq!(
                headers.get(header::CACHE_CONTROL).unwrap(),
                "no-store",
                "{name}"
            );
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&body[..], bytes, "{name}");
        }
    }

    #[tokio::test]
    async fn refuses_paths_the_confined_reader_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "KEY").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let absolute = dir.path().join("a.txt");
        let state = state_for(single_repo(dir.path()), false);

        // (path, repo, status)
        for (path, repo, status) in [
            ("deleted.txt", None, StatusCode::NOT_FOUND),
            ("../secret", None, StatusCode::BAD_REQUEST),
            (absolute.to_str().unwrap(), None, StatusCode::BAD_REQUEST),
            ("", None, StatusCode::BAD_REQUEST),
            ("sub", None, StatusCode::BAD_REQUEST),
            ("link", None, StatusCode::FORBIDDEN),
            ("a.txt", Some("other"), StatusCode::BAD_REQUEST),
        ] {
            let resp = get(&state, "raw", path, repo).await;
            assert_eq!(resp.status(), status, "path={path:?} repo={repo:?}");
        }

        assert_eq!(
            get(&state, "missing", "a.txt", None).await.status(),
            StatusCode::NOT_FOUND
        );
        let cityhall = state_for(single_repo(dir.path()), true);
        assert_eq!(
            get(&cityhall, "raw", "a.txt", None).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    /// Workspace members can share a relative path, so `?repo=` must pick the
    /// worktree, and an omitted one means the first member.
    #[tokio::test]
    async fn reads_from_the_named_workspace_repo() {
        let ws = tempfile::tempdir().unwrap();
        let member = |name: &str| {
            let worktree = ws.path().join(name);
            std::fs::create_dir(&worktree).unwrap();
            std::fs::write(worktree.join("same.txt"), name).unwrap();
            crate::session::WorkspaceRepo {
                name: name.to_string(),
                source_path: format!("/src/{name}"),
                branch: "feature/x".to_string(),
                worktree_path: worktree.to_string_lossy().into_owned(),
                main_repo_path: format!("/src/{name}"),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            }
        };
        let mut inst = single_repo(ws.path());
        inst.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: ws.path().to_string_lossy().into_owned(),
            repos: vec![member("api"), member("web")],
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });
        let state = state_for(inst, false);

        for (repo, expected) in [(Some("web"), "web"), (Some("api"), "api"), (None, "api")] {
            let resp = get(&state, "raw", "same.txt", repo).await;
            assert_eq!(resp.status(), StatusCode::OK, "repo={repo:?}");
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&body[..], expected.as_bytes(), "repo={repo:?}");
        }
    }
}

#[tokio::test]
async fn send_message_refreshes_instance_after_instance_lock() {
    let _home = crate::session::test_support::isolate_app_dir();
    let inst = make_test_instance();
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let lock = state.instance_lock(&id).await;
    let held = lock.lock().await;
    let handler = send_message(
        State(state.clone()),
        Path(id.clone()),
        Ok(Json(SendMessageRequest {
            message: "hello".into(),
            revive: false,
        })),
    );
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    state.instances.write().await.clear();
    drop(held);
    assert_eq!(
        handler.await.into_response().status(),
        StatusCode::NOT_FOUND
    );
}
// ── validate_diff_path: security regression tests ──────────────────────────
//
// Regression for a path-traversal vulnerability in the first cut of the
// `/api/sessions/{id}/diff/file?path=...` endpoint. Any authenticated user
// could pass `?path=/etc/passwd` or `?path=../../etc/shadow` and have the
// server dump the file contents in a diff response. The validator must
// reject absolute paths, parent-dir traversal, and any path that isn't in
// the set of actually-changed files.

use crate::git::diff::{DiffFile, FileStatus};
use std::path::PathBuf;
use tempfile::TempDir;

fn changed(paths: &[&str]) -> Vec<DiffFile> {
    paths
        .iter()
        .map(|p| DiffFile {
            path: PathBuf::from(p),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
        })
        .collect()
}

#[test]
fn validate_diff_path_rejects_absolute() {
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(
        dir.path(),
        std::path::Path::new("/etc/passwd"),
        &changed(&["src/main.rs"]),
    )
    .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

#[test]
fn validate_diff_path_rejects_parent_dir() {
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(
        dir.path(),
        std::path::Path::new("../../etc/passwd"),
        &changed(&["src/main.rs"]),
    )
    .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

#[test]
fn validate_diff_path_rejects_parent_dir_in_middle() {
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(
        dir.path(),
        std::path::Path::new("src/../../etc/passwd"),
        &changed(&["src/main.rs"]),
    )
    .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

#[test]
fn validate_diff_path_rejects_empty() {
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(dir.path(), std::path::Path::new(""), &[]).unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
}

#[test]
fn validate_diff_path_accepts_unchanged_existing_file() {
    // An in-repo file that exists on disk but is not in the changed set is
    // now accepted for the full-file fallback (#1810), flagged
    // `is_changed = false`. The tracked-blob gate that blocks `.git/` and
    // gitignored secrets lives in compute_unchanged_file_contents, not here.
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
    let (_, is_changed) = validate_diff_path(
        dir.path(),
        std::path::Path::new("existing.txt"),
        &changed(&["src/main.rs"]),
    )
    .unwrap();
    assert!(!is_changed);
}

#[test]
fn validate_diff_path_rejects_nonexistent_unchanged_file() {
    // Not in the changed set and not on disk: nothing to show.
    let dir = TempDir::new().unwrap();
    let err = validate_diff_path(
        dir.path(),
        std::path::Path::new("ghost.txt"),
        &changed(&["src/main.rs"]),
    )
    .unwrap_err();
    assert_eq!(err.0, StatusCode::NOT_FOUND);
}

#[test]
fn validate_diff_path_accepts_changed_file() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("changed.txt"), "hello").unwrap();
    let (_, is_changed) = validate_diff_path(
        dir.path(),
        std::path::Path::new("changed.txt"),
        &changed(&["changed.txt"]),
    )
    .unwrap();
    assert!(is_changed);
}

#[test]
fn validate_diff_path_accepts_deleted_file() {
    // A file that has been deleted on disk but is in the changed set
    // (status: Deleted) should still be diffable so the user can see
    // what was removed. canonicalize() on the joined path will fail,
    // so the validator must fall back to the non-canonical path.
    let dir = TempDir::new().unwrap();
    let (_, is_changed) = validate_diff_path(
        dir.path(),
        std::path::Path::new("deleted.txt"),
        &changed(&["deleted.txt"]),
    )
    .unwrap();
    assert!(is_changed);
}

#[test]
fn truncate_title_returns_unchanged_under_limit() {
    assert_eq!(truncate_title("hello", 10), "hello");
}

#[test]
fn truncate_title_returns_unchanged_at_exact_limit() {
    assert_eq!(truncate_title("hello", 5), "hello");
}

#[test]
fn truncate_title_appends_ellipsis_when_over_limit() {
    let out = truncate_title("abcdefghij", 5);
    assert_eq!(out, "abcd…");
    assert_eq!(out.chars().count(), 5);
}

#[test]
fn truncate_title_counts_characters_not_bytes() {
    // Multi-byte input: each ☃ is 3 bytes, 1 char. Truncating to 3
    // chars must split on character boundary, not byte offset.
    let out = truncate_title("☃☃☃☃☃", 3);
    assert_eq!(out, "☃☃…");
    assert_eq!(out.chars().count(), 3);
}

#[test]
fn session_response_serializes_unread_marker() {
    use crate::session::Instance;
    let mut inst = Instance::new("t", "/tmp");
    // Read: the field is omitted from the wire (skip_serializing_if false).
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert!(json.get("unread").is_none());
    // Unread serializes as a bare boolean the web reads directly.
    inst.unread = true;
    let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
    assert_eq!(json["unread"], serde_json::json!(true));
}

fn step(
    id: &str,
    title: &str,
    status: crate::acp::state::PlanStepStatus,
) -> crate::acp::state::PlanStep {
    crate::acp::state::PlanStep {
        id: id.into(),
        title: title.into(),
        detail: None,
        status,
    }
}

#[test]
fn plan_summary_counts_done_steps_only() {
    use crate::acp::state::PlanStepStatus::*;
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![
            step("a", "alpha", Done),
            step("b", "beta", Done),
            step("c", "gamma", InProgress),
            step("d", "delta", Pending),
        ],
    };
    let s = plan_summary_from_plan(plan);
    assert_eq!(s.total, 4);
    assert_eq!(s.completed, 2);
    assert_eq!(s.current_step_title.as_deref(), Some("gamma"));
}

#[test]
fn plan_summary_current_step_skips_done_picks_first_non_done() {
    use crate::acp::state::PlanStepStatus::*;
    // First non-Done is the first Pending; InProgress later doesn't
    // override (matches the helper's `find(..)` semantics).
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![
            step("a", "alpha", Done),
            step("b", "beta", Pending),
            step("c", "gamma", InProgress),
        ],
    };
    let s = plan_summary_from_plan(plan);
    assert_eq!(s.current_step_title.as_deref(), Some("beta"));
}

#[test]
fn plan_summary_none_when_all_done() {
    use crate::acp::state::PlanStepStatus::*;
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![step("a", "alpha", Done), step("b", "beta", Done)],
    };
    let s = plan_summary_from_plan(plan);
    assert_eq!(s.completed, 2);
    assert_eq!(s.total, 2);
    assert!(s.current_step_title.is_none());
}

#[test]
fn plan_summary_truncates_long_current_step_title() {
    use crate::acp::state::PlanStepStatus::*;
    let long_title: String = "x".repeat(120);
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![step("a", &long_title, Pending)],
    };
    let s = plan_summary_from_plan(plan);
    let t = s.current_step_title.unwrap();
    assert_eq!(t.chars().count(), 80);
    assert!(t.ends_with('…'));
}

#[test]
fn plan_summary_empty_steps_yields_zero_total() {
    let plan = crate::acp::state::Plan {
        plan_id: "p1".into(),
        version: 1,
        steps: vec![],
    };
    let s = plan_summary_from_plan(plan);
    assert_eq!(s.total, 0);
    assert_eq!(s.completed, 0);
    assert!(s.current_step_title.is_none());
}

// --- persist_session_update (the persist-first contract from #1589) ---
//
// The five session-mutation PATCH handlers route every write through
// this helper and only touch memory after it returns `Ok`, so disk and
// memory cannot diverge on a write failure. Full-handler coverage is
// impractical (AppState has no test constructor), so these lock the
// helper's two guarantees directly: a success durably writes, and every
// storage failure surfaces as `Err`.

#[test]
#[serial_test::serial]
fn rename_persistence_reports_missing_authoritative_row() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");
    let storage = Storage::new_unwatched("rename-missing").unwrap();

    let outcome = persist_rename_metadata(&storage, "missing-id", "New title", None, None).unwrap();
    assert_eq!(outcome, RenamePersistOutcome::Missing);
    assert!(
        storage.load().unwrap().is_empty(),
        "a missing row must not be synthesized by rename persistence"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn persist_session_update_writes_to_disk() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "persist-success";
    let storage = Storage::new_unwatched(profile).unwrap();
    let seed = make_test_instance();
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    let persist_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "test",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.base_branch_override = Some("release/x".to_string());
            }
        },
    )
    .await
    .expect("persist should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    let inst = reloaded.iter().find(|i| i.id == id).unwrap();
    assert_eq!(
        inst.base_branch_override.as_deref(),
        Some("release/x"),
        "mutation must be durable on disk"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn persist_session_update_surfaces_storage_error() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "persist-failure";
    // Make `sessions.json` a directory so the store's `read_to_string`
    // during `update` fails, forcing the write path to error.
    let dir = crate::session::get_profile_dir(profile).unwrap();
    std::fs::create_dir_all(dir.join("sessions.json")).unwrap();

    let result = persist_session_update(
        profile.to_string(),
        "test",
        crate::file_watch::FileWatchService::noop(),
        |_instances| {},
    )
    .await;
    assert!(result.is_err(), "a storage failure must surface as Err");
}

// Group edit (#1726): the persisted instance's group_path is the only
// thing that changes; the groups Vec is left alone (the group list is
// derived from instance group_path, exactly like create_session). Set
// and clear both round-trip to disk.
#[tokio::test]
#[serial_test::serial]
async fn group_edit_set_and_clear_round_trip_to_disk() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _ = crate::session::get_app_dir().expect("isolated app dir");

    let profile = "group-edit";
    let storage = Storage::new_unwatched(profile).unwrap();
    let seed = make_test_instance(); // seeded in "work/projects"
    let id = seed.id.clone();
    storage
        .update(|instances, _groups| {
            instances.push(seed.clone());
            Ok(())
        })
        .unwrap();

    // Move to a brand-new group.
    let set_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "group update",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == set_id) {
                apply_session_group(inst, "team/alpha".to_string());
            }
        },
    )
    .await
    .expect("set should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    assert_eq!(
        reloaded.iter().find(|i| i.id == id).unwrap().group_path,
        "team/alpha",
        "group must move to the new path on disk"
    );

    // Clear to ungrouped via the empty-string sentinel.
    let clear_id = id.clone();
    persist_session_update(
        profile.to_string(),
        "group update",
        crate::file_watch::FileWatchService::noop(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == clear_id) {
                apply_session_group(inst, String::new());
            }
        },
    )
    .await
    .expect("clear should succeed");

    let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
    assert_eq!(
        reloaded.iter().find(|i| i.id == id).unwrap().group_path,
        "",
        "empty string must clear the group on disk"
    );
}

// --- #2066: web-API on_create hook trust + execution ---

/// Write `.agent-of-empires/config.toml` with the given `on_create` hooks
/// into a fresh project dir. Returns the dir so the caller keeps it alive.
fn project_with_on_create_hooks(commands: &[&str]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let cfg_dir = project.path().join(".agent-of-empires");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let list = commands
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!("[hooks]\non_create = [{list}]\n"),
    )
    .unwrap();
    project
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_refuses_untrusted_repo_hooks() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = project_with_on_create_hooks(&["bash scripts/setup-worktree.sh"]);
    // Approval trusts the whole hooks hash, so the refusal must surface
    // every hook type, not just on_create.
    std::fs::write(
        project.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"bash scripts/setup-worktree.sh\"]\non_launch = [\"npm start\"]\non_destroy = [\"rm -rf /tmp/seed\"]\n",
    )
    .unwrap();

    let err = resolve_create_hook_plan(
        "default",
        &crate::session::resolve_config("default").unwrap().hooks,
        project.path(),
        false,
        None,
        None,
    )
    .expect_err("untrusted hooks must be refused");
    let needs_trust = err
        .downcast_ref::<HooksNeedTrust>()
        .expect("error must be HooksNeedTrust");
    assert_eq!(
        needs_trust.on_create,
        vec!["bash scripts/setup-worktree.sh".to_string()],
        "the refused error must carry the commands for the prompt"
    );
    assert_eq!(
        needs_trust.on_launch,
        vec!["npm start".to_string()],
        "approval also trusts on_launch, so the prompt must show it"
    );
    assert_eq!(needs_trust.on_destroy, vec!["rm -rf /tmp/seed".to_string()]);
    assert!(!needs_trust.needs_mcp_trust);
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_distinguishes_approval_from_skip() {
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = project_with_on_create_hooks(&["echo hi"]);
    let base = crate::session::HooksConfig {
        on_create: vec!["echo global".into()],
        ..Default::default()
    };
    let skipped =
        resolve_create_hook_plan("default", &base, project.path(), false, Some(false), None)
            .unwrap();
    assert_eq!(skipped.on_create(), vec!["echo global"]);
    assert!(skipped.trust_write.is_none());

    assert!(
        resolve_create_hook_plan("default", &base, project.path(), false, Some(true), None)
            .is_err()
    );
    let trust = crate::session::config::repo_config::check_repo_trust(project.path()).unwrap();
    let review = crate::session::config::repo_config::creation_trust_fingerprint(&base, &trust);
    let plan = resolve_create_hook_plan(
        "default",
        &base,
        project.path(),
        false,
        Some(true),
        Some(&review),
    )
    .expect("a matching reviewed fingerprint approves trust");
    assert_eq!(plan.on_create(), vec!["echo hi".to_string()]);
    let (hooks_hash, mcp_hash) = plan
        .trust_write
        .expect("a newly-approved repo must record trust");
    assert!(hooks_hash.is_some(), "hooks hash must be recorded");
    assert!(mcp_hash.is_none(), "no .mcp.json means no mcp hash");

    crate::session::config::repo_config::trust_repo(
        project.path(),
        hooks_hash.as_deref(),
        mcp_hash.as_deref(),
    )
    .unwrap();
    let plan2 =
        resolve_create_hook_plan("default", &base, project.path(), false, Some(false), None)
            .expect("already-trusted hooks must run without trust_hooks");
    assert_eq!(plan2.on_create(), vec!["echo hi".to_string()]);
    assert!(
        plan2.trust_write.is_none(),
        "already-trusted repo needs no new trust record"
    );
    std::fs::write(
        project.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"echo changed\"]\n",
    )
    .unwrap();
    let changed =
        resolve_create_hook_plan("default", &base, project.path(), false, Some(false), None)
            .unwrap();
    assert_eq!(changed.on_create(), vec!["echo global"]);
    assert!(changed.trust_write.is_none());
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_absent_hooks_is_ok() {
    // A repo with no hooks (and no global hooks) is never refused.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = tempfile::tempdir().unwrap();

    let plan = resolve_create_hook_plan(
        "default",
        &crate::session::resolve_config("default").unwrap().hooks,
        project.path(),
        false,
        None,
        None,
    )
    .expect("no hooks means no trust needed");
    assert!(plan.on_create().is_empty());
    assert!(plan.trust_write.is_none());
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_scratch_skips_repo_trust() {
    // Scratch sessions have no repo config anchor; even pointing at a path
    // with untrusted hooks must not refuse (matches the CLI scratch branch).
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = project_with_on_create_hooks(&["echo nope"]);

    let plan = resolve_create_hook_plan(
        "default",
        &crate::session::resolve_config("default").unwrap().hooks,
        project.path(),
        true,
        None,
        None,
    )
    .expect("scratch must skip the repo trust check");
    assert!(
        plan.on_create().is_empty(),
        "no global hooks, so scratch resolves to nothing"
    );
    assert!(plan.trust_write.is_none());
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_does_not_block_on_untrusted_mcp_without_hooks() {
    // A repo with an untrusted `.mcp.json` but no hooks must NOT be refused:
    // the supervisor gates MCP at spawn, so blocking creation here would be
    // stricter than the CLI. The session is created with MCP left untrusted.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        r#"{"mcpServers": {"foo": {"command": "echo"}}}"#,
    )
    .unwrap();

    let plan = resolve_create_hook_plan(
        "default",
        &crate::session::resolve_config("default").unwrap().hooks,
        project.path(),
        false,
        None,
        None,
    )
    .expect("untrusted MCP without hooks must not block creation");
    assert!(plan.on_create().is_empty());
    assert!(
        plan.trust_write.is_none(),
        "MCP is left untrusted when the caller did not opt in"
    );
}

#[test]
#[serial_test::serial]
fn resolve_hook_plan_inherits_trust_across_worktrees() {
    // Secondary half of #2066: hook trust is keyed on the main repo
    // (check_repo_trust resolves a worktree path back to it), so a worktree
    // created from an already-trusted repo inherits that trust without a
    // fresh prompt, even with trust_hooks: false.
    let temp_home = tempfile::tempdir().unwrap();
    let _home = crate::session::test_support::isolate_app_dir_at(temp_home.path());
    let _app_dir = crate::session::get_app_dir().expect("isolated app dir");

    let parent = tempfile::Builder::new()
        .prefix("aoe-test-")
        .tempdir()
        .unwrap();
    let root = parent.path().join("proj");
    std::fs::create_dir(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    std::fs::create_dir_all(root.join(".agent-of-empires")).unwrap();
    std::fs::write(
        root.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"echo wt\"]\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "proj\n").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("README.md")).unwrap();
        index.write_tree().unwrap()
    };
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    // Trust the main repo at its current hooks hash.
    let hooks = crate::session::config::repo_config::load_repo_config(&root)
        .unwrap()
        .and_then(|rc| rc.hooks())
        .unwrap();
    let hash = crate::session::config::repo_config::compute_hooks_hash(&hooks);
    crate::session::config::repo_config::trust_repo(&root, Some(&hash), None).unwrap();

    // A worktree of that repo inherits the trust.
    let main_wt = crate::git::GitWorktree::new(root.clone()).unwrap();
    let wt_path = parent.path().join("proj-wt");
    main_wt
        .create_worktree("wt-branch", &wt_path, true, None)
        .unwrap();

    let plan = resolve_create_hook_plan(
        "default",
        &crate::session::resolve_config("default").unwrap().hooks,
        &wt_path,
        false,
        None,
        None,
    )
    .expect("worktree must inherit the main repo's hook trust");
    assert_eq!(plan.on_create(), vec!["echo wt".to_string()]);
    assert!(
        plan.trust_write.is_none(),
        "inherited trust needs no new record"
    );
}
#[tokio::test]
async fn list_sessions_projects_pending_approvals_only_for_running_workers() {
    use crate::acp::permissions::build_approval;
    use crate::acp::state::ToolCall;
    use crate::acp::Event;

    let mut inst = Instance::new("pending-approval", "/tmp/pending-approval");
    inst.id = "pending-approval".to_string();
    inst.view = crate::session::View::Structured;
    let id = inst.id.clone();
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    let approval = build_approval(
        ToolCall {
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            kind: "execute".to_string(),
            args_preview: r#"{"command":"echo hello"}"#.to_string(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        },
        Vec::new(),
    );
    let nonce = approval.nonce.0.clone();
    state
        .acp_event_store
        .record(&id, 1, &Event::ApprovalRequested { approval })
        .expect("record pending approval");

    // A durable nonce without a live worker cannot be resolved.
    let response = project_sessions(&state).await;
    assert!(
        response[0].pending_approvals.is_empty(),
        "a pending approval on a non-running worker must not be projected"
    );

    state.acp_supervisor.test_insert_worker(&id).await;
    let response = project_sessions(&state).await;
    assert_eq!(
        response[0].pending_approvals,
        vec![PendingApproval {
            nonce,
            tool_name: "shell".to_string(),
            target: "echo hello".to_string(),
            destructive: false,
            choice: false,
        }]
    );
}

// A worktree session's project_path is its checkout, so the override must be keyed by the main repo.
#[tokio::test]
#[serial_test::serial]
async fn list_sessions_applies_project_smart_rename_override_to_worktree_sessions() {
    let tmp_home = tempfile::tempdir().expect("tempdir HOME");
    let _home = crate::session::test_support::isolate_app_dir_at(tmp_home.path());
    let repo = tempfile::tempdir().expect("repo");
    let checkout = tempfile::tempdir().expect("worktree checkout");
    crate::session::projects::add(
        "default",
        crate::session::ProjectScope::Global,
        crate::session::Project::new(
            "demo",
            repo.path().to_string_lossy(),
            crate::session::ProjectScope::Global,
        )
        .with_overrides(crate::session::ProjectOverrides {
            smart_rename: Some(false),
            ..Default::default()
        }),
        false,
    )
    .unwrap();

    let mk = |path: &std::path::Path| {
        let mut inst = Instance::new("Vikings", path.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.source_profile = "default".to_string();
        inst.view = crate::session::View::Structured;
        inst
    };
    let mut in_worktree = mk(checkout.path());
    in_worktree.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feat".to_string(),
        main_repo_path: repo.path().to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    // Same checkout without the worktree link: unregistered, so it stays eligible.
    let unregistered = mk(checkout.path());

    let state = crate::server::test_support::build_test_app_state(vec![
        mk(repo.path()),
        in_worktree,
        unregistered,
    ]);
    let resp = list_sessions(
        axum::extract::State(state),
        axum::extract::Query(ListSessionsQuery { state: None }),
    )
    .await
    .into_response();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let states: Vec<&str> = envelope["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["smart_rename"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["inactive", "inactive", "pending"]);
}

#[test]
fn native_runtime_frame_stays_readable_with_a_valid_full_text_queue() {
    use crate::daemon::{
        QueuedPromptEntry, RuntimeCapabilities, RuntimeContents, RuntimeCursor, RuntimeFrame,
        RuntimeHealth, RuntimeSnapshot,
    };

    let mut instance = Instance::new("queued", "/tmp/queued");
    instance.queued_prompts = (0..64)
        .map(|seq| QueuedPromptEntry {
            id: format!("prompt-{seq}"),
            seq,
            text: "a".repeat(256 * 1024),
            attachments: Vec::new(),
            created_at: "2026-01-01T00:00:00Z".into(),
            origin_device: None,
        })
        .collect();
    let snapshot = RuntimeSnapshot {
        cursor: RuntimeCursor {
            epoch: "epoch".into(),
            revision: 1,
        },
        contents: RuntimeContents {
            health: RuntimeHealth::Healthy,
            capabilities: RuntimeCapabilities {
                mutations: true,
                native_interaction: true,
            },
            default_profile: "default".into(),
            sessions: vec![SessionResponse::from_instance(&instance, false)],
            profiles: Vec::new(),
            workspace_ordering: Vec::new(),
            global_projects: Vec::new(),
        },
    };
    let frame = serde_json::to_vec(&RuntimeFrame::Snapshot(&snapshot)).unwrap();
    assert!(
        frame.len() < 16 * 1024 * 1024,
        "native WS frame is {} bytes",
        frame.len()
    );
}

/// A dirty checkout survives while an outside session uses it; deleting all
/// its users rejects the dirty owner before touching a sibling.
#[tokio::test]
#[serial_test::serial]
async fn permanent_delete_keeps_a_worktree_a_surviving_session_uses() {
    use axum::body::to_bytes;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    // (workspace endpoint, dirty, survivor also selected)
    for (workspace_endpoint, dirty, both_selected) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, true, true),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("home"));
        crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let main_repo = tmp.path().join("main");
        let checkout = tmp.path().join("shared");
        std::fs::create_dir_all(&main_repo).unwrap();
        git(&main_repo, &["init", "-b", "main"]);
        git(&main_repo, &["commit", "--allow-empty", "-m", "init"]);
        git(
            &main_repo,
            &["worktree", "add", "-b", "feat", checkout.to_str().unwrap()],
        );
        std::fs::write(checkout.join("untracked.txt"), b"survivor data").unwrap();

        let profile = "shared-worktree-4084";
        let mk = |title: &str, managed: bool| {
            let mut inst = Instance::new(title, checkout.to_str().unwrap());
            inst.source_profile = profile.to_string();
            inst.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feat".into(),
                main_repo_path: main_repo.to_string_lossy().into_owned(),
                managed_by_aoe: managed,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            inst
        };
        let owner = mk("owner", true);
        let peer = mk("survivor", false);
        let rows = vec![owner.clone(), peer.clone()];
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _groups| {
                instances.extend(rows.clone());
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(rows);
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)
                .unwrap()
                .metadata;
        if dirty {
            std::fs::write(checkout.join("wip.txt"), "unsaved").unwrap();
        }
        let mut session_ids = vec![owner.id.clone()];
        if both_selected {
            session_ids.push(peer.id.clone());
        }

        let resp = if workspace_endpoint {
            delete_workspace(
                State(state.clone()),
                Some(Json(DeleteWorkspaceBody {
                    session_ids,
                    delete_worktree: true,
                    delete_branch: true,
                    ..Default::default()
                })),
            )
            .await
            .into_response()
        } else {
            delete_session(
                State(state.clone()),
                Path(owner.id.clone()),
                Some(Json(DeleteSessionBody {
                    delete_worktree: true,
                    delete_branch: true,
                    ..Default::default()
                })),
            )
            .await
            .into_response()
        };
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let case = format!("endpoint {workspace_endpoint}, dirty {dirty}: {body}");
        if both_selected {
            assert_eq!(status, StatusCode::CONFLICT, "{case}");
            assert_eq!(body["error"], "dirty_worktree", "{case}");
            assert_eq!(storage.load().unwrap().len(), 2, "{case}");
            continue;
        }
        assert_eq!(status, StatusCode::OK, "{case}");
        assert!(
            body["messages"].to_string().contains("another session"),
            "the kept worktree must be reported: {body}"
        );

        assert!(
            checkout.join(".git").exists(),
            "shared worktree was removed: {case}"
        );
        assert_eq!(checkout.join("wip.txt").exists(), dirty, "{case}");
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "feat"])
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(!branches.stdout.is_empty(), "shared branch was deleted");
        let stored: Vec<String> = storage.load().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(stored, vec![peer.id.clone()]);
        assert!(state
            .instances
            .read()
            .await
            .iter()
            .all(|i| i.id != owner.id));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn purges_finish_when_request_is_cancelled() {
    for workspace in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("home"));
        crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "workspace-disconnect";
        let mut instance = Instance::new("cancelled-request", tmp.path().to_str().unwrap());
        instance.source_profile = profile.to_string();
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)
                .unwrap()
                .metadata;

        let lease = state
            .runtime
            .purge_namespace_lease(&state.profile_namespace)
            .await;
        let mut request = Box::pin(async {
            if workspace {
                delete_workspace(
                    State(state.clone()),
                    Some(Json(DeleteWorkspaceBody {
                        session_ids: vec![instance.id.clone()],
                        ..Default::default()
                    })),
                )
                .await
                .into_response()
            } else {
                delete_session(
                    State(state.clone()),
                    Path(instance.id.clone()),
                    Some(Json(DeleteSessionBody::default())),
                )
                .await
                .into_response()
            }
        });
        let first_poll = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(request.as_mut(), cx))
        })
        .await;
        assert!(
            first_poll.is_pending(),
            "purge must wait for the namespace lease"
        );
        drop(request);
        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if storage.load().unwrap().is_empty()
                    && state
                        .instances
                        .read()
                        .await
                        .iter()
                        .all(|row| row.id != instance.id)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{workspace}: deletion must finish after request cancellation"));
    }
}
