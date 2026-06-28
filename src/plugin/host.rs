//! The Tier 1 plugin worker host: launch and supervise plugin workers.
//!
//! The host runs inside the `aoe serve` daemon. For each active community
//! plugin that declares a `[runtime]`, it resolves a launch
//! ([`crate::plugin::launch::resolve_launch`]), applies the sandbox backend
//! ([`crate::plugin::sandbox`]), and spawns the worker as a child process that
//! speaks newline-delimited JSON-RPC ([`crate::plugin::protocol`]) over its
//! stdio. Each worker call is checked against the plugin's granted
//! capabilities and dispatched to the host API
//! ([`crate::plugin::host_api`]).
//!
//! Supervision is the ACP supervision model minus the persistence half: the
//! worker is a child owned by this daemon, not a detached process. There is no
//! socket, no on-disk runner record, and no reattach: a plugin worker is a
//! stateless transformer over a host-owned event stream, so surviving a daemon
//! restart would only strand it with a stale view. The daemon dies, the
//! workers die, and a fresh daemon respawns them. What is kept from ACP:
//! process-group reaping (a worker that forks helpers is torn down whole), a
//! per-worker respawn budget so a crash loop does not spin, and a concurrency
//! cap. The worker's stderr drains to `<plugin-workers>/<id>.log`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aoe_plugin_api::UiSlot;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use crate::plugin::host_api::{dispatch, HostApiState, PluginRpcContext};
use crate::plugin::launch::{resolve_launch, OsLaunchResolver};
use crate::plugin::protocol::{self, codes, RpcResponse};
use crate::plugin::registry::PluginRegistry;
use crate::plugin::sandbox::{NoSandbox, SandboxBackend};
use crate::process::worker;

/// Events kept per topic on the plugin event bus before the oldest are pruned.
const EVENT_RETENTION_PER_TOPIC: usize = 10_000;
/// Most workers the host runs at once. A cooperative cap, not a security one.
const MAX_WORKERS: usize = 32;
/// Respawn budget: at most this many restarts within [`RESPAWN_WINDOW`] before
/// the host gives up on a crash-looping worker.
const MAX_RESPAWNS: usize = 3;
const RESPAWN_WINDOW: Duration = Duration::from_secs(60);
/// Grace period between SIGTERM and SIGKILL when reaping a worker tree.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// One supervised worker: the plugin it runs, its pid, and the task driving it.
struct RunningWorker {
    pid: u32,
    task: tokio::task::JoinHandle<()>,
    /// Sender into the worker's stdin, set while a worker generation is being
    /// served. Lets the host push an unsolicited request (a notification) to the
    /// worker, e.g. a UI action forwarded from the dashboard. `None` between
    /// spawns. See [`PluginHost::notify_worker`].
    inbound: Option<mpsc::UnboundedSender<String>>,
}

/// The plugin worker host, owned by the daemon for its lifetime.
pub struct PluginHost {
    api: Arc<HostApiState>,
    sandbox: Arc<dyn SandboxBackend>,
    workers_dir: PathBuf,
    /// Running workers keyed by plugin id (one worker per plugin in v1).
    running: Mutex<HashMap<String, RunningWorker>>,
}

impl PluginHost {
    /// Build a host bound to `app_dir` (where the worker logs and the plugin
    /// event-bus database live) and `profile` (whose session storage the host
    /// API reads and writes). The only v1 sandbox backend is [`NoSandbox`].
    pub fn new(app_dir: &std::path::Path, profile: &str) -> Result<Arc<Self>> {
        let workers_dir = app_dir.join("plugin-workers");
        worker::ensure_dir(&workers_dir)
            .with_context(|| format!("prepare {}", workers_dir.display()))?;
        let api = HostApiState::open(
            &app_dir.join("plugin_events.db"),
            profile,
            EVENT_RETENTION_PER_TOPIC,
        )?;
        Ok(Arc::new(Self {
            api: Arc::new(api),
            sandbox: Arc::new(NoSandbox),
            workers_dir,
            running: Mutex::new(HashMap::new()),
        }))
    }

    /// The aggregated UI-state snapshot for the web to render. Read
    /// synchronously off the in-memory store; never awaits a worker.
    pub fn ui_snapshot(&self) -> crate::plugin::ui_state::UiSnapshot {
        self.api.ui_snapshot()
    }

