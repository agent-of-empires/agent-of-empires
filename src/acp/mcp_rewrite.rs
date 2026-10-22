//! Plugin rewrite of the MCP servers forwarded to a session's agent.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aoe_plugin_api::is_valid_mcp_server_name;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::acp::acp_client::{known_container_workdir, AcpError, SandboxLayout};
use crate::plugin::mcp_serve::ServedServer;
use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};
use crate::session::{SandboxInfo, WorkspaceInfo};

pub const METHOD: &str = "mcp.rewrite";

pub struct McpRewriteContext {
    pub session_id: String,
    pub agent: String,
    /// Present exactly when the session is sandboxed.
    pub sandbox: Option<McpSandbox>,
    pub profile: Option<String>,
    pub cwd: PathBuf,
    /// The plugin-served entries injected into this session's set.
    pub plugin_served: Vec<PluginServed>,
}

/// A session entry for a server a plugin serves through the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginServed {
    pub plugin_id: String,
    pub server: ProjectMcpServer,
}

impl McpRewriteContext {
    pub fn sandboxed(&self) -> bool {
        self.sandbox.is_some()
    }
}

/// The workspace volumes a sandboxed session's container mounts, so a plugin can map
/// container paths back to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpSandbox {
    pub container_workdir: Option<String>,
    pub mounts: Vec<McpSandboxMount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpSandboxMount {
    pub host: String,
    pub container: String,
    pub read_only: bool,
}

impl McpSandbox {
    /// Blocking: without a pinned workdir it asks the container runtime.
    pub fn resolve(
        info: &SandboxInfo,
        cwd: &Path,
        workspace: Option<&WorkspaceInfo>,
        session_id: &str,
    ) -> Self {
        Self::from_layout(
            SandboxLayout::resolve(info, cwd, workspace),
            || known_container_workdir(info),
            session_id,
        )
    }

    /// A layout that cannot be computed still marks the session as sandboxed, with no
    /// mounts and whatever workdir is known, rather than hiding the sandbox from plugins.
    fn from_layout(
        layout: Result<SandboxLayout, AcpError>,
        fallback_workdir: impl FnOnce() -> Option<String>,
        session_id: &str,
    ) -> Self {
        match layout {
            Ok(layout) => Self {
                container_workdir: Some(layout.container_workdir),
                mounts: layout
                    .volumes
                    .into_iter()
                    .map(|v| McpSandboxMount {
                        host: v.host_path,
                        container: v.container_path,
                        read_only: v.read_only,
                    })
                    .collect(),
            },
            Err(e) => {
                warn!(
                    target: "acp.mcp",
                    session = %session_id,
                    "cannot compute sandbox mounts for mcp.rewrite ({e}); sending none"
                );
                Self {
                    container_workdir: fallback_workdir(),
                    mounts: Vec::new(),
                }
            }
        }
    }
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
    /// Options so a present but empty field next to `serve` is told from an absent one.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    args: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    headers: Option<BTreeMap<String, String>>,
    /// The plugin that contributed the entry. Accepted back in a reply and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plugin: Option<String>,
    /// On an added entry: the replying plugin serves `name` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serve: Option<WireServe>,
}

/// A present field, where `null` stays invalid.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireServe {
    #[serde(default = "crate::plugin::mcp_serve::prefix_by_default")]
    prefix: bool,
}

impl WireServer {
    fn bare(name: String) -> Self {
        Self {
            name,
            transport: None,
            command: None,
            args: None,
            env: None,
            url: None,
            headers: None,
            plugin: None,
            serve: None,
        }
    }
}

impl From<&ProjectMcpServer> for WireServer {
    fn from(server: &ProjectMcpServer) -> Self {
        let empty = |transport: &str| WireServer {
            transport: Some(transport.to_string()),
            ..WireServer::bare(server.name.clone())
        };
        // Empty collections stay off the wire.
        let headers = |h: &BTreeMap<String, String>| (!h.is_empty()).then(|| h.clone());
        match &server.transport {
            ProjectMcpTransport::Stdio { command, args, env } => WireServer {
                command: Some(command.clone()),
                args: (!args.is_empty()).then(|| args.clone()),
                env: (!env.is_empty()).then(|| env.clone()),
                ..empty("stdio")
            },
            ProjectMcpTransport::Http { url, headers: h } => WireServer {
                url: Some(url.clone()),
                headers: headers(h),
                ..empty("http")
            },
            ProjectMcpTransport::Sse { url, headers: h } => WireServer {
                url: Some(url.clone()),
                headers: headers(h),
                ..empty("sse")
            },
        }
    }
}

