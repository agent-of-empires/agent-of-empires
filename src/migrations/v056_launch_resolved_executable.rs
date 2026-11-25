//! Registered `Instance.launch_resolved_executable`, retired by v064.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v056: session launch_resolved_executable is recorded on the next launch; existing rows stay unset"
    );
    Ok(())
}
