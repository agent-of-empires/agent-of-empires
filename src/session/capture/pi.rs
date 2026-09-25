//! Pi-family session headers and the Pi sidecar poller.

use std::io::Read;
use std::path::Path;

use uuid::Uuid;

/// Leading lines and bytes scanned for a session header; the byte cap bounds
/// allocation on one hostile line.
pub(super) const PI_HEADER_SCAN_LINES: usize = 8;
pub(super) const PI_HEADER_SCAN_BYTES: usize = 64 * 1024;

/// `(id, cwd)` from the first session header line, opened without following symlinks.
pub(crate) fn extract_pi_header_fields(path: &Path) -> Option<(Option<String>, Option<String>)> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut reader = std::io::BufReader::new(file);
    let mut consumed = 0usize;
    for _ in 0..PI_HEADER_SCAN_LINES {
        let mut line = String::new();
        let mut limited =
            (&mut reader).take((PI_HEADER_SCAN_BYTES.saturating_sub(consumed) + 1) as u64);
        let read = std::io::BufRead::read_line(&mut limited, &mut line).ok()?;
        if read == 0 {
            return None;
        }
        consumed = consumed.saturating_add(read);
        if consumed > PI_HEADER_SCAN_BYTES {
            return None;
        }
        if let Some(header) = parse_pi_header_json(&line) {
            return Some(header);
        }
    }
    None
}

/// `(id, cwd)` of a `"type":"session"` record; `None` for any other line.
pub(super) fn parse_pi_header_json(line: &str) -> Option<(Option<String>, Option<String>)> {
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    if parsed.get("type")?.as_str()? != "session" {
        return None;
    }
    let session_id = parsed.get("id").and_then(|v| v.as_str()).map(String::from);
    let cwd = parsed
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    Some((session_id, cwd))
}

pub(super) fn extract_pi_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let uuid_part = stem.rsplit('_').next()?;
    Uuid::parse_str(uuid_part).ok()?;
    Some(uuid_part.to_string())
}

pub(crate) fn read_pi_session_observation(
    instance_id: &str,
    source: &crate::session::instance::SessionSidecarSource,
    active: Option<&crate::session::instance::ActiveExecution>,
    any_age: bool,
) -> Option<crate::session::poller::SessionIdObservation> {
    use crate::session::instance::{CaptureContext, SessionSidecarSource};
    crate::session::validate_instance_id(instance_id).ok()?;
    let (sid_leaf, path_leaf) = if let Some(active) = active {
        let Some(CaptureContext::Pi {
            source: expected, ..
        }) = &active.capture
        else {
            return None;
        };
        if expected != source || active.binding.agent != "pi" {
            return None;
        }
        (
            crate::hooks::session_id_leaf(Some(&active.launch_id)).ok()?,
            std::borrow::Cow::Owned(format!("session_path.{}", active.launch_id)),
        )
    } else {
        (
            std::borrow::Cow::Borrowed("session_id"),
            std::borrow::Cow::Borrowed("session_path"),
        )
    };
    let read = |leaf: &str, fresh: bool| {
        source.read_file(
            instance_id,
            leaf,
            4096,
            fresh.then_some(crate::hooks::SESSION_ID_SIDECAR_MAX_AGE),
        )
    };
    let id_bytes = read(&sid_leaf, !any_age)?;
    let sid = std::str::from_utf8(&id_bytes).ok()?.trim();
    Uuid::parse_str(sid).ok()?;
    let path_bytes = read(&path_leaf, false);
    let path = match path_bytes.as_deref() {
        Some(bytes) => Some(Path::new(std::str::from_utf8(bytes).ok()?.trim())),
        None => None,
    };
    let transcript = if let Some(path) = path {
        if !path.is_absolute() || crate::git::template::lexical_normalize(path) != path {
            return None;
        }
        // Pi writes the ID first and leaves the previous path until the new one exists.
        if extract_pi_uuid_from_filename(path).is_some_and(|path_id| path_id != sid) {
            None
        } else {
            let native = match active.and_then(|active| active.container.as_ref()) {
                Some(container) => container.runtime.canonical_path(&container.id, path).ok()?,
                None if matches!(source, SessionSidecarSource::HostHooks(_)) => {
                    super::canonicalize_or_raw(path.to_str()?)
                }
                None => path.to_path_buf(),
            };
            let physical = if let Some(active) = active {
                let Some(CaptureContext::Pi { root, .. }) = &active.capture else {
                    return None;
                };
                if !native.starts_with(root) || native == *root {
                    return None;
                }
                match &active.container {
                    Some(container) => container.host_path(&native, true)?,
                    None => native.clone(),
                }
            } else {
                match source {
                    SessionSidecarSource::HostHooks(_) => native.clone(),
                    SessionSidecarSource::SandboxDir(directory) => directory
                        .parent()?
                        .parent()?
                        .join(native.strip_prefix("/root/.pi").ok()?),
                }
            };
            let parent = physical.parent()?;
            let root = crate::session::AnchoredDir::open(parent).ok()?;
            let leaf = Path::new(physical.file_name()?);
            match root.regular_lookup(leaf).ok()? {
                Some(true) => {
                    if extract_pi_header_fields(&physical)?.0.as_deref() != Some(sid) {
                        return None;
                    }
                }
                None => {
                    if leaf.to_str()?.rsplit_once('_')?.1.strip_suffix(".jsonl")? != sid {
                        return None;
                    }
                }
                Some(false) => return None,
            }
            Some((native, physical))
        }
    } else {
        None
    };
    if read(&sid_leaf, !any_age)? != id_bytes {
        return None;
    }
    let published_path = match transcript.as_ref() {
        Some((native, _)) => Some(native.to_str()?.to_owned()),
        None => None,
    };
    let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
        sid.to_owned(),
        published_path.clone(),
    );
    observation.pi_session_path = published_path;
    if let Some(active) = active {
        let mut binding = active.binding.clone();
        if let Some((_, physical)) = transcript {
            binding.stores = vec![physical.parent()?.to_path_buf()];
            observation.transcript_path = Some(physical);
        }
        observation.execution = Some(active.clone());
        observation.source = Some(binding);
    }
    Some(observation)
}

