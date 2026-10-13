use anyhow::Result;

pub fn run() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    crate::session::purge_owners::initialize(&root)?;
    tracing::info!("Initialized durable pending purge ownership");
    Ok(())
}
