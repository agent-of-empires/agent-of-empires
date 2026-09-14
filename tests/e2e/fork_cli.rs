//! Public CLI fork eligibility, parent isolation and refusal side effects.

use serial_test::parallel;

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

#[test]
#[parallel]
fn fork_from_seeds_child_with_fork_intent() {
    let h = TuiTestHarness::new("fork_cli_happy");
    let project = h.project_path();

    let parent_agent_id = seed_claude_parent(&h, &project, "ForkParent");
    let before = read_sessions(&h);
    let parent_before = session_by_title(&before, "ForkParent").clone();

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
        "--extra-args",
        "--append-system-prompt resume",
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
        Some(parent_agent_id.as_str()),
        "Fork intent must target the asserted parent conversation"
    );
    assert_eq!(session_by_title(&sessions, "ForkParent"), &parent_before);
}

/// Create a parent with an explicitly asserted native conversation.
fn seed_claude_parent(h: &TuiTestHarness, project: &std::path::Path, title: &str) -> String {
    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        title,
    ]);
    assert!(parent.status.success(), "aoe add parent '{title}' failed");
    let parent_agent_id = "11111111-2222-3333-4444-555555555555";
    let assertion = h.run_cli(&["session", "set-session-id", title, parent_agent_id]);
    assert!(
        assertion.status.success(),
        "assert conversation: {}",
        String::from_utf8_lossy(&assertion.stderr)
    );
    parent_agent_id.to_string()
}

fn install_dispatch_marker(h: &mut TuiTestHarness, agent: &str) {
    let bin = h.install_path_command(agent);
    let marker = h.home_path().join("native-spawn");
    std::fs::write(
        bin.join(agent),
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf dispatched > {}\n",
            shell_words::quote(&marker.to_string_lossy()),
        ),
    )
    .unwrap();
}

fn assert_launch_refused(h: &TuiTestHarness, title: &str) {
    let before = read_sessions(h);
    let output = h.run_cli(&["session", "start", title]);
    assert!(
        !output.status.success(),
        "invalid native fork must not launch"
    );
    assert!(
        !h.home_path().join("native-spawn").exists(),
        "native dispatch must not occur"
    );
    let after = read_sessions(h);
    for field in [
        "agent_session_id",
        "agent_session_binding",
        "resume_intent",
        "resume_binding",
        "active_execution",
        "pi_session_path",
    ] {
        assert_eq!(
            session_by_title(&before, title)[field],
            session_by_title(&after, title)[field],
            "refusal changed {field}"
        );
    }
}

/// A mismatched agent is refused at launch without discarding the fork target.
#[test]
#[parallel]
fn fork_from_mismatched_tool_is_refused_at_launch_but_inherits_when_unset() {
    let mut h = TuiTestHarness::new("fork_cli_tool_match");
    let project = h.project_path();
    install_dispatch_marker(&mut h, "gemini");
    seed_claude_parent(&h, &project, "MatchParent");

    let mismatched = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "gemini",
        "-t",
        "MismatchChild",
        "--fork-from",
        "MatchParent",
    ]);
    assert!(
        mismatched.status.success(),
        "a valid parent seed can be queued"
    );
    assert_launch_refused(&h, "MismatchChild");
    // No --tool/--cmd: inherits the parent's agent (claude) and succeeds.
    let inherited = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "InheritChild",
        "--fork-from",
        "MatchParent",
    ]);
    assert!(
        inherited.status.success(),
        "fork with no explicit tool must inherit the parent's agent and succeed: {}",
        String::from_utf8_lossy(&inherited.stderr)
    );
    let sessions = read_sessions(&h);
    assert_eq!(
        session_by_title(&sessions, "InheritChild")["tool"].as_str(),
        Some("claude"),
        "inherited fork must run the parent's agent"
    );
}

