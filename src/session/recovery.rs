//! Startup auto-recovery for AI agent sessions.
//!
//! After a system reboot, tmux loses all its sessions. AoE sessions whose
//! agent supports `--resume <sid>` (claude, opencode, codex, gemini, vibe,
//! pi, hermes, kiro, qwen, copilot) can be transparently recreated by replaying
//! the resume cascade in `start_with_resume_fallback`. This module centralises
//! the candidate selection and the cross-process exclusion needed to make
//! that safe when both the TUI (`aoe`) and the daemon (`aoe serve`) are
//! running.
//!
//! The recovery cascade itself lives in `instance::start_with_resume_fallback`;
//! this module is the policy layer (who runs it, when, with what serialization)
//! that the TUI and daemon entry points share.
//!
//! # Cross-process exclusion
//!
//! Both the TUI and the daemon may attempt recovery on startup. To avoid
//! duplicate cascades against the same `(profile, id)` (which would race on
//! `tmux new-session` and on `sessions.json`), we acquire a non-blocking
//! exclusive `flock` on a marker file in the app data directory. The losing
//! party skips recovery entirely and lets the winner proceed. The file lock
//! is held for the entire recovery pass so that:
//!
//! - A late-starting daemon cannot duplicate a TUI's in-flight workers.
//! - A late-starting TUI cannot duplicate a daemon's in-flight workers.
//!
//! `daemon_pid()` alone is not sufficient because the daemon writes its PID
//! file *after* fork+exec, leaving a tens-to-hundreds-of-millisecond window
//! where both sides observe "no daemon running" and both decide they own
//! recovery.
//!
//! # Bounded on_launch hook execution
//!
//! Recovery installs a [`HookTimeoutScope`] before entering the cascade.
//! `repo_config::run_hooks_captured` reads the scope and bounds each
//! `on_launch` command by [`RECOVERY_HOOK_TIMEOUT`] (30 s, debug-overridable
//! via `AOE_RECOVERY_HOOK_TIMEOUT_MS`); on expiry the child tree is killed
//! through [`crate::process::kill_process_tree`] (SIGTERM, 100 ms grace,
//! then SIGKILL). An `N`-command list releases the lock within
//! `N * (RECOVERY_HOOK_TIMEOUT + kill_grace)` per worker, with up to
//! [`STARTUP_RECOVERY_CONCURRENCY`] workers concurrent.
//!
//! Caveats: `execute_hooks_in_container` kills the host-side
//! `docker`/`podman exec` child, not the in-container process; signal
//! propagation depends on the runtime. Hooks that daemonize (own `setsid`
//! plus reparent to PID 1) escape `kill_process_tree`'s descendant walk;
//! the lock still releases when the direct child exits, but the orphan is
//! the operator's to reap.

use std::cell::Cell;
use std::path::{Path, PathBuf};
#[cfg(feature = "serve")]
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "serve")]
use std::time::Instant;

use anyhow::Result;
use fs2::FileExt;

use super::instance::should_attempt_resume;
use super::{Instance, StartOutcome};

/// File-system claim that the holder is the sole recovery owner for this
/// machine. Dropped automatically (releases the `flock`) when the holder goes
/// out of scope.
pub struct RecoveryLock {
    _file: std::fs::File,
}

/// Try to acquire the cross-process recovery lock without blocking.
///
/// Returns `Some(RecoveryLock)` if this process is now the recovery owner;
/// `None` if another process (TUI or daemon) already holds it. The lock is
/// released when the returned guard is dropped.
///
/// The lock file lives at `<app_dir>/.recovery.lock`. It is created if
/// missing and never deleted (the lock is on the file, not its existence).
pub fn try_acquire_recovery_lock() -> Result<Option<RecoveryLock>> {
    try_acquire_recovery_lock_at(&recovery_lock_path()?)
}

