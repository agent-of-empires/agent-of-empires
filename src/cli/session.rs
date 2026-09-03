//! `agent-of-empires session` subcommands implementation

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use std::collections::HashSet;

use crate::session::{
    acquire_session_identity_lock, duplicate_session_error, is_duplicate_session, GroupTree,
    Instance, LifecycleOperation, ResumeIntent, StartOutcome, Storage,
};

#[derive(Subcommand)]
pub enum SessionCommands {
    /// Start a session's tmux process
    Start(SessionIdArgs),

    /// Stop session process
    Stop(SessionIdArgs),

    /// Restart session (or all sessions with `--all`)
    Restart(RestartArgs),

    /// Attach to session interactively
    Attach(SessionIdArgs),

    /// Show session details
    Show(ShowArgs),

    /// Rename a session
    Rename(RenameArgs),

    /// Edit a managed worktree session's workdir directory name (and,
    /// optionally, its git branch). Moves the worktree directory in place;
    /// the session must not be running. See #1723.
    SetWorktreeName(SetWorktreeNameArgs),

    /// Capture tmux pane output
    Capture(CaptureArgs),

    /// Auto-detect current session
    Current(CurrentArgs),

    /// Attach another repo to an existing session, so an agent that turns out
    /// to need a second repo can keep working in the same conversation instead
    /// of the session being recreated. Creates a worktree for the repo and
    /// restarts the agent so it can see it; the conversation is kept. See #3103.
    AddProject(AddProjectArgs),

    /// Set the resume target for a session; agents with resume disabled in AoE
    /// store the ID but do not use it
    SetSessionId(SetSessionIdArgs),

    /// Set or clear the per-session diff base branch. The diff view
    /// compares the worktree against this ref instead of the
    /// auto-detected default. Useful when the PR target differs from
    /// the project default (stacked PRs, hotfix off `release/*`,
    /// renamed default branch). See #970.
    SetBase(SetBaseArgs),

    /// Snooze a session for a duration (temporary archive, auto wakes)
    Snooze(SnoozeArgs),

    /// Wake a snoozed session immediately
    Unsnooze(SessionIdArgs),

    /// Mark a session as a favorite. With `session.favorites_first` on (the
    /// default), favorited rows pin to the top of their sibling scope in every
    /// sort order; with it off, they pin within their status tier in the
    /// Attention sort only. Either way the row renders with a leading `*`
    /// marker plus bold and underline wherever the pin applies. Snoozing a
    /// favorite suspends the pin until it wakes.
    Favorite(SessionIdArgs),

    /// Clear the favorite flag on a session.
    Unfavorite(SessionIdArgs),

    /// Set (or clear) a per-session color label, rendered as a colored dot in
    /// the web sidebar for at-a-glance status signaling. Intended for a
    /// running agent to flag its own state, e.g.
    /// `aoe session color $(aoe session current -q) red`. Colors: `red`
    /// (needs attention), `amber` (working), `green` (done); `none` clears it.
    Color(SetColorArgs),

    /// Archive a session: sink it in the Attention sort and tear down its
    /// tmux sessions. Worktree, branch, container preserved. `--no-kill`
    /// skips tmux teardown. See #1868.
    Archive(ArchiveArgs),

    /// Unarchive a session (restores it to its tier in the Attention sort)
    Unarchive(SessionIdArgs),

    /// Restore a trashed session, returning it to its prior bucket with its
    /// transcript and metadata intact. See #2489.
    Restore(SessionIdArgs),

    /// Import existing Claude Code sessions from disk. Scans the given
    /// path(s) (default: current directory) for Claude Code conversations
    /// whose working directory is at or under a path, and creates an AoE
    /// session for each: a terminal/tmux session that resumes the
    /// conversation with `claude --resume <id>` (default), or a
    /// structured-view session with `--structured`.
    Import(ImportArgs),

    /// List the sessions currently in the trash.
    ListTrash,

    /// Permanently purge every trashed session in the profile (irreversible).
    EmptyTrash,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Directories to scan. Only Claude sessions whose recorded working
    /// directory is at or under one of these are imported. Defaults to the
    /// current directory. Cannot be combined with `--all`.
    pub paths: Vec<String>,

    /// Import every discoverable Claude session, ignoring the path filter.
    #[arg(long, conflicts_with = "paths")]
    pub all: bool,

    /// Import as structured-view sessions (rendered in the web dashboard and
    /// the structured TUI view) instead of terminal/tmux sessions. Structured
    /// sessions replay their transcript under `aoe serve`.
    #[arg(long)]
    pub structured: bool,

    /// Place imported sessions under this session group.
    #[arg(long)]
    pub group: Option<String>,

    /// Start terminal sessions immediately after importing (spawns the tmux
    /// pane running `claude --resume <id>`). Ignored for structured imports.
    #[arg(long)]
    pub launch: bool,

    /// List what would be imported without creating anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt when importing more than one session.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args)]
pub struct SnoozeArgs {
    /// Session ID or title
    pub identifier: String,

    /// Snooze duration in minutes; if omitted, uses `session.snooze_duration_minutes`
    /// from the active config (default 30)
    #[arg(long)]
    pub minutes: Option<u32>,
}

#[derive(Args)]
pub struct ArchiveArgs {
    /// Session ID or title
    pub identifier: String,

    /// Skip tmux teardown on archive.
    #[arg(long = "no-kill")]
    pub no_kill: bool,
}

#[derive(Args)]
pub struct SessionIdArgs {
    /// Session ID or title
    identifier: String,
}

#[derive(Args)]
pub struct RestartArgs {
    /// Session ID or title (required unless `--all` is passed)
    pub identifier: Option<String>,

    /// Restart every session in the active profile. Useful after
    /// `aoe update`, after editing `sandbox.environment`, after a
    /// Docker hiccup, or after changing a hook. Mutually exclusive
    /// with `identifier`.
    #[arg(long, conflicts_with = "identifier")]
    pub all: bool,

    /// Concurrency cap for `--all`. Restarting many sandboxed
    /// sessions in parallel pressures dockerd, so the default is
    /// intentionally modest. Ignored when `--all` is not set.
    #[arg(long, default_value_t = 3)]
    pub parallel: usize,
}

#[derive(Args)]
pub struct RenameArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// New title for the session
    #[arg(short, long)]
    title: Option<String>,

    /// New group for the session (empty string to ungroup)
    #[arg(short, long)]
    group: Option<String>,

    /// When the session is tied (session.tie_workdir_to_name) and an
    /// aoe-managed worktree, also rename the underlying git branch to match.
    /// Off by default; ignored for untied / non-worktree sessions.
    #[arg(long)]
    rename_branch: bool,
}

#[derive(Args)]
pub struct SetWorktreeNameArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// New workdir (worktree directory) name
    #[arg(long)]
    name: String,

    /// Also rename the underlying git branch to match the new name
    #[arg(long)]
    rename_branch: bool,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct CaptureArgs {
    /// Session ID or title (auto-detects in tmux if omitted)
    identifier: Option<String>,

    /// Number of lines to capture
    #[arg(short = 'n', long, default_value = "50")]
    lines: usize,

    /// Strip ANSI escape codes
    #[arg(long)]
    strip_ansi: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct CurrentArgs {
    /// Just session name (for scripting)
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct CaptureOutput {
    id: String,
    title: String,
    status: String,
    tool: String,
    content: String,
    lines: usize,
}

#[derive(Args)]
pub struct SetSessionIdArgs {
    /// Session ID or title
    identifier: String,
    /// Resume target: for resume-enabled agents, a UUID/sid pins subsequent
    /// launches to that conversation; agents with resume disabled in AoE store
    /// but do not use it. An empty string forces a one-shot fresh start.
    session_id: String,
}

#[derive(Args)]
pub struct AddProjectArgs {
    /// Session ID or title
    pub identifier: String,
    /// Repo to attach: a path, or the name of a registered project
    /// (`aoe project list`).
    pub project: String,
    /// Check out a branch that already exists in the repo being attached
    /// instead of refusing. A same-named branch in another repo can hold
    /// unrelated commits, so this is off by default. When set, aoe records the
    /// branch as not its own and leaves it in place when the session is
    /// deleted.
    #[arg(long)]
    pub attach_existing_branch: bool,
}

#[derive(Args)]
pub struct SetBaseArgs {
    /// Session ID or title
    pub identifier: String,
    /// Branch ref to diff against (short name like `main` or
    /// remote-qualified like `upstream/main`). Required unless
    /// `--clear` is passed.
    pub branch: Option<String>,
    /// Clear the override and fall back to the recorded creation base,
    /// then the profile default, then the auto-detected base.
    #[arg(long, conflicts_with = "branch")]
    pub clear: bool,
    /// Workspace repo to set the base for, by directory name (as shown in
    /// the diff panel and `aoe list --json`). Required on a multi-repo
    /// workspace session, where each repo has its own base; omit it on a
    /// single-repo session.
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Args)]
pub struct SetColorArgs {
    /// Session ID or title
    pub identifier: String,
    /// Color label: `red` (needs attention), `amber` (working), `green`
    /// (done), or `none`/`clear` to remove the label.
    pub color: String,
}

#[derive(Serialize)]
struct SessionDetails {
    id: String,
    title: String,
    path: String,
    group: String,
    tool: String,
    command: String,
    status: String,
    /// The same `live`/`archived`/`trashed` tag `aoe list --json` carries
    /// (#3350/#3361), so a consumer does not have to fall back to `list` to
    /// learn whether the session it just looked up is still around. `status`
    /// cannot carry it: an archived session can be running, so the two are
    /// independent and collapsing them loses one.
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Gated on [`Instance::is_snoozed`] exactly like `aoe list --json` and
    /// the API: surfaced only while the deadline is in the future, so a row
    /// whose snooze expired omits the key instead of advertising it.
    #[serde(skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    /// Set iff the session is currently pinned for the web sidebar.
    /// Independent of `state`, matching the API field from #1581.
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    profile: String,
}

fn session_details(inst: &Instance, profile: &str) -> SessionDetails {
    SessionDetails {
        id: inst.id.clone(),
        title: inst.title.clone(),
        path: inst.project_path.clone(),
        group: inst.group_path.clone(),
        tool: inst.tool.clone(),
        command: inst.command.clone(),
        status: format!("{:?}", inst.status).to_lowercase(),
        state: super::list::state_tag(inst),
        trashed_at: inst.trashed_at,
        archived_at: inst.archived_at,
        snoozed_until: super::list::active_snoozed_until(inst),
        pinned_at: inst.pinned_at,
        agent_session_id: inst.agent_session_id.clone(),
        parent_session_id: inst.parent_session_id.clone(),
        profile: profile.to_string(),
    }
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, command: SessionCommands) -> Result<()> {
    match command {
        SessionCommands::Start(args) => start_session(profile, args).await,
        SessionCommands::Stop(args) => stop_session(profile, args).await,
        SessionCommands::Restart(args) => restart_session_dispatch(profile, args).await,
        SessionCommands::Attach(args) => attach_session(profile, args).await,
        SessionCommands::Show(args) => show_session(profile, args).await,
        SessionCommands::Capture(args) => capture_session(profile, args).await,
        SessionCommands::Rename(args) => rename_session(profile, args).await,
        SessionCommands::SetWorktreeName(args) => set_worktree_name(profile, args).await,
        SessionCommands::Current(args) => current_session(args).await,
        SessionCommands::SetSessionId(args) => set_session_id(profile, args).await,
        SessionCommands::AddProject(args) => add_project(profile, args).await,
        SessionCommands::SetBase(args) => set_base(profile, args).await,
        SessionCommands::Snooze(args) => snooze_session(profile, args).await,
        SessionCommands::Unsnooze(args) => unsnooze_session(profile, args).await,
        SessionCommands::Favorite(args) => favorite_session(profile, args).await,
        SessionCommands::Unfavorite(args) => unfavorite_session(profile, args).await,
        SessionCommands::Color(args) => set_color_session(profile, args).await,
        SessionCommands::Archive(args) => archive_session(profile, args).await,
        SessionCommands::Unarchive(args) => unarchive_session(profile, args).await,
        SessionCommands::Restore(args) => restore_session(profile, args).await,
        SessionCommands::Import(args) => import_sessions(profile, args).await,
        SessionCommands::ListTrash => list_trash(profile).await,
        SessionCommands::EmptyTrash => empty_trash(profile).await,
    }
}

async fn favorite_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.favorite();
            Ok(inst.title.clone())
        })
    })?;
    println!("Favorited: {}", title);
    Ok(())
}