    /// The UI mutation counter for one `(plugin, session)` scope. The action
    /// endpoint reads this for the clicked pane's session before forwarding, so
    /// the dashboard can hold that pane's spinner until its re-pushed state
    /// moves the counter off this baseline.
    pub fn ui_revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.api.ui_revision(plugin_id, session_id)
    }

    /// Push a host-originated notification onto the ring (e.g. the auto-update
    /// sweep telling the user an update needs approval). Best-effort.
    pub fn notify_host(
        &self,
        plugin_id: &str,
        tone: crate::plugin::ui_state::Tone,
        title: String,
        body: Option<String>,
    ) {
        self.api.notify_host(plugin_id, tone, title, body);
    }

    /// Push a fire-and-forget JSON-RPC notification (no id, so the worker sends
    /// no reply) to a running worker's stdin. Used to forward a dashboard UI
    /// action (e.g. a pane's "Refresh" button) to the worker method the plugin
    /// named for it. Returns `false` if the plugin has no live worker. The
    /// worker is the trust boundary: it acts only on methods it implements and
    /// ignores the rest (the honest-plugin model, D8).
    pub async fn notify_worker(&self, plugin_id: &str, method: &str, params: Value) -> bool {
        let line = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params })
            .to_string()
            + "\n";
        let running = self.running.lock().await;
        match running.get(plugin_id).and_then(|w| w.inbound.as_ref()) {
            Some(tx) => tx.send(line).is_ok(),
            None => false,
        }
    }

    /// Launch a worker for every active plugin that declares a runtime, up to
    /// the concurrency cap. A plugin whose runtime cannot be resolved (a
    /// missing interpreter or binary) is logged and skipped; it does not block
    /// the others.
    pub async fn start(self: &Arc<Self>, registry: &PluginRegistry) {
        let candidates: Vec<String> = registry
            .active()
            .filter(|p| p.manifest.runtime.is_some())
            .map(|p| p.id().to_string())
            .collect();
        tracing::info!(
            target: "plugin.host",
            count = candidates.len(),
            "launching plugin workers"
        );
        for id in candidates {
            if self.running.lock().await.len() >= MAX_WORKERS {
                tracing::warn!(
                    target: "plugin.host",
                    cap = MAX_WORKERS,
                    "plugin worker concurrency cap reached; not launching {id}"
                );
                break;
            }
            self.clone().launch(id).await;
        }
    }

    /// Spawn the supervising task for one plugin id. The task spawns the worker
    /// process, runs its protocol loop, and respawns within the budget if it
    /// exits.
    async fn launch(self: Arc<Self>, plugin_id: String) {
        let host = self.clone();
        let id_for_task = plugin_id.clone();
        // Hold the lock across spawn and insert so `supervise` (which updates
        // the pid under the same lock) cannot run its first `spawn_once` before
        // the placeholder entry exists. Otherwise the pid update would no-op and
        // shutdown could never reap the worker.
        let mut running = self.running.lock().await;
        let task = tokio::spawn(async move {
            host.supervise(id_for_task).await;
        });
        running.insert(
            plugin_id,
            RunningWorker {
                pid: 0,
                task,
                inbound: None,
            },
        );
    }

    /// Drive one plugin's worker: spawn, serve, respawn within budget.
    async fn supervise(self: Arc<Self>, plugin_id: String) {
        let mut restarts: Vec<Instant> = Vec::new();
        loop {
            match self.spawn_once(&plugin_id).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(
                        target: "plugin.host",
                        plugin = %plugin_id,
                        "failed to launch plugin worker: {e:#}"
                    );
                    // Drop the entry so a dead worker does not count against the
                    // concurrency cap or block a later retry.
                    self.running.lock().await.remove(&plugin_id);
                    return;
                }
            }
            let now = Instant::now();
            restarts.retain(|t| now.duration_since(*t) < RESPAWN_WINDOW);
            restarts.push(now);
            if restarts.len() > MAX_RESPAWNS {
                tracing::error!(
                    target: "plugin.host",
                    plugin = %plugin_id,
                    "plugin worker exceeded respawn budget ({MAX_RESPAWNS} in {}s); giving up",
                    RESPAWN_WINDOW.as_secs()
                );
                self.running.lock().await.remove(&plugin_id);
                return;
            }
            tracing::warn!(
                target: "plugin.host",
                plugin = %plugin_id,
                "plugin worker exited; respawning"
            );
        }
    }

    /// Spawn the worker process once and run its protocol loop until it exits.
    /// Resolution and the granted-capability list are recomputed from the live
    /// registry each spawn so an enable/disable/regrant between restarts is
    /// honored.
    async fn spawn_once(&self, plugin_id: &str) -> Result<()> {
        let registry = crate::plugin::registry();
        let plugin = registry
            .get(plugin_id)
            .filter(|p| p.active())
            .ok_or_else(|| anyhow::anyhow!("plugin {plugin_id} is no longer active"))?;
        let granted: Vec<String> = plugin
            .manifest
            .capabilities
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        // The slots the plugin may push into; gates every ui.state.* call.
        let ui_contributions: HashSet<(UiSlot, String)> = plugin
            .manifest
            .ui
            .iter()
            .map(|u| (u.slot, u.id.clone()))
            .collect();

        let launch = resolve_launch(plugin, &OsLaunchResolver)?;
        let prepared = self.sandbox.prepare(&launch)?;

        let worker_id = uuid::Uuid::new_v4().to_string();
        let log_path = worker::log_path(&self.workers_dir, &worker_id)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open worker log {}", log_path.display()))?;

        let mut cmd = tokio::process::Command::new(&prepared.program);
        cmd.args(&prepared.args)
            .current_dir(&prepared.cwd)
            .env("AOE_PLUGIN_WORKER_ID", &worker_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        for (k, v) in &prepared.env {
            cmd.env(k, v);
        }
        // New session so the worker and any helpers it forks share one process
        // group, reapable in one signal.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn worker for {plugin_id}"))?;
        let pid = child.id().unwrap_or(0);
        tracing::info!(
            target: "plugin.host",
            plugin = %plugin_id,
            pid,
            program = %prepared.program.display(),
            "launched plugin worker"
        );
        if let Some(w) = self.running.lock().await.get_mut(plugin_id) {
            w.pid = pid;
        }

        let stdin = child.stdin.take().context("worker stdin missing")?;
        let stdout = child.stdout.take().context("worker stdout missing")?;

        // One task owns stdin; both the RPC response path and host-initiated
        // pushes (notify_worker) feed it through this channel, so there is a
        // single writer. Registered in `running` so notify_worker can reach it.
        // ponytail: unbounded by design. A worker that stops draining stdin
        // could let lines accumulate, but under the honest-plugin trust model
        // (D8) that is out of scope; bound this channel if a real backpressure
        // need shows up.
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = inbound_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break; // worker closed stdin; nothing more to send.
                }
            }
        });
        if let Some(w) = self.running.lock().await.get_mut(plugin_id) {
            w.inbound = Some(inbound_tx.clone());
        }
        // A fresh UI generation per spawn: every ui.state.* write the worker
        // makes is stamped with it, so once this generation is retired below a
        // late write cannot resurrect state, and an instant respawn owns a new
        // generation that this worker's cleanup will not touch.
        let ui_generation = self.api.begin_ui_generation(plugin_id);
        let ctx = PluginRpcContext {
            plugin_id: plugin_id.to_string(),
            granted_capabilities: granted,
            ui_contributions,
            ui_generation,
        };
        serve_connection(&self.api, &ctx, stdout, inbound_tx).await;
        // Serving ended; stop accepting host-initiated pushes and tear down the
        // stdin writer so it does not outlive the worker.
        if let Some(w) = self.running.lock().await.get_mut(plugin_id) {
            w.inbound = None;
        }
        writer.abort();

        // The loop returned: the worker closed its stdout (exited or crashed).
        // Drop this generation's UI state (a respawn repopulates it); guarded by
        // the generation so it never wipes a newer worker's state. Then reap the
        // whole group so no forked helper is left behind, and let the caller
        // decide whether to respawn.
        self.api.clear_ui(plugin_id, ui_generation);
        if pid != 0 {
            worker::reap_group_escalating(pid, REAP_GRACE).await;
        }
        let _ = child.wait().await;
        Ok(())
    }

    /// Reap every running worker. Called on daemon shutdown.
    pub async fn shutdown(&self) {
        // Drain under the lock, then reap without holding it: the escalating
        // reap awaits a grace period, and we must not hold the running lock
        // across that await.
        let workers: Vec<_> = self.running.lock().await.drain().collect();
        // Reap in parallel: each escalating reap awaits up to REAP_GRACE before
        // SIGKILL, so a sequential loop would make total shutdown scale with the
        // worker count. The daemon's shutdown grace-period force-exit is armed
        // only after this returns, so a bounded reap keeps that safety net
        // meaningful. join_all bounds the whole thing to one REAP_GRACE.
        futures_util::future::join_all(workers.into_iter().map(|(plugin_id, w)| async move {
            w.task.abort();
            if w.pid != 0 {
                // SIGTERM the group, wait the grace, then SIGKILL: a worker or
                // forked helper that ignores SIGTERM must not survive shutdown.
                worker::reap_group_escalating(w.pid, REAP_GRACE).await;
            }
            tracing::debug!(target: "plugin.host", plugin = %plugin_id, "stopped plugin worker");
        }))
        .await;
    }
}

