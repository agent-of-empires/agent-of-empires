//! Real daemon lifecycle and serve-dialog flows.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Resolve the daemon's PID file inside the harness's isolated home.
/// Mirrors `crate::session::get_app_dir`'s platform split.
fn daemon_pid_path(h: &TuiTestHarness) -> PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("serve.pid")
}

/// Resolve the daemon's persisted launch-state file inside the harness's
/// isolated home.
fn daemon_launch_path(h: &TuiTestHarness) -> PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("serve.launch")
}

/// True iff the kernel still has a process with this PID.
fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[test]
#[parallel]
fn tui_disconnect_hides_sessions_and_explicit_reconnect_restores_them() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("offline_reconnect");
    h.stop_daemon_on_drop();
    let row = agent_of_empires::session::Instance::new(
        "offline-visible-session",
        h.project_path().to_str().unwrap(),
    );
    let app = crate::harness::app_dir_in(h.home_path());
    std::fs::write(
        app.join("profiles/default/sessions.json"),
        serde_json::to_vec(&[row]).unwrap(),
    )
    .unwrap();
    h.spawn_tui();
    h.wait_for("offline-visible-session");
    let stopped = h.run_cli(&["serve", "--stop"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    h.wait_for("Runtime unavailable");
    assert!(!h.capture_screen().contains("offline-visible-session"));
    h.send_keys("n");
    assert!(h.capture_screen().contains("Runtime unavailable"));
    h.send_keys("r");
    h.wait_for_timeout("offline-visible-session", Duration::from_secs(30));
    assert!(!h.capture_screen().contains("Runtime unavailable"));
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn daemon_group_move_preserves_live_terminal_and_restarts_in_target_profile() {
    use agent_of_empires::{
        daemon::{
            CreateProfileBody, DaemonClient, GroupLocation, MoveGroupBody, ProfileMutation,
            RuntimeSnapshot, SessionMutation, StartSessionBody,
        },
        session::{Group, Instance, Status},
    };
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("native_group_move");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let bin = h.install_path_command("group-agent");
    let agent = bin.join("group-agent");
    std::fs::write(&agent, "#!/bin/sh\nwhile true; do sleep 1; done\n").unwrap();
    let app = crate::harness::app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[session.custom_agents]\ngroup-agent = {}\n",
        serde_json::to_string(&agent.to_str().unwrap()).unwrap()
    ));
    std::fs::write(config_path, config).unwrap();
    let mut row = Instance::new("group live", h.project_path().to_str().unwrap());
    row.tool = "group-agent".into();
    row.command = agent.to_string_lossy().into_owned();
    row.status = Status::Stopped;
    row.group_path = "team/sub".into();
    let id = row.id.clone();
    std::fs::write(
        app.join("profiles/default/sessions.json"),
        serde_json::to_vec(&[row]).unwrap(),
    )
    .unwrap();
    let mut team = Group::new("team", "team");
    team.collapsed = true;
    std::fs::write(
        app.join("profiles/default/groups.json"),
        serde_json::to_vec(&[team]).unwrap(),
    )
    .unwrap();
    let migrated = h.run_cli(&["migrate"]);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    std::fs::write(app.join(".schema_version"), b"30").unwrap();
    std::fs::remove_file(app.join("pending-purge-owners.json")).unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let sdk = DaemonClient::new_unix(app.join("daemon/api.sock")).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    sdk.mutate_profile(
        &ProfileMutation::Create(CreateProfileBody {
            name: "destination".into(),
        }),
        &epoch,
    )
    .await
    .unwrap();
    sdk.mutate_session(
        &id,
        &SessionMutation::Start(StartSessionBody::default()),
        &epoch,
    )
    .await
    .unwrap();
    let panes = || {
        let output = std::process::Command::new("tmux")
            .arg("-S")
            .arg(h.home_path().join("tmux.sock"))
            .args([
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_pid} #{pane_current_path}",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let original_panes = panes();
    let http = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    sdk.create_group(
        &GroupLocation {
            profile: "default".into(),
            path: "empty".into(),
        },
        &epoch,
    )
    .await
    .unwrap();
    for _ in 0..2 {
        sdk.collapse_group(
            &agent_of_empires::daemon::CollapseGroupBody {
                group: GroupLocation {
                    profile: "default".into(),
                    path: "empty".into(),
                },
                collapsed: true,
            },
            &epoch,
        )
        .await
        .unwrap();
    }
    for (source_profile, source_path, target_profile, target_path) in [
        ("default", "team", "destination", "moved"),
        ("destination", "moved", "destination", "renamed"),
        ("default", "empty", "destination", "empty moved"),
    ] {
        let receipt = sdk
            .move_group(
                &MoveGroupBody {
                    source: GroupLocation {
                        profile: source_profile.into(),
                        path: source_path.into(),
                    },
                    target: GroupLocation {
                        profile: target_profile.into(),
                        path: target_path.into(),
                    },
                },
                &epoch,
            )
            .await
            .unwrap();
        let snapshot: RuntimeSnapshot = http
            .get("http://localhost/api/runtime/snapshot")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot.cursor.epoch, receipt.epoch);
        assert!(snapshot.cursor.revision >= receipt.revision);
        let moved = snapshot
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(moved.profile, "destination");
        assert_eq!(
            moved.group_path,
            if source_path == "team" {
                "moved/sub"
            } else {
                "renamed/sub"
            }
        );
        assert!(snapshot
            .contents
            .profiles
            .iter()
            .find(|profile| profile.name == target_profile)
            .unwrap()
            .groups
            .iter()
            .any(|group| group.path == target_path && group.collapsed));
        assert_eq!(panes(), original_panes);
    }
    let rows: Vec<Instance> = serde_json::from_slice(
        &std::fs::read(app.join("profiles/destination/sessions.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rows.iter().find(|row| row.id == id).unwrap().group_path,
        "renamed/sub"
    );
    sdk.mutate_session(&id, &SessionMutation::Stop, &epoch)
        .await
        .unwrap();
    sdk.mutate_session(
        &id,
        &SessionMutation::Start(StartSessionBody::default()),
        &epoch,
    )
    .await
    .unwrap();
    let restarted = panes();
    assert!(String::from_utf8_lossy(&restarted).contains(h.project_path().to_str().unwrap()));
    for (path, mode, retains_session) in [
        (
            "renamed",
            agent_of_empires::daemon::DeleteGroupMode::KeepSessions,
            true,
        ),
        (
            "empty moved",
            agent_of_empires::daemon::DeleteGroupMode::EmptyOnly,
            false,
        ),
    ] {
        let receipt = sdk
            .delete_group(
                &agent_of_empires::daemon::DeleteGroupBody {
                    group: GroupLocation {
                        profile: "destination".into(),
                        path: path.into(),
                    },
                    mode,
                },
                &epoch,
            )
            .await
            .unwrap();
        let ids: Vec<_> = receipt
            .outcome
            .sessions
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            ids,
            if retains_session {
                vec![id.as_str()]
            } else {
                Vec::new()
            }
        );
        let snapshot: RuntimeSnapshot = http
            .get("http://localhost/api/runtime/snapshot")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot.cursor.epoch, receipt.cursor.epoch);
        assert!(snapshot.cursor.revision >= receipt.cursor.revision);
        let row = snapshot
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(row.profile, "destination");
        assert!(row.group_path.is_empty());
        let prefix = format!("{path}/");
        assert!(!snapshot
            .contents
            .profiles
            .iter()
            .find(|profile| profile.name == "destination")
            .unwrap()
            .groups
            .iter()
            .any(|group| group.path == path || group.path.starts_with(&prefix)));
        assert_eq!(panes(), restarted);
    }
    sdk.mutate_session(&id, &SessionMutation::Stop, &epoch)
        .await
        .unwrap();
    let purged = sdk
        .purge_session(
            &id,
            &agent_of_empires::daemon::DeleteSessionBody::default(),
            &epoch,
        )
        .await
        .unwrap();
    assert!(
        matches!(purged.outcome, agent_of_empires::daemon::PurgeOutcome::Deleted { cleanup_errors, .. } if cleanup_errors.is_empty())
    );
    let snapshot: RuntimeSnapshot = http
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot.cursor.epoch, purged.cursor.epoch);
    assert!(snapshot.cursor.revision >= purged.cursor.revision);
    assert!(!snapshot.contents.sessions.iter().any(|row| row.id == id));
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn native_creation_hooks_preserve_borrowed_scratch_resources() {
    use agent_of_empires::daemon::{
        ApiErrorCode, CreateSessionBody, CreationTrustRequest, DaemonClient, DaemonClientError,
        RUNTIME_EPOCH_HEADER,
    };
    use std::os::unix::fs::PermissionsExt;
    require_tmux!();
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Skipping test: curl unavailable");
        return;
    }
    let real_git = which::which("git").expect("git is available");
    for stage in [
        "on_create",
        "post_checkout",
        "checkout_failure",
        "invalid_target",
        "trust_before_git",
        "cancel_on_launch",
    ] {
        let mut h = TuiTestHarness::new_in_tmp("native_creation_resources");
        h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
        h.stop_daemon_on_drop();
        let bin = h.install_path_command("creation-agent");
        let agent = bin.join("creation-agent");
        std::fs::write(&agent, "#!/bin/sh\nexec sleep 120\n").unwrap();
        let app = crate::harness::app_dir_in(h.home_path());
        let config_path = app.join("config.toml");
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str(&format!(
            "\n[session.custom_agents]\ncreation-agent = {}\n",
            serde_json::to_string(agent.to_str().unwrap()).unwrap()
        ));
        std::fs::write(&config_path, &config).unwrap();
        let failed_checkout = h.home_path().join("failed-checkout-path");
        if stage == "checkout_failure" {
            let git = h.install_path_command("git").join("git");
            std::fs::write(&git, format!(
            "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = add ]; then mkdir -p \"$3\"; printf preserved > \"$3/foreign-file\"; printf '%s' \"$3\" > {failed_checkout:?}; echo checkout-refused >&2; exit 1; fi\nexec {real_git:?} \"$@\"\n"
        )).unwrap();
        }
        let daemon = h.run_cli(&["serve", "--core-only", "--daemon"]);
        assert!(
            daemon.status.success(),
            "{}",
            String::from_utf8_lossy(&daemon.stderr)
        );
        let socket = app.join("daemon/api.sock");
        let sdk = DaemonClient::new_unix(&socket).unwrap();
        let epoch = sdk.runtime_info().await.unwrap().epoch;
        let owner_body: CreateSessionBody = serde_json::from_value(serde_json::json!({
            "title":"scratch owner", "path":"", "tool":"creation-agent", "scratch":true,
            "profile":"default",
        }))
        .unwrap();
        let owner = sdk
            .create_session(&owner_body, &epoch)
            .await
            .unwrap()
            .outcome;
        let marker = PathBuf::from(&owner.project_path).join("borrowed-resource");
        std::fs::write(&marker, "must survive the owner purge").unwrap();
        if stage != "on_create" {
            let repo = git2::Repository::init(&owner.project_path).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .unwrap();
        }
        if stage == "invalid_target" {
            let blocked = h.home_path().join("not-a-directory");
            std::fs::write(&blocked, "preserve the obstruction").unwrap();
            config.push_str(&format!(
                "\n[worktree]\npath_template = {}\n",
                serde_json::to_string(blocked.join("checkout").to_str().unwrap()).unwrap()
            ));
            std::fs::write(&config_path, &config).unwrap();
        }
        let entered = h.home_path().join("launch-entered");
        let release = h.home_path().join("launch-release");
        if stage == "cancel_on_launch" {
            let command = format!("touch {entered:?}; i=0; while [ ! -f {release:?} ] && [ \"$i\" -lt 200 ]; do i=$((i+1)); sleep 0.05; done; test -f {release:?}");
            config.push_str(&format!(
                "\n[hooks]\non_launch = [{}]\n",
                serde_json::to_string(&command).unwrap()
            ));
            std::fs::write(&config_path, &config).unwrap();
        }
        let hook = h.home_path().join("purge-owner.sh");
        std::fs::write(&hook, format!(r#"#!/bin/sh
set -eu
curl --max-time 5 -fsS --unix-socket "{socket}" -H '{header}: {epoch}' -H 'Content-Type: application/json' -X DELETE --data '{{}}' http://localhost/api/sessions/{id} > /dev/null
"#, socket = socket.display(), header = RUNTIME_EPOCH_HEADER, id = owner.id)).unwrap();
        if stage == "on_create" {
            config.push_str(&format!(
                "\n[hooks]\non_create = [{}]\n",
                serde_json::to_string(&format!("sh {hook:?}")).unwrap()
            ));
            std::fs::write(&config_path, config).unwrap();
        } else if stage == "post_checkout" {
            let git_hook = PathBuf::from(&owner.project_path).join(".git/hooks/post-checkout");
            std::fs::copy(&hook, &git_hook).unwrap();
            std::fs::set_permissions(&git_hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let trust_seen = h.home_path().join("trust-seen");
        if stage == "trust_before_git" {
            let source = PathBuf::from(&owner.project_path);
            let repo_config = source.join(".agent-of-empires/config.toml");
            std::fs::create_dir_all(repo_config.parent().unwrap()).unwrap();
            std::fs::write(repo_config, "[hooks]\non_create = [\"true\"]\n").unwrap();
            let hooks = agent_of_empires::session::HooksConfig {
                on_create: vec!["true".into()],
                ..Default::default()
            };
            let hash = agent_of_empires::session::config::repo_config::compute_hooks_hash(&hooks);
            let trust = app.join("trusted_repos.toml");
            let git_hook = source.join(".git/hooks/post-checkout");
            std::fs::write(&git_hook, format!(r#"#!/bin/sh
if grep -Fq 'hooks_hash = "{hash}"' {trust:?}; then printf approved > {trust_seen:?}; else printf missing > {trust_seen:?}; fi
"#)).unwrap();
            std::fs::set_permissions(git_hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut borrower_body: CreateSessionBody = serde_json::from_value(serde_json::json!({
            "title":"scratch borrower", "path":owner.project_path, "tool":"creation-agent",
            "profile":"default",
            "worktree_enabled": stage != "on_create",
            "worktree_branch": (stage != "on_create").then_some("borrower"),
            "create_new_branch": stage != "on_create",
            "trust_hooks": (stage == "trust_before_git").then_some(true),
        }))
        .unwrap();
        if stage == "trust_before_git" {
            let source = PathBuf::from(&owner.project_path);
            let mcp_path = source.join(".mcp.json");
            let mcp = r#"{"mcpServers":{"local":{"command":"fixture-mcp","env":{"API_TOKEN":"secret-before-review"}},"remote":{"type":"http","url":"https://example.invalid/mcp","headers":{"Authorization":"secret-header"}}}}"#;
            std::fs::write(&mcp_path, mcp).unwrap();
            let request = CreationTrustRequest {
                path: owner.project_path.clone(),
                profile: Some("default".into()),
                scratch: false,
            };
            let review = sdk.review_creation_trust(&request, &epoch).await.unwrap();
            let wire = serde_json::to_string(&review).unwrap();
            assert!(
                wire.contains("API_TOKEN")
                    && wire.contains("Authorization")
                    && wire.contains("fixture-mcp")
            );
            assert!(!wire.contains("secret-before-review") && !wire.contains("secret-header"));
            assert!(!app.join("trusted_repos.toml").exists());
            borrower_body.trust_review = Some(review.fingerprint);
            std::fs::write(
                &mcp_path,
                mcp.replace("secret-before-review", "secret-after-review"),
            )
            .unwrap();
            assert!(matches!(sdk.create_session(&borrower_body, &epoch).await,
                Err(DaemonClientError::Status { code: Some(ApiErrorCode::CreationTrustChanged), body, .. }) if body.is_empty()));
            assert!(!app.join("trusted_repos.toml").exists());
            let repo = git2::Repository::open(&source).unwrap();
            assert!(repo
                .find_branch("borrower", git2::BranchType::Local)
                .is_err());
            let rows = sdk.list_sessions(None).await.unwrap().sessions;
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                [owner.id.as_str()]
            );
            borrower_body.trust_review = Some(
                sdk.review_creation_trust(&request, &epoch)
                    .await
                    .unwrap()
                    .fingerprint,
            );
        }
        let creation = if stage == "cancel_on_launch" {
            let cancel = async {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !entered.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                let rows = sdk.list_sessions(None).await.unwrap().sessions;
                let pending = rows.iter().find(|row| row.id != owner.id).unwrap();
                sdk.cancel_creation(&pending.id, &epoch).await.unwrap();
                std::fs::write(&release, "continue").unwrap();
            };
            let (result, ()) = tokio::join!(sdk.create_session(&borrower_body, &epoch), cancel);
            assert!(
                matches!(&result, Err(DaemonClientError::Status { code: Some(ApiErrorCode::CreationCancelled), body, .. }) if body.is_empty())
            );
            result
        } else {
            sdk.create_session(&borrower_body, &epoch).await
        };
        if matches!(
            stage,
            "checkout_failure" | "invalid_target" | "cancel_on_launch"
        ) {
            assert!(creation.is_err());
            if stage == "checkout_failure" {
                let checkout = std::fs::read_to_string(&failed_checkout).unwrap();
                assert_eq!(
                    std::fs::read(PathBuf::from(checkout).join("foreign-file")).unwrap(),
                    b"preserved"
                );
            }
            let repo = git2::Repository::open(&owner.project_path).unwrap();
            assert!(repo
                .find_branch("borrower", git2::BranchType::Local)
                .is_err());
            let rows = sdk.list_sessions(None).await.unwrap().sessions;
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                vec![owner.id.as_str()]
            );
            assert_eq!(
                std::fs::read_to_string(marker).unwrap(),
                "must survive the owner purge"
            );
            continue;
        }
        let borrower = creation.unwrap().outcome;
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "must survive the owner purge"
        );
        let rows = sdk.list_sessions(None).await.unwrap().sessions;
        if stage == "trust_before_git" {
            assert_eq!(std::fs::read(&trust_seen).unwrap(), b"approved");
            assert!(rows.iter().any(|row| row.id == owner.id));
        } else {
            assert!(!rows.iter().any(|row| row.id == owner.id));
        }
        assert!(rows.iter().any(|row| row.id == borrower.id));
    }
}

/// The rollback is durable before the create request is answered, and so must
/// the snapshot `GET /api/sessions` serves: the failure arm publishes
/// synchronously, so the very first read after the response is already clean.
/// A publish only requested by the rollback (the ~2s background interval) made
/// this window observable, which is why there is no sleep here: a wait would
/// hide exactly the regression this test names.
#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn cancelled_creation_is_gone_from_the_first_read_after_the_failure() {
    use agent_of_empires::daemon::{
        ApiErrorCode, CreateSessionBody, DaemonClient, DaemonClientError, RuntimeSnapshot,
    };
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("cancelled_creation_publish");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let agent = h
        .install_path_command("creation-agent")
        .join("creation-agent");
    std::fs::write(&agent, "#!/bin/sh\nexec sleep 120\n").unwrap();
    let app = crate::harness::app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    let entered = h.home_path().join("launch-entered");
    let release = h.home_path().join("launch-release");
    config.push_str(&format!(
        "\n[session.custom_agents]\ncreation-agent = {}\n",
        serde_json::to_string(agent.to_str().unwrap()).unwrap()
    ));
    std::fs::write(&config_path, &config).unwrap();

    let daemon = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        daemon.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon.stderr)
    );
    let socket = app.join("daemon/api.sock");
    let sdk = DaemonClient::new_unix(&socket).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let http = reqwest::Client::builder()
        .unix_socket(socket.clone())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let owner_body: CreateSessionBody = serde_json::from_value(serde_json::json!({
        "title":"rollback owner", "path":"", "tool":"creation-agent", "scratch":true,
        "profile":"default",
    }))
    .unwrap();
    let owner = sdk
        .create_session(&owner_body, &epoch)
        .await
        .unwrap()
        .outcome;
    let repo = git2::Repository::init(&owner.project_path).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();

    // The gate hook is configured only now: the owner creation above must
    // launch normally, and the borrower's launch is the one cancelled.
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    let gate = format!(
        "touch {entered:?}; i=0; while [ ! -f {release:?} ] && [ \"$i\" -lt 400 ]; do i=$((i+1)); sleep 0.05; done; test -f {release:?}"
    );
    config.push_str(&format!(
        "\n[hooks]\non_launch = [{}]\n",
        serde_json::to_string(&gate).unwrap()
    ));
    std::fs::write(&config_path, &config).unwrap();

    let body: CreateSessionBody = serde_json::from_value(serde_json::json!({
        "title":"rollback borrower", "path":owner.project_path, "tool":"creation-agent",
        "profile":"default", "worktree_enabled":true, "worktree_branch":"rollback-borrower",
        "create_new_branch":true,
    }))
    .unwrap();
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !entered.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the create reached its launch hooks");
        let rows = sdk.list_sessions(None).await.unwrap().sessions;
        let pending = rows.iter().find(|row| row.id != owner.id).unwrap();
        sdk.cancel_creation(&pending.id, &epoch).await.unwrap();
        std::fs::write(&release, "continue").unwrap();
    };
    let (result, ()) = tokio::join!(sdk.create_session(&body, &epoch), cancel);
    assert!(
        matches!(&result, Err(DaemonClientError::Status { code: Some(ApiErrorCode::CreationCancelled), body, .. }) if body.is_empty()),
        "expected a cancelled creation, got {result:?}"
    );

    // No sleep between the failure response and either read: the rollback is
    // already durable, so the snapshot that served it must be too.
    let rows = sdk.list_sessions(None).await.unwrap().sessions;
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec![owner.id.as_str()],
        "the rolled-back creation was still served by GET /api/sessions"
    );
    let snapshot: RuntimeSnapshot = http
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .contents
            .sessions
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec![owner.id.as_str()],
        "the runtime snapshot still advertises the rolled-back row"
    );
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn native_creation_progress_reports_phases_to_the_local_stream() {
    use agent_of_empires::{
        acp::client::DaemonEndpoint,
        daemon::{
            CreateSessionBody, CreationPhase, CreationProgress, DaemonClient, RuntimeConnection,
            RuntimeEvent,
        },
    };
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("native_creation_progress");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let bin = h.install_path_command("progress-agent");
    let agent = bin.join("progress-agent");
    std::fs::write(&agent, "#!/bin/sh\nexec sleep 120\n").unwrap();
    let app = crate::harness::app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let release = h.home_path().join("hook-release");
    // One line then block: the first output line must reach the stream even
    // though the hook never finishes on its own.
    let command = format!(
        "echo building dependency graph; i=0; while [ ! -f {release:?} ] && [ \"$i\" -lt 400 ]; do i=$((i+1)); sleep 0.05; done"
    );
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[session.custom_agents]\nprogress-agent = {}\n",
        serde_json::to_string(agent.to_str().unwrap()).unwrap()
    ));
    config.push_str(&format!(
        "\n[hooks]\non_create = [{}]\n",
        serde_json::to_string(&command).unwrap()
    ));
    std::fs::write(&config_path, &config).unwrap();
    let daemon = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        daemon.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon.stderr)
    );
    let socket = app.join("daemon/api.sock");
    let sdk = DaemonClient::new_unix(&socket).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let endpoint = DaemonEndpoint::local_unix(socket);
    let mut connection = RuntimeConnection::connect(&endpoint, None).await.unwrap();
    assert!(
        connection.creation_progress().is_empty(),
        "an idle daemon must not report progress"
    );
    let body: CreateSessionBody = serde_json::from_value(serde_json::json!({
        "title": "progress", "path": "", "tool": "progress-agent", "scratch": true,
        "profile": "default",
    }))
    .unwrap();

    async fn read_progress(
        connection: &mut RuntimeConnection,
        ready: impl Fn(&CreationProgress) -> bool,
    ) -> CreationProgress {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(20), connection.next_event())
                .await
                .expect("creation progress arrived")
                .expect("runtime stream stayed connected")
            {
                RuntimeEvent::Progress(progress) => {
                    if let Some(entry) = progress.iter().find(|entry| ready(entry)) {
                        return entry.clone();
                    }
                }
                RuntimeEvent::Snapshot(_) => {}
            }
        }
    }

    let creator = sdk.clone();
    let create_epoch = epoch.clone();
    let create = tokio::spawn(async move { creator.create_session(&body, &create_epoch).await });
    // The phase change publishes before the command starts, so wait for the
    // frame that actually names the hook and its output.
    let observed = read_progress(&mut connection, |entry| {
        entry.phase == CreationPhase::CreateHooks
            && !entry.cancelled
            && entry.command.is_some()
            && !entry.output.is_empty()
    })
    .await;
    assert_eq!(
        observed.command.as_deref(),
        Some(command.as_str()),
        "progress must name the running hook"
    );
    assert!(
        observed
            .output
            .iter()
            .any(|line| line.contains("building dependency graph")),
        "progress must carry the hook's first line, got: {:?}",
        observed.output
    );
    let pending = sdk
        .list_sessions(None)
        .await
        .unwrap()
        .sessions
        .into_iter()
        .find(|row| row.title == "progress")
        .expect("the reservation is published while provisioning");
    assert!(
        sdk.cancel_creation(&pending.id, &epoch).await.is_ok(),
        "a pending creation accepts cancellation"
    );
    let observed = read_progress(&mut connection, |entry| entry.cancelled).await;
    assert_eq!(
        observed.session_id, pending.id,
        "the pending cancellation must name the creation it fences"
    );
    std::fs::write(&release, "continue").unwrap();
    let cancelled = create.await.unwrap();
    assert!(
        cancelled.is_err(),
        "a cancelled creation must not report success"
    );
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(20), connection.next_event())
            .await
            .expect("progress cleared")
            .expect("runtime stream stayed connected")
        {
            RuntimeEvent::Progress(progress) if progress.is_empty() => break,
            _ => {}
        }
    }
    assert!(connection.creation_progress().is_empty());
    let rows = sdk.list_sessions(None).await.unwrap().sessions;
    assert!(
        !rows.iter().any(|row| row.id == pending.id),
        "the cancelled creation left its reservation behind"
    );
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn native_agent_hooks_can_reenter_without_releasing_launch_ownership() {
    use agent_of_empires::{
        daemon::{
            DaemonClient, RuntimeSnapshot, StartSessionBody, RUNTIME_EPOCH_HEADER,
            RUNTIME_REVISION_HEADER,
        },
        session::{Instance, Status},
    };
    require_tmux!();
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Skipping test: curl unavailable");
        return;
    }
    for operation in ["ensure", "start"] {
        let mut h = TuiTestHarness::new_in_tmp("native_agent_hooks");
        h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
        h.stop_daemon_on_drop();
        let bin = h.install_path_command("hook-agent");
        let agent = bin.join("hook-agent");
        let launched = h.home_path().join("agent-launched");
        std::fs::write(
            &agent,
            format!("#!/bin/sh\nprintf started >> {launched:?}\nexec sleep 120\n"),
        )
        .unwrap();
        let hook = h.home_path().join("reenter.sh");
        let completed = h.home_path().join("hook-completed");
        let app = crate::harness::app_dir_in(h.home_path());
        let config_path = app.join("config.toml");
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str(&format!(
            "\n[session.custom_agents]\nhook-agent = {}\n[hooks]\non_launch = [{}]\n",
            serde_json::to_string(&agent.to_str().unwrap()).unwrap(),
            serde_json::to_string(&format!("sh {hook:?}")).unwrap(),
        ));
        std::fs::write(config_path, config).unwrap();
        let mut row = Instance::new("hook launch", h.project_path().to_str().unwrap());
        row.tool = "hook-agent".into();
        row.command = agent.to_string_lossy().into_owned();
        row.status = Status::Stopped;
        let id = row.id.clone();
        std::fs::write(
            app.join("profiles/default/sessions.json"),
            serde_json::to_vec(&[row]).unwrap(),
        )
        .unwrap();
        let daemon = h.run_cli(&["serve", "--core-only", "--daemon"]);
        assert!(
            daemon.status.success(),
            "{}",
            String::from_utf8_lossy(&daemon.stderr)
        );
        let socket = app.join("daemon/api.sock");
        let sdk = DaemonClient::new_unix(&socket).unwrap();
        let epoch = sdk.runtime_info().await.unwrap().epoch;
        let http = reqwest::Client::builder()
            .unix_socket(socket.as_path())
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let terminal: serde_json::Value = http
            .post(format!("http://localhost/api/sessions/{id}/terminal"))
            .header(RUNTIME_EPOCH_HEADER, &epoch)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let terminal_name = terminal["tmux_session"].as_str().unwrap();
        std::fs::write(&hook, format!(r#"#!/bin/sh
set -eu
if [ -f "{completed}" ]; then exit 0; fi
curl --max-time 3 -fsS --unix-socket "{socket}" -H '{header}: {epoch}' -H 'Content-Type: application/json' --data '{{"name":"hook-created"}}' http://localhost/api/profiles > /dev/null
test "$(curl --max-time 3 -sS -o /dev/null -w '%{{http_code}}' --unix-socket "{socket}" -H '{header}: {epoch}' -X POST http://localhost/api/sessions/{id}/stop)" = 409
tmux -S "$AOE_TMUX_SOCKET" kill-session -t "={terminal_name}"
printf done > "{completed}"
"#, completed = completed.display(), socket = socket.display(), header = RUNTIME_EPOCH_HEADER)).unwrap();
        let response = http
            .post(format!("http://localhost/api/sessions/{id}/{operation}"))
            .header(RUNTIME_EPOCH_HEADER, &epoch)
            .json(&serde_json::json!({"size": {"cols": 137, "rows": 41}}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let receipt_epoch = response
            .headers()
            .get(RUNTIME_EPOCH_HEADER)
            .expect("missing publication epoch")
            .to_str()
            .unwrap()
            .to_owned();
        let receipt_revision: u64 = response
            .headers()
            .get(RUNTIME_REVISION_HEADER)
            .expect("missing publication revision")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let started_response: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&completed).expect("hook could not reenter the daemon"),
            "done"
        );
        if operation == "start" {
            assert_eq!(
                started_response["auxiliary"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["target"]["kind"] == "host" && item["target"]["index"] == 0)
                    .and_then(|item| item["state"].as_str()),
                Some("absent"),
                "start republished a pre-hook observation"
            );
        }
        let snapshot: RuntimeSnapshot = http
            .get("http://localhost/api/runtime/snapshot")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot.cursor.epoch, receipt_epoch);
        assert!(snapshot.cursor.revision >= receipt_revision);
        let started = snapshot
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(started.lifecycle_reservation.is_none());
        assert_eq!(
            started.agent_pane.state,
            agent_of_empires::session::PanePresence::Alive
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !launched.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "owner did not launch after its hook"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let ready = sdk
            .ensure_agent(&id, &StartSessionBody::default(), &epoch)
            .await
            .unwrap();
        assert_eq!(
            started.agent_pane.tmux_session.as_deref(),
            Some(ready.outcome.tmux_session.as_str())
        );
        let grid = std::process::Command::new("tmux")
            .arg("-S")
            .arg(h.home_path().join("tmux.sock"))
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("={}:^.0", ready.outcome.tmux_session),
                "#{pane_width}x#{pane_height}",
            ])
            .output()
            .unwrap();
        assert!(
            grid.status.success(),
            "{}",
            String::from_utf8_lossy(&grid.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&grid.stdout).trim(), "137x41");
        assert_eq!(ready.cursor.epoch, epoch);
        assert_eq!(
            std::fs::read_to_string(&launched).unwrap(),
            "started",
            "warm ensure relaunched the agent"
        );
        if operation == "ensure" {
            let first = agent_of_empires::tmux::Session::generate_name(&id, "first");
            let second = agent_of_empires::tmux::Session::generate_name(&id, "second");
            let tmux = |args: &[&str]| {
                let output = std::process::Command::new("tmux")
                    .arg("-S")
                    .arg(h.home_path().join("tmux.sock"))
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            tmux(&[
                "rename-session",
                "-t",
                &format!("={}:", ready.outcome.tmux_session),
                &first,
            ]);
            tmux(&["new-session", "-d", "-s", &second, "sleep 120"]);
            assert!(
                sdk.ensure_agent(&id, &StartSessionBody::default(), &epoch)
                    .await
                    .is_err(),
                "ambiguous existing agents admitted another launch"
            );
            assert_eq!(
                std::fs::read_to_string(&launched).unwrap(),
                "started",
                "ambiguous observation relaunched the agent"
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let observed: RuntimeSnapshot = http
                    .get("http://localhost/api/runtime/snapshot")
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let agent = &observed
                    .contents
                    .sessions
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .agent_pane;
                if agent.state == agent_of_empires::session::PanePresence::Unknown {
                    assert!(
                        agent.tmux_session.is_none(),
                        "unknown identity retained a transport target"
                    );
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "ambiguous Agent remained attachable: {agent:?}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            tmux(&["kill-session", "-t", &format!("={second}:")]);
            tmux(&[
                "rename-session",
                "-t",
                &format!("={first}:"),
                &ready.outcome.tmux_session,
            ]);
            assert!(h.run_cli(&["serve", "--stop"]).status.success());
            let readonly = h.run_cli(&["serve", "--core-only", "--read-only", "--daemon"]);
            assert!(
                readonly.status.success(),
                "{}",
                String::from_utf8_lossy(&readonly.stderr)
            );
            let readonly_epoch = sdk.runtime_info().await.unwrap().epoch;
            let viewing = sdk
                .ensure_agent(&id, &StartSessionBody::default(), &readonly_epoch)
                .await
                .unwrap();
            let killed = std::process::Command::new("tmux")
                .arg("-S")
                .arg(h.home_path().join("tmux.sock"))
                .args([
                    "kill-session",
                    "-t",
                    &format!("={}:", viewing.outcome.tmux_session),
                ])
                .output()
                .unwrap();
            assert!(
                killed.status.success(),
                "{}",
                String::from_utf8_lossy(&killed.stderr)
            );
            assert!(matches!(
                sdk.ensure_agent(&id, &StartSessionBody::default(), &readonly_epoch)
                    .await,
                Err(agent_of_empires::daemon::DaemonClientError::Status {
                    code: Some(agent_of_empires::daemon::ApiErrorCode::ReadOnly),
                    ..
                })
            ));
            assert_eq!(
                std::fs::read_to_string(&launched).unwrap(),
                "started",
                "read-only ensure revived the agent"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn daemon_trash_restore_purge_preserves_collisions_and_allows_retry() {
    use agent_of_empires::{
        daemon::RuntimeSnapshot,
        session::{Instance, Status, WorktreeInfo},
    };

    let mut h = TuiTestHarness::new_in_tmp("native_restore_collision");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let repo = h.project_path();
    let worktree = h.home_path().join("worktree");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=E2E",
                "-c",
                "user.email=e2e@example.invalid",
            ])
            .args(args)
            .env("HOME", h.home_path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    std::fs::write(repo.join("payload.txt"), "restore payload\n").unwrap();
    git(&["add", "payload.txt"]);
    git(&["commit", "-m", "fixture"]);
    git(&[
        "worktree",
        "add",
        "-b",
        "feature/restore",
        worktree.to_str().unwrap(),
    ]);
    let mut row = Instance::new("restore collision", worktree.to_str().unwrap());
    row.status = Status::Stopped;
    row.worktree_info = Some(WorktreeInfo {
        branch: "feature/restore".into(),
        main_repo_path: repo.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let id = row.id.clone();
    let app = crate::harness::app_dir_in(h.home_path());
    std::fs::write(
        app.join("profiles/default/sessions.json"),
        serde_json::to_vec(&[row]).unwrap(),
    )
    .unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let client = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("http://localhost/api/sessions/{id}");
    let sdk =
        agent_of_empires::daemon::DaemonClient::new_unix(app.join("daemon/api.sock")).unwrap();
    let initial: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let epoch = initial.cursor.epoch;
    let moved = sdk
        .trash_session(
            &id,
            &agent_of_empires::daemon::TrashSessionBody::default(),
            &epoch,
        )
        .await
        .unwrap();
    assert!(matches!(
        moved.outcome.relocation,
        agent_of_empires::daemon::TrashRelocationOutcome::Relocated
    ));
    let moved: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let holding = PathBuf::from(
        &moved
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .project_path,
    );
    assert!(!worktree.exists());
    assert_eq!(
        std::fs::read_to_string(holding.join("payload.txt")).unwrap(),
        "restore payload\n"
    );
    std::fs::create_dir(&worktree).unwrap();
    let sentinel = worktree.join("unrelated.txt");
    std::fs::write(&sentinel, "keep me").unwrap();
    let rejected = client.post(format!("{base}/restore")).send().await.unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::CONFLICT);
    assert!(!rejected
        .headers()
        .contains_key(agent_of_empires::daemon::RUNTIME_REVISION_HEADER));
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep me");
    let snapshot: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = snapshot
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert!(row.trashed_at.is_some());
    assert_eq!(std::path::Path::new(&row.project_path), holding);
    std::fs::remove_file(sentinel).unwrap();
    std::fs::remove_dir(&worktree).unwrap();
    let restored = client.post(format!("{base}/restore")).send().await.unwrap();
    assert_eq!(
        restored.status(),
        reqwest::StatusCode::OK,
        "{}",
        restored.text().await.unwrap()
    );
    let snapshot: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = snapshot
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert!(row.trashed_at.is_none());
    assert_eq!(row.status, Status::Stopped.wire_str());
    assert_eq!(std::path::Path::new(&row.project_path), worktree);
    assert!(!holding.exists());
    assert_eq!(
        std::fs::read_to_string(worktree.join("payload.txt")).unwrap(),
        "restore payload\n"
    );
    std::fs::create_dir(&holding).unwrap();
    let sentinel = holding.join("unrelated.txt");
    std::fs::write(&sentinel, "keep holding").unwrap();
    let response = client.post(format!("{base}/trash")).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let outcome: serde_json::Value = response.json().await.unwrap();
    assert_eq!(outcome["outcome"]["relocation"]["status"], "failed");
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep holding");
    assert_eq!(
        std::fs::read_to_string(worktree.join("payload.txt")).unwrap(),
        "restore payload\n"
    );
    let snapshot: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = snapshot
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert!(row.trashed_at.is_some());
    assert_eq!(std::path::Path::new(&row.project_path), worktree);
    use agent_of_empires::daemon::{
        DaemonClientError, DeleteSessionBody, PurgeOutcome, RuntimeFrame,
    };
    use futures_util::StreamExt;
    let stream = tokio::net::UnixStream::connect(app.join("daemon/api.sock"))
        .await
        .unwrap();
    let (mut updates, _) = tokio_tungstenite::client_async("ws://localhost/api/runtime/ws", stream)
        .await
        .unwrap();
    let dirty = worktree.join("dirty.txt");
    std::fs::write(&dirty, "preserve until forced").unwrap();
    let refused = sdk
        .purge_session(
            &id,
            &DeleteSessionBody {
                delete_worktree: true,
                ..Default::default()
            },
            &epoch,
        )
        .await
        .expect_err("dirty worktree purge must fail");
    assert!(
        matches!(refused, DaemonClientError::Status { status, .. } if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    );
    assert_eq!(
        std::fs::read_to_string(&dirty).unwrap(),
        "preserve until forced"
    );
    let retained: RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let retained = retained
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(retained.status, Status::Error.wire_str());
    assert!(retained.lifecycle_reservation.is_none());
    let receipt = sdk
        .purge_session(
            &id,
            &DeleteSessionBody {
                delete_worktree: true,
                delete_branch: true,
                force_delete: true,
                ..Default::default()
            },
            &epoch,
        )
        .await
        .unwrap();
    assert!(
        matches!(receipt.outcome, PurgeOutcome::Deleted { ref cleanup_errors, .. } if cleanup_errors.is_empty())
    );
    let reflected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = updates.next().await.unwrap().unwrap();
            let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                continue;
            };
            if let RuntimeFrame::Snapshot(snapshot) =
                serde_json::from_str::<RuntimeFrame>(&text).unwrap()
            {
                if snapshot.cursor.revision >= receipt.cursor.revision {
                    break snapshot;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reflected.cursor.epoch, receipt.cursor.epoch);
    assert!(!reflected.contents.sessions.iter().any(|row| row.id == id));
    assert!(!worktree.exists());
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep holding");
    assert_eq!(
        std::fs::read_to_string(repo.join("payload.txt")).unwrap(),
        "restore payload\n"
    );
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show-ref", "--verify", "refs/heads/feature/restore"])
        .output()
        .unwrap();
    assert!(!branch.status.success());
    updates.close(None).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn daemon_terminal_commands_apply_dimensions_archive_and_abandon_policy() {
    use agent_of_empires::session::{Instance, Status};

    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("native_start_dimensions");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let bin = h.install_path_command("terminal-agent");
    let agent = bin.join("terminal-agent");
    std::fs::write(&agent, "#!/bin/sh\nwhile true; do sleep 1; done\n").unwrap();

    struct ReleaseHook(std::path::PathBuf);
    impl Drop for ReleaseHook {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, []);
        }
    }
    let entered = h.home_path().join("purge-entered");
    let release = ReleaseHook(h.home_path().join("purge-release"));
    let finished = h.home_path().join("purge-finished");
    let hook = h.home_path().join("purge-hook.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\n: > '{}'\n",
            entered.display(),
            release.0.display(),
            finished.display()
        ),
    )
    .unwrap();
    let repo = h.project_path();
    let worktree = h.home_path().join("native-worktree");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=E2E",
                "-c",
                "user.email=e2e@example.invalid",
            ])
            .args(args)
            .env("HOME", h.home_path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    std::fs::write(repo.join("payload.txt"), "retained after abandon\n").unwrap();
    git(&["add", "payload.txt"]);
    git(&["commit", "-m", "fixture"]);
    git(&[
        "worktree",
        "add",
        "-b",
        "feature/native-terminal",
        worktree.to_str().unwrap(),
    ]);

    let app = crate::harness::app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[session.custom_agents]\nterminal-agent = {}\n",
        serde_json::to_string(&agent.to_str().unwrap()).unwrap()
    ));
    config.push_str(&format!(
        "\n[hooks]\non_destroy = [{}]\n",
        serde_json::to_string(&format!("sh '{}'", hook.display())).unwrap()
    ));
    std::fs::write(config_path, config).unwrap();
    let mut row = Instance::new("native dimensions", worktree.to_str().unwrap());
    row.tool = "terminal-agent".into();
    row.command = agent.to_string_lossy().into_owned();
    row.status = Status::Stopped;
    row.worktree_info = Some(agent_of_empires::session::WorktreeInfo {
        branch: "feature/native-terminal".into(),
        main_repo_path: repo.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let id = row.id.clone();
    std::fs::write(
        app.join("profiles/default/sessions.json"),
        serde_json::to_vec(&[row]).unwrap(),
    )
    .unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let client = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("http://localhost/api/sessions/{id}/start");
    let rejected = client
        .post(&url)
        .json(&serde_json::json!({"size": {"cols": 0, "rows": 49}}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let response = client
        .post(&url)
        .json(&serde_json::json!({"size": {"cols": 170, "rows": 49}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "{}",
        response.text().await.unwrap()
    );
    let panes = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{pane_pid} #{pane_width}x#{pane_height}",
        ])
        .output()
        .unwrap();
    assert!(
        panes.status.success(),
        "{}",
        String::from_utf8_lossy(&panes.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&panes.stdout)
            .split_whitespace()
            .last(),
        Some("170x49")
    );
    let archive_url = format!("http://localhost/api/sessions/{id}/archive");
    for (body, retained) in [
        (
            serde_json::json!({"archived": true, "kill_pane": false}),
            true,
        ),
        (serde_json::json!({"archived": true}), false),
    ] {
        let response = client.patch(&archive_url).json(&body).send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        for unarchive in [false, true] {
            if unarchive {
                let response = client
                    .patch(&archive_url)
                    .json(&serde_json::json!({"archived": false}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::OK,
                    "{}",
                    response.text().await.unwrap()
                );
            }
            let current = std::process::Command::new("tmux")
                .arg("-S")
                .arg(h.home_path().join("tmux.sock"))
                .args([
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_id} #{pane_pid} #{pane_width}x#{pane_height}",
                ])
                .output()
                .unwrap();
            assert_eq!(current.status.success(), retained, "unarchive={unarchive}");
            if retained {
                assert_eq!(current.stdout, panes.stdout);
            }
        }
    }

    let sdk =
        agent_of_empires::daemon::DaemonClient::new_unix(app.join("daemon/api.sock")).unwrap();
    let initial: agent_of_empires::daemon::RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let epoch = initial.cursor.epoch;
    sdk.mutate_session(
        &id,
        &agent_of_empires::daemon::SessionMutation::Start(Default::default()),
        &epoch,
    )
    .await
    .unwrap();
    let live = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args(["list-panes", "-a", "-F", "#{pane_id}"])
        .output()
        .unwrap();
    assert!(
        live.status.success(),
        "{}",
        String::from_utf8_lossy(&live.stderr)
    );
    let pane_id = String::from_utf8(live.stdout).unwrap().trim().to_string();
    let mut connection = tokio::net::UnixStream::connect(app.join("daemon/api.sock"))
        .await
        .unwrap();
    let body = serde_json::to_string(&agent_of_empires::daemon::DeleteSessionBody {
        delete_worktree: true,
        delete_branch: true,
        delete_sandbox: true,
        force_delete: true,
        keep_scratch: false,
    })
    .unwrap();
    let request = format!("DELETE /api/sessions/{id} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{}: {epoch}\r\nContent-Length: {}\r\n\r\n{body}",
        agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, body.len());
    tokio::io::AsyncWriteExt::write_all(&mut connection, request.as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("purge must enter the held on_destroy hook");
    drop(connection);
    let pending: agent_of_empires::daemon::RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reservation = pending
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap()
        .lifecycle_reservation
        .as_ref()
        .unwrap();
    assert_eq!(
        reservation.op,
        agent_of_empires::session::LifecycleOperation::Purge
    );
    let receipt = tokio::time::timeout(
        Duration::from_secs(5),
        sdk.mutate_session(
            &id,
            &agent_of_empires::daemon::SessionMutation::AbandonPurge(
                agent_of_empires::daemon::AbandonPurgeBody {
                    expected_generation: std::num::NonZeroU64::new(reservation.generation).unwrap(),
                },
            ),
            &epoch,
        ),
    )
    .await
    .expect("abandon must not wait for the disconnected purge")
    .unwrap();
    let reflected: agent_of_empires::daemon::RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt.epoch, reflected.cursor.epoch);
    assert!(reflected.cursor.revision >= receipt.revision);
    assert!(!reflected.contents.sessions.iter().any(|row| row.id == id));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = std::process::Command::new("tmux")
                .arg("-S")
                .arg(h.home_path().join("tmux.sock"))
                .args(["display-message", "-p", "-t", &pane_id, "#{pane_id}"])
                .output()
                .unwrap();
            if !current.status.success() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon-owned abandon cleanup must remove the real pane");
    assert!(
        !finished.exists(),
        "abandon must not claim that the original hook completed"
    );
    std::fs::write(&release.0, []).unwrap();
    let stopped = h.run_cli(&["serve", "--stop"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        finished.exists(),
        "shutdown must drain the disconnected purge"
    );
    git(&["show-ref", "--verify", "refs/heads/feature/native-terminal"]);
    assert_eq!(
        std::fs::read_to_string(worktree.join("payload.txt")).unwrap(),
        "retained after abandon\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("payload.txt")).unwrap(),
        "retained after abandon\n"
    );
    let restarted = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let client = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let after: agent_of_empires::daemon::RuntimeSnapshot = client
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(after.cursor.epoch, epoch);
    assert!(
        !after.contents.sessions.iter().any(|row| row.id == id),
        "the disconnected purge must not resurrect a row after abandon"
    );
}

/// Pressing `R` from the home screen opens the serve ModePicker,
/// which must render both cards (Local + Internet) and surface the
/// transport-picker-deferred hint on the Tunnel card ("Pick transport
/// on next screen.").
#[test]
#[parallel]
fn tui_serve_dialog_opens_to_mode_picker() {
    require_tmux!();

    let mut h = TuiTestHarness::new("serve_mode_picker");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("R");

    h.wait_for("How should this be reachable?");
    h.assert_screen_contains("Local network");
    h.assert_screen_contains("Internet (HTTPS)");
    // The Tunnel card defers the transport choice to the next screen.
    // If this line disappears, the ModePicker copy is out of sync with
    // the Confirm-screen picker it hands off to.
    h.assert_screen_contains("Pick transport on next screen.");
}

/// Esc dismisses the serve dialog and returns to the home screen
/// without spawning anything. Regression guard against state-transition
/// bugs where ModePicker might latch onto a stale mode.
#[test]
#[parallel]
fn tui_serve_dialog_escape_returns_home() {
    require_tmux!();

    let mut h = TuiTestHarness::new("serve_mode_picker_esc");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("R");
    h.wait_for("How should this be reachable?");

    h.send_keys("Escape");
    // Home-screen footer is the tell that we've returned.
    h.wait_for("No sessions yet");
}

/// `aoe serve --daemon` must spawn a child that actually binds the port and
/// stays alive. Regression guard for the self-detection bug where the parent
/// pre-wrote the child's PID into `serve.pid`, then the child re-entered
/// `run()`, found its own PID via `daemon_pid()`, and bailed with
/// "A serve daemon is already running" — about itself.
#[test]
#[parallel]
fn cli_serve_daemon_starts_and_stops_cleanly() {
    let h = TuiTestHarness::new("serve_daemon_lifecycle");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    let pid_path = daemon_pid_path(&h);
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {} (child likely self-detected and exited).\n\
         pid file exists: {}\n\
         debug.log:\n{}",
        port,
        pid_path.exists(),
        std::fs::read_to_string(pid_path.with_file_name("debug.log")).unwrap_or_default(),
    );

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid should exist after daemon starts")
        .trim()
        .parse()
        .expect("serve.pid should contain a valid integer");
    assert!(
        pid_alive(pid),
        "child PID {} not alive after port bind",
        pid
    );

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "aoe serve --stop failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(pid),
        "daemon PID {} still alive after --stop",
        pid
    );
    assert!(
        !pid_path.exists(),
        "serve.pid should be cleaned up after --stop, found at {}",
        pid_path.display()
    );
}
#[cfg(target_os = "linux")]
#[test]
#[parallel]
fn migration_contention_does_not_block_runtime_publication() {
    use fs2::FileExt;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("runtime_migration_contention");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let project = h.project_path();
    let added = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--title",
        "blocked-metadata",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let app = crate::harness::app_dir_in(h.home_path());
    let rows_path = app.join("profiles/default/sessions.json");
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&rows_path).unwrap()).unwrap();
    let row = rows
        .iter_mut()
        .find(|row| row["title"] == "blocked-metadata")
        .unwrap();
    let id = row["id"].as_str().unwrap().to_owned();
    row["status"] = serde_json::json!("stopped");
    std::fs::write(&rows_path, serde_json::to_vec(&rows).unwrap()).unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let pid: u32 = std::fs::read_to_string(app.join("serve.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let lock_path = app.join(".v027-sandbox-transition.lock");
    let transition = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    transition.lock_exclusive().unwrap();
    let socket = app.join("daemon/api.sock");
    let mut mutation = UnixStream::connect(&socket).unwrap();
    mutation
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = r#"{"color":"red"}"#;
    write!(mutation, "PATCH /api/sessions/{id}/color HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .flatten()
            .any(|entry| {
                std::fs::read_link(entry.path()).ok().as_deref() == Some(lock_path.as_path())
            });
        if waiting {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mutation never reached the held transition lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut capture = UnixStream::connect(&socket).unwrap();
    capture
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    capture
        .write_all(b"GET /api/runtime HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    capture
        .read_to_string(&mut response)
        .expect("waiting for migration must not hold publication");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    fs2::FileExt::unlock(&transition).unwrap();
    response.clear();
    mutation.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response
        .to_ascii_lowercase()
        .contains("aoe-runtime-revision:"));
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(rows_path).unwrap()).unwrap();
    assert_eq!(
        rows.iter().find(|row| row["id"] == id).unwrap()["color"],
        "red"
    );
}

#[cfg(unix)]
#[test]
#[parallel]
fn daemon_stop_reports_docker_failure_without_blocking_publication() {
    use std::io::{Read, Write};
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};

    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("runtime_stop_failure");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let entered = h.home_path().join("stop-entered");
    let release = h.home_path().join("stop-release");
    let missing = h.home_path().join("container-missing");
    struct ReleaseOnDrop(PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }
    let _release = ReleaseOnDrop(release.clone());
    let bin = h.home_path().join("docker-fixture");
    std::fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    std::fs::write(
        &docker,
        format!(
            r#"#!/bin/sh
case " $* " in
  *" inspect "*) echo 'simulated inspect failure' >&2; exit 17 ;;
  *" stop "*)
    if [ -e '{}' ]; then
      echo 'Error response from daemon: No such container: fixture' >&2
      exit 1
    fi
    touch '{}'
    tries=0
    while [ ! -e '{}' ]; do
      tries=$((tries + 1))
      [ "$tries" -lt 1000 ] || exit 124
      sleep 0.01
    done
    echo 'simulated stop failure' >&2
    exit 19 ;;
esac
exit 0
"#,
            missing.display(),
            entered.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    h.add_path_dir(&bin);
    let project = h.project_path();
    let added = h.run_cli(&["add", project.to_str().unwrap(), "--title", "stop-failure"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let app = crate::harness::app_dir_in(h.home_path());
    let rows_path = app.join("profiles/default/sessions.json");
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&rows_path).unwrap()).unwrap();
    let row = rows
        .iter_mut()
        .find(|row| row["title"] == "stop-failure")
        .unwrap();
    let id = row["id"].as_str().unwrap().to_owned();
    row["status"] = serde_json::json!("idle");
    row["sandbox_info"] = serde_json::json!({"enabled": true, "image": "fixture", "container_name": format!("aoe-{id}")});
    std::fs::write(&rows_path, serde_json::to_vec(&rows).unwrap()).unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let socket = app.join("daemon/api.sock");
    let mut stopping = UnixStream::connect(&socket).unwrap();
    stopping
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stopping, "POST /api/sessions/{id}/stop HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !entered.exists() {
        assert!(Instant::now() < deadline, "stop never reached Docker");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut capture = UnixStream::connect(&socket).unwrap();
    capture
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    capture
        .write_all(b"GET /api/runtime HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    capture
        .read_to_string(&mut response)
        .expect("resource teardown must not hold publication");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    std::fs::write(&release, b"").unwrap();
    response.clear();
    stopping.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 500"),
        "failed teardown was acknowledged: {response}"
    );
    assert!(!response
        .to_ascii_lowercase()
        .contains("aoe-runtime-revision:"));
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&rows_path).unwrap()).unwrap();
    let row = rows.iter().find(|row| row["id"] == id).unwrap();
    assert_eq!(row["status"], "error");
    assert!(row
        .get("lifecycle_reservation")
        .is_none_or(serde_json::Value::is_null));
    std::fs::write(&missing, b"").unwrap();
    let mut retry = UnixStream::connect(&socket).unwrap();
    retry
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(retry, "POST /api/sessions/{id}/stop HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    response.clear();
    retry.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "absent container prevented recovery: {response}"
    );
    let body: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["status"], "Stopped");
    assert!(response
        .to_ascii_lowercase()
        .contains("aoe-runtime-revision:"));
}

#[cfg(unix)]
#[test]
#[parallel]
fn daemon_retains_cancelled_commit_through_shutdown() {
    use fs2::FileExt;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut h = TuiTestHarness::new_in_tmp("runtime_commit_lifetime");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let project = h.project_path();
    let added = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--title",
        "cancelled-commit",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let app = crate::harness::app_dir_in(h.home_path());
    let profile = app.join("profiles/default");
    let rows_path = profile.join("sessions.json");
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&rows_path).unwrap()).unwrap();
    let id = rows
        .iter()
        .find(|row| row["title"] == "cancelled-commit")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let socket = app.join("daemon/api.sock");
    let pid_path = daemon_pid_path(&h);
    let pid = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let capture_blocked = || {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        stream
            .write_all(b"GET /api/runtime HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        match stream.read_exact(&mut [0]) {
            Ok(()) => false,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            Err(error) => panic!("runtime capture failed: {error}"),
        }
    };
    let storage_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join(".storage.lock"))
        .unwrap();
    storage_lock.lock_exclusive().unwrap();
    let body = r#"{"color":"red"}"#;
    let mut mutation = UnixStream::connect(&socket).unwrap();
    write!(mutation, "PATCH /api/sessions/{id}/color HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !capture_blocked() {
        assert!(
            Instant::now() < deadline,
            "mutation never reached its commit boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    mutation.shutdown(std::net::Shutdown::Both).unwrap();
    drop(mutation);
    assert!(
        capture_blocked(),
        "disconnect exposed a snapshot while the commit was blocked"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while UnixStream::connect(&socket).is_ok() {
        assert!(
            Instant::now() < deadline,
            "listener stayed open after shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(250));
    let lifetime = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(app.join("daemon/lifetime.lock"))
        .unwrap();
    assert_eq!(
        lifetime.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "ownership ended before the blocked commit"
    );
    fs2::FileExt::unlock(&storage_lock).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_path.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon did not finish after commit unblocked"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let committed: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(rows_path).unwrap()).unwrap();
    assert_eq!(
        committed.iter().find(|row| row["id"] == id).unwrap()["color"],
        "red"
    );
}

#[cfg(unix)]
#[test]
#[parallel]
fn read_only_terminal_view_does_not_create_shell() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("readonly_terminal_lifecycle");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let project = h.project_path();
    let added = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--title",
        "readonly-shell",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let app = crate::harness::app_dir_in(h.home_path());
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(app.join("profiles/default/sessions.json")).unwrap())
            .unwrap();
    let id = rows
        .iter()
        .find(|row| row["title"] == "readonly-shell")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--read-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let mut wire = UnixStream::connect(app.join("daemon/api.sock")).unwrap();
    wire.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(wire, "GET /sessions/{id}/terminal/live-ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 8192);
        let mut byte = [0];
        wire.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    assert!(headers.starts_with(b"HTTP/1.1 101 "));
    let mut frame = [0; 2];
    wire.read_exact(&mut frame).unwrap();
    assert_eq!(
        frame[0], 0x88,
        "missing shell must close, not stream a new pane"
    );
    assert!(frame[1] <= 125);
    let mut close = vec![0; usize::from(frame[1])];
    wire.read_exact(&mut close).unwrap();
    assert_eq!(close.get(..2), Some(1013u16.to_be_bytes().as_slice()));
    let panes = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    assert!(
        panes.stdout.is_empty(),
        "read-only viewing created a shell: {}",
        String::from_utf8_lossy(&panes.stdout)
    );
}

/// Restart retains exposure policy after renaming the bootstrap profile.
#[test]
#[parallel]
fn cli_serve_restart_replays_launch_state() {
    let h = TuiTestHarness::new("serve_restart_replays");
    let port = pick_free_port();
    let port_s = port.to_string();

    let core = h.run_cli(&["serve", "--daemon", "--core-only"]);
    assert!(
        core.status.success(),
        "{}",
        String::from_utf8_lossy(&core.stderr)
    );
    let core_pid: i32 = std::fs::read_to_string(daemon_pid_path(&h))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let invalid = h.run_cli(&["serve", "--daemon", "--host", "0.0.0.0", "--no-auth"]);
    assert!(!invalid.status.success());
    assert!(
        pid_alive(core_pid),
        "invalid exposure stopped the running core"
    );

    let provisional = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        provisional.status.success(),
        "{}",
        String::from_utf8_lossy(&provisional.stderr)
    );
    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let rollback = h.run_cli(&["serve", "--rollback"]);
    assert!(
        rollback.status.success(),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "rollback retained TCP exposure"
    );
    let core_pid: i32 = std::fs::read_to_string(daemon_pid_path(&h))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "initial --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let pid_path = daemon_pid_path(&h);
    let launch_path = daemon_launch_path(&h);
    let pid1: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid after start")
        .trim()
        .parse()
        .expect("serve.pid holds an integer");
    assert_ne!(core_pid, pid1);
    assert!(!pid_alive(core_pid), "core predecessor survived promotion");
    let launch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&launch_path).unwrap()).unwrap();
    let rename = h.run_cli(&[
        "profile",
        "rename",
        launch["profile"].as_str().unwrap(),
        "renamed-bootstrap",
    ]);
    assert!(
        rename.status.success(),
        "{}",
        String::from_utf8_lossy(&rename.stderr)
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/api/runtime/snapshot");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot: serde_json::Value = client
                .get(&url)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if snapshot["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|profile| profile["name"] == "renamed-bootstrap")
            {
                assert_eq!(snapshot["default_profile"], "renamed-bootstrap");
                assert_eq!(snapshot["health"]["state"], "healthy");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "renamed catalogue was not published: {snapshot}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let restart = h.run_cli(&["serve", "--restart"]);
    assert!(
        restart.status.success(),
        "aoe serve --restart failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&restart.stdout),
        String::from_utf8_lossy(&restart.stderr),
    );

    // The replacement child rebinds the same persisted port.
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "restarted daemon never rebound port {port}"
    );

    let pid2: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid after restart")
        .trim()
        .parse()
        .expect("serve.pid holds an integer");
    assert_ne!(pid1, pid2, "restart should spawn a new daemon PID");
    assert!(
        pid_alive(pid2),
        "restarted daemon PID {pid2} should be alive"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(pid1),
        "old daemon PID {pid1} still alive after restart"
    );

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "--stop after restart failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );
}

#[test]
#[parallel]
fn cli_serve_replay_rejects_missing_or_unbound_passphrases() {
    let h = TuiTestHarness::new("serve_restart_secondary_passphrase");
    let port = pick_free_port().to_string();
    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port,
        "--auth",
        "token",
        "--passphrase",
        "retained-secondary-gate",
    ]);
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let pid_path = daemon_pid_path(&h);
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let launch_path = daemon_launch_path(&h);
    let original = std::fs::read(&launch_path).unwrap();
    let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
    legacy.as_object_mut().unwrap().remove("has_passphrase");
    legacy.as_object_mut().unwrap().remove("instance_id");
    legacy["schema"] = 3.into();
    std::fs::write(&launch_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    legacy["port"] = pick_free_port().into();
    std::fs::write(
        launch_path.with_file_name("serve.rollback.launch"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    std::fs::write(launch_path.with_file_name(".schema_version"), "29").unwrap();
    let migrate = h.run_cli(&["profile", "list"]);
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    let rollback = h.run_cli(&["serve", "--rollback"]);
    assert!(
        !rollback.status.success(),
        "rollback borrowed credentials using only a historical PID"
    );
    assert!(
        pid_alive(pid),
        "unbound rollback stopped the protected daemon"
    );
    std::fs::write(&launch_path, original).unwrap();
    std::fs::remove_file(pid_path.with_file_name("serve.passphrase")).unwrap();
    let restart = h.run_cli(&["serve", "--restart"]);
    assert!(
        !restart.status.success(),
        "restart silently removed the secondary login gate"
    );
    assert!(
        pid_alive(pid),
        "missing credentials stopped the protected daemon"
    );
    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

/// Regression guard for the sink consolidation (issue #1124): the daemon
/// must write its tracing stream to the configured `debug.log`, and the
/// retired `serve.log` must not reappear. Without this guard, a future
/// change that misclassifies the daemon child as `ServeForeground` (or
/// reintroduces the `serve.log` redirect) would slip through CI.
#[test]
#[parallel]
fn cli_serve_daemon_writes_marker_to_debug_log_not_serve_log() {
    let h = TuiTestHarness::new("serve_daemon_logging_sinks");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(start.status.success(), "aoe serve --daemon failed");

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    let app_dir = crate::harness::app_dir_in(h.home_path());
    let debug_log = app_dir.join("debug.log");
    let serve_log = app_dir.join("serve.log");

    let debug_contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {}", debug_log.display(), e));
    assert!(
        debug_contents.contains("[AOE_START_MARKER]"),
        "debug.log should carry the filter-immune startup marker; got: {:?}",
        debug_contents
    );
    assert!(
        !serve_log.exists(),
        "serve.log must not be re-created post-consolidation, found at {}",
        serve_log.display()
    );

    let _ = h.run_cli(&["serve", "--stop"]);
}

/// Proxy ingress requires passphrase login and a bound browser session.
#[test]
#[parallel]
fn cli_serve_auth_passphrase_login_round_trip() {
    let h = TuiTestHarness::new("serve_auth_passphrase");
    let port = pick_free_port();
    let port_s = port.to_string();

    // XFF is trusted only on configured proxy ingress.
    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
        "--behind-proxy",
        "--allowed-host",
        "aoe.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    // 32 random-ish bytes; the contents don't matter, just the length and encoding.
    let binding_raw: [u8; 32] = [0x5Au8; 32];
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let binding_b64 = URL_SAFE_NO_PAD.encode(binding_raw);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<(), String> = rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        // reqwest's `cookies` feature is not enabled in the workspace, so
        // pull the session out of `Set-Cookie` by hand. Cheaper than
        // touching Cargo.toml just for one test.
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;

        // Unauthenticated forwarded callers must receive login_required.
        let about_unauth = client
            .get(format!("{base}/api/about"))
            .header("x-forwarded-for", "10.0.0.5")
            .send()
            .await
            .map_err(|e| format!("GET /api/about (unauth): {e}"))?;
        let status = about_unauth.status();
        let body: serde_json::Value = about_unauth
            .json()
            .await
            .map_err(|e| format!("decode unauth body: {e}"))?;
        if status != reqwest::StatusCode::UNAUTHORIZED
            || body.get("error").and_then(|v| v.as_str()) != Some("login_required")
        {
            return Err(format!(
                "expected 401 login_required, got status={status} body={body}"
            ));
        }

        // `/api/sessions` is the surface #3843 reported as readable
        // without a credential: session ids, titles, and filesystem
        // paths. Assert it directly rather than trusting that the wall
        // covers it because `/api/about` is covered.
        let sessions_unauth = client
            .get(format!("{base}/api/sessions"))
            .header("x-forwarded-for", "10.0.0.5")
            .send()
            .await
            .map_err(|e| format!("GET /api/sessions (unauth): {e}"))?;
        if sessions_unauth.status() != reqwest::StatusCode::UNAUTHORIZED {
            let s = sessions_unauth.status();
            let b = sessions_unauth.text().await.unwrap_or_default();
            return Err(format!(
                "remote /api/sessions must 401 under --auth=passphrase, got status={s} body={b}"
            ));
        }

        // 2. POST /api/login with matching passphrase + device binding.
        let login = client
            .post(format!("{base}/api/login"))
            .json(&serde_json::json!({
                "passphrase": "e2e-pass",
                "device_binding_secret": binding_b64,
            }))
            .send()
            .await
            .map_err(|e| format!("POST /api/login: {e}"))?;
        if !login.status().is_success() {
            let s = login.status();
            let b = login.text().await.unwrap_or_default();
            return Err(format!("login failed: status={s} body={b}"));
        }

        // Pull `aoe_session=...` out of the first Set-Cookie value that
        // names it. Multiple Set-Cookie headers may come back (login
        // cookie, push-related cookies, etc.); pick the one we need.
        let session_cookie = login
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .find_map(|v| {
                let s = v.to_str().ok()?;
                let first = s.split(';').next()?.trim();
                if first.starts_with("aoe_session=") {
                    Some(first.to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| "login response missing aoe_session Set-Cookie".to_string())?;

        // 3. Authenticated GET must succeed and report auth_mode=passphrase.
        let about = client
            .get(format!("{base}/api/about"))
            .header("cookie", &session_cookie)
            .header("x-aoe-device-binding", &binding_b64)
            .send()
            .await
            .map_err(|e| format!("GET /api/about (auth): {e}"))?;
        if !about.status().is_success() {
            let s = about.status();
            let b = about.text().await.unwrap_or_default();
            return Err(format!(
                "authenticated /api/about failed: status={s} body={b}"
            ));
        }
        let body: serde_json::Value = about
            .json()
            .await
            .map_err(|e| format!("decode about body: {e}"))?;
        match body.get("auth_mode").and_then(|v| v.as_str()) {
            Some("passphrase") => Ok(()),
            other => Err(format!(
                "expected auth_mode=passphrase, got {other:?} in {body}"
            )),
        }
    });

    // Always tear the daemon down before asserting, so a failed assert
    // doesn't leak a process that owns the test port.
    let _ = h.run_cli(&["serve", "--stop"]);

    if let Err(e) = result {
        panic!("{e}");
    }
}

/// Regression test for #1525. With `--auth=passphrase` the daemon used
/// to route loopback callers through the passphrase wall, breaking the
/// local TUI structured view attach: it had no session cookie + device binding
/// to present so `/api/sessions/{id}/structured view/replay` and the structured view ws
/// upgrade always 401'd. The fix mirrors the token-auth path's #1168
/// carve-out and treats loopback as fs-trusted.
///
/// Flow:
///   1. Start `aoe serve --daemon --auth=passphrase`.
///   2. GET `/api/about` from 127.0.0.1 with no session cookie, no
///      device binding, no XFF -> 200 with `"auth_mode":"passphrase"`.
///      Without the bypass this would 401 `login_required`.
///   3. GET `/api/sessions` from 127.0.0.1 -> 200 (proves the bypass
///      covers the structured view REST surface, not just `/api/about`).
#[test]
#[parallel]
fn cli_serve_auth_passphrase_loopback_bypass() {
    let h = TuiTestHarness::new("serve_auth_passphrase_loopback");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<(), String> = rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;

        // No cookie, no device binding, no XFF: the loopback bypass
        // (#1525) lets the request through. Pre-fix this would have
        // returned 401 login_required.
        let about = client
            .get(format!("{base}/api/about"))
            .send()
            .await
            .map_err(|e| format!("GET /api/about (loopback bypass): {e}"))?;
        if !about.status().is_success() {
            let s = about.status();
            let b = about.text().await.unwrap_or_default();
            return Err(format!(
                "loopback /api/about should 200 under --auth=passphrase, got status={s} body={b}"
            ));
        }
        let body: serde_json::Value = about
            .json()
            .await
            .map_err(|e| format!("decode about body: {e}"))?;
        if body.get("auth_mode").and_then(|v| v.as_str()) != Some("passphrase") {
            return Err(format!(
                "expected auth_mode=passphrase on loopback bypass, got {body}"
            ));
        }

        // The structured view REST surface lives under the same wall, so the
        // bypass must extend to it. A successful 200 on `/api/sessions`
        // from loopback without a session is what unblocks the local
        // TUI structured view attach in the issue report.
        let sessions = client
            .get(format!("{base}/api/sessions"))
            .send()
            .await
            .map_err(|e| format!("GET /api/sessions (loopback bypass): {e}"))?;
        if !sessions.status().is_success() {
            let s = sessions.status();
            let b = sessions.text().await.unwrap_or_default();
            return Err(format!(
                "loopback /api/sessions should 200 under --auth=passphrase, got status={s} body={b}"
            ));
        }

        Ok(())
    });

    let _ = h.run_cli(&["serve", "--stop"]);

    if let Err(e) = result {
        panic!("{e}");
    }
}

/// Regression test for #3843. `--auth=passphrase --behind-proxy` is the
/// documented "TLS terminated upstream, passphrase is the only human
/// gate" deployment. The proxy runs on the same host, so its requests
/// arrive from a loopback socket; the #1525 loopback carve-out then
/// waved them all through, and an upstream that does not set
/// `X-Forwarded-For` (a bare nginx `proxy_pass`) turned the whole API
/// into an unauthenticated public surface.
///
/// Flow: start a `--behind-proxy` passphrase daemon, then send exactly
/// what such a proxy sends: loopback socket, the public `Host`, no
/// forwarding header, no credential. `/api/sessions` must 401. Before
/// the fix it returned 200 with the session list.
///
/// Then sign in over that same unforwarded shape and confirm a
/// step-up-gated route still demands elevation: the request used to
/// carry `LoopbackTrusted`, which would have handed a proxied visitor
/// who knew the passphrase the skill and plugin mutation routes with
/// no re-prompt.
#[test]
#[parallel]
fn cli_serve_auth_passphrase_behind_proxy_gates_unforwarded_requests() {
    let h = TuiTestHarness::new("serve_auth_passphrase_behind_proxy");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase --behind-proxy failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<(), String> = rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;

        for path in ["/api/sessions", "/api/projects"] {
            let res = client
                .get(format!("{base}{path}"))
                .header("host", "aoe.example.test")
                .send()
                .await
                .map_err(|e| format!("GET {path}: {e}"))?;
            if res.status() != reqwest::StatusCode::UNAUTHORIZED {
                let s = res.status();
                let b = res.text().await.unwrap_or_default();
                return Err(format!(
                    "{path} must 401 for an unforwarded proxied request under \
                     --auth=passphrase --behind-proxy, got status={s} body={b}"
                ));
            }
        }

        // The login surfaces stay reachable, otherwise a real user
        // behind the proxy could never get past the wall.
        let login_page = client
            .get(format!("{base}/api/login/status"))
            .header("host", "aoe.example.test")
            .send()
            .await
            .map_err(|e| format!("GET /api/login/status: {e}"))?;
        if !login_page.status().is_success() {
            let s = login_page.status();
            return Err(format!("/api/login/status must stay reachable, got {s}"));
        }

        // Signing in must not restore through elevation what the wall
        // denied. The same unforwarded request used to be stamped
        // `LoopbackTrusted`, which `handler_elevated` reads as an
        // elevated session, so a proxied visitor who knew the
        // passphrase reached the step-up-gated routes (plugin and
        // skill mutation: arbitrary code on the host) with no
        // re-prompt. `SkillMutationGuard` runs before the handler, so
        // the directory below is never touched.
        let binding_raw: [u8; 32] = [0x5Au8; 32];
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let binding_b64 = URL_SAFE_NO_PAD.encode(binding_raw);

        let login = client
            .post(format!("{base}/api/login"))
            .header("host", "aoe.example.test")
            .json(&serde_json::json!({
                "passphrase": "e2e-pass",
                "device_binding_secret": binding_b64,
            }))
            .send()
            .await
            .map_err(|e| format!("POST /api/login: {e}"))?;
        if !login.status().is_success() {
            let s = login.status();
            let b = login.text().await.unwrap_or_default();
            return Err(format!("login failed: status={s} body={b}"));
        }
        let session_cookie = login
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .find_map(|v| {
                let s = v.to_str().ok()?;
                let first = s.split(';').next()?.trim();
                if first.starts_with("aoe_session=") {
                    Some(first.to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| "login response missing aoe_session Set-Cookie".to_string())?;

        let mutation = client
            .delete(format!("{base}/api/skills/e2e-nonexistent"))
            .header("host", "aoe.example.test")
            .header("cookie", &session_cookie)
            .header("x-aoe-device-binding", &binding_b64)
            .send()
            .await
            .map_err(|e| format!("DELETE /api/skills: {e}"))?;
        if mutation.status() != reqwest::StatusCode::FORBIDDEN {
            let s = mutation.status();
            let b = mutation.text().await.unwrap_or_default();
            return Err(format!(
                "a signed-in proxied caller must still step up for a skill \
                 mutation, got status={s} body={b}"
            ));
        }
        let body: serde_json::Value = mutation
            .json()
            .await
            .map_err(|e| format!("decode mutation body: {e}"))?;
        if body.get("error").and_then(|v| v.as_str()) != Some("elevation_required") {
            return Err(format!("expected elevation_required, got {body}"));
        }

        Ok(())
    });

    let _ = h.run_cli(&["serve", "--stop"]);

    if let Err(e) = result {
        panic!("{e}");
    }
}

/// Regression test for #2896: a fatal startup validation failure must reach the
/// `tracing` sink `aoe logs` reads, not only the process's raw stderr. A
/// foreground `aoe serve --behind-proxy` with no `--allowed-host` bails the
/// DNS-rebinding gate before binding; the reason must land in the configured
/// `[logging].file_path` (default `debug.log`) so a supervisor-driven
/// crash-loop is diagnosable from the log, and the process must still exit
/// non-zero.
#[test]
#[parallel]
fn cli_serve_startup_bail_reaches_debug_log() {
    let h = TuiTestHarness::new("serve_startup_bail_logged");

    let out = h.run_cli(&["serve", "--behind-proxy"]);
    assert!(
        !out.status.success(),
        "serve --behind-proxy without --allowed-host must exit non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let debug_log = crate::harness::app_dir_in(h.home_path()).join("debug.log");
    let contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {}", debug_log.display(), e));
    assert!(
        contents.contains("--behind-proxy requires --allowed-host"),
        "fatal startup reason must be routed through the tracing sink; debug.log was:\n{contents}"
    );
}

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| {
            let id = l.trim().strip_prefix("ID:")?.trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

#[test]
fn parse_session_id_extracts_the_trimmed_value() {
    assert_eq!(
        parse_session_id("  Title:    demo\n  ID:      abc-123\n"),
        "abc-123"
    );
}

#[test]
#[should_panic(expected = "could not find session ID")]
fn parse_session_id_panics_when_id_line_is_missing() {
    parse_session_id("  Title:    demo\n");
}

#[test]
#[should_panic(expected = "could not find session ID")]
fn parse_session_id_panics_when_id_value_is_empty() {
    // A blank `ID:` line (broken `aoe add` output) must fail fast rather
    // than hand `prompt_until_accepted` an empty id to retry for 30s.
    parse_session_id("  Title:    demo\n  ID:      \n");
}

/// `aoe acp prompt` 404s until the worker is live and handshaked, so a
/// successful call is the readiness oracle.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "hello"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "worker never accepted a prompt within {timeout:?}.\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `aoe acp <verb>` (built on `HttpClient`, see `src/acp/client/http.rs`)
/// authenticates against a `--behind-proxy --auth=passphrase` daemon by
/// logging in via `POST /api/login` with the daemon's own `serve.passphrase`
/// file (the same file `aoe serve --restart` reads) and caching the
/// resulting `aoe_session` cookie, mirroring how it reuses `serve.token` for
/// `--auth=token`. This matters specifically under `--behind-proxy`: the
/// loopback auth bypass (`cli_serve_auth_passphrase_loopback_bypass`) is
/// withdrawn there
/// (`cli_serve_auth_passphrase_behind_proxy_gates_unforwarded_requests`), so
/// a same-host, same-user caller has no other way in.
#[test]
#[parallel]
fn cli_acp_prompt_authenticates_against_behind_proxy_passphrase_daemon() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("acp_prompt_passphrase_behind_proxy");
    let script_path = h.home_path().join("passphrase-prompt-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase --behind-proxy failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "passphrase-prompt",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));
}

/// The WS half of the passphrase-login fallback: `aoe acp tail` (and the
/// TUI's live structured-view stream, same `src/acp/client/ws.rs`) must
/// offer the literal `aoe-auth` subprotocol alongside
/// `aoe-device.<secret>`. The daemon (`src/server/acp_ws.rs`) only echoes a
/// `Sec-WebSocket-Protocol` response header when the client offered that
/// literal, and tungstenite hard-fails the handshake
/// (`SubProtocolError::NoSubProtocol`) when the response omits it while the
/// request carried one (RFC 6455).
#[test]
#[parallel]
fn cli_acp_tail_connects_to_behind_proxy_passphrase_daemon() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("acp_tail_passphrase_behind_proxy");
    let script_path = h.home_path().join("passphrase-tail-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase --behind-proxy failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "passphrase-tail",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // `tail` replays history from seq 0 by default, so a completed handshake
    // is observable as a printed frame line; a failed one closes stdout
    // (EOF) without ever printing one.
    let mut child = h.spawn_cli(&["acp", "tail", &session_id]);
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let _ = tx.send(std::io::BufReader::new(stdout).lines().next());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Some(Ok(line))) => {
            assert!(
                !line.trim().is_empty(),
                "`aoe acp tail` printed an empty line instead of a frame"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
        other => {
            use std::io::Read;
            let _ = child.kill();
            let status = child.wait().expect("wait for tail process");
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            panic!(
                "`aoe acp tail` never streamed a frame ({other:?}); exited {status}; stderr:\n{stderr}"
            );
        }
    }
}
