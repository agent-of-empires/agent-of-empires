//! Reconciling an in-memory row against what peers wrote to disk.

use super::*;

impl Instance {
    /// Reload this instance from disk before a launch that would re-persist
    /// peer-writable fields. Refreshes `agent_session_id` (poller-observed)
    /// and `resume_intent` (user-set) from disk; carries runtime-only fields
    /// (`#[serde(skip)]` + `source_profile`) onto the disk snapshot. Closes
    /// the ~2s `status_poll_loop` lag window in which a CLI peer
    /// `set-session-id` would otherwise be silently overwritten. No-op on
    /// storage error or if the row is gone from disk.
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
        let mut disk = match storage.load() {
            Ok(instances) => match instances.into_iter().find(|i| i.id == self.id) {
                Some(d) => d,
                None => return,
            },
            Err(e) => {
                tracing::warn!(target: "session.store",
                    session = %self.id,
                    error = %e,
                    "failed to load disk state before launch; using in-memory value");
                return;
            }
        };

        // Carry runtime-only fields (`#[serde(skip)]`) and locally-mutated
        // launch-time state from `self` onto the disk snapshot. This carry
        // set is not required to match `merge_runtime_fields` exactly: each
        // reconciliation path feeds a different consumer, and each consumer
        // rewrites the runtime field it observes before reading
        // (`pane_dead_observed` is rewritten by the TUI's status poller
        // before its consumers read).
        let disk_has_newer_lifecycle = disk.lifecycle_generation > self.lifecycle_generation;
        if !disk_has_newer_lifecycle {
            disk.last_error_check = self.last_error_check;
            disk.last_error = self.last_error.take();
        }
        disk.last_start_time = self.last_start_time;
        disk.session_id_poller = self.session_id_poller.take();
        disk.session_id_poller_retry_after = self.session_id_poller_retry_after;
        disk.retroactive_capture_excludes = std::mem::take(&mut self.retroactive_capture_excludes);
        disk.pane_dead_observed = self.pane_dead_observed;
        disk.force_fresh_next_launch = self.force_fresh_next_launch;
        disk.pending_host_env = std::mem::take(&mut self.pending_host_env);
        disk.identity_publisher_launched = self.identity_publisher_launched;
        disk.source_profile = std::mem::take(&mut self.source_profile);
        disk.ever_confirmed_present = self.ever_confirmed_present;
        disk.unknown_since = self.unknown_since;
        // `before_start_env` is `#[serde(skip)]`, so the disk snapshot always
        // has it empty. Carry the live value forward; otherwise this reload
        // (which runs before every launch) would wipe the host-minted cache and
        // make `get_container_for_instance` re-run the before_start hook on each
        // relaunch of an already-running container, defeating the one-time
        // backfill and re-minting credentials needlessly.
        if let (Some(disk_sandbox), Some(runtime_sandbox)) =
            (disk.sandbox_info.as_mut(), self.sandbox_info.as_ref())
        {
            disk_sandbox.before_start_env = runtime_sandbox.before_start_env.clone();
        }

