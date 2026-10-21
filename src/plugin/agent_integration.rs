//! Plugin-backed agent launch discovery.

use std::collections::HashMap;

/// An active plugin-provided agent that does not collide with a built-in or custom agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginAgent<'a> {
    pub plugin: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
}

fn unique_candidates<'a, 'b, 'c>(
    candidates: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    builtin_names: impl IntoIterator<Item = &'b str>,
    custom_names: impl IntoIterator<Item = &'c str>,
) -> Vec<PluginAgent<'a>> {
    let candidates: Vec<_> = candidates.into_iter().collect();
    let mut candidate_counts = HashMap::<&str, usize>::new();
    for (_, tool, _) in &candidates {
        *candidate_counts.entry(tool).or_default() += 1;
    }
    for name in builtin_names {
        candidate_counts.remove(name);
    }
    for name in custom_names {
        candidate_counts.remove(name);
    }
    candidates
        .into_iter()
        .filter_map(|(plugin, tool, command)| {
            (candidate_counts.get(tool) == Some(&1)).then_some(PluginAgent {
                plugin,
                tool,
                command,
            })
        })
        .collect()
}

/// Catalog unique plugin launch commands after built-in and custom agents take precedence.
pub fn catalog<'a, 'b, 'c>(
    registry: &'a super::PluginRegistry,
    builtin_names: impl IntoIterator<Item = &'b str>,
    custom_names: impl IntoIterator<Item = &'c str>,
) -> Vec<PluginAgent<'a>> {
    let candidates = registry.active().flat_map(|plugin| {
        plugin
            .manifest
            .agent_integrations
            .iter()
            .filter_map(move |entry| {
                entry
                    .command
                    .as_deref()
                    .map(|command| (plugin.id(), entry.tool.as_str(), command))
            })
    });
    unique_candidates(candidates, builtin_names, custom_names)
}

/// Resolve a unique plugin-provided agent launch command.
pub fn agent_command(tool: &str) -> Option<String> {
    let registry = super::registry();
    catalog(
        &registry,
        crate::agents::AGENTS.iter().map(|agent| agent.name),
        std::iter::empty(),
    )
    .into_iter()
    .find(|agent| agent.tool == tool)
    .map(|agent| {
        tracing::debug!(
            tool,
            plugin = agent.plugin,
            "resolved plugin-provided agent command"
        );
        agent.command.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{unique_candidates, PluginAgent};

    #[test]
    fn catalog_suppresses_duplicate_provider_and_custom_or_builtin_collisions() {
        let entries = unique_candidates(
            [
                ("plugin-a", "duplicate", "a"),
                ("plugin-b", "duplicate", "b"),
                ("plugin-c", "builtin-agent", "c"),
                ("plugin-d", "custom-agent", "d"),
                ("plugin-e", "unique-agent", "e"),
            ],
            ["builtin-agent"],
            ["custom-agent"],
        );
        assert_eq!(
            entries,
            [PluginAgent {
                plugin: "plugin-e",
                tool: "unique-agent",
                command: "e",
            }]
        );
    }
}
