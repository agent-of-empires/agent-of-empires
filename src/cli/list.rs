//! `agent-of-empires list` command implementation

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::session::projection::SessionProjection;
use crate::session::{Instance, SessionBucket, SessionScope, Storage};

pub(crate) const TABLE_COL_TITLE: usize = 20;
pub(crate) const TABLE_COL_GROUP: usize = 15;
pub(crate) const TABLE_COL_PATH: usize = 40;
pub(crate) const TABLE_COL_ID_DISPLAY: usize = 12;
pub(crate) const TABLE_COL_STATE: usize = 9;

/// The `aoe list --state=` vocabulary. Mirrors the REST API's
/// `SessionScope` (`GET /api/sessions?state=`) so the two vocabularies
/// share one source of truth (#3350). Kept as a clap-facing enum here
/// rather than deriving `ValueEnum` on the wire type: the API rejects
/// unrecognized values via serde with a JSON 400, while clap wants its
/// own `PossibleValue` list for `--help` and `--state=?` errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum StateFilter {
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
    pub(crate) json: bool,

    /// List sessions from all profiles
    #[arg(long)]
    pub(crate) all: bool,

    /// Filter by session state. Defaults to `all`, every persisted session,
    /// which is what `aoe list` has always shown. Pass `--state=live` to skip
    /// trashed and archived rows; the vocabulary matches the REST API's
    /// `GET /api/sessions?state=`.
    #[arg(long, value_enum, default_value = "all")]
    pub(crate) state: StateFilter,
}

/// `aoe list --json`, the shape the command has always emitted. The renderer
/// fills this same struct from a snapshot rather than keeping a second copy,
/// so the two transports cannot drift on a key, a spelling or a member order.
#[derive(Serialize)]
pub(crate) struct SessionJson {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) path: String,
    pub(crate) group: String,
    pub(crate) tool: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) command: String,
    pub(crate) profile: String,
    pub(crate) state: &'static str,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) archived_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) workspace_repos: Vec<WorkspaceRepoJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<WorktreeJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_session_id: Option<String>,
    /// Expected scope unit glob (`aoe-session-<id>-*.scope`) for live sessions with a process; not proof a unit exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) systemd_unit: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceRepoJson {
    pub(crate) name: String,
    pub(crate) source_path: String,
    pub(crate) branch: String,
}

#[derive(Serialize)]
pub(crate) struct WorktreeJson {
    pub(crate) branch: String,
    pub(crate) main_repo_path: String,
    pub(crate) managed_by_aoe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_branch: Option<String>,
}

/// Whether session scopes are expected, decided without calling systemd, so
/// `aoe list` never waits on it.
pub(crate) fn systemd_scopes_active() -> bool {
    crate::process::systemd::scopes_expected(crate::session::Config::load_or_warn().systemd.enabled)
}

fn session_json(inst: &Instance, profile: &str, systemd_active: bool) -> SessionJson {
    let row = SessionProjection::new(inst, profile);
    let live = matches!(row.bucket, SessionBucket::Active);
    SessionJson {
        id: row.id.to_string(),
        title: row.title.to_string(),
        path: row.project_path.to_string(),
        group: row.group_path.to_string(),
        tool: row.tool.to_string(),
        command: row.command.to_string(),
        profile: row.profile.to_string(),
        state: row.bucket.as_str(),
        created_at: row.created_at,
        trashed_at: row.trashed_at,
        archived_at: row.archived_at,
        snoozed_until: row.active_snoozed_until,
        pinned_at: row.pinned_at,
        workspace_repos: row
            .workspace_repos
            .iter()
            .map(|r| WorkspaceRepoJson {
                name: r.name.clone(),
                source_path: r.source_path.clone(),
                branch: r.branch.clone(),
            })
            .collect(),
        worktree: row.worktree.map(|w| WorktreeJson {
            branch: w.branch.clone(),
            main_repo_path: w.main_repo_path.clone(),
            managed_by_aoe: w.managed_by_aoe,
            base_branch: w.base_branch.clone(),
        }),
        parent_session_id: row.parent_session_id.map(str::to_string),
        systemd_unit: (systemd_active && live && crate::process::systemd::has_process(inst.status))
            .then(|| crate::process::systemd::unit_glob(row.id)),
    }
}

