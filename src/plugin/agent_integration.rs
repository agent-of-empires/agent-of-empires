//! Secure provider-native session reports and identity-checked tmux replies.

use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd,
    os::unix::fs::MetadataExt,
    sync::{LazyLock, Mutex},
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use aoe_plugin_api::agent_integration::{
    launch_prefix, AgentCaptureBackend as DeclaredCaptureBackend,
    AgentCaptureContext as DeclaredCaptureContext, AgentDetectionDeclaration,
    AgentIntegrationContribution, AgentIntegrationPrompt, AgentIntegrationReply,
    AgentIntegrationResponse, AgentIntegrationSnapshot, AgentIntegrationStatus,
    AgentKeyDeclaration, AgentLaunchDeclaration, AgentReplyDeclaration, AgentResumeDeclaration,
    AgentYoloDeclaration, IntegrationApprovalChoice,
};

use crate::agents::{
    AgentDef, AgentLifecycle, DetectionMethod, ForkStrategy, KeyToken, PermissionResponse,
    ResumeStrategy, SessionCaptureBackend, SessionCaptureContext, SessionCaptureSpec,
    SessionSupport, YoloMode,
};
use crate::{hooks::dir_guard, session::Instance};

const MAX_REPORT_BYTES: usize = 64 * 1024;
const REPORT_MAX_AGE: Duration = Duration::from_secs(15);

fn provider<'a>(
    registry: &'a super::PluginRegistry,
    tool: &str,
) -> Result<Option<&'a super::LoadedPlugin>> {
    let mut matches = registry.active().filter(|plugin| {
        plugin
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "session.integration")
            && plugin
                .manifest
                .agent_integrations
                .iter()
                .any(|entry| entry.tool == tool)
    });
    let selected = matches.next();
    ensure!(
        matches.next().is_none(),
        "multiple approved integrations claim {tool}"
    );
    Ok(selected)
}

/// Collisions already reported for a `(plugin, tool)`, so the reserved-name
/// warning fires once per process rather than on every catalog lookup.
static RESERVED_WARNED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// An active plugin-provided agent that does not collide with a built-in or custom agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginAgent<'a> {
    pub plugin: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
}

fn unique_candidates<'a, 'b, 'c>(
    candidates: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    builtin_names: impl IntoIterator<Item = &'b str>,
    custom_names: impl IntoIterator<Item = &'c str>,
) -> Vec<PluginAgent<'a>> {
    let mut candidate_counts = HashMap::<&str, usize>::new();
    let candidates: Vec<_> = candidates.into_iter().collect();
    for (_, tool, _) in &candidates {
        *candidate_counts.entry(tool).or_default() += 1;
    }
    for name in builtin_names {
        candidate_counts.remove(name);
    }
    for name in custom_names {
        candidate_counts.remove(name);
    }
    let mut warned = RESERVED_WARNED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (plugin, tool, _) in &candidates {
        if let Some(builtin) = crate::agents::resolve_builtin_tool_name(tool) {
            // Warn once per (plugin, tool) per process: catalog runs on every
            // agents listing, detection, launch-command resolution and session
            // validation, so repeating it would flood the log.
            if warned.insert((plugin.to_string(), tool.to_string())) {
                tracing::warn!(
                    target: "plugin.agents",
                    plugin = %plugin,
                    tool = %tool,
                    builtin = %builtin,
                    "plugin agent tool name contains a built-in name or alias; preserving built-in precedence"
                );
            }
            candidate_counts.remove(tool);
        }
    }
    drop(warned);
    candidates
        .into_iter()
        .filter_map(|(plugin, tool, command)| {
            (candidate_counts.get(tool) == Some(&1)).then_some(PluginAgent {
                plugin,
                tool,
                command,
            })
        })
        .collect()
}

/// Catalog unique plugin launch commands after built-in and custom agents take precedence.
pub fn catalog<'a, 'b, 'c>(
    registry: &'a super::PluginRegistry,
    builtin_names: impl IntoIterator<Item = &'b str>,
    custom_names: impl IntoIterator<Item = &'c str>,
) -> Vec<PluginAgent<'a>> {
    let candidates = registry.active().flat_map(|plugin| {
        plugin
            .manifest
            .agent_integrations
            .iter()
            .filter_map(move |entry| {
                entry
                    .command
                    .as_deref()
                    .map(|command| (plugin.id(), entry.tool.as_str(), command))
            })
    });
    unique_candidates(candidates, builtin_names, custom_names)
}

/// Whether a character could spoof or rewrite displayed text: control
/// characters (Cc) and Unicode format characters (Cf), especially the bidi
/// controls a browser or terminal would otherwise reorder.
fn display_dangerous(c: char) -> bool {
    c.is_control()
        || unicode_general_category::get_general_category(c)
            == unicode_general_category::GeneralCategory::Format
}

/// Display form of a plugin-declared command: control and format characters
/// (terminal escape sequences, which a malicious manifest could otherwise use
/// to rewrite the approval prompt, and bidi characters that would reorder it)
/// become visible `\x..` escapes. The runnable command is never altered, only
/// this disclosure string.
pub fn display_command(command: &str) -> String {
    if !command.chars().any(display_dangerous) {
        return command.to_string();
    }
    let mut out = String::with_capacity(command.len());
    for character in command.chars() {
        if display_dangerous(character) {
            out.extend(character.escape_default());
        } else {
            out.push(character);
        }
    }
    out
}

