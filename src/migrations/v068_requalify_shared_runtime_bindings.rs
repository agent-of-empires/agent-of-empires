//! Migration v068: a Colima VM's home share now makes the paths under it host paths, so an
//! execution recorded before that, under `runtime:<kind>:<endpoint>`, no longer matches the
//! launch it describes. The proof needs the launch's runtime snapshot, so startup only stamps;
//! each launch requalifies its own row, and `aoe migrate` resolves and requalifies every row.

use crate::containers::ContainerExecutionSnapshot;
use crate::session::{ConversationProvenance, ExecutionBinding, Instance, Storage};
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    info!(target: "migrations.v068", "Rows recorded under a shared runtime are requalified at their next launch or by aoe migrate");
    Ok(())
}

/// Whether `legacy` is `now` as recorded before `container`'s runtime shared its root: the
/// same endpoint, agent and routing, with every path mapping to `now`'s host path.
fn requalifies(
    legacy: &ExecutionBinding,
    now: &ExecutionBinding,
    container: &ContainerExecutionSnapshot,
) -> bool {
    let runtime = &container.runtime;
    if runtime.shared_root.is_none() {
        return false;
    }
    let domain = format!("runtime:{:?}:{}", runtime.kind, runtime.endpoint);
    let host = |path: &Path| {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| crate::git::template::lexical_normalize(path))
    };
    legacy.filesystem == domain
        && legacy.cwd_filesystem == domain
        && now.filesystem == "host"
        && now.cwd_filesystem == "host"
        && legacy.agent == now.agent
        && legacy.exported_default_store == now.exported_default_store
        && host(&legacy.cwd) == now.cwd
        && legacy.stores.len() == now.stores.len()
        && legacy
            .stores
            .iter()
            .zip(&now.stores)
            .all(|(legacy, now)| host(legacy) == *now)
        && legacy.configuration.len() == now.configuration.len()
        && legacy
            .configuration
            .iter()
            .zip(&now.configuration)
            .all(|(legacy, now)| {
                legacy == now
                    || (legacy.filesystem == domain
                        && now.filesystem == "host"
                        && host(&legacy.path) == now.path)
            })
}

/// The asserted conversation bindings and the active execution: the executions a launch or
/// `set-session-id` reuses.
fn recorded(row: &Instance) -> impl Iterator<Item = &ExecutionBinding> {
    [
        row.agent_session_binding.as_ref(),
        row.resume_binding.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter(|binding| binding.provenance == ConversationProvenance::Asserted)
    .filter_map(|binding| binding.execution.as_ref())
    .chain(row.active_execution.as_ref().map(|active| &active.binding))
}

/// Whether the session is pending and every recorded execution either requalifies or is
/// already `now`. One that does neither leaves the whole session as it is, so its launch fails
/// with nothing written and its bindings never disagree.
fn proven(row: &Instance, now: &ExecutionBinding, container: &ContainerExecutionSnapshot) -> bool {
    let mut pending = false;
    for execution in recorded(row) {
        if requalifies(execution, now, container) {
            pending = true;
        } else if execution != now {
            return false;
        }
    }
    pending
}

fn requalify_row(
    row: &mut Instance,
    now: &ExecutionBinding,
    container: &ContainerExecutionSnapshot,
) -> bool {
    if !proven(row, now, container) {
        return false;
    }
    let conversation = [
        row.agent_session_binding.as_mut(),
        row.resume_binding.as_mut(),
    ]
    .into_iter()
    .flatten()
    .filter(|binding| binding.provenance == ConversationProvenance::Asserted)
    .filter_map(|binding| binding.execution.as_mut());
    for execution in conversation.chain(
        row.active_execution
            .as_mut()
            .map(|active| &mut active.binding),
    ) {
        *execution = now.clone();
    }
    true
}

/// Requalify `inst` for the launch resolving `now` in `container`. A launch with its store
/// backs up the sessions file and rewrites its stored row through that store, under the
/// launch's own lock. A session with nothing proven does no I/O.
pub(crate) fn requalify_launch(
    inst: &mut Instance,
    stores: crate::session::CaptureStorage<'_>,
    now: &ExecutionBinding,
    container: &ContainerExecutionSnapshot,
) -> Result<()> {
    if !proven(inst, now, container) {
        return Ok(());
    }
    let mut stored = false;
    if let crate::session::CaptureStorage::Scoped(store) = stores {
        crate::session::backup_before_migration(store.storage().sessions_path())?;
        let id = inst.id.clone();
        stored = store.update(|rows, _| {
            Ok(rows
                .iter_mut()
                .find(|row| row.id == id)
                .is_some_and(|row| requalify_row(row, now, container)))
        })?;
    }
    requalify_row(inst, now, container);
    info!(target: "migrations.v068", id = inst.id, stored, "Requalified executions recorded under a shared runtime");
    Ok(())
}

/// `aoe migrate`'s bulk path. A row whose launch context doesn't resolve now, such as one whose
/// runtime is down, stays pending for its next launch.
pub(crate) fn reconcile_pending(explicit_migrate: bool) -> Result<()> {
    if !explicit_migrate {
        return Ok(());
    }
    for profile in crate::session::list_profiles()? {
        let storage = Storage::open_unwatched(&profile)?;
        // A component that doesn't decode keeps its bytes for repair; its rows wait for it.
        let rows = match storage.load_instances_complete() {
            Ok(rows) => rows,
            Err(error) => {
                info!(target: "migrations.v068", profile, error = %format!("{error:#}"), "Left a profile pending; its sessions do not load");
                continue;
            }
        };
        let mut requalifiable = Vec::new();
        for row in rows {
            if !row.is_sandboxed() {
                continue;
            }
            match row.current_native_execution() {
                Ok((now, Some(container))) if proven(&row, &now, &container) => {
                    requalifiable.push((row.id.clone(), now, container));
                }
                Ok(_) => {}
                Err(error) => {
                    info!(target: "migrations.v068", profile, id = row.id, error = %format!("{error:#}"), "Left a row pending; its launch context does not resolve");
                }
            }
        }
        if requalifiable.is_empty() {
            continue;
        }
        crate::session::backup_before_migration(storage.sessions_path())?;
        let requalified = storage.update_complete(|rows, _| {
            Ok(rows
                .iter_mut()
                .filter_map(|row| {
                    let (_, now, container) =
                        requalifiable.iter().find(|(id, ..)| *id == row.id)?;
                    Some(requalify_row(row, now, container))
                })
                .filter(|requalified| *requalified)
                .count())
        })?;
        info!(target: "migrations.v068", profile, requalified, "Requalified executions recorded under a shared runtime");
    }
    Ok(())
}
