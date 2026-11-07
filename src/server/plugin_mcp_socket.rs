//! Unix socket serving only the plugin MCP endpoints, bind-mounted into sandboxes.
//!
//! A sandbox reaches it through the `node` forwarder in the same directory, so no network route,
//! bind address or allowed host is involved. The directory is mounted, not the socket, so a
//! socket recreated by a daemon restart shows up in running containers.

/// Where every sandbox container mounts the socket directory, read-only.
pub const CONTAINER_DIR: &str = "/aoe/plugin-mcp";
pub const SOCKET_FILE: &str = "mcp.sock";
pub const FORWARDER_FILE: &str = "forward.mjs";
/// The only `Host` the socket router accepts; the forwarder sends it.
pub const SOCKET_HOST: &str = "aoe-plugin-mcp.invalid";
pub const TOKEN_ENV: &str = "AOE_PLUGIN_MCP_TOKEN";
pub const SESSION_ENV: &str = "AOE_PLUGIN_MCP_SESSION";

pub fn container_socket() -> String {
    format!("{CONTAINER_DIR}/{SOCKET_FILE}")
}

pub fn container_forwarder() -> String {
    format!("{CONTAINER_DIR}/{FORWARDER_FILE}")
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::{
        DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::extract::{Request, State};
    use axum::http::{header, StatusCode};
    use axum::middleware::Next;
    use axum::response::{IntoResponse, Response};
    use tokio::net::UnixListener;

    use super::{CONTAINER_DIR, FORWARDER_FILE, SOCKET_FILE, SOCKET_HOST};
    use crate::containers::{InspectedContainer, VolumeMount};
    use crate::server::daemon_endpoint::SandboxSocket;
    use crate::server::state::AppState;

    pub(crate) const FORWARDER: &str = include_str!("../../assets/plugin-mcp/forward.mjs");
    const DIR_NAME: &str = "plugin-mcp";

    fn host_dir() -> anyhow::Result<PathBuf> {
        Ok(crate::session::get_app_dir()?.join(DIR_NAME))
    }

    /// Opens `dir`, created private to this user if missing, only when it is a real directory
    /// owned by this user that nobody else can write, under the ancestors the daemon socket
    /// directory accepts; later operations go through the handle, so a swapped path cannot
    /// redirect them. Returns the handle with the checked real path: only that path may reach
    /// a container runtime, which would resolve a symlink again at mount time. Never removed:
    /// running containers mount it.
    fn open_dir(dir: &Path) -> std::io::Result<(OwnedFd, PathBuf)> {
        let dir = &real_path(dir)?;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let refused = |path: &Path, mode: u32, cause: &str, remedy: &str| {
            std::io::Error::other(format!(
                "{} is {cause} (mode {:04o}).\nResolve this by: {remedy}",
                path.display(),
                mode & 0o7777
            ))
        };
        let owner = nix::unistd::geteuid().as_raw();
        for ancestor in dir.ancestors().skip(1) {
            let meta = std::fs::symlink_metadata(ancestor)?;
            if !meta.is_dir()
                || !crate::daemon::transport::trusted_ancestor(meta.uid(), meta.mode(), owner)
            {
                return Err(refused(
                    ancestor,
                    meta.mode(),
                    "not a directory only this user or root can write",
                    &format!("chmod go-w {}", ancestor.display()),
                ));
            }
        }
        // O_NOFOLLOW: the path itself must be the directory, not a link to one.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(dir)
            .map_err(|e| {
                std::io::Error::other(format!("{} is not a real directory: {e}", dir.display()))
            })?;
        let meta = file.metadata()?;
        if meta.uid() != owner {
            return Err(refused(
                dir,
                meta.mode(),
                "owned by another user",
                "run as the directory's owner",
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(refused(
                dir,
                meta.mode(),
                "writable by its group or by others",
                &format!("chmod go-w {}", dir.display()),
            ));
        }
        let fd = OwnedFd::from(file);
        let here = anchored(&fd, ".")?;
        if std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))? != *dir {
            return Err(std::io::Error::other(format!(
                "{} changed while it was being checked",
                dir.display()
            )));
        }
        std::fs::set_permissions(here, std::fs::Permissions::from_mode(0o700))?;
        Ok((fd, dir.clone()))
    }

    /// `dir` with every symlink in its parents resolved; the last component stays as named,
    /// so it can never be a link. Symlinks in the user's own configured path
    /// (`/home -> /var/home`, a linked XDG dir) are no attack surface once the real path is
    /// checked and opened through a handle.
    fn real_path(dir: &Path) -> std::io::Result<PathBuf> {
        let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
            return Err(std::io::Error::other(format!(
                "{} has no parent directory",
                dir.display()
            )));
        };
        Ok(std::fs::canonicalize(parent)?.join(name))
    }

    fn anchored(dir: &OwnedFd, child: &str) -> std::io::Result<PathBuf> {
        let base = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        if !base.exists() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        Ok(base.join(child))
    }

    /// Replaces the forwarder through a unique temporary file, so concurrent writers never
    /// trip over each other. Only a daemon that bound the socket writes it.
    fn write_forwarder(dir: &OwnedFd) -> std::io::Result<()> {
        let script = anchored(dir, FORWARDER_FILE)?;
        if std::fs::read(&script).ok().as_deref() == Some(FORWARDER.as_bytes()) {
            return Ok(());
        }
        let mut tmp = tempfile::Builder::new()
            .prefix(".forward.")
            .tempfile_in(anchored(dir, ".")?)?;
        tmp.write_all(FORWARDER.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(&script).map_err(|e| e.error)?;
        Ok(())
    }

    /// The read-only mount every sandbox gets, so a session can use plugin MCP servers no
    /// matter when the daemon starts serving them. Only ensures the directory: any aoe
    /// process builds container configs, and the forwarder belongs to the bound daemon.
    pub(crate) fn sandbox_volume() -> Option<VolumeMount> {
        let prepared = host_dir().and_then(|dir| Ok(open_dir(&dir)?.1));
        match prepared {
            Ok(dir) => Some(VolumeMount {
                host_path: dir.to_string_lossy().into_owned(),
                container_path: CONTAINER_DIR.to_string(),
                read_only: true,
            }),
            Err(error) => {
                tracing::warn!(target: "plugin.mcp", error = %format_args!("{error:#}"),
                    "plugin MCP socket directory unavailable; sandboxes get no plugin MCP mount");
                None
            }
        }
    }

    /// How the session's container reaches the socket. Blocking: asks the container runtime.
    pub(crate) fn sandbox_socket(session_id: &str) -> SandboxSocket {
        let container = crate::containers::DockerContainer::from_session_id(session_id);
        let dir = match host_dir().and_then(|dir| Ok(real_path(&dir)?)) {
            Ok(dir) => dir,
            Err(error) => {
                tracing::warn!(target: "plugin.mcp", session = %session_id, error = %format_args!("{error:#}"),
                    "cannot check the sandbox's plugin MCP socket: app dir unavailable");
                return SandboxSocket::Unchecked;
            }
        };
        // A container missing at launch is a race, not proof it lacks the mount.
        let inspected = match container.inspect() {
            Ok(Some(inspected)) => inspected,
            Ok(None) => {
                tracing::warn!(target: "plugin.mcp", session = %session_id, container = %container.name,
                    "cannot check the sandbox's plugin MCP socket: container not found");
                return SandboxSocket::Unchecked;
            }
            Err(error) => {
                tracing::warn!(target: "plugin.mcp", session = %session_id, container = %container.name, %error,
                    "cannot check the sandbox's plugin MCP socket: inspect failed");
                return SandboxSocket::Unchecked;
            }
        };
        socket_reach(&inspected, &dir, || shares_kernel(&container))
    }

    /// The container must mount this daemon's directory and run on the host kernel: a
    /// VM-backed runtime (Docker Desktop, a Podman machine, Apple Container) cannot connect
    /// a host socket. `same_kernel` is `None` when that cannot be checked.
    pub(crate) fn socket_reach(
        inspected: &InspectedContainer,
        dir: &Path,
        same_kernel: impl FnOnce() -> Option<bool>,
    ) -> SandboxSocket {
        let ours = |host: &str| {
            Path::new(host) == dir
                || std::fs::canonicalize(host)
                    .ok()
                    .zip(std::fs::canonicalize(dir).ok())
                    .is_some_and(|(host, dir)| host == dir)
        };
        let mounted = inspected
            .bind_mounts
            .iter()
            .any(|m| m.container_path == CONTAINER_DIR && ours(&m.host_path));
        if !mounted {
            return SandboxSocket::Unmounted;
        }
        if inspected.runtime_handler.is_some() {
            return SandboxSocket::OtherKernel;
        }
        match same_kernel() {
            Some(true) => SandboxSocket::Reachable,
            Some(false) => SandboxSocket::OtherKernel,
            None => SandboxSocket::Unchecked,
        }
    }

    /// Docker and Podman report no VM in inspect, but a container on another kernel sees
    /// another boot id. `None` when either boot id cannot be read (no `cat`, a stopped
    /// container, a timeout).
    fn shares_kernel(container: &crate::containers::DockerContainer) -> Option<bool> {
        let host = crate::process::boot_id()?;
        let cmd = [
            "cat".to_string(),
            "/proc/sys/kernel/random/boot_id".to_string(),
        ];
        let argv = container.build_exec_argv("", &cmd);
        let (program, args) = argv.split_first()?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let out = crate::process::run_with_timeout(&mut command, std::time::Duration::from_secs(5))
            .ok()
            .flatten()
            .filter(|out| out.status.success())?;
        let seen = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!seen.is_empty()).then(|| seen == host.trim())
    }

    pub struct PluginSocket {
        listener: UnixListener,
        dir: OwnedFd,
        inode: (u64, u64),
    }

    /// Binds the socket in the app dir.
    pub(crate) fn bind() -> anyhow::Result<Option<PluginSocket>> {
        bind_in(&host_dir()?)
    }

    /// Binds `dir/mcp.sock` with mode 0600, replacing a stale socket, then writes the
    /// forwarder. `None`, touching nothing, when another process still answers on it.
    pub(crate) fn bind_in(dir: &Path) -> anyhow::Result<Option<PluginSocket>> {
        let (dir, _) = open_dir(dir)?;
        let path = anchored(&dir, SOCKET_FILE)?;
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_socket()
                && std::os::unix::net::UnixStream::connect(&path).is_ok()
            {
                return Ok(None);
            }
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        let ready = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| std::fs::symlink_metadata(&path))
            .and_then(|meta| write_forwarder(&dir).map(|()| meta));
        match ready {
            Ok(meta) => Ok(Some(PluginSocket {
                listener,
                dir,
                inode: (meta.dev(), meta.ino()),
            })),
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                Err(error.into())
            }
        }
    }

    /// Serves until the daemon shuts down, then removes the socket if it is still this one.
    pub(crate) async fn serve(state: Arc<AppState>, socket: PluginSocket) {
        let PluginSocket {
            listener,
            dir,
            inode,
        } = socket;
        let shutdown = state.shutdown.clone();
        if let Err(error) = axum::serve(listener, router(state).into_make_service())
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
        {
            tracing::warn!(target: "plugin.mcp", %error, "plugin MCP socket stopped");
        }
        if let Ok(path) = anchored(&dir, SOCKET_FILE) {
            if std::fs::symlink_metadata(&path).is_ok_and(|m| (m.dev(), m.ino()) == inode) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Only `/mcp/plugins/{plugin}/{server}`, and only for a live derived credential: no
    /// daemon token, no login session, no loopback trust.
    pub(crate) fn router(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/mcp/plugins/{plugin}/{server}",
                axum::routing::any(crate::server::mcp_plugins::plugin_endpoint),
            )
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                socket_auth,
            ))
            .with_state(state)
    }

    async fn socket_auth(
        State(state): State<Arc<AppState>>,
        mut request: Request,
        next: Next,
    ) -> Response {
        // CityHall denies plugin endpoints on every listener, as its TCP gate does.
        if state.cityhall_mode {
            return crate::server::api::cityhall_response();
        }
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok());
        if host != Some(SOCKET_HOST) {
            return crate::server::access::access_denied();
        }
        match crate::server::auth::socket_plugin_caller(&state, &mut request).await {
            Some(caller) => {
                request.extensions_mut().insert(caller);
                next.run(request).await
            }
            None => (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": "unauthorized",
                    "message": "Invalid or missing plugin MCP credential"
                })),
            )
                .into_response(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::server::session_credential::McpSessionSecret;
        use crate::server::test_helpers::vecs;
        use crate::server::test_support;
        use crate::session::Instance;
        use serde_json::Value;
        use tower::ServiceExt;

        const TOKEN: &str = "socket-test-token";

        /// `/mcp` stays off: the socket serves plugins regardless of `web.mcp_enabled`.
        fn state(prior: Vec<Instance>, cityhall: bool) -> (Arc<AppState>, Arc<McpSessionSecret>) {
            let dir = tempfile::tempdir().unwrap();
            let secret =
                Arc::new(McpSessionSecret::load_or_generate(&dir.path().join("secret")).unwrap());
            let state = test_support::build_test_app_state_with_policy_configured(
                prior,
                vecs(&["localhost"]),
                Vec::new(),
                Some(TOKEN.to_string()),
                |state| {
                    state.web_config.mcp_enabled = false;
                    state.mcp_session_secret = Some(secret.clone());
                    state.cityhall_mode = cityhall;
                },
            );
            (state, secret)
        }

        async fn call(
            app: &axum::Router,
            uri: &str,
            host: Option<&str>,
            bearer: Option<&str>,
            session: Option<&str>,
        ) -> (StatusCode, Value) {
            let mut req = axum::http::Request::builder().method("POST").uri(uri);
            if let Some(host) = host {
                req = req.header(header::HOST, host);
            }
            if let Some(bearer) = bearer {
                req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
            }
            if let Some(session) = session {
                req = req.header("x-aoe-session", session);
            }
            let resp = app
                .clone()
                .oneshot(req.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
        }

        /// Gating runs against the live registry, so the env and registry are isolated.
        #[tokio::test]
        #[serial_test::serial]
        async fn socket_router_serves_only_live_derived_credentials_on_plugin_paths() {
            let temp = tempfile::tempdir().unwrap();
            let _reload = crate::plugin::ReloadRegistryOnDrop;
            let _env = crate::session::test_support::EnvGuard::set(&[
                ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
                ("HOME", temp.path().to_path_buf()),
                ("USERPROFILE", temp.path().to_path_buf()),
            ]);
            let worker = r#"["sleep", "600"]"#;
            crate::plugin::install_test_plugin(
                "acme.serve",
                22,
                &["runtime.worker", "mcp.serve"],
                worker,
            );
            crate::plugin::install_test_plugin("acme.plain", 20, &["runtime.worker"], worker);
            crate::plugin::reload_registry();

            let live = Instance::new("live", "/repo");
            let id = live.id.clone();
            let other = Instance::new("other", "/repo");
            let other_id = other.id.clone();
            let mut trashed = Instance::new("trashed", "/repo");
            trashed.trash();
            let trashed_id = trashed.id.clone();
            let (state, secret) = state(vec![live.clone(), other, trashed], false);
            let app = router(state);
            let derived =
                |p: &str, s: &str, id: &str| secret.plugin_server_credential_for(p, s, id);
            let endpoint = "/mcp/plugins/acme.serve/srv";
            let host = Some(SOCKET_HOST);
            let ok = Some(derived("acme.serve", "srv", &id));

            // (case, uri, host, bearer, session, status)
            type Case<'a> = (
                &'a str,
                &'a str,
                Option<&'a str>,
                Option<String>,
                Option<&'a str>,
                StatusCode,
            );
            let cases: Vec<Case> = vec![
                // Gating passed; this test state runs no plugin host.
                (
                    "derived credential",
                    endpoint,
                    host,
                    ok.clone(),
                    Some(&id),
                    StatusCode::SERVICE_UNAVAILABLE,
                ),
                (
                    "plugin without mcp.serve",
                    "/mcp/plugins/acme.plain/srv",
                    host,
                    Some(derived("acme.plain", "srv", &id)),
                    Some(&id),
                    StatusCode::NOT_FOUND,
                ),
                (
                    "other host",
                    endpoint,
                    Some("localhost"),
                    ok.clone(),
                    Some(&id),
                    StatusCode::FORBIDDEN,
                ),
                (
                    "no host",
                    endpoint,
                    None,
                    ok.clone(),
                    Some(&id),
                    StatusCode::FORBIDDEN,
                ),
                (
                    "no credential",
                    endpoint,
                    host,
                    None,
                    None,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "daemon token",
                    endpoint,
                    host,
                    Some(TOKEN.into()),
                    None,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "plain session credential",
                    endpoint,
                    host,
                    Some(secret.credential_for(&id)),
                    Some(&id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "derived without session header",
                    endpoint,
                    host,
                    ok.clone(),
                    None,
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "other server",
                    "/mcp/plugins/acme.serve/other",
                    host,
                    ok.clone(),
                    Some(&id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "other plugin",
                    "/mcp/plugins/acme.plain/srv",
                    host,
                    ok.clone(),
                    Some(&id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "other session header",
                    endpoint,
                    host,
                    ok.clone(),
                    Some(&other_id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "trashed session",
                    endpoint,
                    host,
                    Some(derived("acme.serve", "srv", &trashed_id)),
                    Some(&trashed_id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "third path segment",
                    "/mcp/plugins/acme.serve/srv/x",
                    host,
                    ok.clone(),
                    Some(&id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "mcp root",
                    "/mcp",
                    host,
                    Some(secret.credential_for(&id)),
                    Some(&id),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    "rest api",
                    "/api/sessions",
                    host,
                    Some(TOKEN.into()),
                    None,
                    StatusCode::UNAUTHORIZED,
                ),
            ];
            for (case, uri, host, bearer, session, want) in cases {
                let (status, body) = call(&app, uri, host, bearer.as_deref(), session).await;
                assert_eq!(status, want, "{case}: {body}");
            }

            // CityHall denies plugin endpoints everywhere, a valid credential included.
            let (state, secret) = self::state(vec![live], true);
            let cred = secret.plugin_server_credential_for("acme.serve", "srv", &id);
            let (status, body) = call(&router(state), endpoint, host, Some(&cred), Some(&id)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "cityhall: {body}");
            assert_eq!(body["error"], "cityhall_mode");
        }

        #[tokio::test]
        async fn bind_is_private_replaces_a_stale_socket_and_spares_a_live_one() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("plugin-mcp");
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

            let first = bind_in(&dir).unwrap().expect("a fresh directory binds");
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join(SOCKET_FILE)), 0o600);
            assert_eq!(
                std::fs::read_to_string(dir.join(FORWARDER_FILE)).unwrap(),
                FORWARDER
            );
            assert!(
                bind_in(&dir).unwrap().is_none(),
                "a socket that still answers is left alone"
            );

            // Dropping the listener leaves the file behind, like a crashed daemon.
            drop(first);
            assert!(dir.join(SOCKET_FILE).exists());
            std::fs::write(dir.join(FORWARDER_FILE), "stale").unwrap();
            let second = bind_in(&dir).unwrap().expect("a stale socket is replaced");
            assert_eq!(
                std::fs::read_to_string(dir.join(FORWARDER_FILE)).unwrap(),
                FORWARDER,
                "the forwarder is rewritten"
            );

            let (state, _) = state(Vec::new(), false);
            let task = tokio::spawn(serve(state.clone(), second));
            state.shutdown.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(10), task)
                .await
                .expect("serve stops on shutdown")
                .unwrap();
            assert!(
                !dir.join(SOCKET_FILE).exists(),
                "shutdown removes the socket"
            );
            assert!(
                dir.join(FORWARDER_FILE).exists(),
                "the mounted directory stays"
            );
        }

        #[test]
        fn reach_needs_this_daemons_mount_on_the_same_kernel() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("plugin-mcp");
            std::fs::create_dir(&dir).unwrap();
            let other = temp.path().join("other-app/plugin-mcp");
            let inspected =
                |host: &Path, container: &str, handler: Option<&str>| InspectedContainer {
                    id: "c".into(),
                    running: true,
                    bind_mounts: vec![VolumeMount {
                        host_path: host.to_string_lossy().into_owned(),
                        container_path: container.into(),
                        read_only: true,
                    }],
                    ordinary_mounts: Vec::new(),
                    opaque_mounts: Vec::new(),
                    runtime_handler: handler.map(str::to_owned),
                };
            // (case, container, same kernel: None = could not check, want)
            let cases = [
                (
                    "this daemon's mount",
                    inspected(&dir, CONTAINER_DIR, None),
                    Some(true),
                    SandboxSocket::Reachable,
                ),
                (
                    "no mount",
                    inspected(&dir, "/elsewhere", None),
                    Some(true),
                    SandboxSocket::Unmounted,
                ),
                (
                    "another app dir",
                    inspected(&other, CONTAINER_DIR, None),
                    Some(true),
                    SandboxSocket::Unmounted,
                ),
                (
                    "another boot id",
                    inspected(&dir, CONTAINER_DIR, None),
                    Some(false),
                    SandboxSocket::OtherKernel,
                ),
                (
                    "VM runtime handler",
                    inspected(&dir, CONTAINER_DIR, Some("vm")),
                    None,
                    SandboxSocket::OtherKernel,
                ),
                (
                    "boot id unreadable",
                    inspected(&dir, CONTAINER_DIR, None),
                    None,
                    SandboxSocket::Unchecked,
                ),
            ];
            for (case, container, same_kernel, want) in cases {
                assert_eq!(
                    socket_reach(&container, &dir, || same_kernel),
                    want,
                    "{case}"
                );
            }
        }

        /// Concurrent writers never trip over each other's temporary file.
        #[test]
        fn concurrent_forwarder_writes_all_succeed() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("plugin-mcp");
            for _ in 0..50 {
                if dir.exists() {
                    std::fs::write(dir.join(FORWARDER_FILE), "stale").unwrap();
                }
                let results: Vec<_> = std::thread::scope(|s| {
                    let handles: Vec<_> = (0..8)
                        .map(|_| {
                            s.spawn(|| open_dir(&dir).and_then(|(fd, _)| write_forwarder(&fd)))
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                for result in results {
                    result.expect("every concurrent write succeeds");
                }
                assert_eq!(
                    std::fs::read_to_string(dir.join(FORWARDER_FILE)).unwrap(),
                    FORWARDER
                );
            }
        }

        /// A directory others can write is not trusted, and the log says how to fix it.
        #[test]
        fn open_dir_refuses_a_directory_others_can_write() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("plugin-mcp");
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            let error = open_dir(&dir).unwrap_err().to_string();
            assert!(error.contains("chmod go-w"), "{error}");
            assert!(!dir.join(FORWARDER_FILE).exists());
        }

        /// Only a daemon that bound the socket writes the forwarder: building a sandbox config
        /// in any process leaves another daemon's forwarder alone, and the mount path is the
        /// same before any daemon ran.
        #[test]
        #[serial_test::serial]
        fn sandbox_volume_never_writes_the_forwarder() {
            let temp = tempfile::tempdir().unwrap();
            let _env = crate::session::test_support::EnvGuard::set(&[
                ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
                ("HOME", temp.path().to_path_buf()),
            ]);
            let dir = real_path(&host_dir().unwrap()).unwrap();
            let expected = VolumeMount {
                host_path: dir.to_string_lossy().into_owned(),
                container_path: CONTAINER_DIR.to_string(),
                read_only: true,
            };

            assert_eq!(
                sandbox_volume(),
                Some(expected.clone()),
                "before any daemon"
            );
            assert!(dir.is_dir());
            assert!(!dir.join(FORWARDER_FILE).exists());

            std::fs::write(dir.join(FORWARDER_FILE), "theirs").unwrap();
            assert_eq!(sandbox_volume(), Some(expected));
            assert_eq!(
                std::fs::read_to_string(dir.join(FORWARDER_FILE)).unwrap(),
                "theirs"
            );
        }

        /// Another daemon's live socket is left alone, and so is its forwarder.
        #[tokio::test]
        async fn a_live_socket_keeps_its_forwarder() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("plugin-mcp");
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            let _live = UnixListener::bind(dir.join(SOCKET_FILE)).unwrap();
            std::fs::write(dir.join(FORWARDER_FILE), "theirs").unwrap();
            assert!(bind_in(&dir).unwrap().is_none());
            assert_eq!(
                std::fs::read_to_string(dir.join(FORWARDER_FILE)).unwrap(),
                "theirs"
            );
        }

        #[test]
        fn open_dir_refuses_a_non_directory() {
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("plugin-mcp");
            std::fs::write(&file, "x").unwrap();
            assert!(open_dir(&file).is_err());
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(temp.path(), &link).unwrap();
            assert!(open_dir(&link).is_err());
        }

        /// A link in the configured path is resolved first, so it does not hide a bad real
        /// ancestor and does not cost a good one its mount.
        #[test]
        fn open_dir_follows_a_linked_ancestor_and_checks_the_real_path() {
            let temp = tempfile::tempdir().unwrap();
            let real = temp.path().join("real");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&real)
                .unwrap();
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();

            let dir = link.join("plugin-mcp");
            let (_, checked) =
                open_dir(&dir).expect("a linked ancestor with a private real path is accepted");
            assert_eq!(
                checked,
                std::fs::canonicalize(&real).unwrap().join("plugin-mcp")
            );
            assert!(real.join("plugin-mcp").is_dir());

            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o770)).unwrap();
            let error = open_dir(&dir).unwrap_err().to_string();
            let real = std::fs::canonicalize(&real).unwrap();
            assert!(error.contains(&real.display().to_string()), "{error}");
            assert!(error.contains("chmod go-w"), "{error}");
        }

        /// A container runtime resolves links again at mount time, so the mount source is the
        /// checked real path, never the path as configured.
        #[test]
        #[serial_test::serial]
        fn sandbox_volume_mounts_the_real_path_behind_a_linked_app_dir() {
            let temp = tempfile::tempdir().unwrap();
            let real = std::fs::canonicalize(temp.path()).unwrap().join("real");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&real)
                .unwrap();
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let _env = crate::session::test_support::EnvGuard::set(&[
                ("XDG_CONFIG_HOME", link.clone()),
                ("HOME", link.clone()),
            ]);
            let configured = host_dir().unwrap();
            assert!(configured.starts_with(&link), "{configured:?}");

            let volume = sandbox_volume().expect("a linked app dir still gets the mount");
            let mounted = PathBuf::from(&volume.host_path);
            assert!(mounted.starts_with(&real), "{mounted:?}");
            assert_ne!(mounted, configured);
            assert_eq!(mounted, std::fs::canonicalize(&configured).unwrap());
            assert!(std::fs::symlink_metadata(&mounted).unwrap().is_dir());
        }

        #[test]
        fn open_dir_refuses_a_directory_another_user_owns() {
            if nix::unistd::geteuid().is_root() {
                return;
            }
            let error = open_dir(Path::new("/usr/bin")).unwrap_err().to_string();
            assert!(error.contains("owned by another user"), "{error}");
        }
    }
}
