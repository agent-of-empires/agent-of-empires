//! Hidden environment variable helpers for tmux sessions
//!
//! This module provides utilities to get and set hidden environment variables
//! in tmux sessions using the `-h` flag. Hidden variables are not inherited by
//! child processes, making them ideal for storing session metadata.

use anyhow::bail;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub const AOE_INSTANCE_ID_KEY: &str = "AOE_INSTANCE_ID";
pub const AOE_CAPTURED_SESSION_ID_KEY: &str = "AOE_CAPTURED_SESSION_ID";

const ENV_CACHE_TTL: Duration = Duration::from_secs(30);
const ENV_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(5);

struct EnvCacheEntry {
    value: Option<String>,
    fetched_at: Instant,
}

struct EnvCache {
    entries: Option<HashMap<(String, String), EnvCacheEntry>>,
}

static ENV_CACHE: RwLock<EnvCache> = RwLock::new(EnvCache { entries: None });

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

    invalidate_cache_entry(session_name, key);
    Ok(())
}

/// Get a hidden environment variable from a tmux session.
///
/// Both hits and misses are cached to reduce subprocess spawns: positive
/// results use [`ENV_CACHE_TTL`] (30s), negative results (var not set)
/// use [`ENV_NEGATIVE_CACHE_TTL`] (5s).
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

    let value = fetch_hidden_env(session_name, key);

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

fn fetch_hidden_env(session_name: &str, key: &str) -> Option<String> {
    let output = crate::tmux::tmux_command()
        .args(["show-environment", "-h", "-t", session_name, key])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();

    // tmux outputs "-KEY" when the variable is unset
    if line.starts_with('-') {
        return None;
    }

    if let Some((_, value)) = line.split_once('=') {
        Some(value.to_string())
    } else {
        None
    }
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

    invalidate_cache_entry(session_name, key);
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

fn invalidate_cache_entry(session_name: &str, key: &str) {
    if let Ok(mut cache) = ENV_CACHE.write() {
        if let Some(entries) = &mut cache.entries {
            entries.remove(&(session_name.to_string(), key.to_string()));
        }
    }
}

/// Get hidden environment variables from multiple sessions in a single tmux command
///
/// Attempts to batch-read from all sessions with a single command. Falls back to
/// sequential reads if the batch command fails.
///
/// Returns a vector of (session_name, value) tuples in the same order as input.
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
}
