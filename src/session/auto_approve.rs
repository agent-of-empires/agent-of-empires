//! Acknowledged terminal policy changes and strict same-conversation restarts.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::{ClaimOp, Instance, ResumeIntent, StartOutcome, Status, Storage};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoApproveResult {
    pub enabled: bool,
    pub restarted: bool,
}

/// Native policies that explicitly override restored/configured bypass mode.
/// These retain native per-tool allow/deny rules; OFF is not "revoke every grant".
pub(crate) fn safe_off_args(tool: &str) -> Result<&'static str> {
    match tool {
        "claude" => Ok("--permission-mode default"),
        "codex" => Ok("--ask-for-approval on-request --sandbox workspace-write"),
        "gemini" | "qwen" => Ok("--approval-mode default"),
        "vibe" => bail!("Vibe's safe agent is version-dependent (default/ask); no verified reversible adapter for this version"),
        "omp" => Ok("--approval-mode always-ask"),
        "cursor" | "droid" | "antigravity" => bail!("{tool} has no verified same-conversation terminal resume; approval switching is unsupported"),
        "pi" | "settl" => bail!("{tool} always runs without an approval gate; OFF is unsupported"),
        _ => bail!("{tool} has no verified reversible terminal approval policy; use its native controls"),
    }
}

fn validate_launch(instance: &Instance) -> Result<()> {
    ensure!(
        !instance.is_structured(),
        "structured sessions must use the daemon approval endpoint"
    );
    ensure!(
        !instance.is_sandboxed(),
        "sandboxed terminal approval switching is not verified"
    );
    ensure!(
        !instance.is_trashed(),
        "trashed sessions cannot change approval policy"
    );
    ensure!(
        !matches!(instance.status, Status::Creating | Status::Deleting),
        "session is changing lifecycle state"
    );
    ensure!(
        instance.command.is_empty(),
        "custom commands may force permissions; approval switching is unsupported"
    );
    let config = super::resolve_config_with_repo(
        &instance.effective_profile(),
        std::path::Path::new(&instance.project_path),
    )?;
    ensure!(
        config
            .session
            .resolve_tool_command(&instance.tool)
            .is_empty(),
        "custom agent command overrides may force permissions"
    );
    let extra = config
        .session
        .agent_extra_args
        .get(&instance.tool)
        .map(String::as_str)
        .unwrap_or("");
    for args in [instance.extra_args.as_str(), extra] {
        validate_arguments(args)?;
    }
    safe_off_args(&instance.tool)?;
    Ok(())
}

fn validate_arguments(args: &str) -> Result<()> {
    ensure!(
        !args
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '\n' | '\r' | '<' | '>')),
        "shell expressions in custom arguments prevent verified approval switching"
    );
    for token in shell_words::split(args).context("invalid custom agent arguments")? {
        let flag = token.split('=').next().unwrap_or(&token);
        ensure!(
            !["-a", "-s", "-c", "-r", "-S"]
                .iter()
                .any(|short| flag.starts_with(short) && !flag.starts_with("--")),
            "custom short flag {flag} can override permission or conversation policy"
        );
        ensure!(!matches!(flag,
            "--auto-approve" | "--yolo" | "--allow-all" | "--allow-all-tools" |
            "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions" |
            "--dangerously-bypass-approvals-and-sandbox" | "--skip-permissions-unsafe" |
            "--approval-mode" | "--permission-mode" | "--permission-prompts" |
            "--ask-for-approval" | "--sandbox" | "--full-auto" | "--approve-for-me" |
            "--not-so-yolo" | "--permissions" | "--trust-all-tools" | "--agent" |
            "--config" | "--config-file" | "--settings" | "--settings-file" |
            "--resume" | "--restore" | "--continue" | "--session" | "--session-id" |
            "--resume-id" | "--fork-session" | "--no-session" | "--plan-yolo" | "-y" | "-a" | "-s" | "-c" | "-r" | "-S"),
            "custom flag {flag} can override permission or conversation policy; remove it before switching auto-approve");
    }
    Ok(())
}

