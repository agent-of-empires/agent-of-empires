use serial_test::parallel;

use crate::harness::TuiTestHarness;

/// Exercises `aoe add --sandbox` which builds the full container config.
/// This would have caught the duplicate mount points bug (commit 92d2e53).
///
/// Requires a running Docker daemon; marked `#[ignore]` for CI.
#[test]
#[parallel]
#[ignore = "requires Docker daemon"]
fn test_cli_add_with_sandbox() {
    let h = TuiTestHarness::new("cli_sandbox");
    let project = h.project_path();

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "Sandbox E2E",
        "--sandbox",
    ]);
    assert!(
        output.status.success(),
        "aoe add --sandbox failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let list_output = h.run_cli(&["list", "--json"]);
    assert!(list_output.status.success());

    let stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        stdout.contains("Sandbox E2E"),
        "list should contain the sandboxed session.\nOutput:\n{}",
        stdout
    );
}

/// Regression test for #1989: on_create hooks must execute inside the sandbox
/// container, not on the host, when `aoe add --sandbox` is used.
///
/// Before the fix, the CLI called `execute_hooks()` unconditionally with
/// `HookTarget::Local`, ignoring `sandbox_info`. The TUI already handled this
/// correctly via `execute_hooks_in_container_streamed`.
///
/// Requires a running Docker daemon; marked `#[ignore]` for CI.
#[test]
#[parallel]
#[ignore = "requires Docker daemon"]
fn test_cli_add_sandbox_on_create_hooks_run_in_container() {
    let h = TuiTestHarness::new("cli_sandbox_hooks");
    let project = h.project_path();

    // A path in /tmp that both host and container own as separate namespaces.
    // If the hook runs on the HOST (the bug), this file appears on the host.
    // If it runs correctly INSIDE the container, it only exists in the
    // container's ephemeral /tmp and is invisible on the host.
    let marker = format!("/tmp/aoe-sandbox-hook-{}", std::process::id());
    // Guard against a stale marker from a prior crashed run.
    let _ = std::fs::remove_file(&marker);

    let aoe_config_dir = project.join(".agent-of-empires");
    std::fs::create_dir_all(&aoe_config_dir).expect("create config dir");
    std::fs::write(
        aoe_config_dir.join("config.toml"),
        format!("[hooks]\non_create = [\"touch {}\"]\n", marker),
    )
    .expect("write repo config");

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--sandbox",
        "--trust-hooks",
        "-t",
        "SandboxHookTest",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "aoe add --sandbox --trust-hooks failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("on_create hooks completed"),
        "expected 'on_create hooks completed' in stdout.\nstdout: {}",
        stdout
    );
    // Regression guard: the marker must NOT exist on the host. Before the fix,
    // the CLI ran hooks via HookTarget::Local so the file would have appeared
    // here. The correct path runs the hook inside the container, where the
    // host's /tmp is not visible.
    assert!(
        !std::path::Path::new(&marker).exists(),
        "on_create hook ran on the host instead of inside the container \
         (regression of #1989): marker found at {}",
        marker
    );
}

