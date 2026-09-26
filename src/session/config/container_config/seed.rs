//! Positive native-config seeding, separated from container mount construction.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, Permissions};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result};

use crate::git::template::lexical_normalize;
use crate::session::anchored_fs::AnchoredDir;
use crate::session::capture;
use crate::session::config::SessionConfig;

use super::{AgentConfigMount, AGENT_CONFIG_MOUNTS, SANDBOX_PRIVATE_SUBDIR, SANDBOX_SUBDIR};

mod guard;
mod hermes;
mod policy;
use policy::{Exception, NativeRule, ReadAccess, StateOrigin};

pub(super) fn source_changed(error: &anyhow::Error) -> bool {
    error.is::<guard::Changed>()
}

pub(super) fn retry_source_change<T>(mut seed: impl FnMut() -> Result<T>) -> Result<T> {
    for attempt in 0..5 {
        match seed() {
            Err(error) if source_changed(&error) => {
                std::thread::sleep(std::time::Duration::from_millis(10 << attempt));
            }
            result => return result,
        }
    }
    seed()
}

pub(super) struct NativeStateBoundary {
    source_root: guard::SourceRoot,
    origin_root: PathBuf,
    private_stage: guard::PrivateStage,
    stopped_original: Option<PathBuf>,
    paths: Vec<(PathBuf, StateOrigin)>,
    patterns: Vec<(PathBuf, Arc<NativeRule>, StateOrigin)>,
    routes: Vec<(PathBuf, PathBuf)>,
    hermes: hermes::Scopes,
}

impl NativeStateBoundary {
    fn for_source(source: &Path, destination: &Path) -> Result<Self> {
        let private_stage = guard::PrivateStage::new(destination)?;
        Ok(Self {
            source_root: guard::SourceRoot::new(source)?,
            origin_root: source.to_path_buf(),
            private_stage,
            stopped_original: None,
            paths: Vec::new(),
            patterns: Vec::new(),
            routes: Vec::new(),
            hermes: hermes::Scopes::default(),
        })
    }

    pub(super) fn validate_source(&self) -> Result<()> {
        self.source_root.validate()
    }
    #[cfg(test)]
    pub(super) fn for_fixture(
        source: &Path,
        destination: &Path,
        mount: &AgentConfigMount,
    ) -> Result<Self> {
        let mut boundary = Self::for_source(source, destination)?;
        if mount.tool_name == "hermes" {
            hermes::register_source(&mut boundary, source)?;
        } else {
            boundary.add_root(source, mount)?;
        }
        Ok(boundary)
    }
    pub(super) fn new(
        source: &Path,
        mount: &AgentConfigMount,
        home: &Path,
        config: &SessionConfig,
        destination: &Path,
    ) -> Result<Self> {
        let mut boundary = Self::for_source(source, destination)?;
        boundary.hermes.default_root = Some(canonical_expected_path(&home.join(".hermes"))?);
        for registered in AGENT_CONFIG_MOUNTS {
            boundary.add_root(&home.join(registered.host_rel), registered)?;
        }
        boundary.add_declared_roots(config, home, mount)?;
        for profile in crate::session::list_profiles()? {
            let registered = crate::session::config::profile_config::resolve_config(&profile)?;
            boundary.add_declared_roots(&registered.session, home, mount)?;
        }
        if mount.tool_name == "hermes" {
            hermes::register_source(&mut boundary, source)?;
        } else {
            boundary.add_root(source, mount)?;
        }
        boundary.add_path(home.join(".local/share/kiro-cli"));
        boundary.add_path(home.join(".hermes/heapdumps"));
        if let Ok(app_dir) = crate::session::get_app_dir() {
            boundary.add_path(app_dir);
        }
        boundary.paths.sort();
        boundary.paths.dedup();
        Ok(boundary)
    }

    pub(super) fn for_stopped_original(
        mut self,
        host: &Path,
        mount: &AgentConfigMount,
    ) -> Result<Self> {
        self.add_root(host, mount)?;
        let scopes = hermes::original_scope_map(&mut self, host)?;
        let mapped_origin = |origin| match origin {
            StateOrigin::Hermes { scope, marker } => StateOrigin::Hermes {
                scope: scopes.get(&scope).copied().unwrap_or(scope),
                marker,
            },
            other => other,
        };
        let mapped_paths: Vec<_> = self
            .paths
            .iter()
            .filter_map(|(path, origin)| {
                path.strip_prefix(host).ok().map(|relative| {
                    (
                        self.source_root.path().join(relative),
                        mapped_origin(*origin),
                    )
                })
            })
            .collect();
        let mapped_patterns: Vec<_> = self
            .patterns
            .iter()
            .filter_map(|(root, rule, origin)| {
                root.strip_prefix(host).ok().map(|relative| {
                    (
                        self.source_root.path().join(relative),
                        Arc::clone(rule),
                        mapped_origin(*origin),
                    )
                })
            })
            .collect();
        for (path, origin) in mapped_paths {
            self.add_classified_path(path, origin);
        }
        self.patterns.extend(mapped_patterns);
        self.origin_root = host.to_path_buf();
        self.stopped_original = Some(self.source_root.path().to_path_buf());
        Ok(self)
    }
    fn add_declared_roots(
        &mut self,
        config: &SessionConfig,
        home: &Path,
        active_mount: &AgentConfigMount,
    ) -> Result<()> {
        for tool in config.agent_config_dir.keys() {
            let Some(root) = config.agent_config_dir_for(tool, home) else {
                continue;
            };
            let mut classified = false;
            if let Some(agent) = super::resolve_executed_agent(tool, None, config) {
                for mount in AGENT_CONFIG_MOUNTS
                    .iter()
                    .filter(|mount| mount.tool_name == agent.name)
                {
                    self.add_root(&root, mount)?;
                    classified = true;
                }
            }
            if classified {
                continue;
            }
            // A status-only alias declaring the active source keeps every mount of the active
            // agent fenced there, not only the one seeding now.
            if super::resolve_active_agent(tool, None, config)
                .is_some_and(|agent| agent.name == active_mount.tool_name)
                && canonical_expected_path(&root)?
                    == canonical_expected_path(self.source_root.path())?
            {
                for mount in AGENT_CONFIG_MOUNTS
                    .iter()
                    .filter(|mount| mount.tool_name == active_mount.tool_name)
                {
                    self.add_root(&root, mount)?;
                }
            } else {
                self.add_path(root);
            }
        }
        Ok(())
    }
    fn add_root(&mut self, root: &Path, mount: &AgentConfigMount) -> Result<()> {
        self.add_route(root)?;
        if mount.tool_name == "hermes" {
            hermes::register_home(self, root)?;
            return Ok(());
        }
        self.add_storage_root(root)?;
        for name in mount.native_state_paths {
            self.add_state_rule(
                root,
                Arc::new(NativeRule::new(name, None)?),
                StateOrigin::Native,
            )?;
        }
        Ok(())
    }

    fn add_storage_root(&mut self, root: &Path) -> Result<()> {
        let canonical = canonical_expected_path(root)?;
        for parent in [root.parent(), canonical.parent()].into_iter().flatten() {
            self.add_classified_path(
                parent.join(crate::migrations::v033_isolate_sandbox_content::RECOVERY),
                StateOrigin::Storage,
            );
        }
        for name in [SANDBOX_SUBDIR, SANDBOX_PRIVATE_SUBDIR] {
            self.add_classified_path(root.join(name), StateOrigin::Storage);
            self.add_classified_path(canonical.join(name), StateOrigin::Storage);
        }
        Ok(())
    }

    fn add_state_rule(
        &mut self,
        root: &Path,
        rule: Arc<NativeRule>,
        origin: StateOrigin,
    ) -> Result<()> {
        let canonical = canonical_expected_path(root)?;
        let name = rule.pattern.as_str();
        if name.contains(['*', '?', '[']) {
            for entry in state_glob(root, name)? {
                let entry =
                    entry.context("inspecting a native-state alias before config seeding")?;
                if entry
                    .strip_prefix(root)
                    .is_ok_and(|relative| rule.matches(relative))
                {
                    self.add_classified_path(entry, origin);
                }
            }
            let lexical = lexical_normalize(root);
            if lexical != canonical {
                self.patterns.push((lexical, Arc::clone(&rule), origin));
            }
            self.patterns.push((canonical, rule, origin));
        } else {
            self.add_classified_path(root.join(name), origin);
            self.add_classified_path(canonical.join(name), origin);
        }
        Ok(())
    }

    fn add_path(&mut self, path: PathBuf) {
        self.add_classified_path(path, StateOrigin::Native);
    }

    fn add_classified_path(&mut self, path: PathBuf, origin: StateOrigin) {
        if let Ok(canonical) = canonical_expected_path(&path) {
            self.paths.push((canonical, origin));
        }
        self.paths.push((lexical_normalize(&path), origin));
    }

    /// Pin a declared spelling. Deduplicating native scopes must never drop the
    /// route the caller declared, or a later retarget of that spelling would go
    /// unnoticed and its recorded rules would quietly describe another store.
    fn add_route(&mut self, spelling: &Path) -> Result<()> {
        if self.routes.iter().any(|(recorded, _)| recorded == spelling) {
            return Ok(());
        }
        let canonical = canonical_expected_path(spelling)?;
        self.routes.push((spelling.to_path_buf(), canonical));
        Ok(())
    }

    fn rejects_path(
        &self,
        candidate: &Path,
        state: &Path,
        directory: bool,
        origin: StateOrigin,
        access: ReadAccess<'_>,
    ) -> bool {
        let admitted_ancestor = origin == StateOrigin::Storage
            && self.stopped_original.as_ref().is_some_and(|original| {
                candidate.starts_with(original) && original.starts_with(state) && original != state
            });
        !admitted_ancestor
            && (candidate.starts_with(state) || (directory && state.starts_with(candidate)))
            && !access.allows(candidate, state, directory, origin)
    }

    fn rejects(&self, candidate: &Path, directory: bool, access: ReadAccess<'_>) -> bool {
        self.paths
            .iter()
            .any(|(state, origin)| self.rejects_path(candidate, state, directory, *origin, access))
            || self.patterns.iter().any(|(root, rule, origin)| {
                candidate.strip_prefix(root).is_ok_and(|relative| {
                    relative.ancestors().any(|ancestor| {
                        rule.matches(ancestor)
                            && self.rejects_path(
                                candidate,
                                &root.join(ancestor),
                                directory,
                                *origin,
                                access,
                            )
                    })
                }) || (directory
                    && root.starts_with(candidate)
                    && self.rejects_path(candidate, root, true, *origin, access))
            })
    }
}

