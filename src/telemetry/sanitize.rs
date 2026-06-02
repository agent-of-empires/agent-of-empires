//! The single privacy boundary for telemetry.
//!
//! Every free-form string that could carry user content (agent command,
//! model name) is coerced here against a closed allowlist before it can
//! reach a payload. Raw values never leave this module: an agent that is
//! not a recognised built-in becomes `"custom"`, and a model string that
//! matches no known family becomes a coarse bucket (`"other"` / `"unset"`).
//!
//! The agent allowlist is derived from [`crate::agents::AGENTS`] rather than
//! hardcoded, so adding a built-in agent keeps the sanitizer in sync without
//! a second edit. Anything outside that set collapses to `"custom"`.

/// Bucket for an agent identifier (`tool` / `detect_as`).
///
/// Returns the canonical built-in name when the input matches a known agent
/// (case-insensitive, by canonical name or alias); otherwise `"custom"`. An
/// empty input is treated as unknown and returns `"custom"`.
pub fn agent_bucket(agent: &str) -> String {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return "custom".to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    for def in crate::agents::AGENTS {
        if def.name.eq_ignore_ascii_case(&lower)
            || def.aliases.iter().any(|a| a.eq_ignore_ascii_case(&lower))
        {
            return def.name.to_string();
        }
    }
    "custom".to_string()
}

/// Coarse family bucket for a model string. Never emits the raw value; maps
/// to a small fixed vocabulary so an internal/custom model name can't leak.
///
/// `None` or empty → `"unset"`. A string matching no known family → `"other"`.
pub fn model_bucket(model: Option<&str>) -> &'static str {
    let Some(model) = model.map(str::trim).filter(|s| !s.is_empty()) else {
        return "unset";
    };
    let lower = model.to_ascii_lowercase();
    const FAMILIES: &[(&str, &[&str])] = &[
        ("claude", &["claude", "sonnet", "opus", "haiku"]),
        ("openai", &["gpt", "openai", "codex", "o1", "o3", "o4"]),
        ("gemini", &["gemini"]),
        ("qwen", &["qwen"]),
        ("grok", &["grok"]),
        ("llama", &["llama"]),
        ("mistral", &["mistral", "mixtral"]),
        ("deepseek", &["deepseek"]),
    ];
    for (family, needles) in FAMILIES {
        if needles.iter().any(|n| lower.contains(n)) {
            return family;
        }
    }
    "other"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_agents_keep_canonical_name() {
        assert_eq!(agent_bucket("claude"), "claude");
        assert_eq!(agent_bucket("CLAUDE"), "claude");
        assert_eq!(agent_bucket("codex"), "codex");
        assert_eq!(agent_bucket("gemini"), "gemini");
        assert_eq!(agent_bucket("opencode"), "opencode");
    }

    #[test]
    fn unknown_agent_collapses_to_custom() {
        // A custom command or an internal wrapper must never surface verbatim.
        assert_eq!(agent_bucket("/usr/local/bin/my-secret-agent"), "custom");
        assert_eq!(agent_bucket("acme-internal-llm"), "custom");
        assert_eq!(agent_bucket(""), "custom");
        assert_eq!(agent_bucket("   "), "custom");
    }

    #[test]
    fn model_buckets_map_to_families() {
        assert_eq!(model_bucket(Some("claude-opus-4-8")), "claude");
        assert_eq!(model_bucket(Some("gpt-5")), "openai");
        assert_eq!(model_bucket(Some("o3-mini")), "openai");
        assert_eq!(model_bucket(Some("gemini-2.5-pro")), "gemini");
        assert_eq!(model_bucket(Some("qwen3-coder")), "qwen");
    }

    #[test]
    fn model_bucket_unset_and_other() {
        assert_eq!(model_bucket(None), "unset");
        assert_eq!(model_bucket(Some("")), "unset");
        assert_eq!(model_bucket(Some("   ")), "unset");
        // An internal/unknown model name must collapse to "other", not leak.
        assert_eq!(model_bucket(Some("acme-internal-v2")), "other");
    }
}
