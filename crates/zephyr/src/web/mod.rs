//! The web app (DESIGN §4.3.3): one hyper listener on 127.0.0.1 behind `tailscale serve`. It
//! checks Host, Origin and the Tailscale header on every request, serves the embedded files, and
//! upgrades `/api/ws` into an attach connection on channel `web`. `passkey` and `push` are pure.

pub mod passkey;
pub mod push;
pub(crate) mod signin;
pub mod vnc;

pub(crate) use signin::random;
pub use signin::{command, cookie, devices, Ceremony};

use crate::attach::{Handles, MAX_LINE};
use crate::config::{Config, Web};
use crate::store::{Store, WebSession};
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Connections served at once; one more is closed as it arrives.
const MAX_CONNS: usize = 32;
/// How long a connection has to send each request head, on every listener (`listen`).
const HEAD_WAIT: Duration = Duration::from_secs(10);
/// How long a request has to send its body once its head is in (`body`).
const BODY_WAIT: Duration = Duration::from_secs(10);
/// Web sockets open at once; one more gets 503.
const MAX_SOCKETS: usize = 4;
/// Requests a socket may send in a minute; one more closes it.
const SOCKET_REQUESTS: u32 = 60;
/// How often an open socket's session is read again (and at once on `Hub::revoke`).
const SESSION_CHECK: Duration = Duration::from_secs(60);
/// The session cookie. `__Host-` binds it to this origin over HTTPS, with `Path=/`.
const COOKIE: &str = "__Host-zephyr";

/// The app, embedded: path, content type, bytes. Nothing in it is private.
pub(crate) const FILES: &[(&str, &str, &[u8])] = &[
    (
        "/",
        "text/html; charset=utf-8",
        include_bytes!("../../assets/web/index.html"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/web/app.js"),
    ),
    (
        "/app.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/web/app.css"),
    ),
    (
        "/sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/web/sw.js"),
    ),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../../assets/web/manifest.webmanifest"),
    ),
    (
        "/icon-192.png",
        "image/png",
        include_bytes!("../../assets/web/icon-192.png"),
    ),
    (
        "/icon-512.png",
        "image/png",
        include_bytes!("../../assets/web/icon-512.png"),
    ),
    (
        "/novnc/LICENSE.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../../assets/web/novnc/LICENSE.txt"),
    ),
    (
        "/novnc/vendor/pako/LICENSE",
        "text/plain; charset=utf-8",
        include_bytes!("../../assets/web/novnc/vendor/pako/LICENSE"),
    ),
];

/// noVNC 1.7.0's `core/` and `vendor/pako` (MPL-2.0 and MIT), as its tag has them, for the
/// Desktop screen (DESIGN §4.3.3): path and bytes, one row per file, each served as JavaScript.
macro_rules! novnc {
    ($($path:literal),* $(,)?) => {
        &[$((
            concat!("/novnc/", $path),
            include_bytes!(concat!("../../assets/web/novnc/", $path)) as &[u8],
        )),*]
    };
}

pub(crate) const NOVNC: &[(&str, &[u8])] = novnc![
    "core/base64.js",
    "core/crypto/aes.js",
    "core/crypto/bigint.js",
    "core/crypto/crypto.js",
    "core/crypto/des.js",
    "core/crypto/dh.js",
    "core/crypto/md5.js",
    "core/crypto/rsa.js",
    "core/decoders/copyrect.js",
    "core/decoders/h264.js",
    "core/decoders/hextile.js",
    "core/decoders/jpeg.js",
    "core/decoders/raw.js",
    "core/decoders/rre.js",
    "core/decoders/tight.js",
    "core/decoders/tightpng.js",
    "core/decoders/zlib.js",
    "core/decoders/zrle.js",
    "core/deflator.js",
    "core/display.js",
    "core/encodings.js",
    "core/inflator.js",
    "core/input/domkeytable.js",
    "core/input/fixedkeys.js",
    "core/input/gesturehandler.js",
    "core/input/keyboard.js",
    "core/input/keysym.js",
    "core/input/keysymdef.js",
    "core/input/util.js",
    "core/input/vkeys.js",
    "core/input/xtscancodes.js",
    "core/ra2.js",
    "core/rfb.js",
    "core/util/browser.js",
    "core/util/cursor.js",
    "core/util/element.js",
    "core/util/events.js",
    "core/util/eventtarget.js",
    "core/util/int.js",
    "core/util/logging.js",
    "core/util/strings.js",
    "core/websock.js",
    "vendor/pako/lib/utils/common.js",
    "vendor/pako/lib/zlib/adler32.js",
    "vendor/pako/lib/zlib/constants.js",
    "vendor/pako/lib/zlib/crc32.js",
    "vendor/pako/lib/zlib/deflate.js",
    "vendor/pako/lib/zlib/gzheader.js",
    "vendor/pako/lib/zlib/inffast.js",
    "vendor/pako/lib/zlib/inflate.js",
    "vendor/pako/lib/zlib/inftrees.js",
    "vendor/pako/lib/zlib/messages.js",
    "vendor/pako/lib/zlib/trees.js",
    "vendor/pako/lib/zlib/zstream.js",
];

