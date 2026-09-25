//! Native session ID acquisition and launch lifecycle regressions.

use agent_of_empires::session::{GroupTree, Instance, LaunchSidOutcome, Status, Storage};
use agent_of_empires::tmux;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use crate::common::{setup_temp_home, tmux_socket};

const VALID_CLAUDE_UUID: &str = "019342ab-1234-7def-8901-abcdef012345";

struct TmuxCleanup<'a>(&'a str);

impl Drop for TmuxCleanup<'_> {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(tmux_socket())
            .args(["kill-session", "-t", self.0])
            .output();
    }
}

/// Set an env var for the test body and restore its prior value on drop.
struct EnvRestore(&'static str, Option<std::ffi::OsString>);

impl EnvRestore {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self(key, prev)
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match self.1.take() {
            Some(prev) => std::env::set_var(self.0, prev),
            None => std::env::remove_var(self.0),
        }
    }
}

fn acknowledge_agent_hooks() {
    agent_of_empires::session::update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })
    .expect("acknowledge agent hooks in isolated test home");
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[serial]
fn start_with_size_opts_returns_skipped_when_pane_preexists() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let _temp = setup_temp_home();
    let mut inst = Instance::new("F1Regression", "/tmp/aoe-f1-regression");
    inst.tool = "claude".to_string();
    // Make is_existing=true the path acquire would take if reached, so any
    // fall-through regression builds a real launch command and fails loudly.
    inst.agent_session_id = Some(VALID_CLAUDE_UUID.to_string());
    let session_name = tmux::Session::generate_name(&inst.id, &inst.title);

    let status = Command::new("tmux")
        .arg("-S")
        .arg(tmux_socket())
        .args(["new-session", "-d", "-s", &session_name])
        .status()
        .expect("tmux new-session");
    assert!(
        status.success(),
        "tmux new-session failed for {session_name}"
    );
    let _cleanup = TmuxCleanup(&session_name);

    // `Session::exists()` consults a 2s-TTL cache; refresh after the raw
    // `new-session` so a prior `#[serial]` test's stale snapshot can't
    // make `exists()` miss our session.
    tmux::refresh_session_cache();

    let outcome = inst
        .start_with_size_opts(None, false)
        .expect("start_with_size_opts must succeed on preexisting pane");

    assert_eq!(
        outcome,
        LaunchSidOutcome::Skipped,
        "preexisting pane must short-circuit before apply_session_flags"
    );
}

/// #3399: an agent that dies on launch leaves a `remain-on-exit` corpse pane
/// that still owns the tmux name. Reading that as a running session made every
/// later start return `Skipped`, so `aoe session start` printed "Started" and
/// launched nothing. A dead pane must be torn down and relaunched.
#[test]
#[serial]
fn start_with_size_opts_relaunches_a_dead_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let temp = setup_temp_home();
    acknowledge_agent_hooks();
    let workdir = temp.path().join("workdir");
    std::fs::create_dir_all(&workdir).expect("create workdir");

    // Records one line per launch, then exits so the pane becomes a corpse.
    let launches = temp.path().join("launches");
    let agent = temp.path().join("dying-agent");
    std::fs::write(
        &agent,
        format!(
            "#!/bin/sh\necho launched >> '{}'\nexit 3\n",
            launches.display()
        ),
    )
    .expect("write dying agent");
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
        .expect("chmod dying agent");

    let mut inst = Instance::new("F1Corpse", workdir.to_str().unwrap());
    inst.tool = "claude".to_string();
    inst.command = agent.to_string_lossy().to_string();
    let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
    let _cleanup = TmuxCleanup(&session_name);

    let storage = Storage::new_unwatched("default").expect("open storage");
    storage
        .update(|instances, groups| {
            *instances = vec![inst.clone()];
            *groups = GroupTree::new_with_groups(std::slice::from_ref(&inst), &[]).get_all_groups();
            Ok(())
        })
        .expect("seed storage");

    let _ = inst
        .start_with_size_opts(None, false)
        .expect("first start must launch");
    let session = tmux::Session::from_name(&session_name);
    assert!(
        wait_until(|| session.is_pane_dead()),
        "the dying agent should have left a corpse pane"
    );

    let outcome = inst
        .start_with_size_opts(None, false)
        .expect("second start must launch over the corpse");
    assert_ne!(
        outcome,
        LaunchSidOutcome::Skipped,
        "a dead pane must not short-circuit start as if the agent were running"
    );
    assert!(
        wait_until(|| launch_count(&launches) >= 2),
        "second start must actually relaunch the agent; launches={}",
        launch_count(&launches)
    );
}

