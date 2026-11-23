use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session::AnchoredDir;

use super::{publication_error, remove_owned_tree_checked, PriorMetadata};

pub(super) const MARKER: &str = ".plugin-publication-active";
const MAX_RECORD: usize = 4096;

#[derive(Debug, thiserror::Error)]
#[error("publication backup already exists")]
pub(super) struct PriorBackup;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    device: u64,
    inode: u64,
    birth: SystemTime,
}

impl Identity {
    pub(super) fn capture(fd: BorrowedFd<'_>) -> Result<Self> {
        let metadata = std::fs::File::from(fd.try_clone_to_owned()?).metadata()?;
        ensure!(
            metadata.is_dir() && metadata.uid() == nix::unistd::geteuid().as_raw(),
            "publication path is not an owned directory"
        );
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            birth: metadata
                .created()
                .context("publication directory birth is unproven")?,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Phase {
    Ready,
    // The operation lock proves the publisher is gone, not its build children.
    Working,
    Committed { metadata: [u8; 32] },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    schema: u32,
    id: String,
    directory: String,
    plugins: Identity,
    container: Identity,
    new_tree: Identity,
    old_tree: Option<Identity>,
    prior_metadata: [u8; 32],
    phase: Phase,
}

fn metadata_digest(metadata: &PriorMetadata) -> Result<[u8; 32]> {
    Ok(Sha256::digest(toml::to_string(metadata)?).into())
}

pub(super) fn exists(app: &Path) -> Result<bool> {
    marker_exists(app, MARKER)
}

fn marker_exists(app: &Path, name: &str) -> Result<bool> {
    match std::fs::symlink_metadata(app.join(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting plugin evidence {name}")),
    }
}

fn read(app: &AnchoredDir) -> Result<Journal> {
    let file = app
        .open_regular(Path::new(MARKER), MAX_RECORD)?
        .context("publication journal is not a bounded regular file")?;
    crate::process::validate_recovery_file(&file)?;
    crate::process::prove_same_mount(app.descriptor(), file.as_fd())?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD && bytes.last() == Some(&b'\n'),
        "incomplete publication journal"
    );
    let record: Journal = serde_json::from_slice(&bytes).context("invalid publication journal")?;
    ensure!(record.schema == 1, "unsupported publication journal schema");
    aoe_plugin_api::PluginId::new(record.id.as_str())?;
    ensure!(
        record
            .directory
            .strip_prefix(".publish-")
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            }),
        "invalid publication directory"
    );
    ensure!(
        record.container != record.new_tree
            && record.old_tree != Some(record.new_tree)
            && record.old_tree != Some(record.container)
            && [
                Some(record.container),
                Some(record.new_tree),
                record.old_tree
            ]
            .into_iter()
            .flatten()
            .all(|identity| identity.device == record.plugins.device && identity.inode != 0),
        "invalid publication tree identities"
    );
    Ok(record)
}

fn directory_at(parent: &AnchoredDir, leaf: &Path) -> Result<Option<Identity>> {
    let Some(stat) = parent.entry_stat(leaf)? else {
        return Ok(None);
    };
    ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFDIR
            && stat.st_uid == nix::unistd::geteuid().as_raw(),
        "publication path {} is not an owned directory",
        parent.path().join(leaf).display()
    );
    #[cfg(target_os = "macos")]
    let device = stat.st_dev as u64;
    #[cfg(not(target_os = "macos"))]
    let device = stat.st_dev;
    let child = parent.child(leaf)?;
    crate::process::prove_same_mount(parent.descriptor(), child.descriptor())?;
    let identity = Identity::capture(child.descriptor())?;
    ensure!(
        (identity.device, identity.inode) == (device, stat.st_ino),
        "publication directory changed before opening"
    );
    Ok(Some(identity))
}

fn verify_rename_destination(
    parent: &AnchoredDir,
    leaf: &Path,
    expected: Option<Identity>,
) -> Result<()> {
    let actual = directory_at(parent, leaf).map_err(publication_error)?;
    if actual.is_none() || actual != expected {
        return Err(publication_error(anyhow::anyhow!(
            "publication rename destination {} has an unexpected identity",
            parent.path().join(leaf).display()
        )));
    }
    Ok(())
}

