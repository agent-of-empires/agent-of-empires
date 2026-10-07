//! Writer for the AoE-owned global `<app_dir>/mcp.json`, used to promote servers above native configs.

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use super::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

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

/// Inserts or replaces a server by name, preserving other servers and unknown
/// keys (the file is hand-edited, so it is not round-tripped through the typed model).
pub fn upsert_global_server(server: &ProjectMcpServer) -> Result<()> {
    let path = crate::session::get_app_dir()?.join("mcp.json");
    let entry = server_to_entry(server);
    crate::session::storage::locked_update(
        &path,
        |content| match serde_json::from_str::<Value>(content).context("parsing global mcp.json")? {
            Value::Object(m) => Ok(m),
            _ => anyhow::bail!("global mcp.json is not a JSON object"),
        },
        |root| Ok(serde_json::to_string_pretty(root)?),
        |root: &mut Map<String, Value>| -> Result<()> {
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .context("mcpServers in global mcp.json is not an object")?;
            servers.insert(server.name.clone(), entry);
            Ok(())
        },
    )?
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
}
