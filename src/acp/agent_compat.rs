//! Per-adapter compatibility policy for ACP agents.
//!
//! aoe spawns several ACP adapters (claude-agent-acp, codex-acp,
//! opencode, aoe-agent, gemini, pi-acp). Some require a minimum upstream
//! version because aoe relies on behavior that only landed past a known
//! release. This module centralizes those rules so `acp_client` has a
//! single hook to call after `initialize` succeeds, instead of scattering
//! ad-hoc semver checks at every spawn site.
//!
//! Today only `ClaudeAgentAcp` carries a minimum version (see
//! `CLAUDE_AGENT_ACP_MIN_VERSION`,
//! required for `memory_recall` tool-call emission, native `cancelled`
//! stop reason, force-cancel of a wedged `TaskOutput` block (upstream
//! #680), the upstream #641 fix, the `fable` model, and several other
//! behaviors aoe builds on). Other agents
//! get a permissive policy (protocol check only). Long-term aoe should
//! prefer ACP capability flags over package-version gating; until upstream
//! exposes those, package versions are the only precise contract.
//!
//! Failure mode: missing `agent_info`, missing version, parse failure, or
//! version below the floor all reject for adapters with a minimum. Other
//! adapters are passed through. The supervisor's known spawn intent is
//! the gate, not the self-reported `agent_info.name` on the wire.

use agent_client_protocol::schema::{InitializeResponse, ProtocolVersion};

use super::state::StartupErrorDetail;

/// Single source of truth for the `claude-agent-acp` minimum-version floor.
///
/// Bumping the floor is a one-line edit here: the gate, the startup-error
/// strings, and the boundary tests all derive from this value. The one
/// peer that cannot read a Rust const, the npm pin in `docker/Dockerfile`,
/// is held in sync by the `dockerfile_pin_matches_floor` test below, so a
/// bump that forgets the Dockerfile fails CI rather than shipping a
/// sandbox image stuck below the host floor. User docs deliberately do not
/// restate the number; the startup-error path reports the exact floor
/// dynamically at rejection time.
pub const CLAUDE_AGENT_ACP_MIN_VERSION: &str = "0.44.0";

/// Parsed form of [`CLAUDE_AGENT_ACP_MIN_VERSION`]. Runs once per adapter
/// initialize, not in a hot path, so parsing on demand is fine.
fn claude_agent_acp_min_version() -> semver::Version {
    semver::Version::parse(CLAUDE_AGENT_ACP_MIN_VERSION)
        .expect("CLAUDE_AGENT_ACP_MIN_VERSION must be valid semver")
}

/// The adapter aoe is trying to launch. Drives which `CompatibilityPolicy`
/// is applied at initialize-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedAgent {
    ClaudeAgentAcp,
    CodexAcp,
    OpenCode,
    AoeAgent,
    Gemini,
    PiAcp,
    /// Unknown / user-configured agent. Permissive policy.
    Other,
}

