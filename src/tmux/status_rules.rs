//! Declarative `[[agents.<name>.status_rules]]` (ordered `contains`/`regex`, first
//! match wins, else Idle) and `[session.agent_detect_as]` aliases.
//!
//! Both live in process-global registries keyed by `(profile, agent)` because
//! the poll hot path never loads config; `resolve_config` reinstalls a profile's
//! entries on every resolve, so edits apply without recreating sessions.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::agents::HookStatus;
use crate::session::config::StatusRule;
use crate::session::Status;

#[cfg_attr(test, derive(Clone))]
struct CompiledRule {
    status: Status,
    matcher: Matcher,
}

#[cfg_attr(test, derive(Clone))]
enum Matcher {
    /// Stored lowercased, tested against the lowercased pane text.
    Contains(String),
    Regex(regex::Regex),
}

type Registry = HashMap<(String, String), Vec<CompiledRule>>;

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

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

/// Replace `profile`'s rules and aliases, leaving other profiles standing.
/// Rules that all fail to compile leave no entry.
pub fn install_from_config(profile: &str, config: &crate::session::Config) {
    // An empty `source_profile` keys the resolved default profile, as lookups do.
    let profile = crate::session::config::effective_profile(profile);

    {
        let mut map = aliases().write().unwrap_or_else(|p| p.into_inner());
        map.retain(|(p, _), _| p != &profile);
        for (agent, target) in &config.session.agent_detect_as {
            if agent.is_empty() || target.is_empty() {
                continue;
            }
            map.insert((profile.clone(), agent.clone()), target.clone());
        }
    }

    let mut map = registry().write().unwrap_or_else(|p| p.into_inner());
    map.retain(|(p, _), _| p != &profile);
    for (agent, runtime) in &config.agents {
        if runtime.status_rules.is_empty() {
            continue;
        }
        let compiled = compile_rules(agent, &runtime.status_rules);
        if compiled.is_empty() {
            continue;
        }
        // Rules on a built-in name replace its detector entirely.
        if crate::agents::get_agent(agent).is_some() {
            tracing::warn!(target: "tmux.status",
                "agents.{agent}.status_rules shadow the built-in '{agent}' detector; \
                 panes matching no rule will report Idle rather than using the built-in detector");
        }
        map.insert((profile.clone(), agent.clone()), compiled);
    }
}

/// Lets an agent's own rules outrank its `agent_detect_as` alias.
pub fn has_rules(profile: &str, tool: &str) -> bool {
    let profile = crate::session::config::effective_profile(profile);
    registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&(profile, tool.to_string()))
}

/// `None` when `tool` has no rules under `profile`, `Some(Idle)` when none match.
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

/// The `agent_detect_as` alias for `tool`, or `""`. A non-empty persisted
/// `detect_as` wins; an empty one is only a cache miss, so it falls back to the
/// live config (else a session created before the alias would stay Idle).
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

/// The tool whose pane heuristics apply: `tool` if it has rules, else its alias,
/// else `tool`. Hook reconciliation keeps the alias identity instead.
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

/// Test guard that snapshots one profile's entries in both registries and
/// restores them on drop. Take it before the first mutation. It is rollback,
/// not mutual exclusion: concurrent writers to the same profile must be held off.
#[cfg(test)]
pub(crate) struct ProfileRegistryGuard {
    profile: String,
    aliases: Vec<((String, String), String)>,
    rules: Vec<((String, String), Vec<CompiledRule>)>,
}

#[cfg(test)]
impl ProfileRegistryGuard {
    pub(crate) fn take(profile: &str) -> Self {
        let profile = crate::session::config::effective_profile(profile);
        let aliases = aliases()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|((p, _), _)| *p == profile)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let rules = registry()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|((p, _), _)| *p == profile)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            profile,
            aliases,
            rules,
        }
    }

    fn restore(&mut self) {
        let mut map = aliases().write().unwrap_or_else(|p| p.into_inner());
        map.retain(|(p, _), _| p != &self.profile);
        map.extend(self.aliases.drain(..));
        drop(map);
        let mut reg = registry().write().unwrap_or_else(|p| p.into_inner());
        reg.retain(|(p, _), _| p != &self.profile);
        reg.extend(self.rules.drain(..));
    }
}

