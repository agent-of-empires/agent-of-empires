//! Reconciling an in-memory row against what peers wrote to disk.

use super::*;

impl Instance {
    /// Best-effort CLI reload; native launch uses the fallible store path.
    #[cfg(test)]
    pub(super) fn reconcile_from_disk(&mut self) {
        let Ok(storage) = crate::session::storage::Storage::new(
            &self.effective_profile(),
            self.resolve_file_watch(),
        ) else {
            tracing::warn!(target: "session.store",
                session = %self.id,
                "failed to open storage to reload disk state before launch; using in-memory value");
            return;
        };
        if let Err(error) = self.reconcile_from_store(&storage) {
            tracing::warn!(target: "session.store", session = %self.id, %error,
                "failed to reconcile session from disk");
        }
    }

    pub(crate) fn reconcile_from_store(
        &mut self,
        storage: &dyn crate::session::SessionStore,
    ) -> Result<()> {
        let mut disk = storage
            .load()?
            .into_iter()
            .find(|row| row.id == self.id)
            .ok_or(LifecycleReservationError::Superseded)?;
        let preserve_errors = disk.lifecycle_generation <= self.lifecycle_generation;
        disk.source_profile = if self.source_profile == storage.storage().profile() {
            std::mem::take(&mut self.source_profile)
        } else {
            storage.storage().profile().to_owned()
        };
        let prior = std::mem::replace(self, disk);
        self.inherit_runtime(prior, preserve_errors);
        Ok(())
    }

