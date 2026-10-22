//! Plugin-backed `McpRewriter`: asks each hooked worker to edit the forwarded MCP set.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use aoe_plugin_api::{McpRewriteScope, PluginManifest, MCP_SERVE_CAPABILITY};
use futures_util::future::BoxFuture;

use crate::acp::mcp_rewrite::{
    apply_tracked_edit, initial_contributors, parse_edit, request_params, McpEntry,
    McpRewriteContext, McpRewriter, RewriteError, RewriteFuture, Rewritten, METHOD,
};
use crate::plugin::host::PluginHost;
use crate::plugin::mcp_serve::{plugin_entries, servers_for_session, ServedServer};
use crate::plugin::worker_call::WorkerCall;
use crate::server::daemon_endpoint::DaemonEndpoint;
use crate::session::mcp::project_mcp::ProjectMcpServer;

pub struct RewriteHook {
    pub plugin_id: String,
    pub timeout: Duration,
    pub order: i32,
    /// The plugin also holds `mcp.serve`, so its edits may add serve entries.
    pub serves: bool,
}

/// Plugins hooked for this session kind, by ascending `order` then plugin id, so chaining is deterministic.
pub fn hooks_for<'a>(
    manifests: impl Iterator<Item = &'a PluginManifest>,
    sandboxed: bool,
) -> Vec<RewriteHook> {
    let mut hooks: Vec<RewriteHook> = manifests
        .filter(|m| m.capabilities.iter().any(|c| c.as_str() == "mcp.rewrite"))
        .filter_map(|m| {
            let hook = m.hooks.as_ref()?.mcp_rewrite.as_ref()?;
            (sandboxed || hook.applies_to == McpRewriteScope::All).then(|| RewriteHook {
                plugin_id: m.id.as_str().to_string(),
                timeout: Duration::from_millis(hook.timeout_ms),
                order: hook.order,
                serves: m
                    .capabilities
                    .iter()
                    .any(|c| c.as_str() == MCP_SERVE_CAPABILITY),
            })
        })
        .collect();
    hooks.sort_by(|a, b| (a.order, &a.plugin_id).cmp(&(b.order, &b.plugin_id)));
    hooks
}

pub async fn run_chain(
    caller: &dyn WorkerCall,
    hooks: &[RewriteHook],
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
) -> Result<Vec<McpEntry>, RewriteError> {
    let mut contributors = initial_contributors(ctx, &servers);
    let mut entries: Vec<McpEntry> = servers.into_iter().map(McpEntry::from).collect();
    for hook in hooks {
        let params = request_params(ctx, &entries, &contributors);
        let serving = hook.serves.then_some(hook.plugin_id.as_str());
        let edit = caller
            .call(&hook.plugin_id, METHOD, params, hook.timeout)
            .await
            .map_err(|e| e.to_string())
            .and_then(|reply| parse_edit(reply, serving));
        match edit {
            Ok(edit) => {
                entries = apply_tracked_edit(entries, &mut contributors, &hook.plugin_id, edit)
            }
            Err(cause) => {
                caller.report_failure(&hook.plugin_id, &cause);
                return Err(RewriteError {
                    plugin: hook.plugin_id.clone(),
                    cause,
                });
            }
        }
    }
    Ok(entries)
}

/// Turns the chain's serve entries into credentialed plugin entries. Without a daemon URL
/// the session reaches they are omitted together with any same-key entry, so nothing falls
/// back to a server they replaced, and each serving plugin is reported once.
fn resolve_served(
    caller: &dyn WorkerCall,
    entries: Vec<McpEntry>,
    endpoint: Option<&DaemonEndpoint>,
    ctx: &McpRewriteContext,
) -> Rewritten {
    let target = endpoint.and_then(|e| Some((&*e.secret, e.base_url(ctx.sandboxed())?)));
    let mut out = Rewritten::default();
    let mut unreachable: BTreeSet<String> = BTreeSet::new();
    let mut omitted: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        match (entry, target) {
            (McpEntry::Server(server), _) => out.servers.push(server),
            (McpEntry::Serve { served }, Some((secret, base_url))) => {
                for p in plugin_entries(secret, base_url, &ctx.session_id, &[served]) {
                    out.served_by.insert(p.server.name.clone(), p.plugin_id);
                    out.servers.push(p.server);
                }
            }
            (McpEntry::Serve { served }, None) => {
                omitted.insert(served.entry_key);
                unreachable.insert(served.plugin_id);
            }
        }
    }
    out.servers.retain(|s| !omitted.contains(&s.name));
    for plugin_id in unreachable {
        caller.report_serve_failure(
            &plugin_id,
            "This session cannot reach the daemon's MCP endpoint, so the servers the plugin's rewrite hook serves are skipped for it.",
        );
    }
    out
}