fn state_glob(root: &Path, pattern: &str) -> Result<glob::Paths> {
    let root = glob::Pattern::escape(root.to_str().context("native state root is not UTF-8")?);
    Ok(glob::glob(&format!("{root}/{pattern}"))?)
}

fn canonical_expected_path(path: &Path) -> std::io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = fs::read_link(path)?;
                    return canonical_expected_path(&if target.is_absolute() {
                        target
                    } else {
                        parent.join(target)
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let leaf = path.file_name().ok_or(error)?;
            Ok(canonical_expected_path(parent)?.join(leaf))
        }
        Err(error) => Err(error),
    }
}

/// Resolve intentional /tmp, dotfile and Nix links, then pin the spelling
/// without following any subsequent component replacement.
fn open_canonical_dir(path: &Path) -> Result<AnchoredDir> {
    // The host may reach this path through a link it owns (macOS `/tmp`, a
    // developer's symlinked temporary root), so resolve the spelling before the
    // walk: every component below the anchor is then one this process created.
    let resolved = canonical_expected_path(path)?;
    let filesystem = AnchoredDir::open(Path::new("/"))?;
    filesystem.child(
        resolved
            .strip_prefix("/")
            .context("canonical source must be absolute")?,
    )
}

/// Whether a resolved path leaves the store a container wrote. A retained
/// original is that store, so a link inside it must not reach the host; a host
/// config directory is the user's own, where their links are what they asked
/// for and are followed as before.
fn original_escapes(boundary: &NativeStateBoundary, canonical: &Path) -> bool {
    boundary.stopped_original.is_some() && !canonical.starts_with(boundary.source_root.path())
}

fn canonical_source(
    path: &Path,
    boundary: &NativeStateBoundary,
    directory: bool,
    access: ReadAccess<'_>,
) -> Result<Option<PathBuf>> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // A linked or looped carried file is not carried, like a linked directory.
        Err(error)
            if matches!(access.exception, Exception::Carried { .. })
                && matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) =>
        {
            return Ok(None);
        }
        Err(error) if matches!(access.exception, Exception::Carried { .. }) => {
            return Err(error).context("resolving carried native state");
        }
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error,
            "Skipping unreadable configuration source");
            return Ok(None);
        }
    };
    if original_escapes(boundary, &canonical) {
        tracing::warn!(target: "session.profile", path = %path.display(),
        "Skipping configuration source that leaves the store it was retained from");
        return Ok(None);
    }
    if boundary.rejects(&canonical, directory, access) {
        tracing::warn!(target: "session.profile", path = %path.display(),
        "Skipping configuration resource overlapping native session state");
        return Ok(None);
    }
    Ok(Some(canonical))
}

fn open_canonical_file(path: &Path, access: ReadAccess<'_>) -> Result<Option<File>> {
    let parent = path
        .parent()
        .context("configuration source has no parent")?;
    let anchor = match open_canonical_dir(parent) {
        Ok(anchor) => anchor,
        Err(error) if matches!(access.exception, Exception::Carried { .. }) => {
            return Err(error).context("opening carried native-state parent");
        }
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error, "Skipping changed or unreadable configuration source parent");
            return Ok(None);
        }
    };
    match anchor.open_regular(
        Path::new(path.file_name().context("source has no leaf")?),
        usize::MAX,
    ) {
        Ok(file) => Ok(file),
        Err(error) if matches!(access.exception, Exception::Carried { .. }) => {
            Err(error).context("opening carried native-state file")
        }
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error, "Skipping unreadable configuration file");
            Ok(None)
        }
    }
}

fn publish_source_file(
    path: &Path,
    destination: &AnchoredDir,
    leaf: &Path,
    boundary: &NativeStateBoundary,
    replace: bool,
    access: ReadAccess<'_>,
) -> Result<bool> {
    let Some(canonical) = canonical_source(path, boundary, false, access)? else {
        return Ok(false);
    };
    let Some(mut file) = open_canonical_file(&canonical, access)? else {
        return Ok(false);
    };
    let mut guard = guard::ReadGuard::new(boundary, access)?;
    guard.record_route(path, &canonical)?;
    if !guard.record_file(&canonical, &file)? {
        return Ok(false);
    }
    let permissions = file.metadata()?.permissions();
    publish_guarded_file(&mut file, &guard, destination, leaf, permissions, replace)
}

fn publish_guarded_file(
    file: &mut File,
    guard: &guard::ReadGuard<'_>,
    destination: &AnchoredDir,
    leaf: &Path,
    permissions: Permissions,
    replace: bool,
) -> Result<bool> {
    let validate = || guard.validate();
    destination.publish_file(
        leaf,
        file,
        permissions,
        replace,
        Some(crate::session::anchored_fs::FilePublication {
            staging: &guard.boundary.private_stage.anchor,
            validate: &validate,
        }),
    )
}

pub(super) fn sync_agent_config(
    host_dir: &Path,
    sandbox_dir: &Path,
    copy_files: &[&str],
    seed_files: &[(&str, &str)],
    copy_dirs: &[&str],
    preserve_files: &[&str],
    boundary: &NativeStateBoundary,
) -> Result<()> {
    // A retained original is content a container wrote, so a link inside it
    // must not reach the host. A host config directory is the user's own, where
    // following their links is what they asked for.
    let discovery_links = boundary.stopped_original.is_none();
    let destination = AnchoredDir::open(sandbox_dir)?;
    for &(name, content) in seed_files {
        let relative = Path::new(name);
        destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        destination.publish_file(
            relative,
            &mut content.as_bytes(),
            Permissions::from_mode(0o600),
            false,
            None,
        )?;
    }
    for &name in copy_files {
        let relative = Path::new(name);
        let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(
            relative
                .file_name()
                .context("configuration file has no leaf")?,
        );
        let preserve = preserve_files.contains(&name);
        if preserve && parent.regular_lookup(leaf)?.is_some() {
            continue;
        }
        publish_source_file(
            &host_dir.join(relative),
            &parent,
            leaf,
            boundary,
            !preserve,
            ReadAccess::default(),
        )?;
    }
    for &name in copy_dirs {
        let relative = Path::new(name);
        let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(
            relative
                .file_name()
                .context("resource directory has no leaf")?,
        );
        seed_directory(
            &host_dir.join(relative),
            &parent,
            leaf,
            boundary,
            discovery_links,
            ReadAccess::default(),
        )?;
    }
    Ok(())
}

/// Carry a retired original's never-host-copied state (its own resume) into the
/// fresh store, so isolating the original does not reset resume. Only a stopped
/// original lends it; the `Exception::Carried` scope admits native content
/// under each matched entry while the escape and hardlink guards still refuse
/// anything that resolves outside it (a link out of the store, or to another
/// native path such as the retired history).
pub(super) fn carry_sandbox_state(
    source: &Path,
    destination: &Path,
    patterns: &[&str],
    boundary: &NativeStateBoundary,
) -> Result<()> {
    if patterns.is_empty() || boundary.stopped_original.is_none() {
        return Ok(());
    }
    let destination = AnchoredDir::open(destination)?;
    for &pattern in patterns {
        for entry in state_glob(source, pattern)? {
            let entry = entry.context("expanding a carried native-state pattern")?;
            let Ok(relative) = entry.strip_prefix(source) else {
                continue;
            };
            let canonical = match fs::canonicalize(&entry) {
                Ok(canonical) => canonical,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) =>
                {
                    continue
                }
                Err(error) => return Err(error).context("resolving carried native state"),
            };
            let declared = boundary.source_root.path().join(relative);
            if !canonical.starts_with(&declared) {
                anyhow::bail!(
                    "carried native state {} leaves its declared root {}",
                    entry.display(),
                    declared.display()
                );
            }
            let leaf = Path::new(relative.file_name().context("carried state has no leaf")?);
            let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
            let access = ReadAccess {
                root: Some(&boundary.source_root),
                exception: Exception::Carried { root: &declared },
            };
            if entry.is_dir() {
                seed_directory(&entry, &parent, leaf, boundary, false, access)?;
            } else {
                publish_source_file(&entry, &parent, leaf, boundary, true, access)?;
            }
        }
    }
    Ok(())
}

