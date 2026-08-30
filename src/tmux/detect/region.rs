//! Named slices of a pane capture that manifest rules match against.
//!
//! A rule names the part of the screen its shape can appear in, so a match
//! cannot drift onto unrelated text: the completion line is only meaningful
//! directly above the input box, and the interrupt hint only in the footer.
//! Naming the slice is what keeps the shapes themselves simple.

use std::sync::OnceLock;

/// The pane, sliced once per capture and shared by every rule that runs
/// against it. Regions are computed lazily: a capture whose first rule
/// matches never pays for the rest.
pub(super) struct Screen<'a> {
    /// Non-empty lines, most recent [`RECENT_LINES`] of them. Claude parks the
    /// cursor below its output and small responses sit in a tall pane, so the
    /// blank tail carries nothing and filtering it keeps window sizes
    /// meaningful.
    recent: Vec<&'a str>,
    osc_title: &'a str,
    joined: OnceLock<String>,
    collapsed: OnceLock<String>,
    above_input_box: OnceLock<Option<String>>,
    prompt_box_body: OnceLock<Option<String>>,
    after_last_rule: OnceLock<String>,
    from_marker: OnceLock<String>,
    before_marker: OnceLock<String>,
}

/// How far back a rule may look. Claude's footer, input box and status slot
/// fit well inside this, and a shorter window would drop the completion line
/// on a pane whose box carries several rows of chrome.
const RECENT_LINES: usize = 30;

/// What a rule matches against, resolved from its `region` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Region {
    /// Every recent line, joined.
    WholeRecent,
    /// Recent lines with runs of ASCII whitespace collapsed to one space, so a
    /// footer hint that word-wrapped mid-phrase still reads as one string.
    CollapsedRecent,
    /// The last `n` non-empty lines.
    BottomLines(usize),
    /// The transcript line the input box's status slot sits on: the live
    /// spinner, the background-agent wait line, or the completion line once a
    /// turn ends. Box chrome is skipped to reach it.
    AboveInputBox,
    /// The input box's own line, from `❯` to end of line.
    PromptBoxBody,
    /// Everything below the last horizontal rule, i.e. the current dialog or
    /// footer rather than the transcript above it.
    AfterLastRule,
    /// The terminal title the agent sets through OSC 0/2, which several CLIs
    /// use to publish their own state.
    OscTitle,
    /// The manifest's `prompt_marker` line and everything below it: the live
    /// area an agent redraws, as opposed to the transcript above. Falls back
    /// to the whole window when the marker is off screen.
    FromPromptMarker,
    /// Everything above the `prompt_marker` line.
    BeforePromptMarker,
}

impl Region {
    pub(super) fn parse(raw: &str) -> Option<Self> {
        if let Some(n) = raw
            .strip_prefix("bottom_non_empty_lines(")
            .and_then(|r| r.strip_suffix(')'))
        {
            return n.trim().parse().ok().map(Region::BottomLines);
        }
        Some(match raw {
            "whole_recent" => Region::WholeRecent,
            "collapsed_recent" => Region::CollapsedRecent,
            "last_non_empty_above_prompt_box" => Region::AboveInputBox,
            "prompt_box_body" => Region::PromptBoxBody,
            "after_last_horizontal_rule" => Region::AfterLastRule,
            "osc_title" => Region::OscTitle,
            "from_prompt_marker" => Region::FromPromptMarker,
            "before_prompt_marker" => Region::BeforePromptMarker,
            _ => return None,
        })
    }
}

impl<'a> Screen<'a> {
    pub(super) fn new(clean_screen: &'a str, osc_title: &'a str) -> Self {
        let non_empty: Vec<&str> = clean_screen
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let start = non_empty.len().saturating_sub(RECENT_LINES);
        Self {
            recent: non_empty[start..].to_vec(),
            osc_title,
            joined: OnceLock::new(),
            collapsed: OnceLock::new(),
            above_input_box: OnceLock::new(),
            prompt_box_body: OnceLock::new(),
            after_last_rule: OnceLock::new(),
            from_marker: OnceLock::new(),
            before_marker: OnceLock::new(),
        }
    }

