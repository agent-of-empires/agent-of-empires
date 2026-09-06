//! `agent-of-empires list` command implementation

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::session::{Instance, SessionBucket, SessionScope, Storage};

const TABLE_COL_TITLE: usize = 20;
const TABLE_COL_GROUP: usize = 15;
const TABLE_COL_PATH: usize = 40;
const TABLE_COL_ID_DISPLAY: usize = 12;
const TABLE_COL_STATE: usize = 9;

/// The `aoe list --state=` vocabulary. Mirrors the REST API's
/// `SessionScope` (`GET /api/sessions?state=`) so the two vocabularies
/// share one source of truth (#3350). Kept as a clap-facing enum here
/// rather than deriving `ValueEnum` on the wire type: the API rejects
/// unrecognized values via serde with a JSON 400, while clap wants its
/// own `PossibleValue` list for `--help` and `--state=?` errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
enum StateFilter {
    /// Only sessions that are neither archived nor trashed.
    Live,
    /// Only sessions currently in the trash.
    Trashed,
    /// Every persisted session in the profile (default).
    All,
}

impl From<StateFilter> for SessionScope {
    fn from(v: StateFilter) -> Self {
        match v {
            StateFilter::Live => SessionScope::Live,
            StateFilter::Trashed => SessionScope::Trashed,
            StateFilter::All => SessionScope::All,
        }
    }
}

#[derive(Args)]
pub struct ListArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// List sessions from all profiles
    #[arg(long)]
    all: bool,

    /// Filter by session state. Defaults to `all`, every persisted session,
    /// which is what `aoe list` has always shown. Pass `--state=live` to skip
    /// trashed and archived rows; the vocabulary matches the REST API's
    /// `GET /api/sessions?state=`.
    #[arg(long, value_enum, default_value_t = StateFilter::All)]
    state: StateFilter,
}

/// Simple string tag describing whether a session is live/archived/trashed,
/// exposed alongside `trashed_at`/`archived_at` in `--json` so a consumer
/// keying on state does not have to reason about the timestamps itself.
/// `live` and `trashed` are the [`SessionScope`] filter vocabulary; `archived`
/// is an output-only third value, since `--state=archived` is not a filter the
/// API offers either (an archived row is excluded by `live` and by `trashed`,
/// and only shows under `all`).
///
/// Derived from [`Instance::effective_bucket`] so the `Trashed > Archived >
/// Active` precedence has exactly one definition: an archived row that is then
/// trashed keeps its `archived_at` but reports `trashed`.
pub(super) fn state_tag(inst: &Instance) -> &'static str {
    match inst.effective_bucket() {
        SessionBucket::Trashed => "trashed",
        SessionBucket::Archived => "archived",
        SessionBucket::Active => "live",
    }
}

/// Mirrors the API's snooze surfacing rule (`SessionResponse::from_instance`,
/// `src/server/api/sessions/model.rs`): expose `snoozed_until` only while
/// [`Instance::is_snoozed`] holds. An expired deadline stays persisted until
/// the next mutation rewrites it, so without this gate a woken row would keep
/// advertising a snooze that already ended. Shared by `session show --json`
/// so both CLI projections gate identically.
pub(super) fn active_snoozed_until(inst: &Instance) -> Option<chrono::DateTime<chrono::Utc>> {
    if inst.is_snoozed() {
        inst.snoozed_until
    } else {
        None
    }
}

