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
use crate::session::anchored_fs::{FileExchange, FileExchangeResult, FileExchangeUncertain};
use crate::session::{AnchoredDir, Instance};

// Serialize dashboard saves even when two Sessions share a Working directory.
static SAVES: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
type FileError = (StatusCode, &'static str);
const EXCHANGE_UNSUPPORTED: &str = "This filesystem does not support atomic file replacement";

#[derive(Debug, thiserror::Error)]
#[error("{1}")]
struct SaveError(StatusCode, String);

impl From<FileError> for SaveError {
    fn from((status, message): FileError) -> Self {
        Self(status, message.into())
    }
}

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

fn exchange_reason(confined: &Confined) -> Option<&'static str> {
    let support = || -> anyhow::Result<bool> {
        let root = AnchoredDir::open(&confined.root)?;
        let relative = confined.canonical.strip_prefix(&confined.root)?;
        root.child(relative.parent().unwrap_or(FsPath::new("")))?
            .supports_file_exchange()
    };
    match support() {
        Ok(true) => None,
        Ok(false) => Some(EXCHANGE_UNSUPPORTED),
        Err(_) => Some("Cannot verify atomic file replacement in this folder"),
    }
}

fn opened_file_version(file: &mut std::fs::File, path: &FsPath) -> anyhow::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(MAX_CONTENTS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > MAX_CONTENTS_BYTES
        || before.len() != after.len()
        || before.modified()? != after.modified()?
        || before.permissions() != after.permissions()
    {
        return Ok(None);
    }
    Ok(Some(version(
        &bytes,
        path,
        &cap_std::fs::Metadata::from_just_metadata(after),
    )))
}

