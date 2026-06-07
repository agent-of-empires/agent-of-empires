//! OMP structured-view agent registry.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub command: String,
    pub args: Vec<String>,
    pub description: String,
    pub env_allowlist: Option<Vec<String>>,
}

impl AgentSpec {
    pub fn from_acp_cmd(name: &str, cmd: &str) -> Result<AgentSpec, String> {
        let parts = shell_words::split(cmd).map_err(|e| format!("invalid command: {e}"))?;
        let Some((command, args)) = parts.split_first() else {
            return Err("agent_acp_cmd cannot be empty".to_string());
        };
        Ok(AgentSpec {
            command: command.clone(),
            args: args.to_vec(),
            description: format!("Custom ACP agent `{name}`"),
            env_allowlist: None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRegistry {
    pub agents: HashMap<String, AgentSpec>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.agents.insert(
            "omp".into(),
            AgentSpec {
                command: "omp".into(),
                args: vec!["acp".into()],
                description: "OH-MY-PI native ACP via `omp acp`".into(),
                env_allowlist: None,
            },
        );
        reg
    }

    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.agents.get(name)
    }

    pub fn upsert(&mut self, name: String, spec: AgentSpec) {
        self.agents.insert(name, spec);
    }

    pub fn remove(&mut self, name: &str) -> Option<AgentSpec> {
        self.agents.remove(name)
    }

    pub fn list(&self) -> Vec<(&String, &AgentSpec)> {
        let mut entries: Vec<_> = self.agents.iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_only_omp() {
        let reg = AgentRegistry::with_defaults();
        assert_eq!(reg.list().len(), 1);
        let omp = reg.get("omp").unwrap();
        assert_eq!(omp.command, "omp");
        assert_eq!(omp.args, vec!["acp"]);
        assert!(reg.get("claude").is_none());
    }

    #[test]
    fn from_acp_cmd_splits_argv() {
        let spec = AgentSpec::from_acp_cmd("oc-sp", "ocp run sp acp").unwrap();
        assert_eq!(spec.command, "ocp");
        assert_eq!(spec.args, vec!["run", "sp", "acp"]);
        assert_eq!(spec.description, "Custom ACP agent `oc-sp`");
        assert!(spec.env_allowlist.is_none());
    }

    #[test]
    fn from_acp_cmd_honors_quoting() {
        let spec = AgentSpec::from_acp_cmd("wrap", "sh -lc 'ocp run sp acp'").unwrap();
        assert_eq!(spec.command, "sh");
        assert_eq!(spec.args, vec!["-lc", "ocp run sp acp"]);
    }

    #[test]
    fn from_acp_cmd_rejects_empty() {
        assert!(AgentSpec::from_acp_cmd("x", "").is_err());
        assert!(AgentSpec::from_acp_cmd("x", "   ").is_err());
    }

    #[test]
    fn from_acp_cmd_rejects_unbalanced_quotes() {
        assert!(AgentSpec::from_acp_cmd("x", "ocp run \"unterminated").is_err());
    }

    #[test]
    fn list_is_sorted() {
        let mut reg = AgentRegistry::new();
        reg.upsert(
            "zeta".into(),
            AgentSpec {
                command: "z".into(),
                args: vec![],
                description: "z".into(),
                env_allowlist: None,
            },
        );
        reg.upsert(
            "alpha".into(),
            AgentSpec {
                command: "a".into(),
                args: vec![],
                description: "a".into(),
                env_allowlist: None,
            },
        );
        let names: Vec<&str> = reg.list().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }
}
