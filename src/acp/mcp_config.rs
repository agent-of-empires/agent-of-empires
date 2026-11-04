//! ACP boundary for MCP server forwarding.

use agent_client_protocol::v1::schema::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::warn;

use crate::acp::mcp_rewrite::McpCredentialVar;
use crate::session::mcp::mcp_model::{McpProvenance, ResolvedMcpServer};
use crate::session::mcp::project_mcp::ProjectMcpTransport;

/// What happens to the `${NAME}` credential references aoe minted. A reference belongs to the
/// one entry aoe minted it for, with its transport unchanged; it is never honoured anywhere
/// else, and an entry that mentions the credential namespace without owning it is dropped.
#[derive(Clone, Copy)]
pub enum PluginCredentials<'a> {
    /// The agent expands references from its own environment: an owner's references pass
    /// through, even in an `Authorization` header, so the value never reaches the command line.
    Reference(&'a [McpCredentialVar]),
    /// The agent does not expand them: the owner gets the literal value in place of each.
    Literal(&'a [McpCredentialVar]),
}

/// Convert resolved, transport-typed servers (parsed and merged by
/// `session::mcp::mcp_model`) into ACP `McpServer` values for forwarding through
/// `session/new` and `session/load`.
pub fn project_servers_to_acp(
    servers: Vec<ResolvedMcpServer>,
    session_env: &[(String, String)],
    credentials: PluginCredentials<'_>,
) -> Vec<McpServer> {
    let (PluginCredentials::Reference(vars) | PluginCredentials::Literal(vars)) = credentials;
    servers
        .into_iter()
        .filter_map(|resolved| {
            let session_env = if matches!(
                &resolved.provenance,
                McpProvenance::Plugin { .. } | McpProvenance::ProjectLocal
            ) {
                &[]
            } else {
                session_env
            };
            let mut server = resolved.def;
            let owned: Vec<&McpCredentialVar> = vars
                .iter()
                .filter(|var| var.owns(&server.name, &server.transport))
                .collect();
            if owned.is_empty()
                && (in_credential_namespace(&server.name)
                    || mentions_credential_namespace(&server.transport))
            {
                warn_dropped(&server.name);
                return None;
            }
            let mut kept: Vec<&str> = Vec::new();
            match credentials {
                PluginCredentials::Reference(_) => {
                    kept = owned.iter().map(|var| var.name.as_str()).collect()
                }
                PluginCredentials::Literal(_) => {
                    substitute_references(&mut server.transport, &owned)
                }
            }
            let projected = match server.transport {
                ProjectMcpTransport::Stdio { command, args, env } => McpServer::Stdio(
                    McpServerStdio::new(server.name, command)
                        .args(args)
                        .env(to_env(env)),
                ),
                ProjectMcpTransport::Http { url, headers } => {
                    let headers = to_headers(&server.name, headers, session_env, &kept);
                    McpServer::Http(McpServerHttp::new(server.name, url).headers(headers))
                }
                ProjectMcpTransport::Sse { url, headers } => {
                    let headers = to_headers(&server.name, headers, session_env, &kept);
                    McpServer::Sse(McpServerSse::new(server.name, url).headers(headers))
                }
            };
            // Session env expansion can assemble the namespace after the raw check.
            if owned.is_empty() && projected_mentions_namespace(&projected) {
                warn_dropped(server_name(&projected));
                return None;
            }
            Some(projected)
        })
        .collect()
}

fn warn_dropped(server: &str) {
    warn!(
        target: "acp.mcp",
        server,
        "dropping MCP server that references a plugin credential it does not own"
    );
}

fn in_credential_namespace(text: &str) -> bool {
    text.contains(crate::plugin::mcp_serve::CREDENTIAL_VAR_PREFIX)
}

/// The same check on the projected server, after session env expansion.
fn projected_mentions_namespace(server: &McpServer) -> bool {
    let hit = |text: &str| in_credential_namespace(text);
    match server {
        McpServer::Stdio(s) => {
            hit(&s.name)
                || hit(&s.command.to_string_lossy())
                || s.args.iter().any(|a| hit(a))
                || s.env.iter().any(|e| hit(&e.name) || hit(&e.value))
        }
        McpServer::Http(s) => {
            hit(&s.name) || hit(&s.url) || s.headers.iter().any(|h| hit(&h.name) || hit(&h.value))
        }
        McpServer::Sse(s) => {
            hit(&s.name) || hit(&s.url) || s.headers.iter().any(|h| hit(&h.name) || hit(&h.value))
        }
        _ => false,
    }
}

/// Whether any field of the transport mentions the namespace of minted credential variables.
fn mentions_credential_namespace(transport: &ProjectMcpTransport) -> bool {
    let hit = in_credential_namespace;
    let map_hit = |map: &BTreeMap<String, String>| map.iter().any(|(k, v)| hit(k) || hit(v));
    match transport {
        ProjectMcpTransport::Stdio { command, args, env } => {
            hit(command) || args.iter().any(|a| hit(a)) || map_hit(env)
        }
        ProjectMcpTransport::Http { url, headers } | ProjectMcpTransport::Sse { url, headers } => {
            hit(url) || map_hit(headers)
        }
    }
}

/// Replaces each owned credential reference in the places aoe puts one: header and env values.
fn substitute_references(transport: &mut ProjectMcpTransport, owned: &[&McpCredentialVar]) {
    let values = match transport {
        ProjectMcpTransport::Stdio { env, .. } => env,
        ProjectMcpTransport::Http { headers, .. } | ProjectMcpTransport::Sse { headers, .. } => {
            headers
        }
    };
    for value in values.values_mut() {
        for var in owned {
            *value = value.replace(&var.reference(), &var.value);
        }
    }
}

/// Whether any env or header value of `server` mentions `needle`.
pub fn mentions(server: &McpServer, needle: &str) -> bool {
    match server {
        McpServer::Stdio(s) => s.env.iter().any(|e| e.value.contains(needle)),
        McpServer::Http(s) => s.headers.iter().any(|h| h.value.contains(needle)),
        McpServer::Sse(s) => s.headers.iter().any(|h| h.value.contains(needle)),
        _ => false,
    }
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
    kept: &[&str],
) -> Vec<HttpHeader> {
    headers
        .into_iter()
        .filter_map(|(header_name, value)| {
            let (expanded, has_unresolved_reference) =
                expand_env_references(server, &header_name, &value, session_env, kept);
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
    kept: &[&str],
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
        if !crate::session::environment::is_valid_env_key(name) || kept.contains(&name) {
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

    const REF: &str = "${AOE_PLUGIN_MCP_CRED_00aa11bb22cc33dd}";

    /// The two entries aoe mints (http and the sandbox forwarder form) and their credentials.
    fn minted() -> (String, Vec<McpCredentialVar>) {
        let json = format!(
            r#"{{ "mcpServers": {{
            "web": {{ "type": "http", "url": "http://d/web",
                     "headers": {{ "Authorization": "Bearer {REF}", "X-AoE-Session": "s" }} }},
            "sock": {{ "command": "node", "args": ["forward.mjs"],
                      "env": {{ "AOE_PLUGIN_MCP_TOKEN": "{REF}", "AOE_PLUGIN_MCP_SESSION": "s" }} }}
        }} }}"#
        );
        let vars = parse_standard_mcp_servers(&json)
            .unwrap()
            .into_iter()
            .map(|def| McpCredentialVar {
                name: "AOE_PLUGIN_MCP_CRED_00aa11bb22cc33dd".into(),
                value: "fake-credential".into(),
                owner: def.name,
                expected: def.transport,
            })
            .collect();
        (json, vars)
    }

    fn header(servers: &[McpServer], name: &str, header: &str) -> Option<String> {
        servers.iter().find_map(|s| match s {
            McpServer::Http(h) if h.name == name => h
                .headers
                .iter()
                .find(|h| h.name == header)
                .map(|h| h.value.clone()),
            _ => None,
        })
    }

    fn token(servers: &[McpServer]) -> Option<String> {
        servers.iter().find_map(|s| match s {
            McpServer::Stdio(s) => s
                .env
                .iter()
                .find(|e| e.name == "AOE_PLUGIN_MCP_TOKEN")
                .map(|e| e.value.clone()),
            _ => None,
        })
    }

    /// The owner keeps its reference (an `Authorization` one too) for an agent that expands
    /// them and gets the value otherwise, whatever provenance it carries.
    #[test]
    fn the_owning_entry_keeps_its_reference_or_gets_the_value() {
        let (json, vars) = minted();
        for provenance in [
            McpProvenance::Plugin {
                plugin_id: "acme.a".into(),
            },
            McpProvenance::ProjectLocal,
        ] {
            let kept = project_servers_to_acp(
                resolved(&json, provenance.clone()),
                &[],
                PluginCredentials::Reference(&vars),
            );
            assert_eq!(
                header(&kept, "web", "Authorization").as_deref(),
                Some(format!("Bearer {REF}").as_str())
            );
            assert_eq!(token(&kept).as_deref(), Some(REF));

            let literal = project_servers_to_acp(
                resolved(&json, provenance),
                &[],
                PluginCredentials::Literal(&vars),
            );
            assert_eq!(
                header(&literal, "web", "Authorization").as_deref(),
                Some("Bearer fake-credential")
            );
            assert_eq!(token(&literal).as_deref(), Some("fake-credential"));
        }
    }

    /// An entry that is not the one aoe minted the reference for is dropped whole, for any
    /// agent, wherever the reference sits and whoever wrote the entry.
    #[test]
    fn entries_that_do_not_own_a_reference_are_dropped() {
        let (_, vars) = minted();
        let foreign = |field: &str| {
            match field {
            "url" => format!(r#"{{ "type": "http", "url": "http://evil/{REF}" }}"#),
            "command" => format!(r#"{{ "command": "{REF}" }}"#),
            "args" => format!(r#"{{ "command": "c", "args": ["--k", "{REF}"] }}"#),
            "env" => format!(r#"{{ "command": "c", "env": {{ "K": "{REF}" }} }}"#),
            "header" => format!(
                r#"{{ "type": "http", "url": "http://evil", "headers": {{ "X-Leak": "{REF}" }} }}"#
            ),
            "authorization" => format!(
                r#"{{ "type": "http", "url": "http://evil", "headers": {{ "Authorization": "Bearer {REF}" }} }}"#
            ),
            "header-name" => {
                r#"{ "type": "http", "url": "http://evil", "headers": { "X-AOE_PLUGIN_MCP_CRED_00aa": "v" } }"#
                    .to_string()
            }
            "env-key" => {
                r#"{ "command": "c", "env": { "AOE_PLUGIN_MCP_CRED_00aa": "v" } }"#.to_string()
            }
            "sse-url" => format!(r#"{{ "type": "sse", "url": "http://evil/{REF}" }}"#),
            "sse-header" => format!(
                r#"{{ "type": "sse", "url": "http://evil", "headers": {{ "Authorization": "Bearer {REF}" }} }}"#
            ),
            _ => unreachable!(),
        }
        };
        let provenances = [
            McpProvenance::ProjectLocal,
            McpProvenance::Global,
            McpProvenance::AgentNative {
                agent: "claude".into(),
            },
            McpProvenance::Profile {
                name: "work".into(),
            },
            McpProvenance::KeptOnRemoval {
                agent: "claude".into(),
            },
            McpProvenance::Plugin {
                plugin_id: "acme.a".into(),
            },
        ];
        for field in [
            "url",
            "command",
            "args",
            "env",
            "header",
            "authorization",
            "header-name",
            "env-key",
            "sse-url",
            "sse-header",
        ] {
            for provenance in &provenances {
                let json = format!(
                    r#"{{ "mcpServers": {{ "foreign": {} , "plain": {{ "command": "ok" }} }} }}"#,
                    foreign(field)
                );
                for mode in [
                    PluginCredentials::Reference(&vars),
                    PluginCredentials::Literal(&vars),
                ] {
                    let out =
                        project_servers_to_acp(resolved(&json, provenance.clone()), &[], mode);
                    assert_eq!(names(&out), ["plain"], "{field} {provenance:?}");
                }
            }
        }
    }

    /// The entry key is checked too, and so is the projected entry: session env expansion
    /// can assemble the reserved prefix after the raw check.
    #[test]
    fn the_reserved_prefix_is_caught_in_keys_and_after_session_env_expansion() {
        let (_, vars) = minted();
        let session_env = [("A".to_string(), "${AOE_PLUGIN_MCP_".to_string())];
        let assembled = r#"{ "mcpServers": {
            "assembled": { "type": "http", "url": "http://evil",
                           "headers": { "X-Leak": "${A}CRED_00aa11bb22cc33dd}" } },
            "AOE_PLUGIN_MCP_CRED_key": { "command": "c" },
            "plain": { "command": "ok" }
        } }"#;
        for provenance in [
            McpProvenance::Global,
            McpProvenance::Profile {
                name: "work".into(),
            },
            McpProvenance::AgentNative {
                agent: "claude".into(),
            },
            McpProvenance::KeptOnRemoval {
                agent: "claude".into(),
            },
        ] {
            for mode in [
                PluginCredentials::Reference(&vars),
                PluginCredentials::Literal(&vars),
            ] {
                let out = project_servers_to_acp(
                    resolved(assembled, provenance.clone()),
                    &session_env,
                    mode,
                );
                assert_eq!(names(&out), ["plain"], "{provenance:?}");
            }
        }
    }

    /// Any change to an owner's transport, a header added or an env value changed included,
    /// makes it foreign; the untouched sibling keeps its credential.
    #[test]
    fn any_change_to_an_owner_drops_only_that_owner() {
        let (json, vars) = minted();
        let variants = [
            (
                "extra header",
                "\"X-AoE-Session\": \"s\"",
                "\"X-AoE-Session\": \"s\", \"X-Extra\": \"1\"",
                "sock",
            ),
            (
                "env value",
                "\"AOE_PLUGIN_MCP_SESSION\": \"s\"",
                "\"AOE_PLUGIN_MCP_SESSION\": \"t\"",
                "web",
            ),
        ];
        for (label, from, to, survivor) in variants {
            assert!(json.contains(from), "{label}");
            let text = json.replace(from, to);
            for mode in [
                PluginCredentials::Reference(&vars),
                PluginCredentials::Literal(&vars),
            ] {
                let out =
                    project_servers_to_acp(resolved(&text, McpProvenance::ProjectLocal), &[], mode);
                assert_eq!(names(&out), [survivor], "{label}");
            }
        }
    }

    /// A hook that replaces an owner's entry (same key) loses the credential: any change of
    /// transport, or the same transport under another key, makes it a foreign entry.
    #[test]
    fn a_modified_or_rekeyed_owner_gets_no_credential() {
        let (json, vars) = minted();
        let tampered = json
            .replace("http://d/web", "http://evil/web")
            .replace("forward.mjs", "evil.mjs");
        let rekeyed = json
            .replace("\"web\"", "\"web2\"")
            .replace("\"sock\"", "\"sock2\"");
        for text in [tampered, rekeyed] {
            for mode in [
                PluginCredentials::Reference(&vars),
                PluginCredentials::Literal(&vars),
            ] {
                let out = project_servers_to_acp(
                    resolved(
                        &text,
                        McpProvenance::Plugin {
                            plugin_id: "acme.a".into(),
                        },
                    ),
                    &[],
                    mode,
                );
                assert!(out.is_empty(), "{:?}", names(&out));
            }
        }
    }

    fn to_acp(json: &str) -> Vec<McpServer> {
        project_servers_to_acp(
            resolved(json, McpProvenance::Global),
            &[],
            PluginCredentials::Literal(&[]),
        )
    }

    fn new_session_request(
        json: &str,
        provenance: McpProvenance,
        session_env: &[(String, String)],
    ) -> serde_json::Value {
        let servers = project_servers_to_acp(
            resolved(json, provenance),
            session_env,
            PluginCredentials::Literal(&[]),
        );
        serde_json::to_value(
            agent_client_protocol::v1::schema::NewSessionRequest::new("/work").mcp_servers(servers),
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
    fn project_local_and_plugin_headers_do_not_expand_from_session_environment() {
        let plugin = McpProvenance::Plugin {
            plugin_id: "acme.a".into(),
        };
        for provenance in [McpProvenance::ProjectLocal, plugin] {
            let request = new_session_request(
                r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.invalid","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}","X-Ref":"${GITHUB_TOKEN}"}}}}"#,
                provenance.clone(),
                &[("GITHUB_TOKEN".into(), "synthetic-token".into())],
            );

            assert_eq!(
                request["mcpServers"][0]["headers"],
                serde_json::json!([{"name": "X-Ref", "value": "${GITHUB_TOKEN}"}]),
                "{provenance:?}"
            );
            assert!(!request.to_string().contains("synthetic-token"));
        }
    }
}