/// Resolve a command to a plugin-declared tool without consulting built-ins.
pub(crate) fn resolve_declared_tool(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for plugin in super::registry().active() {
        for entry in &plugin.manifest.agent_integrations {
            let Some(agent) = declared_agent(&entry.tool) else {
                continue;
            };
            for token in std::iter::once(agent.name)
                .chain(std::iter::once(agent.binary))
                .chain(agent.aliases.iter().copied())
            {
                let token_lower = token.to_lowercase();
                let matches = cmd_lower
                    .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')));
                if matches.into_iter().any(|part| part == token_lower)
                    && best.is_none_or(|(len, _)| token_lower.len() > len)
                {
                    best = Some((token_lower.len(), agent.name));
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Resolve a unique plugin-provided agent launch command.
pub fn agent_command<'a>(
    tool: &str,
    custom_names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let registry = super::registry();
    catalog(
        &registry,
        crate::agents::AGENTS.iter().map(|agent| agent.name),
        custom_names,
    )
    .into_iter()
    .find(|agent| agent.tool == tool)
    .map(|agent| {
        tracing::debug!(
            target: "plugin.agents",
            tool,
            plugin = agent.plugin,
            "resolved plugin-provided agent command"
        );
        agent.command.to_string()
    })
}

fn eligible(instance: &Instance) -> bool {
    !instance.is_structured()
        && !instance.is_archived()
        && !instance.is_sandboxed()
        && !matches!(
            instance.status,
            crate::session::Status::Stopped
                | crate::session::Status::Deleting
                | crate::session::Status::Creating
        )
}

pub fn configured(instance: &Instance) -> bool {
    configured_for(instance, false)
}

/// Whether an approved integration owns replies, even when its report is unavailable.
pub fn replies_configured(instance: &Instance) -> bool {
    configured_for(instance, true)
}

fn configured_for(instance: &Instance, replies: bool) -> bool {
    eligible(instance)
        && super::registry().active().any(|plugin| {
            plugin
                .manifest
                .capabilities
                .iter()
                .any(|c| c.as_str() == "session.integration")
                && plugin
                    .manifest
                    .agent_integrations
                    .iter()
                    .any(|entry| entry.tool == instance.tool && (!replies || entry.replies))
        })
}

/// Return a fresh verified provider snapshot when available.
pub fn snapshot(instance: &Instance) -> Option<AgentIntegrationSnapshot> {
    if !eligible(instance) {
        return None;
    }
    let registry = super::registry();
    let plugin = provider(&registry, &instance.tool).ok()??;
    read_report(&instance.id, &instance.tool, plugin)
        .ok()
        .flatten()
}

pub(crate) fn session_status(status: AgentIntegrationStatus) -> crate::session::Status {
    match status {
        AgentIntegrationStatus::Running => crate::session::Status::Running,
        AgentIntegrationStatus::Waiting => crate::session::Status::Waiting,
        AgentIntegrationStatus::Idle => crate::session::Status::Idle,
        AgentIntegrationStatus::Error => crate::session::Status::Error,
    }
}
/// An approved integration owns status for its tool, so a missing, stale, or
/// invalid report is Unknown rather than permission to parse the pane instead.
pub(crate) fn session_status_override(
    configured: bool,
    pane_dead: bool,
    status: Option<AgentIntegrationStatus>,
) -> Option<crate::session::Status> {
    if !configured {
        None
    } else if pane_dead {
        Some(crate::session::Status::Error)
    } else {
        Some(status.map_or(crate::session::Status::Unknown, session_status))
    }
}

#[derive(Clone)]
struct DeclaredAgent {
    tool: String,
    command: Option<String>,
    launch: Option<AgentLaunchDeclaration>,
    detection: Option<AgentDetectionDeclaration>,
    reply_fallback: Option<AgentReplyDeclaration>,
    status: bool,
}

fn declared_for(tool: &str) -> Option<DeclaredAgent> {
    let registry = super::registry();
    let mut candidates = registry.active().flat_map(|plugin| {
        plugin
            .manifest
            .agent_integrations
            .iter()
            .filter(move |entry| entry.tool == tool)
            .map(move |entry| (plugin, entry))
    });
    let (plugin, entry) = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(DeclaredAgent {
        tool: entry.tool.clone(),
        command: entry.command.clone(),
        launch: entry.launch.clone(),
        detection: entry.detection.clone(),
        reply_fallback: entry.reply_fallback.clone(),
        status: plugin
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "session.integration"),
    })
}

static DECLARED_AGENT_CACHE: LazyLock<Mutex<HashMap<String, Option<&'static AgentDef>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn clear_declared_agent_cache() {
    DECLARED_AGENT_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// A plugin-declared tool resolved into the host's agent catalog. Built-in
/// entries win in [`crate::agents::get_agent`], so a declaration only takes
/// effect once the matching core entry is deleted.
pub(crate) fn declared_agent(tool: &str) -> Option<&'static AgentDef> {
    if let Some(cached) = DECLARED_AGENT_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(tool)
        .copied()
    {
        return cached;
    }
    let resolved = declared_for(tool).and_then(|declared| {
        let declares_agent = declared.launch.is_some()
            || declared.detection.is_some()
            || declared.reply_fallback.is_some();
        declares_agent.then(|| build_declared_agent(declared))
    });
    DECLARED_AGENT_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(tool.to_string(), resolved);
    resolved
}

/// Whether an approved provider integration owns status for `tool`.
pub(crate) fn status_owned(tool: &str) -> bool {
    let registry = super::registry();
    let owned = registry.active().any(|plugin| {
        plugin
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "session.integration")
            && plugin
                .manifest
                .agent_integrations
                .iter()
                .any(|entry| entry.tool == tool)
    });
    owned
}

pub(crate) fn launch_executable(command: &str) -> Option<String> {
    let (words, _) = launch_prefix(command)?;
    words.last().map(|word| (*word).to_string())
}

fn leaked(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn leak_slice<T>(values: Vec<T>) -> &'static [T] {
    Box::leak(values.into_boxed_slice())
}

fn native_only_status(_: &str) -> crate::session::Status {
    crate::session::Status::Unknown
}

fn idle_status(_: &str) -> crate::session::Status {
    crate::session::Status::Idle
}

fn capture_backend(backend: DeclaredCaptureBackend) -> SessionCaptureBackend {
    match backend {
        DeclaredCaptureBackend::Claude => SessionCaptureBackend::Claude,
        DeclaredCaptureBackend::OpenCode => SessionCaptureBackend::OpenCode,
        DeclaredCaptureBackend::Codex => SessionCaptureBackend::Codex,
        DeclaredCaptureBackend::Gemini => SessionCaptureBackend::Gemini,
        DeclaredCaptureBackend::Hermes => SessionCaptureBackend::Hermes,
        DeclaredCaptureBackend::Kimi => SessionCaptureBackend::Kimi,
        DeclaredCaptureBackend::Pi => SessionCaptureBackend::Pi,
        DeclaredCaptureBackend::PrimeAgent => SessionCaptureBackend::PrimeAgent,
        DeclaredCaptureBackend::Omp => SessionCaptureBackend::Omp,
    }
}

fn capture_context(context: DeclaredCaptureContext) -> SessionCaptureContext {
    match context {
        DeclaredCaptureContext::PaneScoped => SessionCaptureContext::PaneScoped,
        DeclaredCaptureContext::Preassigned => SessionCaptureContext::Preassigned,
        DeclaredCaptureContext::ManagedExclusiveStore => {
            SessionCaptureContext::ManagedExclusiveStore
        }
        DeclaredCaptureContext::Unsupported => SessionCaptureContext::Unsupported,
    }
}

fn resume_strategy(resume: &AgentResumeDeclaration) -> ResumeStrategy {
    match resume {
        AgentResumeDeclaration::Flag { flag } => ResumeStrategy::Flag(leaked(flag.clone())),
        AgentResumeDeclaration::FlagPair {
            existing,
            new_session,
        } => ResumeStrategy::FlagPair {
            existing: leaked(existing.clone()),
            new_session: leaked(new_session.clone()),
        },
        AgentResumeDeclaration::Subcommand { command } => {
            ResumeStrategy::Subcommand(leaked(command.clone()))
        }
    }
}

fn key_token(key: &AgentKeyDeclaration) -> KeyToken<'static> {
    match key {
        AgentKeyDeclaration::Literal { value } => KeyToken::Literal(leaked(value.clone())),
        AgentKeyDeclaration::Named { value } => KeyToken::Named(leaked(value.clone())),
    }
}

