//! Local Daemon socket discovery, directory policy and peer verification.

use std::path::Path;

use super::DaemonClientError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimePathRemedy {
    Advice(&'static str),
    Command {
        template: &'static str,
        manual: &'static str,
    },
}

/// Format a refused directory without terminal controls or lossy shell commands.
fn runtime_path_refusal(
    path: &std::path::Path,
    mode: u32,
    cause: &str,
    remedy: RuntimePathRemedy,
) -> String {
    fn append_path(output: &mut String, template: &str, path: impl std::fmt::Display) {
        use std::fmt::Write;
        let mut parts = template.split("{path}");
        output.push_str(parts.next().unwrap_or_default());
        for part in parts {
            write!(output, "{path}{part}").unwrap();
        }
    }

    let mut message = format!(
        "Unsafe runtime directory: {path:?} is {cause} (mode {:04o}). ",
        mode & 0o7777
    );
    match remedy {
        RuntimePathRemedy::Advice(advice) => {
            message.push_str("Resolve this by: ");
            message.push_str(advice);
        }
        RuntimePathRemedy::Command { template, manual } => {
            let text = path.to_str().filter(|text| {
                !text.contains(|c: char| {
                    // Bidi_Control characters are not covered by is_control().
                    c.is_control()
                        || matches!(
                            c,
                            '`' | '\u{061c}'
                                | '\u{200e}'..='\u{200f}'
                                | '\u{202a}'..='\u{202e}'
                                | '\u{2066}'..='\u{2069}'
                        )
                })
            });
            if let Some(text) = text {
                message.push_str("Resolve this by: `");
                append_path(
                    &mut message,
                    template,
                    crate::session::environment::shell_escape(text),
                );
                message.push_str("`, then retry.");
            } else {
                message.push_str(manual);
                message.push_str(
                    " manually, then retry. No copyable shell command is available for this path.",
                );
            }
        }
    }
    message
}

enum PermissionRepair {
    Private,
    RemoveGroupOtherWrite,
}

fn permission_refusal(
    path: &Path,
    mode: u32,
    repair: PermissionRepair,
    owner: u32,
) -> anyhow::Error {
    use std::fmt::Write;

    let (cause, template, manual) = match repair {
        PermissionRepair::Private => (
            "not private to its owner",
            "chmod -- 0700 {path}",
            "Set this directory to mode 0700",
        ),
        PermissionRepair::RemoveGroupOtherWrite => (
            "writable by its group or by others",
            "chmod -- go-w {path}",
            "Remove group/other write permission from this directory",
        ),
    };
    let mut highest_unsafe_ancestor = None;
    let mut unverified_ancestor = None;
    for ancestor in path.ancestors().skip(1) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if trusted_directory(&metadata, owner) => {}
            Ok(_) => {
                highest_unsafe_ancestor = Some(ancestor);
                // Lower metadata failures cannot hide a higher unsafe ancestor.
                unverified_ancestor = None;
            }
            Err(_) => unverified_ancestor = Some(ancestor),
        }
    }
    let remedy = if highest_unsafe_ancestor.is_none() && unverified_ancestor.is_none() {
        RuntimePathRemedy::Command { template, manual }
    } else {
        RuntimePathRemedy::Advice(
            "Repair or replace unsafe parent directories, or select a location with trusted ancestors. Then retry before changing this directory.",
        )
    };
    let mut message = runtime_path_refusal(path, mode, cause, remedy);
    if let Some(ancestor) = unverified_ancestor {
        write!(
            message,
            " Ancestor trust could not be verified at {ancestor:?}; the highest unsafe ancestor cannot be determined."
        )
        .unwrap();
    } else if let Some(ancestor) = highest_unsafe_ancestor {
        write!(
            message,
            " Start with the highest unsafe ancestor: {ancestor:?}."
        )
        .unwrap();
    }
    anyhow::anyhow!(message)
}

fn trusted_directory(metadata: &std::fs::Metadata, owner: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_dir()
        && !metadata.file_type().is_symlink()
        && trusted_ancestor(metadata.uid(), metadata.mode(), owner)
}

pub(crate) fn local_socket_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::session::get_app_dir()?
        .canonicalize()?
        .join("daemon")
        .join("api.sock"))
}

pub(crate) fn prepare_runtime_directory(parent: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let owner = nix::unistd::geteuid().as_raw();
    if let Some(app_dir) = parent.parent() {
        for ancestor in app_dir.ancestors().skip(1) {
            check_ancestor(ancestor, parent, owner)?;
        }
        drop_group_other_write(app_dir, owner)?;
    }
    for ancestor in parent.ancestors() {
        check_ancestor(ancestor, parent, owner)?;
    }
    let directory = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        directory.uid() == owner,
        "{}",
        runtime_path_refusal(
            parent,
            directory.mode(),
            "owned by another user",
            RuntimePathRemedy::Advice("Select a socket directory owned by this user."),
        )
    );
    if directory.mode() & 0o777 != 0o700 {
        return Err(permission_refusal(
            parent,
            directory.mode(),
            PermissionRepair::Private,
            owner,
        ));
    }
    Ok(())
}

