//! Writer for the AoE-owned global `<app_dir>/mcp.json`, used to promote servers above native configs.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::project_mcp::{convert_standard, ProjectMcpServer, ProjectMcpTransport};

/// Entry keys owned by the typed model; anything else in an entry is hand-added and kept on update.
const TRANSPORT_KEYS: [&str; 6] = ["command", "args", "env", "type", "url", "headers"];

/// The standard `mcpServers` entry for a server, omitting empty collections.
fn server_to_entry(server: &ProjectMcpServer) -> Value {
    let strings = |map: &std::collections::BTreeMap<String, String>| {
        Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    };
    let mut entry = Map::new();
    match &server.transport {
        ProjectMcpTransport::Stdio { command, args, env } => {
            entry.insert("command".into(), command.clone().into());
            if !args.is_empty() {
                entry.insert("args".into(), args.clone().into());
            }
            if !env.is_empty() {
                entry.insert("env".into(), strings(env));
            }
        }
        ProjectMcpTransport::Http { url, headers } | ProjectMcpTransport::Sse { url, headers } => {
            entry.insert("type".into(), server.kind().into());
            entry.insert("url".into(), url.clone().into());
            if !headers.is_empty() {
                entry.insert("headers".into(), strings(headers));
            }
        }
    }
    Value::Object(entry)
}

fn parse_root(content: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str::<Value>(content).context("parsing global mcp.json")? {
        Value::Object(m) => Ok(m),
        _ => anyhow::bail!("global mcp.json is not a JSON object"),
    }
}

/// Runs `mutate` on the `mcpServers` object under the file lock, creating it if absent.
/// An `Err` from `mutate` leaves the file untouched.
fn with_global_servers<R, E: From<anyhow::Error>>(
    mutate: impl FnOnce(&mut Map<String, Value>) -> std::result::Result<R, E>,
) -> std::result::Result<R, E> {
    let path = crate::session::get_app_dir()?.join("mcp.json");
    crate::session::storage::locked_update(
        &path,
        parse_root,
        |root| Ok(serde_json::to_string_pretty(root)?),
        |root: &mut Map<String, Value>| {
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .context("mcpServers in global mcp.json is not an object")?;
            mutate(servers)
        },
    )?
}

/// Inserts or replaces a server by name, preserving other servers and unknown
/// keys (the file is hand-edited, so it is not round-tripped through the typed model).
pub fn upsert_global_server(server: &ProjectMcpServer) -> Result<()> {
    let entry = server_to_entry(server);
    with_global_servers(|servers| {
        servers.insert(server.name.clone(), entry);
        Ok(())
    })
}

/// A secret-bearing key/value. `value: None` keeps the stored value on edit.
#[derive(Debug, Clone, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// Editable server definition; transport is the wire tag.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum McpServerInput {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<SecretEntry>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Vec<SecretEntry>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: Vec<SecretEntry>,
    },
}

#[derive(Debug)]
pub enum McpWriteError {
    /// The caller's input is wrong.
    Invalid(String),
    /// A global server with that name already exists.
    Exists(String),
    /// No such server in any layer.
    NotFound(String),
    /// The server exists but lives in a layer AoE does not own.
    ReadOnly(String),
    Io(anyhow::Error),
}

impl From<anyhow::Error> for McpWriteError {
    fn from(e: anyhow::Error) -> Self {
        Self::Io(e)
    }
}

fn invalid<T>(message: impl Into<String>) -> std::result::Result<T, McpWriteError> {
    Err(McpWriteError::Invalid(message.into()))
}

fn validate_name(name: &str) -> std::result::Result<(), McpWriteError> {
    let mut chars = name.chars();
    let ok = name.len() <= 128
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        invalid("server name must be 1-128 chars of letters, digits, '.', '_' or '-', starting with a letter or digit")
    }
}

fn check_text(label: &str, value: &str) -> std::result::Result<(), McpWriteError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return invalid(format!(
            "{label} must be non-empty text without control characters"
        ));
    }
    Ok(())
}

fn check_url(url: &str) -> std::result::Result<(), McpWriteError> {
    check_text("url", url)?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return invalid("url must start with http:// or https://");
    }
    Ok(())
}

