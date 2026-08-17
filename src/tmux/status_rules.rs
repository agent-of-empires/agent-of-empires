//! Declarative pane status rules for custom agents.
//!
//! `[[agents.<name>.status_rules]]` config entries give an agent without a
//! built-in pane detector, typically a `[session.custom_agents]` harness
//! that is *similar to but not the same binary as* a built-in agent, basic
//! status detection: ordered `contains`/`regex` rules evaluated against the
//! ANSI-stripped pane snapshot, first match wins, no match reports Idle.
//!
//! The compiled rules live in a process-global registry rather than on each
//! `Instance` because the status poll hot path deliberately never loads
//! config (see `Instance::detect_as`, resolved once at build for the same
//! reason). The registry is keyed by `(profile, agent)`: `resolve_config`
//! runs per profile many times per process, so an install must replace only
//! the calling profile's entries and leave every other profile's rules
//! standing (a bare `gjc` in profile A and a `gjc` in profile B are distinct
//! keys with independent rules). Consumers pass their session's profile so a
//! poll consults exactly that profile's rules. The registry is (re)installed
//! by `profile_config::resolve_config`, which every status-polling surface
//! (TUI boot, `aoe serve`, the session CLI) passes through, so editing the
//! rules takes effect on the next config resolve instead of requiring the
//! session to be re-created.
//!
//! The same registry carries `[session.agent_detect_as]`, for the same reason
//! and with the same freshness guarantee: see [`effective_detect_as`] for why
//! the copy persisted on each `Instance` cannot be the authority.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::agents::HookStatus;
use crate::session::config::StatusRule;
use crate::session::Status;

/// A rule compiled for the poll loop: the matcher is pre-lowered /
/// pre-compiled so per-tick evaluation is substring or regex work only.
struct CompiledRule {
    status: Status,
    matcher: Matcher,
}

enum Matcher {
    /// Case-insensitive substring: stored lowercased, tested against the
    /// lowercased pane text.
    Contains(String),
    /// Compiled regex, tested against the pane text as written.
    Regex(regex::Regex),
}

/// Compiled rules keyed by `(profile, agent)`, so each profile's rules are
/// installed and consulted independently.
type Registry = HashMap<(String, String), Vec<CompiledRule>>;

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// `[session.agent_detect_as]` keyed the same way, so [`effective_detect_as`]
/// can answer from the live config without the poll loop loading it.
type Aliases = HashMap<(String, String), String>;

fn aliases() -> &'static RwLock<Aliases> {
    static ALIASES: OnceLock<RwLock<Aliases>> = OnceLock::new();
    ALIASES.get_or_init(|| RwLock::new(HashMap::new()))
}

fn hook_status_to_status(status: HookStatus) -> Status {
    match status {
        HookStatus::Running => Status::Running,
        HookStatus::Waiting => Status::Waiting,
        HookStatus::Idle => Status::Idle,
        HookStatus::Error => Status::Error,
    }
}

/// Compile one agent's rules, skipping (with a warning) any rule that has
/// neither or both of `contains`/`regex`, or a regex that fails to compile.
fn compile_rules(agent: &str, rules: &[StatusRule]) -> Vec<CompiledRule> {
    let mut compiled = Vec::with_capacity(rules.len());
    for (i, rule) in rules.iter().enumerate() {
        let matcher = match (&rule.contains, &rule.regex) {
            (Some(needle), None) if !needle.is_empty() => Matcher::Contains(needle.to_lowercase()),
            (None, Some(pattern)) if !pattern.is_empty() => match regex::Regex::new(pattern) {
                Ok(re) => Matcher::Regex(re),
                Err(e) => {
                    tracing::warn!(target: "tmux.status",
                        "agents.{agent}.status_rules[{i}]: invalid regex {pattern:?}, rule skipped: {e}");
                    continue;
                }
            },
            (Some(_), Some(_)) => {
                tracing::warn!(target: "tmux.status",
                    "agents.{agent}.status_rules[{i}]: set exactly one of `contains` or `regex`, not both; rule skipped");
                continue;
            }
            _ => {
                tracing::warn!(target: "tmux.status",
                    "agents.{agent}.status_rules[{i}]: needs a non-empty `contains` or `regex`; rule skipped");
                continue;
            }
        };
        compiled.push(CompiledRule {
            status: hook_status_to_status(rule.status),
            matcher,
        });
    }
    compiled
}

