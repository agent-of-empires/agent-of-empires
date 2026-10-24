//! `aoe mcp` CLI: inspect the effective MCP server set (#1996) and manage AoE-owned servers.

use anyhow::{bail, Result};
use clap::{Subcommand, ValueEnum};

use super::serve::{AuthMode, DaemonStatus, ServeLaunch, ServeUrl};
use crate::server::ip_discovery::IpKind;

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
    /// Print an MCP client config for the running daemon's `/mcp` server, ready to paste into
    /// an agent's `mcpServers` (for example a Claude Code `.mcp.json`).
    Config,
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
        McpCommands::Config => config(profile).await,
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

/// Server name used in the printed `mcpServers` entry.
const CLIENT_ENTRY_NAME: &str = "aoe";

/// How long `aoe mcp config` waits for the daemon to answer its `/mcp` probe.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn config(profile: &str) -> Result<()> {
    let pid = match super::serve::daemon_status() {
        DaemonStatus::Verified(pid) => pid,
        DaemonStatus::Absent => bail!(
            "No aoe serve daemon is running, so there is no /mcp server to connect to.\n\
             Start one with: aoe serve --daemon"
        ),
        DaemonStatus::Unverified => bail!(
            "Could not confirm that the aoe serve daemon is running: its pid file is unreadable \
             or the process check failed.\n\
             Check it with: aoe serve --status"
        ),
    };
    if let Some(launch) = super::serve::running_launch(pid) {
        if let Some(reason) = unsupported_auth(&launch) {
            bail!("{reason}");
        }
    }
    let entry = client_entry(&super::serve::read_serve_urls())?;
    let configured = crate::session::config::profile_config::resolve_config_or_warn(profile)
        .web
        .mcp_enabled;
    match probe_mcp(&entry).await {
        RouteProbe::Rejected(status) => bail!(rejected_message(&entry, status)),
        probe => {
            if let Some(notice) = route_notice(&probe, configured) {
                eprintln!("{notice}");
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&entry.to_json())?);
    Ok(())
}

/// The URL and credential an MCP client uses to reach the daemon's `/mcp` route.
#[derive(Debug, PartialEq)]
struct ClientEntry {
    url: String,
    token: Option<String>,
}

impl ClientEntry {
    /// The `mcpServers` object printed for the client.
    fn to_json(&self) -> serde_json::Value {
        let mut server = serde_json::json!({ "type": "http", "url": self.url });
        if let Some(token) = &self.token {
            server["headers"] = serde_json::json!({ "Authorization": format!("Bearer {token}") });
        }
        serde_json::json!({ "mcpServers": { CLIENT_ENTRY_NAME: server } })
    }
}

/// The entry for the daemon advertised in `serve.url`. Prefers the loopback URL, which a remote
/// daemon lists second, because outside agents run on the daemon's host.
fn client_entry(urls: &[ServeUrl]) -> Result<ClientEntry> {
    let Some(chosen) = urls
        .iter()
        .find(|u| u.label.as_deref() == Some(IpKind::Loopback.label()))
        .or_else(|| urls.first())
    else {
        bail!("Daemon is running but the URL file is empty or missing.");
    };
    let base = reqwest::Url::parse(&chosen.url)
        .map_err(|e| anyhow::anyhow!("Daemon URL {:?} is not a valid URL: {e}", chosen.url))?;
    // `join` with an absolute path drops the query, so the token never lands in the URL.
    let url = base.join("/mcp")?.to_string();
    let token = super::url::extract_token(&chosen.url).map(str::to_owned);
    Ok(ClientEntry { url, token })
}

/// Why this daemon has no MCP client entry the command can print.
///
/// A passphrase wall covers every TCP caller, including loopback, and `/mcp`
/// takes that login only as a browser session cookie. A core-only daemon has
/// no TCP listener.
fn unsupported_auth(launch: &ServeLaunch) -> Option<String> {
    if launch.core_only {
        return Some(
            "The running daemon is core-only and does not listen on TCP, so it has no /mcp URL.\n\
             Start a dashboard daemon with: aoe serve --daemon"
                .into(),
        );
    }
    if !matches!(launch.auth_mode, AuthMode::Passphrase) && !launch.has_passphrase {
        return None;
    }
    let mode = if matches!(launch.auth_mode, AuthMode::Passphrase) {
        "--auth passphrase"
    } else if launch.remote {
        "--remote, which keeps a passphrase login session on top of its token"
    } else {
        "a passphrase login session on top of its token"
    };
    Some(format!(
        "The running daemon uses {mode}. /mcp accepts that login only as an aoe_session \
         cookie, not a bearer token, and the wall covers loopback callers too.\n\
         aoe mcp config cannot print a working entry for this mode. Restart the daemon with \
         --auth token and without --passphrase to give MCP clients a bearer token. A remote \
         daemon cannot drop its passphrase."
    ))
}

