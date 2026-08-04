//! Operator policy for which ACP agents a session may run (#3241).
//!
//! `acp.default_agent` only picks a default; it is not a constraint. An
//! operator deploying aoe to end users (a locked-down `aoe serve --cityhall`
//! workspace, or any shared deployment) needs to say "these agents and no
//! others". `acp.restrict_agents` plus `acp.allowed_agents` is that lever, and
//! this module is the single evaluator for it.
//!
//! **The policy is read from the GLOBAL config, never from a profile- or
//! repo-resolved one.** That is deliberate and load-bearing, not an oversight:
//! `global_only` on the two fields only sets `profile_overridable = false` in
//! the settings schema, which the TUI renderer honors when listing rows. It is
//! not a server-side authorization check, so a profile override could otherwise
//! be written and would win at read time, widening the very list it is supposed
//! to be constrained by. Resist the temptation to "fix the inconsistency" by
//! reading `resolved_cfg.acp` at a call site because the resolved config
//! happens to be in scope there.
//!
//! Two enforcement points matter, and both must consult this policy:
//!
//! - [`crate::acp::supervisor::Supervisor::resolve_agent_spec`], the choke
//!   point every fresh spawn funnels through.
//! - [`crate::acp::supervisor::Supervisor::attach`], which reattaches a worker
//!   that outlived the daemon. Without a check there the policy would apply
//!   only to new processes, and a worker running a since-disallowed agent would
//!   survive a tightening indefinitely.
//!
//! Scope: this governs ACP registry keys, so it governs the structured view. A
//! terminal session runs the agent in a tmux pane where the user can exec any
//! binary, so an allowlist there would advertise a guarantee it cannot keep.
//! CityHall forces the structured view and denies the terminal routes outright.

/// The effective agent allowlist. Build with [`AgentPolicy::load`]; ask with
/// [`AgentPolicy::allows`].
#[derive(Debug, Clone)]
pub struct AgentPolicy {
    restrict: bool,
    allowed: Vec<String>,
}

impl AgentPolicy {
    /// Read the policy from the global config. A missing or unreadable config
    /// yields the unrestricted default, matching every other consumer of
    /// [`crate::session::config::load_config`]: a fresh install has no config
    /// file at all, so treating that as "deny everything" would brick it.
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

    /// A policy that permits nothing, for a caller whose load failed. Failing
    /// closed there matters: a panicked config read must not read as "the
    /// operator restricted nothing".
    pub fn deny_all() -> Self {
        Self {
            restrict: true,
            allowed: Vec::new(),
        }
    }

    /// True when `agent_key` may run. Unrestricted mode allows everything;
    /// restricted mode allows only an exact registry-key match, so an empty
    /// list denies every agent.
    ///
    /// Matching is exact rather than case-insensitive or trimmed: registry keys
    /// are lowercase ASCII identifiers (`claude`, `codex`, `opencode`) and a
    /// custom agent's key is whatever the operator typed as its
    /// `agent_acp_cmd` map key, so an exact compare is the only rule that
    /// cannot silently permit a near-miss.
    pub fn allows(&self, agent_key: &str) -> bool {
        !self.restrict || self.allowed.iter().any(|a| a == agent_key)
    }

    /// Build a policy without touching disk, so a test can exercise an
    /// enforcement point without writing a config file and serializing on the
    /// process-global `HOME`. Test-only on purpose: production must go through
    /// [`Self::load`], which is what pins the global-config source.
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
            // Restriction on with an empty list denies everything. This is the
            // state the companion bool exists to make expressible; an empty
            // list alone used to be indistinguishable from "unset".
            (true, &[][..], "claude", false),
            // The alias is a distinct key, so allowing `claude` does not
            // silently allow `claude-code`.
            (true, &["claude"][..], "claude-code", false),
            // Near-misses stay denied: no trimming, no case folding.
            (true, &["claude"][..], "Claude", false),
            (true, &["claude"][..], " claude", false),
            // The binary name is not the registry key. A legacy worker record
            // whose `agent_key` is empty falls back to `agent_name` in
            // `attach`, and that must fail closed rather than match `claude`.
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
