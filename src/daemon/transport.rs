//! Native HTTP transport. Unix identity is checked on the connected stream.

use std::path::Path;
#[cfg(test)]
use std::time::Duration;

use super::DaemonClientError;
use crate::util::{runtime_path_refusal, RuntimePathRemedy};

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
        return Err(anyhow::anyhow!(runtime_path_refusal(
            parent,
            directory.mode(),
            "not private to its owner",
            RuntimePathRemedy::Command {
                template: "chmod -- 0700 {path}",
                manual: "Set this directory to mode 0700",
            },
        )));
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
        if ancestor == parent {
            return Err(anyhow::anyhow!(runtime_path_refusal(
                ancestor,
                meta.mode(),
                "not private to its owner",
                RuntimePathRemedy::Command {
                    template: "chmod -- 0700 {path}",
                    manual: "Set this directory to mode 0700",
                },
            )));
        }
        return Err(anyhow::anyhow!(runtime_path_refusal(
            ancestor,
            meta.mode(),
            "writable by its group or by others",
            RuntimePathRemedy::Command {
                template: "chmod -- go-w {path}",
                manual: "Remove group/other write permission from this directory",
            },
        )));
    }
    anyhow::ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && trusted_ancestor(meta.uid(), meta.mode(), owner),
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
    execute_with_peer(http, unix_path, request, None).await
}

pub(super) async fn execute_with_peer(
    http: &reqwest::Client,
    unix_path: Option<&Path>,
    request: reqwest::Request,
    expected_peer: Option<u32>,
) -> Result<reqwest::Response, DaemonClientError> {
    use futures_util::StreamExt;
    let deadline =
        tokio::time::Instant::now() + request.timeout().copied().unwrap_or(super::DEFAULT_TIMEOUT);
    let Some(path) = unix_path else {
        if expected_peer.is_some() {
            return Err(DaemonClientError::PeerIdentity);
        }
        let mut response = tokio::time::timeout_at(deadline, http.execute(request))
            .await
            .map_err(|_| DaemonClientError::Timeout)?
            .map_err(request_error)?;
        response
            .extensions_mut()
            .insert(OperationDeadline(deadline));
        return Ok(response);
    };
    tokio::time::timeout_at(deadline, async {
        let stream = connect_unix(path).await?;
        if let Some(expected) = expected_peer {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let actual = stream
                .peer_cred()
                .ok()
                .and_then(|credentials| credentials.pid())
                .and_then(|pid| u32::try_from(pid).ok());
            #[cfg(target_os = "macos")]
            let actual =
                nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::LocalPeerPid)
                    .ok()
                    .and_then(|pid| u32::try_from(pid).ok());
            #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
            let actual = None;
            if actual != Some(expected) {
                return Err(DaemonClientError::PeerIdentity);
            }
        }
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| DaemonClientError::UnixTransport)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request: axum::http::Request<reqwest::Body> =
            request.try_into().map_err(request_error)?;
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
        let response = response.map(|body| {
            let source = axum::body::Body::new(body).into_data_stream();
            let chunks = futures_util::stream::try_unfold(source, move |mut source| async move {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                let chunk = tokio::time::timeout_at(deadline, source.next())
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
                match chunk {
                    Some(Ok(chunk)) => Ok(Some((chunk, source))),
                    Some(Err(error)) => Err(std::io::Error::other(error)),
                    None => Ok(None),
                }
            });
            reqwest::Body::wrap_stream(chunks)
        });
        let mut response = reqwest::Response::from(response);
        response
            .extensions_mut()
            .insert(OperationDeadline(deadline));
        Ok(response)
    })
    .await
    .map_err(|_| DaemonClientError::Timeout)?
}

#[derive(Clone, Copy)]
struct OperationDeadline(tokio::time::Instant);

pub(crate) fn response_deadline(response: &reqwest::Response) -> tokio::time::Instant {
    response
        .extensions()
        .get::<OperationDeadline>()
        .map_or_else(
            || tokio::time::Instant::now() + super::DEFAULT_TIMEOUT,
            |deadline| deadline.0,
        )
}