/// A missing worker caller fails closed, but only when a hook applies.
async fn run_hooks(
    caller: Option<&dyn WorkerCall>,
    hooks: &[RewriteHook],
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
    endpoint: Option<&DaemonEndpoint>,
) -> Result<Rewritten, RewriteError> {
    let Some(first) = hooks.first() else {
        return Ok(servers.into());
    };
    let Some(caller) = caller else {
        return Err(RewriteError {
            plugin: first.plugin_id.clone(),
            cause: "plugin host unavailable".into(),
        });
    };
    let entries = run_chain(caller, hooks, ctx, servers).await?;
    Ok(resolve_served(caller, entries, endpoint, ctx))
}

pub struct PluginMcpRewriter {
    host: Option<Arc<PluginHost>>,
}

impl PluginMcpRewriter {
    pub fn new(host: Option<Arc<PluginHost>>) -> Self {
        Self { host }
    }
}

impl McpRewriter for PluginMcpRewriter {
    fn rewrite<'a>(
        &'a self,
        ctx: &'a McpRewriteContext,
        servers: Vec<ProjectMcpServer>,
    ) -> RewriteFuture<'a> {
        Box::pin(async move {
            let registry = crate::plugin::registry();
            let hooks = hooks_for(registry.active().map(|p| &p.manifest), ctx.sandboxed());
            let caller = self.host.as_deref().map(|h| h as &dyn WorkerCall);
            let endpoint = crate::server::daemon_endpoint::daemon_endpoint();
            run_hooks(caller, &hooks, ctx, servers, endpoint.as_deref()).await
        })
    }

    fn served_servers<'a>(
        &'a self,
        ctx: &'a McpRewriteContext,
    ) -> BoxFuture<'a, Vec<ServedServer>> {
        Box::pin(async move {
            let registry = crate::plugin::registry();
            let caller = self.host.as_deref().map(|h| h as &dyn WorkerCall);
            servers_for_session(caller, registry.active().map(|p| &p.manifest), ctx).await
        })
    }

    fn report_serve_notice(&self, plugin_id: &str, message: &str) {
        if let Some(host) = &self.host {
            WorkerCall::report_serve_notice(host.as_ref(), plugin_id, message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::worker_requests::WorkerRequestError;
    use crate::session::mcp::project_mcp::ProjectMcpTransport;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    fn manifest(id: &str, scope: &str, with_cap: bool, with_hook: bool) -> PluginManifest {
        let caps = if with_cap {
            "\"runtime.worker\", \"mcp.rewrite\""
        } else {
            "\"runtime.worker\""
        };
        let hook = if with_hook {
            format!("\n[hooks.mcp_rewrite]\napplies_to = \"{scope}\"\ntimeout_ms = 2000\n")
        } else {
            String::new()
        };
        PluginManifest::from_toml_str(&format!(
            "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 21\ncapabilities = [{caps}]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n{hook}"
        ))
        .unwrap()
    }

    fn ids(hooks: &[RewriteHook]) -> Vec<&str> {
        hooks.iter().map(|h| h.plugin_id.as_str()).collect()
    }

    #[test]
    fn hooks_for_sorts_by_order_then_plugin_id() {
        let with_order = |id: &str, order: i32| {
            PluginManifest::from_toml_str(&format!(
                "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 21\ncapabilities = [\"runtime.worker\", \"mcp.rewrite\"]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n\n[hooks.mcp_rewrite]\norder = {order}\n"
            ))
            .unwrap()
        };
        let ms = [
            with_order("acme.proxy", 1000),
            with_order("acme.b-default", 0),
            with_order("acme.a-default", 0),
            with_order("acme.early", -10),
        ];
        assert_eq!(
            ids(&hooks_for(ms.iter(), true)),
            [
                "acme.early",
                "acme.a-default",
                "acme.b-default",
                "acme.proxy"
            ],
            "the proxy declared the highest order and so runs last"
        );
    }

    #[test]
    fn hooks_for_selects_by_scope_capability_and_sorts_by_id() {
        let ms = [
            manifest("acme.z", "sandbox", true, true),
            manifest("acme.a", "all", true, true),
            manifest("acme.nohook", "sandbox", true, false),
            manifest("acme.nocap", "sandbox", false, false),
        ];
        assert_eq!(ids(&hooks_for(ms.iter(), true)), ["acme.a", "acme.z"]);
        assert_eq!(
            ids(&hooks_for(ms.iter(), false)),
            ["acme.a"],
            "sandbox-scoped hook skips plain sessions"
        );
        assert_eq!(
            hooks_for(ms.iter(), true)[0].timeout,
            Duration::from_millis(2000)
        );
    }

    #[test]
    fn hooks_for_marks_plugins_that_also_serve() {
        let serving = PluginManifest::from_toml_str(
            "id = \"acme.s\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 22\ncapabilities = [\"runtime.worker\", \"mcp.rewrite\", \"mcp.serve\"]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n\n[hooks.mcp_rewrite]\n",
        )
        .unwrap();
        let ms = [serving, manifest("acme.r", "sandbox", true, true)];
        let serves: Vec<(String, bool)> = hooks_for(ms.iter(), true)
            .into_iter()
            .map(|h| (h.plugin_id, h.serves))
            .collect();
        assert_eq!(
            serves,
            [("acme.r".to_string(), false), ("acme.s".to_string(), true)]
        );
    }

    struct FakeCaller {
        replies: Mutex<Vec<Result<Value, WorkerRequestError>>>,
        seen_params: Mutex<Vec<Value>>,
        failures: Mutex<Vec<(String, String)>>,
        serve_failures: Mutex<Vec<String>>,
    }

    impl FakeCaller {
        fn new(replies: Vec<Result<Value, WorkerRequestError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen_params: Mutex::default(),
                failures: Mutex::default(),
                serve_failures: Mutex::default(),
            }
        }
    }

    impl WorkerCall for FakeCaller {
        fn call<'a>(
            &'a self,
            _plugin_id: &'a str,
            method: &'a str,
            params: Value,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
            assert_eq!(method, "mcp.rewrite");
            self.seen_params.lock().unwrap().push(params);
            let reply = self.replies.lock().unwrap().remove(0);
            Box::pin(async move { reply })
        }

        fn request_id(&self) -> String {
            unreachable!("the rewrite chain never names its requests")
        }

        fn call_as<'a>(
            &'a self,
            _id: &'a str,
            _plugin_id: &'a str,
            _method: &'a str,
            _params: Value,
            _start_wait: Duration,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
            unreachable!("the rewrite chain never names its requests")
        }

        fn notify<'a>(
            &'a self,
            _plugin_id: &'a str,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, bool> {
            unreachable!("the rewrite chain sends no notifications")
        }

        fn report_failure(&self, plugin_id: &str, message: &str) {
            self.failures
                .lock()
                .unwrap()
                .push((plugin_id.to_string(), message.to_string()));
        }

        fn report_serve_failure(&self, plugin_id: &str, _message: &str) {
            self.serve_failures
                .lock()
                .unwrap()
                .push(plugin_id.to_string());
        }

        fn report_serve_notice(&self, _plugin_id: &str, _message: &str) {
            unreachable!("hook edits never notify")
        }
    }

    fn hook(id: &str) -> RewriteHook {
        RewriteHook {
            plugin_id: id.into(),
            timeout: Duration::from_secs(1),
            order: 0,
            serves: false,
        }
    }

    fn serving_hook(id: &str) -> RewriteHook {
        RewriteHook {
            serves: true,
            ..hook(id)
        }
    }

    fn ctx() -> McpRewriteContext {
        McpRewriteContext {
            session_id: "s".into(),
            agent: "a".into(),
            sandbox: Some(crate::acp::mcp_rewrite::McpSandbox {
                container_workdir: None,
                mounts: Vec::new(),
            }),
            profile: None,
            cwd: "/w".into(),
            plugin_served: Vec::new(),
        }
    }

    fn stdio(name: &str) -> ProjectMcpServer {
        ProjectMcpServer {
            name: name.into(),
            transport: ProjectMcpTransport::Stdio {
                command: "c".into(),
                args: vec![],
                env: Default::default(),
            },
        }
    }

    #[tokio::test]
    async fn chain_applies_edits_in_order_and_feeds_each_hook_the_previous_output() {
        let caller = FakeCaller::new(vec![
            Ok(json!({"drop":["a"],"add":[{"name":"g","type":"http","url":"http://g"}]})),
            Ok(json!({"drop":["b"]})),
        ]);
        let out = run_chain(
            &caller,
            &[hook("acme.a"), hook("acme.b")],
            &ctx(),
            vec![stdio("a"), stdio("b")],
        )
        .await
        .unwrap();
        assert_eq!(out.iter().map(McpEntry::name).collect::<Vec<_>>(), ["g"]);
        let seen = caller.seen_params.lock().unwrap();
        let names: Vec<&str> = seen[1]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["b", "g"],
            "second hook sees the first hook's output"
        );
    }

    #[tokio::test]
    async fn chain_failure_reports_and_aborts() {
        let cases: Vec<(&str, Result<Value, WorkerRequestError>)> = vec![
            ("timeout", Err(WorkerRequestError::Timeout)),
            ("no worker", Err(WorkerRequestError::NoWorker)),
            ("rejected", Err(WorkerRequestError::Rejected)),
            ("bad shape", Ok(json!({"drop": 3}))),
        ];
        for (label, reply) in cases {
            let caller = FakeCaller::new(vec![reply]);
            let err = run_chain(
                &caller,
                &[hook("acme.a"), hook("acme.b")],
                &ctx(),
                vec![stdio("a")],
            )
            .await
            .unwrap_err();
            assert_eq!(err.plugin, "acme.a", "{label}");
            let failures = caller.failures.lock().unwrap();
            assert_eq!(failures.len(), 1, "{label}: exactly one visible report");
            assert_eq!(failures[0].0, "acme.a");
        }
    }

    #[tokio::test]
    async fn no_hooks_returns_servers_unchanged_without_calling_workers() {
        let caller = FakeCaller::new(vec![]);
        let servers = vec![stdio("a")];
        let out = run_chain(&caller, &[], &ctx(), servers.clone())
            .await
            .unwrap();
        assert_eq!(out, [McpEntry::from(stdio("a"))]);
        assert!(caller.seen_params.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_host_fails_closed_only_when_a_hook_applies() {
        let servers = vec![stdio("a")];
        let err = run_hooks(None, &[hook("acme.a")], &ctx(), servers.clone(), None)
            .await
            .unwrap_err();
        assert_eq!(err.plugin, "acme.a");
        let out = run_hooks(None, &[], &ctx(), servers.clone(), None)
            .await
            .unwrap();
        assert_eq!(out, Rewritten::from(servers));
    }

    use crate::server::session_credential::McpSessionSecret;
    use std::collections::BTreeMap;

    fn host_ctx() -> McpRewriteContext {
        McpRewriteContext {
            sandbox: None,
            ..ctx()
        }
    }

    fn endpoint(dir: &std::path::Path, sandbox_url: Option<&str>) -> DaemonEndpoint {
        DaemonEndpoint {
            host_url: "http://127.0.0.1:4321".into(),
            sandbox_url: sandbox_url.map(str::to_string),
            secret: Arc::new(McpSessionSecret::load_or_generate(&dir.join("secret")).unwrap()),
        }
    }

    /// The entry `plugin_entries` mints for `plugin`/`name` under `key`.
    fn plugin_http(
        secret: &McpSessionSecret,
        base: &str,
        plugin: &str,
        name: &str,
        key: &str,
    ) -> ProjectMcpServer {
        let credential = secret.plugin_server_credential_for(plugin, name, "s");
        ProjectMcpServer {
            name: key.into(),
            transport: ProjectMcpTransport::Http {
                url: format!("{base}/mcp/plugins/{plugin}/{name}"),
                headers: BTreeMap::from([
                    ("Authorization".to_string(), format!("Bearer {credential}")),
                    ("X-AoE-Session".to_string(), "s".to_string()),
                ]),
            },
        }
    }

    fn served_by(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn later_hooks_see_a_serve_marker_and_never_a_credential() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path(), None);
        let caller = FakeCaller::new(vec![
            Ok(json!({"add":[
                {"name":"docs","serve":{}},
                {"name":"fs","serve":{"prefix":false}},
            ]})),
            Ok(json!({})),
            Ok(json!({})),
        ]);
        let hooks = [serving_hook("acme.a"), hook("acme.b"), hook("acme.c")];
        let out = run_hooks(
            Some(&caller),
            &hooks,
            &host_ctx(),
            vec![stdio("fs"), stdio("keep")],
            Some(&ep),
        )
        .await
        .unwrap();

        let seen = caller.seen_params.lock().unwrap();
        for later in &seen[1..] {
            assert_eq!(
                later["servers"],
                json!([
                    {"name":"keep","type":"stdio","command":"c"},
                    {"name":"acme_a__docs","serve":{"prefix":true},"plugin":"acme.a"},
                    {"name":"fs","serve":{"prefix":false},"plugin":"acme.a"},
                ])
            );
        }
        let base = "http://127.0.0.1:4321";
        let docs = plugin_http(&ep.secret, base, "acme.a", "docs", "acme_a__docs");
        let fs = plugin_http(&ep.secret, base, "acme.a", "fs", "fs");
        for request in seen.iter() {
            let text = request.to_string();
            for name in ["docs", "fs"] {
                let credential = ep.secret.plugin_server_credential_for("acme.a", name, "s");
                assert!(!text.contains(&credential), "{text}");
            }
        }
        assert_eq!(
            out,
            Rewritten {
                servers: vec![stdio("keep"), docs, fs],
                served_by: served_by(&[("acme_a__docs", "acme.a"), ("fs", "acme.a")]),
            }
        );
        assert!(caller.serve_failures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn later_hooks_edit_serve_entries_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path(), None);
        let base = "http://127.0.0.1:4321";
        let add_docs = json!({"add":[{"name":"docs","serve":{}}]});
        let gateway = ProjectMcpServer {
            name: "acme_a__docs".into(),
            transport: ProjectMcpTransport::Http {
                url: "http://gateway/docs".into(),
                headers: BTreeMap::new(),
            },
        };
        let cases = [
            (
                "dropped",
                json!({"drop":["acme_a__docs"]}),
                Rewritten::default(),
            ),
            (
                "replaced by a plain add",
                json!({"add":[{"name":"acme_a__docs","type":"http","url":"http://gateway/docs"}]}),
                Rewritten::from(vec![gateway]),
            ),
            (
                "same key served by a later hook wins",
                json!({"add":[{"name":"acme_a__docs","serve":{"prefix":false}}]}),
                Rewritten {
                    servers: vec![plugin_http(
                        &ep.secret,
                        base,
                        "acme.b",
                        "acme_a__docs",
                        "acme_a__docs",
                    )],
                    served_by: served_by(&[("acme_a__docs", "acme.b")]),
                },
            ),
        ];
        for (label, second, want) in cases {
            let caller = FakeCaller::new(vec![Ok(add_docs.clone()), Ok(second)]);
            let hooks = [serving_hook("acme.a"), serving_hook("acme.b")];
            let out = run_hooks(Some(&caller), &hooks, &host_ctx(), Vec::new(), Some(&ep))
                .await
                .unwrap();
            assert_eq!(out, want, "{label}");
        }
    }

    #[tokio::test]
    async fn serve_entries_need_a_daemon_url_the_session_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox_url = "http://host.containers.internal:4321";
        // (case, sandboxed, endpoint: None | Some(sandbox url), served base)
        let cases: [(&str, bool, Option<Option<&str>>, Option<&str>); 5] = [
            ("host, no endpoint", false, None, None),
            ("sandbox, no endpoint", true, None, None),
            ("sandbox without sandbox url", true, Some(None), None),
            ("host", false, Some(None), Some("http://127.0.0.1:4321")),
            ("sandbox", true, Some(Some(sandbox_url)), Some(sandbox_url)),
        ];
        for (label, sandboxed, ep, served_base) in cases {
            let ep = ep.map(|url| endpoint(dir.path(), url));
            let caller = FakeCaller::new(vec![Ok(
                json!({"add":[{"name":"fs","serve":{"prefix":false}}]}),
            )]);
            let c = if sandboxed { ctx() } else { host_ctx() };
            let out = run_hooks(
                Some(&caller),
                &[serving_hook("acme.a")],
                &c,
                vec![stdio("fs"), stdio("keep")],
                ep.as_ref(),
            )
            .await
            .unwrap();
            let failures = caller.serve_failures.lock().unwrap().clone();
            match served_base {
                Some(base) => {
                    let secret = &ep.as_ref().unwrap().secret;
                    assert_eq!(
                        out.servers,
                        [
                            stdio("keep"),
                            plugin_http(secret, base, "acme.a", "fs", "fs")
                        ],
                        "{label}"
                    );
                    assert!(failures.is_empty(), "{label}");
                }
                None => {
                    assert_eq!(
                        out,
                        Rewritten::from(vec![stdio("keep")]),
                        "{label}: omitted, and the replaced original stays dropped"
                    );
                    assert_eq!(failures, ["acme.a"], "{label}: one notification");
                }
            }
        }
    }

    fn http(name: &str, url: &str) -> ProjectMcpServer {
        ProjectMcpServer {
            name: name.into(),
            transport: ProjectMcpTransport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
        }
    }

    /// `plugin` names whoever last contributed an entry, tracked by key through the chain,
    /// and is never taken from a reply.
    #[tokio::test]
    async fn hook_requests_name_the_contributing_plugin() {
        let injected = http("acme_s__docs", "http://d/mcp/plugins/acme.s/docs");
        let mut c = ctx();
        c.plugin_served = vec![crate::acp::mcp_rewrite::PluginServed {
            plugin_id: "acme.s".into(),
            server: injected.clone(),
        }];
        let caller = FakeCaller::new(vec![
            Ok(json!({"add":[
                {"name":"mk","serve":{}},
                {"name":"full","type":"http","url":"http://f","plugin":"acme.x"},
                {"name":"kept","type":"http","url":"http://k"},
            ]})),
            Ok(json!({"drop":["gone","acme_a__mk"],"add":[
                {"name":"full","type":"http","url":"http://f2","plugin":"acme.y"},
                {"name":"user","type":"http","url":"http://u"},
            ]})),
            Ok(json!({})),
        ]);
        let hooks = [serving_hook("acme.a"), hook("acme.b"), hook("acme.c")];
        run_chain(
            &caller,
            &hooks,
            &c,
            vec![stdio("user"), stdio("gone"), injected],
        )
        .await
        .unwrap();

        let seen = caller.seen_params.lock().unwrap();
        let docs = json!({"name":"acme_s__docs","type":"http","url":"http://d/mcp/plugins/acme.s/docs","plugin":"acme.s"});
        assert_eq!(
            seen[0]["servers"],
            json!([
                {"name":"user","type":"stdio","command":"c"},
                {"name":"gone","type":"stdio","command":"c"},
                docs,
            ]),
            "the first hook sees plugin only on session.servers entries"
        );
        assert_eq!(
            seen[1]["servers"],
            json!([
                {"name":"user","type":"stdio","command":"c"},
                {"name":"gone","type":"stdio","command":"c"},
                docs,
                {"name":"acme_a__mk","serve":{"prefix":true},"plugin":"acme.a"},
                {"name":"full","type":"http","url":"http://f","plugin":"acme.a"},
                {"name":"kept","type":"http","url":"http://k","plugin":"acme.a"},
            ]),
            "a forged plugin in the reply is ignored"
        );
        assert_eq!(
            seen[2]["servers"],
            json!([
                docs,
                {"name":"kept","type":"http","url":"http://k","plugin":"acme.a"},
                {"name":"full","type":"http","url":"http://f2","plugin":"acme.b"},
                {"name":"user","type":"http","url":"http://u","plugin":"acme.b"},
            ]),
            "replacing makes the later hook the contributor"
        );
    }

    /// A hook-served entry replaces a same-key entry like any add, silently.
    #[tokio::test]
    async fn an_unprefixed_serve_entry_replaces_a_same_key_entry_silently() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path(), None);
        let base = "http://127.0.0.1:4321";
        let caller = FakeCaller::new(vec![Ok(
            json!({"add":[{"name":"fs","serve":{"prefix":false}}]}),
        )]);
        let out = run_hooks(
            Some(&caller),
            &[serving_hook("acme.a")],
            &host_ctx(),
            vec![stdio("fs"), stdio("keep")],
            Some(&ep),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            Rewritten {
                servers: vec![
                    stdio("keep"),
                    plugin_http(&ep.secret, base, "acme.a", "fs", "fs")
                ],
                served_by: served_by(&[("fs", "acme.a")]),
            }
        );
        assert!(caller.serve_failures.lock().unwrap().is_empty());
        assert!(caller.failures.lock().unwrap().is_empty());
    }

    /// The winner of a session.servers collision is an ordinary entry to the chain.
    #[tokio::test]
    async fn a_hook_replaces_a_session_servers_entry_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path(), None);
        let base = "http://127.0.0.1:4321";
        let winner = plugin_http(&ep.secret, base, "acme.a", "docs", "docs");
        let mut c = host_ctx();
        c.plugin_served = vec![crate::acp::mcp_rewrite::PluginServed {
            plugin_id: "acme.a".into(),
            server: winner.clone(),
        }];
        let caller = FakeCaller::new(vec![Ok(
            json!({"add":[{"name":"docs","serve":{"prefix":false}}]}),
        )]);
        let out = run_hooks(
            Some(&caller),
            &[serving_hook("acme.z")],
            &c,
            vec![winner],
            Some(&ep),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            Rewritten {
                servers: vec![plugin_http(&ep.secret, base, "acme.z", "docs", "docs")],
                served_by: served_by(&[("docs", "acme.z")]),
            }
        );
    }

    /// An unresolvable serve entry never falls back to a same-key entry, on host and sandbox.
    #[test]
    fn unresolvable_serve_entries_take_same_key_entries_with_them() {
        let dir = tempfile::tempdir().unwrap();
        let fs = McpEntry::Serve {
            served: ServedServer::new("acme.a", "fs".into(), false),
        };
        for (label, c, ep) in [
            ("host", host_ctx(), None),
            ("sandbox", ctx(), Some(endpoint(dir.path(), None))),
        ] {
            let caller = FakeCaller::new(vec![]);
            let out = resolve_served(
                &caller,
                vec![stdio("fs").into(), fs.clone(), stdio("keep").into()],
                ep.as_ref(),
                &c,
            );
            assert_eq!(out, Rewritten::from(vec![stdio("keep")]), "{label}");
            assert_eq!(
                *caller.serve_failures.lock().unwrap(),
                ["acme.a"],
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_later_hook_discards_serve_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path(), Some("http://host.containers.internal:4321"));
        let caller = FakeCaller::new(vec![
            Ok(json!({"add":[{"name":"docs","serve":{}}]})),
            Err(WorkerRequestError::Timeout),
        ]);
        let err = run_hooks(
            Some(&caller),
            &[serving_hook("acme.a"), hook("acme.b")],
            &ctx(),
            vec![stdio("a")],
            Some(&ep),
        )
        .await
        .unwrap_err();
        assert_eq!(err.plugin, "acme.b");
        assert!(caller.serve_failures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serve_entries_from_a_plugin_without_mcp_serve_fail_the_chain() {
        let caller = FakeCaller::new(vec![Ok(json!({"add":[{"name":"docs","serve":{}}]}))]);
        let err = run_chain(&caller, &[hook("acme.a")], &ctx(), Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.plugin, "acme.a");
        assert_eq!(caller.failures.lock().unwrap().len(), 1);
    }
}
