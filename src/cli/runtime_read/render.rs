use std::path::Path;

use serde::Serialize;

use super::dto::{
    component_healthy, freshness_observed, profile_component_healthy, ProfileRead, ProjectRead,
    ProjectScope, SessionRead, SnapshotData, WireState,
};
use super::endpoint::{selected_profile_source, ProfileSource};
use super::{ReadFailure, ScopedCommand};
use crate::cli::group::GroupListArgs;
use crate::cli::list::{self, ListArgs, SessionJson, StateFilter, WorkspaceRepoJson, WorktreeJson};
use crate::cli::project::{ProjectListArgs, ScopeFilter};
use crate::cli::session::ShowArgs;
use crate::cli::status::{self, VERBOSE_GROUPS};

/// One rendered read: the text the command prints, and whether it printed the
/// human session table, which is the only place the update notice belongs.
#[derive(Debug)]
pub(crate) struct Projection {
    pub stdout: String,
    pub session_table: bool,
}

impl Projection {
    fn text(stdout: String) -> Self {
        Self {
            stdout,
            session_table: false,
        }
    }
}

pub(crate) fn evaluate(
    command: &ScopedCommand<'_>,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<Projection, ReadFailure> {
    match command {
        ScopedCommand::List(args) => render_list(args, snapshot, source),
        ScopedCommand::Status(args) => render_status(args, snapshot, source, local_home),
        ScopedCommand::Show(args) => render_show(args, snapshot, source),
        ScopedCommand::ListTrash => render_trash(snapshot, source).map(Projection::text),
        ScopedCommand::GroupList(args) => {
            render_groups(args, snapshot, source).map(Projection::text)
        }
        ScopedCommand::Profile => render_profiles(snapshot).map(Projection::text),
        ScopedCommand::ProjectList(args) => {
            render_projects(args, snapshot, source).map(Projection::text)
        }
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
) -> Result<Projection, ReadFailure> {
    let state = args.state;
    if args.all {
        require_list_all_health(snapshot)?;
        if args.json {
            let mut rows = Vec::new();
            for profile in &snapshot.profiles {
                for session in sessions_for_profile(snapshot, &profile.name)
                    .filter(|session| matches_state(session.state, state))
                {
                    rows.push(session_json(session, &profile.name)?);
                }
            }
            return json_lines(&rows).map(Projection::text);
        }
        if snapshot.profiles.is_empty() {
            return Ok(Projection::text("No profiles found.\n".into()));
        }
        let show_state = state == StateFilter::All;
        let mut output = String::new();
        let mut total = 0usize;
        for profile in &snapshot.profiles {
            let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, &profile.name)
                .filter(|session| matches_state(session.state, state))
                .collect();
            // A profile with nothing in this scope prints no banner, exactly as
            // the local all-profiles listing skips it.
            if sessions.is_empty() {
                continue;
            }
            output.push_str(&format!("\n═══ Profile: {} ═══\n\n", profile.name));
            output.push_str(&list::table_header(show_state));
            for (session, depth) in tree_order(&sessions) {
                output.push_str(&list::table_row(
                    &list::table_title(&session.title, depth),
                    &session.group_path,
                    &session.project_path,
                    show_state.then(|| session.state.as_str()),
                    &session.id,
                ));
            }
            output.push_str(&format!("({} sessions)\n", sessions.len()));
            total += sessions.len();
        }
        output.push_str(&format!(
            "\n═══════════════════════════════════════\nTotal: {total} sessions across {} profiles\n",
            snapshot.profiles.len()
        ));
        return Ok(Projection::text(output));
    }

    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name)
        .filter(|session| matches_state(session.state, state))
        .collect();
    require_selected_profile_health(snapshot, profile, true)?;

    if args.json {
        let rows: Vec<SessionJson> = sessions
            .iter()
            .map(|session| session_json(session, profile_name))
            .collect::<Result<_, _>>()?;
        return json_lines(&rows).map(Projection::text);
    }
    if sessions.is_empty() {
        return Ok(Projection::text(format!(
            "No sessions found in profile '{profile_name}'.\n"
        )));
    }
    let show_state = state == StateFilter::All;
    let mut output = format!("Profile: {profile_name}\n\n");
    output.push_str(&list::table_header(show_state));
    for (session, depth) in tree_order(&sessions) {
        output.push_str(&list::table_row(
            &list::table_title(&session.title, depth),
            &session.group_path,
            &session.project_path,
            show_state.then(|| session.state.as_str()),
            &session.id,
        ));
    }
    // The per-profile listing has always closed on its own count, the way the
    // all-profiles listing closes on its own.
    output.push_str(&format!("\nTotal: {} sessions\n", sessions.len()));
    Ok(Projection {
        stdout: output,
        session_table: true,
    })
}