impl ExpectedAgent {
    /// Resolve from the binary name as configured in `AgentRegistry`. Maps
    /// the string the supervisor is about to spawn to the matching policy
    /// enum so the supervisor can call this once and pass the result down.
    ///
    /// Robust to surface variations seen in the wild: a wrapper command
    /// string ("bash claude-agent-acp"), POSIX or Windows path
    /// separators, and the `.exe` / `.cmd` suffixes Windows shims add.
    /// Without this normalization a wrapped or Windows-installed
    /// `claude-agent-acp` would land in `Other` and silently bypass the
    /// minimum-version gate.
    pub fn from_command(command: &str) -> Self {
        // Scan every whitespace-separated token. The actual binary can
        // be the first token (`/usr/local/bin/claude-agent-acp`) but
        // also a later one when a wrapper prefixes the command line
        // (`bash claude-agent-acp`, `env -u FOO claude-agent-acp`,
        // `npx claude-agent-acp`, etc.). Match against the first token
        // that classifies as a known adapter so wrappers don't bypass
        // the minimum-version gate.
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
    /// The protocol version the client requested. Adapter must match.
    required_protocol: ProtocolVersion,
    /// If `true`, missing `agent_info` or empty/unparseable version
    /// rejects. `false` means we tolerate adapters that don't advertise.
    fail_on_missing_agent_info: bool,
}

impl ExpectedAgent {
    fn policy(self) -> CompatibilityPolicy {
        match self {
            Self::ClaudeAgentAcp => CompatibilityPolicy {
                expected_name: Some("@agentclientprotocol/claude-agent-acp"),
                min_version: Some(claude_agent_acp_min_version()),
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: true,
            },
            // Other adapters: protocol check only. aoe doesn't yet
            // depend on a version-gated behavior in any of them.
            Self::CodexAcp
            | Self::OpenCode
            | Self::AoeAgent
            | Self::Gemini
            | Self::PiAcp
            | Self::Other => CompatibilityPolicy {
                expected_name: None,
                min_version: None,
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: false,
            },
        }
    }
}

/// Reasons aoe refuses to enter a session after a successful `initialize`
/// handshake. Surfaced to the user via the structured view StartupErrorScreen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupError {
    /// Adapter reported a version below the minimum aoe requires.
    IncompatibleAgentVersion {
        package_name: String,
        installed: String,
        required: String,
        install_command: String,
    },
    /// Adapter passed name/version checks but reported a protocol version
    /// aoe does not speak.
    UnsupportedProtocolVersion { expected: String, received: String },
    /// Adapter omitted `agent_info` (or `agent_info.version`) entirely,
    /// and the policy for this adapter kind requires it.
    MissingAgentInfo {
        expected_package: String,
        install_command: String,
    },
    /// Adapter advertised a different package name than aoe expected for
    /// this `ExpectedAgent`. Probably a wrapper or stale install.
    MismatchedAgentName {
        expected: String,
        received: String,
        install_command: String,
    },
    /// `agent_info.version` was present but did not parse as semver.
    UnparseableAgentVersion {
        package_name: String,
        raw_version: String,
        required: String,
        install_command: String,
    },
}

impl StartupError {
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
    /// `AgentStartupError { message }` event channel. Lets the existing
    /// status-derivation paths flip the session into Error state without
    /// having to teach every consumer about the structured variant.
    pub fn user_message(&self) -> String {
        match self {
            Self::IncompatibleAgentVersion {
                package_name,
                installed,
                required,
                install_command,
            } => format!(
                "{package_name} {installed} installed; aoe requires >={required}. Run: {install_command}",
            ),
            Self::MissingAgentInfo {
                expected_package,
                install_command,
            } => format!(
                "Adapter did not report its package version. aoe requires {expected_package} >={CLAUDE_AGENT_ACP_MIN_VERSION}. Run: {install_command}",
            ),
            Self::MismatchedAgentName {
                expected,
                received,
                install_command,
            } => format!(
                "Adapter reported package name `{received}` but aoe expected `{expected}`. Run: {install_command}",
            ),
            Self::UnparseableAgentVersion {
                package_name,
                raw_version,
                required,
                install_command,
            } => format!(
                "{package_name} reported version `{raw_version}` which is not valid semver. aoe requires >={required}. Run: {install_command}",
            ),
            Self::UnsupportedProtocolVersion { expected, received } => format!(
                "Adapter speaks ACP protocol {received}; aoe requires {expected}.",
            ),
        }
    }
}

