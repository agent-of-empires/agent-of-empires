//! Where the running daemon serves plugin MCP endpoints, for launch code that has no `AppState`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use super::session_credential::McpSessionSecret;

/// The name rootless Podman (pasta) gives a container for its host.
pub const SANDBOX_HOST: &str = "host.containers.internal";

pub struct DaemonEndpoint {
    /// Base URL a host session reaches the daemon at, without a trailing slash.
    pub host_url: String,
    /// Base URL a sandboxed session reaches the daemon at, when one works and passes the gate.
    pub sandbox_url: Option<String>,
    pub secret: Arc<McpSessionSecret>,
}

impl DaemonEndpoint {
    pub fn base_url(&self, sandboxed: bool) -> Option<&str> {
        if sandboxed {
            self.sandbox_url.as_deref()
        } else {
            Some(&self.host_url)
        }
    }
}

/// The sandbox URL for a daemon bound to `addr`. `SANDBOX_HOST` cannot reach a loopback
/// listener, and the gate only admits it when the user allowed it (`allowed_hosts` is the
/// normalised list the gate enforces).
pub fn sandbox_url_for(addr: SocketAddr, allowed_hosts: &[String]) -> Option<String> {
    let reachable = !addr.ip().to_canonical().is_loopback();
    let allowed = allowed_hosts.iter().any(|h| h == SANDBOX_HOST);
    (reachable && allowed).then(|| format!("http://{SANDBOX_HOST}:{}", addr.port()))
}

static ENDPOINT: Mutex<Option<Arc<DaemonEndpoint>>> = Mutex::new(None);

pub fn set_daemon_endpoint(endpoint: Option<DaemonEndpoint>) {
    *ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()) = endpoint.map(Arc::new);
}

pub fn daemon_endpoint() -> Option<Arc<DaemonEndpoint>> {
    ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The URL a host process uses for a listener bound to `addr`; a wildcard bind is reached
/// over loopback, which the DNS-rebinding gate always allows.
pub fn host_url_for(addr: SocketAddr) -> String {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    match (ip, addr) {
        // `SocketAddr` prints a scope id as a raw `%`, which is not a valid URL.
        (IpAddr::V6(ip), SocketAddr::V6(v6)) if v6.scope_id() != 0 => {
            format!("http://[{ip}%25{}]:{}", v6.scope_id(), addr.port())
        }
        _ => format!("http://{}", SocketAddr::new(ip, addr.port())),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Records an endpoint and clears it on drop. Tests that hold one must be
    /// `#[serial_test::serial]`.
    pub(crate) struct EndpointGuard;

    impl EndpointGuard {
        pub(crate) fn install(endpoint: DaemonEndpoint) -> Self {
            set_daemon_endpoint(Some(endpoint));
            Self
        }

        /// For code under test that records the endpoint itself.
        pub(crate) fn clear_on_drop() -> Self {
            Self
        }
    }

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            set_daemon_endpoint(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_url_needs_a_non_loopback_bind_and_the_allowed_host() {
        use crate::server::access::resolve_access_policy;

        let none: &[&str] = &[];
        // (bind host, bound addr, extra allowed hosts, expected)
        let cases: [(&str, &str, &[&str], Option<&str>); 13] = [
            ("127.0.0.1", "127.0.0.1:8080", &[SANDBOX_HOST], None),
            ("::1", "[::1]:8080", &[SANDBOX_HOST], None),
            ("::", "[::ffff:127.0.0.1]:8080", &[SANDBOX_HOST], None),
            ("0.0.0.0", "0.0.0.0:8081", &[SANDBOX_HOST], Some("8081")),
            ("::", "[::]:9000", &[SANDBOX_HOST], Some("9000")),
            ("192.0.2.7", "192.0.2.7:80", &[SANDBOX_HOST], Some("80")),
            (
                "2001:db8::7",
                "[2001:db8::7]:80",
                &[SANDBOX_HOST],
                Some("80"),
            ),
            ("0.0.0.0", "0.0.0.0:8081", none, None),
            ("192.0.2.7", "192.0.2.7:80", &["example.test"], None),
            (
                "0.0.0.0",
                "0.0.0.0:8081",
                &["Host.Containers.Internal"],
                Some("8081"),
            ),
            (
                "0.0.0.0",
                "0.0.0.0:8081",
                &["host.containers.internal."],
                Some("8081"),
            ),
            (
                "0.0.0.0",
                "0.0.0.0:8081",
                &["host.containers.internal:8081"],
                Some("8081"),
            ),
            (
                "0.0.0.0",
                "0.0.0.0:8081",
                &["host.containers.internal.evil"],
                None,
            ),
        ];
        for (bind, addr, extra, want) in cases {
            let extra: Vec<String> = extra.iter().map(|h| h.to_string()).collect();
            let addr: SocketAddr = addr.parse().unwrap();
            let (hosts, _) = resolve_access_policy(bind, addr.port(), &extra, &[], None);
            let want = want.map(|port| format!("http://{SANDBOX_HOST}:{port}"));
            assert_eq!(
                sandbox_url_for(addr, &hosts),
                want,
                "{bind} {addr} {extra:?}"
            );
        }
    }

    #[test]
    fn host_url_reaches_the_bound_address_or_loopback_for_a_wildcard() {
        let cases = [
            ("127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("0.0.0.0:8081", "http://127.0.0.1:8081"),
            ("[::]:9000", "http://[::1]:9000"),
            ("[::1]:9000", "http://[::1]:9000"),
            ("192.0.2.7:80", "http://192.0.2.7:80"),
            ("[fe80::1%2]:9000", "http://[fe80::1%252]:9000"),
        ];
        for (addr, want) in cases {
            assert_eq!(host_url_for(addr.parse().unwrap()), want, "{addr}");
        }
    }
}
