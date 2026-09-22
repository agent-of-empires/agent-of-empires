//! Operator policy for which ACP agents a session may run.

/// The effective agent allowlist.
#[derive(Debug, Clone)]
pub struct AgentPolicy {
    restrict: bool,
    allowed: Vec<String>,
}

impl AgentPolicy {
    /// Read the policy from the global config.
    pub fn load() -> Self {
        let acp = crate::session::config::load_config()
            .ok()
            .flatten()
            .map(|c| c.acp)
            .unwrap_or_default();
        Self {
            restrict: acp.restrict_agents,
            allowed: acp.allowed_agents,
        }
    }

    /// A policy that permits nothing, for a caller whose load failed (fail closed).
    pub fn deny_all() -> Self {
        Self {
            restrict: true,
            allowed: Vec::new(),
        }
    }

    /// True when `agent_key` may run.
    pub fn allows(&self, agent_key: &str) -> bool {
        !self.restrict || self.allowed.iter().any(|a| a == agent_key)
    }

    /// Build a policy without touching disk, so a test can exercise an
    /// enforcement point without writing a config file and serializing on the
    /// process-global `HOME`.
    #[cfg(test)]
    pub(crate) fn for_test(restrict: bool, allowed: &[&str]) -> Self {
        Self {
            restrict,
            allowed: allowed.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_honors_restrict_flag_and_exact_keys() {
        let cases = [
            // Restriction off: the list is inert, whatever it holds.
            (false, &[][..], "claude", true),
            (false, &["codex"][..], "claude", true),
            // Restriction on: exact registry-key match only.
            (true, &["claude"][..], "claude", true),
            (true, &["claude", "codex"][..], "codex", true),
            (true, &["claude"][..], "codex", false),
            // A custom agent key is just another string; no special case.
            (true, &["oc-superpowers"][..], "oc-superpowers", true),
            (true, &["claude"][..], "oc-superpowers", false),
            // Restriction on with an empty list denies everything.
            (true, &[][..], "claude", false),
            (true, &["claude"][..], "claude-code", false),
            // Near-misses stay denied: no trimming, no case folding.
            (true, &["claude"][..], "Claude", false),
            (true, &["claude"][..], " claude", false),
            // The binary name is not the registry key.
            (true, &["claude"][..], "claude-agent-acp", false),
            (true, &["claude"][..], "", false),
        ];
        for (restrict, allowed, key, expected) in cases {
            assert_eq!(
                AgentPolicy::for_test(restrict, allowed).allows(key),
                expected,
                "restrict={restrict} allowed={allowed:?} key={key:?}"
            );
        }
    }
}
