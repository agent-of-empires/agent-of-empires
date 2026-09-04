//! Manifest schema, compilation, and evaluation.
//!
//! A manifest is one agent's detection rules as data. Each rule names the
//! state it asserts, the [`Region`] it looks at, a priority, and a matcher.
//! Every rule that matches is collected and the highest priority wins, so
//! adding a case means adding a row rather than threading another branch
//! through a chain of detectors.

use serde::Deserialize;

use super::region::{Marker, Region, Screen};
use crate::session::Status;

/// A rule as written in TOML.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct RawRule {
    /// Empty for a shared template, which is named by its table key.
    #[serde(default)]
    pub(super) id: String,
    /// A template in `manifests/shared.toml` to build on. Scalars set here win
    /// over the template's; list fields concatenate, so an agent adds its own
    /// phrases to the shared ones rather than restating them.
    #[serde(default)]
    pub(super) extends: Option<String>,
    /// Omitted by rules that only carry `skip_state_update`.
    #[serde(default)]
    pub(super) state: Option<String>,
    /// Templates carry no priority: each agent ranks the shape itself.
    #[serde(default)]
    pub(super) priority: i32,
    #[serde(default)]
    pub(super) region: Option<String>,
    /// The pane visibly shows this state's own chrome, as opposed to the state
    /// being inferred. The poller uses it to publish a transition immediately
    /// instead of waiting for a second agreeing poll.
    #[serde(default)]
    pub(super) visible: bool,
    /// The screen is an agent-owned viewer (a transcript pager, a model
    /// picker) that shows history rather than live state, so the last known
    /// status must stand.
    #[serde(default)]
    pub(super) skip_state_update: bool,
    /// Freshness bound for a `hook` rule: past it the rule cannot fire and
    /// lower-priority evidence decides.
    #[serde(default)]
    pub(super) max_age_secs: Option<u64>,
    /// Arbitrate this rule against its priority peers by where it matches
    /// rather than by rank: of the positional rules sharing a priority, the
    /// one matching lowest on screen wins. Some agents stack their state
    /// markers, so the bottom-most one is the current one and a fixed ranking
    /// cannot express it.
    #[serde(default)]
    pub(super) positional: bool,
    /// Match against the join of up to this many consecutive lines, for a
    /// marker a narrow pane wraps. Positional rules only.
    #[serde(default = "one")]
    pub(super) wrap: usize,
    /// How far above the bottom the match may sit. A wrapped rule needs a
    /// region one line deeper than its real window so the joined lines are
    /// available; this keeps the match itself inside the window.
    #[serde(default)]
    pub(super) max_position: Option<usize>,
    #[serde(flatten)]
    pub(super) matcher: RawMatcher,
}

/// The match forms a rule (or a nested clause) may carry. Several may be set
/// at once, in which case all of them must hold.
#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct RawMatcher {
    /// Case-insensitive substrings, all of which must appear.
    #[serde(default)]
    pub(super) contains: Vec<String>,
    /// Case-insensitive substrings, at least one of which must appear. The
    /// common shape by far, and `any` with one `contains` per clause said the
    /// same thing five times as long.
    #[serde(default)]
    pub(super) contains_any: Vec<String>,
    /// Regexes over the region text, all of which must match.
    #[serde(default)]
    pub(super) regex: Vec<String>,
    /// Regexes tested per line; each must match at least one line, not
    /// necessarily the same one.
    #[serde(default)]
    pub(super) line_regex: Vec<String>,
    /// One line that matches every `regex` and none of `not_regex`. The
    /// per-line negation is what `not` cannot express: a rule that fires on a
    /// live activity line must not be silenced by a *different* line that
    /// happens to report a finished one.
    #[serde(default)]
    pub(super) line: Option<RawLineClause>,
    /// At least one clause must match.
    #[serde(default)]
    pub(super) any: Vec<RawMatcher>,
    /// Every clause must match.
    #[serde(default)]
    pub(super) all: Vec<RawMatcher>,
    /// No clause may match.
    #[serde(default)]
    pub(super) not: Vec<RawMatcher>,
    /// For `region = "hook"`: the status the hook file must carry.
    #[serde(default)]
    pub(super) hook_status: Option<String>,
    /// Evaluate this clause (and anything nested in it) against a different
    /// region than the rule's own. A prompt whose evidence spans two places on
    /// screen, like Codex's plan dialog, cannot be written any other way.
    #[serde(default)]
    pub(super) region: Option<String>,
    /// The `line_regex` patterns must match at least this many distinct lines.
    #[serde(default)]
    pub(super) min_lines: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct RawLineClause {
    #[serde(default)]
    pub(super) regex: Vec<String>,
    #[serde(default)]
    pub(super) not_regex: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawMarker {
    #[serde(default)]
    pub(super) line_regex: Vec<String>,
    #[serde(default)]
    pub(super) contains: Vec<String>,
    #[serde(default = "one")]
    pub(super) occurrence: usize,
    #[serde(default)]
    pub(super) max_depth: Option<usize>,
    #[serde(default = "one")]
    pub(super) wrap: usize,
    #[serde(default)]
    pub(super) strip_prefix: Option<String>,
    #[serde(default)]
    pub(super) absent_is_whole: bool,
}

fn one() -> usize {
    1
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    id: String,
    /// Landmark lines the rules scope themselves to; see [`Marker`].
    #[serde(default)]
    markers: std::collections::HashMap<String, RawMarker>,
    /// Lines that mark the start of the agent's live area, so rules can name
    /// it (`from_prompt_marker`) instead of matching against a transcript that
    /// still holds the previous turn's chrome.
    #[serde(default)]
    prompt_marker: Vec<String>,
    rules: Vec<RawRule>,
}

/// A compiled matcher. Regexes are compiled once at startup; substrings are
/// pre-lowered, so evaluation is substring and regex work only.
/// What a nested clause needs to resolve a region of its own.
pub(super) struct MatchContext<'a> {
    pub(super) screen: &'a Screen<'a>,
    pub(super) prompt_marker: &'a [regex::Regex],
    pub(super) markers: &'a std::collections::HashMap<String, Marker>,
}

