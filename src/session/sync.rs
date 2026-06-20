//! Drain pollers' session-id mpsc channels and persist observations.
//!
//! Lifted out of `tui/home/mod.rs::apply_session_id_updates` so the daemon's
//! `status_poll_loop` can run the same path. Without it, sessions running
//! under `aoe serve` without an attached TUI never persist post-`/clear`
//! sids through the channel: the TUI is the only consumer of the mpsc that
//! `SessionPoller::on_change` pushes to, leaving `sessions.json` stale until
//! the next launch's resume-time verify (#2291).
//!
//! The helper takes `&mut [Instance]` and mutates the slice's per-instance
//! `agent_session_id` and `resume_probe_failed_sid` directly. It does NOT
//! take any tokio lock and is safe to call from within `spawn_blocking`.
//! Daemon callers MUST satisfy the lock-ordering invariant in
//! `storage.rs:46`: snapshot the instances under a brief read lock, run the
//! helper on the snapshot inside `spawn_blocking`, then reapply the
//! mutations to live state under a brief write lock.

use std::collections::HashSet;
use std::sync::Arc;

use crate::file_watch::FileWatchService;
use crate::session::capture::validated_session_id;
use crate::session::storage::Storage;
use crate::session::{persist_session_to_storage, Instance, SidWrite};

/// Per-tick result of [`drain_and_persist_session_ids`]. Lists touched
/// instance IDs grouped by the persistence outcome so a caller holding an
/// auxiliary in-memory mirror (e.g. the TUI's `instance_map`) can re-sync
/// each affected entry from the slice.
#[derive(Debug, Default, Clone)]
pub struct SessionIdSyncOutcome {
    /// Instances whose `agent_session_id` was updated to a poller-observed
    /// value (CAS-Applied; `resume_probe_failed_sid` is also reset).
    pub applied: Vec<String>,
    /// Instances whose in-memory state was reloaded from disk after a
    /// CAS-Skipped persist (peer wrote a different sid first).
    pub rolled_back: Vec<String>,
    /// Instances whose poller-observed sid was rejected (validation failed,
    /// matched a cleared sid in the per-instance exclusion set, or the
    /// persist returned Failed). The tmux env mirror is republished from
    /// the in-memory value for these so the on_change publish is overwritten.
    pub filtered: Vec<String>,
}

impl SessionIdSyncOutcome {
    /// True iff at least one instance was applied, rolled back, or filtered.
    pub fn touched(&self) -> bool {
        !self.applied.is_empty() || !self.rolled_back.is_empty() || !self.filtered.is_empty()
    }
}

/// Pending CAS update derived from a poller observation.
struct Update {
    id: String,
    sid: String,
    expected_prior: Option<String>,
    profile: String,
}

/// CAS-Skipped reload: peer wrote a different sid first, so memory must
/// adopt the on-disk values rather than the poller's observation.
struct Rollback {
    id: String,
    disk_sid: Option<String>,
    disk_failed_sid: Option<String>,
}

/// Drain each instance's poller channel, persist new sids via CAS, reconcile
/// in-memory state on the slice, and republish tmux env. Mutates `instances`
/// in place; callers with auxiliary mirrors must re-sync touched ids from
/// the slice.
pub fn drain_and_persist_session_ids(
    instances: &mut [Instance],
    file_watch: &Arc<FileWatchService>,
) -> SessionIdSyncOutcome {
    let mut updates: Vec<Update> = Vec::with_capacity(instances.len());
    let mut filtered_ids: HashSet<String> = HashSet::with_capacity(instances.len());

    for inst in instances.iter() {
        let Some(sid) = try_drain_poller(inst) else {
            continue;
        };
        let Some(sid) = validated_session_id(sid) else {
            filtered_ids.insert(inst.id.clone());
            continue;
        };
        if inst.retroactive_capture_excludes.contains(&sid) {
            tracing::debug!(
                target: "session.sync",
                instance = %inst.id,
                sid = %sid,
                "Ignoring poller-reported sid: in retroactive_capture_excludes",
            );
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        if inst.agent_session_id.as_deref() != Some(sid.as_str()) {
            updates.push(Update {
                id: inst.id.clone(),
                sid,
                expected_prior: inst.agent_session_id.clone(),
                profile: inst.source_profile.clone(),
            });
        }
    }

    if updates.is_empty() && filtered_ids.is_empty() {
        return SessionIdSyncOutcome::default();
    }

    let mut to_apply: Vec<(String, String)> = Vec::with_capacity(updates.len());
    let mut to_rollback: Vec<Rollback> = Vec::with_capacity(updates.len());

    for upd in &updates {
        match persist_session_to_storage(
            &upd.profile,
            &upd.id,
            &upd.sid,
            upd.expected_prior.as_deref(),
            file_watch,
        ) {
            SidWrite::Applied => {
                to_apply.push((upd.id.clone(), upd.sid.clone()));
            }
            SidWrite::Skipped => {
                if let Some(rb) = reload_skipped_from_disk(&upd.profile, &upd.id, file_watch) {
                    to_rollback.push(rb);
                } else {
                    tracing::warn!(
                        target: "session.sync",
                        instance = %upd.id,
                        "Skipped reload failed; deferring env reconcile",
                    );
                }
            }
            SidWrite::Failed => {
                filtered_ids.insert(upd.id.clone());
            }
        }
    }

    for (id, sid) in &to_apply {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == *id) {
            inst.agent_session_id = Some(sid.clone());
            inst.resume_probe_failed_sid = None;
        }
    }
    for rb in &to_rollback {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == rb.id) {
            inst.agent_session_id = rb.disk_sid.clone();
            inst.resume_probe_failed_sid = rb.disk_failed_sid.clone();
        }
    }

    publish_tmux_env(instances, &to_apply, &to_rollback, &filtered_ids);

    SessionIdSyncOutcome {
        applied: to_apply.into_iter().map(|(id, _)| id).collect(),
        rolled_back: to_rollback.into_iter().map(|r| r.id).collect(),
        filtered: filtered_ids.into_iter().collect(),
    }
}

