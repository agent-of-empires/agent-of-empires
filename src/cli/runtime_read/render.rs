use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use super::dto::{
    component_healthy, freshness_observed, profile_component_healthy, ProfileRead, ProjectRead,
    ProjectScope, SessionRead, SnapshotData, WireState, WireStatus,
};
use super::endpoint::{selected_profile_source, ProfileSource};
use super::{ReadFailure, ScopedCommand};
use crate::cli::group::GroupListArgs;
use crate::cli::list::{ListArgs, StateFilter};
use crate::cli::project::{ProjectListArgs, ScopeFilter};
use crate::cli::session::ShowArgs;

pub(crate) fn evaluate(
    command: &ScopedCommand<'_>,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<String, ReadFailure> {
    match command {
        ScopedCommand::List(args) => render_list(args, snapshot, source, local_home),
        ScopedCommand::Status(args) => render_status(args, snapshot, source, local_home),
        ScopedCommand::Show(args) => render_show(args, snapshot, source, local_home),
        ScopedCommand::ListTrash => render_trash(snapshot, source),
        ScopedCommand::GroupList(args) => render_groups(args, snapshot, source),
        ScopedCommand::Profile => render_profiles(snapshot),
        ScopedCommand::ProjectList(args) => render_projects(args, snapshot, source, local_home),
    }
}

fn selected_profile<'a>(
    snapshot: &'a SnapshotData,
    source: &super::endpoint::ReadRequestSource,
) -> Result<&'a str, ReadFailure> {
    let selected = match selected_profile_source(source) {
        ProfileSource::Explicit(value) => value,
        ProfileSource::Environment(value) => {
            let value = value
                .to_str()
                .ok_or_else(|| ReadFailure::post("profile_missing"))?;
            value
        }
        ProfileSource::Default => snapshot
            .default_profile
            .as_deref()
            .ok_or_else(|| ReadFailure::post("default_missing"))?,
    };
    if selected.is_empty() {
        return Err(ReadFailure::post("profile_missing"));
    }
    snapshot
        .profiles
        .iter()
        .find(|profile| profile.name == selected)
        .map(|profile| profile.name.as_str())
        .ok_or_else(|| ReadFailure::post("profile_missing"))
}

fn render_list(
    args: &ListArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<String, ReadFailure> {
    let state = args.state;
    if args.all {
        require_list_all_health(snapshot)?;
        if args.json {
            let mut rows = Vec::new();
            for profile in &snapshot.profiles {
                for session in sessions_for_profile(snapshot, &profile.name)
                    .filter(|session| matches_state(session.state, state))
                {
                    rows.push(ListJson::from_session(session, &profile.name));
                }
            }
            rows.sort_by(|left, right| left.id.cmp(&right.id));
            return json_lines(&rows);
        }
        if snapshot.profiles.is_empty() {
            return Ok("No profiles found.\n".into());
        }
        let show_state = state == StateFilter::All;
        let mut output = String::new();
        let mut total = 0usize;
        for profile in &snapshot.profiles {
            let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, &profile.name)
                .filter(|session| matches_state(session.state, state))
                .collect();
            output.push_str(&format!("\n═══ Profile: {} ═══\n\n", profile.name));
            push_table_header(&mut output, show_state);
            for (session, depth) in tree_order(&sessions) {
                push_table_row(&mut output, session, depth, show_state, local_home);
            }
            output.push_str(&format!("({} sessions)\n", sessions.len()));
            total += sessions.len();
        }
        output.push_str(&format!(
            "\n═══════════════════════════════════════\nTotal: {total} sessions across {} profiles\n",
            snapshot.profiles.len()
        ));
        return Ok(output);
    }

    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name)
        .filter(|session| matches_state(session.state, state))
        .collect();
    require_selected_profile_health(snapshot, profile, true)?;

    if args.json {
        let rows: Vec<ListJson> = sessions
            .iter()
            .map(|session| ListJson::from_session(session, profile_name))
            .collect();
        return json_lines(&rows);
    }
    if sessions.is_empty() {
        return Ok(format!("No sessions found in profile '{profile_name}'.\n"));
    }
    let show_state = state == StateFilter::All;
    let mut output = format!("Profile: {profile_name}\n\n");
    push_table_header(&mut output, show_state);
    for (session, depth) in tree_order(&sessions) {
        push_table_row(&mut output, session, depth, show_state, local_home);
    }
    Ok(output)
}

