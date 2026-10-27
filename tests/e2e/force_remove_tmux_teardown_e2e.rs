//! Force Remove abandons an owned, wedged purge and tears down its native pane.
//! The forgotten purge must not resume worktree or branch cleanup afterward.

use agent_of_empires::{
    daemon::{
        CreateSessionBody, DaemonClient, DeleteSessionBody, RuntimeSnapshot, SessionMutation,
    },
    session::{LifecycleOperation, PanePresence},
};
use serial_test::parallel;
use std::path::PathBuf;
use std::time::Duration;

use crate::harness::{app_dir_in, init_git_repo, require_tmux, wait_until, TuiTestHarness};

struct ReleaseHook(PathBuf);

impl Drop for ReleaseHook {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, []);
    }
}

async fn snapshot(http: &reqwest::Client) -> RuntimeSnapshot {
    http.get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn test_force_remove_session_kills_agent_tmux_session() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("force_remove_tmux");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let agent = h
        .install_path_command("terminal-agent")
        .join("terminal-agent");
    std::fs::write(&agent, "#!/bin/sh\nexec sleep 600\n").unwrap();
    let entered = h.home_path().join("purge-entered");
    let release = ReleaseHook(h.home_path().join("purge-release"));
    let finished = h.home_path().join("purge-finished");
    let hook = h.home_path().join("purge-hook.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > {entered:?}\nwhile [ ! -e {release:?} ]; do sleep 0.01; done\n: > {finished:?}\n",
            release = release.0,
        ),
    )
    .unwrap();
    let app = app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[session.custom_agents]\nterminal-agent = {}\n[hooks]\non_destroy = [{}]\n",
        serde_json::to_string(agent.to_str().unwrap()).unwrap(),
        serde_json::to_string(&format!("sh {hook:?}")).unwrap(),
    ));
    std::fs::write(config_path, config).unwrap();
    let repo = h.project_path();
    init_git_repo(&repo);
    h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
    let socket = app.join("daemon/api.sock");
    let sdk = DaemonClient::new_unix(&socket).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let http = reqwest::Client::builder()
        .unix_socket(socket.as_path())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let body: CreateSessionBody = serde_json::from_value(serde_json::json!({
        "title": "StuckDel", "path": repo, "tool": "terminal-agent", "profile": "default",
        "worktree_branch": "feature/force-remove", "create_new_branch": true,
    }))
    .unwrap();
    let created = sdk.create_session(&body, &epoch).await.unwrap();
    let id = created.outcome.id;
    let worktree = PathBuf::from(created.outcome.project_path);
    let payload = worktree.join("payload.txt");
    std::fs::write(&payload, "retained after abandon\n").unwrap();
    let started = sdk
        .mutate_session(&id, &SessionMutation::Start(Default::default()), &epoch)
        .await
        .unwrap();
    let live = snapshot(&http).await;
    assert_eq!(live.cursor.epoch, started.epoch);
    assert!(live.cursor.revision >= started.revision);
    let row = live
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(row.agent_pane.state, PanePresence::Alive);
    assert!(row.agent_pane.legacy_tool.is_none());
    let live_generation = row.lifecycle_generation;
    let tmux_name = row.agent_pane.tmux_session.as_ref().unwrap().clone();
    let identity = h
        .tmux()
        .args([
            "list-panes",
            "-t",
            &format!("={tmux_name}"),
            "-F",
            "#{session_id} #{pane_id} #{pane_pid}",
        ])
        .output()
        .unwrap();
    assert!(identity.status.success());
    let identity = String::from_utf8(identity.stdout).unwrap();
    assert_eq!(identity.split_whitespace().count(), 3);

    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("StuckDel");

    // Disconnect only after the daemon has admitted the purge and entered its hook.
    let mut connection = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let body = serde_json::to_string(&DeleteSessionBody {
        delete_worktree: true,
        delete_branch: true,
        force_delete: true,
        ..Default::default()
    })
    .unwrap();
    let request = format!(
        "DELETE /api/sessions/{id} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{}: {epoch}\r\nContent-Length: {}\r\n\r\n{body}",
        agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, body.len(),
    );
    tokio::io::AsyncWriteExt::write_all(&mut connection, request.as_bytes())
        .await
        .unwrap();
    wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
        entered
            .exists()
            .then_some(())
            .ok_or_else(|| "purge hook not entered".into())
    });
    drop(connection);
    let pending = snapshot(&http).await;
    let row = pending
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
        .unwrap();
    let reservation = row.lifecycle_reservation.as_ref().unwrap();
    assert_eq!(reservation.op, LifecycleOperation::Purge);
    assert!(reservation.generation > live_generation);
    assert_eq!(row.status, "Deleting");
    assert!(h.tmux_has_session(&format!("={tmux_name}")));
    assert!(!finished.exists());
    h.wait_for("✕ StuckDel");
    h.send_keys("d");
    h.wait_for("Force Remove");
    let confirm = h.capture_screen();
    assert!(
        confirm.contains("worktrees and branches are not"),
        "{confirm}"
    );
    assert!(confirm.contains("cleaned up by this action."), "{confirm}");
    h.send_keys("y");
    h.wait_for_absent("StuckDel", Duration::from_secs(5));
    let removed = snapshot(&http).await;
    assert_eq!(removed.cursor.epoch, epoch);
    assert!(removed.cursor.revision > pending.cursor.revision);
    assert!(!removed.contents.sessions.iter().any(|row| row.id == id));
    assert!(!h
        .read_sessions()
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == id));
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        (!h.tmux_has_session(&format!("={tmux_name}")))
            .then_some(())
            .ok_or_else(|| format!("native pane still exists: {identity}"))
    });
    assert!(
        !finished.exists(),
        "Force Remove must not claim hook completion"
    );
    assert_eq!(
        std::fs::read_to_string(&payload).unwrap(),
        "retained after abandon\n"
    );

    std::fs::write(&release.0, []).unwrap();
    h.kill_tui();
    h.run_cli_ok(&["serve", "--stop"]);
    assert!(
        finished.exists(),
        "daemon shutdown must drain the disconnected purge"
    );
    let git = git2::Repository::open(&repo).unwrap();
    assert!(git
        .find_branch("feature/force-remove", git2::BranchType::Local)
        .is_ok());
    assert_eq!(
        std::fs::read_to_string(&payload).unwrap(),
        "retained after abandon\n"
    );
    assert!(!h
        .read_sessions()
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == id));
}
