//! Persists the pre-v034 launch record's `core_only` exposure policy.
//!
//! Takes the daemon lifecycle transaction, so this migration must only ever run
//! in a process that does not already hold that lock. The detached daemon
//! child receives the lock from its parent and therefore never migrates; it
//! checks the schema instead (`migrations::assert_schema_current`).

use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    let _transaction = crate::daemon::lifecycle::Transaction::acquire_blocking()?;
    let dir = crate::session::get_app_dir()?;
    for name in ["serve.launch", "serve.rollback.launch"] {
        let path = dir.join(name);
        let content = match std::fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut launch: serde_json::Value = serde_json::from_slice(&content)?;
        let object = launch
            .as_object_mut()
            .context("Invalid daemon launch record")?;
        if object.contains_key("core_only") {
            continue;
        }
        object.insert("core_only".into(), false.into());
        object.insert("schema".into(), 3.into());
        crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&launch)?)?;
        tracing::info!("Migrated persisted daemon exposure policy");
    }
    Ok(())
}
