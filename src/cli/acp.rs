//! ACP structured-view CLI subcommands.

use anyhow::Result;
use clap::Subcommand;

use crate::acp::agent_registry::AgentRegistry;
use crate::acp::install_hints::install_hint_for;
use crate::acp::node;
use crate::agents::registry_lifecycle;

#[derive(Subcommand)]
pub enum AcpCommands {
    /// Verify the structured view can start: Node runtime, configured agents,
    /// provider auth (claude login).
    Doctor {
        /// Emit machine-readable JSON instead of a human report.
        #[arg(long)]
        json: bool,
        /// Attempt safe remediations: download the bundled Node runtime if
        /// none is present, then install the pinned npm ACP adapter into the
        /// data dir with that Node's own npm (no global install, no sudo).
        /// Installs claude-agent-acp by default; each adapter is a separate
        /// several-hundred-MB tree, so pick others with --adapter.
        #[arg(long)]
        fix: bool,
        /// Adapter to install with --fix (repeatable). Defaults to
        /// claude-agent-acp.
        #[arg(
            long,
            requires = "fix",
            value_parser = clap::builder::PossibleValuesParser::new(bundled_adapter_names())
        )]
        adapter: Vec<String>,
        /// Install every pinned adapter with --fix instead of just the
        /// default one.
        #[arg(long, requires = "fix", conflicts_with = "adapter")]
        all_adapters: bool,
    },
    /// List configured agents (claude-code, aoe-agent, etc.).
    Agents,
    /// Internal: trap for `aoe acp ps`, removed in favour of the unified
    /// `aoe ps --acp`. Redirects rather than 404ing on an unknown subcommand.
    /// Hidden from help.
    #[command(name = "ps", hide = true)]
    Ps {
        /// Swallow any flags the user typed (e.g. `--json`) so the trap fires
        /// instead of clap erroring on an unexpected argument.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stop an agent worker and settle its owned native processes.
    /// Unlike `aoe serve --stop`, this ends the worker rather than detaching.
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
    /// Retire unprovable worker ownership after external shutdown confirmation.
    Recover {
        /// Session whose ownership evidence to retire.
        session: String,
        /// Confirm native processes are stopped for an identifiable missing-route Prime record.
        #[arg(
            long,
            required_unless_present = "confirm_worker_stopped",
            conflicts_with = "confirm_worker_stopped"
        )]
        confirm_prime_stopped: bool,
        /// Confirm the original runner and all host/container native processes are stopped.
        /// Accepts malformed, unattested legacy, or externally settled orphan records.
        #[arg(
            long,
            required_unless_present = "confirm_prime_stopped",
            conflicts_with = "confirm_prime_stopped"
        )]
        confirm_worker_stopped: bool,
    },
    /// SIGKILL the runner, then settle its native workers.
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
    /// Restart a wedged worker after its runner and native workers have stopped.
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
    /// Set a config option (model, mode, effort, ...) on a running agent
    /// session. The agent's own option id is the config id, e.g.
    /// `reasoning_effort`. A rejection by the agent is reported in the
    /// session's event stream, not here.
    SetOption {
        /// Acp session id.
        session: String,
        /// Config option id as advertised by the agent.
        config_id: String,
        /// Value to select.
        value: String,
    },
    /// Resolve a pending approval (default: allow). Use --always for a
    /// session-scoped allow-list entry, --deny to refuse the request, and
    /// --option to answer a request that lists choices.
    Approve {
        /// Acp session id.
        session: String,
        /// Approval nonce, as printed in the pending-approval banner.
        nonce: String,
        /// Allow this kind of operation for the rest of the session.
        #[arg(long, conflicts_with_all = ["deny", "option"])]
        always: bool,
        /// Refuse the request.
        #[arg(long, conflicts_with = "option")]
        deny: bool,
        /// Answer with this option id, from the request's option list. An
        /// id the request never offered cancels it instead.
        #[arg(long, value_name = "ID")]
        option: Option<String>,
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
    /// Combine with `AOE_DAEMON_URL` (+ `AOE_DAEMON_TOKEN`, or
    /// `AOE_DAEMON_PASSPHRASE` against a `--auth=passphrase` daemon) to
    /// attach across machines without going through the home session list.
    Attach {
        /// Acp session id.
        session: String,
    },
    /// Switch an agent session to a different ACP agent, keeping the
    /// transcript. Valid targets are built-in registry agents and any
    /// custom agent configured in `[session.agent_acp_cmd]`. The new
    /// agent starts fresh; use `aoe acp agents` to list built-in
    /// targets. Handy for returning to claude after a rate-limit handoff
    /// to codex.
    SwitchAgent {
        /// Acp session id.
        session: String,
        /// Registry key or configured custom ACP agent name (e.g.
        /// `claude`, `codex`, `my-custom-bridge`).
        target: String,
        /// Optional model override forwarded to the new agent.
        #[arg(long)]
        model: Option<String>,
    },
    /// Re-route a Claude session to a different LLM provider, keeping the
    /// transcript. Refused mid-turn; once idle the worker restarts and
    /// resumes the same conversation. Credentials are not provisioned by
    /// this: the target provider's own variables must already be set on the
    /// host (for example `ANTHROPIC_VERTEX_PROJECT_ID` and `CLOUD_ML_REGION`
    /// for vertex). The model resets to the new provider's default, because
    /// model ids differ between providers.
    SwitchProvider {
        /// Acp session id.
        session: String,
        /// Provider to route through.
        #[arg(value_parser = ["api", "bedrock", "vertex"])]
        provider: String,
    },
}

