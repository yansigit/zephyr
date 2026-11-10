//! The network floor (DESIGN §4.19): the agent's uid has no route out but the proxy here, which
//! connects only to addresses it has checked.

use anyhow::Context;
use futures_util::future::BoxFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use url::Host;

/// The proxy's port on the container's loopback.
pub const PORT: u16 = 7792;
const LOOKUP_WAIT: Duration = Duration::from_secs(5);

/// Whether this container runs under the floor: `zephyr up` sets `ZEPHYR_NETWORK=bridge`.
pub fn bridge() -> bool {
    std::env::var("ZEPHYR_NETWORK").is_ok_and(|v| v == "bridge")
}

/// The container's own address: the `zephyr` line Docker writes to /etc/hosts.
pub fn own_ip() -> anyhow::Result<Ipv4Addr> {
    use std::net::ToSocketAddrs;
    let mut addrs = ("zephyr", 0)
        .to_socket_addrs()
        .context("finding this container's address (zephyr in /etc/hosts)")?;
    match addrs.next().map(|a| a.ip()) {
        Some(IpAddr::V4(ip)) if !ip.is_loopback() => Ok(ip),
        other => anyhow::bail!(
            "this container's address (zephyr in /etc/hosts) is {other:?}, not an IPv4 address of its own"
        ),
    }
}

/// IPv4 blocks that are not the public internet, from the IANA IPv4 Special-Purpose Address
/// Registry and the multicast and reserved blocks.
const V4_SPECIAL: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),       // "This network", RFC 791
    ([10, 0, 0, 0], 8),      // Private-Use, RFC 1918
    ([100, 64, 0, 0], 10),   // Shared Address Space (CGNAT, tailnets), RFC 6598
    ([127, 0, 0, 0], 8),     // Loopback, RFC 1122
    ([169, 254, 0, 0], 16),  // Link Local (cloud metadata), RFC 3927
    ([172, 16, 0, 0], 12),   // Private-Use, RFC 1918
    ([192, 0, 0, 0], 24),    // IETF Protocol Assignments, RFC 6890
    ([192, 0, 2, 0], 24),    // Documentation (TEST-NET-1), RFC 5737
    ([192, 88, 99, 0], 24),  // 6to4 Relay Anycast, deprecated, RFC 7526
    ([192, 168, 0, 0], 16),  // Private-Use, RFC 1918
    ([198, 18, 0, 0], 15),   // Benchmarking, RFC 2544
    ([198, 51, 100, 0], 24), // Documentation (TEST-NET-2), RFC 5737
    ([203, 0, 113, 0], 24),  // Documentation (TEST-NET-3), RFC 5737
    ([224, 0, 0, 0], 4),     // Multicast, RFC 5771
    ([240, 0, 0, 0], 4),     // Reserved, RFC 1112, and Limited Broadcast, RFC 919
];

/// The only IPv6 block with public addresses: Global Unicast, RFC 4291.
const V6_GLOBAL: (Ipv6Addr, u32) = (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);

/// Blocks inside it that are not, from the IANA IPv6 Special-Purpose Address Registry.
const V6_SPECIAL: &[(Ipv6Addr, u32)] = &[
    // IETF Protocol Assignments, Teredo (2001::/32) among them, RFC 2928
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    // Documentation, RFC 3849
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    // 6to4, RFC 3056
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    // Documentation, RFC 9637
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
];

/// Private blocks a `network.allow` entry may reach: RFC 1918 and RFC 6598.
const V4_PRIVATE: &[([u8; 4], u32)] = &[
    ([10, 0, 0, 0], 8),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
    ([100, 64, 0, 0], 10),
];
/// Unique Local, RFC 4193.
const V6_PRIVATE: (Ipv6Addr, u32) = (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7);

fn in4(ip: Ipv4Addr, blocks: &[([u8; 4], u32)]) -> bool {
    let ip = u32::from(ip);
    (blocks.iter()).any(|(base, len)| (ip ^ u32::from_be_bytes(*base)) >> (32 - len) == 0)
}

fn in6(ip: Ipv6Addr, (base, len): (Ipv6Addr, u32)) -> bool {
    (u128::from(ip) ^ u128::from(base)) >> (128 - len) == 0
}

/// Whether `ip` is on the public internet. IPv4 is public unless listed as special. IPv6 is
/// public only when listed as global, so every form carrying an IPv4 address (mapped, compatible,
/// NAT64) is refused with no unwrapping step to get wrong.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !in4(ip, V4_SPECIAL),
        IpAddr::V6(ip) => in6(ip, V6_GLOBAL) && !V6_SPECIAL.iter().any(|b| in6(ip, *b)),
    }
}

/// Whether `ip` is a private address a `network.allow` entry may reach.
pub fn private_ok(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => in4(ip, V4_PRIVATE),
        IpAddr::V6(ip) => in6(ip, V6_PRIVATE),
    }
}

/// One `network.allow` entry: exactly this host on exactly this port.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub host: Host<String>,
    pub port: u16,
}

