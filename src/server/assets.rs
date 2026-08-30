//! Serving the embedded web bundle and the files under the public dir.

use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
pub(super) struct StaticAssets;

pub(super) async fn serve_index(
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && path != "index.html"
        && path.contains('.')
        && StaticAssets::get(path).is_some()
    {
        return serve_embedded_file(path, &headers);
    }
    serve_embedded_file("index.html", &headers)
}

pub(super) async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    serve_embedded_file(&format!("assets/{}", path), &headers)
}

pub(super) async fn serve_public_file(
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    // Strip leading slash to match rust-embed paths
    let path = uri.path().trim_start_matches('/');
    serve_embedded_file(path, &headers)
}

/// The content-hashed entry bundle filename (`index-<hash>.js`) baked
/// into the embedded `index.html`. This is the dashboard's build
/// identity: the client compares its own entry script's filename
/// against this value (via `GET /api/about`) and offers a reload when
/// they differ. Installed PWAs (especially iOS) resume a long-lived
/// page with no refresh affordance, so without this prompt a phone can
/// keep running a stale dashboard for weeks after the binary updates.
pub fn web_build_id() -> Option<&'static str> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let index = StaticAssets::get("index.html")?;
        let html = std::str::from_utf8(index.data.as_ref()).ok()?;
        extract_web_build_id(html)
    })
    .as_deref()
}

/// Pull `index-<hash>.js` out of the built index.html. Vite names the
/// entry chunk `assets/index-<hash>.js`; lazy chunks get their own
/// names, so the first match is the entry.
pub(super) fn extract_web_build_id(html: &str) -> Option<String> {
    let start = html.find("assets/index-")?;
    let name = &html[start + "assets/".len()..];
    let end = name.find(".js")?;
    Some(format!("{}.js", &name[..end]))
}

pub(super) fn serve_embedded_file(
    path: &str,
    request_headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    match StaticAssets::get(path) {
        Some(file) => {
            // Strong ETag from rust-embed's content hash, so `no-cache`
            // revalidation costs a 304 instead of a re-download.
            let etag = {
                let hash = file.metadata.sha256_hash();
                let mut s = String::with_capacity(hash.len() * 2 + 2);
                s.push('"');
                for b in hash {
                    use std::fmt::Write;
                    let _ = write!(s, "{:02x}", b);
                }
                s.push('"');
                s
            };
            if request_headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|inm| {
                    inm.split(',')
                        .any(|t| t.trim().trim_start_matches("W/") == etag)
                })
            {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, etag),
                        (header::CACHE_CONTROL, cache_control_for(path).to_string()),
                    ],
                )
                    .into_response();
            }
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, cache_control_for(path).to_string()),
                ],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Cache policy for embedded dashboard files. Vite content-hashes
/// everything under `assets/`, so those are immutable; everything else
/// (index.html, sw.js, manifest, icons, fonts) must revalidate on every
/// load or an installed PWA keeps booting a stale shell long after the
/// binary shipped new assets. Revalidation is cheap: the ETag above
/// turns it into a 304.
pub(super) fn cache_control_for(path: &str) -> &'static str {
    if is_content_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// True for `assets/<name>-<hash>.<ext>` where `<hash>` is a Rollup
/// content hash: 8 chars (the default length) of the base64url
/// alphabet, immediately preceded by `-`. The `assets/` prefix alone is
/// not enough: should a non-hashed file ever land there through a Vite
/// config change, a year of `immutable` would pin clients to it.
/// Misclassifying a hashed file the other way is harmless; it just
/// revalidates via ETag like everything else.
pub(super) fn is_content_hashed_asset(path: &str) -> bool {
    let Some(name) = path.strip_prefix("assets/") else {
        return false;
    };
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() < 9 || bytes[bytes.len() - 9] != b'-' {
        return false;
    }
    bytes[bytes.len() - 8..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_web_build_id_finds_entry_bundle() {
        let html = r#"<head><script type="module" crossorigin src="/assets/index-DKenwdW0.js"></script>
<link rel="modulepreload" crossorigin href="/assets/vendor-Bx91yz.js"></head>"#;
        assert_eq!(
            extract_web_build_id(html).as_deref(),
            Some("index-DKenwdW0.js")
        );
    }

    #[test]
    fn extract_web_build_id_none_without_entry() {
        assert_eq!(extract_web_build_id("<html><body>hi</body></html>"), None);
    }

    #[test]
    fn cache_control_immutable_only_for_hashed_assets() {
        assert_eq!(
            cache_control_for("assets/index-DKenwdW0.js"),
            "public, max-age=31536000, immutable"
        );
        // Rollup hashes draw from the base64url alphabet (`_` and `-`
        // included), and chunk base names can themselves contain `-`.
        assert_eq!(
            cache_control_for("assets/StructuredView-DM_xphSL.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control_for("assets/theme-bootstrap-Ab12Cd34.css"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control_for("index.html"), "no-cache");
        assert_eq!(cache_control_for("sw.js"), "no-cache");
        assert_eq!(
            cache_control_for("fonts/GeistMono-Regular.woff2"),
            "no-cache"
        );
        // Un-hashed files under assets/ must NOT be pinned for a year.
        assert_eq!(cache_control_for("assets/logo.svg"), "no-cache");
        assert_eq!(cache_control_for("assets/readme"), "no-cache");
        assert_eq!(cache_control_for("assets/short-a1.js"), "no-cache");
    }
}
