//! Approved provider-native session reports and explicit user replies over tmux.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context, Result};
use aoe_plugin_api::agent_integration::{
    AgentIntegrationKey, AgentIntegrationPrompt, AgentIntegrationReply, AgentIntegrationResponse,
    AgentIntegrationSnapshot, AgentIntegrationStatus, IntegrationApprovalChoice,
    IntegrationControl,
};

use crate::hooks::dir_guard;
use crate::session::Instance;

use super::{LoadedPlugin, PluginRegistry};

const MAX_REPORT_BYTES: usize = 64 * 1024;
const REPORT_MAX_AGE: Duration = Duration::from_secs(15);

struct CachedReport {
    plugin_id: String,
    manifest_hash: String,
    modified: SystemTime,
    inode: u64,
    len: u64,
    report: Arc<AgentIntegrationSnapshot>,
}

static CACHE: LazyLock<Mutex<HashMap<String, CachedReport>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn provider<'a>(registry: &'a PluginRegistry, tool: &str) -> Result<Option<&'a LoadedPlugin>> {
    let mut candidates = registry.active().filter(|plugin| {
        plugin
            .manifest
            .capabilities
            .iter()
            .any(|c| c.as_str() == "session.integration")
            && plugin
                .manifest
                .agent_integrations
                .iter()
                .any(|i| i.tool == tool)
    });
    let selected = candidates.next();
    ensure!(
        candidates.next().is_none(),
        "multiple approved integrations claim {tool}"
    );
    Ok(selected)
}

fn supports_provider_integration(instance: &Instance) -> bool {
    !instance.is_structured() && !instance.is_sandboxed()
}

/// Whether an approved integration owns this terminal session's status/replies.
/// Missing or ambiguous reports must not silently fall through to built-in mappings.
pub fn configured(instance: &Instance) -> bool {
    if !supports_provider_integration(instance) {
        return false;
    }
    let registry = super::registry();
    !matches!(provider(&registry, &instance.tool), Ok(None))
}

pub fn snapshot(instance: &Instance) -> Option<Arc<AgentIntegrationSnapshot>> {
    if !supports_provider_integration(instance) {
        return None;
    }
    let registry = super::registry();
    let plugin = provider(&registry, &instance.tool).ok()??;
    read_report(&instance.id, plugin).ok().flatten()
}

fn private_fresh_file(metadata: &Metadata) -> Result<SystemTime> {
    ensure!(
        metadata.uid() == nix::unistd::geteuid().as_raw(),
        "integration report has another owner"
    );
    ensure!(
        metadata.mode() & 0o7777 == 0o600,
        "integration report must be mode 0600"
    );
    ensure!(
        metadata.len() <= MAX_REPORT_BYTES as u64,
        "integration report exceeds size limit"
    );
    let modified = metadata.modified()?;
    ensure!(
        modified.elapsed().is_ok_and(|age| age <= REPORT_MAX_AGE),
        "integration report expired"
    );
    Ok(modified)
}

fn read_report(
    instance_id: &str,
    plugin: &LoadedPlugin,
) -> Result<Option<Arc<AgentIntegrationSnapshot>>> {
    let Some(dir) = dir_guard::open_instance_dir_read_only(instance_id)? else {
        return Ok(None);
    };
    let filename = format!("integration-{}.json", plugin.id());
    let Some(metadata) = dir_guard::metadata_at(dir.as_fd(), &filename)? else {
        return Ok(None);
    };
    let modified = private_fresh_file(&metadata)?;
    let cache = &*CACHE;
    {
        let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(instance_id) {
            if entry.plugin_id == plugin.id()
                && entry.manifest_hash == plugin.manifest_hash
                && entry.modified == modified
                && entry.inode == metadata.ino()
                && entry.len == metadata.len()
            {
                ensure!(
                    crate::process::is_pid_alive(entry.report.writer_pid),
                    "integration writer exited"
                );
                return Ok(Some(entry.report.clone()));
            }
        }
    }
    let Some((metadata, bytes)) =
        dir_guard::read_file_with_metadata_at(dir.as_fd(), &filename, MAX_REPORT_BYTES + 1)?
    else {
        return Ok(None);
    };
    let modified = private_fresh_file(&metadata)?;
    ensure!(
        bytes.len() <= MAX_REPORT_BYTES,
        "integration report exceeds size limit"
    );
    let report: AgentIntegrationSnapshot =
        serde_json::from_slice(&bytes).context("invalid integration report")?;
    validate_report(&report, instance_id)?;
    ensure!(
        crate::process::is_pid_alive(report.writer_pid),
        "integration writer exited"
    );
    let report = Arc::new(report);
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
        instance_id.to_owned(),
        CachedReport {
            plugin_id: plugin.id().to_owned(),
            manifest_hash: plugin.manifest_hash.clone(),
            modified,
            inode: metadata.ino(),
            len: metadata.len(),
            report: report.clone(),
        },
    );
    Ok(Some(report))
}