const JS: &str = "text/javascript; charset=utf-8";

/// The embedded file at `path`: its content type and bytes.
fn embedded(path: &str) -> Option<(&'static str, &'static [u8])> {
    let app = FILES.iter().find(|f| f.0 == path).map(|f| (f.1, f.2));
    app.or_else(|| NOVNC.iter().find(|f| f.0 == path).map(|f| (JS, f.1)))
}

/// The files' ETag: they change only with the binary.
const ETAG: &str = concat!("\"", env!("CARGO_PKG_VERSION"), "\"");

/// What the listener and the router share: how many web sockets are open, and a counter that
/// moves when a `/web` command ends sessions, so open sockets check theirs at once.
pub struct Hub {
    sockets: AtomicUsize,
    revoked: watch::Sender<u64>,
    /// Enrolments and sign-ins between their start and finish: 2 min each, at most 16.
    ceremonies: passkey::Challenges<Ceremony>,
    limits: std::sync::Mutex<signin::Limits>,
    /// What signed-in sessions are about to sign, each owned by its session.
    signing: passkey::Challenges<passkey::Signed>,
    /// Each session's notification-button presses this minute: at most 10.
    decides: std::sync::Mutex<std::collections::HashMap<[u8; 32], Window>>,
    /// Who holds the agent's desktop, and the takeover's tickets and one socket.
    pub(crate) desktop: Arc<crate::desktop::Control>,
    pub(crate) vnc: vnc::Vnc,
}

/// How long a challenge waits for its signature.
const CHALLENGE_LIFE: Duration = Duration::from_secs(120);
/// Challenges waiting at once, in each queue; one more drops the oldest.
const MAX_CHALLENGES: usize = 16;

impl Hub {
    pub fn new(desktop: Arc<crate::desktop::Control>) -> Arc<Hub> {
        Arc::new(Hub {
            desktop,
            vnc: vnc::Vnc::new(),
            sockets: AtomicUsize::new(0),
            revoked: watch::channel(0).0,
            ceremonies: passkey::Challenges::new(CHALLENGE_LIFE, MAX_CHALLENGES),
            limits: std::sync::Mutex::new(signin::Limits::new()),
            signing: passkey::Challenges::new(CHALLENGE_LIFE, MAX_CHALLENGES),
            decides: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Web sockets open now.
    pub fn connected(&self) -> usize {
        self.sockets.load(Ordering::Relaxed)
    }

    /// Sessions were ended: every socket checks its own again.
    pub fn revoke(&self) {
        self.revoked.send_modify(|n| *n += 1);
    }

    pub fn revocations(&self) -> watch::Receiver<u64> {
        self.revoked.subscribe()
    }

    /// Keeps what `session` is about to sign under `challenge`, and names it.
    pub fn sign(&self, session: [u8; 32], what: passkey::Signed, challenge: [u8; 32]) -> String {
        self.signing.issue(session, what, challenge)
    }

    /// Takes a signing challenge out, for the session it was issued to, once.
    pub fn signed(&self, id: &str, session: [u8; 32]) -> Option<(passkey::Signed, [u8; 32])> {
        self.signing.take(id, session)
    }

    /// Counts a socket in until the guard is dropped; `None` when `MAX_SOCKETS` are open.
    fn open(self: &Arc<Self>) -> Option<Socket> {
        let counted = (self.sockets)
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_SOCKETS).then_some(n + 1)
            })
            .is_ok();
        counted.then(|| Socket(self.clone()))
    }
}

/// One open web socket, counted in its hub.
struct Socket(Arc<Hub>);

