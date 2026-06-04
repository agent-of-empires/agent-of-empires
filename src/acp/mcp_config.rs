//! Global MCP server config (`<app_dir>/mcp.json`) parsing and conversion
//! to ACP `McpServer` values forwarded at session creation.
//!
//! AoE forwards the user's MCP servers to structured-view ACP agents through
//! `session/new` and `session/load`; without this the agent reaches no MCP
//! servers at all. The on-disk format mirrors the ecosystem-standard
//! `.mcp.json` so users can reuse the definitions they already maintain for
//! Claude, Gemini, and Codex:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs":     { "command": "mcp-fs", "args": ["--root", "."], "env": { "K": "v" } },
//!     "remote": { "type": "http", "url": "https://example/mcp", "headers": { "Authorization": "Bearer x" } }
//!   }
//! }
//! ```
//!
//! Only a single global file in the AoE app dir is read today. A project-local
//! `cwd/.mcp.json` is intentionally NOT read: AoE opens cloned and potentially
//! untrusted repositories, and stdio MCP servers launch unconditionally when a
//! session spawns, so project scope must sit behind the repo-trust gate (the
//! same boundary that already protects lifecycle hooks). That, plus per-profile
//! config, are tracked as follow-ups.

use std::collections::BTreeMap;
use std::path::Path;

use agent_client_protocol::schema::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::warn;

/// On-disk shape of `<app_dir>/mcp.json`. Unknown top-level keys are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawServer>,
}

/// A single server entry. Absent `type` (or `type: "stdio"`) selects the stdio
/// transport; `type: "http"` / `type: "sse"` select the remote transports. Maps
/// are `BTreeMap` so the converted output ordering is deterministic for tests.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(default, rename = "type")]
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Read and parse the global `<app_dir>/mcp.json`. A missing file yields an
/// empty list (the no-config case behaves exactly as before this feature). A
/// present-but-malformed file is an error the caller surfaces; it must not be
/// silently treated as "no servers".
pub fn load_global_mcp_servers(app_dir: &Path) -> Result<Vec<McpServer>> {
    let path = app_dir.join("mcp.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading MCP config at {}", path.display()))
        }
    };
    parse_mcp_servers(&text).with_context(|| format!("parsing MCP config at {}", path.display()))
}

/// Parse the `.mcp.json` text into ACP `McpServer` values. Separated from the
/// file read so the conversion rules can be unit-tested without touching disk.
fn parse_mcp_servers(text: &str) -> Result<Vec<McpServer>> {
    let parsed: McpConfigFile = serde_json::from_str(text)?;
    parsed
        .mcp_servers
        .into_iter()
        .map(|(name, raw)| convert_server(&name, raw))
        .collect()
}

fn convert_server(name: &str, raw: RawServer) -> Result<McpServer> {
    match raw.transport.as_deref() {
        None | Some("stdio") => {
            let command = raw
                .command
                .with_context(|| format!("MCP server \"{name}\" is missing \"command\""))?;
            Ok(McpServer::Stdio(
                McpServerStdio::new(name, command)
                    .args(raw.args)
                    .env(to_env(raw.env)),
            ))
        }
        Some("http") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            Ok(McpServer::Http(
                McpServerHttp::new(name, url).headers(to_headers(raw.headers)),
            ))
        }
        Some("sse") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            Ok(McpServer::Sse(
                McpServerSse::new(name, url).headers(to_headers(raw.headers)),
            ))
        }
        Some(other) => bail!("MCP server \"{name}\" has unknown type \"{other}\""),
    }
}

fn to_env(env: BTreeMap<String, String>) -> Vec<EnvVariable> {
    env.into_iter()
        .map(|(name, value)| EnvVariable::new(name, value))
        .collect()
}

fn to_headers(headers: BTreeMap<String, String>) -> Vec<HttpHeader> {
    headers
        .into_iter()
        .map(|(name, value)| HttpHeader::new(name, value))
        .collect()
}

/// Drop servers the agent cannot accept: `stdio` is always supported, but
/// `http` / `sse` are only valid when the agent advertised the matching
/// capability in its `initialize` response. Forwarding an unadvertised remote
/// transport is a protocol violation, so drop (with a warning) rather than
/// send. Unknown future transports are dropped for the same reason.
pub fn filter_for_capabilities(
    servers: Vec<McpServer>,
    caps: &McpCapabilities,
    session: &str,
) -> Vec<McpServer> {
    servers
        .into_iter()
        .filter(|server| {
            let keep = match server {
                McpServer::Stdio(_) => true,
                McpServer::Http(_) => caps.http,
                McpServer::Sse(_) => caps.sse,
                _ => false,
            };
            if !keep {
                warn!(
                    target: "acp.mcp",
                    session = %session,
                    server = server_name(server),
                    transport = server_kind(server),
                    "dropping MCP server: agent does not advertise this transport"
                );
            }
            keep
        })
        .collect()
}

