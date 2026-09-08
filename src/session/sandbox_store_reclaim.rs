//! Reclaim per-instance agent stores whose session is gone.
//!
//! A sandboxed session mounts `<agent config>/sandbox-v2/<instance id>` as the
//! agent's config directory, so the store holds that agent's credentials.
//! Purging a session removes its own stores; this pass covers the ones already
//! stranded, including stores left by versions that removed nothing.
//!
//! Both are deliberately narrow, because the thing being deleted is a copy of a
//! live credential:
//!
//! - A store is an orphan only when its id resolves in no profile of either
//!   build namespace. A registry that cannot be read is never "a profile with
//!   no sessions"; the pass fails and deletes nothing.
//! - Liveness is v027's judgement, not a second one: a store whose container is
//!   running, or whose path is not a plain directory, is preserved, and a
//!   container runtime that cannot answer reads as live. It is asked of every
//!   runtime installed, not just the one this build's config names, because
//!   ownership spans both build namespaces and each can name a different one.
//! - A store seeded moments ago is preserved. Container preparation seeds the
//!   store before the session row is inserted (`cli::add` runs `on_create`
//!   hooks, and so `get_container_for_instance`, before it persists), so a
//!   just-created store is briefly indistinguishable from an orphan.
//! - The pass runs under v027's transition lock and refuses while a store move
//!   is mid-flight. It cannot see a half-copied store either way: v027 copies
//!   into `.v027-stage-<id>` and renames, and only a bare 16-hex name is ever a
//!   candidate here, so a store appears to this pass whole or not at all.
//!   Widening that name filter would break the guarantee.

use crate::migrations::v027_isolate_sandbox_stores as v027;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Why a store that resolves in no profile was kept anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preserved {
    /// Its container is live. A runtime that cannot answer takes this arm
    /// too, which is the whole point of v027's probe.
    Live,
    /// Not a plain directory, so what it is cannot be established.
    Ambiguous,
    /// Written to moments ago, so it may be a store being seeded for a session
    /// whose row is not inserted yet.
    Recent,
}

impl Preserved {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "container is running, or the runtime could not be asked",
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
///
/// Container preparation seeds a store before the session row exists, so
/// within this window an orphan and a session being created look the same. A
/// store stranded by a purge is minutes to months old, so the cost of the
/// window is nothing and it closes the only gap the locks cannot.
const CREATION_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Liveness asked of every container runtime installed, not just the one this
/// build's config names.
///
/// `get_container_runtime` resolves through `Config::load`, whose path is the
/// invoking build's app dir, and each namespace (or profile) can name a
/// different runtime. Asking only ours would read a store mounted into a
/// running container of another runtime as quiescent and delete it. Each
/// runtime is asked through v027's own probe, so the fail-closed answer for a
/// runtime that cannot be reached is unchanged; a runtime that is not
/// installed holds no containers and contributes nothing.
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
            Box::new(v027::batched_running_probe_with(
                move || new().batch_container_states(crate::containers::SANDBOX_NAME_PREFIX),
                move |id| probe_running_with(new(), id),
                announce,
            )) as Box<v027::RunningProbe<'static>>
        })
        .collect();
    move |id: &str| any_live(&probes, id)
}

