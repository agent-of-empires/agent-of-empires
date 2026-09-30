//! One store, two transports, the same bytes.
//!
//! Every read command has two implementations: the local one, which opens the
//! store, and the renderer, which paints a snapshot the daemon published. This
//! runs the real `aoe` binary twice per command against the *same* store, once
//! with a daemon to answer and once with no daemon at all so the local command
//! runs, and compares the exit code and both streams. A row, a glyph, a key
//! or a timestamp that differs between the two shows up here as a diff, not as
//! a note in someone's release notes. A command that *refuses* is compared the
//! same way, because the refusal's exit code and its sentence are exactly the
//! things a served read gets wrong.
//!
//! Linux only, because a daemon that cannot publish the namespace refuses
//! with `unsupported_platform` before it touches the filesystem, and the
//! module gate is the honest report of that rather than a skip printed at
//! runtime.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, RuntimeUdsTestServer,
};
use agent_of_empires::session::{Instance, Status, View};

const PROFILE: &str = "main";

/// The profile the broken-store fixture holds. It is named so it sorts after
/// `main`, the profile that carries the sessions: a local listing that
/// printed what it could read and only then gave up would leave those rows on
/// stdout, where a served refusal leaves none at all. The refusal is built
/// before anything is printed, and this name is what makes the comparison
/// prove that rather than happen not to notice it.
const BROKEN: &str = "wrecked";

/// The whole read surface, in the spelling a user types it. One assertion per
/// command, all of them in the single test below. The command set carries the
/// states that used to be invisible here: a human `session show` of a child
/// whose parent was purged, a human `session show` of an archived row whose
/// `State:` line no other command prints, and a `profile` listing over a
/// profile named `default`.
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
    // The terminal row with no `agent_session_id`, named directly, so the local
    // self-heal and the served projection are compared on the one row where
    // they can answer differently.
    &["session", "show", "--json", "l-terminal"],
    &["session", "show", "l-terminal"],
    &["session", "show", "a-archived"],
    // The stored spelling of a project path is the identifier a show is given,
    // so a stored trailing separator has to reach the row that holds it.
    &["session", "show", "--json", "/srv/registered/"],
];

/// The commands a *refusing* read has to agree on too. A command that exits
/// nonzero used to be unassertable here: `run_blocking` demanded success, so
/// adding one of these panicked before anything was compared, and a command
/// whose exit differs between the two transports could not be written down at
/// all. The harness now carries the exit code beside both streams, so these
/// are compared on exactly the same three fields as the succeeding rows above.
///
/// A one-character typo in a session id is the case this exists for: the two
/// halves print the refusal in the operator's own words and both leave 1,
/// because every row here is a refusal on the user's own state rather than a
/// wire failure. That shared exit is what made a divergence invisible.
const REFUSALS: [&[&str]; 4] = [
    &["session", "show", "no-such-session"],
    &["list", "-p", "ghost-profile"],
    &["session", "show", "k-amb"],
    // A whitespace-only `-p` is a profile *name*, so both halves must fail it
    // rather than one of them quietly reading the default profile.
    &["list", "-p", "   ", "--json"],
];

