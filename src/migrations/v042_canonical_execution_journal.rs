use crate::session::anchored_fs::FilePublication;
use crate::session::raw_document::{patch, Emission, RawDocument, RawObject};
use crate::session::{AnchoredDir, DirectoryIdentity, StorageFlock};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const SESSIONS: &str = "sessions.json";

struct Inventory {
    stores: Vec<AnchoredDir>,
    bindings: Vec<(PathBuf, Option<DirectoryIdentity>)>,
    profiles: Option<AnchoredDir>,
    names: Vec<std::ffi::OsString>,
}

impl Inventory {
    fn capture(app: &AnchoredDir) -> Result<Self> {
        let mut result = Self {
            stores: vec![app.relocated(app.path().to_path_buf())?],
            bindings: vec![(app.path().to_path_buf(), Some(app.birth_identity()?))],
            profiles: None,
            names: Vec::new(),
        };
        let mut physical = HashSet::from([app.identity()?]);
        if app.entry_stat(Path::new("profiles"))?.is_none() {
            return Ok(result);
        }
        let profiles = app.child(Path::new("profiles"))?;
        let mut names = profiles.read_dir(Path::new(""), usize::MAX)?;
        names.sort();
        for name in &names {
            let leaf = Path::new(name);
            let Some(stat) = profiles.entry_stat(leaf)? else {
                anyhow::bail!("profile disappeared during migration inventory");
            };
            let kind = stat.st_mode & libc::S_IFMT;
            if kind != libc::S_IFDIR && kind != libc::S_IFLNK {
                continue;
            }
            let directory = match profiles.child_following_alias(leaf) {
                Ok(directory) => directory,
                Err(error)
                    if kind == libc::S_IFLNK
                        && error.downcast_ref::<nix::errno::Errno>()
                            == Some(&nix::errno::Errno::ENOENT) =>
                {
                    result.bindings.push((profiles.path().join(leaf), None));
                    continue;
                }
                Err(error) => return Err(error).context("unreadable original profile inventory"),
            };
            result.bindings.push((
                directory.path().to_path_buf(),
                Some(directory.birth_identity()?),
            ));
            if physical.insert(directory.identity()?) {
                result.stores.push(directory);
            }
        }
        result.names = names;
        result.profiles = Some(profiles);
        result.stores.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(result)
    }

