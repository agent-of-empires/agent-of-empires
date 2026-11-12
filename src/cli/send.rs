//! `agent-of-empires send` subcommand implementation

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Result};
use clap::Args;
use sha2::{Digest, Sha256};

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

    // Revive the pane if needed before delivering keystrokes. Without this,
    // a send to a dead pane silently writes to a corpse with no agent to
    // respond to it.
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

    // Wait for the pane to become ready before typing. A pane that exists
    // is not necessarily an agent that's finished booting: a session
    // started by an earlier, separate `aoe session start` reports
    // `EnsureReadyOutcome::AlreadyAlive` above with no wait at all, and
    // agents with no interposed shell (e.g. opencode) clear the pane's
    // "running a shell" check almost immediately even though their own TUI
    // can still take several more seconds to render and accept input. A
    // message typed into that window is silently dropped with no error.
    // Bounded so a genuinely busy/streaming agent doesn't block `send`
    // forever.
    tmux_session.wait_until_ready(
        std::time::Duration::from_secs(5),
        crate::agents::ready_marker(&tool),
    );

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

    // Like the status remap above, the send log is bookkeeping: the message
    // already reached the agent, so a write failure must not report a failed send.
    let sender = super::session::current_aoe_session()
        .ok()
        .map(|(_, inst)| inst.id);
    let logged = crate::session::get_app_dir().and_then(|dir| {
        append_send_record(
            &dir.join(SEND_LOG_FILE),
            chrono::Utc::now(),
            &session_id,
            sender.as_deref(),
            &args.message,
        )
    });
    if let Err(err) = logged {
        tracing::warn!(?err, "send: failed to append to {SEND_LOG_FILE}");
    }

    println!("Sent message to '{}'", session_title);
    Ok(())
}

const SEND_LOG_FILE: &str = "sends.log";

/// Append one JSON line recording a delivered send. Only a hash of the message
/// is kept, so the log can tell a person's typed prompt from an agent's send
/// without holding any content.
fn append_send_record(
    log: &Path,
    at: chrono::DateTime<chrono::Utc>,
    target: &str,
    sender: Option<&str>,
    message: &str,
) -> Result<()> {
    let sha256: String = Sha256::digest(message.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let record = serde_json::json!({
        "time": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "target": target,
        "sender": sender,
        "sha256": sha256,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    writeln!(file, "{record}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_log_keeps_time_target_sender_and_hash_but_never_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(SEND_LOG_FILE);
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-12T00:19:28Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        append_send_record(&log, at, "target-id", Some("sender-id"), "hello world").unwrap();
        append_send_record(&log, at, "target-id", None, "second secret").unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "one appended line per send");
        assert_eq!(lines[0]["time"], "2026-11-12T00:19:28Z");
        assert_eq!(lines[0]["target"], "target-id");
        assert_eq!(lines[0]["sender"], "sender-id");
        assert_eq!(
            lines[0]["sha256"],
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(lines[1]["sender"].is_null(), "no aoe sender is null");
        assert!(!text.contains("hello") && !text.contains("secret"));
    }
}
