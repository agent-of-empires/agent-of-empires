//! ACP structured-view CLI subcommands.
//!
//! `aoe acp doctor` runs preflight checks (Node runtime, agent
//! binaries, claude auth). `aoe acp agents` lists configured
//! agents. Logs/restart are deferred until the worker
//! supervisor is wired into `aoe serve`.

use anyhow::Result;
use clap::Subcommand;

use crate::acp::agent_registry::AgentRegistry;
use crate::acp::install_hints::install_hint_for;
use crate::acp::node;

#[derive(Subcommand)]
pub enum AcpCommands {
    /// Verify the structured view can start: Node runtime, configured agents,
    /// provider auth (claude login).
    Doctor {
        /// Emit machine-readable JSON instead of a human report.
        #[arg(long)]
        json: bool,
        /// Attempt safe remediations: install missing claude-code-acp
        /// adapter, verify aoe-agent presence, etc. (Reserved for future
        /// release; the flag exists so scripts can opt in early.)
        #[arg(long)]
        fix: bool,
    },
    /// List configured agents (claude-code, aoe-agent, etc.).
    Agents,
    /// List running agent workers (detached or attached).
    Ps {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Gracefully stop an agent worker (SIGTERM the runner, agent
    /// receives stdin EOF). Sessions can be reattached on the next
    /// `aoe serve` only if they are still alive afterward; `stop`
    /// destroys the worker.
    Stop {
        /// Session id to stop. Mutually exclusive with `--all`.
        session: Option<String>,
        /// Stop every running agent worker.
        #[arg(long, conflicts_with = "session")]
        all: bool,
        /// Seconds to wait after SIGTERM before escalating to SIGKILL.
        #[arg(long, default_value = "5")]
        timeout_secs: u64,
    },
    /// SIGKILL a worker immediately (use when `stop` doesn't take).
    Kill {
        /// Session id to kill.
        session: String,
    },
    /// Tail the runner's log file for an agent session.
    Logs {
        /// Session id whose worker logs to tail.
        #[arg(long)]
        session: Option<String>,
        /// Follow new lines as they arrive.
        #[arg(long)]
        follow: bool,
    },
    /// Restart a wedged agent worker: stop the existing runner, then
    /// let the daemon's reconciler spawn a fresh one on the next tick.
    Restart {
        /// Session id whose worker to restart.
        session: String,
    },
    /// Print the persisted transcript for an agent session.
    History {
        /// Acp session id.
        session: String,
        /// Skip events at or below this seq.
        #[arg(long, default_value = "0")]
        since: u64,
        /// Emit raw frames as JSON (one frame per line).
        #[arg(long)]
        json: bool,
    },
    /// Print live status for an agent session: highest/lowest seq, and
    /// whether the on-disk retention window has truncated history.
    Status {
        /// Acp session id.
        session: String,
        /// Emit machine-readable JSON instead of a human report.
        #[arg(long)]
        json: bool,
    },
    /// Send a prompt to an agent session's agent.
    Prompt {
        /// Acp session id.
        session: String,
        /// Prompt text. Pass `-` to read from stdin.
        text: String,
    },
    /// Resolve a pending approval (default: allow). Use --always for a
    /// session-scoped allow-list entry, --deny to refuse the request.
    Approve {
        /// Acp session id.
        session: String,
        /// Approval nonce, as printed in the pending-approval banner.
        nonce: String,
        /// Allow this kind of operation for the rest of the session.
        #[arg(long, conflicts_with = "deny")]
        always: bool,
        /// Refuse the request.
        #[arg(long)]
        deny: bool,
    },
    /// Cancel the in-flight prompt for an agent session.
    Cancel {
        /// Acp session id.
        session: String,
    },
    /// Stream the agent broadcast for a session to stdout as JSON
    /// lines (one frame per line). Press Ctrl-C to stop.
    Tail {
        /// Acp session id.
        session: String,
        /// Start at this seq (default 0 = full replay then live).
        #[arg(long, default_value = "0")]
        since: u64,
    },
    /// Open the TUI structured view directly for a known session id.
    /// Combine with `AOE_DAEMON_URL` (+ `AOE_DAEMON_TOKEN`) to attach
    /// across machines without going through the home session list.
    Attach {
        /// Acp session id.
        session: String,
    },
    /// Switch an agent session to a different ACP agent, keeping the
    /// transcript. The new agent starts fresh; use `aoe acp agents`
    /// to list valid targets. Handy for returning to claude after a
    /// rate-limit handoff to codex.
    SwitchAgent {
        /// Acp session id.
        session: String,
        /// Registry key of the target agent (e.g. `claude`, `codex`).
        target: String,
        /// Optional model override forwarded to the new agent.
        #[arg(long)]
        model: Option<String>,
    },
}

#[tracing::instrument(target = "cli.acp", skip_all)]
pub async fn run(command: AcpCommands) -> Result<()> {
    match command {
        AcpCommands::Doctor { json, fix } => doctor(json, fix).await,
        AcpCommands::Agents => agents(),
        AcpCommands::Ps { json } => ps(json),
        AcpCommands::Stop {
            session,
            all,
            timeout_secs,
        } => stop(session, all, timeout_secs).await,
        AcpCommands::Kill { session } => kill_now(&session),
        AcpCommands::Logs { session, follow } => logs(session, follow),
        AcpCommands::Restart { session } => restart(&session),
        AcpCommands::History {
            session,
            since,
            json,
        } => history(&session, since, json).await,
        AcpCommands::Status { session, json } => status(&session, json).await,
        AcpCommands::Prompt { session, text } => prompt(&session, &text).await,
        AcpCommands::Approve {
            session,
            nonce,
            always,
            deny,
        } => approve(&session, &nonce, always, deny).await,
        AcpCommands::Cancel { session } => cancel(&session).await,
        AcpCommands::Tail { session, since } => tail(&session, since).await,
        AcpCommands::Attach { session } => attach(&session).await,
        AcpCommands::SwitchAgent {
            session,
            target,
            model,
        } => switch_agent(&session, &target, model.as_deref()).await,
    }
}

#[derive(Debug, serde::Serialize)]
struct DoctorReport {
    node: NodeStatus,
    agents: Vec<AgentDoctorEntry>,
    overall: &'static str,
}

#[derive(Debug, serde::Serialize)]
struct NodeStatus {
    found: bool,
    path: Option<String>,
    version: Option<String>,
    meets_minimum: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct AgentDoctorEntry {
    name: String,
    command_present: bool,
    description: String,
}

/// ACP adapters that ship as npm packages (binary name → package id).
/// The doctor's `--fix` path runs `npm install -g <package>` for each
/// entry whose binary isn't already on PATH.
const NPM_INSTALLABLE_ACP: &[(&str, &str)] = &[
    (
        "claude-agent-acp",
        "@agentclientprotocol/claude-agent-acp@latest",
    ),
    ("codex-acp", "@agentclientprotocol/codex-acp@latest"),
    ("pi-acp", "pi-acp"),
];

async fn doctor(json: bool, fix: bool) -> Result<()> {
    if fix {
        // Auto-remediate: download the bundled Node runtime if Node is
        // missing or the wrong version on PATH.
        if let Ok(app_dir) = crate::session::get_app_dir() {
            match node::resolve("", &app_dir) {
                Ok(_) => println!("Node already available; skipping download."),
                Err(node::NodeError::NoNode(_)) | Err(node::NodeError::TooOld { .. }) => {
                    println!("Downloading Node {} runtime...", node::PINNED_NODE_VERSION);
                    match node::download(&app_dir).await {
                        Ok(resolved) => {
                            println!(
                                "Installed Node {} at {}",
                                resolved.version,
                                resolved.path.display()
                            );
                        }
                        Err(e) => {
                            println!("Download failed: {e}");
                        }
                    }
                }
                Err(e) => println!("Cannot probe Node: {e}"),
            }
        }
        // Auto-install npm-distributed ACP adapters that aren't on
        // PATH. Native CLIs (opencode / gemini / vibe) have to be
        // installed via their own channels; we only print a hint for
        // those.
        for (binary, npm_pkg) in NPM_INSTALLABLE_ACP {
            if find_in_path(binary).is_some() {
                continue;
            }
            println!("Installing {npm_pkg} globally via npm...");
            let status = std::process::Command::new("npm")
                .args(["install", "-g", npm_pkg])
                .status();
            match status {
                Ok(s) if s.success() => println!("Installed {npm_pkg}."),
                Ok(s) => println!("npm install {npm_pkg} exited with status {s}"),
                Err(e) => {
                    println!("Could not run npm: {e}. Install Node.js + npm first.");
                    break;
                }
            }
        }
    }
    let registry = AgentRegistry::with_defaults();

    let node_status = check_node();
    let agent_entries: Vec<AgentDoctorEntry> = registry
        .list()
        .into_iter()
        .map(|(name, spec)| AgentDoctorEntry {
            name: name.clone(),
            command_present: command_present(&spec.command),
            description: spec.description.clone(),
        })
        .collect();

    let any_agent_ok = agent_entries.iter().any(|e| e.command_present);
    let node_ok = node_status.meets_minimum.unwrap_or(false);
    let overall = if node_ok && any_agent_ok {
        "ok"
    } else if node_ok || any_agent_ok {
        "partial"
    } else {
        "fail"
    };
    let report = DoctorReport {
        node: node_status,
        agents: agent_entries,
        overall,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("ACP doctor");
    println!("======================");
    println!();
    println!("The structured view is the ACP-based structured rendering. It is the default");
    println!("in the web dashboard; `aoe add` and the TUI default to the terminal view.");
    println!("Pass --structured-view or --agent to opt a CLI session in (or flip a session");
    println!("from the session view).");
    println!();
    let node = &report.node;
    let node_mark = if node.meets_minimum.unwrap_or(false) {
        "[OK]"
    } else {
        "[!! ]"
    };
    println!(
        "{} Node runtime  {}",
        node_mark,
        node.version.as_deref().unwrap_or("not found"),
    );
    if let Some(path) = &node.path {
        println!("    path: {}", path);
    }
    println!();
    println!("Configured agents:");
    let registry_for_hints = AgentRegistry::with_defaults();
    for entry in &report.agents {
        let mark = if entry.command_present {
            "[OK]"
        } else {
            "[!! ]"
        };
        println!("{} {}  ({})", mark, entry.name, entry.description);
        if !entry.command_present {
            // Look up the binary name via the registry so we can
            // print a tailored install hint instead of generic
            // "missing".
            if let Some(spec) = registry_for_hints.get(&entry.name) {
                let bin = spec.command.split('/').next_back().unwrap_or(&spec.command);
                if let Some(hint) = install_hint_for(bin) {
                    println!("    install: {hint}");
                }
            }
        }
    }
    println!();
    println!("Overall: {}", overall);

    if overall != "ok" {
        std::process::exit(if overall == "partial" { 2 } else { 1 });
    }
    Ok(())
}

fn check_node() -> NodeStatus {
    let path = match find_in_path("node") {
        Some(p) => p,
        None => {
            return NodeStatus {
                found: false,
                path: None,
                version: None,
                meets_minimum: None,
            };
        }
    };
    let output = std::process::Command::new(&path).arg("--version").output();
    let (version, meets_minimum) = match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let meets = parse_node_major(&raw).map(|m| m >= 20);
            (Some(raw), meets)
        }
        _ => (None, None),
    };
    NodeStatus {
        found: true,
        path: Some(path),
        version,
        meets_minimum,
    }
}

fn parse_node_major(raw: &str) -> Option<u32> {
    let trimmed = raw.trim_start_matches('v');
    let major_str = trimmed.split('.').next()?;
    major_str.parse::<u32>().ok()
}

fn find_in_path(binary: &str) -> Option<String> {
    which::which(binary)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

pub(crate) fn command_present(command: &str) -> bool {
    // Placeholders like `${aoe_data_dir}/acp-worker/...` resolve at
    // runtime against the app data dir, so the literal string contains
    // both `${` and `/`. Check the placeholder branch FIRST — otherwise
    // the `/`-branch tries to stat a literal path containing `${...}`
    // and reports "missing" for every placeholder-based agent
    // (notably `aoe-agent`, our bundled multi-provider fallback).
    if command.contains("${") {
        true
    } else if command.contains('/') || command.contains('\\') {
        std::path::Path::new(command).exists()
    } else {
        find_in_path(command).is_some()
    }
}

fn agents() -> Result<()> {
    let registry = AgentRegistry::with_defaults();
    println!("Configured agents:");
    println!();
    for (name, spec) in registry.list() {
        let present = command_present(&spec.command);
        let mark = if present { "[OK]" } else { "[!! ]" };
        println!("{} {:<14}  {}", mark, name, spec.description);
        let args = if spec.args.is_empty() {
            String::new()
        } else {
            format!(" {}", spec.args.join(" "))
        };
        println!("        spawn: {}{}", spec.command, args);
    }
    Ok(())
}

fn ps(json: bool) -> Result<()> {
    use crate::acp::worker_registry;
    let mut records = worker_registry::list().unwrap_or_default();
    records.sort_by_key(|r| r.started_at);
    if json {
        let value: Vec<serde_json::Value> = records
            .iter()
            .map(|r| {
                serde_json::json!({
                    "session_id": r.session_id,
                    "pid": r.pid,
                    "alive": worker_registry::is_record_live(r),
                    "agent": r.agent_name,
                    "build_version": r.build_version,
                    "build_stale": !worker_registry::is_build_current(r),
                    "socket": r.socket_path,
                    "cwd": r.cwd,
                    "started_at": r.started_at,
                    "last_attached_at": r.last_attached_at,
                    "detached_at": r.detached_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("No agent workers running.");
        return Ok(());
    }
    println!(
        "{:<24} {:<8} {:<14} {:<10} {:<24} SOCKET",
        "SESSION", "PID", "AGENT", "STATE", "BUILD"
    );
    for r in &records {
        let state = if !worker_registry::is_record_live(r) {
            "dead"
        } else if r.detached_at.is_some()
            && r.last_attached_at.unwrap_or(0) <= r.detached_at.unwrap_or(0)
        {
            "detached"
        } else {
            "attached"
        };
        let build = render_build_cell(&r.build_version, !worker_registry::is_build_current(r));
        println!(
            "{:<24} {:<8} {:<14} {:<10} {:<24} {}",
            truncate(&r.session_id, 24),
            r.pid,
            truncate(&r.agent_name, 14),
            state,
            truncate(&build, 24),
            r.socket_path.display()
        );
    }
    Ok(())
}

/// Render the BUILD cell for `aoe acp ps`. An empty `build_version`
/// (a legacy record written before the field existed) shows `<legacy>`;
/// any worker whose build differs from the running daemon's is tagged
/// `(stale)` so a not-yet-respawned worker is visible rather than silent.
/// See #1754.
fn render_build_cell(build_version: &str, stale: bool) -> String {
    let base = if build_version.is_empty() {
        "<legacy>"
    } else {
        build_version
    };
    if stale {
        format!("{base} (stale)")
    } else {
        base.to_string()
    }
}

async fn stop(session: Option<String>, all: bool, timeout_secs: u64) -> Result<()> {
    use crate::acp::worker_registry;
    let targets: Vec<crate::acp::worker_registry::WorkerRecord> = if all {
        worker_registry::list().unwrap_or_default()
    } else {
        let id = match session {
            Some(s) => s,
            None => {
                anyhow::bail!("aoe acp stop requires <session> or --all");
            }
        };
        worker_registry::load(&id)?
            .map(|r| vec![r])
            .unwrap_or_default()
    };
    if targets.is_empty() {
        println!("No matching agent workers.");
        return Ok(());
    }
    stop_worker_records(&targets, timeout_secs).await;
    Ok(())
}

/// Stop every registered agent worker. Returns the number stopped. Shared by
/// `aoe acp stop --all` and the top-level `aoe stop-all` panic command. A
/// failure to read the worker registry is surfaced as `Err` so callers can
/// reflect it in their exit status instead of silently reporting zero workers;
/// per-worker signaling stays best-effort.
pub(crate) async fn stop_all_workers(timeout_secs: u64) -> Result<usize> {
    use crate::acp::worker_registry;
    let targets = worker_registry::list()?;
    stop_worker_records(&targets, timeout_secs).await;
    Ok(targets.len())
}

async fn stop_worker_records(
    targets: &[crate::acp::worker_registry::WorkerRecord],
    timeout_secs: u64,
) {
    use crate::acp::worker_registry;
    for record in targets {
        // Delete the registry entry BEFORE SIGTERM. The running daemon
        // (if any) uses the registry-gone signal in `restart_decision`
        // to distinguish a user-initiated stop from a crash; without
        // this ordering, the daemon's drain task sees socket EOF first,
        // observes the registry still present, and respawns the runner
        // which immediately gets killed by our SIGTERM, racing into a
        // crash loop that burns the restart budget and surfaces the
        // "ACP agent crashed more than N times" banner.
        worker_registry::delete(&record.session_id).ok();
        signal_and_wait(record, timeout_secs).await;
        println!(
            "Stopped agent worker for {} (PID {}).",
            record.session_id, record.pid
        );
    }
}

fn kill_now(session: &str) -> Result<()> {
    use crate::acp::worker_registry;
    let Some(record) = worker_registry::load(session)? else {
        anyhow::bail!("No agent worker registry entry for session {session}");
    };
    // Delete registry before SIGKILL for the same race reason described
    // on `stop`: the running daemon's drain task uses the registry-gone
    // signal to skip respawn on user-initiated termination.
    worker_registry::delete(session).ok();
    // Group-SIGKILL so the agent's node/SDK grandchildren die with the
    // runner instead of orphaning under PID 1 (#1689). Unconditional: the
    // process group can outlive its leader pid, so gating on leader
    // liveness would skip the killpg and leak surviving descendants.
    // killpg ignores ESRCH, so signaling an already-empty group is a
    // harmless no-op.
    crate::process::worker::kill_process_group(record.pid);
    println!("Killed agent worker for {} (PID {}).", session, record.pid);
    Ok(())
}

async fn signal_and_wait(record: &crate::acp::worker_registry::WorkerRecord, timeout_secs: u64) {
    use crate::acp::worker_registry;
    // Group signals so the whole agent tree (runner + node + SDK child)
    // goes down together, not just the runner pid. Sent unconditionally:
    // the group can outlive its leader pid, so gating on leader liveness
    // would skip the SIGTERM and leak surviving descendants. See #1689.
    crate::process::worker::terminate_process_group(record.pid);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if !worker_registry::is_pid_alive(record.pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    crate::process::worker::kill_process_group(record.pid);
}

fn logs(session: Option<String>, follow: bool) -> Result<()> {
    use crate::acp::worker_registry;
    let id = match session {
        Some(s) => s,
        None => {
            let records = worker_registry::list().unwrap_or_default();
            if records.len() == 1 {
                records[0].session_id.clone()
            } else if records.is_empty() {
                println!("No agent workers running. Use `aoe acp ps` to inspect.");
                return Ok(());
            } else {
                println!("Multiple agent workers running; pass --session <id>:");
                for r in records {
                    println!("  {}", r.session_id);
                }
                return Ok(());
            }
        }
    };
    let log_path = worker_registry::log_path_for(&id)?;
    if !log_path.exists() {
        println!(
            "No log file at {} (worker may not have started yet).",
            log_path.display()
        );
        return Ok(());
    }
    if follow {
        // Use a simple busy-poll tail rather than depending on notify
        // crates; the runner appends a handful of lines per minute, so
        // the wasted wake-ups are negligible.
        use std::io::{BufRead, BufReader, Seek, SeekFrom};
        let mut file = std::fs::File::open(&log_path)?;
        // Seek to end so we only print *new* lines, like `tail -f`.
        file.seek(SeekFrom::End(0))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Ok(_) => print!("{line}"),
                Err(e) => {
                    eprintln!("read error: {e}");
                    break;
                }
            }
        }
    } else {
        let content = std::fs::read_to_string(&log_path)?;
        print!("{content}");
    }
    Ok(())
}

fn restart(session: &str) -> Result<()> {
    use crate::acp::worker_registry;
    let Some(record) = worker_registry::load(session)? else {
        anyhow::bail!("No agent worker registry entry for session {session}");
    };
    // SIGTERM the runner; the next 2s reconciler tick on `aoe serve`
    // notices the session has no live worker and spawns a fresh one
    // (which calls session/load with the cached acp_session_id).
    // Write the restart-pending marker BEFORE deleting the registry so
    // the daemon's reaper can distinguish a restart from `aoe acp
    // stop|kill` and emit `Stopped { reason: "restart_pending" }`
    // instead of `user_stopped` — the UI then renders a transient
    // "Restarting…" banner instead of the persistent "Stopped +
    // Reconnect" affordance.
    worker_registry::mark_restart_pending(session);
    worker_registry::delete(session).ok();
    // Group-SIGTERM so the agent's node/SDK grandchildren die with the
    // runner rather than orphaning under PID 1 before respawn (#1689).
    // Unconditional: the group can outlive its leader pid, so gating on
    // leader liveness would skip the killpg and leak descendants.
    crate::process::worker::terminate_process_group(record.pid);
    println!(
        "Stopped runner for {} (PID {}). `aoe serve` will respawn on its next reconciler tick.",
        session, record.pid
    );
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ── Daemon-backed agent verbs ─────────────────────────────────────
//
// These talk to a running `aoe serve` daemon via the agent HTTP / WS
// client. Mutating verbs (`prompt`, `approve`, `cancel`) auto-spawn a
// loopback daemon when none is running so a user who only ever uses
// the CLI doesn't have to remember to start `aoe serve` first. Read
// verbs (`history`, `status`, `tail`) auto-spawn too because the
// daemon is the only path to the disk-backed event store; there's no
// useful read against "no daemon".

use crate::acp::client::{require_daemon, HttpClient, HttpError, WsMessage, REPLAY_PAGE_SIZE};
use crate::acp::protocol::ApprovalDecisionWire;

async fn history(session: &str, since: u64, json: bool) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let resp = client
        .replay_paged(session, since, REPLAY_PAGE_SIZE)
        .await
        .map_err(map_http)?;
    if resp.lost {
        eprintln!(
            "warning: retention window evicted events before seq {}; transcript is partial.",
            since
        );
    }
    if json {
        for frame in &resp.frames {
            println!("{}", serde_json::to_string(&frame)?);
        }
        return Ok(());
    }
    if resp.frames.is_empty() {
        println!(
            "(no events; highest_seq={}, lowest_seq={})",
            resp.highest_seq,
            resp.lowest_seq
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        return Ok(());
    }
    for frame in &resp.frames {
        println!("seq {:>6}  {}", frame.seq, event_kind(&frame.event));
    }
    Ok(())
}

async fn status(session: &str, json: bool) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint.clone())?;
    // since=highest_seq returns an empty frames vec but keeps the
    // highest/lowest/lost summary intact. Cheaper than full replay.
    let probe = client.replay(session, u64::MAX).await.map_err(map_http)?;
    if json {
        let blob = serde_json::json!({
            "session_id": session,
            "highest_seq": probe.highest_seq,
            "lowest_seq": probe.lowest_seq,
            "lost": probe.lost,
            "daemon_url": endpoint.base_url,
            "daemon_source": format!("{:?}", endpoint.source),
        });
        println!("{}", serde_json::to_string_pretty(&blob)?);
        return Ok(());
    }
    println!("Agent session: {session}");
    println!(
        "  daemon       : {} ({:?})",
        endpoint.base_url, endpoint.source
    );
    println!("  highest_seq  : {}", probe.highest_seq);
    println!(
        "  lowest_seq   : {}",
        probe
            .lowest_seq
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into())
    );
    if probe.highest_seq == 0 {
        println!("  state        : no events recorded yet (worker may be idle or not yet spawned)");
    }
    Ok(())
}

async fn prompt(session: &str, text: &str) -> Result<()> {
    let body = read_text_arg(text)?;
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    client.prompt(session, &body).await.map_err(map_http)?;
    println!("prompt accepted ({} bytes)", body.len());
    Ok(())
}

async fn approve(session: &str, nonce: &str, always: bool, deny: bool) -> Result<()> {
    let decision = match (always, deny) {
        (_, true) => ApprovalDecisionWire::Deny,
        (true, false) => ApprovalDecisionWire::AllowAlways,
        (false, false) => ApprovalDecisionWire::Allow,
    };
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    client
        .resolve_approval(session, nonce, decision)
        .await
        .map_err(map_http)?;
    println!("approval {nonce} -> {decision:?}");
    Ok(())
}

async fn cancel(session: &str) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    client.cancel(session).await.map_err(map_http)?;
    println!(
        "{}",
        cancel_confirmation_message(crate::acp::acp_client::CANCEL_ESCALATION_GRACE.as_secs())
    );
    Ok(())
}

/// Honest confirmation for `aoe acp cancel`. The daemon only arms
/// the auto-restart escalation when a prompt is in flight; for an idle
/// session the cancel is a no-op notification, and the CLI cannot tell
/// which from the 202 it gets back. Spell both out so the operator does
/// not read a bare "cancel sent" as "nothing happened" and reach for
/// `aoe acp restart` before the escalation has a chance to fire. See
/// #1858.
fn cancel_confirmation_message(escalation_grace_secs: u64) -> String {
    format!(
        "cancel sent. If a prompt is in flight and the agent does not stop within ~{escalation_grace_secs}s, \
the worker is restarted automatically and the transcript is preserved. If the session is idle, this is a no-op."
    )
}

async fn switch_agent(session: &str, target: &str, model: Option<&str>) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let resp = client
        .switch_agent(session, target, model, Some("manual"))
        .await
        .map_err(map_http)?;
    println!("switched agent for {session} -> {}", resp.agent);
    Ok(())
}

async fn attach(session: &str) -> Result<()> {
    crate::tui::structured_view::run_standalone(session).await
}

async fn tail(session: &str, since: u64) -> Result<()> {
    let endpoint = require_daemon().await?;
    let mut handle = crate::acp::client::ws_connect(&endpoint, session, since).await?;
    while let Some(msg) = handle.recv().await {
        match msg {
            Ok(WsMessage::Frame(frame)) => {
                let line = serde_json::to_string(&*frame)?;
                println!("{line}");
            }
            Ok(WsMessage::Lagged) => {
                eprintln!("warning: ring buffer lagged; some events lost. Refetch with `aoe acp history <session>`.");
            }
            Err(e) => {
                eprintln!("ws error: {e}");
                anyhow::bail!("ws disconnected: {e}");
            }
        }
    }
    Ok(())
}

fn read_text_arg(text: &str) -> Result<String> {
    if text == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf.trim_end_matches('\n').to_string())
    } else {
        Ok(text.to_string())
    }
}

