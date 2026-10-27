//! Per-request ingress provenance and the plaintext-remote gate.
//!
//! Forwarded headers are read here and nowhere else, and only from the immediate peer of a
//! configured proxy (`proxy_mode`, whose trusted peers are the loopback addresses every
//! supported proxy and tunnel terminates onto). Everything downstream reads the resulting
//! [`Ingress`] instead of re-deriving trust from raw headers.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Rate-limit and login key for a proxied request that names no valid client address.
/// Such callers share one bucket; none is ever mistaken for the loopback peer.
const UNKNOWN_CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ingress {
    /// Identity for login attempts and auth rate limiting.
    pub client_ip: IpAddr,
    /// The connection is not known to be encrypted and is not a direct loopback one.
    pub plaintext_remote: bool,
    /// A trusted proxy asserted that the client connected over HTTPS.
    pub externally_secure: bool,
}

impl AppState {
    pub(crate) fn ingress(&self, peer: SocketAddr, headers: &HeaderMap) -> Ingress {
        Ingress::resolve(peer, headers, self.behind_tunnel, self.trust_cf_header)
    }
}

impl Ingress {
    pub(crate) fn resolve(
        peer: SocketAddr,
        headers: &HeaderMap,
        proxy_mode: bool,
        trust_cf_header: bool,
    ) -> Self {
        let peer_ip = peer.ip().to_canonical();
        let via_trusted_proxy = proxy_mode && peer_ip.is_loopback();
        let forwarded_proto = via_trusted_proxy
            .then(|| last_list_element(headers, "x-forwarded-proto"))
            .flatten();
        let externally_secure = forwarded_proto
            .is_some_and(|p| p.eq_ignore_ascii_case("https") || p.eq_ignore_ascii_case("wss"));
        let forwarded_plaintext = forwarded_proto.is_some() && !externally_secure;
        Self {
            client_ip: if via_trusted_proxy {
                forwarded_client_ip(headers, trust_cf_header)
            } else {
                peer_ip
            },
            plaintext_remote: !externally_secure && (!peer_ip.is_loopback() || forwarded_plaintext),
            externally_secure,
        }
    }
}

/// The last comma-separated element of the last line of `name`: the entry the nearest proxy
/// appended, which a client cannot forge.
fn last_list_element<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let line = headers.get_all(name).iter().next_back()?.to_str().ok()?;
    line.rsplit(',').next().map(str::trim)
}

/// `cf-connecting-ip` when `trust_cf_header` and the tunnel sent it, else the last
/// `x-forwarded-for` element. A generic reverse proxy may pass a client-chosen
/// `cf-connecting-ip` through untouched, so it is ignored there. A missing, repeated or
/// malformed value is [`UNKNOWN_CLIENT`].
fn forwarded_client_ip(headers: &HeaderMap, trust_cf_header: bool) -> IpAddr {
    let mut cf = headers.get_all("cf-connecting-ip").iter();
    let parsed = match (trust_cf_header, cf.next(), cf.next()) {
        (true, Some(_), Some(_)) => None,
        (true, Some(value), None) => value.to_str().ok().and_then(|v| v.trim().parse().ok()),
        _ => last_list_element(headers, "x-forwarded-for").and_then(|v| v.parse::<IpAddr>().ok()),
    };
    parsed.unwrap_or(UNKNOWN_CLIENT)
}

