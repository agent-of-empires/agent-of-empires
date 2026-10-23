use anyhow::Result;

pub fn run() -> Result<()> {
    crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir()?)?;
    tracing::info!(target: "migrations.v047", "Initialized durable original tool retirement journal");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    #[serial_test::serial]
    fn initializes_old_state_idempotently_and_preserves_corruption() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        let path = root.join(crate::tmux::tool_retirement::FILE_NAME);
        assert!(!path.exists());
        super::run().unwrap();
        let first = std::fs::read(&path).unwrap();
        super::run().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        std::fs::write(&path, "broken original evidence").unwrap();
        assert!(super::run().is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "broken original evidence"
        );
    }

    #[test]
    #[serial_test::serial]
    fn schema_46_upgrade_initializes_journal_but_current_schema_never_recreates_it() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        std::fs::write(root.join(super::super::VERSION_FILE), "46").unwrap();
        super::super::run_migrations().unwrap();
        assert_eq!(
            super::super::get_current_version(),
            super::super::CURRENT_VERSION
        );
        let path = root.join(crate::tmux::tool_retirement::FILE_NAME);
        assert!(path.is_file());
        std::fs::remove_file(&path).unwrap();
        super::super::run_migrations().unwrap();
        assert!(!path.exists());
        assert!(crate::tmux::ToolSession::pending_retirement_targets().is_err());
    }
}
