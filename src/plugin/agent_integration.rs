//! Secure provider-native session reports and identity-checked tmux replies.

use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd,
    os::unix::fs::MetadataExt,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use aoe_plugin_api::agent_integration::{
    AgentIntegrationContribution, AgentKeyDeclaration, AgentResumeDeclaration, AgentYoloDeclaration,
};
use aoe_plugin_api::agent_integration::{
    AgentIntegrationPrompt, AgentIntegrationReply, AgentIntegrationResponse,
    AgentIntegrationSnapshot, AgentIntegrationStatus, IntegrationApprovalChoice,
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

fn warn_reserved(plugin: &str, tool: &str, builtin: &str) {
    let mut warned = RESERVED_WARNED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if warned.insert((plugin.to_owned(), tool.to_owned())) {
        tracing::warn!(target: "plugin.agents", %plugin, %tool, %builtin,
            "plugin agent tool name contains a built-in name or alias; preserving built-in precedence");
    }
}

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
    for (plugin, tool, _) in &candidates {
        if let Some(builtin) = crate::agents::resolve_builtin_tool_name(tool) {
            warn_reserved(plugin, tool, builtin);
            candidate_counts.remove(tool);
        }
    }
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
            .filter(move |entry| {
                !super::bundled_agents::definitions().any(|fallback| {
                    entry.tool.contains(fallback.name)
                        && (entry.tool != fallback.name
                            || plugin.manifest.api_version < 21
                            || entry.launch.is_none())
                })
            })
            .filter_map(move |entry| {
                entry
                    .command
                    .as_deref()
                    .map(|command| (plugin.id(), entry.tool.as_str(), command))
            })
    });
    unique_candidates(candidates, builtin_names, custom_names)
}

pub(crate) fn resolve_declared_tool<'a>(
    registry: &super::PluginRegistry,
    command: &str,
    custom_names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let words = shell_words::split(command).ok()?;
    let prefix = aoe_plugin_api::agent_integration::launch_prefix(command);
    let executable = prefix
        .as_ref()
        .and_then(|(words, _)| words.last().copied())
        .or_else(|| words.first().map(String::as_str))?;
    let basename = std::path::Path::new(executable).file_name()?.to_str()?;
    let mut best: Option<(&str, usize)> = None;
    let mut ambiguous = false;
    for candidate in catalog(
        registry,
        crate::agents::AGENTS.iter().map(|agent| agent.name),
        custom_names,
    ) {
        let plugin = registry.get(candidate.plugin)?;
        let entry = plugin
            .manifest
            .agent_integrations
            .iter()
            .find(|entry| entry.tool == candidate.tool)?;
        let declared_executable =
            aoe_plugin_api::agent_integration::launch_prefix(candidate.command)
                .and_then(|(prefix, _)| prefix.last().copied())
                .and_then(|binary| std::path::Path::new(binary).file_name())
                .and_then(|name| name.to_str());
        let aliases = entry
            .launch
            .iter()
            .flat_map(|launch| launch.aliases.iter().map(String::as_str));
        for token in std::iter::once(candidate.tool)
            .chain(declared_executable)
            .chain(aliases)
        {
            if !basename.eq_ignore_ascii_case(token) {
                continue;
            }
            match best {
                None => {
                    best = Some((candidate.tool, token.len()));
                    ambiguous = false;
                }
                Some((_, length)) if token.len() > length => {
                    best = Some((candidate.tool, token.len()));
                    ambiguous = false;
                }
                Some((name, length)) if token.len() == length && name != candidate.tool => {
                    ambiguous = true
                }
                _ => {}
            }
        }
    }
    if ambiguous {
        None
    } else {
        best.map(|(tool, _)| tool.to_owned())
    }
}

/// An agent identity bound to one immutable registry snapshot.
#[derive(Clone)]
pub(crate) struct DeclaredAgent {
    registry: std::sync::Arc<super::PluginRegistry>,
    plugin_index: usize,
    entry_index: usize,
}

