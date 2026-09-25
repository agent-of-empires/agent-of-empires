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
    use crate::file_watch::FileWatchService;
    use crate::session::storage::Storage;
    use crate::session::GroupTree;
    use serial_test::serial;
    use tempfile::{tempdir, TempDir};

    const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";

    fn expected_state(sid: Option<&str>, intent: ResumeIntent) -> ConversationState {
        ConversationState {
            session_id: sid.map(str::to_owned),
            binding: None,
            intent,
            resume_binding: None,
            active: None,
            pi_session_path: None,
        }
    }

    fn observation(sid: &str) -> crate::session::poller::SessionIdObservation {
        crate::session::poller::SessionIdObservation::unguarded(sid.to_owned())
    }
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
                &observation("new"),
                &expected_state(prior, ResumeIntent::Default),
                &FileWatchService::noop(),
            );
            assert_eq!(write, expected);
            assert_eq!(disk_sid(profile, &inst.id).as_deref(), Some(disk));
        }
    }

    #[test]
    #[serial]
    fn captured_foreign_sid_cannot_replace_a_pinned_conversation() {
        const OTHER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";
        let profile = "capture-pinned-foreign";
        let mut inst = make_inst(profile, "pinned");
        inst.agent_session_id = Some(VALID_SID.into());
        inst.resume_intent = ResumeIntent::Use(VALID_SID.into());
        let (_temp, _home, storage) = seeded(profile, &[&inst]);
        let expected = inst.conversation_state();

        assert_eq!(
            persist_session_with_storage(&storage, &inst.id, &observation(OTHER_SID), &expected),
            SidWrite::PinnedForeign
        );
        let disk = storage.load().unwrap();
        assert_eq!(disk[0].agent_session_id.as_deref(), Some(VALID_SID));
        assert_eq!(disk[0].resume_intent, ResumeIntent::Use(VALID_SID.into()));

        assert_eq!(
            persist_session_with_storage(&storage, &inst.id, &observation(VALID_SID), &expected),
            SidWrite::Applied,
            "the pin must still accept its own conversation"
        );
    }

    #[test]
    #[serial]
    fn foreign_and_parked_owners_guard_published_sid_by_namespace() {
        use crate::session::instance::{ActiveExecution, PriorToolSession};
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let sid = VALID_SID;
        for (namespace, expected) in [
            ("same", SidWrite::Skipped),
            ("unknown", SidWrite::Skipped),
            ("different", SidWrite::Applied),
        ] {
            let profile = "sid-parked-owner-namespace";
            let mut claimant = make_inst(profile, "claimant");
            let source = ExecutionBinding {
                agent: "omp".into(),
                stores: vec!["/tmp/sessions/bucket".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: "/tmp/x".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            };
            claimant.active_execution = Some(ActiveExecution {
                launch_id: "qualified".into(),
                binding: source.clone(),
                capture: None,
                container: None,
            });
            let mut parked = make_inst(profile, "parked");
            let mut other = source.clone();
            other.stores = vec!["/tmp/other-store/bucket".into()];
            let binding = if namespace == "unknown" {
                ConversationBinding::unknown(sid)
            } else {
                ConversationBinding {
                    session_id: sid.into(),
                    execution: Some(if namespace == "same" {
                        source.clone()
                    } else {
                        other
                    }),
                    provenance: ConversationProvenance::Observed,
                    transcript_path: None,
                }
            };
            parked.prior_tool_session_ids.insert(
                "omp".into(),
                PriorToolSession {
                    agent_session_id: Some(sid.into()),
                    agent_session_binding: Some(binding),
                    ..Default::default()
                },
            );
            let (_tmp, _home, storage) = seeded(profile, &[&parked, &claimant]);
            let mut observed = observation(sid);
            observed.execution = claimant.active_execution.clone();
            observed.source = Some(source);
            assert_eq!(
                persist_session_to_storage(
                    profile,
                    &claimant.id,
                    &observed,
                    &claimant.conversation_state(),
                    &FileWatchService::noop()
                ),
                expected,
                "{namespace}"
            );
            assert_eq!(
                disk_sid(profile, &claimant.id).as_deref(),
                (expected == SidWrite::Applied).then_some(sid),
                "{namespace}"
            );
            assert_eq!(
                storage.load().unwrap()[0].prior_tool_session_ids["omp"]
                    .agent_session_id
                    .as_deref(),
                Some(sid)
            );
        }
        let profile = "sid-foreign-on-disk";
        let mut owner = make_inst(profile, "owner");
        owner.agent_session_id = Some(sid.into());
        let claimant = make_inst(profile, "claimant");
        let (_tmp, _home, _) = seeded(profile, &[&owner, &claimant]);
        assert_eq!(
            persist_session_to_storage(
                profile,
                &claimant.id,
                &observation(sid),
                &claimant.conversation_state(),
                &FileWatchService::noop()
            ),
            SidWrite::Skipped
        );
        assert_eq!(disk_sid(profile, &claimant.id), None);
        assert_eq!(disk_sid(profile, &owner.id).as_deref(), Some(sid));
    }

    #[test]
    #[serial]
    fn known_pin_transfers_all_compatible_owners_but_not_stale_or_unknown_ones() {
        use crate::session::instance::PriorToolSession;
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let profile = "sid-pin-owner-transfer";
        let sid = VALID_SID;
        let binding = ConversationBinding {
            session_id: sid.into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/tmp/claude-store".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: "/tmp/x".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            }),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        };
        let mut first = make_inst(profile, "first");
        first.set_agent_conversation(Some(sid.into()), Some(binding.clone()), None);
        let mut second = make_inst(profile, "second");
        second.set_agent_conversation(Some(sid.into()), Some(binding.clone()), None);
        let mut parked = make_inst(profile, "parked");
        parked.prior_tool_session_ids.insert(
            "claude".into(),
            PriorToolSession {
                agent_session_id: Some(sid.into()),
                agent_session_binding: Some(binding.clone()),
                acp_session_id: Some("unrelated-acp".into()),
                ..Default::default()
            },
        );
        let mut pinned = make_inst(profile, "pinned");
        pinned.resume_intent = ResumeIntent::Use(sid.into());
        pinned.resume_binding = Some(binding.clone());
        let expected = pinned.conversation_state();
        let (_tmp, _home, storage) = seeded(profile, &[&first, &second, &parked, &pinned]);
        let mut live = pinned.clone();
        live.set_agent_conversation(Some(sid.into()), Some(binding.clone()), None);
        live.identity_publisher_launched = true;
        assert_eq!(
            live.persist_session_id_with_storage(&storage, &expected),
            SidPersistOutcome::Published
        );
        let disk = storage.load().unwrap();
        assert_eq!(
            disk.iter()
                .find(|row| row.id == pinned.id)
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some(sid)
        );
        for owner in [&first, &second] {
            assert_eq!(
                disk.iter()
                    .find(|row| row.id == owner.id)
                    .unwrap()
                    .agent_session_id,
                None
            );
        }
        let parked_state = &disk
            .iter()
            .find(|row| row.id == parked.id)
            .unwrap()
            .prior_tool_session_ids["claude"];
        assert_eq!(parked_state.agent_session_id, None);
        assert_eq!(
            parked_state.acp_session_id.as_deref(),
            Some("unrelated-acp")
        );

        parked
            .prior_tool_session_ids
            .get_mut("claude")
            .unwrap()
            .agent_session_binding = None;
        seed(profile, &[&parked, &pinned]);
        let mut refused = pinned.clone();
        refused.set_agent_conversation(Some(sid.into()), Some(binding.clone()), None);
        assert_eq!(
            refused.persist_session_id_with_storage(&storage, &expected),
            SidPersistOutcome::Published
        );
        assert_eq!(disk_sid(profile, &pinned.id), None);
        assert_eq!(
            storage
                .load()
                .unwrap()
                .iter()
                .find(|row| row.id == parked.id)
                .unwrap()
                .prior_tool_session_ids["claude"]
                .agent_session_id
                .as_deref(),
            Some(sid)
        );

        first.agent_session_id = Some(sid.into());
        let launcher = make_inst(profile, "stale-launcher");
        seed(profile, &[&first, &launcher]);
        let mut stale = launcher.clone();
        stale.agent_session_id = Some(sid.into());
        let stale_expected = ConversationState {
            intent: ResumeIntent::Use(sid.into()),
            ..launcher.conversation_state()
        };
        assert_eq!(
            stale.persist_session_id_with_storage(&storage, &stale_expected),
            SidPersistOutcome::Published
        );
        assert_eq!(disk_sid(profile, &launcher.id), None);
        assert_eq!(disk_sid(profile, &first.id).as_deref(), Some(sid));
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

        let write = persist_session_to_storage(
            profile,
            &inst.id,
            &observation("new-sid"),
            &expected_state(Some("old"), ResumeIntent::Default),
            &svc,
        );
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

        let outcome = inst.persist_session_id_with_storage(
            &storage,
            &expected_state(None, ResumeIntent::Default),
        );
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
    fn finalize_cas_loss_preserves_peer_repin_instead_of_promoting_it() {
        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";
        let profile = "finalize-peer-repin";
        let mut inst = make_inst(profile, "pinned Claude");
        inst.tool = "claude".into();
        inst.agent_session_id = Some(VALID_SID.into());
        inst.resume_intent = ResumeIntent::Use(VALID_SID.into());
        let (_temp, _home, storage) = seeded(profile, &[&inst]);
        let expected = inst.conversation_state();

        // A peer changes only the intent while this launch is in flight. A
        // SID-only CAS would consume the peer's new pin as if this launch won.
        storage
            .update(|instances, _| {
                instances[0].resume_intent = ResumeIntent::Use(PEER_SID.into());
                Ok(())
            })
            .unwrap();
        inst.identity_publisher_launched = true;
        assert_eq!(
            inst.persist_session_id_with_storage(&storage, &expected),
            SidPersistOutcome::Published
        );
        let disk = storage.load().unwrap();
        assert_eq!(disk[0].resume_intent, ResumeIntent::Use(PEER_SID.into()));
        assert_eq!(inst.resume_intent, disk[0].resume_intent);
        assert_eq!(inst.agent_session_id, disk[0].agent_session_id);
    }

    /// Store routing is not conversation identity: a pinned conversation is still captured
    /// when its launch exports a default store the pin was recorded without (#4119).
    #[test]
    #[serial]
    fn pinned_capture_ignores_the_exported_default_store_flag() {
        let profile = "pinned-exported-default";
        let binding = |exported_default_store| crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec!["/home/me/.claude".into()],
            configuration: Vec::new(),
            exported_default_store,
            cwd: "/tmp/x".into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        let active = crate::session::instance::ActiveExecution {
            launch_id: "launch".into(),
            binding: binding(true),
            capture: None,
            container: None,
        };
        let mut inst = make_inst(profile, "pinned");
        inst.agent_session_id = Some(VALID_SID.into());
        inst.resume_intent = ResumeIntent::Use(VALID_SID.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: VALID_SID.into(),
            execution: Some(binding(false)),
            provenance: crate::session::ConversationProvenance::Asserted,
            transcript_path: None,
        });
        inst.active_execution = Some(active.clone());
        let (_temp, _home, storage) = seeded(profile, &[&inst]);
        let observation = crate::session::poller::SessionIdObservation {
            execution: Some(active),
            source: Some(binding(true)),
            ..observation(VALID_SID)
        };

        assert_eq!(
            persist_session_with_storage(
                &storage,
                &inst.id,
                &observation,
                &inst.conversation_state()
            ),
            SidWrite::Applied
        );
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

        let expected = expected_state(Some(VALID_SID), inst.resume_intent.clone());
        let _ = inst.persist_session_id(profile, &expected);
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use(VALID_SID.into()),
            "configured capture support is not enough without a launch publisher"
        );
        inst.identity_publisher_launched = true;
        let expected = expected_state(Some(VALID_SID), inst.resume_intent.clone());
        let _ = inst.persist_session_id(profile, &expected);
        assert_eq!(inst.resume_intent, ResumeIntent::Default);
        assert_eq!(
            storage.load().unwrap()[0].resume_intent,
            ResumeIntent::Default
        );
        let next_sid = "019342aa-2222-7eee-8fff-aaaabbbbcccc";
        let expected = storage.load().unwrap()[0].conversation_state();
        assert_eq!(
            persist_session_with_storage(&storage, &inst.id, &observation(next_sid), &expected),
            SidWrite::Applied
        );
        assert_eq!(disk_sid(profile, &inst.id).as_deref(), Some(next_sid));
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
                persist_session_to_storage(
                    profile,
                    &inst.id,
                    &crate::session::poller::SessionIdObservation::omp(
                        "019342ab-1234-7def-8901-abcdef012349".into(),
                        old_generation.into(),
                    ),
                    &inst.conversation_state(),
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
            poller.inject_test_observation(
                &inst.id,
                crate::session::poller::SessionIdObservation::omp(
                    sid.to_owned(),
                    generation.to_owned(),
                ),
            );
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
                crate::session::capture::compose_exclusion("other-instance", &extra, None)
                    .contains(PEER_SID)
            );
            assert!(
                !crate::session::capture::compose_exclusion(&inst.id, &extra, None)
                    .contains(PEER_SID)
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
                inst.finalize_launch(
                    tmux.name(),
                    profile,
                    &expected_state(prior, prior_intent),
                    None,
                    false,
                )
                .unwrap();
                assert_eq!(captured_env(tmux.name()).as_deref(), want_env, "{label}");
                assert_eq!(inst.agent_session_id.as_deref(), want_sid, "{label}");
                assert_eq!(inst.resume_intent, want_intent, "{label}");
            }
        }
    }
}