/// The refusal an all-profiles listing gives, on either transport, when one
/// profile's data cannot be read. The local command owns the wording, and the
/// served renderer builds the identical string from here, so an operator reads
/// one sentence whichever transport answered. Carried back as an `anyhow`
/// error, which is how `main` prints it: it adds the `Error: ` prefix and the
/// newline, and exits 1.
pub(crate) fn unreadable_profile_message(profile: &str) -> String {
    format!(
        "Profile '{profile}' could not be read, so the answer would be incomplete.\n\
         Fix or remove that profile, then run the command again."
    )
}

/// The two header lines, the way this table has always printed them: the
/// heading row, then a separator as wide as the columns plus their gaps. The
/// renderer emits these exact bytes, so a served listing is the listing.
pub(crate) fn table_header(show_state: bool) -> String {
    let heading = if show_state {
        format!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {:<width_state$} ID",
            "TITLE",
            "GROUP",
            "PATH",
            "STATE",
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH,
            width_state = TABLE_COL_STATE,
        )
    } else {
        format!(
            "{:<width_title$} {:<width_group$} {:<width_path$} ID",
            "TITLE",
            "GROUP",
            "PATH",
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH
        )
    };
    let separator = if show_state {
        TABLE_COL_TITLE
            + TABLE_COL_GROUP
            + TABLE_COL_PATH
            + TABLE_COL_STATE
            + TABLE_COL_ID_DISPLAY
            + 6
    } else {
        TABLE_COL_TITLE + TABLE_COL_GROUP + TABLE_COL_PATH + TABLE_COL_ID_DISPLAY + 5
    };
    format!("{heading}\n{}\n", "-".repeat(separator))
}

/// The title cell, indented by its depth in the parent/child nesting.
pub(crate) fn table_title(title: &str, depth: usize) -> String {
    match depth {
        0 => title.to_string(),
        _ => format!("{}└ {}", "  ".repeat(depth - 1), title),
    }
}

/// One table row. The id column is truncated to the display width with no
/// ellipsis, and the other three to theirs with one; the widths are the
/// constants above, so the row and the separator cannot disagree.
pub(crate) fn table_row(
    title: &str,
    group: &str,
    path: &str,
    state: Option<&str>,
    id: &str,
) -> String {
    let title = super::truncate(title, TABLE_COL_TITLE);
    let group = super::truncate(group, TABLE_COL_GROUP);
    let path = super::truncate(path, TABLE_COL_PATH);
    let id_display = super::truncate_id(id, TABLE_COL_ID_DISPLAY);
    match state {
        Some(state) => format!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {:<width_state$} {}\n",
            title,
            group,
            path,
            state,
            id_display,
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH,
            width_state = TABLE_COL_STATE,
        ),
        None => format!(
            "{:<width_title$} {:<width_group$} {:<width_path$} {}\n",
            title,
            group,
            path,
            id_display,
            width_title = TABLE_COL_TITLE,
            width_group = TABLE_COL_GROUP,
            width_path = TABLE_COL_PATH
        ),
    }
}

/// The listing order: every row whose listed parent is absent or itself, then
/// their children beneath them, then anything the walk did not reach. The
/// renderer runs the same walk over the snapshot, so a served listing is
/// ordered exactly as the local one is.
pub(crate) fn nest_order<'a, T>(
    rows: &'a [T],
    id: impl Fn(&'a T) -> &'a str,
    parent: impl Fn(&'a T) -> Option<&'a str>,
) -> Vec<(usize, usize)> {
    fn place<'a, T>(
        index: usize,
        depth: usize,
        rows: &'a [T],
        id: &impl Fn(&'a T) -> &'a str,
        parent: &impl Fn(&'a T) -> Option<&'a str>,
        placed: &mut std::collections::HashSet<usize>,
        ordered: &mut Vec<(usize, usize)>,
    ) {
        if !placed.insert(index) {
            return;
        }
        ordered.push((index, depth));
        let name = id(&rows[index]);
        for (candidate, row) in rows.iter().enumerate() {
            if parent(row) == Some(name) {
                place(candidate, depth + 1, rows, id, parent, placed, ordered);
            }
        }
    }

    let listed: std::collections::HashSet<&str> = rows.iter().map(&id).collect();
    let mut placed = std::collections::HashSet::new();
    let mut ordered = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let under_listed_parent = parent(row).is_some_and(|p| p != id(row) && listed.contains(p));
        if !under_listed_parent {
            place(index, 0, rows, &id, &parent, &mut placed, &mut ordered);
        }
    }
    for index in 0..rows.len() {
        place(index, 0, rows, &id, &parent, &mut placed, &mut ordered);
    }
    ordered
}

fn nest_children<'a>(instances: &[&'a Instance]) -> Vec<(&'a Instance, usize)> {
    nest_order(
        instances,
        |inst| inst.id.as_str(),
        |inst| inst.parent_session_id.as_deref(),
    )
    .into_iter()
    .map(|(index, depth)| (instances[index], depth))
    .collect()
}