pub(super) struct Matcher {
    region: Option<Region>,
    min_lines: Option<usize>,
    contains: Vec<String>,
    contains_any: Vec<String>,
    regex: Vec<regex::Regex>,
    line_regex: Vec<regex::Regex>,
    line: Option<(Vec<regex::Regex>, Vec<regex::Regex>)>,
    any: Vec<Matcher>,
    all: Vec<Matcher>,
    not: Vec<Matcher>,
    hook_status: Option<Status>,
}

pub(super) struct Rule {
    pub(super) id: String,
    pub(super) positional: bool,
    pub(super) wrap: usize,
    max_position: Option<usize>,
    pub(super) state: Option<Status>,
    pub(super) priority: i32,
    pub(super) region: Region,
    pub(super) visible: bool,
    pub(super) skip_state_update: bool,
    pub(super) max_age: Option<std::time::Duration>,
    matcher: Matcher,
    /// `region = "hook"` is not a slice of the screen, so it is kept out of
    /// [`Region`] and flagged here.
    pub(super) is_hook: bool,
}

pub(super) struct Manifest {
    pub(super) id: String,
    prompt_marker: Vec<regex::Regex>,
    markers: std::collections::HashMap<String, Marker>,
    /// Sorted by descending priority, so evaluation stops at the first match.
    pub(super) rules: Vec<Rule>,
}

/// What the hook file says, when a session has one.
#[derive(Debug, Clone, Copy)]
pub struct HookObservation {
    pub status: Status,
    pub age: Option<std::time::Duration>,
}

fn parse_status(raw: &str) -> Option<Status> {
    Some(match raw {
        "idle" => Status::Idle,
        "running" => Status::Running,
        "waiting" => Status::Waiting,
        "error" => Status::Error,
        _ => return None,
    })
}

impl Matcher {
    fn compile(raw: &RawMatcher, rule_id: &str) -> anyhow::Result<Self> {
        let compile_all = |patterns: &[String]| -> anyhow::Result<Vec<regex::Regex>> {
            patterns
                .iter()
                .map(|p| {
                    regex::Regex::new(p)
                        .map_err(|e| anyhow::anyhow!("rule {rule_id}: invalid regex {p:?}: {e}"))
                })
                .collect()
        };
        Ok(Self {
            region: match &raw.region {
                Some(r) => Some(Region::parse(r).ok_or_else(|| {
                    anyhow::anyhow!("rule {rule_id}: unknown clause region {r:?}")
                })?),
                None => None,
            },
            min_lines: raw.min_lines,
            contains: raw.contains.iter().map(|c| c.to_lowercase()).collect(),
            contains_any: raw.contains_any.iter().map(|c| c.to_lowercase()).collect(),
            regex: compile_all(&raw.regex)?,
            line_regex: compile_all(&raw.line_regex)?,
            line: match &raw.line {
                Some(clause) => {
                    Some((compile_all(&clause.regex)?, compile_all(&clause.not_regex)?))
                }
                None => None,
            },
            any: raw
                .any
                .iter()
                .map(|m| Matcher::compile(m, rule_id))
                .collect::<anyhow::Result<_>>()?,
            all: raw
                .all
                .iter()
                .map(|m| Matcher::compile(m, rule_id))
                .collect::<anyhow::Result<_>>()?,
            not: raw
                .not
                .iter()
                .map(|m| Matcher::compile(m, rule_id))
                .collect::<anyhow::Result<_>>()?,
            hook_status: match &raw.hook_status {
                Some(s) => {
                    Some(parse_status(s).ok_or_else(|| {
                        anyhow::anyhow!("rule {rule_id}: unknown hook_status {s:?}")
                    })?)
                }
                None => None,
            },
        })
    }