pub(crate) fn request_error(error: reqwest::Error) -> DaemonClientError {
    tracing::debug!(target: "daemon.client", %error, "daemon request failed");
    if error.is_timeout() {
        DaemonClientError::Timeout
    } else {
        DaemonClientError::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unix_body_keeps_the_header_operation_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (arrived, mut observed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let mut used = 0;
            while !request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut request[used..]).await.unwrap();
                assert!(count > 0, "request must contain complete HTTP headers");
                used += count;
            }
            arrived.send(()).unwrap();
            released.await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            let mut eof = [0];
            assert_eq!(stream.read(&mut eof).await.unwrap(), 0);
        });
        let http = reqwest::Client::new();
        let request = http
            .get("http://localhost/operation")
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let start = tokio::time::Instant::now();
        let expected_deadline = start + Duration::from_millis(100);
        let client = tokio::spawn(async move { execute(&http, Some(&path), request).await });
        // Keep the runtime runnable while real socket I/O establishes request arrival.
        loop {
            match observed.try_recv() {
                Ok(()) => break,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    tokio::task::yield_now().await;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    panic!("server closed before receiving the request");
                }
            }
        }
        assert_eq!(tokio::time::Instant::now(), start);
        tokio::time::advance(Duration::from_millis(60)).await;
        assert!(!client.is_finished());
        release.send(()).unwrap();
        // Paused time must not auto-advance while the OS makes headers readable.
        while !client.is_finished() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            tokio::time::Instant::now(),
            start + Duration::from_millis(60)
        );
        let mut response = client.await.unwrap().unwrap();
        assert_eq!(response_deadline(&response), expected_deadline);
        {
            let body = tokio::time::timeout(Duration::from_secs(1), response.chunk());
            tokio::pin!(body);
            assert!(futures_util::poll!(&mut body).is_pending());
            tokio::time::advance(Duration::from_millis(39)).await;
            assert!(futures_util::poll!(&mut body).is_pending());
            tokio::time::advance(Duration::from_millis(11)).await;
            let result = body.await;
            assert!(
                matches!(result, Ok(Err(ref error)) if error.is_timeout()),
                "body={result:?}"
            );
            assert_eq!(
                tokio::time::Instant::now(),
                start + Duration::from_millis(110)
            );
        }
        drop(response);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unix_requests_honor_their_operation_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for (deadline, elapsed, should_timeout) in [(30, 16, false), (1, 2, true)] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("api.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let (arrived, observed) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = [0; 4096];
                let mut used = 0;
                while !header[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let read = stream.read(&mut header[used..]).await.unwrap();
                    assert!(read > 0);
                    used += read;
                }
                arrived.send(()).unwrap();
                released.await.unwrap();
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            });
            let http = reqwest::Client::new();
            let request = http
                .get("http://localhost/operation")
                .timeout(Duration::from_secs(deadline))
                .build()
                .unwrap();
            let client = tokio::spawn(async move { execute(&http, Some(&path), request).await });
            observed.await.unwrap();
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(elapsed)).await;
            tokio::time::resume();
            if should_timeout {
                assert!(matches!(
                    client.await.unwrap(),
                    Err(DaemonClientError::Timeout)
                ));
                release.send(()).unwrap();
            } else {
                release.send(()).unwrap();
                assert_eq!(
                    client.await.unwrap().unwrap().status(),
                    reqwest::StatusCode::CREATED
                );
            }
            server.await.unwrap();
        }
    }

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

    async fn serve_deadline_body<S>(
        mut stream: S,
        headers_after: Duration,
        chunks: usize,
        chunk_after: Duration,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        tokio::time::sleep(headers_after).await;
        stream
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {chunks}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        for _ in 0..chunks {
            tokio::time::sleep(chunk_after).await;
            if stream.write_all(b" ").await.is_err() {
                break;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn extended_unix_body_keeps_its_operation_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buf).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buf[..count]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            released.await.unwrap();
            stream.write_all(b"x").await.unwrap();
        });
        let http = reqwest::Client::new();
        let request = http
            .get("http://localhost/operation")
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let start = tokio::time::Instant::now();
        let response = tokio::spawn(async move { execute(&http, Some(&path), request).await });
        let wall_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !response.is_finished() {
            assert!(
                std::time::Instant::now() < wall_deadline,
                "fixture headers did not arrive"
            );
            tokio::task::yield_now().await;
        }
        let mut response = response.await.unwrap().unwrap();
        assert_eq!(
            response_deadline(&response),
            start + Duration::from_secs(30)
        );
        let body = super::super::read_bounded_body(&mut response, 1024);
        tokio::pin!(body);
        assert!(futures_util::poll!(&mut body).is_pending());
        tokio::time::advance(Duration::from_secs(16)).await;
        assert!(futures_util::poll!(&mut body).is_pending());
        release.send(()).unwrap();
        let bytes = loop {
            if let std::task::Poll::Ready(result) = futures_util::poll!(&mut body) {
                break result.unwrap();
            }
            assert!(
                std::time::Instant::now() < wall_deadline,
                "fixture body did not arrive"
            );
            tokio::task::yield_now().await;
        };
        assert_eq!(bytes.as_slice(), b"x");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn body_helpers_share_deadlines_without_shortening_long_operations() {
        for unix in [true, false] {
            for (headers_ms, chunks, chunk_ms, timeout_ms, expires) in [
                (0, 2, 0, 1000, false),
                (70, 1, 60, 100, true),
                (0, 10, 20, 100, true),
            ] {
                for consumer in 0..3 {
                    let temp = tempfile::tempdir().unwrap();
                    let path = temp.path().join("api.sock");
                    let (url, server) = if unix {
                        let listener = tokio::net::UnixListener::bind(&path).unwrap();
                        let server = tokio::spawn(async move {
                            let (stream, _) = listener.accept().await.unwrap();
                            serve_deadline_body(
                                stream,
                                Duration::from_millis(headers_ms),
                                chunks,
                                Duration::from_millis(chunk_ms),
                            )
                            .await;
                        });
                        ("http://localhost/operation".to_owned(), server)
                    } else {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let url = format!("http://{}/operation", listener.local_addr().unwrap());
                        let server = tokio::spawn(async move {
                            let (stream, _) = listener.accept().await.unwrap();
                            serve_deadline_body(
                                stream,
                                Duration::from_millis(headers_ms),
                                chunks,
                                Duration::from_millis(chunk_ms),
                            )
                            .await;
                        });
                        (url, server)
                    };
                    let http = reqwest::Client::new();
                    let request = http
                        .get(&url)
                        .timeout(Duration::from_millis(timeout_ms))
                        .build()
                        .unwrap();
                    let mut response = execute(&http, unix.then_some(path.as_path()), request)
                        .await
                        .unwrap();
                    let client = super::super::DaemonClient::new(&url, None).unwrap();
                    let result = match consumer {
                        0 => super::super::read_bounded_body(&mut response, 1024)
                            .await
                            .map(|_| ()),
                        1 => super::super::discard_bounded_body(&mut response).await,
                        _ => client.read_error_body(&mut response).await.map(|_| ()),
                    };
                    assert_eq!(
                        matches!(result, Err(DaemonClientError::Timeout)),
                        expires,
                        "unix={unix}, consumer={consumer}, chunks={chunks}, result={result:?}"
                    );
                    if !expires {
                        result.unwrap();
                    }
                    drop(response);
                    server.abort();
                    let _ = server.await;
                }
            }
        }
    }

    #[tokio::test]
    async fn create_hook_error_body_uses_the_request_deadline_and_bad_json_is_decode() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for malformed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("api.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let mut used = 0;
                while !request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut request[used..]).await.unwrap();
                    assert!(count > 0, "request must contain complete HTTP headers");
                    used += count;
                }
                if malformed {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{",
                        )
                        .await
                        .unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 400 Bad Request\r\nAoE-Error-Code: create_hook_failed\r\nContent-Length: 100\r\n\r\n").await.unwrap();
                    let mut eof = [0];
                    let _ = stream.read(&mut eof).await;
                }
            });
            let client = super::super::DaemonClient::new_unix(&path).unwrap();
            let request = client
                .http
                .get("http://localhost/operation")
                .timeout(Duration::from_millis(100));
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                client.request_json::<serde_json::Value>(request),
            )
            .await
            .unwrap();
            if malformed {
                assert!(matches!(
                    result,
                    Err(DaemonClientError::AuthenticatedDecode)
                ));
            } else {
                assert!(
                    matches!(result, Err(DaemonClientError::Timeout)),
                    "result={result:?}"
                );
            }
            server.await.unwrap();
        }
    }
}