/// Try to drain one poller observation off the per-instance mpsc. Recovers
/// the inner guard from a poisoned mutex with a logged warning so a poison
/// (typically from a panic in another thread) does not silently freeze the
/// drain forever.
fn try_drain_poller(inst: &Instance) -> Option<String> {
    let arc = inst.session_id_poller.as_ref()?;
    let guard = match arc.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(
                target: "session.sync",
                instance = %inst.id,
                "session_id_poller mutex poisoned; recovering inner guard",
            );
            poisoned.into_inner()
        }
    };
    let (_id, sid) = guard.try_recv_session_update()?;
    Some(sid)
}

fn reload_skipped_from_disk(
    profile: &str,
    id: &str,
    file_watch: &Arc<FileWatchService>,
) -> Option<Rollback> {
    let storage = Storage::new(profile, file_watch.clone()).ok()?;
    let disk_insts = storage.load().ok()?;
    let disk_inst = disk_insts.iter().find(|i| i.id == id)?;
    Some(Rollback {
        id: id.to_string(),
        disk_sid: disk_inst.agent_session_id.clone(),
        disk_failed_sid: disk_inst.resume_probe_failed_sid.clone(),
    })
}

fn publish_tmux_env(
    instances: &[Instance],
    to_apply: &[(String, String)],
    to_rollback: &[Rollback],
    filtered_ids: &HashSet<String>,
) {
    let touched_count = to_apply.len() + to_rollback.len() + filtered_ids.len();
    let mut set_batch: Vec<(String, String, String)> = Vec::with_capacity(touched_count);
    let mut unset_batch: Vec<(String, String)> = Vec::with_capacity(touched_count);

    let touched_ids = to_apply
        .iter()
        .map(|(id, _)| id.as_str())
        .chain(to_rollback.iter().map(|r| r.id.as_str()))
        .chain(filtered_ids.iter().map(|s| s.as_str()));

    for id in touched_ids {
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            continue;
        };
        let tmux_name = match inst.tmux_session() {
            Ok(s) if s.exists() && !s.is_pane_dead() => s.name().to_string(),
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(
                    target: "session.sync",
                    instance = %id,
                    "Skipping tmux env publish; tmux_session() error: {e}",
                );
                continue;
            }
        };
        match &inst.agent_session_id {
            Some(sid) => set_batch.push((
                tmux_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                sid.clone(),
            )),
            None => unset_batch.push((
                tmux_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
            )),
        }
    }

    if !set_batch.is_empty() {
        let refs: Vec<(&str, &str, &str)> = set_batch
            .iter()
            .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
            .collect();
        if let Err(e) = crate::tmux::env::set_hidden_env_batch(&refs) {
            tracing::warn!(target: "session.sync", "Post-CAS env publish failed: {e}");
        }
    }
    if !unset_batch.is_empty() {
        let refs: Vec<(&str, &str)> = unset_batch
            .iter()
            .map(|(s, k)| (s.as_str(), k.as_str()))
            .collect();
        if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&refs) {
            tracing::warn!(target: "session.sync", "Post-CAS env unset failed: {e}");
        }
    }
}
