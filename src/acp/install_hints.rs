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

/// Extra operator env vars to forward to a given ACP binary, on top of
/// `ALWAYS_FORWARD_ENV` in `acp_client.rs` (which already carries the
/// Anthropic/Claude keys plus PATH/HOME/locale/SSH). Empty slice means
/// nothing extra: Claude adapters use `ALWAYS_FORWARD_ENV` verbatim, and
/// four adapters (`pi-acp`, `omp`, `kimi`, `vibe-acp`) are intentionally
/// deferred because their env-var names could not be source-verified for
/// #3238 and shipping a guess that never matches would silently no-op
/// the fix. Follow-up: verify each adapter's real reads from its own
/// package/binary and add its arm.
///
/// The key is the friendly binary token used at registration time (e.g.
/// `"aoe-agent"`), NOT `AgentSpec.command`. `command` for `aoe-agent`
/// carries a `${aoe_data_dir}/...` placeholder that is substituted at
/// spawn time (`supervisor.rs`), so keying on it here would silently
/// miss the bundled agent.
pub fn env_allowlist_for(binary: &str) -> &'static [&'static str] {
    match binary {
        // Verified from source: acp-worker/aoe-agent/src/index.ts imports
        // only @ai-sdk/{anthropic,openai,google}. Anthropic is already in
        // ALWAYS_FORWARD_ENV. @ai-sdk/google reads GOOGLE_GENERATIVE_AI_API_KEY
        // (NOT GEMINI_API_KEY, which is the gemini-CLI-native name).
        "aoe-agent" => &["OPENAI_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
        // Verified from canonical provider env names: codex reads OpenAI SDK
        // env (OPENAI_API_KEY/BASE_URL) and CODEX_HOME to reuse the operator's
        // `codex login` auth.json.
        "codex-acp" => &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"],
        // Verified from canonical AI-SDK provider names: opencode is
        // AI-SDK-based (Google reads GOOGLE_GENERATIVE_AI_API_KEY, distinct
        // from gemini CLI-native GEMINI_API_KEY). OPENROUTER is a common
        // opencode target. OPENCODE_API_KEY covers OpenCode Cloud auth.
        "opencode" => &[
            "OPENAI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_API_KEY",
        ],
        // Verified from canonical Google Gemini CLI env: uses the CLI-native
        // names (distinct from AI-SDK's GOOGLE_GENERATIVE_AI_API_KEY), plus
        // Vertex/ADC (GOOGLE_APPLICATION_CREDENTIALS, GOOGLE_CLOUD_PROJECT).
        "gemini" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
        ],
        _ => &[],
    }
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
