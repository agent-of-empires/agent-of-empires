//! Plugin rewrite of the MCP servers forwarded to a session's agent.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

pub const METHOD: &str = "mcp.rewrite";

pub struct McpRewriteContext {
    pub session_id: String,
    pub agent: String,
    pub sandboxed: bool,
    pub profile: Option<String>,
    pub cwd: PathBuf,
}

/// The standard `mcpServers` entry shape plus its name, as sent to and received from plugins.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireServer {
    name: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

impl From<&ProjectMcpServer> for WireServer {
    fn from(server: &ProjectMcpServer) -> Self {
        let name = server.name.clone();
        let empty = |transport: &str| WireServer {
            name: name.clone(),
            transport: Some(transport.to_string()),
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
        };
        match &server.transport {
            ProjectMcpTransport::Stdio { command, args, env } => WireServer {
                command: Some(command.clone()),
                args: args.clone(),
                env: env.clone(),
                ..empty("stdio")
            },
            ProjectMcpTransport::Http { url, headers } => WireServer {
                url: Some(url.clone()),
                headers: headers.clone(),
                ..empty("http")
            },
            ProjectMcpTransport::Sse { url, headers } => WireServer {
                url: Some(url.clone()),
                headers: headers.clone(),
                ..empty("sse")
            },
        }
    }
}

impl WireServer {
    fn into_server(self) -> Result<ProjectMcpServer, String> {
        if self.name.is_empty() {
            return Err("added server has an empty name".into());
        }
        let name = self.name;
        let transport = match self.transport.as_deref() {
            None | Some("stdio") => {
                if self.url.is_some() || !self.headers.is_empty() {
                    return Err(format!("stdio server {name:?} must not set url or headers"));
                }
                let command = self
                    .command
                    .ok_or_else(|| format!("stdio server {name:?} needs a command"))?;
                ProjectMcpTransport::Stdio {
                    command,
                    args: self.args,
                    env: self.env,
                }
            }
            Some(kind @ ("http" | "sse")) => {
                if self.command.is_some() || !self.args.is_empty() || !self.env.is_empty() {
                    return Err(format!(
                        "{kind} server {name:?} must not set command, args or env"
                    ));
                }
                let url = self
                    .url
                    .ok_or_else(|| format!("{kind} server {name:?} needs a url"))?;
                if kind == "http" {
                    ProjectMcpTransport::Http {
                        url,
                        headers: self.headers,
                    }
                } else {
                    ProjectMcpTransport::Sse {
                        url,
                        headers: self.headers,
                    }
                }
            }
            Some(_) => return Err(format!("server {name:?} has an unknown type")),
        };
        Ok(ProjectMcpServer { name, transport })
    }
}

