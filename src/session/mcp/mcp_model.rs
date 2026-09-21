//! Resolver for the effective MCP server set, shared by forwarding and every display surface.
//!
//! Layers, lowest first: agent-native, global, per-profile, project-local; higher
//! wins per server name. Display must go through [`ResolvedMcpServer::redacted`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::project_mcp::{
    convert_standard, load_standard_mcp_servers, ProjectMcpServer, ProjectMcpTransport,
    StandardMcpFile,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpProvenance {
    AgentNative {
        agent: String,
    },
    Global,
    Profile {
        name: String,
    },
    ProjectLocal,
    /// Vanished from the agent's native config; shown but not forwarded until kept or dropped.
    KeptOnRemoval {
        agent: String,
    },
}

impl McpProvenance {
    pub fn label(&self) -> String {
        match self {
            McpProvenance::AgentNative { agent } => format!("agent-native:{agent}"),
            McpProvenance::Global => "global".to_string(),
            McpProvenance::Profile { name } => format!("profile:{name}"),
            McpProvenance::ProjectLocal => "project-local".to_string(),
            McpProvenance::KeptOnRemoval { agent } => format!("kept-on-removal:{agent}"),
        }
    }
}

pub struct McpLayer {
    pub provenance: McpProvenance,
    pub servers: Vec<ProjectMcpServer>,
}

/// A winning definition plus the lower layers it shadowed, lowest first.
#[derive(Debug, Clone)]
pub struct ResolvedMcpServer {
    pub def: ProjectMcpServer,
    pub provenance: McpProvenance,
    pub shadowed: Vec<McpProvenance>,
}