impl Drop for Socket {
    fn drop(&mut self) {
        self.0.sockets.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A fixed window: `limit` hits in each `span`, counted from the first hit of the window.
pub struct Window {
    start: Option<Instant>,
    n: u32,
    limit: u32,
    span: Duration,
}

impl Window {
    pub fn new(limit: u32, span: Duration) -> Self {
        Self {
            start: None,
            n: 0,
            limit,
            span,
        }
    }

    /// Counts a hit; `false` past the limit, until the span rolls.
    pub fn hit(&mut self, now: Instant) -> bool {
        if self
            .start
            .is_none_or(|start| now.duration_since(start) >= self.span)
        {
            (self.start, self.n) = (Some(now), 0);
        }
        self.n += 1;
        self.n <= self.limit
    }
}

/// Whether a request may be looked at, before any route or cookie (DESIGN §4.3.3):
/// 1. Host is the configured name, else 421: a rebinding page reaches the port under its own.
/// 2. `tailscale serve` named a tailnet user (one of `tailnet_users`, when set), else 403. The
///    header decides nothing more: whoever reaches the port can send it.
/// 3. A request that changes something, and the WebSocket upgrade, come from the app's own
///    origin, else 403; a changing request is JSON, else 415, which no cross-site form can send.
pub fn admit(method: &Method, path: &str, h: &HeaderMap, web: &Web) -> Result<(), StatusCode> {
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
    if !get("host").is_some_and(|host| host.eq_ignore_ascii_case(&web.host)) {
        return Err(StatusCode::MISDIRECTED_REQUEST);
    }
    let login = get("tailscale-user-login").unwrap_or_default();
    let listed = web.tailnet_users.is_empty()
        || web
            .tailnet_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(login));
    if login.is_empty() || !listed {
        return Err(StatusCode::FORBIDDEN);
    }
    let changes = method == Method::POST || method == Method::DELETE;
    if changes || path == "/api/ws" || path == "/api/vnc" {
        let own = get("origin") == Some(&format!("https://{}", web.host));
        if !own || get("sec-fetch-site").is_some_and(|site| site != "same-origin") {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    if changes {
        let kind = get("content-type").unwrap_or_default();
        let kind = kind.split(';').next().unwrap_or_default().trim();
        if !kind.eq_ignore_ascii_case("application/json") {
            return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
    }
    Ok(())
}

type Res = Response<Full<Bytes>>;

fn text(code: StatusCode, body: &'static str) -> Res {
    let mut res = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *res.status_mut() = code;
    res
}

fn json(code: StatusCode, body: serde_json::Value) -> Res {
    let mut res = Response::new(Full::new(Bytes::from(body.to_string())));
    *res.status_mut() = code;
    let kind = HeaderValue::from_static("application/json");
    res.headers_mut().insert(header::CONTENT_TYPE, kind);
    res
}

/// A refusal the page shows: `{"error": why}`. Never a value from the request.
fn error(code: StatusCode, why: &str) -> Res {
    json(code, serde_json::json!({ "error": why }))
}

/// What every listener task needs.
struct App {
    web: Web,
    handles: Handles,
    hub: Arc<Hub>,
}

impl App {
    fn cfg(&self) -> Arc<Config> {
        self.handles.cfgs.borrow().clone()
    }

    /// The live session a request's cookie names, with the cookie's hash. Only the hash is
    /// ever stored or compared, by SQLite as a key.
    fn session(&self, h: &HeaderMap) -> Option<WebSession> {
        let cookies = h.get(header::COOKIE)?.to_str().ok()?;
        let token = cookies
            .split(';')
            .filter_map(|c| c.trim().split_once('='))
            .find_map(|(name, value)| (name == COOKIE).then_some(value))?;
        let token = data_encoding::BASE64URL_NOPAD
            .decode(token.as_bytes())
            .ok()?;
        if token.len() != 32 {
            return None;
        }
        self.live(&passkey::sha256(&token))
    }

    /// The session with this hash, if it has not ended.
    fn live(&self, hash: &[u8; 32]) -> Option<WebSession> {
        let store = Store::open(&self.cfg().db_path()).ok()?;
        store.web_session(hash, crate::now()).ok().flatten()
    }
}

/// Why the web app does not start under host networking (`floored`).
pub const NEEDS_FLOOR: &str = "the web app needs the network floor; host.toml network = \"bridge\"";

/// Whether the web app may run: only under the network floor (DESIGN §4.20), where the agent's
/// uid cannot reach its port. Under host networking the agent could, and the sign-in limits,
/// which cannot tell one client at the port from another, would be its to use against the owner.
pub fn floored(cfg: &Config) -> bool {
    crate::egress::bridge() || cfg.web_off_floor
}

/// Where the web app listens: the container's own address, which `zephyr up` publishes on the
/// host's loopback for `tailscale serve`. `Err` says why it cannot listen; `None` without `[web]`.
pub fn address(cfg: &Config) -> Result<std::net::SocketAddr, String> {
    let port = cfg.web.as_ref().map_or(0, |web| web.port);
    if !floored(cfg) {
        return Err(NEEDS_FLOOR.into());
    }
    crate::egress::listen_addr(([127, 0, 0, 1], port).into()).map_err(|e| format!("{e:#}"))
}

/// Serves the web app until dropped. Pending forever without `[web]`, and when the port cannot
/// be bound: the daemon runs on without the app, and `doctor` says so.
pub async fn serve(cfg: &Config, handles: Handles, hub: Arc<Hub>) -> anyhow::Result<()> {
    let Some(web) = cfg.web.clone() else {
        return std::future::pending().await;
    };
    if let Err(e) = push::vapid(cfg, true) {
        eprintln!("zephyr: web push is off: {e:#}");
    }
    // An address that cannot be had is the web app's trouble, not the daemon's: it runs on.
    let addr = match address(cfg) {
        Ok(addr) => addr,
        Err(why) => {
            eprintln!("zephyr: web app not started: {why}");
            return std::future::pending().await;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("zephyr: web app not listening on port {}: {e}", web.port);
            return std::future::pending().await;
        }
    };
    eprintln!("zephyr: web app listening on {addr}");
    let app = Arc::new(App { web, handles, hub });
    let svc = {
        let app = app.clone();
        service_fn(move |req| {
            let app = app.clone();
            async move { Ok::<_, Infallible>(handle(req, app).await) }
        })
    };
    // Keep-alive stays on: with it off hyper adds `close` to the 101's `Connection: Upgrade`,
    // which a WebSocket client need not accept.
    tokio::select! {
        () = listen(listener, MAX_CONNS, true, svc) => {}
        // Beside the listener: a takeover nobody ends goes back by itself.
        () = vnc::watchdog(app) => {}
    }
    std::future::pending().await
}

/// Serves `svc` on each connection `listener` takes, for ever: the one loop of the web app,
/// the iMessage webhook and the calls listener. Anything that reaches the port can connect
/// (the agent too, under host networking), so at most `max_conns` are served at once (one
/// more is closed as it arrives) and each request head has `HEAD_WAIT`, which bounds an idle
/// connection too. `keep_alive` false serves one request a connection. Upgrades are served.
pub(crate) async fn listen<S, B>(listener: TcpListener, max_conns: usize, keep_alive: bool, svc: S)
where
    S: hyper::service::Service<Request<Incoming>, Response = Response<B>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let slots = Arc::new(tokio::sync::Semaphore::new(max_conns));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // shortcut: a persistent accept error (e.g. out of fds) retries every 100 ms with no backoff.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue; // dropped, so closed at once
        };
        let svc = svc.clone();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEAD_WAIT)
                .keep_alive(keep_alive)
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await;
            drop(slot);
        });
    }
}

