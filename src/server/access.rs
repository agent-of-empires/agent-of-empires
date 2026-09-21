//! Who may reach the dashboard.

use std::sync::Arc;

use super::state::AppState;
use crate::server::api;

/// True when `host` is a wildcard bind ("all interfaces") rather than a
/// concrete, routable name a browser would send back as `Host`.
pub(crate) fn is_wildcard_bind(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

/// Strip an optional `:port` and IPv6 brackets from a `Host`/authority value, yielding the
/// canonical bare host.
pub(super) fn strip_host_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rfind(':') {
        Some(idx) if !host[..idx].contains(':') => &host[..idx],
        _ => host,
    }
}

/// Canonical host key for the allowlist and for `Host` comparison.
pub(crate) fn norm_host(host: &str) -> String {
    let bare = strip_host_port(host);
    bare.strip_suffix('.').unwrap_or(bare).to_ascii_lowercase()
}

/// True when a `norm_host`'d value is a routable IP literal we trust unconditionally.
pub(super) fn is_trusted_ip_literal(host: &str) -> bool {
    use std::net::IpAddr;
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    let ip = ip.to_canonical();
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local(),
        // `Ipv6Addr::is_unicast_link_local` is unstable; match `fe80::/10` by
        // hand (top 10 bits `1111111010`).
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// True when a `norm_host`'d value parses as an IP literal the gate refuses to trust.
pub(crate) fn is_untrusted_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok() && !is_trusted_ip_literal(host)
}

/// Wrap an IPv6 literal in brackets for use inside an origin authority;
/// hostnames and IPv4 literals pass through unchanged.
pub(super) fn bracket_if_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

pub(super) fn push_unique(list: &mut Vec<String>, item: String) {
    if !item.is_empty() && !list.contains(&item) {
        list.push(item);
    }
}

/// Canonicalize an `Origin` to the exact form a browser serializes.
pub(super) fn norm_origin(origin: &str) -> String {
    let o = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    // Strip a single trailing FQDN root dot from the host so `https://example.com.` ==
    // `https://example.com`, mirroring `norm_host`.
    let o = match o.strip_suffix('.') {
        Some(rest) => rest.to_string(),
        None => o.replacen(".:", ":", 1),
    };
    for (scheme, default_port) in [("http://", ":80"), ("https://", ":443")] {
        if let Some(host) = o
            .strip_prefix(scheme)
            .and_then(|r| r.strip_suffix(default_port))
        {
            return format!("{scheme}{host}");
        }
    }
    o
}

pub(super) fn push_origin(list: &mut Vec<String>, raw: String) {
    push_unique(list, norm_origin(&raw));
}

/// Extract the bare host from a tunnel URL like `https://x.trycloudflare.com`.
pub(super) fn host_from_url(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = norm_host(authority);
    (!host.is_empty()).then_some(host)
}