/// What the running daemon said about `/mcp` when probed with the printed entry.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RouteProbe {
    /// The route exists and accepted the credential.
    Served,
    /// The daemon has no `/mcp` route: it started with `web.mcp_enabled` off.
    Absent,
    /// The daemon refused the credential the entry carries.
    Rejected(reqwest::StatusCode),
    /// The daemon's auth rate limiter answered 429 before checking the credential or routing
    /// the request, so the reply says nothing about whether `/mcp` exists.
    RateLimited,
    /// The daemon could not be reached, so its route state is unknown.
    Unreachable,
}

impl RouteProbe {
    fn from_status(status: reqwest::StatusCode) -> Self {
        use reqwest::StatusCode;
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::Rejected(status),
            // Auth middleware rate-limits before route dispatch, even for an absent route.
            StatusCode::TOO_MANY_REQUESTS => Self::RateLimited,
            // Without the route, a `web` build's GET-only dashboard fallback answers 405.
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => Self::Absent,
            _ => Self::Served,
        }
    }
}

/// POST a JSON-RPC `ping` to `/mcp` with the entry's credential. The server is stateless, so
/// the probe leaves nothing behind on the daemon.
async fn probe_mcp(entry: &ClientEntry) -> RouteProbe {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .timeout(PROBE_TIMEOUT)
        .build()
    else {
        return RouteProbe::Unreachable;
    };
    let mut req = client
        .post(&entry.url)
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }));
    if let Some(token) = &entry.token {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) => RouteProbe::from_status(resp.status()),
        Err(_) => RouteProbe::Unreachable,
    }
}

/// The stderr note for a probe result, given the `web.mcp_enabled` value in `config.toml`.
/// The daemon reads that setting only at startup, so the two can disagree until a restart.
fn route_notice(probe: &RouteProbe, configured: bool) -> Option<String> {
    match (probe, configured) {
        (RouteProbe::Served, true) | (RouteProbe::Rejected(_), _) => None,
        (RouteProbe::Served, false) => Some(
            "note: the running daemon serves /mcp, but config.toml now turns web.mcp_enabled \
             off, so the route goes away at the next daemon restart."
                .into(),
        ),
        (RouteProbe::Absent, true) => Some(
            "warning: the running daemon does not serve /mcp. It started before \
             web.mcp_enabled was turned on, and reads the setting only at startup.\n\
             Apply it with: aoe serve --restart"
                .into(),
        ),
        (RouteProbe::Absent, false) => Some(
            "warning: the running daemon does not serve /mcp because web.mcp_enabled is off.\n\
             Turn it on in Settings or set `[web] mcp_enabled = true` in config.toml, then \
             run: aoe serve --restart"
                .into(),
        ),
        (RouteProbe::RateLimited, configured) => Some(format!(
            "warning: the daemon is rate-limiting authentication from this address after \
             repeated failed attempts, so this command could not confirm it serves /mcp. \
             config.toml has web.mcp_enabled {}, which the daemon applies only at startup.\n\
             Wait for the lockout to clear (up to 15 minutes), then run aoe mcp config again.",
            if configured { "on" } else { "off" }
        )),
        (RouteProbe::Unreachable, configured) => Some(format!(
            "warning: could not reach the daemon to confirm it serves /mcp. config.toml has \
             web.mcp_enabled {}, which the daemon applies only at startup.",
            if configured { "on" } else { "off" }
        )),
    }
}