fn matches_state(state: WireState, filter: StateFilter) -> bool {
    match filter {
        StateFilter::All => true,
        StateFilter::Live => state == WireState::Live,
        StateFilter::Trashed => state == WireState::Trashed,
    }
}

fn push_table_header(output: &mut String, show_state: bool) {
    if show_state {
        output.push_str(&format!(
            "{} {} {} {} ID\n",
            pad("TITLE", 20),
            pad("GROUP", 15),
            pad("PATH", 40),
            pad("STATE", 9)
        ));
        output.push_str(&"-".repeat(90));
    } else {
        output.push_str(&format!(
            "{} {} {} ID\n",
            pad("TITLE", 20),
            pad("GROUP", 15),
            pad("PATH", 40)
        ));
        output.push_str(&"-".repeat(80));
    }
    output.push('\n');
}

fn push_table_row(
    output: &mut String,
    session: &SessionRead,
    depth: usize,
    show_state: bool,
    local_home: Option<&Path>,
) {
    let title = if depth == 0 {
        session.title.clone()
    } else {
        format!("{}└ {}", "  ".repeat(depth - 1), session.title)
    };
    let title = truncate(&title, 20);
    let group = truncate(&session.group_path, 15);
    let path = truncate(&collapse_home(&session.project_path, local_home), 40);
    let id = session.id.clone();
    if show_state {
        output.push_str(&format!(
            "{} {} {} {} {}\n",
            pad(&title, 20),
            pad(&group, 15),
            pad(&path, 40),
            pad(session.state.as_str(), 9),
            id
        ));
    } else {
        output.push_str(&format!(
            "{} {} {} {}\n",
            pad(&title, 20),
            pad(&group, 15),
            pad(&path, 40),
            id
        ));
    }
}

fn tree_order<'a>(sessions: &'a [&'a SessionRead]) -> Vec<(&'a SessionRead, usize)> {
    let ids: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<&SessionRead>> = HashMap::new();
    for session in sessions {
        let parent = session
            .parent_session_id
            .as_deref()
            .filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(session);
    }
    for rows in children.values_mut() {
        rows.sort_by(|left, right| left.id.cmp(&right.id));
    }
    let mut output = Vec::with_capacity(sessions.len());
    fn place<'a>(
        session: &'a SessionRead,
        depth: usize,
        children: &HashMap<Option<&'a str>, Vec<&'a SessionRead>>,
        output: &mut Vec<(&'a SessionRead, usize)>,
    ) {
        output.push((session, depth));
        if let Some(rows) = children.get(&Some(session.id.as_str())) {
            for child in rows {
                place(child, depth + 1, children, output);
            }
        }
    }
    if let Some(roots) = children.get(&None) {
        for root in roots {
            place(root, 0, &children, &mut output);
        }
    }
    output
}

#[derive(Serialize)]
struct ListJson {
    id: String,
    title: String,
    path: String,
    group: String,
    tool: String,
    command: String,
    profile: String,
    status: &'static str,
    state: &'static str,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<String>,
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
    base_branch: Option<String>,
}