fn build_declared_agent(declared: DeclaredAgent) -> &'static AgentDef {
    let tool = leaked(declared.tool.clone());
    let binary = leaked(
        declared
            .command
            .as_deref()
            .and_then(launch_executable)
            .unwrap_or_else(|| declared.tool.clone()),
    );
    let launch = declared.launch.unwrap_or_default();
    let aliases = leak_slice(
        launch
            .aliases
            .iter()
            .map(|alias| leaked(alias.clone()))
            .collect(),
    );
    let container_env = leak_slice(
        launch
            .container_env
            .iter()
            .map(|(key, value)| (leaked(key.clone()), leaked(value.clone())))
            .collect(),
    );
    let session_support = match (&launch.resume, &launch.capture) {
        (Some(resume), Some(capture)) => Some(SessionSupport {
            resume: resume_strategy(resume),
            capture: Some(SessionCaptureSpec {
                backend: capture_backend(capture.backend),
                host: capture_context(capture.host),
                sandbox: capture_context(capture.sandbox),
            }),
        }),
        (Some(resume), None) => Some(SessionSupport {
            resume: resume_strategy(resume),
            capture: None,
        }),
        _ => None,
    };
    let permission_response = declared
        .reply_fallback
        .as_ref()
        .map(|reply| PermissionResponse {
            allow: leak_slice(reply.allow.iter().map(key_token).collect()),
            allow_always: reply
                .allow_always
                .as_ref()
                .map(|keys| leak_slice(keys.iter().map(key_token).collect())),
            deny: leak_slice(reply.deny.iter().map(key_token).collect()),
        });
    let yolo = launch.yolo.as_ref().map(|yolo| match yolo {
        AgentYoloDeclaration::Flag { flag } => YoloMode::CliFlag(leaked(flag.clone())),
        AgentYoloDeclaration::Always => YoloMode::AlwaysYolo,
        AgentYoloDeclaration::Env { key, value } => {
            YoloMode::EnvVar(leaked(key.clone()), leaked(value.clone()))
        }
    });
    let detect_status = if declared.status || declared.detection.is_some() {
        native_only_status
    } else {
        idle_status
    };
    Box::leak(Box::new(AgentDef {
        name: tool,
        binary,
        launch_subcommand: None,
        aliases,
        detection: DetectionMethod::Which(binary),
        yolo,
        spans_agent_generations: false,
        instruction_flag: launch
            .instruction_flag
            .as_deref()
            .map(|value| leaked(value.to_string())),
        oneshot_flag: launch
            .oneshot_flag
            .as_deref()
            .map(|value| leaked(value.to_string())),
        model_flag: launch
            .model_flag
            .as_deref()
            .map(|value| leaked(value.to_string())),
        set_default_command: false,
        detect_status,
        container_env,
        hook_config: None,
        sidecar_hooks: None,
        session_support,
        fork_strategy: ForkStrategy::Unsupported,
        host_only: launch.host_only,
        send_keys_enter_delay_ms: 0,
        ready_marker: None,
        install_hint: leaked(
            launch
                .install_hint
                .clone()
                .unwrap_or_else(|| format!("install {tool}")),
        ),
        permission_response,
        lifecycle: AgentLifecycle::Active,
    }))
}

fn key_label(key: &AgentKeyDeclaration) -> String {
    match key {
        AgentKeyDeclaration::Literal { value } => format!("literal {value:?}"),
        AgentKeyDeclaration::Named { value } => format!("key {value}"),
    }
}

