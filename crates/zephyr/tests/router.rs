//! Router and recovery against a scripted fake `rpi` (tests/fake-rpi.py).

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::store::{client_id, InboxRow, Store};

/// Each delivered reply, and whether its outbox row was already committed as pending.
type Sent = Arc<Mutex<Vec<(Outbound, bool)>>>;

struct TestChannel {
    name: &'static str,
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    db: PathBuf,
    sent: Sent,
}

impl Channel for TestChannel {
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
        let pending = Store::open(&self.db)
            .unwrap()
            .outbox_status(&msg.client_id)
            .unwrap();
        self.sent
            .lock()
            .unwrap()
            .push((msg.clone(), pending.as_deref() == Some("pending")));
        Ok(())
    }
}

struct Gateway {
    feeds: HashMap<&'static str, mpsc::Sender<Inbound>>,
    sent: HashMap<&'static str, Sent>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-it-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(home: &Path) -> Config {
    toml::from_str(&format!(
        "rpi = {:?}\nhome = {:?}\n[owners]\nchat = [\"owner\"]\ndiscord = [\"downer\"]\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home
    ))
    .unwrap()
}

/// A gateway with a `chat` and a `discord` test channel, both feeding one router.
fn start(cfg: Config) -> Gateway {
    let mut feeds = HashMap::new();
    let mut sent = HashMap::new();
    let mut channels = Vec::new();
    for name in ["chat", "discord"] {
        let (feed, rx) = mpsc::channel(16);
        let s = Sent::default();
        feeds.insert(name, feed);
        sent.insert(name, s.clone());
        channels.push(TestChannel {
            name,
            feed: tokio::sync::Mutex::new(rx),
            db: cfg.db_path(),
            sent: s,
        });
    }
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, &channels, async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        feeds,
        sent,
        stop: Some(stop),
        task,
    }
}

impl Gateway {
    async fn say(&self, id: &str, sender: &str, text: &str) {
        self.say_on("chat", id, sender, text).await;
    }
    async fn say_on(&self, channel: &str, id: &str, sender: &str, text: &str) {
        let m = Inbound {
            channel: channel.into(),
            platform_msg_id: id.into(),
            sender: sender.into(),
            text: text.into(),
        };
        self.feeds[channel].send(m).await.unwrap();
    }
    fn replies_on(&self, channel: &str) -> Vec<String> {
        let sent = self.sent[channel].lock().unwrap();
        assert!(
            sent.iter().all(|(_, pending)| *pending),
            "sent before the outbox row was written"
        );
        sent.iter().map(|(m, _)| m.text.clone()).collect()
    }
    fn replies(&self) -> Vec<String> {
        self.replies_on("chat")
    }
    async fn wait_replies_on(&self, channel: &str, n: usize) -> Vec<String> {
        wait(|| self.replies_on(channel).len() >= n).await;
        tokio::time::sleep(Duration::from_millis(300)).await; // nothing extra trickles in
        self.replies_on(channel)
    }
    async fn wait_replies(&self, n: usize) -> Vec<String> {
        self.wait_replies_on("chat", n).await
    }
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }
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

/// Commands the fake child received, across all its incarnations.
fn log(home: &Path, kind: &str) -> Vec<Value> {
    std::fs::read_to_string(home.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["type"] == kind)
        .collect()
}

fn inbox_status(home: &Path, status: &str) -> i64 {
    let counts = Store::open(&home.join("state.db"))
        .unwrap()
        .counts()
        .unwrap();
    counts
        .iter()
        .find(|(t, s, _)| t == "inbox" && s == status)
        .map_or(0, |c| c.2)
}

#[tokio::test]
async fn replies_once_and_dedupes_platform_ids() {
    let home = home("dedupe");
    let gw = start(config(&home));
    gw.say("m1", "owner", "hello").await;
    gw.say("m1", "owner", "hello").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] hello"]);
    assert_eq!(log(&home, "prompt").len(), 1);
    gw.shutdown().await;
    assert_eq!(inbox_status(&home, "done"), 1);
}

#[tokio::test]
async fn unknown_senders_are_dropped() {
    let home = home("unknown");
    let gw = start(config(&home));
    gw.say("m1", "stranger", "let me in").await;
    gw.say("m2", "owner", "hi").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #2] hi"]);
    assert_eq!(log(&home, "prompt").len(), 1);
    gw.shutdown().await;
    assert_eq!(inbox_status(&home, "dropped"), 1);
}