async fn unfavorite_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unfavorite();
            Ok(inst.title.clone())
        })
    })?;
    println!("Unfavorited: {}", title);
    Ok(())
}

async fn set_color_session(profile: &str, args: SetColorArgs) -> Result<()> {
    // `none`/`clear`/empty clears the label; anything else must be a palette
    // member (validated inside `Instance::set_color`).
    let normalized = args.color.trim().to_lowercase();
    let new_color = match normalized.as_str() {
        "none" | "clear" | "" => None,
        other => Some(other.to_string()),
    };

    let storage = Storage::new_unwatched(profile)?;
    let (title, color) = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.set_color(new_color.clone())
                .map_err(|e| anyhow::anyhow!(e))?;
            Ok((inst.title.clone(), inst.color.clone()))
        })
    })?;

    match color {
        Some(c) => println!("✓ Set color for '{}': {}", title, c),
        None => println!("✓ Cleared color for '{}'", title),
    }
    Ok(())
}

async fn archive_session(profile: &str, args: ArchiveArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Phase 1 (unlocked): resolve identifier.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();
    let inst = inst.clone();

    // Serialize teardown and the archive commit as one lifecycle transition.
    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire instance archive lock")?;
    if !args.no_kill {
        if let Err(e) = inst.kill_locked() {
            eprintln!("Warning: failed to kill agent tmux session: {}", e);
        }
        inst.kill_ancillary_tmux_sessions_locked();
    }

    // Archive under the lifecycle lock so the state and its generation bump are
    // durable before the lock releases and a concurrent restart can observe them.
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.archive();
            stored.lifecycle_generation = stored.lifecycle_generation.saturating_add(1);
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
    if landed {
        println!("Archived: {}", title);
        Ok(())
    } else {
        bail!(
            "Session {} was removed by another process before archive could land",
            title
        );
    }
}

async fn unarchive_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unarchive();
            Ok(inst.title.clone())
        })
    })?;
    println!("Unarchived: {}", title);
    Ok(())
}

async fn restore_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Resolve within the trashed subset only. The CLI advertises the argument
    // as an id OR title, and a live or archived session can share a title/path
    // with a trashed one; resolving against the full list would let that row
    // win and make `untrash()` a silent no-op on an already-live session.
    // See #2489.
    let (instances, _groups) = storage.load_with_groups()?;
    let trashed: Vec<_> = instances
        .iter()
        .filter(|i| i.is_trashed())
        .cloned()
        .collect();
    let mut inst = super::resolve_session(&args.identifier, &trashed)
        .map_err(|_| anyhow::anyhow!("No trashed session matching '{}'", args.identifier))?
        .clone();
    let restore_id = inst.id.clone();

    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&restore_id)
        .context("failed to acquire instance restore lock")?;
    let decision = storage.update(|instances, _groups| {
        crate::session::claim::decide_restore_claim(instances, &restore_id, chrono::Utc::now())
            .map_err(anyhow::Error::new)
    })?;
    let restore_generation = match decision {
        crate::session::claim::RestoreClaimDecision::AlreadyGone => {
            anyhow::bail!("No trashed session matching '{}'", args.identifier)
        }
        crate::session::claim::RestoreClaimDecision::Busy(holder) => anyhow::bail!(
            "Session {} is {}, so it was not restored",
            inst.title,
            holder.busy_reason()
        ),
        crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
    };

    // Move the worktree back to its pre-trash location before flipping the
    // marker. Strict: if the original path is occupied or git refuses, leave
    // the session trashed and surface the error rather than restoring it to
    // the holding-area path.
    if let crate::session::trash::RestoreOutcome::Failed { reason } =
        crate::session::trash::restore_worktree_location(&mut inst)
    {
        release_restore_reservation(&storage, &restore_id, restore_generation);
        anyhow::bail!("Cannot restore worktree: {reason}");
    }
    let restored_path = inst.project_path.clone();
    let restored_pre = inst.pre_trash_project_path.clone();

    let commit = storage.update(|instances, _groups| {
        Ok(crate::session::claim::finalize_restore_commit(
            instances,
            &restore_id,
            restore_generation,
            &restored_path,
            &restored_pre,
        ))
    })?;
    match commit {
        crate::session::claim::RestoreCommit::Committed => {}
        crate::session::claim::RestoreCommit::Superseded => anyhow::bail!(
            "Session {} lost its lifecycle reservation during restore",
            inst.title
        ),
        crate::session::claim::RestoreCommit::AlreadyGone => {
            anyhow::bail!("No trashed session matching '{}'", args.identifier)
        }
    }
    println!("Restored: {}", inst.title);
    Ok(())
}

fn release_restore_reservation(storage: &Storage, restore_id: &str, generation: u64) {
    let _ = storage.update(|instances, _groups| {
        if let Some(stored) = instances
            .iter_mut()
            .find(|instance| instance.id == restore_id)
        {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
        }
        Ok(())
    });
}

async fn list_trash(profile: &str) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let trashed: Vec<_> = instances.iter().filter(|i| i.is_trashed()).collect();
    if trashed.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }
    println!("Trashed sessions in profile '{}':", storage.profile());
    for inst in trashed {
        let when = inst
            .trashed_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "?".to_string());
        println!("  {}  {}  (trashed {})", inst.id, inst.title, when);
    }
    Ok(())
}

async fn empty_trash(profile: &str) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Snapshot the trashed sessions. Each row's selected-profile lifecycle
    // lock is acquired immediately before its purge claim and retained through
    // that row's durable finalize. Purge forces removal so a dirty worktree
    // cannot keep an emptied session pinned in the trash.
    let (instances, _groups) = storage.load_with_groups()?;
    let mut trashed: Vec<_> = instances
        .iter()
        .filter(|i| i.is_trashed())
        .cloned()
        .collect();
    for instance in &mut trashed {
        instance.source_profile = storage.profile().to_string();
    }
    // Deterministic order keeps concurrent batch reports stable. Each row is
    // finalized before the next lock is acquired, so batches cannot deadlock.
    trashed.sort_by(|left, right| left.id.cmp(&right.id));
    if trashed.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }

    let mut removed = 0usize;
    let mut restored_after_teardown = 0usize;
    let mut kept_for_retry = 0usize;
    // Rows whose reservation was refused before teardown are benign and
    // reported separately from restores that land after teardown began.
    let mut being_restored_elsewhere = 0usize;
    let mut being_purged_elsewhere = 0usize;
    for inst in &trashed {
        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            profile,
            std::path::Path::new(&inst.project_path),
        );
        let delete_worktree =
            config.worktree.auto_cleanup && inst.has_managed_worktree_or_workspace();
        let delete_branch = delete_worktree && config.worktree.delete_branch_on_cleanup;
        let delete_sandbox =
            inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) && config.sandbox.auto_cleanup;
        let row_storage = Storage::open_unwatched(profile)?;
        let reservation = crate::session::deletion::PurgeTransaction::reserve(
            row_storage,
            crate::session::deletion::DeletionRequest {
                session_id: inst.id.clone(),
                instance: inst.clone(),
                delete_worktree,
                delete_branch,
                delete_sandbox,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
            },
        )?;
        let transaction = match reservation {
            crate::session::deletion::PurgeReservation::Reserved(transaction) => transaction,
            crate::session::deletion::PurgeReservation::Rejected(result) => {
                match result.disposition {
                    crate::session::deletion::DeletionDisposition::KeptRestored => {
                        being_restored_elsewhere += 1;
                    }
                    crate::session::deletion::DeletionDisposition::Busy => {
                        being_purged_elsewhere += 1;
                    }
                    _ => {}
                }
                continue;
            }
        };
        let result = transaction.run_hooks().complete_with(|instance| {
            super::purge_acp_transcript(instance).map_err(|error| {
                format!("transcript not purged, keeping session in trash: {error}")
            })
        });
        for err in &result.errors {
            eprintln!("Warning ({}): {}", inst.title, err);
        }
        match result.disposition {
            crate::session::deletion::DeletionDisposition::Removed => removed += 1,
            crate::session::deletion::DeletionDisposition::KeptRestored => {
                if result.teardown_started {
                    restored_after_teardown += 1;
                } else {
                    being_restored_elsewhere += 1;
                }
            }
            crate::session::deletion::DeletionDisposition::Busy => {
                being_purged_elsewhere += 1;
            }
            crate::session::deletion::DeletionDisposition::Failed => kept_for_retry += 1,
            crate::session::deletion::DeletionDisposition::AlreadyGone => {}
        }
    }
    let outcome = super::EmptyTrashOutcome {
        removed,
        restored_after_teardown,
        kept_for_retry,
    };
    // A restore that raced our teardown after it began is the only case that
    // risks orphaned artifacts, so it gets the repair warning; benign
    // being-restored-elsewhere rows (no teardown ran) do not.
    if outcome.restored_after_teardown > 0 {
        eprintln!(
            "Warning: {} session(s) were restored mid-purge after teardown began; kept the \
             restored records, but their worktree, branch, container, or transcript may already \
             have been removed. Inspect and repair them.",
            outcome.restored_after_teardown
        );
    }
    // Each figure is its own disjoint category, so the "restored mid-purge"
    // count matches the warning above exactly (no summary/warning mismatch).
    let mut parts = vec![format!("purged {} session(s)", outcome.removed)];
    if outcome.kept_for_retry > 0 {
        parts.push(format!("kept {} for retry", outcome.kept_for_retry));
    }
    if being_restored_elsewhere > 0 {
        parts.push(format!(
            "{being_restored_elsewhere} being restored by another process"
        ));
    }
    if being_purged_elsewhere > 0 {
        parts.push(format!(
            "{being_purged_elsewhere} being purged by another process"
        ));
    }
    if outcome.restored_after_teardown > 0 {
        parts.push(format!(
            "{} restored mid-purge",
            outcome.restored_after_teardown
        ));
    }
    println!(
        "Emptied trash: {} (profile '{}').",
        parts.join(", "),
        storage.profile()
    );
    Ok(())
}

async fn snooze_session(profile: &str, args: SnoozeArgs) -> Result<()> {
    let config = crate::session::config::profile_config::resolve_config(profile)?;

    // `--minutes` overrides the profile default; otherwise use the
    // configured `snooze_duration_minutes`. Validate either way so the
    // on-disk config can't sneak in an out of range value.
    let raw_minutes = args
        .minutes
        .map(|m| m as u64)
        .unwrap_or(config.session.snooze_duration_minutes as u64);
    crate::session::validate_snooze_duration(raw_minutes).map_err(|e| anyhow::anyhow!("{}", e))?;
    let minutes = raw_minutes as u32;

    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.snooze(minutes);
            Ok(inst.title.clone())
        })
    })?;
    println!("Snoozed for {}m: {}", minutes, title);
    Ok(())
}

async fn unsnooze_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unsnooze();
            Ok(inst.title.clone())
        })
    })?;
    println!("Woke: {}", title);
    Ok(())
}