/// One disclosure entry per declaration so install and update previews name
/// every behavior a plugin adds.
pub fn declaration_summary(integration: &AgentIntegrationContribution) -> Vec<String> {
    let mut lines = vec![format!(
        "{}: {}",
        integration.tool,
        integration
            .command
            .as_deref()
            .map(display_command)
            .unwrap_or_else(|| "No launch command declared".to_string())
    )];
    if let Some(launch) = &integration.launch {
        let mut flags = Vec::new();
        if let Some(flag) = &launch.oneshot_flag {
            flags.push(format!("oneshot {flag}"));
        }
        if let Some(flag) = &launch.instruction_flag {
            flags.push(format!("instruction {flag}"));
        }
        if let Some(flag) = &launch.model_flag {
            flags.push(format!("model {flag}"));
        }
        if let Some(yolo) = &launch.yolo {
            flags.push(match yolo {
                AgentYoloDeclaration::Flag { flag } => format!("yolo {flag}"),
                AgentYoloDeclaration::Always => "yolo always".to_string(),
                AgentYoloDeclaration::Env { key, value } => format!("yolo {key}={value}"),
            });
        }
        if !flags.is_empty() {
            lines.push(format!("  launch flags: {}", flags.join("; ")));
        }
        if !launch.container_env.is_empty() {
            let env = launch
                .container_env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("  sandbox environment: {env}"));
        }
        if !launch.aliases.is_empty() {
            lines.push(format!("  aliases: {}", launch.aliases.join(", ")));
        }
        if let Some(resume) = &launch.resume {
            let resume = match resume {
                AgentResumeDeclaration::Flag { flag } => format!("flag {flag}"),
                AgentResumeDeclaration::FlagPair {
                    existing,
                    new_session,
                } => format!("flags {existing} / {new_session}"),
                AgentResumeDeclaration::Subcommand { command } => {
                    format!("subcommand {command}")
                }
            };
            let capture = launch.capture.as_ref().map_or_else(
                || "none".to_string(),
                |capture| {
                    format!(
                        "{:?} (host {:?}, sandbox {:?})",
                        capture.backend, capture.host, capture.sandbox
                    )
                },
            );
            lines.push(format!("  resume: {resume}; capture: {capture}"));
        }
        if launch.host_only {
            lines.push("  host only: yes".to_string());
        }
        if let Some(hint) = &launch.install_hint {
            lines.push(format!("  install: {hint}"));
        }
    }
    if integration.detection.is_some() {
        lines.push("  detection: native_only".to_string());
    }
    if let Some(replies) = &integration.reply_fallback {
        let allow = replies
            .allow
            .iter()
            .map(key_label)
            .collect::<Vec<_>>()
            .join(", ");
        let deny = replies
            .deny
            .iter()
            .map(key_label)
            .collect::<Vec<_>>()
            .join(", ");
        let always = replies
            .allow_always
            .as_ref()
            .map(|keys| keys.iter().map(key_label).collect::<Vec<_>>().join(", "));
        lines.push(format!(
            "  reply fallback: allow [{allow}], deny [{deny}]{};",
            always.map_or_else(String::new, |keys| format!(", allow always [{keys}]"))
        ));
    }
    lines
}

fn check_file(meta: &std::fs::Metadata) -> Result<()> {
    ensure!(
        meta.uid() == nix::unistd::geteuid().as_raw(),
        "integration report has another owner"
    );
    ensure!(
        meta.mode() & 0o7777 == 0o600,
        "integration report must be mode 0600"
    );
    ensure!(
        meta.len() <= MAX_REPORT_BYTES as u64,
        "integration report exceeds size limit"
    );
    let modified = meta.modified()?;
    ensure!(
        modified.elapsed().is_ok_and(|age| age <= REPORT_MAX_AGE),
        "integration report expired"
    );
    Ok(())
}

fn read_report(
    instance_id: &str,
    tool: &str,
    plugin: &super::LoadedPlugin,
) -> Result<Option<AgentIntegrationSnapshot>> {
    let Some(dir) = dir_guard::open_instance_dir_read_only(instance_id)? else {
        return Ok(None);
    };
    let leaf = format!("integration-{}.json", plugin.id());
    let Some((meta, bytes)) =
        dir_guard::read_file_with_metadata_at(dir.as_fd(), &leaf, MAX_REPORT_BYTES + 1)?
    else {
        return Ok(None);
    };
    check_file(&meta)?;
    ensure!(
        bytes.len() <= MAX_REPORT_BYTES,
        "integration report exceeds size limit"
    );
    let mut report: AgentIntegrationSnapshot =
        serde_json::from_slice(&bytes).context("invalid integration report")?;
    validate_report(&report, instance_id)?;
    let integration = plugin
        .manifest
        .agent_integrations
        .iter()
        .find(|entry| entry.tool == tool)
        .context("provider integration declaration is unavailable")?;
    integration
        .provider_compat(&report.provider_version)
        .map_err(anyhow::Error::msg)?;
    if !integration.replies {
        report.prompt = None;
    }
    sanitize_prompt(&mut report);
    ensure!(
        crate::process::worker::is_pid_alive(report.writer_pid),
        "integration writer exited"
    );
    Ok(Some(report))
}

fn validate_report(r: &AgentIntegrationSnapshot, id: &str) -> Result<()> {
    ensure!(
        r.schema_version == 1 && r.instance_id == id,
        "integration identity mismatch"
    );
    ensure!(
        !r.provider_session_id.is_empty() && r.provider_session_id.len() <= 128,
        "invalid provider session identity"
    );
    ensure!(
        r.writer_pid > 0 && r.writer_pid <= i32::MAX as u32,
        "invalid integration writer PID"
    );
    ensure!(r.reply_keys.len() <= 8, "invalid integration reply keys");
    let mut reply_keys = HashSet::new();
    for key in &r.reply_keys {
        ensure!(
            matches!(
                key.as_str(),
                "F1" | "F2"
                    | "F3"
                    | "F4"
                    | "F5"
                    | "F6"
                    | "F7"
                    | "F8"
                    | "F9"
                    | "F10"
                    | "F11"
                    | "F12"
            ),
            "invalid integration reply key"
        );
        ensure!(reply_keys.insert(key), "duplicate integration reply key");
    }
    Ok(())
}

fn valid_prompt(prompt: &AgentIntegrationPrompt, status: AgentIntegrationStatus) -> bool {
    if status != AgentIntegrationStatus::Waiting
        || prompt.id().is_empty()
        || prompt.id().len() > 256
    {
        return false;
    }
    let AgentIntegrationPrompt::Question { questions, .. } = prompt else {
        return true;
    };
    if questions.is_empty() {
        return false;
    }
    let mut ids = HashSet::new();
    for question in questions {
        if question.id.is_empty() || !ids.insert(&question.id) {
            return false;
        }
        let mut labels = HashSet::new();
        if question
            .options
            .iter()
            .any(|option| option.label.is_empty() || !labels.insert(&option.label))
        {
            return false;
        }
        if question
            .recommended
            .is_some_and(|index| index >= question.options.len())
        {
            return false;
        }
    }
    true
}

fn sanitize_prompt(report: &mut AgentIntegrationSnapshot) {
    if report.reply_keys.is_empty()
        || report
            .prompt
            .as_ref()
            .is_some_and(|prompt| !valid_prompt(prompt, report.status))
    {
        report.prompt = None;
    }
}

fn same_prompt(a: &AgentIntegrationSnapshot, b: &AgentIntegrationSnapshot) -> bool {
    a.instance_id == b.instance_id
        && a.provider_version == b.provider_version
        && a.provider_session_id == b.provider_session_id
        && a.writer_pid == b.writer_pid
        && a.revision == b.revision
        && a.prompt == b.prompt
        && a.reply_keys == b.reply_keys
}

