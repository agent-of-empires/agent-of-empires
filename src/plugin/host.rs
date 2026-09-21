//! Launches and supervises plugin workers inside `aoe serve`.

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

const EVENT_RETENTION_PER_TOPIC: usize = 10_000;
const MAX_WORKERS: usize = 32;
const MAX_RESPAWNS: usize = 3;
const RESPAWN_WINDOW: Duration = Duration::from_secs(60);
const REAP_GRACE: Duration = Duration::from_secs(2);

struct RunningWorker {
    supervisor_id: u64,
    pid: u32,
    task: tokio::task::JoinHandle<()>,
    inbound: Option<mpsc::UnboundedSender<String>>,
    ui_generation: Option<u64>,
}

struct WorkerTable {
    running: HashMap<String, RunningWorker>,
    crashed: HashSet<String>,
    next_supervisor_id: u64,
}

fn inactive_launch_diagnostic(enabled: bool, granted: bool) -> (tracing::Level, &'static str) {
    if !enabled {
        (tracing::Level::DEBUG, "disabled")
    } else if !granted {
        (tracing::Level::WARN, "ungranted; awaiting reapproval")
    } else {
        (tracing::Level::WARN, "inactive")
    }
}

pub struct PluginHost {
    api: Arc<HostApiState>,
    sandbox: Arc<dyn SandboxBackend>,
    workers_dir: PathBuf,
    state: Mutex<WorkerTable>,
    max_workers: usize,
    session_rpc: Option<Arc<crate::plugin::session_api::SessionRpcDeps>>,
}