pub fn request_params(ctx: &McpRewriteContext, servers: &[ProjectMcpServer]) -> Value {
    json!({
        "session_id": ctx.session_id,
        "agent": ctx.agent,
        "sandboxed": ctx.sandboxed,
        "profile": ctx.profile,
        "cwd": ctx.cwd,
        "servers": servers.iter().map(WireServer::from).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DropSpec {
    #[default]
    None,
    All,
    Names(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEdit {
    pub drop: DropSpec,
    pub add: Vec<ProjectMcpServer>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawDrop {
    Star(String),
    Names(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEdit {
    #[serde(default)]
    drop: Option<RawDrop>,
    #[serde(default)]
    add: Vec<WireServer>,
}

/// Errors carry categories and server names only, never values from the response.
pub fn parse_edit(value: Value) -> Result<McpEdit, String> {
    let raw: RawEdit = serde_json::from_value(value).map_err(|e| {
        format!(
            "response does not match the mcp.rewrite schema ({:?})",
            e.classify()
        )
    })?;
    let drop = match raw.drop {
        None => DropSpec::None,
        Some(RawDrop::Names(names)) => DropSpec::Names(names),
        Some(RawDrop::Star(s)) if s == "*" => DropSpec::All,
        Some(RawDrop::Star(_)) => return Err("drop must be a list of names or \"*\"".into()),
    };
    let add = raw
        .add
        .into_iter()
        .map(WireServer::into_server)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpEdit { drop, add })
}

pub fn apply_edit(servers: Vec<ProjectMcpServer>, edit: McpEdit) -> Vec<ProjectMcpServer> {
    let mut out: Vec<ProjectMcpServer> = match edit.drop {
        DropSpec::All => Vec::new(),
        DropSpec::Names(dropped) => servers
            .into_iter()
            .filter(|s| !dropped.contains(&s.name))
            .collect(),
        DropSpec::None => servers,
    };
    for added in edit.add {
        out.retain(|s| s.name != added.name);
        out.push(added);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn stdio(name: &str, env: &[(&str, &str)]) -> ProjectMcpServer {
        ProjectMcpServer {
            name: name.into(),
            transport: ProjectMcpTransport::Stdio {
                command: "cmd".into(),
                args: vec!["--x".into()],
                env: map(env),
            },
        }
    }

    fn http(name: &str, url: &str, headers: &[(&str, &str)]) -> ProjectMcpServer {
        ProjectMcpServer {
            name: name.into(),
            transport: ProjectMcpTransport::Http {
                url: url.into(),
                headers: map(headers),
            },
        }
    }

    fn names(servers: &[ProjectMcpServer]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    fn ctx() -> McpRewriteContext {
        McpRewriteContext {
            session_id: "s1".into(),
            agent: "agent-x".into(),
            sandboxed: true,
            profile: Some("work".into()),
            cwd: "/w".into(),
        }
    }

    #[test]
    fn request_params_carry_full_definitions() {
        let servers = vec![
            stdio("ha", &[("TOKEN", "sekret")]),
            http("docs", "https://e/mcp", &[("Authorization", "Bearer t")]),
        ];
        let p = request_params(&ctx(), &servers);
        assert_eq!(p["session_id"], "s1");
        assert_eq!(p["agent"], "agent-x");
        assert_eq!(p["sandboxed"], true);
        assert_eq!(p["profile"], "work");
        assert_eq!(p["cwd"], "/w");
        assert_eq!(
            p["servers"][0],
            json!({"name":"ha","type":"stdio","command":"cmd","args":["--x"],"env":{"TOKEN":"sekret"}})
        );
        assert_eq!(
            p["servers"][1],
            json!({"name":"docs","type":"http","url":"https://e/mcp","headers":{"Authorization":"Bearer t"}})
        );
    }

    #[test]
    fn parse_edit_accepts_names_star_and_adds() {
        let e = parse_edit(json!({
            "drop": ["a", "b"],
            "add": [{"name":"x","type":"http","url":"http://g/mcp/x","headers":{"Authorization":"Bearer t"}}]
        }))
        .unwrap();
        assert_eq!(e.drop, DropSpec::Names(vec!["a".into(), "b".into()]));
        assert_eq!(
            e.add,
            vec![http(
                "x",
                "http://g/mcp/x",
                &[("Authorization", "Bearer t")]
            )]
        );

        let e = parse_edit(json!({"drop": "*"})).unwrap();
        assert_eq!(e.drop, DropSpec::All);
        assert!(e.add.is_empty());

        let e = parse_edit(json!({})).unwrap();
        assert_eq!(e.drop, DropSpec::None);
    }

    #[test]
    fn parse_edit_defaults_missing_type_to_stdio() {
        let e = parse_edit(json!({"add":[{"name":"s","command":"c"}]})).unwrap();
        assert_eq!(
            e.add,
            vec![ProjectMcpServer {
                name: "s".into(),
                transport: ProjectMcpTransport::Stdio {
                    command: "c".into(),
                    args: vec![],
                    env: BTreeMap::new()
                }
            }]
        );
    }

    #[test]
    fn parse_edit_rejects_malformed_responses() {
        let cases = [
            ("unknown top-level field", json!({"drop":[],"bogus":1})),
            ("drop wrong string", json!({"drop":"all"})),
            ("drop wrong type", json!({"drop": 3})),
            (
                "unknown type",
                json!({"add":[{"name":"x","type":"ws","url":"u"}]}),
            ),
            (
                "http without url",
                json!({"add":[{"name":"x","type":"http"}]}),
            ),
            (
                "stdio with url",
                json!({"add":[{"name":"x","command":"c","url":"u"}]}),
            ),
            ("stdio without command", json!({"add":[{"name":"x"}]})),
            (
                "http with command",
                json!({"add":[{"name":"x","type":"http","url":"u","command":"c"}]}),
            ),
            ("empty name", json!({"add":[{"name":"","command":"c"}]})),
            (
                "unknown server field",
                json!({"add":[{"name":"x","command":"c","bogus":1}]}),
            ),
            ("not an object", json!([1, 2])),
        ];
        for (label, v) in cases {
            assert!(parse_edit(v).is_err(), "{label} must be rejected");
        }
    }

    #[test]
    fn parse_edit_errors_never_echo_values() {
        let secret = "s3cr3t-token-value";
        let bad = [
            json!({"add":[{"name":"x","type":"http","url":secret,"headers":[secret]}]}),
            json!({"drop": secret}),
            json!({"add":[{"name":"x","command":"c","env":{"K":secret},"url":secret}]}),
        ];
        for v in bad {
            let err = parse_edit(v).unwrap_err();
            assert!(!err.contains(secret), "error leaked a value: {err}");
        }
    }

    #[test]
    fn apply_edit_drop_names_star_and_none() {
        let base = || vec![stdio("a", &[]), stdio("b", &[]), stdio("c", &[])];
        let out = apply_edit(
            base(),
            McpEdit {
                drop: DropSpec::Names(vec!["b".into(), "zz".into()]),
                add: vec![],
            },
        );
        assert_eq!(names(&out), ["a", "c"]);
        let out = apply_edit(
            base(),
            McpEdit {
                drop: DropSpec::All,
                add: vec![],
            },
        );
        assert!(out.is_empty());
        let out = apply_edit(
            base(),
            McpEdit {
                drop: DropSpec::None,
                add: vec![],
            },
        );
        assert_eq!(names(&out), ["a", "b", "c"]);
    }

    #[test]
    fn apply_edit_added_server_replaces_same_name_and_last_duplicate_wins() {
        let out = apply_edit(
            vec![stdio("a", &[("K", "old")]), stdio("keep", &[])],
            McpEdit {
                drop: DropSpec::None,
                add: vec![
                    http("a", "http://one", &[]),
                    http("n", "http://first", &[]),
                    http("n", "http://second", &[]),
                ],
            },
        );
        assert_eq!(names(&out), ["keep", "a", "n"]);
        assert_eq!(out[1], http("a", "http://one", &[]));
        assert_eq!(out[2], http("n", "http://second", &[]));
    }

    #[test]
    fn apply_edit_drop_all_then_add_keeps_only_added() {
        let out = apply_edit(
            vec![stdio("a", &[]), stdio("b", &[])],
            McpEdit {
                drop: DropSpec::All,
                add: vec![http("a", "http://g", &[])],
            },
        );
        assert_eq!(out, vec![http("a", "http://g", &[])]);
    }
}
