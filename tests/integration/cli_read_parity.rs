//! Preserve CLI projections over live local UDS and loopback HTTP transports.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, serve_owner_router_for_test,
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
        std::fs::write(
            app_dir.join("config.toml"),
            format!("default_profile = \"{PROFILE}\"\n\n[updates]\nupdate_check_mode = \"off\"\n[systemd]\nenabled = false\n"),
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
    let mut instances = Vec::new();
    for profile in agent_of_empires::session::list_profiles().expect("profiles") {
        let Ok((mut rows, _)) = agent_of_empires::session::Storage::open_unwatched(&profile)
            .and_then(|storage| storage.load_with_groups())
        else {
            continue;
        };
        for instance in &mut rows {
            instance.source_profile = profile.clone();
            instance.update_status_once(None, None);
        }
        instances.extend(rows);
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

#[tokio::test]
#[serial_test::serial]
async fn local_and_http_daemons_preserve_the_cli_projection() {
    let fixture = Fixture::new();
    let mut rows = fixture_sessions(fixture.path());
    rows.push(row(
        "n-legacy-worktree",
        Status::Stopped,
        "/srv/repo/../repo-worktrees/feat",
        "",
        "Legacy worktree",
        false,
        false,
    ));
    std::fs::write(
        fixture.sessions_path(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
    compare_both_transports(&fixture).await;
}

#[tokio::test]
#[serial_test::serial]
async fn profile_scoped_ids_preserve_each_store_on_both_transports() {
    let fixture = Fixture::new();
    let mut child = row(
        "i-child",
        Status::Idle,
        "/srv/registered",
        "",
        "Other child",
        false,
        false,
    );
    child["parent_session_id"] = serde_json::json!("h-parent");
    let copied = vec![
        row(
            "h-parent",
            Status::Idle,
            "/srv/registered",
            "",
            "Other parent",
            false,
            false,
        ),
        child,
    ];
    std::fs::write(
        fixture.app_dir().join("profiles/zeta/sessions.json"),
        serde_json::to_vec_pretty(&copied).unwrap(),
    )
    .unwrap();
    let answers = compare_transports(
        &fixture,
        &[
            &["list", "--json", "-p", "main"],
            &["list", "--json", "-p", "zeta"],
            &["list", "--json", "--all"],
            &["list", "--all"],
            &["session", "show", "i-child", "-p", "main"],
            &["session", "show", "i-child", "-p", "zeta"],
        ],
        usize::MAX,
    )
    .await;
    let all: Vec<serde_json::Value> = serde_json::from_str(&answers[2].stdout).unwrap();
    let mut parents: Vec<_> = all
        .iter()
        .filter(|row| row["id"] == "h-parent")
        .map(|row| {
            (
                row["profile"].as_str().unwrap(),
                row["title"].as_str().unwrap(),
            )
        })
        .collect();
    parents.sort_unstable();
    assert_eq!(parents, [("main", "Parent"), ("zeta", "Other parent")]);
    assert!(answers[4].stdout.contains("Parent"));
    assert!(!answers[4].stdout.contains("Other parent"));
    assert!(answers[5].stdout.contains("Other parent"));
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
    )
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn unreadable_profiles_preserve_inventory_and_healthy_aggregate_rows() {
    for name in [BROKEN, "ALL", "legacy\\name"] {
        let fixture = Fixture::new();
        if name == BROKEN {
            fixture.write_broken_profile();
        } else {
            std::fs::create_dir_all(fixture.app_dir().join("profiles").join(name)).unwrap();
        }
        let served = compare_transports_with(
            &fixture,
            &[
                &["profile"],
                &["list", "--all"],
                &["list", "--json", "--all"],
            ],
            usize::MAX,
            |args, served, local| {
                if args.first().is_none_or(|arg| arg != "list") {
                    assert_same(args, served, local);
                    return;
                }
                assert_eq!(served.stdout, local.stdout, "{name}: {}", args.join(" "));
                assert_eq!(served.exit, local.exit, "{name}: {}", args.join(" "));
                for answer in [served, local] {
                    assert!(answer.stderr.contains(name), "{name}: {}", answer.stderr);
                }
            },
        )
        .await;
        assert_eq!(served[0].exit, 0);
        assert_eq!(served[1].exit, 0);
        assert_eq!(served[2].exit, 1);
        assert!(served[0].stdout.contains(name));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&served[2].stdout).unwrap();
        let mut actual: Vec<_> = rows
            .iter()
            .map(|row| {
                assert_eq!(row["profile"], PROFILE);
                row["id"].as_str().unwrap().to_owned()
            })
            .collect();
        let mut expected: Vec<_> = fixture_sessions(fixture.path())
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "healthy rows must survive {name}");
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
    let served = compare_transports(&fixture, &[&["list", "-p", "   ", "--json"]], 0).await;
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
    let _served = compare_transports(fixture, &commands, COMMANDS.len()).await;
}

/// `refusals_from` bounds the successful prefix; usize::MAX means no refusals.
async fn compare_transports(
    fixture: &Fixture,
    commands: &[&[&str]],
    refusals_from: usize,
) -> Vec<Run> {
    compare_transports_with(fixture, commands, refusals_from, assert_same).await
}

async fn compare_transports_with(
    fixture: &Fixture,
    commands: &[&[&str]],
    refusals_from: usize,
    compare: impl Fn(&[String], &Run, &Run),
) -> Vec<Run> {
    // Normalize caller-prepared data before caching or comparing either transport.
    tokio::task::spawn_blocking(agent_of_empires::migrations::run_migrations)
        .await
        .expect("the fixture migration task joins")
        .expect("migrate the fixture before publishing its read cache");
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
        let socket = xdg_base
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
            .join("daemon/api.sock");
        let owner = serve_owner_router_for_test(
            &socket,
            agent_of_empires::server::test_support::build_router_for_test(state.clone()),
        )
        .await;
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = agent_of_empires::server::test_support::build_router_for_test(state.clone());
        let stopped = shutdown.clone();
        let http = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
        });
        for (args, expected) in &served {
            let mut remote_args = args.clone();
            remote_args.extend(["--daemon-url".into(), url.clone()]);
            let remote = run(home.clone(), remote_args).await;
            compare(args, expected, &remote);
        }
        shutdown.cancel();
        owner.abort();
        let _ = owner.await;
        http.await.unwrap();
    }
    for (args, _) in &served {
        let mut disabled = args.clone();
        disabled.push("--no-autostart".into());
        let absent = run(home.clone(), disabled).await;
        assert_ne!(absent.exit, 0, "{}", args.join(" "));
        assert!(absent.stdout.is_empty(), "{absent:?}");
        assert!(!absent.stderr.contains("Started aoe daemon"), "{absent:?}");
    }
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