/// `network.allow` as entries: `HOST:PORT`, comma separated. No wildcard, no scheme, and no
/// address that is never allowed.
pub fn parse_allow(raw: &str) -> Result<Vec<Entry>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',').map(|e| entry(e.trim())).collect()
}

fn entry(e: &str) -> Result<Entry, String> {
    let shape = || format!("{e:?}: use HOST:PORT, with no wildcard and no scheme");
    if e.contains(['*', '/']) {
        return Err(shape());
    }
    let (host, port) = target(e).map_err(|_| shape())?;
    let ip = match &host {
        Host::Ipv4(ip) => Some(IpAddr::V4(*ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(*ip)),
        Host::Domain(_) => None,
    };
    match ip.filter(|ip| !public(*ip) && !private_ok(*ip)) {
        Some(ip) => Err(format!("{e}: {ip} is never allowed")),
        None => Ok(Entry { host, port }),
    }
}

/// Why a request is not carried, and the status it gets.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// 400: not a request the proxy takes.
    Bad(String),
    /// 403: the policy refuses the destination.
    Denied(String),
    /// 502: the destination could not be found or reached.
    Unreachable(String),
}

/// The host and port an authority (`host:port`) names. The host is read as a browser reads it
/// (WHATWG, through `url`), so `2130706433`, `0x7f.1` and `127.1` are all 127.0.0.1.
pub fn target(authority: &str) -> Result<(Host<String>, u16), Refusal> {
    let bad = || Refusal::Bad(format!("{authority:?} is not HOST:PORT"));
    // `url` drops tabs and newlines; an authority holding one is not taken at all.
    if authority.bytes().any(|b| b <= b' ') {
        return Err(bad());
    }
    // The port comes from the text: `Url::port` drops a scheme's default one.
    let (_, port) = authority.rsplit_once(':').ok_or_else(bad)?;
    let digits = !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit());
    let port: u16 = (port.parse().ok())
        .filter(|p| digits && *p != 0)
        .ok_or_else(bad)?;
    let url = url::Url::parse(&format!("http://{authority}")).map_err(|_| bad())?;
    let plain = url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(port);
    match url.host() {
        Some(host) if plain => Ok((host.to_owned(), port)),
        _ => Err(bad()),
    }
}

/// Resolves a name to addresses. The proxy's is `system_lookup`; tests pass their own.
pub type Lookup = fn(String, u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>;

/// The system resolver, as root: Docker's, then the host's.
pub fn system_lookup(
    host: String,
    port: u16,
) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
    Box::pin(async move { Ok(tokio::net::lookup_host((host, port)).await?.collect()) })
}