fn validate_report(report: &AgentIntegrationSnapshot, instance_id: &str) -> Result<()> {
    ensure!(report.schema_version == 1, "unsupported integration schema");
    ensure!(
        report.instance_id == instance_id,
        "integration instance mismatch"
    );
    ensure!(
        !report.provider_session_id.is_empty() && report.provider_session_id.len() <= 128,
        "missing provider session identity"
    );
    ensure!(
        report.writer_pid > 0 && report.writer_pid <= i32::MAX as u32,
        "invalid integration writer PID"
    );
    ensure!(
        !report.reply_keys.is_empty() && report.reply_keys.len() <= 8,
        "invalid integration wake sequence"
    );
    for key in &report.reply_keys {
        let value = match key {
            AgentIntegrationKey::Literal(v) | AgentIntegrationKey::Named(v) => v,
        };
        ensure!(
            !value.is_empty() && value.len() <= 128,
            "invalid integration wake key"
        );
        if matches!(key, AgentIntegrationKey::Named(_)) {
            ensure!(
                value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'_')),
                "invalid named integration key"
            );
        }
    }
    if let Some(prompt) = &report.prompt {
        ensure!(
            report.status == AgentIntegrationStatus::Waiting,
            "prompt on non-waiting integration"
        );
        ensure!(
            !prompt.id().is_empty() && prompt.id().len() <= 256,
            "missing prompt identity"
        );
        if let AgentIntegrationPrompt::Question { questions, .. } = prompt {
            ensure!(!questions.is_empty(), "empty question prompt");
            let mut ids = HashSet::new();
            for question in questions {
                ensure!(
                    !question.id.is_empty() && ids.insert(&question.id),
                    "duplicate or missing question identity"
                );
                let mut labels = HashSet::new();
                for option in &question.options {
                    ensure!(
                        !option.label.is_empty() && labels.insert(&option.label),
                        "duplicate or missing option label"
                    );
                }
                if let Some(index) = question.recommended {
                    ensure!(index < question.options.len(), "invalid recommended option");
                }
            }
        }
    }
    Ok(())
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
            for (question, answer) in questions.iter().zip(answers) {
                ensure!(question.id == answer.id, "question identity changed");
                let mut selected = HashSet::new();
                for label in &answer.selected_options {
                    ensure!(
                        selected.insert(label)
                            && question.options.iter().any(|o| o.label == *label),
                        "invalid question selection"
                    );
                }
                let custom = answer
                    .custom_input
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !question.multi {
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

fn same_prompt(expected: &AgentIntegrationSnapshot, current: &AgentIntegrationSnapshot) -> bool {
    expected.instance_id == current.instance_id
        && expected.provider_session_id == current.provider_session_id
        && expected.writer_pid == current.writer_pid
        && expected.revision == current.revision
        && expected.prompt.as_ref().map(AgentIntegrationPrompt::id)
            == current.prompt.as_ref().map(AgentIntegrationPrompt::id)
}

/// Validate against the current provider report before waking the existing pane.
/// The provider checks the same identity again before applying the user's answer.
pub fn respond(
    instance: &Instance,
    expected: &AgentIntegrationSnapshot,
    response: AgentIntegrationResponse,
) -> Result<()> {
    ensure!(
        !instance.is_structured(),
        "structured sessions use ACP responses"
    );
    ensure!(
        !instance.is_sandboxed(),
        "sandboxed sessions do not support provider integration replies"
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
        .map(|key| match key {
            AgentIntegrationKey::Literal(text) => crate::agents::KeyToken::Literal(text),
            AgentIntegrationKey::Named(name) => crate::agents::KeyToken::Named(name),
        })
        .collect();
    tmux.send_key_tokens(&tokens)
}

/// Change policy only through an authenticated native provider acknowledgment.
/// No key is sent: providers poll controls independently of open prompt dialogs.
pub fn set_auto_approve(instance: &Instance, enabled: bool) -> Result<()> {
    ensure!(
        supports_provider_integration(instance),
        "native approval control is unavailable"
    );
    let registry = super::registry();
    let plugin = provider(&registry, &instance.tool)?
        .context("provider integration is not enabled and approved")?;
    let current =
        read_report(&instance.id, plugin)?.context("provider integration report is unavailable")?;
    let policy = current
        .auto_approve
        .as_ref()
        .context("provider does not advertise runtime approval control")?;
    ensure!(
        enabled || policy.reversible,
        "provider requires restart to disable forced auto-approve"
    );
    let tmux = instance.tmux_session()?;
    ensure!(
        tmux.exists() && !tmux.is_pane_dead(),
        "target session is not live"
    );
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = IntegrationControl {
        schema_version: 1,
        instance_id: &current.instance_id,
        provider_session_id: &current.provider_session_id,
        writer_pid: current.writer_pid,
        revision: current.revision,
        request_id: &request_id,
        enabled,
    };
    let dir = dir_guard::open_instance_dir(&instance.id)?;
    let filename = format!("integration-{}.control.json", plugin.id());
    dir_guard::write_atomic(dir.as_fd(), &filename, &serde_json::to_vec(&request)?)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let outcome = (|| loop {
        let report = read_report(&instance.id, plugin)?
            .context("provider report disappeared before acknowledgment")?;
        ensure!(
            report.provider_session_id == current.provider_session_id
                && report.writer_pid == current.writer_pid,
            "provider session changed before acknowledgment"
        );
        if let Some(ack) = report
            .control_ack
            .as_ref()
            .filter(|ack| ack.request_id == request_id)
        {
            ensure!(
                ack.revision == current.revision,
                "control acknowledgment revision mismatch"
            );
            if let Some(error) = &ack.error {
                break Err(anyhow::anyhow!("{error}"));
            }
            ensure!(
                ack.enabled == enabled
                    && report
                        .auto_approve
                        .as_ref()
                        .is_some_and(|p| p.enabled == enabled),
                "provider did not acknowledge requested approval policy"
            );
            break Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break Err(anyhow::anyhow!("provider did not acknowledge approval control; live state is unknown, metadata was not changed"));
        }
        std::thread::sleep(Duration::from_millis(25));
    })();
    let _ = nix::unistd::unlinkat(
        dir.as_fd(),
        filename.as_str(),
        nix::unistd::UnlinkatFlags::NoRemoveDir,
    );
    outcome
}

pub(crate) fn forget(instance_id: &str) {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(instance_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::agent_integration::{
        AgentIntegrationAnswer, AgentIntegrationOption, AgentIntegrationQuestion,
    };

    fn report() -> AgentIntegrationSnapshot {
        AgentIntegrationSnapshot {
            schema_version: 1,
            instance_id: "native_test".into(),
            provider_session_id: "provider-session".into(),
            writer_pid: std::process::id(),
            revision: 1,
            status: AgentIntegrationStatus::Waiting,
            reply_keys: vec![AgentIntegrationKey::Named("F12".into())],
            auto_approve: None,
            control_ack: None,
            prompt: Some(AgentIntegrationPrompt::Approval {
                id: "approval-1".into(),
                title: "Bash".into(),
                allow_always: false,
            }),
        }
    }

    #[test]
    fn sandboxed_instances_are_not_eligible_for_provider_integration() {
        let mut instance = Instance::new("native_test", "/tmp/native_test");
        instance.tool = "claude".into();
        instance.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".into(),
            container_name: "test".into(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        assert!(!supports_provider_integration(&instance));
        let error = respond(
            &instance,
            &report(),
            AgentIntegrationResponse::Approval {
                choice: IntegrationApprovalChoice::Deny,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "sandboxed sessions do not support provider integration replies"
        );
    }

    #[test]
    fn prompt_identity_and_answer_validation_reject_stale_or_invalid_replies() {
        let expected = report();
        assert!(validate_report(&expected, "native_test").is_ok());
        for mutate in [
            (|r: &mut AgentIntegrationSnapshot| r.revision += 1)
                as fn(&mut AgentIntegrationSnapshot),
            |r| r.writer_pid += 1,
            |r| r.provider_session_id.push('x'),
            |r| r.prompt = None,
        ] {
            let mut current = expected.clone();
            mutate(&mut current);
            assert!(!same_prompt(&expected, &current));
        }
        assert!(validate_report(&expected, "another_session").is_err());
        for mutate in [
            (|r: &mut AgentIntegrationSnapshot| r.status = AgentIntegrationStatus::Running)
                as fn(&mut AgentIntegrationSnapshot),
            |r| r.reply_keys.clear(),
            |r| r.reply_keys = vec![AgentIntegrationKey::Named("bad key".into())],
            |r| r.writer_pid = 0,
        ] {
            let mut invalid = expected.clone();
            mutate(&mut invalid);
            assert!(
                validate_report(&invalid, "native_test").is_err(),
                "{invalid:?}"
            );
        }
        assert!(validate_response(
            expected.prompt.as_ref().unwrap(),
            &AgentIntegrationResponse::Approval {
                choice: IntegrationApprovalChoice::AllowAlways
            }
        )
        .is_err());
        let prompt = AgentIntegrationPrompt::Question {
            id: "ask-1".into(),
            title: "Choose".into(),
            questions: vec![AgentIntegrationQuestion {
                id: "storage".into(),
                question: "Database?".into(),
                multi: false,
                recommended: None,
                options: vec![AgentIntegrationOption {
                    label: "SQLite".into(),
                    description: None,
                }],
            }],
        };
        for (selected, custom, valid) in [
            (vec!["SQLite"], None, true),
            (vec![], Some("Postgres"), true),
            (vec!["missing"], None, false),
            (vec!["SQLite", "SQLite"], None, false),
            (vec!["SQLite"], Some("Postgres"), false),
            (vec![], None, false),
        ] {
            let response = AgentIntegrationResponse::Question {
                answers: vec![AgentIntegrationAnswer {
                    id: "storage".into(),
                    selected_options: selected.into_iter().map(str::to_owned).collect(),
                    custom_input: custom.map(str::to_owned),
                }],
            };
            assert_eq!(validate_response(&prompt, &response).is_ok(), valid);
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn report_reader_rejects_symlinks_permissions_and_dead_writers() {
        use crate::hooks::test_support::BaseGuard;
        use std::os::unix::fs::PermissionsExt;
        let (_guard, base, _tmp) = BaseGuard::ready();
        let plugin = LoadedPlugin {
            manifest: aoe_plugin_api::PluginManifest::from_toml_str("id='test.native'\nname='Native'\nversion='1'\napi_version=13\ncapabilities=['session.integration']\n[[agent_integrations]]\ntool='test'").unwrap(),
            enabled: true, granted: true, trust: aoe_plugin_api::TrustLevel::Community,
            validation: super::super::registry::ValidationState::Local, source: None, dir: None, manifest_hash: "test".into(),
        };
        let dir = dir_guard::open_instance_dir("native_test").unwrap();
        let filename = "integration-test.native.json";
        dir_guard::write_atomic(
            dir.as_fd(),
            filename,
            &serde_json::to_vec(&report()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_report("native_test", &plugin).unwrap().unwrap().status,
            AgentIntegrationStatus::Waiting
        );
        let path = base.join("native_test").join(filename);
        let old_time = SystemTime::now() - REPORT_MAX_AGE - Duration::from_secs(1);
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        assert!(read_report("native_test", &plugin).is_err());
        dir_guard::write_atomic(
            dir.as_fd(),
            filename,
            &serde_json::to_vec(&report()).unwrap(),
        )
        .unwrap();
        assert!(read_report("native_test", &plugin).unwrap().is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_report("native_test", &plugin).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(read_report("native_test", &plugin).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        let mut dead = report();
        dead.writer_pid = i32::MAX as u32;
        dir_guard::write_atomic(dir.as_fd(), filename, &serde_json::to_vec(&dead).unwrap())
            .unwrap();
        assert!(read_report("native_test", &plugin).is_err());
        forget("native_test");
    }
}