fn map_http(e: HttpError) -> anyhow::Error {
    anyhow::Error::new(e)
}

fn event_kind(event: &crate::acp::Event) -> &'static str {
    use crate::acp::Event;
    match event {
        Event::PlanUpdated { .. } => "plan_updated",
        Event::TodoListUpdated { .. } => "todo_list_updated",
        Event::SessionTitleSuggested { .. } => "session_title_suggested",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::ToolCallContent { .. } => "tool_call_content",
        Event::ToolCallUpdated { .. } => "tool_call_updated",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::ElicitationRequested { .. } => "elicitation_requested",
        Event::ElicitationResolved { .. } => "elicitation_resolved",
        Event::DiffEmitted { .. } => "diff_emitted",
        Event::ThinkingStarted => "thinking_started",
        Event::ThinkingEnded => "thinking_ended",
        Event::RateLimit { .. } => "rate_limit",
        Event::RateLimitAutoResumed { .. } => "rate_limit_auto_resumed",
        Event::UsageUpdated { .. } => "usage_updated",
        Event::ModeChanged { .. } => "mode_changed",
        Event::ModesAvailable { .. } => "modes_available",
        Event::CurrentModeChanged { .. } => "current_mode_changed",
        Event::ModeSwitchFailed { .. } => "mode_switch_failed",
        Event::AvailableCommandsUpdated { .. } => "available_commands_updated",
        Event::ConfigOptionsUpdated { .. } => "config_options_updated",
        Event::ConfigOptionSwitchFailed { .. } => "config_option_switch_failed",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
        Event::BackgroundAgentLaunched { .. } => "background_agent_launched",
        Event::BackgroundAgentProgress { .. } => "background_agent_progress",
        Event::BackgroundAgentCompleted { .. } => "background_agent_completed",
        Event::PromptRuntimeError { .. } => "prompt_runtime_error",
        Event::AgentMessageChunk { .. } => "agent_message_chunk",
        Event::CancelRequested { .. } => "cancel_requested",
        Event::Stopped { .. } => "stopped",
        Event::AgentStartupError { .. } => "agent_startup_error",
        Event::IncompatibleAgent { .. } => "incompatible_agent",
        Event::UserPromptSent { .. } => "user_prompt_sent",
        Event::UserDiffCommentsPrompt { .. } => "user_diff_comments_prompt",
        Event::PromptCapabilities { .. } => "prompt_capabilities",
        Event::AcpSessionAssigned { .. } => "acp_session_assigned",
        Event::SessionContextReset { .. } => "session_context_reset",
        Event::SessionCleared => "session_cleared",
        Event::ConversationCompacted => "conversation_compacted",
        Event::WakeupScheduled { .. } => "wakeup_scheduled",
        Event::MonitorArmed { .. } => "monitor_armed",
        Event::PromptRejected { .. } => "prompt_rejected",
        Event::AgentSwitched { .. } => "agent_switched",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_major_works() {
        assert_eq!(parse_node_major("v22.21.0"), Some(22));
        assert_eq!(parse_node_major("v20.0.0"), Some(20));
        assert_eq!(parse_node_major("18.17.1"), Some(18));
        assert_eq!(parse_node_major("not a version"), None);
    }

    /// Story 3: `aoe acp ps` surfaces the worker build version, tags
    /// a build-stale worker, and renders an empty (legacy) version as
    /// `<legacy>`. See #1754.
    #[test]
    fn render_build_cell_cases() {
        // Current build: bare version, no marker.
        assert_eq!(render_build_cell("1.9.5+gabc123", false), "1.9.5+gabc123");
        // Stale build: version plus marker.
        assert_eq!(
            render_build_cell("1.9.4+gdeadbe", true),
            "1.9.4+gdeadbe (stale)"
        );
        // Legacy record (field absent on disk): placeholder, always stale.
        assert_eq!(render_build_cell("", true), "<legacy> (stale)");
    }

    /// #1858: `aoe acp cancel` must explain the conditional
    /// auto-restart escalation and the idle no-op, not print a bare
    /// "cancel sent" that reads as "nothing happened".
    #[test]
    fn cancel_confirmation_message_states_escalation_and_no_op() {
        let msg =
            cancel_confirmation_message(crate::acp::acp_client::CANCEL_ESCALATION_GRACE.as_secs());
        assert!(
            msg.contains("10s"),
            "must surface the escalation grace: {msg}"
        );
        assert!(
            msg.contains("restarted automatically"),
            "must mention the auto-restart escalation: {msg}"
        );
        assert!(
            msg.contains("no-op"),
            "must spell out the idle no-op case: {msg}"
        );
    }
}
