//! `aoe hooks` subcommands: record your approval for AoE to write agent hooks
//! into each agent's own config, and show what that approval covers.
//!
//! The approval is the same install-wide flag the TUI dialog writes.
//! `--trust-hooks` is a different surface: per-repository trust for hooks the
//! repo declares itself.

use anyhow::Result;
use clap::Subcommand;

use crate::session::{
    host_hook_agent, host_hook_declared_agent, host_hook_disclosure, update_app_state, Config,
    HostHookDisclosure,
};

#[derive(Subcommand)]
pub enum HooksCommands {
    /// Show whether AoE may write agent hooks, and what they resolve for a profile
    Status,
    /// Let AoE write agent hooks for every agent, on every profile
    Approve,
}

#[tracing::instrument(target = "cli.hooks", skip_all, fields(profile = %profile))]
pub fn run(profile: &str, command: HooksCommands) -> Result<()> {
    match command {
        HooksCommands::Status => print_status(profile),
        HooksCommands::Approve => approve(profile),
    }
}

/// A corrupt `state.toml` is an error here rather than a silent "not approved",
/// which would point the user at an approve that cannot succeed either.
fn approved() -> Result<bool> {
    Ok(Config::load()?.app_state.has_acknowledged_agent_hooks)
}

fn print_status(profile: &str) -> Result<()> {
    if approved()? {
        println!("Agent hooks: approved for this installation");
    } else {
        println!("Agent hooks: not approved");
        println!("  Run `aoe hooks approve` to let AoE write them, or accept the");
        println!("  dialog the TUI shows when you create a host session.");
    }
    print_disclosure(profile);
    Ok(())
}

fn approve(profile: &str) -> Result<()> {
    // Always disclose, even on a repeat run, so the output can be used to
    // review what the standing approval covers for another profile.
    print_disclosure(profile);
    if approved()? {
        println!();
        println!("Agent hooks already approved for this installation");
        return Ok(());
    }
    update_app_state(|state| {
        state.has_acknowledged_agent_hooks = true;
    })?;
    println!();
    println!("✓ Agent hooks approved for this installation");
    Ok(())
}

