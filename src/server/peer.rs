//! Connection identity originates at the acceptor, never in HTTP headers.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{request::Parts, StatusCode};

#[derive(Clone, Copy, Debug)]
pub enum ConnectionPeer {
    UnixOwner { uid: u32 },
    Tcp(SocketAddr),
}

impl<S: Send + Sync> FromRequestParts<S> for ConnectionPeer {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<Self>>() {
            return Ok(*peer);
        }
        ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map(|ConnectInfo(addr)| Self::Tcp(addr))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    }
}

pub(super) struct OwnerUnixListener(pub tokio::net::UnixListener);

impl axum::serve::Listener for OwnerUnixListener {
    type Io = tokio::net::UnixStream;
    type Addr = ConnectionPeer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, _)) => {
                    let Ok(uid) = crate::process::unix_peer_uid(&stream) else {
                        continue;
                    };
                    if uid == nix::unistd::geteuid().as_raw() {
                        return (stream, ConnectionPeer::UnixOwner { uid });
                    }
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()?;
        Ok(ConnectionPeer::UnixOwner {
            uid: nix::unistd::geteuid().as_raw(),
        })
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, OwnerUnixListener>>
    for ConnectionPeer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, OwnerUnixListener>) -> Self {
        *stream.remote_addr()
    }
}

pub(super) struct SocketLease {
    path: std::path::PathBuf,
    dev: u64,
    ino: u64,
    // Linux only: the lease pins the socket inode with a private hard link,
    // so a recycled inode number after a replacement rebind cannot make Drop
    // delete the newer socket. macOS rejects hard links to sockets (EPERM)
    // and cannot open one by path, so it keeps the metadata-only check and
    // its theoretical ABA window.
    #[cfg(target_os = "linux")]
    _identity: tempfile::TempDir,
}

impl Drop for SocketLease {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.dev() == self.dev && meta.ino() == self.ino)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The caller holds daemon lifetime ownership until this lease is dropped.
pub(super) async fn bind_private(
    path: &std::path::Path,
) -> anyhow::Result<(OwnerUnixListener, SocketLease)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    anyhow::ensure!(
        path.as_os_str().as_bytes().len() < 104,
        "Daemon Unix socket path is too long: {}",
        path.display()
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Daemon socket has no directory"))?;
    crate::daemon::transport::prepare_runtime_directory(parent)?;
    let owner = nix::unistd::geteuid().as_raw();
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.file_type().is_socket() && meta.uid() == owner && meta.mode() & 0o777 == 0o600,
                "Unsafe existing daemon socket"
            );
            match tokio::time::timeout(
                std::time::Duration::from_millis(300),
                tokio::net::UnixStream::connect(path),
            )
            .await
            {
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    let current = std::fs::symlink_metadata(path)?;
                    anyhow::ensure!(
                        current.dev() == meta.dev() && current.ino() == meta.ino(),
                        "Daemon socket changed during stale check"
                    );
                    std::fs::remove_file(path)?;
                }
                _ => anyhow::bail!("Daemon socket is live or cannot be proven stale"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    #[cfg(target_os = "linux")]
    let identity = {
        let dir = tempfile::Builder::new()
            .prefix(".socket-lease-")
            .tempdir_in(parent)?;
        std::fs::hard_link(path, dir.path().join("socket"))?;
        dir
    };
    let meta = std::fs::symlink_metadata(path)?;
    let lease = SocketLease {
        path: path.to_owned(),
        dev: meta.dev(),
        ino: meta.ino(),
        #[cfg(target_os = "linux")]
        _identity: identity,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((OwnerUnixListener(listener), lease))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_replacement_preserves_live_and_newer_owners() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let path = root.join("daemon/api.sock");
        let (first, old_lease) = bind_private(&path).await.unwrap();
        assert!(bind_private(&path).await.is_err());
        drop(first);
        let (second, lease) = bind_private(&path).await.unwrap();
        drop(old_lease);
        assert!(tokio::net::UnixStream::connect(&path).await.is_ok());
        drop(second);
        drop(lease);
        let target = root.join("unrelated");
        std::fs::write(&target, "preserve").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(bind_private(&path).await.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve");
    }
}
