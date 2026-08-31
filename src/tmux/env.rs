//! Hidden environment variable helpers for tmux sessions
//!
//! This module provides utilities to get and set hidden environment variables
//! in tmux sessions using the `-h` flag. Hidden variables are not inherited by
//! child processes, making them ideal for storing session metadata.

use anyhow::bail;
use std::collections::HashMap;

pub const AOE_INSTANCE_ID_KEY: &str = "AOE_INSTANCE_ID";
pub const AOE_CAPTURED_SESSION_ID_KEY: &str = "AOE_CAPTURED_SESSION_ID";
pub const AOE_OMP_CAPTURE_META_KEY: &str = "AOE_OMP_CAPTURE_META";
pub const AOE_OMP_LAUNCH_ID_KEY: &str = "AOE_OMP_LAUNCH_ID";
pub const AOE_OMP_CAPTURE_READY_KEY: &str = "AOE_OMP_CAPTURE_READY";

/// Set a hidden environment variable in a tmux session
///
/// Hidden variables (set with `-h`) are not inherited by child processes.
pub fn set_hidden_env(session_name: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let output = crate::tmux::tmux_command()
        .args(["set-environment", "-h", "-t", session_name, key, value])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "tmux set-environment -h -t '{}' {}: exit {}: {}",
            session_name,
            key,
            output.status,
            stderr.trim()
        );
    }

    Ok(())
}

/// Get a hidden environment variable from a tmux session.
pub fn get_hidden_env(session_name: &str, key: &str) -> Option<String> {
    fetch_env(session_name, key, true)
}

pub(crate) fn get_env(session_name: &str, key: &str) -> Option<String> {
    fetch_env(session_name, key, false)
}

fn fetch_env(session_name: &str, key: &str, hidden: bool) -> Option<String> {
    let mut command = crate::tmux::tmux_command();
    command.arg("show-environment");
    if hidden {
        command.arg("-h");
    }
    let output = command.args(["-t", session_name, key]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.starts_with('-') {
        return None;
    }
    line.split_once('=').map(|(_, value)| value.to_string())
}

/// Remove a hidden environment variable from a tmux session
pub fn remove_hidden_env(session_name: &str, key: &str) -> anyhow::Result<()> {
    let output = crate::tmux::tmux_command()
        .args(["set-environment", "-h", "-u", "-t", session_name, key])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to remove hidden env var: {}", stderr);
    }

    Ok(())
}

/// Remove hidden environment variables from multiple sessions with a single tmux command.
///
/// Each tuple is `(session_name, key)`. Falls back to per-entry calls on
/// batch failure; per-entry failures are logged but do not abort subsequent
/// entries (best-effort cleanup).
pub fn remove_hidden_env_batch(entries: &[(&str, &str)]) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut args: Vec<String> = Vec::new();
    for (i, (session_name, key)) in entries.iter().enumerate() {
        if i > 0 {
            args.push(";".to_string());
        }
        args.push("set-environment".to_string());
        args.push("-h".to_string());
        args.push("-u".to_string());
        args.push("-t".to_string());
        args.push(session_name.to_string());
        args.push(key.to_string());
    }

    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = crate::tmux::tmux_command().args(&str_args).output();

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment -u failed (exit {}), falling back to sequential unsets",
                out.status
            );
            sequential_remove_fallback(entries);
            Ok(())
        }
        Err(e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment -u error: {}, falling back to sequential unsets",
                e
            );
            sequential_remove_fallback(entries);
            Ok(())
        }
    }
}

fn sequential_remove_fallback(entries: &[(&str, &str)]) {
    for (session_name, key) in entries {
        if let Err(e) = remove_hidden_env(session_name, key) {
            tracing::debug!(target: "tmux.command",
                "Sequential unset of {} on {} failed: {}",
                key,
                session_name,
                e
            );
        }
    }
}

/// Set hidden environment variables in multiple sessions with a single tmux command.
///
/// Each tuple is `(session_name, key, value)`. Falls back to individual
/// `set_hidden_env` calls if the batch command fails (same pattern as
/// `get_hidden_env_batch`).
pub fn set_hidden_env_batch(entries: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut args: Vec<String> = Vec::new();
    for (i, (session_name, key, value)) in entries.iter().enumerate() {
        if i > 0 {
            args.push(";".to_string());
        }
        args.push("set-environment".to_string());
        args.push("-h".to_string());
        args.push("-t".to_string());
        args.push(session_name.to_string());
        args.push(key.to_string());
        args.push(value.to_string());
    }

    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = crate::tmux::tmux_command().args(&str_args).output();

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment failed (exit {}), falling back to sequential writes",
                out.status
            );
            sequential_set_fallback(entries);
            Ok(())
        }
        Err(e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment error: {}, falling back to sequential writes",
                e
            );
            sequential_set_fallback(entries);
            Ok(())
        }
    }
}

fn sequential_set_fallback(entries: &[(&str, &str, &str)]) {
    for (session_name, key, value) in entries {
        if let Err(e) = set_hidden_env(session_name, key, value) {
            tracing::debug!(target: "tmux.command",
                "Sequential set of {} on {} failed: {}",
                key,
                session_name,
                e
            );
        }
    }
}

/// First character of the marker line each batched segment prints ahead of
/// its `show-environment` output, so a block that is empty (the session has
/// no hidden vars) cannot shift every later line onto the wrong session.
const BATCH_MARKER: char = '\u{1f}';

