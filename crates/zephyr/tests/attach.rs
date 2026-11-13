//! `rpi --connect` attach (src/attach.rs), spoken frame by frame as the stock 0.21.2
//! client does, against the scripted fake child (tests/fake-rpi.py).

mod common;
use common::{free_port, wait};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
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

fn start(name: &str) -> Gateway {
    start_with(name, "")
}

/// `extra` is appended to the config, after `[attach]`.
fn start_with(name: &str, extra: &str) -> Gateway {
    common::reap_logins_at_exit();
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
            derived: None,
            sent_at: None,
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
            // The fake may be halfway through its last line.
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
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

#[tokio::test]
async fn history_replays_then_live_events_stream() {
    let gw = start("history");
    gw.say("m1", "earlier").await;
    wait(10, || gw.count("outbox", "sent") == 1).await;

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
    wait(10, || {
        gw.count("inbox", "done") == 2 && gw.count("outbox", "sent") == 2
    })
    .await;
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
    wait(10, || gw.log("abort").len() == 1).await;
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

/// The TUI's way in (DESIGN §4.3.1): the daemon proves it holds the token over both nonces,
/// then the client does, and the token itself is never sent. A wrong proof, or one with no
/// challenge before it, is refused and the connection closed.
#[tokio::test]
async fn the_daemon_proves_itself_and_takes_a_proof_in_place_of_the_token() {
    use zephyr::attach::{nonce, proof, proven, BY_CLIENT, BY_DAEMON};
    let gw = start("challenge");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.prove", json!({"proof": "00"})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    assert!(c.read().await.is_none(), "a prove with no challenge closes");

    async fn challenge(c: &mut Client) -> ([u8; 32], Vec<u8>) {
        let (mine, hex) = nonce();
        let said = c.call("zephyr.challenge", json!({"nonce": hex})).await;
        let theirs = said["result"]["nonce"].as_str().unwrap().to_string();
        let theirs = data_encoding::HEXLOWER.decode(theirs.as_bytes()).unwrap();
        let given = said["result"]["proof"].as_str().unwrap();
        assert!(proven(TOKEN, BY_DAEMON, &mine, &theirs, given), "{said}");
        // A daemon's proof is never a client's.
        assert!(!proven(TOKEN, BY_CLIENT, &mine, &theirs, given));
        (mine, theirs)
    }
    let mut c = Client::connect(gw.port).await;
    let (mine, theirs) = challenge(&mut c).await;
    let wrong = proof("not-the-token", BY_CLIENT, &mine, &theirs);
    let bad = c.call("zephyr.prove", json!({"proof": wrong})).await;
    assert_eq!(bad["error"]["code"], -32001, "{bad}");
    assert!(c.read().await.is_none(), "a wrong proof closes");

    let mut c = Client::connect(gw.port).await;
    let (mine, theirs) = challenge(&mut c).await;
    let right = proof(TOKEN, BY_CLIENT, &mine, &theirs);
    let ok = c.call("zephyr.prove", json!({"proof": right})).await;
    assert_eq!(ok["result"]["authenticated"], true, "{ok}");
    let hello = c.call("zephyr.hello", json!({})).await;
    assert!(hello.get("result").is_some(), "{hello}");
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
async fn idle_connections_cannot_crowd_out_a_real_client() {
    let gw = start("idle");
    drop(Client::connect(gw.port).await); // the listener is up
    let mut idle = Vec::new();
    for _ in 0..40 {
        idle.push(TcpStream::connect(("127.0.0.1", gw.port)).await.unwrap());
    }
    // Past the cap a connection is closed as it arrives; the rest once 10 s pass unauthenticated.
    let started = std::time::Instant::now();
    let mut c = loop {
        assert!(started.elapsed() < Duration::from_secs(20), "never let in");
        let mut c = Client::connect(gw.port).await;
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "authenticate", "params": {"token": TOKEN}});
        if c.write
            .write_all(format!("{req}\n").as_bytes())
            .await
            .is_ok()
            && c.read()
                .await
                .is_some_and(|v| v["result"]["authenticated"] == true)
        {
            break c;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let started = c.call("start_session", json!({})).await;
    assert_eq!(started["result"]["sessionId"], "main", "{started}");
    for mut s in idle {
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), s.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0) | Err(_))),
            "an idle socket is still open"
        );
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn detaching_never_stops_the_run() {
    let gw = start("detach");
    std::fs::write(gw.home.join("slow"), "").unwrap();
    let mut c = Client::attach(gw.port).await;
    c.send(json!({"type": "prompt", "content": "long job", "id": 1}))
        .await;
    wait(10, || !gw.log("prompt").is_empty()).await;
    let pid = std::fs::read_to_string(gw.home.join("pid")).unwrap();
    let stopped = c.call("stop_session", json!({"sessionId": "main"})).await;
    assert!(stopped.get("result").is_some(), "{stopped}");
    drop(c);

    wait(10, || gw.count("inbox", "done") == 1).await;
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
    wait(10, || gw.count("outbox", "sent") == 1).await; // the child is up and read its state
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
    wait(10, || gw.count("outbox", "sent") == 1).await;
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(
        snap,
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "session": "main",
            "account": "a",
            "model": "fake/m1",
            "thinking": "medium",
            "context_window": 200000,
            "paused": null,
            "channels": [
                {"name": "chat", "state": "connected"},
                {"name": "discord", "state": "off"},
                {"name": "imessage", "state": "off"},
                {"name": "telegram", "state": "off"},
                {"name": "signal", "state": "off"},
                {"name": "web", "state": "off"},
            ],
            "spend": {"tokens": 0, "cost": 0.0, "daily_tokens_limit": null, "daily_cost_limit": null},
            "queue": [],
            "failed_replies": 0,
            "run": {"busy": false, "since_unix_ms": null, "origin": null, "current_tool": null, "tools_done": 0},
            "proposals": 0,
            // No `$ZEPHYR_DESKTOP` here.
            "desktop": null,
            // No auth.json in the account's dir.
            "accounts": [{"name": "a", "model": "fake/m1", "state": "needs login", "until_unix": null}],
            "jobs": [],
        })
    );

    // The desktop's takeover is the web app's, signed with its passkey: not open here.
    let take = c.call("zephyr.desktop", json!({"verb": "give"})).await;
    assert_eq!(take["error"]["code"], -32601, "{take}");

    // Jobs come by their next run.
    gw.say("j1", "/job add later | 2999-01-01T00:00:00Z | hi")
        .await;
    gw.say("j2", "/job add sooner | 0 8 * * * | hi").await;
    wait(10, || gw.count("outbox", "sent") == 3).await;
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
    wait(10, || gw.count("inbox", "new") == 1).await;
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
    wait(10, || gw.log("prompt").len() == 1).await;
    gw.say("m2", "second").await;
    wait(10, || gw.log("queue_message").len() == 1).await;
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
    wait(10, || gw.count("outbox", "sent") == 1).await;

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
    // Snapshots go out on a watch channel, so a slow reader sees only the newest: the run may
    // already be over (busy false, its counts reset) by the time the next one is read.
    let done = status_until(&mut c, |s| {
        s["run"]["tools_done"] == 1 || s["run"]["busy"] == false
    })
    .await;
    assert_eq!(done["run"]["current_tool"], Value::Null);
    if done["run"]["busy"] == true {
        status_until(&mut c, |s| s["run"]["busy"] == false).await;
    }

    // The agent's reply is not repeated; a command's reply is.
    wait(10, || gw.count("outbox", "sent") == 2).await;
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