#[tokio::test]
async fn child_killed_mid_run_resumes_without_loss_or_duplicate() {
    let home = home("kill-child");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await; // the child is now mid-run
    let pid = std::fs::read_to_string(home.join("pid")).unwrap();
    std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap();

    assert_eq!(gw.wait_replies(1).await, ["resumed: [chat #1] long job"]);
    assert_eq!(log(&home, "prompt").len(), 1);
    assert_eq!(log(&home, "resume_pending").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn gateway_killed_mid_run_resumes_after_restart() {
    let home = home("kill-gateway");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await;
    gw.task.abort(); // like kill -9: no drain, and kill_on_drop takes the child with it
    let _ = gw.task.await;

    let gw = start(config(&home));
    assert_eq!(gw.wait_replies(1).await, ["resumed: [chat #1] long job"]);
    assert_eq!(log(&home, "prompt").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn restart_with_dispatched_rows_resends_only_what_the_session_lacks() {
    let home = home("restart");
    let cfg = config(&home);
    let store = Store::open(&cfg.db_path()).unwrap();
    let mut seeded = Vec::new();
    for (id, text) in [
        ("a", "never reached rpi"),
        ("b", "answered while we were down"),
    ] {
        let row_id = store
            .insert_inbox(&Inbound {
                channel: "chat".into(),
                platform_msg_id: id.into(),
                sender: "owner".into(),
                text: text.into(),
            })
            .unwrap()
            .unwrap();
        store.mark_dispatched(row_id).unwrap();
        seeded.push(InboxRow {
            id: row_id,
            channel: "chat".into(),
            sender: "owner".into(),
            text: text.into(),
            tries: 1,
        });
    }
    // Row b is already in the session, answered.
    std::fs::create_dir_all(cfg.session_dir()).unwrap();
    let session = serde_json::json!({"pending": false, "messages": [
        {"kind": "user", "role": "user", "content": zephyr::router::tagged(&seeded[1])},
        {"kind": "assistant", "role": "assistant", "content": [{"type": "text", "text": "b's answer"}]},
    ]});
    std::fs::write(cfg.session_dir().join("main.json"), session.to_string()).unwrap();
    drop(store);

    let gw = start(cfg);
    let mut replies = gw.wait_replies(2).await;
    replies.sort();
    assert_eq!(replies, ["b's answer", "echo: [chat #1] never reached rpi"]);
    let prompts = log(&home, "prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], "[chat #1] never reached rpi");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_child_without_the_policy_beacon_gets_no_prompts() {
    let home = home("beacon");
    let mut cfg = config(&home);
    cfg.policy_plugin = Some(home.join("missing-policy.so"));
    let gw = start(cfg.clone());
    gw.say("m1", "owner", "hi").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(gw.replies().is_empty());
    assert!(log(&home, "prompt").is_empty());
    assert_eq!(inbox_status(&home, "new"), 1);
    gw.shutdown().await;

    // Once the plugin loads, the waiting row goes through.
    std::fs::write(home.join("missing-policy.so"), "").unwrap();
    let gw = start(cfg);
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] hi"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn replies_go_back_on_the_channel_they_came_from() {
    let home = home("two-channels");
    let gw = start(config(&home));
    gw.say("m1", "owner", "from chat").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] from chat"]);
    gw.say_on("discord", "d1", "downer", "from discord").await;
    assert_eq!(
        gw.wait_replies_on("discord", 1).await,
        ["echo: [discord #2] from discord"]
    );
    assert_eq!(gw.replies(), ["echo: [chat #1] from chat"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn both_channels_feed_one_main_session() {
    let home = home("one-session");
    let gw = start(config(&home));
    gw.say("m1", "owner", "from chat").await;
    gw.wait_replies(1).await;
    gw.say_on("discord", "d1", "downer", "from discord").await;
    gw.wait_replies_on("discord", 1).await;
    gw.shutdown().await;

    let session: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("sessions/main.json")).unwrap())
            .unwrap();
    let users: Vec<_> = session["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == "user")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(users, ["[chat #1] from chat", "[discord #2] from discord"]);
}

#[tokio::test]
async fn outbox_rows_for_unconfigured_channels_stay_pending() {
    let home = home("unconfigured");
    let cfg = config(&home);
    let mut store = Store::open(&cfg.db_path()).unwrap();
    let id = store
        .insert_inbox(&Inbound {
            channel: "imessage".into(),
            platform_msg_id: "x".into(),
            sender: "owner".into(),
            text: "hi".into(),
        })
        .unwrap()
        .unwrap();
    let row = InboxRow {
        id,
        channel: "imessage".into(),
        sender: "owner".into(),
        text: "hi".into(),
        tries: 0,
    };
    store.reply(&row, "stranded", "done").unwrap();
    drop(store);

    let gw = start(cfg);
    gw.say("m1", "owner", "hi").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #2] hi"]);
    gw.shutdown().await;
    let store = Store::open(&home.join("state.db")).unwrap();
    assert_eq!(
        store.outbox_status(&client_id(id)).unwrap().as_deref(),
        Some("pending")
    );
}
