//! tmux session environment helpers; `-h` variables are not inherited by children.

use anyhow::bail;
use std::collections::{HashMap, HashSet};

pub const AOE_INSTANCE_ID_KEY: &str = "AOE_INSTANCE_ID";
pub const AOE_CAPTURED_SESSION_ID_KEY: &str = "AOE_CAPTURED_SESSION_ID";
pub const AOE_OMP_CAPTURE_META_KEY: &str = "AOE_OMP_CAPTURE_META";
pub const AOE_OMP_LAUNCH_ID_KEY: &str = "AOE_OMP_LAUNCH_ID";
pub const AOE_OMP_CAPTURE_READY_KEY: &str = "AOE_OMP_CAPTURE_READY";

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
    // `-KEY` means the variable is marked for removal.
    if line.starts_with('-') {
        return None;
    }
    line.split_once('=').map(|(_, value)| value.to_string())
}

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

/// Join per-entry argv segments with `;` into one tmux invocation.
fn chain_segments<T>(entries: &[T], segment: impl Fn(usize, &T) -> Vec<String>) -> Vec<String> {
    let mut args = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            args.push(";".to_string());
        }
        args.extend(segment(i, entry));
    }
    args
}

/// Run a chained write; on failure fall back to `sequential` per entry, logging
/// (not propagating) each failure.
fn run_write_batch<T>(
    entries: &[T],
    what: &str,
    segment: impl Fn(usize, &T) -> Vec<String>,
    sequential: impl Fn(&T) -> anyhow::Result<()>,
) {
    if entries.is_empty() {
        return;
    }
    let failure = match crate::tmux::tmux_command()
        .args(chain_segments(entries, segment))
        .output()
    {
        Ok(out) if out.status.success() => return,
        Ok(out) => format!("failed (exit {})", out.status),
        Err(e) => format!("error: {e}"),
    };
    tracing::debug!(target: "tmux.command",
        "Batch tmux {what} {failure}, falling back to sequential calls"
    );
    for entry in entries {
        if let Err(e) = sequential(entry) {
            tracing::debug!(target: "tmux.command", "Sequential tmux {what} failed: {e}");
        }
    }
}

/// Best-effort: failures are logged, never returned.
pub fn remove_hidden_env_batch(entries: &[(&str, &str)]) -> anyhow::Result<()> {
    run_write_batch(
        entries,
        "set-environment -u",
        |_, (session, key)| strings(["set-environment", "-h", "-u", "-t", session, key]),
        |(session, key)| remove_hidden_env(session, key),
    );
    Ok(())
}

/// Best-effort: failures are logged, never returned.
pub fn set_hidden_env_batch(entries: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    run_write_batch(
        entries,
        "set-environment",
        |_, (session, key, value)| strings(["set-environment", "-h", "-t", session, key, value]),
        |(session, key, value)| set_hidden_env(session, key, value),
    );
    Ok(())
}

fn strings<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.map(str::to_string).to_vec()
}

/// Marker line prefix ahead of each batched segment. It carries the batch index,
/// not the session name: tmux rewrites non-ASCII bytes to `_` for a non-UTF-8
/// client, and digits survive that rewrite injectively.
const BATCH_MARKER: char = '@';

/// Read `key` from many sessions in one tmux call, in input order. tmux aborts a
/// `;` chain at the first failing command, so each segment reads the whole hidden
/// environment (which cannot fail on an unset key); sessions whose marker never
/// came back are re-read sequentially.
pub fn get_hidden_env_batch(session_names: &[&str], key: &str) -> Vec<(String, Option<String>)> {
    if session_names.is_empty() {
        return Vec::new();
    }
    let output = crate::tmux::tmux_command()
        .args(batch_args(session_names))
        .output();
    let mut covered = match output {
        Ok(out) => parse_batch_output(&String::from_utf8_lossy(&out.stdout), session_names, key),
        Err(e) => {
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
            let value = covered.remove(name).unwrap_or_else(|| {
                repaired += 1;
                get_hidden_env(name, key)
            });
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

fn batch_args(session_names: &[&str]) -> Vec<String> {
    chain_segments(session_names, |i, session| {
        let mut args = strings(["display-message", "-p", "-t", session]);
        args.push(format!("{BATCH_MARKER}{i}"));
        args.extend(strings([
            ";",
            "show-environment",
            "-h",
            "-s",
            "-t",
            session,
        ]));
        args
    })
}

/// `key`'s value per session whose marker came back (`None` = unset). A session
/// that is absent was never reached or its block did not parse; the caller
/// must re-read it.
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
            // tmux prints each marker once, and each key once per session.
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
                    if !read_key.insert(session) {
                        unparsed.insert(session);
                    }
                    values.insert(session, value);
                }
            }
            rest = after_entry;
            continue;
        }
        // An unknown marker ends the block; any other stray line spoils it.
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

