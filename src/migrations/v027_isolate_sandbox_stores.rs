//! Migration v027: move shared sandbox stores to the private v2 layout.
//!
//! A live legacy cohort remains readable until it stops. Stopped cohorts are
//! planned under a global transition lock, copied under a per-root cohort lock
//! so unrelated registry writes carry on, then synced, atomically published
//! under the transition lock again, and switched by their durable generation
//! field. Staging directories, the journal, and the legacy quarantine exist
//! only while that bounded transition is pending; the committed state keeps
//! only `sandbox-v2/<instance>`.

use super::progress;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const JOURNAL: &str = ".v027-sandbox-transition.json";
pub(crate) const LOCK: &str = ".v027-sandbox-transition.lock";
/// Set to any non-empty value to start without moving stores: every sandboxed
/// session stays on its shared store and is retried on a later start (or with
/// `aoe migrate`). The schema version still advances, so this is a deferral,
/// not a downgrade.
pub const DEFER_ENV: &str = "AOE_DEFER_SANDBOX_MIGRATION";

fn defer_requested() -> bool {
    defer_requested_by(std::env::var_os(DEFER_ENV).as_deref())
}

fn defer_requested_by(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Progress counters for one store move, plus whether this filesystem pair
/// has already refused a clone. Per move, not process-wide: cohort locks let
/// moves run at once, each reporting on its own thread (#3777).
#[derive(Default)]
struct CopyState {
    files: u64,
    bytes: u64,
    /// Only the Unix copy path clones; the portable fallback uses `fs::copy`.
    #[cfg(unix)]
    clone: super::store_fs::CloneSupport,
}

impl CopyState {
    /// One file copied, reported every hundredth so a large store shows
    /// movement without flooding the reporter.
    fn copied_file(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
        if self.files % 100 == 0 {
            progress::progress(format!(
                "{} files, {}",
                self.files,
                progress::format_bytes(self.bytes)
            ));
        }
    }
}

/// Ask the runtime for every sandbox container once, rather than paying a
/// subprocess per row; anything the listing does not cover still gets the
/// per-row probe, so an unreachable runtime keeps reading as live.
///
/// The snapshot is taken at the first probe and reused for the pass, so a
/// container started mid-pass reads as stopped for later cohorts. That is
/// contained: `reap_migrated_container` removes without force, so such a
/// container fails the removal and the transition rather than losing its
/// store under a running agent.
///
/// `announce` lets the fallback say once per pass that the runtime could not
/// be asked; the per-startup reconcile passes `false` so a machine whose
/// runtime is down is not told on every command.
pub(crate) fn batched_running_probe(announce: bool) -> impl Fn(&str) -> Result<bool> {
    batched_running_probe_with(
        crate::containers::batch_container_states,
        probe_container_running,
        announce,
    )
}

thread_local! {
    /// Bumped by [`refresh_liveness`]; a batched probe lists again when it
    /// no longer matches the epoch its snapshot was taken under.
    static LIVENESS_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Invalidate the container listing a batched probe on this thread cached,
/// so its next answer asks the runtime again. Publishing calls this because
/// the copy before it ran without the transition lock, and a container may
/// have come up meanwhile.
pub(crate) fn refresh_liveness() {
    LIVENESS_EPOCH.with(|epoch| epoch.set(epoch.get() + 1));
}

/// [`batched_running_probe`] over an injected listing and per-row inspect.
///
/// The listing answers only where it is certain: `paused` and `restarting`
/// are live, since the container still holds its mounts and a non-forced
/// removal would refuse it, while anything transitional or unrecognised is
/// inspected rather than read as stopped.
pub(crate) fn batched_running_probe_with(
    batch: impl Fn() -> std::collections::HashMap<String, crate::containers::ContainerState>,
    inspect: impl Fn(&str) -> Result<(bool, bool)>,
    announce: bool,
) -> impl Fn(&str) -> Result<bool> {
    batched_probe_with(
        batch,
        crate::containers::ContainerState::is_live,
        inspect,
        "checking which sandbox containers are running",
        announce,
    )
}

/// [`batched_running_probe_with`] over an arbitrary reading of a listed
/// state, so another question reuses the batching and the fail-closed
/// fallback. `listed` returning `None` falls through to `inspect`, the only
/// path that can tell "absent" from "could not be asked".
pub(crate) fn batched_probe_with(
    batch: impl Fn() -> std::collections::HashMap<String, crate::containers::ContainerState>,
    listed: impl Fn(crate::containers::ContainerState) -> Option<bool>,
    inspect: impl Fn(&str) -> Result<(bool, bool)>,
    step: &'static str,
    announce: bool,
) -> impl Fn(&str) -> Result<bool> {
    let snapshot: std::cell::RefCell<
        Option<(
            u64,
            std::collections::HashMap<String, crate::containers::ContainerState>,
        )>,
    > = std::cell::RefCell::new(None);
    let runtime_noticed = std::cell::Cell::new(false);
    move |id: &str| {
        let epoch = LIVENESS_EPOCH.with(std::cell::Cell::get);
        let mut snapshot = snapshot.borrow_mut();
        if !matches!(&*snapshot, Some((at, _)) if *at == epoch) {
            progress::step(step);
            *snapshot = Some((epoch, batch()));
        }
        let listed = snapshot
            .as_ref()
            .and_then(|(_, states)| {
                states.get(&crate::containers::DockerContainer::generate_name(id))
            })
            .and_then(|state| listed(*state));
        drop(snapshot);
        if let Some(live) = listed {
            return Ok(live);
        }
        let (running, unanswered) = inspect(id)?;
        if unanswered && announce && !runtime_noticed.replace(true) {
            progress::notice(
                "container runtime unavailable; sandboxed sessions keep their shared agent store until their containers can be checked",
            );
        }
        Ok(running)
    }
}

/// Whether a migrated row's container is live, so its store is left alone
/// this pass, and whether that answer is the fail-closed substitute for a
/// runtime that could not be asked. Publishing a store while a container AoE
/// cannot see may still be writing it is the one outcome this migration must
/// never produce, so an unknown answer copies nothing.
fn probe_container_running(id: &str) -> Result<(bool, bool)> {
    match crate::containers::DockerContainer::from_session_id(id).is_running() {
        Ok(running) => Ok((running, false)),
        Err(error) if runtime_cannot_answer(&error) => {
            tracing::warn!("v027 treating {id} as live: container runtime unavailable ({error})");
            Ok((true, true))
        }
        Err(error) => Err(error.into()),
    }
}

/// Reports whether a migrated row's container is live. See
/// [`probe_container_running`] for what an unreachable runtime answers.
pub(crate) type RunningProbe<'a> = dyn Fn(&str) -> Result<bool> + 'a;

/// Reaps the stopped container of a row whose store has moved. `Ok(false)`
/// leaves the row pending; see [`reap_migrated_container`].
type ReapProbe<'a> = dyn Fn(&str) -> Result<bool> + 'a;

/// Whether a runtime error means the runtime could not answer, rather than
/// saying anything about the container.
///
/// An absent binary, a stopped daemon, a denied socket and the
/// `InspectFailed` catch-all all mean AoE asked and learned nothing. None may
/// abort the migration: that aborts `run_migrations` before the schema
/// version commits, so every later `aoe` fails too. Callers substitute their
/// own fail-closed answer and leave the row pending. A local I/O fault is a
/// real failure and still propagates.
pub(crate) fn runtime_cannot_answer(error: &crate::containers::error::DockerError) -> bool {
    use crate::containers::error::DockerError;
    matches!(
        error,
        DockerError::NotInstalled
            | DockerError::DaemonNotRunning
            | DockerError::PermissionDenied
            | DockerError::InspectFailed(_)
    )
}

/// Remove the stopped container of a row whose store has moved, so its next
/// launch recreates it against the private layout.
///
/// `Ok(false)` means the runtime could not answer, which leaves the row
/// pending for a later pass. A `remove` that fails for any other reason still
/// aborts: `force=false` is what makes a container that became live after the
/// probe fail the transition rather than be stopped underneath its agent.
pub(super) fn reap_migrated_container(id: &str) -> Result<bool> {
    let container = crate::containers::DockerContainer::from_session_id(id);
    match container.exists() {
        Ok(true) => {
            container.remove(false)?;
            Ok(true)
        }
        Ok(false) => Ok(true),
        Err(error) if runtime_cannot_answer(&error) => {
            tracing::warn!("v027 deferring {id}: container runtime unavailable ({error})");
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// What this pass may do with one row's store.
///
/// Every sandboxed row becomes a `Target` whatever its disposition, because
/// the liveness gate reasons over whole cohorts: a member missing from its
/// cohort is one the gate never asks about, and its peers' store is then
/// published while that session is still writing to it. Eligibility is a
/// property of the target, never a filter on the rows that build one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Disposition {
    /// Copy and publish this store once the cohort is quiescent.
    Move,
    /// Leave this row on the shared store: it is parked, or this pass was
    /// scoped to a different cohort. It is still a cohort member, so it is
    /// still asked about, and it still holds its source against retirement.
    Hold,
}

#[derive(Clone)]
struct Target {
    registry: usize,
    row: usize,
    id: String,
    shared: PathBuf,
    private: PathBuf,
    cleanup_root: PathBuf,
    disposition: Disposition,
}

struct Registry {
    path: PathBuf,
    value: Value,
}

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for sandbox migration")?;
    run_in(
        &app_dir,
        &home,
        &batched_running_probe(true),
        &reap_migrated_container,
        defer_requested(),
        true,
        None,
    )
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) struct TestReconcileProbes {
    pub(super) running: fn(&str) -> Result<bool>,
    pub(super) reap: fn(&str) -> Result<bool>,
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_RECONCILE_PROBES: std::cell::Cell<Option<TestReconcileProbes>> = const { std::cell::Cell::new(None) };
}

/// Retry cohorts that were live during the schema migration. This is called on
/// every startup until no pre-v2 row remains, then becomes a cheap read.
/// `announce` narrates pending rows.
pub(crate) fn reconcile_pending(announce: bool) -> Result<()> {
    #[cfg(test)]
    if let Some(probes) = TEST_RECONCILE_PROBES.get() {
        return reconcile_scoped(announce, None, &probes.running, &probes.reap);
    }
    reconcile_scoped(
        announce,
        None,
        &batched_running_probe(announce),
        &reap_migrated_container,
    )
}

/// Move one session's store for the start about to launch it, together with
/// the cohort sharing it, since the cohort is the unit the liveness gate
/// reasons about. Every other cohort is left alone, so one launch does not pay
/// for every pending store on the machine.
pub(crate) fn migrate_instance(id: &str) -> Result<()> {
    reconcile_scoped(
        false,
        Some(id),
        &batched_running_probe(false),
        &reap_migrated_container,
    )
}

/// [`migrate_instance`] with the container probes injected, so a test can
/// drive the launch-time move end to end with no container runtime.
#[cfg(test)]
pub(crate) fn migrate_instance_with(
    id: &str,
    is_running: &RunningProbe<'_>,
    reap: &ReapProbe<'_>,
) -> Result<()> {
    reconcile_scoped(false, Some(id), is_running, reap)
}

/// `only` scopes the move to a single instance; `announce` both narrates and
/// selects the bulk path, so a bare `aoe` start reports what is pending
/// without copying while `aoe migrate` moves everything eligible.
fn reconcile_scoped(
    announce: bool,
    only: Option<&str>,
    is_running: &RunningProbe<'_>,
    reap: &ReapProbe<'_>,
) -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    if !transition_may_be_pending(&app_dir, !announce && only.is_none())? {
        return Ok(());
    }
    let home = dirs::home_dir().context("home directory unavailable for sandbox migration")?;
    let start = std::time::Instant::now();
    progress::report(progress::Event::Started {
        version: 27,
        name: "isolate_sandbox_stores",
        position: 1,
        total: 1,
    });
    run_in(
        &app_dir,
        &home,
        is_running,
        reap,
        defer_requested() || (!announce && only.is_none()),
        announce,
        only,
    )?;
    progress::report(progress::Event::Finished {
        version: 27,
        elapsed: start.elapsed(),
    });
    Ok(())
}

/// Whether a pass has anything to do. A bare start copies, publishes and
/// retires nothing, so the journal and a parked row, even one planned before
/// it was parked, are work only for a launch or `aoe migrate`.
fn transition_may_be_pending(app_dir: &Path, bare_start: bool) -> Result<bool> {
    if !bare_start && app_dir.join(JOURNAL).exists() {
        return Ok(true);
    }
    for registry in load_registries(app_dir)? {
        let Some(rows) = registry.value.as_array() else {
            continue;
        };
        if rows.iter().any(|row| {
            if on_shared_store(row) {
                !(bare_start && row_is_parked(row))
            } else {
                is_sandboxed(row) && transition_paths(row).ok().flatten().is_some()
            }
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_sandboxed(row: &Value) -> bool {
    row.pointer("/sandbox_info/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn on_shared_store(row: &Value) -> bool {
    is_sandboxed(row)
        && row
            .get("sandbox_store_generation")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            < u64::from(crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION)
}

/// How many sandboxed sessions still use a shared agent store, so `aoe
/// migrate` does not call a run that left some there complete.
pub(crate) fn sessions_on_shared_store() -> Result<usize> {
    let app_dir = crate::session::get_app_dir()?;
    Ok(load_registries(&app_dir)?
        .iter()
        .filter_map(|registry| registry.value.as_array())
        .flatten()
        .filter(|row| on_shared_store(row))
        .count())
}

/// Whether a row's move is published but unfinished, so its private store is
/// being written.
///
/// Narrower than [`transition_may_be_pending`], which also says yes for a row
/// merely still on the shared store and for a journal naming an unretired
/// root. A parked row holds both for as long as it stays parked, so neither
/// ever returns to `false` and neither can gate a user-facing command.
pub(crate) fn transition_in_flight(app_dir: &Path) -> Result<bool> {
    for registry in load_registries(app_dir)? {
        let Some(rows) = registry.value.as_array() else {
            continue;
        };
        // Not `.ok()`: metadata the migration cannot parse is state we cannot
        // validate, and reading it as "no transition" would let a reclaim run
        // against a store move it cannot see.
        for row in rows {
            if transition_paths(row)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether a row is trashed or archived. Such a session is not about to be
/// started, and a trashed one is usually deleted within
/// `trash_retention_days`, so copying its store costs a full store and buys
/// nothing; it migrates on the start that follows a restore.
///
/// A parked row still blocks retirement of the shared source it reads, via
/// `defer_source_retirement`: retiring underneath it would leave a restored
/// session with no store to open.
///
/// A pass scoped to the parked row itself ignores this: `aoe send`, `aoe
/// session start` and the HTTP start/send handlers all launch a trashed or
/// archived session without unparking it first, and skipping it there leaves
/// it on a shared store no later pass will move, so the launch bails forever.
pub(super) fn row_is_parked(row: &Value) -> bool {
    ["trashed_at", "archived_at"]
        .iter()
        .any(|key| row.get(key).is_some_and(|value| !value.is_null()))
}

/// `defer_stores` leaves every cohort on its shared store this pass (see
/// [`DEFER_ENV`]); rows stay pending as they do behind a live container.
/// `announce` narrates deferrals and pending rows: the schema migration and
/// `aoe migrate` do, the per-startup reconcile reports only stores it moves.
fn run_in(
    app_dir: &Path,
    home: &Path,
    is_running: &RunningProbe<'_>,
    reap: &ReapProbe<'_>,
    defer_stores: bool,
    announce: bool,
    only: Option<&str>,
) -> Result<()> {
    progress::step("reading session registries");
    fs::create_dir_all(app_dir)?;
    // A scoped pass that finds its cohort mid-transition in another process
    // waits for that pass and looks again; the row is then usually current.
    for _ in 0..SCOPED_RETRIES {
        match run_pass(
            app_dir,
            home,
            is_running,
            reap,
            defer_stores,
            announce,
            only,
        )? {
            PassOutcome::Done => return Ok(()),
            PassOutcome::WaitFor(root) => {
                progress::step(format!(
                    "waiting for another process to finish moving {}",
                    root.display()
                ));
                drop(acquire_cohort_lock(app_dir, &root)?);
                refresh_liveness();
            }
        }
    }
    tracing::warn!(
        "v027 giving up on {} after {SCOPED_RETRIES} waits; it stays on its shared store for now",
        only.unwrap_or("this pass")
    );
    Ok(())
}

/// How many times a scoped pass looks again after waiting for another
/// process's transition of its cohort.
const SCOPED_RETRIES: usize = 3;

/// A pass that finished, or one that found the named root mid-transition in
/// another process and, being scoped to a row under it, must wait for that
/// process before looking again.
enum PassOutcome {
    Done,
    WaitFor(PathBuf),
}

/// The lock a pass holds on one legacy root while it copies and publishes the
/// stores under it. Per root rather than global, so a copy that takes minutes
/// blocks neither registry writes nor moves under other roots; the transition
/// lock covers only planning and publishing.
///
/// Lock order is cohort, then transition, then registry. A holder of the
/// transition lock therefore never waits for a cohort lock: `run_pass` only
/// tries it, and leaves a busy root pending.
fn cohort_lock_name(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(".v027-cohort-{hex}.lock")
}

pub(super) fn acquire_cohort_lock(
    app_dir: &Path,
    root: &Path,
) -> Result<crate::session::StorageFlock> {
    crate::session::acquire_storage_flock(app_dir, &cohort_lock_name(root))
}

fn try_acquire_cohort_lock(
    app_dir: &Path,
    root: &Path,
) -> Result<Option<crate::session::StorageFlock>> {
    crate::session::try_acquire_storage_flock(app_dir, &cohort_lock_name(root))
}

#[cfg(test)]
pub(crate) type CopyGate = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    /// A test's hold on the copy phase: called once per root about to be
    /// copied, before its first `publish_store`, on the thread running the
    /// pass, with only that root's cohort lock held.
    pub(crate) static COPY_GATE: std::cell::RefCell<Option<CopyGate>> =
        const { std::cell::RefCell::new(None) };
}

fn copy_gate(root: &Path) {
    #[cfg(test)]
    COPY_GATE.with(|gate| {
        if let Some(gate) = gate.borrow().as_ref() {
            gate(root);
        }
    });
    #[cfg(not(test))]
    let _ = root;
}

/// One pass over the registries: plan under the transition and registry
/// locks, copy under the cohort locks alone, then reacquire the former,
/// revalidate the plan against the registries as they are now, and publish.
fn run_pass(
    app_dir: &Path,
    home: &Path,
    is_running: &RunningProbe<'_>,
    reap: &ReapProbe<'_>,
    defer_stores: bool,
    announce: bool,
    only: Option<&str>,
) -> Result<PassOutcome> {
    let mut transition_lock = Some(crate::session::acquire_storage_flock(app_dir, LOCK)?);
    let planned_paths = registry_paths(app_dir)?;
    let registry_dirs = registry_dirs_of(&planned_paths);
    let mut registry_locks = Some(lock_registry_dirs(&registry_dirs)?);
    let mut registries = load_registry_paths(planned_paths)?;
    let journal = app_dir.join(JOURNAL);
    match fs::read(&journal) {
        Ok(bytes) => {
            let _: Vec<String> =
                serde_json::from_slice(&bytes).context("parsing v027 transition journal")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut targets = Vec::new();
    let mut affected_rows = BTreeSet::new();
    // Rows this pass will not copy but must still count as cohort members,
    // so the liveness fold below asks about them before publishing a store.
    let mut known_sources = BTreeSet::new();
    let mut cleanup_roots = BTreeSet::new();
    let mut needs_registry_write = false;
    let mut defer_source_retirement = false;
    let row_ids_by_root = collect_row_ids_by_root(&registries, app_dir, home)?;

    for (registry_index, registry) in registries.iter_mut().enumerate() {
        let profile = profile_for_registry(app_dir, &registry.path);
        let Some(rows) = registry.value.as_array_mut() else {
            continue;
        };
        for (row_index, row) in rows.iter_mut().enumerate() {
            if !row
                .pointer("/sandbox_info/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let generation = row
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let Some(id) = row.get("id").and_then(Value::as_str).map(str::to_owned) else {
                defer_source_retirement = true;
                continue;
            };
            crate::session::validate_instance_id(&id)?;
            if generation
                >= u64::from(
                    crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
                )
            {
                if clear_transition_metadata(row) {
                    needs_registry_write = true;
                }
                continue;
            }
            // Parked defers per root, through the `Hold` target `all_ready`
            // refuses to retire under. The pass-wide flag would block every
            // unrelated root on a machine with one archived session.
            let parked = row_is_parked(row) && only != Some(id.as_str());
            let Some(tool) = row.get("tool").and_then(Value::as_str) else {
                defer_source_retirement = true;
                continue;
            };
            let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
            let detect_as = row
                .get("detect_as")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| config.session.agent_detect_as.get(tool).map(String::as_str));
            let Some(agent) = crate::agents::get_agent(tool)
                .or_else(|| detect_as.and_then(crate::agents::get_agent))
            else {
                defer_source_retirement = true;
                continue;
            };
            let declared = config.session.agent_config_dir_for(tool, home);
            let mut fresh_plans =
                crate::session::config::container_config::sandbox_store_migration_paths(
                    agent.name,
                    home,
                    declared.as_deref(),
                    &id,
                )?;
            let stored_plans = transition_paths(row)
                .with_context(|| format!("validating v027 transition plan for {id}"))?;
            let stored_private = stored_plans.as_ref().is_some_and(|plans| {
                plans.iter().all(|(source, _)| {
                    source.file_name().is_some_and(|name| name == id.as_str())
                        && source
                            .parent()
                            .and_then(Path::file_name)
                            .is_some_and(|name| name == "sandbox")
                })
            });
            let old_private = agent.name == "codex" || stored_private;
            if old_private {
                for (shared, _) in &mut fresh_plans {
                    *shared = shared.join(&id);
                }
            }
            let mut plans = if let Some(stored) = stored_plans.as_ref() {
                if stored.len() != fresh_plans.len()
                    || stored.iter().zip(&fresh_plans).any(
                        |((source, destination), (fresh_source, fresh_destination))| {
                            !same_authorized_path(destination, fresh_destination)
                                || !same_authorized_path(source, fresh_source)
                        },
                    )
                {
                    bail!(
                        "v027 checkpointed transition plan is outside the expected sandbox roots for {id}; restore the previous session.agent_config_dir before retrying"
                    );
                }
                stored.clone()
            } else {
                fresh_plans
            };
            for (shared, destination) in &mut plans {
                match fs::symlink_metadata(&*shared) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            bail!(
                                "v027 source is a symlink or non-directory: {}",
                                shared.display()
                            );
                        }
                        if stored_plans.is_none() {
                            *shared = fs::canonicalize(&*shared)?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspecting {}", shared.display()))
                    }
                }
                if stored_plans.is_none() {
                    *destination = resolve_existing_ancestor(destination)
                        .unwrap_or_else(|| destination.clone());
                }
            }
            cleanup_roots.extend(plans.iter().filter_map(|(source, _)| {
                if old_private {
                    source.parent().map(Path::to_path_buf)
                } else {
                    Some(source.clone())
                }
            }));
            // A parked row is carried only as a cohort member; it publishes
            // nothing this pass, so it gets neither the drift checkpoint nor
            // the pending stamp.
            if stored_plans.is_none() && !parked {
                set_transition_paths(row, &plans);
                needs_registry_write = true;
            }
            known_sources.extend(plans.iter().map(|(shared, _)| shared.clone()));
            known_sources.extend(cleanup_roots.iter().cloned());
            if plans.is_empty() {
                mark_current(row, generation, &mut needs_registry_write);
                continue;
            }
            let pending_generation = if old_private { 0 } else { 1 };
            if !parked && generation != u64::from(pending_generation) {
                set_generation(row, pending_generation);
                needs_registry_write = true;
            }
            if !parked {
                affected_rows.insert((registry_index, row_index));
            }
            let disposition = if parked {
                Disposition::Hold
            } else {
                Disposition::Move
            };
            targets.extend(plans.into_iter().map(|(shared, private)| {
                let cleanup_root = if old_private {
                    shared.parent().unwrap_or(&shared).to_path_buf()
                } else {
                    shared.clone()
                };
                Target {
                    registry: registry_index,
                    row: row_index,
                    id: id.clone(),
                    disposition,
                    shared,
                    private,
                    cleanup_root,
                }
            }));
        }
    }

    let mut cohorts: BTreeMap<PathBuf, Vec<Target>> = BTreeMap::new();
    for target in targets {
        cohorts
            .entry(target.shared.clone())
            .or_default()
            .push(target);
    }
    // Scoping demotes, never removes: an unnamed cohort keeps every member in
    // the liveness fold, so the gate still asks about sessions this pass will
    // not touch. Dropping them is what let an earlier revision copy a store
    // out from under a live peer.
    if let Some(wanted) = only {
        let selected: BTreeSet<PathBuf> = cohorts
            .iter()
            .filter(|(_, cohort)| cohort.iter().any(|target| target.id == wanted))
            .map(|(shared, _)| shared.clone())
            .collect();
        for (shared, cohort) in cohorts.iter_mut() {
            if selected.contains(shared) {
                continue;
            }
            // Another cohort still holds its shared source. `Hold` protects
            // it through `all_ready`; the pass-wide flag would also strand the
            // cohort this pass just emptied.
            for target in cohort.iter_mut() {
                target.disposition = Disposition::Hold;
            }
        }
    }
    // The rows this pass will actually publish. Derived from the targets, so a
    // row cannot be marked ready without a target that says it may move.
    let movable_rows: BTreeSet<(usize, usize)> = cohorts
        .values()
        .flatten()
        .filter(|target| target.disposition == Disposition::Move)
        .map(|target| (target.registry, target.row))
        .collect();
    // Reporting only: `affected_rows` excludes held rows, so without this the
    // completion notice claims the transition finished while parked sessions
    // are still on the shared store.
    let held_row_count = cohorts
        .values()
        .flatten()
        .filter(|target| target.disposition == Disposition::Hold)
        .map(|target| (target.registry, target.row))
        .collect::<BTreeSet<_>>()
        .len();
    if announce && defer_stores && !affected_rows.is_empty() {
        progress::notice(format!(
            "{DEFER_ENV} is set: {} sandboxed session(s) keep their shared agent store for now; \
             the move is retried on a later start or with `aoe migrate`.",
            affected_rows.len()
        ));
    } else if !announce && only.is_none() {
        // A bare start copies nothing, so without this it would also say
        // nothing, and the first later launch would pay for a move the user
        // was never warned about.
        if let Some(notice) = pending_work_notice(affected_rows.len(), held_row_count) {
            progress::notice(notice);
        }
    }
    if !needs_registry_write
        && cohorts.is_empty()
        && known_sources.iter().all(|source| !source.exists())
        && !journal.exists()
    {
        return Ok(PassOutcome::Done);
    }
    if needs_registry_write {
        for registry in &registries {
            let bytes = serde_json::to_vec_pretty(&registry.value)?;
            crate::session::atomic_write(&registry.path, &bytes)?;
            sync_parent(&registry.path)?;
        }
    }
    if !known_sources.is_empty() {
        let paths: Vec<String> = known_sources
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        crate::session::atomic_write(&journal, &serde_json::to_vec(&paths)?)?;
        sync_parent(&journal)?;
    }

    // Intersect rather than subtract: a row is publishable only if a target
    // says so, so no future filter can leave a row stamped current whose store
    // was never copied.
    let mut ready_rows: BTreeSet<(usize, usize)> = affected_rows
        .iter()
        .filter(|key| movable_rows.contains(key))
        .copied()
        .collect();
    let mut pending = Vec::new();
    let mut blocked_roots = BTreeSet::new();
    let mut orphan_blocked_roots = BTreeSet::new();
    let mut excluded_by_root = BTreeMap::new();
    let private_roots: BTreeSet<PathBuf> = cohorts
        .values()
        .flatten()
        .filter(|target| target.shared != target.cleanup_root)
        .map(|target| target.cleanup_root.clone())
        .collect();

    for root in &cleanup_roots {
        let mut excluded = BTreeSet::new();
        if private_roots.contains(root) {
            excluded = instance_children(root)?;
            excluded.extend(row_ids_by_root.get(root).into_iter().flatten().cloned());
            excluded.extend(
                cohorts
                    .values()
                    .flatten()
                    .filter(|target| &target.cleanup_root == root)
                    .map(|target| std::ffi::OsString::from(&target.id)),
            );
        }
        excluded_by_root.insert(root.clone(), excluded);
    }

    // Codex already used per-instance legacy children before this migration.
    // Preserve an unregistered child independently instead of overlaying it
    // into a registered peer or deleting it with the parent.
    let mut orphan_plan: Vec<(PathBuf, PathBuf, Vec<std::ffi::OsString>)> = Vec::new();
    for root in &private_roots {
        let mut row_ids: BTreeSet<std::ffi::OsString> = row_ids_by_root
            .get(root)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        row_ids.extend(
            cohorts
                .values()
                .flatten()
                .filter(|target| &target.cleanup_root == root)
                .map(|target| std::ffi::OsString::from(&target.id)),
        );
        let destination_parents: BTreeSet<PathBuf> = cohorts
            .values()
            .flatten()
            .filter(|target| &target.cleanup_root == root)
            .filter_map(|target| target.private.parent().map(Path::to_path_buf))
            .collect();
        let Some(destination_parent) = destination_parents
            .iter()
            .next()
            .filter(|_| destination_parents.len() == 1)
        else {
            if excluded_by_root
                .get(root)
                .is_some_and(|children| children.iter().any(|id| !row_ids.contains(id)))
            {
                blocked_roots.insert(root.clone());
                orphan_blocked_roots.insert(root.clone());
            }
            continue;
        };
        let orphans: Vec<std::ffi::OsString> = excluded_by_root
            .get(root)
            .into_iter()
            .flatten()
            .filter(|id| !row_ids.contains(*id))
            .cloned()
            .collect();
        if !orphans.is_empty() {
            orphan_plan.push((root.clone(), destination_parent.clone(), orphans));
        }
    }

    // The copies below run without the transition and registry locks, so each
    // root's own lock serialises it. Tried, not waited for (see
    // `cohort_lock_name`): a busy root is another process's transition, which
    // stays pending here, and a scoped pass waits for it and looks again.
    let mut copy_roots: BTreeSet<PathBuf> = cohorts
        .values()
        .flatten()
        .filter(|target| target.disposition == Disposition::Move)
        .map(|target| target.cleanup_root.clone())
        .collect();
    copy_roots.extend(orphan_plan.iter().map(|(root, _, _)| root.clone()));
    let mut cohort_locks = Vec::new();
    let mut busy_roots = BTreeSet::new();
    if !defer_stores {
        for root in &copy_roots {
            match try_acquire_cohort_lock(app_dir, root)? {
                Some(lock) => cohort_locks.push(lock),
                None => {
                    busy_roots.insert(root.clone());
                    blocked_roots.insert(root.clone());
                }
            }
        }
        // Copies must not hold the transition lock: `Storage::update` takes
        // it shared in every profile, so a copy under it stalls unrelated
        // session and group writes for its whole duration.
        registry_locks = None;
        transition_lock = None;
    }
    let wait_for = only.and_then(|wanted| {
        cohorts
            .values()
            .flatten()
            .find(|target| target.id == wanted && busy_roots.contains(&target.cleanup_root))
            .map(|target| target.cleanup_root.clone())
    });

    let mut gated_roots = BTreeSet::new();
    for (root, destination_parent, orphans) in &orphan_plan {
        if busy_roots.contains(root) {
            orphan_blocked_roots.insert(root.clone());
            continue;
        }
        for orphan in orphans {
            let id = orphan.to_string_lossy();
            let source = root.join(orphan);
            let metadata = fs::symlink_metadata(&source)?;
            if defer_stores
                || metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || is_running(&id)?
            {
                tracing::warn!(
                    "v027 preserving ambiguous or live orphan store: {}",
                    source.display()
                );
                blocked_roots.insert(root.clone());
                orphan_blocked_roots.insert(root.clone());
                continue;
            }
            progress::step(format!("moving unregistered store {}", source.display()));
            if gated_roots.insert(root.clone()) {
                copy_gate(root);
            }
            // Reaped before the move: a rename leaves no source for a
            // container that came up since the probe, and nothing can put one
            // back. Removing without force fails on a live container.
            if !reap(&id)? {
                blocked_roots.insert(root.clone());
                orphan_blocked_roots.insert(root.clone());
                continue;
            }
            if !relocate_store(&source, &destination_parent.join(orphan))? {
                // The orphan is still at its source, so this root keeps both,
                // and every row reading it waits with them for a later pass.
                tracing::warn!(
                    "v027 leaving orphan store {} in place: its retention was deferred",
                    source.display()
                );
                blocked_roots.insert(root.clone());
                orphan_blocked_roots.insert(root.clone());
            }
        }
    }

    for target in cohorts.values().flatten() {
        if orphan_blocked_roots.contains(&target.cleanup_root) {
            ready_rows.remove(&(target.registry, target.row));
        }
    }

    let total_targets: usize = cohorts
        .values()
        .flatten()
        .filter(|target| target.disposition == Disposition::Move)
        .count();
    let mut copied_targets = 0usize;
    let mut announced = false;
    // Cohorts this pass copied, to be asked about once more before publishing.
    let mut copied_cohorts: Vec<&PathBuf> = Vec::new();
    for (shared, cohort) in &cohorts {
        let ids: BTreeSet<&str> = cohort.iter().map(|target| target.id.as_str()).collect();
        let busy = cohort
            .iter()
            .any(|target| busy_roots.contains(&target.cleanup_root));
        if busy {
            for target in cohort {
                ready_rows.remove(&(target.registry, target.row));
            }
            pending.push(shared.to_string_lossy().into_owned());
            continue;
        }
        let live = defer_stores
            || ids.iter().try_fold(false, |live, id| {
                is_running(id).map(|running| live || running)
            })?;
        if live {
            for target in cohort {
                ready_rows.remove(&(target.registry, target.row));
                blocked_roots.insert(target.cleanup_root.clone());
            }
            if announce && !defer_stores {
                progress::notice(format!(
                    "session(s) {} running or unverified; their agent store moves after they stop",
                    ids.iter().copied().collect::<Vec<_>>().join(", ")
                ));
            }
            pending.push(shared.to_string_lossy().into_owned());
            continue;
        }
        for target in cohort {
            // A parked row is here only so the fold above could ask about it.
            // Copying its store is the cost this skip exists to avoid.
            if target.disposition == Disposition::Hold {
                // Held members are asked about by the fold above but publish
                // nothing. Their root is protected by `all_ready`, which
                // requires every member to be `Move` and ready.
                continue;
            }
            let ordinal = copied_targets + 1;
            if !announced && target.disposition == Disposition::Move {
                // Said once per pass, right before the first attempt: this is
                // the part that can take minutes, and the one a user may want
                // to skip. A deferred attempt does not re-announce it.
                announced = true;
                progress::notice(format!(
                    "Isolating agent stores for {} sandboxed session(s): each gets its own copy of the \
                     shared agent store under sandbox-v2/. Large stores take a while. To start without \
                     waiting, quit and run with {DEFER_ENV}=1; finish later with `aoe migrate`.",
                    total_targets
                ));
            }
            progress::step(format!(
                "copying agent store {ordinal}/{total_targets}: {} -> {}",
                shared.display(),
                target.private.display()
            ));
            let excluded = excluded_by_root
                .get(&target.cleanup_root)
                .context("missing cleanup-root exclusions")?;
            if gated_roots.insert(target.cleanup_root.clone()) {
                copy_gate(&target.cleanup_root);
            }
            if !publish_store(
                shared,
                &target.private,
                excluded,
                (shared != &target.cleanup_root).then_some(target.cleanup_root.as_path()),
                shared == &target.cleanup_root,
            )? {
                // The store was not published, so the row stays pending with
                // its whole root rather than counting as moved.
                tracing::warn!(
                    "v027 leaving {} pending: the retention of its source was deferred",
                    target.id
                );
                ready_rows.remove(&(target.registry, target.row));
                continue;
            }
            copied_targets += 1;
        }
        if cohort
            .iter()
            .any(|target| target.disposition == Disposition::Move)
        {
            copied_cohorts.push(shared);
        }
    }

    // Publish under the locks the plan was made under, against re-read
    // registries and a fresh liveness answer: rows may have changed and a
    // container may have come up during the copy.
    if transition_lock.is_none() {
        transition_lock = Some(crate::session::acquire_storage_flock(app_dir, LOCK)?);
        // A profile created during the copy is locked too, since every
        // registry read below is written back.
        let fresh_paths = registry_paths(app_dir)?;
        let mut dirs = registry_dirs_of(&fresh_paths);
        dirs.extend(registry_dirs.iter().cloned());
        dirs.sort();
        dirs.dedup();
        registry_locks = Some(lock_registry_dirs(&dirs)?);
    }
    let _held = (transition_lock, registry_locks, cohort_locks);
    refresh_liveness();
    let mut fresh = load_registries(app_dir)?;
    let fresh_ids_by_root = collect_row_ids_by_root(&fresh, app_dir, home)?;
    for root in &cleanup_roots {
        let planned = row_ids_by_root.get(root);
        let arrived = fresh_ids_by_root
            .get(root)
            .into_iter()
            .flatten()
            .any(|id| !planned.is_some_and(|planned| planned.contains(id)));
        if arrived {
            tracing::warn!(
                "v027 keeping {}: a session started reading it during the copy",
                root.display()
            );
            blocked_roots.insert(root.clone());
        }
    }
    let mut published_rows: BTreeMap<(usize, usize), (usize, usize)> = BTreeMap::new();
    for key in std::mem::take(&mut ready_rows) {
        match locate_planned_row(&registries, &fresh, key) {
            Some(fresh_key) => {
                published_rows.insert(key, fresh_key);
                ready_rows.insert(key);
            }
            None => {
                for target in cohorts
                    .values()
                    .flatten()
                    .filter(|target| (target.registry, target.row) == key)
                {
                    tracing::warn!(
                        "v027 leaving {} pending: its row changed during the copy",
                        target.id
                    );
                    blocked_roots.insert(target.cleanup_root.clone());
                }
            }
        }
    }
    for shared in copied_cohorts {
        let cohort = &cohorts[shared];
        let live = cohort.iter().try_fold(false, |live, target| {
            is_running(&target.id).map(|running| live || running)
        })?;
        if live {
            for target in cohort {
                if ready_rows.remove(&(target.registry, target.row)) {
                    tracing::warn!(
                        "v027 leaving {} pending: its container came up during the copy",
                        target.id
                    );
                }
                blocked_roots.insert(target.cleanup_root.clone());
            }
            pending.push(shared.to_string_lossy().into_owned());
        }
    }

    // Only once every store for the row is durable. `force=false` makes a
    // concurrent start fail the transition rather than stopping a container
    // that came alive after the probe, and a runtime that cannot be asked
    // defers the row. Keyed by id to reap once, but carrying every row naming
    // it, since two profiles can hold one instance.
    let mut ready_ids: BTreeMap<String, BTreeSet<(usize, usize)>> = BTreeMap::new();
    for &(registry, row) in &ready_rows {
        if let Some(id) = registries[registry]
            .value
            .as_array()
            .and_then(|rows| rows.get(row))
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
        {
            ready_ids
                .entry(id.to_owned())
                .or_default()
                .insert((registry, row));
        }
    }
    let mut deferred_rows = BTreeSet::new();
    if !ready_ids.is_empty() {
        progress::step(format!(
            "removing {} stopped sandbox container(s) so they relaunch on the new store",
            ready_ids.len()
        ));
    }
    for (id, keys) in &ready_ids {
        if !reap(id)? {
            deferred_rows.extend(keys.iter().copied());
        }
    }
    for key in deferred_rows {
        ready_rows.remove(&key);
        for target in cohorts
            .values()
            .flatten()
            .filter(|target| (target.registry, target.row) == key)
        {
            blocked_roots.insert(target.cleanup_root.clone());
        }
    }

    for root in &cleanup_roots {
        let related: Vec<&Target> = cohorts
            .values()
            .flatten()
            .filter(|target| &target.cleanup_root == root)
            .collect();
        // Every member under this root must have published, held members
        // included: a held row still reads this source, so retiring it would
        // leave that session with no store to open.
        //
        // The `Move` clause is the mechanism, not a restatement. It is what
        // keeps a root alive for a held member, per root. The pass-wide
        // `defer_source_retirement` below stays for the rows that never
        // produce a target at all (no id, no tool, no agent), whose root
        // cannot be known; routing an ordinary parked row through it instead
        // would block every unrelated root on the machine.
        let all_ready = !related.is_empty()
            && related.iter().all(|target| {
                target.disposition == Disposition::Move
                    && ready_rows.contains(&(target.registry, target.row))
            });
        if !defer_source_retirement && !blocked_roots.contains(root) && all_ready {
            progress::step(format!("retiring shared agent store {}", root.display()));
            if !retire_legacy(root)? {
                // The retention was deferred, so the shared store is still
                // there. A journal path alone carries no retirement authority,
                // and this pass would otherwise stamp the cohort current and
                // never plan this root again: keep the rows pending so a later
                // pass re-derives the root and retries the retirement.
                for target in &related {
                    ready_rows.remove(&(target.registry, target.row));
                }
                pending.push(root.to_string_lossy().into_owned());
            }
        } else {
            pending.push(root.to_string_lossy().into_owned());
        }
    }

    for key in &ready_rows {
        let (registry, row) = published_rows[key];
        if let Some(value) = fresh[registry]
            .value
            .as_array_mut()
            .and_then(|rows| rows.get_mut(row))
        {
            set_generation(
                value,
                crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
            );
        }
    }
    for registry in &mut fresh {
        if let Some(rows) = registry.value.as_array_mut() {
            for row in rows {
                if row.get("sandbox_store_generation").and_then(Value::as_u64)
                    == Some(u64::from(
                        crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
                    ))
                {
                    clear_transition_metadata(row);
                }
            }
        }
    }
    for registry in &fresh {
        let bytes = serde_json::to_vec_pretty(&registry.value)?;
        crate::session::atomic_write(&registry.path, &bytes)?;
        sync_parent(&registry.path)?;
    }

    let done = ready_rows.len();
    // A held-only backlog is announced too: without it `aoe migrate` reports
    // nothing about sessions it deliberately left on the shared store.
    if (!affected_rows.is_empty() || held_row_count > 0) && (announce || done > 0) {
        let left = affected_rows.len().saturating_sub(done);
        progress::notice(match (left, held_row_count) {
            (0, held) if affected_rows.is_empty() => format!(
                "{held} trashed or archived sandboxed session(s) stay on the shared agent store; each moves when it is started, or restore or unarchive it and run `aoe migrate`."
            ),
            (0, 0) => format!("{done} sandboxed session(s) now use private agent stores."),
            (0, held) => format!(
                "{done} sandboxed session(s) now use private agent stores. {held} trashed or archived session(s) stay on the shared agent store; each moves when it is started, or restore or unarchive it and run `aoe migrate`."
            ),
            (left, 0) => format!(
                "{done} sandboxed session(s) moved to private agent stores, {left} still pending; the move resumes on a later start or with `aoe migrate`."
            ),
            (left, held) => format!(
                "{done} sandboxed session(s) moved to private agent stores, {left} still pending and {held} trashed or archived; the move resumes on a later start or with `aoe migrate`."
            ),
        });
    }
    if pending.is_empty() {
        match fs::remove_file(&journal) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing v027 journal"),
        }
        sync_parent(&journal)?;
    } else {
        pending.sort();
        pending.dedup();
        crate::session::atomic_write(&journal, &serde_json::to_vec(&pending)?)?;
        sync_parent(&journal)?;
    }
    Ok(match wait_for {
        Some(root) => PassOutcome::WaitFor(root),
        None => PassOutcome::Done,
    })
}

fn registry_dirs_of(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = paths
        .iter()
        .filter_map(|path| path.parent())
        .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

pub(super) fn lock_registry_dirs(dirs: &[PathBuf]) -> Result<Vec<crate::session::StorageFlock>> {
    dirs.iter()
        .map(|dir| {
            crate::session::acquire_storage_flock(dir, crate::session::STORAGE_LOCK_FILENAME)
        })
        .collect()
}

/// Every legacy source each sandboxed row reads, by canonical root. The plan
/// is made from one snapshot and publication checks another: a root that
/// gained a reader in between is not retired.
fn collect_row_ids_by_root(
    registries: &[Registry],
    app_dir: &Path,
    home: &Path,
) -> Result<BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>>> {
    let mut row_ids_by_root: BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>> = BTreeMap::new();
    for registry in registries {
        let profile = profile_for_registry(app_dir, &registry.path);
        let Some(rows) = registry.value.as_array() else {
            continue;
        };
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        for row in rows {
            if !row
                .pointer("/sandbox_info/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let (Some(id), Some(tool)) = (
                row.get("id").and_then(Value::as_str),
                row.get("tool").and_then(Value::as_str),
            ) else {
                continue;
            };
            if crate::session::validate_instance_id(id).is_err() {
                continue;
            }
            let detect_as = row
                .get("detect_as")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| config.session.agent_detect_as.get(tool).map(String::as_str));
            let Some(agent) = crate::agents::get_agent(tool)
                .or_else(|| detect_as.and_then(crate::agents::get_agent))
            else {
                continue;
            };
            let declared = config.session.agent_config_dir_for(tool, home);
            for (source, _) in
                crate::session::config::container_config::sandbox_store_migration_paths(
                    agent.name,
                    home,
                    declared.as_deref(),
                    id,
                )?
            {
                let root = fs::canonicalize(&source).unwrap_or(source);
                row_ids_by_root
                    .entry(root)
                    .or_default()
                    .insert(std::ffi::OsString::from(id));
            }
        }
    }
    Ok(row_ids_by_root)
}

/// Where the planned row `key` (a registry and row index into `planned`) is
/// in `fresh`, if it is still there with the plan this pass copied under: the
/// same id, the same pending generation and the same transition paths.
/// Looked up by registry path and row id, since a concurrent write may have
/// reordered rows or added a registry.
fn locate_planned_row(
    planned: &[Registry],
    fresh: &[Registry],
    key: (usize, usize),
) -> Option<(usize, usize)> {
    let registry = planned.get(key.0)?;
    let row = registry.value.as_array()?.get(key.1)?;
    let id = row.get("id").and_then(Value::as_str)?;
    let fresh_index = fresh
        .iter()
        .position(|candidate| candidate.path == registry.path)?;
    let fresh_row_index = fresh[fresh_index]
        .value
        .as_array()?
        .iter()
        .position(|candidate| candidate.get("id").and_then(Value::as_str) == Some(id))?;
    let fresh_row = &fresh[fresh_index].value[fresh_row_index];
    let same = |field: &str| row.get(field) == fresh_row.get(field);
    (same("sandbox_store_generation") && same("sandbox_store_transition_paths"))
        .then_some((fresh_index, fresh_row_index))
}

/// The one line a bare start says about pending work. `movable` rows move on
/// their next launch or under `aoe migrate`; `held` ones only once they are
/// launched or brought back, so a held-only backlog says nothing, since
/// nothing the user can do now clears it.
fn pending_work_notice(movable: usize, held: usize) -> Option<String> {
    match (movable, held) {
        (0, _) => None,
        (movable, 0) => Some(format!(
            "{movable} sandboxed session(s) still use the shared agent store; each moves when it is next started, or run `aoe migrate` to move them now."
        )),
        (movable, held) => Some(format!(
            "{movable} sandboxed session(s) still use the shared agent store, plus {held} trashed or archived; each moves when it is next started, or run `aoe migrate` to move the {movable} now."
        )),
    }
}

fn transition_paths(row: &Value) -> Result<Option<Vec<(PathBuf, PathBuf)>>> {
    let Some(value) = row.get("sandbox_store_transition_paths") else {
        return Ok(None);
    };
    let entries = value
        .as_array()
        .context("sandbox_store_transition_paths must be an array")?;
    let paths = entries
        .iter()
        .map(|entry| {
            let source = entry
                .get("source")
                .and_then(Value::as_str)
                .context("transition source must be a path string")?;
            let destination = entry
                .get("destination")
                .and_then(Value::as_str)
                .context("transition destination must be a path string")?;
            if source.is_empty() || destination.is_empty() {
                bail!("transition paths must not be empty");
            }
            Ok((PathBuf::from(source), PathBuf::from(destination)))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(paths))
}

fn same_authorized_path(stored: &Path, fresh: &Path) -> bool {
    stored == fresh
        || matches!(
            (resolve_existing_ancestor(stored), resolve_existing_ancestor(fresh)),
            (Some(stored), Some(fresh)) if stored == fresh
        )
}

fn resolve_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut resolved) = fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        suffix.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
}

fn set_transition_paths(row: &mut Value, plans: &[(PathBuf, PathBuf)]) {
    let entries: Vec<Value> = plans
        .iter()
        .map(|(source, destination)| {
            serde_json::json!({
                "source": source,
                "destination": destination,
            })
        })
        .collect();
    if let Some(object) = row.as_object_mut() {
        object.insert("sandbox_store_transition_paths".to_string(), entries.into());
    }
}

fn clear_transition_metadata(row: &mut Value) -> bool {
    let Some(object) = row.as_object_mut() else {
        return false;
    };
    object.remove("sandbox_store_transition_paths").is_some()
}

fn mark_current(row: &mut Value, generation: u64, dirty: &mut bool) {
    let current = crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION;
    if generation != u64::from(current) {
        set_generation(row, current);
        *dirty = true;
    }
    if clear_transition_metadata(row) {
        *dirty = true;
    }
}

fn set_generation(row: &mut Value, generation: u8) {
    if let Some(object) = row.as_object_mut() {
        object.insert("sandbox_store_generation".to_string(), generation.into());
    }
}

pub(super) fn registry_paths(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let profiles = app_dir.join("profiles");
    match fs::read_dir(&profiles) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path().join("sessions.json");
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", profiles.display())),
    }
    let default = app_dir.join("sessions.json");
    if default.is_file() {
        paths.push(default);
    }
    paths.sort();
    Ok(paths)
}

fn load_registry_paths(paths: Vec<PathBuf>) -> Result<Vec<Registry>> {
    paths
        .into_iter()
        .map(|path| {
            let value = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Registry { path, value })
        })
        .collect()
}

fn load_registries(app_dir: &Path) -> Result<Vec<Registry>> {
    load_registry_paths(registry_paths(app_dir)?)
}

pub(crate) fn profile_for_registry(app_dir: &Path, path: &Path) -> String {
    path.strip_prefix(app_dir.join("profiles"))
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn instance_children(root: &Path) -> Result<BTreeSet<std::ffi::OsString>> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "v027 cleanup root is a symlink or non-directory: {}",
            root.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", root.display())),
    }
    let mut children = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.to_string_lossy();
        if bytes.len() == 16
            && bytes
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            children.insert(name);
        }
    }
    Ok(children)
}

/// Publish one store, answering whether it did. `false` means a live mount
/// kept the quarantine's original where it was, so nothing was published and
/// the caller must leave the target pending for a later pass.
fn publish_store(
    source: &Path,
    destination: &Path,
    excluded_root_children: &BTreeSet<std::ffi::OsString>,
    overlay_shared_root: Option<&Path>,
    exclude_source_children: bool,
) -> Result<bool> {
    let parent = destination
        .parent()
        .context("private store has no parent")?;
    let source_exists = match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
        Ok(_) => bail!(
            "v027 source is a symlink or non-directory: {}",
            source.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()))
        }
    };
    let Some(publication) = Publication::prepare(destination)? else {
        return Ok(false);
    };
    if !source_exists {
        publication.anchored_parent.ensure_dir(Path::new(
            destination
                .file_name()
                .context("private store has no leaf")?,
        ))?;
        fs::File::open(parent)?.sync_all()?;
        return Ok(true);
    }
    let stage = &publication.stage;
    fs::create_dir(stage)?;
    let mut copied = CopyState::default();
    copy_tree_no_links(
        source,
        stage,
        exclude_source_children.then_some(excluded_root_children),
        false,
        false,
        &mut copied,
    )?;
    if let Some(overlay) = overlay_shared_root {
        // Files only. The overlay folds a shared agent home into a session
        // that already has a private one of its own, so what it has to carry
        // is the credentials, config and state files that home kept at its
        // root. Its directories are the shared home's own accumulation:
        // conversation history belonging to other sessions, caches, logs and
        // plugin trees, none of them the single-instance lock this migration
        // exists to unshare, and each replicated once per session (#3819).
        // The full shared original, including these directories, is retained
        // outside managed mounts when its stopped cohort is retired.
        copy_tree_no_links(
            overlay,
            stage,
            Some(excluded_root_children),
            true,
            true,
            &mut copied,
        )?;
    }
    fs::set_permissions(stage, fs::symlink_metadata(source)?.permissions())?;
    sync_tree(stage)?;
    // One barrier for the whole tree instead of a flush per file: it orders
    // every write ahead of the rename, so a crash can lose the publish but
    // cannot expose a store whose bytes never reached the media. The parent
    // sync after the rename is what makes the publish durable.
    super::store_fs::barrier(&fs::File::open(stage)?)?;

    let destination_exists = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            remove_tree_no_links(stage)?;
            bail!("v027 destination is a symlink: {}", destination.display());
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", destination.display()))
        }
    };
    if destination_exists {
        fs::rename(destination, &publication.quarantine)?;
    }
    fs::rename(stage, destination)?;
    fs::File::open(parent)?.sync_all()?;
    let retained = super::v033_isolate_sandbox_content::retain_legacy_original(
        &publication.quarantine,
        parent.parent().context("private layout has no parent")?,
    )?;
    fs::File::open(parent)?.sync_all()?;
    // The displaced destination is still at the quarantine when this defers,
    // so the publish is not finished and the target is retried with it.
    Ok(!matches!(
        retained,
        super::v033_isolate_sandbox_content::Retained::Deferred
    ))
}

