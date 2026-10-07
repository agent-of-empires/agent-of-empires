//! Standard `.mcp.json` parsing, redaction, and trust fingerprinting, free of ACP types.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PROJECT_MCP_FILE: &str = ".mcp.json";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StandardMcpFile {
    #[serde(default)]
    pub(super) mcp_servers: BTreeMap<String, StandardRawServer>,
}

/// Absent `type` (or `"stdio"`) selects stdio; `"http"` / `"sse"` select remote transports.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StandardRawServer {
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

/// Secret env/header values are kept for forwarding and fingerprinting; display
/// paths must go through the redacting helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMcpServer {
    pub name: String,
    pub transport: ProjectMcpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn kind(&self) -> &'static str {
        match self.transport {
            ProjectMcpTransport::Stdio { .. } => "stdio",
            ProjectMcpTransport::Http { .. } => "http",
            ProjectMcpTransport::Sse { .. } => "sse",
        }
    }

    /// One-line summary showing command/args or URL plus env/header names, never values.
    pub fn redacted_summary(&self) -> String {
        let (target, label, keys) = match &self.transport {
            ProjectMcpTransport::Stdio { command, args, env } => {
                let target = std::iter::once(command).chain(args).cloned();
                (target.collect::<Vec<_>>().join(" "), "env", env)
            }
            ProjectMcpTransport::Http { url, headers }
            | ProjectMcpTransport::Sse { url, headers } => (url.clone(), "headers", headers),
        };
        let mut s = format!("{} ({}): {target}", self.name, self.kind());
        if !keys.is_empty() {
            let names: Vec<_> = keys.keys().map(String::as_str).collect();
            s.push_str(&format!("  [{label}: {}]", names.join(", ")));
        }
        s
    }
}

/// Parses `<repo_path>/.mcp.json`. Unlike native agent configs, one bad entry
/// fails the whole file, since the file is under trust review.
pub fn load_project_mcp_servers(repo_path: &Path) -> Result<Vec<ProjectMcpServer>> {
    load_standard_mcp_servers(&repo_path.join(PROJECT_MCP_FILE))
}

/// A missing file yields no servers; a malformed one is an error.
pub(crate) fn load_standard_mcp_servers(path: &Path) -> Result<Vec<ProjectMcpServer>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading MCP config at {}", path.display()))
        }
    };
    parse_standard_mcp_servers(&text)
        .with_context(|| format!("parsing MCP config at {}", path.display()))
}

pub fn parse_standard_mcp_servers(text: &str) -> Result<Vec<ProjectMcpServer>> {
    let parsed: StandardMcpFile = serde_json::from_str(text)?;
    parsed
        .mcp_servers
        .into_iter()
        .map(|(name, raw)| convert_standard(name, raw))
        .collect()
}

pub(super) fn convert_standard(name: String, raw: StandardRawServer) -> Result<ProjectMcpServer> {
    let url = |url: Option<String>| {
        url.with_context(|| format!("MCP server \"{name}\" is missing \"url\""))
    };
    let transport = match raw.transport.as_deref() {
        None | Some("stdio") => ProjectMcpTransport::Stdio {
            command: raw
                .command
                .with_context(|| format!("MCP server \"{name}\" is missing \"command\""))?,
            args: raw.args,
            env: raw.env,
        },
        Some("http") => ProjectMcpTransport::Http {
            url: url(raw.url)?,
            headers: raw.headers,
        },
        Some("sse") => ProjectMcpTransport::Sse {
            url: url(raw.url)?,
            headers: raw.headers,
        },
        Some(other) => bail!("MCP server \"{name}\" has unknown type \"{other}\""),
    };
    Ok(ProjectMcpServer { name, transport })
}

/// SHA-256 over the name-sorted set, including secret values so a rotated token
/// re-prompts trust. Never log it.
pub fn fingerprint(servers: &[ProjectMcpServer]) -> String {
    fn pairs(tag: &[u8], map: &BTreeMap<String, String>, hasher: &mut Sha256) {
        for (k, v) in map {
            hasher.update(tag);
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
        }
    }
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
                pairs(b"\nenv:", env, &mut hasher);
            }
            ProjectMcpTransport::Http { url, headers }
            | ProjectMcpTransport::Sse { url, headers } => {
                hasher.update(b"\nurl:");
                hasher.update(url.as_bytes());
                pairs(b"\nheader:", headers, &mut hasher);
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

    #[test]
    fn load_and_parse() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_project_mcp_servers(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join(PROJECT_MCP_FILE), "{ not json").unwrap();
        assert!(load_project_mcp_servers(dir.path()).is_err());

        let servers = parse_standard_mcp_servers(
            r#"{ "mcpServers": {
            "zebra": { "command": "z" },
            "alpha": { "command": "a", "args": ["--x"], "env": { "TOKEN": "secret" } },
            "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } }
        } }"#,
        )
        .unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "remote", "zebra"]);

        for bad in [
            r#"{ "mcpServers": { "x": { "args": ["--y"] } } }"#,
            r#"{ "mcpServers": { "x": { "type": "http" } } }"#,
            r#"{ "mcpServers": { "x": { "type": "pigeon", "url": "u" } } }"#,
        ] {
            assert!(parse_standard_mcp_servers(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fingerprint_is_order_independent_and_covers_secret_values() {
        let fp = |env: &str| {
            fingerprint(
                &parse_standard_mcp_servers(&format!(
                    r#"{{ "mcpServers": {{ "fs": {{ "command": "c", "env": {env} }} }} }}"#
                ))
                .unwrap(),
            )
        };
        assert_eq!(
            fp(r#"{ "A": "1", "B": "2" }"#),
            fp(r#"{ "B": "2", "A": "1" }"#)
        );
        assert_ne!(fp(r#"{ "TOKEN": "old" }"#), fp(r#"{ "TOKEN": "new" }"#));
    }

    #[test]
    fn redacted_summary_hides_values_shows_names() {
        let servers = parse_standard_mcp_servers(
            r#"{ "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "supersecret" } },
                "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer hunter2" } }
            } }"#,
        )
        .unwrap();
        assert_eq!(
            servers[0].redacted_summary(),
            "fs (stdio): mcp-fs --root .  [env: TOKEN]"
        );
        assert_eq!(
            servers[1].redacted_summary(),
            "remote (http): https://e/mcp  [headers: Authorization]"
        );
    }
}
