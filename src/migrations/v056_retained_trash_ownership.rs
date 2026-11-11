use anyhow::Result;

pub fn run() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    crate::session::purge_owners::migrate_retained_ownership(&root)?;
    tracing::info!(target: "migrations.v055", "Migrated cleanup ownership purposes");
    Ok(())
}
