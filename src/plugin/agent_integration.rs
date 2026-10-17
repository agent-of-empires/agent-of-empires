//! Secure provider-native session reports and identity-checked tmux replies.

use std::{
    collections::HashSet,
    os::fd::AsFd,
    os::unix::fs::MetadataExt,
    time::{Duration, SystemTime},
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
            .any(|c| c.as_str() == "session.integration")
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
                    .any(|entry| entry.tool == instance.tool)
        })
}

/// Return a fresh verified provider snapshot when available.
pub fn snapshot(instance: &Instance) -> Option<AgentIntegrationSnapshot> {
    if !eligible(instance) {
        return None;
    }
    let registry = super::registry();
    let plugin = provider(&registry, &instance.tool).ok()??;
    read_report(&instance.id, plugin).ok().flatten()
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

fn check_file(meta: &std::fs::Metadata) -> Result<SystemTime> {
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
    Ok(modified)
}

fn read_report(
    instance_id: &str,
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
    ensure!(
        !r.reply_keys.is_empty() && r.reply_keys.len() <= 8,
        "invalid integration reply keys"
    );
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
    if report
        .prompt
        .as_ref()
        .is_some_and(|prompt| !valid_prompt(prompt, report.status))
    {
        report.prompt = None;
    }
}

fn same_prompt(a: &AgentIntegrationSnapshot, b: &AgentIntegrationSnapshot) -> bool {
    a.instance_id == b.instance_id
        && a.provider_session_id == b.provider_session_id
        && a.writer_pid == b.writer_pid
        && a.revision == b.revision
        && a.prompt.as_ref().map(AgentIntegrationPrompt::id)
            == b.prompt.as_ref().map(AgentIntegrationPrompt::id)
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
    let current =
        read_report(&instance.id, plugin)?.context("provider integration report is unavailable")?;
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
}
