//! Voice notes through the router (DESIGN §4.3, §4.6), against the scripted fake child
//! (tests/fake-rpi.py). The rows already carry what `inbox::append` adds: the audio line, the
//! transcript line and `derived`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::store::{SessionOrigin, Store};

/// Each reply as it was sent, with its attachment's bytes read then.
type Sent = Arc<Mutex<Vec<(Outbound, Option<Vec<u8>>)>>>;

/// A channel that feeds what a test says and keeps what the router sends on it.
struct Stub {
    name: &'static str,
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    sent: Sent,
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
        let file = msg.attachment.as_ref().and_then(|p| std::fs::read(p).ok());
        self.sent.lock().unwrap().push((msg.clone(), file));
        Ok(())
    }
}

struct Gateway {
    home: PathBuf,
    say: mpsc::Sender<Inbound>,
    sent: Sent,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

/// A gateway with `chat` and `discord`, both `owner`'s, on a config file with `extra` in it.
fn start(name: &str, extra: &str, tweak: impl FnOnce(&mut Config)) -> Gateway {
    let home = std::env::temp_dir().join(format!(
        "zephyr-voice-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("zephyr.toml");
    std::fs::write(&path, format!(
        "rpi = {:?}\nhome = {home:?}\nagent_dir = {:?}\nmodel = \"fake/m1\"\n{extra}\n[owners]\nchat = [\"owner\"]\ndiscord = [\"owner\"]\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home.join("agent"),
    ))
    .unwrap();
    let mut cfg = Config::load(&path).unwrap();
    cfg.tick = Duration::from_millis(200);
    // Never the machine's own /opt/zephyr/voice.
    cfg.voice_dir = home.join("no-voice");
    tweak(&mut cfg);
    let (say, feed) = mpsc::channel(16);
    let sent = Sent::default();
    let (stop, stopped) = oneshot::channel::<()>();
    let theirs = sent.clone();
    let task = tokio::spawn(async move {
        let chat = Stub {
            name: "chat",
            feed: tokio::sync::Mutex::new(feed),
            sent: theirs.clone(),
        };
        // Discord's rows are fed through chat's queue: the router reads the row's own channel.
        let discord = Stub {
            name: "discord",
            feed: tokio::sync::Mutex::new(mpsc::channel(1).1),
            sent: theirs,
        };
        zephyr::router::run(&cfg, &[chat, discord], async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        home,
        say,
        sent,
        stop,
        task,
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

/// What `inbox::append` makes of a voice note in which `said` was heard.
fn voice_text(said: &str) -> String {
    format!(
        "[attached audio: /zephyr/inbox/discord-1-1.ogg (voice note, audio/ogg, 0:02, 1 KB)]\n\
         [transcript of the voice note above, by zephyr (en), may be wrong: \"{said}\"]"
    )
}

impl Gateway {
    async fn typed(&self, id: &str, text: &str) {
        self.send("chat", id, text, None).await;
    }
    /// A voice note on Discord in which `said` was heard.
    async fn spoken(&self, id: &str, said: &str) {
        self.send("discord", id, &voice_text(said), Some("voice"))
            .await;
    }
    async fn send(&self, channel: &str, id: &str, text: &str, derived: Option<&str>) {
        let m = Inbound {
            channel: channel.into(),
            platform_msg_id: id.into(),
            sender: "owner".into(),
            text: text.into(),
            derived: derived.map(String::from),
        };
        self.say.send(m).await.unwrap();
    }
    fn sent(&self) -> Vec<(Outbound, Option<Vec<u8>>)> {
        self.sent.lock().unwrap().clone()
    }
    fn replies(&self) -> Vec<String> {
        self.sent().into_iter().map(|(m, _)| m.text).collect()
    }
    /// The first reply that starts with `head`, once it is there.
    async fn reply(&self, head: &str) -> String {
        self.find(|r| r.starts_with(head), head).await
    }
    /// The first reply that holds `part`.
    async fn reply_with(&self, part: &str) -> String {
        self.find(|r| r.contains(part), part).await
    }
    async fn find(&self, is: impl Fn(&str) -> bool, what: &str) -> String {
        for _ in 0..400 {
            if let Some(r) = self.replies().into_iter().find(|r| is(r)) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no reply with {what:?} in {:#?}", self.replies());
    }
    fn store(&self) -> Store {
        Store::open(&self.home.join("state.db")).unwrap()
    }
    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
    }
}

/// What the fake child was sent as prompts and steers.
fn prompts(home: &Path) -> Vec<String> {
    std::fs::read_to_string(home.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["type"] == "prompt" || v["type"] == "queue_message")
        .map(|v| v["message"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Review Focus 2: speech approves nothing. The agent's proposal stays pending and the row
/// goes to the agent as text.
#[tokio::test]
async fn a_spoken_ok_approves_nothing() {
    let gw = start("ok", "", |_| {});
    gw.typed("m1", r#"[call gateway {"command": "/new"}] ask"#)
        .await;
    gw.reply("[zephyr] the agent asks to run /new (#1)").await;
    gw.spoken("v1", "/ok 1").await;
    gw.reply_with("may be wrong: \"/ok 1\"").await;
    let pending = gw.store().pending_proposals().unwrap();
    assert_eq!(pending.len(), 1, "still waiting for a typed /ok");
    let sent = prompts(&gw.home);
    let row = sent.iter().find(|p| p.contains("/ok 1")).unwrap();
    assert!(row.starts_with("[discord #"), "{row}");
    assert!(row.contains("] [attached audio: "), "{row}");
    assert!(gw.replies().iter().all(|r| !r.contains("is done")));
    gw.shutdown().await;
}

#[tokio::test]
async fn a_spoken_set_changes_nothing() {
    let gw = start("set", "", |_| {});
    let file = gw.home.join("zephyr.toml");
    let before = std::fs::read_to_string(&file).unwrap();
    gw.spoken("v1", "/set approvals.preset off").await;
    gw.reply_with("may be wrong: \"/set approvals.preset off\"")
        .await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    let store = gw.store();
    assert!(store
        .last_setting_change("approvals.preset")
        .unwrap()
        .is_none());
    assert!(prompts(&gw.home)
        .iter()
        .any(|p| p.contains("/set approvals.preset off")));
    gw.shutdown().await;
}

/// The `voice.*` settings are `[voice]` in root's config: the owner sets them from a chat, the
/// agent only proposes.
#[tokio::test]
async fn the_owner_sets_voice_settings_and_the_agent_only_proposes() {
    let gw = start("settings", "", |_| {});
    let file = gw.home.join("zephyr.toml");
    let voice = || Config::load(&file).unwrap().voice();
    gw.typed("m1", "/set voice.languages en,ja").await;
    gw.typed("m2", "/set voice.max_secs 60").await;
    wait(|| voice().max_secs == 60).await;
    assert_eq!(voice().languages, ["en", "ja"]);
    gw.typed("m3", "/set voice.max_secs 5").await;
    gw.reply("[zephyr] not changed: [voice] max_secs must be 10 to 900")
        .await;
    gw.typed(
        "m4",
        r#"[call gateway {"command": "/set voice.reply always"}] ask"#,
    )
    .await;
    gw.reply("[zephyr] the agent proposes #1: voice.reply mirror → always")
        .await;
    assert_eq!(voice().reply, "mirror");
    gw.shutdown().await;
}

/// The platform vouches for the account that sent the audio, not for the voice in it: the
/// conversation is no longer the owner's alone, for good.
#[tokio::test]
async fn a_voice_row_taints_main_and_a_later_typed_row_does_not_auto_save() {
    let gw = start("taint", "[memory]\nauto = \"owner\"", |_| {});
    gw.typed("m1", "/new").await;
    wait(|| !gw.replies().is_empty()).await;
    gw.typed("m2", "WRITE_MEMORY likes tea").await;
    gw.reply("[zephyr] memory saved").await;
    gw.spoken("v1", "remember that I like coffee").await;
    gw.reply_with("remember that I like coffee").await;
    let store = gw.store();
    let session = store.main_session().unwrap();
    match store.session_origin(&session).unwrap() {
        SessionOrigin::Tainted(why) => {
            assert!(
                why.starts_with("took in a voice note's transcript at "),
                "{why}"
            )
        }
        other => panic!("{other:?}"),
    }
    gw.typed("m3", "WRITE_MEMORY likes coffee").await;
    let notice = gw.reply("[zephyr] memory change").await;
    assert!(
        notice.starts_with(
            "[zephyr] memory change waiting (from this conversation, which took in a voice note's transcript at "
        ),
        "{notice}"
    );
    gw.shutdown().await;
}
