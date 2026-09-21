//! Per-adapter compatibility policy for ACP agents.

use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::schema::ProtocolVersion;

use super::state::StartupErrorDetail;

/// Single source of truth for the `claude-agent-acp` minimum-version floor.
pub const CLAUDE_AGENT_ACP_MIN_VERSION: &str = "0.55.0";
pub const CLAUDE_AGENT_ACP_STEERING_MIN_VERSION: &str = "0.64.0";
/// Single source of truth for the `opencode` minimum-version floor.
pub const OPENCODE_MIN_VERSION: &str = "1.16.0";

/// Parse one of the floor constants above.
fn floor(version: &str) -> semver::Version {
    semver::Version::parse(version).expect("version floors must be valid semver")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionGate {
    pub expected: ExpectedAgent,
    pub binary: &'static str,
    pub package_name: &'static str,
    pub min_version: &'static str,
    pub install_command: &'static str,
    pub auto_install: bool,
}

/// The adapter aoe is trying to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpectedAgent {
    ClaudeAgentAcp,
    CodexAcp,
    OpenCode,
    AoeAgent,
    Gemini,
    PiAcp,
    /// Unknown / user-configured agent.
    Other,
}

impl ExpectedAgent {
    /// Resolve from the binary name as configured in `AgentRegistry`.
    pub fn from_command(command: &str) -> Self {
        // Scan every whitespace-separated token.
        command
            .split_whitespace()
            .find_map(|token| {
                let basename = token.rsplit(['/', '\\']).next().unwrap_or(token);
                let stem = basename
                    .strip_suffix(".exe")
                    .or_else(|| basename.strip_suffix(".cmd"))
                    .or_else(|| basename.strip_suffix(".bat"))
                    .unwrap_or(basename);
                match stem {
                    "claude-agent-acp" => Some(Self::ClaudeAgentAcp),
                    "codex-acp" => Some(Self::CodexAcp),
                    "opencode" => Some(Self::OpenCode),
                    "aoe-agent" => Some(Self::AoeAgent),
                    "gemini" => Some(Self::Gemini),
                    "pi-acp" => Some(Self::PiAcp),
                    _ => None,
                }
            })
            .unwrap_or(Self::Other)
    }
}

/// What the policy requires from the adapter's `InitializeResponse`.
struct CompatibilityPolicy {
    /// If set, the adapter must report this exact `agent_info.name`.
    expected_name: Option<&'static str>,
    /// If set, the adapter's `agent_info.version` must parse as semver
    /// and be at least this value.
    min_version: Option<semver::Version>,
    /// The protocol version the client requested.
    required_protocol: ProtocolVersion,
    /// If `true`, missing `agent_info` or empty/unparseable version
    /// rejects.
    fail_on_missing_agent_info: bool,
}

impl ExpectedAgent {
    fn policy(self) -> CompatibilityPolicy {
        match self {
            Self::ClaudeAgentAcp => CompatibilityPolicy {
                expected_name: Some("@agentclientprotocol/claude-agent-acp"),
                min_version: Some(floor(CLAUDE_AGENT_ACP_MIN_VERSION)),
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: true,
            },
            Self::OpenCode => CompatibilityPolicy {
                expected_name: Some("OpenCode"),
                min_version: Some(floor(OPENCODE_MIN_VERSION)),
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: true,
            },
            Self::CodexAcp => CompatibilityPolicy {
                // The deprecated @zed-industries package still exposes the
                // same binary name, but lacks current Codex model metadata.
                expected_name: Some("@agentclientprotocol/codex-acp"),
                min_version: None,
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: false,
            },
            // Other adapters: protocol check only.
            Self::AoeAgent | Self::Gemini | Self::PiAcp | Self::Other => CompatibilityPolicy {
                expected_name: None,
                min_version: None,
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: false,
            },
        }
    }
}

/// Why aoe refuses a session after a successful `initialize`; the same
/// shape the structured view event carries.
pub type StartupError = StartupErrorDetail;