/// `aoe sandbox reclaim` reports the stores whose session resolves in no
/// profile, and removes them only when asked. The stub `docker` on PATH
/// answers "not running" for every container, which is the quiescent arm; the
/// unstubbed runtime fails closed to "live" and would preserve everything.
#[test]
#[parallel]
fn sandbox_reclaim_reports_before_it_removes() {
    let mut h = TuiTestHarness::new("sandbox_reclaim");
    // A runtime that lists nothing and reports every container absent. The
    // reclaim keeps any store a container still exists for, so a stub that
    // merely exits 0 would report every store as attached.
    let bin = h.install_path_command("docker");
    std::fs::write(
        bin.join("docker"),
        "#!/bin/sh\ncase \"$1\" in\n  ps) exit 0 ;;\nesac\n\
         echo \"Error: No such container: $3\" >&2\nexit 1\n",
    )
    .expect("write docker stub");
    let project = h.project_path();

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Reclaim Owner"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let registry = crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let rows: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&registry).expect("read registry"))
            .expect("parse registry");
    let owned_id = rows[0]["id"].as_str().expect("session id").to_string();

    let root = h.home_path().join(".claude").join("sandbox-v2");
    let owned = root.join(&owned_id);
    let orphan = root.join("2222222222222222");
    for store in [&owned, &orphan] {
        std::fs::create_dir_all(store).expect("create store");
        std::fs::write(store.join(".credentials.json"), vec![b'x'; 4096]).expect("write store");
        // Past the creation grace period, which exists to protect a store
        // being seeded for a session whose row is not inserted yet.
        let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
        std::fs::File::open(store)
            .expect("open store")
            .set_times(std::fs::FileTimes::new().set_modified(aged))
            .expect("age store");
    }

    let report = h.run_cli(&["sandbox", "reclaim"]);
    assert!(
        report.status.success(),
        "aoe sandbox reclaim failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let reported = String::from_utf8_lossy(&report.stdout);
    assert!(
        reported.contains("2222222222222222") && !reported.contains(&owned_id),
        "report should name only the orphan.\nOutput:\n{reported}"
    );
    assert!(orphan.exists(), "a bare report must delete nothing");

    // A store written to just now is held back, so a session being created
    // concurrently cannot have its seeded credentials swept.
    let fresh = root.join("3333333333333333");
    std::fs::create_dir_all(&fresh).expect("create fresh store");
    std::fs::write(fresh.join(".credentials.json"), b"seeding").expect("write fresh store");

    let deleted = h.run_cli(&["sandbox", "reclaim", "--delete"]);
    assert!(
        deleted.status.success(),
        "aoe sandbox reclaim --delete failed: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    // An orphan is reclaimed only when AoE can prove it wrote the store. This
    // one has no content certificate, so the pass reports it and retains it.
    let deleted_output = String::from_utf8_lossy(&deleted.stdout);
    assert!(
        deleted_output.contains("2222222222222222"),
        "an unproven store must be reported.\nOutput:\n{deleted_output}"
    );
    assert!(orphan.exists(), "an unproven store must survive `--delete`");
    assert!(owned.exists(), "a claimed store must survive the pass");
    assert!(fresh.exists(), "a store being seeded right now was swept");
}

#[tokio::test]
#[parallel]
async fn native_purge_never_starts_the_managed_container() {
    use agent_of_empires::{daemon::DaemonClient, session::Instance};
    use std::{fs, path::PathBuf, process::Command, time::Duration};
    crate::harness::require_tmux!();
    const IMAGE: &str = "ghcr.io/agent-of-empires/aoe-sandbox:latest";
    const HOST: &str = "unix:///var/run/docker.sock";
    fn docker(args: &[&str]) -> std::io::Result<Option<std::process::Output>> {
        let mut command = Command::new("docker");
        command
            .args(args)
            .env("DOCKER_HOST", HOST)
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_TLS")
            .env_remove("DOCKER_TLS_VERIFY");
        agent_of_empires::process::run_with_timeout(&mut command, Duration::from_secs(30))
    }
    if !docker(&["image", "inspect", IMAGE])
        .ok()
        .flatten()
        .is_some_and(|out| out.status.success())
    {
        eprintln!(
            "Skipping container purge: local Docker or the cached sandbox image is unavailable"
        );
        return;
    }
    let mut h = TuiTestHarness::new_in_tmp("native_purge_entrypoint");
    for (key, value) in [
        ("DOCKER_HOST", HOST),
        ("DOCKER_CONTEXT", ""),
        ("DOCKER_TLS", ""),
        ("DOCKER_TLS_VERIFY", ""),
        ("AGENT_OF_EMPIRES_PROFILE", "default"),
    ] {
        h.set_env(key, value);
    }
    h.stop_daemon_on_drop();
    let app = crate::harness::app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[sandbox]\ncontainer_runtime = \"docker\"\n");
    fs::write(config_path, config).unwrap();
    let project = h.project_path();
    {
        let repository = git2::Repository::init(&project).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree_id = repository.index().unwrap().write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
    }
    let added = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--worktree",
        "purge-entrypoint",
        "--new-branch",
        "--tool",
        "claude",
        "--sandbox-image",
        IMAGE,
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let rows_path = app.join("profiles/default/sessions.json");
    let mut rows: Vec<Instance> = serde_json::from_slice(&fs::read(&rows_path).unwrap()).unwrap();
    let row = &mut rows[0];
    assert!(row.worktree_info.as_ref().unwrap().managed_by_aoe);
    let worktree = PathBuf::from(&row.project_path);
    let control = h.home_path().join("container-control");
    fs::create_dir(&control).unwrap();
    fs::write(worktree.join("owned-data"), "keep until runtime teardown").unwrap();
    struct OwnedContainer {
        name: String,
        worktree: PathBuf,
        control: PathBuf,
    }
    impl Drop for OwnedContainer {
        fn drop(&mut self) {
            let Some(output) = docker(&["container", "inspect", &self.name])
                .ok()
                .flatten()
                .filter(|output| output.status.success())
            else {
                return;
            };
            let Ok(values) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
            else {
                return;
            };
            let Some(value) = values.first() else {
                return;
            };
            let Some(mounts) = value["Mounts"].as_array() else {
                return;
            };
            if [&self.worktree, &self.control].iter().all(|path| {
                mounts
                    .iter()
                    .any(|mount| mount["Source"].as_str() == path.to_str())
            }) {
                if let Some(id) = value["Id"].as_str() {
                    let _ = docker(&["rm", "-fv", id]);
                }
            }
        }
    }
    let sandbox = row.sandbox_info.as_mut().unwrap();
    let owned = OwnedContainer {
        name: sandbox.container_name.clone(),
        worktree: worktree.clone(),
        control: control.clone(),
    };
    let work_mount = format!("type=bind,source={},target=/work", worktree.display());
    let control_mount = format!("type=bind,source={},target=/control", control.display());
    let created = docker(&[
        "create",
        "--name",
        &owned.name,
        "--entrypoint",
        "/bin/sh",
        "--mount",
        &work_mount,
        "--mount",
        &control_mount,
        "--workdir",
        "/work",
        IMAGE,
        "-c",
        "printf 'started\\n' >> /control/starts; exec sleep infinity",
    ])
    .unwrap()
    .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    sandbox.container_id = Some(id.clone());
    sandbox.container_workdir = Some("/work".into());
    let session_id = row.id.clone();
    fs::write(&rows_path, serde_json::to_vec(&rows).unwrap()).unwrap();
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let sdk = DaemonClient::new_unix(app.join("daemon/api.sock")).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let http = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let response = http
        .delete(format!("http://localhost/api/sessions/{session_id}"))
        .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, epoch)
        .json(
            &serde_json::json!({"delete_worktree":true,"delete_branch":true,
                                 "delete_sandbox":true,"force_delete":true}),
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    let outcome: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{outcome}");
    assert_eq!(
        outcome["cleanup_errors"],
        serde_json::json!([]),
        "{outcome}"
    );
    assert!(
        !control.join("starts").exists(),
        "purge executed the managed container entrypoint"
    );
    assert!(!worktree.exists(), "purge left the managed worktree behind");
    let filter = format!("id={id}");
    let remaining = docker(&[
        "container",
        "ls",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
        &filter,
    ])
    .unwrap()
    .unwrap();
    assert!(
        remaining.status.success(),
        "{}",
        String::from_utf8_lossy(&remaining.stderr)
    );
    assert!(
        remaining.stdout.is_empty(),
        "purge retained its managed container"
    );
}