impl ListJson {
    fn from_session(session: &SessionRead, profile: &str) -> Self {
        Self {
            id: session.id.clone(),
            title: session.title.clone(),
            path: session.project_path.clone(),
            group: session.group_path.clone(),
            tool: session.tool.clone(),
            command: session.command.clone(),
            profile: profile.to_string(),
            status: session.status.as_str(),
            state: session.state.as_str(),
            created_at: session.created_at.clone(),
            trashed_at: session.trashed_at.clone(),
            archived_at: session.archived_at.clone(),
            snoozed_until: session.active_snoozed_until.clone(),
            pinned_at: session.pinned_at.clone(),
            workspace_repos: session
                .workspace_repos
                .iter()
                .map(|repository| WorkspaceRepoJson {
                    name: repository.name.clone(),
                    source_path: repository.source_path.clone(),
                    branch: repository.branch.clone(),
                })
                .collect(),
            worktree: session.worktree.as_ref().map(|worktree| WorktreeJson {
                branch: worktree.branch.clone(),
                main_repo_path: worktree.main_repo_path.clone(),
                managed_by_aoe: worktree.managed_by_aoe,
                base_branch: worktree.base_branch.clone(),
            }),
            parent_session_id: session.parent_session_id.clone(),
        }
    }
}

fn render_status(
    args: &crate::cli::status::StatusArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<String, ReadFailure> {
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name).collect();
    require_selected_profile_health(snapshot, profile, true)?;
    if !freshness_observed(&snapshot.status_freshness) {
        return Err(ReadFailure::post("freshness_unavailable"));
    }
    let counts = StatusCounts::new(&sessions);
    if args.json {
        return Ok(format!(
            "{{\"waiting\":{},\"running\":{},\"idle\":{},\"stopped\":{},\"error\":{},\"total\":{}}}\n",
            counts.waiting,
            counts.running,
            counts.idle,
            counts.stopped,
            counts.error,
            counts.total
        ));
    }
    if args.quiet {
        return Ok(format!("{}\n", counts.waiting));
    }
    if sessions.is_empty() {
        return Ok(format!("No sessions in profile '{profile_name}'.\n"));
    }
    if args.verbose {
        let mut output = String::new();
        for (label, symbol, status) in [
            ("WAITING", "⠃", WireStatus::Waiting),
            ("RUNNING", "⠋", WireStatus::Running),
            ("IDLE", "⠒", WireStatus::Idle),
            ("STOPPED", "◼", WireStatus::Stopped),
            ("ERROR", "✕", WireStatus::Error),
        ] {
            let rows: Vec<&&SessionRead> = sessions
                .iter()
                .filter(|session| session.status == status)
                .collect();
            if rows.is_empty() {
                continue;
            }
            output.push_str(&format!("{label} ({}):\n", rows.len()));
            for session in rows {
                let title = truncate(&session.title, 16);
                let tool = truncate(&session.tool, 10);
                output.push_str(&format!(
                    "  {} {} {} {}\n",
                    symbol,
                    pad(&title, 16),
                    pad(&tool, 10),
                    collapse_home(&session.project_path, local_home)
                ));
            }
            output.push('\n');
        }
        output.push_str(&format!(
            "Total: {} sessions in profile '{profile_name}'\n",
            counts.total
        ));
        return Ok(output);
    }
    if counts.stopped > 0 {
        Ok(format!(
            "{} waiting • {} running • {} idle • {} stopped\n",
            counts.waiting, counts.running, counts.idle, counts.stopped
        ))
    } else {
        Ok(format!(
            "{} waiting • {} running • {} idle\n",
            counts.waiting, counts.running, counts.idle
        ))
    }
}

struct StatusCounts {
    waiting: usize,
    running: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
}

impl StatusCounts {
    fn new(sessions: &[&SessionRead]) -> Self {
        let mut counts = Self {
            waiting: 0,
            running: 0,
            idle: 0,
            stopped: 0,
            error: 0,
            total: sessions.len(),
        };
        for status in sessions.iter().map(|session| session.status) {
            match status {
                WireStatus::Waiting => counts.waiting += 1,
                WireStatus::Running => counts.running += 1,
                WireStatus::Idle | WireStatus::Unknown | WireStatus::Starting => counts.idle += 1,
                WireStatus::Stopped => counts.stopped += 1,
                WireStatus::Error => counts.error += 1,
                WireStatus::Creating | WireStatus::Deleting => {}
            }
        }
        counts
    }
}

