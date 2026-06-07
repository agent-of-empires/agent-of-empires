//! Project-local `.mcp.json` parsing for the repo-trust gate (#1985).
//!
//! This lives outside the serve-gated `acp` module so the always-compiled trust
//! system (`repo_config`) and the TUI trust dialog can read, fingerprint, and
//! display a repo's project MCP servers without depending on the ACP schema
//! types. The supervisor converts these into ACP `McpServer` values for
//! forwarding (see `acp::mcp_config::project_servers_to_acp`), parsing the file
//! exactly once so the fingerprint that gates trust and the servers that are
//! forwarded can never diverge.
//!
//! The on-disk shape is the ecosystem-standard `.mcp.json` (`mcpServers` map),
//! the same shape AoE already reads for the global and per-profile layers.
//! Unlike those, a project-local file is repo-provided and therefore only
//! forwarded once the repo is trusted: a stdio server launches its `command`
//! the moment a session spawns, so an untrusted repo's `.mcp.json` is a
//! zero-click RCE surface gated exactly like lifecycle hooks.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Project-local MCP config filename, read from the repository root.
pub const PROJECT_MCP_FILE: &str = ".mcp.json";

/// On-disk shape of `<repo>/.mcp.json`. Unknown top-level keys are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectMcpFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawServer>,
}

/// A single raw server entry. Absent `type` (or `type: "stdio"`) selects the
/// stdio transport; `type: "http"` / `type: "sse"` select the remote transports.
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

/// One project MCP server, transport-resolved. `env` / `headers` are `BTreeMap`
/// so the fingerprint is order-independent. Values are kept in memory for the
/// fingerprint (a changed token is an effective config change), but the display
/// helpers redact them so secrets never reach a screen or log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMcpServer {
    pub name: String,
    pub transport: ProjectMcpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMcpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl ProjectMcpServer {
    /// Transport label for display and fingerprinting.
    pub fn kind(&self) -> &'static str {
        match self.transport {
            ProjectMcpTransport::Stdio { .. } => "stdio",
            ProjectMcpTransport::Http { .. } => "http",
            ProjectMcpTransport::Sse { .. } => "sse",
        }
    }

    /// One-line redacted summary for the trust dialog and CLI prompt: shows the
    /// command/args or URL plus env-var / header NAMES, never their values.
    pub fn redacted_summary(&self) -> String {
        match &self.transport {
            ProjectMcpTransport::Stdio { command, args, env } => {
                let mut s = format!("{} (stdio): {}", self.name, command);
                if !args.is_empty() {
                    s.push(' ');
                    s.push_str(&args.join(" "));
                }
                if !env.is_empty() {
                    s.push_str(&format!(
                        "  [env: {}]",
                        env.keys().cloned().collect::<Vec<_>>().join(", ")
                    ));
                }
                s
            }
            ProjectMcpTransport::Http { url, headers } => {
                redacted_remote(&self.name, "http", url, headers)
            }
            ProjectMcpTransport::Sse { url, headers } => {
                redacted_remote(&self.name, "sse", url, headers)
            }
        }
    }
}