/// The addresses a request to `host:port` may connect to, or why it may not. The port rule runs
/// before any lookup, and every address is checked before any is returned: one refused address
/// refuses the request, so an answer mixing public and private addresses gets nowhere.
pub async fn decide(
    host: &Host<String>,
    port: u16,
    allow: &[Entry],
    own: IpAddr,
    public: fn(IpAddr) -> bool,
    lookup: Lookup,
) -> Result<Vec<SocketAddr>, Refusal> {
    let listed = allow.iter().any(|e| e.port == port && e.host == *host);
    if !listed && port != 80 && port != 443 {
        return Err(Refusal::Denied(format!("port {port} needs network.allow")));
    }
    let ips: Vec<IpAddr> = match host {
        Host::Ipv4(ip) => vec![IpAddr::V4(*ip)],
        Host::Ipv6(ip) => vec![IpAddr::V6(*ip)],
        Host::Domain(name) => {
            let found = tokio::time::timeout(LOOKUP_WAIT, lookup(name.clone(), port)).await;
            match found {
                Ok(Ok(addrs)) if !addrs.is_empty() => addrs.iter().map(SocketAddr::ip).collect(),
                _ => return Err(Refusal::Unreachable(format!("{name} has no address"))),
            }
        }
    };
    for &ip in &ips {
        if ip != own && (public(ip) || listed && private_ok(ip)) {
            continue;
        }
        return Err(Refusal::Denied(match (listed, ip.is_loopback()) {
            (false, _) => format!("private address {ip}"),
            (true, true) => "loopback is never allowed".into(),
            (true, false) => format!("{ip} is never allowed"),
        }));
    }
    // The port is the request's, whatever the lookup answered.
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn names(host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
        let ips: &[&str] = match host.as_str() {
            "example.com" | "imap.gmail.com" => &["1.1.1.1"],
            "homeassistant.lan" => &["192.168.1.10"],
            "mixed.test" => &["1.1.1.1", "10.0.0.5"],
            "meta.lan" => &["169.254.169.254"],
            "loop.lan" => &["127.0.0.1"],
            "self.lan" => &["172.18.0.2"],
            _ => &[],
        };
        let addrs = ips.iter().map(|i| SocketAddr::new(ip(i), port)).collect();
        Box::pin(async move { Ok(addrs) })
    }

    const OWN: &str = "172.18.0.2";

    async fn decided(authority: &str, allow: &str) -> Result<Vec<SocketAddr>, Refusal> {
        let (host, port) = target(authority)?;
        let allow = parse_allow(allow).unwrap();
        decide(&host, port, &allow, ip(OWN), public, names).await
    }

    fn denied(reason: &str) -> Result<Vec<SocketAddr>, Refusal> {
        Err(Refusal::Denied(reason.into()))
    }

    #[test]
    fn public_refuses_every_special_range_and_embedded_ipv4() {
        for denied in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "100.100.100.100",
            "169.254.169.254",
            "172.17.0.1",
            "192.168.1.1",
            "192.0.0.170",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:7f00:1",
            "::ffff:127.0.0.1",
            "::7f00:1",
            "::127.0.0.1",
            "fea0::1",
            "fe80::1",
            "fd00:ec2::254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:7f00:1::",
            "2001:0:4136:e378::",
            "2001:db8::1",
            "3fff::1",
            "ff02::1",
            "4000::1",
        ] {
            assert!(!public(ip(denied)), "{denied} counted as public");
        }
        for open in [
            "1.1.1.1",
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "2606:4700::1111",
            "2001:200::1",
        ] {
            assert!(public(ip(open)), "{open} counted as special");
        }
    }

    #[test]
    fn targets_normalize_numeric_hosts_through_url() {
        let loopback = Host::Ipv4(Ipv4Addr::LOCALHOST);
        for numeric in [
            "2130706433:443",
            "0x7f.1:443",
            "0177.0.0.1:443",
            "127.1:443",
        ] {
            assert_eq!(target(numeric), Ok((loopback.clone(), 443)), "{numeric}");
        }
        assert_eq!(
            target("[::ffff:127.0.0.1]:443"),
            Ok((Host::Ipv6("::ffff:7f00:1".parse().unwrap()), 443))
        );
        assert_eq!(
            target("example.com:80"),
            Ok((Host::Domain("example.com".into()), 80))
        );
        assert_eq!(
            target("EXAMPLE.com:8443"),
            Ok((Host::Domain("example.com".into()), 8443))
        );
        for bad in [
            "example.com",
            "user@example.com:443",
            "example.com:0",
            "example.com:65536",
            "example.com:+443",
            "example.com:443/path",
            "example.com:443?q",
            "exam\tple.com:443",
            "[::1]",
            ":443",
            "",
        ] {
            assert!(matches!(target(bad), Err(Refusal::Bad(_))), "{bad:?}");
        }
    }

    #[test]
    fn parse_allow_takes_exact_host_ports_only() {
        let three = parse_allow("imap.gmail.com:993, github.com:22,homeassistant.lan:8123");
        assert_eq!(
            three.unwrap(),
            [
                ("imap.gmail.com", 993),
                ("github.com", 22),
                ("homeassistant.lan", 8123)
            ]
            .map(|(h, port)| Entry {
                host: Host::Domain(h.into()),
                port
            })
        );
        assert_eq!(parse_allow(""), Ok(vec![]));
        assert_eq!(parse_allow("10.0.0.5:5432").unwrap().len(), 1);
        for bad in [
            "*.x:443",
            "x",
            "x:0",
            "x:65536",
            "http://x:1",
            "127.0.0.1:22",
            "[::1]:22",
            "169.254.169.254:80",
            "x:1,",
        ] {
            assert!(parse_allow(bad).is_err(), "{bad} was taken");
        }
    }

    #[tokio::test]
    async fn decide_applies_the_port_rule_and_the_allow_list() {
        let at = |a: &str| Ok(vec![a.parse::<SocketAddr>().unwrap()]);
        assert_eq!(
            decided("example.com:22", "").await,
            denied("port 22 needs network.allow")
        );
        assert_eq!(
            decided("example.com:22", "example.com:22").await,
            at("1.1.1.1:22")
        );
        assert_eq!(decided("1.1.1.1:443", "").await, at("1.1.1.1:443"));
        assert_eq!(decided("example.com:80", "").await, at("1.1.1.1:80"));
        assert_eq!(
            decided("10.0.0.5:443", "").await,
            denied("private address 10.0.0.5")
        );
        let ha = "homeassistant.lan:8123";
        assert_eq!(decided(ha, ha).await, at("192.168.1.10:8123"));
        assert_eq!(
            decided("homeassistant.lan:443", ha).await,
            denied("private address 192.168.1.10")
        );
        assert_eq!(
            decided("mixed.test:443", "").await,
            denied("private address 10.0.0.5")
        );
        assert!(matches!(
            decided("nowhere.test:443", "").await,
            Err(Refusal::Unreachable(_))
        ));
        assert_eq!(
            decided("IMAP.GMAIL.COM:993", "imap.gmail.com:993").await,
            at("1.1.1.1:993")
        );
    }

    #[tokio::test]
    async fn a_listed_name_never_reaches_loopback_link_local_or_this_container() {
        assert_eq!(
            decided("meta.lan:80", "meta.lan:80").await,
            denied("169.254.169.254 is never allowed")
        );
        assert_eq!(
            decided("loop.lan:8080", "loop.lan:8080").await,
            denied("loopback is never allowed")
        );
        assert_eq!(
            decided("self.lan:7790", "self.lan:7790").await,
            denied("172.18.0.2 is never allowed")
        );
    }
}
