//! Who may reach the dashboard: the host and origin allowlist, the
//! city-hall mutation gate, and the security headers every response carries.

use std::sync::Arc;

use super::state::AppState;
use crate::server::api;

/// True when `host` is a wildcard bind ("all interfaces") rather than a
/// concrete, routable name a browser would send back as `Host`.
pub(crate) fn is_wildcard_bind(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

/// Strip an optional `:port` and IPv6 brackets from a `Host`/authority value,
/// yielding the canonical bare host. `localhost:8080` -> `localhost`,
/// `[::1]:8080` -> `::1`, `127.0.0.1` -> `127.0.0.1`. A bare (unbracketed)
/// IPv6 literal has multiple colons and no port, so it is returned unchanged.
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

/// Canonical host key for the allowlist and for `Host` comparison: bare host,
/// ASCII-lowercased (DNS is case-insensitive), with a single trailing FQDN
/// root dot stripped so `example.com.` and `example.com` compare equal. Runs
/// on both the incoming `Host` and every allowlist entry, so the two stay
/// symmetric. `pub(crate)` so the CLI `--allowed-host` validator can reject an
/// entry that normalizes to nothing (e.g. `:8080`).
pub(crate) fn norm_host(host: &str) -> String {
    let bare = strip_host_port(host);
    bare.strip_suffix('.').unwrap_or(bare).to_ascii_lowercase()
}

/// True when a `norm_host`'d value is a routable IP literal we trust
/// unconditionally. An IP literal is dialed directly and never DNS-resolved, so
/// it cannot be the target of DNS rebinding: a browser only sends an IP as
/// `Host`/`Origin` when the user navigated straight to that address. Trusting
/// it restores `aoe serve --host 0.0.0.0` reachability by LAN/tailnet IP with
/// no `--allowed-host` (Vite's "Pattern A"). Hostnames are NOT trusted here and
/// still require an explicit allowlist entry. See #2735.
///
/// The excluded ranges are hygiene, not rebinding-necessity (IPs can't be
/// rebound): the unspecified address (`0.0.0.0` / `::`, also a Linux/macOS
/// rebinding bypass), multicast, and link-local (v4 `169.254.0.0/16`, which
/// contains the `169.254.169.254` cloud-metadata address; v6 `fe80::/10`) are
/// never a legitimate dashboard endpoint. Routable IPs (LAN, tailnet
/// `100.64.0.0/10`, ULA, global unicast) are trusted. IPv4-mapped IPv6 forms
/// (`::ffff:a.b.c.d`) are canonicalized first so those exclusions also cover
/// e.g. `::ffff:169.254.169.254`.
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

/// True when a `norm_host`'d value parses as an IP literal the gate refuses to
/// trust: unspecified (`0.0.0.0` / `::`), link-local, or multicast. A hostname
/// is not an IP literal and returns false, so the CLI validators still accept
/// `aoe.example.com`. This is the inverse of `is_trusted_ip_literal` over the
/// values that actually parse as an IP; sharing the one predicate keeps the
/// `--allowed-host` / `--allowed-origin` validators from ever admitting an entry
/// that the gate's trust check excludes (the exact ordering bypass where an
/// allowlist match wins before `is_trusted_ip_literal` runs). See #2735.
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

/// Canonicalize an `Origin` to the exact form a browser serializes: trimmed,
/// ASCII-lowercased, no trailing slash, and with the scheme's default port
/// elided (`https://x:443` -> `https://x`, `http://x:80` -> `http://x`). Runs
/// on both the allowlist build and the incoming header so the two never drift;
/// without it a copy-pasted `https://x/` or `https://x:443` would silently 403
/// every request. See #2735.
pub(super) fn norm_origin(origin: &str) -> String {
    let o = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    // Strip a single trailing FQDN root dot from the host so
    // `https://example.com.` == `https://example.com`, mirroring `norm_host`.
    // The dot sits at the authority end or just before `:port`; IPv6
    // authorities are bracketed (`]` precedes any port), so a `.` / `.:` here
    // is only ever the root dot. A trailing dot (the `Some` arm) ends the
    // authority, so no `:port` follows and `.:` cannot also be present; the two
    // arms are mutually exclusive, which is why the dot arm skips the `replacen`
    // that only the `.:port` form needs.
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

/// Resolve the `(allowed_hosts, allowed_origins)` pair the DNS-rebinding gate
/// enforces. Pure so the defaulting, wildcard handling, and tunnel
/// auto-injection are unit-testable without a live server (#2735).
///
/// - Loopback trio (`localhost`, `127.0.0.1`, `::1`) is always trusted, plus
///   the concrete bind `host` (wildcards excluded: they mean "all interfaces",
///   not a routable Host).
/// - Each local host gets `http`/`https` origins on the actual bind `port`.
/// - Operator `--allowed-host` entries are trusted for direct access on the
///   bind port and for standard-port (proxy) access.
/// - A `tunnel_host` (Cloudflare/Tailscale public name) is auto-injected with
///   its portless `https` origin, so tunnels work with no operator flag.
/// - Operator `--allowed-origin` entries are normalized to the browser's
///   `Origin` form (lowercased, no trailing slash, default port elided) for
///   reverse proxies on nonstandard ports.
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

/// Pure DNS-rebinding decision: reject a missing `Host`; accept a `Host` that
/// is allowlisted or a routable IP literal (IPs can't be rebound, see
/// `is_trusted_ip_literal`); exempt requests with no `Origin` (curl / native
/// TUI / non-browser WS); reject a present `Origin` that is neither allowlisted
/// nor a routable IP literal. Comparisons are case-insensitive on the host and
/// on the whole origin. See #2735.
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
        // A by-IP dashboard (`http://<ip>:port`) sends `Origin: http://<ip>:port`
        // on its own fetch/WS, so trust an IP-literal origin on the same basis
        // as the Host. This is a deliberate relaxation: a cross-origin page
        // served from a bare IP would also pass this check, but it cannot read
        // the auth token, so auth remains the backstop; a per-origin allowlist
        // is the deferred stricter posture. `host_from_url` strips
        // scheme/port/brackets.
        let origin_is_trusted_ip =
            host_from_url(&origin).is_some_and(|h| is_trusted_ip_literal(&h));
        if !allowed_origins.contains(&origin) && !origin_is_trusted_ip {
            return AccessDecision::DenyOrigin;
        }
    }
    AccessDecision::Allow
}

