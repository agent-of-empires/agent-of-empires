//! Startup auto-recovery for AI agent sessions.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use fs2::FileExt;

use super::{Instance, StartOutcome};

/// File-system claim that the holder is the sole recovery owner for this machine.
pub struct RecoveryLock {
    _file: std::fs::File,
}

/// Try to acquire the cross-process recovery lock without blocking.
pub fn try_acquire_recovery_lock() -> Result<Option<RecoveryLock>> {
    try_acquire_recovery_lock_at(&recovery_lock_path()?)
}

/// Inner helper that takes the lock-file path directly.
fn try_acquire_recovery_lock_at(path: &Path) -> Result<Option<RecoveryLock>> {
    if let Some(parent) = path.parent() {
        // Propagate so an unwritable app dir surfaces here with the real OS error (e.g. EACCES,
        // EROFS) rather than as a confusing ENOENT from the subsequent `open()`.
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(RecoveryLock { _file: file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn recovery_lock_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join(".recovery.lock"))
}

/// Pure predicate: should this instance go through the startup recovery cascade?
pub fn is_recovery_candidate(inst: &Instance) -> bool {
    let resumable_id = inst
        .agent_session_id
        .as_deref()
        .is_some_and(super::is_valid_session_id);
    !inst.is_structured()
        && !inst.is_archived()
        && !inst.is_snoozed()
        && !inst.is_trashed()
        && inst.status != super::Status::Stopped
        && inst.agent_session_id != inst.resume_probe_failed_sid
        && inst.supports_native_resume()
        && resumable_id
}

/// Minimum `agent_session_id` length before it is trusted as a process-argv needle.
const ORPHAN_SCAN_MIN_SID_LEN: usize = 8;

/// True when aoe injects `AOE_INSTANCE_ID` into this agent's environment.
fn agent_injects_instance_id_env(inst: &Instance) -> bool {
    inst.status_agent()
        .is_some_and(|agent| agent.hook_config.is_some() || agent.sidecar_hooks.is_some())
}

/// The identity needles used to detect a live agent process for `inst`.
pub fn orphan_needles(inst: &Instance) -> (String, Option<String>, Option<String>) {
    if agent_injects_instance_id_env(inst) {
        if inst.id.is_empty() {
            return (String::new(), None, None);
        }
        let env = format!("{}={}", crate::tmux::env::AOE_INSTANCE_ID_KEY, inst.id);
        // The wrapper's own name when the launch renames the agent: the marker is injected on hook
        // presence alone, so a wrapper carries it and must be matched by the token its process
        // really shows.
        let executable = inst.launch_executable_token();
        return (env, None, executable);
    }

    let cmdline = inst
        .agent_session_id
        .as_deref()
        .filter(|sid| {
            sid.len() >= ORPHAN_SCAN_MIN_SID_LEN && super::capture::is_valid_session_id(sid)
        })
        .map(str::to_string);
    (String::new(), cmdline, None)
}

/// Batched orphan check: one process-table walk deciding, for each instance, whether a live agent
/// process belongs to it.
pub fn orphaned_agents_alive(insts: &[Instance]) -> Vec<bool> {
    if insts.is_empty() {
        return Vec::new();
    }
    let mut env = Vec::with_capacity(insts.len());
    let mut cmdline = Vec::with_capacity(insts.len());
    let mut executable = Vec::with_capacity(insts.len());
    for inst in insts {
        let (env_needle, cmdline_needle, executable_needle) = orphan_needles(inst);
        env.push(env_needle);
        cmdline.push(cmdline_needle);
        executable.push(executable_needle);
    }
    crate::process::processes_matching(&env, &cmdline, &executable)
}

/// Defense-in-depth guard against the sequential-recovery duplication in.
pub fn orphaned_agent_process_alive(inst: &Instance) -> bool {
    orphaned_agents_alive(std::slice::from_ref(inst))
        .first()
        .copied()
        .unwrap_or(false)
}

/// Env override for the recovery-attempt ledger directory, so tests isolate it
/// from real user state. Honored in all builds, mirroring `AOE_TMUX_SOCKET`.
pub const RECOVERY_ATTEMPT_DIR_ENV: &str = "AOE_RECOVERY_ATTEMPT_DIR";

/// Directory holding the per-boot recovery-attempt ledgers, or `None` if the
/// app dir cannot be resolved.
fn recovery_attempt_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(RECOVERY_ATTEMPT_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    super::get_app_dir()
        .ok()
        .map(|d| d.join("recovery_attempts"))
}

/// Path to the current boot's ledger file, or `None` if the app dir or boot id
/// is unavailable. The boot id is reduced to a filesystem-safe filename.
fn recovery_ledger_path() -> Option<PathBuf> {
    let dir = recovery_attempt_dir()?;
    let boot = crate::process::boot_id()?;
    let safe: String = boot
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(dir.join(safe))
}

/// Instance ids for which a startup-recovery attempt has already been recorded this boot.
pub fn recovery_attempted_this_boot() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Some(path) = recovery_ledger_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let id = line.trim();
                if !id.is_empty() {
                    set.insert(id.to_string());
                }
            }
        }
    }
    set
}