/// Get a hidden environment variable from multiple sessions in one tmux
/// command, returning `(session_name, value)` in input order.
///
/// tmux ABORTS a `;`-separated command list at the first command that fails,
/// so no segment may fail: each one queries the session's whole hidden
/// environment (`show-environment -h` with no variable exits 0 even when the
/// variable, or every variable, is unset) rather than the single key, and the
/// key is picked out of the marked block. A session that disappears mid-batch
/// still truncates the run, so any session whose marker never came back is
/// re-read sequentially instead of being reported as unset.
pub fn get_hidden_env_batch(session_names: &[&str], key: &str) -> Vec<(String, Option<String>)> {
    if session_names.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = Vec::new();
    for (i, session_name) in session_names.iter().enumerate() {
        if i > 0 {
            args.push(";".to_string());
        }
        args.extend([
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            session_name.to_string(),
            format!("{BATCH_MARKER}{}", session_name.replace('#', "##")),
            ";".to_string(),
            "show-environment".to_string(),
            "-h".to_string(),
            "-t".to_string(),
            session_name.to_string(),
        ]);
    }
    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = crate::tmux::tmux_command().args(&str_args).output();
    let mut covered = match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_batch_output(&stdout, session_names, key)
        }
        Err(ref e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux show-environment error: {}, falling back to sequential reads",
                e
            );
            HashMap::new()
        }
    };
    let mut repaired = 0usize;
    let results: Vec<(String, Option<String>)> = session_names
        .iter()
        .map(|name| {
            let value = match covered.remove(name) {
                Some(value) => value,
                None => {
                    repaired += 1;
                    get_hidden_env(name, key)
                }
            };
            (name.to_string(), value)
        })
        .collect();
    if repaired > 0 {
        tracing::debug!(target: "tmux.command",
            "Batch tmux show-environment covered {} of {} sessions; read the rest sequentially",
            session_names.len() - repaired,
            session_names.len()
        );
    }

    results
}

/// Parse marker-delimited batch output into `key`'s value per session.
///
/// Only sessions whose marker line came back are present in the map: an entry
/// is the authoritative reading for that session (`None` = the key is unset),
/// while an ABSENT session is one the run never reached and the caller must
/// read separately. `-KEY` (explicitly removed) reads as unset.
fn parse_batch_output<'a>(
    output: &str,
    session_names: &[&'a str],
    key: &str,
) -> HashMap<&'a str, Option<String>> {
    let prefix = format!("{key}=");
    let mut values: HashMap<&str, Option<String>> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix(BATCH_MARKER) {
            current = session_names.iter().copied().find(|n| *n == name);
            if let Some(name) = current {
                values.entry(name).or_insert(None);
            }
            continue;
        }
        let Some(name) = current else { continue };
        if let Some(value) = line.strip_prefix(&prefix) {
            values.insert(name, Some(value.to_string()));
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(name: &str, body: &str) -> String {
        format!("{BATCH_MARKER}{name}\n{body}")
    }

    #[test]
    fn test_parse_batch_output_attributes_by_marker() {
        let m = BATCH_MARKER;
        let key = "AOE_INSTANCE_ID";
        // (output, sessions, expected per session: None = not covered by the
        // run at all, Some(None) = covered and unset)
        let cases = vec![
            (
                marked("s1", "AOE_INSTANCE_ID=abc123\n"),
                &["s1"][..],
                vec![Some(Some("abc123"))],
            ),
            // Covered but unset: the block is empty, or holds other keys only.
            (marked("s1", ""), &["s1"][..], vec![Some(None)]),
            (
                marked("s1", "AOE_CAPTURED_SESSION_ID=other\n"),
                &["s1"][..],
                vec![Some(None)],
            ),
            (
                marked("s1", "-AOE_INSTANCE_ID\n"),
                &["s1"][..],
                vec![Some(None)],
            ),
            (
                marked("s1", "AOE_INSTANCE_ID=value=with=equals\n"),
                &["s1"][..],
                vec![Some(Some("value=with=equals"))],
            ),
            // A session lacking the variable must not shift the rest.
            (
                format!("{m}s1\nAOE_INSTANCE_ID=abc123\n{m}s2\n{m}s3\nAOE_INSTANCE_ID=xyz789\n"),
                &["s1", "s2", "s3"][..],
                vec![Some(Some("abc123")), Some(None), Some(Some("xyz789"))],
            ),
            // The regression: tmux aborts the list at a failing segment, so
            // sessions past it produce no marker and must read as uncovered
            // (the caller re-reads them) rather than as unset.
            (
                format!("{m}s1\nAOE_INSTANCE_ID=abc123\n"),
                &["s1", "s2"][..],
                vec![Some(Some("abc123")), None],
            ),
            (String::new(), &["s1", "s2"][..], vec![None, None]),
            ("AOE_INSTANCE_ID=abc\n".to_string(), &["s1"][..], vec![None]),
            // A block for a session that was not asked about is ignored.
            (
                format!("{m}other\nAOE_INSTANCE_ID=nope\n{m}s1\nAOE_INSTANCE_ID=abc\n"),
                &["s1"][..],
                vec![Some(Some("abc"))],
            ),
            (
                format!("  {m}s1  \n  AOE_INSTANCE_ID=value123  \n"),
                &["s1"][..],
                vec![Some(Some("value123"))],
            ),
        ];
        for (output, sessions, expected) in cases {
            let parsed = parse_batch_output(&output, sessions, key);
            let got: Vec<Option<Option<&str>>> = sessions
                .iter()
                .map(|name| parsed.get(name).map(|v| v.as_deref()))
                .collect();
            assert_eq!(got, expected, "values for {output:?}");
        }
    }

    #[test]
    fn test_get_hidden_env_batch_empty_input() {
        let result = get_hidden_env_batch(&[], "KEY");
        assert_eq!(result.len(), 0);
    }
}