/// A request's body, at most `limit` bytes, within `BODY_WAIT` of its head: hyper bounds only
/// the head, so a body that never came would hold its connection's slot for good. `Err` is
/// the status to answer: 408 for a body too slow, 413 for one too large or cut off.
pub(crate) async fn body(req: Request<Incoming>, limit: usize) -> Result<Bytes, StatusCode> {
    use http_body_util::BodyExt;
    let read = http_body_util::Limited::new(req.into_body(), limit).collect();
    match tokio::time::timeout(BODY_WAIT, read).await {
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT),
        Ok(Err(_)) => Err(StatusCode::PAYLOAD_TOO_LARGE),
        Ok(Ok(body)) => Ok(body.to_bytes()),
    }
}

async fn handle(req: Request<Incoming>, app: Arc<App>) -> Res {
    let path = req.uri().path().to_string();
    let mut res = match admit(req.method(), &path, req.headers(), &app.web) {
        Err(StatusCode::FORBIDDEN) => text(
            StatusCode::FORBIDDEN,
            "open the app through tailscale serve",
        ),
        Err(code) => text(code, ""),
        Ok(()) => route(req, &path, &app).await,
    };
    secure(res.headers_mut(), &app.web.host, path.starts_with("/api/"));
    res
}

/// The most a request's body may hold.
const MAX_BODY: usize = 64 * 1024;

