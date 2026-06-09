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

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use super::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

/// Path to the global `mcp.json` AoE owns and may write.
fn global_mcp_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join("mcp.json"))
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
fn mutate_servers(mutate: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

    let path = global_mcp_path()?;
    if !path.exists() {
        std::fs::write(&path, "").with_context(|| format!("creating {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive().context("locking global mcp.json")?;

    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let mut root = read_root(&content)?;

    let mut servers = match root.remove("mcpServers") {
        Some(Value::Object(m)) => m,
        Some(_) => anyhow::bail!("mcpServers in global mcp.json is not an object"),
        None => Map::new(),
    };
    mutate(&mut servers);
    root.insert("mcpServers".into(), Value::Object(servers));

    let new_content = serde_json::to_string_pretty(&Value::Object(root))?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(new_content.as_bytes())?;
    Ok(())
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

/// Remove a server from the global `mcp.json` by name. A no-op if it is not
/// present. Used by the surface to delete a `global`-provenance entry.
pub fn remove_global_server(name: &str) -> Result<()> {
    let name = name.to_string();
    mutate_servers(move |servers| {
        servers.remove(&name);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::mcp_model::load_global_mcp_servers;
    use crate::session::project_mcp::ProjectMcpTransport;

    /// Serialized across the suite by `#[serial_test::serial]`; the returned
    /// `TempDir` must outlive the test body.
    fn set_tmp_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by `#[serial]`; matches the existing pattern.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join(".config"));
        }
        dir
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
    fn remove_deletes_by_name() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        upsert_global_server(&stdio("a", "x")).unwrap();
        upsert_global_server(&stdio("b", "y")).unwrap();
        remove_global_server("a").unwrap();
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b"]);
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
}