fn check_ancestor(ancestor: &Path, parent: &Path, owner: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(ancestor)?;
    if meta.is_dir()
        && meta.uid() == owner
        && meta.mode() & 0o022 != 0
        && !trusted_ancestor(meta.uid(), meta.mode(), owner)
    {
        let repair = if ancestor == parent {
            PermissionRepair::Private
        } else {
            PermissionRepair::RemoveGroupOtherWrite
        };
        return Err(permission_refusal(ancestor, meta.mode(), repair, owner));
    }
    anyhow::ensure!(
        trusted_directory(&meta, owner),
        "{}",
        runtime_path_refusal(
            ancestor,
            meta.mode(),
            "not a trusted real directory",
            RuntimePathRemedy::Advice(
                "Inspect ownership and symlinks; select a real directory owned by this user or root.",
            ),
        )
    );
    Ok(())
}

/// The app dir belongs to aoe, but a umask of 002 creates it group-writable.
/// Callers validate its ancestors first; the open pins the inode so a swapped
/// entry cannot be chmodded.
fn drop_group_other_write(dir: &Path, owner: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC)
        .open(dir)
    {
        Ok(file) => file,
        // A symlink or non-directory is refused with a diagnostic by the ancestor check.
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(nix::libc::ELOOP | nix::libc::ENOTDIR)
            ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    drop_write_through_fd(&file, owner)
}