fn visit_carried_file<T>(
    source: &Path,
    relative: &Path,
    boundary: &NativeStateBoundary,
    visit: impl FnOnce(&mut File, &guard::ReadGuard<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let declared = boundary.source_root.path().join(relative);
    let access = ReadAccess {
        root: Some(&boundary.source_root),
        exception: Exception::Carried { root: &declared },
    };
    let lookup = source.join(relative);
    let Some(canonical) = canonical_source(&lookup, boundary, false, access)? else {
        return Ok(None);
    };
    if canonical != declared {
        return Ok(None);
    }
    let Some(mut file) = open_canonical_file(&canonical, access)? else {
        return Ok(None);
    };
    let mut guard = guard::ReadGuard::new(boundary, access)?;
    guard.record_route(&lookup, &canonical)?;
    if !guard.record_file(&canonical, &file)? {
        return Ok(None);
    }
    let result = visit(&mut file, &guard)?;
    guard.validate()?;
    Ok(Some(result))
}

fn bounded_carried_bytes(
    source: &Path,
    relative: &Path,
    boundary: &NativeStateBoundary,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    Ok(visit_carried_file(source, relative, boundary, |file, _| {
        let mut bytes = Vec::with_capacity(max.min(4096));
        file.take(max.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        Ok((bytes.len() <= max).then_some(bytes))
    })?
    .flatten())
}

fn copy_selected_file(
    source: &Path,
    destination: &Path,
    relative: &Path,
    boundary: &NativeStateBoundary,
    max: usize,
    matches: impl FnOnce(&[u8]) -> bool,
) -> Result<bool> {
    Ok(
        visit_carried_file(source, relative, boundary, |file, guard| {
            let mut bytes = Vec::with_capacity(max.min(4096));
            Read::by_ref(file)
                .take(max.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > max || !matches(&bytes) {
                return Ok(false);
            }
            file.seek(SeekFrom::Start(0))?;
            let output = AnchoredDir::open(destination)?;
            let parent = output.create_child(relative.parent().unwrap_or(Path::new("")))?;
            let leaf = Path::new(relative.file_name().context("selected file has no leaf")?);
            let permissions = file.metadata()?.permissions();
            publish_guarded_file(file, guard, &parent, leaf, permissions, true)
        })?
        .unwrap_or(false),
    )
}

fn unavailable_directory(error: &anyhow::Error) -> bool {
    let unavailable = |code| matches!(code, libc::ENOENT | libc::ENOTDIR | libc::ELOOP);
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .is_some_and(unavailable)
        || error
            .downcast_ref::<nix::errno::Errno>()
            .is_some_and(|errno| unavailable(*errno as i32))
}

fn bounded_directory_names(
    root: &AnchoredDir,
    source: &Path,
    relative: &Path,
    max: usize,
) -> Result<Option<Vec<std::ffi::OsString>>> {
    match fs::symlink_metadata(source.join(relative)) {
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_dir() => return Ok(None),
        Ok(_) => {}
    }
    match root.read_dir(relative, max.saturating_add(1)) {
        Ok(names) => Ok((names.len() <= max).then_some(names)),
        Err(error) if unavailable_directory(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn carry_gemini_session(
    root: &AnchoredDir,
    source: &Path,
    destination: &Path,
    boundary: &NativeStateBoundary,
    id: &str,
    cwd: &str,
) -> Result<bool> {
    let hash = capture::project_hash(cwd);
    let chats = Path::new("tmp").join(&hash).join("chats");
    let Some(names) =
        bounded_directory_names(root, source, &chats, capture::GEMINI_SCAN_MAX_CANDIDATES)?
    else {
        return Ok(false);
    };
    let mut selected = None;
    for name in names {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let relative = chats.join(&name);
        if !name_str.starts_with("session-")
            || !matches!(
                relative
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("json" | "jsonl")
            )
        {
            continue;
        }
        let Some(bytes) = root.read_regular(&relative, capture::GEMINI_SESSION_MAX_BYTES)? else {
            continue;
        };
        let fields = std::str::from_utf8(&bytes)
            .ok()
            .and_then(capture::parse_gemini_session_json);
        if fields.is_some_and(|(session, project)| {
            session.as_deref() == Some(id) && project.as_deref() == Some(hash.as_str())
        }) {
            if selected.is_some() {
                return Ok(false);
            }
            selected = Some(relative);
        }
    }
    let Some(relative) = selected else {
        return Ok(false);
    };
    copy_selected_file(
        source,
        destination,
        &relative,
        boundary,
        capture::GEMINI_SESSION_MAX_BYTES,
        |bytes| {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(capture::parse_gemini_session_json)
                .is_some_and(|(session, project)| {
                    session.as_deref() == Some(id) && project.as_deref() == Some(hash.as_str())
                })
        },
    )
}

fn prime_header(file: &mut File) -> Result<Option<(String, String)>> {
    let mut header = Vec::with_capacity(4096);
    std::io::BufReader::new(Read::by_ref(file))
        .take(capture::PRIME_AGENT_HEADER_SCAN_BYTES.saturating_add(1))
        .read_until(b'\n', &mut header)?;
    Ok(capture::root_session_header(&header))
}

fn carry_prime_session(
    root: &AnchoredDir,
    source: &Path,
    destination: &Path,
    boundary: &NativeStateBoundary,
    id: &str,
    cwd: &str,
) -> Result<bool> {
    let sessions = Path::new("sessions");
    let Some(names) = bounded_directory_names(
        root,
        source,
        sessions,
        capture::PRIME_AGENT_MAX_SESSION_FILES,
    )?
    else {
        return Ok(false);
    };
    let mut selected = None;
    for name in names {
        let relative = sessions.join(&name);
        if relative
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("jsonl")
        {
            continue;
        }
        let Some(mut file) = root.open_regular(&relative, usize::MAX)? else {
            continue;
        };
        if prime_header(&mut file)?
            .is_some_and(|(session, workdir)| session == id && workdir == cwd)
        {
            if selected.is_some() {
                return Ok(false);
            }
            selected = Some(relative);
        }
    }
    let Some(relative) = selected else {
        return Ok(false);
    };
    Ok(
        visit_carried_file(source, &relative, boundary, |file, guard| {
            if !prime_header(file)?
                .is_some_and(|(session, workdir)| session == id && workdir == cwd)
            {
                return Ok(false);
            }
            file.seek(SeekFrom::Start(0))?;
            let output = AnchoredDir::open(destination)?;
            let parent = output.create_child(sessions)?;
            let leaf = Path::new(
                relative
                    .file_name()
                    .context("Prime session has no file name")?,
            );
            let permissions = file.metadata()?.permissions();
            publish_guarded_file(file, guard, &parent, leaf, permissions, true)
        })?
        .unwrap_or(false),
    )
}

fn carry_kimi_session(
    root: &AnchoredDir,
    source: &Path,
    destination: &Path,
    boundary: &NativeStateBoundary,
    id: &str,
    cwd: &str,
    container_suffix: &str,
) -> Result<Option<serde_json::Value>> {
    let index = Path::new("session_index.jsonl");
    let Some(bytes) =
        bounded_carried_bytes(source, index, boundary, capture::KIMI_INDEX_MAX_BYTES)?
    else {
        return Ok(None);
    };
    let managed_sessions = Path::new("/root").join(container_suffix).join("sessions");
    let Ok(Some((leaf, mut record))) =
        capture::selected_index_record(&bytes, id, cwd, &managed_sessions)
    else {
        return Ok(None);
    };
    let relative = Path::new("sessions").join(&leaf);
    match root.child(&relative) {
        Ok(_) => {}
        Err(error) if unavailable_directory(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    match fs::canonicalize(source.join(&relative)) {
        Ok(canonical) if canonical == boundary.source_root.path().join(&relative) => {}
        Ok(_) => return Ok(None),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    }
    let pattern = format!("sessions/{leaf}");
    carry_sandbox_state(source, destination, &[pattern.as_str()], boundary)?;
    if !destination.join(&relative).is_dir() {
        return Ok(None);
    }
    if bounded_carried_bytes(source, index, boundary, capture::KIMI_INDEX_MAX_BYTES)?.as_deref()
        != Some(bytes.as_slice())
    {
        return Err(guard::Changed("Kimi session index changed during carry".into()).into());
    }
    record["sessionDir"] = format!("/root/{container_suffix}/sessions/{leaf}").into();
    Ok(Some(record))
}

pub(super) fn carry_selected_sandbox_state(
    source: &Path,
    destination: &Path,
    mount: &AgentConfigMount,
    resumes: &[crate::migrations::v033_isolate_sandbox_content::ResumeCandidate],
    cwd: &str,
    boundary: &NativeStateBoundary,
) -> Result<BTreeSet<String>> {
    let mut carried = BTreeSet::new();
    if boundary.stopped_original.is_none()
        || !resumes.iter().any(|resume| resume.agent == mount.tool_name)
    {
        return Ok(carried);
    }
    let root = AnchoredDir::open(source)?;
    let mut kimi_records = Vec::new();
    for resume in resumes
        .iter()
        .filter(|resume| resume.agent == mount.tool_name)
    {
        let succeeded = match mount.tool_name {
            "gemini" => {
                carry_gemini_session(&root, source, destination, boundary, &resume.id, cwd)?
            }
            "prime-agent" => {
                carry_prime_session(&root, source, destination, boundary, &resume.id, cwd)?
            }
            "kimi" => {
                if let Some(record) = carry_kimi_session(
                    &root,
                    source,
                    destination,
                    boundary,
                    &resume.id,
                    cwd,
                    mount.container_suffix,
                )? {
                    kimi_records.push((resume.tool.clone(), record));
                }
                false
            }
            _ => false,
        };
        if succeeded {
            carried.insert(resume.tool.clone());
        }
    }
    if !kimi_records.is_empty() {
        let mut index = Vec::new();
        for (_, record) in &kimi_records {
            serde_json::to_writer(&mut index, record)?;
            index.push(b'\n');
        }
        AnchoredDir::open(destination)?.publish_file(
            Path::new("session_index.jsonl"),
            &mut index.as_slice(),
            Permissions::from_mode(0o600),
            true,
            None,
        )?;
        carried.extend(kimi_records.into_iter().map(|(tool, _)| tool));
    }
    Ok(carried)
}

fn seed_directory(
    source: &Path,
    destination: &AnchoredDir,
    leaf: &Path,
    boundary: &NativeStateBoundary,
    discovery_links: bool,
    access: ReadAccess<'_>,
) -> Result<()> {
    if destination.regular_lookup(leaf)?.is_some() {
        return Ok(());
    }
    let Some(canonical) = canonical_source(source, boundary, true, access)? else {
        return Ok(());
    };
    let lookup = source;
    let source = match open_canonical_dir(&canonical) {
        Ok(source) => source,
        Err(error) if matches!(access.exception, Exception::Carried { .. }) => {
            return Err(error).context("opening carried native-state directory");
        }
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %canonical.display(), %error, "Skipping unreadable resource directory");
            return Ok(());
        }
    };
    let mut copy = ResourceCopy {
        guard: guard::ReadGuard::new(boundary, access)?,
        ancestors: HashSet::new(),
    };
    copy.guard.record_route(lookup, &canonical)?;
    copy.guard.record_directory(&source)?;
    let entries = match source.read_dir(Path::new(""), usize::MAX) {
        Ok(entries) => entries,
        Err(error) if matches!(access.exception, Exception::Carried { .. }) => {
            return Err(error).context("reading carried native-state directory");
        }
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %canonical.display(), %error, "Skipping unreadable resource directory");
            return Ok(());
        }
    };
    let private = &boundary.private_stage;
    let stage_name = PathBuf::from(format!(".aoe-resource-{}", uuid::Uuid::new_v4()));
    let stage = private.anchor.create_child(&stage_name)?;
    copy.ancestors.insert(source.identity()?);
    let result = copy
        .entries(&source, Path::new(""), &stage, entries, discovery_links)
        .and_then(|()| stage.sync())
        .and_then(|()| copy.guard.validate())
        .and_then(|()| {
            if private.anchor.child(&stage_name)?.identity()? != stage.identity()? {
                anyhow::bail!("private resource stage changed before publication");
            }
            destination.publish_directory(&private.anchor, &stage_name, leaf)
        });
    if !matches!(result, Ok(true)) {
        stage.remove_contents()?;
        let _ = fs::remove_dir(stage.path());
    }
    result.map(|_| ())
}

struct ResourceCopy<'a> {
    guard: guard::ReadGuard<'a>,
    ancestors: HashSet<(libc::dev_t, libc::ino_t)>,
}

impl ResourceCopy<'_> {
    fn entries(
        &mut self,
        source: &AnchoredDir,
        relative: &Path,
        destination: &AnchoredDir,
        entries: Vec<std::ffi::OsString>,
        discovery_links: bool,
    ) -> Result<()> {
        for name in entries {
            let input = relative.join(&name);
            let spelling = source.path().join(&input);
            let Some(canonical) =
                canonical_source(&spelling, self.guard.boundary, false, self.guard.access)?
            else {
                continue;
            };
            self.guard.record_route(&spelling, &canonical)?;
            let within = match canonical.strip_prefix(source.path()) {
                Ok(within) => within,
                Err(_) => {
                    if discovery_links && relative.as_os_str().is_empty() {
                        self.discovered_entry(&canonical, destination, Path::new(&name))?;
                    } else {
                        tracing::warn!(target: "session.profile", path = %spelling.display(), "Skipping resource link escaping its approved source root");
                    }
                    continue;
                }
            };
            match source.open_regular(within, usize::MAX) {
                Ok(Some(mut file)) => {
                    if self.guard.record_file(&canonical, &file)? {
                        let permissions = file.metadata()?.permissions();
                        destination.publish_file(
                            Path::new(&name),
                            &mut file,
                            permissions,
                            false,
                            None,
                        )?;
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) if matches!(self.guard.access.exception, Exception::Carried { .. }) => {
                    return Err(error).context("opening carried native-state file");
                }
                Err(error) => {
                    tracing::warn!(target: "session.profile", path = %spelling.display(), %error, "Skipping unreadable resource file");
                    continue;
                }
            }
            if self
                .guard
                .boundary
                .rejects(&canonical, true, self.guard.access)
            {
                continue;
            }
            let child = match source.child(within) {
                Ok(child) => child,
                Err(error)
                    if matches!(self.guard.access.exception, Exception::Carried { .. })
                        && !matches!(
                            error.downcast_ref::<nix::errno::Errno>(),
                            Some(
                                nix::errno::Errno::ENOENT
                                    | nix::errno::Errno::ELOOP
                                    | nix::errno::Errno::ENOTDIR
                            )
                        ) =>
                {
                    return Err(error).context("opening carried native-state directory");
                }
                Err(error) => {
                    tracing::warn!(target: "session.profile", path = %spelling.display(), %error, "Skipping unreadable resource entry");
                    continue;
                }
            };
            let identity = child.identity()?;
            if !self.ancestors.insert(identity) {
                continue;
            }
            self.guard.record_directory(&child)?;
            let children = match child.read_dir(Path::new(""), usize::MAX) {
                Ok(children) => children,
                Err(error) if matches!(self.guard.access.exception, Exception::Carried { .. }) => {
                    return Err(error).context("reading carried native-state subtree");
                }
                Err(error) => {
                    self.ancestors.remove(&identity);
                    tracing::warn!(target: "session.profile", path = %spelling.display(), %error, "Skipping unreadable resource subtree");
                    continue;
                }
            };
            let target = destination.create_child(Path::new(&name))?;
            self.entries(source, within, &target, children, false)?;
            publish_or_prune(&target, destination, Path::new(&name))?;
            self.ancestors.remove(&identity);
        }
        Ok(())
    }

    fn discovered_entry(
        &mut self,
        canonical: &Path,
        destination: &AnchoredDir,
        leaf: &Path,
    ) -> Result<()> {
        if let Some(mut file) = open_canonical_file(canonical, self.guard.access)? {
            if self.guard.record_file(canonical, &file)? {
                let permissions = file.metadata()?.permissions();
                destination.publish_file(leaf, &mut file, permissions, false, None)?;
            }
        } else if !self
            .guard
            .boundary
            .rejects(canonical, true, self.guard.access)
        {
            // A discovered link can point at anything the host holds; an entry
            // this walk cannot open is skipped like every other unreadable one
            // rather than failing the launch.
            let source = match open_canonical_dir(canonical) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                        "Skipping unreadable discovered resource directory");
                    return Ok(());
                }
            };
            let identity = match source.identity() {
                Ok(identity) => identity,
                Err(error) => {
                    tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                        "Skipping changed discovered resource directory");
                    return Ok(());
                }
            };
            if !self.ancestors.insert(identity) {
                return Ok(());
            }
            if let Err(error) = self.guard.record_directory(&source) {
                self.ancestors.remove(&identity);
                tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                    "Skipping changed discovered resource directory");
                return Ok(());
            }
            let entries = match source.read_dir(Path::new(""), usize::MAX) {
                Ok(entries) => entries,
                Err(error) => {
                    self.ancestors.remove(&identity);
                    tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                        "Skipping unreadable discovered resource directory");
                    return Ok(());
                }
            };
            let target = destination.create_child(leaf)?;
            self.entries(&source, Path::new(""), &target, entries, false)?;
            publish_or_prune(&target, destination, leaf)?;
            self.ancestors.remove(&identity);
        }
        Ok(())
    }
}

