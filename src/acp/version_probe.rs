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
    Version {
        raw: String,
        parsed: Version,
        /// stdout only, what the spawn-side tokenizer sees; stderr is
        /// folded into `raw`.
        stdout_raw: String,
    },
    Unparseable {
        raw: String,
    },
    Failed {
        message: String,
    },
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

/// The spawn side's tokenizer (`path_copy_below_floor`): the first
/// whitespace-delimited token that parses as strict semver once a
/// leading `v` is stripped.
pub fn whitespace_token_below_floor(raw: &str, min: Version) -> bool {
    whitespace_token_semver(raw).is_some_and(|found| found < min)
}

pub fn whitespace_token_semver(raw: &str) -> Option<Version> {
    raw.split_whitespace()
        .filter_map(|tok| Version::parse(tok.trim_start_matches('v')).ok())
        .next()
}

pub async fn probe_binary_version(binary: &str) -> ProbeStatus {
    match which::which(binary) {
        Ok(path) => probe_path_version(&path).await,
        Err(_) => ProbeStatus::Missing,
    }
}

/// Probe an explicit executable path.
pub async fn probe_path_version(path: &std::path::Path) -> ProbeStatus {
    let child = tokio::process::Command::new(path)
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
            let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
            let raw = format!("{}{}", stdout_raw, String::from_utf8_lossy(&output.stderr),)
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
                Some(parsed) => ProbeStatus::Version {
                    raw,
                    parsed,
                    stdout_raw,
                },
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
    // Only host-run structured sessions gate on the host toolchain.
    for inst in instances
        .iter()
        .filter(|inst| inst.is_structured() && !inst.is_sandboxed())
    {
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

    /// A `ProbeStatus::Version` as the probe builds it from one raw line.
    fn probed(raw: &str) -> ProbeStatus {
        ProbeStatus::Version {
            raw: raw.to_string(),
            parsed: Version::parse(raw).unwrap(),
            stdout_raw: raw.to_string(),
        }
    }

    fn structured(name: &str, tool: &str) -> Instance {
        let mut instance = Instance::new(name, &format!("/tmp/{name}"));
        instance.view = View::Structured;
        instance.tool = tool.to_string();
        instance
    }

    #[test]
    fn version_tokens_parse_like_spawn() {
        // (raw output, parsed version)
        let cases = [
            ("0.55.0", Some("0.55.0")),
            ("claude-agent-acp 0.55.0", Some("0.55.0")),
            ("v1.16.0", Some("1.16.0")),
            ("version=0.55.0-alpha.1", Some("0.55.0-alpha.1")),
            ("not-semver", None),
        ];
        for (raw, want) in cases {
            let got = extract_semver(raw).map(|v| v.to_string());
            assert_eq!(got.as_deref(), want, "{raw:?}");
        }

        // `whitespace_token_below_floor` mirrors spawn parsing.
        let min = Version::parse(CLAUDE_AGENT_ACP_MIN_VERSION).unwrap();
        // (raw, below_floor)
        let cases = [
            ("0.37.0", true),
            ("claude-agent-acp 0.37.0", true),
            ("v0.37.0", true),
            ("0.55.0", false),
            ("0.56.0", false),
            ("version=0.37.0", false),
            ("0.37.0-beta.1", true),
            ("junk", false),
            ("", false),
        ];
        for (raw, below) in cases {
            assert_eq!(
                whitespace_token_below_floor(raw, min.clone()),
                below,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn warning_for_probe_flags_only_unusable_adapters() {
        // (probe outcome, warning kind it raises)
        let cases = [
            (probed("0.0.1"), Some("below")),
            (probed(CLAUDE_AGENT_ACP_MIN_VERSION), None),
            (probed("999.0.0"), None),
            (ProbeStatus::Missing, Some("missing")),
            (
                ProbeStatus::Unparseable {
                    raw: "weird".to_string(),
                },
                Some("unparseable"),
            ),
            (ProbeStatus::TimedOut, Some("timed_out")),
        ];
        for (status, want) in cases {
            let got = warning_for_probe(claude_gate(), &status).map(|w| match w.kind {
                VersionWarningKind::BelowMinimum { .. } => "below",
                VersionWarningKind::Missing => "missing",
                VersionWarningKind::Unparseable { .. } => "unparseable",
                VersionWarningKind::Failed { .. } => "failed",
                VersionWarningKind::TimedOut => "timed_out",
            });
            assert_eq!(got, want, "{status:?}");
        }
    }

    #[test]
    fn gates_needed_by_instances_scopes_to_host_structured_sessions_and_dedupes() {
        let mut terminal = Instance::new("terminal", "/tmp/terminal");
        terminal.tool = "claude".to_string();
        let mut custom_agent = structured("custom", "claude");
        custom_agent.agent_name = Some("custom-acp".to_string());
        let mut sandboxed = structured("sandboxed", "opencode");
        sandboxed.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "ghcr.io/agent-of-empires/aoe-sandbox:latest".to_string(),
            container_name: "aoe-sandbox-sandboxe".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        assert!(gates_needed_by_instances(&[sandboxed.clone()]).is_empty());

        let gates = gates_needed_by_instances(&[
            sandboxed,
            terminal,
            structured("structured", "claude"),
            structured("structured-2", "claude"),
            structured("opencode", "opencode"),
            custom_agent,
        ]);

        assert_eq!(gates.len(), 2);
        assert!(gates
            .iter()
            .any(|g| g.min_version == CLAUDE_AGENT_ACP_MIN_VERSION));
        assert!(gates.iter().any(|g| g.min_version == OPENCODE_MIN_VERSION));
    }
}
