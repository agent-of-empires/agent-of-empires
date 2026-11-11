use anyhow::Result;

pub fn run() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    crate::session::purge_owners::migrate_unpublished_authority(&root)?;
    tracing::info!(target: "migrations.v056", "Migrated unpublished session authority");
    Ok(())
}
