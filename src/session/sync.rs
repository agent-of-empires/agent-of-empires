//! Drain pollers' session-id mpsc channels and persist observations.
//!
//! Lifted out of `tui/home/mod.rs::apply_session_id_updates` so the daemon's
//! `status_poll_loop` can run the same path. Without it, sessions running
//! under `aoe serve` without an attached TUI never persist post-`/clear`
//! sids through the channel: the TUI is the only consumer of the mpsc that
//! `SessionPoller::on_change` pushes to, leaving `sessions.json` stale until
//! the next launch's resume-time verify (#2291).

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
    pub fn touched(&self) -> bool {
        !self.applied.is_empty() || !self.rolled_back.is_empty() || !self.filtered.is_empty()
    }
}

/// Drain each instance's poller channel, persist new sids via CAS, reconcile
/// in-memory state on the slice, and republish tmux env. Mutates `instances`
/// in place; callers with auxiliary mirrors must re-sync touched ids from
/// the slice.
pub fn drain_and_persist_session_ids(
    instances: &mut [Instance],
    file_watch: &Arc<FileWatchService>,
) -> SessionIdSyncOutcome {
    let mut updates: Vec<(String, String, Option<String>)> = Vec::new();
    let mut filtered_ids: HashSet<String> = HashSet::new();

    for inst in instances.iter() {
        let Some((_id, session_id)) = inst
            .session_id_poller
            .as_ref()
            .and_then(|p| p.lock().ok())
            .and_then(|p| p.try_recv_session_update())
        else {
            continue;
        };
        let Some(session_id) = validated_session_id(session_id) else {
            filtered_ids.insert(inst.id.clone());
            continue;
        };
        if inst.retroactive_capture_excludes.contains(&session_id) {
            tracing::debug!(
                target: "session.sync",
                "Ignoring poller-reported sid {} for {}: in retroactive_capture_excludes",
                session_id,
                inst.id,
            );
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        if inst.agent_session_id.as_deref() != Some(session_id.as_str()) {
            let expected_prior = inst.agent_session_id.clone();
            updates.push((inst.id.clone(), session_id, expected_prior));
        }
    }

    if updates.is_empty() && filtered_ids.is_empty() {
        return SessionIdSyncOutcome::default();
    }

    let mut to_apply: Vec<(String, String)> = Vec::new();
    let mut to_rollback: Vec<(String, Option<String>, Option<String>)> = Vec::new();

    for (id, session_id, expected_prior) in &updates {
        let Some(profile) = instances
            .iter()
            .find(|i| i.id == *id)
            .map(|i| i.source_profile.clone())
        else {
            continue;
        };
        match persist_session_to_storage(
            &profile,
            id,
            session_id,
            expected_prior.as_deref(),
            file_watch,
        ) {
            SidWrite::Applied => {
                to_apply.push((id.clone(), session_id.clone()));
            }
            SidWrite::Skipped => {
                let mut reloaded = false;
                if let Ok(storage) = Storage::new(&profile, file_watch.clone()) {
                    if let Ok(disk_insts) = storage.load() {
                        if let Some(disk_inst) = disk_insts.iter().find(|i| i.id == *id) {
                            to_rollback.push((
                                id.clone(),
                                disk_inst.agent_session_id.clone(),
                                disk_inst.resume_probe_failed_sid.clone(),
                            ));
                            reloaded = true;
                        }
                    }
                }
                if !reloaded {
                    tracing::warn!(
                        target: "session.sync",
                        instance = %id,
                        "Skipped reload failed; deferring env reconcile"
                    );
                }
            }
            SidWrite::Failed => {
                filtered_ids.insert(id.clone());
            }
        }
    }

    for (id, session_id) in &to_apply {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == *id) {
            inst.agent_session_id = Some(session_id.clone());
            inst.resume_probe_failed_sid = None;
        }
    }
    for (id, disk_sid, disk_failed_sid) in &to_rollback {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == *id) {
            inst.agent_session_id = disk_sid.clone();
            inst.resume_probe_failed_sid = disk_failed_sid.clone();
        }
    }

    publish_tmux_env(instances, &to_apply, &to_rollback, &filtered_ids);

    SessionIdSyncOutcome {
        applied: to_apply.into_iter().map(|(id, _)| id).collect(),
        rolled_back: to_rollback.into_iter().map(|(id, _, _)| id).collect(),
        filtered: filtered_ids.into_iter().collect(),
    }
}

fn publish_tmux_env(
    instances: &[Instance],
    to_apply: &[(String, String)],
    to_rollback: &[(String, Option<String>, Option<String>)],
    filtered_ids: &HashSet<String>,
) {
    let touched_ids: Vec<&str> = to_apply
        .iter()
        .map(|(id, _)| id.as_str())
        .chain(to_rollback.iter().map(|(id, _, _)| id.as_str()))
        .chain(filtered_ids.iter().map(|s| s.as_str()))
        .collect();
    let mut set_batch: Vec<(String, String, String)> = Vec::new();
    let mut unset_batch: Vec<(String, String)> = Vec::new();
    for id in &touched_ids {
        let Some(inst) = instances.iter().find(|i| i.id == **id) else {
            continue;
        };
        let tmux_name = match inst.tmux_session() {
            Ok(s) if s.exists() && !s.is_pane_dead() => s.name().to_string(),
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(
                    target: "session.sync",
                    instance = %id,
                    "Skipping tmux env publish; tmux_session() error: {}",
                    e
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
            tracing::warn!(target: "session.sync", "Post-CAS env publish failed: {}", e);
        }
    }
    if !unset_batch.is_empty() {
        let refs: Vec<(&str, &str)> = unset_batch
            .iter()
            .map(|(s, k)| (s.as_str(), k.as_str()))
            .collect();
        if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&refs) {
            tracing::warn!(target: "session.sync", "Post-CAS env unset failed: {}", e);
        }
    }
}