fn render_show(
    args: &ShowArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<String, ReadFailure> {
    let identifier = args
        .identifier()
        .ok_or_else(|| ReadFailure::exit(2, "identifier required in daemon read mode\n"))?;
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    require_selected_profile_health(snapshot, profile, true)?;
    if !freshness_observed(&snapshot.status_freshness) {
        return Err(ReadFailure::post("freshness_unavailable"));
    }
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name).collect();
    let session = find_session(&sessions, identifier)?;

    if args.json {
        #[derive(Serialize)]
        struct ShowJson {
            id: String,
            title: String,
            path: String,
            group: String,
            tool: String,
            command: String,
            status: &'static str,
            state: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            trashed_at: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            archived_at: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            snoozed_until: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pinned_at: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            agent_session_id: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            parent_session_id: Option<String>,
            profile: String,
        }
        return json_lines(&[ShowJson {
            id: session.id.clone(),
            title: session.title.clone(),
            path: session.project_path.clone(),
            group: session.group_path.clone(),
            tool: session.tool.clone(),
            command: session.command.clone(),
            status: session.status.as_str(),
            state: session.state.as_str(),
            trashed_at: session.trashed_at.clone(),
            archived_at: session.archived_at.clone(),
            snoozed_until: session.active_snoozed_until.clone(),
            pinned_at: session.pinned_at.clone(),
            agent_session_id: session.agent_session_id.clone(),
            parent_session_id: session.parent_session_id.clone(),
            profile: profile_name.to_string(),
        }]);
    }

    let mut output = format!(
        "Session: {}\n  ID:      {}\n  Path:    {}\n  Group:   {}\n  Tool:    {}\n  Command: {}\n  Status:  {}\n",
        session.title,
        session.id,
        collapse_home(&session.project_path, local_home),
        session.group_path,
        session.tool,
        session.command,
        session.status.as_str()
    );
    match session.state {
        WireState::Live => {}
        WireState::Archived => output.push_str(&format!(
            "  State:   archived ({})\n",
            session
                .archived_at
                .as_deref()
                .expect("validated archived timestamp")
        )),
        WireState::Trashed => output.push_str(&format!(
            "  State:   trashed ({})\n",
            session
                .trashed_at
                .as_deref()
                .expect("validated trashed timestamp")
        )),
    }
    output.push_str(&format!("  Profile: {profile_name}\n"));
    if let Some(parent_id) = &session.parent_session_id {
        let parent = sessions
            .iter()
            .find(|candidate| &candidate.id == parent_id)
            .ok_or_else(|| ReadFailure::post("schema_invalid"))?;
        output.push_str(&format!("  Parent:  {} ({parent_id})\n", parent.title));
    }
    let mut children: Vec<&&SessionRead> = sessions
        .iter()
        .filter(|candidate| candidate.parent_session_id.as_deref() == Some(session.id.as_str()))
        .collect();
    if !children.is_empty() {
        children.sort_by(|left, right| left.id.cmp(&right.id));
        output.push_str("  Children:\n");
        for child in children {
            output.push_str(&format!("    {} ({})\n", child.title, child.id));
        }
    }
    Ok(output)
}

fn find_session<'a>(
    sessions: &[&'a SessionRead],
    identifier: &str,
) -> Result<&'a SessionRead, ReadFailure> {
    let exact: Vec<&SessionRead> = sessions
        .iter()
        .copied()
        .filter(|session| session.id == identifier)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    let matches = |test: &dyn Fn(&SessionRead) -> bool| -> Vec<&SessionRead> {
        sessions
            .iter()
            .copied()
            .filter(|session| test(session))
            .collect()
    };
    let prefix = matches(&|session| session.id.starts_with(identifier));
    if !prefix.is_empty() {
        return unique_or_ambiguous(prefix);
    }
    let title = matches(&|session| session.title == identifier);
    if !title.is_empty() {
        return unique_or_ambiguous(title);
    }
    let project = matches(&|session| session.project_path == identifier);
    if !project.is_empty() {
        return unique_or_ambiguous(project);
    }
    Err(ReadFailure::post("session_missing"))
}

