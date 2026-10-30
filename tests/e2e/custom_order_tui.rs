//! Custom row moves through the TUI survive a restart.

use std::time::Duration;

use serial_test::parallel;

use crate::harness::{require_tmux, wait_until, TuiTestHarness};

fn add_session(h: &TuiTestHarness, title: &str) {
    let project = h.project_path();
    let out = h.run_cli(&[
        "add",
        project.to_str().expect("utf8 project path"),
        "-t",
        title,
        "-g",
        "work",
        "-c",
        "claude",
    ]);
    assert!(
        out.status.success(),
        "aoe add {title} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Only sidebar titles count; the preview also names the selected session.
fn order_on_screen(h: &TuiTestHarness) -> Vec<String> {
    const LIST_COLUMNS: usize = 34;
    let screen = h.capture_screen();
    let mut seen: Vec<String> = Vec::new();
    for line in screen.lines() {
        let list: String = line.chars().take(LIST_COLUMNS).collect();
        for title in ["alpha-one", "beta-two"] {
            if list.contains(title) && !seen.iter().any(|s| s == title) {
                seen.push(title.to_string());
            }
        }
    }
    seen
}

fn wait_for_order(h: &TuiTestHarness, expected: &[String]) -> Vec<String> {
    wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
        let actual = order_on_screen(h);
        if actual == expected {
            Ok(actual)
        } else {
            Err(format!(
                "expected rows {expected:?}, got {actual:?}\n{}",
                h.capture_screen()
            ))
        }
    })
}

#[test]
#[parallel]
fn test_tui_custom_sort_reorders_and_persists() {
    require_tmux!();

    let mut h = TuiTestHarness::new("custom_order_tui");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 600\n").expect("write fake claude");

    add_session(&h, "alpha-one");
    add_session(&h, "beta-two");

    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("alpha-one");
    h.wait_for("beta-two");

    // The picker's `c` mnemonic applies Custom immediately.
    h.send_keys("C-o");
    h.wait_for(" Sort Order ");
    h.send_keys("c");
    h.wait_for("Custom");
    h.wait_for_absent(" Sort Order ", Duration::from_secs(10));
    let before = order_on_screen(&h);
    assert_eq!(before.len(), 2, "both sessions on screen: {before:?}");

    // Home selects the group header; Down selects its first session.
    h.send_keys("Home");
    h.send_keys("Down");
    h.send_keys("C-Down");
    let expected = vec![before[1].clone(), before[0].clone()];
    // Input ACK fences submission, not the daemon's committed publication.
    let moved = wait_for_order(&h, &expected);
    h.wait_for_runtime_ready();

    // A fresh TUI must display the persisted order.
    h.kill_tui();
    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("alpha-one");
    h.wait_for("beta-two");
    wait_for_order(&h, &moved);

    // Both siblings received their new indices.
    let rows: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(h.sessions_path()).expect("read sessions.json"),
    )
    .expect("parse sessions.json");
    let index_of = |title: &str| -> Option<u64> {
        rows.as_array()?
            .iter()
            .find(|r| r["title"] == title)?
            .get("sort_index")?
            .as_u64()
    };
    assert_eq!(index_of(&moved[0]), Some(0), "moved row stored first");
    assert_eq!(index_of(&moved[1]), Some(1), "its sibling stored second");
}
