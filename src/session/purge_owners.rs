//! Frozen resource ownership retained across irreversible purge failures.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::anchored_fs::ResolvedDataFile;
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
        )
    }

    fn ensure_stopped_with(
        &self,
        session_id: Option<&str>,
        load: impl FnOnce(&str) -> Result<Option<crate::process::worker_registry::WorkerRecord>>,
        process_group_alive: impl Fn(u32) -> bool,
    ) -> Result<()> {
        let Self::Captured {
            pid, generation, ..
        } = self
        else {
            return match self {
                Self::Uncaptured => Ok(()),
                Self::Unresolved { error } => {
                    anyhow::bail!("Runner ownership unresolved; purge resources retained: {error}")
                }
                Self::Captured { .. } => unreachable!(),
            };
        };
        let Some(session_id) = session_id else {
            anyhow::bail!("Captured runner session identity is missing; purge resources retained");
        };
        let record = load(session_id)
            .map_err(|error| anyhow::anyhow!("worker registry is unreadable: {error:#}"))?;
        let alive = process_group_alive(*pid);
        match record {
            None => anyhow::ensure!(
                !alive,
                "Captured runner {pid} has not exited; purge resources retained"
            ),
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
                    !alive && !crate::process::worker_registry::is_record_live(&record),
                    "Captured runner {pid} has not exited; purge resources retained"
                );
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Owner {
    token: String,
    session_id: String,
    profile: String,
    generation: u64,
    protection: CleanupProtection,
    runner: RunnerCapture,
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
    }
    Ok(journal)
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
        let runner = match crate::process::worker_registry::load(&row.id) {
            Ok(Some(record)) if record.pid > 0 && i32::try_from(record.pid).is_ok() => {
                RunnerCapture::Captured {
                    pid: record.pid,
                    generation: record.generation,
                }
            }
            Ok(Some(_)) => RunnerCapture::Unresolved {
                error: "Invalid captured runner pid".into(),
            },
            Ok(None) => RunnerCapture::Uncaptured,
            Err(error) => RunnerCapture::Unresolved {
                error: format!("{error:#}"),
            },
        };
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
    pub(crate) fn record(
        storage: &Storage,
        row: &Instance,
        mut capture: PurgeCapture,
    ) -> Result<Self> {
        capture.protection.extend([row])?;
        capture.protection.validate()?;
        let file = open_for(storage)?;
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(file.read()?.as_deref())?;
        journal.owners.push(Owner {
            token: uuid::Uuid::new_v4().to_string(),
            session_id: row.id.clone(),
            profile: storage.profile().to_owned(),
            generation: row.lifecycle_generation,
            protection: capture.protection,
            runner: capture.runner,
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
    Ok(decode(open(root)?.read()?.as_deref())?
        .owners
        .into_iter()
        .map(|owner| owner.session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .ensure_stopped_with(Some("session"), |_| Ok(Some(record(41, 7))), |_| false)
            .expect("an exact, reaped registry identity releases cleanup");
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(std::process::id(), 8))),
                |_| true,
            )
            .expect_err("a live replacement must retain purge resources");
        assert!(error.to_string().contains("identity was replaced"));
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Err(anyhow::anyhow!("permission denied")),
                |_| false,
            )
            .expect_err("an unreadable registry is ambiguous");
        assert!(error.to_string().contains("unreadable"));
        captured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| true)
            .expect_err("a missing registry cannot prove a live captured process exited");

        RunnerCapture::Unresolved {
            error: "worker record unreadable".into(),
        }
        .ensure_stopped(None)
        .expect_err("unresolved ownership blocks cleanup");
        RunnerCapture::Uncaptured
            .ensure_stopped(None)
            .expect("nothing was captured, so nothing blocks cleanup");
        Ok(())
    }
}
