//! `agent-of-empires remove` command implementation

use anyhow::Result;
use clap::Args;

use crate::acp::client::{require_daemon, HttpClient};
use crate::daemon::{
    DeleteSessionBody, PurgeOutcome, RuntimeHealth, SessionResponse, TrashRelocationOutcome,
    TrashSessionBody, RUNTIME_PROTOCOL_VERSION,
};

#[derive(Args)]
pub struct RemoveArgs {
    /// Session ID or title to remove
    identifier: String,

    /// With --purge, delete the worktree directory (default: keep it).
    ///
    /// Without --purge, trashing moves a managed worktree into a sibling
    /// `.aoe-trash` folder, unless another session uses it.
    #[arg(long = "delete-worktree")]
    delete_worktree: bool,

    /// Delete git branch after worktree removal (default: per config)
    #[arg(long = "delete-branch")]
    delete_branch: bool,

    /// Force worktree removal even with untracked/modified files
    #[arg(long)]
    force: bool,

    /// Keep container instead of deleting it (default: delete per config)
    #[arg(long = "keep-container")]
    keep_container: bool,

    /// For scratch sessions, keep the scratch directory on disk instead of
    /// removing it. The session record is still deleted; the kept path is
    /// logged so you can find the files later. No effect on non-scratch
    /// sessions.
    #[arg(long = "keep-scratch")]
    keep_scratch: bool,

    /// Permanently delete instead of moving to trash.
    ///
    /// By default `rm` moves the session to the trash (when
    /// `session.delete_to_trash` is enabled, the default) so it can be
    /// restored; `--purge` forces the irreversible
    /// teardown (worktree/branch/container cleanup per the other flags) and
    /// removes the session's structured-view transcript. Removing the sandbox
    /// container also attempts to remove its private agent stores, including
    /// its config home; keeping the container keeps those stores. Host agent
    /// conversation history outside these stores is not removed.
    #[arg(long)]
    purge: bool,
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: RemoveArgs) -> Result<()> {
    let client = HttpClient::new(require_daemon().await?)?;
    let profile = super::send::resolve_profile(&client, profile).await?;
    let mut sessions = client.list_sessions(None).await?.sessions;
    sessions.retain(|s| s.profile == profile);
    let session = super::resolve_session(&args.identifier, &sessions)?;

    let daemon = client.daemon_client().await?;
    let info = daemon.runtime_info().await?;
    anyhow::ensure!(
        info.protocol_version == RUNTIME_PROTOCOL_VERSION && info.health == RuntimeHealth::Healthy,
        "Daemon runtime is not ready"
    );

    let title = &session.title;
    let id = &session.id;
    if session.cleanup_defaults.delete_to_trash && !args.purge {
        let receipt = daemon
            .trash_session(id, &TrashSessionBody::default(), &info.epoch)
            .await?;
        match receipt.outcome.relocation {
            TrashRelocationOutcome::Relocated {
                warning: Some(warning),
            } => eprintln!("  Warning: {warning}"),
            TrashRelocationOutcome::Failed { reason } => {
                eprintln!("  Note: left worktree in place ({reason}).")
            }
            _ => {}
        }
        println!("  Moved session to trash: {title} (from profile '{profile}')");
        println!(
            "  Restore with `aoe session restore {id}`, or delete permanently with `aoe rm --purge {id}`."
        );
        return Ok(());
    }

    let body = purge_body(session, &args)?;
    let outcome = daemon.purge_session(id, &body, &info.epoch).await?.outcome;
    match outcome {
        PurgeOutcome::Kept {
            messages,
            teardown_started: false,
        } => anyhow::bail!("{}: {title}; nothing changed", messages.join("; ")),
        PurgeOutcome::Kept { messages, .. } => {
            for msg in &messages {
                println!("  {msg}");
            }
            eprintln!(
                "Warning: session {title} was restored while its purge was running; kept the \
                 restored record, but its worktree, branch, container, or transcript may \
                 already have been removed by the purge. Inspect and repair it."
            );
            return Ok(());
        }
        PurgeOutcome::Deleted {
            messages,
            cleanup_errors,
        } => {
            for msg in &messages {
                println!("  {msg}");
            }
            for err in &cleanup_errors {
                eprintln!("Warning: {err}");
            }
        }
    }

    if !body.delete_worktree && session.has_cleanable_worktree {
        println!(
            "Worktree preserved at: {} (use --delete-worktree to remove)",
            session.project_path
        );
    }
    if session.is_sandboxed {
        if args.keep_container {
            println!("Container preserved (--keep-container)");
        } else if !body.delete_sandbox {
            println!("Container preserved (auto_cleanup disabled in config)");
        }
    }
    println!("  Removed session: {title} (from profile '{profile}')");
    Ok(())
}