fn matches_state(state: WireState, filter: StateFilter) -> bool {
    match filter {
        StateFilter::All => true,
        StateFilter::Live => state == WireState::Live,
        StateFilter::Trashed => state == WireState::Trashed,
    }
}

/// The same walk the local listing performs, over the same rows.
fn tree_order<'a>(sessions: &'a [&'a SessionRead]) -> Vec<(&'a SessionRead, usize)> {
    list::nest_order(
        sessions,
        |session| session.id.as_str(),
        |session| session.parent_session_id.as_deref(),
    )
    .into_iter()
    .map(|(index, depth)| (sessions[index], depth))
    .collect()
}

/// The local `aoe list --json` row, filled from a snapshot. The timestamps are
/// parsed back to instants so the serializer spells them the way the local
/// command's own `DateTime<Utc>` fields do.
fn session_json(session: &SessionRead, profile: &str) -> Result<SessionJson, ReadFailure> {
    Ok(SessionJson {
        id: session.id.clone(),
        title: session.title.clone(),
        path: session.project_path.clone(),
        group: session.group_path.clone(),
        tool: session.tool.clone(),
        command: session.command.clone(),
        profile: profile.to_string(),
        state: session.state.as_str(),
        created_at: wire_instant_at(&session.created_at)?,
        trashed_at: wire_instant(session.trashed_at.as_deref())?,
        archived_at: wire_instant(session.archived_at.as_deref())?,
        snoozed_until: wire_instant(session.active_snoozed_until.as_deref())?,
        pinned_at: wire_instant(session.pinned_at.as_deref())?,
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
    })
}

fn wire_instant(value: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, ReadFailure> {
    value
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|parsed| parsed.with_timezone(&chrono::Utc))
                .map_err(|_| ReadFailure::post("schema_invalid"))
        })
        .transpose()
}

fn wire_instant_at(value: &str) -> Result<chrono::DateTime<chrono::Utc>, ReadFailure> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|_| ReadFailure::post("schema_invalid"))
}

fn render_status(
    args: &crate::cli::status::StatusArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
    local_home: Option<&Path>,
) -> Result<Projection, ReadFailure> {
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name).collect();
    require_selected_profile_health(snapshot, profile, true)?;
    if !freshness_observed(&snapshot.status_freshness) {
        return Err(ReadFailure::post("freshness_unavailable"));
    }
    let counts = status::count_statuses(
        sessions
            .iter()
            .map(|session| session.status.session_status()),
    );
    if args.json {
        return render_json(Ok(status::status_json(&counts))).map(Projection::text);
    }
    if args.quiet {
        return Ok(Projection::text(format!("{}\n", counts.waiting)));
    }
    if sessions.is_empty() {
        return Ok(Projection::text(format!(
            "No sessions in profile '{profile_name}'.\n"
        )));
    }
    if args.verbose {
        let mut output = String::new();
        for (label, symbol, status) in VERBOSE_GROUPS {
            let rows: Vec<(String, String, String)> = sessions
                .iter()
                .filter(|session| session.status.session_status() == status)
                .map(|session| {
                    (
                        session.title.clone(),
                        session.tool.clone(),
                        collapse_home(&session.project_path, local_home),
                    )
                })
                .collect();
            output.push_str(&status::verbose_group(label, symbol, &rows));
        }
        output.push_str(&format!(
            "Total: {} sessions in profile '{profile_name}'\n",
            counts.total
        ));
        return Ok(Projection {
            stdout: output,
            session_table: true,
        });
    }
    let summary = if counts.stopped > 0 {
        format!(
            "{} waiting • {} running • {} idle • {} stopped\n",
            counts.waiting, counts.running, counts.idle, counts.stopped
        )
    } else {
        format!(
            "{} waiting • {} running • {} idle\n",
            counts.waiting, counts.running, counts.idle
        )
    };
    Ok(Projection {
        stdout: summary,
        session_table: true,
    })
}

