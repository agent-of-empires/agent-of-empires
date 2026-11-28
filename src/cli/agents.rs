//! `agent-of-empires agents` command implementation

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub enum AgentsCommand {
    /// Evaluate a detection manifest over a saved pane capture
    #[command(hide = true)]
    Detect(DetectArgs),
}

#[derive(Args)]
pub struct DetectArgs {
    /// Detection manifest (TOML)
    #[arg(long)]
    manifest: PathBuf,
    /// Pane capture to evaluate; ANSI escapes are stripped
    #[arg(long)]
    pane: PathBuf,
    /// Hook status file contents (`idle`, `running`, `waiting`, `error`); its mtime sets the age
    #[arg(long)]
    hook: Option<PathBuf>,
    /// OSC terminal title
    #[arg(long, default_value = "")]
    osc_title: String,
    /// Print the verdict as JSON
    #[arg(long)]
    json: bool,
}

#[tracing::instrument(target = "cli.agents", skip_all)]
pub fn run() -> Result<()> {
    let config = crate::session::Config::load_or_warn();
    let available = crate::tmux::AvailableTools::detect_with_custom_agents(&config.session);
    let available_list = available.available_list();
    let colorize = crate::cli::color_enabled();
    let installed_mark = if colorize {
        "\x1b[32m✓\x1b[0m"
    } else {
        "✓"
    };
    let missing_mark = if colorize {
        "\x1b[31m✗\x1b[0m"
    } else {
        "✗"
    };

    println!("Supported AI coding agents:\n");

    for agent in crate::agents::AGENTS
        .iter()
        .chain(crate::plugin::bundled_agents::available())
    {
        let installed = available_list.iter().any(|s| s == agent.name);
        if installed {
            println!("  {installed_mark} {:<12} installed", agent.name);
        } else {
            println!(
                "  {missing_mark} {:<12} not installed -- {}",
                agent.name, agent.install_hint
            );
        }
        if let Some(notice) = agent.lifecycle_notice() {
            println!("{}", crate::cli::lifecycle_notice_line("      ", &notice));
        }
    }
    let registry = crate::plugin::registry();
    let plugin_agents = crate::plugin::agent_integration::catalog(
        &registry,
        crate::agents::AGENTS.iter().map(|agent| agent.name),
        config
            .session
            .custom_agents
            .iter()
            .filter(|(_, command)| !command.trim().is_empty())
            .map(|(name, _)| name.as_str()),
    );
    for agent in &plugin_agents {
        if available_list
            .iter()
            .any(|installed| installed == agent.tool)
        {
            println!(
                "  {installed_mark} {:<12} installed (plugin: {})",
                agent.tool, agent.plugin
            );
        } else {
            println!(
                "  {missing_mark} {:<12} runtime not installed (plugin: {})",
                agent.tool, agent.plugin
            );
        }
    }

    let plugin_installed = plugin_agents
        .iter()
        .filter(|agent| {
            available_list
                .iter()
                .any(|installed| installed == agent.tool)
        })
        .count();
    let installed_count = plugin_installed
        + crate::agents::AGENTS
            .iter()
            .chain(crate::plugin::bundled_agents::available())
            .filter(|a| available_list.iter().any(|s| s == a.name))
            .count();
    let total_count = crate::agents::AGENTS.len()
        + crate::plugin::bundled_agents::available().count()
        + plugin_agents.len();

    println!("\n{}/{} agents installed.", installed_count, total_count);

    if installed_count == 0 {
        println!("\nInstall at least one agent to get started.");
        println!("Recommended: npm install -g @anthropic-ai/claude-code");
    }

    Ok(())
}

/// Exits 0 when a rule matched, 1 when none did, 2 when an input could not be used.
pub fn run_detect(args: DetectArgs) -> Result<()> {
    match detect(&args) {
        Ok(true) => Ok(()),
        Ok(false) => Err(super::Exit(1).into()),
        Err(e) => {
            eprintln!("Error: {e:#}");
            Err(super::Exit(2).into())
        }
    }
}

fn detect(args: &DetectArgs) -> Result<bool> {
    let read =
        |path: &PathBuf| std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    let source = String::from_utf8(read(&args.manifest)?).context("manifest is not UTF-8")?;
    let pane = String::from_utf8_lossy(&read(&args.pane)?).into_owned();
    let hook = args
        .hook
        .as_ref()
        .map(|path| -> Result<_> {
            let status = crate::hooks::parse_status(&read(path)?)
                .with_context(|| format!("{} holds no hook status", path.display()))?;
            let age = std::fs::metadata(path)?.modified()?.elapsed().ok();
            Ok(crate::tmux::detect::HookObservation { status, age })
        })
        .transpose()?;
    let observation = crate::tmux::detect::Observation::new(&args.osc_title, hook, true);
    let detection = crate::tmux::detect::detect_with_manifest_source(&source, &pane, &observation)?;
    let status = detection
        .as_ref()
        .and_then(|d| d.status)
        .map(|status| status.as_str());
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "matched": detection.is_some(),
                "rule": detection.as_ref().map(|d| &d.rule),
                "status": status,
                "visible": detection.as_ref().is_some_and(|d| d.visible),
                "diagnostic": detection.as_ref().and_then(|d| d.diagnostic.as_ref()),
            })
        );
    } else if let Some(d) = &detection {
        println!(
            "rule {} -> {}",
            d.rule,
            status.unwrap_or("no status change")
        );
    } else {
        println!("no rule matched");
    }
    Ok(detection.is_some())
}
