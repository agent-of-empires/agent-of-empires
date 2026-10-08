use serial_test::parallel;
use std::io::Write;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[parallel]
fn test_space_chords_are_named_in_live_banner_and_leader_menu() {
    require_tmux!();

    let mut h = TuiTestHarness::new("live_send_space_labels");
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(app_dir_in(h.home_path()).join("config.toml"))
        .expect("open config");
    writeln!(
        config,
        r#"
[session]
default_attach_mode = "live_send"
live_send_leader = "C-Space"
live_send_exit_chord = "M-Space,C-q"
"#
    )
    .expect("write live-send chords");

    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 300\n")
        .expect("write persistent agent stub");
    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Space labels"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Space labels");
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    h.wait_for("Alt+Space / Ctrl+Q to exit");
    h.assert_screen_contains("Ctrl+Space menu");
    println!("Live banner:\n{}", h.capture_screen());

    h.send_keys("C-Space");
    h.wait_for("Ctrl+Space:  k palette");
    println!("Leader menu:\n{}", h.capture_screen());

    h.send_keys("q");
    h.wait_for_absent("LIVE", Duration::from_secs(5));
}