/// The staging and quarantine paths one destination publishes through, with
/// whatever an interrupted pass left at them already removed.
///
/// Shared by the copy and the rename so both publish through the same
/// artifacts and both clear the same debris: a killed `aoe migrate` leaves a
/// half-written `.v027-stage-<id>` behind, and the pass that picks the store
/// up again must not build on it or leave it sitting in the private layout.
struct Publication {
    anchored_parent: crate::session::AnchoredDir,
    stage: PathBuf,
    quarantine: PathBuf,
}

impl Publication {
    /// `None` means a live mount keeps the recovery namespace reachable, so
    /// the quarantine still holds an original the destination would have to be
    /// renamed onto. The caller publishes nothing and retries later.
    fn prepare(destination: &Path) -> Result<Option<Self>> {
        let parent = destination
            .parent()
            .context("private store has no parent")?;
        let layout_root = parent.parent().context("private layout has no parent")?;
        fs::create_dir_all(layout_root)?;
        let anchored_parent = crate::session::AnchoredDir::create(parent)?;
        fs::File::open(layout_root)?.sync_all()?;
        let leaf = destination
            .file_name()
            .context("private store has no leaf")?
            .to_string_lossy()
            .into_owned();
        let stage = anchored_parent.path().join(format!(".v027-stage-{leaf}"));
        let quarantine = anchored_parent
            .path()
            .join(format!(".v027-quarantine-{leaf}"));
        // Before the stage, and before anything else the caller would build
        // on: a quarantine the retention cannot move is one the destination
        // cannot be renamed onto either.
        if matches!(
            super::v033_isolate_sandbox_content::retain_legacy_original(&quarantine, layout_root)?,
            super::v033_isolate_sandbox_content::Retained::Deferred
        ) {
            return Ok(None);
        }
        remove_tree_no_links(&stage)?;
        Ok(Some(Self {
            anchored_parent,
            stage,
            quarantine,
        }))
    }
}

