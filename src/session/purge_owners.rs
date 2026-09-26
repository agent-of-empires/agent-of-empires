//! Frozen resource ownership retained across irreversible purge failures.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::anchored_fs::ResolvedDataFile;
use super::deletion::DeletionRequest;
use super::path_identity::CleanupProtection;
use super::storage::{acquire_open_storage_flock, app_dir_for_profile_dir};
use super::{AnchoredDir, Instance, Storage};

pub(crate) const FILE_NAME: &str = "pending-purge-owners.json";

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RunnerCapture {
    Uncaptured,
    Captured { pid: u32, generation: u64 },
    Unresolved { error: String },
}

impl RunnerCapture {
    fn ensure_stopped(&self, session_id: Option<&str>) -> Result<()> {
        self.ensure_stopped_with(
            session_id,
            crate::process::worker_registry::load_strict,
            crate::process::worker::is_process_group_alive,
            peer_pid_for,
        )
    }

    fn ensure_stopped_with(
        &self,
        session_id: Option<&str>,
        load: impl FnOnce(&str) -> Result<Option<crate::process::worker_registry::WorkerRecord>>,
        process_group_alive: impl Fn(u32) -> bool,
        peer_pid: impl Fn(&str) -> Result<Option<u32>>,
    ) -> Result<()> {
        let Self::Unresolved { error } = self else {
            let Some(session_id) = session_id else {
                anyhow::bail!("Runner session identity is missing; purge resources retained");
            };
            let record = load(session_id)
                .map_err(|error| anyhow::anyhow!("worker registry is unreadable: {error:#}"))?;
            let peer_pid = peer_pid(session_id).map_err(|error| {
                anyhow::anyhow!("runner socket identity is unreadable: {error:#}")
            })?;
            let Self::Captured {
                pid, generation, ..
            } = self
            else {
                anyhow::ensure!(
                    record.is_none() && peer_pid.is_none(),
                    "Runner ownership changed after capture; purge resources retained"
                );
                return Ok(());
            };
            let alive = process_group_alive(*pid);
            match record {
                None => {
                    anyhow::ensure!(
                        !alive && peer_pid.is_none(),
                        "Captured runner {pid} has not exited; purge resources retained"
                    );
                }
                Some(record) => {
                    let identity = crate::acp::runner_lifecycle::RunnerIdentity {
                        pid: *pid,
                        generation: *generation,
                    };
                    anyhow::ensure!(
                        identity.matches_record(record.pid, record.generation)
                            && identity.generation == record.generation,
                        "Captured runner identity was replaced (pid {} generation {}); purge resources retained",
                        record.pid,
                        record.generation
                    );
                    anyhow::ensure!(
                        !alive
                            && !crate::process::worker_registry::is_record_live(&record)
                            && peer_pid.is_none_or(|peer| peer == *pid),
                        "Captured runner {pid} has not exited; purge resources retained"
                    );
                }
            }
            return Ok(());
        };
        anyhow::bail!("Runner ownership unresolved; purge resources retained: {error}")
    }

    fn serialized(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("runner capture is serializable")
    }
}

fn peer_pid_for(session_id: &str) -> Result<Option<u32>> {
    let base = crate::process::worker_registry::socket_path_for(session_id)?;
    let control = crate::process::worker::control_socket_sibling(&base);
    let peer_pid = crate::process::worker::peer_pid_from_socket(&control)
        .or_else(|| crate::process::worker::peer_pid_from_socket(&base));
    if peer_pid.is_none()
        && [control.as_path(), base.as_path()]
            .into_iter()
            .any(|path| std::fs::symlink_metadata(path).is_ok())
    {
        anyhow::bail!("runner socket exists but its peer identity could not be proven");
    }
    Ok(peer_pid)
}

fn capture_runner(session_id: &str) -> RunnerCapture {
    match crate::process::worker_registry::load_strict(session_id) {
        Ok(Some(record)) if record.pid > 0 && i32::try_from(record.pid).is_ok() => {
            RunnerCapture::Captured {
                pid: record.pid,
                generation: record.generation,
            }
        }
        Ok(Some(_)) => RunnerCapture::Unresolved {
            error: "Invalid captured runner pid".into(),
        },
        Ok(None) => match peer_pid_for(session_id) {
            Ok(None) => RunnerCapture::Uncaptured,
            Ok(Some(pid)) => RunnerCapture::Unresolved {
                error: format!("registry absent but runner socket peer {pid} is active"),
            },
            Err(error) => RunnerCapture::Unresolved {
                error: format!("runner socket probe failed: {error:#}"),
            },
        },
        Err(error) => RunnerCapture::Unresolved {
            error: format!("{error:#}"),
        },
    }
}

