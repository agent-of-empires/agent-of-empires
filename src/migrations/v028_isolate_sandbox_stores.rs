//! Migration v028: move shared sandbox stores to the private v2 layout.
//!
//! A live legacy cohort remains readable until it stops. Stopped cohorts are
//! copied under a global transition lock, synced, atomically published, and
//! switched by their durable generation field. Staging directories, the
//! journal, and the legacy quarantine exist only while that bounded transition
//! is pending; the committed state keeps only `sandbox-v2/<instance>`.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const JOURNAL: &str = ".v028-sandbox-transition.json";
pub(crate) const LOCK: &str = ".v028-sandbox-transition.lock";

type RunningProbe<'a> = dyn Fn(&str) -> Result<bool> + 'a;

#[derive(Clone)]
struct Target {
    registry: usize,
    row: usize,
    id: String,
    shared: PathBuf,
    private: PathBuf,
    exclude_instance_children: bool,
    overlay_shared_root: Option<PathBuf>,
}

struct Registry {
    path: PathBuf,
    value: Value,
}

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for sandbox migration")?;
    run_in(&app_dir, &home, super::get_current_version() == 27, &|id| {
        Ok(crate::containers::DockerContainer::from_session_id(id).is_running()?)
    })
}

/// Retry cohorts that were live during the schema migration. This is called on
/// every startup until no pre-v2 row remains, then becomes a cheap read.
pub(crate) fn reconcile_pending() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    if !transition_may_be_pending(&app_dir)? {
        return Ok(());
    }
    let home = dirs::home_dir().context("home directory unavailable for sandbox migration")?;
    run_in(&app_dir, &home, true, &|id| {
        Ok(crate::containers::DockerContainer::from_session_id(id).is_running()?)
    })
}

