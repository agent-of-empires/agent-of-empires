//! `agent-of-empires session move`: transfer sessions to another profile through the Daemon.

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::require_daemon;
use crate::daemon::{RenameSessionBody, RuntimeHealth, SessionResponse, RUNTIME_PROTOCOL_VERSION};
use crate::session::{PanePresence, Status};

#[derive(Args)]
pub struct MoveArgs {
    /// Session IDs or titles to move
    #[arg(required = true)]
    identifiers: Vec<String>,

    /// Destination profile; it must already exist
    #[arg(long)]
    to_profile: String,
}

pub async fn run(_profile: &str, args: MoveArgs) -> Result<()> {
    let client = require_daemon().await?.daemon_client()?;
    let info = client.runtime_info().await?;
    anyhow::ensure!(
        info.protocol_version == RUNTIME_PROTOCOL_VERSION && info.health == RuntimeHealth::Healthy,
        "Daemon runtime is not ready"
    );
    let target = args.to_profile.as_str();
    if !info.profiles.iter().any(|p| p == target) {
        bail!("Profile '{target}' does not exist");
    }

    let rows = client.list_sessions(None).await?.sessions;
    let mut batch: Vec<&SessionResponse> = Vec::with_capacity(args.identifiers.len());
    for identifier in &args.identifiers {
        let row = resolve(identifier, &rows)?;
        if batch.iter().any(|b| b.id == row.id) {
            bail!("Session {} was named more than once", row.id);
        }
        if row.profile == target {
            bail!("Session {} is already in profile '{target}'", row.id);
        }
        refuse_if_active(row)?;
        batch.push(row);
    }

    // The Daemon commits each move atomically; a later failure leaves earlier moves in place.
    let mut moved: Vec<&str> = Vec::new();
    for row in &batch {
        let body = RenameSessionBody {
            title: None,
            group: None,
            profile: Some(target.to_string()),
            rename_branch: false,
        };
        if let Err(error) = client.rename_session(&row.id, &body, &info.epoch).await {
            let error = anyhow::Error::from(error).context(format!("Failed to move {}", row.id));
            if moved.is_empty() {
                return Err(error);
            }
            let untried: Vec<&str> = batch
                .iter()
                .map(|r| r.id.as_str())
                .filter(|id| *id != row.id && !moved.contains(id))
                .collect();
            bail!(
                "{error:#}\nAlready moved to '{target}': {}\nNot confirmed (check with `aoe session list`): {}\nNot attempted: {}",
                moved.join(", "),
                row.id,
                untried.join(", ")
            );
        }
        moved.push(&row.id);
        println!("✓ Moved {} ({}) to profile '{target}'", row.title, row.id);
    }
    Ok(())
}

/// Id, then unique id prefix, then unique title.
fn resolve<'a>(identifier: &str, rows: &'a [SessionResponse]) -> Result<&'a SessionResponse> {
    if let Some(row) = rows.iter().find(|r| r.id == identifier) {
        return Ok(row);
    }
    let by_prefix: Vec<&SessionResponse> = rows
        .iter()
        .filter(|r| r.id.starts_with(identifier))
        .collect();
    let candidates = if by_prefix.is_empty() {
        rows.iter().filter(|r| r.title == identifier).collect()
    } else {
        by_prefix
    };
    match candidates.as_slice() {
        [] => bail!("Session not found: {identifier}"),
        [row] => Ok(row),
        many => {
            let mut lines: Vec<String> = many
                .iter()
                .map(|r| format!("  {} ({}, profile {})", r.id, r.title, r.profile))
                .collect();
            lines.sort();
            bail!(
                "Ambiguous session identifier {identifier:?} matches {} sessions:\n{}\nUse the full ID.",
                many.len(),
                lines.join("\n")
            )
        }
    }
}

/// The Daemon does not refuse active sessions on a profile move, so refuse them here.
/// An unreadable status counts as active.
fn refuse_if_active(row: &SessionResponse) -> Result<()> {
    let active = row.lifecycle_reservation.is_some()
        || matches!(
            row.agent_pane.state,
            PanePresence::Alive | PanePresence::Unknown
        )
        || Status::from_api_str(&row.status).is_none_or(Status::blocks_worktree_edit);
    if active {
        bail!(
            "Cannot move session {} ({}) while it is active; stop it first",
            row.title,
            row.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PaneObservation;

    fn row(id: &str, title: &str, profile: &str) -> SessionResponse {
        let mut row: SessionResponse = serde_json::from_value(serde_json::json!({ "id": id }))
            .expect("only id is required when decoding");
        row.title = title.into();
        row.profile = profile.into();
        row.status = "Stopped".into();
        row.agent_pane = PaneObservation {
            state: PanePresence::Absent,
            ..Default::default()
        };
        row
    }

    #[test]
    fn resolves_id_then_prefix_then_title_and_rejects_ambiguity() {
        let rows = vec![
            row("abc123", "alpha", "default"),
            row("abd456", "beta", "default"),
            row("zzz999", "alpha", "work"),
        ];
        let cases = [
            ("abc123", Some("abc123")),
            ("abc", Some("abc123")),
            ("beta", Some("abd456")),
            ("ab", None),
            ("alpha", None),
            ("missing", None),
        ];
        for (identifier, expected) in cases {
            let found = resolve(identifier, &rows).ok().map(|r| r.id.as_str());
            assert_eq!(found, expected, "{identifier}");
        }
    }

    #[test]
    fn only_quiet_sessions_may_move() {
        assert!(refuse_if_active(&row("a", "t", "p")).is_ok());

        let mut dead = row("a", "t", "p");
        dead.agent_pane.state = PanePresence::Dead;
        assert!(refuse_if_active(&dead).is_ok());

        let busy: [fn(&mut SessionResponse); 5] = [
            |r| r.status = "Running".into(),
            |r| r.status = "Deleting".into(),
            |r| r.status = "from a newer daemon".into(),
            |r| r.agent_pane.state = PanePresence::Alive,
            |r| r.agent_pane.state = PanePresence::Unknown,
        ];
        for mutate in busy {
            let mut active = row("a", "t", "p");
            mutate(&mut active);
            assert!(refuse_if_active(&active).is_err());
        }
    }
}
