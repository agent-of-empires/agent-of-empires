//! Event logs were created world-readable. Restrict every existing event log, its WAL and SHM
//! sidecars and its backups to the owner.

use anyhow::Result;

const PREFIXES: [&str; 3] = ["acp_events.db", "plugin_events.db", "cockpit_events.db"];

/// Follows symlinks. A missing path has nothing to restrict; any other error aborts so the
/// migration is retried instead of stamped complete over a readable log.
#[cfg(unix)]
fn restrict_file(path: &std::path::Path) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    if !meta.is_file() || meta.permissions().mode() & 0o077 == 0 {
        return Ok(0);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(1)
}

/// Follows symlink hops without requiring the final target to exist. Like Linux, it follows 40
/// links and refuses a 41st.
#[cfg(unix)]
fn link_target(path: &std::path::Path) -> Result<std::path::PathBuf> {
    const MAX_LINKS: usize = 40;
    let mut current = path.to_path_buf();
    for hops in 0..=MAX_LINKS {
        if !std::fs::symlink_metadata(&current)?
            .file_type()
            .is_symlink()
        {
            return Ok(current);
        }
        if hops == MAX_LINKS {
            break;
        }
        let link = std::fs::read_link(&current)?;
        current = current
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .join(link);
        if matches!(std::fs::symlink_metadata(&current), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(current);
        }
    }
    anyhow::bail!("too many symlink hops resolving {}", path.display())
}

#[cfg(unix)]
fn restrict(dir: &std::path::Path) -> Result<usize> {
    let mut changed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let path = entry.path();
        changed += restrict_file(&path)?;
        // SQLite names sidecars after the link's target, which may be missing while they survive.
        let target = link_target(&path)?;
        if target != path {
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = target.clone().into_os_string();
                sidecar.push(suffix);
                changed += restrict_file(sidecar.as_ref())?;
            }
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
        let target = dir.path().join("history.sqlite");
        std::fs::write(&target, b"x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("cockpit_events.db")).unwrap();
        let target_wal = dir.path().join("history.sqlite-wal");
        std::fs::write(&target_wal, b"x").unwrap();
        std::fs::set_permissions(&target_wal, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(super::restrict(dir.path()).unwrap(), 8);
        assert_eq!(super::restrict(dir.path()).unwrap(), 0);
        for restricted in [&target, &target_wal] {
            assert_eq!(
                std::fs::metadata(restricted).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

    #[test]
    fn unreadable_log_aborts_then_succeeds_after_repair() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("acp_events.db");
        std::os::unix::fs::symlink(&link, &link).unwrap();
        let sibling = dir.path().join("plugin_events.db");
        std::fs::write(&sibling, b"x").unwrap();
        std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(super::restrict(dir.path()).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"x").unwrap();
        std::fs::set_permissions(&link, std::fs::Permissions::from_mode(0o644)).unwrap();
        // A failed pass may already have restricted the sibling, depending on directory order.
        super::restrict(dir.path()).unwrap();
        assert_eq!(super::restrict(dir.path()).unwrap(), 0);
        for path in [&link, &sibling] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }

    #[test]
    fn a_target_forty_links_away_is_restricted_and_a_forty_first_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // The kernel counts every link it follows, and each absolute hop would re-walk a
        // symlinked TMPDIR.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let target = root.join("history.sqlite");
        let files = ["", "-wal", "-shm"].map(|suffix| root.join(format!("history.sqlite{suffix}")));
        for file in &files {
            std::fs::write(file, b"x").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let modes = || {
            files
                .iter()
                .map(|file| std::fs::metadata(file).unwrap().permissions().mode() & 0o777)
                .collect::<Vec<_>>()
        };
        // `acp_events.db` is the first of `links` links, the last of which names `target`.
        let link = root.join("acp_events.db");
        let chain = |links: usize| {
            let hop = |n: usize| match n {
                1 => link.clone(),
                n => root.join(format!("hop-{n}")),
            };
            for n in 1..=41 {
                let _ = std::fs::remove_file(hop(n));
            }
            for n in 1..=links {
                let next = if n == links {
                    target.clone()
                } else {
                    hop(n + 1)
                };
                std::os::unix::fs::symlink(next, hop(n)).unwrap();
            }
        };

        // The kernel refuses the 41st link too, so the refusal comes before any change.
        chain(41);
        assert!(super::link_target(&link).is_err());
        assert!(super::restrict(&root).is_err());
        assert_eq!(modes(), [0o644; 3], "nothing restricted");

        // The 40th link's target is accepted, and a rerun after the repair completes. macOS
        // resolves only 32 links, so `stat` fails on this chain there.
        chain(40);
        assert_eq!(super::link_target(&link).unwrap(), target);
        if cfg!(target_os = "linux") {
            assert_eq!(super::restrict(&root).unwrap(), 3);
            assert_eq!(modes(), [0o600; 3], "the database and both sidecars");
        }
    }

    #[test]
    fn restricts_surviving_sidecars_of_a_missing_link_target() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("history.sqlite", dir.path().join("acp_events.db")).unwrap();
        let wal = dir.path().join("history.sqlite-wal");
        std::fs::write(&wal, b"x").unwrap();
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(super::restrict(dir.path()).unwrap(), 1);
        let mode = std::fs::metadata(&wal).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
