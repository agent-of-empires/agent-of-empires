//! e2e: peer-process writes to `sessions.json` propagate to the TUI through
//! both the kernel-watcher path and the five-second storage heartbeat fallback.
//!
//! The watcher case must land within 1.5 seconds. The fallback case creates a
//! previously unknown profile while live-send is active, so no disk watch can
//! already cover it; the periodic storage-only reload must discover it without
//! running the deferred full heartbeat.

use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::file_watch::FileWatchService;
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;

use crate::harness::{require_tmux, HomeGuard, TuiTestHarness};

#[test]
#[serial]
fn peer_storage_update_reflects_within_sub_tick_budget() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_reload");
    h.spawn_tui();
    h.wait_for(" aoe ");

    // Set HOME/XDG_CONFIG_HOME for THIS process so `Storage::new`
    // resolves the same app dir the TUI is watching.
    let _home = HomeGuard::new(h.home_path());

    let svc: Arc<FileWatchService> = FileWatchService::noop();
    let storage = Storage::new("default", svc).expect("storage in test process");

    let title = "filewatch-test-row";
    storage
        .update(|i, _g| {
            let mut inst = Instance::new(title, "/tmp/filewatch-test");
            inst.source_profile = "default".to_string();
            i.push(inst);
            Ok(())
        })
        .expect("peer write to sessions.json");

    h.wait_for_timeout(title, Duration::from_millis(1_500));
}

#[test]
#[serial]
fn peer_cli_add_reflects_during_live_send_without_changing_target() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_live_send_reload");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\ncat > live-send-input\n")
        .expect("write interactive claude");

    let active_project = h.project_path();
    let active = h.run_cli(&[
        "add",
        active_project.to_str().expect("utf8 active project"),
        "-t",
        "active-live",
        "-c",
        "claude",
    ]);
    assert!(
        active.status.success(),
        "initial aoe add failed: {}",
        String::from_utf8_lossy(&active.stderr)
    );

    h.spawn_tui();
    h.wait_for("active-live");
    h.send_keys("Tab");
    h.wait_for_timeout("LIVE →  active-live", Duration::from_secs(10));

    let peer_project = h.home_path().join("peer-project");
    std::fs::create_dir_all(&peer_project).expect("create peer project");
    let peer = h.run_cli(&[
        "add",
        peer_project.to_str().expect("utf8 peer project"),
        "-t",
        "peer-added",
        "-c",
        "claude",
    ]);
    assert!(
        peer.status.success(),
        "peer aoe add failed: {}",
        String::from_utf8_lossy(&peer.stderr)
    );

    h.wait_for_timeout("peer-added", Duration::from_millis(1_500));
    h.assert_screen_contains("LIVE →  active-live");
    let _home = HomeGuard::new(h.home_path());
    let late_storage =
        Storage::new("late-live-profile", FileWatchService::noop()).expect("late profile storage");
    late_storage
        .update(|instances, _groups| {
            let mut inst = Instance::new("late-profile-added", "/tmp/late-profile-added");
            inst.source_profile = "late-live-profile".to_string();
            instances.push(inst);
            Ok(())
        })
        .expect("peer write in previously unwatched profile");
    h.wait_for_timeout("late-profile-added", Duration::from_secs(7));
    h.assert_screen_contains("LIVE →  active-live");
    h.type_text("routed-after-refresh");
    h.send_keys("Enter");

    let active_input = active_project.join("live-send-input");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let routed = loop {
        if let Ok(contents) = std::fs::read_to_string(&active_input) {
            if contents.contains("routed-after-refresh") {
                break contents;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live-send input did not reach the original session"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(routed.contains("routed-after-refresh"));
    let peer_input = peer_project.join("live-send-input");
    assert!(
        std::fs::read_to_string(peer_input)
            .map(|contents| !contents.contains("routed-after-refresh"))
            .unwrap_or(true),
        "storage refresh must not reroute input to the peer-added session"
    );
}
