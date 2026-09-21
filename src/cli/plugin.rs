//! `aoe plugin`: plugin management (list, info, enable, disable, install,

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
    /// Update an installed external plugin from its recorded source and restart
    /// its worker in a running daemon. Prompts to re-approve capabilities if the
    /// update changes the capability set.
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
        PluginCommands::Enable { id } => run_set_enabled(&id, true).await,
        PluginCommands::Disable { id } => run_set_enabled(&id, false).await,
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

async fn run_set_enabled(id: &str, enabled: bool) -> Result<()> {
    use crate::plugin::install::LiveToggle;
    let outcome = crate::plugin::install::set_enabled_live(id, enabled).await?;
    println!("{} {id}.", if enabled { "Enabled" } else { "Disabled" });
    match outcome {
        LiveToggle::Daemon => println!("  the running daemon reconciled its workers."),
        LiveToggle::Local => {}
        LiveToggle::LocalDaemonStale { reason } => println!(
            "  warning: a daemon is running but was not updated ({reason}); restart it or toggle from the dashboard."
        ),
    }
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
    use crate::plugin::install::LiveRestart;
    let report = crate::plugin::install::update(id).await?;
    print_report(&report, "Updated");
    match crate::plugin::install::restart_worker_live(id).await {
        LiveRestart::Daemon => println!("  the running daemon reloaded the plugin."),
        LiveRestart::NoDaemon => {}
        LiveRestart::DaemonStale { reason } => println!(
            "  warning: a daemon is running but did not reload the plugin ({reason}); its worker keeps the previous build until the daemon restarts."
        ),
    }
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
    fn format_report_surfaces_validation_and_grant_state() {
        let report =
            |version: &str, capabilities: Vec<String>, granted, validation| InstallReport {
                id: "acme.foo".into(),
                version: version.into(),
                capabilities,
                granted,
                validation,
            };

        let granted = report(
            "1.2.3",
            vec!["session.read".into(), "filesystem.read".into()],
            true,
            ValidationState::Community,
        );
        assert_eq!(
            format_report(&granted, "Installed"),
            "Installed acme.foo 1.2.3.\n  validation: community\n  capabilities: session.read, filesystem.read (granted)"
        );

        let local = report("0.1.0", vec![], true, ValidationState::Local);
        let out = format_report(&local, "Installed");
        assert!(
            out.contains("\n  validation: local\n"),
            "a local install surfaces its validation: {out:?}"
        );

        let inactive = report("0.1.0", vec![], false, ValidationState::Community);
        let out = format_report(&inactive, "Updated");
        assert!(
            out.ends_with("  capabilities: none (not granted, plugin inactive)"),
            "inactivity is surfaced with no capabilities: {out:?}"
        );
    }
}
