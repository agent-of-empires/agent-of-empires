//! Positive native-config seeding, separated from container mount construction.

use std::collections::HashSet;
use std::fs::{self, File, Permissions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};

use crate::git::template::lexical_normalize;
use crate::session::anchored_fs::AnchoredDir;
use crate::session::config::SessionConfig;

use super::{AgentConfigMount, AGENT_CONFIG_MOUNTS, SANDBOX_PRIVATE_SUBDIR, SANDBOX_SUBDIR};

#[derive(Default)]
pub(super) struct NativeStateBoundary {
    source_root: PathBuf,
    stopped_original: Option<PathBuf>,
    paths: Vec<PathBuf>,
    patterns: Vec<(PathBuf, glob::Pattern)>,
}

impl NativeStateBoundary {
    pub(super) fn new(
        source: &Path,
        mount: &AgentConfigMount,
        home: &Path,
        config: &SessionConfig,
    ) -> Result<Self> {
        let mut boundary = Self {
            source_root: fs::canonicalize(source)?,
            ..Self::default()
        };
        for registered in AGENT_CONFIG_MOUNTS {
            boundary.add_root(&home.join(registered.host_rel), registered)?;
        }
        boundary.add_declared_roots(config, home)?;
        for profile in crate::session::list_profiles()? {
            let registered = crate::session::config::profile_config::resolve_config(&profile)?;
            boundary.add_declared_roots(&registered.session, home)?;
        }
        // Declared config roots retain the same native-state exclusions as
        // defaults; a declaration is not provenance for an existing history.
        boundary.add_root(source, mount)?;
        // Kiro's mixed native database is outside its existing .kiro mount.
        boundary.add_path(home.join(".local/share/kiro-cli"));
        if let Ok(app_dir) = crate::session::get_app_dir() {
            boundary.add_path(app_dir);
        }
        boundary.paths.sort();
        boundary.paths.dedup();
        Ok(boundary)
    }

    pub(super) fn for_stopped_original(mut self, host: &Path) -> Self {
        // The old copier could have placed every configured role here. Carry
        // their known host-state boundaries into the private original too,
        // including roles declared by another profile.
        let mapped_paths: Vec<_> = self
            .paths
            .iter()
            .filter_map(|path| {
                path.strip_prefix(host)
                    .ok()
                    .map(|relative| self.source_root.join(relative))
            })
            .collect();
        let mapped_patterns: Vec<_> = self
            .patterns
            .iter()
            .filter_map(|(root, pattern)| {
                root.strip_prefix(host)
                    .ok()
                    .map(|relative| (self.source_root.join(relative), pattern.clone()))
            })
            .collect();
        for path in mapped_paths {
            self.add_path(path);
        }
        self.patterns.extend(mapped_patterns);
        self.stopped_original = Some(self.source_root.clone());
        self
    }
    fn add_declared_roots(&mut self, config: &SessionConfig, home: &Path) -> Result<()> {
        for tool in config.agent_config_dir.keys() {
            let Some(root) = config.agent_config_dir_for(tool, home) else {
                continue;
            };
            let detect_as = config.agent_detect_as.get(tool).map(String::as_str);
            let Some(agent) = super::resolve_active_agent(tool, detect_as, config) else {
                continue;
            };
            for mount in AGENT_CONFIG_MOUNTS
                .iter()
                .filter(|mount| mount.tool_name == agent.name)
            {
                self.add_root(&root, mount)?;
            }
        }
        Ok(())
    }
    fn add_root(&mut self, root: &Path, mount: &AgentConfigMount) -> Result<()> {
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| lexical_normalize(root));
        if let Some(parent) = canonical_root.parent() {
            self.add_path(parent.join(crate::migrations::v030_isolate_sandbox_content::RECOVERY));
        }
        for name in [SANDBOX_SUBDIR, SANDBOX_PRIVATE_SUBDIR]
            .into_iter()
            .chain(mount.native_state_paths.iter().copied())
        {
            if name.contains(['*', '?', '[']) {
                self.patterns
                    .push((canonical_root.clone(), glob::Pattern::new(name)?));
                let spelling = root.join(name);
                for entry in glob::glob(&spelling.to_string_lossy())? {
                    match entry {
                        Ok(path) => self.add_path(path),
                        Err(error) => tracing::warn!(target: "session.profile", %error,
                            "Cannot inspect a native-state alias while seeding config"),
                    }
                }
            } else {
                self.add_path(root.join(name));
                self.add_path(canonical_root.join(name));
            }
        }
        Ok(())
    }

    fn add_path(&mut self, path: PathBuf) {
        if let Ok(canonical) = fs::canonicalize(&path) {
            self.paths.push(canonical);
        }
        self.paths.push(lexical_normalize(&path));
    }

    fn rejects(&self, candidate: &Path, directory: bool) -> bool {
        self.paths.iter().any(|state| {
            let admitted_ancestor = self.stopped_original.as_ref().is_some_and(|original| {
                candidate.starts_with(original) && original.starts_with(state) && original != state
            });
            !admitted_ancestor
                && (candidate.starts_with(state) || (directory && state.starts_with(candidate)))
        }) || self.patterns.iter().any(|(root, pattern)| {
            candidate
                .strip_prefix(root)
                .is_ok_and(|relative| pattern.matches_path(relative))
                || (directory && root.starts_with(candidate))
        })
    }
}