/// The disclosure of every tool that installs hooks or declares a trust file, and
/// the tools that resolved to no agent. A configured tool that resolves to no agent
/// is kept by name: the user cannot otherwise tell "no extra agent" from "an agent I
/// could not name".
fn collect_disclosures<'a>(
    config: &crate::session::config::Config,
    tool_names: Vec<&'a str>,
) -> (Vec<(&'a str, HostHookDisclosure)>, Vec<&'a str>) {
    let status_hooks = config.session.agent_status_hooks;
    let mut unresolved: Vec<&str> = Vec::new();
    let disclosures = tool_names
        .into_iter()
        .filter_map(|tool_name| {
            let command = config.session.launch_command_for(tool_name);
            let describe = |agent: &crate::agents::AgentDef| {
                let mut disclosure = host_hook_disclosure(tool_name, agent, config);
                if !crate::agents::hook_install_required(agent, status_hooks) {
                    disclosure.settings_paths.clear();
                    disclosure.extra_settings_paths.clear();
                    disclosure.hook_commands.clear();
                }
                Some(disclosure)
            };
            let disclosure = match host_hook_declared_agent(tool_name, &command, &config.session) {
                Some(declared) => declared.with_definition(describe).flatten(),
                None => {
                    let Some(agent) = host_hook_agent(tool_name, &command, &config.session) else {
                        unresolved.push(tool_name);
                        return None;
                    };
                    describe(agent)
                }
            }?;
            (!disclosure.settings_paths.is_empty() || !disclosure.trust_paths.is_empty())
                .then_some((tool_name, disclosure))
        })
        .collect();
    (disclosures, unresolved)
}

/// Print the files and hook commands this profile resolves, for every tool it
/// installs hooks for. The printed caveat is the bound on that list.
fn print_disclosure(profile: &str) {
    let profile = crate::session::config::effective_profile(profile);
    let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let declared_tools =
        crate::plugin::agent_integration::hook_declaring_tools(&crate::plugin::registry());
    let mut tool_names = crate::agents::agent_names();
    tool_names.extend(config.session.custom_agents.keys().map(String::as_str));
    tool_names.extend(declared_tools.iter().map(String::as_str));
    tool_names.sort_unstable();
    tool_names.dedup();

    let (disclosures, unresolved) = collect_disclosures(&config, tool_names);

    let status_hooks_active = disclosures
        .iter()
        .any(|(_, disclosure)| disclosure.status_hooks_enabled);
    let hooks_installed = disclosures
        .iter()
        .any(|(_, disclosure)| !disclosure.settings_paths.is_empty());
    if !hooks_installed {
        println!("No agent under this profile installs hooks, so there is nothing to");
        println!("approve.");
    } else if status_hooks_active {
        println!("AoE installs agent hooks into each agent's own config. The status");
        println!("hooks detect session status (running/waiting/idle); the identity hooks");
        println!("record the conversation id native resume needs.");
    } else {
        println!("No status hook survives this profile, so AoE installs only the");
        println!("identity hooks native resume needs.");
    }
    println!();
    println!("Profile: {profile}");
    println!();
    println!("Files AoE targets:");
    for (tool_name, disclosure) in &disclosures {
        for path in &disclosure.settings_paths {
            println!("  {tool_name}: {path}");
        }
        for (label, path) in &disclosure.extra_settings_paths {
            println!("  {tool_name}: {path}");
            println!("    ({label})");
        }
        if let Some(config) = &disclosure.disabled_by_agent {
            println!(
                "    (this agent's own config turns its hooks off: {})",
                config.display()
            );
        }
    }

    let trust_files: Vec<_> = disclosures
        .iter()
        .flat_map(|(tool_name, disclosure)| {
            disclosure
                .trust_paths
                .iter()
                .map(move |path| (tool_name, path))
        })
        .collect();
    if !trust_files.is_empty() {
        println!();
        println!("Trust files (only with session.pre_trust_agent_folders):");
        for (tool_name, path) in trust_files {
            println!("  {tool_name}: {path}");
        }
    }

    if !unresolved.is_empty() {
        println!();
        println!("Configured but not named here:");
        for tool_name in &unresolved {
            println!("  {tool_name}");
        }
        println!("  A repository can set session.agent_detect_as, and this command has");
        println!("  no project directory, so a launch inside one may resolve those to a");
        println!("  different agent. Declaring agent_execution_as and agent_config_dir for");
        println!("  them in this profile pins the file, because a repository cannot move");
        println!("  either of those.");
    }

    let events: Vec<_> = disclosures
        .iter()
        .filter(|(_, disclosure)| !disclosure.hook_commands.is_empty())
        .collect();
    let notes = crate::session::host_hook_post_install_notes();
    if !notes.is_empty() {
        println!();
        println!("This approval covers every agent and profile. Besides the files");
        println!("above, installing hooks for these agents also changes launcher state:");
        for (agent, note) in &notes {
            println!("  {agent}: {note}");
        }
    }

    if !events.is_empty() {
        println!();
        println!("Hook events a launch would install:");
        for (tool_name, disclosure) in events {
            println!("  {tool_name}:");
            for (event, effect) in &disclosure.hook_commands {
                println!("    {event} -> {effect}");
            }
        }
    }
    if status_hooks_active {
        println!();
        println!("A status event writes under the session's own directory, named by");
        println!(
            "  printf {{status}} > {}/$AOE_INSTANCE_ID/status",
            crate::hooks::hook_base_path().display()
        );
    }
    println!();
    println!("Hooks are guarded by $AOE_INSTANCE_ID and are a");
    println!("no-op outside of AoE sessions.");
    println!();
    println!("This is what the effective profile resolves, not a manifest of every");
    println!("write a launch can make. A launch that routes through a native store,");
    println!("merges into a selected agent, or targets a selected or recorded Claude");
    println!("conversation store resolves that target at launch time.");
    println!();
    println!("A session launched with its own command resolves the file that command");
    println!("names; the creation dialog describes such a session exactly.");
    println!();
    println!("The approval is per installation and is not bound to this profile, so");
    println!("another profile resolves its own paths under the same approval.");
    if disclosures
        .iter()
        .any(|(_, d)| d.needs_codex_trust_note && d.disabled_by_agent.is_none())
    {
        println!();
        println!("Codex may ask you to review and trust these hooks in /hooks.");
        if status_hooks_active {
            println!("Until then, AoE falls back to pane-based status detection.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;

    #[test]
    #[serial_test::serial(shell_env)]
    fn status_lists_a_trust_only_declaration_and_its_trust_file() {
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());
        let _app = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = EnvGuard::unset(&["EXAMPLE_CONFIG_DIR"]);
        crate::session::test_support::install_agent_plugin(
            "acme.trust-only",
            "id = \"acme.trust-only\"\nname = \"Trust only\"\nversion = \"1.0.0\"\n\
             api_version = 26\n[[agent_integrations]]\ntool = \"example-helper\"\n\
             command = \"example-helper\"\n[agent_integrations.trust]\n\
             file = \".example.json\"\nlocation = \"config_dir_or_home\"\n\
             pointer = \"/projects/{path}/trusted\"\nvalue = true\nsandbox = \"always\"\n",
        );
        let config = crate::session::config::profile_config::resolve_config_or_warn("");

        let (disclosures, unresolved) = collect_disclosures(&config, vec!["example-helper"]);

        assert!(unresolved.is_empty());
        let [(tool, disclosure)] = &disclosures[..] else {
            panic!("the trust-only agent is listed: {}", disclosures.len());
        };
        assert_eq!(*tool, "example-helper");
        assert!(disclosure.settings_paths.is_empty());
        assert_eq!(
            disclosure.trust_paths,
            vec![tmp
                .path()
                .join(".example.json")
                .to_string_lossy()
                .into_owned()]
        );
    }
}
