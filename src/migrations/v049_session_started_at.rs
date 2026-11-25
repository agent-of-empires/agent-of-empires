//! Registered `Instance.started_at`, retired by v064.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v049: session started_at is recorded on the next launch; existing rows stay unset"
    );
    Ok(())
}
