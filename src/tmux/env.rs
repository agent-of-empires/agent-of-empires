//! Hidden environment variable helpers for tmux sessions
//!
//! This module provides utilities to get and set hidden environment variables
//! in tmux sessions using the `-h` flag. Hidden variables are not inherited by
//! child processes, making them ideal for storing session metadata.

use anyhow::bail;
use std::collections::{HashMap, HashSet};

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
///
/// It carries the session's batch index, not its name, because tmux rewrites
/// every byte outside `0x20..=0x7e` to `_` for a client whose locale is not
/// UTF-8: a control character would erase every marker, and a name would let
/// `aoe_café` print the marker of a live `aoe_caf_`. Decimal digits are
/// injective under that rewrite, so a marker names exactly one session.
const BATCH_MARKER: char = '@';

/// Get a hidden environment variable from multiple sessions in one tmux
/// command, returning `(session_name, value)` in input order.
///
/// tmux ABORTS a `;`-separated command list at the first command that fails,
/// so no segment may fail: each one queries the session's whole hidden
/// environment (`show-environment -h -s` with no variable exits 0 even when
/// the variable, or every variable, is unset) rather than the single key, and
/// the key is picked out of the marked block. A session that disappears
/// mid-batch still truncates the run, so any session whose marker never came
/// back is re-read sequentially instead of being reported as unset.
pub fn get_hidden_env_batch(session_names: &[&str], key: &str) -> Vec<(String, Option<String>)> {
    if session_names.is_empty() {
        return Vec::new();
    }
    let args = batch_args(session_names);
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

/// tmux argument list for one batched read: a marker line then the whole
/// hidden environment, per session.
fn batch_args(session_names: &[&str]) -> Vec<String> {
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
            format!("{BATCH_MARKER}{i}"),
            ";".to_string(),
            "show-environment".to_string(),
            "-h".to_string(),
            "-s".to_string(),
            "-t".to_string(),
            session_name.to_string(),
        ]);
    }
    args
}

/// Parse marker-delimited batch output into `key`'s value per session.
///
/// Only sessions whose marker line came back are present in the map: an entry
/// is the authoritative reading for that session (`None` = the key is unset),
/// while an ABSENT session is one the run never reached, or one whose block
/// did not parse, and the caller must read separately. `unset KEY;`
/// (explicitly removed) reads as unset.
fn parse_batch_output<'a>(
    output: &str,
    session_names: &[&'a str],
    key: &str,
) -> HashMap<&'a str, Option<String>> {
    let mut values: HashMap<&str, Option<String>> = HashMap::new();
    let mut unparsed: HashSet<&str> = HashSet::new();
    let mut read_key: HashSet<&str> = HashSet::new();
    let mut current: Option<&str> = None;
    let mut rest = output;
    while !rest.is_empty() {
        let (line, after_line) = split_line(rest);
        let marked = line.trim().strip_prefix(BATCH_MARKER);
        if let Some(name) = marked
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| session_names.get(i).copied())
        {
            // Each index is printed once, so a repeat did not come from tmux.
            if values.insert(name, None).is_some() {
                unparsed.insert(name);
            }
            current = Some(name);
            rest = after_line;
            continue;
        }
        if let Some((name, value, after_entry)) = parse_env_entry(rest) {
            if name == key {
                if let Some(session) = current {
                    // tmux walks a keyed tree once, so a second record for the
                    // requested key is not a shape tmux can emit.
                    if !read_key.insert(session) {
                        unparsed.insert(session);
                    }
                    values.insert(session, value);
                }
            }
            rest = after_entry;
            continue;
        }
        // A marker for a session nobody asked about ends the current block;
        // anything else means the block did not parse as tmux wrote it, so
        // drop it rather than guess which entry a stray line belonged to.
        if marked.is_some() {
            current = None;
        } else if let Some(session) = current {
            unparsed.insert(session);
        }
        rest = after_line;
    }
    for name in unparsed {
        values.remove(name);
    }
    values
}