impl WireServer {
    /// `serving_plugin` is the replying plugin when it may add serve entries.
    fn into_entry(mut self, serving_plugin: Option<&str>) -> Result<McpEntry, String> {
        let Some(serve) = self.serve.take() else {
            return self.into_server().map(McpEntry::Server);
        };
        if self.transport.is_some()
            || self.command.is_some()
            || self.args.is_some()
            || self.env.is_some()
            || self.url.is_some()
            || self.headers.is_some()
        {
            return Err("a serve entry must not set a transport".into());
        }
        let Some(plugin_id) = serving_plugin else {
            return Err("serve entries need both mcp.rewrite and mcp.serve".into());
        };
        if !is_valid_mcp_server_name(&self.name) {
            return Err("a serve entry has an invalid server name".into());
        }
        Ok(McpEntry::Serve {
            served: ServedServer::new(plugin_id, self.name, serve.prefix),
        })
    }

    fn into_server(self) -> Result<ProjectMcpServer, String> {
        if self.name.is_empty() {
            return Err("added server has an empty name".into());
        }
        let name = self.name;
        let args = self.args.unwrap_or_default();
        let env = self.env.unwrap_or_default();
        let headers = self.headers.unwrap_or_default();
        let transport = match self.transport.as_deref() {
            None | Some("stdio") => {
                if self.url.is_some() || !headers.is_empty() {
                    return Err(format!("stdio server {name:?} must not set url or headers"));
                }
                let command = self
                    .command
                    .ok_or_else(|| format!("stdio server {name:?} needs a command"))?;
                ProjectMcpTransport::Stdio { command, args, env }
            }
            Some(kind @ ("http" | "sse")) => {
                if self.command.is_some() || !args.is_empty() || !env.is_empty() {
                    return Err(format!(
                        "{kind} server {name:?} must not set command, args or env"
                    ));
                }
                let url = self
                    .url
                    .ok_or_else(|| format!("{kind} server {name:?} needs a url"))?;
                if kind == "http" {
                    ProjectMcpTransport::Http { url, headers }
                } else {
                    ProjectMcpTransport::Sse { url, headers }
                }
            }
            Some(_) => return Err(format!("server {name:?} has an unknown type")),
        };
        Ok(ProjectMcpServer { name, transport })
    }
}

/// The session description every plugin callback carries.
pub fn context_params(ctx: &McpRewriteContext) -> Value {
    let mut params = json!({
        "session_id": ctx.session_id,
        "agent": ctx.agent,
        "sandboxed": ctx.sandboxed(),
        "profile": ctx.profile,
        "cwd": ctx.cwd,
    });
    if let Some(sandbox) = &ctx.sandbox {
        params["sandbox"] = json!(sandbox);
    }
    params
}

/// One entry of the set a hook chain edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEntry {
    Server(ProjectMcpServer),
    /// A server a hook's own plugin serves. Its credential is minted only after the last
    /// hook, so no hook ever sees it.
    Serve {
        served: ServedServer,
    },
}

impl McpEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Server(server) => &server.name,
            Self::Serve { served } => &served.entry_key,
        }
    }
}

impl From<ProjectMcpServer> for McpEntry {
    fn from(server: ProjectMcpServer) -> Self {
        Self::Server(server)
    }
}

/// The plugin that contributed each plugin entry of a chain's set, by entry key. Built by
/// the host only, never from a reply.
pub type Contributors = BTreeMap<String, String>;

/// Before the first hook only the `mcp.session.servers` entries have a contributor.
pub fn initial_contributors(ctx: &McpRewriteContext, servers: &[ProjectMcpServer]) -> Contributors {
    servers
        .iter()
        .filter_map(|server| {
            ctx.plugin_served
                .iter()
                .find(|p| p.server == *server)
                .map(|p| (server.name.clone(), p.plugin_id.clone()))
        })
        .collect()
}

/// Applies `plugin_id`'s edit: an added entry names it as contributor, a dropped one loses
/// its contributor, untouched entries keep theirs.
pub fn apply_tracked_edit(
    entries: Vec<McpEntry>,
    contributors: &mut Contributors,
    plugin_id: &str,
    edit: McpEdit,
) -> Vec<McpEntry> {
    let added: Vec<String> = edit.add.iter().map(|e| e.name().to_string()).collect();
    let entries = apply_edit(entries, edit);
    contributors.retain(|key, _| entries.iter().any(|e| e.name() == key));
    for key in added {
        contributors.insert(key, plugin_id.to_string());
    }
    entries
}

/// Hooks see a serve entry as a marker without url or headers, and `plugin` on every entry
/// a plugin contributed.
pub fn request_params(
    ctx: &McpRewriteContext,
    entries: &[McpEntry],
    contributors: &Contributors,
) -> Value {
    let mut params = context_params(ctx);
    let wire: Vec<WireServer> = entries
        .iter()
        .map(|entry| {
            let plugin = contributors.get(entry.name()).cloned();
            match entry {
                McpEntry::Server(server) => WireServer {
                    plugin,
                    ..WireServer::from(server)
                },
                McpEntry::Serve { served } => WireServer {
                    plugin,
                    serve: Some(WireServe {
                        prefix: served.prefixed(),
                    }),
                    ..WireServer::bare(served.entry_key.clone())
                },
            }
        })
        .collect();
    params["servers"] = json!(wire);
    params
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
    pub add: Vec<McpEntry>,
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
/// `serving_plugin` is the replying plugin's id when it holds both `mcp.rewrite` and
/// `mcp.serve`; only then may it add serve entries.
pub fn parse_edit(value: Value, serving_plugin: Option<&str>) -> Result<McpEdit, String> {
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
        .map(|wire| wire.into_entry(serving_plugin))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpEdit { drop, add })
}

