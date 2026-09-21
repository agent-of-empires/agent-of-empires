//! Reclaim per-instance agent stores whose session is gone.

use crate::migrations::v027_isolate_sandbox_stores as v027;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Why a store that resolves in no profile was kept anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preserved {
    /// A container for it still exists, running or stopped, so it may be
    /// started again. A runtime that cannot answer takes this arm too.
    Retained,
    /// Not a plain directory, so what it is cannot be established.
    Ambiguous,
    /// Written to moments ago, so it may be a store being seeded for a session
    /// whose row is not inserted yet.
    Recent,
}

impl Preserved {
    pub fn label(self) -> &'static str {
        match self {
            Self::Retained => "a container for it still exists, or a runtime could not be asked",
            Self::Ambiguous => "not a plain directory",
            Self::Recent => "written to too recently to rule out a session being created",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Orphan {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Store roots scanned, in path order.
    pub roots: Vec<PathBuf>,
    /// Stores that resolve in no profile and are safe to remove.
    pub orphans: Vec<Orphan>,
    /// Stores that resolve in no profile but were kept.
    pub preserved: Vec<(PathBuf, Preserved)>,
    /// Session rows that claim a store id, across every profile of both
    /// build namespaces.
    pub owners: usize,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.orphans.iter().map(|orphan| orphan.bytes).sum()
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub plan: Plan,
    /// Stores actually removed.
    pub removed: Vec<Orphan>,
    pub failures: Vec<(PathBuf, String)>,
}

impl Outcome {
    pub fn freed(&self) -> u64 {
        self.removed.iter().map(|orphan| orphan.bytes).sum()
    }
}

/// What would be reclaimed, without removing anything.
pub fn report() -> Result<Plan> {
    let app_dir = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for store reclaim")?;
    let _locks = guard(&app_dir)?;
    plan_in(
        &app_dir,
        &also_owned(&app_dir),
        &home,
        CREATION_GRACE,
        &every_runtime_probe(true),
    )
}

/// Remove every store [`report`] would name.
pub fn reclaim() -> Result<Outcome> {
    let app_dir = crate::session::get_app_dir()?;
    let home = dirs::home_dir().context("home directory unavailable for store reclaim")?;
    let _locks = guard(&app_dir)?;
    reclaim_in(
        &app_dir,
        &also_owned(&app_dir),
        &home,
        CREATION_GRACE,
        &every_runtime_probe(true),
    )
}

/// How recently a store may have been written to and still be reclaimed.
const CREATION_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Whether any installed runtime still has a container for this id, in any state.
fn every_runtime_probe(announce: bool) -> impl Fn(&str) -> Result<bool> {
    let constructors: Vec<fn() -> crate::containers::ContainerRuntime> = vec![
        crate::containers::ContainerRuntime::docker,
        crate::containers::ContainerRuntime::podman,
        // Apple's runtime exists only on macOS; probing it elsewhere would let
        // an unrelated binary named `container` decide whether a store lives.
        #[cfg(target_os = "macos")]
        crate::containers::ContainerRuntime::apple_container,
    ];
    let probes: Vec<Box<v027::RunningProbe<'static>>> = constructors
        .into_iter()
        .map(|new| {
            Box::new(presence_probe(
                move || new().batch_container_states(crate::containers::SANDBOX_NAME_PREFIX),
                move |id| probe_exists_with(new(), id),
                announce,
            )) as Box<v027::RunningProbe<'static>>
        })
        .collect();
    move |id: &str| any_retained(&probes, id)
}

/// One runtime's presence answer, batched.
fn presence_probe(
    batch: impl Fn() -> std::collections::HashMap<String, crate::containers::ContainerState>,
    inspect: impl Fn(&str) -> Result<(bool, bool)>,
    announce: bool,
) -> impl Fn(&str) -> Result<bool> {
    v027::batched_probe_with(
        batch,
        |_| Some(true),
        inspect,
        "checking which sandbox containers exist",
        announce,
    )
}

/// Retained if any runtime says so.
fn any_retained(probes: &[Box<v027::RunningProbe<'_>>], id: &str) -> Result<bool> {
    for probe in probes {
        if probe(id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One runtime's answer for one id, in the shape v027's probe expects: whether a container exists,
/// and whether that is the fail-closed substitute for a runtime that could not be asked.
fn probe_exists_with(
    runtime: crate::containers::ContainerRuntime,
    id: &str,
) -> Result<(bool, bool)> {
    let name = crate::containers::DockerContainer::generate_name(id);
    match runtime.does_container_exist(&name) {
        Ok(running) => Ok((running, false)),
        Err(crate::containers::error::DockerError::NotInstalled) => Ok((false, false)),
        Err(error) if v027::runtime_cannot_answer(&error) => {
            tracing::warn!("sandbox reclaim keeping {id}: container runtime unavailable ({error})");
            Ok((true, true))
        }
        Err(error) => Err(error.into()),
    }
}

/// App dirs beyond our own whose sessions still claim a store under these roots.
fn also_owned(app_dir: &Path) -> Vec<PathBuf> {
    crate::session::sibling_namespace_app_dir()
        .filter(|sibling| sibling != app_dir)
        .into_iter()
        .collect()
}

/// Serialise reclaim passes against each other, and hold v027's transition lock for the duration.
fn guard(app_dir: &Path) -> Result<(crate::session::StorageFlock, crate::session::StorageFlock)> {
    fs::create_dir_all(app_dir)?;
    let pass = crate::session::acquire_storage_flock(app_dir, RECLAIM_LOCK)?;
    let transition = crate::session::acquire_storage_shared_flock(app_dir, v027::LOCK)?;
    if v027::transition_in_flight(app_dir)? {
        bail!(
            "the sandbox store migration is still moving stores; run `aoe migrate` and try again"
        );
    }
    Ok((pass, transition))
}

/// Serialises reclaim passes so two do not race to remove the same store.
const RECLAIM_LOCK: &str = ".sandbox-reclaim.lock";

/// The store ids every profile's registry claims.
fn owned_ids(app_dir: &Path, also: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut paths = registry_paths(app_dir)?;
    if paths.is_empty() {
        bail!(
            "no session registry under {}; refusing to treat every agent store as unowned",
            app_dir.display()
        );
    }
    for dir in also {
        paths.extend(registry_paths(dir)?);
    }
    let mut ids = BTreeSet::new();
    for path in paths {
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let rows = value
            .as_array()
            .with_context(|| format!("{} is not a session array", path.display()))?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .with_context(|| format!("session row without an id in {}", path.display()))?;
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

/// Every profile's registry, plus the default one.
fn registry_paths(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![app_dir.to_path_buf()];
    let profiles = app_dir.join("profiles");
    match fs::read_dir(&profiles) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                // Resolved, not `DirEntry::file_type`, which does not follow symlinks: a symlinked
                // profile directory would otherwise be skipped and its sessions would read as
                // unowned.
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => dirs.push(path),
                    // A stray file under `profiles/` is not a profile.
                    Ok(_) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "{} cannot be inspected; refusing to reclaim stores \
                                 without reading every profile",
                                path.display()
                            )
                        })
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", profiles.display())),
    }
    let mut paths = Vec::new();
    for dir in dirs {
        let path = dir.join("sessions.json");
        // Only a genuinely absent registry is a profile with no sessions.
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "{} cannot be inspected; refusing to reclaim stores without reading it",
                        path.display()
                    )
                })
            }
        }
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => paths.push(path),
            Ok(_) => bail!(
                "{} is not a regular file; refusing to reclaim stores without reading it",
                path.display()
            ),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "{} cannot be resolved; refusing to reclaim stores without reading it",
                        path.display()
                    )
                })
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Every `sandbox-v2` root any profile can place a store under: the built-in root per agent config
/// mount, plus the root of each `agent_config_dir` a profile declares.
fn store_roots(app_dir: &Path, home: &Path) -> Result<Vec<PathBuf>> {
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    let tools = crate::session::config::container_config::agent_config_mount_tools();
    for tool in &tools {
        roots.extend(
            crate::session::config::container_config::sandbox_store_roots(tool, home, None),
        );
    }
    for path in registry_paths(app_dir)? {
        let profile = v027::profile_for_registry(app_dir, &path);
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        for tool in &tools {
            let declared = config.session.agent_config_dir_for(tool, home);
            if declared.is_some() {
                roots.extend(
                    crate::session::config::container_config::sandbox_store_roots(
                        tool,
                        home,
                        declared.as_deref(),
                    ),
                );
            }
        }
    }
    Ok(roots.into_iter().collect())
}

