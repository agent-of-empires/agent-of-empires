//! `aoe mcp` CLI: inspect the effective MCP server set (#1996) and manage AoE-owned servers.

use anyhow::{bail, Result};
use clap::{Subcommand, ValueEnum};

use crate::session::mcp::mcp_model::{resolve_surface, McpSurfaceView};
use crate::session::mcp::mcp_overrides::{
    self, classify_missing, McpServerInput, McpWriteError, SecretEntry,
};
use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

#[derive(Subcommand, Debug)]
pub enum McpCommands {
    /// List the merged effective MCP server set with provenance, plus any
    /// conflicts and servers kept after removal from a native config.
    List(McpListArgs),
    /// Add an AoE-owned MCP server to the global mcp.json.
    Add(McpAddArgs),
    /// Edit an AoE-owned MCP server. Secrets you do not mention keep their stored value.
    Edit(McpEditArgs),
    /// Remove an AoE-owned MCP server.
    Remove(McpRemoveArgs),
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum TransportArg {
    Stdio,
    Http,
    Sse,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| "expected KEY=VALUE".to_string())
}

#[derive(clap::Args, Debug)]
pub struct McpAddArgs {
    /// Server name (letters, digits, '.', '_', '-').
    pub name: String,

    #[arg(long, value_enum, default_value = "stdio")]
    pub transport: TransportArg,

    /// Executable to run (stdio only).
    #[arg(long)]
    pub command: Option<String>,

    /// Argument for the command; repeat for several (stdio only).
    #[arg(long = "arg", allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Environment variable as KEY=VALUE; repeatable (stdio only).
    #[arg(long, value_parser = parse_kv)]
    pub env: Vec<(String, String)>,

    /// Server URL (http and sse only).
    #[arg(long)]
    pub url: Option<String>,

    /// Request header as KEY=VALUE; repeatable (http and sse only).
    #[arg(long, value_parser = parse_kv)]
    pub header: Vec<(String, String)>,
}

#[derive(clap::Args, Debug)]
pub struct McpEditArgs {
    /// Name of the AoE-owned server to edit. Its transport cannot change; remove and add instead.
    pub name: String,

    /// Replace the executable (stdio only).
    #[arg(long)]
    pub command: Option<String>,

    /// Replace all arguments; repeat for several (stdio only).
    #[arg(long = "arg", allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Remove all arguments (stdio only).
    #[arg(long, conflicts_with = "args")]
    pub clear_args: bool,

    /// Set an environment variable as KEY=VALUE; repeatable (stdio only).
    #[arg(long, value_parser = parse_kv)]
    pub env: Vec<(String, String)>,

    /// Remove an environment variable by name; repeatable (stdio only).
    #[arg(long)]
    pub unset_env: Vec<String>,

    /// Replace the URL (http and sse only).
    #[arg(long)]
    pub url: Option<String>,

    /// Set a header as KEY=VALUE; repeatable (http and sse only).
    #[arg(long, value_parser = parse_kv)]
    pub header: Vec<(String, String)>,

    /// Remove a header by name; repeatable (http and sse only).
    #[arg(long)]
    pub unset_header: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct McpRemoveArgs {
    /// Name of the AoE-owned server to remove.
    pub name: String,
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
        McpCommands::Add(args) => add(profile, args),
        McpCommands::Edit(args) => edit(profile, args),
        McpCommands::Remove(args) => remove(profile, args),
    }
}

fn default_agent(profile: &str) -> String {
    crate::session::config::profile_config::resolve_config_or_warn(profile)
        .session
        .default_tool
        .unwrap_or_else(|| "claude".to_string())
}

/// Turns a write failure into the CLI error, saying "read-only" when another layer owns the name.
fn write_failure(error: McpWriteError, profile: &str) -> anyhow::Error {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let profile_opt = (!profile.is_empty()).then_some(profile);
    match classify_missing(error, &default_agent(profile), profile_opt, &cwd) {
        McpWriteError::Io(e) => e,
        McpWriteError::Exists(name) => {
            anyhow::anyhow!(
                "an AoE-owned server named \"{name}\" already exists; use `aoe mcp edit`"
            )
        }
        McpWriteError::Invalid(m) | McpWriteError::NotFound(m) | McpWriteError::ReadOnly(m) => {
            anyhow::anyhow!(m)
        }
    }
}

fn secrets(pairs: Vec<(String, String)>) -> Vec<SecretEntry> {
    pairs
        .into_iter()
        .map(|(name, value)| SecretEntry {
            name,
            value: Some(value),
        })
        .collect()
}

fn reject_flags(transport: &str, flags: &[(&str, bool)]) -> Result<()> {
    if let Some((flag, _)) = flags.iter().find(|(_, given)| *given) {
        bail!("{flag} does not apply to a {transport} server");
    }
    Ok(())
}

fn add(profile: &str, args: McpAddArgs) -> Result<()> {
    let input = match args.transport {
        TransportArg::Stdio => {
            reject_flags(
                "stdio",
                &[
                    ("--url", args.url.is_some()),
                    ("--header", !args.header.is_empty()),
                ],
            )?;
            let Some(command) = args.command else {
                bail!("--command is required for a stdio server");
            };
            McpServerInput::Stdio {
                command,
                args: args.args,
                env: secrets(args.env),
            }
        }
        TransportArg::Http | TransportArg::Sse => {
            reject_flags(
                "http/sse",
                &[
                    ("--command", args.command.is_some()),
                    ("--arg", !args.args.is_empty()),
                    ("--env", !args.env.is_empty()),
                ],
            )?;
            let Some(url) = args.url else {
                bail!("--url is required for an http or sse server");
            };
            let headers = secrets(args.header);
            if matches!(args.transport, TransportArg::Http) {
                McpServerInput::Http { url, headers }
            } else {
                McpServerInput::Sse { url, headers }
            }
        }
    };
    let server = mcp_overrides::create_global_server(&args.name, &input)
        .map_err(|e| write_failure(e, profile))?;
    println!("Added {}", server.redacted_summary());
    Ok(())
}

