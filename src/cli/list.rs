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

pub(super) fn state_tag(inst: &Instance) -> &'static str {
    match inst.effective_bucket() {
        SessionBucket::Trashed => "trashed",
        SessionBucket::Archived => "archived",
        SessionBucket::Active => "live",
    }
}

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
    state: &'static str,
    created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    workspace_repos: Vec<WorkspaceRepoJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<WorktreeJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
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
        parent_session_id: inst.parent_session_id.clone(),
    }
}

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

fn nest_children<'a>(instances: &[&'a Instance]) -> Vec<(&'a Instance, usize)> {
    fn place<'a>(
        inst: &'a Instance,
        depth: usize,
        instances: &[&'a Instance],
        placed: &mut std::collections::HashSet<&'a str>,
        ordered: &mut Vec<(&'a Instance, usize)>,
    ) {
        if !placed.insert(inst.id.as_str()) {
            return;
        }
        ordered.push((inst, depth));
        for child in instances
            .iter()
            .filter(|c| c.parent_session_id.as_deref() == Some(inst.id.as_str()))
        {
            place(child, depth + 1, instances, placed, ordered);
        }
    }

    let listed: std::collections::HashSet<&str> = instances.iter().map(|i| i.id.as_str()).collect();
    let mut placed = std::collections::HashSet::new();
    let mut ordered = Vec::with_capacity(instances.len());
    for inst in instances {
        let under_listed_parent = inst
            .parent_session_id
            .as_deref()
            .is_some_and(|p| p != inst.id && listed.contains(p));
        if !under_listed_parent {
            place(inst, 0, instances, &mut placed, &mut ordered);
        }
    }
    for inst in instances {
        place(inst, 0, instances, &mut placed, &mut ordered);
    }
    ordered
}

fn table_title(inst: &Instance, depth: usize) -> String {
    match depth {
        0 => inst.title.clone(),
        _ => format!("{}└ {}", "  ".repeat(depth - 1), inst.title),
    }
}

fn print_table_row(inst: &Instance, depth: usize, show_state: bool) {
    let title = super::truncate(&table_title(inst, depth), TABLE_COL_TITLE);
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
    let listed: Vec<&Instance> = instances.iter().collect();
    for (inst, depth) in nest_children(&listed) {
        print_table_row(inst, depth, show_state);
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
                for (inst, depth) in nest_children(&instances) {
                    print_table_row(inst, depth, show_state);
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
            .map(|(inst, depth)| table_title(inst, depth))
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
            session_json(&child, "p").parent_session_id.as_deref(),
            Some(root.id.as_str())
        );
    }

    #[test]
    fn session_json_reports_state_and_only_the_timestamps_that_apply() {
        let plain = Instance::new("z", "/repo");
        assert_eq!(state_tag(&plain), "live");
        let json = session_json(&plain, "p");
        assert_eq!(json.state, "live");
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(!serialized.contains("trashed_at"));
        assert!(!serialized.contains("archived_at"));
        assert!(serialized.contains("\"state\":\"live\""));

        let mut archived = Instance::new("z", "/repo");
        archived.archive();
        assert_eq!(state_tag(&archived), "archived");
        let json = session_json(&archived, "p");
        assert_eq!(json.state, "archived");
        assert!(json.archived_at.is_some());
        assert!(json.trashed_at.is_none());

        let mut trashed = Instance::new("z", "/repo");
        trashed.trash();
        assert_eq!(state_tag(&trashed), "trashed");
        let json = session_json(&trashed, "p");
        assert_eq!(json.state, "trashed");
        assert!(json.trashed_at.is_some());
        assert!(json.archived_at.is_none());
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
            let value = serde_json::to_value(session_json(&row(f), "p")).unwrap();
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

        let active =
            serde_json::to_value(session_json(&row(&|i| i.snoozed_until = Some(future)), "p"))
                .unwrap();
        assert_eq!(
            active["snoozed_until"],
            serde_json::to_value(future).unwrap()
        );
    }

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