/// Resolve the `(allowed_hosts, allowed_origins)` pair the DNS-rebinding gate enforces.
pub(super) fn resolve_access_policy(
    host: &str,
    port: u16,
    extra_hosts: &[String],
    extra_origins: &[String],
    tunnel_host: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut hosts: Vec<String> = Vec::new();
    let mut origins: Vec<String> = Vec::new();

    let mut local: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if !is_wildcard_bind(host) {
        push_unique(&mut local, norm_host(host));
    }
    for h in &local {
        push_unique(&mut hosts, h.clone());
        let hb = bracket_if_ipv6(h);
        push_origin(&mut origins, format!("http://{hb}:{port}"));
        push_origin(&mut origins, format!("https://{hb}:{port}"));
    }

    for h in extra_hosts {
        let nh = norm_host(h);
        if nh.is_empty() {
            continue;
        }
        push_unique(&mut hosts, nh.clone());
        let hb = bracket_if_ipv6(&nh);
        push_origin(&mut origins, format!("http://{hb}:{port}"));
        push_origin(&mut origins, format!("https://{hb}:{port}"));
        push_origin(&mut origins, format!("http://{hb}"));
        push_origin(&mut origins, format!("https://{hb}"));
    }

    if let Some(th) = tunnel_host {
        let nh = norm_host(th);
        if !nh.is_empty() {
            push_unique(&mut hosts, nh.clone());
            push_origin(&mut origins, format!("https://{}", bracket_if_ipv6(&nh)));
        }
    }

    for o in extra_origins {
        push_origin(&mut origins, o.clone());
    }

    (hosts, origins)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AccessDecision {
    Allow,
    DenyMissingHost,
    DenyHost,
    DenyOrigin,
}

/// Pure DNS-rebinding decision.
pub(super) fn evaluate_access(
    host_header: Option<&str>,
    origin_header: Option<&str>,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> AccessDecision {
    let Some(raw_host) = host_header else {
        return AccessDecision::DenyMissingHost;
    };
    let host = norm_host(raw_host);
    if !allowed_hosts.contains(&host) && !is_trusted_ip_literal(&host) {
        return AccessDecision::DenyHost;
    }
    if let Some(origin) = origin_header {
        let origin = norm_origin(origin);
        // A by-IP dashboard sends an IP-literal Origin; trust it like the Host (auth is the backstop).
        let origin_is_trusted_ip =
            host_from_url(&origin).is_some_and(|h| is_trusted_ip_literal(&h));
        if !allowed_origins.contains(&origin) && !origin_is_trusted_ip {
            return AccessDecision::DenyOrigin;
        }
    }
    AccessDecision::Allow
}

/// Uniform 403 for every DNS-rebinding rejection.
pub(super) fn access_denied() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::FORBIDDEN,
        "forbidden: host or origin not allowed",
    )
        .into_response()
}

/// DNS-rebinding gate.
pub(super) async fn access_policy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host_header = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_string()));
    let origin_header = request
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    match evaluate_access(
        host_header.as_deref(),
        origin_header.as_deref(),
        &state.allowed_hosts,
        &state.allowed_origins,
    ) {
        AccessDecision::Allow => next.run(request).await,
        AccessDecision::DenyMissingHost => {
            tracing::debug!(target: "http.access", "rejected: missing Host header");
            access_denied()
        }
        AccessDecision::DenyHost => {
            tracing::debug!(target: "http.access", host = ?host_header, "rejected: host not in allowlist");
            access_denied()
        }
        AccessDecision::DenyOrigin => {
            tracing::debug!(target: "http.access", origin = ?origin_header, "rejected: origin not in allowlist");
            access_denied()
        }
    }
}

