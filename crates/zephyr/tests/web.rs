//! The web app (src/web/, DESIGN §4.3.3) over HTTP and its WebSocket, against the scripted fake
//! child (tests/fake-rpi.py). Requests carry what `tailscale serve` and a browser on the app's
//! own origin would send.

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
    sent: tokio::sync::Mutex<mpsc::UnboundedReceiver<Outbound>>,
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
    // The test process is not a container under the network floor, which the web app needs.
    cfg.web_off_floor = true;
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
        sent: tokio::sync::Mutex::new(got),
        stop,
        task,
    }
}

/// A connection to a listener of the gateway's, once it is up.
async fn listening(port: u16) -> tokio::net::TcpStream {
    for _ in 0..200 {
        if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nothing ever listened on port {port}");
}

impl Gateway {
    /// Waits for the web listener to answer. An answer, not a connection: after a restart on
    /// the same port, a child another test forked may still hold the old listening socket for
    /// a moment, and the kernel completes connections to it that nothing will ever accept.
    async fn up(&self) {
        for _ in 0..200 {
            let get = self
                .req(reqwest::Method::GET, "/")
                .timeout(Duration::from_millis(500));
            if get.send().await.is_ok_and(|res| res.status() == 200) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the web listener never answered");
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
        // noVNC, for the Desktop screen: served by the app itself, under the same policy.
        ("/novnc/core/rfb.js", "text/javascript; charset=utf-8"),
        (
            "/novnc/vendor/pako/lib/zlib/inflate.js",
            "text/javascript; charset=utf-8",
        ),
        ("/novnc/LICENSE.txt", "text/plain; charset=utf-8"),
    ] {
        let res = gw.get(path).await;
        assert_eq!(res.status(), 200, "{path}");
        assert_eq!(res.headers()["content-type"], kind, "{path}");
        assert_eq!(res.headers()["content-security-policy"], CSP, "{path}");
    }
    let page = gw.get("/").await.text().await.unwrap();
    assert!(page.contains(r#"data-screen="desktop" hidden"#), "{page}");
    for path in ["/novnc/", "/novnc/core/", "/novnc/vendor/pako/README.md"] {
        assert_eq!(gw.get(path).await.status(), 404, "{path}");
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
    let tui = listening(attach).await;
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

// ---- enrolment, sign-in and /web ----

use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

/// A software authenticator: a P-256 key that answers `navigator.credentials` as a platform
/// passkey would, with user verification.
struct Authenticator {
    key: EcdsaKeyPair,
    rng: ring::rand::SystemRandom,
    id: Vec<u8>,
}

impl Authenticator {
    fn new(id: &str) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let alg = &ECDSA_P256_SHA256_ASN1_SIGNING;
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        Self {
            key: EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap(),
            rng,
            id: id.as_bytes().to_vec(),
        }
    }

    fn client_data(kind: &str, options: &Value) -> Vec<u8> {
        json!({"type": kind, "challenge": options["challenge"], "origin": ORIGIN, "crossOrigin": false})
            .to_string()
            .into_bytes()
    }

    /// rpIdHash, the flags (UP, UV, and AT for a registration) and a zero sign count.
    fn auth_data(flags: u8) -> Vec<u8> {
        let mut out = sha256(HOST.as_bytes()).to_vec();
        out.push(flags);
        out.extend_from_slice(&[0; 4]);
        out
    }

    /// What the page posts to `enrol/finish`, less `challengeId` and `name`.
    fn create(&self, options: &Value) -> Value {
        assert_eq!(options["rp"]["id"], HOST);
        assert_eq!(
            options["pubKeyCredParams"],
            json!([{"type": "public-key", "alg": -7}])
        );
        let mut auth = Self::auth_data(0x45);
        auth.extend_from_slice(&[0; 16]);
        auth.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
        auth.extend_from_slice(&self.id);
        let mut spki = data_encoding::HEXLOWER
            .decode(b"3059301306072a8648ce3d020106082a8648ce3d030107034200")
            .unwrap();
        spki.extend_from_slice(self.key.public_key().as_ref());
        json!({"id": b64(&self.id), "clientDataJSON": b64(&Self::client_data("webauthn.create", options)),
            "authenticatorData": b64(&auth), "publicKey": b64(&spki), "alg": -7})
    }

    /// What the page posts to `login/finish`, or sends as an `assertion`.
    fn get(&self, options: &Value) -> Value {
        assert_eq!(options["rpId"], HOST);
        assert_eq!(options["userVerification"], "required");
        let client = Self::client_data("webauthn.get", options);
        let auth = Self::auth_data(0x05);
        let mut msg = auth.clone();
        msg.extend_from_slice(&sha256(&client));
        let signature = self.key.sign(&self.rng, &msg).unwrap();
        json!({"id": b64(&self.id), "clientDataJSON": b64(&client), "authenticatorData": b64(&auth),
            "signature": b64(signature.as_ref())})
    }
}

/// `value` with the fields of `more` added.
fn with(mut value: Value, more: Value) -> Value {
    for (k, v) in more.as_object().unwrap() {
        value[k] = v.clone();
    }
    value
}

const COOKIE_PATTERN: &str = "; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=2592000";

/// The `name=value` of a response's session cookie, after checking its attributes.
fn set_cookie(res: &reqwest::Response) -> String {
    let header = res.headers()["set-cookie"].to_str().unwrap();
    let (pair, attributes) = header.split_at(header.find(';').unwrap());
    assert_eq!(attributes, COOKIE_PATTERN);
    let token = pair
        .strip_prefix("__Host-zephyr=")
        .expect("the cookie's name");
    assert_eq!(
        data_encoding::BASE64URL_NOPAD
            .decode(token.as_bytes())
            .unwrap()
            .len(),
        32
    );
    pair.to_string()
}

impl Gateway {
    async fn say(&self, channel: &str, text: &str) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let m = Inbound {
            channel: channel.into(),
            platform_msg_id: format!("m{n}"),
            sender: "owner".into(),
            text: text.into(),
            derived: None,
        };
        self.say.send(m).await.unwrap();
    }

    /// The next thing sent on `channel` that `f` takes; others are dropped.
    async fn sent(&self, channel: &str, f: impl Fn(&str) -> bool) -> String {
        let mut sent = self.sent.lock().await;
        loop {
            let out = tokio::time::timeout(WAIT, sent.recv()).await;
            let out = out.expect("a reply in time").expect("the gateway runs");
            if out.channel == channel && f(&out.text) {
                return out.text;
            }
        }
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.req(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// `/web enrol NAME` on chat, and the code its reply carries.
    async fn enrol_code(&self, name: &str) -> String {
        self.say("chat", &format!("/web enrol {name}")).await;
        let reply = self.sent("chat", |t| t.contains("#enrol=")).await;
        let expected = "[zephyr] open https://zephyr.test/#enrol=";
        assert!(reply.starts_with(expected), "{reply}");
        assert!(
            reply.ends_with(" on the phone within 10 min, or type the code there; it works once")
        );
        let code = &reply[expected.len()..expected.len() + 11];
        let ok = |c: char| c.is_ascii_uppercase() || ('2'..='7').contains(&c);
        assert!(
            code.len() == 11
                && code
                    .char_indices()
                    .all(|(i, c)| if i == 5 { c == '-' } else { ok(c) }),
            "{code}"
        );
        code.to_string()
    }

    /// Enrols `auth` as NAME with a fresh code, and gives the new session's cookie.
    async fn enrol(&self, name: &str, auth: &Authenticator) -> String {
        let code = self.enrol_code(name).await;
        let start: Value = self
            .post("/api/enrol/start", json!({"code": code}))
            .await
            .json()
            .await
            .unwrap();
        let body = with(
            auth.create(&start["publicKey"]),
            json!({"challengeId": start["challengeId"]}),
        );
        let done = self.post("/api/enrol/finish", body).await;
        assert_eq!(done.status(), 200);
        set_cookie(&done)
    }

    /// A sign-in with `auth`: the `login/finish` response.
    async fn login(&self, auth: &Authenticator) -> reqwest::Response {
        let start: Value = self
            .post("/api/login/start", json!({}))
            .await
            .json()
            .await
            .unwrap();
        let body = with(
            auth.get(&start["publicKey"]),
            json!({"challengeId": start["challengeId"]}),
        );
        self.post("/api/login/finish", body).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn enrolment_takes_a_code_once_and_says_so() {
    let gw = start("enrol");
    gw.up().await;
    // The owner the agent last answered is told of a new passkey.
    gw.say("chat", "hi").await;
    gw.sent("chat", |t| t.starts_with("echo:")).await;
    let code = gw.enrol_code("phone").await;
    let wrong = gw
        .post("/api/enrol/start", json!({"code": "AAAAA-AAAAA"}))
        .await;
    assert_eq!(wrong.status(), 403);
    assert_eq!(wrong.headers()["cache-control"], "no-store");
    let said: Value = wrong.json().await.unwrap();
    assert_eq!(
        said,
        json!({"error": "that code is wrong, used or expired"})
    );

    let start = gw.post("/api/enrol/start", json!({"code": code})).await;
    assert_eq!(start.status(), 200);
    let start: Value = start.json().await.unwrap();
    let options = &start["publicKey"];
    assert_eq!(options["rp"], json!({"id": HOST, "name": "zephyr"}));
    assert_eq!(options["user"]["name"], "zephyr owner");
    assert_eq!(b64_decode(&options["user"]["id"]).len(), 16);
    assert_eq!(b64_decode(&options["challenge"]).len(), 32);
    assert_eq!(
        options["authenticatorSelection"],
        json!({"residentKey": "required", "userVerification": "required"})
    );
    assert_eq!(
        (&options["attestation"], &options["timeout"]),
        (&json!("none"), &json!(120_000))
    );
    assert_eq!(options["excludeCredentials"], json!([]));

    let phone = Authenticator::new("phone-cred");
    let body = with(
        phone.create(options),
        json!({"challengeId": start["challengeId"]}),
    );
    let done = gw.post("/api/enrol/finish", body.clone()).await;
    assert_eq!(done.status(), 200);
    let cookie = set_cookie(&done);
    assert_eq!(
        done.json::<Value>().await.unwrap(),
        json!({"name": "phone"})
    );
    let notice = gw.sent("chat", |t| t.contains("passkey phone")).await;
    assert_eq!(
        notice,
        "[zephyr] passkey phone was added to the web app; /web rm phone removes it"
    );
    // The cookie is a session: the socket opens.
    let mut ws = gw.ws(&cookie).await;
    assert_eq!(ws.rpc("ping", json!({})).await["result"]["type"], "pong");

    // The code is used, and so is the ceremony.
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": code}))
            .await
            .status(),
        403
    );
    assert_eq!(gw.post("/api/enrol/finish", body).await.status(), 403);
    let store = gw.store();
    let keys = store.passkeys().unwrap();
    assert_eq!(
        (
            keys.len(),
            keys[0].name.as_str(),
            keys[0].cred_id.as_slice()
        ),
        (1, "phone", &b"phone-cred"[..])
    );
    assert_eq!(store.meta("web_enrol").unwrap(), None);
    // The next ceremony names the enrolled passkey, so a device does not enrol twice.
    let code = gw.enrol_code("ipad").await;
    let start: Value = gw
        .post("/api/enrol/start", json!({"code": code}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        start["publicKey"]["excludeCredentials"],
        json!([{"type": "public-key", "id": b64(b"phone-cred")}])
    );
    gw.shutdown().await;
}

fn b64_decode(v: &Value) -> Vec<u8> {
    data_encoding::BASE64URL_NOPAD
        .decode(v.as_str().unwrap().as_bytes())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_enrol_code_typed_in_lowercase_with_spaces_is_accepted() {
    let gw = start("enrol-typed");
    gw.up().await;
    let code = gw.enrol_code("phone").await;
    let typed = code.to_lowercase().replace('-', " ");
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": typed}))
            .await
            .status(),
        200
    );
    let spaced = format!("  {} - {}\n", &code[..5], &code[6..]);
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": spaced}))
            .await
            .status(),
        200
    );
    // One code, one passkey: of two ceremonies started with it, the second to finish fails.
    let (a, b) = (Authenticator::new("a"), Authenticator::new("b"));
    let mut bodies = vec![];
    for auth in [&a, &b] {
        let start: Value = gw
            .post("/api/enrol/start", json!({"code": code}))
            .await
            .json()
            .await
            .unwrap();
        bodies.push(with(
            auth.create(&start["publicKey"]),
            json!({"challengeId": start["challengeId"]}),
        ));
    }
    assert_eq!(
        gw.post("/api/enrol/finish", bodies.remove(0))
            .await
            .status(),
        200
    );
    assert_eq!(
        gw.post("/api/enrol/finish", bodies.remove(0))
            .await
            .status(),
        403
    );
    assert_eq!(gw.store().passkeys().unwrap().len(), 1);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn five_failures_lock_sign_in_and_tell_the_owner() {
    let home = new_home("lockout");
    let gw = start_in(&home, free_port(), "", |cfg| {
        cfg.web_lockout = Duration::from_secs(1)
    });
    gw.up().await;
    gw.say("chat", "hi").await;
    gw.sent("chat", |t| t.starts_with("echo:")).await;
    let phone = Authenticator::new("phone-cred");
    let cookie = gw.enrol("phone", &phone).await;
    let mut ws = gw.ws(&cookie).await;
    // An unknown credential, a bad signature, a used challenge and a wrong code all count.
    let stranger = Authenticator::new("nobody");
    assert_eq!(gw.login(&stranger).await.status(), 403);
    let forged = Authenticator::new("phone-cred");
    assert_eq!(gw.login(&forged).await.status(), 403);
    let start: Value = gw
        .post("/api/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let body = with(
        phone.get(&start["publicKey"]),
        json!({"challengeId": start["challengeId"]}),
    );
    assert_eq!(
        gw.post("/api/login/finish", body.clone()).await.status(),
        200
    );
    assert_eq!(
        gw.post("/api/login/finish", body).await.status(),
        403,
        "a used challenge"
    );
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": "AAAAA-AAAAA"}))
            .await
            .status(),
        403
    );
    assert_eq!(
        gw.post("/api/login/finish", json!({"challengeId": "c0"}))
            .await
            .status(),
        403
    );
    // Closed: both ceremonies, to the right passkey too.
    let closed = gw.post("/api/login/start", json!({})).await;
    assert_eq!(closed.status(), 429);
    let said = closed.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        said.starts_with("sign-in is closed until ")
            && said.len() == "sign-in is closed until 00:00".len(),
        "{said}"
    );
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": "x"}))
            .await
            .status(),
        429
    );
    let notice = gw
        .sent("chat", |t| t.contains("failed passkey attempts"))
        .await;
    assert_eq!(
        notice,
        format!("[zephyr] 5 failed passkey attempts on the web app; {said}")
    );
    // An open session keeps working.
    assert_eq!(ws.rpc("ping", json!({})).await["result"]["type"], "pong");
    // It opens again by itself; refusals while closed did not use up the minute's requests.
    loop {
        let res = gw.post("/api/login/start", json!({})).await;
        if res.status() == 200 {
            break;
        }
        assert_eq!(res.status(), 429);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gw.login(&phone).await.status(), 200);
    // Told once: the next notice on chat is not a second lockout.
    gw.say("chat", "/web").await;
    let next = gw.sent("chat", |_| true).await;
    assert!(
        next.starts_with("[zephyr] web app https://zephyr.test · passkeys: phone (used "),
        "{next}"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_with_a_passkey_sets_a_cookie_and_logout_ends_it() {
    let gw = start("login");
    gw.up().await;
    let phone = Authenticator::new("phone-cred");
    gw.enrol("phone", &phone).await;
    let start: Value = gw
        .post("/api/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let options = &start["publicKey"];
    assert_eq!(b64_decode(&options["challenge"]).len(), 32);
    assert_eq!(
        (&options["timeout"], options.get("allowCredentials")),
        (&json!(120_000), None)
    );
    let body = with(
        phone.get(options),
        json!({"challengeId": start["challengeId"]}),
    );
    let done = gw.post("/api/login/finish", body).await;
    assert_eq!(done.status(), 200);
    let cookie = set_cookie(&done);
    assert!(gw.store().passkeys().unwrap()[0].last_used_at.is_some());
    assert_eq!(
        gw.store().web_sessions().unwrap().len(),
        2,
        "the enrolment's and this one"
    );

    let mut ws = gw.ws(&cookie).await;
    let out = gw
        .req(reqwest::Method::POST, "/api/logout")
        .header("Cookie", &cookie)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(out.status(), 204);
    assert_eq!(
        out.headers()["set-cookie"],
        "__Host-zephyr=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0"
    );
    ws.closed().await;
    let headers = [
        ("Origin", ORIGIN),
        ("Tailscale-User-Login", LOGIN),
        ("Cookie", cookie.as_str()),
    ];
    assert_eq!(gw.upgrade(HOST, &headers).await.err(), Some(401));
    assert_eq!(
        gw.store().web_sessions().unwrap().len(),
        1,
        "only this session ended"
    );
    assert_eq!(
        gw.post("/api/logout", json!({})).await.status(),
        401,
        "no cookie"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_enrol_from_a_weak_channel_is_refused() {
    let gw = start("enrol-weak");
    gw.up().await;
    gw.say("discord", "/web enrol phone").await;
    let reply = gw.sent("discord", |_| true).await;
    assert_eq!(reply, "[zephyr] /web enrol needs tui or chat");
    assert_eq!(gw.store().meta("web_enrol").unwrap(), None);
    for (text, reply) in [
        ("/web enrol two words", "[zephyr] usage: /web | /web enrol [NAME] | /web rm NAME | /web signout"),
        ("/web enrol bad/name", "[zephyr] usage: /web | /web enrol [NAME] | /web rm NAME | /web signout"),
        ("/web nope", "[zephyr] usage: /web | /web enrol [NAME] | /web rm NAME | /web signout"),
        ("/web rm nope", "[zephyr] no passkey nope; /web lists them"),
        ("/web", "[zephyr] web app https://zephyr.test · passkeys: none · 0 sessions · push on 0 devices"),
    ] {
        gw.say("chat", text).await;
        assert_eq!(gw.sent("chat", |_| true).await, reply, "{text}");
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_enrol_code_never_reaches_a_web_socket() {
    let gw = start("enrol-quiet");
    gw.up().await;
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    gw.enrol_code("ipad").await;
    // Everything the socket was sent up to the answer of a later call of its own.
    ws.say("/status").await;
    ws.event(|e| e["type"] == "zephyr_reply" && e["command"] == "/status")
        .await;
    for frame in &ws.events {
        assert!(!frame.to_string().contains("enrol="), "{frame}");
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_rm_closes_that_passkeys_socket_at_once() {
    let gw = start("rm");
    gw.up().await;
    let (mut phone, mut ipad) = (
        gw.ws(&gw.cookie("phone")).await,
        gw.ws(&gw.cookie("ipad")).await,
    );
    gw.store()
        .add_push("https://fcm.googleapis.com/x", &[4; 65], &[1; 16], 1)
        .unwrap();
    gw.say("discord", "/web rm phone").await;
    let reply = gw.sent("discord", |_| true).await;
    assert_eq!(
        reply,
        "[zephyr] removed passkey phone and its 1 sessions and pushes"
    );
    phone.closed().await;
    assert_eq!(ipad.rpc("ping", json!({})).await["result"]["type"], "pong");
    assert!(gw.store().pushes().unwrap().is_empty());
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_signout_ends_every_session() {
    let gw = start("signout");
    gw.up().await;
    let (mut phone, mut ipad) = (
        gw.ws(&gw.cookie("phone")).await,
        gw.ws(&gw.cookie("ipad")).await,
    );
    gw.say("chat", "/web signout").await;
    assert_eq!(
        gw.sent("chat", |_| true).await,
        "[zephyr] ended 2 web sessions"
    );
    phone.closed().await;
    ipad.closed().await;
    assert_eq!(gw.store().passkeys().unwrap().len(), 2, "the passkeys stay");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_guessing_routes_take_20_a_minute() {
    let gw = start("rate");
    gw.up().await;
    // A wrong code is an attempt; a sign-in merely started is not (it can guess nothing).
    for n in 0..4 {
        let wrong = gw.post("/api/enrol/start", json!({"code": "x"})).await;
        assert_eq!(wrong.status(), 403, "{n}");
        assert_eq!(gw.post("/api/login/start", json!({})).await.status(), 200);
    }
    // The fifth failure closes sign-in; here the lockout is what answers.
    let closed = gw.post("/api/enrol/start", json!({"code": "x"})).await;
    assert_eq!(closed.status(), 403);
    assert_eq!(gw.post("/api/login/start", json!({})).await.status(), 429);
    // The files and a signed-in browser's routes are not counted.
    assert_eq!(gw.get("/").await.status(), 200);
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    assert_eq!(ws.rpc("ping", json!({})).await["result"]["type"], "pong");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_devices_lists_this_session() {
    let gw = start("devices");
    gw.up().await;
    let (phone, _ipad) = (gw.cookie("phone"), gw.cookie("ipad"));
    gw.store()
        .add_push("https://fcm.googleapis.com/x", &[4; 65], &[1; 16], 2)
        .unwrap();
    let mut ws = gw.ws(&phone).await;
    let devices = ws.rpc("zephyr.web_devices", json!({})).await["result"].take();
    let names = |list: &Value, key: &str| -> Vec<String> {
        (list.as_array().unwrap().iter())
            .map(|v| v[key].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(&devices["passkeys"], "name"), ["phone", "ipad"]);
    assert!(
        devices["passkeys"][0]["created_at"].is_i64()
            && devices["passkeys"][0]["last_used_at"].is_null()
    );
    assert_eq!(names(&devices["sessions"], "passkey"), ["phone", "ipad"]);
    assert_eq!(
        (
            &devices["sessions"][0]["this"],
            &devices["sessions"][1]["this"]
        ),
        (&json!(true), &json!(false))
    );
    assert!(devices["sessions"][0]["seen_at"].is_i64());
    assert_eq!(names(&devices["push"], "passkey"), ["ipad"]);
    // Names and times only: no credential id, key, cookie hash or endpoint.
    let text = devices.to_string();
    assert!(
        !text.contains("fcm") && !text.contains("cred") && !text.contains("token"),
        "{text}"
    );
    gw.shutdown().await;
}

// ---- signed approvals and settings ----

use zephyr::store::{ActionKind, NewAction, Origin};

const SEND: &str = "himalaya message send --to a@x.org";

impl Gateway {
    /// Holds an action for the owner, as the gate stores one.
    fn hold(&self, kind: ActionKind, class: &str, payload: String) -> i64 {
        let action = NewAction {
            kind,
            class: class.into(),
            targets: vec!["a@x.org".into()],
            summary: format!("{class} to a@x.org"),
            reason: Some("trust me".into()),
            payload,
            secrets: vec![],
            origin: Origin {
                channel: "chat".into(),
                sender: "owner".into(),
                session: "main".into(),
                run: "main:1".into(),
                call_id: Some("c1".into()),
            },
            due_at: None,
        };
        self.store().hold_action(&action).unwrap()
    }

    /// Holds the `himalaya` send.
    fn hold_send(&self) -> i64 {
        let payload = zephyr::store::run_payload(&json!({"command": SEND}));
        self.hold(ActionKind::Run, "messages", payload)
    }

    fn status(&self, id: i64) -> String {
        let db = rusqlite::Connection::open(self.home.join("state.db")).unwrap();
        db.query_row("SELECT status FROM proposals WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// The commands started for approved actions: `(command, action)`.
    fn ran(&self) -> Vec<(String, i64)> {
        let db = rusqlite::Connection::open(self.home.join("state.db")).unwrap();
        let mut stmt = db
            .prepare("SELECT command, action FROM procs WHERE action IS NOT NULL ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        rows.map(Result::unwrap).collect()
    }
}

/// A gateway with approvals on, and a phone signed in with its own passkey.
async fn signed_in(name: &str, extra: &str) -> (Gateway, Authenticator, Ws) {
    let extra = format!("[approvals]\npreset = \"manual\"\n{extra}");
    let gw = start_in(&new_home(name), free_port(), &extra, |_| {});
    gw.up().await;
    let phone = Authenticator::new("phone-cred");
    let cookie = gw.enrol("phone", &phone).await;
    let ws = gw.ws(&cookie).await;
    (gw, phone, ws)
}

impl Ws {
    /// The hex SHA-256 of proposal `id`'s card, as the app hashes what it shows.
    async fn card_hash(&mut self, id: i64) -> String {
        let all = self.rpc("zephyr.proposals", json!({})).await;
        let list = all["result"]["proposals"].as_array().unwrap();
        // What is signed: the card without its countdown.
        let card = list.iter().find(|p| p["id"] == id).expect("it waits")["signed"]
            .as_str()
            .unwrap();
        data_encoding::HEXLOWER.encode(&sha256(card.as_bytes()))
    }

    /// Asks for a challenge for `op` and signs it: `{challengeId, assertion}`.
    async fn sign(&mut self, auth: &Authenticator, op: Value) -> Value {
        let answer = self.rpc("zephyr.web_challenge", op).await;
        let made = &answer["result"];
        assert!(made.is_object(), "{answer}");
        let options = &made["publicKey"];
        assert_eq!(b64_decode(&options["challenge"]).len(), 32);
        assert_eq!(options["timeout"], 120_000);
        json!({"challengeId": made["challengeId"], "assertion": auth.get(options)})
    }

    /// A signed `ok` (or `once`) on proposal `id`: the challenge for the card now, signed.
    async fn sign_decide(&mut self, auth: &Authenticator, id: i64, verb: &str) -> Value {
        let card = self.card_hash(id).await;
        let op = json!({"op": "decide", "id": id, "verb": verb, "card_sha256": card});
        with(self.sign(auth, op).await, json!({"id": id, "verb": verb}))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_ok_or_a_changed_card_is_refused() {
    let (gw, phone, mut ws) = signed_in("sign-unsigned", "").await;
    let id = gw.hold_send();
    for verb in ["ok", "once"] {
        let answer = ws
            .rpc("zephyr.decide", json!({"id": id, "verb": verb}))
            .await;
        assert_eq!(answer["error"]["code"], -32602, "{answer}");
        assert_eq!(answer["error"]["message"], "approve with your passkey");
    }
    let op = |card: &str| json!({"op": "decide", "id": id, "verb": "ok", "card_sha256": card});
    let stale = data_encoding::HEXLOWER.encode(&sha256(b"x"));
    let answer = ws.rpc("zephyr.web_challenge", op(&stale)).await;
    assert_eq!(
        answer["error"]["message"], "the card changed; look at it again",
        "{answer}"
    );
    let other = json!({"op": "decide", "id": id + 1, "verb": "ok", "card_sha256": stale});
    let answer = ws.rpc("zephyr.web_challenge", other).await;
    assert_eq!(
        answer["error"]["message"],
        format!("no proposal #{} is waiting", id + 1)
    );
    for bad in [
        json!({"op": "decide", "id": id, "verb": "no", "card_sha256": stale}),
        json!({"op": "nope"}),
        json!({"op": "set", "key": "no.such.key", "value": "1"}),
    ] {
        assert_eq!(
            ws.rpc("zephyr.web_challenge", bad).await["error"]["code"],
            -32602
        );
    }
    // The challenge names this session's own passkey, and no other.
    let card = ws.card_hash(id).await;
    let made = ws.rpc("zephyr.web_challenge", op(&card)).await["result"].take();
    assert_eq!(
        made["publicKey"]["allowCredentials"],
        json!([{"type": "public-key", "id": b64(&phone.id)}])
    );
    assert_eq!(
        (
            &made["publicKey"]["rpId"],
            &made["publicKey"]["userVerification"]
        ),
        (&json!(HOST), &json!("required"))
    );
    assert_eq!((gw.status(id), gw.ran()), ("pending".to_string(), vec![]));
    // Refusing needs no signature.
    let no = ws
        .rpc("zephyr.decide", json!({"id": id, "verb": "no"}))
        .await;
    assert_eq!(no["result"]["ok"], true, "{no}");
    assert_eq!(gw.status(id), "dropped");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_ok_runs_the_stored_command_and_is_audited_as_passkey() {
    let (gw, phone, mut ws) = signed_in("sign-ok", "").await;
    let id = gw.hold_send();
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let done = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(done["result"]["ok"], true, "{done}");
    assert!(
        done["result"]["text"]
            .as_str()
            .unwrap()
            .starts_with("[zephyr] approved #1"),
        "{done}"
    );
    assert_eq!(gw.ran(), [(SEND.to_string(), id)]);
    let audit = gw.store().audit_rows().unwrap();
    let row = (
        "web".to_string(),
        "passkey".to_string(),
        "#1 ok".to_string(),
        "signed".to_string(),
    );
    assert!(audit.contains(&row), "{audit:?}");
    assert!(gw.store().passkeys().unwrap()[0].last_used_at.is_some());
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_used_twice_is_refused() {
    let (gw, phone, mut ws) = signed_in("sign-twice", "").await;
    let (first, second) = (gw.hold_send(), gw.hold_send());
    let signed = ws.sign_decide(&phone, first, "ok").await;
    assert_eq!(
        ws.rpc("zephyr.decide", signed.clone()).await["result"]["ok"],
        true
    );
    let again = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(
        again["error"]["message"], "that signature expired or was used; tap again",
        "{again}"
    );
    // A signature made by another passkey, or by this one for another challenge.
    let other = Authenticator::new("phone-cred");
    let forged = ws.sign_decide(&other, second, "ok").await;
    let answer = ws.rpc("zephyr.decide", forged).await;
    assert_eq!(
        answer["error"]["message"], "the passkey signature did not verify",
        "{answer}"
    );
    let one = ws.sign_decide(&phone, second, "ok").await;
    let two = ws.sign_decide(&phone, second, "ok").await;
    let mixed = with(one, json!({"assertion": two["assertion"]}));
    let answer = ws.rpc("zephyr.decide", mixed).await;
    assert_eq!(
        answer["error"]["message"], "the passkey signature did not verify",
        "{answer}"
    );
    // Another session cannot use this one's challenge, nor use it up.
    let mut ipad = gw.ws(&gw.cookie("ipad")).await;
    let answer = ipad.rpc("zephyr.decide", two.clone()).await;
    assert_eq!(
        answer["error"]["message"],
        "that signature expired or was used; tap again"
    );
    assert_eq!(
        (gw.status(second), gw.ran().len()),
        ("pending".to_string(), 1)
    );
    assert_eq!(ws.rpc("zephyr.decide", two).await["result"]["ok"], true);
    assert_eq!(gw.ran().len(), 2);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signature_for_one_proposal_does_not_approve_another() {
    let (gw, phone, mut ws) = signed_in("sign-other", "").await;
    let (first, second) = (gw.hold_send(), gw.hold_send());
    let signed = ws.sign_decide(&phone, first, "ok").await;
    let replayed = with(signed.clone(), json!({"id": second}));
    let answer = ws.rpc("zephyr.decide", replayed).await;
    assert_eq!(
        answer["error"]["message"], "that signature is for another request",
        "{answer}"
    );
    // Its challenge is used up, so the first waits too.
    let answer = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(
        answer["error"]["message"],
        "that signature expired or was used; tap again"
    );
    // Nor does an `ok` signature approve `once`.
    let signed = ws.sign_decide(&phone, first, "ok").await;
    let answer = ws
        .rpc("zephyr.decide", with(signed, json!({"verb": "once"})))
        .await;
    assert_eq!(
        answer["error"]["message"],
        "that signature is for another request"
    );
    assert_eq!(
        (gw.status(first), gw.status(second)),
        ("pending".to_string(), "pending".to_string())
    );
    assert_eq!(gw.ran(), vec![]);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signature_is_refused_after_the_payload_changes() {
    let (gw, phone, mut ws) = signed_in("sign-changed", "").await;
    let id = gw.hold_send();
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let db = rusqlite::Connection::open(gw.home.join("state.db")).unwrap();
    let swapped = r#"{"command":"himalaya message send --to b@x.org"}"#;
    db.execute(
        "UPDATE proposals SET payload = ?1 WHERE id = ?2",
        rusqlite::params![swapped, id],
    )
    .unwrap();
    let answer = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(
        answer["result"],
        json!({"ok": false, "text": "[zephyr] the card changed; look at it again"}),
        "{answer}"
    );
    assert_eq!((gw.status(id), gw.ran()), ("pending".to_string(), vec![]));
    // The targets are read by `/ok` too, though the card may not show them.
    let signed = ws.sign_decide(&phone, id, "ok").await;
    db.execute(
        "UPDATE proposals SET targets = '[\"b@x.org\"]' WHERE id = ?1",
        [id],
    )
    .unwrap();
    let answer = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(answer["result"]["ok"], false, "{answer}");
    assert_eq!((gw.status(id), gw.ran()), ("pending".to_string(), vec![]));
    let audit = gw.store().audit_rows().unwrap();
    assert!(audit.iter().all(|row| row.1 != "passkey"), "{audit:?}");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn with_money_strong_only_a_payment_needs_the_signature() {
    let strong = "strong_only = [\"loosen\", \"money\"]\n";
    let (gw, phone, mut ws) = signed_in("sign-money", strong).await;
    let payload = r#"{"server":"shop","tool":"pay","arguments":{"amount":5,"currency":"USD"}}"#;
    let id = gw.hold(ActionKind::Pass, "money", payload.into());
    let answer = ws
        .rpc("zephyr.decide", json!({"id": id, "verb": "ok"}))
        .await;
    assert_eq!(
        answer["error"]["message"], "approve with your passkey",
        "{answer}"
    );
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let done = ws.rpc("zephyr.decide", signed).await;
    assert_eq!(done["result"]["ok"], true, "{done}");
    assert!(
        done["result"]["text"]
            .as_str()
            .unwrap()
            .starts_with("[zephyr] approved #1"),
        "{done}"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_strong_loosening_is_refused_unsigned_and_applied_signed() {
    let (gw, phone, mut ws) = signed_in("sign-set", "[budget]\ndaily_cost_usd = 5.0\n").await;
    let change = json!({"key": "budget.daily_cost_usd", "value": "8"});
    let unsigned = ws.rpc("zephyr.settings_set", change.clone()).await["result"].take();
    assert_eq!(unsigned["outcome"], "refused", "{unsigned}");
    assert_eq!(
        unsigned["reason"],
        "loosening budget.daily_cost_usd needs tui or chat or passkey"
    );
    let signed = ws
        .sign(&phone, with(change.clone(), json!({"op": "set"})))
        .await;
    let done = ws
        .rpc("zephyr.settings_set", with(signed, change.clone()))
        .await["result"]
        .take();
    assert_eq!(
        (&done["outcome"], &done["old"], &done["new"]),
        (&json!("changed"), &json!("5"), &json!("8")),
        "{done}"
    );
    let logged = gw
        .store()
        .last_setting_change("budget.daily_cost_usd")
        .unwrap()
        .unwrap();
    assert_eq!(
        (logged.actor.as_str(), logged.channel.as_deref()),
        ("owner", Some("passkey"))
    );
    // A signature for one value does not set another.
    let signed = ws
        .sign(
            &phone,
            json!({"op": "set", "key": "budget.daily_cost_usd", "value": "9"}),
        )
        .await;
    let other = with(
        signed,
        json!({"key": "budget.daily_cost_usd", "value": "900"}),
    );
    let answer = ws.rpc("zephyr.settings_set", other).await;
    assert_eq!(
        answer["error"]["message"], "that signature is for another request",
        "{answer}"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_change_is_refused_when_the_value_moved() {
    let (gw, phone, mut ws) = signed_in("sign-moved", "").await;
    let change = json!({"key": "tasks.parallel", "value": "3"});
    let signed = ws
        .sign(&phone, with(change.clone(), json!({"op": "set"})))
        .await;
    gw.say("chat", "/set tasks.parallel 4").await;
    gw.sent("chat", |t| t.contains("tasks.parallel")).await;
    let done = ws.rpc("zephyr.settings_set", with(signed, change)).await["result"].take();
    assert_eq!(
        done,
        json!({"outcome": "refused", "reason": "tasks.parallel changed; look at it again"})
    );
    gw.shutdown().await;
}

// ---- Web Push ----

/// A stand-in for a push service: keeps each request, and answers with `status`.
struct PushService {
    origin: String,
    got: tokio::sync::Mutex<mpsc::UnboundedReceiver<(reqwest::header::HeaderMap, Vec<u8>)>>,
    status: std::sync::Arc<std::sync::atomic::AtomicU16>,
}

impl PushService {
    async fn start() -> Self {
        use http_body_util::BodyExt;
        use std::sync::atomic::{AtomicU16, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, got) = mpsc::unbounded_channel();
        let status = std::sync::Arc::new(AtomicU16::new(201));
        let answer = status.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (tx, answer) = (tx.clone(), answer.clone());
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(
                        move |req: hyper::Request<hyper::body::Incoming>| {
                            let (tx, answer) = (tx.clone(), answer.clone());
                            async move {
                                let (head, body) = req.into_parts();
                                let body = body.collect().await.unwrap().to_bytes().to_vec();
                                // Its answer is the one set when it arrived: a test that sees
                                // this push and then changes `status` changes the next one only.
                                let status = answer.load(Ordering::Relaxed);
                                let _ = tx.send((head.headers, body));
                                let mut res = hyper::Response::new(http_body_util::Full::new(
                                    hyper::body::Bytes::new(),
                                ));
                                *res.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                                Ok::<_, std::convert::Infallible>(res)
                            }
                        },
                    );
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
        Self {
            origin,
            got: tokio::sync::Mutex::new(got),
            status,
        }
    }

    /// The next push it was sent.
    async fn next(&self) -> (reqwest::header::HeaderMap, Vec<u8>) {
        let mut got = self.got.lock().await;
        tokio::time::timeout(WAIT, got.recv())
            .await
            .expect("a push in time")
            .unwrap()
    }
}

/// A browser's push subscription: a P-256 key of its own (OpenSSL's, so one key opens many
/// pushes and the ECDH is not ring's on both ends) and an auth secret.
struct Subscriber {
    key: openssl::ec::EcKey<openssl::pkey::Private>,
    auth: [u8; 16],
}

impl Subscriber {
    fn new() -> Self {
        let group = Self::group();
        let mut auth = [0; 16];
        openssl::rand::rand_bytes(&mut auth).unwrap();
        Self {
            key: openssl::ec::EcKey::generate(&group).unwrap(),
            auth,
        }
    }

    fn group() -> openssl::ec::EcGroup {
        openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap()
    }

    fn public(&self) -> Vec<u8> {
        let mut ctx = openssl::bn::BigNumContext::new().unwrap();
        let form = openssl::ec::PointConversionForm::UNCOMPRESSED;
        self.key
            .public_key()
            .to_bytes(&Self::group(), form, &mut ctx)
            .unwrap()
    }

    /// What `pushManager.subscribe` gives the page to post.
    fn subscription(&self, endpoint: &str) -> Value {
        json!({"endpoint": endpoint, "keys": {"p256dh": b64(&self.public()), "auth": b64(&self.auth)}})
    }

    /// Opens a push body as the browser does: ECDH with the sender's key in the header.
    fn open(&self, body: &[u8]) -> Value {
        let (group, mut ctx) = (Self::group(), openssl::bn::BigNumContext::new().unwrap());
        assert!(body.len() <= 4096, "{}", body.len());
        let point = openssl::ec::EcPoint::from_bytes(&group, &body[21..86], &mut ctx).unwrap();
        let theirs = openssl::ec::EcKey::from_public_key(&group, &point).unwrap();
        let (ours, theirs) = (
            openssl::pkey::PKey::from_ec_key(self.key.clone()).unwrap(),
            openssl::pkey::PKey::from_ec_key(theirs).unwrap(),
        );
        let mut ecdh = openssl::derive::Deriver::new(&ours).unwrap();
        ecdh.set_peer(&theirs).unwrap();
        let secret = ecdh.derive_to_vec().unwrap();
        let plain = zephyr::web::push::decrypt(&secret, &self.public(), &self.auth, body);
        serde_json::from_slice(&plain.expect("it decrypts")).unwrap()
    }
}

/// A gateway that may push to `service`, a signed-in phone's cookie, and its subscription.
async fn pushing(name: &str, extra: &str, service: &PushService) -> (Gateway, String, Subscriber) {
    let extra = format!("[approvals]\npreset = \"manual\"\n{extra}");
    let origin = service.origin.clone();
    let gw = start_in(&new_home(name), free_port(), &extra, |cfg| {
        cfg.push_hosts = vec![origin];
        cfg.web_ping = Duration::from_millis(200);
    });
    gw.up().await;
    let cookie = gw.cookie("phone");
    let phone = Subscriber::new();
    let body = phone.subscription(&format!("{}/push/phone", service.origin));
    let res = gw
        .req(reqwest::Method::POST, "/api/push")
        .header("Cookie", &cookie)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    (gw, cookie, phone)
}

/// The claims of a VAPID header's token, verified under its own `k`, and `k`.
fn vapid(header: &str) -> (Value, String) {
    let rest = header.strip_prefix("vapid t=").expect("a vapid header");
    let (token, k) = rest.split_once(", k=").unwrap();
    let (signed, signature) = token.rsplit_once('.').unwrap();
    let point = data_encoding::BASE64URL_NOPAD.decode(k.as_bytes()).unwrap();
    let signature = data_encoding::BASE64URL_NOPAD
        .decode(signature.as_bytes())
        .unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(signed.as_bytes(), &signature)
        .expect("the token verifies under k");
    let claims =
        data_encoding::BASE64URL_NOPAD.decode(signed.split_once('.').unwrap().1.as_bytes());
    (
        serde_json::from_slice(&claims.unwrap()).unwrap(),
        k.to_string(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_card_is_pushed_encrypted_without_the_agents_reason() {
    let service = PushService::start().await;
    let (gw, cookie, phone) = pushing("push-card", "", &service).await;
    let key: Value = gw
        .req(reqwest::Method::GET, "/api/push/key")
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(gw.get("/api/push/key").await.status(), 401, "no cookie");
    let id = gw.hold_send();
    let (headers, body) = service.next().await;
    let h = |name: &str| headers[name].to_str().unwrap().to_string();
    assert_eq!(
        (h("urgency"), h("ttl")),
        ("high".to_string(), "86400".to_string())
    );
    assert_eq!(
        (h("content-encoding"), h("content-type")),
        (
            "aes128gcm".to_string(),
            "application/octet-stream".to_string()
        )
    );
    let (claims, k) = vapid(&h("authorization"));
    assert_eq!(claims["aud"], service.origin);
    assert_eq!(claims["sub"], "https://zephyr.test");
    let life = claims["exp"].as_i64().unwrap() - zephyr::now();
    assert!((43_000..=43_200).contains(&life), "{life}");
    assert_eq!(key, json!({ "key": k }), "the key the page subscribes with");
    let card = phone.open(&body);
    assert_eq!(card["kind"], "card");
    assert_eq!((&card["id"], &card["quick"]), (&json!(id), &json!(true)));
    assert_eq!(card["title"], "zephyr: approve #1");
    assert_eq!(card["body"], "messages: messages to a@x.org");
    assert!(!card.to_string().contains("trust me"), "{card}");
    // After a restart the card that is still waiting is not pushed again: the next push is
    // for the next card.
    let (home, port) = (gw.home.clone(), gw.port);
    gw.shutdown().await;
    let origin = service.origin.clone();
    let gw = start_in(&home, port, "[approvals]\npreset = \"manual\"\n", |cfg| {
        cfg.push_hosts = vec![origin]
    });
    gw.up().await;
    let next = gw.hold(ActionKind::Pass, "money", "{}".into());
    let card = phone.open(&service.next().await.1);
    assert_eq!(
        (&card["id"], &card["quick"]),
        (&json!(next), &json!(false)),
        "{card}"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_is_pushed_only_while_no_socket_is_open() {
    let service = PushService::start().await;
    let (gw, cookie, phone) = pushing("push-reply", "", &service).await;
    let mut ws = gw.ws(&cookie).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    gw.store().notify("web", "web", "[zephyr] one").unwrap();
    let shown = ws.event(|e| e["type"] == "zephyr_reply").await;
    assert_eq!(shown["text"], "[zephyr] one");
    ws.sock.close(None).await.unwrap();
    ws.closed().await;
    gw.store().notify("web", "web", "[zephyr] two").unwrap();
    let (headers, body) = service.next().await;
    assert_eq!(
        phone.open(&body),
        json!({"kind": "reply", "body": "[zephyr] two"}),
        "the first push"
    );
    assert_eq!(headers["ttl"], "3600");
    assert_eq!(headers["urgency"], "normal");
    // Pushed once, though it still waits for a socket: the next push is the next row's.
    gw.store().notify("web", "web", &"3".repeat(500)).unwrap();
    assert_eq!(phone.open(&service.next().await.1)["body"], "3".repeat(120));
    // The rows are shown when a socket opens.
    let mut ws = gw.ws(&cookie).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    let shown = ws.event(|e| e["type"] == "zephyr_reply").await;
    assert_eq!(shown["text"], "[zephyr] two");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_that_stops_answering_pings_gets_the_reply_pushed() {
    let service = PushService::start().await;
    let (gw, cookie, phone) = pushing("push-frozen", "", &service).await;
    // The reply takes far longer than two ping intervals.
    std::fs::write(gw.home.join("slow"), "2").unwrap();
    let mut ws = gw.ws(&cookie).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    assert_eq!(ws.say("are you there").await["result"]["status"], "sent");
    // From here the socket is never read again, as on a phone that froze the page: no pong
    // goes back, so the server closes it and the reply is pushed rather than marked sent.
    let pushed = phone.open(&service.next().await.1);
    assert_eq!(
        pushed,
        json!({"kind": "reply", "body": "echo: [web #1] are you there"})
    );
    drop(ws);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_410_drops_the_subscription() {
    let service = PushService::start().await;
    let (gw, _cookie, _phone) = pushing("push-gone", "", &service).await;
    service
        .status
        .store(500, std::sync::atomic::Ordering::Relaxed);
    gw.hold_send();
    service.next().await;
    service
        .status
        .store(410, std::sync::atomic::Ordering::Relaxed);
    gw.hold_send();
    service.next().await;
    // The 500 kept it; the 410 drops it, so the next card goes nowhere.
    for _ in 0..400 {
        if gw.store().pushes().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gw.store().pushes().unwrap().is_empty());
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn push_decide_takes_ok_only_for_quick_messages() {
    let service = PushService::start().await;
    let strong = "strong_only = [\"loosen\", \"money\"]\n";
    let (gw, cookie, _phone) = pushing("push-decide", strong, &service).await;
    let decide = |id: i64, verb: &'static str| {
        let req = gw
            .req(reqwest::Method::POST, "/api/push/decide")
            .header("Cookie", &cookie);
        async move {
            req.json(&json!({"id": id, "verb": verb}))
                .send()
                .await
                .unwrap()
        }
    };
    let message = gw.hold_send();
    let done = decide(message, "ok").await;
    assert_eq!(done.status(), 200);
    let done: Value = done.json().await.unwrap();
    assert_eq!(done["ok"], true, "{done}");
    assert!(
        done["text"]
            .as_str()
            .unwrap()
            .starts_with("[zephyr] approved #1"),
        "{done}"
    );
    assert_eq!(gw.ran(), [(SEND.to_string(), message)]);

    let payload = r#"{"server":"shop","tool":"pay","arguments":{"amount":5,"currency":"USD"}}"#;
    let payment = gw.hold(ActionKind::Pass, "money", payload.into());
    let refused = decide(payment, "ok").await;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        refused.json::<Value>().await.unwrap(),
        json!({"error": "open the app to approve this"})
    );
    assert_eq!(gw.status(payment), "pending");
    let denied = decide(payment, "no").await;
    assert_eq!(denied.status(), 200);
    assert_eq!(denied.json::<Value>().await.unwrap()["ok"], true);
    assert_eq!(gw.status(payment), "dropped");
    // Nothing waits under that id, an unknown verb, and no cookie.
    assert_eq!(decide(payment, "no").await.status(), 409);
    assert_eq!(decide(message, "once").await.status(), 409);
    assert_eq!(
        gw.post("/api/push/decide", json!({"id": 1, "verb": "no"}))
            .await
            .status(),
        401
    );
    // Ten a minute for a session.
    for _ in 0..5 {
        assert_eq!(decide(99, "no").await.status(), 409);
    }
    assert_eq!(decide(99, "no").await.status(), 429);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_outside_the_allowlist_is_refused() {
    let service = PushService::start().await;
    let (gw, cookie, phone) = pushing("push-allow", "", &service).await;
    let post = |method: reqwest::Method, body: Value| {
        let req = gw.req(method, "/api/push").header("Cookie", &cookie);
        async move { req.json(&body).send().await.unwrap().status().as_u16() }
    };
    let (post_, delete) = (reqwest::Method::POST, reqwest::Method::DELETE);
    assert_eq!(
        post(post_.clone(), phone.subscription("https://evil.test/push")).await,
        400
    );
    assert_eq!(
        post(post_.clone(), phone.subscription("http://127.0.0.1:9/push")).await,
        400
    );
    let short = json!({"endpoint": "https://fcm.googleapis.com/fcm/send/x", "keys": {"p256dh": "BA", "auth": "AA"}});
    assert_eq!(
        post(post_.clone(), short).await,
        400,
        "keys of the wrong size"
    );
    assert_eq!(
        post(
            post_.clone(),
            phone.subscription("https://fcm.googleapis.com/fcm/send/x")
        )
        .await,
        204
    );
    assert_eq!(
        gw.post(
            "/api/push",
            phone.subscription("https://fcm.googleapis.com/fcm/send/y")
        )
        .await
        .status(),
        401
    );
    let saved: Vec<String> = gw
        .store()
        .pushes()
        .unwrap()
        .into_iter()
        .map(|p| p.endpoint)
        .collect();
    assert_eq!(
        saved,
        [
            format!("{}/push/phone", service.origin),
            "https://fcm.googleapis.com/fcm/send/x".to_string()
        ]
    );
    assert_eq!(
        post(
            delete,
            json!({"endpoint": "https://fcm.googleapis.com/fcm/send/x"})
        )
        .await,
        204
    );
    assert_eq!(gw.store().pushes().unwrap().len(), 1);
    // The eleventh device is refused.
    for n in 0..9 {
        let endpoint = format!("https://fcm.googleapis.com/fcm/send/{n}");
        assert_eq!(
            post(post_.clone(), phone.subscription(&endpoint)).await,
            204
        );
    }
    assert_eq!(
        post(
            post_,
            phone.subscription("https://fcm.googleapis.com/fcm/send/more")
        )
        .await,
        409
    );
    gw.shutdown().await;
}

// ---- review of 91d0c76..98a575b ----

impl Gateway {
    /// Has the agent propose `command` through its `gateway` tool, and gives the proposal's id.
    async fn proposed(&self, calls: &str) -> i64 {
        self.say("chat", calls).await;
        let card = self
            .sent("chat", |t| {
                t.contains("the agent proposes #") || t.contains("the agent asks to run")
            })
            .await;
        let at = card.find('#').expect("a number") + 1;
        let digits: String = card[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_approval_of_a_strong_proposal_applies_it() {
    let (gw, phone, mut ws) = signed_in("sign-strong", "").await;
    // A loosening of a protected setting: only a strong channel may approve it.
    let id = gw
        .proposed(r#"[call gateway {"command": "/set approvals.messages auto"}] go"#)
        .await;
    let unsigned = ws
        .rpc("zephyr.decide", json!({"id": id, "verb": "ok"}))
        .await;
    assert_eq!(unsigned["error"]["message"], "approve with your passkey");
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let done = ws.rpc("zephyr.decide", signed).await["result"].take();
    assert_eq!(done["ok"], true, "{done}");
    let text = done["text"].as_str().unwrap();
    assert!(
        text.contains("approvals.messages") && text.contains(" → auto"),
        "{text}"
    );
    let logged = gw
        .store()
        .last_setting_change("approvals.messages")
        .unwrap()
        .unwrap();
    assert_eq!(
        (logged.channel.as_deref(), logged.proposal),
        (Some("passkey"), Some(id))
    );
    let file = std::fs::read_to_string(gw.home.join("zephyr.toml")).unwrap();
    assert!(file.contains("messages = \"auto\""), "{file}");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_approval_removes_a_private_fact() {
    let (gw, phone, mut ws) = signed_in("sign-fact", "").await;
    let calls = r#"[call gateway {"command": "/facts add pin digits 12345678"}] [call gateway {"command": "/facts rm pin"}] go"#;
    let id = gw.proposed(calls).await;
    const FACTS: &str = "zephyr.facts_list";
    assert_eq!(
        ws.rpc(FACTS, json!({})).await["result"]["facts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let done = ws.rpc("zephyr.decide", signed).await["result"].take();
    assert_eq!(done["ok"], true, "{done}");
    assert_eq!(
        ws.rpc(FACTS, json!({})).await["result"]["facts"],
        json!([]),
        "{done}"
    );
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_approvals_on_web_are_refused() {
    let (gw, _phone, mut ws) = signed_in("typed-ok", "").await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    let id = gw.hold_send();
    for typed in [
        // Every verb that approves, grants, adds or undoes, in each parser, aliases included.
        format!("/ok {id}"),
        format!("/ok {id} once"),
        format!("/ok #{id}"),
        "/vault grant login bank".to_string(),
        "/vault grant token gh git".to_string(),
        "/memory ok".to_string(),
        "/memory undo".to_string(),
        "/job ok 1".to_string(),
        "/job add n | * * * * * | p".to_string(),
        "/job run 1".to_string(),
        "/skill ok notes".to_string(),
        "/skills ok notes".to_string(),
        "/skill add notes".to_string(),
        "/skills add notes".to_string(),
        "/skill undo notes".to_string(),
        "/skills undo notes".to_string(),
        "/mcp ok notes".to_string(),
    ] {
        ws.say(&typed).await;
        let reply = ws
            .event(|e| e["type"] == "zephyr_reply" && e["command"] == typed.as_str())
            .await;
        let text = reply["text"].as_str().unwrap();
        assert!(
            text.starts_with("[zephyr] approve with your passkey"),
            "{typed}: {text}"
        );
    }
    assert_eq!((gw.status(id), gw.ran()), ("pending".to_string(), vec![]));
    // What only reads or takes away is answered as on any channel.
    for typed in [
        "/vault",
        "/vault revoke bank",
        "/memory diff",
        "/job list",
        "/job rm 9",
        "/skills",
        "/skill no notes",
        "/skill diff notes",
        "/mcp",
    ] {
        ws.say(typed).await;
        let reply = ws
            .event(|e| e["type"] == "zephyr_reply" && e["command"] == typed)
            .await;
        let text = reply["text"].as_str().unwrap();
        assert!(!text.contains("passkey"), "{typed}: {text}");
    }
    // Refusing takes nothing away from the check, so it is typed freely.
    ws.say(&format!("/no {id}")).await;
    let no = format!("/no {id}");
    ws.event(|e| e["type"] == "zephyr_reply" && e["command"] == no.as_str())
        .await;
    assert_eq!(gw.status(id), "dropped");
    // Elsewhere a typed /ok is what it always was.
    let other = gw.hold_send();
    gw.say("chat", &format!("/ok {other}")).await;
    gw.sent("chat", |t| {
        t.starts_with(&format!("[zephyr] approved #{other}"))
    })
    .await;
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn what_is_signed_holds_no_countdown() {
    let (gw, phone, mut ws) = signed_in("sign-countdown", "").await;
    let id = gw.hold_send();
    let all = ws.rpc("zephyr.proposals", json!({})).await;
    let p = &all["result"]["proposals"][0];
    let (card, signed) = (p["card"].as_str().unwrap(), p["signed"].as_str().unwrap());
    assert!(card.contains("expires in 24 h"), "{card}");
    assert!(
        !signed.contains("expires in") && signed.ends_with(&format!("/ok {id} · /no {id}")),
        "{signed}"
    );
    assert_eq!(signed.lines().count(), card.lines().count());
    // Signed with 24 h left, approved with 22 h left: the same text, so the same signature.
    let signature = ws.sign_decide(&phone, id, "ok").await;
    let db = rusqlite::Connection::open(gw.home.join("state.db")).unwrap();
    db.execute(
        "UPDATE proposals SET created_at = created_at - 7200 WHERE id = ?1",
        [id],
    )
    .unwrap();
    let later = ws.rpc("zephyr.proposals", json!({})).await;
    assert!(later["result"]["proposals"][0]["card"]
        .as_str()
        .unwrap()
        .contains("expires in 22 h"));
    let done = ws.rpc("zephyr.decide", signature).await["result"].take();
    assert_eq!(done["ok"], true, "{done}");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_button_the_router_refuses_is_not_a_success() {
    let service = PushService::start().await;
    let (gw, cookie, _phone) = pushing("push-refused", "", &service).await;
    let id = gw.hold_send();
    gw.say("chat", "/pause").await;
    gw.sent("chat", |t| t.contains("paused")).await;
    let res = (gw
        .req(reqwest::Method::POST, "/api/push/decide")
        .header("Cookie", &cookie))
    .json(&json!({"id": id, "verb": "ok"}))
    .send()
    .await
    .unwrap();
    assert_eq!(
        res.status(),
        409,
        "the service worker opens the app on a refusal"
    );
    let said: Value = res.json().await.unwrap();
    assert_eq!(said["ok"], false, "{said}");
    assert!(said["text"].as_str().unwrap().contains("paused"), "{said}");
    assert_eq!((gw.status(id), gw.ran()), ("pending".to_string(), vec![]));
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_sign_in_starts_keeps_the_one_in_flight() {
    let gw = start("flood");
    gw.up().await;
    let phone = Authenticator::new("phone-cred");
    gw.enrol("phone", &phone).await;
    // The owner's phone is showing its passkey prompt.
    let start: Value = gw
        .post("/api/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let mut refused = 0;
    for _ in 0..40 {
        let res = gw.post("/api/login/start", json!({})).await;
        match res.status().as_u16() {
            200 => {}
            429 => {
                refused += 1;
                let said: Value = res.json().await.unwrap();
                assert_eq!(
                    said,
                    json!({"error": "too many sign-ins are waiting; try again in two minutes"})
                );
            }
            other => panic!("{other}"),
        }
    }
    assert_eq!(refused, 25, "16 wait at once: the owner's and 15 more");
    // Starts that were never finished used up neither the owner's challenge nor the minute's
    // attempts, and closed nothing.
    let body = with(
        phone.get(&start["publicKey"]),
        json!({"challengeId": start["challengeId"]}),
    );
    let done = gw.post("/api/login/finish", body).await;
    assert_eq!(done.status(), 200);
    set_cookie(&done);
    // A new device's enrolment, proven by its code, is never the one turned away.
    let code = gw.enrol_code("ipad").await;
    assert_eq!(
        gw.post("/api/enrol/start", json!({"code": code}))
            .await
            .status(),
        200
    );
    // The attempts that can guess something still take 20 a minute.
    let mut over = 0;
    for _ in 0..20 {
        let res = gw
            .post("/api/login/finish", json!({"challengeId": "nope"}))
            .await;
        over += usize::from(res.status() == 429);
    }
    assert!(over > 0, "finishes are counted");
    gw.shutdown().await;
}

// ---- the activity tab (A4's zephyr.activity) ----

#[tokio::test(flavor = "multi_thread")]
async fn activity_is_open_on_web_and_pages() {
    let gw = start("activity");
    gw.up().await;
    let store = gw.store();
    for n in 0..3 {
        store
            .audit("main", "bash", &format!("ls {n}"), "started")
            .unwrap();
    }
    store
        .remember_targets("messages", &["bob@x.org".into()])
        .unwrap();
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    let first = ws
        .rpc("zephyr.activity", json!({"kind": "tool", "limit": 1}))
        .await["result"]
        .take();
    let entries = first["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "{first}");
    assert!(
        entries[0]["line"]
            .as_str()
            .is_some_and(|l| l.contains("ls 2")),
        "{first}"
    );
    assert!(first["next"].is_string(), "more remain: {first}");
    let older = json!({"kind": "tool", "limit": 5, "before": first["next"]});
    let second = ws.rpc("zephyr.activity", older).await["result"].take();
    let lines: Vec<&str> = (second["entries"].as_array().unwrap().iter())
        .map(|e| e["line"].as_str().unwrap())
        .collect();
    assert!(
        lines.len() == 2 && lines[0].contains("ls 1") && lines[1].contains("ls 0"),
        "{second}"
    );
    assert!(second["next"].is_null());
    let bad = ws.rpc("zephyr.activity", json!({"limit": 0})).await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    // The grants, each with the command that takes it back.
    let grants = ws.rpc("zephyr.activity", json!({"kind": "grants"})).await["result"].take();
    assert!(grants["since"].is_null());
    let grant = &grants["entries"][0];
    assert_eq!(
        (&grant["kind"], &grant["revoke"]),
        (&json!("grant"), &json!("/approvals forget 1"))
    );
    // A revoke only takes away, so the app sends it as the owner's own command, on the cookie.
    ws.rpc("zephyr.subscribe", json!({})).await;
    ws.say("/approvals forget 1").await;
    ws.event(|e| e["type"] == "zephyr_reply" && e["command"] == "/approvals forget 1")
        .await;
    let grants = ws.rpc("zephyr.activity", json!({"kind": "grants"})).await["result"].take();
    assert_eq!(grants["entries"], json!([]), "{grants}");
    gw.shutdown().await;
}

// ---- on top of the goals and memory lane ----

#[tokio::test(flavor = "multi_thread")]
async fn a_message_typed_in_the_app_is_the_owners_and_keeps_the_conversation_clean() {
    let gw = start("web-owner-row");
    gw.up().await;
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    ws.say("hi").await;
    ws.event(|e| e["type"] == "message_end" && e["message"]["role"] == "assistant")
        .await;
    ws.event(|e| e["type"] == "agent_settled").await;
    let db = rusqlite::Connection::open(gw.home.join("state.db")).unwrap();
    let row: (String, i64) = db
        .query_row("SELECT channel, owner FROM inbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        row,
        ("web".to_string(), 1),
        "written by the router for the owner on web"
    );
    // So it does not count as text from elsewhere: the conversation is still the owner's own.
    let origin = gw.store().session_origin("main").unwrap();
    assert_eq!(origin, zephyr::store::SessionOrigin::Clean);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_auto_is_turned_on_only_with_the_passkey() {
    let (gw, phone, mut ws) = signed_in("sign-memory-auto", "").await;
    let change = json!({"key": "memory.auto", "value": "owner"});
    let unsigned = ws.rpc("zephyr.settings_set", change.clone()).await["result"].take();
    assert_eq!(unsigned["outcome"], "refused", "{unsigned}");
    assert_eq!(
        unsigned["reason"],
        "loosening memory.auto needs tui or chat or passkey"
    );
    // As the app lists it: a strong choice, so its row is saved with the passkey.
    let all = ws.rpc("zephyr.settings", json!({})).await["result"].take();
    let rows = all["sections"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["settings"].as_array().unwrap());
    let row = rows
        .clone()
        .find(|r| r["key"] == "memory.auto")
        .expect("listed")
        .clone();
    assert_eq!(
        (&row["strong"], &row["value"]),
        (&json!(true), &json!("off")),
        "{row}"
    );
    assert_eq!(row["kind"]["options"], json!(["off", "owner"]), "{row}");
    let signed = ws
        .sign(&phone, with(change.clone(), json!({"op": "set"})))
        .await;
    let done = ws.rpc("zephyr.settings_set", with(signed, change)).await["result"].take();
    assert_eq!(
        (&done["outcome"], &done["new"]),
        (&json!("changed"), &json!("owner")),
        "{done}"
    );
    let logged = gw
        .store()
        .last_setting_change("memory.auto")
        .unwrap()
        .unwrap();
    assert_eq!(logged.channel.as_deref(), Some("passkey"));
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn goals_are_read_on_web_and_changed_by_command() {
    let gw = start("goals");
    gw.up().await;
    let mut ws = gw.ws(&gw.cookie("phone")).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    assert_eq!(
        ws.rpc("zephyr.goals", json!({})).await["result"],
        json!({"goals": []})
    );
    // `/goal` is not a strong setting: it is the owner's own command, on the cookie.
    ws.say("/goal add ship W").await;
    let reply = ws
        .event(|e| e["type"] == "zephyr_reply" && e["command"] == "/goal add ship W")
        .await;
    assert_eq!(reply["channel"], "web");
    assert!(
        reply["text"]
            .as_str()
            .unwrap()
            .starts_with("[zephyr] goal 1 set"),
        "{reply}"
    );
    let goals = ws.rpc("zephyr.goals", json!({})).await["result"].take();
    assert_eq!(
        goals,
        json!({"goals": [{"n": 1, "text": "ship W", "done": false}]})
    );
    ws.say("/goal done 1").await;
    ws.event(|e| e["type"] == "zephyr_reply" && e["command"] == "/goal done 1")
        .await;
    let goals = ws.rpc("zephyr.goals", json!({})).await["result"].take();
    assert_eq!(
        goals,
        json!({"goals": [{"n": null, "text": "ship W", "done": true}]})
    );
    gw.shutdown().await;
}

// ---- review of PR #43 ----

#[tokio::test(flavor = "multi_thread")]
async fn a_command_proposals_signed_text_is_its_whole_command() {
    let (gw, phone, mut ws) = signed_in("sign-whole", "").await;
    // The agent's own text, shaped like a card's countdown.
    let rule = "/rule be careful · expires in 2 h, and bcc x@evil.com";
    let id = gw.store().propose_command(rule).unwrap();
    let all = ws.rpc("zephyr.proposals", json!({})).await;
    let p = &all["result"]["proposals"][0];
    assert_eq!(
        (&p["id"], &p["signed"], &p["card"]),
        (&json!(id), &json!(rule), &json!(rule))
    );
    assert!(p["signed"].as_str().unwrap().contains("bcc x@evil.com"));
    // What is signed is that text: a hash of anything shorter is refused.
    let short = data_encoding::HEXLOWER.encode(&sha256(b"/rule be careful"));
    let op = json!({"op": "decide", "id": id, "verb": "ok", "card_sha256": short});
    let answer = ws.rpc("zephyr.web_challenge", op).await;
    assert_eq!(
        answer["error"]["message"], "the card changed; look at it again",
        "{answer}"
    );
    let signed = ws.sign_decide(&phone, id, "ok").await;
    assert_eq!(ws.rpc("zephyr.decide", signed).await["result"]["ok"], true);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_approval_that_does_not_apply_says_so() {
    let (gw, phone, mut ws) = signed_in("sign-not-applied", "").await;
    for command in [
        "/vault grant login nosuch",
        "/facts rm nosuch",
        "/switch nosuch",
        "/set tasks.parallel 99999",
    ] {
        let id = gw.store().propose_command(command).unwrap();
        let signed = ws.sign_decide(&phone, id, "ok").await;
        let done = ws.rpc("zephyr.decide", signed).await["result"].take();
        assert_eq!(done["ok"], false, "{command}: {done}");
        let text = done["text"].as_str().unwrap();
        assert!(
            text.contains("did not apply") && !text.contains("approved"),
            "{command}: {text}"
        );
    }
    // One that applies still says so.
    let id = gw.store().propose_command("/set tasks.parallel 3").unwrap();
    let signed = ws.sign_decide(&phone, id, "ok").await;
    let done = ws.rpc("zephyr.decide", signed).await["result"].take();
    assert_eq!(done["ok"], true, "{done}");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_network_floor_the_web_app_does_not_start() {
    let home = new_home("no-floor");
    let port = free_port();
    let gw = start_in(&home, port, "", |cfg| cfg.web_off_floor = false);
    // The owner is told why when they ask for it; by then the listener would have been up.
    gw.say("chat", "/web enrol phone").await;
    let reply = gw.sent("chat", |_| true).await;
    let floor = "the web app needs the network floor; host.toml network = \"bridge\"";
    assert_eq!(reply, format!("[zephyr] {floor}"));
    gw.say("chat", "/web").await;
    assert_eq!(gw.sent("chat", |_| true).await, format!("[zephyr] {floor}"));
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "nothing listens"
    );
    assert_eq!(
        gw.store().meta("web_enrol").unwrap(),
        None,
        "and no code was made"
    );
    gw.shutdown().await;
}

// ---- desktop takeover ----

/// A stand-in for x11vnc: it logs its argv, its PATH and what its stdin and stdout are, then
/// sends back whatever it is sent.
const X11VNC: &str = r#"#!/bin/sh
d=$(dirname "$0")
fds="$(readlink /proc/$$/fd/0) $(readlink /proc/$$/fd/1)"
{
  printf 'pid %s\n' "$$"
  printf 'argv %s\n' "$*"
  printf 'path %s\n' "$PATH"
  printf 'fds %s\n' "$fds"
} >> "$d/x11vnc.log"
exec cat
"#;

type Vnc = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// A gateway with a desktop (`:99`, never started: only the fake x11vnc is run for it), and a
/// phone signed in with its own passkey: its socket, subscribed, and its cookie.
async fn with_desktop(
    name: &str,
    tweak: impl FnOnce(&mut Config),
) -> (Gateway, Authenticator, Ws, String) {
    use std::os::unix::fs::PermissionsExt;
    let home = new_home(name);
    let fake = home.join("x11vnc");
    std::fs::write(&fake, X11VNC).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let extra = "[approvals]\npreset = \"manual\"\n";
    let gw = start_in(&home, free_port(), extra, |cfg| {
        cfg.desktop = Some(":99".into());
        cfg.x11vnc = fake;
        tweak(cfg);
    });
    gw.up().await;
    let phone = Authenticator::new("phone-cred");
    let cookie = gw.enrol("phone", &phone).await;
    let mut ws = gw.ws(&cookie).await;
    ws.rpc("zephyr.subscribe", json!({})).await;
    (gw, phone, ws, cookie)
}

impl Ws {
    /// A signed "Take over": the whole answer.
    async fn take(&mut self, auth: &Authenticator) -> Value {
        let signed = self
            .sign(auth, json!({"op": "desktop", "verb": "take"}))
            .await;
        self.rpc("zephyr.desktop", with(signed, json!({"verb": "take"})))
            .await
    }

    /// A signed "Take over" that works: its ticket.
    async fn ticket(&mut self, auth: &Authenticator) -> String {
        let took = self.take(auth).await;
        let ticket = took["result"]["ticket"].as_str();
        ticket.unwrap_or_else(|| panic!("{took}")).to_string()
    }

    /// Waits for the snapshot to say who holds the desktop. What came before is dropped, so a
    /// later wait never reads an older state.
    async fn desktop(&mut self, owner: bool) -> Value {
        let status = self
            .event(|e| e["type"] == "zephyr_status" && e["snapshot"]["desktop"]["owner"] == owner)
            .await;
        self.events.clear();
        status["snapshot"]["desktop"].clone()
    }

    /// Waits for a notice that holds `text`.
    async fn told(&mut self, text: &str) {
        self.event(|e| e.to_string().contains(text)).await;
    }
}

impl Gateway {
    /// Asks for the takeover's socket as the app does, the ticket in the subprotocols.
    async fn vnc(&self, cookie: &str, ticket: Option<&str>) -> Result<Vnc, u16> {
        let mut req = format!("ws://{HOST}/api/vnc")
            .into_client_request()
            .unwrap();
        let protocols = match ticket {
            Some(ticket) => format!("zephyr-vnc, zt.{ticket}"),
            None => "zephyr-vnc".to_string(),
        };
        for (name, value) in [
            ("Origin", ORIGIN),
            ("Tailscale-User-Login", LOGIN),
            ("Cookie", cookie),
            ("Sec-WebSocket-Protocol", protocols.as_str()),
        ] {
            let name: reqwest::header::HeaderName = name.parse().unwrap();
            req.headers_mut().insert(name, value.parse().unwrap());
        }
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        match tokio_tungstenite::client_async(req, tcp).await {
            Ok((sock, res)) => {
                assert_eq!(res.headers()["sec-websocket-protocol"], "zephyr-vnc");
                Ok(sock)
            }
            Err(WsError::Http(res)) => Err(res.status().as_u16()),
            Err(e) => panic!("the upgrade failed oddly: {e}"),
        }
    }

    /// Root's flag that makes the agent's browser and xdotool wait.
    fn held_flag(&self) -> PathBuf {
        let name = self.home.file_name().unwrap().to_string_lossy();
        (self.home).with_file_name(format!("{name}-public/desktop-held"))
    }

    /// The fake x11vnc's lines that start with `key `, less the key.
    fn x11vnc(&self, key: &str) -> Vec<String> {
        let log = std::fs::read_to_string(self.home.join("x11vnc.log")).unwrap_or_default();
        log.lines()
            .filter_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
            .map(String::from)
            .collect()
    }

    /// The fake rpi's commands of `kind`, in the order it got them.
    fn rpi_log(&self, kind: &str) -> Vec<Value> {
        let log = std::fs::read_to_string(self.home.join("log.jsonl")).unwrap_or_default();
        log.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| kind.is_empty() || v["type"] == kind)
            .collect()
    }
}

/// Waits until `done` holds, looking again every 20 ms.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let until = std::time::Instant::now() + WAIT;
    while !done() {
        assert!(std::time::Instant::now() < until, "never: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sends `bytes` and reads as many back.
async fn echoed(sock: &mut Vnc, bytes: &[u8]) -> Vec<u8> {
    sock.send(Message::Binary(bytes.to_vec())).await.unwrap();
    let mut back = Vec::new();
    while back.len() < bytes.len() {
        let next = tokio::time::timeout(WAIT, sock.next()).await;
        match next.expect("bytes back in time") {
            Some(Ok(Message::Binary(part))) => back.extend(part),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("the socket ended: {other:?}"),
        }
    }
    back
}

/// Waits for the server to close the takeover's socket.
async fn vnc_closed(sock: &mut Vnc) {
    loop {
        let next = tokio::time::timeout(WAIT, sock.next()).await;
        match next.expect("the socket closes in time") {
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
            Some(Ok(_)) => {}
        }
    }
}

const OTHER: &str = "that signature is for another request";

#[tokio::test(flavor = "multi_thread")]
async fn take_over_needs_a_passkey_signature_and_give_does_not() {
    let (gw, phone, mut ws, _) = with_desktop("desk-sign", |_| {}).await;
    let unsigned = ws.rpc("zephyr.desktop", json!({"verb": "take"})).await;
    assert_eq!(
        unsigned["error"],
        json!({"code": -32602, "message": "approve with your passkey"})
    );
    // A signature for a setting or for a card does not take the desktop.
    let setting = json!({"op": "set", "key": "tasks.parallel", "value": "3"});
    let signed = ws.sign(&phone, setting).await;
    let answer = ws
        .rpc("zephyr.desktop", with(signed, json!({"verb": "take"})))
        .await;
    assert_eq!(answer["error"]["message"], OTHER, "{answer}");
    let card = gw.hold_send();
    let signed = ws.sign_decide(&phone, card, "ok").await;
    let answer = ws
        .rpc("zephyr.desktop", with(signed, json!({"verb": "take"})))
        .await;
    assert_eq!(answer["error"]["message"], OTHER, "{answer}");
    let hello = ws.rpc("zephyr.hello", json!({})).await;
    assert_eq!(
        hello["result"]["desktop"],
        json!({"owner": false, "since_unix": 0})
    );
    assert!(!gw.held_flag().exists());
    // Nor does one for the desktop approve a card or change a setting.
    let desk = json!({"op": "desktop", "verb": "take"});
    let signed = ws.sign(&phone, desk.clone()).await;
    let answer = ws
        .rpc(
            "zephyr.decide",
            with(signed, json!({"id": card, "verb": "ok"})),
        )
        .await;
    assert_eq!(answer["error"]["message"], OTHER, "{answer}");
    assert_eq!(gw.status(card), "pending");
    let signed = ws.sign(&phone, desk.clone()).await;
    let change = json!({"key": "tasks.parallel", "value": "3"});
    let answer = ws.rpc("zephyr.settings_set", with(signed, change)).await;
    assert_eq!(answer["error"]["message"], OTHER, "{answer}");
    // Another passkey's signature takes nothing.
    let forged = ws
        .sign(&Authenticator::new("phone-cred"), desk.clone())
        .await;
    let answer = ws
        .rpc("zephyr.desktop", with(forged, json!({"verb": "take"})))
        .await;
    assert_eq!(
        answer["error"]["message"],
        "the passkey signature did not verify"
    );
    // Signed: a ticket, and root's flag is there. The same signature does not work again.
    let signed = with(ws.sign(&phone, desk).await, json!({"verb": "take"}));
    let took = ws.rpc("zephyr.desktop", signed.clone()).await;
    let ticket = took["result"]["ticket"].as_str().expect("a ticket");
    assert_eq!(b64_decode(&json!(ticket)).len(), 32, "{took}");
    assert!(ws.desktop(true).await["since_unix"].as_i64().unwrap() > 0);
    assert!(gw.held_flag().exists());
    let again = ws.rpc("zephyr.desktop", signed).await;
    assert_eq!(
        again["error"]["message"],
        "that signature expired or was used; tap again"
    );
    // Another device is told who has it, and may hand it back: that only takes away.
    let mut ipad = gw.ws(&gw.cookie("ipad")).await;
    let give = ipad.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    assert!(give["result"].is_object(), "{give}");
    ws.desktop(false).await;
    assert!(!gw.held_flag().exists());
    let odd = ws.rpc("zephyr.desktop", json!({"verb": "keep"})).await;
    assert_eq!(odd["error"]["message"], "verb is take or give");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_desktop_there_is_nothing_to_take() {
    let (gw, phone, mut ws) = signed_in("desk-none", "").await;
    let hello = ws.rpc("zephyr.hello", json!({})).await;
    assert!(hello["result"]["desktop"].is_null(), "{hello}");
    let took = ws.take(&phone).await;
    assert_eq!(took["error"]["message"], "there is no desktop", "{took}");
    gw.shutdown().await;
}

/// Review Focus 4. The 30 s are `web::vnc`'s own test: no ticket here is that old.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_works_once_for_its_session_within_30_s() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-ticket", |_| {}).await;
    let ticket = ws.ticket(&phone).await;
    assert_eq!(gw.vnc(&cookie, None).await.err(), Some(401), "no ticket");
    let made_up = b64(&[7; 32]);
    assert_eq!(gw.vnc(&cookie, Some(&made_up)).await.err(), Some(401));
    // No cookie: refused before the ticket is looked at.
    assert_eq!(gw.vnc("", Some(&ticket)).await.err(), Some(401));
    // Another signed-in session with this one's ticket, which is then spent.
    let tablet = gw.cookie("tablet");
    assert_eq!(gw.vnc(&tablet, Some(&ticket)).await.err(), Some(401));
    assert_eq!(gw.vnc(&cookie, Some(&ticket)).await.err(), Some(401));
    let ticket = ws.ticket(&phone).await;
    let mut sock = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    assert_eq!(echoed(&mut sock, b"hi").await, b"hi");
    sock.close(None).await.unwrap();
    vnc_closed(&mut sock).await;
    assert_eq!(
        gw.vnc(&cookie, Some(&ticket)).await.err(),
        Some(401),
        "used"
    );
    // After the hand back a ticket opens nothing.
    let ticket = ws.ticket(&phone).await;
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    assert_eq!(gw.vnc(&cookie, Some(&ticket)).await.err(), Some(401));
    // And the socket is the app's own origin's, like `/api/ws`.
    let mut req = format!("ws://{HOST}/api/vnc")
        .into_client_request()
        .unwrap();
    for (name, value) in [("Tailscale-User-Login", LOGIN), ("Cookie", cookie.as_str())] {
        let name: reqwest::header::HeaderName = name.parse().unwrap();
        req.headers_mut().insert(name, value.parse().unwrap());
    }
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", gw.port))
        .await
        .unwrap();
    match tokio_tungstenite::client_async(req, tcp).await {
        Err(WsError::Http(res)) => assert_eq!(res.status(), 403),
        other => panic!("{:?}", other.map(drop)),
    }
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_vnc_socket_is_refused_and_a_text_frame_closes_the_first() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-one", |_| {}).await;
    // Two tickets, asked for before any socket opened: the second opens no second socket.
    let (ticket, spare) = (ws.ticket(&phone).await, ws.ticket(&phone).await);
    let mut first = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    assert_eq!(gw.vnc(&cookie, Some(&spare)).await.err(), Some(503));
    assert_eq!(echoed(&mut first, b"still here").await, b"still here");
    // The holder takes over again (a phone that loaded the page anew, its old socket not yet
    // dead): the old socket is ended, and the new ticket's is served.
    let ticket = ws.ticket(&phone).await;
    let mut second = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    vnc_closed(&mut first).await;
    assert_eq!(echoed(&mut second, b"again").await, b"again");
    second.send(Message::Text("hello".into())).await.unwrap();
    vnc_closed(&mut second).await;
    // Its place is free again.
    let ticket = ws.ticket(&phone).await;
    let mut next = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    // A frame over 1 MiB closes it too, maybe before the whole of it is sent.
    let _ = next.send(Message::Binary(vec![0; (1 << 20) + 1])).await;
    vnc_closed(&mut next).await;
    // And so does the hand back.
    let ticket = ws.ticket(&phone).await;
    let mut last = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    vnc_closed(&mut last).await;
    gw.shutdown().await;
}

/// A phone that loaded the page anew while its old socket is stalled (its client stopped
/// reading, so the bridge is blocked in a write): the old one is cut at once, and the new one
/// is served, not refused as a second.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_old_socket_does_not_refuse_the_holders_new_one() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-stall", |_| {}).await;
    let ticket = ws.ticket(&phone).await;
    let mut old = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    // Sent and never read back: what x11vnc answers piles up until the bridge's write blocks,
    // and then it takes no more from this side either.
    let frame = vec![3u8; 1 << 16];
    let mut stalled = false;
    for _ in 0..4096 {
        let sent = old.send(Message::Binary(frame.clone()));
        if tokio::time::timeout(Duration::from_millis(500), sent)
            .await
            .is_err()
        {
            stalled = true;
            break;
        }
    }
    assert!(stalled, "the old socket never stalled");
    let asked = std::time::Instant::now();
    let ticket = ws.ticket(&phone).await;
    let mut new = gw
        .vnc(&cookie, Some(&ticket))
        .await
        .expect("the new socket is served");
    assert!(
        asked.elapsed() < Duration::from_secs(4),
        "the take waited out the old socket: {:?}",
        asked.elapsed()
    );
    assert_eq!(echoed(&mut new, b"here").await, b"here");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bytes_go_to_x11vnc_and_back() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-bytes", |_| {}).await;
    let ticket = ws.ticket(&phone).await;
    let mut sock = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    assert_eq!(echoed(&mut sock, b"RFB 003.008\n").await, b"RFB 003.008\n");
    // No cap on how many: a pointer drag sends hundreds, where `/api/ws` closes at 60 a minute.
    for i in 0..200u8 {
        let frame = [5, 1, 0, i, 0, i];
        assert_eq!(echoed(&mut sock, &frame).await, frame);
    }
    assert_eq!(
        ws.rpc("zephyr.hello", json!({})).await["result"]["desktop"]["owner"],
        true
    );
    // The session ending closes it at once, like the chat socket.
    gw.say("chat", "/web signout").await;
    vnc_closed(&mut sock).await;
    gw.shutdown().await;
}

/// Review Focus 3: nothing for the agent to connect to.
#[tokio::test(flavor = "multi_thread")]
async fn x11vnc_is_started_with_inetd_and_listens_nowhere() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-inetd", |_| {}).await;
    let ticket = ws.ticket(&phone).await;
    let mut sock = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    assert_eq!(echoed(&mut sock, b"up").await, b"up");
    let argv = gw.x11vnc("argv");
    let auth = gw.home.join(".zephyr-xauthority");
    assert_eq!(
        argv,
        [format!(
            "-inetd -once -norc -nocmds -noremote -nossl -quiet -display :99 -auth {}",
            auth.display()
        )]
    );
    for listens in ["-listen", "-rfbport", "-autoport", "-unixsock", "-connect"] {
        assert!(!argv[0].contains(listens), "{argv:?}");
    }
    // Run for the agent (`agent::cmd`), and its stdin and stdout are one unnamed socket.
    assert_eq!(gw.x11vnc("path"), [zephyr::agent::AGENT_PATH]);
    let fds = gw.x11vnc("fds").remove(0);
    let (stdin, stdout) = fds.split_once(' ').unwrap();
    assert!(stdin.starts_with("socket:[") && stdin == stdout, "{fds}");
    // It is killed when the socket ends: gone, or waiting to be reaped.
    let stat = format!("/proc/{}/stat", gw.x11vnc("pid")[0]);
    let running = || {
        let stat = std::fs::read_to_string(&stat).unwrap_or_default();
        let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
        !matches!(state, None | Some(Some('Z')))
    };
    assert!(running());
    drop(sock);
    let until = std::time::Instant::now() + WAIT;
    while running() {
        assert!(
            std::time::Instant::now() < until,
            "x11vnc outlived its socket"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    gw.shutdown().await;
}

/// Review Focus 5: a phone that locks mid-takeover does not stop the agent's browser for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_socket_gives_the_desktop_back_after_the_wait() {
    let (gw, phone, mut ws, cookie) = with_desktop("desk-gone", |cfg| {
        cfg.desktop_gone = Duration::from_millis(200);
    })
    .await;
    let ticket = ws.ticket(&phone).await;
    ws.desktop(true).await;
    let sock = gw.vnc(&cookie, Some(&ticket)).await.expect("it opens");
    assert!(gw.held_flag().exists());
    drop(sock); // no close frame: the phone just went
    ws.told("[zephyr] the desktop went back to the agent after the app closed")
        .await;
    ws.desktop(false).await;
    assert!(!gw.held_flag().exists(), "the wrappers would still wait");
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_takeover_gives_the_desktop_back() {
    let (gw, phone, mut ws, _) = with_desktop("desk-idle", |cfg| {
        cfg.desktop_idle = Duration::from_millis(200);
    })
    .await;
    ws.ticket(&phone).await;
    ws.told("[zephyr] the desktop went back to the agent after 10 min without input")
        .await;
    ws.desktop(false).await;
    assert!(!gw.held_flag().exists());
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_takeover_ends_at_its_cap() {
    let (gw, phone, mut ws, _) = with_desktop("desk-cap", |cfg| {
        cfg.desktop_cap = Duration::from_millis(200);
    })
    .await;
    ws.ticket(&phone).await;
    ws.told("[zephyr] the desktop went back to the agent after 60 min")
        .await;
    ws.desktop(false).await;
    assert!(!gw.held_flag().exists());
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn main_is_told_at_take_and_at_hand_back_and_stays_clean() {
    let (gw, phone, mut ws, _) = with_desktop("desk-notes", |_| {}).await;
    let settled = |e: &Value| e["type"] == "agent_settled";
    let steers = |gw: &Gateway| -> Vec<String> {
        gw.rpi_log("queue_message")
            .iter()
            .map(|c| {
                assert_eq!(c["deliverAs"], "steer");
                c["message"].as_str().unwrap().to_string()
            })
            .collect()
    };
    ws.say("one").await;
    ws.event(settled).await;
    // Main is idle: nothing is steered at the take, and the hand-back note waits.
    ws.ticket(&phone).await;
    ws.desktop(true).await;
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    ws.desktop(false).await;
    assert_eq!(steers(&gw), [""; 0]);
    // It goes in front of the next prompt: rpi delivers a steer queued while idle as the run
    // starts.
    ws.say("two").await;
    ws.event(settled).await;
    let notes = steers(&gw);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("[zephyr] the owner used the desktop from ")
            && notes[0]
                .ends_with(". The page may have changed: take a fresh snapshot before acting."),
        "{notes:?}"
    );
    let order: Vec<String> = (gw.rpi_log("").iter())
        .filter(|c| c["type"] == "queue_message" || c["type"] == "prompt")
        .map(|c| c["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(order, ["prompt", "queue_message", "prompt"]);
    // In a run: both notes are steered in as they happen.
    ws.say("[hang] three").await;
    ws.event(|e| e["type"] == "message_start" && e.to_string().contains("[hang] three"))
        .await;
    ws.ticket(&phone).await;
    ws.desktop(true).await;
    let notes = steers(&gw);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[1].starts_with("[zephyr] the owner took over the desktop at ")
            && notes[1].ends_with(
                "; browser and desktop commands wait until they hand it back. Do other work \
                 meanwhile."
            ),
        "{notes:?}"
    );
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    ws.desktop(false).await;
    let notes = steers(&gw);
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(
        notes[2].starts_with("[zephyr] the owner used the desktop from "),
        "{notes:?}"
    );
    // A take and a hand back made back to back are still two notes, in that order.
    ws.ticket(&phone).await;
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    eventually("both notes", || steers(&gw).len() == 5).await;
    let notes = steers(&gw);
    assert!(
        notes[3].starts_with("[zephyr] the owner took over the desktop at ")
            && notes[4].starts_with("[zephyr] the owner used the desktop from "),
        "{notes:?}"
    );
    ws.say("/stop").await;
    ws.event(settled).await;
    // The daemon's own notes: the conversation is still the owner's alone.
    let origin = gw.store().session_origin("main").unwrap();
    assert_eq!(origin, zephyr::store::SessionOrigin::Clean);
    gw.shutdown().await;
}

/// The ticket is the owner's only once nothing of the agent's is on the desktop: a command
/// still running after the wait is killed first.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_still_running_is_killed_before_the_ticket_is_issued() {
    use std::io::BufRead;
    use std::os::unix::process::ExitStatusExt;
    let (gw, phone, mut ws, _) = with_desktop("desk-drain", |cfg| {
        cfg.desktop_drain = Duration::from_millis(300);
    })
    .await;
    let lock = gw.held_flag().with_file_name("desktop.lock");
    let script = "import fcntl, sys, time\nf = open(sys.argv[1])\n\
                  fcntl.flock(f, fcntl.LOCK_SH)\nprint('placed', flush=True)\ntime.sleep(600)\n";
    let mut stuck = std::process::Command::new("python3")
        .args(["-I", "-c", script])
        .arg(&lock)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(stuck.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "placed");
    ws.ticket(&phone).await;
    // Already dead when the ticket is in hand.
    let ended = stuck.try_wait().unwrap();
    assert_eq!(
        ended.and_then(|status| status.signal()),
        Some(9),
        "{ended:?}"
    );
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    gw.shutdown().await;
}

/// A restart or a re-exec ends a takeover: the owner and main are told, never left guessing.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_during_a_takeover_tells_the_owner_and_main() {
    let home = new_home("desk-restart");
    let gw = start_in(&home, free_port(), "", |_| {});
    gw.up().await;
    gw.say("chat", "one").await;
    gw.sent("chat", |t| t.contains("echo: ")).await;
    let flag = gw.held_flag();
    gw.shutdown().await;
    // A flag with no take on record ends no takeover: it is removed, and nothing is said.
    std::fs::write(&flag, "50400\n").unwrap();
    let gw = start_in(&home, free_port(), "", |_| {});
    gw.up().await;
    gw.say("chat", "idle").await;
    gw.sent("chat", |t| t.contains("echo: ")).await;
    assert!(!flag.exists());
    assert_eq!(gw.rpi_log("queue_message").len(), 0);
    // The daemon went down while the owner held the desktop: its own record says since when.
    gw.store().set_meta("desktop_take", Some("50400")).unwrap();
    gw.shutdown().await;
    std::fs::write(&flag, "1\n").unwrap();
    let gw = start_in(&home, free_port(), "", |_| {});
    gw.sent("chat", |t| {
        t == "[zephyr] the desktop went back to the agent when zephyr restarted"
    })
    .await;
    assert!(!flag.exists());
    gw.say("chat", "two").await;
    gw.sent("chat", |t| t.contains("echo: ")).await;
    let notes = gw.rpi_log("queue_message");
    assert_eq!(notes.len(), 1, "{notes:?}");
    let note = notes[0]["message"].as_str().unwrap();
    assert!(
        note.starts_with("[zephyr] the owner used the desktop from ")
            && note.ends_with(". The page may have changed: take a fresh snapshot before acting."),
        "{note}"
    );
    assert_eq!(gw.store().meta("desktop_take").unwrap(), None);
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_during_a_takeover_is_refused() {
    let (gw, phone, mut ws, _) = with_desktop("desk-fill", |_| {}).await;
    let vault = "[[login]]\nname = \"github\"\nsite = \"https://github.com\"\n\
                 username = \"aiden\"\npassword = \"hunter2-secret\"\ngranted = true\n";
    std::fs::write(gw.home.join("vault.toml"), vault).unwrap();
    ws.ticket(&phone).await;
    ws.desktop(true).await;
    ws.say(r##"[call vault {"action": "fill", "name": "github", "password": "#pw"}]"##)
        .await;
    let reply = ws
        .event(|e| e["type"] == "message_end" && e.to_string().contains("vault: "))
        .await;
    assert!(
        reply.to_string().contains(
            "vault: not filled: the owner has the desktop; try again when they hand it back"
        ),
        "{reply}"
    );
    // The fake browser was never asked: nothing typed into the page under the owner's hands.
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    ws.desktop(false).await;
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_shows_who_holds_the_desktop() {
    let (gw, phone, mut ws, _) = with_desktop("desk-snap", |_| {}).await;
    let hello = ws.rpc("zephyr.hello", json!({})).await;
    assert_eq!(
        hello["result"]["desktop"],
        json!({"owner": false, "since_unix": 0})
    );
    let before = zephyr::now();
    ws.ticket(&phone).await;
    let held = ws.desktop(true).await;
    let since = held["since_unix"].as_i64().unwrap();
    assert!(since >= before && since <= zephyr::now(), "{held}");
    // A second device is told another has it.
    let tablet = Authenticator::new("tablet-cred");
    let mut other = gw.ws(&gw.enrol("tablet", &tablet).await).await;
    let refused = other.take(&tablet).await;
    assert_eq!(
        refused["error"]["message"], "another device has the desktop",
        "{refused}"
    );
    ws.rpc("zephyr.desktop", json!({"verb": "give"})).await;
    assert_eq!(
        ws.desktop(false).await,
        json!({"owner": false, "since_unix": 0})
    );
    gw.shutdown().await;
}
