//! Existing project text files: versioned reads and confined atomic saves.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{rejection::JsonRejection, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::diff::{SessionFileResponse, SessionFileScope, MAX_CONTENTS_BYTES};
use crate::server::api::file_provenance::{self, Confined};
use crate::server::api::{api_error, cityhall_block, find_instance, read_only_block};
use crate::server::AppState;
use crate::session::anchored_fs::FilePublication;
use crate::session::{AnchoredDir, Instance};

// Serialize dashboard saves even when two Sessions share a Working directory.
static SAVES: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
type FileError = (StatusCode, &'static str);

#[derive(Deserialize)]
pub struct SaveSessionFileRequest {
    pub path: String,
    pub version: String,
    pub content: String,
}

fn io_error(error: std::io::Error) -> FileError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "Permission denied"),
        std::io::ErrorKind::NotFound => (
            StatusCode::CONFLICT,
            "File moved or deleted; reload before saving",
        ),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "File operation failed"),
    }
}

fn version(bytes: &[u8], path: &FsPath, metadata: &cap_std::fs::Metadata) -> String {
    use cap_std::fs::MetadataExt;
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(format!(
        "{}:{}:{}:{:?}:{:?}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.modified(),
        metadata.permissions()
    ));
    hash.update(bytes);
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn project_file(roots: &[PathBuf], path: &FsPath) -> Result<Confined, FileError> {
    let roots: Vec<_> = roots.iter().filter_map(|p| p.canonicalize().ok()).collect();
    file_provenance::confine_path(&roots, Default::default, path)
}

fn sandbox_reason(instance: &Instance, canonical: &FsPath) -> Option<&'static str> {
    if !instance.is_sandboxed() {
        return None;
    }
    let inspected = crate::containers::DockerContainer::from_session_id(&instance.id).inspect();
    let Ok(Some(container)) = inspected else {
        return Some("Cannot verify this Sandbox's shared files");
    };
    if instance
        .sandbox_info
        .as_ref()
        .and_then(|info| info.container_id.as_deref())
        != Some(container.id.as_str())
    {
        return Some("Cannot verify this Sandbox's shared files");
    }
    sandbox_mount_reason(&container, canonical)
}

fn sandbox_mount_reason(
    container: &crate::containers::InspectedContainer,
    canonical: &FsPath,
) -> Option<&'static str> {
    let bind = container
        .bind_mounts
        .iter()
        .filter_map(|bind| {
            let host = FsPath::new(&bind.host_path).canonicalize().ok()?;
            let relative = canonical.strip_prefix(&host).ok()?;
            Some((bind, relative, host.components().count()))
        })
        .max_by_key(|(_, _, depth)| *depth);
    let Some((bind, relative, _)) = bind else {
        return Some("This file is not shared with the Sandbox");
    };
    if relative.as_os_str().is_empty() {
        return Some("This file is mounted separately in the Sandbox");
    }
    let container_path = FsPath::new(&bind.container_path).join(relative);
    let mapped = crate::containers::container_interface::host_path_for_mounts(
        &container.bind_mounts,
        container
            .ordinary_mounts
            .iter()
            .chain(&container.opaque_mounts)
            .filter_map(|mount| mount.container_path.to_str()),
        &container_path,
        true,
    );
    let shared = mapped
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|path| path == canonical);
    if bind.read_only {
        Some("The Sandbox mounts this folder read-only")
    } else if !shared {
        Some("The Sandbox uses a separate volume for this folder")
    } else {
        None
    }
}

/// A confined file's content, edit version, and why editing is refused, if it is.
struct Snapshot {
    read: file_provenance::FileRead,
    version: Option<String>,
    disabled: Option<&'static str>,
}

