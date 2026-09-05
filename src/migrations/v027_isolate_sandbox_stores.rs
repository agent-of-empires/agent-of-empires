//! Migration v027: move shared sandbox stores to the private v2 layout.
//!
//! A live legacy cohort remains readable until it stops. Stopped cohorts are
//! copied under a global transition lock, synced, atomically published, and
//! switched by their durable generation field. Staging directories, the
//! journal, and the legacy quarantine exist only while that bounded transition
//! is pending; the committed state keeps only `sandbox-v2/<instance>`.

use super::progress;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Files and bytes copied by the store copy in flight, for the progress line.
static COPY_FILES: AtomicU64 = AtomicU64::new(0);
static COPY_BYTES: AtomicU64 = AtomicU64::new(0);

fn begin_copy_progress() {
    COPY_FILES.store(0, Ordering::Relaxed);
    COPY_BYTES.store(0, Ordering::Relaxed);
}

/// One regular file copied. Reports every 100 files so a large store shows
/// movement without flooding the reporter.
fn copied_file(bytes: u64) {
    let files = COPY_FILES.fetch_add(1, Ordering::Relaxed) + 1;
    let total = COPY_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if files % 100 == 0 {
        progress::progress(format!("{files} files, {}", progress::format_bytes(total)));
    }
}

/// One liveness probe per session inspects a container each; a machine with
/// many sandboxed sessions paid for a subprocess per row. Ask the runtime for
/// every sandbox container once and answer from that; a session it did not
/// list (or a runtime that returned nothing) still gets the per-row probe, so
/// an unreachable runtime keeps reading as live.
///
/// The snapshot is taken at the first probe and reused for the pass, so a
/// container started mid-pass reads as stopped for later cohorts. That is
/// contained: `reap_migrated_container` removes without force, so a container
/// that came alive fails the removal and the transition, rather than losing
/// its store underneath a running agent.
///
/// `announce` lets the fallback probe say once, per pass, that the runtime
/// could not be asked; the per-startup reconcile passes `false` so a machine
/// whose runtime is down is not told the same thing on every command.
fn batched_running_probe(announce: bool) -> impl Fn(&str) -> Result<bool> {
    let batch: std::sync::OnceLock<std::collections::HashMap<String, bool>> =
        std::sync::OnceLock::new();
    let runtime_noticed = std::cell::Cell::new(false);
    move |id: &str| {
        let states = batch.get_or_init(|| {
            progress::step("checking which sandbox containers are running");
            crate::containers::batch_container_health()
        });
        if let Some(running) = states.get(&crate::containers::DockerContainer::generate_name(id)) {
            return Ok(*running);
        }
        let (running, unanswered) = probe_container_running(id)?;
        if unanswered && announce && !runtime_noticed.replace(true) {
            progress::notice(
                "container runtime unavailable; sandboxed sessions keep their shared agent store until their containers can be checked",
            );
        }
        Ok(running)
    }
}

/// Whether a migrated row's container is live, so its store must be left
/// alone this pass, plus whether that answer is the fail-closed substitute for
/// a runtime that could not be asked. Publishing a store and retiring its
/// legacy source while a container AoE cannot see may still be writing to it
/// is the one outcome this migration must never produce, so an unknown answer
/// takes the arm that copies nothing.
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
type RunningProbe<'a> = dyn Fn(&str) -> Result<bool> + 'a;

/// Reaps the stopped container of a row whose store has moved. `Ok(false)`
/// leaves the row pending; see [`reap_migrated_container`].
type ReapProbe<'a> = dyn Fn(&str) -> Result<bool> + 'a;