fn publication_error(error: anyhow::Error) -> SaveError {
    tracing::warn!(target: "http.api.sessions", %error, "file save could not be confirmed");
    if error.downcast_ref::<FileExchangeUncertain>().is_none() {
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            if crate::process::exchange_unsupported(error) {
                return (StatusCode::FORBIDDEN, EXCHANGE_UNSUPPORTED).into();
            }
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) {
                return io_error(std::io::Error::from(error.kind())).into();
            }
        }
    }
    match error.downcast::<SaveError>() {
        Ok(error) => error,
        Err(error) => SaveError(StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not confirm file save: {error:#}. Your edits are kept; check the file before retrying.")),
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
        instance
            .and_then(|inst| sandbox_reason(inst, &confined.canonical))
            .or_else(|| exchange_reason(confined))
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
) -> Result<(), SaveError> {
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
        )
            .into());
    }
    let path = FsPath::new(&request.path);
    let confined = project_file(roots, path)?;
    // Outside `SAVES`: a runtime probe can take a minute. The caller's instance
    // lock keeps the container from being replaced before publication.
    if let Some(reason) = sandbox_reason(instance, &confined.canonical) {
        return Err((StatusCode::FORBIDDEN, reason).into());
    }
    let _save = SAVES.lock();
    // Revalidate while the draft stage is private; verify the displaced inode after exchange.
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
            current.read.metadata.mode() & 0o777,
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
    let validate = || {
        check()
            .map(drop)
            .map_err(|error| SaveError::from(error).into())
    };
    let file_version = |file: &mut std::fs::File| opened_file_version(file, &confined.canonical);
    match parent.exchange_file(
        FsPath::new(relative.file_name().ok_or((StatusCode::FORBIDDEN, "Cannot replace project root"))?),
        &mut request.content.as_bytes(),
        permissions,
        FileExchange { validate: &validate, version: &file_version, expected: &request.version },
    ).map_err(publication_error)? {
        FileExchangeResult::Saved => Ok(()),
        FileExchangeResult::Conflict { preserved: None } => Err(CHANGED.into()),
        FileExchangeResult::Conflict { preserved: Some(path) } => Err(SaveError(
            StatusCode::CONFLICT,
            format!("File changed during save. A displaced version was preserved at {}. Copy your edits or reload.", path.display()),
        )),
    }
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
    // Keep one Sandbox through publication, even if the request is cancelled.
    let guard = state.instance_lock(&id).await.lock_owned().await;
    let Some(instance) = find_instance(&state, &id).await else {
        return crate::server::api::session_not_found();
    };
    match tokio::task::spawn_blocking(move || {
        let _guard = guard;
        save_file(&scope.roots, &instance, request)
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(SaveError(status, message))) => api_error(status, "file_save", message),
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
        fs::set_permissions(&target, fs::Permissions::from_mode(0o6751)).unwrap();
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
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
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
                .filter(|result| matches!(result, Err(SaveError(StatusCode::CONFLICT, _))))
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

    struct ResetExchangeHooks;

    impl Drop for ResetExchangeHooks {
        fn drop(&mut self) {
            crate::session::anchored_fs::FILE_EXCHANGE_HOOK.set(None);
            crate::session::anchored_fs::FAIL_SYNC_ONCE.set(None);
            crate::process::EXCHANGE_ERROR.set(None);
        }
    }

    #[test]
    fn exchange_preserves_writers_across_validation_verification_and_undo() {
        use crate::session::anchored_fs::{FileExchangePhase as Phase, FILE_EXCHANGE_HOOK};
        use std::os::unix::fs::FileExt;
        for (case, expected, recovery) in [
            ("in-place before exchange", "external", None),
            ("rename before exchange", "external", None),
            ("recreate before exchange", "external", None),
            ("held writer after exchange", "external", None),
            ("third writer after exchange", "third", Some("original")),
            ("third writer before undo", "third", Some("external")),
            ("third writer during undo", "external", Some("third")),
            ("in-place writer during undo", "external", Some("third")),
            ("in-place writer after publication", "third", None),
        ] {
            let _hooks = ResetExchangeHooks;
            let (_dir, roots, instance) = fixture();
            let target = roots[0].join("a.txt");
            fs::write(&target, "original").unwrap();
            let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
            let modified = fs::metadata(&target).unwrap().modified().unwrap();
            let held = fs::OpenOptions::new().write(true).open(&target).unwrap();
            if case == "third writer after exchange" {
                fs::write(roots[0].join("a.txt.aoe-conflict"), "earlier conflict").unwrap();
            }
            FILE_EXCHANGE_HOOK.set(Some(Box::new(move |phase, target, _stage| {
                let replace = |text: &str| {
                    let temporary = target.with_extension("agent-stage");
                    fs::write(&temporary, text).unwrap();
                    fs::rename(temporary, target).unwrap();
                };
                if phase == Phase::BeforeExchange {
                    match case {
                        "in-place before exchange" => {
                            fs::write(target, "external").unwrap();
                            fs::File::options()
                                .write(true)
                                .open(target)
                                .unwrap()
                                .set_times(fs::FileTimes::new().set_modified(modified))
                                .unwrap();
                        }
                        "rename before exchange" => replace("external"),
                        "recreate before exchange" => {
                            fs::remove_file(target).unwrap();
                            fs::write(target, "external").unwrap();
                        }
                        "third writer before undo"
                        | "third writer during undo"
                        | "in-place writer during undo" => {
                            fs::write(target, "external").unwrap();
                        }
                        _ => {}
                    }
                }
                if phase == Phase::AfterExchange {
                    match case {
                        "held writer after exchange" => {
                            assert_eq!(held.write_at(b"external", 0).unwrap(), 8);
                        }
                        "third writer after exchange" => replace("third"),
                        "in-place writer during undo" | "in-place writer after publication" => {
                            fs::write(target, "third").unwrap();
                        }
                        _ => {}
                    }
                }
                if phase == Phase::BeforeUndo && case == "third writer before undo" {
                    replace("third");
                }
                if phase == Phase::UndoExchange && case == "third writer during undo" {
                    replace("third");
                }
            })));
            let result = save_file(&roots, &instance, request("a.txt", &opened, "browser"));
            FILE_EXCHANGE_HOOK.set(None);
            if case == "in-place writer after publication" {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT, "{case}");
            }
            assert_eq!(fs::read_to_string(&target).unwrap(), expected, "{case}");
            if let Some(recovery) = recovery {
                let suffix = if case == "third writer after exchange" {
                    ".1"
                } else {
                    ""
                };
                assert_eq!(
                    fs::read_to_string(roots[0].join(format!("a.txt.aoe-conflict{suffix}")))
                        .unwrap(),
                    recovery,
                    "{case}"
                );
                if !suffix.is_empty() {
                    assert_eq!(
                        fs::read_to_string(roots[0].join("a.txt.aoe-conflict")).unwrap(),
                        "earlier conflict"
                    );
                }
            } else {
                assert_eq!(
                    fs::read_dir(&roots[0]).unwrap().count(),
                    1,
                    "{case}: verified stages are removed"
                );
            }
        }
    }

    #[test]
    fn exchange_restores_special_entries_without_following_or_unlinking_them() {
        use crate::session::anchored_fs::{FileExchangePhase, FILE_EXCHANGE_HOOK};
        for kind in ["symlink", "directory", "fifo"] {
            let _hooks = ResetExchangeHooks;
            let (_dir, roots, instance) = fixture();
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("secret");
            fs::write(&secret, "outside").unwrap();
            let target = roots[0].join("a.txt");
            fs::write(&target, "original").unwrap();
            let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
            let secret_for_hook = secret.clone();
            FILE_EXCHANGE_HOOK.set(Some(Box::new(move |phase, target, _| {
                if phase == FileExchangePhase::BeforeExchange {
                    fs::remove_file(target).unwrap();
                    match kind {
                        "symlink" => std::os::unix::fs::symlink(&secret_for_hook, target).unwrap(),
                        "directory" => fs::create_dir(target).unwrap(),
                        _ => nix::unistd::mkfifo(
                            target,
                            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
                        )
                        .unwrap(),
                    }
                }
            })));
            assert_eq!(
                save_file(&roots, &instance, request("a.txt", &opened, "browser"))
                    .unwrap_err()
                    .0,
                StatusCode::CONFLICT,
                "{kind}"
            );
            let metadata = fs::symlink_metadata(&target).unwrap();
            use std::os::unix::fs::FileTypeExt;
            match kind {
                "symlink" => assert!(metadata.is_symlink()),
                "directory" => assert!(metadata.is_dir()),
                _ => assert!(metadata.file_type().is_fifo()),
            }
            assert_eq!(fs::read_to_string(&secret).unwrap(), "outside");
            assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 1, "{kind}");
        }
    }

    #[test]
    fn failed_undo_preserves_the_displaced_version_and_reports_its_path() {
        use crate::session::anchored_fs::{FileExchangePhase, FILE_EXCHANGE_HOOK};
        let _hooks = ResetExchangeHooks;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        FILE_EXCHANGE_HOOK.set(Some(Box::new(|phase, target, _| {
            if phase == FileExchangePhase::BeforeExchange {
                fs::write(target, "external").unwrap();
            }
            if phase == FileExchangePhase::BeforeUndo {
                crate::process::EXCHANGE_ERROR.set(Some(libc::EIO));
            }
        })));
        let error = save_file(&roots, &instance, request("a.txt", &opened, "browser")).unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        let recovery = roots[0].join("a.txt.aoe-conflict");
        assert_eq!(fs::read_to_string(&recovery).unwrap(), "external");
        assert!(error.1.contains(recovery.to_str().unwrap()), "{}", error.1);
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "browser"
        );
    }

    #[test]
    fn exhausted_recovery_names_never_remove_the_unverified_displaced_file() {
        use crate::session::anchored_fs::{FileExchangePhase, FILE_EXCHANGE_HOOK};
        let _hooks = ResetExchangeHooks;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        for n in 0..100 {
            let suffix = if n == 0 {
                String::new()
            } else {
                format!(".{n}")
            };
            fs::write(
                roots[0].join(format!("a.txt.aoe-conflict{suffix}")),
                "earlier",
            )
            .unwrap();
        }
        FILE_EXCHANGE_HOOK.set(Some(Box::new(|phase, target, _| {
            if phase == FileExchangePhase::AfterExchange {
                let other = target.with_extension("other");
                fs::write(&other, "third").unwrap();
                fs::rename(other, target).unwrap();
            }
        })));
        let error = save_file(&roots, &instance, request("a.txt", &opened, "browser")).unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        let staged = fs::read_dir(&roots[0])
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".aoe-copy-")
            })
            .unwrap();
        assert_eq!(fs::read_to_string(&staged).unwrap(), "original");
        assert!(error.1.contains(staged.to_str().unwrap()), "{}", error.1);
        assert_eq!(fs::read_to_string(roots[0].join("a.txt")).unwrap(), "third");
        assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 102);
    }

    #[test]
    fn unsupported_exchange_disables_editing_and_leaves_the_target_untouched() {
        for errno in [libc::EINVAL, libc::EOPNOTSUPP, libc::ENOSYS] {
            let _hooks = ResetExchangeHooks;
            let (_dir, roots, instance) = fixture();
            fs::write(roots[0].join("a.txt"), "original").unwrap();
            let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
            crate::process::EXCHANGE_ERROR.set(Some(errno));
            let disabled = snapshot(&roots, &instance, "a.txt");
            assert_eq!(
                disabled.edit_disabled_reason.as_deref(),
                Some(EXCHANGE_UNSUPPORTED)
            );
            assert_eq!(disabled.version.as_deref(), Some(opened.as_str()));
            let error =
                save_file(&roots, &instance, request("a.txt", &opened, "browser")).unwrap_err();
            assert_eq!(error.0, StatusCode::FORBIDDEN);
            assert_eq!(
                fs::read_to_string(roots[0].join("a.txt")).unwrap(),
                "original"
            );
            assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 1);
        }
    }

    #[test]
    fn an_exchange_that_becomes_unsupported_after_the_probe_never_falls_back() {
        use crate::session::anchored_fs::{FileExchangePhase, FILE_EXCHANGE_HOOK};
        let _hooks = ResetExchangeHooks;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        FILE_EXCHANGE_HOOK.set(Some(Box::new(|phase, _, _| {
            if phase == FileExchangePhase::BeforeExchange {
                crate::process::EXCHANGE_ERROR.set(Some(libc::EOPNOTSUPP));
            }
        })));
        let error = save_file(&roots, &instance, request("a.txt", &opened, "browser")).unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert_eq!(error.1, EXCHANGE_UNSUPPORTED);
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 1);
    }

    #[test]
    fn a_rejected_draft_stage_never_receives_public_permissions() {
        use crate::session::anchored_fs::{FileExchangePhase, FILE_EXCHANGE_HOOK};
        let _hooks = ResetExchangeHooks;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        fs::set_permissions(roots[0].join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        FILE_EXCHANGE_HOOK.set(Some(Box::new(|phase, target, stage| {
            if phase == FileExchangePhase::Staged {
                assert_eq!(
                    fs::metadata(stage).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                fs::write(target, "external").unwrap();
            }
        })));
        let error = save_file(
            &roots,
            &instance,
            request("a.txt", &opened, "private draft"),
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "external"
        );
        assert_eq!(fs::read_dir(&roots[0]).unwrap().count(), 1);
    }

    #[test]
    fn sync_failure_after_publication_reports_uncertainty_without_a_false_conflict() {
        use crate::session::anchored_fs::{FileExchangePhase, FAIL_SYNC_ONCE, FILE_EXCHANGE_HOOK};
        let _hooks = ResetExchangeHooks;
        let (_dir, roots, instance) = fixture();
        fs::write(roots[0].join("a.txt"), "original").unwrap();
        let opened = snapshot(&roots, &instance, "a.txt").version.unwrap();
        FILE_EXCHANGE_HOOK.set(Some(Box::new(|phase, target, _| {
            if phase == FileExchangePhase::AfterExchange {
                FAIL_SYNC_ONCE.set(Some(target.parent().unwrap().to_path_buf()));
            }
        })));
        let error = save_file(&roots, &instance, request("a.txt", &opened, "browser")).unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error
                .1
                .contains("file was saved, but directory sync failed"),
            "{}",
            error.1
        );
        assert!(!error.1.contains("File changed on disk"));
        assert_eq!(
            fs::read_to_string(roots[0].join("a.txt")).unwrap(),
            "browser"
        );
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

    #[test]
    fn cancelled_handler_keeps_instance_locked_until_the_save_finishes() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;
        use std::time::Duration;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_dir, roots, instance) = fixture();
            let target = roots[0].join("a.txt");
            fs::write(&target, "original").unwrap();
            let opened = snapshot(&roots, &instance, "a.txt");
            let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
            let lock = state.instance_lock(&instance.id).await;
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            // Queue the save so cancellation cannot race its completion.
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
            });
            started_rx.await.unwrap();
            let mut handler = Box::pin(save_session_file(
                State(state),
                Path(instance.id),
                Ok(Json(request(
                    "a.txt",
                    opened.version.as_deref().unwrap(),
                    "updated",
                ))),
            ));
            poll_fn(|cx| {
                assert!(handler.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(lock.try_lock().is_err());
            drop(handler);
            assert!(
                lock.try_lock().is_err(),
                "cancelled request released the save lock"
            );
            assert_eq!(fs::read_to_string(&target).unwrap(), "original");
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            let _guard = tokio::time::timeout(Duration::from_secs(10), lock.lock())
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&target).unwrap(), "updated");
        });
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
