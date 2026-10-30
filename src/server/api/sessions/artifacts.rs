//! Serving files from a session artifact directory.

use super::*;

/// Largest raw file the dashboard serves; the cap just bounds a pathological
/// read.
pub(super) const MAX_RAW_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Serve a file from a session's managed artifact directory.
///
/// `resolve_artifact_confined` canonicalizes and confines the request to the
/// session's artifact root, so neither `..` nor a symlink can escape it; the
/// bytes are then read through the shared bounded, race-safe confined reader
/// (`read_confined_bytes`), so the cap holds on a file that grows after the
/// stat and an endless special file cannot stall the read. Scriptable types
/// (HTML, SVG, XML, JavaScript) are always downloaded, never rendered, by
/// `raw_file_response` (#2587), and any other type the browser would not render
/// inline is sent as an attachment, exactly as the diff route's "Open file"
/// does, so a blob URL can never become a scriptable same-origin document.
pub async fn serve_session_artifact(
    State(state): State<Arc<AppState>>,
    Path((id, path)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }

    let result = tokio::task::spawn_blocking(move || {
        let (root, file) = crate::session::artifacts::resolve_artifact_confined(&id, &path)
            .ok_or((StatusCode::NOT_FOUND, "artifact not found"))?;
        let confined = crate::server::api::file_provenance::Confined {
            canonical: file.clone(),
            root,
        };
        let bytes = crate::server::api::file_provenance::read_confined_bytes(
            &confined,
            MAX_RAW_FILE_BYTES,
        )?;
        Ok::<_, (StatusCode, &'static str)>((file, bytes))
    })
    .await;

    let (file, bytes) = match result {
        Ok(Ok(v)) => v,
        Ok(Err((StatusCode::NOT_FOUND, _))) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err((status, _))) => return status.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    raw_file_response(&mime, !renders_inline(&mime), "private, max-age=60", bytes)
}

/// True for a type a browser renders inline in a tab, so it needs no forced
/// download. Everything else (notably every scriptable type) is an attachment.
pub(super) fn renders_inline(ty: &mime_guess::Mime) -> bool {
    use mime_guess::mime;
    let top = ty.type_();
    top == mime::IMAGE
        || top == mime::AUDIO
        || top == mime::VIDEO
        || ty.essence_str() == "text/plain"
        || *ty == mime::APPLICATION_PDF
}

/// True for a type that can execute script when opened as a top-level
/// document, which includes every XML type and the whole JavaScript /
/// ECMAScript family (`text/javascript`, `application/javascript`, the legacy
/// `x-` spellings, and their ECMAScript counterparts).
pub(super) fn is_scriptable(essence: &str) -> bool {
    matches!(
        essence,
        "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "application/xml"
            | "text/xml"
            | "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "application/x-ecmascript"
            | "module"
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
