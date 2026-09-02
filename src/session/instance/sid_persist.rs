//! CAS-guarded persistence of `agent_session_id` and `resume_intent`.

use super::*;

/// Outcome of a CAS-guarded `agent_session_id` or `resume_intent` write.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidWrite {
    /// Disk matched `expected_prior`; new value committed.
    Applied,
    /// Disk diverged (peer wrote between caller's read and this write);
    /// caller should reload the in-memory mirror from disk.
    Skipped,
    /// I/O failure or row gone from disk; in-memory mirror is unchanged.
    Failed,
}

/// Caller contract for `persist_session_id`: whether to publish the
/// post-CAS `agent_session_id` to the tmux hidden env.
///
/// `Published`: memory reflects disk (Applied: just committed; Skipped:
/// reloaded). Caller publishes.
/// `Skip`: memory unchanged on invalid sid, storage error, or row gone.
/// Caller must not touch env.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidPersistOutcome {
    Published,
    Skip,
}

/// Find another on-disk row that already holds `sid`.
///
/// Must be evaluated inside a `Storage::update` closure — i.e. under the
/// cross-process storage flock — so the answer reflects writes made by
/// concurrent aoe processes. The drain guards in `sync.rs` enforce the same
/// ownership rule, but only against the calling process's in-memory snapshot:
/// with a TUI and a serve daemon each draining their own pollers, one
/// process can assign a sid to instance A while the other's stale snapshot
/// still sees it unowned and hands it to instance B — both per-instance CAS
/// checks pass and disk ends up with a duplicate. This flock-scoped re-check
/// is the authoritative backstop (#2858).
pub(super) fn foreign_sid_holder<'a>(
    instances: &'a [Instance],
    instance_id: &str,
    sid: &str,
) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.id != instance_id && i.agent_session_id.as_deref() == Some(sid))
}

/// CAS-write `agent_session_id` to disk. Caller passes the value the
/// in-memory mirror held at last reconcile as `expected_prior`; the closure
/// inside `Storage::update`'s flock skips the write if disk has diverged
/// (peer-poller observed a different sid). On Skipped, callers should
/// reload memory from disk to converge on the peer's value.
///
/// Beyond the per-instance CAS, the closure rejects (as `Skipped`) any write
/// that would violate a disk-level invariant a concurrent process may have
/// established since the caller's snapshot (#2858):
/// - the sid is already owned by another instance on disk;
/// - the target row carries an on-disk `set-session-id` pin
///   (`ResumeIntent::Use`) that the sid contradicts.
pub(crate) fn persist_session_to_storage(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    persist_session_to_storage_guarded(
        profile,
        instance_id,
        session_id,
        expected_prior,
        false,
        None,
        file_watch,
    )
}

pub(super) fn persist_session_to_storage_guarded(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    guard_generation: bool,
    expected_generation: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    if !is_valid_session_id(session_id) {
        tracing::warn!(target: "session.store",
            "Refusing to persist invalid session ID {:?} for {}",
            session_id,
            instance_id
        );
        return SidWrite::Failed;
    }

    let storage = match crate::session::storage::Storage::new(profile, file_watch.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "session.store", "Failed to create storage for session ID persistence: {}", e);
            return SidWrite::Failed;
        }
    };

    let outcome = storage.update(|instances, _groups| {
        if !instances.iter().any(|i| i.id == instance_id) {
            return Ok(SidWrite::Failed);
        }
        if let Some(holder) = foreign_sid_holder(instances, instance_id, session_id) {
            tracing::warn!(target: "session.store",
                instance_id = %instance_id,
                sid = %session_id,
                holder = %holder.id,
                "sid write rejected under flock: already owned by another instance"
            );
            return Ok(SidWrite::Skipped);
        }
        if let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) {
            if let ResumeIntent::Use(pinned) = &inst.resume_intent {
                if pinned != session_id {
                    tracing::warn!(target: "session.store",
                        instance_id = %instance_id,
                        sid = %session_id,
                        pinned = %pinned,
                        "sid write rejected under flock: contradicts on-disk set-session-id pin"
                    );
                    return Ok(SidWrite::Skipped);
                }
            }
            if guard_generation && inst.omp_capture_generation.as_deref() != expected_generation {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_generation = ?expected_generation,
                    disk_generation = ?inst.omp_capture_generation,
                    "OMP generation CAS mismatch; skipping sid persist"
                );
                return Ok(SidWrite::Skipped);
            }
            if inst.agent_session_id.as_deref() != expected_prior {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected = ?expected_prior,
                    disk = ?inst.agent_session_id,
                    target = session_id,
                    "sid CAS mismatch; skipping persist"
                );
                return Ok(SidWrite::Skipped);
            }
            inst.agent_session_id = Some(session_id.to_string());
            inst.resume_probe_failed_sid = None;
            Ok(SidWrite::Applied)
        } else {
            Ok(SidWrite::Failed)
        }
    });

    match outcome {
        Ok(SidWrite::Applied) => {
            tracing::debug!(target: "session.store", "Session ID persisted for {}", instance_id);
            SidWrite::Applied
        }
        Ok(other) => other,
        Err(e) => {
            tracing::warn!(target: "session.store", "Failed to persist session ID for {}: {}", instance_id, e);
            SidWrite::Failed
        }
    }
}

/// Emit `fresh` only when it differs from the stored session id, the
/// "override only when distinct" contract shared by both branches of
/// `capture_freshest_session_id` (sidecar and mtime fallback).
pub(super) fn override_if_distinct(stored: Option<&str>, fresh: String) -> Option<String> {
    match stored {
        Some(known) if known == fresh => None,
        _ => Some(fresh),
    }
}

