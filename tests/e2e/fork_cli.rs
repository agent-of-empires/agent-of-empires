//! End-to-end coverage for `aoe add --fork-from` (terminal fork).
//!
//! These tests drive the real `aoe` binary as a subprocess (`run_cli`, no
//! tmux) and assert on the persisted `sessions.json`, so the fork gate is
//! exercised through the full CLI surface without a live agent.
//!
//! The happy path needs a parent with a captured `agent_session_id`. A freshly
//! created session has none (no conversation has happened), and the CLI has no
//! way to set one, so the test seeds it by hand-editing the persisted parent
//! before forking. That makes the assertion deterministic: the forked child
//! must carry a fresh, distinct `agent_session_id` and a one-shot
//! `Fork { from: <parent id> }` resume intent.
//!
//! The two denial paths need no seeding and prove the gate end to end: forking
//! a parent that never captured an agent session is refused, and forking with
//! an agent that has no fork capability is refused. `aoe add` (without
//! `--launch`) does not spawn the agent or use tmux, so all three tests run
//! deterministically anywhere `cargo test` runs.

use serial_test::serial;

use crate::harness::TuiTestHarness;

fn sessions_path(h: &TuiTestHarness) -> std::path::PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn read_sessions(h: &TuiTestHarness) -> serde_json::Value {
    let path = sessions_path(h);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

fn session_by_title<'a>(sessions: &'a serde_json::Value, title: &str) -> &'a serde_json::Value {
    sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .unwrap_or_else(|| panic!("no session titled '{title}' in sessions.json"))
}

/// Scratch sessions provision their working directory under
/// `<app_dir>/scratch/<id>/`. A refused fork must not leave anything here.
fn scratch_root(h: &TuiTestHarness) -> std::path::PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("scratch")
}

/// Forking a session whose conversation has been observed pre-pins a fresh
/// child agent id and a one-shot `Fork` resume intent pointing at the parent's
/// captured id. The parent's own id is left untouched.
#[test]
#[serial]
fn fork_from_seeds_child_with_fork_intent() {
    let h = TuiTestHarness::new("fork_cli_happy");
    let project = h.project_path();

    // Parent: a plain claude session (the stub satisfies the PATH check).
    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "ForkParent",
    ]);
    assert!(
        parent.status.success(),
        "aoe add parent failed: {}",
        String::from_utf8_lossy(&parent.stderr)
    );

    // Seed the parent's captured agent session id by hand: a real conversation
    // would set this, but the CLI cannot, and the fork gate keys off it.
    let parent_agent_id = "11111111-2222-3333-4444-555555555555";
    let mut sessions = read_sessions(&h);
    let arr = sessions.as_array_mut().expect("sessions array");
    let parent_obj = arr
        .iter_mut()
        .find(|s| s["title"].as_str() == Some("ForkParent"))
        .expect("parent session present");
    parent_obj["agent_session_id"] = serde_json::Value::String(parent_agent_id.to_string());
    std::fs::write(
        sessions_path(&h),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write seeded sessions.json");

    // Child: fork from the parent by title.
    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "ForkChild",
        "--fork-from",
        "ForkParent",
    ]);
    assert!(
        child.status.success(),
        "aoe add --fork-from failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );

    let sessions = read_sessions(&h);
    let child_obj = session_by_title(&sessions, "ForkChild");

    let child_agent_id = child_obj["agent_session_id"]
        .as_str()
        .expect("forked child must pre-pin a fresh agent_session_id");
    assert!(
        !child_agent_id.is_empty(),
        "child agent_session_id must be non-empty"
    );
    assert_ne!(
        child_agent_id, parent_agent_id,
        "child must fork into a NEW id, not reuse the parent's"
    );

    let resume_intent = &child_obj["resume_intent"];
    assert_eq!(
        resume_intent["kind"].as_str(),
        Some("Fork"),
        "child resume_intent must be a one-shot Fork, got: {resume_intent:?}"
    );
    assert_eq!(
        resume_intent["value"]["from"].as_str(),
        Some(parent_agent_id),
        "Fork intent must resume the parent's captured agent id, got: {resume_intent:?}"
    );

    // The parent is left untouched: same captured id, no fork intent.
    let parent_obj = session_by_title(&sessions, "ForkParent");
    assert_eq!(
        parent_obj["agent_session_id"].as_str(),
        Some(parent_agent_id),
        "parent's captured id must be unchanged"
    );
    assert!(
        parent_obj["resume_intent"].is_null()
            || parent_obj["resume_intent"]["kind"].as_str() == Some("Default"),
        "parent must not gain a Fork intent, got: {:?}",
        parent_obj["resume_intent"]
    );
}

