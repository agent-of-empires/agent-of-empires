//! Plugin rewrite of the MCP servers forwarded to a session's agent.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::acp::acp_client::{known_container_workdir, AcpError, SandboxLayout};
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
    let mut params = json!({
        "session_id": ctx.session_id,
        "agent": ctx.agent,
        "sandboxed": ctx.sandboxed(),
        "profile": ctx.profile,
        "cwd": ctx.cwd,
        "servers": servers.iter().map(WireServer::from).collect::<Vec<_>>(),
    });
    if let Some(sandbox) = &ctx.sandbox {
        params["sandbox"] = json!(sandbox);
    }
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
    Pin<Box<dyn Future<Output = Result<Vec<ProjectMcpServer>, RewriteError>> + Send + 'a>>;

/// Rewrites the resolved set for one session. An implementation with no applicable hook
/// returns the servers unchanged.
pub trait McpRewriter: Send + Sync {
    fn rewrite<'a>(
        &'a self,
        ctx: &'a McpRewriteContext,
        servers: Vec<ProjectMcpServer>,
    ) -> RewriteFuture<'a>;
}

static REWRITER: Mutex<Option<Arc<dyn McpRewriter>>> = Mutex::new(None);

fn current_rewriter() -> Option<Arc<dyn McpRewriter>> {
    REWRITER.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set_rewriter(rewriter: Option<Arc<dyn McpRewriter>>) {
    *REWRITER.lock().unwrap_or_else(|e| e.into_inner()) = rewriter;
}

pub async fn rewrite_servers(
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
) -> Vec<ProjectMcpServer> {
    rewrite_servers_with(current_rewriter(), ctx, servers).await
}

/// A failed hook forwards nothing to a sandboxed session and leaves other sessions unchanged.
pub async fn rewrite_servers_with(
    rewriter: Option<Arc<dyn McpRewriter>>,
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
) -> Vec<ProjectMcpServer> {
    let Some(rewriter) = rewriter else {
        return servers;
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
            original.unwrap_or_default()
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
        result: Vec<ProjectMcpServer>,
    }

    impl RecordingRewriter {
        pub(crate) fn returning(result: Vec<ProjectMcpServer>) -> Arc<Self> {
            Arc::new(Self {
                sandboxes: Mutex::new(Vec::new()),
                result,
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
        let p = request_params(&c, &[]);
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
        let p = request_params(&c, &[]);
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

    struct Stub(Result<Vec<ProjectMcpServer>, RewriteError>);

    impl McpRewriter for Stub {
        fn rewrite<'a>(
            &'a self,
            _ctx: &'a McpRewriteContext,
            _servers: Vec<ProjectMcpServer>,
        ) -> RewriteFuture<'a> {
            let out = match &self.0 {
                Ok(v) => Ok(v.clone()),
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
        assert_eq!(out, servers);
    }

    #[tokio::test]
    async fn successful_rewrite_replaces_the_set() {
        let ok: Arc<dyn McpRewriter> = Arc::new(Stub(Ok(vec![http("p", "http://g", &[])])));
        let out = rewrite_servers_with(Some(ok), &ctx(), vec![stdio("a", &[])]).await;
        assert_eq!(names(&out), ["p"]);
    }

    #[tokio::test]
    async fn sandboxed_failure_forwards_nothing() {
        let out =
            rewrite_servers_with(Some(failing()), &ctx(), vec![stdio("a", &[("K", "v")])]).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn non_sandboxed_failure_keeps_the_original_set() {
        let mut c = ctx();
        c.sandbox = None;
        let servers = vec![stdio("a", &[])];
        let out = rewrite_servers_with(Some(failing()), &c, servers.clone()).await;
        assert_eq!(out, servers);
    }
}
