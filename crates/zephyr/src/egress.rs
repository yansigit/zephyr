//! The network floor (DESIGN §4.19): the agent's uid has no route out but the proxy here, which
//! connects only to addresses it has checked.

use anyhow::Context;
use futures_util::future::BoxFuture;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, HOST};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

/// Every daemon TCP port `zephyr up` publishes on the host's loopback, by the zephyr.toml key
/// that sets it.
pub fn published(cfg: &Config) -> Vec<(&'static str, u16)> {
    let attach = cfg.attach.as_ref().map(|a| ("attach.port", a.port));
    let imessage = cfg.imessage.as_ref();
    let imessage = imessage.map(|i| ("imessage.listen", i.listen.port()));
    attach.into_iter().chain(imessage).collect()
}

/// The keys whose published port differs between two configs: a container made for `old` does
/// not publish what `new` listens on.
pub fn moved(old: &Config, new: &Config) -> Vec<&'static str> {
    let (old, new) = (published(old), published(new));
    let port =
        |list: &[(&str, u16)], key: &str| list.iter().find(|(k, _)| *k == key).map(|(_, p)| *p);
    let mut keys: Vec<&'static str> = Vec::new();
    for (key, _) in old.iter().chain(&new) {
        if port(&old, key) != port(&new, key) && !keys.contains(key) {
            keys.push(key);
        }
    }
    keys
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

