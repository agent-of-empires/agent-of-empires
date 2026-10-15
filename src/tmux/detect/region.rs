//! Named slices of a pane capture that manifest rules match against, so a
//! shape cannot drift onto unrelated text.

use std::sync::OnceLock;

/// The pane, sliced lazily once per capture.
pub(super) struct Screen<'a> {
    /// The most recent [`RECENT_LINES`] non-empty lines.
    recent: Vec<&'a str>,
    /// Whether a blank row sat above each [`Self::recent`] line.
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

const RECENT_LINES: usize = 30;

/// A landmark line (a turn divider, an input box rule, a banner) that regions
/// like `after(<name>)` and `above(<name>, n)` resolve through.
pub(super) struct Marker {
    pub(super) line_regex: Vec<regex::Regex>,
    pub(super) contains: Vec<String>,
    /// Which candidate, counting from the bottom.
    pub(super) occurrence: usize,
    /// Deeper candidates are transcript content; also the fallback window for `above`.
    pub(super) max_depth: Option<usize>,
    /// Join up to this many lines, for a banner a narrow pane wraps.
    pub(super) wrap: usize,
    /// Stripped from each line's front before matching.
    pub(super) strip_prefix: Option<String>,
    /// `after(<marker>)` with the marker absent: empty for a meaningful landmark,
    /// the whole window for one that only bounds a region.
    pub(super) absent_is_whole: bool,
}

impl Marker {
    /// Index of the marker line, or `None`. A wrapped marker resolves to the line
    /// where the phrase completes, growing forward from each candidate start.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Region {
    WholeRecent,
    /// Recent lines with whitespace runs collapsed, so a wrapped hint reads whole.
    CollapsedRecent,
    /// Recent lines with runs of single-character lines glued into words.
    UnstackedRecent,
    BottomLines(usize),
    /// The input box's status slot line (spinner, wait line or completion), box
    /// chrome skipped.
    AboveInputBox,
    PromptBoxBody,
    /// Below the last horizontal rule: the current dialog or footer.
    AfterLastRule,
    /// The OSC 0/2 terminal title.
    OscTitle,
    /// The `prompt_marker` line and below, else the whole window.
    FromPromptMarker,
    BeforePromptMarker,
    /// Below a named marker (exclusive), optionally its last `n` lines; empty when
    /// the marker is absent.
    AfterMarker(&'static str, Option<usize>),
    /// The `n` lines above a named marker, else its `max_depth` bottom lines.
    AboveMarker(&'static str, usize),
}

impl Region {
    /// Marker region names are leaked once at compile time (process lifetime).
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
            // Zero is a manifest bug, reported at build time by `manifests_compile`.
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

    /// `""` when the pane has no such slice; an empty region matches nothing.
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

/// A run of box-drawing dashes, optionally broken by a right-aligned label.
fn line_is_horizontal_rule(trimmed: &str) -> bool {
    trimmed.chars().take_while(|c| *c == '─').count() >= 3 && trimmed.ends_with('─')
}

/// Input-box furniture that [`Region::AboveInputBox`] skips; unlisted furniture
/// hides the status slot.
fn line_is_input_box_chrome(line: &str) -> bool {
    let trimmed = line.trim();
    line_is_horizontal_rule(trimmed)
        || (trimmed.starts_with('⎿') && trimmed.contains("Tip:"))
        || trimmed.starts_with("new task?")
        || line_is_mode_footer(trimmed)
        || line_is_update_banner(trimmed)
}

fn line_is_mode_footer(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    (trimmed.starts_with('⏵') || trimmed.starts_with('⏸'))
        && (lower.contains(" on") || lower.contains("shift+tab"))
}

/// Claude's self-update notice, which otherwise hides the completion line.
fn line_is_update_banner(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    (trimmed.starts_with('✔') || trimmed.starts_with('✓'))
        && lower.contains("update")
        && (lower.contains("restart") || lower.contains("installed"))
}

/// Glue runs of single-character lines (a narrow Textual pane stacking a word);
/// wider lines and blank rows end a run.
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
