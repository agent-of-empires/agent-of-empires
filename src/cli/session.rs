//! `agent-of-empires session` subcommands implementation

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use std::collections::HashSet;

use crate::session::{GroupTree, StartOutcome, Storage};

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

    /// Set the resume target for a session (pin a conversation or force a
    /// one-shot fresh start)
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

    /// Mark a session as a favorite. Favorited rows pin to the top of
    /// their status tier in the Attention sort and render with a leading
    /// `* ` glyph plus bold + underline.
    Favorite(SessionIdArgs),

    /// Clear the favorite flag on a session.
    Unfavorite(SessionIdArgs),

    /// Archive a session: sink it in the Attention sort and tear down its
    /// tmux sessions. Worktree, branch, container preserved. `--no-kill`
    /// skips tmux teardown. See #1868.
    Archive(ArchiveArgs),

    /// Unarchive a session (restores it to its tier in the Attention sort)
    Unarchive(SessionIdArgs),

    /// Restore a trashed session, returning it to its prior bucket with its
    /// transcript and metadata intact. See #2489.
    Restore(SessionIdArgs),

    /// List the sessions currently in the trash.
    ListTrash,

    /// Permanently purge every trashed session in the profile (irreversible).
    EmptyTrash,
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
    /// Resume target: a UUID/sid pins the next launches to that
    /// conversation; an empty string forces a one-shot fresh start (after
    /// which the system reverts to auto-resume).
    session_id: String,
}

#[derive(Args)]
pub struct SetBaseArgs {
    /// Session ID or title
    pub identifier: String,
    /// Branch ref to diff against (short name like `main` or
    /// remote-qualified like `upstream/main`). Required unless
    /// `--clear` is passed.
    pub branch: Option<String>,
    /// Clear the override and fall back to the profile default /
    /// auto-detected base.
    #[arg(long, conflicts_with = "branch")]
    pub clear: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    profile: String,
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
        SessionCommands::SetBase(args) => set_base(profile, args).await,
        SessionCommands::Snooze(args) => snooze_session(profile, args).await,
        SessionCommands::Unsnooze(args) => unsnooze_session(profile, args).await,
        SessionCommands::Favorite(args) => favorite_session(profile, args).await,
        SessionCommands::Unfavorite(args) => unfavorite_session(profile, args).await,
        SessionCommands::Archive(args) => archive_session(profile, args).await,
        SessionCommands::Unarchive(args) => unarchive_session(profile, args).await,
        SessionCommands::Restore(args) => restore_session(profile, args).await,
        SessionCommands::ListTrash => list_trash(profile).await,
        SessionCommands::EmptyTrash => empty_trash(profile).await,
    }
}

async fn favorite_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
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
    let storage = Storage::new_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unfavorite();
            Ok(inst.title.clone())
        })
    })?;
    println!("Unfavorited: {}", title);
    Ok(())
}

async fn archive_session(profile: &str, args: ArchiveArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): resolve identifier.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();
    let inst = inst.clone();

    // Phase 2 (unlocked): tmux work. Agent kill split from ancillary so
    // the CLI prints a warn on agent failure. #1868.
    if !args.no_kill {
        if let Err(e) = inst.kill() {
            eprintln!("Warning: failed to kill agent tmux session: {}", e);
        }
        inst.kill_ancillary_tmux_sessions();
    }

    // Phase 3 (locked, fast): set archived_at by id.
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.archive();
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
    let storage = Storage::new_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        let id = super::resolve_session(&args.identifier, instances)?
            .id
            .clone();
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .expect("resolve_session returned an id that is no longer in instances");
        inst.unarchive();
        Ok(inst.title.clone())
    })?;
    println!("Unarchived: {}", title);
    Ok(())
}

async fn restore_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

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

    // Move the worktree back to its pre-trash location before flipping the
    // marker. Strict: if the original path is occupied or git refuses, leave
    // the session trashed and surface the error rather than restoring it to
    // the holding-area path.
    if let crate::session::trash::RestoreOutcome::Failed { reason } =
        crate::session::trash::restore_worktree_location(&mut inst)
    {
        anyhow::bail!("Cannot restore worktree: {reason}");
    }
    let restored_path = inst.project_path.clone();
    let restored_pre = inst.pre_trash_project_path.clone();

    let title = storage.update(|instances, _groups| {
        let stored = instances
            .iter_mut()
            .find(|i| i.id == restore_id)
            .ok_or_else(|| anyhow::anyhow!("No trashed session matching '{}'", args.identifier))?;
        stored.project_path = restored_path.clone();
        stored.pre_trash_project_path = restored_pre.clone();
        stored.untrash();
        Ok(stored.title.clone())
    })?;
    println!("Restored: {}", title);
    Ok(())
}

