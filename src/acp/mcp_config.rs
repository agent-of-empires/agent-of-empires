//! Global MCP server config (`<app_dir>/mcp.json`) parsing and conversion
//! to ACP `McpServer` values forwarded at session creation.
//!
//! AoE forwards the user's MCP servers to structured-view ACP agents through
//! `session/new` and `session/load`; without this the agent reaches no MCP
//! servers at all. The on-disk format mirrors the ecosystem-standard
//! `.mcp.json` so users can reuse the definitions they already maintain for
//! Claude, Gemini, and Codex:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs":     { "command": "mcp-fs", "args": ["--root", "."], "env": { "K": "v" } },
//!     "remote": { "type": "http", "url": "https://example/mcp", "headers": { "Authorization": "Bearer x" } }
//!   }
//! }
//! ```
//!
//! The global file in the AoE app dir and a per-profile `<profile_dir>/mcp.json`
//! (issue #1986) are both read; the per-profile layer merges above global. A
//! project-local `cwd/.mcp.json` is intentionally NOT read: AoE opens cloned and
//! potentially untrusted repositories, and stdio MCP servers launch
//! unconditionally when a session spawns, so project scope must sit behind the
//! repo-trust gate (the same boundary that already protects lifecycle hooks).
//! That project-local layer is tracked as a follow-up.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use agent_client_protocol::schema::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::warn;

/// On-disk shape of `<app_dir>/mcp.json`. Unknown top-level keys are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawServer>,
}

/// A single server entry. Absent `type` (or `type: "stdio"`) selects the stdio
/// transport; `type: "http"` / `type: "sse"` select the remote transports. Maps
/// are `BTreeMap` so the converted output ordering is deterministic for tests.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(default, rename = "type")]
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Read and parse the global `<app_dir>/mcp.json`. A missing file yields an
/// empty list (the no-config case behaves exactly as before this feature). A
/// present-but-malformed file is an error the caller surfaces; it must not be
/// silently treated as "no servers".
pub fn load_global_mcp_servers(app_dir: &Path) -> Result<Vec<McpServer>> {
    read_mcp_json(&app_dir.join("mcp.json"))
}

/// Read and parse a profile's `<profile_dir>/mcp.json` (issue #1986). Same
/// on-disk shape and same missing/malformed semantics as the global file; it is
/// just resolved from the active session's profile directory. The per-profile
/// layer is merged ABOVE global (so a same-named server in the profile file
/// overrides the global one) and below project-local. Per-profile entries are
/// AoE state only: they are never written back to any agent's native config.
pub fn load_profile_mcp_servers(profile_dir: &Path) -> Result<Vec<McpServer>> {
    read_mcp_json(&profile_dir.join("mcp.json"))
}

/// Read and parse an `mcp.json` at `path`. A missing file yields an empty list;
/// a present-but-malformed file is an error the caller surfaces. Shared by the
/// global and per-profile loaders so both layers behave identically.
fn read_mcp_json(path: &Path) -> Result<Vec<McpServer>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading MCP config at {}", path.display()))
        }
    };
    parse_mcp_servers(&text).with_context(|| format!("parsing MCP config at {}", path.display()))
}

/// Parse the `.mcp.json` text into ACP `McpServer` values. Separated from the
/// file read so the conversion rules can be unit-tested without touching disk.
fn parse_mcp_servers(text: &str) -> Result<Vec<McpServer>> {
    let parsed: McpConfigFile = serde_json::from_str(text)?;
    parsed
        .mcp_servers
        .into_iter()
        .map(|(name, raw)| convert_server(&name, raw))
        .collect()
}

fn convert_server(name: &str, raw: RawServer) -> Result<McpServer> {
    match raw.transport.as_deref() {
        None | Some("stdio") => {
            let command = raw
                .command
                .with_context(|| format!("MCP server \"{name}\" is missing \"command\""))?;
            Ok(McpServer::Stdio(
                McpServerStdio::new(name, command)
                    .args(raw.args)
                    .env(to_env(raw.env)),
            ))
        }
        Some("http") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            Ok(McpServer::Http(
                McpServerHttp::new(name, url).headers(to_headers(raw.headers)),
            ))
        }
        Some("sse") => {
            let url = raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?;
            Ok(McpServer::Sse(
                McpServerSse::new(name, url).headers(to_headers(raw.headers)),
            ))
        }
        Some(other) => bail!("MCP server \"{name}\" has unknown type \"{other}\""),
    }
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

