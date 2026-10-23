//! Writer for the AoE-owned global `<app_dir>/mcp.json` (#1996).
//!
//! The unified MCP surface resolves conflicts (feature C) and keep-on-removal
//! (feature D) by writing the server's definition into the global `mcp.json`,
//! never into an agent-native config. Because the global layer outranks
//! agent-native in the precedence stack, a server promoted here shadows the
//! native one and resolves with provenance `global`, so no bespoke "aoe-added"
//! layer is needed.
//!
//! The file is the ecosystem-standard `{ "mcpServers": { ... } }` shape that
//! users also hand-edit, so mutations go through a `serde_json::Value` that
//! preserves every other server and any unknown keys, rather than round-tripping
//! through the typed model (which would drop fields AoE does not model). Writes
//! serialize through an exclusive file lock so a concurrent surface write cannot
//! clobber another.

use std::path::PathBuf;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::project_mcp::{convert_standard, ProjectMcpServer, ProjectMcpTransport};

/// Entry keys owned by the typed model; anything else in an entry is hand-added and kept on update.
const TRANSPORT_KEYS: [&str; 6] = ["command", "args", "env", "type", "url", "headers"];

/// Path to the global `mcp.json` AoE owns and may write.
fn global_mcp_path() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("mcp.json"))
}

/// The standard `.mcp.json` entry object for a server: stdio carries
/// command/args/env; remote transports carry a `type` plus url/headers. Empty
/// maps and arg lists are omitted so the file stays clean.
fn server_to_entry(server: &ProjectMcpServer) -> Value {
    let mut entry = Map::new();
    match &server.transport {
        ProjectMcpTransport::Stdio { command, args, env } => {
            entry.insert("command".into(), Value::String(command.clone()));
            if !args.is_empty() {
                entry.insert(
                    "args".into(),
                    Value::Array(args.iter().cloned().map(Value::String).collect()),
                );
            }
            if !env.is_empty() {
                entry.insert("env".into(), to_object(env));
            }
        }
        ProjectMcpTransport::Http { url, headers } => {
            entry.insert("type".into(), Value::String("http".into()));
            entry.insert("url".into(), Value::String(url.clone()));
            if !headers.is_empty() {
                entry.insert("headers".into(), to_object(headers));
            }
        }
        ProjectMcpTransport::Sse { url, headers } => {
            entry.insert("type".into(), Value::String("sse".into()));
            entry.insert("url".into(), Value::String(url.clone()));
            if !headers.is_empty() {
                entry.insert("headers".into(), to_object(headers));
            }
        }
    }
    Value::Object(entry)
}