/// Install `profile`'s rules and `agent_detect_as` aliases from `config`,
/// replacing only that profile's entries and leaving every other profile's
/// standing. Called on every config resolve, once per profile; an agent whose
/// rules all fail to compile ends up with no entry (falling back to the
/// built-in detector or Idle), matching the behavior of not configuring rules
/// at all.
pub fn install_from_config(profile: &str, config: &crate::session::Config) {
    // Normalize so an empty `source_profile` keys the same slot as the resolved
    // default profile; the lookup side (`detect` / `has_rules`) normalizes the
    // same way, so an unpopulated field degrades to the default profile's rules
    // instead of silently missing.
    let profile = crate::session::config::effective_profile(profile);

    // Held in its own scope: `install_from_config` is the only place that
    // writes both registries, and taking them one at a time keeps it that way.
    {
        let mut map = aliases().write().unwrap_or_else(|p| p.into_inner());
        map.retain(|(p, _), _| p != &profile);
        for (agent, target) in &config.session.agent_detect_as {
            // Malformed entries are reported by `Config::validate`; skipping
            // them here leaves the tool unaliased, which is what an absent
            // entry would have done.
            if agent.is_empty() || target.is_empty() {
                continue;
            }
            map.insert((profile.clone(), agent.clone()), target.clone());
        }
    }

    let mut map = registry().write().unwrap_or_else(|p| p.into_inner());
    // Drop this profile's previous entries; other profiles' keys are untouched.
    map.retain(|(p, _), _| p != &profile);
    for (agent, runtime) in &config.agents {
        if runtime.status_rules.is_empty() {
            continue;
        }
        let compiled = compile_rules(agent, &runtime.status_rules);
        if compiled.is_empty() {
            continue;
        }
        // Rules for a name that also has a built-in detector fully replace it:
        // a pane matching no rule reports Idle instead of falling through to the
        // built-in, so a single narrow rule on a built-in name silently loses
        // that agent's detection everywhere it doesn't match.
        if crate::agents::get_agent(agent).is_some() {
            tracing::warn!(target: "tmux.status",
                "agents.{agent}.status_rules shadow the built-in '{agent}' detector; \
                 panes matching no rule will report Idle rather than using the built-in detector");
        }
        map.insert((profile.clone(), agent.clone()), compiled);
    }
}

/// Whether `tool` has configured rules under `profile`. Used by the status
/// poller to let an agent's own rules take precedence over its
/// `agent_detect_as` alias.
pub fn has_rules(profile: &str, tool: &str) -> bool {
    let profile = crate::session::config::effective_profile(profile);
    registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&(profile, tool.to_string()))
}

/// Evaluate `tool`'s rules under `profile` against ANSI-stripped pane text.
/// Returns `None` when the tool has no rules for that profile (caller falls
/// back to the built-in detector), `Some(Idle)` when rules exist but none
/// match.
pub fn detect(profile: &str, tool: &str, clean_content: &str) -> Option<Status> {
    let profile = crate::session::config::effective_profile(profile);
    let reg = registry().read().unwrap_or_else(|p| p.into_inner());
    let rules = reg.get(&(profile, tool.to_string()))?;
    let lower = clean_content.to_lowercase();
    for rule in rules {
        let matched = match &rule.matcher {
            Matcher::Contains(needle) => lower.contains(needle),
            Matcher::Regex(re) => re.is_match(clean_content),
        };
        if matched {
            tracing::trace!(target: "tmux.status",
                "status rules for '{tool}': matched -> {:?}", rule.status);
            return Some(rule.status);
        }
    }
    tracing::trace!(target: "tmux.status", "status rules for '{tool}': no match -> Idle");
    Some(Status::Idle)
}