    /// Whether every form this matcher carries holds. `text`/`lower` are the
    /// enclosing region; a clause naming its own `region` re-slices the screen
    /// instead. An empty matcher matches, which is what lets a rule be pure
    /// `not` clauses.
    fn matches(&self, text: &str, lower: &str, ctx: &MatchContext) -> bool {
        match self.region {
            Some(region) => {
                let sliced = ctx
                    .screen
                    .region_text(region, ctx.prompt_marker, ctx.markers);
                let lowered = sliced.to_lowercase();
                self.matches_in(&sliced, &lowered, ctx)
            }
            None => self.matches_in(text, lower, ctx),
        }
    }

    fn matches_in(&self, text: &str, lower: &str, ctx: &MatchContext) -> bool {
        self.contains.iter().all(|c| lower.contains(c.as_str()))
            && (self.contains_any.is_empty()
                || self.contains_any.iter().any(|c| lower.contains(c.as_str())))
            && self.regex.iter().all(|r| r.is_match(text))
            && self.line_regex.iter().all(|r| {
                let hits = text.lines().filter(|line| r.is_match(line)).count();
                hits >= self.min_lines.unwrap_or(1)
            })
            && self.line.as_ref().is_none_or(|(want, reject)| {
                text.lines().any(|line| {
                    want.iter().all(|r| r.is_match(line))
                        && !reject.iter().any(|r| r.is_match(line))
                })
            })
            && (self.any.is_empty() || self.any.iter().any(|m| m.matches(text, lower, ctx)))
            && self.all.iter().all(|m| m.matches(text, lower, ctx))
            && !self.not.iter().any(|m| m.matches(text, lower, ctx))
    }
}

impl Rule {
    fn compile(raw: RawRule) -> anyhow::Result<Self> {
        let region_name = raw.region.clone().ok_or_else(|| {
            anyhow::anyhow!("rule {}: no region, and no template gave one", raw.id)
        })?;
        let is_hook = region_name == "hook";
        let region = if is_hook {
            Region::WholeRecent
        } else {
            Region::parse(&region_name).ok_or_else(|| {
                anyhow::anyhow!("rule {}: unknown region {:?}", raw.id, region_name)
            })?
        };
        let state = match &raw.state {
            Some(s) => Some(
                parse_status(s)
                    .ok_or_else(|| anyhow::anyhow!("rule {}: unknown state {:?}", raw.id, s))?,
            ),
            None => None,
        };
        if state.is_none() && !raw.skip_state_update {
            anyhow::bail!("rule {}: needs a state or skip_state_update", raw.id);
        }
        let matcher = Matcher::compile(&raw.matcher, &raw.id)?;
        Ok(Self {
            id: raw.id,
            positional: raw.positional,
            wrap: raw.wrap.max(1),
            max_position: raw.max_position,
            state,
            priority: raw.priority,
            region,
            visible: raw.visible,
            skip_state_update: raw.skip_state_update,
            max_age: raw.max_age_secs.map(std::time::Duration::from_secs),
            matcher,
            is_hook,
        })
    }

    fn matches(
        &self,
        screen: &Screen,
        hook: Option<HookObservation>,
        prompt_marker: &[regex::Regex],
        markers: &std::collections::HashMap<String, Marker>,
    ) -> bool {
        let ctx = MatchContext {
            screen,
            prompt_marker,
            markers,
        };
        if self.is_hook {
            let Some(hook) = hook else {
                return false;
            };
            if self.matcher.hook_status != Some(hook.status) {
                return false;
            }
            // A hook write nobody has refreshed within the bound is not
            // evidence of anything: the agent's terminating hook can be lost
            // (a turn that ends on a tool result fires none), and without a
            // bound that lost write outranks the screen indefinitely.
            let fresh = match (self.max_age, hook.age) {
                (Some(max), Some(age)) => age < max,
                // An unreadable mtime is missing evidence, not evidence of
                // staleness, so the bound does not fire on it.
                (Some(_), None) => true,
                (None, _) => true,
            };
            // A hook rule's clauses still apply, and they carry their own
            // regions: `hook` is a source of evidence rather than a slice of
            // screen, so there is no text to match here beyond what a clause
            // asks for itself.
            return fresh && self.matcher.matches("", "", &ctx);
        }
        let text = screen.region_text(self.region, prompt_marker, markers);
        if text.is_empty() {
            return false;
        }
        let lower = text.to_lowercase();
        self.matcher.matches(&text, &lower, &ctx)
    }
}