fn snapshot(
    roots: &[PathBuf],
    confined: &Confined,
    instance: Option<&Instance>,
    read_only: bool,
) -> Result<Snapshot, FileError> {
    let read = file_provenance::read_confined(confined, MAX_CONTENTS_BYTES)?;
    let disabled = if read_only {
        Some("The dashboard is read-only")
    } else if read.truncated {
        Some("File exceeds the 5 MB editing limit")
    } else if read.is_binary {
        Some("Binary files cannot be edited")
    } else if !read.valid_utf8 {
        Some("Only UTF-8 text files can be edited")
    } else if project_file(roots, &confined.canonical).is_err() {
        Some("Files outside the Session's project are read-only")
    } else if read.metadata.permissions().readonly() {
        Some("File permissions are read-only")
    } else {
        instance.and_then(|inst| sandbox_reason(inst, &confined.canonical))
    };
    let version = (!read.truncated && !read.is_binary && read.valid_utf8)
        .then(|| version(read.content.as_bytes(), &confined.canonical, &read.metadata));
    Ok(Snapshot {
        read,
        version,
        disabled,
    })
}

pub(super) fn read_file_snapshot(
    roots: &[PathBuf],
    confined: &Confined,
    instance: Option<&Instance>,
    read_only: bool,
    editing: bool,
) -> Result<SessionFileResponse, FileError> {
    let (read, version, disabled) = if editing {
        let Snapshot {
            read,
            version,
            disabled,
        } = snapshot(roots, confined, instance, read_only)?;
        (read, version, disabled)
    } else {
        let read = file_provenance::read_confined(confined, MAX_CONTENTS_BYTES)?;
        (read, None, None)
    };
    Ok(SessionFileResponse {
        content: read.content,
        is_binary: read.is_binary,
        truncated: read.truncated,
        version,
        edit_disabled_reason: disabled.map(String::from),
    })
}

fn save_file(
    roots: &[PathBuf],
    instance: &Instance,
    request: SaveSessionFileRequest,
) -> Result<(), FileError> {
    use cap_std::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    const CHANGED: FileError = (
        StatusCode::CONFLICT,
        "File changed on disk. Copy your edits or reload",
    );
    if request.content.len() > MAX_CONTENTS_BYTES || request.content.contains('\0') {
        return Err((
            StatusCode::BAD_REQUEST,
            "Only UTF-8 text files up to 5 MB can be saved",
        ));
    }
    let path = FsPath::new(&request.path);
    let confined = project_file(roots, path)?;
    // Once, outside the lock: a runtime probe can take a minute, and mounts
    // change only with the container ID that `sandbox_reason` verifies.
    if let Some(reason) = sandbox_reason(instance, &confined.canonical) {
        return Err((StatusCode::FORBIDDEN, reason));
    }
    let _save = SAVES.lock();
    // Runs again immediately before publication: optimistic, not a lock on external writers.
    let check = || -> Result<std::fs::Permissions, FileError> {
        let current = project_file(roots, path)?;
        if current.canonical != confined.canonical {
            return Err(CHANGED);
        }
        let current = snapshot(roots, &current, None, false).map_err(|_| CHANGED)?;
        if current.version.as_deref() != Some(request.version.as_str()) {
            return Err(CHANGED);
        }
        if let Some(reason) = current.disabled {
            return Err((StatusCode::FORBIDDEN, reason));
        }
        Ok(std::fs::Permissions::from_mode(
            current.read.metadata.mode() & 0o7777,
        ))
    };
    let permissions = check()?;
    let relative = confined
        .canonical
        .strip_prefix(&confined.root)
        .map_err(|_| (StatusCode::FORBIDDEN, "Path escapes project"))?;
    // Atomic replacement also needs the target's own write permission, including ACLs.
    let directory =
        cap_std::fs::Dir::open_ambient_dir(&confined.root, cap_std::ambient_authority())
            .map_err(io_error)?;
    use cap_std::fs::OpenOptionsExt;
    directory
        .open_with(
            relative,
            cap_std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK),
        )
        .map_err(io_error)?;
    let root = AnchoredDir::open(&confined.root)
        .map_err(|_| (StatusCode::FORBIDDEN, "Project directory is unavailable"))?;
    let parent = root
        .child(relative.parent().unwrap_or(FsPath::new("")))
        .map_err(|_| (StatusCode::FORBIDDEN, "Cannot open file directory"))?;
    let validate = || check().map(drop).map_err(|(_, msg)| anyhow::anyhow!(msg));
    root.publish_file(
        relative,
        &mut request.content.as_bytes(),
        permissions,
        true,
        Some(FilePublication {
            staging: &parent,
            validate: &validate,
        }),
    )
    .map_err(|error| {
        tracing::warn!(target: "http.api.sessions", %error, "file save failed");
        check().err().unwrap_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save file; check directory permissions and free disk space",
        ))
    })?;
    Ok(())
}

