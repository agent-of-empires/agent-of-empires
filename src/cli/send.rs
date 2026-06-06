//! `agent-of-empires send` subcommand implementation
//!
//! Two modes share one entry point:
//! - **single target** — `aoe send <id> "msg"` (unchanged, backward-compatible);
//! - **bulk** — `aoe send [FILTER] "msg"` selects a set via [`TargetFilter`] and,
//!   by default, PREVIEWS it (prints matched targets, sends nothing). Only
//!   `--yes` actually delivers. This preview gate is the whole reason bulk send
//!   is safe: a blind `send` appends + submits, clobbering any input the agent
//!   has staged at its prompt, so we never fan out keystrokes the operator
//!   hasn't seen the blast radius of first.

use anyhow::{bail, Result};
use clap::Args;

use crate::cli::session::stale_history_suffix;
use crate::cli::target::{self, ResolvedTarget, TargetFilter};
use crate::session::{EnsureReadyError, EnsureReadyOutcome, Status, Storage};

#[derive(Args)]
pub struct SendArgs {
    /// In single-target mode: the session id or title. In bulk mode (a filter
    /// is present): omit this and pass only the message. Positional parsing:
    /// when two positionals are given they are `<identifier> <message>`; when
    /// one is given it is the `<message>` and targets come from the filter.
    arg1: Option<String>,

    /// Second positional; present only in `<identifier> <message>` form.
    arg2: Option<String>,

    #[command(flatten)]
    filter: TargetFilter,

    /// Actually deliver in bulk mode. Without it, bulk send only previews the
    /// matched targets. No effect on single-target sends (those always send).
    #[arg(long, short = 'y')]
    yes: bool,

    /// Fail loud on dead/stopped sessions instead of auto-respawning. Default
    /// behavior is to revive the session so a `send` after a crash or stop
    /// just works; pass this for scripts that want the previous bail-out.
    #[arg(long = "no-revive")]
    no_revive: bool,
}

#[tracing::instrument(target = "cli.send", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: SendArgs) -> Result<()> {
    // Disambiguate positionals: `<id> <msg>` vs filter-mode `<msg>`.
    let (identifier, message) = match (args.arg1, args.arg2) {
        (Some(id), Some(msg)) => (Some(id), msg),
        (Some(msg), None) => (None, msg),
        (None, _) => bail!("a message is required"),
    };

    if message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    // Bulk when any filter selector is present, or when no identifier was
    // given at all (a filter-less, identifier-less send is a no-op guard, not
    // a whole-fleet blast).
    if args.filter.has_selector() || identifier.is_none() {
        return bulk_send(profile, &args.filter, &message, args.yes, args.no_revive).await;
    }

    let identifier = identifier.expect("checked: Some in single-target branch");
    let title = deliver_send(profile, &identifier, &message, args.no_revive)?;
    println!("Sent message to '{}'", title);
    Ok(())
}

/// Deliver one message to one session in one profile: revive the pane if
/// needed, send keystrokes, then stamp `last_accessed`/`Running`. Returns the
/// session title on success. Shared by the single-target and bulk paths.
fn deliver_send(profile: &str, identifier: &str, message: &str, no_revive: bool) -> Result<String> {
    let storage = Storage::new(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    let inst = super::resolve_session(identifier, &instances)?;
    let session_id = inst.id.clone();
    let session_title = inst.title.clone();
    let tool = inst.tool.clone();

    // Revive the pane if needed before delivering keystrokes. Without this,
    // a send to a dead pane silently writes to a corpse with no agent to
    // respond to it.
    if !no_revive {
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
                Err(EnsureReadyError::CockpitMode) => {
                    bail!("Cockpit-mode sessions have no tmux pane; send is not supported")
                }
                Err(EnsureReadyError::Tmux(e)) => bail!("{}", e),
            }
        }
    }

    let tmux_session = crate::tmux::Session::new(&session_id, &session_title)?;
    if !tmux_session.exists() {
        bail!(
            "Session is not running. Start it first with: aoe session start {}",
            identifier
        );
    }

    let delay = crate::agents::send_keys_enter_delay(&tool);
    tmux_session.send_keys_with_delay(message, delay)?;

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

    Ok(session_title)
}

/// Bulk send: resolve the matched set across profiles, PREVIEW by default,
/// deliver only on `--yes`. Excludes the invoking session so the operator
/// never messages themselves; warns on `waiting`/`idle` targets that may hold
/// input staged at the prompt (a send appends + submits, mixing the two).
async fn bulk_send(
    ambient_profile: &str,
    filter: &TargetFilter,
    message: &str,
    yes: bool,
    no_revive: bool,
) -> Result<()> {
    let self_id = target::self_instance_id();
    let targets = target::resolve_targets(ambient_profile, filter, self_id.as_deref(), true)?;

    if targets.is_empty() {
        println!("No sessions match the filter (nothing to send).");
        return Ok(());
    }

    let staged: Vec<&ResolvedTarget> = targets
        .iter()
        .filter(|t| matches!(t.instance.status, Status::Waiting | Status::Idle))
        .collect();

    println!(
        "{} session(s) match{}:",
        targets.len(),
        if yes { "" } else { " (preview)" }
    );
    for t in &targets {
        println!(
            "  · [{}] {:<20} {:<8} {}",
            t.profile,
            super::truncate(&t.instance.title, 20),
            t.instance.status.as_str(),
            t.instance.id,
        );
    }
    if self_id.is_some() {
        println!("  (excluding this session)");
    }
    if !staged.is_empty() {
        println!(
            "⚠ {} target(s) are waiting/idle and may hold input staged at the prompt; \
             a send appends + submits, mixing your message with theirs:",
            staged.len()
        );
        for t in &staged {
            println!("    · [{}] {}", t.profile, t.instance.title);
        }
    }

    if !yes {
        println!(
            "\nPreview only — nothing sent. Re-run with --yes to deliver to the {} matched session(s).",
            targets.len()
        );
        return Ok(());
    }

    println!("\nMessage: {message:?}\n");
    let mut sent: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for t in &targets {
        match deliver_send(&t.profile, &t.instance.id, message, no_revive) {
            Ok(title) => sent.push(format!("[{}] {}", t.profile, title)),
            Err(e) => failed.push((
                format!("[{}] {}", t.profile, t.instance.title),
                e.to_string(),
            )),
        }
    }

    println!("✓ Sent to {}/{} sessions:", sent.len(), targets.len());
    for s in &sent {
        println!("  · {s}");
    }
    if !failed.is_empty() {
        println!("✗ {} failed:", failed.len());
        for (label, err) in &failed {
            println!("  · {label}: {err}");
        }
        bail!("{} session(s) failed to receive the message", failed.len());
    }

    Ok(())
}
