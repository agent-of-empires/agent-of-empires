//! Prompt attachment validation.

use axum::http::StatusCode;

use crate::acp::event_store::AttachmentBlob;
use crate::acp::protocol::PromptAttachmentUpload;
use crate::daemon::PromptAttachmentKind;
use crate::server::AppState;

const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Accepted MIME types per kind. SVG is excluded (scriptable XML).
fn mime_allowed(kind: PromptAttachmentKind, mime: &str) -> bool {
    match kind {
        PromptAttachmentKind::Image => {
            matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        }
        PromptAttachmentKind::Audio => matches!(
            mime,
            "audio/mpeg" | "audio/wav" | "audio/x-wav" | "audio/webm" | "audio/ogg" | "audio/mp4"
        ),
        PromptAttachmentKind::Resource => matches!(
            mime,
            "text/plain" | "text/markdown" | "application/json" | "application/pdf"
        ),
    }
}

/// The raster image MIME type the magic bytes identify.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn attachment_too_large() -> (StatusCode, String) {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        format!(
            "attachment exceeds {} MiB limit",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ),
    )
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

/// Decode and validate uploads before the prompt is published, so a rejected
/// prompt never leaves a half-rendered attachment in the transcript.
pub(crate) fn validate_attachments(
    state: &AppState,
    session_id: &str,
    uploads: &[PromptAttachmentUpload],
) -> Result<Vec<AttachmentBlob>, (StatusCode, String)> {
    use base64::Engine as _;
    if uploads.is_empty() {
        return Ok(Vec::new());
    }
    if uploads.len() > MAX_ATTACHMENTS {
        return Err(bad_request(format!(
            "too many attachments (max {MAX_ATTACHMENTS})"
        )));
    }
    // `None` means the handshake has not reported capabilities yet.
    let Some((image_ok, audio_ok, embedded_ok)) =
        state.acp_event_store.latest_prompt_capabilities(session_id)
    else {
        return Err((
            StatusCode::CONFLICT,
            "agent capabilities not known yet; cannot accept attachments".to_string(),
        ));
    };

    let mut blobs = Vec::with_capacity(uploads.len());
    let mut total = 0usize;
    for up in uploads {
        let kind_ok = match up.kind {
            PromptAttachmentKind::Image => image_ok,
            PromptAttachmentKind::Audio => audio_ok,
            PromptAttachmentKind::Resource => embedded_ok,
        };
        if !kind_ok {
            return Err(bad_request(format!(
                "the current agent does not accept {} attachments",
                up.kind.as_str()
            )));
        }
        if !mime_allowed(up.kind, &up.mime_type) {
            return Err(bad_request(format!(
                "unsupported attachment type: {}",
                up.mime_type
            )));
        }
        // Bail before decoding so a huge payload cannot force a large
        // allocation; +4 covers base64 padding.
        if up.data.len() > MAX_ATTACHMENT_BYTES / 3 * 4 + 4 {
            return Err(attachment_too_large());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(up.data.as_bytes())
            .map_err(|_| bad_request("attachment is not valid base64"))?;
        if bytes.is_empty() {
            return Err(bad_request("empty attachment"));
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(attachment_too_large());
        }
        if up.kind == PromptAttachmentKind::Image {
            let Some(sniffed_mime) = sniff_image_mime(&bytes) else {
                return Err(bad_request("attachment bytes are not a supported image"));
            };
            if sniffed_mime != up.mime_type {
                return Err(bad_request(
                    "attachment MIME does not match declared content-type",
                ));
            }
        }
        total += bytes.len();
        if total > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachments exceed {} MiB total limit",
                    MAX_TOTAL_ATTACHMENT_BYTES / (1024 * 1024)
                ),
            ));
        }
        blobs.push(AttachmentBlob {
            id: uuid::Uuid::new_v4().to_string(),
            kind: up.kind,
            mime_type: up.mime_type.clone(),
            name: up.name.clone(),
            data: bytes,
        });
    }
    Ok(blobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_allowlist_gates_by_kind() {
        assert!(mime_allowed(PromptAttachmentKind::Image, "image/png"));
        assert!(mime_allowed(PromptAttachmentKind::Image, "image/webp"));
        assert!(!mime_allowed(PromptAttachmentKind::Image, "image/svg+xml"));
        assert!(!mime_allowed(PromptAttachmentKind::Image, "audio/mpeg"));
        assert!(mime_allowed(PromptAttachmentKind::Audio, "audio/mpeg"));
        assert!(mime_allowed(
            PromptAttachmentKind::Resource,
            "application/pdf"
        ));
        assert!(!mime_allowed(PromptAttachmentKind::Resource, "text/html"));
    }

    #[test]
    fn image_magic_bytes_sniff() {
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        let cases: [(&[u8], Option<&str>); 6] = [
            (
                &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
                Some("image/png"),
            ),
            (&[0xFF, 0xD8, 0xFF, 0xE0], Some("image/jpeg")),
            (b"GIF89a.....", Some("image/gif")),
            (&webp, Some("image/webp")),
            (b"<svg>not an image</svg>", None),
            (b"", None),
        ];
        for (bytes, expected) in cases {
            assert_eq!(sniff_image_mime(bytes), expected, "{bytes:?}");
        }
    }
}