        *self = disk;
    }

    /// Closes the data-loss window where `/clear` writes the sidecar but
    /// the daemon crashes before the next poll tick persists it: without
    /// this step, the next launch's wipe destroys the fresh sid.
    ///
    /// Claude-only (sole sidecar tool); `Default` intent only (`Use(X)`
    /// and `Cleared` override); excluded sids skipped (cascade re-poison
    /// guard).
    pub(super) fn reconcile_sidecar_into_disk(&mut self) {
        if !matches!(
            self.resolved_capture_backend(),
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
        let Some(fresh) = crate::hooks::read_hook_session_id_any_age(&self.id) else {
            return;
        };
        if Some(&fresh) == self.agent_session_id.as_ref() {
            return;
        }
        if self.retroactive_capture_excludes.contains(&fresh) {
            return;
        }
        let profile = self.effective_profile();
        let baseline = self.agent_session_id.as_deref();
        match persist_session_to_storage(
            &profile,
            &self.id,
            &fresh,
            baseline,
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.agent_session_id = Some(fresh);
            }
            SidWrite::Skipped => {
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

    #[test]
    #[serial]
    fn reconcile_from_disk_picks_up_peer_persist() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("reconcile-test").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "reconcile-test".to_string();
        inst.agent_session_id = Some("old-sid".to_string());
        let id = inst.id.clone();
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

        // Simulate a peer CLI `set-session-id` write to disk.
        let _ = super::persist_session_to_storage(
            "reconcile-test",
            &id,
            "new-sid",
            Some("old-sid"),
            &crate::file_watch::FileWatchService::noop(),
        );

        assert_eq!(inst.agent_session_id.as_deref(), Some("old-sid"));
        inst.reconcile_from_disk();
        assert_eq!(inst.agent_session_id.as_deref(), Some("new-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_preserves_publisher_launch_proof() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        let storage =
            crate::session::storage::Storage::new_unwatched("reconcile-publisher").unwrap();
        let mut inst = Instance::new("publisher proof", "/tmp/test");
        inst.source_profile = "reconcile-publisher".to_string();
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
        inst.identity_publisher_launched = true;

        inst.reconcile_from_disk();

        assert!(inst.identity_publisher_launched);
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_preserves_before_start_env() {
        // `before_start_env` is `#[serde(skip)]`, so the disk snapshot has
        // it empty. reconcile_from_disk (run before every launch) must carry
        // the live host-minted cache forward, or an already-running
        // container would re-mint on every relaunch.
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("reconcile-before-start").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "reconcile-before-start".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "img".to_string(),
            container_name: "ctr".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
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

        // Stamp a freshly-minted value into the in-memory cache only.
        inst.sandbox_info.as_mut().unwrap().before_start_env =
            vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())];

        inst.reconcile_from_disk();

        assert_eq!(
            inst.sandbox_info.as_ref().unwrap().before_start_env,
            vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())],
            "live before_start_env must survive the pre-launch disk reload"
        );
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_preserves_unknown_streak_tracking() {
        // `ever_confirmed_present` and `unknown_since` are both
        // `#[serde(skip)]`, so the disk snapshot always has them at their
        // defaults (`false` / `None`). reconcile_from_disk (run before
        // every launch) must carry the live values forward, or a
        // previously-confirmed-present session would lose its long
        // tolerance window and drop back to the short never-present one
        // on every relaunch.
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage =
            crate::session::storage::Storage::new_unwatched("reconcile-unknown-since").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "reconcile-unknown-since".to_string();
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

        // Stamp the runtime tracking state into the in-memory instance
        // only, mirroring what a live poll tick would have set.
        inst.ever_confirmed_present = true;
        let unknown_since = std::time::Instant::now() - std::time::Duration::from_secs(5);
        inst.unknown_since = Some(unknown_since);

        inst.reconcile_from_disk();

        assert!(
            inst.ever_confirmed_present,
            "ever_confirmed_present must survive the pre-launch disk reload"
        );
        assert_eq!(
            inst.unknown_since,
            Some(unknown_since),
            "unknown_since must survive the pre-launch disk reload"
        );
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_picks_up_peer_clear() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("reconcile-clear").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "reconcile-clear".to_string();
        inst.agent_session_id = Some("old-sid".to_string());
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
                i[0].agent_session_id = None;
                Ok(())
            })
            .unwrap();

        inst.reconcile_from_disk();
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    #[serial]
    fn reconcile_from_disk_picks_up_peer_resume_intent() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("intent-reconcile").unwrap();
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = "intent-reconcile".to_string();
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

        storage
            .update(|i, _g| {
                i[0].resume_intent = ResumeIntent::Use("peer-pinned".to_string());
                Ok(())
            })
            .unwrap();

        assert_eq!(inst.resume_intent, ResumeIntent::Default);
        inst.reconcile_from_disk();
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use("peer-pinned".to_string())
        );
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_adopts_fresh_sid_for_claude_default() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-adopt";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("stale-disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some(SIDECAR_TEST_FRESH_UUID)
        );
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(
            on_disk.agent_session_id.as_deref(),
            Some(SIDECAR_TEST_FRESH_UUID)
        );
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_adopts_published_cursor_conversation() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "cursor-sidecar-adopt";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "cursor".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("stale-disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);
        let dir = write_sidecar(&inst.id, "cursor-conversation-new");

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == inst.id)
            .unwrap();
        assert_eq!(
            on_disk.agent_session_id.as_deref(),
            Some("cursor-conversation-new")
        );
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_noop_without_identity_sidecar_backend() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-noop-tool";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "opencode".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_noop_when_intent_use() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-noop-use";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());
        inst.agent_session_id = Some("disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_noop_when_intent_cleared() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-noop-cleared";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Cleared;
        inst.agent_session_id = Some("disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_noop_when_sid_in_retroactive_excludes() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-noop-excluded";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("disk-sid".to_string());
        inst.retroactive_capture_excludes
            .insert(SIDECAR_TEST_FRESH_UUID.to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_noop_when_sidecar_absent() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-absent";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("disk-sid".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        inst.reconcile_sidecar_into_disk();

        assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
    }

    #[test]
    #[serial]
    fn reconcile_sidecar_reloads_on_cas_skip() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let profile = "sidecar-cas-skip";
        let mut inst = Instance::new("title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "claude".to_string();
        inst.resume_intent = ResumeIntent::Default;
        inst.agent_session_id = Some("memory-baseline".to_string());
        seed_disk_for_sidecar_test(profile, &inst);

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|i, _g| {
                i[0].agent_session_id = Some("peer-wrote-this".to_string());
                Ok(())
            })
            .unwrap();

        let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

        inst.reconcile_sidecar_into_disk();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(inst.agent_session_id.as_deref(), Some("peer-wrote-this"));
        let on_disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == inst.id)
            .unwrap();
        assert_eq!(on_disk.agent_session_id.as_deref(), Some("peer-wrote-this"));
    }
}