pub async fn save_session_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SaveSessionFileRequest>, JsonRejection>,
) -> Response {
    if let Some(response) = read_only_block(&state).or_else(|| cityhall_block(&state)) {
        return response;
    }
    let Json(request) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let scope = match SessionFileScope::of(&state, &id).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let Some(instance) = find_instance(&state, &id).await else {
        return crate::server::api::session_not_found();
    };
    match tokio::task::spawn_blocking(move || save_file(&scope.roots, &instance, request)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err((status, message))) => api_error(status, "file_save", message),
        Err(error) => {
            tracing::error!(target: "http.api.sessions", %error, "file save panicked");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "File save failed",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, Vec<PathBuf>, Instance) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let instance = Instance::new("editor", root.to_str().unwrap());
        (dir, vec![root], instance)
    }

    fn snapshot(roots: &[PathBuf], instance: &Instance, path: &str) -> SessionFileResponse {
        let confined = project_file(roots, FsPath::new(path)).unwrap();
        read_file_snapshot(roots, &confined, Some(instance), false, true).unwrap()
    }

    fn request(path: &str, version: &str, content: &str) -> SaveSessionFileRequest {
        SaveSessionFileRequest {
            path: path.into(),
            version: version.into(),
            content: content.into(),
        }
    }

    #[test]
    fn saves_existing_text_atomically_preserving_mode_and_exact_bytes() {
        let (_dir, roots, instance) = fixture();
        fs::create_dir(roots[0].join("sub")).unwrap();
        let target = roots[0].join("sub/script.sh");
        fs::write(&target, "#!/bin/sh\r\necho old\r\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751)).unwrap();
        let original = snapshot(&roots, &instance, "sub/script.sh");
        let before = snapshot(&roots, &instance, "sub/script.sh");
        assert_eq!(
            original.version, before.version,
            "reading must not change the version"
        );
        let content = "#!/bin/sh\r\necho new\r\n";
        save_file(
            &roots,
            &instance,
            request(
                "sub/script.sh",
                original.version.as_deref().unwrap(),
                content,
            ),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), content.as_bytes());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_ne!(
            snapshot(&roots, &instance, "sub/script.sh").version,
            original.version
        );
        assert_eq!(
            fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1,
            "no staging files remain"
        );
    }

    #[test]
    fn refuses_stale_versions_replacements_deletions_and_read_only_files() {
        let (_dir, roots, instance) = fixture();
        let target = roots[0].join("a.txt");
        for action in ["edit", "replace", "delete", "permissions"] {
            fs::write(&target, "original").unwrap();
            let opened = snapshot(&roots, &instance, "a.txt");
            match action {
                "edit" => fs::write(&target, "agent update").unwrap(),
                "replace" => {
                    fs::remove_file(&target).unwrap();
                    fs::write(&target, "replacement").unwrap();
                }
                "delete" => fs::remove_file(&target).unwrap(),
                _ => fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap(),
            }
            let expected = fs::read(&target).ok();
            assert!(
                save_file(
                    &roots,
                    &instance,
                    request("a.txt", opened.version.as_deref().unwrap(), "user update")
                )
                .is_err(),
                "{action}"
            );
            assert_eq!(
                fs::read(&target).ok(),
                expected,
                "{action} must not be overwritten"
            );
            if target.exists() {
                fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
    }

    #[test]
    fn rejects_non_text_oversized_special_and_outside_paths() {
        let (_dir, roots, instance) = fixture();
        for (name, bytes) in [
            ("binary", vec![b'a', 0]),
            ("latin1", vec![0xff]),
            ("large", vec![b'a'; MAX_CONTENTS_BYTES + 1]),
        ] {
            fs::write(roots[0].join(name), bytes).unwrap();
            let opened = snapshot(&roots, &instance, name);
            assert!(opened.edit_disabled_reason.is_some(), "{name}");
            assert!(save_file(&roots, &instance, request(name, "anything", "update")).is_err());
        }
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, "secret").unwrap();
        std::os::unix::fs::symlink(&secret, roots[0].join("escape")).unwrap();
        for path in ["../secret", "escape", secret.to_str().unwrap(), "missing"] {
            assert!(
                save_file(&roots, &instance, request(path, "anything", "overwrite")).is_err(),
                "{path}"
            );
        }
        assert_eq!(fs::read_to_string(secret).unwrap(), "secret");
        assert!(!roots[0].join("missing").exists());
        fs::create_dir(roots[0].join("folder")).unwrap();
        assert!(save_file(&roots, &instance, request("folder", "v", "overwrite")).is_err());
        nix::unistd::mkfifo(&roots[0].join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(save_file(&roots, &instance, request("pipe", "v", "overwrite")).is_err());
    }

    #[test]
    fn transcript_provenance_never_grants_write_access() {
        let (_dir, roots, instance) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("plan.md");
        fs::write(&path, "plan").unwrap();
        let confined =
            file_provenance::confine_path(&roots, || [path.clone()].into_iter().collect(), &path)
                .unwrap();
        let opened = read_file_snapshot(&roots, &confined, Some(&instance), false, true).unwrap();
        assert_eq!(
            opened.edit_disabled_reason.as_deref(),
            Some("Files outside the Session's project are read-only")
        );
        assert!(save_file(
            &roots,
            &instance,
            request(
                path.to_str().unwrap(),
                opened.version.as_deref().unwrap(),
                "overwrite"
            )
        )
        .is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "plan");
    }

    #[test]
    fn concurrent_dashboard_saves_accept_only_one_opened_version() {
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = ["one", "two"]
                .into_iter()
                .map(|text| {
                    let (roots, instance, barrier, opened) = (&roots, &instance, &barrier, &opened);
                    scope.spawn(move || {
                        barrier.wait();
                        save_file(roots, instance, request("a.txt", opened, text))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err((StatusCode::CONFLICT, _))))
                .count(),
            1
        );
    }

    #[test]
    fn permission_and_size_failures_keep_the_original_and_remove_staging() {
        let (_dir, roots, instance) = fixture();
        let path = roots[0].join("a.txt");
        fs::write(&path, "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        for content in [
            String::from("text\0binary"),
            "a".repeat(MAX_CONTENTS_BYTES + 1),
        ] {
            assert!(save_file(&roots, &instance, request("a.txt", &opened, &content)).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        }
        // Some write bit exists, but the owner has no write access.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o464)).unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        assert_eq!(
            save_file(&roots, &instance, request("a.txt", &opened, "overwrite"))
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        fs::set_permissions(&roots[0], fs::Permissions::from_mode(0o555)).unwrap();
        let failed = save_file(&roots, &instance, request("a.txt", &opened, "overwrite"));
        fs::set_permissions(&roots[0], fs::Permissions::from_mode(0o755)).unwrap();
        assert!(failed.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 1);
    }

    #[test]
    fn inspected_sandbox_mounts_distinguish_shared_and_separate_copies() {
        use crate::containers::container_interface::{InspectedContainer, InspectedMount};
        use crate::containers::VolumeMount;
        let (_dir, roots, _instance) = fixture();
        let path = roots[0].join("a.txt");
        fs::write(&path, "a").unwrap();
        let mut container = InspectedContainer {
            id: "container".into(),
            running: true,
            runtime_handler: None,
            bind_mounts: vec![VolumeMount {
                host_path: roots[0].to_string_lossy().into_owned(),
                container_path: "/workspace".into(),
                read_only: false,
            }],
            ordinary_mounts: vec![],
            opaque_mounts: vec![],
        };
        assert_eq!(sandbox_mount_reason(&container, &path), None);
        container.bind_mounts[0].read_only = true;
        assert_eq!(
            sandbox_mount_reason(&container, &path),
            Some("The Sandbox mounts this folder read-only")
        );
        container.bind_mounts[0].read_only = false;
        for opaque in [false, true] {
            let mount = InspectedMount {
                kind: "volume".into(),
                name: None,
                source: None,
                container_path: "/workspace".into(),
                read_only: false,
            };
            if opaque {
                container.opaque_mounts.push(mount);
            } else {
                container.ordinary_mounts.push(mount);
            }
            assert_eq!(
                sandbox_mount_reason(&container, &path),
                Some("The Sandbox uses a separate volume for this folder")
            );
            container.ordinary_mounts.clear();
            container.opaque_mounts.clear();
        }
        container.bind_mounts.push(VolumeMount {
            host_path: path.to_string_lossy().into_owned(),
            container_path: "/workspace/a.txt".into(),
            read_only: false,
        });
        assert_eq!(
            sandbox_mount_reason(&container, &path),
            Some("This file is mounted separately in the Sandbox")
        );
    }

    #[tokio::test]
    async fn router_round_trips_versions_and_existing_text() {
        use axum::{
            body::{to_bytes, Body},
            http::Request,
        };
        use tower::ServiceExt;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        let router =
            crate::server::test_support::build_router_for_test(state).layer(axum::Extension(
                axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))),
            ));
        let endpoint = format!("/api/sessions/{}/file", instance.id);
        let get = || {
            Request::builder()
                .header("host", "127.0.0.1")
                .uri(format!("{endpoint}?path=a.txt&editing=true"))
                .body(Body::empty())
                .unwrap()
        };
        let response = router.clone().oneshot(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let opened: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
        let put = || {
            Request::builder().header("host", "127.0.0.1").method("PUT").uri(&endpoint).header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"path":"a.txt", "version":opened["version"], "content":"updated\n"}).to_string())).unwrap()
        };
        assert_eq!(
            router.clone().oneshot(put()).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            router.clone().oneshot(put()).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "updated\n"
        );
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .header("host", "127.0.0.1")
                    .uri(format!("{endpoint}?path=a.txt&version_only=true"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let current: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
        assert_eq!(current["content"], "");
        assert_ne!(current["version"], opened["version"]);
        let content = "x".repeat(1_100_000);
        let response = router.oneshot(Request::builder().header("host", "127.0.0.1").method("PUT").uri(&endpoint)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"path":"a.txt", "version":current["version"], "content":content}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(fs::read_to_string(roots[0].join("a.txt")).unwrap(), content);
    }

    #[tokio::test]
    async fn router_denies_cityhall_before_malformed_bodies() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let state =
            crate::server::test_support::build_test_app_state_cityhall(vec![instance.clone()]);
        let router =
            crate::server::test_support::build_router_for_test(state).layer(axum::Extension(
                axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))),
            ));
        let response = router
            .oneshot(
                Request::builder()
                    .header("host", "127.0.0.1")
                    .method("PUT")
                    .uri(format!("/api/sessions/{}/file", instance.id))
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 10_000)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "cityhall_mode");
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "original"
        );
    }
}