fn validate_response(
    prompt: &AgentIntegrationPrompt,
    response: &AgentIntegrationResponse,
) -> Result<()> {
    match (prompt, response) {
        (
            AgentIntegrationPrompt::Approval { allow_always, .. },
            AgentIntegrationResponse::Approval { choice },
        ) => {
            ensure!(
                *choice != IntegrationApprovalChoice::AllowAlways || *allow_always,
                "prompt does not offer allow always"
            );
        }
        (
            AgentIntegrationPrompt::Question { questions, .. },
            AgentIntegrationResponse::Question { answers },
        ) => {
            ensure!(
                questions.len() == answers.len(),
                "question answer count changed"
            );
            for (q, answer) in questions.iter().zip(answers) {
                ensure!(q.id == answer.id, "question identity changed");
                let mut selected = HashSet::new();
                for label in &answer.selected_options {
                    ensure!(
                        selected.insert(label) && q.options.iter().any(|o| o.label == *label),
                        "invalid question selection"
                    );
                }
                let custom = answer
                    .custom_input
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !q.multi {
                    ensure!(
                        answer.selected_options.len() + usize::from(custom) == 1,
                        "single-choice question requires one answer"
                    );
                }
            }
        }
        _ => bail!("prompt type changed"),
    }
    Ok(())
}