/// Canonical spelling has already resolved intentional /tmp, dotfile and Nix
/// links. Walk that spelling without following any later component swap.
fn open_canonical_dir(path: &Path) -> Result<AnchoredDir> {
    let filesystem = AnchoredDir::open(Path::new("/"))?;
    filesystem.child(
        path.strip_prefix("/")
            .context("canonical source must be absolute")?,
    )
}

fn canonical_source(
    path: &Path,
    boundary: &NativeStateBoundary,
    directory: bool,
) -> Result<Option<PathBuf>> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error,
                "Skipping unreadable configuration source");
            return Ok(None);
        }
    };
    if boundary.rejects(&canonical, directory) {
        tracing::warn!(target: "session.profile", path = %path.display(),
            "Skipping configuration resource overlapping native session state");
        return Ok(None);
    }
    Ok(Some(canonical))
}

fn open_source_file(path: &Path, boundary: &NativeStateBoundary) -> Result<Option<File>> {
    let Some(canonical) = canonical_source(path, boundary, false)? else {
        return Ok(None);
    };
    let parent = canonical
        .parent()
        .context("configuration source has no parent")?;
    let anchor = match open_canonical_dir(parent) {
        Ok(anchor) => anchor,
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error,
                "Skipping changed or unreadable configuration source parent");
            return Ok(None);
        }
    };
    match anchor.open_regular(
        Path::new(canonical.file_name().context("source has no leaf")?),
        usize::MAX,
    ) {
        Ok(file) => Ok(file),
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %path.display(), %error,
                "Skipping unreadable configuration file");
            Ok(None)
        }
    }
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
    let destination = AnchoredDir::open(sandbox_dir)?;
    for &(name, content) in seed_files {
        let relative = Path::new(name);
        destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        destination.publish_file(
            relative,
            &mut content.as_bytes(),
            Permissions::from_mode(0o600),
            false,
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
        let Some(mut source) = open_source_file(&host_dir.join(relative), boundary)? else {
            continue;
        };
        let permissions = source.metadata()?.permissions();
        parent.publish_file(leaf, &mut source, permissions, !preserve)?;
    }
    for &name in copy_dirs {
        let relative = Path::new(name);
        let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(
            relative
                .file_name()
                .context("resource directory has no leaf")?,
        );
        seed_directory(&host_dir.join(relative), &parent, leaf, boundary, true)?;
    }
    Ok(())
}

