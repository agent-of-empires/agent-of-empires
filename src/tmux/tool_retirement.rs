//! Durable original tool cohorts. Runtime never initializes missing ownership evidence.

use super::tool_session::ToolStopReceipt;
use crate::session::{AnchoredDir, LockedDataFile};
use anyhow::{Context, Result};

pub(crate) const FILE_NAME: &str = "tool-retirements.json";

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    pub token: uuid::Uuid,
    pub instance_id: String,
    pub tool_name: String,
    pub socket: String,
    pub server_pid: u32,
    pub server_birth: crate::process::ProcessBirthIdentity,
    pub receipt: ToolStopReceipt,
}

#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    owners: Vec<Retirement>,
}

fn decode(content: Option<&str>) -> Result<Journal> {
    let journal: Journal = serde_json::from_str(content.context("Tool retirement journal is missing; restore its ownership evidence or complete pending data migrations")?)?;
    validate(&journal)?;
    Ok(journal)
}

fn validate(journal: &Journal) -> Result<()> {
    anyhow::ensure!(
        journal.version == 1,
        "Unsupported tool retirement journal version"
    );
    for (index, owner) in journal.owners.iter().enumerate() {
        anyhow::ensure!(
            owner.token.get_version_num() == 4
                && !owner.instance_id.is_empty()
                && !owner.tool_name.is_empty()
                && std::path::Path::new(&owner.socket).is_absolute()
                && owner.server_pid > 0
                && i32::try_from(owner.server_pid).is_ok()
                && !owner.server_birth.boot_id.is_empty()
                && !owner.server_birth.start_id.is_empty()
                && owner.receipt.tree.boot_identity() == owner.server_birth.boot_id
                && owner.receipt.owner
                    == serde_json::to_string(&(&owner.instance_id, &owner.tool_name))?
                && !owner.receipt.name.is_empty()
                && super::tool_session::ToolSession::valid_identity(&owner.receipt.identity)
                && !journal.owners[..index].iter().any(|known| {
                    known.token == owner.token
                        || (known.instance_id == owner.instance_id
                            && known.tool_name == owner.tool_name)
                }),
            "Invalid or duplicate tool retirement ownership"
        );
        owner.receipt.tree.validate(
            owner
                .receipt
                .identity
                .pane_pid
                .context("Retirement lacks its original live process identity")?,
        )?;
    }
    Ok(())
}

pub(super) struct LockedJournal {
    file: LockedDataFile,
    journal: Journal,
}

impl LockedJournal {
    pub fn open() -> Result<Self> {
        Self::open_at(&crate::session::get_app_dir()?)
    }

    fn open_at(root: &std::path::Path) -> Result<Self> {
        let target = AnchoredDir::open(root)?.bind_file(FILE_NAME.as_ref())?;
        let file = LockedDataFile::lock(target)?;
        let journal = decode(file.read()?.as_deref())?;
        Ok(Self { file, journal })
    }

    pub fn pending(&self, id: &str, tool: &str) -> Option<&Retirement> {
        self.journal
            .owners
            .iter()
            .find(|owner| owner.instance_id == id && owner.tool_name == tool)
    }

    pub fn pending_by_token(&self, token: uuid::Uuid) -> Option<&Retirement> {
        self.journal
            .owners
            .iter()
            .find(|owner| owner.token == token)
    }

    pub fn retirements_for_instance(&self, id: &str) -> Vec<(uuid::Uuid, String)> {
        self.journal
            .owners
            .iter()
            .filter(|owner| owner.instance_id == id)
            .map(|owner| (owner.token, owner.tool_name.clone()))
            .collect()
    }

    pub fn targets(&self) -> Vec<(String, String)> {
        self.journal
            .owners
            .iter()
            .map(|owner| (owner.instance_id.clone(), owner.tool_name.clone()))
            .collect()
    }

    pub fn publish(&mut self, owner: Retirement) -> Result<()> {
        anyhow::ensure!(
            self.pending(&owner.instance_id, &owner.tool_name).is_none(),
            "Another tool incarnation has outstanding retirement debt"
        );
        self.journal.owners.push(owner);
        validate(&self.journal)?;
        self.save()
    }

    pub fn sync(&self) -> Result<()> {
        let current = decode(self.file.read()?.as_deref())?;
        anyhow::ensure!(
            current == self.journal,
            "Tool retirement journal changed before effects"
        );
        self.save()
    }

    fn save(&self) -> Result<()> {
        self.file.require_writable_existing()?;
        self.file
            .replace_and_sync(&serde_json::to_vec(&self.journal)?)
    }

