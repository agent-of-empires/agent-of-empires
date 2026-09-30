//! Shared input handling for the hidden hook subcommands.

use std::io::Read;

pub(crate) const STDIN_BYTE_CAP: u64 = 1 << 20;
const MAX_ANCESTORS: usize = 64;

/// Reads at most [`STDIN_BYTE_CAP`] bytes and parses them as JSON.
pub(crate) fn read_json<R: Read>(stdin: R) -> anyhow::Result<serde_json::Value> {
    let mut buf = String::new();
    stdin.take(STDIN_BYTE_CAP).read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

/// A nested agent of another kind (Claude's Bash tool running `codex exec`, say) inherits the
/// pane's `AOE_*` environment, and the ancestor walk only counts copies of the pane's own binary.
/// A hook that names its agent is accepted only from that agent's pane; an unnamed hook (an install
/// predating `--agent`) or a pane without `AOE_AGENT_BIN` keeps the walk as its only guard.
pub(crate) fn publisher_is_pane_agent(publisher: Option<&str>, pane_agent: Option<&str>) -> bool {
    match (publisher, pane_agent.filter(|agent| !agent.is_empty())) {
        (Some(publisher), Some(pane_agent)) => publisher == pane_agent,
        _ => true,
    }
}

pub(crate) fn fired_by_pane_agent() -> bool {
    let agent_pid = std::env::var("AOE_AGENT_PID")
        .ok()
        .and_then(|pid| pid.parse().ok());
    let agent_bin = std::env::var("AOE_AGENT_BIN")
        .ok()
        .filter(|bin| !bin.is_empty());
    let (Some(agent_pid), Some(agent_bin)) = (agent_pid, agent_bin) else {
        return true;
    };
    // A launch under another name (`company-codex` running Codex) is the agent too.
    let program = std::env::var("AOE_AGENT_PROGRAM")
        .ok()
        .filter(|program| !program.is_empty() && *program != agent_bin);
    let names: Vec<&str> = std::iter::once(agent_bin.as_str())
        .chain(program.as_deref())
        .collect();
    walk_reaches_single_agent(
        std::os::unix::process::parent_id(),
        agent_pid,
        &names,
        crate::process::parent_and_argv0,
    )
}

/// Whether the walk from the hook up to the launched agent meets at most one process named like
/// the agent, i.e. the hook was fired by the pane's own agent rather than a nested copy.
fn walk_reaches_single_agent(
    start: u32,
    agent_pid: u32,
    agent_names: &[&str],
    parent_and_argv0: impl Fn(u32) -> Option<(u32, String)>,
) -> bool {
    let mut pid = start;
    let mut agents = 0;
    for hop in 0..MAX_ANCESTORS {
        let Some((ppid, argv0)) = parent_and_argv0(pid) else {
            return hop == 0;
        };
        if std::path::Path::new(&argv0)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| agent_names.contains(&name))
        {
            agents += 1;
        }
        if pid == agent_pid {
            return agents <= 1;
        }
        if ppid == 0 || ppid == pid {
            return false;
        }
        pid = ppid;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_pane_agent_owns_the_hook() {
        type Chain = &'static [(u32, u32, &'static str)];
        let cases: [(&str, u32, Chain, bool); 7] = [
            ("direct launch", 9, &[(10, 9, "sh"), (9, 1, "claude")], true),
            (
                "wrapper runs the agent as a child",
                8,
                &[(10, 9, "sh"), (9, 8, "/opt/bin/claude"), (8, 1, "/bin/sh")],
                true,
            ),
            (
                "nested agent from the shell tool",
                7,
                &[
                    (10, 9, "sh"),
                    (9, 8, "claude"),
                    (8, 7, "bash"),
                    (7, 1, "claude"),
                ],
                false,
            ),
            (
                "nested agent under a wrapper",
                6,
                &[
                    (10, 9, "sh"),
                    (9, 8, "claude"),
                    (8, 7, "bash"),
                    (7, 6, "claude"),
                    (6, 1, "sh"),
                ],
                false,
            ),
            (
                "detached from the launched pid",
                7,
                &[(10, 9, "sh"), (9, 1, "claude"), (1, 0, "init")],
                false,
            ),
            ("unreadable ancestor", 7, &[(10, 9, "sh")], false),
            ("process table unavailable", 7, &[], true),
        ];
        for (name, agent_pid, chain, owned) in cases {
            let table: std::collections::HashMap<u32, (u32, String)> = chain
                .iter()
                .map(|(pid, ppid, argv0)| (*pid, (*ppid, argv0.to_string())))
                .collect();
            let lookup = |pid| table.get(&pid).cloned();
            assert_eq!(
                walk_reaches_single_agent(10, agent_pid, &["claude"], lookup),
                owned,
                "{name}"
            );
        }
    }

    /// `company-codex` can be Codex itself under another name. Its launch passes that name, so a
    /// nested `codex exec` below it is the second agent on the walk, not the first.
    #[test]
    fn a_renamed_outer_agent_still_counts_on_the_walk() {
        type Chain = &'static [(u32, u32, &'static str)];
        let renamed: &[&str] = &["codex", "company-codex"];
        let cases: [(&str, &[&str], u32, Chain, bool); 4] = [
            (
                "the renamed outer agent publishes",
                renamed,
                9,
                &[(10, 9, "sh"), (9, 1, "/opt/bin/company-codex")],
                true,
            ),
            (
                "nested codex exec under the renamed outer agent",
                renamed,
                7,
                &[
                    (10, 9, "sh"),
                    (9, 8, "codex"),
                    (8, 7, "bash"),
                    (7, 1, "/opt/bin/company-codex"),
                ],
                false,
            ),
            (
                "a wrapper script that runs codex as a child",
                renamed,
                8,
                &[(10, 9, "sh"), (9, 8, "codex"), (8, 1, "/bin/sh")],
                true,
            ),
            (
                // What the walk saw before the launch named its program.
                "without the program name the nested codex passed",
                &["codex"],
                7,
                &[
                    (10, 9, "sh"),
                    (9, 8, "codex"),
                    (8, 7, "bash"),
                    (7, 1, "/opt/bin/company-codex"),
                ],
                true,
            ),
        ];
        for (name, names, agent_pid, chain, owned) in cases {
            let table: std::collections::HashMap<u32, (u32, String)> = chain
                .iter()
                .map(|(pid, ppid, argv0)| (*pid, (*ppid, argv0.to_string())))
                .collect();
            let lookup = |pid| table.get(&pid).cloned();
            assert_eq!(
                walk_reaches_single_agent(10, agent_pid, names, lookup),
                owned,
                "{name}"
            );
        }
    }
}