fn unique_or_ambiguous(values: Vec<&SessionRead>) -> Result<&SessionRead, ReadFailure> {
    if values.len() == 1 {
        Ok(values[0])
    } else {
        Err(ReadFailure::post("session_ambiguous"))
    }
}

fn render_trash(
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
) -> Result<String, ReadFailure> {
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let mut sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name)
        .filter(|session| session.state == WireState::Trashed)
        .collect();
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    require_selected_profile_health(snapshot, profile, true)?;
    if sessions.is_empty() {
        return Ok("Trash is empty.\n".into());
    }
    let mut output = format!("Trashed sessions in profile '{profile_name}':\n");
    for session in sessions {
        output.push_str(&format!(
            "  {}  {}  (trashed {})\n",
            session.id,
            session.title,
            session
                .trashed_at
                .as_deref()
                .expect("validated trashed timestamp")
        ));
    }
    Ok(output)
}

fn render_groups(
    args: &GroupListArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
) -> Result<String, ReadFailure> {
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name).collect();
    require_selected_profile_health(snapshot, profile, true)?;
    if args.json() {
        #[derive(Serialize)]
        struct GroupJson {
            name: String,
            path: String,
            session_count: usize,
            children: Vec<String>,
        }
        let groups: Vec<GroupJson> = profile
            .groups
            .iter()
            .map(|group| GroupJson {
                name: group.name.clone(),
                path: group.path.clone(),
                session_count: sessions
                    .iter()
                    .filter(|session| session.group_path == group.path)
                    .count(),
                children: group.children.clone(),
            })
            .collect();
        return json_lines(&groups);
    }
    if profile.groups.is_empty() {
        return Ok("No groups found.\nCreate one with: aoe group create <name>\n".into());
    }
    let mut output = String::from("Groups:\n\n");
    for group in &profile.groups {
        let count = sessions
            .iter()
            .filter(|session| session.group_path == group.path)
            .count();
        let depth = group.path.matches('/').count();
        output.push_str(&format!(
            "{}• {} ({} sessions)\n",
            "  ".repeat(depth),
            group.name,
            count
        ));
    }
    output.push_str(&format!("\nTotal: {} groups\n", profile.groups.len()));
    Ok(output)
}

fn render_profiles(snapshot: &SnapshotData) -> Result<String, ReadFailure> {
    if !component_healthy(&snapshot.health.global_enumeration)
        || !component_healthy(&snapshot.health.global_metadata)
        || snapshot.profiles.iter().any(|profile| {
            !profile_component_healthy(&profile.health.profile_enumeration)
                || !profile_component_healthy(&profile.health.metadata)
        })
    {
        return Err(ReadFailure::post("health_degraded"));
    }
    if snapshot.profiles.is_empty() {
        return Ok(
            "No profiles found.\nRun 'aoe' to create the first profile automatically.\n".into(),
        );
    }
    let mut output = String::from("Profiles:\n");
    for profile in &snapshot.profiles {
        if snapshot.default_profile.as_deref() == Some(profile.name.as_str()) {
            output.push_str(&format!("  * {} (default)\n", profile.name));
        } else {
            output.push_str(&format!("    {}\n", profile.name));
        }
    }
    output.push_str(&format!("\nTotal: {} profiles\n", snapshot.profiles.len()));
    Ok(output)
}

