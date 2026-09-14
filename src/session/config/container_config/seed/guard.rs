//! Per-publication source stability and native-state hardlink checks.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{canonical_expected_path, NativeStateBoundary};
use crate::session::anchored_fs::AnchoredDir;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    size: u64,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<&Metadata> for Fingerprint {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            links: metadata.nlink(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

pub(super) struct SourceRoot {
    anchor: AnchoredDir,
    lookup: PathBuf,
    fingerprint: Fingerprint,
}

impl SourceRoot {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let anchor = super::open_canonical_dir(&fs::canonicalize(path)?)?;
        let metadata = fs::metadata(anchor.path())?;
        let (device, inode) = anchor.identity()?;
        #[cfg(target_os = "macos")]
        let device = device as u64;
        if metadata.dev() != device || metadata.ino() != inode {
            bail!("native source root changed before capture");
        }
        Ok(Self {
            anchor,
            lookup: path.to_path_buf(),
            fingerprint: Fingerprint::from(&metadata),
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.anchor.path()
    }

    fn validate(&self) -> Result<()> {
        if fs::canonicalize(&self.lookup)? != self.anchor.path()
            || Fingerprint::from(&fs::metadata(&self.lookup)?) != self.fingerprint
        {
            bail!("native configuration source root changed during seeding");
        }
        Ok(())
    }
}

pub(super) struct PrivateStage {
    path: PathBuf,
    pub(super) anchor: AnchoredDir,
}

impl PrivateStage {
    pub(super) fn new(destination: &Path) -> Result<Self> {
        let parent = destination
            .parent()
            .context("native destination has no parent")?;
        let temporary = tempfile::Builder::new()
            .prefix(".aoe-config-stage-")
            .tempdir_in(parent)?;
        let anchor = AnchoredDir::open(temporary.path())?;
        Ok(Self {
            path: temporary.keep(),
            anchor,
        })
    }
}

impl Drop for PrivateStage {
    fn drop(&mut self) {
        // Traverse only the directory we created, never a replacement at its name.
        if let Err(error) = self.anchor.remove_contents() {
            tracing::warn!(target: "session.profile", %error, path = %self.path.display(), "Cannot remove private config stage");
        }
        let _ = fs::remove_dir(&self.path);
    }
}

pub(super) struct ReadGuard<'a> {
    pub(super) boundary: &'a NativeStateBoundary,
    aliases: Vec<(PathBuf, bool)>,
    routes: Vec<(PathBuf, PathBuf)>,
    directories: BTreeMap<PathBuf, Fingerprint>,
    files: BTreeMap<PathBuf, Fingerprint>,
    state_inodes: Option<HashSet<(u64, u64)>>,
    entries: BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
}

impl<'a> ReadGuard<'a> {
    pub(super) fn new(boundary: &'a NativeStateBoundary) -> Result<Self> {
        boundary.source_root.validate()?;
        let mut guard = Self {
            boundary,
            aliases: Vec::new(),
            routes: Vec::new(),
            directories: BTreeMap::new(),
            files: BTreeMap::new(),
            state_inodes: None,
            entries: BTreeMap::new(),
        };
        for (path, storage) in &boundary.paths {
            guard.add_alias(path, *storage)?;
        }
        for (root, pattern) in &boundary.patterns {
            // Seal every actual directory prefix used by glob expansion. A new
            // intermediate directory or symlink must invalidate this snapshot.
            for ancestor in Path::new(pattern.as_str()).ancestors().skip(1) {
                let prefix = root.join(ancestor);
                guard.watch_entry(&prefix)?;
                for entry in super::state_glob(
                    root,
                    ancestor
                        .to_str()
                        .context("native state pattern is not UTF-8")?,
                )? {
                    let path = entry.context("inspecting native-state pattern parent")?;
                    if path.is_dir() {
                        guard.seal_directory(&path)?;
                    }
                }
            }
            for entry in super::state_glob(root, pattern.as_str())? {
                guard.add_alias(&entry.context("inspecting native-state pattern")?, false)?;
            }
        }
        Ok(guard)
    }

    fn add_alias(&mut self, path: &Path, storage: bool) -> Result<()> {
        self.watch_entry(path)?;
        let canonical = canonical_expected_path(path)
            .with_context(|| format!("resolving native-state boundary {}", path.display()))?;
        self.routes.push((path.to_path_buf(), canonical.clone()));
        self.aliases.push((canonical, storage));
        Ok(())
    }

    fn watch_entry(&mut self, path: &Path) -> Result<()> {
        // Watch the relevant entry or first missing component, not the mtime of
        // an unrelated ancestor such as /tmp or HOME.
        let mut cursor = Some(path);
        let mut missing = None;
        while let Some(candidate) = cursor {
            match fs::symlink_metadata(candidate) {
                Ok(metadata) => {
                    self.entries.entry(candidate.to_path_buf()).or_insert(Some((
                        metadata.dev(),
                        metadata.ino(),
                        metadata.mode(),
                    )));
                    if let Some(missing) = missing {
                        self.entries.entry(missing).or_insert(None);
                    }
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    missing = Some(candidate.to_path_buf());
                    cursor = candidate.parent();
                }
                Err(error) => return Err(error).context("inspecting native-state namespace entry"),
            }
        }
        bail!("native-state boundary has no existing ancestor")
    }

    fn seal_directory(&mut self, path: &Path) -> Result<()> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_dir() {
            bail!("native-state directory changed type");
        }
        self.directories
            .entry(path.to_path_buf())
            .or_insert_with(|| Fingerprint::from(&metadata));
        Ok(())
    }

    pub(super) fn record_directory(&mut self, directory: &AnchoredDir) -> Result<()> {
        let metadata = fs::metadata(directory.path())?;
        let (device, inode) = directory.identity()?;
        #[cfg(target_os = "macos")]
        let device = device as u64;
        if metadata.dev() != device || metadata.ino() != inode {
            bail!("configuration source directory changed before reading");
        }
        self.seal_directory(directory.path())
    }

    pub(super) fn record_file(&mut self, path: &Path, file: &File) -> Result<bool> {
        let metadata = file.metadata()?;
        if self
            .aliases
            .iter()
            .any(|(state, storage)| self.boundary.rejects_path(path, state, false, *storage))
        {
            return Ok(false);
        }
        let fingerprint = Fingerprint::from(&metadata);
        if metadata.nlink() > 1 {
            if self.state_inodes.is_none() {
                let mut inodes = HashSet::new();
                let mut visited = HashSet::new();
                for (path, storage) in &self.aliases {
                    Self::inventory(
                        self.boundary,
                        &mut self.directories,
                        path,
                        *storage,
                        &mut visited,
                        &mut inodes,
                    )?;
                }
                self.state_inodes = Some(inodes);
            }
            if self
                .state_inodes
                .as_ref()
                .is_some_and(|inodes| inodes.contains(&(metadata.dev(), metadata.ino())))
            {
                tracing::warn!(target: "session.profile", path = %path.display(), "Skipping native-state hardlink in configuration");
                return Ok(false);
            }
        }
        if let Some(previous) = self.files.get(path) {
            if *previous != fingerprint {
                bail!("configuration source changed between reads");
            }
        } else {
            self.files.insert(path.to_path_buf(), fingerprint);
        }
        self.seal_directory(
            path.parent()
                .context("configuration source has no parent")?,
        )?;
        Ok(true)
    }

    fn inventory(
        boundary: &NativeStateBoundary,
        directories: &mut BTreeMap<PathBuf, Fingerprint>,
        path: &Path,
        storage: bool,
        visited: &mut HashSet<(u64, u64, bool)>,
        inodes: &mut HashSet<(u64, u64)>,
    ) -> Result<()> {
        if storage
            && (boundary
                .stopped_original
                .as_ref()
                .is_some_and(|original| path.starts_with(original))
                || path.starts_with(&boundary.private_stage.path))
        {
            return Ok(());
        }
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inventorying native-state hardlinks"),
        };
        if metadata.is_file() {
            if metadata.nlink() > 1 {
                inodes.insert((metadata.dev(), metadata.ino()));
            }
        } else if metadata.is_dir() && visited.insert((metadata.dev(), metadata.ino(), storage)) {
            directories
                .entry(path.to_path_buf())
                .or_insert_with(|| Fingerprint::from(&metadata));
            for entry in fs::read_dir(path)? {
                Self::inventory(
                    boundary,
                    directories,
                    &entry?.path(),
                    storage,
                    visited,
                    inodes,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<()> {
        for (path, expected) in &self.entries {
            let current = match fs::symlink_metadata(path) {
                Ok(metadata) => Some((metadata.dev(), metadata.ino(), metadata.mode())),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error).context("validating native-state namespace entry"),
            };
            if current != *expected {
                bail!("native-state namespace changed during configuration seeding");
            }
        }
        self.boundary.source_root.validate()?;
        for (path, expected) in &self.routes {
            if canonical_expected_path(path)? != *expected {
                bail!("native-state boundary changed during configuration seeding");
            }
        }
        for (path, expected) in self.directories.iter().chain(&self.files) {
            if Fingerprint::from(&fs::metadata(path)?) != *expected {
                bail!(
                    "configuration source or native-state inventory changed during seeding: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
}