/// Rejects plaintext non-loopback requests with 426 before any other gate runs, unless
/// the bind explicitly opted out.
pub(super) async fn plaintext_gate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // The owner-only Unix listener is never remote, and an unstamped request has no peer
    // to judge: every real listener stamps one.
    let peer = match request
        .extensions()
        .get::<ConnectInfo<super::peer::ConnectionPeer>>()
    {
        Some(ConnectInfo(super::peer::ConnectionPeer::Tcp(addr))) => Some(*addr),
        Some(_) => None,
        None => request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr),
    };
    if let Some(peer) = peer {
        let ingress = state.ingress(peer, request.headers());
        if ingress.plaintext_remote && !state.allow_plaintext_remote {
            tracing::debug!(target: "http.access", peer = %peer.ip(), "rejected plaintext remote request");
            return super::api::api_error_with_code_header(
                crate::daemon::ApiErrorCode::TlsRequired,
                "HTTPS is required for non-loopback connections",
            );
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `proxy_mode` also trusts `cf-connecting-ip`, as `--remote` does.
    fn resolve(peer: &str, headers: &[(&'static str, &str)], proxy_mode: bool) -> Ingress {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(*name, value.parse().unwrap());
        }
        Ingress::resolve(
            SocketAddr::new(ip(peer), 4000),
            &map,
            proxy_mode,
            proxy_mode,
        )
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forwarded_headers_count_only_from_a_loopback_peer_in_proxy_mode() {
        let forged = [
            ("cf-connecting-ip", "203.0.113.50"),
            ("x-forwarded-for", "203.0.113.50"),
            ("x-forwarded-proto", "https"),
        ];
        // Without proxy mode a loopback peer's headers assert nothing.
        let direct = resolve("127.0.0.1", &forged, false);
        assert_eq!(direct.client_ip, ip("127.0.0.1"));
        assert!(!direct.externally_secure && !direct.plaintext_remote);
        // A remote peer cannot assert TLS or identity in either mode.
        for proxy_mode in [false, true] {
            let remote = resolve("198.51.100.7", &forged, proxy_mode);
            assert_eq!(remote.client_ip, ip("198.51.100.7"));
            assert!(!remote.externally_secure);
            assert!(remote.plaintext_remote, "proxy_mode={proxy_mode}");
        }
        let proxied = resolve("127.0.0.1", &forged, true);
        assert_eq!(proxied.client_ip, ip("203.0.113.50"));
        assert!(proxied.externally_secure && !proxied.plaintext_remote);
        // Some proxies label a WebSocket upgrade `wss`, which is TLS; `ws` and `http` are not.
        for (proto, secure) in [("wss", true), ("WSS", true), ("ws", false), ("http", false)] {
            let ingress = resolve("127.0.0.1", &[("x-forwarded-proto", proto)], true);
            assert_eq!(ingress.externally_secure, secure, "{proto}");
            assert_eq!(ingress.plaintext_remote, !secure, "{proto}");
        }
        let remote = resolve("198.51.100.7", &[("x-forwarded-proto", "wss")], true);
        assert!(remote.plaintext_remote && !remote.externally_secure);
    }

    /// A proxy that names no client, or a malformed or repeated one, never keys login or
    /// rate limiting to the loopback peer.
    #[test]
    fn proxy_mode_never_keys_an_unnamed_client_to_the_loopback_peer() {
        for headers in [
            &[][..],
            &[("x-forwarded-for", "not-an-ip")],
            &[
                ("cf-connecting-ip", "not-an-ip"),
                ("x-forwarded-for", "203.0.113.5"),
            ],
            &[
                ("cf-connecting-ip", "203.0.113.5"),
                ("cf-connecting-ip", "203.0.113.6"),
            ],
        ] {
            assert_eq!(
                resolve("127.0.0.1", headers, true).client_ip,
                UNKNOWN_CLIENT
            );
        }
        assert_eq!(
            resolve(
                "127.0.0.1",
                &[("x-forwarded-for", "spoofed, 203.0.113.9")],
                true
            )
            .client_ip,
            ip("203.0.113.9"),
            "the last hop is the one the proxy appended"
        );
        assert_eq!(
            resolve(
                "127.0.0.1",
                &[
                    ("cf-connecting-ip", "203.0.113.50"),
                    ("x-forwarded-for", "10.0.0.1")
                ],
                true
            )
            .client_ip,
            ip("203.0.113.50")
        );
    }

    #[test]
    fn generic_proxy_mode_ignores_cf_connecting_ip() {
        let peer = SocketAddr::new(ip("127.0.0.1"), 4000);
        let mut headers = HeaderMap::new();
        headers.append("cf-connecting-ip", "203.0.113.50".parse().unwrap());
        assert_eq!(
            Ingress::resolve(peer, &headers, true, false).client_ip,
            UNKNOWN_CLIENT
        );
        headers.append("x-forwarded-for", "198.51.100.4".parse().unwrap());
        assert_eq!(
            Ingress::resolve(peer, &headers, true, false).client_ip,
            ip("198.51.100.4")
        );
    }

    #[test]
    fn plaintext_gate_applies_to_remote_peers_and_proxies_asserting_http() {
        let cases: &[(&str, &[(&'static str, &str)], bool, bool)] = &[
            // (peer, headers, proxy_mode, plaintext_remote)
            ("127.0.0.1", &[], false, false),
            ("127.0.0.1", &[], true, false),
            ("::1", &[], true, false),
            ("203.0.113.9", &[], false, true),
            ("203.0.113.9", &[("x-forwarded-proto", "https")], true, true),
            ("127.0.0.1", &[("x-forwarded-proto", "https")], true, false),
            ("127.0.0.1", &[("x-forwarded-proto", "HTTPS")], true, false),
            ("127.0.0.1", &[("x-forwarded-proto", "http")], true, true),
            (
                "127.0.0.1",
                &[("x-forwarded-proto", "http, https")],
                true,
                false,
            ),
            (
                "127.0.0.1",
                &[("x-forwarded-proto", "https, http")],
                true,
                true,
            ),
        ];
        for (peer, headers, proxy_mode, want) in cases {
            assert_eq!(
                resolve(peer, headers, *proxy_mode).plaintext_remote,
                *want,
                "{peer} {headers:?} proxy_mode={proxy_mode}"
            );
        }
    }

    mod router {
        use std::net::SocketAddr;

        use axum::extract::ConnectInfo;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::daemon::ApiErrorCode;
        use crate::server::test_support::{build_router_for_test, build_test_app_state_ingress};

        const TOKEN: &str = "secret-token";

        async fn probe(
            state: std::sync::Arc<crate::server::AppState>,
            peer: &str,
            headers: &[(&str, &str)],
        ) -> (StatusCode, Option<ApiErrorCode>) {
            let mut builder = Request::builder().uri("/api/sessions");
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            let mut request = builder.body(axum::body::Body::empty()).unwrap();
            request.extensions_mut().insert(ConnectInfo(
                format!("{peer}:5555").parse::<SocketAddr>().unwrap(),
            ));
            let response = build_router_for_test(state).oneshot(request).await.unwrap();
            let code = ApiErrorCode::from_headers(response.status(), response.headers(), false);
            (response.status(), code)
        }

        const TLS_REQUIRED: (StatusCode, Option<ApiErrorCode>) = (
            StatusCode::UPGRADE_REQUIRED,
            Some(ApiErrorCode::TlsRequired),
        );

        /// Remote plaintext gets 426 before the Host gate and before auth, whatever it
        /// presents; loopback and an explicit opt-out reach the later gates.
        #[tokio::test]
        async fn remote_plaintext_is_rejected_before_host_and_auth_gates() {
            let strict = || build_test_app_state_ingress(Some(TOKEN.into()), None, false, false);
            let bearer = format!("Bearer {TOKEN}");
            for headers in [
                &[][..],
                &[("host", "evil.example")],
                &[("host", "localhost"), ("authorization", bearer.as_str())],
            ] {
                assert_eq!(probe(strict(), "203.0.113.9", headers).await, TLS_REQUIRED);
            }
            let (status, code) = probe(strict(), "127.0.0.1", &[("host", "localhost")]).await;
            assert_eq!((status, code), (StatusCode::UNAUTHORIZED, None));
            let opted_out = build_test_app_state_ingress(Some(TOKEN.into()), None, false, true);
            let (status, _) = probe(opted_out, "203.0.113.9", &[("host", "localhost")]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn only_a_trusted_proxy_can_assert_https() {
            let proxy = || build_test_app_state_ingress(Some(TOKEN.into()), None, true, false);
            let https = [("host", "localhost"), ("x-forwarded-proto", "https")];
            // A remote peer's assertion is ignored, with or without proxy mode.
            assert_eq!(probe(proxy(), "203.0.113.9", &https).await, TLS_REQUIRED);
            let plain = build_test_app_state_ingress(Some(TOKEN.into()), None, false, false);
            assert_eq!(probe(plain, "203.0.113.9", &https).await, TLS_REQUIRED);
            // The loopback proxy's assertion passes the gate and lands on auth.
            let (status, _) = probe(proxy(), "127.0.0.1", &https).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            // A trusted proxy that says the client used plain HTTP is not exempt.
            let http = [("host", "localhost"), ("x-forwarded-proto", "http")];
            assert_eq!(probe(proxy(), "127.0.0.1", &http).await, TLS_REQUIRED);
        }
    }
}