/// Mutating routes (POST/PUT/PATCH/DELETE) reachable in CityHall client mode.
pub(super) const CITYHALL_MUTATION_ALLOW: &[(&str, &str)] = &[
    // Session creation (server-derived) + lifecycle / metadata on the structured
    // sessions this mode owns; each handler re-checks the target is structured.
    ("POST", "/api/sessions"),
    ("DELETE", "/api/sessions/{id}"),
    ("DELETE", "/api/workspaces"),
    ("PATCH", "/api/sessions/{id}"),
    ("PATCH", "/api/sessions/{id}/archive"),
    ("PATCH", "/api/sessions/{id}/color"),
    ("PATCH", "/api/sessions/{id}/diff-base"),
    ("PATCH", "/api/sessions/{id}/group"),
    ("PATCH", "/api/sessions/{id}/notifications"),
    ("PATCH", "/api/sessions/{id}/pin"),
    ("PATCH", "/api/sessions/{id}/snooze"),
    ("PATCH", "/api/sessions/{id}/unread"),
    ("PATCH", "/api/sessions/{id}/worktree-name"),
    ("POST", "/api/sessions/{id}/restore"),
    ("POST", "/api/sessions/{id}/start"),
    ("POST", "/api/sessions/{id}/stop"),
    ("POST", "/api/sessions/{id}/summarize"),
    ("POST", "/api/sessions/{id}/smart-rename"),
    ("POST", "/api/sessions/{id}/trash"),
    // Composer.
    ("POST", "/api/sessions/{id}/paste-image"),
    ("POST", "/api/sessions/{id}/acp/prompt"),
    ("POST", "/api/sessions/{id}/acp/prompt/diff-comments"),
    ("POST", "/api/sessions/{id}/acp/cancel"),
    ("POST", "/api/sessions/{id}/acp/force_end_turn"),
    ("POST", "/api/sessions/{id}/acp/approvals/{nonce}"),
    ("POST", "/api/sessions/{id}/acp/elicitations/{nonce}"),
    // Server-owned prompt queue.
    ("POST", "/api/sessions/{id}/queue"),
    ("DELETE", "/api/sessions/{id}/queue"),
    ("PATCH", "/api/sessions/{id}/queue/{promptId}"),
    ("DELETE", "/api/sessions/{id}/queue/{promptId}"),
    // Curated settings surfaces (the handlers field-filter / strip color-mode).
    ("PATCH", "/api/profiles/{name}/settings"),
    ("PATCH", "/api/theme"),
    ("POST", "/api/plugins/{id}/settings/options/resolve"),
    // Telemetry consent (its own surface, still prompted).
    ("POST", "/api/telemetry/consent"),
    ("POST", "/api/telemetry/seen"),
    ("POST", "/api/telemetry/structured-interaction"),
    // Ephemeral foreground-presence heartbeat. Does not mutate user data.
    ("POST", "/api/presence"),
    // Per-device UI preferences / client log.
    ("PATCH", "/api/app-state/web-ui-state"),
    ("POST", "/api/app-state/dismiss-update"),
    ("POST", "/api/app-state/tip-seen"),
    ("POST", "/api/app-state/volume-ignores-globs-acknowledged"),
    ("POST", "/api/app-state/web-tour-seen"),
    ("POST", "/api/tips/show"),
    ("POST", "/api/client-log"),
    // Notifications (owner-scoped) + auth / session (must stay open).
    ("POST", "/api/push/subscribe"),
    ("POST", "/api/push/unsubscribe"),
    ("POST", "/api/push/test"),
    ("POST", "/api/login"),
    ("POST", "/api/login/elevate"),
    ("POST", "/api/login/logout-all"),
    ("POST", "/api/logout"),
    ("DELETE", "/api/login/sessions/{id}"),
];

/// Mutating routes deliberately UNREACHABLE in CityHall.
#[cfg(test)]
pub(super) const CITYHALL_MUTATION_DENY: &[(&str, &str)] = &[
    // Terminal surface.
    ("POST", "/api/sessions/{id}/ensure"),
    ("POST", "/api/sessions/{id}/send"),
    ("POST", "/api/sessions/{id}/terminal"),
    ("DELETE", "/api/sessions/{id}/terminal"),
    ("POST", "/api/sessions/{id}/container-terminal"),
    // Git / project / profile management.
    ("POST", "/api/git/clone"),
    ("POST", "/api/projects"),
    // Attaching a repo to a session takes an arbitrary host path, so it is denied for the
    // same reason `git/clone` and `POST /api/projects` are.
    ("POST", "/api/sessions/{id}/projects"),
    ("PATCH", "/api/projects/{name}"),
    ("DELETE", "/api/projects/{name}"),
    ("POST", "/api/profiles"),
    ("DELETE", "/api/profiles/{name}"),
    ("PATCH", "/api/profiles/{name}/rename"),
    ("PATCH", "/api/default-profile"),
    // MCP mutations.
    ("POST", "/api/mcp/servers/{name}/drop"),
    ("POST", "/api/mcp/servers/{name}/keep"),
    ("POST", "/api/mcp/servers/{name}/resolve"),
    // Skills mutations.
    ("POST", "/api/skills"),
    ("POST", "/api/skills/sync"),
    ("PUT", "/api/skills/{directory}"),
    ("DELETE", "/api/skills/{directory}"),
    ("POST", "/api/skills/{source}/{directory}/adopt"),
    // Plugin lifecycle.
    ("POST", "/api/plugins/install"),
    ("POST", "/api/plugins/install/preview"),
    ("POST", "/api/plugins/{id}/action"),
    ("POST", "/api/plugins/{id}/enabled"),
    ("POST", "/api/plugins/{id}/worker/restart"),
    ("POST", "/api/plugins/{id}/uninstall"),
    ("POST", "/api/plugins/{id}/update/apply"),
    ("POST", "/api/plugins/{id}/update/dismiss"),
    ("POST", "/api/plugins/commands/{fqid}/invoke"),
    // ACP agent / worker lifecycle + config.
    ("DELETE", "/api/sessions/{id}/acp"),
    ("POST", "/api/sessions/{id}/acp/config-option"),
    ("POST", "/api/sessions/{id}/acp/disable"),
    ("POST", "/api/sessions/{id}/acp/enable"),
    ("POST", "/api/sessions/{id}/acp/install-agent"),
    ("POST", "/api/sessions/{id}/acp/mode"),
    ("POST", "/api/sessions/{id}/acp/spawn"),
    ("POST", "/api/sessions/{id}/acp/switch-agent"),
    // Global settings / ops / shared workspace ordering.
    ("PATCH", "/api/settings"),
    ("PATCH", "/api/log-level"),
    ("PUT", "/api/workspace-ordering"),
];

