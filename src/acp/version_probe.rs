use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use semver::Version;

use crate::acp::agent_compat::{version_gate_for, ExpectedAgent, VersionGate};
use crate::acp::agent_registry::AgentRegistry;
use crate::session::Instance;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    Missing,
    Version { raw: String, parsed: Version },
    Unparseable { raw: String },
    Failed { message: String },
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionWarningKind {
    Missing,
    BelowMinimum { installed: String },
    Unparseable { raw: String },
    Failed { message: String },
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionWarning {
    pub gate: VersionGate,
    pub kind: VersionWarningKind,
}

impl VersionWarning {
    pub fn reason(&self) -> String {
        match &self.kind {
            VersionWarningKind::Missing => "not found on PATH".to_string(),
            VersionWarningKind::BelowMinimum { installed } => format!(
                "installed {installed}; requires >={}",
                self.gate.min_version
            ),
            VersionWarningKind::Unparseable { raw } => {
                format!("reported an unparseable version `{raw}`")
            }
            VersionWarningKind::Failed { message } => format!("version probe failed: {message}"),
            VersionWarningKind::TimedOut => "version probe timed out".to_string(),
        }
    }

    pub fn render(&self) -> String {
        format!(
            "warning: structured ACP adapter {} {}; aoe requires {} >= {}. Run: {}. Existing structured sessions using this adapter will fail until upgraded.",
            self.gate.binary,
            self.reason(),
            self.gate.package_name,
            self.gate.min_version,
            self.gate.install_command,
        )
    }
}

pub fn extract_semver(raw: &str) -> Option<Version> {
    raw.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+'))
        .filter_map(|token| {
            let token = token.trim_start_matches('v');
            token
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
                .then(|| Version::parse(token).ok())
                .flatten()
        })
        .next()
}

pub async fn probe_binary_version(binary: &str) -> ProbeStatus {
    let Ok(path) = which::which(binary) else {
        return ProbeStatus::Missing;
    };
    let child = tokio::process::Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => {
            return ProbeStatus::Failed {
                message: e.to_string(),
            }
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let raw = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
            .trim()
            .to_string();
            if !output.status.success() {
                return ProbeStatus::Failed {
                    message: if raw.is_empty() {
                        format!("exited with {}", output.status)
                    } else {
                        raw
                    },
                };
            }
            match extract_semver(&raw) {
                Some(parsed) => ProbeStatus::Version { raw, parsed },
                None => ProbeStatus::Unparseable { raw },
            }
        }
        Ok(Err(e)) => ProbeStatus::Failed {
            message: e.to_string(),
        },
        Err(_) => ProbeStatus::TimedOut,
    }
}

pub fn warning_for_probe(gate: VersionGate, probe: &ProbeStatus) -> Option<VersionWarning> {
    match probe {
        ProbeStatus::Missing => Some(VersionWarning {
            gate,
            kind: VersionWarningKind::Missing,
        }),
        ProbeStatus::Version { parsed, .. } => {
            let min = Version::parse(gate.min_version).ok()?;
            (parsed < &min).then(|| VersionWarning {
                gate,
                kind: VersionWarningKind::BelowMinimum {
                    installed: parsed.to_string(),
                },
            })
        }
        ProbeStatus::Unparseable { raw } => Some(VersionWarning {
            gate,
            kind: VersionWarningKind::Unparseable { raw: raw.clone() },
        }),
        ProbeStatus::Failed { message } => Some(VersionWarning {
            gate,
            kind: VersionWarningKind::Failed {
                message: message.clone(),
            },
        }),
        ProbeStatus::TimedOut => Some(VersionWarning {
            gate,
            kind: VersionWarningKind::TimedOut,
        }),
    }
}

pub fn gates_needed_by_instances(instances: &[Instance]) -> Vec<VersionGate> {
    let registry = AgentRegistry::with_defaults();
    let mut seen = HashSet::new();
    let mut gates = Vec::new();
    for inst in instances.iter().filter(|inst| inst.is_structured()) {
        let explicit_agent = inst.agent_name.as_deref().filter(|name| !name.is_empty());
        let Some(spec) = explicit_agent
            .and_then(|agent| registry.get(agent))
            .or_else(|| {
                explicit_agent
                    .is_none()
                    .then(|| registry.get(&inst.tool))
                    .flatten()
            })
        else {
            continue;
        };
        let expected = ExpectedAgent::from_command(&spec.command);
        let Some(gate) = version_gate_for(expected) else {
            continue;
        };
        if seen.insert(gate.expected) {
            gates.push(gate);
        }
    }
    gates
}