pub(crate) fn capture_runner_json(session_id: &str) -> serde_json::Value {
    capture_runner(session_id).serialized()
}

#[derive(Serialize, Deserialize)]
struct Owner {
    token: String,
    session_id: String,
    profile: String,
    generation: u64,
    protection: CleanupProtection,
    runner: RunnerCapture,
    #[serde(default)]
    request: Option<DeletionRequest>,
    #[serde(default)]
    additional_protection: Option<CleanupProtection>,
}

pub(super) struct RecoveryPlan {
    pub(super) token: String,
    pub(super) profile: String,
    pub(super) request: Option<DeletionRequest>,
    pub(super) additional_protection: Option<CleanupProtection>,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    owners: Vec<Owner>,
}

fn open(root: &Path) -> Result<ResolvedDataFile> {
    AnchoredDir::open(root)?.bind_file(FILE_NAME.as_ref())
}

fn open_for(storage: &Storage) -> Result<ResolvedDataFile> {
    let profile = storage
        .sessions_path()
        .parent()
        .context("profile directory missing")?;
    open(app_dir_for_profile_dir(profile))
}

fn decode(content: Option<&str>) -> Result<Journal> {
    let content = content.context("Pending purge ownership journal is missing")?;
    let journal =
        serde_json::from_str::<Journal>(content).context("reading pending purge ownership")?;
    anyhow::ensure!(
        journal.version == 2,
        "Unsupported pending purge ownership version"
    );
    let mut tokens = HashSet::with_capacity(journal.owners.len());
    for owner in &journal.owners {
        anyhow::ensure!(
            uuid::Uuid::parse_str(&owner.token).is_ok()
                && tokens.insert(&owner.token)
                && super::is_valid_session_id(&owner.session_id)
                && !owner.profile.is_empty(),
            "Invalid pending purge owner"
        );
        owner.protection.validate()?;
        if let RunnerCapture::Captured { pid, .. } = &owner.runner {
            anyhow::ensure!(
                *pid > 0 && i32::try_from(*pid).is_ok(),
                "Invalid captured runner pid"
            );
        }
        if let Some(request) = &owner.request {
            let source_profile_matches = request.instance.source_profile.is_empty()
                || request.instance.source_profile == owner.profile;
            anyhow::ensure!(
                request.session_id == owner.session_id
                    && request.instance.id == owner.session_id
                    && request.instance.lifecycle_generation == owner.generation
                    && source_profile_matches,
                "Pending purge request identity does not match its owner"
            );
        }
        if let Some(protection) = &owner.additional_protection {
            protection.validate()?;
        }
    }
    Ok(journal)
}

/// Decode the journal for a SIBLING reader that only needs to know which
/// sessions other namespaces still have in flight, treating an ABSENT file as
/// an empty one. A namespace created before v036 never had a journal and
/// genuinely has no pending owners, so its absence is not corruption and must
/// not fail the whole pass — the store reclaim would otherwise orphan every
/// store on an install that predates the migration. A journal that exists but
/// does not parse is still refused, so corruption stays fail-closed.
///
/// Deliberately NOT used by [`protection`] or [`recovery_plans`]: those drive
/// destructive cleanup and recovery, where "the journal is gone" is not proof
/// that nothing is in flight, so a missing file must keep failing them.
fn decode_sibling(content: Option<&str>) -> Result<Journal> {
    match content {
        Some(_) => decode(content),
        None => Ok(Journal {
            version: 2,
            owners: Vec::new(),
        }),
    }
}

pub(crate) fn validate_serialized(content: &str) -> Result<()> {
    decode(Some(content)).map(|_| ())
}

fn save(file: &ResolvedDataFile, journal: &Journal) -> Result<()> {
    file.replace(&serde_json::to_vec(journal)?)?;
    file.sync_parent()
}

pub(crate) fn initialize(root: &Path) -> Result<()> {
    let file = open(root)?;
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let content = file.read()?;
    match content.as_deref() {
        Some(content) => {
            decode(Some(content))?;
        }
        None => save(
            &file,
            &Journal {
                version: 2,
                owners: Vec::new(),
            },
        )?,
    }
    Ok(())
}

/// Dropping this handle deliberately leaves its durable ownership intact.
pub(crate) struct PurgeOwner {
    file: ResolvedDataFile,
    token: String,
    runner: RunnerCapture,
    session_id: Option<String>,
}

pub(crate) struct PurgeCapture {
    protection: CleanupProtection,
    runner: RunnerCapture,
    session_id: String,
}

impl PurgeCapture {
    pub(crate) fn new(row: &Instance) -> Result<Self> {
        let runner = capture_runner(&row.id);
        let protection = CleanupProtection::new([row])?;
        protection.validate()?;
        Ok(Self {
            protection,
            runner,
            session_id: row.id.clone(),
        })
    }