/// A trashed row carries its own consent so a concurrent restore is refused.
fn purge_body(session: &SessionResponse, args: &RemoveArgs) -> Result<DeleteSessionBody> {
    let defaults = &session.cleanup_defaults;
    let delete_worktree = args.delete_worktree && session.has_cleanable_worktree;
    let mut body = DeleteSessionBody {
        delete_worktree,
        delete_branch: session.has_cleanable_worktree
            && (args.delete_branch || (delete_worktree && defaults.delete_branch)),
        delete_sandbox: session.is_sandboxed && !args.keep_container && defaults.delete_sandbox,
        force_delete: args.force,
        keep_scratch: args.keep_scratch,
        ..Default::default()
    };
    if let (Some(trashed_at), Some(lifecycle_id)) =
        (&session.trashed_at, &session.trash_lifecycle_id)
    {
        body.expected_trash = true;
        body.expected_trashed_at =
            Some(chrono::DateTime::parse_from_rfc3339(trashed_at)?.with_timezone(&chrono::Utc));
        body.expected_trash_lifecycle_id = Some(lifecycle_id.clone());
        body.expected_generation = Some(session.lifecycle_generation);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(extra: serde_json::Value) -> SessionResponse {
        let mut base = serde_json::json!({
            "id": "abc", "title": "t", "project_path": "/p",
            "has_cleanable_worktree": true, "is_sandboxed": true,
            "cleanup_defaults": {
                "delete_worktree": true, "delete_branch": true,
                "delete_sandbox": true, "delete_to_trash": true
            }
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn args(f: impl FnOnce(&mut RemoveArgs)) -> RemoveArgs {
        let mut a = RemoveArgs {
            identifier: "x".into(),
            delete_worktree: false,
            delete_branch: false,
            force: false,
            keep_container: false,
            keep_scratch: false,
            purge: true,
        };
        f(&mut a);
        a
    }

    #[test]
    fn purge_body_resolves_cleanup_flags() {
        let live = row(serde_json::json!({}));
        // Default keeps the worktree and its branch; config only drives the sandbox.
        let b = purge_body(&live, &args(|_| {})).unwrap();
        assert!(!b.delete_worktree && !b.delete_branch && b.delete_sandbox);
        // --delete-worktree pulls the branch in via the config default.
        let b = purge_body(&live, &args(|a| a.delete_worktree = true)).unwrap();
        assert!(b.delete_worktree && b.delete_branch);
        // --keep-container wins over the config default.
        let b = purge_body(&live, &args(|a| a.keep_container = true)).unwrap();
        assert!(!b.delete_sandbox);
        // No managed worktree: nothing to delete regardless of flags.
        let plain = row(serde_json::json!({"has_cleanable_worktree": false}));
        let b = purge_body(
            &plain,
            &args(|a| {
                a.delete_worktree = true;
                a.delete_branch = true;
            }),
        )
        .unwrap();
        assert!(!b.delete_worktree && !b.delete_branch);
    }

    #[test]
    fn purge_body_of_trashed_row_carries_consent() {
        let trashed = row(serde_json::json!({
            "trashed_at": "2026-01-02T03:04:05Z",
            "trash_lifecycle_id": "life-1",
            "lifecycle_generation": 7
        }));
        let b = purge_body(&trashed, &args(|_| {})).unwrap();
        assert!(b.expected_trash);
        assert_eq!(b.expected_generation, Some(7));
        assert_eq!(b.expected_trash_lifecycle_id.as_deref(), Some("life-1"));
        assert!(b.expected_trashed_at.is_some());
    }
}
