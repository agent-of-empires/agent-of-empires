//! Mark pre-existing runtime identities as unknown rather than assigning a reused PID's birth.

use anyhow::Result;

pub fn run() -> Result<()> {
    let changed = crate::process::worker_registry::migrate_kernel_identities()?;
    if changed > 0 {
        tracing::info!("v041: cleared {changed} legacy runtime process identity claims");
    }
    Ok(())
}
