//! Classifying and discovering the local addresses the dashboard can be reached on.

/// Kind tag for a local IPv4 address.
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

/// Classify a v4 address into Tailscale (CGNAT 100.64.0.0/10, which is what Tailscale hands
/// out), regular LAN (RFC1918), or loopback.
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

/// Discover non-loopback IPv4 addresses on all network interfaces, tagged by kind and
/// sorted so the preferred URL (Tailscale > LAN) is first.
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
    fn classify_ip_splits_cgnat_lan_and_loopback() {
        use std::net::Ipv4Addr;
        // The Tailscale range is CGNAT, 100.64.0.0/10, so 100.63 and 100.128 are not it.
        let cases = [
            ([100, 64, 0, 1], IpKind::Tailscale),
            ([100, 100, 50, 50], IpKind::Tailscale),
            ([100, 127, 255, 254], IpKind::Tailscale),
            ([100, 63, 0, 1], IpKind::Lan),
            ([100, 128, 0, 1], IpKind::Lan),
            ([192, 168, 1, 42], IpKind::Lan),
            ([10, 0, 0, 1], IpKind::Lan),
            ([172, 16, 5, 10], IpKind::Lan),
            ([127, 0, 0, 1], IpKind::Loopback),
            ([127, 1, 2, 3], IpKind::Loopback),
        ];
        for ([a, b, c, d], want) in cases {
            assert_eq!(
                classify_ip(Ipv4Addr::new(a, b, c, d)),
                want,
                "{a}.{b}.{c}.{d}"
            );
        }
    }

    #[test]
    fn ip_kind_ordering_prefers_tailscale() {
        // This is the "Tailscale first in QR" contract.
        let mut v = [IpKind::Loopback, IpKind::Lan, IpKind::Tailscale];
        v.sort();
        assert_eq!(v, [IpKind::Tailscale, IpKind::Lan, IpKind::Loopback]);
    }
}
