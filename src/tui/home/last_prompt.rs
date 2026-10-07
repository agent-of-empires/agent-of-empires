//! Scraping the last prompt the user sent to a *terminal-view* session.
//!
//! aoe keeps no record of prompts for terminal sessions: the `aoe send` path
//! relays the text into the tmux pane and discards it, and a directly-typed
//! prompt is never seen at all. So the only source for "what did I last ask" is
//! the pane text itself. This module captures the pane and pulls out the most
//! recent submitted prompt, which the home view paints as a one-line footer
//! (toggled with `Ctrl+L`).
//!
//! It is a best-effort heuristic tuned to the Claude Code TUI, whose submitted
//! prompts render as a line beginning with the `❯ ` marker. The live input box
//! at the very bottom uses the same marker, so the box — the region below the
//! last horizontal rule — is excluded, and an in-progress draft is never
//! mistaken for a sent prompt.

use std::time::{Duration, Instant};

use crate::tmux::Session;

/// Marker Claude Code renders before each submitted user prompt (and before the
/// live input line, which is why the input box is excluded by rule).
const PROMPT_MARKER: &str = "❯ ";

/// How many lines of scrollback to scrape. Generous enough to find the last
/// prompt behind a long agent turn (a quiet turn can emit hundreds of lines),
/// but bounded well under tmux's 50k `history-limit` so a capture stays cheap.
/// If the last prompt is older than this the footer simply shows nothing.
const CAPTURE_LINES: usize = 4000;

/// Minimum gap between pane captures. The last prompt only changes when the user
/// sends one, so a relaxed cadence keeps the footer responsive without spawning
/// a `tmux capture-pane` on every redraw.
const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// Longest prompt text kept for the footer, in characters. The footer is a
/// single clipped row anyway, so a long prompt is truncated with an ellipsis
/// rather than carried (and whitespace-flattened) in full every frame.
const MAX_PROMPT_CHARS: usize = 200;

/// Cached scrape for one session, with the instant it was taken so the caller
/// can throttle refreshes and invalidate on a session switch.
pub(super) struct LastPromptCache {
    pub(super) session: String,
    pub(super) text: Option<String>,
    pub(super) at: Instant,
}

impl LastPromptCache {
    fn is_fresh(&self, session: &str, now: Instant) -> bool {
        self.session == session && now.duration_since(self.at) < REFRESH_INTERVAL
    }
}

/// Capture the pane for `tmux_name` and return its last submitted prompt.
fn scrape(tmux_name: &str) -> Option<String> {
    let pane = Session::from_name(tmux_name)
        .capture_plain(CAPTURE_LINES)
        .ok()?;
    extract_last_prompt(&pane)
}

/// Pure extraction: given captured pane text, return the most recent submitted
/// prompt, or `None`. Submitted prompts are `❯ `-prefixed lines above the live
/// input box; the box is the trailing region after the last horizontal rule.
fn extract_last_prompt(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    // The live input box is the trailing pair of horizontal rules (top border,
    // input line(s), bottom border); scan only the transcript above its top
    // border, so an in-progress draft between the rules is never read as a sent
    // prompt. One rule: scan above it. None (an agent that draws no box): all.
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_horizontal_rule(line))
        .map(|(i, _)| i)
        .collect();
    let scan_end = match rules.len() {
        0 => lines.len(),
        1 => rules[0],
        n => rules[n - 2],
    };
    lines[..scan_end].iter().rev().find_map(|line| {
        let rest = line.trim_start().strip_prefix(PROMPT_MARKER)?;
        // Collapse runs of whitespace to single spaces (the footer is one line)
        // and cap the length so a huge prompt is not carried in full.
        let collapsed = rest.split_whitespace().collect::<Vec<_>>().join(" ");
        (!collapsed.is_empty()).then(|| truncate_chars(&collapsed, MAX_PROMPT_CHARS))
    })
}