/// Record that a startup-recovery attempt is being made for each id in `ids`, this boot.
pub fn mark_recovery_attempted(ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let Some(path) = recovery_ledger_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        gc_stale_boot_ledgers(dir, path.file_name());
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        for id in ids {
            let _ = writeln!(file, "{id}");
        }
    }
}

/// Remove ledger files for boots other than the current one. Best-effort.
fn gc_stale_boot_ledgers(dir: &Path, keep: Option<&std::ffi::OsStr>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if Some(entry.file_name().as_os_str()) != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Warm up the tmux server so that the first concurrent `new-session` from recovery workers does
/// not race the server's cold start.
pub fn warm_tmux_server() {
    let _ = crate::tmux::tmux_command().arg("start-server").status();
}

/// Maximum number of recovery workers running concurrently.
pub const STARTUP_RECOVERY_CONCURRENCY: usize = 3;

/// Time-to-live entries in the `recently_restarted` map remain authoritative for.
pub const RECENTLY_RESTARTED_TTL: Duration = Duration::from_secs(8);

/// Periodic GC interval for `recently_restarted`.
pub const RECENTLY_RESTARTED_GC_INTERVAL: Duration = Duration::from_secs(60);

/// Shared `recently_restarted` map: instance id → time of last successful recovery start.
pub type RecentlyRestarted = Arc<std::sync::RwLock<std::collections::HashMap<String, Instant>>>;

/// Construct an empty `recently_restarted` map.
pub fn new_recently_restarted() -> RecentlyRestarted {
    Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Tick-local snapshot of the suppression set, capturing every id whose mark is currently fresh.
pub fn snapshot_recently_restarted(map: &RecentlyRestarted) -> std::collections::HashSet<String> {
    let guard = match map.read() {
        Ok(g) => g,
        Err(_) => return std::collections::HashSet::new(),
    };
    guard
        .iter()
        .filter(|(_, t)| t.elapsed() < RECENTLY_RESTARTED_TTL)
        .map(|(id, _)| id.clone())
        .collect()
}

pub fn mark_recently_restarted(map: &RecentlyRestarted, id: &str) {
    if let Ok(mut guard) = map.write() {
        guard.insert(id.to_string(), Instant::now());
    }
}

/// Inverse of `mark_recently_restarted`.
pub fn unmark_recently_restarted(map: &RecentlyRestarted, id: &str) {
    if let Ok(mut guard) = map.write() {
        guard.remove(id);
    }
}

/// Remove entries older than `2 × RECENTLY_RESTARTED_TTL`.
pub fn gc_recently_restarted(map: &RecentlyRestarted) {
    let cutoff = RECENTLY_RESTARTED_TTL * 2;
    if let Ok(mut guard) = map.write() {
        guard.retain(|_, t| t.elapsed() < cutoff);
    }
}

/// Set of instance ids whose startup-recovery cascade has been scheduled but not yet completed.
pub type RecoveryPending = Arc<std::sync::RwLock<std::collections::HashSet<String>>>;

/// Construct an empty `recovery_pending` set.
pub fn new_recovery_pending() -> RecoveryPending {
    Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()))
}

/// Seed the pending set with every scheduled candidate id.
pub fn seed_recovery_pending(pending: &RecoveryPending, ids: impl IntoIterator<Item = String>) {
    if let Ok(mut guard) = pending.write() {
        guard.extend(ids);
    }
}

