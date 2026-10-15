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

    /// Model names are free text and may be private: every output must come
    /// from the closed vocabulary, and short tokens must not false-positive.
    #[test]
    fn model_bucket_maps_families_into_the_closed_vocabulary() {
        const VOCAB: &[&str] = &[
            "claude", "openai", "gemini", "qwen", "grok", "llama", "mistral", "deepseek", "other",
            "unset",
        ];
        let openai = [
            "o1",
            "o1-mini",
            "o1-preview",
            "o3",
            "o3-mini",
            "o4-mini",
            "gpt-5",
            "gpt-4o",
            "codex",
        ];
        let other = [
            "kilo3",
            "macro1-7b",
            "kilo3-experimental",
            "halo4",
            "mono1x",
            "acme-internal-v2",
            "future-model-9000",
            "kimi-k2",
            "phi-4",
            "command-r-plus",
            "acme-secret-internal-llm-v7",
            "/opt/models/customer-private-finetune",
            "name with spaces and / slashes",
        ];
        let cases = [
            (Some("claude-opus-4-8"), "claude"),
            (Some("gemini-2.5-pro"), "gemini"),
            (Some("qwen3-coder"), "qwen"),
            (None, "unset"),
            (Some(""), "unset"),
            (Some("   "), "unset"),
        ]
        .into_iter()
        .chain(openai.into_iter().map(|name| (Some(name), "openai")))
        .chain(other.into_iter().map(|name| (Some(name), "other")));
        for (raw, bucket) in cases {
            let got = model_bucket(raw);
            assert_eq!(got, bucket, "{raw:?}");
            assert!(VOCAB.contains(&got), "{raw:?} -> {got}");
        }
    }
}
