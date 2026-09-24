//! Pi-family session headers and the Pi sidecar poller.

use std::io::Read;
use std::path::Path;

use uuid::Uuid;

/// Leading lines and bytes scanned for a session header; the byte cap bounds
/// allocation on one hostile line.
pub(super) const PI_HEADER_SCAN_LINES: usize = 8;
pub(super) const PI_HEADER_SCAN_BYTES: usize = 64 * 1024;

/// `(id, cwd)` from the first session header line, opened without following symlinks.
pub(super) fn extract_pi_header_fields(path: &Path) -> Option<(Option<String>, Option<String>)> {
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

/// Polls the sidecars Pi's AoE extension writes for the pane's own conversation and transcript.
/// The caller supplies where the pane publishes; a wrong source silently never observes.
pub(crate) fn pi_sidecar_poll_fn(
    instance_id: String,
    source: crate::session::instance::SessionSidecarSource,
) -> impl Fn() -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move || {
        use crate::session::instance::SessionSidecarSource;
        let (id, transcript) = match source {
            SessionSidecarSource::SandboxDir(ref dir) => {
                let root = dir
                    .parent()
                    .and_then(Path::parent)
                    .filter(|root| root.join("aoe-session").join(&instance_id) == *dir)
                    .and_then(|root| crate::session::AnchoredDir::open(root).ok());
                let read = |leaf: &str| {
                    let raw = root
                        .as_ref()?
                        .read_regular(
                            &Path::new("aoe-session").join(&instance_id).join(leaf),
                            4096,
                        )
                        .ok()??;
                    Some(String::from_utf8(raw).ok()?.trim().to_string())
                };
                (
                    read("session_id").filter(|id| Uuid::parse_str(id).is_ok()),
                    read("session_path").filter(|path| path.starts_with('/')),
                )
            }
            SessionSidecarSource::HostHooks => (
                crate::hooks::read_hook_session_id(&instance_id),
                crate::hooks::read_hook_session_path(&instance_id),
            ),
        };
        id.and_then(super::validated_session_id).map(|sid| {
            crate::session::poller::SessionIdObservation::instance_sidecar(sid, transcript)
        })
    }
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