/// Move a store that belongs to no session into the private layout.
///
/// No row names an unregistered legacy child, so folding the shared agent
/// home into it would buy a dead session a private copy of live sessions'
/// history: one interrupted `aoe migrate` wrote 16 GB across 63 of them
/// (#3819). A rename preserves it exactly for one syscall.
///
/// The publish protocol is [`publish_store`]'s: stale staging is cleared
/// first, an existing destination is quarantined, and the parent is synced
/// around the rename, so a crash leaves either the old destination or the new
/// one. A rename that cannot reach the destination, across filesystems or
/// otherwise, falls back to the copy.
///
/// `false` is [`publish_store`]'s deferral: the store is not where the plan
/// wanted it, so the caller leaves its root and rows pending.
fn relocate_store(source: &Path, destination: &Path) -> Result<bool> {
    let parent = destination
        .parent()
        .context("private store has no parent")?;
    let Some(publication) = Publication::prepare(destination)? else {
        return Ok(false);
    };
    let mut quarantined = false;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("v027 destination is a symlink: {}", destination.display())
        }
        Ok(_) => {
            fs::rename(destination, &publication.quarantine)?;
            quarantined = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", destination.display()))
        }
    }
    if let Err(error) = fs::rename(source, destination) {
        tracing::warn!(
            "v027 copying {} instead of moving it: {error}",
            source.display()
        );
        // The copy stages before it publishes, so put the destination back
        // first: falling back with it still quarantined would leave nothing
        // published for the length of the copy.
        if quarantined {
            fs::rename(&publication.quarantine, destination)?;
        }
        return publish_store(source, destination, &BTreeSet::new(), None, false);
    }
    fs::File::open(parent)?.sync_all()?;
    let retained = super::v033_isolate_sandbox_content::retain_legacy_original(
        &publication.quarantine,
        parent.parent().context("private layout has no parent")?,
    )?;
    fs::File::open(parent)?.sync_all()?;
    Ok(!matches!(
        retained,
        super::v033_isolate_sandbox_content::Retained::Deferred
    ))
}