/// Rule templates shared across manifests, keyed by name.
const SHARED_TEMPLATES: &str = include_str!("manifests/shared.toml");

#[derive(Debug, Deserialize)]
struct RawTemplates {
    templates: std::collections::HashMap<String, RawRule>,
}

impl RawRule {
    /// Fold a template into this rule: scalars already set here win, lists
    /// concatenate so an agent extends the shared phrases rather than
    /// restating them.
    fn inherit(&mut self, base: &RawRule) {
        self.state = self.state.take().or_else(|| base.state.clone());
        self.region = self.region.take().or_else(|| base.region.clone());
        self.visible |= base.visible;
        self.skip_state_update |= base.skip_state_update;
        self.positional |= base.positional;
        self.max_age_secs = self.max_age_secs.or(base.max_age_secs);
        self.max_position = self.max_position.or(base.max_position);
        if self.wrap == 1 {
            self.wrap = base.wrap;
        }
        self.matcher.inherit(&base.matcher);
    }
}

impl RawMatcher {
    fn inherit(&mut self, base: &RawMatcher) {
        self.contains.extend(base.contains.iter().cloned());
        self.contains_any.extend(base.contains_any.iter().cloned());
        self.regex.extend(base.regex.iter().cloned());
        self.line_regex.extend(base.line_regex.iter().cloned());
        self.not.extend(base.not.iter().cloned());
        self.all.extend(base.all.iter().cloned());
        self.any.extend(base.any.iter().cloned());
        if self.line.is_none() {
            self.line = base.line.clone();
        }
        self.hook_status = self.hook_status.take().or_else(|| base.hook_status.clone());
        self.min_lines = self.min_lines.or(base.min_lines);
    }
}

/// The hook rules every agent shares, so ten manifests do not carry ten
/// copies of the same five rows. They rank below the screen rules that read
/// state off live chrome and above the ones that only guess, which is the
/// arrangement the whole design turns on. A manifest that needs different
/// bounds declares its own rule with the same id and wins.
const SHARED_HOOK_RULES: &str = r#"
# A `waiting` write speaks only for a capture with nothing in it.
#
# Several agents write `waiting` the moment a prompt appears, and
# Esc-cancelling that prompt fires no clearing hook, so the file sticks on
# `waiting` until the next turn (#2937). The screen releases it: a prompt still
# up matches a blocking-prompt rule, a parked pane matches an idle one, and an
# unrecognised pane is still better evidence than a write nothing will clear,
# so it falls to the default. The write speaks for the one case the screen
# cannot, a capture that came back empty.
#
# Only one hook rule can fire for a given write, so the ranking among the hook
# rules is inert; every number here is a ranking against the screen.
[[rules]]
id = "hook_waiting"
state = "waiting"
priority = 240
region = "hook"
hook_status = "waiting"
not = [{ region = "whole_recent", regex = ['\S'] }]

# Younger than the poll's own settling time: a turn that has just started
# still shows the previous turn's parked chrome.
[[rules]]
id = "hook_running_fresh"
state = "running"
priority = 600
region = "hook"
hook_status = "running"
max_age_secs = 30

# Older than that, it still beats no evidence at all, but positive parked
# evidence on screen wins. Bounded, because a turn that ends on a tool result
# fires no terminating hook and an unbounded write then outranks every later
# capture for the life of the session.
[[rules]]
id = "hook_running_standing"
state = "running"
priority = 400
region = "hook"
hook_status = "running"
max_age_secs = 900

[[rules]]
id = "hook_idle"
state = "idle"
priority = 300
region = "hook"
hook_status = "idle"

[[rules]]
id = "hook_error"
state = "error"
priority = 300
region = "hook"
hook_status = "error"
"#;

