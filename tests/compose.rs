use std::net::{IpAddr, Ipv4Addr};

use mediaqueue::clientip::{parse_cidr, parse_cidr_list};

const COMPOSE: &str = include_str!("../compose.yaml");
const DEFAULT_TRUSTED: &str = "127.0.0.1/32,::1/128,172.16.0.0/12";

/// The lines of one service block, up to the next top-level entry.
fn service_block(name: &str) -> String {
    let head = format!("  {name}:");
    COMPOSE
        .lines()
        .skip_while(|line| *line != head)
        .skip(1)
        .take_while(|line| {
            line.is_empty() || line.starts_with("    ") || line.trim_start().starts_with('#')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn workernet_subnet_is_inside_the_trusted_proxy_range() {
    let subnet = COMPOSE
        .lines()
        .find_map(|line| line.trim().strip_prefix("- subnet:"))
        .expect("subnet declared")
        .split('#')
        .next()
        .expect("split yields one item")
        .trim();
    let net = parse_cidr(subnet).expect("subnet parses");
    let trusted = parse_cidr_list(DEFAULT_TRUSTED).expect("default list parses");
    let (addr, len) = subnet.split_once('/').expect("has a prefix");
    let len: u32 = len.parse().expect("prefix is a number");
    let base: Ipv4Addr = addr.parse().expect("ipv4 subnet");
    let first = u32::from(base);
    let last = first | (u32::MAX >> len);
    let gateway = IpAddr::V4(Ipv4Addr::from(first + 1));
    for ip in [
        gateway,
        IpAddr::V4(Ipv4Addr::from(first)),
        IpAddr::V4(Ipv4Addr::from(last)),
    ] {
        assert!(net.contains(ip), "{ip} not in {subnet}");
        assert!(
            trusted.iter().any(|cidr| cidr.contains(ip)),
            "{ip} is outside the default TRUSTED_PROXY_CIDRS"
        );
    }
}

#[test]
fn both_services_join_workernet() {
    assert!(service_block("portal").contains("workernet"));
    assert!(service_block("worker").contains("workernet"));
}

#[test]
fn worker_has_no_env_file_and_no_ports() {
    let worker = service_block("worker");
    let code: String = worker
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.contains("env_file"), "worker must not read .env");
    assert!(!code.contains("ports:"), "worker must not publish ports");
}