/// Names + transports of the configured servers for logging. Deliberately omits
/// env values and header values so secrets never reach the log sink.
pub fn summarize(servers: &[McpServer]) -> String {
    servers
        .iter()
        .map(|s| format!("{}({})", server_name(s), server_kind(s)))
        .collect::<Vec<_>>()
        .join(", ")
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

/// Convert the always-compiled project-local servers (parsed and fingerprinted
/// by `session::project_mcp` outside the serve gate) into ACP `McpServer`
/// values for forwarding (#1985). The supervisor calls this only after the repo
/// is trusted for the matching fingerprint, so the converted set is exactly the
/// reviewed set.
pub fn project_servers_to_acp(
    servers: Vec<crate::session::project_mcp::ProjectMcpServer>,
) -> Vec<McpServer> {
    use crate::session::project_mcp::ProjectMcpTransport;
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

/// A named source of MCP servers for precedence merging. Layers are passed
/// lowest-precedence first; on a server-name collision the higher (later) layer
/// wins and both labels are logged so the override is visible. The `label` is
/// the provenance the unified MCP surface (issue #1996) will display; it is
/// read here today in the collision warning, so it is not dead state.
pub struct McpLayer {
    pub label: &'static str,
    pub servers: Vec<McpServer>,
}

/// Merge MCP server layers by precedence. `layers` are ordered lowest first; a
/// server in a later layer overrides one of the same name in an earlier layer
/// (per-server, not whole-layer). Output is name-sorted so forwarding is
/// deterministic. This is the reusable seam the per-profile (#1986) and
/// project-local (#1985) layers extend by appending their own `McpLayer`.
pub fn merge_by_precedence(layers: Vec<McpLayer>) -> Vec<McpServer> {
    let mut by_name: BTreeMap<String, (&'static str, McpServer)> = BTreeMap::new();
    for layer in layers {
        for server in layer.servers {
            let name = server_name(&server).to_string();
            if let Some((shadowed, _)) = by_name.get(&name) {
                warn!(
                    target: "acp.mcp",
                    server = %name,
                    kept = layer.label,
                    shadowed = *shadowed,
                    "MCP server name defined in multiple sources; higher-precedence source wins"
                );
            }
            by_name.insert(name, (layer.label, server));
        }
    }
    by_name.into_values().map(|(_, server)| server).collect()
}

/// Where an agent keeps its own MCP server config, and in what shape. This is
/// the per-agent seam: adding an agent is one match arm in `native_config_for`
/// plus, if its format differs, one converter. AoE only ever reads these files;
/// it never writes them.
enum NativeMcpConfig {
    /// Standard `{ "mcpServers": { ... } }` JSON with the ecosystem `type`/`url`
    /// shape, at a home-relative path. Claude (`~/.claude.json`).
    StandardJson(&'static str),
    /// Gemini `settings.json`: an `mcpServers` block whose entries discriminate
    /// transport by which key is present (`command` -> stdio, `httpUrl` -> http,
    /// `url` -> sse) rather than by a `type` field.
    GeminiJson(&'static str),
    /// Codex `config.toml`: `[mcp_servers.<name>]` tables (stdio, or `url` for
    /// streamable http on newer Codex).
    CodexToml(&'static str),
}

/// Map an agent registry key to its native MCP config descriptor, or `None` for
/// an agent AoE has no native reader for (those contribute no native servers).
fn native_config_for(agent_key: &str) -> Option<NativeMcpConfig> {
    match agent_key {
        "claude" | "claude-code" => Some(NativeMcpConfig::StandardJson(".claude.json")),
        "gemini" => Some(NativeMcpConfig::GeminiJson(".gemini/settings.json")),
        "codex" => Some(NativeMcpConfig::CodexToml(".codex/config.toml")),
        _ => None,
    }
}

/// Read the active agent's own MCP config (the file its CLI reads) and convert
/// it to ACP servers. Live read-through: called once per spawn, no caching, so
/// edits are picked up on the next session. Returns an empty list for an agent
/// with no known native reader and for a missing file. A present-but-unparseable
/// file is an error the caller downgrades to a warning, so a broken native file
/// (which AoE does not own) never blocks a spawn. Individual malformed server
/// entries are skipped with a warning rather than failing the whole file.
pub fn load_native_mcp_servers(agent_key: &str, home: &Path) -> Result<Vec<McpServer>> {
    let Some(config) = native_config_for(agent_key) else {
        return Ok(Vec::new());
    };
    match config {
        NativeMcpConfig::StandardJson(rel) => read_standard_json(&home.join(rel)),
        NativeMcpConfig::GeminiJson(rel) => read_gemini_json(&home.join(rel)),
        NativeMcpConfig::CodexToml(rel) => read_codex_toml(&home.join(rel)),
    }
}

/// Convenience wrapper that resolves the real home dir. Kept separate from
/// `load_native_mcp_servers` so tests can inject a temp home.
pub fn load_native_mcp_servers_from_home(agent_key: &str) -> Result<Vec<McpServer>> {
    let home = dirs::home_dir().context("could not resolve home dir for native MCP config")?;
    load_native_mcp_servers(agent_key, &home)
}

/// Convert a map of raw server entries, skipping (with a warning) any entry that
/// fails to convert rather than failing the whole file. Native configs are owned
/// by other tools, so one bad entry must not discard the user's other servers.
fn convert_tolerant<R>(
    servers: BTreeMap<String, R>,
    path: &Path,
    convert: impl Fn(&str, R) -> Result<McpServer>,
) -> Vec<McpServer> {
    servers
        .into_iter()
        .filter_map(|(name, raw)| match convert(&name, raw) {
            Ok(server) => Some(server),
            Err(e) => {
                warn!(
                    target: "acp.mcp",
                    server = %name,
                    path = %path.display(),
                    error = %e,
                    "skipping malformed MCP server in native config"
                );
                None
            }
        })
        .collect()
}

/// Read a JSON config in the standard `mcpServers` shape, streaming so a large
/// host file (e.g. `~/.claude.json`, which also holds project history) is parsed
/// without materializing the unrelated keys: serde skips them in the reader.
fn read_standard_json(path: &Path) -> Result<Vec<McpServer>> {
    let Some(file) = open_optional(path)? else {
        return Ok(Vec::new());
    };
    let parsed: McpConfigFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(parsed.mcp_servers, path, convert_server))
}

/// Gemini `settings.json` reader: same streaming approach as Claude, but entries
/// discriminate transport by key rather than a `type` field.
fn read_gemini_json(path: &Path) -> Result<Vec<McpServer>> {
    let Some(file) = open_optional(path)? else {
        return Ok(Vec::new());
    };
    let parsed: GeminiConfigFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(
        parsed.mcp_servers,
        path,
        convert_gemini_server,
    ))
}

/// Codex `config.toml` reader. These files are small (no embedded history), so a
/// whole-string read is fine; `toml` has no streaming reader.
fn read_codex_toml(path: &Path) -> Result<Vec<McpServer>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading native MCP config at {}", path.display()))
        }
    };
    let parsed: CodexConfigFile = toml::from_str(&text)
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(
        parsed.mcp_servers,
        path,
        convert_codex_server,
    ))
}

