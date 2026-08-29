//! Hidden environment variable helpers for tmux sessions
//!
//! This module provides utilities to get and set hidden environment variables
//! in tmux sessions using the `-h` flag. Hidden variables are not inherited by
//! child processes, making them ideal for storing session metadata.

use anyhow::bail;
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
#[cfg(any(test, feature = "test-support"))]
use std::sync::RwLock;
#[cfg(any(test, feature = "test-support"))]
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
#[error("tmux session disappeared: {0}")]
struct MissingSession(String);

fn missing_session_stderr(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("can't find session:") || line.starts_with("no such session:")
    }) || crate::tmux::tmux_no_server_running(stderr.as_bytes())
}
pub(crate) fn is_missing_session_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<MissingSession>().is_some()
}

pub const AOE_INSTANCE_ID_KEY: &str = "AOE_INSTANCE_ID";
pub const AOE_CAPTURED_SESSION_ID_KEY: &str = "AOE_CAPTURED_SESSION_ID";
pub const AOE_SESSION_STORE_NAMESPACE_KEY: &str = "AOE_SESSION_STORE_NAMESPACE";
pub const AOE_OMP_CAPTURE_META_KEY: &str = "AOE_OMP_CAPTURE_META";
pub const AOE_OMP_LAUNCH_ID_KEY: &str = "AOE_OMP_LAUNCH_ID";
pub const AOE_OMP_CAPTURE_READY_KEY: &str = "AOE_OMP_CAPTURE_READY";

#[cfg(any(test, feature = "test-support"))]
const ENV_CACHE_TTL: Duration = Duration::from_secs(30);
#[cfg(any(test, feature = "test-support"))]
const ENV_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(5);

#[cfg(any(test, feature = "test-support"))]
struct EnvCacheEntry {
    value: Option<String>,
    fetched_at: Instant,
}

#[cfg(any(test, feature = "test-support"))]
struct EnvCache {
    entries: Option<HashMap<(String, String), EnvCacheEntry>>,
}

#[cfg(any(test, feature = "test-support"))]
static ENV_CACHE: RwLock<EnvCache> = RwLock::new(EnvCache { entries: None });

/// Set a hidden environment variable in a tmux session
///
/// Hidden variables (set with `-h`) are not inherited by child processes.
pub fn set_hidden_env(session_name: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let mut command = crate::tmux::tmux_command();
    command.args(["set-environment", "-h", "-t", session_name, key, value]);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if missing_session_stderr(&stderr) {
            return Err(MissingSession(session_name.to_string()).into());
        }
        bail!(
            "tmux set-environment -h -t '{}' {}: exit {}: {}",
            session_name,
            key,
            output.status,
            stderr.trim()
        );
    }

    invalidate_cache_entry(session_name, key);
    Ok(())
}

/// Get a hidden environment variable from a tmux session.
///
/// Both hits and misses are cached to reduce subprocess spawns: positive
/// results use [`ENV_CACHE_TTL`] (30s), negative results (var not set)
/// use [`ENV_NEGATIVE_CACHE_TTL`] (5s).
#[cfg(any(test, feature = "test-support"))]
pub fn get_hidden_env(session_name: &str, key: &str) -> Option<String> {
    let cache_key = (session_name.to_string(), key.to_string());

    if let Ok(cache) = ENV_CACHE.read() {
        if let Some(entries) = &cache.entries {
            if let Some(entry) = entries.get(&cache_key) {
                let ttl = if entry.value.is_some() {
                    ENV_CACHE_TTL
                } else {
                    ENV_NEGATIVE_CACHE_TTL
                };
                if entry.fetched_at.elapsed() < ttl {
                    return entry.value.clone();
                }
            }
        }
    }

    if let Ok(mut cache) = ENV_CACHE.write() {
        if let Some(entries) = &mut cache.entries {
            entries.remove(&cache_key);
        }
    }

    let value = get_hidden_env_uncached(session_name, key);

    if let Ok(mut cache) = ENV_CACHE.write() {
        let entries = cache.entries.get_or_insert_with(HashMap::new);
        entries.insert(
            cache_key,
            EnvCacheEntry {
                value: value.clone(),
                fetched_at: Instant::now(),
            },
        );
    }

    value
}

