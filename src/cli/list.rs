//! `agent-of-empires list` command implementation

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session::{Instance, Storage};

const TABLE_COL_TITLE: usize = 20;
const TABLE_COL_GROUP: usize = 15;
const TABLE_COL_PATH: usize = 40;
const TABLE_COL_ID_DISPLAY: usize = 12;

#[derive(Args)]
pub struct ListArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// List sessions from all profiles
    #[arg(long)]
    all: bool,
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
    created_at: chrono::DateTime<chrono::Utc>,
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
        created_at: inst.created_at,
        workspace_repos: workspace_repos_for(inst),
        worktree: worktree_for(inst),
    }
}

fn workspace_repos_for(inst: &Instance) -> Vec<WorkspaceRepoJson> {
    inst.workspace_info
        .as_ref()
        .map(|w| {
            w.repos
                .iter()
                .map(|r| WorkspaceRepoJson {
                    name: r.name.clone(),
                    source_path: r.source_path.clone(),
                    branch: r.branch.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn print_table_header() {
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
        "-".repeat(TABLE_COL_TITLE + TABLE_COL_GROUP + TABLE_COL_PATH + TABLE_COL_ID_DISPLAY + 5)
    );
}

fn print_table_row(inst: &Instance) {
    let title = super::truncate(&inst.title, TABLE_COL_TITLE);
    let group = super::truncate(&inst.group_path, TABLE_COL_GROUP);
    let path = super::truncate(&inst.project_path, TABLE_COL_PATH);
    let id_display = super::truncate_id(&inst.id, TABLE_COL_ID_DISPLAY);
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

#[tracing::instrument(target = "cli.list", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: ListArgs) -> Result<()> {
    if args.all {
        return run_all_profiles(args.json).await;
    }

    let storage = Storage::new_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    if instances.is_empty() {
        println!("No sessions found in profile '{}'.", storage.profile());
        return Ok(());
    }

    if args.json {
        let sessions: Vec<SessionJson> = instances
            .iter()
            .map(|inst| session_json(inst, storage.profile()))
            .collect();
        super::output::print_json(&sessions)?;
        return Ok(());
    }

    println!("Profile: {}\n", storage.profile());
    print_table_header();
    for inst in &instances {
        print_table_row(inst);
    }
    println!("\nTotal: {} sessions", instances.len());

    crate::update::print_update_notice().await;

    Ok(())
}

async fn run_all_profiles(json: bool) -> Result<()> {
    let profiles = crate::session::list_profiles()?;

    if profiles.is_empty() {
        println!("No profiles found.");
        return Ok(());
    }

    if json {
        let mut all_sessions: Vec<SessionJson> = Vec::new();
        for profile_name in &profiles {
            if let Ok(storage) = Storage::new_unwatched(profile_name) {
                if let Ok((instances, _)) = storage.load_with_groups() {
                    for inst in &instances {
                        all_sessions.push(session_json(inst, profile_name));
                    }
                }
            }
        }
        super::output::print_json(&all_sessions)?;
        return Ok(());
    }

    let mut total_sessions = 0;
    for profile_name in &profiles {
        if let Ok(storage) = Storage::new_unwatched(profile_name) {
            if let Ok((instances, _)) = storage.load_with_groups() {
                if instances.is_empty() {
                    continue;
                }

                println!("\n═══ Profile: {} ═══\n", profile_name);
                print_table_header();
                for inst in &instances {
                    print_table_row(inst);
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