/// Whether a runtime error means the runtime could not answer, rather than
/// telling us anything about the container.
///
/// An absent binary, a stopped daemon, a denied socket, and the
/// `InspectFailed` catch-all that a timed-out or unrecognised probe falls
/// through to ([`crate::containers::error::DockerError`], produced by
/// `classify_probe_failure`) all mean the same thing: AoE asked and learned
/// nothing. None of them may abort the migration, because that aborts
/// `run_migrations` before the schema version is committed and so fails every
/// later `aoe` invocation too. Callers substitute their own fail-closed answer
/// and leave the row pending. A local I/O fault is a real failure and still
/// propagates.
fn runtime_cannot_answer(error: &crate::containers::error::DockerError) -> bool {
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
fn reap_migrated_container(id: &str) -> Result<bool> {
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
/// cohort is a member the gate never asks about, and its peers' store is then
/// published while that session is still writing to it. Three reviews of this
/// migration found that same defect three times, each from a different filter
/// applied before cohorts were assembled. Eligibility is therefore a property
/// of the target and never a filter on the rows that build one.
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

/// Retry cohorts that were live during the schema migration. This is called on
/// every startup until no pre-v2 row remains, then becomes a cheap read.
/// `announce` narrates pending rows (see [`ANNOUNCE`]).
pub(crate) fn reconcile_pending(announce: bool) -> Result<()> {
    reconcile_scoped(announce, None)
}

/// Move the store of one session, for the start that is about to launch it.
/// The cohort sharing that session's store moves with it, since the cohort is
/// the unit the liveness gate reasons about; every other cohort is left alone,
/// so a machine with many parked or idle sessions does not pay for all of them
/// on one launch.
pub(crate) fn migrate_instance(id: &str) -> Result<()> {
    reconcile_scoped(false, Some(id))
}

/// `only` scopes the move to a single instance; `announce` both narrates and
/// selects the bulk path, so a bare `aoe` start reports what is pending
/// without copying while `aoe migrate` moves everything eligible.
fn reconcile_scoped(announce: bool, only: Option<&str>) -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    if !transition_may_be_pending(&app_dir)? {
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
        &batched_running_probe(announce),
        &reap_migrated_container,
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

fn transition_may_be_pending(app_dir: &Path) -> Result<bool> {
    if app_dir.join(JOURNAL).exists() {
        return Ok(true);
    }
    for registry in load_registries(app_dir)? {
        let Some(rows) = registry.value.as_array() else {
            continue;
        };
        if rows.iter().any(|row| {
            row.get("sandbox_info")
                .and_then(|sandbox| sandbox.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && (row
                    .get("sandbox_store_generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    < u64::from(
                        crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
                    )
                    || transition_paths(row).ok().flatten().is_some())
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a row is parked: trashed or archived. Such a session is not going
/// to be started, and a trashed one is usually deleted within
/// `trash_retention_days`, so copying its store buys nothing and costs a full
/// store per row. Parked rows keep their shared store and migrate on the start
/// that follows a restore or unarchive.
///
/// A parked row still blocks retirement of the shared source it reads, via
/// `defer_source_retirement`: retiring underneath it would leave a restored
/// session with no store to open.
///
/// A pass scoped to the parked row itself ignores this: `aoe send`, `aoe
/// session start` and the HTTP start/send handlers all launch a trashed or
/// archived session without unparking it first, and skipping it there leaves
/// it on a shared store no later pass will move, so the launch bails forever.
fn row_is_parked(row: &Value) -> bool {
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
    let _lock = crate::session::acquire_storage_flock(app_dir, LOCK)?;
    let registry_paths = registry_paths(app_dir)?;
    let mut registry_dirs: Vec<PathBuf> = registry_paths
        .iter()
        .filter_map(|path| path.parent())
        .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
        .collect();
    registry_dirs.sort();
    registry_dirs.dedup();
    let _registry_locks: Vec<crate::session::StorageFlock> = registry_dirs
        .iter()
        .map(|dir| {
            crate::session::acquire_storage_flock(dir, crate::session::STORAGE_LOCK_FILENAME)
        })
        .collect::<Result<_>>()?;
    let mut registries = load_registry_paths(registry_paths)?;
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
    let mut row_ids_by_root: BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>> = BTreeMap::new();

    for registry in &registries {
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
            // A parked row does not defer retirement globally: it becomes a
            // `Hold` target, and `all_ready` refuses to retire any root that
            // carries one. Setting the pass-wide flag here would block every
            // unrelated root too, so no store on a machine with a single
            // archived session would ever be reclaimed.
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
    // Scoping demotes; it never removes. A cohort not named by this pass keeps
    // every member and every member keeps its place in the liveness fold, so
    // the gate still asks about sessions this pass will not touch. Dropping
    // them instead is what let an earlier revision copy a store out from under
    // a live peer.
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
            // Another session's cohort: it still holds its shared source.
            // Demoting its members to `Hold` is what protects it, through
            // `all_ready`; the pass-wide flag would also strand the cohort
            // this pass just emptied.
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
    // Reporting only. `affected_rows` excludes held rows by construction, so
    // without this the completion notice subtracts them from nothing and tells
    // a user the transition finished while parked sessions are still on the
    // shared store.
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
    }
    if !needs_registry_write
        && cohorts.is_empty()
        && known_sources.iter().all(|source| !source.exists())
        && !journal.exists()
    {
        return Ok(());
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
    let mut orphan_ready_ids = BTreeSet::new();
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
        for orphan in excluded_by_root
            .get(root)
            .into_iter()
            .flatten()
            .filter(|id| !row_ids.contains(*id))
        {
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
            progress::step(format!("copying unregistered store {}", source.display()));
            publish_store(
                &source,
                &destination_parent.join(orphan),
                excluded_by_root
                    .get(root)
                    .context("missing orphan cleanup-root exclusions")?,
                Some(root),
                false,
            )?;
            orphan_ready_ids.insert((root.clone(), id.into_owned()));
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
    for (shared, cohort) in &cohorts {
        let ids: BTreeSet<&str> = cohort.iter().map(|target| target.id.as_str()).collect();
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
            copied_targets += 1;
            if copied_targets == 1 {
                // Said once, right before the first copy: this is the part
                // that can take minutes, and the one a user may want to skip.
                progress::notice(format!(
                    "Isolating agent stores for {} sandboxed session(s): each gets its own copy of the \
                     shared agent store under sandbox-v2/. Large stores take a while. To start without \
                     waiting, quit and run with {DEFER_ENV}=1; finish later with `aoe migrate`.",
                    total_targets
                ));
            }
            progress::step(format!(
                "copying agent store {copied_targets}/{total_targets}: {} -> {}",
                shared.display(),
                target.private.display()
            ));
            let excluded = excluded_by_root
                .get(&target.cleanup_root)
                .context("missing cleanup-root exclusions")?;
            publish_store(
                shared,
                &target.private,
                excluded,
                (shared != &target.cleanup_root).then_some(target.cleanup_root.as_path()),
                shared == &target.cleanup_root,
            )?;
        }
    }

    for (root, id) in orphan_ready_ids {
        if !reap(&id)? {
            blocked_roots.insert(root);
        }
    }

    // Remove stopped containers only after every store for the row is durable.
    // `force=false` makes a concurrent start fail the transition rather than
    // stopping a container that became live after the probe. A runtime that
    // cannot be asked defers the row instead, so its legacy source survives for
    // the pass that can finish it. Keyed by id to reap once, but carrying every
    // row naming it: two profiles can hold one instance and all must be held.
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
            retire_legacy(root)?;
        } else {
            pending.push(root.to_string_lossy().into_owned());
        }
    }

    for &(registry, row) in &ready_rows {
        if let Some(value) = registries[registry]
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
    for registry in &mut registries {
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
    for registry in &registries {
        let bytes = serde_json::to_vec_pretty(&registry.value)?;
        crate::session::atomic_write(&registry.path, &bytes)?;
        sync_parent(&registry.path)?;
    }

    let done = ready_rows.len();
    if !affected_rows.is_empty() && (announce || done > 0) {
        let left = affected_rows.len().saturating_sub(done);
        progress::notice(match (left, held_row_count) {
            (0, 0) => format!("{done} sandboxed session(s) now use private agent stores."),
            (0, held) => format!(
                "{done} sandboxed session(s) now use private agent stores. {held} trashed or archived session(s) stay on the shared store; each moves when it is started, or restore or unarchive it and run `aoe migrate`."
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
    Ok(())
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

fn registry_paths(app_dir: &Path) -> Result<Vec<PathBuf>> {
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

fn profile_for_registry(app_dir: &Path, path: &Path) -> String {
    path.strip_prefix(app_dir.join("profiles"))
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("")
        .to_string()
}

fn instance_children(root: &Path) -> Result<BTreeSet<std::ffi::OsString>> {
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

fn publish_store(
    source: &Path,
    destination: &Path,
    excluded_root_children: &BTreeSet<std::ffi::OsString>,
    overlay_shared_root: Option<&Path>,
    exclude_source_children: bool,
) -> Result<()> {
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
    let layout_root = parent.parent().context("private layout has no parent")?;
    fs::create_dir_all(layout_root)?;
    let anchored_parent = crate::session::AnchoredDir::create(parent)?;
    fs::File::open(layout_root)?.sync_all()?;
    let leaf = destination
        .file_name()
        .context("private store has no leaf")?;
    let stage_leaf = format!(".v027-stage-{}", leaf.to_string_lossy());
    let stage = anchored_parent.path().join(&stage_leaf);
    let quarantine_leaf = format!(".v027-quarantine-{}", leaf.to_string_lossy());
    let quarantine = anchored_parent.path().join(&quarantine_leaf);
    remove_tree_no_links(&stage)?;
    remove_tree_no_links(&quarantine)?;
    if !source_exists {
        anchored_parent.ensure_dir(Path::new(leaf))?;
        fs::File::open(parent)?.sync_all()?;
        return Ok(());
    }
    fs::create_dir(&stage)?;
    begin_copy_progress();
    copy_tree_no_links(
        source,
        &stage,
        exclude_source_children.then_some(excluded_root_children),
        false,
    )?;
    if let Some(overlay) = overlay_shared_root {
        copy_tree_no_links(overlay, &stage, Some(excluded_root_children), true)?;
    }
    fs::set_permissions(&stage, fs::symlink_metadata(source)?.permissions())?;
    sync_tree(&stage)?;

    let destination_exists = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            remove_tree_no_links(&stage)?;
            bail!("v027 destination is a symlink: {}", destination.display());
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", destination.display()))
        }
    };
    if destination_exists {
        fs::rename(destination, &quarantine)?;
    }
    fs::rename(&stage, destination)?;
    fs::File::open(parent)?.sync_all()?;
    remove_tree_no_links(&quarantine)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn copy_tree_no_links(
    source: &Path,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    overwrite_newer: bool,
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
    )
}

#[cfg(unix)]
fn copy_tree_from_fd(
    fd: std::os::fd::OwnedFd,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    relative: &Path,
    overwrite_newer: bool,
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
            copy_tree_from_fd(child, &target, None, &relative.join(name), overwrite_newer)?;
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
            let mut options = fs::OpenOptions::new();
            options.write(true);
            if target_exists {
                options.truncate(true);
            } else {
                options.create_new(true);
            }
            let mut output = options.open(&target)?;
            let bytes = std::io::copy(&mut input, &mut output)?;
            copied_file(bytes);
            output.set_permissions(fs::Permissions::from_mode(opened.st_mode as u32))?;
            futimens(
                &output,
                &TimeSpec::new(opened.st_atime, opened.st_atime_nsec),
                &TimeSpec::new(opened.st_mtime, opened.st_mtime_nsec),
            )?;
            output.sync_all()?;
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
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            match fs::create_dir(&target) {
                Ok(()) => {}
                Err(error)
                    if overwrite_newer && error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            copy_tree_no_links(&entry.path(), &target, None, overwrite_newer)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_file() {
            let should_copy = match fs::symlink_metadata(&target) {
                Ok(existing) if overwrite_newer && existing.is_file() => {
                    metadata.modified()? > existing.modified()?
                }
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(error.into()),
            };
            if should_copy {
                copied_file(fs::copy(entry.path(), &target)?);
                fs::set_permissions(&target, metadata.permissions())?;
                fs::File::open(&target)?.sync_all()?;
            }
        }
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            sync_tree(&entry.path())?;
        }
    }
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn retire_legacy(source: &Path) -> Result<()> {
    let parent = source.parent().context("legacy store has no parent")?;
    let quarantine = parent.join(format!(
        ".{}.v027-quarantine",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("v027 legacy source became a symlink: {}", source.display())
        }
        Ok(_) => {
            remove_tree_no_links(&quarantine)?;
            fs::rename(source, &quarantine)?;
            fs::File::open(parent)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()))
        }
    }
    remove_tree_no_links(&quarantine)?;
    fs::File::open(parent)?.sync_all()?;
    if parent.file_name().is_some_and(|name| name == "sandbox") {
        let _ = fs::remove_dir(parent);
        if let Some(grandparent) = parent.parent() {
            let _ = fs::File::open(grandparent).and_then(|dir| dir.sync_all());
        }
    }
    Ok(())
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

    /// A machine whose container runtime is absent or unreachable must still
    /// complete startup. The row stays pending and its legacy source survives,
    /// so the pass that can reach the runtime finishes the transition.
    #[test]
    #[serial_test::serial]
    fn unreachable_container_runtime_defers_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let deferred: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(
            deferred[0]["sandbox_store_generation"], 1,
            "an unreaped row must not commit the current generation"
        );
        assert!(
            home.join(".gemini/sandbox").is_dir(),
            "the legacy source must survive a deferred reap"
        );
        assert!(transition_may_be_pending(&app).unwrap());

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
        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!home.join(".gemini/sandbox").exists());
        assert!(!transition_may_be_pending(&app).unwrap());
    }

    /// `AOE_DEFER_SANDBOX_MIGRATION` must behave exactly like a live cohort: no
    /// copy, no reap, the row pending, and a later undeferred pass finishing
    /// the move. It also has to say so, since a silent deferral would look
    /// like a migration that did nothing.
    #[test]
    #[serial_test::serial]
    fn deferral_leaves_stores_pending_and_reports_it() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let pending: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();
        fs::write(app.join(".schema_version"), b"26").unwrap();

        assert!(!defer_requested_by(None));
        assert!(!defer_requested_by(Some(std::ffi::OsStr::new(""))));
        assert!(defer_requested_by(Some(std::ffi::OsStr::new("1"))));

        std::env::set_var(DEFER_ENV, "1");
        let result = super::super::run_migrations_announced(None);
        std::env::remove_var(DEFER_ENV);
        result.unwrap();

        assert_eq!(
            fs::read_to_string(app.join(".schema_version"))
                .unwrap()
                .trim(),
            "27"
        );
        assert!(!super::super::has_pending_migrations());
        assert!(transition_may_be_pending(&app).unwrap());
        assert!(home.join(".gemini/sandbox").is_dir());
        assert!(!home.join(".gemini/sandbox-v2").exists());
        let pending: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
    }

    #[test]
    #[serial_test::serial]
    fn publishes_only_after_quiescence_and_removes_transition_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        let pending: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
        assert!(app.join(JOURNAL).is_file());
        assert!(home.join(".gemini/sandbox").is_dir());

        run_in(&app, &home, &|_| Ok(false)).unwrap();
        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let source = home.join(".gemini/sandbox");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("conversation.json"), b"original").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, &|_| Ok(true)).unwrap();
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                temp.path().join("changed-gemini").display()
            ),
        )
        .unwrap();

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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::create_dir_all(custom_a.join("sandbox/one")).unwrap();
        fs::write(custom_a.join("sandbox/one/data"), b"source").unwrap();
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
        fs::create_dir_all(custom_a.join("sandbox-v2/one")).unwrap();
        fs::write(custom_a.join("sandbox-v2/one/data"), b"published").unwrap();
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
        assert_eq!(
            fs::read(custom_a.join("sandbox-v2/one/data")).unwrap(),
            b"published"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pending_absent_custom_source_still_refuses_plan_drift() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let checkpoint: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let checkpoint: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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

        let error = run_in(&app, &home, &|_| Ok(false)).unwrap_err();
        assert!(error
            .to_string()
            .contains("restore the previous session.agent_config_dir"));
        assert_eq!(
            fs::read(custom_a.join("sandbox/one/data")).unwrap(),
            b"data"
        );
        assert!(!custom_b.join("sandbox-v2/one").exists());
    }

    #[test]
    #[serial_test::serial]
    fn codex_generation_only_fast_path_moves_its_existing_private_store() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
    }

    #[test]
    #[serial_test::serial]
    fn recovers_publication_before_registry_commit() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn recovers_legacy_quarantine_before_generation_commit() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn missing_source_still_cleans_publication_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn ignores_unprovenanced_journal_paths() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true},
             "trashed_at":"2026-09-05T00:00:00Z"},
            {"id":"2222222222222222","tool":"gemini","sandbox_info":{"enabled":true},
             "archived_at":"2026-09-05T00:00:00Z"},
            {"id":"3333333333333333","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, &|_| Ok(false)).unwrap();

        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
    }

    /// Clearing `trashed_at` makes the row eligible again, and the start that
    /// follows the restore moves its store.
    /// Retirement is per root. A parked row holds its own root and nothing
    /// else: every test above asserts a source *survives*, so nothing caught
    /// a pass-wide flag suppressing retirement everywhere, which left the
    /// transition unable to finish on any machine with one archived session.
    #[test]
    #[serial_test::serial]
    fn an_unrelated_parked_row_does_not_hold_a_ready_root() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let gemini = home.join(".gemini/sandbox");
        fs::create_dir_all(&gemini).unwrap();
        fs::write(gemini.join("data"), b"g").unwrap();
        let claude = home.join(".claude/sandbox");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("data"), b"c").unwrap();
        let rows = serde_json::json!([
            {"id":"1111111111111111","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"3333333333333333","tool":"claude","sandbox_info":{"enabled":true},
             "archived_at":"2026-09-05T00:00:00Z"}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
            let rows: Value =
                serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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

        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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

        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
    fn orphan_store_is_preserved_without_contaminating_a_peer() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let root = home.join(".codex/sandbox");
        let peer = "1111111111111111";
        let orphan = "2222222222222222";
        fs::create_dir_all(root.join(peer)).unwrap();
        fs::create_dir_all(root.join(orphan)).unwrap();
        fs::write(root.join(peer).join("peer"), b"peer").unwrap();
        fs::write(root.join(orphan).join("orphan"), b"orphan").unwrap();
        fs::write(root.join("common"), b"common").unwrap();
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
            fs::read(destination.join(orphan).join("orphan")).unwrap(),
            b"orphan"
        );
        assert_eq!(
            fs::read(destination.join(peer).join("common")).unwrap(),
            b"common"
        );
        assert_eq!(
            fs::read(destination.join(orphan).join("common")).unwrap(),
            b"common"
        );
        assert!(!destination.join(peer).join(orphan).exists());
        assert!(!root.exists());
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

        let written: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
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

        publish_store(&source, &destination, &BTreeSet::new(), None, false).unwrap();

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
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
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
        publish_store(&source, &destination, &BTreeSet::new(), None, false).unwrap();
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
}
