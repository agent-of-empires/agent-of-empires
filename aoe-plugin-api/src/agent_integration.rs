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
/// command, plus how many of the words are assignments.
///
/// Discovery checks that executable without running anything, and the launch
/// passes these words to `exec env` unchanged. Only spaces and tabs separate
/// them, and no word may contain anything the launch shell would act on, so a
/// probed word is the word `env` receives. Quoting, expansion, field splitting
/// and command separators therefore cannot make discovery disagree with the
/// launch. Only the prefix is inspected: arguments after the executable are
/// never parsed.
pub fn launch_prefix(command: &str) -> Option<(Vec<&str>, usize)> {
    let mut words: Vec<&str> = Vec::new();
    for word in command.split([' ', '\t']) {
        // Repeated separators produce no word in the shell either.
        if word.is_empty() {
            continue;
        }
        if !literal_word(word) {
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

/// Whether the launch shell leaves a word intact: no separators, redirections,
/// globs, braces, expansions, quoting or whitespace other than what
/// `launch_prefix` already split on. A newline is a command separator to the
/// shell and a non-breaking space is one to `split_whitespace`, so both would
/// make the probed words differ from the launched ones.
fn literal_word(word: &str) -> bool {
    word.chars().all(|c| {
        !c.is_whitespace()
            && !c.is_control()
            && !matches!(
                c,
                '\'' | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | '~'
                    | '#'
                    | ';'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '['
                    | ']'
            )
    })
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
            // A newline ends the shell command, so `env` would run without an
            // executable instead of running the probed one.
            "TOKEN=local\nacmebot",
            "TOKEN=local\r\nacmebot",
            // Command separators and redirections split or redirect the launch.
            "TOKEN=local; acmebot",
            "TOKEN=local; acmebot",
            "TOKEN=local&acmebot",
            "TOKEN=local|acmebot",
            "TOKEN=local&&acmebot",
            "PATH=/opt/a>b acmebot",
            "PATH=/opt/a<b acmebot",
            "PATH=(x) acmebot",
            // Globs and braces expand to words discovery never sees.
            "PATH=/opt/*/bin acmebot",
            "PATH=/opt/[ab]/bin acmebot",
            "PATH=/opt/?/bin acmebot",
            "PATH={/opt/a,/opt/b} acmebot",
            // Non-ASCII whitespace is not a shell word separator.
            "TOKEN=local\u{a0}acmebot",
        ] {
            assert_eq!(launch_prefix(command), None, "{command:?}");
        }
    }

    #[test]
    fn launch_prefix_accepts_tabs_and_repeated_spaces_between_words() {
        assert_eq!(
            launch_prefix("PATH=/opt/agents/bin\tacmebot  --flag"),
            Some((vec!["PATH=/opt/agents/bin", "acmebot"], 1))
        );
    }
}
