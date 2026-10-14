//! Resolves the client IP for a request: the TCP peer address, or the
//! `X-Real-IP` header's value if (and only if) that peer is a trusted
//! reverse proxy. `X-Forwarded-For` is never consulted — it's trivially
//! spoofable by anyone who can reach the app directly.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, request::Parts},
};

use crate::state::AppState;

/// A parsed CIDR block: a network address plus prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix_len: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => prefix_matches(
                net.to_bits().into(),
                addr.to_bits().into(),
                self.prefix_len,
                32,
            ),
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                prefix_matches(net.to_bits(), addr.to_bits(), self.prefix_len, 128)
            }
            // An IPv4 CIDR never matches an IPv6 peer and vice versa; we
            // don't attempt v4-mapped-v6 normalisation since axum/hyper
            // never hands us those forms from a real TCP peer.
            _ => false,
        }
    }
}

fn prefix_matches(net_bits: u128, addr_bits: u128, prefix_len: u8, total_bits: u8) -> bool {
    if prefix_len >= total_bits {
        return net_bits == addr_bits;
    }
    if prefix_len == 0 {
        return true;
    }
    let shift = total_bits - prefix_len;
    (net_bits >> shift) == (addr_bits >> shift)
}

/// Parses a single `a.b.c.d/nn` or `xxxx::/nn` CIDR string.
pub fn parse_cidr(s: &str) -> Result<Cidr, String> {
    let (addr_part, len_part) = s
        .split_once('/')
        .ok_or_else(|| format!("{s:?} is not in CIDR notation (missing /prefix)"))?;
    let network: IpAddr = addr_part
        .parse()
        .map_err(|_| format!("{addr_part:?} is not a valid IP address"))?;
    let prefix_len: u8 = len_part
        .parse()
        .map_err(|_| format!("{len_part:?} is not a valid prefix length"))?;
    let max_len = match network {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix_len > max_len {
        return Err(format!("prefix length {prefix_len} exceeds {max_len}"));
    }
    Ok(Cidr {
        network,
        prefix_len,
    })
}

/// Parses a comma-separated list of CIDR blocks.
pub fn parse_cidr_list(s: &str) -> Result<Vec<Cidr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(parse_cidr)
        .collect()
}

fn is_trusted(peer: IpAddr, trusted: &[Cidr]) -> bool {
    trusted.iter().any(|cidr| cidr.contains(peer))
}

/// Extracts the real client IP for a header map and TCP peer address,
/// honouring `X-Real-IP` only when the peer is a trusted proxy.
pub fn resolve_client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[Cidr]) -> IpAddr {
    if !is_trusted(peer, trusted) {
        return peer;
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

/// Normalises an IPv6 address to its /64 for rate-limit bucketing, per the
/// spec ("IPv6 clients are keyed by /64"). IPv4 addresses pass through.
pub fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let bits = v6.to_bits() & (u128::MAX << 64);
            IpAddr::V6(Ipv6Addr::from_bits(bits))
        }
    }
}

/// Extractor yielding the resolved client IP for the current request.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        Ok(ClientIp(resolve_client_ip(
            peer,
            &parts.headers,
            &state.config.trusted_proxy_cidrs,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidrs(list: &[&str]) -> Vec<Cidr> {
        list.iter().map(|s| parse_cidr(s).unwrap()).collect()
    }

    fn headers_with_real_ip(ip: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", ip.parse().unwrap());
        h
    }

    #[test]
    fn trusted_peer_honours_x_real_ip() {
        let trusted = cidrs(&["127.0.0.1/32"]);
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let headers = headers_with_real_ip("203.0.113.9");
        assert_eq!(
            resolve_client_ip(peer, &headers, &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn untrusted_peer_ignores_x_real_ip() {
        let trusted = cidrs(&["10.0.0.0/8"]); // does not cover 127.0.0.1
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let headers = headers_with_real_ip("203.0.113.9");
        assert_eq!(resolve_client_ip(peer, &headers, &trusted), peer);
    }

    #[test]
    fn missing_real_ip_header_falls_back_to_peer() {
        let trusted = cidrs(&["127.0.0.1/32"]);
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let headers = HeaderMap::new();
        assert_eq!(resolve_client_ip(peer, &headers, &trusted), peer);
    }

    #[test]
    fn x_forwarded_for_is_never_consulted() {
        let trusted = cidrs(&["127.0.0.1/32"]);
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(resolve_client_ip(peer, &headers, &trusted), peer);
    }

    #[test]
    fn ipv4_cidr_boundary() {
        let cidr = parse_cidr("192.168.1.0/24").unwrap();
        assert!(cidr.contains("192.168.1.255".parse().unwrap()));
        assert!(!cidr.contains("192.168.2.0".parse().unwrap()));
    }

    #[test]
    fn ipv6_cidr_boundary() {
        let cidr = parse_cidr("2001:db8::/32").unwrap();
        assert!(cidr.contains("2001:db8:ffff::1".parse().unwrap()));
        assert!(!cidr.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn docker_bridge_range_is_trusted_by_convention() {
        let trusted = cidrs(&["172.16.0.0/12"]);
        assert!(trusted[0].contains("172.17.0.5".parse().unwrap()));
        assert!(trusted[0].contains("172.31.255.255".parse().unwrap()));
        assert!(!trusted[0].contains("172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn rejects_garbage_cidr() {
        assert!(parse_cidr("not-a-cidr").is_err());
        assert!(parse_cidr("1.2.3.4/99").is_err());
        assert!(parse_cidr("1.2.3.4").is_err());
    }

    #[test]
    fn ipv6_addresses_in_same_64_share_bucket() {
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff:ffff:ffff:ffff".parse().unwrap();
        assert_eq!(bucket_key(a), bucket_key(b));
    }

    #[test]
    fn ipv4_bucket_key_is_unchanged() {
        let a: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(bucket_key(a), a);
    }
}
