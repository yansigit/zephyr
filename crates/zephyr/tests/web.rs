//! The web app (src/web/, DESIGN §4.3.3) over HTTP and its WebSocket, against the scripted fake
//! child (tests/fake-rpi.py). Requests carry what `tailscale serve` and a browser on the app's
//! own origin would send.

#![allow(dead_code)] // the harness is filled in as the routes arrive

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;

const HOST: &str = "zephyr.test";
const ORIGIN: &str = "https://zephyr.test";
const LOGIN: &str = "owner@github";
const WAIT: Duration = Duration::from_secs(20);

/// A channel that feeds what a test says and keeps what the router sends on it.
struct Stub {
    name: &'static str,
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    sent: mpsc::UnboundedSender<Outbound>,
}

impl Channel for Stub {
    fn name(&self) -> &str {
        self.name
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let mut feed = self.feed.lock().await;
        while let Some(m) = feed.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        let _ = self.sent.send(msg.clone());
        Ok(())
    }
}

struct Gateway {
    home: PathBuf,
    port: u16,
    say: mpsc::Sender<Inbound>,
    sent: mpsc::UnboundedReceiver<Outbound>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

/// A port below the kernel's ephemeral range, from this test process's own block of 100 (see
/// tests/attach.rs `free_port`).
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let block = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let port = block + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 100;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

fn new_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "zephyr-web-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn start(name: &str) -> Gateway {
    start_in(&new_home(name), free_port(), "", |_| {})
}

/// A gateway on `home` whose web app answers as `zephyr.test` on `port`. `extra` is appended to
/// the config before `[web]`; `tweak` sets the fields no file sets.
fn start_in(home: &Path, port: u16, extra: &str, tweak: impl FnOnce(&mut Config)) -> Gateway {
    let path = home.join("zephyr.toml");
    std::fs::write(&path, format!(
        "rpi = {:?}\nhome = {home:?}\nagent_dir = {:?}\nmodel = \"fake/m1\"\n[owners]\nchat = [\"owner\"]\ndiscord = [\"owner\"]\n{extra}\n[web]\nhost = \"{HOST}\"\nport = {port}\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home.join("agent"),
    ))
    .unwrap();
    let mut cfg = Config::load(&path).unwrap();
    cfg.tick = Duration::from_millis(200);
    tweak(&mut cfg);
    let (say, feed) = mpsc::channel(16);
    let (sent, got) = mpsc::unbounded_channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let chat = Stub {
            name: "chat",
            feed: tokio::sync::Mutex::new(feed),
            sent: sent.clone(),
        };
        // Discord's rows are fed through chat's queue: the router reads the row's own channel.
        let discord = Stub {
            name: "discord",
            feed: tokio::sync::Mutex::new(mpsc::channel(1).1),
            sent,
        };
        zephyr::router::run(&cfg, &[chat, discord], async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        home: home.to_path_buf(),
        port,
        say,
        sent: got,
        stop,
        task,
    }
}

impl Gateway {
    /// Waits for the web listener.
    async fn up(&self) {
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(("127.0.0.1", self.port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the web listener never came up");
    }

    /// A request as the app's own page would send it through `tailscale serve`.
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
            .header("Host", HOST)
            .header("Origin", ORIGIN)
            .header("Tailscale-User-Login", LOGIN)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.req(reqwest::Method::GET, path).send().await.unwrap()
    }

    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
    }
}

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self' wss://zephyr.test; manifest-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

#[tokio::test(flavor = "multi_thread")]
async fn the_files_carry_the_security_headers() {
    let gw = start("headers");
    gw.up().await;
    let res = gw.get("/").await;
    assert_eq!(res.status(), 200);
    let h = |name: &str| res.headers()[name].to_str().unwrap().to_string();
    assert_eq!(h("content-security-policy"), CSP);
    assert_eq!(h("x-frame-options"), "DENY");
    assert_eq!(h("x-content-type-options"), "nosniff");
    assert_eq!(h("referrer-policy"), "no-referrer");
    assert_eq!(h("cross-origin-opener-policy"), "same-origin");
    assert_eq!(h("cross-origin-resource-policy"), "same-origin");
    assert_eq!(h("cache-control"), "no-cache");
    assert_eq!(h("content-type"), "text/html; charset=utf-8");
    let etag = h("etag");
    assert_eq!(etag, format!("\"{}\"", env!("CARGO_PKG_VERSION")));
    assert!(res.text().await.unwrap().contains("/app.js"));

    let again = (gw.req(reqwest::Method::GET, "/app.js"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 304);
    assert_eq!(again.headers()["content-security-policy"], CSP);
    for (path, kind) in [
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/app.css", "text/css; charset=utf-8"),
        ("/sw.js", "text/javascript; charset=utf-8"),
        ("/manifest.webmanifest", "application/manifest+json"),
        ("/icon-192.png", "image/png"),
        ("/icon-512.png", "image/png"),
    ] {
        let res = gw.get(path).await;
        assert_eq!(res.status(), 200, "{path}");
        assert_eq!(res.headers()["content-type"], kind, "{path}");
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_paths_hosts_and_callers_are_refused() {
    let gw = start("refused");
    gw.up().await;
    let res = gw.get("/nope").await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.headers()["x-frame-options"], "DENY", "every response");
    assert_eq!(
        gw.get("/api/nope").await.headers()["cache-control"],
        "no-store"
    );
    assert_eq!(gw.get("/../zephyr.toml").await.status(), 404);

    let url = format!("http://127.0.0.1:{}/", gw.port);
    let client = reqwest::Client::new();
    let evil = (client.get(&url).header("Host", "evil.test"))
        .header("Tailscale-User-Login", LOGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), 421);
    // Straight at the port, as a browser on the box or a funnel would arrive.
    let bare = client.get(&url).header("Host", HOST).send().await.unwrap();
    assert_eq!(bare.status(), 403);
    assert_eq!(
        bare.text().await.unwrap(),
        "open the app through tailscale serve"
    );
    gw.shutdown().await;
}
