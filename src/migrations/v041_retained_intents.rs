use anyhow::{Context, Result};

pub(crate) fn is_legacy_schema(bytes: Option<&[u8]>) -> Result<bool> {
    let Some(bytes) = bytes else { return Ok(true) };
    let version: u32 = std::str::from_utf8(bytes)?
        .trim()
        .parse()
        .context("unreadable data schema version")?;
    Ok(version < 41)
}

pub fn run() -> Result<()> {
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    crate::session::retained_intents::initialize_legacy_in(&crate::session::get_app_dir()?)?;
    tracing::info!(target: "migrations", "initialized permanent retained filesystem intent ownership");
    Ok(())
}
