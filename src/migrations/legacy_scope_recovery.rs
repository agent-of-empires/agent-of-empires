use crate::process::worker_registry::ScopeOwnership;
use crate::session::{AnchoredDir, ResolvedDataFile};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(super) fn run_under_exclusion(
    transaction: &crate::daemon::lifecycle::Transaction,
    serve: &crate::cli::serve::ServeLock,
) -> Result<()> {
    crate::cli::serve::with_inactive_serve_state(transaction, Some(serve), |_| recover_components())
}

struct SessionPlan {
    path: PathBuf,
    file: ResolvedDataFile,
    original: Option<String>,
    rows: Vec<Value>,
    encoded: Option<Vec<u8>>,
    primary: bool,
}

struct WorkerPlan {
    path: PathBuf,
    file: ResolvedDataFile,
    original: String,
    document: Value,
    encoded: Option<Vec<u8>>,
}

pub(super) fn session_paths(app: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![app.join("sessions.json")];
    let profiles_path = app.join("profiles");
    match std::fs::symlink_metadata(&profiles_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error).context("Inspecting recovery profile namespace"),
        Ok(_) => {}
    }
    let profiles = AnchoredDir::open(&profiles_path)?;
    for name in profiles.read_dir(Path::new(""), usize::MAX)? {
        let path = profiles_path.join(&name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_file() {
            continue;
        }
        let profile = name.to_str().context("Non-UTF8 profile name")?;
        crate::session::validate_profile_name(profile)?;
        // Follow only the existing user-managed profile reference convention.
        AnchoredDir::open_reference(&path)?;
        paths.push(path.join("sessions.json"));
    }
    paths.sort();
    Ok(paths)
}

