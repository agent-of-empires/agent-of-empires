use anyhow::Result;

pub fn run() -> Result<()> {
    let root = crate::session::get_app_dir()?;
    if !root
        .join(crate::session::purge_owners::FILE_NAME)
        .try_exists()?
    {
        crate::session::purge_owners::initialize(&root)?;
    }
    tracing::info!(target: "migrations.v041", "Initialized durable pending purge ownership");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    #[serial_test::serial]
    fn initialization_respects_the_migration_log_target() {
        let _home = crate::session::test_support::isolate_app_dir();
        for (filter, visible) in [
            ("agent_of_empires=off,migrations=info", true),
            ("agent_of_empires=off,migrations=off", false),
        ] {
            let output = tempfile::NamedTempFile::new().unwrap();
            let writer = output.reopen().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter(filter)
                .with_writer(writer)
                .finish();
            tracing::subscriber::with_default(subscriber, || super::run().unwrap());
            let logs = std::fs::read_to_string(output.path()).unwrap();
            assert_eq!(
                logs.contains("Initialized durable pending purge ownership"),
                visible
            );
        }
    }
}