    fn validate(&self, app: &AnchoredDir) -> Result<()> {
        for (path, expected) in &self.bindings {
            let current = match std::fs::metadata(path) {
                Ok(metadata) => {
                    anyhow::ensure!(
                        metadata.is_dir(),
                        "original profile is no longer a directory"
                    );
                    Some(DirectoryIdentity::from_metadata(&metadata))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(
                &current == expected,
                "original migration profile namespace changed"
            );
        }
        match &self.profiles {
            None => anyhow::ensure!(
                app.entry_stat(Path::new("profiles"))?.is_none(),
                "profile inventory changed"
            ),
            Some(profiles) => {
                anyhow::ensure!(
                    app.child(Path::new("profiles"))?.birth_identity()?
                        == profiles.birth_identity()?,
                    "profile inventory directory changed"
                );
                let mut names = profiles.read_dir(Path::new(""), usize::MAX)?;
                names.sort();
                anyhow::ensure!(names == self.names, "profile inventory changed");
            }
        }
        Ok(())
    }
}

struct OriginalLock<'a> {
    directory: &'a AnchoredDir,
    name: &'static str,
    guard: StorageFlock,
}

fn locks<'a>(app: &'a AnchoredDir, inventory: &'a Inventory) -> Result<Vec<OriginalLock<'a>>> {
    use crate::session::OpenStorageLock;
    let mut files = Vec::with_capacity(4 + inventory.stores.len());
    for name in [
        crate::session::SESSION_WORKSPACE_CLAIM_LOCK_FILENAME,
        crate::session::SESSION_IDENTITY_LOCK_FILENAME,
        crate::session::PROFILE_NAMESPACE_LOCK_FILENAME,
        super::v027_isolate_sandbox_stores::LOCK,
    ] {
        files.push((
            app,
            name,
            OpenStorageLock::new(app.open_lock_file(Path::new(name))?, app.path().join(name))?,
        ));
    }
    for directory in &inventory.stores {
        let name = crate::session::STORAGE_LOCK_FILENAME;
        files.push((
            directory,
            name,
            OpenStorageLock::new(
                directory.open_lock_file(Path::new(name))?,
                directory.path().join(name),
            )?,
        ));
    }
    let mut physical = HashSet::with_capacity(files.len());
    for (_, _, file) in &files {
        anyhow::ensure!(
            physical.insert(file.physical_key()),
            "migration lock names alias one physical file"
        );
    }
    files[4..].sort_unstable_by_key(|(_, _, file)| file.physical_key());
    files
        .into_iter()
        .map(|(directory, name, file)| {
            Ok(OriginalLock {
                directory,
                name,
                guard: file.acquire(false)?,
            })
        })
        .collect()
}

fn validate_locks(locks: &[OriginalLock<'_>]) -> Result<()> {
    for lock in locks {
        let original = lock.guard.file_identity()?;
        let current = lock
            .directory
            .open_regular(Path::new(lock.name), usize::MAX)?
            .context("original migration lock disappeared")?;
        anyhow::ensure!(
            DirectoryIdentity::from_metadata(&current.metadata()?) == original,
            "original migration lock changed"
        );
    }
    Ok(())
}

struct Document<'a> {
    directory: &'a AnchoredDir,
    bytes: Vec<u8>,
    rows: RawDocument,
    permissions: std::fs::Permissions,
}

fn documents(inventory: &Inventory) -> Result<Vec<Document<'_>>> {
    let mut physical = HashSet::new();
    let mut documents = Vec::new();
    for directory in &inventory.stores {
        match directory.regular_lookup(Path::new(SESSIONS))? {
            None => continue,
            Some(false) => anyhow::bail!("original sessions document is not a regular file"),
            Some(true) => {}
        }
        let mut file = directory
            .open_regular(Path::new(SESSIONS), usize::MAX)?
            .context("original sessions document disappeared")?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.nlink() == 1 && physical.insert((metadata.dev(), metadata.ino())),
            "sessions documents alias one physical file"
        );
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len()).context("sessions document is too large")?,
        );
        file.read_to_end(&mut bytes)?;
        let rows = RawDocument::parse(std::str::from_utf8(&bytes)?)?;
        documents.push(Document {
            directory,
            bytes,
            rows,
            permissions: metadata.permissions(),
        });
    }
    Ok(documents)
}

fn file_has_bytes(file: &mut impl Read, bytes: &[u8]) -> Result<bool> {
    let mut buffer = [0; 4096];
    for expected in bytes.chunks(buffer.len()) {
        match file.read_exact(&mut buffer[..expected.len()]) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        if &buffer[..expected.len()] != expected {
            return Ok(false);
        }
    }
    Ok(file.read(&mut buffer[..1])? == 0)
}

fn validate_source(document: &Document<'_>) -> Result<()> {
    let mut file = document
        .directory
        .open_regular(Path::new(SESSIONS), usize::MAX)?
        .context("original sessions document disappeared")?;
    anyhow::ensure!(
        file_has_bytes(&mut file, &document.bytes)?,
        "original migration document changed"
    );
    Ok(())
}