pub(crate) fn get_hidden_env_uncached(session_name: &str, key: &str) -> Option<String> {
    fetch_env_uncached(session_name, key, true)
}

/// Read a hidden value without collapsing tmux failures into a missing key.
pub(crate) fn get_hidden_env_strict(
    session_name: &str,
    key: &str,
) -> anyhow::Result<Option<String>> {
    let mut command = crate::tmux::tmux_query_command();
    command.args(["show-environment", "-h", "-t", session_name, key]);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if missing_session_stderr(&stderr) {
            return Err(MissingSession(session_name.to_string()).into());
        }
        let missing = format!("unknown variable: {key}");
        if stderr.lines().any(|line| line.trim() == missing) {
            return Ok(None);
        }
        bail!("Failed to read hidden env var from {session_name}: {stderr}");
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    if line.starts_with('-') {
        return Ok(None);
    }
    Ok(line.split_once('=').map(|(_, value)| value.to_string()))
}

pub(crate) fn get_env_uncached(session_name: &str, key: &str) -> Option<String> {
    fetch_env_uncached(session_name, key, false)
}

fn fetch_env_uncached(session_name: &str, key: &str, hidden: bool) -> Option<String> {
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
    let mut command = crate::tmux::tmux_command();
    command.args(["set-environment", "-h", "-u", "-t", session_name, key]);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if missing_session_stderr(&stderr) {
            return Err(MissingSession(session_name.to_string()).into());
        }
        bail!("Failed to remove hidden env var: {}", stderr);
    }

    invalidate_cache_entry(session_name, key);
    Ok(())
}

/// Remove hidden environment variables from multiple sessions with a single tmux command.
/// Each tuple is `(session_name, key)`. Falls back to per-entry calls on
/// batch failure, attempts every entry, and reports any fallback failure.
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
    let mut command = crate::tmux::tmux_command();
    command.args(&str_args);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command);

    match output {
        Ok(out) if out.status.success() => {
            for (session_name, key) in entries {
                invalidate_cache_entry(session_name, key);
            }
            Ok(())
        }
        Ok(out) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment -u failed (exit {}), falling back to sequential unsets",
                out.status
            );
            sequential_remove_fallback(entries)
        }
        Err(e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment -u error: {}, falling back to sequential unsets",
                e
            );
            sequential_remove_fallback(entries)
        }
    }
}

fn sequential_remove_fallback(entries: &[(&str, &str)]) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for (session_name, key) in entries {
        if let Err(error) = remove_hidden_env(session_name, key) {
            if !is_missing_session_error(&error) {
                failures.push(format!("{session_name}:{key}: {error}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "Failed to remove {} hidden env vars: {}",
            failures.len(),
            failures.join("; ")
        )
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
    let mut command = crate::tmux::tmux_command();
    command.args(&str_args);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command);

    match output {
        Ok(out) if out.status.success() => {
            for (session_name, key, _) in entries {
                invalidate_cache_entry(session_name, key);
            }
            Ok(())
        }
        Ok(out) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment failed (exit {}), falling back to sequential writes",
                out.status
            );
            sequential_set_fallback(entries)
        }
        Err(e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux set-environment error: {}, falling back to sequential writes",
                e
            );
            sequential_set_fallback(entries)
        }
    }
}

