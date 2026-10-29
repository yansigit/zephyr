//! `rpi --connect` attach (src/attach.rs), spoken frame by frame as the stock 0.12.0
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
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(name: &str) -> Gateway {
    let home = std::env::temp_dir().join(format!(
        "zephyr-attach-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let token = home.join("tui.token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\n[attach]\nport = {port}\ntoken_file = {token:?}\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        home.join("agent"),
    ))
    .unwrap();
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
            pair("message_start", "> [chat #1] earlier"),
            pair("message_end", "> [chat #1] earlier"),
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
        "> [chat #3] from discord"
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
#[ignore = "drives the real rpi 0.12.0 --connect client"]
async fn stock_rpi_connect_attaches_chats_and_detaches() {
    let rpi = Path::new("/home/user/Projects/.always-on/rpi-0.12.0/bin/rpi");
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