/// One `aoe` invocation as the user sees it: the exit code and both streams.
/// The three are what a refusal is made of, so they are what a comparison of a
/// refusal has to be made of.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    exit: i32,
    stdout: String,
    stderr: String,
}

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
    /// Always a fresh temporary home. The ambient environment cannot name one:
    /// an assertion whose store depends on a variable nobody sets is an
    /// assertion that silently stops testing anything, so the named-home
    /// capability lives in its own `#[ignore]`d smoke run below instead.
    fn new() -> Self {
        Self::new_with_registries(
            serde_json::json!([
                {"name": "registered", "path": "/srv/registered", "scope": "global"},
            ]),
            serde_json::json!([]),
        )
    }

    /// The same store, with the caller supplying both project registries. A
    /// registry may name one directory more than once, and the merged view has
    /// to count that once whichever transport answers.
    fn new_with_registries(global: serde_json::Value, profile: serde_json::Value) -> Self {
        let home = tempfile::tempdir().expect("temp home");
        Self::seed_registries(home.path().to_path_buf(), Some(home), global, profile)
    }

    fn seed(home: PathBuf, owned: Option<tempfile::TempDir>) -> Self {
        Self::seed_registries(
            home,
            owned,
            serde_json::json!([]),
            serde_json::json!([
                {"name": "registered", "path": "/srv/registered", "scope": "global"},
            ]),
        )
    }

    fn seed_registries(
        home: PathBuf,
        owned: Option<tempfile::TempDir>,
        global: serde_json::Value,
        profile: serde_json::Value,
    ) -> Self {
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
        // A registry group no session uses, and the two project registries the
        // caller asked for, so both inventories carry rows the sessions alone
        // would never produce.
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
        // Two more empty profiles, one of them named `default`: the profile
        // listing is a picker, so a profile with that name is printed last, and
        // only a fixture that holds one can tell the two orders apart. They
        // hold no sessions, so the profile-scoped commands are unaffected.
        for name in ["default", "zeta"] {
            std::fs::create_dir_all(app_dir.join("profiles").join(name)).expect("extra profile");
        }
        // The update check is the one preflight step that reaches the network,
        // and its notice would land on stdout and make the two passes differ for
        // a reason that has nothing to do with the transport. The default is
        // named explicitly so it does not follow the new profiles' spelling.
        std::fs::write(
            app_dir.join("config.toml"),
            format!("default_profile = \"{PROFILE}\"\n\n[updates]\nupdate_check_mode = \"off\"\n"),
        )
        .expect("config");
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

    /// A profile the store cannot be read from, because its `groups.json` is
    /// not the JSON array the store expects of it. Both transports have to
    /// refuse it the same way: the daemon reports the profile's enumeration
    /// degraded, and the local command fails to load it.
    ///
    /// The break is in `groups.json` rather than `sessions.json` for a
    /// reason that is not cosmetic. Migrations run before every command and
    /// read `sessions.json`, and a migration refuses a session registry that
    /// is not a JSON array, so an unreadable `sessions.json` would be turned
    /// away before the read this test is about and the two transports would
    /// agree for the wrong reason. `groups.json` is not a migration's
    /// business, and a store that will not load it is the same failure the
    /// all-profiles listing has to survive.
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

    /// A global `projects.json` that is not the JSON the store expects. Only
    /// the project listing consumes this registry, so every other read stays
    /// answerable and the served transport must not refuse on it.
    fn write_broken_global_projects(&self) {
        std::fs::write(self.global_projects_path(), b"not json").expect("break the registry");
    }

    fn global_projects_path(&self) -> PathBuf {
        self.path()
            .join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
            .join("projects.json")
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
        // A child whose parent is gone: the stored id is kept and printed by
        // both paths, so a producer that cleared it, or a client that refused
        // it, shows up here rather than in a reviewer's reading.
        row(
            "j-orphan",
            idle,
            "/srv/registered",
            "",
            "Orphan",
            false,
            false,
        ),
        // Two ids that share a prefix, so `session show k-amb` has more than
        // one candidate and no full id tells the pair apart. No other fixture
        // id starts with `k`, so the prefix is ambiguous on its own rather
        // than one row among several. The local command prints the candidates;
        // the served one refuses with `session_ambiguous` and discards them,
        // which is only observable at all if the store holds the case.
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
        // A terminal row with no `agent_session_id`: the local command's
        // self-heal backfills one, the served projection reports the daemon's
        // own view, so this is the row that can see a served answer and a
        // local one disagree. `Stopped` is required rather than incidental:
        // the probe returns before the tmux read for Stopped, so the local
        // pass stays probe-inert. This must be the only Terminal row, or the
        // comparison starts depending on whether a tmux server is running.
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
        // A stored project path with a trailing separator. The store treats
        // `/srv/registered` and `/srv/registered/` as one project, so the
        // path is admissible, and it has to stay the spelling the row is
        // identified by: the producer used to normalise it away, after which
        // `aoe session show /srv/registered/` could not find the row it named.
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
        // A workspace whose repos are stored in an order no canonical sort
        // would produce, so `aoe list --json` reads the array differently the
        // moment either side sorts it.
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

/// The stored spelling of one workspace repo, so the fixture's `workspace_info`
/// is a row the local projection can emit verbatim.
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
    // `.25` and `.5` are deliberately short. They are the only observation
    // this gate has that an instant is normalised to the spelling the local
    // command prints, and a tidy-up to nine digits would remove the coverage
    // silently.
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
async fn run(home: PathBuf, args: Vec<String>) -> Run {
    tokio::task::spawn_blocking(move || run_blocking(&home, &args))
        .await
        .expect("the command task joins")
}

/// One invocation, whole. The exit code is carried rather than asserted: a
/// command that refuses is a result the two transports have to agree on, and
/// an assertion that it succeeded made such a command unwriteable here.
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

/// The whole comparison, as a value: what a served run and a local run for the
/// same command must agree on, and where they do not. Returned rather than
/// asserted so the harness's own coverage is testable: a comparison that
/// quietly stopped looking at a command would otherwise be indistinguishable
/// from one that passed.
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

/// The assertion every command here ends in, refusing or not: the two
/// transports must produce the same exit code and the same bytes on both
/// streams. For a command that succeeds this is exactly the stdout equality
/// the branch claims, with two more fields agreeing; for one that refuses it
/// is the only assertion that can exist.
fn assert_same(args: &[String], served: &Run, local: &Run) {
    if let Some(difference) = difference(args, served, local) {
        panic!("{difference}");
    }
}

/// The two transports are the ones a user actually has: the local daemon's own
/// socket, and no daemon at all. The publication is dropped before the second
/// pass, so the local run really does fall through to the store.
#[tokio::test]
#[serial_test::serial]
async fn the_served_bytes_are_the_bytes_the_local_command_prints() {
    let fixture = Fixture::new();
    compare_both_transports(&fixture).await;
}

/// Which projects exist is the store's answer, not a rendering choice, so a
/// registry that names one directory under two spellings must produce the same
/// inventory on both transports. The local path merges on the canonical path;
/// a renderer that matched the wire string instead reported both spellings as
/// their own projects, and a global row and a profile row for one directory both
/// survived where locally the profile one shadows the global one. Byte
/// equality over the listing is the assertion, so the count, the surviving row
/// and the `scope` it reports all have to agree.
///
/// The second spelling is a symlink, which is the reachable form of the
/// duplicate: an ordinary absolute path that names a directory the other row
/// already names, so it passes the wire's path grammar and still reaches the
/// merge.
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

/// A store the reader cannot open in full has to fail closed on both
/// transports, because a partial listing is a wrong answer wearing a zero
/// exit. The served half already refused, so this is the local command that
/// used to skip the unreadable profile, print the rest and exit 0: a script
/// reading that total would have believed the store held fewer sessions than
/// it does, and only when a daemon happened to be publishing, which is the
/// recommended local setup.
///
/// The refusal has to name the profile, because the name is the only thing
/// the operator can act on. The parity comparison has already proved the two
/// transports printed the same bytes, so the served copy is the whole claim.
#[tokio::test]
#[serial_test::serial]
async fn a_profile_that_cannot_be_read_is_refused_by_both_transports() {
    let fixture = Fixture::new();
    fixture.write_broken_profile();
    // The picker row comes first because `refusals_from` is the index the
    // succeeding rows end at, and this set has one succeeding row.
    let served = compare_transports(
        &fixture,
        &[
            // The picker names profiles without opening them, so it still
            // answers here. Refusing on a component it never consults is the
            // over-refusal half of the same defect, and the local command is
            // the oracle for what this one should print.
            &["profile"],
            &["list", "--all"],
            &["list", "--json", "--all"],
        ],
        1,
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

/// A store whose global project registry will not parse is still a store the
/// session reads can answer from: no session, status, group or profile read
/// consults the registry. The served renderer refused all of them anyway,
/// turning one corrupt file into `health_degraded` on commands that never read
/// it. These three are the ones that were refused, and `usize::MAX` is the
/// refusal index because every one of them must succeed: a served refusal
/// against a local success is a difference the comparison reports rather than
/// two agreeing refusals.
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

/// A stored project path is the identifier a `session show` is given, trailing
/// separator and all. The producer normalised it away, so a served show by the
/// stored spelling answered "not found" while the local one found the row, and
/// the listing showed a path no row could be found by.
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

/// Only the empty profile is "no profile". A whitespace-only `-p` names a
/// profile that does not exist, and the local command says so at exit 1; the
/// served half used to read the default profile instead and answer `[]` at
/// exit 0. The variable keeps its own local rule, which is the oracle for
/// this comparison rather than something asserted here.
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

    // An explicitly empty `-p` is the one value that means "no selection", and a
    // named profile still beats a variable that is not empty.
    let home = fixture.path().to_path_buf();
    let explicit_empty = run(
        home.clone(),
        vec![
            "list".to_string(),
            "-p".to_string(),
            String::new(),
            "--json".to_string(),
        ],
    )
    .await;
    assert_eq!(
        explicit_empty.exit, 0,
        "an empty flag is the default profile: {:?}",
        explicit_empty.stderr
    );
    let named = run(
        home,
        vec![
            "list".to_string(),
            "-p".to_string(),
            PROFILE.to_string(),
            "--json".to_string(),
        ],
    )
    .await;
    assert_eq!(
        named.exit, 0,
        "a named profile beats a blank variable: {:?}",
        named.stderr
    );
}

async fn compare_both_transports(fixture: &Fixture) {
    let mut commands: Vec<&[&str]> = COMMANDS.to_vec();
    commands.extend_from_slice(&REFUSALS);
    let _served = compare_transports(fixture, &commands, COMMANDS.len()).await;
}

/// `refusals_from` is the index at which the succeeding rows end; `usize::MAX`
/// for a set with no refusal rows. Past it every row has to have actually
/// refused on the served pass: a "refusal" that succeeded would be compared on
/// stdout alone and would pass whether or not the two transports agreed about
/// the refusal, which is exactly the gap this row set exists to close.
async fn compare_transports(
    fixture: &Fixture,
    commands: &[&[&str]],
    refusals_from: usize,
) -> Vec<Run> {
    let xdg_base = fixture.path().join(".config");
    let state = build_test_app_state_with_policy(
        daemon_instances(),
        vec!["127.0.0.1".to_string()],
        Vec::new(),
        None,
    );
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
            without_a_store.stdout.contains("long-session"),
            "the served pass answered from the local store, not from the daemon:\n{}\nSTDERR: {}\nEXIT: {}",
            without_a_store.stdout, without_a_store.stderr, without_a_store.exit
        );

        // Shutdown retracts the publication, so the second pass really does
        // find no daemon publishing into this namespace.
        shutdown.cancel();
        daemon.join().await;
    }

    for (args, expected) in &served {
        let local = run(home.clone(), args.clone()).await;
        assert_same(args, expected, &local);
    }
    served.into_iter().map(|(_, run)| run).collect()
}

/// Hermeticity, asserted rather than intended: with the update check off in the
/// seeded config, a probe read must come back with no update notice on either
/// stream. A notice here would mean the fixture depends on the network, and the
/// comparison below would be measuring that instead of the transport.
#[tokio::test]
#[serial_test::serial]
async fn the_fixture_read_reaches_no_network() {
    let fixture = Fixture::new();
    let home = fixture.path().to_path_buf();
    let output = tokio::task::spawn_blocking({
        let home = home.clone();
        move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_aoe"));
            command
                .current_dir(&home)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env_remove("AOE_DAEMON_URL")
                .env_remove("AOE_DAEMON_TOKEN")
                .args(["list"]);
            command.output().expect("the aoe binary runs")
        }
    })
    .await
    .expect("the probe task joins");
    let streams = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !streams.contains("Update available"),
        "the fixture read reached the network:\n{streams}"
    );
}