impl From<&StartupError> for StartupErrorDetail {
    fn from(err: &StartupError) -> Self {
        match err {
            StartupError::IncompatibleAgentVersion {
                package_name,
                installed,
                required,
                install_command,
            } => StartupErrorDetail::IncompatibleAgentVersion {
                package_name: package_name.clone(),
                installed: installed.clone(),
                required: required.clone(),
                install_command: install_command.clone(),
            },
            StartupError::MissingAgentInfo {
                expected_package,
                install_command,
            } => StartupErrorDetail::MissingAgentInfo {
                expected_package: expected_package.clone(),
                install_command: install_command.clone(),
            },
            StartupError::MismatchedAgentName {
                expected,
                received,
                install_command,
            } => StartupErrorDetail::MismatchedAgentName {
                expected: expected.clone(),
                received: received.clone(),
                install_command: install_command.clone(),
            },
            StartupError::UnparseableAgentVersion {
                package_name,
                raw_version,
                required,
                install_command,
            } => StartupErrorDetail::UnparseableAgentVersion {
                package_name: package_name.clone(),
                raw_version: raw_version.clone(),
                required: required.clone(),
                install_command: install_command.clone(),
            },
            StartupError::UnsupportedProtocolVersion { expected, received } => {
                StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: expected.clone(),
                    received: received.clone(),
                }
            }
        }
    }
}

/// Validate an `InitializeResponse` against the policy for the adapter
/// aoe was launching. Returns `Ok(())` on success; `Err(StartupError)`
/// surfaces a structured failure the supervisor can route into the
/// startup-error UI path.
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

    let Some(info) = init.agent_info.as_ref() else {
        if policy.fail_on_missing_agent_info {
            return Err(StartupError::MissingAgentInfo {
                expected_package: policy.expected_name.unwrap_or("(unspecified)").to_string(),
                install_command,
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
                });
            }
        };
        if parsed < min {
            return Err(StartupError::IncompatibleAgentVersion {
                package_name: info.name.clone(),
                installed: parsed.to_string(),
                required: min.to_string(),
                install_command,
            });
        }
    }

    Ok(())
}

