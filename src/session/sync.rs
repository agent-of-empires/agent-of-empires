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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::file_watch::FileWatchService;
use crate::session::capture::validated_session_id;
use crate::session::storage::Storage;
use crate::session::{persist_session_to_storage, Instance, ResumeIntent, SidWrite, Status};

/// Per-tick result of [`drain_and_persist_session_ids`]. Lists touched
/// instance IDs grouped by the persistence outcome so a caller holding an
/// auxiliary in-memory mirror (e.g. the TUI's `instances` map) can re-sync
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

    // Frozen pre-update ownership snapshot. Collision checks must read this,
    // never a map mutated mid-loop: with two pollers that transiently cross
    // streams (A reports B's id while B reports A's), a dynamic map would
    // accept or reject by slice iteration order. The snapshot rejects every
    // cross-claim deterministically (see #2708).
    let mut sid_owners: HashMap<String, String> = HashMap::with_capacity(instances.len());
    for inst in instances.iter() {
        if let Some(sid) = inst.agent_session_id.as_deref() {
            sid_owners
                .entry(sid.to_string())
                .or_insert_with(|| inst.id.clone());
        }
    }
    for inst in instances.iter() {
        let Some(sid) = try_drain_poller(inst) else {
            continue;
        };
        let Some(sid) = validated_session_id(sid) else {
            filtered_ids.insert(inst.id.clone());
            continue;
        };
        // A stopped session generates no live transcript activity, so any sid
        // its poller reports that isn't already its own belongs to a different
        // session sharing the cwd. Never adopt it (#2708 invariant 2).
        if matches!(inst.status, Status::Stopped)
            && inst.agent_session_id.as_deref() != Some(sid.as_str())
        {
            tracing::debug!(
                target: "session.sync",
                instance = %inst.id,
                sid = %sid,
                "Ignoring poller-reported sid for stopped session",
            );
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        // An explicit set-session-id pin is authoritative until the session
        // itself launches (which promotes Use -> Default). While pinned, the
        // poller must not overwrite it, even with an unowned fresher jsonl the
        // collision guard below would otherwise wave through (#2708 invariant 1).
        if let ResumeIntent::Use(pinned) = &inst.resume_intent {
            if sid != *pinned {
                tracing::debug!(
                    target: "session.sync",
                    instance = %inst.id,
                    sid = %sid,
                    pinned = %pinned,
                    "Ignoring poller-reported sid: contradicts explicit set-session-id pin",
                );
                filtered_ids.insert(inst.id.clone());
                continue;
            }
        }
        // Never adopt an id another instance already owns: that is the
        // same-cwd cross-assignment drift itself (#2708 symptom 1).
        if let Some(owner) = sid_owners.get(sid.as_str()) {
            if owner != &inst.id {
                tracing::warn!(
                    target: "session.sync",
                    instance = %inst.id,
                    sid = %sid,
                    owner = %owner,
                    "Ignoring poller-reported sid already owned by another instance",
                );
                filtered_ids.insert(inst.id.clone());
                continue;
            }
        }
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

    // Reject, don't arbitrate: if two same-cwd peers both claim the same
    // currently-unowned sid in one tick (neither is in the frozen snapshot, so
    // the collision guard passed both), picking a winner by iteration order is
    // silent misassignment. Drop every claimant and defer; the next tick sees
    // the real owner's anchor advance and the collision guard resolves it (#2708).
    let mut sid_claim_counts: HashMap<String, usize> = HashMap::with_capacity(updates.len());
    for upd in &updates {
        *sid_claim_counts.entry(upd.sid.clone()).or_insert(0) += 1;
    }
    updates.retain(|upd| {
        if sid_claim_counts.get(&upd.sid).copied().unwrap_or(0) > 1 {
            tracing::warn!(
                target: "session.sync",
                instance = %upd.id,
                sid = %upd.sid,
                "Ignoring poller-reported sid claimed by multiple instances this tick",
            );
            filtered_ids.insert(upd.id.clone());
            false
        } else {
            true
        }
    });

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
        // Re-assert the instance-id alongside the captured sid: this publish
        // replaced the poller's on_change pre-CAS publish (which wrote both
        // keys), and `build_exclusion_set` can only attribute a captured sid
        // to its owner when AOE_INSTANCE_ID is present on the same session.
        set_batch.push((
            tmux_name.clone(),
            crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
            inst.id.clone(),
        ));
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
    use crate::session::test_support::EnvGuard;
    use crate::session::{GroupTree, Instance};
    use serial_test::serial;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::{tempdir, TempDir};

    /// Points `HOME` (and, on Linux/macOS, `XDG_CONFIG_HOME`) at `temp`
    /// for the current test body. See [`crate::session::test_support`]:
    /// the snapshot/restore is `EnvGuard`'s, so a non-UTF-8 prior value
    /// round-trips instead of being dropped (#2751).
    fn storage_home_guard(temp: &TempDir) -> EnvGuard {
        #[allow(unused_mut)]
        let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
        EnvGuard::set(&pairs)
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

    fn seed_instances_on_disk(profile: &str, insts: &[&Instance]) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let owned: Vec<Instance> = insts.iter().map(|i| (*i).clone()).collect();
        storage
            .update(|i, g| {
                *i = owned.clone();
                *g = GroupTree::new_with_groups(&owned, &[]).get_all_groups();
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
        let _guard = storage_home_guard(&temp);

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
        let _guard = storage_home_guard(&temp);

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
        let _guard = storage_home_guard(&temp);

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

    #[test]
    #[serial]
    fn drain_rejects_observed_sid_for_stopped_session() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let own = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let peer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let mut inst = Instance::new("stopped-title", "/tmp/x");
        inst.source_profile = "sync-stopped".to_string();
        inst.agent_session_id = Some(own.to_string());
        inst.status = Status::Stopped;
        seed_instances_on_disk("sync-stopped", &[&inst]);

        attach_poller_with_update(&mut inst, peer);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(own));
    }

    #[test]
    #[serial]
    fn drain_rejects_observed_sid_contradicting_use_pin() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let pin = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let peer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let mut inst = Instance::new("pinned-title", "/tmp/x");
        inst.source_profile = "sync-pinned".to_string();
        inst.agent_session_id = Some(pin.to_string());
        inst.resume_intent = ResumeIntent::Use(pin.to_string());
        // Idle (Instance::new default), so the stopped guard does not fire and
        // the pin guard is what rejects the peer id.
        seed_instances_on_disk("sync-pinned", &[&inst]);

        attach_poller_with_update(&mut inst, peer);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(pin));
    }

    #[test]
    #[serial]
    fn drain_rejects_sid_owned_by_another_instance() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let owned = "019342ab-1234-7def-8901-cccccccccccc";
        let mut owner = Instance::new("owner-title", "/tmp/x");
        owner.source_profile = "sync-collision".to_string();
        owner.agent_session_id = Some(owned.to_string());

        let mut thief = Instance::new("thief-title", "/tmp/x");
        thief.source_profile = "sync-collision".to_string();
        thief.agent_session_id = None;
        seed_instances_on_disk("sync-collision", &[&owner, &thief]);
        attach_poller_with_update(&mut thief, owned);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![owner, thief];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[1].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(owned));
        assert_eq!(instances[1].agent_session_id, None);
    }

    #[test]
    #[serial]
    fn drain_rejects_sid_owned_on_disk_by_unseen_peer() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        // Cross-process shape (#2858): another aoe process (e.g. the serve
        // daemon, while this process is the TUI) has already assigned
        // `contested` to a peer ON DISK, but this process's in-memory slice
        // predates that write, so every in-memory guard waves the claim
        // through. The flock-scoped ownership check inside
        // `persist_session_to_storage` must reject the write and the drain
        // must roll the claimant back to its disk value.
        let contested = "019342ab-1234-7def-8901-eeeeeeeeeeee";
        let profile = "sync-diskowner";

        let mut owner = Instance::new("disk-owner-title", "/tmp/x");
        owner.source_profile = profile.to_string();
        owner.agent_session_id = Some(contested.to_string());

        let mut claimant = Instance::new("claimant-title", "/tmp/x");
        claimant.source_profile = profile.to_string();
        claimant.agent_session_id = None;
        seed_instances_on_disk(profile, &[&owner, &claimant]);

        attach_poller_with_update(&mut claimant, contested);

        let file_watch = FileWatchService::noop();
        // The slice deliberately omits `owner`: its assignment exists only on
        // disk, as after a concurrent process's drain.
        let mut instances = vec![claimant];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.rolled_back, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id, None);

        let storage = Storage::new_unwatched(profile).unwrap();
        let loaded = storage.load().unwrap();
        let disk_owner = loaded
            .iter()
            .find(|i| i.title == "disk-owner-title")
            .unwrap();
        assert_eq!(disk_owner.agent_session_id.as_deref(), Some(contested));
        let disk_claimant = loaded.iter().find(|i| i.title == "claimant-title").unwrap();
        assert_eq!(
            disk_claimant.agent_session_id, None,
            "claimant must not adopt a sid a disk peer already owns"
        );
    }

    #[test]
    #[serial]
    fn drain_rejects_all_claimants_of_same_batch_duplicate_sid() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let contested = "019342ab-1234-7def-8901-dddddddddddd";
        let mut a = Instance::new("peer-a-title", "/tmp/x");
        a.source_profile = "sync-samebatch".to_string();
        a.agent_session_id = None;
        attach_poller_with_update(&mut a, contested);

        let mut b = Instance::new("peer-b-title", "/tmp/x");
        b.source_profile = "sync-samebatch".to_string();
        b.agent_session_id = None;
        seed_instances_on_disk("sync-samebatch", &[&a, &b]);
        attach_poller_with_update(&mut b, contested);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![a, b];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert!(outcome.applied.is_empty());
        assert!(outcome.filtered.contains(&instances[0].id));
        assert!(outcome.filtered.contains(&instances[1].id));
        assert_eq!(instances[0].agent_session_id, None);
        assert_eq!(instances[1].agent_session_id, None);
    }
}
