//! Always-compiled, ACP-free model of the merged MCP server set (#1996).
//!
//! AoE assembles an effective MCP server set from up to four layers
//! (`agent-native` -> `global` -> `per-profile` -> `project-local`, higher wins
//! per server name). The serve-gated `acp::mcp_config` used to own both the
//! parsing and the merge, but it returned ACP `McpServer` wire types and dropped
//! the layer label on merge, so the unified management surface had nowhere to
//! read the merged set, its provenance, or its shadowing.
//!
//! This module is the single resolver for both jobs. It lives outside the serve
//! gate (like [`super::project_mcp`]) so the TUI panel, the CLI, and the
//! always-compiled drift store can read the model without depending on the ACP
//! schema. Serve-side forwarding consumes the SAME resolver and converts only
//! the winning set to ACP just before sending, so what the user sees and what
//! the agent receives can never diverge.
//!
//! Secret values (env values, header values) are kept in memory because
//! forwarding and the drift store need them, but every display path goes through
//! [`ProjectMcpServer::redacted_summary`], so names reach a screen, a log, or
//! CLI output and values never do.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

/// Where a resolved server came from, carrying the dynamic name where the layer
/// needs one (the agent key, the profile name). `label` renders the provenance
/// string the management surfaces display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpProvenance {
    /// The active agent's own config (`~/.claude.json`, `~/.gemini/...`, ...).
    AgentNative { agent: String },
    /// The global `<app_dir>/mcp.json`.
    Global,
    /// A per-profile `<profile_dir>/mcp.json`.
    Profile { name: String },
    /// The repo's trusted `cwd/.mcp.json`.
    ProjectLocal,
    /// A server AoE last saw in the named agent's native config that has since
    /// disappeared from it. Kept in AoE's view (keep-on-removal, feature D) and
    /// not forwarded until the user keeps it (promoting it to `global`) or drops
    /// it. `agent` is the native config it last lived in.
    KeptOnRemoval { agent: String },
}

