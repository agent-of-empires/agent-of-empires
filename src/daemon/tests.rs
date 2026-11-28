use super::*;

#[tokio::test]
async fn native_admission_preserves_degraded_reads_without_relaxing_identity() {
    let root = tempfile::tempdir().unwrap();
    let daemon = root.path().join("daemon");
    std::fs::create_dir(&daemon).unwrap();
    let path = daemon.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let info = std::sync::Arc::new(tokio::sync::RwLock::new(RuntimeInfo {
        owner_diagnostics: Vec::new(),
        owner_instance_id: None,
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        app_version: None,
        cli_creation: true,
        epoch: "native-epoch".into(),
        namespace: root.path().to_str().unwrap().into(),
        tmux_socket: None,
        local_owner: true,
        profiles: vec!["healthy".into()],
        read_only: false,
        cityhall_mode: false,
        health: RuntimeHealth::Degraded {
            code: ReloadFailureCode::ProfileData,
            profiles: vec!["damaged".into()],
        },
        interaction_capabilities: RuntimeCapabilities {
            mutations: false,
            native_interaction: false,
        },
    }));
    let served_info = info.clone();
    let router = axum::Router::new().route(
        "/api/runtime",
        axum::routing::get(move || {
            let info = served_info.clone();
            async move { axum::Json(info.read().await.clone()) }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = local_unix_client(path).unwrap();
    assert!(local_daemon_ready(&client, std::process::id())
        .await
        .unwrap());
    for profile in ["healthy", "damaged"] {
        let admitted = local_runtime_ready(&client, Some(profile)).await.unwrap();
        assert!(matches!(admitted.health, RuntimeHealth::Degraded { .. }));
        assert!(!admitted.interaction_capabilities.mutations);
        assert!(!admitted.interaction_capabilities.native_interaction);
    }
    assert!(matches!(
        local_daemon_ready(&client, std::process::id() + 1).await,
        Err(DaemonClientError::PeerIdentity)
    ));
    info.write().await.protocol_version = RUNTIME_PROTOCOL_VERSION + 1;
    assert!(matches!(
        local_runtime_ready(&client, None).await,
        Err(LocalRuntimeError::Protocol { expected, actual })
            if expected == RUNTIME_PROTOCOL_VERSION && actual == RUNTIME_PROTOCOL_VERSION + 1
    ));
    info.write().await.protocol_version = RUNTIME_PROTOCOL_VERSION;
    info.write().await.namespace = "different-namespace".into();
    assert!(matches!(
        local_runtime_ready(&client, None).await,
        Err(LocalRuntimeError::Identity)
    ));
    info.write().await.namespace = root.path().to_str().unwrap().into();
    info.write().await.health = RuntimeHealth::Healthy;
    assert!(matches!(
        local_runtime_ready(&client, Some("missing")).await,
        Err(LocalRuntimeError::ProfileUnavailable(profile)) if profile == "missing"
    ));
    server.abort();
    let _ = server.await;
}

#[test]
fn managed_resume_recovery_commands_parse() {
    use clap::CommandFactory;

    let mut recovery_command = crate::cli::Cli::command();
    for cause in [
        ManagedResumeCause::PiTranscriptBindingRequired,
        ManagedResumeCause::OmpTranscriptBindingRequired,
    ] {
        let message = cause.message();
        let mut rebind = false;
        let mut fresh = false;
        for line in message
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("aoe "))
        {
            let argv = shell_words::split(line).expect("recovery command quoting");
            let parsed = recovery_command
                .try_get_matches_from_mut(argv)
                .expect("recovery command must parse");
            let args = parsed
                .subcommand_matches("session")
                .and_then(|args| args.subcommand_matches("set-session-id"))
                .expect("recovery command must select a native conversation");
            let sid = args.get_one::<String>("session_id").unwrap();
            let store = args.get_one::<std::path::PathBuf>("store");
            if sid.is_empty() {
                assert!(store.is_none(), "{message}");
                fresh = true;
            } else {
                assert!(store.is_some_and(|path| path.is_absolute()), "{message}");
                rebind = true;
            }
        }
        assert!(rebind && fresh, "{message}");
    }
}

#[test]
fn persisted_prompts_default_unknown_origins() {
    use crate::acp::state::Event;

    for extra in ["", r#", "origin": "from_a_newer_daemon""#] {
        let event: Event =
            serde_json::from_str(&format!(r#"{{"UserPromptSent":{{"text":"hi"{extra}}}}}"#))
                .unwrap();
        assert!(matches!(
            event,
            Event::UserPromptSent {
                origin: PromptOrigin::Unknown,
                ..
            }
        ));
    }
}