fn print_table_row(inst: &Instance, depth: usize, show_state: bool) {
    print!(
        "{}",
        table_row(
            &table_title(&inst.title, depth),
            &inst.group_path,
            &inst.project_path,
            show_state.then(|| inst.effective_bucket().as_str()),
            &inst.id,
        )
    );
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

    let storage = Storage::open_reference(profile)?;
    let (all_instances, _) = storage.load_with_groups()?;
    let instances: Vec<Instance> = all_instances
        .into_iter()
        .filter(|inst| {
            SessionScope::matches(Some(scope), (inst).is_archived(), (inst).is_trashed())
        })
        .collect();

    // `--json` is answered before the empty-listing message: an empty result is
    // `[]`, never human prose on stdout, matching `aoe ps --json` and `aoe group
    // list --json`. `--state=live` makes this reachable with sessions present
    // (a profile whose rows are all trashed), which is precisely the scripted
    // consumer #3350 is about.
    if args.json {
        let systemd_active = systemd_scopes_active();
        let sessions: Vec<SessionJson> = instances
            .iter()
            .map(|inst| session_json(inst, storage.profile(), systemd_active))
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
    print!("{}", table_header(show_state));
    let listed: Vec<&Instance> = instances.iter().collect();
    for (inst, depth) in nest_children(&listed) {
        print_table_row(inst, depth, show_state);
    }
    println!("\nTotal: {} sessions", instances.len());

    crate::update::print_update_notice().await;

    Ok(())
}

/// Read one profile's rows, naming the failure instead of dropping the profile.
fn read_profile(profile_name: &str) -> Result<Vec<Instance>, String> {
    let storage = Storage::open_unwatched(profile_name).map_err(|error| error.to_string())?;
    storage
        .load_with_groups()
        .map(|(instances, _)| instances)
        .map_err(|error| error.to_string())
}

/// The `--json` listing across every profile, plus a per-profile note for each
/// one that could not be read. An unreadable profile must never be silently
/// skipped: the caller has no other way to tell a partial list from a complete
/// one.
fn collect_profile_sessions(
    profiles: &[String],
    scope: SessionScope,
) -> (Vec<SessionJson>, Vec<String>) {
    let systemd_active = systemd_scopes_active();
    let mut all_sessions: Vec<SessionJson> = Vec::new();
    let mut failures = Vec::new();
    for profile_name in profiles {
        match read_profile(profile_name) {
            Ok(instances) => {
                for inst in &instances {
                    if !SessionScope::matches(Some(scope), inst.is_archived(), inst.is_trashed()) {
                        continue;
                    }
                    all_sessions.push(session_json(inst, profile_name, systemd_active));
                }
            }
            Err(error) => failures.push(format!("profile '{profile_name}': {error}")),
        }
    }
    (all_sessions, failures)
}

/// Report each unreadable profile on stderr and fail the command, leaving
/// stdout untouched.
fn report_profile_failures(failures: Vec<String>) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    for failure in &failures {
        eprintln!("warning: {failure}");
    }
    anyhow::bail!(
        "{} profile(s) could not be read; the listing is incomplete",
        failures.len()
    )
}

