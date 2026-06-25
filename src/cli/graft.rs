//! Runtime grafting of plugin-declared commands onto the clap tree.
//!
//! Core commands stay clap-derive. Active plugins' declared commands are
//! appended to the derived [`Command`] at runtime so they appear in `aoe --help`
//! and parse. Dispatch tries the core derive first (`Cli::from_arg_matches`); a
//! grafted command falls through to [`dispatch_plugin_command`]. Core always
//! wins a name conflict: a plugin command whose name collides with a core
//! subcommand is not grafted.
//!
//! Tier 0 has no executor, so a grafted command parses and is discoverable but
//! reports that it needs the plugin runtime (#2095) when invoked.

use std::collections::HashSet;

use anyhow::Result;
use clap::{ArgMatches, Command, CommandFactory};

use super::definition::Cli;

/// A command a plugin contributes to the CLI.
pub struct PluginCommand {
    pub plugin_id: String,
    pub name: String,
    pub title: String,
}

/// Commands declared by the active plugin set.
pub fn plugin_commands() -> Vec<PluginCommand> {
    let mut out = Vec::new();
    for p in crate::plugin::registry().active() {
        for c in &p.manifest.commands {
            out.push(PluginCommand {
                plugin_id: p.id().to_string(),
                name: c.id.clone(),
                title: c.title.clone(),
            });
        }
    }
    out
}

/// The clap command augmented with active plugins' commands. A plugin command
/// whose name collides with a core subcommand (or an already-grafted plugin
/// command) is skipped, so core always wins.
pub fn augmented_command() -> Command {
    graft_onto(Cli::command(), plugin_commands())
}

/// Graft `commands` onto `cmd`, skipping any whose name collides with an
/// existing subcommand (core wins) or with an earlier grafted command.
fn graft_onto(mut cmd: Command, commands: Vec<PluginCommand>) -> Command {
    let core: HashSet<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    let mut grafted: HashSet<String> = HashSet::new();
    for pc in commands {
        if core.contains(&pc.name) || !grafted.insert(pc.name.clone()) {
            continue;
        }
        let about = if pc.title.is_empty() {
            format!("Plugin command (from {})", pc.plugin_id)
        } else {
            format!("{} (from {})", pc.title, pc.plugin_id)
        };
        cmd = cmd.subcommand(Command::new(pc.name).about(about));
    }
    cmd
}

/// Handle a grafted plugin command. At Tier 0 there is no executor, so this
/// reports the command is plugin-provided and needs the runtime (#2095).
pub fn dispatch_plugin_command(matches: &ArgMatches) -> Result<()> {
    let Some(name) = matches.subcommand_name() else {
        anyhow::bail!("no command given");
    };
    match plugin_commands().into_iter().find(|p| p.name == name) {
        Some(pc) => {
            println!(
                "'{name}' is a command from plugin '{}'. Running plugin commands needs the \
                 plugin runtime, which is not available yet.",
                pc.plugin_id
            );
            Ok(())
        }
        None => anyhow::bail!("unknown command '{name}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn augmented_command_keeps_core_subcommands() {
        // With no active plugins in the test process, augmentation is a no-op:
        // the command still carries the core subcommands and no others.
        let core: HashSet<String> = Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        let augmented: HashSet<String> = augmented_command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        assert_eq!(core, augmented);
        // Sanity: a known core command is present.
        assert!(augmented.contains("add"));
    }

    fn pc(plugin_id: &str, name: &str) -> PluginCommand {
        PluginCommand {
            plugin_id: plugin_id.to_string(),
            name: name.to_string(),
            title: String::new(),
        }
    }

    #[test]
    fn graft_onto_skips_core_and_duplicate_names() {
        let commands = vec![
            // Collides with the core `add` command: core wins, not grafted.
            pc("acme.kit", "add"),
            pc("acme.kit", "do-thing"),
            // Duplicate of an already-grafted plugin command: skipped.
            pc("acme.other", "do-thing"),
        ];
        let cmd = graft_onto(Cli::command(), commands);
        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert_eq!(names.iter().filter(|n| **n == "add").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "do-thing").count(), 1);
    }

    #[test]
    fn dispatch_rejects_unknown_command() {
        let matches = Cli::command()
            .try_get_matches_from(["aoe", "agents"])
            .expect("core agents parses");
        // `agents` is a core command, not a plugin one: dispatch refuses it
        // rather than claiming it as a plugin command.
        assert!(dispatch_plugin_command(&matches).is_err());
    }
}
