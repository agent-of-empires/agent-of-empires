//! Cross-process purge/restore claim e2e (#2541). Uses the real `aoe` binary as
//! the two "processes": a session is trashed via the real trash flow, then an
//! on-disk `op_claim` is injected into sessions.json to simulate a peer holding
//! it mid-purge / mid-restore under the storage flock. Deterministic: the claim
//! TTL is 10 minutes, so a `now()` claim is always fresh, and no timing/tmux
//! coordination is needed for the claim contract.
//!
//! The mid-flight `Restored` / `PurgeStoleClaim` transitions (state changing
//! between a command's resolve-read and its locked claim/commit) require genuine
//! concurrency at the flock, which a single injected file cannot produce inside
//! one process; those are covered by the unit tests in `session::claim`. Here we
//! pin the deterministic cross-process contract: a fresh peer claim on disk
//! refuses the opposing operation, and a clean trashed session purges.

use serial_test::serial;

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

/// Inject a FRESH on-disk claim (`op` = "purge" | "restore") onto the row titled
/// `title`, simulating a peer holding it mid-operation under the flock. The `op`
/// must be lowercase to match `#[serde(rename_all = "lowercase")]` on `ClaimOp`.
fn inject_claim(h: &TuiTestHarness, title: &str, op: &str) {
    let mut v = read_sessions_json(h);
    let row = v
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["title"] == title)
        .expect("row present for claim injection");
    row["op_claim"] = serde_json::json!({ "op": op, "at": chrono::Utc::now().to_rfc3339() });
    write_sessions(h, &v);
}

fn clear_claim(h: &TuiTestHarness, title: &str) {
    let mut v = read_sessions_json(h);
    let row = v
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["title"] == title)
        .expect("row present for claim clear");
    row.as_object_mut().unwrap().remove("op_claim");
    write_sessions(h, &v);
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

/// (a) `session restore` is refused while a fresh on-disk Purge claim holds the
/// row (a peer is mid-purge), and succeeds once that claim clears.
#[test]
#[serial]
fn restore_refused_while_purge_claim_present_then_succeeds() {
    let h = TuiTestHarness::new("purge_restore_race_restore");
    create_trashed(&h, "RaceRestore");

    inject_claim(&h, "RaceRestore", "purge");

    let refused = h.run_cli(&["session", "restore", "RaceRestore"]);
    assert!(
        !refused.status.success(),
        "restore must be refused while a Purge claim holds the row"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("is being purged by another process, so it was not restored"),
        "unexpected stderr:\n{stderr}"
    );
    // Refusal leaves the row trashed and the peer's claim intact.
    let after = read_sessions_json(&h);
    let row = row_title(&after, "RaceRestore").expect("row kept on refusal");
    assert!(row.get("trashed_at").is_some(), "row must stay trashed");
    assert_eq!(
        row["op_claim"]["op"], "purge",
        "peer's Purge claim must be untouched"
    );

    // Peer finished: claim cleared -> restore now lands.
    clear_claim(&h, "RaceRestore");
    let ok = h.run_cli(&["session", "restore", "RaceRestore"]);
    assert!(
        ok.status.success(),
        "restore must succeed once the claim clears:\nstdout: {}\nstderr: {}",
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
        row.get("op_claim").is_none(),
        "restore must clear its own claim"
    );
}

/// (b) symmetry: `rm --purge` is refused and KEEPS the row while a fresh
/// on-disk Restore claim holds it (a peer is mid-restore).
#[test]
#[serial]
fn purge_refused_while_restore_claim_present() {
    let h = TuiTestHarness::new("purge_restore_race_purge");
    create_trashed(&h, "RacePurge");

    inject_claim(&h, "RacePurge", "restore");

    let refused = h.run_cli(&["rm", "--purge", "RacePurge"]);
    assert!(
        !refused.status.success(),
        "purge must be refused while a Restore claim holds the row"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("is being restored by another process, so it was not purged"),
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
        row["op_claim"]["op"], "restore",
        "peer's Restore claim must be untouched"
    );
}

/// (c) A trashed session with NO competing claim purges cleanly and is dropped.
#[test]
#[serial]
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
