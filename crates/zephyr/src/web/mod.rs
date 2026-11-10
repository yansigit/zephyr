//! The web app (DESIGN §4.3.3): one hyper listener on 127.0.0.1 behind `tailscale serve`. It
//! checks Host, Origin and the Tailscale header on every request, and serves the embedded files.
//! `passkey` and `push` are pure.

pub mod passkey;
pub mod push;

use crate::attach::Handles;
use crate::config::{Config, Web};
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
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Connections served at once; one more is closed as it arrives.
const MAX_CONNS: usize = 32;
/// How long a connection has to send its request head.
const HEAD_WAIT: Duration = Duration::from_secs(10);

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
];

/// The files' ETag: they change only with the binary.
const ETAG: &str = concat!("\"", env!("CARGO_PKG_VERSION"), "\"");

/// What the listener and the router share: how many web sockets are open, and a counter that
/// moves when a `/web` command ends sessions, so open sockets check theirs at once.
pub struct Hub {
    sockets: AtomicUsize,
    revoked: watch::Sender<u64>,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub {
            sockets: AtomicUsize::new(0),
            revoked: watch::channel(0).0,
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
    if changes || path == "/api/ws" {
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

/// What every listener task needs.
struct App {
    web: Web,
    #[allow(dead_code)] // the WebSocket bridge and the ceremonies read them
    handles: Handles,
    #[allow(dead_code)]
    hub: Arc<Hub>,
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
    let listener = match TcpListener::bind(("127.0.0.1", web.port)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("zephyr: web app not listening on port {}: {e}", web.port);
            return std::future::pending().await;
        }
    };
    eprintln!("zephyr: web app listening on 127.0.0.1:{}", web.port);
    let app = Arc::new(App { web, handles, hub });
    // Anything on the host can connect (the agent too, under host networking): a cap, so idle
    // sockets cannot use up the daemon's file descriptors, and `HEAD_WAIT` for the head.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // shortcut: a persistent accept error (e.g. out of fds) retries every 100 ms with no backoff.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue; // dropped, so closed at once
        };
        let app = app.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(handle(req, &app).await) }
            });
            // One request per connection, so the head deadline bounds an idle one too.
            let _ = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEAD_WAIT)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await;
            drop(slot);
        });
    }
}

async fn handle(req: Request<Incoming>, app: &App) -> Res {
    let path = req.uri().path().to_string();
    let mut res = match admit(req.method(), &path, req.headers(), &app.web) {
        Err(StatusCode::FORBIDDEN) => text(
            StatusCode::FORBIDDEN,
            "open the app through tailscale serve",
        ),
        Err(code) => text(code, ""),
        Ok(()) => route(&req, &path),
    };
    secure(res.headers_mut(), &app.web.host, path.starts_with("/api/"));
    res
}

fn route(req: &Request<Incoming>, path: &str) -> Res {
    match FILES.iter().find(|f| f.0 == path) {
        Some(&(_, kind, bytes)) if req.method() == Method::GET => {
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
        _ => text(StatusCode::NOT_FOUND, ""),
    }
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
    fn the_app_holds_no_inline_code() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/web");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        for name in ["app.js", "sw.js"] {
            let js = read(name);
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
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| format!("/{}", e.unwrap().file_name().to_string_lossy()))
            .collect();
        on_disk.sort();
        let mut served: Vec<String> = FILES.iter().map(|f| f.0.to_string()).collect();
        served.retain(|p| p != "/");
        served.sort();
        on_disk.retain(|p| p != "/index.html");
        assert_eq!(served, on_disk);
    }
}