impl PluginHost {
    pub fn new(
        app_dir: &std::path::Path,
        profile: &str,
        session_rpc: Option<Arc<crate::plugin::session_api::SessionRpcDeps>>,
    ) -> Result<Arc<Self>> {
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
            state: Mutex::new(WorkerTable {
                running: HashMap::new(),
                crashed: HashSet::new(),
                next_supervisor_id: 1,
            }),
            max_workers: MAX_WORKERS,
            session_rpc,
        }))
    }

    pub fn ui_snapshot(&self) -> crate::plugin::ui_state::UiSnapshot {
        self.api.ui_snapshot()
    }

    pub fn ui_revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.api.ui_revision(plugin_id, session_id)
    }

    pub fn notify_host(
        &self,
        plugin_id: &str,
        tone: crate::plugin::ui_state::Tone,
        title: String,
        body: Option<String>,
    ) {
        self.api.notify_host(plugin_id, tone, title, body);
    }

    pub async fn notify_worker(&self, plugin_id: &str, method: &str, params: Value) -> bool {
        let line = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params })
            .to_string()
            + "\n";
        let table = self.state.lock().await;
        match table
            .running
            .get(plugin_id)
            .and_then(|w| w.inbound.as_ref())
        {
            Some(tx) => tx.send(line).is_ok(),
            None => false,
        }
    }

    pub async fn emit_settings_changed(&self, changes: &[(String, Vec<String>)]) {
        if changes.is_empty() {
            return;
        }
        let revision = self.api.bump_settings_revision();
        for (plugin_id, changed_keys) in changes {
            let params = serde_json::json!({
                "revision": revision,
                "changed_keys": changed_keys,
            });
            let delivered = self
                .notify_worker(plugin_id, "plugin.settings.changed", params)
                .await;
            if !delivered {
                tracing::debug!(
                    target: "plugin.host",
                    plugin = %plugin_id,
                    "plugin.settings.changed not delivered (no live worker); config.get is the fallback"
                );
            }
        }
    }

    pub async fn start(self: &Arc<Self>, registry: &PluginRegistry) {
        Self::log_start_observability(registry);
        self.reconcile(registry).await;
    }

    fn log_start_observability(registry: &PluginRegistry) {
        for err in registry.load_errors() {
            tracing::warn!(
                target: "plugin.host",
                "plugin load error; a worker may not launch: {err}"
            );
        }
        for p in registry.all() {
            if p.manifest.runtime.is_some() && !p.active() {
                let (level, reason) = inactive_launch_diagnostic(p.enabled, p.granted);
                let msg = "plugin declares a runtime but is inactive; not launching a worker";
                if level == tracing::Level::DEBUG {
                    tracing::debug!(target: "plugin.host", plugin = %p.id(), reason, "{msg}");
                } else {
                    tracing::warn!(target: "plugin.host", plugin = %p.id(), reason, "{msg}");
                }
            }
        }
    }

    pub async fn reconcile(self: &Arc<Self>, registry: &PluginRegistry) {
        let desired: HashSet<String> = registry
            .active()
            .filter(|p| p.manifest.runtime.is_some())
            .map(|p| p.id().to_string())
            .collect();

        let to_teardown: Vec<(String, RunningWorker)> = {
            let mut table = self.state.lock().await;
            table.crashed.retain(|id| desired.contains(id));

            let running_ids: HashSet<String> = table.running.keys().cloned().collect();
            let (to_launch, teardown_ids, truncated) =
                plan_reconcile(&desired, &running_ids, &table.crashed, self.max_workers);

            if truncated {
                tracing::warn!(
                    target: "plugin.host",
                    cap = self.max_workers,
                    "plugin worker concurrency cap reached; some workers not launched"
                );
            }

            let mut drained = Vec::with_capacity(teardown_ids.len());
            for id in teardown_ids {
                if let Some(w) = table.running.remove(&id) {
                    drained.push((id, w));
                }
            }
            for id in to_launch {
                self.launch_locked(&mut table, id);
            }
            drained
        };

        self.teardown_workers(to_teardown).await;
    }

    pub async fn restart_worker(self: &Arc<Self>, plugin_id: &str, registry: &PluginRegistry) {
        let replaced = {
            let mut table = self.state.lock().await;
            table.crashed.remove(plugin_id);
            table.running.remove(plugin_id).map(|stale| {
                let reservation = table.next_supervisor_id;
                table.next_supervisor_id += 1;
                table.running.insert(
                    plugin_id.to_string(),
                    RunningWorker {
                        supervisor_id: reservation,
                        pid: 0,
                        task: tokio::spawn(async {}),
                        inbound: None,
                        ui_generation: None,
                    },
                );
                (stale, reservation)
            })
        };
        if let Some((stale, reservation)) = replaced {
            self.teardown_workers(vec![(plugin_id.to_string(), stale)])
                .await;
            let mut table = self.state.lock().await;
            if table
                .running
                .get(plugin_id)
                .is_some_and(|w| w.supervisor_id == reservation)
            {
                table.running.remove(plugin_id);
                if registry
                    .get(plugin_id)
                    .is_some_and(|p| p.active() && p.manifest.runtime.is_some())
                {
                    self.launch_locked(&mut table, plugin_id.to_string());
                }
            }
        }
        self.reconcile(registry).await;
    }

    fn launch_locked(self: &Arc<Self>, table: &mut WorkerTable, plugin_id: String) {
        if table.running.contains_key(&plugin_id) {
            return;
        }
        let supervisor_id = table.next_supervisor_id;
        table.next_supervisor_id += 1;
        let host = self.clone();
        let id_for_task = plugin_id.clone();
        let task = tokio::spawn(async move {
            host.supervise(id_for_task, supervisor_id).await;
        });
        table.running.insert(
            plugin_id,
            RunningWorker {
                supervisor_id,
                pid: 0,
                task,
                inbound: None,
                ui_generation: None,
            },
        );
    }

    async fn teardown_workers(&self, workers: Vec<(String, RunningWorker)>) {
        futures_util::future::join_all(workers.into_iter().map(|(plugin_id, w)| async move {
            w.task.abort();
            let _ = w.task.await;
            if let Some(generation) = w.ui_generation {
                self.api.clear_ui(&plugin_id, generation);
            }
            if w.pid != 0 {
                worker::reap_group_escalating(w.pid, REAP_GRACE).await;
            }
            tracing::debug!(target: "plugin.host", plugin = %plugin_id, "stopped plugin worker");
        }))
        .await;
    }

    async fn remove_if_current(&self, plugin_id: &str, supervisor_id: u64) {
        let mut table = self.state.lock().await;
        if table
            .running
            .get(plugin_id)
            .is_some_and(|w| w.supervisor_id == supervisor_id)
        {
            table.running.remove(plugin_id);
        }
    }

    async fn mark_crashed_and_remove_if_current(
        &self,
        plugin_id: &str,
        supervisor_id: u64,
    ) -> bool {
        let mut table = self.state.lock().await;
        let current = table
            .running
            .get(plugin_id)
            .is_some_and(|w| w.supervisor_id == supervisor_id);
        if current {
            table.running.remove(plugin_id);
            table.crashed.insert(plugin_id.to_string());
        }
        current
    }

    async fn supervise(self: Arc<Self>, plugin_id: String, supervisor_id: u64) {
        let mut restarts: Vec<Instant> = Vec::new();
        loop {
            match self.spawn_once(&plugin_id, supervisor_id).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(
                        target: "plugin.host",
                        plugin = %plugin_id,
                        "failed to launch plugin worker: {e:#}"
                    );
                    self.remove_if_current(&plugin_id, supervisor_id).await;
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
                if self
                    .mark_crashed_and_remove_if_current(&plugin_id, supervisor_id)
                    .await
                {
                    self.notify_host(
                        &plugin_id,
                        crate::plugin::ui_state::Tone::Danger,
                        "Plugin worker stopped".to_string(),
                        Some(format!(
                            "The worker crashed more than {MAX_RESPAWNS} times in {}s and will not restart until the plugin is disabled and re-enabled.",
                            RESPAWN_WINDOW.as_secs()
                        )),
                    );
                }
                return;
            }
            tracing::warn!(
                target: "plugin.host",
                plugin = %plugin_id,
                "plugin worker exited; respawning"
            );
        }
    }

    async fn spawn_once(&self, plugin_id: &str, supervisor_id: u64) -> Result<()> {
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

        let stdin = child.stdin.take().context("worker stdin missing")?;
        let stdout = child.stdout.take().context("worker stdout missing")?;

        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = inbound_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break; // worker closed stdin; nothing more to send.
                }
            }
        });
        let ui_generation = self.api.begin_ui_generation(plugin_id);

        let accepted = {
            let mut table = self.state.lock().await;
            match table.running.get_mut(plugin_id) {
                Some(w) if w.supervisor_id == supervisor_id => {
                    w.pid = pid;
                    w.inbound = Some(inbound_tx.clone());
                    w.ui_generation = Some(ui_generation);
                    true
                }
                _ => false,
            }
        };
        if !accepted {
            writer.abort();
            self.api.clear_ui(plugin_id, ui_generation);
            if pid != 0 {
                worker::reap_group_escalating(pid, REAP_GRACE).await;
            }
            let _ = child.wait().await;
            anyhow::bail!("plugin {plugin_id} worker slot was torn down before registration");
        }

        tracing::info!(
            target: "plugin.host",
            plugin = %plugin_id,
            pid,
            program = %prepared.program.display(),
            "launched plugin worker"
        );
        let ctx = PluginRpcContext {
            plugin_id: plugin_id.to_string(),
            granted_capabilities: granted,
            ui_contributions,
            ui_generation,
        };
        serve_connection(
            &self.api,
            &ctx,
            stdout,
            inbound_tx,
            self.session_rpc.as_ref(),
        )
        .await;
        {
            let mut table = self.state.lock().await;
            if let Some(w) = table.running.get_mut(plugin_id) {
                if w.supervisor_id == supervisor_id {
                    w.inbound = None;
                    w.ui_generation = None;
                }
            }
        }
        writer.abort();

        self.api.clear_ui(plugin_id, ui_generation);
        if pid != 0 {
            worker::reap_group_escalating(pid, REAP_GRACE).await;
        }
        let _ = child.wait().await;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let workers: Vec<_> = self.state.lock().await.running.drain().collect();
        self.teardown_workers(workers).await;
    }
}