/// Live if any runtime says so. Short-circuits, so a runtime that cannot
/// answer (and therefore answers "live") keeps the store without the rest
/// being asked.
fn any_live(probes: &[Box<v027::RunningProbe<'_>>], id: &str) -> Result<bool> {
    for probe in probes {
        if probe(id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One runtime's answer for one id, in the shape v027's probe expects: whether
/// it is running, and whether that is the fail-closed substitute for a runtime
/// that could not be asked. A runtime that is not installed answers a
/// definitive "not running": it has no containers to hold this store.
fn probe_running_with(
    runtime: crate::containers::ContainerRuntime,
    id: &str,
) -> Result<(bool, bool)> {
    let name = crate::containers::DockerContainer::generate_name(id);
    match runtime.is_container_running(&name) {
        Ok(running) => Ok((running, false)),
        Err(crate::containers::error::DockerError::NotInstalled) => Ok((false, false)),
        Err(error) if v027::runtime_cannot_answer(&error) => {
            tracing::warn!(
                "sandbox reclaim treating {id} as live: container runtime unavailable ({error})"
            );
            Ok((true, true))
        }
        Err(error) => Err(error.into()),
    }
}

/// App dirs beyond our own whose sessions still claim a store under these
/// roots. Debug and release builds keep separate app dirs but share `$HOME`,
/// and so share the store roots under it: reading only our own registry would
/// call every session of the other build an orphan and delete its credentials.
fn also_owned(app_dir: &Path) -> Vec<PathBuf> {
    crate::session::sibling_namespace_app_dir()
        .filter(|sibling| sibling != app_dir)
        .into_iter()
        .collect()
}

/// Serialise reclaim passes against each other, and hold v027's transition
/// lock for the duration.
///
/// The transition lock is taken *shared*. Exclusive would also block
/// `Storage::update`, which is what publishes the session row for a store
/// being created: holding it exclusively turns the creation race below into a
/// guaranteed loss by preventing the very insert that would mark the store
/// owned. Shared still excludes v027's own planning and publishing, which take
/// it exclusively, and v027's copy phase holds no transition lock at all, so
/// exclusivity buys nothing there either.
///
/// A row that is merely still on the shared store does not block the pass: it
/// owns no private store to reclaim yet, and the one it will own carries its
/// id, which this pass reads as claimed. Blocking on that instead would refuse
/// forever on any machine holding an archived or trashed pre-transition
/// session, since those keep their shared store until they are started again.
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
///
/// Fails rather than answering short. A missing registry file is a profile
/// with no sessions; a registry that exists but cannot be read, parsed, or
/// understood is a profile whose sessions we cannot see, and treating its
/// stores as unowned would delete them.
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

/// Every profile's registry, plus the default one. A `sessions.json` that is
/// present but not a regular file is a registry we cannot read, so it fails
/// the pass rather than being skipped.
fn registry_paths(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![app_dir.to_path_buf()];
    let profiles = app_dir.join("profiles");
    match fs::read_dir(&profiles) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                // Resolved, not `DirEntry::file_type`, which does not follow
                // symlinks: a symlinked profile directory would be skipped and
                // its sessions would read as unowned. Reading more registries
                // is always the safe direction here.
                if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                    dirs.push(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", profiles.display())),
    }
    let mut paths = Vec::new();
    for dir in dirs {
        let path = dir.join("sessions.json");
        // Presence is decided without following, resolution with: a registry
        // that is there but cannot be resolved to a regular file (a dangling
        // symlink, a directory) is one we cannot read, and reading none of it
        // would call its sessions orphans.
        if fs::symlink_metadata(&path).is_err() {
            continue;
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

/// Every `sandbox-v2` root any profile can place a store under: the built-in
/// root per agent config mount, plus the root of each `agent_config_dir` a
/// profile declares.
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
    is_running: &v027::RunningProbe<'_>,
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
            match classify(&path, &id, grace, is_running)? {
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

/// `None` when the store may be removed. Mirrors v027's orphan gate: a path
/// that is not a plain directory says nothing about what it holds, and a
/// running container is still writing to it.
fn classify(
    path: &Path,
    id: &str,
    grace: std::time::Duration,
    is_running: &v027::RunningProbe<'_>,
) -> Result<Option<Preserved>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(Some(Preserved::Ambiguous));
    }
    if written_within(&metadata, grace) {
        return Ok(Some(Preserved::Recent));
    }
    if is_running(id)? {
        return Ok(Some(Preserved::Live));
    }
    Ok(None)
}

/// Whether `metadata` was modified inside `window`. An unreadable or
/// future-dated timestamp counts as recent: it is the arm that keeps the
/// store.
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
    is_running: &v027::RunningProbe<'_>,
) -> Result<Outcome> {
    let plan = plan_in(app_dir, also_owned, home, grace, is_running)?;
    let mut outcome = Outcome {
        plan,
        ..Outcome::default()
    };
    // Sizing the plan can take a while on a large store, and a container may
    // have started since. v027's probe caches its container listing until the
    // liveness epoch moves, and reclaim is the only thing that can move it
    // here, so without this the re-check below would replay the planning-time
    // answer and delete a store that has since been mounted. v027 does the
    // same before it publishes.
    v027::refresh_liveness();
    // Ownership is re-read too: the pass holds the transition lock shared, so
    // a session created during it can publish its row, and a store that was
    // unclaimed at planning time may be claimed by the time we reach it.
    let owned = owned_ids(app_dir, also_owned)?;
    for orphan in &outcome.plan.orphans {
        if owned.contains(&orphan.id) {
            outcome
                .failures
                .push((orphan.path.clone(), "claimed since the scan".to_string()));
            continue;
        }
        // Re-classified against the path as it is now, with fresh liveness:
        // the only thing standing between a swapped or newly live store and
        // `remove_dir_all`.
        match classify(&orphan.path, &orphan.id, grace, is_running) {
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
        // Sizing is advisory, so a subtree that cannot be read is counted as
        // zero rather than failing the report. A store that cannot be read
        // also cannot be removed, and that failure is reported per store.
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
///
/// Returns the paths removed and the bytes they held. A session still on a
/// shared legacy store owns no per-instance directory to remove, and v027 may
/// be publishing the private one it will own, so it is left to the reclaim
/// pass.
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

    /// Tests plant a store and reclaim it in the same millisecond, so the
    /// creation grace period is opted out of except where it is the subject.
    const NO_GRACE: std::time::Duration = std::time::Duration::ZERO;

    fn quiescent(_: &str) -> Result<bool> {
        Ok(false)
    }

    fn live(_: &str) -> Result<bool> {
        Ok(true)
    }

    #[test]
    fn a_store_no_profile_claims_is_an_orphan_and_one_that_is_claimed_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &["1111111111111111"]);
        store(&home, "1111111111111111", 10);
        let orphan = store(&home, "2222222222222222", 40);

        let plan = plan_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap();

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

        let plan = plan_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap();

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
            let error = plan_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap_err();
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
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        store(&home, "2222222222222222", 40);

        let error = plan_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap_err();

        assert!(error.to_string().contains("refusing"), "{error:#}");
    }

    #[test]
    fn a_live_orphan_is_preserved_and_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &live).unwrap();

        assert!(outcome.removed.is_empty());
        assert_eq!(
            outcome.plan.preserved,
            vec![(path.clone(), Preserved::Live)]
        );
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_store_is_preserved_and_its_target_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let target = dir.path().join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), b"keep").unwrap();
        let root = home.join(".claude").join("sandbox-v2");
        fs::create_dir_all(&root).unwrap();
        let link = root.join("2222222222222222");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap();

        assert_eq!(
            outcome.plan.preserved,
            vec![(link.clone(), Preserved::Ambiguous)]
        );
        assert!(target.join("keep").exists());
    }

    #[test]
    fn reclaiming_removes_the_orphan_and_reports_what_it_freed() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &["1111111111111111"]);
        let kept = store(&home, "1111111111111111", 10);
        let orphan = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap();

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
        // Archived and trashed rows keep their shared store until they are
        // started again, so this one is pending for as long as it exists.
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

    /// Debug and release builds keep separate app dirs but share `$HOME`, so
    /// a pass that read only its own registry would delete the credentials of
    /// every session belonging to the other build.
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

        let outcome = reclaim_in(&app, &[sibling], &home, NO_GRACE, &quiescent).unwrap();

        assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
        assert!(store.exists(), "the other build's store was reclaimed");
    }

    /// Container preparation seeds the store before the session row is
    /// inserted, so a store written to moments ago may belong to a session
    /// being created right now rather than to no one.
    #[test]
    fn a_store_being_created_is_preserved_until_the_grace_period_lapses() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let seeding = store(&home, "2222222222222222", 40);

        let outcome = reclaim_in(
            &app,
            &[],
            &home,
            std::time::Duration::from_secs(600),
            &quiescent,
        )
        .unwrap();

        assert!(seeding.exists(), "a store being seeded was reclaimed");
        assert_eq!(
            outcome.plan.preserved,
            vec![(seeding, Preserved::Recent)],
            "and the report says why"
        );
    }

    /// A store unclaimed when the plan was made can be claimed by the time the
    /// pass reaches it: the transition lock is held shared precisely so that
    /// insert is not blocked, so the delete phase has to look again.
    #[test]
    fn a_store_claimed_after_the_scan_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);

        // The probe runs between planning and deletion, which is where a
        // concurrent `aoe add` would publish its row.
        let claim_on_probe = |_: &str| {
            app_with_rows(&app, &["2222222222222222"]);
            Ok(false)
        };
        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &claim_on_probe).unwrap();

        assert!(path.exists(), "a store claimed mid-pass was reclaimed");
        assert!(outcome.removed.is_empty());
    }

    /// A container of a runtime this build's config does not name still holds
    /// the store it has mounted, so every runtime is asked and any one of them
    /// saying live is enough.
    #[test]
    fn liveness_is_the_union_of_every_runtime_asked() {
        let quiet: Box<v027::RunningProbe<'_>> = Box::new(|_| Ok(false));
        let busy: Box<v027::RunningProbe<'_>> = Box::new(|_| Ok(true));
        let angry: Box<v027::RunningProbe<'_>> =
            Box::new(|_| Err(anyhow::anyhow!("runtime exploded")));

        assert!(!any_live(&[], "1111111111111111").unwrap());
        assert!(!any_live(std::slice::from_ref(&quiet), "1111111111111111").unwrap());
        assert!(any_live(&[quiet, busy, angry], "1111111111111111").unwrap());

        let angry: Box<v027::RunningProbe<'_>> =
            Box::new(|_| Err(anyhow::anyhow!("runtime exploded")));
        assert!(
            any_live(&[angry], "1111111111111111").is_err(),
            "a real fault must fail the pass, not read as quiescent"
        );
    }

    /// A store live under a runtime this build does not use must survive.
    #[test]
    fn a_store_live_under_another_runtime_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let path = store(&home, "2222222222222222", 40);
        let probes: Vec<Box<v027::RunningProbe<'_>>> =
            vec![Box::new(|_| Ok(false)), Box::new(|_| Ok(true))];
        let any = move |id: &str| any_live(&probes, id);

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &any).unwrap();

        assert!(
            path.exists(),
            "a store live under another runtime was removed"
        );
        assert_eq!(outcome.plan.preserved, vec![(path, Preserved::Live)]);
    }

    /// `remove_stores_for` must not follow a symlink out of the store root.
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

    #[test]
    fn a_directory_that_is_not_an_instance_id_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let home = dir.path().join("home");
        fs::create_dir_all(&app).unwrap();
        app_with_rows(&app, &[]);
        let staging = home
            .join(".claude")
            .join("sandbox-v2")
            .join(".v027-staging");
        fs::create_dir_all(&staging).unwrap();

        let outcome = reclaim_in(&app, &[], &home, NO_GRACE, &quiescent).unwrap();

        assert!(staging.exists());
        assert!(outcome.plan.orphans.is_empty());
    }
}
