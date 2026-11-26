//! Runtime grafting of plugin-declared commands onto the clap tree.
//!
//! Core commands stay clap-derive. Active plugins' declared commands are
//! appended to the derived [`Command`] at runtime so they appear in `aoe --help`
//! and parse. Dispatch tries the core derive first (`Cli::from_arg_matches`); a
//! grafted command falls through to [`dispatch_plugin_command`]. Core always
//! wins a name conflict: a plugin command or group whose name collides with a
//! core subcommand is not grafted.
//!
//! A command that names a `[[command_groups]]` id appears as
//! `aoe <group> <command>`; an ungrouped command keeps its flat top-level name.
//!
//! Tier 0 has no executor, so a grafted command parses and is discoverable but
//! reports that it needs the plugin runtime (#2095) when invoked.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use clap::{ArgMatches, Command, CommandFactory};

use super::definition::Cli;

/// A command a plugin contributes to the CLI.
pub struct PluginCommand {
    pub plugin_id: String,
    pub name: String,
    pub title: String,
    /// Group the command nests under, when the manifest declares one.
    pub group: Option<PluginCommandGroup>,
}

/// The verb a group of plugin commands is nested under.
pub struct PluginCommandGroup {
    pub id: String,
    pub about: String,
}

/// Commands declared by the active plugin set.
pub fn plugin_commands() -> Vec<PluginCommand> {
    let mut out = Vec::new();
    for p in crate::plugin::registry().active() {
        for c in &p.manifest.commands {
            let group = c.group.as_ref().map(|id| {
                let about = p
                    .manifest
                    .command_groups
                    .iter()
                    .find(|g| &g.id == id)
                    .map(|g| g.about.clone())
                    .unwrap_or_default();
                PluginCommandGroup {
                    id: id.clone(),
                    about,
                }
            });
            out.push(PluginCommand {
                plugin_id: p.id().to_string(),
                name: c.id.clone(),
                title: c.title.clone(),
                group,
            });
        }
    }
    out
}

/// Add active plugin commands; core commands remain available independently.
pub fn augmented_command() -> Command {
    graft_onto(Cli::command(), plugin_commands())
}

/// Whether global configuration disables the registered Web plugin.
pub fn web_disabled(config: &crate::session::Config) -> bool {
    config
        .plugins
        .get("aoe.web")
        .is_some_and(|plugin| !plugin.enabled)
        && crate::plugin::registry().get("aoe.web").is_some()
}

/// Reject fresh web starts when `aoe.web` is disabled, not core recovery.
pub fn serve_start_blocked(cli: &Cli, web_disabled: bool) -> bool {
    let Some(super::definition::Commands::Serve(args)) = &cli.command else {
        return false;
    };
    if args.core_only || args.stop || args.status || args.restart || args.rollback {
        return false;
    }
    web_disabled
}

fn command_about(title: &str, plugin_id: &str) -> String {
    if title.is_empty() {
        format!("Plugin command (from {plugin_id})")
    } else {
        format!("{title} (from {plugin_id})")
    }
}

fn group_about(about: &str, plugin_id: &str) -> String {
    if about.is_empty() {
        format!("Plugin commands (from {plugin_id})")
    } else {
        format!("{about} (from {plugin_id})")
    }
}

