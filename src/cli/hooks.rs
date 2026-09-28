//! `aoe hooks` subcommands: inspect and grant AoE's consent to write agent
//! status hooks into the host agent's own config.
//!
//! The consent is the same install-wide acknowledgement the TUI dialog writes,
//! so both surfaces converge on one source of truth. `--trust-hooks` is a
//! different surface: per-repository trust for hooks the repo declares itself.

use anyhow::Result;
use clap::Subcommand;

use crate::session::{host_hook_disclosure, update_app_state, Config};

#[derive(Subcommand)]
pub enum HooksCommands {
    /// Show whether AoE may write agent status hooks, and where it would write them
    Status,
    /// Allow AoE to write agent status hooks into the host agents' own config
    Approve,
}

#[tracing::instrument(target = "cli.hooks", skip_all)]
pub fn run(profile: &str, command: HooksCommands) -> Result<()> {
    match command {
        HooksCommands::Status => print_status(profile),
        HooksCommands::Approve => approve(profile),
    }
}

fn acknowledged() -> bool {
    Config::load_or_warn()
        .app_state
        .has_acknowledged_agent_hooks
}

fn print_status(profile: &str) -> Result<()> {
    if acknowledged() {
        println!("Agent status hooks: approved for this installation");
    } else {
        println!("Agent status hooks: not approved");
        println!("  Run `aoe hooks approve` to let AoE write them, or accept the");
        println!("  dialog when the TUI offers it on the next launch.");
    }
    print_disclosure(profile);
    Ok(())
}

fn approve(profile: &str) -> Result<()> {
    if acknowledged() {
        println!("Agent status hooks already approved for this installation");
        return Ok(());
    }
    print_disclosure(profile);
    update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    println!();
    println!("✓ Agent status hooks approved for this installation");
    Ok(())
}

/// Print what a launch would write, for every agent that installs hooks under
/// the effective profile. Empty when the profile disables status hooks
/// everywhere and declares no identity hooks.
fn print_disclosure(profile: &str) {
    let profile = (!profile.is_empty()).then_some(profile);
    let status_hooks =
        crate::session::config::profile_config::resolve_config_or_warn(profile.unwrap_or_default())
            .session
            .agent_status_hooks;
    let disclosures: Vec<_> = crate::agents::AGENTS
        .iter()
        .filter(|agent| crate::agents::hook_install_required(agent, status_hooks))
        .filter_map(|agent| {
            let disclosure = host_hook_disclosure(agent.name, agent.name, profile);
            (!disclosure.settings_paths.is_empty()).then_some((agent.name, disclosure))
        })
        .collect();

    println!("AoE installs status hooks into each agent's own config, under your");
    println!("home directory, to detect session status (running/waiting/idle).");
    println!();
    if disclosures.is_empty() {
        println!("Files AoE would write: (none, this profile installs no status hooks)");
    } else {
        println!("Files AoE would write:");
        for (name, disclosure) in &disclosures {
            for path in &disclosure.settings_paths {
                println!("  {name}: {path}");
            }
        }
    }
    println!();
    println!("Each hook runs:");
    println!(
        "  printf {{status}} > {}/$AOE_INSTANCE_ID/status",
        crate::hooks::hook_base_path().display()
    );
    println!();
    println!("Hooks are guarded by $AOE_INSTANCE_ID and are a");
    println!("no-op outside of AoE sessions.");
    if disclosures
        .iter()
        .any(|(_, disclosure)| disclosure.needs_codex_trust_note)
    {
        println!();
        println!("Codex may ask you to review and trust these hooks in /hooks.");
        println!("Until then, AoE falls back to pane-based status detection.");
    }
}
