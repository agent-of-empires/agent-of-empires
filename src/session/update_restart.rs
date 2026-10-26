//! Detects a running agent whose binary was replaced on disk after it launched.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::acp::agent_registry::AgentRegistry;
use crate::process::worker_registry::WorkerRecord;
use crate::session::{Instance, Status};

const MTIME_TTL: Duration = Duration::from_secs(30);

/// Bounded by the number of distinct agent binaries.
static MTIME_CACHE: Mutex<Option<HashMap<String, (Instant, Option<SystemTime>)>>> =
    Mutex::new(None);

/// What a structured worker actually runs: the registered ACP command, which for adapter-backed
/// agents is the adapter (it bundles its own CLI), not the CLI on `PATH`.
fn adapter_command(inst: &Instance, record: &WorkerRecord) -> Option<String> {
    if !inst.command.is_empty() {
        return None;
    }
    AgentRegistry::with_defaults()
        .get(&record.agent_key)
        .map(|spec| spec.command.clone())
}

fn newest_mtime(path: &std::path::Path) -> Option<SystemTime> {
    // A package manager may keep a fixed mtime on the target but recreate the bin symlink.
    let target = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let link = std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok();
    target.max(link)
}

/// Cached by `key`; `path` runs only on a miss, outside the cache lock.
fn cached_mtime(
    key: String,
    path: impl FnOnce() -> Option<std::path::PathBuf>,
) -> Option<SystemTime> {
    {
        let mut guard = MTIME_CACHE.lock().ok()?;
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some((at, mtime)) = cache.get(&key) {
            if at.elapsed() < MTIME_TTL {
                return *mtime;
            }
        }
    }
    let mtime = path().and_then(|path| newest_mtime(&path));
    MTIME_CACHE
        .lock()
        .ok()?
        .get_or_insert_with(HashMap::new)
        .insert(key, (Instant::now(), mtime));
    mtime
}

/// Which adapter copy a spawn runs. With both a PATH copy and a bundled one, spawn runs a version
/// probe to choose, which a list poll must not do, so the offer is withheld instead.
fn pick_adapter(
    path_copy: Option<std::path::PathBuf>,
    bundled: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    match (path_copy, bundled) {
        (Some(path), None) | (None, Some(path)) => Some(path),
        _ => None,
    }
}

fn adapter_mtime(command: &str) -> Option<SystemTime> {
    cached_mtime(format!("acp:{command}"), || {
        let app_dir = crate::session::get_app_dir().ok();
        pick_adapter(
            which::which(command).ok(),
            app_dir
                .as_deref()
                .and_then(|dir| crate::acp::adapters::bundled_adapter_bin(dir, command)),
        )
    })
}

/// Whole seconds, so a binary written in the launch second is not flagged.
fn modified_after(mtime: SystemTime, started_at: DateTime<Utc>) -> bool {
    let modified = mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    modified as i64 > started_at.timestamp()
}

/// An idle row whose agent binary was replaced after launch. Reads only the row and the
/// filesystem, so it is safe to call while painting.
pub fn binary_updated(inst: &Instance) -> bool {
    // A sandboxed agent runs the container's copy, which host files say nothing about.
    if inst.status != Status::Idle || inst.is_archived() || inst.is_trashed() || inst.is_sandboxed()
    {
        return false;
    }
    let stale = if inst.is_structured() {
        crate::process::worker_registry::load(&inst.id)
            .ok()
            .flatten()
            .and_then(|record| {
                let started_at =
                    DateTime::from_timestamp(i64::try_from(record.started_at).ok()?, 0)?;
                let command = adapter_command(inst, &record)?;
                Some(modified_after(adapter_mtime(&command)?, started_at))
            })
    } else {
        // The executable the launch itself ran, so the observer's PATH never decides.
        inst.launch_executable
            .as_ref()
            .zip(inst.started_at)
            .and_then(|(path, started_at)| {
                let mtime = cached_mtime(path.display().to_string(), || Some(path.clone()))?;
                Some(modified_after(mtime, started_at))
            })
    };
    stale.unwrap_or(false)
}

/// Whether to offer "restart to apply update". `resume_available` is the row's
/// `ContextResumeAvailability::Available`; `structured_ready` is a running worker with no pending
/// approvals. The restart must keep the conversation, so every gate fails closed.
pub fn offer_pending(inst: &Instance, resume_available: bool, structured_ready: bool) -> bool {
    inst.queued_prompts.is_empty()
        && resume_available
        && (!inst.is_structured() || structured_ready)
        && binary_updated(inst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtime_is_compared_in_whole_seconds() {
        let started = DateTime::from_timestamp(1_000, 500_000_000).unwrap();
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        assert!(!modified_after(at(999), started));
        assert!(!modified_after(at(1_000), started));
        assert!(modified_after(at(1_001), started));
    }

    #[test]
    fn a_structured_worker_watches_its_adapter_not_the_cli() {
        let record = |agent_key: &str| {
            WorkerRecord::new(
                "t".into(),
                1,
                "/tmp/s".into(),
                agent_key.into(),
                agent_key.into(),
                "/tmp".into(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
        };
        let mut inst = Instance::new("t", "/tmp");
        assert_eq!(
            adapter_command(&inst, &record("claude")).as_deref(),
            Some("claude-agent-acp")
        );
        assert_eq!(adapter_command(&inst, &record("my-custom")), None);
        inst.command = "wrapper".into();
        assert_eq!(adapter_command(&inst, &record("claude")), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_terminal_row_is_judged_by_the_executable_its_launch_ran() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("agent");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Idle;
        inst.started_at = DateTime::from_timestamp(1_000, 0);
        // No recorded executable (older row or a launch that did not pin one): never offered,
        // whatever the observer's PATH holds.
        assert!(!binary_updated(&inst));

        inst.launch_executable = Some(bin.clone());
        assert!(binary_updated(&inst));
        // A launch after the file was written is current.
        inst.started_at = Some(Utc::now() + chrono::Duration::hours(1));
        assert!(!binary_updated(&inst));
    }

    #[cfg(unix)]
    #[test]
    fn a_sandboxed_row_is_never_flagged_by_host_binaries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("agent");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Idle;
        inst.launch_executable = Some(bin);
        inst.started_at = DateTime::from_timestamp(1_000, 0);
        assert!(binary_updated(&inst));

        inst.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "alpine".to_string(),
            container_name: "sandbox".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(!binary_updated(&inst));
    }

    #[test]
    fn an_adapter_on_both_path_and_bundle_is_not_guessed() {
        let path = || Some(std::path::PathBuf::from("/usr/bin/adapter"));
        let bundled = || Some(std::path::PathBuf::from("/data/adapters/adapter"));
        assert_eq!(pick_adapter(path(), None), path());
        assert_eq!(pick_adapter(None, bundled()), bundled());
        assert_eq!(pick_adapter(path(), bundled()), None);
        assert_eq!(pick_adapter(None, None), None);
    }

    #[test]
    fn busy_or_unresumable_rows_are_never_offered() {
        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Running;
        assert!(!offer_pending(&inst, true, true));
        inst.status = Status::Idle;
        assert!(!offer_pending(&inst, false, true));
        // No `started_at`: the row predates v041 and has never launched since.
        assert!(!offer_pending(&inst, true, true));
    }
}