fn render_show(
    args: &ShowArgs,
    snapshot: &SnapshotData,
    source: &super::endpoint::ReadRequestSource,
) -> Result<Projection, ReadFailure> {
    let profile_name = selected_profile(snapshot, source)?;
    let profile = profile(snapshot, profile_name)?;
    require_selected_profile_health(snapshot, profile, true)?;
    if !freshness_observed(&snapshot.status_freshness) {
        return Err(ReadFailure::post("freshness_unavailable"));
    }
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name).collect();
    let identifier = match args.identifier() {
        Some(identifier) => identifier.to_string(),
        None => tmux_session_id(&sessions)?,
    };
    let session = find_session(&sessions, &identifier)?;

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
            trashed_at: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            archived_at: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pinned_at: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            agent_session_id: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            parent_session_id: Option<String>,
            profile: String,
        }
        return json_lines(&ShowJson {
            id: session.id.clone(),
            title: session.title.clone(),
            path: session.project_path.clone(),
            group: session.group_path.clone(),
            tool: session.tool.clone(),
            command: session.command.clone(),
            status: session.status.json_str(),
            state: session.state.as_str(),
            trashed_at: wire_instant(session.trashed_at.as_deref())?,
            archived_at: wire_instant(session.archived_at.as_deref())?,
            snoozed_until: wire_instant(session.active_snoozed_until.as_deref())?,
            pinned_at: wire_instant(session.pinned_at.as_deref())?,
            agent_session_id: session.agent_session_id.clone(),
            parent_session_id: session.parent_session_id.clone(),
            profile: profile_name.to_string(),
        })
        .map(Projection::text);
    }

    let mut output = format!(
        "Session: {}\n  ID:      {}\n  Path:    {}\n  Group:   {}\n  Tool:    {}\n  Command: {}\n  Status:  {}\n",
        session.title,
        session.id,
        session.project_path,
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
    let children: Vec<&&SessionRead> = sessions
        .iter()
        .filter(|candidate| candidate.parent_session_id.as_deref() == Some(session.id.as_str()))
        .collect();
    if !children.is_empty() {
        output.push_str("  Children:\n");
        for child in children {
            output.push_str(&format!("    {} ({})\n", child.title, child.id));
        }
    }
    Ok(Projection::text(output))
}

/// `aoe session show` with no identifier names the session whose agent pane is
/// the one this command runs in, as the local path has always done. The
/// refusals are the operator's own sentences, not internal codes: either the
/// command is not running inside tmux, or the tmux session it is in belongs to
/// no session this profile knows.
fn tmux_session_id(sessions: &[&SessionRead]) -> Result<String, ReadFailure> {
    let name = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|_| crate::tmux::get_current_session_name())
        .ok_or_else(|| {
            ReadFailure::exit(
                2,
                "Not in a tmux session. Specify a session ID or run inside tmux.\n",
            )
        })?;
    sessions
        .iter()
        .find(|session| crate::tmux::agent_session_belongs_to(&name, &session.id))
        .map(|session| session.id.clone())
        .ok_or_else(|| {
            ReadFailure::exit(
                2,
                "Current tmux session is not an Agent of Empires session\n",
            )
        })
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
    let sessions: Vec<&SessionRead> = sessions_for_profile(snapshot, profile_name)
        .filter(|session| session.state == WireState::Trashed)
        .collect();
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
) -> Result<String, ReadFailure> {
    let scope = args.scope();
    let projects: Vec<ProjectRead> = match scope {
        ScopeFilter::Global => {
            if !component_healthy(&snapshot.health.global_metadata) {
                return Err(ReadFailure::post("health_degraded"));
            }
            snapshot.global_projects.clone()
        }
        ScopeFilter::Profile => {
            let name = selected_profile(snapshot, source)?;
            let profile = profile(snapshot, name)?;
            require_profile_components(profile, true, true)?;
            profile.projects.clone()
        }
        ScopeFilter::All => {
            let name = selected_profile(snapshot, source)?;
            let profile = profile(snapshot, name)?;
            if !component_healthy(&snapshot.health.global_metadata) {
                return Err(ReadFailure::post("health_degraded"));
            }
            require_profile_components(profile, true, true)?;
            // The merged registry in the local order: the global rows, then the
            // profile rows that shadow them by path. A synthesized row is not a
            // registry entry, so it can neither shadow a global row nor add a
            // row of its own.
            let mut merged: Vec<ProjectRead> = Vec::new();
            for project in snapshot
                .global_projects
                .iter()
                .chain(profile.projects.iter())
            {
                if !project.registered {
                    continue;
                }
                match merged.iter_mut().find(|row| row.path == project.path) {
                    Some(row) => *row = project.clone(),
                    None => merged.push(project.clone()),
                }
            }
            merged
        }
    };
    let projects: Vec<ProjectRead> = projects
        .into_iter()
        .filter(|project| project.registered)
        .collect();
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
            project.path
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
    render_json(serde_json::to_string_pretty(value))
}