fn seed_directory(
    source: &Path,
    destination: &AnchoredDir,
    leaf: &Path,
    boundary: &NativeStateBoundary,
    discovery_links: bool,
) -> Result<()> {
    if destination.regular_lookup(leaf)?.is_some() {
        return Ok(());
    }
    let Some(canonical) = canonical_source(source, boundary, true)? else {
        return Ok(());
    };
    let source = match open_canonical_dir(&canonical) {
        Ok(source) => source,
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                "Skipping unreadable resource directory");
            return Ok(());
        }
    };
    // Read before creating a stage: an unreadable top-level source must not
    // become an empty, permanently seed-once resource directory.
    let entries = match source.read_dir(Path::new(""), usize::MAX) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(target: "session.profile", path = %canonical.display(), %error,
                "Skipping unreadable resource directory");
            return Ok(());
        }
    };
    let stage_name = PathBuf::from(format!(".aoe-resource-{}", uuid::Uuid::new_v4()));
    let stage = destination.create_child(&stage_name)?;
    let mut ancestors = HashSet::new();
    ancestors.insert(source.identity()?);
    let result = copy_entries(
        &source,
        Path::new(""),
        &stage,
        entries,
        boundary,
        &mut ancestors,
        discovery_links,
    )
    .and_then(|()| stage.sync())
    .and_then(|()| destination.publish_directory(&stage_name, leaf));
    if !matches!(result, Ok(true)) {
        destination.remove_staged_dir(&stage_name)?;
    }
    result.map(|_| ())
}

fn copy_entries(
    source: &AnchoredDir,
    relative: &Path,
    destination: &AnchoredDir,
    entries: Vec<std::ffi::OsString>,
    boundary: &NativeStateBoundary,
    ancestors: &mut HashSet<(libc::dev_t, libc::ino_t)>,
    discovery_links: bool,
) -> Result<()> {
    for name in entries {
        let input = relative.join(&name);
        let spelling = source.path().join(&input);
        let Some(canonical) = canonical_source(&spelling, boundary, false)? else {
            continue;
        };
        let within = canonical.strip_prefix(source.path());
        if within.is_err() {
            if discovery_links && relative.as_os_str().is_empty() {
                // An entry in a native discovery collection is an explicit
                // resource root. Its own descendants cannot grant further roots.
                copy_discovered_entry(&canonical, destination, Path::new(&name), boundary)?;
            } else {
                tracing::warn!(target: "session.profile", path = %spelling.display(),
                    "Skipping resource link escaping its approved source root");
            }
            continue;
        }
        let within = within.expect("checked above");
        match source.open_regular(within, usize::MAX) {
            Ok(Some(mut file)) => {
                let permissions = file.metadata()?.permissions();
                destination.publish_file(Path::new(&name), &mut file, permissions, false)?;
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(target: "session.profile", path = %spelling.display(), %error,
                    "Skipping unreadable resource file");
                continue;
            }
        }
        if boundary.rejects(&canonical, true) {
            continue;
        }
        let child = match source.child(within) {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(target: "session.profile", path = %spelling.display(), %error,
                    "Skipping unreadable resource entry");
                continue;
            }
        };
        let identity = child.identity()?;
        if !ancestors.insert(identity) {
            tracing::warn!(target: "session.profile", path = %spelling.display(),
                "Skipping a resource symlink cycle");
            continue;
        }
        let children = match child.read_dir(Path::new(""), usize::MAX) {
            Ok(children) => children,
            Err(error) => {
                ancestors.remove(&identity);
                tracing::warn!(target: "session.profile", path = %spelling.display(), %error,
                    "Skipping unreadable resource subtree");
                continue;
            }
        };
        let target = destination.create_child(Path::new(&name))?;
        copy_entries(
            source, within, &target, children, boundary, ancestors, false,
        )?;
        target.sync()?;
        ancestors.remove(&identity);
    }
    Ok(())
}

