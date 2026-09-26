//! `agent-of-empires list` command implementation

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::session::{Instance, SessionBucket, SessionScope, Storage};

pub(crate) const TABLE_COL_TITLE: usize = 20;
pub(crate) const TABLE_COL_GROUP: usize = 15;
pub(crate) const TABLE_COL_PATH: usize = 40;
pub(crate) const TABLE_COL_ID_DISPLAY: usize = 12;
pub(crate) const TABLE_COL_STATE: usize = 9;

/// The `aoe list --state=` vocabulary shared with the REST API.
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

    /// Filter by session state
    #[arg(long, value_enum, default_value = "all")]
    pub(crate) state: StateFilter,
}

pub(super) fn state_tag(inst: &Instance) -> &'static str {
    match inst.effective_bucket() {
        SessionBucket::Trashed => "trashed",
        SessionBucket::Archived => "archived",
        SessionBucket::Active => "live",
    }
}

/// One timestamp spelling for every human line that shows one: RFC 3339 in UTC
/// with the fractional part `AutoSi` keeps, zoned with `Z`. This is the same
/// spelling `DateTime<Utc>` serializes to, so the human lines and the `--json`
/// lines of one command agree — and so do the two transports.
pub(crate) fn display_timestamp(value: chrono::DateTime<chrono::Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

pub(super) fn active_snoozed_until(inst: &Instance) -> Option<chrono::DateTime<chrono::Utc>> {
    if inst.is_snoozed() {
        inst.snoozed_until
    } else {
        None
    }
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
    pub(crate) base_branch: Option<String>,
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
            show_state.then(|| state_tag(inst)),
            &inst.id,
        )
    );
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
    print!("{}", table_header(show_state));
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
                print!("{}", table_header(show_state));
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
