//! `aoe hooks` subcommands: inspect and grant AoE's consent to write agent
//! status hooks into the host agent's own config.
//!
//! The consent is the same install-wide acknowledgement the TUI dialog writes,
//! so both surfaces converge on one source of truth. `--trust-hooks` is a
//! different surface: per-repository trust for hooks the repo declares itself.

use anyhow::Result;
use clap::Subcommand;

use crate::session::{host_hook_agent_name, host_hook_disclosure, update_app_state, Config};

#[derive(Subcommand)]
pub enum HooksCommands {
    /// Show whether AoE may write agent status hooks, and the files it targets
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

/// Whether the install already carries the acknowledgement. Reading it through
/// `load` keeps a corrupt `state.toml` an error rather than a silent "not
/// approved", which would send the user to approve something already approved.
fn acknowledged() -> Result<bool> {
    Ok(Config::load()?.app_state.has_acknowledged_agent_hooks)
}

fn print_status(profile: &str) -> Result<()> {
    if acknowledged()? {
        println!("Agent status hooks: approved for this installation");
    } else {
        println!("Agent status hooks: not approved");
        println!("  Run `aoe hooks approve` to let AoE write them, or accept the");
        println!("  dialog when the TUI offers it on the next launch.");
    }
    print_disclosure(profile)
}

fn approve(profile: &str) -> Result<()> {
    // Always disclose, even on a repeat run, so the output can be used to
    // review what the standing consent covers for another profile.
    print_disclosure(profile)?;
    if acknowledged()? {
        println!();
        println!("Agent status hooks already approved for this installation");
        return Ok(());
    }
    update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    println!();
    println!("✓ Agent status hooks approved for this installation");
    Ok(())
}

/// Print the files and hook commands a launch targets under `profile`, for
/// every tool the profile installs hooks for. A launch resolves more than this
/// when it routes through a native store or merges into a selected agent, so
/// the list is a disclosure, not an exhaustive manifest.
fn print_disclosure(profile: &str) -> Result<()> {
    let profile = crate::session::config::effective_profile(profile);
    let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let status_hooks = config.session.agent_status_hooks;

    let mut tool_names: Vec<&str> = crate::agents::AGENTS.iter().map(|a| a.name).collect();
    tool_names.extend(config.session.custom_agents.keys().map(String::as_str));
    tool_names.sort_unstable();
    tool_names.dedup();

    let disclosures: Vec<_> = tool_names
        .into_iter()
        .filter_map(|tool_name| {
            let detect_as = config
                .session
                .agent_detect_as
                .get(tool_name)
                .map(String::as_str);
            let agent_name = host_hook_agent_name(tool_name, detect_as);
            if !crate::agents::get_agent(&agent_name)
                .is_some_and(|agent| crate::agents::hook_install_required(agent, status_hooks))
            {
                return None;
            }
            let disclosure = host_hook_disclosure(tool_name, &agent_name, Some(&profile));
            (!disclosure.settings_paths.is_empty()).then_some((tool_name, disclosure))
        })
        .collect();

    println!("AoE installs status hooks into each agent's own config, to detect");
    println!("session status (running/waiting/idle).");
    println!();
    println!("Profile: {profile}");
    println!();
    if disclosures.is_empty() {
        println!("Files AoE targets: (none, this profile installs no status hooks)");
    } else {
        println!("Files AoE targets:");
        for (tool_name, disclosure) in &disclosures {
            for path in &disclosure.settings_paths {
                println!("  {tool_name}: {path}");
            }
        }
    }

    if !disclosures.is_empty() {
        println!();
        println!("Hook events added:");
        for (tool_name, disclosure) in &disclosures {
            println!("  {tool_name}:");
            for (event, status) in &disclosure.hook_commands {
                println!("    {event} -> {status}");
            }
        }
        println!();
        println!("Each of them runs:");
        println!(
            "  printf {{status}} > {}/$AOE_INSTANCE_ID/status",
            crate::hooks::hook_base_path().display()
        );
        println!();
        println!("Hooks are guarded by $AOE_INSTANCE_ID and are a");
        println!("no-op outside of AoE sessions.");
    }
    if disclosures
        .iter()
        .any(|(_, disclosure)| disclosure.needs_codex_trust_note)
    {
        println!();
        println!("Codex may ask you to review and trust these hooks in /hooks.");
        println!("Until then, AoE falls back to pane-based status detection.");
    }
    Ok(())
}