/// Consume one `show-environment -s` record from the head of `input`,
/// returning `(name, value, remainder)`; `None` when the head is not a record.
///
/// `-s` wraps every value in double quotes and backslash-escapes any quote or
/// backslash inside it, so a value holding a newline cannot end a record: the
/// scan runs to the first unescaped quote, not to the next line break. That is
/// what stops a continuation line reading `KEY=...` from impersonating an
/// entry for `KEY` (#3616). The `; export <name>;` tail is required rather
/// than skipped, so a record tmux did not write is rejected outright instead
/// of being read up to its quote.
fn parse_env_entry(input: &str) -> Option<(&str, Option<String>, &str)> {
    if let Some(rest) = input.strip_prefix("unset ") {
        let (line, after) = split_line(rest);
        let name = line.strip_suffix(';')?;
        return (!name.is_empty()).then_some((name, None, after));
    }
    let (head, _) = split_line(input);
    let name = &input[..head.find("=\"")?];
    if name.is_empty() {
        return None;
    }
    let mut value = String::new();
    let body = &input[name.len() + 2..];
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => value.push(chars.next()?.1),
            '"' => {
                let (tail, after) = split_line(&body[i + 1..]);
                let exported = tail.strip_prefix("; export ")?.strip_suffix(';')?;
                return (exported == name).then_some((name, Some(value), after));
            }
            _ => value.push(c),
        }
    }
    None
}