async fn start_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the target by identifier, rehydrate
    // `source_profile` so config resolution honors the right profile.
    // `source_profile` is runtime-only (skip_serializing) so storage-loaded
    // instances always come back blank.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "start")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();

    // Snapshot the sid for the same reason `restart_session` does: a persisted
    // `ResumeIntent::Cleared` (from `aoe session set-session-id <id> ""`) makes
    // `acquire_session_id` drop it on this launch, but the abandoned rollout
    // lingers and stays newest-by-mtime, so the fresh poller's immediate first
    // poll can re-observe it and the drain below would silently revert the
    // user's clear.
    let prior_sid = working.agent_session_id.clone();

    // Launch orchestration owns its lifecycle locks and deliberately releases
    // them while user hooks run.
    let _ = working.start_with_size_opts(crate::terminal::get_size(), false)?;

    // Cleared on this launch, so the sid we came in with is abandoned.
    if working.agent_session_id.is_none() {
        if let Some(sid) = prior_sid {
            working.retroactive_capture_excludes.insert(sid);
        }
    }

    // The CLI has no long-lived loop to drain the just-started session-id
    // poller, so a capture-deferred agent would exit with agent_session_id unset
    // and silently lose resume. Wait briefly for the poller and persist via the
    // same drain the TUI/daemon use.
    let file_watch = crate::file_watch::FileWatchService::noop();
    crate::session::sync::capture_launched_session_id_blocking(
        &mut working,
        &file_watch,
        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
        true,
    );

    let title = working.title.clone();
    let id = working.id.clone();

    // Reacquire only for the final merge. The generation-aware merge rejects
    // this working snapshot if a peer completed a newer lifecycle operation.
    let _merge_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire instance start merge lock")?;
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.merge_post_start(&working);
            Ok(true)
        } else {
            tracing::warn!(
                target: "session.cli",
                session_id = %id,
                "session row removed by peer between phase 1 and phase 3 of start; tmux session is now orphan"
            );
            Ok(false)
        }
    })?;
    if !landed {
        bail!(
            "Session {} was removed by another process before start could land; tmux session is now orphan",
            title
        );
    }

    println!("✓ Started session: {}", title);
    Ok(())
}

/// Acp-mode sessions are not backed by tmux; their ACP worker is owned
/// by `aoe serve`'s supervisor (auto-spawned by the reconciler within ~2s
/// of the session appearing on disk). Calling `start`/`stop`/`restart`
/// from the CLI silently no-ops, which previously misled users into
/// thinking the session was up. Bail loudly with the actual remediation.
fn bail_if_acp(inst: &crate::session::Instance, verb: &str) -> Result<()> {
    if inst.is_structured() {
        bail!(
            "structured view sessions are managed by `aoe serve`; \
             cannot `aoe session {verb}` from the CLI.\n\
             The ACP worker is auto-spawned within ~2s of an structured-view session \
             while serve is running, or on next `aoe serve` startup.\n\
             To control an structured-view session, use the web dashboard or the REST API."
        );
    }
    Ok(())
}

/// Resolve the scan roots for `aoe session import`. Empty input means the
/// current directory. Each root is canonicalized (falling back to the path as
/// given if it does not resolve) so the component-aware filter compares against
/// absolute paths.
fn resolve_import_roots(paths: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let raw: Vec<std::path::PathBuf> = if paths.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        paths.iter().map(std::path::PathBuf::from).collect()
    };
    Ok(raw
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect())
}

/// True when `id` is already imported by some instance, so a re-run does not
/// create duplicates. Checks the terminal resume target, the poller-observed
/// id, and the structured-view id.
fn already_imported(instances: &[Instance], id: &str) -> bool {
    instances.iter().any(|inst| {
        if inst.agent_session_id.as_deref() == Some(id) {
            return true;
        }
        if matches!(&inst.resume_intent, ResumeIntent::Use(s) if s == id) {
            return true;
        }
        if inst.acp_session_id.as_deref() == Some(id) {
            return true;
        }
        false
    })
}

/// Build the AoE `Instance` for one discovered Claude session. Terminal imports
/// pin `resume_intent` so the first launch emits `claude --resume <id>`;
/// structured imports (serve only) seed the fields the reconciler reads to
/// replay the transcript.
fn build_import_instance(
    s: &crate::session::claude_import::ClaudeSessionSummary,
    structured: bool,
    group: &str,
) -> Instance {
    let title = s.title.clone().unwrap_or_else(|| {
        let short = s.session_id.get(..8).unwrap_or(s.session_id.as_str());
        format!("Claude import {short}")
    });
    let mut inst = Instance::new(&title, &s.cwd);
    inst.tool = "claude".to_string();
    if !group.is_empty() {
        inst.group_path = group.to_string();
    }
    apply_import_mode(&mut inst, s, structured);
    inst
}

fn apply_import_mode(
    inst: &mut Instance,
    s: &crate::session::claude_import::ClaudeSessionSummary,
    structured: bool,
) {
    if structured {
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some(s.session_id.clone());
        inst.import_pending = Some(true);
    } else {
        inst.resume_intent = ResumeIntent::Use(s.session_id.clone());
    }
}

async fn import_sessions(profile: &str, args: ImportArgs) -> Result<()> {
    use crate::session::claude_import::{scan_sessions, sessions_under_paths, MAX_SESSIONS};

    let structured = args.structured;

    // Discover, then narrow to the requested paths unless --all.
    let mut discovered = scan_sessions();
    if !args.all {
        let roots = resolve_import_roots(&args.paths)?;
        discovered = sessions_under_paths(discovered, &roots);
    }

    // A session whose recorded cwd no longer exists cannot be resumed:
    // `claude --resume` resolves the transcript by cwd, so a dead cwd would
    // silently start a fresh conversation. Skip and report those.
    let (candidates, missing_cwd): (Vec<_>, Vec<_>) =
        discovered.into_iter().partition(|s| s.cwd_exists);

    // Dedupe against sessions already imported into this profile.
    let (existing, _groups) = Storage::open_unwatched(profile)?.load_with_groups()?;
    let candidate_count = candidates.len();
    let mut to_import: Vec<_> = candidates
        .into_iter()
        .filter(|s| !already_imported(&existing, &s.session_id))
        .collect();
    let already = candidate_count - to_import.len();

    // Bulk safety backstop; the picker cap also applies to the CLI.
    let capped = to_import.len() > MAX_SESSIONS;
    if capped {
        to_import.truncate(MAX_SESSIONS);
    }

    let report_skipped = || {
        if already > 0 {
            println!("  ({already} already imported, skipped)");
        }
        if !missing_cwd.is_empty() {
            println!(
                "  ({} skipped: working directory no longer exists)",
                missing_cwd.len()
            );
        }
        if capped {
            println!("  (capped at {MAX_SESSIONS}; narrow the path(s) to import the rest)");
        }
    };

    if to_import.is_empty() {
        println!("No new Claude Code sessions to import.");
        report_skipped();
        return Ok(());
    }

    let kind = if structured { "structured" } else { "terminal" };
    println!(
        "Found {} Claude Code session(s) to import as {kind} sessions:",
        to_import.len()
    );
    for s in &to_import {
        let short = s.session_id.get(..8).unwrap_or(s.session_id.as_str());
        let title = s.title.as_deref().unwrap_or("(no title)");
        println!("  {short}  {title}  [{}]", s.cwd);
    }
    report_skipped();

    if args.dry_run {
        println!("Dry run: nothing created.");
        return Ok(());
    }

    if to_import.len() > 1 && !args.yes {
        use std::io::Write;
        print!("Import {} session(s)? [y/N] ", to_import.len());
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let group = args.group.clone().unwrap_or_default();
    let storage = Storage::open_unwatched(profile)?;
    let created_ids = storage.update(|all_instances, groups| {
        let mut ids = Vec::new();
        for s in &to_import {
            // Re-check under the lock so a concurrent import does not duplicate.
            if already_imported(all_instances, &s.session_id) {
                continue;
            }
            let inst = build_import_instance(s, structured, &group);
            ids.push(inst.id.clone());
            all_instances.push(inst.clone());
            if !inst.group_path.is_empty() {
                let mut tree = GroupTree::new_with_groups(all_instances, groups);
                tree.create_group(&inst.group_path);
                *groups = tree.get_all_groups();
            }
        }
        Ok(ids)
    })?;

    println!("✓ Imported {} session(s).", created_ids.len());

    if structured {
        if args.launch {
            println!("Note: --launch is ignored for structured imports.");
        }
        println!(
            "Structured sessions replay their transcript on the next `aoe serve` \
             (auto-spawned within ~2s while serve is running)."
        );
        return Ok(());
    }

    if args.launch {
        launch_imported(profile, &created_ids)?;
    } else if !created_ids.is_empty() {
        println!("Start them with `aoe session start <id>` (or launch on import with --launch).");
    }
    Ok(())
}

/// Start freshly imported terminal sessions, spawning each tmux pane. Mirrors
/// `start_session`'s three-phase pattern; failures are reported per session and
/// do not abort the rest.
fn launch_imported(profile: &str, ids: &[String]) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let file_watch = crate::file_watch::FileWatchService::noop();
    for id in ids {
        let (instances, _groups) = storage.load_with_groups()?;
        let Some(inst) = instances.iter().find(|i| &i.id == id) else {
            continue;
        };
        let mut working = inst.clone();
        working.source_profile = profile.to_string();
        // See `start_session`: a cleared sid whose rollout is still newest on
        // disk would be re-adopted by the drain below.
        let prior_sid = working.agent_session_id.clone();
        if let Err(e) = working.start_with_size(crate::terminal::get_size()) {
            eprintln!("Warning: failed to start {}: {e}", working.title);
            continue;
        }
        if working.agent_session_id.is_none() {
            if let Some(sid) = prior_sid {
                working.retroactive_capture_excludes.insert(sid);
            }
        }
        // Persist the poller-observed id before exit (see start_session).
        crate::session::sync::capture_launched_session_id_blocking(
            &mut working,
            &file_watch,
            crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
            true,
        );
        let wid = working.id.clone();
        storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|i| i.id == wid) {
                stored.merge_post_start(&working);
            }
            Ok(())
        })?;
        println!("✓ Started {}", working.title);
    }
    Ok(())
}

/// CLI handler for `aoe session stop`.
///
/// Treats a docker inspect failure ([`crate::containers::Probe::Unknown`])
/// as "possibly running" so the session stop proceeds rather than printing
/// "Session is not running" against a container whose state cannot be
/// confirmed. The `warn!` for the Unknown case is emitted inside
/// [`crate::session::Instance::stop`], so this call site does not re-warn.
async fn stop_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Resolve the identifier before the lifecycle-locked shutdown.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "stop")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();
    let session_id = inst.id.clone();
    let title = inst.title.clone();
    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;
    let was_running = tmux_session.exists();
    let had_container = inst.is_sandboxed()
        && match crate::containers::DockerContainer::from_session_id(&inst.id).probe_running() {
            crate::containers::Probe::Running | crate::containers::Probe::Unknown(_) => true,
            crate::containers::Probe::NotRunning => false,
        };

    if !was_running && !had_container {
        println!("Session is not running: {}", title);
        return Ok(());
    }

    working.stop()?;

    // `Instance::stop` persisted Stopped while still holding the lifecycle
    // lock. Only verify that a peer did not remove the row; writing status here
    // would race a restart that linearized after the stop.
    let landed = storage.load()?.iter().any(|stored| stored.id == session_id);
    if !landed {
        bail!(
            "Session {} was removed by another process before stop could land",
            title
        );
    }

    if had_container {
        println!("✓ Stopped session and container: {}", title);
    } else {
        println!("✓ Stopped session: {}", title);
    }

    Ok(())
}