/// Graft `commands` onto `cmd`, skipping any whose name collides with a core
/// subcommand or an earlier grafted one. Group verbs are claimed before flat
/// commands, so core wins, then the first plugin to declare a group id, then
/// the first plugin to claim a flat name. A group that is not grafted drops its
/// commands rather than flattening them.
fn graft_onto(mut cmd: Command, commands: Vec<PluginCommand>) -> Command {
    // Aliases count as taken (`aoe rm refresh` parses as core `remove`), and so
    // does clap's generated `help`, which no subcommand list contains.
    let mut taken: HashSet<String> = cmd
        .get_subcommands()
        .flat_map(|s| {
            std::iter::once(s.get_name().to_string()).chain(s.get_all_aliases().map(str::to_string))
        })
        .chain(std::iter::once("help".to_string()))
        .collect();

    let mut groups: Vec<(String, String, String)> = Vec::new();
    for pc in &commands {
        let Some(group) = &pc.group else { continue };
        if taken.insert(group.id.clone()) {
            groups.push((group.id.clone(), group.about.clone(), pc.plugin_id.clone()));
        }
    }
    let accepted: HashSet<&str> = groups.iter().map(|(id, ..)| id.as_str()).collect();

    let mut leaves: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<Command>> = BTreeMap::new();
    let mut flat: HashSet<String> = HashSet::new();
    for pc in commands {
        let leaf = Command::new(&pc.name).about(command_about(&pc.title, &pc.plugin_id));
        match pc.group.as_ref().map(|g| g.id.as_str()) {
            Some(group_id) if accepted.contains(group_id) => {
                if leaves
                    .entry(group_id.to_string())
                    .or_default()
                    .insert(pc.name.clone())
                {
                    children.entry(group_id.to_string()).or_default().push(leaf);
                }
            }
            Some(_) => {}
            None => {
                if !taken.contains(&pc.name) && flat.insert(pc.name.clone()) {
                    cmd = cmd.subcommand(leaf);
                }
            }
        }
    }

    for (group_id, about, plugin_id) in groups {
        let mut group_cmd = Command::new(&group_id)
            .about(group_about(&about, &plugin_id))
            // A bare group verb must not fall through to a flat lookup.
            .subcommand_required(true)
            .arg_required_else_help(true);
        for child in children.remove(&group_id).unwrap_or_default() {
            group_cmd = group_cmd.subcommand(child);
        }
        cmd = cmd.subcommand(group_cmd);
    }
    cmd
}

/// Handle a grafted plugin command. At Tier 0 there is no executor, so this
/// reports the command is plugin-provided and needs the runtime (#2095).
pub fn dispatch_plugin_command(matches: &ArgMatches) -> Result<()> {
    let (pc, path) = resolve_plugin_command(plugin_commands(), matches)?;
    println!(
        "'{path}' is a command from plugin '{}'. Running plugin commands needs the \
         plugin runtime, which is not available yet.",
        pc.plugin_id
    );
    Ok(())
}