/// Resolves entries to values; a valueless entry keeps its `stored` value.
fn resolve_secrets(
    label: &str,
    entries: &[SecretEntry],
    stored: Option<&BTreeMap<String, String>>,
) -> std::result::Result<BTreeMap<String, String>, McpWriteError> {
    let mut out = BTreeMap::new();
    for entry in entries {
        check_text(&format!("{label} name"), &entry.name)?;
        let value = match (&entry.value, stored.and_then(|s| s.get(&entry.name))) {
            (Some(v), _) => v.clone(),
            (None, Some(v)) => v.clone(),
            (None, None) => {
                return invalid(format!(
                    "no stored {label} value to keep for \"{}\"",
                    entry.name
                ))
            }
        };
        if out.insert(entry.name.clone(), value).is_some() {
            return invalid(format!("duplicate {label} \"{}\"", entry.name));
        }
    }
    Ok(out)
}

/// Builds the server from `input`; `existing` supplies values for kept keys, but only
/// within the same secret namespace (stdio env, or http/sse headers).
fn build_server(
    name: &str,
    input: &McpServerInput,
    existing: Option<&ProjectMcpServer>,
) -> std::result::Result<ProjectMcpServer, McpWriteError> {
    let stored_headers = || match existing.map(|e| &e.transport) {
        Some(
            ProjectMcpTransport::Http { headers, .. } | ProjectMcpTransport::Sse { headers, .. },
        ) => Some(headers),
        _ => None,
    };
    let transport = match input {
        McpServerInput::Stdio { command, args, env } => {
            check_text("command", command)?;
            if args.iter().any(|a| a.contains('\0')) {
                return invalid("args must not contain NUL");
            }
            let stored_env = match existing.map(|e| &e.transport) {
                Some(ProjectMcpTransport::Stdio { env, .. }) => Some(env),
                _ => None,
            };
            ProjectMcpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: resolve_secrets("env", env, stored_env)?,
            }
        }
        McpServerInput::Http { url, headers } => {
            check_url(url)?;
            ProjectMcpTransport::Http {
                url: url.clone(),
                headers: resolve_secrets("header", headers, stored_headers())?,
            }
        }
        McpServerInput::Sse { url, headers } => {
            check_url(url)?;
            ProjectMcpTransport::Sse {
                url: url.clone(),
                headers: resolve_secrets("header", headers, stored_headers())?,
            }
        }
    };
    Ok(ProjectMcpServer {
        name: name.to_string(),
        transport,
    })
}

/// The stored global entry, or `None` if absent or unparseable.
fn stored_server(servers: &Map<String, Value>, name: &str) -> Option<ProjectMcpServer> {
    let raw = serde_json::from_value(servers.get(name)?.clone()).ok()?;
    convert_standard(name.to_string(), raw).ok()
}

/// Adds a global server. The name may shadow a lower layer but not another global entry.
pub fn create_global_server(
    name: &str,
    input: &McpServerInput,
) -> std::result::Result<ProjectMcpServer, McpWriteError> {
    validate_name(name)?;
    with_global_servers(|servers| {
        if servers.contains_key(name) {
            return Err(McpWriteError::Exists(name.to_string()));
        }
        let server = build_server(name, input, None)?;
        servers.insert(name.to_string(), server_to_entry(&server));
        Ok(server)
    })
}

/// Replaces a global server's definition; valueless secret entries keep their stored value.
pub fn update_global_server(
    name: &str,
    input: &McpServerInput,
) -> std::result::Result<ProjectMcpServer, McpWriteError> {
    update_global_server_with(name, |_| Ok(input.clone()))
}

/// Like `update_global_server`, but `input` sees the stored server (`None` if unparseable)
/// under the file lock, so a read-modify-write cannot lose a concurrent edit.
pub fn update_global_server_with(
    name: &str,
    input: impl FnOnce(Option<&ProjectMcpServer>) -> std::result::Result<McpServerInput, McpWriteError>,
) -> std::result::Result<ProjectMcpServer, McpWriteError> {
    with_global_servers(|servers| {
        if !servers.contains_key(name) {
            return Err(missing(name));
        }
        let stored = stored_server(servers, name);
        let input = input(stored.as_ref())?;
        let server = build_server(name, &input, stored.as_ref())?;
        let Value::Object(fresh) = server_to_entry(&server) else {
            unreachable!("server_to_entry returns an object")
        };
        match servers.get_mut(name) {
            Some(Value::Object(existing)) => {
                for key in TRANSPORT_KEYS {
                    existing.remove(key);
                }
                existing.extend(fresh);
            }
            _ => {
                servers.insert(name.to_string(), Value::Object(fresh));
            }
        }
        Ok(server)
    })
}