    pub(crate) fn ensure_captured_runner_stopped(&self) -> Result<()> {
        self.runner.ensure_stopped(Some(&self.session_id))
    }
}

impl PurgeOwner {
    #[cfg(test)]
    pub(crate) fn record(storage: &Storage, row: &Instance, capture: PurgeCapture) -> Result<Self> {
        Self::record_plan(storage, row, row.lifecycle_generation, None, None, capture)
    }

    pub(crate) fn record_plan(
        storage: &Storage,
        row: &Instance,
        generation: u64,
        request: Option<&DeletionRequest>,
        additional_protection: Option<&CleanupProtection>,
        mut capture: PurgeCapture,
    ) -> Result<Self> {
        capture.protection.extend([row])?;
        capture.protection.validate()?;
        let file = open_for(storage)?;
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(file.read()?.as_deref())?;
        let mut request = request.cloned();
        if let Some(request) = &mut request {
            request.session_id = row.id.clone();
            request.instance.id = row.id.clone();
            request.instance.source_profile = storage.profile().to_owned();
            request.instance.lifecycle_generation = row.lifecycle_generation;
            request.instance.lifecycle_reservation = row.lifecycle_reservation.clone();
        }
        journal.owners.push(Owner {
            token: uuid::Uuid::new_v4().to_string(),
            session_id: row.id.clone(),
            profile: storage.profile().to_owned(),
            generation,
            protection: capture.protection,
            runner: capture.runner,
            request,
            additional_protection: additional_protection.cloned(),
        });
        save(&file, &journal)?;
        let recorded = journal.owners.pop().expect("recorded owner");
        Ok(Self {
            file,
            token: recorded.token,
            runner: recorded.runner,
            session_id: Some(row.id.clone()),
        })
    }