/// The error for a daemon that refused the entry's credential on `/mcp`.
fn rejected_message(entry: &ClientEntry, status: reqwest::StatusCode) -> String {
    let cause = if entry.token.is_some() {
        "the daemon token in serve.url was not accepted; it may have just rotated, so run \
         aoe mcp config again"
    } else {
        "serve.url carries no token, and the daemon requires a credential this command cannot \
         provide, such as a passphrase login session"
    };
    format!(
        "The daemon answered /mcp with {status}: {cause}.\n\
         Check its auth mode with: aoe serve --status"
    )
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

    fn serve_urls(lines: &[(Option<&str>, &str)]) -> Vec<ServeUrl> {
        lines
            .iter()
            .map(|(label, url)| ServeUrl {
                label: label.map(Into::into),
                url: (*url).into(),
            })
            .collect()
    }

    #[test]
    fn client_entry_targets_loopback_mcp_with_the_daemon_token() {
        let bearer = |url: &str, token: &str| {
            serde_json::json!({ "mcpServers": { "aoe": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") },
            } } })
        };
        for (what, urls, expected) in [
            (
                "localhost-only daemon",
                serve_urls(&[(None, "http://localhost:8080/?token=abc")]),
                bearer("http://localhost:8080/mcp", "abc"),
            ),
            (
                "0.0.0.0 daemon lists LAN first and localhost last",
                serve_urls(&[
                    (None, "http://192.168.1.5:8080/?token=abc"),
                    (Some("lan"), "http://10.0.0.2:8080/?token=abc"),
                    (Some("localhost"), "http://localhost:8080/?token=abc"),
                ]),
                bearer("http://localhost:8080/mcp", "abc"),
            ),
            (
                "remote daemon lists the public URL first",
                serve_urls(&[
                    (None, "https://aoe.example.test/?token=rot"),
                    (Some("localhost"), "http://127.0.0.1:8080/?token=rot"),
                ]),
                bearer("http://127.0.0.1:8080/mcp", "rot"),
            ),
            (
                "daemon without a token gate",
                serve_urls(&[(None, "http://localhost:8080/")]),
                serde_json::json!({ "mcpServers": { "aoe": {
                    "type": "http",
                    "url": "http://localhost:8080/mcp",
                } } }),
            ),
            (
                "other query parameters are dropped with the token",
                serve_urls(&[(None, "http://localhost:8080/?foo=bar&token=zzz")]),
                bearer("http://localhost:8080/mcp", "zzz"),
            ),
        ] {
            assert_eq!(client_entry(&urls).unwrap().to_json(), expected, "{what}");
        }
    }

    #[test]
    fn client_entry_rejects_a_missing_or_malformed_url_file() {
        for (what, urls) in [
            ("empty url file", serve_urls(&[])),
            ("unparseable url", serve_urls(&[(None, "not a url")])),
        ] {
            assert!(client_entry(&urls).is_err(), "{what}");
        }
    }

    fn launch(
        auth_mode: AuthMode,
        behind_proxy: bool,
        remote: bool,
        has_passphrase: bool,
    ) -> ServeLaunch {
        ServeLaunch {
            schema: super::super::serve::SERVE_LAUNCH_SCHEMA,
            pid: 1,
            instance_id: None,
            profile: "default".into(),
            core_only: false,
            host: "127.0.0.1".into(),
            port: 8080,
            auth_mode,
            has_passphrase,
            behind_proxy,
            read_only: false,
            cityhall: false,
            remote,
            tunnel_name: None,
            tunnel_url: None,
            no_tailscale: false,
            allowed_host: Vec::new(),
            allowed_origin: Vec::new(),
            additional_fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn passphrase_wall_and_core_only_have_no_printable_entry() {
        // (auth, behind_proxy, remote, has_passphrase, unsupported). The passphrase wall covers
        // loopback, and a token daemon with a passphrase still requires the login cookie.
        for (auth, proxy, remote, has_passphrase, unsupported) in [
            (AuthMode::Passphrase, true, false, true, true),
            (AuthMode::Passphrase, false, false, true, true),
            (AuthMode::Token, true, false, false, false),
            (AuthMode::Token, false, true, true, true),
            (AuthMode::Token, false, true, false, false),
            (AuthMode::None, true, false, false, false),
            (AuthMode::None, false, false, false, false),
        ] {
            let reason = unsupported_auth(&launch(auth, proxy, remote, has_passphrase));
            assert_eq!(
                reason.is_some(),
                unsupported,
                "{auth:?} proxy={proxy} remote={remote} passphrase={has_passphrase}: {reason:?}"
            );
        }
        let reason = unsupported_auth(&launch(AuthMode::Passphrase, false, false, true)).unwrap();
        assert!(reason.contains("loopback"), "{reason}");
        assert!(reason.contains("--auth token"), "{reason}");

        let mut core = launch(AuthMode::None, false, false, false);
        core.core_only = true;
        let reason = unsupported_auth(&core).unwrap();
        assert!(reason.contains("core-only"), "{reason}");
    }

    #[test]
    fn probe_status_classifies_the_live_route() {
        use reqwest::StatusCode;
        for (status, expected) in [
            (StatusCode::OK, RouteProbe::Served),
            (StatusCode::ACCEPTED, RouteProbe::Served),
            (StatusCode::BAD_REQUEST, RouteProbe::Served),
            (StatusCode::NOT_ACCEPTABLE, RouteProbe::Served),
            (StatusCode::NOT_FOUND, RouteProbe::Absent),
            (StatusCode::METHOD_NOT_ALLOWED, RouteProbe::Absent),
            (
                StatusCode::UNAUTHORIZED,
                RouteProbe::Rejected(StatusCode::UNAUTHORIZED),
            ),
            (
                StatusCode::FORBIDDEN,
                RouteProbe::Rejected(StatusCode::FORBIDDEN),
            ),
            (StatusCode::TOO_MANY_REQUESTS, RouteProbe::RateLimited),
        ] {
            assert_eq!(RouteProbe::from_status(status), expected, "{status}");
        }
    }

    #[test]
    fn route_notice_follows_the_live_route_not_the_config_file() {
        let notice = |probe, configured| route_notice(&probe, configured).unwrap_or_default();
        assert_eq!(notice(RouteProbe::Served, true), "");
        // Turned off since startup: the route still works until the next restart.
        let off_since = notice(RouteProbe::Served, false);
        assert!(off_since.contains("next daemon restart"), "{off_since}");
        assert!(!off_since.contains("warning"), "{off_since}");
        // Turned on since startup: the route is still missing.
        let on_since = notice(RouteProbe::Absent, true);
        assert!(on_since.contains("started before"), "{on_since}");
        assert!(on_since.contains("aoe serve --restart"), "{on_since}");
        let off = notice(RouteProbe::Absent, false);
        assert!(off.contains("web.mcp_enabled is off"), "{off}");
        assert!(off.contains("aoe serve --restart"), "{off}");
        let unknown = notice(RouteProbe::Unreachable, true);
        assert!(unknown.contains("could not reach"), "{unknown}");
        assert!(unknown.contains("only at startup"), "{unknown}");
    }

    #[test]
    fn rate_limited_probe_is_not_route_evidence() {
        // A 429 comes from the auth limiter before routing, so neither configured value may
        // read as served or absent, and both must say to retry.
        for configured in [true, false] {
            let msg = route_notice(&RouteProbe::RateLimited, configured).unwrap_or_default();
            assert!(msg.starts_with("warning:"), "{msg}");
            assert!(msg.contains("could not confirm"), "{msg}");
            assert!(msg.contains("run aoe mcp config again"), "{msg}");
            assert!(!msg.contains("serves /mcp, but"), "{msg}");
            assert!(!msg.contains("does not serve"), "{msg}");
            let state = if configured {
                "enabled on"
            } else {
                "enabled off"
            };
            assert!(msg.contains(state), "{msg}");
        }
    }

    #[test]
    fn rejected_message_names_the_missing_credential() {
        let status = reqwest::StatusCode::UNAUTHORIZED;
        let tokenless = ClientEntry {
            url: "http://127.0.0.1:8080/mcp".into(),
            token: None,
        };
        let msg = rejected_message(&tokenless, status);
        assert!(msg.contains("no token"), "{msg}");
        assert!(msg.contains("aoe serve --status"), "{msg}");
        let with_token = ClientEntry {
            token: Some("abc".into()),
            ..tokenless
        };
        let msg = rejected_message(&with_token, status);
        assert!(msg.contains("rotated"), "{msg}");
        assert!(!msg.contains("abc"), "the token is never echoed: {msg}");
    }
}