/// Default-deny CityHall reachability boundary.
pub(super) async fn cityhall_gate(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::Method;
    let mutating = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if !state.cityhall_mode || !mutating {
        return next.run(request).await;
    }
    let method = request.method().as_str();
    let template = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string());
    let allowed = template.as_deref().is_some_and(|t| {
        CITYHALL_MUTATION_ALLOW
            .iter()
            .any(|(m, p)| *m == method && *p == t)
    });
    if allowed {
        next.run(request).await
    } else {
        tracing::debug!(
            target: "http.access",
            method,
            template = ?template,
            "rejected: mutating route not reachable in CityHall mode"
        );
        api::cityhall_response()
    }
}

/// Content-Security-Policy for the dashboard.
pub(super) const CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com; \
    font-src 'self'; \
    connect-src 'self' ws: wss:; \
    frame-ancestors 'none'; \
    base-uri 'self'; \
    form-action 'self'; \
    object-src 'none'";

/// Middleware that adds security headers to all responses.
pub(super) async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy", CSP.parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_helpers::vecs;
    use crate::server::test_support;

    /// Extract every mutating `(METHOD, path-template)` pair registered in `build_router`
    /// by scanning `.route("<path>", <handlers>)` and reading the method combinators inside
    /// each handler expression (balanced parens so a nested `get(...).post(...)` doesn't
    /// bleed into the next route).
    fn router_mutating_routes() -> std::collections::BTreeSet<(String, String)> {
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
            // Handler expression.
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

    #[test]
    fn every_mutating_route_is_cityhall_classified() {
        let routed = router_mutating_routes();
        assert!(
            routed.len() > 60,
            "router scan found only {} mutating routes; parser likely broke",
            routed.len()
        );
        let classified: std::collections::BTreeSet<(String, String)> = CITYHALL_MUTATION_ALLOW
            .iter()
            .chain(CITYHALL_MUTATION_DENY.iter())
            .map(|(m, p)| ((*m).to_string(), (*p).to_string()))
            .collect();

        let mut failures = Vec::new();
        for route in routed.difference(&classified) {
            failures.push(format!(
                "{} {} is a mutating route but is in neither CITYHALL_MUTATION_ALLOW nor \
                 CITYHALL_MUTATION_DENY. Add it to the allow table if the CityHall client must \
                 reach it, else to the deny table.",
                route.0, route.1
            ));
        }
        for entry in classified.difference(&routed) {
            failures.push(format!(
                "{} {} is listed in a CityHall table but no router route matches it; remove the \
                 stale entry (path template or method changed?).",
                entry.0, entry.1
            ));
        }
        for a in CITYHALL_MUTATION_ALLOW {
            assert!(
                !CITYHALL_MUTATION_DENY.contains(a),
                "{} {} is in both CityHall allow and deny tables",
                a.0,
                a.1
            );
        }
        assert!(
            failures.is_empty(),
            "CityHall route classification is not exhaustive:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn host_and_origin_are_canonicalized_to_browser_form() {
        // (raw, strip_host_port, norm_host)
        for (raw, stripped, normed) in [
            ("localhost:8080", "localhost", "localhost"),
            ("localhost", "localhost", "localhost"),
            ("127.0.0.1:8080", "127.0.0.1", "127.0.0.1"),
            ("[::1]:8080", "::1", "::1"),
            ("[::1]", "::1", "::1"),
            ("::1", "::1", "::1"),
            ("example.com", "example.com", "example.com"),
            ("example.com.", "example.com.", "example.com"),
            ("example.com.:8080", "example.com.", "example.com"),
            ("LOCALHOST", "LOCALHOST", "localhost"),
        ] {
            assert_eq!(strip_host_port(raw), stripped, "strip_host_port({raw})");
            assert_eq!(norm_host(raw), normed, "norm_host({raw})");
        }

        for (raw, normed) in [
            ("https://x/", "https://x"),
            ("https://x:443", "https://x"),
            ("http://x:80", "http://x"),
            ("https://x:8443", "https://x:8443"),
            ("http://x:443", "http://x:443"),
            ("HTTPS://X", "https://x"),
            ("https://[::1]:443", "https://[::1]"),
            ("https://example.com.", "https://example.com"),
            ("https://example.com.:443", "https://example.com"),
            ("http://example.com.:80", "http://example.com"),
            ("https://example.com.:8443", "https://example.com:8443"),
        ] {
            assert_eq!(norm_origin(raw), normed, "norm_origin({raw})");
        }
        // The Origin gate and the Host gate agree on the trailing FQDN dot.
        assert_eq!(
            norm_origin("https://example.com."),
            format!("https://{}", norm_host("example.com."))
        );

        for (url, host) in [
            ("https://x.trycloudflare.com", Some("x.trycloudflare.com")),
            ("https://foo.ts.net/path?x=1", Some("foo.ts.net")),
            ("https://Foo.TS.net", Some("foo.ts.net")),
            ("", None),
        ] {
            assert_eq!(host_from_url(url).as_deref(), host, "host_from_url({url})");
        }
    }

    #[test]
    fn ip_literal_trust_follows_the_canonicalized_address() {
        for good in [
            "127.0.0.1",
            "192.168.1.5",
            "10.0.0.9",
            "100.68.123.45", // tailnet CGNAT
            "::1",
            "2001:db8::1",
            "fd00::1",            // ULA
            "::ffff:192.168.1.5", // IPv4-mapped routable: canonicalized, then trusted
        ] {
            assert!(is_trusted_ip_literal(good), "{good} should be trusted");
            assert!(!is_untrusted_ip_literal(good), "{good} is not excluded");
        }
        for excluded in [
            "0.0.0.0",
            "::",
            "169.254.169.254", // cloud metadata (v4 link-local)
            "fe80::1",         // v6 link-local
            "224.0.0.1",       // multicast
            "ff02::1",
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:0.0.0.0",
            "::ffff:224.0.0.1",
        ] {
            assert!(!is_trusted_ip_literal(excluded), "{excluded} not trusted");
            assert!(is_untrusted_ip_literal(excluded), "{excluded} is excluded");
        }
        // Hostnames are not IP literals at all, so neither validator claims them.
        for name in ["example.com", "my-box", "aoe.example.com", ""] {
            assert!(!is_trusted_ip_literal(name), "{name} is not an IP literal");
            assert!(
                !is_untrusted_ip_literal(name),
                "{name} is not an IP literal"
            );
        }
    }

    #[test]
    fn evaluate_access_gates_host_then_origin() {
        use AccessDecision::*;
        let hosts = vecs(&["localhost"]);
        let origins = vecs(&["http://localhost:8080"]);
        let d = |host: Option<&str>, origin: Option<&str>| {
            evaluate_access(host, origin, &hosts, &origins)
        };

        assert_eq!(d(Some("localhost"), None), Allow);
        assert_eq!(d(Some("localhost:8080"), None), Allow, "port stripped");
        assert_eq!(d(Some("[::1]:8080"), None), Allow, "bracketed ipv6");
        assert_eq!(d(Some("LOCALHOST"), None), Allow, "case-insensitive");
        assert_eq!(d(Some("localhost."), None), Allow, "trailing fqdn dot");
        assert_eq!(d(Some("evil.com"), None), DenyHost);
        assert_eq!(d(Some("user@localhost"), None), DenyHost, "userinfo");
        assert_eq!(d(None, None), DenyMissingHost);

        // A routable IP literal is trusted without an --allowed-host entry, as Host
        // and as Origin; the excluded literals are not.
        assert_eq!(d(Some("192.168.1.5:8080"), None), Allow);
        assert_eq!(d(Some("[2001:db8::5]:8080"), None), Allow);
        let ip_origin = d(Some("192.168.1.5:8080"), Some("http://192.168.1.5:8080"));
        assert_eq!(ip_origin, Allow);
        assert_eq!(d(Some("0.0.0.0"), None), DenyHost, "unspecified");
        assert_eq!(d(Some("169.254.169.254"), None), DenyHost, "metadata");
        assert_eq!(d(Some("fe80::1"), None), DenyHost, "link-local");

        // Origin is consulted only once the Host passes, and an absent one is exempt.
        assert_eq!(d(Some("localhost"), Some("http://localhost:8080")), Allow);
        assert_eq!(d(Some("localhost"), Some("https://evil.com")), DenyOrigin);
        assert_eq!(d(Some("localhost"), Some("null")), DenyOrigin);
        // An unlisted hostname Origin must not slip through the IP exemption.
        assert_eq!(d(Some("192.168.1.5"), Some("https://evil.com")), DenyOrigin);

        // Origin matching is case-insensitive against the allowlist.
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                Some("https://X.TryCloudflare.com"),
                &hosts,
                &vecs(&["https://x.trycloudflare.com"]),
            ),
            Allow
        );
    }

    #[test]
    fn resolve_access_policy_builds_the_allowlists_the_gate_enforces() {
        // A wildcard bind adds no routable *name*: the static allowlist stays the
        // localhost trio, so a hostname is denied without --allowed-host while a
        // LAN IP literal is trusted unconditionally.
        for wild in ["0.0.0.0", "::", "[::]"] {
            let (h, _o) = resolve_access_policy(wild, 8080, &[], &[], None);
            assert_eq!(
                h,
                vecs(&["localhost", "127.0.0.1", "::1"]),
                "wildcard {wild}"
            );
            assert_eq!(
                evaluate_access(Some("my-box.local"), None, &h, &[]),
                AccessDecision::DenyHost
            );
            assert_eq!(
                evaluate_access(Some("192.168.1.5"), None, &h, &[]),
                AccessDecision::Allow
            );
        }

        // A concrete bind trusts itself as Host and as Origin.
        let (h, o) = resolve_access_policy("192.168.1.5", 8080, &[], &[], None);
        assert!(h.contains(&"192.168.1.5".to_string()));
        assert!(o.contains(&"http://192.168.1.5:8080".to_string()));

        // `--allowed-host` extends both lists, with and without the port, and a
        // trailing FQDN dot is normalized away on both sides of the comparison.
        let (h, o) =
            resolve_access_policy("0.0.0.0", 8080, &vecs(&["aoe.example.com."]), &[], None);
        assert!(h.contains(&"aoe.example.com".to_string()));
        assert!(o.contains(&"https://aoe.example.com".to_string()));
        assert!(o.contains(&"https://aoe.example.com:8080".to_string()));
        assert_eq!(
            evaluate_access(
                Some("aoe.example.com."),
                Some("https://aoe.example.com."),
                &h,
                &o
            ),
            AccessDecision::Allow
        );
        // A portless allowlist entry still matches the browser's default-port Origin.
        assert_eq!(
            evaluate_access(
                Some("aoe.example.com"),
                Some("https://aoe.example.com:443"),
                &h,
                &o
            ),
            AccessDecision::Allow
        );

        // A tunnel host is injected as an https Origin as well as a Host.
        for tunnel in ["x.trycloudflare.com", "host.tailnet.ts.net"] {
            let (h, o) = resolve_access_policy("127.0.0.1", 8080, &[], &[], Some(tunnel));
            assert!(h.contains(&tunnel.to_string()), "{tunnel} host");
            assert!(o.contains(&format!("https://{tunnel}")), "{tunnel} origin");
            assert_eq!(
                evaluate_access(Some(tunnel), Some(&format!("https://{tunnel}")), &h, &o),
                AccessDecision::Allow
            );
        }

        // `--allowed-origin` values are stored in canonical browser form.
        let (_h, o) = resolve_access_policy(
            "127.0.0.1",
            8080,
            &[],
            &vecs(&[
                "https://aoe.example.com:8443",
                "https://trail.example.com/",
                "https://std.example.com:443",
            ]),
            None,
        );
        assert!(o.contains(&"https://aoe.example.com:8443".to_string()));
        assert!(o.contains(&"https://trail.example.com".to_string()));
        assert!(!o.contains(&"https://trail.example.com/".to_string()));
        assert!(o.contains(&"https://std.example.com".to_string()));
    }

    /// The gate runs ahead of auth (403 wins over 401) and every rejection returns the
    /// same non-leaking body whatever the reason.
    #[tokio::test]
    async fn access_gate_runs_before_auth_at_the_router() {
        use axum::http::StatusCode;
        use tower::ServiceExt;

        // Status plus body, so the generic deny text is asserted alongside the verdict.
        async fn probe(
            allowed_hosts: &[&str],
            uri: &str,
            host: Option<&str>,
            origin: Option<&str>,
        ) -> (StatusCode, String) {
            let state = test_support::build_test_app_state_with_policy(
                Vec::new(),
                vecs(allowed_hosts),
                vecs(&["http://localhost:8080"]),
                Some("secret-token".to_string()),
            );
            let mut builder = axum::http::Request::builder().uri(uri);
            for (name, value) in [("host", host), ("origin", origin)] {
                if let Some(value) = value {
                    builder = builder.header(name, value);
                }
            }
            let mut req = builder.body(axum::body::Body::empty()).unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(
                "203.0.113.7:5555".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let resp = test_support::build_router_for_test(state)
                .oneshot(req)
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
        let denied = (
            StatusCode::FORBIDDEN,
            "forbidden: host or origin not allowed".to_string(),
        );
        let local: &[&str] = &["localhost"];
        let api = "/api/sessions";

        assert_eq!(probe(local, api, Some("evil.com"), None).await, denied);
        assert_eq!(
            probe(local, api, Some("localhost"), Some("https://evil.com")).await,
            denied
        );
        // A listed Host or Origin clears the gate and lands on auth instead.
        for origin in [None, Some("http://localhost:8080")] {
            let (status, _) = probe(local, api, Some("localhost"), origin).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "origin {origin:?}");
        }

        // With no Host header the gate falls back to the absolute-form `:authority`.
        let (status, _) = probe(
            &["x.trycloudflare.com"],
            "http://x.trycloudflare.com/api/sessions",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            probe(
                local,
                "http://evil.trycloudflare.com/api/sessions",
                None,
                None
            )
            .await,
            denied
        );
    }

    #[test]
    fn csp_parses_as_valid_header_value() {
        // Catches typos that would make the header unparseable.
        let parsed: axum::http::HeaderValue = CSP.parse().expect("CSP must parse");
        let rendered = parsed.to_str().expect("CSP must be ASCII");
        // Spot-check load-bearing directives so a future edit that
        // accidentally drops one fails loudly.
        for needle in [
            "default-src 'self'",
            "script-src 'self' 'wasm-unsafe-eval'",
            "img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com",
            "connect-src 'self' ws: wss:",
            "frame-ancestors 'none'",
        ] {
            assert!(
                rendered.contains(needle),
                "CSP is missing required directive fragment `{needle}`"
            );
        }
    }
}
