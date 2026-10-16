//! ACP boundary for MCP server forwarding.

use agent_client_protocol::schema::v1::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::warn;

use crate::session::mcp::mcp_model::{McpProvenance, ResolvedMcpServer};
use crate::session::mcp::project_mcp::ProjectMcpTransport;

/// Convert resolved, transport-typed servers (parsed and merged by
/// `session::mcp::mcp_model`) into ACP `McpServer` values for forwarding through
/// `session/new` and `session/load`.
pub fn project_servers_to_acp(
    servers: Vec<ResolvedMcpServer>,
    session_env: &[(String, String)],
) -> Vec<McpServer> {
    servers
        .into_iter()
        .map(|resolved| {
            let session_env = if matches!(&resolved.provenance, McpProvenance::ProjectLocal) {
                &[]
            } else {
                session_env
            };
            let server = resolved.def;
            match server.transport {
                ProjectMcpTransport::Stdio { command, args, env } => McpServer::Stdio(
                    McpServerStdio::new(server.name, command)
                        .args(args)
                        .env(to_env(env)),
                ),
                ProjectMcpTransport::Http { url, headers } => {
                    let headers = to_headers(&server.name, headers, session_env);
                    McpServer::Http(McpServerHttp::new(server.name, url).headers(headers))
                }
                ProjectMcpTransport::Sse { url, headers } => {
                    let headers = to_headers(&server.name, headers, session_env);
                    McpServer::Sse(McpServerSse::new(server.name, url).headers(headers))
                }
            }
        })
        .collect()
}

fn to_env(env: BTreeMap<String, String>) -> Vec<EnvVariable> {
    env.into_iter()
        .map(|(name, value)| EnvVariable::new(name, value))
        .collect()
}

fn to_headers(
    server: &str,
    headers: BTreeMap<String, String>,
    session_env: &[(String, String)],
) -> Vec<HttpHeader> {
    headers
        .into_iter()
        .filter_map(|(header_name, value)| {
            let (expanded, has_unresolved_reference) =
                expand_env_references(server, &header_name, &value, session_env);
            if has_unresolved_reference && header_name.eq_ignore_ascii_case("authorization") {
                return None;
            }
            let value = match expanded {
                Cow::Borrowed(_) => value,
                Cow::Owned(value) => value,
            };
            Some(HttpHeader::new(header_name, value))
        })
        .collect()
}

