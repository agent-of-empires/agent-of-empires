//! `aoe mcp` CLI: inspect the effective MCP server set (#1996).
//!
//! Mirrors the read model the web and TUI surfaces render, so a user can debug
//! "which MCP servers will my agent reach, and where did each come from" without
//! a serve build. Top-level (not under the serve-gated `aoe acp` group) and
//! always compiled, because inspecting config is useful before any session runs.
//! Every value is redacted: command/args/url identify a server, env and header
//! VALUES are reduced to names.

use anyhow::Result;
use clap::Subcommand;

use crate::session::mcp_model::{resolve_surface, McpSurfaceView};

#[derive(Subcommand, Debug)]
pub enum McpCommands {
    /// List the merged effective MCP server set with provenance, plus any
    /// conflicts and servers kept after removal from a native config.
    List(McpListArgs),
}

#[derive(clap::Args, Debug)]
pub struct McpListArgs {
    /// Agent whose effective set to resolve. Defaults to the configured default
    /// tool. MCP forwarding is per-agent because the agent-native layer differs.
    #[arg(long)]
    pub agent: Option<String>,

    /// Output machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(profile: &str, command: McpCommands) -> Result<()> {
    match command {
        McpCommands::List(args) => list(profile, args),
    }
}

fn list(profile: &str, args: McpListArgs) -> Result<()> {
    let agent = args.agent.unwrap_or_else(|| {
        crate::session::profile_config::resolve_config_or_warn(profile)
            .session
            .default_tool
            .unwrap_or_else(|| "claude".to_string())
    });
    let profile_opt = (!profile.is_empty()).then_some(profile);
    let cwd = std::env::current_dir()?;

    let view = resolve_surface(&agent, profile_opt, &cwd);

    if args.json {
        print_json(&agent, &view);
    } else {
        print_table(&agent, &view);
    }
    Ok(())
}

fn print_json(agent: &str, view: &McpSurfaceView) {
    let conflicts: Vec<_> = view
        .conflicts
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.current.name,
                "agent": c.agent,
                // Redacted one-line summaries: never the raw env/header values.
                "previous": c.previous.redacted_summary(),
                "current": c.current.redacted_summary(),
            })
        })
        .collect();
    let out = serde_json::json!({
        "agent": agent,
        "effective": view.effective.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
        "keptOnRemoval": view.kept_on_removal.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
        "conflicts": conflicts,
        "driftPaused": view.drift_paused,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

fn print_table(agent: &str, view: &McpSurfaceView) {
    println!("MCP servers for agent `{agent}`:\n");
    if view.effective.is_empty() {
        println!("  (no servers forwarded)");
    } else {
        for s in &view.effective {
            let r = s.redacted();
            print!("  {}  [{}]  {}", r.name, r.provenance, transport_detail(&r));
            if !r.shadowed.is_empty() {
                print!("  (shadows {})", r.shadowed.join(", "));
            }
            println!();
        }
    }

    if !view.kept_on_removal.is_empty() {
        println!("\nKept after removal from the native config (not forwarded; keep or drop):");
        for s in &view.kept_on_removal {
            let r = s.redacted();
            println!("  {}  {}", r.name, transport_detail(&r));
        }
    }

    if !view.conflicts.is_empty() {
        println!("\nConflicts (native config changed since AoE last saw it):");
        for c in &view.conflicts {
            println!("  {}", c.current.name);
            println!("    AoE:    {}", c.previous.redacted_summary());
            println!("    native: {}", c.current.redacted_summary());
        }
    }

    if view.drift_paused {
        println!(
            "\nNote: drift detection is paused for `{agent}` because its native MCP \
             config has a malformed entry."
        );
    }
}

fn transport_detail(r: &crate::session::mcp_model::RedactedMcpServer) -> String {
    let mut s = match (&r.command, &r.url) {
        (Some(command), _) => {
            let mut d = format!("{} ({})", command, r.transport);
            if !r.args.is_empty() {
                d.push(' ');
                d.push_str(&r.args.join(" "));
            }
            d
        }
        (None, Some(url)) => format!("{} ({})", url, r.transport),
        (None, None) => r.transport.to_string(),
    };
    if !r.env_names.is_empty() {
        s.push_str(&format!("  [env: {}]", r.env_names.join(", ")));
    }
    if !r.header_names.is_empty() {
        s.push_str(&format!("  [headers: {}]", r.header_names.join(", ")));
    }
    s
}