/// Open a config file, mapping "not found" to `None` (the user does not use that
/// agent) and any other IO error to a context-tagged failure.
fn open_optional(path: &Path) -> Result<Option<File>> {
    match File::open(path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("opening native MCP config at {}", path.display()))
        }
    }
}

/// On-disk shape of a Gemini `mcpServers` entry. Transport is selected by which
/// of `command` / `httpUrl` / `url` is present; more than one is ambiguous.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, GeminiRawServer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRawServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    http_url: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn convert_gemini_server(name: &str, raw: GeminiRawServer) -> Result<McpServer> {
    match (raw.command, raw.http_url, raw.url) {
        (Some(command), None, None) => Ok(McpServer::Stdio(
            McpServerStdio::new(name, command)
                .args(raw.args)
                .env(to_env(raw.env)),
        )),
        (None, Some(http_url), None) => Ok(McpServer::Http(
            McpServerHttp::new(name, http_url).headers(to_headers(raw.headers)),
        )),
        (None, None, Some(url)) => Ok(McpServer::Sse(
            McpServerSse::new(name, url).headers(to_headers(raw.headers)),
        )),
        (None, None, None) => {
            bail!("MCP server \"{name}\" has none of \"command\", \"httpUrl\", \"url\"")
        }
        _ => bail!("MCP server \"{name}\" sets more than one of \"command\", \"httpUrl\", \"url\""),
    }
}

/// On-disk shape of a Codex `[mcp_servers.<name>]` entry. `command` selects
/// stdio; `url` selects streamable http (newer Codex). The TOML key is already
/// snake_case, so no rename is needed.
#[derive(Debug, Deserialize)]
struct CodexConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, CodexRawServer>,
}

