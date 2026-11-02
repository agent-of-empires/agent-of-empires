//! Plugin-declared agent launch metadata.

use serde::{Deserialize, Serialize};

/// A plugin-provided agent launch command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationContribution {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The leading `NAME=value` assignments and the executable word of a launch
/// command, split on whitespace, plus how many of the words are assignments.
///
/// Discovery checks that executable without running anything, and the launch
/// passes these words to `exec env` unchanged. Quoting, expansion and field
/// splitting differ between shells, so a prefix that depends on them cannot be
/// verified the way it runs and is refused instead. Only the prefix is
/// inspected: arguments after the executable are never parsed.
pub fn launch_prefix(command: &str) -> Option<(Vec<&str>, usize)> {
    let mut words: Vec<&str> = Vec::new();
    for word in command.split_whitespace() {
        if !word
            .chars()
            .all(|c| !matches!(c, '\'' | '"' | '\\' | '$' | '`' | '~' | '#'))
        {
            return None;
        }
        if let Some((key, _)) = word.split_once('=') {
            // `env` reads every `NAME=value` word as an assignment, valid name
            // or not, so one it cannot set is not a launchable program either.
            if !valid_env_key(key) {
                return None;
            }
            words.push(word);
            continue;
        }
        // `env` reads a leading `-` word as its own option, never a program.
        if word.starts_with('-') {
            return None;
        }
        let assignments = words.len();
        words.push(word);
        return Some((words, assignments));
    }
    None
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::launch_prefix;

    #[test]
    fn launch_prefix_stops_at_the_executable() {
        assert_eq!(
            launch_prefix("PATH=/opt/agents/bin acmebot --filter='a b'"),
            Some((vec!["PATH=/opt/agents/bin", "acmebot"], 1))
        );
        assert_eq!(
            launch_prefix("acmebot \"$(printf x)\" `unrun`"),
            Some((vec!["acmebot"], 0))
        );
        assert_eq!(launch_prefix("TOKEN=local"), None);
        // Not assignments `env` can set, and `env` options, not programs.
        assert_eq!(launch_prefix("9INVALID=1 acmebot"), None);
        assert_eq!(launch_prefix("=x acmebot"), None);
        assert_eq!(launch_prefix("-i acmebot"), None);
        assert_eq!(launch_prefix("  "), None);
    }

    #[test]
    fn launch_prefix_refuses_shell_syntax_before_the_executable() {
        for command in [
            "PATH=$HOME/bin acmebot",
            "PATH=\"$HOME/bin\" acmebot",
            "PATH=${HOME}/bin acmebot",
            "PATH=${HOME/bin acmebot",
            "PATH=~/bin acmebot",
            "~/bin/acmebot",
            "PATH=/opt/agents\\\n/bin acmebot",
            "\"acmebot",
            "'acmebot",
            "\"\" acmebot",
            "PATH=$(printf /tmp) acmebot",
            "PATH=`echo /tmp` acmebot",
            "PATH=/tmp # comment",
        ] {
            assert_eq!(launch_prefix(command), None, "{command:?}");
        }
    }
}
