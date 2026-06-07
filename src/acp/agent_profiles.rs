//! Server-side OMP structured-view profile.

/// Per-agent server-side profile. Static; resolved by registry key.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub key: &'static str,
    pub parent_meta_namespaces: &'static [&'static str],
    pub clear_aliases: &'static [&'static str],
    pub supports_exit_plan_mode: bool,
    pub supports_wakeup_tools: bool,
}

impl AgentProfile {
    pub fn is_clear_command(&self, text: &str) -> bool {
        let trimmed = text.trim();
        for alias in self.clear_aliases {
            if trimmed == *alias {
                return true;
            }
            if let Some(rest) = trimmed.strip_prefix(*alias) {
                if rest.starts_with(char::is_whitespace) {
                    return true;
                }
            }
        }
        false
    }

    pub fn parent_tool_use_id_from_meta(
        &self,
        meta: &Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Option<String> {
        let map = meta.as_ref()?;
        for namespace in self.parent_meta_namespaces {
            if let Some(v) = map
                .get(*namespace)
                .and_then(|ns| ns.get("parentToolUseId"))
                .and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
        None
    }

    pub fn supports_memory_recall_tool(&self) -> bool {
        self.parent_meta_namespaces.contains(&"claudeCode")
    }
}

pub const OMP: AgentProfile = AgentProfile {
    key: "omp",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
};

pub const DEFAULT: AgentProfile = AgentProfile {
    key: "default",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
};

pub fn resolve(key: &str) -> &'static AgentProfile {
    match key {
        "omp" => &OMP,
        _ => &DEFAULT,
    }
}

#[cfg(test)]
pub const CLAUDE: AgentProfile = OMP;

#[cfg(test)]
pub const CODEX: AgentProfile = DEFAULT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_agents() {
        assert_eq!(resolve("omp").key, "omp");
    }

    #[test]
    fn resolve_falls_back_to_default() {
        assert_eq!(resolve("").key, "default");
        assert_eq!(resolve("claude").key, "default");
        assert_eq!(resolve("unknown-agent").key, "default");
    }

    #[test]
    fn omp_has_no_clear_command_alias() {
        assert!(!OMP.is_clear_command("/clear"));
        assert!(!OMP.is_clear_command("/new"));
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_omp() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "omp".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-9" }),
        );
        assert!(OMP.parent_tool_use_id_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_none_meta() {
        assert!(OMP.parent_tool_use_id_from_meta(&None).is_none());
    }

    #[test]
    fn capability_flags_are_off_for_omp() {
        assert!(!OMP.supports_exit_plan_mode);
        assert!(!OMP.supports_wakeup_tools);
        assert!(!OMP.supports_memory_recall_tool());
    }
}