/// Forking a parent that never captured an agent session is refused with a
/// clear "Nothing to fork" message, and no child session is persisted.
#[test]
#[serial]
fn fork_from_parent_without_agent_session_is_refused() {
    let h = TuiTestHarness::new("fork_cli_no_parent_sid");
    let project = h.project_path();

    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "BareParent",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");

    // No agent_session_id seeded: the parent has no captured conversation.
    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "WouldBeChild",
        "--fork-from",
        "BareParent",
    ]);
    assert!(
        !child.status.success(),
        "fork from a session with no captured agent id must fail"
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("Nothing to fork"),
        "expected a 'Nothing to fork' message, got: {stderr}"
    );

    let sessions = read_sessions(&h);
    assert!(
        sessions
            .as_array()
            .map(|arr| arr
                .iter()
                .all(|s| s["title"].as_str() != Some("WouldBeChild")))
            .unwrap_or(true),
        "no child session should have been persisted on a refused fork"
    );
}

/// A refused `--scratch --fork-from` must not orphan a scratch directory. The
/// fork-eligibility gate runs before scratch provisioning, so a denial returns
/// before any directory is created: the scratch root stays empty (or absent),
/// and no child session is persisted.
#[test]
#[serial]
fn refused_scratch_fork_leaves_no_orphaned_dir() {
    let h = TuiTestHarness::new("fork_cli_scratch_no_leak");
    let project = h.project_path();

    // A bare parent with no captured agent session: enough to make the fork
    // gate refuse on the NoParentSession path.
    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "ScratchForkParent",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");

    // The parent is a project session, so nothing has touched the scratch root
    // yet. A successful scratch fork would create <app_dir>/scratch/<id>/.
    let scratch_root = scratch_root(&h);
    assert!(
        !scratch_root.exists()
            || std::fs::read_dir(&scratch_root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "scratch root should be empty before the fork attempt"
    );

    let child = h.run_cli(&[
        "add",
        "--scratch",
        "--cmd",
        "claude",
        "-t",
        "ScratchForkChild",
        "--fork-from",
        "ScratchForkParent",
    ]);
    assert!(
        !child.status.success(),
        "scratch fork from a session with no captured agent id must fail"
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("Nothing to fork"),
        "expected a 'Nothing to fork' message, got: {stderr}"
    );

    // Leak check: the denial fires before scratch provisioning, so no scratch
    // directory was created.
    assert!(
        !scratch_root.exists()
            || std::fs::read_dir(&scratch_root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "refused scratch fork must not leave an orphaned scratch dir under {}",
        scratch_root.display()
    );

    // And no child session was persisted.
    let sessions = read_sessions(&h);
    assert!(
        sessions
            .as_array()
            .map(|arr| arr
                .iter()
                .all(|s| s["title"].as_str() != Some("ScratchForkChild")))
            .unwrap_or(true),
        "no child session should have been persisted on a refused fork"
    );
}

/// Forking from a session whose own fork has not launched yet is refused. Such
/// a source still carries a one-shot `Fork` resume intent and a pre-pinned
/// child `agent_session_id` that no agent has written, so forking from it would
/// resume a conversation that does not exist. The gate keys off `resume_intent`,
/// not the (synthetic) captured id.
#[test]
#[serial]
fn fork_from_unlaunched_fork_is_refused() {
    let h = TuiTestHarness::new("fork_cli_unlaunched_fork");
    let project = h.project_path();

    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "PendingFork",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");

    // Make the source look like an unlaunched fork: a pre-pinned (synthetic)
    // child id plus a one-shot Fork intent, exactly what a forked-but-not-yet-
    // started session persists.
    let mut sessions = read_sessions(&h);
    let arr = sessions.as_array_mut().expect("sessions array");
    let src = arr
        .iter_mut()
        .find(|s| s["title"].as_str() == Some("PendingFork"))
        .expect("source session present");
    src["agent_session_id"] =
        serde_json::Value::String("99999999-8888-7777-6666-555555555555".to_string());
    src["resume_intent"] = serde_json::json!({
        "kind": "Fork",
        "value": { "from": "11111111-2222-3333-4444-555555555555" }
    });
    std::fs::write(
        sessions_path(&h),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write seeded sessions.json");

    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "WouldBeGrandchild",
        "--fork-from",
        "PendingFork",
    ]);
    assert!(
        !child.status.success(),
        "fork from an unlaunched fork must fail"
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("its own fork has not launched yet"),
        "expected an 'unlaunched fork' message, got: {stderr}"
    );

    let sessions = read_sessions(&h);
    assert!(
        sessions
            .as_array()
            .map(|arr| arr
                .iter()
                .all(|s| s["title"].as_str() != Some("WouldBeGrandchild")))
            .unwrap_or(true),
        "no child session should have been persisted on a refused fork"
    );
}

/// `--fork-from` is a terminal-only fork; combining it with `--structured-view`
/// would write terminal fork state onto a structured session. The CLI rejects
/// the combination up front. This flag only exists in `--features serve`.
#[cfg(feature = "serve")]
#[test]
#[serial]
fn fork_from_with_structured_view_is_refused() {
    let h = TuiTestHarness::new("fork_cli_structured_reject");
    let project = h.project_path();

    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "StructForkParent",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");

    // Seed a captured id so the request would otherwise reach the seed apply
    // step: the structured-view rejection must fire regardless.
    let parent_agent_id = "abcdef00-1111-2222-3333-444444444444";
    let mut sessions = read_sessions(&h);
    sessions
        .as_array_mut()
        .expect("sessions array")
        .iter_mut()
        .find(|s| s["title"].as_str() == Some("StructForkParent"))
        .expect("parent present")["agent_session_id"] =
        serde_json::Value::String(parent_agent_id.to_string());
    std::fs::write(
        sessions_path(&h),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write seeded sessions.json");

    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "StructForkChild",
        "--fork-from",
        "StructForkParent",
        "--structured-view",
    ]);
    assert!(
        !child.status.success(),
        "--fork-from --structured-view must fail"
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("cannot be combined with"),
        "expected an incompatible-flags message, got: {stderr}"
    );

    let sessions = read_sessions(&h);
    assert!(
        sessions
            .as_array()
            .map(|arr| arr
                .iter()
                .all(|s| s["title"].as_str() != Some("StructForkChild")))
            .unwrap_or(true),
        "no child session should have been persisted on a rejected combination"
    );
}

