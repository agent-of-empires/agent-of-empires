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
    /// Whether a blank row sat above each [`Self::recent`] line. Blank rows are
    /// dropped from the window, so this is the only thing left that says two
    /// stacked characters came from different blocks rather than one word.
    blank_before: Vec<bool>,
    osc_title: &'a str,
    joined: OnceLock<String>,
    collapsed: OnceLock<String>,
    unstacked: OnceLock<String>,
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

/// A landmark line a manifest names so its rules can be scoped relative to it:
/// the divider a finished turn draws, the rule pair around an input box, an
/// interruption banner. Regions like `after(<name>)` and `above(<name>, n)`
/// resolve through one of these.
pub(super) struct Marker {
    /// Any of these matching a line makes it a candidate.
    pub(super) line_regex: Vec<regex::Regex>,
    /// Case-insensitive substrings, all of which must appear in the candidate.
    pub(super) contains: Vec<String>,
    /// Which candidate to take, counting from the bottom. Pi's input box is
    /// its *second* rule from the bottom, since it draws one above and one
    /// below the prompt.
    pub(super) occurrence: usize,
    /// A candidate further from the bottom than this is transcript content
    /// drawing the same shape, not the landmark, and resolves to absent.
    /// Doubles as the fallback window for `above`.
    pub(super) max_depth: Option<usize>,
    /// Join up to this many consecutive lines before matching, for a banner a
    /// narrow pane wraps. The marker resolves to the last line of the window.
    pub(super) wrap: usize,
    /// Dropped from the front of each line before matching, so a banner glyph
    /// does not have to appear in every pattern.
    pub(super) strip_prefix: Option<String>,
    /// What `after(<marker>)` means when the marker is not on screen. A
    /// landmark whose absence is meaningful (an interruption banner) leaves
    /// the region empty, so a rule scoped to it cannot fire. One that only
    /// bounds a region (the divider between turns) leaves the whole window,
    /// since with no divider the current block *is* the whole window.
    pub(super) absent_is_whole: bool,
}

impl Marker {
    /// Index into `lines` of the marker, or `None` when it is absent, deeper
    /// than `max_depth`, or occurs fewer than `occurrence` times.
    ///
    /// A wrapped marker is located by growing a window *forward* from each
    /// candidate start and taking the line where the phrase completes, not by
    /// asking which windows contain it: every window that reaches back far
    /// enough contains it, so the latter resolves to the bottom of the pane
    /// and leaves `after(<marker>)` empty.
    fn resolve(&self, lines: &[&str]) -> Option<usize> {
        let body = |line: &str| -> String {
            let trimmed = line.trim_start();
            match &self.strip_prefix {
                Some(p) => trimmed
                    .strip_prefix(p.as_str())
                    .unwrap_or(trimmed)
                    .trim_start(),
                None => trimmed,
            }
            .to_string()
        };
        let hit = |joined: &str| {
            let collapsed = collapse_ascii_whitespace(joined);
            let lower = collapsed.to_lowercase();
            (self.line_regex.is_empty() || self.line_regex.iter().any(|r| r.is_match(&collapsed)))
                && self.contains.iter().all(|c| lower.contains(c.as_str()))
        };
        let mut seen = 0;
        for start in (0..lines.len()).rev() {
            let last = (start + self.wrap).min(lines.len());
            let mut joined = String::new();
            for (end, line) in lines.iter().enumerate().take(last).skip(start) {
                if !joined.is_empty() {
                    joined.push(' ');
                }
                joined.push_str(&body(line));
                if !hit(&joined) {
                    continue;
                }
                seen += 1;
                if seen < self.occurrence {
                    break;
                }
                let depth = lines.len() - end;
                return match self.max_depth {
                    Some(max) if depth > max => None,
                    _ => Some(end),
                };
            }
        }
        None
    }
}