    pub fn acknowledge(&mut self, token: uuid::Uuid) -> Result<()> {
        // Re-read under the same sidecar lock. Never clear a replacement operation,
        // even if an external writer changed this slot without respecting the lock.
        let owner = self
            .pending_by_token(token)
            .context("Selected tool retirement is no longer pending")?;
        let current = decode(self.file.read()?.as_deref())?;
        let index = current
            .owners
            .iter()
            .position(|known| known.token == token)
            .context("Tool retirement disappeared before exact acknowledgement")?;
        anyhow::ensure!(
            &current.owners[index] == owner,
            "Tool retirement changed before exact acknowledgement"
        );
        self.journal = current;
        self.journal.owners.remove(index);
        self.save().context(
            "Syncing acknowledged tool retirement journal after original cohort settlement",
        )
    }
}

pub(crate) fn restore_from_complete_backup(
    source: &std::path::Path,
    _transaction: &crate::daemon::lifecycle::Transaction,
    _serve: &crate::cli::serve::ServeLock,
) -> Result<()> {
    let parent = source.parent().context("Backup needs a parent directory")?;
    let parent = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    let source = AnchoredDir::open(parent)?
        .bind_file(source.file_name().context("Backup needs a file name")?)?;
    let original = source
        .read_recovery_bytes()?
        .context("Tool retirement backup is missing")?;
    let journal = decode(Some(std::str::from_utf8(&original)?))?;
    let encoded = serde_json::to_vec(&journal)?;
    let app = crate::session::get_app_dir()?;
    let _identity = crate::session::acquire_recovery_identity_lock()?;
    let _profiles = crate::migrations::recovery::profile_locks()?;
    let target = AnchoredDir::open(&app)?.bind_file(FILE_NAME.as_ref())?;
    let file = LockedDataFile::lock_recovery(target)?;
    if let Some(bytes) = file.read_recovery_bytes()? {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                anyhow::ensure!(
                    value.get("version").and_then(serde_json::Value::as_u64) == Some(1),
                    "Unsupported destination tool retirement journal version; refusing replacement"
                );
            }
            anyhow::ensure!(
                decode(Some(text)).is_err(),
                "Destination tool retirement journal is healthy; refusing replacement"
            );
        }
        file.require_writable_existing()?;
        let quarantine = file.quarantine_recovery_bytes(&bytes)?;
        tracing::warn!(path = %quarantine.display(), "Preserved corrupt tool retirement evidence");
    }
    file.replace_and_sync(&encoded)
}
/// Migration-only initialization. Existing malformed bytes are never replaced.
pub(crate) fn initialize(root: &std::path::Path) -> Result<()> {
    let target = AnchoredDir::open(root)?.bind_file(FILE_NAME.as_ref())?;
    let file = LockedDataFile::lock(target)?;
    match file.read()? {
        Some(content) => {
            decode(Some(&content))?;
        }
        None => file.replace_and_sync(&serde_json::to_vec(&Journal {
            version: 1,
            owners: Vec::new(),
        })?)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_missing_blank_corrupt_and_symlink_never_mean_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(LockedJournal::open_at(root.path()).is_err());
        initialize(root.path()).unwrap();
        assert!(LockedJournal::open_at(root.path())
            .unwrap()
            .pending("id", "tool")
            .is_none());
        for bytes in ["", "{", r#"{"version":2,"owners":[]}"#] {
            std::fs::write(root.path().join(FILE_NAME), bytes).unwrap();
            assert!(LockedJournal::open_at(root.path()).is_err());
            assert!(initialize(root.path()).is_err());
            assert_eq!(
                std::fs::read_to_string(root.path().join(FILE_NAME)).unwrap(),
                bytes
            );
        }
        std::fs::remove_file(root.path().join(FILE_NAME)).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, r#"{"version":1,"owners":[]}"#).unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join(FILE_NAME)).unwrap();
        assert!(LockedJournal::open_at(root.path()).is_err());
        assert!(initialize(root.path()).is_err());
    }

    #[test]
    fn read_only_evidence_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        initialize(root.path()).unwrap();
        let path = root.path().join(FILE_NAME);
        let evidence = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let journal = LockedJournal::open_at(root.path()).unwrap();
        assert!(journal.sync().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), evidence);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn owner() -> Retirement {
        let pid = std::process::id();
        Retirement {
            token: uuid::Uuid::new_v4(),
            instance_id: "full-instance-id".into(),
            tool_name: "raw/tool-name".into(),
            socket: "/original/tmux.sock".into(),
            server_pid: pid,
            server_birth: crate::process::process_birth_identity(pid)
                .unwrap()
                .unwrap(),
            receipt: ToolStopReceipt {
                name: "aoe_tool_original".into(),
                identity: crate::session::LegacyToolIdentity {
                    session_id: "$1".into(),
                    pane_id: "%1".into(),
                    pane_pid: Some(pid),
                },
                owner: serde_json::to_string(&("full-instance-id", "raw/tool-name")).unwrap(),
                tree: crate::process::CapturedProcessTree::capture(pid)
                    .unwrap()
                    .receipt()
                    .clone(),
            },
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publication_retains_debt_and_ack_sync_failure_reports_proven_clear() {
        let root = tempfile::tempdir().unwrap();
        initialize(root.path()).unwrap();
        let owner = owner();
        let mut journal = LockedJournal::open_at(root.path()).unwrap();
        crate::session::anchored_fs::FAIL_SYNC_ONCE
            .with(|path| *path.borrow_mut() = Some(root.path().to_owned()));
        assert!(journal.publish(owner.clone()).is_err());
        drop(journal);
        let mut journal = LockedJournal::open_at(root.path()).unwrap();
        assert_eq!(
            journal
                .pending(&owner.instance_id, &owner.tool_name)
                .unwrap()
                .token,
            owner.token
        );
        crate::session::anchored_fs::FAIL_SYNC_ONCE
            .with(|path| *path.borrow_mut() = Some(root.path().to_owned()));
        assert!(journal.acknowledge(owner.token).is_err());
        assert!(decode(Some(
            &std::fs::read_to_string(root.path().join(FILE_NAME)).unwrap()
        ))
        .unwrap()
        .owners
        .is_empty());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn different_incarnation_cannot_publish_or_acknowledge_original_debt() {
        let root = tempfile::tempdir().unwrap();
        initialize(root.path()).unwrap();
        let original = owner();
        let mut journal = LockedJournal::open_at(root.path()).unwrap();
        journal.publish(original.clone()).unwrap();
        let evidence = std::fs::read(root.path().join(FILE_NAME)).unwrap();
        let mut replacement = original.clone();
        replacement.token = uuid::Uuid::new_v4();
        assert!(journal.publish(replacement.clone()).is_err());
        assert!(journal.acknowledge(replacement.token).is_err());
        replacement.token = original.token;
        replacement.receipt.identity.pane_id = "%999".into();
        let changed = serde_json::to_vec(&Journal {
            version: 1,
            owners: vec![replacement],
        })
        .unwrap();
        journal.file.replace_and_sync(&changed).unwrap();
        assert!(journal.acknowledge(original.token).is_err());
        assert_eq!(std::fs::read(root.path().join(FILE_NAME)).unwrap(), changed);
        journal.file.replace_and_sync(&evidence).unwrap();
        journal.acknowledge(original.token).unwrap();
        assert!(journal
            .pending(&original.instance_id, &original.tool_name)
            .is_none());
    }
    #[test]
    #[serial_test::serial]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn complete_restore_retains_original_births_and_never_overwrites_healthy_authority(
    ) -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        let original = owner();
        let backup = home.path().join("complete-tool-backup.json");
        let bytes = serde_json::to_vec(&Journal {
            version: 1,
            owners: vec![original.clone()],
        })?;
        std::fs::write(&backup, &bytes)?;
        let target = app.join(FILE_NAME);
        let corrupt = [0xff, b'{'];
        std::fs::write(&target, corrupt)?;
        let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
            std::time::Duration::from_secs(1),
        )?;
        let serve = crate::cli::serve::try_lock_serve()?.unwrap();
        crate::cli::serve::with_inactive_serve_state(&transaction, Some(&serve), |serve| {
            restore_from_complete_backup(&backup, &transaction, serve)
        })?;
        let restored = LockedJournal::open_at(&app)?;
        assert!(restored.pending_by_token(original.token) == Some(&original));
        drop(restored);
        let committed = std::fs::read(&target)?;
        assert!(crate::cli::serve::with_inactive_serve_state(
            &transaction,
            Some(&serve),
            |serve| { restore_from_complete_backup(&backup, &transaction, serve) }
        )
        .is_err());
        assert_eq!(std::fs::read(&target)?, committed);
        let quarantines: Vec<_> = std::fs::read_dir(&app)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{FILE_NAME}.quarantine-"))
            })
            .collect();
        assert_eq!(quarantines.len(), 1);
        assert_eq!(std::fs::read(quarantines[0].path())?, corrupt);
        Ok(())
    }
}