/// Take at most `max` characters, appending an ellipsis when the text was
/// longer. Counts characters, not bytes, so multibyte prompts stay valid.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// A box-drawing horizontal rule (the input box's border): at least ten `─` and
/// nothing but box-drawing glyphs or spaces otherwise.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let dashes = trimmed.chars().filter(|&c| c == '─').count();
    dashes >= 10
        && trimmed
            .chars()
            .all(|c| c == ' ' || ('\u{2500}'..='\u{257F}').contains(&c))
}

impl super::HomeView {
    /// Refresh the throttled scrape of the selected terminal session's last
    /// prompt. A no-op (and cache clear) unless the footer is toggled on and a
    /// terminal session is selected; otherwise captures at most once per
    /// [`REFRESH_INTERVAL`], and re-captures immediately on a session switch.
    pub(super) fn refresh_last_prompt(&mut self) {
        if !self.show_last_prompt {
            self.last_prompt_cache = None;
            return;
        }
        let Some(session_id) = self.selected_session.clone() else {
            self.last_prompt_cache = None;
            return;
        };
        let now = Instant::now();
        if matches!(&self.last_prompt_cache, Some(c) if c.is_fresh(&session_id, now)) {
            return;
        }
        let Some(inst) = self.instances.get(&session_id) else {
            self.last_prompt_cache = None;
            return;
        };
        let tmux_name = Session::generate_name(&inst.id, &inst.title);
        let text = scrape(&tmux_name);
        self.last_prompt_cache = Some(LastPromptCache {
            session: session_id,
            text,
            at: now,
        });
    }

    /// The footer line to paint, or `None` when the footer is off or no session
    /// is selected. When on but no prompt was scraped (a non-Claude pane, or the
    /// last prompt scrolled past the capture window), a muted placeholder shows
    /// so the toggle is always visibly acknowledged.
    pub(super) fn last_prompt_footer_line(&self) -> Option<String> {
        if !self.show_last_prompt || self.selected_session.is_none() {
            return None;
        }
        let text = self
            .last_prompt_cache
            .as_ref()
            .and_then(|c| c.text.as_deref());
        Some(text.unwrap_or("(no recent prompt found)").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_last_submitted_prompt_above_the_input_box() {
        let pane = "\
❯ pierwsze pytanie
● some agent output
  more output
❯ drugie pytanie
● agent is replying…
────────────────────────────────────────
❯ draft not yet sent
────────────────────────────────────────
  ⏵⏵ bypass permissions on";
        assert_eq!(extract_last_prompt(pane).as_deref(), Some("drugie pytanie"));
    }

    #[test]
    fn ignores_an_empty_input_line_and_returns_the_sent_prompt() {
        let pane = "\
❯ co robi ten serwis
● odpowiedź agenta
────────────────────────────────────────
❯
────────────────────────────────────────";
        assert_eq!(
            extract_last_prompt(pane).as_deref(),
            Some("co robi ten serwis")
        );
    }

    #[test]
    fn none_when_no_prompt_present() {
        assert_eq!(
            extract_last_prompt("just some output\nno markers here"),
            None
        );
        assert_eq!(extract_last_prompt(""), None);
    }

    #[test]
    fn no_rule_falls_back_to_the_last_marker_line() {
        let pane = "❯ only prompt\n● reply";
        assert_eq!(extract_last_prompt(pane).as_deref(), Some("only prompt"));
    }

    #[test]
    fn whitespace_is_collapsed() {
        let pane = "❯   lots    of   spaces\n● reply";
        assert_eq!(extract_last_prompt(pane).as_deref(), Some("lots of spaces"));
    }

    #[test]
    fn long_prompt_is_truncated_with_an_ellipsis() {
        let long = "x".repeat(MAX_PROMPT_CHARS + 50);
        let pane = format!("❯ {long}\n● reply");
        let got = extract_last_prompt(&pane).expect("prompt");
        assert_eq!(got.chars().count(), MAX_PROMPT_CHARS + 1);
        assert!(got.ends_with('…'));
    }
}
