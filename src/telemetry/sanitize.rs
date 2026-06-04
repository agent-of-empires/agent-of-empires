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

/// How a family needle is matched against a model string.
#[derive(Clone, Copy)]
enum Needle {
    /// Plain substring. Safe for distinctive needles long enough not to collide
    /// (`claude`, `gpt`, `gemini`, ...).
    Substr(&'static str),
    /// Whole-token match: the needle must equal a token of the model string when
    /// split on non-alphanumeric boundaries. Required for the 2-char OpenAI
    /// tokens (`o1` / `o3` / `o4`) so `o3-mini` buckets as openai but `kilo3` or
    /// `macro1` do not. `o3` is a token of `o3-mini` but not of `kilo3`.
    Token(&'static str),
}

impl Needle {
    fn matches(self, lower: &str) -> bool {
        match self {
            Needle::Substr(n) => lower.contains(n),
            Needle::Token(n) => lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|tok| tok == n),
        }
    }
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
    use Needle::{Substr, Token};
    // Manually maintained, privacy-preserving allowlist. Unlike `agent_bucket`,
    // which derives from `crate::agents::AGENTS`, there is no in-repo source of
    // model names to generate this from, so adding a newly common public family
    // is a deliberate release chore: add a `(family, &[needle...])` row here.
    // A model matching no row buckets as `"other"` and its raw name never
    // leaves this function, so unknowns are coarse-counted, never leaked. Watch
    // the `other` rate in the model-bucket telemetry (PostHog) to know when this
    // list has drifted and needs a new family. See `docs/telemetry.md`.
    const FAMILIES: &[(&str, &[Needle])] = &[
        (
            "claude",
            &[
                Substr("claude"),
                Substr("sonnet"),
                Substr("opus"),
                Substr("haiku"),
            ],
        ),
        (
            "openai",
            &[
                Substr("gpt"),
                Substr("openai"),
                Substr("codex"),
                Token("o1"),
                Token("o3"),
                Token("o4"),
            ],
        ),
        ("gemini", &[Substr("gemini")]),
        ("qwen", &[Substr("qwen")]),
        ("grok", &[Substr("grok")]),
        ("llama", &[Substr("llama")]),
        ("mistral", &[Substr("mistral"), Substr("mixtral")]),
        ("deepseek", &[Substr("deepseek")]),
    ];
    for (family, needles) in FAMILIES {
        if needles.iter().any(|n| n.matches(&lower)) {
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

    // #1876: the short OpenAI tokens o1/o3/o4 are matched on a boundary, not as a
    // bare substring, so a name that merely contains those two chars adjacent
    // does not over-count as openai.
    #[test]
    fn short_openai_tokens_do_not_false_positive() {
        for name in [
            "kilo3",
            "macro1-7b",
            "kilo3-experimental",
            "halo4",
            "mono1x",
        ] {
            assert_eq!(
                model_bucket(Some(name)),
                "other",
                "`{name}` must not bucket as openai"
            );
        }
    }

    // #1878 user story (maintainer visibility): a model from an unlisted family
    // is not silently swallowed. It buckets as the observable `"other"`
    // discriminator, which the snapshot reports as a non-zero count, so the
    // allowlist drift is visible in the aggregate instead of vanishing.
    #[test]
    fn unknown_family_is_observable_as_other() {
        for name in [
            "acme-internal-v2",
            "future-model-9000",
            "kimi-k2",
            "phi-4",
            "command-r-plus",
        ] {
            assert_eq!(
                model_bucket(Some(name)),
                "other",
                "`{name}` from an unlisted family must surface as the observable `other` bucket"
            );
        }
    }

    // #1878 user story (privacy): no raw model name, nor any reversible form of
    // it, escapes the sanitizer. Whatever is fed in, the output is always one of
    // the fixed, closed vocabulary, so an internal or sensitive model name can
    // never reach a payload.
    #[test]
    fn output_is_always_from_the_closed_vocabulary() {
        const VOCAB: &[&str] = &[
            "claude", "openai", "gemini", "qwen", "grok", "llama", "mistral", "deepseek", "other",
            "unset",
        ];
        for input in [
            None,
            Some(""),
            Some("   "),
            Some("claude-opus-4-8"),
            Some("gpt-5"),
            Some("acme-secret-internal-llm-v7"),
            Some("/opt/models/customer-private-finetune"),
            Some("name with spaces and / slashes"),
        ] {
            let bucket = model_bucket(input);
            assert!(
                VOCAB.contains(&bucket),
                "model_bucket({input:?}) returned `{bucket}`, outside the closed vocabulary"
            );
            if let Some(raw) = input {
                let raw = raw.trim();
                if !raw.is_empty() && !VOCAB.contains(&raw.to_ascii_lowercase().as_str()) {
                    assert_ne!(
                        bucket, raw,
                        "the raw model string must never be returned verbatim"
                    );
                }
            }
        }
    }

    #[test]
    fn real_openai_models_still_bucket() {
        for name in [
            "o1",
            "o1-mini",
            "o1-preview",
            "o3",
            "o3-mini",
            "o4-mini",
            "gpt-5",
            "gpt-4o",
            "codex",
        ] {
            assert_eq!(
                model_bucket(Some(name)),
                "openai",
                "`{name}` must bucket as openai"
            );
        }
    }
}
