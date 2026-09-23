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
    let path_bytes = read(&path_leaf, false)?;
    let path = Path::new(std::str::from_utf8(&path_bytes).ok()?.trim());
    if !path.is_absolute() || crate::git::template::lexical_normalize(path) != path {
        return None;
    }
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
    if read(&sid_leaf, !any_age)? != id_bytes {
        return None;
    }
    let published_path = native.to_str()?.to_owned();
    let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
        sid.to_owned(),
        Some(published_path.clone()),
    );
    observation.pi_session_path = Some(published_path);
    if let Some(active) = active {
        let mut binding = active.binding.clone();
        binding.stores = vec![parent.to_path_buf()];
        observation.execution = Some(active.clone());
        observation.source = Some(binding);
        observation.transcript_path = Some(physical);
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
}
