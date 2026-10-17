//! Mark pre-existing runtime identities as unknown rather than assigning a reused PID's birth.

use anyhow::Result;

pub fn run() -> Result<()> {
    super::v042_lifecycle_cutover::run()
}
