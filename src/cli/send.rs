//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::http::PromptDispositionWire;
use crate::acp::client::{require_daemon, HttpClient, HttpError};
use crate::session::View;

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
    if args.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let mut sessions = client.list_sessions(None).await?.sessions;
    sessions.retain(|s| s.profile == profile);
    let session = super::resolve_session(&args.identifier, &sessions)?;

    let result = if session.view == View::Structured {
        client
            .prompt(&session.id, &args.message, args.no_revive)
            .await
            .map(|dispatch| match dispatch.disposition {
                PromptDispositionWire::Sent => "Sent",
                PromptDispositionWire::Steered => "Steered into",
                PromptDispositionWire::Queued => "Queued",
            })
    } else {
        client
            .send_message(&session.id, &args.message, args.no_revive)
            .await
            .map(|()| "Sent")
    };
    let verb = result.map_err(|e| refusal(e, &args.identifier))?;
    println!("{verb} message to '{}'", session.title);
    Ok(())
}

/// Daemon refusals carry the wording the local path used to print.
fn refusal(error: HttpError, identifier: &str) -> anyhow::Error {
    let HttpError::Server { status, body } = &error else {
        return error.into();
    };
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if json["error"] == "session_not_running" {
        return anyhow::anyhow!(
            "Session is not running. Start it first with: aoe session start {identifier}"
        );
    }
    match json["message"].as_str() {
        Some(message) if status.as_u16() == 409 => anyhow::anyhow!("{message}"),
        _ => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    fn conflict(body: &str) -> HttpError {
        HttpError::Server {
            status: StatusCode::CONFLICT,
            body: body.to_string(),
        }
    }

    #[test]
    fn refusal_keeps_the_local_path_wording() {
        let cases = [
            (
                conflict(r#"{"error":"session_not_running"}"#),
                "Session is not running. Start it first with: aoe session start s1",
            ),
            (
                conflict(
                    r#"{"error":"session_archived","message":"session is archived; unarchive it first"}"#,
                ),
                "session is archived; unarchive it first",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(refusal(error, "s1").to_string(), expected);
        }
        let other = HttpError::Server {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: r#"{"error":"tmux_error"}"#.to_string(),
        };
        assert!(refusal(other, "s1").to_string().contains("tmux_error"));
    }
}
