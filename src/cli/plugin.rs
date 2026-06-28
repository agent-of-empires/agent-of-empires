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
    /// directory. With no `@ref`, installs the repo's latest release; an
    /// explicit `@ref` installs unverified, un-audited code. Community plugins
    /// run at your own risk.
    Install {
        /// `gh:owner/repo` (latest release) or `gh:owner/repo@ref` (unverified)
        /// or a local directory path
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
    /// Search GitHub's `aoe-plugin` topic for installable plugins
    Discover {
        /// Optional free-text term to narrow the search
        query: Option<String>,
    },
    /// List installed external plugins that have an update available
    Outdated,
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
        PluginCommands::Discover { query } => run_discover(query.as_deref()).await,
        PluginCommands::Outdated => run_outdated().await,
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

fn format_report(report: &crate::plugin::install::InstallReport, verb: &str) -> String {
    let mut out = format!("{verb} {} {}.\n", report.id, report.version);
    out.push_str(&format!("  validation: {}\n", report.validation.as_str()));
    out.push_str("  capabilities: ");
    if report.capabilities.is_empty() {
        out.push_str("none");
    } else {
        out.push_str(&report.capabilities.join(", "));
    }
    // Surface inactivity whenever the grant did not cover the install, including
    // the empty-capabilities case (declining a UI-only manifest change leaves a
    // plugin ungranted with no capabilities to list).
    if !report.granted {
        out.push_str(" (not granted, plugin inactive)");
    } else if !report.capabilities.is_empty() {
        out.push_str(" (granted)");
    }
    out
}

fn print_report(report: &crate::plugin::install::InstallReport, verb: &str) {
    println!("{}", format_report(report, verb));
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

async fn run_discover(query: Option<&str>) -> Result<()> {
    let results = crate::plugin::discover::discover(query).await?;
    if results.is_empty() {
        println!("No plugins found on the `aoe-plugin` topic.");
        return Ok(());
    }
    println!("{:<11} {:<6} {:<32} ABOUT", "BADGE", "STARS", "SOURCE");
    for r in &results {
        let about = r.description.as_deref().unwrap_or("");
        println!(
            "{:<11} {:<6} {:<32} {}",
            r.badge.as_str(),
            r.stars,
            r.slug,
            about
        );
    }
    println!("\nInstall with:\n  aoe plugin install <source>");
    Ok(())
}

async fn run_outdated() -> Result<()> {
    let statuses = crate::plugin::update_check::outdated().await;
    if statuses.is_empty() {
        println!("No external plugins installed.");
        return Ok(());
    }
    let mut any_outdated = false;
    for s in &statuses {
        if let Some(err) = &s.error {
            println!("error         {:<20} {}", s.id, err);
        } else if s.needs_update {
            any_outdated = true;
            let available = s.available.as_deref().unwrap_or("modified");
            println!("needs update  {:<20} {} -> {}", s.id, s.current, available);
        } else {
            println!("up to date    {:<20} {}", s.id, s.current);
        }
    }
    if any_outdated {
        println!("\nUpdate with:\n  aoe plugin update <id>");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_report;
    use crate::plugin::install::InstallReport;
    use crate::plugin::registry::ValidationState;

    #[test]
    fn report_shows_validation_line() {
        let report = InstallReport {
            id: "acme.foo".into(),
            version: "1.2.3".into(),
            capabilities: vec!["session.read".into(), "filesystem.read".into()],
            granted: true,
            validation: ValidationState::Community,
        };
        let out = format_report(&report, "Installed");
        assert_eq!(
            out,
            "Installed acme.foo 1.2.3.\n  validation: community\n  capabilities: session.read, filesystem.read (granted)"
        );
    }

    #[test]
    fn local_install_validation_labelled_local() {
        let report = InstallReport {
            id: "acme.foo".into(),
            version: "0.1.0".into(),
            capabilities: vec![],
            granted: true,
            validation: ValidationState::Local,
        };
        let out = format_report(&report, "Installed");
        assert!(
            out.contains("\n  validation: local\n"),
            "local install surfaces its validation: {out:?}"
        );
    }

    #[test]
    fn inactive_with_no_capabilities_still_warns() {
        // An ungranted update with no capabilities (e.g. a declined UI-only
        // manifest change) must still flag that the plugin is inactive.
        let report = InstallReport {
            id: "acme.foo".into(),
            version: "0.1.0".into(),
            capabilities: vec![],
            granted: false,
            validation: ValidationState::Community,
        };
        let out = format_report(&report, "Updated");
        assert!(
            out.ends_with("  capabilities: none (not granted, plugin inactive)"),
            "inactivity is surfaced with no capabilities: {out:?}"
        );
    }
}
