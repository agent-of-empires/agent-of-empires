//! Small crate-internal utilities shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Whether `url` is a plain web URL, the only kind aoe hands to a browser
/// opener or renders as a clickable link.
///
/// Deliberately narrow: agent output, plugin UI state and pane text are all
/// untrusted, and `javascript:`, `file:` and `data:` must never reach an
/// opener. Scheme comparison is case-insensitive because a URL's scheme is.
/// Mirrors the web `isExternalHttpUrl`.
pub(crate) fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// `path` with a leading `home` replaced by `~`, only when `path` is `home` or
/// lies under it; a sibling that merely shares a string prefix is unchanged.
pub(crate) fn collapse_home(path: &str, home: &str) -> String {
    let home = match home.trim_end_matches('/') {
        "" => home,
        trimmed => trimmed,
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// [`collapse_home`] against the user's home directory, for display.
pub(crate) fn collapse_tilde(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => collapse_home(path, &home.to_string_lossy()),
        None => path.to_string(),
    }
}

/// Current Unix time in whole seconds, saturating to 0 if the clock is before
/// the epoch (which should never happen on a sane system).
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whole milliseconds between `t` and the Unix epoch, saturating to 0 if `t`
/// predates the epoch.
pub(crate) fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current Unix time in whole milliseconds. Wall-clock (not a per-process
/// monotonic), so values are comparable across processes; saturating to 0 if
/// the clock is before the epoch.
pub(crate) fn now_ms() -> u64 {
    system_time_to_ms(SystemTime::now())
}

/// Fail-closed check for an owner-only credential store: the parent dir must
/// exist, not be a symlink, and not be group/world writable; the file, when
/// present, must be a regular owner-only file rather than a symlink, so a
/// secret is never read from or written through a planted link. A missing
/// file passes. `label` names the store in errors, e.g. `"login sessions"`.
pub(crate) fn check_owner_only_file(path: &std::path::Path, label: &str) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    if let Some(parent) = path.parent() {
        let meta = std::fs::symlink_metadata(parent)
            .with_context(|| format!("stat {label} parent dir"))?;
        if meta.file_type().is_symlink() {
            bail!("{label} parent dir is a symlink; refusing");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o022 != 0 {
                bail!("{label} parent dir is group/world writable; refusing");
            }
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("{label} path is a symlink; refusing");
            }
            if !meta.file_type().is_file() {
                bail!("{label} path is not a regular file; refusing");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    bail!("{label} file is group/world accessible; refusing");
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("stat {label} file")),
    }
}

/// This machine's hostname, when it has a usable one.
pub fn hostname() -> Option<String> {
    nix::unistd::gethostname()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn owner_only_file_check_rejects_symlink_dir_and_loose_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        // A symlink at the path is rejected (no following).
        let target = dir.path().join("real.toml");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            check_owner_only_file(&link, "store").is_err(),
            "symlink must be rejected"
        );

        // A world/group-accessible file is rejected.
        let loose = dir.path().join("loose.toml");
        std::fs::write(&loose, "x").unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            check_owner_only_file(&loose, "store").is_err(),
            "0644 file must be rejected"
        );

        // A directory at the path is rejected.
        let subdir = dir.path().join("dir.toml");
        std::fs::create_dir(&subdir).unwrap();
        assert!(check_owner_only_file(&subdir, "store").is_err());

        // A 0600 file under a private dir passes.
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "x").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o600)).unwrap();
        // tempfile dirs are 0700, so the parent check passes too.
        assert!(
            check_owner_only_file(&ok, "store").is_ok(),
            "0600 file should pass"
        );

        // A missing file (not yet created) passes: the parent is fine.
        assert!(check_owner_only_file(&dir.path().join("missing.toml"), "store").is_ok());
    }

    #[test]
    fn collapse_home_requires_a_separator_after_home() {
        for (path, expect) in [
            ("/home/u", "~"),
            ("/home/u/projects/app", "~/projects/app"),
            ("/home/u/projects/", "~/projects/"),
            ("/home/uextra/not/home", "/home/uextra/not/home"),
            ("/tmp/elsewhere", "/tmp/elsewhere"),
            ("relative/path", "relative/path"),
        ] {
            assert_eq!(collapse_home(path, "/home/u"), expect, "{path}");
            assert_eq!(
                collapse_home(path, "/home/u/"),
                expect,
                "{path} (trailing /)"
            );
        }
    }

    #[test]
    fn system_time_to_ms_at_epoch_is_zero() {
        assert_eq!(system_time_to_ms(UNIX_EPOCH), 0);
    }

    #[test]
    fn system_time_to_ms_converts_offset() {
        let t = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(system_time_to_ms(t), 1_500);
    }

    #[test]
    fn pre_epoch_saturates_to_zero() {
        let before = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(system_time_to_ms(before), 0);
    }

    #[test]
    fn now_ms_matches_seconds_at_same_instant() {
        let t = SystemTime::now();
        let ms = system_time_to_ms(t);
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        assert_eq!(ms / 1_000, secs);
    }

    #[test]
    fn now_helpers_are_post_epoch() {
        assert!(now_secs() > 0);
        assert!(now_ms() > 0);
    }
}