pub fn apply_edit(entries: Vec<McpEntry>, edit: McpEdit) -> Vec<McpEntry> {
    let mut out: Vec<McpEntry> = match edit.drop {
        DropSpec::All => Vec::new(),
        DropSpec::Names(dropped) => entries
            .into_iter()
            .filter(|e| !dropped.iter().any(|d| d == e.name()))
            .collect(),
        DropSpec::None => entries,
    };
    for added in edit.add {
        out.retain(|e| e.name() != added.name());
        out.push(added);
    }
    out
}

/// A rewritten set, with the serving plugin of each hook-served entry by entry key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewritten {
    pub servers: Vec<ProjectMcpServer>,
    pub served_by: BTreeMap<String, String>,
}

impl From<Vec<ProjectMcpServer>> for Rewritten {
    fn from(servers: Vec<ProjectMcpServer>) -> Self {
        Self {
            servers,
            served_by: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct RewriteError {
    pub plugin: String,
    pub cause: String,
}

impl std::fmt::Display for RewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.plugin, self.cause)
    }
}

pub type RewriteFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Rewritten, RewriteError>> + Send + 'a>>;

/// Rewrites the resolved set for one session. An implementation with no applicable hook
/// returns the servers unchanged.
pub trait McpRewriter: Send + Sync {
    fn rewrite<'a>(
        &'a self,
        ctx: &'a McpRewriteContext,
        servers: Vec<ProjectMcpServer>,
    ) -> RewriteFuture<'a>;

    /// The plugin-served servers this session gets.
    fn served_servers<'a>(
        &'a self,
        _ctx: &'a McpRewriteContext,
    ) -> BoxFuture<'a, Vec<ServedServer>> {
        Box::pin(std::future::ready(Vec::new()))
    }

    /// Tells the user a plugin's MCP server replaced one of their configured servers.
    fn report_serve_notice(&self, _plugin_id: &str, _message: &str) {}
}

static REWRITER: Mutex<Option<Arc<dyn McpRewriter>>> = Mutex::new(None);

