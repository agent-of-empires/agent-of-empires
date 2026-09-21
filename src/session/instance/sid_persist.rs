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

/// Caller contract for `persist_session_id`: whether to publish the post-CAS `agent_session_id` to
/// the tmux hidden env.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidPersistOutcome {
    Published,
    Skip,
}

/// Find another on-disk row that already holds `sid`.
pub(super) fn foreign_sid_holder<'a>(
    instances: &'a [Instance],
    instance_id: &str,
    sid: &str,
) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.id != instance_id && i.agent_session_id.as_deref() == Some(sid))
}

/// CAS-write `agent_session_id` to disk. Caller passes the value the in-memory mirror held at last
/// reconcile as `expected_prior`.
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

/// Emit `fresh` only when it differs from the stored session id, the "override only when distinct"
/// contract shared by both branches of `capture_freshest_session_id` (sidecar and mtime fallback).
pub(super) fn override_if_distinct(stored: Option<&str>, fresh: String) -> Option<String> {
    match stored {
        Some(known) if known == fresh => None,
        _ => Some(fresh),
    }
}

impl Instance {
    /// Consume an explicit OMP resume pin only after the matching launch reports the
    /// already-durable sid.
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
        // Cleared and Fork are one-shot launch directives. Use stays durable only when no
        // pane-scoped capture backend can observe a later `/new`.
        let promote_one_shot = matches!(
            expected_prior_intent,
            ResumeIntent::Cleared | ResumeIntent::Fork { .. }
        ) || matches!(expected_prior_intent, ResumeIntent::Use(_))
            && self.launch_has_session_publisher();
        // OMP seeds its poller with the in-memory sid. Keeping the pin there would suppress the
        // first equal observation, which is the launch's confirmation.
        let await_omp_pin_confirmation = matches!(
            (&expected_prior_intent, new_sid.as_deref()),
            (ResumeIntent::Use(pinned), Some(sid)) if pinned == sid
        ) && self.resolved_capture_backend()
            == Some(crate::agents::SessionCaptureBackend::Omp)
            && self
                .omp_capture_generation
                .as_deref()
                .is_some_and(|generation| !generation.starts_with("tombstone-"));

        let mut cleared_holder_ids: Vec<String> = Vec::new();
        let outcome = storage.update(|instances, _groups| {
            Ok(commit_finalize_persist(
                instances,
                &self.id,
                new_sid.as_deref(),
                expected_prior_sid,
                &expected_prior_intent,
                promote_one_shot,
                &mut cleared_holder_ids,
            ))
        });

        match outcome {
            Ok(SidWrite::Applied) => {
                // Outside the flock.
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
            Ok(SidWrite::Skipped) => {
                self.reload_after_skipped_persist(storage, await_omp_pin_confirmation)
            }
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

    /// A peer won the CAS: converge memory on the disk row.
    fn reload_after_skipped_persist(
        &mut self,
        storage: &crate::session::storage::Storage,
        await_omp_pin_confirmation: bool,
    ) -> SidPersistOutcome {
        match storage.load() {
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
        }
    }
}

/// Under the flock: CAS the sid, enforce ownership (an explicit pin consumed at launch takes the
/// sid from stale holders), and promote a one-shot intent.
fn commit_finalize_persist(
    instances: &mut [Instance],
    instance_id: &str,
    new_sid: Option<&str>,
    expected_prior_sid: Option<&str>,
    expected_prior_intent: &ResumeIntent,
    promote_one_shot: bool,
    cleared_holder_ids: &mut Vec<String>,
) -> SidWrite {
    let Some(inst) = instances.iter().find(|i| i.id == instance_id) else {
        return SidWrite::Failed;
    };

    if inst.agent_session_id.as_deref() != expected_prior_sid {
        tracing::warn!(target: "session.store",
            instance_id = %instance_id,
            expected_sid = ?expected_prior_sid,
            disk_sid = ?inst.agent_session_id,
            "sid CAS mismatch in finalize persist; skipping both writes"
        );
        return SidWrite::Skipped;
    }

    if let Some(sid) = new_sid {
        let consumed_pin = matches!(
            expected_prior_intent,
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
            if !consumed_pin {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    sid = %sid,
                    holder = %holder_ids[0],
                    "sid write rejected under flock in finalize persist: already owned by another instance"
                );
                return SidWrite::Skipped;
            }
            for holder_id in &holder_ids {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    sid = %sid,
                    holder = %holder_id,
                    "explicit pin consumed at launch: taking sid ownership from stale holder"
                );
                if let Some(holder) = instances.iter_mut().find(|i| &i.id == holder_id) {
                    holder.agent_session_id = None;
                    holder.resume_probe_failed_sid = None;
                }
            }
            *cleared_holder_ids = holder_ids;
        }
    }