fn plan_in(
    app_dir: &Path,
    also_owned: &[PathBuf],
    home: &Path,
    grace: std::time::Duration,
    container_exists: &v027::RunningProbe<'_>,
) -> Result<Plan> {
    let owned = owned_ids(app_dir, also_owned)?;
    let roots = store_roots(app_dir, home)?;
    let mut plan = Plan {
        owners: owned.len(),
        ..Plan::default()
    };
    for root in roots {
        if !root.exists() {
            continue;
        }
        plan.roots.push(root.clone());
        for child in v027::instance_children(&root)? {
            let id = child.to_string_lossy().into_owned();
            if owned.contains(&id) {
                continue;
            }
            let path = root.join(&child);
            match classify(&path, &id, grace, container_exists)? {
                Some(reason) => plan.preserved.push((path, reason)),
                None => {
                    let bytes = directory_bytes(&path);
                    plan.orphans.push(Orphan { id, path, bytes });
                }
            }
        }
    }
    Ok(plan)
}

/// `None` when the store may be removed.
fn classify(
    path: &Path,
    id: &str,
    grace: std::time::Duration,
    container_exists: &v027::RunningProbe<'_>,
) -> Result<Option<Preserved>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(Some(Preserved::Ambiguous));
    }
    if written_within(&metadata, grace) {
        return Ok(Some(Preserved::Recent));
    }
    if container_exists(id)? {
        return Ok(Some(Preserved::Retained));
    }
    Ok(None)
}