fn current_rewriter() -> Option<Arc<dyn McpRewriter>> {
    REWRITER.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set_rewriter(rewriter: Option<Arc<dyn McpRewriter>>) {
    *REWRITER.lock().unwrap_or_else(|e| e.into_inner()) = rewriter;
}

pub fn report_serve_notice(plugin_id: &str, message: &str) {
    if let Some(rewriter) = current_rewriter() {
        rewriter.report_serve_notice(plugin_id, message);
    }
}

pub async fn served_servers(ctx: &McpRewriteContext) -> Vec<ServedServer> {
    match current_rewriter() {
        Some(rewriter) => rewriter.served_servers(ctx).await,
        None => Vec::new(),
    }
}

pub async fn rewrite_servers(ctx: &McpRewriteContext, servers: Vec<ProjectMcpServer>) -> Rewritten {
    rewrite_servers_with(current_rewriter(), ctx, servers).await
}

/// A failed hook forwards nothing to a sandboxed session and leaves other sessions unchanged.
pub async fn rewrite_servers_with(
    rewriter: Option<Arc<dyn McpRewriter>>,
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
) -> Rewritten {
    let Some(rewriter) = rewriter else {
        return servers.into();
    };
    let sandboxed = ctx.sandboxed();
    let original = (!sandboxed).then(|| servers.clone());
    match rewriter.rewrite(ctx, servers).await {
        Ok(rewritten) => rewritten,
        Err(e) => {
            warn!(
                target: "acp.mcp",
                session = %ctx.session_id,
                sandboxed,
                "mcp.rewrite hook failed ({e}); {}",
                if sandboxed { "forwarding no servers" } else { "keeping the original servers" }
            );
            original.unwrap_or_default().into()
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Installs a process-global rewriter and restores the empty slot on drop, panic included.
    /// Tests that hold one must be `#[serial_test::serial]`.
    pub(crate) struct SlotGuard;

    impl SlotGuard {
        pub(crate) fn install(rewriter: Arc<dyn McpRewriter>) -> Self {
            set_rewriter(Some(rewriter));
            Self
        }

        /// For code under test that installs the rewriter itself.
        pub(crate) fn clear_on_drop() -> Self {
            Self
        }
    }

    impl Drop for SlotGuard {
        fn drop(&mut self) {
            set_rewriter(None);
        }
    }

    pub(crate) fn slot_is_set() -> bool {
        current_rewriter().is_some()
    }

    /// Returns a fixed set and records each call's sandbox for assertions after the run.
    pub(crate) struct RecordingRewriter {
        pub(crate) sandboxes: Mutex<Vec<Option<McpSandbox>>>,
        result: Rewritten,
    }

    impl RecordingRewriter {
        pub(crate) fn returning(result: impl Into<Rewritten>) -> Arc<Self> {
            Arc::new(Self {
                sandboxes: Mutex::new(Vec::new()),
                result: result.into(),
            })
        }

        pub(crate) fn sandboxed_calls(&self) -> Vec<bool> {
            let sandboxes = self.sandboxes.lock().unwrap_or_else(|e| e.into_inner());
            sandboxes.iter().map(Option::is_some).collect()
        }
    }

    impl McpRewriter for RecordingRewriter {
        fn rewrite<'a>(
            &'a self,
            ctx: &'a McpRewriteContext,
            _servers: Vec<ProjectMcpServer>,
        ) -> RewriteFuture<'a> {
            self.sandboxes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ctx.sandbox.clone());
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }
    }

    /// A workspace at `dir` whose repos have worktrees `dir/<name>` and the given main repos.
    pub(crate) fn workspace_info(dir: &Path, repos: &[(&str, &Path)]) -> WorkspaceInfo {
        WorkspaceInfo {
            branch: "feature".into(),
            workspace_dir: dir.to_string_lossy().into_owned(),
            repos: repos
                .iter()
                .map(|(name, main)| crate::session::WorkspaceRepo {
                    name: (*name).into(),
                    source_path: main.to_string_lossy().into_owned(),
                    branch: "feature".into(),
                    worktree_path: dir.join(name).to_string_lossy().into_owned(),
                    main_repo_path: main.to_string_lossy().into_owned(),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                })
                .collect(),
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        }
    }

    /// A main repo that cannot exist, so a fixture needs no git repos: it shares only `/`
    /// with any workspace dir and mounts at `/workspace` plus its own path.
    pub(crate) const OUTSIDE_MAIN: &str = "/aoe-test-no-such-dir/alpha";

    pub(crate) fn outside_main_mount() -> McpSandboxMount {
        McpSandboxMount {
            host: OUTSIDE_MAIN.into(),
            container: format!("/workspace{OUTSIDE_MAIN}"),
            read_only: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;

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
            sandbox: Some(McpSandbox {
                container_workdir: Some("/workspace/foo".into()),
                mounts: vec![McpSandboxMount {
                    host: "/abs/host/foo".into(),
                    container: "/workspace/foo".into(),
                    read_only: false,
                }],
            }),
            profile: Some("work".into()),
            cwd: "/w".into(),
            plugin_served: Vec::new(),
        }
    }

    fn entries(servers: Vec<ProjectMcpServer>) -> Vec<McpEntry> {
        servers.into_iter().map(McpEntry::from).collect()
    }

    fn served(plugin_id: &str, name: &str, prefix: bool) -> McpEntry {
        McpEntry::Serve {
            served: ServedServer::new(plugin_id, name.into(), prefix),
        }
    }

    /// Plain request without contributor tracking beyond the start of the chain.
    fn initial_params(c: &McpRewriteContext, servers: &[ProjectMcpServer]) -> Value {
        request_params(
            c,
            &entries(servers.to_vec()),
            &initial_contributors(c, servers),
        )
    }

    #[test]
    fn initial_contributors_are_exactly_the_injected_plugin_entries() {
        let injected = http("acme_a__docs", "http://d/mcp/plugins/acme.a/docs", &[]);
        let mut c = ctx();
        c.plugin_served = vec![PluginServed {
            plugin_id: "acme.a".into(),
            server: injected.clone(),
        }];
        let replaced = http("acme_a__docs", "http://gateway/docs", &[]);
        let p = initial_params(&c, &[injected, stdio("ha", &[])]);
        assert_eq!(p["servers"][0]["plugin"], "acme.a");
        assert!(p["servers"][1].get("plugin").is_none(), "{p}");
        let p = initial_params(&c, &[replaced]);
        assert!(
            p["servers"][0].get("plugin").is_none(),
            "a different definition under the key is not the plugin entry: {p}"
        );
    }

    #[test]
    fn request_params_show_serve_entries_as_markers_without_url_or_headers() {
        let set = [
            served("acme.a", "docs", true),
            served("acme.a", "fs", false),
        ];
        let contributors: Contributors = set
            .iter()
            .map(|e| (e.name().to_string(), "acme.a".to_string()))
            .collect();
        let p = request_params(&ctx(), &set, &contributors);
        assert_eq!(
            p["servers"],
            json!([
                {"name": "acme_a__docs", "serve": {"prefix": true}, "plugin": "acme.a"},
                {"name": "fs", "serve": {"prefix": false}, "plugin": "acme.a"},
            ])
        );
    }

    #[test]
    fn parse_edit_accepts_an_echoed_plugin_marker() {
        let e = parse_edit(
            json!({
                "add": [{"name":"x","type":"http","url":"http://g","plugin":"acme.a"}]
            }),
            None,
        )
        .unwrap();
        assert_eq!(e.add, entries(vec![http("x", "http://g", &[])]));
    }

    #[test]
    fn parse_edit_accepts_serve_entries_from_a_serving_plugin() {
        let e = parse_edit(
            json!({
                "add": [
                    {"name": "docs", "serve": {}},
                    {"name": "git.v2", "serve": {"prefix": true}},
                    {"name": "fs", "serve": {"prefix": false}},
                ]
            }),
            Some("acme.a"),
        )
        .unwrap();
        assert_eq!(
            e.add,
            [
                served("acme.a", "docs", true),
                served("acme.a", "git.v2", true),
                served("acme.a", "fs", false),
            ]
        );
        assert_eq!(
            entry_names(&e.add),
            ["acme_a__docs", "acme_a__git.v2", "fs"]
        );
    }

    /// `prefix` defaults to true; `false` keys the entry by the bare name, with no `drop` needed.
    #[test]
    fn parse_edit_serve_prefix_chooses_the_key() {
        let cases = [
            (
                "default",
                json!({"name": "docs", "serve": {}}),
                "acme_a__docs",
            ),
            (
                "explicit true",
                json!({"name": "docs", "serve": {"prefix": true}}),
                "acme_a__docs",
            ),
            (
                "explicit false",
                json!({"name": "fs", "serve": {"prefix": false}}),
                "fs",
            ),
        ];
        for (label, entry, key) in cases {
            let e = parse_edit(json!({"add": [entry]}), Some("acme.a")).expect(label);
            assert_eq!(entry_names(&e.add), [key], "{label}");
            assert_eq!(e.drop, DropSpec::None, "{label}");
        }
    }

    /// An echoed marker is a new serve entry of the replying plugin, `plugin` ignored.
    #[test]
    fn parse_edit_treats_an_echoed_marker_as_the_repliers_serve_entry() {
        let echo =
            json!({"add":[{"name":"acme_a__docs","serve":{"prefix":true},"plugin":"acme.a"}]});
        let e = parse_edit(echo.clone(), Some("acme.b")).unwrap();
        assert_eq!(e.add, [served("acme.b", "acme_a__docs", true)]);
        assert_eq!(entry_names(&e.add), ["acme_b__acme_a__docs"]);
        assert!(parse_edit(echo, None).is_err());
    }

    #[test]
    fn parse_edit_rejects_invalid_serve_entries() {
        let secret = "s3cr3t-value";
        let serving = Some("acme.a");
        let cases = [
            (
                "type alongside",
                json!({"add":[{"name":"d","type":"http","serve":{}}]}),
                serving,
            ),
            (
                "url alongside",
                json!({"add":[{"name":"d","url":secret,"serve":{}}]}),
                serving,
            ),
            (
                "headers alongside",
                json!({"add":[{"name":"d","headers":{"A":secret},"serve":{}}]}),
                serving,
            ),
            (
                "command alongside",
                json!({"add":[{"name":"d","command":secret,"serve":{}}]}),
                serving,
            ),
            (
                "args alongside",
                json!({"add":[{"name":"d","args":[secret],"serve":{}}]}),
                serving,
            ),
            (
                "env alongside",
                json!({"add":[{"name":"d","env":{"K":secret},"serve":{}}]}),
                serving,
            ),
            (
                "empty args alongside",
                json!({"add":[{"name":"d","args":[],"serve":{}}]}),
                serving,
            ),
            (
                "empty env alongside",
                json!({"add":[{"name":"d","env":{},"serve":{}}]}),
                serving,
            ),
            (
                "empty headers alongside",
                json!({"add":[{"name":"d","headers":{},"serve":{}}]}),
                serving,
            ),
            (
                "unknown serve field",
                json!({"add":[{"name":"d","serve":{"bogus":secret}}]}),
                serving,
            ),
            (
                "invalid name",
                json!({"add":[{"name":"bad name","serve":{}}]}),
                serving,
            ),
            (
                "empty name",
                json!({"add":[{"name":"","serve":{}}]}),
                serving,
            ),
            (
                "shadow is unknown",
                json!({"drop":["fs"],"add":[{"name":"fs","serve":{"shadow":true}}]}),
                serving,
            ),
            (
                "prefix not a bool",
                json!({"add":[{"name":"fs","serve":{"prefix":secret}}]}),
                serving,
            ),
            (
                "invalid name unprefixed",
                json!({"add":[{"name":"bad name","serve":{"prefix":false}}]}),
                serving,
            ),
            (
                "transport next to an unprefixed entry",
                json!({"add":[{"name":"fs","url":secret,"serve":{"prefix":false}}]}),
                serving,
            ),
            (
                "plugin without both capabilities",
                json!({"add":[{"name":"d","serve":{}}]}),
                None,
            ),
        ];
        for (label, v, plugin) in cases {
            let err = parse_edit(v, plugin).expect_err(label);
            assert!(
                !err.contains(secret),
                "{label}: error leaked a value: {err}"
            );
        }
    }

    #[test]
    fn request_params_carry_full_definitions() {
        let servers = vec![
            stdio("ha", &[("TOKEN", "sekret")]),
            http("docs", "https://e/mcp", &[("Authorization", "Bearer t")]),
        ];
        let p = initial_params(&ctx(), &servers);
        assert_eq!(p["session_id"], "s1");
        assert_eq!(p["agent"], "agent-x");
        assert_eq!(p["sandboxed"], true);
        assert_eq!(p["profile"], "work");
        assert_eq!(p["cwd"], "/w");
        assert_eq!(
            p["sandbox"],
            json!({
                "container_workdir": "/workspace/foo",
                "mounts": [{"host": "/abs/host/foo", "container": "/workspace/foo", "read_only": false}]
            })
        );
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
    fn request_params_omit_sandbox_for_a_host_session() {
        let mut c = ctx();
        c.sandbox = None;
        let p = initial_params(&c, &[]);
        assert_eq!(p["sandboxed"], false);
        assert!(p.get("sandbox").is_none(), "{p}");
    }

    fn linked_worktree(repo: &Path, worktree: &Path) {
        let git = git2::Repository::init(repo).unwrap();
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        let tree = git
            .find_tree(git.index().unwrap().write_tree().unwrap())
            .unwrap();
        let head = git
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let branch = git
            .branch("feature", &git.find_commit(head).unwrap(), false)
            .unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(branch.get()));
        git.worktree("feature", worktree, Some(&opts)).unwrap();
    }

    fn sandbox_info(container_workdir: &str) -> SandboxInfo {
        SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-mcp".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: Some(container_workdir.into()),
            provider: None,
        }
    }

    fn host(p: &Path) -> String {
        p.canonicalize().unwrap().to_string_lossy().into_owned()
    }

    /// A sibling worktree mounts both itself and its main repo, so both reach the plugin.
    #[test]
    fn sandbox_of_a_linked_worktree_lists_worktree_and_main_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("repo-feature");
        linked_worktree(&repo, &worktree);
        let info = sandbox_info("/workspace/repo-feature");

        let sandbox = McpSandbox::resolve(&info, &worktree, None, "s1");

        assert_eq!(
            sandbox,
            McpSandbox {
                container_workdir: Some("/workspace/repo-feature".into()),
                mounts: vec![
                    McpSandboxMount {
                        host: host(&repo),
                        container: "/workspace/repo".into(),
                        read_only: false,
                    },
                    McpSandboxMount {
                        host: host(&worktree),
                        container: "/workspace/repo-feature".into(),
                        read_only: false,
                    },
                ],
            }
        );
    }

    /// A workspace container mounts the workspace dir plus each main repo outside it.
    #[test]
    fn sandbox_of_a_workspace_lists_the_workspace_and_main_repos_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws").join("feat");
        let outside_main = tmp.path().join("projects").join("alpha");
        let inside_main = ws.join("beta-main");
        std::fs::create_dir_all(&ws).unwrap();
        linked_worktree(&outside_main, &ws.join("alpha"));
        linked_worktree(&inside_main, &ws.join("beta"));
        let workspace =
            test_support::workspace_info(&ws, &[("alpha", &outside_main), ("beta", &inside_main)]);
        let mut info = sandbox_info("unused");
        info.container_workdir = None;
        info.container_name = "aoe-sandbox-mcp-workspace-test-no-such-container".into();

        let sandbox = McpSandbox::resolve(&info, &ws, Some(&workspace), "s1");

        let (created, created_workdir) =
            crate::session::config::container_config::compute_workspace_volume_paths(
                &ws, &workspace,
            )
            .unwrap();
        let expected = vec![
            McpSandboxMount {
                host: host(&ws),
                container: "/workspace/ws/feat".into(),
                read_only: false,
            },
            McpSandboxMount {
                host: host(&outside_main),
                container: "/workspace/projects/alpha".into(),
                read_only: false,
            },
        ];
        assert_eq!(
            sandbox,
            McpSandbox {
                container_workdir: Some("/workspace/ws/feat".into()),
                mounts: expected,
            }
        );
        let created: Vec<_> = created
            .into_iter()
            .map(|v| (v.host_path, v.container_path))
            .collect();
        let reported: Vec<_> = sandbox
            .mounts
            .iter()
            .map(|m| (m.host.clone(), m.container.clone()))
            .collect();
        assert_eq!(reported, created, "mounts match container creation");
        assert_eq!(sandbox.container_workdir, Some(created_workdir));

        let mut c = ctx();
        c.sandbox = Some(sandbox);
        let p = initial_params(&c, &[]);
        assert_eq!(
            p["sandbox"]["mounts"],
            json!([
                {"host": host(&ws), "container": "/workspace/ws/feat", "read_only": false},
                {"host": host(&outside_main), "container": "/workspace/projects/alpha", "read_only": false}
            ])
        );
    }

    #[test]
    fn sandbox_without_a_layout_keeps_the_known_workdir_and_no_mounts() {
        let failed = McpSandbox::from_layout(
            Err(AcpError::Spawn("no layout".into())),
            || Some("/workspace/pinned".into()),
            "s1",
        );
        assert_eq!(
            failed,
            McpSandbox {
                container_workdir: Some("/workspace/pinned".into()),
                mounts: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_edit_accepts_names_star_and_adds() {
        let e = parse_edit(
            json!({
                "drop": ["a", "b"],
                "add": [{"name":"x","type":"http","url":"http://g/mcp/x","headers":{"Authorization":"Bearer t"}}]
            }),
            None,
        )
        .unwrap();
        assert_eq!(e.drop, DropSpec::Names(vec!["a".into(), "b".into()]));
        assert_eq!(
            e.add,
            entries(vec![http(
                "x",
                "http://g/mcp/x",
                &[("Authorization", "Bearer t")]
            )])
        );

        let e = parse_edit(json!({"drop": "*"}), None).unwrap();
        assert_eq!(e.drop, DropSpec::All);
        assert!(e.add.is_empty());

        let e = parse_edit(json!({}), None).unwrap();
        assert_eq!(e.drop, DropSpec::None);
    }

    /// Explicit empty collections on a plain entry mean the same as absent ones; `null` does not.
    #[test]
    fn parse_edit_plain_entries_accept_empty_collections_but_not_null() {
        let e = parse_edit(
            json!({"add":[
                {"name":"s","command":"c","args":[],"env":{},"headers":{}},
                {"name":"h","type":"http","url":"http://g","args":[],"env":{},"headers":{}},
            ]}),
            None,
        )
        .unwrap();
        assert_eq!(
            e.add,
            entries(vec![
                ProjectMcpServer {
                    name: "s".into(),
                    transport: ProjectMcpTransport::Stdio {
                        command: "c".into(),
                        args: vec![],
                        env: BTreeMap::new()
                    }
                },
                http("h", "http://g", &[]),
            ])
        );
        for field in ["args", "env", "headers"] {
            let mut entry = json!({"name":"s","command":"c"});
            entry[field] = Value::Null;
            assert!(
                parse_edit(json!({"add":[entry]}), None).is_err(),
                "{field}: null"
            );
        }
    }

    #[test]
    fn parse_edit_defaults_missing_type_to_stdio() {
        let e = parse_edit(json!({"add":[{"name":"s","command":"c"}]}), None).unwrap();
        assert_eq!(
            e.add,
            entries(vec![ProjectMcpServer {
                name: "s".into(),
                transport: ProjectMcpTransport::Stdio {
                    command: "c".into(),
                    args: vec![],
                    env: BTreeMap::new()
                }
            }])
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
            assert!(parse_edit(v, None).is_err(), "{label} must be rejected");
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
            let err = parse_edit(v, None).unwrap_err();
            assert!(!err.contains(secret), "error leaked a value: {err}");
        }
    }

    #[test]
    fn apply_edit_drop_names_star_and_none() {
        let base = || entries(vec![stdio("a", &[]), stdio("b", &[]), stdio("c", &[])]);
        let out = apply_edit(
            base(),
            McpEdit {
                drop: DropSpec::Names(vec!["b".into(), "zz".into()]),
                add: vec![],
            },
        );
        assert_eq!(entry_names(&out), ["a", "c"]);
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
        assert_eq!(entry_names(&out), ["a", "b", "c"]);
    }

    #[test]
    fn apply_edit_added_server_replaces_same_name_and_last_duplicate_wins() {
        let out = apply_edit(
            entries(vec![stdio("a", &[("K", "old")]), stdio("keep", &[])]),
            McpEdit {
                drop: DropSpec::None,
                add: entries(vec![
                    http("a", "http://one", &[]),
                    http("n", "http://first", &[]),
                    http("n", "http://second", &[]),
                ]),
            },
        );
        assert_eq!(entry_names(&out), ["keep", "a", "n"]);
        assert_eq!(out[1], McpEntry::from(http("a", "http://one", &[])));
        assert_eq!(out[2], McpEntry::from(http("n", "http://second", &[])));
    }

    #[test]
    fn apply_edit_drop_all_then_add_keeps_only_added() {
        let out = apply_edit(
            entries(vec![stdio("a", &[]), stdio("b", &[])]),
            McpEdit {
                drop: DropSpec::All,
                add: entries(vec![http("a", "http://g", &[])]),
            },
        );
        assert_eq!(out, entries(vec![http("a", "http://g", &[])]));
    }

    /// Serve markers share the name space: drop and later adds hit them by entry key.
    #[test]
    fn apply_edit_treats_serve_markers_like_any_entry() {
        let marker = served("acme.a", "docs", true);
        let set = vec![stdio("keep", &[]).into(), marker.clone()];
        let cases = [
            (
                "drop by key",
                DropSpec::Names(vec!["acme_a__docs".into()]),
                vec![],
                vec![stdio("keep", &[]).into()],
            ),
            (
                "replace by key",
                DropSpec::None,
                entries(vec![http("acme_a__docs", "http://g", &[])]),
                entries(vec![
                    stdio("keep", &[]),
                    http("acme_a__docs", "http://g", &[]),
                ]),
            ),
            (
                "marker replaces a server",
                DropSpec::None,
                vec![served("acme.b", "keep", false)],
                vec![marker.clone(), served("acme.b", "keep", false)],
            ),
        ];
        for (label, drop, add, want) in cases {
            assert_eq!(
                apply_edit(set.clone(), McpEdit { drop, add }),
                want,
                "{label}"
            );
        }
    }

    /// A key added twice in one reply resolves to the last, with the replying plugin as its
    /// only contributor.
    #[test]
    fn a_key_added_twice_in_one_reply_resolves_to_the_last_with_one_contributor() {
        let cases = [
            (
                "server then server",
                vec![
                    http("dup", "http://first", &[]).into(),
                    http("dup", "http://last", &[]).into(),
                ],
                McpEntry::from(http("dup", "http://last", &[])),
            ),
            (
                "server then serve marker",
                vec![
                    http("dup", "http://first", &[]).into(),
                    served("acme.a", "dup", false),
                ],
                served("acme.a", "dup", false),
            ),
        ];
        for (label, add, last) in cases {
            let set = entries(vec![stdio("keep", &[]), stdio("dup", &[])]);
            let mut contributors = Contributors::from([
                ("keep".to_string(), "acme.z".to_string()),
                ("dup".to_string(), "acme.z".to_string()),
            ]);
            let out = apply_tracked_edit(
                set,
                &mut contributors,
                "acme.a",
                McpEdit {
                    drop: DropSpec::None,
                    add,
                },
            );
            assert_eq!(out, [McpEntry::from(stdio("keep", &[])), last], "{label}");
            assert_eq!(
                contributors,
                Contributors::from([
                    ("keep".to_string(), "acme.z".to_string()),
                    ("dup".to_string(), "acme.a".to_string()),
                ]),
                "{label}"
            );
        }
    }

    fn entry_names(entries: &[McpEntry]) -> Vec<&str> {
        entries.iter().map(McpEntry::name).collect()
    }

    struct Stub(Result<Vec<ProjectMcpServer>, RewriteError>);

    impl McpRewriter for Stub {
        fn rewrite<'a>(
            &'a self,
            _ctx: &'a McpRewriteContext,
            _servers: Vec<ProjectMcpServer>,
        ) -> RewriteFuture<'a> {
            let out = match &self.0 {
                Ok(v) => Ok(v.clone().into()),
                Err(e) => Err(RewriteError {
                    plugin: e.plugin.clone(),
                    cause: e.cause.clone(),
                }),
            };
            Box::pin(async move { out })
        }
    }

    fn failing() -> Arc<dyn McpRewriter> {
        Arc::new(Stub(Err(RewriteError {
            plugin: "acme.gate".into(),
            cause: "timed out".into(),
        })))
    }

    #[tokio::test]
    async fn no_rewriter_passes_servers_through() {
        let servers = vec![stdio("a", &[("K", "v")])];
        let out = rewrite_servers_with(None, &ctx(), servers.clone()).await;
        assert_eq!(out, Rewritten::from(servers));
    }

    #[tokio::test]
    async fn successful_rewrite_replaces_the_set() {
        let ok: Arc<dyn McpRewriter> = Arc::new(Stub(Ok(vec![http("p", "http://g", &[])])));
        let out = rewrite_servers_with(Some(ok), &ctx(), vec![stdio("a", &[])]).await;
        assert_eq!(names(&out.servers), ["p"]);
    }

    #[tokio::test]
    async fn sandboxed_failure_forwards_nothing() {
        let out =
            rewrite_servers_with(Some(failing()), &ctx(), vec![stdio("a", &[("K", "v")])]).await;
        assert_eq!(out, Rewritten::default());
    }

    #[tokio::test]
    async fn non_sandboxed_failure_keeps_the_original_set() {
        let mut c = ctx();
        c.sandbox = None;
        let servers = vec![stdio("a", &[])];
        let out = rewrite_servers_with(Some(failing()), &c, servers.clone()).await;
        assert_eq!(out, Rewritten::from(servers));
    }
}