async fn restart_session_dispatch(profile: &str, args: RestartArgs) -> Result<()> {
    if args.all {
        return restart_all_sessions(profile, args.parallel).await;
    }
    let identifier = args
        .identifier
        .ok_or_else(|| anyhow::anyhow!("session identifier required (or pass --all)"))?;
    restart_session(profile, SessionIdArgs { identifier }).await
}

async fn restart_all_sessions(profile: &str, parallel: usize) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the targets. We don't hold the flock
    // across the parallel restart fan-out below; phase 3 re-loads under
    // the lock and merges by id.
    let (instances, _groups) = storage.load_with_groups()?;
    let target_ids = pick_targets_for_restart_all(&instances);
    if target_ids.is_empty() {
        println!("No sessions to restart in profile '{}'.", profile);
        return Ok(());
    }

    let total = target_ids.len();
    let size = crate::terminal::get_size();
    let parallel = parallel.max(1);

    // Clone each target into its worker. `source_profile` is runtime-only
    // (skip_serializing) so storage-loaded instances always come back
    // blank; rehydrate it from the storage profile so start-time config
    // resolution honors the right profile's overrides (sandbox.environment,
    // on_launch hooks, etc.).
    let mut targets: Vec<crate::session::Instance> = Vec::with_capacity(total);
    for id in &target_ids {
        if let Some(inst) = instances.iter().find(|i| &i.id == id) {
            let mut clone = inst.clone();
            clone.source_profile = profile.to_string();
            targets.push(clone);
        }
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut join_set: tokio::task::JoinSet<(
        String,
        Option<crate::session::Instance>,
        Result<StartOutcome>,
    )> = tokio::task::JoinSet::new();

    // Phase 2 (unlocked): parallel tmux restarts.
    for mut inst in targets {
        let permit_sem = semaphore.clone();
        join_set.spawn(async move {
            let _permit = permit_sem
                .acquire_owned()
                .await
                .expect("semaphore not closed");
            let title = inst.title.clone();
            let res = tokio::task::spawn_blocking(move || {
                let prior_sid = inst.agent_session_id.clone();
                let result = inst.restart_with_size(size);
                // Drain the fresh poller so a fresh-relaunched capture-deferred
                // agent persists its new agent_session_id. No-op for Resumed /
                // ResumeFailed. In spawn_blocking: off the runtime, parallel,
                // bounded by the semaphore.
                if result.is_ok() {
                    if matches!(
                        result,
                        Ok(StartOutcome::Fresh) | Ok(StartOutcome::FreshAfterFailedResume { .. })
                    ) {
                        if let Some(sid) = prior_sid {
                            inst.retroactive_capture_excludes.insert(sid);
                        }
                    }
                    let file_watch = crate::file_watch::FileWatchService::noop();
                    crate::session::sync::capture_launched_session_id_blocking(
                        &mut inst,
                        &file_watch,
                        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
                        false,
                    );
                }
                (inst, result)
            })
            .await;
            match res {
                Ok((inst, result)) => (title, Some(inst), result),
                Err(join_err) => (
                    title,
                    None,
                    Err(anyhow::anyhow!("worker panicked: {}", join_err)),
                ),
            }
        });
    }

    let mut succeeded: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut fresh_after_failed_resume: Vec<(String, String)> = Vec::new();
    let mut restarted: Vec<crate::session::Instance> = Vec::new();
    while let Some(joined) = join_set.join_next().await {
        let (title, inst_opt, result) = joined.expect("JoinSet shouldn't panic on join itself");
        let id = inst_opt.as_ref().map(|i| i.id.clone()).unwrap_or_default();
        if let Some(inst) = inst_opt {
            restarted.push(inst);
        }
        match result {
            Ok(StartOutcome::ResumeFailed { sid }) => failed.push((
                title,
                format!("resume failed for sid {sid}; preserved for explicit retry"),
            )),
            Ok(StartOutcome::FreshAfterFailedResume { sid }) => {
                fresh_after_failed_resume.push((title.clone(), sid));
                succeeded.push((id, title));
            }
            Ok(StartOutcome::Resumed | StartOutcome::Fresh) => succeeded.push((id, title)),
            Err(e) => failed.push((title, e.to_string())),
        }
    }

    // Phase 3 (locked, fast): merge each restarted instance by id into the
    // freshly-loaded persisted state. Concurrent mutations to OTHER
    // sessions during phase 2 (status updates from a parallel daemon
    // poller, sibling CLI invocations, ...) are preserved because the
    // closure receives the latest disk state.
    let orphaned: Vec<(String, String)> = storage.update(|instances, _groups| {
        let mut orphaned = Vec::new();
        for restarted_inst in restarted {
            if let Some(stored) = instances.iter_mut().find(|i| i.id == restarted_inst.id) {
                stored.merge_post_restart(&restarted_inst);
            } else {
                tracing::warn!(
                    target: "session.cli",
                    session_id = %restarted_inst.id,
                    "session row removed by peer between phase 1 and phase 3 of restart --all; tmux session is now orphan"
                );
                orphaned.push((restarted_inst.id.clone(), restarted_inst.title.clone()));
            }
        }
        Ok(orphaned)
    })?;

    // Sessions can share a title across paths; orphan filter keys on id.
    let orphaned_ids: HashSet<&String> = orphaned.iter().map(|(id, _)| id).collect();
    succeeded.retain(|(id, _)| !orphaned_ids.contains(id));

    println!("✓ Restarted {}/{} sessions:", succeeded.len(), total);
    for (_id, title) in &succeeded {
        println!("  · {}", title);
    }
    if !fresh_after_failed_resume.is_empty() {
        println!(
            "ℹ {} started fresh (a prior resume attempt failed for the stored sid; the old conversation is still reachable via the agent's own resume/history picker):",
            fresh_after_failed_resume.len()
        );
        for (title, sid) in &fresh_after_failed_resume {
            println!("  · {}: sid {}", title, sid);
        }
    }
    if !orphaned.is_empty() {
        println!(
            "⚠ {} orphaned (row removed by peer mid-flight; tmux running but unrooted):",
            orphaned.len()
        );
        for (_, title) in &orphaned {
            println!("  · {}", title);
        }
    }
    if !failed.is_empty() {
        println!("✗ {} failed:", failed.len());
        for (title, err) in &failed {
            println!("  · {}: {}", title, err);
        }
        bail!("{} session(s) failed to restart", failed.len());
    }

    Ok(())
}

/// Sessions in `Deleting` or `Creating` are mid-transition; restarting them
/// would race the deletion/boot path. Acp-mode sessions are skipped
/// because their lifecycle is owned by `aoe serve`'s supervisor, not
/// tmux: a CLI-side restart would no-op silently and (with the explicit
/// bail in `restart_session`) flood `--all` with per-session errors.
/// Everything else is fair game; agents have their own resume-or-restart
/// logic on the next start.
fn pick_targets_for_restart_all(instances: &[crate::session::Instance]) -> Vec<String> {
    use crate::session::Status;
    instances
        .iter()
        .filter(|i| !matches!(i.status, Status::Deleting | Status::Creating))
        .filter(|i| !i.is_structured())
        .map(|i| i.id.clone())
        .collect()
}

async fn restart_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the target by identifier and
    // rehydrate `source_profile` for config resolution.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "restart")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();

    // Snapshot the sid before `restart_with_size` clears it on a forced-fresh
    // path: the abandoned rollout lingers and stays newest-by-mtime, so the
    // fresh poller can re-observe it. Excluded below so the drain rejects it.
    let prior_sid = working.agent_session_id.clone();

    // Restart orchestration owns its lifecycle locks and releases them while
    // user hooks run, so recursive same-id commands cannot deadlock.
    let outcome = working.restart_with_resume_policy(
        crate::terminal::get_size(),
        false,
        crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting,
    )?;
    let title = working.title.clone();
    let session_id = working.id.clone();
    let tool = working.tool.clone();

    // Resolve the configured wake message (global default with per-profile
    // override). Empty string is the documented opt-out: the restart still
    // runs but no keys are sent.
    let wake_msg = crate::session::resolve_config(profile)
        .map(|c| c.session.restart_wake_message.clone())
        .unwrap_or_else(|_| "wake up: pick up what you were doing".to_string());

    let mut wake_succeeded = false;
    if !wake_msg.is_empty() && !matches!(outcome, StartOutcome::ResumeFailed { .. }) {
        // Restart re-execs the agent at a blank prompt; nudge it back into
        // its prior task. Wait for a real readiness signal when one is
        // known for this agent, falling back to steady-state pane output
        // otherwise, so the keys land as soon as the agent is at a prompt
        // and don't get stranded mid-banner on slow machines.
        let tmux_session = crate::tmux::Session::new(&session_id, &title)?;
        tmux_session.wait_until_ready(
            std::time::Duration::from_secs(5),
            crate::agents::ready_marker(&tool),
        );

        if tmux_session.exists() {
            let delay = crate::agents::send_keys_enter_delay(&tool);
            match tmux_session.send_keys_with_delay(&wake_msg, delay) {
                Ok(()) => {
                    wake_succeeded = true;
                }
                Err(e) => {
                    eprintln!("Warning: failed to send wake-up message: {}", e);
                }
            }
        }
    }

    // Restart starts a fresh session-id poller; a capture-deferred agent that
    // relaunches fresh mints a new agent_session_id no CLI loop would drain.
    // Same drain as `session start`; no-op for Resumed (sid kept) and
    // ResumeFailed (poller cleared). After the wake wait, so it is usually ready.
    if matches!(
        outcome,
        StartOutcome::Fresh | StartOutcome::FreshAfterFailedResume { .. }
    ) {
        if let Some(sid) = prior_sid {
            working.retroactive_capture_excludes.insert(sid);
        }
    }
    let file_watch = crate::file_watch::FileWatchService::noop();
    crate::session::sync::capture_launched_session_id_blocking(
        &mut working,
        &file_watch,
        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
        true,
    );

    // Reacquire only for the final generation-aware merge. A newer peer
    // lifecycle wins instead of being overwritten by this snapshot.
    let _merge_lock = storage
        .acquire_instance_lifecycle_lock(&session_id)
        .context("failed to acquire instance restart merge lock")?;
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == session_id) {
            stored.merge_post_restart(&working);
            if wake_succeeded {
                stored.touch_last_accessed();
            }
            Ok(true)
        } else {
            tracing::warn!(
                target: "session.cli",
                session_id = %session_id,
                "session row removed by peer between phase 1 and phase 3 of restart; tmux session is now orphan"
            );
            Ok(false)
        }
    })?;
    if !landed {
        bail!(
            "Session {} was removed by another process before restart could land; tmux session is now orphan",
            title
        );
    }

    match outcome {
        StartOutcome::ResumeFailed { sid } => {
            bail!("Resume failed for sid {sid}; preserved for explicit retry");
        }
        StartOutcome::FreshAfterFailedResume { sid } => {
            println!(
                "✓ Restarted session: {} (started fresh; a prior resume attempt failed for sid {sid}, the old conversation is still reachable via the agent's own resume/history picker)",
                title
            );
        }
        StartOutcome::Resumed | StartOutcome::Fresh => {
            println!("✓ Restarted session: {}", title);
        }
    }
    Ok(())
}

async fn attach_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "attach")?;
    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;

    if !tmux_session.exists() {
        bail!(
            "Session is not running. Start it first with: aoe session start {}",
            args.identifier
        );
    }

    tmux_session.attach()?;
    Ok(())
}