    /// Flush the previous pane's publication before its source is retired.
    pub(super) fn reconcile_sidecar_into_disk_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
    ) -> Result<()> {
        if self.source_capture_backend() == Some(crate::agents::SessionCaptureBackend::Pi) {
            self.absorb_published_pi_session_in(storage);
            return Ok(());
        }
        if !matches!(
            self.source_capture_backend(),
            Some(
                crate::agents::SessionCaptureBackend::Claude
                    | crate::agents::SessionCaptureBackend::HookSidecar
            )
        ) || !matches!(self.resume_intent, ResumeIntent::Default)
        {
            return Ok(());
        }
        let Some(observation) = super::execution::hook_session_observation(
            &self.id,
            self.active_execution.as_ref(),
            None,
        ) else {
            return Ok(());
        };
        let fresh = &observation.sid;
        let binding = self.observed_binding(&observation);
        if Some(fresh) == self.agent_session_id.as_ref() && self.agent_session_binding == binding {
            return Ok(());
        }
        if self.is_capture_excluded(fresh, observation.source.as_ref()) {
            return Ok(());
        }
        let baseline = self.conversation_state();
        match super::sid_persist::persist_session_with_storage(
            storage,
            &self.id,
            &observation,
            &baseline,
        ) {
            SidWrite::Applied => self.apply_conversation_observation(&observation),
            SidWrite::Skipped | SidWrite::PinnedForeign => {
                self.reconcile_from_store(storage)?;
            }
            SidWrite::Failed => {}
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn reconcile_sidecar_into_disk(&mut self) -> anyhow::Result<()> {
        let storage =
            crate::session::storage::Storage::new_unwatched(&self.effective_profile()).unwrap();
        self.reconcile_sidecar_into_disk_in(&storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    use serial_test::serial;
    use tempfile::tempdir;

    fn seeded(profile: &str, inst: &Instance) -> crate::session::storage::Storage {
        seed_disk_for_sidecar_test(profile, inst);
        crate::session::storage::Storage::new_unwatched(profile).unwrap()
    }

    fn disk_row(storage: &crate::session::storage::Storage, id: &str) -> Instance {
        storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_picks_up_peer_writes() {
        type ReconcileCase = (&'static str, fn(&mut Instance), fn(&Instance));
        let cases: &[ReconcileCase] = &[
            (
                "peer persist",
                |row| row.agent_session_id = Some("new-sid".to_string()),
                |inst| assert_eq!(inst.agent_session_id.as_deref(), Some("new-sid")),
            ),
            (
                "peer clear",
                |row| row.agent_session_id = None,
                |inst| assert_eq!(inst.agent_session_id, None),
            ),
            (
                "peer resume intent",
                |row| row.resume_intent = ResumeIntent::Use("peer-pinned".to_string()),
                |inst| {
                    assert_eq!(
                        inst.resume_intent,
                        ResumeIntent::Use("peer-pinned".to_string())
                    )
                },
            ),
        ];
        for (label, peer_write, expect) in cases {
            let temp = tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp.path());
            let profile = "reconcile-peer";
            let mut inst = Instance::new(label, "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("old-sid".to_string());
            let storage = seeded(profile, &inst);
            storage
                .update(|rows, _| {
                    peer_write(&mut rows[0]);
                    Ok(())
                })
                .unwrap();

            inst.reconcile_from_disk();

            expect(&inst);
        }
    }

    /// Runtime-only (`#[serde(skip)]`) state is absent from the disk snapshot, so the reload has to
    /// carry it across from memory.
    #[test]
    #[serial]
    fn reconcile_from_disk_keeps_runtime_only_state() {
        let temp = tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp.path());
        let profile = "reconcile-runtime";
        let mut inst = Instance::new("runtime state", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.sandbox_info = Some(test_sandbox("ctr", None));
        seeded(profile, &inst);

        let now = std::time::Instant::now();
        inst.poller_repair.defer(now);
        inst.poller_repair.defer(now);
        assert!(!inst.poller_repair.due(now));
        inst.identity_publisher_launched = true;
        inst.sandbox_info.as_mut().unwrap().before_start_env =
            vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())];
        inst.ever_confirmed_present = true;
        let unknown_since = now - std::time::Duration::from_secs(5);
        inst.unknown_since = Some(unknown_since);

        inst.reconcile_from_disk();

        assert!(!inst.poller_repair.due(now), "poller backoff must survive");
        assert!(inst.identity_publisher_launched);
        assert_eq!(
            inst.sandbox_info.as_ref().unwrap().before_start_env,
            vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())]
        );
        assert!(inst.ever_confirmed_present);
        assert_eq!(inst.unknown_since, Some(unknown_since));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_adopts_only_an_unclaimed_fresh_conversation() {
        struct Case {
            label: &'static str,
            tool: &'static str,
            intent: ResumeIntent,
            sidecar: Option<&'static str>,
            exclude_sidecar: bool,
            peer_sid: Option<&'static str>,
            want: &'static str,
        }
        let case = |label, tool, sidecar| Case {
            label,
            tool,
            intent: ResumeIntent::Default,
            sidecar,
            exclude_sidecar: false,
            peer_sid: None,
            want: "disk-sid",
        };
        let cases = [
            Case {
                want: SIDECAR_TEST_FRESH_UUID,
                ..case(
                    "claude adopts a fresh sidecar",
                    "claude",
                    Some(SIDECAR_TEST_FRESH_UUID),
                )
            },
            Case {
                want: "cursor-conversation-new",
                ..case(
                    "cursor adopts its published conversation",
                    "cursor",
                    Some("cursor-conversation-new"),
                )
            },
            case(
                "no identity sidecar backend",
                "opencode",
                Some(SIDECAR_TEST_FRESH_UUID),
            ),
            case("sidecar absent", "claude", None),
            Case {
                intent: ResumeIntent::Use("user-pinned".to_string()),
                ..case("user pin", "claude", Some(SIDECAR_TEST_FRESH_UUID))
            },
            Case {
                intent: ResumeIntent::Cleared,
                ..case("cleared intent", "claude", Some(SIDECAR_TEST_FRESH_UUID))
            },
            Case {
                exclude_sidecar: true,
                ..case(
                    "sid already excluded from capture",
                    "claude",
                    Some(SIDECAR_TEST_FRESH_UUID),
                )
            },
            Case {
                peer_sid: Some("peer-wrote-this"),
                want: "peer-wrote-this",
                ..case(
                    "CAS skip reloads the peer write",
                    "claude",
                    Some(SIDECAR_TEST_FRESH_UUID),
                )
            },
        ];
        for c in cases {
            let temp = tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(temp.path());
            let profile = "sidecar-reconcile";
            let mut inst = Instance::new(c.label, "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = c.tool.to_string();
            inst.resume_intent = c.intent;
            inst.agent_session_id = Some("disk-sid".to_string());
            if c.exclude_sidecar {
                inst.retroactive_capture_excludes
                    .insert(ConversationBinding::unknown(SIDECAR_TEST_FRESH_UUID));
            }
            let storage = seeded(profile, &inst);
            if let Some(peer) = c.peer_sid {
                storage
                    .update(|rows, _| {
                        rows[0].agent_session_id = Some(peer.to_string());
                        Ok(())
                    })
                    .unwrap();
            }
            let dir = c.sidecar.map(|sid| write_sidecar(&inst.id, sid));

            inst.reconcile_sidecar_into_disk().unwrap();

            if let Some(dir) = dir {
                std::fs::remove_dir_all(&dir).ok();
            }
            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some(c.want),
                "{}",
                c.label
            );
            assert_eq!(
                disk_row(&storage, &inst.id).agent_session_id.as_deref(),
                Some(c.want),
                "{}",
                c.label
            );
        }
    }
}