/// The `agent_detect_as` alias in force for `tool` under `profile`, or `""`
/// when the tool is not aliased.
///
/// `Instance::detect_as` is resolved once at session build and persisted, so a
/// session outlives the config that produced it: a `[session.agent_detect_as]`
/// entry added, renamed, or removed after the session was created leaves the
/// stored field stale, and an empty one is indistinguishable from "never had an
/// alias". A session created in that window loses its detector entirely, since
/// `detect_status_from_content_in` falls through to `Status::Idle` for a tool
/// with neither rules nor a built-in, so its status freezes at Idle forever.
///
/// So the stored value is a cache, not the authority: when it is empty this
/// consults the registry the config resolve installed. A non-empty stored value
/// still wins outright, keeping the hot path allocation-free for the sessions
/// that have one and preserving the per-session pin for anything that rewrites
/// the field directly. That precedence bounds what this heals: a session whose
/// stored alias is empty tracks the config live (an entry added, retargeted, or
/// removed later all take effect on the next resolve), while a session that
/// already has one is pinned to it until something rewrites the field.
pub fn effective_detect_as<'a>(profile: &str, tool: &str, detect_as: &'a str) -> Cow<'a, str> {
    if !detect_as.is_empty() {
        return Cow::Borrowed(detect_as);
    }
    let profile = crate::session::config::effective_profile(profile);
    aliases()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(profile, tool.to_string()))
        .map(|alias| Cow::Owned(alias.clone()))
        .unwrap_or(Cow::Borrowed(""))
}