fn render_projects(
    args: &ProjectListArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<String, ReadFailure> {
    let scope = args.scope();
    let (_profile_name, projects) = match scope {
        ScopeFilter::Global => {
            if !component_healthy(&snapshot.health.global_metadata) {
                return Err(ReadFailure::post("health_degraded"));
            }
            (None, snapshot.global_projects.clone())
        }
        ScopeFilter::Profile => {
            let name = selected_profile(snapshot, source)?;
            let profile = profile(snapshot, name)?;
            require_profile_components(profile, true, true)?;
            (Some(name), profile.projects.clone())
        }
        ScopeFilter::All => {
            let name = selected_profile(snapshot, source)?;
            let profile = profile(snapshot, name)?;
            if !component_healthy(&snapshot.health.global_metadata) {
                return Err(ReadFailure::post("health_degraded"));
            }
            require_profile_components(profile, true, true)?;
            let mut by_path: BTreeMap<String, ProjectRead> = snapshot
                .global_projects
                .iter()
                .cloned()
                .map(|project| (project.path.clone(), project))
                .collect();
            by_path.extend(
                profile
                    .projects
                    .iter()
                    .cloned()
                    .map(|project| (project.path.clone(), project)),
            );
            (Some(name), by_path.into_values().collect())
        }
    };
    let mut projects = projects;
    projects.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
    if args.json() {
        #[derive(Serialize)]
        struct ProjectJson {
            name: String,
            path: String,
            scope: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            default_base_branch: Option<String>,
        }
        let rows: Vec<ProjectJson> = projects
            .iter()
            .map(|project| ProjectJson {
                name: project.name.clone(),
                path: project.path.clone(),
                scope: project.scope.as_str(),
                default_base_branch: project.default_base_branch.clone(),
            })
            .collect();
        return json_lines(&rows);
    }
    if projects.is_empty() {
        return Ok("No projects registered.\nAdd one with: aoe project add <path>\n".into());
    }
    let mut output = String::from("Projects:\n\n");
    for project in &projects {
        output.push_str(&format!(
            "  • {} [{}]  {}\n",
            project.name,
            project.scope.as_str(),
            collapse_home(&project.path, local_home)
        ));
        if let Some(branch) = &project.default_base_branch {
            output.push_str(&format!("      base branch: {branch}\n"));
        }
    }
    let count = projects.len();
    output.push_str(&format!(
        "\nTotal: {} {}\n",
        count,
        if count == 1 { "project" } else { "projects" }
    ));
    Ok(output)
}

fn profile<'a>(snapshot: &'a SnapshotData, name: &str) -> Result<&'a ProfileRead, ReadFailure> {
    snapshot
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .ok_or_else(|| ReadFailure::post("profile_missing"))
}

fn sessions_for_profile<'a>(
    snapshot: &'a SnapshotData,
    profile: &'a str,
) -> impl Iterator<Item = &'a SessionRead> + use<'a> {
    snapshot
        .sessions
        .iter()
        .filter(move |session| session.profile == profile)
}

fn require_list_all_health(snapshot: &SnapshotData) -> Result<(), ReadFailure> {
    if !component_healthy(&snapshot.health.global_enumeration)
        || !component_healthy(&snapshot.health.global_metadata)
        || snapshot
            .profiles
            .iter()
            .any(|profile| !require_profile_components(profile, true, true).is_ok())
    {
        return Err(ReadFailure::post("health_degraded"));
    }
    Ok(())
}

fn require_selected_profile_health(
    snapshot: &SnapshotData,
    profile: &ProfileRead,
    include_global: bool,
) -> Result<(), ReadFailure> {
    if include_global
        && (!component_healthy(&snapshot.health.global_enumeration)
            || !component_healthy(&snapshot.health.global_metadata))
    {
        return Err(ReadFailure::post("health_degraded"));
    }
    require_profile_components(profile, true, true)
}

fn require_profile_components(
    profile: &ProfileRead,
    enumeration: bool,
    data: bool,
) -> Result<(), ReadFailure> {
    if (enumeration && !profile_component_healthy(&profile.health.profile_enumeration))
        || (data && !profile_component_healthy(&profile.health.profile_data))
    {
        return Err(ReadFailure::post("health_degraded"));
    }
    Ok(())
}