/// Whether `metadata` was modified inside `window`.
fn written_within(metadata: &fs::Metadata, window: std::time::Duration) -> bool {
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(age) => age < window,
        Err(_) => true,
    }
}

fn reclaim_in(
    app_dir: &Path,
    also_owned: &[PathBuf],
    home: &Path,
    grace: std::time::Duration,
    container_exists: &v027::RunningProbe<'_>,
) -> Result<Outcome> {
    let plan = plan_in(app_dir, also_owned, home, grace, container_exists)?;
    let mut outcome = Outcome {
        plan,
        ..Outcome::default()
    };
    // Ownership is re-read: the pass holds the transition lock shared, so a session created during
    // it can publish its row, and a store that was unclaimed at planning time may be claimed by the
    // time we reach it.
    let owned = owned_ids(app_dir, also_owned)?;
    for orphan in &outcome.plan.orphans {
        // Per candidate, not once for the loop.
        v027::refresh_liveness();
        if owned.contains(&orphan.id) {
            outcome
                .failures
                .push((orphan.path.clone(), "claimed since the scan".to_string()));
            continue;
        }
        // Re-classified against the path as it is now, with fresh container evidence: the last
        // thing standing between a swapped store, or one whose container reappeared, and
        // `remove_dir_all`.
        match classify(&orphan.path, &orphan.id, grace, container_exists) {
            Ok(None) => {}
            Ok(Some(reason)) => {
                outcome
                    .failures
                    .push((orphan.path.clone(), reason.label().to_string()));
                continue;
            }
            Err(error) => {
                outcome
                    .failures
                    .push((orphan.path.clone(), error.to_string()));
                continue;
            }
        }
        match fs::remove_dir_all(&orphan.path) {
            Ok(()) => {
                tracing::info!(target: "session.store",
                    "reclaimed orphan agent store {}", orphan.path.display());
                outcome.removed.push(orphan.clone());
            }
            Err(error) => outcome
                .failures
                .push((orphan.path.clone(), error.to_string())),
        }
    }
    Ok(outcome)
}

/// Bytes held by a directory tree, counting symlinks themselves rather than
/// what they point at.
fn directory_bytes(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Sizing is advisory, so a subtree that cannot be read is counted as zero rather than
        // failing the report.
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `DirEntry::metadata` does not traverse symlinks.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    total
}

