//! ACP boundary for MCP server forwarding.
//!
//! Parsing, layering, provenance, and the precedence merge all live in the
//! always-compiled `session::mcp_model` resolver, which the unified management
//! surface (#1996), the CLI, and the TUI also read. This serve-gated module is
//! the thin edge that converts the resolver's winning set into ACP `McpServer`
//! wire values and drops any transport the agent did not advertise. Sharing one
//! resolver across forwarding and display guarantees what the user sees equals
//! what the agent receives.

use agent_client_protocol::schema::v1::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use std::collections::BTreeMap;
use tracing::warn;

use crate::session::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

/// Convert resolved, transport-typed servers (parsed and merged by
/// `session::mcp_model`) into ACP `McpServer` values for forwarding through
/// `session/new` and `session/load`. The caller passes the winning set of the
/// precedence merge, so the converted list is exactly what reaches the agent.
pub fn project_servers_to_acp(servers: Vec<ProjectMcpServer>) -> Vec<McpServer> {
    servers
        .into_iter()
        .map(|server| match server.transport {
            ProjectMcpTransport::Stdio { command, args, env } => McpServer::Stdio(
                McpServerStdio::new(server.name, command)
                    .args(args)
                    .env(to_env(env)),
            ),
            ProjectMcpTransport::Http { url, headers } => {
                McpServer::Http(McpServerHttp::new(server.name, url).headers(to_headers(headers)))
            }
            ProjectMcpTransport::Sse { url, headers } => {
                McpServer::Sse(McpServerSse::new(server.name, url).headers(to_headers(headers)))
            }
        })
        .collect()
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
    use crate::session::project_mcp::parse_standard_mcp_servers;

    fn names(servers: &[McpServer]) -> Vec<&str> {
        servers.iter().map(server_name).collect()
    }

    fn to_acp(json: &str) -> Vec<McpServer> {
        project_servers_to_acp(parse_standard_mcp_servers(json).unwrap())
    }

    #[test]
    fn converts_stdio_with_args_and_env() {
        let servers = to_acp(
            r#"{ "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "secret" } }
            } }"#,
        );
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "fs");
                assert_eq!(s.command.to_string_lossy(), "mcp-fs");
                assert_eq!(s.args, vec!["--root".to_string(), ".".to_string()]);
                assert_eq!(s.env[0].name, "TOKEN");
                assert_eq!(s.env[0].value, "secret");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn converts_remote_transports() {
        let servers = to_acp(
            r#"{ "mcpServers": {
                "h": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                "s": { "type": "sse", "url": "https://e/sse" }
            } }"#,
        );
        match &servers[0] {
            McpServer::Http(h) => {
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer x");
            }
            other => panic!("expected http, got {other:?}"),
        }
        assert!(matches!(&servers[1], McpServer::Sse(_)));
    }

    #[test]
    fn capability_filter_keeps_stdio_drops_unadvertised_remotes() {
        let servers = to_acp(
            r#"{ "mcpServers": {
                "stdio":  { "command": "c" },
                "http":   { "type": "http", "url": "u" },
                "sse":    { "type": "sse",  "url": "u" }
            } }"#,
        );

        let none = McpCapabilities::new();
        assert_eq!(
            names(&filter_for_capabilities(servers.clone(), &none, "t")),
            vec!["stdio"]
        );

        let http_only = McpCapabilities::new().http(true);
        assert_eq!(
            names(&filter_for_capabilities(servers.clone(), &http_only, "t")),
            vec!["http", "stdio"]
        );

        let both = McpCapabilities::new().http(true).sse(true);
        assert_eq!(
            names(&filter_for_capabilities(servers, &both, "t")),
            vec!["http", "sse", "stdio"]
        );
    }
}