/// Forking with an agent whose CLI has no fork capability is refused with a
/// clear message naming the agent. `gemini` is resume-only (no fork flag); a
/// PATH stub lets `aoe add --tool gemini` reach the fork gate without the real
/// binary installed.
#[test]
#[serial]
fn fork_from_unforkable_agent_is_refused() {
    let mut h = TuiTestHarness::new("fork_cli_unforkable");
    let project = h.project_path();

    // Stub `gemini` on PATH so the availability check passes and the fork gate
    // is the only thing that can reject the request.
    h.install_path_command("gemini");

    // A parent in the same project; its tool is irrelevant since the child's
    // own tool (gemini) is what the fork gate checks.
    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "GemParent",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");

    let parent_agent_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let mut sessions = read_sessions(&h);
    let arr = sessions.as_array_mut().expect("sessions array");
    arr.iter_mut()
        .find(|s| s["title"].as_str() == Some("GemParent"))
        .expect("parent present")["agent_session_id"] =
        serde_json::Value::String(parent_agent_id.to_string());
    std::fs::write(
        sessions_path(&h),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write seeded sessions.json");

    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "gemini",
        "-t",
        "GemChild",
        "--fork-from",
        "GemParent",
    ]);
    assert!(
        !child.status.success(),
        "fork with an unforkable agent must fail"
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("does not support forking"),
        "expected a 'does not support forking' message, got: {stderr}"
    );
}