#[tracing::instrument(target = "cli.acp", skip_all)]
pub async fn run(command: AcpCommands) -> Result<()> {
    match command {
        AcpCommands::Doctor {
            json,
            fix,
            adapter,
            all_adapters,
        } => doctor(json, fix, adapter, all_adapters).await,
        AcpCommands::Agents => agents(),
        AcpCommands::Ps { .. } => ps_trap(),
        AcpCommands::Stop {
            session,
            all,
            timeout_secs,
        } => stop(session, all, timeout_secs).await,
        AcpCommands::Recover {
            session,
            confirm_prime_stopped,
            confirm_worker_stopped,
        } => recover(&session, confirm_prime_stopped, confirm_worker_stopped),
        AcpCommands::Kill { session } => kill_now(&session).await,
        AcpCommands::Logs { session, follow } => logs(session, follow),
        AcpCommands::Restart { session } => restart(&session).await,
        AcpCommands::History {
            session,
            since,
            json,
        } => history(&session, since, json).await,
        AcpCommands::Status { session, json } => status(&session, json).await,
        AcpCommands::Prompt { session, text } => prompt(&session, &text).await,
        AcpCommands::SetOption {
            session,
            config_id,
            value,
        } => set_option(&session, &config_id, &value).await,
        AcpCommands::Approve {
            session,
            nonce,
            always,
            deny,
            option,
        } => approve(&session, &nonce, always, deny, option).await,
        AcpCommands::Cancel { session } => cancel(&session).await,
        AcpCommands::Tail { session, since } => tail(&session, since).await,
        AcpCommands::Attach { session } => attach(&session).await,
        AcpCommands::SwitchAgent {
            session,
            target,
            model,
        } => switch_agent(&session, &target, model.as_deref()).await,
        AcpCommands::SwitchProvider { session, provider } => {
            switch_provider(&session, &provider).await
        }
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
    #[serde(skip_serializing_if = "crate::agents::AgentLifecycle::is_active")]
    lifecycle: crate::agents::AgentLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_issue: Option<AgentVersionIssue>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AgentVersionIssue {
    reason: String,
    install_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DoctorFixAction {
    PrintHint { reason: String },
    Skip,
}

fn doctor_fix_action(
    gate: Option<crate::acp::agent_compat::VersionGate>,
    probe: &crate::acp::version_probe::ProbeStatus,
) -> DoctorFixAction {
    use crate::acp::version_probe::ProbeStatus;
    let is_gated = gate.is_some();

    match probe {
        ProbeStatus::Missing if is_gated => DoctorFixAction::PrintHint {
            reason: "not found on PATH".to_string(),
        },
        ProbeStatus::Missing => DoctorFixAction::Skip,
        ProbeStatus::Version { parsed, .. } => {
            let Some(gate) = gate else {
                return DoctorFixAction::Skip;
            };
            let Ok(min) = semver::Version::parse(gate.min_version) else {
                return DoctorFixAction::Skip;
            };
            if parsed >= &min {
                return DoctorFixAction::Skip;
            }
            DoctorFixAction::PrintHint {
                reason: format!("installed {parsed}; requires >={}", gate.min_version),
            }
        }
        ProbeStatus::Unparseable { raw } if is_gated => DoctorFixAction::PrintHint {
            reason: format!("reported an unparseable version `{raw}`"),
        },
        ProbeStatus::Failed { message } if is_gated => DoctorFixAction::PrintHint {
            reason: format!("version probe failed: {message}"),
        },
        ProbeStatus::TimedOut if is_gated => DoctorFixAction::PrintHint {
            reason: "version probe timed out".to_string(),
        },
        ProbeStatus::Unparseable { .. } | ProbeStatus::Failed { .. } | ProbeStatus::TimedOut => {
            DoctorFixAction::Skip
        }
    }
}

fn skip_gate_check(binary: &str, on_path: bool) -> bool {
    !on_path && crate::acp::adapters::is_bundled(binary)
}

fn doctor_version_issue(
    gate: &crate::acp::agent_compat::VersionGate,
    probe: &crate::acp::version_probe::ProbeStatus,
    bundle_ok: bool,
) -> Option<AgentVersionIssue> {
    use crate::acp::version_probe::ProbeStatus;
    if matches!(probe, ProbeStatus::Missing) {
        return None;
    }
    match doctor_fix_action(Some(*gate), probe) {
        DoctorFixAction::Skip => None,
        DoctorFixAction::PrintHint { reason } => {
            let bundle_backs_spawn = match probe {
                ProbeStatus::Version { stdout_raw, .. } => semver::Version::parse(gate.min_version)
                    .is_ok_and(|min| {
                        crate::acp::version_probe::whitespace_token_below_floor(stdout_raw, min)
                    }),
                _ => false,
            };
            if bundle_ok && bundle_backs_spawn {
                return None;
            }
            Some(AgentVersionIssue {
                reason,
                install_command: gate.install_command.to_string(),
            })
        }
    }
}

fn bundled_copy_installed(binary: &str) -> bool {
    crate::session::get_app_dir().is_ok_and(|app_dir| bundled_copy_usable(&app_dir, binary))
}

fn bundled_copy_usable(app_dir: &std::path::Path, binary: &str) -> bool {
    crate::acp::adapters::bundled_adapter_bin(app_dir, binary).is_some()
        && !crate::acp::adapters::installed_copy_is_stale(app_dir, binary)
        && crate::acp::adapters::runtime_too_old_for(app_dir, binary).is_none()
}

async fn run_doctor_version_issue(
    gate: &crate::acp::agent_compat::VersionGate,
) -> Option<AgentVersionIssue> {
    let on_path = find_in_path(gate.binary).is_some();
    let bundle_installed = bundled_copy_installed(gate.binary);
    if !on_path && !bundle_installed {
        return None;
    }

    if !on_path {
        let strict = bundled_copy_strict_version(gate.binary).await;
        let min = semver::Version::parse(gate.min_version);
        if strict
            .as_ref()
            .is_some_and(|found| min.as_ref().is_ok_and(|min| found >= min))
        {
            return None;
        }
        let reason = match (strict, min) {
            (Some(found), Ok(min)) => {
                format!("installed {found} (aoe's pinned copy); requires >={min}")
            }
            (_, _) => {
                format!(
                    "the bundled copy did not report a usable version; requires >={}",
                    gate.min_version
                )
            }
        };
        return Some(AgentVersionIssue {
            reason,
            install_command: gate.install_command.to_string(),
        });
    }

    let probe = crate::acp::version_probe::probe_binary_version(gate.binary).await;
    let bundle_ok = bundle_installed && bundled_copy_meets_floor(gate).await;
    doctor_version_issue(gate, &probe, bundle_ok)
}

async fn bundled_copy_strict_version(binary: &str) -> Option<semver::Version> {
    let app_dir = crate::session::get_app_dir().ok()?;
    let path = crate::acp::adapters::bundled_adapter_bin(&app_dir, binary)?;
    match crate::acp::version_probe::probe_path_version(&path).await {
        crate::acp::version_probe::ProbeStatus::Version { stdout_raw, .. } => {
            crate::acp::version_probe::whitespace_token_semver(&stdout_raw)
        }
        _ => None,
    }
}

async fn bundled_copy_meets_floor(gate: &crate::acp::agent_compat::VersionGate) -> bool {
    let found = bundled_copy_strict_version(gate.binary).await;
    semver::Version::parse(gate.min_version).is_ok_and(|min| found.is_some_and(|v| v >= min))
}

async fn run_doctor_fix_action(binary: &str) {
    let gate = crate::acp::agent_compat::version_gate_for(
        crate::acp::agent_compat::ExpectedAgent::from_command(binary),
    );
    let probe = crate::acp::version_probe::probe_binary_version(binary).await;
    match doctor_fix_action(gate, &probe) {
        DoctorFixAction::PrintHint { reason } => {
            let hint = install_hint_for(binary).unwrap_or("(see project docs)");
            let bundle_installed = bundled_copy_installed(binary);
            if crate::acp::adapters::is_bundled(binary) && !bundle_installed {
                println!(
                    "{binary}: {reason}. That copy is on your PATH and no bundled copy is \
                     installed yet. Upgrade it ({hint}), or run `aoe acp doctor --fix` to \
                     install the pinned one."
                );
            } else if crate::acp::adapters::is_bundled(binary) {
                println!(
                    "{binary}: {reason}. aoe will use its pinned bundled copy for new sessions; \
                     upgrade the PATH copy ({hint}) or remove it to silence this."
                );
            } else {
                println!("{binary}: {reason}. Install manually: {hint}");
            }
        }
        DoctorFixAction::Skip => {}
    }
}

fn adapters_to_install(requested: &[String], all: bool) -> Result<Vec<&'static str>, Vec<String>> {
    use crate::acp::adapters;
    if all {
        return Ok(adapters::BUNDLED_ADAPTERS
            .iter()
            .map(|a| a.binary)
            .collect());
    }
    if requested.is_empty() {
        return Ok(vec![adapters::DEFAULT_ADAPTER]);
    }
    let (known, unknown): (Vec<_>, Vec<_>) = requested
        .iter()
        .partition(|name| adapters::is_bundled(name));
    if !unknown.is_empty() {
        return Err(unknown.into_iter().cloned().collect());
    }
    Ok(known
        .into_iter()
        .filter_map(|name| adapters::lookup(name).map(|a| a.binary))
        .collect())
}

async fn doctor(json: bool, fix: bool, adapter: Vec<String>, all_adapters: bool) -> Result<()> {
    if fix {
        match crate::session::get_app_dir() {
            Err(e) => println!(
                "Cannot resolve the app data dir ({e}); skipping the Node and adapter install."
            ),
            Ok(app_dir) => {
                let needs_sources =
                    adapters_to_install(&adapter, all_adapters).is_ok_and(|wanted| {
                        wanted
                            .iter()
                            .any(|b| crate::acp::adapters::ships_sources(b))
                    });
                let node = match node::resolve_for("", &app_dir, needs_sources) {
                    Ok(node) => {
                        println!("Node available: {} ({})", node.path.display(), node.version);
                        Some(node)
                    }
                    Err(node::NodeError::NoNode(_)) | Err(node::NodeError::TooOld { .. }) => {
                        println!("Downloading Node {} runtime...", node::PINNED_NODE_VERSION);
                        match node::download(&app_dir).await {
                            Ok(node) => {
                                println!(
                                    "Installed Node {} at {}",
                                    node.version,
                                    node.path.display()
                                );
                                Some(node)
                            }
                            Err(e) => {
                                println!("Node download failed: {e}");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        println!("Cannot probe Node: {e}");
                        None
                    }
                };
                if let Some(node) = node {
                    match adapters_to_install(&adapter, all_adapters) {
                        Err(unknown) => println!(
                            "Unknown adapter(s): {}. Valid values: {}.",
                            unknown.join(", "),
                            crate::acp::adapters::BUNDLED_ADAPTERS
                                .iter()
                                .map(|a| a.binary)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        Ok(wanted) => {
                            for binary in wanted {
                                println!("Installing bundled ACP adapter {binary}...");
                                match crate::acp::adapters::install(&app_dir, &node, binary) {
                                    Ok(()) => println!("{binary} ready."),
                                    Err(e) => println!("{binary} install failed: {e}"),
                                }
                            }
                        }
                    }
                }
            }
        }
        for gate in crate::acp::agent_compat::version_gates() {
            if skip_gate_check(gate.binary, find_in_path(gate.binary).is_some()) {
                continue;
            }
            run_doctor_fix_action(gate.binary).await;
        }
    }
    let registry = AgentRegistry::with_defaults();

    let node_status = check_node();
    let mut gate_issues: Vec<(&'static str, Option<AgentVersionIssue>)> = Vec::new();
    let mut agent_entries: Vec<AgentDoctorEntry> = Vec::new();
    for (name, spec) in registry.list() {
        let command_present = command_present(&spec.command);
        let version_issue = if command_present {
            let expected = crate::acp::agent_compat::ExpectedAgent::from_command(&spec.command);
            match crate::acp::agent_compat::version_gate_for(expected) {
                None => None,
                Some(gate) => {
                    match gate_issues
                        .iter()
                        .find(|(binary, _)| *binary == gate.binary)
                    {
                        Some((_, cached)) => cached.clone(),
                        None => {
                            let issue = run_doctor_version_issue(&gate).await;
                            gate_issues.push((gate.binary, issue.clone()));
                            issue
                        }
                    }
                }
            }
        } else {
            None
        };
        agent_entries.push(AgentDoctorEntry {
            lifecycle: registry_lifecycle(name),
            name: name.clone(),
            command_present,
            description: spec.description.clone(),
            version_issue,
        });
    }

    let any_agent_ok = agent_entries.iter().any(|e| e.command_present);
    let any_version_issue = agent_entries.iter().any(|e| e.version_issue.is_some());
    let node_ok = node_status.meets_minimum.unwrap_or(false);
    let overall = overall_status(node_ok, any_agent_ok, any_version_issue);
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
        let mark = agent_mark(entry);
        println!("{} {}  ({})", mark, entry.name, entry.description);
        if let Some(notice) = entry.lifecycle.notice() {
            println!("{}", crate::cli::lifecycle_notice_line("    ", &notice));
        }
        if !entry.command_present {
            if let Some(spec) = registry_for_hints.get(&entry.name) {
                let bin = spec.command.split('/').next_back().unwrap_or(&spec.command);
                if let Some(hint) = install_hint_for(bin) {
                    println!("    install: {hint}");
                }
            }
        } else if let Some(issue) = &entry.version_issue {
            println!("    {}", issue.reason);
            println!("    install: {}", issue.install_command);
        }
    }
    println!();
    println!("Overall: {}", overall);

    if overall != "ok" {
        std::process::exit(if overall == "partial" { 2 } else { 1 });
    }
    Ok(())
}

fn agent_mark(entry: &AgentDoctorEntry) -> &'static str {
    if entry.command_present && entry.version_issue.is_none() {
        "[OK]"
    } else {
        "[!! ]"
    }
}

fn overall_status(node_ok: bool, any_agent_ok: bool, any_version_issue: bool) -> &'static str {
    if node_ok && any_agent_ok && !any_version_issue {
        "ok"
    } else if node_ok || any_agent_ok {
        "partial"
    } else {
        "fail"
    }
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
            let meets = node::meets_minimum(&raw);
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

fn find_in_path(binary: &str) -> Option<String> {
    which::which(binary)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn bundled_adapter_names() -> Vec<&'static str> {
    crate::acp::adapters::BUNDLED_ADAPTERS
        .iter()
        .map(|a| a.binary)
        .collect()
}

pub(crate) fn command_present(command: &str) -> bool {
    if command.contains("${aoe_data_dir}") {
        return crate::session::get_app_dir()
            .map(|dir| {
                std::path::Path::new(&command.replace("${aoe_data_dir}", &dir.to_string_lossy()))
                    .exists()
            })
            .unwrap_or(false);
    }
    if command.contains("${") {
        false
    } else if command.contains('/') || command.contains('\\') {
        std::path::Path::new(command).exists()
    } else {
        find_in_path(command).is_some()
            || crate::session::get_app_dir()
                .ok()
                .is_some_and(|app_dir| bundled_copy_usable(&app_dir, command))
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
        if let Some(notice) = registry_lifecycle(name).notice() {
            println!("{}", crate::cli::lifecycle_notice_line("        ", &notice));
        }
        let args = if spec.args.is_empty() {
            String::new()
        } else {
            format!(" {}", spec.args.join(" "))
        };
        println!("        spawn: {}{}", spec.command, args);
    }
    Ok(())
}

pub(crate) fn ps_trap() -> Result<()> {
    anyhow::bail!(
        "`aoe acp ps` has been removed. Use the unified runtime view:\n  \
         aoe ps --acp                  live workers, with BUILD/MODEL/CWD/SOCKET\n  \
         aoe ps --acp --dead           every registry entry, as `acp ps` listed them\n  \
         aoe ps --acp --dead --json    same, machine-readable\n\
        \n\
        The JSON is a superset of the old schema (adds `substrate`, `state`, \
        `age_secs`, `model`), but rows now sort by substrate, then title, then \
        id rather than by `started_at`. Pipe through `jq 'sort_by(.started_at)'` \
        to restore the old order."
    )
}

async fn stop(session: Option<String>, all: bool, timeout_secs: u64) -> Result<()> {
    use crate::process::worker_registry;
    let targets: Vec<crate::process::worker_registry::WorkerRecord> = if all {
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
    stop_worker_records(&targets, timeout_secs).await?;
    Ok(())
}

pub(crate) async fn stop_all_workers(timeout_secs: u64) -> Result<usize> {
    use crate::process::worker_registry;
    let targets = worker_registry::list()?;
    stop_worker_records(&targets, timeout_secs).await?;
    Ok(targets.len())
}

async fn stop_worker_records(
    targets: &[crate::process::worker_registry::WorkerRecord],
    timeout_secs: u64,
) -> Result<()> {
    use crate::process::worker_registry;
    let mut failures = Vec::new();
    for record in targets {
        let result = async {
            worker_registry::request_stop(record)?;
            signal_and_wait(record, timeout_secs).await?;
            worker_registry::retire_selected(record)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => println!(
                "Stopped agent worker for {} (PID {}).",
                record.session_id, record.pid
            ),
            Err(error) => failures.push(format!("{}: {error:#}", record.session_id)),
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "Worker shutdown failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

fn recover(session: &str, confirm_prime_stopped: bool, confirm_worker_stopped: bool) -> Result<()> {
    if confirm_worker_stopped {
        crate::process::worker_registry::recover_worker(session)?;
        println!("Retired unprovable worker ownership for {session} on external shutdown confirmation; AoE did not verify all detached processes.");
        return Ok(());
    }
    anyhow::ensure!(
        confirm_prime_stopped,
        "external Prime shutdown must be confirmed"
    );
    let record = crate::process::worker_registry::load(session)?
        .ok_or_else(|| anyhow::anyhow!("No agent worker registry entry for session {session}"))?;
    crate::process::worker_registry::recover_untracked_prime(&record)?;
    println!(
        "Retired untracked Prime record for {} (PID {}, generation {}) on operator confirmation; AoE did not verify native shutdown.",
        record.session_id, record.pid, record.generation
    );
    Ok(())
}

async fn kill_now(session: &str) -> Result<()> {
    use crate::process::worker_registry;
    let Some(record) = worker_registry::load(session)? else {
        anyhow::bail!("No agent worker registry entry for session {session}");
    };
    worker_registry::request_stop(&record)?;
    signal_and_wait(&record, 0).await?;
    worker_registry::retire_selected(&record)?;
    println!("Killed agent worker for {} (PID {}).", session, record.pid);
    Ok(())
}

async fn signal_and_wait(
    record: &crate::process::worker_registry::WorkerRecord,
    timeout_secs: u64,
) -> Result<()> {
    use crate::process::worker_registry;
    worker_registry::settle_runner(record, std::time::Duration::from_secs(timeout_secs)).await?;
    let record = record.clone();
    tokio::task::spawn_blocking(move || worker_registry::cleanup_runtime(&record)).await??;
    Ok(())
}

fn logs(session: Option<String>, follow: bool) -> Result<()> {
    use crate::process::worker_registry;
    let id = match session {
        Some(s) => s,
        None => {
            let records = worker_registry::list().unwrap_or_default();
            if records.len() == 1 {
                records[0].session_id.clone()
            } else if records.is_empty() {
                println!("No agent workers running. Use `aoe ps --acp --dead` to inspect.");
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
        use std::io::{BufRead, BufReader, Seek, SeekFrom};
        let mut file = std::fs::File::open(&log_path)?;
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

async fn restart(session: &str) -> Result<()> {
    use crate::process::worker_registry;
    let Some(record) = worker_registry::load(session)? else {
        anyhow::bail!("No agent worker registry entry for session {session}");
    };
    let native = match record.source_profile.clone() {
        Some(profile) => {
            let id = session.to_owned();
            Some(
                tokio::task::spawn_blocking(move || -> Result<_> {
                    let storage = crate::session::Storage::open_unwatched(&profile)?;
                    let _lifecycle = storage.acquire_instance_lifecycle_lock(&id)?;
                    let row = storage
                        .load()?
                        .into_iter()
                        .find(|row| row.id == id)
                        .ok_or(crate::session::SessionGone)?;
                    row.ensure_startable()?;
                    anyhow::ensure!(
                        row.is_structured() && row.terminal_handoff.is_none(),
                        "session is no longer an available structured target"
                    );
                    Ok((storage, row.lifecycle_generation))
                })
                .await??,
            )
        }
        None => None,
    };
    let intent = worker_registry::request_stop(&record)?;
    let pid = record.pid;
    worker_registry::terminate_record_and_wait(record.clone(), ()).await?;
    if let Some((storage, generation)) = native {
        let id = session.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lifecycle = storage.acquire_instance_lifecycle_lock(&id)?;
            storage.update(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == id)
                    .ok_or(crate::session::SessionGone)?;
                row.ensure_startable()?;
                anyhow::ensure!(
                    row.is_structured() && row.terminal_handoff.is_none(),
                    "session is no longer an available structured target"
                );
                anyhow::ensure!(
                    row.lifecycle_generation == generation,
                    crate::session::LifecycleReservationError::Superseded
                );
                worker_registry::admit_resume(
                    &id,
                    worker_registry::ResumeAuthority::Explicit,
                    &Some(intent),
                )?;
                row.resume_structured_after_retirement(generation)?;
                Ok(())
            })
        })
        .await??;
    } else {
        worker_registry::admit_resume(
            session,
            worker_registry::ResumeAuthority::Explicit,
            &Some(intent),
        )?;
    }
    worker_registry::mark_restart_pending(session, record.generation)?;

    println!(
        "Stopped runner for {} (PID {}). `aoe serve` will respawn on its next reconciler tick.",
        session, pid
    );
    Ok(())
}

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
    let probe = client.replay(session, u64::MAX).await.map_err(map_http)?;
    if json {
        let blob = serde_json::json!({
            "session_id": session,
            "highest_seq": probe.highest_seq,
            "lowest_seq": probe.lowest_seq,
            "lost": probe.lost,
            "daemon_source": format!("{:?}", endpoint.source),
        });
        println!("{}", serde_json::to_string_pretty(&blob)?);
        return Ok(());
    }
    println!("Agent session: {session}");
    println!("  daemon source: {:?}", endpoint.source);
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
    client
        .prompt(session, &body, false)
        .await
        .map_err(map_http)?;
    println!("prompt accepted ({} bytes)", body.len());
    Ok(())
}

async fn set_option(session: &str, config_id: &str, value: &str) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    client
        .set_config_option(session, config_id, value)
        .await
        .map_err(map_http)?;
    println!("set {config_id}={value} requested");
    Ok(())
}

async fn approve(
    session: &str,
    nonce: &str,
    always: bool,
    deny: bool,
    option: Option<String>,
) -> Result<()> {
    let decision = match (always, deny) {
        (_, true) => ApprovalDecisionWire::Deny,
        (true, false) => ApprovalDecisionWire::AllowAlways,
        (false, false) => ApprovalDecisionWire::Allow,
    };
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let label = match &option {
        Some(id) => format!("{decision:?} (option {id})"),
        None => format!("{decision:?}"),
    };
    client
        .resolve_approval(session, nonce, decision, option)
        .await
        .map_err(map_http)?;
    println!("approval {nonce} -> {label}");
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

async fn switch_provider(session: &str, provider: &str) -> Result<()> {
    let endpoint = require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let resp = client
        .switch_provider(session, provider)
        .await
        .map_err(map_http)?;
    println!("switched provider for {session} -> {}", resp.provider);
    if resp.model_cleared {
        println!("model pick replaced by the provider's default; model ids are provider-specific");
    }
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
            Ok(WsMessage::TranscriptSnapshot(_))
            | Ok(WsMessage::TranscriptDelta(_))
            | Ok(WsMessage::ReducedState { .. }) => {}
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
        Event::AuthStatusUpdated { .. } => "auth_status_updated",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
        Event::BackgroundAgentLaunched { .. } => "background_agent_launched",
        Event::BackgroundAgentProgress { .. } => "background_agent_progress",
        Event::BackgroundAgentCompleted { .. } => "background_agent_completed",
        Event::BackgroundTaskStarted { .. } => "background_task_started",
        Event::BackgroundTaskFinished { .. } => "background_task_finished",
        Event::PromptRuntimeError { .. } => "prompt_runtime_error",
        Event::AcpConnectionUnavailable { .. } => "acp_connection_unavailable",
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
        Event::ConversationCompactionStarted => "conversation_compaction_started",
        Event::ConversationCompacted => "conversation_compacted",
        Event::ConversationCompactionFailed => "conversation_compaction_failed",
        Event::ConversationCompactionSummary { .. } => "conversation_compaction_summary",
        Event::ConversationSummary { .. } => "conversation_summary",
        Event::WakeupScheduled { .. } => "wakeup_scheduled",
        Event::MonitorArmed { .. } => "monitor_armed",
        Event::PromptRejected { .. } => "prompt_rejected",
        Event::AgentSwitched { .. } => "agent_switched",
        Event::SessionNotice { .. } => "session_notice",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn native_acp_restart_consumes_only_its_retired_observed_generation() {
        use crate::process::worker_registry::{self, WorkerRecord};
        use crate::session::{Instance, LifecycleOperation, Status, Storage, View};
        use tokio::io::AsyncWriteExt as _;
        for newer_stop in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let term = project.path().join("term");
            let ready = project.path().join("ready");
            let mut row = Instance::new("native CLI restart", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = Status::Stopped;
            row.mark_idle_dormant();
            let observed = row
                .try_acquire_lifecycle_reservation(
                    LifecycleOperation::Stop,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )
                .unwrap();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let mut child = tokio::process::Command::new("sh")
                .args(["-c", r#"trap 'printf term > "$1"; IFS= read -r release; exit 0' TERM; printf ready > "$2"; IFS= read -r initial"#, "sh"])
                .arg(&term).arg(&ready).stdin(std::process::Stdio::piped())
                .process_group(0).kill_on_drop(true).spawn().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let record = WorkerRecord::new(
                row.id.clone(),
                child.id().unwrap(),
                worker_registry::socket_path_for(&row.id).unwrap(),
                "sh".into(),
                "claude".into(),
                project.path().to_owned(),
                None,
                Vec::new(),
                Vec::new(),
                None,
                Some("default".into()),
            )
            .with_generation(101);
            assert!(record.runner_identity.is_some());
            worker_registry::save(&record).unwrap();
            let id = row.id.clone();
            let restarting = tokio::spawn(async move { restart(&id).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !term.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let storage = Storage::open_unwatched("default").unwrap();
            let newest = if newer_stop {
                Some(
                    storage
                        .update(|rows, _| {
                            let row = &mut rows[0];
                            row.release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Stop,
                                observed,
                            );
                            let generation = row.try_acquire_lifecycle_reservation(
                                LifecycleOperation::Stop,
                                Instance::LIFECYCLE_RESERVATION_TTL,
                                chrono::Utc::now(),
                            )?;
                            row.status = Status::Stopped;
                            row.mark_idle_dormant();
                            Ok(generation)
                        })
                        .unwrap(),
                )
            } else {
                None
            };
            child.stdin.take().unwrap().write_all(b"\n").await.unwrap();
            child.wait().await.unwrap();
            let result = restarting.await.unwrap();
            assert!(worker_registry::load(&row.id).unwrap().is_none());
            let stored = storage.load().unwrap().remove(0);
            if let Some(newest) = newest {
                let error = result.expect_err("newer native stop wins over earlier restart");
                assert!(error.is::<crate::session::LifecycleReservationError>());
                assert_eq!(stored.lifecycle_generation, newest);
                assert_eq!(stored.status, Status::Stopped);
                assert!(stored.is_idle_dormant());
                assert!(worker_registry::stop_intent(&row.id).unwrap().is_some());
            } else {
                result.unwrap();
                assert!(stored.lifecycle_generation > observed);
                assert_eq!(stored.status, Status::Idle);
                assert!(!stored.is_idle_dormant());
                assert!(stored.lifecycle_reservation.is_none());
                assert!(worker_registry::stop_intent(&row.id).unwrap().is_none());
                let admission = crate::acp::supervisor::LaunchAdmission {
                    store: std::sync::Arc::new(Storage::open_unwatched("default").unwrap()),
                    generation: stored.lifecycle_generation,
                    namespace: None,
                    native_scope: None,
                };
                let mut admitted = crate::acp::supervisor::with_durable_launch(
                    Some(admission),
                    row.id.clone(),
                    || {
                        std::process::Command::new("true").spawn().map_err(|error| {
                            crate::acp::acp_client::AcpError::Spawn(error.to_string())
                        })
                    },
                )
                .await
                .unwrap();
                assert!(admitted.wait().unwrap().success());
                let _ = worker_registry::claim_restart_marker(&row.id);
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn bulk_stop_settles_later_workers_after_native_failure() {
        use crate::process::worker_registry::{self, WorkerRecord};
        let _app = crate::session::test_support::isolate_app_dir();
        let make = |id: &str, key: &str| {
            WorkerRecord::new(
                id.into(),
                i32::MAX as u32,
                worker_registry::socket_path_for(id).unwrap(),
                key.into(),
                key.into(),
                "/tmp".into(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
        };
        let failed = make("untracked-prime", "prime-agent");
        let healthy = make("healthy-worker", "aoe-agent");
        for record in [&failed, &healthy] {
            worker_registry::save(record).unwrap();
        }
        let error = stop_worker_records(&[failed.clone(), healthy], 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("untracked-prime"));
        assert_eq!(
            worker_registry::load(&failed.session_id)
                .unwrap()
                .unwrap()
                .pid,
            failed.pid
        );
        assert!(worker_registry::load("healthy-worker").unwrap().is_none());
    }

    #[test]
    fn registry_lifecycle_mirrors_agents_registry() {
        let cases = [
            ("gemini", false),
            ("claude", true),
            ("codex", true),
            ("opencode", true),
            ("no-such-adapter", true),
        ];
        for (key, active) in cases {
            assert_eq!(registry_lifecycle(key).is_active(), active, "{key}");
        }
    }

    #[test]
    fn doctor_entry_json_omits_active_lifecycle() {
        let active = AgentDoctorEntry {
            name: "claude".to_string(),
            command_present: true,
            description: "claude adapter".to_string(),
            lifecycle: registry_lifecycle("claude"),
            version_issue: None,
        };
        let value = serde_json::to_value(&active).unwrap();
        assert!(value.get("lifecycle").is_none(), "{value}");

        let deprecated = AgentDoctorEntry {
            name: "gemini".to_string(),
            command_present: false,
            description: "gemini adapter".to_string(),
            lifecycle: registry_lifecycle("gemini"),
            version_issue: None,
        };
        let value = serde_json::to_value(&deprecated).unwrap();
        assert_eq!(value["lifecycle"]["replacement"], "antigravity", "{value}");
        assert_eq!(value["lifecycle"]["since"], "2026-06-18", "{value}");
    }

    #[test]
    fn doctor_fix_hints_only_for_a_gated_adapter_off_its_floor() {
        use crate::acp::agent_compat::{
            version_gate_for, ExpectedAgent, CLAUDE_AGENT_ACP_MIN_VERSION,
        };
        use crate::acp::version_probe::ProbeStatus;

        let ver = |v: &str| ProbeStatus::Version {
            raw: v.to_string(),
            parsed: semver::Version::parse(v).unwrap(),
            stdout_raw: v.to_string(),
        };
        let unparseable = || ProbeStatus::Unparseable {
            raw: "weird".to_string(),
        };
        let claude = version_gate_for(ExpectedAgent::ClaudeAgentAcp);
        let opencode = version_gate_for(ExpectedAgent::OpenCode);

        let cases = [
            ("missing gated adapter", claude, ProbeStatus::Missing, true),
            ("stale gated adapter", claude, ver("0.0.1"), true),
            ("unparseable gated adapter", claude, unparseable(), true),
            ("stale non-npm adapter", opencode, ver("1.15.0"), true),
            (
                "gated adapter at its floor",
                claude,
                ver(CLAUDE_AGENT_ACP_MIN_VERSION),
                false,
            ),
            ("ungated, unparseable", None, unparseable(), false),
            ("ungated, missing", None, ProbeStatus::Missing, false),
        ];
        for (label, gate, probe, hints) in cases {
            let action = doctor_fix_action(gate, &probe);
            assert_eq!(
                matches!(action, DoctorFixAction::PrintHint { .. }),
                hints,
                "{label}: {action:?}"
            );
        }
    }

    #[test]
    fn skip_gate_check_only_skips_absent_bundled_adapters() {
        assert!(skip_gate_check("claude-agent-acp", false));
        assert!(!skip_gate_check("claude-agent-acp", true));
        assert!(!skip_gate_check("opencode", false));
        assert!(!skip_gate_check("opencode", true));
    }

    fn claude_gate() -> crate::acp::agent_compat::VersionGate {
        crate::acp::agent_compat::version_gate_for(
            crate::acp::agent_compat::ExpectedAgent::ClaudeAgentAcp,
        )
        .expect("claude-agent-acp must carry a version gate")
    }

    #[test]
    fn doctor_version_issue_verdicts() {
        use crate::acp::version_probe::ProbeStatus;
        let gate = claude_gate();
        let ver = |v: &str| ProbeStatus::Version {
            raw: v.to_string(),
            parsed: semver::Version::parse(v).unwrap(),
            stdout_raw: v.to_string(),
        };
        let cases: Vec<(&str, ProbeStatus, bool, bool)> = vec![
            (
                "at_floor",
                ver(crate::acp::agent_compat::CLAUDE_AGENT_ACP_MIN_VERSION),
                false,
                false,
            ),
            ("above_floor", ver("1.0.0"), false, false),
            ("stale_but_bundled", ver("0.37.0"), true, false),
            (
                "lenient_raw_but_bundled",
                ProbeStatus::Version {
                    raw: "version=0.37.0".to_string(),
                    parsed: semver::Version::parse("0.37.0").unwrap(),
                    stdout_raw: "version=0.37.0".to_string(),
                },
                true,
                true,
            ),
            (
                "stderr_only_but_bundled",
                ProbeStatus::Version {
                    raw: "0.37.0".to_string(),
                    parsed: semver::Version::parse("0.37.0").unwrap(),
                    stdout_raw: String::new(),
                },
                true,
                true,
            ),
            ("missing_but_bundled", ProbeStatus::Missing, true, false),
            ("absent_unbundled", ProbeStatus::Missing, false, false),
            (
                "unparseable",
                ProbeStatus::Unparseable {
                    raw: "junk".to_string(),
                },
                false,
                true,
            ),
            (
                "unparseable_but_bundled",
                ProbeStatus::Unparseable {
                    raw: "junk".to_string(),
                },
                true,
                true,
            ),
            (
                "failed",
                ProbeStatus::Failed {
                    message: "boom".to_string(),
                },
                false,
                true,
            ),
            (
                "failed_but_bundled",
                ProbeStatus::Failed {
                    message: "boom".to_string(),
                },
                true,
                true,
            ),
            ("timed_out", ProbeStatus::TimedOut, false, true),
            ("timed_out_but_bundled", ProbeStatus::TimedOut, true, true),
        ];
        for (label, probe, bundled, expect_issue) in cases {
            let issue = doctor_version_issue(&gate, &probe, bundled);
            assert_eq!(issue.is_some(), expect_issue, "{label}: {issue:?}");
        }
        let opencode = crate::acp::agent_compat::version_gate_for(
            crate::acp::agent_compat::ExpectedAgent::OpenCode,
        )
        .expect("opencode must carry a version gate");
        let issue = doctor_version_issue(&opencode, &ver("1.15.0"), false)
            .expect("stale opencode must produce a version issue");
        assert_eq!(issue.install_command, opencode.install_command);

        let issue = doctor_version_issue(&gate, &ver("0.37.0"), false)
            .expect("a below-floor adapter must produce a version issue");
        assert!(issue.reason.contains("0.37.0"), "{}", issue.reason);
        assert!(
            issue.reason.contains(gate.min_version),
            "the reason must name the required floor: {}",
            issue.reason
        );
        assert_eq!(issue.install_command, gate.install_command);
    }

    #[test]
    fn agent_mark_demotes_on_version_issue() {
        let entry = |present: bool, issue: Option<AgentVersionIssue>| AgentDoctorEntry {
            name: "claude".to_string(),
            command_present: present,
            description: String::new(),
            lifecycle: registry_lifecycle("claude"),
            version_issue: issue,
        };
        let stale_issue = AgentVersionIssue {
            reason: "installed 0.37.0; requires >=0.99.1".to_string(),
            install_command: "npm install -g @x/y@latest".to_string(),
        };
        let marks = [
            (entry(true, None), "[OK]"),
            (entry(true, Some(stale_issue)), "[!! ]"),
            (entry(false, None), "[!! ]"),
        ];
        for (e, mark) in &marks {
            assert_eq!(&agent_mark(e), mark);
        }
    }

    #[test]
    fn overall_status_caps_at_partial_on_version_issue() {
        let cases = [
            (true, true, false, "ok"),
            (true, true, true, "partial"),
            (true, false, false, "partial"),
            (false, true, false, "partial"),
            (false, false, false, "fail"),
        ];
        for (node_ok, agents_ok, stale, expected) in cases {
            assert_eq!(overall_status(node_ok, agents_ok, stale), expected);
        }
    }

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

    #[test]
    fn ps_trap_fails_and_names_the_replacement_flags() {
        let err = ps_trap().expect_err("the trap must exit non-zero");
        let msg = err.to_string();
        for needle in ["aoe ps --acp", "--dead", "--json", "started_at"] {
            assert!(
                msg.contains(needle),
                "redirect must mention {needle}: {msg}"
            );
        }
        assert!(!msg.contains('\u{2014}'), "no emdash separators: {msg}");
    }
}