/// A test that needs the pinned rpi skips without it, unless ZEPHYR_REAL_RPI=1 (deploy/release.sh
/// sets it): then it fails, so a run cannot go green without testing anything.
fn skip_without_rpi(rpi: impl std::fmt::Display) {
    if std::env::var("ZEPHYR_REAL_RPI").as_deref() == Ok("1") {
        panic!("ZEPHYR_REAL_RPI=1 but {rpi} is not installed");
    }
    eprintln!("skipping: {rpi} is not installed");
}

/// The pinned stock client, typed into through a pty (its TUI needs a terminal; with piped
/// stdin it only handshakes and exits). Needs python3 and the binary; no model is called.
#[tokio::test]
#[ignore = "drives the real rpi 0.21.2 --connect client"]
async fn stock_rpi_connect_attaches_chats_and_detaches() {
    let rpi = std::env::var("RPI_BIN")
        .unwrap_or_else(|_| "/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi".into());
    let rpi = Path::new(&rpi);
    if !rpi.exists() {
        skip_without_rpi(rpi.display());
        return;
    }
    let gw = start("stock");
    gw.say("m1", "earlier").await;
    wait(10, || gw.count("outbox", "sent") == 1).await;
    let pid = std::fs::read_to_string(gw.home.join("pid")).unwrap();
    let agent_dir = gw.home.join("rpi-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    // The script gives up within about 40 s; a hang past that fails the test, never the suite.
    let drive = tokio::process::Command::new("python3")
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
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(60), drive)
        .await
        .expect("drive-connect.py ended within 60 s")
        .unwrap();
    assert!(status.success(), "the reply never showed in the client");

    let prompts = gw.log("prompt");
    assert_eq!(prompts.last().unwrap()["message"], "[tui #2] hello stock");
    assert!(gw.log("abort").is_empty() && gw.log("stop").is_empty());
    assert_eq!(std::fs::read_to_string(gw.home.join("pid")).unwrap(), pid);
    wait(10, || gw.count("inbox", "done") == 2).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn zephyr_vault_methods_save_list_and_remove_without_echoing_a_value() {
    const PASSWORD: &str = "pw-attach-s3cret";
    let gw = start("vault");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.vault_list", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;

    let entry = json!({"kind": "login", "name": "github", "site": "https://github.com",
                       "username": "aiden", "password": PASSWORD, "granted": true});
    let put = c.call("zephyr.vault_put", entry).await;
    let listed = c.call("zephyr.vault_list", json!({})).await;
    for frame in [&put, &listed] {
        let entries = frame["result"]["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1, "{frame}");
        assert_eq!(entries[0]["name"], "github");
        assert_eq!(entries[0]["username"], "aiden");
        assert_eq!(entries[0]["granted"], false, "only the owner's /ok grants");
        assert!(!frame.to_string().contains(PASSWORD), "{frame}");
    }
    let saved = std::fs::read_to_string(gw.home.join("vault.toml")).unwrap();
    assert!(saved.contains(PASSWORD));
    // The agent hears of the new entry, by name only.
    wait(10, || {
        gw.log("prompt").iter().any(|p| {
            p["message"].as_str().is_some_and(|m| {
                m.ends_with("the owner saved the login github in the vault; you can use it now")
            })
        })
    })
    .await;
    assert!(!std::fs::read_to_string(gw.home.join("log.jsonl"))
        .unwrap()
        .contains(PASSWORD));
    // The notice is zephyr's text, not the owner's: its row is no owner's (DESIGN §4.6).
    let owner: i64 = rusqlite::Connection::open(gw.home.join("state.db"))
        .unwrap()
        .query_row(
            "SELECT owner FROM inbox WHERE text LIKE '%in the vault; you can use it now'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner, 0);

    // A refusal names the problem, never the value.
    let bad = json!({"kind": "token", "name": "lower", "value": PASSWORD});
    let refused = c.call("zephyr.vault_put", bad).await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(!refused.to_string().contains(PASSWORD), "{refused}");
    let odd = json!({"kind": "token", "name": "A", "value": PASSWORD, "granted": PASSWORD});
    let refused = c.call("zephyr.vault_put", odd).await;
    assert!(!refused.to_string().contains(PASSWORD), "{refused}");

    // A token's hosts are taken from the owner's form (DESIGN §4.21), normalized; `url` and
    // `raw` are `/vault`'s alone, and a wildcard saves nothing.
    let token = json!({"kind": "token", "name": "API_KEY", "value": PASSWORD,
                       "hosts": ["API.example.com"], "url": true, "raw": true});
    let put = c.call("zephyr.vault_put", token).await;
    let entries = put["result"]["entries"].as_array().expect("entries");
    let saved = entries
        .iter()
        .find(|e| e["name"] == "API_KEY")
        .expect("saved");
    assert_eq!(saved["hosts"], json!(["api.example.com:443"]), "{put}");
    assert_eq!(
        (&saved["url"], &saved["raw"]),
        (&json!(false), &json!(false))
    );
    let wild = json!({"kind": "token", "name": "WILD", "value": PASSWORD,
                      "hosts": ["*.example.com"]});
    let refused = c.call("zephyr.vault_put", wild).await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    c.call("zephyr.vault_rm", json!({"name": "API_KEY"})).await;

    let gone = c.call("zephyr.vault_rm", json!({"name": "github"})).await;
    assert_eq!(gone["result"]["entries"], json!([]), "{gone}");
    let again = c.call("zephyr.vault_rm", json!({"name": "github"})).await;
    assert_eq!(again["error"]["code"], -32602, "{again}");
    gw.shutdown().await;
}

#[tokio::test]
async fn facts_put_answers_without_values() {
    const PHONE: &str = "+1 555 010 2345";
    let gw = start("facts");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.facts_list", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;

    let fact =
        json!({"name": "phone", "kind": "digits", "value": PHONE, "allow": ["mum@example.com"]});
    let put = c.call("zephyr.facts_put", fact).await;
    let listed = c.call("zephyr.facts_list", json!({})).await;
    for frame in [&put, &listed] {
        let facts = frame["result"]["facts"].as_array().expect("facts");
        assert_eq!(facts.len(), 1, "{frame}");
        assert_eq!(facts[0]["name"], "phone");
        assert_eq!(facts[0]["kind"], "digits");
        assert_eq!(facts[0]["allow"], json!(["mum@example.com"]));
        assert!(!frame.to_string().contains("555"), "{frame}");
    }
    let saved = std::fs::read_to_string(gw.home.join("facts.toml")).unwrap();
    assert!(saved.contains(PHONE));

    // An empty value on a saved name keeps the value and changes the rest.
    let edit = json!({"name": "phone", "kind": "digits", "value": "", "allow": []});
    let kept = c.call("zephyr.facts_put", edit).await;
    assert_eq!(kept["result"]["facts"][0]["allow"], json!([]), "{kept}");
    assert!(std::fs::read_to_string(gw.home.join("facts.toml"))
        .unwrap()
        .contains(PHONE));

    // A refusal names the problem, never the value; the agent is not told of the fact.
    let short = json!({"name": "pin", "kind": "digits", "value": "98765"});
    let refused = c.call("zephyr.facts_put", short).await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(!refused.to_string().contains("98765"), "{refused}");
    let odd = json!({"name": "x", "kind": PHONE, "value": PHONE});
    let refused = c.call("zephyr.facts_put", odd).await;
    assert!(!refused.to_string().contains("555"), "{refused}");
    // A6: no log at all means the child was told nothing, which is what this says.
    assert!(!std::fs::read_to_string(gw.home.join("log.jsonl"))
        .unwrap_or_default()
        .contains("555"));

    let gone = c.call("zephyr.facts_rm", json!({"name": "phone"})).await;
    assert_eq!(gone["result"]["facts"], json!([]), "{gone}");
    let again = c.call("zephyr.facts_rm", json!({"name": "phone"})).await;
    assert_eq!(again["error"]["code"], -32602, "{again}");
    gw.shutdown().await;
}

/// The prompts the fake child got, in order.
fn prompts(gw: &Gateway) -> Vec<String> {
    let log = gw.log("prompt");
    log.iter()
        .map(|p| p["message"].as_str().unwrap().into())
        .collect()
}

/// Alt+Enter (DESIGN §4.1): a follow-up is not steered into the running turn; it starts a run of
/// its own once that one settles. A plain message beside it still steers.
#[tokio::test]
async fn a_follow_up_waits_for_the_settle_and_then_runs() {
    let gw = start("followup");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    std::fs::write(gw.home.join("tool_wait"), "").unwrap();
    gw.say("m1", "[tool] first").await;
    wait(10, || prompts(&gw).len() == 1).await;
    let later = json!({"type": "prompt", "content": "later", "followUp": true});
    assert_eq!(c.send(later).await["result"]["status"], "sent");
    c.send(json!({"type": "prompt", "content": "now"})).await;
    wait(10, || gw.log("queue_message").len() == 1).await;
    let steer = &gw.log("queue_message")[0];
    let steered = steer["message"].as_str().unwrap();
    // The two travel to the router on separate channels, so either may get the lower inbox id.
    assert!(
        steered.starts_with("[tui #") && steered.contains("] now"),
        "{steer}"
    );
    assert_eq!(gw.count("inbox", "new"), 1, "the follow-up waits");
    std::fs::remove_file(gw.home.join("tool_wait")).unwrap();
    wait(10, || prompts(&gw).len() == 2).await;
    let second = &prompts(&gw)[1];
    assert!(
        second.starts_with("[tui #") && second.ends_with("] later"),
        "{second}"
    );
    assert_eq!(gw.log("queue_message").len(), 1, "never steered");
    gw.shutdown().await;
}

/// A follow-up waiting when `/new` was asked mid-run is the new conversation's first message:
/// the settle restarts the child before it dispatches the follow-up.
#[tokio::test]
async fn a_follow_up_after_new_mid_run_goes_to_the_new_conversation() {
    let gw = start("followup-new");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    std::fs::write(gw.home.join("tool_wait"), "").unwrap();
    gw.say("m1", "[tool] first").await;
    wait(10, || prompts(&gw).len() == 1).await;
    let later = json!({"type": "prompt", "content": "later", "followUp": true});
    assert_eq!(c.send(later).await["result"]["status"], "sent");
    gw.say("m2", "/new").await;
    wait(10, || gw.count("outbox", "sent") == 1).await; // /new answered: after the current reply
    std::fs::remove_file(gw.home.join("tool_wait")).unwrap();
    wait(10, || prompts(&gw).len() == 2).await;
    // The session each prompt went to: that of the main child started last before it.
    let log = std::fs::read_to_string(gw.home.join("log.jsonl")).unwrap();
    let mut session = String::new();
    let mut went = vec![];
    for line in log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        match line["type"].as_str() {
            Some("start") => {
                if let Some(id) = line["sessionId"].as_str().filter(|s| s.starts_with("main")) {
                    session = id.to_string();
                }
            }
            Some("prompt") => went.push(session.clone()),
            _ => {}
        }
    }
    assert_eq!(went[0], "main");
    assert!(went[1].starts_with("main-"), "{went:?}");
    gw.shutdown().await;
}

/// Alt+Up: `zephyr.dequeue` takes back the attached client's messages rpi has not been sent,
/// oldest first; another channel's, and ones already sent, stay.
#[tokio::test]
async fn dequeue_withdraws_only_the_waiting_tui_messages() {
    let gw = start("dequeue");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    c.send(json!({"type": "prompt", "content": "answered"}))
        .await;
    wait(10, || gw.count("outbox", "sent") == 1).await;
    gw.say("m1", "/pause").await;
    wait(10, || gw.count("outbox", "sent") == 2).await;
    c.send(json!({"type": "prompt", "content": "one"})).await;
    let two = json!({"type": "prompt", "content": "two", "followUp": true});
    c.send(two).await;
    gw.say("m2", "from chat").await;
    wait(10, || gw.count("inbox", "new") == 3).await;
    let got = c.call("zephyr.dequeue", json!({})).await;
    assert_eq!(got["result"]["texts"], json!(["one", "two"]), "{got}");
    assert_eq!(gw.count("inbox", "withdrawn"), 2);
    assert_eq!(gw.count("inbox", "new"), 1);
    let again = c.call("zephyr.dequeue", json!({})).await;
    assert_eq!(again["result"]["texts"], json!([]), "{again}");
    gw.say("m3", "/resume").await;
    wait(10, || prompts(&gw).len() == 2).await;
    assert_eq!(prompts(&gw)[1], "[chat #5] from chat");
    gw.shutdown().await;
}

/// `/thinking next` (Shift+Tab) moves to rpi's next level, the snapshot follows, and a new child
/// is set to it again, since rpi keeps it nowhere. The snapshot has the model's context window.
#[tokio::test]
async fn thinking_next_cycles_the_level_and_survives_a_child_restart() {
    let gw = start("thinking");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    gw.say("m1", "hi").await;
    wait(10, || gw.count("outbox", "sent") == 1).await;
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(snap["thinking"], "medium");
    assert_eq!(snap["context_window"], 200000);
    gw.say("m2", "/thinking next").await;
    wait(10, || gw.count("outbox", "sent") == 2).await;
    assert_eq!(gw.log("set_thinking_level")[0]["level"], "high");
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(snap["thinking"], "high");
    gw.say("m3", "/thinking next").await;
    wait(10, || gw.count("outbox", "sent") == 3).await;
    let set = gw.log("set_thinking_level");
    assert_eq!(set[1]["level"], "off", "wraps around");
    gw.say("m4", "/thinking turbo").await;
    wait(10, || gw.count("outbox", "sent") == 4).await;
    let set = gw.log("set_thinking_level");
    assert_eq!(set.len(), 2, "an unknown level is not sent");
    // A rule restarts the child, which starts at rpi's default.
    gw.say("m5", "/rule be brief").await;
    wait(10, || gw.log("set_thinking_level").len() == 3).await;
    assert_eq!(gw.log("set_thinking_level")[2]["level"], "off");
    gw.shutdown().await;
}

#[tokio::test]
async fn zephyr_sessions_and_turns_list_the_conversations_and_what_undo_counts() {
    let gw = start("sessions");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.sessions", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;
    gw.say("m1", "one").await;
    gw.say("m2", "two").await;
    gw.say("m3", "/rename trip").await;
    wait(10, || gw.count("outbox", "sent") == 3).await;

    let sessions = c.call("zephyr.sessions", json!({})).await["result"]["sessions"].clone();
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    assert_eq!(sessions[0]["id"], "main", "{sessions}");
    assert_eq!(sessions[0]["name"], "trip", "{sessions}");
    assert!(sessions[0]["at"].as_i64().unwrap() > 0, "{sessions}");
    // Newest first, untagged: the k-th is what `/undo k` takes back to.
    let turns = c.call("zephyr.turns", json!({})).await["result"]["turns"].clone();
    assert_eq!(
        turns,
        json!([{"channel": "chat", "text": "two"}, {"channel": "chat", "text": "one"}])
    );
    // The header shows the name.
    let snap = c.call("zephyr.hello", json!({})).await["result"].clone();
    assert_eq!(snap["session"], "trip");
    gw.shutdown().await;
}

/// The panel draws its editors from `zephyr.settings` alone: kinds carry their bounds.
#[tokio::test]
async fn settings_lists_sections_with_kinds() {
    let gw = start("settings-list");
    let mut c = Client::connect(gw.port).await;
    let early = c.call("zephyr.settings", json!({})).await;
    assert_eq!(early["error"]["code"], -32001, "{early}");
    c.call("authenticate", json!({"token": TOKEN})).await;
    let got = c.call("zephyr.settings", json!({})).await;
    let sections = got["result"]["sections"].as_array().unwrap();
    let bg = sections
        .iter()
        .find(|s| s["name"] == "Background tasks")
        .unwrap_or_else(|| panic!("{got}"));
    let parallel = bg["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "tasks.parallel")
        .unwrap();
    assert_eq!(
        parallel["kind"],
        json!({"type": "int", "min": 1, "max": 32})
    );
    assert_eq!(parallel["value"], "2");
    assert_eq!(parallel["source"], "default");
    assert_eq!(parallel["access"], "tighten");
    assert_eq!(parallel["strong"], false);
    assert_eq!(parallel["until"], Value::Null);
    assert!(parallel["label"].is_string() && parallel["help"].is_string());
    // The approvals entries (approvals spec §4.3), a new install's manual preset.
    let approvals = sections
        .iter()
        .find(|s| s["name"] == "Approvals")
        .unwrap_or_else(|| panic!("{got}"));
    let row = |key: &str| {
        (approvals["settings"].as_array().unwrap().iter())
            .find(|s| s["key"] == key)
            .unwrap_or_else(|| panic!("{key}: {approvals}"))
            .clone()
    };
    let preset = row("approvals.preset");
    assert_eq!(
        preset["kind"],
        json!({"type": "choice", "options": ["manual", "semi", "full"]})
    );
    assert_eq!(
        (&preset["value"], &preset["source"], &preset["strong"]),
        (&json!("manual"), &json!("default"), &json!(true))
    );
    assert_eq!(preset["modified"], false, "{preset}");
    let money = row("approvals.money");
    assert_eq!(money["kind"]["inner"]["type"], "mode", "{money}");
    assert_eq!(
        (&money["value"], &money["source"]),
        (&json!("ask"), &json!("preset"))
    );
    assert_eq!(row("approvals.currency")["value"], "USD");
    let read_only = sections.iter().find(|s| s["name"] == "Read-only").unwrap();
    assert!(read_only["settings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["access"] == "read-only"));
    gw.shutdown().await;
}

/// Each connector's row names its connector, so two rows Space can flip are told apart.
#[tokio::test]
async fn settings_name_each_connector_row() {
    let gw = start_with(
        "settings-mcp",
        "[[mcp]]\nname = \"mail\"\ncommand = \"m\"\n[[mcp]]\nname = \"notes\"\ncommand = \"n\"\n",
    );
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let got = c.call("zephyr.settings", json!({})).await;
    let sections = got["result"]["sections"].as_array().unwrap();
    let connectors = sections
        .iter()
        .find(|s| s["name"] == "Connectors")
        .unwrap_or_else(|| panic!("{got}"));
    let rows: Vec<(String, String)> = (connectors["settings"].as_array().unwrap().iter())
        .map(|r| {
            let text = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (text("key"), text("label"))
        })
        .collect();
    let row = |key: &str, label: &str| (key.to_string(), label.to_string());
    assert_eq!(
        rows,
        [
            row("mcp.mail.enabled", "Connector mail enabled"),
            row("mcp.notes.enabled", "Connector notes enabled"),
            row("mcp.mail.hosts", "Connector mail hosts"),
            row("mcp.notes.hosts", "Connector notes hosts"),
            row("mcp.mail.secrets", "Connector mail secrets"),
            row("mcp.notes.secrets", "Connector notes secrets"),
        ]
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn goals_lists_open_and_recently_done_goals() {
    let gw = start("goals");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let empty = c.call("zephyr.goals", json!({})).await;
    assert_eq!(empty["result"], json!({"goals": []}), "{empty}");
    gw.say("m1", "/goal add Renew passport").await;
    gw.say("m2", "/goal add Ship zephyr 0.8").await;
    gw.say("m3", "/goal done 1").await;
    let goals = gw.home.join("GOALS.md");
    wait(10, || {
        std::fs::read_to_string(&goals).is_ok_and(|g| g == "Ship zephyr 0.8\n")
    })
    .await;
    let got = c.call("zephyr.goals", json!({})).await;
    assert_eq!(
        got,
        json!({"jsonrpc": "2.0", "id": got["id"], "result": {"goals": [
            {"n": 1, "text": "Ship zephyr 0.8", "done": false},
            {"n": null, "text": "Renew passport", "done": true}]}})
    );
    // Params may be absent.
    let bare = c.call("zephyr.goals", Value::Null).await;
    assert_eq!(bare["result"], got["result"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn settings_set_answers_changed_or_refused() {
    let gw = start("settings-set");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let got = c
        .call(
            "zephyr.settings_set",
            json!({"key": "tasks.parallel", "value": "4"}),
        )
        .await;
    let r = &got["result"];
    assert_eq!(r["outcome"], "changed", "{got}");
    assert_eq!((&r["old"], &r["new"]), (&json!("2"), &json!("4")), "{got}");
    assert_eq!(r["until"], Value::Null);
    let file = std::fs::read_to_string(gw.home.join("zephyr.toml")).unwrap();
    assert!(file.contains("parallel = 4"), "{file}");
    let listed = c.call("zephyr.settings", json!({})).await;
    let row = listed["result"]["sections"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["settings"].as_array().unwrap())
        .find(|s| s["key"] == "tasks.parallel")
        .unwrap()
        .clone();
    assert_eq!(
        (&row["value"], &row["source"]),
        (&json!("4"), &json!("you"))
    );

    // A time limit.
    let timed = c
        .call(
            "zephyr.settings_set",
            json!({"key": "tasks.parallel", "value": "3", "until": "2h"}),
        )
        .await;
    assert_eq!(timed["result"]["outcome"], "changed", "{timed}");
    assert!(timed["result"]["until"].as_i64().unwrap() > 0, "{timed}");

    for (params, why) in [
        (json!({"key": "tasks.parallel", "value": "99"}), "32"),
        (
            json!({"key": "no.such", "value": "1"}),
            "no setting no.such",
        ),
        (
            json!({"key": "tasks.parallel", "value": "1", "until": "45d"}),
            "time limit",
        ),
    ] {
        let got = c.call("zephyr.settings_set", params).await;
        assert_eq!(got["result"]["outcome"], "refused", "{got}");
        assert!(
            got["result"]["reason"].as_str().unwrap().contains(why),
            "{got}"
        );
    }
    let bad = c
        .call("zephyr.settings_set", json!({"key": "tasks.parallel"}))
        .await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    gw.shutdown().await;
}

#[tokio::test]
async fn proposals_and_decide_settle_a_waiting_proposal() {
    let gw = start("proposals");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let none = c.call("zephyr.proposals", json!({})).await;
    assert_eq!(none["result"]["proposals"], json!([]), "{none}");

    let store = Store::open(&gw.home.join("state.db")).unwrap();
    let a = store
        .propose_setting("/set tasks.parallel 4", "tasks.parallel", None, false)
        .unwrap();
    let b = store.propose_command("/pause").unwrap();
    let got = c.call("zephyr.proposals", json!({})).await;
    let list = got["result"]["proposals"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{got}");
    let first = list.iter().find(|p| p["id"] == a).unwrap();
    assert_eq!(first["card"], "/set tasks.parallel 4");
    assert_eq!(first["kind"], "setting");
    assert_eq!(first["strong"], false);
    assert!(first["age_secs"].as_i64().unwrap() >= 0);
    assert!(first["expires_in_secs"].as_i64().unwrap() > 80_000);
    assert_eq!(
        list.iter().find(|p| p["id"] == b).unwrap()["kind"],
        "command"
    );
    let snap = c.call("zephyr.hello", json!({})).await;
    assert_eq!(snap["result"]["proposals"], 2, "{snap}");

    let ok = c
        .call("zephyr.decide", json!({"id": a, "verb": "once"}))
        .await;
    assert_eq!(ok["result"]["ok"], true, "{ok}");
    assert!(
        ok["result"]["text"]
            .as_str()
            .unwrap()
            .contains("tasks.parallel"),
        "{ok}"
    );
    let file = std::fs::read_to_string(gw.home.join("zephyr.toml")).unwrap();
    assert!(file.contains("parallel = 4"), "{file}");
    // Settled: a second approval finds nothing waiting.
    let again = c
        .call("zephyr.decide", json!({"id": a, "verb": "ok"}))
        .await;
    assert_eq!(again["result"]["ok"], false, "{again}");
    assert!(
        again["result"]["text"]
            .as_str()
            .unwrap()
            .contains("no proposal #"),
        "{again}"
    );

    let no = c
        .call("zephyr.decide", json!({"id": b, "verb": "no"}))
        .await;
    assert_eq!(no["result"]["ok"], true, "{no}");
    assert!(
        no["result"]["text"].as_str().unwrap().contains("dropped"),
        "{no}"
    );
    let left = c.call("zephyr.proposals", json!({})).await;
    assert_eq!(left["result"]["proposals"], json!([]), "{left}");
    let bad = c
        .call("zephyr.decide", json!({"id": 1, "verb": "maybe"}))
        .await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    gw.shutdown().await;
}

const WEAK_CHAT: &str = "[settings]\nstrong_channels = [\"tui\"]\n[budget]\nrun_turns = 5\n";

/// `zephyr.decide` keeps `/ok N`'s strong-channel rule: a loosening of a strong setting waits
/// when `chat` says `/ok`, and the TUI, a strong channel, settles it.
#[tokio::test]
async fn decide_applies_a_strong_proposal_that_a_weak_channel_cannot() {
    let gw = start_with("strong-decide", WEAK_CHAT);
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    let n = store
        .propose_setting(
            "/set budget.run_turns 10",
            "budget.run_turns",
            Some("5"),
            true,
        )
        .unwrap();
    gw.say("m1", &format!("/ok {n}")).await;
    wait(10, || gw.count("outbox", "sent") == 1).await;
    let file = std::fs::read_to_string(gw.home.join("zephyr.toml")).unwrap();
    assert!(
        file.contains("run_turns = 5") && !file.contains("run_turns = 10"),
        "{file}"
    );
    let got = c.call("zephyr.proposals", json!({})).await;
    let list = got["result"]["proposals"].as_array().unwrap();
    assert!(
        list.iter().any(|p| p["id"] == n && p["strong"] == true),
        "{got}"
    );

    let ok = c
        .call("zephyr.decide", json!({"id": n, "verb": "ok"}))
        .await;
    assert_eq!(ok["result"]["ok"], true, "{ok}");
    let file = std::fs::read_to_string(gw.home.join("zephyr.toml")).unwrap();
    assert!(file.contains("run_turns = 10"), "{file}");
    let left = c.call("zephyr.proposals", json!({})).await;
    assert_eq!(left["result"]["proposals"], json!([]), "{left}");

    // The panel's own edit of a strong setting: a loosening applies from the TUI.
    let set = json!({"key": "budget.run_turns", "value": "20"});
    let set = c.call("zephyr.settings_set", set).await;
    assert_eq!(set["result"]["outcome"], "changed", "{set}");
    let big = json!({"key": "budget.run_turns", "value": "100001"});
    let big = c.call("zephyr.settings_set", big).await;
    assert_eq!(big["result"]["outcome"], "refused", "{big}");
    assert!(
        big["result"]["reason"].as_str().unwrap().contains("100000"),
        "{big}"
    );
    gw.shutdown().await;
}

/// A connector call held for the owner, as the gate stores one (fixed, fake data).
fn held_pass(store: &Store, class: &str, to: &str, due_at: Option<i64>) -> i64 {
    held_as(store, zephyr::store::ActionKind::Pass, class, to, due_at)
}

fn held_as(
    store: &Store,
    kind: zephyr::store::ActionKind,
    class: &str,
    to: &str,
    due_at: Option<i64>,
) -> i64 {
    use zephyr::store::{NewAction, Origin};
    let args = json!({"server": "mail", "tool": "send", "arguments": {"to": to}});
    store
        .hold_action(&NewAction {
            kind,
            class: class.into(),
            targets: vec![to.into()],
            summary: format!("mail/send to {to}"),
            reason: None,
            payload: args.to_string(),
            secrets: vec![],
            origin: Origin {
                channel: "chat".into(),
                sender: "owner".into(),
                session: "main".into(),
                run: "main:1".into(),
                call_id: None,
            },
            due_at,
        })
        .unwrap()
}

/// `zephyr.proposals` lists a held action with its class and the daemon's whole card, and
/// says whether `once` makes a difference (ask-new) and whether it needs a strong channel;
/// `zephyr.decide` `once` approves it without remembering its recipient, `ok` remembers it.
#[tokio::test]
async fn proposals_carry_a_held_actions_card_and_once_skips_remembering() {
    let gw = start_with(
        "held-once",
        "[approvals]\npreset = \"semi\"\nstrong_only = [\"loosen\", \"money\"]\n",
    );
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    let a = held_pass(&store, "messages", "a@x.org", None);
    let pay = held_pass(&store, "money", "shop.example.com", None);
    let got = c.call("zephyr.proposals", json!({})).await;
    let list = got["result"]["proposals"].as_array().unwrap();
    let first = list.iter().find(|p| p["id"] == a).unwrap();
    assert_eq!(
        (&first["kind"], &first["class"], &first["status"]),
        (&json!("pass"), &json!("messages"), &json!("pending")),
        "{got}"
    );
    assert_eq!(first["summary"], "mail/send to a@x.org", "{got}");
    // The card is the daemon's own, as the chat shows it, not the stored summary.
    let card = first["card"].as_str().unwrap();
    assert!(
        card.starts_with(&format!("[zephyr] approve #{a}: ")),
        "{card}"
    );
    assert!(card.contains("a@x.org (new)"), "{card}");
    assert!(card.contains(&format!("/ok {a} once")), "{card}");
    assert_eq!(
        (&first["once"], &first["strong"]),
        (&json!(true), &json!(false))
    );
    // Money under semi asks, so once is no different; and only a strong channel approves it.
    let money = list.iter().find(|p| p["id"] == pay).unwrap();
    assert_eq!(
        (&money["once"], &money["strong"]),
        (&json!(false), &json!(true))
    );

    let once = c
        .call("zephyr.decide", json!({"id": a, "verb": "once"}))
        .await;
    assert_eq!(once["result"]["ok"], true, "{once}");
    let text = once["result"]["text"].as_str().unwrap();
    assert!(text.contains(&format!("approved #{a}")), "{once}");
    assert!(!text.contains("without asking"), "{once}");
    assert!(!store.all_known("messages", &["a@x.org".into()]).unwrap());
    let b = held_pass(&store, "messages", "a@x.org", None);
    let ok = c
        .call("zephyr.decide", json!({"id": b, "verb": "ok"}))
        .await;
    assert!(
        ok["result"]["text"]
            .as_str()
            .unwrap()
            .ends_with("from now on a@x.org goes without asking"),
        "{ok}"
    );
    assert!(store.all_known("messages", &["a@x.org".into()]).unwrap());
    gw.shutdown().await;
}

/// A held phone call never offers `once` (no number is remembered), as its card's footer
/// says; a held card counts down its 10 minutes, as the footer and the expiry do.
#[tokio::test]
async fn proposals_give_a_call_no_once_and_a_card_its_ten_minutes() {
    use zephyr::store::ActionKind;
    let gw = start_with(
        "held-call-card",
        "[approvals]\npreset = \"semi\"\nstrong_only = [\"loosen\", \"money\"]\n",
    );
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    let message = held_pass(&store, "messages", "a@x.org", None);
    let call = held_as(
        &store,
        ActionKind::Call,
        "messages",
        "tel:+14165550123",
        None,
    );
    let card = held_as(
        &store,
        ActionKind::Card,
        "money",
        "https://shop.example.com",
        None,
    );
    let got = c.call("zephyr.proposals", json!({})).await;
    let list = got["result"]["proposals"].as_array().unwrap();
    let find = |id: i64| list.iter().find(|p| p["id"] == id).unwrap();
    assert_eq!(find(message)["once"], true, "{got}");
    assert_eq!(find(call)["once"], false, "{got}");
    let left = find(card)["expires_in_secs"].as_i64().unwrap();
    assert!((1..=600).contains(&left), "{got}");
    assert!(
        find(call)["expires_in_secs"].as_i64().unwrap() > 80_000,
        "{got}"
    );
    gw.shutdown().await;
}

/// `zephyr.decide` `cancel` is `/cancel N`: a delayed action is cancelled before it goes; one
/// that waits for the owner is not waiting out a delay.
#[tokio::test]
async fn decide_cancel_stops_a_delayed_action() {
    let gw = start("held-cancel");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    let due = zephyr::unix_millis() as i64 / 1000 + 300;
    let later = held_pass(&store, "posts", "x.example.com", Some(due));
    let now = held_pass(&store, "messages", "a@x.org", None);
    let got = c.call("zephyr.proposals", json!({})).await;
    let list = got["result"]["proposals"].as_array().unwrap();
    let delayed = list.iter().find(|p| p["id"] == later).unwrap();
    assert_eq!(
        (&delayed["status"], &delayed["due_at"]),
        (&json!("scheduled"), &json!(due)),
        "{got}"
    );
    assert!(
        delayed["card"]
            .as_str()
            .unwrap()
            .ends_with(&format!(" unless /cancel {later}")),
        "{got}"
    );
    let refused = c
        .call("zephyr.decide", json!({"id": now, "verb": "cancel"}))
        .await;
    assert_eq!(refused["result"]["ok"], false, "{refused}");
    assert!(
        refused["result"]["text"]
            .as_str()
            .unwrap()
            .contains("is not waiting out a delay"),
        "{refused}"
    );
    let cancelled = c
        .call("zephyr.decide", json!({"id": later, "verb": "cancel"}))
        .await;
    assert_eq!(cancelled["result"]["ok"], true, "{cancelled}");
    assert_eq!(
        cancelled["result"]["text"],
        format!("[zephyr] cancelled #{later} (mail/send to x.example.com)")
    );
    assert_eq!(store.action(later).unwrap().unwrap().status, "cancelled");
    let left = c.call("zephyr.proposals", json!({})).await;
    let ids: Vec<&Value> = (left["result"]["proposals"].as_array().unwrap().iter())
        .map(|p| &p["id"])
        .collect();
    assert_eq!(ids, [&json!(now)], "{left}");
    gw.shutdown().await;
}

/// The preset's row says when a hand-set value differs from it (`semi (modified)`).
#[tokio::test]
async fn settings_mark_a_modified_preset() {
    let gw = start_with(
        "settings-modified",
        "[approvals]\npreset = \"semi\"\nmessages = \"ask\"\n",
    );
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let got = c.call("zephyr.settings", json!({})).await;
    let rows: Vec<Value> = (got["result"]["sections"].as_array().unwrap().iter())
        .flat_map(|s| s["settings"].as_array().unwrap().clone())
        .collect();
    let row = |key: &str| rows.iter().find(|r| r["key"] == key).unwrap().clone();
    let preset = row("approvals.preset");
    assert_eq!(
        (&preset["value"], &preset["modified"]),
        (&json!("semi"), &json!(true)),
        "{preset}"
    );
    assert_eq!(row("approvals.messages")["modified"], false);
    gw.shutdown().await;
}

/// A numeric time limit is refused, never read as a permanent change.
#[tokio::test]
async fn settings_set_refuses_a_numeric_time_limit() {
    let gw = start("settings-until");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let before = std::fs::read(gw.home.join("zephyr.toml")).unwrap();
    let params = json!({"key": "tasks.parallel", "value": "3", "until": 7200});
    let got = c.call("zephyr.settings_set", params).await;
    assert_eq!(got["error"]["code"], -32602, "{got}");
    assert!(
        got["error"]["message"].as_str().unwrap().contains("until"),
        "{got}"
    );
    assert_eq!(std::fs::read(gw.home.join("zephyr.toml")).unwrap(), before);
    gw.shutdown().await;
}

/// `zephyr.activity` pages back by cursor: each page starts strictly before the last one's end,
/// so two rows in one second are neither repeated nor skipped.
#[tokio::test]
async fn activity_pages_by_cursor() {
    let gw = start("activity-pages");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    for n in 1..=5 {
        store
            .audit("main", "read", &n.to_string(), "started")
            .unwrap();
    }
    // Rows 2 and 3 share a second.
    rusqlite::Connection::open(gw.home.join("state.db"))
        .unwrap()
        .execute(
            "UPDATE audit SET ts = unixepoch() - 100 * (6 - id) - iif(id = 3, 100, 0)",
            [],
        )
        .unwrap();
    let mut seen = Vec::new();
    let mut before = Value::Null;
    for page in 0..3 {
        let got = c
            .call(
                "zephyr.activity",
                json!({"kind": "tool", "limit": 2, "before": before}),
            )
            .await;
        let result = &got["result"];
        let entries = result["entries"]
            .as_array()
            .unwrap_or_else(|| panic!("{got}"));
        assert_eq!(entries.len(), if page < 2 { 2 } else { 1 }, "{got}");
        assert!(result["since"].is_i64(), "{got}");
        for e in entries {
            assert_eq!(e["kind"], "tool");
            assert_eq!(e["stream"], true);
            let line = e["line"].as_str().unwrap();
            assert!(
                line.ends_with(&format!(": {}", e["text"].as_str().unwrap())),
                "{line}"
            );
            seen.push((
                e["at"].as_i64().unwrap(),
                e["text"].as_str().unwrap().to_string(),
            ));
        }
        before = result["next"].clone();
        assert_eq!(before.is_null(), page == 2, "{got}");
    }
    let texts: Vec<&str> = seen.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(texts, ["read 5", "read 4", "read 3", "read 2", "read 1"]);
    assert!(seen.windows(2).all(|w| w[0].0 >= w[1].0), "{seen:?}");
    gw.shutdown().await;
}

/// With `kind: "grants"`, each entry is a standing grant with the command that takes it back.
#[tokio::test]
async fn activity_grants_carry_their_revoke() {
    let gw = start("activity-grants");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    let store = Store::open(&gw.home.join("state.db")).unwrap();
    store
        .remember_targets("messages", &["bob@x.org".into()])
        .unwrap();
    let got = c.call("zephyr.activity", json!({"kind": "grants"})).await;
    let result = &got["result"];
    assert_eq!(result["since"], Value::Null, "{got}");
    assert_eq!(result["next"], Value::Null, "{got}");
    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{got}");
    assert_eq!(entries[0]["kind"], "grant");
    assert_eq!(entries[0]["subject"], "messages");
    assert_eq!(entries[0]["revoke"], "/approvals forget 1");
    let line = entries[0]["line"].as_str().unwrap();
    assert!(
        line.starts_with("messages bob@x.org, known since ")
            && line.ends_with(" · /approvals forget 1"),
        "{line}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn activity_refuses_bad_params() {
    let gw = start("activity-bad");
    let mut c = Client::connect(gw.port).await;
    c.call("authenticate", json!({"token": TOKEN})).await;
    for params in [
        json!({"range": "91d"}),
        json!({"range": 6}),
        json!({"kind": "inbox"}),
        json!({"before": "1.inbox.1"}),
        json!({"limit": 0}),
        json!({"limit": 201}),
        json!({"limit": "5"}),
    ] {
        let got = c.call("zephyr.activity", params.clone()).await;
        assert_eq!(got["error"]["code"], -32602, "{params}: {got}");
        let message = got["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("[zephyr] usage: /activity"),
            "{message}"
        );
    }
    let got = c
        .call(
            "zephyr.activity",
            json!({"range": "Today", "kind": "Tasks", "limit": null}),
        )
        .await;
    assert_eq!(got["result"]["entries"], json!([]), "{got}");
    gw.shutdown().await;
}
