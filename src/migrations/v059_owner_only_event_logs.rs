//! Event logs were created world-readable. Restrict every existing event log, its WAL and SHM
//! sidecars and its backups to the owner.

use anyhow::Result;

const PREFIXES: [&str; 3] = ["acp_events.db", "plugin_events.db", "cockpit_events.db"];

#[cfg(unix)]
fn restrict(dir: &std::path::Path) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut changed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !PREFIXES.iter().any(|p| name.starts_with(p)) || !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let mode = std::fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(not(unix))]
fn restrict(_dir: &std::path::Path) -> Result<usize> {
    Ok(0)
}

pub fn run() -> Result<()> {
    let changed = restrict(&crate::session::get_app_dir()?)?;
    tracing::info!(target: "migrations", "v059: restricted {changed} event log file(s) to owner-only");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restricts_event_logs_sidecars_and_backups_only() {
        let dir = tempfile::tempdir().unwrap();
        let names = [
            "acp_events.db",
            "acp_events.db-wal",
            "acp_events.db-shm",
            "plugin_events.db",
            "cockpit_events.db.bak",
            "cockpit_events.db-wal.bak",
            "other.txt",
        ];
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(super::restrict(dir.path()).unwrap(), 6);
        assert_eq!(super::restrict(dir.path()).unwrap(), 0);
        for name in names {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode,
                if name == "other.txt" { 0o644 } else { 0o600 },
                "{name}"
            );
        }
    }
}
