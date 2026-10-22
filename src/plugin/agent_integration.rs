//! Secure provider-native session reports and identity-checked tmux replies.

use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd,
    os::unix::fs::MetadataExt,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
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
        if let Some(builtin) = crate::agents::resolve_tool_name(tool) {
            tracing::warn!(
                plugin = %plugin,
                tool = %tool,
                builtin = %builtin,
                "plugin agent tool name contains a built-in name or alias; preserving built-in precedence"
            );
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
        status.map(session_status)
    }
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
    fn provider_status_maps_to_terminal_status_and_uses_native_fallback_without_report() {
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
        assert_eq!(session_status_override(true, false, None), None);
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
        assert_eq!(session_status_override(true, false, None), None);
        assert!(!base
            .join("host-test")
            .join(format!("integration-{}.reply.json", plugin.id()))
            .exists());
        let mut changed = snapshot();
        changed.provider_version = "1.3.0".into();
        assert!(!same_prompt(&snapshot(), &changed));
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
}