fn app_for(final_dir: &Path) -> Result<AnchoredDir> {
    let app = final_dir
        .parent()
        .and_then(Path::parent)
        .context("missing publication app directory")?;
    AnchoredDir::open(app)
}

impl Journal {
    pub(super) fn prepare(
        final_dir: &Path,
        tree: &Path,
        prior: &PriorMetadata,
        update: bool,
    ) -> Result<Self> {
        let app = app_for(final_dir)?;
        let marker = Path::new(MARKER);
        if app.entry_stat(marker).map_err(publication_error)?.is_some() {
            return Err(publication_error(anyhow::anyhow!(
                "publication evidence already exists"
            )));
        }
        let plugins = app.child(Path::new("plugins"))?;
        crate::process::prove_same_mount(app.descriptor(), plugins.descriptor())?;
        let container = tree.parent().context("missing publication container")?;
        let directory = container
            .file_name()
            .and_then(OsStr::to_str)
            .context("invalid publication container name")?;
        let id = final_dir
            .file_name()
            .and_then(OsStr::to_str)
            .context("invalid publication target")?;
        let anchored = plugins.child(Path::new(directory))?;
        crate::process::prove_same_mount(plugins.descriptor(), anchored.descriptor())?;
        if plugins
            .entry_stat(Path::new(&format!("{id}.bak")))?
            .is_some()
        {
            return Err(PriorBackup.into());
        }
        let record = Self {
            schema: 1,
            id: id.to_owned(),
            directory: directory.to_owned(),
            plugins: Identity::capture(plugins.descriptor())?,
            container: Identity::capture(anchored.descriptor())?,
            new_tree: directory_at(&anchored, Path::new("tree"))?
                .context("missing publication tree")?,
            old_tree: directory_at(&plugins, Path::new(id))?,
            prior_metadata: metadata_digest(prior)?,
            phase: Phase::Ready,
        };
        ensure!(
            update || record.old_tree.is_none(),
            "plugin destination already exists"
        );
        let bytes = record.encode()?;
        anchored.sync()?;
        plugins.sync()?;
        // From exclusive creation onward, an error cannot prove that no journal exists.
        (|| -> Result<()> {
            let mut file = app
                .create_new_regular(marker)?
                .context("publication evidence already exists")?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            app.sync()
        })()
        .map_err(publication_error)?;
        Ok(record)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_RECORD,
            "publication journal exceeds its bound"
        );
        Ok(bytes)
    }

    fn backup(&self) -> PathBuf {
        PathBuf::from(format!("{}.bak", self.id))
    }

    fn plugins(&self, app: &AnchoredDir) -> Result<AnchoredDir> {
        let plugins = app.child(Path::new("plugins"))?;
        crate::process::prove_same_mount(app.descriptor(), plugins.descriptor())?;
        ensure!(
            Identity::capture(plugins.descriptor())? == self.plugins,
            "publication plugins directory changed"
        );
        Ok(plugins)
    }

    fn advance(&mut self, app: &AnchoredDir, phase: Phase) -> Result<()> {
        ensure!(read(app)? == *self, "publication journal changed");
        self.phase = phase;
        app.bind_file(OsStr::new(MARKER))?
            .replace(&self.encode()?)?;
        app.sync()
    }

    pub(super) fn working(&mut self, final_dir: &Path) -> Result<()> {
        if self.phase == Phase::Ready {
            let app = app_for(final_dir)?;
            self.advance(&app, Phase::Working)?;
        }
        Ok(())
    }

    pub(super) fn remove_installed(&self, final_dir: &Path) -> Result<()> {
        let plugins = self.plugins(&app_for(final_dir)?)?;
        remove_owned_tree_checked(&plugins, Path::new(&self.id), self.new_tree)
    }

    pub(super) fn remove_backup(&self, backup_dir: &Path) -> Result<()> {
        let old = self.old_tree.context("missing journaled backup identity")?;
        let plugins = self.plugins(&app_for(backup_dir)?)?;
        remove_owned_tree_checked(&plugins, &self.backup(), old)
    }

    pub(super) fn restore_backup(&self, final_dir: &Path) -> Result<()> {
        self.restore(&self.plugins(&app_for(final_dir)?)?)
    }

    fn restore(&self, plugins: &AnchoredDir) -> Result<()> {
        let backup = self.backup();
        let target = Path::new(&self.id);
        ensure!(
            self.old_tree.is_some() && directory_at(plugins, &backup)? == self.old_tree,
            "publication backup changed before restoration"
        );
        plugins
            .rename_child_exclusive(&backup, plugins, target)
            .context("restoring the owned plugin backup")?;
        verify_rename_destination(plugins, target, self.old_tree)
    }

    pub(super) fn has_prior_tree(&self) -> bool {
        self.old_tree.is_some()
    }

    pub(super) fn displace(&self, final_dir: &Path) -> Result<()> {
        let plugins = self.plugins(&app_for(final_dir)?)?;
        let target = Path::new(&self.id);
        let backup = self.backup();
        ensure!(
            self.old_tree.is_some() && directory_at(&plugins, target)? == self.old_tree,
            "installed tree changed before backup"
        );
        plugins.rename_child_exclusive(target, &plugins, &backup)?;
        verify_rename_destination(&plugins, &backup, self.old_tree)
    }

    pub(super) fn place(&self, final_dir: &Path) -> Result<()> {
        let plugins = self.plugins(&app_for(final_dir)?)?;
        let container = self
            .open_container(&plugins)?
            .context("publication container disappeared")?;
        let tree = Path::new("tree");
        ensure!(
            directory_at(&container, tree)? == Some(self.new_tree),
            "staged publication tree changed"
        );
        let target = Path::new(&self.id);
        container.rename_child_exclusive(tree, &plugins, target)?;
        verify_rename_destination(&plugins, target, Some(self.new_tree))
    }

    fn open_container(&self, plugins: &AnchoredDir) -> Result<Option<AnchoredDir>> {
        let name = Path::new(&self.directory);
        let Some(identity) = directory_at(plugins, name)? else {
            return Ok(None);
        };
        let container = plugins.child(name)?;
        crate::process::prove_same_mount(plugins.descriptor(), container.descriptor())?;
        ensure!(
            identity == self.container && Identity::capture(container.descriptor())? == identity,
            "publication container changed"
        );
        Ok(Some(container))
    }

    fn staged(&self, container: &AnchoredDir) -> Result<Option<Identity>> {
        ensure!(
            container
                .read_dir_complete(Path::new(""), 2)?
                .iter()
                .all(|entry| entry == "tree"),
            "unexpected publication container contents"
        );
        let tree = directory_at(container, Path::new("tree"))?;
        ensure!(
            tree.is_none() || tree == Some(self.new_tree),
            "staged publication tree changed"
        );
        Ok(tree)
    }

    pub(super) fn clean_container(&self, final_dir: &Path) -> Result<()> {
        self.clean_container_at(&self.plugins(&app_for(final_dir)?)?)
    }

    fn clean_container_at(&self, plugins: &AnchoredDir) -> Result<()> {
        let Some(container) = self.open_container(plugins)? else {
            return Ok(());
        };
        if let Some(tree) = self.staged(&container)? {
            remove_owned_tree_checked(&container, Path::new("tree"), tree)?;
        }
        let name = Path::new(&self.directory);
        ensure!(
            directory_at(plugins, name)? == Some(self.container),
            "publication container changed before removal"
        );
        plugins.remove_empty_dir(name)
    }

    pub(super) fn committed(&mut self, final_dir: &Path) -> Result<()> {
        let app = app_for(final_dir)?;
        let plugins = self.plugins(&app)?;
        ensure!(
            directory_at(&plugins, Path::new(&self.id))? == Some(self.new_tree),
            "installed publication tree changed"
        );
        ensure!(
            directory_at(&plugins, &self.backup())? == self.old_tree,
            "publication backup changed"
        );
        let container = self
            .open_container(&plugins)?
            .context("publication container disappeared")?;
        ensure!(
            self.staged(&container)?.is_none(),
            "publication tree appears twice"
        );
        container.sync()?;
        plugins.sync()?;
        let metadata = metadata_digest(&PriorMetadata::capture(&self.id)?)?;
        self.advance(&app, Phase::Committed { metadata })
    }

    pub(super) fn finish(&self, final_dir: &Path, committed: bool) -> Result<()> {
        let app = app_for(final_dir)?;
        let plugins = self.plugins(&app)?;
        let (expected, metadata) = if committed {
            let Phase::Committed { metadata } = self.phase else {
                anyhow::bail!("missing durable publication commit");
            };
            (Some(self.new_tree), metadata)
        } else {
            ensure!(
                !matches!(self.phase, Phase::Committed { .. }),
                "publication already committed"
            );
            (self.old_tree, self.prior_metadata)
        };
        ensure!(
            directory_at(&plugins, Path::new(&self.id))? == expected
                && plugins.entry_stat(&self.backup())?.is_none()
                && plugins.entry_stat(Path::new(&self.directory))?.is_none()
                && metadata_digest(&PriorMetadata::capture(&self.id)?)? == metadata,
            "publication settlement is unproven"
        );
        self.retire(&app, &plugins)
    }

    fn retire(&self, app: &AnchoredDir, plugins: &AnchoredDir) -> Result<()> {
        plugins.sync()?;
        ensure!(
            read(app)? == *self,
            "publication journal changed before retirement"
        );
        app.remove_file(Path::new(MARKER))?;
        app.sync()
    }
}

