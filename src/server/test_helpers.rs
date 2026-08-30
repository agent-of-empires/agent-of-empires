//! Test fixtures shared by more than one submodule's tests.

pub(super) fn vecs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Extract every mutating `(METHOD, path-template)` pair registered in
/// `build_router` by scanning `.route("<path>", <handlers>)` and reading the
/// method combinators inside each handler expression (balanced parens so a
/// nested `get(...).post(...)` doesn't bleed into the next route). Shared by
/// the CityHall table-exhaustiveness audit below.
pub(super) fn router_mutating_routes() -> std::collections::BTreeSet<(String, String)> {
    let src = include_str!("router.rs");
    let start = src.find("fn build_router").expect("build_router present");
    let end = src[start..]
        .find(".layer(axum::middleware::from_fn_with_state")
        .map(|o| start + o)
        .unwrap_or(src.len());
    let body = &src[start..end];
    let mut out = std::collections::BTreeSet::new();
    let bytes = body.as_bytes();
    let marker = ".route(";
    let mut i = 0;
    while let Some(rel) = body[i..].find(marker) {
        let mut j = i + rel + marker.len();
        // Skip to the opening quote of the path literal.
        while j < body.len() && bytes[j] != b'"' {
            j += 1;
        }
        j += 1;
        let path_start = j;
        while j < body.len() && bytes[j] != b'"' {
            j += 1;
        }
        let path = &body[path_start..j];
        // Handler expression: from here to the matching close paren of
        // `.route(` at depth 0.
        let mut depth = 1i32;
        let mut k = j;
        while k < body.len() && depth > 0 {
            match bytes[k] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            k += 1;
        }
        let expr = &body[j..k];
        for method in ["post", "patch", "put", "delete"] {
            if expr.contains(&format!("{method}(")) {
                out.insert((method.to_uppercase(), path.to_string()));
            }
        }
        i = k;
    }
    out
}