fn backup(document: &Document<'_>, validate: &dyn Fn() -> Result<()>) -> Result<()> {
    let prefix = format!("{SESSIONS}{}", crate::session::MIGRATION_BACKUP_MARKER);
    let mut newest = 0_u128;
    for name in document.directory.read_dir(Path::new(""), usize::MAX)? {
        let Some(stamp) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|stamp| stamp.parse::<u128>().ok())
        else {
            continue;
        };
        newest = newest.max(stamp);
        let mut file = document
            .directory
            .open_regular(Path::new(&name), usize::MAX)?
            .context("migration backup is not a readable regular file")?;
        if file_has_bytes(&mut file, &document.bytes)? {
            validate()?;
            file.sync_all()?;
            document.directory.sync()?;
            return Ok(());
        }
    }
    let mut stamp = newest
        .checked_add(1)
        .context("migration backup stamp space exhausted")?;
    loop {
        let name = format!("{prefix}{stamp}");
        if document.directory.publish_file(
            Path::new(&name),
            &mut document.bytes.as_slice(),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
            false,
            Some(FilePublication {
                staging: document.directory,
                validate,
            }),
        )? {
            document.directory.sync()?;
            return Ok(());
        }
        stamp = stamp
            .checked_add(1)
            .context("migration backup stamp space exhausted")?;
    }
}