impl ResolvedMcpServer {
    /// The only display view: env and header values are reduced to their names.
    pub fn redacted(&self) -> RedactedMcpServer {
        let mut view = RedactedMcpServer {
            name: self.def.name.clone(),
            transport: self.def.kind(),
            command: None,
            args: Vec::new(),
            url: None,
            env_names: Vec::new(),
            header_names: Vec::new(),
            provenance: self.provenance.label(),
            shadowed: self.shadowed.iter().map(McpProvenance::label).collect(),
        };
        match &self.def.transport {
            ProjectMcpTransport::Stdio { command, args, env } => {
                view.command = Some(command.clone());
                view.args = args.clone();
                view.env_names = env.keys().cloned().collect();
            }
            ProjectMcpTransport::Http { url, headers }
            | ProjectMcpTransport::Sse { url, headers } => {
                view.url = Some(url.clone());
                view.header_names = headers.keys().cloned().collect();
            }
        }
        view
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RedactedMcpServer {
    pub name: String,
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header_names: Vec<String>,
    pub provenance: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<String>,
}

/// Merges `layers` (lowest first) per server name; output is name-sorted.
pub fn resolve(layers: Vec<McpLayer>) -> Vec<ResolvedMcpServer> {
    let mut by_name: BTreeMap<String, ResolvedMcpServer> = BTreeMap::new();
    for layer in layers {
        for server in layer.servers {
            let provenance = layer.provenance.clone();
            match by_name.get_mut(&server.name) {
                Some(existing) => {
                    let shadowed = std::mem::replace(&mut existing.provenance, provenance);
                    existing.shadowed.push(shadowed);
                    existing.def = server;
                }
                None => {
                    let resolved = ResolvedMcpServer {
                        def: server,
                        provenance,
                        shadowed: Vec::new(),
                    };
                    by_name.insert(resolved.def.name.clone(), resolved);
                }
            }
        }
    }
    by_name.into_values().collect()
}

/// Names and transports only, safe for logs.
pub fn summarize(servers: &[ResolvedMcpServer]) -> String {
    servers
        .iter()
        .map(|s| format!("{}({})", s.def.name, s.def.kind()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The valued static `environment` entries a host session of `profile` launches
/// with. Spawn passes its full minted environment instead (includes `before_session`).
pub fn session_env_for_discovery(profile: Option<&str>) -> Vec<(String, String)> {
    let valued: Vec<String> = resolved_profile_config(profile)
        .environment
        .into_iter()
        .filter(|entry| entry.contains('='))
        .collect();
    crate::session::environment::resolve_host_environment_pairs(&valued)
}

fn resolved_profile_config(profile: Option<&str>) -> crate::session::config::Config {
    crate::session::config::profile_config::resolve_config_or_warn(
        &crate::session::config::effective_profile(profile.unwrap_or_default()),
    )
}

/// A broken source must never block a spawn: warn and contribute nothing.
fn or_warn<T: Default>(result: Result<T>, source: &str) -> T {
    result.unwrap_or_else(|e| {
        warn!(target: "acp.mcp", error = %e, "failed to load {source} MCP config; contributing none from it");
        T::default()
    })
}

/// The set forwarded to the agent for this session context. Project-local servers
/// are included only when the repo is trusted at the file's current fingerprint.
pub fn resolve_effective(
    agent_key: &str,
    profile: Option<&str>,
    cwd: &Path,
    session_env: &[(String, String)],
) -> Vec<ResolvedMcpServer> {
    let native = load_native_mcp_servers_checked_from_home(agent_key, profile, session_env)
        .map(NativeRead::into_enabled);
    let global = crate::session::get_app_dir().and_then(|dir| load_global_mcp_servers(&dir));
    let per_profile = crate::session::get_profile_dir_path(profile.unwrap_or_default())
        .and_then(|dir| load_standard_mcp_servers(&dir.join("mcp.json")));

    resolve(vec![
        McpLayer {
            provenance: McpProvenance::AgentNative {
                agent: agent_key.to_string(),
            },
            servers: or_warn(native, &format!("native ({agent_key})")),
        },
        McpLayer {
            provenance: McpProvenance::Global,
            servers: or_warn(global, "global"),
        },
        McpLayer {
            provenance: McpProvenance::Profile {
                name: profile.unwrap_or_default().to_string(),
            },
            servers: or_warn(per_profile, "per-profile"),
        },
        McpLayer {
            provenance: McpProvenance::ProjectLocal,
            servers: or_warn(trusted_project_servers(cwd), "project-local"),
        },
    ])
}

fn trusted_project_servers(cwd: &Path) -> Result<Vec<ProjectMcpServer>> {
    use crate::session::config::repo_config::{is_repo_trusted, repo_config_source_path};
    let source = repo_config_source_path(cwd);
    let servers = super::project_mcp::load_project_mcp_servers(&source)?;
    if servers.is_empty() {
        return Ok(servers);
    }
    let hash = super::project_mcp::fingerprint(&servers);
    if is_repo_trusted(&source, None, Some(&hash))? {
        return Ok(servers);
    }
    warn!(
        target: "acp.mcp",
        repo = %source.display(),
        count = servers.len(),
        "skipping project-local MCP servers: repo not trusted at this .mcp.json fingerprint; review and approve by creating a session for this repo in the TUI or CLI"
    );
    Ok(Vec::new())
}

pub struct McpSurfaceView {
    /// Same set [`resolve_effective`] forwards.
    pub effective: Vec<ResolvedMcpServer>,
    /// Not forwarded until the user keeps (promotes to global) or drops them.
    pub kept_on_removal: Vec<ResolvedMcpServer>,
    pub conflicts: Vec<super::mcp_state::McpConflict>,
    /// Drift detection paused on a malformed native entry; the two lists above it are empty.
    pub drift_paused: bool,
}

/// The management-surface view: the effective set plus drift reconciled
/// against (and recorded in) the drift store.
pub fn resolve_surface(agent: &str, profile: Option<&str>, cwd: &Path) -> McpSurfaceView {
    let session_env = session_env_for_discovery(profile);
    let effective = resolve_effective(agent, profile, cwd, &session_env);
    let reconcile = load_native_mcp_servers_checked_from_home(agent, profile, &session_env)
        .and_then(|read| super::mcp_state::reconcile_agent(agent, &read))
        .unwrap_or_else(|e| {
            warn!(target: "acp.mcp", agent = %agent, error = %e, "failed to reconcile MCP drift store");
            Default::default()
        });
    let kept_on_removal = reconcile
        .removed
        .into_iter()
        .map(|def| ResolvedMcpServer {
            def,
            provenance: McpProvenance::KeptOnRemoval {
                agent: agent.to_string(),
            },
            shadowed: Vec::new(),
        })
        .collect();
    McpSurfaceView {
        effective,
        kept_on_removal,
        conflicts: reconcile.conflicts,
        drift_paused: reconcile.paused,
    }
}

pub fn load_global_mcp_servers(app_dir: &Path) -> Result<Vec<ProjectMcpServer>> {
    load_standard_mcp_servers(&app_dir.join("mcp.json"))
}

/// Native config readers AoE knows, by home-relative path. AoE never writes these.
enum NativeMcpConfig {
    /// Standard `mcpServers` JSON (Claude).
    StandardJson(&'static str),
    /// Transport chosen by which of `command` / `httpUrl` / `url` is present.
    GeminiJson(&'static str),
    /// `[mcp_servers.<name>]` tables with an optional boolean `enabled`.
    CodexToml(&'static str),
}

fn native_config_for(agent_key: &str) -> Option<NativeMcpConfig> {
    match agent_key {
        "claude" | "claude-code" => Some(NativeMcpConfig::StandardJson(".claude.json")),
        "gemini" => Some(NativeMcpConfig::GeminiJson(".gemini/settings.json")),
        "codex" => Some(NativeMcpConfig::CodexToml(".codex/config.toml")),
        _ => None,
    }
}

/// Disabled definitions stay in `servers` so drift sees them as present.
/// `skipped` names malformed entries, which pause drift detection.
#[derive(Default)]
pub struct NativeRead {
    pub servers: Vec<ProjectMcpServer>,
    pub disabled_names: BTreeSet<String>,
    pub skipped: Vec<String>,
}

impl NativeRead {
    fn into_enabled(self) -> Vec<ProjectMcpServer> {
        let disabled = self.disabled_names;
        self.servers
            .into_iter()
            .filter(|server| !disabled.contains(&server.name))
            .collect()
    }
}

/// Reads an agent's native config. A missing file or unknown agent yields nothing;
/// an unparseable file is an error; malformed entries are skipped. Only the Claude
/// reader honors `config_dir`.
fn read_native(agent_key: &str, home: &Path, config_dir: Option<&Path>) -> Result<NativeRead> {
    match native_config_for(agent_key) {
        None => Ok(NativeRead::default()),
        Some(NativeMcpConfig::StandardJson(rel)) => read_json::<StandardMcpFile, _>(
            &config_dir.unwrap_or(home).join(rel),
            |f| f.mcp_servers,
            convert_standard,
        ),
        Some(NativeMcpConfig::GeminiJson(rel)) => {
            read_json::<GeminiConfigFile, _>(&home.join(rel), |f| f.mcp_servers, convert_gemini)
        }
        Some(NativeMcpConfig::CodexToml(rel)) => read_codex_toml(&home.join(rel)),
    }
}

/// Enabled native servers under an injected `home`.
pub fn load_native_mcp_servers(agent_key: &str, home: &Path) -> Result<Vec<ProjectMcpServer>> {
    read_native(agent_key, home, None).map(NativeRead::into_enabled)
}

pub fn load_native_mcp_servers_checked_from_home(
    agent_key: &str,
    profile: Option<&str>,
    session_env: &[(String, String)],
) -> Result<NativeRead> {
    let home = dirs::home_dir().context("could not resolve home dir for native MCP config")?;
    let explicit = resolved_profile_config(profile)
        .session
        .agent_config_dir_for(agent_key, &home);
    let reads_env = matches!(
        native_config_for(agent_key),
        Some(NativeMcpConfig::StandardJson(_))
    );
    let config_dir = pick_native_config_dir(
        explicit,
        reads_env,
        session_env,
        std::env::var_os("CLAUDE_CONFIG_DIR"),
    );
    read_native(agent_key, &home, config_dir.as_deref())
}

/// The directory the launched agent reads its native config from: the exact-tool
/// `agent_config_dir` setting, then `CLAUDE_CONFIG_DIR` from the session env (last
/// entry wins), then from the daemon env, else the home default (`None`).
fn pick_native_config_dir(
    explicit: Option<PathBuf>,
    reads_claude_config_dir: bool,
    session_env: &[(String, String)],
    daemon_env: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if explicit.is_some() || !reads_claude_config_dir {
        return explicit;
    }
    // A relative path would resolve differently for the daemon and the agent.
    let usable = |dir: PathBuf| dir.is_absolute().then_some(dir);
    session_env
        .iter()
        .rev()
        .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
        .and_then(|(_, value)| usable(PathBuf::from(value)))
        .or_else(|| daemon_env.map(PathBuf::from).and_then(usable))
}

/// Converts entries individually so one bad entry in a config AoE does not own
/// does not discard the others.
fn convert_tolerant<R>(
    servers: BTreeMap<String, R>,
    path: &Path,
    convert: impl Fn(String, R) -> Result<ProjectMcpServer>,
) -> NativeRead {
    let mut read = NativeRead::default();
    for (name, raw) in servers {
        match convert(name.clone(), raw) {
            Ok(server) => read.servers.push(server),
            Err(e) => {
                warn!(
                    target: "acp.mcp",
                    server = %name,
                    path = %path.display(),
                    error = %e,
                    "skipping malformed MCP server in native config"
                );
                read.skipped.push(name);
            }
        }
    }
    read
}

/// Streams the file so a large `~/.claude.json` is parsed without materializing history.
fn read_json<F: serde::de::DeserializeOwned, R>(
    path: &Path,
    servers: impl FnOnce(F) -> BTreeMap<String, R>,
    convert: impl Fn(String, R) -> Result<ProjectMcpServer>,
) -> Result<NativeRead> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NativeRead::default()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("opening native MCP config at {}", path.display()))
        }
    };
    let parsed: F = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(servers(parsed), path, convert))
}

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

fn convert_gemini(name: String, raw: GeminiRawServer) -> Result<ProjectMcpServer> {
    let transport = match (raw.command, raw.http_url, raw.url) {
        (Some(command), None, None) => ProjectMcpTransport::Stdio {
            command,
            args: raw.args,
            env: raw.env,
        },
        (None, Some(url), None) => ProjectMcpTransport::Http {
            url,
            headers: raw.headers,
        },
        (None, None, Some(url)) => ProjectMcpTransport::Sse {
            url,
            headers: raw.headers,
        },
        (None, None, None) => {
            bail!("MCP server \"{name}\" has none of \"command\", \"httpUrl\", \"url\"")
        }
        _ => bail!("MCP server \"{name}\" sets more than one of \"command\", \"httpUrl\", \"url\""),
    };
    Ok(ProjectMcpServer { name, transport })
}

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
    enabled: Option<toml::Value>,
}

fn convert_codex(name: String, raw: CodexRawServer) -> Result<ProjectMcpServer> {
    if !matches!(raw.enabled, None | Some(toml::Value::Boolean(_))) {
        bail!("MCP server \"{name}\" has non-boolean \"enabled\"");
    }
    let transport = match (raw.command, raw.url) {
        (Some(command), None) => ProjectMcpTransport::Stdio {
            command,
            args: raw.args,
            env: raw.env,
        },
        (None, Some(url)) => ProjectMcpTransport::Http {
            url,
            headers: raw.headers,
        },
        (None, None) => bail!("MCP server \"{name}\" has neither \"command\" nor \"url\""),
        (Some(_), Some(_)) => bail!("MCP server \"{name}\" sets both \"command\" and \"url\""),
    };
    Ok(ProjectMcpServer { name, transport })
}

fn read_codex_toml(path: &Path) -> Result<NativeRead> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NativeRead::default()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading native MCP config at {}", path.display()))
        }
    };
    let parsed: CodexConfigFile = toml::from_str(&text)
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    let disabled_names = parsed
        .mcp_servers
        .iter()
        .filter(|(_, raw)| matches!(raw.enabled, Some(toml::Value::Boolean(false))))
        .map(|(name, _)| name.clone())
        .collect();
    let mut read = convert_tolerant(parsed.mcp_servers, path, convert_codex);
    read.disabled_names = disabled_names;
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(servers: &[ProjectMcpServer]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    fn resolved_names(servers: &[ResolvedMcpServer]) -> Vec<&str> {
        servers.iter().map(|s| s.def.name.as_str()).collect()
    }

    fn write(home: &Path, rel: &str, contents: &str) {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn stdio_command(server: &ProjectMcpServer) -> &str {
        match &server.transport {
            ProjectMcpTransport::Stdio { command, .. } => command,
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    fn layer(provenance: McpProvenance, json: &str) -> McpLayer {
        McpLayer {
            provenance,
            servers: super::super::project_mcp::parse_standard_mcp_servers(json).unwrap(),
        }
    }

    fn native_claude() -> McpProvenance {
        McpProvenance::AgentNative {
            agent: "claude".into(),
        }
    }

    /// A temp HOME with `CLAUDE_CONFIG_DIR` cleared so a developer's own export is not read.
    fn set_tmp_home() -> (tempfile::TempDir, crate::session::test_support::EnvGuard) {
        let dir = tempfile::tempdir().unwrap();
        let guard = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"])
            .and_set("HOME", dir.path())
            .and_set("XDG_CONFIG_HOME", dir.path().join(".config"));
        (dir, guard)
    }

    #[test]
    #[serial_test::serial]
    fn discovery_reads_claude_config_dir_from_the_profile_environment() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            "environment = [\"CLAUDE_CONFIG_DIR=/from-profile\"]\n",
        )
        .unwrap();
        let env = session_env_for_discovery(None);
        assert!(
            env.iter()
                .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == "/from-profile"),
            "{env:?}"
        );
    }

    #[test]
    fn native_config_dir_precedence_follows_the_session_environment() {
        let p = |s: &str| Some(PathBuf::from(s));
        let daemon = || Some(std::ffi::OsString::from("/daemon"));
        let cases: [(&str, Option<PathBuf>, bool, &[(&str, &str)], _, _); 8] = [
            (
                "explicit wins",
                p("/explicit"),
                true,
                &[("CLAUDE_CONFIG_DIR", "/profile")],
                daemon(),
                p("/explicit"),
            ),
            (
                "profile beats daemon",
                None,
                true,
                &[("CLAUDE_CONFIG_DIR", "/profile")],
                daemon(),
                p("/profile"),
            ),
            (
                "relative ignored",
                None,
                true,
                &[("CLAUDE_CONFIG_DIR", "relative/dir")],
                daemon(),
                p("/daemon"),
            ),
            (
                "last entry wins",
                None,
                true,
                &[
                    ("CLAUDE_CONFIG_DIR", "/profile"),
                    ("CLAUDE_CONFIG_DIR", "/minted"),
                ],
                daemon(),
                p("/minted"),
            ),
            (
                "daemon fallback",
                None,
                true,
                &[("OTHER", "x")],
                daemon(),
                p("/daemon"),
            ),
            ("home default", None, true, &[], None, None),
            (
                "empty falls through",
                None,
                true,
                &[("CLAUDE_CONFIG_DIR", "")],
                daemon(),
                p("/daemon"),
            ),
            (
                "non-claude ignores env",
                None,
                false,
                &[("CLAUDE_CONFIG_DIR", "/profile")],
                daemon(),
                None,
            ),
        ];
        for (label, explicit, reads, env, daemon_env, expected) in cases {
            let env: Vec<_> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            assert_eq!(
                pick_native_config_dir(explicit, reads, &env, daemon_env),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn resolve_merges_by_precedence_with_shadow_chain() {
        assert!(resolve(vec![]).is_empty());
        let profile = McpProvenance::Profile {
            name: "rust".into(),
        };
        let merged = resolve(vec![
            layer(
                native_claude(),
                r#"{ "mcpServers": { "fs": { "command": "n" }, "zebra": { "command": "z" } } }"#,
            ),
            layer(
                McpProvenance::Global,
                r#"{ "mcpServers": { "fs": { "command": "g" }, "alpha": { "command": "a" } } }"#,
            ),
            layer(
                profile.clone(),
                r#"{ "mcpServers": { "fs": { "command": "p" } } }"#,
            ),
        ]);
        assert_eq!(resolved_names(&merged), vec!["alpha", "fs", "zebra"]);
        let fs = &merged[1];
        assert_eq!(stdio_command(&fs.def), "p");
        assert_eq!(fs.provenance, profile);
        assert_eq!(fs.shadowed, vec![native_claude(), McpProvenance::Global]);
        assert_eq!(
            fs.redacted().shadowed,
            vec!["agent-native:claude", "global"]
        );
        assert_eq!(merged[1].redacted().provenance, "profile:rust");
    }

    #[test]
    fn redacted_view_and_summary_keep_names_drop_secret_values() {
        let merged = resolve(vec![layer(
            McpProvenance::Global,
            r#"{ "mcpServers": {
                "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "SUPER_SECRET_DO_NOT_LEAK" } },
                "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer HEADER_SECRET_DO_NOT_LEAK" } }
            } }"#,
        )]);
        assert_eq!(summarize(&merged), "fs(stdio), remote(http)");

        let stdio = merged[0].redacted();
        assert_eq!(stdio.transport, "stdio");
        assert_eq!(stdio.command.as_deref(), Some("mcp-fs"));
        assert_eq!(stdio.args, vec!["--root", "."]);
        assert_eq!(stdio.env_names, vec!["TOKEN"]);
        assert_eq!(stdio.provenance, "global");
        let remote = merged[1].redacted();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("https://e/mcp"));
        assert_eq!(remote.header_names, vec!["Authorization"]);

        let json = serde_json::to_string(&[stdio, remote]).unwrap();
        assert!(!json.contains("SECRET_DO_NOT_LEAK"), "{json}");
    }

    #[test]
    #[serial_test::serial]
    fn surface_keep_on_removal_then_keep_or_drop() {
        let (home, _env) = set_tmp_home();
        let cwd = home.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let claude_json = home.path().join(".claude.json");
        std::fs::write(
            &claude_json,
            r#"{ "mcpServers": { "fs": { "command": "c" }, "kept": { "command": "k" }, "dropped": { "command": "d" } } }"#,
        )
        .unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert_eq!(view.effective.len(), 3);
        assert!(view.kept_on_removal.is_empty() && view.conflicts.is_empty() && !view.drift_paused);

        std::fs::write(
            &claude_json,
            r#"{ "mcpServers": { "fs": { "command": "c" } } }"#,
        )
        .unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert_eq!(resolved_names(&view.effective), vec!["fs"]);
        assert_eq!(
            resolved_names(&view.kept_on_removal),
            vec!["dropped", "kept"]
        );
        assert_eq!(
            view.kept_on_removal[0].provenance.label(),
            "kept-on-removal:claude"
        );

        assert!(super::super::mcp_state::keep_removed("claude", "kept").unwrap());
        super::super::mcp_state::forget_native("claude", "dropped").unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert!(view.kept_on_removal.is_empty());
        assert_eq!(resolved_names(&view.effective), vec!["fs", "kept"]);
        assert_eq!(view.effective[1].provenance, McpProvenance::Global);
    }

    #[test]
    fn standard_layer_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_global_mcp_servers(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
        assert!(load_global_mcp_servers(dir.path()).is_err());
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "mcp-fs" } } }"#,
        )
        .unwrap();
        assert_eq!(
            names(&load_global_mcp_servers(dir.path()).unwrap()),
            vec!["fs"]
        );
    }

    #[test]
    fn native_missing_malformed_and_unknown() {
        let home = tempfile::tempdir().unwrap();
        for agent in ["claude", "gemini", "codex", "opencode"] {
            assert!(load_native_mcp_servers(agent, home.path())
                .unwrap()
                .is_empty());
        }
        write(home.path(), ".claude.json", "{ not json");
        write(home.path(), ".codex/config.toml", "this = = not toml");
        for agent in ["claude", "codex"] {
            assert!(
                load_native_mcp_servers(agent, home.path()).is_err(),
                "{agent}"
            );
        }
    }

    /// Only the Claude reader honors `config_dir`, and an injected home ignores
    /// an ambient `CLAUDE_CONFIG_DIR`.
    #[test]
    fn native_claude_reads_config_dir_over_injected_home() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let stray = tempfile::tempdir().unwrap();
        for (dir, name) in [
            (&home, "from-home"),
            (&config_dir, "from-config-dir"),
            (&stray, "from-env"),
        ] {
            write(
                dir.path(),
                ".claude.json",
                &format!(r#"{{ "mcpServers": {{ "{name}": {{ "command": "x" }} }} }}"#),
            );
        }
        let _guard = crate::session::test_support::EnvGuard::set(&[(
            "CLAUDE_CONFIG_DIR",
            stray.path().to_path_buf(),
        )]);
        let read = read_native("claude", home.path(), Some(config_dir.path())).unwrap();
        assert_eq!(names(&read.servers), vec!["from-config-dir"]);
        assert_eq!(
            names(&load_native_mcp_servers("claude", home.path()).unwrap()),
            vec!["from-home"]
        );
    }

    #[test]
    fn native_readers_convert_and_skip_bad_entries() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".claude.json",
            r#"{
                "projects": { "/some/path": { "lastSessionId": "abc" } },
                "mcpServers": {
                    "broken": { "args": ["--x"] },
                    "fs": { "command": "mcp-fs", "args": ["--root", "."] },
                    "remote": { "type": "http", "url": "https://e/mcp" }
                }
            }"#,
        );
        write(
            home.path(),
            ".gemini/settings.json",
            r#"{ "theme": "dark", "mcpServers": {
                "ambiguous": { "command": "c", "httpUrl": "https://e/mcp" },
                "httpish": { "httpUrl": "https://e/mcp", "headers": { "Authorization": "Bearer x" } },
                "local": { "command": "g", "args": ["--x"] },
                "sseish": { "url": "https://e/sse" }
            } }"#,
        );
        write(
            home.path(),
            ".codex/config.toml",
            r#"
model = "gpt-5"
[mcp_servers.bad]
command = "bad"
enabled = "sometimes"
[mcp_servers.fs]
command = "mcp-fs"
env = { TOKEN = "secret" }
[mcp_servers.remote]
url = "https://e/mcp"
[mcp_servers.off]
command = "off"
enabled = false
[mcp_servers.on]
command = "on"
enabled = true
"#,
        );

        for agent in ["claude", "claude-code"] {
            let read = read_native(agent, home.path(), None).unwrap();
            assert_eq!(names(&read.servers), vec!["fs", "remote"]);
            assert_eq!(read.skipped, vec!["broken"]);
        }

        let gemini = read_native("gemini", home.path(), None).unwrap();
        assert_eq!(gemini.skipped, vec!["ambiguous"]);
        let kinds: Vec<_> = gemini
            .servers
            .iter()
            .map(|s| (s.name.as_str(), s.kind()))
            .collect();
        assert_eq!(
            kinds,
            vec![("httpish", "http"), ("local", "stdio"), ("sseish", "sse")]
        );

        let codex = read_native("codex", home.path(), None).unwrap();
        assert_eq!(names(&codex.servers), vec!["fs", "off", "on", "remote"]);
        assert_eq!(codex.skipped, vec!["bad"]);
        assert_eq!(codex.disabled_names, BTreeSet::from(["off".to_string()]));
        assert_eq!(codex.servers[3].kind(), "http");
        let enabled = load_native_mcp_servers("codex", home.path()).unwrap();
        assert_eq!(names(&enabled), vec!["fs", "on", "remote"]);
    }

    #[test]
    #[serial_test::serial]
    fn codex_enabled_toggle_preserves_drift_and_restores_effective_server() {
        let (home, _env) = set_tmp_home();
        let cwd = home.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        for enabled in [true, false, true] {
            write(
                home.path(),
                ".codex/config.toml",
                &format!("[mcp_servers.toggle]\ncommand = \"toggle\"\nenabled = {enabled}\n"),
            );
            let view = resolve_surface("codex", None, &cwd);
            let expected: &[&str] = if enabled { &["toggle"] } else { &[] };
            assert_eq!(resolved_names(&view.effective), expected);
            assert!(view.kept_on_removal.is_empty());
            assert!(view.conflicts.is_empty(), "re-enabling must not conflict");
            assert!(!view.drift_paused);
        }
    }
}