async fn show_session(profile: &str, args: ShowArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let mut inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?.clone()
    } else {
        // Auto-detect from tmux
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
                .clone()
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };
    inst.source_profile = storage.profile().to_string();

    // Resolving the profile config installs the declarative status-rule
    // registry for this profile; the status detection below never loads config
    // itself, so a rules-having custom agent would otherwise report Idle.
    crate::session::config::profile_config::resolve_config_or_warn(profile);

    // Refresh status from tmux so the output reflects current state
    // rather than the stale persisted value.
    crate::tmux::refresh_session_cache();
    inst.update_status_once(None, None);
    let contended = crate::session::Instance::contended_capture_cwds(&instances);
    inst.self_heal_session_id(profile, &contended);

    if args.json {
        super::output::print_json(&session_details(&inst, storage.profile()))?;
    } else {
        println!("Session: {}", inst.title);
        println!("  ID:      {}", inst.id);
        println!("  Path:    {}", inst.project_path);
        println!("  Group:   {}", inst.group_path);
        println!("  Tool:    {}", inst.tool);
        println!("  Command: {}", inst.command);
        println!("  Status:  {:?}", inst.status);
        // Only for a session that is not live: an archived or trashed row is
        // otherwise indistinguishable here from a stopped one, and `status`
        // cannot carry it (a session can be archived and running at once).
        if let Some(at) = inst.trashed_at.or(inst.archived_at) {
            println!(
                "  State:   {} ({})",
                super::list::state_tag(&inst),
                at.to_rfc3339()
            );
        }
        println!("  Profile: {}", storage.profile());
        if let Some(parent_id) = &inst.parent_session_id {
            println!("  Parent:  {}", parent_id);
        }
    }

    Ok(())
}

async fn capture_session(profile: &str, args: CaptureArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    // Resolving the profile config installs the declarative status-rule
    // registry for this profile; the status detection below never loads config
    // itself, so a rules-having custom agent would otherwise report Idle.
    crate::session::config::profile_config::resolve_config_or_warn(profile);

    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;

    let (content, status) = if !tmux_session.exists() {
        (String::new(), "stopped".to_string())
    } else {
        let raw = tmux_session.capture_pane(args.lines)?;
        // The poller's two detection identities, resolved the same way: the
        // manifest follows the `agent_detect_as` alias, while configured rules
        // stay keyed to the session's own tool.
        let hook_alias =
            crate::tmux::status_rules::effective_detect_as(profile, &inst.tool, &inst.detect_as);
        let manifest_tool: &str = if hook_alias.is_empty() {
            &inst.tool
        } else {
            &hook_alias
        };
        let rules_tool =
            crate::tmux::status_rules::detection_tool(profile, &inst.tool, &inst.detect_as);
        let hook = crate::hooks::read_hook_status(&inst.id).map(|status| {
            crate::tmux::detect::HookObservation {
                status,
                age: crate::hooks::read_hook_status_age(&inst.id),
            }
        });
        // The same rule table the poller runs, so `aoe session capture`
        // reports what the dashboard would. A short `--lines` still detects
        // against the window the rules expect.
        let status = if crate::tmux::detect::has_manifest(manifest_tool) {
            let status_raw;
            let status_content = if args.lines >= 50 {
                raw.as_str()
            } else {
                status_raw = tmux_session
                    .capture_pane(50)
                    .unwrap_or_else(|_| raw.clone());
                status_raw.as_str()
            };
            // The pane title is a rule region like any other (Claude ranks it
            // above every screen shape), so it has to be read here too; the
            // poller gets it batched with the rest of the pane metadata.
            let osc_title = crate::tmux::utils::pane_title(tmux_session.name()).unwrap_or_default();
            crate::tmux::detect_with_rules(
                profile,
                &rules_tool,
                manifest_tool,
                &crate::tmux::utils::strip_ansi(status_content),
                &osc_title,
                hook,
            )
            .and_then(|d| d.status)
            .unwrap_or_default()
        } else {
            // Configured rules outrank the hook file here as they do in the
            // poller; `detect_status` runs them ahead of the built-in detector.
            let hook = hook.filter(|_| !crate::tmux::status_rules::has_rules(profile, &rules_tool));
            match hook {
                Some(hook) => hook.status,
                None => tmux_session
                    .detect_status(profile, &rules_tool)
                    .unwrap_or_default(),
            }
        };
        let content = if args.strip_ansi {
            crate::tmux::utils::strip_ansi(&raw)
        } else {
            raw
        };
        (content, format!("{:?}", status).to_lowercase())
    };

    if args.json {
        let output = CaptureOutput {
            id: inst.id.clone(),
            title: inst.title.clone(),
            status,
            tool: inst.tool.clone(),
            content,
            lines: args.lines,
        };
        super::output::print_json(&output)?;
    } else {
        print!("{}", content);
    }

    Ok(())
}

fn rename_success_message(
    persisted_old_title: &str,
    committed_title: &str,
    title_requested: bool,
) -> String {
    if title_requested && persisted_old_title != committed_title {
        format!("✓ Renamed session: {persisted_old_title} → {committed_title}")
    } else {
        format!("✓ Updated session: {committed_title}")
    }
}

async fn rename_session(profile: &str, args: RenameArgs) -> Result<()> {
    if args.title.is_none() && args.group.is_none() {
        bail!("At least one of --title or --group must be specified");
    }

    let storage = Storage::open_unwatched(profile)?;

    // Phase 1 (unlocked): resolve the target id (auto-detect from tmux if
    // no identifier given) and the old/new title pair so we can do the
    // tmux rename outside the storage flock.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?.clone()
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
    let title_requested = args.title.is_some();
    let session_lock_required = title_requested || args.rename_branch;

    // The initial load only resolves the requested row. Serialize every
    // identity-changing rename from the fresh duplicate check through external
    // effects and the durable commit. Existing-session mutations nest the
    // per-session title and source lifecycle guards beneath the identity lock.
    let _identity_lock = acquire_session_identity_lock()?;
    let _session_title_lock = if session_lock_required {
        Some(
            crate::session::acquire_session_title_lock(&id)
                .context("failed to acquire session title lock")?,
        )
    } else {
        None
    };
    let _lifecycle_lock = if session_lock_required {
        Some(
            storage
                .acquire_instance_lifecycle_lock(&id)
                .context("failed to acquire session lifecycle lock")?,
        )
    } else {
        None
    };
    let (authoritative_instances, _groups) = storage.load_with_groups()?;
    let inst = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
    // Heal a `project_path` left stale by an external `git worktree move`
    // before the duplicate-identity check and the container gate below derive
    // anything from it. The tied branch of this command moves the worktree, so
    // it needs the same repair as the standalone workdir edit; this is a fresh
    // process per invocation, so no startup sweep has run for it (#2002).
    let mut inst = inst.clone();
    if let Err(error) = crate::session::worktree_reconcile::reconcile_and_persist(
        &storage,
        &mut inst,
        &mut Default::default(),
    ) {
        tracing::warn!(target: "cli.session", session = %id, "worktree path reconciliation skipped: {error}");
    }
    let old_title = inst.title.clone();
    let effective_title = args
        .title
        .clone()
        .unwrap_or_else(|| old_title.clone())
        .trim()
        .to_string();
    let new_group = args.group.as_ref().map(|g| g.trim().to_string());
    let title_changed = old_title != effective_title;

    // Tied mode (#1927): renaming an aoe-managed worktree session also moves
    // its directory leaf to match the title (and optionally the branch), so
    // the two cannot drift. Decided per-session from the resolved setting.
    let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
    let tied = inst.tie_workdir_applies(config.session.tie_workdir_to_name);
    let tied_edit = tied && (args.title.is_some() || args.rename_branch);
    let duplicate_path = if tied_edit {
        crate::session::worktree_edit::derived_worktree_path(
            std::path::Path::new(&inst.project_path),
            &effective_title,
        )
    } else {
        inst.project_path.clone()
    };
    let pair_changed = title_changed
        || duplicate_path.trim_end_matches('/') != inst.project_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(
            authoritative_instances.iter(),
            &effective_title,
            &duplicate_path,
            Some(&id),
        )
    {
        return Err(duplicate_session_error(&effective_title));
    }

    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;
    if tied_edit {
        let current_path = inst.project_path.clone();
        let worktree_info = inst
            .worktree_info
            .clone()
            .expect("tie_workdir_applies implies worktree_info is Some");
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
        let moves_worktree = crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&current_path),
            &leaf,
        );
        let renames_branch = crate::session::worktree_edit::worktree_branch_rename_required(
            &worktree_info,
            &leaf,
            args.rename_branch,
        );
        let is_sandboxed = inst.is_sandboxed();
        if moves_worktree || renames_branch {
            // Persisted status can lag the live tmux pane; recompute only when
            // the request will mutate the checkout. A cwd/branch-stable title
            // no-op must remain valid for an active session.
            //
            // Deliberately the holding entry point rather than
            // `update_status_once`: a held proposal leaves the row on Running,
            // so an ambiguous frame refuses the edit instead of moving a
            // worktree out from under a live agent.
            let mut live = inst.clone();
            live.source_profile = profile.to_string();
            crate::tmux::refresh_session_cache();
            live.update_status_with_metadata(None, None);
            let container_holds = !live.status.blocks_worktree_edit()
                && moves_worktree
                && crate::session::worktree_edit::ensure_sandbox_container_released(
                    &id,
                    is_sandboxed,
                );
            if live.status.blocks_worktree_edit() || container_holds {
                bail!("Stop the session before renaming its worktree directory or branch. Disable session.tie_workdir_to_name to relabel a running session.");
            }
        }
        match crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info: &worktree_info,
                current_path: std::path::Path::new(&current_path),
                new_name: &leaf,
                rename_branch: args.rename_branch,
            },
        ) {
            Ok(outcome) => {
                // The dir moved (path changed): a sandbox container created
                // against the old path is now stale, so drop it to force a
                // fresh create on next start. A branch-only edit leaves the
                // path (and the mount) unchanged.
                if outcome.new_path != std::path::Path::new(&current_path) {
                    crate::session::worktree_edit::discard_sandbox_container_after_move(
                        &id,
                        is_sandboxed,
                    );
                }
                new_path = Some(outcome.new_path.to_string_lossy().to_string());
                new_branch = outcome.new_branch;
            }
            // The title slug maps to the current leaf and no branch rename was
            // requested: nothing to move, fall through to a plain title rename.
            Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
            Err(e) => return Err(e.into()),
        }
    } else if args.rename_branch {
        bail!("--rename-branch only applies to a tied aoe-managed worktree session (session.tie_workdir_to_name)");
    }

    // Persist before rekeying the live tmux session. Re-resolve by id under
    // the profile lock so concurrent mutations to other sessions are
    // preserved. `create_group` is idempotent and only runs when the closure
    // actually mutated `group_path`, so `groups.json` is rewritten only on
    // real group changes (cf. `update`'s diff check).
    let persist = storage.update(|instances, groups| {
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
        let persisted_old_title = inst.title.clone();
        if title_requested {
            inst.title = effective_title.clone();
        }
        if let Some(path) = &new_path {
            inst.project_path = path.clone();
        }
        if let Some(branch) = &new_branch {
            if let Some(wt) = inst.worktree_info.as_mut() {
                wt.branch = branch.clone();
            }
        }
        if let Some(group) = &new_group {
            inst.group_path = group.clone();
        }
        let committed_title = inst.title.clone();
        let group_path = inst.group_path.clone();
        if !group_path.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(instances, groups);
            group_tree.create_group(&group_path);
            *groups = group_tree.get_all_groups();
        }
        Ok((persisted_old_title, committed_title))
    });
    let (persisted_old_title, committed_title) = match persist {
        Ok(titles) => titles,
        Err(error) => {
            // When the git move already landed, surface that the disk and
            // metadata are out of sync rather than a bare persist error.
            if let Some(path) = &new_path {
                bail!("Worktree was moved on disk to {path}, but persisting the new session metadata failed: {error}. Re-run to retry.");
            }
            return Err(error);
        }
    };
    // Storage::update durably commits the identity and publishes its file-watch
    // notification. Rekey needs only the per-session title/lifecycle guards.
    drop(_identity_lock);

    let committed_title_changed = title_requested && persisted_old_title != committed_title;
    if committed_title_changed {
        let rekey_id = id.clone();
        let rekey_old_title = persisted_old_title.clone();
        let rekey_new_title = committed_title.clone();
        match tokio::task::spawn_blocking(move || {
            crate::tmux::rekey_session(&rekey_id, &rekey_old_title, &rekey_new_title)
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("Warning: failed to rename tmux session: {error}"),
            Err(error) => eprintln!("Warning: tmux rename task failed: {error}"),
        }
    }

    if let Some(path) = &new_path {
        println!("✓ Worktree moved to: {}", path);
        if let Some(branch) = &new_branch {
            println!("  Branch renamed to: {}", branch);
        }
    }
    println!(
        "{}",
        rename_success_message(&persisted_old_title, &committed_title, title_requested,)
    );

    Ok(())
}

