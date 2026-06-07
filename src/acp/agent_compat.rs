//! Compatibility policy for OMP ACP.

use agent_client_protocol::schema::{InitializeResponse, ProtocolVersion};

use super::state::StartupErrorDetail;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedAgent {
    Omp,
    Other,
}

impl ExpectedAgent {
    pub fn from_command(command: &str) -> Self {
        command
            .split_whitespace()
            .find_map(|token| {
                let basename = token.rsplit(['/', '\\']).next().unwrap_or(token);
                let stem = basename
                    .strip_suffix(".exe")
                    .or_else(|| basename.strip_suffix(".cmd"))
                    .or_else(|| basename.strip_suffix(".bat"))
                    .unwrap_or(basename);
                (stem == "omp").then_some(Self::Omp)
            })
            .unwrap_or(Self::Other)
    }
}

struct CompatibilityPolicy {
    expected_name: Option<&'static str>,
    min_version: Option<semver::Version>,
    required_protocol: ProtocolVersion,
    fail_on_missing_agent_info: bool,
}

impl ExpectedAgent {
    fn policy(self) -> CompatibilityPolicy {
        match self {
            Self::Omp | Self::Other => CompatibilityPolicy {
                expected_name: None,
                min_version: None,
                required_protocol: ProtocolVersion::V1,
                fail_on_missing_agent_info: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupError {
    IncompatibleAgentVersion {
        package_name: String,
        installed: String,
        required: String,
        install_command: String,
    },
    MissingAgentInfo {
        expected_package: String,
        install_command: String,
    },
    UnparseableAgentVersion {
        package_name: String,
        raw_version: String,
        required: String,
        install_command: String,
    },
    MismatchedAgentName {
        expected: String,
        received: String,
        install_command: String,
    },
    UnsupportedProtocolVersion {
        expected: ProtocolVersion,
        received: ProtocolVersion,
    },
}

impl StartupError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IncompatibleAgentVersion { .. } => "incompatible_agent_version",
            Self::MissingAgentInfo { .. } => "missing_agent_info",
            Self::UnparseableAgentVersion { .. } => "unparseable_agent_version",
            Self::MismatchedAgentName { .. } => "mismatched_agent_name",
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version",
        }
    }

    pub fn user_message(&self) -> String {
        match self {
            Self::IncompatibleAgentVersion {
                package_name,
                installed,
                required,
                ..
            } => format!("{package_name} {installed} is too old; install {required} or newer."),
            Self::MissingAgentInfo {
                expected_package, ..
            } => {
                format!("{expected_package} did not report ACP agent info.")
            }
            Self::UnparseableAgentVersion {
                package_name,
                raw_version,
                ..
            } => format!("{package_name} reported an invalid version: {raw_version}."),
            Self::MismatchedAgentName {
                expected, received, ..
            } => format!("ACP agent mismatch: expected {expected}, received {received}."),
            Self::UnsupportedProtocolVersion { expected, received } => {
                format!("ACP protocol mismatch: expected {expected:?}, received {received:?}.")
            }
        }
    }
}

impl From<&StartupError> for StartupErrorDetail {
    fn from(value: &StartupError) -> Self {
        match value {
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
            StartupError::MismatchedAgentName {
                expected,
                received,
                install_command,
            } => StartupErrorDetail::MismatchedAgentName {
                expected: expected.clone(),
                received: received.clone(),
                install_command: install_command.clone(),
            },
            StartupError::UnsupportedProtocolVersion { expected, received } => {
                StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: format!("{expected:?}"),
                    received: format!("{received:?}"),
                }
            }
        }
    }
}

impl From<StartupError> for StartupErrorDetail {
    fn from(value: StartupError) -> Self {
        StartupErrorDetail::from(&value)
    }
}

pub fn validate(expected: ExpectedAgent, init: &InitializeResponse) -> Result<(), StartupError> {
    let policy = expected.policy();

    if init.protocol_version != policy.required_protocol {
        return Err(StartupError::UnsupportedProtocolVersion {
            expected: policy.required_protocol.clone(),
            received: init.protocol_version.clone(),
        });
    }

    let Some(info) = init.agent_info.as_ref() else {
        return if policy.fail_on_missing_agent_info {
            Err(StartupError::MissingAgentInfo {
                expected_package: expected_package_for(expected),
                install_command: install_command_for(expected),
            })
        } else {
            Ok(())
        };
    };

    if let Some(expected_name) = policy.expected_name {
        if info.name != expected_name {
            return Err(StartupError::MismatchedAgentName {
                expected: expected_name.to_string(),
                received: info.name.clone(),
                install_command: install_command_for(expected),
            });
        }
    }

    if let Some(min) = policy.min_version {
        if info.version.trim().is_empty() {
            return Err(StartupError::MissingAgentInfo {
                expected_package: expected_package_for(expected),
                install_command: install_command_for(expected),
            });
        }
        let parsed = semver::Version::parse(&info.version).map_err(|_| {
            StartupError::UnparseableAgentVersion {
                package_name: info.name.clone(),
                raw_version: info.version.clone(),
                required: min.to_string(),
                install_command: install_command_for(expected),
            }
        })?;
        if parsed < min {
            return Err(StartupError::IncompatibleAgentVersion {
                package_name: info.name.clone(),
                installed: parsed.to_string(),
                required: min.to_string(),
                install_command: install_command_for(expected),
            });
        }
    }

    Ok(())
}

fn install_command_for(expected: ExpectedAgent) -> String {
    match expected {
        ExpectedAgent::Omp => crate::acp::install_hints::install_hint_for("omp")
            .unwrap_or("install OMP")
            .to_string(),
        ExpectedAgent::Other => String::new(),
    }
}

fn expected_package_for(expected: ExpectedAgent) -> String {
    match expected {
        ExpectedAgent::Omp => "omp".to_string(),
        ExpectedAgent::Other => "ACP agent".to_string(),
    }
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
    fn omp_is_permissive_on_missing_info() {
        validate(ExpectedAgent::Omp, &make_init_no_info()).unwrap();
    }

    #[test]
    fn omp_is_permissive_on_old_version() {
        validate(ExpectedAgent::Omp, &make_init("omp", "0.0.1")).unwrap();
    }

    #[test]
    fn from_command_recognises_omp() {
        assert_eq!(
            ExpectedAgent::from_command("/usr/local/bin/omp"),
            ExpectedAgent::Omp
        );
        assert_eq!(ExpectedAgent::from_command("omp acp"), ExpectedAgent::Omp);
        assert_eq!(
            ExpectedAgent::from_command("unknown-bin"),
            ExpectedAgent::Other
        );
    }

    #[test]
    fn from_command_handles_windows_paths_and_extensions() {
        assert_eq!(
            ExpectedAgent::from_command("C:\\Users\\u\\AppData\\Roaming\\npm\\omp.cmd"),
            ExpectedAgent::Omp
        );
        assert_eq!(ExpectedAgent::from_command("omp.exe"), ExpectedAgent::Omp);
        assert_eq!(
            ExpectedAgent::from_command("D:\\bin\\omp.bat"),
            ExpectedAgent::Omp
        );
    }

    #[test]
    fn from_command_handles_wrapper_token_prefix() {
        assert_eq!(ExpectedAgent::from_command("bash omp"), ExpectedAgent::Omp);
        assert_eq!(
            ExpectedAgent::from_command("env FOO=bar /usr/local/bin/omp"),
            ExpectedAgent::Omp
        );
    }
}