fn expand_env_references<'a>(
    server: &str,
    header: &str,
    value: &'a str,
    session_env: &[(String, String)],
) -> (Cow<'a, str>, bool) {
    let mut cursor = 0;
    let mut copied_until = 0;
    let mut expanded = None;
    let mut has_unresolved_reference = false;
    while let Some(relative_start) = value[cursor..].find("${") {
        let start = cursor + relative_start;
        let name_start = start + 2;
        let Some(relative_end) = value[name_start..].find('}') else {
            break;
        };
        let end = name_start + relative_end;
        let name = &value[name_start..end];
        cursor = end + 1;
        if !crate::session::environment::is_valid_env_key(name) {
            continue;
        }
        let replacement = session_env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
            .filter(|value| !value.is_empty());
        let Some(replacement) = replacement else {
            warn!(
                target: "acp.mcp",
                server,
                header,
                env_var = name,
                "MCP header environment reference could not be resolved or is not allowed for this config source"
            );
            has_unresolved_reference = true;
            continue;
        };
        let output = expanded.get_or_insert_with(String::new);
        output.push_str(&value[copied_until..start]);
        output.push_str(replacement);
        copied_until = end + 1;
    }

    let expanded = match expanded {
        Some(mut output) => {
            output.push_str(&value[copied_until..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(value),
    };
    (expanded, has_unresolved_reference)
}

/// Drop servers the agent cannot accept: `stdio` is always supported, but
/// `http` / `sse` are only valid when the agent advertised the matching
/// capability in its `initialize` response.
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

    use crate::session::mcp::mcp_model::{McpProvenance, ResolvedMcpServer};
    use crate::session::mcp::project_mcp::parse_standard_mcp_servers;

    fn names(servers: &[McpServer]) -> Vec<&str> {
        servers.iter().map(server_name).collect()
    }

    fn resolved(json: &str, provenance: McpProvenance) -> Vec<ResolvedMcpServer> {
        parse_standard_mcp_servers(json)
            .unwrap()
            .into_iter()
            .map(|def| ResolvedMcpServer {
                def,
                provenance: provenance.clone(),
                shadowed: Vec::new(),
            })
            .collect()
    }

    fn to_acp(json: &str) -> Vec<McpServer> {
        project_servers_to_acp(resolved(json, McpProvenance::Global), &[])
    }

    fn new_session_request(
        json: &str,
        provenance: McpProvenance,
        session_env: &[(String, String)],
    ) -> serde_json::Value {
        let servers = project_servers_to_acp(resolved(json, provenance), session_env);
        serde_json::to_value(
            agent_client_protocol::schema::v1::NewSessionRequest::new("/work").mcp_servers(servers),
        )
        .unwrap()
    }

    #[test]
    fn converts_every_transport_with_its_args_env_and_headers() {
        let servers = to_acp(
            r#"{ "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "secret" } },
                "h": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                "s": { "type": "sse", "url": "https://e/sse" }
            } }"#,
        );
        assert_eq!(names(&servers), vec!["fs", "h", "s"]);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.command.to_string_lossy(), "mcp-fs");
                assert_eq!(s.args, vec!["--root".to_string(), ".".to_string()]);
                assert_eq!(s.env[0].name, "TOKEN");
                assert_eq!(s.env[0].value, "secret");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &servers[1] {
            McpServer::Http(h) => {
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer x");
            }
            other => panic!("expected http, got {other:?}"),
        }
        assert!(matches!(&servers[2], McpServer::Sse(_)));

        // The capability filter keeps stdio and drops unadvertised remotes.
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

    #[test]
    fn serializes_resolved_auth_header_in_session_new_request() {
        let session_env = vec![
            ("GITHUB_TOKEN".to_string(), "older-token".to_string()),
            ("GITHUB_TOKEN".to_string(), "synthetic-token".to_string()),
        ];
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}}}"#,
            McpProvenance::Profile {
                name: "work".into(),
            },
            &session_env,
        );

        assert_eq!(
            request["mcpServers"][0]["headers"][0],
            serde_json::json!({
                "name": "Authorization",
                "value": "Bearer synthetic-token"
            })
        );
    }

    #[test]
    fn expands_reference_after_a_literal_dollar() {
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer $${GITHUB_TOKEN}"}}}}"#,
            McpProvenance::Profile {
                name: "work".into(),
            },
            &[("GITHUB_TOKEN".into(), "synthetic-token".into())],
        );

        assert_eq!(
            request["mcpServers"][0]["headers"][0]["value"],
            "Bearer $synthetic-token"
        );
    }

    #[test]
    fn unresolved_authorization_is_omitted_but_server_and_other_placeholders_remain() {
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}","X-Reference":"${MISSING}"}}}}"#,
            McpProvenance::Global,
            &[],
        );

        assert_eq!(request["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["mcpServers"][0]["headers"],
            serde_json::json!([{
                "name": "X-Reference",
                "value": "${MISSING}"
            }])
        );
    }

    #[test]
    fn unresolved_authorization_after_a_literal_dollar_is_omitted() {
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer $${GITHUB_TOKEN}","X-Reference":"$${MISSING}"}}}}"#,
            McpProvenance::Global,
            &[],
        );

        assert_eq!(request["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["mcpServers"][0]["headers"],
            serde_json::json!([{
                "name": "X-Reference",
                "value": "$${MISSING}"
            }])
        );
    }

    #[test]
    fn empty_authorization_reference_is_omitted_instead_of_sending_bearer_space() {
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}}}"#,
            McpProvenance::Profile {
                name: "work".into(),
            },
            &[
                ("GITHUB_TOKEN".into(), "older-token".into()),
                ("GITHUB_TOKEN".into(), String::new()),
            ],
        );

        assert_eq!(request["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(request["mcpServers"][0]["headers"], serde_json::json!([]));
        assert!(!request.to_string().contains("older-token"));
    }

    #[test]
    fn project_local_authorization_does_not_expand_from_session_environment() {
        let request = new_session_request(
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}}}"#,
            McpProvenance::ProjectLocal,
            &[("GITHUB_TOKEN".into(), "synthetic-token".into())],
        );

        assert_eq!(request["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(request["mcpServers"][0]["headers"], serde_json::json!([]));
        assert!(!request.to_string().contains("synthetic-token"));
    }
}
