//! End-to-end coverage for terminal (non-ACP) smart rename (issue #2222).
//!
//! Exercises the detached `run_terminal_rename` runner against a real tmux pane
//! and a fake `claude` one-shot shim: a still-civ-named session is renamed from
//! its first turn (user story 1), and a manually-named session is never touched
//! (user story 2). Runs under `serve` only because that is where the async test
//! harness (`#[tokio::test]`) is wired; the code under test is not serve-gated.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use agent_of_empires::session::smart_rename;
use agent_of_empires::session::{Instance, Storage};
use agent_of_empires::tmux;
use serial_test::serial;
use tempfile::TempDir;

use crate::common::{setup_temp_home, tmux_socket};

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Kills every named session on drop. A successful rename leaves the live
/// session under the NEW title-derived name, so tests register both the
/// pre- and post-rename names.
struct TmuxCleanup(Vec<String>);

impl Drop for TmuxCleanup {
    fn drop(&mut self) {
        for name in &self.0 {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(tmux_socket())
                .args(["kill-session", "-t", name])
                .output();
        }
    }
}

/// RAII guard for the fake-agent install: keeps the temp bin dir alive and
/// restores the original `PATH` on drop, so a later `#[serial]` test never sees
/// a `PATH` entry pointing at a deleted directory.
struct FakeAgent {
    _dir: TempDir,
    orig_path: Option<std::ffi::OsString>,
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        match &self.orig_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Write a fake `claude` onto a temp dir at the front of `PATH` so the one-shot
/// (`claude -p <prompt>`) resolves to it and prints a deterministic title.
fn install_fake_claude(title: &str) -> FakeAgent {
    let bin = TempDir::new().unwrap();
    let shim = bin.path().join("claude");
    let mut f = std::fs::File::create(&shim).unwrap();
    // Ignore args; emit the title and exit 0.
    writeln!(f, "#!/bin/sh\necho '{title}'").unwrap();
    let mut perm = std::fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&shim, perm).unwrap();
    let orig_path = std::env::var_os("PATH");
    let joined = match &orig_path {
        Some(p) => format!("{}:{}", bin.path().display(), p.to_string_lossy()),
        None => bin.path().display().to_string(),
    };
    std::env::set_var("PATH", joined);
    FakeAgent {
        _dir: bin,
        orig_path,
    }
}

/// Seed one claude session into the (temp-home) default profile and return it.
fn seed_instance(title: &str) -> Instance {
    let storage = Storage::new_unwatched("default").expect("storage");
    let mut inst = Instance::new(title, "/tmp");
    inst.tool = "claude".to_string();
    let seeded = inst.clone();
    storage
        .update(|instances, _| {
            instances.push(inst);
            Ok(())
        })
        .unwrap();
    seeded
}

/// Create a real tmux pane and type a line into it (no Enter, so it is not run
/// as a shell command) so the capture has first-turn text.
fn create_pane(name: &str, typed: &str) {
    let status = Command::new("tmux")
        .arg("-S")
        .arg(tmux_socket())
        .args(["new-session", "-d", "-s", name])
        .status()
        .expect("tmux new-session");
    assert!(status.success(), "tmux new-session failed for {name}");
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(tmux_socket())
        .args(["send-keys", "-t", name, "-l", typed])
        .output();
    tmux::refresh_session_cache();
}

fn reload_title(id: &str) -> (String, bool) {
    let (instances, _) = Storage::new_unwatched("default")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let inst = instances.iter().find(|i| i.id == id).unwrap();
    (inst.title.clone(), inst.smart_rename_attempted)
}

#[tokio::test]
#[serial]
async fn renames_civ_named_terminal_session_from_first_turn() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let _home = setup_temp_home();
    let _bin = install_fake_claude("Fix login redirect");

    let inst = seed_instance("Vikings"); // civ default name
    let name = tmux::Session::generate_name(&inst.id, &inst.title);
    create_pane(&name, "implement the login redirect fix please");
    // The rename rekeys the live session, so also clean up the post-rename name.
    let renamed = tmux::Session::generate_name(&inst.id, "Fix login redirect");
    let _cleanup = TmuxCleanup(vec![name, renamed]);

    smart_rename::run_terminal_rename("default", &inst.id, false)
        .await
        .expect("run_terminal_rename");

    let (title, attempted) = reload_title(&inst.id);
    assert_eq!(title, "Fix login redirect");
    assert!(
        attempted,
        "a produced title must mark the session attempted"
    );
}

#[tokio::test]
#[serial]
async fn forced_rename_bypasses_a_prior_failed_attempt() {
    // Regression guard for the `(already && !force)` gate (#3058 review): a
    // still-civ-named session whose automatic one-shot already ran but produced
    // no usable title (smart_rename_attempted = true) must stay blocked on the
    // unforced path yet re-run under the manual "Auto-name now" force. Without
    // `!force` this test's forced call would no-op and the assertion fail.
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let _home = setup_temp_home();
    let _bin = install_fake_claude("Retry title wins");

    let inst = seed_instance("Britons"); // civ default name
    let id = inst.id.clone();
    // Simulate a prior automatic attempt that produced no usable title: the flag
    // is set while the civ name stays.
    Storage::new_unwatched("default")
        .unwrap()
        .update(|instances, _| {
            if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                i.smart_rename_attempted = true;
            }
            Ok(())
        })
        .unwrap();

    let name = tmux::Session::generate_name(&id, "Britons");
    create_pane(&name, "wire up the retry backoff");
    let renamed = tmux::Session::generate_name(&id, "Retry title wins");
    let _cleanup = TmuxCleanup(vec![name, renamed]);

    // Unforced: the attempted gate blocks the re-run, so the civ name stays.
    smart_rename::run_terminal_rename("default", &id, false)
        .await
        .expect("unforced run");
    assert_eq!(
        reload_title(&id).0,
        "Britons",
        "the attempted gate must block the unforced path"
    );

    // Forced: bypasses the attempted gate and renames.
    smart_rename::run_terminal_rename("default", &id, true)
        .await
        .expect("forced run");
    assert_eq!(
        reload_title(&id).0,
        "Retry title wins",
        "force must bypass the attempted gate"
    );
}

#[tokio::test]
#[serial]
async fn never_overwrites_a_manual_title() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let _home = setup_temp_home();
    let _bin = install_fake_claude("Should Not Apply");

    let inst = seed_instance("Zulu");
    let id = inst.id.clone();
    // Manually rename: diverges title from last_auto_title, freezing it.
    Storage::new_unwatched("default")
        .unwrap()
        .update(|instances, _| {
            if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                i.title = "Hand picked".to_string();
            }
            Ok(())
        })
        .unwrap();

    let name = tmux::Session::generate_name(&id, "Hand picked");
    create_pane(&name, "do something else entirely");
    // A manual title is not eligible, so no rename/rekey happens here.
    let _cleanup = TmuxCleanup(vec![name]);

    smart_rename::run_terminal_rename("default", &id, false)
        .await
        .expect("run_terminal_rename");

    let (title, _) = reload_title(&id);
    assert_eq!(
        title, "Hand picked",
        "a manual title must never be overwritten"
    );
}
