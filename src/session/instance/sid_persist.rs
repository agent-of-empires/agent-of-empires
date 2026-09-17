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
    /// Deterministic refusal, not a race: the row pins a different
    /// conversation (`ResumeIntent::Use`), so this publication must not
    /// overwrite it. Teardown may proceed without touching the pin.
    PinnedForeign,
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

/// Compare a captured conversation with the complete snapshot read before capture.
pub(crate) fn persist_session_to_storage(
    profile: &str,
    instance_id: &str,
    observation: &crate::session::poller::SessionIdObservation,
    expected: &ConversationState,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    let storage = match crate::session::storage::Storage::new(profile, file_watch.clone()) {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(target: "session.store", "Cannot open capture storage: {error}");
            return SidWrite::Failed;
        }
    };
    persist_session_with_storage(&storage, instance_id, observation, expected)
}

pub(super) fn persist_session_with_storage(
    storage: &crate::session::storage::Storage,
    instance_id: &str,
    observation: &crate::session::poller::SessionIdObservation,
    expected: &ConversationState,
) -> SidWrite {
    use crate::session::poller::SessionIdGuard;
    let session_id = observation.sid.as_str();
    if !is_valid_session_id(session_id) {
        return SidWrite::Failed;
    }
    if observation.execution != expected.active {
        return SidWrite::Skipped;
    }
    let binding = observation.conversation_binding();
    let result = storage.update(|instances, _groups| {
        let Some(index) = instances
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Ok(SidWrite::Failed);
        };
        let instance = &instances[index];
        if !expected.matches(instance)
            || matches!(instance.resume_intent, ResumeIntent::Fork { .. })
        {
            return Ok(SidWrite::Skipped);
        }
        match &observation.guard {
            SessionIdGuard::OmpGeneration(generation)
                if instance.omp_capture_generation.as_ref() != Some(generation) =>
            {
                return Ok(SidWrite::Skipped)
            }
            SessionIdGuard::OmpLegacy if instance.omp_capture_generation.is_some() => {
                return Ok(SidWrite::Skipped)
            }
            _ => {}
        }
        // A pin to another conversation is a deliberate refusal, not a race:
        // report it distinctly so teardown can proceed without touching the
        // pin. Only the sid mismatch qualifies; a divergent execution binding
        // for the pinned sid stays a namespace doubt (`Skipped`).
        if let ResumeIntent::Use(pinned) = &instance.resume_intent {
            if pinned != session_id {
                return Ok(SidWrite::PinnedForeign);
            }
            if instance.resume_binding.as_ref().is_some_and(|target| {
                target.execution.as_ref()
                    != binding
                        .as_ref()
                        .and_then(|binding| binding.execution.as_ref())
            }) {
                return Ok(SidWrite::Skipped);
            }
        }
        if instance.is_capture_excluded(session_id, observation.source.as_ref()) {
            return Ok(SidWrite::Skipped);
        }
        let owns = |sid: Option<&str>, owner: Option<&ConversationBinding>| {
            sid == Some(session_id)
                && crate::session::capture::owner_excludes(
                    observation.source.as_ref(),
                    owner,
                    session_id,
                )
        };
        let conflict = instances.iter().any(|peer| {
            peer.id != instance_id
                && (owns(
                    peer.agent_session_id.as_deref(),
                    peer.agent_session_binding.as_ref(),
                ) || peer.prior_tool_session_ids.values().any(|parked| {
                    owns(
                        parked.agent_session_id.as_deref(),
                        parked.agent_session_binding.as_ref(),
                    )
                }))
        });
        if conflict {
            return Ok(SidWrite::Skipped);
        }
        let instance = &mut instances[index];
        let confirms_pin = observation.confirms_omp_pin(&instance.resume_intent);
        // A source-less observation of the id the row already holds is not
        // evidence that a conversation qualified, nor that a failed resume now
        // works; keep the binding and the loop breaker.
        let establishes = binding.is_some();
        let new_conversation = instance.agent_session_id.as_deref() != Some(session_id);
        let binding = binding.or_else(|| instance.observed_binding(observation));
        instance.set_agent_conversation(
            Some(session_id.into()),
            binding,
            observation.pi_session_path.clone(),
        );
        if establishes || new_conversation {
            instance.resume_probe_failed_sid = None;
        }
        if confirms_pin {
            instance.resume_intent = ResumeIntent::Default;
            instance.resume_binding = None;
        }
        Ok(SidWrite::Applied)
    });
    match result {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(target: "session.store", "Cannot persist captured conversation: {error}");
            SidWrite::Failed
        }
    }
}