fn claim(storage: &Storage, id: &str, profile: &str) -> Result<Instance> {
    storage.update(|instances, _| {
        let row = instances
            .iter_mut()
            .find(|i| i.id == id)
            .context("session not found")?;
        row.source_profile = profile.to_owned();
        validate_launch(row)?;
        let now = chrono::Utc::now();
        ensure!(
            !row.op_claim
                .as_ref()
                .is_some_and(|claim| now - claim.at < Instance::OP_CLAIM_TTL),
            "another lifecycle operation is in progress"
        );
        row.try_claim(ClaimOp::AutoApprove, Instance::OP_CLAIM_TTL, now)
            .map_err(|op| anyhow::anyhow!("session is reserved for {op:?}"))?;
        Ok(row.clone())
    })
}
/// Keep an approval override from taking precedence over the terminal policy.
/// OMP's Default/Plan selector is independent of approvals and must remain pinned.
fn reconcile_approval_mode(instance: &mut Instance, enabled: bool) {
    if instance.tool == "omp" {
        return;
    }
    let native = crate::acp::auto_approve::is_native_approval_mode(
        &instance.tool,
        instance.acp_mode_id.as_deref().unwrap_or_default(),
    );
    if !native {
        return;
    }
    instance.acp_mode_id = if enabled {
        crate::acp::agent_profiles::resolve(&instance.tool)
            .yolo_mode_id
            .map(str::to_owned)
    } else {
        match instance.tool.as_str() {
            "claude" | "gemini" | "qwen" => Some("default".into()),
            "codex" => Some("agent".into()),
            "vibe" => Some("ask".into()),
            _ => None,
        }
    };
}

fn persist_policy(instance: &mut Instance, enabled: bool) {
    reconcile_approval_mode(instance, enabled);
    instance.yolo_mode = enabled;
    instance.auto_approve_managed = true;
}

fn omp_restart_requires_ack(
    report: Option<&aoe_plugin_api::agent_integration::AgentIntegrationSnapshot>,
) -> bool {
    report.is_some_and(|report| report.auto_approve.is_some())
}