pub async fn warn_for_structured_sessions(instances: &[Instance], print_to_stderr: bool) {
    let gates = gates_needed_by_instances(instances);
    let mut printed = false;
    for gate in gates {
        let probe = probe_binary_version(gate.binary).await;
        if let Some(warning) = warning_for_probe(gate, &probe) {
            let message = warning.render();
            if print_to_stderr {
                eprintln!("{message}");
                printed = true;
            }
            tracing::warn!(
                target: "acp.preflight",
                binary = warning.gate.binary,
                package = warning.gate.package_name,
                required = warning.gate.min_version,
                reason = %warning.reason(),
                "structured ACP adapter preflight failed"
            );
        }
    }
    if printed {
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::agent_compat::{CLAUDE_AGENT_ACP_MIN_VERSION, OPENCODE_MIN_VERSION};
    use crate::session::View;

    fn claude_gate() -> VersionGate {
        version_gate_for(ExpectedAgent::ClaudeAgentAcp).unwrap()
    }

    #[test]
    fn extract_semver_handles_realistic_outputs() {
        assert_eq!(extract_semver("0.55.0").unwrap().to_string(), "0.55.0");
        assert_eq!(
            extract_semver("claude-agent-acp 0.55.0")
                .unwrap()
                .to_string(),
            "0.55.0"
        );
        assert_eq!(extract_semver("v1.16.0").unwrap().to_string(), "1.16.0");
        assert_eq!(
            extract_semver("version=0.55.0-alpha.1")
                .unwrap()
                .to_string(),
            "0.55.0-alpha.1"
        );
        assert!(extract_semver("not-semver").is_none());
    }

    #[test]
    fn warning_for_probe_flags_only_unusable_versions() {
        let gate = claude_gate();
        assert!(matches!(
            warning_for_probe(
                gate,
                &ProbeStatus::Version {
                    raw: "0.0.1".to_string(),
                    parsed: Version::parse("0.0.1").unwrap(),
                },
            )
            .unwrap()
            .kind,
            VersionWarningKind::BelowMinimum { .. }
        ));
        assert!(warning_for_probe(
            gate,
            &ProbeStatus::Version {
                raw: CLAUDE_AGENT_ACP_MIN_VERSION.to_string(),
                parsed: Version::parse(CLAUDE_AGENT_ACP_MIN_VERSION).unwrap(),
            },
        )
        .is_none());
        assert!(warning_for_probe(
            gate,
            &ProbeStatus::Version {
                raw: "999.0.0".to_string(),
                parsed: Version::parse("999.0.0").unwrap(),
            },
        )
        .is_none());
    }

    #[test]
    fn warning_for_probe_covers_failed_probes() {
        let gate = claude_gate();
        assert!(matches!(
            warning_for_probe(gate, &ProbeStatus::Missing).unwrap().kind,
            VersionWarningKind::Missing
        ));
        assert!(matches!(
            warning_for_probe(
                gate,
                &ProbeStatus::Unparseable {
                    raw: "weird".to_string(),
                },
            )
            .unwrap()
            .kind,
            VersionWarningKind::Unparseable { .. }
        ));
        assert!(matches!(
            warning_for_probe(gate, &ProbeStatus::TimedOut)
                .unwrap()
                .kind,
            VersionWarningKind::TimedOut
        ));
    }

    #[test]
    fn gates_needed_by_instances_scopes_to_structured_sessions_and_dedupes() {
        let mut terminal = Instance::new("terminal", "/tmp/terminal");
        terminal.tool = "claude".to_string();

        let mut structured_claude = Instance::new("structured", "/tmp/structured");
        structured_claude.view = View::Structured;
        structured_claude.tool = "claude".to_string();

        let mut duplicate_claude = Instance::new("structured 2", "/tmp/structured-2");
        duplicate_claude.view = View::Structured;
        duplicate_claude.tool = "claude".to_string();

        let mut structured_opencode = Instance::new("opencode", "/tmp/opencode");
        structured_opencode.view = View::Structured;
        structured_opencode.tool = "opencode".to_string();

        let mut custom_agent = Instance::new("custom", "/tmp/custom");
        custom_agent.view = View::Structured;
        custom_agent.tool = "claude".to_string();
        custom_agent.agent_name = Some("custom-acp".to_string());

        let gates = gates_needed_by_instances(&[
            terminal,
            structured_claude,
            duplicate_claude,
            structured_opencode,
            custom_agent,
        ]);

        assert_eq!(gates.len(), 2);
        assert!(gates
            .iter()
            .any(|g| g.min_version == CLAUDE_AGENT_ACP_MIN_VERSION));
        assert!(gates.iter().any(|g| g.min_version == OPENCODE_MIN_VERSION));
    }
}
