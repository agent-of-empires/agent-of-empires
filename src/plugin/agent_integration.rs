//! Plugin-backed agent launch discovery.

use std::collections::HashMap;

/// Collisions already reported for a `(plugin, tool)`, so the reserved-name
/// warning fires once per process rather than on every catalog lookup.
static RESERVED_WARNED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

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
    let mut warned = RESERVED_WARNED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (plugin, tool, _) in &candidates {
        if let Some(builtin) = crate::agents::resolve_tool_name(tool) {
            // Warn once per (plugin, tool) per process: catalog runs on every
            // agents listing, detection, launch-command resolution and session
            // validation, so repeating it would flood the log.
            if warned.insert((plugin.to_string(), tool.to_string())) {
                tracing::warn!(
                    target: "plugin.agents",
                    plugin = %plugin,
                    tool = %tool,
                    builtin = %builtin,
                    "plugin agent tool name contains a built-in name or alias; preserving built-in precedence"
                );
            }
            candidate_counts.remove(tool);
        }
    }
    drop(warned);
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

/// Whether a character could spoof or rewrite displayed text: control
/// characters (Cc) and Unicode format characters (Cf), especially the bidi
/// controls a browser or terminal would otherwise reorder.
fn display_dangerous(c: char) -> bool {
    c.is_control()
        || matches!(
            c as u32,
            // ZWNJ/ZWJ/LRM/RLM, the bidi controls, invisible operators,
            // Arabic letter mark and the zero-width no-break space.
            0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x2064
                | 0x061C
                | 0xFEFF
        )
}

/// Display form of a plugin-declared command: control and format characters
/// (terminal escape sequences, which a malicious manifest could otherwise use
/// to rewrite the approval prompt, and bidi characters that would reorder it)
/// become visible `\x..` escapes. The runnable command is never altered, only
/// this disclosure string.
pub fn display_command(command: &str) -> String {
    if !command.chars().any(display_dangerous) {
        return command.to_string();
    }
    let mut out = String::with_capacity(command.len());
    for character in command.chars() {
        if display_dangerous(character) {
            out.extend(character.escape_default());
        } else {
            out.push(character);
        }
    }
    out
}

/// Resolve a unique plugin-provided agent launch command.
pub fn agent_command<'a>(
    tool: &str,
    custom_names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let registry = super::registry();
    catalog(
        &registry,
        crate::agents::AGENTS.iter().map(|agent| agent.name),
        custom_names,
    )
    .into_iter()
    .find(|agent| agent.tool == tool)
    .map(|agent| {
        tracing::debug!(
            target: "plugin.agents",
            tool,
            plugin = agent.plugin,
            "resolved plugin-provided agent command"
        );
        agent.command.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{display_command, unique_candidates, PluginAgent};

    #[test]
    fn catalog_suppresses_duplicate_provider_and_custom_or_builtin_collisions() {
        let entries = unique_candidates(
            [
                ("plugin-a", "duplicate", "a"),
                ("plugin-b", "duplicate", "b"),
                ("plugin-c", "builtin-agent", "c"),
                ("plugin-d", "custom-agent", "d"),
                ("plugin-e", "unique-helper", "e"),
            ],
            ["builtin-agent"],
            ["custom-agent"],
        );
        assert_eq!(
            entries,
            [PluginAgent {
                plugin: "plugin-e",
                tool: "unique-helper",
                command: "e",
            }]
        );
    }

    #[test]
    fn builtin_substring_aliases_are_reserved_from_plugins() {
        for tool in ["claude-yolo", "session-agent"] {
            assert!(
                unique_candidates([("plugin", tool, "agent")], [], []).is_empty(),
                "built-in alias should reserve {tool}"
            );
        }
    }

    #[test]
    fn display_command_escapes_controls_and_bidi() {
        assert_eq!(
            display_command("session-helper --flag"),
            "session-helper --flag"
        );
        assert_eq!(display_command("x\u{1b}[2Jtail"), "x\\u{1b}[2Jtail");
        // Bidi controls are escaped too, not just Cc controls.
        assert_eq!(display_command("x\u{202e}tail"), "x\\u{202e}tail");
    }
}