/// Change a terminal session's policy. Storage is authoritative and is only
/// updated after native acknowledgment (or a strict resumed native launch).
/// Stopped sessions only receive the intended policy for their next launch.
pub fn set_terminal_auto_approve(
    profile: &str,
    id: &str,
    enabled: bool,
) -> Result<AutoApproveResult> {
    ensure!(
        std::env::var_os("AOE_CITYHALL_MODE").is_none(),
        "approval switching is unavailable in CityHall mode"
    );
    let storage = Storage::open_unwatched(profile)?;
    let mut working = claim(&storage, id, profile)?;
    let before = working.clone();
    let outcome = (|| {
        if working.status == Status::Starting {
            working.update_status();
            ensure!(
                working.status != Status::Starting,
                "session is still starting"
            );
        }
        let tmux = working.tmux_session()?;
        if !tmux.exists() || tmux.is_pane_dead() {
            return Ok(AutoApproveResult {
                enabled,
                restarted: false,
            });
        }
        let report = crate::plugin::agent_integration::snapshot(&working);
        if report
            .as_ref()
            .and_then(|r| r.auto_approve.as_ref())
            .is_some_and(|policy| enabled || policy.reversible)
        {
            crate::plugin::agent_integration::set_auto_approve(&working, enabled)?;
            return Ok(AutoApproveResult {
                enabled,
                restarted: false,
            });
        }
        let agent = crate::agents::get_agent(&working.tool).context("unknown agent")?;
        ensure!(
            !matches!(
                agent.resume_strategy,
                crate::agents::ResumeStrategy::Unsupported
            ),
            "agent cannot resume; refusing to replace the conversation"
        );
        let sid = working
            .agent_session_id
            .clone()
            .context("no captured resumable conversation ID; refusing to restart")?;
        ensure!(
            super::is_valid_session_id(&sid),
            "invalid captured conversation ID"
        );
        ensure!(
            working.resume_intent == ResumeIntent::Default
                || working.resume_intent == ResumeIntent::Use(sid.clone()),
            "pending resume intent would change the conversation; refusing to restart"
        );
        ensure!(
            report
                .as_ref()
                .is_none_or(|report| report.provider_session_id == sid),
            "native provider conversation differs from captured resume ID"
        );
        if working.tool == "omp" {
            ensure!(
                report
                    .as_ref()
                    .and_then(|snapshot| snapshot.auto_approve.as_ref())
                    .is_some(),
                "OMP native approval extension is unavailable; refusing to restart without acknowledged controls"
            );
        }
        let help = crate::process::run_with_timeout(
            std::process::Command::new(agent.binary).arg("--help"),
            std::time::Duration::from_secs(5),
        )
        .with_context(|| {
            format!(
                "cannot check {} approval flags before restart",
                agent.binary
            )
        })?
        .context("agent help timed out; refusing to restart")?;
        ensure!(
            help.status.success(),
            "agent help failed; cannot verify reversible launch support"
        );
        let help = String::from_utf8_lossy(&help.stdout);
        let required = safe_off_args(&working.tool)?
            .split_whitespace()
            .next()
            .unwrap();
        ensure!(
            help.contains(required),
            "installed agent does not advertise {required}; refusing to restart"
        );
        let outcome = working.restart_with_auto_approve(enabled, &sid)?;
        ensure!(
            outcome == StartOutcome::Resumed,
            "approval restart did not preserve the conversation"
        );
        if working.tool == "omp" && omp_restart_requires_ack(report.as_deref()) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(resumed) = crate::plugin::agent_integration::snapshot(&working) {
                    if report
                        .as_ref()
                        .is_none_or(|prior| prior.writer_pid != resumed.writer_pid)
                    {
                        ensure!(
                            resumed.provider_session_id == sid,
                            "OMP resumed a different conversation; metadata was not changed"
                        );
                        ensure!(
                            resumed
                                .auto_approve
                                .as_ref()
                                .is_some_and(|policy| policy.enabled == enabled),
                            "OMP did not start with the requested approval policy"
                        );
                        crate::plugin::agent_integration::set_auto_approve(&working, enabled)?;
                        break;
                    }
                }
                ensure!(std::time::Instant::now() < deadline,
                    "OMP did not acknowledge its resumed conversation and approval policy; metadata was not changed");
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        Ok(AutoApproveResult {
            enabled,
            restarted: true,
        })
    })();
    let committed = storage.update(|instances, _| {
        let row = instances
            .iter_mut()
            .find(|i| i.id == id)
            .context("session disappeared during approval change")?;
        ensure!(
            row.op_claim == before.op_claim,
            "approval lifecycle reservation was lost; metadata was not changed"
        );
        if let Ok(result) = &outcome {
            ensure!(
                row.tool == before.tool
                    && row.command == before.command
                    && row.extra_args == before.extra_args
                    && row.project_path == before.project_path,
                "launch configuration changed during approval control; metadata was not changed"
            );
            if result.restarted {
                row.merge_post_restart_with_baseline(&before, &working);
            }
            persist_policy(row, result.enabled);
        } else if working.last_error != before.last_error {
            row.status = working.status;
            row.last_error = working.last_error.clone();
        }
        row.op_claim = None;
        Ok(())
    });
    if committed.is_err() {
        let _ = storage.update(|instances, _| {
            if let Some(row) = instances
                .iter_mut()
                .find(|i| i.id == id && i.op_claim == before.op_claim)
            {
                row.op_claim = None;
            }
            Ok(())
        });
    }
    committed?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_permission_and_conversation_overrides_before_mutation() {
        for args in [
            "--model 'model[1m]' --extension plugin.ts",
            "--thinking high",
            "",
        ] {
            assert!(validate_arguments(args).is_ok(), "{args}");
        }
        for args in [
            "--yolo",
            "--approval-mode=yolo",
            "--config overlay.yaml",
            "-a never",
            "--resume other",
            "--agent custom",
            "--model x; omp --auto-approve",
            "--model $MODEL",
        ] {
            assert!(validate_arguments(args).is_err(), "{args}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_claim_excludes_peers_and_failed_validation_does_not_mutate_policy() {
        let _home = super::super::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("default").unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut instance = Instance::new("approval", project.path().to_str().unwrap());
        instance.tool = "omp".into();
        let id = instance.id.clone();
        storage
            .update(|rows, _| {
                rows.push(instance);
                Ok(())
            })
            .unwrap();
        let claimed = claim(&storage, &id, "default").unwrap();
        assert!(claim(&storage, &id, "default").is_err());
        storage
            .update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                assert_eq!(row.op_claim, claimed.op_claim);
                assert!(!row.yolo_mode);
                row.op_claim = None;
                row.extra_args = "--auto-approve".into();
                Ok(())
            })
            .unwrap();
        assert!(set_terminal_auto_approve("default", &id, false).is_err());
        let row = storage.load().unwrap().pop().unwrap();
        assert!(!row.yolo_mode);
        assert!(row.op_claim.is_none());
        storage
            .update(|rows, _| {
                rows[0].extra_args.clear();
                Ok(())
            })
            .unwrap();
        let repo_config = project.path().join(".agent-of-empires");
        std::fs::create_dir(&repo_config).unwrap();
        std::fs::write(repo_config.join("config.toml"), "[session\n").unwrap();
        assert!(
            set_terminal_auto_approve("default", &id, true).is_err(),
            "malformed repository overrides must not silently fall back to profile-only policy"
        );
        let row = storage.load().unwrap().pop().unwrap();
        assert!(!row.yolo_mode);
        assert!(row.op_claim.is_none());
    }

    #[test]
    fn terminal_policy_reconciles_native_approval_pins_without_changing_omp_mode() {
        let mut instance = Instance::new("approval", "/tmp");
        instance.tool = "claude".into();
        instance.acp_mode_id = Some("default".into());
        instance.yolo_mode = false;
        persist_policy(&mut instance, true);
        assert!(instance.yolo_mode && instance.auto_approve_managed);
        assert_eq!(instance.acp_mode_id.as_deref(), Some("bypassPermissions"));
        persist_policy(&mut instance, false);
        assert!(!instance.yolo_mode && instance.auto_approve_managed);
        assert_eq!(instance.acp_mode_id.as_deref(), Some("default"));

        instance.tool = "omp".into();
        instance.acp_mode_id = Some("plan".into());
        persist_policy(&mut instance, true);
        assert!(instance.yolo_mode);
        assert_eq!(instance.acp_mode_id.as_deref(), Some("plan"));
        persist_policy(&mut instance, false);
        assert!(!instance.yolo_mode);
        assert_eq!(instance.acp_mode_id.as_deref(), Some("plan"));
    }

    #[test]
    fn omp_restart_ack_is_required_only_when_runtime_advertises_policy() {
        use aoe_plugin_api::agent_integration::{
            AgentIntegrationSnapshot, AgentIntegrationStatus, IntegrationAutoApprove,
        };

        let report = |auto_approve| AgentIntegrationSnapshot {
            schema_version: 1,
            instance_id: "session".into(),
            provider_session_id: "conversation".into(),
            writer_pid: 1,
            revision: 1,
            status: AgentIntegrationStatus::Running,
            reply_keys: Vec::new(),
            prompt: None,
            auto_approve,
            control_ack: None,
        };
        assert!(!omp_restart_requires_ack(None));
        assert!(!omp_restart_requires_ack(Some(&report(None))));
        assert!(omp_restart_requires_ack(Some(&report(Some(
            IntegrationAutoApprove {
                enabled: false,
                reversible: false,
            }
        )))));
    }
}
