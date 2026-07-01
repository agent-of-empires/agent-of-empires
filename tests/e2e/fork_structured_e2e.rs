//! Full-stack e2e: a structured (ACP) fork mints a fresh child session id via
//! the real `session/fork` handshake, leaving the parent untouched.
//!
//! This drives a real `aoe serve --daemon`, the shared Node fake-ACP agent
//! (`web/tests/helpers/fakeAcpAgent.mjs`, which advertises the `fork`
//! capability and mints a distinct id for `session/fork`), and the structured
//! view supervisor. The parent is created with `aoe add --structured-view`; its
//! worker handshakes via `session/new` and captures an `acp_session_id`. The
//! fork is requested through the REST create endpoint with
//! `{ view: "structured", fork_from: <parent acp id> }` (the CLI `--fork-from`
//! is terminal-only), which builds a `ForkSeed::Structured`; the fork's worker
//! then issues `session/fork` against the parent id and captures the minted
//! child id.
//!
//! The proof is read straight off the persisted `sessions.json`:
//!   1. the fork's `acp_session_id` is present, non-empty, DIFFERENT from the
//!      parent's, and carries the fake agent's `session/fork` marker (the fake
//!      mints `fake-acp-fork-<hex>` only from its `session/fork` handler; a
//!      plain `session/new` yields `fake-acp-<ts>-<hex>` with no `fork-`
//!      segment). The marker is what proves `session/fork` fired rather than
//!      `session/new`, so this fails if the create path stops building a fork
//!      seed;
//!   2. the parent's `acp_session_id` is UNCHANGED;
//!   3. the fork's one-shot `fork_pending` marker is cleared after the child id
//!      lands.
//!
//! Compiled only with the default `serve` feature (the structured view and
//! `aoe add --structured-view` do not exist otherwise). Run via:
//!
//! ```sh
//! cargo test --test e2e -- fork_structured
//! ```
#![cfg(feature = "serve")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{
    app_dir_in, pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness,
};

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn read_sessions(h: &TuiTestHarness) -> serde_json::Value {
    let path = sessions_path(h);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

/// The `acp_session_id` of the session titled `title`, if it is present and
/// non-empty on disk right now.
fn acp_id_by_title(h: &TuiTestHarness, title: &str) -> Option<String> {
    read_sessions(h)
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))
        .and_then(|s| s["acp_session_id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse the `  ID:      <id>` line that `aoe add` prints on success.
fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// Poll `sessions.json` until the session titled `title` has captured an
/// `acp_session_id`. The reconciler auto-spawns structured workers on its tick,
/// so a created structured session handshakes (`session/new` for the parent,
/// `session/fork` for the child) without a prompt; this is the readiness oracle
/// for "worker live + handshake done + id persisted".
fn wait_for_acp_id(h: &TuiTestHarness, title: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = acp_id_by_title(h, title) {
            return id;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["acp", "ps", "--json"]);
            panic!(
                "session '{title}' never captured an acp_session_id within {timeout:?}.\n\
                 sessions.json: {}\n acp ps: {}",
                read_sessions(h),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Read the raw session object titled `title`, if present.
fn session_by_title(h: &TuiTestHarness, title: &str) -> Option<serde_json::Value> {
    read_sessions(h)
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))
        .cloned()
}

#[test]
#[serial]
fn structured_fork_mints_distinct_child_id_and_preserves_parent() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("fork_structured");

    // The shared fake-ACP agent (no script needed; the default handlers mint
    // ids for session/new and session/fork). It advertises the fork capability,
    // which is what gates the structured fork handshake.
    let script_path = h.home_path().join("fork-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    // Tear down the worker + daemon on Drop so a panicking assertion can't leak
    // a daemon onto the test port between serial tests.
    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Start the daemon.
    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    // Parent: a structured claude session. The daemon's reconciler auto-spawns
    // its worker, which handshakes via session/new and captures an acp id.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "ForkParent",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let _parent_session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));
    let parent_acp_id = wait_for_acp_id(&h, "ForkParent", Duration::from_secs(45));

    // Fork: request a STRUCTURED fork through the REST create endpoint. The CLI
    // `--fork-from` only builds a terminal seed; the structured path is the
    // create endpoint with `view: "structured"`, which builds a
    // `ForkSeed::Structured` from `fork_from`. `--no-auth` plus the loopback
    // caller means no token is required.
    let project_str = project.to_str().unwrap().to_string();
    let parent_acp_for_post = parent_acp_id.clone();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let post: Result<(), String> = rt.block_on(async move {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;
        let resp = client
            .post(format!("{base}/api/sessions"))
            .json(&serde_json::json!({
                "title": "ForkChild",
                "path": project_str,
                "tool": "claude",
                "view": "structured",
                "fork_from": parent_acp_for_post,
            }))
            .send()
            .await
            .map_err(|e| format!("POST /api/sessions: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decode create response: {e}"))?;
        if status != reqwest::StatusCode::CREATED {
            return Err(format!(
                "structured fork create should be 201, got status={status} body={body}"
            ));
        }
        Ok(())
    });
    post.expect("structured fork create");

    // The fork's worker issues session/fork against the parent id; the fake
    // mints a fresh child id, which lands as the fork's acp_session_id.
    let child_acp_id = wait_for_acp_id(&h, "ForkChild", Duration::from_secs(45));

    // 1. The fork minted a NEW id, not reused the parent's via session/load.
    assert!(
        !child_acp_id.is_empty(),
        "forked child acp_session_id must be non-empty"
    );
    assert_ne!(
        child_acp_id, parent_acp_id,
        "structured fork must mint a fresh acp_session_id, not reuse the parent's"
    );

    // 1b. The child id carries the fake agent's session/fork marker. A plain
    //     session/new (which a non-fork structured create would do) mints a
    //     "fake-acp-<ts>-<hex>" id with no "fork-" segment; only the fake's
    //     session/fork handler produces "fake-acp-fork-<hex>". This is what
    //     actually distinguishes a fork from a new session: if the create path
    //     regressed to building no fork seed, the worker would handshake via
    //     session/new and this assertion would fail.
    assert!(
        child_acp_id.contains("fork-"),
        "child id must come from session/fork, not session/new: {child_acp_id}"
    );

    // 2. The parent's captured id is untouched by the fork.
    let parent_acp_after =
        acp_id_by_title(&h, "ForkParent").expect("parent must still have its acp_session_id");
    assert_eq!(
        parent_acp_after, parent_acp_id,
        "parent's acp_session_id must be unchanged after the fork"
    );

    // 3. The fork's one-shot fork_pending marker is consumed once the child id
    //    lands, so a reattach resumes the child rather than re-forking.
    let sessions = read_sessions(&h);
    let child = sessions
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|s| s["title"].as_str() == Some("ForkChild"))
        })
        .expect("ForkChild present in sessions.json");
    assert!(
        child["fork_pending"].is_null(),
        "fork_pending must be cleared after the forked id is assigned, got: {:?}",
        child["fork_pending"]
    );
}

