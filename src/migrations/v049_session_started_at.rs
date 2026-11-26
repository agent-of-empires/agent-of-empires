//! Registered `Instance.started_at`, retired by v066.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!("v049: no data to change");
    Ok(())
}
