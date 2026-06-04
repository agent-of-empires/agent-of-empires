//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::cli::session::stale_history_suffix;
use crate::session::{EnsureReadyError, EnsureReadyOutcome, Storage};

#[derive(Args)]
pub struct SendArgs {
    /// Session ID or title
    identifier: String,

    /// Message to send to the agent
    message: String,

    /// Fail loud on dead/stopped sessions instead of auto-respawning. Default
    /// behavior is to revive the session so a `send` after a crash or stop
    /// just works; pass this for scripts that want the previous bail-out.
    #[arg(long = "no-revive")]
    no_revive: bool,
}

#[tracing::instrument(target = "cli.send", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: SendArgs) -> Result<()> {
    let storage = Storage::new(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    if args.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let session_id = inst.id.clone();
    let session_title = inst.title.clone();
    let tool = inst.tool.clone();

    // Revive the pane if needed before delivering keystrokes. Without this,
    // a send to a dead pane silently writes to a corpse with no agent to
    // respond to it.
    if !args.no_revive {
        if let Some(target) = instances.iter_mut().find(|i| i.id == session_id) {
            match target.ensure_pane_ready() {
                Ok(EnsureReadyOutcome::Respawned { stale_sid: None }) => {
                    eprintln!("  (respawned dead pane before send)");
                }
                Ok(EnsureReadyOutcome::Respawned {
                    stale_sid: Some(sid),
                }) => {
                    eprintln!(
                        "  (respawned dead pane before send){}",
                        stale_history_suffix(&sid),
                    );
                }
                Ok(EnsureReadyOutcome::Started { stale_sid: None }) => {
                    eprintln!("  (started stopped session before send)");
                }
                Ok(EnsureReadyOutcome::Started {
                    stale_sid: Some(sid),
                }) => {
                    eprintln!(
                        "  (started stopped session before send){}",
                        stale_history_suffix(&sid),
                    );
                }
                Ok(EnsureReadyOutcome::AlreadyAlive) => {}
                Err(EnsureReadyError::Transient(status)) => {
                    bail!("Session is mid-lifecycle ({status:?}); cannot send right now")
                }
                Err(EnsureReadyError::StructuredView) => {
                    bail!("Acp-mode sessions have no tmux pane; send is not supported")
                }
                Err(EnsureReadyError::Tmux(e)) => bail!("{}", e),
            }
        }
    }

    let tmux_session = crate::tmux::Session::new(&session_id, &session_title)?;
    if !tmux_session.exists() {
        bail!(
            "Session is not running. Start it first with: aoe session start {}",
            args.identifier
        );
    }

    let delay = crate::agents::send_keys_enter_delay(&tool);
    tmux_session.send_keys_with_delay(&args.message, delay)?;

    // Stamp last_accessed_at so the "last activity" column reflects user
    // interaction, and remap the status to Running. The agent has just been
    // given fresh input; the next status poll will reconcile the real state,
    // but flipping to Running immediately keeps the row from sticking on a
    // stale Idle/Waiting label during the gap between send and poll.
    // `touch_last_accessed` also auto-clears `archived_at` and `snoozed_until`
    // (see Instance::touch_last_accessed), so a user can wake any sunk row by
    // sending to it.
    let id_for_save = session_id.clone();
    if let Err(err) = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
            inst.touch_last_accessed();
            inst.status = crate::session::Status::Running;
        }
        Ok(())
    }) {
        // The tmux send succeeded; the storage write is best-effort
        // bookkeeping (status remap + auto-unarchive). Surfacing this as a
        // hard error would tell the user "send failed" when the message
        // actually reached the agent, so log a warning and keep the success
        // line. The next status poll will reconcile the row anyway.
        tracing::warn!(
            ?err,
            "send: failed to persist status remap after successful send"
        );
    }

    println!("Sent message to '{}'", session_title);
    Ok(())
}
