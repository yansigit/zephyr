//! The network floor (DESIGN §4.20): the agent's uid has no route out but the proxy here, which
//! connects only to addresses it has checked.

use anyhow::Context;
use futures_util::future::BoxFuture;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, HOST};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use url::Host;

use crate::config::Config;
use crate::store::Store;

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
    let addrs = ("zephyr", 0)
        .to_socket_addrs()
        .context("finding this container's address (zephyr in /etc/hosts)")?;
    own_of(addrs.map(|a| a.ip()))
        .context("this container has no IPv4 address of its own (zephyr in /etc/hosts)")
}

/// The first IPv4 address among `addrs`, whatever other families the name also has; `None` when
/// there is none or it is loopback.
fn own_of(addrs: impl Iterator<Item = IpAddr>) -> Option<Ipv4Addr> {
    let first = addrs.into_iter().find_map(|ip| match ip {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    })?;
    (!first.is_loopback()).then_some(first)
}

/// Where a daemon listener binds, and where clients inside the container connect: under the
/// floor the container's own address with `addr`'s port, which the agent's uid cannot reach,
/// otherwise `addr` as it is. Nothing of root's listens on the container's loopback but the
/// proxy (doctor checks it).
pub fn listen_addr(addr: SocketAddr) -> anyhow::Result<SocketAddr> {
    match bridge() {
        true => Ok(SocketAddr::new(own_ip()?.into(), addr.port())),
        false => Ok(addr),
    }
}

/// Every daemon TCP listener `zephyr up` publishes on the host's loopback: the zephyr.toml key
/// that sets its port, and the port a config has there. A new listener is one row here.
type Listener = (&'static str, fn(&Config) -> Option<u16>);
const LISTENERS: &[Listener] = &[
    ("attach.port", |c| c.attach.as_ref().map(|a| a.port)),
    ("imessage.listen", |c| {
        c.imessage.as_ref().map(|i| i.listen.port())
    }),
    // The web app (DESIGN §4.3.3): `tailscale serve` on the host forwards to this one.
    ("web.port", |c| c.web.as_ref().map(|w| w.port)),
];

/// `[web] port` when the table names only its host.
const WEB_PORT: &str = "7793";

/// The ports `cfg` has the daemon listen on, by key.
pub fn published(cfg: &Config) -> Vec<(&'static str, u16)> {
    let port = |(key, port): &Listener| port(cfg).map(|p| (*key, p));
    LISTENERS.iter().filter_map(port).collect()
}

/// The same from zephyr.toml's own text, where `get` reads a key's value: a port, or an address
/// with one. The host reads the file this way, so a key this build does not know cannot stop it.
pub fn published_from(get: impl Fn(&str) -> Option<String>) -> Vec<(&'static str, u16)> {
    let port = |(key, _): &Listener| {
        // The one port with a default: `[web]` listens once it has a host.
        let default = (*key == "web.port" && get("web.host").is_some()).then(|| WEB_PORT.into());
        let value = get(key).or(default)?;
        let port = value.parse().ok();
        let port = port.or_else(|| Some(value.parse::<SocketAddr>().ok()?.port()));
        port.map(|p| (*key, p))
    };
    LISTENERS.iter().filter_map(port).collect()
}

/// The keys whose port the container does not publish: `made_with` is `ZEPHYR_PUBLISHED`, the
/// ports `zephyr up` published when it made the container, and `None` (a container that does
/// not say) names nothing.
pub fn unpublished(cfg: &Config, made_with: Option<&str>) -> Vec<&'static str> {
    let Some(made_with) = made_with else {
        return Vec::new();
    };
    let ports: Vec<u16> = (made_with.split(','))
        .filter_map(|p| p.parse().ok())
        .collect();
    let missing = published(cfg)
        .into_iter()
        .filter(|(_, p)| !ports.contains(p));
    missing.map(|(key, _)| key).collect()
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
    let bad = || Refusal::Bad("the target is not HOST:PORT".into());
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

/// Why `ip` may not be connected to, if it may not: it must be public, or private and of a
/// listed entry, and never this container's own address. The one rule for the proxy and for
/// the direct routes of `network.allow`.
fn refused(ip: IpAddr, listed: bool, own: IpAddr, public: fn(IpAddr) -> bool) -> Option<String> {
    if ip != own && (public(ip) || listed && private_ok(ip)) {
        return None;
    }
    Some(match (listed, ip.is_loopback()) {
        (false, _) => format!("private address {ip}"),
        (true, true) => "loopback is never allowed".into(),
        (true, false) => format!("{ip} is never allowed"),
    })
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
                _ => return Err(Refusal::Unreachable("the name has no address".into())),
            }
        }
    };
    if let Some(reason) = ips.iter().find_map(|ip| refused(*ip, listed, own, public)) {
        return Err(Refusal::Denied(reason));
    }
    // The port is the request's, whatever the lookup answered.
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

/// What the proxy runs with. Tests pass their own.
#[derive(Clone)]
pub struct Floor {
    pub public: fn(IpAddr) -> bool,
    /// This container's address, never a destination.
    pub own: IpAddr,
    pub lookup: Lookup,
    /// Connections served at once; one more is closed at once.
    pub conns: usize,
    /// How long a client has to send its request head.
    pub head_wait: Duration,
    /// How long each checked address gets to answer.
    pub connect_wait: Duration,
    /// How long a server has to start its reply to a forwarded request, the client's upload
    /// included.
    pub reply_wait: Duration,
    /// How long a tunnel, or a forwarded reply's body, may go with nothing moving.
    pub idle: Duration,
    /// How the proxy speaks TLS to the upstream of a decrypted host (DESIGN §4.21), built
    /// once: it verifies against the roots compiled into this binary (`ca::public_roots`),
    /// which no file or variable of the agent's reaches.
    pub upstream: Arc<rustls::ClientConfig>,
    /// What signs the leaf a decrypted host is served with. `None` when the CA could not be
    /// loaded: hosts with a token bound then answer 502, and the rest are tunnelled as ever.
    pub ca: Option<Arc<crate::ca::Authority>>,
}

impl Floor {
    pub fn live() -> anyhow::Result<Floor> {
        Ok(Floor {
            public,
            own: own_ip()?.into(),
            lookup: system_lookup,
            conns: 256,
            head_wait: Duration::from_secs(10),
            connect_wait: Duration::from_secs(10),
            // Past the longest long-poll in common use (a minute or so), with room to spare.
            reply_wait: Duration::from_secs(5 * 60),
            // Websockets and event streams ping far more often; an IMAP IDLE is renewed within
            // 29 minutes. A dead peer's slot comes back after this.
            idle: Duration::from_secs(30 * 60),
            upstream: crate::ca::client(crate::ca::public_roots())?,
            ca: None,
        })
    }
}

/// How a request ended, for the audit.
pub enum Outcome {
    Allowed,
    Refused(String),
}

/// Refusals written in one minute; the rest of that minute are counted in one closing row.
const REFUSALS_A_MINUTE: usize = 60;

/// Which audit rows a request leaves: a destination once an hour, and each refusal up to
/// `REFUSALS_A_MINUTE`, so neither a browser nor a looping agent floods state.db.
#[derive(Default)]
pub struct Audit {
    hour: i64,
    seen: HashSet<String>,
    minute: i64,
    refused: usize,
}

impl Audit {
    /// Once the minute of the last refusals is over: the row counting those not written, if
    /// any. The writer calls it every second, so the count does not wait for a later request.
    pub fn flush(&mut self, now: i64) -> Option<(String, String, String)> {
        if now / 60 == self.minute {
            return None;
        }
        let more = self.refused.saturating_sub(REFUSALS_A_MINUTE);
        (self.minute, self.refused) = (now / 60, 0);
        // No request's own row: `proxy`, with no destination.
        (more > 0).then(|| {
            (
                "proxy".into(),
                String::new(),
                format!("refused: {more} more"),
            )
        })
    }

    /// The `(tool, args, outcome)` rows to write for a request to `dest` (`host:port -> ip`).
    pub fn note(
        &mut self,
        now: i64,
        tool: &str,
        dest: &str,
        o: Outcome,
    ) -> Vec<(String, String, String)> {
        let mut rows: Vec<_> = self.flush(now).into_iter().collect();
        let outcome = match o {
            Outcome::Allowed => {
                if now / 3600 != self.hour {
                    self.hour = now / 3600;
                    self.seen.clear();
                }
                // A destination once an hour, and each token on each host (`NAME -> host`).
                let key = match tool {
                    "swap" => format!("swap {dest}"),
                    _ => dest.split(" -> ").next().unwrap_or(dest).to_string(),
                };
                (self.seen.insert(key)).then(|| "allowed".to_string())
            }
            Outcome::Refused(reason) => {
                self.refused += 1;
                (self.refused <= REFUSALS_A_MINUTE).then(|| format!("refused: {reason}"))
            }
        };
        rows.extend(outcome.map(|o| (tool.into(), dest.into(), o)));
        rows
    }
}

/// What a 403 says after its reason.
const HINT: &str =
    "the owner can allow HOST:PORT in network.allow; propose it with gateway /set network.allow\n";

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;

/// One connection's share of the proxy.
#[derive(Clone)]
struct Conn {
    cfgs: watch::Receiver<Arc<Config>>,
    floor: Floor,
    log: std::sync::mpsc::Sender<Note>,
    /// The token broker's map of what is swapped where (DESIGN §4.21).
    maps: Arc<crate::broker::Maps>,
    /// Held until the connection and its tunnel are both over.
    slot: Arc<OwnedSemaphorePermit>,
}

/// Serves the proxy until dropped. Pending forever outside bridge mode, and when it cannot
/// start: nothing else listens on its port, so the agent then has no network at all.
pub async fn serve(
    cfgs: watch::Receiver<Arc<Config>>,
    wake: &crate::scheduler::Wake,
) -> anyhow::Result<()> {
    if !bridge() {
        return std::future::pending().await;
    }
    let db = cfgs.borrow().db_path();
    let state = cfgs.borrow().state_dir().to_path_buf();
    let run = async {
        let mut floor = Floor::live()?;
        // The CA `supervise` made when the container started. Without it nothing is decrypted,
        // and no token is handed out in its place.
        match crate::ca::Authority::load(&state) {
            Ok(ca) => floor.ca = Some(Arc::new(ca)),
            Err(e) => {
                eprintln!("zephyr: the token broker is down, so vault tokens do not work: {e:#}");
                let notice = format!(
                    "[zephyr] the token broker is down, so vault tokens do not work: {e:#}"
                );
                if Store::open(&db)
                    .and_then(|s| s.notify_last_owner(&notice))
                    .is_ok()
                {
                    wake.outbox.notify_one();
                }
            }
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, PORT))
            .await
            .with_context(|| format!("binding 127.0.0.1:{PORT}"))?;
        serve_on(listener, cfgs, floor).await
    };
    let Err(e) = run.await else { return Ok(()) };
    eprintln!("zephyr: the network proxy is down, so the agent has no internet: {e:#}");
    let notice = format!("[zephyr] the network proxy is down, so the agent has no internet: {e:#}");
    match Store::open(&db).and_then(|s| s.notify_last_owner(&notice)) {
        Ok(()) => wake.outbox.notify_one(),
        Err(e) => eprintln!("zephyr: telling the owner that the proxy is down failed: {e}"),
    }
    std::future::pending().await
}

/// Serves the proxy on `listener`: `CONNECT host:port` tunnels and absolute-form `http://`
/// requests, each to addresses `decide` passed, and nothing else.
pub async fn serve_on(
    listener: TcpListener,
    cfgs: watch::Receiver<Arc<Config>>,
    floor: Floor,
) -> anyhow::Result<()> {
    let store = Store::open(&cfgs.borrow().db_path()).context("opening state.db")?;
    let (log, notes) = std::sync::mpsc::channel();
    let cfg = cfgs.borrow().clone();
    std::thread::spawn(move || write_audit(store, notes, cfg));
    let slots = Arc::new(Semaphore::new(floor.conns));
    let maps = Arc::new(crate::broker::Maps::default());
    let head_wait = floor.head_wait;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // shortcut: a persistent accept error (e.g. out of fds) retries every 100 ms with no backoff.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue; // dropped, so closed at once
        };
        let conn = Conn {
            cfgs: cfgs.clone(),
            floor: floor.clone(),
            log: log.clone(),
            maps: maps.clone(),
            slot: Arc::new(slot),
        };
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let conn = conn.clone();
                async move { Ok::<_, std::convert::Infallible>(conn.answer(req).await) }
            });
            // One request per connection, so the head deadline bounds an idle one too.
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(head_wait)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await;
        });
    }
}

impl Conn {
    async fn answer(self, req: Request<Incoming>) -> Response<Body> {
        let cfg = self.cfgs.borrow().clone();
        let uri = req.uri().clone();
        // The two forms a proxy is asked in; `authority` always carries a port.
        let (tool, authority) = match (req.method() == Method::CONNECT, uri.authority()) {
            (true, Some(a)) if uri.scheme().is_none() && uri.path_and_query().is_none() => {
                ("connect", a.to_string())
            }
            (false, Some(a)) if uri.scheme_str() == Some("http") => match a.port() {
                Some(_) => ("http", a.to_string()),
                None => ("http", format!("{a}:80")),
            },
            _ => ("http", String::new()),
        };
        // A stand-in is carried nowhere it is not swapped (DESIGN §4.21): not in a host name,
        // and nowhere in a plain request, which is never swapped. Before any lookup or connect.
        let map = self.maps.current(&cfg);
        let whole = uri.to_string();
        let mut seen: Vec<&[u8]> = vec![authority.as_bytes()];
        if tool == "http" {
            seen.push(whole.as_bytes());
            seen.extend(req.headers().values().map(|v| v.as_bytes()));
        }
        if let Err(refused) = crate::broker::scan(&seen, &map) {
            let dest = match authority.is_empty() {
                true => whole.as_str(),
                false => authority.as_str(),
            };
            return self.refuse_token(&cfg, dest, &refused);
        }
        let checked = match target(&authority) {
            Ok((host, port)) => {
                // Malformed entries were refused when the file loaded; none means none.
                let allow = parse_allow(&cfg.network.allow).unwrap_or_default();
                let f = &self.floor;
                let addrs = decide(&host, port, &allow, f.own, f.public, f.lookup).await;
                addrs.map(|addrs| (format!("{host}:{port}"), addrs))
            }
            Err(e) => Err(e),
        };
        let (dest, addrs) = match checked {
            Ok(checked) => checked,
            Err(e) => {
                let dest = match authority.is_empty() {
                    true => uri.to_string(),
                    false => authority,
                };
                return self.refuse(&cfg, tool, &dest, e);
            }
        };
        // A host some token is bound to is decrypted, and only such a host.
        let leaf = match tool == "connect" && map.bound(&dest) {
            true => match self.leaf(&map, &dest) {
                Ok(leaf) => Some(leaf),
                Err(why) => return self.refuse(&cfg, tool, &dest, Refusal::Unreachable(why)),
            },
            false => None,
        };
        // Only the addresses just checked, never the name again.
        let Some((upstream, addr)) = connect(&addrs, self.floor.connect_wait).await else {
            let e = Refusal::Unreachable("no address answered".into());
            return self.refuse(&cfg, tool, &dest, e);
        };
        let went = format!("{dest} -> {}", addr.ip());
        if tool == "connect" {
            self.record(&cfg, tool, &went, Outcome::Allowed);
            match leaf {
                Some(leaf) => drop(tokio::spawn(self.serve_tls(req, upstream, dest, leaf))),
                None => drop(tokio::spawn(self.tunnel(req, upstream))),
            }
            return Response::new(body(""));
        }
        match forward(req, upstream, &self.floor).await {
            Ok(reply) => {
                self.record(&cfg, tool, &went, Outcome::Allowed);
                reply
            }
            Err(_) => {
                let e = Refusal::Unreachable("the server did not answer".into());
                self.refuse(&cfg, tool, &went, e)
            }
        }
    }

    /// What a decrypted host is served with, or why it cannot be: no CA, or a vault that could
    /// not be read. Either way the host is not tunnelled instead.
    fn leaf(
        &self,
        map: &crate::broker::Map,
        dest: &str,
    ) -> Result<Arc<rustls::ServerConfig>, String> {
        let down = || "the token broker is down".to_string();
        let ca = self.floor.ca.as_ref().ok_or_else(down)?;
        if map.unreadable {
            return Err("the vault could not be read".into());
        }
        let name = dest.rsplit_once(':').map_or(dest, |(name, _)| name);
        ca.leaf(name, crate::now()).map_err(|_| down())
    }