#[cfg(test)]
mod rename_tests {
    use super::{rename_session, rename_success_message, RenameArgs};
    use crate::session::{Instance, Status, Storage};
    use serial_test::serial;

    // Three duplicate-identity behaviors kept in one test on purpose: they
    // share the costly setup that forces `#[serial]` (an isolated app dir plus
    // a process-global `tie_workdir_to_name` config flip), so splitting them
    // into three serial tests would triple that setup and the critical-path
    // serial time for no added coverage. Each behavior asserts independently.
    #[tokio::test]
    #[serial]
    async fn rename_rejects_duplicate_pair_but_allows_group_only_change() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("rename-duplicate").unwrap();
        let existing = Instance::new("main branch", "/tmp/repo/");
        let target = Instance::new("throwaway", "/tmp/repo");
        let target_id = target.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![existing, target];
                Ok(())
            })
            .unwrap();

        let error = rename_session(
            "rename-duplicate",
            RenameArgs {
                identifier: Some(target_id.clone()),
                title: Some("main branch".to_string()),
                group: None,
                rename_branch: false,
            },
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Session already exists with same title and path"));

        rename_session(
            "rename-duplicate",
            RenameArgs {
                identifier: Some(target_id.clone()),
                title: None,
                group: Some("work".to_string()),
                rename_branch: false,
            },
        )
        .await
        .unwrap();

        let instances = storage.load().unwrap();
        let target = instances
            .iter()
            .find(|instance| instance.id == target_id)
            .unwrap();
        assert_eq!(target.title, "throwaway");
        assert_eq!(target.group_path, "work");
        // In tied mode the duplicate identity uses the derived destination
        // path, not the row's current worktree path. The collision must reject
        // before any git side effect is attempted.
        let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
        let mut tied = Instance::new("main branch", "/tmp/worktrees/drifted");
        tied.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "drifted".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let tied_id = tied.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![existing, tied];
                Ok(())
            })
            .unwrap();

        let error = rename_session(
            "rename-duplicate",
            RenameArgs {
                identifier: Some(tied_id.clone()),
                title: Some("main branch".to_string()),
                group: None,
                rename_branch: false,
            },
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Session already exists with same title and path"));
        let tied = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == tied_id)
            .unwrap();
        assert_eq!(tied.title, "main branch");
        assert_eq!(tied.project_path, "/tmp/worktrees/drifted");

        // An explicit unchanged title still enters tied mode so a drifted
        // directory can be repaired. When the directory and branch are already
        // stable, however, it is a true no-op and must remain valid even if the
        // persisted session is active.
        let mut active = Instance::new("Main Branch", "/tmp/worktrees/main-branch");
        active.status = Status::Running;
        active.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "main-branch".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let active_id = active.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![active];
                Ok(())
            })
            .unwrap();

        rename_session(
            "rename-duplicate",
            RenameArgs {
                identifier: Some(active_id.clone()),
                title: Some("Main Branch".to_string()),
                group: None,
                rename_branch: false,
            },
        )
        .await
        .expect("active cwd-stable title no-op must succeed");
        let active = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == active_id)
            .unwrap();
        assert_eq!(active.title, "Main Branch");
        assert_eq!(active.project_path, "/tmp/worktrees/main-branch");
    }

    #[test]
    fn group_only_success_uses_authoritative_committed_title() {
        assert_eq!(
            rename_success_message("stale resolver title", "peer committed title", false),
            "✓ Updated session: peer committed title"
        );
    }
}

async fn set_worktree_name(profile: &str, args: SetWorktreeNameArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());
        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
    let _identity_lock = acquire_session_identity_lock()?;
    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire worktree rename lifecycle lock")?;
    let authoritative_instances = storage.load()?;
    let inst = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
    // The recorded path can be stale: someone may have `git worktree move`d the
    // directory outside aoe. Heal it from git before anything below derives a
    // target path or a container gate from it, so those decisions are made
    // against the live parent. Best-effort; a lookup failure just leaves the
    // stale path, which `edit_worktree_workdir` then rejects as before (#2002).
    let mut inst = inst.clone();
    if let Err(error) = crate::session::worktree_reconcile::reconcile_and_persist(
        &storage,
        &mut inst,
        &mut Default::default(),
    ) {
        tracing::warn!(target: "cli.session", session = %id, "worktree path reconciliation skipped: {error}");
    }
    let current_path = inst.project_path.clone();
    let Some(worktree_info) = inst.worktree_info.clone() else {
        bail!("Session does not use a worktree");
    };
    // When tied (#1927) the directory follows the title, so reject the
    // standalone edit and point at the unified rename instead.
    if inst.tie_workdir_applies(
        crate::session::config::profile_config::resolve_config_or_warn(profile)
            .session
            .tie_workdir_to_name,
    ) {
        bail!("Renaming is unified while session.tie_workdir_to_name is on; use 'aoe session rename --title <name>' instead, and the worktree directory follows. Disable the setting to edit the directory independently.");
    }
    let duplicate_path = crate::session::worktree_edit::target_worktree_path(
        std::path::Path::new(&current_path),
        args.name.trim(),
    )
    .unwrap_or_else(|| std::path::PathBuf::from(&current_path))
    .to_string_lossy()
    .into_owned();
    if duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/')
        && is_duplicate_session(
            authoritative_instances.iter(),
            &inst.title,
            &duplicate_path,
            Some(&id),
        )
    {
        return Err(duplicate_session_error(&inst.title));
    }
    // Persisted status can lag the real tmux pane, and moving the worktree of
    // a still-running session is unsafe. Recompute from live tmux state before
    // enforcing the guard. The holding entry point, for the reason
    // `rename_session` gives: an ambiguous frame must refuse the move.
    let mut live = inst.clone();
    crate::tmux::refresh_session_cache();
    live.update_status_with_metadata(None, None);
    // A sandbox container keeps the worktree dir mounted even while the agent
    // is Idle, so the move would fail. The gate drops a merely-stopped
    // container to free the mount and only reports held for a live one, which
    // the user has to stop, same as the active-status case. Gated on the
    // directory actually moving so a no-op or branch-only edit does not discard
    // a container for a move that never happens.
    let moves_worktree = crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&current_path),
        args.name.trim(),
    );
    if live.status.blocks_worktree_edit()
        || (moves_worktree
            && crate::session::worktree_edit::ensure_sandbox_container_released(
                &id,
                live.is_sandboxed(),
            ))
    {
        bail!("Cannot edit the workdir name while the session is active; stop it first");
    }

    let outcome = crate::session::worktree_edit::edit_worktree_workdir(
        crate::session::worktree_edit::WorktreeEditRequest {
            worktree_info: &worktree_info,
            current_path: std::path::Path::new(&current_path),
            new_name: args.name.trim(),
            rename_branch: args.rename_branch,
        },
    )?;
    // The dir moved (path changed): a sandbox container created against the old
    // path is now stale, so drop it to force a fresh create on next start. A
    // branch-only edit leaves the path (and the mount) unchanged.
    if outcome.new_path != std::path::Path::new(&current_path) {
        crate::session::worktree_edit::discard_sandbox_container_after_move(
            &id,
            live.is_sandboxed(),
        );
    }
    let new_path = outcome.new_path.to_string_lossy().to_string();
    let new_branch = outcome.new_branch.clone();

    storage
        .update(|instances, _groups| {
            let inst = instances
                .iter_mut()
                .find(|i| i.id == id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
            inst.project_path = new_path.clone();
            if let Some(branch) = &new_branch {
                if let Some(wt) = inst.worktree_info.as_mut() {
                    wt.branch = branch.clone();
                }
            }
            Ok(())
        })
        .map_err(|e| {
            anyhow::anyhow!(
                "Worktree was moved on disk to {new_path}, but persisting the new session metadata failed: {e}. Re-run to retry."
            )
        })?;
    drop(_identity_lock);

    println!("✓ Worktree moved to: {}", new_path);
    if let Some(branch) = &new_branch {
        println!("  Branch renamed to: {}", branch);
    }
    Ok(())
}

async fn current_session(args: CurrentArgs) -> Result<()> {
    // Auto-detect profile and session from tmux
    let current_session = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|_| crate::tmux::get_current_session_name());

    let session_name = current_session.ok_or_else(|| anyhow::anyhow!("Not in a tmux session"))?;

    // Search all profiles for this session
    let profiles = crate::session::list_profiles()?;

    for profile_name in &profiles {
        if let Ok(storage) = Storage::open_unwatched(profile_name) {
            if let Ok((instances, _)) = storage.load_with_groups() {
                if let Some(inst) = instances
                    .iter()
                    .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                {
                    if args.json {
                        #[derive(Serialize)]
                        struct CurrentInfo {
                            session: String,
                            profile: String,
                            id: String,
                        }
                        let info = CurrentInfo {
                            session: inst.title.clone(),
                            profile: profile_name.clone(),
                            id: inst.id.clone(),
                        };
                        super::output::print_json(&info)?;
                    } else if args.quiet {
                        println!("{}", inst.title);
                    } else {
                        println!("Session: {}", inst.title);
                        println!("Profile: {}", profile_name);
                        println!("ID:      {}", inst.id);
                    }
                    return Ok(());
                }
            }
        }
    }

    bail!("Current tmux session is not an Agent of Empires session")
}