/// Uniform 403 for every DNS-rebinding rejection. Names both gates but not
/// which one tripped, so it is accurate for a missing/unlisted `Host` and an
/// unlisted `Origin` alike without handing a prober a which-check oracle; the
/// specific reason stays in the `http.access` debug log. See #2735.
pub(super) fn access_denied() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::FORBIDDEN,
        "forbidden: host or origin not allowed",
    )
        .into_response()
}

/// DNS-rebinding gate. Runs before `auth_middleware` (layered outside it) so a
/// rejected request never reaches auth: the 403 short-circuits here. HTTP/1.1
/// always carries `Host`; for HTTP/2 the `:authority` pseudo-header maps to it,
/// with the URI authority as a fallback. See #2735.
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
/// Entries are `(method, matched-path template)`. `cityhall_gate` refuses any
/// mutating request whose `(method, template)` is not listed here, BEFORE the
/// handler runs, so reachability is default-deny: a new mutating route is closed
/// until deliberately classified (in this table or [`CITYHALL_MUTATION_DENY`]).
/// This replaces the previous per-handler deny-list as the enforcement boundary;
/// the handlers keep their `cityhall_block*` calls as defense in depth. Reads
/// (GET/HEAD) pass the gate; the few sensitive ones keep their per-handler
/// guard. See #7.
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
    // Server-owned prompt queue: deferred prompting into a session the caller
    // already sees, so it is classified exactly like `acp/prompt` above.
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