    /// Serves a `CONNECT` to a bound host (`dest`, `host:port`) decrypted: TLS to the client
    /// with the host's leaf, TLS to the address already checked and connected, verified for
    /// that name against `Floor::roots`, and HTTP/1.1 between them, each request's stand-ins
    /// swapped (`Decrypted::answer`). A client that names another host in its handshake, or an
    /// upstream that does not prove the name, gets that as the answer to every request.
    async fn serve_tls(
        self,
        req: Request<Incoming>,
        upstream: TcpStream,
        dest: String,
        leaf: Arc<rustls::ServerConfig>,
    ) {
        let cfg = self.cfgs.borrow().clone();
        let name = dest
            .rsplit_once(':')
            .map_or(dest.as_str(), |(name, _)| name);
        let name = name.to_string();
        let Ok(client) = hyper::upgrade::on(req).await else {
            return;
        };
        let accept = tokio_rustls::TlsAcceptor::from(leaf).accept(TokioIo::new(client));
        let Ok(Ok(client)) = tokio::time::timeout(self.floor.head_wait, accept).await else {
            return;
        };
        // Dropped when the upstream connection ends, so the client's ends with it and the
        // next request starts with a new CONNECT, checked afresh.
        let (ended, upstream_ended) = tokio::sync::oneshot::channel::<()>();
        let mut ended = Some(ended);
        let sni = client
            .get_ref()
            .1
            .server_name()
            .map(str::to_ascii_lowercase);
        let sender = if sni.is_some_and(|sni| sni != name) {
            Err((StatusCode::FORBIDDEN, format!("Host does not match {name}")))
        } else {
            let unproven = || {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("{name} did not prove it is {name}"),
                )
            };
            let tls = async {
                let config = self.floor.upstream.clone();
                let server = rustls::pki_types::ServerName::try_from(name.clone()).ok()?;
                let tls = tokio_rustls::TlsConnector::from(config).connect(server, upstream);
                let tls = tokio::time::timeout(self.floor.connect_wait, tls).await;
                hyper::client::conn::http1::handshake(TokioIo::new(tls.ok()?.ok()?))
                    .await
                    .ok()
            };
            match tls.await {
                Some((sender, conn)) => {
                    let ended = ended.take();
                    tokio::spawn(async move {
                        let _ = conn.with_upgrades().await;
                        drop(ended);
                    });
                    Ok(tokio::sync::Mutex::new(sender))
                }
                None => Err(unproven()),
            }
        };
        if let Err((_, why)) = &sender {
            self.record(&cfg, "swap", &dest, Outcome::Refused(why.clone()));
        }
        let this = Arc::new(Decrypted {
            conn: self,
            name,
            dest,
            sender,
        });
        let svc = hyper::service::service_fn(move |req| {
            let this = this.clone();
            async move { Ok::<_, std::convert::Infallible>(this.answer(req).await) }
        });
        // shortcut: one timer for a request head and for the wait between two requests, 90 s;
        // split them if a slow head ever holds slots.
        let serving = hyper::server::conn::http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(KEEP_ALIVE)
            .serve_connection(TokioIo::new(client), svc)
            .with_upgrades();
        tokio::pin!(serving);
        tokio::select! {
            _ = &mut serving => {}
            _ = upstream_ended => {
                serving.as_mut().graceful_shutdown();
                let _ = serving.await;
            }
        }
        drop(ended);
    }

    /// The reply to a request that carries a stand-in where it is not swapped: 403, an audit
    /// row by the token's name, and for a token that exists a notice to the owner. Nothing was
    /// sent on in any form.
    fn refuse_token(
        &self,
        cfg: &Arc<Config>,
        dest: &str,
        refused: &crate::broker::Refused,
    ) -> Response<Body> {
        // Neither the reply nor the row quotes a stand-in, which a host name may hold.
        let shown = match dest.contains(crate::broker::PREFIX) {
            true => "that host",
            false => dest,
        };
        let args = match refused.token() {
            Some(name) => format!("{name} -> {dest}"),
            None => dest.to_string(),
        };
        let note = Note {
            cfg: cfg.clone(),
            at: crate::now(),
            tool: "swap",
            dest: args,
            // The row's reason never repeats the agent's text: the destination is in its args.
            outcome: Outcome::Refused(refused.reason("this host")),
            notice: (refused
                .token()
                .zip(refused.notice(crate::vault::shown(dest))))
            .map(|(token, text)| (format!("{token} {}", refused.reason("")), dest.into(), text)),
        };
        let _ = self.log.send(note);
        answer(
            StatusCode::FORBIDDEN,
            &refused.reason(shown),
            "zephyr sends a vault token only to the hosts its owner bound it to; ask with the \
             vault tool's bind\n",
        )
    }

    /// Carries bytes between the client and `upstream` (`relay`), then frees the slot.
    async fn tunnel(self, req: Request<Incoming>, upstream: TcpStream) {
        if let Ok(client) = hyper::upgrade::on(req).await {
            relay(TokioIo::new(client), upstream, self.floor.idle).await;
        }
        drop(self.slot);
    }

    /// The reply to a request that is not carried, with its audit row.
    fn refuse(
        &self,
        cfg: &Arc<Config>,
        tool: &'static str,
        dest: &str,
        e: Refusal,
    ) -> Response<Body> {
        let (status, reason, text) = match e {
            Refusal::Bad(r) => (StatusCode::BAD_REQUEST, r, String::new()),
            Refusal::Denied(r) => (StatusCode::FORBIDDEN, r, HINT.into()),
            Refusal::Unreachable(r) => (StatusCode::BAD_GATEWAY, r, String::new()),
        };
        let reply = answer(status, &reason, &text);
        self.record(cfg, tool, dest, Outcome::Refused(reason));
        reply
    }

    /// Hands the request to the audit's writer: nothing here touches a file.
    fn record(&self, cfg: &Arc<Config>, tool: &'static str, dest: &str, outcome: Outcome) {
        let note = Note {
            cfg: cfg.clone(),
            at: crate::now(),
            tool,
            dest: dest.into(),
            outcome,
            notice: None,
        };
        // The writer is gone only when the proxy is.
        let _ = self.log.send(note);
    }
}

/// One request, on its way to the audit.
struct Note {
    cfg: Arc<Config>,
    at: i64,
    tool: &'static str,
    dest: String,
    outcome: Outcome,
    /// A token the agent put where it does not go: what makes it the same attempt (the token
    /// and how, and the host), and what the owner is told.
    notice: Option<(String, String, String)>,
}

/// A reply of the proxy's own, its reason in the status line, where `curl -v` and a library's
/// error show it.
fn answer(status: StatusCode, reason: &str, text: &str) -> Response<Body> {
    let mut reply = Response::new(body(text));
    *reply.status_mut() = status;
    if let Ok(phrase) = hyper::ext::ReasonPhrase::try_from(format!("zephyr: {reason}")) {
        reply.extensions_mut().insert(phrase);
    }
    reply
}

/// How long a decrypted connection may wait for its next request.
const KEEP_ALIVE: Duration = Duration::from_secs(90);

/// One decrypted connection to a bound host (DESIGN §4.21).
struct Decrypted {
    conn: Conn,
    /// The bound host, and it with its port, as the owner bound it.
    name: String,
    dest: String,
    /// The one upstream connection; or why there is none, which answers every request.
    sender: Result<
        tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<Incoming>>,
        (StatusCode, String),
    >,
}

impl Decrypted {
    /// Whether the request names the bound host and no other: one `Host`, the name with or
    /// without its port. Without this a bound host on a shared CDN would carry the token to
    /// another customer's origin.
    fn for_this_host(&self, req: &Request<Incoming>) -> bool {
        let named = |h: &str| {
            let h = h.to_ascii_lowercase();
            h == self.name || h == self.dest
        };
        let mut hosts = req.headers().get_all(HOST).iter();
        let (Some(host), None) = (hosts.next(), hosts.next()) else {
            return false;
        };
        host.to_str().is_ok_and(named) && req.uri().authority().is_none_or(|a| named(a.as_str()))
    }

    fn refuse(&self, cfg: &Arc<Config>, status: StatusCode, why: String) -> Response<Body> {
        let reply = answer(status, &why, "");
        (self.conn).record(cfg, "swap", &self.dest, Outcome::Refused(why));
        reply
    }

    /// One request on the decrypted connection: checked to be for this host, its stand-ins
    /// swapped for the tokens bound here (by the map as it is now, so an unbinding holds for
    /// the next request), sent on this connection's own upstream connection (made for it at
    /// its `CONNECT`, never pooled or shared with another client's), and the reply turned
    /// back. Bodies pass as frames, never collected.
    async fn answer(&self, req: Request<Incoming>) -> Response<Body> {
        let cfg = self.conn.cfgs.borrow().clone();
        let sender = match &self.sender {
            Ok(sender) => sender,
            // Audited once, when the connection was made.
            Err((status, why)) => return answer(*status, why, ""),
        };
        let map = self.conn.maps.current(&cfg);
        if map.unreadable {
            let why = "the vault could not be read".to_string();
            return self.refuse(&cfg, StatusCode::BAD_GATEWAY, why);
        }
        if !self.for_this_host(&req) {
            let why = format!("Host does not match {}", self.name);
            return self.refuse(&cfg, StatusCode::FORBIDDEN, why);
        }
        let (mut parts, body_in) = req.into_parts();
        // Origin form, whatever form the client wrote the target in.
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        parts.uri = target.parse().unwrap_or_default();
        // What is between the client and this proxy stays here (`hop_by_hop`): an `Upgrade`
        // among it, so no websocket is carried on a decrypted host, whose frames nobody
        // would read. And the whole reply is asked for, as it is: a range of a body could cut
        // a value in a place of the client's choosing, and an encoded one cannot be read.
        hop_by_hop(&mut parts.headers);
        // This hop's own: the proxy takes a reply's trailers, and passes them on.
        (parts.headers).insert(hyper::header::TE, HeaderValue::from_static("trailers"));
        parts.headers.remove(hyper::header::RANGE);
        parts.headers.remove(hyper::header::IF_RANGE);
        let swapped =
            match crate::broker::swap(&mut parts.uri, &mut parts.headers, &map, &self.dest) {
                Ok(swapped) => swapped,
                Err(refused) => return self.conn.refuse_token(&cfg, &self.dest, &refused),
            };
        let identity = HeaderValue::from_static("identity");
        parts
            .headers
            .insert(hyper::header::ACCEPT_ENCODING, identity);
        let no_body = parts.method == Method::HEAD;
        for name in &swapped.used {
            let dest = format!("{name} -> {}", self.dest);
            self.conn.record(&cfg, "swap", &dest, Outcome::Allowed);
        }
        let floor = &self.conn.floor;
        let sent = async {
            let mut sender = sender.lock().await;
            sender.ready().await.ok()?;
            let reply = sender.send_request(Request::from_parts(parts, body_in));
            tokio::time::timeout(floor.reply_wait, reply)
                .await
                .ok()?
                .ok()
        };
        let Some(mut reply) = sent.await else {
            let why = "the server did not answer".to_string();
            return self.refuse(&cfg, StatusCode::BAD_GATEWAY, why);
        };
        // A reply that is not the whole body as the server wrote it is not passed on unread:
        // another protocol, a part of a body, or an encoded one.
        let unreadable = match reply.status() {
            StatusCode::SWITCHING_PROTOCOLS => Some("switched protocols"),
            StatusCode::PARTIAL_CONTENT => Some("sent a part of a reply"),
            _ if encoded(reply.headers()) => Some("sent a reply zephyr cannot read (compressed)"),
            _ => None,
        };
        if let Some(what) = unreadable {
            let why = format!("{} {what}", self.name);
            return self.refuse(&cfg, StatusCode::BAD_GATEWAY, why);
        }
        // A host that sends back what it was sent (an echo, an error page that prints the
        // request) must not hand the agent the token: every value bound here goes back to its
        // stand-in, in the reply's headers and in its body, and so does a credential this
        // request carried encoded. The status line's own text is the server's to choose, so
        // it is left out: the client gets the status's usual words.
        // A server that ends its connection with this reply leaves the proxy none to send the
        // client's next request on: the client is told its own ends too, so it reconnects
        // (a new `CONNECT`, checked afresh) and does not write into a closed one.
        let closing = (reply.headers().get_all(hyper::header::CONNECTION).iter())
            .flat_map(|v| v.to_str().unwrap_or_default().split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("close"));
        hop_by_hop(reply.headers_mut());
        if closing || sender.lock().await.is_closed() {
            let close = HeaderValue::from_static("close");
            reply.headers_mut().insert(hyper::header::CONNECTION, close);
        }
        reply.extensions_mut().remove::<hyper::ext::ReasonPhrase>();
        crate::broker::unswap(reply.headers_mut(), &map, &self.dest, &swapped.sent);
        let idle = |body| Idle {
            body,
            wait: floor.idle,
            timer: Box::pin(tokio::time::sleep(floor.idle)),
        };
        let replacer = map.unswapper(&self.dest, &swapped.sent);
        let bodiless = matches!(
            reply.status(),
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
        );
        if no_body || bodiless || replacer.is_empty() {
            return reply.map(|body| idle(body).boxed_unsync());
        }
        // A stand-in is not as long as its value, so the length is no longer known ahead.
        reply.headers_mut().remove(hyper::header::CONTENT_LENGTH);
        reply.map(|body| {
            let body = Unswapped {
                body: idle(body),
                replacer,
                trailers: None,
                done: false,
            };
            body.boxed_unsync()
        })
    }
}

/// Removes what RFC 9110 §7.6.1 says goes no further than one connection: `Connection` and
/// every header it names, and the ones a proxy never passes on. On the decrypted path each
/// side's are its own: a client cannot have the proxy carry a header it named as hop-by-hop
/// past the checks, nor keep a say in how the upstream connection is used.
fn hop_by_hop(headers: &mut hyper::HeaderMap) {
    use hyper::header::{HeaderName, CONNECTION};
    // Not `Trailer`: it describes the message, and goes with it end to end.
    const ALWAYS: [&str; 6] = [
        "connection",
        "keep-alive",
        "te",
        "upgrade",
        "proxy-authorization",
        "proxy-connection",
    ];
    let named: Vec<HeaderName> = (headers.get_all(CONNECTION).iter())
        .flat_map(|v| v.to_str().unwrap_or_default().split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        // The body's framing is hyper's to keep right on each side.
        .filter(|name| {
            ![
                hyper::header::TRANSFER_ENCODING,
                hyper::header::CONTENT_LENGTH,
                HOST,
            ]
            .contains(name)
        })
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in ALWAYS {
        headers.remove(name);
    }
}

/// Whether a reply's body is anything but the bytes the server meant: a `Content-Encoding`
/// other than `identity`, or a `Transfer-Encoding` beyond `chunked` (which hyper has undone).
fn encoded(headers: &hyper::HeaderMap) -> bool {
    let other = |name, plain: &str| {
        let values = headers.get_all(name).iter();
        let mut codings = values.flat_map(|v| v.to_str().unwrap_or("?").split(','));
        codings.any(|c| !c.trim().is_empty() && !c.trim().eq_ignore_ascii_case(plain))
    };
    other(hyper::header::CONTENT_ENCODING, "identity")
        || other(hyper::header::TRANSFER_ENCODING, "chunked")
}

/// A decrypted host's reply body with each real value bound there turned back into its
/// stand-in, as it streams: a value cut across two frames is held until it is whole.
struct Unswapped {
    body: Idle,
    replacer: crate::broker::Replacer,
    /// Trailers, waiting for the data held back before them.
    trailers: Option<hyper::body::Frame<Bytes>>,
    done: bool,
}

impl hyper::body::Body for Unswapped {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        use hyper::body::Frame;
        let this = &mut *self;
        loop {
            if let Some(trailers) = this.trailers.take() {
                return Poll::Ready(Some(Ok(trailers)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            let out = match std::task::ready!(Pin::new(&mut this.body).poll_frame(cx)) {
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => this.replacer.push(&data),
                    Err(trailers) => {
                        let out = this.replacer.finish();
                        let mut trailers = trailers.into_trailers().unwrap_or_default();
                        (trailers.values_mut()).for_each(|v| this.replacer.header(v));
                        this.trailers = Some(Frame::trailers(trailers));
                        out
                    }
                },
                None => {
                    this.done = true;
                    this.replacer.finish()
                }
            };
            if !out.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
            }
        }
    }
}

/// The audit's one writer, on a thread of its own, so reading the vault and the facts and
/// writing state.db never hold up the runtime the proxy runs on. It decides which rows a request
/// leaves (`Audit`), scrubs vault and fact values out of them and writes them; every second it
/// closes a minute whose refusals went over the cap. Ends when the proxy does.
fn write_audit(store: Store, notes: std::sync::mpsc::Receiver<Note>, mut cfg: Arc<Config>) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut audit = Audit::default();
    let mut notices = crate::broker::Notices::default();
    loop {
        let mut tell = None;
        let rows = match notes.recv_timeout(Duration::from_secs(1)) {
            Ok(note) => {
                cfg = note.cfg;
                tell = (note.notice).filter(|(what, host, _)| notices.due(note.at, what, host));
                audit.note(note.at, note.tool, &note.dest, note.outcome)
            }
            Err(RecvTimeoutError::Timeout) => audit.flush(crate::now()).into_iter().collect(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if rows.is_empty() && tell.is_none() {
            continue;
        }
        let scrub = crate::gate::Scrub::load(&cfg);
        for (tool, args, outcome) in rows {
            crate::gate::audit_row(&store, "net", &tool, &scrub.text(&args), &outcome);
        }
        // It names no command: the answer to an attack should not be one tap from allowing it.
        // shortcut: sent with the router's next tick (5 s), not at once.
        if let Some((_, _, text)) = tell {
            // Scrubbed like a row: a host name may hold a stand-in or a private fact.
            let told = store.notify_last_owner(&scrub.text(&text));
            if let Err(e) = told {
                eprintln!("zephyr: telling the owner of a refused token failed: {e}");
            }
        }
    }
}

fn body(text: &str) -> Body {
    Full::new(Bytes::from(text.to_owned()))
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Carries bytes both ways until both sides have closed, one fails, or nothing has moved for
/// `idle` (a write that stalls counts): a peer that went away without a word must not hold its
/// slot for ever. A side that closes is closed on the other, which may go on sending.
async fn relay<A, B>(a: A, b: B, idle: Duration)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    /// What one side sent goes to the other; its end of stream ends that direction.
    async fn pass<W: AsyncWrite + Unpin>(
        read: std::io::Result<usize>,
        from: &[u8],
        to: &mut W,
        open: &mut bool,
    ) -> std::io::Result<()> {
        match read? {
            0 => {
                *open = false;
                to.shutdown().await
            }
            n => to.write_all(&from[..n]).await,
        }
    }
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let (mut from_a, mut from_b) = (vec![0u8; 16 * 1024], vec![0u8; 16 * 1024]);
    let (mut a_open, mut b_open) = (true, true);
    while a_open || b_open {
        let step = async {
            tokio::select! {
                n = a_read.read(&mut from_a), if a_open => pass(n, &from_a, &mut b_write, &mut a_open).await,
                n = b_read.read(&mut from_b), if b_open => pass(n, &from_b, &mut a_write, &mut b_open).await,
            }
        };
        if !matches!(tokio::time::timeout(idle, step).await, Ok(Ok(()))) {
            break;
        }
    }
}

/// A forwarded reply's body, ended with an error when the server sends nothing for `wait`.
struct Idle {
    body: Incoming,
    wait: Duration,
    timer: Pin<Box<tokio::time::Sleep>>,
}

impl hyper::body::Body for Idle {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(frame) => {
                let wait = this.wait;
                this.timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + wait);
                Poll::Ready(frame.map(|f| f.map_err(Into::into)))
            }
            Poll::Pending => {
                let stopped = this.timer.as_mut().poll(cx);
                stopped.map(|()| Some(Err("the server stopped sending".into())))
            }
        }
    }
}

