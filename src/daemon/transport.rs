//! Native HTTP transport. Unix identity is checked on the connected stream.

use std::path::Path;
use std::time::Duration;

use super::DaemonClientError;

const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

pub(crate) fn path_segment(
    value: &str,
) -> Result<percent_encoding::PercentEncode<'_>, DaemonClientError> {
    // URL parsers normalize even percent-encoded dot segments.
    if matches!(value, "" | "." | "..") {
        return Err(DaemonClientError::InvalidPathSegment);
    }
    Ok(percent_encoding::utf8_percent_encode(value, PATH_SEGMENT))
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
        drop_group_other_write(app_dir, owner)?;
    }
    let private_gid = user_private_gid(owner);
    for ancestor in parent.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor)?;
        anyhow::ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && trusted_ancestor(meta.uid(), meta.gid(), meta.mode(), owner, private_gid),
            "Unsafe daemon socket directory: {}",
            ancestor.display()
        );
    }
    let directory = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        directory.uid() == owner && directory.mode() & 0o777 == 0o700,
        "Daemon socket directory must be owned by this user with mode0700"
    );
    Ok(())
}

/// The app dir belongs to aoe, but a umask of 002 creates it group-writable.
fn drop_group_other_write(dir: &Path, owner: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.is_dir() && meta.uid() == owner && meta.mode() & 0o022 != 0 {
        let mode = meta.mode() & 0o7777 & !0o022;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The owner's primary group when it is a user-private group: same name as
/// the user and no listed members, as Debian and Ubuntu create by default.
fn user_private_gid(owner: u32) -> Option<u32> {
    use nix::unistd::{Group, Uid, User};
    let user = User::from_uid(Uid::from_raw(owner)).ok()??;
    let group = Group::from_gid(user.gid).ok()??;
    (group.name == user.name && group.mem.is_empty()).then(|| user.gid.as_raw())
}

fn trusted_ancestor(uid: u32, gid: u32, mode: u32, owner: u32, private_gid: Option<u32>) -> bool {
    let trusted_sticky = uid == 0 && mode & 0o1000 != 0;
    let private_group_write = uid == owner && mode & 0o002 == 0 && private_gid == Some(gid);
    (uid == owner || uid == 0) && (mode & 0o022 == 0 || trusted_sticky || private_group_write)
}

pub(crate) async fn connect_unix(path: &Path) -> Result<tokio::net::UnixStream, DaemonClientError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|_| DaemonClientError::UnixTransport)?;
    let uid =
        crate::process::unix_peer_uid(&stream).map_err(|_| DaemonClientError::PeerIdentity)?;
    if uid != nix::unistd::geteuid().as_raw() {
        return Err(DaemonClientError::PeerIdentity);
    }
    Ok(stream)
}

pub(crate) async fn execute(
    http: &reqwest::Client,
    unix_path: Option<&Path>,
    request: reqwest::Request,
) -> Result<reqwest::Response, DaemonClientError> {
    let Some(path) = unix_path else {
        return http
            .execute(request)
            .await
            .map_err(|_| DaemonClientError::Transport);
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        let stream = connect_unix(path).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| DaemonClientError::UnixTransport)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request: axum::http::Request<reqwest::Body> = request
            .try_into()
            .map_err(|_: reqwest::Error| DaemonClientError::Transport)?;
        if !request.headers().contains_key(axum::http::header::HOST) {
            request.headers_mut().insert(
                axum::http::header::HOST,
                axum::http::HeaderValue::from_static("localhost"),
            );
        }
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| DaemonClientError::UnixTransport)?;
        let response = response
            .map(|body| reqwest::Body::wrap_stream(axum::body::Body::new(body).into_data_stream()));
        Ok(reqwest::Response::from(response))
    })
    .await
    .map_err(|_| DaemonClientError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_trust_allows_only_owner_private_or_sticky_writes() {
        const ME: u32 = 1000;
        const PRIVATE: Option<u32> = Some(1000);
        let cases = [
            (ME, 1000, 0o755, PRIVATE, true),
            (ME, 1000, 0o775, PRIVATE, true),
            (ME, 1000, 0o775, None, false),
            (ME, 100, 0o775, PRIVATE, false),
            (ME, 1000, 0o777, PRIVATE, false),
            (0, 0, 0o1777, PRIVATE, true),
            (0, 1000, 0o775, PRIVATE, false),
            (2000, 2000, 0o755, PRIVATE, false),
        ];
        for (uid, gid, mode, private_gid, expected) in cases {
            assert_eq!(
                trusted_ancestor(uid, gid, mode, ME, private_gid),
                expected,
                "uid={uid} gid={gid} mode={mode:o} private={private_gid:?}"
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
}
