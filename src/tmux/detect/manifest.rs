//! Manifest schema, compilation, and evaluation.
//!
//! A manifest is one agent's detection rules as data. Each rule names the
//! state it asserts, the [`Region`] it looks at, a priority, and a matcher.
//! Every rule that matches is collected and the highest priority wins, so
//! adding a case means adding a row rather than threading another branch
//! through a chain of detectors.

use serde::Deserialize;

use super::region::{Region, Screen};
use crate::session::Status;

/// A rule as written in TOML.
#[derive(Debug, Deserialize)]
pub(super) struct RawRule {
    pub(super) id: String,
    /// Omitted by rules that only carry `skip_state_update`.
    #[serde(default)]
    pub(super) state: Option<String>,
    pub(super) priority: i32,
    pub(super) region: String,
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
    #[serde(flatten)]
    pub(super) matcher: RawMatcher,
}

/// The match forms a rule (or a nested clause) may carry. Several may be set
/// at once, in which case all of them must hold.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RawMatcher {
    /// Case-insensitive substrings, all of which must appear.
    #[serde(default)]
    pub(super) contains: Vec<String>,
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
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawLineClause {
    #[serde(default)]
    pub(super) regex: Vec<String>,
    #[serde(default)]
    pub(super) not_regex: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    id: String,
    /// Lines that mark the start of the agent's live area, so rules can name
    /// it (`from_prompt_marker`) instead of matching against a transcript that
    /// still holds the previous turn's chrome.
    #[serde(default)]
    prompt_marker: Vec<String>,
    rules: Vec<RawRule>,
}

/// A compiled matcher. Regexes are compiled once at startup; substrings are
/// pre-lowered, so evaluation is substring and regex work only.
pub(super) struct Matcher {
    contains: Vec<String>,
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
            contains: raw.contains.iter().map(|c| c.to_lowercase()).collect(),
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

    /// Whether every form this matcher carries holds for `text`. An empty
    /// matcher matches, which is what lets a rule be pure `not` clauses.
    fn matches(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|c| lower.contains(c.as_str()))
            && self.regex.iter().all(|r| r.is_match(text))
            && self
                .line_regex
                .iter()
                .all(|r| text.lines().any(|line| r.is_match(line)))
            && self.line.as_ref().is_none_or(|(want, reject)| {
                text.lines().any(|line| {
                    want.iter().all(|r| r.is_match(line))
                        && !reject.iter().any(|r| r.is_match(line))
                })
            })
            && (self.any.is_empty() || self.any.iter().any(|m| m.matches(text, lower)))
            && self.all.iter().all(|m| m.matches(text, lower))
            && !self.not.iter().any(|m| m.matches(text, lower))
    }
}

impl Rule {
    fn compile(raw: RawRule) -> anyhow::Result<Self> {
        let is_hook = raw.region == "hook";
        let region = if is_hook {
            Region::WholeRecent
        } else {
            Region::parse(&raw.region).ok_or_else(|| {
                anyhow::anyhow!("rule {}: unknown region {:?}", raw.id, raw.region)
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
    ) -> bool {
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
            return match (self.max_age, hook.age) {
                (Some(max), Some(age)) => age < max,
                // An unreadable mtime is missing evidence, not evidence of
                // staleness, so the bound does not fire on it.
                (Some(_), None) => true,
                (None, _) => true,
            };
        }
        let text = screen.region_text(self.region, prompt_marker);
        if text.is_empty() {
            return false;
        }
        let lower = text.to_lowercase();
        self.matcher.matches(text, &lower)
    }
}

impl Manifest {
    pub(super) fn parse(source: &str) -> anyhow::Result<Self> {
        let raw: RawManifest = toml::from_str(source)?;
        let mut rules = raw
            .rules
            .into_iter()
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
        Ok(Self {
            id: raw.id,
            prompt_marker,
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
            .is_some_and(|r| r.matches(screen, hook, &self.prompt_marker))
    }

    /// The highest-priority rule that matches, if any.
    pub(super) fn evaluate(&self, screen: &Screen, hook: Option<HookObservation>) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.matches(screen, hook, &self.prompt_marker))
    }
}