fn copy_discovered_entry(
    canonical: &Path,
    destination: &AnchoredDir,
    leaf: &Path,
    boundary: &NativeStateBoundary,
) -> Result<()> {
    if let Some(mut file) = open_source_file(canonical, boundary)? {
        let permissions = file.metadata()?.permissions();
        destination.publish_file(leaf, &mut file, permissions, false)?;
    } else {
        seed_directory(canonical, destination, leaf, boundary, false)?;
    }
    Ok(())
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
    if pairs.is_empty() {
        return Ok(());
    }
    let destination = AnchoredDir::open(destination)?;
    let units = destination.create_child(Path::new(".aoe-credential-pairs"))?;
    for &(data_name, key_name) in pairs {
        let final_name = Path::new(data_name);
        let data_link = PathBuf::from(".aoe-credential-pairs")
            .join(final_name)
            .join("data");
        let key_link = PathBuf::from(".aoe-credential-pairs")
            .join(final_name)
            .join("key");
        if units.regular_lookup(final_name)?.is_some() {
            continue;
        }
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
        let Some(mut data) = open_source_file(&source.join(data_name), boundary)? else {
            continue;
        };
        let Some(mut key) = open_source_file(&source.join(key_name), boundary)? else {
            continue;
        };
        let stage_name = PathBuf::from(format!(".pair-{}", uuid::Uuid::new_v4()));
        let stage = units.create_child(&stage_name)?;
        let result = (|| -> Result<bool> {
            stage.publish_file(
                Path::new("data"),
                &mut data,
                Permissions::from_mode(0o600),
                false,
            )?;
            stage.publish_file(
                Path::new("key"),
                &mut key,
                Permissions::from_mode(0o600),
                false,
            )?;
            stage.sync()?;
            for (path, target) in paths {
                destination.create_symlink(path, target)?;
            }
            // A native login that replaced either path owns the pair now.
            for (path, target) in paths {
                if destination.read_link(path)?.as_ref() != Some(target) {
                    return Ok(false);
                }
            }
            units.publish_directory(&stage_name, final_name)
        })();
        if !matches!(result, Ok(true)) {
            units.remove_staged_dir(&stage_name)?;
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
    stopped_original: bool,
) -> Result<()> {
    let destination = AnchoredDir::open(destination)?;
    for &name in files {
        let relative = Path::new(name);
        let parent = destination.create_child(relative.parent().unwrap_or(Path::new("")))?;
        let leaf = Path::new(relative.file_name().context("SQLite seed has no leaf")?);
        if parent.regular_lookup(leaf)?.is_some() {
            continue;
        }
        let Some(canonical) = canonical_source(&source.join(relative), boundary, false)? else {
            continue;
        };
        let scratch = tempfile::tempdir()?;
        let scratch_path = fs::canonicalize(scratch.path())?;
        let scratch_anchor = open_canonical_dir(&scratch_path)?;
        let input = if stopped_original {
            // SQLite readers may create or alter SHM read marks. Never let a
            // snapshot change any byte of the original that v030 must retain.
            for suffix in ["", "-wal", "-shm"] {
                let original = PathBuf::from(format!("{}{suffix}", canonical.display()));
                let Some(mut file) = open_source_file(&original, boundary)? else {
                    continue;
                };
                scratch_anchor.publish_file(
                    Path::new(&format!("source.db{suffix}")),
                    &mut file,
                    Permissions::from_mode(0o600),
                    false,
                )?;
            }
            scratch_path.join("source.db")
        } else {
            canonical
        };
        let connection = rusqlite::Connection::open_with_flags(
            &input,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let snapshot = scratch_path.join("snapshot.db");
        connection.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
        drop(connection);
        let mut snapshot = scratch_anchor
            .open_regular(Path::new("snapshot.db"), usize::MAX)?
            .context("SQLite did not produce a regular config snapshot")?;
        parent.publish_file(leaf, &mut snapshot, Permissions::from_mode(0o600), false)?;
    }
    Ok(())
}

pub(super) fn seed_configured_resources(
    mount: &AgentConfigMount,
    source: &Path,
    destination: &Path,
    home: &Path,
    workspace: &Path,
    boundary: &NativeStateBoundary,
) -> Result<()> {
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
        "claude" => {
            resources.follow_file_imports(Path::new("CLAUDE.md"), 0, &mut HashSet::new())?
        }
        _ => {}
    }
    Ok(())
}

fn read_document(
    root: &AnchoredDir,
    relative: &Path,
    yaml: bool,
) -> Result<Option<serde_json::Value>> {
    let Some(mut file) = root.open_regular(relative, usize::MAX)? else {
        return Ok(None);
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
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
            .or_else(|_| resolved.strip_prefix(&self.boundary.source_root))
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
        if let Some(mut file) = open_source_file(&input, self.boundary)? {
            let permissions = file.metadata()?.permissions();
            parent.publish_file(leaf, &mut file, permissions, false)?;
        } else if directory_allowed {
            seed_directory(&input, &parent, leaf, self.boundary, false)?;
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
        let Some(mut file) = self.destination.open_regular(relative, usize::MAX)? else {
            return Ok(());
        };
        let mut content = String::new();
        file.read_to_string(&mut content)?;
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
