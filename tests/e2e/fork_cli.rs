//! `aoe add --fork-from` (terminal fork) driven as a subprocess, asserting on
//! the persisted `sessions.json`. No agent runs, so the fork gate is exercised
//! without tmux: a real conversation would capture the parent's
//! `agent_session_id`, so the tests seed it by hand.

use std::path::Path;

use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{app_dir_in, session_by_title, TuiTestHarness};

const PARENT_AGENT_ID: &str = "11111111-2222-3333-4444-555555555555";

/// Patch the persisted session titled `title`.
fn patch_session(h: &TuiTestHarness, title: &str, patch: impl Fn(&mut Value)) {
    let mut sessions = h.read_sessions();
    let session = sessions
        .as_array_mut()
        .expect("sessions array")
        .iter_mut()
        .find(|s| s["title"].as_str() == Some(title))
        .unwrap_or_else(|| panic!("session '{title}' present"));
    patch(session);
    std::fs::write(
        h.sessions_path(),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write seeded sessions.json");
}

/// Add a parent session running `tool` and give it a captured agent id, which
/// is what the fork gate keys off.
fn seed_parent(h: &TuiTestHarness, project: &Path, title: &str, tool: &str) {
    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        tool,
        "-t",
        title,
    ]);
    patch_session(h, title, |session| {
        session["agent_session_id"] = json!(PARENT_AGENT_ID);
    });
}

fn assert_not_persisted(h: &TuiTestHarness, title: &str) {
    assert!(
        h.read_sessions()
            .as_array()
            .is_some_and(|rows| rows.iter().all(|s| s["title"].as_str() != Some(title))),
        "a refused fork must not persist '{title}'"
    );
}

/// Scratch sessions provision `<app_dir>/scratch/<id>/`; a refused fork must
/// leave that root empty.
fn assert_no_scratch_dirs(h: &TuiTestHarness) {
    let root = app_dir_in(h.home_path()).join("scratch");
    assert!(
        !root.exists()
            || std::fs::read_dir(&root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "refused fork left a scratch dir under {}",
        root.display()
    );
}

/// A fork pre-pins a fresh child agent id plus a one-shot `Fork` resume intent
/// pointing at the parent's captured id, and leaves the parent untouched.
#[test]
#[parallel]
fn fork_from_seeds_child_with_fork_intent() {
    let h = TuiTestHarness::new("fork_cli_happy");
    let project = h.project_path();
    seed_parent(&h, &project, "ForkParent", "claude");

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "ForkChild",
        "--fork-from",
        "ForkParent",
    ]);

    let sessions = h.read_sessions();
    let child = session_by_title(&sessions, "ForkChild");
    let child_agent_id = child["agent_session_id"]
        .as_str()
        .expect("forked child must pre-pin a fresh agent_session_id");
    assert!(!child_agent_id.is_empty());
    assert_ne!(
        child_agent_id, PARENT_AGENT_ID,
        "child must fork into a NEW id, not reuse the parent's"
    );
    assert_eq!(child["resume_intent"]["kind"].as_str(), Some("Fork"));
    assert_eq!(
        child["resume_intent"]["value"]["from"].as_str(),
        Some(PARENT_AGENT_ID),
        "the Fork intent must resume the parent's captured id"
    );

    let parent = session_by_title(&sessions, "ForkParent");
    assert_eq!(parent["agent_session_id"].as_str(), Some(PARENT_AGENT_ID));
    assert!(
        parent["resume_intent"].is_null()
            || parent["resume_intent"]["kind"].as_str() == Some("Default"),
        "parent must not gain a Fork intent, got: {:?}",
        parent["resume_intent"]
    );
}

/// With no `--tool`/`--cmd` the fork inherits the parent's agent.
#[test]
#[parallel]
fn fork_from_inherits_the_parents_agent() {
    let h = TuiTestHarness::new("fork_cli_inherit");
    let project = h.project_path();
    seed_parent(&h, &project, "MatchParent", "claude");

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "InheritChild",
        "--fork-from",
        "MatchParent",
    ]);
    let sessions = h.read_sessions();
    assert_eq!(
        session_by_title(&sessions, "InheritChild")["tool"].as_str(),
        Some("claude")
    );
}

