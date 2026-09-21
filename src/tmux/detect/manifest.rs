//! Manifest schema, compilation, and evaluation.

use serde::Deserialize;

use super::region::{Marker, Region, Screen};
use crate::session::Status;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct RawRule {
    /// Empty for a shared template, which is named by its table key.
    #[serde(default)]
    pub(super) id: String,
    /// A `shared.toml` template: scalars here win, lists concatenate.
    #[serde(default)]
    pub(super) extends: Option<String>,
    #[serde(default)]
    pub(super) state: Option<String>,
    #[serde(default)]
    pub(super) priority: i32,
    #[serde(default)]
    pub(super) region: Option<String>,
    /// Read off the state's own chrome; published without waiting for a second poll.
    #[serde(default)]
    pub(super) visible: bool,
    /// An agent-owned viewer (pager, picker): the last known status stands.
    #[serde(default)]
    pub(super) skip_state_update: bool,
    /// For `hook` rules: past this age the rule cannot fire.
    #[serde(default)]
    pub(super) max_age_secs: Option<u64>,
    /// Among positional peers of equal priority, the lowest match on screen wins
    /// (for agents whose markers stack).
    #[serde(default)]
    pub(super) positional: bool,
    /// Join up to this many lines, for a marker a narrow pane wraps.
    #[serde(default = "one")]
    pub(super) wrap: usize,
    /// How far above the bottom the match may sit, for wrapped rules whose region
    /// is one line deeper.
    #[serde(default)]
    pub(super) max_position: Option<usize>,
    #[serde(flatten)]
    pub(super) matcher: RawMatcher,
}

/// Match forms; all that are set must hold.
#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct RawMatcher {
    /// Case-insensitive substrings, all of which must appear.
    #[serde(default)]
    pub(super) contains: Vec<String>,
    /// Case-insensitive substrings, at least one of which must appear.
    #[serde(default)]
    pub(super) contains_any: Vec<String>,
    /// Regexes over the region text, all of which must match.
    #[serde(default)]
    pub(super) regex: Vec<String>,
    /// Per-line regexes; each must match some line.
    #[serde(default)]
    pub(super) line_regex: Vec<String>,
    /// One line matching every `regex` and no `not_regex`.
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
    /// Evaluate this clause against a different region than the rule's own.
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
    #[serde(default)]
    markers: std::collections::HashMap<String, RawMarker>,
    /// Lines starting the agent's live area (`from_prompt_marker`).
    #[serde(default)]
    prompt_marker: Vec<String>,
    rules: Vec<RawRule>,
}

/// A compiled matcher: regexes compiled once, substrings pre-lowered.
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
    pub(super) is_hook: bool,
}

pub(super) struct Manifest {
    pub(super) id: String,
    prompt_marker: Vec<regex::Regex>,
    markers: std::collections::HashMap<String, Marker>,
    /// Sorted by descending priority, so evaluation stops at the first match.
    pub(super) rules: Vec<Rule>,
}

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
            // A stale hook write is not evidence: the terminating hook can be lost.
            let fresh = match (self.max_age, hook.age) {
                (Some(max), Some(age)) => age < max,
                // An unreadable mtime is missing evidence, not staleness.
                (Some(_), None) => true,
                (None, _) => true,
            };
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

const SHARED_TEMPLATES: &str = include_str!("manifests/shared.toml");

#[derive(Debug, Deserialize)]
struct RawTemplates {
    templates: std::collections::HashMap<String, RawRule>,
}

impl RawRule {
    /// Fold a template in: scalars set here win, lists concatenate.
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

/// Hook rules shared by every agent. They rank below live-chrome screen rules
/// and above guesses; a manifest overrides one by reusing its id.
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
    /// Match height above the region's bottom (bottom line = 1).
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
            // Newline-joined so `line_regex` still sees individual lines.
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

    /// The deciding rule: descending priority; positional peers of equal priority
    /// are evaluated together and the lowest match wins.
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
