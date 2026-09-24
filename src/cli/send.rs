//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::http::PromptDispositionWire;
use crate::acp::client::{require_daemon, HttpClient};
use crate::session::{EnsureReadyOutcome, Storage};

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
    for inst in &mut instances {
        inst.source_profile = profile.to_string();
    }

    if args.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let session_id = inst.id.clone();
    let session_title = inst.title.clone();
    let tool = inst.tool.clone();
    let is_structured = inst.is_structured();

    // Refuse before any revive; the terminal path rechecks under the lock right before sending.
    inst.ensure_startable()?;
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
                Err(e) => bail!("{e}"),
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

    let target = instances
        .iter()
        .find(|i| i.id == session_id)
        .expect("resolved above");
    let _input_lock = target.lock_for_input()?;
    let delay = crate::agents::send_keys_enter_delay(&tool);
    tmux_session.send_keys_with_delay(&args.message, delay)?;

    let id_for_save = session_id.clone();
    if let Err(err) = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
            inst.touch_after_input();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use serial_test::serial;

    /// #4116: auto-revive refuses to start an archived or trashed session, terminal or structured.
    #[tokio::test]
    #[serial]
    async fn send_does_not_revive_archived_or_trashed_session() {
        let dismissals: [(fn(&mut Instance), &str); 2] = [
            (Instance::archive, "session is archived; unarchive it first"),
            (Instance::trash, "session is in trash; restore it first"),
        ];
        for ((dismiss, message), structured) in
            dismissals.into_iter().flat_map(|d| [(d, false), (d, true)])
        {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "send-blocked";
            let mut inst = Instance::new("dismissed", "/tmp/x");
            if structured {
                inst.view = crate::session::View::Structured;
            }
            dismiss(&mut inst);
            let id = inst.id.clone();
            Storage::new_unwatched(profile)
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![inst.clone()];
                    Ok(())
                })
                .unwrap();

            let args = SendArgs {
                identifier: id.clone(),
                message: "hello".to_string(),
                no_revive: false,
            };
            let err = run(profile, args).await.unwrap_err();
            assert_eq!(err.to_string(), message, "structured={structured}");
            let tmux = crate::tmux::Session::new(&id, &inst.title).unwrap();
            assert!(!tmux.exists());
        }
    }

    /// #4116: an archived session with a live pane (`archive --no-kill`) takes no input and
    /// stays archived, with or without `--no-revive`.
    #[tokio::test]
    #[serial]
    async fn send_refuses_a_live_archived_pane() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        for no_revive in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "send-live-archived";
            let mut inst = Instance::new("live-archived", "/tmp/x");
            inst.archive();
            let id = inst.id.clone();
            Storage::new_unwatched(profile)
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![inst.clone()];
                    Ok(())
                })
                .unwrap();
            let pane = crate::tmux::Session::generate_name(&id, &inst.title);
            let created = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &pane, "sleep", "60"])
                .status();
            if !created.map(|s| s.success()).unwrap_or(false) {
                eprintln!("tmux new-session failed; skipping");
                return;
            }
            crate::tmux::refresh_session_cache();

            let args = SendArgs {
                identifier: id.clone(),
                message: "hello".to_string(),
                no_revive,
            };
            let err = run(profile, args).await.unwrap_err();
            let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &pane])
                .output();
            assert_eq!(
                err.to_string(),
                "session is archived; unarchive it first",
                "no_revive={no_revive}"
            );
            assert!(stored[0].is_archived(), "no_revive={no_revive}");
        }
    }
}