/// Removes a global server, leaving everything else in the file as is.
pub fn delete_global_server(name: &str) -> std::result::Result<(), McpWriteError> {
    with_global_servers(|servers| servers.remove(name).map(drop).ok_or_else(|| missing(name)))
}

/// Marker for a name absent from the global file; `classify_missing` refines it.
fn missing(name: &str) -> McpWriteError {
    McpWriteError::NotFound(name.to_string())
}

/// Distinguishes a server owned by another layer (read-only) from a nonexistent one.
pub fn classify_missing(
    error: McpWriteError,
    agent: &str,
    profile: Option<&str>,
    cwd: &std::path::Path,
) -> McpWriteError {
    let McpWriteError::NotFound(name) = error else {
        return error;
    };
    let in_other_layer = super::mcp_model::resolve_surface(agent, profile, cwd)
        .effective
        .iter()
        .any(|s| s.def.name == name);
    if in_other_layer {
        McpWriteError::ReadOnly(format!(
            "\"{name}\" is not an AoE-owned server; edit it in its own config"
        ))
    } else {
        McpWriteError::NotFound(format!("no MCP server named \"{name}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::mcp::mcp_model::load_global_mcp_servers;

    fn stdio(name: &str, command: &str) -> ProjectMcpServer {
        ProjectMcpServer {
            name: name.into(),
            transport: ProjectMcpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: Default::default(),
            },
        }
    }

    #[test]
    #[serial_test::serial]
    fn upsert_replaces_by_name_and_preserves_other_content() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "someOtherKey": 7, "mcpServers": { "keepme": { "command": "k" } } }"#,
        )
        .unwrap();

        upsert_global_server(&stdio("fs", "first")).unwrap();
        upsert_global_server(&stdio("fs", "second")).unwrap();
        let remote = ProjectMcpServer {
            name: "remote".into(),
            transport: ProjectMcpTransport::Http {
                url: "https://e/mcp".into(),
                headers: [("Authorization".to_string(), "Bearer secret".to_string())].into(),
            },
        };
        upsert_global_server(&remote).unwrap();

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(app_dir.join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(raw["someOtherKey"], serde_json::json!(7));
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(
            servers,
            vec![stdio("fs", "second"), stdio("keepme", "k"), remote]
        );
    }

    fn secret(name: &str, value: Option<&str>) -> SecretEntry {
        SecretEntry {
            name: name.into(),
            value: value.map(Into::into),
        }
    }

    fn stdio_input(command: &str, env: Vec<SecretEntry>) -> McpServerInput {
        McpServerInput::Stdio {
            command: command.into(),
            args: vec![],
            env,
        }
    }

    fn global(app_dir: &std::path::Path) -> Vec<ProjectMcpServer> {
        load_global_mcp_servers(app_dir).unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn update_keeps_unknown_entry_keys_and_ignores_malformed_neighbours() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "mcpServers": {
                "fs": { "command": "k", "env": { "A": "1" }, "timeout": 30 },
                "broken": { "args": [] } } }"#,
        )
        .unwrap();

        let remote = McpServerInput::Http {
            url: "https://e/mcp".into(),
            headers: vec![],
        };
        update_global_server("fs", &remote).unwrap();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(app_dir.join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            raw["mcpServers"]["fs"],
            serde_json::json!({ "type": "http", "url": "https://e/mcp", "timeout": 30 })
        );
    }

    #[test]
    #[serial_test::serial]
    fn create_edit_delete_round_trip_preserves_other_content() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "someOtherKey": 7, "mcpServers": { "keepme": { "command": "k" } } }"#,
        )
        .unwrap();

        create_global_server(
            "fs",
            &stdio_input("first", vec![secret("TOKEN", Some("s1"))]),
        )
        .unwrap();
        assert!(matches!(
            create_global_server("fs", &stdio_input("dup", vec![])),
            Err(McpWriteError::Exists(_))
        ));

        // Keep TOKEN's stored value, drop nothing, add a new one.
        update_global_server(
            "fs",
            &stdio_input(
                "second",
                vec![secret("TOKEN", None), secret("NEW", Some("n"))],
            ),
        )
        .unwrap();
        let fs = global(&app_dir)
            .into_iter()
            .find(|s| s.name == "fs")
            .unwrap();
        let ProjectMcpTransport::Stdio { command, env, .. } = fs.transport else {
            panic!("stdio")
        };
        assert_eq!(command, "second");
        assert_eq!(env["TOKEN"], "s1");
        assert_eq!(env["NEW"], "n");

        // An omitted key is removed; an explicit empty string is a value.
        update_global_server("fs", &stdio_input("second", vec![secret("NEW", Some(""))])).unwrap();
        let ProjectMcpTransport::Stdio { env, .. } = global(&app_dir)
            .into_iter()
            .find(|s| s.name == "fs")
            .unwrap()
            .transport
        else {
            panic!("stdio")
        };
        assert_eq!(env.len(), 1);
        assert_eq!(env["NEW"], "");

        delete_global_server("fs").unwrap();
        assert!(matches!(
            delete_global_server("fs"),
            Err(McpWriteError::NotFound(_))
        ));
        assert!(matches!(
            update_global_server("fs", &stdio_input("x", vec![])),
            Err(McpWriteError::NotFound(_))
        ));

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(app_dir.join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(raw["someOtherKey"], serde_json::json!(7));
        assert_eq!(global(&app_dir), vec![stdio("keepme", "k")]);
    }

    #[test]
    #[serial_test::serial]
    fn kept_secrets_never_cross_a_transport_or_dangle() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        create_global_server("fs", &stdio_input("c", vec![secret("TOKEN", Some("s"))])).unwrap();
        create_global_server(
            "remote",
            &McpServerInput::Http {
                url: "https://e/mcp".into(),
                headers: vec![secret("Authorization", Some("Bearer x"))],
            },
        )
        .unwrap();
        let before = std::fs::read_to_string(app_dir.join("mcp.json")).unwrap();

        let http_keep = |name: &str| McpServerInput::Http {
            url: "https://e/mcp".into(),
            headers: vec![secret(name, None)],
        };
        let sse_keep = McpServerInput::Sse {
            url: "https://e/sse".into(),
            headers: vec![secret("Authorization", None)],
        };
        // stdio env names do not carry over to headers, and a missing key is not invented.
        for (name, input) in [
            ("fs", http_keep("TOKEN")),
            (
                "remote",
                stdio_input("c", vec![secret("Authorization", None)]),
            ),
            ("remote", http_keep("Missing")),
        ] {
            assert!(
                matches!(
                    update_global_server(name, &input),
                    Err(McpWriteError::Invalid(_))
                ),
                "{name}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(app_dir.join("mcp.json")).unwrap(),
            before
        );

        // http -> sse shares the header namespace, so the stored value carries over.
        update_global_server("remote", &sse_keep).unwrap();
        let remote = global(&app_dir)
            .into_iter()
            .find(|s| s.name == "remote")
            .unwrap();
        assert_eq!(
            remote.transport,
            ProjectMcpTransport::Sse {
                url: "https://e/sse".into(),
                headers: [("Authorization".to_string(), "Bearer x".to_string())].into(),
            }
        );
    }

    #[test]
    #[serial_test::serial]
    fn invalid_input_is_rejected_without_writing() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        let long = "a".repeat(129);
        for name in [
            "",
            " ",
            "a/b",
            "a b",
            ".hidden",
            "-x",
            "a%2Fb",
            "..",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    create_global_server(name, &stdio_input("c", vec![])),
                    Err(McpWriteError::Invalid(_))
                ),
                "name {name:?}"
            );
        }
        for input in [
            stdio_input("", vec![]),
            stdio_input("c\n", vec![]),
            stdio_input("c", vec![secret("A", Some("1")), secret("A", Some("2"))]),
            stdio_input("c", vec![secret("", Some("1"))]),
            stdio_input("c", vec![secret("NEW", None)]),
            McpServerInput::Http {
                url: "ftp://e".into(),
                headers: vec![],
            },
            McpServerInput::Sse {
                url: "".into(),
                headers: vec![],
            },
        ] {
            assert!(
                matches!(
                    create_global_server("ok", &input),
                    Err(McpWriteError::Invalid(_))
                ),
                "{input:?}"
            );
        }
        assert!(!app_dir.join("mcp.json").exists());
        create_global_server("a.b_c-1", &stdio_input("c", vec![])).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_creates_have_exactly_one_winner() {
        let _home = crate::session::test_support::isolate_app_dir();
        let wins = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| create_global_server("race", &stdio_input("c", vec![])).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|ok| *ok)
                .count()
        });
        assert_eq!(wins, 1);
    }
}