#[derive(Serialize)]
struct SessionJson {
    id: String,
    title: String,
    path: String,
    group: String,
    tool: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    command: String,
    profile: String,
    /// One of `live`, `archived`, `trashed`; the natural way to distinguish a
    /// trashed row from a failed one that #3350 was filed for.
    state: &'static str,
    created_at: chrono::DateTime<chrono::Utc>,
    /// Set iff the session is currently in the trash. Together with
    /// `archived_at`, lets a scripted consumer read the state without a
    /// second `aoe session list-trash` shellout.
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set iff the session is currently archived. Orthogonal to `trashed_at`
    /// on the store: `trash()` deliberately leaves `archived_at` alone so a
    /// restore is faithful, so both keys can be present at once and `state`
    /// reports `trashed` in that case.
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set while a snooze deadline is in the future, gated on
    /// [`Instance::is_snoozed`] exactly like the API: an expired deadline
    /// lingers on disk but the row has woken, so the key disappears instead
    /// of advertising a stale snooze. Orthogonal to `state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    /// Set iff the session is currently pinned for the web sidebar.
    /// Independent of `state`; the client derives the boolean as
    /// `pinned_at != null`, matching the API field from #1581.
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Empty for single-repo sessions; populated with one entry per repo
    /// (including the primary) for sessions created with `--repo`/`--project`.
    workspace_repos: Vec<WorkspaceRepoJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<WorktreeJson>,
}

#[derive(Serialize)]
struct WorkspaceRepoJson {
    name: String,
    source_path: String,
    branch: String,
}

#[derive(Serialize)]
struct WorktreeJson {
    branch: String,
    main_repo_path: String,
    managed_by_aoe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_branch: Option<String>,
}

fn worktree_for(inst: &Instance) -> Option<WorktreeJson> {
    inst.worktree_info.as_ref().map(|w| WorktreeJson {
        branch: w.branch.clone(),
        main_repo_path: w.main_repo_path.clone(),
        managed_by_aoe: w.managed_by_aoe,
        base_branch: w.base_branch.clone(),
    })
}

fn session_json(inst: &Instance, profile: &str) -> SessionJson {
    SessionJson {
        id: inst.id.clone(),
        title: inst.title.clone(),
        path: inst.project_path.clone(),
        group: inst.group_path.clone(),
        tool: inst.tool.clone(),
        command: inst.command.clone(),
        profile: profile.to_string(),
        state: state_tag(inst),
        created_at: inst.created_at,
        trashed_at: inst.trashed_at,
        archived_at: inst.archived_at,
        snoozed_until: active_snoozed_until(inst),
        pinned_at: inst.pinned_at,
        workspace_repos: workspace_repos_for(inst),
        worktree: worktree_for(inst),
    }
}

/// Via `all_repos` so a session converted by `attach_project` (#3103) lists both
/// repos here, exactly like one created multi-repo.
fn workspace_repos_for(inst: &Instance) -> Vec<WorkspaceRepoJson> {
    inst.all_repos()
        .iter()
        .map(|r| WorkspaceRepoJson {
            name: r.name.clone(),
            source_path: r.source_path.clone(),
            branch: r.branch.clone(),
        })
        .collect()
}

fn print_table_header(show_state: bool) {
    if show_state {
        println!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {:<width_state$} ID",
            "TITLE",
            "GROUP",
            "PATH",
            "STATE",
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH,
            width_state = TABLE_COL_STATE,
        );
        println!(
            "{}",
            "-".repeat(
                TABLE_COL_TITLE
                    + TABLE_COL_GROUP
                    + TABLE_COL_PATH
                    + TABLE_COL_STATE
                    + TABLE_COL_ID_DISPLAY
                    + 6
            )
        );
    } else {
        println!(
            "{:<width_title$} {:<width_group$} {:<width_path$} ID",
            "TITLE",
            "GROUP",
            "PATH",
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH
        );
        println!(
            "{}",
            "-".repeat(
                TABLE_COL_TITLE + TABLE_COL_GROUP + TABLE_COL_PATH + TABLE_COL_ID_DISPLAY + 5
            )
        );
    }
}

fn print_table_row(inst: &Instance, show_state: bool) {
    let title = super::truncate(&inst.title, TABLE_COL_TITLE);
    let group = super::truncate(&inst.group_path, TABLE_COL_GROUP);
    let path = super::truncate(&inst.project_path, TABLE_COL_PATH);
    let id_display = super::truncate_id(&inst.id, TABLE_COL_ID_DISPLAY);
    if show_state {
        println!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {:<width_state$} {}",
            title,
            group,
            path,
            state_tag(inst),
            id_display,
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH,
            width_state = TABLE_COL_STATE,
        );
    } else {
        println!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {}",
            title,
            group,
            path,
            id_display,
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH
        );
    }
}