/// Every way a fork can be refused: a different agent (a captured id is
/// agent-specific), an agent with no fork capability, flags that change the
/// working directory or carry their own resume/fork flags, a parent with no
/// captured conversation, and a parent whose own fork has not launched. Each
/// refusal fires before provisioning, so nothing is persisted or left on disk.
#[test]
#[parallel]
fn fork_from_refusals_persist_nothing() {
    /// How the fork source is prepared before the refused `aoe add`.
    enum Parent {
        Seeded(&'static str),
        /// No captured conversation to fork from.
        Bare,
        /// Forked but never launched: a synthetic id plus a live Fork intent.
        UnlaunchedFork,
    }
    struct Case {
        parent: Parent,
        args: &'static [&'static str],
        expect: &'static str,
    }
    let cases = [
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--tool", "gemini"],
            expect: "must use the parent's agent",
        },
        Case {
            // gemini is resume-only, so the parent uses it too and the
            // unforkable-agent gate is the only possible rejection.
            parent: Parent::Seeded("gemini"),
            args: &["--tool", "gemini"],
            expect: "does not support forking",
        },
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--worktree", "wt-branch"],
            expect: "",
        },
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--scratch"],
            expect: "--scratch",
        },
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--sandbox"],
            expect: "",
        },
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--cmd", "claude --resume abc"],
            expect: "",
        },
        Case {
            // Codex's bare `fork` subcommand, matched as a word.
            parent: Parent::Seeded("claude"),
            args: &["--cmd", "codex fork abc"],
            expect: "",
        },
        Case {
            parent: Parent::Seeded("claude"),
            args: &["--cmd-override", "some-other-binary"],
            expect: "",
        },
        Case {
            // A terminal fork cannot carry its state onto a structured session.
            // `--scratch` makes a late rejection observable as a leaked dir.
            parent: Parent::Seeded("claude"),
            args: &["--scratch", "--structured-view"],
            expect: "cannot be combined with",
        },
        Case {
            parent: Parent::Bare,
            args: &[],
            expect: "Nothing to fork",
        },
        Case {
            parent: Parent::UnlaunchedFork,
            args: &[],
            expect: "its own fork has not launched yet",
        },
    ];

    for case in cases {
        let mut h = TuiTestHarness::new("fork_cli_refusal");
        h.install_path_command("gemini");
        let project = h.project_path();
        match case.parent {
            Parent::Seeded(tool) => seed_parent(&h, &project, "Parent", tool),
            Parent::Bare => {
                h.run_cli_ok(&[
                    "add",
                    project.to_str().unwrap(),
                    "--cmd",
                    "claude",
                    "-t",
                    "Parent",
                ]);
            }
            Parent::UnlaunchedFork => {
                h.run_cli_ok(&[
                    "add",
                    project.to_str().unwrap(),
                    "--cmd",
                    "claude",
                    "-t",
                    "Parent",
                ]);
                patch_session(&h, "Parent", |session| {
                    session["agent_session_id"] = json!("99999999-8888-7777-6666-555555555555");
                    session["resume_intent"] =
                        json!({ "kind": "Fork", "value": { "from": PARENT_AGENT_ID } });
                });
            }
        }
        assert_no_scratch_dirs(&h);

        let mut args = vec!["add"];
        if !case.args.contains(&"--scratch") {
            args.push(project.to_str().unwrap());
        }
        args.extend_from_slice(case.args);
        args.extend_from_slice(&["-t", "Child", "--fork-from", "Parent"]);

        let stderr = h.run_cli_err(&args);
        assert!(
            stderr.contains(case.expect),
            "{args:?}: expected {:?} in:\n{stderr}",
            case.expect
        );
        assert_not_persisted(&h, "Child");
        assert_no_scratch_dirs(&h);
    }
}
