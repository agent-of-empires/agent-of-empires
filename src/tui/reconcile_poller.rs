//! Owned background load-time healing and durable intent recovery.

use std::sync::mpsc::TryRecvError;

use crate::tui::worker::Worker;

pub enum ReconcileRequest {
    Healing {
        profiles: Vec<String>,
    },
    Journal {
        storages: Vec<crate::session::Storage>,
        app_dir: std::path::PathBuf,
        all_pending: bool,
    },
}

pub enum ReconcileResult {
    Healing {
        changed: bool,
    },
    Journal {
        notices: Result<Vec<String>, String>,
    },
}

pub struct ReconcilePoller {
    worker: Worker<ReconcileRequest, ReconcileResult>,
    #[cfg(test)]
    seeded_result: Option<ReconcileResult>,
    #[cfg(test)]
    skip_healing: bool,
}

impl ReconcilePoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn(
                "aoe-reconcile-poller",
                |request: ReconcileRequest| match request {
                    ReconcileRequest::Healing { profiles } => ReconcileResult::Healing {
                        changed: sweep(&profiles),
                    },
                    ReconcileRequest::Journal {
                        storages,
                        app_dir,
                        all_pending,
                    } => ReconcileResult::Journal {
                        notices: recover(&storages, &app_dir, all_pending)
                            .map_err(|error| format!("{error:#}")),
                    },
                },
            ),
            #[cfg(test)]
            seeded_result: None,
            #[cfg(test)]
            skip_healing: false,
        }
    }

    pub fn request(&mut self, profiles: Vec<String>) {
        #[cfg(test)]
        if self.skip_healing {
            return;
        }
        self.worker.request(ReconcileRequest::Healing { profiles });
    }

    pub fn request_journal(
        &mut self,
        storages: Vec<crate::session::Storage>,
        app_dir: std::path::PathBuf,
        all_pending: bool,
    ) {
        self.worker.request(ReconcileRequest::Journal {
            storages,
            app_dir,
            all_pending,
        });
    }

    pub fn try_recv_result(&mut self) -> Result<ReconcileResult, TryRecvError> {
        #[cfg(test)]
        if let Some(result) = self.seeded_result.take() {
            return Ok(result);
        }
        self.worker.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn with_result_for_test(changed: bool) -> Self {
        let mut poller = Self::new();
        poller.seeded_result = Some(ReconcileResult::Healing { changed });
        poller.skip_healing = true;
        poller
    }

    #[cfg(test)]
    pub(crate) fn set_healing_result_for_test(&mut self, changed: Option<bool>) {
        self.seeded_result = changed.map(|changed| ReconcileResult::Healing { changed });
    }
    #[cfg(test)]
    pub(crate) fn finish_for_test(&mut self) -> std::thread::Result<()> {
        self.worker.finish_for_test()
    }
}

impl Default for ReconcilePoller {
    fn default() -> Self {
        Self::new()
    }
}

fn recover(
    storages: &[crate::session::Storage],
    app_dir: &std::path::Path,
    all_pending: bool,
) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        app_dir.canonicalize()? == crate::session::get_app_dir_path()?.canonicalize()?,
        "recovery namespace changed"
    );
    if all_pending {
        return crate::session::Storage::recover_pending_moves(
            crate::file_watch::FileWatchService::noop(),
        );
    }
    let loaded = storages
        .iter()
        .map(|storage| storage.load().map(|rows| (storage.profile(), rows)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let loads = loaded
        .iter()
        .map(|(profile, rows)| (*profile, rows.as_slice()))
        .collect::<Vec<_>>();
    let stores = storages
        .iter()
        .map(|storage| (storage.profile(), storage))
        .collect::<Vec<_>>();
    Ok(crate::session::reconcile_profile_duplicates(&loads, &stores).conflicts)
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