fn plan_reconcile(
    desired: &HashSet<String>,
    running_ids: &HashSet<String>,
    crashed: &HashSet<String>,
    cap: usize,
) -> (Vec<String>, Vec<String>, bool) {
    let mut to_teardown: Vec<String> = running_ids.difference(desired).cloned().collect();
    to_teardown.sort();

    let mut candidates: Vec<String> = desired
        .iter()
        .filter(|id| !running_ids.contains(*id) && !crashed.contains(*id))
        .cloned()
        .collect();
    candidates.sort();

    let remaining = running_ids.len() - to_teardown.len();
    let slots = cap.saturating_sub(remaining);
    let truncated = candidates.len() > slots;
    let to_launch: Vec<String> = candidates.into_iter().take(slots).collect();
    (to_launch, to_teardown, truncated)
}

async fn serve_connection(
    api: &Arc<HostApiState>,
    ctx: &PluginRpcContext,
    stdout: tokio::process::ChildStdout,
    stdin: mpsc::UnboundedSender<String>,
    session_rpc: Option<&Arc<crate::plugin::session_api::SessionRpcDeps>>,
) {
    let mut lines = BufReader::new(stdout).lines();
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
                let resp =
                    RpcResponse::error(Value::Null, codes::PARSE_ERROR, e.to_string()).to_line();
                let _ = stdin.send(resp);
                return;
            }
        };

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

        if crate::plugin::session_api::handles(&method) {
            let outcome = match session_rpc {
                Some(deps) => {
                    crate::plugin::session_api::dispatch(deps, ctx, &method, &request.params).await
                }
                None => {
                    let unavailable = crate::plugin::host_api::DispatchError::with_kind(
                        codes::SERVICE_UNAVAILABLE,
                        "service_unavailable",
                        "session service is not available in this host",
                    );
                    match crate::plugin::session_api::required_capability(&method) {
                        Some(cap) => ctx.require(cap).and(Err(unavailable)),
                        None => Err(unavailable),
                    }
                }
            };
            match &outcome {
                Ok(_) => tracing::debug!(
                    target: "plugin.host",
                    plugin = %ctx.plugin_id,
                    method = %method,
                    "worker rpc ok"
                ),
                Err(e) => tracing::warn!(
                    target: "plugin.host",
                    plugin = %ctx.plugin_id,
                    method = %method,
                    code = e.code,
                    "worker rpc rejected: {}",
                    e.message
                ),
            }
            let Some(id) = request.id else {
                continue;
            };
            let response = match outcome {
                Ok(result) => RpcResponse::success(id, result),
                Err(e) => RpcResponse::error_with_data(id, e.code, e.message, e.data),
            };
            if stdin.send(response.to_line()).is_err() {
                return;
            }
            continue;
        }

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

        let Some(id) = request.id else {
            continue;
        };

        let response = match outcome {
            Ok(Ok(result)) => RpcResponse::success(id, result),
            Ok(Err(e)) => RpcResponse::error_with_data(id, e.code, e.message, e.data),
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

    #[test]
    fn inactive_launch_diagnostic_warns_only_on_unchosen_states() {
        let cases = [
            (false, false, tracing::Level::DEBUG, "disabled"),
            (false, true, tracing::Level::DEBUG, "disabled"),
            (
                true,
                false,
                tracing::Level::WARN,
                "ungranted; awaiting reapproval",
            ),
            (true, true, tracing::Level::WARN, "inactive"),
        ];
        for (enabled, granted, level, reason) in cases {
            assert_eq!(
                inactive_launch_diagnostic(enabled, granted),
                (level, reason),
                "enabled={enabled} granted={granted}"
            );
        }
    }

    fn worker_api(dir: &std::path::Path) -> (Arc<HostApiState>, PluginRpcContext) {
        let api =
            Arc::new(HostApiState::open(&dir.join("plugin_events.db"), "default", 100).unwrap());
        let ctx = PluginRpcContext {
            plugin_id: "acme.worker".to_string(),
            granted_capabilities: vec!["runtime.worker".to_string()],
            ui_contributions: std::collections::HashSet::new(),
            ui_generation: 0,
        };
        (api, ctx)
    }

    fn spawn_node(script: &str) -> tokio::process::Child {
        tokio::process::Command::new("node")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    fn published(api: &HostApiState, ctx: &PluginRpcContext, topic: &str) -> serde_json::Value {
        let got = dispatch(
            api,
            ctx,
            "events.subscribe",
            &json!({ "topics": [topic], "after_seq": 0 }),
        )
        .unwrap();
        got["events"].clone()
    }

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

    #[tokio::test]
    async fn worker_subprocess_round_trip_and_capability_gate() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (api, ctx) = worker_api(tmp.path());

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

        let mut child = spawn_node(WORKER);
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        serve_connection(&api, &ctx, stdout, stdin_writer(stdin), None).await;
        let _ = child.wait().await;

        let events = published(&api, &ctx, "result");
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 1, "worker should have published one result");
        assert_eq!(
            events[0]["payload"]["forbidden_code"],
            json!(codes::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn host_initiated_notification_reaches_worker() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (api, ctx) = worker_api(tmp.path());

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

        let mut child = spawn_node(WORKER);
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let tx = stdin_writer(stdin);
        tx.send(
            json!({ "jsonrpc": "2.0", "method": "host.ping", "params": {} }).to_string() + "\n",
        )
        .unwrap();

        serve_connection(&api, &ctx, stdout, tx, None).await;
        let _ = child.wait().await;

        let events = published(&api, &ctx, "pinged");
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 1, "worker should react to the host push");
        assert_eq!(events[0]["payload"]["ok"], json!(true));
    }

    async fn worker_pid(host: &PluginHost, plugin_id: &str) -> Option<u32> {
        let table = host.state.lock().await;
        table
            .running
            .get(plugin_id)
            .map(|w| w.pid)
            .filter(|pid| *pid != 0)
    }

    async fn wait_until<F, Fut>(what: &str, mut done: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done().await {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn install_sleeper(plugin_id: &str) {
        use crate::session::{update_config, CapabilityGrant, PluginConfig};

        let manifest = format!(
            r#"
id = "{plugin_id}"
name = "Sleeper"
version = "1.0.0"
api_version = 8
capabilities = ["runtime.worker"]

[runtime]
kind = "command"
system = true
command = ["sleep", "600"]
"#
        );
        let dir = crate::plugin::plugins_dir().unwrap().join(plugin_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), &manifest).unwrap();
        update_config(|config| {
            config.plugins.insert(
                plugin_id.to_string(),
                PluginConfig {
                    grant: Some(CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities: vec!["runtime.worker".to_string()],
                        granted_at: chrono::Utc::now(),
                    }),
                    ..PluginConfig::default()
                },
            );
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn restart_worker_replaces_the_running_worker_in_its_slot() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
            ("HOME", temp.path().to_path_buf()),
            ("USERPROFILE", temp.path().to_path_buf()),
        ]);
        let plugin_id = "acme.sleeper";
        install_sleeper(plugin_id);
        let registry = crate::plugin::reload_registry();

        let mut host = PluginHost::new(&temp.path().join("host"), "default", None).unwrap();
        Arc::get_mut(&mut host).unwrap().max_workers = 1;
        host.start(&registry).await;
        wait_until("the first worker", || async {
            worker_pid(&host, plugin_id).await.is_some()
        })
        .await;
        let first = worker_pid(&host, plugin_id).await.unwrap();

        host.reconcile(&registry).await;
        assert_eq!(worker_pid(&host, plugin_id).await, Some(first));

        let waiting = "acme.aaa";
        install_sleeper(waiting);
        let registry = crate::plugin::reload_registry();
        host.reconcile(&registry).await;
        assert!(!host.state.lock().await.running.contains_key(waiting));

        host.restart_worker(plugin_id, &registry).await;
        wait_until("a replacement worker", || async {
            worker_pid(&host, plugin_id)
                .await
                .is_some_and(|pid| pid != first)
        })
        .await;
        assert!(
            !host.state.lock().await.running.contains_key(waiting),
            "the waiting plugin must not take the restarted worker's slot"
        );
        wait_until("the old worker to exit", || async {
            !worker::is_pid_alive(first)
        })
        .await;

        host.shutdown().await;
    }

    #[test]
    fn plan_reconcile_launches_missing_tears_down_extras_and_respects_the_cap() {
        let set = |ids: &[&str]| -> HashSet<String> { ids.iter().map(|s| s.to_string()).collect() };
        // name, desired, running, crashed, cap, launch, teardown, truncated
        type HostCase = (
            &'static str,
            &'static [&'static str],
            &'static [&'static str],
            &'static [&'static str],
            usize,
            &'static [&'static str],
            &'static [&'static str],
            bool,
        );
        let cases: [HostCase; 5] = [
            (
                "launches missing",
                &["a", "b"],
                &[],
                &[],
                MAX_WORKERS,
                &["a", "b"],
                &[],
                false,
            ),
            (
                "tears down extras",
                &["a"],
                &["a", "b"],
                &[],
                MAX_WORKERS,
                &[],
                &["b"],
                false,
            ),
            (
                "idempotent",
                &["a"],
                &["a"],
                &[],
                MAX_WORKERS,
                &[],
                &[],
                false,
            ),
            (
                "skips crashed",
                &["a"],
                &[],
                &["a"],
                MAX_WORKERS,
                &[],
                &[],
                false,
            ),
            (
                "cap truncates",
                &["a", "b", "c"],
                &[],
                &[],
                2,
                &["a", "b"],
                &[],
                true,
            ),
        ];
        for (name, desired, running, crashed, cap, want_launch, want_teardown, want_truncated) in
            cases
        {
            let (launch, teardown, truncated) =
                plan_reconcile(&set(desired), &set(running), &set(crashed), cap);
            assert_eq!(launch, want_launch, "{name}");
            assert_eq!(teardown, want_teardown, "{name}");
            assert_eq!(truncated, want_truncated, "{name}");
        }

        let mut crashed = set(&["a"]);
        crashed.retain(|id| set(&[]).contains(id));
        assert!(
            crashed.is_empty(),
            "disable must forget the crash tombstone"
        );
        let (launch, _, _) = plan_reconcile(&set(&["a"]), &set(&[]), &crashed, MAX_WORKERS);
        assert_eq!(launch, vec!["a".to_string()], "a retry is allowed after it");
    }
}
