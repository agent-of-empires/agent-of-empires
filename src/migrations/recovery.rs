use anyhow::Result;

pub(super) fn reconcile_repaired_components() -> Result<()> {
    super::v038_prime_native_runtime::run()?;
    super::v039_prime_control_path::run()?;
    super::v040_worker_ownership::run()?;
    super::v045_runner_scope_ownership::run()?;
    super::v046_reconcile_runtime_lineages::reconcile_sidebar_settings()
}

pub(crate) fn profile_locks() -> Result<Vec<crate::session::StorageFlock>> {
    use anyhow::Context;
    use std::collections::{BTreeMap, BTreeSet};
    let app = crate::session::get_app_dir()?;
    let mut files = BTreeMap::new();
    for path in super::legacy_scope_recovery::session_paths(&app)? {
        let directory = path
            .parent()
            .context("Session file has no parent")?
            .canonicalize()?;
        let file = crate::session::ResolvedDataFile::open(&path)?;
        let raw = file.read()?;
        let rows: Vec<crate::session::Instance> = match raw.as_deref() {
            Some(raw) => serde_json::from_str(raw).with_context(|| {
                format!("Inventorying {} for ownership recovery", path.display())
            })?,
            None => Vec::new(),
        };
        let ids: BTreeSet<_> = rows.into_iter().map(|row| row.id).collect();
        if let Some((_, prior_raw, prior_ids)) = files.get(&directory) {
            anyhow::ensure!(
                prior_raw == &raw && prior_ids == &ids,
                "Aliased recovery profile inventories differ"
            );
        } else {
            files.insert(directory, (file, raw, ids));
        }
    }
    let mut locks = Vec::new();
    for (directory, (file, raw, ids)) in files {
        for id in ids {
            locks.push(crate::session::acquire_migration_lifecycle_lock(
                &directory, &id,
            )?);
        }
        locks.push(crate::session::acquire_recovery_storage_flock(
            &directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?);
        anyhow::ensure!(
            file.read()? == raw,
            "Profile changed before ownership recovery; retry aoe migrate"
        );
    }
    Ok(locks)
}