async fn set_session_id(profile: &str, args: SetSessionIdArgs) -> Result<()> {
    let new_intent = if args.session_id.trim().is_empty() {
        crate::session::ResumeIntent::Cleared
    } else {
        let trimmed = args.session_id.trim().to_string();
        if !crate::session::is_valid_session_id(&trimmed) {
            bail!(
                "Invalid session ID {:?}: must be 1-256 ASCII alphanumeric, dash, underscore, or dot characters",
                trimmed
            );
        }
        crate::session::ResumeIntent::Use(trimmed)
    };

    let storage = Storage::open_unwatched(profile)?;
    let target_id = {
        let instances = storage.load()?;
        super::resolve_session(&args.identifier, &instances)?
            .id
            .clone()
    };
    let lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&target_id)
        .context("failed to acquire instance resume-target lock")?;
    let (title, tool) = storage.update(|instances, _groups| {
        super::patch_instance(instances, &target_id, |inst| {
            if inst.is_structured() {
                anyhow::bail!(
                    "cannot set resume target on structured view-mode session '{}'; structured view manages its own conversation lifecycle via ACP",
                    inst.title
                );
            }
            inst.resume_intent = new_intent.clone();
            inst.resume_probe_failed_sid = None;
            Ok((inst.title.clone(), inst.tool.clone()))
        })
    })?;
    drop(lifecycle_lock);

    match &new_intent {
        crate::session::ResumeIntent::Use(id) => {
            println!("✓ Set resume target for '{}': {}", title, id);
            if let Some(agent) = crate::agents::get_agent(&tool) {
                if matches!(
                    agent.resume_strategy,
                    crate::agents::ResumeStrategy::Unsupported
                ) {
                    eprintln!(
                        "Warning: session resume is disabled for {} in AoE; this ID will be stored but not used.",
                        tool
                    );
                }
            }
        }
        crate::session::ResumeIntent::Cleared => {
            println!(
                "✓ Cleared resume intent for '{}' (next launches will be fresh)",
                title
            );
        }
        crate::session::ResumeIntent::Default | crate::session::ResumeIntent::Fork { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

/// `aoe session add-project <session> <path|name>`. See #3103.
///
/// Attaching converts the session into a multi-repo workspace, so unless it
/// already is one its working directory moves. That means stopping it, doing the
/// conversion, and starting it again, which is what
/// `attach_project::quiesce_for_conversion` / `resume_after_conversion` do for
/// every blocking surface. A live structured worker is signalled through the
/// same restart marker `aoe acp restart` uses, because the supervisor lives in
/// `aoe serve`; the marker is written after the persist so the daemon cannot
/// respawn the worker into the directory the conversion is moving.
async fn add_project(profile: &str, args: AddProjectArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let instances = storage.load()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();
    let is_sandboxed = inst.is_sandboxed();

    // Attaching restarts the agent, so a turn in flight would lose its reply (or,
    // in `Waiting`, a pending approval). The daemon refuses this on the
    // event-log probe; the CLI has no handle on that store, so it uses the status
    // set `blocks_worktree_edit` encodes for exactly this class of operation. The
    // unambiguous states (Creating, Deleting, trashed, archived) are refused in
    // `attach_project::plan`, shared with every other surface.
    if inst.status.blocks_worktree_edit() {
        bail!(
            "'{title}' has a turn in flight and attaching restarts the agent. Wait for it to \
             finish, or stop the session first."
        );
    }

    // A path-shaped argument is used as-is; a bare name is a registry lookup,
    // matching how `aoe add --projects` resolves its extras.
    let repo_path = if std::path::Path::new(&args.project).exists()
        || args.project.contains(std::path::MAIN_SEPARATOR)
    {
        std::path::PathBuf::from(&args.project)
    } else {
        let resolved =
            crate::session::projects::resolve_names(profile, std::slice::from_ref(&args.project))?;
        match resolved.into_iter().next() {
            Some(p) => std::path::PathBuf::from(p.path),
            None => bail!("Project '{}' is not in the registry", args.project),
        }
    };

    let on_existing = if args.attach_existing_branch {
        crate::session::attach_project::ExistingBranch::Attach
    } else {
        crate::session::attach_project::ExistingBranch::Refuse
    };

    // Validate before stopping anything: a refusal here must not cost the user a
    // stopped session.
    let plan = crate::session::attach_project::plan(inst, profile, &repo_path, on_existing)?;
    let restarts = crate::session::attach_project::needs_restart(&plan, is_sandboxed);
    let quiesced = if restarts {
        println!("Stopping '{title}' so its working directory can move...");
        crate::session::attach_project::quiesce_for_conversion(&storage, inst)?
    } else {
        crate::session::attach_project::Quiesced::default()
    };

    let outcome = match crate::session::attach_project::attach_planned(&storage, &id, inst, plan) {
        Ok(outcome) => outcome,
        Err(e) => {
            // The rollback already undid the filesystem half; bringing the
            // session back is the only thing left that would otherwise persist.
            crate::session::attach_project::resume_after_conversion(&storage, &id, quiesced);
            return Err(e);
        }
    };

    println!("Attached '{}' to session '{}'", outcome.repo.name, title);
    println!("  Worktree: {}", outcome.repo.worktree_path);
    println!(
        "  Branch:   {} ({})",
        outcome.repo.branch,
        if outcome.repo.branch_preexisting {
            "existing, aoe will not delete it"
        } else {
            "created"
        }
    );
    if let Some(moved_to) = &outcome.moved_to {
        println!("  Workspace: {moved_to}");
        println!(
            "  This session is now a multi-repo workspace; its working directory moved to the \
             path above."
        );
    }
    for warning in &outcome.warnings {
        println!("  Warning:  {warning}");
    }

    if restarts {
        if quiesced.worker_was_running {
            println!("Restarting the agent so it comes up with the new repo; the conversation is preserved.");
        } else {
            println!("Restarting the session so it comes up with the new repo.");
        }
    } else {
        println!("The agent is already working in this directory, so nothing was restarted.");
    }
    for warning in crate::session::attach_project::resume_after_conversion(&storage, &id, quiesced)
    {
        println!("  Warning:  {warning}");
    }

    Ok(())
}

async fn set_base(profile: &str, args: SetBaseArgs) -> Result<()> {
    if !args.clear && args.branch.is_none() {
        bail!("Provide a branch ref or pass --clear to remove the override.");
    }
    let storage = Storage::open_unwatched(profile)?;
    let instances = storage.load()?;

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();

    // Each repo in a workspace has its own base, so the target has to be
    // named. Validating and writing against the first repo, as this used to,
    // set a ref the other repos may not even have. See #3329.
    let target = resolve_base_target(inst, args.repo.as_deref())?;

    let new_value = if args.clear {
        None
    } else {
        let trimmed = args.branch.as_deref().unwrap_or("").trim().to_string();
        if trimmed.is_empty() {
            bail!("Branch name is empty. Pass --clear to remove the override.");
        }
        if let Err(e) =
            crate::git::diff::validate_ref(std::path::Path::new(&target.validate_path), &trimmed)
        {
            bail!(
                "Branch '{}' does not resolve in {}: {}",
                trimmed,
                target.validate_path,
                e
            );
        }
        Some(trimmed)
    };

    let repo_name = target.repo_name.clone();
    storage.update(|instances, _groups| {
        let stored = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", args.identifier))?;
        match repo_name.as_deref() {
            // The target was resolved from the copy loaded above, so a repo
            // missing here means the session changed under us (the workspace
            // was converted, the repo detached). Fail instead of dropping the
            // write and printing success.
            Some(name) => {
                let repo = stored
                    .workspace_info
                    .as_mut()
                    .and_then(|ws| ws.repos.iter_mut().find(|r| r.name == name))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Repo '{}' is no longer part of this session; nothing was changed",
                            name
                        )
                    })?;
                repo.base_branch_override = new_value.clone();
            }
            None => stored.base_branch_override = new_value.clone(),
        }
        Ok(())
    })?;

    // Quoted here rather than in the format strings below: leaving the quotes
    // out there and relying on the label to supply its own middle pair reads as
    // unbalanced at a glance, even though it composes correctly.
    let label = match target.repo_name {
        Some(ref name) => format!("'{title}' / '{name}'"),
        None => format!("'{title}'"),
    };
    match new_value {
        Some(ref v) => println!("✓ Set diff base for {}: {}", label, v),
        None => println!("✓ Cleared diff base override for {}", label),
    }
    Ok(())
}

/// Which entry a `set-base` invocation writes, and the checkout its ref is
/// validated against.
#[derive(Debug)]
struct BaseTarget {
    /// Workspace repo name, or None for the session's own checkout.
    repo_name: Option<String>,
    validate_path: String,
}

