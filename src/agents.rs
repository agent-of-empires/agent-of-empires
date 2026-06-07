//! OMP-only agent registry.

use crate::session::Status;
use crate::tmux::status_detection;

/// How to check whether an agent binary is installed on the host.
pub enum DetectionMethod {
    /// Run `which <binary>` and check exit code.
    Which(&'static str),
    /// Run `<binary> <arg>` and check that it doesn't error.
    RunWithArg(&'static str, &'static str),
}

/// How to enable YOLO / auto-approve mode for an agent.
pub enum YoloMode {
    /// Append a CLI flag.
    CliFlag(&'static str),
    /// Set an environment variable (name, value).
    EnvVar(&'static str, &'static str),
    /// Agent always runs in YOLO mode with no opt-in needed.
    AlwaysYolo,
}

/// How an agent resumes an existing session from the CLI.
pub enum ResumeStrategy {
    /// Append a flag. New and existing sessions use the same flag.
    Flag(&'static str),
    /// Use different flags for existing vs new conversation data.
    FlagPair {
        existing: &'static str,
        new_session: &'static str,
    },
    /// Resume is a subcommand rather than a flag.
    Subcommand(&'static str),
    /// Agent does not support session resume.
    Unsupported,
}

/// A single hook event that AoE registers in an agent's settings file.
pub struct HookEvent {
    pub name: &'static str,
    pub matcher: Option<&'static str>,
    pub status: Option<&'static str>,
    pub session_id_capture: bool,
}

/// Configuration for installing status-detection hooks into an agent's settings file.
pub struct AgentHookConfig {
    pub settings_rel_path: &'static str,
    pub events: &'static [HookEvent],
}

/// Everything we know about a single agent CLI.
pub struct AgentDef {
    pub name: &'static str,
    pub binary: &'static str,
    pub aliases: &'static [&'static str],
    pub detection: DetectionMethod,
    pub yolo: Option<YoloMode>,
    pub instruction_flag: Option<&'static str>,
    pub set_default_command: bool,
    pub detect_status: fn(&str) -> Status,
    pub container_env: &'static [(&'static str, &'static str)],
    pub hook_config: Option<AgentHookConfig>,
    pub resume_strategy: ResumeStrategy,
    pub host_only: bool,
    pub send_keys_enter_delay_ms: u64,
    pub install_hint: &'static str,
}

pub const AGENTS: &[AgentDef] = &[AgentDef {
    name: "omp",
    binary: "omp",
    aliases: &["oh-my-pi"],
    detection: DetectionMethod::Which("omp"),
    yolo: Some(YoloMode::CliFlag("--auto-approve")),
    instruction_flag: None,
    set_default_command: true,
    detect_status: status_detection::detect_omp_status,
    container_env: &[],
    hook_config: None,
    resume_strategy: ResumeStrategy::Flag("--resume"),
    host_only: false,
    send_keys_enter_delay_ms: 0,
    install_hint:
        "curl -fsSL https://raw.githubusercontent.com/nicepkg/oh-my-pi/main/scripts/install.sh | bash",
}];

/// Look up an agent by canonical name.
pub fn get_agent(name: &str) -> Option<&'static AgentDef> {
    AGENTS.iter().find(|a| a.name == name)
}

/// Returns the delay (in ms) to insert before the submit-Enter for this agent.
pub fn send_keys_enter_delay(tool: &str) -> u64 {
    get_agent(tool)
        .map(|a| a.send_keys_enter_delay_ms)
        .unwrap_or(0)
}

/// All canonical agent names in registry order.
pub fn agent_names() -> Vec<&'static str> {
    AGENTS.iter().map(|a| a.name).collect()
}

/// Given a command string, return the canonical agent name if one is recognised.
pub fn resolve_tool_name(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower.is_empty() {
        return Some("omp");
    }
    for agent in AGENTS {
        if cmd_lower.contains(agent.name) {
            return Some(agent.name);
        }
        for alias in agent.aliases {
            if cmd_lower.contains(alias) {
                return Some(agent.name);
            }
        }
    }
    None
}

/// Return the install hint for an agent, looked up by canonical name.
pub fn install_hint(name: &str) -> Option<&'static str> {
    get_agent(name).map(|a| a.install_hint)
}

/// Convert a tool name to a 1-based settings index (0 = Auto).
pub fn settings_index_from_name(name: Option<&str>) -> usize {
    match name {
        Some(n) => AGENTS
            .iter()
            .position(|a| a.name == n)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    }
}

/// Convert a 1-based settings index back to a tool name (0 = Auto/None).
pub fn name_from_settings_index(index: usize) -> Option<&'static str> {
    if index == 0 {
        None
    } else {
        AGENTS.get(index - 1).map(|a| a.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_agent_known() {
        assert_eq!(get_agent("omp").unwrap().binary, "omp");
    }

    #[test]
    fn test_omp_agent_definition() {
        let omp = get_agent("omp").unwrap();
        assert!(matches!(&omp.detection, DetectionMethod::Which("omp")));
        assert!(matches!(
            &omp.yolo,
            Some(YoloMode::CliFlag("--auto-approve"))
        ));
        assert!(matches!(
            &omp.resume_strategy,
            ResumeStrategy::Flag("--resume")
        ));
        assert!(omp.set_default_command);
        assert!(!omp.host_only);
        assert_eq!(omp.send_keys_enter_delay_ms, 0);
        assert_eq!(
            omp.install_hint,
            "curl -fsSL https://raw.githubusercontent.com/nicepkg/oh-my-pi/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn test_get_agent_unknown() {
        assert!(get_agent("claude").is_none());
        assert!(get_agent("unknown").is_none());
    }

    #[test]
    fn test_agent_names() {
        assert_eq!(agent_names(), vec!["omp"]);
    }

    #[test]
    fn test_resolve_tool_name() {
        assert_eq!(resolve_tool_name("omp"), Some("omp"));
        assert_eq!(resolve_tool_name("oh-my-pi"), Some("omp"));
        assert_eq!(resolve_tool_name(""), Some("omp"));
        assert_eq!(resolve_tool_name("claude"), None);
    }

    #[test]
    fn test_settings_index_roundtrip() {
        assert_eq!(settings_index_from_name(None), 0);
        assert_eq!(settings_index_from_name(Some("omp")), 1);
        assert_eq!(settings_index_from_name(Some("claude")), 0);
        assert_eq!(name_from_settings_index(0), None);
        assert_eq!(name_from_settings_index(1), Some("omp"));
        assert_eq!(name_from_settings_index(2), None);
    }

    #[test]
    fn test_all_agents_have_yolo_support() {
        for agent in AGENTS {
            assert!(agent.yolo.is_some(), "{} should have YOLO mode", agent.name);
        }
    }

    #[test]
    fn test_send_keys_enter_delay() {
        assert_eq!(send_keys_enter_delay("omp"), 0);
        assert_eq!(send_keys_enter_delay("unknown_agent"), 0);
    }

    #[test]
    fn test_install_hint_lookup() {
        assert_eq!(
            install_hint("omp"),
            Some("curl -fsSL https://raw.githubusercontent.com/nicepkg/oh-my-pi/main/scripts/install.sh | bash")
        );
        assert!(install_hint("claude").is_none());
    }
}