/// The same comparison, against a home the operator names, so a real store can
/// be replayed through both transports out of band:
///
/// ```text
/// AOE_PARITY_HOME=~/src/my-project cargo test --test integration \
///   cli_read_parity -- --ignored --nocapture
/// ```
///
/// Ignored by default: the assertion suite above must not read the ambient
/// environment, and this is the one case that deliberately does.
#[tokio::test]
#[serial_test::serial]
#[ignore = "needs a named home in AOE_PARITY_HOME"]
async fn the_named_home_produces_the_same_bytes_on_both_transports() {
    let home = match std::env::var_os("AOE_PARITY_HOME") {
        Some(home) => PathBuf::from(home),
        None => panic!("AOE_PARITY_HOME must name a home for this run"),
    };
    std::fs::create_dir_all(&home).expect("the named home");
    let fixture = Fixture::seed(home, None);
    compare_both_transports(&fixture).await;
}

/// The comparison itself, checked. Two in-session reviews found the same gap
/// in this file, a command that refuses could not be compared at all, so the
/// refusal rows are only worth anything if the comparison notices when two
/// refusals differ. This is what pins that: a differing exit code, a differing
/// sentence and a differing stdout are each reported, and identical runs are
/// not.
#[test]
fn a_differing_refusal_is_reported_not_swallowed() {
    let args: Vec<String> = ["session", "show", "no-such-session"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let refusal = Run {
        exit: 4,
        stdout: String::new(),
        stderr: "daemon read: session_missing\n".to_string(),
    };
    assert_eq!(difference(&args, &refusal, &refusal), None);
    let other_exit = Run {
        exit: 1,
        ..refusal.clone()
    };
    assert!(
        difference(&args, &refusal, &other_exit)
            .expect("a differing exit code is a difference")
            .contains("exits differently"),
        "the exit code must be part of the comparison"
    );
    let other_sentence = Run {
        stderr: "No sessions found matching 'no-such-session'.\n".to_string(),
        ..refusal.clone()
    };
    assert!(
        difference(&args, &refusal, &other_sentence)
            .expect("a differing sentence is a difference")
            .contains("reports differently"),
        "the refusal's own sentence must be part of the comparison"
    );
    let other_stdout = Run {
        stdout: "a row".to_string(),
        ..refusal.clone()
    };
    assert!(
        difference(&args, &refusal, &other_stdout)
            .expect("a differing stdout is a difference")
            .contains("differs between the two transports"),
        "stdout must stay part of the comparison"
    );
}
