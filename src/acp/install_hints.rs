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
        "codex-acp" => "npm install -g @zed-industries/codex-acp",
        "pi-acp" => {
            "npm install -g pi-acp (also requires `npm install -g @earendil-works/pi-coding-agent`)"
        }
        "opencode" => "curl -fsSL https://opencode.ai/install | bash  (then `opencode acp`)",
        "gemini" => "npm install -g @google/gemini-cli  (then `gemini --acp`)",
        "vibe-acp" => {
            "follow https://github.com/mistralai/mistral-vibe (ships the `vibe-acp` binary)"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_default_registry_binary() {
        for binary in [
            "claude-agent-acp",
            "codex-acp",
            "opencode",
            "gemini",
            "vibe-acp",
            "pi-acp",
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