impl StartupErrorDetail {
    /// Short, machine-stable identifier used by tests, logs, and the
    /// frontend reducer's discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IncompatibleAgentVersion { .. } => "incompatible_agent_version",
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version",
            Self::MissingAgentInfo { .. } => "missing_agent_info",
            Self::MismatchedAgentName { .. } => "mismatched_agent_name",
            Self::UnparseableAgentVersion { .. } => "unparseable_agent_version",
        }
    }

    /// User-facing one-liner suitable for the legacy
    /// `AgentStartupError { message }` event channel.
    pub fn user_message(&self) -> String {
        match self {
            Self::IncompatibleAgentVersion {
                package_name,
                installed,
                required,
                install_command,
                ..
            } => format!(
                "{package_name} {installed} installed; aoe requires >={required}. Run: {install_command}",
            ),
            Self::MissingAgentInfo {
                expected_package,
                install_command,
                ..
            } => format!(
                "Adapter did not report its package version. aoe requires {expected_package} >={CLAUDE_AGENT_ACP_MIN_VERSION}. Run: {install_command}",
            ),
            Self::MismatchedAgentName {
                expected,
                received,
                install_command,
                ..
            } => format!(
                "Adapter reported package name `{received}` but aoe expected `{expected}`. Run: {install_command}",
            ),
            Self::UnparseableAgentVersion {
                package_name,
                raw_version,
                required,
                install_command,
                ..
            } => format!(
                "{package_name} reported version `{raw_version}` which is not valid semver. aoe requires >={required}. Run: {install_command}",
            ),
            Self::UnsupportedProtocolVersion { expected, received } => format!(
                "Adapter speaks ACP protocol {received}; aoe requires {expected}.",
            ),
        }
    }
}

impl From<&StartupErrorDetail> for StartupErrorDetail {
    fn from(err: &StartupErrorDetail) -> Self {
        err.clone()
    }
}

/// Validate an `InitializeResponse` against the policy for the adapter
/// aoe was launching.
pub fn validate(expected: ExpectedAgent, init: &InitializeResponse) -> Result<(), StartupError> {
    let policy = expected.policy();

    if init.protocol_version != policy.required_protocol {
        return Err(StartupError::UnsupportedProtocolVersion {
            expected: format!("{:?}", policy.required_protocol),
            received: format!("{:?}", init.protocol_version),
        });
    }

    // Fast path for adapters with no name/version requirement.
    if policy.min_version.is_none() && policy.expected_name.is_none() {
        return Ok(());
    }

    let install_command =
        install_command_for(expected).unwrap_or_else(|| "(see project docs)".to_string());
    let auto_install = auto_install_for(expected);

    let Some(info) = init.agent_info.as_ref() else {
        if policy.fail_on_missing_agent_info {
            return Err(StartupError::MissingAgentInfo {
                expected_package: policy.expected_name.unwrap_or("(unspecified)").to_string(),
                install_command,
                auto_install,
            });
        }
        return Ok(());
    };

    if let Some(expected_name) = policy.expected_name {
        if info.name != expected_name {
            return Err(StartupError::MismatchedAgentName {
                expected: expected_name.to_string(),
                received: info.name.clone(),
                install_command,
                auto_install,
            });
        }
    }

    if let Some(min) = policy.min_version {
        let raw = info.version.trim();
        if raw.is_empty() {
            if policy.fail_on_missing_agent_info {
                return Err(StartupError::MissingAgentInfo {
                    expected_package: policy
                        .expected_name
                        .unwrap_or(info.name.as_str())
                        .to_string(),
                    install_command,
                    auto_install,
                });
            }
            return Ok(());
        }
        let parsed = match semver::Version::parse(raw) {
            Ok(v) => v,
            Err(_) => {
                return Err(StartupError::UnparseableAgentVersion {
                    package_name: info.name.clone(),
                    raw_version: raw.to_string(),
                    required: min.to_string(),
                    install_command,
                    auto_install,
                });
            }
        };
        if parsed < min {
            return Err(StartupError::IncompatibleAgentVersion {
                package_name: info.name.clone(),
                installed: parsed.to_string(),
                required: min.to_string(),
                install_command,
                auto_install,
            });
        }
    }

    Ok(())
}