#[cfg(unix)]
fn copy_tree_no_links(
    source: &Path,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    overwrite_newer: bool,
    files_only: bool,
    copied: &mut CopyState,
) -> Result<()> {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::Mode;
    let fd = open(
        source,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
        Mode::empty(),
    )?;
    copy_tree_from_fd(
        fd,
        destination,
        excluded_children,
        Path::new(""),
        overwrite_newer,
        files_only,
        copied,
    )
}

#[cfg(unix)]
fn copy_tree_from_fd(
    fd: std::os::fd::OwnedFd,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    relative: &Path,
    overwrite_newer: bool,
    files_only: bool,
    copied: &mut CopyState,
) -> Result<()> {
    use nix::dir::Dir;
    use nix::fcntl::{openat, readlinkat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, futimens, Mode};
    use nix::sys::time::TimeSpec;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let mut dir = Dir::from_fd(fd)?;
    let names: Result<Vec<std::ffi::OsString>> = dir
        .iter()
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(std::ffi::OsStr::from_bytes(
                entry.file_name().to_bytes(),
            )
            .to_owned())),
            Err(error) => Some(Err(error.into())),
        })
        .collect();
    for name in names? {
        let name = name.as_os_str();
        if excluded_children.is_some_and(|excluded| excluded.contains(name)) {
            continue;
        }
        let stat = fstatat(&dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
        let kind = stat.st_mode & nix::libc::S_IFMT;
        // A symlink at a shared root usually points into one of the
        // directories below, so carrying it would publish a dangler.
        if files_only && kind != nix::libc::S_IFREG {
            continue;
        }
        let target = destination.join(name);
        if kind == nix::libc::S_IFLNK {
            let link = readlinkat(&dir, name)?;
            if !relative_symlink_stays_in_root(relative, Path::new(&link)) {
                tracing::warn!(
                    "v027 skipping source symlink that escapes its sandbox root: {}",
                    relative.join(name).display()
                );
                continue;
            }
            match fs::symlink_metadata(&target) {
                Ok(_) if overwrite_newer && source_stat_is_newer(&stat, &target)? => {
                    remove_tree_no_links(&target)?;
                }
                Ok(_) if overwrite_newer => continue,
                Ok(_) => bail!("v027 copy destination already exists: {}", target.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            std::os::unix::fs::symlink(&link, &target)?;
            #[cfg(not(target_os = "redox"))]
            {
                let target_dir = fs::File::open(destination)?;
                nix::sys::stat::utimensat(
                    &target_dir,
                    name,
                    &TimeSpec::new(stat.st_atime, stat.st_atime_nsec),
                    &TimeSpec::new(stat.st_mtime, stat.st_mtime_nsec),
                    nix::sys::stat::UtimensatFlags::NoFollowSymlink,
                )?;
            }
            continue;
        }
        if kind == nix::libc::S_IFDIR {
            let child = openat(
                &dir,
                name,
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )?;
            let existed = match fs::symlink_metadata(&target) {
                Ok(metadata)
                    if overwrite_newer
                        && metadata.is_dir()
                        && !metadata.file_type().is_symlink() =>
                {
                    true
                }
                Ok(_) => bail!(
                    "v027 copy destination has conflicting type: {}",
                    target.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&target)?;
                    false
                }
                Err(error) => return Err(error.into()),
            };
            copy_tree_from_fd(
                child,
                &target,
                None,
                &relative.join(name),
                overwrite_newer,
                false,
                copied,
            )?;
            if !existed || source_stat_is_newer(&stat, &target)? {
                // `st_mode` is u32 on Linux and u16 on Darwin, so the cast is
                // a no-op on one and a widening on the other.
                fs::set_permissions(&target, fs::Permissions::from_mode(stat.st_mode as u32))?;
            }
        } else if kind == nix::libc::S_IFREG {
            let file = openat(
                &dir,
                name,
                OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY | OFlag::O_NONBLOCK,
                Mode::empty(),
            )?;
            let opened = fstat(&file)?;
            if (opened.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFREG {
                bail!("v027 source entry changed type during copy");
            }
            let mut input = fs::File::from(file);
            let target_exists = match fs::symlink_metadata(&target) {
                Ok(metadata)
                    if overwrite_newer
                        && metadata.is_file()
                        && !metadata.file_type().is_symlink() =>
                {
                    if !source_stat_is_newer(&opened, &target)? {
                        continue;
                    }
                    true
                }
                Ok(_) => bail!(
                    "v027 copy destination has conflicting type: {}",
                    target.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            // A clone has to create its own destination, so an entry being
            // replaced is unlinked rather than truncated and both paths take
            // the same `create_new` route.
            if target_exists {
                fs::remove_file(&target)?;
            }
            let output = match copied.clone.clone_file(&input, &opened, &target) {
                Some(output) => {
                    copied.copied_file(opened.st_size.max(0) as u64);
                    output
                }
                None => {
                    let mut output = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)?;
                    let bytes = std::io::copy(&mut input, &mut output)?;
                    copied.copied_file(bytes);
                    output.set_permissions(fs::Permissions::from_mode(opened.st_mode as u32))?;
                    futimens(
                        &output,
                        &TimeSpec::new(opened.st_atime, opened.st_atime_nsec),
                        &TimeSpec::new(opened.st_mtime, opened.st_mtime_nsec),
                    )?;
                    output
                }
            };
            super::store_fs::sync_to_drive(&output)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn source_stat_is_newer(stat: &nix::libc::stat, target: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let target = fs::symlink_metadata(target)?;
    Ok((stat.st_mtime, stat.st_mtime_nsec) > (target.mtime(), target.mtime_nsec()))
}

#[cfg(unix)]
fn relative_symlink_stays_in_root(parent: &Path, link: &Path) -> bool {
    use std::path::Component;
    if link.is_absolute() {
        return false;
    }
    let mut depth = parent.components().count();
    for component in link.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => return false,
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(not(unix))]
fn copy_tree_no_links(
    source: &Path,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    overwrite_newer: bool,
    files_only: bool,
    copied: &mut CopyState,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if excluded_children.is_some_and(|excluded| excluded.contains(&entry.file_name())) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "v027 cannot safely copy source symlink on this platform: {}",
                entry.path().display()
            );
        }
        if files_only && !metadata.is_file() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            match fs::create_dir(&target) {
                Ok(()) => {}
                Err(error)
                    if overwrite_newer && error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            copy_tree_no_links(&entry.path(), &target, None, overwrite_newer, false, copied)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_file() {
            let should_copy = match fs::symlink_metadata(&target) {
                Ok(existing) if overwrite_newer && existing.is_file() => {
                    metadata.modified()? > existing.modified()?
                }
                // Conflicting types are not evidence that the required
                // configuration reached the destination. Fail closed.
                Ok(_) => bail!(
                    "v027 copy destination has conflicting type: {}",
                    target.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(error.into()),
            };
            if should_copy {
                copied.copied_file(fs::copy(entry.path(), &target)?);
                fs::set_permissions(&target, metadata.permissions())?;
                super::store_fs::sync_to_drive(&fs::File::open(&target)?)?;
            }
        }
    }
    Ok(())
}

/// Push every directory of the staged tree to the drive. Its regular files
/// were pushed as they were created; this covers the directory entries that
/// name them. What makes the whole tree durable is the barrier
/// [`publish_store`] issues afterwards, not these calls.
fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            sync_tree(&entry.path())?;
        }
    }
    super::store_fs::sync_to_drive(&fs::File::open(path)?)?;
    Ok(())
}

fn retire_legacy(source: &Path) -> Result<bool> {
    let parent = source.parent().context("legacy store has no parent")?;
    let quarantine = parent.join(format!(
        ".{}.v027-quarantine",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));
    let host = if parent.file_name().is_some_and(|name| name == "sandbox") {
        parent
            .parent()
            .context("legacy layout has no native home")?
    } else {
        parent
    };
    // A killed old migration may already have renamed the original aside.
    // Preserve that whole original too; never clear a recovery candidate. A
    // mount that reaches the recovery namespace defers both, so the caller
    // leaves the root pending and a later pass retires it.
    let mut deferred = false;
    for candidate in [&quarantine, source] {
        if matches!(
            super::v033_isolate_sandbox_content::retain_legacy_original(candidate, host)?,
            super::v033_isolate_sandbox_content::Retained::Deferred
        ) {
            deferred = true;
        }
    }
    if deferred {
        return Ok(false);
    }
    if parent.file_name().is_some_and(|name| name == "sandbox") {
        let _ = fs::remove_dir(parent);
        if let Some(grandparent) = parent.parent() {
            let _ = fs::File::open(grandparent).and_then(|dir| dir.sync_all());
        }
    }
    Ok(true)
}

fn remove_tree_no_links(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o700);
            fs::set_permissions(path, permissions)?;
        }
        #[cfg(not(unix))]
        {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
        for entry in fs::read_dir(path)? {
            remove_tree_no_links(&entry?.path())?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// [`super::run_in`] with every container reported reaped, which is what
    /// each case below assumes unless it drives the probe itself. Shadowing
    /// the glob import keeps these tests hermetic: they assert the migration's
    /// own logic and must not depend on a container runtime being installed.
    fn run_in(app_dir: &Path, home: &Path, is_running: &RunningProbe<'_>) -> Result<()> {
        super::run_in(app_dir, home, is_running, &|_| Ok(true), false, true, None)
    }

    fn run_in_only(
        app_dir: &Path,
        home: &Path,
        is_running: &RunningProbe<'_>,
        only: &str,
    ) -> Result<()> {
        super::run_in(
            app_dir,
            home,
            is_running,
            &|_| Ok(true),
            false,
            true,
            Some(only),
        )
    }

    fn row(id: &str) -> String {
        format!(r#"{{"id":"{id}","tool":"gemini","sandbox_info":{{"enabled":true}}}}"#)
    }

    /// An isolated app dir and `HOME` for one pass. Bind the tempdir before
    /// the guard: the guard has to restore `HOME` before the directory it
    /// points at is removed.
    fn isolated() -> (
        tempfile::TempDir,
        crate::session::test_support::AppDirGuard,
        PathBuf,
        PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        (temp, guard, app, home)
    }

    /// The persisted session rows.
    fn read_rows(app: &Path) -> Value {
        serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap()
    }

    fn write_rows(app: &Path, rows: &Value) {
        fs::write(app.join("sessions.json"), serde_json::to_vec(rows).unwrap()).unwrap();
    }

    fn pin_agent_dir(app: &Path, root: &Path) {
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                root.display()
            ),
        )
        .unwrap();
    }

    fn seed_store(parent: &Path, rel: &str, data: &[u8]) -> PathBuf {
        let root = parent.join(rel);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data"), data).unwrap();
        root
    }

    /// Every runtime error that means "could not answer" must be classified
    /// as such. `InspectFailed` is the catch-all `classify_probe_failure`
    /// returns for an unrecognised stderr, which is what a timed-out probe on
    /// a loaded daemon produces, so leaving it out aborts startup on exactly
    /// the transient failure this is meant to survive.
    #[test]
    fn every_unanswerable_runtime_error_defers_rather_than_aborting() {
        use crate::containers::error::DockerError;
        for error in [
            DockerError::NotInstalled,
            DockerError::DaemonNotRunning,
            DockerError::PermissionDenied,
            DockerError::InspectFailed("context deadline exceeded".to_string()),
        ] {
            assert!(
                runtime_cannot_answer(&error),
                "{error} must defer the row rather than fail the migration"
            );
        }
        // A local fault is a real failure and must still surface.
        assert!(!runtime_cannot_answer(&DockerError::IoError(
            std::io::Error::other("disk")
        )));
        // A refused removal is the deliberate `force=false` abort.
        assert!(!runtime_cannot_answer(&DockerError::RemoveFailed(
            "container is running".to_string()
        )));
    }

    /// The batch listing decides only where inspect would agree with it:
    /// paused and restarting are live, exited and created are stopped, and
    /// everything else (a transitional state, a container it did not list)
    /// goes to the per-row inspect, which answers fail-closed.
    #[test]
    fn batched_probe_keeps_live_semantics_and_inspects_the_rest() {
        use crate::containers::{ContainerState, DockerContainer};
        let listing: std::collections::HashMap<String, ContainerState> = [
            ("running", ContainerState::Running),
            ("paused", ContainerState::Paused),
            ("restarting", ContainerState::Restarting),
            ("exited", ContainerState::Exited),
            ("created", ContainerState::Created),
            ("removing", ContainerState::Other),
        ]
        .into_iter()
        .map(|(id, state)| (DockerContainer::generate_name(id), state))
        .collect();
        let batches = std::cell::Cell::new(0);
        let inspected = std::cell::RefCell::new(Vec::new());
        let probe = batched_running_probe_with(
            || {
                batches.set(batches.get() + 1);
                listing.clone()
            },
            |id| {
                inspected.borrow_mut().push(id.to_string());
                // Inspect stands in for an unreachable runtime: unknown reads live.
                Ok((true, true))
            },
            false,
        );
        let cases = [
            ("running", true),
            ("paused", true),
            ("restarting", true),
            ("exited", false),
            ("created", false),
            ("removing", true),
            ("missing", true),
        ];
        for (id, live) in cases {
            assert_eq!(probe(id).unwrap(), live, "{id}");
        }
        assert_eq!(batches.get(), 1, "one listing per pass");
        assert_eq!(*inspected.borrow(), ["removing", "missing"]);
    }

    /// A parked row keeps its shared store, and the journal keeps naming the
    /// legacy root it holds, for as long as it stays archived. Neither says a
    /// private store is being written, so neither may gate `aoe sandbox
    /// reclaim`, which would otherwise refuse on such a machine forever.
    #[test]
    fn only_a_published_transition_counts_as_in_flight() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"archived_at":"2026-01-01T00:00:00Z"}]"#,
        )
        .unwrap();
        fs::write(app.join(JOURNAL), br#"["/home/u/.claude/sandbox"]"#).unwrap();

        assert!(transition_may_be_pending(&app, false).unwrap());
        assert!(
            !transition_may_be_pending(&app, true).unwrap(),
            "a bare start has nothing to do for a parked row and a journal"
        );
        assert!(!transition_in_flight(&app).unwrap());

        // Planned by an earlier pass, then archived.
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"archived_at":"2026-01-01T00:00:00Z","sandbox_store_generation":1,"sandbox_store_transition_paths":[{"source":"/a","destination":"/b"}]}]"#,
        )
        .unwrap();
        assert!(transition_may_be_pending(&app, false).unwrap());
        assert!(!transition_may_be_pending(&app, true).unwrap());

        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();
        assert!(transition_may_be_pending(&app, true).unwrap());

        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"sandbox_store_transition_paths":[{"source":"/a","destination":"/b"}]}]"#,
        )
        .unwrap();

        assert!(transition_may_be_pending(&app, true).unwrap());
        assert!(transition_in_flight(&app).unwrap());

        // Metadata the migration cannot parse is state it cannot validate, so
        // it fails rather than reading as "no transition" and letting a
        // reclaim run against a move it cannot see.
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"sandbox_store_transition_paths":5}]"#,
        )
        .unwrap();

        assert!(transition_in_flight(&app).is_err());
    }

    /// A machine whose container runtime is absent or unreachable must still
    /// complete startup. The row stays pending and its legacy source survives,
    /// so the pass that can reach the runtime finishes the transition.
    #[test]
    #[serial_test::serial]
    fn unreachable_container_runtime_defers_instead_of_failing() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        // The answers the production probes give when the runtime is
        // unreachable: liveness cannot be disproved, and nothing is reaped.
        super::run_in(
            &app,
            &home,
            &|_| Ok(true),
            &|_| Ok(false),
            false,
            true,
            None,
        )
        .unwrap();
        let deferred: Value = read_rows(&app);
        assert_eq!(
            deferred[0]["sandbox_store_generation"], 1,
            "an unreaped row must not commit the current generation"
        );
        assert!(
            home.join(".gemini/sandbox").is_dir(),
            "the legacy source must survive a deferred reap"
        );
        assert!(transition_may_be_pending(&app, false).unwrap());

        super::run_in(
            &app,
            &home,
            &|_| Ok(false),
            &|_| Ok(true),
            false,
            true,
            None,
        )
        .unwrap();
        let committed: Value = read_rows(&app);
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!home.join(".gemini/sandbox").exists());
        assert!(!transition_may_be_pending(&app, false).unwrap());
    }

    /// `AOE_DEFER_SANDBOX_MIGRATION` must behave exactly like a live cohort: no
    /// copy, no reap, the row pending, and a later undeferred pass finishing
    /// the move. It also has to say so, since a silent deferral would look
    /// like a migration that did nothing.
    #[test]
    #[serial_test::serial]
    fn deferral_leaves_stores_pending_and_reports_it() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let guard = progress::install(Some(std::sync::Arc::new(move |event| {
            sink.lock().unwrap().push(event)
        })));
        let probed = std::cell::Cell::new(false);
        super::run_in(
            &app,
            &home,
            &|_| {
                probed.set(true);
                Ok(false)
            },
            &|_| panic!("a deferred pass must not reap containers"),
            true,
            true,
            None,
        )
        .unwrap();
        drop(guard);
        assert!(!probed.get(), "deferral skips the container probe");
        let pending: Value = read_rows(&app);
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
        assert!(!home.join(".gemini/sandbox-v2").exists());
        assert!(home.join(".gemini/sandbox").is_dir());
        assert!(app.join(JOURNAL).is_file());
        let notices: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                progress::Event::Notice(line) => Some(line.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|line| line.contains(DEFER_ENV) && line.contains("1 sandboxed session")),
            "deferral is announced: {notices:?}"
        );
        assert!(
            notices.iter().any(|line| line.contains("1 still pending")),
            "summary counts the pending row: {notices:?}"
        );

        run_in(&app, &home, &|_| Ok(false)).unwrap();
        let committed: Value = read_rows(&app);
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!app.join(JOURNAL).exists());
    }

    /// The documented recipe end to end: `AOE_DEFER_SANDBOX_MIGRATION=1` on a
    /// pre-v27 install commits the schema version (so the next start reaches
    /// the reconcile path) while the store stays put and the row stays pending.
    #[test]
    #[serial_test::serial]
    fn deferring_through_the_runner_advances_the_schema_and_keeps_the_store() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();
        fs::write(app.join(".schema_version"), b"26").unwrap();

        assert!(!defer_requested_by(None));
        assert!(!defer_requested_by(Some(std::ffi::OsStr::new(""))));
        assert!(defer_requested_by(Some(std::ffi::OsStr::new("1"))));

        let _defer = crate::session::test_support::EnvGuard::set(&[(DEFER_ENV, "1")]);
        let result = super::super::run_migrations_announced(None);
        result.unwrap();

        // The runner commits the build's target version, not v27 in
        // particular: a later migration in the chain must not fail this test.
        assert_eq!(
            fs::read_to_string(app.join(".schema_version"))
                .unwrap()
                .trim(),
            super::super::CURRENT_VERSION.to_string()
        );
        assert!(!super::super::has_pending_migrations());
        assert!(transition_may_be_pending(&app, false).unwrap());
        assert!(home.join(".gemini/sandbox").is_dir());
        assert!(!home.join(".gemini/sandbox-v2").exists());
        let pending: Value = read_rows(&app);
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
    }

    /// A bare start defers every copy, so its only output is the count of
    /// what it left pending: movable rows, with held (trashed or archived)
    /// rows counted beside them. A held-only backlog and a machine with
    /// nothing pending both say nothing.
    #[test]
    #[serial_test::serial]
    fn a_bare_start_reports_pending_rows_without_copying() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        let movable = |id: &str| serde_json::json!({"id": id, "tool": "gemini", "sandbox_info": {"enabled": true}});
        let held = |id: &str| {
            serde_json::json!({"id": id, "tool": "gemini", "sandbox_info": {"enabled": true},
                "archived_at": "2026-09-05T00:00:00Z"})
        };
        let current = serde_json::json!({"id": "cccccccccccccccc", "tool": "gemini",
            "sandbox_info": {"enabled": true}, "sandbox_store_generation": 2});
        let cases: [(&str, Vec<Value>, Option<&str>); 4] = [
            ("movable only", vec![movable("aaaaaaaaaaaaaaaa"), movable("bbbbbbbbbbbbbbbb")],
                Some("2 sandboxed session(s) still use the shared agent store; each moves")),
            ("held only", vec![held("aaaaaaaaaaaaaaaa")], None),
            ("mixed", vec![movable("aaaaaaaaaaaaaaaa"), held("bbbbbbbbbbbbbbbb"), current.clone()],
                Some("1 sandboxed session(s) still use the shared agent store, plus 1 trashed or archived")),
            ("nothing pending", vec![current.clone()], None),
        ];
        for (name, rows, expected) in cases {
            let _ = fs::remove_file(app.join(JOURNAL));
            fs::write(
                app.join("sessions.json"),
                serde_json::to_vec(&rows).unwrap(),
            )
            .unwrap();
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = events.clone();
            let guard = progress::install(Some(std::sync::Arc::new(move |event| {
                sink.lock().unwrap().push(event)
            })));
            super::run_in(
                &app,
                &home,
                &|_| panic!("a bare start must not probe containers"),
                &|_| panic!("a bare start must not reap containers"),
                true,
                false,
                None,
            )
            .unwrap();
            drop(guard);
            let notices: Vec<String> = events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    progress::Event::Notice(line) => Some(line.clone()),
                    _ => None,
                })
                .collect();
            match expected {
                Some(expected) => {
                    assert_eq!(notices.len(), 1, "{name}: {notices:?}");
                    assert!(notices[0].contains(expected), "{name}: {notices:?}");
                    assert!(notices[0].contains("aoe migrate"), "{name}: {notices:?}");
                }
                None => assert!(notices.is_empty(), "{name}: {notices:?}"),
            }
            assert!(
                !home.join(".gemini/sandbox-v2").exists(),
                "{name}: a bare start must not copy"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn publishes_only_after_quiescence_and_removes_transition_artifacts() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        let pending: Value = read_rows(&app);
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
        assert!(app.join(JOURNAL).is_file());
        assert!(home.join(".gemini/sandbox").is_dir());

        run_in(&app, &home, &|_| Ok(false)).unwrap();
        let committed: Value = read_rows(&app);
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!home.join(".gemini/sandbox").exists());
        assert!(!app.join(JOURNAL).exists());
        assert!(committed[0].get("sandbox_store_transition_paths").is_none());
        assert!(fs::read_dir(home.join(".gemini/sandbox-v2"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".v027")));
    }

    #[test]
    #[serial_test::serial]
    fn pending_cohort_refuses_destination_drift_before_writing_it() {
        let (temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        let source = home.join(".gemini/sandbox");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("conversation.json"), b"original").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        pin_agent_dir(&app, &temp.path().join("changed-gemini"));

        let changed_destination = temp.path().join("changed-gemini/sandbox-v2/one");
        let error = run_in(&app, &home, &|_| Ok(false)).unwrap_err();
        assert!(error
            .to_string()
            .contains("restore the previous session.agent_config_dir"));
        assert!(!changed_destination.exists());
        assert_eq!(
            fs::read(home.join(".gemini/sandbox/conversation.json")).unwrap(),
            b"original"
        );
    }

    #[test]
    #[serial_test::serial]
    fn destination_drift_after_publication_keeps_the_checkpointed_store() {
        let (temp, _app_guard, app, home) = isolated();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::create_dir_all(custom_a.join("sandbox/one")).unwrap();
        fs::write(custom_a.join("sandbox/one/data"), b"source").unwrap();
        pin_agent_dir(&app, &custom_a);
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();
        run_in(&app, &home, &|_| Ok(true)).unwrap();
        fs::create_dir_all(custom_a.join("sandbox-v2/one")).unwrap();
        fs::write(custom_a.join("sandbox-v2/one/data"), b"published").unwrap();
        pin_agent_dir(&app, &custom_b);

        let error = run_in(&app, &home, &|_| Ok(false)).unwrap_err();

        assert!(error
            .to_string()
            .contains("restore the previous session.agent_config_dir"));
        assert!(!custom_b.join("sandbox-v2/one").exists());
        assert_eq!(
            fs::read(custom_a.join("sandbox-v2/one/data")).unwrap(),
            b"published"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pending_absent_custom_source_still_refuses_plan_drift() {
        let (temp, _app_guard, app, home) = isolated();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                custom_a.display()
            ),
        )
        .unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        let checkpoint: Value = read_rows(&app);
        assert_eq!(
            checkpoint[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            1
        );
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                custom_b.display()
            ),
        )
        .unwrap();
        let error = run_in(&app, &home, &|_| Ok(false)).unwrap_err();

        assert!(error
            .to_string()
            .contains("restore the previous session.agent_config_dir"));
        assert!(!custom_b.join("sandbox-v2/one").exists());
        let checkpoint: Value = read_rows(&app);
        assert_eq!(
            checkpoint[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            1
        );
        assert!(checkpoint[0]
            .get("sandbox_store_transition_paths")
            .is_some());
    }

    #[test]
    #[serial_test::serial]
    fn pending_present_custom_source_fails_closed_after_path_change() {
        let (temp, _app_guard, app, home) = isolated();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::create_dir_all(custom_a.join("sandbox/one")).unwrap();
        fs::write(custom_a.join("sandbox/one/data"), b"data").unwrap();
        fs::write(
            app.join("config.toml"),
            format!(
                r#"[session.agent_config_dir]
gemini = "{}"
"#,
                custom_a.display()
            ),
        )
        .unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        let checkpoint: Value = read_rows(&app);
        assert_eq!(checkpoint[0]["sandbox_store_generation"], 1);
        assert_eq!(
            checkpoint[0]["sandbox_store_transition_paths"][0]["source"],
            serde_json::json!(fs::canonicalize(custom_a.join("sandbox")).unwrap())
        );
        fs::write(
            app.join("config.toml"),
            format!(
                r#"[session.agent_config_dir]
gemini = "{}"
"#,
                custom_b.display()
            ),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap_err();
        assert_eq!(
            fs::read(custom_a.join("sandbox/one/data")).unwrap(),
            b"data"
        );
        assert!(!custom_b.join("sandbox-v2/one").exists());
        let pending: Value = read_rows(&app);
        assert_eq!(
            pending[0]["sandbox_store_generation"],
            checkpoint[0]["sandbox_store_generation"]
        );
        assert_eq!(
            pending[0]["sandbox_store_transition_paths"],
            checkpoint[0]["sandbox_store_transition_paths"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn codex_generation_only_fast_path_moves_its_existing_private_store() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        let source = home.join(".codex/sandbox/codex-one");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("auth.json"), b"secret").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"codex-one","tool":"codex","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".codex/sandbox-v2/codex-one/auth.json")).unwrap(),
            b"secret"
        );
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
    }

    #[test]
    #[serial_test::serial]
    fn recovers_publication_before_registry_commit() {
        let (_temp, _app_guard, app, home) = isolated();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("legacy"), b"legacy").unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("published"), b"published").unwrap();
        let quarantine = destination.parent().unwrap().join(".v027-quarantine-one");
        let stage = destination.parent().unwrap().join(".v027-stage-one");
        fs::create_dir_all(&quarantine).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(quarantine.join("secret"), b"secret").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert!(!destination.join("published").exists());
        assert_eq!(fs::read(destination.join("legacy")).unwrap(), b"legacy");
        assert!(!source.exists());
        assert!(!quarantine.exists());
        assert!(!stage.exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn recovers_legacy_quarantine_before_generation_commit() {
        let (_temp, _app_guard, app, home) = isolated();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        let legacy_quarantine = home.join(".gemini/.sandbox.v027-quarantine");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("published"), b"published").unwrap();
        fs::create_dir_all(&legacy_quarantine).unwrap();
        fs::write(legacy_quarantine.join("legacy"), b"legacy").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(destination.join("published")).unwrap(),
            b"published"
        );
        assert!(!source.exists());
        assert!(!legacy_quarantine.exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn missing_source_still_cleans_publication_artifacts() {
        let (_temp, _app_guard, app, home) = isolated();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent.join(".v027-stage-one")).unwrap();
        fs::create_dir_all(parent.join(".v027-quarantine-one")).unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert!(destination.is_dir());
        assert!(!parent.join(".v027-stage-one").exists());
        assert!(!parent.join(".v027-quarantine-one").exists());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn rejects_untrusted_persisted_transition_paths() {
        let (_temp, _app_guard, app, home) = isolated();
        let victim = home.join("documents/sandbox");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": victim,
                "destination": home.join(".gemini/sandbox-v2/one")
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        let error = run_in(&app, &home, &|_| Ok(false)).unwrap_err();

        assert!(error
            .to_string()
            .contains("outside the expected sandbox roots"));
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
    }

    #[test]
    #[serial_test::serial]
    fn current_rows_scrub_forged_transition_metadata_without_io() {
        let (_temp, _app_guard, app, home) = isolated();
        let victim = home.join("current-generation-victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 2,
            "sandbox_store_transition_paths": [{
                "source": victim,
                "destination": home.join(".gemini/sandbox-v2/one")
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
        assert!(!home.join(".gemini/sandbox-v2/one").exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value = read_rows(&app);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn ignores_unprovenanced_journal_paths() {
        let (_temp, _app_guard, app, home) = isolated();
        let victim = home.join("journal-victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        fs::write(app.join("sessions.json"), b"[]").unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
        assert!(!app.join(JOURNAL).exists());
    }

    /// A trashed or archived row must not have its store copied, and must not
    /// let the shared source be retired: a restore would otherwise open a
    /// session whose store had been moved out from under it.
    #[test]
    #[serial_test::serial]
    fn parked_rows_are_not_copied_and_hold_the_shared_source() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = seed_store(&home, ".gemini/sandbox", b"data");
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true},
             "trashed_at":"2026-09-05T00:00:00Z"},
            {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true},
             "archived_at":"2026-09-05T00:00:00Z"},
            {"id":"3333333333333333","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        write_rows(&app, &rows);

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let rows: Value = read_rows(&app);
        assert!(
            rows[0].get("sandbox_store_generation").is_none(),
            "trashed row moved"
        );
        assert!(
            rows[1].get("sandbox_store_generation").is_none(),
            "archived row moved"
        );
        assert_eq!(rows[2]["sandbox_store_generation"], 2);
        for id in ["1111111111111111", "2222222222222222"] {
            assert!(
                !home.join(".gemini/sandbox-v2").join(id).exists(),
                "parked row {id} must not get a private store"
            );
        }
        assert!(
            legacy.exists(),
            "a parked row must protect the shared source it still reads"
        );

        // Only the parked rows are left: `aoe migrate` must still say why.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let guard = progress::install(Some(std::sync::Arc::new(move |event| {
            sink.lock().unwrap().push(event)
        })));
        run_in(&app, &home, &|_| Ok(false)).unwrap();
        drop(guard);
        assert!(
            events.lock().unwrap().iter().any(|event| matches!(
                event,
                progress::Event::Notice(line) if line.starts_with("2 trashed or archived")
            )),
            "{:?}",
            events.lock().unwrap()
        );
        assert_eq!(sessions_on_shared_store().unwrap(), 2);
    }

    /// A parked row holds only its own root; unrelated roots can retire.
    #[test]
    #[serial_test::serial]
    fn an_unrelated_parked_row_does_not_hold_a_ready_root() {
        let (_temp, _app_guard, app, home) = isolated();
        let gemini = seed_store(&home, ".gemini/sandbox", b"g");
        let claude = seed_store(&home, ".claude/sandbox", b"c");
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"3333333333333333","tool":"claude","sandbox_info":{"enabled":true},
             "archived_at":"2026-09-05T00:00:00Z"}
        ]);
        write_rows(&app, &rows);

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
            b"g"
        );
        assert!(
            !gemini.exists(),
            "the gemini root had no held member and must be retired"
        );
        assert!(
            claude.exists(),
            "the claude root carries a parked member and must survive"
        );
    }

    /// The scoped counterpart: a launch that empties its own cohort retires
    /// that root, while the cohort it held keeps its own.
    #[test]
    #[serial_test::serial]
    fn a_scoped_pass_retires_the_root_it_emptied() {
        let (_temp, _app_guard, app, home) = isolated();
        let gemini = home.join(".gemini/sandbox");
        fs::create_dir_all(&gemini).unwrap();
        fs::write(gemini.join("data"), b"g").unwrap();
        let claude = home.join(".claude/sandbox");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("data"), b"c").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"3333333333333333","tool":"claude","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();

        assert!(
            !gemini.exists(),
            "the scoped cohort moved in full and its root must be retired"
        );
        assert!(
            claude.exists(),
            "the cohort this pass held must keep its source"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_restored_row_migrates_on_its_next_pass() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true},
             "trashed_at":"2026-09-05T00:00:00Z"}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();
        run_in(&app, &home, &|_| Ok(false)).unwrap();
        assert!(legacy.exists());

        let restored = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&restored).unwrap(),
        )
        .unwrap();
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
            b"data"
        );
    }

    /// The regression the cohort scoping exists for: a scoped pass must still
    /// ask about every session sharing the store, not just the one being
    /// started. Scoping by row instead drops the peers from the cohort, the
    /// liveness fold then only sees the named session, and the store is copied
    /// while a live peer is still writing to it. The copy becomes
    /// authoritative at generation 2, so the loss is silent.
    #[test]
    #[serial_test::serial]
    fn a_scoped_pass_refuses_a_store_a_live_peer_is_writing() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        // Start 1111 while its cohort peer 2222 is live.
        run_in_only(
            &app,
            &home,
            &|id| Ok(id == "2222222222222222"),
            "1111111111111111",
        )
        .unwrap();

        assert!(
            !home.join(".gemini/sandbox-v2/1111111111111111").exists(),
            "a live cohort peer must block the scoped copy"
        );
        let rows: Value = read_rows(&app);
        assert_ne!(
            rows[0]["sandbox_store_generation"], 2,
            "a blocked row must not be stamped current"
        );
        assert!(legacy.exists(), "the shared source must survive");

        // Once the peer stops, the same scoped pass moves it.
        run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
            b"data"
        );
    }

    /// A scoped pass moves the named session's whole cohort, since the cohort
    /// is the unit the liveness gate reasons about, and leaves every other
    /// agent's cohort alone. That is what stops one launch paying for every
    /// pending store on the machine.
    #[test]
    #[serial_test::serial]
    fn a_scoped_pass_moves_only_the_named_cohort() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let other = home.join(".claude/sandbox");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("data"), b"other").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"3333333333333333","tool":"claude","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
            b"data"
        );
        assert!(
            !home.join(".claude/sandbox-v2/3333333333333333").exists(),
            "another agent's cohort must not be moved by a scoped pass"
        );
        assert!(
            other.exists(),
            "the untouched cohort still needs its shared source"
        );
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        // The scoped-out row must not be stamped current: its store was never
        // copied, and generation 2 would point the session at a private store
        // that does not exist.
        assert_ne!(rows[1]["sandbox_store_generation"], 2);
    }

    /// A parked row is skipped by the bulk passes, but `aoe send` / `aoe
    /// session start` / the HTTP handlers start a trashed or archived session
    /// without unparking it. The pass scoped to that session must move it, or
    /// nothing ever will and its launch bails on the pending transition.
    #[test]
    #[serial_test::serial]
    fn a_scoped_pass_moves_the_parked_row_it_names() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true},
             "archived_at":"2026-09-05T00:00:00Z"},
            {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true},
             "trashed_at":"2026-09-05T00:00:00Z"}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
            b"data"
        );
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert_ne!(rows[1]["sandbox_store_generation"], 2);
        assert!(
            legacy.exists(),
            "the still-parked peer must keep the shared source"
        );
    }

    /// A parked row is skipped for copying, not for the liveness question.
    /// `archive` does not stop the container, and `aoe send` starts an
    /// archived session without unparking it, so a parked peer can be writing
    /// the shared store while an unparked peer is started. Dropping it from
    /// the cohort would publish that store mid-write, at generation 2.
    #[test]
    #[serial_test::serial]
    fn a_live_parked_peer_blocks_its_cohort() {
        for scoped in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir().unwrap();
            let home = dirs::home_dir().unwrap();
            let legacy = home.join(".gemini/sandbox");
            fs::create_dir_all(&legacy).unwrap();
            fs::write(legacy.join("data"), b"data").unwrap();
            let rows = serde_json::json!([
                {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
                {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true},
                 "archived_at":"2026-09-05T00:00:00Z"}
            ]);
            fs::write(
                app.join("sessions.json"),
                serde_json::to_vec(&rows).unwrap(),
            )
            .unwrap();

            let live_peer = |id: &str| Ok(id == "2222222222222222");
            if scoped {
                run_in_only(&app, &home, &live_peer, "1111111111111111").unwrap();
            } else {
                run_in(&app, &home, &live_peer).unwrap();
            }

            assert!(
                !home.join(".gemini/sandbox-v2/1111111111111111").exists(),
                "scoped={scoped}: a live archived peer must block the copy"
            );
            let rows: Value = read_rows(&app);
            assert_ne!(
                rows[0]["sandbox_store_generation"], 2,
                "scoped={scoped}: a blocked row must not be stamped current"
            );
            assert!(
                legacy.exists(),
                "scoped={scoped}: the shared source must survive"
            );

            // Once the parked peer stops, the same pass moves the unparked row
            // and still leaves the parked one on the shared store.
            if scoped {
                run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();
            } else {
                run_in(&app, &home, &|_| Ok(false)).unwrap();
            }
            assert_eq!(
                fs::read(home.join(".gemini/sandbox-v2/1111111111111111/data")).unwrap(),
                b"data",
                "scoped={scoped}: the unparked row must move once the peer stops"
            );
            assert!(
                !home.join(".gemini/sandbox-v2/2222222222222222").exists(),
                "scoped={scoped}: the parked peer must not be copied"
            );
            assert!(
                legacy.exists(),
                "scoped={scoped}: the parked peer still holds the shared source"
            );
        }
    }

    /// A scoped pass must not retire a cleanup root a scoped-out cohort still
    /// lives under. Codex sessions each own a child of one shared root, so
    /// dropping `defer_source_retirement` in the cohort filter would quarantine
    /// and delete every other codex session's store.
    #[test]
    #[serial_test::serial]
    fn a_scoped_pass_holds_the_root_a_scoped_out_cohort_lives_under() {
        let (_temp, _app_guard, app, home) = isolated();
        let root = home.join(".codex/sandbox");
        fs::create_dir_all(root.join("1111111111111111")).unwrap();
        fs::write(root.join("1111111111111111/data"), b"one").unwrap();
        fs::create_dir_all(root.join("2222222222222222")).unwrap();
        fs::write(root.join("2222222222222222/data"), b"two").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"codex","sandbox_info":{"enabled":true}},
            {"id":"2222222222222222","tool":"codex","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in_only(&app, &home, &|_| Ok(false), "1111111111111111").unwrap();

        assert_eq!(
            fs::read(root.join("2222222222222222/data")).unwrap(),
            b"two",
            "a scoped-out cohort's shared store must survive the pass"
        );
        assert_eq!(
            fs::read(home.join(".codex/sandbox-v2/1111111111111111/data")).unwrap(),
            b"one"
        );
    }

    #[test]
    fn row_is_parked_reads_both_fields_and_ignores_nulls() {
        assert!(!row_is_parked(&serde_json::json!({"id":"a"})));
        assert!(!row_is_parked(
            &serde_json::json!({"trashed_at":null,"archived_at":null})
        ));
        assert!(row_is_parked(
            &serde_json::json!({"trashed_at":"2026-09-05T00:00:00Z"})
        ));
        assert!(row_is_parked(
            &serde_json::json!({"archived_at":"2026-09-05T00:00:00Z"})
        ));
    }

    #[test]
    #[serial_test::serial]
    fn unresolved_rows_do_not_block_ready_rows_or_retire_the_shared_source() {
        let (_temp, _app_guard, app, home) = isolated();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"missing-agent","sandbox_info":{"enabled":true}},
            {"tool":"missing-agent","sandbox_info":{"enabled":true}},
            {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let rows: Value = read_rows(&app);
        assert!(rows[0].get("sandbox_store_generation").is_none());
        assert_eq!(rows[2]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/2222222222222222/data")).unwrap(),
            b"data"
        );
        assert!(
            legacy.exists(),
            "an unresolved row must protect the shared source"
        );

        let repaired = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"3333333333333333","tool":"gemini","sandbox_info":{"enabled":true}},
            rows[2].clone()
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&repaired).unwrap(),
        )
        .unwrap();
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let rows: Value = read_rows(&app);
        assert!(rows
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["sandbox_store_generation"] == 2));
        for id in ["1111111111111111", "2222222222222222", "3333333333333333"] {
            assert_eq!(
                fs::read(home.join(".gemini/sandbox-v2").join(id).join("data")).unwrap(),
                b"data"
            );
        }
        assert!(!legacy.exists());
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn persisted_sources_survive_ancestor_symlink_canonicalization() {
        use std::os::unix::fs::symlink;
        let (temp, _app_guard, app, home) = isolated();
        let external = temp.path().join("external-gemini");
        fs::create_dir_all(external.join("sandbox")).unwrap();
        fs::write(external.join("sandbox/data"), b"data").unwrap();
        symlink(&external, home.join(".gemini")).unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        run_in(&app, &home, &|_| Ok(true)).unwrap();
        assert!(external.join("sandbox/data").is_file());
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(external.join("sandbox-v2/one/data")).unwrap(),
            b"data"
        );
        assert!(!external.join("sandbox").exists());
    }

    #[test]
    #[serial_test::serial]
    fn an_unregistered_store_moves_without_being_expanded() {
        let (_temp, _app_guard, app, home) = isolated();
        let root = home.join(".codex/sandbox");
        let peer = "1111111111111111";
        let orphan = "2222222222222222";
        fs::create_dir_all(root.join(peer)).unwrap();
        fs::create_dir_all(root.join(orphan)).unwrap();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join(peer).join("peer"), b"peer").unwrap();
        fs::write(root.join(orphan).join("orphan"), b"orphan").unwrap();
        fs::write(root.join("common"), b"common").unwrap();
        fs::write(root.join("sessions").join("other"), b"other").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(r#"[{{"id":"{peer}","tool":"codex","sandbox_info":{{"enabled":true}}}}]"#),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let destination = home.join(".codex/sandbox-v2");
        assert_eq!(
            fs::read(destination.join(peer).join("peer")).unwrap(),
            b"peer"
        );
        assert_eq!(
            fs::read(destination.join(peer).join("common")).unwrap(),
            b"common",
            "a shared root file still reaches the session that had no copy of it"
        );
        assert!(
            !destination.join(peer).join("sessions").exists(),
            "another session's history must not be replicated into a private store"
        );
        assert_eq!(
            fs::read(destination.join(orphan).join("orphan")).unwrap(),
            b"orphan",
            "a store no session claims is preserved as it was"
        );
        assert!(
            !destination.join(orphan).join("common").exists(),
            "a store no session claims must not be expanded with the shared root"
        );
        assert!(!destination.join(peer).join(orphan).exists());
        assert!(!root.join(peer).exists());
        assert!(!root.join(orphan).exists());
        assert!(!root.join("common").exists());
        let recovered: Vec<_> =
            fs::read_dir(home.join(crate::migrations::v033_isolate_sandbox_content::RECOVERY))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("v027-"))
                })
                .collect();
        assert_eq!(recovered.len(), 1, "the shared root is retained whole");
        assert_eq!(
            fs::read(recovered[0].join("original/sessions/other")).unwrap(),
            b"other",
            "what no private store received stays in the retained original"
        );
        assert!(
            !root.exists(),
            "a retained root leaves nothing at its old path"
        );
    }

    /// A retirement a live mount defers must not be lost: the cohort stays
    /// pending, and the pass that follows retires the root once the mount is
    /// gone instead of stamping it current and leaving the shared store forever.
    #[test]
    #[serial_test::serial]
    fn a_deferred_retirement_is_retried_by_a_later_pass() {
        let (temp, _app_guard, app, home) = isolated();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let id = "3333333333333333";
        let root = home.join(".gemini/sandbox");
        fs::create_dir_all(root.join(id)).unwrap();
        fs::write(root.join(id).join("own"), b"own").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(
                r#"[{{"id":"{id}","tool":"gemini","project_path":"{}","sandbox_info":{{"enabled":true}}}}]"#,
                project.display()
            ),
        )
        .unwrap();

        let expose = |sources: Option<Vec<std::path::PathBuf>>| {
            super::super::v033_isolate_sandbox_content::EXPOSED_SOURCES
                .with(|hook| *hook.borrow_mut() = sources);
        };
        expose(Some(vec![home.clone()]));
        run_in(&app, &home, &|_| Ok(false)).unwrap();
        let rows: Value = read_rows(&app);
        assert_eq!(
            rows[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64),
            Some(1),
            "a deferred retirement keeps its cohort on the pending generation: {rows}"
        );
        assert!(app.join(JOURNAL).is_file(), "the root stays pending");
        assert!(root.join(id).is_dir(), "nothing moved while it was exposed");

        expose(None);
        run_in(&app, &home, &|_| Ok(false)).unwrap();
        assert!(
            !root.exists(),
            "the next pass retires the shared store once nothing exposes it"
        );
        assert!(!app.join(JOURNAL).exists());
        assert!(home.join(".gemini/sandbox-v2").join(id).is_dir());
    }

    /// A publish its quarantine defers must lose nothing either: the displaced
    /// original keeps the destination from being renamed onto it, so the pass
    /// publishes nothing and the row is retried with both stores intact.
    #[test]
    #[serial_test::serial]
    fn a_deferred_publication_is_retried_by_a_later_pass() {
        let (temp, _app_guard, app, home) = isolated();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let id = "4444444444444444";
        let root = home.join(".gemini/sandbox");
        fs::create_dir_all(root.join(id)).unwrap();
        fs::write(root.join(id).join("own"), b"own").unwrap();
        fs::write(root.join("auth.json"), b"auth").unwrap();
        let layout = home.join(".gemini/sandbox-v2");
        // A killed pass published this session and left the destination it
        // displaced at the quarantine, where nothing has retained it yet.
        let destination = layout.join(id);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("published"), b"published").unwrap();
        let quarantine = layout.join(format!(".v027-quarantine-{id}"));
        fs::create_dir_all(&quarantine).unwrap();
        fs::write(quarantine.join("displaced"), b"displaced").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(
                r#"[{{"id":"{id}","tool":"gemini","project_path":"{}","sandbox_info":{{"enabled":true}}}}]"#,
                project.display()
            ),
        )
        .unwrap();

        let expose = |sources: Option<Vec<std::path::PathBuf>>| {
            super::super::v033_isolate_sandbox_content::EXPOSED_SOURCES
                .with(|hook| *hook.borrow_mut() = sources);
        };
        expose(Some(vec![home.clone()]));
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(destination.join("published")).unwrap(),
            b"published",
            "a deferred publish leaves the destination as it was"
        );
        assert_eq!(
            fs::read(quarantine.join("displaced")).unwrap(),
            b"displaced",
            "a deferred retention leaves the quarantine as it was"
        );
        assert!(!destination.join("auth.json").exists());
        assert!(!quarantine.join("published").exists());
        assert!(root.join("auth.json").is_file());
        let rows: Value = read_rows(&app);
        assert_eq!(
            rows[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64),
            Some(1),
            "a deferred publish keeps its row on the pending generation: {rows}"
        );
        assert!(app.join(JOURNAL).is_file(), "the root stays pending");

        expose(None);
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(destination.join("auth.json")).unwrap(), b"auth");
        assert!(!destination.join("published").exists());
        assert!(
            !quarantine.exists(),
            "the pass that publishes retains the quarantine it publishes through"
        );
        assert!(!root.exists(), "the next pass retires the shared store");
        assert!(!app.join(JOURNAL).exists());
        let mut retained = Vec::new();
        let recovery = home.join(crate::migrations::v033_isolate_sandbox_content::RECOVERY);
        for transaction in fs::read_dir(recovery).unwrap() {
            for original in fs::read_dir(transaction.unwrap().path().join("original")).unwrap() {
                retained.push(original.unwrap().file_name().to_string_lossy().into_owned());
            }
        }
        for name in ["displaced", "published"] {
            assert!(
                retained.iter().any(|entry| entry == name),
                "{name} is retained whole rather than dropped: {retained:?}"
            );
        }
    }

    /// The orphan path defers on the same retention: an orphan that cannot
    /// move keeps its root, and every row reading that root, pending rather
    /// than retiring a root that still holds it.
    #[test]
    #[serial_test::serial]
    fn a_deferred_orphan_move_keeps_its_root_pending() {
        let (_temp, _app_guard, app, home) = isolated();
        let root = home.join(".codex/sandbox");
        let peer = "1111111111111111";
        let orphan = "2222222222222222";
        fs::create_dir_all(root.join(peer)).unwrap();
        fs::create_dir_all(root.join(orphan)).unwrap();
        fs::write(root.join(peer).join("peer"), b"peer").unwrap();
        fs::write(root.join(orphan).join("orphan"), b"orphan").unwrap();
        let layout = home.join(".codex/sandbox-v2");
        // A killed pass moved this orphan once and left the destination it
        // displaced at the quarantine, where nothing has retained it yet.
        let destination = layout.join(orphan);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("moved"), b"moved").unwrap();
        let quarantine = layout.join(format!(".v027-quarantine-{orphan}"));
        fs::create_dir_all(&quarantine).unwrap();
        fs::write(quarantine.join("displaced"), b"displaced").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(r#"[{{"id":"{peer}","tool":"codex","sandbox_info":{{"enabled":true}}}}]"#),
        )
        .unwrap();

        let expose = |sources: Option<Vec<std::path::PathBuf>>| {
            super::super::v033_isolate_sandbox_content::EXPOSED_SOURCES
                .with(|hook| *hook.borrow_mut() = sources);
        };
        expose(Some(vec![home.clone()]));
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(root.join(orphan).join("orphan")).unwrap(),
            b"orphan",
            "a deferred move leaves the orphan at its source"
        );
        assert_eq!(fs::read(destination.join("moved")).unwrap(), b"moved");
        assert_eq!(
            fs::read(quarantine.join("displaced")).unwrap(),
            b"displaced"
        );
        assert!(root.is_dir(), "the root holding it stays where it is");
        let rows: Value = read_rows(&app);
        assert!(
            rows[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                < 2,
            "a root that still holds an orphan keeps its rows pending: {rows}"
        );
        assert!(app.join(JOURNAL).is_file(), "the root stays pending");

        expose(None);
        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(destination.join("orphan")).unwrap(), b"orphan");
        assert!(!destination.join("moved").exists());
        assert!(!quarantine.exists());
        assert!(!root.exists(), "the next pass retires the emptied root");
        assert!(!app.join(JOURNAL).exists());
        let mut retained = Vec::new();
        let recovery = home.join(crate::migrations::v033_isolate_sandbox_content::RECOVERY);
        for transaction in fs::read_dir(recovery).unwrap() {
            for original in fs::read_dir(transaction.unwrap().path().join("original")).unwrap() {
                retained.push(original.unwrap().file_name().to_string_lossy().into_owned());
            }
        }
        assert!(
            retained.iter().any(|entry| entry == "moved"),
            "the displaced orphan store is retained whole: {retained:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn retiring_a_root_retains_its_complete_original() {
        let temp = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temp.path());
        let host = temp.path().join(".claude");
        let root = host.join("sandbox");
        fs::create_dir_all(root.join("one")).unwrap();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("one/own"), b"own").unwrap();
        fs::write(root.join("sessions/other"), b"other").unwrap();
        fs::write(root.join("auth.json"), b"secret").unwrap();
        std::os::unix::fs::symlink("/outside-do-not-follow", root.join("latest")).unwrap();
        let super::super::v033_isolate_sandbox_content::Retained::Original(kept) =
            super::super::v033_isolate_sandbox_content::retain_legacy_original(&root, &host)
                .unwrap()
        else {
            panic!("the complete original is retained")
        };
        assert!(!root.exists());
        assert_eq!(fs::read(kept.join("one/own")).unwrap(), b"own");
        assert_eq!(fs::read(kept.join("sessions/other")).unwrap(), b"other");
        assert_eq!(fs::read(kept.join("auth.json")).unwrap(), b"secret");
        assert_eq!(
            fs::read_link(kept.join("latest")).unwrap(),
            Path::new("/outside-do-not-follow")
        );
        assert!(matches!(
            super::super::v033_isolate_sandbox_content::retain_legacy_original(&root, &host)
                .unwrap(),
            super::super::v033_isolate_sandbox_content::Retained::Absent
        ));
    }

    /// Retiring a fully replicated root deletes it, so every later row that
    /// plans against it meets a root that is not there: a restored session, a
    /// second profile, a generation reset. That row must publish and commit.
    /// Failing instead would fail `aoe migrate`, and every command that runs
    /// migrations, from then on, with nothing able to clear it.
    #[test]
    #[serial_test::serial]
    fn a_retired_root_does_not_fail_the_next_row() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        let root = home.join(".codex/sandbox");
        fs::create_dir_all(root.join("codex-one")).unwrap();
        fs::write(root.join("codex-one").join("own"), b"own").unwrap();
        fs::write(root.join("auth.json"), b"secret").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"codex-one","tool":"codex","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();
        assert!(!root.exists(), "nothing was left unreplicated, so it goes");

        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"codex-one","tool":"codex","sandbox_info":{"enabled":true},"sandbox_store_generation":2},
                {"id":"codex-two","tool":"codex","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let rows: Value = read_rows(&app);
        assert_eq!(rows[1]["sandbox_store_generation"], 2);
        assert!(home.join(".codex/sandbox-v2/codex-two").is_dir());
    }

    /// Retirement runs before the generation commit, so a pass killed partway
    /// through it leaves the row pending and the shared root half emptied.
    /// Removing the per-instance stores first is what makes that safe: the
    /// row's source is gone, so the next pass keeps the store it published
    /// rather than re-copying a root that has lost the credential it needs.
    #[test]
    #[serial_test::serial]
    fn an_interrupted_retirement_keeps_the_published_store() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        let root = home.join(".codex/sandbox");
        let destination = home.join(".codex/sandbox-v2/codex-one");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&destination).unwrap();
        // The store published, its per-instance source already removed, and
        // the shared root still holding the files that reached it.
        fs::write(destination.join("auth.json"), b"secret").unwrap();
        fs::write(destination.join("own"), b"own").unwrap();
        fs::write(root.join("auth.json"), b"secret").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"codex-one","tool":"codex","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(destination.join("auth.json")).unwrap(), b"secret");
        assert_eq!(fs::read(destination.join("own")).unwrap(), b"own");
        assert!(!root.exists());
        let rows: Value = read_rows(&app);
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
    }

    /// The shared root goes when everything in it was replicated, exactly as
    /// it did before the overlay stopped folding directories in.
    #[test]
    #[serial_test::serial]
    fn a_fully_replicated_shared_root_is_still_retired() {
        let (_temp, _app_guard, app, home) = isolated();
        let root = home.join(".codex/sandbox");
        let peer = "1111111111111111";
        fs::create_dir_all(root.join(peer)).unwrap();
        fs::write(root.join(peer).join("peer"), b"peer").unwrap();
        fs::write(root.join("common"), b"common").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(r#"[{{"id":"{peer}","tool":"codex","sandbox_info":{{"enabled":true}}}}]"#),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".codex/sandbox-v2").join(peer).join("common")).unwrap(),
            b"common"
        );
        assert!(!root.exists());
    }

    /// A killed `aoe migrate` leaves a half-written staging directory in the
    /// private layout. The pass that picks that store up again must clear it,
    /// including on the rename path, which never opens the staging directory
    /// it has to remove.
    #[test]
    #[serial_test::serial]
    fn an_interrupted_stage_is_cleared_when_an_unregistered_store_moves() {
        let (_temp, _app_guard, app, home) = isolated();
        let root = home.join(".codex/sandbox");
        let peer = "1111111111111111";
        let orphan = "2222222222222222";
        let destination = home.join(".codex/sandbox-v2");
        fs::create_dir_all(root.join(peer)).unwrap();
        fs::create_dir_all(root.join(orphan)).unwrap();
        fs::write(root.join(orphan).join("orphan"), b"orphan").unwrap();
        let stage = destination.join(format!(".v027-stage-{orphan}"));
        fs::create_dir_all(stage.join("half")).unwrap();
        fs::write(stage.join("half").join("written"), b"partial").unwrap();
        fs::create_dir_all(destination.join(orphan)).unwrap();
        fs::write(destination.join(orphan).join("stale"), b"stale").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!(r#"[{{"id":"{peer}","tool":"codex","sandbox_info":{{"enabled":true}}}}]"#),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        assert!(!stage.exists(), "the interrupted staging tree must be gone");
        assert!(!destination
            .join(format!(".v027-quarantine-{orphan}"))
            .exists());
        assert_eq!(
            fs::read(destination.join(orphan).join("orphan")).unwrap(),
            b"orphan"
        );
        assert!(
            !destination.join(orphan).join("stale").exists(),
            "the interrupted run's destination must be replaced, not merged into"
        );
    }

    /// A registry row shaped like a real `Instance`, so `Storage::update`
    /// can load the registry it sits in. The project must not be an ancestor
    /// of HOME: the isolation pass refuses a sandbox whose mount would expose
    /// the recovery namespace.
    fn instance_row(id: &str, project: &str) -> Value {
        let mut row = serde_json::to_value(crate::session::Instance::new(id, project)).unwrap();
        row["id"] = id.into();
        row["tool"] = "gemini".into();
        // A fresh `Instance` is born on the current generation; this one
        // still reads the shared store.
        row["sandbox_store_generation"] = 0.into();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": format!("aoe-sandbox-{id}"),
        });
        row
    }

    struct PausedPass {
        pass: Option<std::thread::JoinHandle<Result<()>>>,
        paused: std::sync::mpsc::Receiver<()>,
        release: Option<std::sync::mpsc::Sender<()>>,
    }

    impl PausedPass {
        fn finish(mut self) -> std::thread::Result<Result<()>> {
            drop(self.release.take());
            self.pass.take().unwrap().join()
        }
    }

    impl Drop for PausedPass {
        fn drop(&mut self) {
            drop(self.release.take());
            if let Some(pass) = self.pass.take() {
                let _ = pass.join();
            }
        }
    }

    fn paused_pass(app: &Path, home: &Path) -> PausedPass {
        paused_pass_with(app, home, false)
    }

    fn paused_pass_with(app: &Path, home: &Path, live_after_gate: bool) -> PausedPass {
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let app = app.to_path_buf();
        let home = home.to_path_buf();
        let pass = std::thread::spawn(move || {
            let live = std::rc::Rc::new(std::cell::Cell::new(false));
            let gate_live = live.clone();
            COPY_GATE.with(|gate| {
                *gate.borrow_mut() = Some(Box::new(move |_: &Path| {
                    paused_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                    gate_live.set(live_after_gate);
                }));
            });
            super::run_in(
                &app,
                &home,
                &|_| Ok(live.get()),
                &|_| Ok(true),
                false,
                true,
                None,
            )
        });
        PausedPass {
            pass: Some(pass),
            paused: paused_rx,
            release: Some(release_tx),
        }
    }

    /// The copy runs without the transition lock, so publication re-reads
    /// the registries and asks about liveness again. Each guard, alone: a
    /// row that lost its plan is not published, a root that gained a reader
    /// is not retired, and a container that came up keeps its cohort
    /// pending.
    #[test]
    #[serial_test::serial]
    fn publication_revalidates_what_changed_during_the_copy() {
        enum Case {
            RowGone,
            NewReader,
            ContainerUp,
        }
        for case in [Case::RowGone, Case::NewReader, Case::ContainerUp] {
            let temp = tempfile::tempdir().unwrap();
            let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir().unwrap();
            let home = dirs::home_dir().unwrap();
            fs::create_dir_all(&app).unwrap();
            fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
            fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
            fs::write(
                app.join("sessions.json"),
                format!("[{}]", row("1111111111111111")),
            )
            .unwrap();
            let pass = paused_pass_with(&app, &home, matches!(case, Case::ContainerUp));
            pass.paused
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the pass reached its copy");
            match case {
                Case::RowGone => fs::write(app.join("sessions.json"), b"[]").unwrap(),
                Case::NewReader => {
                    // Appended beside the checkpointed row, as a writer
                    // holding the registry lock would.
                    let mut rows: Value =
                        serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap())
                            .unwrap();
                    rows.as_array_mut()
                        .unwrap()
                        .push(serde_json::from_str(&row("2222222222222222")).unwrap());
                    fs::write(
                        app.join("sessions.json"),
                        serde_json::to_vec(&rows).unwrap(),
                    )
                    .unwrap();
                }
                Case::ContainerUp => {}
            }
            pass.finish().unwrap().unwrap();

            let rows: Value = read_rows(&app);
            assert!(
                home.join(".gemini/sandbox/history/id.json").is_file(),
                "the shared source must survive"
            );
            match case {
                Case::RowGone => {
                    assert_eq!(rows.as_array().unwrap().len(), 0, "the deletion stands");
                }
                Case::NewReader => {
                    assert_eq!(
                        rows[0]["sandbox_store_generation"], 2,
                        "the planned row publishes"
                    );
                    assert_ne!(rows[1]["sandbox_store_generation"], 2, "the newcomer waits");
                }
                Case::ContainerUp => {
                    assert_eq!(
                        rows[0]["sandbox_store_generation"], 1,
                        "a live cohort stays pending"
                    );
                    assert!(app.join(JOURNAL).is_file());
                }
            }
        }
    }

    fn wait_finished<T>(handle: &std::thread::ScopedJoinHandle<'_, T>, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !handle.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "{what} did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The copy runs without the transition and registry locks, so session
    /// and group writes in every profile go through while it is in flight,
    /// and the publish that follows lands on the registry as those writes
    /// left it rather than on the snapshot the plan was made from.
    #[test]
    #[serial_test::serial]
    fn registries_stay_writable_while_a_store_copies() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        let alpha = app.join("profiles/alpha/sessions.json");
        let beta = app.join("profiles/beta/sessions.json");
        fs::create_dir_all(alpha.parent().unwrap()).unwrap();
        fs::create_dir_all(beta.parent().unwrap()).unwrap();
        fs::write(
            &alpha,
            serde_json::to_vec(&vec![instance_row(
                "1111111111111111",
                project.to_str().unwrap(),
            )])
            .unwrap(),
        )
        .unwrap();
        fs::write(&beta, b"[]").unwrap();

        std::thread::scope(|scope| {
            let pass = paused_pass(&app, &home);
            pass.paused
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the pass reached its copy");

            let writes = {
                let (alpha, beta) = (alpha.clone(), beta.clone());
                let project = project.clone();
                scope.spawn(move || {
                    let write = |profile: &str, path: PathBuf, title: &str| {
                        crate::session::Storage::new_for_test_path(profile, path).update(
                            |instances, _| {
                                match instances.first_mut() {
                                    Some(first) => first.title = title.to_string(),
                                    None => instances.push(crate::session::Instance::new(
                                        title,
                                        project.to_str().unwrap(),
                                    )),
                                }
                                Ok(())
                            },
                        )
                    };
                    (
                        write("alpha", alpha, "renamed mid-copy"),
                        write("beta", beta, "beta row"),
                    )
                })
            };
            wait_finished(&writes, "storage writes during the copy");
            let (alpha_write, beta_write) = writes.join().unwrap();
            alpha_write.unwrap();
            beta_write.unwrap();

            pass.finish().unwrap().unwrap();
        });

        let alpha_rows: Value = serde_json::from_slice(&fs::read(&alpha).unwrap()).unwrap();
        assert_eq!(alpha_rows[0]["title"], "renamed mid-copy");
        assert_eq!(alpha_rows[0]["sandbox_store_generation"], 2);
        assert!(alpha_rows[0]
            .get("sandbox_store_transition_paths")
            .is_none());
        let beta_rows: Value = serde_json::from_slice(&fs::read(&beta).unwrap()).unwrap();
        assert_eq!(beta_rows[0]["title"], "beta row");
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/1111111111111111/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!home.join(".gemini/sandbox").exists());
        assert!(!app.join(JOURNAL).exists());
    }

    /// Two passes over one cohort cannot both copy and publish it. A full pass
    /// that finds the root held elsewhere leaves it pending and copies
    /// nothing; a pass scoped to a member waits for the holder and then finds
    /// the row current, so the store is copied exactly once.
    #[test]
    #[serial_test::serial]
    fn competing_passes_on_one_cohort_publish_once() {
        let (_temp, _app_guard, app, home) = isolated();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(
            app.join("sessions.json"),
            format!("[{},{}]", row("1111111111111111"), row("2222222222222222")),
        )
        .unwrap();

        // Planning canonicalizes the source before choosing its cohort lock.
        // Resolve it before publication removes it, including symlinked temp
        // ancestors such as macOS /var -> /private/var.
        let cohort_root = fs::canonicalize(home.join(".gemini/sandbox")).unwrap();

        std::thread::scope(|scope| {
            let holder = paused_pass(&app, &home);
            holder
                .paused
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the holder reached its copy");

            let copies = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (contended_tx, contended_rx) = std::sync::mpsc::channel();
            let counting_pass = |only: Option<&'static str>| {
                let contended_tx = contended_tx.clone();
                let (app, home, copies) = (app.clone(), home.clone(), copies.clone());
                scope.spawn(move || {
                    let _observer = only
                        .map(|_| crate::session::observe_lock_contention_for_test(contended_tx));
                    COPY_GATE.with(|gate| {
                        *gate.borrow_mut() = Some(Box::new(move |_: &Path| {
                            copies.fetch_add(1, Ordering::Relaxed);
                        }));
                    });
                    super::run_in(
                        &app,
                        &home,
                        &|_| Ok(false),
                        &|_| Ok(true),
                        false,
                        true,
                        only,
                    )
                })
            };

            // A full pass gets its turn at the transition lock, sees the root is
            // busy, and returns without copying while the holder is still paused.
            let full = counting_pass(None);
            wait_finished(&full, "a full pass beside a held cohort");
            full.join().unwrap().unwrap();
            assert_eq!(copies.load(Ordering::Relaxed), 0);
            let rows: Value = read_rows(&app);
            assert_eq!(rows[0]["sandbox_store_generation"], 1, "nothing published");

            // A scoped pass waits for the holder instead of copying beside it.
            let scoped = counting_pass(Some("1111111111111111"));
            let contention = contended_rx.recv_timeout(std::time::Duration::from_secs(10));
            let finished_while_held = scoped.is_finished();
            let copies_while_held = copies.load(Ordering::Relaxed);
            let holder_result = holder.finish();
            wait_finished(&scoped, "the scoped pass after the holder finished");
            let scoped_result = scoped.join();
            holder_result.unwrap().unwrap();
            scoped_result.unwrap().unwrap();
            assert_eq!(
                contention.expect("scoped pass must reach the held cohort lock"),
                app.join(cohort_lock_name(&cohort_root))
            );
            assert!(
                !finished_while_held,
                "a scoped pass must wait for the holder"
            );
            assert_eq!(copies_while_held, 0);
            assert_eq!(
                copies.load(Ordering::Relaxed),
                0,
                "the holder's copy was the only one"
            );

            let rows: Value = read_rows(&app);
            assert_eq!(rows[0]["sandbox_store_generation"], 2);
            assert_eq!(rows[1]["sandbox_store_generation"], 2);
            for id in ["1111111111111111", "2222222222222222"] {
                assert_eq!(
                    fs::read(
                        home.join(".gemini/sandbox-v2")
                            .join(id)
                            .join("history/id.json")
                    )
                    .unwrap(),
                    b"legacy"
                );
            }
            assert!(!home.join(".gemini/sandbox").exists());
            assert!(!app.join(JOURNAL).exists());
        });
    }

    #[test]
    #[serial_test::serial]
    fn storage_update_preserves_a_checkpointed_plan() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let mut value = serde_json::to_value(crate::session::Instance::new("one", "/tmp")).unwrap();
        value["sandbox_store_generation"] = 1.into();
        value["sandbox_store_transition_paths"] = serde_json::json!([{
            "source": source,
            "destination": destination,
        }]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![value]).unwrap(),
        )
        .unwrap();
        let storage = crate::session::Storage::new_for_test_path(
            "v027-plan-roundtrip",
            app.join("sessions.json"),
        );

        storage
            .update(|instances, _| {
                instances[0].title = "updated".to_string();
                Ok(())
            })
            .unwrap();

        let written: Value = read_rows(&app);
        assert_eq!(
            written[0]["sandbox_store_transition_paths"][0]["source"],
            serde_json::json!(source)
        );
        assert_eq!(
            written[0]["sandbox_store_transition_paths"][0]["destination"],
            serde_json::json!(destination)
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn preserves_symlink_mtimes() {
        use nix::sys::stat::{utimensat, UtimensatFlags};
        use nix::sys::time::TimeSpec;
        use std::os::unix::fs::{symlink, MetadataExt};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("v2/one");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("target"), b"target").unwrap();
        let link = source.join("link");
        symlink("target", &link).unwrap();
        let atime = TimeSpec::new(1_600_000_000, 123_000_000);
        let mtime = TimeSpec::new(1_600_000_001, 456_000_000);
        let source_dir = fs::File::open(&source).unwrap();
        utimensat(
            &source_dir,
            "link",
            &atime,
            &mtime,
            UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();

        assert!(publish_store(&source, &destination, &BTreeSet::new(), None, false).unwrap());

        let copied = fs::symlink_metadata(destination.join("link")).unwrap();
        assert_eq!(
            (copied.atime(), copied.atime_nsec()),
            (atime.tv_sec(), atime.tv_nsec())
        );
        assert_eq!(
            (copied.mtime(), copied.mtime_nsec()),
            (mtime.tv_sec(), mtime.tv_nsec())
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn preserves_root_and_read_only_directory_modes() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, _app_guard, app, home) = isolated();
        let source = home.join(".gemini/sandbox");
        fs::create_dir_all(source.join("readonly")).unwrap();
        fs::write(source.join("readonly/data"), b"data").unwrap();
        fs::set_permissions(source.join("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let destination = home.join(".gemini/sandbox-v2/one");
        assert_eq!(
            fs::read(destination.join("readonly/data")).unwrap(),
            b"data"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(destination.join("readonly"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn skips_escaping_source_symlinks_and_refuses_destination_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside");
        let destination = temp.path().join("v2/one");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        fs::write(source.join("credential"), b"credential").unwrap();
        fs::set_permissions(source.join("credential"), fs::Permissions::from_mode(0o600)).unwrap();
        symlink(outside.join("secret"), source.join("escape")).unwrap();
        symlink("credential", source.join("credential-link")).unwrap();
        assert!(publish_store(&source, &destination, &BTreeSet::new(), None, false).unwrap());
        assert!(!destination.join("escape").exists());
        assert_eq!(
            fs::read(destination.join("credential-link")).unwrap(),
            b"credential"
        );
        assert_eq!(
            fs::metadata(destination.join("credential"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        remove_tree_no_links(&destination).unwrap();
        symlink(&outside, &destination).unwrap();
        assert!(publish_store(&source, &destination, &BTreeSet::new(), None, false).is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");
    }

    /// Move A is parked inside its first progress callback while move B runs
    /// start to finish, so A's remaining files must be counted from where A
    /// left off. A shared counter reported B's reset and larger files to A.
    #[test]
    #[cfg(unix)]
    fn concurrent_moves_report_only_their_own_totals() {
        use std::sync::{mpsc, Arc, Mutex};

        // Distinct file sizes so a leaked byte total is visible, and counts
        // that make the two moves' 100-file report boundaries interleave.
        const A_FILES: usize = 200;
        const A_SIZE: usize = 10;
        const B_FILES: usize = 150;
        const B_SIZE: usize = 1000;

        fn seed(root: &Path, files: usize, size: usize) {
            fs::create_dir_all(root).unwrap();
            for index in 0..files {
                fs::write(root.join(format!("f{index}")), vec![b'x'; size]).unwrap();
            }
        }

        fn collecting_reporter(seen: &Arc<Mutex<Vec<String>>>) -> progress::Reporter {
            let seen = Arc::clone(seen);
            Arc::new(move |event| {
                if let progress::Event::Progress(message) = event {
                    seen.lock().unwrap().push(message);
                }
            })
        }

        let temp = tempfile::tempdir().unwrap();
        seed(&temp.path().join("a/source"), A_FILES, A_SIZE);
        seed(&temp.path().join("b/source"), B_FILES, B_SIZE);

        let (parked_tx, parked_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let a_seen = Arc::new(Mutex::new(Vec::new()));

        let mover_a = {
            let root = temp.path().to_path_buf();
            let a_seen = Arc::clone(&a_seen);
            std::thread::spawn(move || {
                let collect = collecting_reporter(&a_seen);
                // Park inside the first report only, so move B's whole copy
                // lands between A's 100th and 101st file.
                let gate = Mutex::new(Some((parked_tx, resume_rx)));
                let reporter: progress::Reporter = Arc::new(move |event| {
                    collect(event);
                    if let Some((parked_tx, resume_rx)) = gate.lock().unwrap().take() {
                        parked_tx.send(()).unwrap();
                        resume_rx.recv().unwrap();
                    }
                });
                let _guard = progress::install(Some(reporter));
                assert!(publish_store(
                    &root.join("a/source"),
                    &root.join("a/private/one"),
                    &BTreeSet::new(),
                    None,
                    false,
                )
                .unwrap());
            })
        };

        parked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("move A parked inside its first report");
        let b_seen = Arc::new(Mutex::new(Vec::new()));
        {
            let _guard = progress::install(Some(collecting_reporter(&b_seen)));
            assert!(publish_store(
                &temp.path().join("b/source"),
                &temp.path().join("b/private/two"),
                &BTreeSet::new(),
                None,
                false,
            )
            .unwrap());
        }
        resume_tx.send(()).unwrap();
        mover_a.join().unwrap();

        assert_eq!(
            *a_seen.lock().unwrap(),
            vec![
                format!("100 files, {}", progress::format_bytes(100 * A_SIZE as u64)),
                format!("200 files, {}", progress::format_bytes(200 * A_SIZE as u64)),
            ]
        );
        assert_eq!(
            *b_seen.lock().unwrap(),
            vec![format!(
                "100 files, {}",
                progress::format_bytes(100 * B_SIZE as u64)
            )]
        );
    }
}