async fn route(req: Request<Incoming>, path: &str, app: &Arc<App>) -> Res {
    const POST: &Method = &Method::POST;
    match (req.method(), path) {
        (&Method::GET, "/api/ws") => ws(req, app),
        (&Method::GET, "/api/vnc") => vnc::ws(req, app),
        (
            POST,
            "/api/enrol/start" | "/api/enrol/finish" | "/api/login/start" | "/api/login/finish",
        ) => {
            // Before the body is read: a closed or flooded sign-in costs nothing more.
            if let Some(res) = app.closed(path != "/api/login/start") {
                return res;
            }
            let body = match json_body(req).await {
                Ok(body) => body,
                Err(res) => return res,
            };
            match path {
                "/api/enrol/start" => app.enrol_start(&body).await,
                "/api/enrol/finish" => app.enrol_finish(&body).await,
                "/api/login/start" => app.login_start(),
                _ => app.login_finish(&body).await,
            }
        }
        (POST, "/api/logout") => app.logout(req.headers()),
        (&Method::GET, "/api/push/key")
        | (POST | &Method::DELETE, "/api/push" | "/api/push/decide") => {
            let Some(session) = app.session(req.headers()) else {
                return error(StatusCode::UNAUTHORIZED, "sign in first");
            };
            if req.method() == Method::GET {
                return app.push_key();
            }
            let add = req.method() == Method::POST;
            let body = match json_body(req).await {
                Ok(body) => body,
                Err(res) => return res,
            };
            match path {
                "/api/push" => app.push_save(add, session.passkey, &body),
                _ if add => app.push_decide(&session.token_hash, &body).await,
                _ => text(StatusCode::NOT_FOUND, ""),
            }
        }
        (&Method::GET, _) => file(&req, path),
        _ => text(StatusCode::NOT_FOUND, ""),
    }
}

/// A request's JSON object, of at most `MAX_BODY` bytes; `Err` is the refusal to answer.
async fn json_body(req: Request<Incoming>) -> Result<serde_json::Value, Res> {
    let malformed = || error(StatusCode::BAD_REQUEST, "the request is malformed");
    let bytes = match body(req, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(StatusCode::REQUEST_TIMEOUT) => {
            return Err(error(
                StatusCode::REQUEST_TIMEOUT,
                "the request took too long",
            ))
        }
        Err(_) => return Err(malformed()),
    };
    (serde_json::from_slice(&bytes).ok())
        .filter(serde_json::Value::is_object)
        .ok_or_else(malformed)
}

fn file(req: &Request<Incoming>, path: &str) -> Res {
    match embedded(path) {
        Some((kind, bytes)) => {
            let fresh = req
                .headers()
                .get(header::IF_NONE_MATCH)
                .is_some_and(|v| v == ETAG);
            let mut res = match fresh {
                true => text(StatusCode::NOT_MODIFIED, ""),
                false => Response::new(Full::new(Bytes::from_static(bytes))),
            };
            let h = res.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
            h.insert(header::ETAG, HeaderValue::from_static(ETAG));
            res
        }
        None => text(StatusCode::NOT_FOUND, ""),
    }
}

/// What answers a WebSocket handshake (`Sec-WebSocket-Accept`); `None` when it is not one.
pub(crate) fn accept_key(h: &HeaderMap) -> Option<HeaderValue> {
    let asks = |name: header::HeaderName, want: &str| {
        (h.get(name).and_then(|v| v.to_str().ok())).is_some_and(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(want))
        })
    };
    if !asks(header::UPGRADE, "websocket") || !asks(header::SEC_WEBSOCKET_VERSION, "13") {
        return None;
    }
    let key = derive_accept_key(h.get(header::SEC_WEBSOCKET_KEY)?.as_bytes());
    HeaderValue::from_str(&key).ok()
}

/// `GET /api/ws`: the attach protocol over a WebSocket, for a signed-in browser (`admit` has
/// checked its Origin). Answers the handshake, then bridges the socket once hyper hands it over.
fn ws(mut req: Request<Incoming>, app: &Arc<App>) -> Res {
    let Some(session) = app.session(req.headers()) else {
        return text(StatusCode::UNAUTHORIZED, "");
    };
    let Some(accept) = accept_key(req.headers()) else {
        return text(StatusCode::BAD_REQUEST, "");
    };
    let Some(socket) = app.hub.open() else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "");
    };
    let (upgrade, app) = (hyper::upgrade::on(&mut req), app.clone());
    tokio::spawn(async move {
        // Fails when the client went away before the 101 reached it.
        if let Ok(upgraded) = upgrade.await {
            bridge(TokioIo::new(upgraded), session, &app).await;
        }
        drop(socket);
    });
    let mut res = text(StatusCode::SWITCHING_PROTOCOLS, "");
    let h = res.headers_mut();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    res
}

