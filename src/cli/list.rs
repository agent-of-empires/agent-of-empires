//! `agent-of-empires list` command implementation

use crate::session::SessionScope;
use clap::{Args, ValueEnum};
use serde::Serialize;

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

/// Refuse an incomplete single-profile listing.
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