fn redacted_remote(
    name: &str,
    kind: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> String {
    let mut s = format!("{} ({}): {}", name, kind, url);
    if !headers.is_empty() {
        s.push_str(&format!(
            "  [headers: {}]",
            headers.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    s
}

/// Read and parse `<repo_path>/.mcp.json` into transport-resolved servers,
/// sorted by name (the `BTreeMap` key order). A missing file yields an empty
/// list; a present-but-malformed file is an error the caller surfaces (the
/// trust dialog shows it; the supervisor warns and skips). Unlike the native
/// agent configs, a project file is small and fully under review, so a single
/// bad entry fails the whole parse rather than being silently dropped.
pub fn load_project_mcp_servers(repo_path: &Path) -> Result<Vec<ProjectMcpServer>> {
    let path = repo_path.join(PROJECT_MCP_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading project MCP config at {}", path.display()))
        }
    };
    parse_project_mcp_servers(&text)
        .with_context(|| format!("parsing project MCP config at {}", path.display()))
}

/// Parse `.mcp.json` text into transport-resolved servers. Split from the file
/// read so the conversion rules are unit-testable without touching disk.
fn parse_project_mcp_servers(text: &str) -> Result<Vec<ProjectMcpServer>> {
    let parsed: ProjectMcpFile = serde_json::from_str(text)?;
    parsed
        .mcp_servers
        .into_iter()
        .map(|(name, raw)| resolve_server(name, raw))
        .collect()
}

fn resolve_server(name: String, raw: RawServer) -> Result<ProjectMcpServer> {
    let transport = match raw.transport.as_deref() {
        None | Some("stdio") => {
            let command = raw
                .command
                .with_context(|| format!("MCP server \"{name}\" is missing \"command\""))?;
            ProjectMcpTransport::Stdio {
                command,
                args: raw.args,
                env: raw.env,
            }
        }
        Some("http") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            ProjectMcpTransport::Http {
                url,
                headers: raw.headers,
            }
        }
        Some("sse") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            ProjectMcpTransport::Sse {
                url,
                headers: raw.headers,
            }
        }
        Some(other) => bail!("MCP server \"{name}\" has unknown type \"{other}\""),
    };
    Ok(ProjectMcpServer { name, transport })
}

/// Deterministic SHA-256 fingerprint over the effective server set. Includes
/// env and header VALUES: a rotated token changes what the forwarded server
/// does, so it must re-prompt trust (the issue's "re-prompt when the effective
/// config changes"). The display helpers redact those same values, so the hash
/// input is more sensitive than anything shown; never log it. Servers are
/// already name-sorted and `env` / `headers` are `BTreeMap`, so the encoding is
/// stable across runs and JSON key orderings.
pub fn fingerprint(servers: &[ProjectMcpServer]) -> String {
    let mut hasher = Sha256::new();
    for server in servers {
        hasher.update(b"name:");
        hasher.update(server.name.as_bytes());
        hasher.update(b"\nkind:");
        hasher.update(server.kind().as_bytes());
        match &server.transport {
            ProjectMcpTransport::Stdio { command, args, env } => {
                hasher.update(b"\ncommand:");
                hasher.update(command.as_bytes());
                for arg in args {
                    hasher.update(b"\narg:");
                    hasher.update(arg.as_bytes());
                }
                for (k, v) in env {
                    hasher.update(b"\nenv:");
                    hasher.update(k.as_bytes());
                    hasher.update(b"=");
                    hasher.update(v.as_bytes());
                }
            }
            ProjectMcpTransport::Http { url, headers }
            | ProjectMcpTransport::Sse { url, headers } => {
                hasher.update(b"\nurl:");
                hasher.update(url.as_bytes());
                for (k, v) in headers {
                    hasher.update(b"\nheader:");
                    hasher.update(k.as_bytes());
                    hasher.update(b"=");
                    hasher.update(v.as_bytes());
                }
            }
        }
        hasher.update(b"\n;;\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(servers: &[ProjectMcpServer]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_project_mcp_servers(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn parses_and_sorts_by_name() {
        let text = r#"{ "mcpServers": {
            "zebra": { "command": "z" },
            "alpha": { "command": "a", "args": ["--x"], "env": { "TOKEN": "secret" } },
            "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } }
        } }"#;
        let servers = parse_project_mcp_servers(text).unwrap();
        assert_eq!(names(&servers), vec!["alpha", "remote", "zebra"]);
    }

    #[test]
    fn stdio_without_command_is_error() {
        assert!(
            parse_project_mcp_servers(r#"{ "mcpServers": { "x": { "args": ["--y"] } } }"#).is_err()
        );
    }

    #[test]
    fn remote_without_url_is_error() {
        assert!(
            parse_project_mcp_servers(r#"{ "mcpServers": { "x": { "type": "http" } } }"#).is_err()
        );
    }

    #[test]
    fn unknown_type_is_error() {
        assert!(parse_project_mcp_servers(
            r#"{ "mcpServers": { "x": { "type": "pigeon", "url": "u" } } }"#
        )
        .is_err());
    }

    #[test]
    fn malformed_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PROJECT_MCP_FILE), "{ not json").unwrap();
        assert!(load_project_mcp_servers(dir.path()).is_err());
    }

    #[test]
    fn fingerprint_is_deterministic_and_order_independent() {
        let a = parse_project_mcp_servers(
            r#"{ "mcpServers": { "fs": { "command": "c", "env": { "A": "1", "B": "2" } } } }"#,
        )
        .unwrap();
        let b = parse_project_mcp_servers(
            r#"{ "mcpServers": { "fs": { "command": "c", "env": { "B": "2", "A": "1" } } } }"#,
        )
        .unwrap();
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_changes_when_secret_value_changes() {
        let a = parse_project_mcp_servers(
            r#"{ "mcpServers": { "fs": { "command": "c", "env": { "TOKEN": "old" } } } }"#,
        )
        .unwrap();
        let b = parse_project_mcp_servers(
            r#"{ "mcpServers": { "fs": { "command": "c", "env": { "TOKEN": "new" } } } }"#,
        )
        .unwrap();
        assert_ne!(
            fingerprint(&a),
            fingerprint(&b),
            "rotating a secret value must re-prompt trust"
        );
    }

    #[test]
    fn redacted_summary_hides_values_shows_names() {
        let servers = parse_project_mcp_servers(
            r#"{ "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "supersecret" } },
                "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer hunter2" } }
            } }"#,
        )
        .unwrap();
        let stdio = servers[0].redacted_summary();
        assert!(stdio.contains("mcp-fs") && stdio.contains("--root") && stdio.contains("TOKEN"));
        assert!(!stdio.contains("supersecret"), "env value leaked: {stdio}");
        let http = servers[1].redacted_summary();
        assert!(http.contains("https://e/mcp") && http.contains("Authorization"));
        assert!(!http.contains("hunter2"), "header value leaked: {http}");
    }
}