/// A connection to the first of `addrs` that answers within `wait`. It takes addresses, never
/// a name, so nothing is resolved again between the check and the connect.
async fn connect(addrs: &[SocketAddr], wait: Duration) -> Option<(TcpStream, SocketAddr)> {
    for &addr in addrs {
        if let Ok(Ok(stream)) = tokio::time::timeout(wait, TcpStream::connect(addr)).await {
            return Some((stream, addr));
        }
    }
    None
}

/// Sends an absolute-form request on to `upstream` in origin form and returns its reply.
async fn forward(
    mut req: Request<Incoming>,
    upstream: TcpStream,
    floor: &Floor,
) -> anyhow::Result<Response<Body>> {
    let uri = req.uri().clone();
    let authority = uri.authority().context("no authority")?.as_str();
    *req.uri_mut() = uri.path_and_query().map_or("/", |p| p.as_str()).parse()?;
    let headers = req.headers_mut();
    headers.remove(hyper::header::PROXY_AUTHORIZATION);
    headers.remove("proxy-connection");
    // The server the proxy checked, whatever the request's own header named (RFC 9112 §3.2.2).
    headers.insert(HOST, HeaderValue::from_str(authority)?);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(upstream)).await?;
    tokio::spawn(conn);
    let reply = tokio::time::timeout(floor.reply_wait, sender.send_request(req)).await??;
    let idle = |body| Idle {
        body,
        wait: floor.idle,
        timer: Box::pin(tokio::time::sleep(floor.idle)),
    };
    Ok(reply.map(|body| idle(body).boxed_unsync()))
}

/// The rules that leave the agent's uid no route but its own loopback (DESIGN §4.20). The jump
/// is on the agent's uid, so a packet with no socket (a kernel reset) falls through to accept;
/// TCP is reset, so a connect fails at once instead of timing out; and there is no port list,
/// because nothing of root's listens on loopback but the proxy. The `allow` set holds the
/// `address . port` pairs of `network.allow` (`keep_allow`), the only direct routes there are.
/// The first rule keeps the agent from sending as Docker's resolver (127.0.0.11, an address any
/// local process may bind): a lookup root makes there must not be answerable by the agent.
pub fn ruleset(agent_uid: u32) -> String {
    format!(
        "\
table inet zephyr
delete table inet zephyr
table inet zephyr {{
  set allow {{
    type ipv4_addr . inet_service
  }}
  chain output {{
    type filter hook output priority filter; policy accept;
    meta skuid {agent_uid} jump agent
  }}
  chain agent {{
    ip saddr 127.0.0.11 reject            # never as Docker's DNS: root's lookups cannot be answered by the agent
    ct state established accept           # replies on connections made to the agent (x11vnc, a dev server the owner opens)
    ip daddr 127.0.0.11 reject            # Docker's embedded DNS: the agent gets no resolver
    ip daddr 127.0.0.0/8 accept           # its own servers, and the proxy at 127.0.0.1:7792
    ip6 daddr ::1 accept
    ip daddr . tcp dport @allow accept    # network.allow, as supervise resolved it (egress::keep_allow)
    meta l4proto tcp reject with tcp reset
    reject with icmpx admin-prohibited
  }}
}}
"
    )
}

/// Loads the rules for the agent's uid (`ZEPHYR_AGENT_UID`) with root's own `nft`. `supervise`
/// runs this once, before the first daemon: a container start is a fresh network namespace.
pub fn load_floor() -> anyhow::Result<()> {
    let uid = std::env::var("ZEPHYR_AGENT_UID").context("ZEPHYR_AGENT_UID is not set")?;
    let uid: u32 = uid.parse().context("ZEPHYR_AGENT_UID is not a uid")?;
    anyhow::ensure!(uid != 0, "ZEPHYR_AGENT_UID is root's");
    nft(&ruleset(uid))
}

/// Runs `script` through root's own `nft`, as one transaction.
fn nft(script: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut nft = Command::new("/usr/sbin/nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running /usr/sbin/nft")?;
    let mut stdin = nft.stdin.take().context("nft's stdin")?;
    stdin.write_all(script.as_bytes())?;
    drop(stdin);
    let out = nft.wait_with_output()?;
    let said = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(out.status.success(), "nft: {}", said.trim());
    Ok(())
}

/// The direct routes `network.allow` gives the agent: what goes into the `allow` set and into
/// /etc/hosts, and what got no route and why.
#[derive(Debug, Default, PartialEq)]
pub struct Routes {
    /// `address . port` pairs, sorted, each once.
    pub pairs: Vec<(Ipv4Addr, u16)>,
    /// One address per listed name, for /etc/hosts: the agent has no resolver.
    pub names: Vec<(Ipv4Addr, String)>,
    pub problems: Vec<String>,
}

/// Resolves `allow` into direct routes under the rule the proxy applies to a listed entry
/// (`refused`): every address of a host must pass, or the host gets none. IPv4 only, as the
/// container's network is.
pub async fn routes(
    allow: &[Entry],
    own: IpAddr,
    public: fn(IpAddr) -> bool,
    lookup: Lookup,
) -> Routes {
    let mut out = Routes::default();
    let mut seen: Vec<&Host<String>> = Vec::new();
    for entry in allow {
        if seen.contains(&&entry.host) {
            continue;
        }
        seen.push(&entry.host);
        let ips: Vec<IpAddr> = match &entry.host {
            Host::Ipv4(ip) => vec![IpAddr::V4(*ip)],
            Host::Ipv6(ip) => vec![IpAddr::V6(*ip)],
            Host::Domain(name) => {
                let found = tokio::time::timeout(LOOKUP_WAIT, lookup(name.clone(), entry.port));
                let found = found.await.ok().and_then(Result::ok).unwrap_or_default();
                found.iter().map(SocketAddr::ip).collect()
            }
        };
        let v4: Vec<Ipv4Addr> = (ips.iter())
            .filter_map(|ip| match ip {
                IpAddr::V4(ip) => Some(*ip),
                IpAddr::V6(_) => None,
            })
            .collect();
        let problem = match ips.iter().find_map(|ip| refused(*ip, true, own, public)) {
            Some(reason) => Some(reason),
            None if ips.is_empty() => Some("the name has no address".into()),
            None if v4.is_empty() => Some("it has no IPv4 address".into()),
            None => None,
        };
        if let Some(problem) = problem {
            out.problems
                .push(format!("{}:{}: {problem}", entry.host, entry.port));
            continue;
        }
        if let Host::Domain(name) = &entry.host {
            out.names.push((v4[0], name.clone()));
        }
        let ports = allow
            .iter()
            .filter(|e| e.host == entry.host)
            .map(|e| e.port);
        for port in ports {
            out.pairs.extend(v4.iter().map(|ip| (*ip, port)));
        }
    }
    out.pairs.sort();
    out.pairs.dedup();
    out
}

/// The nft commands that make the `allow` set exactly `pairs`, in one transaction.
fn allow_script(pairs: &[(Ipv4Addr, u16)]) -> String {
    let mut script = String::from("flush set inet zephyr allow\n");
    if !pairs.is_empty() {
        let pairs: Vec<String> = (pairs.iter())
            .map(|(ip, port)| format!("{ip} . {port}"))
            .collect();
        script += &format!("add element inet zephyr allow {{ {} }}\n", pairs.join(", "));
    }
    script
}

/// What marks a line of /etc/hosts as one of ours.
const HOSTS_MARK: &str = "\t# zephyr network.allow";

/// /etc/hosts with the lines for `names` in place of the ones written before; every other line
/// is kept as it is.
fn hosts_with(current: &str, names: &[(Ipv4Addr, String)]) -> String {
    let kept = current.lines().filter(|l| !l.ends_with(HOSTS_MARK));
    let ours = (names.iter()).map(|(ip, name)| format!("{ip}\t{name}{HOSTS_MARK}"));
    let lines: Vec<String> = kept.map(String::from).chain(ours).collect();
    match lines.is_empty() {
        true => String::new(),
        false => lines.join("\n") + "\n",
    }
}

/// A DNS query for `name`'s IPv4 addresses; `None` for a name that cannot be asked for.
fn dns_query(name: &str, id: u16) -> Option<Vec<u8>> {
    let mut query = id.to_be_bytes().to_vec();
    // Recursion desired, one question.
    query.extend([1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        query.push(label.len() as u8);
        query.extend(label.bytes());
    }
    // The root, then type A, class IN.
    query.extend([0, 0, 1, 0, 1]);
    Some(query)
}

/// The IPv4 addresses in `reply`, when it is the answer to query `id` and reports no error.
/// Every read is checked: the packet comes off the network.
fn dns_answers(reply: &[u8], id: u16) -> Option<Vec<Ipv4Addr>> {
    let u16_at = |at: usize| Some(u16::from_be_bytes([*reply.get(at)?, *reply.get(at + 1)?]));
    let (response, failed) = (reply.get(2)? & 0x80 != 0, reply.get(3)? & 0x0f != 0);
    if u16_at(0)? != id || !response || failed {
        return None;
    }
    // A name ends at its root label or at a pointer to an earlier one.
    let after_name = |mut at: usize| loop {
        match *reply.get(at)? as usize {
            0 => return Some(at + 1),
            len if len & 0xc0 == 0xc0 => return Some(at + 2),
            len => at += 1 + len,
        }
    };
    let mut at = 12;
    for _ in 0..u16_at(4)? {
        at = after_name(at)? + 4;
    }
    let mut ips = Vec::new();
    for _ in 0..u16_at(6)? {
        at = after_name(at)?;
        let (kind, class, len) = (u16_at(at)?, u16_at(at + 2)?, u16_at(at + 8)? as usize);
        let data = reply.get(at + 10..at + 10 + len)?;
        if let (1, 1, [a, b, c, d]) = (kind, class, data) {
            ips.push(Ipv4Addr::new(*a, *b, *c, *d));
        }
        at += 10 + len;
    }
    Some(ips)
}

/// A name's IPv4 addresses as the nameservers of /etc/resolv.conf give them, never as /etc/hosts
/// does: the lines `keep_allow` writes there would otherwise answer its own next lookup for ever.
pub fn dns_lookup(host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
    use std::hash::{BuildHasher, Hasher};
    Box::pin(async move {
        let conf = tokio::fs::read_to_string("/etc/resolv.conf").await?;
        let servers = conf.lines().filter_map(|l| {
            let server = l.strip_prefix("nameserver")?.trim();
            server.parse::<IpAddr>().ok()
        });
        let id = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish() as u16;
        let query = dns_query(&host, id).ok_or(std::io::ErrorKind::InvalidInput)?;
        for server in servers {
            let any: SocketAddr = match server {
                IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
                IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
            };
            // Connected, so only that server's own packets are read.
            let socket = tokio::net::UdpSocket::bind(any).await?;
            socket.connect((server, 53)).await?;
            socket.send(&query).await?;
            let mut reply = [0u8; 1500];
            let got = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut reply)).await;
            if let Some(ips) = got
                .ok()
                .and_then(Result::ok)
                .and_then(|n| dns_answers(&reply[..n], id))
            {
                return Ok(ips.into_iter().map(|ip| (ip, port).into()).collect());
            }
        }
        Err(std::io::Error::other("no nameserver answered"))
    })
}

/// `network.allow` as zephyr.toml has it now; `None` when the file cannot be read. Read by key,
/// so nothing else in the file matters here.
fn allow_in(config: &std::path::Path) -> Option<String> {
    let file: toml::Value = toml::from_str(&std::fs::read_to_string(config).ok()?).ok()?;
    let allow = file.get("network").and_then(|n| n.get("allow"));
    Some(
        allow
            .and_then(|a| a.as_str())
            .unwrap_or_default()
            .to_string(),
    )
}

/// Keeps the agent's direct routes in step with `network.allow` (DESIGN §4.20), for the
/// programs that speak no proxy: a mail client, `ssh`, a database client. Run by `supervise`,
/// which alone holds NET_ADMIN; the daemon cannot change the rules. Every 5 s it reads the
/// setting, and when it changed, or an hour has passed (a minute, while an entry has no route),
/// it resolves the entries (`routes`), replaces the `allow` set and rewrites its own lines of
/// /etc/hosts, so a listed name resolves for the agent, to an address in the set, and nothing
/// else does. A file it cannot read, or an entry list the daemon would refuse, changes nothing.
pub async fn keep_allow(config: std::path::PathBuf) {
    const HOSTS: &str = "/etc/hosts";
    let (mut applied, mut said): (Option<(String, std::time::Instant, bool)>, _) =
        (None, Vec::new());
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Some(raw) = allow_in(&config) else {
            continue;
        };
        let due = applied.as_ref().is_none_or(|(was, at, whole)| {
            let again = Duration::from_secs(if *whole { 3600 } else { 60 });
            *was != raw || at.elapsed() >= again
        });
        let (Ok(entries), Ok(own), true) = (parse_allow(&raw), own_ip(), due) else {
            continue;
        };
        let routes = routes(&entries, own.into(), public, dns_lookup).await;
        let loaded = nft(&allow_script(&routes.pairs)).and_then(|()| {
            let current = std::fs::read_to_string(HOSTS)?;
            let next = hosts_with(&current, &routes.names);
            // In place: Docker mounts this one file, so it cannot be replaced by a rename.
            if next != current {
                std::fs::write(HOSTS, next)?;
            }
            Ok(())
        });
        if let Err(e) = loaded {
            eprintln!("zephyr: the routes for network.allow were not loaded: {e:#}");
            continue;
        }
        if routes.problems != said {
            for problem in &routes.problems {
                eprintln!("zephyr: network.allow has no direct route for {problem}");
            }
            said = routes.problems.clone();
        }
        applied = Some((raw, std::time::Instant::now(), routes.problems.is_empty()));
    }
}

/// CAP_NET_ADMIN's and CAP_SETPCAP's numbers (linux/capability.h).
const NET_ADMIN: u32 = 12;
const SETPCAP: u32 = 8;

