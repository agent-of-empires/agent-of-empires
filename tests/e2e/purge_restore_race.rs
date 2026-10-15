//! Cross-process lifecycle ownership e2e. A session is trashed through the
//! real CLI, then a fresh unified lifecycle reservation is injected into
//! `sessions.json` to simulate a peer transition. The opposing operation must
//! refuse the row without disturbing the peer's reservation.

use serial_test::parallel;

use crate::harness::TuiTestHarness;

fn write_sessions(h: &TuiTestHarness, v: &serde_json::Value) {
    std::fs::write(h.sessions_path(), serde_json::to_string_pretty(v).unwrap())
        .expect("write sessions.json");
}

fn row_title<'a>(v: &'a serde_json::Value, title: &str) -> Option<&'a serde_json::Value> {
    v.as_array()?.iter().find(|r| r["title"] == title)
}

/// Inject a fresh unified lifecycle reservation onto the named row.
fn inject_reservation(h: &TuiTestHarness, title: &str, operation: &str) {
    let mut value = h.read_sessions();
    let row = value
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["title"] == title)
        .expect("row present for reservation injection");
    let generation = row["lifecycle_generation"].as_u64().unwrap_or(0) + 1;
    row["lifecycle_generation"] = serde_json::json!(generation);
    row["lifecycle_reservation"] = serde_json::json!({
        "op": operation,
        "generation": generation,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    write_sessions(h, &value);
}

fn clear_reservation(h: &TuiTestHarness, title: &str) {
    let mut value = h.read_sessions();
    let row = value
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["title"] == title)
        .expect("row present for reservation clear");
    row.as_object_mut().unwrap().remove("lifecycle_reservation");
    write_sessions(h, &value);
}

/// Trash a scratch session through the real trash-first `rm` flow. A scratch
/// session has no managed worktree, so restore later takes the no-op path.
fn create_trashed(h: &TuiTestHarness, title: &str) {
    h.run_cli_ok(&["add", "--scratch", "-t", title]);
    h.run_cli_ok(&["rm", title]);
    let v = h.read_sessions();
    let row = row_title(&v, title).expect("row present after trash");
    assert!(
        row.get("trashed_at").is_some(),
        "row must be trashed after `aoe rm`"
    );
}

/// Restore is refused while a fresh Purge reservation owns the row, then
/// succeeds after that reservation clears.
#[test]
#[parallel]
fn restore_refused_while_purge_reservation_present_then_succeeds() {
    let h = TuiTestHarness::new("purge_restore_race_restore");
    create_trashed(&h, "RaceRestore");

    inject_reservation(&h, "RaceRestore", "purge");

    let stderr = h.run_cli_err(&["session", "restore", "RaceRestore"]);
    assert!(
        stderr.contains("busy with lifecycle operation Purge"),
        "unexpected stderr:\n{stderr}"
    );
    // Refusal leaves the row trashed and the peer's reservation intact.
    let after = h.read_sessions();
    let row = row_title(&after, "RaceRestore").expect("row kept on refusal");
    assert!(row.get("trashed_at").is_some(), "row must stay trashed");
    assert_eq!(
        row["lifecycle_reservation"]["op"], "purge",
        "peer's Purge reservation must be untouched"
    );

    // Peer finished: reservation cleared, so restore now lands.
    clear_reservation(&h, "RaceRestore");
    let stdout = h.run_cli_ok(&["session", "restore", "RaceRestore"]);
    assert!(stdout.contains("Restored: RaceRestore"), "{stdout}");
    let done = h.read_sessions();
    let row = row_title(&done, "RaceRestore").expect("row still present after restore");
    assert!(
        row.get("trashed_at").is_none(),
        "restored row must be untrashed"
    );
    assert!(
        row.get("lifecycle_reservation").is_none(),
        "restore must clear its own reservation"
    );
}

/// Symmetry: purge is refused and keeps the row while a fresh Restore
/// reservation owns it, and purges cleanly once no reservation competes.
#[test]
#[parallel]
fn purge_refused_while_restore_reservation_present_then_removes_row() {
    let h = TuiTestHarness::new("purge_restore_race_purge");
    create_trashed(&h, "RacePurge");

    inject_reservation(&h, "RacePurge", "restore");

    let stderr = h.run_cli_err(&["rm", "--purge", "RacePurge"]);
    assert!(
        stderr.contains("lifecycle operation Restore is already in progress"),
        "unexpected stderr:\n{stderr}"
    );
    // The row must survive with the peer's Restore claim intact.
    let after = h.read_sessions();
    let row = row_title(&after, "RacePurge").expect("row must be kept when purge is refused");
    assert!(
        row.get("trashed_at").is_some(),
        "kept row must still be trashed"
    );
    assert_eq!(
        row["lifecycle_reservation"]["op"], "restore",
        "peer's Restore reservation must be untouched"
    );

    clear_reservation(&h, "RacePurge");
    let stdout = h.run_cli_ok(&["rm", "--purge", "RacePurge"]);
    assert!(stdout.contains("Removed session: RacePurge"), "{stdout}");
    assert!(
        row_title(&h.read_sessions(), "RacePurge").is_none(),
        "purged row must be gone from disk"
    );
}
