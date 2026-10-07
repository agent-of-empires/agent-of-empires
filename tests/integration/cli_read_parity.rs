//! Compare exit code and both streams from the same store over UDS and local reads.
//! These fixtures require the Linux UDS publisher. Other platforms fall back
//! to the local store unless an HTTP endpoint is selected.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, RuntimeUdsTestServer,
};
use agent_of_empires::session::{Instance, Status, View};

const PROFILE: &str = "main";

/// Sort after `main` so a broken profile exposes any partial successful stdout.
const BROKEN: &str = "wrecked";

/// Successful human and JSON reads across the complete classified command set.
const COMMANDS: [&[&str]; 20] = [
    &["list"],
    &["list", "--state", "all"],
    &["list", "--all"],
    &["list", "--json"],
    &["list", "--json", "--all"],
    &["status"],
    &["status", "--json"],
    &["status", "--verbose"],
    &["group", "list"],
    &["group", "list", "--json"],
    &["project", "list"],
    &["project", "list", "--json"],
    &["profile"],
    &["session", "show", "--json", "long-session-id-01"],
    &["session", "show", "j-orphan"],
    &["session", "list-trash"],
    // Compare terminal self-healing with the served session projection.
    &["session", "show", "--json", "l-terminal"],
    &["session", "show", "l-terminal"],
    &["session", "show", "a-archived"],
    // Session lookup retains the stored trailing separator.
    &["session", "show", "--json", "/srv/registered/"],
];

/// State-dependent refusals compared by exit code and both output streams.
const REFUSALS: [&[&str]; 4] = [
    &["session", "show", "no-such-session"],
    &["list", "-p", "ghost-profile"],
    &["session", "show", "k-amb"],
    // Whitespace names a profile; only an empty flag is unset.
    &["list", "-p", "   ", "--json"],
];

/// Registry and missing-selector answers are independent of session rows.
/// Emptying sessions witnesses other answers, including ambiguous selectors.
const NOT_WITNESSED_BY_EMPTYING: &[&[&str]] = &[
    &["profile"],
    &["project", "list"],
    &["project", "list", "--json"],
    &["session", "show", "no-such-session"],
    &["list", "-p", "ghost-profile"],
    &["list", "-p", "   ", "--json"],
];

/// Exit code and both byte streams from one invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    exit: i32,
    stdout: String,
    stderr: String,
}