impl DeclaredAgent {
    pub(crate) fn contribution(
        &self,
    ) -> &aoe_plugin_api::agent_integration::AgentIntegrationContribution {
        &self.registry.all()[self.plugin_index]
            .manifest
            .agent_integrations[self.entry_index]
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.registry, &other.registry)
            && self.plugin_index == other.plugin_index
            && self.entry_index == other.entry_index
    }

    pub(crate) fn while_current<R>(&self, operation: impl FnOnce() -> R) -> Option<R> {
        let registry = super::REGISTRY
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let current = registry.as_ref()?;
        if !std::sync::Arc::ptr_eq(&self.registry, current)
            || !self.registry.all()[self.plugin_index].active()
        {
            return None;
        }
        Some(operation())
    }

    pub(crate) fn with_definition<R>(
        &self,
        use_agent: impl FnOnce(&crate::agents::AgentDef<'_>) -> R,
    ) -> Option<R> {
        let entry = self.contribution();
        let data = super::agent_definition::DefinitionData::new(entry);
        let status = self.registry.all()[self.plugin_index]
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "session.integration");
        Some(use_agent(&data.definition(entry, status)?))
    }

    fn plugin_id(&self) -> &str {
        self.registry.all()[self.plugin_index].id()
    }
}

pub(crate) fn declared_agent<'a>(
    registry: std::sync::Arc<super::PluginRegistry>,
    tool: &str,
    custom_names: impl IntoIterator<Item = &'a str>,
) -> Option<DeclaredAgent> {
    if custom_names.into_iter().any(|name| name == tool) {
        return None;
    }
    if super::bundled_agents::definitions()
        .any(|fallback| tool != fallback.name && tool.contains(fallback.name))
    {
        return None;
    }
    if let Some(builtin) = crate::agents::resolve_builtin_tool_name(tool) {
        for plugin in registry.active().filter(|plugin| {
            plugin
                .manifest
                .agent_integrations
                .iter()
                .any(|entry| entry.tool == tool && entry.command.is_some())
        }) {
            warn_reserved(plugin.id(), tool, builtin);
        }
        return None;
    }
    let mut selected = None;
    for (plugin_index, plugin) in registry.all().iter().enumerate() {
        if !plugin.active() {
            continue;
        }
        for (entry_index, entry) in plugin.manifest.agent_integrations.iter().enumerate() {
            if super::bundled_agents::contains(tool)
                && (plugin.manifest.api_version < 21 || entry.launch.is_none())
            {
                continue;
            }
            if entry.tool == tool && entry.command.is_some() {
                if selected.is_some() {
                    return None;
                }
                selected = Some((plugin_index, entry_index));
            }
        }
    }
    let (plugin_index, entry_index) = selected?;
    Some(DeclaredAgent {
        registry,
        plugin_index,
        entry_index,
    })
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

/// Display every declaration without interpreting its contents as terminal control sequences.
pub fn consent_lines(integration: &AgentIntegrationContribution) -> Vec<String> {
    let mut lines = vec![format!(
        "{}: {}",
        display_command(&integration.tool),
        integration
            .command
            .as_deref()
            .map(display_command)
            .unwrap_or_else(|| "No launch command declared".to_string())
    )];
    if let Some(launch) = &integration.launch {
        let mut flags = Vec::new();
        for (name, flag) in [
            ("oneshot", &launch.oneshot_flag),
            ("instruction", &launch.instruction_flag),
            ("model", &launch.model_flag),
        ] {
            if let Some(flag) = flag {
                flags.push(format!("{name} {}", display_command(flag)));
            }
        }
        if let Some(yolo) = &launch.yolo {
            flags.push(match yolo {
                AgentYoloDeclaration::Flag { flag } => format!("yolo {}", display_command(flag)),
                AgentYoloDeclaration::Always => "yolo always".to_string(),
                AgentYoloDeclaration::Env { key, value } => {
                    format!("yolo {}={}", display_command(key), display_command(value))
                }
            });
        }
        if !flags.is_empty() {
            lines.push(format!("  launch flags: {}", flags.join("; ")));
        }
        if !launch.aliases.is_empty() {
            lines.push(format!(
                "  aliases: {}",
                launch
                    .aliases
                    .iter()
                    .map(|value| display_command(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !launch.container_env.is_empty() {
            lines.push(format!(
                "  sandbox environment: {}",
                launch
                    .container_env
                    .iter()
                    .map(|(key, value)| format!(
                        "{}={}",
                        display_command(key),
                        display_command(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(resume) = &launch.resume {
            let resume = match resume {
                AgentResumeDeclaration::Flag { flag } => format!("flag {}", display_command(flag)),
                AgentResumeDeclaration::FlagPair {
                    existing,
                    new_session,
                } => format!(
                    "flags {} / {}",
                    display_command(existing),
                    display_command(new_session)
                ),
                AgentResumeDeclaration::Subcommand { command } => {
                    format!("subcommand {}", display_command(command))
                }
            };
            lines.push(format!("  resume: {resume}"));
        }
        if let Some(capture) = &launch.capture {
            lines.push(format!(
                "  capture: {:?} (host {:?}, sandbox {:?})",
                capture.backend, capture.host, capture.sandbox
            ));
        }
        lines.push(format!(
            "  host only: {}",
            if launch.host_only { "yes" } else { "no" }
        ));
        if let Some(hint) = &launch.install_hint {
            lines.push(format!("  install: {}", display_command(hint)));
        }
    }
    if let Some(detection) = &integration.detection {
        lines.push(format!("  detection: {:?}", detection.mode));
    }
    if !integration.provider_versions.is_empty() {
        lines.push(format!(
            "  provider versions: {}",
            integration
                .provider_versions
                .iter()
                .map(|value| display_command(value))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.push(format!(
        "  native replies: {}",
        if integration.replies { "yes" } else { "no" }
    ));
    if let Some(reply) = &integration.reply_fallback {
        let labels = |keys: &[AgentKeyDeclaration]| {
            keys.iter()
                .map(|key| match key {
                    AgentKeyDeclaration::Literal { value } => {
                        format!("literal {:?}", display_command(value))
                    }
                    AgentKeyDeclaration::Named { value } => {
                        format!("key {}", display_command(value))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let always = reply
            .allow_always
            .as_ref()
            .map_or_else(String::new, |keys| {
                format!(", allow always [{}]", labels(keys))
            });
        lines.push(format!(
            "  reply fallback: allow [{}], deny [{}]{always}",
            labels(&reply.allow),
            labels(&reply.deny)
        ));
    }
    lines
}
/// Resolve a unique plugin-provided agent launch command.
pub fn agent_command<'a>(
    tool: &str,
    custom_names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let agent = declared_agent(super::registry(), tool, custom_names)?;
    tracing::debug!(
        target: "plugin.agents",
        tool,
        plugin = agent.plugin_id(),
        "resolved plugin-provided agent command"
    );
    agent.contribution().command.clone()
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

/// Resolve ownership and the report against one registry snapshot.
pub(crate) fn owned_status(
    instance: &Instance,
    pane_dead: bool,
) -> Option<(crate::session::Status, bool)> {
    if !eligible(instance) {
        return None;
    }
    let registry = super::registry();
    let configured = registry.active().any(|plugin| {
        plugin
            .manifest
            .capabilities
            .iter()
            .any(|c| c.as_str() == "session.integration")
            && plugin
                .manifest
                .agent_integrations
                .iter()
                .any(|entry| entry.tool == instance.tool)
    });
    if !configured {
        return None;
    }
    let report = if pane_dead {
        None
    } else {
        provider(&registry, &instance.tool)
            .ok()
            .flatten()
            .and_then(|plugin| {
                read_report(&instance.id, &instance.tool, plugin)
                    .ok()
                    .flatten()
            })
    };
    session_status_override(true, pane_dead, report.as_ref().map(|report| report.status))
        .map(|status| (status, report.is_some()))
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
    fn borrowed_definition_preserves_generic_omp_contract() {
        let manifest: aoe_plugin_api::PluginManifest = toml::from_str(
            r#"
id = "acme.capture"
name = "Capture"
version = "1.0.0"
api_version = 21
[[agent_integrations]]
tool = "example-helper"
command = "TOKEN=local example-helper --flag"
[agent_integrations.launch]
oneshot_flag = "-p"
model_flag = "--model"
aliases = ["other-helper"]
[agent_integrations.launch.yolo]
kind = "flag"
flag = "--auto"
[agent_integrations.launch.resume]
kind = "flag"
flag = "--continue"
[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "unsupported"
[agent_integrations.reply_fallback]
allow = [{ kind = "named", value = "Enter" }]
deny = [{ kind = "literal", value = "n" }]
"#,
        )
        .unwrap();
        let registry = std::sync::Arc::new(super::super::PluginRegistry::from_test_plugins(vec![
            super::super::LoadedPlugin {
                manifest,
                enabled: true,
                trust: aoe_plugin_api::TrustLevel::Community,
                validation: super::super::registry::ValidationState::Local,
                source: None,
                dir: None,
                manifest_hash: String::new(),
                granted: true,
            },
        ]));
        let agent = declared_agent(registry.clone(), "example-helper", std::iter::empty()).unwrap();
        let mut instance = Instance::new("generic-capture", "/tmp");
        instance.tool = "example-helper".into();
        instance.command = "TOKEN=local example-helper --flag".into();
        agent
            .with_definition(|definition| {
                assert_eq!(definition.name, "example-helper");
                assert_eq!(definition.binary, "example-helper");
                assert!(instance.launch_invokes_resolved_agent_directly(definition));
                assert_eq!(definition.aliases, ["other-helper"]);
                assert_eq!(definition.oneshot_flag, Some("-p"));
                assert_eq!(definition.oneshot_model_flag(), Some("--model"));
                let capture = definition.session_support.unwrap().capture.unwrap();
                assert_eq!(capture.backend, crate::agents::SessionCaptureBackend::Omp);
                assert_eq!(
                    capture.host,
                    crate::agents::SessionCaptureContext::PaneScoped
                );
                assert_eq!(
                    capture.sandbox,
                    crate::agents::SessionCaptureContext::Unsupported
                );
                assert_eq!(
                    definition.permission_response.unwrap().deny,
                    &[crate::agents::KeyToken::Literal("n")]
                );
            })
            .unwrap();
        assert_eq!(
            resolve_declared_tool(&registry, "other-helper", std::iter::empty()).as_deref(),
            Some("example-helper")
        );
        assert_eq!(
            resolve_declared_tool(&registry, "unrelated other-helper", std::iter::empty()),
            None
        );
        let models = [("example-helper".to_string(), "model x".to_string())]
            .into_iter()
            .collect();
        let argv = crate::session::smart_rename::OneshotAgent::Declared(agent.clone())
            .argv("hello; safe", Some(&models))
            .unwrap();
        assert_eq!(&argv[..2], ["/bin/sh", "-c"]);
        assert_eq!(
            shell_words::split(&argv[2]).unwrap(),
            [
                "exec",
                "env",
                "TOKEN=local",
                "example-helper",
                "--flag",
                "-p",
                "--model",
                "model x",
                "hello; safe"
            ]
        );
    }
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
command = "{tool}"
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

    #[test]
    #[serial_test::serial]
    fn always_yolo_does_not_persist_after_declaration_update() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        let dir = super::super::plugins_dir().unwrap().join("acme.yolo");
        std::fs::create_dir_all(&dir).unwrap();
        let mut instance = Instance::new("yolo", temp.path().to_str().unwrap());
        instance.tool = "yolo-helper".into();
        instance.command = "yolo-helper".into();

        for (declaration, expected) in [
            ("kind = \"always\"", true),
            ("kind = \"flag\"\nflag = \"--yes\"", false),
        ] {
            let manifest = format!(
                "id = \"acme.yolo\"\nname = \"Yolo\"\nversion = \"1.0.0\"\napi_version = 21\n[[agent_integrations]]\ntool = \"yolo-helper\"\ncommand = \"yolo-helper\"\n[agent_integrations.launch.yolo]\n{declaration}\n"
            );
            std::fs::write(dir.join("aoe-plugin.toml"), &manifest).unwrap();
            crate::session::update_config(|config| {
                config.plugins.insert(
                    "acme.yolo".into(),
                    crate::session::PluginConfig {
                        grant: Some(crate::session::CapabilityGrant {
                            manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                                manifest.as_bytes(),
                            ),
                            capabilities: Vec::new(),
                            granted_at: chrono::Utc::now(),
                        }),
                        ..Default::default()
                    },
                );
            })
            .unwrap();
            super::super::reload_registry();
            assert_eq!(instance.is_yolo_mode(), expected);
            assert!(!instance.yolo_mode);
        }
    }

    #[test]
    #[serial_test::serial]
    fn declaration_identity_is_bound_to_its_registry_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        install_test_integration("acme.identity", "identity-helper");
        let registry = super::super::reload_registry();
        let original = declared_agent(registry, "identity-helper", std::iter::empty()).unwrap();
        original
            .with_definition(|agent| {
                assert_eq!(agent.name, "identity-helper");
                assert_eq!(agent.binary, "identity-helper");
                assert_eq!((agent.detect_status)(""), crate::session::Status::Unknown);
            })
            .unwrap();
        crate::session::update_config(|config| {
            config.plugins.get_mut("acme.identity").unwrap().enabled = false;
        })
        .unwrap();
        let disabled = super::super::reload_registry();
        assert!(declared_agent(disabled, "identity-helper", std::iter::empty()).is_none());
        assert_eq!(
            original.contribution().command.as_deref(),
            Some("identity-helper")
        );
        assert!(original.while_current(|| ()).is_none());
        assert!(agent_command("identity-helper", std::iter::empty()).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn omp_fallback_remains_available_without_an_active_status_provider() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        let mut instance = Instance::new("omp-fallback", temp.path().to_str().unwrap());
        instance.tool = "omp".into();
        instance.status = crate::session::Status::Running;

        let assert_fallback = || {
            assert_eq!(instance.get_tool_command(), "omp");
            assert_eq!(owned_status(&instance, false), None);
            let agent = instance.status_agent().expect("built-in OMP");
            assert_eq!(
                (agent.detect_status)("plain command output"),
                crate::session::Status::Idle
            );
            assert_eq!(
                agent.session_support.unwrap().capture.unwrap().backend,
                crate::agents::SessionCaptureBackend::Omp
            );
        };

        super::super::reload_registry();
        assert_fallback();
        install_test_integration("acme.omp-owner", "omp");
        super::super::reload_registry();
        assert_eq!(instance.get_tool_command(), "omp");
        assert_eq!(
            owned_status(&instance, false),
            Some((crate::session::Status::Unknown, false))
        );
        crate::session::update_config(|config| {
            config.plugins.get_mut("acme.omp-owner").unwrap().enabled = false;
        })
        .unwrap();
        super::super::reload_registry();
        assert_fallback();
    }

    #[test]
    #[serial_test::serial]
    fn approved_omp_declaration_supersedes_bundled_contract_and_revocation_restores_it() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        let manifest = r#"
id = "acme.omp-owner"
name = "OMP integration"
version = "1.0.0"
api_version = 21
capabilities = ["session.integration"]
[[agent_integrations]]
tool = "omp"
command = "OMP_MODE=plugin omp-next --verbose"
provider_versions = ["^18"]
replies = true
[agent_integrations.launch]
oneshot_flag = "--print"
model_flag = "--model"
container_env = { PI_CODING_AGENT_DIR = "/root/plugin-omp" }
[agent_integrations.launch.yolo]
kind = "flag"
flag = "--plugin-auto"
[agent_integrations.launch.resume]
kind = "flag"
flag = "--continue"
[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "unsupported"
[agent_integrations.detection]
mode = "native_only"
"#;
        let dir = super::super::plugins_dir().unwrap().join("acme.omp-owner");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), manifest).unwrap();
        let mut instance = Instance::new("omp-declaration", temp.path().to_str().unwrap());
        instance.tool = "omp".into();
        instance.status = crate::session::Status::Running;
        assert!(
            instance.command.is_empty(),
            "legacy rows have no stored command"
        );
        let fallback = || {
            let agent = crate::agents::get_agent("omp").expect("bundled fallback");
            assert_eq!(agent.oneshot_flag, Some("-p"));
            assert_eq!(
                agent.container_env,
                &[("PI_CODING_AGENT_DIR", "/root/.omp/agent")]
            );
            assert_eq!(
                (agent.detect_status)("plain command output"),
                crate::session::Status::Idle
            );
            assert!(agent.session_support.unwrap().capture.is_some());
            assert_eq!(instance.get_tool_command(), "omp");
            assert!(instance.declared_agent().is_none());
        };
        super::super::reload_registry();
        fallback(); // Present but not approved.
        crate::session::update_config(|config| {
            config.plugins.insert(
                "acme.omp-owner".into(),
                crate::session::PluginConfig {
                    grant: Some(crate::session::CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities: vec!["session.integration".into()],
                        granted_at: chrono::Utc::now(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        super::super::reload_registry();
        assert!(crate::agents::get_agent("omp").is_none());
        assert_eq!(
            instance.get_tool_command(),
            "OMP_MODE=plugin omp-next --verbose"
        );
        assert_eq!(
            instance.launch_executable_token().as_deref(),
            Some("omp-next")
        );
        assert!(instance.supports_native_resume());
        let declared = instance.declared_agent().expect("approved declaration");
        declared.with_definition(|agent| {
            assert!(instance.launch_invokes_resolved_agent_directly(agent));
            assert_eq!(agent.oneshot_flag, Some("--print"));
            assert_eq!(agent.container_env, &[("PI_CODING_AGENT_DIR", "/root/plugin-omp")]);
            assert!(matches!(agent.yolo.as_ref(), Some(crate::agents::YoloMode::CliFlag(flag)) if *flag == "--plugin-auto"));
            assert!(matches!(agent.session_support.unwrap().resume, crate::agents::ResumeStrategy::Flag("--continue")));
            assert_eq!(agent.session_support.unwrap().capture.unwrap().sandbox, crate::agents::SessionCaptureContext::Unsupported);
        }).unwrap();
        assert!(replies_configured(&instance));
        assert_eq!(
            owned_status(&instance, false),
            Some((crate::session::Status::Unknown, false))
        );
        crate::session::update_config(|config| {
            config.plugins.get_mut("acme.omp-owner").unwrap().enabled = false;
        })
        .unwrap();
        super::super::reload_registry();
        assert!(declared.while_current(|| ()).is_none());
        fallback();
        crate::session::update_config(|config| {
            let plugin = config.plugins.get_mut("acme.omp-owner").unwrap();
            plugin.enabled = true;
            plugin.grant = None;
        })
        .unwrap();
        super::super::reload_registry();
        fallback(); // Revoked approval must not authorize the declaration.
    }

    #[test]
    #[serial_test::serial]
    fn api21_generic_capture_stops_when_plugin_is_disabled_without_removing_omp() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = super::super::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path()),
            ("XDG_CONFIG_HOME", temp.path()),
            ("USERPROFILE", temp.path()),
        ]);
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        let manifest = r#"
id = "acme.capture"
name = "Capture"
version = "1.0.0"
api_version = 21
[[agent_integrations]]
tool = "example-helper"
command = "TOKEN=local example-helper"
[agent_integrations.launch.resume]
kind = "flag"
flag = "--continue"
[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "unsupported"
"#;
        let dir = super::super::plugins_dir().unwrap().join("acme.capture");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), manifest).unwrap();
        crate::session::update_config(|config| {
            config.plugins.insert(
                "acme.capture".into(),
                crate::session::PluginConfig {
                    grant: Some(crate::session::CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities: Vec::new(),
                        granted_at: chrono::Utc::now(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        super::super::reload_registry();
        let mut instance = Instance::new("capture", temp.path().to_str().unwrap());
        instance.tool = "example-helper".into();
        instance.command = "TOKEN=local example-helper".into();
        assert!(instance.supports_native_resume());
        crate::session::update_config(|config| {
            config.plugins.get_mut("acme.capture").unwrap().enabled = false;
        })
        .unwrap();
        super::super::reload_registry();
        assert!(!instance.supports_native_resume());
        let omp = crate::agents::get_agent("omp").expect("built-in OMP remains available");
        assert_eq!(
            omp.session_support.unwrap().capture.unwrap().backend,
            crate::agents::SessionCaptureBackend::Omp
        );
        assert_eq!(
            (omp.detect_status)("plain command output"),
            crate::session::Status::Idle
        );
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
    fn consent_lines_escape_nested_values() {
        use aoe_plugin_api::agent_integration::{AgentLaunchDeclaration, AgentReplyDeclaration};
        let integration = AgentIntegrationContribution {
            tool: "helper".into(),
            command: Some("helper".into()),
            launch: Some(AgentLaunchDeclaration {
                install_hint: Some("install \u{202e}now".into()),
                container_env: [("TEST".into(), "x\u{1b}y".into())].into_iter().collect(),
                ..Default::default()
            }),
            reply_fallback: Some(AgentReplyDeclaration {
                allow: vec![AgentKeyDeclaration::Literal {
                    value: "\u{202e}".into(),
                }],
                allow_always: None,
                deny: vec![AgentKeyDeclaration::Named {
                    value: "Enter".into(),
                }],
            }),
            ..Default::default()
        };
        let lines = consent_lines(&integration).join("\n");
        assert!(!lines.contains('\u{202e}') && !lines.contains('\u{1b}'));
        assert!(lines.contains("\\u{202e}") && lines.contains("\\u{1b}"));
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
    fn provider_status_maps_to_terminal_status_and_preserves_ownership_without_report() {
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
            Some(crate::session::Status::Unknown)
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
