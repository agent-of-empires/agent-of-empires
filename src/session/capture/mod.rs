//! Session ID capture for every supported agent, plus the shared exclusion and validation rules.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use uuid::Uuid;

mod claude;
mod codex;
mod gemini;
mod hermes;
mod kimi;
mod omp;
mod opencode;
mod pi;
mod prime;

#[cfg(test)]
pub(crate) use claude::encode_claude_project_path;
pub(crate) use claude::{
    claude_home_for_host_environment, claude_host_transcript_confirmed_absent,
};
pub(crate) use codex::codex_poll_fn_sandboxed_store;
pub(crate) use gemini::gemini_poll_fn_sandboxed_store;
pub(crate) use hermes::hermes_poll_fn_sandboxed_store;
pub(crate) use kimi::kimi_poll_fn_sandboxed_store;
pub(crate) use omp::*;
pub(crate) use opencode::preassign_opencode_session_id;
pub(crate) use pi::pi_sidecar_poll_fn;
pub(crate) use prime::{prime_agent_poll_fn_sandboxed, PrimeRootPublication};

/// Canonicalizes an existing path, else normalizes lexically so an unnormalized
/// spelling of a deleted directory still compares equal.
pub(crate) fn canonicalize_or_raw(path: &str) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| crate::git::template::lexical_normalize(Path::new(path)))
}

/// The capture boundary check that keeps invalid ids out of storage.
pub(crate) fn validated_session_id(id: String) -> Option<String> {
    if is_valid_session_id(&id) {
        Some(id)
    } else {
        tracing::warn!(target: "session.capture", "Captured session ID failed validation: {:?}", id);
        None
    }
}

/// A UUID v4 for pinning an agent session id at launch.
pub(crate) fn generate_session_uuid() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) const MAX_SESSION_ID_LEN: usize = 256;

pub(crate) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id.len() <= MAX_SESSION_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Ids claimed by other live instances plus `extra` ids this instance cleared
/// but that may still be on disk.
pub(crate) fn compose_exclusion(
    current_instance_id: &str,
    extra: &HashSet<String>,
) -> HashSet<String> {
    compose_exclusion_in(
        current_instance_id,
        extra,
        &crate::tmux::LiveSessionSnapshot::new(),
    )
}

fn compose_exclusion_in(
    current_instance_id: &str,
    extra: &HashSet<String>,
    live: &crate::tmux::LiveSessionSnapshot,
) -> HashSet<String> {
    let mut set = build_exclusion_set(current_instance_id, live);
    set.extend(extra.iter().cloned());
    set
}

/// [`compose_exclusion`] plus every conversation id parked by a same-project
/// peer's engine swap; the peer still owns those whatever its current tool.
pub(crate) fn compose_exclusion_with_persisted_peers(
    current_instance_id: &str,
    current_project_path: &str,
    profile: &str,
    retroactive_capture_excludes: &HashSet<String>,
) -> HashSet<String> {
    // One tmux observation for the whole pass; per-instance probes fork each time.
    let live = crate::tmux::LiveSessionSnapshot::new();
    let mut set = compose_exclusion_in(current_instance_id, retroactive_capture_excludes, &live);
    let Ok(storage) = crate::session::storage::Storage::new_unwatched(profile) else {
        return set;
    };
    let Ok(instances) = storage.load() else {
        return set;
    };
    // Canonical comparison: older worktree sessions stored unnormalized paths.
    let canonical_current = canonicalize_or_raw(current_project_path);
    for inst in instances {
        if inst.id == current_instance_id
            || canonicalize_or_raw(&inst.project_path) != canonical_current
        {
            continue;
        }
        set.extend(
            inst.prior_tool_session_ids
                .values()
                .filter_map(|parked| parked.agent_session_id.clone())
                .filter(|sid| !sid.is_empty()),
        );
    }
    set
}