/// Validate the current provider identity, publish its reply atomically, then send only declared keys.
pub fn respond(
    instance: &Instance,
    expected: &AgentIntegrationSnapshot,
    response: AgentIntegrationResponse,
) -> Result<()> {
    ensure!(
        eligible(instance),
        "session does not support provider integration replies"
    );
    let registry = super::registry();
    let plugin = provider(&registry, &instance.tool)?
        .context("provider integration is not enabled and approved")?;
    let current = read_report(&instance.id, &instance.tool, plugin)?
        .context("provider integration report is unavailable")?;
    ensure!(
        same_prompt(expected, &current),
        "prompt changed; reopen the reply dialog"
    );
    let prompt = current
        .prompt
        .as_ref()
        .context("provider no longer has a pending prompt")?;
    validate_response(prompt, &response)?;
    let tmux = crate::tmux::Session::new(&instance.id, &instance.title)?;
    ensure!(
        tmux.exists() && !tmux.is_pane_dead(),
        "target session is not live"
    );
    let reply = AgentIntegrationReply {
        schema_version: 1,
        instance_id: &current.instance_id,
        provider_session_id: &current.provider_session_id,
        writer_pid: current.writer_pid,
        revision: current.revision,
        prompt_id: prompt.id(),
        response_id: uuid::Uuid::new_v4().to_string(),
        response,
    };
    let bytes = serde_json::to_vec(&reply)?;
    ensure!(
        bytes.len() <= MAX_REPORT_BYTES,
        "reply exceeds integration size limit"
    );
    let dir = dir_guard::open_instance_dir(&instance.id)?;
    dir_guard::write_atomic(
        dir.as_fd(),
        &format!("integration-{}.reply.json", plugin.id()),
        &bytes,
    )?;
    let tokens: Vec<_> = current
        .reply_keys
        .iter()
        .map(|name| crate::agents::KeyToken::Named(name))
        .collect();
    tmux.send_key_tokens(&tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    #[test]
    fn catalog_suppresses_duplicate_provider_and_custom_or_builtin_collisions() {
        let entries = unique_candidates(
            [
                ("plugin-a", "duplicate", "a"),
                ("plugin-b", "duplicate", "b"),
                ("plugin-c", "builtin-agent", "c"),
                ("plugin-d", "custom-agent", "d"),
                ("plugin-e", "unique-helper", "e"),
            ],
            ["builtin-agent"],
            ["custom-agent"],
        );
        assert_eq!(
            entries,
            [PluginAgent {
                plugin: "plugin-e",
                tool: "unique-helper",
                command: "e",
            }]
        );
    }

    #[test]
    fn builtin_substring_aliases_are_reserved_from_plugins() {
        for tool in ["claude-yolo", "session-agent"] {
            assert!(
                unique_candidates([("plugin", tool, "agent")], [], []).is_empty(),
                "built-in alias should reserve {tool}"
            );
        }
    }

    #[test]
    fn same_prompt_rejects_changed_prompt_content_and_reply_keys() {
        let expected = snapshot();

        let mut changed = expected.clone();
        if let Some(AgentIntegrationPrompt::Approval { title, .. }) = &mut changed.prompt {
            *title = "Different action".into();
        }
        assert!(!same_prompt(&expected, &changed));

        let mut changed = expected.clone();
        changed.reply_keys = vec!["F1".into()];
        assert!(!same_prompt(&expected, &changed));

        let mut expected_question = expected.clone();
        expected_question.prompt = Some(AgentIntegrationPrompt::Question {
            id: "ask".into(),
            title: "Pick a database".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "db".into(),
                question: "Which database?".into(),
                options: vec![AgentIntegrationOption {
                    label: "SQLite".into(),
                    description: Some("Embedded".into()),
                }],
                multi: false,
                recommended: None,
            }],
        });
        let mut changed = expected_question.clone();
        let Some(AgentIntegrationPrompt::Question { questions, .. }) = &mut changed.prompt else {
            unreachable!();
        };
        questions[0].question = "Which database version?".into();
        assert!(!same_prompt(&expected_question, &changed));
    }

    #[test]
    fn check_file_rejects_insecure_mode_and_oversized_reports() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.json");

        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_file(&std::fs::metadata(&path).unwrap())
            .unwrap_err()
            .to_string()
            .contains("mode 0600"));

        std::fs::write(&path, vec![b'x'; MAX_REPORT_BYTES + 1]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_file(&std::fs::metadata(&path).unwrap())
            .unwrap_err()
            .to_string()
            .contains("size limit"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_file_rejects_a_report_owned_by_another_user_when_root() {
        use crate::hooks::test_support::{make_alien_owned, privdrop_test_enabled};
        use nix::unistd::chown;
        use std::os::unix::fs::PermissionsExt;

        if !privdrop_test_enabled() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        make_alien_owned(&path);

        let rejected = check_file(&std::fs::metadata(&path).unwrap());
        chown(
            &path,
            Some(nix::unistd::geteuid()),
            Some(nix::unistd::getegid()),
        )
        .unwrap();
        assert!(rejected.unwrap_err().to_string().contains("another owner"));
    }

    fn install_test_integration(plugin_id: &str, tool: &str) {
        use crate::session::{CapabilityGrant, PluginConfig};

        let manifest = format!(
            r#"
id = "{plugin_id}"
name = "Integration test"
version = "1.0.0"
api_version = 19
capabilities = ["session.integration"]

[[agent_integrations]]
tool = "{tool}"
provider_versions = ["^18"]
replies = true
"#
        );
        let dir = super::super::plugins_dir().unwrap().join(plugin_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), &manifest).unwrap();
        crate::session::update_config(|config| {
            config.plugins.insert(
                plugin_id.to_string(),
                PluginConfig {
                    grant: Some(CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities: vec!["session.integration".into()],
                        granted_at: chrono::Utc::now(),
                    }),
                    ..PluginConfig::default()
                },
            );
        })
        .unwrap();
    }

    fn install_declared_integration(
        plugin_id: &str,
        tool: &str,
        status: bool,
        replies: bool,
        extra: &str,
    ) {
        use crate::session::{CapabilityGrant, PluginConfig};

        let capability = if status {
            "capabilities = [\"session.integration\"]\n"
        } else {
            ""
        };
        let versions = if status {
            "provider_versions = [\"^1\"]\n"
        } else {
            ""
        };
        let manifest = format!(
            r#"
id = "{plugin_id}"
name = "Declared agent test"
version = "1.0.0"
api_version = 20
{capability}
[[agent_integrations]]
tool = "{tool}"
{versions}command = "{tool} --serve"
replies = {replies}
{extra}
"#
        );
        let dir = super::super::plugins_dir().unwrap().join(plugin_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), &manifest).unwrap();
        let capabilities = if status {
            vec!["session.integration".to_string()]
        } else {
            Vec::new()
        };
        crate::session::update_config(|config| {
            config.plugins.insert(
                plugin_id.to_string(),
                PluginConfig {
                    grant: Some(CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities,
                        granted_at: chrono::Utc::now(),
                    }),
                    ..PluginConfig::default()
                },
            );
        })
        .unwrap();
        super::super::reload_registry();
    }

    #[test]
    #[serial_test::serial]
    fn declared_launch_env_capture_and_detection_reach_the_host_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_declared_integration(
            "acme.declared",
            "acme-helper",
            true,
            false,
            r#"
[agent_integrations.launch]
oneshot_flag = "-p"
model_flag = "--model"
instruction_flag = "--append-system-prompt {}"
[agent_integrations.launch.container_env]
PI_CODING_AGENT_DIR = "/root/.omp/agent"
[agent_integrations.launch.resume]
kind = "flag"
flag = "--resume"
[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "pane_scoped"
[agent_integrations.detection]
mode = "native_only"
"#,
        );

        let agent = crate::agents::get_agent("acme-helper").expect("plugin agent resolves");
        assert!(status_owned("acme-helper"));
        assert_eq!(
            agent.container_env,
            &[("PI_CODING_AGENT_DIR", "/root/.omp/agent")]
        );
        assert_eq!(agent.oneshot_flag, Some("-p"));
        assert_eq!(agent.oneshot_model_flag(), Some("--model"));
        assert_eq!(
            agent
                .session_support
                .and_then(|support| support.capture)
                .map(|capture| capture.backend),
            Some(crate::agents::SessionCaptureBackend::Omp)
        );
        assert_eq!(
            crate::tmux::detect_status_from_content_in("", "esc to interrupt", "acme-helper"),
            crate::session::Status::Unknown
        );

        let resolved = crate::session::config::container_config::resolve_active_agent(
            "acme-helper",
            Some("acme-helper --serve"),
            &crate::session::config::SessionConfig::default(),
        )
        .expect("the sandbox launcher resolves the declared env");
        assert_eq!(resolved.container_env, agent.container_env);
    }

    #[test]
    #[serial_test::serial]
    fn built_in_agent_definitions_win_over_plugin_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_declared_integration(
            "acme.shadow",
            "claude",
            true,
            false,
            r#"
[agent_integrations.launch.container_env]
SHADOW_ONLY = "1"
"#,
        );

        let claude = crate::agents::get_agent("claude").expect("built-in resolves");
        assert_eq!(
            claude.container_env,
            &[("CLAUDE_CONFIG_DIR", "/root/.claude")]
        );
        assert!(!claude
            .container_env
            .iter()
            .any(|(key, _)| *key == "SHADOW_ONLY"));
    }

    #[test]
    #[serial_test::serial]
    fn reply_fallback_resolves_when_the_integration_does_not_own_replies() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_declared_integration(
            "acme.reply",
            "acme-reply",
            false,
            false,
            r#"
[agent_integrations.reply_fallback]
allow = [{ kind = "named", value = "Enter" }]
allow_always = [{ kind = "named", value = "Down" }, { kind = "named", value = "Enter" }]
deny = [{ kind = "literal", value = "n" }]
"#,
        );

        let agent = crate::agents::get_agent("acme-reply").expect("fallback agent resolves");
        let response = agent.permission_response.expect("fallback keys resolve");
        assert_eq!(response.allow, &[KeyToken::Named("Enter")]);
        assert_eq!(
            response.allow_always,
            Some(&[KeyToken::Named("Down"), KeyToken::Named("Enter")][..])
        );
        assert_eq!(response.deny, &[KeyToken::Literal("n")]);
    }

    #[test]
    fn declaration_summary_names_every_launch_declaration() {
        let integration = AgentIntegrationContribution {
            tool: "acme-helper".into(),
            command: Some("acme-helper --serve".into()),
            launch: Some(AgentLaunchDeclaration {
                container_env: [(
                    "PI_CODING_AGENT_DIR".to_string(),
                    "/root/.omp/agent".to_string(),
                )]
                .into_iter()
                .collect(),
                oneshot_flag: Some("-p".into()),
                ..Default::default()
            }),
            detection: Some(AgentDetectionDeclaration {
                mode: aoe_plugin_api::agent_integration::AgentDetectionMode::NativeOnly,
            }),
            reply_fallback: Some(AgentReplyDeclaration {
                allow: vec![AgentKeyDeclaration::Named {
                    value: "Enter".into(),
                }],
                allow_always: None,
                deny: vec![AgentKeyDeclaration::Literal { value: "n".into() }],
            }),
            ..Default::default()
        };
        let summary = declaration_summary(&integration);
        assert!(summary.iter().any(|line| line.contains("acme-helper:")));
        assert!(summary.iter().any(|line| line.contains("-p")));
        assert!(
            summary
                .iter()
                .any(|line| line.contains("PI_CODING_AGENT_DIR=/root/.omp/agent")),
            "{summary:?}"
        );
        assert!(summary.iter().any(|line| line.contains("native_only")));
        assert!(summary.iter().any(|line| line.contains("reply fallback")));
    }

    #[test]
    #[serial_test::serial]
    fn provider_rejects_duplicate_active_claims() {
        let temp = tempfile::tempdir().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_test_integration("acme.integration-one", "shared-agent");
        install_test_integration("acme.integration-two", "shared-agent");

        let registry = super::super::PluginRegistry::load(&crate::session::Config::load_or_warn());
        let error = match provider(&registry, "shared-agent") {
            Err(error) => error,
            Ok(_) => panic!("duplicate integration claims must be rejected"),
        };
        assert!(error.to_string().contains("multiple approved integrations"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn respond_writes_private_reply_and_delivers_declared_wake_key() {
        if !crate::tmux::test_helpers::tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let (_base, _base_path, _hook_tmp) = crate::hooks::test_support::BaseGuard::ready();
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_test_integration("acme.integration-reply", "reply-agent");
        let registry = super::super::reload_registry();

        let mut instance = Instance::new("integration-reply-test", "/tmp");
        instance.tool = "reply-agent".into();
        instance.status = crate::session::Status::Running;
        let tmux = crate::tmux::Session::new(&instance.id, &instance.title).unwrap();
        tmux.create("/tmp", Some("od -An -tx1"), "default").unwrap();
        struct KillSession(crate::tmux::Session);
        impl Drop for KillSession {
            fn drop(&mut self) {
                let _ = self.0.kill();
            }
        }
        let _kill = KillSession(crate::tmux::Session::from_name(tmux.name()));

        let mut current = snapshot();
        current.instance_id = instance.id.clone();
        current.provider_version = "18.11.1".into();
        current.reply_keys = vec!["F12".into()];
        let dir = dir_guard::open_instance_dir(&instance.id).unwrap();
        let report_name = "integration-acme.integration-reply.json";
        dir_guard::write_atomic(
            dir.as_fd(),
            report_name,
            &serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();

        let response = AgentIntegrationResponse::Approval {
            choice: IntegrationApprovalChoice::Allow,
        };
        respond(&instance, &current, response).unwrap();

        let reply_path = _hook_tmp
            .path()
            .join("aoe-hooks")
            .join(&instance.id)
            .join("integration-acme.integration-reply.reply.json");
        let reply_meta = std::fs::metadata(&reply_path).unwrap();
        assert_eq!(reply_meta.mode() & 0o7777, 0o600);
        let reply: serde_json::Value =
            serde_json::from_slice(&std::fs::read(reply_path).unwrap()).unwrap();
        assert_eq!(reply["prompt_id"], "approval");
        assert_eq!(reply["instance_id"], instance.id);
        assert_eq!(reply["response"]["choice"], "allow");

        let mut pane = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            pane = tmux.capture_pane(20).unwrap();
            if pane.contains("^[[24~") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            pane.contains("^[[24~"),
            "provider-declared F12 must be delivered to the pane, got {pane:?}"
        );
        drop(registry);
    }

    use aoe_plugin_api::agent_integration::{
        AgentIntegrationAnswer, AgentIntegrationOption, AgentIntegrationQuestion,
    };

    #[test]
    fn validates_prompt_identity_and_answer_payloads() {
        let prompt = AgentIntegrationPrompt::Question {
            id: "ask".into(),
            title: "Pick".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "db".into(),
                question: "Database?".into(),
                multi: false,
                recommended: None,
                options: vec![AgentIntegrationOption {
                    label: "SQLite".into(),
                    description: None,
                }],
            }],
        };
        let answer = |id: &str, label: &str| AgentIntegrationResponse::Question {
            answers: vec![AgentIntegrationAnswer {
                id: id.into(),
                selected_options: vec![label.into()],
                custom_input: None,
            }],
        };
        assert!(validate_response(&prompt, &answer("db", "SQLite")).is_ok());
        assert!(validate_response(&prompt, &answer("wrong", "SQLite")).is_err());
        assert!(validate_response(&prompt, &answer("db", "unknown")).is_err());
    }
    fn snapshot() -> AgentIntegrationSnapshot {
        AgentIntegrationSnapshot {
            schema_version: 1,
            instance_id: "host-test".into(),
            provider_version: "18.11.1".into(),
            provider_session_id: "provider-session".into(),
            writer_pid: std::process::id(),
            revision: 1,
            status: AgentIntegrationStatus::Waiting,
            reply_keys: vec!["F12".into()],
            prompt: Some(AgentIntegrationPrompt::Approval {
                id: "approval".into(),
                title: "Run command?".into(),
                allow_always: false,
            }),
        }
    }

    #[test]
    fn display_command_escapes_controls_and_bidi() {
        assert_eq!(
            display_command("session-helper --flag"),
            "session-helper --flag"
        );
        assert_eq!(display_command("x\u{1b}[2Jtail"), "x\\u{1b}[2Jtail");
        // Bidi controls are escaped too, not just Cc controls.
        assert_eq!(display_command("x\u{202e}tail"), "x\\u{202e}tail");
        // Any Unicode Format character, including tag characters, is escaped.
        assert_eq!(display_command("x\u{E0001}tail"), "x\\u{e0001}tail");
    }

    #[test]
    fn report_validation_rejects_wrong_identity_and_unsafe_reply_keys() {
        let mut valid = snapshot();
        assert!(validate_report(&valid, "host-test").is_ok());
        let mut status_only = valid.clone();
        status_only.prompt = None;
        status_only.reply_keys.clear();
        assert!(validate_report(&status_only, "host-test").is_ok());
        let mut valid_f1 = valid.clone();
        valid_f1.reply_keys = vec!["F1".into()];
        assert!(validate_report(&valid_f1, "host-test").is_ok());
        assert!(validate_report(&valid, "other").is_err());
        for key in ["F0", "F13", "F01", "Enter", "F12; echo unsafe"] {
            let mut changed = valid.clone();
            changed.reply_keys = vec![key.into()];
            assert!(validate_report(&changed, "host-test").is_err(), "{key}");
        }
        let mut duplicate = valid.clone();
        duplicate.reply_keys = vec!["F12".into(), "F12".into()];
        assert!(validate_report(&duplicate, "host-test").is_err());
        valid.status = AgentIntegrationStatus::Running;
        assert!(validate_report(&valid, "host-test").is_ok());
        sanitize_prompt(&mut valid);
        assert_eq!(valid.status, AgentIntegrationStatus::Running);
        assert!(valid.prompt.is_none());
    }

    #[test]
    fn malformed_prompt_is_dropped_without_discarding_status() {
        let mut report = snapshot();
        report.prompt = Some(AgentIntegrationPrompt::Question {
            id: "ask".into(),
            title: "Pick".into(),
            questions: vec![
                AgentIntegrationQuestion {
                    id: "db".into(),
                    question: "Database?".into(),
                    multi: false,
                    recommended: Some(0),
                    options: vec![AgentIntegrationOption {
                        label: "SQLite".into(),
                        description: None,
                    }],
                },
                AgentIntegrationQuestion {
                    id: "db".into(),
                    question: "Duplicate id".into(),
                    multi: false,
                    recommended: None,
                    options: Vec::new(),
                },
            ],
        });
        assert!(validate_report(&report, "host-test").is_ok());
        sanitize_prompt(&mut report);
        assert_eq!(report.status, AgentIntegrationStatus::Waiting);
        assert!(report.prompt.is_none());
    }

    #[test]
    fn allow_always_must_be_offered_by_the_provider() {
        let prompt = snapshot().prompt.unwrap();
        assert!(validate_response(
            &prompt,
            &AgentIntegrationResponse::Approval {
                choice: IntegrationApprovalChoice::AllowAlways,
            },
        )
        .is_err());
    }

    #[test]
    fn provider_status_maps_to_terminal_status_and_fails_closed_without_report() {
        for (provider, expected) in [
            (
                AgentIntegrationStatus::Running,
                crate::session::Status::Running,
            ),
            (
                AgentIntegrationStatus::Waiting,
                crate::session::Status::Waiting,
            ),
            (AgentIntegrationStatus::Idle, crate::session::Status::Idle),
            (AgentIntegrationStatus::Error, crate::session::Status::Error),
        ] {
            assert_eq!(session_status(provider), expected);
            assert_eq!(
                session_status_override(true, false, Some(provider)),
                Some(expected)
            );
        }
        assert_eq!(session_status_override(false, false, None), None);
        assert_eq!(
            session_status_override(true, false, None),
            Some(crate::session::Status::Unknown),
            "an approved integration must not fall back to pane detection"
        );
        assert_eq!(
            session_status_override(true, true, Some(AgentIntegrationStatus::Running)),
            Some(crate::session::Status::Error)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn unavailable_incompatible_and_expired_reports_fail_closed() {
        let (_guard, base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let manifest = aoe_plugin_api::PluginManifest::from_toml_str(
            r#"
            id = "acme.agent-integration"
            name = "Test Agent Integration"
            version = "0.1.0"
            api_version = 19
            capabilities = ["session.integration"]

            [[agent_integrations]]
            tool = "test-agent"
            provider_versions = ["^1.2"]
            "#,
        )
        .unwrap();
        let mut plugin = super::super::LoadedPlugin {
            manifest,
            enabled: true,
            trust: aoe_plugin_api::TrustLevel::Community,
            validation: super::super::registry::ValidationState::Local,
            source: None,
            dir: None,
            manifest_hash: String::new(),
            granted: true,
        };
        assert!(read_report("host-test", "test-agent", &plugin)
            .unwrap()
            .is_none());
        let dir = dir_guard::open_instance_dir("host-test").unwrap();
        let leaf = format!("integration-{}.json", plugin.id());
        for version in ["1.1.9", "1.2.0", "1.9.0", "2.0.0", "1.2.3-rc.1", "unknown"] {
            let mut report = snapshot();
            report.provider_version = version.into();
            dir_guard::write_atomic(dir.as_fd(), &leaf, &serde_json::to_vec(&report).unwrap())
                .unwrap();
            let loaded = read_report("host-test", "test-agent", &plugin);
            if !matches!(version, "1.2.0" | "1.9.0") {
                assert!(loaded.is_err(), "{version}");
            } else {
                assert!(loaded.unwrap().is_some(), "{version}");
            }
        }
        let mut fresh = snapshot();
        fresh.provider_version = "1.2.3".into();
        for (replies, keys) in [
            (false, vec!["F12".to_string()]),
            (false, Vec::new()),
            (true, Vec::new()),
            (true, vec!["F12".to_string()]),
        ] {
            plugin.manifest.agent_integrations[0].replies = replies;
            fresh.reply_keys = keys;
            let has_keys = !fresh.reply_keys.is_empty();
            let mut wire = serde_json::to_value(&fresh).unwrap();
            if !has_keys {
                wire.as_object_mut().unwrap().remove("reply_keys");
            }
            dir_guard::write_atomic(dir.as_fd(), &leaf, &serde_json::to_vec(&wire).unwrap())
                .unwrap();
            let loaded = read_report("host-test", "test-agent", &plugin)
                .unwrap()
                .unwrap();
            assert_eq!(loaded.status, AgentIntegrationStatus::Waiting);
            assert_eq!(loaded.prompt.is_some(), replies && has_keys);
        }
        dir_guard::write_atomic(dir.as_fd(), &leaf, &serde_json::to_vec(&fresh).unwrap()).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(base.join("host-test").join(&leaf))
            .unwrap();
        file.set_modified(SystemTime::now() - REPORT_MAX_AGE - Duration::from_secs(60))
            .unwrap();
        assert!(read_report("host-test", "test-agent", &plugin)
            .unwrap_err()
            .to_string()
            .contains("expired"));
        assert_eq!(
            session_status_override(true, false, None),
            Some(crate::session::Status::Unknown)
        );
        assert!(!base
            .join("host-test")
            .join(format!("integration-{}.reply.json", plugin.id()))
            .exists());
        let mut changed = snapshot();
        changed.provider_version = "1.3.0".into();
        assert!(!same_prompt(&snapshot(), &changed));
    }
}