/// Find the grafted command an invocation names, flat or nested under a group.
fn resolve_plugin_command(
    commands: Vec<PluginCommand>,
    matches: &ArgMatches,
) -> Result<(PluginCommand, String)> {
    let Some((name, sub)) = matches.subcommand() else {
        anyhow::bail!("no command given");
    };
    let (group, leaf, path) = match sub.subcommand() {
        Some((leaf, _)) => (Some(name), leaf, format!("{name} {leaf}")),
        None => (None, name, name.to_string()),
    };
    match commands
        .into_iter()
        .find(|pc| pc.name == leaf && pc.group.as_ref().map(|g| g.id.as_str()) == group)
    {
        Some(found) => Ok((found, path)),
        None => anyhow::bail!("unknown command '{path}'"),
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
            group: None,
        }
    }

    fn grouped(plugin_id: &str, group: &str, name: &str) -> PluginCommand {
        PluginCommand {
            plugin_id: plugin_id.to_string(),
            name: name.to_string(),
            title: String::new(),
            group: Some(PluginCommandGroup {
                id: group.to_string(),
                about: String::new(),
            }),
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
    fn graft_onto_nests_grouped_commands() {
        let commands = vec![
            grouped("linear", "linear", "refresh"),
            grouped("linear", "linear", "status"),
            // Within a group the first plugin to claim a leaf name wins.
            grouped("other.linear", "linear", "refresh"),
            // An ungrouped command keeps its flat top-level name.
            pc("linear", "linear_sync"),
        ];
        let cmd = graft_onto(Cli::command(), commands);

        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert_eq!(names.iter().filter(|n| **n == "linear").count(), 1);
        assert!(!names.contains(&"refresh"));
        assert!(names.contains(&"linear_sync"));

        let group = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "linear")
            .expect("group grafted");
        let leaves: Vec<&str> = group.get_subcommands().map(|s| s.get_name()).collect();
        assert_eq!(leaves, ["refresh", "status"]);
    }

    #[test]
    fn graft_onto_skips_group_colliding_with_core() {
        let commands = vec![
            grouped("acme.kit", "add", "refresh"),
            pc("acme.other", "add"),
            // `rm` is a core alias, so core still wins the token.
            grouped("acme.kit", "rm", "refresh"),
            pc("acme.other", "rm"),
        ];
        let cmd = graft_onto(Cli::command(), commands);
        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert_eq!(names.iter().filter(|n| **n == "add").count(), 1);
        assert!(!names.contains(&"rm"));
        // The rejected group does not flatten its commands to the top level.
        assert!(!names.contains(&"refresh"));
        let add = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "add")
            .expect("core add");
        assert_eq!(add.get_subcommands().count(), 0);
    }

    #[test]
    fn graft_onto_never_claims_clap_builtin_help() {
        let commands = vec![
            grouped("acme.kit", "help", "status"),
            pc("acme.other", "help"),
        ];
        let cmd = graft_onto(Cli::command(), commands);
        assert!(cmd.get_subcommands().all(|s| s.get_name() != "help"));
        // `aoe help` still reaches clap's own help rather than a plugin group.
        let err = cmd
            .try_get_matches_from(["aoe", "help"])
            .expect_err("help prints help");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn bare_group_verb_does_not_resolve_as_a_flat_command() {
        // A group verb needs its leaf, so `aoe linear` shows the group's help
        // instead of resolving to another plugin's flat `linear` command.
        let commands = vec![
            grouped("linear", "linear", "refresh"),
            pc("acme.other", "linear"),
        ];
        let err = graft_onto(Cli::command(), commands)
            .try_get_matches_from(["aoe", "linear"])
            .expect_err("a bare group verb is not a command");
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn resolve_finds_flat_and_nested_commands() {
        let commands = vec![
            grouped("linear", "linear", "refresh"),
            pc("acme.kit", "do-thing"),
        ];
        let nested = graft_onto(Cli::command(), commands);
        let nested = nested
            .try_get_matches_from(["aoe", "linear", "refresh"])
            .expect("nested parses");
        let (found, path) =
            resolve_plugin_command(vec![grouped("linear", "linear", "refresh")], &nested)
                .expect("nested resolves");
        assert_eq!(found.plugin_id, "linear");
        assert_eq!(path, "linear refresh");

        let flat = graft_onto(Cli::command(), vec![pc("acme.kit", "do-thing")])
            .try_get_matches_from(["aoe", "do-thing"])
            .expect("flat parses");
        let (found, path) =
            resolve_plugin_command(vec![pc("acme.kit", "do-thing")], &flat).expect("flat resolves");
        assert_eq!(found.plugin_id, "acme.kit");
        assert_eq!(path, "do-thing");
    }

    #[test]
    fn resolve_picks_the_leaf_of_the_named_group() {
        let commands = vec![
            grouped("linear", "linear", "refresh"),
            grouped("acme.kit", "acme", "refresh"),
        ];
        let matches = graft_onto(Cli::command(), commands)
            .try_get_matches_from(["aoe", "acme", "refresh"])
            .expect("args parse");
        let (found, path) = resolve_plugin_command(
            vec![
                grouped("linear", "linear", "refresh"),
                grouped("acme.kit", "acme", "refresh"),
            ],
            &matches,
        )
        .expect("resolves");
        assert_eq!(found.plugin_id, "acme.kit");
        assert_eq!(path, "acme refresh");
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
        for verb in [
            "--core-only",
            "--stop",
            "--status",
            "--restart",
            "--rollback",
        ] {
            let c = parse(&["aoe", "serve", verb]);
            assert!(
                !serve_start_blocked(&c, true),
                "{verb} must bypass the gate"
            );
        }
        assert!(!serve_start_blocked(&parse(&["aoe", "agents"]), true));
    }
}
