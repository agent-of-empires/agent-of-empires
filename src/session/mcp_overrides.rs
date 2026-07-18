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
/// The closure's return value is passed back to the caller, so an existence
/// check and the mutation it gates run inside the same lock (no check-then-act
/// race against a concurrent surface write).
fn mutate_servers<T>(mutate: impl FnOnce(&mut Map<String, Value>) -> T) -> Result<T> {
    let path = global_mcp_path()?;
    super::storage::locked_update(
        &path,
        read_root,
        |root| Ok(serde_json::to_string_pretty(root)?),
        |root| -> Result<T> {
            let mut servers = take_servers(root)?;
            let out = mutate(&mut servers);
            root.insert("mcpServers".into(), Value::Object(servers));
            Ok(out)
        },
    )?
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

/// Reason a conditional mutation did not persist, carried through
/// `locked_update`'s error channel (it writes only on `Ok`). `Unchanged` means
/// the operation was a no-op, so the file is left byte-for-byte untouched (no
/// create, no rewrite); `Corrupt` propagates a malformed-file error.
enum SkipWrite {
    Unchanged,
    Corrupt(anyhow::Error),
}

/// Run a conditional mutation on the global `mcpServers` map under the exclusive
/// lock. `mutate` returns whether it changed anything; when it did not, the file
/// is left exactly as it was (a rejected or idempotent op performs no write) and
/// `false` is returned. This is what lets a forbidden delete or a duplicate add
/// touch nothing on disk.
fn try_mutate_servers(mutate: impl FnOnce(&mut Map<String, Value>) -> bool) -> Result<bool> {
    let path = global_mcp_path()?;
    let outcome = super::storage::locked_update(
        &path,
        read_root,
        |root| Ok(serde_json::to_string_pretty(root)?),
        |root| -> std::result::Result<(), SkipWrite> {
            let mut servers = take_servers(root).map_err(SkipWrite::Corrupt)?;
            if !mutate(&mut servers) {
                return Err(SkipWrite::Unchanged);
            }
            root.insert("mcpServers".into(), Value::Object(servers));
            Ok(())
        },
    )?;
    match outcome {
        Ok(()) => Ok(true),
        Err(SkipWrite::Unchanged) => Ok(false),
        Err(SkipWrite::Corrupt(e)) => Err(e),
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

/// Remove a server from the global `mcp.json` by name. Returns `true` if a
/// server of that name was present and removed, `false` if it was already
/// absent. The existence check runs inside the same exclusive lock as the
/// removal, so a concurrent surface write cannot slip between the two; an absent
/// name leaves the file untouched (no write).
pub fn remove_global_server(name: &str) -> Result<bool> {
    let name = name.to_string();
    try_mutate_servers(move |servers| servers.remove(&name).is_some())
}

/// Insert a server into the global `mcp.json` only if no server of that name is
/// already present. Returns `true` on insert, `false` if the name already
/// existed (the caller then reports "already exists; use edit"). Existence is
/// checked on the raw server map under the lock, so a malformed existing entry
/// still counts as present and is never silently overwritten; a duplicate leaves
/// the file untouched (no write).
pub fn insert_global_server_if_absent(server: &ProjectMcpServer) -> Result<bool> {
    let entry = server_to_entry(server);
    let name = server.name.clone();
    try_mutate_servers(move |servers| {
        if servers.contains_key(&name) {
            false
        } else {
            servers.insert(name, entry);
            true
        }
    })
}

/// Replace an existing global server definition, only if a server of that name
/// is already present. Returns `true` on replace, `false` if absent (the caller
/// then reports "no such global server; use add"). Checked under the lock on the
/// raw map, mirroring [`insert_global_server_if_absent`]; an absent name leaves
/// the file untouched (no write).
pub fn replace_global_server_if_present(server: &ProjectMcpServer) -> Result<bool> {
    let entry = server_to_entry(server);
    let name = server.name.clone();
    try_mutate_servers(move |servers| {
        if servers.contains_key(&name) {
            servers.insert(name, entry);
            true
        } else {
            false
        }
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
    fn remove_returns_whether_present_and_preserves_others() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();
        upsert_global_server(&stdio("keep", "k")).unwrap();
        upsert_global_server(&stdio("gone", "g")).unwrap();

        assert!(remove_global_server("gone").unwrap(), "present -> removed");
        assert!(
            !remove_global_server("gone").unwrap(),
            "absent -> false, idempotent"
        );
        let servers = load_global_mcp_servers(&app_dir).unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep"], "other servers untouched");
    }

    #[test]
    #[serial_test::serial]
    fn insert_if_absent_creates_once_and_never_overwrites() {
        let _home = set_tmp_home();
        let app_dir = crate::session::get_app_dir().unwrap();

        assert!(insert_global_server_if_absent(&stdio("fs", "first")).unwrap());
        // Second insert of the same name refuses and leaves the definition intact.
        assert!(!insert_global_server_if_absent(&stdio("fs", "second")).unwrap());
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
        assert!(!replace_global_server_if_present(&stdio("fs", "x")).unwrap());
        assert!(load_global_mcp_servers(&app_dir).unwrap().is_empty());

        upsert_global_server(&stdio("fs", "old")).unwrap();
        assert!(replace_global_server_if_present(&stdio("fs", "new")).unwrap());
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
        assert!(!remove_global_server("nope").unwrap());
        assert!(!replace_global_server_if_present(&stdio("nope", "x")).unwrap());
        assert!(!mcp.exists(), "a no-op must not create mcp.json");

        // With a known file, a duplicate insert and an absent remove leave the
        // exact bytes untouched (no spurious rewrite).
        upsert_global_server(&stdio("fs", "c")).unwrap();
        let before = std::fs::read(&mcp).unwrap();
        assert!(!insert_global_server_if_absent(&stdio("fs", "other")).unwrap());
        assert!(!remove_global_server("nope").unwrap());
        let after = std::fs::read(&mcp).unwrap();
        assert_eq!(before, after, "unchanged ops must not rewrite mcp.json");
    }
}
