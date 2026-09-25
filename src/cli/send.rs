//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::http::PromptDispositionWire;
use crate::acp::client::{require_daemon, HttpClient};
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
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    if args.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let session_id = inst.id.clone();
    let session_title = inst.title.clone();
    let tool = inst.tool.clone();
    let is_structured = inst.is_structured();

    if is_structured {
        return send_structured(&session_id, &session_title, &args.message, args.no_revive).await;
    }

    if !args.no_revive {
        if let Some(target) = instances.iter_mut().find(|i| i.id == session_id) {
            match target.ensure_pane_ready() {
                Ok(EnsureReadyOutcome::Respawned) => {
                    eprintln!("  (respawned dead pane before send)");
                }
                Ok(EnsureReadyOutcome::Started) => {
                    eprintln!("  (started stopped session before send)");
                }
                Ok(EnsureReadyOutcome::ResumeFailed { sid }) => {
                    bail!("Resume failed for sid {sid}; preserved for explicit retry")
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

    tmux_session.wait_until_ready(
        std::time::Duration::from_secs(5),
        crate::agents::ready_marker(&tool),
    );

    let delay = crate::agents::send_keys_enter_delay(&tool);
    tmux_session.send_keys_with_delay(&args.message, delay)?;

    let id_for_save = session_id.clone();
    if let Err(err) = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
            inst.touch_last_accessed();
            inst.status = crate::session::Status::Running;
        }
        Ok(())
    }) {
        tracing::warn!(
            ?err,
            "send: failed to persist status remap after successful send"
        );
    }

    println!("Sent message to '{}'", session_title);
    Ok(())
}

/// ACP/structured-view sessions have no tmux pane; delivering a message means
/// hitting the running daemon's prompt endpoint instead, the same path the
/// web composer's send button uses. `no_revive` is enforced by the daemon
/// itself (atomically, at admission), not checked here first, since a
/// separate client-side liveness probe would race the daemon's own decision.
async fn send_structured(
    session_id: &str,
    session_title: &str,
    message: &str,
    no_revive: bool,
) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let dispatch = client.prompt(session_id, message, no_revive).await?;
    let verb = match dispatch.disposition {
        PromptDispositionWire::Sent => "Sent",
        PromptDispositionWire::Steered => "Steered into",
        PromptDispositionWire::Queued => "Queued",
    };
    println!("{verb} message to '{session_title}'");
    Ok(())
}
