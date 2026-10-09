use super::watchers::{ConfigWatchKey, WatcherInitError};
use super::*;
use crate::session::Status;
use crate::tui::worker::Worker;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub(super) type EditTokens = HashMap<String, u64>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RowDeletion {
    pub revision: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub(super) type RowDeletions = HashMap<String, RowDeletion>;

pub(super) struct ProfileSaveSnapshot {
    pub name: String,
    pub storage: Storage,
    pub revision: u64,
    pub rows: Vec<Instance>,
    pub groups: Vec<Group>,
    pub deletions: RowDeletions,
    pub group_deletions: EditTokens,
    pub additions: EditTokens,
}

pub(super) struct SaveSnapshot {
    pub profiles: Vec<ProfileSaveSnapshot>,
}

pub(super) struct ProfileSaveDone {
    pub snapshot: ProfileSaveSnapshot,
    pub result: anyhow::Result<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum ReloadKind {
    Storage,
    Full,
    Reconciled,
}

pub(super) struct ReloadContext {
    pub storages: HashMap<String, Storage>,
    pub active_profile: Option<String>,
    pub kind: ReloadKind,
}

pub(super) struct ProfileLoad {
    pub name: String,
    pub storage: Storage,
    pub rows: Vec<Instance>,
    pub groups: Vec<Group>,
}

pub(super) struct ReloadSnapshot {
    pub profiles: Vec<ProfileLoad>,
    pub reports: Vec<crate::session::DuplicateIdReport>,
    pub projects: Vec<crate::session::Project>,
    pub hook_configs: Option<HashMap<String, crate::status_hooks::StatusHookConfig>>,
    pub config_profile: String,
    pub mouse_capture: Option<bool>,
    pub watches: WatchPass,
}

pub(in crate::tui) struct WatchView<K> {
    pub dirty: Arc<AtomicBool>,
    pub installed: HashMap<K, crate::file_watch::WatchIdentity>,
}

pub(super) struct WatchTargets {
    pub disk: Option<Vec<String>>,
    pub config: Option<Vec<String>>,
}

pub(super) struct WatchPass {
    pub disk: HashMap<String, crate::file_watch::WatchIdentity>,
    pub config: HashMap<ConfigWatchKey, crate::file_watch::WatchIdentity>,
    pub disk_error: Option<WatcherInitError>,
    pub config_error: Option<WatcherInitError>,
}

pub(super) enum PersistenceJob {
    Save(SaveSnapshot),
    Reload(ReloadContext),
    Rewire(WatchTargets),
    Transaction {
        save: SaveSnapshot,
        context: ReloadContext,
        request: Box<persistence_transactions::TransactionRequest>,
    },
    Close,
}

pub(super) struct PersistenceRequest {
    pub id: u64,
    pub job: PersistenceJob,
}

pub(super) enum PersistenceResult {
    Save(Vec<ProfileSaveDone>),
    Reload(anyhow::Result<ReloadSnapshot>),
    Rewired(WatchPass),
    Transaction {
        saved: Vec<ProfileSaveDone>,
        result: anyhow::Result<Box<TransactionCompletion>>,
        failure_snapshot: Option<ReloadSnapshot>,
    },
    Closed,
}

pub(super) struct TransactionCompletion {
    pub effect: persistence_transactions::TransactionEffect,
    pub snapshot: Option<ReloadSnapshot>,
}

pub(super) struct PersistenceDone {
    pub id: u64,
    pub result: PersistenceResult,
}

struct PersistenceState {
    file_watch: Arc<crate::file_watch::FileWatchService>,
    disk: DiskWatchState,
    config: ConfigWatchState,
    failures: ReloadFailureState,
}

pub(super) fn spawn(
    file_watch: Arc<crate::file_watch::FileWatchService>,
) -> (
    Worker<PersistenceRequest, PersistenceDone>,
    WatchView<String>,
    WatchView<ConfigWatchKey>,
) {
    let disk_dirty = Arc::new(AtomicBool::new(false));
    let config_dirty = Arc::new(AtomicBool::new(false));
    let mut state = PersistenceState {
        file_watch,
        disk: DiskWatchState {
            dirty: Arc::clone(&disk_dirty),
            handles: HashMap::new(),
        },
        config: ConfigWatchState {
            dirty: Arc::clone(&config_dirty),
            handles: HashMap::new(),
        },
        failures: ReloadFailureState::default(),
    };
    let runtime = tokio::runtime::Handle::try_current().ok();
    let worker = Worker::spawn("tui.persistence", move |request: PersistenceRequest| {
        let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
        let result = match request.job {
            PersistenceJob::Save(snapshot) => PersistenceResult::Save(persist_snapshot(snapshot)),
            PersistenceJob::Reload(mut context) => {
                PersistenceResult::Reload(state.reload(&mut context))
            }
            PersistenceJob::Rewire(targets) => PersistenceResult::Rewired(state.rewire(targets)),
            PersistenceJob::Transaction {
                save,
                mut context,
                request,
            } => {
                // The fence is this request's captured snapshot, not a preceding Save(None).
                let saved = persist_snapshot(save);
                let result = (|| {
                    for done in &saved {
                        if let Err(error) = &done.result {
                            anyhow::bail!(
                                "Accepted profile '{}' save fence failed: {error:#}",
                                done.snapshot.name
                            );
                        }
                        let missing = done.result.as_ref().unwrap();
                        request.try_for_each_captured_row(|row| {
                            anyhow::ensure!(
                                !done.snapshot.storage.same_origin_as(&row.origin.storage)
                                    || !missing.contains(&row.before.id),
                                "Original transaction row disappeared at its save fence: {}",
                                row.before.id
                            );
                            Ok(())
                        })?;
                    }
                    request.try_for_each_captured_storage(Storage::verify_profile_identity)?;
                    let effect = persistence_transactions::perform(*request, &state.file_watch)?;
                    let snapshot=match &effect {
                        persistence_transactions::TransactionEffect::Admitted(_)
                        | persistence_transactions::TransactionEffect::AdmissionCancelledBeforeEffect(_)
                        | persistence_transactions::TransactionEffect::PresentationSaved
                        | persistence_transactions::TransactionEffect::QuitConfirmationDisabled => None,
                        persistence_transactions::TransactionEffect::Switched {profile} => {context.active_profile=profile.clone();Some(state.reload(&mut context)?)},
                        persistence_transactions::TransactionEffect::ProfileDeleted(name)=>{context.storages.remove(name);if context.active_profile.as_deref()==Some(name) {context.active_profile=None;}Some(state.reload(&mut context)?)},
                        _ => Some(state.reload(&mut context)?),
                    };
                    Ok(Box::new(TransactionCompletion { effect, snapshot }))
                })();
                // Failure is not a continuation ACK. A read-only snapshot can still expose
                // a retained Creating row for its actual original recovery workflow.
                let failure_snapshot = if result.is_err() {
                    state.reload(&mut context).ok()
                } else {
                    None
                };
                PersistenceResult::Transaction {
                    saved,
                    result,
                    failure_snapshot,
                }
            }
            PersistenceJob::Close => {
                for (_, entry) in state.disk.handles.drain() {
                    watchers::drop_disk_watch_entry(entry);
                }
                for (_, entry) in state.config.handles.drain() {
                    watchers::drop_disk_watch_entry(entry);
                }
                PersistenceResult::Closed
            }
        };
        PersistenceDone {
            id: request.id,
            result,
        }
    });
    (
        worker,
        WatchView {
            dirty: disk_dirty,
            installed: HashMap::new(),
        },
        WatchView {
            dirty: config_dirty,
            installed: HashMap::new(),
        },
    )
}

fn persist_profile(snapshot: &ProfileSaveSnapshot) -> anyhow::Result<Vec<String>> {
    snapshot.storage.verify_profile_identity()?;
    snapshot
        .storage
        .update_under_workspace_claim_lock(|rows, groups| {
            rows.retain(|row| {
                !snapshot
                    .deletions
                    .get(&row.id)
                    .is_some_and(|deletion| deletion.created_at == row.created_at)
            });
            let mut peer_deleted = Vec::new();
            for edited in &snapshot.rows {
                if let Some(stored) = rows.iter_mut().find(|row| row.id == edited.id) {
                    if stored.created_at != edited.created_at {
                        peer_deleted.push(edited.id.clone());
                        continue;
                    }
                    let durable_status = stored.status;
                    stored.merge_from_tui(edited);
                    if edited.status == Status::Deleting {
                        stored.status = durable_status;
                    }
                } else if snapshot.additions.contains_key(&edited.id) {
                    if edited.status != Status::Deleting {
                        rows.push(edited.clone());
                    }
                } else {
                    peer_deleted.push(edited.id.clone());
                }
            }
            groups.retain(|group| !snapshot.group_deletions.contains_key(&group.path));
            for edited in &snapshot.groups {
                if let Some(stored) = groups.iter_mut().find(|group| group.path == edited.path) {
                    stored.name.clone_from(&edited.name);
                    stored.collapsed = edited.collapsed;
                    stored.archived_at = edited.archived_at;
                }
                // Group creation is an acknowledged transaction, not a save of a stale
                // view tree. A peer-deleted group must never be recreated by an overlay.
            }
            Ok(peer_deleted)
        })
}

fn persist_snapshot(snapshot: SaveSnapshot) -> Vec<ProfileSaveDone> {
    let fences = (|| {
        let workspace = crate::session::acquire_session_workspace_claim_lock()?;
        let identity = crate::session::acquire_session_identity_lock()?;
        anyhow::Ok((workspace, identity))
    })();
    match fences {
        Ok(_fences) => snapshot
            .profiles
            .into_iter()
            .map(|snapshot| {
                let result = persist_profile(&snapshot);
                ProfileSaveDone { snapshot, result }
            })
            .collect(),
        Err(error) => snapshot
            .profiles
            .into_iter()
            .map(|snapshot| ProfileSaveDone {
                snapshot,
                result: Err(anyhow::anyhow!("{error:#}")),
            })
            .collect(),
    }
}

pub(super) fn load_projects(
    active_profile: Option<&str>,
    config_profile: &str,
    storages: &HashMap<String, Storage>,
) -> Vec<crate::session::Project> {
    use crate::session::projects::{canonical_key, load_merged};
    if active_profile.is_some() {
        return load_merged(config_profile).unwrap_or_default();
    }
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for profile in storages.keys() {
        for project in load_merged(profile).unwrap_or_default() {
            if seen.insert(canonical_key(&project.path)) {
                merged.push(project);
            }
        }
    }
    merged
}

fn collect_loads(storages: &HashMap<String, Storage>) -> anyhow::Result<Vec<ProfileLoad>> {
    storages
        .iter()
        .map(|(name, storage)| {
            storage.verify_profile_identity()?;
            if let Err(error) = storage.reconcile_filesystem_claims() {
                tracing::warn!(target: "tui.file_watch", profile = %name, %error, "filesystem claims retained after uncertain reconciliation");
            }
            let (mut rows, groups) = storage.load_with_groups()?;
            storage.verify_profile_identity()?;
            for row in &mut rows {
                row.source_profile.clone_from(name);
            }
            Ok(ProfileLoad {
                name: name.clone(),
                storage: storage.clone(),
                rows,
                groups,
            })
        })
        .collect()
}

impl PersistenceState {
    fn rewire(&mut self, targets: WatchTargets) -> WatchPass {
        if let Some(disk) = targets.disk {
            self.disk
                .rewire(&self.file_watch, &disk, &mut self.failures);
        }
        if let Some(config) = targets.config {
            self.config
                .rewire(&self.file_watch, &config, &mut self.failures);
        }
        WatchPass {
            disk: self
                .disk
                .handles
                .iter()
                .map(|(key, entry)| (key.clone(), entry.installed_identity))
                .collect(),
            config: self
                .config
                .handles
                .iter()
                .map(|(key, entry)| (key.clone(), entry.installed_identity))
                .collect(),
            disk_error: self.failures.disk_watcher_init_error.clone(),
            config_error: self.failures.config_watcher_init_error.clone(),
        }
    }

    fn reload(&mut self, context: &mut ReloadContext) -> anyhow::Result<ReloadSnapshot> {
        let profiles = crate::session::list_profiles().unwrap_or_else(|error| {
            tracing::warn!(target: "tui.file_watch", %error, "profile discovery failed; retaining captured stores");
            let mut names: Vec<_> = context.storages.keys().cloned().collect();
            names.sort();
            names
        });
        {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            let mut desired = context
                .active_profile
                .as_ref()
                .map_or_else(|| profiles.clone(), |name| vec![name.clone()]);
            for name in context.storages.keys() {
                if !desired.contains(name) {
                    desired.push(name.clone());
                }
            }
            for name in desired {
                let valid = context
                    .storages
                    .get(&name)
                    .is_some_and(|storage| storage.verify_profile_identity().is_ok());
                if !valid {
                    if profiles.contains(&name) {
                        context.storages.insert(
                            name.clone(),
                            Storage::open(&name, Arc::clone(&self.file_watch))?,
                        );
                    } else {
                        context.storages.remove(&name);
                    }
                }
            }
        }
        let mut loads = collect_loads(&context.storages)?;
        let loads_view: Vec<_> = loads
            .iter()
            .map(|load| (load.name.as_str(), load.rows.as_slice()))
            .collect();
        let stores_view: Vec<_> = context
            .storages
            .iter()
            .map(|(name, storage)| (name.as_str(), storage))
            .collect();
        let reconciled = crate::session::reconcile_profile_duplicates(&loads_view, &stores_view);
        if reconciled.repaired {
            loads = collect_loads(&context.storages)?;
        }
        let config_profile = context
            .active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile);
        let hook_configs = (context.kind == ReloadKind::Full).then(|| {
            HomeView::load_status_hook_configs(HomeView::status_hook_profile_names(
                context.active_profile.as_deref(),
                &context.storages,
            ))
        });
        let mouse_capture = (context.kind == ReloadKind::Full)
            .then(|| {
                crate::session::resolve_config(&config_profile)
                    .map(|config| crate::tui::mouse_capture_requested(&config.session))
                    .ok()
            })
            .flatten();
        let projects = load_projects(
            context.active_profile.as_deref(),
            &config_profile,
            &context.storages,
        );
        let mut disk: Vec<_> = context.storages.keys().cloned().collect();
        disk.sort();
        let watches = self.rewire(WatchTargets {
            disk: Some(disk),
            config: Some(profiles),
        });
        Ok(ReloadSnapshot {
            profiles: loads,
            reports: reconciled.reports,
            projects,
            hook_configs,
            config_profile,
            mouse_capture,
            watches,
        })
    }
}