/// Session ids captured by live AoE panes owned by other instances.
fn build_exclusion_set(
    current_instance_id: &str,
    live: &crate::tmux::LiveSessionSnapshot,
) -> HashSet<String> {
    let Some(names) = live.names() else {
        return HashSet::new();
    };
    let aoe_sessions: Vec<&str> = names
        .filter(|name| {
            name.starts_with(crate::tmux::SESSION_PREFIX)
                && !name.starts_with(crate::tmux::TOOL_PREFIX)
        })
        .collect();
    if aoe_sessions.is_empty() {
        return HashSet::new();
    }
    let instance_ids = crate::tmux::env::get_hidden_env_batch(
        &aoe_sessions,
        crate::tmux::env::AOE_INSTANCE_ID_KEY,
    );
    let other_sessions: Vec<&str> = instance_ids
        .iter()
        .filter(|(_, owner)| {
            owner
                .as_deref()
                .is_some_and(|owner| owner != current_instance_id)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if other_sessions.is_empty() {
        return HashSet::new();
    }
    crate::tmux::env::get_hidden_env_batch(
        &other_sessions,
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
    )
    .into_iter()
    .filter_map(|(_, id)| id)
    .collect()
}

/// Runs `cmd` to completion with a deadline and a stdout byte cap, killing it on timeout.
pub(super) fn run_with_timeout_limit(
    mut cmd: std::process::Command,
    timeout: Duration,
    label: &str,
    max_stdout_bytes: usize,
) -> Result<Vec<u8>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", label))?;

    let stdout_pipe = child.stdout.take();
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let buf = stdout_pipe.map(|reader| {
            let mut buf = Vec::new();
            reader
                .take(max_stdout_bytes.saturating_add(1) as u64)
                .read_to_end(&mut buf)
                .ok();
            buf
        });
        let _ = stdout_tx.send(buf);
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!("{} timed out", label));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(anyhow::anyhow!("Failed to wait on {}: {}", label, error));
            }
        }
    };

    // A grandchild holding the stdout pipe would block the drain forever, so it
    // is bounded by the remaining deadline and the reader thread is left detached.
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let stdout_bytes = stdout_rx
        .recv_timeout(remaining)
        .ok()
        .flatten()
        .unwrap_or_default();
    if stdout_bytes.len() > max_stdout_bytes {
        anyhow::bail!("{} exceeded its stdout limit", label);
    }
    if !status.success() {
        anyhow::bail!("{} command failed", label);
    }
    Ok(stdout_bytes)
}

#[cfg(test)]
pub(super) mod test_support {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    #[cfg(unix)]
    pub(crate) fn open_fifo_guard(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap()
    }

    #[cfg(unix)]
    pub(crate) fn make_fifo(path: &Path) {
        nix::unistd::mkfifo(
            path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
    }

    pub(crate) fn capture_floor(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    pub(crate) fn set_mtime_ms(path: &Path, millis: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(millis)),
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn canonicalize_or_raw_normalizes_deleted_dirs_lexically() {
        assert_eq!(
            canonicalize_or_raw("/nonexistent-aoe-test/decoy/../wt"),
            canonicalize_or_raw("/nonexistent-aoe-test/wt"),
        );
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("x")).unwrap();
        let spelled = temp.path().join("x").join("..");
        assert_eq!(
            canonicalize_or_raw(&spelled.to_string_lossy()),
            std::fs::canonicalize(temp.path()).unwrap()
        );
    }

    #[test]
    #[serial_test::serial]
    fn parked_conversation_exclusion_survives_alias_config_changes() {
        const PROFILE: &str = "capture-parked-alias-test";
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(PROFILE);
        let mut config = crate::session::Config::default();
        config
            .session
            .agent_detect_as
            .insert("claude-personal".to_string(), "claude".to_string());
        crate::tmux::status_rules::install_from_config(PROFILE, &config);

        let project = "/tmp/capture-parked-alias";
        let parked_sid = "88888888-8888-4888-8888-888888888888";
        let mut peer = crate::session::Instance::new("peer", project);
        peer.source_profile = PROFILE.to_string();
        peer.tool = "codex".to_string();
        peer.prior_tool_session_ids.insert(
            "claude-personal".to_string(),
            crate::session::instance::PriorToolSession {
                agent_session_id: Some(parked_sid.to_string()),
                acp_session_id: None,
            },
        );
        let storage = crate::session::Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|instances, groups| {
                *instances = vec![peer.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(instances, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
        // Ownership must survive the alias being removed before the peer swaps back.
        crate::tmux::status_rules::install_from_config(PROFILE, &crate::session::Config::default());

        let exclusions =
            compose_exclusion_with_persisted_peers("current", project, PROFILE, &HashSet::new());
        assert!(exclusions.contains(parked_sid));
    }

    #[test]
    fn test_is_valid_session_id() {
        for valid in [
            "abc-123",
            "session_id.v2",
            "a",
            "ABC-def_123.456",
            "20260429_193246_adcddd",
        ] {
            assert!(is_valid_session_id(valid), "{valid}");
        }
        let too_long = "x".repeat(257);
        for invalid in [
            "",
            "-looks-like-an-option",
            "bad id!@#",
            "semi;colon",
            "back`tick",
            "path/slash",
            too_long.as_str(),
        ] {
            assert!(!is_valid_session_id(invalid), "{invalid}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_bounds_drain_when_grandchild_holds_pipe() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 10 & printf done"]);
        let start = Instant::now();
        let out = run_with_timeout_limit(
            cmd,
            Duration::from_millis(500),
            "grandchild-test",
            usize::MAX,
        )
        .expect("the sh child exits quickly, so a buffer is produced");
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(out.is_empty() || out == b"done");
    }
}