/// Drops NET_ADMIN from the calling thread's bounding set, so what it then executes as root
/// cannot change the rules, and then SETPCAP, which the drop itself needs and nothing after it
/// does. For `pre_exec`: two syscalls, nothing allocated.
pub fn drop_net_admin() -> std::io::Result<()> {
    for cap in [NET_ADMIN, SETPCAP] {
        // SAFETY: a plain syscall with integer arguments.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether the process whose `/proc/<pid>/status` this is holds NET_ADMIN; `None` when the
/// text does not say.
fn net_admin(status: &str) -> Option<bool> {
    let line = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?;
    let caps = u64::from_str_radix(line.trim(), 16).ok()?;
    Some(caps >> NET_ADMIN & 1 == 1)
}

/// `Ok` only when a direct connection to `addr` is refused within `wait`, as the rules refuse
/// the agent's. A connection, a timeout or an unreachable network all mean they are missing.
pub fn direct_refused(addr: SocketAddr, wait: Duration) -> Result<(), String> {
    match std::net::TcpStream::connect_timeout(&addr, wait) {
        Ok(_) => Err(format!("connected to {addr}")),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(()),
        Err(e) => Err(format!("{addr}: {e}")),
    }
}

/// How one check of `__netcheck` came out; `Warn` is something the floor does not depend on.
enum Check {
    Ok,
    Warn(String),
    Fail(String),
}

impl From<Result<(), String>> for Check {
    fn from(r: Result<(), String>) -> Check {
        r.map_or_else(Check::Fail, |()| Check::Ok)
    }
}

/// What the token broker keeps from the agent (DESIGN §4.21), tried as whoever runs this: its
/// keys under `state` must not open, and nothing may be put into the folders `trust` that the
/// agent's tools take their roots from.
fn broker_reach(state: &Path, trust: &[&str]) -> Vec<(String, Check)> {
    let mut checks = Vec::new();
    let keys = [state.join("ca/ca.key"), state.join(crate::broker::KEY_FILE)];
    for key in keys {
        let check = match std::fs::File::open(&key) {
            Err(_) => Check::Ok,
            Ok(_) => Check::Fail("it opened".into()),
        };
        checks.push((format!("the agent cannot read {}", key.display()), check));
    }
    for dir in trust {
        let probe = Path::new(dir).join(format!(".zephyr-probe-{}", std::process::id()));
        let made = std::fs::File::create(&probe).is_ok();
        let _ = std::fs::remove_file(&probe);
        // A certificate already there must not be the agent's to rewrite either.
        let rewrite = |name: &str| {
            let file = Path::new(dir).join(name);
            std::fs::OpenOptions::new().append(true).open(file).is_ok()
        };
        let check = match made || rewrite("ca.pem") || rewrite("ca-certificates.crt") {
            false => Check::Ok,
            true => Check::Fail("a file could be written there".into()),
        };
        checks.push((format!("the agent cannot add trust in {dir}"), check));
    }
    checks
}

/// `zephyr __netcheck [OWN_IP:ATTACH_PORT]`: what the agent's uid can reach, one
/// `ok|fail|warn <what>` line per check. Run as the agent: by the daemon at start with no
/// argument (the direct connection alone), and by `doctor` with the daemon's address, which adds
/// the daemon's own port, name lookups and the proxy's refusals. Fails when any check does.
pub fn netcheck(args: &[String]) -> std::process::ExitCode {
    let wait = Duration::from_secs(2);
    let public: SocketAddr = (Ipv4Addr::new(1, 1, 1, 1), 443).into();
    let mut checks: Vec<(String, Check)> = vec![(
        "a direct connection to the internet is refused".into(),
        direct_refused(public, wait).into(),
    )];
    if let Some(own) = args.first() {
        match own.parse::<SocketAddr>() {
            // Port 0: no attach port to try.
            Ok(own) if own.port() == 0 => {}
            Ok(own) => checks.push((
                format!("the daemon's port {} is refused", own.port()),
                direct_refused(own, wait).into(),
            )),
            Err(_) => checks.push((own.clone(), Check::Fail("not ADDRESS:PORT".into()))),
        }
        checks.push((
            "no name resolves without the proxy".into(),
            unresolved("example.com"),
        ));
        let proxy: SocketAddr = (Ipv4Addr::LOCALHOST, PORT).into();
        for target in ["169.254.169.254:80", "[::ffff:7f00:1]:443"] {
            let check = match proxy_status(proxy, target) {
                Ok(403) => Check::Ok,
                Ok(status) => Check::Fail(format!("it answered {status}")),
                // No proxy, no way out: the daemon may be stopped.
                Err(e) => Check::Warn(format!("the proxy did not answer: {e}")),
            };
            checks.push((format!("the proxy refuses {target}"), check));
        }
        let check = match proxy_status(proxy, "example.com:443") {
            Ok(200) => Check::Ok,
            Ok(status) => Check::Warn(format!("it answered {status}; this machine may be offline")),
            Err(e) => Check::Warn(format!("the proxy did not answer: {e}")),
        };
        checks.push(("the proxy reaches example.com:443".into(), check));
    }
    if let (Some(state), Some(public)) = (args.get(1), args.get(2)) {
        let trust = [
            "/usr/local/share/ca-certificates",
            "/etc/ssl/certs",
            public.as_str(),
        ];
        checks.extend(broker_reach(Path::new(state), &trust));
    }
    let mut code = std::process::ExitCode::SUCCESS;
    for (what, check) in checks {
        match check {
            Check::Ok => println!("ok {what}"),
            Check::Warn(why) => println!("warn {what}: {why}"),
            Check::Fail(why) => {
                println!("fail {what}: {why}");
                code = std::process::ExitCode::FAILURE;
            }
        }
    }
    code
}

/// Fails when `name` resolves, or is still being looked up after 5 s: the agent has no resolver.
fn unresolved(name: &'static str) -> Check {
    use std::net::ToSocketAddrs;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send((name, 80).to_socket_addrs().map(|a| a.count())));
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(found)) if found > 0 => Check::Fail(format!("{name} resolved")),
        Ok(_) => Check::Ok,
        Err(_) => Check::Fail(format!("looking up {name} neither failed nor finished")),
    }
}

/// The status the proxy at `proxy` gives a `CONNECT` to `target`.
fn proxy_status(proxy: SocketAddr, target: &str) -> Result<u16, String> {
    use std::io::{BufRead, Write};
    let io = |e: std::io::Error| e.to_string();
    let mut stream =
        std::net::TcpStream::connect_timeout(&proxy, Duration::from_secs(2)).map_err(io)?;
    // The proxy refuses at once; this bounds a reachable target that is slow to connect, so
    // all of `netcheck` stays under doctor's limit for it.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(io)?;
    let head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(head.as_bytes()).map_err(io)?;
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .map_err(io)?;
    let status = line.strip_prefix("HTTP/1.1 ").and_then(|l| l.get(..3));
    status
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "not an HTTP reply".to_string())
}

/// An address as `/proc/net/tcp` prints it: each 32-bit word in hex as this machine holds it,
/// then the port.
fn proc_addr(text: &str) -> Option<SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| {
        let hex = ip.get(i * 8..i * 8 + 8)?;
        Some(u32::from_str_radix(hex, 16).ok()?.to_ne_bytes())
    };
    let ip: IpAddr = match ip.len() {
        8 => Ipv4Addr::from(word(0)?).into(),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            Ipv6Addr::from(bytes).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Root's sockets the agent could reach, as `ADDR:PORT`: uid-0 TCP listeners and unconnected UDP
/// sockets on loopback or a wildcard address, except the proxy and Docker's DNS (127.0.0.11,
/// which the rules refuse). The rules hold no port list because this list is empty (doctor
/// checks). `files` are `/proc/net`'s `tcp`, `tcp6`, `udp` and `udp6`, by name. A line that
/// cannot be read is listed too.
pub fn exposed(files: &[(&str, &str)]) -> Vec<String> {
    let mut found = Vec::new();
    for (kind, text) in files {
        let tcp = kind.starts_with("tcp");
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let read = || {
                let (local, remote) = (proc_addr(fields.get(1)?)?, proc_addr(fields.get(2)?)?);
                Some((
                    local,
                    remote,
                    *fields.get(3)?,
                    fields.get(7)?.parse::<u32>().ok()?,
                ))
            };
            let Some((local, remote, state, uid)) = read() else {
                found.push(format!("(unreadable: {})", line.trim()));
                continue;
            };
            let listens = match tcp {
                true => state == "0A", // TCP_LISTEN
                false => remote.ip().is_unspecified() && remote.port() == 0,
            };
            let ip = match local.ip() {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(local.ip(), IpAddr::V4),
                v4 => v4,
            };
            let proxy = tcp && ip == IpAddr::V4(Ipv4Addr::LOCALHOST) && local.port() == PORT;
            let dns = ip == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 11));
            if uid == 0 && listens && (ip.is_loopback() || ip.is_unspecified()) && !proxy && !dns {
                found.push(local.to_string());
            }
        }
    }
    found
}