fn transition_may_be_pending(app_dir: &Path) -> Result<bool> {
    if app_dir.join(JOURNAL).exists() {
        return Ok(true);
    }
    for registry in load_registries(app_dir)? {
        let Some(rows) = registry.value.as_array() else {
            continue;
        };
        if rows.iter().any(|row| {
            row.get("sandbox_info")
                .and_then(|sandbox| sandbox.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && (row
                    .get("sandbox_store_generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    < u64::from(crate::session::container_config::CURRENT_SANDBOX_STORE_GENERATION)
                    || transition_paths(row).is_some()
                    || row.get("sandbox_store_transition_sources").is_some())
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run_in(
    app_dir: &Path,
    home: &Path,
    resume_schema_27: bool,
    is_running: &RunningProbe<'_>,
) -> Result<()> {
    fs::create_dir_all(app_dir)?;
    let _lock = crate::session::acquire_storage_flock(app_dir, LOCK)?;
    let registry_paths = registry_paths(app_dir)?;
    let mut registry_dirs: Vec<PathBuf> = registry_paths
        .iter()
        .filter_map(|path| path.parent())
        .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
        .collect();
    registry_dirs.sort();
    registry_dirs.dedup();
    let _registry_locks: Vec<crate::session::StorageFlock> = registry_dirs
        .iter()
        .map(|dir| {
            crate::session::acquire_storage_flock(dir, crate::session::STORAGE_LOCK_FILENAME)
        })
        .collect::<Result<_>>()?;
    let mut registries = load_registry_paths(registry_paths)?;
    let journal = app_dir.join(JOURNAL);
    match fs::read(&journal) {
        Ok(bytes) => {
            let _: Vec<String> =
                serde_json::from_slice(&bytes).context("parsing v028 transition journal")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut targets = Vec::new();
    let mut affected_rows = BTreeSet::new();
    let mut all_sandbox_ids = BTreeSet::new();
    let mut known_sources = BTreeSet::new();
    let mut cleanup_roots = BTreeSet::new();
    let mut needs_registry_write = false;
    let mut defer_source_retirement = false;

    for (registry_index, registry) in registries.iter_mut().enumerate() {
        let profile = profile_for_registry(app_dir, &registry.path);
        let Some(rows) = registry.value.as_array_mut() else {
            continue;
        };
        for (row_index, row) in rows.iter_mut().enumerate() {
            if row
                .as_object_mut()
                .and_then(|object| object.remove("sandbox_store_transition_sources"))
                .is_some()
            {
                needs_registry_write = true;
            }
            if !row
                .pointer("/sandbox_info/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let generation = row
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let Some(id) = row.get("id").and_then(Value::as_str).map(str::to_owned) else {
                defer_source_retirement = true;
                continue;
            };
            crate::session::validate_instance_id(&id)?;
            all_sandbox_ids.insert(std::ffi::OsString::from(&id));
            if generation
                >= u64::from(crate::session::container_config::CURRENT_SANDBOX_STORE_GENERATION)
            {
                if clear_transition_metadata(row) {
                    needs_registry_write = true;
                }
                continue;
            }
            let Some(tool) = row.get("tool").and_then(Value::as_str) else {
                defer_source_retirement = true;
                continue;
            };
            let config = crate::session::profile_config::resolve_config_or_warn(&profile);
            let detect_as = row
                .get("detect_as")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| config.session.agent_detect_as.get(tool).map(String::as_str));
            let Some(agent) = crate::agents::get_agent(tool)
                .or_else(|| detect_as.and_then(crate::agents::get_agent))
            else {
                defer_source_retirement = true;
                continue;
            };
            let declared = config.session.agent_config_dir_for(tool, home);
            let mut fresh_plans = crate::session::container_config::sandbox_store_migration_paths(
                agent.name,
                home,
                declared.as_deref(),
                &id,
            )?;
            let stored_plans = transition_paths(row);
            let stored_private = stored_plans.as_ref().is_some_and(|plans| {
                plans.iter().all(|(source, _)| {
                    source.file_name().is_some_and(|name| name == id.as_str())
                        && source
                            .parent()
                            .and_then(Path::file_name)
                            .is_some_and(|name| name == "sandbox")
                })
            });
            let old_private =
                agent.name == "codex" || (resume_schema_27 && generation == 0) || stored_private;
            if old_private {
                for (shared, _) in &mut fresh_plans {
                    *shared = shared.join(&id);
                }
            }
            let mut trusted_sources: BTreeSet<PathBuf> = fresh_plans
                .iter()
                .map(|(source, _)| source.clone())
                .collect();
            let mut default_plans =
                crate::session::container_config::sandbox_store_migration_paths(
                    agent.name, home, None, &id,
                )?;
            if old_private {
                for (source, _) in &mut default_plans {
                    *source = source.join(&id);
                }
            }
            trusted_sources.extend(default_plans.into_iter().map(|(source, _)| source));
            trusted_sources = trusted_sources
                .into_iter()
                .map(|source| fs::canonicalize(&source).unwrap_or(source))
                .collect();
            let mut plans = if let Some(stored) = stored_plans.as_ref() {
                if stored.len() != fresh_plans.len() {
                    bail!("v028 stored transition plan count changed for {id}");
                }
                let mut resumed = Vec::with_capacity(stored.len());
                for ((source, _), (fresh_source, destination)) in
                    stored.iter().zip(fresh_plans.iter())
                {
                    let canonical = fs::canonicalize(source).unwrap_or_else(|_| source.clone());
                    if trusted_sources.contains(source) || trusted_sources.contains(&canonical) {
                        resumed.push((source.clone(), destination.clone()));
                        continue;
                    }
                    match fs::symlink_metadata(source) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            // A previous custom root may have been absent when its plan was
                            // checkpointed. It carries no data and grants no authority: replace
                            // it with the currently trusted source instead of touching the stale
                            // absolute path.
                            resumed.push((fresh_source.clone(), destination.clone()));
                        }
                        Ok(_) => bail!(
                            "v028 stored transition source is outside the expected sandbox roots for {id}: {}",
                            source.display()
                        ),
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("inspecting {}", source.display()))
                        }
                    }
                }
                resumed
            } else {
                fresh_plans
            };
            for (shared, _) in &mut plans {
                match fs::symlink_metadata(&*shared) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            bail!(
                                "v028 source is a symlink or non-directory: {}",
                                shared.display()
                            );
                        }
                        *shared = fs::canonicalize(&*shared)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspecting {}", shared.display()))
                    }
                }
            }
            if old_private {
                cleanup_roots.extend(
                    plans
                        .iter()
                        .filter_map(|(source, _)| source.parent().map(Path::to_path_buf)),
                );
            }
            if stored_plans.as_ref() != Some(&plans) {
                set_transition_paths(row, &plans);
                needs_registry_write = true;
            }
            known_sources.extend(plans.iter().map(|(shared, _)| shared.clone()));
            known_sources.extend(cleanup_roots.iter().cloned());
            if plans.is_empty() {
                mark_current(row, generation, &mut needs_registry_write);
                continue;
            }
            let pending_generation = if old_private { 0 } else { 1 };
            if generation != u64::from(pending_generation) {
                set_generation(row, pending_generation);
                needs_registry_write = true;
            }
            affected_rows.insert((registry_index, row_index));
            targets.extend(plans.into_iter().map(|(shared, private)| {
                let overlay_shared_root =
                    (resume_schema_27 && old_private && agent.name != "codex")
                        .then(|| shared.parent().map(Path::to_path_buf))
                        .flatten();
                Target {
                    registry: registry_index,
                    row: row_index,
                    id: id.clone(),
                    shared,
                    private,
                    exclude_instance_children: !old_private,
                    overlay_shared_root,
                }
            }));
        }
    }

    let mut cohorts: BTreeMap<PathBuf, Vec<Target>> = BTreeMap::new();
    for target in targets {
        cohorts
            .entry(target.shared.clone())
            .or_default()
            .push(target);
    }
    if !needs_registry_write
        && cohorts.is_empty()
        && known_sources.iter().all(|source| !source.exists())
        && !journal.exists()
    {
        return Ok(());
    }
    if needs_registry_write {
        for registry in &registries {
            let bytes = serde_json::to_vec_pretty(&registry.value)?;
            crate::session::atomic_write(&registry.path, &bytes)?;
            sync_parent(&registry.path)?;
        }
    }
    if !known_sources.is_empty() {
        let paths: Vec<String> = known_sources
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        crate::session::atomic_write(&journal, &serde_json::to_vec(&paths)?)?;
        sync_parent(&journal)?;
    }

    let mut ready_rows = affected_rows.clone();
    let mut pending = Vec::new();

    for (shared, cohort) in &cohorts {
        let ids: BTreeSet<&str> = cohort.iter().map(|target| target.id.as_str()).collect();
        let live = ids.iter().try_fold(false, |live, id| {
            is_running(id).map(|running| live || running)
        })?;
        if live {
            for target in cohort {
                ready_rows.remove(&(target.registry, target.row));
            }
            pending.push(shared.to_string_lossy().into_owned());
            continue;
        }
        let excluded = all_sandbox_ids.clone();
        for target in cohort {
            publish_store(
                shared,
                &target.private,
                &excluded,
                target.overlay_shared_root.as_deref(),
                target.exclude_instance_children,
            )?;
        }
    }

    if defer_source_retirement {
        ready_rows.clear();
    }

    // Remove stopped containers only after every store for the row is durable.
    // `force=false` makes a concurrent start fail the transition rather than
    // stopping a container that became live after the probe.
    let ready_ids: BTreeSet<String> = ready_rows
        .iter()
        .filter_map(|(registry, row)| {
            registries[*registry].value.as_array()?[*row]
                .get("id")?
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    for id in ready_ids {
        let container = crate::containers::DockerContainer::from_session_id(&id);
        if container.exists()? {
            container.remove(false)?;
        }
    }

    // Retire a legacy source only after every related row has a durable private
    // store and its stopped container has been removed. Retirement deliberately
    // precedes the generation-2 commit: if AoE crashes during the rename or
    // deletion, the still-pre-v2 row makes the next run finish the quarantine
    // cleanup before committing. A committed row never has to trust persisted
    // absolute paths again.
    let all_cohorts_ready = cohorts.values().all(|cohort| {
        cohort
            .iter()
            .all(|target| ready_rows.contains(&(target.registry, target.row)))
    });
    for shared in &known_sources {
        if defer_source_retirement {
            pending.push(shared.to_string_lossy().into_owned());
            continue;
        }
        if cleanup_roots
            .iter()
            .any(|root| root != shared && shared.starts_with(root))
        {
            continue;
        }
        if cleanup_roots.contains(shared) {
            if all_cohorts_ready {
                retire_legacy(shared)?;
            } else {
                pending.push(shared.to_string_lossy().into_owned());
            }
            continue;
        }
        let related: Vec<&Target> = cohorts
            .iter()
            .filter(|(source, _)| *source == shared || source.starts_with(shared))
            .flat_map(|(_, cohort)| cohort)
            .collect();
        let all_ready = !related.is_empty()
            && related
                .iter()
                .all(|target| ready_rows.contains(&(target.registry, target.row)));
        if all_ready {
            retire_legacy(shared)?;
        } else {
            pending.push(shared.to_string_lossy().into_owned());
        }
    }

    for &(registry, row) in &ready_rows {
        if let Some(value) = registries[registry]
            .value
            .as_array_mut()
            .and_then(|rows| rows.get_mut(row))
        {
            set_generation(
                value,
                crate::session::container_config::CURRENT_SANDBOX_STORE_GENERATION,
            );
        }
    }
    for registry in &mut registries {
        if let Some(rows) = registry.value.as_array_mut() {
            for row in rows {
                if row.get("sandbox_store_generation").and_then(Value::as_u64)
                    == Some(u64::from(
                        crate::session::container_config::CURRENT_SANDBOX_STORE_GENERATION,
                    ))
                {
                    clear_transition_metadata(row);
                }
            }
        }
    }
    for registry in &registries {
        let bytes = serde_json::to_vec_pretty(&registry.value)?;
        crate::session::atomic_write(&registry.path, &bytes)?;
        sync_parent(&registry.path)?;
    }

    if pending.is_empty() {
        match fs::remove_file(&journal) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing v028 journal"),
        }
        sync_parent(&journal)?;
    } else {
        pending.sort();
        pending.dedup();
        crate::session::atomic_write(&journal, &serde_json::to_vec(&pending)?)?;
        sync_parent(&journal)?;
    }
    Ok(())
}

fn transition_paths(row: &Value) -> Option<Vec<(PathBuf, PathBuf)>> {
    let entries = row.get("sandbox_store_transition_paths")?.as_array()?;
    entries
        .iter()
        .map(|entry| {
            Some((
                PathBuf::from(entry.get("source")?.as_str()?),
                PathBuf::from(entry.get("destination")?.as_str()?),
            ))
        })
        .collect()
}

fn set_transition_paths(row: &mut Value, plans: &[(PathBuf, PathBuf)]) {
    let entries: Vec<Value> = plans
        .iter()
        .map(|(source, destination)| {
            serde_json::json!({
                "source": source,
                "destination": destination,
            })
        })
        .collect();
    if let Some(object) = row.as_object_mut() {
        object.insert("sandbox_store_transition_paths".to_string(), entries.into());
    }
}

fn clear_transition_metadata(row: &mut Value) -> bool {
    let Some(object) = row.as_object_mut() else {
        return false;
    };
    let removed_paths = object.remove("sandbox_store_transition_paths").is_some();
    let removed_sources = object.remove("sandbox_store_transition_sources").is_some();
    removed_paths || removed_sources
}

fn mark_current(row: &mut Value, generation: u64, dirty: &mut bool) {
    let current = crate::session::container_config::CURRENT_SANDBOX_STORE_GENERATION;
    if generation != u64::from(current) {
        set_generation(row, current);
        *dirty = true;
    }
    if clear_transition_metadata(row) {
        *dirty = true;
    }
}

fn set_generation(row: &mut Value, generation: u8) {
    if let Some(object) = row.as_object_mut() {
        object.insert("sandbox_store_generation".to_string(), generation.into());
    }
}

fn registry_paths(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let profiles = app_dir.join("profiles");
    match fs::read_dir(&profiles) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path().join("sessions.json");
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", profiles.display())),
    }
    let default = app_dir.join("sessions.json");
    if default.is_file() {
        paths.push(default);
    }
    paths.sort();
    Ok(paths)
}

fn load_registry_paths(paths: Vec<PathBuf>) -> Result<Vec<Registry>> {
    paths
        .into_iter()
        .map(|path| {
            let value = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Registry { path, value })
        })
        .collect()
}