/// Resolve `--repo` against a session. A workspace session must name one of
/// its repos; a single-repo session must not name any, since the only entry
/// it has is its own checkout.
fn resolve_base_target(inst: &crate::session::Instance, repo: Option<&str>) -> Result<BaseTarget> {
    let names = || {
        inst.all_repos()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match repo {
        Some(name) => match inst.all_repos().iter().find(|r| r.name == name) {
            Some(r) => Ok(BaseTarget {
                repo_name: Some(r.name.clone()),
                validate_path: r.worktree_path.clone(),
            }),
            None if inst.all_repos().is_empty() => bail!(
                "This session has no workspace repos, so --repo does not apply. Drop it to set \
                 the session's own diff base."
            ),
            None => bail!("Unknown repo '{}'. This session has: {}", name, names()),
        },
        None if inst.workspace_info.is_some() => bail!(
            "This session is a multi-repo workspace, and each repo has its own diff base.\nPass \
             --repo <name> to pick one. Available: {}",
            names()
        ),
        None => Ok(BaseTarget {
            repo_name: None,
            validate_path: inst.project_path.clone(),
        }),
    }
}

#[cfg(test)]
mod restart_args_tests {
    use super::SessionCommands;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: SessionCommands,
    }

    #[test]
    fn restart_with_identifier_still_parses() {
        let cli = Cli::try_parse_from(["aoe", "restart", "claude-3"])
            .expect("identifier-only must parse");
        match cli.cmd {
            SessionCommands::Restart(args) => {
                assert!(!args.all);
                assert_eq!(args.identifier.as_deref(), Some("claude-3"));
                assert_eq!(args.parallel, 3);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    /// Refusing an existing branch is the default, because a same-named branch
    /// in another repo can hold unrelated commits.
    #[test]
    fn add_project_parses_its_identifier_project_and_branch_opt_in() {
        let cases = [
            (vec!["aoe", "add-project", "claude-3", "../frontend"], false),
            (
                vec![
                    "aoe",
                    "add-project",
                    "claude-3",
                    "../frontend",
                    "--attach-existing-branch",
                ],
                true,
            ),
        ];
        for (argv, attach_existing) in cases {
            let cli = Cli::try_parse_from(&argv).expect("add-project must parse");
            match cli.cmd {
                SessionCommands::AddProject(args) => {
                    assert_eq!(args.identifier, "claude-3");
                    assert_eq!(args.project, "../frontend");
                    assert_eq!(args.attach_existing_branch, attach_existing, "{argv:?}");
                }
                _ => panic!("wrong subcommand"),
            }
        }
    }

    #[test]
    fn restart_all_alone_parses() {
        let cli = Cli::try_parse_from(["aoe", "restart", "--all"]).expect("--all alone must parse");
        match cli.cmd {
            SessionCommands::Restart(args) => {
                assert!(args.all);
                assert!(args.identifier.is_none());
                assert_eq!(args.parallel, 3);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn restart_all_with_parallel_parses() {
        let cli = Cli::try_parse_from(["aoe", "restart", "--all", "--parallel", "5"])
            .expect("--all --parallel must parse");
        match cli.cmd {
            SessionCommands::Restart(args) => {
                assert!(args.all);
                assert_eq!(args.parallel, 5);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn restart_identifier_and_all_conflicts() {
        let result = Cli::try_parse_from(["aoe", "restart", "claude-3", "--all"]);
        assert!(
            result.is_err(),
            "passing both identifier and --all should error"
        );
    }

    #[test]
    fn set_base_with_branch_parses() {
        let cli = Cli::try_parse_from(["aoe", "set-base", "claude-3", "upstream/main"])
            .expect("set-base with branch must parse");
        match cli.cmd {
            SessionCommands::SetBase(args) => {
                assert_eq!(args.identifier, "claude-3");
                assert_eq!(args.branch.as_deref(), Some("upstream/main"));
                assert!(!args.clear);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn set_base_with_clear_parses() {
        let cli = Cli::try_parse_from(["aoe", "set-base", "claude-3", "--clear"])
            .expect("set-base --clear must parse");
        match cli.cmd {
            SessionCommands::SetBase(args) => {
                assert_eq!(args.identifier, "claude-3");
                assert!(args.branch.is_none());
                assert!(args.clear);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn set_base_branch_and_clear_conflicts() {
        let result = Cli::try_parse_from(["aoe", "set-base", "claude-3", "main", "--clear"]);
        assert!(
            result.is_err(),
            "passing both branch and --clear should error"
        );
    }
}

#[cfg(test)]
mod set_base_target_tests {
    use super::resolve_base_target;
    use crate::session::{Instance, WorkspaceInfo, WorkspaceRepo};

    fn workspace_instance() -> Instance {
        let mut inst = Instance::new("ws", "/ws");
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: "/ws".to_string(),
            repos: ["api", "web"]
                .iter()
                .map(|n| WorkspaceRepo {
                    name: n.to_string(),
                    source_path: format!("/src/{n}"),
                    branch: "feature/x".to_string(),
                    worktree_path: format!("/ws/{n}"),
                    main_repo_path: format!("/src/{n}"),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                })
                .collect(),
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });
        inst
    }

    #[test]
    fn resolves_named_repo_and_validates_against_its_own_worktree() {
        let inst = workspace_instance();
        let t = resolve_base_target(&inst, Some("web")).expect("named repo resolves");
        assert_eq!(t.repo_name.as_deref(), Some("web"));
        // The bug this replaces validated every ref against repos[0].
        assert_eq!(t.validate_path, "/ws/web");
    }

    #[test]
    fn rejects_unknown_repo_and_missing_repo_on_a_workspace() {
        let inst = workspace_instance();
        let err = resolve_base_target(&inst, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("api, web"),
            "should list the repos, got: {err}"
        );

        let err = resolve_base_target(&inst, None).unwrap_err().to_string();
        assert!(
            err.contains("--repo") && err.contains("api, web"),
            "should demand a repo and list them, got: {err}"
        );
    }

    #[test]
    fn single_repo_session_targets_its_own_checkout() {
        let inst = Instance::new("solo", "/tmp/solo");
        let t = resolve_base_target(&inst, None).expect("single repo resolves");
        assert_eq!(t.repo_name, None);
        assert_eq!(t.validate_path, "/tmp/solo");

        let err = resolve_base_target(&inst, Some("api"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no workspace repos"),
            "should explain --repo does not apply, got: {err}"
        );
    }
}

#[cfg(test)]
mod target_filter_tests {
    use super::pick_targets_for_restart_all;
    use crate::session::{Instance, Status};

    fn instance_with_status(id: &str, status: Status) -> Instance {
        let mut inst = Instance::new(id, "/tmp");
        inst.id = id.to_string();
        inst.status = status;
        inst
    }

    #[test]
    fn skips_deleting_and_creating() {
        let instances = vec![
            instance_with_status("running", Status::Running),
            instance_with_status("idle", Status::Idle),
            instance_with_status("stopped", Status::Stopped),
            instance_with_status("error", Status::Error),
            instance_with_status("waiting", Status::Waiting),
            instance_with_status("starting", Status::Starting),
            instance_with_status("unknown", Status::Unknown),
            instance_with_status("deleting", Status::Deleting),
            instance_with_status("creating", Status::Creating),
        ];
        let mut picked = pick_targets_for_restart_all(&instances);
        picked.sort();
        let mut expected = vec![
            "error".to_string(),
            "idle".to_string(),
            "running".to_string(),
            "starting".to_string(),
            "stopped".to_string(),
            "unknown".to_string(),
            "waiting".to_string(),
        ];
        expected.sort();
        assert_eq!(picked, expected);
    }

    #[test]
    fn empty_input_yields_empty_targets() {
        assert!(pick_targets_for_restart_all(&[]).is_empty());
    }
}

#[cfg(test)]
mod set_session_id_tests {
    use super::{set_session_id, SetSessionIdArgs};
    use crate::session::{Instance, ResumeIntent, Storage};
    use serial_test::serial;
    use tempfile::tempdir;

    #[tokio::test]
    #[serial]
    async fn set_session_id_clears_resume_probe_failed_marker() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = Storage::new_unwatched("set-sid-clear-marker").unwrap();
        let mut inst = Instance::new("marked_session", "/tmp/x");
        inst.agent_session_id = Some("11111111-1111-1111-1111-111111111111".to_string());
        inst.resume_probe_failed_sid = Some("11111111-1111-1111-1111-111111111111".to_string());
        let id = inst.id.clone();
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        set_session_id(
            "set-sid-clear-marker",
            SetSessionIdArgs {
                identifier: id.clone(),
                session_id: "22222222-2222-2222-2222-222222222222".to_string(),
            },
        )
        .await
        .unwrap();

        let loaded = storage.load().unwrap();
        let inst_disk = loaded.iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            inst_disk.resume_intent,
            ResumeIntent::Use("22222222-2222-2222-2222-222222222222".to_string())
        );
        assert_eq!(inst_disk.resume_probe_failed_sid, None);
    }
}

#[cfg(test)]
mod set_color_tests {
    use super::{set_color_session, SetColorArgs};
    use crate::session::{Instance, Storage};
    use serial_test::serial;
    use tempfile::tempdir;

    async fn seed(profile: &str) -> (Storage, String) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let inst = Instance::new("color_session", "/tmp/x");
        let id = inst.id.clone();
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();
        (storage, id)
    }

    #[tokio::test]
    #[serial]
    async fn set_color_persists_palette_value_and_clears() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let (storage, id) = seed("set-color-ok").await;

        set_color_session(
            "set-color-ok",
            SetColorArgs {
                identifier: id.clone(),
                color: "Red".to_string(), // case-insensitive
            },
        )
        .await
        .unwrap();
        let loaded = storage.load().unwrap();
        assert_eq!(
            loaded.iter().find(|i| i.id == id).unwrap().color.as_deref(),
            Some("red")
        );

        set_color_session(
            "set-color-ok",
            SetColorArgs {
                identifier: id.clone(),
                color: "none".to_string(),
            },
        )
        .await
        .unwrap();
        let loaded = storage.load().unwrap();
        assert_eq!(loaded.iter().find(|i| i.id == id).unwrap().color, None);
    }

    #[tokio::test]
    #[serial]
    async fn set_color_rejects_unknown_color() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let (storage, id) = seed("set-color-bad").await;

        let result = set_color_session(
            "set-color-bad",
            SetColorArgs {
                identifier: id.clone(),
                color: "chartreuse".to_string(),
            },
        )
        .await;
        assert!(result.is_err(), "unknown color must error");
        // The rejected write must not have touched disk.
        let loaded = storage.load().unwrap();
        assert_eq!(loaded.iter().find(|i| i.id == id).unwrap().color, None);
    }
}

#[cfg(test)]
mod acp_reject_tests {
    use super::{set_session_id, SetSessionIdArgs};
    use crate::session::{Instance, Storage};
    use serial_test::serial;
    use tempfile::tempdir;

    #[tokio::test]
    #[serial]
    async fn set_session_id_rejects_structured_view_session() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = Storage::new_unwatched("acp-reject").unwrap();
        let mut inst = Instance::new("acp_session", "/tmp/x");
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        let result = set_session_id(
            "acp-reject",
            SetSessionIdArgs {
                identifier: id.clone(),
                session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            },
        )
        .await;

        let err = result.expect_err("set-session-id must reject structured view-mode sessions");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("acp"),
            "error must mention structured view: {}",
            msg
        );

        let loaded = storage.load().unwrap();
        let inst_disk = loaded.iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            inst_disk.resume_intent,
            crate::session::ResumeIntent::Default,
            "rejected call must not mutate intent",
        );
        assert_eq!(
            inst_disk.agent_session_id, None,
            "rejected call must not mutate sid",
        );
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::session::claude_import::ClaudeSessionSummary;

    fn summary(id: &str, cwd: &str, title: Option<&str>) -> ClaudeSessionSummary {
        ClaudeSessionSummary {
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            title: title.map(str::to_string),
            last_modified_ms: 0,
            cwd_exists: true,
        }
    }

    #[test]
    fn terminal_import_pins_resume_target() {
        let s = summary("abc123-def456", "/home/me/proj", Some("Fix bug"));
        let inst = build_import_instance(&s, false, "");
        assert_eq!(inst.tool, "claude");
        assert_eq!(inst.project_path, "/home/me/proj");
        assert_eq!(inst.title, "Fix bug");
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use("abc123-def456".to_string())
        );
    }

    #[test]
    fn title_falls_back_to_short_id() {
        let s = summary("abcdef12-3456-7890", "/home/me/proj", None);
        let inst = build_import_instance(&s, false, "team/imports");
        assert_eq!(inst.title, "Claude import abcdef12");
        assert_eq!(inst.group_path, "team/imports");
    }

    #[test]
    fn structured_import_seeds_replay_fields() {
        let s = summary("sid-1", "/home/me/proj", Some("x"));
        let inst = build_import_instance(&s, true, "");
        assert!(inst.is_structured());
        assert_eq!(inst.acp_session_id.as_deref(), Some("sid-1"));
        assert_eq!(inst.import_pending, Some(true));
        // Structured imports do not pin a terminal resume target.
        assert_eq!(inst.resume_intent, ResumeIntent::Default);
    }

    #[test]
    fn already_imported_matches_resume_and_observed_ids() {
        let mut by_resume = Instance::new("a", "/p");
        by_resume.resume_intent = ResumeIntent::Use("id-1".to_string());
        let mut by_observed = Instance::new("b", "/p");
        by_observed.agent_session_id = Some("id-2".to_string());
        let fresh = Instance::new("c", "/p");
        let instances = vec![by_resume, by_observed, fresh];

        assert!(already_imported(&instances, "id-1"));
        assert!(already_imported(&instances, "id-2"));
        assert!(!already_imported(&instances, "id-3"));
    }
}

#[cfg(test)]
mod show_json_tests {
    use super::*;

    /// #3350 gave `aoe list --json` a `state` tag and both timestamps;
    /// `session show --json` was left behind, so a scripted consumer that
    /// looked a session up by id could not tell an archived one from a live
    /// one without a second `aoe list` shellout.
    #[test]
    fn show_json_exposes_state_and_archived_at_for_an_archived_row() {
        let mut inst = Instance::new("z", "/repo");
        inst.archive();
        let details = session_details(&inst, "p");
        assert_eq!(details.state, "archived");
        assert!(details.archived_at.is_some());
        assert!(details.trashed_at.is_none());
    }

    /// `trash()` deliberately leaves `archived_at` alone so a restore is
    /// faithful, so both keys can be present at once and `state` reports
    /// `trashed`: the same precedence `list --json` reports, from the same
    /// `state_tag`.
    #[test]
    fn show_json_reports_trashed_for_a_row_that_was_archived_first() {
        let mut inst = Instance::new("z", "/repo");
        inst.archive();
        inst.trash();
        let details = session_details(&inst, "p");
        assert_eq!(details.state, "trashed");
        assert!(details.trashed_at.is_some());
        assert!(details.archived_at.is_some());
    }

    /// A live row must serialize exactly as wide as it did before, so a
    /// consumer parsing today's output sees one new key (`state`) and no
    /// `null` timestamps.
    #[test]
    fn show_json_omits_absent_timestamps_and_keeps_state_live() {
        let inst = Instance::new("z", "/repo");
        let serialized = serde_json::to_string(&session_details(&inst, "p")).unwrap();
        assert!(!serialized.contains("trashed_at"), "{serialized}");
        assert!(!serialized.contains("archived_at"), "{serialized}");
        assert!(serialized.contains("\"state\":\"live\""), "{serialized}");
    }

    /// #3415: same four-timestamp mirror as `aoe list --json`, gated the
    /// same way: the snooze key surfaces only while `is_snoozed()` holds,
    /// the pin key whenever set, neither appears on a plain row, and
    /// `state` stays the untouched bucket tag throughout.
    #[test]
    fn show_json_mirrors_the_api_snooze_and_pin_keys() {
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
            let value = serde_json::to_value(session_details(inst, "p")).unwrap();
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
        let active = serde_json::to_value(session_details(&snoozed, "p")).unwrap();
        assert_eq!(
            active["snoozed_until"],
            serde_json::to_value(future).unwrap()
        );
    }
}
