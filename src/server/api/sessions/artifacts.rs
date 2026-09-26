//! Serving files from a session artifact directory.

use super::*;

/// Largest raw file the dashboard serves; the cap just bounds a pathological
/// read.
pub(super) const MAX_RAW_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Serve a file from a session's managed artifact directory.
/// `resolve_artifact_path` canonicalizes and confines the request to the
/// session's artifact root, so neither `..` nor a symlink can escape it.
/// Scriptable types (HTML, SVG, XML) are always downloaded, never rendered,
/// by `raw_file_response` (#2587).
pub async fn serve_session_artifact(Path((id, path)): Path<(String, String)>) -> impl IntoResponse {
    let resolved = tokio::task::spawn_blocking(move || {
        crate::session::artifacts::resolve_artifact_path(&id, &path)
    })
    .await;

    let file_path = match resolved {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    match tokio::fs::metadata(&file_path).await {
        Ok(m) if m.len() > MAX_RAW_FILE_BYTES => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        Ok(_) => {}
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    let bytes = match tokio::fs::read(&file_path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    raw_file_response(&mime, false, "private, max-age=60", bytes)
}

/// True for a type that can execute script when opened as a top-level
/// document, which includes every XML type.
pub(super) fn is_scriptable(essence: &str) -> bool {
    matches!(
        essence,
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "application/xml" | "text/xml"
    ) || essence.ends_with("+xml")
}

/// Raw file bytes served as `mime` with `nosniff`. The frontend opens these
/// through a blob URL, which inherits the dashboard's authenticated origin, so
/// a scriptable type is sent as an opaque attachment and never renders there
/// (#2587).
pub(super) fn raw_file_response(
    mime: &mime_guess::Mime,
    attachment: bool,
    cache_control: &'static str,
    bytes: Vec<u8>,
) -> axum::response::Response {
    use axum::http::{header, HeaderMap, HeaderValue};

    let force_download = is_scriptable(mime.essence_str());
    let content_type = if force_download {
        "application/octet-stream"
    } else {
        mime.as_ref()
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if attachment || force_download {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }

    (StatusCode::OK, headers, bytes).into_response()
}