/// Filesystem conflicts fail before provisioning; native selectors fail before dispatch.
#[test]
#[parallel]
fn fork_from_rejects_conflicting_flags() {
    let mut h = TuiTestHarness::new("fork_cli_flag_mutex");
    install_dispatch_marker(&mut h, "claude");
    let project = h.project_path();
    seed_claude_parent(&h, &project, "FenceParent");

    let base = |extra: &[&str]| {
        let mut args = vec!["add", project.to_str().unwrap(), "--cmd", "claude"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--fork-from", "FenceParent"]);
        args.into_iter().map(str::to_string).collect::<Vec<_>>()
    };

    for (label, extra) in [
        ("worktree", vec!["-t", "W", "--worktree", "wt-branch"]),
        ("scratch", vec!["-t", "S", "--scratch"]),
        ("sandbox", vec!["-t", "B", "--sandbox"]),
    ] {
        let args = base(&extra);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = h.run_cli(&refs);
        assert!(
            !out.status.success(),
            "`--fork-from` with --{label} must be refused"
        );
    }

    let child = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude --resume abc",
        "-t",
        "SelectorChild",
        "--fork-from",
        "FenceParent",
    ]);
    assert!(child.status.success(), "a valid parent seed can be queued");
    assert_launch_refused(&h, "SelectorChild");

    // --cmd-override swaps the binary out from under the tool, decoupling it
    // from the parent's agent and its fork flags; rejected.
    let out = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd-override",
        "some-other-binary",
        "-t",
        "O",
        "--fork-from",
        "FenceParent",
    ]);
    assert!(
        !out.status.success(),
        "`--fork-from` with --cmd-override must be refused"
    );
}

/// Forking a parent that never captured an agent session is refused with a
/// clear "Nothing to fork" message, and no child session is persisted.
#[test]
#[parallel]
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

/// `--scratch --fork-from` is refused (a scratch session runs in a fresh temp
/// dir, so the fork could not resume the parent's conversation), and the refusal
/// must not orphan a scratch directory. The mutex fires before scratch
/// provisioning, so the scratch root stays empty (or absent) and no child
/// session is persisted.
#[test]
#[parallel]
fn refused_scratch_fork_leaves_no_orphaned_dir() {
    let h = TuiTestHarness::new("fork_cli_scratch_no_leak");
    let project = h.project_path();

    seed_claude_parent(&h, &project, "ScratchForkParent");

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
        "a scratch fork must be refused (scratch cwd cannot resume the parent)"
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

/// A fork child cannot be forked before its first qualified observation.
#[test]
#[parallel]
fn fork_from_unlaunched_fork_is_refused() {
    let h = TuiTestHarness::new("fork_cli_unlaunched_fork");
    let project = h.project_path();
    seed_claude_parent(&h, &project, "Parent");
    let pending = h.run_cli(&["add", "--fork-from", "Parent", "-t", "PendingFork"]);
    assert!(
        pending.status.success(),
        "create pending fork: {}",
        String::from_utf8_lossy(&pending.stderr)
    );

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
/// the combination up front, BEFORE provisioning a scratch directory, so the
/// refusal leaks nothing. Using `--scratch` here makes that leak observable:
/// were the rejection still buried in the post-creation view block, a scratch
/// dir would already exist by the time it fired.
#[test]
#[parallel]
fn fork_from_with_structured_view_is_refused() {
    let h = TuiTestHarness::new("fork_cli_structured_reject");
    let project = h.project_path();

    seed_claude_parent(&h, &project, "StructForkParent");

    // The parent is a project session, so nothing has touched the scratch root
    // yet. A scratch fork that got past the rejection would create
    // <app_dir>/scratch/<id>/.
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
        "StructForkChild",
        "--fork-from",
        "StructForkParent",
        "--structured-view",
    ]);
    assert!(
        !child.status.success(),
        "--fork-from --structured-view must fail"
    );
    assert!(
        !scratch_root.exists()
            || std::fs::read_dir(&scratch_root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "refused structured fork must not leave an orphaned scratch dir under {}",
        scratch_root.display()
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
#[parallel]
fn fork_from_unforkable_agent_is_refused() {
    let mut h = TuiTestHarness::new("fork_cli_unforkable");
    let project = h.project_path();

    // Stub `gemini` on PATH so the availability check passes and the fork gate
    // is the only thing that can reject the request.
    h.install_path_command("gemini");

    // The parent must use the SAME agent as the fork (gemini): a fork inherits
    // (or must match) the parent's agent, so a claude parent forked as gemini
    // would be rejected for tool mismatch, not for the agent being unforkable.
    // Making the parent gemini too isolates the unforkable-agent gate as the
    // only possible rejection.
    let parent = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "gemini",
        "-t",
        "GemParent",
    ]);
    assert!(parent.status.success(), "aoe add parent failed");
    let assertion = h.run_cli(&[
        "session",
        "set-session-id",
        "GemParent",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    ]);
    assert!(
        assertion.status.success(),
        "assert conversation: {}",
        String::from_utf8_lossy(&assertion.stderr)
    );

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
}
