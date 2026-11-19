//! ACP boundary for MCP server forwarding.

use agent_client_protocol::v1::schema::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::warn;

use crate::session::mcp::mcp_model::{McpProvenance, ResolvedMcpServer};
use crate::session::mcp::project_mcp::ProjectMcpTransport;

/// What identifies the daemon to a sandboxed session's MCP set.
pub struct DaemonIdentity {
    /// Tokens the daemon accepts, grace token included. `None` when this process has no token
    /// authority; remote servers are then dropped, since none can be proven token-free.
    tokens: Option<Vec<String>>,
    /// Canonical host names the daemon is reachable on, from `serve.url`. Best effort: only the
    /// token check is authoritative.
    hosts: Vec<String>,
}

impl DaemonIdentity {
    pub fn new(tokens: Option<Vec<String>>, hosts: impl IntoIterator<Item = String>) -> Self {
        let hosts = hosts.into_iter().map(|h| canonical_host(&h)).collect();
        Self { tokens, hosts }
    }

    pub async fn current() -> Self {
        let hosts = crate::cli::serve::read_serve_urls()
            .iter()
            .filter_map(|u| reqwest::Url::parse(&u.url).ok())
            .filter_map(|u| u.host_str().map(str::to_owned))
            .collect::<Vec<_>>();
        Self::new(crate::server::token::accepted_daemon_tokens().await, hosts)
    }
}

/// One comparable spelling per host: no brackets, no root dot, lowercase, IPv4-mapped IPv6
/// folded to IPv4.
fn canonical_host(host: &str) -> String {
    let host = host.trim_matches(['[', ']']);
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_canonical().to_string(),
        Err(_) => host,
    }
}

const MAX_DECODE_ROUNDS: usize = 32;

/// True if `value`, or any percent-decoding of it, contains a token, so a token spelled with
/// escapes in the userinfo, path, query or fragment is still found. Fails closed: a value still
/// changing after `MAX_DECODE_ROUNDS` counts as a hit.
fn mentions_token(value: &str, tokens: &[String]) -> bool {
    let mut current = value.to_owned();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if tokens.iter().any(|token| current.contains(token.as_str())) {
            return true;
        }
        let next = percent_encoding::percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if next == current {
            return false;
        }
        current = next;
    }
    true
}

