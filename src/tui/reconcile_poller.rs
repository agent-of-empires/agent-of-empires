//! Background startup healing and tmux identity publication, before auto-recovery.

use std::sync::mpsc::TryRecvError;

use crate::tui::worker::Worker;

pub struct ReconcileRequest {
    /// Profiles to sweep, in the order the view loaded them.
    pub profiles: Vec<String>,
}

pub struct ReconcileResult {
    /// True when a durable row changed, so the view must reload to show it.
    pub changed: bool,
}

pub struct ReconcilePoller {
    worker: Worker<ReconcileRequest, ReconcileResult>,
}

impl ReconcilePoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-reconcile-poller", |request: ReconcileRequest| {
                let changed = sweep(&request.profiles);
                publish_session_environment(&request.profiles);
                ReconcileResult { changed }
            }),
        }
    }

    pub fn request(&mut self, profiles: Vec<String>) {
        self.worker.request(ReconcileRequest { profiles });
    }

    pub fn try_recv_result(&mut self) -> Result<ReconcileResult, TryRecvError> {
        self.worker.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn with_result_for_test(changed: bool) -> Self {
        Self {
            worker: Worker::seeded_for_test(
                "aoe-reconcile-poller-test",
                ReconcileResult { changed },
            ),
        }
    }
}

fn publish_session_environment(profiles: &[String]) {
    use crate::session::Storage;
    use crate::tmux::env::{AOE_CAPTURED_SESSION_ID_KEY, AOE_INSTANCE_ID_KEY};

    let live = crate::tmux::LiveSessionSnapshot::new();
    let mut set_batch = Vec::new();
    let mut unset_batch = Vec::new();
    for profile in profiles {
        let instances = match Storage::open_unwatched(profile).and_then(|storage| storage.load()) {
            Ok(instances) => instances,
            Err(error) => {
                tracing::warn!(target: "tui.home", %profile, %error, "tmux identity publication skipped");
                continue;
            }
        };
        for instance in instances {
            let Some(name) = instance.tmux_env_session_name_in_or_probe(&live) else {
                continue;
            };
            set_batch.push((name.clone(), AOE_INSTANCE_ID_KEY, instance.id));
            if let Some(sid) = instance.agent_session_id {
                set_batch.push((name, AOE_CAPTURED_SESSION_ID_KEY, sid));
            } else {
                unset_batch.push((name, AOE_CAPTURED_SESSION_ID_KEY));
            }
        }
    }
    if !set_batch.is_empty() {
        let refs: Vec<_> = set_batch
            .iter()
            .map(|(name, key, value)| (name.as_str(), *key, value.as_str()))
            .collect();
        if let Err(error) = crate::tmux::env::set_hidden_env_batch(&refs) {
            tracing::warn!(target: "tui.home", %error, "tmux identity publication failed");
        }
    }
    if !unset_batch.is_empty() {
        let refs: Vec<_> = unset_batch
            .iter()
            .map(|(name, key)| (name.as_str(), *key))
            .collect();
        if let Err(error) = crate::tmux::env::remove_hidden_env_batch(&refs) {
            tracing::warn!(target: "tui.home", %error, "tmux identity removal failed");
        }
    }
}

impl Default for ReconcilePoller {
    fn default() -> Self {
        Self::new()
    }
}

/// Run both healing sweeps over `profiles`. Returns true when anything changed.
///
/// Storage is opened unwatched: the writes land from a worker thread, and the
/// view reloads from the returned verdict rather than from a local-change
/// notification.
fn sweep(profiles: &[String]) -> bool {
    let mut changed = false;
    for profile in profiles {
        match crate::session::trash::reconcile_trashed_profile(profile) {
            Ok(healed) => changed |= !healed.is_empty(),
            Err(error) => tracing::warn!(
                target: "tui.home",
                profile = %profile,
                "trash reconciliation skipped: {error}",
            ),
        }
        changed |= crate::session::worktree_reconcile::reconcile_profile(profile);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Storage;
    use crate::session::{Instance, WorktreeInfo};

    /// A trashed managed worktree whose recorded dir is gone but whose holding
    /// dir exists needs only a pointer repair, so the sweep heals it without
    /// touching git. Proves the worker path reaches durable state.
    #[test]
    #[serial_test::serial]
    fn sweep_heals_a_trashed_pointer_and_reports_the_change() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let mut poller = ReconcilePoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
        let project = tempfile::tempdir().unwrap();
        let storage = Storage::new_unwatched("default").unwrap();

        let mut instance = Instance::new("trashed", project.path().join("feat").to_str().unwrap());
        instance.worktree_info = Some(WorktreeInfo {
            managed_by_aoe: true,
            branch: "feat".to_string(),
            main_repo_path: project.path().to_string_lossy().into_owned(),
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        instance.trash();
        let holding =
            crate::session::trash::trash_holding_path(&project.path().join("feat"), &instance.id)
                .unwrap();
        std::fs::create_dir_all(&holding).unwrap();
        let id = instance.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(instance);
                Ok(())
            })
            .unwrap();

        assert!(sweep(&["default".to_string()]));
        let healed = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .unwrap();
        assert_eq!(healed.project_path, holding.to_string_lossy());
        assert_eq!(
            healed.pre_trash_project_path.as_deref(),
            project.path().join("feat").to_str(),
        );
        // Idempotent: a second pass has nothing left to do.
        assert!(!sweep(&["default".to_string()]));
    }
}
