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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tmux::Session;

/// Drop-box a background scrape writes its result into; the render thread adopts
/// it on the next frame. Keeps `tmux capture-pane` off the render/input path.
pub(super) type LastPromptSlot = Arc<Mutex<Option<LastPromptCache>>>;

/// A fresh, empty slot for [`super::HomeView`] construction.
pub(super) fn new_slot() -> LastPromptSlot {
    Arc::new(Mutex::new(None))
}

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

/// Cached scrape for one pane, keyed by the displayed tmux session name and
/// stamped with when it was taken, so the caller can throttle refreshes and
/// invalidate on a pane / view switch or a rename.
pub(super) struct LastPromptCache {
    pub(super) pane: String,
    pub(super) text: Option<String>,
    pub(super) at: Instant,
}

impl LastPromptCache {
    fn is_fresh(&self, pane: &str, now: Instant) -> bool {
        self.pane == pane && now.duration_since(self.at) < REFRESH_INTERVAL
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
    /// Keep the last-prompt cache fresh without ever blocking the render/input
    /// thread: adopt any completed background scrape, then, when the cache for the
    /// displayed pane is stale and no capture is running, dispatch one on a short
    /// background thread. The result's timestamp is set on completion, so a slow
    /// capture does not immediately read as stale and re-fire.
    pub(super) fn refresh_last_prompt(&mut self) {
        if !self.show_last_prompt {
            self.last_prompt_cache = None;
            self.last_prompt_in_flight = false;
            return;
        }
        // The pane actually shown in the preview (honouring the view mode,
        // live-send, and any rename), not a name rebuilt from the title.
        let Some(pane) = self.displayed_pane_tmux_name() else {
            self.last_prompt_cache = None;
            return;
        };
        if let Some(done) = self
            .last_prompt_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        {
            self.last_prompt_in_flight = false;
            self.last_prompt_cache = Some(done);
        }
        let fresh = matches!(&self.last_prompt_cache, Some(c) if c.is_fresh(&pane, Instant::now()));
        if !fresh && !self.last_prompt_in_flight {
            self.last_prompt_in_flight = true;
            let slot = self.last_prompt_slot.clone();
            std::thread::spawn(move || {
                let text = scrape(&pane);
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(LastPromptCache {
                        pane,
                        text,
                        at: Instant::now(),
                    });
                }
            });
        }
    }

    /// The footer line to paint, or `None` when the footer is off or no pane is
    /// shown. The cache is only used when it matches the displayed pane, so a
    /// pane switch shows the placeholder until its own scrape lands rather than a
    /// stale neighbour's prompt.
    pub(super) fn last_prompt_footer_line(&self) -> Option<String> {
        if !self.show_last_prompt {
            return None;
        }
        let pane = self.displayed_pane_tmux_name()?;
        let text = self
            .last_prompt_cache
            .as_ref()
            .filter(|c| c.pane == pane)
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
