//! Reconciling an in-memory row against what peers wrote to disk.

use super::*;

impl Instance {
    /// Reload this instance from disk before a launch that would re-persist peer-writable fields.
    pub(super) fn reconcile_from_disk(&mut self) {
        if let Err(error) = self.try_reconcile_from_disk() {
            tracing::warn!(target: "session.store",
                session = %self.id,
                error = %format_args!("{error:#}"),
                "failed to reload disk state before launch; using in-memory value");
        }
    }

    /// [`Self::reconcile_from_disk`] that reports a storage failure. `Ok(false)`
    /// means the row is gone from disk and `self` is unchanged.
    pub(super) fn try_reconcile_from_disk(&mut self) -> Result<bool> {
        let storage = crate::session::storage::Storage::new(
            &self.effective_profile(),
            self.resolve_file_watch(),
        )
        .context("failed to open storage")?;
        let Some(mut disk) = storage
            .load()
            .context("failed to load sessions")?
            .into_iter()
            .find(|i| i.id == self.id)
        else {
            return Ok(false);
        };

        // Carry runtime-only fields (`#[serde(skip)]`) and locally-mutated launch-time state from
        // `self` onto the disk snapshot.
        let disk_has_newer_lifecycle = disk.lifecycle_generation > self.lifecycle_generation;
        if !disk_has_newer_lifecycle {
            disk.last_error_check = self.last_error_check;
            disk.last_error = self.last_error.take();
        }
        if self.active_execution != disk.active_execution {
            self.stop_poller();
            self.session_id_poller = None;
        }
        disk.last_start_time = self.last_start_time;
        disk.session_id_poller = self.session_id_poller.take();
        disk.session_id_poller_retry_after = self.session_id_poller_retry_after;
        // Preserve the serde-skipped backoff so reloads cannot trigger an early retry.
        disk.poller_repair = self.poller_repair.clone();
        disk.pane_dead_observed = self.pane_dead_observed;
        disk.force_fresh_next_launch = self.force_fresh_next_launch;
        disk.pending_host_env = std::mem::take(&mut self.pending_host_env);
        disk.identity_publisher_launched = self.identity_publisher_launched;
        disk.source_profile = std::mem::take(&mut self.source_profile);
        disk.ever_confirmed_present = self.ever_confirmed_present;
        disk.unknown_since = self.unknown_since;
        // `before_start_env` is `#[serde(skip)]`, so the disk snapshot always has it empty.
        if let (Some(disk_sandbox), Some(runtime_sandbox)) =
            (disk.sandbox_info.as_mut(), self.sandbox_info.as_ref())
        {
            disk_sandbox.before_start_env = runtime_sandbox.before_start_env.clone();
        }

        *self = disk;
        Ok(true)
    }

    /// Flush the previous pane's publication before its source is retired.
    pub(super) fn reconcile_sidecar_into_disk(&mut self) {
        if self.source_capture_backend() == Some(crate::agents::SessionCaptureBackend::Pi) {
            self.absorb_published_pi_session();
            return;
        }
        if !matches!(
            self.source_capture_backend(),
            Some(
                crate::agents::SessionCaptureBackend::Claude
                    | crate::agents::SessionCaptureBackend::HookSidecar
            )
        ) {
            return;
        }
        if !matches!(self.resume_intent, ResumeIntent::Default) {
            return;
        }
        let Some(observation) = super::execution::hook_session_observation(
            &self.id,
            self.active_execution.as_ref(),
            None,
        ) else {
            return;
        };
        let fresh = &observation.sid;
        let binding = self.observed_binding(&observation);
        if Some(fresh) == self.agent_session_id.as_ref() && self.agent_session_binding == binding {
            return;
        }
        if self.is_capture_excluded(fresh, observation.source.as_ref()) {
            return;
        }
        let profile = self.effective_profile();
        let baseline = self.conversation_state();
        match persist_session_to_storage(
            &profile,
            &self.id,
            &observation,
            &baseline,
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.set_agent_conversation(Some(observation.sid), binding, None);
            }
            // A pinned-foreign publication is a deliberate non-write; like a
            // divergence skip, it carries no update worth reconciling.
            SidWrite::Skipped | SidWrite::PinnedForeign => {
                // Peer wrote between reconcile and CAS; reload to converge.
                self.reconcile_from_disk();
            }
            SidWrite::Failed => {}
        }
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

            inst.reconcile_sidecar_into_disk();

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