#[derive(Debug, Deserialize)]
struct CodexRawServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn convert_codex_server(name: &str, raw: CodexRawServer) -> Result<McpServer> {
    match (raw.command, raw.url) {
        (Some(command), None) => Ok(McpServer::Stdio(
            McpServerStdio::new(name, command)
                .args(raw.args)
                .env(to_env(raw.env)),
        )),
        (None, Some(url)) => Ok(McpServer::Http(
            McpServerHttp::new(name, url).headers(to_headers(raw.headers)),
        )),
        (None, None) => bail!("MCP server \"{name}\" has neither \"command\" nor \"url\""),
        (Some(_), Some(_)) => bail!("MCP server \"{name}\" sets both \"command\" and \"url\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(servers: &[McpServer]) -> Vec<&str> {
        servers.iter().map(server_name).collect()
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_global_mcp_servers(dir.path()).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn loads_and_parses_app_dir_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "mcp-fs" } } }"#,
        )
        .unwrap();
        let servers = load_global_mcp_servers(dir.path()).unwrap();
        assert_eq!(names(&servers), vec!["fs"]);
    }

    #[test]
    fn malformed_app_dir_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
        assert!(load_global_mcp_servers(dir.path()).is_err());
    }

    #[test]
    fn empty_or_absent_mcp_servers_key_is_empty() {
        assert!(parse_mcp_servers("{}").unwrap().is_empty());
        assert!(parse_mcp_servers(r#"{"mcpServers":{}}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parses_stdio_entry() {
        let text = r#"{
            "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "secret" } }
            }
        }"#;
        let servers = parse_mcp_servers(text).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "fs");
                assert_eq!(s.command.to_string_lossy(), "mcp-fs");
                assert_eq!(s.args, vec!["--root".to_string(), ".".to_string()]);
                assert_eq!(s.env.len(), 1);
                assert_eq!(s.env[0].name, "TOKEN");
                assert_eq!(s.env[0].value, "secret");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn parses_remote_entries() {
        let text = r#"{
            "mcpServers": {
                "h": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                "s": { "type": "sse", "url": "https://e/sse" }
            }
        }"#;
        let servers = parse_mcp_servers(text).unwrap();
        // BTreeMap key ordering: "h" before "s".
        match &servers[0] {
            McpServer::Http(h) => {
                assert_eq!(h.name, "h");
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer x");
            }
            other => panic!("expected http, got {other:?}"),
        }
        match &servers[1] {
            McpServer::Sse(s) => assert_eq!(s.url, "https://e/sse"),
            other => panic!("expected sse, got {other:?}"),
        }
    }

    #[test]
    fn ordering_is_deterministic() {
        let text = r#"{ "mcpServers": {
            "zebra": { "command": "z" },
            "alpha": { "command": "a" },
            "mike":  { "command": "m" }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();
        assert_eq!(names(&servers), vec!["alpha", "mike", "zebra"]);
    }

    #[test]
    fn unknown_type_is_error() {
        let text = r#"{ "mcpServers": { "x": { "type": "carrier-pigeon", "url": "u" } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn stdio_without_command_is_error() {
        let text = r#"{ "mcpServers": { "x": { "args": ["--y"] } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn remote_without_url_is_error() {
        let text = r#"{ "mcpServers": { "x": { "type": "http" } } }"#;
        assert!(parse_mcp_servers(text).is_err());
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(parse_mcp_servers("{ not json").is_err());
    }

    #[test]
    fn capability_filter_keeps_stdio_drops_unadvertised_remotes() {
        let text = r#"{ "mcpServers": {
            "stdio":  { "command": "c" },
            "http":   { "type": "http", "url": "u" },
            "sse":    { "type": "sse",  "url": "u" }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();

        let none = McpCapabilities::new();
        let kept = filter_for_capabilities(servers.clone(), &none, "t");
        assert_eq!(names(&kept), vec!["stdio"]);

        let http_only = McpCapabilities::new().http(true);
        let kept = filter_for_capabilities(servers.clone(), &http_only, "t");
        assert_eq!(names(&kept), vec!["http", "stdio"]);

        let both = McpCapabilities::new().http(true).sse(true);
        let kept = filter_for_capabilities(servers, &both, "t");
        assert_eq!(names(&kept), vec!["http", "sse", "stdio"]);
    }

    #[test]
    fn summarize_omits_secret_values() {
        let text = r#"{ "mcpServers": {
            "fs": { "command": "c", "env": { "TOKEN": "supersecret" } }
        }}"#;
        let servers = parse_mcp_servers(text).unwrap();
        let s = summarize(&servers);
        assert_eq!(s, "fs(stdio)");
        assert!(!s.contains("supersecret"));
    }

    fn layer(label: &'static str, json: &str) -> McpLayer {
        McpLayer {
            label,
            servers: parse_mcp_servers(json).unwrap(),
        }
    }

    fn stdio_command(server: &McpServer) -> &str {
        match server {
            McpServer::Stdio(s) => s.command.to_str().unwrap(),
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn merge_higher_layer_overrides_same_name() {
        // `low` first, `high` second; on the shared "fs" name, high wins.
        let low = layer(
            "agent-native",
            r#"{ "mcpServers": { "fs": { "command": "native" } } }"#,
        );
        let high = layer(
            "global",
            r#"{ "mcpServers": { "fs": { "command": "global" } } }"#,
        );
        let merged = merge_by_precedence(vec![low, high]);
        assert_eq!(names(&merged), vec!["fs"]);
        assert_eq!(stdio_command(&merged[0]), "global");
    }

    #[test]
    fn merge_unions_distinct_names_sorted() {
        let low = layer(
            "agent-native",
            r#"{ "mcpServers": { "zebra": { "command": "z" }, "fs": { "command": "n" } } }"#,
        );
        let high = layer(
            "global",
            r#"{ "mcpServers": { "alpha": { "command": "a" } } }"#,
        );
        let merged = merge_by_precedence(vec![low, high]);
        assert_eq!(names(&merged), vec!["alpha", "fs", "zebra"]);
    }

    #[test]
    fn merge_empty_layers_is_empty() {
        assert!(merge_by_precedence(vec![]).is_empty());
        let empty = McpLayer {
            label: "global",
            servers: Vec::new(),
        };
        assert!(merge_by_precedence(vec![empty]).is_empty());
    }

    #[test]
    fn profile_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_profile_mcp_servers(dir.path()).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn profile_loads_and_parses_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "profile-fs" } } }"#,
        )
        .unwrap();
        let servers = load_profile_mcp_servers(dir.path()).unwrap();
        assert_eq!(names(&servers), vec!["fs"]);
    }

    #[test]
    fn profile_malformed_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
        assert!(load_profile_mcp_servers(dir.path()).is_err());
    }

    #[test]
    fn merge_profile_overrides_global() {
        // Ordering native < global < per-profile: on the shared "fs" name, the
        // per-profile layer (highest of the three) wins.
        let native = layer(
            "agent-native",
            r#"{ "mcpServers": { "fs": { "command": "native" } } }"#,
        );
        let global = layer(
            "global",
            r#"{ "mcpServers": { "fs": { "command": "global" } } }"#,
        );
        let profile = layer(
            "per-profile",
            r#"{ "mcpServers": { "fs": { "command": "profile" } } }"#,
        );
        let merged = merge_by_precedence(vec![native, global, profile]);
        assert_eq!(names(&merged), vec!["fs"]);
        assert_eq!(stdio_command(&merged[0]), "profile");
    }

    #[test]
    fn project_servers_to_acp_converts_all_transports() {
        use crate::session::project_mcp::{ProjectMcpServer, ProjectMcpTransport};
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "secret".to_string());
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer x".to_string());
        let project = vec![
            ProjectMcpServer {
                name: "a-stdio".to_string(),
                transport: ProjectMcpTransport::Stdio {
                    command: "cmd".to_string(),
                    args: vec!["--arg".to_string()],
                    env,
                },
            },
            ProjectMcpServer {
                name: "b-http".to_string(),
                transport: ProjectMcpTransport::Http {
                    url: "https://e/mcp".to_string(),
                    headers,
                },
            },
            ProjectMcpServer {
                name: "c-sse".to_string(),
                transport: ProjectMcpTransport::Sse {
                    url: "https://e/sse".to_string(),
                    headers: BTreeMap::new(),
                },
            },
        ];
        let acp = project_servers_to_acp(project);
        assert_eq!(names(&acp), vec!["a-stdio", "b-http", "c-sse"]);
        match &acp[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.command.to_string_lossy(), "cmd");
                assert_eq!(s.args, vec!["--arg".to_string()]);
                assert_eq!(s.env[0].name, "TOKEN");
                assert_eq!(s.env[0].value, "secret");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &acp[1] {
            McpServer::Http(h) => {
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
            }
            other => panic!("expected http, got {other:?}"),
        }
        assert!(matches!(&acp[2], McpServer::Sse(_)));
    }

    fn write(home: &Path, rel: &str, contents: &str) {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn native_unknown_agent_is_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(load_native_mcp_servers("opencode", home.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn native_missing_file_is_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(load_native_mcp_servers("claude", home.path())
            .unwrap()
            .is_empty());
        assert!(load_native_mcp_servers("gemini", home.path())
            .unwrap()
            .is_empty());
        assert!(load_native_mcp_servers("codex", home.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn native_claude_parses_and_ignores_history() {
        let home = tempfile::tempdir().unwrap();
        // `~/.claude.json` also holds unrelated keys (project history); they must
        // be skipped without affecting the mcpServers parse.
        write(
            home.path(),
            ".claude.json",
            r#"{
                "projects": { "/some/path": { "lastSessionId": "abc" } },
                "numStartups": 42,
                "mcpServers": {
                    "fs": { "command": "mcp-fs", "args": ["--root", "."] },
                    "remote": { "type": "http", "url": "https://e/mcp" }
                }
            }"#,
        );
        let servers = load_native_mcp_servers("claude", home.path()).unwrap();
        assert_eq!(names(&servers), vec!["fs", "remote"]);
        // `claude-code` is the legacy alias and resolves to the same reader.
        let aliased = load_native_mcp_servers("claude-code", home.path()).unwrap();
        assert_eq!(names(&aliased), vec!["fs", "remote"]);
    }

    #[test]
    fn native_claude_skips_bad_entry_keeps_rest() {
        let home = tempfile::tempdir().unwrap();
        // The "broken" stdio entry has no command; it is skipped, "ok" survives.
        write(
            home.path(),
            ".claude.json",
            r#"{ "mcpServers": {
                "broken": { "args": ["--x"] },
                "ok": { "command": "good" }
            } }"#,
        );
        let servers = load_native_mcp_servers("claude", home.path()).unwrap();
        assert_eq!(names(&servers), vec!["ok"]);
    }

    #[test]
    fn native_claude_malformed_file_is_error() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), ".claude.json", "{ not json");
        assert!(load_native_mcp_servers("claude", home.path()).is_err());
    }

    #[test]
    fn native_gemini_discriminates_transport_by_key() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".gemini/settings.json",
            r#"{
                "theme": "dark",
                "mcpServers": {
                    "local":  { "command": "g", "args": ["--x"] },
                    "httpish": { "httpUrl": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                    "sseish":  { "url": "https://e/sse" }
                }
            }"#,
        );
        let servers = load_native_mcp_servers("gemini", home.path()).unwrap();
        // BTreeMap order: httpish, local, sseish.
        assert_eq!(names(&servers), vec!["httpish", "local", "sseish"]);
        match &servers[0] {
            McpServer::Http(h) => {
                assert_eq!(h.url, "https://e/mcp");
                assert_eq!(h.headers[0].name, "Authorization");
            }
            other => panic!("expected http from httpUrl, got {other:?}"),
        }
        assert!(matches!(&servers[1], McpServer::Stdio(_)));
        assert!(matches!(&servers[2], McpServer::Sse(_)));
    }

    #[test]
    fn native_gemini_ambiguous_entry_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        // "ambiguous" sets both command and httpUrl; skipped, "ok" survives.
        write(
            home.path(),
            ".gemini/settings.json",
            r#"{ "mcpServers": {
                "ambiguous": { "command": "c", "httpUrl": "https://e/mcp" },
                "ok": { "command": "good" }
            } }"#,
        );
        let servers = load_native_mcp_servers("gemini", home.path()).unwrap();
        assert_eq!(names(&servers), vec!["ok"]);
    }

    #[test]
    fn native_codex_parses_toml() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".codex/config.toml",
            r#"
model = "gpt-5"

[mcp_servers.fs]
command = "mcp-fs"
args = ["--root", "."]
env = { TOKEN = "secret" }

[mcp_servers.remote]
url = "https://e/mcp"
"#,
        );
        let servers = load_native_mcp_servers("codex", home.path()).unwrap();
        assert_eq!(names(&servers), vec!["fs", "remote"]);
        assert_eq!(stdio_command(&servers[0]), "mcp-fs");
        assert!(matches!(&servers[1], McpServer::Http(_)));
        // Secret env value never appears in the summary.
        assert!(!summarize(&servers).contains("secret"));
    }

    #[test]
    fn native_codex_malformed_file_is_error() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), ".codex/config.toml", "this = = not toml");
        assert!(load_native_mcp_servers("codex", home.path()).is_err());
    }
}
