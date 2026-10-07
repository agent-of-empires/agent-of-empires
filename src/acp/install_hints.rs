//! Per-binary install hint catalog for ACP adapters and native CLIs.

/// Friendly binary token for aoe's own multi-provider agent.
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
        // Bundled in the aoe binary; installed into the data dir on demand.
        AOE_AGENT_BINARY => "aoe acp doctor --fix --adapter aoe-agent",
        "prime-agent" => {
            "curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | sh  (then `/login` once)"
        }
        _ => return None,
    })
}

/// The npm package spec for an agent the daemon can install itself via a
/// plain `npm install -g <pkg>`, or `None` for agents that need a different
/// installer (curl|bash, brew, manual).
pub fn npm_package_for(binary: &str) -> Option<&'static str> {
    Some(match binary {
        "claude-agent-acp" => "@agentclientprotocol/claude-agent-acp@latest",
        "codex-acp" => "@agentclientprotocol/codex-acp@latest",
        "gemini" => "@google/gemini-cli",
        _ => return None,
    })
}

/// Operator env vars to forward to a given ACP binary, on top of the
/// infrastructure-only `ALWAYS_FORWARD_ENV` in `acp_client/spawn.rs`.
pub fn env_allowlist_for(binary: &str) -> &'static [&'static str] {
    match binary {
        "claude-agent-acp" => &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
        ],
        AOE_AGENT_BINARY => &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "GOOGLE_GENERATIVE_AI_API_KEY",
        ],
        // Verified from codex-acp 1.3.0 (dist, `readApiKeyFromEnv`): it takes
        // CODEX_API_KEY first, then OPENAI_API_KEY.
        "codex-acp" => &[
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_HOME",
        ],
        "opencode" => &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GOOGLE_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_API_KEY",
        ],
        "gemini" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
        ],
        "prime-agent" => &[
            "PRIME_API_KEY",
            "PRIME_TEAM_ID",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AZURE_OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "GEMINI_API_KEY",
            "GROQ_API_KEY",
            "CEREBRAS_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "AI_GATEWAY_API_KEY",
            "ZAI_API_KEY",
            "MISTRAL_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "MOONSHOT_API_KEY",
            "HF_TOKEN",
            "FIREWORKS_API_KEY",
            "OPENCODE_API_KEY",
            "KIMI_API_KEY",
            "CLOUDFLARE_API_KEY",
            "XIAOMI_API_KEY",
            "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            "COPILOT_GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GOOGLE_CLOUD_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_ROLE_ARN",
            "AWS_ROLE_SESSION_NAME",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        ],
        _ => &[],
    }
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
            "kimi",
            "omp",
            "prime-agent",
        ] {
            assert!(
                install_hint_for(binary).is_some(),
                "missing install hint for {binary}"
            );
        }
    }
}