/// Whether the human table should render the `STATE` column. Off for
/// `--state=live` (every row is `live` and the column carries no
/// information) and off when filtering to only `trashed` (same). Only
/// meaningful under `all`, where rows are mixed and a scripted consumer
/// or human reader benefits from distinguishing a live row from a trashed
/// or archived one.
fn table_shows_state(scope: SessionScope) -> bool {
    matches!(scope, SessionScope::All)
}

#[tracing::instrument(target = "cli.list", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: ListArgs) -> Result<()> {
    let scope: SessionScope = args.state.into();
    if args.all {
        return run_all_profiles(args.json, scope).await;
    }

    let storage = Storage::open_unwatched(profile)?;
    let (all_instances, _) = storage.load_with_groups()?;
    let instances: Vec<Instance> = all_instances
        .into_iter()
        .filter(|inst| SessionScope::matches(Some(scope), inst))
        .collect();

    // `--json` is answered before the empty-listing message: an empty result is
    // `[]`, never human prose on stdout, matching `aoe ps --json` and `aoe group
    // list --json`. `--state=live` makes this reachable with sessions present
    // (a profile whose rows are all trashed), which is precisely the scripted
    // consumer #3350 is about.
    if args.json {
        let sessions: Vec<SessionJson> = instances
            .iter()
            .map(|inst| session_json(inst, storage.profile()))
            .collect();
        super::output::print_json(&sessions)?;
        return Ok(());
    }

    if instances.is_empty() {
        println!("No sessions found in profile '{}'.", storage.profile());
        return Ok(());
    }

    let show_state = table_shows_state(scope);
    println!("Profile: {}\n", storage.profile());
    print_table_header(show_state);
    for inst in &instances {
        print_table_row(inst, show_state);
    }
    println!("\nTotal: {} sessions", instances.len());

    crate::update::print_update_notice().await;

    Ok(())
}

