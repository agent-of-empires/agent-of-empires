//! Background load-time healing for trashed and relocated worktrees.
//!
//! Both sweeps below repair durable state that a crash, an older aoe version,
//! or a `git worktree move` from another shell left inconsistent. Neither
//! belongs on the first-frame path: a broken row costs a lifecycle flock, a
//! storage write, and git spawns, so a store with a few hundred trashed rows
//! held the TUI at a blank screen for seconds before it could paint (#3611,
//! #3554). It is healing work, not render input, so it runs on a worker and
//! `HomeView::apply_reconcile_results` reloads once the sweep lands.

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
                ReconcileResult {
                    changed: sweep(&request.profiles),
                }
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

    #[test]
    fn try_recv_reports_empty_while_no_sweep_has_landed() {
        let mut poller = ReconcilePoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    /// A trashed managed worktree whose recorded dir is gone but whose holding
    /// dir exists needs only a pointer repair, so the sweep heals it without
    /// touching git. Proves the worker path reaches durable state.
    #[test]
    #[serial_test::serial]
    fn sweep_heals_a_trashed_pointer_and_reports_the_change() {
        let _guard = crate::session::test_support::isolate_app_dir();
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
