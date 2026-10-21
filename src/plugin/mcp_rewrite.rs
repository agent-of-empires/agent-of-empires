//! Plugin-backed `McpRewriter`: asks each hooked worker to edit the forwarded MCP set.

use std::sync::Arc;
use std::time::Duration;

use aoe_plugin_api::{McpRewriteScope, PluginManifest};
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::acp::mcp_rewrite::{
    apply_edit, parse_edit, request_params, McpRewriteContext, McpRewriter, RewriteError,
    RewriteFuture, METHOD,
};
use crate::plugin::host::PluginHost;
use crate::plugin::worker_requests::WorkerRequestError;
use crate::session::mcp::project_mcp::ProjectMcpServer;

pub trait WorkerCall: Send + Sync {
    fn call<'a>(
        &'a self,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>>;

    fn report_failure(&self, plugin_id: &str, message: &str);
}

pub struct RewriteHook {
    pub plugin_id: String,
    pub timeout: Duration,
    pub order: i32,
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
    mut servers: Vec<ProjectMcpServer>,
) -> Result<Vec<ProjectMcpServer>, RewriteError> {
    for hook in hooks {
        let params = request_params(ctx, &servers);
        let edit = caller
            .call(&hook.plugin_id, METHOD, params, hook.timeout)
            .await
            .map_err(|e| e.to_string())
            .and_then(parse_edit);
        match edit {
            Ok(edit) => servers = apply_edit(servers, edit),
            Err(cause) => {
                caller.report_failure(&hook.plugin_id, &cause);
                return Err(RewriteError {
                    plugin: hook.plugin_id.clone(),
                    cause,
                });
            }
        }
    }
    Ok(servers)
}

impl WorkerCall for PluginHost {
    fn call<'a>(
        &'a self,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
        Box::pin(self.request_worker(plugin_id, method, params, timeout))
    }

    fn report_failure(&self, plugin_id: &str, message: &str) {
        self.notify_host(
            plugin_id,
            crate::plugin::ui_state::Tone::Danger,
            "MCP rewrite hook failed".to_string(),
            Some(message.to_string()),
        );
    }
}

/// A missing worker caller fails closed, but only when a hook applies.
async fn run_hooks(
    caller: Option<&dyn WorkerCall>,
    hooks: &[RewriteHook],
    ctx: &McpRewriteContext,
    servers: Vec<ProjectMcpServer>,
) -> Result<Vec<ProjectMcpServer>, RewriteError> {
    let Some(first) = hooks.first() else {
        return Ok(servers);
    };
    match caller {
        Some(caller) => run_chain(caller, hooks, ctx, servers).await,
        None => Err(RewriteError {
            plugin: first.plugin_id.clone(),
            cause: "plugin host unavailable".into(),
        }),
    }
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
            let hooks = hooks_for(registry.active().map(|p| &p.manifest), ctx.sandboxed);
            let caller = self.host.as_deref().map(|h| h as &dyn WorkerCall);
            run_hooks(caller, &hooks, ctx, servers).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::mcp::project_mcp::ProjectMcpTransport;
    use serde_json::json;
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
            "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 19\ncapabilities = [{caps}]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n{hook}"
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
                "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 19\ncapabilities = [\"runtime.worker\", \"mcp.rewrite\"]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n\n[hooks.mcp_rewrite]\norder = {order}\n"
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

    struct FakeCaller {
        replies: Mutex<Vec<Result<Value, WorkerRequestError>>>,
        seen_params: Mutex<Vec<Value>>,
        failures: Mutex<Vec<(String, String)>>,
    }

    impl FakeCaller {
        fn new(replies: Vec<Result<Value, WorkerRequestError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen_params: Mutex::default(),
                failures: Mutex::default(),
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

        fn report_failure(&self, plugin_id: &str, message: &str) {
            self.failures
                .lock()
                .unwrap()
                .push((plugin_id.to_string(), message.to_string()));
        }
    }

    fn hook(id: &str) -> RewriteHook {
        RewriteHook {
            plugin_id: id.into(),
            timeout: Duration::from_secs(1),
            order: 0,
        }
    }

    fn ctx() -> McpRewriteContext {
        McpRewriteContext {
            session_id: "s".into(),
            agent: "a".into(),
            sandboxed: true,
            profile: None,
            cwd: "/w".into(),
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
        assert_eq!(
            out.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["g"]
        );
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
        assert_eq!(out, servers);
        assert!(caller.seen_params.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_host_fails_closed_only_when_a_hook_applies() {
        let servers = vec![stdio("a")];
        let err = run_hooks(None, &[hook("acme.a")], &ctx(), servers.clone())
            .await
            .unwrap_err();
        assert_eq!(err.plugin, "acme.a");
        let out = run_hooks(None, &[], &ctx(), servers.clone()).await.unwrap();
        assert_eq!(out, servers);
    }
}