async fn list_trash(profile: &str) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
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
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the trashed sessions and run the slow
    // teardown for each. Purge is permanent; force removal so a dirty
    // worktree cannot keep an emptied session pinned in the trash.
    let (instances, _groups) = storage.load_with_groups()?;
    let trashed: Vec<_> = instances
        .iter()
        .filter(|i| i.is_trashed())
        .cloned()
        .collect();
    if trashed.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }

    let mut purged_ids = Vec::new();
    for inst in &trashed {
        let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
            profile,
            std::path::Path::new(&inst.project_path),
        );
        let delete_worktree =
            config.worktree.auto_cleanup && inst.has_managed_worktree_or_workspace();
        // Tie branch deletion to worktree deletion + config so it also fires
        // for multi-repo workspace sessions (which have no `worktree_info`);
        // `perform_deletion` keys the workspace-repo branch cleanup off this
        // same flag. See #2489.
        let delete_branch = delete_worktree && config.worktree.delete_branch_on_cleanup;
        let delete_sandbox =
            inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) && config.sandbox.auto_cleanup;

        let result = crate::session::deletion::perform_deletion(
            &crate::session::deletion::DeletionRequest {
                session_id: inst.id.clone(),
                instance: inst.clone(),
                delete_worktree,
                delete_branch,
                delete_sandbox,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
            },
        );
        for err in &result.errors {
            eprintln!("Warning ({}): {}", inst.title, err);
        }
        // Only after teardown succeeded: purge the durable structured-view
        // transcript (the daemon does this via the supervisor; the CLI opens
        // the event store directly since it has no live worker) and drop the
        // session row. Doing the irreversible transcript delete last keeps a
        // failed purge fully restorable, and keeping the row on failure (here
        // or in perform_deletion) lets the orphaned worktree/container/
        // transcript be retried instead of abandoned. See #2489.
        if result.success {
            match super::purge_acp_transcript(inst) {
                Ok(()) => purged_ids.push(inst.id.clone()),
                Err(e) => eprintln!(
                    "Warning ({}): transcript not purged, keeping session in trash: {}",
                    inst.title, e
                ),
            }
        }
    }

    // Phase 2 (locked): drop every successfully-purged id from the latest disk
    // state. #2534: revalidate under the lock; a candidate restored mid-purge
    // (no longer trashed) must survive even though its teardown already ran on
    // the snapshot. #2527: report the count actually removed, not the candidate
    // count, plus how many were kept (teardown/transcript failed, or restored).
    let purged_set: HashSet<String> = purged_ids.into_iter().collect();
    let candidate_ids: HashSet<String> = trashed.iter().map(|i| i.id.clone()).collect();
    // Compute `kept` from candidate rows that are STILL present after the purge,
    // not `candidates - removed`: a candidate a peer already removed before this
    // lock is neither removed by us nor still around, so subtracting would
    // wrongly report it as kept for retry.
    let (removed, restored, kept) = storage.update(|all_instances, _groups| {
        let (removed, restored) = super::apply_empty_trash_purge(all_instances, &purged_set);
        let kept = all_instances
            .iter()
            .filter(|i| candidate_ids.contains(&i.id))
            .count();
        Ok((removed, restored, kept))
    })?;
    if restored > 0 {
        eprintln!(
            "Warning: {restored} session(s) were restored while the trash was being \
             emptied; kept the restored records, but their worktree, branch, container, \
             or transcript may already have been removed. Inspect and repair them."
        );
    }
    if kept > 0 {
        println!(
            "Emptied trash: purged {removed} session(s), kept {kept} for retry, from profile '{}'.",
            storage.profile()
        );
    } else {
        println!(
            "Emptied trash: purged {removed} session(s) from profile '{}'.",
            storage.profile()
        );
    }
    Ok(())
}