/// Names of `stored` minus `unset` and `set`, then `set`: kept secrets carry no value so the
/// model keeps what is stored.
fn merged_secrets(
    stored: &std::collections::BTreeMap<String, String>,
    unset: &[String],
    set: Vec<(String, String)>,
) -> Vec<SecretEntry> {
    let kept: Vec<_> = stored
        .keys()
        .filter(|k| !unset.contains(k) && !set.iter().any(|(n, _)| n == *k))
        .map(|name| SecretEntry {
            name: name.clone(),
            value: None,
        })
        .collect();
    kept.into_iter().chain(secrets(set)).collect()
}

fn edit(profile: &str, args: McpEditArgs) -> Result<()> {
    let server = mcp_overrides::update_global_server_with(&args.name, |stored| {
        let stored = stored.ok_or_else(|| {
            McpWriteError::Invalid(format!("\"{}\" in global mcp.json is malformed", args.name))
        })?;
        edit_input(stored, &args).map_err(|e| McpWriteError::Invalid(e.to_string()))
    })
    .map_err(|e| write_failure(e, profile))?;
    println!("Updated {}", server.redacted_summary());
    Ok(())
}

/// The edited definition: unset flags keep the stored value, secrets merge by name.
fn edit_input(stored: &ProjectMcpServer, args: &McpEditArgs) -> Result<McpServerInput> {
    let is_http = stored.kind() == "http";
    Ok(match stored.transport.clone() {
        ProjectMcpTransport::Stdio {
            command,
            args: old_args,
            env,
        } => {
            reject_flags(
                "stdio",
                &[
                    ("--url", args.url.is_some()),
                    ("--header", !args.header.is_empty()),
                    ("--unset-header", !args.unset_header.is_empty()),
                ],
            )?;
            McpServerInput::Stdio {
                command: args.command.clone().unwrap_or(command),
                args: if args.clear_args {
                    Vec::new()
                } else if args.args.is_empty() {
                    old_args
                } else {
                    args.args.clone()
                },
                env: merged_secrets(&env, &args.unset_env, args.env.clone()),
            }
        }
        ProjectMcpTransport::Http { url, headers } | ProjectMcpTransport::Sse { url, headers } => {
            reject_flags(
                "http/sse",
                &[
                    ("--command", args.command.is_some()),
                    ("--arg", !args.args.is_empty()),
                    ("--clear-args", args.clear_args),
                    ("--env", !args.env.is_empty()),
                    ("--unset-env", !args.unset_env.is_empty()),
                ],
            )?;
            let url = args.url.clone().unwrap_or(url);
            let headers = merged_secrets(&headers, &args.unset_header, args.header.clone());
            if is_http {
                McpServerInput::Http { url, headers }
            } else {
                McpServerInput::Sse { url, headers }
            }
        }
    })
}

fn remove(profile: &str, args: McpRemoveArgs) -> Result<()> {
    mcp_overrides::delete_global_server(&args.name).map_err(|e| write_failure(e, profile))?;
    println!("Removed {}", args.name);
    Ok(())
}

fn list(profile: &str, args: McpListArgs) -> Result<()> {
    let agent = args.agent.unwrap_or_else(|| default_agent(profile));
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

fn transport_detail(r: &crate::session::mcp::mcp_model::RedactedMcpServer) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(args: &[&str]) -> ProjectMcpServer {
        ProjectMcpServer {
            name: "fs".into(),
            transport: ProjectMcpTransport::Stdio {
                command: "mcp-fs".into(),
                args: args.iter().map(ToString::to_string).collect(),
                env: Default::default(),
            },
        }
    }

    fn edit_args(argv: &[&str]) -> McpEditArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            edit: McpEditArgs,
        }
        Wrap::try_parse_from(
            std::iter::once("t")
                .chain(["fs"])
                .chain(argv.iter().copied()),
        )
        .unwrap()
        .edit
    }

    fn stdio_args(input: McpServerInput) -> Vec<String> {
        match input {
            McpServerInput::Stdio { args, .. } => args,
            _ => panic!("stdio"),
        }
    }

    #[test]
    fn edit_args_keep_replace_clear_and_empty_string_differ() {
        let old = stored(&["--root", "."]);
        let run = |argv: &[&str]| stdio_args(edit_input(&old, &edit_args(argv)).unwrap());
        assert_eq!(run(&[]), ["--root", "."]);
        assert_eq!(run(&["--arg", "x"]), ["x"]);
        assert_eq!(run(&["--arg="]), [""]);
        assert!(run(&["--clear-args"]).is_empty());
    }

    #[test]
    fn clear_args_conflicts_with_arg_and_rejects_remote_servers() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            edit: McpEditArgs,
        }
        assert!(Wrap::try_parse_from(["t", "fs", "--clear-args", "--arg", "x"]).is_err());

        let remote = ProjectMcpServer {
            name: "r".into(),
            transport: ProjectMcpTransport::Http {
                url: "https://e/mcp".into(),
                headers: Default::default(),
            },
        };
        assert!(edit_input(&remote, &edit_args(&["--clear-args"])).is_err());
    }
}