/// Publish a copied subtree, or drop it when nothing crossed. A directory
/// whose every entry was native state carries only a state name, and the
/// sandbox must not learn that name.
fn publish_or_prune(target: &AnchoredDir, destination: &AnchoredDir, leaf: &Path) -> Result<()> {
    if target.read_dir(Path::new(""), 1)?.is_empty() {
        destination.remove_staged_dir(leaf)?;
        return Ok(());
    }
    target.sync()
}

/// Both public credential paths remain unreadable until the complete pair's
/// directory is atomically published. A crash between link creation and that
/// publication can retry from a new complete source pair, never mix generations.
pub(super) fn seed_credential_pairs(
    source: &Path,
    destination: &Path,
    pairs: &[(&str, &str)],
    boundary: &NativeStateBoundary,
) -> Result<()> {
    let access = ReadAccess::default();
    if pairs.is_empty() {
        return Ok(());
    }
    let destination = AnchoredDir::open(destination)?;
    let units = destination.create_child(Path::new(".aoe-credential-pairs"))?;
    let private = &boundary.private_stage;
    for &(data_name, key_name) in pairs {
        let final_name = Path::new(data_name);
        if units.regular_lookup(final_name)?.is_some() {
            continue;
        }
        let data_link = PathBuf::from(".aoe-credential-pairs")
            .join(final_name)
            .join("data");
        let key_link = PathBuf::from(".aoe-credential-pairs")
            .join(final_name)
            .join("key");
        let paths = [
            (Path::new(data_name), &data_link),
            (Path::new(key_name), &key_link),
        ];
        let mut local = false;
        for (path, target) in paths {
            if destination.regular_lookup(path)?.is_some()
                && destination.read_link(path)?.as_ref() != Some(target)
            {
                local = true;
            }
        }
        if local {
            continue;
        }
        let Some(data_path) = canonical_source(
            &source.join(data_name),
            boundary,
            false,
            ReadAccess::default(),
        )?
        else {
            continue;
        };
        let Some(key_path) = canonical_source(
            &source.join(key_name),
            boundary,
            false,
            ReadAccess::default(),
        )?
        else {
            continue;
        };
        let Some(mut data) = open_canonical_file(&data_path, access)? else {
            continue;
        };
        let Some(mut key) = open_canonical_file(&key_path, access)? else {
            continue;
        };
        let mut guard = guard::ReadGuard::new(boundary, access)?;
        guard.record_route(&source.join(data_name), &data_path)?;
        guard.record_route(&source.join(key_name), &key_path)?;
        if !guard.record_file(&data_path, &data)? || !guard.record_file(&key_path, &key)? {
            continue;
        }
        let stage_name = PathBuf::from(format!(".pair-{}", uuid::Uuid::new_v4()));
        let stage = private.anchor.create_child(&stage_name)?;
        let result = (|| -> Result<bool> {
            stage.publish_file(
                Path::new("data"),
                &mut data,
                Permissions::from_mode(0o600),
                false,
                None,
            )?;
            stage.publish_file(
                Path::new("key"),
                &mut key,
                Permissions::from_mode(0o600),
                false,
                None,
            )?;
            stage.sync()?;
            guard.validate()?;
            for (path, target) in paths {
                destination.create_symlink(path, target)?;
            }
            for (path, target) in paths {
                if destination.read_link(path)?.as_ref() != Some(target) {
                    return Ok(false);
                }
            }
            if private.anchor.child(&stage_name)?.identity()? != stage.identity()? {
                anyhow::bail!("private credential stage changed before publication");
            }
            units.publish_directory(&private.anchor, &stage_name, final_name)
        })();
        if !matches!(result, Ok(true)) {
            stage.remove_contents()?;
            let _ = fs::remove_dir(stage.path());
        }
        result?;
    }
    Ok(())
}

pub(super) fn seed_sqlite_files(
    source: &Path,
    destination: &Path,
    files: &[&str],
    boundary: &NativeStateBoundary,
) -> Result<()> {
    let destination = AnchoredDir::open(destination)?;
    for &name in files {
        let relative = Path::new(name);
        let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(relative.file_name().context("SQLite seed has no leaf")?);
        if parent.regular_lookup(leaf)?.is_some() {
            continue;
        }
        if let Some(snapshot) =
            snapshot_config_database(&source.join(relative), boundary, ReadAccess::default())?
        {
            snapshot.publish(&parent, leaf)?;
        }
    }
    Ok(())
}

struct ConfigSnapshot<'a> {
    directory: AnchoredDir,
    guard: guard::ReadGuard<'a>,
}

impl ConfigSnapshot<'_> {
    fn publish(&self, destination: &AnchoredDir, leaf: &Path) -> Result<bool> {
        let mut file = self
            .directory
            .open_regular(Path::new("snapshot.db"), usize::MAX)?
            .context("SQLite did not produce a regular config snapshot")?;
        let private = &self.guard.boundary.private_stage;
        let validate = || self.guard.validate();
        destination.publish_file(
            leaf,
            &mut file,
            Permissions::from_mode(0o600),
            false,
            Some(crate::session::anchored_fs::FilePublication {
                staging: &private.anchor,
                validate: &validate,
            }),
        )
    }
}

