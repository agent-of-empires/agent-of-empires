//! Age of Empires cheat-code easter eggs for the TUI command palette.
//!
//! A wink at the "Agent of Empires" name. Mirrors the web registry
//! (`web/src/lib/cheats.ts`); the two are hand-kept parallel since Rust and
//! TypeScript can't share a literal. The TUI surfaces only the toast message
//! (via `Action::SetTransientStatus`), not the web's visual flourishes.

/// Cheat code (already normalized: lowercase, single-spaced) to the transient
/// message shown when the user types it into the command palette.
const CHEATS: &[(&str, &str)] = &[
    // Age of Empires 1
    (
        "wololo",
        "Wololo. The agent in the next worktree converts to your cause.",
    ),
    ("photon man", "A Photon Man vaporizes your merge conflicts."),
    ("pow", "A baby on a tricycle with a gun. Don't ask."),
    // Age of Empires 2
    (
        "how do you turn this on",
        "🚗 A Cobra Car spawns in your sandbox and floors it.",
    ),
    (
        "tuck tuck tuck",
        "A monster truck flattens your flaky tests.",
    ),
    ("marco", "Marco! Every session revealed."),
    (
        "polo",
        "Polo! Fog lifted. You see what every agent thinks. (No you don't.)",
    ),
    (
        "aegis",
        "AEGIS on. Worktrees build instantly. (cargo still takes 4 min.)",
    ),
    ("rock on", "+1000 stone. Spent it on rate limits."),
    ("lumberjack", "+1000 wood. The daemon is cozy."),
    ("robin hood", "+1000 tokens. Use them wisely."),
    (
        "cheese steak jimmy's",
        "+1000 food. Agents refuse to stop coding.",
    ),
    (
        "i love the monkey head",
        "🐵 VDML. A furious monkey reviews your PR.",
    ),
    (
        "black death",
        "Black Death. All flaky tests eliminated. (They'll be back.)",
    ),
    // Age of Empires 3
    (
        "ya gotta make do with what ya got",
        "The Tommynator rolls in and crushes your tech debt.",
    ),
    (
        "nova & orion",
        "+10000 XP. You leveled up to Staff Engineer.",
    ),
    ("medium rare please", "+10000 food. The grill never stops."),
    (
        "give me liberty or give me coin",
        "+10000 coin. Still can't afford Opus.",
    ),
    ("this is too hard", "You win. PR merged. (It wasn't.)"),
    ("speed always wins", "Research speed x100. CI still queued."),
    (
        "x marks the spot",
        "Map revealed. The bug was in your code all along.",
    ),
];

/// Normalize palette input the same way the web matcher does: lowercase, trim,
/// and collapse internal whitespace runs to a single space.
fn normalize(input: &str) -> String {
    input
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the cheat message for a full-string match, or `None`. Substrings
/// never match, so ordinary palette searches ("settings", "new") pass through.
pub fn match_cheat(input: &str) -> Option<&'static str> {
    let normalized = normalize(input);
    CHEATS
        .iter()
        .find(|(code, _)| *code == normalized)
        .map(|(_, message)| *message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_code_is_normalized_and_matches() {
        // An unnormalized key would never match what match_cheat looks up.
        for (code, message) in CHEATS {
            assert_eq!(normalize(code), *code, "not normalized: {code:?}");
            assert_eq!(match_cheat(code), Some(*message));
        }
    }

    #[test]
    fn match_cheat_is_exact_modulo_case_and_whitespace() {
        for (query, hit) in [
            ("WOLOLO", true),
            ("  Rock On  ", true),
            ("how do  you   turn this on", true),
            ("settings", false),
            ("new session", false),
            ("", false),
            ("wolol", false),
            ("wololo and more", false),
            ("say marco", false),
        ] {
            assert_eq!(match_cheat(query).is_some(), hit, "{query:?}");
        }
    }
}
