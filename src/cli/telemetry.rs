//! `aoe telemetry` subcommands: inspect and control anonymous opt-in usage
//! telemetry from the CLI.

use anyhow::Result;
use clap::Subcommand;

use crate::session::{update_app_state, update_config, Config};

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
pub fn run(command: TelemetryCommands) -> Result<()> {
    match command {
        TelemetryCommands::Status => run_status(),
        TelemetryCommands::Enable => run_set_enabled(true),
        TelemetryCommands::Disable => run_set_enabled(false),
        TelemetryCommands::ResetId => run_reset_id(),
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

fn run_set_enabled(enabled: bool) -> Result<()> {
    update_config(|config| {
        config.telemetry.enabled = enabled;
    })?;
    update_app_state(|state| {
        state.has_responded_to_telemetry = true;
    })?;
    crate::telemetry::apply_opt_in_change(enabled);

    if enabled {
        if crate::telemetry::do_not_track() {
            println!(
                "Telemetry enabled in config, but DO_NOT_TRACK is set, so nothing is sent \
                 and no install id is generated."
            );
        } else {
            println!("Telemetry enabled. Thank you for helping improve aoe.");
            if let Some(id) = crate::telemetry::install_id() {
                println!("  anonymous install id: {id}");
            }
        }
    } else if crate::telemetry::install_id().is_some() {
        // apply_opt_in_change only logs delete failures, so confirm rather
        // than assume the file is gone.
        println!(
            "Telemetry disabled, but the local install id could not be removed; see the debug log."
        );
    } else {
        println!("Telemetry disabled. The local install id has been deleted.");
    }
    Ok(())
}

fn run_reset_id() -> Result<()> {
    if !Config::load_or_warn().telemetry.enabled {
        println!(
            "Telemetry is disabled; nothing to reset. Enable it first with `aoe telemetry enable`."
        );
        return Ok(());
    }
    if crate::telemetry::do_not_track() {
        println!("DO_NOT_TRACK is set; no install id is generated, so there is nothing to reset.");
        return Ok(());
    }
    match crate::telemetry::reset_install_id() {
        Some(id) => {
            println!("Generated a fresh anonymous install id: {id}");
            println!(
                "Note: the old id is gone, so this install now counts as a new one in \
                 aggregate stats (distinct-install and retention counts)."
            );
        }
        None => println!("Could not generate a new install id."),
    }
    Ok(())
}