    /// The region's text, or `""` when the pane has no such slice (no input
    /// box on screen, no title set). An empty region matches nothing, which is
    /// the safe direction: a rule that cannot see its evidence must not fire.
    pub(super) fn region_text(&self, region: Region, prompt_marker: &[regex::Regex]) -> &str {
        match region {
            Region::WholeRecent => self.joined(),
            Region::CollapsedRecent => self
                .collapsed
                .get_or_init(|| collapse_ascii_whitespace(self.joined())),
            Region::BottomLines(n) => {
                // Bottom-n is a suffix of the joined recent window, so it is
                // sliced from it rather than joined again per rule.
                let start = self.recent.len().saturating_sub(n);
                if start == 0 {
                    return self.joined();
                }
                let skipped: usize = self.recent[..start].iter().map(|l| l.len() + 1).sum();
                &self.joined()[skipped..]
            }
            Region::AboveInputBox => self
                .above_input_box
                .get_or_init(|| self.compute_above_input_box())
                .as_deref()
                .unwrap_or(""),
            Region::PromptBoxBody => self
                .prompt_box_body
                .get_or_init(|| self.compute_prompt_box_body())
                .as_deref()
                .unwrap_or(""),
            Region::AfterLastRule => self
                .after_last_rule
                .get_or_init(|| self.compute_after_last_rule()),
            Region::OscTitle => self.osc_title,
            Region::FromPromptMarker => {
                self.from_marker
                    .get_or_init(|| match self.marker_index(prompt_marker) {
                        Some(idx) => self.recent[idx..].join("\n"),
                        None => self.joined().to_string(),
                    })
            }
            Region::BeforePromptMarker => {
                self.before_marker
                    .get_or_init(|| match self.marker_index(prompt_marker) {
                        Some(idx) => self.recent[..idx].join("\n"),
                        None => self.joined().to_string(),
                    })
            }
        }
    }

    /// The last line matching any of the manifest's prompt-marker patterns.
    fn marker_index(&self, prompt_marker: &[regex::Regex]) -> Option<usize> {
        self.recent
            .iter()
            .rposition(|line| prompt_marker.iter().any(|re| re.is_match(line)))
    }

    fn joined(&self) -> &str {
        self.joined.get_or_init(|| self.recent.join("\n"))
    }

    fn compute_above_input_box(&self) -> Option<String> {
        let box_top = self
            .recent
            .iter()
            .rposition(|l| l.trim_start().starts_with('❯'))
            .unwrap_or(self.recent.len());
        self.recent[..box_top]
            .iter()
            .rev()
            .find(|l| !line_is_input_box_chrome(l))
            .map(|l| (*l).to_string())
    }

    fn compute_prompt_box_body(&self) -> Option<String> {
        self.recent
            .iter()
            .rposition(|l| l.trim_start().starts_with('❯'))
            .map(|idx| self.recent[idx].to_string())
    }

    fn compute_after_last_rule(&self) -> String {
        match self
            .recent
            .iter()
            .rposition(|l| line_is_horizontal_rule(l.trim()))
        {
            Some(idx) => self.recent[idx + 1..].join("\n"),
            None => self.joined().to_string(),
        }
    }
}

/// A run of box-drawing dashes, optionally broken by the right-aligned label
/// an agent renders inside it. Requiring a leading run *and* a trailing dash
/// separates it from transcript prose that merely contains a rule.
fn line_is_horizontal_rule(trimmed: &str) -> bool {
    trimmed.chars().take_while(|c| *c == '─').count() >= 3 && trimmed.ends_with('─')
}

/// Input-box furniture as opposed to transcript content: the box's own
/// separators, `⎿ Tip:` rows, the right-aligned context hint, and the mode
/// footer under the box. [`Region::AboveInputBox`] skips these to reach the
/// status slot; a shape missing here reads as transcript and hides the
/// evidence behind it, so new furniture belongs in this list.
fn line_is_input_box_chrome(line: &str) -> bool {
    let trimmed = line.trim();
    line_is_horizontal_rule(trimmed)
        || (trimmed.starts_with('⎿') && trimmed.contains("Tip:"))
        || trimmed.starts_with("new task?")
        || line_is_mode_footer(trimmed)
        || line_is_update_banner(trimmed)
}

/// The mode/permission footer under the input box (`⏵⏵ accept edits on`,
/// `⏸ plan mode on`), including the shortcut tail it carries.
fn line_is_mode_footer(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    (trimmed.starts_with('⏵') || trimmed.starts_with('⏸'))
        && (lower.contains(" on") || lower.contains("shift+tab"))
}

/// The self-update notice Claude renders between the transcript and the input
/// box (`✔ Update installed · Restart to update`). It is chrome, not
/// transcript: leaving it out of this list let it stand in for the status slot
/// and hide a finished turn's completion line, pinning parked sessions on
/// Running for hours.
fn line_is_update_banner(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    (trimmed.starts_with('✔') || trimmed.starts_with('✓'))
        && lower.contains("update")
        && (lower.contains("restart") || lower.contains("installed"))
}

pub(super) fn collapse_ascii_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if c.is_ascii_whitespace() {
            in_ws = true;
            continue;
        }
        if in_ws && !out.is_empty() {
            out.push(' ');
        }
        in_ws = false;
        out.push(c);
    }
    out
}