impl Instance {
    /// Consume an explicit OMP resume pin only after the matching launch
    /// reports the already-durable sid. All three facts are checked under the
    /// storage flock so a concurrent re-pin or relaunch cannot be consumed.
    pub(crate) fn persist_omp_pin_confirmation(
        profile: &str,
        instance_id: &str,
        session_id: &str,
        generation: &str,
        file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> SidWrite {
        if !is_valid_session_id(session_id) {
            return SidWrite::Failed;
        }
        let storage = match crate::session::storage::Storage::new(profile, file_watch.clone()) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(target: "session.store",
                    instance = %instance_id,
                    "Failed to open storage for OMP pin confirmation: {error}"
                );
                return SidWrite::Failed;
            }
        };
        let outcome = storage.update(|instances, _groups| {
            let Some(instance) = instances
                .iter_mut()
                .find(|instance| instance.id == instance_id)
            else {
                return Ok(SidWrite::Failed);
            };
            let intent_matches = matches!(
                &instance.resume_intent,
                ResumeIntent::Use(pinned) if pinned == session_id
            );
            if instance.agent_session_id.as_deref() != Some(session_id)
                || !intent_matches
                || instance.omp_capture_generation.as_deref() != Some(generation)
            {
                tracing::warn!(target: "session.store",
                    instance = %instance_id,
                    sid = %session_id,
                    generation = %generation,
                    disk_sid = ?instance.agent_session_id,
                    disk_intent = ?instance.resume_intent,
                    disk_generation = ?instance.omp_capture_generation,
                    "OMP pin confirmation CAS mismatch"
                );
                return Ok(SidWrite::Skipped);
            }
            instance.resume_intent = ResumeIntent::Default;
            Ok(SidWrite::Applied)
        });
        match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(target: "session.store",
                    instance = %instance_id,
                    "Failed to persist OMP pin confirmation: {error}"
                );
                SidWrite::Failed
            }
        }
    }

    pub(super) fn persist_session_id(
        &mut self,
        profile: &str,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
    ) -> SidPersistOutcome {
        let new_sid = self.agent_session_id.clone();

        if let Some(ref sid) = new_sid {
            if !is_valid_session_id(sid) {
                tracing::warn!(target: "session.store",
                    "Refusing to persist invalid session ID {:?} for {}",
                    sid,
                    self.id
                );
                return SidPersistOutcome::Skip;
            }
        }

        let storage =
            match crate::session::storage::Storage::new(profile, self.resolve_file_watch()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Failed to create storage for finalize-launch persist for {}: {}",
                        self.id,
                        e
                    );
                    return SidPersistOutcome::Skip;
                }
            };

        self.persist_session_id_with_storage(&storage, expected_prior_sid, expected_prior_intent)
    }

    fn persist_session_id_with_storage(
        &mut self,
        storage: &crate::session::storage::Storage,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
    ) -> SidPersistOutcome {
        let new_sid = self.agent_session_id.clone();
        // Cleared and Fork are one-shot launch directives. Use stays durable
        // only when no pane-scoped capture backend can observe a later `/new`;
        // capture-backed agents hand ownership back to their poller.
        let promote_one_shot = matches!(
            expected_prior_intent,
            ResumeIntent::Cleared | ResumeIntent::Fork { .. }
        ) || matches!(expected_prior_intent, ResumeIntent::Use(_))
            && self.launch_has_session_publisher();
        // OMP seeds its poller with the in-memory sid. Keeping the pin there
        // would suppress the first equal observation, which is the launch's
        // confirmation. Leave the durable sid intact but make this launch's
        // generation report it back through the guarded sync path.
        let await_omp_pin_confirmation = matches!(
            (&expected_prior_intent, new_sid.as_deref()),
            (ResumeIntent::Use(pinned), Some(sid)) if pinned == sid
        ) && self.resolved_capture_backend()
            == Some(crate::agents::SessionCaptureBackend::Omp)
            && self
                .omp_capture_generation
                .as_deref()
                .is_some_and(|generation| !generation.starts_with("tombstone-"));

        let instance_id = self.id.clone();
        let new_sid_for_closure = new_sid.clone();
        let expected_prior_intent_for_closure = expected_prior_intent.clone();
        let mut cleared_holder_ids: Vec<String> = Vec::new();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            if inst.agent_session_id.as_deref() != expected_prior_sid {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = ?expected_prior_sid,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in finalize persist; skipping both writes"
                );
                return Ok(SidWrite::Skipped);
            }

            // Disk-level ownership guard, mirrored from
            // `persist_session_to_storage` (see `foreign_sid_holder`): a
            // concurrent process may have assigned this sid to a peer since
            // the caller's snapshot. One exception: a launch that consumes an
            // explicit `set-session-id` pin for exactly this sid. The pin is
            // authoritative (#2708), and it is also the documented repair for
            // an existing duplicate — so instead of rejecting, the pinned
            // launch takes ownership and every stale holder is relieved of
            // the sid (their next capture re-establishes their own
            // conversations). The takeover requires the pin to still be
            // present on the target's on-disk row, not just in the caller's
            // pre-launch snapshot: a peer process may have re-pinned or
            // cleared the intent since, and a stale snapshot must not
            // authorize an ownership transfer the current disk state no
            // longer sanctions.
            if let Some(sid) = new_sid_for_closure.as_deref() {
                let consumed_pin = matches!(
                    &expected_prior_intent_for_closure,
                    ResumeIntent::Use(pinned) if pinned == sid
                ) && matches!(
                    &inst.resume_intent,
                    ResumeIntent::Use(pinned) if pinned == sid
                );
                let holder_ids: Vec<String> = instances
                    .iter()
                    .filter(|i| i.id != instance_id && i.agent_session_id.as_deref() == Some(sid))
                    .map(|i| i.id.clone())
                    .collect();
                if !holder_ids.is_empty() {
                    if consumed_pin {
                        for holder_id in &holder_ids {
                            tracing::warn!(target: "session.store",
                                instance_id = %instance_id,
                                sid = %sid,
                                holder = %holder_id,
                                "explicit pin consumed at launch: taking sid ownership from stale holder"
                            );
                            if let Some(holder) =
                                instances.iter_mut().find(|i| &i.id == holder_id)
                            {
                                holder.agent_session_id = None;
                                holder.resume_probe_failed_sid = None;
                            }
                        }
                        cleared_holder_ids = holder_ids;
                    } else {
                        tracing::warn!(target: "session.store",
                            instance_id = %instance_id,
                            sid = %sid,
                            holder = %holder_ids[0],
                            "sid write rejected under flock in finalize persist: already owned by another instance"
                        );
                        return Ok(SidWrite::Skipped);
                    }
                }
            }

            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };
            inst.agent_session_id = new_sid_for_closure.clone();
            inst.resume_probe_failed_sid = None;

            if promote_one_shot {
                if inst.resume_intent == expected_prior_intent_for_closure {
                    inst.resume_intent = ResumeIntent::Default;
                } else {
                    tracing::warn!(target: "session.store",
                        instance_id = %instance_id,
                        expected_intent = ?expected_prior_intent_for_closure,
                        disk_intent = ?inst.resume_intent,
                        "resume_intent CAS mismatch in finalize persist; sid persisted but intent left as peer wrote it"
                    );
                }
            }

            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(SidWrite::Applied) => {
                // Outside the flock: a live cleared holder may still advertise
                // the taken sid via AOE_CAPTURED_SESSION_ID, which
                // `build_exclusion_set` treats as ownership truth, so the new
                // owner would exclude its own sid until the holder's next
                // capture republishes. Unset it best-effort; a holder with no
                // tmux session (stopped) has no env to poison.
                for holder_id in &cleared_holder_ids {
                    let Some(tmux_name) = tmux_env_session_name_for_instance_id(holder_id) else {
                        continue;
                    };
                    if let Err(e) = crate::tmux::env::remove_hidden_env(
                        &tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                    ) {
                        tracing::warn!(target: "session.store",
                            holder = %holder_id,
                            "Failed to clear taken sid from stale holder's tmux env: {e}");
                    }
                }
                self.resume_probe_failed_sid = None;
                if promote_one_shot {
                    if let Ok(insts) = storage.load() {
                        if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                            self.resume_intent = disk.resume_intent;
                            self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        }
                    }
                }
                if await_omp_pin_confirmation {
                    self.agent_session_id = None;
                }
                SidPersistOutcome::Published
            }
            Ok(SidWrite::Skipped) => match storage.load() {
                Ok(insts) => match insts.into_iter().find(|i| i.id == self.id) {
                    Some(disk) => {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        let disk_still_awaits_confirmation = matches!(
                            (&self.resume_intent, self.agent_session_id.as_deref()),
                            (ResumeIntent::Use(pinned), Some(sid)) if pinned == sid
                        );
                        if await_omp_pin_confirmation && disk_still_awaits_confirmation {
                            self.agent_session_id = None;
                        }
                        SidPersistOutcome::Published
                    }
                    None => {
                        tracing::warn!(target: "session.store",
                            "Skipped reload found no row for {}; leaving memory and env untouched",
                            self.id
                        );
                        SidPersistOutcome::Skip
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Skipped reload failed for {}: {}; leaving memory and env untouched",
                        self.id, e
                    );
                    SidPersistOutcome::Skip
                }
            },
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Finalize persist found no instance row for {}",
                    self.id
                );
                SidPersistOutcome::Skip
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to persist session state for {}: {}",
                    self.id,
                    e
                );
                SidPersistOutcome::Skip
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn persist_session_to_storage_skips_on_cas_mismatch() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("cas-persist-mismatch").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.agent_session_id = Some("peer-wrote".to_string());
        let id = inst.id.clone();
        let xs = vec![inst];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let outcome = super::persist_session_to_storage(
            "cas-persist-mismatch",
            &id,
            "ours",
            Some("old"),
            &crate::file_watch::FileWatchService::noop(),
        );
        assert_eq!(outcome, super::SidWrite::Skipped);

        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some("peer-wrote"));
    }

    #[test]
    #[serial]
    fn persist_session_to_storage_writes_on_cas_match() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("cas-persist-match").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.agent_session_id = Some("old".to_string());
        let id = inst.id.clone();
        let xs = vec![inst];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let outcome = super::persist_session_to_storage(
            "cas-persist-match",
            &id,
            "new",
            Some("old"),
            &crate::file_watch::FileWatchService::noop(),
        );
        assert_eq!(outcome, super::SidWrite::Applied);

        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some("new"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn persist_session_to_storage_delivers_notification_to_in_process_subscriber() {
        use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        // Seed via a noop service so the seed write produces no Local
        // notification on the live service constructed below; the
        // subscriber attaches AFTER the seed so any seed-side kernel
        // echo is filtered out by the subscribe boundary.
        let seed_storage =
            crate::session::storage::Storage::new_unwatched("sid-persist-notify").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.agent_session_id = Some("old".to_string());
        let id = inst.id.clone();
        let on_disk = vec![inst.clone()];
        seed_storage
            .update(|i, g| {
                *i = on_disk.clone();
                *g = crate::session::GroupTree::new_with_groups(&on_disk, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
        drop(seed_storage);

        let svc: Arc<FileWatchService> = FileWatchService::new().expect("init");
        let profile_dir = crate::session::get_profile_dir_path("sid-persist-notify").unwrap();
        let sessions_path = profile_dir.join("sessions.json");
        let (mut rx, _handle) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir,
                    matcher: FileMatcher::Exact(sessions_path),
                    debounce: Some(Duration::from_millis(75)),
                },
                4,
            )
            .expect("subscribe");

        let outcome = super::persist_session_to_storage(
            "sid-persist-notify",
            &id,
            "new-sid",
            Some("old"),
            &svc,
        );
        assert_eq!(outcome, super::SidWrite::Applied);

        // Wiring assertion: the in-process subscriber receives a delivery
        // for sessions.json within sub-tick budget. The Local-first
        // invariant of notify_local_change vs the kernel echo is locked
        // separately by file_watch::tests::
        // notify_local_change_delivers_local_first_and_tolerates_late_kernel_echo;
        // the dispatcher's debounce window may coalesce both into a
        // kernel-sourced slot on platforms where canonicalize latency
        // exceeds the kernel pipeline.
        let evt = timeout(Duration::from_millis(2_500), rx.recv())
            .await
            .expect("delivery within budget")
            .expect("dispatcher alive");
        assert_eq!(
            evt.path.file_name().and_then(|n| n.to_str()),
            Some("sessions.json"),
            "subscriber must observe the sessions.json write"
        );
    }

    #[test]
    #[serial]
    fn persist_session_id_reloads_memory_on_skipped() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-skipped-reload").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-skipped-reload".to_string();
        inst.agent_session_id = Some("peer-wrote".to_string());
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        // Daemon thinks disk is "stale" but peer wrote "peer-wrote".
        // After persist_session_id, in-memory should converge on disk.
        inst.agent_session_id = Some("daemon-fresh".to_string());
        let _ = inst.persist_session_id(
            "persist-skipped-reload",
            Some("stale"),
            ResumeIntent::Default,
        );

        assert_eq!(inst.agent_session_id.as_deref(), Some("peer-wrote"));
    }

    #[test]
    #[serial]
    fn persist_session_id_atomic_writes_both_fields_on_match() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-atomic-match").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-atomic-match".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Cleared;
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
        let _ = inst.persist_session_id("persist-atomic-match", None, ResumeIntent::Cleared);

        let loaded = storage.load().unwrap();
        assert_eq!(
            loaded[0].agent_session_id.as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345"),
            "sid must persist atomically with intent promotion"
        );
        assert_eq!(
            loaded[0].resume_intent,
            ResumeIntent::Default,
            "Cleared must auto-promote to Default in the same flock"
        );
        assert_eq!(inst.resume_intent, ResumeIntent::Default);
    }

    #[test]
    #[serial]
    fn persist_session_id_writes_none_atomically_when_sid_absent() {
        let temp = tempdir().unwrap();
        let profile = "persist-none-sid";
        let storage = crate::session::storage::Storage::new_for_test_path(
            profile,
            temp.path()
                .join("profiles")
                .join(profile)
                .join("sessions.json"),
        );
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        let outcome = inst.persist_session_id_with_storage(&storage, None, ResumeIntent::Default);

        assert_eq!(outcome, SidPersistOutcome::Published);
        assert_eq!(inst.agent_session_id, None);
        let loaded = storage.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, inst.id);
        assert_eq!(loaded[0].agent_session_id, None);
        assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
    }

    #[test]
    #[serial]
    fn fork_intent_promotes_to_default_after_launch() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "fork-promote";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "claude".into();
        inst.source_profile = profile.into();
        inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".into());
        inst.resume_intent = ResumeIntent::Fork {
            from: "019342aa-2222-7eee-8fff-aaaabbbbcccc".into(),
        };
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        // Simulate the post-launch persist: expected_prior_intent is the Fork
        // we launched with; the child id is already pinned in agent_session_id.
        let expected_prior = inst.resume_intent.clone();
        let expected_sid = inst.agent_session_id.clone();
        let _ = inst.persist_session_id(profile, expected_sid.as_deref(), expected_prior);

        let reloaded = storage.load().unwrap();
        let disk = reloaded.iter().find(|i| i.id == inst.id).unwrap();
        assert_eq!(
            disk.resume_intent,
            ResumeIntent::Default,
            "Fork must auto-promote to Default after the first launch so restarts resume the child plainly"
        );
        assert_eq!(
            disk.agent_session_id.as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345")
        );
    }

    #[test]
    #[serial]
    fn use_intent_remains_sticky_after_launch() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "use-promote";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let pinned = "019342ab-1234-7def-8901-abcdef012345";

        let mut inst = Instance::new("Pinned", "/tmp/x");
        inst.tool = "copilot".into();
        inst.source_profile = profile.into();
        inst.agent_session_id = Some(pinned.into());
        inst.resume_intent = ResumeIntent::Use(pinned.into());

        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        // Simulate the post-launch persist: expected_prior_intent is the Use
        // we launched with; the pinned id is already in agent_session_id.
        let expected_prior = inst.resume_intent.clone();
        let expected_sid = inst.agent_session_id.clone();
        let _ = inst.persist_session_id(profile, expected_sid.as_deref(), expected_prior);

        let reloaded = storage.load().unwrap();
        let disk = reloaded.iter().find(|i| i.id == inst.id).unwrap();
        assert_eq!(
            disk.resume_intent,
            ResumeIntent::Use(pinned.to_string()),
            "an explicit pin must remain authoritative across later launches",
        );
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use(pinned.to_string()),
            "the in-memory pin must remain aligned with durable state",
        );
        assert_eq!(disk.agent_session_id.as_deref(), Some(pinned));
    }

    #[test]
    #[serial]
    fn omp_pinned_launch_leaves_equal_sid_for_guarded_poller_confirmation() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "use-omp-confirm";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let pinned = "019342ab-1234-7def-8901-abcdef012345";
        let mut inst = Instance::new("Pinned OMP", "/tmp/x");
        inst.tool = "omp".into();
        inst.source_profile = profile.into();
        inst.agent_session_id = Some(pinned.into());
        inst.resume_intent = ResumeIntent::Use(pinned.into());
        inst.omp_capture_generation = Some("launch-current".into());
        let on_disk = inst.clone();
        storage
            .update(|instances, groups| {
                *instances = vec![on_disk.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        let expected_sid = inst.agent_session_id.clone();
        let expected_intent = inst.resume_intent.clone();
        let outcome = inst.persist_session_id(profile, expected_sid.as_deref(), expected_intent);

        assert_eq!(outcome, SidPersistOutcome::Published);
        assert_eq!(inst.agent_session_id, None);
        assert_eq!(inst.resume_intent, ResumeIntent::Use(pinned.into()));
        let disk = storage.load().unwrap();
        assert_eq!(disk[0].agent_session_id.as_deref(), Some(pinned));
        assert_eq!(disk[0].resume_intent, ResumeIntent::Use(pinned.into()));
    }

    #[test]
    #[serial]
    fn capture_backed_use_promotes_so_a_later_conversation_can_be_adopted() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        let profile = "use-capture-promote";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let pinned = "019342ab-1234-7def-8901-abcdef012345";
        let mut inst = Instance::new("Pinned Claude", "/tmp/x");
        inst.tool = "claude".into();
        inst.source_profile = profile.into();
        inst.agent_session_id = Some(pinned.into());
        inst.resume_intent = ResumeIntent::Use(pinned.into());
        let on_disk = inst.clone();
        storage
            .update(|instances, groups| {
                *instances = vec![on_disk.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        let expected_sid = inst.agent_session_id.clone();
        let _ =
            inst.persist_session_id(profile, expected_sid.as_deref(), inst.resume_intent.clone());
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use(pinned.into()),
            "configured capture support is not enough without a launch publisher"
        );

        inst.identity_publisher_launched = true;
        let _ =
            inst.persist_session_id(profile, expected_sid.as_deref(), inst.resume_intent.clone());

        assert_eq!(inst.resume_intent, ResumeIntent::Default);
        assert_eq!(
            storage.load().unwrap()[0].resume_intent,
            ResumeIntent::Default
        );
    }

    #[test]
    #[serial]
    fn persist_session_id_writes_sid_only_on_default_intent() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-default-intent").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-default-intent".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
        let _ = inst.persist_session_id("persist-default-intent", None, ResumeIntent::Default);

        let loaded = storage.load().unwrap();
        assert_eq!(
            loaded[0].agent_session_id.as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345"),
        );
        assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Default,
            "Default intent path must not mutate in-memory intent",
        );
    }

    #[test]
    #[serial]
    fn persist_session_id_clears_resume_probe_failed_marker() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-clear-resume-marker").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-clear-resume-marker".to_string();
        inst.agent_session_id = Some("019342aa-2222-7eee-8fff-aaaabbbbcccc".to_string());
        inst.resume_probe_failed_sid = Some("019342aa-2222-7eee-8fff-aaaabbbbcccc".to_string());
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
        let _ = inst.persist_session_id(
            "persist-clear-resume-marker",
            Some("019342aa-2222-7eee-8fff-aaaabbbbcccc"),
            ResumeIntent::Default,
        );

        let loaded = storage.load().unwrap();
        assert_eq!(
            loaded[0].agent_session_id.as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345"),
        );
        assert_eq!(loaded[0].resume_probe_failed_sid, None);
        assert_eq!(inst.resume_probe_failed_sid, None);
    }

    #[test]
    #[serial]
    fn persist_session_id_persists_sid_when_intent_cas_mismatches() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-intent-mismatch").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-intent-mismatch".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Cleared;
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        storage
            .update(|i, _g| {
                i[0].resume_intent = ResumeIntent::Use("peer-pinned".to_string());
                Ok(())
            })
            .unwrap();

        inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
        let _ = inst.persist_session_id("persist-intent-mismatch", None, ResumeIntent::Cleared);

        let loaded = storage.load().unwrap();
        assert_eq!(
            loaded[0].agent_session_id.as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345"),
            "sid must persist even when peer rewrote intent",
        );
        assert_eq!(
            loaded[0].resume_intent,
            ResumeIntent::Use("peer-pinned".to_string()),
            "peer's intent must survive when CAS mismatches",
        );
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use("peer-pinned".to_string()),
            "memory must converge on peer's intent",
        );
    }

    #[test]
    #[serial]
    fn persist_session_id_skipped_reloads_both_fields() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("persist-skipped-reload-both").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "persist-skipped-reload-both".to_string();
        inst.agent_session_id = Some("peer-sid".to_string());
        inst.resume_intent = ResumeIntent::Use("peer-pinned".to_string());
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();

        inst.agent_session_id = Some("daemon-fresh".to_string());
        inst.resume_intent = ResumeIntent::Cleared;
        let _ = inst.persist_session_id(
            "persist-skipped-reload-both",
            Some("stale"),
            ResumeIntent::Cleared,
        );

        assert_eq!(inst.agent_session_id.as_deref(), Some("peer-sid"));
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use("peer-pinned".to_string()),
            "intent must reload from disk on sid CAS skip",
        );
    }

    mod sid_disk_guards {
        use super::super::{
            persist_session_to_storage, Instance, ResumeIntent, SidPersistOutcome, SidWrite,
        };
        use crate::file_watch::FileWatchService;
        use crate::session::storage::Storage;
        use crate::session::test_support::EnvGuard;
        use crate::session::GroupTree;
        use serial_test::serial;
        use std::path::PathBuf;
        use tempfile::{tempdir, TempDir};

        const SID_X: &str = "019342ab-1234-7def-8901-111111111111";
        const SID_Y: &str = "019342ab-1234-7def-8901-222222222222";

        fn storage_home_guard(temp: &TempDir) -> EnvGuard {
            #[allow(unused_mut)]
            let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            EnvGuard::set(&pairs)
        }

        fn seed(profile: &str, insts: &[&Instance]) {
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

        fn load(profile: &str) -> Vec<Instance> {
            Storage::new_unwatched(profile).unwrap().load().unwrap()
        }

        fn make_inst(profile: &str, title: &str) -> Instance {
            let mut inst = Instance::new(title, "/tmp/x");
            inst.source_profile = profile.to_string();
            inst
        }

        #[test]
        #[serial]
        fn persist_rejects_sid_owned_by_another_instance_on_disk() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-owned";

            let mut owner = make_inst(profile, "owner");
            owner.agent_session_id = Some(SID_X.to_string());
            let claimant = make_inst(profile, "claimant");
            seed(profile, &[&owner, &claimant]);

            let file_watch = FileWatchService::noop();
            let write = persist_session_to_storage(profile, &claimant.id, SID_X, None, &file_watch);

            assert_eq!(write, SidWrite::Skipped);
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == claimant.id)
                    .unwrap()
                    .agent_session_id,
                None
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == owner.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
        }

        #[test]
        #[serial]
        fn persist_rejects_sid_contradicting_on_disk_pin() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-pin";

            // The pin exists only on disk (written by `aoe session
            // set-session-id` in another process); the caller's expected_prior
            // matches, so only the flock-scoped pin guard can reject this.
            let mut pinned = make_inst(profile, "pinned");
            pinned.agent_session_id = Some(SID_X.to_string());
            pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
            seed(profile, &[&pinned]);

            let file_watch = FileWatchService::noop();
            let write =
                persist_session_to_storage(profile, &pinned.id, SID_Y, Some(SID_X), &file_watch);
            assert_eq!(write, SidWrite::Skipped);
            assert_eq!(
                load(profile)[0].agent_session_id.as_deref(),
                Some(SID_X),
                "pin must stay authoritative against a differing write"
            );

            // A write matching the pin is normal capture and must pass.
            let write =
                persist_session_to_storage(profile, &pinned.id, SID_X, Some(SID_X), &file_watch);
            assert_eq!(write, SidWrite::Applied);
        }

        #[test]
        #[serial]
        fn finalize_persist_rejects_foreign_sid_without_pin() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-reject";

            let mut owner = make_inst(profile, "owner");
            owner.agent_session_id = Some(SID_X.to_string());
            let claimant = make_inst(profile, "claimant");
            seed(profile, &[&owner, &claimant]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = claimant.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome =
                live.persist_session_id_with_storage(&storage, None, ResumeIntent::Default);

            // Skipped-and-reloaded: memory converges back to the disk value.
            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(live.agent_session_id, None);
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == owner.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == claimant.id)
                    .unwrap()
                    .agent_session_id,
                None
            );
        }

        #[test]
        #[serial]
        fn finalize_persist_consuming_pin_takes_ownership_from_stale_holder() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-pin";

            // The documented repair for a same-cwd duplicate: pin the true
            // owner via `set-session-id`, then launch it. The launch that
            // consumes the pin must take the sid even though stale holders
            // still carry it on disk — and every stale holder is relieved of
            // it so no duplicate can persist. Two holders because the bug
            // being repaired manufactures duplicates, so more than one stale
            // row with the same sid is a reachable state.
            let mut stale_holder = make_inst(profile, "stale-holder");
            stale_holder.agent_session_id = Some(SID_X.to_string());
            let mut second_holder = make_inst(profile, "second-holder");
            second_holder.agent_session_id = Some(SID_X.to_string());
            let mut pinned = make_inst(profile, "pinned");
            pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
            seed(profile, &[&stale_holder, &second_holder, &pinned]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = pinned.clone();
            live.agent_session_id = Some(SID_X.to_string());
            live.identity_publisher_launched = true;
            let outcome = live.persist_session_id_with_storage(
                &storage,
                None,
                ResumeIntent::Use(SID_X.to_string()),
            );

            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(live.agent_session_id.as_deref(), Some(SID_X));
            assert_eq!(
                live.resume_intent,
                ResumeIntent::Default,
                "capture-backed pins must hand ownership back after launch"
            );
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == pinned.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == stale_holder.id)
                    .unwrap()
                    .agent_session_id,
                None,
                "stale holder must be relieved of the sid the pin claimed"
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == second_holder.id)
                    .unwrap()
                    .agent_session_id,
                None,
                "every duplicate holder must be relieved, not just the first"
            );
        }

        #[test]
        #[serial]
        fn finalize_persist_stale_pin_snapshot_does_not_take_ownership() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-stale-pin";

            // The caller consumed a Use(SID_X) pin pre-launch, but a peer
            // process has since rewritten the on-disk intent (here: cleared
            // it back to Default). The stale snapshot alone must not
            // authorize taking the sid from its current holder; the write is
            // rejected and memory converges to disk.
            let mut holder = make_inst(profile, "holder");
            holder.agent_session_id = Some(SID_X.to_string());
            let launcher = make_inst(profile, "launcher");
            seed(profile, &[&holder, &launcher]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = launcher.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome = live.persist_session_id_with_storage(
                &storage,
                None,
                ResumeIntent::Use(SID_X.to_string()),
            );

            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(
                live.agent_session_id, None,
                "launcher must converge to disk, not keep the contested sid"
            );
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == holder.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X),
                "holder must keep the sid when the pin is gone from disk"
            );
        }
    }

    mod publish_captured_sid {
        use super::super::{Instance, ResumeIntent, Status};
        use serial_test::serial;
        use std::collections::HashSet;
        use tempfile::{tempdir, TempDir};

        const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";
        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";

        /// Stand-in for the post-CAS env publish in
        /// `sync::drain_and_persist_session_ids` (the poller's pre-CAS
        /// on_change publish was removed in #2858): writes the same two keys
        /// so these tests keep exercising the env naming and the
        /// `build_exclusion_set` attribution contract.
        fn publish_session_to_tmux_env(
            tmux_session_name: &str,
            instance_id: &str,
            session_id: &str,
        ) {
            for (key, value) in [
                (crate::tmux::env::AOE_INSTANCE_ID_KEY, instance_id),
                (crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY, session_id),
            ] {
                crate::tmux::env::set_hidden_env(tmux_session_name, key, value)
                    .unwrap_or_else(|e| panic!("failed to write {key} to tmux env: {e}"));
            }
        }

        struct TmuxSession(String);

        impl TmuxSession {
            fn create(id: &str, title: &str) -> Self {
                Self::create_named(crate::tmux::Session::generate_name(id, title))
            }

            fn create_terminal(id: &str, title: &str) -> Self {
                Self::create_named(crate::tmux::TerminalSession::generate_name(id, title))
            }

            fn create_named(name: String) -> Self {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &name])
                    .output();
                let status = crate::tmux::tmux_command()
                    .args(["new-session", "-d", "-s", &name])
                    .status()
                    .expect("failed to spawn tmux");
                assert!(status.success(), "tmux new-session failed for {}", name);
                Self(name)
            }

            fn name(&self) -> &str {
                &self.0
            }
        }

        impl Drop for TmuxSession {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }

        fn skip_if_no_tmux() -> bool {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("Skipping: tmux not available");
                return true;
            }
            false
        }

        fn isolate_home(temp: &TempDir) {
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        fn captured_env(name: &str) -> Option<String> {
            crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
        }

        fn instance_env(name: &str) -> Option<String> {
            crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_INSTANCE_ID_KEY)
        }

        fn make_inst(profile: &str, title: &str) -> Instance {
            let mut inst = Instance::new(title, "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = profile.to_string();
            inst
        }

        fn seed_disk_row(profile: &str, inst: &Instance) {
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();
        }
        #[test]
        #[serial]
        fn omp_launch_without_capture_plan_publishes_tombstone_generation() {
            let temp = tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "omp-plan-failure-tombstone";
            let mut inst = make_inst(profile, "omp-plan-failure");
            inst.tool = "omp".to_string();
            let old_generation = "omp-old-generation";
            let stale_sid = "019342ab-1234-7def-8901-abcdef012349";
            inst.omp_capture_generation = Some(old_generation.to_string());
            seed_disk_row(profile, &inst);

            assert!(inst.publish_omp_launch_generation(profile, None, Some(old_generation)));
            let disk = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap();
            assert!(disk[0].omp_capture_generation.is_some());
            assert_eq!(disk[0].omp_capture_generation, inst.omp_capture_generation);
            assert_ne!(
                disk[0].omp_capture_generation.as_deref(),
                Some(old_generation)
            );
            assert_eq!(
                super::super::persist_omp_session_to_storage(
                    profile,
                    &inst.id,
                    stale_sid,
                    None,
                    Some(old_generation),
                    &crate::file_watch::FileWatchService::noop(),
                ),
                super::super::SidWrite::Skipped
            );
        }

        #[test]
        #[serial]
        fn stopped_poller_flush_persists_newest_omp_observation_without_tmux() {
            let temp = tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "omp-restart-final-flush";
            let generation = "omp-restart-generation";
            let sid = "019342ab-1234-7def-8901-abcdef012348";
            let mut inst = make_inst(profile, "omp-restart-flush");
            inst.tool = "omp".to_string();
            inst.omp_capture_generation = Some(generation.to_string());
            inst.status = Status::Stopped;
            seed_disk_row(profile, &inst);

            let poller = crate::session::poller::SessionPoller::new("unused-tmux".to_string());
            poller.inject_test_omp_update(&inst.id, sid, generation);
            inst.session_id_poller = Some(std::sync::Arc::new(std::sync::Mutex::new(poller)));
            inst.stop_and_flush_poller();

            assert!(inst.session_id_poller.is_none());
            assert_eq!(inst.agent_session_id.as_deref(), Some(sid));
            let disk = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap();
            assert_eq!(disk[0].agent_session_id.as_deref(), Some(sid));
        }

        #[test]
        #[serial]
        fn poller_publish_writes_terminal_session_env() {
            if skip_if_no_tmux() {
                return;
            }

            let mut inst = make_inst("publish-terminal", "tailscale-operator-followup");
            inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
            let tmux = TmuxSession::create_terminal(&inst.id, &inst.title);
            inst.title = "renamed-after-terminal-create".to_string();

            assert_eq!(inst.tmux_env_session_name().as_deref(), Some(tmux.name()));
            assert!(tmux.name().starts_with(crate::tmux::TERMINAL_PREFIX));
            assert!(tmux.name().contains("tailscale-operator-f"));

            let agent_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            publish_session_to_tmux_env(tmux.name(), &inst.id, VALID_SID);

            assert!(captured_env(&agent_name).is_none());
            assert_eq!(instance_env(tmux.name()).as_deref(), Some(inst.id.as_str()));
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }

        #[test]
        #[serial]
        fn terminal_publish_feeds_exclusion_set_for_other_instances() {
            if skip_if_no_tmux() {
                return;
            }

            let mut peer = make_inst("publish-terminal-exclusion", "peer-terminal");
            peer.terminal_info = Some(crate::session::TerminalInfo { created: true });
            let tmux = TmuxSession::create_terminal(&peer.id, &peer.title);

            publish_session_to_tmux_env(tmux.name(), &peer.id, PEER_SID);

            let extra = HashSet::new();
            let other_exclusion =
                crate::session::capture::compose_exclusion("other-instance", &extra);
            assert!(other_exclusion.contains(PEER_SID));

            let own_exclusion = crate::session::capture::compose_exclusion(&peer.id, &extra);
            assert!(!own_exclusion.contains(PEER_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_omp_metadata() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-applied";
            let mut inst = make_inst(profile, "fpaw");
            inst.tool = "omp".to_string();
            inst.pending_host_env = vec![
                ("OMP_PROFILE".to_string(), "work".to_string()),
                ("PI_CONFIG_DIR".to_string(), "/custom".to_string()),
            ];
            inst.agent_session_id = None;
            let plan = inst
                .resolve_omp_capture_plan(&inst.omp_capture_options().unwrap())
                .expect("OMP launch plan");
            let expected_layout = plan.layout.clone();
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            // Simulate dotenv/config drift after snapshot. Finalize must
            // publish the transported plan, not resolve these live values.
            inst.pending_host_env = vec![(
                "PI_CODING_AGENT_SESSION_DIR".to_string(),
                "/must-not-be-reread".to_string(),
            )];

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                None,
                ResumeIntent::Default,
                Some(crate::session::capture::OmpCaptureMetadata {
                    layout: plan.layout,
                    launched_at_ms: 1000,
                    launch_id: plan.launch_id.clone(),
                    launch_marker: plan.launch_marker.clone(),
                    routing_fingerprint: plan.routing_fingerprint.clone(),
                    container_runtime: plan.container_runtime,
                }),
            );

            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
            let metadata: crate::session::capture::OmpCaptureMetadata = serde_json::from_str(
                &crate::tmux::env::get_hidden_env(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .expect("typed OMP capture metadata must survive poller reconstruction"),
            )
            .unwrap();
            assert_eq!(metadata.launched_at_ms, 1000);
            assert_eq!(metadata.layout, expected_layout);
            assert!(metadata.layout.sessions.is_absolute());
            assert!(metadata.layout.terminal_sessions.is_absolute());
            assert!(metadata.layout.managed_sessions.is_absolute());
            assert_eq!(metadata.launch_id, plan.launch_id);
        }

        #[test]
        #[serial]
        fn legacy_omp_pane_backfills_typed_metadata_from_tmux_creation() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let mut inst = make_inst("omp-legacy-metadata", "legacy-omp");
            inst.tool = "omp".to_string();
            inst.agent_session_id = Some(VALID_SID.to_string());
            let tmux = TmuxSession::create(&inst.id, &inst.title);
            assert!(crate::tmux::env::get_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            )
            .is_none());

            let expected_launch = crate::tmux::Session::from_name(tmux.name())
                .created_at_ms()
                .unwrap();
            let options = inst.omp_capture_options().unwrap();
            let metadata = inst
                .omp_capture_metadata(tmux.name(), &options, None)
                .expect("legacy pane should migrate");
            assert_eq!(metadata.launched_at_ms, expected_launch);
            assert_eq!(
                metadata.launch_id,
                format!("legacy-{}-{expected_launch}", inst.id)
            );
            assert!(metadata.layout.managed_sessions.is_absolute());

            let persisted: crate::session::capture::OmpCaptureMetadata = serde_json::from_str(
                &crate::tmux::env::get_hidden_env(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .expect("migration must backfill metadata"),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(persisted).unwrap(),
                serde_json::to_value(metadata).unwrap()
            );

            inst.omp_capture_generation = Some("modern-generation".to_string());
            assert!(
                inst.omp_capture_metadata(tmux.name(), &options, None)
                    .is_none(),
                "markerless typed metadata is legacy only while no durable generation exists"
            );
        }

        #[test]
        #[serial]
        fn modern_omp_pane_without_hidden_metadata_does_not_legacy_migrate() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let mut inst = make_inst("omp-modern-missing-metadata", "modern-omp");
            inst.tool = "omp".to_string();
            let generation = "modern-launch-generation";
            inst.omp_capture_generation = Some(generation.to_string());
            let tmux = TmuxSession::create(&inst.id, &inst.title);
            let status = crate::tmux::tmux_command()
                .args([
                    "set-environment",
                    "-t",
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY,
                    generation,
                ])
                .status()
                .unwrap();
            assert!(status.success());

            let options = inst.omp_capture_options().unwrap();
            assert!(
                inst.omp_capture_metadata(tmux.name(), &options, None)
                    .is_none(),
                "a current pane missing its hidden launch snapshot must fail closed"
            );
            assert!(
                crate::tmux::env::get_hidden_env(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .is_none(),
                "the legacy path must not synthesize metadata for a current pane"
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_env_for_non_claude_tool() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-applied-opencode";
            let mut inst = make_inst(profile, "fpaw-oc");
            inst.tool = "opencode".to_string();
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some(VALID_SID),
                "non-claude tools must also publish AOE_CAPTURED_SESSION_ID at finalize"
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_skipped_disk_some_publishes_disk_value() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-skipped-some";
            let mut inst = make_inst(profile, "fpsdspd");
            inst.agent_session_id = Some(PEER_SID.to_string());
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                Some("stale"),
                ResumeIntent::Default,
                None,
            );

            assert_eq!(inst.agent_session_id.as_deref(), Some(PEER_SID));
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(PEER_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_skipped_disk_none_unsets_env() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-skipped-none";
            let mut inst = make_inst(profile, "fpsdne");
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-leftover",
            )
            .unwrap();

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                Some("stale"),
                ResumeIntent::Default,
                None,
            );

            assert!(inst.agent_session_id.is_none());
            assert!(captured_env(tmux.name()).is_none());
        }

        #[test]
        #[serial]
        fn finalize_publish_failed_leaves_env_unchanged() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-failed";
            let _ = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let mut inst = make_inst(profile, "fpfle");

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-untouched",
            )
            .unwrap();

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some("stale-untouched")
            );
            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some(VALID_SID),
                "memory must keep the daemon-set sid when persist returns Failed"
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_invalid_sid_skips_publish() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-invalid";
            let mut inst = make_inst(profile, "fpisp");
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-untouched",
            )
            .unwrap();

            inst.agent_session_id = Some("bad sid!".to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some("stale-untouched")
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_promote_cleared_applied_uses_new_sid() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-promote";
            let mut inst = make_inst(profile, "fppca");
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Cleared;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Cleared, None);

            assert_eq!(inst.agent_session_id.as_deref(), Some(VALID_SID));
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }
    }
}
