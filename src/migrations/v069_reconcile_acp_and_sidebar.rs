//! Reconcile both meanings of the published v068 marker.

pub fn run() -> anyhow::Result<()> {
    super::v068_reconcile_acp_lineages::run()?;
    super::v068_shared_sidebar_view::reconcile()?;
    tracing::info!(target: "migrations.v069", "Reconciled ACP history and sidebar preferences");
    Ok(())
}