/// One refresher tick: re-stamp every still-pending id in `recently_restarted`.
pub fn refresh_recovery_pending(
    pending: &RecoveryPending,
    recently_restarted: &RecentlyRestarted,
) -> bool {
    let guard = match pending.read() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if guard.is_empty() {
        return false;
    }
    for id in guard.iter() {
        mark_recently_restarted(recently_restarted, id);
    }
    true
}

#[cfg(test)]
thread_local! {
    static DRAIN_CONTENTION_OBSERVER: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Worker-completion drain: remove `id` from the pending set so the refresher stops re-stamping it,
/// *then* clear its suppression mark.
pub fn drain_recovery_pending(
    pending: &RecoveryPending,
    recently_restarted: &RecentlyRestarted,
    id: &str,
) {
    #[cfg(test)]
    let lock = crate::session::test_support::write_reporting_contention(pending, || {
        DRAIN_CONTENTION_OBSERVER.with(|slot| {
            if let Some(sender) = slot.borrow_mut().take() {
                let _ = sender.send(());
            }
        })
    });
    #[cfg(not(test))]
    let lock = pending.write();
    if let Ok(mut guard) = lock {
        guard.remove(id);
    }
    unmark_recently_restarted(recently_restarted, id);
}

/// Run the recovery cascade for one instance.
pub fn run_recovery_for_instance(inst: &mut Instance) -> Result<StartOutcome> {
    let _scope = HookTimeoutScope::new(recovery_hook_timeout());
    let result = inst.restart_with_size_opts(None, false);
    if let Err(ref e) = result {
        stamp_recovery_error(inst, e);
    }
    result
}

fn stamp_recovery_error(inst: &mut Instance, e: &anyhow::Error) {
    inst.status = super::Status::Error;
    inst.last_error = Some(format_recovery_last_error(e));
    inst.last_error_check = Some(std::time::Instant::now());
}

/// Project a cascade `anyhow::Error` onto the operator-facing `last_error` string.
fn format_recovery_last_error(e: &anyhow::Error) -> String {
    if let Some(t) = e
        .chain()
        .find_map(|c| c.downcast_ref::<super::config::repo_config::HookTimeout>())
    {
        format!(
            "on_launch hook timed out after {}s: {}",
            t.timeout_secs, t.cmd
        )
    } else {
        format!("recovery cascade: {}", e)
    }
}

/// 30 s default; the operational guidance for non-interactive on_launch hooks.
pub const RECOVERY_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Lower bound on `AOE_RECOVERY_HOOK_TIMEOUT_MS` so a misconfigured test
/// cannot race fork+exec and trip the timeout before the child spawns.
#[cfg(debug_assertions)]
const RECOVERY_HOOK_TIMEOUT_FLOOR: Duration = Duration::from_millis(50);

/// Resolve the recovery hook timeout.
pub fn recovery_hook_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_RECOVERY_HOOK_TIMEOUT_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return Duration::from_millis(ms).max(RECOVERY_HOOK_TIMEOUT_FLOOR);
        }
    }
    RECOVERY_HOOK_TIMEOUT
}