/// A structured fork whose `session/fork` handshake fails (the agent rejects it,
/// simulated by `FAKE_ACP_FORK_FAIL`) must fail the spawn cleanly and clear the
/// one-shot `fork_pending` marker so the fork is not silently downgraded to a
/// `session/new`. Without the reset on the Err path (PR-review Required #3), the
/// child would wedge re-issuing the same failing fork; here we assert the
/// observable end state: the marker clears, the child never captures a forked
/// id, and the parent is untouched. (This asserts the settled state, not the
/// attempt count; the no-re-fork-loop guard is unit-tested at the
/// supervisor/reducer level.)
#[test]
#[serial]
fn structured_fork_failure_clears_fork_pending_and_fails_cleanly() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("fork_structured_fail");

    // Make the fake reject session/fork. Must be set BEFORE install_acp_shim so
    // the knob is baked into the shim: the daemon strips arbitrary env before
    // spawning the worker, so a daemon-env knob would never reach the fake.
    h.set_acp_fork_fail();
    let script_path = h.home_path().join("fork-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {:?} failed", args);
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(start.status.success(), "aoe serve --daemon failed");
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    // Parent uses session/new (FAKE_ACP_FORK_FAIL only trips session/fork), so
    // it captures an acp id normally.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "FailParent",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(add.status.success(), "aoe add --structured-view failed");
    let parent_acp_id = wait_for_acp_id(&h, "FailParent", Duration::from_secs(45));

    // Request the structured fork; the child's worker will issue session/fork,
    // which the fake rejects.
    let project_str = project.to_str().unwrap().to_string();
    let parent_acp_for_post = parent_acp_id.clone();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let post: Result<(), String> = rt.block_on(async move {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;
        let resp = client
            .post(format!("{base}/api/sessions"))
            .json(&serde_json::json!({
                "title": "FailChild",
                "path": project_str,
                "tool": "claude",
                "view": "structured",
                "fork_from": parent_acp_for_post,
            }))
            .send()
            .await
            .map_err(|e| format!("POST /api/sessions: {e}"))?;
        // The create itself is accepted (201); the fork fails later at the
        // worker handshake, not at create time.
        if resp.status() != reqwest::StatusCode::CREATED {
            return Err(format!("create should be 201, got {}", resp.status()));
        }
        Ok(())
    });
    post.expect("structured fork create");

    // The Err path emits SessionContextReset, which the daemon consumes to clear
    // fork_pending. Poll until the child's marker clears (bounded), proving the
    // reconciler is not stuck re-forking.
    let deadline = Instant::now() + Duration::from_secs(45);
    let child = loop {
        if let Some(child) = session_by_title(&h, "FailChild") {
            if child["fork_pending"].is_null() {
                break child;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "FailChild.fork_pending never cleared after a failed session/fork (retry-loop \
                 regression). sessions.json: {}",
                read_sessions(&h)
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    };

    // The failed fork never captured a child acp id (no session/new fallback
    // that would masquerade as a fork).
    assert!(
        child["acp_session_id"]
            .as_str()
            .map(str::is_empty)
            .unwrap_or(true),
        "a failed fork must not capture an acp_session_id, got: {:?}",
        child["acp_session_id"]
    );

    // The parent is untouched by the failed fork.
    assert_eq!(
        acp_id_by_title(&h, "FailParent").as_deref(),
        Some(parent_acp_id.as_str()),
        "parent's acp_session_id must be unchanged after a failed fork"
    );
}

/// The create handler's fork-mutex 400 paths, exercised end-to-end by POSTing
/// bad bodies to a live `aoe serve` rather than only asserting the predicates.
/// A refactor that dropped an early return would pass the predicate tests but
/// fail here. Covers: both import + fork set, a malformed fork id, and a
/// structured fork requested for a resume-only agent (aoe-agent).
#[test]
#[serial]
fn create_handler_rejects_bad_fork_requests_with_400() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("fork_create_400");
    let script_path = h.home_path().join("fork-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let project = h.project_path();
    let out = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .output()
        .expect("git init");
    assert!(out.status.success(), "git init failed");

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(start.status.success(), "aoe serve --daemon failed");
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let project_str = project.to_str().unwrap().to_string();
    // (body, expected error code) for each mutex the handler must enforce.
    let cases = vec![
        (
            serde_json::json!({
                "title": "BadBoth", "path": project_str, "tool": "claude",
                "import_acp_session_id": "some-import-id", "fork_from": "parent-uuid_123",
            }),
            "both import and fork set",
        ),
        (
            serde_json::json!({
                "title": "BadForkId", "path": project_str, "tool": "claude",
                "fork_from": "../etc/passwd",
            }),
            "malformed fork id",
        ),
        (
            serde_json::json!({
                "title": "BadStructuredFork", "path": project_str, "tool": "aoe-agent",
                "view": "structured", "fork_from": "parent-uuid_123",
            }),
            "structured fork for a resume-only agent",
        ),
    ];

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().build().expect("build client");
        for (body, label) in cases {
            let resp = client
                .post(format!("{base}/api/sessions"))
                .json(&body)
                .send()
                .await
                .unwrap_or_else(|e| panic!("POST for '{label}': {e}"));
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "'{label}' should be rejected with 400"
            );
            let json: serde_json::Value = resp.json().await.expect("decode error body");
            assert!(
                json["error"].as_str().is_some(),
                "'{label}' 400 body should carry an error code, got: {json}"
            );
        }
    });
}