/// Remove the stores of one session being purged.
pub(crate) fn remove_stores_for(
    instance: &crate::session::Instance,
) -> Result<(Vec<PathBuf>, u64)> {
    if instance.sandbox_store_generation
        < crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION
    {
        return Ok((Vec::new(), 0));
    }
    let home = dirs::home_dir().context("home directory unavailable for store removal")?;
    let Some(agent) = instance.resolved_agent() else {
        return Ok((Vec::new(), 0));
    };
    let config = crate::session::config::profile_config::resolve_config_or_warn(
        &instance.effective_profile(),
    );
    let declared = config.session.agent_config_dir_for(&instance.tool, &home);
    let mut removed = Vec::new();
    let mut freed = 0;
    for path in crate::session::config::container_config::sandbox_store_dirs(
        agent.name,
        &home,
        declared.as_deref(),
        &instance.id,
    )? {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                tracing::warn!(target: "session.store",
                    "leaving agent store {}: not a plain directory", path.display());
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()))
            }
        }
        let bytes = directory_bytes(&path);
        fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        freed += bytes;
        removed.push(path);
    }
    Ok((removed, freed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_rows(app: &Path, rows: &[&str]) {
        let ids: Vec<String> = rows
            .iter()
            .map(|id| format!(r#"{{"id":"{id}"}}"#))
            .collect();
        fs::write(app.join("sessions.json"), format!("[{}]", ids.join(","))).unwrap();
    }

    fn store(home: &Path, id: &str, bytes: usize) -> PathBuf {
        let path = home.join(".claude").join("sandbox-v2").join(id);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".credentials.json"), vec![b'x'; bytes]).unwrap();
        path
    }

    /// A fresh `(tempdir, app dir, home)` triple with the app dir created.
    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        (dir, app, home)
    }

    const NO_GRACE: std::time::Duration = std::time::Duration::ZERO;

    fn gone(_: &str) -> Result<bool> {
        Ok(false)
    }

    fn retained(_: &str) -> Result<bool> {
        Ok(true)
    }

    #[test]
    fn a_store_no_profile_claims_is_an_orphan_and_one_that_is_claimed_is_not() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &["1111111111111111"]);
        store(&home, "1111111111111111", 10);
        let orphan = store(&home, "2222222222222222", 40);

        let plan = plan_in(&app, &[], &home, NO_GRACE, &gone).unwrap();

        assert_eq!(
            plan.orphans.iter().map(|o| &o.path).collect::<Vec<_>>(),
            vec![&orphan]
        );
        assert_eq!(plan.bytes(), 40);
        assert!(plan.preserved.is_empty());
    }

    #[test]
    fn a_store_claimed_by_another_profile_is_not_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        let other = app.join("profiles").join("work");
        fs::create_dir_all(&other).unwrap();
        app_with_rows(&app, &[]);
        fs::write(
            other.join("sessions.json"),
            r#"[{"id":"2222222222222222"}]"#,
        )
        .unwrap();
        store(&home, "2222222222222222", 40);

        let plan = plan_in(&app, &[], &home, NO_GRACE, &gone).unwrap();

        assert!(plan.orphans.is_empty(), "{:?}", plan.orphans);
        assert_eq!(plan.owners, 1);
    }

    #[test]
    fn an_unreadable_registry_fails_the_pass_instead_of_orphaning_everything() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        let broken = app.join("profiles").join("work");
        fs::create_dir_all(&broken).unwrap();
        app_with_rows(&app, &[]);
        store(&home, "2222222222222222", 40);

        for content in [r#"{"not":"an array"}"#, "{", r#"[{"title":"no id"}]"#] {
            fs::write(broken.join("sessions.json"), content).unwrap();
            let error = plan_in(&app, &[], &home, NO_GRACE, &gone).unwrap_err();
            assert!(
                error.chain().any(|cause| {
                    let text = cause.to_string();
                    text.contains("sessions.json") || text.contains("session array")
                }),
                "{content}: {error:#}"
            );
        }
    }

    #[test]
    fn no_registry_at_all_fails_rather_than_reclaiming_every_store() {
        let (_dir, app, home) = dirs();
        store(&home, "2222222222222222", 40);

        let error = plan_in(&app, &[], &home, NO_GRACE, &gone).unwrap_err();

        assert!(error.to_string().contains("refusing"), "{error:#}");
    }

    #[test]
    fn a_live_orphan_is_preserved_and_never_removed() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &retained).unwrap();

        assert!(outcome.removed.is_empty());
        assert_eq!(
            outcome.plan.preserved,
            vec![(path.clone(), Preserved::Retained)]
        );
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_store_is_preserved_and_its_target_is_left_alone() {
        let (dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let target = dir.path().join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), b"keep").unwrap();
        let root = home.join(".claude").join("sandbox-v2");
        fs::create_dir_all(&root).unwrap();
        let link = root.join("2222222222222222");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &gone).unwrap();

        assert_eq!(
            outcome.plan.preserved,
            vec![(link.clone(), Preserved::Ambiguous)]
        );
        assert!(target.join("keep").exists());
    }

    #[test]
    fn reclaiming_removes_the_orphan_and_reports_what_it_freed() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &["1111111111111111"]);
        let kept = store(&home, "1111111111111111", 10);
        let orphan = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &gone).unwrap();

        assert!(!orphan.exists());
        assert!(kept.exists());
        assert_eq!(outcome.freed(), 40);
        assert!(outcome.failures.is_empty());
    }

    #[test]
    fn a_move_in_flight_blocks_the_pass_but_a_parked_session_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"archived_at":"2026-01-01T00:00:00Z"}]"#,
        )
        .unwrap();

        guard(&app).expect("a parked pre-transition session must not block the pass");

        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"1111111111111111","sandbox_info":{"enabled":true},"sandbox_store_transition_paths":[{"source":"/a","destination":"/b"}]}]"#,
        )
        .unwrap();

        assert!(guard(&app).is_err(), "a move in flight must block the pass");
    }

    #[test]
    fn a_session_of_the_other_build_namespace_is_not_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let sibling = dir.path().join("app-dev");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        app_with_rows(&app, &[]);
        fs::write(
            sibling.join("sessions.json"),
            r#"[{"id":"2222222222222222"}]"#,
        )
        .unwrap();
        let store = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(&app, &[sibling], &home, NO_GRACE, &gone).unwrap();

        assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
        assert!(store.exists(), "the other build's store was reclaimed");
    }

    #[test]
    fn a_store_being_created_is_preserved_until_the_grace_period_lapses() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let seeding = store(&home, "2222222222222222", 40);

        let outcome =
            reclaim_in(&app, &[], &home, std::time::Duration::from_secs(600), &gone).unwrap();

        assert!(seeding.exists(), "a store being seeded was reclaimed");
        assert_eq!(
            outcome.plan.preserved,
            vec![(seeding, Preserved::Recent)],
            "and the report says why"
        );
    }

    #[test]
    fn a_store_claimed_after_the_scan_is_not_removed() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);

        let claim_on_probe = |_: &str| {
            app_with_rows(&app, &["2222222222222222"]);
            Ok(false)
        };
        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &claim_on_probe).unwrap();

        assert!(path.exists(), "a store claimed mid-pass was reclaimed");
        assert!(outcome.removed.is_empty());
    }

    #[test]
    fn liveness_is_the_union_of_every_runtime_asked() {
        let quiet: Box<v027::RunningProbe<'_>> = Box::new(|_| Ok(false));
        let busy: Box<v027::RunningProbe<'_>> = Box::new(|_| Ok(true));
        let angry: Box<v027::RunningProbe<'_>> =
            Box::new(|_| Err(anyhow::anyhow!("runtime exploded")));

        assert!(!any_retained(&[], "1111111111111111").unwrap());
        assert!(!any_retained(std::slice::from_ref(&quiet), "1111111111111111").unwrap());
        assert!(any_retained(&[quiet, busy, angry], "1111111111111111").unwrap());

        let angry: Box<v027::RunningProbe<'_>> =
            Box::new(|_| Err(anyhow::anyhow!("runtime exploded")));
        assert!(
            any_retained(&[angry], "1111111111111111").is_err(),
            "a real fault must fail the pass, not read as quiescent"
        );
    }

    // The batch listing is read for presence, not liveness.
    #[test]
    fn a_listed_stopped_container_counts_as_present() {
        use crate::containers::{ContainerState, DockerContainer};

        let id = "2222222222222222";
        let listed = |state: ContainerState| {
            let probe = presence_probe(
                move || {
                    let mut states = std::collections::HashMap::new();
                    states.insert(DockerContainer::generate_name(id), state);
                    states
                },
                |_| Ok((false, false)),
                false,
            );
            probe(id).unwrap()
        };

        for state in [
            ContainerState::Exited,
            ContainerState::Created,
            ContainerState::Dead,
            ContainerState::Running,
            ContainerState::Paused,
            ContainerState::Restarting,
            ContainerState::Other,
        ] {
            v027::refresh_liveness();
            assert!(listed(state), "{state:?} was read as absent");
        }

        v027::refresh_liveness();
        let absent = presence_probe(
            std::collections::HashMap::new,
            |_| Ok((false, false)),
            false,
        );
        assert!(!absent(id).unwrap());
        v027::refresh_liveness();
        let unanswerable =
            presence_probe(std::collections::HashMap::new, |_| Ok((true, true)), false);
        assert!(
            unanswerable(id).unwrap(),
            "an unreachable runtime must keep the store"
        );
    }

    #[test]
    fn a_store_live_under_another_runtime_is_preserved() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);
        let probes: Vec<Box<v027::RunningProbe<'_>>> =
            vec![Box::new(|_| Ok(false)), Box::new(|_| Ok(true))];
        let any = move |id: &str| any_retained(&probes, id);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &any).unwrap();

        assert!(
            path.exists(),
            "a store live under another runtime was removed"
        );
        assert_eq!(outcome.plan.preserved, vec![(path, Preserved::Retained)]);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn purging_never_follows_a_symlinked_store() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(dir.path());
        let target = dir.path().join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), b"keep").unwrap();
        let mut instance = crate::session::Instance::new("t", "/tmp/p");
        let root = dir.path().join(".claude").join("sandbox-v2");
        fs::create_dir_all(&root).unwrap();
        let link = root.join(&instance.id);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        instance.sandbox_store_generation =
            crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION;

        let (removed, freed) = remove_stores_for(&instance).unwrap();

        assert!(removed.is_empty(), "{removed:?}");
        assert_eq!(freed, 0);
        assert!(link.exists(), "the symlink was removed");
        assert!(
            target.join("keep").exists(),
            "the symlink target was followed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unresolvable_registry_aborts_before_removing_anything() {
        type Plant = fn(&Path);
        let cases: &[(&str, Plant)] = &[
            ("profile directory", |app: &Path| {
                std::os::unix::fs::symlink(app.join("nowhere"), app.join("profiles").join("work"))
                    .unwrap();
            }),
            ("registry file", |app: &Path| {
                let profile = app.join("profiles").join("work");
                fs::create_dir_all(&profile).unwrap();
                std::os::unix::fs::symlink(app.join("nowhere"), profile.join("sessions.json"))
                    .unwrap();
            }),
        ];

        for (name, plant) in cases {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path().join("app");
            let home = dir.path().join("home");
            fs::create_dir_all(app.join("profiles")).unwrap();
            app_with_rows(&app, &[]);
            let orphan = store(&home, "2222222222222222", 40);
            plant(&app);

            let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &gone);

            assert!(
                outcome.is_err(),
                "{name}: an unresolvable registry must fail the pass, \
                 not empty the ownership set"
            );
            assert!(
                orphan.exists(),
                "{name}: a store was removed despite the failure"
            );
        }
    }

    #[test]
    fn a_candidate_whose_container_appears_mid_pass_survives() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let first = store(&home, "2222222222222222", 40);
        let second = store(&home, "3333333333333333", 40);

        let gate = first.clone();
        let appears = move |_: &str| Ok(!gate.exists());

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &appears).unwrap();

        assert!(!first.exists(), "the first orphan should still be removed");
        assert!(
            second.exists(),
            "a store whose container appeared mid-pass was removed"
        );
        assert_eq!(
            outcome.plan.orphans.len(),
            2,
            "both were candidates when the plan was made"
        );
        assert_eq!(outcome.removed.len(), 1);
    }

    #[test]
    fn a_directory_that_is_not_an_instance_id_is_never_touched() {
        let (_dir, app, home) = dirs();
        app_with_rows(&app, &[]);
        let staging = home
            .join(".claude")
            .join("sandbox-v2")
            .join(".v027-staging");
        fs::create_dir_all(&staging).unwrap();

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &gone).unwrap();

        assert!(staging.exists());
        assert!(outcome.plan.orphans.is_empty());
    }
}
