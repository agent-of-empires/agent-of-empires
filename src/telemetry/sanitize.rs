//! The telemetry privacy boundary: free-form strings are coerced to closed vocabularies here.

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

#[derive(Clone, Copy)]
enum Needle {
    Substr(&'static str),
    /// Whole-token match for short needles, so `o3-mini` is openai but `kilo3` is not.
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

pub fn model_bucket(model: Option<&str>) -> &'static str {
    let Some(model) = model.map(str::trim).filter(|s| !s.is_empty()) else {
        return "unset";
    };
    let lower = model.to_ascii_lowercase();
    use Needle::{Substr, Token};
    // No in-repo source of model names; unmatched models bucket as `other`, so watch that rate.
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
    fn agent_bucket_keeps_known_names_and_collapses_the_rest_to_custom() {
        for (raw, bucket) in [
            ("claude", "claude"),
            ("CLAUDE", "claude"),
            ("codex", "codex"),
            ("gemini", "gemini"),
            ("opencode", "opencode"),
            ("/usr/local/bin/my-secret-agent", "custom"),
            ("acme-internal-llm", "custom"),
            ("", "custom"),
            ("   ", "custom"),
        ] {
            assert_eq!(agent_bucket(raw), bucket, "{raw:?}");
        }
    }

    #[test]
    fn model_bucket_maps_families_and_reports_unset_separately_from_other() {
        for (raw, bucket) in [
            (Some("claude-opus-4-8"), "claude"),
            (Some("gpt-5"), "openai"),
            (Some("o3-mini"), "openai"),
            (Some("gemini-2.5-pro"), "gemini"),
            (Some("qwen3-coder"), "qwen"),
            (None, "unset"),
            (Some(""), "unset"),
            (Some("   "), "unset"),
            (Some("acme-internal-v2"), "other"),
        ] {
            assert_eq!(model_bucket(raw), bucket, "{raw:?}");
        }
    }

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
