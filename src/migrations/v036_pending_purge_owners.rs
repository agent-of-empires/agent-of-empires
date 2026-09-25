use anyhow::Result;

pub fn run() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    super::v037_capture_purge_runners::migrate(&root, true)?;
    tracing::info!("Initialized durable pending purge ownership");
    Ok(())
}