/// Polls the Pi extension's pane-scoped sidecar for its conversation and transcript.
/// The caller supplies where the pane publishes; a wrong source silently never observes.
pub(crate) fn pi_sidecar_poll_fn(
    instance_id: String,
    source: crate::session::instance::SessionSidecarSource,
    active: Option<crate::session::instance::ActiveExecution>,
) -> impl Fn() -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move || read_pi_session_observation(&instance_id, &source, active.as_ref(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn header_fields_and_filename_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"model_change\"}\n{\"type\":\"session\",\"id\":\"aaa\",\"cwd\":\"/home/user/project\"}",
        )
        .unwrap();
        assert_eq!(
            extract_pi_header_fields(&path),
            Some((Some("aaa".into()), Some("/home/user/project".into())))
        );
        assert_eq!(
            extract_pi_uuid_from_filename(Path::new(
                "2024-12-03T14-00-00-000Z_019342ab-1234-7def-8901-abcdef012345.jsonl"
            ))
            .as_deref(),
            Some("019342ab-1234-7def-8901-abcdef012345")
        );
    }

    #[test]
    #[serial_test::serial]
    fn pi_sidecar_captures_id_before_its_transcript_is_published() {
        let (_hooks, _base, _hooks_tmp) = crate::hooks::test_support::BaseGuard::ready();
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let inst = crate::session::Instance::new("pi-path-later", tmp.path().to_str().unwrap());
        let source = crate::session::instance::SessionSidecarSource::host_hooks(&inst.id);
        let poll = pi_sidecar_poll_fn(inst.id.clone(), source, None);
        let old = "11111111-2222-4333-8444-555555555555";
        let current = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        crate::hooks::write_session_id_via_guard(&inst.id, current, None).unwrap();

        let observation = poll().expect("the ID precedes Pi getSessionFile()");
        assert_eq!(observation.sid, current);
        assert!(observation.pi_session_path.is_none());
        assert_eq!(
            observation.guard,
            crate::session::poller::SessionIdGuard::InstanceSidecar { transcript: None }
        );

        let sidecar = crate::hooks::ensure_instance_dir_path(&inst.id).unwrap();
        let old_file = tmp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{old}.jsonl"));
        std::fs::write(
            &old_file,
            format!("{}\n", serde_json::json!({"type": "session", "id": old})),
        )
        .unwrap();
        std::fs::write(sidecar.join("session_path"), old_file.to_str().unwrap()).unwrap();
        let observation = poll().expect("an old transcript cannot hide a newer ID");
        assert_eq!(observation.sid, current);
        assert!(observation.pi_session_path.is_none());

        let current_file = tmp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{current}.jsonl"));
        std::fs::write(
            &current_file,
            format!(
                "{}\n",
                serde_json::json!({"type": "session", "id": current})
            ),
        )
        .unwrap();
        std::fs::write(sidecar.join("session_path"), current_file.to_str().unwrap()).unwrap();
        let canonical_root = tmp.path().canonicalize().unwrap();
        let canonical_file = current_file.canonicalize().unwrap();
        let observation = poll().expect("the matching transcript becomes available later");
        assert_eq!(observation.sid, current);
        assert_eq!(
            observation.pi_session_path.as_deref(),
            canonical_file.to_str()
        );

        let launch = "22222222-3333-4333-8444-555555555555";
        let source = crate::session::instance::SessionSidecarSource::host_hooks(&inst.id);
        let binding = crate::session::ExecutionBinding {
            agent: "pi".into(),
            stores: vec![canonical_root.join("store")],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: canonical_root.clone(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        let active = crate::session::instance::ActiveExecution {
            launch_id: launch.into(),
            binding: binding.clone(),
            capture: Some(crate::session::instance::CaptureContext::Pi {
                source: source.clone(),
                root: canonical_root.clone(),
            }),
            container: None,
        };
        crate::hooks::write_session_id_via_guard(&inst.id, current, Some(launch)).unwrap();
        let scoped = pi_sidecar_poll_fn(inst.id.clone(), source, Some(active.clone()));
        let observation = scoped().expect("a launch-scoped Pi ID is attributable without a path");
        assert_eq!(observation.source.as_ref(), Some(&binding));
        assert_eq!(observation.execution.as_ref(), Some(&active));
        assert!(observation.transcript_path.is_none());
        assert!(observation.pi_session_path.is_none());

        let path_leaf = format!("session_path.{launch}");
        std::fs::write(sidecar.join(&path_leaf), current_file.to_str().unwrap()).unwrap();
        let observation = scoped().expect("the active launch publishes its path later");
        assert_eq!(
            observation.transcript_path.as_deref(),
            Some(canonical_file.as_path())
        );
        assert_eq!(observation.source.unwrap().stores, vec![canonical_root]);

        let outside = tempfile::tempdir().unwrap();
        let foreign = outside
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{current}.jsonl"));
        std::fs::write(
            &foreign,
            format!(
                "{}\n",
                serde_json::json!({"type": "session", "id": current})
            ),
        )
        .unwrap();
        std::fs::write(sidecar.join(path_leaf), foreign.to_str().unwrap()).unwrap();
        assert!(
            scoped().is_none(),
            "a path outside the launch root must fail closed"
        );
    }
}