fn recover_components() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    let _identity = crate::session::acquire_recovery_identity_lock()?;
    let mut sessions: Vec<SessionPlan> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut lifecycle_slots = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for path in session_paths(&app)? {
        let directory = path
            .parent()
            .context("Session file has no parent")?
            .canonicalize()?;
        let file = ResolvedDataFile::open(&path)?;
        let original = file.read()?;
        let rows: Vec<Value> = match original.as_deref() {
            Some(raw) => serde_json::from_str(raw).with_context(|| {
                format!("Reading complete session component {}", path.display())
            })?,
            None => Vec::new(),
        };
        let mut local_ids = BTreeSet::new();
        for value in &rows {
            let row = crate::session::Instance::deserialize(value)
                .with_context(|| format!("Invalid session row in {}", path.display()))?;
            crate::session::validate_instance_id(&row.id)?;
            anyhow::ensure!(
                local_ids.insert(row.id.clone()),
                "Duplicate row {} in {}",
                row.id,
                path.display()
            );
            if let Some(previous) = seen.get(&row.id) {
                anyhow::ensure!(
                    file.same_target(&sessions[*previous].file)?
                        && original == sessions[*previous].original,
                    "Session {} has ambiguous ownership across profile components",
                    row.id
                );
            } else {
                seen.insert(row.id.clone(), sessions.len());
            }
            lifecycle_slots.insert((directory.clone(), row.id));
        }
        directories.insert(directory);
        let primary = !sessions
            .iter()
            .try_fold(false, |found, prior| -> Result<bool> {
                Ok(found || file.same_target(&prior.file)?)
            })?;
        sessions.push(SessionPlan {
            path,
            file,
            original,
            rows,
            encoded: None,
            primary,
        });
    }
    let journal_inventory = crate::session::purge_owners::scope_recovery_inventory()?;
    for (profile, id) in &journal_inventory.owners {
        crate::session::validate_profile_name(profile)?;
        let store = crate::session::Storage::open_unwatched(profile)?;
        let directory = store
            .sessions_path()
            .parent()
            .context("Pending owner lacks profile directory")?
            .canonicalize()?;
        lifecycle_slots.insert((directory.clone(), id.clone()));
        directories.insert(directory);
    }
    let _lifecycles = lifecycle_slots
        .iter()
        .map(|(directory, id)| crate::session::acquire_migration_lifecycle_lock(directory, id))
        .collect::<Result<Vec<_>>>()?;
    let _profiles = directories
        .iter()
        .map(|directory| {
            crate::session::acquire_recovery_storage_flock(
                directory,
                crate::session::STORAGE_LOCK_FILENAME,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    for plan in &sessions {
        anyhow::ensure!(
            plan.file.read()? == plan.original,
            "Session component changed before scope recovery: {}",
            plan.path.display()
        );
    }
    // Pin original workers while planning field-only scope recovery.
    let mut workers = Vec::new();
    let mut worker_locks = Vec::new();
    let workers_path = app.join("acp-workers");
    match std::fs::symlink_metadata(&workers_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Inspecting worker namespace"),
        Ok(_) => {
            let directory = AnchoredDir::open(&workers_path)?;
            let mut names = directory.read_dir(Path::new(""), usize::MAX)?;
            names.sort();
            for name in names {
                let path = Path::new(&name);
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let id = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .context("Invalid worker component name")?;
                crate::process::worker_registry::validate_session_id(id)?;
                let lock = crate::process::worker_registry::acquire_registry_lock(id)?;
                let file = directory.bind_file(&name)?;
                let raw = file.read()?.context("Worker component disappeared")?;
                let document: Value = serde_json::from_str(&raw)
                    .with_context(|| format!("Invalid worker component {}", path.display()))?;
                let record = crate::process::worker_registry::WorkerRecord::deserialize(&document)
                    .with_context(|| format!("Invalid worker component {}", path.display()))?;
                anyhow::ensure!(
                    record.session_id == id,
                    "Worker component identity does not match its file name"
                );
                workers.push(WorkerPlan {
                    path: workers_path.join(&name),
                    file,
                    original: raw,
                    document,
                    encoded: None,
                });
                worker_locks.push(lock);
            }
        }
    }
    // Finish all kernel/manager proof and all plans before the first scope write.
    for plan in &mut sessions {
        if plan.primary && resolve_rows_with(&mut plan.rows, inventory_clear)? {
            plan.encoded = Some(serde_json::to_vec(&plan.rows)?);
        }
    }
    for plan in &mut workers {
        if resolve_worker_with(&mut plan.document, inventory_clear)? {
            plan.encoded = Some(serde_json::to_vec(&plan.document)?);
        }
    }
    let journal = crate::session::purge_owners::plan_scope_recovery(journal_inventory)?;
    for plan in &workers {
        anyhow::ensure!(
            plan.file.read()?.as_deref() == Some(plan.original.as_str()),
            "Worker component changed before scope recovery"
        );
        if plan.encoded.is_some() {
            plan.file.require_writable_existing()?;
        }
    }
    for plan in &sessions {
        anyhow::ensure!(
            plan.file.read()? == plan.original,
            "Session component changed during scope proof: {}",
            plan.path.display()
        );
        if plan.encoded.is_some() {
            plan.file.require_writable_existing()?;
        }
    }
    journal.check_current()?;
    // Back up the complete original authority for every planned rewrite first.
    for plan in &sessions {
        if plan.encoded.is_some() {
            crate::session::backup_before_migration(&plan.path)?;
            AnchoredDir::open(plan.path.parent().unwrap())?.sync()?;
        }
    }
    journal.backup()?;
    for plan in &workers {
        if plan.encoded.is_some() {
            crate::session::backup_before_migration(&plan.path)?;
            AnchoredDir::open(plan.path.parent().context("Worker path has no parent")?)?.sync()?;
        }
    }
    for plan in &sessions {
        if let Some(encoded) = &plan.encoded {
            plan.file.replace(encoded)?;
            plan.file.sync_parent()?;
        }
    }
    for plan in &workers {
        if let Some(encoded) = &plan.encoded {
            plan.file.replace(encoded)?;
            plan.file.sync_parent()?;
        }
    }
    journal.commit()?;
    drop(worker_locks);
    tracing::info!(target: "migrations.scope_recovery", "Completed explicit legacy scope authority recovery");
    Ok(())
}

fn inventory_clear(id: &str, scopes: &[ScopeOwnership]) -> Result<bool> {
    #[cfg(test)]
    if test_support::inventory_denied() {
        anyhow::bail!("injected legacy scope inventory unavailable");
    }
    crate::process::systemd::legacy_unknown_scope_absent(id, scopes)
}

#[cfg(test)]
pub(super) mod test_support {
    thread_local! {
        static DENIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    pub(super) fn inventory_denied() -> bool {
        if !DENIED.get() {
            return false;
        }
        ATTEMPTS.set(ATTEMPTS.get() + 1);
        true
    }
    pub(crate) struct Denied;
    impl Denied {
        pub(crate) fn attempts(&self) -> usize {
            ATTEMPTS.get()
        }
    }
    impl Drop for Denied {
        fn drop(&mut self) {
            DENIED.set(false);
        }
    }
    pub(crate) fn deny_inventory() -> Denied {
        assert!(!DENIED.replace(true));
        ATTEMPTS.set(0);
        Denied
    }
}

fn resolve_worker_with(
    document: &mut Value,
    inventory_clear: impl Fn(&str, &[ScopeOwnership]) -> Result<bool>,
) -> Result<bool> {
    let record = crate::process::worker_registry::WorkerRecord::deserialize(&*document)?;
    if record.scope != ScopeOwnership::Unresolved {
        return Ok(false);
    }
    anyhow::ensure!(inventory_clear(&record.session_id, &[])?,
        "Worker {} still has a live unknown legacy scope; independently establish and retire the exact original unit before retrying aoe migrate", record.session_id);
    document["scope"] = serde_json::to_value(ScopeOwnership::Unscoped)?;
    Ok(true)
}

fn resolve_rows_with(
    rows: &mut [Value],
    inventory_clear: impl Fn(&str, &[ScopeOwnership]) -> Result<bool>,
) -> Result<bool> {
    let mut changed = false;
    for value in rows {
        let row = crate::session::Instance::deserialize(&*value)?;
        if row.is_structured() {
            continue;
        }
        let unknown = row
            .terminal_scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(&ScopeOwnership::Unresolved));
        if !unknown {
            continue;
        }
        anyhow::ensure!(
            row.lifecycle_reservation.is_none(),
            "Session {} has an unfinished constructor or lifecycle owner; scope recovery refused",
            row.id
        );
        let retained: Vec<_> = row
            .terminal_scopes
            .unwrap_or_default()
            .into_iter()
            .filter(|scope| *scope != ScopeOwnership::Unresolved)
            .collect();
        anyhow::ensure!(inventory_clear(&row.id, &retained)?,
            "Session {} still has a live unknown legacy scope; discover the exact original unit, independently establish ownership, retire that exact unit, then rerun aoe migrate", row.id);
        value["terminal_scopes"] = serde_json::to_value(retained)?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn recovered_terminal_scope_debt_unlocks_stop_and_purge_without_new_custody() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let mut row = crate::session::Instance::new("legacy", "/tmp/repo");
        row.view = crate::session::View::Terminal;
        row.terminal_scopes = None;
        assert!(row.release_scope().is_err());
        let before = crate::session::purge_owners::PurgeCapture::new(&row)?;
        assert!(before.ensure_captured_runner_stopped().is_err());
        let mut rows = vec![serde_json::to_value(&row)?];
        let original = rows.clone();
        assert!(resolve_rows_with(&mut rows, |_, _| Ok(false)).is_err());
        assert_eq!(rows, original);
        assert!(resolve_rows_with(&mut rows, |_, _| Ok(true))?);
        let recovered: crate::session::Instance = serde_json::from_value(rows.remove(0))?;
        assert_eq!(recovered.id, row.id);
        assert_eq!(recovered.lifecycle_generation, row.lifecycle_generation);
        recovered.release_scope()?;
        crate::session::purge_owners::PurgeCapture::new(&recovered)?
            .ensure_captured_runner_stopped()?;
        Ok(())
    }

    #[test]
    fn worker_scope_recovery_preserves_physical_and_opaque_authority() -> Result<()> {
        let mut record = crate::process::worker_registry::WorkerRecord::new(
            "old-worker".into(),
            std::process::id(),
            "/tmp/old-worker.sock".into(),
            "agent".into(),
            "agent".into(),
            "/tmp/repo".into(),
            None,
            vec![],
            vec![],
            None,
            Some("default".into()),
        )
        .with_generation(71);
        record.scope = ScopeOwnership::Unresolved;
        let mut document = serde_json::to_value(&record)?;
        document["opaque_original_authority"] = serde_json::json!({"keep": 19});
        let before = document.clone();
        assert!(resolve_worker_with(&mut document, |_, _| Ok(false)).is_err());
        assert_eq!(document, before);
        assert!(resolve_worker_with(&mut document, |_, _| anyhow::bail!("denied")).is_err());
        assert_eq!(document, before);
        assert!(resolve_worker_with(&mut document, |id, retained| {
            assert_eq!(id, "old-worker");
            assert!(retained.is_empty());
            Ok(true)
        })?);
        let mut expected = before;
        expected["scope"] = serde_json::to_value(ScopeOwnership::Unscoped)?;
        assert_eq!(document, expected);
        assert!(!resolve_worker_with(&mut document, |_, _| panic!(
            "settled proof must not be reacquired"
        ))?);
        Ok(())
    }

    #[test]
    fn scope_proof_is_strict_and_retains_every_original_owned_unit() -> Result<()> {
        let mut row = serde_json::to_value(crate::session::Instance::new("old", "/tmp/repo"))?;
        row["terminal_scopes"] = Value::Null;
        row["opaque"] = serde_json::json!({"keep":17});
        let owned = ScopeOwnership::Owned {
            unit: "aoe-session-old-original.scope".into(),
        };
        let mut started = row.clone();
        started["terminal_scopes"] =
            serde_json::to_value(vec![ScopeOwnership::Unresolved, owned.clone()])?;
        let mut rows = vec![row, started];
        let before = rows.clone();
        assert!(resolve_rows_with(&mut rows, |_, _| Ok(false)).is_err());
        assert_eq!(rows, before);
        assert!(resolve_rows_with(&mut rows, |_, _| anyhow::bail!("manager denied")).is_err());
        assert_eq!(rows, before);
        assert!(resolve_rows_with(&mut rows, |_, _| Ok(true))?);
        let first: crate::session::Instance = serde_json::from_value(rows[0].clone())?;
        let second: crate::session::Instance = serde_json::from_value(rows[1].clone())?;
        assert_eq!(first.terminal_scopes, Some(Vec::new()));
        assert_eq!(second.terminal_scopes, Some(vec![owned]));
        assert_eq!(rows[0]["opaque"], serde_json::json!({"keep":17}));
        let completed = rows.clone();
        assert!(!resolve_rows_with(&mut rows, |_, _| panic!(
            "settled authority needs no inventory"
        ))?);
        assert_eq!(rows, completed);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn a_later_invalid_component_prevents_every_scope_authority_write() -> Result<()> {
        for malformed in ["{", "{}", "[null]"] {
            let home = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            crate::session::purge_owners::initialize(&app)?;
            let good = crate::session::Storage::new_unwatched("a-good")?;
            let bad = crate::session::Storage::new_unwatched("z-bad")?;
            let mut row = crate::session::Instance::new("old", "/tmp/repo");
            row.view = crate::session::View::Terminal;
            row.terminal_scopes = None;
            good.update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })?;
            std::fs::write(bad.sessions_path(), malformed)?;
            let good_before = std::fs::read(good.sessions_path())?;
            let journal_before = std::fs::read(app.join(crate::session::purge_owners::FILE_NAME))?;
            let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
                std::time::Duration::from_secs(1),
            )?;
            let serve = crate::cli::serve::try_lock_serve()?.unwrap();
            assert!(crate::cli::serve::with_inactive_serve_state(
                &transaction,
                Some(&serve),
                |_| recover_components()
            )
            .is_err());
            assert_eq!(std::fs::read(good.sessions_path())?, good_before);
            assert_eq!(std::fs::read(bad.sessions_path())?, malformed.as_bytes());
            assert_eq!(
                std::fs::read(app.join(crate::session::purge_owners::FILE_NAME))?,
                journal_before
            );
        }
        Ok(())
    }
}