fn is_loopback_host(canonical: &str) -> bool {
    canonical == "localhost"
        || canonical.ends_with(".localhost")
        || canonical
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Drop servers a sandboxed session must not receive: remote servers on a loopback host or on
/// the daemon's own listener, and any remote server carrying an accepted daemon token.
pub fn drop_daemon_servers(
    servers: Vec<ResolvedMcpServer>,
    daemon: &DaemonIdentity,
) -> Vec<ResolvedMcpServer> {
    servers
        .into_iter()
        .filter(|resolved| {
            let (url, headers) = match &resolved.def.transport {
                ProjectMcpTransport::Http { url, headers }
                | ProjectMcpTransport::Sse { url, headers } => (url, headers),
                ProjectMcpTransport::Stdio { .. } => return true,
            };
            let credential_risk = daemon.tokens.as_ref().is_none_or(|tokens| {
                std::iter::once(url)
                    .chain(headers.values())
                    .any(|value| mentions_token(value, tokens))
            });
            let host_is_daemon = reqwest::Url::parse(url).is_ok_and(|url| {
                url.host_str().is_some_and(|host| {
                    let host = canonical_host(host);
                    is_loopback_host(&host) || daemon.hosts.contains(&host)
                })
            });
            let drop = host_is_daemon || credential_risk;
            if drop {
                warn!(
                    target: "acp.mcp",
                    server = %resolved.def.name,
                    "not forwarding a daemon or loopback MCP server into a sandboxed session"
                );
            }
            !drop
        })
        .collect()
}

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
            agent_client_protocol::v1::schema::NewSessionRequest::new("/work").mcp_servers(servers),
        )
        .unwrap()
    }

    #[test]
    fn sandboxed_sessions_never_get_daemon_or_loopback_servers() {
        let json = r#"{ "mcpServers": {
            "aoe": { "type": "http", "url": "http://127.0.0.1:9000/mcp",
                     "headers": { "Authorization": "Bearer sekrit" } },
            "local": { "type": "sse", "url": "http://localhost:9000/sse" },
            "mapped": { "type": "http", "url": "http://[::ffff:127.0.0.1]:9000/mcp" },
            "v6": { "type": "http", "url": "http://[::1]:9000/mcp" },
            "lan": { "type": "http", "url": "http://192.168.1.5:9000/mcp" },
            "unparseable": { "type": "http", "url": "u",
                             "headers": { "Authorization": "Bearer sekrit" } },
            "urltoken": { "type": "http", "url": "https://proxy.example.test/mcp?token=sekrit" },
            "previous": { "type": "http", "url": "https://alias.example.test/mcp",
                          "headers": { "Authorization": "Bearer prev-token" } },
            "renamed": { "type": "http", "url": "https://proxy.example.test/mcp",
                         "headers": { "X-Key": "Bearer sekrit" } },
            "github": { "type": "http", "url": "https://api.github.test/mcp",
                        "headers": { "Authorization": "Bearer pat" } },
            "fs": { "command": "fs-server", "env": { "K": "v" } }
        } }"#;
        let names = |daemon: &DaemonIdentity| {
            let kept = drop_daemon_servers(resolved(json, McpProvenance::Global), daemon);
            let mut kept: Vec<_> = kept.iter().map(|s| s.def.name.clone()).collect();
            kept.sort();
            kept
        };
        let tokens = Some(vec!["sekrit".to_string(), "prev-token".to_string()]);
        let daemon = DaemonIdentity::new(tokens.clone(), ["192.168.1.5".to_string()]);
        assert_eq!(names(&daemon), ["fs", "github"]);

        // An unreadable or half-written `serve.url` leaves no hosts; tokens still decide.
        let no_hosts = DaemonIdentity::new(tokens, []);
        assert_eq!(names(&no_hosts), ["fs", "github", "lan"]);

        // Without a token authority no remote server can be proven token-free.
        let no_authority = DaemonIdentity::new(None, []);
        assert_eq!(names(&no_authority), ["fs"]);
    }

    #[test]
    fn escaped_tokens_in_any_url_component_are_still_found() {
        let daemon = DaemonIdentity::new(Some(vec!["abcdef0123".into()]), []);
        for (url, header, dropped) in [
            ("https://p.example.test/%61bcdef0123", "", true),
            ("https://p.example.test/mcp?k=%61bcdef0123", "", true),
            ("https://u:%61bcdef0123@p.example.test/mcp", "", true),
            ("https://p.example.test/mcp#%61bcdef0123", "", true),
            ("https://p.example.test/mcp?k=%2561bcdef0123", "", true),
            (
                "https://p.example.test/mcp?k=%2525252561bcdef0123",
                "",
                true,
            ),
            (
                &format!("https://p.example.test/mcp?k=%{}41", "25".repeat(40)),
                "",
                true,
            ),
            (
                &format!("https://p.example.test/mcp?k=%{}41", "25".repeat(50_000)),
                "",
                true,
            ),
            ("https://p.example.test/mcp", "Bearer %61bcdef0123", true),
            (
                "https://p.example.test/mcp?k=%61bcdef9999",
                "Bearer other",
                false,
            ),
        ] {
            let json = format!(
                r#"{{ "mcpServers": {{ "x": {{ "type": "http", "url": "{url}",
                    "headers": {{ "Authorization": "{header}" }} }} }} }}"#
            );
            let kept = drop_daemon_servers(resolved(&json, McpProvenance::Global), &daemon);
            assert_eq!(kept.is_empty(), dropped, "{url} {header}");
        }
    }

    #[test]
    fn equivalent_host_spellings_make_the_same_forwarding_decision() {
        let daemon = DaemonIdentity::new(
            Some(vec!["sekrit".into()]),
            [
                "Host.Example.Test.".to_string(),
                "::ffff:192.168.1.5".to_string(),
            ],
        );
        for (url, dropped) in [
            ("http://LOCALHOST:9000/mcp", true),
            ("http://localhost.:9000/mcp", true),
            ("http://foo.localhost:9000/mcp", true),
            ("http://[::ffff:7f00:1]:9000/mcp", true),
            ("http://127.1:9000/mcp", true),
            ("http://host.example.test:9000/mcp", true),
            ("http://HOST.example.test.:9000/mcp", true),
            ("http://192.168.1.5:9000/mcp", true),
            ("http://[::ffff:192.168.1.5]:9000/mcp", true),
            ("http://other.example.test:9000/mcp", false),
        ] {
            let json =
                format!(r#"{{ "mcpServers": {{ "x": {{ "type": "http", "url": "{url}" }} }} }}"#);
            let kept = drop_daemon_servers(resolved(&json, McpProvenance::Global), &daemon);
            assert_eq!(kept.is_empty(), dropped, "{url}");
        }
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