/// The temporary home plus the environment binding that points both the
/// daemon's own reads and the subprocesses at it, restored on drop.
struct Fixture {
    home: PathBuf,
    _owned: tempfile::TempDir,
    previous: Vec<(&'static str, Option<OsString>)>,
    _env: agent_of_empires::server::test_support::RuntimeEnvGuard,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

impl Fixture {
    /// Normal tests own a fresh home; the opt-in smoke takes a named parent.
    fn new() -> Self {
        Self::new_with_registries(
            serde_json::json!([
                {"name": "registered", "path": "/srv/registered", "scope": "global"},
            ]),
            serde_json::json!([]),
        )
    }

    /// Seed both registries to exercise deduplication and profile shadowing.
    fn new_with_registries(global: serde_json::Value, profile: serde_json::Value) -> Self {
        let env = agent_of_empires::server::test_support::RuntimeEnvGuard::read_lock();
        let home = tempfile::tempdir().expect("temp home");
        Self::seed_registries(home, global, profile, env)
    }

    fn seed(owned: tempfile::TempDir) -> Self {
        let env = agent_of_empires::server::test_support::RuntimeEnvGuard::read_lock();
        Self::seed_registries(
            owned,
            serde_json::json!([]),
            serde_json::json!([
                {"name": "registered", "path": "/srv/registered", "scope": "global"},
            ]),
            env,
        )
    }

    fn seed_registries(
        owned: tempfile::TempDir,
        global: serde_json::Value,
        profile: serde_json::Value,
        mut env: agent_of_empires::server::test_support::RuntimeEnvGuard,
    ) -> Self {
        let home = owned.path().to_path_buf();
        env.bind(&home.join(".config"));
        let app_dir = home
            .join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG);
        std::fs::create_dir_all(&app_dir).expect("app dir");
        let profile_dir = app_dir.join("profiles").join(PROFILE);
        std::fs::create_dir_all(&profile_dir).expect("profile dir");
        let sessions = fixture_sessions(&home);
        std::fs::write(
            profile_dir.join("sessions.json"),
            serde_json::to_vec_pretty(&sessions).expect("sessions"),
        )
        .expect("seed sessions");
        // Registry-only rows must not depend on session-derived inventory.
        std::fs::write(
            profile_dir.join("groups.json"),
            serde_json::to_vec_pretty(&serde_json::json!([
                {"name": "empty", "path": "empty", "collapsed": false},
                {"name": "work", "path": "work", "collapsed": false},
            ]))
            .expect("groups"),
        )
        .expect("seed groups");
        if !global.as_array().is_some_and(|rows| rows.is_empty()) {
            std::fs::write(
                app_dir.join("projects.json"),
                serde_json::to_vec_pretty(&global).expect("global projects"),
            )
            .expect("seed global projects");
        }
        std::fs::write(
            profile_dir.join("projects.json"),
            serde_json::to_vec_pretty(&profile).expect("projects"),
        )
        .expect("seed projects");
        // The literal `default` profile exercises the picker-specific ordering.
        for name in ["default", "zeta"] {
            std::fs::create_dir_all(app_dir.join("profiles").join(name)).expect("extra profile");
        }
        // Disable network-dependent update notices and keep the default profile explicit.
        std::fs::write(
            app_dir.join("config.toml"),
            format!("default_profile = \"{PROFILE}\"\n\n[updates]\nupdate_check_mode = \"off\"\n"),
        )
        .expect("config");
        let base = home.clone();
        let mut fixture = Self {
            home: base.clone(),
            _env: env,
            _owned: owned,
            previous: Vec::new(),
        };
        for (key, value) in [
            ("HOME", base.clone()),
            ("XDG_DATA_HOME", base.join(".local/share")),
        ] {
            fixture.previous.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        std::fs::create_dir_all(base.join(".config")).expect("xdg base");
        fixture
    }

    /// Malformed groups fail the store load without first failing sessions migrations.
    fn write_broken_profile(&self) {
        let dir = self
            .path()
            .join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
            .join("profiles")
            .join(BROKEN);
        std::fs::create_dir_all(&dir).expect("the broken profile dir");
        std::fs::write(dir.join("sessions.json"), b"[]").expect("seed the broken store");
        std::fs::write(dir.join("groups.json"), br#"{"groups": []}"#)
            .expect("seed the unreadable registry");
    }

    /// A failed project registry must not refuse unrelated reads.
    fn write_broken_global_projects(&self) {
        std::fs::write(self.global_projects_path(), b"not json").expect("break the registry");
    }

    fn global_projects_path(&self) -> PathBuf {
        self.path()
            .join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
            .join("projects.json")
    }

    fn app_dir(&self) -> PathBuf {
        self.path()
            .join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
    }

    fn sessions_path(&self) -> PathBuf {
        self.app_dir()
            .join("profiles")
            .join(PROFILE)
            .join("sessions.json")
    }

    /// Removing local sessions leaves the accepted daemon cache unchanged.
    fn take_sessions(&self) -> Option<Vec<u8>> {
        let path = self.sessions_path();
        let saved = std::fs::read(&path).ok();
        std::fs::write(&path, "[]").expect("empty the session registry");
        saved
    }

    fn restore_sessions(&self, saved: Option<Vec<u8>>) {
        let path = self.sessions_path();
        match saved {
            Some(bytes) => std::fs::write(path, bytes).expect("restore the session registry"),
            None => {
                std::fs::remove_file(path).expect("remove the registry this fixture did not have")
            }
        }
    }

    fn path(&self) -> &Path {
        &self.home
    }
}

/// Representative lifecycle rows and registry-only groups/projects.
/// Structured rows avoid dependence on an external tmux server.
fn fixture_sessions(home: &Path) -> Vec<serde_json::Value> {
    let under_home = home.join("code/under-home");
    let unregistered = home.join("code/not-registered");
    let (waiting, running, idle, stopped, error) = (
        Status::Waiting,
        Status::Running,
        Status::Idle,
        Status::Stopped,
        Status::Error,
    );
    let rows = vec![
        row(
            "a-archived",
            idle,
            "/srv/registered",
            "",
            "Archived",
            true,
            false,
        ),
        row(
            "b-running",
            running,
            &under_home.to_string_lossy(),
            "work",
            "Running",
            false,
            false,
        ),
        row(
            "c-trashed",
            idle,
            "/srv/registered",
            "",
            "Trashed",
            false,
            true,
        ),
        row(
            "d-waiting",
            waiting,
            &unregistered.to_string_lossy(),
            "",
            "Waiting",
            false,
            false,
        ),
        row(
            "e-idle",
            idle,
            "/srv/registered",
            "work",
            "Idle",
            false,
            false,
        ),
        row(
            "f-stopped",
            stopped,
            "/srv/registered",
            "",
            "Stopped",
            false,
            false,
        ),
        row(
            "g-error",
            error,
            "/srv/registered",
            "",
            "Error",
            false,
            false,
        ),
        row(
            "h-parent",
            idle,
            "/srv/registered",
            "",
            "Parent",
            false,
            false,
        ),
        row(
            "i-child",
            idle,
            "/srv/registered",
            "",
            "Child",
            false,
            false,
        ),
        // Wider than the id column, so the row has to be cut to 12 characters.
        row(
            "long-session-id-01",
            idle,
            &under_home.to_string_lossy(),
            "work",
            "Long id",
            false,
            false,
        ),
        // Preserve a deleted parent's stored id in both transports.
        row(
            "j-orphan",
            idle,
            "/srv/registered",
            "",
            "Orphan",
            false,
            false,
        ),
        // Matching IDs exercise identical ambiguous-prefix candidate diagnostics on both transports.
        row(
            "k-amb-01",
            idle,
            "/srv/registered",
            "",
            "Ambiguous one",
            false,
            false,
        ),
        row(
            "k-amb-02",
            idle,
            "/srv/registered",
            "",
            "Ambiguous two",
            false,
            false,
        ),
        // A stopped terminal without an agent ID keeps local backfill probe-inert.
        {
            let mut value = row(
                "l-terminal",
                stopped,
                "/srv/terminal",
                "terminal",
                "Terminal",
                false,
                false,
            );
            value["view"] = serde_json::json!(View::Terminal);
            value["agent_session_id"] = serde_json::Value::Null;
            value
        },
        // Session lookup preserves the stored trailing separator.
        row(
            "m-trailing",
            stopped,
            "/srv/registered/",
            "",
            "Trailing separator",
            false,
            false,
        ),
    ];
    let mut rows: Vec<serde_json::Value> = rows;
    // A parent/child relation, spelled the way the store spells it.
    for row in &mut rows {
        if row["id"] == "i-child" {
            row["parent_session_id"] = serde_json::json!("h-parent");
        }
        if row["id"] == "j-orphan" {
            row["parent_session_id"] = serde_json::json!("gone");
        }
        // Preserve an intentionally noncanonical workspace repository order.
        if row["id"] == "b-running" {
            row["workspace_info"] = serde_json::json!({
                "branch": "main",
                "workspace_dir": "/srv/registered/.aoe/workspace",
                "created_at": "2026-01-02T03:04:05.123456789Z",
                "cleanup_on_delete": true,
                "repos": [stored_repo("zeta", "/srv/zeta"), stored_repo("alpha", "/srv/alpha")],
            });
        }
    }
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    rows
}

/// Stored workspace repository metadata is projected without rewriting.
fn stored_repo(name: &str, source_path: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "source_path": source_path,
        "branch": "main",
        "worktree_path": format!("{source_path}/.worktrees/main"),
        "main_repo_path": source_path,
        "managed_by_aoe": true,
        "branch_preexisting": false,
        "base_branch": null,
        "base_branch_override": null,
    })
}

fn row(
    id: &str,
    status: Status,
    project_path: &str,
    group_path: &str,
    title: &str,
    archived: bool,
    trashed: bool,
) -> serde_json::Value {
    let mut instance = Instance::new(id, project_path);
    // Use stable lookup IDs instead of the UUID minted by Instance::new.
    instance.id = id.to_string();
    instance.title = title.to_string();
    instance.tool = "claude".into();
    instance.command = "claude --resume".into();
    instance.view = View::Structured;
    instance.status = status;
    instance.group_path = group_path.to_string();
    instance.agent_session_id = Some(format!("{id}-agent"));
    instance.created_at = "2026-01-02T03:04:05.123456789Z"
        .parse()
        .expect("created_at");
    // Short fractions exercise AutoSi timestamp normalization on both transports.
    if archived {
        instance.archived_at = Some("2026-02-03T04:05:06.25Z".parse().expect("archived_at"));
    }
    if trashed {
        instance.trashed_at = Some("2026-03-04T05:06:07.5Z".parse().expect("trashed_at"));
    }
    serde_json::to_value(&instance).expect("row serializes")
}

/// Probe cached fixture rows as the daemon status watcher does.
fn daemon_instances() -> Vec<Instance> {
    let storage =
        agent_of_empires::session::Storage::open_unwatched(PROFILE).expect("profile storage");
    let (mut instances, _) = storage.load_with_groups().expect("load");
    for instance in &mut instances {
        instance.source_profile = PROFILE.to_string();
        instance.update_status_once(None, None);
    }
    instances
}

/// A blocking child must leave the runtime available to serve its connection.
async fn run(home: PathBuf, args: Vec<String>) -> Run {
    tokio::task::spawn_blocking(move || run_blocking(&home, &args))
        .await
        .expect("the command task joins")
}

/// Carry the exit code and both streams so refusal outcomes can be compared.
fn run_blocking(home: &Path, args: &[String]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aoe"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("AOE_DAEMON_URL")
        .env_remove("AOE_DAEMON_TOKEN")
        .env_remove("AGENT_OF_EMPIRES_PROFILE")
        .args(args);
    let output = command.output().expect("the aoe binary runs");
    Run {
        exit: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Compare exit code and both output streams, including refusal outcomes.
fn difference(args: &[String], served: &Run, local: &Run) -> Option<String> {
    let label = args.join(" ");
    if served.exit != local.exit {
        return Some(format!(
            "`aoe {label}` exits differently between the two transports:\n  served: {}\n  local:  {}\n  served stderr: {}\n  local stderr:  {}",
            served.exit, local.exit, served.stderr, local.stderr,
        ));
    }
    if served.stdout != local.stdout {
        return Some(format!(
            "`aoe {label}` differs between the two transports:\n  served: {}\n  local:  {}",
            served.stdout, local.stdout,
        ));
    }
    if served.stderr != local.stderr {
        return Some(format!(
            "`aoe {label}` reports differently between the two transports:\n  served: {}\n  local:  {}",
            served.stderr, local.stderr,
        ));
    }
    None
}

/// Require identical exit code and both byte streams.
fn assert_same(args: &[String], served: &Run, local: &Run) {
    if let Some(difference) = difference(args, served, local) {
        panic!("{difference}");
    }
}

/// Retract publication before comparing the same store through local reads.
#[tokio::test]
#[serial_test::serial]
async fn the_served_bytes_are_the_bytes_the_local_command_prints() {
    let fixture = Fixture::new();
    compare_both_transports(&fixture).await;
}

/// Canonical equality merges symlink spellings and applies profile shadowing.
#[tokio::test]
#[serial_test::serial]
async fn one_directory_under_two_spellings_is_one_project_on_both_transports() {
    let base = tempfile::tempdir().expect("temp home");
    let repo = base.path().join("code/dup");
    std::fs::create_dir_all(&repo).expect("the registered directory");
    let link = base.path().join("code/link");
    std::os::unix::fs::symlink(&repo, &link).expect("the second spelling");
    let fixture = Fixture::new_with_registries(
        serde_json::json!([
            {"name": "dup", "path": repo.to_string_lossy()},
            {"name": "dup-symlink", "path": link.to_string_lossy()},
        ]),
        serde_json::json!([
            {"name": "dup-trailing", "path": repo.to_string_lossy()},
        ]),
    );
    compare_transports(
        &fixture,
        &[
            &["project", "list", "--json"],
            &["project", "list"],
            &["project", "list", "--scope", "profile", "--json"],
        ],
        usize::MAX,
        // Registered projects do not depend on the session rows the probe empties.
        &[
            &["project", "list", "--json"],
            &["project", "list"],
            &["project", "list", "--scope", "profile", "--json"],
        ],
    )
    .await;
}

/// An unreadable profile prevents a successful partial listing on either transport.
/// The refusal identifies the unreadable profile.
#[tokio::test]
#[serial_test::serial]
async fn a_profile_that_cannot_be_read_is_refused_by_both_transports() {
    let fixture = Fixture::new();
    fixture.write_broken_profile();
    // Keep the successful picker before the refusal boundary.
    let served = compare_transports(
        &fixture,
        &[
            // Inventory does not open the broken store.
            &["profile"],
            &["list", "--all"],
            &["list", "--json", "--all"],
        ],
        1,
        // These answers do not depend on the session rows emptied by the probe.
        &[
            &["profile"],
            &["list", "--all"],
            &["list", "--json", "--all"],
        ],
    )
    .await;

    assert_eq!(
        served[0].exit, 0,
        "the picker reads no profile's data, so a broken one does not refuse it: {:?}",
        served[0].stderr
    );
    for refusal in served.iter().skip(1) {
        assert_eq!(
            refusal.exit, 1,
            "an unreadable profile is the operator's own state, so it leaves 1: {:?}",
            refusal.stderr
        );
        assert!(
            refusal.stderr.contains(BROKEN),
            "the refusal names the profile it could not read: {:?}",
            refusal.stderr
        );
        assert!(
            refusal.stdout.is_empty(),
            "a refusal prints no rows on either transport: {:?}",
            refusal.stdout
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn legacy_native_profile_names_keep_raw_picker_bytes_and_all_profile_refusals() {
    for name in ["ALL", "legacy\\name"] {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.app_dir().join("profiles").join(name)).unwrap();
        let commands: &[&[&str]] = &[
            &["profile"],
            &["list", "--all"],
            &["list", "--all", "--json"],
        ];
        let served = compare_transports(&fixture, commands, 1, commands).await;
        assert_eq!(served[0].exit, 0);
        assert!(served[0].stdout.contains(name));
        for result in served.iter().skip(1) {
            assert_eq!(result.exit, 1);
            assert!(result.stdout.is_empty());
            assert!(result.stderr.contains(name));
        }
    }
}

/// Project-registry failure cannot refuse commands independent of that registry.
#[tokio::test]
#[serial_test::serial]
async fn a_broken_global_registry_does_not_refuse_the_reads_that_ignore_it() {
    let fixture = Fixture::new();
    fixture.write_broken_global_projects();
    let served = compare_transports(
        &fixture,
        &[
            &["list", "--json"],
            &["status", "--json"],
            &["session", "list-trash"],
        ],
        usize::MAX,
        &[],
    )
    .await;
    for run in &served {
        assert_eq!(
            run.exit, 0,
            "a read that never opens the project registry still answers: {:?}",
            run.stderr
        );
    }
}

/// A stored trailing separator remains part of the session lookup identifier.
#[tokio::test]
#[serial_test::serial]
async fn a_stored_trailing_separator_still_names_its_session() {
    let fixture = Fixture::new();
    let served = compare_transports(
        &fixture,
        &[
            &["session", "show", "/srv/registered/", "--json"],
            &["list", "--json"],
        ],
        usize::MAX,
        &[],
    )
    .await;
    assert!(
        served[0].stdout.contains("m-trailing"),
        "the show found the row the stored path names: {:?}",
        served[0]
    );
    assert!(
        served[0].stdout.contains("/srv/registered/"),
        "the row keeps the spelling it was stored with: {:?}",
        served[0].stdout
    );
    assert!(
        served[1].stdout.contains("/srv/registered/"),
        "the listing reports the stored spelling too: {:?}",
        served[1].stdout
    );
}

/// A whitespace-only profile flag names a profile; only an empty flag is unset.
#[tokio::test]
#[serial_test::serial]
async fn a_whitespace_profile_is_a_profile_name_on_both_transports() {
    let fixture = Fixture::new();
    let served = compare_transports(
        &fixture,
        &[&["list", "-p", "   ", "--json"]],
        0,
        // The refusal is the profile that does not exist, which emptying the
        // sessions does not change.
        &[&["list", "-p", "   ", "--json"]],
    )
    .await;
    assert_eq!(
        served[0].exit, 1,
        "a profile that does not exist is the operator's own state: {:?}",
        served[0].stderr
    );
    assert!(
        served[0].stdout.is_empty(),
        "a refusal prints no rows: {:?}",
        served[0].stdout
    );
    assert!(
        served[0].stderr.contains("Profile '   ' does not exist"),
        "the refusal names the profile the user typed: {:?}",
        served[0].stderr
    );
}

async fn compare_both_transports(fixture: &Fixture) {
    let mut commands: Vec<&[&str]> = COMMANDS.to_vec();
    commands.extend_from_slice(&REFUSALS);
    let _served = compare_transports(
        fixture,
        &commands,
        COMMANDS.len(),
        // Emptying session rows cannot distinguish profile-directory inventory.
        NOT_WITNESSED_BY_EMPTYING,
    )
    .await;
}

/// `refusals_from` bounds the successful prefix; usize::MAX means no refusals.
/// `not_witnessed` names fixture answers unchanged by removing local sessions.
async fn compare_transports(
    fixture: &Fixture,
    commands: &[&[&str]],
    refusals_from: usize,
    not_witnessed: &[&[&str]],
) -> Vec<Run> {
    let xdg_base = fixture.path().join(".config");
    let state = build_test_app_state_with_policy(
        daemon_instances(),
        vec!["127.0.0.1".to_string()],
        Vec::new(),
        None,
    );
    agent_of_empires::server::test_support::accept_runtime_read_cache_for_test(&state).await;
    let shutdown = state.shutdown.clone();
    let home = fixture.path().to_path_buf();

    let mut served: Vec<(Vec<String>, Run)> = Vec::new();
    {
        let daemon = RuntimeUdsTestServer::start_in(&xdg_base, state)
            .unwrap_or_else(|reason| panic!("the fixture home is a trusted namespace: {reason}"));
        for (index, args) in commands.iter().enumerate() {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            let result = run(home.clone(), args.clone()).await;
            if index >= refusals_from {
                assert_ne!(
                    result.exit, 0,
                    "`aoe {}` is a refusal row but exited 0, so nothing about the refusal was compared",
                    args.join(" ")
                );
            }
            served.push((args, result));
        }
        // Empty local sessions while keeping the daemon cache published.
        let saved = fixture.take_sessions();
        for (args, expected) in &served {
            let result = run(home.clone(), args.clone()).await;
            assert_eq!(
                result,
                *expected,
                "`aoe {}` answered from the emptied store, so the served pass was never served",
                args.join(" ")
            );
        }
        fixture.restore_sessions(saved);

        // Retraction makes the next pass use the local store.
        shutdown.cancel();
        daemon.join().await;
    }

    for (args, expected) in &served {
        let local = run(home.clone(), args.clone()).await;
        assert_same(args, expected, &local);
    }

    // Verify which answers are unchanged by emptying the local session registry.
    let saved = fixture.take_sessions();
    let mut unwitnessed: Vec<String> = Vec::new();
    for (args, expected) in &served {
        let local = run(home.clone(), args.clone()).await;
        if &local == expected {
            unwitnessed.push(args.join(" "));
        }
    }
    fixture.restore_sessions(saved);
    let mut declared: Vec<String> = not_witnessed
        .iter()
        .map(|command| command.join(" "))
        .filter(|command| served.iter().any(|(args, _)| args.join(" ") == *command))
        .collect();
    // Compare the declared and observed exceptions as sets.
    unwitnessed.sort();
    declared.sort();
    assert_eq!(
        unwitnessed, declared,
        "the rows the emptied-store probe cannot witness have changed: say why \
         here, or take them out of the set"
    );
    served.into_iter().map(|(_, run)| run).collect()
}

/// Run the synthetic parity fixture in a temporary child of an existing parent.
/// Set AOE_PARITY_HOME to that parent; its existing entries are left untouched.
#[tokio::test]
#[serial_test::serial]
#[ignore = "needs an existing temporary parent in AOE_PARITY_HOME"]
async fn the_named_home_produces_the_same_bytes_on_both_transports() {
    let parent = std::env::var_os("AOE_PARITY_HOME")
        .map(PathBuf::from)
        .expect("AOE_PARITY_HOME must name an existing temporary parent");
    let home = tempfile::TempDir::new_in(parent).expect("temporary child of named parent");
    let fixture = Fixture::seed(home);
    compare_both_transports(&fixture).await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_named_parent_keeps_every_existing_entry_after_comparison_and_drop() {
    let parent = tempfile::tempdir().unwrap();
    std::fs::write(parent.path().join("config.toml"), b"caller configuration\n").unwrap();
    std::fs::create_dir(parent.path().join("profiles")).unwrap();
    let sentinel = parent.path().join("profiles/sessions.json");
    std::fs::write(&sentinel, b"caller session bytes\0\n").unwrap();
    let before = std::fs::read(&sentinel).unwrap();
    let child;
    {
        let fixture = Fixture::seed(tempfile::TempDir::new_in(parent.path()).unwrap());
        child = fixture.path().to_path_buf();
        compare_both_transports(&fixture).await;
    }
    assert!(!child.exists());
    assert_eq!(std::fs::read(&sentinel).unwrap(), before);
    assert_eq!(
        std::fs::read(parent.path().join("config.toml")).unwrap(),
        b"caller configuration\n"
    );
    let mut names: Vec<_> = std::fs::read_dir(parent.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![OsString::from("config.toml"), OsString::from("profiles")]
    );
}
