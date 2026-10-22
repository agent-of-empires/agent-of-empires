//! Asks plugin workers which MCP servers a session gets.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use aoe_plugin_api::{
    is_valid_mcp_server_name, mcp_prefix, PluginManifest, MCP_SERVE_CAPABILITY,
    MCP_SERVE_MAX_PER_PLUGIN,
};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::Value;

use crate::acp::mcp_rewrite::{context_params, McpRewriteContext, PluginServed};
use crate::plugin::worker_call::WorkerCall;
use crate::plugin::worker_requests::WorkerRequestError;
use crate::server::daemon_endpoint::{plugin_route, PluginRoute};
use crate::server::plugin_mcp_socket as socket;
use crate::server::session_credential::McpSessionSecret;
use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

const METHOD: &str = "mcp.session.servers";
const CALLBACK_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedServer {
    pub plugin_id: String,
    pub name: String,
    pub entry_key: String,
}

pub async fn servers_for_session<'a>(
    caller: Option<&dyn WorkerCall>,
    manifests: impl Iterator<Item = &'a PluginManifest>,
    ctx: &McpRewriteContext,
) -> Vec<ServedServer> {
    servers_with_timeout(caller, manifests, ctx, CALLBACK_TIMEOUT).await
}

impl ServedServer {
    /// `prefix` keys the entry `<mcp_prefix(plugin id)>__<name>`, otherwise by the bare name.
    pub fn new(plugin_id: &str, name: String, prefix: bool) -> Self {
        let entry_key = if prefix {
            format!("{}__{}", mcp_prefix(plugin_id), name)
        } else {
            name.clone()
        };
        Self {
            plugin_id: plugin_id.to_string(),
            name,
            entry_key,
        }
    }

    /// Keyed under its plugin's own prefix.
    pub fn prefixed(&self) -> bool {
        self.entry_key != self.name
    }
}

/// Serde default for the `prefix` flag of a served entry.
pub(crate) const fn prefix_by_default() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    #[serde(default = "prefix_by_default")]
    prefix: bool,
}

/// One plugin's entries with valid names, in reply order.
fn parse_names(reply: Value) -> Result<Vec<Entry>, serde_json::Error> {
    let entries: Vec<Entry> = serde_json::from_value(reply)?;
    Ok(entries
        .into_iter()
        .filter(|e| is_valid_mcp_server_name(&e.name))
        .collect())
}

/// One entry per key, the first capped keys in reply order. A prefixed entry beats an
/// unprefixed one on the same key, otherwise the first in reply order wins.
fn served_by_key(plugin_id: &str, entries: Vec<Entry>) -> Vec<ServedServer> {
    let mut served: Vec<ServedServer> = Vec::new();
    for Entry { name, prefix } in entries {
        let entry = ServedServer::new(plugin_id, name, prefix);
        match served.iter().position(|s| s.entry_key == entry.entry_key) {
            Some(i) => {
                if entry.prefixed() && !served[i].prefixed() {
                    served[i] = entry;
                }
            }
            None if served.len() < MCP_SERVE_MAX_PER_PLUGIN => served.push(entry),
            None => {}
        }
    }
    served
}

async fn ask(
    caller: &dyn WorkerCall,
    plugin_id: &str,
    ctx: &McpRewriteContext,
    timeout: Duration,
) -> Option<Vec<Entry>> {
    // The outer timeout bounds a caller that ignores the `call` argument.
    let reply = tokio::time::timeout(
        timeout,
        caller.call(plugin_id, METHOD, context_params(ctx), timeout),
    )
    .await;
    let names = match reply {
        Ok(Ok(value)) => parse_names(value).ok(),
        Ok(Err(WorkerRequestError::MethodNotFound)) => return Some(Vec::new()),
        Ok(Err(_)) | Err(_) => None,
    };
    if names.is_none() {
        caller.report_serve_failure(
            plugin_id,
            "The plugin did not return a valid MCP server list; its servers are skipped for this session.",
        );
    }
    names
}