/// What a rule matches against, resolved from its `region` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Region {
    /// Every recent line, joined.
    WholeRecent,
    /// Recent lines with runs of ASCII whitespace collapsed to one space, so a
    /// footer hint that word-wrapped mid-phrase still reads as one string.
    CollapsedRecent,
    /// Recent lines with runs of single-character lines glued back into one
    /// line, so a word a Textual pane stacks one character per row reads as a
    /// word. Wider lines keep their newline, so this cannot spell a word out
    /// of two ordinary lines that only meet at their ends.
    UnstackedRecent,
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
    /// Everything below a named marker, exclusive, optionally limited to that
    /// slice's own last `n` lines. Empty when the marker is absent, so a rule
    /// scoped to it cannot fire without its landmark.
    AfterMarker(&'static str, Option<usize>),
    /// The `n` lines directly above a named marker, falling back to the
    /// marker's `max_depth` lines at the bottom when it is absent.
    AboveMarker(&'static str, usize),
}

impl Region {
    /// Region names carrying a marker are leaked once at manifest compile
    /// time, which happens on a `OnceLock` path that lives for the process.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        if let Some(rest) = raw.strip_prefix("after(").and_then(|r| r.strip_suffix(')')) {
            let (name, limit) = match rest.split_once(',') {
                Some((name, n)) => (name, Some(n.trim().parse().ok()?)),
                None => (rest, None),
            };
            return Some(Region::AfterMarker(
                Box::leak(name.trim().to_string().into_boxed_str()),
                limit,
            ));
        }
        if let Some(rest) = raw.strip_prefix("above(").and_then(|r| r.strip_suffix(')')) {
            let (name, n) = rest.split_once(',')?;
            return Some(Region::AboveMarker(
                Box::leak(name.trim().to_string().into_boxed_str()),
                n.trim().parse().ok()?,
            ));
        }
        if let Some(n) = raw
            .strip_prefix("bottom_non_empty_lines(")
            .and_then(|r| r.strip_suffix(')'))
        {
            // Zero is rejected rather than clamped: an empty region matches
            // nothing, so a rule asking for it is a manifest bug, and
            // `manifests_compile` names the rule at build time.
            return n
                .trim()
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .map(Region::BottomLines);
        }
        Some(match raw {
            "whole_recent" => Region::WholeRecent,
            "collapsed_recent" => Region::CollapsedRecent,
            "unstacked_recent" => Region::UnstackedRecent,
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
        let mut non_empty: Vec<&str> = Vec::new();
        let mut blank_before: Vec<bool> = Vec::new();
        let mut after_blank = false;
        for line in clean_screen.lines() {
            if line.trim().is_empty() {
                after_blank = true;
                continue;
            }
            non_empty.push(line);
            blank_before.push(std::mem::take(&mut after_blank));
        }
        let start = non_empty.len().saturating_sub(RECENT_LINES);
        Self {
            recent: non_empty[start..].to_vec(),
            blank_before: blank_before[start..].to_vec(),
            osc_title,
            joined: OnceLock::new(),
            collapsed: OnceLock::new(),
            unstacked: OnceLock::new(),
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
    pub(super) fn region_text(
        &self,
        region: Region,
        prompt_marker: &[regex::Regex],
        markers: &std::collections::HashMap<String, Marker>,
    ) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(match region {
            Region::WholeRecent => self.joined(),
            Region::CollapsedRecent => self
                .collapsed
                .get_or_init(|| collapse_ascii_whitespace(self.joined())),
            Region::UnstackedRecent => self
                .unstacked
                .get_or_init(|| unstack(&self.recent, &self.blank_before)),
            Region::BottomLines(n) => {
                // Bottom-n is a suffix of the joined recent window, so it is
                // sliced from it rather than joined again per rule.
                let start = self.recent.len().saturating_sub(n);
                if start == 0 {
                    return std::borrow::Cow::Borrowed(self.joined());
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
            // The marker regions are built per lookup rather than cached: a
            // manifest scopes only a handful of rules to a marker, and the
            // capture they run against is discarded at the end of the poll.
            Region::AfterMarker(name, limit) => {
                return std::borrow::Cow::Owned(match markers.get(name) {
                    Some(marker) => match marker.resolve(&self.recent) {
                        Some(idx) => {
                            let after = &self.recent[idx + 1..];
                            let start = limit.map_or(0, |n| after.len().saturating_sub(n));
                            after[start..].join("\n")
                        }
                        None if marker.absent_is_whole => {
                            let start = limit.map_or(0, |n| self.recent.len().saturating_sub(n));
                            self.recent[start..].join("\n")
                        }
                        None => String::new(),
                    },
                    None => String::new(),
                })
            }
            Region::AboveMarker(name, n) => {
                let marker = markers.get(name);
                return std::borrow::Cow::Owned(
                    match marker.and_then(|m| m.resolve(&self.recent)) {
                        Some(idx) => self.recent[idx.saturating_sub(n)..idx].join("\n"),
                        None => {
                            let window = marker.and_then(|m| m.max_depth).unwrap_or(n);
                            self.recent[self.recent.len().saturating_sub(window)..].join("\n")
                        }
                    },
                );
            }
        })
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

/// Glue each run of single-character lines into one line, leaving wider lines
/// alone. A Textual pane too narrow for its status word stacks the word one
/// character per row, and a run of such rows is the word; anything wider is
/// ordinary content, so it keeps its own line and cannot be glued to the text
/// under it.
///
/// A blank row ends a run for the same reason: characters either side of one
/// came from different blocks, so they are not one word.
fn unstack(lines: &[&str], blank_before: &[bool]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run = String::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let stacked = trimmed.chars().count() == 1;
        if (!stacked || blank_before[i]) && !run.is_empty() {
            out.push(std::mem::take(&mut run));
        }
        if stacked {
            run.push_str(trimmed);
        } else {
            out.push((*line).to_string());
        }
    }
    if !run.is_empty() {
        out.push(run);
    }
    out.join("\n")
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
