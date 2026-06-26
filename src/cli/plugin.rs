//! `aoe plugin`: plugin management (list, info, enable, disable, install,
//! update, uninstall).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum PluginCommands {
    /// List every known plugin with version, validation, and state
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
    /// Install an external plugin from a `gh:owner/repo[@ref]` slug or a local
    /// directory. Community plugins run at your own risk.
    Install {
        /// `gh:owner/repo[@ref]` or a local directory path
        source: String,
        /// Grant all requested capabilities without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Update an installed external plugin from its recorded source. Prompts to
    /// re-approve capabilities if the update changes the capability set.
    Update {
        /// Plugin id
        id: String,
    },
    /// Uninstall an external plugin, removing its files and capability grant
    Uninstall {
        /// Plugin id
        id: String,
    },
    /// Print the deterministic source tree hash for a plugin directory, the
    /// value a maintainer pins in the featured index
    Hash {
        /// Path to the plugin directory
        path: String,
    },
}

pub async fn run(command: PluginCommands) -> Result<()> {
    match command {
        PluginCommands::List => run_list(),
        PluginCommands::Info { id } => run_info(&id),
        PluginCommands::Enable { id } => run_set_enabled(&id, true),
        PluginCommands::Disable { id } => run_set_enabled(&id, false),
        PluginCommands::Install { source, yes } => run_install(&source, yes).await,
        PluginCommands::Update { id } => run_update(&id).await,
        PluginCommands::Uninstall { id } => run_uninstall(&id),
        PluginCommands::Hash { path } => run_hash(&path),
    }
}

fn run_hash(path: &str) -> Result<()> {
    let hash = crate::plugin::integrity::tree_hash(std::path::Path::new(path))?;
    println!("{hash}");
    Ok(())
}

fn state_label(plugin: &crate::plugin::LoadedPlugin) -> &'static str {
    if !plugin.enabled {
        "disabled"
    } else if plugin.needs_reapproval() {
        "needs approval"
    } else {
        "enabled"
    }
}

fn run_list() -> Result<()> {
    let registry = crate::plugin::registry();
    if registry.all().is_empty() {
        println!("No plugins installed.");
    } else {
        println!("{:<20} {:<9} {:<12} STATE", "ID", "VERSION", "VALIDATION");
        for plugin in registry.all() {
            println!(
                "{:<20} {:<9} {:<12} {}",
                plugin.id(),
                plugin.manifest.version,
                plugin.validation.as_str(),
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
    println!("  version:    {}", m.version);
    println!("  validation: {}", plugin.validation.as_str());
    println!("  state:      {}", state_label(plugin));
    if let Some(source) = &plugin.source {
        println!("  source:     {source}");
    }
    if m.capabilities.is_empty() {
        println!("  caps:       none");
    } else {
        let caps: Vec<&str> = m.capabilities.iter().map(|c| c.as_str()).collect();
        println!(
            "  caps:       {} ({})",
            caps.join(", "),
            if plugin.granted {
                "granted"
            } else {
                "not granted"
            }
        );
    }
    if !m.ui.is_empty() {
        println!("  ui:");
        for u in &m.ui {
            println!("    - {} ({})", u.slot.as_str(), u.id);
        }
    }
    if !m.description.is_empty() {
        println!("  about:      {}", m.description);
    }
    if !m.keybinds.is_empty() {
        println!("  keybinds:");
        for kb in &m.keybinds {
            // A core binding on the same chord always wins; flag the conflict so
            // the author knows the plugin keybind will never fire (#2094).
            // An unparseable key is skipped by the TUI resolver, so flag it
            // here rather than print it as if it were usable.
            let note = match crate::tui::home::bindings::parse_chord(&kb.key) {
                Some(c) if crate::tui::home::bindings::core_shadows(&c) => "  (shadowed by core)",
                Some(_) => "",
                None => "  (invalid key, ignored)",
            };
            println!("    {} -> {}{note}", kb.key, kb.command);
        }
    }
    Ok(())
}

fn run_set_enabled(id: &str, enabled: bool) -> Result<()> {
    crate::plugin::install::set_enabled(id, enabled)?;
    println!("{} {id}.", if enabled { "Enabled" } else { "Disabled" });
    Ok(())
}

fn print_report(report: &crate::plugin::install::InstallReport, verb: &str) {
    println!("{verb} {} {}.", report.id, report.version);
    if report.capabilities.is_empty() {
        println!("  capabilities: none");
    } else {
        println!(
            "  capabilities: {} ({})",
            report.capabilities.join(", "),
            if report.granted {
                "granted"
            } else {
                "not granted, plugin inactive"
            }
        );
    }
}

async fn run_install(source: &str, yes: bool) -> Result<()> {
    let report = crate::plugin::install::install(source, yes).await?;
    print_report(&report, "Installed");
    Ok(())
}

async fn run_update(id: &str) -> Result<()> {
    let report = crate::plugin::install::update(id).await?;
    print_report(&report, "Updated");
    Ok(())
}

fn run_uninstall(id: &str) -> Result<()> {
    crate::plugin::install::uninstall(id)?;
    println!("Uninstalled {id}.");
    Ok(())
}
