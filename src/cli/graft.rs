//! Runtime grafting of plugin-declared commands onto the clap tree.

use std::collections::HashSet;

use anyhow::Result;
use clap::{ArgMatches, Command, CommandFactory};

use super::definition::Cli;

pub struct PluginCommand {
    pub plugin_id: String,
    pub name: String,
    pub title: String,
}

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

pub fn augmented_command() -> Command {
    let cmd = graft_onto(Cli::command(), plugin_commands());

    hide_disabled_serve(cmd, web_disabled())
}

pub fn web_disabled() -> bool {
    crate::plugin::registry()
        .get("aoe.web")
        .is_some_and(|p| !p.enabled)
}

fn hide_disabled_serve(cmd: Command, web_disabled: bool) -> Command {
    if web_disabled {
        cmd.mut_subcommand("serve", |c| c.hide(true))
    } else {
        cmd
    }
}

pub fn serve_start_blocked(cli: &Cli, web_disabled: bool) -> bool {
    let Some(super::definition::Commands::Serve(args)) = &cli.command else {
        return false;
    };
    if args.stop || args.status || args.restart {
        return false;
    }
    web_disabled
}

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
        let core: HashSet<String> = Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        let augmented: HashSet<String> = augmented_command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        assert_eq!(core, augmented);
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
            pc("acme.kit", "add"),
            pc("acme.kit", "do-thing"),
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
        assert!(dispatch_plugin_command(&matches).is_err());
    }

    fn parse(args: &[&str]) -> Cli {
        use clap::FromArgMatches;
        Cli::from_arg_matches(
            &Cli::command()
                .try_get_matches_from(args)
                .expect("args parse"),
        )
        .expect("into Cli")
    }

    #[test]
    fn serve_start_blocked_only_when_web_off_and_not_lifecycle() {
        let start = parse(&["aoe", "serve"]);
        assert!(serve_start_blocked(&start, true));
        assert!(!serve_start_blocked(&start, false));
        for verb in ["--stop", "--status", "--restart"] {
            let c = parse(&["aoe", "serve", verb]);
            assert!(
                !serve_start_blocked(&c, true),
                "{verb} must bypass the gate"
            );
        }
        assert!(!serve_start_blocked(&parse(&["aoe", "agents"]), true));
    }

    #[test]
    fn hide_disabled_serve_hides_only_when_disabled() {
        let shown = hide_disabled_serve(Cli::command(), false);
        assert!(!shown
            .find_subcommand("serve")
            .expect("serve present")
            .is_hide_set());
        let hidden = hide_disabled_serve(Cli::command(), true);
        assert!(hidden
            .find_subcommand("serve")
            .expect("serve present")
            .is_hide_set());
    }
}
