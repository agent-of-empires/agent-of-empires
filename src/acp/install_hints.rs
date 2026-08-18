//! Per-binary install hint catalog for ACP adapters and native CLIs.
//!
//! Surfaced by the doctor (`aoe acp doctor`), the `aoe add` path, and
//! the ACP handshake failure path so the user sees the correct command
//! for whichever agent they tried to spawn.

/// Friendly binary token for aoe's bundled multi-provider agent. Shared by the
/// registry's spawn registration and [`env_allowlist_for`] so the two cannot
/// drift onto different spellings (which would silently drop the agent's
/// provider keys). Distinct from `AgentSpec.command`, whose value for this
/// agent is the placeholder-templated `${aoe_data_dir}/...` path.
pub const AOE_AGENT_BINARY: &str = "aoe-agent";

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

/// Operator env vars to forward to a given ACP binary, on top of the
/// infrastructure-only `ALWAYS_FORWARD_ENV` in `acp_client.rs`. Empty slice
/// means no ambient provider credentials. Four adapters (`pi-acp`, `omp`,
/// `kimi`, `vibe-acp`) are intentionally deferred because their env-var names
/// could not be source-verified for #3238 and shipping a guess that never
/// matches would silently no-op the fix. Follow-up: verify each adapter's real
/// reads from its own package/binary and add its arm. Every arm below cites the
/// artifact its names came from; do not add one on convention alone.
///
/// The key is the friendly binary token used at registration time (e.g.
/// [`AOE_AGENT_BINARY`]), NOT `AgentSpec.command`. `command` for `aoe-agent`
/// carries a `${aoe_data_dir}/...` placeholder that is substituted at
/// spawn time (`supervisor.rs`), so keying on it here would silently
/// miss the bundled agent.
pub fn env_allowlist_for(binary: &str) -> &'static [&'static str] {
    match binary {
        // Existing Claude adapter contract, previously supplied through
        // ALWAYS_FORWARD_ENV. Keep all four names on the Claude adapter while
        // stopping unrelated and custom adapters from receiving them.
        "claude-agent-acp" => &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
        ],
        // Verified from source: acp-worker/aoe-agent/src/index.ts imports only
        // @ai-sdk/{anthropic,openai,google}, and those providers read their key
        // from the environment themselves. @ai-sdk/anthropic reads
        // ANTHROPIC_API_KEY; @ai-sdk/openai 4.0.27 reads OPENAI_API_KEY and
        // OPENAI_BASE_URL (src/openai-provider.ts); @ai-sdk/google 4.0.31 reads
        // GOOGLE_GENERATIVE_AI_API_KEY, NOT GEMINI_API_KEY (the gemini CLI's
        // own name).
        AOE_AGENT_BINARY => &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "GOOGLE_GENERATIVE_AI_API_KEY",
        ],
        // Verified from codex-acp 1.3.0 (dist, `readApiKeyFromEnv`): it takes
        // CODEX_API_KEY first, then OPENAI_API_KEY. It then execs the codex
        // CLI, which resolves its config dir from CODEX_HOME and its endpoint
        // from OPENAI_BASE_URL, so both ride along for the operator's
        // `codex login` auth.json and any proxy endpoint.
        "codex-acp" => &[
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_HOME",
        ],
        // Verified from the models.dev provider registry opencode resolves
        // against: its `google` entry accepts GOOGLE_API_KEY,
        // GOOGLE_GENERATIVE_AI_API_KEY, and GEMINI_API_KEY alike, so all three
        // are here, and its `anthropic` entry declares ANTHROPIC_API_KEY, which
        // opencode needs for a Claude model now that the key no longer rides
        // the shared forward list. OPENROUTER_API_KEY and OPENCODE_API_KEY
        // (OpenCode Zen) are the `openrouter` / `opencode` entries' declared
        // env.
        "opencode" => &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GOOGLE_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_API_KEY",
        ],
        // Verified from @google/gemini-cli 0.55.1: CLI-native GEMINI_API_KEY /
        // GOOGLE_API_KEY (distinct from AI-SDK's GOOGLE_GENERATIVE_AI_API_KEY),
        // plus the Vertex set. GOOGLE_GENAI_USE_VERTEXAI is what selects Vertex
        // at all, and GOOGLE_CLOUD_LOCATION pairs with GOOGLE_CLOUD_PROJECT, so
        // forwarding the credential without them leaves that path dead.
        "gemini" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
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
