//! Verified, advertised native ACP approval controls. Planning modes are not
//! approval controls (notably OMP's Default/Plan selector).

use super::state::{ConfigOptionCategory, ConfigOptionDescriptor, Event};

#[derive(Default)]
pub(crate) struct ApprovalOptions {
    config: Vec<ConfigOptionDescriptor>,
    modes: Vec<String>,
    current_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalTarget {
    pub config_id: Option<String>,
    pub mode_id: String,
}

fn native_modes(agent: &str) -> Option<(&'static str, &'static [&'static str])> {
    match agent {
        "claude" | "claude-code" => Some(("bypassPermissions", &["default"])),
        "codex" => Some(("agent-full-access", &["agent"])),
        "gemini" | "kimi" => Some(("yolo", &["default"])),
        "vibe" => Some(("auto-approve", &["ask", "default"])),
        _ => None,
    }
}

pub(crate) fn is_native_approval_mode(agent: &str, mode: &str) -> bool {
    native_modes(agent).is_some_and(|(bypass, safe)| mode == bypass || safe.contains(&mode))
}

impl ApprovalOptions {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::ConfigOptionsUpdated { options } => {
                self.config = options
                    .iter()
                    .filter(|option| option.category == ConfigOptionCategory::Mode)
                    .cloned()
                    .collect();
                if let Some(option) = self.config.first() {
                    self.current_mode = Some(option.current_value.clone());
                }
            }
            Event::ModesAvailable {
                modes,
                current_mode_id,
            } => {
                self.current_mode = Some(current_mode_id.clone());
                self.modes = modes.iter().map(|mode| mode.id.clone()).collect();
            }
            Event::CurrentModeChanged { current_mode_id } => {
                self.current_mode = Some(current_mode_id.clone());
            }
            _ => {}
        }
    }

    pub fn current_mode(&self) -> Option<&str> {
        self.current_mode.as_deref()
    }

    pub fn target(&self, agent: &str, enabled: bool) -> Result<ApprovalTarget, String> {
        let (bypass, safe) = native_modes(agent).ok_or_else(|| {
            if agent == "omp" {
                "OMP Default/Plan modes do not control approvals; a resumable native approval-policy restart is required".into()
            } else {
                format!("Auto-approve switching is unsupported for ACP agent {agent:?}")
            }
        })?;
        let mut config_modes = self
            .config
            .iter()
            .filter(|option| option.category == ConfigOptionCategory::Mode);
        let config_mode = config_modes.next();
        if config_modes.next().is_some() {
            return Err("The adapter advertises ambiguous mode selectors".into());
        }
        let contains = |value: &str| match config_mode {
            Some(option) => option.options.iter().any(|choice| choice.value == value),
            None => self.modes.iter().any(|mode| mode == value),
        };
        let safe = safe.iter().copied().find(|safe| contains(safe));
        if !contains(bypass) || safe.is_none() {
            return Err(format!("ACP agent {agent:?} does not advertise both a verified bypass mode and a safe approval mode"));
        }
        Ok(ApprovalTarget {
            config_id: config_mode.map(|option| option.id.clone()),
            mode_id: if enabled { bypass } else { safe.unwrap() }.to_string(),
        })
    }
}

/// Native OMP flags are separate from its ACP Default/Plan channel. Reject
/// wrappers and custom argv rather than trying to override unknown policies.
pub(crate) fn configure_omp_approval(
    spec: &mut super::AgentSpec,
    enabled: bool,
) -> Result<(), String> {
    if std::path::Path::new(&spec.command)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("omp")
        || spec.args.first().map(String::as_str) != Some("acp")
        || spec.args.len() > 2
        || spec
            .args
            .get(1)
            .is_some_and(|arg| arg != "--approval-mode=yolo" && arg != "--approval-mode=always-ask")
    {
        return Err("OMP approval switching is unsupported with a custom ACP launch command or permission flags".into());
    }
    spec.args.truncate(1);
    spec.args.push(
        if enabled {
            "--approval-mode=yolo"
        } else {
            "--approval-mode=always-ask"
        }
        .into(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::ConfigOptionChoice;

    #[test]
    fn requires_verified_agent_and_advertised_reversible_control() {
        let mut metadata = ApprovalOptions::default();
        for (agent, on, off) in [
            ("claude", "bypassPermissions", "default"),
            ("codex", "agent-full-access", "agent"),
            ("gemini", "yolo", "default"),
            ("kimi", "yolo", "default"),
            ("vibe", "auto-approve", "ask"),
            ("vibe", "auto-approve", "default"),
        ] {
            metadata.modes = vec![on.into(), off.into()];
            assert_eq!(metadata.target(agent, true).unwrap().mode_id, on);
            assert_eq!(metadata.target(agent, false).unwrap().mode_id, off);
        }
        metadata.modes = vec!["bypassPermissions".into(), "default".into()];
        assert!(metadata.target("aoe-agent", true).is_err());
        assert!(metadata.target("custom", true).is_err());
        assert!(metadata.target("omp", true).is_err());
        metadata.config = vec![ConfigOptionDescriptor {
            id: "actual-permission-selector".into(),
            name: "Mode".into(),
            description: None,
            category: ConfigOptionCategory::Mode,
            current_value: "default".into(),
            options: ["default", "bypassPermissions"]
                .into_iter()
                .map(|value| ConfigOptionChoice {
                    value: value.into(),
                    name: value.into(),
                    description: None,
                })
                .collect(),
        }];
        assert_eq!(
            metadata
                .target("claude", true)
                .unwrap()
                .config_id
                .as_deref(),
            Some("actual-permission-selector")
        );
        metadata.config[0].options.pop();
        assert!(
            metadata.target("claude", true).is_err(),
            "config modes take precedence over legacy metadata"
        );
    }

    #[test]
    fn omp_launch_policy_replaces_only_known_native_flags() {
        let mut spec = super::super::AgentSpec {
            command: "/opt/homebrew/bin/omp".into(),
            args: vec!["acp".into()],
            description: String::new(),
            env_allowlist: None,
        };
        configure_omp_approval(&mut spec, true).unwrap();
        assert_eq!(spec.args, ["acp", "--approval-mode=yolo"]);
        configure_omp_approval(&mut spec, false).unwrap();
        assert_eq!(spec.args, ["acp", "--approval-mode=always-ask"]);
        for args in [
            vec!["acp".into(), "--auto-approve".into()],
            vec![
                "acp".into(),
                "--approval-mode=yolo".into(),
                "--approval-mode=write".into(),
            ],
        ] {
            spec.args = args;
            let original = spec.args.clone();
            assert!(configure_omp_approval(&mut spec, false).is_err());
            assert_eq!(spec.args, original);
        }
    }

    #[test]
    fn latest_mode_event_wins_across_native_channels() {
        let mut metadata = ApprovalOptions::default();
        let option = ConfigOptionDescriptor {
            id: "mode".into(),
            name: "Mode".into(),
            description: None,
            category: ConfigOptionCategory::Mode,
            current_value: "default".into(),
            options: Vec::new(),
        };
        for (event, expected) in [
            (
                Event::ConfigOptionsUpdated {
                    options: vec![option.clone()],
                },
                "default",
            ),
            (
                Event::CurrentModeChanged {
                    current_mode_id: "plan".into(),
                },
                "plan",
            ),
            (
                Event::ConfigOptionsUpdated {
                    options: vec![option],
                },
                "default",
            ),
        ] {
            metadata.apply(&event);
            assert_eq!(metadata.current_mode(), Some(expected));
        }
    }
}
