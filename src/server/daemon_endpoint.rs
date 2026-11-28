//! Where the running daemon serves plugin MCP endpoints, for launch code that has no `AppState`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use super::session_credential::McpSessionSecret;

/// The name rootless Podman (pasta) gives a container for its host.
pub const SANDBOX_HOST: &str = "host.containers.internal";

pub struct DaemonEndpoint {
    /// Base URL a host session reaches the daemon at, without a trailing slash. `None` when
    /// the ingress gate would reject it: a specific non-loopback bind with plaintext remote
    /// access off.
    pub host_url: Option<String>,
    /// Base URL a sandboxed session reaches the daemon at, when one works and passes the gate.
    pub sandbox_url: Option<String>,
    /// Set while the daemon serves the plugin socket: how a session's container reaches it.
    /// Blocking.
    pub sandbox_socket: Option<fn(&str) -> SandboxSocket>,
    pub secret: Arc<McpSessionSecret>,
}

/// Whether a sandbox container can use the daemon's plugin socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxSocket {
    Reachable,
    /// Created without this daemon's socket directory mounted.
    #[default]
    Unmounted,
    /// Runs on another kernel (a VM-backed runtime), where a host unix socket does not connect.
    OtherKernel,
    /// Could not be checked: the runtime or the container did not answer. Treated as
    /// unavailable.
    Unchecked,
    /// The container's user cannot open the 0700 directory or the 0600 socket the daemon's
    /// user owns.
    NoAccess,
    /// The container image has no `node`, which the forwarder needs.
    NoNode,
}

/// How a session reaches the plugin MCP endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRoute<'a> {
    /// HTTP entries under this base URL.
    Http(&'a str),
    /// Stdio entries running the forwarder over the mounted socket.
    Socket,
}

/// Why a session gets no plugin entries: the piece it lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoute {
    /// The daemon serves no plugin endpoint.
    Endpoint,
    /// No socket on this platform and no sandbox URL.
    Platform,
    /// The socket did not start and there is no sandbox URL.
    Socket,
    /// The container lacks this daemon's socket mount and there is no sandbox URL.
    Mount,
    /// The container runs on another kernel and there is no sandbox URL.
    Kernel,
    /// The container could not be checked and there is no sandbox URL.
    Probe,
    /// The container's user cannot open the socket and there is no sandbox URL.
    Access,
    /// The container has no `node` for the forwarder and there is no sandbox URL.
    Node,
    /// A host session has no URL the daemon admits.
    Host,
}

impl NoRoute {
    /// Fixed text for the notification attributed to each skipped plugin.
    pub fn notice(self) -> &'static str {
        match self {
            Self::Endpoint => {
                "The daemon is not serving plugin MCP endpoints, so this plugin's MCP servers are skipped for this session. It serves them when a plugin holding mcp.serve is installed at daemon start and the session secret loads; restart the daemon."
            }
            Self::Platform => {
                "This sandboxed session has no route to the daemon: the plugin MCP socket exists on Linux only, and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). This plugin's MCP servers are skipped for it."
            }
            Self::Socket => {
                "This sandboxed session has no route to the daemon: the plugin MCP socket did not start (see the daemon log), and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). This plugin's MCP servers are skipped for it."
            }
            Self::Mount => {
                "This session's sandbox container was created without this daemon's plugin MCP socket mount, so this plugin's MCP servers are skipped for it. Recreate the container to get them."
            }
            Self::Kernel => {
                "This session's sandbox runs in a VM-backed container runtime, which cannot reach the daemon's plugin MCP socket, and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). This plugin's MCP servers are skipped for it."
            }
            Self::Probe => {
                "AoE could not check whether this session's sandbox container can reach the daemon's plugin MCP socket (the container runtime or the container did not answer), and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). This plugin's MCP servers are skipped for it."
            }
            Self::Access => {
                "This session's sandbox user cannot open the daemon's plugin MCP socket (the directory is private to the daemon's user), and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). Run the sandbox as the daemon's user or enable that address. This plugin's MCP servers are skipped for it."
            }
            Self::Node => {
                "This session's sandbox image has no node executable, which the plugin MCP socket forwarder needs, and the sandbox HTTP address is unavailable (it needs a wildcard bind, host.containers.internal in the allowed hosts and plaintext remote access enabled). Use an image with node or enable that address. This plugin's MCP servers are skipped for it."
            }
            Self::Host => {
                "The daemon listens on a specific non-loopback address with plaintext remote access off, so it rejects this session's plugin MCP calls. Bind to a loopback or wildcard address, or enable plaintext remote access. This plugin's MCP servers are skipped for it."
            }
        }
    }
}