/// Mutating routes deliberately UNREACHABLE in CityHall. Same shape as
/// [`CITYHALL_MUTATION_ALLOW`]; kept explicit so the
/// `every_mutating_route_is_cityhall_classified` audit can prove every
/// router-registered mutation is consciously classified (a new one absent from
/// both tables fails the build). `cityhall_gate` denies these anyway (they are
/// simply not in the allow table), but listing them documents the intent and
/// lets the audit prove exhaustiveness, so it is only needed under `cfg(test)`.
/// #7.
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
    // Attaching a repo to a session (#3103) takes an arbitrary host path, so it
    // is denied for the same reason `git/clone` and `POST /api/projects` are: it
    // would let a CityHall client create a git worktree anywhere the daemon user
    // can write, and it also stops the agent worker and removes the sandbox
    // container. The session lifecycle routes this mode does allow all operate on
    // state the session already owns.
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

/// Default-deny CityHall reachability boundary. A no-op outside CityHall mode
/// and for read methods (GET/HEAD/OPTIONS); for a mutating method it refuses any
/// request whose matched-path template is not in [`CITYHALL_MUTATION_ALLOW`]
/// with the canonical 403. This is the single choke point the reviewer asked
/// for: it covers every module prefix and method uniformly (an unmatched or
/// unlisted mutating route fails closed), so a handler can no longer silently
/// reopen a hole by omission. See #7.
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
///
/// - `default-src 'self'`: deny everything we don't explicitly allow.
/// - `script-src 'self' 'wasm-unsafe-eval'`: scripts are bundled by
///   Vite from the same origin; no inline scripts, no `eval`. The
///   `'wasm-unsafe-eval'` source is the CSP3 opt-in for WebAssembly
///   compilation; Shiki's Oniguruma regex engine ships as WASM, so
///   the diff syntax highlighter falls over without it (PR #1275
///   dropped this when wterm was replaced with xterm.js on the
///   incorrect premise that nothing else still needed WASM).
/// - `style-src 'self' 'unsafe-inline'`: React writes to element.style at
///   runtime (terminal font-size updates) and Tailwind v4 emits inline
///   `<style>` blocks in dev. Blocking inline styles breaks xterm.js's
///   rendered viewport.
/// - `img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com`:
///   repo-owner avatars are loaded from `github.com/{user}.png` which 302s
///   to `avatars.githubusercontent.com`; CSP checks both URLs across the
///   redirect, so both hosts must be allowed. `data:` covers inline icons.
///   `raw.githubusercontent.com` serves plugin screenshots resolved by the
///   plugin detail endpoint (#2484).
/// - `font-src 'self'`: Geist fonts are bundled under /fonts/.
/// - `connect-src 'self' ws: wss:`: REST + PTY WebSocket to same origin.
/// - `frame-ancestors 'none'`: CSP-native equivalent of X-Frame-Options.
/// - `base-uri 'self'`, `form-action 'self'`, `object-src 'none'`: tighten
///   the usual attack surfaces on injection bugs.
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
    /// Extract every mutating `(METHOD, path-template)` pair registered in
    /// `build_router` by scanning `.route("<path>", <handlers>)` and reading the
    /// method combinators inside each handler expression (balanced parens so a
    /// nested `get(...).post(...)` doesn't bleed into the next route). Shared by
    /// the CityHall table-exhaustiveness audit below.
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

    use crate::server::test_helpers::vecs;
    use crate::server::test_support;

    /// CityHall audit (route-table exhaustiveness, replaces the old
    /// handler-body text scan). Both sides are route enumerations, so it is
    /// sound where a text scan was not: every mutating route the router
    /// registers (ANY module prefix, ANY method) must appear in exactly the
    /// `CITYHALL_MUTATION_ALLOW` / `CITYHALL_MUTATION_DENY` tables that drive the
    /// default-deny `cityhall_gate`. A new mutating route absent from both fails
    /// the build (forcing a reachable/closed decision), and a stale table entry
    /// with no matching route also fails. See #7.
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
        for route in &routed {
            if !classified.contains(route) {
                failures.push(format!(
                    "{} {} is a mutating route but is in neither CITYHALL_MUTATION_ALLOW nor \
                     CITYHALL_MUTATION_DENY. Add it to the allow table if the CityHall client \
                     must reach it, else to the deny table.",
                    route.0, route.1
                ));
            }
        }
        for entry in &classified {
            if !routed.contains(entry) {
                failures.push(format!(
                    "{} {} is listed in a CityHall table but no router route matches it; remove \
                     the stale entry (path template or method changed?).",
                    entry.0, entry.1
                ));
            }
        }
        // Allow and deny must be disjoint.
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
    fn strip_host_port_variants() {
        assert_eq!(strip_host_port("localhost:8080"), "localhost");
        assert_eq!(strip_host_port("localhost"), "localhost");
        assert_eq!(strip_host_port("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(strip_host_port("[::1]:8080"), "::1");
        assert_eq!(strip_host_port("[::1]"), "::1");
        assert_eq!(strip_host_port("::1"), "::1");
        assert_eq!(strip_host_port("example.com"), "example.com");
    }

    #[test]
    fn host_from_url_extracts_bare_host() {
        assert_eq!(
            host_from_url("https://x.trycloudflare.com").as_deref(),
            Some("x.trycloudflare.com")
        );
        assert_eq!(
            host_from_url("https://foo.ts.net/path?x=1").as_deref(),
            Some("foo.ts.net")
        );
        assert_eq!(
            host_from_url("https://Foo.TS.net").as_deref(),
            Some("foo.ts.net")
        );
        assert_eq!(host_from_url(""), None);
    }

    #[test]
    fn host_in_allowlist_passes() {
        assert_eq!(
            evaluate_access(Some("localhost"), None, &vecs(&["localhost"]), &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn host_not_in_allowlist_403() {
        assert_eq!(
            evaluate_access(Some("evil.com"), None, &vecs(&["localhost"]), &[]),
            AccessDecision::DenyHost
        );
    }

    #[test]
    fn host_port_stripped_before_match() {
        assert_eq!(
            evaluate_access(Some("localhost:8080"), None, &vecs(&["localhost"]), &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn host_ipv6_bracketed_port_stripped() {
        assert_eq!(
            evaluate_access(Some("[::1]:8080"), None, &vecs(&["::1"]), &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn host_match_is_case_insensitive() {
        assert_eq!(
            evaluate_access(Some("LOCALHOST"), None, &vecs(&["localhost"]), &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn missing_host_denied() {
        assert_eq!(
            evaluate_access(None, None, &vecs(&["localhost"]), &[]),
            AccessDecision::DenyMissingHost
        );
    }

    #[test]
    fn origin_absent_is_exempt() {
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                None,
                &vecs(&["localhost"]),
                &vecs(&["http://localhost:8080"])
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn origin_in_allowlist_passes() {
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                Some("http://localhost:8080"),
                &vecs(&["localhost"]),
                &vecs(&["http://localhost:8080"])
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn origin_not_in_allowlist_403() {
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                Some("https://evil.com"),
                &vecs(&["localhost"]),
                &vecs(&["http://localhost:8080"])
            ),
            AccessDecision::DenyOrigin
        );
    }

    #[test]
    fn origin_match_is_case_insensitive() {
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                Some("https://X.TryCloudflare.com"),
                &vecs(&["localhost"]),
                &vecs(&["https://x.trycloudflare.com"])
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn null_origin_is_denied() {
        assert_eq!(
            evaluate_access(
                Some("localhost"),
                Some("null"),
                &vecs(&["localhost"]),
                &vecs(&["http://localhost:8080"])
            ),
            AccessDecision::DenyOrigin
        );
    }

    #[test]
    fn userinfo_host_is_denied() {
        assert_eq!(
            evaluate_access(Some("user@localhost"), None, &vecs(&["localhost"]), &[]),
            AccessDecision::DenyHost
        );
    }

    #[test]
    fn wildcard_bind_defaults_to_localhost_trio() {
        let (h, _o) = resolve_access_policy("0.0.0.0", 8080, &[], &[], None);
        // The static allowlist is still just the trio; a wildcard bind adds no
        // routable *name*. A HOSTNAME is still denied without --allowed-host.
        assert_eq!(h, vecs(&["localhost", "127.0.0.1", "::1"]));
        assert_eq!(
            evaluate_access(Some("my-box.local"), None, &h, &[]),
            AccessDecision::DenyHost
        );
        // But a LAN IP literal is trusted unconditionally (Pattern A: an IP
        // cannot be DNS-rebound), so by-IP access works with no flag.
        assert_eq!(
            evaluate_access(Some("192.168.1.5"), None, &h, &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn is_trusted_ip_literal_accepts_routable_rejects_special() {
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
        }
        for bad in [
            "0.0.0.0",
            "::",
            "169.254.169.254", // cloud metadata (v4 link-local)
            "fe80::1",         // v6 link-local
            "224.0.0.1",       // multicast
            "ff02::1",
            "::ffff:169.254.169.254", // IPv4-mapped metadata: canonicalized, then excluded
            "::ffff:0.0.0.0",         // IPv4-mapped unspecified
            "::ffff:224.0.0.1",       // IPv4-mapped multicast
            "example.com",
            "my-box",
            "",
        ] {
            assert!(!is_trusted_ip_literal(bad), "{bad} must not be trusted");
        }
    }

    #[test]
    fn is_untrusted_ip_literal_flags_only_excluded_literals() {
        for excluded in [
            "0.0.0.0",
            "::",
            "169.254.169.254",
            "fe80::1",
            "224.0.0.1",
            "ff02::1",
            "::ffff:169.254.169.254",
        ] {
            assert!(
                is_untrusted_ip_literal(excluded),
                "{excluded} is an IP literal the gate excludes"
            );
        }
        // Routable/loopback literals pass, and hostnames are not IP literals at
        // all, so both must clear the validators.
        for allowed in [
            "127.0.0.1",
            "::1",
            "192.168.1.5",
            "100.68.123.45",
            "2001:db8::1",
            "aoe.example.com",
            "my-box",
            "",
        ] {
            assert!(
                !is_untrusted_ip_literal(allowed),
                "{allowed} must not be flagged as an untrusted IP literal"
            );
        }
    }

    #[test]
    fn ip_literal_host_allowed_without_flag() {
        let allow = vecs(&["localhost"]);
        assert_eq!(
            evaluate_access(Some("192.168.1.5:8080"), None, &allow, &[]),
            AccessDecision::Allow
        );
        assert_eq!(
            evaluate_access(Some("[2001:db8::5]:8080"), None, &allow, &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn ip_literal_origin_allowed_without_flag() {
        let allow = vecs(&["localhost"]);
        assert_eq!(
            evaluate_access(
                Some("192.168.1.5:8080"),
                Some("http://192.168.1.5:8080"),
                &allow,
                &[]
            ),
            AccessDecision::Allow
        );
        assert_eq!(
            evaluate_access(
                Some("[2001:db8::5]:8080"),
                Some("http://[2001:db8::5]:8080"),
                &allow,
                &[]
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn excluded_ip_literals_still_denied() {
        let allow = vecs(&["localhost"]);
        for bad in ["0.0.0.0", "169.254.169.254", "fe80::1"] {
            assert_eq!(
                evaluate_access(Some(bad), None, &allow, &[]),
                AccessDecision::DenyHost,
                "{bad}"
            );
        }
        // An unlisted hostname origin must not slip through the IP exemption.
        assert_eq!(
            evaluate_access(
                Some("192.168.1.5"),
                Some("https://evil.com"),
                &allow,
                &vecs(&["http://localhost:8080"])
            ),
            AccessDecision::DenyOrigin
        );
    }

    #[test]
    fn concrete_bind_host_is_allowed() {
        let (h, o) = resolve_access_policy("192.168.1.5", 8080, &[], &[], None);
        assert!(h.contains(&"192.168.1.5".to_string()));
        assert!(o.contains(&"http://192.168.1.5:8080".to_string()));
    }

    #[test]
    fn explicit_host_flag_extends_allowlist() {
        let (h, o) = resolve_access_policy("0.0.0.0", 8080, &vecs(&["aoe.example.com"]), &[], None);
        assert!(h.contains(&"aoe.example.com".to_string()));
        assert!(o.contains(&"https://aoe.example.com".to_string()));
        assert!(o.contains(&"https://aoe.example.com:8080".to_string()));
    }

    #[test]
    fn remote_tunnel_host_auto_injected() {
        let (h, _o) =
            resolve_access_policy("127.0.0.1", 8080, &[], &[], Some("x.trycloudflare.com"));
        assert!(h.contains(&"x.trycloudflare.com".to_string()));
        assert_eq!(
            evaluate_access(Some("x.trycloudflare.com"), None, &h, &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn tunnel_origin_auto_injected() {
        let (h, o) =
            resolve_access_policy("127.0.0.1", 8080, &[], &[], Some("x.trycloudflare.com"));
        assert!(o.contains(&"https://x.trycloudflare.com".to_string()));
        assert_eq!(
            evaluate_access(
                Some("x.trycloudflare.com"),
                Some("https://x.trycloudflare.com"),
                &h,
                &o
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn tailscale_host_auto_injected() {
        let (h, o) =
            resolve_access_policy("127.0.0.1", 8080, &[], &[], Some("host.tailnet.ts.net"));
        assert!(h.contains(&"host.tailnet.ts.net".to_string()));
        assert!(o.contains(&"https://host.tailnet.ts.net".to_string()));
    }

    #[test]
    fn explicit_origin_flag_normalized() {
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

    #[test]
    fn norm_origin_canonicalizes_to_browser_form() {
        assert_eq!(norm_origin("https://x/"), "https://x");
        assert_eq!(norm_origin("https://x:443"), "https://x");
        assert_eq!(norm_origin("http://x:80"), "http://x");
        assert_eq!(norm_origin("https://x:8443"), "https://x:8443");
        assert_eq!(norm_origin("http://x:443"), "http://x:443");
        assert_eq!(norm_origin("HTTPS://X"), "https://x");
        assert_eq!(norm_origin("https://[::1]:443"), "https://[::1]");
    }

    #[test]
    fn norm_origin_strips_trailing_fqdn_dot() {
        assert_eq!(norm_origin("https://example.com."), "https://example.com");
        assert_eq!(
            norm_origin("https://example.com.:443"),
            "https://example.com"
        );
        assert_eq!(norm_origin("http://example.com.:80"), "http://example.com");
        assert_eq!(
            norm_origin("https://example.com.:8443"),
            "https://example.com:8443"
        );
        // Symmetric with the Host gate.
        assert_eq!(
            norm_origin("https://example.com."),
            format!("https://{}", norm_host("example.com."))
        );
    }

    #[test]
    fn origin_default_port_matches_portless_allowlist() {
        let (_h, o) =
            resolve_access_policy("127.0.0.1", 8080, &vecs(&["proxy.example.com"]), &[], None);
        assert_eq!(
            evaluate_access(
                Some("proxy.example.com"),
                Some("https://proxy.example.com:443"),
                &vecs(&["proxy.example.com"]),
                &o
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn norm_host_strips_trailing_dot() {
        assert_eq!(norm_host("example.com."), "example.com");
        assert_eq!(norm_host("example.com.:8080"), "example.com");
        assert_eq!(norm_host("[::1]:8080"), "::1");
    }

    #[test]
    fn trailing_dot_host_matches_allowlist() {
        let (h, _o) =
            resolve_access_policy("0.0.0.0", 8080, &vecs(&["aoe.example.com."]), &[], None);
        assert!(h.contains(&"aoe.example.com".to_string()));
        assert_eq!(
            evaluate_access(Some("aoe.example.com."), None, &h, &[]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn trailing_dot_origin_matches_allowlist() {
        let (h, o) = resolve_access_policy("0.0.0.0", 8080, &vecs(&["aoe.example.com"]), &[], None);
        assert_eq!(
            evaluate_access(
                Some("aoe.example.com."),
                Some("https://aoe.example.com."),
                &h,
                &o
            ),
            AccessDecision::Allow
        );
    }

    #[test]
    fn wildcard_bind_ipv6_forms_default_to_trio() {
        for wild in ["::", "[::]"] {
            let (h, _o) = resolve_access_policy(wild, 8080, &[], &[], None);
            assert_eq!(
                h,
                vecs(&["localhost", "127.0.0.1", "::1"]),
                "wildcard {wild}"
            );
        }
    }

    #[tokio::test]
    async fn access_policy_rejects_unlisted_host_at_router() {
        use tower::ServiceExt;
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["localhost"]),
            vecs(&["http://localhost:8080"]),
            None,
        );
        let app = test_support::build_router_for_test(state);
        let req = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "evil.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn access_policy_runs_before_auth() {
        use tower::ServiceExt;
        let remote: std::net::SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let make_state = || {
            test_support::build_test_app_state_with_policy(
                Vec::new(),
                vecs(&["localhost"]),
                vecs(&["http://localhost:8080"]),
                Some("secret-token".to_string()),
            )
        };

        let app = test_support::build_router_for_test(make_state());
        let mut bad = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "evil.com")
            .body(axum::body::Body::empty())
            .unwrap();
        bad.extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let resp = app.oneshot(bad).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "an unlisted Host must 403 before auth can 401"
        );

        let app = test_support::build_router_for_test(make_state());
        let mut good = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        good.extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let resp = app.oneshot(good).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "a listed Host passes the gate and reaches auth"
        );
    }

    #[tokio::test]
    async fn denied_body_is_generic_for_host_and_origin() {
        use tower::ServiceExt;
        async fn body_of(resp: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }
        let make_state = || {
            test_support::build_test_app_state_with_policy(
                Vec::new(),
                vecs(&["localhost"]),
                vecs(&["http://localhost:8080"]),
                None,
            )
        };

        let app = test_support::build_router_for_test(make_state());
        let host_deny = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "evil.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let host_body = body_of(app.oneshot(host_deny).await.unwrap()).await;

        let app = test_support::build_router_for_test(make_state());
        let origin_deny = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "localhost")
            .header("origin", "https://evil.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let origin_body = body_of(app.oneshot(origin_deny).await.unwrap()).await;

        assert_eq!(host_body, "forbidden: host or origin not allowed");
        assert_eq!(
            host_body, origin_body,
            "both deny reasons must return an identical, non-leaking body"
        );
    }

    #[tokio::test]
    async fn listed_origin_passes_gate_to_auth() {
        use tower::ServiceExt;
        let remote: std::net::SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["localhost"]),
            vecs(&["http://localhost:8080"]),
            Some("secret-token".to_string()),
        );
        let app = test_support::build_router_for_test(state);
        let mut req = axum::http::Request::builder()
            .uri("/api/sessions")
            .header("host", "localhost")
            .header("origin", "http://localhost:8080")
            .body(axum::body::Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "a listed Origin must pass the gate and reach auth"
        );
    }

    #[tokio::test]
    async fn access_policy_authority_fallback_allows_listed() {
        use tower::ServiceExt;
        let remote: std::net::SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["x.trycloudflare.com"]),
            Vec::new(),
            Some("secret-token".to_string()),
        );
        let app = test_support::build_router_for_test(state);
        // Absolute-form URI + no Host header: access_policy falls back to
        // request.uri().authority() (the HTTP/2 :authority path).
        let mut req = axum::http::Request::builder()
            .uri("http://x.trycloudflare.com/api/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(req.headers().get(axum::http::header::HOST).is_none());
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "an :authority in the allowlist passes the gate and reaches auth"
        );
    }

    #[tokio::test]
    async fn access_policy_authority_fallback_rejects_unlisted() {
        use tower::ServiceExt;
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["localhost"]),
            Vec::new(),
            None,
        );
        let app = test_support::build_router_for_test(state);
        let req = axum::http::Request::builder()
            .uri("http://evil.trycloudflare.com/api/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn csp_parses_as_valid_header_value() {
        // Catches typos that would make the header unparseable.
        // security_headers() calls `.parse().unwrap()` at request time;
        // this test surfaces any regression at `cargo test` time instead.
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