/// #3399: a fresh launch that pins an id the session already had stored
/// (`--session-id <sid>`, the empty-thread downgrade) dies at once when the
/// agent considers that id taken. Surface the pane's own error on the session
/// instead of leaving a corpse behind a successful-looking return.
#[test]
#[serial]
fn restart_surfaces_a_pinned_fresh_launch_that_dies() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let temp = setup_temp_home();
    acknowledge_agent_hooks();
    // The transcript probe reads this dir; an empty one is what makes the
    // stored sid launch fresh-pinned rather than with `--resume`. Restored on
    // drop: unlike `HOME`, this var is not part of `setup_temp_home`'s set, so
    // leaking it would point every later test at a deleted tempdir.
    let _claude_home = EnvRestore::set("CLAUDE_CONFIG_DIR", temp.path().join(".claude"));
    let workdir = temp.path().join("workdir");
    std::fs::create_dir_all(&workdir).expect("create workdir");

    // Print the line in red: `capture_pane` captures with `-e`, so an agent
    // whose error is styled (every real one is) would otherwise splice raw SGR
    // sequences into the persisted `last_error`.
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create test bin directory");
    let agent = bin.join("claude");
    std::fs::write(
        &agent,
        "#!/bin/sh\nprintf '\\033[31mError: Session ID is already in use.\\033[0m\\n'\nexit 1\n",
    )
    .expect("write agent");
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).expect("chmod agent");
    std::fs::write(
        temp.path().join(".profile"),
        format!("export PATH={}:$PATH\n", bin.display()),
    )
    .expect("write test login profile");
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(
            std::env::var_os("PATH")
                .iter()
                .flat_map(|value| std::env::split_paths(value)),
        ),
    )
    .expect("join test PATH");
    let _path = EnvRestore::set("PATH", path);
    let _shell = EnvRestore::set("SHELL", "/bin/sh");

    let mut inst = Instance::new("F1Pinned", workdir.to_str().unwrap());
    inst.tool = "claude".to_string();
    inst.command = "claude".to_string();
    inst.agent_session_id = Some(VALID_CLAUDE_UUID.to_string());
    inst.agent_session_binding = Some(agent_of_empires::session::ConversationBinding {
        session_id: VALID_CLAUDE_UUID.into(),
        provenance: agent_of_empires::session::ConversationProvenance::Preallocated,
        execution: Some(agent_of_empires::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![temp.path().join(".claude")],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: workdir.clone(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        }),
        transcript_path: None,
    });
    let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
    let _cleanup = TmuxCleanup(&session_name);

    let storage = Storage::new_unwatched("default").expect("open storage");
    storage
        .update(|instances, groups| {
            *instances = vec![inst.clone()];
            *groups = GroupTree::new_with_groups(std::slice::from_ref(&inst), &[]).get_all_groups();
            Ok(())
        })
        .expect("seed storage");

    let error = inst
        .restart_with_size_opts(None, false)
        .expect_err("a launch that died in the probe window must not report success");
    let message = format!("{error:#}");
    assert!(
        message.contains("already in use"),
        "the pane's own diagnosis must reach the caller, got {message:?}"
    );
    assert!(
        !message.contains('\u{1b}'),
        "the persisted error must be plain text, got {message:?}"
    );
    assert_eq!(inst.status, Status::Error);
    assert!(
        wait_until(|| !tmux::Session::from_name(&session_name).exists()),
        "the corpse pane must be torn down so a later start is not a no-op"
    );
}

#[test]
#[serial]
fn fork_child_publication_confirms_preallocated_identity() {
    if !tmux_available() {
        return;
    }
    let temp = setup_temp_home();
    acknowledge_agent_hooks();
    let project = temp.path().join("project");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let aoe = env!("CARGO_BIN_EXE_aoe");
    let claude = bin.join("claude");
    let publish = temp.path().join("publish");
    let published = temp.path().join("published");
    std::fs::write(
        &claude,
        format!(
            r#"#!/bin/sh
sid=
while [ "$#" -gt 0 ]; do if [ "$1" = --session-id ]; then shift; sid=$1; fi; shift; done
[ -n "$sid" ] || exit 1
while [ ! -e {} ]; do sleep 0.05; done
printf '{{"session_id":"%s"}}\n' "$sid" | {} __extract-session-id || exit 1
: > {}
sleep 60
"#,
            shell_words::quote(publish.to_str().unwrap()),
            shell_words::quote(aoe),
            shell_words::quote(published.to_str().unwrap()),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let cli = |args: &[&str]| {
        let output = Command::new(aoe)
            .args(["--profile", "default"])
            .args(args)
            .current_dir(&project)
            .env("PATH", &path)
            .env("SHELL", "/bin/bash")
            .env("TMPDIR", temp.path())
            .env("AOE_TMUX_SOCKET", tmux_socket())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    cli(&["profile", "create", "default"]);
    cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "claude",
        "-t",
        "parent",
    ]);
    cli(&["session", "set-session-id", "parent", VALID_CLAUDE_UUID]);
    cli(&["add", "--fork-from", "parent", "-t", "child"]);
    let storage = Storage::open_unwatched("default").unwrap();
    let child = storage
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.title == "child")
        .unwrap();
    let session_name = tmux::Session::generate_name(&child.id, &child.title);
    let _cleanup = TmuxCleanup(&session_name);
    let child_sid = child.agent_session_id.clone();
    cli(&["session", "start", "child"]);
    std::fs::write(&publish, "").unwrap();
    assert!(
        wait_until(|| published.is_file()),
        "native publication did not complete"
    );
    cli(&["session", "stop", "child"]);
    assert!(
        storage.load().unwrap().iter().any(|row| {
            row.id == child.id
                && row.agent_session_id == child_sid
                && row.agent_session_binding.as_ref().is_some_and(|binding| {
                    binding.provenance
                        == agent_of_empires::session::ConversationProvenance::Observed
                })
        }),
        "qualified same-SID publication must confirm the fork child"
    );
    cli(&["add", "--fork-from", "child", "-t", "grandchild"]);
}

fn launch_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

/// Poll `check` for up to 5s. tmux state (pane death, the relaunch) settles
/// asynchronously after the call that triggers it returns.
fn wait_until(check: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    check()
}
