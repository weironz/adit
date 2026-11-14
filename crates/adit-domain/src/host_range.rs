//! Which address ranges count as "the user's own network".
//!
//! The acceptance probes and the credential bridge all guard themselves with a
//! check that the target host is not on the public internet: the point of the
//! check is that a stray command line cannot aim a real password at a third
//! party's machine.
//!
//! `std::net::Ipv4Addr::is_private` only covers RFC 1918 (`10/8`, `172.16/12`,
//! `192.168/16`). That is not enough for how people actually connect to their
//! own machines: a Tailscale or WireGuard tailnet lives in RFC 6598's shared
//! address space (`100.64/10`), which is routed only inside the tailnet and is
//! not reachable from the public internet, yet falls outside every `is_private`
//! range. A guard that rejects it rejects the user's own host while still
//! admitting every genuinely public address.
//!
//! So the predicate is stated in terms of what the guard actually cares about —
//! "is this address part of the public internet?" — and enumerated over the
//! reserved ranges that are not: RFC 1918, RFC 6598, RFC 3927 link-local, and
//! loopback.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// RFC 6598 shared address space, `100.64.0.0/10`.
///
/// Reserved for carrier-grade NAT and, since Tailscale adopted it, for
/// tailnet nodes. Routable only inside the tailnet.
const SHARED_ADDRESS_SPACE: (u8, u8) = (100, 64);

/// RFC 1918 private space, `172.16.0.0/12`.
const PRIVATE_172: (u8, u8) = (172, 16);

/// The mask half of a `/12` or `/10`: the top two bits of the second octet.
const TWELVE_BIT_MASK: u8 = 0xf0;
const TEN_BIT_MASK: u8 = 0xc0;

/// Whether an IPv4 address is outside the publicly routable internet.
#[must_use]
pub fn is_non_public_v4(addr: Ipv4Addr) -> bool {
    let [a, b, ..] = addr.octets();
    if a == 10 || a == 127 {
        return true;
    }
    if a == 192 && b == 168 {
        return true;
    }
    if a == PRIVATE_172.0 && b & TWELVE_BIT_MASK == PRIVATE_172.1 {
        return true;
    }
    if a == SHARED_ADDRESS_SPACE.0 && b & TEN_BIT_MASK == SHARED_ADDRESS_SPACE.1 {
        return true;
    }
    // RFC 3927 link-local, `169.254/16`.
    a == 169 && b == 254
}

/// Whether an IPv6 address is outside the publicly routable internet.
#[must_use]
pub fn is_non_public_v6(addr: Ipv6Addr) -> bool {
    let segments = addr.segments();
    if addr.is_loopback() {
        return true;
    }
    // RFC 4193 unique local addresses, `fc00::/7`.
    if segments[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    // RFC 4291 link-local, `fe80::/10`.
    if segments[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    false
}

/// Whether an address is outside the publicly routable internet.
///
/// This is the predicate the acceptance guards want: it admits the RFC 1918,
/// RFC 6598, link-local and loopback ranges, and rejects everything else.
#[must_use]
pub fn is_non_public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_non_public_v4(v4),
        IpAddr::V6(v6) => is_non_public_v6(v6),
    }
}

/// Whether a host string names an address that is outside the public internet.
///
/// A name that does not parse as an IP address is not accepted: the guards
/// exist to narrow what a command line can reach, and a hostname would widen
/// it again by resolving to anything.
#[must_use]
pub fn is_non_public_host(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok_and(is_non_public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1918_is_non_public() {
        assert!(is_non_public_v4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_non_public_v4(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_non_public_v4(Ipv4Addr::new(172, 31, 255, 254)));
        assert!(is_non_public_v4(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn shared_address_space_is_non_public() {
        // A Tailscale tailnet node.
        assert!(is_non_public_v4(Ipv4Addr::new(100, 70, 113, 193)));
        assert!(is_non_public_v4(Ipv4Addr::new(100, 64, 0, 0)));
        assert!(is_non_public_v4(Ipv4Addr::new(100, 127, 255, 255)));
    }

    #[test]
    fn the_edges_of_shared_address_space_are_public() {
        assert!(!is_non_public_v4(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_non_public_v4(Ipv4Addr::new(100, 128, 0, 0)));
    }

    #[test]
    fn the_edges_of_rfc1918_172_are_public() {
        assert!(!is_non_public_v4(Ipv4Addr::new(172, 15, 255, 255)));
        assert!(!is_non_public_v4(Ipv4Addr::new(172, 32, 0, 0)));
    }

    #[test]
    fn loopback_and_link_local_are_non_public() {
        assert!(is_non_public_v4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(is_non_public_v4(Ipv4Addr::new(169, 254, 1, 1)));
    }

    #[test]
    fn public_addresses_are_public() {
        assert!(!is_non_public_v4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_non_public_v4(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(!is_non_public_v4(Ipv4Addr::new(93, 184, 216, 34)));
    }

    #[test]
    fn host_strings_need_to_parse() {
        assert!(is_non_public_host("100.70.113.193"));
        assert!(is_non_public_host("192.168.72.10"));
        assert!(!is_non_public_host("8.8.8.8"));
        // A hostname could resolve anywhere, so it is not accepted.
        assert!(!is_non_public_host("example.com"));
        assert!(!is_non_public_host(""));
    }

    #[test]
    fn ipv6_non_public_ranges() {
        assert!(is_non_public_v6("::1".parse().unwrap()));
        assert!(is_non_public_v6("fd00::1".parse().unwrap()));
        assert!(is_non_public_v6("fe80::1".parse().unwrap()));
        assert!(!is_non_public_v6("2606:4700:4700::1111".parse().unwrap()));
    }
}
