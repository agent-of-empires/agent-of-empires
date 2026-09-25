//! Anchored native-state inode inventory with descendant alias witnesses.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::super::canonical_expected_path;
use super::super::policy::Exception;
use super::{Changed, DirectoryIdentity, NativeStateBoundary, ReadAccess, StateOrigin};
use crate::session::anchored_fs::AnchoredDir;

pub(super) struct Inventory<'a> {
    boundary: &'a NativeStateBoundary,
    access: ReadAccess<'a>,
    directories: &'a mut BTreeMap<PathBuf, DirectoryIdentity>,
    routes: &'a mut Vec<(PathBuf, PathBuf)>,
    entries: &'a mut BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
    visited: HashSet<(u64, u64, StateOrigin, bool, bool)>,
    inodes: HashSet<(u64, u64)>,
    skip_original: bool,
    symlinks_only: bool,
}

impl<'a> Inventory<'a> {
    pub(super) fn new(
        boundary: &'a NativeStateBoundary,
        access: ReadAccess<'a>,
        directories: &'a mut BTreeMap<PathBuf, DirectoryIdentity>,
        routes: &'a mut Vec<(PathBuf, PathBuf)>,
        entries: &'a mut BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
    ) -> Self {
        Self {
            boundary,
            access,
            directories,
            routes,
            entries,
            visited: HashSet::new(),
            inodes: HashSet::new(),
            skip_original: false,
            symlinks_only: false,
        }
    }

    pub(super) fn symlinks(
        boundary: &'a NativeStateBoundary,
        access: ReadAccess<'a>,
        directories: &'a mut BTreeMap<PathBuf, DirectoryIdentity>,
        routes: &'a mut Vec<(PathBuf, PathBuf)>,
        entries: &'a mut BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
    ) -> Self {
        let mut inventory = Self::new(boundary, access, directories, routes, entries);
        inventory.symlinks_only = true;
        inventory
    }

    pub(super) fn root(&mut self, path: &Path, origin: StateOrigin) -> Result<()> {
        self.skip_original = origin == StateOrigin::Storage
            && self
                .boundary
                .stopped_original
                .as_ref()
                .is_some_and(|original| original != path && original.starts_with(path));
        self.resolve(path, origin, false)
    }

    pub(super) fn finish(self) -> HashSet<(u64, u64)> {
        self.inodes
    }

    fn skip(&self, physical: &Path, origin: StateOrigin) -> bool {
        origin == StateOrigin::Storage
            && (physical.starts_with(&self.boundary.private_stage.path)
                || (self.skip_original
                    && self
                        .boundary
                        .stopped_original
                        .as_ref()
                        .is_some_and(|original| physical == original)))
    }

    fn resolve(&mut self, lookup: &Path, origin: StateOrigin, linked: bool) -> Result<()> {
        let canonical = match canonical_expected_path(lookup) {
            Ok(canonical) => canonical,
            // A loop reaches no inode; watching its entry catches a swap before publication.
            Err(error) if super::unresolvable(&error) => {
                return super::watch_entry(self.entries, lookup);
            }
            Err(error) => return Err(error.into()),
        };
        if self.skip(&canonical, origin) {
            return Ok(());
        }
        super::watch_entry(self.entries, lookup)?;
        super::watch_entry(self.entries, &canonical)?;
        if lookup != canonical {
            self.routes.push((lookup.to_path_buf(), canonical.clone()));
        }
        let parent = canonical
            .parent()
            .context("native state entry has no parent")?;
        let directory = match super::super::open_canonical_dir(parent) {
            Ok(directory) => directory,
            Err(error) if missing(&error) => return Ok(()),
            Err(error) => return Err(error).context("anchoring native-state inventory"),
        };
        let leaf = Path::new(
            canonical
                .file_name()
                .context("native state entry has no name")?,
        );
        self.entry(&directory, lookup, leaf, origin, linked)
    }

    fn entry(
        &mut self,
        directory: &AnchoredDir,
        lookup: &Path,
        leaf: &Path,
        origin: StateOrigin,
        linked: bool,
    ) -> Result<()> {
        let Some(stat) = directory.entry_stat(leaf)? else {
            return Ok(());
        };
        let mode = stat.st_mode & libc::S_IFMT;
        if mode == libc::S_IFLNK {
            // Lease descendant aliases, including absent targets.
            self.resolve(lookup, origin, true)?;
        } else if mode == libc::S_IFREG {
            let eligible = if self.symlinks_only {
                linked
            } else {
                stat.st_nlink > 1
            };
            if eligible
                && !self
                    .access
                    .allows_file(lookup, &directory.path().join(leaf), origin)
            {
                self.inodes.insert(identity(&stat));
            }
        } else if mode == libc::S_IFDIR {
            let child = directory.child(leaf).map_err(|error| {
                if missing(&error) {
                    Changed(format!(
                        "native state directory changed before inventory: {}",
                        lookup.display()
                    ))
                    .into()
                } else {
                    error
                }
            })?;
            let (device, inode) = child.identity()?;
            if device != stat.st_dev || inode != stat.st_ino {
                return Err(
                    Changed("native state directory changed before inventory".into()).into(),
                );
            }
            self.directory(&child, lookup, origin, linked)?;
        }
        Ok(())
    }

    fn directory(
        &mut self,
        directory: &AnchoredDir,
        lookup: &Path,
        origin: StateOrigin,
        linked: bool,
    ) -> Result<()> {
        if self.skip(directory.path(), origin) {
            return Ok(());
        }
        let (device, inode) = directory.identity()?;
        #[cfg(target_os = "macos")]
        let device = device as u64;
        let exceptional_spelling = match self.access.exception {
            Exception::Mixed {
                origin: allowed,
                leaves,
            } => allowed == origin && leaves.iter().any(|leaf| leaf.starts_with(lookup)),
            Exception::Carried { root } => {
                origin == StateOrigin::Native
                    && lookup.starts_with(root)
                    && directory.path().starts_with(root)
            }
            _ => false,
        };
        if !self.visited.insert((
            device,
            inode,
            origin,
            exceptional_spelling,
            self.symlinks_only && linked,
        )) {
            return Ok(());
        }
        if !self.symlinks_only || origin != StateOrigin::Storage {
            super::pin_anchored_directory(self.directories, directory)?;
        }
        for name in directory.read_dir(Path::new(""), usize::MAX)? {
            self.entry(
                directory,
                &lookup.join(&name),
                Path::new(&name),
                origin,
                linked,
            )?;
        }
        Ok(())
    }
}

fn identity(stat: &nix::sys::stat::FileStat) -> (u64, u64) {
    #[cfg(target_os = "macos")]
    let device = stat.st_dev as u64;
    #[cfg(not(target_os = "macos"))]
    let device = stat.st_dev;
    (device, stat.st_ino)
}

fn missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    }) || error
        .downcast_ref::<nix::errno::Errno>()
        .is_some_and(|error| {
            matches!(
                error,
                nix::errno::Errno::ENOENT | nix::errno::Errno::ENOTDIR
            )
        })
}