/// What the proxy runs with. Tests pass their own.
#[derive(Clone, Copy)]
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
    /// The `(tool, args, outcome)` rows to write for a request to `dest` (`host:port -> ip`).
    pub fn note(
        &mut self,
        now: i64,
        tool: &str,
        dest: &str,
        o: Outcome,
    ) -> Vec<(String, String, String)> {
        let mut rows = Vec::new();
        if now / 60 != self.minute {
            if let Some(more) = (self.refused.checked_sub(REFUSALS_A_MINUTE)).filter(|m| *m > 0) {
                rows.push((
                    "connect".into(),
                    String::new(),
                    format!("refused: {more} more"),
                ));
            }
            (self.minute, self.refused) = (now / 60, 0);
        }
        let outcome = match o {
            Outcome::Allowed => {
                if now / 3600 != self.hour {
                    self.hour = now / 3600;
                    self.seen.clear();
                }
                let host_port = dest.split(" -> ").next().unwrap_or(dest);
                (self.seen.insert(host_port.into())).then(|| "allowed".to_string())
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

type Body = BoxBody<Bytes, hyper::Error>;

/// One connection's share of the proxy.
#[derive(Clone)]
struct Conn {
    cfgs: watch::Receiver<Arc<Config>>,
    floor: Floor,
    log: Arc<Mutex<(Store, Audit)>>,
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
    let run = async {
        let floor = Floor::live()?;
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
    let log = Arc::new(Mutex::new((store, Audit::default())));
    let slots = Arc::new(Semaphore::new(floor.conns));
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
            floor,
            log: log.clone(),
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
                .header_read_timeout(floor.head_wait)
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
        // Only the addresses just checked, never the name again.
        let Some((upstream, addr)) = connect(&addrs, self.floor.connect_wait).await else {
            let e = Refusal::Unreachable("no address answered".into());
            return self.refuse(&cfg, tool, &dest, e);
        };
        let dest = format!("{dest} -> {}", addr.ip());
        if tool == "connect" {
            self.record(&cfg, tool, &dest, Outcome::Allowed);
            tokio::spawn(self.tunnel(req, upstream));
            return Response::new(body(""));
        }
        match forward(req, upstream).await {
            Ok(reply) => {
                self.record(&cfg, tool, &dest, Outcome::Allowed);
                reply
            }
            Err(_) => {
                let e = Refusal::Unreachable("the server did not answer".into());
                self.refuse(&cfg, tool, &dest, e)
            }
        }
    }

    /// Carries bytes between the client and `upstream` until either side closes.
    async fn tunnel(self, req: Request<Incoming>, mut upstream: TcpStream) {
        if let Ok(client) = hyper::upgrade::on(req).await {
            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut upstream).await;
        }
        drop(self.slot);
    }

    /// The reply to a request that is not carried, with its audit row.
    fn refuse(&self, cfg: &Config, tool: &str, dest: &str, e: Refusal) -> Response<Body> {
        let (status, reason, text) = match e {
            Refusal::Bad(r) => (StatusCode::BAD_REQUEST, r, String::new()),
            Refusal::Denied(r) => (StatusCode::FORBIDDEN, r, HINT.into()),
            Refusal::Unreachable(r) => (StatusCode::BAD_GATEWAY, r, String::new()),
        };
        let mut reply = Response::new(body(&text));
        *reply.status_mut() = status;
        // The reason is in the status line, where curl -v and a library's error show it.
        if let Ok(phrase) = hyper::ext::ReasonPhrase::try_from(format!("zephyr: {reason}")) {
            reply.extensions_mut().insert(phrase);
        }
        self.record(cfg, tool, dest, Outcome::Refused(reason));
        reply
    }

    /// Writes the request's audit rows, if it leaves any, with vault and fact values scrubbed.
    fn record(&self, cfg: &Config, tool: &str, dest: &str, o: Outcome) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let rows = log.1.note(crate::now(), tool, dest, o);
        if rows.is_empty() {
            return;
        }
        let scrub = crate::gate::Scrub::load(cfg);
        for (tool, args, outcome) in rows {
            crate::gate::audit_row(&log.0, "net", &tool, &scrub.text(&args), &outcome);
        }
    }
}

fn body(text: &str) -> Body {
    Full::new(Bytes::from(text.to_owned()))
        .map_err(|never| match never {})
        .boxed()
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
    Ok(sender.send_request(req).await?.map(BodyExt::boxed))
}

/// The rules that leave the agent's uid no route but its own loopback (DESIGN §4.19). The jump
/// is on the agent's uid, so a packet with no socket (a kernel reset) falls through to accept;
/// TCP is reset, so a connect fails at once instead of timing out; and there is no port list,
/// because nothing of root's listens on loopback but the proxy.
pub fn ruleset(agent_uid: u32) -> String {
    format!(
        "\
table inet zephyr
delete table inet zephyr
table inet zephyr {{
  chain output {{
    type filter hook output priority filter; policy accept;
    meta skuid {agent_uid} jump agent
  }}
  chain agent {{
    ct state established accept           # replies on connections made to the agent (x11vnc, a dev server the owner opens)
    ip daddr 127.0.0.11 reject            # Docker's embedded DNS: the agent gets no resolver
    ip daddr 127.0.0.0/8 accept           # its own servers, and the proxy at 127.0.0.1:7792
    ip6 daddr ::1 accept
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
    use std::io::Write;
    use std::process::{Command, Stdio};
    let uid = std::env::var("ZEPHYR_AGENT_UID").context("ZEPHYR_AGENT_UID is not set")?;
    let uid: u32 = uid.parse().context("ZEPHYR_AGENT_UID is not a uid")?;
    anyhow::ensure!(uid != 0, "ZEPHYR_AGENT_UID is root's");
    let mut nft = Command::new("/usr/sbin/nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running /usr/sbin/nft")?;
    let mut stdin = nft.stdin.take().context("nft's stdin")?;
    stdin.write_all(ruleset(uid).as_bytes())?;
    drop(stdin);
    let out = nft.wait_with_output()?;
    let said = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(out.status.success(), "nft: {}", said.trim());
    Ok(())
}

/// CAP_NET_ADMIN's number (linux/capability.h).
const NET_ADMIN: u32 = 12;

/// Drops NET_ADMIN from the calling thread's bounding set, so what it then executes as root
/// cannot change the rules. For `pre_exec`: one syscall, nothing allocated.
pub fn drop_net_admin() -> std::io::Result<()> {
    // SAFETY: a plain syscall with integer arguments.
    match unsafe { libc::prctl(libc::PR_CAPBSET_DROP, NET_ADMIN as libc::c_ulong, 0, 0, 0) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
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
    // Past the proxy's own lookup and connect waits.
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
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
        }
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

        fn net_rows(&self) -> Vec<(String, String, String)> {
            let store = Store::open(&config(&self.dir, "").db_path()).unwrap();
            let rows = store.audit_rows().unwrap().into_iter();
            rows.filter(|r| r.0 == "net")
                .map(|r| (r.1, r.2, r.3))
                .collect()
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
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
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
        assert_eq!(proxy.net_rows(), [row("connect")]);
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
        assert_eq!(proxy.net_rows()[0].0, "http");
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
            proxy.net_rows(),
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
        assert_eq!(
            refuse(&mut a, 60),
            [
                row("connect", "", "refused: 1 more"),
                row("http", "x:22", "refused: no")
            ]
        );
        assert_eq!(refuse(&mut a, 120), [row("http", "x:22", "refused: no")]);
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
        let rows = proxy.net_rows();
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
    fn moved_names_a_changed_port_only() {
        let old = listeners(7790, "http://100.64.0.2:1234");
        assert_eq!(
            moved(&old, &listeners(7799, "http://100.64.0.2:1234")),
            ["attach.port"]
        );
        assert!(moved(&old, &listeners(7790, "http://100.64.0.3:1234")).is_empty());
        // A listener added or removed is published or unpublished too.
        let bare: Config = toml::from_str("rpi = '/r'\nhome = '/h'\n").unwrap();
        assert_eq!(moved(&bare, &old), ["attach.port", "imessage.listen"]);
        assert_eq!(moved(&old, &bare), ["attach.port", "imessage.listen"]);
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
  chain output {
    type filter hook output priority filter; policy accept;
    meta skuid 1000 jump agent
  }
  chain agent {
    ct state established accept           # replies on connections made to the agent (x11vnc, a dev server the owner opens)
    ip daddr 127.0.0.11 reject            # Docker's embedded DNS: the agent gets no resolver
    ip daddr 127.0.0.0/8 accept           # its own servers, and the proxy at 127.0.0.1:7792
    ip6 daddr ::1 accept
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
}