/// `sandbox` is `None` for a host session, else how its container reaches the socket.
/// A sandbox prefers the socket and falls back to the opt-in sandbox URL.
pub fn plugin_route(
    endpoint: Option<&DaemonEndpoint>,
    sandbox: Option<SandboxSocket>,
) -> Result<(&McpSessionSecret, PluginRoute<'_>), NoRoute> {
    let endpoint = endpoint.ok_or(NoRoute::Endpoint)?;
    let served = endpoint.sandbox_socket.is_some();
    let route = match sandbox {
        None => PluginRoute::Http(endpoint.host_url.as_deref().ok_or(NoRoute::Host)?),
        Some(SandboxSocket::Reachable) if served => PluginRoute::Socket,
        Some(socket) => match endpoint.sandbox_url.as_deref() {
            Some(url) => PluginRoute::Http(url),
            None => {
                return Err(match socket {
                    _ if !served && cfg!(target_os = "linux") => NoRoute::Socket,
                    _ if !served => NoRoute::Platform,
                    SandboxSocket::OtherKernel => NoRoute::Kernel,
                    SandboxSocket::Unchecked => NoRoute::Probe,
                    SandboxSocket::NoAccess => NoRoute::Access,
                    SandboxSocket::NoNode => NoRoute::Node,
                    SandboxSocket::Unmounted | SandboxSocket::Reachable => NoRoute::Mount,
                })
            }
        },
    };
    Ok((&endpoint.secret, route))
}

/// The sandbox URL for a daemon bound to `addr`. `SANDBOX_HOST` resolves to the container
/// gateway, which only a wildcard listener accepts connections on, and the gate only admits
/// it when the user allowed it (`allowed_hosts` is the normalised list the gate enforces). A
/// sandbox is a non-loopback peer, so the plaintext gate rejects it before auth unless
/// `allow_plaintext_remote`.
pub fn sandbox_url_for(
    addr: SocketAddr,
    allowed_hosts: &[String],
    allow_plaintext_remote: bool,
) -> Option<String> {
    let reachable = addr.ip().to_canonical().is_unspecified();
    let allowed = allowed_hosts.iter().any(|h| h == SANDBOX_HOST);
    (reachable && allowed && allow_plaintext_remote)
        .then(|| format!("http://{SANDBOX_HOST}:{}", addr.port()))
}

static ENDPOINT: Mutex<Option<Arc<DaemonEndpoint>>> = Mutex::new(None);

pub fn set_daemon_endpoint(endpoint: Option<DaemonEndpoint>) {
    *ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()) = endpoint.map(Arc::new);
}

