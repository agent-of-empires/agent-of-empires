//! Detects a running agent whose binary was replaced on disk after it launched.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::acp::agent_registry::AgentRegistry;
use crate::process::worker_registry::WorkerRecord;
use crate::session::{Instance, Status};

const OBSERVED_TTL: Duration = Duration::from_secs(30);

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

/// Where an executable path resolves now. Symlink metadata is ignored: re-creating a link with
/// the same target is not an update.
#[derive(Clone)]
struct Observed {
    resolved: std::path::PathBuf,
    mtime: Option<SystemTime>,
}

fn observe(path: &std::path::Path) -> Option<Observed> {
    let resolved = std::fs::canonicalize(path).ok()?;
    let mtime = std::fs::metadata(&resolved).and_then(|m| m.modified()).ok();
    Some(Observed { resolved, mtime })
}

/// Bounded by the number of distinct agent binaries.
static OBSERVED_CACHE: Mutex<Option<HashMap<String, (Instant, Option<Observed>)>>> =
    Mutex::new(None);

/// Cached by `key`; `path` runs only on a miss, outside the cache lock.
fn cached_observation(
    key: String,
    path: impl FnOnce() -> Option<std::path::PathBuf>,
) -> Option<Observed> {
    {
        let mut guard = OBSERVED_CACHE.lock().ok()?;
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some((at, observed)) = cache.get(&key) {
            if at.elapsed() < OBSERVED_TTL {
                return observed.clone();
            }
        }
    }
    let observed = path().and_then(|path| observe(&path));
    OBSERVED_CACHE
        .lock()
        .ok()?
        .get_or_insert_with(HashMap::new)
        .insert(key, (Instant::now(), observed.clone()));
    observed
}

/// `launched` is the resolved path recorded at launch, when known: a different one means the link
/// was switched to another version, which a fixed target mtime would hide.
fn executable_updated(
    observed: &Observed,
    launched: Option<&std::path::Path>,
    started_at: DateTime<Utc>,
) -> bool {
    launched.is_some_and(|launched| launched != observed.resolved)
        || observed
            .mtime
            .is_some_and(|mtime| modified_after(mtime, started_at))
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

fn adapter_observation(command: &str) -> Option<Observed> {
    cached_observation(format!("acp:{command}"), || {
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
                Some(executable_updated(
                    &adapter_observation(&command)?,
                    None,
                    started_at,
                ))
            })
    } else {
        // The executable the launch itself ran, so the observer's PATH never decides.
        inst.launch_executable
            .as_ref()
            .zip(inst.started_at)
            .and_then(|(path, started_at)| {
                let observed =
                    cached_observation(path.display().to_string(), || Some(path.clone()))?;
                let launched = inst.launch_resolved_executable.as_deref();
                // A cached view may predate the launch; confirm a flag against the live path.
                Some(
                    executable_updated(&observed, launched, started_at)
                        && observe(path)
                            .is_some_and(|live| executable_updated(&live, launched, started_at)),
                )
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
    fn a_relinked_executable_is_flagged_only_when_its_target_changed() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1");
        let v2 = dir.path().join("v2");
        let link = dir.path().join("agent");
        for version in [&v1, &v2] {
            std::fs::write(version, "#!/bin/sh\n").unwrap();
            std::fs::File::options()
                .write(true)
                .open(version)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(500))
                .unwrap();
        }
        symlink(&v1, &link).unwrap();

        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Idle;
        inst.started_at = DateTime::from_timestamp(1_000, 0);
        inst.launch_executable = Some(link.clone());
        inst.launch_resolved_executable = std::fs::canonicalize(&link).ok();
        assert!(!binary_updated(&inst));

        // Same target, new link mtime: the Claude Code installer case.
        std::fs::remove_file(&link).unwrap();
        symlink(&v1, &link).unwrap();
        let started = inst.started_at.unwrap();
        assert!(!executable_updated(
            &observe(&link).unwrap(),
            inst.launch_resolved_executable.as_deref(),
            started
        ));

        // Another version with the same old mtime: only the resolved path shows it.
        std::fs::remove_file(&link).unwrap();
        symlink(&v2, &link).unwrap();
        let switched = observe(&link).unwrap();
        assert!(executable_updated(
            &switched,
            inst.launch_resolved_executable.as_deref(),
            started
        ));
        // A row with no recorded path cannot tell, and does not guess.
        assert!(!executable_updated(&switched, None, started));

        // Rewritten in place after launch.
        std::fs::File::options()
            .write(true)
            .open(&v2)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(2_000))
            .unwrap();
        assert!(executable_updated(&observe(&link).unwrap(), None, started));

        // A dangling link is not an update.
        std::fs::remove_file(&v2).unwrap();
        assert!(observe(&link).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_right_after_a_link_switch_is_not_flagged_by_a_cached_view() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let (v1, v2, link) = (
            dir.path().join("v1"),
            dir.path().join("v2"),
            dir.path().join("agent"),
        );
        for version in [&v1, &v2] {
            std::fs::write(version, "#!/bin/sh\n").unwrap();
            std::fs::File::options()
                .write(true)
                .open(version)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(500))
                .unwrap();
        }
        symlink(&v1, &link).unwrap();
        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Idle;
        inst.started_at = DateTime::from_timestamp(1_000, 0);
        inst.launch_executable = Some(link.clone());
        inst.launch_resolved_executable = std::fs::canonicalize(&link).ok();
        assert!(!binary_updated(&inst));

        // The link moves to v2 and the session restarts on it inside the cache TTL.
        std::fs::remove_file(&link).unwrap();
        symlink(&v2, &link).unwrap();
        inst.launch_resolved_executable = std::fs::canonicalize(&link).ok();
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
            before_start_hook_env: Vec::new(),
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
