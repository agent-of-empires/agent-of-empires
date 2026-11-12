//! `aoe telemetry` subcommands: inspect and control anonymous opt-in usage

use anyhow::Result;
use clap::Subcommand;

use reqwest::Method;

use crate::session::Config;

#[derive(Subcommand)]
pub enum TelemetryCommands {
    /// Show the current telemetry opt-in state and install id
    Status,
    /// Opt in to anonymous usage telemetry
    Enable,
    /// Opt out of telemetry (deletes the local install id)
    Disable,
    /// Generate a fresh anonymous install id (only while opted in)
    ResetId,
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(command: TelemetryCommands) -> Result<()> {
    match command {
        TelemetryCommands::Status => run_status(),
        TelemetryCommands::Enable => run_set_enabled(true).await,
        TelemetryCommands::Disable => run_set_enabled(false).await,
        TelemetryCommands::ResetId => run_reset_id().await,
    }
}

fn run_status() -> Result<()> {
    let enabled = Config::load_or_warn().telemetry.enabled;
    let dnt = crate::telemetry::do_not_track();
    let id = crate::telemetry::install_id();
    let endpoint = crate::telemetry::endpoint();

    if dnt {
        println!("Telemetry: suppressed by DO_NOT_TRACK");
        println!("  DO_NOT_TRACK is set, so nothing is sent and no install id is generated,");
        println!("  regardless of the config setting (config enabled = {enabled}).");
    } else if enabled {
        println!("Telemetry: enabled (opt-in)");
        match id {
            Some(id) => println!("  install id: {id}"),
            None => println!("  install id: (not yet generated)"),
        }
    } else {
        println!("Telemetry: disabled (default)");
    }

    let overridden = std::env::var("AOE_TELEMETRY_ENDPOINT")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if overridden {
        println!("  endpoint: {endpoint}  (overridden via AOE_TELEMETRY_ENDPOINT)");
    } else {
        println!("  endpoint: {endpoint}  (default)");
    }
    Ok(())
}

/// What the Daemon reports after a consent change. Its `DO_NOT_TRACK` and
/// install id are the ones that count, since it owns them.
#[derive(serde::Deserialize)]
struct Outcome {
    enabled: bool,
    do_not_track: bool,
    install_id: Option<String>,
}

async fn post(path: &str, body: Option<serde_json::Value>) -> Result<Outcome> {
    let value = super::daemon_client()
        .await?
        .mutate(Method::POST, path, body)
        .await?;
    Ok(serde_json::from_value(value)?)
}

async fn run_set_enabled(enabled: bool) -> Result<()> {
    let outcome = post(
        "/api/telemetry/consent",
        Some(serde_json::json!({ "enabled": enabled })),
    )
    .await?;

    if enabled {
        if outcome.do_not_track {
            println!(
                "Telemetry enabled in config, but DO_NOT_TRACK is set, so nothing is sent \
                 and no install id is generated."
            );
        } else {
            println!("Telemetry enabled. Thank you for helping improve aoe.");
            if let Some(id) = outcome.install_id {
                println!("  anonymous install id: {id}");
            }
        }
    } else if outcome.install_id.is_some() {
        println!(
            "Telemetry disabled, but the local install id could not be removed; see the debug log."
        );
    } else {
        println!("Telemetry disabled. The local install id has been deleted.");
    }
    Ok(())
}

async fn run_reset_id() -> Result<()> {
    let outcome = post("/api/telemetry/reset-id", None).await?;
    if !outcome.enabled {
        println!(
            "Telemetry is disabled; nothing to reset. Enable it first with `aoe telemetry enable`."
        );
    } else if outcome.do_not_track {
        println!("DO_NOT_TRACK is set; no install id is generated, so there is nothing to reset.");
    } else if let Some(id) = outcome.install_id {
        println!("Generated a fresh anonymous install id: {id}");
        println!(
            "Note: the old id is gone, so this install now counts as a new one in \
             aggregate stats (distinct-install and retention counts)."
        );
    } else {
        println!("Could not generate a new install id.");
    }
    Ok(())
}
