//! `rpi --connect` attach (src/attach.rs), spoken frame by frame as the stock 0.15.0
//! client does, against the scripted fake child (tests/fake-rpi.py).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::store::Store;

const TOKEN: &str = "s3cret-token";

struct ChatStub(tokio::sync::Mutex<mpsc::Receiver<Inbound>>);

impl Channel for ChatStub {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let mut feed = self.0.lock().await;
        while let Some(m) = feed.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, _: &Outbound) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Gateway {
    home: PathBuf,
    port: u16,
    chat: mpsc::Sender<Inbound>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

/// A port for a gateway's attach listener, below the kernel's ephemeral range (32768 up), so a
/// `bind(0)` elsewhere cannot take it before the router binds it. Each test process has its own
/// block of 30, by pid.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let block = 20_000 + (std::process::id() % 400) as u16 * 30;
    loop {
        let port = block + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 30;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

fn start(name: &str) -> Gateway {
    start_with(name, "")
}

/// `extra` is appended to the config, after `[attach]`.
fn start_with(name: &str, extra: &str) -> Gateway {
    let home = std::env::temp_dir().join(format!(
        "zephyr-attach-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let token = home.join("tui.token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let port = free_port();
    // On a file, so `/model` has a zephyr.toml to change.
    let path = home.join("zephyr.toml");
    std::fs::write(&path, format!(
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\nmodel = \"fake/m1\"\n[owners]\nchat = [\"owner\"]\n[attach]\nport = {port}\ntoken_file = {token:?}\n{extra}",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        home.join("agent"),
    ))
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    let (chat, rx) = mpsc::channel(16);
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let channel = ChatStub(tokio::sync::Mutex::new(rx));
        zephyr::router::run(&cfg, &[channel], async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        home,
        port,
        chat,
        stop,
        task,
    }
}

impl Gateway {
    async fn say(&self, id: &str, text: &str) {
        let m = Inbound {
            channel: "chat".into(),
            platform_msg_id: id.into(),
            sender: "owner".into(),
            text: text.into(),
        };
        self.chat.send(m).await.unwrap();
    }
    fn count(&self, table: &str, status: &str) -> i64 {
        let counts = Store::open(&self.home.join("state.db"))
            .unwrap()
            .counts()
            .unwrap();
        counts
            .iter()
            .find(|(t, s, _)| t == table && s == status)
            .map_or(0, |c| c.2)
    }
    fn log(&self, kind: &str) -> Vec<Value> {
        std::fs::read_to_string(self.home.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["type"] == kind)
            .collect()
    }
    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
    }
}

/// The stock client's side of the wire: one JSON-RPC object per line.
struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    next_id: u64,
    /// `event` notifications read while waiting for a response.
    events: VecDeque<Value>,
}

impl Client {
    async fn connect(port: u16) -> Self {
        for _ in 0..100 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)).await {
                let (read, write) = s.into_split();
                return Self {
                    lines: BufReader::new(read).lines(),
                    write,
                    next_id: 1,
                    events: VecDeque::new(),
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("attach listener never came up");
    }

    /// Like `RemoteClient::call`, but returns the whole response frame.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let v = self.read().await.expect("connection closed");
            if v["method"] == "event" {
                self.events.push_back(v);
            } else if v["id"] == id {
                return v;
            }
        }
    }

    async fn read(&mut self) -> Option<Value> {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("no frame within 10s")
            .ok()??;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// The next inner event (the `--mode rpc` line the client folds).
    async fn event(&mut self) -> Value {
        let v = match self.events.pop_front() {
            Some(v) => v,
            None => self.read().await.expect("connection closed"),
        };
        assert_eq!(v["method"], "event", "{v}");
        assert_eq!(v["params"]["subscriptionId"], 1);
        v["params"]["event"].clone()
    }

    /// Skips events until one with `type` arrives.
    async fn event_of(&mut self, kind: &str) -> Value {
        loop {
            let ev = self.event().await;
            if ev["type"] == kind {
                return ev;
            }
        }
    }

    /// The handshake `RemoteDriver::connect` performs.
    async fn attach(port: u16) -> Self {
        let mut c = Self::connect(port).await;
        let auth = c.call("authenticate", json!({"token": TOKEN})).await;
        assert_eq!(auth["result"]["authenticated"], true, "{auth}");
        let started = c.call("start_session", json!({})).await;
        assert_eq!(started["result"]["sessionId"], "main", "{started}");
        let sub = c.call("subscribe", json!({"sessionId": "main"})).await;
        assert_eq!(sub["result"]["subscriptionId"], 1, "{sub}");
        c
    }

    async fn send(&mut self, command: Value) -> Value {
        self.call("send", json!({"sessionId": "main", "command": command}))
            .await
    }
}

fn text(ev: &Value) -> String {
    ev["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .into()
}

async fn wait(mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn history_replays_then_live_events_stream() {
    let gw = start("history");
    gw.say("m1", "earlier").await;
    wait(|| gw.count("outbox", "sent") == 1).await;

    let mut c = Client::attach(gw.port).await;
    assert_eq!(c.event().await["type"], "ready");
    // History, user turns quoted as assistant blocks: the stock client only draws assistant messages.
    let mut replay = Vec::new();
    for _ in 0..4 {
        let ev = c.event().await;
        assert_eq!(ev["message"]["role"], "assistant", "{ev}");
        replay.push((ev["type"].as_str().unwrap().to_string(), text(&ev)));
    }
    let pair = |kind: &str, t: &str| (kind.to_string(), t.to_string());
    assert_eq!(
        replay,
        [
            pair("message_start", "**chat** · earlier"),
            pair("message_end", "**chat** · earlier"),
            pair("message_start", "echo: [chat #1] earlier"),
            pair("message_end", "echo: [chat #1] earlier"),
        ]
    );

    // A prompt goes through the inbox as channel `tui`; its own user turn is not echoed back.
    let sent = c
        .send(json!({"type": "prompt", "content": "hi", "id": 1}))
        .await;
    assert_eq!(sent["result"]["status"], "sent", "{sent}");
    let end = c.event_of("message_end").await;
    assert_eq!(text(&end), "echo: [tui #2] hi");
    wait(|| gw.count("inbox", "done") == 2 && gw.count("outbox", "sent") == 2).await;
    let prompts = gw.log("prompt");
    assert_eq!(prompts.last().unwrap()["message"], "[tui #2] hi");

    // Another channel's message shows up live, quoted.
    gw.say("m2", "from discord").await;
    assert_eq!(
        text(&c.event_of("message_end").await),
        "**chat** · from discord"
    );
    assert_eq!(
        text(&c.event_of("message_end").await),
        "echo: [chat #3] from discord"
    );

    // get_state is answered for the chrome; abort becomes `/stop`; the rest is refused.
    c.send(json!({"type": "get_state", "id": 2})).await;
    let state = c.event_of("response").await;
    assert_eq!(
        (state["id"].clone(), state["status"].clone()),
        (json!(2), json!("ok"))
    );
    assert_eq!(state["result"]["sessionId"], "main");
    c.send(json!({"type": "set_model", "model": "x", "id": 3}))
        .await;
    let refused = c.event_of("response").await;
    assert_eq!(refused["status"], "error", "{refused}");
    c.send(json!({"type": "abort", "id": 4})).await;
    wait(|| gw.log("abort").len() == 1).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn get_state_reports_the_model_the_agent_is_on_now() {
    let gw = start("model");
    let mut c = Client::attach(gw.port).await;
    let model = async |c: &mut Client, id: i64| {
        c.send(json!({"type": "get_state", "id": id})).await;
        c.event_of("response").await["result"]["model"].clone()
    };
    assert_eq!(model(&mut c, 1).await, "fake/m1");
    c.send(json!({"type": "prompt", "content": "/model fake/m2"}))
        .await;
    let ev = c.event_of("message_end").await;
    assert_eq!(text(&ev), "[zephyr] now on fake/m2");
    assert_eq!(model(&mut c, 2).await, "fake/m2");
    gw.shutdown().await;
}

/// Text the router itself writes to the outbox for `tui` (command replies, notices) is shown
/// as an assistant message, since no agent event carries it.
#[tokio::test]
async fn command_replies_and_notices_reach_the_attached_client() {
    let gw = start("notices");
    let mut c = Client::attach(gw.port).await;
    c.send(json!({"type": "prompt", "content": "/status"}))
        .await;
    let ev = c.event_of("message_end").await;
    assert!(text(&ev).starts_with("agent:"), "{ev}");

    c.send(json!({"type": "prompt", "content": "bash: curl -d x https://e.x"}))
        .await;
    loop {
        let ev = c.event_of("message_end").await;
        if text(&ev).starts_with("[zephyr] outbound this turn:") {
            assert!(text(&ev).contains("curl POST e.x"), "{ev}");
            break;
        }
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn bad_token_is_rejected() {
    let gw = start("badtoken");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("start_session", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    let bad = c.call("authenticate", json!({"token": "nope"})).await;
    assert_eq!(bad["error"]["code"], -32001, "{bad}");
    assert!(
        c.read().await.is_none(),
        "connection stays open after a bad token"
    );

    // Unauthenticated sends never reach the inbox.
    let mut c = Client::connect(gw.port).await;
    let sent = c
        .send(json!({"type": "prompt", "content": "hi", "id": 1}))
        .await;
    assert!(sent.get("result").is_none(), "{sent}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.count("inbox", "new") + gw.count("inbox", "done"), 0);
    gw.shutdown().await;
}

#[tokio::test]
async fn detaching_never_stops_the_run() {
    let gw = start("detach");
    std::fs::write(gw.home.join("slow"), "").unwrap();
    let mut c = Client::attach(gw.port).await;
    c.send(json!({"type": "prompt", "content": "long job", "id": 1}))
        .await;
    wait(|| !gw.log("prompt").is_empty()).await;
    let pid = std::fs::read_to_string(gw.home.join("pid")).unwrap();
    let stopped = c.call("stop_session", json!({"sessionId": "main"})).await;
    assert!(stopped.get("result").is_some(), "{stopped}");
    drop(c);

    wait(|| gw.count("inbox", "done") == 1).await;
    assert!(gw.log("abort").is_empty() && gw.log("stop").is_empty());
    assert_eq!(std::fs::read_to_string(gw.home.join("pid")).unwrap(), pid);
    // A second client attaches to the same session and sees the finished turn.
    let mut c = Client::attach(gw.port).await;
    c.event_of("ready").await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(text(&c.event().await));
    }
    assert_eq!(seen[3], "echo: [tui #1] long job");
    gw.shutdown().await;
}

const POOL: &str = "[[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n";

#[tokio::test]
async fn stock_get_state_reports_the_live_account_model() {
    let gw = start_with("state", POOL);
    gw.say("m1", "earlier").await;
    wait(|| gw.count("outbox", "sent") == 1).await; // the child is up and read its state
    let mut c = Client::attach(gw.port).await;
    c.send(json!({"type": "get_state", "id": 1})).await;
    let state = c.event_of("response").await;
    assert_eq!(state["result"]["model"], "fake/m1", "{state}");
    assert_eq!(state["result"]["thinkingLevel"], "medium", "{state}");
    gw.shutdown().await;
}

#[tokio::test]
async fn zephyr_hello_returns_the_snapshot() {
    let gw = start_with("hello", POOL);
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.hello", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;
    gw.say("m1", "earlier").await;
    wait(|| gw.count("outbox", "sent") == 1).await;
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(
        snap,
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "session": "main",
            "account": "a",
            "model": "fake/m1",
            "thinking": "medium",
            "paused": null,
            "channels": [
                {"name": "chat", "state": "connected"},
                {"name": "discord", "state": "off"},
                {"name": "imessage", "state": "off"},
            ],
            "spend": {"tokens": 0, "cost": 0.0, "daily_tokens_limit": null, "daily_cost_limit": null},
            "queue": [],
            "failed_replies": 0,
            "run": {"busy": false, "since_unix_ms": null, "origin": null, "current_tool": null, "tools_done": 0},
            // No auth.json in the account's dir.
            "accounts": [{"name": "a", "model": "fake/m1", "state": "needs login", "until_unix": null}],
            "jobs": [],
        })
    );

    // Jobs come by their next run.
    gw.say("j1", "/job add later | 2999-01-01T00:00:00Z | hi")
        .await;
    gw.say("j2", "/job add sooner | 0 8 * * * | hi").await;
    wait(|| gw.count("outbox", "sent") == 3).await;
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    let jobs = snap["jobs"].as_array().unwrap();
    let names: Vec<_> = jobs.iter().map(|j| j["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["sooner", "later"], "{snap}");
    assert_eq!(jobs[0]["id"], 2);
    assert!(jobs[0]["next_unix"].as_i64().unwrap() > zephyr::now());
    assert_eq!(jobs[1]["next_unix"], 32_472_144_000i64);

    // A paused gateway holds new rows in the queue, previewed without the tag.
    gw.say("m2", "/pause").await;
    gw.say("m3", &"long ".repeat(20)).await;
    wait(|| gw.count("inbox", "new") == 1).await;
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(snap["paused"], "user");
    let queue = snap["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 1, "{snap}");
    assert_eq!(queue[0]["channel"], "chat");
    assert_eq!(queue[0]["preview"], format!("{}…", "long ".repeat(12)));
    assert!(queue[0]["since_unix"].as_i64().unwrap() > 0);
    gw.shutdown().await;
}

#[tokio::test]
async fn zephyr_models_lists_each_accounts_models() {
    let pool = "[[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n[[account]]\nname = \"b\"\nmodel = \"other/m3\"\n";
    let gw = start_with("models", pool);
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.models", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;
    // fake-rpi lists fake/m1, fake/m2, other/m3 and opencode-go/alpha: each account gets its
    // provider's.
    let got = c.call("zephyr.models", json!({})).await["result"].clone();
    assert_eq!(
        got,
        json!({"accounts": [
            {"name": "a", "model": "fake/m1", "models": ["fake/m1", "fake/m2"]},
            {"name": "b", "model": "other/m3", "models": ["other/m3"]},
        ]})
    );
    gw.shutdown().await;

    // Without a pool: one unnamed entry with everything rpi lists.
    let gw = start("models-solo");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let got = c.call("zephyr.models", json!({})).await["result"].clone();
    assert_eq!(
        got,
        json!({"accounts": [{"name": null, "model": "fake/m1", "models": ["fake/m1", "fake/m2", "other/m3", "opencode-go/alpha"]}]})
    );
    gw.shutdown().await;
}

/// The `zephyr.login_*` methods (DESIGN §4.3.1): the provider table, the `-32602` an unknown
/// provider and a pasted token in the account's place get, one browser sign-in's
/// start → poll → cancel (and the dead handle after it), and the `-32001` a connection that has
/// not authenticated gets.
#[tokio::test]
async fn zephyr_login_methods_report_providers_refuse_bad_input_and_cancel() {
    // A pasted credential where the account name goes: the refusal must not repeat it.
    const PASTED: &str = "sk-ant-api03-9f4a2c7b8e1d6f0a3c5b7e2d";
    let gw = start("login");
    // An OAuth provider with a URL to print: the child waits there for the browser to come back
    // (`login_go`), which this test never writes — it cancels before the sign-in can finish.
    std::fs::write(gw.home.join("login_url"), "").unwrap();

    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.login_providers", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;

    // 1. The provider table: six rows with an id, a label and a kind that says what finishing
    //    the sign-in takes, and antigravity among them as a browser sign-in.
    let got = c.call("zephyr.login_providers", json!({})).await;
    assert!(got.get("result").is_some(), "{got}");
    let providers = got["result"]["providers"].as_array().expect("a list");
    assert_eq!(providers.len(), 6, "{got}");
    for p in providers {
        for field in ["id", "label", "kind"] {
            assert!(
                p[field].as_str().is_some_and(|s| !s.is_empty()),
                "{field} missing from {p}"
            );
        }
        assert!(
            matches!(p["kind"].as_str(), Some("oauth" | "key" | "setup_token")),
            "{p}"
        );
    }
    let kinds: Vec<(&str, &str)> = providers
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["kind"].as_str().unwrap()))
        .collect();
    assert!(
        kinds.contains(&("antigravity", "oauth")),
        "no oauth antigravity: {kinds:?}"
    );

    // 2. Two `-32602`s that need no child: an unknown provider, and a token pasted where the
    //    account name goes. Neither reply repeats the token back.
    let unknown = c
        .call("zephyr.login_start", json!({"provider": "nope"}))
        .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let token = c
        .call(
            "zephyr.login_start",
            json!({"provider": "antigravity", "account": PASTED, "model": "antigravity/gemini-3"}),
        )
        .await;
    assert_eq!(token["error"]["code"], -32602, "{token}");
    assert!(
        !token.to_string().contains(PASTED),
        "the pasted token came back in the error: {token}"
    );

    // 3. One browser sign-in: start, poll, cancel — and after the cancel the id is a dead handle.
    let started = c
        .call("zephyr.login_start", json!({"provider": "antigravity"}))
        .await;
    assert!(started.get("result").is_some(), "{started}");
    let login = &started["result"];
    let id = login["loginId"].as_str().expect("a loginId").to_string();
    assert_eq!(login["provider"], "antigravity", "{login}");
    // No pool in this home: the first free name for the provider.
    assert_eq!(login["account"], "antigravity-1", "{login}");
    assert_eq!(login["kind"], "oauth", "{login}");
    // The URL follows a moment after the child starts, so poll until it is there. Every answer
    // carries one of the four phases a client may see (`recording` stays internal).
    let mut polled = Value::Null;
    for _ in 0..100 {
        let reply = c.call("zephyr.login_poll", json!({"loginId": id})).await;
        assert!(reply.get("result").is_some(), "{reply}");
        polled = reply["result"].clone();
        assert!(
            matches!(
                polled["phase"].as_str(),
                Some("waiting" | "need_input" | "done" | "failed")
            ),
            "{polled}"
        );
        if polled["url"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(polled["phase"], "waiting", "{polled}");
    assert_eq!(polled["url"], "https://example.test/login/antigravity");
    assert_eq!(polled["account"], "antigravity-1", "{polled}");

    let cancelled = c.call("zephyr.login_cancel", json!({"loginId": id})).await;
    assert_eq!(cancelled["result"], json!({}), "{cancelled}");
    // The login is gone from the registry: cancelling it again, polling it and typing into it are
    // all unknown ids, so nothing can be answered over a sign-in that already ended.
    let again = c.call("zephyr.login_cancel", json!({"loginId": id})).await;
    assert_eq!(again["error"]["code"], -32602, "{again}");
    assert_eq!(again["error"]["message"], "unknown login", "{again}");
    let after = c.call("zephyr.login_poll", json!({"loginId": id})).await;
    assert_eq!(after["error"]["code"], -32602, "{after}");
    let typed = c
        .call("zephyr.login_input", json!({"loginId": id, "text": PASTED}))
        .await;
    assert_eq!(typed["error"]["code"], -32602, "{typed}");
    assert!(!typed.to_string().contains(PASTED), "{typed}");

    gw.shutdown().await;
}

#[tokio::test]
async fn a_slow_model_listing_does_not_hold_up_the_connection() {
    let gw = start("models-slow");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    std::fs::write(gw.home.join("list_wait"), "").unwrap();
    let req = json!({"jsonrpc": "2.0", "id": 99, "method": "zephyr.models", "params": {}});
    c.write
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let pong = c.call("ping", json!({})).await;
    assert_eq!(pong["result"]["type"], "pong", "{pong}");
    std::fs::remove_file(gw.home.join("list_wait")).unwrap();
    let models = loop {
        let v = c.read().await.expect("connection closed");
        if v["id"] == 99 {
            break v;
        }
    };
    assert_eq!(
        models["result"]["accounts"][0]["models"][0], "fake/m1",
        "{models}"
    );
    gw.shutdown().await;
}

/// The next `zephyr_status` snapshot that `done` accepts.
async fn status_until(c: &mut Client, done: impl Fn(&Value) -> bool) -> Value {
    loop {
        let ev = c.event_of("zephyr_status").await;
        if done(&ev["snapshot"]) {
            return ev["snapshot"].clone();
        }
    }
}

/// rpi sends no event for a prompt, so the router reports each user turn itself: once, even when
/// `/stop` clears rpi's queue and the router sends a queued message again.
#[tokio::test]
async fn each_user_turn_is_shown_once_even_when_sent_again() {
    let gw = start("requeue");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    c.call("zephyr.subscribe", json!({})).await;
    gw.say("m1", "[hang] first").await;
    wait(|| gw.log("prompt").len() == 1).await;
    gw.say("m2", "second").await;
    wait(|| gw.log("queue_message").len() == 1).await;
    gw.say("m3", "/stop").await;
    let mut turns = Vec::new();
    loop {
        let ev = c.event().await;
        if ev["type"] == "message_end" && ev["message"]["role"] == "user" {
            turns.push(ev["message"]["text"].as_str().unwrap().to_string());
        }
        if ev["type"] == "message_end" && text(&ev) == "echo: [chat #2] second" {
            break;
        }
    }
    assert_eq!(
        gw.log("prompt").len(),
        2,
        "second is sent again after the stop"
    );
    assert_eq!(turns, ["[hang] first", "second"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn zephyr_subscribe_replays_raw_history_and_streams_status_and_replies() {
    let gw = start("zsub");
    gw.say("m1", "[tool] earlier").await;
    wait(|| gw.count("outbox", "sent") == 1).await;

    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let sub = c.call("zephyr.subscribe", json!({})).await;
    assert_eq!(sub["result"]["subscriptionId"], 1, "{sub}");
    assert_eq!(sub["result"]["status"], "subscribed", "{sub}");
    // Raw messages: the tool call and its result are kept; user turns carry their origin.
    let messages = sub["result"]["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"]);
    assert_eq!(messages[0]["origin"], json!({"channel": "chat", "id": 1}));
    assert_eq!(messages[0]["text"], "[tool] earlier");
    assert_eq!(messages[0]["content"], "[chat #1] [tool] earlier");
    assert_eq!(messages[1]["content"][0]["type"], "toolCall");
    assert!(messages[1].get("origin").is_none());
    assert_eq!(c.event().await["type"], "ready");
    let first = c.event_of("zephyr_status").await;
    assert_eq!(first["snapshot"]["session"], "main");

    // A live run: its own user turn comes back annotated, and the run state follows the tool.
    // The tool runs until `tool_wait` goes, so its state can't be coalesced away.
    std::fs::write(gw.home.join("tool_wait"), "").unwrap();
    c.send(json!({"type": "prompt", "content": "[tool] now", "id": 1}))
        .await;
    // Either can come first: the status is whatever the router had seen by then.
    let (mut user, mut running) = (Value::Null, Value::Null);
    while user.is_null() || running.is_null() {
        let ev = c.event().await;
        if ev["type"] == "message_end" && ev["message"]["role"] == "user" {
            user = ev;
        } else if ev["type"] == "zephyr_status" && ev["snapshot"]["run"]["current_tool"] == "bash" {
            running = ev["snapshot"].clone();
        }
    }
    assert_eq!(
        user["message"]["origin"],
        json!({"channel": "tui", "id": 2})
    );
    assert_eq!(user["message"]["text"], "[tool] now");
    assert_eq!(running["run"]["busy"], true);
    assert_eq!(
        running["run"]["origin"],
        json!({"channel": "tui", "id": 2, "sender": "token"})
    );
    assert!(running["run"]["since_unix_ms"].as_i64().unwrap() > 0);
    std::fs::remove_file(gw.home.join("tool_wait")).unwrap();
    let done = status_until(&mut c, |s| s["run"]["tools_done"] == 1).await;
    assert_eq!(done["run"]["current_tool"], Value::Null);
    status_until(&mut c, |s| s["run"]["busy"] == false).await;

    // The agent's reply is not repeated; a command's reply is.
    wait(|| gw.count("outbox", "sent") == 2).await;
    c.send(json!({"type": "prompt", "content": "/status", "id": 2}))
        .await;
    let reply = c.event_of("zephyr_reply").await;
    assert_eq!(reply["command"], "/status", "{reply}");
    assert!(
        reply["text"].as_str().unwrap().starts_with("agent: on "),
        "{reply}"
    );
    gw.shutdown().await;
}

#[test]
fn token_file_must_be_private() {
    let dir = std::env::temp_dir().join(format!("zephyr-attach-perm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("token");
    std::fs::write(&file, "t\n").unwrap();
    let attach = |p: &Path| {
        toml::from_str::<Config>(&format!(
            "rpi='r'\nhome='h'\n[attach]\nport=1\ntoken_file={p:?}\n"
        ))
        .unwrap()
        .attach
        .unwrap()
    };
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(zephyr::attach::token(&attach(&file)).is_err());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(zephyr::attach::token(&attach(&file)).unwrap(), "t");
}

/// The pinned stock client, typed into through a pty (its TUI needs a terminal; with piped
/// stdin it only handshakes and exits). Needs python3 and the binary; no model is called.
#[tokio::test]
#[ignore = "drives the real rpi 0.15.0 --connect client"]
async fn stock_rpi_connect_attaches_chats_and_detaches() {
    let rpi = Path::new("/home/user/Projects/.always-on/rpi-0.15.0/bin/rpi");
    if !rpi.exists() {
        eprintln!("skipped: {} is absent", rpi.display());
        return;
    }
    let gw = start("stock");
    gw.say("m1", "earlier").await;
    wait(|| gw.count("outbox", "sent") == 1).await;
    let pid = std::fs::read_to_string(gw.home.join("pid")).unwrap();
    let agent_dir = gw.home.join("rpi-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let status = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/drive-connect.py"
        ))
        .args([
            rpi.to_str().unwrap(),
            &format!("127.0.0.1:{}", gw.port),
            TOKEN,
        ])
        .args(["hello stock", "echo: [tui #2] hello stock"])
        .env("RPI_CODING_AGENT_DIR", &agent_dir)
        .env("HOME", &gw.home)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "the reply never showed in the client");

    let prompts = gw.log("prompt");
    assert_eq!(prompts.last().unwrap()["message"], "[tui #2] hello stock");
    assert!(gw.log("abort").is_empty() && gw.log("stop").is_empty());
    assert_eq!(std::fs::read_to_string(gw.home.join("pid")).unwrap(), pid);
    wait(|| gw.count("inbox", "done") == 2).await;
    gw.shutdown().await;
}
