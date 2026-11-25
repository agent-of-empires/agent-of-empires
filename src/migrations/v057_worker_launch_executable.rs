//! Registered `WorkerRecord.launch_executable` and `launch_resolved_executable`, since retired.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v057: worker launch executable is recorded on the next spawn; existing records stay unset"
    );
    Ok(())
}
