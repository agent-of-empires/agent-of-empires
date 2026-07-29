//! Per-binary install hint catalog for ACP adapters and native CLIs.
//!
//! Surfaced by the doctor (`aoe acp doctor`), the `aoe add` path, and
//! the ACP handshake failure path so the user sees the correct command
//! for whichever agent they tried to spawn.

/// Returns the install command for a known ACP binary, or `None` for
/// unknown commands so callers can fall through to a generic message.
pub fn install_hint_for(binary: &str) -> Option<&'static str> {
    Some(match binary {
        "claude-agent-acp" => "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        "codex-acp" => "npm install -g @agentclientprotocol/codex-acp@latest",
        "pi-acp" => {
            "npm install -g pi-acp (also requires `npm install -g @earendil-works/pi-coding-agent`)"
        }
        "opencode" => "curl -fsSL https://opencode.ai/install | bash  (then `opencode acp`)",
        "gemini" => "npm install -g @google/gemini-cli  (then `gemini --acp`)",
        "vibe-acp" => {
            "follow https://github.com/mistralai/mistral-vibe (ships the `vibe-acp` binary)"
        }
        "kimi" => "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash  (then `kimi acp`)",
        "omp" => "curl -fsSL https://omp.sh/install | sh",
        _ => return None,
    })
}

/// The npm package spec for an agent the daemon can install itself via a
/// plain `npm install -g <pkg>`, or `None` for agents that need a different
/// installer (curl|bash, brew, manual). Distinct from `install_hint_for`,
/// whose strings are human-facing and not shell-safe to execute. Only the
/// npm subset is eligible for the web "Update & restart" action; everything
/// else falls back to the displayed manual hint. See #2109.
pub fn npm_package_for(binary: &str) -> Option<&'static str> {
    Some(match binary {
        "claude-agent-acp" => "@agentclientprotocol/claude-agent-acp@latest",
        "codex-acp" => "@agentclientprotocol/codex-acp@latest",
        "gemini" => "@google/gemini-cli",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_package_only_for_clean_npm_agents() {
        assert_eq!(
            npm_package_for("codex-acp"),
            Some("@agentclientprotocol/codex-acp@latest")
        );
        assert_eq!(
            npm_package_for("claude-agent-acp"),
            Some("@agentclientprotocol/claude-agent-acp@latest")
        );
        assert_eq!(npm_package_for("gemini"), Some("@google/gemini-cli"));
        // curl|bash and manual-install agents are intentionally excluded.
        assert_eq!(npm_package_for("opencode"), None);
        assert_eq!(npm_package_for("vibe-acp"), None);
        assert_eq!(npm_package_for("pi-acp"), None);
        assert_eq!(npm_package_for("omp"), None);
        assert_eq!(npm_package_for("nonexistent"), None);
    }

    #[test]
    fn covers_every_default_registry_binary() {
        for binary in [
            "claude-agent-acp",
            "codex-acp",
            "opencode",
            "gemini",
            "vibe-acp",
            "pi-acp",
            "kimi",
            "omp",
        ] {
            assert!(
                install_hint_for(binary).is_some(),
                "missing install hint for {binary}"
            );
        }
    }

    #[test]
    fn returns_none_for_unknown_binary() {
        assert!(install_hint_for("nonexistent-acp").is_none());
        assert!(install_hint_for("").is_none());
    }
}