pub(super) fn run(app: &AnchoredDir, version: u32) -> Result<()> {
    crate::session::retained_intents::validate_at(app)?;
    let original_version = super::schema::read_at(app)?;
    let inventory = Inventory::capture(app)?;
    let locks = locks(app, &inventory)?;
    let validate = || -> Result<()> {
        inventory.validate(app)?;
        validate_locks(&locks)?;
        anyhow::ensure!(
            super::schema::read_at(app)? == original_version,
            "data schema changed during migration"
        );
        Ok(())
    };
    validate()?;
    crate::session::retained_intents::validate_at(app)?;
    let mut documents = documents(&inventory)?;
    let mut owners = HashMap::<String, (usize, bool)>::new();
    for document in &documents {
        for (id, owner) in document.rows.owners("id") {
            let total = owners.entry(id).or_default();
            total.0 += owner.count;
            total.1 |= owner.ambiguous;
        }
    }
    let after = serde_json::json!({"runner_journal":
        crate::session::runner_journal::RunnerExecutionJournal::legacy_unknown()});
    let absent = serde_json::json!({});
    let null = serde_json::json!({"runner_journal": null});
    let mut changed = 0;
    for document in &mut documents {
        let mut modified = false;
        for raw in &mut document.rows.rows {
            let fields = match RawObject::parse(raw) {
                Ok(fields) => fields,
                Err(_) => continue,
            };
            let id = match fields
                .unique("id")
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
            {
                Some(id) => id,
                None => continue,
            };
            if owners.get(&id) != Some(&(1, false)) {
                continue;
            }
            let before = match fields.unique("runner_journal") {
                Ok(None) => &absent,
                Ok(Some(value)) if value.get() == "null" => &null,
                _ => continue,
            };
            if let Emission::Changed(updated) = patch(raw, before, &after)? {
                *raw = updated;
                modified = true;
                changed += 1;
            }
        }
        if modified {
            let check = || {
                validate()?;
                validate_source(document)
            };
            backup(document, &check)?;
            let bytes = serde_json::to_vec(&document.rows.rows)?;
            document.directory.publish_file(
                Path::new(SESSIONS),
                &mut bytes.as_slice(),
                document.permissions.clone(),
                true,
                Some(FilePublication {
                    staging: document.directory,
                    validate: &check,
                }),
            )?;
        }
    }
    crate::session::retained_intents::initialize_legacy_at(app)?;
    validate()?;
    // A visible prior rename on retry is not a completed directory durability barrier.
    for directory in &inventory.stores {
        directory.sync()?;
    }
    super::set_version_at(app, version, Some(&validate))?;
    tracing::info!(target: "migrations", changed, "published canonical execution journals without native authority reconstruction");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    #[serial_test::serial]
    fn raw_upgrade_preserves_opaque_and_ambiguous_owners_and_deduplicates_aliases() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let _environment = crate::session::test_support::isolate_app_dir_at(temp.path());
        let root = crate::session::get_app_dir()?;
        fs::write(root.join(super::super::VERSION_FILE), "35")?;
        let profile = root.join("profiles/real");
        fs::create_dir_all(&profile)?;
        symlink(&profile, root.join("profiles/alias"))?;
        let original = r#"[
 {"id":"unique","opaque":{"same":1,"same":2,"large":1e400}},
 {"id":"null-owner","runner_journal":null,"opaque":1e400},
 {"id":"shared","opaque":{"first":1,"first":2}},
 {"id":"partial","runner_journal":{"launches":[],"future":1e400}},
 {"id":"ambiguous","runner_journal":null,"runner_journal":null},
 {"id":"ambiguous-id","id":"other-id","opaque":1e400}
]"#;
        let peer =
            r#"[{"id":"shared","opaque":1e400},{"id":"physical-profile-owner","opaque":1e400}]"#;
        fs::write(root.join(SESSIONS), original)?;
        fs::write(profile.join(SESSIONS), peer)?;
        let app = AnchoredDir::open(&root)?;
        run(&app, super::super::CURRENT_VERSION)?;
        let before = RawDocument::parse(original)?;
        let after_bytes = fs::read(root.join(SESSIONS))?;
        let after = RawDocument::parse(std::str::from_utf8(&after_bytes)?)?;
        for index in [0, 1] {
            let fields = RawObject::parse(&after.rows[index])?;
            let journal: crate::session::runner_journal::RunnerExecutionJournal =
                serde_json::from_str(
                    fields
                        .unique("runner_journal")?
                        .context("missing migrated journal")?
                        .get(),
                )?;
            assert!(!journal.proves_quiescent());
            assert_eq!(
                fields.unique("opaque")?.unwrap().get(),
                RawObject::parse(&before.rows[index])?
                    .unique("opaque")?
                    .unwrap()
                    .get()
            );
        }
        for index in 2..before.rows.len() {
            assert_eq!(after.rows[index].get(), before.rows[index].get());
        }
        let after_peer = RawDocument::parse(&fs::read_to_string(profile.join(SESSIONS))?)?;
        let before_peer = RawDocument::parse(peer)?;
        assert_eq!(after_peer.rows[0].get(), before_peer.rows[0].get());
        let journal = RawObject::parse(&after_peer.rows[1])?
            .unique("runner_journal")?
            .unwrap();
        assert!(
            !serde_json::from_str::<crate::session::runner_journal::RunnerExecutionJournal>(
                journal.get()
            )?
            .proves_quiescent()
        );
        for (directory, preimage) in [(&root, original), (&profile, peer)] {
            let backups = crate::session::migration_backups(&directory.join(SESSIONS))?;
            assert_eq!(fs::read(&backups[0].1)?, preimage.as_bytes());
        }
        run(&app, super::super::CURRENT_VERSION)?;
        assert_eq!(fs::read(root.join(SESSIONS))?, after_bytes);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn unchanged_retry_requires_every_original_profile_directory_barrier() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let _environment = crate::session::test_support::isolate_app_dir_at(temp.path());
        let root = crate::session::get_app_dir()?;
        fs::write(root.join(super::super::VERSION_FILE), "35")?;
        let profile_path = root.join("profiles/retry");
        fs::create_dir_all(&profile_path)?;
        fs::write(
            profile_path.join(SESSIONS),
            r#"[{"id":"old-owner","opaque":1e400}]"#,
        )?;
        let app = AnchoredDir::open(&root)?;
        run(&app, super::super::CURRENT_VERSION)?;
        let published = fs::read(profile_path.join(SESSIONS))?;
        // The prior row publication is visible, but its schema commit did not complete.
        fs::write(root.join(super::super::VERSION_FILE), "35")?;
        let profile = AnchoredDir::open(&profile_path)?;
        crate::session::anchored_fs::FAIL_SYNC_IDENTITY_ONCE.set(Some(profile.identity()?));
        assert!(run(&app, super::super::CURRENT_VERSION).is_err());
        assert_eq!(fs::read(root.join(super::super::VERSION_FILE))?, b"35");
        assert_eq!(fs::read(profile_path.join(SESSIONS))?, published);
        run(&app, super::super::CURRENT_VERSION)?;
        assert_eq!(
            super::super::schema::read_at(&app)?,
            super::super::CURRENT_VERSION
        );
        assert_eq!(fs::read(profile_path.join(SESSIONS))?, published);
        Ok(())
    }
}