fn load_registries(app_dir: &Path) -> Result<Vec<Registry>> {
    load_registry_paths(registry_paths(app_dir)?)
}

fn profile_for_registry(app_dir: &Path, path: &Path) -> String {
    path.strip_prefix(app_dir.join("profiles"))
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("")
        .to_string()
}

fn publish_store(
    source: &Path,
    destination: &Path,
    excluded_root_children: &BTreeSet<std::ffi::OsString>,
    overlay_shared_root: Option<&Path>,
    exclude_source_children: bool,
) -> Result<()> {
    let parent = destination
        .parent()
        .context("private store has no parent")?;
    let source_exists = match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
        Ok(_) => bail!(
            "v028 source is a symlink or non-directory: {}",
            source.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()))
        }
    };
    let layout_root = parent.parent().context("private layout has no parent")?;
    fs::create_dir_all(layout_root)?;
    let anchored_parent = crate::session::AnchoredDir::create(parent)?;
    fs::File::open(layout_root)?.sync_all()?;
    let leaf = destination
        .file_name()
        .context("private store has no leaf")?;
    let stage_leaf = format!(".v028-stage-{}", leaf.to_string_lossy());
    let stage = anchored_parent.path().join(&stage_leaf);
    let quarantine_leaf = format!(".v028-quarantine-{}", leaf.to_string_lossy());
    let quarantine = anchored_parent.path().join(&quarantine_leaf);
    remove_tree_no_links(&stage)?;
    remove_tree_no_links(&quarantine)?;
    if !source_exists {
        anchored_parent.ensure_dir(Path::new(leaf))?;
        fs::File::open(parent)?.sync_all()?;
        return Ok(());
    }
    fs::create_dir(&stage)?;
    copy_tree_no_links(
        source,
        &stage,
        exclude_source_children.then_some(excluded_root_children),
        false,
    )?;
    if let Some(overlay) = overlay_shared_root {
        copy_tree_no_links(overlay, &stage, Some(excluded_root_children), true)?;
    }
    fs::set_permissions(&stage, fs::symlink_metadata(source)?.permissions())?;
    sync_tree(&stage)?;

    let destination_exists = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            remove_tree_no_links(&stage)?;
            bail!("v028 destination is a symlink: {}", destination.display());
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", destination.display()))
        }
    };
    if destination_exists {
        fs::rename(destination, &quarantine)?;
    }
    fs::rename(&stage, destination)?;
    fs::File::open(parent)?.sync_all()?;
    remove_tree_no_links(&quarantine)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn copy_tree_no_links(
    source: &Path,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    overwrite_newer: bool,
) -> Result<()> {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::Mode;
    let fd = open(
        source,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
        Mode::empty(),
    )?;
    copy_tree_from_fd(
        fd,
        destination,
        excluded_children,
        Path::new(""),
        overwrite_newer,
    )
}