async fn run_all_profiles(json: bool, scope: SessionScope) -> Result<()> {
    let profiles = crate::session::list_profiles()?;

    if profiles.is_empty() {
        println!("No profiles found.");
        return Ok(());
    }

    if json {
        let mut all_sessions: Vec<SessionJson> = Vec::new();
        for profile_name in &profiles {
            if let Ok(storage) = Storage::open_unwatched(profile_name) {
                if let Ok((instances, _)) = storage.load_with_groups() {
                    for inst in &instances {
                        if !SessionScope::matches(Some(scope), inst) {
                            continue;
                        }
                        all_sessions.push(session_json(inst, profile_name));
                    }
                }
            }
        }
        super::output::print_json(&all_sessions)?;
        return Ok(());
    }

    let show_state = table_shows_state(scope);
    let mut total_sessions = 0;
    for profile_name in &profiles {
        if let Ok(storage) = Storage::open_unwatched(profile_name) {
            if let Ok((all_instances, _)) = storage.load_with_groups() {
                let instances: Vec<&Instance> = all_instances
                    .iter()
                    .filter(|inst| SessionScope::matches(Some(scope), inst))
                    .collect();
                if instances.is_empty() {
                    continue;
                }

                println!("\n═══ Profile: {} ═══\n", profile_name);
                print_table_header(show_state);
                for inst in &instances {
                    print_table_row(inst, show_state);
                }
                println!("({} sessions)", instances.len());
                total_sessions += instances.len();
            }
        }
    }

    println!("\n═══════════════════════════════════════");
    println!(
        "Total: {} sessions across {} profiles",
        total_sessions,
        profiles.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_tag_covers_the_three_states() {
        let live = Instance::new("live", "/repo");
        assert_eq!(state_tag(&live), "live");

        let mut archived = Instance::new("archived", "/repo");
        archived.archive();
        assert_eq!(state_tag(&archived), "archived");

        let mut trashed = Instance::new("trashed", "/repo");
        trashed.trash();
        assert_eq!(state_tag(&trashed), "trashed");
    }

    /// #3350: the whole point of the JSON change. A consumer keying on
    /// state needs the `state` string AND the timestamp to distinguish
    /// a trashed session from a genuinely failed one without a second
    /// `aoe session list-trash` shellout.
    #[test]
    fn session_json_exposes_state_and_trashed_at_for_a_trashed_row() {
        let mut inst = Instance::new("z", "/repo");
        inst.trash();
        let json = session_json(&inst, "p");
        assert_eq!(json.state, "trashed");
        assert!(json.trashed_at.is_some());
        assert!(json.archived_at.is_none());
    }

    /// Companion for the follow-up comment on #3350: archived sessions
    /// need the same treatment. The two states are semantically distinct
    /// and both must be observable from a single `aoe list --json` call.
    #[test]
    fn session_json_exposes_state_and_archived_at_for_an_archived_row() {
        let mut inst = Instance::new("z", "/repo");
        inst.archive();
        let json = session_json(&inst, "p");
        assert_eq!(json.state, "archived");
        assert!(json.archived_at.is_some());
        assert!(json.trashed_at.is_none());
    }

    /// The default `state = "live"` and both timestamp fields being
    /// `None` must not serialize any of the state-tracking keys as
    /// `null`: consumers depending on `serde_if_none` semantics see no
    /// difference from the pre-#3350 output. The `state` field is a
    /// small addition and always serialized, so a v1.14.1 consumer that
    /// parses JSON strictly will see one new key.
    #[test]
    fn session_json_omits_absent_timestamps_and_keeps_state_alive() {
        let inst = Instance::new("z", "/repo");
        let json = session_json(&inst, "p");
        assert_eq!(json.state, "live");
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(!serialized.contains("trashed_at"));
        assert!(!serialized.contains("archived_at"));
        assert!(serialized.contains("\"state\":\"live\""));
    }

    /// #3415: snooze and pin complete the four-timestamp state set the API
    /// has exposed since #1581. The table pins the whole contract: the
    /// snooze key follows the API's `is_snoozed()` gate (surfaced while
    /// active, dropped once expired even though the stale timestamp stays
    /// on disk), the pin key is a plain presence mirror, a plain row
    /// carries neither key, and neither key bends `state`, which stays the
    /// bucket tag.
    #[test]
    fn session_json_mirrors_the_api_snooze_and_pin_keys() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::minutes(15);
        let past = now - chrono::Duration::minutes(15);

        let mut snoozed = Instance::new("z", "/repo");
        snoozed.snoozed_until = Some(future);

        let mut expired = Instance::new("z", "/repo");
        expired.snoozed_until = Some(past);

        let mut pinned = Instance::new("z", "/repo");
        pinned.pinned_at = Some(now);

        // pin() and snooze() clear each other's marker, but peer store
        // writes bypass the mutators, so a row can carry both at once
        // and neither key may suppress the other.
        let mut both = Instance::new("z", "/repo");
        both.pinned_at = Some(now);
        both.snoozed_until = Some(future);

        // archive() clears a concurrent snooze through the mutators, but
        // snooze() leaves archived_at alone, so archiving a row and then
        // snoozing it persists the pair through ordinary CLI commands:
        // the keys must stay independent of the bucket tag.
        let mut sunk = Instance::new("z", "/repo");
        sunk.archived_at = Some(now);
        sunk.snoozed_until = Some(future);

        // snoozed then trashed through ordinary commands: trash()
        // preserves the sibling timestamps, so a triaged row must still
        // report its deadline from the trash.
        let mut trashed_snoozed = Instance::new("z", "/repo");
        trashed_snoozed.snooze(30);
        trashed_snoozed.trash();

        // pinned for the web sidebar, then trashed the same way.
        let mut trashed_pinned = Instance::new("z", "/repo");
        trashed_pinned.pin();
        trashed_pinned.trash();

        // pin() clears archived_at through the mutators, but peer store
        // writes bypass them: an archived row can still carry a pin.
        let mut archived_pinned = Instance::new("z", "/repo");
        archived_pinned.archived_at = Some(now);
        archived_pinned.pinned_at = Some(now);

        let plain = Instance::new("z", "/repo");

        let cases = [
            ("active snooze", &snoozed, true, false, "live"),
            ("expired snooze", &expired, false, false, "live"),
            ("pinned", &pinned, false, true, "live"),
            ("plain row", &plain, false, false, "live"),
            ("snoozed and archived", &sunk, true, false, "archived"),
            ("pinned and snoozed", &both, true, true, "live"),
            (
                "trashed and snoozed",
                &trashed_snoozed,
                true,
                false,
                "trashed",
            ),
            (
                "trashed and pinned",
                &trashed_pinned,
                false,
                true,
                "trashed",
            ),
            (
                "pinned and archived",
                &archived_pinned,
                false,
                true,
                "archived",
            ),
        ];
        for (label, inst, want_snooze, want_pin, want_state) in cases {
            let value = serde_json::to_value(session_json(inst, "p")).unwrap();
            assert_eq!(
                value.get("snoozed_until").is_some(),
                want_snooze,
                "{label}: {value}"
            );
            assert_eq!(
                value.get("pinned_at").is_some(),
                want_pin,
                "{label}: {value}"
            );
            assert_eq!(value["state"].as_str(), Some(want_state), "{label}");
        }

        // Value fidelity on the one row whose exact deadline we set:
        // presence alone would accept a regression emitting any instant.
        let active = serde_json::to_value(session_json(&snoozed, "p")).unwrap();
        assert_eq!(
            active["snoozed_until"],
            serde_json::to_value(future).unwrap()
        );
    }

    /// Backward-compat: `aoe list` (no `--state`) shows what it always
    /// showed, i.e. every persisted session. Guards against a future
    /// well-meaning refactor flipping the default to `live` and quietly
    /// dropping trashed rows for anyone scripted against today's output.
    #[test]
    fn default_state_is_all_for_backward_compat() {
        let default: SessionScope = StateFilter::All.into();
        assert!(matches!(default, SessionScope::All));

        let live_inst = Instance::new("l", "/r");
        let mut trashed = Instance::new("t", "/r");
        trashed.trash();
        let mut archived = Instance::new("a", "/r");
        archived.archive();
        for inst in [&live_inst, &trashed, &archived] {
            assert!(
                SessionScope::matches(Some(default), inst),
                "default state=all must list every session"
            );
        }
    }

    /// Argument-level pin for the #148 auto-mint guard: `list --all`
    /// enumerates every profile and never consumes `--profile`, so a stale
    /// or mistyped `-p` must not block it; the single-profile form goes
    /// through `Storage::open_unwatched`, which refuses an unknown name.
    mod profile_guard {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        use serial_test::serial;

        fn dispatch_argv(argv: &[&str]) -> (String, super::super::ListArgs) {
            let cli = Cli::try_parse_from(argv).expect("argv parses");
            let profile = cli.profile.unwrap_or_default();
            match cli.command {
                Some(Commands::List(args)) => (profile, args),
                _ => panic!("expected a list invocation"),
            }
        }

        #[tokio::test]
        #[serial]
        async fn list_all_ignores_an_unknown_profile() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();

            let (profile, args) =
                dispatch_argv(&["aoe", "list", "--all", "--json", "-p", "ghost-profile"]);
            super::super::run(&profile, args)
                .await
                .expect("`list --all` never consults --profile");
            assert!(!profiles.join("ghost-profile").exists());
        }

        #[tokio::test]
        #[serial]
        async fn list_single_profile_refuses_an_unknown_profile() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();

            let (profile, args) = dispatch_argv(&["aoe", "list", "--json", "-p", "ghost-profile"]);
            let msg = super::super::run(&profile, args)
                .await
                .expect_err("unknown profile must refuse `list`")
                .to_string();
            assert!(msg.contains("does not exist"), "got: {msg}");
            assert!(!profiles.join("ghost-profile").exists());
        }
    }
}
