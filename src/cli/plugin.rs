//! `aoe plugin`: plugin management (list, info, enable, disable).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum PluginCommands {
    /// List every known plugin with version and state
    List,
    /// Show one plugin's manifest details
    Info {
        /// Plugin id, e.g. `aoe.web`
        id: String,
    },
    /// Enable a plugin's contributions
    Enable {
        /// Plugin id
        id: String,
    },
    /// Disable a plugin; its settings stay on disk for re-enabling
    Disable {
        /// Plugin id
        id: String,
    },
}

pub fn run(command: PluginCommands) -> Result<()> {
    match command {
        PluginCommands::List => run_list(),
        PluginCommands::Info { id } => run_info(&id),
        PluginCommands::Enable { id } => run_set_enabled(&id, true),
        PluginCommands::Disable { id } => run_set_enabled(&id, false),
    }
}

fn state_label(plugin: &crate::plugin::LoadedPlugin) -> &'static str {
    if plugin.enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn run_list() -> Result<()> {
    let registry = crate::plugin::registry();
    if registry.all().is_empty() {
        println!("No plugins installed.");
    } else {
        println!("{:<18} {:<9} STATE", "ID", "VERSION");
        for plugin in registry.all() {
            println!(
                "{:<18} {:<9} {}",
                plugin.id(),
                plugin.manifest.version,
                state_label(plugin),
            );
        }
    }
    for err in registry.load_errors() {
        eprintln!("warning: {err}");
    }
    Ok(())
}

fn run_info(id: &str) -> Result<()> {
    let registry = crate::plugin::registry();
    let Some(plugin) = registry.get(id) else {
        anyhow::bail!("unknown plugin {id:?}; see `aoe plugin list`");
    };
    let m = &plugin.manifest;
    println!("{} ({})", m.name, m.id);
    println!("  version:  {}", m.version);
    println!("  state:    {}", state_label(plugin));
    if !m.description.is_empty() {
        println!("  about:    {}", m.description);
    }
    Ok(())
}

fn run_set_enabled(id: &str, enabled: bool) -> Result<()> {
    crate::plugin::install::set_enabled(id, enabled)?;
    println!("{} {id}.", if enabled { "Enabled" } else { "Disabled" });
    Ok(())
}