async fn servers_with_timeout<'a>(
    caller: Option<&dyn WorkerCall>,
    manifests: impl Iterator<Item = &'a PluginManifest>,
    ctx: &McpRewriteContext,
    timeout: Duration,
) -> Vec<ServedServer> {
    let Some(caller) = caller else {
        return Vec::new();
    };
    let mut ids: Vec<&str> = manifests
        .filter(|m| m.runtime.is_some())
        .filter(|m| {
            m.capabilities
                .iter()
                .any(|c| c.as_str() == MCP_SERVE_CAPABILITY)
        })
        .map(|m| m.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();

    let replies = join_all(ids.iter().map(|id| ask(caller, id, ctx, timeout))).await;
    let mut served: Vec<ServedServer> = Vec::new();
    for (id, entries) in ids.iter().zip(replies) {
        served.extend(served_by_key(id, entries.unwrap_or_default()));
    }

    served.sort_by(|a, b| {
        (&a.plugin_id, &a.name, &a.entry_key).cmp(&(&b.plugin_id, &b.name, &b.entry_key))
    });

    let (served, losers) = drop_collisions(served);
    for (id, keys) in losers {
        caller.report_serve_failure(
            &id,
            &format!(
                "These MCP server keys collide with another plugin's servers and are skipped for this session: {}.",
                keys.join(", ")
            ),
        );
    }
    served
}

/// The plugin entries for a session, and the plugins told it cannot reach them.
/// Workers are asked even without a route, so only plugins that offered servers are told.
pub async fn session_plugin_entries(ctx: &McpRewriteContext) -> (Vec<PluginServed>, Vec<String>) {
    let served = crate::acp::mcp_rewrite::served_servers(ctx).await;
    let endpoint = crate::server::daemon_endpoint::daemon_endpoint();
    match plugin_route(endpoint.as_deref(), ctx.sandbox_socket()) {
        Ok((secret, route)) => (
            plugin_entries(secret, route, &ctx.session_id, &served),
            Vec::new(),
        ),
        Err(cause) => {
            let mut plugins: Vec<String> = served.into_iter().map(|s| s.plugin_id).collect();
            plugins.dedup();
            for plugin_id in &plugins {
                crate::acp::mcp_rewrite::report_serve_failure(plugin_id, cause.notice());
            }
            (Vec::new(), plugins)
        }
    }
}

/// One entry per served server, authenticated with the credential for exactly that server.
pub fn plugin_entries(
    secret: &McpSessionSecret,
    route: PluginRoute<'_>,
    session_id: &str,
    served: &[ServedServer],
) -> Vec<PluginServed> {
    // Plugin ids and validated server names are URL-path safe as they are.
    served
        .iter()
        .map(|s| {
            let credential = secret.plugin_server_credential_for(&s.plugin_id, &s.name, session_id);
            let transport = match route {
                PluginRoute::Http(base_url) => ProjectMcpTransport::Http {
                    url: format!("{base_url}/mcp/plugins/{}/{}", s.plugin_id, s.name),
                    headers: [
                        ("Authorization".to_string(), format!("Bearer {credential}")),
                        ("X-AoE-Session".to_string(), session_id.to_string()),
                    ]
                    .into(),
                },
                PluginRoute::Socket => ProjectMcpTransport::Stdio {
                    command: "node".to_string(),
                    args: vec![
                        socket::container_forwarder(),
                        socket::container_socket(),
                        s.plugin_id.clone(),
                        s.name.clone(),
                    ],
                    env: [
                        (socket::TOKEN_ENV.to_string(), credential),
                        (socket::SESSION_ENV.to_string(), session_id.to_string()),
                    ]
                    .into(),
                },
            };
            PluginServed {
                plugin_id: s.plugin_id.clone(),
                server: ProjectMcpServer {
                    name: s.entry_key.clone(),
                    transport,
                },
            }
        })
        .collect()
}

/// A key several plugins produce goes to a prefixed entry over an unprefixed one, so no
/// plugin can take another's namespace, then to the lowest plugin id, since serving has no
/// `order`. Returns the survivors and each losing plugin's lost keys.
fn drop_collisions(
    served: Vec<ServedServer>,
) -> (Vec<ServedServer>, BTreeMap<String, Vec<String>>) {
    let rank = |s: &ServedServer| (!s.prefixed(), s.plugin_id.clone());
    let mut winner: HashMap<String, (bool, String)> = HashMap::new();
    for s in &served {
        let best = winner.entry(s.entry_key.clone()).or_insert_with(|| rank(s));
        if rank(s) < *best {
            *best = rank(s);
        }
    }
    let mut losers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let kept = served
        .into_iter()
        .filter(|s| {
            let won = winner[&s.entry_key].1 == s.plugin_id;
            if !won {
                losers
                    .entry(s.plugin_id.clone())
                    .or_default()
                    .push(s.entry_key.clone());
            }
            won
        })
        .collect();
    (kept, losers)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::acp::mcp_rewrite::{McpEntry, McpRewriter, RewriteError, RewriteFuture};
    use crate::session::mcp::project_mcp::ProjectMcpServer;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub(crate) type Reply = Result<Value, WorkerRequestError>;

    #[derive(Default)]
    pub(crate) struct FakeCaller {
        /// Missing plugin: the call never resolves.
        replies: Mutex<HashMap<String, Reply>>,
        pub(crate) calls: Mutex<Vec<(String, Value)>>,
        pub(crate) failures: Mutex<Vec<String>>,
        pub(crate) notices: Mutex<Vec<(String, String)>>,
    }

    impl FakeCaller {
        pub(crate) fn with(replies: Vec<(&str, Reply)>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().map(|(p, r)| (p.into(), r)).collect()),
                ..Self::default()
            }
        }
    }

    impl WorkerCall for FakeCaller {
        fn call<'a>(
            &'a self,
            plugin_id: &'a str,
            method: &'a str,
            params: Value,
            _timeout: Duration,
        ) -> BoxFuture<'a, Reply> {
            assert_eq!(method, "mcp.session.servers");
            self.calls.lock().unwrap().push((plugin_id.into(), params));
            match self.replies.lock().unwrap().remove(plugin_id) {
                Some(reply) => Box::pin(async move { reply }),
                None => Box::pin(std::future::pending()),
            }
        }

        fn request_id(&self) -> String {
            unreachable!()
        }

        fn call_as<'a>(
            &'a self,
            _id: &'a str,
            _plugin_id: &'a str,
            _method: &'a str,
            _params: Value,
            _start_wait: Duration,
            _timeout: Duration,
        ) -> BoxFuture<'a, Reply> {
            unreachable!()
        }

        fn notify<'a>(
            &'a self,
            _plugin_id: &'a str,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, bool> {
            unreachable!()
        }

        fn report_failure(&self, _plugin_id: &str, _message: &str) {
            unreachable!()
        }

        fn report_serve_failure(&self, plugin_id: &str, _message: &str) {
            self.failures.lock().unwrap().push(plugin_id.into());
        }

        fn report_serve_notice(&self, plugin_id: &str, message: &str) {
            self.notices
                .lock()
                .unwrap()
                .push((plugin_id.into(), message.into()));
        }
    }

    /// Manifest validation already rejects `mcp.serve` without a runtime worker.
    pub(crate) fn manifest(id: &str, serve: bool) -> PluginManifest {
        let caps = if serve {
            "\"runtime.worker\", \"mcp.serve\""
        } else {
            "\"runtime.worker\""
        };
        PluginManifest::from_toml_str(&format!(
            "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 22\ncapabilities = [{caps}]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n"
        ))
        .unwrap()
    }

    pub(crate) fn names(list: &[&str]) -> Reply {
        Ok(Value::Array(
            list.iter().map(|n| json!({"name": n})).collect(),
        ))
    }

    /// Serves through the real callback against a fake worker, records each hook request,
    /// and passes the servers through (or fails the hook).
    pub(crate) struct ServingRewriter {
        caller: FakeCaller,
        manifests: Vec<PluginManifest>,
        fail_hook: bool,
        pub(crate) requests: Mutex<Vec<Value>>,
        skipped: Mutex<Vec<(String, String)>>,
    }

    impl ServingRewriter {
        pub(crate) fn new(replies: Vec<(&str, Reply)>, fail_hook: bool) -> std::sync::Arc<Self> {
            let manifests = replies.iter().map(|(id, _)| manifest(id, true)).collect();
            std::sync::Arc::new(Self {
                caller: FakeCaller::with(replies),
                manifests,
                fail_hook,
                requests: Mutex::new(Vec::new()),
                skipped: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn worker_calls(&self) -> usize {
            self.caller.calls.lock().unwrap().len()
        }

        pub(crate) fn notices(&self) -> Vec<(String, String)> {
            self.caller.notices.lock().unwrap().clone()
        }

        /// The `report_serve_failure` calls, `(plugin, message)`.
        pub(crate) fn skipped(&self) -> Vec<(String, String)> {
            self.skipped.lock().unwrap().clone()
        }
    }

    impl McpRewriter for ServingRewriter {
        fn rewrite<'a>(
            &'a self,
            ctx: &'a McpRewriteContext,
            servers: Vec<ProjectMcpServer>,
        ) -> RewriteFuture<'a> {
            let entries: Vec<McpEntry> = servers.iter().cloned().map(McpEntry::from).collect();
            let contributors = crate::acp::mcp_rewrite::initial_contributors(ctx, &servers);
            self.requests
                .lock()
                .unwrap()
                .push(crate::acp::mcp_rewrite::request_params(
                    ctx,
                    &entries,
                    &contributors,
                ));
            let out = if self.fail_hook {
                Err(RewriteError {
                    plugin: "acme.hook".into(),
                    cause: "failed".into(),
                })
            } else {
                Ok(servers.into())
            };
            Box::pin(std::future::ready(out))
        }

        fn served_servers<'a>(
            &'a self,
            ctx: &'a McpRewriteContext,
        ) -> BoxFuture<'a, Vec<ServedServer>> {
            Box::pin(servers_for_session(
                Some(&self.caller),
                self.manifests.iter(),
                ctx,
            ))
        }

        fn report_serve_notice(&self, plugin_id: &str, message: &str) {
            self.caller.report_serve_notice(plugin_id, message);
        }

        fn report_serve_failure(&self, plugin_id: &str, message: &str) {
            self.skipped
                .lock()
                .unwrap()
                .push((plugin_id.into(), message.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::acp::mcp_rewrite::McpSandbox;
    use serde_json::json;

    fn ctx(sandboxed: bool) -> McpRewriteContext {
        McpRewriteContext {
            session_id: "s1".into(),
            agent: "claude".into(),
            sandbox: sandboxed.then(|| McpSandbox {
                container_workdir: Some("/work".into()),
                mounts: Vec::new(),
                plugin_socket: crate::server::daemon_endpoint::SandboxSocket::Reachable,
            }),
            profile: Some("p".into()),
            cwd: "/w".into(),
            plugin_served: Vec::new(),
            unreachable_notified: Vec::new(),
        }
    }

    async fn run(
        caller: &FakeCaller,
        manifests: &[PluginManifest],
        timeout: Duration,
    ) -> Vec<ServedServer> {
        servers_with_timeout(Some(caller), manifests.iter(), &ctx(false), timeout).await
    }

    const SHORT: Duration = Duration::from_millis(50);

    fn keys(out: &[ServedServer]) -> Vec<&str> {
        out.iter().map(|s| s.entry_key.as_str()).collect()
    }

    #[tokio::test]
    async fn names_become_prefixed_entries_ordered_by_plugin_then_name() {
        let caller = FakeCaller::with(vec![
            ("acme.b", names(&["z", "a"])),
            ("acme.a", names(&["m"])),
        ]);
        let ms = [manifest("acme.b", true), manifest("acme.a", true)];
        let out = run(&caller, &ms, CALLBACK_TIMEOUT).await;
        assert_eq!(keys(&out), ["acme_a__m", "acme_b__a", "acme_b__z"]);
        assert_eq!(out[0].plugin_id, "acme.a");
        assert_eq!(out[0].name, "m");
        assert!(caller.failures.lock().unwrap().is_empty());
    }

    /// `prefix` defaults to true; `false` keys the entry by the bare name. Dedup is by key.
    #[tokio::test]
    async fn prefix_chooses_the_entry_key() {
        let cases = [
            ("default", json!([{"name": "a"}]), vec![("a", "acme_a__a")]),
            (
                "explicit true",
                json!([{"name": "a", "prefix": true}]),
                vec![("a", "acme_a__a")],
            ),
            (
                "explicit false",
                json!([{"name": "a", "prefix": false}]),
                vec![("a", "a")],
            ),
            (
                "both forms are distinct keys",
                json!([{"name": "a", "prefix": false}, {"name": "a"}]),
                vec![("a", "a"), ("a", "acme_a__a")],
            ),
            (
                "same key twice",
                json!([{"name": "a", "prefix": false}, {"name": "a", "prefix": false}]),
                vec![("a", "a")],
            ),
            (
                "prefixed beats unprefixed on one key, prefixed first",
                json!([{"name": "x"}, {"name": "acme_a__x", "prefix": false}]),
                vec![("x", "acme_a__x")],
            ),
            (
                "prefixed beats unprefixed on one key, unprefixed first",
                json!([{"name": "acme_a__x", "prefix": false}, {"name": "x"}]),
                vec![("x", "acme_a__x")],
            ),
        ];
        for (label, reply, want) in cases {
            let caller = FakeCaller::with(vec![("acme.a", Ok(reply))]);
            let out = run(&caller, &[manifest("acme.a", true)], CALLBACK_TIMEOUT).await;
            let got: Vec<(&str, &str)> = out
                .iter()
                .map(|s| (s.name.as_str(), s.entry_key.as_str()))
                .collect();
            assert_eq!(got, want, "{label}");
            assert!(out.iter().all(|s| s.plugin_id == "acme.a"), "{label}");
            assert!(caller.failures.lock().unwrap().is_empty(), "{label}");
        }
    }

    #[tokio::test]
    async fn empty_list_gives_nothing_without_notification() {
        let caller = FakeCaller::with(vec![("acme.a", names(&[]))]);
        let out = run(&caller, &[manifest("acme.a", true)], CALLBACK_TIMEOUT).await;
        assert!(out.is_empty());
        assert!(caller.failures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_names_are_dropped_and_duplicates_deduped() {
        let caller = FakeCaller::with(vec![(
            "acme.a",
            names(&["ok", "bad name", "a/b", "ok", ""]),
        )]);
        let out = run(&caller, &[manifest("acme.a", true)], CALLBACK_TIMEOUT).await;
        assert_eq!(keys(&out), ["acme_a__ok"]);
    }

    #[tokio::test]
    async fn only_the_first_64_valid_names_are_kept() {
        let all: Vec<String> = (0..65).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let caller = FakeCaller::with(vec![("acme.a", names(&refs))]);
        let out = run(&caller, &[manifest("acme.a", true)], CALLBACK_TIMEOUT).await;
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|s| s.name != "s64"));
    }

    #[tokio::test]
    async fn failures_contribute_nothing_and_notify_once() {
        let cases: Vec<(&str, Option<Reply>)> = vec![
            ("error", Some(Err(WorkerRequestError::Rejected))),
            ("no worker", Some(Err(WorkerRequestError::NoWorker))),
            ("timeout", None),
            ("not an array", Some(Ok(json!({"name": "a"})))),
            ("string entries", Some(Ok(json!(["a"])))),
            (
                "unknown field",
                Some(Ok(json!([{"name": "a", "url": "x"}]))),
            ),
            ("missing name", Some(Ok(json!([{}])))),
            (
                "shadow is no longer a field",
                Some(Ok(json!([{"name": "a", "shadow": true}]))),
            ),
            (
                "prefix not a bool",
                Some(Ok(json!([{"name": "a", "prefix": "no"}]))),
            ),
            (
                "prefix null",
                Some(Ok(json!([{"name": "a", "prefix": null}]))),
            ),
        ];
        for (label, reply) in cases {
            let replies = reply.map(|r| vec![("acme.a", r)]).unwrap_or_default();
            let caller = FakeCaller::with(replies);
            let out = run(&caller, &[manifest("acme.a", true)], SHORT).await;
            assert!(out.is_empty(), "{label}");
            assert_eq!(*caller.failures.lock().unwrap(), ["acme.a"], "{label}");
        }
    }

    /// A rewrite-only worker that never implemented the method offers nothing, silently.
    #[tokio::test]
    async fn method_not_found_offers_nothing_without_notification() {
        let caller = FakeCaller::with(vec![
            ("acme.a", Err(WorkerRequestError::MethodNotFound)),
            ("acme.b", Err(WorkerRequestError::Rejected)),
        ]);
        let ms = [manifest("acme.a", true), manifest("acme.b", true)];
        let out = run(&caller, &ms, CALLBACK_TIMEOUT).await;
        assert!(out.is_empty());
        assert_eq!(*caller.failures.lock().unwrap(), ["acme.b"]);
    }

    #[tokio::test]
    async fn one_failing_plugin_does_not_affect_another() {
        let caller = FakeCaller::with(vec![
            ("acme.bad", Err(WorkerRequestError::Rejected)),
            ("acme.good", names(&["g"])),
        ]);
        let ms = [manifest("acme.bad", true), manifest("acme.good", true)];
        let out = run(&caller, &ms, CALLBACK_TIMEOUT).await;
        assert_eq!(keys(&out), ["acme_good__g"]);
        assert_eq!(*caller.failures.lock().unwrap(), ["acme.bad"]);
    }

    #[tokio::test]
    async fn request_params_describe_the_session() {
        let caller = FakeCaller::with(vec![("acme.a", names(&[]))]);
        let ms = [manifest("acme.a", true)];
        servers_with_timeout(Some(&caller), ms.iter(), &ctx(true), CALLBACK_TIMEOUT).await;
        let (plugin, params) = caller.calls.lock().unwrap()[0].clone();
        assert_eq!(plugin, "acme.a");
        assert_eq!(
            params,
            json!({
                "session_id": "s1",
                "agent": "claude",
                "sandboxed": true,
                "profile": "p",
                "cwd": "/w",
                "sandbox": {"container_workdir": "/work", "mounts": []},
            })
        );

        let caller = FakeCaller::with(vec![("acme.a", names(&[]))]);
        run(&caller, &ms, CALLBACK_TIMEOUT).await;
        let params = caller.calls.lock().unwrap()[0].1.clone();
        assert_eq!(params["sandboxed"], false);
        assert!(params.get("sandbox").is_none());
    }

    #[tokio::test]
    async fn plugins_without_the_capability_are_never_called() {
        let caller = FakeCaller::default();
        let ms = [manifest("acme.nocap", false)];
        let out = run(&caller, &ms, CALLBACK_TIMEOUT).await;
        assert!(out.is_empty());
        assert!(caller.calls.lock().unwrap().is_empty());
        assert!(caller.failures.lock().unwrap().is_empty());
    }

    fn served(plugin_id: &str, name: &str) -> ServedServer {
        ServedServer::new(plugin_id, name.into(), true)
    }

    /// Input order does not decide the winner.
    #[test]
    fn colliding_keys_prefer_prefixed_entries_then_the_lowest_plugin_id() {
        let bare = |plugin: &str, name: &str| ServedServer::new(plugin, name.into(), false);
        // The prefixed key `acme_b__x` as a bare name of a lower plugin id.
        let squatter = bare("acme.a", "acme_b__x");
        // Different plugins never share a prefixed key; forced here to check it is harmless.
        let forced = |plugin: &str| ServedServer {
            entry_key: "acme_q__p".into(),
            ..served(plugin, "p")
        };
        let own = served("acme.y", "own");
        let cases = [
            (
                "prefixed beats a lower-id unprefixed",
                vec![squatter.clone(), served("acme.b", "x")],
                vec![served("acme.b", "x")],
                vec![("acme.a", vec!["acme_b__x"])],
            ),
            (
                "two unprefixed: lowest id",
                vec![
                    bare("acme.z", "same"),
                    own.clone(),
                    bare("acme.x", "same"),
                    bare("acme.y", "same"),
                ],
                vec![own.clone(), bare("acme.x", "same")],
                vec![("acme.y", vec!["same"]), ("acme.z", vec!["same"])],
            ),
            (
                "two prefixed: lowest id, no panic",
                vec![forced("acme.r"), forced("acme.q")],
                vec![forced("acme.q")],
                vec![("acme.r", vec!["acme_q__p"])],
            ),
        ];
        for (label, input, want_kept, want_losers) in cases {
            let (kept, losers) = drop_collisions(input);
            assert_eq!(kept, want_kept, "{label}");
            let losers: Vec<(&str, Vec<&str>)> = losers
                .iter()
                .map(|(p, keys)| (p.as_str(), keys.iter().map(String::as_str).collect()))
                .collect();
            assert_eq!(losers, want_losers, "{label}");
        }
    }

    /// Each loser loses only the contested entry and is told once.
    #[tokio::test]
    async fn a_contested_key_goes_to_the_lowest_plugin_id() {
        let caller = FakeCaller::with(vec![
            (
                "acme.b",
                Ok(json!([{"name": "docs", "prefix": false}, {"name": "own"}])),
            ),
            (
                "acme.a",
                Ok(json!([{"name": "docs", "prefix": false}, {"name": "x"}])),
            ),
            (
                "acme.c",
                Ok(
                    json!([{"name": "acme_a__x", "prefix": false}, {"name": "docs", "prefix": false}]),
                ),
            ),
        ]);
        let ms = [
            manifest("acme.c", true),
            manifest("acme.b", true),
            manifest("acme.a", true),
        ];
        let out = run(&caller, &ms, CALLBACK_TIMEOUT).await;
        let got: Vec<(&str, &str)> = out
            .iter()
            .map(|s| (s.plugin_id.as_str(), s.entry_key.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("acme.a", "docs"),
                ("acme.a", "acme_a__x"),
                ("acme.b", "acme_b__own"),
            ]
        );
        assert_eq!(*caller.failures.lock().unwrap(), ["acme.b", "acme.c"]);
    }

    #[test]
    fn one_plugin_repeating_a_key_is_not_a_collision() {
        let entries = vec![served("acme.x", "a"), served("acme.x", "a")];
        let (kept, involved) = drop_collisions(entries.clone());
        assert_eq!(kept, entries);
        assert!(involved.is_empty());
    }

    #[test]
    fn plugin_entries_point_at_the_daemon_with_the_server_credential() {
        use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};

        let dir = tempfile::tempdir().unwrap();
        let secret = McpSessionSecret::load_or_generate(&dir.path().join("secret")).unwrap();
        let out = plugin_entries(
            &secret,
            PluginRoute::Http("http://127.0.0.1:4321"),
            "sess-1",
            &[served("acme.a", "docs"), served("acme.b", "git.v2")],
        );
        let entry = |plugin: &str, name: &str| PluginServed {
            plugin_id: plugin.into(),
            server: ProjectMcpServer {
                name: format!("{}__{}", mcp_prefix(plugin), name),
                transport: ProjectMcpTransport::Http {
                    url: format!("http://127.0.0.1:4321/mcp/plugins/{plugin}/{name}"),
                    headers: [
                        (
                            "Authorization".to_string(),
                            format!(
                                "Bearer {}",
                                secret.plugin_server_credential_for(plugin, name, "sess-1")
                            ),
                        ),
                        ("X-AoE-Session".to_string(), "sess-1".to_string()),
                    ]
                    .into(),
                },
            },
        };
        assert_eq!(out, [entry("acme.a", "docs"), entry("acme.b", "git.v2")]);
    }

    /// Sandboxes reach the socket through the mounted forwarder; the credential travels in
    /// env, never in argv.
    #[test]
    fn socket_entries_run_the_forwarder_with_the_server_credential() {
        use crate::server::plugin_mcp_socket::{SESSION_ENV, TOKEN_ENV};

        let dir = tempfile::tempdir().unwrap();
        let secret = McpSessionSecret::load_or_generate(&dir.path().join("secret")).unwrap();
        let docs = ServedServer::new("acme.a", "docs".into(), false);
        let out = plugin_entries(&secret, PluginRoute::Socket, "sess-1", &[docs]);
        let credential = secret.plugin_server_credential_for("acme.a", "docs", "sess-1");
        assert_eq!(
            out,
            [PluginServed {
                plugin_id: "acme.a".into(),
                server: ProjectMcpServer {
                    name: "docs".into(),
                    transport: ProjectMcpTransport::Stdio {
                        command: "node".into(),
                        args: vec![
                            "/aoe/plugin-mcp/forward.mjs".into(),
                            "/aoe/plugin-mcp/mcp.sock".into(),
                            "acme.a".into(),
                            "docs".into(),
                        ],
                        env: [
                            (TOKEN_ENV.to_string(), credential),
                            (SESSION_ENV.to_string(), "sess-1".to_string()),
                        ]
                        .into(),
                    },
                },
            }]
        );
    }

    #[tokio::test]
    async fn no_caller_gives_nothing() {
        let ms = [manifest("acme.a", true)];
        assert!(servers_for_session(None, ms.iter(), &ctx(false))
            .await
            .is_empty());
    }
}