async fn run_all_profiles(json: bool, scope: SessionScope) -> Result<()> {
    let profiles = crate::session::list_profiles()?;

    if profiles.is_empty() {
        println!("No profiles found.");
        return Ok(());
    }

    if json {
        let (all_sessions, failures) = collect_profile_sessions(&profiles, scope);
        super::output::print_json(&all_sessions)?;
        // stdout stays pure JSON: a profile that could not be read is reported
        // on stderr and turns the exit code non-zero, so a consumer never
        // mistakes a partial listing for a complete one.
        return report_profile_failures(failures);
    }

    let show_state = table_shows_state(scope);
    let mut total_sessions = 0;
    for profile_name in &profiles {
        let (all_instances, failure) = match read_profile(profile_name) {
            Ok(instances) => (instances, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        if let Some(error) = &failure {
            eprintln!("warning: profile '{profile_name}': {error}");
        }
        let instances: Vec<&Instance> = all_instances
            .iter()
            .filter(|inst| {
                SessionScope::matches(Some(scope), (inst).is_archived(), (inst).is_trashed())
            })
            .collect();
        if instances.is_empty() {
            continue;
        }

        println!("\n═══ Profile: {} ═══\n", profile_name);
        print!("{}", table_header(show_state));
        for (inst, depth) in nest_children(&instances) {
            print_table_row(inst, depth, show_state);
        }
        println!("({} sessions)", instances.len());
        total_sessions += instances.len();
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

    /// The CLI, runtime wire and REST rows share one derivation, so they agree
    /// on the instant of every shared timestamp and on the worktree facts.
    #[test]
    fn projections_agree_on_shared_facts() {
        use crate::cli::runtime_read::dto::SessionRead;
        use crate::daemon::SessionResponse;
        use crate::session::{WorkspaceInfo, WorkspaceRepo, WorktreeInfo};

        let now = chrono::Utc::now();
        let instant = |value: &str| chrono::DateTime::parse_from_rfc3339(value).unwrap();
        let cases: [(&str, fn(&mut Instance)); 5] = [
            ("live", |_| {}),
            ("archived", |i| i.archived_at = Some(chrono::Utc::now())),
            ("trashed", |i| i.trash()),
            ("snoozed", |i| {
                i.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::minutes(5))
            }),
            ("expired snooze", |i| {
                i.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::minutes(5))
            }),
        ];
        for (label, mutate) in cases {
            let mut inst = Instance::new("t", "/repo");
            inst.pinned_at = Some(now);
            inst.worktree_info = Some(WorktreeInfo {
                branch: "feat".into(),
                main_repo_path: "/main".into(),
                managed_by_aoe: true,
                created_at: now,
                base_branch: Some("base".into()),
            });
            inst.workspace_info = Some(WorkspaceInfo {
                branch: "feat".into(),
                workspace_dir: "/ws".into(),
                created_at: now,
                cleanup_on_delete: true,
                repos: vec![WorkspaceRepo {
                    name: "a".into(),
                    source_path: "/a".into(),
                    branch: "feat".into(),
                    worktree_path: "/ws/a".into(),
                    main_repo_path: "/a".into(),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                }],
            });
            mutate(&mut inst);

            let cli =
                serde_json::to_value(session_json(&inst, &inst.source_profile, false)).unwrap();
            let wire = SessionRead::from_instance(&inst);
            let rest = SessionResponse::from_instance(&inst, false);

            assert_eq!(wire.state.as_str(), cli["state"], "{label}");
            let cli_time = |key: &str| cli[key].as_str().map(instant);
            let wire_time = |v: &Option<String>| v.as_deref().map(instant);
            let rest_time = |v: &Option<String>| v.as_deref().map(instant);
            assert_eq!(
                cli_time("snoozed_until"),
                wire_time(&wire.active_snoozed_until),
                "{label}"
            );
            assert_eq!(
                cli_time("snoozed_until"),
                rest_time(&rest.snoozed_until),
                "{label}"
            );
            assert_eq!(
                cli_time("archived_at"),
                wire_time(&wire.archived_at),
                "{label}"
            );
            assert_eq!(
                cli_time("archived_at"),
                rest_time(&rest.archived_at),
                "{label}"
            );
            assert_eq!(
                cli_time("trashed_at"),
                wire_time(&wire.trashed_at),
                "{label}"
            );
            assert_eq!(
                cli_time("trashed_at"),
                rest_time(&rest.trashed_at),
                "{label}"
            );
            assert_eq!(cli_time("pinned_at"), wire_time(&wire.pinned_at), "{label}");
            assert_eq!(cli_time("pinned_at"), rest_time(&rest.pinned_at), "{label}");
            assert_eq!(cli["worktree"]["branch"], "feat", "{label}");
            assert_eq!(wire.worktree.as_ref().unwrap().branch, "feat", "{label}");
            assert_eq!(rest.branch.as_deref(), Some("feat"), "{label}");
            assert_eq!(rest.base_branch.as_deref(), Some("base"), "{label}");
            assert!(
                wire.has_managed_worktree && rest.has_managed_worktree,
                "{label}"
            );
            assert_eq!(cli["workspace_repos"][0]["name"], "a", "{label}");
            assert_eq!(wire.workspace_repos[0].name, "a", "{label}");
            assert_eq!(rest.workspace_repos[0].name, "a", "{label}");
        }
    }

    /// An optional field the row has no value for is left out, as every other
    /// optional field in the struct is. A `null` here would make a consumer
    /// carry a branch that does not exist.
    #[test]
    fn a_worktree_without_a_base_branch_emits_no_key() {
        let row = |base_branch| WorktreeJson {
            branch: "feature".into(),
            main_repo_path: "/repo".into(),
            managed_by_aoe: true,
            base_branch,
        };
        let keys = |value: Option<String>| {
            let text = serde_json::to_string(&row(value)).expect("encodes");
            serde_json::from_str::<serde_json::Value>(&text).expect("is JSON")
        };
        assert!(
            keys(None).get("base_branch").is_none(),
            "an absent base branch is omitted, not null"
        );
        assert_eq!(
            keys(Some("main".into()))["base_branch"],
            serde_json::json!("main")
        );
    }

    /// #3472: `aoe add -P` children render under their parent in the table.
    #[test]
    fn nest_children_lists_each_child_under_its_listed_parent() {
        let row = |title: &str, parent: Option<&str>| {
            let mut inst = Instance::new(title, "/repo");
            inst.parent_session_id = parent.map(str::to_string);
            inst
        };
        let root = row("root", None);
        let child = row("child", Some(&root.id));
        let grandchild = row("grandchild", Some(&child.id));
        let orphan = row("orphan", Some("not-listed"));
        let other = row("other", None);
        let mut loop_a = row("loop-a", None);
        let loop_b = row("loop-b", Some(&loop_a.id));
        loop_a.parent_session_id = Some(loop_b.id.clone());

        let listed = [
            &grandchild,
            &root,
            &orphan,
            &child,
            &other,
            &loop_a,
            &loop_b,
        ];
        let titles: Vec<String> = nest_children(&listed)
            .into_iter()
            .map(|(inst, depth)| table_title(&inst.title, depth))
            .collect();
        assert_eq!(
            titles,
            [
                "root",
                "└ child",
                "  └ grandchild",
                "orphan",
                "other",
                "loop-a",
                "└ loop-b",
            ]
        );

        assert_eq!(
            session_json(&child, "p", false)
                .parent_session_id
                .as_deref(),
            Some(root.id.as_str())
        );
    }

    #[test]
    fn session_json_reports_state_and_only_the_timestamps_that_apply() {
        let plain = Instance::new("z", "/repo");
        let json = session_json(&plain, "p", false);
        assert_eq!(json.state, "live");
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(!serialized.contains("trashed_at"));
        assert!(!serialized.contains("archived_at"));
        assert!(serialized.contains("\"state\":\"live\""));

        let mut archived = Instance::new("z", "/repo");
        archived.archive();
        let json = session_json(&archived, "p", false);
        assert_eq!(json.state, "archived");
        assert!(json.archived_at.is_some());
        assert!(json.trashed_at.is_none());

        let mut trashed = Instance::new("z", "/repo");
        trashed.trash();
        let json = session_json(&trashed, "p", false);
        assert_eq!(json.state, "trashed");
        assert!(json.trashed_at.is_some());
        assert!(json.archived_at.is_none());
    }

    /// An unreadable profile must surface, not vanish: a silent skip makes a
    /// partial listing indistinguishable from a complete one.
    #[test]
    fn an_unreadable_profile_is_reported_rather_than_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        // A regular file where the profile directory belongs: the name still
        // exists, so nothing creates it, and opening it fails deterministically.
        let profiles = temp.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join("broken"), b"x").unwrap();
        let (rows, failures) =
            collect_profile_sessions(&["broken".to_string()], SessionScope::Live);
        assert!(rows.is_empty());
        assert_eq!(failures.len(), 1, "got: {failures:?}");
        assert!(
            failures[0].contains("broken"),
            "the failure must name the profile: {}",
            failures[0]
        );
        assert!(
            report_profile_failures(failures)
                .unwrap_err()
                .to_string()
                .contains("incomplete"),
            "an incomplete listing must not exit zero"
        );
    }

    #[test]
    fn session_json_mirrors_the_api_snooze_and_pin_keys() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::minutes(15);
        let past = now - chrono::Duration::minutes(15);
        let row = |f: &dyn Fn(&mut Instance)| {
            let mut inst = Instance::new("z", "/repo");
            f(&mut inst);
            inst
        };
        let check = |label: &str, f: &dyn Fn(&mut Instance), snooze: bool, pin: bool, state| {
            let value = serde_json::to_value(session_json(&row(f), "p", false)).unwrap();
            let seen = (
                value.get("snoozed_until").is_some(),
                value.get("pinned_at").is_some(),
                value["state"].as_str(),
            );
            assert_eq!(seen, (snooze, pin, Some(state)), "{label}: {value}");
        };

        check("plain row", &|_| {}, false, false, "live");
        check(
            "active snooze",
            &|i| i.snoozed_until = Some(future),
            true,
            false,
            "live",
        );
        check(
            "expired snooze",
            &|i| i.snoozed_until = Some(past),
            false,
            false,
            "live",
        );
        check("pinned", &|i| i.pinned_at = Some(now), false, true, "live");
        check(
            "snoozed and archived",
            &|i| {
                i.archived_at = Some(now);
                i.snoozed_until = Some(future);
            },
            true,
            false,
            "archived",
        );
        check(
            "pinned and snoozed",
            &|i| {
                i.pinned_at = Some(now);
                i.snoozed_until = Some(future);
            },
            true,
            true,
            "live",
        );
        check(
            "trashed and snoozed",
            &|i| {
                i.snooze(30);
                i.trash();
            },
            true,
            false,
            "trashed",
        );
        check(
            "trashed and pinned",
            &|i| {
                i.pin();
                i.trash();
            },
            false,
            true,
            "trashed",
        );
        check(
            "pinned and archived",
            &|i| {
                i.archived_at = Some(now);
                i.pinned_at = Some(now);
            },
            false,
            true,
            "archived",
        );

        let active = serde_json::to_value(session_json(
            &row(&|i| i.snoozed_until = Some(future)),
            "p",
            false,
        ))
        .unwrap();
        assert_eq!(
            active["snoozed_until"],
            serde_json::to_value(future).unwrap()
        );
    }

    #[test]
    fn systemd_unit_only_for_live_sessions_when_scopes_are_active() {
        let live = Instance::new("example", "/repo");
        let mut trashed = Instance::new("example", "/repo");
        trashed.trash();
        let mut archived = Instance::new("example", "/repo");
        archived.archive();
        let mut stopped = Instance::new("example", "/repo");
        stopped.status = crate::session::Status::Stopped;
        assert_eq!(session_json(&live, "p", false).systemd_unit, None);
        assert_eq!(
            session_json(&live, "p", true).systemd_unit,
            Some(format!("aoe-session-{}-*.scope", live.id))
        );
        for no_scope in [&trashed, &archived, &stopped] {
            assert_eq!(session_json(no_scope, "p", true).systemd_unit, None);
        }
    }

    /// `list --all` never consumes `--profile`; the single-profile form goes
    /// through `Storage::open_unwatched`, which refuses an unknown name (#148).
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
