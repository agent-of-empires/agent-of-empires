//! Cross-process lifecycle ownership e2e. A session is trashed through the
//! real CLI, then a fresh unified lifecycle reservation is injected into
//! `sessions.json` to simulate a peer transition. The opposing operation must
//! refuse the row without disturbing the peer's reservation.

use serial_test::parallel;

use crate::harness::TuiTestHarness;

fn sessions_path(h: &TuiTestHarness) -> std::path::PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn read_sessions_json(h: &TuiTestHarness) -> serde_json::Value {
    let p = sessions_path(h);
    serde_json::from_str(
        &std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
    )
    .expect("sessions.json is valid JSON")
}

fn write_sessions(h: &TuiTestHarness, v: &serde_json::Value) {
    std::fs::write(sessions_path(h), serde_json::to_string_pretty(v).unwrap())
        .expect("write sessions.json");
}

fn row_title<'a>(v: &'a serde_json::Value, title: &str) -> Option<&'a serde_json::Value> {
    v.as_array()?.iter().find(|r| r["title"] == title)
}

/// Inject a fresh unified lifecycle reservation onto the named row.
fn inject_reservation(h: &TuiTestHarness, title: &str, operation: &str) {
    let mut value = read_sessions_json(h);
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
    let mut value = read_sessions_json(h);
    let row = value
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["title"] == title)
        .expect("row present for reservation clear");
    row.as_object_mut().unwrap().remove("lifecycle_reservation");
    write_sessions(h, &value);
}

/// Create a scratch session and move it to the trash via the real trash-first
/// `rm` flow (`session.delete_to_trash` defaults on). A scratch session has no
/// managed worktree, so no relocation happens and restore later takes the
/// no-op worktree path, keeping the test deterministic.
fn create_trashed(h: &TuiTestHarness, title: &str) {
    let add = h.run_cli(&["add", "--scratch", "-t", title]);
    assert!(
        add.status.success(),
        "aoe add --scratch failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let rm = h.run_cli(&["rm", title]);
    assert!(
        rm.status.success(),
        "aoe rm (trash-first) failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rm.stdout),
        String::from_utf8_lossy(&rm.stderr),
    );
    let v = read_sessions_json(h);
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

    let refused = h.run_cli(&["session", "restore", "RaceRestore"]);
    assert!(
        !refused.status.success(),
        "restore must be refused while a Purge claim holds the row"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("busy with lifecycle operation Purge"),
        "unexpected stderr:\n{stderr}"
    );
    // Refusal leaves the row trashed and the peer's reservation intact.
    let after = read_sessions_json(&h);
    let row = row_title(&after, "RaceRestore").expect("row kept on refusal");
    assert!(row.get("trashed_at").is_some(), "row must stay trashed");
    assert_eq!(
        row["lifecycle_reservation"]["op"], "purge",
        "peer's Purge reservation must be untouched"
    );

    // Peer finished: reservation cleared, so restore now lands.
    clear_reservation(&h, "RaceRestore");
    let ok = h.run_cli(&["session", "restore", "RaceRestore"]);
    assert!(
        ok.status.success(),
        "restore must succeed once the reservation clears:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr),
    );
    assert!(String::from_utf8_lossy(&ok.stdout).contains("Restored: RaceRestore"));
    let done = read_sessions_json(&h);
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
/// reservation owns it.
#[test]
#[parallel]
fn purge_refused_while_restore_reservation_present() {
    let h = TuiTestHarness::new("purge_restore_race_purge");
    create_trashed(&h, "RacePurge");

    inject_reservation(&h, "RacePurge", "restore");

    let refused = h.run_cli(&["rm", "--purge", "RacePurge"]);
    assert!(
        !refused.status.success(),
        "purge must be refused while a Restore claim holds the row"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("lifecycle operation Restore is already in progress"),
        "unexpected stderr:\n{stderr}"
    );
    // The row must survive with the peer's Restore claim intact.
    let after = read_sessions_json(&h);
    let row = row_title(&after, "RacePurge").expect("row must be kept when purge is refused");
    assert!(
        row.get("trashed_at").is_some(),
        "kept row must still be trashed"
    );
    assert_eq!(
        row["lifecycle_reservation"]["op"], "restore",
        "peer's Restore reservation must be untouched"
    );
}

/// A trashed session with no competing reservation purges cleanly.
#[test]
#[parallel]
fn purge_trashed_no_claim_removes_row() {
    let h = TuiTestHarness::new("purge_restore_race_clean");
    create_trashed(&h, "RaceClean");

    let ok = h.run_cli(&["rm", "--purge", "RaceClean"]);
    assert!(
        ok.status.success(),
        "clean purge must succeed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr),
    );
    assert!(String::from_utf8_lossy(&ok.stdout).contains("Removed session: RaceClean"));
    let after = read_sessions_json(&h);
    assert!(
        row_title(&after, "RaceClean").is_none(),
        "purged row must be gone from disk"
    );
}