    let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
        return SidWrite::Failed;
    };
    inst.agent_session_id = new_sid.map(str::to_string);
    inst.resume_probe_failed_sid = None;

    if promote_one_shot {
        if inst.resume_intent == *expected_prior_intent {
            inst.resume_intent = ResumeIntent::Default;
        } else {
            tracing::warn!(target: "session.store",
                instance_id = %instance_id,
                expected_intent = ?expected_prior_intent,
                disk_intent = ?inst.resume_intent,
                "resume_intent CAS mismatch in finalize persist; sid persisted but intent left as peer wrote it"
            );
        }
    }

    SidWrite::Applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::FileWatchService;
    use crate::session::storage::Storage;
    use crate::session::GroupTree;
    use serial_test::serial;
    use tempfile::{tempdir, TempDir};

    const SID_A: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";
    const SID_X: &str = "019342ab-1234-7def-8901-111111111111";
    const SID_Y: &str = "019342ab-1234-7def-8901-222222222222";
    const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";

    /// Isolated home plus a profile whose sessions.json holds `insts`.
    fn seeded(
        profile: &str,
        insts: &[&Instance],
    ) -> (TempDir, crate::session::test_support::EnvGuard, Storage) {
        let temp = tempdir().unwrap();
        let guard = crate::session::test_support::isolate_home(temp.path());
        seed(profile, insts);
        (temp, guard, Storage::new_unwatched(profile).unwrap())
    }

    fn seed(profile: &str, insts: &[&Instance]) {
        let owned: Vec<Instance> = insts.iter().map(|i| (*i).clone()).collect();
        Storage::new_unwatched(profile)
            .unwrap()
            .update(|i, g| {
                *i = owned.clone();
                *g = GroupTree::new_with_groups(&owned, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    fn make_inst(profile: &str, title: &str) -> Instance {
        let mut inst = Instance::new(title, "/tmp/x");
        inst.source_profile = profile.to_string();
        inst
    }

    fn disk_sid(profile: &str, id: &str) -> Option<String> {
        Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap()
            .agent_session_id
    }

    #[test]
    #[serial]
    fn persist_session_to_storage_is_cas_guarded() {
        for (prior, expected, disk) in [
            (Some("old"), SidWrite::Applied, "new"),
            (Some("stale"), SidWrite::Skipped, "old"),
        ] {
            let profile = "cas-persist";
            let mut inst = make_inst(profile, "title");
            inst.agent_session_id = Some("old".to_string());
            let (_temp, _home, _) = seeded(profile, &[&inst]);
            let write = persist_session_to_storage(
                profile,
                &inst.id,
                "new",
                prior,
                &FileWatchService::noop(),
            );
            assert_eq!(write, expected);
            assert_eq!(disk_sid(profile, &inst.id).as_deref(), Some(disk));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn persist_session_to_storage_delivers_notification_to_in_process_subscriber() {
        use crate::file_watch::{FileMatcher, WatchSpec};
        use std::time::Duration;

        let profile = "sid-persist-notify";
        let mut inst = make_inst(profile, "title");
        inst.agent_session_id = Some("old".to_string());
        let (_temp, _home, _) = seeded(profile, &[&inst]);

        let svc = FileWatchService::new().expect("init");
        let profile_dir = crate::session::get_profile_dir_path(profile).unwrap();
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

        let write = persist_session_to_storage(profile, &inst.id, "new-sid", Some("old"), &svc);
        assert_eq!(write, SidWrite::Applied);
        let evt = tokio::time::timeout(Duration::from_millis(2_500), rx.recv())
            .await
            .expect("delivery within budget")
            .expect("dispatcher alive");
        assert_eq!(
            evt.path.file_name().and_then(|n| n.to_str()),
            Some("sessions.json")
        );
    }

    struct FinalizeCase {
        label: &'static str,
        tool: &'static str,
        disk_sid: Option<&'static str>,
        disk_intent: ResumeIntent,
        disk_marker: Option<&'static str>,
        peer_intent: Option<ResumeIntent>,
        memory_sid: Option<&'static str>,
        prior_sid: Option<&'static str>,
        prior_intent: ResumeIntent,
        want_disk: (Option<&'static str>, ResumeIntent),
        want_memory: (Option<&'static str>, ResumeIntent),
    }

    #[test]
    #[serial]
    fn persist_session_id_cas_and_intent_promotion() {
        let use_pin = || ResumeIntent::Use(VALID_SID.to_string());
        let peer_pin = || ResumeIntent::Use("peer-pinned".to_string());
        let case = |label, tool, disk_sid, disk_intent, memory_sid, prior_sid, prior_intent| {
            FinalizeCase {
                label,
                tool,
                disk_sid,
                disk_intent,
                disk_marker: None,
                peer_intent: None,
                memory_sid,
                prior_sid,
                prior_intent,
                want_disk: (None, ResumeIntent::Default),
                want_memory: (None, ResumeIntent::Default),
            }
        };
        let cases = [
            FinalizeCase {
                want_disk: (Some("peer-wrote"), ResumeIntent::Default),
                want_memory: (Some("peer-wrote"), ResumeIntent::Default),
                ..case(
                    "sid CAS skip reloads memory",
                    "claude",
                    Some("peer-wrote"),
                    ResumeIntent::Default,
                    Some("daemon-fresh"),
                    Some("stale"),
                    ResumeIntent::Default,
                )
            },
            FinalizeCase {
                want_disk: (Some("peer-sid"), peer_pin()),
                want_memory: (Some("peer-sid"), peer_pin()),
                ..case(
                    "sid CAS skip reloads intent",
                    "claude",
                    Some("peer-sid"),
                    peer_pin(),
                    Some("daemon-fresh"),
                    Some("stale"),
                    ResumeIntent::Cleared,
                )
            },
            FinalizeCase {
                want_disk: (Some(VALID_SID), ResumeIntent::Default),
                want_memory: (Some(VALID_SID), ResumeIntent::Default),
                ..case(
                    "cleared promotes with the sid",
                    "claude",
                    None,
                    ResumeIntent::Cleared,
                    Some(VALID_SID),
                    None,
                    ResumeIntent::Cleared,
                )
            },
            FinalizeCase {
                want_disk: (Some(VALID_SID), ResumeIntent::Default),
                want_memory: (Some(VALID_SID), ResumeIntent::Default),
                ..case(
                    "default writes sid only",
                    "claude",
                    None,
                    ResumeIntent::Default,
                    Some(VALID_SID),
                    None,
                    ResumeIntent::Default,
                )
            },
            FinalizeCase {
                want_disk: (Some(VALID_SID), ResumeIntent::Default),
                want_memory: (Some(VALID_SID), ResumeIntent::Default),
                ..case(
                    "fork promotes after launch",
                    "claude",
                    Some(VALID_SID),
                    ResumeIntent::Fork {
                        from: SID_A.to_string(),
                    },
                    Some(VALID_SID),
                    Some(VALID_SID),
                    ResumeIntent::Fork {
                        from: SID_A.to_string(),
                    },
                )
            },
            FinalizeCase {
                want_disk: (Some(VALID_SID), use_pin()),
                want_memory: (Some(VALID_SID), use_pin()),
                ..case(
                    "use stays sticky without a publisher",
                    "copilot",
                    Some(VALID_SID),
                    use_pin(),
                    Some(VALID_SID),
                    Some(VALID_SID),
                    use_pin(),
                )
            },
            FinalizeCase {
                want_disk: (Some(VALID_SID), use_pin()),
                want_memory: (None, use_pin()),
                ..case(
                    "omp pin awaits poller confirmation",
                    "omp",
                    Some(VALID_SID),
                    use_pin(),
                    Some(VALID_SID),
                    Some(VALID_SID),
                    use_pin(),
                )
            },
            FinalizeCase {
                disk_marker: Some(SID_A),
                want_disk: (Some(VALID_SID), ResumeIntent::Default),
                want_memory: (Some(VALID_SID), ResumeIntent::Default),
                ..case(
                    "clears resume probe marker",
                    "claude",
                    Some(SID_A),
                    ResumeIntent::Default,
                    Some(VALID_SID),
                    Some(SID_A),
                    ResumeIntent::Default,
                )
            },
            FinalizeCase {
                peer_intent: Some(peer_pin()),
                want_disk: (Some(VALID_SID), peer_pin()),
                want_memory: (Some(VALID_SID), peer_pin()),
                ..case(
                    "intent CAS mismatch keeps peer intent",
                    "claude",
                    None,
                    ResumeIntent::Cleared,
                    Some(VALID_SID),
                    None,
                    ResumeIntent::Cleared,
                )
            },
        ];
        for c in cases {
            let profile = "persist-finalize";
            let mut inst = make_inst(profile, c.label);
            inst.tool = c.tool.to_string();
            inst.agent_session_id = c.disk_sid.map(str::to_string);
            inst.resume_intent = c.disk_intent.clone();
            inst.resume_probe_failed_sid = c.disk_marker.map(str::to_string);
            if c.tool == "omp" {
                inst.omp_capture_generation = Some("launch-current".into());
            }
            let (_temp, _home, storage) = seeded(profile, &[&inst]);
            if let Some(peer) = c.peer_intent.clone() {
                storage
                    .update(|i, _| {
                        i[0].resume_intent = peer;
                        Ok(())
                    })
                    .unwrap();
            }
            inst.agent_session_id = c.memory_sid.map(str::to_string);
            let outcome = inst.persist_session_id(profile, c.prior_sid, c.prior_intent);
            assert_eq!(outcome, SidPersistOutcome::Published, "{}", c.label);
            let disk = &storage.load().unwrap()[0];
            assert_eq!(
                (disk.agent_session_id.as_deref(), disk.resume_intent.clone()),
                c.want_disk,
                "{}",
                c.label
            );
            assert_eq!(disk.resume_probe_failed_sid, None, "{}", c.label);
            assert_eq!(inst.resume_probe_failed_sid, None, "{}", c.label);
            assert_eq!(
                (inst.agent_session_id.as_deref(), inst.resume_intent.clone()),
                c.want_memory,
                "{}",
                c.label
            );
        }
    }

    #[test]
    #[serial]
    fn persist_session_id_writes_none_atomically_when_sid_absent() {
        let temp = tempdir().unwrap();
        let profile = "persist-none-sid";
        let storage = Storage::new_for_test_path(
            profile,
            temp.path()
                .join("profiles")
                .join(profile)
                .join("sessions.json"),
        );
        let mut inst = make_inst(profile, "title");
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g = GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                    .get_all_groups();
                Ok(())
            })
            .unwrap();

        let outcome = inst.persist_session_id_with_storage(&storage, None, ResumeIntent::Default);
        assert_eq!(outcome, SidPersistOutcome::Published);
        let loaded = storage.load().unwrap();
        assert_eq!(
            (loaded.len(), loaded[0].agent_session_id.clone()),
            (1, None)
        );
        assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
    }

    #[test]
    #[serial]
    fn capture_backed_use_promotes_so_a_later_conversation_can_be_adopted() {
        let profile = "use-capture-promote";
        let mut inst = make_inst(profile, "Pinned Claude");
        inst.tool = "claude".into();
        inst.agent_session_id = Some(VALID_SID.into());
        inst.resume_intent = ResumeIntent::Use(VALID_SID.into());
        let (_temp, _home, storage) = seeded(profile, &[&inst]);

        let _ = inst.persist_session_id(profile, Some(VALID_SID), inst.resume_intent.clone());
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use(VALID_SID.into()),
            "configured capture support is not enough without a launch publisher"
        );
        inst.identity_publisher_launched = true;
        let _ = inst.persist_session_id(profile, Some(VALID_SID), inst.resume_intent.clone());
        assert_eq!(inst.resume_intent, ResumeIntent::Default);
        assert_eq!(
            storage.load().unwrap()[0].resume_intent,
            ResumeIntent::Default
        );
    }

    #[test]
    #[serial]
    fn persist_rejects_sid_owned_by_another_row_or_contradicting_a_pin() {
        let profile = "guards-owned";
        let noop = FileWatchService::noop();
        let mut owner = make_inst(profile, "owner");
        owner.agent_session_id = Some(SID_X.to_string());
        let claimant = make_inst(profile, "claimant");
        let (_temp, _home, _) = seeded(profile, &[&owner, &claimant]);
        assert_eq!(
            persist_session_to_storage(profile, &claimant.id, SID_X, None, &noop),
            SidWrite::Skipped
        );
        assert_eq!(disk_sid(profile, &claimant.id), None);
        assert_eq!(disk_sid(profile, &owner.id).as_deref(), Some(SID_X));

        // A pin that exists only on disk stays authoritative against a differing write.
        let profile = "guards-pin";
        let mut pinned = make_inst(profile, "pinned");
        pinned.agent_session_id = Some(SID_X.to_string());
        pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
        seed(profile, &[&pinned]);
        assert_eq!(
            persist_session_to_storage(profile, &pinned.id, SID_Y, Some(SID_X), &noop),
            SidWrite::Skipped
        );
        assert_eq!(disk_sid(profile, &pinned.id).as_deref(), Some(SID_X));
        assert_eq!(
            persist_session_to_storage(profile, &pinned.id, SID_X, Some(SID_X), &noop),
            SidWrite::Applied
        );
    }

    #[test]
    #[serial]
    fn finalize_persist_takes_a_foreign_sid_only_for_a_pin_still_on_disk() {
        // No pin, and a stale pin snapshot whose disk intent a peer cleared: both converge to disk.
        for (profile, prior_intent) in [
            ("guards-finalize-reject", ResumeIntent::Default),
            (
                "guards-finalize-stale-pin",
                ResumeIntent::Use(SID_X.to_string()),
            ),
        ] {
            let mut holder = make_inst(profile, "holder");
            holder.agent_session_id = Some(SID_X.to_string());
            let launcher = make_inst(profile, "launcher");
            let (_temp, _home, storage) = seeded(profile, &[&holder, &launcher]);
            let mut live = launcher.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome = live.persist_session_id_with_storage(&storage, None, prior_intent);
            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(live.agent_session_id, None, "{profile}");
            assert_eq!(disk_sid(profile, &holder.id).as_deref(), Some(SID_X));
            assert_eq!(disk_sid(profile, &launcher.id), None);
        }

        // The documented same-cwd repair: pin the true owner, then launch it.
        let profile = "guards-finalize-pin";
        let mut stale_holder = make_inst(profile, "stale-holder");
        stale_holder.agent_session_id = Some(SID_X.to_string());
        let mut second_holder = make_inst(profile, "second-holder");
        second_holder.agent_session_id = Some(SID_X.to_string());
        let mut pinned = make_inst(profile, "pinned");
        pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
        let (_temp, _home, storage) = seeded(profile, &[&stale_holder, &second_holder, &pinned]);
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
        assert_eq!(live.resume_intent, ResumeIntent::Default);
        assert_eq!(disk_sid(profile, &pinned.id).as_deref(), Some(SID_X));
        assert_eq!(disk_sid(profile, &stale_holder.id), None);
        assert_eq!(disk_sid(profile, &second_holder.id), None);
    }

    mod publish_captured_sid {
        use super::*;
        use std::collections::HashSet;

        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";

        /// Stand-in for the post-CAS env publish in `sync::drain_and_persist_session_ids`.
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

        fn no_tmux() -> bool {
            let missing = crate::tmux::tmux_command().arg("-V").output().is_err();
            if missing {
                eprintln!("Skipping: tmux not available");
            }
            missing
        }

        fn hidden_env(name: &str, key: &str) -> Option<String> {
            crate::tmux::env::get_hidden_env(name, key)
        }

        fn captured_env(name: &str) -> Option<String> {
            hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
        }

        fn omp_inst(profile: &str, title: &str) -> Instance {
            let mut inst = make_inst(profile, title);
            inst.tool = "omp".to_string();
            inst
        }

        #[test]
        #[serial]
        fn omp_launch_without_capture_plan_publishes_tombstone_generation() {
            let temp = tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "omp-plan-failure-tombstone";
            let old_generation = "omp-old-generation";
            let mut inst = omp_inst(profile, "omp-plan-failure");
            inst.omp_capture_generation = Some(old_generation.to_string());
            seed(profile, &[&inst]);

            assert!(inst.publish_omp_launch_generation(profile, None, Some(old_generation)));
            let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
            assert!(disk[0].omp_capture_generation.is_some());
            assert_eq!(disk[0].omp_capture_generation, inst.omp_capture_generation);
            assert_ne!(
                disk[0].omp_capture_generation.as_deref(),
                Some(old_generation)
            );
            assert_eq!(
                crate::session::instance::persist_omp_session_to_storage(
                    profile,
                    &inst.id,
                    "019342ab-1234-7def-8901-abcdef012349",
                    None,
                    Some(old_generation),
                    &FileWatchService::noop(),
                ),
                SidWrite::Skipped
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
            let mut inst = omp_inst(profile, "omp-restart-flush");
            inst.omp_capture_generation = Some(generation.to_string());
            inst.status = Status::Stopped;
            seed(profile, &[&inst]);

            let poller = crate::session::poller::SessionPoller::new("unused-tmux".to_string());
            poller.inject_test_omp_update(&inst.id, sid, generation);
            inst.session_id_poller = Some(std::sync::Arc::new(std::sync::Mutex::new(poller)));
            inst.stop_and_flush_poller();

            assert!(inst.session_id_poller.is_none());
            assert_eq!(inst.agent_session_id.as_deref(), Some(sid));
            assert_eq!(disk_sid(profile, &inst.id).as_deref(), Some(sid));
        }

        #[test]
        #[serial]
        fn terminal_publish_is_visible_to_other_instances_exclusion() {
            if no_tmux() {
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
            publish_session_to_tmux_env(tmux.name(), &inst.id, PEER_SID);
            assert!(captured_env(&agent_name).is_none());
            assert_eq!(
                hidden_env(tmux.name(), crate::tmux::env::AOE_INSTANCE_ID_KEY).as_deref(),
                Some(inst.id.as_str())
            );
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(PEER_SID));

            let extra = HashSet::new();
            assert!(
                crate::session::capture::compose_exclusion("other-instance", &extra)
                    .contains(PEER_SID)
            );
            assert!(
                !crate::session::capture::compose_exclusion(&inst.id, &extra).contains(PEER_SID)
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_omp_metadata() {
            if no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let profile = "publish-applied";
            let mut inst = omp_inst(profile, "fpaw");
            inst.pending_host_env = vec![
                ("OMP_PROFILE".to_string(), "work".to_string()),
                ("PI_CONFIG_DIR".to_string(), "/custom".to_string()),
            ];
            let plan = inst
                .resolve_omp_capture_plan(&inst.omp_capture_options().unwrap())
                .expect("OMP launch plan");
            let expected_layout = plan.layout.clone();
            seed(profile, &[&inst]);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            // Config drift after the snapshot: finalize publishes the transported plan.
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
                &hidden_env(tmux.name(), crate::tmux::env::AOE_OMP_CAPTURE_META_KEY)
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
        fn only_a_generationless_omp_pane_backfills_legacy_metadata() {
            if no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let mut inst = omp_inst("omp-legacy-metadata", "legacy-omp");
            inst.agent_session_id = Some(VALID_SID.to_string());
            let tmux = TmuxSession::create(&inst.id, &inst.title);
            let meta_key = crate::tmux::env::AOE_OMP_CAPTURE_META_KEY;
            assert!(hidden_env(tmux.name(), meta_key).is_none());

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
            let persisted: crate::session::capture::OmpCaptureMetadata =
                serde_json::from_str(&hidden_env(tmux.name(), meta_key).expect("backfilled"))
                    .unwrap();
            assert_eq!(
                serde_json::to_value(persisted).unwrap(),
                serde_json::to_value(metadata).unwrap()
            );
            inst.omp_capture_generation = Some("modern-generation".to_string());
            assert!(inst
                .omp_capture_metadata(tmux.name(), &options, None)
                .is_none());

            // A current pane missing its hidden launch snapshot fails closed.
            let mut modern = omp_inst("omp-modern-missing-metadata", "modern-omp");
            let generation = "modern-launch-generation";
            modern.omp_capture_generation = Some(generation.to_string());
            let tmux = TmuxSession::create(&modern.id, &modern.title);
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
            let options = modern.omp_capture_options().unwrap();
            assert!(modern
                .omp_capture_metadata(tmux.name(), &options, None)
                .is_none());
            assert!(hidden_env(tmux.name(), meta_key).is_none());
        }

        #[test]
        #[serial]
        fn finalize_launch_publishes_the_post_cas_sid() {
            if no_tmux() {
                return;
            }
            // (label, tool, seeded disk row, disk intent, preset env, memory sid, prior sid,
            //  prior intent, want env, want memory sid, want memory intent)
            type SidCase = (
                &'static str,
                &'static str,
                Option<Option<&'static str>>,
                ResumeIntent,
                Option<&'static str>,
                &'static str,
                Option<&'static str>,
                ResumeIntent,
                Option<&'static str>,
                Option<&'static str>,
                ResumeIntent,
            );
            let cases: [SidCase; 6] = [
                (
                    "applied non-claude",
                    "opencode",
                    Some(None),
                    ResumeIntent::Default,
                    None,
                    VALID_SID,
                    None,
                    ResumeIntent::Default,
                    Some(VALID_SID),
                    Some(VALID_SID),
                    ResumeIntent::Default,
                ),
                (
                    "skipped publishes disk value",
                    "claude",
                    Some(Some(PEER_SID)),
                    ResumeIntent::Default,
                    None,
                    VALID_SID,
                    Some("stale"),
                    ResumeIntent::Default,
                    Some(PEER_SID),
                    Some(PEER_SID),
                    ResumeIntent::Default,
                ),
                (
                    "skipped with no disk sid unsets",
                    "claude",
                    Some(None),
                    ResumeIntent::Default,
                    Some("stale-leftover"),
                    VALID_SID,
                    Some("stale"),
                    ResumeIntent::Default,
                    None,
                    None,
                    ResumeIntent::Default,
                ),
                (
                    "failed leaves env and memory",
                    "claude",
                    None,
                    ResumeIntent::Default,
                    Some("stale-untouched"),
                    VALID_SID,
                    None,
                    ResumeIntent::Default,
                    Some("stale-untouched"),
                    Some(VALID_SID),
                    ResumeIntent::Default,
                ),
                (
                    "invalid sid skips publish",
                    "claude",
                    Some(None),
                    ResumeIntent::Default,
                    Some("stale-untouched"),
                    "bad sid!",
                    None,
                    ResumeIntent::Default,
                    Some("stale-untouched"),
                    Some("bad sid!"),
                    ResumeIntent::Default,
                ),
                (
                    "cleared promotes and publishes",
                    "claude",
                    Some(None),
                    ResumeIntent::Cleared,
                    None,
                    VALID_SID,
                    None,
                    ResumeIntent::Cleared,
                    Some(VALID_SID),
                    Some(VALID_SID),
                    ResumeIntent::Default,
                ),
            ];
            for (
                label,
                tool,
                disk,
                disk_intent,
                preset,
                memory,
                prior,
                prior_intent,
                want_env,
                want_sid,
                want_intent,
            ) in cases
            {
                let temp = tempdir().unwrap();
                let _home_guard = crate::session::test_support::isolate_home(temp.path());
                let profile = "publish-finalize";
                let mut inst = make_inst(profile, "fpub");
                inst.tool = tool.to_string();
                inst.resume_intent = disk_intent;
                match disk {
                    Some(sid) => {
                        inst.agent_session_id = sid.map(str::to_string);
                        seed(profile, &[&inst]);
                    }
                    None => drop(Storage::new_unwatched(profile).unwrap()),
                }
                let tmux = TmuxSession::create(&inst.id, &inst.title);
                if let Some(value) = preset {
                    crate::tmux::env::set_hidden_env(
                        tmux.name(),
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                        value,
                    )
                    .unwrap();
                }
                inst.agent_session_id = Some(memory.to_string());
                inst.finalize_launch(tmux.name(), profile, prior, prior_intent, None);
                assert_eq!(captured_env(tmux.name()).as_deref(), want_env, "{label}");
                assert_eq!(inst.agent_session_id.as_deref(), want_sid, "{label}");
                assert_eq!(inst.resume_intent, want_intent, "{label}");
            }
        }
    }
}
