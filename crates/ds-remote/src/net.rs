//! Which peers may connect at all: loopback, private LAN, link-local and
//! tailnet addresses. Everything else is dropped before the TLS handshake.
//!
//! The server listens on every interface (binding to the Tailscale interface
//! alone breaks whenever Tailscale reconnects), so this filter plus pinned
//! keys is what keeps it off the internet.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// Decides whether a peer address may connect. The server's seam: tests
/// substitute one that sees loopback as a public address.
pub type PeerPolicy = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

/// The default [`PeerPolicy`].
pub fn private_peers() -> PeerPolicy {
    Arc::new(|peer: SocketAddr| allowed_ip(peer.ip()))
}

/// Loopback, RFC 1918, link-local, the Tailscale CGNAT range
/// `100.64.0.0/10`, IPv6 unique-local `fc00::/7` (which holds Tailscale's
/// `fd7a:115c:a1e0::/48`) and IPv6 link-local. IPv4-mapped IPv6 addresses
/// are judged as IPv4.
pub fn allowed_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => allowed_v4(v4),
        IpAddr::V6(v6) => allowed_v6(v6),
    }
}

/// One peer, for the per-address limits: an IPv4 address, or an IPv6 /64
/// (a host picks any address in its /64, so counting addresses there would
/// count nothing). IPv4-mapped IPv6 addresses count as IPv4.
pub fn prefix(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !u128::from(u64::MAX))),
        v4 @ IpAddr::V4(_) => v4,
    }
}

fn allowed_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    // Tailscale's range, which ISPs also use for carrier-grade NAT; keys are
    // pinned either way, so this only narrows who can reach the handshake.
    let tailnet = a == 100 && (64..128).contains(&b);
    ip.is_loopback() || ip.is_private() || ip.is_link_local() || tailnet
}

fn allowed_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    let unique_local = first & 0xfe00 == 0xfc00;
    let link_local = first & 0xffc0 == 0xfe80;
    ip.is_loopback() || unique_local || link_local
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> bool {
        allowed_ip(s.parse().unwrap())
    }

    #[test]
    fn private_and_tailnet_ranges_are_allowed() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.20",
            "169.254.10.10",
            "100.64.0.1",
            "100.127.255.254",
            "::1",
            "fd7a:115c:a1e0::1234",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.20",
        ] {
            assert!(ok(a), "{a} should be allowed");
        }
    }

    #[test]
    fn public_addresses_are_refused() {
        for a in [
            "8.8.8.8",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "192.169.0.1",
            "0.0.0.0",
            "2001:4860:4860::8888",
            "fec0::1",
            "::",
            "::ffff:8.8.8.8",
        ] {
            assert!(!ok(a), "{a} should be refused");
        }
        let p = private_peers();
        assert!(p("127.0.0.1:9".parse().unwrap()));
        assert!(!p("1.1.1.1:9".parse().unwrap()));
    }

    #[test]
    fn a_v6_slash_64_is_one_peer_and_v4_addresses_are_each_one() {
        let p = |s: &str| prefix(s.parse().unwrap());
        assert_eq!(p("fe80::1"), p("fe80::dead:beef:1:2"));
        assert_eq!(p("fd00:1:2:3:4::5"), p("fd00:1:2:3::")); // one /64
        assert_ne!(p("fd00:1:2:3::1"), p("fd00:1:2:4::1")); // next /64
        assert_eq!(p("fd00:1:2:3:ffff::1").to_string(), "fd00:1:2:3::");
        assert_ne!(p("192.168.1.20"), p("192.168.1.21"));
        assert_eq!(p("::ffff:192.168.1.20"), p("192.168.1.20"));
    }
}