impl Rule {
    /// How far above the bottom of its region this rule matches, counting the
    /// bottom line as 1. `None` when it does not match at all.
    fn match_position(
        &self,
        screen: &Screen,
        prompt_marker: &[regex::Regex],
        markers: &std::collections::HashMap<String, Marker>,
    ) -> Option<usize> {
        let text = screen.region_text(self.region, prompt_marker, markers);
        let lines: Vec<&str> = text.lines().collect();
        let ctx = MatchContext {
            screen,
            prompt_marker,
            markers,
        };
        (0..lines.len()).rev().find_map(|idx| {
            let start = idx + 1 - self.wrap.min(idx + 1);
            // Joined with newlines so a rule can still speak about the
            // individual lines of a wrapped match: `line_regex` tests each,
            // and `\s` in a plain regex spans the break.
            let window = lines[start..=idx].join("\n");
            let lower = window.to_lowercase();
            let position = lines.len() - idx;
            (self.max_position.is_none_or(|max| position <= max)
                && self.matcher.matches(&window, &lower, &ctx))
            .then_some(position)
        })
    }
}

impl Manifest {
    pub(super) fn parse(source: &str) -> anyhow::Result<Self> {
        let mut raw: RawManifest = toml::from_str(source)?;
        let templates: RawTemplates = toml::from_str(SHARED_TEMPLATES)?;
        for rule in &mut raw.rules {
            let Some(name) = rule.extends.clone() else {
                continue;
            };
            let base = templates.templates.get(&name).ok_or_else(|| {
                anyhow::anyhow!("rule {}: no shared template named {name:?}", rule.id)
            })?;
            rule.inherit(base);
        }
        let shared: RawManifest = toml::from_str(&format!("id = \"shared\"\n{SHARED_HOOK_RULES}"))?;
        let declared: std::collections::HashSet<&str> =
            raw.rules.iter().map(|r| r.id.as_str()).collect();
        let inherited: Vec<RawRule> = shared
            .rules
            .into_iter()
            .filter(|r| !declared.contains(r.id.as_str()))
            .collect();
        let mut rules = raw
            .rules
            .into_iter()
            .chain(inherited)
            .map(Rule::compile)
            .collect::<anyhow::Result<Vec<_>>>()?;
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        let prompt_marker = raw
            .prompt_marker
            .iter()
            .map(|p| {
                regex::Regex::new(p)
                    .map_err(|e| anyhow::anyhow!("prompt_marker {p:?} is not a valid regex: {e}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let markers = raw
            .markers
            .into_iter()
            .map(|(name, m)| {
                Ok((
                    name.clone(),
                    Marker {
                        line_regex: m
                            .line_regex
                            .iter()
                            .map(|p| {
                                regex::Regex::new(p).map_err(|e| {
                                    anyhow::anyhow!("marker {name}: invalid regex {p:?}: {e}")
                                })
                            })
                            .collect::<anyhow::Result<Vec<_>>>()?,
                        contains: m.contains.iter().map(|c| c.to_lowercase()).collect(),
                        occurrence: m.occurrence.max(1),
                        max_depth: m.max_depth,
                        wrap: m.wrap.max(1),
                        strip_prefix: m.strip_prefix,
                        absent_is_whole: m.absent_is_whole,
                    },
                ))
            })
            .collect::<anyhow::Result<std::collections::HashMap<_, _>>>()?;
        Ok(Self {
            id: raw.id,
            prompt_marker,
            markers,
            rules,
        })
    }

    #[cfg(test)]
    pub(super) fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    #[cfg(test)]
    pub(super) fn rule_matches(
        &self,
        id: &str,
        screen: &Screen,
        hook: Option<HookObservation>,
    ) -> bool {
        self.rule(id)
            .is_some_and(|r| r.matches(screen, hook, &self.prompt_marker, &self.markers))
    }

    /// The rule that decides, if any.
    ///
    /// Rules are tried in descending priority. Positional rules sharing a
    /// priority are one group: every member is evaluated and the one matching
    /// lowest on screen wins, since for those agents the bottom-most marker is
    /// the current one.
    pub(super) fn evaluate(&self, screen: &Screen, hook: Option<HookObservation>) -> Option<&Rule> {
        let mut i = 0;
        while i < self.rules.len() {
            let rule = &self.rules[i];
            if !rule.positional {
                if rule.matches(screen, hook, &self.prompt_marker, &self.markers) {
                    return Some(rule);
                }
                i += 1;
                continue;
            }
            let group_end = self.rules[i..]
                .iter()
                .position(|r| !r.positional || r.priority != rule.priority)
                .map_or(self.rules.len(), |offset| i + offset);
            let winner = self.rules[i..group_end]
                .iter()
                .filter_map(|r| {
                    r.match_position(screen, &self.prompt_marker, &self.markers)
                        .map(|pos| (pos, r))
                })
                .min_by_key(|(pos, _)| *pos)
                .map(|(_, r)| r);
            if winner.is_some() {
                return winner;
            }
            i = group_end;
        }
        None
    }
}