fn sequential_set_fallback(entries: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for (session_name, key, value) in entries {
        if let Err(error) = set_hidden_env(session_name, key, value) {
            if !is_missing_session_error(&error) {
                failures.push(format!("{session_name}:{key}: {error}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "Failed to set {} hidden env vars: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}
#[cfg(any(test, feature = "test-support"))]
fn invalidate_cache_entry(session_name: &str, key: &str) {
    if let Ok(mut cache) = ENV_CACHE.write() {
        if let Some(entries) = &mut cache.entries {
            entries.remove(&(session_name.to_string(), key.to_string()));
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
fn invalidate_cache_entry(_: &str, _: &str) {}

/// Strictly read several hidden keys for several sessions with one tmux client.
/// A session that disappears during the batch is retried and omitted; every
/// other tmux failure remains visible to the caller.
pub(crate) fn get_hidden_env_keys_batch_strict(
    session_names: &[&str],
    keys: &[&str],
) -> anyhow::Result<Vec<(String, Vec<Option<String>>)>> {
    if session_names.is_empty() || keys.is_empty() {
        return Ok(session_names
            .iter()
            .map(|name| ((*name).to_string(), Vec::new()))
            .collect());
    }

    let mut args = Vec::new();
    for (index, session_name) in session_names.iter().enumerate() {
        if !args.is_empty() {
            args.push(";".to_string());
        }
        args.extend([
            "show-environment".to_string(),
            "-h".to_string(),
            "-t".to_string(),
            (*session_name).to_string(),
            ";".to_string(),
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            (*session_name).to_string(),
            format!("__AOE_ENV_END_{index}__"),
        ]);
    }
    let str_args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut command = crate::tmux::tmux_query_command();
    command.args(&str_args);
    let output = crate::tmux::run_tmux_command_with_timeout(&mut command)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if missing_session_stderr(&stderr) {
            let mut results = Vec::new();
            for session_name in session_names {
                let mut values = Vec::with_capacity(keys.len());
                let mut vanished = false;
                for key in keys {
                    match get_hidden_env_strict(session_name, key) {
                        Ok(value) => values.push(value),
                        Err(error) if is_missing_session_error(&error) => {
                            vanished = true;
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if !vanished {
                    results.push(((*session_name).to_string(), values));
                }
            }
            return Ok(results);
        }
        bail!(
            "Failed to batch-read hidden tmux environment: {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let mut results = Vec::with_capacity(session_names.len());
    for (index, session_name) in session_names.iter().enumerate() {
        let marker = format!("__AOE_ENV_END_{index}__");
        let mut values = vec![None; keys.len()];
        loop {
            let line = lines.next().ok_or_else(|| {
                anyhow::anyhow!("tmux hidden environment batch returned too few sections")
            })?;
            let line = line.trim();
            if line == marker {
                break;
            }
            if let Some((actual, value)) = line.split_once('=') {
                if let Some(position) = keys.iter().position(|key| *key == actual) {
                    values[position] = Some(value.to_string());
                }
            }
        }
        results.push(((*session_name).to_string(), values));
    }
    if lines.next().is_some() {
        anyhow::bail!("tmux hidden environment batch returned too many sections");
    }
    Ok(results)
}

/// Get hidden environment variables from multiple sessions in a single tmux command
///
/// Attempts to batch-read from all sessions with a single command. Falls back to
/// sequential reads if the batch command fails.
///
/// Returns a vector of (session_name, value) tuples in the same order as input.
#[cfg(any(test, feature = "test-support"))]
pub fn get_hidden_env_batch(session_names: &[&str], key: &str) -> Vec<(String, Option<String>)> {
    if session_names.is_empty() {
        return Vec::new();
    }

    // Build a batch tmux command: each segment needs the full
    // `show-environment -h` prefix since `;` is a command separator.
    let mut args: Vec<String> = Vec::new();
    for (i, session_name) in session_names.iter().enumerate() {
        if i > 0 {
            args.push(";".to_string());
        }
        args.push("show-environment".to_string());
        args.push("-h".to_string());
        args.push("-t".to_string());
        args.push(session_name.to_string());
        args.push(key.to_string());
    }

    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = crate::tmux::tmux_command().args(&str_args).output();

    let fallback = || {
        session_names
            .iter()
            .map(|name| (name.to_string(), get_hidden_env(name, key)))
            .collect()
    };

    let results = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_batch_output(&stdout, session_names).unwrap_or_else(|| {
                tracing::debug!(target: "tmux.command", 
                    "Batch env parse failed (line count mismatch for {} sessions), falling back to sequential reads",
                    session_names.len()
                );
                fallback()
            })
        }
        Ok(out) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux show-environment failed (exit {}), falling back to sequential reads",
                out.status
            );
            fallback()
        }
        Err(ref e) => {
            tracing::debug!(target: "tmux.command",
                "Batch tmux show-environment error: {}, falling back to sequential reads",
                e
            );
            fallback()
        }
    };

    if let Ok(mut cache) = ENV_CACHE.write() {
        let entries = cache.entries.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        for (session_name, value) in &results {
            entries.insert(
                (session_name.clone(), key.to_string()),
                EnvCacheEntry {
                    value: value.clone(),
                    fetched_at: now,
                },
            );
        }
    }

    results
}

/// Parse output from batch show-environment command.
///
/// Each session's output is on a separate line in the format "KEY=VALUE" or "-KEY".
/// If the number of output lines does not match the number of sessions (e.g. due to
/// tmux error lines), returns `None` so the caller can fall back to sequential reads.
#[cfg(any(test, feature = "test-support"))]
fn parse_batch_output(
    output: &str,
    session_names: &[&str],
) -> Option<Vec<(String, Option<String>)>> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() != session_names.len() {
        return None;
    }
    let mut results = Vec::new();

    for (i, session_name) in session_names.iter().enumerate() {
        let line = lines[i].trim();
        let value = if line.starts_with('-') {
            None
        } else if let Some((_, val)) = line.split_once('=') {
            Some(val.to_string())
        } else {
            None
        };
        results.push((session_name.to_string(), value));
    }

    Some(results)
}

#[cfg(test)]
fn clear_env_cache() {
    if let Ok(mut cache) = ENV_CACHE.write() {
        cache.entries = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_cache_populate_and_lookup() {
        clear_env_cache();
        let key = ("cache_test_sess".to_string(), "MY_KEY".to_string());

        if let Ok(mut cache) = ENV_CACHE.write() {
            let entries = cache.entries.get_or_insert_with(HashMap::new);
            entries.insert(
                key.clone(),
                EnvCacheEntry {
                    value: Some("cached_val".to_string()),
                    fetched_at: Instant::now(),
                },
            );
        }

        let hit = ENV_CACHE.read().ok().and_then(|c| {
            c.entries
                .as_ref()?
                .get(&key)
                .filter(|e| e.fetched_at.elapsed() < ENV_CACHE_TTL)
                .and_then(|e| e.value.clone())
        });
        assert_eq!(hit, Some("cached_val".to_string()));
        clear_env_cache();
    }

    #[test]
    #[serial]
    fn test_cache_stale_entry_not_returned() {
        clear_env_cache();
        let key = ("stale_sess".to_string(), "MY_KEY".to_string());

        if let Ok(mut cache) = ENV_CACHE.write() {
            let entries = cache.entries.get_or_insert_with(HashMap::new);
            entries.insert(
                key.clone(),
                EnvCacheEntry {
                    value: Some("old_val".to_string()),
                    fetched_at: Instant::now() - Duration::from_secs(60),
                },
            );
        }

        let hit = ENV_CACHE.read().ok().and_then(|c| {
            c.entries
                .as_ref()?
                .get(&key)
                .filter(|e| e.fetched_at.elapsed() < ENV_CACHE_TTL)
                .and_then(|e| e.value.clone())
        });
        assert_eq!(hit, None);
        clear_env_cache();
    }

    #[test]
    #[serial]
    fn test_invalidate_cache_entry_removes_key() {
        clear_env_cache();
        let session = "inv_test_sess";
        let key = "MY_KEY";

        if let Ok(mut cache) = ENV_CACHE.write() {
            let entries = cache.entries.get_or_insert_with(HashMap::new);
            entries.insert(
                (session.to_string(), key.to_string()),
                EnvCacheEntry {
                    value: Some("val".to_string()),
                    fetched_at: Instant::now(),
                },
            );
        }

        invalidate_cache_entry(session, key);

        let exists = ENV_CACHE
            .read()
            .ok()
            .and_then(|c| {
                c.entries
                    .as_ref()
                    .map(|e| e.contains_key(&(session.to_string(), key.to_string())))
            })
            .unwrap_or(false);
        assert!(!exists);
        clear_env_cache();
    }

    #[test]
    fn test_parse_key_value() {
        let output = "AOE_INSTANCE_ID=abc123";
        let result = parse_batch_output(output, &["test_session"]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "test_session");
        assert_eq!(result[0].1, Some("abc123".to_string()));
    }

    #[test]
    fn test_parse_unset_key() {
        let output = "-AOE_INSTANCE_ID";
        let result = parse_batch_output(output, &["test_session"]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "test_session");
        assert_eq!(result[0].1, None);
    }

    #[test]
    fn test_parse_multiple_sessions() {
        let output = "AOE_INSTANCE_ID=abc123\n-AOE_INSTANCE_ID\nAOE_INSTANCE_ID=xyz789";
        let result = parse_batch_output(output, &["session1", "session2", "session3"]).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].1, Some("abc123".to_string()));
        assert_eq!(result[1].1, None);
        assert_eq!(result[2].1, Some("xyz789".to_string()));
    }

    #[test]
    fn test_parse_value_with_equals() {
        let output = "KEY=value=with=equals";
        let result = parse_batch_output(output, &["test_session"]).unwrap();
        assert_eq!(result[0].1, Some("value=with=equals".to_string()));
    }

    #[test]
    fn test_parse_line_count_mismatch_returns_none() {
        let output = "";
        assert!(parse_batch_output(output, &["session1", "session2"]).is_none());

        let output = "VAL1\nVAL2\nVAL3";
        assert!(parse_batch_output(output, &["session1"]).is_none());
    }

    #[test]
    fn test_parse_whitespace_handling() {
        let output = "  AOE_INSTANCE_ID=value123  \n  -AOE_INSTANCE_ID  ";
        let result = parse_batch_output(output, &["session1", "session2"]).unwrap();
        assert_eq!(result[0].1, Some("value123".to_string()));
        assert_eq!(result[1].1, None);
    }

    #[test]
    fn test_get_hidden_env_batch_empty_input() {
        let result = get_hidden_env_batch(&[], "KEY");
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn strict_hidden_env_operations_classify_missing_session() {
        for stderr in [
            "can't find session: missing",
            "no such session: missing",
            "no server running on /tmp/tmux.sock",
            "error connecting to /tmp/tmux.sock (No such file or directory)",
        ] {
            assert!(missing_session_stderr(stderr), "{stderr}");
        }
        assert!(!missing_session_stderr(
            "error connecting to /tmp/tmux.sock (Permission denied)"
        ));
        if !crate::tmux::is_tmux_available() {
            eprintln!("Skipping: tmux not available");
            return;
        }
        let live = format!(
            "{}env_live_{}",
            crate::tmux::SESSION_PREFIX,
            uuid::Uuid::new_v4().simple()
        );
        let output = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &live])
            .output()
            .unwrap();
        assert!(output.status.success());
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }
        let _cleanup = Cleanup(live);
        let missing = format!(
            "{}missing_{}",
            crate::tmux::SESSION_PREFIX,
            uuid::Uuid::new_v4().simple()
        );
        for error in [
            get_hidden_env_strict(&missing, AOE_INSTANCE_ID_KEY).unwrap_err(),
            remove_hidden_env(&missing, AOE_CAPTURED_SESSION_ID_KEY).unwrap_err(),
            set_hidden_env(&missing, AOE_CAPTURED_SESSION_ID_KEY, "captured").unwrap_err(),
        ] {
            assert!(is_missing_session_error(&error), "{error:#}");
        }
        remove_hidden_env_batch(&[(&missing, AOE_CAPTURED_SESSION_ID_KEY)]).unwrap();
        set_hidden_env_batch(&[(&missing, AOE_CAPTURED_SESSION_ID_KEY, "captured")]).unwrap();
    }

    #[test]
    fn strict_hidden_env_read_treats_absent_key_as_none() {
        if !crate::tmux::is_tmux_available() {
            eprintln!("Skipping: tmux not available");
            return;
        }

        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }

        let name = format!(
            "{}env_absent_{}",
            crate::tmux::SESSION_PREFIX,
            uuid::Uuid::new_v4().simple()
        );
        let created = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name])
            .output()
            .expect("create tmux session");
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let _cleanup = Cleanup(name.clone());

        assert_eq!(
            get_hidden_env_strict(&name, AOE_CAPTURED_SESSION_ID_KEY).unwrap(),
            None
        );
        remove_hidden_env_batch(&[(&name, AOE_CAPTURED_SESSION_ID_KEY)]).unwrap();
        assert_eq!(
            get_hidden_env_strict(&name, AOE_CAPTURED_SESSION_ID_KEY).unwrap(),
            None
        );
    }
}
