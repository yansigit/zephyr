//! The Signal adapter (DESIGN §4.3) against tests/fake-signal-cli.py, on the router with fake-rpi
//! and a test `chat` channel.

use serde_json::{json, Value};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Activity, Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::signal::Signal;
use zephyr::store::Store;

const ACCOUNT: &str = "+15550001111";
/// Zephyr's own account's ACI, as signal-cli's accounts.json names it.
const OWN: &str = "00000000-1111-4222-8333-444444444444";
const OWNER: &str = "7f3c0000-aaaa-4bbb-8ccc-dddddddddddd";
const STRANGER: &str = "99999999-eeee-4fff-8000-111111111111";

/// The owner's `chat`: what it is sent.
struct Chat {
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    sent: Arc<Mutex<Vec<String>>>,
}

/// `router::run` takes one channel type. Two values for a test's life: no need to box.
#[allow(clippy::large_enum_variant)]
enum Ch {
    Chat(Chat),
    Signal(Signal),
}

impl Channel for Ch {
    fn name(&self) -> &str {
        match self {
            Ch::Chat(_) => "chat",
            Ch::Signal(s) => s.name(),
        }
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        match self {
            Ch::Chat(c) => {
                let mut feed = c.feed.lock().await;
                while let Some(m) = feed.recv().await {
                    tx.send(m).await?;
                }
                std::future::pending().await
            }
            Ch::Signal(s) => s.run(tx).await,
        }
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        match self {
            Ch::Chat(c) => {
                c.sent.lock().unwrap().push(msg.text.clone());
                Ok(())
            }
            Ch::Signal(s) => s.send(msg).await,
        }
    }
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        match self {
            Ch::Chat(_) => Ok(()),
            Ch::Signal(s) => s.activity(to, a).await,
        }
    }
    fn live(&self, cfg: tokio::sync::watch::Receiver<Arc<Config>>) {
        if let Ch::Signal(s) = self {
            s.live(cfg)
        }
    }
    fn known_as(&self, sender: &str) -> Option<String> {
        match self {
            Ch::Chat(_) => None,
            Ch::Signal(s) => s.known_as(sender),
        }
    }
}

struct Gw {
    home: PathBuf,
    /// signal-cli's data dir.
    d: PathBuf,
    chat: Arc<Mutex<Vec<String>>>,
    _feed: mpsc::Sender<Inbound>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-sig-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(home: &Path, state: Option<&Path>, signal_owners: &[&str]) -> Config {
    let state = state.map_or(String::new(), |s| format!("state = {s:?}\n"));
    let mut cfg: Config = toml::from_str(&format!(
        "rpi = {:?}\nhome = {home:?}\nagent_dir = {:?}\n{state}[signal]\naccount = {ACCOUNT:?}\n\
         [owners]\nchat = [\"owner\"]\nsignal = {signal_owners:?}\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home.join("agent"),
    ))
    .unwrap();
    cfg.signal_cli = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-signal-cli.py").into();
    cfg.tick = Duration::from_millis(200);
    cfg
}

fn start(cfg: Config) -> Gw {
    let (feed, rx) = mpsc::channel(16);
    let chat = Arc::new(Mutex::new(Vec::new()));
    let d = zephyr::signal::data_dir(&cfg);
    let channels = vec![
        Ch::Chat(Chat {
            feed: tokio::sync::Mutex::new(rx),
            sent: chat.clone(),
        }),
        Ch::Signal(Signal::new(&cfg).unwrap()),
    ];
    let home = cfg.home.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, &channels, async {
            let _ = stopped.await;
        })
        .await
    });
    Gw {
        home,
        d,
        chat,
        _feed: feed,
        stop: Some(stop),
        task,
    }
}

