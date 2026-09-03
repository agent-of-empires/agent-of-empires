//! Serving files from a session artifact directory.

use super::*;

/// Largest artifact the dashboard will serve inline. Generated screenshots
/// and status pages are small; the cap just bounds a pathological read.
const MAX_ARTIFACT_BYTES: u64 = 50 * 1024 * 1024;

/// Serve a file from a session's managed artifact directory
/// (`GET /api/sessions/{id}/artifacts/{*path}`). Auth is enforced by the
/// global middleware; `resolve_artifact_path` canonicalizes and confines the
/// request to the session's artifact root, so neither `..` nor a symlink can
/// escape it and arbitrary host paths are never served. HTML is sent as an
/// attachment (never inline) so a generated page cannot execute script in the
/// dashboard's authenticated origin. See #2587.
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
        Ok(m) if m.len() > MAX_ARTIFACT_BYTES => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        Ok(_) => {}
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    let bytes = match tokio::fs::read(&file_path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    use axum::http::{header, HeaderMap, HeaderValue};
    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    let essence = mime.essence_str();
    // Any type that can execute script when opened as a top-level document is
    // served as a download, never inline. The frontend opens artifacts via
    // `window.open(blob:)`, and a blob URL inherits the dashboard's origin, so
    // an HTML/XHTML/SVG/XML artifact would otherwise run script in the
    // authenticated origin. Images and other passive types stay inline. See #2587.
    let force_download = matches!(
        essence,
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "application/xml" | "text/xml"
    );
    let content_type = if force_download {
        "application/octet-stream"
    } else {
        essence
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
        HeaderValue::from_static("private, max-age=60"),
    );
    if force_download {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }

    (StatusCode::OK, headers, bytes).into_response()
}