/// Read JSON-RPC requests from a worker's stdout, dispatch each through the
/// capability-gated host API, and write the response to its stdin. Returns when
/// the worker closes stdout or sends an unparseable line (fatal).
async fn serve_connection(
    api: &Arc<HostApiState>,
    ctx: &PluginRpcContext,
    stdout: tokio::process::ChildStdout,
    stdin: mpsc::UnboundedSender<String>,
) {
    let mut lines = BufReader::new(stdout).lines();
    // Unbounded line read: per the honest model (D8) the worker is cooperative,
    // not adversarial, so a malicious oversized line is out of scope here; an
    // OS-level sandbox backend is where that ceiling belongs.
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return, // EOF: worker exited.
            Err(e) => {
                tracing::warn!(target: "plugin.host", plugin = %ctx.plugin_id, "worker read error: {e}");
                return;
            }
        };
        let request = match protocol::parse_request(&line) {
            Ok(Some(req)) => req,
            Ok(None) => continue, // blank line
            Err(e) => {
                // A malformed line is a protocol violation; answer with a parse
                // error (best effort) and stop reading from this worker.
                let resp =
                    RpcResponse::error(Value::Null, codes::PARSE_ERROR, e.to_string()).to_line();
                let _ = stdin.send(resp);
                return;
            }
        };

        // The JSON parsed; now check the JSON-RPC envelope. A well-formed JSON
        // object with the wrong shape is an invalid request, distinct from
        // malformed JSON (PARSE_ERROR above). This message is rejected; the
        // connection continues.
        let method = match request.validate_envelope() {
            Ok(m) => m.to_string(),
            Err(msg) => {
                let id = request.id.clone().unwrap_or(Value::Null);
                let resp = RpcResponse::error(id, codes::INVALID_REQUEST, msg).to_line();
                if stdin.send(resp).is_err() {
                    return;
                }
                continue;
            }
        };

        // Dispatch does blocking SQLite and session-storage IO; run it off the
        // async runtime. The handler is fully synchronous and self-contained.
        let api = api.clone();
        let ctx_id = ctx.plugin_id.clone();
        let caps = ctx.granted_capabilities.clone();
        let ui_contributions = ctx.ui_contributions.clone();
        let ui_generation = ctx.ui_generation;
        let params = request.params.clone();
        let method_log = method.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let ctx = PluginRpcContext {
                plugin_id: ctx_id,
                granted_capabilities: caps,
                ui_contributions,
                ui_generation,
            };
            dispatch(&api, &ctx, &method, &params)
        })
        .await;

        // Trace every dispatch outcome host-side, before the notification
        // early-return below: a rejected call (a worker pushing an undeclared
        // slot, a malformed payload, an ungranted capability) is otherwise
        // invisible here, since the only signal is the error response the
        // worker may or may not log. A notification (no id) is logged the same
        // way even though it gets no response.
        match &outcome {
            Ok(Ok(_)) => tracing::debug!(
                target: "plugin.host",
                plugin = %ctx.plugin_id,
                method = %method_log,
                "worker rpc ok"
            ),
            Ok(Err(e)) => tracing::warn!(
                target: "plugin.host",
                plugin = %ctx.plugin_id,
                method = %method_log,
                code = e.code,
                "worker rpc rejected: {}",
                e.message
            ),
            Err(_) => {}
        }

        // A notification (no id) gets no response, but still ran for its side
        // effects above.
        let Some(id) = request.id else {
            continue;
        };

        let response = match outcome {
            Ok(Ok(result)) => RpcResponse::success(id, result),
            Ok(Err(e)) => RpcResponse::error(id, e.code, e.message),
            Err(join_err) => RpcResponse::error(
                id,
                codes::INTERNAL_ERROR,
                format!("host dispatch task failed: {join_err}"),
            ),
        };
        if stdin.send(response.to_line()).is_err() {
            return; // writer task gone; nothing more to say.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::host_api::PluginRpcContext;
    use serde_json::json;

    /// Spawn the single stdin-writer task `serve_connection` now expects, and
    /// return its sender (mirrors what `spawn_once` wires in production).
    fn stdin_writer(stdin: tokio::process::ChildStdin) -> mpsc::UnboundedSender<String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        tx
    }

    /// End-to-end over a real child process: a Node worker speaks ndjson
    /// JSON-RPC through `serve_connection`, hits the capability gate on a method
    /// it was not granted (`session.meta.set` with only `runtime.worker`), then
    /// publishes the refusal code over the granted events path. The test reads
    /// the event back, proving the wire, the capability gate, and the host
    /// dispatch all work through a genuine subprocess. Node-gated like the ACP
    /// fake-agent e2e; the capability refusal happens before any storage access,
    /// so this needs no profile isolation.
    #[tokio::test]
    async fn worker_subprocess_round_trip_and_capability_gate() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let api = Arc::new(
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap(),
        );
        // Granted only runtime.worker: events.* succeed, session.meta.set is
        // refused with FORBIDDEN.
        let ctx = PluginRpcContext {
            plugin_id: "acme.worker".to_string(),
            granted_capabilities: vec!["runtime.worker".to_string()],
            ui_contributions: std::collections::HashSet::new(),
            ui_generation: 0,
        };

        // The worker: request a forbidden method, then publish the error code it
        // got back over the granted events bus, then exit.
        const WORKER: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