async fn snooze_session(profile: &str, args: SnoozeArgs) -> Result<()> {
    let config = crate::session::profile_config::resolve_config(profile)?;

    // `--minutes` overrides the profile default; otherwise use the
    // configured `snooze_duration_minutes`. Validate either way so the
    // on-disk config can't sneak in an out of range value.
    let raw_minutes = args
        .minutes
        .map(|m| m as u64)
        .unwrap_or(config.session.snooze_duration_minutes as u64);
    crate::session::validate_snooze_duration(raw_minutes).map_err(|e| anyhow::anyhow!("{}", e))?;
    let minutes = raw_minutes as u32;

    let storage = Storage::new_unwatched(profile)?;
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
    let storage = Storage::new_unwatched(profile)?;
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
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the target by identifier, rehydrate
    // `source_profile` so config resolution honors the right profile.
    // `source_profile` is runtime-only (skip_serializing) so storage-loaded
    // instances always come back blank.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "start")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();

    // Phase 2 (unlocked): tmux work happens outside the cross-process flock
    // so a slow agent startup does not block peer mutators on the same
    // profile (daemon poller, sibling CLI invocations).
    working.start_with_size(crate::terminal::get_size())?;
    let title = working.title.clone();
    let id = working.id.clone();

    // Phase 3 (locked, fast): merge the post-start instance back by id, so
    // any concurrent mutation to OTHER sessions during phase 2 is preserved.
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
///
/// `structured_view` is gated behind the `serve` feature; without it the
/// field doesn't exist on `Instance` and no session can be in structured view
/// mode, so this is a no-op shim.
#[cfg(feature = "serve")]
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

#[cfg(not(feature = "serve"))]
fn bail_if_acp(_inst: &crate::session::Instance, _verb: &str) -> Result<()> {
    Ok(())
}

async fn stop_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): resolve identifier, do tmux/container shutdown.
    // Loaded snapshot is read-only here; the persistence happens in phase 2.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "stop")?;
    let session_id = inst.id.clone();
    let title = inst.title.clone();
    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;
    let was_running = tmux_session.exists();
    let had_container = inst.is_sandboxed()
        && crate::containers::DockerContainer::from_session_id(&inst.id)
            .is_running()
            .unwrap_or(false);

    if !was_running && !had_container {
        println!("Session is not running: {}", title);
        return Ok(());
    }

    inst.stop()?;

    // Phase 2 (locked): persist Stopped status by id so it survives TUI
    // restarts. Field-level merge preserves any concurrent mutation that
    // landed between phase 1 and phase 2.
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == session_id) {
            stored.status = crate::session::Status::Stopped;
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
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
    let storage = Storage::new_unwatched(profile)?;

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
                let result = inst.restart_with_size(size);
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
        .filter(|_i| {
            #[cfg(feature = "serve")]
            {
                !_i.is_structured()
            }
            #[cfg(not(feature = "serve"))]
            {
                true
            }
        })
        .map(|i| i.id.clone())
        .collect()
}

async fn restart_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): snapshot the target by identifier and
    // rehydrate `source_profile` for config resolution.
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "restart")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();

    // Phase 2 (unlocked): tmux restart, agent boot, optional wake-up
    // send-keys. Slow; the cross-process flock is not held here so peer
    // mutators on this profile are not starved.
    let outcome = working.restart_with_size(crate::terminal::get_size())?;
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
        // its prior task. Poll capture-pane for steady-state output instead
        // of a blind sleep, so the keys land as soon as the agent is at a
        // prompt and don't get stranded mid-banner on slow machines.
        wait_for_pane_ready(&session_id, &title, std::time::Duration::from_secs(5)).await;

        let tmux_session = crate::tmux::Session::new(&session_id, &title)?;
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

    // touch_last_accessed runs on `stored`, not `working`: its fields are
    // peer-mutable and do not belong in `merge_post_restart`.
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

/// Poll the tmux pane until capture-pane content stops changing for two
/// consecutive samples (the agent has finished printing its startup banner
/// and is sitting at a prompt) or `max_wait` elapses. Failsafe: always
/// returns by `max_wait` so the caller's send-keys still runs even if the
/// pane never settles.
async fn wait_for_pane_ready(session_id: &str, title: &str, max_wait: std::time::Duration) {
    let Ok(tmux) = crate::tmux::Session::new(session_id, title) else {
        return;
    };
    let poll_interval = std::time::Duration::from_millis(200);
    let start = std::time::Instant::now();
    let mut last: Option<String> = None;
    while start.elapsed() < max_wait {
        tokio::time::sleep(poll_interval).await;
        let Ok(now) = tmux.capture_pane(5) else {
            continue;
        };
        if now.trim().len() > 20 {
            if last.as_deref() == Some(&now) {
                return;
            }
            last = Some(now);
        }
    }
}

