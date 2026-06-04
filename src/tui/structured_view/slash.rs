//! Slash-command picker helpers: detect when the composer holds a
//! slash query and filter the daemon-advertised command list against
//! it. Pure functions so the picker logic is unit-testable without a
//! ratatui surface or a live daemon.
//!
//! Mirrors the web composer's `fuzzyFilter` scoring (prefix beats
//! substring beats description match), lowercased on both sides so the
//! TUI and web rank the same way.

use crate::acp::state::AvailableCommand;

/// Largest number of matches the picker will surface. The render layer
/// windows around the selection, so this is a sanity bound, not a
/// display cap.
const MAX_MATCHES: usize = 30;

/// If `text` is a single-line slash query (`/word`, no whitespace
/// after the slash), return the query portion **without** the leading
/// slash. Returns `None` when the composer holds anything else: empty,
/// multi-line, not slash-prefixed, or a slash followed by whitespace
/// (which means the user already finished the command name and is
/// typing arguments).
pub fn slash_query(text: &str) -> Option<&str> {
    if text.contains('\n') {
        return None;
    }
    let rest = text.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Filter + rank `commands` against a slash `query` (the text after the
/// leading slash, possibly empty). Empty query returns every command in
/// declaration order. Otherwise scores case-insensitively: name prefix
/// (3) beats name substring (2) beats description substring (1); zero
/// scores drop out. Ties break on shorter name first, then name order.
pub fn filter_commands<'a>(
    query: &str,
    commands: &'a [AvailableCommand],
) -> Vec<&'a AvailableCommand> {
    if query.is_empty() {
        return commands.iter().take(MAX_MATCHES).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(i32, &'a AvailableCommand)> = commands
        .iter()
        .filter_map(|cmd| {
            let name = cmd.name.to_lowercase();
            let score = if name.starts_with(&q) {
                3
            } else if name.contains(&q) {
                2
            } else if cmd.description.to_lowercase().contains(&q) {
                1
            } else {
                0
            };
            (score > 0).then_some((score, cmd))
        })
        .collect();
    scored.sort_by(|a, b| {
        // Tie-break case-insensitively so mixed-case names sort the same
        // way the (case-insensitive) scoring ranks them, matching web
        // parity; fall back to raw bytes only to keep the order total.
        let a_lc = a.1.name.to_lowercase();
        let b_lc = b.1.name.to_lowercase();
        b.0.cmp(&a.0)
            .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
            .then_with(|| a_lc.cmp(&b_lc))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, c)| c)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, desc: &str) -> AvailableCommand {
        AvailableCommand {
            name: name.to_string(),
            description: desc.to_string(),
            accepts_input: false,
        }
    }

    #[test]
    fn slash_query_extracts_word_after_slash() {
        assert_eq!(slash_query("/com"), Some("com"));
        assert_eq!(slash_query("/"), Some(""));
    }

    #[test]
    fn slash_query_rejects_non_slash_and_whitespace_and_multiline() {
        assert_eq!(slash_query("hello"), None);
        assert_eq!(slash_query(""), None);
        // Slash followed by a space = command name finished, typing args.
        assert_eq!(slash_query("/compact now"), None);
        assert_eq!(slash_query("/multi\nline"), None);
    }

    #[test]
    fn filter_empty_query_returns_all_in_order() {
        let commands = vec![cmd("compact", ""), cmd("clear", "")];
        let got = filter_commands("", &commands);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "compact");
        assert_eq!(got[1].name, "clear");
    }

    #[test]
    fn filter_prefix_outranks_substring_and_description() {
        let commands = vec![
            cmd("recompact", "rebuild"),    // substring of "comp"
            cmd("compact", "shrink"),       // prefix
            cmd("noop", "compact the log"), // description only
        ];
        let got = filter_commands("comp", &commands);
        assert_eq!(got[0].name, "compact");
        assert_eq!(got[1].name, "recompact");
        assert_eq!(got[2].name, "noop");
    }

    #[test]
    fn filter_is_case_insensitive() {
        let commands = vec![cmd("Compact", "")];
        assert_eq!(filter_commands("comp", &commands).len(), 1);
        assert_eq!(filter_commands("COMP", &commands).len(), 1);
    }

    #[test]
    fn filter_drops_zero_scores() {
        let commands = vec![cmd("compact", "shrink"), cmd("clear", "wipe")];
        let got = filter_commands("xyz", &commands);
        assert!(got.is_empty());
    }

    #[test]
    fn filter_ties_break_on_shorter_name() {
        let commands = vec![cmd("compactor", ""), cmd("compact", "")];
        let got = filter_commands("compact", &commands);
        assert_eq!(got[0].name, "compact");
        assert_eq!(got[1].name, "compactor");
    }
}