fn snapshot_config_database<'a>(
    path: &Path,
    boundary: &'a NativeStateBoundary,
    access: ReadAccess<'a>,
) -> Result<Option<ConfigSnapshot<'a>>> {
    let Some(canonical) = canonical_source(path, boundary, false, access)? else {
        return Ok(None);
    };
    let private = &boundary.private_stage;
    let directory = private
        .anchor
        .create_child(Path::new(&format!(".sqlite-{}", uuid::Uuid::new_v4())))?;
    let mut guard = guard::ReadGuard::new(boundary, access)?;
    // Lease the namespace before inspecting optional sidecars, including absence.
    let source_parent =
        open_canonical_dir(canonical.parent().context("SQLite source has no parent")?)?;
    guard.pin_directory(&source_parent)?;
    let mut journal = canonical.as_os_str().to_os_string();
    journal.push("-journal");
    let journal = PathBuf::from(journal);
    match fs::symlink_metadata(&journal) {
        Ok(_) => {
            let input = canonical_source(&journal, boundary, false, access)?
                .context("native SQLite journal is not an admissible source")?;
            let mut file = open_canonical_file(&input, access)?
                .context("native SQLite journal is not a readable regular file")?;
            guard.record_route(&journal, &input)?;
            if !guard.record_file(&input, &file)? {
                anyhow::bail!("native SQLite journal overlaps native state");
            }
            // TRUNCATE leaves an empty journal; PERSIST clears its 28-byte header.
            if file.metadata()?.len() != 0 {
                let mut header = [0; 28];
                file.read_exact(&mut header)
                    .context("reading native SQLite journal header")?;
                if header != [0; 28] {
                    anyhow::bail!("native SQLite source has an active rollback journal; retry after the transaction finishes");
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspecting native SQLite journal"),
    }
    for suffix in ["", "-wal"] {
        let mut spelling = canonical.as_os_str().to_os_string();
        spelling.push(suffix);
        let spelling = PathBuf::from(spelling);
        match fs::symlink_metadata(&spelling) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {
                continue
            }
            Err(error) => return Err(error).context("inspecting native SQLite source"),
        }
        let Some(input) = canonical_source(&spelling, boundary, false, access)? else {
            return Ok(None);
        };
        let Some(mut file) = open_canonical_file(&input, access)? else {
            anyhow::bail!("native SQLite source is not a readable regular file");
        };
        guard.record_route(if suffix.is_empty() { path } else { &spelling }, &input)?;
        if !guard.record_file(&input, &file)? {
            return Ok(None);
        }
        directory.publish_file(
            Path::new(&format!("source.db{suffix}")),
            &mut file,
            Permissions::from_mode(0o600),
            false,
            None,
        )?;
    }
    // Never SQLite-open the original: even read-only WAL readers can update
    // SHM read marks. Rebuild that coordination state only in the private copy.
    guard.validate()?;
    let connection = rusqlite::Connection::open_with_flags(
        directory.path().join("source.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let snapshot = directory.path().join("snapshot.db");
    connection.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
    drop(connection);
    Ok(Some(ConfigSnapshot { directory, guard }))
}

pub(super) fn seed_configured_resources(
    mount: &AgentConfigMount,
    source: &Path,
    destination: &Path,
    home: &Path,
    workspace: &Path,
    boundary: &NativeStateBoundary,
) -> Result<()> {
    let discovery_links = boundary.stopped_original.is_none();
    let destination = AnchoredDir::open(destination)?;
    let resources = ResourceSeed {
        mount,
        source,
        destination: &destination,
        home,
        boundary,
    };
    match mount.tool_name {
        "pi" => {
            if let Some(settings) =
                read_document(&destination, Path::new("agent/settings.json"), false)?
            {
                let base = home.join(mount.container_suffix).join("agent");
                for kind in ["extensions", "skills", "prompts", "themes"] {
                    for entry in settings
                        .get(kind)
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let Some(path) = entry.as_str() else { continue };
                        // Native splitPatterns keeps these as enable/disable selectors.
                        // Preserved native config still applies them; they do not
                        // independently grant a new source directory.
                        if path.starts_with(['!', '+', '-']) || path.contains(['*', '?']) {
                            continue;
                        }
                        if let Some(relative) = resources.resolve(path, &base) {
                            resources.seed(&relative, true)?;
                        }
                    }
                }
                for package in settings
                    .get("packages")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(path) = package
                        .as_str()
                        .or_else(|| package.get("source").and_then(serde_json::Value::as_str))
                    else {
                        continue;
                    };
                    let trimmed = path.trim();
                    if path.starts_with("npm:")
                        || trimmed.starts_with("git:")
                        || ["http://", "https://", "ssh://", "git://"]
                            .iter()
                            .any(|prefix| trimmed.starts_with(prefix))
                    {
                        continue;
                    }
                    if let Some(relative) = resources.resolve(path, &base) {
                        resources.seed(&relative, true)?;
                    }
                }
            }
        }
        "omp" => {
            let mut settings =
                read_document(&destination, Path::new("agent/settings.json"), false)?
                    .unwrap_or_else(|| serde_json::json!({}));
            for name in ["agent/config.yml", "agent/config.yaml"] {
                if let Some(overrides) = read_document(&destination, Path::new(name), true)? {
                    crate::session::config::settings_schema::merge_json(&mut settings, &overrides);
                    break;
                }
            }
            for entry in settings
                .get("extensions")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    settings
                        .pointer("/skills/customDirectories")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            {
                let Some(path) = entry.as_str() else { continue };
                if let Some(relative) = resources.resolve(path, workspace) {
                    resources.seed(&relative, true)?;
                }
            }
            for name in ["agent/AGENTS.md", "agent/WATCHDOG.md"] {
                resources.follow_file_imports(Path::new(name), 0, &mut HashSet::new())?;
            }
            // These are native instruction fields, not a walk over every
            // path-looking string in advisor configuration.
            for name in ["agent/WATCHDOG.yml", "agent/WATCHDOG.yaml"] {
                if let Some(config) = read_document(&destination, Path::new(name), true)? {
                    if let Some(instructions) = config
                        .get("instructions")
                        .and_then(serde_json::Value::as_str)
                    {
                        resources.follow_imports(
                            instructions,
                            Path::new(name),
                            0,
                            &mut HashSet::from([PathBuf::from(name)]),
                        )?;
                    }
                    for advisor in config
                        .get("advisors")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if advisor
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .is_none()
                        {
                            continue;
                        }
                        if let Some(instructions) = advisor
                            .get("instructions")
                            .and_then(serde_json::Value::as_str)
                        {
                            resources.follow_imports(
                                instructions,
                                Path::new(name),
                                0,
                                &mut HashSet::from([PathBuf::from(name)]),
                            )?;
                        }
                    }
                }
            }
        }
        "gemini" | "qwen" => {
            if let Some(settings) = read_document(&destination, Path::new("settings.json"), false)?
            {
                if let Some(names) = settings.pointer("/context/fileName") {
                    let base = home.join(mount.container_suffix);
                    for name in names.as_str().into_iter().chain(
                        names
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str),
                    ) {
                        if let Some(relative) = resources.resolve(name, &base) {
                            resources.seed(&relative, false)?;
                        }
                    }
                }
            }
        }
        "hermes" => {
            // Native Hermes configuration lives beside conversation state, so
            // only the declared projections and authored collections cross.
            if let Some(scope) = boundary.hermes.source {
                let retained = hermes::seed_skills(
                    boundary,
                    scope,
                    &boundary.source_root,
                    &destination,
                    discovery_links,
                )?;
                hermes::seed_plugins(
                    boundary,
                    scope,
                    &boundary.source_root,
                    &destination,
                    discovery_links,
                )?;
                hermes::seed_nodes(boundary, scope, &boundary.source_root, &destination)?;
                hermes::seed_projects(boundary, scope, &boundary.source_root, &destination)?;
                hermes::seed_skill_controls(
                    boundary,
                    scope,
                    &boundary.source_root,
                    &destination,
                    &retained,
                )?;
            }
        }
        "claude" => {
            resources.follow_file_imports(Path::new("CLAUDE.md"), 0, &mut HashSet::new())?
        }
        _ => {}
    }
    Ok(())
}

/// A store document is rewritten by the container that mounts the store, so its
/// size is not trusted: a document past this bound is skipped, not read.
const MAX_STORE_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

fn read_document(
    root: &AnchoredDir,
    relative: &Path,
    yaml: bool,
) -> Result<Option<serde_json::Value>> {
    let Some(bytes) = root.read_regular(relative, MAX_STORE_DOCUMENT_BYTES)? else {
        return Ok(None);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        // Arbitrary bytes in a store document are a skip, like malformed
        // configuration, never a reason to refuse the launch.
        tracing::warn!(target: "session.profile", path = %root.path().join(relative).display(),
            "Cannot enumerate resources from an undecodable native configuration");
        return Ok(None);
    };
    let text = text.trim_start_matches('\u{feff}');
    let parsed = if yaml {
        serde_yaml::from_str::<serde_json::Value>(text).map_err(anyhow::Error::from)
    } else {
        serde_json::from_str(text).map_err(anyhow::Error::from)
    };
    match parsed {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            // Preserve the native file unchanged; malformed configuration is
            // not permission to guess additional source resources.
            tracing::warn!(target: "session.profile", path = %root.path().join(relative).display(), %error,
                "Cannot enumerate resources from malformed native configuration");
            Ok(None)
        }
    }
}

struct ResourceSeed<'a> {
    mount: &'a AgentConfigMount,
    source: &'a Path,
    destination: &'a AnchoredDir,
    home: &'a Path,
    boundary: &'a NativeStateBoundary,
}

