//! Asks plugin workers which MCP servers a session gets.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use aoe_plugin_api::{
    is_valid_mcp_server_name, mcp_prefix, PluginManifest, MCP_SERVE_CAPABILITY,
    MCP_SERVE_MAX_PER_PLUGIN,
};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::Value;

use crate::acp::mcp_rewrite::{context_params, McpRewriteContext};
use crate::plugin::worker_call::WorkerCall;

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

/// The first capped entries per key.
fn served_by_key(plugin_id: &str, entries: Vec<Entry>) -> Vec<ServedServer> {
    let mut served: Vec<ServedServer> = Vec::new();
    for Entry { name, prefix } in entries {
        if served.len() == MCP_SERVE_MAX_PER_PLUGIN {
            break;
        }
        let entry = ServedServer::new(plugin_id, name, prefix);
        if !served.iter().any(|s| s.entry_key == entry.entry_key) {
            served.push(entry);
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

    let (served, involved) = drop_collisions(served);
    for id in involved {
        caller.report_serve_failure(
            &id,
            "An MCP server name collides with another plugin's; the colliding servers are skipped for this session.",
        );
    }
    served
}

/// Removes every entry whose key two plugins produce; returns the survivors and the plugins involved.
/// Defensive: unreachable while `mcp_prefix` is injective on valid plugin ids.
fn drop_collisions(served: Vec<ServedServer>) -> (Vec<ServedServer>, BTreeSet<String>) {
    let mut colliding: BTreeSet<&str> = BTreeSet::new();
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for s in &served {
        if owner
            .insert(&s.entry_key, &s.plugin_id)
            .is_some_and(|prev| prev != s.plugin_id)
        {
            colliding.insert(&s.entry_key);
        }
    }
    let involved = served
        .iter()
        .filter(|s| colliding.contains(s.entry_key.as_str()))
        .map(|s| s.plugin_id.clone())
        .collect();
    let kept = served
        .iter()
        .filter(|s| !colliding.contains(s.entry_key.as_str()))
        .cloned()
        .collect();
    (kept, involved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::mcp_rewrite::McpSandbox;
    use crate::plugin::worker_requests::WorkerRequestError;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Reply = Result<Value, WorkerRequestError>;

    #[derive(Default)]
    struct FakeCaller {
        /// Missing plugin: the call never resolves.
        replies: Mutex<HashMap<String, Reply>>,
        calls: Mutex<Vec<(String, Value)>>,
        failures: Mutex<Vec<String>>,
    }

    impl FakeCaller {
        fn with(replies: Vec<(&str, Reply)>) -> Self {
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
    }

    /// Manifest validation already rejects `mcp.serve` without a runtime worker.
    fn manifest(id: &str, serve: bool) -> PluginManifest {
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

    fn ctx(sandboxed: bool) -> McpRewriteContext {
        McpRewriteContext {
            session_id: "s1".into(),
            agent: "claude".into(),
            sandbox: sandboxed.then(|| McpSandbox {
                container_workdir: Some("/work".into()),
                mounts: Vec::new(),
            }),
            profile: Some("p".into()),
            cwd: "/w".into(),
        }
    }

    fn names(list: &[&str]) -> Reply {
        Ok(Value::Array(
            list.iter().map(|n| json!({"name": n})).collect(),
        ))
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

    /// Valid plugin ids never sanitize to the same prefix, so the host-side guard is tested directly.
    #[test]
    fn colliding_keys_from_different_plugins_are_all_dropped() {
        let mut a = served("acme.x", "same");
        a.entry_key = "shared__same".into();
        let mut b = served("acme.y", "same");
        b.entry_key = "shared__same".into();
        let own = served("acme.x", "own");
        let (kept, involved) = drop_collisions(vec![a, own.clone(), b, served("acme.z", "ok")]);
        assert_eq!(kept, [own, served("acme.z", "ok")]);
        assert_eq!(
            involved.iter().map(String::as_str).collect::<Vec<_>>(),
            ["acme.x", "acme.y"]
        );
    }

    #[test]
    fn one_plugin_repeating_a_key_is_not_a_collision() {
        let entries = vec![served("acme.x", "a"), served("acme.x", "a")];
        let (kept, involved) = drop_collisions(entries.clone());
        assert_eq!(kept, entries);
        assert!(involved.is_empty());
    }

    #[tokio::test]
    async fn no_caller_gives_nothing() {
        let ms = [manifest("acme.a", true)];
        assert!(servers_for_session(None, ms.iter(), &ctx(false))
            .await
            .is_empty());
    }
}