/// Consume one `show-environment -s` record: `(name, value, remainder)`. `-s`
/// quotes and escapes values, so scanning to the unescaped quote stops a
/// multi-line value from forging a `KEY=` line; the `; export <name>;` tail is
/// required.
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

fn split_line(input: &str) -> (&str, &str) {
    match input.split_once('\n') {
        Some((line, rest)) => (line.strip_suffix('\r').unwrap_or(line), rest),
        None => (input, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &str) -> String {
        format!("{name}=\"{value}\"; export {name};\n")
    }

    fn marked(index: usize, body: &str) -> String {
        format!("{BATCH_MARKER}{index}\n{body}")
    }

    #[test]
    fn test_parse_batch_output() {
        let m = BATCH_MARKER;
        let key = "AOE_INSTANCE_ID";
        let id = entry(key, "abc123");
        let cases = vec![
            (marked(0, &id), &["s1"][..], vec![Some(Some("abc123"))]),
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
            (
                marked(0, &entry(key, r#"a\"b\\c"#)),
                &["s1"][..],
                vec![Some(Some(r#"a"b\c"#))],
            ),
            (
                format!("{m}0\n{id}{m}1\n{m}2\n{}", entry(key, "xyz789")),
                &["s1", "s2", "s3"][..],
                vec![Some(Some("abc123")), Some(None), Some(Some("xyz789"))],
            ),
            (
                format!("{m}0\n{id}"),
                &["s1", "s2"][..],
                vec![Some(Some("abc123")), None],
            ),
            (String::new(), &["s1", "s2"][..], vec![None, None]),
            (id.clone(), &["s1"][..], vec![None]),
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
            (format!("{m}0\nnot an entry\n{id}"), &["s1"][..], vec![None]),
            (
                format!("{m}0\n{id}{m}0\n{}", entry(key, "second")),
                &["s1"][..],
                vec![None],
            ),
            (
                format!("{m}0\n{id}{}", entry(key, "second")),
                &["s1"][..],
                vec![None],
            ),
            (
                format!("{m}0\nAOE_INSTANCE_ID=\"real\"; export AOE_INSTANCE_ID; junk\n"),
                &["s1"][..],
                vec![None],
            ),
            (
                format!("{m}0\n{}", entry(key, "cafe-id")),
                &["aoe_caf\u{e9}", "aoe_caf_"][..],
                vec![Some(Some("cafe-id")), None],
            ),
            (
                format!(
                    "{m}0\n{}{}",
                    entry(key, "real-id"),
                    entry("ZZZ", "unrelated\nAOE_INSTANCE_ID=spoofed-id"),
                ),
                &["s1"][..],
                vec![Some(Some("real-id"))],
            ),
            (
                format!("{m}0\n{}", entry("AAA", "x\nAOE_INSTANCE_ID=spoofed-id")),
                &["s1"][..],
                vec![Some(None)],
            ),
            (
                format!(
                    "{m}0\n{}{m}1\n{}",
                    entry("ZZZ", &format!("x\n{m}1\nAOE_INSTANCE_ID=spoofed-id")),
                    entry(key, "s2-id"),
                ),
                &["s1", "s2"][..],
                vec![Some(None), Some(Some("s2-id"))],
            ),
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

    #[test]
    #[serial_test::serial]
    fn test_batch_output_frames_records_against_a_real_tmux() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        crate::tmux::test_helpers::require_tmux!();
        let session = crate::tmux::test_helpers::TmuxTestSession::new("aoe_env_batch_probe");
        let name = session.name();
        let created = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", name, "sh"])
            .output()
            .expect("create tmux fixture");
        assert!(
            created.status.success(),
            "tmux fixture: {}",
            String::from_utf8_lossy(&created.stderr)
        );

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

    #[test]
    #[serial_test::serial]
    fn test_batch_marker_survives_a_sanitized_name_collision() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        crate::tmux::test_helpers::require_tmux!();
        let base = format!("aoe_env_collide_{}", std::process::id());
        let unicode =
            crate::tmux::test_helpers::TmuxTestSession::from_name(format!("{base}\u{e9}"));
        let ascii = crate::tmux::test_helpers::TmuxTestSession::from_name(format!("{base}_"));
        for (session, id) in [(&unicode, "unicode-id"), (&ascii, "ascii-id")] {
            let created = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", session.name(), "sh"])
                .output()
                .expect("create tmux fixture");
            assert!(
                created.status.success(),
                "tmux fixture: {}",
                String::from_utf8_lossy(&created.stderr)
            );
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