/// Joins a WebSocket to an attach connection on channel `web`: each text frame goes in as one
/// line, each line out as one frame. The socket is closed on a binary frame, a frame holding a
/// newline or over 1 MiB, more than 60 frames in a minute, no frame or pong for two ping
/// intervals (a phone that froze the page), and once its session has ended.
async fn bridge<S>(io: S, session: WebSession, app: &App)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let limits = WebSocketConfig {
        max_message_size: Some(MAX_LINE as usize),
        max_frame_size: Some(MAX_LINE as usize),
        ..Default::default()
    };
    let ws = WebSocketStream::from_raw_socket(io, Role::Server, Some(limits)).await;
    let (mut sink, mut frames) = ws.split();
    let (near, far) = tokio::io::duplex(2 * MAX_LINE as usize);
    let origin = crate::attach::Origin::Web {
        session: session.token_hash,
        passkey: session.passkey,
        hub: app.hub.clone(),
    };
    let conn = tokio::spawn(crate::attach::serve_stream(
        far,
        origin,
        app.handles.clone(),
    ));
    let (read, mut write) = tokio::io::split(near);
    let mut lines = tokio::io::BufReader::new(read).lines();
    let ping = app.cfg().web_ping;
    let (mut pings, mut checks) = (
        tokio::time::interval(ping),
        tokio::time::interval(SESSION_CHECK),
    );
    let mut revoked = app.hub.revocations();
    let mut requests = Window::new(SOCKET_REQUESTS, Duration::from_secs(60));
    let mut heard = Instant::now();
    // A send to a phone that stopped reading must not hold the loop past the dead-socket limit.
    let limit = 2 * ping;
    loop {
        let out = tokio::select! {
            frame = frames.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    heard = Instant::now();
                    if text.contains('\n') || !requests.hit(heard) {
                        break;
                    }
                    let line = format!("{text}\n");
                    if write.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    continue;
                }
                // tungstenite answers a ping itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    heard = Instant::now();
                    continue;
                }
                _ => break, // binary, a close, an oversized frame, the end
            },
            line = lines.next_line() => match line {
                Ok(Some(line)) => Message::Text(line),
                _ => break, // the connection ended (the router is going down)
            },
            _ = pings.tick() => {
                if heard.elapsed() > limit {
                    break;
                }
                Message::Ping(vec![])
            }
            _ = checks.tick() => match app.live(&session.token_hash) {
                Some(_) => continue,
                None => break,
            },
            changed = revoked.changed() => match (changed, app.live(&session.token_hash)) {
                (Ok(()), Some(_)) => continue,
                _ => break,
            },
        };
        if !matches!(
            tokio::time::timeout(limit, sink.send(out)).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
    conn.abort();
    let _ = tokio::time::timeout(limit, sink.close()).await;
}