#[cfg(test)]
impl Drop for ProfileRegistryGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    /// The first matching rule wins, no match is idle, and no rules at all is
    /// no answer. `contains` ignores case; `regex` is matched as written.
    #[serial]
    #[test]
    fn first_matching_rule_decides_the_status() {
        let _registry = ProfileRegistryGuard::take("default");
        let ordered = vec![
            rule(HookStatus::Waiting, Some("(y/n)"), None),
            rule(HookStatus::Running, Some("esc to interrupt"), None),
        ];
        let mixed = vec![
            rule(HookStatus::Running, Some("Thinking"), None),
            rule(
                HookStatus::Waiting,
                None,
                Some(r"waiting for [0-9]+ approvals?"),
            ),
        ];
        let cases = [
            (vec![], "some pane text", None),
            (
                ordered.clone(),
                "approve? (y/n)\nesc to interrupt",
                Some(Status::Waiting),
            ),
            (
                ordered.clone(),
                "working... esc to interrupt",
                Some(Status::Running),
            ),
            (ordered, "$ ", Some(Status::Idle)),
            (mixed.clone(), "THINKING hard", Some(Status::Running)),
            (
                mixed.clone(),
                "waiting for 2 approvals",
                Some(Status::Waiting),
            ),
            (mixed, "WAITING FOR 2 APPROVALS", Some(Status::Idle)),
        ];
        for (rules, pane, expected) in cases {
            let has = !rules.is_empty();
            install_from_config("default", &config_with_rules("rules-agent", rules));
            assert_eq!(has_rules("default", "rules-agent"), has);
            assert_eq!(detect("default", "rules-agent", pane), expected, "{pane:?}");
        }
    }

    #[serial]
    #[test]
    fn malformed_rules_are_skipped_and_all_skipped_means_no_entry() {
        let _registry = ProfileRegistryGuard::take("default");
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
        assert!(!has_rules("default", "rules-agent"));
        assert_eq!(detect("default", "rules-agent", "x"), None);

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
    }

    #[serial]
    #[test]
    fn install_replaces_previous_rules() {
        let _registry = ProfileRegistryGuard::take("default");
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

    #[serial]
    #[test]
    fn install_is_scoped_to_its_profile() {
        let _registry_p1 = ProfileRegistryGuard::take("p1");
        let _registry_p2 = ProfileRegistryGuard::take("p2");
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

        install_from_config("p2", &crate::session::Config::default());
        assert!(!has_rules("p2", "gjc"));
        assert_eq!(detect("p1", "gjc", "busy marker"), Some(Status::Running));

        install_from_config("p1", &crate::session::Config::default());
        assert!(!has_rules("p1", "gjc"));
    }

    #[serial]
    #[test]
    fn detection_tool_prefers_own_rules_over_detect_as() {
        let _registry = ProfileRegistryGuard::take("default");
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![rule(HookStatus::Running, Some("spin"), None)],
            ),
        );
        assert_eq!(
            detection_tool("default", "rules-agent", "claude"),
            "rules-agent"
        );
        assert_eq!(detection_tool("default", "other-agent", "claude"), "claude");
        assert_eq!(detection_tool("default", "other-agent", ""), "other-agent");
    }

    #[serial]
    #[test]
    fn effective_detect_as_falls_back_to_installed_config() {
        let _registry = ProfileRegistryGuard::take("default");
        install_from_config("default", &config_with_alias("claude-personal", "claude"));

        let cases = [
            ("claude-personal", "", "claude", "claude"),
            ("claude-personal", "codex", "codex", "codex"),
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

        assert_eq!(
            effective_detect_as("other-profile", "claude-personal", ""),
            ""
        );

        install_from_config("default", &crate::session::Config::default());
        assert_eq!(effective_detect_as("default", "claude-personal", ""), "");
    }

    #[serial]
    #[test]
    fn own_rules_outrank_the_config_fallback() {
        let _registry = ProfileRegistryGuard::take("default");
        let mut config = config_with_alias("rules-agent", "claude");
        config
            .agents
            .entry("rules-agent".to_string())
            .or_default()
            .status_rules = vec![rule(HookStatus::Running, Some("spin"), None)];
        install_from_config("default", &config);

        assert_eq!(effective_detect_as("default", "rules-agent", ""), "claude");
        assert_eq!(detection_tool("default", "rules-agent", ""), "rules-agent");
    }

    #[serial]
    #[test]
    fn rules_dispatch_through_detect_status_from_content_in() {
        let _registry = ProfileRegistryGuard::take("default");
        install_from_config(
            "default",
            &config_with_rules(
                "rules-agent",
                vec![rule(HookStatus::Running, Some("esc to interrupt"), None)],
            ),
        );
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default",
                "\x1b[31mesc to interrupt\x1b[0m",
                "rules-agent"
            ),
            Status::Running
        );
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default", "anything", "no-rules"
            ),
            Status::Idle
        );
    }

    #[serial]
    #[test]
    fn rules_override_builtin_detector() {
        let _registry = ProfileRegistryGuard::take("default");
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
        assert_eq!(
            super::super::status_detection::detect_status_from_content_in(
                "default",
                "custom fail marker",
                "claude"
            ),
            Status::Idle
        );
    }

    #[serial]
    #[test]
    fn profile_registry_guard_restores_the_snapshotted_entries_on_drop() {
        let cases = [
            ("empty", None, "", None),
            (
                "seeded",
                Some(config_with_alias("sentinel-agent", "codex")),
                "codex",
                None,
            ),
            (
                "rules-only",
                Some(config_with_rules(
                    "rules-agent",
                    vec![rule(HookStatus::Running, Some("spin"), None)],
                )),
                "",
                Some(Status::Running),
            ),
            (
                "seeded-with-rules",
                Some({
                    let mut config = config_with_alias("sentinel-agent", "codex");
                    config
                        .agents
                        .entry("rules-agent".to_string())
                        .or_default()
                        .status_rules = vec![rule(HookStatus::Running, Some("spin"), None)];
                    config
                }),
                "codex",
                Some(Status::Running),
            ),
        ];
        for (label, prior, want_alias, want_spin) in cases {
            let profile = format!("guard-{label}");
            let _pre_test = ProfileRegistryGuard::take(&profile);
            if let Some(prior) = prior.as_ref() {
                install_from_config(&profile, prior);
            }

            {
                let _restore = ProfileRegistryGuard::take(&profile);
                let mut clobber = config_with_alias("sentinel-agent", "claude");
                clobber
                    .agents
                    .entry("rules-agent".to_string())
                    .or_default()
                    .status_rules = vec![rule(HookStatus::Error, Some("boom"), None)];
                install_from_config(&profile, &clobber);
            }

            assert_eq!(
                effective_detect_as(&profile, "sentinel-agent", ""),
                want_alias,
                "{label}: prior alias entry must be restored"
            );
            assert_eq!(
                detect(&profile, "rules-agent", "spin"),
                want_spin,
                "{label}: prior rule must be restored with its original matcher"
            );
        }
    }
}
