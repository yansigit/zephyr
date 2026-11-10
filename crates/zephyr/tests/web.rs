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

// ---- the WebSocket bridge ----

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use zephyr::store::Store;

fn b64(bytes: &[u8]) -> String {
    data_encoding::BASE64URL_NOPAD.encode(bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().try_into().unwrap()
}

impl Gateway {
    fn store(&self) -> Store {
        Store::open(&self.home.join("state.db")).unwrap()
    }

    /// A signed-in browser's cookie: a passkey named `name` and a session for it, written
    /// straight to the store.
    fn cookie(&self, name: &str) -> String {
        let store = self.store();
        let token = sha256(name.as_bytes());
        let passkey = store.add_passkey(name, name.as_bytes(), &[4; 65]).unwrap();
        store
            .add_web_session(&sha256(&token), passkey, zephyr::now())
            .unwrap();
        format!("__Host-zephyr={}", b64(&token))
    }

    /// Asks for the WebSocket as `host`, with these headers beside the handshake's own.
    async fn upgrade(&self, host: &str, headers: &[(&str, &str)]) -> Result<Ws, u16> {
        let mut req = format!("ws://{host}/api/ws").into_client_request().unwrap();
        for (name, value) in headers {
            let name: reqwest::header::HeaderName = name.parse().unwrap();
            req.headers_mut().insert(name, value.parse().unwrap());
        }
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        match tokio_tungstenite::client_async(req, tcp).await {
            Ok((sock, _)) => Ok(Ws {
                sock,
                next: 1,
                events: vec![],
            }),
            Err(WsError::Http(res)) => Err(res.status().as_u16()),
            Err(e) => panic!("the upgrade failed oddly: {e}"),
        }
    }

    /// The app's socket for a signed-in browser.
    async fn ws(&self, cookie: &str) -> Ws {
        let headers = [
            ("Origin", ORIGIN),
            ("Tailscale-User-Login", LOGIN),
            ("Cookie", cookie),
        ];
        self.upgrade(HOST, &headers)
            .await
            .expect("the socket opens")
    }
}

struct Ws {
    sock: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    next: u64,
    /// Frames that were not the answer being waited for, oldest first.
    events: Vec<Value>,
}

impl Ws {
    /// The next JSON frame; `None` once the socket is closed.
    async fn frame(&mut self) -> Option<Value> {
        loop {
            let next = tokio::time::timeout(WAIT, self.sock.next())
                .await
                .expect("a frame in time");
            match next {
                Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    }

    /// Calls a method and gives its whole answer frame; what arrives meanwhile is kept.
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.sock
            .send(Message::Text(req.to_string()))
            .await
            .unwrap();
        loop {
            let frame = self
                .frame()
                .await
                .expect("an answer before the socket closes");
            if frame["id"] == id {
                return frame;
            }
            self.events.push(frame);
        }
    }

    /// Sends the owner's text as a prompt.
    async fn say(&mut self, text: &str) -> Value {
        let cmd = json!({"type": "prompt", "content": text});
        self.rpc("send", json!({"sessionId": "main", "command": cmd}))
            .await
    }

    /// The first event (kept or still to come) that `f` takes.
    async fn event(&mut self, f: impl Fn(&Value) -> bool) -> Value {
        if let Some(at) = self.events.iter().position(|e| f(&e["params"]["event"])) {
            return self.events.remove(at)["params"]["event"].take();
        }
        loop {
            let mut frame = self
                .frame()
                .await
                .expect("the event before the socket closes");
            if f(&frame["params"]["event"]) {
                return frame["params"]["event"].take();
            }
            self.events.push(frame);
        }
    }

    /// Waits for the server to close the socket.
    async fn closed(&mut self) {
        while self.frame().await.is_some() {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_needs_a_cookie_origin_host_and_tailnet_header() {
    let gw = start("ws-checks");
    gw.up().await;
    let cookie = gw.cookie("phone");
    let (origin, login) = (("Origin", ORIGIN), ("Tailscale-User-Login", LOGIN));
    let cookie = ("Cookie", cookie.as_str());
    let no_cookie = gw.upgrade(HOST, &[origin, login]).await;
    assert_eq!(no_cookie.err(), Some(401));
    let wrong = (
        "Cookie",
        "__Host-zephyr=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    );
    assert_eq!(
        gw.upgrade(HOST, &[origin, login, wrong]).await.err(),
        Some(401)
    );
    let evil = ("Origin", "https://evil.test");
    assert_eq!(
        gw.upgrade(HOST, &[evil, login, cookie]).await.err(),
        Some(403)
    );
    assert_eq!(
        gw.upgrade(HOST, &[login, cookie]).await.err(),
        Some(403),
        "no Origin"
    );
    assert_eq!(
        gw.upgrade("evil.test", &[origin, login, cookie])
            .await
            .err(),
        Some(421)
    );
    assert_eq!(
        gw.upgrade(HOST, &[origin, cookie]).await.err(),
        Some(403),
        "no Tailscale header"
    );
    let mut ws = gw
        .upgrade(HOST, &[origin, login, cookie])
        .await
        .expect("all four");
    assert_eq!(ws.rpc("ping", json!({})).await["result"]["type"], "pong");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_web_send_is_a_web_row_and_its_reply_streams_back() {
    let gw = start("ws-send");
    gw.up().await;
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    let hello = ws.rpc("zephyr.hello", json!({})).await;
    assert_eq!(hello["result"]["version"], env!("CARGO_PKG_VERSION"));
    let sub = ws.rpc("zephyr.subscribe", json!({})).await;
    assert_eq!(sub["result"]["status"], "subscribed");
    assert_eq!(ws.say("hi").await["result"]["status"], "sent");
    let done = ws
        .event(|e| e["type"] == "message_end" && e["message"]["role"] == "assistant")
        .await;
    assert!(done.to_string().contains("echo: [web #1] hi"), "{done}");
    let db = rusqlite::Connection::open(gw.home.join("state.db")).unwrap();
    let row: (String, String) = db
        .query_row("SELECT channel, sender FROM inbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(row, ("web".into(), "web".into()));
    // An open socket shows as the channel being connected, once the snapshot is next built.
    let status = ws
        .event(|e| {
            e["type"] == "zephyr_status"
                && e["snapshot"]["channels"]
                    .to_string()
                    .contains(r#"{"name":"web","state":"connected"}"#)
        })
        .await;
    assert_eq!(status["snapshot"]["session"], "main");
    // The queue is this channel's own to take back from.
    assert_eq!(
        ws.rpc("zephyr.dequeue", json!({})).await["result"]["texts"],
        json!([])
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_status_answers_on_web_only() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let home = new_home("ws-status");
    let (token, attach) = (home.join("tui.token"), free_port());
    std::fs::write(&token, "s3cret\n").unwrap();
    std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let extra = format!("[attach]\nport = {attach}\ntoken_file = {token:?}\n");
    let gw = start_in(&home, free_port(), &extra, |_| {});
    gw.up().await;
    // A TUI, attached and subscribed.
    let tui = tokio::net::TcpStream::connect(("127.0.0.1", attach))
        .await
        .unwrap();
    let (read, mut write) = tui.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    for (id, method, params) in [
        (1, "authenticate", json!({"token": "s3cret"})),
        (2, "zephyr.subscribe", json!({})),
    ] {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            if serde_json::from_str::<Value>(&line).unwrap()["id"] == id {
                break;
            }
        }
    }
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    ws.say("/status").await;
    let reply = ws.event(|e| e["type"] == "zephyr_reply").await;
    assert_eq!(
        (&reply["channel"], &reply["command"]),
        (&json!("web"), &json!("/status"))
    );
    assert!(
        reply["text"].as_str().unwrap().starts_with("agent: "),
        "{reply}"
    );
    // The TUI was sent nothing for it before the answer to its own next call.
    let req = json!({"jsonrpc": "2.0", "id": 3, "method": "ping", "params": {}});
    write
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let line = lines.next_line().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(frame["params"]["event"]["type"], "zephyr_reply", "{frame}");
        if frame["id"] == 3 {
            break;
        }
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn methods_closed_to_the_web_app_answer_32601() {
    let gw = start("ws-closed");
    gw.up().await;
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    for method in [
        "authenticate",
        "start_session",
        "subscribe",
        "stop_session",
        "list_sessions",
        "zephyr.vault_put",
        "zephyr.vault_rm",
        "zephyr.vault_revoke",
        "zephyr.facts_put",
        "zephyr.facts_rm",
        "zephyr.sessions",
        "zephyr.login_providers",
        "zephyr.login_start",
        "zephyr.login_poll",
        "zephyr.login_input",
        "zephyr.login_cancel",
        "zephyr.nope",
    ] {
        let params = json!({"token": "x", "name": "n", "provider": "p"});
        let answer = ws.rpc(method, params).await;
        assert_eq!(answer["error"]["code"], -32601, "{method}: {answer}");
        assert_eq!(
            answer["error"]["message"], "not available in the web app",
            "{method}"
        );
    }
    for open in [
        "zephyr.settings",
        "zephyr.proposals",
        "zephyr.facts_list",
        "zephyr.vault_list",
        "zephyr.turns",
    ] {
        assert!(
            ws.rpc(open, json!({})).await["result"].is_object(),
            "{open}"
        );
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fifth_socket_gets_503() {
    let gw = start("ws-five");
    gw.up().await;
    let cookie = gw.cookie("phone");
    let mut open = vec![];
    for _ in 0..4 {
        open.push(gw.ws(&cookie).await);
    }
    let headers = [
        ("Origin", ORIGIN),
        ("Tailscale-User-Login", LOGIN),
        ("Cookie", cookie.as_str()),
    ];
    assert_eq!(gw.upgrade(HOST, &headers).await.err(), Some(503));
    // A closed socket frees its place.
    let mut first = open.remove(0);
    first.sock.close(None).await.unwrap();
    first.closed().await;
    let mut fifth = loop {
        match gw.upgrade(HOST, &headers).await {
            Ok(ws) => break ws,
            Err(503) => tokio::task::yield_now().await, // the server has not seen the close yet
            Err(code) => panic!("{code}"),
        }
    };
    assert_eq!(fifth.rpc("ping", json!({})).await["result"]["type"], "pong");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_over_1_mib_or_with_a_newline_closes_the_socket() {
    let gw = start("ws-frames");
    gw.up().await;
    let cookie = gw.cookie("phone");
    let mut big = gw.ws(&cookie).await;
    // The server closes mid-frame, so the send itself may fail.
    let _ = big.sock.send(Message::Text("x".repeat(2 << 20))).await;
    big.closed().await;
    let mut two = gw.ws(&cookie).await;
    two.sock.send(Message::Text("{}\n{}".into())).await.unwrap();
    two.closed().await;
    let mut binary = gw.ws(&cookie).await;
    binary
        .sock
        .send(Message::Binary(b"{}".to_vec()))
        .await
        .unwrap();
    binary.closed().await;
    // More than 60 requests in a minute.
    let mut busy = gw.ws(&cookie).await;
    for _ in 0..60 {
        assert_eq!(busy.rpc("ping", json!({})).await["result"]["type"], "pong");
    }
    let _ = busy.sock.send(Message::Text("{}".into())).await;
    busy.closed().await;
    gw.shutdown().await;
}