let step = 0;
rl.on('line', (line) => {
  const resp = JSON.parse(line);
  if (step === 0) {
    step = 1;
    const code = resp.error ? resp.error.code : 0;
    process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:2,method:"events.publish",params:{topic:"result",payload:{forbidden_code:code}}}) + "\n");
  } else {
    process.exit(0);
  }
});
process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:1,method:"session.meta.set",params:{session_id:"x",key:"k",value:1}}) + "\n");
"#;

        let mut child = tokio::process::Command::new("node")
            .arg("-e")
            .arg(WORKER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        serve_connection(&api, &ctx, stdout, stdin_writer(stdin)).await;
        let _ = child.wait().await;

        // Read the event the worker published: it carries the FORBIDDEN code the
        // host returned for the ungranted session.meta.set.
        let got = dispatch(
            &api,
            &ctx,
            "events.subscribe",
            &json!({ "topics": ["result"], "after_seq": 0 }),
        )
        .unwrap();
        let events = got["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "worker should have published one result");
        assert_eq!(
            events[0]["payload"]["forbidden_code"],
            json!(codes::FORBIDDEN)
        );
    }

    /// The host->worker push path (what `notify_worker` uses): a notification
    /// written to the worker's stdin reaches it and is acted on. The worker waits
    /// idle, then on an unsolicited `host.ping` notification publishes an event,
    /// proving the unsolicited stdin write landed.
    #[tokio::test]
    async fn host_initiated_notification_reaches_worker() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let api = Arc::new(
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap(),
        );
        let ctx = PluginRpcContext {
            plugin_id: "acme.worker".to_string(),
            granted_capabilities: vec!["runtime.worker".to_string()],
            ui_contributions: std::collections::HashSet::new(),
            ui_generation: 0,
        };

        // Worker initiates nothing; it reacts to the host's `host.ping` push by
        // publishing, then exits once it sees the publish response.
        const WORKER: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const m = JSON.parse(line);
  if (m.method === 'host.ping') {
    process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:1,method:"events.publish",params:{topic:"pinged",payload:{ok:true}}}) + "\n");
  } else if (m.id === 1) {
    process.exit(0);
  }
});
"#;

        let mut child = tokio::process::Command::new("node")
            .arg("-e")
            .arg(WORKER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let tx = stdin_writer(stdin);
        // Host-initiated push, exactly as notify_worker builds it.
        tx.send(
            json!({ "jsonrpc": "2.0", "method": "host.ping", "params": {} }).to_string() + "\n",
        )
        .unwrap();

        serve_connection(&api, &ctx, stdout, tx).await;
        let _ = child.wait().await;

        let got = dispatch(
            &api,
            &ctx,
            "events.subscribe",
            &json!({ "topics": ["pinged"], "after_seq": 0 }),
        )
        .unwrap();
        let events = got["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "worker should react to the host push");
        assert_eq!(events[0]["payload"]["ok"], json!(true));
    }
}
