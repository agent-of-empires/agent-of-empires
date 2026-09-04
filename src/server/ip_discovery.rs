//! Classifying and discovering the local addresses the dashboard can be
//! reached on.

/// Kind tag for a local IPv4 address. Ordering in this enum is also the
/// preference order for picking the "primary" URL to show in a QR: when
/// the user serves on a Tailnet, that's almost always the one they want
/// a phone (on cellular) to scan, not the LAN IP behind their NAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IpKind {
    Tailscale,
    Lan,
    Loopback,
}

impl IpKind {
    pub fn label(self) -> &'static str {
        match self {
            IpKind::Tailscale => "tailscale",
            IpKind::Lan => "lan",
            IpKind::Loopback => "localhost",
        }
    }
}

/// Classify a v4 address into Tailscale (CGNAT 100.64.0.0/10, which is
/// what Tailscale hands out), regular LAN (RFC1918), or loopback.
/// Public non-RFC1918 / non-CGNAT addresses are rare on an `aoe serve`
/// host (would mean serving directly on the open internet) and fall
/// through to `Lan` so we still surface them.
pub fn classify_ip(ip: std::net::Ipv4Addr) -> IpKind {
    let octets = ip.octets();
    if ip.is_loopback() {
        return IpKind::Loopback;
    }
    // CGNAT 100.64.0.0/10 (RFC 6598). Second octet is 64..=127.
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return IpKind::Tailscale;
    }
    IpKind::Lan
}

/// Discover non-loopback IPv4 addresses on all network interfaces,
/// tagged by kind and sorted so the preferred URL (Tailscale > LAN)
/// is first. Caller decides whether to include loopback.
pub fn discover_tagged_ips() -> Vec<(IpKind, std::net::Ipv4Addr)> {
    let mut out: Vec<(IpKind, std::net::Ipv4Addr)> = Vec::new();
    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        for ifaddr in addrs {
            if let Some(addr) = ifaddr.address {
                if let Some(sockaddr) = addr.as_sockaddr_in() {
                    let ip = sockaddr.ip();
                    if ip.is_loopback() {
                        continue;
                    }
                    if !out.iter().any(|(_, existing)| *existing == ip) {
                        out.push((classify_ip(ip), ip));
                    }
                }
            }
        }
    }
    out.sort_by_key(|(k, _)| *k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ip_recognizes_tailscale_cgnat() {
        use std::net::Ipv4Addr;
        // CGNAT range 100.64.0.0/10 = second octet 64..=127.
        assert_eq!(classify_ip(Ipv4Addr::new(100, 64, 0, 1)), IpKind::Tailscale);
        assert_eq!(
            classify_ip(Ipv4Addr::new(100, 100, 50, 50)),
            IpKind::Tailscale
        );
        assert_eq!(
            classify_ip(Ipv4Addr::new(100, 127, 255, 254)),
            IpKind::Tailscale
        );
        // Boundary: 100.63.x.x is NOT CGNAT, it's just regular public
        // space — classify as LAN so we still surface it (rare but
        // possible on a weird home network).
        assert_eq!(classify_ip(Ipv4Addr::new(100, 63, 0, 1)), IpKind::Lan);
        // Boundary: 100.128.x.x is also not CGNAT.
        assert_eq!(classify_ip(Ipv4Addr::new(100, 128, 0, 1)), IpKind::Lan);
    }

    #[test]
    fn classify_ip_recognizes_rfc1918_lan() {
        use std::net::Ipv4Addr;
        assert_eq!(classify_ip(Ipv4Addr::new(192, 168, 1, 42)), IpKind::Lan);
        assert_eq!(classify_ip(Ipv4Addr::new(10, 0, 0, 1)), IpKind::Lan);
        assert_eq!(classify_ip(Ipv4Addr::new(172, 16, 5, 10)), IpKind::Lan);
    }

    #[test]
    fn classify_ip_recognizes_loopback() {
        use std::net::Ipv4Addr;
        assert_eq!(classify_ip(Ipv4Addr::new(127, 0, 0, 1)), IpKind::Loopback);
        assert_eq!(classify_ip(Ipv4Addr::new(127, 1, 2, 3)), IpKind::Loopback);
    }

    #[test]
    fn ip_kind_ordering_prefers_tailscale() {
        // This is the "Tailscale first in QR" contract. If the sort order
        // ever flips, the user's phone would scan a LAN IP from cellular
        // and hit a timeout — regression test locks it in.
        let mut v = [IpKind::Loopback, IpKind::Lan, IpKind::Tailscale];
        v.sort();
        assert_eq!(v, [IpKind::Tailscale, IpKind::Lan, IpKind::Loopback]);
    }
}
