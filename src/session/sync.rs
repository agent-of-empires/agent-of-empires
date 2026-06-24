//! Drain pollers' session-id mpsc channels and persist observations.
//!
//! Shared by the TUI tick (`apply_session_id_updates`) and the daemon's
//! `status_poll_loop`. Without the daemon-side caller, sessions running
//! under `aoe serve` without an attached TUI never persist post-`/clear`
//! sids through the channel and `sessions.json` stays stale until the
//! next launch's resume-time verify (#2291).
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
pub(crate) struct SessionIdSyncOutcome {
    /// Instances whose `agent_session_id` was updated to a poller-observed
    /// value (CAS-Applied; `resume_probe_failed_sid` is also reset).
    pub(crate) applied: Vec<String>,
    /// Instances whose in-memory state was reloaded from disk after a
    /// CAS-Skipped persist (peer wrote a different sid first).
    pub(crate) rolled_back: Vec<String>,
    /// Instances whose poller-observed sid was rejected (validation failed,
    /// matched a cleared sid in the per-instance exclusion set, or the
    /// persist returned Failed). The tmux env mirror is republished from
    /// the in-memory value for these so the on_change publish is overwritten.
    pub(crate) filtered: Vec<String>,
}

impl SessionIdSyncOutcome {
    pub(crate) fn touched(&self) -> bool {
        !self.applied.is_empty() || !self.rolled_back.is_empty() || !self.filtered.is_empty()
    }
}

struct Update {
    id: String,
    sid: String,
    expected_prior: Option<String>,
    profile: String,
}

struct Rollback {
    id: String,
    disk_sid: Option<String>,
    disk_failed_sid: Option<String>,
}

/// Drain each instance's poller channel, persist new sids via CAS, reconcile
/// in-memory state, and republish tmux env. Callers with auxiliary mirrors
/// must re-sync touched ids from the slice.
pub(crate) fn drain_and_persist_session_ids(
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
        let tmux_name = match inst.tmux_env_session_name() {
            Some(name) => name,
            None => continue,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::FileWatchService;
    use crate::session::poller::SessionPoller;
    use crate::session::storage::Storage;
    use crate::session::{GroupTree, Instance};
    use serial_test::serial;
    use std::sync::Mutex;
    use tempfile::{tempdir, TempDir};

    struct StorageHomeGuard {
        prev_home: Option<String>,
        prev_xdg: Option<String>,
    }

    impl StorageHomeGuard {
        fn set(temp: &TempDir) -> Self {
            let prev_home = std::env::var("HOME").ok();
            let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
            Self {
                prev_home,
                prev_xdg,
            }
        }
    }

    impl Drop for StorageHomeGuard {
        fn drop(&mut self) {
            restore_or_remove("HOME", self.prev_home.take());
            restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
        }
    }

    fn restore_or_remove(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    fn seed_instance_on_disk(profile: &str, inst: &Instance) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g = GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                    .get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    fn attach_poller_with_update(inst: &mut Instance, sid: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_update(&inst.id, sid);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    #[test]
    #[serial]
    fn drain_applied_updates_memory_and_clears_failed_sid() {
        let temp = tempdir().unwrap();
        let _guard = StorageHomeGuard::set(&temp);

        let profile = "sync-applied";
        let mut inst = Instance::new("sync-applied-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        inst.resume_probe_failed_sid = Some("old-failed".to_string());
        seed_instance_on_disk(profile, &inst);

        let fresh = "019342ab-1234-7def-8901-abcdef012345";
        attach_poller_with_update(&mut inst, fresh);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert!(outcome.rolled_back.is_empty());
        assert!(outcome.filtered.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(fresh));
        assert_eq!(instances[0].resume_probe_failed_sid, None);

        let storage = Storage::new_unwatched(profile).unwrap();
        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some(fresh));
        assert_eq!(loaded[0].resume_probe_failed_sid, None);
    }

    #[test]
    #[serial]
    fn drain_filters_invalid_sid_and_leaves_state_unchanged() {
        let temp = tempdir().unwrap();
        let _guard = StorageHomeGuard::set(&temp);

        let profile = "sync-filtered-validation";
        let mut inst = Instance::new("sync-validation-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some("original-sid".to_string());
        seed_instance_on_disk(profile, &inst);

        attach_poller_with_update(&mut inst, "bad sid!");

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert!(outcome.rolled_back.is_empty());
        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some("original-sid")
        );
    }

    #[test]
    #[serial]
    fn drain_filters_sid_present_in_retroactive_capture_excludes() {
        let temp = tempdir().unwrap();
        let _guard = StorageHomeGuard::set(&temp);

        let profile = "sync-filtered-excludes";
        let excluded = "019342ab-1234-7def-8901-abcdef012345";

        let mut inst = Instance::new("sync-excludes-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some("original-sid".to_string());
        inst.retroactive_capture_excludes
            .insert(excluded.to_string());
        seed_instance_on_disk(profile, &inst);

        attach_poller_with_update(&mut inst, excluded);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert!(outcome.rolled_back.is_empty());
        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some("original-sid")
        );
    }
}