/// Lookup table for the install commands surfaced in startup errors.
/// Kept in sync with `install_hints::install_hint_for` so the error UI
/// shows the exact command the doctor would run.
fn install_command_for(expected: ExpectedAgent) -> Option<String> {
    let bin = match expected {
        ExpectedAgent::ClaudeAgentAcp => "claude-agent-acp",
        ExpectedAgent::CodexAcp => "codex-acp",
        ExpectedAgent::OpenCode => "opencode",
        ExpectedAgent::AoeAgent => return None,
        ExpectedAgent::Gemini => "gemini",
        ExpectedAgent::PiAcp => "pi-acp",
        ExpectedAgent::Other => return None,
    };
    crate::acp::install_hints::install_hint_for(bin).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::Implementation;

    fn make_init(name: &str, version: &str) -> InitializeResponse {
        InitializeResponse::new(ProtocolVersion::V1).agent_info(Implementation::new(name, version))
    }

    fn make_init_no_info() -> InitializeResponse {
        InitializeResponse::new(ProtocolVersion::V1)
    }

    #[test]
    fn claude_below_minimum_rejected() {
        let init = make_init("@agentclientprotocol/claude-agent-acp", "0.0.0");
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "incompatible_agent_version");
        let StartupError::IncompatibleAgentVersion {
            installed,
            required,
            ..
        } = err
        else {
            panic!()
        };
        assert_eq!(installed, "0.0.0");
        assert_eq!(required, CLAUDE_AGENT_ACP_MIN_VERSION);
    }

    #[test]
    fn claude_just_below_floor_rejected() {
        // The strict lower boundary, derived from the floor so a bump
        // never needs to touch this fixture: a prerelease of the floor
        // sorts strictly below the release under semver, so it must be
        // rejected. Guards an accidental `<=` slip in the gate.
        let version = format!("{CLAUDE_AGENT_ACP_MIN_VERSION}-alpha.1");
        let init = make_init("@agentclientprotocol/claude-agent-acp", &version);
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "incompatible_agent_version");
    }

    #[test]
    fn claude_at_minimum_accepted() {
        let init = make_init(
            "@agentclientprotocol/claude-agent-acp",
            CLAUDE_AGENT_ACP_MIN_VERSION,
        );
        validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap();
    }

    #[test]
    fn claude_above_minimum_accepted() {
        let init = make_init("@agentclientprotocol/claude-agent-acp", "999.0.0");
        validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap();
    }

    #[test]
    fn dockerfile_pin_matches_floor() {
        // docker/Dockerfile cannot read a Rust const, so the sandbox npm
        // pin is the one floor restatement outside this module. Assert it
        // tracks the gate so a bump that forgets the Dockerfile fails CI
        // instead of shipping an image stuck below the host floor.
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
            vec![CLAUDE_AGENT_ACP_MIN_VERSION.to_string()],
            "docker/Dockerfile claude-agent-acp pin must match CLAUDE_AGENT_ACP_MIN_VERSION",
        );
    }

    #[test]
    fn claude_missing_agent_info_rejected() {
        let init = make_init_no_info();
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "missing_agent_info");
    }

    #[test]
    fn claude_empty_version_rejected() {
        let init = make_init("@agentclientprotocol/claude-agent-acp", "");
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "missing_agent_info");
    }

    #[test]
    fn claude_unparseable_version_rejected() {
        let init = make_init("@agentclientprotocol/claude-agent-acp", "not-semver");
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "unparseable_agent_version");
    }

    #[test]
    fn claude_mismatched_name_rejected() {
        let init = make_init("some-other-package", "0.39.0");
        let err = validate(ExpectedAgent::ClaudeAgentAcp, &init).unwrap_err();
        assert_eq!(err.kind(), "mismatched_agent_name");
    }

    #[test]
    fn non_claude_permissive_on_missing_info() {
        let init = make_init_no_info();
        validate(ExpectedAgent::CodexAcp, &init).unwrap();
        validate(ExpectedAgent::OpenCode, &init).unwrap();
        validate(ExpectedAgent::AoeAgent, &init).unwrap();
        validate(ExpectedAgent::Other, &init).unwrap();
    }

    #[test]
    fn non_claude_permissive_on_old_version() {
        let init = make_init("@zed-industries/codex-acp", "0.0.1");
        validate(ExpectedAgent::CodexAcp, &init).unwrap();
    }

    #[test]
    fn from_command_recognises_path_prefixed_binary() {
        assert_eq!(
            ExpectedAgent::from_command("/usr/local/bin/claude-agent-acp"),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("claude-agent-acp"),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("unknown-bin"),
            ExpectedAgent::Other
        );
    }

    #[test]
    fn from_command_handles_windows_paths_and_extensions() {
        assert_eq!(
            ExpectedAgent::from_command(
                "C:\\Users\\u\\AppData\\Roaming\\npm\\claude-agent-acp.cmd"
            ),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("claude-agent-acp.exe"),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("D:\\bin\\claude-agent-acp.bat"),
            ExpectedAgent::ClaudeAgentAcp
        );
    }

    #[test]
    fn from_command_handles_wrapper_token_prefix() {
        // `AgentRegistry` exposes `command` plus a separate `args`
        // vector, but defensive code paths sometimes hand us the joined
        // string (e.g. `aoe acp doctor --json` output, log lines
        // round-tripped through user config). Tolerate the joined form
        // so the gate doesn't silently flip to `Other` and skip the
        // claude check. Wrapper-prefixed commands count too: the
        // classifier scans every whitespace-separated token so
        // `bash claude-agent-acp` or `npx claude-agent-acp` is gated
        // on the wrapped binary, not on the wrapper.
        assert_eq!(
            ExpectedAgent::from_command("claude-agent-acp --some-flag"),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("  /usr/local/bin/claude-agent-acp  "),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("bash claude-agent-acp"),
            ExpectedAgent::ClaudeAgentAcp
        );
        assert_eq!(
            ExpectedAgent::from_command("env FOO=bar /usr/local/bin/claude-agent-acp"),
            ExpectedAgent::ClaudeAgentAcp
        );
        // `npx`-style npm-package-spec invocations are out of scope:
        // npx itself resolves the package and the spawned binary's
        // argv[0] is `claude-agent-acp`, not `claude-agent-acp@0.39.0`.
        // The version-suffixed form would only show up if a user wired
        // it manually into AgentSpec.command, which is not a supported
        // configuration today.
    }
}
