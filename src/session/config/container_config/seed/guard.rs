//! Per-publication source stability and native-state hardlink checks.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{canonical_expected_path, NativeStateBoundary, ReadAccess, StateOrigin};
use crate::session::anchored_fs::AnchoredDir;

mod inventory;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct Changed(pub(super) String);

fn source_io(error: std::io::Error) -> anyhow::Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) {
        Changed("native source disappeared during seeding".into()).into()
    } else {
        error.into()
    }
}

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

#[derive(Clone, Copy, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl From<&Metadata> for DirectoryIdentity {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

pub(super) struct SourceRoot {
    anchor: AnchoredDir,
    lookup: PathBuf,
    identity: DirectoryIdentity,
}

impl SourceRoot {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let anchor = super::open_canonical_dir(&fs::canonicalize(path).map_err(source_io)?)?;
        let metadata = fs::metadata(anchor.path()).map_err(source_io)?;
        let (device, inode) = anchor.identity()?;
        #[cfg(target_os = "macos")]
        let device = device as u64;
        if metadata.dev() != device || metadata.ino() != inode {
            return Err(Changed("native source root changed before capture".into()).into());
        }
        Ok(Self {
            anchor,
            lookup: path.to_path_buf(),
            identity: DirectoryIdentity::from(&metadata),
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.anchor.path()
    }

    pub(super) fn validate(&self) -> Result<()> {
        if fs::canonicalize(&self.lookup).map_err(source_io)? != self.anchor.path()
            || DirectoryIdentity::from(&fs::metadata(&self.lookup).map_err(source_io)?)
                != self.identity
        {
            return Err(
                Changed("native configuration source root changed during seeding".into()).into(),
            );
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
        // Native-state routes are compared in canonical spelling, so pin the
        // resolved spelling of the stage instead of the lexical one.
        let path = fs::canonicalize(temporary.keep())?;
        let anchor = AnchoredDir::open(&path)?;
        Ok(Self { path, anchor })
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
    pub(super) access: ReadAccess<'a>,
    aliases: Vec<(PathBuf, StateOrigin)>,
    routes: Vec<(PathBuf, PathBuf)>,
    directories: BTreeMap<PathBuf, DirectoryIdentity>,
    files: BTreeMap<PathBuf, Fingerprint>,
    state_inodes: Option<HashSet<(u64, u64)>>,
    symlink_inodes: Option<HashSet<(u64, u64)>>,
    entries: BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
}

impl<'a> ReadGuard<'a> {
    pub(super) fn new(boundary: &'a NativeStateBoundary, access: ReadAccess<'a>) -> Result<Self> {
        boundary.source_root.validate()?;
        boundary.hermes.validate()?;
        access.validate()?;
        let mut guard = Self {
            boundary,
            access,
            aliases: Vec::new(),
            routes: Vec::new(),
            directories: BTreeMap::new(),
            files: BTreeMap::new(),
            state_inodes: None,
            symlink_inodes: None,
            entries: BTreeMap::new(),
        };
        for (path, origin) in &boundary.paths {
            guard.add_alias(path, *origin)?;
        }
        for (spelling, expected) in &boundary.routes {
            watch_entry(&mut guard.entries, spelling)?;
            guard.routes.push((spelling.clone(), expected.clone()));
        }
        for (root, rule, origin) in &boundary.patterns {
            for ancestor in Path::new(rule.pattern.as_str()).ancestors().skip(1) {
                let prefix = root.join(ancestor);
                watch_entry(&mut guard.entries, &prefix)?;
                for entry in super::state_glob(
                    root,
                    ancestor
                        .to_str()
                        .context("native state pattern is not UTF-8")?,
                )? {
                    let path = entry.context("inspecting native-state pattern parent")?;
                    if path.is_dir() {
                        seal_directory(&mut guard.directories, &path)?;
                    }
                }
            }
            for entry in super::state_glob(root, rule.pattern.as_str())? {
                let entry = entry.context("inspecting native-state pattern")?;
                if entry
                    .strip_prefix(root)
                    .is_ok_and(|relative| rule.matches(relative))
                {
                    guard.add_alias(&entry, *origin)?;
                }
            }
        }
        Ok(guard)
    }

    fn add_alias(&mut self, path: &Path, origin: StateOrigin) -> Result<()> {
        watch_entry(&mut self.entries, path)?;
        let canonical = match canonical_expected_path(path) {
            Ok(canonical) => canonical,
            // A loop names no reachable state: fence its spelling; the watched entry catches a
            // swap to a real alias before publication.
            Err(error) if unresolvable(&error) => crate::git::template::lexical_normalize(path),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolving native-state boundary {}", path.display()))
            }
        };
        self.routes.push((path.to_path_buf(), canonical.clone()));
        self.aliases.push((canonical, origin));
        Ok(())
    }

    pub(super) fn record_route(&mut self, lookup: &Path, canonical: &Path) -> Result<()> {
        if fs::canonicalize(lookup).map_err(source_io)? != canonical {
            return Err(Changed("configuration source alias changed before reading".into()).into());
        }
        if lookup != canonical {
            self.routes
                .push((lookup.to_path_buf(), canonical.to_path_buf()));
        }
        Ok(())
    }

    pub(super) fn record_directory(&mut self, directory: &AnchoredDir) -> Result<()> {
        if self.aliases.iter().any(|(state, origin)| {
            self.boundary
                .rejects_path(directory.path(), state, true, *origin, self.access)
        }) {
            bail!("configuration resource overlaps native state after source resolution");
        }
        pin_anchored_directory(&mut self.directories, directory)
    }

    pub(super) fn pin_directory(&mut self, directory: &AnchoredDir) -> Result<()> {
        pin_anchored_directory(&mut self.directories, directory)
    }

    pub(super) fn record_file(&mut self, path: &Path, file: &File) -> Result<bool> {
        let metadata = file.metadata()?;
        if self.aliases.iter().any(|(state, origin)| {
            self.boundary
                .rejects_path(path, state, false, *origin, self.access)
        }) {
            return Ok(false);
        }
        if self.symlink_inodes.is_none() {
            self.symlink_inodes = Some(scan_symlinks(
                self.boundary,
                self.access,
                &self.aliases,
                &mut self.directories,
                &mut self.routes,
                &mut self.entries,
            )?);
        }
        if self
            .symlink_inodes
            .as_ref()
            .is_some_and(|inodes| inodes.contains(&(metadata.dev(), metadata.ino())))
        {
            tracing::warn!(target: "session.profile", path = %path.display(), "Skipping native-state symlink target in configuration");
            return Ok(false);
        }
        let fingerprint = Fingerprint::from(&metadata);
        if metadata.nlink() > 1 {
            if self.state_inodes.is_none() {
                let mut inodes = HashSet::new();
                for (state, origin) in &self.aliases {
                    let mut walk = inventory::Inventory::new(
                        self.boundary,
                        self.access,
                        &mut self.directories,
                        &mut self.routes,
                        &mut self.entries,
                    );
                    walk.root(state, *origin)?;
                    inodes.extend(walk.finish());
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
                return Err(Changed("configuration source changed between reads".into()).into());
            }
        } else {
            self.files.insert(path.to_path_buf(), fingerprint);
        }
        seal_directory(
            &mut self.directories,
            path.parent()
                .context("configuration source has no parent")?,
        )?;
        Ok(true)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if !self.files.is_empty() {
            let current = Self::new(self.boundary, self.access)?;
            if self.files.keys().any(|file| {
                current.aliases.iter().any(|(state, origin)| {
                    self.boundary
                        .rejects_path(file, state, false, *origin, self.access)
                })
            }) {
                bail!("configuration source became native state during seeding");
            }
            let aliases = current.aliases;
            let mut directories = BTreeMap::new();
            let mut routes = Vec::new();
            let mut entries = BTreeMap::new();
            let mut inodes = scan_symlinks(
                self.boundary,
                self.access,
                &aliases,
                &mut directories,
                &mut routes,
                &mut entries,
            )?;
            if self.files.values().any(|file| file.links > 1) {
                for (state, origin) in &aliases {
                    let mut walk = inventory::Inventory::new(
                        self.boundary,
                        self.access,
                        &mut directories,
                        &mut routes,
                        &mut entries,
                    );
                    walk.root(state, *origin)?;
                    inodes.extend(walk.finish());
                }
            }
            validate_namespace(&entries, &routes)?;
            for (path, expected) in &directories {
                if DirectoryIdentity::from(&fs::metadata(path).map_err(source_io)?) != *expected {
                    return Err(
                        Changed("native-state inventory changed during validation".into()).into(),
                    );
                }
            }
            if self
                .files
                .values()
                .any(|file| inodes.contains(&(file.device, file.inode)))
            {
                bail!("configuration source became native state during seeding");
            }
        }
        validate_namespace(&self.entries, &self.routes)?;
        self.boundary.source_root.validate()?;
        self.boundary.hermes.validate()?;
        self.access.validate()?;
        for (path, expected) in &self.directories {
            if DirectoryIdentity::from(&fs::metadata(path).map_err(source_io)?) != *expected {
                return Err(Changed(format!(
                    "configuration source directory changed during seeding: {}",
                    path.display()
                ))
                .into());
            }
        }
        for (path, expected) in &self.files {
            if Fingerprint::from(&fs::metadata(path).map_err(source_io)?) != *expected {
                return Err(Changed(format!(
                    "configuration source file changed during seeding: {}",
                    path.display()
                ))
                .into());
            }
        }
        Ok(())
    }
}

fn scan_symlinks(
    boundary: &NativeStateBoundary,
    access: ReadAccess<'_>,
    aliases: &[(PathBuf, StateOrigin)],
    directories: &mut BTreeMap<PathBuf, DirectoryIdentity>,
    routes: &mut Vec<(PathBuf, PathBuf)>,
    entries: &mut BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
) -> Result<HashSet<(u64, u64)>> {
    let mut walk = inventory::Inventory::symlinks(boundary, access, directories, routes, entries);
    for (state, origin) in aliases {
        walk.root(state, *origin)?;
    }
    Ok(walk.finish())
}

fn validate_namespace(
    entries: &BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
    routes: &[(PathBuf, PathBuf)],
) -> Result<()> {
    for (path, expected) in entries {
        let current = match fs::symlink_metadata(path) {
            Ok(metadata) => Some((metadata.dev(), metadata.ino(), metadata.mode())),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) || unresolvable(&error) =>
            {
                None
            }
            Err(error) => return Err(error).context("validating native-state namespace entry"),
        };
        if current != *expected {
            return Err(Changed(format!(
                "native-state namespace changed during configuration seeding at {}: expected {:?}, found {:?}",
                path.display(),
                expected,
                current
            )).into());
        }
    }
    for (path, expected) in routes {
        let current = match canonical_expected_path(path) {
            Ok(current) => current,
            // Still a loop matches its recorded spelling; one that now resolves does not.
            Err(error) if unresolvable(&error) => crate::git::template::lexical_normalize(path),
            Err(error) => return Err(error.into()),
        };
        if current != *expected {
            return Err(Changed(
                "native-state boundary changed during configuration seeding".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// A symlink loop, or a component that is not a directory, resolves to nothing.
pub(super) fn unresolvable(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

fn watch_entry(
    entries: &mut BTreeMap<PathBuf, Option<(u64, u64, u32)>>,
    path: &Path,
) -> Result<()> {
    // Watch the relevant entry or first missing component, not the mtime of
    // an unrelated ancestor such as /tmp or HOME.
    let mut cursor = Some(path);
    let mut missing = None;
    while let Some(candidate) = cursor {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                entries.entry(candidate.to_path_buf()).or_insert(Some((
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mode(),
                )));
                if let Some(missing) = missing {
                    entries.entry(missing).or_insert(None);
                }
                return Ok(());
            }
            // Below a looped ancestor, watch the loop itself.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) || unresolvable(&error) =>
            {
                missing = Some(candidate.to_path_buf());
                cursor = candidate.parent();
            }
            Err(error) => return Err(error).context("inspecting native-state namespace entry"),
        }
    }
    bail!("native-state boundary has no existing ancestor")
}

fn seal_directory(
    directories: &mut BTreeMap<PathBuf, DirectoryIdentity>,
    path: &Path,
) -> Result<()> {
    let metadata = fs::metadata(path).map_err(source_io)?;
    if !metadata.is_dir() {
        return Err(Changed("native-state directory changed type".into()).into());
    }
    directories
        .entry(path.to_path_buf())
        .or_insert_with(|| DirectoryIdentity::from(&metadata));
    Ok(())
}

fn pin_anchored_directory(
    directories: &mut BTreeMap<PathBuf, DirectoryIdentity>,
    directory: &AnchoredDir,
) -> Result<()> {
    let metadata = fs::metadata(directory.path()).map_err(source_io)?;
    let (device, inode) = directory.identity()?;
    #[cfg(target_os = "macos")]
    let device = device as u64;
    if metadata.dev() != device || metadata.ino() != inode {
        return Err(Changed("configuration source directory changed before reading".into()).into());
    }
    seal_directory(directories, directory.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn external_state_alias_identities_are_rejected() {
        for directory_alias in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            let state = temporary.path().join("state");
            let external = temporary.path().join("external");
            for path in [&source, &active, &state, &external] {
                fs::create_dir(path).unwrap();
            }
            let candidate = external.join("state.json");
            let authored = external.join("authored.json");
            fs::write(&candidate, b"SYNTHETIC_STATE").unwrap();
            fs::write(&authored, b"AUTHORED_CONFIG").unwrap();
            if directory_alias {
                fs::create_dir(external.join("records")).unwrap();
                symlink(&candidate, external.join("records/record")).unwrap();
                symlink(external.join("records"), state.join("alias")).unwrap();
            } else {
                symlink(&candidate, state.join("alias")).unwrap();
            }
            let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            boundary.add_path(state);
            let mut guard = ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
            assert!(!guard
                .record_file(&candidate, &File::open(&candidate).unwrap())
                .unwrap());
            assert!(guard
                .record_file(&authored, &File::open(&authored).unwrap())
                .unwrap());
            guard.validate().unwrap();
            assert_eq!(fs::read(candidate).unwrap(), b"SYNTHETIC_STATE");
            assert_eq!(fs::read(authored).unwrap(), b"AUTHORED_CONFIG");
        }
    }

    #[test]
    fn validation_rechecks_new_state_subdirectories() {
        for (origin, conflict) in [
            (StateOrigin::Native, true),
            (StateOrigin::Storage, true),
            (StateOrigin::Storage, false),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            let state = temporary.path().join("state");
            for path in [&source, &active, &state] {
                fs::create_dir(path).unwrap();
            }
            let candidate = source.join("config.json");
            fs::write(&candidate, b"SYNTHETIC_CONFIG").unwrap();
            fs::write(active.join("config.json"), b"LOCAL_CONFIG").unwrap();
            let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            boundary.add_classified_path(state.clone(), origin);
            let mut guard = ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
            assert!(guard
                .record_file(&candidate, &File::open(&candidate).unwrap())
                .unwrap());
            fs::create_dir(state.join("new")).unwrap();
            if conflict {
                symlink(&candidate, state.join("new/record")).unwrap();
                assert!(guard.validate().is_err());
            } else {
                fs::write(state.join("new/record"), b"UNRELATED_STATE").unwrap();
                guard.validate().unwrap();
            }
            assert_eq!(fs::read(&candidate).unwrap(), b"SYNTHETIC_CONFIG");
            assert_eq!(
                fs::read(active.join("config.json")).unwrap(),
                b"LOCAL_CONFIG"
            );
        }
    }

    #[test]
    fn moving_a_hardlink_into_native_state_blocks_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        let state = temporary.path().join("state");
        for path in [&source, &active, &state] {
            fs::create_dir(path).unwrap();
        }
        let selected = source.join("settings.json");
        let peer = temporary.path().join("authored-link");
        fs::write(&selected, b"AUTHORED_CONFIG").unwrap();
        fs::hard_link(&selected, &peer).unwrap();
        fs::write(active.join("settings.json"), b"LOCAL_CONFIG").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        boundary.add_path(state.clone());
        let mut guard = ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
        let mut file = File::open(&selected).unwrap();
        assert!(guard.record_file(&selected, &file).unwrap());

        fs::rename(peer, state.join("native-link")).unwrap();
        let error = super::super::publish_guarded_file(
            &mut file,
            &guard,
            &AnchoredDir::open(&active).unwrap(),
            Path::new("settings.json"),
            fs::metadata(&selected).unwrap().permissions(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("native state"));
        assert_eq!(
            fs::read(active.join("settings.json")).unwrap(),
            b"LOCAL_CONFIG"
        );
    }

    #[test]
    fn new_globbed_native_scope_cannot_alias_a_selected_file() {
        for direct in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&active).unwrap();
            let selected = source.join("settings.json");
            fs::write(&selected, b"APPROVED_CONFIG").unwrap();
            let selected = fs::canonicalize(selected).unwrap();
            let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            boundary
                .add_state_rule(
                    &source,
                    std::sync::Arc::new(
                        super::super::policy::NativeRule::new("history-*", None).unwrap(),
                    ),
                    StateOrigin::Native,
                )
                .unwrap();
            let mut guard = ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
            assert!(guard
                .record_file(&selected, &File::open(&selected).unwrap())
                .unwrap());
            let new_scope = source.join("history-new");
            if direct {
                symlink(&selected, &new_scope).unwrap();
            } else {
                fs::create_dir(&new_scope).unwrap();
                symlink(&selected, new_scope.join("alias")).unwrap();
            }
            let error = guard.validate().unwrap_err();
            assert!(
                error.to_string().contains("native state"),
                "direct={direct}: {error}"
            );
        }
    }

    #[test]
    fn a_looped_native_path_or_ancestor_fails_validation_once_it_resolves() {
        for (layout, looped, watched) in [
            ("path", "projects", "projects"),
            ("ancestor", "agent", "agent/sessions"),
        ] {
            for swap in [false, true] {
                let temporary = tempfile::tempdir().unwrap();
                let source = temporary.path().join("source");
                let active = temporary.path().join("active");
                fs::create_dir(&source).unwrap();
                fs::create_dir(&active).unwrap();
                let looped = source.join(looped);
                symlink(looped.file_name().unwrap(), &looped).unwrap();
                let candidate = source.join("config.json");
                fs::write(&candidate, b"{}").unwrap();
                let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
                boundary.add_path(source.join(watched));
                let mut guard = ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
                assert!(guard
                    .record_file(&candidate, &File::open(&candidate).unwrap())
                    .unwrap());
                if swap {
                    fs::remove_file(&looped).unwrap();
                    if layout == "path" {
                        symlink("config.json", &looped).unwrap();
                    } else {
                        fs::create_dir(&looped).unwrap();
                        symlink("../config.json", looped.join("sessions")).unwrap();
                    }
                }
                assert_eq!(guard.validate().is_err(), swap, "{layout} swap={swap}");
            }
        }
    }

    #[test]
    fn changed_file_between_reads_is_reopened_before_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&active).unwrap();
        let candidate = source.join("config.json");
        fs::write(&candidate, b"OLD_SOURCE").unwrap();
        let boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mut attempts = 0;
        let copied = super::super::retry_source_change(|| {
            attempts += 1;
            let mut guard = ReadGuard::new(&boundary, ReadAccess::default())?;
            guard.record_file(&candidate, &File::open(&candidate)?)?;
            let bytes = fs::read(&candidate)?;
            // A different length, so the rewrite is visible within one timestamp tick.
            if attempts == 1 {
                fs::write(&candidate, b"NEW_LONGER_SOURCE")?;
            }
            guard.record_file(&candidate, &File::open(&candidate)?)?;
            guard.validate()?;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(copied, b"NEW_LONGER_SOURCE");
        assert_eq!(attempts, 2);
    }
}