fn render_json(rendered: serde_json::Result<String>) -> Result<String, ReadFailure> {
    rendered
        .map(|value| format!("{value}\n"))
        .map_err(|_| ReadFailure::exit(1, "daemon read: renderer_internal\n"))
}

/// The local verbose listing collapses a path under the owner's home, and only
/// that listing does. A read from another host has no home to collapse
/// against, so the value passes through untouched.
fn collapse_home(value: &str, home: Option<&Path>) -> String {
    match home {
        Some(home) => crate::util::collapse_home(value, &home.to_string_lossy()),
        None => value.to_string(),
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
        WireStatus,
    };
    use crate::cli::runtime_read::endpoint::ReadRequestSource;
    use std::collections::BTreeMap;
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
        let output = render_status(&args, &value, &source(), None)
            .unwrap()
            .stdout;
        assert_eq!(
            output,
            "{\"waiting\":1,\"running\":0,\"idle\":0,\"stopped\":0,\"error\":0,\"total\":2}\n"
        );
    }
    /// The human `Status:` line and the machine JSON disagree on purpose: one
    /// is read by a person, the other by a script that has always seen the
    /// lowercase spelling.
    #[test]
    fn show_json_spells_status_lowercase_while_the_human_line_keeps_the_wire_form() {
        let value = snapshot(vec![session("a", WireStatus::Waiting)]);
        let json = render_show(
            &ShowArgs {
                identifier: Some("a".into()),
                json: true,
            },
            &value,
            &source(),
        )
        .unwrap()
        .stdout;
        assert!(json.contains("\"status\": \"waiting\""), "{json}");

        let human = render_show(
            &ShowArgs {
                identifier: Some("a".into()),
                json: false,
            },
            &value,
            &source(),
        )
        .unwrap()
        .stdout;
        assert!(human.contains("  Status:  Waiting\n"), "{human}");
    }

    /// `aoe session show` with no identifier still auto-detects inside tmux and
    /// still refuses, in the operator's own words, when it is not.
    #[test]
    fn show_without_an_identifier_refuses_outside_tmux_in_plain_words() {
        let value = snapshot(vec![session("a", WireStatus::Waiting)]);
        let error = render_show(
            &ShowArgs {
                identifier: None,
                json: false,
            },
            &value,
            &source(),
        )
        .unwrap_err();
        let outcome = crate::cli::runtime_read::ReadOutcome::from(error);
        assert_eq!(outcome.stdout, None);
        assert_eq!(outcome.exit, 2);
        assert_eq!(
            outcome.stderr.as_deref(),
            Some("Not in a tmux session. Specify a session ID or run inside tmux.\n")
        );
    }

    /// The per-profile listing closes on its own count, the way it always did,
    /// and only the human listing is the kind of output a notice follows.
    #[test]
    fn the_human_listing_closes_on_its_count_and_asks_for_a_notice() {
        let value = snapshot(vec![
            session("a", WireStatus::Waiting),
            session("b", WireStatus::Idle),
        ]);
        let args = crate::cli::list::ListArgs {
            json: false,
            all: false,
            state: StateFilter::All,
        };
        let projection = render_list(&args, &value, &source()).unwrap();
        assert!(projection.session_table);
        assert!(
            projection.stdout.ends_with("\nTotal: 2 sessions\n"),
            "{}",
            projection.stdout
        );

        let empty = snapshot(vec![]);
        let empty_projection = render_list(&args, &empty, &source()).unwrap();
        assert!(!empty_projection.session_table);
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
}