impl Gw {
    /// Appends one envelope for signal-cli to deliver.
    fn feed(&self, envelope: Value) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.d.join("feed.jsonl"))
            .unwrap();
        f.write_all(format!("{envelope}\n").as_bytes()).unwrap();
    }
    fn log(&self, kind: &str) -> Vec<Value> {
        std::fs::read_to_string(self.d.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["type"] == kind)
            .collect()
    }
    /// The params of each `method` request signal-cli was sent.
    fn requests(&self, method: &str) -> Vec<Value> {
        self.log("request")
            .into_iter()
            .filter(|v| v["request"]["method"] == method)
            .map(|v| v["request"]["params"].clone())
            .collect()
    }
    fn to(&self, method: &str, aci: &str) -> Vec<Value> {
        self.requests(method)
            .into_iter()
            .filter(|p| p["recipient"] == json!([aci]))
            .collect()
    }
    /// The child is up and its account named (`.ok`, which doctor reads).
    async fn up(&self) {
        wait(|| std::fs::read_to_string(self.d.join(".ok")).is_ok_and(|a| a == ACCOUNT)).await;
    }
    fn prompts(&self) -> Vec<String> {
        std::fs::read_to_string(self.home.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["type"] == "prompt")
            .map(|v| v["message"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    fn inbox(&self, status: &str) -> i64 {
        let counts = Store::open(&self.home.join("state.db"))
            .unwrap()
            .counts()
            .unwrap();
        (counts.iter())
            .find(|(t, s, _)| t == "inbox" && s == status)
            .map_or(0, |c| c.2)
    }
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }
}

async fn wait(mut done: impl FnMut() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

/// A direct message from `from`, as signal-cli 0.14.10 writes it.
fn message(from: &str, ts: u64, text: &str) -> Value {
    json!({
        "source": from, "sourceNumber": "+15559990000", "sourceUuid": from,
        "sourceName": "Aiden", "sourceDevice": 1, "timestamp": ts,
        "serverReceivedTimestamp": ts + 5, "serverDeliveredTimestamp": ts + 9,
        "dataMessage": {"timestamp": ts, "message": text, "expiresInSeconds": 0,
                        "isExpirationUpdate": false, "viewOnce": false}
    })
}

/// `message` with one attachment signal-cli saved as `D/attachments/<id>`.
fn with_file(gw: &Gw, from: &str, ts: u64, text: &str, id: &str, bytes: &[u8]) -> Value {
    let dir = gw.d.join("attachments");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(id), bytes).unwrap();
    let mut m = message(from, ts, text);
    m["dataMessage"]["attachments"] = json!([{
        "contentType": "image/png", "filename": "a.png", "id": id,
        "size": bytes.len(), "isVoiceNote": false, "isGif": false, "isBorderless": false
    }]);
    m
}

#[tokio::test]
async fn a_stranger_gets_nothing() {
    let home = dir("stranger");
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    gw.feed(with_file(
        &gw,
        STRANGER,
        1000,
        "let me in",
        "s1.png",
        b"png!",
    ));
    wait(|| gw.inbox("dropped") == 1).await;
    assert!(
        !gw.d.join("attachments/s1.png").exists(),
        "signal-cli's copy is deleted"
    );
    // The owner's message after it is answered, so the stranger's was taken first.
    gw.feed(message(OWNER, 1001, "hi"));
    wait(|| !gw.to("send", OWNER).is_empty()).await;
    assert!(gw.to("send", STRANGER).is_empty());
    assert!(gw.to("sendTyping", STRANGER).is_empty());
    assert!(!home.join("inbox").exists());
    assert_eq!(gw.prompts().len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn an_owner_is_answered_and_their_file_copied() {
    let home = dir("owner");
    // A run of a second, so its typing shows.
    std::fs::write(home.join("slow"), "1").unwrap();
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    gw.feed(with_file(
        &gw,
        OWNER,
        1700000000123,
        "look",
        "o1.png",
        b"png!",
    ));
    wait(|| !gw.to("send", OWNER).is_empty()).await;
    let saved = home.join(format!("inbox/signal-{OWNER}-1700000000123-1.png"));
    let sent = &gw.to("send", OWNER)[0];
    assert_eq!(
        sent["message"],
        format!(
            "echo: [signal #1] look\n[attached image: {} (a.png, image/png, 1 KB); open it with read]",
            saved.display()
        )
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"png!");
    assert!(!gw.d.join("attachments/o1.png").exists());
    let typing = gw.to("sendTyping", OWNER);
    assert!(typing.first().is_some_and(|t| t.get("stop").is_none()));
    wait(|| gw.to("sendTyping", OWNER).iter().any(|t| t["stop"] == true)).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn pairing_works_by_source_uuid() {
    let home = dir("pair");
    // A file-backed config, so the claim can write `owners.signal`.
    let path = home.join("zephyr.toml");
    std::fs::write(
        &path,
        format!(
            "rpi = {:?}\nhome = {home:?}\nagent_dir = {:?}\n[signal]\naccount = {ACCOUNT:?}\n\
             [owners]\nchat = [\"owner\"]\n",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
            home.join("agent"),
        ),
    )
    .unwrap();
    let mut cfg = Config::load(&path).unwrap();
    cfg.signal_cli = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-signal-cli.py").into();
    cfg.tick = Duration::from_millis(200);
    // The code `/pair signal` would have given on chat (tested in tests/router.rs).
    let (code, row) = zephyr::pairing::issue(zephyr::now());
    let store = Store::open(&cfg.db_path()).unwrap();
    store.set_meta("pair_signal", Some(&row)).unwrap();
    let gw = start(cfg);
    gw.up().await;
    gw.feed(message(STRANGER, 3000, "/pair AAAAA-AAAAA"));
    gw.feed(message(STRANGER, 3001, &format!("/pair {code}")));
    wait(|| !gw.to("send", STRANGER).is_empty()).await;
    let told = format!(
        "[zephyr] Signal user {STRANGER} (\"Aiden\", +15559990000) is now an owner; \
         /unpair signal {STRANGER} removes them"
    );
    wait(|| gw.chat.lock().unwrap().contains(&told)).await;
    let sent = gw.to("send", STRANGER);
    gw.shutdown().await;
    assert_eq!(sent.len(), 1, "the wrong code got no answer: {sent:?}");
    assert_eq!(
        sent[0]["message"],
        "[zephyr] paired: you are an owner of this zephyr"
    );
    assert_eq!(Config::load(&path).unwrap().owners["signal"], [STRANGER]);
    let texts: Vec<String> = (store.inbox_with_status("dropped").unwrap().into_iter())
        .map(|r| r.text)
        .collect();
    assert_eq!(texts, ["/pair", "/pair"], "no code is kept");
}

#[tokio::test]
async fn sync_messages_and_our_own_are_ignored() {
    let home = dir("sync");
    // Even with our own ACI an owner, as when linked to the owner's own account.
    let cfg = config(&home, None, &[OWNER, OWN]);
    // signal-cli's own record of the account it holds.
    let data = zephyr::signal::data_dir(&cfg).join("data");
    std::fs::create_dir_all(&data).unwrap();
    let accounts = json!({"accounts": [{"path": "1", "environment": "LIVE", "number": ACCOUNT, "uuid": OWN}], "version": 2});
    std::fs::write(data.join("accounts.json"), accounts.to_string()).unwrap();
    let gw = start(cfg);
    gw.up().await;
    // Linked to the owner's own account: everything they write elsewhere is a sync message.
    gw.feed(json!({
        "source": ACCOUNT, "sourceNumber": ACCOUNT, "sourceUuid": OWN, "sourceDevice": 1,
        "timestamp": 1, "syncMessage": {"sentMessage": {
            "destination": "+15552220000", "destinationUuid": STRANGER, "timestamp": 1,
            "message": "a private chat"}}
    }));
    let mut own = message(OWN, 2, "from our own account");
    own["sourceNumber"] = json!(ACCOUNT);
    gw.feed(own);
    let mut own_aci = message(OWN, 3, "from our own ACI");
    own_aci["sourceNumber"] = Value::Null;
    gw.feed(own_aci);
    let mut ours = message(OWNER, 4, "from our own number");
    ours["sourceNumber"] = json!(ACCOUNT);
    gw.feed(ours);
    let mut edit = message(OWNER, 5, "");
    edit.as_object_mut().unwrap().remove("dataMessage");
    edit["editMessage"] =
        json!({"targetSentTimestamp": 9, "dataMessage": {"timestamp": 5, "message": "edited"}});
    gw.feed(edit);
    for (key, value) in [
        (
            "reaction",
            json!({"emoji": "👍", "targetAuthorUuid": OWN, "targetSentTimestamp": 9, "isRemove": false}),
        ),
        ("remoteDelete", json!({"timestamp": 9})),
    ] {
        let mut m = message(OWNER, 6, "");
        m["dataMessage"][key] = value;
        gw.feed(m);
    }
    for (key, value) in [
        (
            "typingMessage",
            json!({"action": "STARTED", "timestamp": 7}),
        ),
        (
            "receiptMessage",
            json!({"when": 7, "isDelivery": true, "isRead": false, "timestamps": [9]}),
        ),
        (
            "storyMessage",
            json!({"allowsReplies": true, "textAttachment": {"text": "story"}}),
        ),
    ] {
        let mut m = message(OWNER, 7, "");
        m.as_object_mut().unwrap().remove("dataMessage");
        m[key] = value;
        gw.feed(m);
    }
    gw.feed(message(OWNER, 8, "real"));
    wait(|| !gw.to("send", OWNER).is_empty()).await;
    assert_eq!(gw.prompts(), ["[signal #1] real"]);
    assert_eq!(gw.inbox("dropped"), 0);
    gw.shutdown().await;
}

#[tokio::test]
async fn group_messages_are_ignored() {
    let home = dir("group");
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    let mut v1 = message(OWNER, 1, "in a group");
    v1["dataMessage"]["groupInfo"] =
        json!({"groupId": "abc=", "groupName": "g", "revision": 1, "type": "DELIVER"});
    gw.feed(v1);
    let mut v2 = message(OWNER, 2, "in a v2 group");
    v2["dataMessage"]["groupV2"] = json!({"id": "def="});
    gw.feed(v2);
    gw.feed(message(OWNER, 3, "direct"));
    wait(|| !gw.to("send", OWNER).is_empty()).await;
    assert_eq!(gw.prompts(), ["[signal #1] direct"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn the_child_restarts_after_an_exit() {
    let home = dir("restart");
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    std::fs::write(gw.d.join("exit"), "").unwrap();
    wait(|| !gw.d.join(".ok").exists()).await;
    // Backed off 5 s, then started again.
    wait(|| gw.log("start").len() == 2).await;
    gw.up().await;
    gw.feed(message(OWNER, 1, "still there?"));
    wait(|| !gw.to("send", OWNER).is_empty()).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn a_changed_safety_number_is_told_once() {
    let home = dir("identity");
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    std::fs::write(gw.d.join("untrusted"), "").unwrap();
    gw.feed(message(OWNER, 1, "new phone"));
    // The reply fails and the router keeps trying it.
    wait(|| gw.to("send", OWNER).len() >= 3).await;
    let notice = format!(
        "[zephyr] the Signal safety number of {OWNER} changed; compare it, then run zephyr channel signal trust {OWNER}"
    );
    assert_eq!(*gw.chat.lock().unwrap(), [notice]);
    std::fs::write(gw.d.join(".trust"), OWNER).unwrap();
    wait(|| !gw.requests("trust").is_empty()).await;
    assert!(!gw.d.join(".trust").exists());
    assert_eq!(
        gw.requests("trust"),
        [json!({"recipient": OWNER, "trustAllKnownKeys": true})]
    );
    std::fs::remove_file(gw.d.join("untrusted")).unwrap();
    let store = Store::open(&home.join("state.db")).unwrap();
    wait(|| {
        (store.counts().unwrap().iter()).any(|(t, s, n)| t == "outbox" && s == "sent" && *n >= 2)
    })
    .await;
    assert_eq!(gw.chat.lock().unwrap().len(), 1, "told once");
    gw.shutdown().await;
}

#[tokio::test]
async fn the_data_dir_is_roots_and_private() {
    let home = dir("private-home");
    let state = dir("private-state");
    let gw = start(config(&home, Some(&state), &[OWNER]));
    assert_eq!(gw.d, state.join("signal"));
    gw.up().await;
    let meta = std::fs::metadata(&gw.d).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), std::fs::metadata(&state).unwrap().uid());
    assert!(!home.join("signal").exists());
    gw.shutdown().await;
}

#[tokio::test]
async fn the_child_gets_a_clean_environment() {
    let home = dir("env");
    let gw = start(config(&home, None, &[OWNER]));
    gw.up().await;
    let start = &gw.log("start")[0];
    assert_eq!(
        start["env"],
        json!(["JAVA_HOME", "JAVA_TOOL_OPTIONS", "PATH"])
    );
    let d = gw.d.display().to_string();
    assert_eq!(
        start["argv"],
        json!([
            "--data-dir",
            d,
            "-a",
            ACCOUNT,
            "jsonRpc",
            "--ignore-stories",
            "--ignore-avatars",
            "--ignore-stickers"
        ])
    );
    gw.shutdown().await;
}