/// Names + transports of the configured servers for logging. Deliberately omits
/// env values and header values so secrets never reach the log sink.
pub fn summarize(servers: &[McpServer]) -> String {
    servers
        .iter()
        .map(|s| format!("{}({})", server_name(s), server_kind(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(s) => &s.name,
        McpServer::Http(s) => &s.name,
        McpServer::Sse(s) => &s.name,
        _ => "unknown",
    }
}

fn server_kind(server: &McpServer) -> &'static str {
    match server {
        McpServer::Stdio(_) => "stdio",
        McpServer::Http(_) => "http",
        McpServer::Sse(_) => "sse",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(servers: &[McpServer]) -> Vec<&str> {
        servers.iter().map(server_name).collect()
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_global_mcp_servers(dir.path()).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn loads_and_parses_app_dir_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "mcp-fs" } } }"#,
        )
        .unwrap();
        let servers = load_global_mcp_servers(dir.path()).unwrap();
        assert_eq!(names(&servers), vec!["fs"]);
    }

    #[test]
    fn malformed_app_dir_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
        assert!(load_global_mcp_servers(dir.path()).is_err());
    }

    #[test]
    fn empty_or_absent_mcp_servers_key_is_empty() {
        assert!(parse_mcp_servers("{}").unwrap().is_empty());
        assert!(parse_mcp_servers(r#"{"mcpServers":{}}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parses_stdio_entry() {
        let text = r#"{
            "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "secret" } }
            }
        }"#;
        let servers = parse_mcp_servers(text).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "fs");
                assert_eq!(s.command.to_string_lossy(), "mcp-fs");
                assert_eq!(s.args, vec!["--root".to_string(), ".".to_string()]);
                assert_eq!(s.env.len(), 1);
                assert_eq!(s.env[0].name, "TOKEN");
                assert_eq!(s.env[0].value, "secret");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn parses_remote_entries() {
        let text = r#"{
            "mcpServers": {
                "h": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                "s": { "type": "sse", "url": "https://e/sse" }
            }
        }"#;
        let servers = parse_mcp_servers(text).unwrap();
        // BTreeMap key ordering: "h" before "s".
        match &servers[0] {
            McpServer::Http(h) => {
                assert_eq!(h.name, "h");
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer x");
            }
            other => panic!("expected http, got {other:?}"),
        }
        match &servers[1] {
            McpServer::Sse(s) => assert_eq!(s.url, "https://e/sse"),
            other => panic!("expected sse, got {other:?}"),
        }
    }

    #[test]
    fn ordering_is_deterministic() {
        let text = r#"{ "mcpServers": {
            "zebra": { "command": "z" },
            "alpha": { "command": "a" },
            "mike":  { "command": "m" }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();
        assert_eq!(names(&servers), vec!["alpha", "mike", "zebra"]);
    }

    #[test]
    fn unknown_type_is_error() {
        let text = r#"{ "mcpServers": { "x": { "type": "carrier-pigeon", "url": "u" } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn stdio_without_command_is_error() {
        let text = r#"{ "mcpServers": { "x": { "args": ["--y"] } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn remote_without_url_is_error() {
        let text = r#"{ "mcpServers": { "x": { "type": "http" } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(parse_mcp_servers("{ not json").is_err());
    }

    #[test]
    fn capability_filter_keeps_stdio_drops_unadvertised_remotes() {
        let text = r#"{ "mcpServers": {
            "stdio":  { "command": "c" },
            "http":   { "type": "http", "url": "u" },
            "sse":    { "type": "sse",  "url": "u" }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();

        let none = McpCapabilities::new();
        let kept = filter_for_capabilities(servers.clone(), &none, "t");
        assert_eq!(names(&kept), vec!["stdio"]);

        let http_only = McpCapabilities::new().http(true);
        let kept = filter_for_capabilities(servers.clone(), &http_only, "t");
        assert_eq!(names(&kept), vec!["http", "stdio"]);

        let both = McpCapabilities::new().http(true).sse(true);
        let kept = filter_for_capabilities(servers, &both, "t");
        assert_eq!(names(&kept), vec!["http", "sse", "stdio"]);
    }

    #[test]
    fn summarize_omits_secret_values() {
        let text = r#"{ "mcpServers": {
            "fs": { "command": "c", "env": { "TOKEN": "supersecret" } }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();
        let s = summarize(&servers);
        assert_eq!(s, "fs(stdio)");
        assert!(!s.contains("supersecret"));
    }
}