pub fn daemon_endpoint() -> Option<Arc<DaemonEndpoint>> {
    ENDPOINT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The URL a host process uses for a listener bound to `addr`; a wildcard bind is reached
/// over loopback, which the DNS-rebinding gate always allows. A specific non-loopback bind
/// is reached from its own address, a non-loopback peer the plaintext gate rejects unless
/// `allow_plaintext_remote`; there is no URL then.
pub fn host_url_for(addr: SocketAddr, allow_plaintext_remote: bool) -> Option<String> {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    if !ip.to_canonical().is_loopback() && !allow_plaintext_remote {
        return None;
    }
    Some(match (ip, addr) {
        // `SocketAddr` prints a scope id as a raw `%`, which is not a valid URL.
        (IpAddr::V6(ip), SocketAddr::V6(v6)) if v6.scope_id() != 0 => {
            format!("http://[{ip}%25{}]:{}", v6.scope_id(), addr.port())
        }
        _ => format!("http://{}", SocketAddr::new(ip, addr.port())),
    })
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
    fn sandbox_url_needs_a_wildcard_bind_and_the_allowed_host() {
        use crate::server::access::resolve_access_policy;

        let none: &[&str] = &[];
        // (bind host, bound addr, extra allowed hosts, expected)
        let cases: [(&str, &str, &[&str], Option<&str>); 13] = [
            ("127.0.0.1", "127.0.0.1:8080", &[SANDBOX_HOST], None),
            ("::1", "[::1]:8080", &[SANDBOX_HOST], None),
            ("::", "[::ffff:127.0.0.1]:8080", &[SANDBOX_HOST], None),
            ("0.0.0.0", "0.0.0.0:8081", &[SANDBOX_HOST], Some("8081")),
            ("::", "[::]:9000", &[SANDBOX_HOST], Some("9000")),
            // A specific interface does not accept connections on the container gateway
            // address `host.containers.internal` resolves to, so only a wildcard bind
            // gets a URL.
            ("192.0.2.7", "192.0.2.7:80", &[SANDBOX_HOST], None),
            ("2001:db8::7", "[2001:db8::7]:80", &[SANDBOX_HOST], None),
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
                sandbox_url_for(addr, &hosts, true),
                want,
                "{bind} {addr} {extra:?}"
            );
            assert_eq!(
                sandbox_url_for(addr, &hosts, false),
                None,
                "{bind} {addr} {extra:?}: the plaintext gate rejects a sandbox peer"
            );
        }
    }

    #[test]
    fn plugin_route_prefers_the_socket_then_the_sandbox_url() {
        let dir = tempfile::tempdir().unwrap();
        let secret = Arc::new(McpSessionSecret::load_or_generate(&dir.path().join("s")).unwrap());
        let endpoint = |url: Option<&str>, socket: bool| DaemonEndpoint {
            host_url: Some("http://127.0.0.1:1".into()),
            sandbox_url: url.map(str::to_string),
            sandbox_socket: socket
                .then_some((|_: &str| SandboxSocket::Reachable) as fn(&str) -> SandboxSocket),
            secret: secret.clone(),
        };
        let url = Some("http://host.containers.internal:1");
        let host = Ok(PluginRoute::Http("http://127.0.0.1:1"));
        let sandbox_url = Ok(PluginRoute::Http("http://host.containers.internal:1"));
        let no_socket = if cfg!(target_os = "linux") {
            NoRoute::Socket
        } else {
            NoRoute::Platform
        };
        use SandboxSocket::{OtherKernel, Reachable, Unchecked, Unmounted};
        // (case, endpoint, session: None = host, Some(how its container reaches the socket), want)
        type Case<'a> = (
            &'a str,
            Option<DaemonEndpoint>,
            Option<SandboxSocket>,
            Result<PluginRoute<'a>, NoRoute>,
        );
        let cases: Vec<Case> = vec![
            ("host", Some(endpoint(url, true)), None, host),
            ("host, no endpoint", None, None, Err(NoRoute::Endpoint)),
            (
                "sandbox, no endpoint",
                None,
                Some(Reachable),
                Err(NoRoute::Endpoint),
            ),
            (
                "sandbox, socket reachable",
                Some(endpoint(url, true)),
                Some(Reachable),
                Ok(PluginRoute::Socket),
            ),
            (
                "sandbox, unmounted, url",
                Some(endpoint(url, true)),
                Some(Unmounted),
                sandbox_url,
            ),
            (
                "sandbox, unmounted",
                Some(endpoint(None, true)),
                Some(Unmounted),
                Err(NoRoute::Mount),
            ),
            (
                "sandbox, other kernel, url",
                Some(endpoint(url, true)),
                Some(OtherKernel),
                sandbox_url,
            ),
            (
                "sandbox, other kernel",
                Some(endpoint(None, true)),
                Some(OtherKernel),
                Err(NoRoute::Kernel),
            ),
            (
                "sandbox, unchecked, url",
                Some(endpoint(url, true)),
                Some(Unchecked),
                sandbox_url,
            ),
            (
                "sandbox, unchecked",
                Some(endpoint(None, true)),
                Some(Unchecked),
                Err(NoRoute::Probe),
            ),
            (
                "sandbox, no socket, url",
                Some(endpoint(url, false)),
                Some(Reachable),
                sandbox_url,
            ),
            (
                "sandbox, no socket",
                Some(endpoint(None, false)),
                Some(Reachable),
                Err(no_socket),
            ),
            (
                "sandbox, user cannot open the socket, url",
                Some(endpoint(url, true)),
                Some(SandboxSocket::NoAccess),
                sandbox_url,
            ),
            (
                "sandbox, user cannot open the socket",
                Some(endpoint(None, true)),
                Some(SandboxSocket::NoAccess),
                Err(NoRoute::Access),
            ),
            (
                "sandbox, no node in the image, url",
                Some(endpoint(url, true)),
                Some(SandboxSocket::NoNode),
                sandbox_url,
            ),
            (
                "sandbox, no node in the image",
                Some(endpoint(None, true)),
                Some(SandboxSocket::NoNode),
                Err(NoRoute::Node),
            ),
            (
                "host, no admitted url",
                Some(DaemonEndpoint {
                    host_url: None,
                    ..endpoint(url, true)
                }),
                None,
                Err(NoRoute::Host),
            ),
        ];
        for (case, endpoint, sandbox, want) in cases {
            let got = plugin_route(endpoint.as_ref(), sandbox).map(|(_, route)| route);
            assert_eq!(got, want, "{case}");
        }
        let causes = [
            NoRoute::Endpoint,
            NoRoute::Platform,
            NoRoute::Socket,
            NoRoute::Mount,
            NoRoute::Kernel,
            NoRoute::Probe,
            NoRoute::Access,
            NoRoute::Node,
            NoRoute::Host,
        ];
        let notices: std::collections::BTreeSet<&str> = causes.map(NoRoute::notice).into();
        assert_eq!(notices.len(), causes.len(), "one distinct notice per cause");
        assert!(!notices.contains(""));
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
            assert_eq!(
                host_url_for(addr.parse().unwrap(), true).as_deref(),
                Some(want),
                "{addr}"
            );
        }
    }

    /// A specific non-loopback bind is reached from its own address, a remote peer to the
    /// plaintext gate, so there is no host URL unless plaintext remote access is on.
    #[test]
    fn host_url_is_withheld_where_the_plaintext_gate_would_reject_it() {
        for (addr, admitted_by_default) in [
            ("127.0.0.1:8080", true),
            ("[::1]:9000", true),
            ("0.0.0.0:8081", true),
            ("[::]:9000", true),
            ("192.0.2.7:80", false),
            ("[fe80::1%2]:9000", false),
        ] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(
                host_url_for(addr, false).is_some(),
                admitted_by_default,
                "{addr}"
            );
            assert!(host_url_for(addr, true).is_some(), "{addr}");
        }
    }
}
