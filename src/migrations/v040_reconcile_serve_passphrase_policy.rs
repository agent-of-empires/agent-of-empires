//! Reconcile persisted daemon launch policy with the current schema.

pub fn run() -> anyhow::Result<()> {
    super::v035_serve_passphrase_policy::run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn skipped_policy_upgrade_preserves_identity_and_login_policy() -> anyhow::Result<()> {
        for version in 35..=39 {
            for (auth, secret, protected) in [
                ("none", None, false),
                ("token", Some("secondary login"), true),
                ("passphrase", None, true),
            ] {
                let temp = tempfile::tempdir()?;
                let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
                let root = crate::session::get_app_dir()?;
                std::fs::write(root.join(".schema_version"), version.to_string())?;
                if version >= 36 {
                    std::fs::write(
                        root.join("pending-purge-owners.json"),
                        serde_json::to_vec(
                            &serde_json::json!({"version": if version == 36 { 1 } else { 2 }, "owners": []}),
                        )?,
                    )?;
                }
                let launch = serde_json::json!({"schema":2,"pid":42,"instance_id":"retained-instance","auth_mode":auth,"remote":false});
                for name in ["serve.launch", "serve.rollback.launch"] {
                    std::fs::write(root.join(name), serde_json::to_vec(&launch)?)?;
                }
                if let Some(secret) = secret {
                    std::fs::write(root.join("serve.passphrase"), secret)?;
                }
                crate::migrations::run_migrations()?;
                for name in ["serve.launch", "serve.rollback.launch"] {
                    let migrated: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(root.join(name))?)?;
                    assert_eq!(
                        migrated["has_passphrase"], protected,
                        "version={version}, auth={auth}"
                    );
                    assert_eq!(migrated["pid"], launch["pid"]);
                    assert_eq!(migrated["instance_id"], launch["instance_id"]);
                    assert_eq!(migrated["auth_mode"], launch["auth_mode"]);
                }
                let credential: serde_json::Value = serde_json::from_slice(&std::fs::read(
                    root.join("serve.rollback.passphrase"),
                )?)?;
                assert_eq!(credential["pid"], launch["pid"]);
                assert_eq!(credential["instance_id"], launch["instance_id"]);
                assert_eq!(credential["passphrase"].as_str(), secret);
                let retained = std::fs::read(root.join("serve.rollback.passphrase"))?;
                run()?;
                assert_eq!(
                    std::fs::read(root.join("serve.rollback.passphrase"))?,
                    retained
                );
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn malformed_launch_does_not_advance_the_schema() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let root = crate::session::get_app_dir()?;
        std::fs::write(root.join(".schema_version"), "39")?;
        std::fs::write(root.join("serve.launch"), "not JSON")?;
        assert!(crate::migrations::run_migrations().is_err());
        assert_eq!(std::fs::read_to_string(root.join(".schema_version"))?, "39");
        assert_eq!(
            std::fs::read_to_string(root.join("serve.launch"))?,
            "not JSON"
        );
        Ok(())
    }
}