pub(super) fn recover(app_path: &Path) -> Result<()> {
    if !exists(app_path)? {
        return Ok(());
    }
    for marker in [super::RECOVERY_MARKER, super::STAGING_MARKER] {
        ensure!(
            !marker_exists(app_path, marker)?,
            "publication recovery conflicts with {marker}"
        );
    }
    let app = AnchoredDir::open(app_path)?;
    let record = read(&app)?;
    let expected_metadata = match record.phase {
        Phase::Ready => record.prior_metadata,
        Phase::Committed { metadata } => metadata,
        Phase::Working => anyhow::bail!("interrupted plugin publication may have live build writers or partial metadata; manual recovery required"),
    };
    ensure!(
        metadata_digest(&PriorMetadata::capture(&record.id)?)? == expected_metadata,
        "publication metadata differs from the journal"
    );
    let plugins = record.plugins(&app)?;
    let final_name = Path::new(&record.id);
    let backup_name = record.backup();
    let installed = directory_at(&plugins, final_name)?;
    let backup = directory_at(&plugins, &backup_name)?;
    let staged = match record.open_container(&plugins)? {
        Some(container) => record.staged(&container)?,
        None => None,
    };
    ensure!(
        backup.is_none() || backup == record.old_tree,
        "publication backup changed"
    );
    ensure!(
        read(&app)? == record,
        "publication journal changed before recovery"
    );
    match record.phase {
        Phase::Ready => {
            ensure!(
                (installed == record.old_tree && backup.is_none())
                    || (installed.is_none() || installed == Some(record.new_tree))
                        && backup == record.old_tree,
                "installation and backup do not match publication journal"
            );
            ensure!(
                installed != Some(record.new_tree) || staged.is_none(),
                "publication tree appears twice"
            );
            if installed == Some(record.new_tree) {
                remove_owned_tree_checked(&plugins, final_name, record.new_tree)?;
            }
            if backup.is_some() {
                record.restore(&plugins)?;
            }
        }
        Phase::Committed { .. } => {
            ensure!(
                installed == Some(record.new_tree) && staged.is_none(),
                "committed publication tree changed"
            );
            if let Some(backup) = backup {
                remove_owned_tree_checked(&plugins, &backup_name, backup)?;
            }
        }
        Phase::Working => unreachable!(),
    }
    record.clean_container_at(&plugins)?;
    record.retire(&app, &plugins)
}
