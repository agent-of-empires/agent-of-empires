//! e2e: the Custom sort order and the keys that arrange rows inside it, driven through a
//! real TUI: the picker's mnemonic applies an order, Ctrl+Down moves a session past its
//! sibling, and the move survives a restart because it is stored rather than recomputed.

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

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

/// Row order the session list shows. Only the list column counts: the preview pane to its
/// right carries the selected session's title too, and it is drawn above the rows.
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
    h.wait_for("alpha-one");
    h.wait_for("beta-two");
    let before = order_on_screen(&h);
    assert_eq!(before.len(), 2, "both sessions on screen: {before:?}");

    // Ctrl+O opens the sort picker; `c` is Custom's mnemonic and applies it outright.
    h.send_keys("C-o");
    h.wait_for(" Sort Order ");
    h.send_keys("c");
    h.wait_for("Custom");

    // Put the cursor on the first of the two and move it past the second.
    h.send_keys("Down");
    h.send_keys("C-Down");
    let expected = vec![before[1].clone(), before[0].clone()];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let moved = loop {
        let order = order_on_screen(&h);
        if order == expected {
            break order;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reorder was not acknowledged: {order:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(
        moved,
        vec![before[1].clone(), before[0].clone()],
        "the rows swapped"
    );

    // A fresh TUI reads the order back from disk: this is the claim the feature makes, and
    // asserting only on the stored indices would not test it.
    h.kill_tui();
    h.spawn_tui();
    h.wait_for("alpha-one");
    h.wait_for("beta-two");
    assert_eq!(
        order_on_screen(&h),
        moved,
        "a restarted TUI shows the moved order"
    );

    // And the indices themselves are what the move assigned.
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