fn drop_write_through_fd(file: &std::fs::File, owner: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = file.metadata()?;
    if meta.uid() == owner && meta.mode() & 0o022 != 0 {
        let mode = meta.mode() & 0o7777 & !0o022;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

pub(crate) fn trusted_ancestor(uid: u32, mode: u32, owner: u32) -> bool {
    let trusted_sticky = uid == 0 && mode & 0o1000 != 0;
    (uid == owner || uid == 0) && (mode & 0o022 == 0 || trusted_sticky)
}

pub(crate) async fn connect_unix(path: &Path) -> Result<tokio::net::UnixStream, DaemonClientError> {
    aoe_daemon_client::transport::connect_unix(path, verify_peer, None).await
}

fn verify_peer(
    stream: &tokio::net::UnixStream,
    expected_pid: Option<u32>,
) -> Result<(), DaemonClientError> {
    let uid = crate::process::unix_peer_uid(stream).map_err(|_| DaemonClientError::PeerIdentity)?;
    if uid != nix::unistd::geteuid().as_raw()
        || expected_pid.is_some_and(|pid| crate::process::unix_peer_pid(stream) != Some(pid))
    {
        return Err(DaemonClientError::PeerIdentity);
    }
    Ok(())
}

pub fn local_unix_client(
    path: impl Into<std::path::PathBuf>,
) -> Result<super::DaemonClient, DaemonClientError> {
    super::DaemonClient::new_unix(aoe_daemon_client::transport::UnixTransport::new(
        path,
        verify_peer,
    ))
}

pub fn local_unix_endpoint(
    path: std::path::PathBuf,
    dashboard_url: Option<String>,
) -> super::DaemonEndpoint {
    super::DaemonEndpoint::local_unix(
        aoe_daemon_client::transport::UnixTransport::new(path, verify_peer),
        dashboard_url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_trust_allows_only_owner_or_root_sticky_writes() {
        const ME: u32 = 1000;
        let cases = [
            (ME, 0o755, true),
            (ME, 0o775, false),
            (ME, 0o777, false),
            (0, 0o1777, true),
            (0, 0o775, false),
            (2000, 0o755, false),
        ];
        for (uid, mode, expected) in cases {
            assert_eq!(
                trusted_ancestor(uid, mode, ME),
                expected,
                "uid={uid} mode={mode:o}"
            );
        }
    }

    #[test]
    fn group_writable_app_dir_is_tightened_before_the_socket_directory_check() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let app = root.path().canonicalize().unwrap().join("app");
        std::fs::create_dir(&app).unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o775)).unwrap();

        prepare_runtime_directory(&app.join("daemon")).unwrap();

        assert_eq!(std::fs::metadata(&app).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn app_dir_tightening_follows_the_opened_inode_not_the_path() {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let app = base.join("app");
        let moved = base.join("moved");
        let sentinel = base.join("sentinel");
        std::fs::create_dir(&app).unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::write(&sentinel, b"secret").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pinned = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY)
            .open(&app)
            .unwrap();
        std::fs::rename(&app, &moved).unwrap();
        std::os::unix::fs::symlink(&sentinel, &app).unwrap();

        drop_write_through_fd(&pinned, nix::unistd::geteuid().as_raw()).unwrap();

        assert_eq!(std::fs::metadata(&sentinel).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&moved).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn symlinked_app_dir_is_refused_and_its_target_left_alone() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::os::unix::fs::symlink(&target, base.join("app")).unwrap();

        prepare_runtime_directory(&base.join("app/daemon")).unwrap_err();

        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o775);
    }

    #[test]
    fn untrusted_upper_parent_is_refused_before_the_app_dir_is_tightened() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let upper = base.join("upper");
        let app = upper.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&upper, std::fs::Permissions::from_mode(0o777)).unwrap();

        let error = prepare_runtime_directory(&app.join("daemon")).unwrap_err();

        assert!(
            error.to_string().contains(upper.to_str().unwrap()),
            "{error}"
        );
        assert_eq!(std::fs::metadata(&app).unwrap().mode() & 0o777, 0o775);
    }

    fn run_directory_recovery(error: &anyhow::Error) {
        let diagnostic = error.to_string();
        let command = diagnostic
            .split('`')
            .nth(1)
            .unwrap_or_else(|| panic!("missing recovery command: {diagnostic}"));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .unwrap();
        assert!(status.success(), "recovery command failed: {diagnostic}");
    }

    #[test]
    fn socket_directory_recovery_command_repairs_every_nonprivate_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        for mode in [0o755, 0o775, 0o777] {
            let root = tempfile::tempdir().unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let app = root
                .path()
                .canonicalize()
                .unwrap()
                .join("owner's café directory");
            std::fs::create_dir(&app).unwrap();
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).unwrap();
            let parent = app.join("owner's daemon directory");
            std::fs::create_dir(&parent).unwrap();
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();

            let error = prepare_runtime_directory(&parent).unwrap_err();
            assert!(
                error.to_string().contains(parent.to_str().unwrap()),
                "{error}"
            );
            assert_eq!(std::fs::metadata(&parent).unwrap().mode() & 0o777, mode);
            run_directory_recovery(&error);
            assert_eq!(std::fs::metadata(&parent).unwrap().mode() & 0o777, 0o700);
            prepare_runtime_directory(&parent).unwrap();
        }
    }

    #[test]
    fn higher_ancestor_recovery_command_removes_group_and_other_write() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let ancestor = root
            .path()
            .canonicalize()
            .unwrap()
            .join("owner's unsafe ancestor");
        let app = ancestor.join("app");
        let parent = app.join("daemon");
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::create_dir(&app).unwrap();
        std::fs::create_dir(&parent).unwrap();
        for directory in [&app, &parent] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o777)).unwrap();

        let error = prepare_runtime_directory(&parent).unwrap_err();
        assert!(
            error.to_string().contains(ancestor.to_str().unwrap()),
            "{error}"
        );
        assert_eq!(std::fs::metadata(&ancestor).unwrap().mode() & 0o777, 0o777);
        // Removing only group write still leaves an unsafe world-writable ancestor.
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o757)).unwrap();
        let error = prepare_runtime_directory(&parent).unwrap_err();
        assert_eq!(std::fs::metadata(&ancestor).unwrap().mode() & 0o777, 0o757);
        run_directory_recovery(&error);
        assert_eq!(std::fs::metadata(&ancestor).unwrap().mode() & 0o777, 0o755);
        prepare_runtime_directory(&parent).unwrap();
    }

    #[test]
    fn unsafe_upper_ancestors_suppress_copyable_recovery_commands() {
        use std::os::unix::fs::PermissionsExt;

        for name in ["unsafe upper", "unsafe\x1b[31m\u{202e} upper"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let upper = root.path().canonicalize().unwrap().join(name);
            let lower = upper.join("unsafe lower");
            let app = lower.join("app");
            let socket = app.join("daemon");
            std::fs::create_dir_all(&socket).unwrap();
            for directory in [&upper, &lower, &socket] {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o777))
                    .unwrap();
            }
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).unwrap();

            let ancestor_diagnostic = prepare_runtime_directory(&socket).unwrap_err().to_string();
            let socket_diagnostic =
                check_ancestor(&socket, &socket, nix::unistd::geteuid().as_raw())
                    .unwrap_err()
                    .to_string();
            for (diagnostic, refused) in [
                (&ancestor_diagnostic, &lower),
                (&socket_diagnostic, &socket),
            ] {
                assert!(diagnostic.contains(&format!("{refused:?}")), "{diagnostic}");
                assert!(diagnostic.contains(&format!("{upper:?}")), "{diagnostic}");
                assert!(!diagnostic.contains("`chmod"), "{diagnostic}");
                assert!(!diagnostic.contains(['\x1b', '\u{202e}']), "{diagnostic:?}");
            }
        }
    }

    #[test]
    fn ancestor_metadata_failures_do_not_hide_higher_unsafe_components() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        for ancestor in [base.join("missing"), base.join("not-a-directory")] {
            if ancestor.ends_with("not-a-directory") {
                std::fs::write(&ancestor, b"not a directory").unwrap();
            }
            let refused = ancestor.join("unverified/refused");
            assert!(std::fs::symlink_metadata(refused.parent().unwrap()).is_err());
            for repair in [
                PermissionRepair::Private,
                PermissionRepair::RemoveGroupOtherWrite,
            ] {
                let diagnostic =
                    permission_refusal(&refused, 0o777, repair, nix::unistd::geteuid().as_raw())
                        .to_string();
                assert!(diagnostic.contains(&format!("{refused:?}")), "{diagnostic}");
                assert!(
                    diagnostic.contains(&format!("{ancestor:?}")),
                    "{diagnostic}"
                );
                assert!(!diagnostic.contains("`chmod"), "{diagnostic}");
            }
        }
    }

    #[test]
    fn recovery_command_omits_unrepresentable_paths() {
        use std::ffi::OsString;
        use std::os::unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, PermissionsExt},
        };

        // Filesystems need not accept non-UTF-8 names.
        let invalid = std::path::PathBuf::from(OsString::from_vec(b"invalid-\xff".to_vec()));
        for template in ["chmod -- 0700 {path}", "chmod -- go-w {path}"] {
            let diagnostic = runtime_path_refusal(
                &invalid,
                0o777,
                "not private to its owner",
                RuntimePathRemedy::Command {
                    template,
                    manual: "Repair directory permissions",
                },
            );
            assert!(!diagnostic.contains("`chmod"), "{diagnostic}");
            assert!(diagnostic.contains(&format!("{invalid:?}")), "{diagnostic}");
        }

        for name in [
            OsString::from("carriage\rreturn"),
            OsString::from("line\nfeed"),
            OsString::from("back`tick"),
            OsString::from("escape\x1b[31m"),
            OsString::from("bell\x07"),
            OsString::from("tab\tname"),
            OsString::from("delete\x7f"),
            OsString::from("c1\u{9b}"),
            OsString::from("alm\u{061c}"),
            OsString::from("lrm\u{200e}"),
            OsString::from("rlm\u{200f}"),
            OsString::from("lre\u{202a}"),
            OsString::from("rle\u{202b}"),
            OsString::from("pdf\u{202c}"),
            OsString::from("lro\u{202d}"),
            OsString::from("rlo\u{202e}"),
            OsString::from("lri\u{2066}"),
            OsString::from("rli\u{2067}"),
            OsString::from("fsi\u{2068}"),
            OsString::from("pdi\u{2069}"),
        ] {
            for (socket, mode) in [(true, 0o755), (true, 0o777), (false, 0o777)] {
                let root = tempfile::tempdir().unwrap();
                let base = root.path().canonicalize().unwrap();
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
                let refused = if socket {
                    base.join("app").join(&name)
                } else {
                    base.join(&name)
                };
                let parent = if socket {
                    refused.clone()
                } else {
                    refused.join("app/daemon")
                };
                std::fs::create_dir_all(&parent).unwrap();
                for directory in parent.ancestors().take_while(|path| *path != base) {
                    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(mode)).unwrap();
                let diagnostic = prepare_runtime_directory(&parent).unwrap_err().to_string();
                assert!(!diagnostic.contains("`chmod"), "{diagnostic}");
                assert!(diagnostic.contains(&format!("{refused:?}")), "{diagnostic}");
                assert_eq!(std::fs::metadata(&refused).unwrap().mode() & 0o777, mode);
                let repaired = if socket { 0o700 } else { mode & !0o022 };
                std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(repaired))
                    .unwrap();
                prepare_runtime_directory(&parent).unwrap();
            }
        }
    }

    #[test]
    fn untrusted_ancestor_diagnostics_escape_controls() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        for name in ["escape\x1b[31m", "bidi\u{202e}"] {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().canonicalize().unwrap();
            let target = base.join("real");
            let parent = target.join("app/daemon");
            std::fs::create_dir_all(&parent).unwrap();
            for path in [&target, &target.join("app"), &parent] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let alias = base.join(name);
            symlink(&target, &alias).unwrap();
            let error = prepare_runtime_directory(&alias.join("app/daemon")).unwrap_err();
            let diagnostic = error.to_string();
            assert!(!diagnostic.contains(['\x1b', '\u{202e}']), "{diagnostic:?}");
            assert!(diagnostic.contains(&format!("{alias:?}")), "{diagnostic:?}");
            assert!(std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }
}