impl Instance {
    pub(super) fn persist_session_id(
        &mut self,
        profile: &str,
        expected: &ConversationState,
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

        self.persist_session_id_with_storage(&storage, expected)
    }

    fn persist_session_id_with_storage(
        &mut self,
        storage: &crate::session::storage::Storage,
        expected: &ConversationState,
    ) -> SidPersistOutcome {
        let expected_prior_intent = expected.intent.clone();
        let new_sid = self.agent_session_id.clone();
        // Cleared and Fork are one-shot launch directives. Use stays durable
        // only when no pane-scoped capture backend can observe a later `/new`;
        // capture-backed agents hand ownership back to their poller.
        let promote_one_shot = matches!(
            self.resume_intent,
            ResumeIntent::Cleared | ResumeIntent::Fork { .. }
        ) || matches!(self.resume_intent, ResumeIntent::Use(_))
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
            let Some(index) = instances
                .iter()
                .position(|instance| instance.id == instance_id)
            else {
                return Ok(SidWrite::Failed);
            };
            if !expected.matches(&instances[index]) {
                return Ok(SidWrite::Skipped);
            }
            if let Some(sid) = new_sid_for_closure.as_deref() {
                let binding = self
                    .agent_session_binding
                    .as_ref()
                    .filter(|binding| binding.session_id == sid);
                let source = binding
                    .filter(|binding| binding.provenance != ConversationProvenance::Unknown)
                    .and_then(|binding| binding.execution.as_ref());
                let owns = |id: Option<&str>, owner: Option<&ConversationBinding>| {
                    id == Some(sid) && crate::session::capture::owner_excludes(source, owner, sid)
                };
                let consumed_pin = binding.is_some_and(|binding| {
                    let Some(original) = expected.resume_binding.as_ref() else {
                        return false;
                    };
                    if !binding.is_known() {
                        return false;
                    }
                    match (&expected_prior_intent_for_closure, &self.resume_intent) {
                        (ResumeIntent::Use(pinned), _) if pinned == sid => original == binding,
                        (ResumeIntent::Use(pinned), ResumeIntent::Use(current)) => {
                            current == sid
                                && original.session_id == *pinned
                                && original.is_known()
                                && self.resume_binding.as_ref() == Some(binding)
                                && binding
                                    .execution
                                    .as_ref()
                                    .is_some_and(|execution| execution.agent == "hermes")
                                && binding.execution == original.execution
                                && binding.provenance == original.provenance
                                && binding.transcript_path == original.transcript_path
                        }
                        _ => false,
                    }
                });
                let refuses_transfer = |id: Option<&str>, owner: Option<&ConversationBinding>| {
                    owns(id, owner)
                        && (!consumed_pin
                            || !owner
                                .filter(|binding| binding.session_id == sid)
                                .is_some_and(ConversationBinding::is_known))
                };
                if instances
                    .iter()
                    .filter(|peer| peer.id != instance_id)
                    .any(|peer| {
                        refuses_transfer(
                            peer.agent_session_id.as_deref(),
                            peer.agent_session_binding.as_ref(),
                        ) || peer.prior_tool_session_ids.values().any(|parked| {
                            refuses_transfer(
                                parked.agent_session_id.as_deref(),
                                parked.agent_session_binding.as_ref(),
                            )
                        })
                    })
                {
                    return Ok(SidWrite::Skipped);
                }
                for peer in instances.iter_mut().filter(|peer| peer.id != instance_id) {
                    if owns(
                        peer.agent_session_id.as_deref(),
                        peer.agent_session_binding.as_ref(),
                    ) {
                        cleared_holder_ids.push(peer.id.clone());
                        peer.set_agent_conversation(None, None, None);
                        peer.resume_probe_failed_sid = None;
                    }
                    peer.prior_tool_session_ids.retain(|_, parked| {
                        if owns(
                            parked.agent_session_id.as_deref(),
                            parked.agent_session_binding.as_ref(),
                        ) {
                            parked.agent_session_id = None;
                            parked.agent_session_binding = None;
                            parked.pi_session_path = None;
                        }
                        !parked.is_empty()
                    });
                }
            }
            let instance = &mut instances[index];
            instance.set_agent_conversation(
                new_sid_for_closure.clone(),
                self.agent_session_binding.clone(),
                self.pi_session_path.clone(),
            );
            instance.active_execution = self.active_execution.clone();
            instance
                .retroactive_capture_excludes
                .clone_from(&self.retroactive_capture_excludes);
            instance.resume_probe_failed_sid = None;
            if promote_one_shot {
                instance.resume_intent = ResumeIntent::Default;
                instance.resume_binding = None;
            } else {
                instance.resume_intent = self.resume_intent.clone();
                instance.resume_binding = self.resume_binding.clone();
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
                            self.adopt_conversation_state(disk.conversation_state());
                            self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        }
                    }
                }
                if await_omp_pin_confirmation {
                    self.set_agent_conversation(None, None, None);
                }
                SidPersistOutcome::Published
            }
            // PinnedForeign can only be produced by or_pinned_foreign_publication;
            // these storage-update closures never emit it, so it shares the
            // divergence path without changing behavior.
            Ok(SidWrite::Skipped) | Ok(SidWrite::PinnedForeign) => match storage.load() {
                Ok(insts) => match insts.into_iter().find(|i| i.id == self.id) {
                    Some(disk) => {
                        self.adopt_conversation_state(disk.conversation_state());
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        let disk_still_awaits_confirmation = matches!(
                            (&self.resume_intent, self.agent_session_id.as_deref()),
                            (ResumeIntent::Use(pinned), Some(sid)) if pinned == sid
                        );
                        if await_omp_pin_confirmation && disk_still_awaits_confirmation {
                            self.set_agent_conversation(None, None, None);
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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

        let storage =
            crate::session::storage::Storage::new_unwatched("cas-persist-mismatch").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.agent_session_id = Some("old".into());
        let expected = inst.conversation_state();
        inst.agent_session_id = Some("peer-wrote".into());
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
            &crate::session::poller::SessionIdObservation::unguarded("ours".into()),
            &expected,
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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

        let storage = crate::session::storage::Storage::new_unwatched("cas-persist-match").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.agent_session_id = Some("old".to_string());
        let id = inst.id.clone();
        let expected = inst.conversation_state();
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
            &crate::session::poller::SessionIdObservation::unguarded("new".into()),
            &expected,
            &crate::file_watch::FileWatchService::noop(),
        );
        assert_eq!(outcome, super::SidWrite::Applied);

        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some("new"));
    }

    #[test]
    #[serial]
    fn source_less_observation_cannot_withdraw_a_qualified_binding() {
        let temp = tempdir().unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        let profile = "source-less-observation";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let sid = "019342ab-1234-7def-8901-abcdef012345";
        let execution = crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![temp.path().join("claude")],
            configuration: Vec::new(),
            cwd: "/tmp/x".into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        inst.set_agent_conversation(
            Some(sid.into()),
            Some(ConversationBinding {
                session_id: sid.into(),
                execution: Some(execution.clone()),
                provenance: ConversationProvenance::Observed,
                transcript_path: None,
            }),
            None,
        );
        inst.resume_probe_failed_sid = Some(sid.into());
        let on_disk = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = on_disk.clone();
                *g = crate::session::GroupTree::new_with_groups(&on_disk, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        // A plain sidecar publication carries no launch evidence, so it may
        // refresh the transcript path but it cannot withdraw the qualification
        // or the loop breaker for the id the row already holds.
        let mut source_less = crate::session::poller::SessionIdObservation::unguarded(sid.into());
        source_less.pi_session_path = Some("/sidecar/transcript.jsonl".into());
        assert_eq!(
            super::persist_session_to_storage(
                profile,
                &inst.id,
                &source_less,
                &inst.conversation_state(),
                &crate::file_watch::FileWatchService::noop(),
            ),
            super::SidWrite::Applied
        );

        let loaded = storage.load().unwrap();
        let row = loaded.iter().find(|row| row.id == inst.id).unwrap();
        assert_eq!(
            row.agent_session_binding
                .as_ref()
                .and_then(|binding| binding.execution.as_ref()),
            Some(&execution),
            "a source-less observation must keep the qualified binding"
        );
        assert_eq!(
            row.resume_probe_failed_sid.as_deref(),
            Some(sid),
            "a source-less observation must keep the resume loop breaker"
        );
        assert_eq!(
            row.pi_session_path.as_deref(),
            Some("/sidecar/transcript.jsonl"),
            "a source-less observation still refreshes the transcript path"
        );

        let mut live = inst.clone();
        live.apply_conversation_observation(&source_less);
        assert_eq!(
            live.agent_session_binding
                .as_ref()
                .and_then(|binding| binding.execution.as_ref()),
            Some(&execution),
            "the in-memory apply must keep the qualified binding"
        );

        // A different id is a different conversation: its publication must not
        // inherit the previous binding or loop breaker.
        let expected = row.conversation_state();
        assert_eq!(
            super::persist_session_to_storage(
                profile,
                &inst.id,
                &crate::session::poller::SessionIdObservation::unguarded(
                    "019342ac-5678-7def-8901-abcdef012345".into()
                ),
                &expected,
                &crate::file_watch::FileWatchService::noop(),
            ),
            super::SidWrite::Applied
        );
        let loaded = storage.load().unwrap();
        let row = loaded.iter().find(|row| row.id == inst.id).unwrap();
        assert!(row.agent_session_binding.is_none());
        assert_eq!(row.resume_probe_failed_sid, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn persist_session_to_storage_delivers_notification_to_in_process_subscriber() {
        use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp = tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            &crate::session::poller::SessionIdObservation::unguarded("new-sid".into()),
            &inst.conversation_state(),
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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            &ConversationState {
                session_id: Some("stale".to_owned()),
                intent: ResumeIntent::Default,
                ..inst.conversation_state()
            },
        );

        assert_eq!(inst.agent_session_id.as_deref(), Some("peer-wrote"));
    }

    #[test]
    #[serial]
    fn persist_session_id_atomic_writes_both_fields_on_match() {
        let temp = tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let _ = inst.persist_session_id(
            "persist-atomic-match",
            &ConversationState {
                session_id: None.map(str::to_owned),
                intent: ResumeIntent::Cleared,
                ..inst.conversation_state()
            },
        );

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

        let outcome = inst.persist_session_id_with_storage(
            &storage,
            &ConversationState {
                session_id: None.map(str::to_owned),
                intent: ResumeIntent::Default,
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let _ = inst.persist_session_id(
            profile,
            &ConversationState {
                session_id: expected_sid.as_deref().map(str::to_owned),
                intent: expected_prior,
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let _ = inst.persist_session_id(
            profile,
            &ConversationState {
                session_id: expected_sid.as_deref().map(str::to_owned),
                intent: expected_prior,
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let outcome = inst.persist_session_id(
            profile,
            &ConversationState {
                session_id: expected_sid.as_deref().map(str::to_owned),
                intent: expected_intent,
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());
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
        let _ = inst.persist_session_id(
            profile,
            &ConversationState {
                session_id: expected_sid.as_deref().map(str::to_owned),
                intent: inst.resume_intent.clone(),
                ..inst.conversation_state()
            },
        );
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use(pinned.into()),
            "configured capture support is not enough without a launch publisher"
        );

        inst.identity_publisher_launched = true;
        let _ = inst.persist_session_id(
            profile,
            &ConversationState {
                session_id: expected_sid.as_deref().map(str::to_owned),
                intent: inst.resume_intent.clone(),
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let _ = inst.persist_session_id(
            "persist-default-intent",
            &ConversationState {
                session_id: None.map(str::to_owned),
                intent: ResumeIntent::Default,
                ..inst.conversation_state()
            },
        );

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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            &ConversationState {
                session_id: Some("019342aa-2222-7eee-8fff-aaaabbbbcccc".to_owned()),
                intent: ResumeIntent::Default,
                ..inst.conversation_state()
            },
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
    fn persist_session_id_rejects_a_concurrent_intent_change() {
        let temp = tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
        let expected = inst.conversation_state();
        storage
            .update(|i, _g| {
                i[0].resume_intent = ResumeIntent::Use("peer-pinned".to_string());
                i[0].resume_binding = Some(ConversationBinding::unknown("peer-pinned"));
                Ok(())
            })
            .unwrap();

        let candidate = "019342ab-1234-7def-8901-abcdef012345";
        inst.set_agent_conversation(
            Some(candidate.into()),
            Some(ConversationBinding::unknown(candidate)),
            None,
        );
        let _ = inst.persist_session_id("persist-intent-mismatch", &expected);

        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id, None);
        assert_eq!(
            loaded[0].resume_binding,
            Some(ConversationBinding::unknown("peer-pinned"))
        );
        assert_eq!(inst.conversation_state(), loaded[0].conversation_state());
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
        let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            &ConversationState {
                session_id: Some("stale".to_owned()),
                intent: ResumeIntent::Cleared,
                ..inst.conversation_state()
            },
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
            persist_session_to_storage, ConversationState, Instance, ResumeIntent,
            SidPersistOutcome, SidWrite,
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
            let write = persist_session_to_storage(
                profile,
                &claimant.id,
                &crate::session::poller::SessionIdObservation::unguarded(SID_X.into()),
                &claimant.conversation_state(),
                &file_watch,
            );

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
        fn capture_respects_parked_owners_and_abandoned_namespaces() {
            use crate::session::instance::{ActiveExecution, PriorToolSession};
            use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-scoped-exclusions";
            let source = ExecutionBinding {
                agent: "omp".into(),
                stores: vec![temp.path().join("sessions/bucket")],
                configuration: Vec::new(),
                cwd: temp.path().into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            };
            for own in [false, true] {
                for namespace in ["same", "different", "unknown"] {
                    let mut claimant = make_inst(profile, "claimant");
                    let mut search = source.clone();
                    search.stores = vec![temp.path().join("sessions")];
                    claimant.active_execution = Some(ActiveExecution {
                        launch_id: "qualified-launch".into(),
                        binding: search,
                        capture: None,
                        container: None,
                    });
                    let mut bound = source.clone();
                    if namespace == "different" {
                        bound.stores = vec![temp.path().join("other-store/bucket")];
                    }
                    let binding = if namespace == "unknown" {
                        ConversationBinding::unknown(SID_X)
                    } else {
                        ConversationBinding {
                            session_id: SID_X.into(),
                            execution: Some(bound),
                            provenance: ConversationProvenance::Observed,
                            transcript_path: None,
                        }
                    };
                    let mut peer = make_inst(profile, "parked-owner");
                    if own {
                        seed(profile, &[&peer, &claimant]);
                        claimant.retroactive_capture_excludes.insert(binding);
                        let expected = claimant.conversation_state();
                        let storage = Storage::new_unwatched(profile).unwrap();
                        let _ = claimant.persist_session_id_with_storage(&storage, &expected);
                    } else {
                        peer.prior_tool_session_ids.insert(
                            "omp".into(),
                            PriorToolSession {
                                agent_session_id: Some(SID_X.into()),
                                agent_session_binding: Some(binding),
                                ..Default::default()
                            },
                        );
                        seed(profile, &[&peer, &claimant]);
                    }
                    let mut observed =
                        crate::session::poller::SessionIdObservation::unguarded(SID_X.into());
                    observed.execution = claimant.active_execution.clone();
                    observed.source = Some(source.clone());
                    let applied = namespace == "different";
                    assert_eq!(
                        persist_session_to_storage(
                            profile,
                            &claimant.id,
                            &observed,
                            &claimant.conversation_state(),
                            &FileWatchService::noop()
                        ),
                        if applied {
                            SidWrite::Applied
                        } else {
                            SidWrite::Skipped
                        },
                        "own={own}, namespace={namespace}"
                    );
                    let rows = load(profile);
                    let saved = rows.iter().find(|row| row.id == claimant.id).unwrap();
                    assert_eq!(saved.agent_session_id.as_deref(), applied.then_some(SID_X));
                }
            }
        }

        #[test]
        #[serial]
        fn persist_rejects_sid_contradicting_on_disk_pin() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-pin";

            // A capture must not replace the durable user-selected target.
            let mut pinned = make_inst(profile, "pinned");
            pinned.agent_session_id = Some(SID_X.to_string());
            pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
            seed(profile, &[&pinned]);

            let file_watch = FileWatchService::noop();
            let write = persist_session_to_storage(
                profile,
                &pinned.id,
                &crate::session::poller::SessionIdObservation::unguarded(SID_Y.into()),
                &pinned.conversation_state(),
                &file_watch,
            );
            assert_eq!(write, SidWrite::PinnedForeign);
            assert_eq!(
                load(profile)[0].agent_session_id.as_deref(),
                Some(SID_X),
                "pin must stay authoritative against a differing write"
            );

            // A write matching the pin is normal capture and must pass.
            let write = persist_session_to_storage(
                profile,
                &pinned.id,
                &crate::session::poller::SessionIdObservation::unguarded(SID_X.into()),
                &pinned.conversation_state(),
                &file_watch,
            );
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
            let outcome = live.persist_session_id_with_storage(
                &storage,
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Default,
                    ..live.conversation_state()
                },
            );

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

            let binding = crate::session::ConversationBinding {
                session_id: SID_X.into(),
                execution: Some(crate::session::ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![temp.path().join("claude")],
                    configuration: Vec::new(),
                    cwd: "/tmp/x".into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                }),
                provenance: crate::session::ConversationProvenance::Asserted,
                transcript_path: None,
            };
            let mut stale_holder = make_inst(profile, "stale-holder");
            stale_holder.set_agent_conversation(Some(SID_X.into()), Some(binding.clone()), None);
            let mut second_holder = make_inst(profile, "second-holder");
            second_holder.set_agent_conversation(Some(SID_X.into()), Some(binding.clone()), None);
            let mut parked_holder = make_inst(profile, "parked-holder");
            parked_holder.prior_tool_session_ids.insert(
                "claude".into(),
                crate::session::instance::PriorToolSession {
                    agent_session_id: Some(SID_X.into()),
                    agent_session_binding: Some(binding.clone()),
                    acp_session_id: Some("independent-acp-history".into()),
                    pi_session_path: None,
                },
            );
            let mut pinned = make_inst(profile, "pinned");
            pinned.resume_intent = ResumeIntent::Use(SID_X.into());
            pinned.resume_binding = Some(binding.clone());
            let expected = pinned.conversation_state();
            seed(
                profile,
                &[&stale_holder, &second_holder, &parked_holder, &pinned],
            );

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = pinned.clone();
            live.set_agent_conversation(Some(SID_X.into()), Some(binding.clone()), None);
            live.identity_publisher_launched = true;
            let outcome = live.persist_session_id_with_storage(&storage, &expected);

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
            let parked = &disk
                .iter()
                .find(|row| row.id == parked_holder.id)
                .unwrap()
                .prior_tool_session_ids["claude"];
            assert!(parked.agent_session_id.is_none());
            assert_eq!(
                parked.acp_session_id.as_deref(),
                Some("independent-acp-history")
            );

            parked_holder
                .prior_tool_session_ids
                .get_mut("claude")
                .unwrap()
                .agent_session_binding = None;
            seed(profile, &[&parked_holder, &pinned]);
            let mut refused = pinned.clone();
            refused.set_agent_conversation(Some(SID_X.into()), Some(binding), None);
            let _ = refused.persist_session_id_with_storage(&storage, &expected);
            let disk = load(profile);
            assert!(disk
                .iter()
                .find(|row| row.id == pinned.id)
                .unwrap()
                .agent_session_id
                .is_none());
            assert_eq!(
                disk.iter()
                    .find(|row| row.id == parked_holder.id)
                    .unwrap()
                    .prior_tool_session_ids["claude"]
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
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
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Use(SID_X.to_string()),
                    ..live.conversation_state()
                },
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
        use super::super::{ConversationState, Instance, ResumeIntent, Status};
        use serial_test::serial;
        use std::collections::HashSet;
        use tempfile::tempdir;

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
        fn hermes_canonical_publication_preserves_authority_and_rejects_false_success() {
            if skip_if_no_tmux() {
                return;
            }
            for case in [
                "explicit",
                "default-owner",
                "unknown-owner",
                "stale-generation",
                "exact-reload",
                "unavailable",
            ] {
                let temp = tempdir().unwrap();
                let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
                let profile = "hermes-canonical-publication";
                let mut instance = make_inst(profile, case);
                instance.tool = "my-hermes".into();
                instance.command = "my-hermes".into();
                let original_binding = crate::session::ConversationBinding {
                    session_id: "S".into(),
                    execution: Some(crate::session::ExecutionBinding {
                        agent: "hermes".into(),
                        stores: vec![temp.path().join("state.db")],
                        configuration: Vec::new(),
                        cwd: "/tmp/x".into(),
                        cwd_filesystem: "host".into(),
                        filesystem: "host".into(),
                    }),
                    provenance: crate::session::ConversationProvenance::Asserted,
                    transcript_path: None,
                };
                instance.set_agent_conversation(
                    Some("S".into()),
                    Some(original_binding.clone()),
                    None,
                );
                if case != "default-owner" {
                    instance.resume_intent = ResumeIntent::Use("S".into());
                    instance.resume_binding = Some(original_binding.clone());
                }
                if case == "explicit" {
                    instance.set_agent_conversation(Some("previous".into()), None, None);
                }
                seed_disk_row(profile, &instance);
                let expected = instance.conversation_state();
                let mut target_binding = original_binding;
                target_binding.session_id = "T".into();
                instance.set_agent_conversation(
                    Some("T".into()),
                    Some(target_binding.clone()),
                    None,
                );
                if case != "default-owner" {
                    instance.resume_intent = ResumeIntent::Use("T".into());
                    instance.resume_binding = Some(target_binding.clone());
                }
                instance.active_execution = Some(crate::session::instance::ActiveExecution {
                    launch_id: uuid::Uuid::new_v4().to_string(),
                    binding: target_binding.execution.clone().unwrap(),
                    capture: None,
                    container: None,
                });
                let desired = instance.conversation_state();
                let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
                if matches!(case, "explicit" | "default-owner" | "unknown-owner") {
                    let mut owner = make_inst(profile, "owner");
                    let binding = if case == "unknown-owner" {
                        crate::session::ConversationBinding::unknown("T")
                    } else {
                        target_binding
                    };
                    owner.set_agent_conversation(Some("T".into()), Some(binding), None);
                    storage
                        .update(|rows, _| {
                            rows.push(owner.clone());
                            Ok(())
                        })
                        .unwrap();
                } else if matches!(case, "stale-generation" | "exact-reload") {
                    let mut committed = instance.clone();
                    if case == "stale-generation" {
                        committed.active_execution.as_mut().unwrap().launch_id =
                            "older-generation".into();
                    }
                    storage
                        .update(|rows, _| {
                            rows[0] = committed.clone();
                            Ok(())
                        })
                        .unwrap();
                }
                let tmux = TmuxSession::create(&instance.id, &instance.title);
                let profile_dir = crate::session::get_profile_dir_path(profile).unwrap();
                let saved = temp.path().join("saved-profile");
                if case == "unavailable" {
                    std::fs::rename(&profile_dir, &saved).unwrap();
                    std::fs::write(&profile_dir, b"not a directory").unwrap();
                }
                let result = instance.finalize_launch(tmux.name(), profile, &expected, None, true);
                let success = matches!(case, "explicit" | "exact-reload");
                assert_eq!(result.is_ok(), success, "{case}: {result:?}");
                assert_eq!(
                    captured_env(tmux.name()).as_deref(),
                    success.then_some("T"),
                    "{case}"
                );
                assert!(instance.session_id_poller.is_none(), "{case}");
                if case == "unavailable" {
                    assert!(desired.matches(&instance));
                    std::fs::remove_file(&profile_dir).unwrap();
                    std::fs::rename(&saved, &profile_dir).unwrap();
                }
                let rows = storage.load().unwrap();
                let row = rows.iter().find(|row| row.id == instance.id).unwrap();
                if success {
                    assert!(desired.matches(row));
                    assert_eq!(row.resume_intent, ResumeIntent::Use("T".into()));
                } else if case == "stale-generation" {
                    assert_eq!(
                        row.active_execution.as_ref().unwrap().launch_id,
                        "older-generation"
                    );
                } else {
                    assert!(expected.matches(row));
                }
                if let Some(owner) = rows.iter().find(|row| row.id != instance.id) {
                    assert_eq!(
                        owner.agent_session_id.as_deref(),
                        if success { None } else { Some("T") }
                    );
                }
            }
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
                super::super::persist_session_to_storage(
                    profile,
                    &inst.id,
                    &crate::session::poller::SessionIdObservation::omp(
                        stale_sid.into(),
                        old_generation.into()
                    ),
                    &inst.conversation_state(),
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
            poller.inject_test_observation(
                &inst.id,
                crate::session::poller::SessionIdObservation::omp(
                    sid.to_string(),
                    generation.to_string(),
                ),
            );
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
                crate::session::capture::compose_exclusion("other-instance", &extra, None);
            assert!(other_exclusion.contains(PEER_SID));

            let own_exclusion = crate::session::capture::compose_exclusion(&peer.id, &extra, None);
            assert!(!own_exclusion.contains(PEER_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_omp_metadata() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let profile = "publish-applied";
            let mut inst = make_inst(profile, "fpaw");
            inst.tool = "omp".to_string();
            inst.pending_host_env = vec![
                ("OMP_PROFILE".to_string(), "work".to_string()),
                ("PI_CONFIG_DIR".to_string(), "/custom".to_string()),
            ];
            inst.agent_session_id = None;
            let context = crate::session::capture::resolve_omp_store_layout_with_environment(
                std::collections::HashMap::from([
                    ("HOME".into(), temp.path().display().to_string()),
                    ("OMP_PROFILE".into(), "work".into()),
                    ("PI_CONFIG_DIR".into(), "/custom".into()),
                ]),
                &inst.project_path,
                &inst.omp_capture_options().unwrap(),
            )
            .unwrap();
            let plan = inst
                .resolve_omp_capture_plan(&context, None)
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
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                Some(crate::session::capture::OmpCaptureMetadata {
                    layout: plan.layout,
                    launched_at_ms: 1000,
                    launch_id: plan.launch_id.clone(),
                    launch_marker: plan.launch_marker.clone(),
                    routing_fingerprint: plan.routing_fingerprint.clone(),
                    container_runtime: plan.container_runtime,
                }),
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let profile = "publish-applied-opencode";
            let mut inst = make_inst(profile, "fpaw-oc");
            inst.tool = "opencode".to_string();
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let profile = "publish-skipped-some";
            let mut inst = make_inst(profile, "fpsdspd");
            inst.agent_session_id = Some(PEER_SID.to_string());
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                &ConversationState {
                    session_id: Some("stale".to_owned()),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
                &ConversationState {
                    session_id: Some("stale".to_owned()),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            inst.finalize_launch(
                tmux.name(),
                profile,
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

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
            inst.finalize_launch(
                tmux.name(),
                profile,
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Default,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

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
            let _home_guard = crate::session::test_support::isolate_home(temp.path());

            let profile = "publish-promote";
            let mut inst = make_inst(profile, "fppca");
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Cleared;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                &ConversationState {
                    session_id: None.map(str::to_owned),
                    intent: ResumeIntent::Cleared,
                    ..inst.conversation_state()
                },
                None,
                false,
            )
            .unwrap();

            assert_eq!(inst.agent_session_id.as_deref(), Some(VALID_SID));
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }
    }
}