#[cfg(unix)]
fn copy_tree_from_fd(
    fd: std::os::fd::OwnedFd,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    relative: &Path,
    overwrite_newer: bool,
) -> Result<()> {
    use nix::dir::Dir;
    use nix::fcntl::{openat, readlinkat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, futimens, Mode};
    use nix::sys::time::TimeSpec;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let mut dir = Dir::from_fd(fd)?;
    let names: Result<Vec<std::ffi::OsString>> = dir
        .iter()
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(std::ffi::OsStr::from_bytes(
                entry.file_name().to_bytes(),
            )
            .to_owned())),
            Err(error) => Some(Err(error.into())),
        })
        .collect();
    for name in names? {
        let name = name.as_os_str();
        if excluded_children.is_some_and(|excluded| excluded.contains(name)) {
            continue;
        }
        let stat = fstatat(&dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
        let kind = stat.st_mode & nix::libc::S_IFMT;
        let target = destination.join(name);
        if kind == nix::libc::S_IFLNK {
            let link = readlinkat(&dir, name)?;
            if !relative_symlink_stays_in_root(relative, Path::new(&link)) {
                bail!("v028 source symlink escapes its sandbox root");
            }
            match fs::symlink_metadata(&target) {
                Ok(_) if overwrite_newer && source_stat_is_newer(&stat, &target)? => {
                    remove_tree_no_links(&target)?;
                }
                Ok(_) if overwrite_newer => continue,
                Ok(_) => bail!("v028 copy destination already exists: {}", target.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            std::os::unix::fs::symlink(&link, &target)?;
            continue;
        }
        if kind == nix::libc::S_IFDIR {
            let child = openat(
                &dir,
                name,
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )?;
            let existed = match fs::symlink_metadata(&target) {
                Ok(metadata)
                    if overwrite_newer
                        && metadata.is_dir()
                        && !metadata.file_type().is_symlink() =>
                {
                    true
                }
                Ok(_) => bail!(
                    "v028 copy destination has conflicting type: {}",
                    target.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&target)?;
                    false
                }
                Err(error) => return Err(error.into()),
            };
            copy_tree_from_fd(child, &target, None, &relative.join(name), overwrite_newer)?;
            if !existed || source_stat_is_newer(&stat, &target)? {
                fs::set_permissions(&target, fs::Permissions::from_mode(stat.st_mode))?;
            }
        } else if kind == nix::libc::S_IFREG {
            let file = openat(
                &dir,
                name,
                OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY | OFlag::O_NONBLOCK,
                Mode::empty(),
            )?;
            let opened = fstat(&file)?;
            if (opened.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFREG {
                bail!("v028 source entry changed type during copy");
            }
            let mut input = fs::File::from(file);
            let target_exists = match fs::symlink_metadata(&target) {
                Ok(metadata)
                    if overwrite_newer
                        && metadata.is_file()
                        && !metadata.file_type().is_symlink() =>
                {
                    if !source_stat_is_newer(&opened, &target)? {
                        continue;
                    }
                    true
                }
                Ok(_) => bail!(
                    "v028 copy destination has conflicting type: {}",
                    target.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            let mut options = fs::OpenOptions::new();
            options.write(true);
            if target_exists {
                options.truncate(true);
            } else {
                options.create_new(true);
            }
            let mut output = options.open(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.set_permissions(fs::Permissions::from_mode(opened.st_mode))?;
            futimens(
                &output,
                &TimeSpec::new(opened.st_atime, opened.st_atime_nsec),
                &TimeSpec::new(opened.st_mtime, opened.st_mtime_nsec),
            )?;
            output.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn source_stat_is_newer(stat: &nix::libc::stat, target: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let target = fs::symlink_metadata(target)?;
    Ok((stat.st_mtime, stat.st_mtime_nsec) > (target.mtime(), target.mtime_nsec()))
}

#[cfg(unix)]
fn relative_symlink_stays_in_root(parent: &Path, link: &Path) -> bool {
    use std::path::Component;
    if link.is_absolute() {
        return false;
    }
    let mut depth = parent.components().count();
    for component in link.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => return false,
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(not(unix))]
fn copy_tree_no_links(
    source: &Path,
    destination: &Path,
    excluded_children: Option<&BTreeSet<std::ffi::OsString>>,
    overwrite_newer: bool,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if excluded_children.is_some_and(|excluded| excluded.contains(&entry.file_name())) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            match fs::create_dir(&target) {
                Ok(()) => {}
                Err(error)
                    if overwrite_newer && error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            copy_tree_no_links(&entry.path(), &target, None, overwrite_newer)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_file() {
            let should_copy = match fs::symlink_metadata(&target) {
                Ok(existing) if overwrite_newer && existing.is_file() => {
                    metadata.modified()? > existing.modified()?
                }
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(error.into()),
            };
            if should_copy {
                fs::copy(entry.path(), &target)?;
                fs::set_permissions(&target, metadata.permissions())?;
                fs::File::open(&target)?.sync_all()?;
            }
        }
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            sync_tree(&entry.path())?;
        }
    }
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn retire_legacy(source: &Path) -> Result<()> {
    let parent = source.parent().context("legacy store has no parent")?;
    let quarantine = parent.join(format!(
        ".{}.v028-quarantine",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("v028 legacy source became a symlink: {}", source.display())
        }
        Ok(_) => {
            remove_tree_no_links(&quarantine)?;
            fs::rename(source, &quarantine)?;
            fs::File::open(parent)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()))
        }
    }
    remove_tree_no_links(&quarantine)?;
    fs::File::open(parent)?.sync_all()?;
    if parent.file_name().is_some_and(|name| name == "sandbox") {
        let _ = fs::remove_dir(parent);
        if let Some(grandparent) = parent.parent() {
            let _ = fs::File::open(grandparent).and_then(|dir| dir.sync_all());
        }
    }
    Ok(())
}

fn remove_tree_no_links(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o700);
            fs::set_permissions(path, permissions)?;
        }
        #[cfg(not(unix))]
        {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
        for entry in fs::read_dir(path)? {
            remove_tree_no_links(&entry?.path())?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> String {
        format!(r#"{{"id":"{id}","tool":"gemini","sandbox_info":{{"enabled":true}}}}"#)
    }

    #[test]
    #[serial_test::serial]
    fn publishes_only_after_quiescence_and_removes_transition_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/one/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/one/partial.json"), b"partial").unwrap();
        fs::write(home.join(".gemini/sandbox/one/history/id.json"), b"legacy").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, false, &|_| Ok(true)).unwrap();
        let pending: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(pending[0]["sandbox_store_generation"], 1);
        assert!(app.join(JOURNAL).is_file());
        assert!(home.join(".gemini/sandbox").is_dir());

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();
        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/history/id.json")).unwrap(),
            b"legacy"
        );
        assert!(!home.join(".gemini/sandbox-v2/one/one").exists());
        assert!(!home.join(".gemini/sandbox").exists());
        assert!(!app.join(JOURNAL).exists());
        assert!(committed[0].get("sandbox_store_transition_paths").is_none());
        assert!(committed[0]
            .get("sandbox_store_transition_sources")
            .is_none());
        assert!(fs::read_dir(home.join(".gemini/sandbox-v2"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".v028")));
    }

    #[test]
    #[serial_test::serial]
    fn pending_cohort_keeps_its_journaled_paths_when_config_changes() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let source = home.join(".gemini/sandbox");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("conversation.json"), b"original").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, false, &|_| Ok(true)).unwrap();
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                temp.path().join("changed-gemini").display()
            ),
        )
        .unwrap();

        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        assert!(
            home.join(".gemini/sandbox/conversation.json").is_file(),
            "a still-live second reconciliation must retain the legacy source"
        );
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(
                temp.path()
                    .join("changed-gemini/sandbox-v2/one/conversation.json")
            )
            .unwrap(),
            b"original"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pending_absent_custom_source_uses_the_current_trusted_path() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                custom_a.display()
            ),
        )
        .unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        let checkpoint: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(
            checkpoint[0]
                .get("sandbox_store_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            0
        );
        assert!(checkpoint[0]
            .get("sandbox_store_transition_sources")
            .is_none());
        fs::write(
            app.join("config.toml"),
            format!(
                "[session.agent_config_dir]\ngemini = \"{}\"\n",
                custom_b.display()
            ),
        )
        .unwrap();
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        assert!(custom_b.join("sandbox-v2/one").is_dir());
        assert!(!custom_a.join("sandbox").exists());
        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(committed[0]["sandbox_store_generation"], 2);
        assert!(committed[0].get("sandbox_store_transition_paths").is_none());
        assert!(committed[0]
            .get("sandbox_store_transition_sources")
            .is_none());
    }

    #[test]
    #[serial_test::serial]
    fn pending_present_custom_source_fails_closed_after_path_change() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let custom_a = temp.path().join("custom-a");
        let custom_b = temp.path().join("custom-b");
        fs::create_dir_all(custom_a.join("sandbox/one")).unwrap();
        fs::write(custom_a.join("sandbox/one/data"), b"data").unwrap();
        fs::write(
            app.join("config.toml"),
            format!(
                r#"[session.agent_config_dir]
gemini = "{}"
"#,
                custom_a.display()
            ),
        )
        .unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        fs::write(
            app.join("config.toml"),
            format!(
                r#"[session.agent_config_dir]
gemini = "{}"
"#,
                custom_b.display()
            ),
        )
        .unwrap();

        let error = run_in(&app, &home, true, &|_| Ok(false)).unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the expected sandbox roots"));
        assert_eq!(
            fs::read(custom_a.join("sandbox/one/data")).unwrap(),
            b"data"
        );
        assert!(!custom_b.join("sandbox-v2/one").exists());
    }

    #[test]
    #[serial_test::serial]
    fn schema_27_private_snapshot_merges_later_shared_root_writes() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let root = home.join(".gemini/sandbox");
        fs::create_dir_all(root.join("one")).unwrap();
        fs::write(root.join("one/state"), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(root.join("state"), b"new").unwrap();
        fs::write(root.join("after"), b"after").unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        let destination = home.join(".gemini/sandbox-v2/one");
        assert_eq!(fs::read(destination.join("state")).unwrap(), b"new");
        assert_eq!(fs::read(destination.join("after")).unwrap(), b"after");
        assert!(!root.exists());
    }

    #[test]
    #[serial_test::serial]
    fn schema_27_staggered_cohorts_do_not_copy_a_committed_peer() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        for id in ["one", "two"] {
            fs::create_dir_all(home.join(".gemini/sandbox").join(id)).unwrap();
            fs::write(
                home.join(".gemini/sandbox").join(id).join("secret"),
                id.as_bytes(),
            )
            .unwrap();
        }
        fs::write(
            app.join("sessions.json"),
            format!("[{},{}]", row("one"), row("two")),
        )
        .unwrap();

        run_in(&app, &home, true, &|id| Ok(id == "two")).unwrap();
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/two/secret")).unwrap(),
            b"two"
        );
        assert!(!home.join(".gemini/sandbox-v2/two/one").exists());
    }

    #[test]
    #[serial_test::serial]
    fn reconciles_the_already_private_v028_layout_without_copying_peers() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        for (id, contents) in [("one", b"one".as_slice()), ("two", b"two".as_slice())] {
            let root = home.join(".gemini/sandbox").join(id);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("conversation.json"), contents).unwrap();
        }
        fs::write(
            app.join("sessions.json"),
            format!("[{},{}]", row("one"), row("two")),
        )
        .unwrap();

        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        assert!(home.join(".gemini/sandbox/one/conversation.json").is_file());
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/one/conversation.json")).unwrap(),
            b"one"
        );
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/two/conversation.json")).unwrap(),
            b"two"
        );
        assert!(!home.join(".gemini/sandbox-v2/one/two").exists());
        assert!(!home.join(".gemini/sandbox-v2/two/one").exists());
        assert!(!home.join(".gemini/sandbox").exists());
    }

    #[test]
    #[serial_test::serial]
    fn codex_generation_only_fast_path_moves_its_existing_private_store() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let source = home.join(".codex/sandbox/codex-one");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("auth.json"), b"secret").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"codex-one","tool":"codex","sandbox_info":{"enabled":true}}]"#,
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(home.join(".codex/sandbox-v2/codex-one/auth.json")).unwrap(),
            b"secret"
        );
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
    }

    #[test]
    #[serial_test::serial]
    fn recovers_publication_before_registry_commit() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("legacy"), b"legacy").unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("published"), b"published").unwrap();
        let quarantine = destination.parent().unwrap().join(".v028-quarantine-one");
        let stage = destination.parent().unwrap().join(".v028-stage-one");
        fs::create_dir_all(&quarantine).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(quarantine.join("secret"), b"secret").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert!(!destination.join("published").exists());
        assert_eq!(fs::read(destination.join("legacy")).unwrap(), b"legacy");
        assert!(!source.exists());
        assert!(!quarantine.exists());
        assert!(!stage.exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
        assert!(rows[0].get("sandbox_store_transition_sources").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn recovers_legacy_quarantine_before_generation_commit() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        let legacy_quarantine = home.join(".gemini/.sandbox.v028-quarantine");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("published"), b"published").unwrap();
        fs::create_dir_all(&legacy_quarantine).unwrap();
        fs::write(legacy_quarantine.join("legacy"), b"legacy").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(destination.join("published")).unwrap(),
            b"published"
        );
        assert!(!source.exists());
        assert!(!legacy_quarantine.exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn missing_source_still_cleans_publication_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let source = home.join(".gemini/sandbox");
        let destination = home.join(".gemini/sandbox-v2/one");
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent.join(".v028-stage-one")).unwrap();
        fs::create_dir_all(parent.join(".v028-quarantine-one")).unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": source,
                "destination": destination
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![source.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert!(destination.is_dir());
        assert!(!parent.join(".v028-stage-one").exists());
        assert!(!parent.join(".v028-quarantine-one").exists());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn rejects_untrusted_persisted_transition_sources() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let victim = home.join("documents/sandbox");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        use std::os::unix::fs::MetadataExt;
        let victim_metadata = fs::symlink_metadata(&victim).unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 1,
            "sandbox_store_transition_paths": [{
                "source": victim,
                "destination": home.join(".gemini/sandbox-v2/one")
            }],
            "sandbox_store_transition_sources": [{
                "source": fs::canonicalize(&victim).unwrap(),
                "device": victim_metadata.dev(),
                "inode": victim_metadata.ino()
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        let error = run_in(&app, &home, false, &|_| Ok(false)).unwrap_err();

        assert!(error
            .to_string()
            .contains("outside the expected sandbox roots"));
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
    }

    #[test]
    #[serial_test::serial]
    fn current_rows_scrub_forged_transition_metadata_without_io() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let victim = home.join("current-generation-victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        let row = serde_json::json!({
            "id": "one",
            "tool": "gemini",
            "sandbox_info": {"enabled": true},
            "sandbox_store_generation": 2,
            "sandbox_store_transition_paths": [{
                "source": victim,
                "destination": home.join(".gemini/sandbox-v2/one")
            }],
            "sandbox_store_transition_sources": [{
                "source": victim,
                "device": 1,
                "inode": 1
            }]
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
        assert!(!home.join(".gemini/sandbox-v2/one").exists());
        assert!(!app.join(JOURNAL).exists());
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert!(rows[0].get("sandbox_store_transition_paths").is_none());
        assert!(rows[0].get("sandbox_store_transition_sources").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn scrubs_obsolete_source_fingerprints_from_ignored_rows() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let fingerprint = serde_json::json!([{
            "source": "/tmp/untrusted",
            "device": 1,
            "inode": 1
        }]);
        let rows = serde_json::json!([
            {
                "id": "disabled",
                "tool": "gemini",
                "sandbox_info": {"enabled": false},
                "sandbox_store_transition_sources": fingerprint
            },
            {
                "tool": "gemini",
                "sandbox_info": {"enabled": true},
                "sandbox_store_transition_sources": fingerprint
            },
            {
                "id": "missing-tool",
                "sandbox_info": {"enabled": true},
                "sandbox_store_transition_sources": fingerprint
            }
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        let committed: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert!(committed
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("sandbox_store_transition_sources").is_none()));
    }

    #[test]
    #[serial_test::serial]
    fn ignores_unprovenanced_journal_paths() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let victim = home.join("journal-victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"keep").unwrap();
        fs::write(app.join("sessions.json"), b"[]").unwrap();
        fs::write(
            app.join(JOURNAL),
            serde_json::to_vec(&vec![victim.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"keep");
        assert!(!app.join(JOURNAL).exists());
    }

    #[test]
    #[serial_test::serial]
    fn unresolved_rows_and_current_rows_without_provenance_are_not_retired() {
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let legacy = home.join(".gemini/sandbox");
        fs::create_dir_all(legacy.join("unknown")).unwrap();
        fs::create_dir_all(legacy.join("known")).unwrap();
        fs::create_dir_all(legacy.join("malformed")).unwrap();
        fs::write(legacy.join("unknown/keep"), b"keep").unwrap();
        fs::write(legacy.join("malformed/keep"), b"keep").unwrap();
        fs::write(legacy.join("known/data"), b"data").unwrap();
        let rows = serde_json::json!([
            {"id":"unknown","tool":"missing-agent","sandbox_info":{"enabled":true}},
            {"tool":"missing-agent","sandbox_info":{"enabled":true}},
            {"id":"known","tool":"gemini","sandbox_info":{"enabled":true}}
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();

        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert!(rows[0].get("sandbox_store_generation").is_none());
        assert!(rows[2].get("sandbox_store_generation").is_none());
        assert_eq!(fs::read(legacy.join("unknown/keep")).unwrap(), b"keep");
        assert_eq!(fs::read(legacy.join("malformed/keep")).unwrap(), b"keep");

        let repaired = serde_json::json!([
            {"id":"unknown","tool":"gemini","sandbox_info":{"enabled":true}},
            {"id":"malformed","tool":"gemini","sandbox_info":{"enabled":true}},
            rows[2].clone()
        ]);
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&repaired).unwrap(),
        )
        .unwrap();
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();
        let rows: Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[2]["sandbox_store_generation"], 2);
        assert_eq!(
            fs::read(home.join(".gemini/sandbox-v2/known/data")).unwrap(),
            b"data"
        );
        assert!(!home.join(".gemini/sandbox-v2/known/unknown").exists());
        assert!(!home.join(".gemini/sandbox-v2/known/malformed").exists());
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn persisted_sources_survive_ancestor_symlink_canonicalization() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let external = temp.path().join("external-gemini");
        fs::create_dir_all(external.join("sandbox/one")).unwrap();
        fs::write(external.join("sandbox/one/data"), b"data").unwrap();
        symlink(&external, home.join(".gemini")).unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        run_in(&app, &home, true, &|_| Ok(true)).unwrap();
        assert!(external.join("sandbox/one/data").is_file());
        run_in(&app, &home, true, &|_| Ok(false)).unwrap();

        assert_eq!(
            fs::read(external.join("sandbox-v2/one/data")).unwrap(),
            b"data"
        );
        assert!(!external.join("sandbox").exists());
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn preserves_root_and_read_only_directory_modes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        let source = home.join(".gemini/sandbox");
        fs::create_dir_all(source.join("readonly")).unwrap();
        fs::write(source.join("readonly/data"), b"data").unwrap();
        fs::set_permissions(source.join("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(app.join("sessions.json"), format!("[{}]", row("one"))).unwrap();

        run_in(&app, &home, false, &|_| Ok(false)).unwrap();

        let destination = home.join(".gemini/sandbox-v2/one");
        assert_eq!(
            fs::read(destination.join("readonly/data")).unwrap(),
            b"data"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(destination.join("readonly"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn skips_source_symlinks_and_refuses_destination_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside");
        let destination = temp.path().join("v2/one");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        fs::write(source.join("credential"), b"credential").unwrap();
        fs::set_permissions(source.join("credential"), fs::Permissions::from_mode(0o600)).unwrap();
        symlink(outside.join("secret"), source.join("escape")).unwrap();
        symlink("credential", source.join("credential-link")).unwrap();
        assert!(publish_store(&source, &destination, &BTreeSet::new(), None, false).is_err());
        assert!(!destination.exists());

        fs::remove_file(source.join("escape")).unwrap();
        publish_store(&source, &destination, &BTreeSet::new(), None, false).unwrap();
        assert_eq!(
            fs::read(destination.join("credential-link")).unwrap(),
            b"credential"
        );
        assert_eq!(
            fs::metadata(destination.join("credential"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        remove_tree_no_links(&destination).unwrap();
        symlink(&outside, &destination).unwrap();
        assert!(publish_store(&source, &destination, &BTreeSet::new(), None, false).is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");
    }
}