/// The tool whose *pane* detection heuristics apply to a session: the
/// session's own tool when it has configured rules, else the
/// `agent_detect_as` alias when set, else the tool itself. Hook
/// reconciliation deliberately does not use this helper: hooks are
/// installed for the alias, so their reconcilers keep the alias identity
/// (see `Instance::update_status_with_metadata`), though they resolve that
/// identity through [`effective_detect_as`] for the same staleness reason.
pub fn detection_tool<'a>(profile: &str, tool: &'a str, detect_as: &'a str) -> Cow<'a, str> {
    if has_rules(profile, tool) {
        return Cow::Borrowed(tool);
    }
    let alias = effective_detect_as(profile, tool, detect_as);
    if alias.is_empty() {
        Cow::Borrowed(tool)
    } else {
        alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The registry is process-global; serialize the tests that write to it
    /// so parallel test threads can't clobber each other's installs.
    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn rule(status: HookStatus, contains: Option<&str>, regex: Option<&str>) -> StatusRule {
        StatusRule {
            status,
            contains: contains.map(str::to_string),
            regex: regex.map(str::to_string),
        }
    }

    fn config_with_rules(agent: &str, rules: Vec<StatusRule>) -> crate::session::Config {
        let mut config = crate::session::Config::default();
        config
            .agents
            .entry(agent.to_string())
            .or_default()
            .status_rules = rules;
        config
    }

    fn config_with_alias(agent: &str, target: &str) -> crate::session::Config {
        let mut config = crate::session::Config::default();
        config
            .session
            .agent_detect_as
            .insert(agent.to_string(), target.to_string());
        config
    }

    #[test]
    fn no_rules_returns_none() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config("default", &crate::session::Config::default());
        assert_eq!(detect("default", "anything", "some pane text"), None);
        assert!(!has_rules("default", "anything"));
    }

    #[test]
    fn first_match_wins_and_no_match_is_idle() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![
                    rule(HookStatus::Waiting, Some("(y/n)"), None),
                    rule(HookStatus::Running, Some("esc to interrupt"), None),
                ],
            ),
        );
        // Both substrings present: the earlier rule wins.
        assert_eq!(
            detect("default", "rules-agent", "approve? (y/n)\nesc to interrupt"),
            Some(Status::Waiting)
        );
        assert_eq!(
            detect("default", "rules-agent", "working... esc to interrupt"),
            Some(Status::Running)
        );
        assert_eq!(detect("default", "rules-agent", "$ "), Some(Status::Idle));
        install_from_config("default", &crate::session::Config::default());
    }

    #[test]
    fn contains_is_case_insensitive_and_regex_is_as_written() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![
                    rule(HookStatus::Running, Some("Thinking"), None),
                    rule(
                        HookStatus::Waiting,
                        None,
                        Some(r"waiting for [0-9]+ approvals?"),
                    ),
                ],
            ),
        );
        assert_eq!(
            detect("default", "rules-agent", "THINKING hard"),
            Some(Status::Running)
        );
        assert_eq!(
            detect("default", "rules-agent", "waiting for 2 approvals"),
            Some(Status::Waiting)
        );
        // Regex is case-sensitive unless the pattern opts in.
        assert_eq!(
            detect("default", "rules-agent", "WAITING FOR 2 APPROVALS"),
            Some(Status::Idle)
        );
        install_from_config("default", &crate::session::Config::default());
    }

    #[test]
    fn malformed_rules_are_skipped_and_all_skipped_means_no_entry() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![
                    rule(HookStatus::Running, None, Some("(unclosed")),
                    rule(HookStatus::Running, Some("x"), Some("y")),
                    rule(HookStatus::Running, None, None),
                    rule(HookStatus::Running, Some(""), None),
                ],
            ),
        );
        // Every rule was invalid: the agent has no entry at all.
        assert!(!has_rules("default", "rules-agent"));
        assert_eq!(detect("default", "rules-agent", "x"), None);

        // A valid rule survives alongside a skipped one.
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![
                    rule(HookStatus::Running, None, Some("(unclosed")),
                    rule(HookStatus::Error, Some("panicked at"), None),
                ],
            ),
        );
        assert_eq!(
            detect(
                "default",
                "rules-agent",
                "thread 'main' panicked at src/x.rs"
            ),
            Some(Status::Error)
        );
        install_from_config("default", &crate::session::Config::default());
    }

    #[test]
    fn install_replaces_previous_rules() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![rule(HookStatus::Running, Some("spin"), None)],
            ),
        );
        assert!(has_rules("default", "rules-agent"));
        install_from_config("default", &crate::session::Config::default());
        assert!(!has_rules("default", "rules-agent"));
    }

    #[test]
    fn install_is_scoped_to_its_profile() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        // p1's `gjc` maps a marker to Running; p2's `gjc` maps the same marker
        // to Error. Distinct keys, so neither install touches the other.
        install_from_config(
            "p1",
            &config_with_rules(
                "gjc",
                vec![rule(HookStatus::Running, Some("busy marker"), None)],
            ),
        );
        install_from_config(
            "p2",
            &config_with_rules(
                "gjc",
                vec![rule(HookStatus::Error, Some("busy marker"), None)],
            ),
        );
        assert_eq!(
            detect("p1", "gjc", "busy marker"),
            Some(Status::Running),
            "p1 rules must survive p2's install"
        );
        assert_eq!(
            detect("p2", "gjc", "busy marker"),
            Some(Status::Error),
            "p2 rules are independent of p1's"
        );

        // Reinstalling p2 with no rules clears only p2; p1 still detects.
        install_from_config("p2", &crate::session::Config::default());
        assert!(!has_rules("p2", "gjc"));
        assert_eq!(detect("p1", "gjc", "busy marker"), Some(Status::Running));

        install_from_config("p1", &crate::session::Config::default());
        assert!(!has_rules("p1", "gjc"));
    }

    #[test]
    fn detection_tool_prefers_own_rules_over_detect_as() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![rule(HookStatus::Running, Some("spin"), None)],
            ),
        );
        // Rules configured: the alias is ignored.
        assert_eq!(
            detection_tool("default", "rules-agent", "claude"),
            "rules-agent"
        );
        // No rules: alias applies, and no alias means the tool itself.
        assert_eq!(detection_tool("default", "other-agent", "claude"), "claude");
        assert_eq!(detection_tool("default", "other-agent", ""), "other-agent");
        install_from_config("default", &crate::session::Config::default());
    }

    /// A session built before its tool was added to `[session.agent_detect_as]`
    /// persists an empty alias, which used to strand it on the `Status::Idle`
    /// fallback forever. The stored value is a cache, so an empty one defers to
    /// the config the resolve installed.
    #[test]
    fn effective_detect_as_falls_back_to_installed_config() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config("default", &config_with_alias("claude-personal", "claude"));

        // (tool, stored detect_as, expected alias, expected detection tool)
        let cases = [
            // The bug: empty stored alias now resolves from config.
            ("claude-personal", "", "claude", "claude"),
            // A stored alias still wins, so a hand-pinned session is not
            // retargeted by an unrelated config edit.
            ("claude-personal", "codex", "codex", "codex"),
            // A tool with no entry stays unaliased and detects as itself.
            ("unmapped-agent", "", "", "unmapped-agent"),
        ];
        for (tool, stored, want_alias, want_detection) in cases {
            assert_eq!(
                effective_detect_as("default", tool, stored),
                want_alias,
                "alias for {tool:?} stored={stored:?}"
            );
            assert_eq!(
                detection_tool("default", tool, stored),
                want_detection,
                "detection tool for {tool:?} stored={stored:?}"
            );
        }

        // The fallback is scoped per profile like the rules are.
        assert_eq!(
            effective_detect_as("other-profile", "claude-personal", ""),
            ""
        );

        install_from_config("default", &crate::session::Config::default());
        // Cleared with its profile's install, so a removed entry stops applying.
        assert_eq!(effective_detect_as("default", "claude-personal", ""), "");
    }

    /// Own rules outrank the alias whether the alias came from the stored field
    /// or from the config fallback, so the fallback cannot silently take over an
    /// agent the user wrote rules for.
    #[test]
    fn own_rules_outrank_the_config_fallback() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let mut config = config_with_alias("rules-agent", "claude");
        config
            .agents
            .entry("rules-agent".to_string())
            .or_default()
            .status_rules = vec![rule(HookStatus::Running, Some("spin"), None)];
        install_from_config("default", &config);

        assert_eq!(effective_detect_as("default", "rules-agent", ""), "claude");
        assert_eq!(detection_tool("default", "rules-agent", ""), "rules-agent");

        install_from_config("default", &crate::session::Config::default());
    }

    #[test]
    fn rules_dispatch_through_detect_status_from_content_in() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![rule(HookStatus::Running, Some("esc to interrupt"), None)],
            ),
        );
        // ANSI codes are stripped by the dispatcher before rules run.
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default",
                "\x1b[31mesc to interrupt\x1b[0m",
                "rules-agent"
            ),
            Status::Running
        );
        // An unknown tool without rules still reports Idle.
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default", "anything", "no-rules"
            ),
            Status::Idle
        );
        install_from_config("default", &crate::session::Config::default());
    }

    #[test]
    fn rules_override_builtin_detector() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        // "claude" has a built-in pane detector; configured rules outrank it.
        install_from_config(
            "default",
            &config_with_rules(
                "claude",
                vec![rule(HookStatus::Error, Some("custom fail marker"), None)],
            ),
        );
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default",
                "custom fail marker",
                "claude"
            ),
            Status::Error
        );
        install_from_config("default", &crate::session::Config::default());
        // Registry cleared: the built-in detector is back in charge.
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default",
                "custom fail marker",
                "claude"
            ),
            Status::Idle
        );
    }
}