/// Proves the floor before the daemon starts anything for the agent, in bridge mode: the
/// container has an address of its own, the daemon cannot change the rules, and a process run
/// as the agent is refused a direct connection.
pub async fn verify() -> anyhow::Result<()> {
    own_ip()?;
    let status = std::fs::read_to_string("/proc/self/status").context("/proc/self/status")?;
    // Dropping a capability from the bounding set does not take one already inherited.
    anyhow::ensure!(
        net_admin(&status) == Some(false),
        "the daemon still holds NET_ADMIN"
    );
    let mut check = std::process::Command::new("/usr/local/bin/zephyr");
    crate::agent::cmd(&mut check).arg("__netcheck");
    let mut check = tokio::process::Command::from(check);
    let out = tokio::time::timeout(Duration::from_secs(5), check.kill_on_drop(true).output())
        .await
        .context("the network floor check did not finish")?
        .context("running the network floor check")?;
    let said = String::from_utf8_lossy(&out.stdout);
    anyhow::ensure!(
        out.status.success(),
        "the network floor is not in place: {}",
        said.trim()
    );
    Ok(())
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

    // The proxy. 127.0.0.1 counts as public here, so a local listener can stand in for the
    // internet; 127.0.0.2 stays refused.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const WAIT: Duration = Duration::from_secs(10);

    fn local_is_public(ip: IpAddr) -> bool {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    fn local(host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
        let ips: &[&str] = match host.as_str() {
            "upstream.test" => &["127.0.0.1"],
            "rebind.test" => &["127.0.0.1", "127.0.0.2"],
            _ => &[],
        };
        let addrs = ips.iter().map(|i| SocketAddr::new(ip(i), port)).collect();
        Box::pin(async move { Ok(addrs) })
    }

    fn floor() -> Floor {
        Floor {
            public: local_is_public,
            own: ip("10.255.255.1"),
            lookup: local,
            conns: 256,
            head_wait: WAIT,
            connect_wait: WAIT,
            reply_wait: WAIT,
            idle: WAIT,
            upstream: crate::ca::client(crate::ca::public_roots()).unwrap(),
            ca: None,
        }
    }

    /// Whether the connection ends within 3 s, and what came before that.
    async fn ends(s: &mut TcpStream) -> Option<Vec<u8>> {
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut rest));
        read.await.ok().map(|_| rest)
    }

    fn config(dir: &std::path::Path, allow: &str) -> Arc<Config> {
        let text = format!("rpi = '/r'\nhome = {dir:?}\n[network]\nallow = {allow:?}\n");
        Arc::new(toml::from_str(&text).unwrap())
    }

    struct Proxy {
        addr: SocketAddr,
        dir: std::path::PathBuf,
        cfgs: watch::Sender<Arc<Config>>,
    }

    impl Proxy {
        async fn start(name: &str, allow: &str, floor: Floor) -> Proxy {
            let dir = crate::test_dir(name);
            let (cfgs, rx) = watch::channel(config(&dir, allow));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve_on(listener, rx, floor));
            Proxy { addr, dir, cfgs }
        }

        /// Sends `head` and returns the connection with the reply's head.
        async fn ask(&self, head: &str) -> (TcpStream, String) {
            let mut s = TcpStream::connect(self.addr).await.unwrap();
            s.write_all(head.as_bytes()).await.unwrap();
            let reply = head_of(&mut s).await;
            (s, reply)
        }

        /// The proxy's audit rows, once its writer has written `n` of them.
        async fn net_rows(&self, n: usize) -> Vec<(String, String, String)> {
            let store = Store::open(&config(&self.dir, "").db_path()).unwrap();
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let rows = store.audit_rows().unwrap().into_iter();
                let rows: Vec<_> = (rows.filter(|r| r.0 == "net"))
                    .map(|r| (r.1, r.2, r.3))
                    .collect();
                if rows.len() >= n || tokio::time::Instant::now() > deadline {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    /// Reads up to the blank line that ends a head.
    async fn head_of(s: &mut TcpStream) -> String {
        let mut got = Vec::new();
        let read = async {
            while !got.ends_with(b"\r\n\r\n") {
                let byte = s.read_u8().await.expect("the head ended early");
                got.push(byte);
            }
        };
        tokio::time::timeout(WAIT, read)
            .await
            .expect("no head came");
        String::from_utf8(got).unwrap()
    }

    /// A server on 127.0.0.1 that counts its connections. It answers `ping` with `pong`, and
    /// anything else as an HTTP request, whose head it hands back.
    struct Upstream {
        port: u16,
        accepts: Arc<AtomicUsize>,
        heads: tokio::sync::mpsc::UnboundedReceiver<String>,
    }

    async fn upstream() -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let (tx, heads) = tokio::sync::mpsc::unbounded_channel();
        let count = accepts.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut first = [0u8; 4];
                    if s.read_exact(&mut first).await.is_err() {
                        return;
                    }
                    if &first == b"ping" {
                        let _ = s.write_all(b"pong").await;
                        return;
                    }
                    let rest = head_of(&mut s).await;
                    let _ = tx.send(format!("{}{rest}", String::from_utf8_lossy(&first)));
                    // `/stall` never answers; `/half` stops after 2 of its 10 bytes.
                    if rest.starts_with("/stall") {
                        return std::future::pending().await;
                    }
                    let length = if rest.starts_with("/half") { 10 } else { 2 };
                    let reply = format!("HTTP/1.1 200 OK\r\ncontent-length: {length}\r\n\r\nok");
                    let _ = s.write_all(reply.as_bytes()).await;
                    if length > 2 {
                        std::future::pending::<()>().await;
                    }
                });
            }
        });
        Upstream {
            port,
            accepts,
            heads,
        }
    }

    #[tokio::test]
    async fn a_connect_tunnel_carries_bytes_both_ways() {
        let up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let proxy = Proxy::start("egress-tunnel", &dest, floor()).await;
        let (mut s, reply) = proxy.ask(&format!("CONNECT {dest} HTTP/1.1\r\n\r\n")).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        s.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        tokio::time::timeout(WAIT, s.read_exact(&mut pong))
            .await
            .expect("nothing came back")
            .unwrap();
        assert_eq!(&pong, b"pong");
        let row = |tool: &str| {
            (
                tool.to_string(),
                format!("{dest} -> 127.0.0.1"),
                "allowed".to_string(),
            )
        };
        assert_eq!(proxy.net_rows(1).await, [row("connect")]);
    }

    #[tokio::test]
    async fn an_absolute_get_arrives_in_origin_form_without_proxy_headers() {
        let mut up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let proxy = Proxy::start("egress-get", &dest, floor()).await;
        let head = format!(
            "GET http://{dest}/p?q HTTP/1.1\r\nHost: other.example\r\nProxy-Authorization: Basic eDp5\r\n\
             Proxy-Connection: keep-alive\r\nX-Kept: 1\r\n\r\n"
        );
        let (mut s, reply) = proxy.ask(&head).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        let mut body = [0u8; 2];
        s.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"ok");
        let seen = up.heads.recv().await.unwrap().to_lowercase();
        assert!(seen.starts_with("get /p?q http/1.1\r\n"), "{seen}");
        // The host is the one the proxy checked, whatever the request's own header said.
        assert!(seen.contains(&format!("\r\nhost: {dest}\r\n")), "{seen}");
        assert!(seen.contains("\r\nx-kept: 1\r\n"), "{seen}");
        assert!(
            !seen.contains("proxy-") && !seen.contains("other.example"),
            "{seen}"
        );
        assert_eq!(proxy.net_rows(1).await[0].0, "http");
    }

    #[tokio::test]
    async fn a_name_with_one_refused_address_is_refused_whole() {
        let up = upstream().await;
        let dest = format!("rebind.test:{}", up.port);
        let proxy = Proxy::start("egress-rebind", &dest, floor()).await;
        let (_s, reply) = proxy.ask(&format!("CONNECT {dest} HTTP/1.1\r\n\r\n")).await;
        assert!(
            reply.starts_with("HTTP/1.1 403 zephyr: loopback is never allowed\r\n"),
            "{reply}"
        );
        let (_s, reply) = proxy.ask("CONNECT rebind.test:443 HTTP/1.1\r\n\r\n").await;
        assert!(
            reply.starts_with("HTTP/1.1 403 zephyr: private address 127.0.0.2\r\n"),
            "{reply}"
        );
        // The replies are out, so a connection to the public address would have been made by now.
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_embedded_ipv4_literal_is_refused() {
        let real = Floor { public, ..floor() };
        let proxy = Proxy::start("egress-mapped", "", real).await;
        for (literal, shown) in [
            ("[::ffff:127.0.0.1]", "::ffff:127.0.0.1"),
            ("[::ffff:7f00:1]", "::ffff:127.0.0.1"),
            ("[64:ff9b::a9fe:a9fe]", "64:ff9b::a9fe:a9fe"),
            ("[::127.0.0.1]", "::7f00:1"),
            ("2130706433", "127.0.0.1"),
            ("169.254.169.254", "169.254.169.254"),
        ] {
            let (_s, reply) = proxy
                .ask(&format!("CONNECT {literal}:443 HTTP/1.1\r\n\r\n"))
                .await;
            let line = format!("HTTP/1.1 403 zephyr: private address {shown}\r\n");
            assert!(reply.starts_with(&line), "{literal}: {reply}");
        }
    }

    #[tokio::test]
    async fn an_unlisted_port_is_refused_until_the_owner_lists_it() {
        let up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let proxy = Proxy::start("egress-port", "", floor()).await;
        let connect = format!("CONNECT {dest} HTTP/1.1\r\n\r\n");
        let (mut s, reply) = proxy.ask(&connect).await;
        let line = format!(
            "HTTP/1.1 403 zephyr: port {} needs network.allow\r\n",
            up.port
        );
        assert!(reply.starts_with(&line), "{reply}");
        let mut body = String::new();
        s.read_to_string(&mut body).await.unwrap();
        assert!(
            body.contains("propose it with gateway /set network.allow"),
            "{body}"
        );
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
        let refusal = format!("refused: port {} needs network.allow", up.port);
        assert_eq!(
            proxy.net_rows(1).await,
            [("connect".into(), dest.clone(), refusal)]
        );

        proxy.cfgs.send(config(&proxy.dir, &dest)).unwrap();
        let (_s, reply) = proxy.ask(&connect).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    }

    #[tokio::test]
    async fn absolute_form_follows_the_same_rules() {
        let up = upstream().await;
        let proxy = Proxy::start("egress-forms", "", floor()).await;
        let proxy = &proxy;
        let status = |head: String| async move {
            let (_s, reply) = proxy.ask(&head).await;
            reply[..12].to_string()
        };
        let get = format!("GET http://upstream.test:{}/ HTTP/1.1\r\n\r\n", up.port);
        assert_eq!(status(get).await, "HTTP/1.1 403");
        for bad in [
            "GET https://example.com/ HTTP/1.1\r\n\r\n",
            "CONNECT example.com HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: upstream.test\r\n\r\n",
            "GET http://user@upstream.test/ HTTP/1.1\r\n\r\n",
            "GET ftp://upstream.test/ HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(status(bad.into()).await, "HTTP/1.1 400", "{bad}");
        }
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_full_proxy_refuses_at_once() {
        let two = Floor {
            conns: 2,
            ..floor()
        };
        let proxy = Proxy::start("egress-full", "", two).await;
        let _a = TcpStream::connect(proxy.addr).await.unwrap();
        let _b = TcpStream::connect(proxy.addr).await.unwrap();
        let mut third = TcpStream::connect(proxy.addr).await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(1), third.read(&mut [0u8; 1])).await;
        assert!(matches!(closed, Ok(Ok(0) | Err(_))), "{closed:?}");
    }

    #[tokio::test]
    async fn an_unfinished_head_is_closed() {
        let quick = Floor {
            head_wait: Duration::from_millis(200),
            ..floor()
        };
        let proxy = Proxy::start("egress-head", "", quick).await;
        let mut s = TcpStream::connect(proxy.addr).await.unwrap();
        s.write_all(b"CONNECT x").await.unwrap();
        let mut rest = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut rest)).await;
        assert!(closed.is_ok(), "the connection stayed open");
    }

    #[test]
    fn audit_lists_destinations_hourly_and_folds_refusal_floods() {
        let mut a = Audit::default();
        let dest = "example.com:443 -> 1.1.1.1";
        let allowed = [(
            "connect".to_string(),
            dest.to_string(),
            "allowed".to_string(),
        )];
        assert_eq!(a.note(0, "connect", dest, Outcome::Allowed), allowed);
        let other_ip = "example.com:443 -> 1.0.0.1";
        assert_eq!(a.note(3599, "connect", other_ip, Outcome::Allowed), []);
        assert_eq!(a.note(3600, "connect", dest, Outcome::Allowed), allowed);

        let mut a = Audit::default();
        let refuse =
            |a: &mut Audit, now| a.note(now, "http", "x:22", Outcome::Refused("no".into()));
        let rows: usize = (0..61).map(|_| refuse(&mut a, 59).len()).sum();
        assert_eq!(rows, 60);
        let row = |tool: &str, args: &str, outcome: &str| {
            (tool.to_string(), args.to_string(), outcome.to_string())
        };
        // The minute's end writes the count of the rest, with no later request to wait for,
        // once, and under no one request's tool.
        assert_eq!(a.flush(59), None);
        assert_eq!(a.flush(60), Some(row("proxy", "", "refused: 1 more")));
        assert_eq!(a.flush(60), None);
        assert_eq!(refuse(&mut a, 60), [row("http", "x:22", "refused: no")]);
        // A request that comes before any flush closes the minute before it itself.
        let rows: usize = (0..62).map(|_| refuse(&mut a, 119).len()).sum();
        assert_eq!(rows, 59);
        assert_eq!(
            refuse(&mut a, 120),
            [
                row("proxy", "", "refused: 3 more"),
                row("http", "x:22", "refused: no")
            ]
        );
    }

    #[tokio::test]
    async fn audit_rows_carry_no_fact_value() {
        let proxy = Proxy::start("egress-facts", "", floor()).await;
        let fact = "[[fact]]\nname = \"phone\"\nkind = \"digits\"\nvalue = \"5550102345\"\n";
        std::fs::write(crate::facts::path(&config(&proxy.dir, "")), fact).unwrap();
        let (_s, reply) = proxy
            .ask("CONNECT 5550102345.example:443 HTTP/1.1\r\n\r\n")
            .await;
        assert!(reply.starts_with("HTTP/1.1 502 "), "{reply}");
        let rows = proxy.net_rows(1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].2.starts_with("refused: "), "{rows:?}");
        assert!(rows[0].1.contains("[fact:phone]"), "{rows:?}");
        assert!(!format!("{rows:?}").contains("5550102345"), "{rows:?}");
    }

    fn listeners(attach: u16, server_url: &str) -> Config {
        let text = format!(
            "rpi = '/r'\nhome = '/h'\n[attach]\nport = {attach}\n[imessage]\n\
             listen = '100.64.0.1:7791'\nserver_url = '{server_url}'\n\
             password_env = 'P'\nsecret_env = 'S'\n"
        );
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn published_lists_the_configured_ports() {
        let cfg = listeners(7790, "http://100.64.0.2:1234");
        assert_eq!(
            published(&cfg),
            [("attach.port", 7790), ("imessage.listen", 7791)]
        );
        let bare: Config = toml::from_str("rpi = '/r'\nhome = '/h'\n").unwrap();
        assert_eq!(published(&bare), []);
    }

    #[test]
    fn the_web_app_is_a_published_listener() {
        let text = "rpi = '/r'\nhome = '/h'\n[web]\nhost = 'box.ts.net'\n";
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(published(&cfg), [("web.port", 7793)]);
        let moved: Config = toml::from_str(&format!("{text}port = 8000\n")).unwrap();
        assert_eq!(published(&moved), [("web.port", 8000)]);
        // From the file's text: the default port when `[web]` names only its host.
        let file = |port: Option<&'static str>| {
            move |key: &str| match key {
                "web.host" => Some("box.ts.net".to_string()),
                "web.port" => port.map(String::from),
                _ => None,
            }
        };
        assert_eq!(published_from(file(None)), [("web.port", 7793)]);
        assert_eq!(published_from(file(Some("8000"))), [("web.port", 8000)]);
        assert_eq!(published_from(|_| None), []);
    }

    #[test]
    fn published_reads_the_same_keys_from_the_files_text() {
        // As the host reads zephyr.toml: by key, so a key this build does not know is no obstacle.
        let file = |key: &str| match key {
            "attach.port" => Some("7790".to_string()),
            "imessage.listen" => Some("100.64.0.1:7791".to_string()),
            _ => None,
        };
        assert_eq!(
            published_from(file),
            published(&listeners(7790, "http://100.64.0.2:1234"))
        );
        assert_eq!(published_from(|_| None), []);
        // A value that is no port is not published; the daemon refuses that file anyway.
        let odd = |key: &str| (key == "attach.port").then(|| "soon".to_string());
        assert_eq!(published_from(odd), []);
    }

    #[test]
    fn unpublished_names_the_ports_the_container_was_not_made_with() {
        let cfg = listeners(7799, "http://100.64.0.2:1234");
        assert_eq!(unpublished(&cfg, Some("7790,7791")), ["attach.port"]);
        assert!(unpublished(&cfg, Some("7799,7791")).is_empty());
        // A listener added since: its port is not published either.
        assert_eq!(unpublished(&cfg, Some("7799")), ["imessage.listen"]);
        assert_eq!(
            unpublished(&cfg, Some("")),
            ["attach.port", "imessage.listen"]
        );
        // A container that does not say what it publishes is not second-guessed.
        assert!(unpublished(&cfg, None).is_empty());
    }

    #[test]
    fn listen_addr_keeps_the_address_outside_bridge_mode() {
        let addr: SocketAddr = "127.0.0.1:7790".parse().unwrap();
        assert_eq!(listen_addr(addr).unwrap(), addr);
        let tailnet: SocketAddr = "100.64.0.1:7791".parse().unwrap();
        assert_eq!(listen_addr(tailnet).unwrap(), tailnet);
    }

    #[test]
    fn ruleset_matches_the_golden_text() {
        let golden = "\
table inet zephyr
delete table inet zephyr
table inet zephyr {
  set allow {
    type ipv4_addr . inet_service
  }
  chain output {
    type filter hook output priority filter; policy accept;
    meta skuid 1000 jump agent
  }
  chain agent {
    ip saddr 127.0.0.11 reject            # never as Docker's DNS: root's lookups cannot be answered by the agent
    ct state established accept           # replies on connections made to the agent (x11vnc, a dev server the owner opens)
    ip daddr 127.0.0.11 reject            # Docker's embedded DNS: the agent gets no resolver
    ip daddr 127.0.0.0/8 accept           # its own servers, and the proxy at 127.0.0.1:7792
    ip6 daddr ::1 accept
    ip daddr . tcp dport @allow accept    # network.allow, as supervise resolved it (egress::keep_allow)
    meta l4proto tcp reject with tcp reset
    reject with icmpx admin-prohibited
  }
}
";
        assert_eq!(ruleset(1000), golden);
        assert!(ruleset(4242).contains("    meta skuid 4242 jump agent\n"));
    }

    #[test]
    fn direct_refused_accepts_only_a_refusal() {
        let wait = Duration::from_secs(2);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let e = direct_refused(addr, wait).unwrap_err();
        assert!(e.contains("connected"), "{e}");
        drop(listener);
        // A port held but not listening refuses, and no other test can take it meanwhile (a
        // dropped listener's port could be bound again by one running beside this).
        let closed = tokio::net::TcpSocket::new_v4().unwrap();
        closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = closed.local_addr().unwrap();
        assert_eq!(direct_refused(addr, wait), Ok(()));
    }

    #[test]
    fn net_admin_is_read_from_the_effective_set() {
        // Docker's default set, and the same with NET_ADMIN (bit 12) added.
        let status = |eff: &str| {
            format!("Name:\tzephyr\nCapPrm:\t00000000a80435fb\nCapEff:\t{eff}\nCapBnd:\t0\n")
        };
        assert_eq!(net_admin(&status("00000000a80425fb")), Some(false));
        assert_eq!(net_admin(&status("00000000a80435fb")), Some(true));
        assert_eq!(net_admin("Name:\tzephyr\n"), None);
        assert_eq!(net_admin("CapEff:\tnonsense\n"), None);
    }

    /// A `/proc/net/tcp` or `udp` line: local address, remote address, state and uid.
    fn sock(local: &str, remote: &str, state: &str, uid: u32) -> String {
        format!("   0: {local} {remote} {state} 00000000:00000000 00:00000000 00000000  {uid}        0 6417364 1 0000000000000000 100 0 0 10 0\n")
    }

    #[test]
    fn exposed_names_root_listeners_the_agent_could_reach() {
        const HEAD: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let (none4, none6) = ("00000000:0000", "00000000000000000000000000000000:0000");
        let tcp = [
            sock("0100007F:1F90", none4, "0A", 0), // root on 127.0.0.1:8080
            sock("00000000:1E6E", none4, "0A", 0), // root on 0.0.0.0:7790
            sock("0100007F:1E70", none4, "0A", 0), // the proxy
            sock("0100007F:0BB8", none4, "0A", 1000), // the agent's own server
            sock("020012AC:1E6E", none4, "0A", 0), // root on the container's address
            sock("0B00007F:9C41", none4, "0A", 0), // Docker's DNS
            sock("0100007F:C350", "0100007F:1E70", "01", 0), // root's connection, not a listener
        ];
        let tcp6 = [
            sock("00000000000000000000000001000000:1F90", none6, "0A", 0), // root on [::1]:8080
            sock("0000000000000000FFFF00000100007F:1F91", none6, "0A", 0), // and on mapped loopback
            sock("B80D0120000000000000000001000000:1F90", none6, "0A", 0), // 2001:db8::1, not loopback
        ];
        let udp = [
            sock("0B00007F:D431", none4, "07", 0),           // Docker's DNS
            sock("00000000:14E9", none4, "07", 0),           // root bound on 0.0.0.0:5353
            sock("0100007F:D432", "0B00007F:0035", "01", 0), // root's own lookup, connected
        ];
        let text = |lines: &[String]| format!("{HEAD}{}", lines.concat());
        let files = [
            ("tcp", text(&tcp)),
            ("tcp6", text(&tcp6)),
            ("udp", text(&udp)),
            ("udp6", HEAD.to_string()),
        ];
        let files: Vec<(&str, &str)> = files.iter().map(|(k, t)| (*k, t.as_str())).collect();
        assert_eq!(
            exposed(&files),
            [
                "127.0.0.1:8080",
                "0.0.0.0:7790",
                "[::1]:8080",
                "[::ffff:127.0.0.1]:8081",
                "0.0.0.0:5353"
            ]
        );
        // A line that cannot be read is named, never skipped.
        assert_eq!(
            exposed(&[("tcp", "head\n   0: nonsense\n")]),
            ["(unreadable: 0: nonsense)"]
        );
    }

    #[test]
    fn proxy_status_reads_the_status_code() {
        let answering = |reply: &'static str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut s, _) = listener.accept().unwrap();
                let mut head = [0u8; 256];
                let n = s.read(&mut head).unwrap();
                assert!(head[..n].starts_with(b"CONNECT example.com:443 HTTP/1.1\r\n"));
                s.write_all(reply.as_bytes()).unwrap();
            });
            addr
        };
        let status = |reply| proxy_status(answering(reply), "example.com:443");
        assert_eq!(status("HTTP/1.1 403 zephyr: x\r\n\r\n"), Ok(403));
        assert_eq!(status("HTTP/1.1 200 OK\r\n\r\n"), Ok(200));
        assert!(status("SSH-2.0-OpenSSH\r\n").is_err());
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        assert!(proxy_status(addr, "example.com:443").is_err());
    }

    #[test]
    fn the_containers_address_is_its_first_ipv4_one() {
        let of = |addrs: &[&str]| own_of(addrs.iter().map(|a| ip(a)));
        let own = Some("172.18.0.2".parse().unwrap());
        assert_eq!(of(&["fd00::2", "172.18.0.2", "172.19.0.2"]), own);
        assert_eq!(of(&["172.18.0.2"]), own);
        // Never loopback, and never an IPv6 address alone: the rules and the listeners are IPv4.
        assert_eq!(of(&["127.0.0.1", "172.18.0.2"]), None);
        assert_eq!(of(&["fd00::2"]), None);
        assert_eq!(of(&[]), None);
    }

    #[tokio::test]
    async fn a_tunnel_with_nothing_moving_is_closed() {
        let up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let quick = Floor {
            idle: Duration::from_millis(300),
            ..floor()
        };
        let proxy = Proxy::start("egress-idle", &dest, quick).await;
        let (mut s, reply) = proxy.ask(&format!("CONNECT {dest} HTTP/1.1\r\n\r\n")).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert_eq!(
            ends(&mut s).await,
            Some(vec![]),
            "the idle tunnel stayed open"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_answers_gets_a_502() {
        let up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let quick = Floor {
            reply_wait: Duration::from_millis(300),
            ..floor()
        };
        let proxy = Proxy::start("egress-stall", &dest, quick).await;
        let get = format!("GET http://{dest}/stall HTTP/1.1\r\n\r\n");
        let (_s, reply) = proxy.ask(&get).await;
        assert!(
            reply.starts_with("HTTP/1.1 502 zephyr: the server did not answer\r\n"),
            "{reply}"
        );
    }

    #[tokio::test]
    async fn a_reply_that_stops_midway_is_cut() {
        let up = upstream().await;
        let dest = format!("upstream.test:{}", up.port);
        let quick = Floor {
            idle: Duration::from_millis(300),
            ..floor()
        };
        let proxy = Proxy::start("egress-half", &dest, quick).await;
        let get = format!("GET http://{dest}/half HTTP/1.1\r\n\r\n");
        let (mut s, reply) = proxy.ask(&get).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert_eq!(
            ends(&mut s).await,
            Some(b"ok".to_vec()),
            "the stalled reply stayed open"
        );
    }

    #[tokio::test]
    async fn routes_follow_the_allow_list_under_decides_rules() {
        let allow = "imap.gmail.com:993, example.com:22, example.com:2222, 10.0.0.5:5432, \
                     mixed.test:8443, meta.lan:80, loop.lan:8080, self.lan:7790, nowhere.test:25";
        let allow = parse_allow(allow).unwrap();
        let got = routes(&allow, ip(OWN), public, names).await;
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(
            got.pairs,
            [
                (v4("1.1.1.1"), 22),
                (v4("1.1.1.1"), 993),
                (v4("1.1.1.1"), 2222),
                (v4("1.1.1.1"), 8443),
                // A listed entry may be on the LAN, as through the proxy.
                (v4("10.0.0.5"), 5432),
                (v4("10.0.0.5"), 8443),
            ]
        );
        // One line per listed name, on an address that is in the set; a literal needs none.
        assert_eq!(
            got.names,
            [
                (v4("1.1.1.1"), "imap.gmail.com".to_string()),
                (v4("1.1.1.1"), "example.com".to_string()),
                (v4("1.1.1.1"), "mixed.test".to_string()),
            ]
        );
        // What the proxy would refuse gets no route either, and a name with no address none.
        assert_eq!(
            got.problems,
            [
                "meta.lan:80: 169.254.169.254 is never allowed",
                "loop.lan:8080: loopback is never allowed",
                "self.lan:7790: 172.18.0.2 is never allowed",
                "nowhere.test:25: the name has no address",
            ]
        );
        let none = routes(&[], ip(OWN), public, names).await;
        assert!(none.pairs.is_empty() && none.names.is_empty() && none.problems.is_empty());
    }

    #[test]
    fn the_allow_set_is_replaced_whole() {
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(
            allow_script(&[(v4("1.1.1.1"), 993), (v4("10.0.0.5"), 5432)]),
            "flush set inet zephyr allow\nadd element inet zephyr allow { 1.1.1.1 . 993, 10.0.0.5 . 5432 }\n"
        );
        assert_eq!(allow_script(&[]), "flush set inet zephyr allow\n");
    }

    #[test]
    fn hosts_lines_are_replaced_and_everything_else_is_kept() {
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let docker = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost\n172.18.0.2\tzephyr\n";
        let one = hosts_with(docker, &[(v4("1.1.1.1"), "imap.gmail.com".into())]);
        assert_eq!(
            one,
            format!("{docker}1.1.1.1\timap.gmail.com\t# zephyr network.allow\n")
        );
        // A refresh replaces the old lines; with nothing listed the file is Docker's again.
        let two = hosts_with(&one, &[(v4("8.8.8.8"), "github.com".into())]);
        assert_eq!(
            two,
            format!("{docker}8.8.8.8\tgithub.com\t# zephyr network.allow\n")
        );
        assert_eq!(hosts_with(&two, &[]), docker);
        assert_eq!(hosts_with(docker, &[]), docker);
    }

    #[test]
    fn dns_asks_for_a_names_ipv4_addresses_and_reads_only_its_own_answer() {
        let query = dns_query("example.com", 0xbeef).unwrap();
        assert_eq!(
            query,
            b"\xbe\xef\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01"
        );
        assert!(dns_query("a..b", 1).is_none());
        assert!(dns_query(&"x".repeat(64), 1).is_none());

        // The question back, a CNAME, then two A records, names compressed as servers send them.
        let mut reply = b"\xbe\xef\x81\x80\x00\x01\x00\x03\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01".to_vec();
        reply.extend(b"\xc0\x0c\x00\x05\x00\x01\x00\x00\x00\x3c\x00\x06\x03www\xc0\x0c");
        reply.extend(b"\xc0\x29\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x01\x02\x03\x04");
        reply.extend(b"\xc0\x29\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x05\x06\x07\x08");
        let ips = [
            "1.2.3.4".parse::<Ipv4Addr>().unwrap(),
            "5.6.7.8".parse().unwrap(),
        ];
        assert_eq!(dns_answers(&reply, 0xbeef), Some(ips.to_vec()));
        // Another query's answer, a query, an error and a cut-off packet are not answers.
        assert_eq!(dns_answers(&reply, 0xbeee), None);
        let mut question = reply.clone();
        question[2] = 0x01;
        assert_eq!(dns_answers(&question, 0xbeef), None);
        let mut nxdomain = reply.clone();
        nxdomain[3] = 0x83;
        assert_eq!(dns_answers(&nxdomain, 0xbeef), None);
        for cut in 0..reply.len() {
            assert_eq!(dns_answers(&reply[..cut], 0xbeef), None, "cut at {cut}");
        }
    }

    #[test]
    fn the_brokers_keys_and_trust_are_checked_as_the_caller() {
        let dir = crate::test_dir("egress-broker-reach");
        let (state, open, closed) = (dir.join("state"), dir.join("open"), dir.join("nope"));
        std::fs::create_dir_all(state.join("ca")).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        let fails = |checks: Vec<(String, Check)>| -> Vec<bool> {
            let failed = |c: &(String, Check)| matches!(c.1, Check::Fail(_));
            checks.iter().map(failed).collect()
        };
        // Keys that are not there cannot be read; a folder that is not there takes no file.
        let none = broker_reach(&state, &[closed.to_str().unwrap()]);
        assert_eq!(fails(none), [false, false, false]);
        // A key this user can open, and a folder it can write: both are failures.
        std::fs::write(state.join("ca/ca.key"), "k").unwrap();
        let reached = broker_reach(&state, &[open.to_str().unwrap()]);
        assert_eq!(fails(reached), [true, false, true]);
        assert_eq!(
            std::fs::read_dir(&open).unwrap().count(),
            0,
            "the probe is removed"
        );
    }

    /// The token broker's path through the proxy (DESIGN §4.21): a host a token is bound to is
    /// decrypted and swapped, and a stand-in is refused wherever else the proxy can see it.
    mod decrypted {
        use super::*;
        use crate::broker;
        use crate::ca::tests::test_ca;
        use rustls::pki_types::ServerName;
        use tokio::io::{AsyncRead, AsyncWrite};
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        const A: &str = "real-token-A-123456";
        const B: &str = "real-token-B-654321";
        type Roots = Arc<rustls::RootCertStore>;
        type Stream = tokio_rustls::client::TlsStream<TcpStream>;

        static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

        /// Every `.test` name is 127.0.0.1; `rebind.test` also has an address that is refused,
        /// and lookups of `counted.test` are counted.
        fn names(host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
            if host == "counted.test" {
                LOOKUPS.fetch_add(1, Ordering::SeqCst);
            }
            let ips: &[&str] = match host.as_str() {
                "rebind.test" => &["127.0.0.1", "127.0.0.2"],
                h if h.ends_with(".test") => &["127.0.0.1"],
                _ => &[],
            };
            let addrs = ips.iter().map(|i| SocketAddr::new(ip(i), port)).collect();
            Box::pin(async move { Ok(addrs) })
        }

        /// Reads up to the blank line that ends a head; `None` when the stream ends first.
        async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Option<String> {
            let mut got = Vec::new();
            while !got.ends_with(b"\r\n\r\n") {
                let byte = tokio::time::timeout(WAIT, s.read_u8()).await.ok()?.ok()?;
                got.push(byte);
            }
            String::from_utf8(got).ok()
        }

        /// Reads until what came holds `want`.
        async fn read_until<S: AsyncRead + Unpin>(s: &mut S, want: &str) -> String {
            let mut got = Vec::new();
            while !String::from_utf8_lossy(&got).contains(want) {
                let byte = tokio::time::timeout(WAIT, s.read_u8()).await;
                got.push(byte.expect("nothing more came").expect("the stream ended"));
            }
            String::from_utf8_lossy(&got).into_owned()
        }

        /// A TLS server on 127.0.0.1 standing for a host on the internet. It hands back each
        /// request's head. `/events` sends three events, the next only once `go` is notified;
        /// `/upload` hands back a chunked body as it arrives; `/ws` switches protocols and
        /// answers in upper case; `/redirect` repeats its target in `Location`.
        struct Server {
            port: u16,
            accepts: Arc<AtomicUsize>,
            heads: tokio::sync::mpsc::UnboundedReceiver<String>,
            bodies: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
            go: Arc<tokio::sync::Notify>,
        }

        async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(
            mut s: S,
            heads: tokio::sync::mpsc::UnboundedSender<String>,
            bodies: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
            go: Arc<tokio::sync::Notify>,
        ) -> Option<()> {
            loop {
                let head = read_head(&mut s).await?;
                let _ = heads.send(head.clone());
                let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                if path.starts_with("/events") {
                    let start = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 transfer-encoding: chunked\r\n\r\n";
                    s.write_all(start.as_bytes()).await.ok()?;
                    for n in 1..=3 {
                        let event = format!("data: {n}\n\n");
                        let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                        s.write_all(chunk.as_bytes()).await.ok()?;
                        s.flush().await.ok()?;
                        if n < 3 {
                            go.notified().await;
                        }
                    }
                    s.write_all(b"0\r\n\r\n").await.ok()?;
                    continue;
                }
                // What the request's `x-key` header held, sent back: cut in two chunks in the
                // middle of it, the second only once `go` is notified.
                let named = |name: &str| head.lines().find_map(|l| l.strip_prefix(name));
                let sent = named("x-key: ").or(named("authorization: "));
                let sent = sent.unwrap_or_default().to_string();
                // The same in the status line's own words, in headers of this hop only, and
                // as a part of a body.
                let other = match path.as_str() {
                    "/reason" => Some(format!(
                        "HTTP/1.1 200 {sent}\r\ncontent-length: 2\r\n\r\nok"
                    )),
                    "/hop" => Some(format!(
                        "HTTP/1.1 200 OK\r\nconnection: x-hop\r\nx-hop: {sent}\r\n\
                         keep-alive: timeout=5\r\ncontent-length: 2\r\n\r\nok"
                    )),
                    "/partial" => Some(
                        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-1/9\r\n\
                         content-length: 2\r\n\r\nre"
                            .to_string(),
                    ),
                    _ => None,
                };
                if let Some(reply) = other {
                    s.write_all(reply.as_bytes()).await.ok()?;
                    s.flush().await.ok()?;
                    continue;
                }
                // A reply that ends its connection, and one with a trailer after its body.
                if path == "/close" {
                    let reply =
                        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok";
                    s.write_all(reply.as_bytes()).await.ok()?;
                    s.flush().await.ok()?;
                    return None;
                }
                if path == "/trailers" {
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ntrailer: x-sum\r\n\r\n\
                         2\r\nok\r\n0\r\nx-sum: {sent}\r\n\r\n"
                    );
                    s.write_all(reply.as_bytes()).await.ok()?;
                    s.flush().await.ok()?;
                    continue;
                }
                if path.starts_with("/echo-chunked") {
                    let start = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
                    s.write_all(start.as_bytes()).await.ok()?;
                    let (one, two) = sent.split_at(sent.len() / 2);
                    for (i, part) in [format!("you sent {one}"), format!("{two} ok")]
                        .iter()
                        .enumerate()
                    {
                        let chunk = format!("{:x}\r\n{part}\r\n", part.len());
                        s.write_all(chunk.as_bytes()).await.ok()?;
                        s.flush().await.ok()?;
                        if i == 0 {
                            go.notified().await;
                        }
                    }
                    s.write_all(b"0\r\n\r\n").await.ok()?;
                    continue;
                }
                if path.starts_with("/echo-length") || path.starts_with("/gzip") {
                    let coding = match path.starts_with("/gzip") {
                        true => "content-encoding: gzip\r\n",
                        false => "",
                    };
                    let text = format!("key={sent}");
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nx-echo: {sent}\r\n{coding}content-length: {}\r\n\r\n{text}",
                        text.len()
                    );
                    s.write_all(reply.as_bytes()).await.ok()?;
                    s.flush().await.ok()?;
                    continue;
                }
                if path.starts_with("/ws") {
                    let switch = "HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\n\
                                  upgrade: websocket\r\n\r\n";
                    s.write_all(switch.as_bytes()).await.ok()?;
                    let mut buf = [0u8; 64];
                    loop {
                        let n = s.read(&mut buf).await.ok().filter(|n| *n > 0)?;
                        s.write_all(&buf[..n].to_ascii_uppercase()).await.ok()?;
                        s.flush().await.ok()?;
                    }
                }
                if path.starts_with("/upload") {
                    let (mut all, mut buf) = (Vec::new(), [0u8; 1024]);
                    while !all.ends_with(b"0\r\n\r\n") {
                        let n = s.read(&mut buf).await.ok().filter(|n| *n > 0)?;
                        all.extend_from_slice(&buf[..n]);
                        let _ = bodies.send(buf[..n].to_vec());
                    }
                }
                let reply = match path.starts_with("/redirect") {
                    true => format!(
                        "HTTP/1.1 302 Found\r\nlocation: https://bound.test{path}\r\n\
                         content-length: 0\r\n\r\n"
                    ),
                    false => "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_string(),
                };
                s.write_all(reply.as_bytes()).await.ok()?;
                s.flush().await.ok()?;
            }
        }

        async fn server(tls: Arc<rustls::ServerConfig>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let accepts = Arc::new(AtomicUsize::new(0));
            let (heads_tx, heads) = tokio::sync::mpsc::unbounded_channel();
            let (bodies_tx, bodies) = tokio::sync::mpsc::unbounded_channel();
            let go = Arc::new(tokio::sync::Notify::new());
            let (count, wait) = (accepts.clone(), go.clone());
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    count.fetch_add(1, Ordering::SeqCst);
                    let (tls, heads, bodies) = (tls.clone(), heads_tx.clone(), bodies_tx.clone());
                    let go = wait.clone();
                    tokio::spawn(async move {
                        if let Ok(s) = TlsAcceptor::from(tls).accept(tcp).await {
                            serve_one(s, heads, bodies, go).await;
                        }
                    });
                }
            });
            Server {
                port,
                accepts,
                heads,
                bodies,
                go,
            }
        }

        /// The proxy with a CA of its own, a server that is `bound.test` under roots standing
        /// for the public web's, and what the agent's tools trust (the proxy's CA).
        struct Net {
            proxy: Proxy,
            up: Server,
            /// `bound.test:<port>`.
            dest: String,
            /// What the agent's tools trust: the proxy's CA.
            trust: Roots,
            /// What the proxy trusts upstream: roots standing for the public web.
            web_roots: Roots,
        }

        impl Net {
            /// `allow` lists further `host:port`s; `{port}` in it is the bound server's.
            async fn start(name: &str, allow: &str, with_ca: bool) -> Net {
                let (web, web_roots) = test_ca(&format!("{name}-web"));
                let (ca, trust) = test_ca(&format!("{name}-ca"));
                let up = server(web.leaf("bound.test", crate::now()).unwrap()).await;
                let dest = format!("bound.test:{}", up.port);
                let floor = Floor {
                    lookup: names,
                    upstream: crate::ca::client(web_roots.clone()).unwrap(),
                    ca: with_ca.then(|| Arc::new(ca)),
                    ..floor()
                };
                let allow = format!("{dest},{allow}").replace("{port}", &up.port.to_string());
                let proxy = Proxy::start(name, allow.trim_end_matches(','), floor).await;
                Net {
                    proxy,
                    up,
                    dest,
                    trust,
                    web_roots,
                }
            }

            /// Saves the vault's tokens, `(name, value, hosts, url)`, and returns their
            /// stand-ins.
            fn vault(&self, tokens: &[(&str, &str, &[&str], bool)]) -> Vec<String> {
                let mut text = String::new();
                for (name, value, hosts, url) in tokens {
                    text += &format!(
                        "[[token]]\nname = {name:?}\nvalue = {value:?}\nhosts = {hosts:?}\nurl = {url}\nheaders = [\"X-Key\"]\n"
                    );
                }
                std::fs::write(self.proxy.dir.join("vault.toml"), text).unwrap();
                let key = broker::key(&config(&self.proxy.dir, "")).unwrap();
                let stand_in =
                    |t: &(&str, &str, &[&str], bool)| broker::surrogate(&key, "token", t.0);
                tokens.iter().map(stand_in).collect()
            }

            /// `CONNECT dest`, then TLS as `sni` with `config`.
            async fn connect(
                &self,
                dest: &str,
                sni: &str,
                config: Arc<rustls::ClientConfig>,
            ) -> Result<Stream, String> {
                let ask = format!("CONNECT {dest} HTTP/1.1\r\n\r\n");
                let (tcp, reply) = self.proxy.ask(&ask).await;
                if !reply.starts_with("HTTP/1.1 200") {
                    return Err(reply);
                }
                let name = ServerName::try_from(sni.to_string()).unwrap();
                let tls = TlsConnector::from(config).connect(name, tcp).await;
                tls.map_err(|e| e.to_string())
            }

            /// A decrypted connection to the bound server, as a tool of the agent's makes it.
            async fn bound(&self) -> Stream {
                let config = crate::ca::client(self.trust.clone()).unwrap();
                self.connect(&self.dest, "bound.test", config)
                    .await
                    .unwrap()
            }

            /// The head the bound server read next.
            async fn head(&mut self) -> String {
                let head = tokio::time::timeout(WAIT, self.up.heads.recv()).await;
                head.expect("the server read no request").unwrap()
            }

            fn rows(&self) -> Vec<(String, String, String)> {
                let store = Store::open(&config(&self.proxy.dir, "").db_path()).unwrap();
                let rows = store.audit_rows().unwrap().into_iter();
                (rows.filter(|r| r.0 == "net"))
                    .map(|r| (r.1, r.2, r.3))
                    .collect()
            }
        }

        /// Sends `request` and returns the reply's head. A body with a length is read too, so
        /// the connection is ready for the next request.
        async fn ask<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, request: &str) -> String {
            s.write_all(request.as_bytes()).await.unwrap();
            let head = read_head(s).await.expect("no reply came");
            let length = head.lines().find_map(|l| {
                let (name, value) = l.split_once(':')?;
                (name.eq_ignore_ascii_case("content-length")).then(|| value.trim().parse().ok())?
            });
            let mut body = vec![0u8; length.unwrap_or(0)];
            s.read_exact(&mut body).await.unwrap();
            head
        }

        /// A chunked body to its end, without the framing.
        async fn chunked<S: AsyncRead + Unpin>(s: &mut S) -> String {
            let mut text = String::new();
            loop {
                let size = read_until(s, "\r\n").await;
                let size = usize::from_str_radix(size.trim(), 16).expect("a chunk size");
                let mut chunk = vec![0u8; size + 2];
                s.read_exact(&mut chunk).await.unwrap();
                text.push_str(&String::from_utf8_lossy(&chunk[..size]));
                if size == 0 {
                    return text;
                }
            }
        }

        /// `GET target` for the bound server with `headers`, and the reply's head.
        async fn get(s: &mut Stream, net: &Net, target: &str, headers: &str) -> String {
            let (head, _) = get_body(s, net, target, headers).await;
            head
        }

        /// As `get`, with the reply's body. A decrypted host's reply comes chunked: its length
        /// is not known once values in it may be turned back into stand-ins.
        async fn get_body(
            s: &mut Stream,
            net: &Net,
            target: &str,
            headers: &str,
        ) -> (String, String) {
            let request = format!(
                "GET {target} HTTP/1.1\r\nHost: {}\r\n{headers}\r\n",
                net.dest
            );
            let head = ask(s, &request).await;
            let body = match head.to_lowercase().contains("transfer-encoding: chunked") {
                true => chunked(s).await,
                false => String::new(),
            };
            (head, body)
        }

        #[tokio::test]
        async fn a_bound_host_gets_the_real_token_and_never_the_stand_in() {
            let mut net = Net::start("broker-swap", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[
                ("A", A, &[dest.as_str()], false),
                ("B", B, &[dest.as_str()], true),
            ]);
            let mut tls = net.bound().await;
            let reply = get(
                &mut tls,
                &net,
                "/repos",
                &format!("Authorization: Bearer {}\r\n", s[0]),
            )
            .await;
            assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
            let head = net.head().await;
            assert!(
                head.contains(&format!("authorization: Bearer {A}\r\n")),
                "{head}"
            );
            assert!(!head.contains("zsur_"), "{head}");
            // The same connection again: `git` sends its token as a Basic password.
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD;
            let basic = b64.encode(format!("x-access-token:{}", s[0]));
            get(
                &mut tls,
                &net,
                "/info/refs",
                &format!("Authorization: Basic {basic}\r\n"),
            )
            .await;
            let head = net.head().await;
            let want = b64.encode(format!("x-access-token:{A}"));
            assert!(
                head.contains(&format!("authorization: Basic {want}\r\n")),
                "{head}"
            );
            // A token with `url` goes into the target, and a reply that repeats it gets the
            // stand-in back.
            let reply = get(&mut tls, &net, &format!("/redirect?key={}", s[1]), "").await;
            let head = net.head().await;
            assert!(
                head.starts_with(&format!("GET /redirect?key={B} HTTP/1.1")),
                "{head}"
            );
            assert!(
                reply.contains(&format!(
                    "location: https://bound.test/redirect?key={}\r\n",
                    s[1]
                )),
                "{reply}"
            );
            assert!(!reply.contains(B), "{reply}");
            // Three requests, one connection to the server.
            assert_eq!(net.up.accepts.load(Ordering::SeqCst), 1);
            // One row for where it went, and one per token there, by name, for the hour.
            let rows = net.proxy.net_rows(3).await;
            let row = |tool: &str, args: String| (tool.to_string(), args, "allowed".to_string());
            assert_eq!(
                rows,
                [
                    row("connect", format!("{dest} -> 127.0.0.1")),
                    row("swap", format!("A -> {dest}")),
                    row("swap", format!("B -> {dest}")),
                ]
            );
        }

        #[tokio::test]
        async fn an_unbound_host_is_a_plain_tunnel_that_carries_the_stand_in_as_it_is() {
            let net = Net::start("broker-unbound", "other.test:{port}", true).await;
            let s = net.vault(&[("A", A, &["elsewhere.test:443"], false)]);
            // The same server, asked for by a name nothing is bound to.
            let other = format!("other.test:{}", net.up.port);
            // Not decrypted: the proxy's CA is not what signed what the client sees.
            let ours = crate::ca::client(net.trust.clone()).unwrap();
            assert!(net.connect(&other, "bound.test", ours).await.is_err());
            // It is the server's own certificate, end to end.
            let webs = crate::ca::client(net.web_roots.clone()).unwrap();
            let mut tls = net.connect(&other, "bound.test", webs).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nHost: other.test\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            assert!(ask(&mut tls, &request).await.starts_with("HTTP/1.1 200 "));
            // What the proxy cannot see arrives: the stand-in, worth nothing there, and never
            // the token (§4.6 of the spec).
            let mut net = net;
            let head = loop {
                let head = net.head().await;
                if head.contains("x-key") || head.contains("X-Key") {
                    break head;
                }
            };
            assert!(head.contains(&s[0]) && !head.contains(A), "{head}");
        }

        #[tokio::test]
        async fn a_chunked_upload_and_an_event_stream_pass_unbuffered() {
            let mut net = Net::start("broker-stream", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let head = format!(
                "POST /upload HTTP/1.1\r\nHost: {dest}\r\nX-Key: {}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n",
                s[0]
            );
            tls.write_all(head.as_bytes()).await.unwrap();
            // The first chunk is at the server before the second is sent at all.
            let mut got = Vec::new();
            while !String::from_utf8_lossy(&got).contains("first") {
                let part = tokio::time::timeout(WAIT, net.up.bodies.recv()).await;
                got.extend(part.expect("the upload was held back").unwrap());
            }
            assert!(!String::from_utf8_lossy(&got).contains("second"));
            let reply = ask(&mut tls, "6\r\nsecond\r\n0\r\n\r\n").await;
            assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
            assert_eq!(chunked(&mut tls).await, "ok");
            assert!(net.head().await.contains(&format!("x-key: {A}\r\n")));
            // Each event reaches the client while the server still waits to send the next.
            let events = format!("GET /events HTTP/1.1\r\nHost: {dest}\r\n\r\n");
            let reply = ask(&mut tls, &events).await;
            assert!(reply.contains("text/event-stream"), "{reply}");
            read_until(&mut tls, "data: 1\n\n").await;
            net.up.go.notify_one();
            let more = read_until(&mut tls, "data: 2\n\n").await;
            assert!(!more.contains("data: 3"), "{more}");
            net.up.go.notify_one();
            read_until(&mut tls, "data: 3\n\n").await;
        }

        /// A websocket's frames would pass unread, so none is carried on a decrypted host: the
        /// `Upgrade` is this hop's and goes no further, and a server that switches protocols
        /// all the same is refused.
        #[tokio::test]
        async fn no_websocket_is_carried_on_a_bound_host() {
            let mut net = Net::start("broker-ws", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let headers = format!(
                "Connection: Upgrade\r\nUpgrade: websocket\r\nAuthorization: Bearer {}\r\n",
                s[0]
            );
            let reply = get(&mut tls, &net, "/ws", &headers).await;
            let want = "HTTP/1.1 502 zephyr: bound.test switched protocols\r\n";
            assert!(reply.starts_with(want), "{reply}");
            let head = net.head().await.to_lowercase();
            assert!(
                !head.contains("upgrade") && !head.contains("connection"),
                "{head}"
            );
        }

        /// The ways a host can send a token back other than as it is, each of which the
        /// review of the first build found open: a Basic credential (base64 of the value), the
        /// status line's own words, a part of a body, and headers meant for one hop.
        #[tokio::test]
        async fn a_token_sent_back_in_another_form_does_not_reach_the_agent() {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD;
            let mut net = Net::start("broker-forms", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let leaked = |text: &str| {
                let forms = crate::vault::base64_forms(A);
                text.contains(A) || forms.iter().any(|f| text.contains(f.as_str()))
            };

            // `curl -u x:$TOKEN`: the server gets the token inside a Basic credential, and an
            // echo of that credential comes back as the one the agent's program wrote.
            let written = b64.encode(format!("x:{}", s[0]));
            let basic = format!("Authorization: Basic {written}\r\n");
            let (head, body) = get_body(&mut tls, &net, "/echo-length", &basic).await;
            let sent = net.head().await;
            let real = b64.encode(format!("x:{A}"));
            assert!(
                sent.contains(&format!("authorization: Basic {real}\r\n")),
                "{sent}"
            );
            assert_eq!(body, format!("key=Basic {written}"));
            assert!(
                head.contains(&format!("x-echo: Basic {written}\r\n")),
                "{head}"
            );
            assert!(!leaked(&head) && !leaked(&body), "{head}{body}");

            // The token in base64 at another offset, as a server that keeps it might send it.
            let blob = b64.encode(format!("{{\"k\":\"{A}\"}}"));
            let (head, body) = get_body(
                &mut tls,
                &net,
                "/echo-length",
                &format!("X-Key: {blob}\r\n"),
            )
            .await;
            net.head().await;
            assert!(!leaked(&head) && !leaked(&body), "{head}{body}");
            assert!(body.contains(&s[0]), "{body}");

            // The status line's own text is the server's: the client gets the usual words.
            let key = format!("X-Key: {}\r\n", s[0]);
            let head = get(&mut tls, &net, "/reason", &key).await;
            net.head().await;
            assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
            assert!(!leaked(&head), "{head}");

            // A range is not asked for, so no value is cut where the client chose.
            let ranged = format!("{key}Range: bytes=4-9\r\nIf-Range: \"x\"\r\n");
            let (_, body) = get_body(&mut tls, &net, "/echo-length", &ranged).await;
            let sent = net.head().await.to_lowercase();
            assert!(!sent.contains("range"), "{sent}");
            assert_eq!(body, format!("key={}", s[0]));

            // What is one hop's stays on that hop, both ways: a header the client names in
            // `Connection` is not carried past the checks, nor the server's to the client.
            let hop = format!(
                "{key}Connection: x-internal, content-length, host\r\nX-Internal: {}\r\nKeep-Alive: timeout=9\r\n\
                 TE: gzip\r\nProxy-Authorization: Bearer {}\r\n",
                s[0], s[0]
            );
            let (head, _) = get_body(&mut tls, &net, "/hop", &hop).await;
            let sent = net.head().await.to_lowercase();
            for gone in [
                "x-internal",
                "keep-alive",
                "te: gzip",
                "proxy-authorization",
                "connection",
            ] {
                assert!(!sent.contains(gone), "{gone} in {sent}");
            }
            assert!(sent.contains("\r\nhost: bound.test"), "{sent}");
            let head = head.to_lowercase();
            assert!(
                !head.contains("x-hop") && !head.contains("keep-alive"),
                "{head}"
            );
            assert!(!leaked(&head), "{head}");

            // A part of a body is not passed on: last, since the reply is dropped unread.
            let head = get(&mut tls, &net, "/partial", &key).await;
            let want = "HTTP/1.1 502 zephyr: bound.test sent a part of a reply\r\n";
            assert!(head.starts_with(want), "{head}");
            let row = (
                "swap".to_string(),
                dest.clone(),
                "refused: bound.test sent a part of a reply".to_string(),
            );
            let deadline = tokio::time::Instant::now() + WAIT;
            while !net.rows().contains(&row) {
                assert!(tokio::time::Instant::now() < deadline, "{:?}", net.rows());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// When the server ends its connection the client's ends with that reply, and says so:
        /// the proxy has no upstream left to send a next request on, so the client must not
        /// be told the connection is kept.
        #[tokio::test]
        async fn a_reply_that_closes_upstream_closes_the_clients_connection() {
            let mut net = Net::start("broker-close", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let request = format!(
                "GET /close HTTP/1.1\r\nHost: {dest}\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            let head = ask(&mut tls, &request).await.to_lowercase();
            assert!(head.starts_with("http/1.1 200 "), "{head}");
            assert!(head.contains("\r\nconnection: close\r\n"), "{head}");
            // The body, and then the end: nothing waits for a request that could not be sent.
            let mut rest = Vec::new();
            let ended = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut rest));
            assert!(ended.await.is_ok(), "the client's connection stayed open");
            assert!(String::from_utf8_lossy(&rest).contains("ok"));
            net.head().await;
            // The next request is a new connection, checked afresh, and works.
            let mut again = net.bound().await;
            assert!(get(&mut again, &net, "/", "")
                .await
                .starts_with("HTTP/1.1 200 "));
        }

        /// A trailer is the message's, not one hop's: it reaches the client, with a value in
        /// it turned back like any header's.
        #[tokio::test]
        async fn trailers_reach_the_client_with_their_values_turned_back() {
            let mut net = Net::start("broker-trailers", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let request = format!(
                "GET /trailers HTTP/1.1\r\nHost: {dest}\r\nTE: trailers\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            let head = ask(&mut tls, &request).await.to_lowercase();
            assert!(head.contains("\r\ntrailer: x-sum\r\n"), "{head}");
            let rest = read_until(&mut tls, "\r\n\r\n").await;
            assert!(rest.contains(&format!("x-sum: {}\r\n", s[0])), "{rest}");
            assert!(!rest.contains(A), "{rest}");
            net.head().await;
        }

        /// A request shaped to be read two ways (a length and a chunked body) reaches the
        /// server as one well-formed request, and what follows it is a request of its own,
        /// checked like any other.
        #[tokio::test]
        async fn a_request_with_two_framings_reaches_the_server_as_one() {
            let mut net = Net::start("broker-smuggle", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let two = format!(
                "POST /upload HTTP/1.1\r\nHost: {dest}\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n\
                 0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: attacker.test\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            tls.write_all(two.as_bytes()).await.unwrap();
            let first = read_head(&mut tls).await.expect("no reply");
            if first.starts_with("HTTP/1.1 400 ") {
                // Refused outright: nothing reached the server.
                assert!(net.up.heads.try_recv().is_err());
                return;
            }
            let sent = net.head().await.to_lowercase();
            assert!(sent.starts_with("post /upload "), "{sent}");
            assert!(!sent.contains("content-length"), "{sent}");
            // What followed it was either never read (the connection is closed after a request
            // framed two ways) or read as a request of its own and refused: it names another
            // host. Either way the server read one request, and no token went out.
            let mut rest = Vec::new();
            let _ = tokio::time::timeout(WAIT, tls.read_to_end(&mut rest)).await;
            let rest = String::from_utf8_lossy(&rest);
            assert!(
                !rest.contains(" 200 ") || rest.contains("Host does not match"),
                "{rest}"
            );
            assert!(
                net.up.heads.try_recv().is_err(),
                "the server read no second request"
            );
        }

        #[tokio::test]
        async fn a_client_offering_h2_gets_http1() {
            let net = Net::start("broker-alpn", "", true).await;
            net.vault(&[("A", A, &[net.dest.as_str()], false)]);
            let mut config = (*crate::ca::client(net.trust.clone()).unwrap()).clone();
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let tls = net
                .connect(&net.dest, "bound.test", Arc::new(config))
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        }

        /// A bound host that sends back what it was sent must not hand the agent the token:
        /// in the reply the agent's program reads, every value bound to that host is its
        /// stand-in again, in a header, in a body with a length, and in a chunked body that
        /// cuts the value in two. A reply that cannot be read is not passed on.
        #[tokio::test]
        async fn an_echo_from_a_bound_host_shows_the_stand_in_never_the_token() {
            let mut net = Net::start("broker-echo", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[
                ("A", A, &[dest.as_str()], false),
                ("B", B, &[dest.as_str()], false),
            ]);
            let mut tls = net.bound().await;
            let key = format!("X-Key: {}\r\nAccept-Encoding: gzip, br\r\n", s[0]);

            // A body with a length, and a header.
            let (head, body) = get_body(&mut tls, &net, "/echo-length", &key).await;
            let sent = net.head().await;
            assert!(sent.contains(&format!("x-key: {A}\r\n")), "{sent}");
            // The server is asked for the bytes as they are, whatever the client offered.
            assert!(sent.contains("accept-encoding: identity\r\n"), "{sent}");
            assert!(!sent.contains("gzip"), "{sent}");
            assert!(head.contains(&format!("x-echo: {}\r\n", s[0])), "{head}");
            assert_eq!(body, format!("key={}", s[0]));
            // Its length changed with it, so the old one is gone.
            assert!(!head.to_lowercase().contains("content-length"), "{head}");
            assert!(!head.contains(A) && !body.contains(A));

            // Chunked, the value cut in two: the first half does not come ahead of the rest.
            let request = format!("GET /echo-chunked HTTP/1.1\r\nHost: {dest}\r\n{key}\r\n");
            let head = ask(&mut tls, &request).await;
            assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
            let early = read_until(&mut tls, "you sent \r\n").await;
            let mut more = [0u8; 64];
            let waited = tokio::time::timeout(Duration::from_millis(300), tls.read(&mut more));
            assert!(waited.await.is_err(), "half a token was passed on");
            net.up.go.notify_one();
            let rest = read_until(&mut tls, "0\r\n\r\n").await;
            let all = format!("{early}{rest}");
            assert!(all.contains(&s[0]) && all.contains(" ok"), "{all}");
            assert!(
                !all.contains(A) && !all.contains(&A[..A.len() / 2]),
                "{all}"
            );
            net.head().await;

            // The other token bound here comes back as its stand-in too, though this request
            // did not carry it: the server may have it from an earlier one.
            let (_, body) = get_body(
                &mut tls,
                &net,
                "/echo-length",
                &format!("X-Key: was {B}\r\n"),
            )
            .await;
            assert_eq!(body, format!("key=was {}", s[1]));
            net.head().await;

            // Compressed all the same: refused, with a row, and none of it passed on.
            let (head, body) = get_body(&mut tls, &net, "/gzip", &key).await;
            let want =
                "HTTP/1.1 502 zephyr: bound.test sent a reply zephyr cannot read (compressed)\r\n";
            assert!(head.starts_with(want), "{head}");
            assert!(!head.contains(A) && !body.contains(A), "{head}{body}");
            let refused = "refused: bound.test sent a reply zephyr cannot read (compressed)";
            let row = ("swap".to_string(), dest.clone(), refused.to_string());
            let deadline = tokio::time::Instant::now() + WAIT;
            while !net.rows().contains(&row) {
                assert!(tokio::time::Instant::now() < deadline, "{:?}", net.rows());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let printed = format!("{:?}", net.rows());
            assert!(
                !printed.contains(A) && !printed.contains("zsur_"),
                "{printed}"
            );
        }

        /// Makes `owner-1` on `discord` the owner last answered, so a notice has someone to go to.
        fn an_owner(dir: &std::path::Path) -> Store {
            let store = Store::open(&config(dir, "").db_path()).unwrap();
            let said = crate::channel::Inbound {
                channel: "discord".into(),
                platform_msg_id: "m1".into(),
                sender: "owner-1".into(),
                text: "hi".into(),
            };
            store.insert_inbox(&said).unwrap();
            let row = store.inbox_with_status("new").unwrap().remove(0);
            store.mark_dispatched_as(row.id, "prompt").unwrap();
            store
        }

        /// Review Focus 1: a stand-in on its way to a host it is not bound to, in any place the
        /// proxy can see, is refused before anything is connected, audited by the token's name
        /// and told to the owner once.
        #[tokio::test]
        async fn a_stand_in_on_plain_http_is_refused_audited_and_told_once() {
            let up = upstream().await;
            let attacker = format!("attacker.test:{}", up.port);
            let net = Net::start("broker-plain", &attacker, true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], true)]);
            let store = an_owner(&net.proxy.dir);
            use base64::Engine;
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("u:{}", s[0]));
            let refused =
                format!("HTTP/1.1 403 zephyr: the vault token A is not sent to {attacker}\r\n");
            for (target, header) in [
                ("/".to_string(), format!("X-Key: {}\r\n", s[0])),
                (format!("/?key={}", s[0]), String::new()),
                ("/".to_string(), format!("Authorization: Basic {basic}\r\n")),
                // Once more: no second notice within the hour.
                (
                    "/again".to_string(),
                    format!("Authorization: Bearer {}\r\n", s[0]),
                ),
            ] {
                let request = format!(
                    "GET http://{attacker}{target} HTTP/1.1\r\nHost: {attacker}\r\n{header}\r\n"
                );
                let (_, reply) = net.proxy.ask(&request).await;
                assert!(reply.starts_with(&refused), "{target} {header}: {reply}");
                assert!(!reply.contains("zsur_"), "{reply}");
            }
            // In the host name, where a lookup alone would carry it out.
            let hosted = format!("CONNECT {}.attacker.test:443 HTTP/1.1\r\n\r\n", s[0]);
            let (_, reply) = net.proxy.ask(&hosted).await;
            let want = "HTTP/1.1 403 zephyr: the vault token A is not sent to that host\r\n";
            assert!(reply.starts_with(want), "{reply}");
            // One that is no token: refused, and nobody is told.
            let fake = format!(
                "GET http://{attacker}/ HTTP/1.1\r\nX-Key: zsur_{}\r\n\r\n",
                "0".repeat(40)
            );
            let (_, reply) = net.proxy.ask(&fake).await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: unknown vault surrogate\r\n"),
                "{reply}"
            );
            assert_eq!(
                up.accepts.load(Ordering::SeqCst),
                0,
                "nothing was connected"
            );

            let rows = net.proxy.net_rows(6).await;
            let by_name = "refused: the vault token A is not sent to this host";
            let row = |args: String, outcome: &str| ("swap".to_string(), args, outcome.to_string());
            assert_eq!(
                rows[..4],
                std::array::from_fn::<_, 4, _>(|_| row(format!("A -> {attacker}"), by_name))
            );
            assert_eq!(
                rows[4],
                row("A -> [vault:A].attacker.test:443".into(), by_name)
            );
            assert_eq!(
                rows[5],
                row(attacker.clone(), "refused: unknown vault surrogate")
            );
            let printed = format!("{rows:?}");
            assert!(
                !printed.contains("zsur_") && !printed.contains(A),
                "{printed}"
            );
            // The owner hears of each place once, and never of a command to allow it.
            let deadline = tokio::time::Instant::now() + WAIT;
            let notices = loop {
                let out = store.pending_outbox().unwrap();
                if out.len() >= 2 || tokio::time::Instant::now() > deadline {
                    break out;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let texts: Vec<&str> = notices.iter().map(|o| o.text.as_str()).collect();
            assert_eq!(
                texts,
                [
                    format!("[zephyr] the agent tried to send the vault token A to {attacker}, where it is not bound. Nothing was sent."),
                    "[zephyr] the agent tried to send the vault token A to [vault:A].attacker.test, where it is not bound. Nothing was sent.".to_string(),
                ]
            );
        }

        /// Review Focus 1, on a decrypted connection: only what is bound to this very host is
        /// swapped, and the server reads none of the rest.
        #[tokio::test]
        async fn on_a_bound_host_another_token_another_host_and_a_url_are_refused() {
            let mut net = Net::start("broker-other", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[
                ("A", A, &[dest.as_str()], false),
                ("B", B, &["elsewhere.test:443"], true),
            ]);
            let mut tls = net.bound().await;
            // Token B's stand-in, to the host token A is bound to.
            let reply = get(&mut tls, &net, "/", &format!("X-Key: {}\r\n", s[1])).await;
            let want = format!("HTTP/1.1 403 zephyr: the vault token B is not sent to {dest}\r\n");
            assert!(reply.starts_with(&want), "{reply}");
            // The bound host's connection, asked to carry a request for another origin.
            let request = format!(
                "GET / HTTP/1.1\r\nHost: attacker.test\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            let reply = ask(&mut tls, &request).await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: Host does not match bound.test\r\n"),
                "{reply}"
            );
            let twice = format!(
                "GET / HTTP/1.1\r\nHost: {dest}\r\nHost: attacker.test\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            let reply = ask(&mut tls, &twice).await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: Host does not match bound.test\r\n"),
                "{reply}"
            );
            // A token the owner did not allow in URLs.
            let reply = get(&mut tls, &net, &format!("/?key={}", s[0]), "").await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: A is not swapped in URLs\r\n"),
                "{reply}"
            );
            // A stand-in in a header that is not a credential's: never swapped into it, and
            // not sent on as it is either.
            let probe = format!("User-Agent: probe {}\r\n", s[0]);
            let reply = get(&mut tls, &net, "/", &probe).await;
            let want = "HTTP/1.1 403 zephyr: A is not swapped in the header user-agent\r\n";
            assert!(reply.starts_with(want), "{reply}");
            // The connection still works, and the server read only this.
            get(&mut tls, &net, "/fine", "").await;
            assert!(net.head().await.starts_with("GET /fine "));
            assert!(net.up.heads.try_recv().is_err());
            let rows = net.proxy.net_rows(6).await;
            assert!(
                rows.contains(&(
                    "swap".into(),
                    format!("A -> {dest}"),
                    "refused: A is not swapped in the header user-agent".into()
                )),
                "{rows:?}"
            );
            assert!(
                rows.contains(&(
                    "swap".into(),
                    format!("B -> {dest}"),
                    "refused: the vault token B is not sent to this host".into()
                )),
                "{rows:?}"
            );
            assert!(
                rows.contains(&(
                    "swap".into(),
                    dest.clone(),
                    "refused: Host does not match bound.test".into()
                )),
                "{rows:?}"
            );
        }

        /// Takes any certificate: a client that does not check, so the proxy's own check of
        /// the name the client asked for is what the test sees.
        #[derive(Debug)]
        struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

        impl rustls::client::danger::ServerCertVerifier for AnyCert {
            fn verify_server_cert(
                &self,
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &[rustls::pki_types::CertificateDer<'_>],
                _: &ServerName<'_>,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                let algs = &self.0.signature_verification_algorithms;
                rustls::crypto::verify_tls12_signature(message, cert, dss, algs)
            }
            fn verify_tls13_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                let algs = &self.0.signature_verification_algorithms;
                rustls::crypto::verify_tls13_signature(message, cert, dss, algs)
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                self.0.signature_verification_algorithms.supported_schemes()
            }
        }

        /// Review Focus 1: a CONNECT to the bound name whose TLS then asks for another.
        #[tokio::test]
        async fn a_client_sni_for_another_name_is_refused() {
            let mut net = Net::start("broker-sni", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let careless = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
                .with_no_client_auth();
            let mut tls = net
                .connect(&dest, "attacker.test", Arc::new(careless))
                .await
                .unwrap();
            let reply = get(&mut tls, &net, "/", &format!("X-Key: {}\r\n", s[0])).await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: Host does not match bound.test\r\n"),
                "{reply}"
            );
            assert!(
                net.up.heads.try_recv().is_err(),
                "the server read no request"
            );
            let rows = net.proxy.net_rows(2).await;
            assert_eq!(
                rows[1],
                (
                    "swap".into(),
                    dest,
                    "refused: Host does not match bound.test".into()
                )
            );
        }

        /// Review Focus 1: DNS that answers something else for a bound name. A private address
        /// in the answer refuses it whole, as for any host; an address that is public but not
        /// the host's cannot prove the name, so no request is read; and a name is resolved once.
        #[tokio::test]
        async fn a_bound_name_that_resolves_elsewhere_gets_no_token() {
            let allow = "rebind.test:{port},counted.test:{port}";
            let mut net = Net::start("broker-rebind", allow, true).await;
            let port = net.up.port;
            let (rebind, counted) = (
                format!("rebind.test:{port}"),
                format!("counted.test:{port}"),
            );
            let s = net.vault(&[("A", A, &[rebind.as_str(), counted.as_str()], false)]);
            let (_, reply) = net
                .proxy
                .ask(&format!("CONNECT {rebind} HTTP/1.1\r\n\r\n"))
                .await;
            assert!(
                reply.starts_with("HTTP/1.1 403 zephyr: loopback is never allowed"),
                "{reply}"
            );
            assert_eq!(net.up.accepts.load(Ordering::SeqCst), 0);
            // The server at the answered address holds a valid certificate, for another name.
            let config = crate::ca::client(net.trust.clone()).unwrap();
            let mut tls = net.connect(&counted, "counted.test", config).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nHost: {counted}\r\nX-Key: {}\r\n\r\n",
                s[0]
            );
            let reply = ask(&mut tls, &request).await;
            let want = "HTTP/1.1 502 zephyr: counted.test did not prove it is counted.test\r\n";
            assert!(reply.starts_with(want), "{reply}");
            // A second request gets the same, and nothing is looked up again.
            assert!(ask(&mut tls, &request).await.starts_with(want));
            assert_eq!(LOOKUPS.load(Ordering::SeqCst), 1);
            assert!(
                net.up.heads.try_recv().is_err(),
                "the server read no request"
            );
        }

        /// Review Focus 2: a CA made in the container, the agent's own included, is not what
        /// the proxy checks an upstream against. Only `Floor::roots` counts, which `live` takes
        /// from the binary (`ca::public_roots`); no file or variable is read on this path.
        #[tokio::test]
        async fn a_ca_the_agent_planted_gains_no_trust() {
            // A server whose certificate for the bound name a CA made here signed.
            let (planted, _) = test_ca("broker-planted-theirs");
            let mut theirs = server(planted.leaf("planted.test", crate::now()).unwrap()).await;
            let dest = format!("planted.test:{}", theirs.port);
            let net = Net::start("broker-planted", &dest, true).await;
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let config = crate::ca::client(net.trust.clone()).unwrap();
            let mut tls = net.connect(&dest, "planted.test", config).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nHost: {dest}\r\nAuthorization: Bearer {}\r\n\r\n",
                s[0]
            );
            let reply = ask(&mut tls, &request).await;
            let want = "HTTP/1.1 502 zephyr: planted.test did not prove it is planted.test\r\n";
            assert!(reply.starts_with(want), "{reply}");
            assert!(
                theirs.heads.try_recv().is_err(),
                "the server read no request"
            );
            let rows = net.proxy.net_rows(2).await;
            let outcome = "refused: planted.test did not prove it is planted.test";
            assert_eq!(rows[1], ("swap".into(), dest, outcome.into()));
            // Nor is the proxy's own CA, which the agent's tools trust, a public one.
            let (ours, _) = test_ca("broker-planted-ours");
            let leaf = ours.leaf("bound.test", crate::now()).unwrap();
            let checked = crate::ca::tests::handshake(
                leaf,
                net.web_roots.clone(),
                "bound.test",
                &["http/1.1"],
            );
            assert!(checked.await.is_err());
        }

        /// Review Focus 4: the map is read for each request, so an unbinding holds on a
        /// connection that was open before it.
        #[tokio::test]
        async fn an_unbind_stops_the_next_request_on_an_open_connection() {
            let mut net = Net::start("broker-unbind", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let header = format!("X-Key: {}\r\n", s[0]);
            assert!(get(&mut tls, &net, "/", &header)
                .await
                .starts_with("HTTP/1.1 200 "));
            net.head().await;
            net.vault(&[("A", A, &[], false)]);
            let reply = get(&mut tls, &net, "/", &header).await;
            let want = format!("HTTP/1.1 403 zephyr: the vault token A is not sent to {dest}\r\n");
            assert!(reply.starts_with(&want), "{reply}");
            assert!(net.up.heads.try_recv().is_err());
        }

        /// Review Focus 5: a vault that stops reading does not turn a decrypted host back into
        /// a plain tunnel, and nothing is swapped from what was read before.
        #[tokio::test]
        async fn an_unreadable_vault_answers_502_on_bound_hosts() {
            let mut net = Net::start("broker-unreadable", "", true).await;
            let dest = net.dest.clone();
            let s = net.vault(&[("A", A, &[dest.as_str()], false)]);
            let mut tls = net.bound().await;
            let header = format!("X-Key: {}\r\n", s[0]);
            assert!(get(&mut tls, &net, "/", &header)
                .await
                .starts_with("HTTP/1.1 200 "));
            net.head().await;
            std::fs::write(net.proxy.dir.join("vault.toml"), "[[token]\nbroken").unwrap();
            let want = "HTTP/1.1 502 zephyr: the vault could not be read\r\n";
            let reply = get(&mut tls, &net, "/", &header).await;
            assert!(reply.starts_with(want), "{reply}");
            let (_, reply) = net
                .proxy
                .ask(&format!("CONNECT {dest} HTTP/1.1\r\n\r\n"))
                .await;
            assert!(reply.starts_with(want), "{reply}");
            assert!(net.up.heads.try_recv().is_err());
        }

        #[tokio::test]
        async fn without_a_ca_bound_hosts_answer_502_and_others_still_tunnel() {
            let net = Net::start("broker-no-ca", "other.test:{port}", false).await;
            net.vault(&[("A", A, &[net.dest.as_str()], false)]);
            let (_, reply) = net
                .proxy
                .ask(&format!("CONNECT {} HTTP/1.1\r\n\r\n", net.dest))
                .await;
            assert!(
                reply.starts_with("HTTP/1.1 502 zephyr: the token broker is down\r\n"),
                "{reply}"
            );
            let other = format!("CONNECT other.test:{} HTTP/1.1\r\n\r\n", net.up.port);
            let (_, reply) = net.proxy.ask(&other).await;
            assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
            assert!(net.rows().is_empty() || !format!("{:?}", net.rows()).contains(A));
        }
    }
}