fn to_object(map: &std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

/// Read the current global `mcp.json` as a JSON object, treating a missing or
/// empty file as `{}`.
fn read_root(content: &str) -> Result<Map<String, Value>> {
    if content.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(content).context("parsing global mcp.json")? {
        Value::Object(m) => Ok(m),
        _ => anyhow::bail!("global mcp.json is not a JSON object"),
    }
}

/// Apply a mutation to the `mcpServers` object of the global `mcp.json` under an
/// exclusive lock, preserving every other server and any unknown top-level keys.
/// The closure's return value is passed back to the caller, so an existence
/// check and the mutation it gates run inside the same lock (no check-then-act
/// race against a concurrent surface write).
fn mutate_servers<T>(mutate: impl FnOnce(&mut Map<String, Value>) -> T) -> Result<T> {
    let path = global_mcp_path()?;
    crate::session::storage::LockedDataFile::open(&path)?
        .update(
            read_root,
            |root| Ok(serde_json::to_string_pretty(root)?),
            |root| -> Result<T> {
                let mut servers = take_servers(root)?;
                let out = mutate(&mut servers);
                root.insert("mcpServers".into(), Value::Object(servers));
                Ok(out)
            },
        )?
        .map(|(result, _)| result)
}

/// Take the `mcpServers` object out of the parsed root, defaulting a missing key
/// to an empty map and rejecting a non-object value.
fn take_servers(root: &mut Map<String, Value>) -> Result<Map<String, Value>> {
    match root.remove("mcpServers") {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => anyhow::bail!("mcpServers in global mcp.json is not an object"),
        None => Ok(Map::new()),
    }
}

/// Insert or replace a server in the global `mcp.json` by name. Used by both
/// keep-on-removal ("keep") and conflict resolution ("AoE wins"): the promoted
/// definition then resolves with provenance `global` and shadows any
/// same-named agent-native server.
pub fn upsert_global_server(server: &ProjectMcpServer) -> Result<()> {
    let entry = server_to_entry(server);
    let name = server.name.clone();
    mutate_servers(move |servers| {
        servers.insert(name, entry);
    })
}

/// Mutate the global server map under its anchored file lock.
fn with_global_servers<R, E: From<anyhow::Error>>(
    mutate: impl FnOnce(&mut Map<String, Value>) -> std::result::Result<R, E>,
) -> std::result::Result<R, E> {
    let path = global_mcp_path()?;
    crate::session::storage::LockedDataFile::open(&path)?
        .update(
            read_root,
            |root| Ok(serde_json::to_string_pretty(root)?),
            |root| {
                let servers = root
                    .entry("mcpServers")
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                    .context("mcpServers in global mcp.json is not an object")?;
                mutate(servers)
            },
        )?
        .map(|(result, _)| result)
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
    use crate::session::mcp::project_mcp::ProjectMcpTransport;

    /// Retain environment restoration and root isolation for the whole test.
    fn set_tmp_home() -> crate::session::test_support::AppDirGuard {
        crate::session::test_support::isolate_app_dir()
    }

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
    fn upsert_creates_then_replaces_and_round_trips() {
        let home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();

        upsert_global_server(&stdio("fs", "first")).unwrap();
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fs");

        // Replace same name.
        upsert_global_server(&stdio("fs", "second")).unwrap();
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            ProjectMcpTransport::Stdio { command, .. } => assert_eq!(command, "second"),
            other => panic!("expected stdio, got {other:?}"),
        }
        let _ = home;
    }

    #[test]
    #[serial_test::serial]
    fn upsert_preserves_other_servers_and_unknown_keys() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "someOtherKey": 7, "mcpServers": { "keepme": { "command": "k" } } }"#,
        )
        .unwrap();

        upsert_global_server(&stdio("added", "a")).unwrap();

        let raw = std::fs::read_to_string(app_dir.join("mcp.json")).unwrap();
        let val: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            val["someOtherKey"],
            serde_json::json!(7),
            "unknown key dropped"
        );
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["added", "keepme"]);
    }

    #[test]
    #[serial_test::serial]
    fn remote_round_trips_with_type_and_headers() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer secret".to_string());
        upsert_global_server(&ProjectMcpServer {
            name: "remote".into(),
            transport: ProjectMcpTransport::Http {
                url: "https://e/mcp".into(),
                headers,
            },
        })
        .unwrap();
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        match &servers[0].transport {
            ProjectMcpTransport::Http { url, headers } => {
                assert_eq!(url, "https://e/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer secret")
                );
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn delete_reports_absence_and_preserves_others() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        upsert_global_server(&stdio("keep", "k")).unwrap();
        upsert_global_server(&stdio("gone", "g")).unwrap();

        delete_global_server("gone").unwrap();
        assert!(matches!(
            delete_global_server("gone"),
            Err(McpWriteError::NotFound(_))
        ));
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep"], "other servers untouched");
    }

    #[test]
    #[serial_test::serial]
    fn insert_if_absent_creates_once_and_never_overwrites() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();

        create_global_server("fs", &stdio_input("first", vec![])).unwrap();
        // Second insert of the same name refuses and leaves the definition intact.
        assert!(matches!(
            create_global_server("fs", &stdio_input("second", vec![])),
            Err(McpWriteError::Exists(_))
        ));
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            ProjectMcpTransport::Stdio { command, .. } => assert_eq!(command, "first"),
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn replace_if_present_edits_only_existing() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();

        // Absent -> refused, nothing written.
        assert!(matches!(
            update_global_server("fs", &stdio_input("x", vec![])),
            Err(McpWriteError::NotFound(_))
        ));
        assert!(load_global_mcp_servers(&app_dir).unwrap().is_empty());

        upsert_global_server(&stdio("fs", "old")).unwrap();
        update_global_server("fs", &stdio_input("new", vec![])).unwrap();
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            ProjectMcpTransport::Stdio { command, .. } => assert_eq!(command, "new"),
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn unchanged_ops_do_not_write_the_file() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        let mcp = app_dir.join("mcp.json");

        // No file yet: an absent remove and an absent replace must not create it.
        assert!(matches!(
            delete_global_server("nope"),
            Err(McpWriteError::NotFound(_))
        ));
        assert!(matches!(
            update_global_server("nope", &stdio_input("x", vec![])),
            Err(McpWriteError::NotFound(_))
        ));
        assert!(!mcp.exists(), "a no-op must not create mcp.json");

        // With a known file, a duplicate insert and an absent remove leave the
        // exact bytes untouched (no spurious rewrite).
        upsert_global_server(&stdio("fs", "c")).unwrap();
        let before = std::fs::read(&mcp).unwrap();
        assert!(matches!(
            create_global_server("fs", &stdio_input("other", vec![])),
            Err(McpWriteError::Exists(_))
        ));
        assert!(matches!(
            delete_global_server("nope"),
            Err(McpWriteError::NotFound(_))
        ));
        let after = std::fs::read(&mcp).unwrap();
        assert_eq!(before, after, "unchanged ops must not rewrite mcp.json");
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
