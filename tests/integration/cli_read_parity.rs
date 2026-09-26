//! One store, two transports, the same bytes.
//!
//! Every read command has two implementations: the local one, which opens the
//! store, and the renderer, which paints a snapshot the daemon published. This
//! runs the real `aoe` binary twice per command against the *same* store — once
//! with a daemon to answer, once with no daemon at all so the local command
//! runs — and compares stdout byte for byte. A row, a glyph, a key or a
//! timestamp that differs between the two shows up here as a diff, not as a
//! note in someone's release notes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, RuntimeUdsTestServer,
};
use agent_of_empires::session::{Instance, Status, View};

const PROFILE: &str = "main";

/// The whole read surface, in the spelling a user types it. One assertion per
/// command, all of them in the single test below.
const COMMANDS: [&[&str]; 15] = [
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
    &["session", "list-trash"],
];

/// The temporary home plus the environment binding that points both the
/// daemon's own reads and the subprocesses at it, restored on drop.
struct Fixture {
    /// The home the store lives in. A temporary one is owned by the fixture and
    /// removed with it; a named one belongs to the caller.
    home: PathBuf,
    _owned: Option<tempfile::TempDir>,
    previous: Vec<(&'static str, Option<OsString>)>,
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
    /// The store lives in a temporary home unless `AOE_PARITY_HOME` names one,
    /// which is how an out-of-band smoke run reuses exactly the fixture these
    /// assertions compare.
    fn new() -> Self {
        if let Some(base) = std::env::var_os("AOE_PARITY_HOME") {
            let home = PathBuf::from(base);
            std::fs::create_dir_all(&home).expect("the named home");
            return Self::seed(home, None);
        }
        let home = tempfile::tempdir().expect("temp home");
        Self::seed(home.path().to_path_buf(), Some(home))
    }

    fn seed(home: PathBuf, owned: Option<tempfile::TempDir>) -> Self {
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
        // A registry group no session uses, and a registry project, so both
        // inventories carry a row the sessions alone would never produce.
        std::fs::write(
            profile_dir.join("groups.json"),
            serde_json::to_vec_pretty(&serde_json::json!([
                {"name": "empty", "path": "empty", "collapsed": false},
                {"name": "work", "path": "work", "collapsed": false},
            ]))
            .expect("groups"),
        )
        .expect("seed groups");
        std::fs::write(
            profile_dir.join("projects.json"),
            serde_json::to_vec_pretty(&serde_json::json!([
                {"name": "registered", "path": "/srv/registered", "scope": "global"},
            ]))
            .expect("projects"),
        )
        .expect("seed projects");
        let base = home.clone();
        let mut fixture = Self {
            home: base.clone(),
            _owned: owned,
            previous: Vec::new(),
        };
        for (key, value) in [
            ("HOME", base.clone()),
            ("XDG_CONFIG_HOME", base.join(".config")),
            ("XDG_DATA_HOME", base.join(".local/share")),
        ] {
            fixture.previous.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        std::fs::create_dir_all(base.join(".config")).expect("xdg base");
        fixture
    }

    fn path(&self) -> &Path {
        &self.home
    }
}

/// The store every command in this test reads. It is built to hit the cases the
/// two transports used to get apart: an id wider than the display column, a
/// path under `$HOME`, a group and a project that exist only in a registry, a
/// session whose directory no registry names, a parent/child pair, a trashed
/// and an archived row, and one session in each of the five statuses.
///
/// The rows are structured (ACP) sessions on purpose: `aoe status` re-probes
/// the tmux server for every terminal row, which would make the local answer
/// depend on whether a tmux server happens to be running. A structured row is
/// left alone by that probe, so the comparison is about the transport and
/// nothing else.
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
    ];
    let mut rows: Vec<serde_json::Value> = rows;
    // A parent/child relation, spelled the way the store spells it.
    for row in &mut rows {
        if row["id"] == "i-child" {
            row["parent_session_id"] = serde_json::json!("h-parent");
        }
    }
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    rows
}

#[allow(clippy::too_many_arguments)]
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
    // `Instance::new` mints its own id, so the fixture names the one the
    // commands will be asked for.
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
    if archived {
        instance.archived_at = Some("2026-02-03T04:05:06.25Z".parse().expect("archived_at"));
    }
    if trashed {
        instance.trashed_at = Some("2026-03-04T05:06:07.5Z".parse().expect("trashed_at"));
    }
    serde_json::to_value(&instance).expect("row serializes")
}

/// The daemon's own view of the store: the same rows the local command loads,
/// put through the same status probe, because that probe is what a running
/// daemon's own watcher does to its cache.
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

/// The daemon lives in this process, so the command runs off the runtime's
/// worker threads: a synchronous child would otherwise keep the task that
/// answers the connection from ever being polled.
async fn run(home: PathBuf, args: Vec<String>) -> String {
    tokio::task::spawn_blocking(move || run_blocking(&home, &args))
        .await
        .expect("the command task joins")
}

fn run_blocking(home: &Path, args: &[String]) -> String {
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
    assert!(
        output.status.success(),
        "`aoe {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is utf-8")
}

/// The two transports are the ones a user actually has: the local daemon's own
/// socket, and no daemon at all. The publication is dropped before the second
/// pass, so the local run really does fall through to the store.
#[tokio::test]
#[serial_test::serial]
async fn the_served_bytes_are_the_bytes_the_local_command_prints() {
    let fixture = Fixture::new();
    let xdg_base = fixture.path().join(".config");
    let state = build_test_app_state_with_policy(
        daemon_instances(),
        vec!["127.0.0.1".to_string()],
        Vec::new(),
        None,
    );
    let shutdown = state.shutdown.clone();
    let home = fixture.path().to_path_buf();

    let mut served: Vec<(Vec<String>, String)> = Vec::new();
    {
        let daemon = RuntimeUdsTestServer::start_in(&xdg_base, state)
            .expect("the fixture home is a trusted namespace");
        for args in COMMANDS {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            served.push((args.clone(), run(home.clone(), args).await));
        }
        // The pass above is only worth comparing if it was served at all. With
        // the store emptied out from under the client, a local read would answer
        // "no sessions"; a served one answers from the daemon's own cache. So
        // this assertion fails loudly if the served pass quietly fell back.
        let sessions = xdg_base
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
            .join("profiles")
            .join(PROFILE)
            .join("sessions.json");
        let store = std::fs::read(&sessions).expect("the fixture store");
        std::fs::write(&sessions, "[]").expect("empty the store");
        let without_a_store = run(home.clone(), vec!["list".to_string()]).await;
        std::fs::write(&sessions, &store).expect("restore the store");
        assert!(
            without_a_store.contains("long-session"),
            "the served pass answered from the local store, not from the daemon:\n{without_a_store}"
        );

        // Shutdown retracts the publication, so the second pass really does
        // find no daemon publishing into this namespace.
        shutdown.cancel();
        daemon.join().await;
    }

    for (args, expected) in served {
        let local = run(home.clone(), args.clone()).await;
        assert_eq!(
            expected,
            local,
            "`aoe {}` differs between the two transports",
            args.join(" ")
        );
    }
}