/// Inner helper that takes the lock-file path directly. Split out so tests
/// can exercise the flock logic without depending on the env-var-driven
/// `get_app_dir()` resolution, which races with non-`#[serial]` readers of
/// `HOME` / `XDG_CONFIG_HOME` elsewhere in the suite.
fn try_acquire_recovery_lock_at(path: &Path) -> Result<Option<RecoveryLock>> {
    if let Some(parent) = path.parent() {
        // Propagate so an unwritable app dir surfaces here with the real
        // OS error (e.g. EACCES, EROFS) rather than as a confusing
        // ENOENT from the subsequent `open()`.
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

/// Pure predicate: should this instance go through the startup recovery
/// cascade? Excludes structured view-mode sessions (handled by `acp_reconciler`),
/// sessions whose agent has `ResumeStrategy::Unsupported`, sessions without
/// a valid `agent_session_id`, and sunk rows (archived, currently snoozed, or
/// explicitly stopped). Live tmux panes are filtered separately by the caller
/// using `Instance::has_live_tmux_pane()`.
///
/// Archive, snooze, and stop are explicit "leave this session alone" signals;
/// each of them kills the tmux pane, so without this guard the next TUI
/// launch (or daemon startup) would observe a dead pane on a resumable agent
/// and respawn the row the user just dismissed. Snooze flips back to
/// recoverable on its own schedule (`is_snoozed()` returns false once the
/// timer expires). Stop only flips back to recoverable when the user
/// explicitly reopens the session (Enter / send-message / live-send), which
/// transitions `Status::Stopped` to `Status::Starting` before recovery is
/// consulted.
pub fn is_recovery_candidate(inst: &Instance) -> bool {
    !inst.is_structured()
        && !inst.is_archived()
        && !inst.is_snoozed()
        && !inst.is_trashed()
        && inst.status != super::Status::Stopped
        && inst.agent_session_id != inst.resume_probe_failed_sid
        && should_attempt_resume(inst.agent_session_id.as_deref(), &inst.tool)
}

/// Minimum `agent_session_id` length before it is trusted as a process-argv
/// needle. This fallback signal is used only for non-hook agents (hook agents
/// match on the anchored `AOE_INSTANCE_ID` env entry instead). Real sids are
/// long (16 hex for claude, 36 for a UUID); refusing shorter ids keeps a
/// pathological 1-3 char id from matching an unrelated command line.
const ORPHAN_SCAN_MIN_SID_LEN: usize = 8;

/// True when aoe injects `AOE_INSTANCE_ID` into this agent's environment. Gated
/// on the same hook presence as `status_hook_env_prefix`, so it tracks exactly
/// the agents whose live process carries the anchored env marker.
fn agent_injects_instance_id_env(tool: &str) -> bool {
    crate::agents::get_agent(tool)
        .is_some_and(|a| a.hook_config.is_some() || a.sidecar_hooks.is_some())
}

/// The identity needles used to detect a live agent process for `inst`: the
/// anchored `AOE_INSTANCE_ID=<id>` env entry, and (only for non-hook agents)
/// the `agent_session_id` as a command-line needle.
///
/// Hook agents deliberately do *not* use the sid needle: a live
/// `claude --resume <parent_sid> --fork-session` child carries the *parent's*
/// sid in its argv, so a bare-sid match could let a fork-child suppress the
/// parent's recovery (#3006 review). The env marker names the exact instance,
/// so it has no such collision.
pub fn orphan_needles(inst: &Instance) -> (String, Option<String>) {
    let env = if inst.id.is_empty() {
        String::new()
    } else {
        format!("{}={}", crate::tmux::env::AOE_INSTANCE_ID_KEY, inst.id)
    };
    let cmdline = if agent_injects_instance_id_env(&inst.tool) {
        None
    } else {
        inst.agent_session_id
            .as_deref()
            .filter(|sid| {
                sid.len() >= ORPHAN_SCAN_MIN_SID_LEN && super::capture::is_valid_session_id(sid)
            })
            .map(str::to_string)
    };
    (env, cmdline)
}

/// Batched orphan check: one process-table walk deciding, for each instance,
/// whether a live agent process belongs to it. See [`orphaned_agent_process_alive`]
/// for the rationale; this is the form the recovery paths use so N candidates
/// cost one `/proc` walk (or one `ps` fork), and the callers wrap it in
/// `tokio::task::spawn_blocking`.
pub fn orphaned_agents_alive(insts: &[Instance]) -> Vec<bool> {
    if insts.is_empty() {
        return Vec::new();
    }
    let (env, cmdline): (Vec<String>, Vec<Option<String>>) =
        insts.iter().map(orphan_needles).unzip();
    crate::process::processes_matching(&env, &cmdline)
}

/// Defense-in-depth guard against the sequential-recovery duplication in #2994.
///
/// A prior recovery pass may have resumed this instance on a tmux server that
/// this process can no longer see: on a mid-crash `/tmp` wipe (WSL2) the
/// server's socket file is unlinked, orphaning the still-running server, and a
/// later pass resolving a fresh default socket observes the session as
/// "missing" (`has_live_tmux_pane()` is false) and would recreate it,
/// orphaning the first batch's agent processes. After a socket loss the OS
/// process table is the *only* source of truth that survives (the socket file,
/// and therefore any `tmux` query, is gone), and it is host-local, so unlike a
/// persisted PID it is safe under a `sessions.json` synced across hosts.
///
/// A match means the agent is alive on a tmux server we cannot see, so recovery
/// skips it rather than duplicate it. Returns `false` (allow recovery) when no
/// live process matches, including the genuine post-reboot case where the agent
/// processes are gone. This positively identifies live orphans; the boot-scoped
/// [`mark_recovery_attempted`] ledger is the deterministic backstop that also
/// covers agents with no usable identity needle.
///
/// Callers on an async runtime must invoke this inside `spawn_blocking`: it
/// walks the process table and would otherwise stall the executor.
///
/// The TUI path scans in a batch via [`orphaned_agents_alive`], so this
/// single-instance convenience is compiled only where it is actually used: the
/// serve-gated daemon Phase B re-check and the unit tests.
#[cfg(any(feature = "serve", test))]
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

/// Instance ids for which a startup-recovery attempt has already been recorded
/// this boot. Recovery filters these out so a session is attempted at most once
/// per boot, which deterministically prevents the #2994 sequential duplication
/// for *every* agent (including those with no usable process-scan needle).
/// Empty when the ledger is unavailable or unreadable (fail open to the scan).
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

/// Record that a startup-recovery attempt is being made for each id in `ids`,
/// this boot. Called *before* the cascade creates any tmux session, so a
/// mid-pass crash fails toward "already attempted, do not duplicate" rather
/// than "no record, recreate". Best-effort (an unwritable app dir is ignored);
/// also GCs ledgers from prior boots so the directory stays bounded.
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

/// Warm up the tmux server so that the first concurrent `new-session` from
/// recovery workers does not race the server's cold start. On macOS post-reboot,
/// tmux is not running until the first client connects; without this warm-up,
/// three workers calling `new-session` simultaneously can hit a connect-race
/// window where the socket file exists but no listener accepts yet.
///
/// Best-effort: `tmux start-server` is idempotent; if tmux is unavailable the
/// caller will fail downstream with a more specific error.
pub fn warm_tmux_server() {
    let _ = crate::tmux::tmux_command().arg("start-server").status();
}

/// Maximum number of recovery workers running concurrently. Sized to cover
/// the typical case (a handful of resume-capable sessions surviving a
/// daemon restart) without thundering-herd-ing tmux at server warm-up.
/// Shared between the TUI standalone path and the daemon path so both sides
/// behave identically when run separately. Users with more than this many
/// simultaneously-missing sessions will see the 4th+ candidate enter its
/// cascade after `RECENTLY_RESTARTED_TTL` has expired for it, producing a
/// brief `Starting -> Error` blip before completion; raising both this
/// constant and the TTL together is the right knob if telemetry warrants.
pub const STARTUP_RECOVERY_CONCURRENCY: usize = 3;

/// Time-to-live entries in the `recently_restarted` map remain authoritative
/// for. Sized to cover the typical worst-case cascade latency
/// (`RESUME_PROBE_MAX` ~3s × 2 tiers + kill_clean grace ~150ms ≈ 6.15s) plus
/// a ~1.85s margin for slow cold-start agents (opencode importing on a cold
/// cache). Lower values cause spurious `Status::Error` chips on still-starting
/// sessions; higher values delay the first real status update past the user's
/// patience window.
///
/// The absolute worst case (both tiers running the full
/// `RESUME_PROBE_POST_SHELL_GRACE` of 2s on top of `RESUME_PROBE_MAX`) would
/// reach ~10s and exceed this TTL. In practice the cascade aborts early on a
/// confirmed-Dead pane, so the typical bound holds; if production telemetry
/// shows the absolute case occurring, raise this to 11s rather than relying
/// on early abort.
#[cfg(feature = "serve")]
pub const RECENTLY_RESTARTED_TTL: Duration = Duration::from_secs(8);

/// Periodic GC interval for `recently_restarted`. Long-running daemons may
/// accumulate thousands of entries over a session if they never GC; the TTL
/// check on read filters but does not remove. Sweeping every 60s keeps the
/// map bounded by `O(recoveries_in_last_60s)` rather than total uptime.
#[cfg(feature = "serve")]
pub const RECENTLY_RESTARTED_GC_INTERVAL: Duration = Duration::from_secs(60);

/// Shared `recently_restarted` map: instance id → time of last successful
/// recovery start. Status pollers consult this to suppress the
/// `Status::Error` transition while a freshly-restarted agent is still
/// settling. Entries older than `RECENTLY_RESTARTED_TTL` are ignored on read
/// and removed by the GC task.
#[cfg(feature = "serve")]
pub type RecentlyRestarted = Arc<std::sync::RwLock<std::collections::HashMap<String, Instant>>>;

/// Construct an empty `recently_restarted` map.
#[cfg(feature = "serve")]
pub fn new_recently_restarted() -> RecentlyRestarted {
    Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Tick-local snapshot of the suppression set, capturing every id whose
/// mark is currently fresh. `status_poll_loop` takes this snapshot once
/// per tick *before* `batch_pane_metadata()` runs, then uses it for the
/// `Status::Starting` decision so that a worker which unmarks mid-tick
/// (after the pane scrape, before the decision) cannot combine stale
/// pane-missing metadata with a cleared mark and re-emit the phantom
/// `Status::Error` the suppression is there to prevent.
#[cfg(feature = "serve")]
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

#[cfg(feature = "serve")]
pub fn mark_recently_restarted(map: &RecentlyRestarted, id: &str) {
    if let Ok(mut guard) = map.write() {
        guard.insert(id.to_string(), Instant::now());
    }
}

/// Inverse of `mark_recently_restarted`. Called when a pre-marked
/// candidate turns out not to need recovery (post-lock re-check fails),
/// to avoid suppressing the real status for the full TTL.
#[cfg(feature = "serve")]
pub fn unmark_recently_restarted(map: &RecentlyRestarted, id: &str) {
    if let Ok(mut guard) = map.write() {
        guard.remove(id);
    }
}

/// Remove entries older than `2 × RECENTLY_RESTARTED_TTL`. The 2x factor
/// avoids a tight read-vs-GC race where a reader observes an entry just
/// before GC removes it; with 2x, a reader that saw the entry at age T has
/// at least T more time before GC reaps it.
#[cfg(feature = "serve")]
pub fn gc_recently_restarted(map: &RecentlyRestarted) {
    let cutoff = RECENTLY_RESTARTED_TTL * 2;
    if let Ok(mut guard) = map.write() {
        guard.retain(|_, t| t.elapsed() < cutoff);
    }
}

/// Set of instance ids whose startup-recovery cascade has been scheduled
/// but not yet completed. Populated by Phase A (`daemon_startup_recovery_mark`)
/// for every candidate; each Phase B worker drains its own id when its
/// cascade terminates (success, skip, error, or panic). The background
/// refresher walks this set every `RECENTLY_RESTARTED_TTL / 2` and re-stamps
/// each member in `recently_restarted`, so a candidate that sits in the
/// `STARTUP_RECOVERY_CONCURRENCY` semaphore queue past the TTL does not age
/// out of suppression and trip a phantom `Status::Error` before its worker
/// even begins.
#[cfg(feature = "serve")]
pub type RecoveryPending = Arc<std::sync::RwLock<std::collections::HashSet<String>>>;

/// Construct an empty `recovery_pending` set.
#[cfg(feature = "serve")]
pub fn new_recovery_pending() -> RecoveryPending {
    Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()))
}

/// Seed the pending set with every scheduled candidate id. Called by Phase A
/// alongside the initial `mark_recently_restarted` so the refresher has the
/// full work set before the cascade (and the refresher) start.
#[cfg(feature = "serve")]
pub fn seed_recovery_pending(pending: &RecoveryPending, ids: impl IntoIterator<Item = String>) {
    if let Ok(mut guard) = pending.write() {
        guard.extend(ids);
    }
}

/// One refresher tick: re-stamp every still-pending id in `recently_restarted`.
/// Returns `false` once the pending set is empty so the caller can stop
/// ticking (the cascade is done).
///
/// Lock order is `R(pending)` → `W(recently_restarted)`, with the marking
/// performed *inside* the `pending` read-lock scope. That is the load-bearing
/// detail: a concurrent [`drain_recovery_pending`] takes `W(pending)` first,
/// so it cannot interleave between this function observing an id and stamping
/// it. Either the drain wins the write lock before this read (the id is gone,
/// never re-stamped) or it blocks until this read releases (its later unmark
/// strictly succeeds this stamp). No mark-after-unmark resurrection is
/// possible. See [`drain_recovery_pending`].
#[cfg(feature = "serve")]
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

/// Worker-completion drain: remove `id` from the pending set so the refresher
/// stops re-stamping it, *then* clear its suppression mark. The ordering
/// (`W(pending)` before unmarking `recently_restarted`) is what makes the
/// unmark stick against a racing refresher; see [`refresh_recovery_pending`].
#[cfg(feature = "serve")]
pub fn drain_recovery_pending(
    pending: &RecoveryPending,
    recently_restarted: &RecentlyRestarted,
    id: &str,
) {
    if let Ok(mut guard) = pending.write() {
        guard.remove(id);
    }
    unmark_recently_restarted(recently_restarted, id);
}

/// Run the recovery cascade for one instance. Wraps
/// `restart_with_size_opts(None, false)` in a [`HookTimeoutScope`] so a
/// hung `on_launch` hook cannot pin the recovery lock (#1265).
///
/// `skip_on_launch=false` is mandatory: hooks must run on the first start
/// after a reboot. Ambiguous resume-probe failures return `ResumeFailed`
/// instead of launching fresh, so hooks are not double-fired on that path.
///
/// On failure, stamps `Status::Error`, `last_error`, and `last_error_check`
/// on the instance before propagating the error so daemon and TUI workers
/// share the same translation. The `last_error` for hook-timeout failures
/// is `"on_launch hook timed out after Ns: <cmd>"` (#1889); generic cascade
/// failures keep the historical `"recovery cascade: <e>"` shape.
///
/// Blocks; callers must invoke it off the main event-loop thread. Worst
/// case is `N_hooks * RECOVERY_HOOK_TIMEOUT + ~4 s` resume-probe latency.
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

/// Project a cascade `anyhow::Error` onto the operator-facing `last_error`
/// string. A `HookTimeout` carried in the error chain produces the exact
/// `"on_launch hook timed out after Ns: <cmd>"` shape called out by #1889;
/// every other error keeps the daemon's historical `"recovery cascade: <e>"`
/// wrapping so non-timeout failures stay unambiguously cascade-attributed.
///
/// Walks the full chain (not just the root) so that a future `.context(...)`
/// wrap somewhere in the cascade does not silently regress the timeout
/// classification. Replacing `.context()` with `anyhow!("...: {e}")` would
/// detach the source and is the only way to defeat this; the `HookTimeout`
/// type's docstring calls that pattern out.
fn format_recovery_last_error(e: &anyhow::Error) -> String {
    if let Some(t) = e
        .chain()
        .find_map(|c| c.downcast_ref::<super::repo_config::HookTimeout>())
    {
        format!(
            "on_launch hook timed out after {}s: {}",
            t.timeout_secs, t.cmd
        )
    } else {
        format!("recovery cascade: {}", e)
    }
}

/// 30 s default; the operational guidance for non-interactive on_launch
/// hooks (#1265).
pub const RECOVERY_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Lower bound on `AOE_RECOVERY_HOOK_TIMEOUT_MS` so a misconfigured test
/// cannot race fork+exec and trip the timeout before the child spawns.
#[cfg(debug_assertions)]
const RECOVERY_HOOK_TIMEOUT_FLOOR: Duration = Duration::from_millis(50);

/// Resolve the recovery hook timeout. Release builds always return
/// [`RECOVERY_HOOK_TIMEOUT`]; debug builds honor `AOE_RECOVERY_HOOK_TIMEOUT_MS`
/// for tests, clamped to [`RECOVERY_HOOK_TIMEOUT_FLOOR`].
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
// ponytail: save/restore covers LIFO nesting only; production installs a
// single scope (recovery), so out-of-order drops never occur.
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

    #[cfg(feature = "serve")]
    #[test]
    fn snapshot_recently_restarted_includes_fresh_excludes_missing() {
        let map = new_recently_restarted();
        mark_recently_restarted(&map, "abc");
        let snap = snapshot_recently_restarted(&map);
        assert!(snap.contains("abc"));
        assert!(!snap.contains("other"));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn snapshot_recently_restarted_excludes_expired() {
        let map = new_recently_restarted();
        let stale = Instant::now() - RECENTLY_RESTARTED_TTL * 2;
        {
            let mut g = map.write().unwrap();
            g.insert("stale".into(), stale);
        }
        mark_recently_restarted(&map, "fresh");
        let snap = snapshot_recently_restarted(&map);
        assert!(!snap.contains("stale"));
        assert!(snap.contains("fresh"));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn recently_restarted_gc_removes_stale_entries() {
        let map = new_recently_restarted();
        let stale = Instant::now() - RECENTLY_RESTARTED_TTL * 3;
        let fresh = Instant::now();
        {
            let mut g = map.write().unwrap();
            g.insert("stale".into(), stale);
            g.insert("fresh".into(), fresh);
        }
        gc_recently_restarted(&map);
        let g = map.read().unwrap();
        assert!(!g.contains_key("stale"));
        assert!(g.contains_key("fresh"));
    }

    /// Regression for the queued-candidate TTL race (#1264): the background
    /// refresher must not resurrect a mark that a completed worker has just
    /// cleared. The worker drains its id from `recovery_pending` *before*
    /// unmarking; a subsequent refresher tick sees an empty (for that id)
    /// pending set and leaves `recently_restarted` clear. Without the drain,
    /// the refresher would re-stamp the id forever and suppress its real
    /// status for the rest of the cascade.
    #[cfg(feature = "serve")]
    #[test]
    fn refresher_does_not_resurrect_drained_worker_mark() {
        let recently = new_recently_restarted();
        let pending = new_recovery_pending();

        // Phase A: schedule the candidate and stamp its initial mark.
        seed_recovery_pending(&pending, ["abc".to_string()]);
        mark_recently_restarted(&recently, "abc");

        // A refresher tick while the worker is still queued keeps it fresh.
        assert!(
            refresh_recovery_pending(&pending, &recently),
            "non-empty pending set should keep ticking",
        );
        assert!(
            recently.read().unwrap().contains_key("abc"),
            "refresher must keep a queued candidate's mark fresh",
        );

        // Worker completes: drain from pending, then unmark.
        drain_recovery_pending(&pending, &recently, "abc");
        assert!(
            !recently.read().unwrap().contains_key("abc"),
            "drain must clear the suppression mark",
        );

        // A later refresher tick must not bring the mark back, and reports
        // the set as drained so the loop can exit.
        assert!(
            !refresh_recovery_pending(&pending, &recently),
            "empty pending set signals the refresher to stop",
        );
        assert!(
            !recently.read().unwrap().contains_key("abc"),
            "refresher must not resurrect a drained worker's mark",
        );
    }

    /// The refresher keeps a still-queued candidate marked while a *different*
    /// candidate finishes. Draining one id must not stop refreshing the rest.
    #[cfg(feature = "serve")]
    #[test]
    fn refresher_keeps_remaining_candidates_after_partial_drain() {
        let recently = new_recently_restarted();
        let pending = new_recovery_pending();
        seed_recovery_pending(&pending, ["done".to_string(), "queued".to_string()]);

        // First worker finishes; the second is still waiting on a permit.
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

    /// The two tests above are sequential, so they would still pass even if
    /// [`refresh_recovery_pending`] snapshotted the ids and *released* the
    /// `pending` read lock before stamping. That ordering is the whole point
    /// of the fix, so prove it under a real lock overlap: hold the `pending`
    /// read lock (standing in for a refresher mid-tick), start a concurrent
    /// drain that blocks on the write lock, stamp the mark at the last
    /// possible moment while still holding the read lock, then release and
    /// let the drain finish. The drain's unmark must win.
    ///
    /// This fails if [`drain_recovery_pending`] is reordered to unmark before
    /// taking `W(pending)`: the premature unmark would race ahead of the
    /// stamp and the id would be resurrected.
    #[cfg(feature = "serve")]
    #[test]
    fn refresher_mark_loses_to_concurrent_drain_under_lock_overlap() {
        use std::thread;
        use std::time::Duration;

        let recently = new_recently_restarted();
        let pending = new_recovery_pending();
        seed_recovery_pending(&pending, ["x".to_string()]);
        mark_recently_restarted(&recently, "x");

        // Stand in for a refresher tick that is *inside* its `pending`
        // read-lock scope and has not yet stamped.
        let read_guard = pending.read().unwrap();

        // A worker completes concurrently. `drain_recovery_pending` takes
        // `W(pending)` first, which blocks behind our read lock, so its
        // unmark is forced to serialize after we release.
        let drain_pending = pending.clone();
        let drain_recently = recently.clone();
        let drainer = thread::spawn(move || {
            drain_recovery_pending(&drain_pending, &drain_recently, "x");
        });

        // Give the drainer time to reach (and block on) the write lock, or,
        // if drain were buggily reordered to unmark first, to perform that
        // premature unmark. Then stamp at the latest possible moment, exactly
        // as the refresher would just before releasing its read lock.
        thread::sleep(Duration::from_millis(100));
        mark_recently_restarted(&recently, "x");

        // Release: the blocked drain now removes the id and unmarks.
        drop(read_guard);
        drainer.join().unwrap();

        assert!(
            !pending.read().unwrap().contains("x"),
            "drain must remove the id from the pending set",
        );
        assert!(
            !recently.read().unwrap().contains_key("x"),
            "the worker's unmark must win over the refresher's last mark; \
             no mark-after-unmark resurrection",
        );
    }

    /// Regression: archiving a session kills its tmux pane, so the next
    /// startup observes a dead pane on a resume-capable agent. Without an
    /// archive guard on `is_recovery_candidate`, the cascade respawns the
    /// row the user just dismissed (reported: "archive a session, leave
    /// and re-enter the TUI, it restarts").
    #[test]
    fn archived_instance_is_not_recovery_candidate() {
        let mut inst = Instance::new("archived", "/tmp/test");
        inst.agent_session_id = Some("11111111-1111-4111-8111-111111111111".into());
        assert!(
            is_recovery_candidate(&inst),
            "baseline: claude + valid sid is a recovery candidate"
        );
        inst.archive();
        assert!(
            !is_recovery_candidate(&inst),
            "archived sessions must be excluded from startup recovery"
        );
        inst.unarchive();
        assert!(
            is_recovery_candidate(&inst),
            "unarchive must restore recovery eligibility"
        );
    }

    /// Regression for #1583: pressing `x` in the session picker stops a
    /// session, which sets `Status::Stopped` and kills the tmux pane. The
    /// next TUI launch (or daemon startup) sees a dead pane on a resume-
    /// capable agent; without a Stopped guard, the cascade respawns the row
    /// the user just stopped. Only an explicit user action (Enter / send /
    /// live-send) transitions Stopped to Starting, which is consulted before
    /// recovery runs.
    #[test]
    fn stopped_instance_is_not_recovery_candidate() {
        let mut inst = Instance::new("stopped", "/tmp/test");
        inst.agent_session_id = Some("33333333-3333-4333-8333-333333333333".into());
        assert!(
            is_recovery_candidate(&inst),
            "baseline: claude + valid sid is a recovery candidate"
        );
        inst.status = super::super::Status::Stopped;
        assert!(
            !is_recovery_candidate(&inst),
            "stopped sessions must be excluded from startup recovery"
        );
        inst.status = super::super::Status::Starting;
        assert!(
            is_recovery_candidate(&inst),
            "transitioning off Stopped (e.g. user reopens) must restore recovery eligibility"
        );
    }

    /// Snooze is the temporary sibling of archive. While the timer is in
    /// the future, the row sits in tier 99 and must not be revived by a
    /// pane-dead probe; once the timer expires, `is_snoozed()` flips to
    /// false and the row naturally rejoins the recovery set.
    #[test]
    fn snoozed_instance_is_not_recovery_candidate_until_expiry() {
        let mut inst = Instance::new("snoozed", "/tmp/test");
        inst.agent_session_id = Some("22222222-2222-4222-8222-222222222222".into());
        inst.snooze(30);
        assert!(
            !is_recovery_candidate(&inst),
            "snoozed sessions must be excluded while the timer is live"
        );
        inst.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        assert!(
            is_recovery_candidate(&inst),
            "expired snooze must restore recovery eligibility"
        );
    }

    /// A valid, long session id that no live process carries returns `false`
    /// (the genuine post-reboot case: the agent processes are gone). Also
    /// covers the no-sid path, where only the `AOE_INSTANCE_ID` env needle is
    /// scanned and no live process carries this synthetic id.
    #[test]
    fn orphaned_agent_process_alive_false_when_no_process_matches() {
        let mut inst = Instance::new("absent", "/tmp/test");
        inst.id = format!("absent{:012}", std::process::id());
        inst.agent_session_id = None;
        assert!(
            !orphaned_agent_process_alive(&inst),
            "no matching live process (no sid) must allow recovery",
        );

        // Non-hook agent so the sid needle is actually built and tested absent.
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some(format!(
            "11111111-1111-4111-8111-{:012}",
            std::process::id()
        ));
        assert!(
            !orphaned_agent_process_alive(&inst),
            "no matching live process must allow recovery",
        );
    }

    /// A too-short session id is not trusted as a cmdline needle; with no live
    /// process carrying the env id either, the guard returns `false`. Uses a
    /// non-hook agent so the sid path (not the env marker) is exercised.
    #[test]
    fn orphaned_agent_process_alive_ignores_short_sid() {
        let mut inst = Instance::new("short-sid", "/tmp/test");
        inst.id = format!("shortsid{:012}", std::process::id());
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("short".into());
        assert!(
            !orphaned_agent_process_alive(&inst),
            "a sub-{ORPHAN_SCAN_MIN_SID_LEN}-char sid must not be trusted, and nothing else matches",
        );
    }

    /// End-to-end #2994: a *non-hook* agent still alive off-socket, detected by
    /// the `agent_session_id` in a live process's argv (hook agents match on
    /// the env marker instead; see the fork-collision rationale).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn orphaned_agent_process_alive_detects_live_agent_by_sid() {
        let sid = format!("22222222-2222-4222-8222-{:012}", std::process::id());
        let mut inst = Instance::new("orphan-sid", "/tmp/test");
        inst.id = format!("orphansid{:012}", std::process::id());
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some(sid.clone());

        // Stand in for the orphaned `<agent> --resume <sid>` child: the sid
        // rides as `$0` of a compound-list `sh` so it stays alive with the id
        // in argv.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30; true")
            .arg(&sid)
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
            "a live agent carrying the sid in argv must be detected as an orphan",
        );
    }

    /// End-to-end #2994, argv-rewrite-proof path: an agent whose sid is absent
    /// from argv is still detected via the `AOE_INSTANCE_ID` env marker aoe
    /// injects for hook agents. Linux-only (stable `/proc/<pid>/environ`).
    #[cfg(target_os = "linux")]
    #[test]
    fn orphaned_agent_process_alive_detects_live_agent_by_env_marker() {
        let mut inst = Instance::new("orphan-env", "/tmp/test");
        inst.id = format!("orphanenv{:012}", std::process::id());
        // No sid at all: the only identity signal is the env marker.
        inst.agent_session_id = None;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env(crate::tmux::env::AOE_INSTANCE_ID_KEY, &inst.id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn env-marker orphan stand-in");

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
            "a live agent carrying AOE_INSTANCE_ID in its env must be detected as an orphan",
        );
    }

    /// The boot-scoped ledger round-trips: a marked id is reported attempted,
    /// an unmarked id is not. Isolated to a tempdir via the env override so it
    /// never touches real user state. `#[serial]` because the override is a
    /// process-global env var.
    #[test]
    #[serial_test::serial]
    fn recovery_attempt_ledger_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(RECOVERY_ATTEMPT_DIR_ENV, dir.path());
        if crate::process::boot_id().is_none() {
            std::env::remove_var(RECOVERY_ATTEMPT_DIR_ENV);
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

        std::env::remove_var(RECOVERY_ATTEMPT_DIR_ENV);
    }

    #[test]
    fn resume_probe_failed_sid_is_not_recovery_candidate_until_user_action_changes_state() {
        let sid = "44444444-4444-4444-8444-444444444444".to_string();
        let mut inst = Instance::new("resume-failed", "/tmp/test");
        inst.agent_session_id = Some(sid.clone());
        inst.resume_probe_failed_sid = Some(sid.clone());

        assert!(
            !is_recovery_candidate(&inst),
            "startup recovery must not loop on an ambiguously failed resume sid"
        );

        inst.resume_probe_failed_sid = None;
        assert!(
            is_recovery_candidate(&inst),
            "clearing the marker through an explicit path restores recovery eligibility"
        );
    }

    /// Cross-process exclusion is a POSIX `flock(2)` guarantee, not
    /// something this unit test can verify (BSD flock and Linux flock
    /// both treat all fds in the same process as one holder; only a
    /// distinct process would be locked out). This test only verifies
    /// the wrapper successfully creates the lock file and acquires/
    /// releases the lock without erroring. The cross-process behavior
    /// is exercised by the e2e suite (TUI + daemon spawned together).
    ///
    /// Driven through `try_acquire_recovery_lock_at` rather than the
    /// public entry point so the lock path is fixed and independent of
    /// `HOME` / `XDG_CONFIG_HOME`. The public function reads those env
    /// vars via `dirs::config_dir()`; `getenv` and `setenv` are not
    /// thread-safe, and non-`#[serial]` HOME readers elsewhere in the
    /// suite have been observed to race a `set_var` from another test
    /// and resolve the lock path under the wrong sandbox, surfacing as
    /// a flaky "re-acquisition after drop" failure on CI.
    #[test]
    fn recovery_lock_acquires_and_releases() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join(".recovery.lock");

        let first = try_acquire_recovery_lock_at(&path).unwrap();
        assert!(first.is_some(), "acquisition should succeed");
        drop(first);
        let second = try_acquire_recovery_lock_at(&path).unwrap();
        assert!(second.is_some(), "re-acquisition after drop should succeed");
    }

    /// `HookTimeout` carries the lifecycle-agnostic shape; the helper adds the
    /// `on_launch` framing that the recovery cascade is the only producer of
    /// today. AC #3 of #1889 specifies the exact `last_error` text.
    #[test]
    fn format_recovery_last_error_renders_hook_timeout_with_on_launch_prefix() {
        let err = anyhow::Error::new(super::super::repo_config::HookTimeout {
            cmd: "sleep 60".to_string(),
            timeout_secs: 30,
        });
        assert_eq!(
            format_recovery_last_error(&err),
            "on_launch hook timed out after 30s: sleep 60",
        );
    }

    #[test]
    fn stamp_recovery_error_sets_error_status_and_operator_fields() {
        let mut inst = Instance::new("timeout", "/tmp/test");
        let before = std::time::Instant::now();
        let err = anyhow::Error::new(super::super::repo_config::HookTimeout {
            cmd: "sleep 60".to_string(),
            timeout_secs: 30,
        });

        stamp_recovery_error(&mut inst, &err);

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

    /// Non-timeout cascade failures keep the historical wrapper so they stay
    /// unambiguously cascade-attributed in operator logs.
    #[test]
    fn format_recovery_last_error_falls_back_to_recovery_cascade_wrapper() {
        let err = anyhow::anyhow!("tmux session is gone");
        assert_eq!(
            format_recovery_last_error(&err),
            "recovery cascade: tmux session is gone",
        );
    }

    /// A future `.context("...")` wrap somewhere in the cascade must not
    /// regress the timeout classification: the helper walks the full chain,
    /// not just the root, so a contextualized `HookTimeout` still produces
    /// the timeout-shaped message.
    #[test]
    fn format_recovery_last_error_walks_chain_through_context() {
        let err = anyhow::Error::new(super::super::repo_config::HookTimeout {
            cmd: "echo hi && sleep 60".to_string(),
            timeout_secs: 12,
        })
        .context("recovery cascade tier 1");
        assert_eq!(
            format_recovery_last_error(&err),
            "on_launch hook timed out after 12s: echo hi && sleep 60",
        );
    }
}