fn json_lines<T: Serialize>(value: &T) -> Result<String, ReadFailure> {
    serde_json::to_string_pretty(value)
        .map(|value| format!("{value}\n"))
        .map_err(|_| ReadFailure::exit(1, "daemon read: renderer_internal\n"))
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let mut output: String = value.chars().take(max - 3).collect();
    output.push_str("...");
    output
}

fn pad(value: &str, width: usize) -> String {
    let mut output = value.to_string();
    output.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(output.chars().count()),
    ));
    output
}

fn collapse_home(value: &str, home: Option<&Path>) -> String {
    let Some(home) = home.map(|path| path.to_string_lossy().into_owned()) else {
        return value.to_string();
    };
    let value = value.strip_suffix('/').unwrap_or(value);
    if value == home {
        "~".into()
    } else if let Some(rest) = value.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        value.to_string()
    }
}

impl ProjectScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Profile => "profile",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::runtime_read::dto::{
        CleanupDefaults, ComponentHealth, Cursor, ProfileHealth, SnapshotHealth, StatusFreshness,
    };
    use crate::cli::runtime_read::endpoint::ReadRequestSource;
    use std::ffi::OsString;

    fn health() -> ProfileHealth {
        ProfileHealth {
            profile_enumeration: ComponentHealth::Healthy,
            metadata: ComponentHealth::Healthy,
            profile_data: ComponentHealth::Healthy,
        }
    }

    fn session(id: &str, status: WireStatus) -> SessionRead {
        SessionRead {
            id: id.into(),
            title: id.into(),
            project_path: "/repo".into(),
            group_path: String::new(),
            tool: "tool".into(),
            command: String::new(),
            profile: "main".into(),
            status,
            state: WireState::Live,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            archived_at: None,
            trashed_at: None,
            active_snoozed_until: None,
            pinned_at: None,
            agent_session_id: None,
            parent_session_id: None,
            has_terminal: false,
            has_worktree_info: false,
            has_managed_worktree: false,
            has_cleanable_worktree: false,
            worktree: None,
            workspace_repos: vec![],
            cleanup_defaults: CleanupDefaults {
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                delete_to_trash: false,
            },
        }
    }

    fn snapshot(sessions: Vec<SessionRead>) -> SnapshotData {
        let profile = ProfileRead {
            name: "main".into(),
            groups: vec![],
            projects: vec![],
            health: health(),
        };
        SnapshotData {
            namespace: "debug:agent-of-empires-dev".into(),
            cursor: Cursor {
                epoch: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                revision: 1,
            },
            health: SnapshotHealth {
                global_enumeration: ComponentHealth::Healthy,
                global_metadata: ComponentHealth::Healthy,
                profiles: BTreeMap::from([("main".into(), health())]),
            },
            default_profile: Some("main".into()),
            profiles: vec![profile],
            sessions,
            global_projects: vec![],
            status_freshness: StatusFreshness::Observed {
                revision: 1,
                observed_at: "2026-01-01T00:00:00Z".into(),
            },
        }
    }

    fn source() -> ReadRequestSource {
        ReadRequestSource {
            explicit_url: None,
            env_url: None,
            token: None,
            explicit_profile: Some("main".into()),
            env_profile: Some(OsString::from("ignored")),
        }
    }

    #[test]
    fn status_json_is_exact_compact_shape() {
        let value = snapshot(vec![
            session("a", WireStatus::Waiting),
            session("b", WireStatus::Creating),
        ]);
        let args = crate::cli::status::StatusArgs::test_json();
        let output = render_status(&args, &value, &source(), None).unwrap();
        assert_eq!(
            output,
            "{\"waiting\":1,\"running\":0,\"idle\":0,\"stopped\":0,\"error\":0,\"total\":2}\n"
        );
    }

    #[test]
    fn list_json_preserves_v73_key_order_and_status_case() {
        let value = snapshot(vec![session("a", WireStatus::Waiting)]);
        let output = render_list(
            &crate::cli::list::ListArgs::test_json(),
            &value,
            &source(),
            None,
        )
        .unwrap();
        let keys = [
            "id",
            "title",
            "path",
            "group",
            "tool",
            "command",
            "profile",
            "status",
            "state",
            "created_at",
            "workspace_repos",
        ];
        let mut previous = 0;
        for key in keys {
            let position = output.find(&format!("\"{key}\"")).unwrap();
            assert!(position >= previous, "{key} out of order in {output}");
            previous = position;
        }
        assert!(output.contains("\"status\": \"Waiting\""));
        assert!(output.contains("\"state\": \"live\""));
    }

    #[test]
    fn list_all_zero_profiles_has_no_banner_or_table() {
        let mut value = snapshot(vec![]);
        value.profiles.clear();
        value.default_profile = None;
        value.health.profiles.clear();
        let args = crate::cli::list::ListArgs::test_all();
        let output = render_list(&args, &value, &source(), None).unwrap();
        assert_eq!(output, "No profiles found.\n");
    }

    #[test]
    fn home_collapse_matches_only_exact_path_boundary() {
        let home = Path::new("/home/alice");
        assert_eq!(collapse_home("/home/alice", Some(home)), "~");
        assert_eq!(collapse_home("/home/alice/repo", Some(home)), "~/repo");
        assert_eq!(
            collapse_home("/home/alice2/repo", Some(home)),
            "/home/alice2/repo"
        );
        assert_eq!(collapse_home("/remote/home", None), "/remote/home");
    }

    #[test]
    fn table_padding_and_separator_width_are_exact() {
        let mut output = String::new();
        push_table_header(&mut output, true);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0].chars().count(), 90);
        assert_eq!(lines[1], "-".repeat(90));
        let mut output = String::new();
        push_table_header(&mut output, false);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0].chars().count(), 80);
        assert_eq!(lines[1], "-".repeat(80));
    }

    /// The final ID column is unbounded: a long id is never truncated or
    /// padded, so a row may be wider than the fixed-width separator.
    #[test]
    fn the_final_id_column_is_never_truncated() {
        let long_id = "session-0123456789abcdef0123456789abcdef";
        let value = snapshot(vec![session(long_id, WireStatus::Waiting)]);
        let args = crate::cli::list::ListArgs {
            json: false,
            all: false,
            state: StateFilter::All,
        };
        let output = render_list(&args, &value, &source(), None).unwrap();
        let row = output
            .lines()
            .find(|line| line.ends_with(long_id))
            .expect("the row carries the whole id");
        // 20 + 1 + 15 + 1 + 40 + 1 + 9 + 1 fixed columns, then the whole id.
        assert_eq!(row.chars().count(), 88 + long_id.chars().count());
    }

    /// Verbose prints one group per non-empty status, no summary, and a Total line.
    #[test]
    fn verbose_status_emits_groups_without_a_summary() {
        let value = snapshot(vec![
            session("a", WireStatus::Waiting),
            session("b", WireStatus::Idle),
        ]);
        let args = crate::cli::status::StatusArgs {
            verbose: true,
            quiet: false,
            json: false,
        };
        let output = render_status(&args, &value, &source(), None).unwrap();
        let row = |symbol: &str, id: &str| {
            format!(
                "  {symbol} {} {} {}\n",
                pad(&truncate(id, 16), 16),
                pad(&truncate("tool", 10), 10),
                "/repo"
            )
        };

        assert_eq!(
            output,
            format!(
                "WAITING (1):\n{}\nIDLE (1):\n{}\nTotal: 2 sessions in profile 'main'\n",
                row("⠃", "a"),
                row("⠒", "b")
            )
        );
        assert!(!output.contains("waiting •"));
    }
}