    pub(super) fn recover(storage: &Storage, token: &str) -> Result<Self> {
        let file = open_for(storage)?;
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let journal = decode(file.read()?.as_deref())?;
        let owner = journal
            .owners
            .into_iter()
            .find(|owner| owner.token == token)
            .context("Pending purge owner disappeared during recovery")?;
        Ok(Self {
            file,
            token: owner.token,
            runner: owner.runner,
            session_id: Some(owner.session_id),
        })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn ensure_captured_runner_stopped(&self) -> Result<()> {
        self.runner.ensure_stopped(self.session_id.as_deref())
    }

    pub(crate) fn release(self) -> Result<()> {
        let (lock, path) = self.file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(self.file.read()?.as_deref())?;
        journal.owners.retain(|owner| owner.token != self.token);
        save(&self.file, &journal)
    }
}

pub(crate) fn protection(
    storage: &Storage,
    except: Option<&str>,
) -> Result<Vec<CleanupProtection>> {
    let journal = decode(open_for(storage)?.read()?.as_deref())?;
    Ok(journal
        .owners
        .into_iter()
        .filter(|owner| Some(owner.token.as_str()) != except)
        .map(|owner| owner.protection)
        .collect())
}

pub(super) fn session_ids(root: &Path) -> Result<impl Iterator<Item = String>> {
    Ok(decode_sibling(open(root)?.read()?.as_deref())?
        .owners
        .into_iter()
        .map(|owner| owner.session_id))
}

pub(super) fn recovery_plans() -> Result<Vec<RecoveryPlan>> {
    let root = crate::session::get_app_dir()?;
    Ok(decode(open(&root)?.read()?.as_deref())?
        .owners
        .into_iter()
        .map(|owner| RecoveryPlan {
            token: owner.token,
            profile: owner.profile,
            request: owner.request,
            additional_protection: owner.additional_protection,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn an_absent_journal_is_no_owners_but_a_corrupt_one_is_still_refused() -> Result<()> {
        // A namespace that predates v036 has no journal at all, and it holds no
        // pending owners. The store reclaim reads this as a sibling, so an
        // absent file must not fail the pass and orphan every store.
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        assert!(session_ids(root.path())?.next().is_none());

        // A journal that exists but does not parse is corruption, not absence:
        // staying fail-closed is the point.
        std::fs::write(root.path().join(FILE_NAME), b"{")?;
        assert!(session_ids(root.path()).is_err());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn frozen_owners_survive_alias_loss_and_reject_incomplete_journals() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        initialize(root.path())?;
        let storage = Storage::new_for_test_path("source", root.path().join("sessions.json"));
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        std::fs::create_dir(&real)?;
        std::os::unix::fs::symlink(&real, &alias)?;
        let mut row = Instance::new("owner", alias.join("checkout").to_str().unwrap());
        row.worktree_info = Some(super::super::WorktreeInfo {
            branch: "work".into(),
            main_repo_path: alias.to_string_lossy().into_owned(),
            managed_by_aoe: false,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let owner = PurgeOwner::record(&storage, &row, PurgeCapture::new(&row)?)?;
        let peer_path = root.path().join("peer");
        let peer_row = Instance::new("peer", peer_path.to_str().unwrap());
        let peer = PurgeOwner::record(&storage, &peer_row, PurgeCapture::new(&peer_row)?)?;
        initialize(root.path())?;
        std::fs::remove_file(&alias)?;
        let frozen = protection(&storage, None)?;
        assert!(frozen
            .iter()
            .any(|entry| entry.references_path(&real.join("checkout"))));
        assert!(frozen
            .iter()
            .any(|entry| entry.references_branch(&real, "work")));
        let path = root.path().join(FILE_NAME);
        let saved = std::fs::read(&path)?;
        let mut invalid_index: serde_json::Value = serde_json::from_slice(&saved)?;
        invalid_index["owners"][0]["protection"]["branches"] = serde_json::json!([[999, "work"]]);
        for corrupt in [b"{".to_vec(), serde_json::to_vec(&invalid_index)?] {
            std::fs::write(&path, corrupt)?;
            assert!(protection(&storage, None).is_err());
            assert!(PurgeOwner::record(&storage, &row, PurgeCapture::new(&row)?).is_err());
        }
        std::fs::remove_file(&path)?;
        assert!(
            protection(&storage, None).is_err(),
            "a missing journal cannot prove absent ownership"
        );
        std::fs::write(&path, saved)?;
        owner.release()?;
        let remaining = protection(&storage, None)?;
        assert!(!remaining.iter().any(|entry| entry.references_path(&real)));
        assert!(remaining
            .iter()
            .any(|entry| entry.references_path(&peer_path)));
        peer.release()?;
        assert!(!protection(&storage, None)?
            .iter()
            .any(|entry| entry.references_path(&peer_path)));
        Ok(())
    }

    /// A process-group check alone cannot distinguish a reaped child from a
    /// replacement that reused its pid. The frozen pid+generation must still
    /// match the strict registry, and ambiguous registry reads retain data.
    #[test]
    fn runner_cleanup_checks_reaped_replacement_and_unreadable_registry() -> Result<()> {
        fn record(pid: u32, generation: u64) -> crate::process::worker_registry::WorkerRecord {
            crate::process::worker_registry::WorkerRecord::new(
                "session".into(),
                pid,
                std::path::PathBuf::from("unused.sock"),
                String::new(),
                String::new(),
                std::path::PathBuf::new(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .with_generation(generation)
        }

        let captured = RunnerCapture::Captured {
            pid: 41,
            generation: 7,
        };
        captured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(41, 7))),
                |_| false,
                |_| Ok(None),
            )
            .expect("an exact, reaped registry identity releases cleanup");
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(std::process::id(), 8))),
                |_| true,
                |_| Ok(None),
            )
            .expect_err("a live replacement must retain purge resources");
        assert!(error.to_string().contains("identity was replaced"));
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Err(anyhow::anyhow!("permission denied")),
                |_| false,
                |_| Ok(None),
            )
            .expect_err("an unreadable registry is ambiguous");
        assert!(error.to_string().contains("unreadable"));
        captured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| true, |_| Ok(None))
            .expect_err("a missing registry cannot prove a live captured process exited");
        captured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(Some(99)))
            .expect_err("an active socket blocks a missing registry");

        RunnerCapture::Unresolved {
            error: "worker record unreadable".into(),
        }
        .ensure_stopped(None)
        .expect_err("unresolved ownership blocks cleanup");
        RunnerCapture::Uncaptured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(None))
            .expect("a strict registry absence and inactive sockets prove no runner");
        RunnerCapture::Uncaptured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(Some(42)))
            .expect_err("an active socket prevents an absent capture from authorizing cleanup");
        RunnerCapture::Uncaptured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(42, 1))),
                |_| false,
                |_| Ok(None),
            )
            .expect_err("a runner published after capture is unresolved");
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn malformed_registry_is_unresolved_at_capture() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let row = Instance::new(
            "malformed-registry",
            root.path().join("checkout").to_str().unwrap(),
        );
        let record = crate::process::worker_registry::record_path(&row.id)?;
        std::fs::write(record, b"{")?;

        let capture = PurgeCapture::new(&row)?;

        assert!(capture.ensure_captured_runner_stopped().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn active_socket_without_registry_is_unresolved() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let row = Instance::new(
            "socket-without-registry",
            root.path().join("checkout").to_str().unwrap(),
        );
        let socket = crate::process::worker_registry::socket_path_for(&row.id)?;
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;

        let capture = PurgeCapture::new(&row)?;

        assert!(capture.ensure_captured_runner_stopped().is_err());
        Ok(())
    }
}