/// Whether AoE may steer this agent's running turn via the
/// `_session/steering` extension request.
pub fn supports_steering(expected: ExpectedAgent, init: &InitializeResponse) -> bool {
    let advertised = init
        .meta
        .as_ref()
        .and_then(|meta| meta.get("steering"))
        .and_then(|steering| steering.get("supported"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !advertised {
        return false;
    }
    if expected != ExpectedAgent::ClaudeAgentAcp {
        return true;
    }
    init.agent_info
        .as_ref()
        .and_then(|info| semver::Version::parse(info.version.trim()).ok())
        .is_some_and(|version| version >= floor(CLAUDE_AGENT_ACP_STEERING_MIN_VERSION))
}

/// The ACP binary name aoe expects for this agent, or `None` for agents
/// with no fixed binary (`AoeAgent`, `Other`).
fn binary_for(expected: ExpectedAgent) -> Option<&'static str> {
    Some(match expected {
        ExpectedAgent::ClaudeAgentAcp => "claude-agent-acp",
        ExpectedAgent::CodexAcp => "codex-acp",
        ExpectedAgent::OpenCode => "opencode",
        ExpectedAgent::Gemini => "gemini",
        ExpectedAgent::PiAcp => "pi-acp",
        ExpectedAgent::AoeAgent | ExpectedAgent::Other => return None,
    })
}

/// Lookup table for the install commands surfaced in startup errors.
fn install_command_for(expected: ExpectedAgent) -> Option<String> {
    let bin = binary_for(expected)?;
    crate::acp::install_hints::install_hint_for(bin).map(|s| s.to_string())
}

/// Whether the web "Update & restart" action can install this agent itself
/// via a plain `npm install -g`.
fn auto_install_for(expected: ExpectedAgent) -> bool {
    binary_for(expected)
        .and_then(crate::acp::install_hints::npm_package_for)
        .is_some()
}

pub fn version_gate_for(expected: ExpectedAgent) -> Option<VersionGate> {
    let (binary, package_name, min_version) = match expected {
        ExpectedAgent::ClaudeAgentAcp => (
            "claude-agent-acp",
            "@agentclientprotocol/claude-agent-acp",
            CLAUDE_AGENT_ACP_MIN_VERSION,
        ),
        ExpectedAgent::OpenCode => ("opencode", "OpenCode", OPENCODE_MIN_VERSION),
        _ => return None,
    };
    Some(VersionGate {
        expected,
        binary,
        package_name,
        min_version,
        install_command: crate::acp::install_hints::install_hint_for(binary)
            .unwrap_or("(see project docs)"),
        auto_install: crate::acp::install_hints::npm_package_for(binary).is_some(),
    })
}

pub fn version_gates() -> impl Iterator<Item = VersionGate> {
    [ExpectedAgent::ClaudeAgentAcp, ExpectedAgent::OpenCode]
        .into_iter()
        .filter_map(version_gate_for)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::Implementation;

    fn init(info: Option<(&str, &str)>) -> InitializeResponse {
        let init = InitializeResponse::new(ProtocolVersion::V1);
        match info {
            Some((name, version)) => init.agent_info(Implementation::new(name, version)),
            None => init,
        }
    }

    const CLAUDE: &str = "@agentclientprotocol/claude-agent-acp";

    #[test]
    fn validate_gates_name_and_version_per_agent() {
        use ExpectedAgent::*;
        let below_floor = format!("{CLAUDE_AGENT_ACP_MIN_VERSION}-alpha.1");
        // (agent, reported name and version, expected error kind or None for accepted)
        type Case<'a> = (ExpectedAgent, Option<(&'a str, &'a str)>, Option<&'a str>);
        let cases: Vec<Case> = vec![
            (
                ClaudeAgentAcp,
                Some((CLAUDE, "0.0.0")),
                Some("incompatible_agent_version"),
            ),
            (
                ClaudeAgentAcp,
                Some((CLAUDE, &below_floor)),
                Some("incompatible_agent_version"),
            ),
            (
                ClaudeAgentAcp,
                Some((CLAUDE, CLAUDE_AGENT_ACP_MIN_VERSION)),
                None,
            ),
            (ClaudeAgentAcp, Some((CLAUDE, "999.0.0")), None),
            (ClaudeAgentAcp, None, Some("missing_agent_info")),
            (
                ClaudeAgentAcp,
                Some((CLAUDE, "")),
                Some("missing_agent_info"),
            ),
            (
                ClaudeAgentAcp,
                Some((CLAUDE, "not-semver")),
                Some("unparseable_agent_version"),
            ),
            (
                ClaudeAgentAcp,
                Some(("some-other-package", "0.39.0")),
                Some("mismatched_agent_name"),
            ),
            (CodexAcp, None, None),
            (AoeAgent, None, None),
            (Other, None, None),
            (
                OpenCode,
                Some(("OpenCode", "1.15.13")),
                Some("incompatible_agent_version"),
            ),
            (OpenCode, Some(("OpenCode", OPENCODE_MIN_VERSION)), None),
            (OpenCode, Some(("OpenCode", "1.17.9")), None),
            (OpenCode, None, Some("missing_agent_info")),
            (
                OpenCode,
                Some(("opencode", OPENCODE_MIN_VERSION)),
                Some("mismatched_agent_name"),
            ),
            (
                CodexAcp,
                Some(("@agentclientprotocol/codex-acp", "0.0.1")),
                None,
            ),
            (
                CodexAcp,
                Some(("codex-acp", "0.16.0")),
                Some("mismatched_agent_name"),
            ),
        ];
        for (agent, info, want) in cases {
            let got = validate(agent, &init(info)).err();
            assert_eq!(got.as_ref().map(|e| e.kind()), want, "{agent:?} {info:?}");
        }

        let Err(StartupError::IncompatibleAgentVersion {
            installed,
            required,
            auto_install,
            ..
        }) = validate(ClaudeAgentAcp, &init(Some((CLAUDE, "0.0.0"))))
        else {
            panic!("expected an incompatible version");
        };
        assert_eq!(
            (installed.as_str(), required.as_str()),
            ("0.0.0", CLAUDE_AGENT_ACP_MIN_VERSION)
        );
        assert!(auto_install);

        let legacy_codex = validate(CodexAcp, &init(Some(("codex-acp", "0.16.0")))).unwrap_err();
        assert!(legacy_codex
            .user_message()
            .contains("npm install -g @agentclientprotocol/codex-acp@latest"));
    }

    #[test]
    fn install_metadata_matches_each_agent() {
        use ExpectedAgent::*;
        for (agent, auto) in [
            (ClaudeAgentAcp, true),
            (CodexAcp, true),
            (Gemini, true),
            (OpenCode, false),
            (PiAcp, false),
            (AoeAgent, false),
            (Other, false),
        ] {
            assert_eq!(auto_install_for(agent), auto, "{agent:?}");
        }

        let claude = version_gate_for(ClaudeAgentAcp).unwrap();
        assert_eq!(
            (
                claude.binary,
                claude.package_name,
                claude.min_version,
                claude.auto_install
            ),
            (
                "claude-agent-acp",
                CLAUDE,
                CLAUDE_AGENT_ACP_MIN_VERSION,
                true
            )
        );
        let opencode = version_gate_for(OpenCode).unwrap();
        assert_eq!(
            (
                opencode.binary,
                opencode.package_name,
                opencode.min_version,
                opencode.auto_install
            ),
            ("opencode", "OpenCode", OPENCODE_MIN_VERSION, false)
        );
        assert!(version_gate_for(CodexAcp).is_none() && version_gate_for(Other).is_none());

        let dockerfile = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/docker/Dockerfile"));
        let needle = "@agentclientprotocol/claude-agent-acp@^";
        let pins: Vec<String> = dockerfile
            .match_indices(needle)
            .map(|(idx, _)| {
                dockerfile[idx + needle.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect()
            })
            .collect();
        assert_eq!(
            pins,
            [CLAUDE_AGENT_ACP_MIN_VERSION],
            "docker/Dockerfile claude-agent-acp pin must match CLAUDE_AGENT_ACP_MIN_VERSION",
        );
    }

    #[test]
    fn steering_gate_requires_advert_and_floor() {
        use ExpectedAgent::*;
        assert!(
            floor(CLAUDE_AGENT_ACP_STEERING_MIN_VERSION) >= floor(CLAUDE_AGENT_ACP_MIN_VERSION),
            "the steering floor must not sit below the startup floor",
        );
        // (agent, advertised bit, version, expected)
        let cases = [
            (
                ClaudeAgentAcp,
                Some(true),
                CLAUDE_AGENT_ACP_STEERING_MIN_VERSION,
                true,
            ),
            (ClaudeAgentAcp, Some(true), "999.0.0", true),
            // Advertised but pre-opt-in: the case the floor exists for.
            (ClaudeAgentAcp, Some(true), "0.63.9", false),
            (ClaudeAgentAcp, Some(true), "0.64.0-alpha.1", false),
            (ClaudeAgentAcp, Some(false), "999.0.0", false),
            (ClaudeAgentAcp, None, "999.0.0", false),
            (ClaudeAgentAcp, Some(true), "nightly", false),
            // Other adapters have no floor, only the bit.
            (CodexAcp, Some(true), "0.0.1", true),
        ];
        for (agent, advertised, version, expected) in cases {
            let mut init = init(Some((CLAUDE, version)));
            if let Some(supported) = advertised {
                init = init.meta(
                    serde_json::json!({ "steering": { "supported": supported } })
                        .as_object()
                        .unwrap()
                        .clone(),
                );
            }
            assert_eq!(
                supports_steering(agent, &init),
                expected,
                "{agent:?} advertised={advertised:?} version={version}"
            );
        }
    }

    #[test]
    fn from_command_finds_the_adapter_binary_in_any_launch_shape() {
        for command in [
            "claude-agent-acp",
            "/usr/local/bin/claude-agent-acp",
            "C:\\Users\\u\\AppData\\Roaming\\npm\\claude-agent-acp.cmd",
            "claude-agent-acp.exe",
            "D:\\bin\\claude-agent-acp.bat",
            "claude-agent-acp --some-flag",
            "  /usr/local/bin/claude-agent-acp  ",
            "bash claude-agent-acp",
            "env FOO=bar /usr/local/bin/claude-agent-acp",
        ] {
            assert_eq!(
                ExpectedAgent::from_command(command),
                ExpectedAgent::ClaudeAgentAcp,
                "{command:?}"
            );
        }
        assert_eq!(
            ExpectedAgent::from_command("unknown-bin"),
            ExpectedAgent::Other
        );
    }
}
