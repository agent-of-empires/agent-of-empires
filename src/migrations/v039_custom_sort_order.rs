//! Migration v039: `state.toml` can hold `sort_order = "custom"`, which a build
//! before this one cannot decode. Nothing is rewritten; the version step is the
//! change, so such a build refuses to start instead of falling back to defaults.

use anyhow::Result;
use tracing::info;

pub fn run() -> Result<()> {
    info!(target: "migrations", "v039: schema step for the custom sort order, nothing to rewrite");
    Ok(())
}
