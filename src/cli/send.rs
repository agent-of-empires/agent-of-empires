//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::http::PromptDispositionWire;
use crate::acp::client::{require_daemon, HttpClient, HttpError};
use crate::session::{StartBlocked, View};

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
    let profile = crate::session::config::effective_profile(profile);
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

/// The client carries only a validated refusal code, so the wording lives here.
fn refusal(error: HttpError, identifier: &str) -> anyhow::Error {
    let HttpError::Server { body, .. } = &error else {
        return error.into();
    };
    match body.as_str() {
        "session_not_running" => anyhow::anyhow!(
            "Session is not running. Start it first with: aoe session start {identifier}"
        ),
        "session_archived" => anyhow::anyhow!(StartBlocked::Archived),
        "session_trashed" => anyhow::anyhow!(StartBlocked::Trashed),
        _ => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::DaemonEndpoint;
    use crate::daemon::ApiErrorCode;
    use crate::server::api::{api_error_with_code_header, start_blocked_response};
    use axum::{extract::Path, routing::post, Router};

    /// Drives the real client against the refusals the daemon actually emits.
    #[tokio::test]
    async fn refusals_survive_the_real_client() {
        let app = Router::new().route(
            "/api/sessions/{id}/send",
            post(|Path(id): Path<String>| async move {
                match id.as_str() {
                    "archived" => start_blocked_response(StartBlocked::Archived),
                    "trashed" => start_blocked_response(StartBlocked::Trashed),
                    "dead" => api_error_with_code_header(
                        ApiErrorCode::SessionNotRunning,
                        "Terminal session is not running",
                    ),
                    _ => api_error_with_code_header(ApiErrorCode::StartFailed, "boom"),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = HttpClient::new(DaemonEndpoint::new(
            format!("http://{address}"),
            Some("secret".to_string()),
            crate::acp::client::discovery::Source::Env,
        ))
        .unwrap();

        for (id, expected) in [
            (
                "dead",
                "Session is not running. Start it first with: aoe session start dead",
            ),
            ("archived", "session is archived; unarchive it first"),
            ("trashed", "session is in trash; restore it first"),
        ] {
            let error = client.send_message(id, "hi", true).await.unwrap_err();
            assert_eq!(refusal(error, id).to_string(), expected);
        }
        let error = client.send_message("other", "hi", true).await.unwrap_err();
        assert!(refusal(error, "other").to_string().contains("start_failed"));
        server.abort();
    }
}