/// The headers every response carries. Nothing in the app is inline, so the CSP needs no nonce.
fn secure(h: &mut HeaderMap, host: &str, api: bool) {
    let csp = format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
         connect-src 'self' wss://{host}; manifest-src 'self'; worker-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    );
    // `Config::validate` keeps the host to letters, digits, '-' and '.'.
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    for (name, value) in [
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            header::CACHE_CONTROL,
            if api { "no-store" } else { "no-cache" },
        ),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web(users: &[&str]) -> crate::config::Web {
        crate::config::Web {
            host: "zephyr.test".into(),
            port: 7793,
            tailnet_users: users.iter().map(|u| u.to_string()).collect(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn admit_checks_host_then_tailnet_then_origin_fetch_site_and_type() {
        const HOST: (&str, &str) = ("host", "zephyr.test");
        const TS: (&str, &str) = ("tailscale-user-login", "owner@github");
        const ORIGIN: (&str, &str) = ("origin", "https://zephyr.test");
        const JSON: (&str, &str) = ("content-type", "application/json");
        let (get, post, delete) = (Method::GET, Method::POST, Method::DELETE);
        let any = web(&[]);
        type Case<'a> = (
            &'a Method,
            &'a str,
            &'a [(&'a str, &'a str)],
            Result<(), u16>,
        );
        let cases: &[Case] = &[
            (&get, "/", &[HOST, TS], Ok(())),
            (&get, "/", &[("host", "Zephyr.Test"), TS], Ok(())),
            (&get, "/", &[("host", "evil.test"), TS, ORIGIN], Err(421)),
            (&get, "/", &[("host", "zephyr.test:443"), TS], Err(421)),
            (&get, "/", &[TS], Err(421)),
            (&get, "/", &[HOST], Err(403)),
            (&get, "/", &[("host", "evil.test")], Err(421)),
            (&get, "/api/vnc", &[HOST, TS, ORIGIN], Ok(())),
            (&get, "/api/vnc", &[HOST, TS], Err(403)),
            (
                &get,
                "/api/vnc",
                &[HOST, TS, ("origin", "https://evil.test")],
                Err(403),
            ),
            (&post, "/api/logout", &[HOST, TS, ORIGIN, JSON], Ok(())),
            (&post, "/api/logout", &[HOST, TS, JSON], Err(403)),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ("origin", "https://evil.test"), JSON],
                Err(403),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ("origin", "http://zephyr.test"), JSON],
                Err(403),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ("origin", "null"), JSON],
                Err(403),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ORIGIN, JSON, ("sec-fetch-site", "same-origin")],
                Ok(()),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ORIGIN, JSON, ("sec-fetch-site", "cross-site")],
                Err(403),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ORIGIN, JSON, ("sec-fetch-site", "same-site")],
                Err(403),
            ),
            (&post, "/api/logout", &[HOST, TS, ORIGIN], Err(415)),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ORIGIN, ("content-type", "text/plain")],
                Err(415),
            ),
            (
                &post,
                "/api/logout",
                &[
                    HOST,
                    TS,
                    ORIGIN,
                    ("content-type", "application/json; charset=utf-8"),
                ],
                Ok(()),
            ),
            (
                &post,
                "/api/logout",
                &[HOST, TS, ORIGIN, ("content-type", "application/jsonx")],
                Err(415),
            ),
            (&delete, "/api/push", &[HOST, TS, JSON], Err(403)),
            (&delete, "/api/push", &[HOST, TS, ORIGIN, JSON], Ok(())),
            (&get, "/api/ws", &[HOST, TS], Err(403)),
            (
                &get,
                "/api/ws",
                &[HOST, TS, ("origin", "https://evil.test")],
                Err(403),
            ),
            (&get, "/api/ws", &[HOST, TS, ORIGIN], Ok(())),
        ];
        for (method, path, pairs, want) in cases {
            let got = admit(method, path, &headers(pairs), &any).map_err(|s| s.as_u16());
            assert_eq!(got, *want, "{method} {path} {pairs:?}");
        }
        let listed = web(&["owner@github"]);
        let as_user = |login| {
            admit(
                &get,
                "/",
                &headers(&[HOST, ("tailscale-user-login", login)]),
                &listed,
            )
        };
        assert_eq!(as_user("Owner@GitHub"), Ok(()));
        assert_eq!(as_user("x@y"), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn a_window_allows_its_limit_then_refuses_until_it_rolls() {
        let start = Instant::now();
        let mut w = Window::new(3, Duration::from_secs(60));
        assert!(w.hit(start) && w.hit(start) && w.hit(start));
        assert!(!w.hit(start));
        assert!(!w.hit(start + Duration::from_secs(59)));
        assert!(w.hit(start + Duration::from_secs(60)), "the span rolled");
        assert!(w.hit(start + Duration::from_secs(61)) && w.hit(start + Duration::from_secs(62)));
        assert!(!w.hit(start + Duration::from_secs(63)));
    }

    #[test]
    fn every_method_the_app_calls_is_open_on_web() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web");
        let js = std::fs::read_to_string(dir.join("app.js")).unwrap();
        let name = |after: &str| after.split('"').next().unwrap_or_default().to_string();
        // What `rpc("…")` is called with, and any other `"zephyr.…"` name in the file.
        let mut called: Vec<String> = js.split("rpc(\"").skip(1).map(name).collect();
        called.extend(
            js.split("\"zephyr.")
                .skip(1)
                .map(|after| format!("zephyr.{}", name(after))),
        );
        for method in [
            "zephyr.activity",
            "zephyr.goals",
            "zephyr.subscribe",
            "send",
            "zephyr.proposals",
            "zephyr.decide",
            "zephyr.web_challenge",
            "zephyr.desktop",
        ] {
            assert!(
                called.iter().any(|c| c == method),
                "the app never calls {method}"
            );
        }
        for method in &called {
            assert!(
                crate::attach::WEB_METHODS.contains(&method.as_str()),
                "{method} is closed to the web app"
            );
        }
        // The service worker's one call is a route, with the JSON type `admit` asks for.
        let sw = std::fs::read_to_string(dir.join("sw.js")).unwrap();
        assert!(
            sw.contains("\"/api/push/decide\"")
                && sw.contains("\"Content-Type\": \"application/json\"")
        );
    }

    #[test]
    fn the_app_holds_no_inline_code() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        // Every script that is served, noVNC's too.
        let app = FILES.iter().filter(|f| f.1 == JS).map(|f| (f.0, f.2));
        let scripts: Vec<_> = app.chain(NOVNC.iter().copied()).collect();
        assert_eq!(scripts.len(), 2 + NOVNC.len());
        for (name, js) in scripts {
            let js = std::str::from_utf8(js).unwrap();
            for banned in [
                "innerHTML",
                "insertAdjacentHTML",
                "outerHTML",
                "eval(",
                "new Function",
            ] {
                assert!(!js.contains(banned), "{name} holds {banned}");
            }
        }
        let html = read("index.html");
        assert!(!html.contains("style="), "an inline style");
        for tag in html.split("<script").skip(1) {
            let open = tag.split('>').next().unwrap_or_default();
            assert!(open.contains("src="), "an inline script: <script{open}>");
        }
        // Every embedded file is one of the folder's, and the other way round.
        let mut on_disk = tree(&dir);
        let mut served: Vec<String> = FILES.iter().map(|f| f.0.to_string()).collect();
        served.extend(NOVNC.iter().map(|f| f.0.to_string()));
        served.retain(|p| p != "/");
        served.sort();
        // pako's README is kept with its tree and not served.
        on_disk.retain(|p| p != "/index.html" && p != "/novnc/vendor/pako/README.md");
        assert_eq!(served, on_disk);
    }

    /// Every file under `dir`, as `/path` from it, sorted.
    fn tree(dir: &std::path::Path) -> Vec<String> {
        fn walk(dir: &std::path::Path, under: &str, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = format!("{under}/{}", entry.file_name().to_string_lossy());
                match entry.file_type().unwrap().is_dir() {
                    true => walk(&entry.path(), &path, out),
                    false => out.push(path),
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn every_novnc_file_is_served_as_javascript() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web");
        let scripts: Vec<String> = (tree(&dir).into_iter())
            .filter(|p| p.starts_with("/novnc/") && p.ends_with(".js"))
            .collect();
        assert_eq!(scripts.len(), 54);
        for path in &scripts {
            let (kind, bytes) = embedded(path).unwrap_or_else(|| panic!("{path} is not served"));
            assert_eq!(kind, "text/javascript; charset=utf-8", "{path}");
            assert_eq!(
                bytes,
                std::fs::read(dir.join(&path[1..])).unwrap(),
                "{path}"
            );
        }
        assert_eq!(NOVNC.len(), scripts.len());
        // The licences go with them, as text.
        for path in ["/novnc/LICENSE.txt", "/novnc/vendor/pako/LICENSE"] {
            assert_eq!(embedded(path).unwrap().0, "text/plain; charset=utf-8");
        }
        assert!(embedded("/novnc/core/../../app.js").is_none());
        assert!(embedded("/novnc/").is_none());
    }

    /// noVNC is the v1.7.0 tag's `core/`, `vendor/pako/` and `LICENSE.txt`, byte for byte: 57
    /// files. The hash is that of `sha256sum`'s lines for them, sorted by path, as
    /// `find * -type f | LC_ALL=C sort | xargs sha256sum | sha256sum` prints in the folder.
    #[test]
    fn the_novnc_tree_is_the_pinned_one() {
        use data_encoding::HEXLOWER;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web/novnc");
        let files = tree(&dir);
        assert_eq!(files.len(), 57);
        let mut lines = String::new();
        for path in &files {
            let bytes = std::fs::read(dir.join(&path[1..])).unwrap();
            let sum = HEXLOWER.encode(&passkey::sha256(&bytes));
            lines.push_str(&format!("{sum}  {}\n", &path[1..]));
        }
        assert_eq!(
            HEXLOWER.encode(&passkey::sha256(lines.as_bytes())),
            "9921ed08026b78e6f851aa3f3f5bb9afe1715ac558f8fd905b9e5747affefeb3"
        );
    }

    /// The CSP's `img-src 'self'` refuses a `data:` image, which is how noVNC 1.7.0 draws JPEG
    /// and PNG tiles (`display.js` `imageRect`) and the pointer's shape (`util/cursor.js`). The
    /// CSP stays as it is and noVNC's files too: the app asks the server for none of them, by
    /// dropping those encodings from the one message that lists them.
    #[test]
    fn the_app_asks_novnc_for_no_data_images() {
        let novnc = |path: &str| std::str::from_utf8(embedded(path).unwrap().1).unwrap();
        let encodings = novnc("/novnc/core/encodings.js");
        for line in [
            "encodingTightPNG: -260,",
            "encodingJPEG: 21,",
            "pseudoEncodingQualityLevel9: -23,",
            "pseudoEncodingQualityLevel0: -32,",
            "pseudoEncodingCursor: -239,",
            "pseudoEncodingVMwareCursor: 0x574d5664,",
        ] {
            assert!(encodings.contains(line), "{line}");
        }
        // The one place the encodings are sent from, and the two that build a `data:` URL.
        let rfb = novnc("/novnc/core/rfb.js");
        assert_eq!(rfb.matches("RFB.messages.clientEncodings(").count(), 1);
        assert!(novnc("/novnc/core/display.js").contains(r#"img.src = "data: " + mime"#));
        assert!(novnc("/novnc/core/util/cursor.js").contains("this._canvas.toDataURL()"));
        let app = std::str::from_utf8(embedded("/app.js").unwrap().1).unwrap();
        assert!(app.contains(
            "e === 21 || e === -260 || e === -239 || e === 0x574d5664 || (e >= -32 && e <= -23)"
        ));
        assert!(app.contains("RFB.messages.clientEncodings = "));
    }
}