thread_local! {
    static HOOK_TIMEOUT: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// The current thread's on_launch hook deadline, if a scope is active.
pub(crate) fn current_hook_timeout() -> Option<Duration> {
    HOOK_TIMEOUT.with(|c| c.get())
}

/// RAII guard for the per-thread on_launch hook deadline. Restores the
/// previous value on drop, so nested scopes behave LIFO.
// Save/restore covers LIFO nesting only; production installs at most one scope per thread
// (recovery, or a bounded `perform_restart`), so out-of-order drops never occur.
pub struct HookTimeoutScope {
    previous: Option<Duration>,
}

impl HookTimeoutScope {
    pub fn new(timeout: Duration) -> Self {
        let previous = HOOK_TIMEOUT.with(|c| c.replace(Some(timeout)));
        Self { previous }
    }
}

impl Drop for HookTimeoutScope {
    fn drop(&mut self) {
        HOOK_TIMEOUT.with(|c| c.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recently_restarted_snapshot_and_gc_drop_expired_marks() {
        let map = new_recently_restarted();
        map.write()
            .unwrap()
            .insert("stale".into(), Instant::now() - RECENTLY_RESTARTED_TTL * 2);
        mark_recently_restarted(&map, "fresh");
        let snap = snapshot_recently_restarted(&map);
        assert!(snap.contains("fresh"));
        assert!(!snap.contains("stale") && !snap.contains("other"));
        assert!(
            map.read().unwrap().contains_key("stale"),
            "snapshot is read-only"
        );
        gc_recently_restarted(&map);
        let g = map.read().unwrap();
        assert!(!g.contains_key("stale"));
        assert!(g.contains_key("fresh"));
    }

    // Regression for the queued-candidate TTL race: the background refresher must not resurrect a
    // mark that a completed worker has just cleared.
    #[test]
    fn refresher_does_not_resurrect_drained_worker_mark() {
        let recently = new_recently_restarted();
        let pending = new_recovery_pending();

        seed_recovery_pending(&pending, ["abc".to_string()]);
        mark_recently_restarted(&recently, "abc");

        assert!(
            refresh_recovery_pending(&pending, &recently),
            "non-empty pending set should keep ticking",
        );
        assert!(
            recently.read().unwrap().contains_key("abc"),
            "refresher must keep a queued candidate's mark fresh",
        );

        drain_recovery_pending(&pending, &recently, "abc");
        assert!(
            !recently.read().unwrap().contains_key("abc"),
            "drain must clear the suppression mark",
        );

        assert!(
            !refresh_recovery_pending(&pending, &recently),
            "empty pending set signals the refresher to stop",
        );
        assert!(
            !recently.read().unwrap().contains_key("abc"),
            "refresher must not resurrect a drained worker's mark",
        );
    }

    #[test]
    fn refresher_keeps_remaining_candidates_after_partial_drain() {
        let recently = new_recently_restarted();
        let pending = new_recovery_pending();
        seed_recovery_pending(&pending, ["done".to_string(), "queued".to_string()]);

        drain_recovery_pending(&pending, &recently, "done");

        assert!(
            refresh_recovery_pending(&pending, &recently),
            "the queued candidate keeps the refresher alive",
        );
        assert!(
            recently.read().unwrap().contains_key("queued"),
            "still-queued candidate must stay suppressed",
        );
        assert!(
            !recently.read().unwrap().contains_key("done"),
            "drained candidate must not be re-stamped",
        );
    }

    #[test]
    fn refresher_mark_loses_to_concurrent_drain_under_lock_overlap() {
        let recently = new_recently_restarted();
        let pending = new_recovery_pending();
        seed_recovery_pending(&pending, ["x".to_string()]);
        mark_recently_restarted(&recently, "x");
        let read_guard = pending.read().unwrap();
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let drain_pending = pending.clone();
        let drain_recently = recently.clone();
        let drainer = std::thread::spawn(move || {
            DRAIN_CONTENTION_OBSERVER.with(|slot| *slot.borrow_mut() = Some(contended_tx));
            drain_recovery_pending(&drain_pending, &drain_recently, "x");
        });
        let contended = contended_rx.recv_timeout(Duration::from_secs(2));
        mark_recently_restarted(&recently, "x");
        drop(read_guard);
        drainer.join().unwrap();
        assert!(
            contended.is_ok(),
            "drain must reach the contested write-lock boundary before stamping"
        );
        assert!(!pending.read().unwrap().contains("x"));
        assert!(
            !recently.read().unwrap().contains_key("x"),
            "the drain's unmark must win over the refresher's last mark"
        );
    }

    /// Parked sessions (archive, stop, live snooze), an ambiguously failed resume sid, and a
    /// wrapper without native resume identity (#3678) are never startup-recovery candidates;
    /// clearing the state restores eligibility. Archive and stop both kill the pane, so a
    /// dead pane alone must not trigger recovery.
    #[test]
    fn recovery_candidacy_follows_parked_state_and_resume_identity() {
        let sid = "11111111-1111-4111-8111-111111111111";
        type Set = fn(&mut Instance);
        let cases: [(&str, Set, Set); 5] = [
            ("archived", |i| i.archive(), |i| i.unarchive()),
            (
                "stopped",
                |i| i.status = super::super::Status::Stopped,
                |i| i.status = super::super::Status::Starting,
            ),
            (
                "snoozed",
                |i| i.snooze(30),
                |i| i.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
            ),
            (
                "probe-failed",
                |i| i.resume_probe_failed_sid = i.agent_session_id.clone(),
                |i| i.resume_probe_failed_sid = None,
            ),
            (
                "wrapper",
                |i| i.command = "/opt/wrappers/claude".to_string(),
                |i| i.command = "claude --model opus".to_string(),
            ),
        ];
        let mut failures = Vec::new();
        for (case, park, clear) in cases {
            let mut inst = Instance::new(case, "/tmp/test");
            inst.tool = "custom-agent".to_string();
            inst.detect_as = "claude".to_string();
            inst.command = "claude --model opus".to_string();
            inst.agent_session_id = Some(sid.into());
            let baseline = is_recovery_candidate(&inst);
            park(&mut inst);
            let parked = is_recovery_candidate(&inst);
            clear(&mut inst);
            let observed = (baseline, parked, is_recovery_candidate(&inst));
            if observed != (true, false, true) {
                failures.push(format!(
                    "{case}: (baseline, parked, cleared) = {observed:?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn orphaned_agent_process_alive_is_false_without_a_matching_live_process() {
        let pid = std::process::id();
        let mut inst = Instance::new("absent", "/tmp/test");
        inst.id = format!("absent{pid:012}");
        inst.tool = "opencode".to_string();
        // (case, agent session id)
        for (case, sid) in [
            ("no sid at all", None),
            (
                "a sid no process carries",
                Some(format!("11111111-1111-4111-8111-{pid:012}")),
            ),
            // Too short to trust as a cmdline needle, and nothing carries the env id either.
            (
                "a sub-ORPHAN_SCAN_MIN_SID_LEN sid",
                Some("short".to_string()),
            ),
        ] {
            inst.agent_session_id = sid;
            assert!(!orphaned_agent_process_alive(&inst), "{case}");
        }
    }

    /// A live agent is detected by its sid in argv (#2994), or for a hook agent by the instance
    /// marker plus its executable rather than the captured sid (#3678).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn orphaned_agent_process_alive_detects_a_live_agent() {
        let bin = tempfile::tempdir().unwrap();
        let agent = bin.path().join("claude");
        std::fs::write(&agent, "#!/bin/sh\nsleep 10\n").unwrap();
        // (tool, sid, found by the sid in argv rather than marker plus executable)
        for (tool, sid, by_sid) in [
            (
                "opencode",
                format!("22222222-2222-4222-8222-{:012}", std::process::id()),
                true,
            ),
            (
                "claude",
                "66666666-7777-4888-8999-000000000000".to_string(),
                false,
            ),
        ] {
            // Reading another process's environment needs /proc.
            if !by_sid && !cfg!(target_os = "linux") {
                continue;
            }
            let mut inst = Instance::new("orphan", "/tmp/test");
            inst.id = format!("orphan{tool}{:012}", std::process::id());
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.clone());
            let mut command = std::process::Command::new("/bin/sh");
            if by_sid {
                command.arg("-c").arg("sleep 30; true").arg(&sid);
            } else {
                command
                    .arg(&agent)
                    .env(crate::tmux::env::AOE_INSTANCE_ID_KEY, &inst.id);
            }
            let mut child = command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn orphan-agent stand-in");

            let mut detected = false;
            for _ in 0..100 {
                if orphaned_agent_process_alive(&inst) {
                    detected = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }

            let _ = child.kill();
            let _ = child.wait();
            assert!(
                detected,
                "{tool}: a live agent must be detected as an orphan"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphaned_agent_process_ignores_env_only_descendant() {
        let mut inst = Instance::new("orphan-env-descendant", "/tmp/test");
        inst.id = format!("orphanenv{:012}", std::process::id());
        inst.agent_session_id = Some("11111111-2222-4333-8444-555555555555".to_string());
        let marker = format!("{}={}", crate::tmux::env::AOE_INSTANCE_ID_KEY, inst.id);

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env(crate::tmux::env::AOE_INSTANCE_ID_KEY, &inst.id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn env-only descendant stand-in");

        let mut visible = false;
        for _ in 0..100 {
            if crate::process::processes_matching(std::slice::from_ref(&marker), &[None], &[None])
                [0]
            {
                visible = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            visible,
            "the descendant must be visible before testing the guard"
        );
        assert!(
            !orphaned_agent_process_alive(&inst),
            "an env-only descendant must not suppress recovery"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn wrapper_hook_agent_keeps_the_env_marker_and_never_matches_on_sid() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "orphan-wrapper-needles";
        let _registry = crate::session::instance::test_helpers::install_aliases(
            PROFILE,
            &[("claude-personal", "claude")],
        );
        crate::session::instance::test_helpers::declare_execution_aliases(
            PROFILE,
            &[("claude-personal", "claude")],
            home.path(),
        );
        let mut inst = Instance::new("wrapper", "/tmp/orphan-wrapper");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-personal".to_string();
        inst.command = "claude-personal".to_string();
        inst.id = "orphanwrapper01".to_string();
        inst.agent_session_id = Some("11111111-2222-4333-8444-555555555555".to_string());

        let (env, cmdline, executable) = orphan_needles(&inst);
        assert_eq!(
            env,
            format!("{}={}", crate::tmux::env::AOE_INSTANCE_ID_KEY, inst.id),
            "a wrapper carries the marker and must be matched on it"
        );
        assert_eq!(
            cmdline, None,
            "a fork child carries the parent's sid, so it must never be a needle here"
        );
        assert_eq!(
            executable.as_deref(),
            Some("claude-personal"),
            "the needle must be the token the wrapper's process really shows"
        );
    }

    #[test]
    #[serial_test::serial]
    fn recovery_attempt_ledger_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let _env =
            crate::session::test_support::EnvGuard::set(&[(RECOVERY_ATTEMPT_DIR_ENV, dir.path())]);
        if crate::process::boot_id().is_none() {
            return; // ledger disabled on this host; nothing to assert
        }

        let id = format!("ledger{:012}", std::process::id());
        let other = format!("other{:012}", std::process::id());
        assert!(
            !recovery_attempted_this_boot().contains(&id),
            "fresh ledger must not report an unmarked id",
        );

        mark_recovery_attempted(std::slice::from_ref(&id));
        let attempted = recovery_attempted_this_boot();
        assert!(
            attempted.contains(&id),
            "a marked id must be reported attempted"
        );
        assert!(
            !attempted.contains(&other),
            "an unmarked id must not appear"
        );
    }

    #[test]
    fn recovery_lock_acquires_and_releases() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join(".recovery.lock");

        let first = try_acquire_recovery_lock_at(&path).unwrap();
        assert!(first.is_some(), "acquisition should succeed");
        assert!(try_acquire_recovery_lock_at(&path).unwrap().is_none());
        drop(first);

        assert!(
            try_acquire_recovery_lock_at(&path).unwrap().is_some(),
            "re-acquisition after drop should succeed"
        );
    }

    fn hook_timeout(cmd: &str, timeout_secs: u64) -> anyhow::Error {
        anyhow::Error::new(super::super::config::repo_config::HookTimeout {
            cmd: cmd.to_string(),
            timeout_secs,
        })
    }

    #[test]
    fn recovery_error_classifies_a_hook_timeout_anywhere_in_the_chain_and_stamps_it() {
        assert_eq!(
            format_recovery_last_error(&hook_timeout("sleep 60", 30)),
            "on_launch hook timed out after 30s: sleep 60",
        );
        assert_eq!(
            format_recovery_last_error(
                &hook_timeout("echo hi && sleep 60", 12).context("recovery cascade tier 1")
            ),
            "on_launch hook timed out after 12s: echo hi && sleep 60",
            "a later `.context(..)` wrap must not hide the timeout",
        );
        assert_eq!(
            format_recovery_last_error(&anyhow::anyhow!("tmux session is gone")),
            "recovery cascade: tmux session is gone",
        );

        let mut inst = Instance::new("timeout", "/tmp/test");
        let before = std::time::Instant::now();
        stamp_recovery_error(&mut inst, &hook_timeout("sleep 60", 30));
        assert_eq!(inst.status, super::super::Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some("on_launch hook timed out after 30s: sleep 60"),
        );
        assert!(
            inst.last_error_check
                .is_some_and(|checked| checked >= before),
            "last_error_check must arm sticky error handling",
        );
    }
}