impl ResourceSeed<'_> {
    fn resolve(&self, raw: &str, base: &Path) -> Option<PathBuf> {
        let resolved = if raw == "~" {
            self.home.to_path_buf()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            self.home.join(rest)
        } else if raw.starts_with("file://") {
            reqwest::Url::parse(raw).ok()?.to_file_path().ok()?
        } else {
            base.join(raw)
        };
        let resolved = lexical_normalize(&resolved);
        let host_native = self.home.join(self.mount.container_suffix);
        let container_native = Path::new("/root").join(self.mount.container_suffix);
        let relative = resolved
            .strip_prefix(&host_native)
            .or_else(|_| resolved.strip_prefix(&container_native))
            .or_else(|_| resolved.strip_prefix(self.source))
            .or_else(|_| resolved.strip_prefix(self.boundary.source_root.path()))
            .ok()?;
        let previously_supplied = if matches!(self.mount.tool_name, "pi" | "omp") {
            relative.starts_with("agent") && relative.components().count() > 1
        } else {
            relative.components().count() == 1
                || self
                    .mount
                    .copy_dirs
                    .iter()
                    .any(|directory| relative.starts_with(directory))
        };
        previously_supplied.then(|| relative.to_path_buf())
    }

    fn seed(&self, relative: &Path, directory_allowed: bool) -> Result<()> {
        let parent = self
            .destination
            .create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(
            relative
                .file_name()
                .context("configured resource has no leaf")?,
        );
        if parent.regular_lookup(leaf)?.is_some() {
            return Ok(());
        }
        let input = self.source.join(relative);
        if !publish_source_file(
            &input,
            &parent,
            leaf,
            self.boundary,
            false,
            ReadAccess::default(),
        )? && directory_allowed
        {
            seed_directory(
                &input,
                &parent,
                leaf,
                self.boundary,
                false,
                ReadAccess::default(),
            )?;
        }
        Ok(())
    }

    fn follow_file_imports(
        &self,
        relative: &Path,
        depth: usize,
        visited: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if depth >= 5 || !visited.insert(relative.to_path_buf()) {
            return Ok(());
        }
        let Some(bytes) = self
            .destination
            .read_regular(relative, MAX_STORE_DOCUMENT_BYTES)?
        else {
            return Ok(());
        };
        let Ok(content) = String::from_utf8(bytes) else {
            tracing::warn!(target: "session.profile", path = %self.destination.path().join(relative).display(),
                "Cannot follow imports from an undecodable native configuration");
            return Ok(());
        };
        self.follow_imports(&content, relative, depth, visited)
    }

    fn follow_imports(
        &self,
        content: &str,
        relative: &Path,
        depth: usize,
        visited: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if depth >= 5 {
            return Ok(());
        }
        static IMPORT: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"(^|[ \t])@([./~A-Za-z0-9_-][^\s]*)").expect("native import grammar")
        });
        let base = self
            .source
            .join(relative)
            .parent()
            .context("context source has no parent")?
            .to_path_buf();
        let mut fence: Option<(u8, usize)> = None;
        for line in content.lines() {
            let trimmed = line.trim_start_matches([' ', '\t']).as_bytes();
            let marker = trimmed
                .first()
                .copied()
                .filter(|c| matches!(c, b'`' | b'~'));
            let marks = marker
                .map(|c| trimmed.iter().take_while(|b| **b == c).count())
                .unwrap_or(0);
            if marks >= 3 {
                match fence {
                    None => fence = marker.map(|c| (c, marks)),
                    Some((c, count)) if Some(c) == marker && marks >= count => fence = None,
                    _ => {}
                }
                continue;
            }
            if fence.is_some() {
                continue;
            }
            for capture in IMPORT.captures_iter(line) {
                let token = capture.get(2).expect("import token");
                let position = token.start() - 1;
                let mut inline = false;
                let mut index = 0;
                let bytes = line.as_bytes();
                while index < position {
                    if bytes[index] == b'`' {
                        while index < position && bytes[index] == b'`' {
                            index += 1;
                        }
                        inline = !inline;
                    } else {
                        index += 1;
                    }
                }
                if inline {
                    continue;
                }
                let token = token
                    .as_str()
                    .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
                let Some(imported) = self.resolve(token, &base) else {
                    continue;
                };
                self.seed(&imported, false)?;
                self.follow_file_imports(&imported, depth + 1, visited)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_seed_refuses_uncommitted_spilled_pages_and_retries_after_rollback() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&active).unwrap();
        let database = source.join("config.db");
        let journal = source.join("config.db-journal");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
             PRAGMA page_size=1024;
             PRAGMA cache_size=2;
             PRAGMA cache_spill=ON;
             CREATE TABLE config (id INTEGER PRIMARY KEY, value TEXT, padding BLOB);
             WITH RECURSIVE rows(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM rows WHERE id<128)
             INSERT INTO config SELECT id, 'committed', zeroblob(900) FROM rows;",
            )
            .unwrap();
        let committed = fs::read(&database).unwrap();
        connection
            .execute_batch("BEGIN IMMEDIATE; UPDATE config SET value='uncommitted';")
            .unwrap();
        let database_before = fs::read(&database).unwrap();
        let journal_before = fs::read(&journal).unwrap();
        assert_ne!(
            database_before, committed,
            "the open transaction must spill pages to the main file"
        );
        assert!(journal_before.len() >= 28 && journal_before[..28].iter().any(|byte| *byte != 0));
        let boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let result = seed_sqlite_files(&source, &active, &["config.db"], &boundary);
        assert_eq!(fs::read(&database).unwrap(), database_before);
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
        assert!(
            result.is_err(),
            "an active rollback journal must prevent a snapshot: {result:?}"
        );
        assert!(!active.join("config.db").exists());

        connection.execute_batch("ROLLBACK;").unwrap();
        let boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        seed_sqlite_files(&source, &active, &["config.db"], &boundary).unwrap();
        let snapshot = rusqlite::Connection::open(active.join("config.db")).unwrap();
        let rows: (i64, i64) = snapshot
            .query_row(
                "SELECT COUNT(*), SUM(value='committed') FROM config",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, (128, 128));
    }

    #[test]
    fn sqlite_seed_accepts_inactive_rollback_journals_without_changing_source() {
        for mode in ["PERSIST", "TRUNCATE"] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&active).unwrap();
            let database = source.join("config.db");
            let journal = source.join("config.db-journal");
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute_batch(&format!(
                    "PRAGMA journal_mode={mode};
                 CREATE TABLE config (value TEXT);
                 INSERT INTO config VALUES ('committed');"
                ))
                .unwrap();
            let database_before = fs::read(&database).unwrap();
            let journal_before = fs::read(&journal).unwrap();
            if mode == "PERSIST" {
                assert!(journal_before.len() >= 28);
                assert_eq!(&journal_before[..28], &[0; 28]);
            } else {
                assert!(journal_before.is_empty());
            }
            let boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            seed_sqlite_files(&source, &active, &["config.db"], &boundary).unwrap();
            assert_eq!(fs::read(&database).unwrap(), database_before, "{mode}");
            assert_eq!(fs::read(&journal).unwrap(), journal_before, "{mode}");
            let snapshot = rusqlite::Connection::open(active.join("config.db")).unwrap();
            let value: String = snapshot
                .query_row("SELECT value FROM config", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, "committed", "{mode}");
        }
    }

    #[test]
    fn carried_entries_cannot_redirect_their_declared_root_to_internal_history() {
        // (history dir, history file, declared link, link target, boundary path, pattern)
        for (dir, history, link, target, internal, pattern) in [
            (
                "history-tree/proj",
                "history-tree/proj/session.jsonl",
                "projects",
                "history-tree",
                "history-tree",
                "projects",
            ),
            (
                "",
                "history.db",
                "opencode.db",
                "history.db",
                "history.db",
                "opencode.db*",
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            fs::create_dir_all(source.join(dir)).unwrap();
            fs::create_dir(&active).unwrap();
            let history = source.join(history);
            fs::write(&history, b"OTHER_NATIVE_HISTORY").unwrap();
            std::os::unix::fs::symlink(target, source.join(link)).unwrap();
            let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            boundary.stopped_original = Some(boundary.source_root.path().to_path_buf());
            boundary.add_path(source.join(internal));
            let result = carry_sandbox_state(&source, &active, &[pattern], &boundary);
            assert!(result.is_err(), "a redirected {link} must fail: {result:?}");
            assert!(!active.join(link).exists(), "{link}");
            assert_eq!(fs::read(&history).unwrap(), b"OTHER_NATIVE_HISTORY");
            assert_eq!(fs::read_link(source.join(link)).unwrap(), Path::new(target));
        }
    }

    #[test]
    fn carried_state_accepts_a_source_beneath_a_symlinked_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("parent");
        let alias = temporary.path().join("alias");
        fs::create_dir_all(parent.join("source/projects/proj")).unwrap();
        std::os::unix::fs::symlink(&parent, &alias).unwrap();
        let source = alias.join("source");
        let active = temporary.path().join("active");
        fs::create_dir(&active).unwrap();
        let history = source.join("projects/proj/session.jsonl");
        fs::write(&history, b"OWN_NATIVE_HISTORY").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        boundary.stopped_original = Some(boundary.source_root.path().to_path_buf());
        boundary.add_path(source.join("projects"));
        carry_sandbox_state(&source, &active, &["projects"], &boundary).unwrap();
        assert_eq!(
            fs::read(active.join("projects/proj/session.jsonl")).unwrap(),
            b"OWN_NATIVE_HISTORY"
        );
        assert_eq!(fs::read(&history).unwrap(), b"OWN_NATIVE_HISTORY");
    }

    #[test]
    fn a_looped_carried_entry_is_not_carried() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&active).unwrap();
        fs::write(source.join("opencode.db"), b"OWN_NATIVE_HISTORY").unwrap();
        std::os::unix::fs::symlink("opencode.db-wal", source.join("opencode.db-wal")).unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        boundary.stopped_original = Some(boundary.source_root.path().to_path_buf());
        carry_sandbox_state(&source, &active, &["opencode.db*"], &boundary).unwrap();
        assert_eq!(
            fs::read(active.join("opencode.db")).unwrap(),
            b"OWN_NATIVE_HISTORY"
        );
        assert!(fs::symlink_metadata(active.join("opencode.db-wal")).is_err());
    }

    #[test]
    fn carried_hardlinks_keep_outside_directory_alias_witnesses() {
        for outward in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let active = temporary.path().join("active");
            fs::create_dir_all(source.join("projects/proj")).unwrap();
            fs::create_dir(&active).unwrap();
            let candidate = source.join("projects/proj/session.jsonl");
            fs::write(&candidate, b"OUTSIDE_STATE").unwrap();
            if outward {
                fs::create_dir(source.join("history-tree")).unwrap();
                fs::hard_link(&candidate, source.join("history-tree/session.jsonl")).unwrap();
                std::os::unix::fs::symlink("../history-tree", source.join("projects/leak"))
                    .unwrap();
            } else {
                fs::hard_link(&candidate, source.join("projects/proj/backup.jsonl")).unwrap();
                std::os::unix::fs::symlink("projects/proj", source.join("history-tree")).unwrap();
            }
            let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
            boundary.stopped_original = Some(boundary.source_root.path().to_path_buf());
            // The outward fixture discovers history only through the carried alias.
            boundary.add_path(if outward {
                source.join("projects")
            } else {
                source.clone()
            });
            carry_sandbox_state(&source, &active, &["projects"], &boundary).unwrap();
            assert!(
                !active.join("projects/proj/session.jsonl").exists(),
                "outward={outward}"
            );
            assert!(
                !active.join("projects/proj/backup.jsonl").exists(),
                "outward={outward}"
            );
            assert!(!active.join("projects/leak").exists(), "outward={outward}");
            assert_eq!(fs::read(&candidate).unwrap(), b"OUTSIDE_STATE");
        }
    }

    #[test]
    fn native_state_hardlinks_are_not_configuration_but_authored_hardlinks_are() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        let hermes = temporary.path().join(".hermes");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(hermes.join("sessions")).unwrap();
        let history = hermes.join("sessions/foreign.jsonl");
        fs::write(&history, b"FOREIGN_NATIVE_CONTEXT").unwrap();
        fs::hard_link(&history, source.join("auth.json")).unwrap();
        let authored = temporary.path().join("authored-settings");
        fs::write(&authored, b"AUTHORED_SETTINGS").unwrap();
        fs::hard_link(&authored, source.join("settings.json")).unwrap();
        fs::write(active.join("auth.json"), b"LOCAL_AUTH").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        boundary.add_root(&hermes, mount).unwrap();
        sync_agent_config(
            &source,
            &active,
            &["auth.json", "settings.json"],
            &[],
            &[],
            &[],
            &boundary,
        )
        .unwrap();
        assert_eq!(fs::read(active.join("auth.json")).unwrap(), b"LOCAL_AUTH");
        assert_eq!(
            fs::read(active.join("settings.json")).unwrap(),
            b"AUTHORED_SETTINGS"
        );
        assert_eq!(fs::read(history).unwrap(), b"FOREIGN_NATIVE_CONTEXT");
    }
    #[test]
    fn a_dangling_native_directory_alias_cannot_export_state_names() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        let native = temporary.path().join("native-home");
        fs::create_dir_all(source.join("plugins")).unwrap();
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&native).unwrap();
        std::os::unix::fs::symlink(source.join("plugins/native-state"), native.join("sessions"))
            .unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        boundary.add_root(&native, mount).unwrap();
        fs::create_dir_all(source.join("plugins/native-state/PRIVATE_SESSION_TITLE")).unwrap();
        fs::write(
            source.join("plugins/native-state/PRIVATE_SESSION_TITLE/transcript.jsonl"),
            b"PRIVATE_NATIVE_CONTEXT",
        )
        .unwrap();
        // An authored file inside the same directory: the walk refuses the
        // whole directory when it overlaps native state, so this file must not
        // cross either. Without it the absence below is also true when the walk
        // simply had nothing to publish.
        fs::write(source.join("plugins/keep.json"), b"AUTHORED_PLUGIN").unwrap();
        seed_directory(
            &source.join("plugins"),
            &AnchoredDir::open(&active).unwrap(),
            Path::new("plugins"),
            &boundary,
            true,
            ReadAccess::default(),
        )
        .unwrap();
        assert!(
            !active.join("plugins").exists(),
            "a resource overlapping a native state directory must not export even its names"
        );
        assert!(
            !active.join("plugins/keep.json").exists(),
            "not even an authored file in that directory crosses"
        );
    }

    #[test]
    fn an_original_does_not_lend_a_link_to_the_host() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let outside = temporary.path().join("outside");
        let active = temporary.path().join("active");
        for path in [&source, &outside, &active] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(outside.join("secret"), b"HOST_BYTES").unwrap();
        fs::create_dir_all(source.join("plugins")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), source.join("plugins/leak.json"))
            .unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "claude")
            .unwrap();
        let host = temporary.path().join("host");
        fs::create_dir_all(&host).unwrap();
        let boundary = NativeStateBoundary::for_fixture(&source, &active, mount)
            .unwrap()
            .for_stopped_original(&host, mount)
            .unwrap();
        sync_agent_config(&source, &active, &[], &[], &["plugins"], &[], &boundary).unwrap();
        assert!(
            !active.join("plugins/leak.json").exists(),
            "a link a container could leave in its own store must not reach the host"
        );

        // The host's own directory is the user's, and following their link is
        // what they asked for.
        let host_active = temporary.path().join("host-active");
        fs::create_dir_all(&host_active).unwrap();
        let host_boundary = NativeStateBoundary::for_fixture(&source, &host_active, mount).unwrap();
        sync_agent_config(
            &source,
            &host_active,
            &[],
            &[],
            &["plugins"],
            &[],
            &host_boundary,
        )
        .unwrap();
        assert_eq!(
            fs::read(host_active.join("plugins/leak.json")).unwrap(),
            b"HOST_BYTES"
        );
    }

    #[test]
    fn an_original_lends_no_link_out_of_itself() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let outside = temporary.path().join("outside");
        let active = temporary.path().join("active");
        let host = temporary.path().join("host");
        for path in [&source, &outside, &active, &host] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(outside.join("secret"), b"HOST_BYTES").unwrap();
        fs::create_dir_all(outside.join("tree/meetings")).unwrap();
        fs::write(outside.join("tree/inside"), b"HOST_TREE_BYTES").unwrap();
        fs::write(
            outside.join("tree/meetings/nodes.json"),
            b"HOST_NODES_BYTES",
        )
        .unwrap();
        fs::create_dir_all(source.join("agent")).unwrap();
        // A declared config file, a declared directory, and the projection
        // Hermes reads out of its own store, each pointing at the host.
        std::os::unix::fs::symlink(outside.join("secret"), source.join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(outside.join("tree"), source.join("plugins")).unwrap();
        std::os::unix::fs::symlink(outside.join("tree"), source.join("workspace")).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "claude")
            .unwrap();
        let boundary = NativeStateBoundary::for_fixture(&source, &active, mount)
            .unwrap()
            .for_stopped_original(&host, mount)
            .unwrap();
        sync_agent_config(
            &source,
            &active,
            &["CLAUDE.md"],
            &[],
            &["plugins"],
            &[],
            &boundary,
        )
        .unwrap();
        assert!(
            !active.join("CLAUDE.md").exists(),
            "a declared file that leaves the store must not be published"
        );
        assert!(
            !active.join("plugins").exists(),
            "a declared directory that leaves the store must not be walked"
        );

        let hermes = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        let hermes_active = temporary.path().join("hermes-active");
        fs::create_dir_all(&hermes_active).unwrap();
        let hermes_boundary = NativeStateBoundary::for_fixture(&source, &hermes_active, hermes)
            .unwrap()
            .for_stopped_original(&host, hermes)
            .unwrap();
        let scope = hermes_boundary.hermes.source.unwrap();
        hermes::seed_nodes(
            &hermes_boundary,
            scope,
            &hermes_boundary.source_root,
            &AnchoredDir::open(&hermes_active).unwrap(),
        )
        .unwrap();
        assert!(
            !hermes_active.join("workspace").exists(),
            "a projection that leaves the store must not be published"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_descendant_state_alias_change_blocks_publication() {
        use std::os::unix::fs::MetadataExt;

        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        let external = temporary.path().join("external");
        let active = temporary.path().join("active");
        for path in [&source, &external, &active] {
            fs::create_dir(path).unwrap();
        }
        fs::create_dir_all(native.join("sessions")).unwrap();
        let input = source.join("auth.json");
        fs::write(&input, b"BECOMES_NATIVE_HISTORY").unwrap();
        std::os::unix::fs::symlink(&input, external.join("candidate")).unwrap();
        assert_eq!(fs::metadata(&input).unwrap().nlink(), 1);
        fs::write(external.join("benign"), b"other state").unwrap();
        std::os::unix::fs::symlink("benign", external.join("alias")).unwrap();
        std::os::unix::fs::symlink(external.join("alias"), native.join("sessions/ref")).unwrap();
        fs::write(active.join("auth.json"), b"LOCAL_AUTH").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        boundary.add_root(&native, mount).unwrap();
        let mut guard = guard::ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
        let mut file = open_canonical_file(&input, ReadAccess::default())
            .unwrap()
            .unwrap();
        assert!(guard.record_file(&input, &file).unwrap());
        fs::remove_file(external.join("alias")).unwrap();
        std::os::unix::fs::symlink("candidate", external.join("alias")).unwrap();
        let result = publish_guarded_file(
            &mut file,
            &guard,
            &AnchoredDir::open(&active).unwrap(),
            Path::new("auth.json"),
            Permissions::from_mode(0o600),
            true,
        );
        assert!(
            result.is_err(),
            "a newly forbidden inode must not be published through the cached inventory"
        );
        assert_eq!(fs::read(active.join("auth.json")).unwrap(), b"LOCAL_AUTH");
        assert_eq!(fs::read(&input).unwrap(), b"BECOMES_NATIVE_HISTORY");
        assert_eq!(fs::read_link(external.join("candidate")).unwrap(), input);
    }

    #[test]
    fn a_deduplicated_declared_home_alias_keeps_its_epoch() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        let other = temporary.path().join("other");
        let alias = temporary.path().join("declared");
        let active = temporary.path().join("active");
        for path in [&source, &native, &active] {
            fs::create_dir(path).unwrap();
        }
        fs::create_dir_all(other.join("sessions")).unwrap();
        let input = source.join("auth.json");
        fs::write(&input, b"BECOMES_DECLARED_NATIVE_HISTORY").unwrap();
        fs::hard_link(&input, other.join("sessions/foreign")).unwrap();
        std::os::unix::fs::symlink(&native, &alias).unwrap();
        fs::write(active.join("auth.json"), b"LOCAL_AUTH").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        boundary.add_root(&native, mount).unwrap();
        boundary.add_root(&alias, mount).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        let result = publish_source_file(
            &input,
            &AnchoredDir::open(&active).unwrap(),
            Path::new("auth.json"),
            &boundary,
            true,
            ReadAccess::default(),
        );
        assert!(
            result.is_err(),
            "deduplicated native scope rules must not discard the declared route"
        );
        assert_eq!(fs::read(active.join("auth.json")).unwrap(), b"LOCAL_AUTH");
    }

    #[test]
    #[serial_test::serial]
    fn a_status_alias_declaring_the_active_source_fences_every_mount_of_its_agent() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = temporary.path().join("home");
        let source = home.join("opencode-work");
        let active = temporary.path().join("active");
        fs::create_dir_all(source.join("storage")).unwrap();
        fs::create_dir_all(&active).unwrap();
        fs::write(source.join("opencode.db"), b"HISTORY").unwrap();
        fs::write(source.join("opencode.json"), b"{}").unwrap();
        let mut config = crate::session::config::SessionConfig::default();
        config
            .agent_detect_as
            .insert("oc-work".into(), "opencode".into());
        config
            .agent_config_dir
            .insert("oc-work".into(), source.display().to_string());
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "opencode" && mount.host_rel == ".config/opencode")
            .unwrap();
        let boundary = NativeStateBoundary::new(&source, mount, &home, &config, &active).unwrap();
        let canonical = source.canonicalize().unwrap();
        for (native, directory) in [("opencode.db", false), ("storage", true)] {
            assert!(
                boundary.rejects(&canonical.join(native), directory, ReadAccess::default()),
                "{native} is the data mount's native state"
            );
        }
        assert!(!boundary.rejects(
            &canonical.join("opencode.json"),
            false,
            ReadAccess::default()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn a_status_alias_cannot_export_another_profiles_native_history() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let home = temporary.path().join("home");
        let source = home.join(".claude");
        let active = temporary.path().join("active");
        let codex = home.join("private-codex");
        let wrapped = home.join("private-wrapped-codex");
        let unverified = home.join("private-remote-codex");
        let host_only = home.join("private-settl");
        for path in [&source, &active] {
            fs::create_dir_all(path).unwrap();
        }
        for (resource, root) in [
            ("skills", &codex),
            ("hooks", &wrapped),
            ("plugins", &unverified),
        ] {
            fs::create_dir_all(root.join("sessions")).unwrap();
            fs::write(root.join("sessions/other.jsonl"), b"OTHER_PROFILE_HISTORY").unwrap();
            symlink(root.join("sessions"), source.join(resource)).unwrap();
        }
        fs::create_dir_all(host_only.join("sessions")).unwrap();
        fs::write(host_only.join("sessions/other.jsonl"), b"HOST_ONLY_HISTORY").unwrap();
        symlink(
            host_only.join("sessions/other.jsonl"),
            source.join("settings.json"),
        )
        .unwrap();
        fs::write(source.join("CLAUDE.md"), b"AUTHORED_RESOURCE").unwrap();
        let profile = crate::session::get_profile_dir("native-owner").unwrap();
        fs::write(
            profile.join("config.toml"),
            format!(
                "[session]\nagent_detect_as = {{ codex = \"claude\", \"wrapped-codex\" = \"claude\", \"remote-codex\" = \"claude\" }}\nagent_execution_as = {{ \"wrapped-codex\" = \"codex\" }}\ncustom_agents = {{ \"wrapped-codex\" = \"wrapper --serve\", \"remote-codex\" = \"ssh remote codex\" }}\nagent_config_dir = {{ codex = {:?}, \"wrapped-codex\" = {:?}, \"remote-codex\" = {:?}, settl = {:?} }}\n",
                codex.display().to_string(),
                wrapped.display().to_string(),
                unverified.display().to_string(),
                host_only.display().to_string(),
            ),
        )
        .unwrap();

        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "claude")
            .unwrap();
        let boundary = NativeStateBoundary::new(
            &source,
            mount,
            &home,
            &crate::session::config::SessionConfig::default(),
            &active,
        )
        .unwrap();
        let destination = AnchoredDir::open(&active).unwrap();
        for resource in ["skills", "hooks", "plugins"] {
            seed_directory(
                &source.join(resource),
                &destination,
                Path::new(resource),
                &boundary,
                false,
                ReadAccess::default(),
            )
            .unwrap();
            assert!(
                !active.join(resource).exists(),
                "{resource} exported another profile's native conversation"
            );
        }
        assert!(!publish_source_file(
            &source.join("settings.json"),
            &destination,
            Path::new("settings.json"),
            &boundary,
            false,
            ReadAccess::default(),
        )
        .unwrap());
        assert!(!active.join("settings.json").exists());
        assert!(publish_source_file(
            &source.join("CLAUDE.md"),
            &destination,
            Path::new("CLAUDE.md"),
            &boundary,
            false,
            ReadAccess::default(),
        )
        .unwrap());
        assert_eq!(
            fs::read(active.join("CLAUDE.md")).unwrap(),
            b"AUTHORED_RESOURCE"
        );
    }

    #[test]
    fn a_stopped_original_does_not_exempt_its_nested_private_stores() {
        let temporary = tempfile::tempdir().unwrap();
        let host = temporary.path().join("host");
        let original = temporary
            .path()
            .join(".aoe-sandbox-recovery/receipt/original");
        let active = temporary.path().join("active");
        fs::create_dir(&host).unwrap();
        fs::create_dir(&active).unwrap();
        let nested = original.join(SANDBOX_PRIVATE_SUBDIR).join("other");
        fs::create_dir_all(nested.join("sessions")).unwrap();
        let input = original.join("auth.json");
        fs::write(&input, b"OTHER_INSTANCE_CONTEXT").unwrap();
        fs::hard_link(&input, nested.join("sessions/foreign.json")).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        let boundary = NativeStateBoundary::for_fixture(&original, &active, mount)
            .unwrap()
            .for_stopped_original(&host, mount)
            .unwrap();
        let published = publish_source_file(
            &input,
            &AnchoredDir::open(&active).unwrap(),
            Path::new("auth.json"),
            &boundary,
            false,
            ReadAccess::default(),
        )
        .unwrap();
        assert!(!published, "waiving the retained original's storage ancestor must not waive nested other-instance stores");
        assert!(!active.join("auth.json").exists());
    }

    #[test]
    fn directory_publication_filters_state_hardlinks_under_literal_root_names() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        let native = temporary.path().join("native[fixture]");
        fs::create_dir_all(source.join("plugins/package")).unwrap();
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("state.db-wal"), b"FOREIGN_NATIVE_CONTEXT").unwrap();
        fs::hard_link(
            native.join("state.db-wal"),
            source.join("plugins/package/data.json"),
        )
        .unwrap();
        let authored = temporary.path().join("authored-code");
        fs::write(&authored, b"export const authored = true;").unwrap();
        fs::hard_link(authored, source.join("plugins/package/index.js")).unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        boundary.add_root(&native, mount).unwrap();
        seed_directory(
            &source.join("plugins"),
            &AnchoredDir::open(&active).unwrap(),
            Path::new("plugins"),
            &boundary,
            true,
            ReadAccess::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(active.join("plugins/package/index.js")).unwrap(),
            b"export const authored = true;"
        );
        assert!(!active.join("plugins/package/data.json").exists());
        assert_eq!(
            fs::read(native.join("state.db-wal")).unwrap(),
            b"FOREIGN_NATIVE_CONTEXT"
        );
    }

    #[test]
    fn native_directory_churn_preserves_config_but_new_alias_blocks_publication() {
        use std::io::Seek;
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        let native = source.join("projects");
        fs::create_dir_all(&native).unwrap();
        fs::create_dir_all(&active).unwrap();
        let input = source.join("settings.json");
        fs::write(&input, b"APPROVED_CONFIGURATION").unwrap();
        let mut boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        boundary.add_classified_path(native.clone(), StateOrigin::Native);
        let mut guard = guard::ReadGuard::new(&boundary, ReadAccess::default()).unwrap();
        let mut file = open_canonical_file(&input, ReadAccess::default())
            .unwrap()
            .unwrap();
        assert!(guard.record_file(&input, &file).unwrap());
        let output = AnchoredDir::open(&active).unwrap();
        fs::create_dir(native.join("new-project")).unwrap();
        fs::write(
            native.join("new-project/history.jsonl"),
            b"UNRELATED_HISTORY",
        )
        .unwrap();
        let validate = || guard.validate();
        output
            .publish_file(
                Path::new("settings.json"),
                &mut file,
                Permissions::from_mode(0o600),
                true,
                Some(crate::session::anchored_fs::FilePublication {
                    staging: &boundary.private_stage.anchor,
                    validate: &validate,
                }),
            )
            .unwrap();
        assert_eq!(
            fs::read(active.join("settings.json")).unwrap(),
            b"APPROVED_CONFIGURATION"
        );
        fs::write(active.join("settings.json"), b"LOCAL_CONFIGURATION").unwrap();
        std::os::unix::fs::symlink(&input, native.join("new-project/alias")).unwrap();
        file.rewind().unwrap();
        assert!(output
            .publish_file(
                Path::new("settings.json"),
                &mut file,
                Permissions::from_mode(0o600),
                true,
                Some(crate::session::anchored_fs::FilePublication {
                    staging: &boundary.private_stage.anchor,
                    validate: &validate
                })
            )
            .is_err());
        assert_eq!(
            fs::read(active.join("settings.json")).unwrap(),
            b"LOCAL_CONFIGURATION"
        );
    }

    #[test]
    fn sqlite_seed_reads_committed_wal_without_mutating_originals_or_local_state() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let active = temporary.path().join("active");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&active).unwrap();
        let original = rusqlite::Connection::open(source.join("agent.db")).unwrap();
        original.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE configuration(value TEXT); INSERT INTO configuration VALUES ('INITIAL_CONFIGURATION');").unwrap();
        let before: std::collections::BTreeMap<_, _> = fs::read_dir(&source)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        let boundary = NativeStateBoundary::for_source(&source, &active).unwrap();
        seed_sqlite_files(&source, &active, &["agent.db"], &boundary).unwrap();
        let after: std::collections::BTreeMap<_, _> = fs::read_dir(&source)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        assert_eq!(after, before);
        let local = rusqlite::Connection::open(active.join("agent.db")).unwrap();
        let initial: String = local
            .query_row("SELECT value FROM configuration", [], |row| row.get(0))
            .unwrap();
        assert_eq!(initial, "INITIAL_CONFIGURATION");
        local
            .execute(
                "INSERT INTO configuration VALUES ('LOCAL_CONFIGURATION')",
                [],
            )
            .unwrap();
        original
            .execute(
                "INSERT INTO configuration VALUES ('LATER_HOST_CONFIGURATION')",
                [],
            )
            .unwrap();
        seed_sqlite_files(&source, &active, &["agent.db"], &boundary).unwrap();
        let values: Vec<String> = local
            .prepare("SELECT value FROM configuration ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(values, ["INITIAL_CONFIGURATION", "LOCAL_CONFIGURATION"]);
    }
}