impl McpProvenance {
    /// The provenance string shown on every surface, e.g. `agent-native:claude`,
    /// `profile:rust`, `global`, `project-local`.
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

/// One layer of the precedence stack, lowest first. The `provenance` is owned so
/// the per-profile layer can carry the dynamic profile name (the old
/// `&'static str` label could not).
pub struct McpLayer {
    pub provenance: McpProvenance,
    pub servers: Vec<ProjectMcpServer>,
}

/// A server in the merged effective set: its winning definition, the layer it
/// won from, and every lower layer it shadowed (in precedence order, lowest
/// first). `shadowed` is what lets the surface explain "this `fs` came from
/// per-profile and overrode the agent-native one".
#[derive(Debug, Clone)]
pub struct ResolvedMcpServer {
    pub def: ProjectMcpServer,
    pub provenance: McpProvenance,
    pub shadowed: Vec<McpProvenance>,
}

impl ResolvedMcpServer {
    /// The redaction-safe, serializable view shared by every surface (web JSON,
    /// CLI `--json`, TUI rows). This is the single chokepoint: the connection
    /// detail the user needs to identify a server (command/args/url) is kept, but
    /// env and header VALUES never leave [`ResolvedMcpServer`]; only their names
    /// do. The unredacted [`def`](Self::def) is reachable only for forwarding and
    /// the drift store, never for display.
    pub fn redacted(&self) -> RedactedMcpServer {
        let (transport, command, args, url, env_names, header_names) = match &self.def.transport {
            ProjectMcpTransport::Stdio { command, args, env } => (
                "stdio",
                Some(command.clone()),
                args.clone(),
                None,
                env.keys().cloned().collect(),
                Vec::new(),
            ),
            ProjectMcpTransport::Http { url, headers } => (
                "http",
                None,
                Vec::new(),
                Some(url.clone()),
                Vec::new(),
                headers.keys().cloned().collect(),
            ),
            ProjectMcpTransport::Sse { url, headers } => (
                "sse",
                None,
                Vec::new(),
                Some(url.clone()),
                Vec::new(),
                headers.keys().cloned().collect(),
            ),
        };
        RedactedMcpServer {
            name: self.def.name.clone(),
            transport,
            command,
            args,
            url,
            env_names,
            header_names,
            provenance: self.provenance.label(),
            shadowed: self.shadowed.iter().map(McpProvenance::label).collect(),
        }
    }
}

/// Redaction-safe view of a resolved server for all surfaces. `command`, `args`,
/// and `url` identify the server and are not secret; env and header VALUES are
/// secret and are reduced to their NAMES (`env_names` / `header_names`). Built
/// only via [`ResolvedMcpServer::redacted`].
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

/// Merge layers by precedence. `layers` are ordered lowest first; a server in a
/// later layer overrides one of the same name in an earlier layer (per server,
/// not whole layer). The shadowed provenances accumulate in precedence order so
/// the surface can render the full override chain. Output is name-sorted (the
/// `BTreeMap` key order) so every surface lists servers deterministically.
pub fn resolve(layers: Vec<McpLayer>) -> Vec<ResolvedMcpServer> {
    let mut by_name: BTreeMap<String, ResolvedMcpServer> = BTreeMap::new();
    for layer in layers {
        for server in layer.servers {
            match by_name.get_mut(&server.name) {
                Some(existing) => {
                    let shadowed =
                        std::mem::replace(&mut existing.provenance, layer.provenance.clone());
                    existing.shadowed.push(shadowed);
                    existing.def = server;
                }
                None => {
                    by_name.insert(
                        server.name.clone(),
                        ResolvedMcpServer {
                            def: server,
                            provenance: layer.provenance.clone(),
                            shadowed: Vec::new(),
                        },
                    );
                }
            }
        }
    }
    by_name.into_values().collect()
}

/// Names + transports of a resolved set for logging. Reuses the redaction-safe
/// `kind()` so no secret value can reach the log sink.
pub fn summarize(servers: &[ResolvedMcpServer]) -> String {
    servers
        .iter()
        .map(|s| format!("{}({})", s.def.name, s.def.kind()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve the effective MCP server set for a session context, applying the
/// full precedence stack: agent-native -> global -> per-profile ->
/// project-local (trust-gated), higher wins per name. This is the single source
/// of truth for BOTH forwarding (the supervisor converts the winning set to ACP)
/// and the management surfaces (#1996), so what the user sees equals what the
/// agent receives.
///
/// Each layer is isolated: a missing, unreadable, or malformed source warns and
/// contributes nothing rather than aborting, so a single broken file never
/// blocks a spawn. `profile` is the session's source profile; an empty/`None`
/// value resolves to the default. `cwd` is the session working directory, from
/// which the project-local repo (and its `.mcp.json`) is resolved. The
/// project-local layer is forwarded ONLY when the repo is trusted at the file's
/// current fingerprint; an untrusted (or changed) file is skipped and logged,
/// exactly like the create-time trust gate refuses untrusted hooks.
pub fn resolve_effective(
    agent_key: &str,
    profile: Option<&str>,
    cwd: &Path,
) -> Vec<ResolvedMcpServer> {
    let native = load_native_mcp_servers_from_home(agent_key).unwrap_or_else(|e| {
        warn!(
            target: "acp.mcp",
            agent = %agent_key,
            error = %e,
            "failed to load native MCP config; contributing none from it"
        );
        Vec::new()
    });

    let global = match super::get_app_dir() {
        Ok(app_dir) => load_global_mcp_servers(&app_dir).unwrap_or_else(|e| {
            warn!(target: "acp.mcp", error = %e, "failed to load global MCP config; contributing none from it");
            Vec::new()
        }),
        Err(e) => {
            warn!(target: "acp.mcp", error = %e, "could not resolve app dir for MCP config; contributing none from it");
            Vec::new()
        }
    };

    let per_profile = match super::get_profile_dir_path(profile.unwrap_or_default()) {
        Ok(profile_dir) => load_profile_mcp_servers(&profile_dir).unwrap_or_else(|e| {
            warn!(target: "acp.mcp", error = %e, "failed to load per-profile MCP config; contributing none from it");
            Vec::new()
        }),
        Err(e) => {
            warn!(target: "acp.mcp", error = %e, "could not resolve profile dir for MCP config; contributing none from it");
            Vec::new()
        }
    };

    let source = super::repo_config::repo_config_source_path(cwd);
    let project_local = match super::project_mcp::load_project_mcp_servers(&source) {
        Ok(servers) if servers.is_empty() => Vec::new(),
        Ok(servers) => {
            let hash = super::project_mcp::fingerprint(&servers);
            match super::repo_config::is_repo_trusted(&source, None, Some(&hash)) {
                Ok(true) => servers,
                Ok(false) => {
                    warn!(
                        target: "acp.mcp",
                        repo = %source.display(),
                        count = servers.len(),
                        "skipping project-local MCP servers: repo not trusted at this .mcp.json fingerprint; review and approve by creating a session for this repo in the TUI or CLI"
                    );
                    Vec::new()
                }
                Err(e) => {
                    warn!(target: "acp.mcp", repo = %source.display(), error = %e, "could not check project-local MCP trust; contributing none from it");
                    Vec::new()
                }
            }
        }
        Err(e) => {
            warn!(target: "acp.mcp", repo = %source.display(), error = %e, "failed to load project-local MCP config; contributing none from it");
            Vec::new()
        }
    };

    resolve(vec![
        McpLayer {
            provenance: McpProvenance::AgentNative {
                agent: agent_key.to_string(),
            },
            servers: native,
        },
        McpLayer {
            provenance: McpProvenance::Global,
            servers: global,
        },
        McpLayer {
            provenance: McpProvenance::Profile {
                name: profile.unwrap_or_default().to_string(),
            },
            servers: per_profile,
        },
        McpLayer {
            provenance: McpProvenance::ProjectLocal,
            servers: project_local,
        },
    ])
}

/// The full management-surface view for a session context (#1996): the
/// effective forwarded set, plus servers kept-on-removal, plus the conflicts and
/// drift-paused state the surfaces render. Built by [`resolve_surface`].
pub struct McpSurfaceView {
    /// The merged, trust-gated set that actually forwards to the agent, each
    /// tagged with its winning provenance and shadow chain. Same set
    /// [`resolve_effective`] feeds to forwarding.
    pub effective: Vec<ResolvedMcpServer>,
    /// Servers that vanished from the active agent's native config since AoE
    /// last saw them, kept in the view with `KeptOnRemoval` provenance and NOT
    /// forwarded until the user keeps (promote to global) or drops them.
    pub kept_on_removal: Vec<ResolvedMcpServer>,
    /// Servers whose native definition diverged from AoE's snapshot, awaiting a
    /// which-side-wins decision.
    pub conflicts: Vec<super::mcp_state::McpConflict>,
    /// True when drift detection was paused for the active agent because its
    /// native config has a malformed entry; conflicts and kept-on-removal are
    /// then empty and the surface should say so rather than imply no drift.
    pub drift_paused: bool,
}

/// Resolve the full management-surface view for the active agent and session
/// context. Combines the forwarded effective set ([`resolve_effective`]) with a
/// reconcile of the agent's native config against the drift store, so the
/// surface shows provenance, conflicts, and kept-on-removal in one shot. The
/// reconcile updates the snapshot (silent adoption of new servers); conflicts
/// and removals persist until the user resolves them.
pub fn resolve_surface(agent: &str, profile: Option<&str>, cwd: &Path) -> McpSurfaceView {
    let effective = resolve_effective(agent, profile, cwd);

    let reconcile = match load_native_mcp_servers_checked_from_home(agent) {
        Ok(read) => super::mcp_state::reconcile_agent(agent, &read).unwrap_or_else(|e| {
            warn!(target: "acp.mcp", agent = %agent, error = %e, "failed to reconcile MCP drift store");
            Default::default()
        }),
        Err(e) => {
            warn!(target: "acp.mcp", agent = %agent, error = %e, "failed to read native MCP config for drift");
            Default::default()
        }
    };

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

// ---------------------------------------------------------------------------
// Standard `.mcp.json` layers: global and per-profile.
// ---------------------------------------------------------------------------

/// Read and parse the global `<app_dir>/mcp.json`. A missing file yields an
/// empty list; a present-but-malformed file is an error the caller surfaces.
pub fn load_global_mcp_servers(app_dir: &Path) -> Result<Vec<ProjectMcpServer>> {
    super::project_mcp::load_standard_mcp_servers(&app_dir.join("mcp.json"))
}

/// Read and parse a profile's `<profile_dir>/mcp.json` (#1986). Same on-disk
/// shape and missing/malformed semantics as the global file.
pub fn load_profile_mcp_servers(profile_dir: &Path) -> Result<Vec<ProjectMcpServer>> {
    super::project_mcp::load_standard_mcp_servers(&profile_dir.join("mcp.json"))
}

// ---------------------------------------------------------------------------
// Agent-native layer: each agent's own MCP config, read live (no caching).
// ---------------------------------------------------------------------------

/// Where an agent keeps its own MCP server config, and in what shape. Adding an
/// agent is one match arm in [`native_config_for`] plus, if its format differs,
/// one converter. AoE only ever READS these files; it never writes them.
enum NativeMcpConfig {
    /// Standard `{ "mcpServers": { ... } }` JSON with the ecosystem `type`/`url`
    /// shape, at a home-relative path. Claude (`~/.claude.json`).
    StandardJson(&'static str),
    /// Gemini `settings.json`: entries discriminate transport by which key is
    /// present (`command` -> stdio, `httpUrl` -> http, `url` -> sse).
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

/// A native config read: the converted servers plus the names of any entries
/// that were skipped because they failed to convert. The skipped list gates
/// drift detection (#1996): a native file with a malformed entry must not make
/// the drift detector report that entry as "removed" (it is still there, just
/// unparseable), so callers pause drift for that agent when `skipped` is
/// non-empty.
pub struct NativeRead {
    pub servers: Vec<ProjectMcpServer>,
    pub skipped: Vec<String>,
}

impl NativeRead {
    fn empty() -> Self {
        NativeRead {
            servers: Vec::new(),
            skipped: Vec::new(),
        }
    }
}

/// Read the active agent's own MCP config (the file its CLI reads) and convert
/// it to neutral servers, reporting any skipped malformed entries. Live
/// read-through: called once per spawn, no caching, so edits are picked up on
/// the next session. Returns an empty read for an agent with no known native
/// reader and for a missing file. A present-but-unparseable file is an error the
/// caller downgrades to a warning, so a broken native file (which AoE does not
/// own) never blocks a spawn. Individual malformed server entries are skipped
/// with a warning and recorded in `skipped`.
pub fn load_native_mcp_servers_checked(agent_key: &str, home: &Path) -> Result<NativeRead> {
    let Some(config) = native_config_for(agent_key) else {
        return Ok(NativeRead::empty());
    };
    match config {
        NativeMcpConfig::StandardJson(rel) => read_standard_json(&home.join(rel)),
        NativeMcpConfig::GeminiJson(rel) => read_gemini_json(&home.join(rel)),
        NativeMcpConfig::CodexToml(rel) => read_codex_toml(&home.join(rel)),
    }
}

/// Like [`load_native_mcp_servers_checked`] but discards the skipped-entry list.
/// Used by forwarding, which only needs the parseable servers.
pub fn load_native_mcp_servers(agent_key: &str, home: &Path) -> Result<Vec<ProjectMcpServer>> {
    Ok(load_native_mcp_servers_checked(agent_key, home)?.servers)
}

/// Convenience wrapper that resolves the real home dir. Kept separate from
/// [`load_native_mcp_servers`] so tests can inject a temp home.
pub fn load_native_mcp_servers_from_home(agent_key: &str) -> Result<Vec<ProjectMcpServer>> {
    let home = dirs::home_dir().context("could not resolve home dir for native MCP config")?;
    load_native_mcp_servers(agent_key, &home)
}

/// Like [`load_native_mcp_servers_checked`] but resolves the real home dir. Used
/// by the management surface, which needs the skipped-entry list to gate drift.
pub fn load_native_mcp_servers_checked_from_home(agent_key: &str) -> Result<NativeRead> {
    let home = dirs::home_dir().context("could not resolve home dir for native MCP config")?;
    load_native_mcp_servers_checked(agent_key, &home)
}

/// Convert a map of raw server entries, skipping (with a warning) any entry that
/// fails to convert rather than failing the whole file. Native configs are owned
/// by other tools, so one bad entry must not discard the user's other servers.
/// Returns the converted servers and the names of the skipped entries.
fn convert_tolerant<R>(
    servers: BTreeMap<String, R>,
    path: &Path,
    convert: impl Fn(String, R) -> Result<ProjectMcpServer>,
) -> NativeRead {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for (name, raw) in servers {
        match convert(name.clone(), raw) {
            Ok(server) => out.push(server),
            Err(e) => {
                warn!(
                    target: "acp.mcp",
                    server = %name,
                    path = %path.display(),
                    error = %e,
                    "skipping malformed MCP server in native config"
                );
                skipped.push(name);
            }
        }
    }
    NativeRead {
        servers: out,
        skipped,
    }
}

/// Open a config file, mapping "not found" to `None` (the user does not use that
/// agent) and any other IO error to a context-tagged failure.
fn open_optional(path: &Path) -> Result<Option<std::fs::File>> {
    match std::fs::File::open(path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("opening native MCP config at {}", path.display()))
        }
    }
}

/// On-disk shape of a standard `mcpServers` entry (Claude `~/.claude.json` and
/// the AoE-owned `mcp.json` layers share this shape). Unknown keys are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StandardConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, StandardRawServer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StandardRawServer {
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

fn convert_standard(name: String, raw: StandardRawServer) -> Result<ProjectMcpServer> {
    let transport = match raw.transport.as_deref() {
        None | Some("stdio") => ProjectMcpTransport::Stdio {
            command: raw
                .command
                .with_context(|| format!("MCP server \"{name}\" is missing \"command\""))?,
            args: raw.args,
            env: raw.env,
        },
        Some("http") => ProjectMcpTransport::Http {
            url: raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?,
            headers: raw.headers,
        },
        Some("sse") => ProjectMcpTransport::Sse {
            url: raw
                .url
                .with_context(|| format!("MCP server \"{name}\" is missing \"url\""))?,
            headers: raw.headers,
        },
        Some(other) => bail!("MCP server \"{name}\" has unknown type \"{other}\""),
    };
    Ok(ProjectMcpServer { name, transport })
}

/// Read a standard `mcpServers` JSON config, streaming so a large host file
/// (e.g. `~/.claude.json`, which also holds project history) is parsed without
/// materializing the unrelated keys.
fn read_standard_json(path: &Path) -> Result<NativeRead> {
    let Some(file) = open_optional(path)? else {
        return Ok(NativeRead::empty());
    };
    let parsed: StandardConfigFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(parsed.mcp_servers, path, convert_standard))
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

fn convert_gemini(name: String, raw: GeminiRawServer) -> Result<ProjectMcpServer> {
    let transport = match (raw.command, raw.http_url, raw.url) {
        (Some(command), None, None) => ProjectMcpTransport::Stdio {
            command,
            args: raw.args,
            env: raw.env,
        },
        (None, Some(http_url), None) => ProjectMcpTransport::Http {
            url: http_url,
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

fn read_gemini_json(path: &Path) -> Result<NativeRead> {
    let Some(file) = open_optional(path)? else {
        return Ok(NativeRead::empty());
    };
    let parsed: GeminiConfigFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(parsed.mcp_servers, path, convert_gemini))
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

fn convert_codex(name: String, raw: CodexRawServer) -> Result<ProjectMcpServer> {
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

/// Codex config files are small (no embedded history), so a whole-string read is
/// fine; `toml` has no streaming reader.
fn read_codex_toml(path: &Path) -> Result<NativeRead> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NativeRead::empty()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading native MCP config at {}", path.display()))
        }
    };
    let parsed: CodexConfigFile = toml::from_str(&text)
        .with_context(|| format!("parsing native MCP config at {}", path.display()))?;
    Ok(convert_tolerant(parsed.mcp_servers, path, convert_codex))
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

    fn layer(provenance: McpProvenance, servers: Vec<ProjectMcpServer>) -> McpLayer {
        McpLayer {
            provenance,
            servers,
        }
    }

    fn standard(json: &str) -> Vec<ProjectMcpServer> {
        super::super::project_mcp::parse_standard_mcp_servers(json).unwrap()
    }

    #[test]
    fn provenance_labels() {
        assert_eq!(
            McpProvenance::AgentNative {
                agent: "claude".into()
            }
            .label(),
            "agent-native:claude"
        );
        assert_eq!(McpProvenance::Global.label(), "global");
        assert_eq!(
            McpProvenance::Profile {
                name: "rust".into()
            }
            .label(),
            "profile:rust"
        );
        assert_eq!(McpProvenance::ProjectLocal.label(), "project-local");
    }

    #[test]
    fn resolve_higher_layer_wins_and_records_shadow() {
        let merged = resolve(vec![
            layer(
                McpProvenance::AgentNative {
                    agent: "claude".into(),
                },
                standard(r#"{ "mcpServers": { "fs": { "command": "native" } } }"#),
            ),
            layer(
                McpProvenance::Global,
                standard(r#"{ "mcpServers": { "fs": { "command": "global" } } }"#),
            ),
        ]);
        assert_eq!(resolved_names(&merged), vec!["fs"]);
        assert_eq!(stdio_command(&merged[0].def), "global");
        assert_eq!(merged[0].provenance, McpProvenance::Global);
        assert_eq!(
            merged[0].shadowed,
            vec![McpProvenance::AgentNative {
                agent: "claude".into()
            }]
        );
    }

    #[test]
    fn resolve_records_full_shadow_chain_in_precedence_order() {
        let merged = resolve(vec![
            layer(
                McpProvenance::AgentNative {
                    agent: "claude".into(),
                },
                standard(r#"{ "mcpServers": { "fs": { "command": "n" } } }"#),
            ),
            layer(
                McpProvenance::Global,
                standard(r#"{ "mcpServers": { "fs": { "command": "g" } } }"#),
            ),
            layer(
                McpProvenance::Profile {
                    name: "rust".into(),
                },
                standard(r#"{ "mcpServers": { "fs": { "command": "p" } } }"#),
            ),
        ]);
        assert_eq!(stdio_command(&merged[0].def), "p");
        assert_eq!(
            merged[0].provenance,
            McpProvenance::Profile {
                name: "rust".into()
            }
        );
        assert_eq!(
            merged[0].shadowed,
            vec![
                McpProvenance::AgentNative {
                    agent: "claude".into()
                },
                McpProvenance::Global,
            ]
        );
    }

    #[test]
    fn resolve_unions_distinct_names_sorted() {
        let merged = resolve(vec![
            layer(
                McpProvenance::AgentNative {
                    agent: "claude".into(),
                },
                standard(
                    r#"{ "mcpServers": { "zebra": { "command": "z" }, "fs": { "command": "n" } } }"#,
                ),
            ),
            layer(
                McpProvenance::Global,
                standard(r#"{ "mcpServers": { "alpha": { "command": "a" } } }"#),
            ),
        ]);
        assert_eq!(resolved_names(&merged), vec!["alpha", "fs", "zebra"]);
    }

    #[test]
    fn resolve_empty_is_empty() {
        assert!(resolve(vec![]).is_empty());
        assert!(resolve(vec![layer(McpProvenance::Global, Vec::new())]).is_empty());
    }

    #[test]
    fn summarize_omits_secret_values() {
        let merged = resolve(vec![layer(
            McpProvenance::Global,
            standard(
                r#"{ "mcpServers": { "fs": { "command": "c", "env": { "TOKEN": "supersecret" } } } }"#,
            ),
        )]);
        let s = summarize(&merged);
        assert_eq!(s, "fs(stdio)");
        assert!(!s.contains("supersecret"));
    }

    #[test]
    fn redacted_view_keeps_names_drops_secret_values() {
        let merged = resolve(vec![
            layer(
                McpProvenance::AgentNative {
                    agent: "claude".into(),
                },
                standard(r#"{ "mcpServers": { "fs": { "command": "fs-old" } } }"#),
            ),
            layer(
                McpProvenance::Global,
                standard(
                    r#"{ "mcpServers": {
                        "fs": { "command": "mcp-fs", "args": ["--root", "."], "env": { "TOKEN": "SUPER_SECRET_DO_NOT_LEAK" } },
                        "remote": { "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer HEADER_SECRET_DO_NOT_LEAK" } }
                    } }"#,
                ),
            ),
        ]);

        let stdio = merged
            .iter()
            .find(|s| s.def.name == "fs")
            .unwrap()
            .redacted();
        assert_eq!(stdio.transport, "stdio");
        assert_eq!(stdio.command.as_deref(), Some("mcp-fs"));
        assert_eq!(stdio.args, vec!["--root", "."]);
        assert_eq!(stdio.env_names, vec!["TOKEN"]);
        assert_eq!(stdio.provenance, "global");
        assert_eq!(stdio.shadowed, vec!["agent-native:claude"]);

        let remote = merged
            .iter()
            .find(|s| s.def.name == "remote")
            .unwrap()
            .redacted();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("https://e/mcp"));
        assert_eq!(remote.header_names, vec!["Authorization"]);

        // The single chokepoint must never serialize a secret VALUE on any
        // surface (web JSON, CLI --json, TUI rows all go through this).
        let json = serde_json::to_string(&merged.iter().map(|s| s.redacted()).collect::<Vec<_>>())
            .unwrap();
        assert!(
            !json.contains("SUPER_SECRET_DO_NOT_LEAK"),
            "env value leaked: {json}"
        );
        assert!(
            !json.contains("HEADER_SECRET_DO_NOT_LEAK"),
            "header value leaked: {json}"
        );
        assert!(json.contains("TOKEN") && json.contains("Authorization"));
    }

    fn set_tmp_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by `#[serial]`; matches the existing pattern.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join(".config"));
        }
        dir
    }

    #[test]
    #[serial_test::serial]
    fn surface_shows_effective_then_keep_on_removal_then_promote() {
        let home = set_tmp_home();
        let cwd = home.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        // Native config (claude) with two servers; first open adopts both.
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{ "mcpServers": { "fs": { "command": "c" }, "gone": { "command": "g" } } }"#,
        )
        .unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert_eq!(view.effective.len(), 2);
        assert!(view.kept_on_removal.is_empty());
        assert!(view.conflicts.is_empty() && !view.drift_paused);

        // Drop "gone" from native: it must be kept-on-removal, not forwarded.
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{ "mcpServers": { "fs": { "command": "c" } } }"#,
        )
        .unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert_eq!(view.effective.len(), 1, "removed server no longer forwards");
        assert_eq!(view.kept_on_removal.len(), 1);
        let kept = &view.kept_on_removal[0];
        assert_eq!(kept.def.name, "gone");
        assert_eq!(kept.provenance.label(), "kept-on-removal:claude");

        // Keep it: promote to global mcp.json; it now forwards as `global` and is
        // no longer reported as kept-on-removal.
        assert!(super::super::mcp_state::keep_removed("claude", &kept.def.name).unwrap());
        let view = resolve_surface("claude", None, &cwd);
        assert!(
            view.kept_on_removal.is_empty(),
            "kept server no longer flagged"
        );
        let promoted = view
            .effective
            .iter()
            .find(|s| s.def.name == "gone")
            .expect("kept server now forwards");
        assert_eq!(promoted.provenance, McpProvenance::Global);
        let _ = home;
    }

    #[test]
    #[serial_test::serial]
    fn surface_drop_removed_discards_without_promoting() {
        let home = set_tmp_home();
        let cwd = home.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{ "mcpServers": { "gone": { "command": "g" } } }"#,
        )
        .unwrap();
        resolve_surface("claude", None, &cwd);
        std::fs::write(home.path().join(".claude.json"), r#"{ "mcpServers": {} }"#).unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert_eq!(view.kept_on_removal.len(), 1);

        super::super::mcp_state::forget_native("claude", "gone").unwrap();
        let view = resolve_surface("claude", None, &cwd);
        assert!(
            view.kept_on_removal.is_empty(),
            "dropped server is gone entirely"
        );
        assert!(view.effective.is_empty(), "drop must not promote to global");
        let _ = home;
    }

    #[test]
    fn global_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_global_mcp_servers(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn global_loads_and_parses_file() {
        let dir = tempfile::tempdir().unwrap();
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
    fn global_malformed_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
        assert!(load_global_mcp_servers(dir.path()).is_err());
    }

    #[test]
    fn profile_loads_and_parses_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "fs": { "command": "profile-fs" } } }"#,
        )
        .unwrap();
        assert_eq!(
            names(&load_profile_mcp_servers(dir.path()).unwrap()),
            vec!["fs"]
        );
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
        for agent in ["claude", "gemini", "codex"] {
            assert!(load_native_mcp_servers(agent, home.path())
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn native_claude_parses_and_ignores_history() {
        let home = tempfile::tempdir().unwrap();
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
        write(
            home.path(),
            ".claude.json",
            r#"{ "mcpServers": {
                "broken": { "args": ["--x"] },
                "ok": { "command": "good" }
            } }"#,
        );
        assert_eq!(
            names(&load_native_mcp_servers("claude", home.path()).unwrap()),
            vec!["ok"]
        );
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
        assert_eq!(names(&servers), vec!["httpish", "local", "sseish"]);
        assert!(matches!(
            servers[0].transport,
            ProjectMcpTransport::Http { .. }
        ));
        assert!(matches!(
            servers[1].transport,
            ProjectMcpTransport::Stdio { .. }
        ));
        assert!(matches!(
            servers[2].transport,
            ProjectMcpTransport::Sse { .. }
        ));
    }

    #[test]
    fn native_gemini_ambiguous_entry_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".gemini/settings.json",
            r#"{ "mcpServers": {
                "ambiguous": { "command": "c", "httpUrl": "https://e/mcp" },
                "ok": { "command": "good" }
            } }"#,
        );
        assert_eq!(
            names(&load_native_mcp_servers("gemini", home.path()).unwrap()),
            vec!["ok"]
        );
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
        assert!(matches!(
            servers[1].transport,
            ProjectMcpTransport::Http { .. }
        ));
        // Secret env value never appears in the redacted summary.
        assert!(!servers[0].redacted_summary().contains("secret"));
    }

    #[test]
    fn native_codex_malformed_file_is_error() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), ".codex/config.toml", "this = = not toml");
        assert!(load_native_mcp_servers("codex", home.path()).is_err());
    }
}
