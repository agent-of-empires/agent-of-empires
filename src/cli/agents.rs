//! `hmp agents` command implementation
//!
//! Lists all supported agents, shows which are installed, and prints
//! install commands for missing ones.

use anyhow::Result;

#[tracing::instrument(target = "cli.agents", skip_all)]
pub fn run() -> Result<()> {
    let available = crate::tmux::AvailableTools::detect();
    let available_list = available.available_list();

    println!("Supported AI coding agents:\n");

    for agent in crate::agents::AGENTS {
        let installed = available_list.iter().any(|s| s == agent.name);
        if installed {
            println!("  \x1b[32m✓\x1b[0m {:<12} installed", agent.name);
        } else {
            println!(
                "  \x1b[31m✗\x1b[0m {:<12} not installed -- {}",
                agent.name, agent.install_hint
            );
        }
    }

    let installed_count = crate::agents::AGENTS
        .iter()
        .filter(|a| available_list.iter().any(|s| s == a.name))
        .count();

    println!(
        "\n{}/{} agents installed.",
        installed_count,
        crate::agents::AGENTS.len()
    );

    if installed_count == 0 {
        println!("\nInstall at least one agent to get started.");
        println!("Recommended: npm install -g @anthropic-ai/claude-code");
    }

    Ok(())
}