async fn attach_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
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
    let storage = Storage::new_unwatched(profile)?;
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
                .find(|i| {
                    let tmux_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                    tmux_name == session_name
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
                .clone()
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    // Refresh status from tmux so the output reflects current state
    // rather than the stale persisted value.
    crate::tmux::refresh_session_cache();
    inst.update_status();

    if args.json {
        let details = SessionDetails {
            id: inst.id.clone(),
            title: inst.title.clone(),
            path: inst.project_path.clone(),
            group: inst.group_path.clone(),
            tool: inst.tool.clone(),
            command: inst.command.clone(),
            status: format!("{:?}", inst.status).to_lowercase(),
            parent_session_id: inst.parent_session_id.clone(),
            profile: storage.profile().to_string(),
        };
        super::output::print_json(&details)?;
    } else {
        println!("Session: {}", inst.title);
        println!("  ID:      {}", inst.id);
        println!("  Path:    {}", inst.project_path);
        println!("  Group:   {}", inst.group_path);
        println!("  Tool:    {}", inst.tool);
        println!("  Command: {}", inst.command);
        println!("  Status:  {:?}", inst.status);
        println!("  Profile: {}", storage.profile());
        if let Some(parent_id) = &inst.parent_session_id {
            println!("  Parent:  {}", parent_id);
        }
    }

    Ok(())
}

async fn capture_session(profile: &str, args: CaptureArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
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
                .find(|i| {
                    let tmux_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                    tmux_name == session_name
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;

    let (content, status) = if !tmux_session.exists() {
        (String::new(), "stopped".to_string())
    } else {
        let raw = tmux_session.capture_pane(args.lines)?;
        let detection_tool = if inst.detect_as.is_empty() {
            &inst.tool
        } else {
            &inst.detect_as
        };
        let status = if let Some(hook_status) = crate::hooks::read_hook_status(&inst.id) {
            if detection_tool == "codex" && hook_status == crate::session::Status::Running {
                let status_raw;
                let status_content = if args.lines >= 50 {
                    raw.as_str()
                } else {
                    status_raw = tmux_session
                        .capture_pane(50)
                        .unwrap_or_else(|_| raw.clone());
                    status_raw.as_str()
                };
                crate::tmux::reconcile_codex_hook_status(hook_status, status_content)
            } else {
                hook_status
            }
        } else {
            tmux_session
                .detect_status(detection_tool)
                .unwrap_or_default()
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

async fn rename_session(profile: &str, args: RenameArgs) -> Result<()> {
    if args.title.is_none() && args.group.is_none() {
        bail!("At least one of --title or --group must be specified");
    }

    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): resolve the target id (auto-detect from tmux if
    // no identifier given) and the old/new title pair so we can do the
    // tmux rename outside the storage flock.
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
                .find(|i| {
                    let tmux_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                    tmux_name == session_name
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
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
    let config = crate::session::profile_config::resolve_config_or_warn(profile);
    let tied = inst.tie_workdir_applies(config.session.tie_workdir_to_name);

    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;
    if tied && (title_changed || args.rename_branch) {
        let current_path = inst.project_path.clone();
        let worktree_info = inst
            .worktree_info
            .clone()
            .expect("tie_workdir_applies implies worktree_info is Some");
        // Persisted status can lag the live tmux pane; moving a running
        // worktree is unsafe, so recompute before enforcing the gate.
        let mut live = inst.clone();
        crate::tmux::refresh_session_cache();
        live.update_status();
        // A sandbox session's container keeps the worktree dir mounted even
        // while the agent is Idle, so `git worktree move` would fail with
        // EBUSY; stopping the session tears the container down and releases it.
        if live.status.blocks_worktree_edit()
            || crate::session::worktree_edit::sandbox_container_holds_worktree(
                &id,
                live.is_sandboxed(),
            )
        {
            bail!("Stop the session before renaming it: its worktree directory moves to match the new name. Disable session.tie_workdir_to_name to relabel a running session.");
        }
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
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
                        live.is_sandboxed(),
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

    // Phase 2 (unlocked): tmux rename if the title changed. Side effect on
    // the running tmux server, fast but external state, do it outside the
    // closure.
    if title_changed {
        let tmux_session = crate::tmux::Session::new(&id, &old_title)?;
        if tmux_session.exists() {
            let new_tmux_name = crate::tmux::Session::generate_name(&id, &effective_title);
            if let Err(e) = tmux_session.rename(&new_tmux_name) {
                eprintln!("Warning: failed to rename tmux session: {}", e);
            } else {
                crate::tmux::refresh_session_cache();
            }
        }
    }

    // Phase 3 (locked): persist the new title and (optional) new group.
    // Re-resolve by id under the lock so concurrent mutations to other
    // sessions are preserved. `create_group` is idempotent and only runs
    // when the closure actually mutated `group_path`, so `groups.json` is
    // rewritten only on real group changes (cf. `update`'s diff check).
    let persist = storage.update(|instances, groups| {
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
        inst.title = effective_title.clone();
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
        let group_path = inst.group_path.clone();
        if !group_path.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(instances, groups);
            group_tree.create_group(&group_path);
            *groups = group_tree.get_all_groups();
        }
        Ok(())
    });
    if let Err(e) = persist {
        // When the git move already landed, surface that the disk and metadata
        // are out of sync rather than a bare persist error.
        if let Some(path) = &new_path {
            bail!("Worktree was moved on disk to {path}, but persisting the new session metadata failed: {e}. Re-run to retry.");
        }
        return Err(e);
    }

    if let Some(path) = &new_path {
        println!("✓ Worktree moved to: {}", path);
        if let Some(branch) = &new_branch {
            println!("  Branch renamed to: {}", branch);
        }
    }
    if title_changed {
        println!("✓ Renamed session: {} → {}", old_title, effective_title);
    } else {
        println!("✓ Updated session: {}", effective_title);
    }

    Ok(())
}

async fn set_worktree_name(profile: &str, args: SetWorktreeNameArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
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
                .find(|i| {
                    let tmux_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                    tmux_name == session_name
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
    let current_path = inst.project_path.clone();
    let Some(worktree_info) = inst.worktree_info.clone() else {
        bail!("Session does not use a worktree");
    };
    // When tied (#1927) the directory follows the title, so reject the
    // standalone edit and point at the unified rename instead.
    if inst.tie_workdir_applies(
        crate::session::profile_config::resolve_config_or_warn(profile)
            .session
            .tie_workdir_to_name,
    ) {
        bail!("Renaming is unified while session.tie_workdir_to_name is on; use 'aoe session rename --title <name>' instead, and the worktree directory follows. Disable the setting to edit the directory independently.");
    }
    // Persisted status can lag the real tmux pane, and moving the worktree of
    // a still-running session is unsafe. Recompute from live tmux state before
    // enforcing the guard.
    let mut live = inst.clone();
    crate::tmux::refresh_session_cache();
    live.update_status();
    // A sandbox container keeps the worktree dir mounted even while the agent
    // is Idle, so the move would fail with EBUSY; stopping the session releases
    // the mount, same as the active-status case.
    if live.status.blocks_worktree_edit()
        || crate::session::worktree_edit::sandbox_container_holds_worktree(&id, live.is_sandboxed())
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
        if let Ok(storage) = Storage::new_unwatched(profile_name) {
            if let Ok((instances, _)) = storage.load_with_groups() {
                if let Some(inst) = instances.iter().find(|i| {
                    let tmux_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                    tmux_name == session_name
                }) {
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

    let storage = Storage::new_unwatched(profile)?;
    let (title, tool) = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            #[cfg(feature = "serve")]
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

    match &new_intent {
        crate::session::ResumeIntent::Use(id) => {
            println!("✓ Set resume target for '{}': {}", title, id);
            if let Some(agent) = crate::agents::get_agent(&tool) {
                if matches!(
                    agent.resume_strategy,
                    crate::agents::ResumeStrategy::Unsupported
                ) {
                    eprintln!("Warning: {} does not support session resume; this ID will be stored but not used.", tool);
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

async fn set_base(profile: &str, args: SetBaseArgs) -> Result<()> {
    if !args.clear && args.branch.is_none() {
        bail!("Provide a branch ref or pass --clear to remove the override.");
    }
    let storage = Storage::new_unwatched(profile)?;
    let instances = storage.load()?;

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();

    let new_value = if args.clear {
        None
    } else {
        let trimmed = args.branch.as_deref().unwrap_or("").trim().to_string();
        if trimmed.is_empty() {
            bail!("Branch name is empty. Pass --clear to remove the override.");
        }
        let validate_path = inst
            .workspace_info
            .as_ref()
            .and_then(|w| w.repos.first().map(|r| r.worktree_path.clone()))
            .unwrap_or_else(|| inst.project_path.clone());
        if let Err(e) =
            crate::git::diff::validate_ref(std::path::Path::new(&validate_path), &trimmed)
        {
            bail!(
                "Branch '{}' does not resolve in {}: {}",
                trimmed,
                validate_path,
                e
            );
        }
        Some(trimmed)
    };

    storage.update(|instances, _groups| {
        let stored = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", args.identifier))?;
        stored.base_branch_override = new_value.clone();
        Ok(())
    })?;

    match new_value {
        Some(ref v) => println!("✓ Set diff base for '{}': {}", title, v),
        None => println!("✓ Cleared diff base override for '{}'", title),
    }
    Ok(())
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

#[cfg(all(test, feature = "serve"))]
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