/// Split off the first line, dropping its terminator.
fn split_line(input: &str) -> (&str, &str) {
    match input.split_once('\n') {
        Some((line, rest)) => (line.strip_suffix('\r').unwrap_or(line), rest),
        None => (input, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `show-environment -s` record. `value` is already tmux-escaped.
    fn entry(name: &str, value: &str) -> String {
        format!("{name}=\"{value}\"; export {name};\n")
    }

    /// One session's segment: its batch-index marker, then its records.
    fn marked(index: usize, body: &str) -> String {
        format!("{BATCH_MARKER}{index}\n{body}")
    }

    #[test]
    fn test_parse_batch_output_attributes_by_marker() {
        let m = BATCH_MARKER;
        let key = "AOE_INSTANCE_ID";
        let id = entry(key, "abc123");
        // (output, sessions, expected per session: None = not covered by the
        // run at all, Some(None) = covered and unset)
        let cases = vec![
            (marked(0, &id), &["s1"][..], vec![Some(Some("abc123"))]),
            // Covered but unset: the block is empty, or holds other keys only.
            (marked(0, ""), &["s1"][..], vec![Some(None)]),
            (
                marked(0, &entry(AOE_CAPTURED_SESSION_ID_KEY, "other")),
                &["s1"][..],
                vec![Some(None)],
            ),
            (
                marked(0, "unset AOE_INSTANCE_ID;\n"),
                &["s1"][..],
                vec![Some(None)],
            ),
            (
                marked(0, &entry(key, "value=with=equals")),
                &["s1"][..],
                vec![Some(Some("value=with=equals"))],
            ),
            // tmux escapes quotes and backslashes inside the value; the
            // reading must undo that rather than stop at the first quote.
            (
                marked(0, &entry(key, r#"a\"b\\c"#)),
                &["s1"][..],
                vec![Some(Some(r#"a"b\c"#))],
            ),
            // A session lacking the variable must not shift the rest.
            (
                format!("{m}0\n{id}{m}1\n{m}2\n{}", entry(key, "xyz789")),
                &["s1", "s2", "s3"][..],
                vec![Some(Some("abc123")), Some(None), Some(Some("xyz789"))],
            ),
            // tmux aborts the list at a failing segment, so sessions past it
            // produce no marker and must read as uncovered (the caller
            // re-reads them) rather than as unset.
            (
                format!("{m}0\n{id}"),
                &["s1", "s2"][..],
                vec![Some(Some("abc123")), None],
            ),
            (String::new(), &["s1", "s2"][..], vec![None, None]),
            (id.clone(), &["s1"][..], vec![None]),
            // A block for a session that was not asked about is ignored.
            (
                format!("{m}9\n{}{m}0\n{id}", entry(key, "nope")),
                &["s1"][..],
                vec![Some(Some("abc123"))],
            ),
            (
                format!("  {m}0  \n{id}"),
                &["s1"][..],
                vec![Some(Some("abc123"))],
            ),
            // A line tmux could not have written leaves the block ambiguous,
            // so it drops out and the caller re-reads the session.
            (format!("{m}0\nnot an entry\n{id}"), &["s1"][..], vec![None]),
            // A repeated marker did not come from tmux; neither reading of
            // that block is trustworthy.
            (
                format!("{m}0\n{id}{m}0\n{}", entry(key, "second")),
                &["s1"][..],
                vec![None],
            ),
            // Jerome #3628: a second record for the requested key is a shape
            // tmux cannot emit, so the block loses its exact-read fallback
            // unless it drops out here.
            (
                format!("{m}0\n{id}{}", entry(key, "second")),
                &["s1"][..],
                vec![None],
            ),
            // Trailing text after the closing quote means the record is not
            // what tmux wrote.
            (
                format!("{m}0\nAOE_INSTANCE_ID=\"real\"; export AOE_INSTANCE_ID; junk\n"),
                &["s1"][..],
                vec![None],
            ),
            // A sanitized name can no longer claim a colliding session: the
            // marker is the batch index, so the truncated run leaves the ASCII
            // session uncovered instead of handing it the Unicode session's
            // block.
            (
                format!("{m}0\n{}", entry(key, "cafe-id")),
                &["aoe_caf\u{e9}", "aoe_caf_"][..],
                vec![Some(Some("cafe-id")), None],
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

    /// #3616: an unrelated multiline value emits continuation lines that can
    /// read as `KEY=...` or as a marker. They belong to the variable that
    /// opened the quote and must not be read as entries of their own.
    #[test]
    fn test_parse_batch_output_ignores_multiline_continuations() {
        let m = BATCH_MARKER;
        let key = "AOE_INSTANCE_ID";
        let cases = vec![
            // The key is set and a later variable's continuation claims it.
            (
                format!(
                    "{m}0\n{}{}",
                    entry(key, "real-id"),
                    entry("ZZZ", "unrelated\nAOE_INSTANCE_ID=spoofed-id"),
                ),
                &["s1"][..],
                vec![Some(Some("real-id"))],
            ),
            // The key is unset and an earlier variable's continuation invents
            // it. Sorted output puts that continuation where the real entry
            // would have been.
            (
                format!("{m}0\n{}", entry("AAA", "x\nAOE_INSTANCE_ID=spoofed-id")),
                &["s1"][..],
                vec![Some(None)],
            ),
            // A continuation that imitates the next session's marker must not
            // reattribute the rest of the run.
            (
                format!(
                    "{m}0\n{}{m}1\n{}",
                    entry("ZZZ", &format!("x\n{m}1\nAOE_INSTANCE_ID=spoofed-id")),
                    entry(key, "s2-id"),
                ),
                &["s1", "s2"][..],
                vec![Some(None), Some(Some("s2-id"))],
            ),
            // Escaped quotes cannot close the value early to fake an entry.
            (
                format!(
                    "{m}0\n{}",
                    entry(
                        "NASTY",
                        "a\\\"; export ZZZ;\nAOE_INSTANCE_ID=\\\"spoofed-id\\\""
                    ),
                ),
                &["s1"][..],
                vec![Some(None)],
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

    /// Exercise framing against a real tmux under both common client locales.
    /// The printable batch marker must survive either locale. Newline rendering
    /// inside a value is tmux-version dependent, so parser safety is asserted
    /// from the decoded record rather than from one raw formatting shape.
    #[test]
    #[serial_test::serial]
    fn test_batch_output_frames_records_against_a_real_tmux() {
        let session = crate::tmux::test_helpers::TmuxTestSession::new("aoe_env_batch_probe");
        let name = session.name();
        let created = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", name, "sh"])
            .output();
        if !created.is_ok_and(|out| out.status.success()) {
            eprintln!("skipping: tmux unavailable");
            return;
        }

        set_hidden_env(name, AOE_INSTANCE_ID_KEY, "real-id").unwrap();
        set_hidden_env(name, "ZZZ", "unrelated\nAOE_INSTANCE_ID=spoofed-id").unwrap();

        for locale in ["C", "C.UTF-8"] {
            let output = crate::tmux::tmux_command()
                .env("LC_ALL", locale)
                .args(batch_args(&[name]))
                .output()
                .unwrap();
            if !output.status.success() && locale == "C.UTF-8" {
                eprintln!("skipping unavailable locale {locale}");
                continue;
            }
            assert!(
                output.status.success(),
                "{locale}: tmux failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(stdout.lines().next(), Some("@0"), "{locale}: {stdout:?}");
            let parsed = parse_batch_output(&stdout, &[name], AOE_INSTANCE_ID_KEY);
            assert_eq!(
                parsed.get(name).map(|v| v.as_deref()),
                Some(Some("real-id")),
                "{locale}: {stdout:?}"
            );
        }
    }

    /// #3628 review: under a non-UTF-8 client tmux prints `aoe_..caf\u{e9}` as
    /// `aoe_..caf_`, so a name-based marker let the Unicode session's block be
    /// attributed to the live ASCII session whose name it now matched. A
    /// repeated marker catches that only when the twin marker arrives; tmux
    /// aborts the list at a failing segment, so here it never does. Batch
    /// indices are injective under that rewrite, so the block stays with the
    /// session that produced it and the unreached one falls back.
    #[test]
    #[serial_test::serial]
    fn test_batch_marker_survives_a_sanitized_name_collision() {
        let base = format!("aoe_env_collide_{}", std::process::id());
        // `sanitize_session_name` keeps any Unicode alphanumeric, and tmux
        // rewrites the last character of this one to `_`, producing `ascii`.
        let unicode =
            crate::tmux::test_helpers::TmuxTestSession::from_name(format!("{base}\u{e9}"));
        let ascii = crate::tmux::test_helpers::TmuxTestSession::from_name(format!("{base}_"));
        for (session, id) in [(&unicode, "unicode-id"), (&ascii, "ascii-id")] {
            let created = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", session.name(), "sh"])
                .output();
            if !created.is_ok_and(|out| out.status.success()) {
                eprintln!("skipping: tmux unavailable");
                return;
            }
            set_hidden_env(session.name(), AOE_INSTANCE_ID_KEY, id).unwrap();
        }

        let sanitized = crate::tmux::tmux_command()
            .env("LC_ALL", "C")
            .args([
                "display-message",
                "-p",
                "-t",
                unicode.name(),
                "#{session_name}",
            ])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&sanitized.stdout).trim() != ascii.name() {
            eprintln!("skipping: this tmux client does not sanitize the name");
            return;
        }

        // The middle session does not exist, so tmux aborts the list there and
        // the ASCII session's own marker is never printed.
        let missing = format!("{base}_gone");
        let names = [unicode.name(), missing.as_str(), ascii.name()];
        let output = crate::tmux::tmux_command()
            .env("LC_ALL", "C")
            .args(batch_args(&names))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_batch_output(&stdout, &names, AOE_INSTANCE_ID_KEY);
        assert_eq!(
            names.map(|n| parsed.get(n).map(|v| v.as_deref())),
            // The unreached ASCII session is what a name-based marker used to
            // fill in with the Unicode session's value. The middle name never
            // existed, so reporting it unset matches what re-reading it gives.
            [Some(Some("unicode-id")), Some(None), None],
            "{stdout:?}"
        );
    }

    #[test]
    fn test_get_hidden_env_batch_empty_input() {
        let result = get_hidden_env_batch(&[], "KEY");
        assert_eq!(result.len(), 0);
    }
}
