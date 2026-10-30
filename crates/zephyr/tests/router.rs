//! Router and recovery against a scripted fake `rpi` (tests/fake-rpi.py).

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Activity, Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::store::{client_id, InboxRow, Store};

/// Each delivered reply, and whether its outbox row was already committed as pending.
type Sent = Arc<Mutex<Vec<(Outbound, bool)>>>;
/// Each activity shown, as `<kind> <recipient>`.
type Acts = Arc<Mutex<Vec<String>>>;

struct TestChannel {
    name: &'static str,
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    db: PathBuf,
    sent: Sent,
    acts: Acts,
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
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        let kind = match a {
            Activity::Start => "start",
            Activity::Progress { .. } => "progress",
            Activity::Stop => "stop",
        };
        self.acts.lock().unwrap().push(format!("{kind} {to}"));
        Ok(())
    }
}

struct Gateway {
    feeds: HashMap<&'static str, mpsc::Sender<Inbound>>,
    sent: HashMap<&'static str, Sent>,
    acts: HashMap<&'static str, Acts>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
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
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\ndiscord = [\"downer\"]\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        home.join("agent"),
    ))
    .unwrap()
}

/// A gateway with a `chat` and a `discord` test channel, both feeding one router.
fn start(cfg: Config) -> Gateway {
    let mut feeds = HashMap::new();
    let mut sent = HashMap::new();
    let mut acts = HashMap::new();
    let mut channels = Vec::new();
    for name in ["chat", "discord"] {
        let (feed, rx) = mpsc::channel(16);
        let s = Sent::default();
        feeds.insert(name, feed);
        sent.insert(name, s.clone());
        let a = Acts::default();
        acts.insert(name, a.clone());
        channels.push(TestChannel {
            name,
            feed: tokio::sync::Mutex::new(rx),
            db: cfg.db_path(),
            sent: s,
            acts: a,
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
        acts,
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

const PAUSED: &str = "[zephyr] paused: messages wait in the inbox until /resume";
const BUDGET_NOTICE: &str =
    "[zephyr] daily budget reached: paused until the next UTC day or /resume";

#[tokio::test]
async fn pause_aborts_the_run_and_holds_new_messages_until_resume() {
    let home = home("pause");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await; // mid-run
    gw.say("m2", "owner", "/pause").await;
    gw.wait_replies(2).await; // the pause notice, then the aborted run's fallback reply
    assert_eq!(log(&home, "abort").len(), 1);

    gw.say("m3", "owner", "held").await;
    assert_eq!(gw.wait_replies(2).await.len(), 2);
    assert_eq!(
        log(&home, "prompt").len(),
        1,
        "nothing is dispatched while paused"
    );
    assert_eq!(inbox_status(&home, "new"), 1);

    gw.say("m4", "owner", "/resume").await;
    assert_eq!(
        gw.wait_replies(4).await,
        [
            PAUSED,
            "[zephyr] the run ended without a reply",
            "[zephyr] resumed",
            "echo: [chat #3] held"
        ]
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_pause_survives_restart() {
    let home = home("pause-restart");
    let gw = start(config(&home));
    gw.say("m1", "owner", "/pause").await;
    assert_eq!(gw.wait_replies(1).await, [PAUSED]);
    gw.shutdown().await;

    let gw = start(config(&home));
    gw.say("m2", "owner", "hi").await;
    assert!(gw.wait_replies(0).await.is_empty());
    assert!(log(&home, "prompt").is_empty());
    assert_eq!(inbox_status(&home, "new"), 1);
    gw.say("m3", "owner", "/resume").await;
    assert_eq!(
        gw.wait_replies(2).await,
        ["[zephyr] resumed", "echo: [chat #2] hi"]
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn budget_pauses_at_the_limit_and_holds_until_resume() {
    let home = home("budget");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("daily_tokens = 100").unwrap());
    // Each reply carries 60 tokens: the second takes today to 120.
    std::fs::write(home.join("usage.json"), r#"{"totalTokens": 60}"#).unwrap();
    let gw = start(cfg.clone());
    gw.say("m1", "owner", "small").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] small"]);

    gw.say("m2", "owner", "big").await;
    assert_eq!(
        gw.wait_replies(3).await,
        [
            "echo: [chat #1] small",
            "echo: [chat #2] big",
            BUDGET_NOTICE
        ]
    );

    gw.say("m3", "owner", "held").await;
    assert_eq!(gw.wait_replies(3).await.len(), 3);
    assert_eq!(log(&home, "prompt").len(), 2);
    gw.shutdown().await;

    // The pause is in the store, so a restart keeps it.
    let gw = start(cfg);
    gw.say("m4", "owner", "also held").await;
    assert!(gw.wait_replies(0).await.is_empty());
    assert_eq!(log(&home, "prompt").len(), 2);
    assert_eq!(inbox_status(&home, "new"), 2);

    // `/resume` overrides the limit for the day: the backlog runs, and no second notice follows.
    gw.say("m5", "owner", "/resume").await;
    assert_eq!(
        gw.wait_replies(3).await,
        [
            "[zephyr] resumed",
            "echo: [chat #3] held",
            "echo: [chat #4] also held"
        ]
    );
    assert_eq!(log(&home, "prompt").len(), 4);
    gw.shutdown().await;
}

fn run_status(home: &Path, status: &str) -> i64 {
    let counts = Store::open(&home.join("state.db"))
        .unwrap()
        .counts()
        .unwrap();
    counts
        .iter()
        .find(|(t, s, _)| t == "runs" && s == status)
        .map_or(0, |c| c.2)
}

/// A schedule that will not fire during a test, so only `/job run` starts runs.
const NEVER: &str = "0 0 1 1 *";

#[tokio::test]
async fn job_output_is_delivered_unless_silent() {
    let home = home("job-deliver");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add hi | {NEVER} | say: hello"),
    )
    .await;
    gw.say(
        "m2",
        "owner",
        &format!("/job add quiet | {NEVER} | say:  [SILENT] "),
    )
    .await;
    gw.say("m3", "owner", "/job run 1").await;
    gw.say("m4", "owner", "/job run 2").await;
    wait(|| run_status(&home, "completed") == 2).await;
    let replies = gw.wait_replies(5).await;
    assert_eq!(replies.len(), 5, "{replies:?}");
    assert!(replies[0].starts_with("[zephyr] job #1 (hi) added"));
    assert_eq!(
        replies[2..4],
        [
            "[zephyr] job #1 queued as run 1",
            "[zephyr] job #2 queued as run 2"
        ]
    );
    assert_eq!(replies[4], "hello");
    assert!(gw.replies_on("discord").is_empty());
    let prompts = log(&home, "prompt");
    assert_eq!(prompts.len(), 2, "no /job command reaches the agent");
    assert!(home.join("sessions/job-1-1.json").exists());
    assert!(home.join("sessions/job-2-2.json").exists());
    gw.shutdown().await;
}

#[tokio::test]
async fn a_running_job_does_not_delay_main() {
    let home = home("job-beside-main");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add slow | {NEVER} | [hang] forever"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| !log(&home, "prompt").is_empty()).await; // the job child is mid-run
    let t = std::time::Instant::now();
    gw.say("m3", "owner", "hi").await;
    wait(|| gw.replies().iter().any(|r| r == "echo: [chat #3] hi")).await;
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(run_status(&home, "running"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_run_past_its_time_limit_is_aborted() {
    let home = home("job-timeout");
    let mut cfg = config(&home);
    cfg.job_timeout = Duration::from_millis(500);
    let gw = start(cfg);
    gw.say(
        "m1",
        "owner",
        &format!("/job add slow | {NEVER} | [hang] forever"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    let replies = gw.wait_replies(3).await;
    assert_eq!(
        replies[2],
        "[zephyr] job #1 (slow) run 1 was aborted after 500ms"
    );
    assert_eq!(run_status(&home, "failed"), 1);
    assert_eq!(log(&home, "abort").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_run_cut_off_by_a_restart_is_reported_not_retried() {
    let home = home("job-unknown");
    let cfg = config(&home);
    let store = Store::open(&cfg.db_path()).unwrap();
    store
        .add_job("nightly", NEVER, "say: hi", "chat", "owner")
        .unwrap();
    store.queue_run(1, zephyr::now()).unwrap();
    store.start_next_run(zephyr::now()).unwrap().unwrap(); // `running` when the gateway died
    drop(store);

    let gw = start(cfg);
    assert_eq!(
        gw.wait_replies(1).await,
        ["[zephyr] job #1 (nightly) run 1 was interrupted by a restart; it will not be retried"]
    );
    assert!(log(&home, "prompt").is_empty());
    assert_eq!(run_status(&home, "unknown"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn pause_aborts_a_running_job() {
    let home = home("job-pause");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add slow | {NEVER} | [hang] forever"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| !log(&home, "prompt").is_empty()).await; // the job child is mid-run
    gw.say("m3", "owner", "/pause").await;
    wait(|| run_status(&home, "failed") == 1).await;
    let replies = gw.wait_replies(4).await;
    assert!(replies.iter().any(|r| r == PAUSED), "{replies:?}");
    assert!(replies
        .iter()
        .any(|r| r == "[zephyr] job #1 (slow) run 1 was aborted: paused"));
    assert_eq!(
        log(&home, "abort").len(),
        1,
        "only the job child was aborted"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_paused_scheduler_claims_nothing_and_catches_up_once_on_resume() {
    let home = home("job-resume");
    let cfg = config(&home);
    let mut store = Store::open(&cfg.db_path()).unwrap();
    store
        .add_job("tick", "* * * * *", "say: caught up", "chat", "owner")
        .unwrap();
    // Last ran an hour ago, so sixty minutes of runs are missed.
    store.queue_run(1, zephyr::now() - 3600).unwrap();
    let run = store.start_next_run(zephyr::now()).unwrap().unwrap();
    store
        .finish_run(&run, "completed", "old", None, zephyr::now())
        .unwrap();
    // A budget trip pauses the same way `/pause` does.
    let today = store.today().unwrap();
    store.set_paused(Some(&format!("budget:{today}"))).unwrap();
    drop(store);

    let gw = start(cfg);
    tokio::time::sleep(Duration::from_millis(1500)).await; // first tick and a pause poll
    assert!(log(&home, "prompt").is_empty());
    assert_eq!(run_status(&home, "claimed"), 0);
    gw.say("m1", "owner", "/resume").await;
    wait(|| run_status(&home, "completed") == 2).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(run_status(&home, "completed"), 2, "one catch-up run");
    assert_eq!(gw.wait_replies(2).await, ["[zephyr] resumed", "caught up"]);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_memory_edit_waits_for_the_owner_who_sees_its_diff() {
    let home = home("memory");
    let gw = start(config(&home));
    gw.say("m1", "owner", "WRITE_MEMORY likes tea").await;
    let replies = gw.wait_replies(2).await;
    assert_eq!(replies[0], "echo: [chat #1] WRITE_MEMORY likes tea");
    assert!(
        replies[1].starts_with("[zephyr] memory change waiting; /memory ok keeps it")
            && replies[1].ends_with("+ remembered: [chat #1] WRITE_MEMORY likes tea"),
        "{}",
        replies[1]
    );
    let agents = || std::fs::read_to_string(home.join("AGENTS.md")).unwrap();
    gw.say("m2", "owner", "/new").await;
    gw.say("m3", "owner", "hi").await;
    gw.wait_replies(4).await;
    assert!(!agents().contains("likes tea"), "not before the owner's ok");

    gw.say("m4", "owner", "/memory ok").await;
    assert!(gw.wait_replies(5).await[4].starts_with("[zephyr] memory kept"));
    gw.say("m5", "owner", "/new").await;
    gw.say("m6", "owner", "again").await;
    gw.wait_replies(7).await;
    assert!(agents().contains("likes tea"));

    gw.say("m7", "owner", "/memory undo").await;
    let replies = gw.wait_replies(8).await;
    assert!(
        replies[7].starts_with("[zephyr] undid "),
        "got {}",
        replies[7]
    );
    let memory = std::fs::read_to_string(home.join("memory/MEMORY.md")).unwrap();
    assert!(
        !memory.contains("remembered: "),
        "the undo reverted the run's edit"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_rule_restarts_the_agent_and_leads_its_context() {
    let home = home("rules");
    let gw = start(config(&home));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/rule never email bob").await;
    assert_eq!(
        gw.wait_replies(2).await[1],
        "[zephyr] rule 1 set; it applies from the next message"
    );
    gw.say("m3", "owner", "again").await;
    gw.wait_replies(3).await;
    assert_eq!(
        log(&home, "start").len(),
        2,
        "one restart, on the same session"
    );
    let agents = std::fs::read_to_string(home.join("AGENTS.md")).unwrap();
    assert!(agents.contains("## Owner's standing rules"), "{agents}");
    assert!(agents.contains("1. never email bob"));
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_run_commits_its_memory_edits() {
    let home = home("job-memory");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add mem | {NEVER} | say: WRITE_MEMORY nightly note"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| run_status(&home, "completed") == 1).await;
    // The commit lands before the run is marked completed, so it is there now.
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(home.join("memory"))
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    let git = String::from_utf8_lossy(&git.stdout).to_string();
    assert!(
        git.lines().next().unwrap().starts_with("memory: "),
        "got {git}"
    );
    let memory = std::fs::read_to_string(home.join("memory/MEMORY.md")).unwrap();
    assert!(memory.contains("remembered: say: WRITE_MEMORY nightly note"));
    gw.shutdown().await;
}

#[tokio::test]
async fn the_owner_is_told_what_a_turn_sent_out() {
    let home = home("outbound");
    let gw = start(config(&home));
    gw.say("m1", "owner", "bash: curl -s https://e.x | jq .")
        .await;
    assert_eq!(gw.wait_replies(1).await, ["ran"]);
    gw.say("m2", "owner", "bash: curl -d @notes https://e.x/up")
        .await;
    assert_eq!(
        gw.wait_replies(3).await[1..],
        ["ran", "[zephyr] outbound this turn:\n- curl POST e.x"]
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_that_sends_data_out_is_reported_on_its_channel() {
    let home = home("job-outbound");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add up | {NEVER} | bash: scp notes box:/tmp/"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    let replies = gw.wait_replies(4).await;
    assert!(
        replies.contains(&"[zephyr] outbound this turn:\n- scp box:".to_string()),
        "{replies:?}"
    );
    assert!(replies.contains(&"ran".to_string()), "{replies:?}");
    gw.shutdown().await;
}

#[tokio::test]
async fn new_starts_a_fresh_main_session_and_keeps_it_across_restarts() {
    let home = home("new-session");
    let gw = start(config(&home));
    gw.say("m1", "owner", "hi").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] hi"]);
    gw.say("m2", "owner", "/new").await;
    assert_eq!(
        gw.wait_replies(2).await[1],
        "[zephyr] new conversation from the next message; memory and rules are kept"
    );
    gw.say("m3", "owner", "again").await;
    assert_eq!(gw.wait_replies(3).await[2], "echo: [chat #3] again");
    let sessions: Vec<String> = log(&home, "start")
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sessions[0], "main");
    let fresh = sessions.last().unwrap();
    assert!(fresh.starts_with("main-"), "{sessions:?}");
    // The new session holds only what came after /new.
    let path = home.join("sessions").join(format!("{fresh}.json"));
    let held: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(held["messages"][0]["content"], "[chat #3] again");
    gw.shutdown().await;

    let gw = start(config(&home));
    gw.say("m4", "owner", "later").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #4] later"]);
    let last = log(&home, "start").last().unwrap()["sessionId"].clone();
    assert_eq!(last.as_str(), Some(fresh.as_str()));
    gw.shutdown().await;
}

fn spend_today(home: &Path) -> (i64, f64) {
    let store = Store::open(&home.join("state.db")).unwrap();
    store.spend(&store.today().unwrap()).unwrap()
}

#[tokio::test]
async fn a_job_over_the_budget_pauses_everything() {
    let home = home("job-budget-over");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("daily_tokens = 100").unwrap());
    // The job's reply carries 120 tokens.
    std::fs::write(home.join("usage.json"), r#"{"totalTokens": 120}"#).unwrap();
    let gw = start(cfg);
    gw.say(
        "m1",
        "owner",
        &format!("/job add big | {NEVER} | say: done"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    let replies = gw.wait_replies(4).await;
    assert_eq!(replies[2..], ["done", BUDGET_NOTICE]);
    assert_eq!(spend_today(&home).0, 120);

    gw.say("m3", "owner", "held").await;
    assert_eq!(gw.wait_replies(4).await.len(), 4);
    assert_eq!(
        log(&home, "prompt").len(),
        1,
        "main gets nothing while paused"
    );
    assert_eq!(inbox_status(&home, "new"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_under_the_budget_records_its_spend() {
    let home = home("job-budget-under");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("daily_tokens = 100").unwrap());
    std::fs::write(
        home.join("usage.json"),
        r#"{"totalTokens": 40, "costTotal": 0.5}"#,
    )
    .unwrap();
    let gw = start(cfg);
    gw.say(
        "m1",
        "owner",
        &format!("/job add small | {NEVER} | say: done"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    assert_eq!(gw.wait_replies(3).await[2], "done");
    assert_eq!(spend_today(&home), (40, 0.5));
    gw.say("m3", "owner", "hi").await;
    assert_eq!(gw.wait_replies(4).await[3], "echo: [chat #3] hi");
    gw.shutdown().await;
}

/// Two pooled accounts, `a` then `b`, on providers `pa` and `pb`.
fn pooled(home: &Path) -> Config {
    let mut cfg = config(home);
    cfg.accounts = [("a", "pa/m"), ("b", "pb/m")]
        .map(|(name, model)| zephyr::config::Account {
            name: name.into(),
            model: model.into(),
        })
        .into();
    cfg
}

/// Makes every prompt on `account` end with `error`, as a rate-limited provider does.
fn rate_limit(home: &Path, account: &str, error: &str) {
    let dir = home.join("accounts").join(account);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rate_limited"), error).unwrap();
}

fn cooldowns(home: &Path) -> HashMap<String, zephyr::store::Cooldown> {
    Store::open(&home.join("state.db"))
        .unwrap()
        .cooldowns()
        .unwrap()
}

#[tokio::test]
async fn a_rate_limited_account_fails_over_to_the_next_on_the_same_session() {
    let home = home("failover");
    let reset = zephyr::now() + 3600;
    rate_limit(&home, "a", &format!("rate_limited; resets_at={reset}"));
    let gw = start(pooled(&home));
    gw.say("m1", "owner", "hi").await;
    let replies = gw.wait_replies(2).await;
    assert!(
        replies[0].starts_with("[zephyr] account a is rate-limited until ")
            && replies[0].ends_with("; switched to b"),
        "{replies:?}"
    );
    assert_eq!(replies[1], "echo: [chat #1] hi");

    let starts = log(&home, "start");
    let dirs: Vec<_> = starts.iter().map(|s| s["agentDir"].clone()).collect();
    let accounts = home.join("accounts");
    assert_eq!(
        dirs,
        [
            accounts.join("a").display().to_string(),
            accounts.join("b").display().to_string()
        ]
    );
    let models: Vec<_> = starts.iter().map(|s| s["model"].clone()).collect();
    assert_eq!(models, ["pa/m", "pb/m"]);
    assert!(starts.iter().all(|s| s["sessionId"] == "main"));
    let prompts = log(&home, "prompt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1]["message"], "[chat #1] hi", "the turn is re-run");
    // `resets_at` is honoured.
    assert_eq!(cooldowns(&home)["a"].until, reset);
    gw.shutdown().await;

    // The cooldown is in the store, so a restart goes straight to b.
    let gw = start(pooled(&home));
    gw.say("m2", "owner", "again").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #2] again"]);
    let starts = log(&home, "start");
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2]["model"], "pb/m");
    gw.shutdown().await;
}

#[tokio::test]
async fn without_a_reset_time_the_text_fallback_backs_off() {
    let home = home("failover-regex");
    rate_limit(&home, "a", "HTTP 429: Too Many Requests");
    let gw = start(pooled(&home));
    gw.say("m1", "owner", "hi").await;
    assert_eq!(gw.wait_replies(2).await[1], "echo: [chat #1] hi");
    let c = &cooldowns(&home)["a"];
    let wait = c.until - zephyr::now();
    assert!((14 * 60..=15 * 60).contains(&wait), "{wait}");
    assert_eq!(c.level, 1);
    assert!(c.reason.contains("429"));
    gw.shutdown().await;
}

#[tokio::test]
async fn an_ordinary_error_does_not_fail_over() {
    let home = home("failover-other-error");
    rate_limit(&home, "a", "invalid request: bad tool schema");
    let gw = start(pooled(&home));
    gw.say("m1", "owner", "hi").await;
    assert_eq!(
        gw.wait_replies(1).await,
        ["[zephyr] run failed: invalid request: bad tool schema"]
    );
    assert_eq!(log(&home, "start").len(), 1);
    assert!(cooldowns(&home).is_empty());
    gw.shutdown().await;
}

#[tokio::test]
async fn every_account_cooling_holds_the_inbox_until_the_earliest_reset() {
    let home = home("failover-all");
    let reset = zephyr::now() + 3;
    rate_limit(&home, "a", &format!("rate_limited; resets_at={reset}"));
    rate_limit(
        &home,
        "b",
        &format!("rate_limited; resets_at={}", reset + 60),
    );
    let gw = start(pooled(&home));
    gw.say("m1", "owner", "hi").await;
    let replies = gw.wait_replies(2).await;
    assert!(
        replies[1].starts_with("[zephyr] every account is rate-limited"),
        "{replies:?}"
    );
    assert_eq!(inbox_status(&home, "new"), 1, "the message waits");
    std::fs::remove_file(home.join("accounts/a/rate_limited")).unwrap();
    gw.say("m2", "owner", "later").await;

    // At a's reset the gateway starts on a again and runs what waited.
    let replies = gw.wait_replies(4).await;
    assert_eq!(
        replies[2..],
        ["echo: [chat #1] hi", "echo: [chat #2] later"]
    );
    assert!(zephyr::now() >= reset);
    let starts = log(&home, "start");
    assert_eq!(starts.last().unwrap()["model"], "pa/m");
    assert!(
        !cooldowns(&home).contains_key("a"),
        "a success clears a's backoff"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn account_dirs_share_models_and_settings_and_rpi_retries_once() {
    let home = home("account-dirs");
    let agent = home.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("models.json"), "{}").unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"theme": "dark", "retry": {"maxRetries": 10}}"#,
    )
    .unwrap();
    let gw = start(pooled(&home));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    gw.shutdown().await;
    let dir = home.join("accounts/a");
    for f in ["models.json", "settings.json"] {
        assert_eq!(std::fs::read_link(dir.join(f)).unwrap(), agent.join(f));
    }
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["retry"]["maxRetries"], 1);
    assert_eq!(settings["theme"], "dark");
}

#[tokio::test]
async fn a_rate_limited_job_retries_on_the_next_account() {
    let home = home("job-failover");
    let reset = zephyr::now() + 3600;
    rate_limit(&home, "a", &format!("rate_limited; resets_at={reset}"));
    let gw = start(pooled(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add hi | {NEVER} | say: hello"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| run_status(&home, "completed") == 1).await;
    let replies = gw.wait_replies(3).await;
    assert_eq!(replies[2], "hello", "{replies:?}");
    assert_eq!(cooldowns(&home)["a"].until, reset);
    assert!(
        home.join("sessions/job-1-1-1.json").exists(),
        "a fresh session on b"
    );
    gw.shutdown().await;
}

/// A config loaded from `<home>/zephyr.toml`, so reloads have a file to read: accounts `a`
/// (`fake/m1`) then `b` (`other/m3`), both models in the fake rpi's catalogue.
fn on_file(home: &Path, extra: &str) -> Config {
    let path = home.join("zephyr.toml");
    std::fs::write(&path, file_text(home, extra)).unwrap();
    Config::load(&path).unwrap()
}

fn file_text(home: &Path, extra: &str) -> String {
    format!(
        "rpi = {:?}\nhome = {:?}\n{extra}\n[owners]\nchat = [\"owner\"]\n\
         [[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n[[account]]\nname = \"b\"\nmodel = \"other/m3\"\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
    )
}

fn accounts_in_file(home: &Path) -> Vec<(String, String)> {
    let cfg = Config::load(&home.join("zephyr.toml")).unwrap();
    cfg.accounts
        .into_iter()
        .map(|a| (a.name, a.model))
        .collect()
}

#[tokio::test]
async fn model_switches_after_the_current_reply_without_a_restart() {
    let home = home("model-same");
    std::fs::write(home.join("slow"), "1").unwrap();
    let gw = start(on_file(&home, ""));
    gw.say("m1", "owner", "long").await;
    wait(|| !log(&home, "prompt").is_empty()).await; // mid-run
    gw.say("m2", "owner", "/model fake/m2").await;
    gw.say("m3", "owner", "model?").await;
    assert_eq!(
        gw.wait_replies(3).await,
        [
            "[zephyr] switching to fake/m2 after the current reply",
            "echo: [chat #1] long",
            "model: fake/m2"
        ]
    );
    assert_eq!(log(&home, "start").len(), 1, "no restart");
    assert_eq!(log(&home, "abort").len(), 0, "the reply was not cut off");
    assert_eq!(log(&home, "set_model").len(), 1);
    assert_eq!(accounts_in_file(&home)[0], ("a".into(), "fake/m2".into()));
    gw.shutdown().await;
}

#[tokio::test]
async fn model_on_another_provider_restarts_on_that_account_and_keeps_the_session() {
    let home = home("model-other");
    let gw = start(on_file(&home, ""));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/model other/m3").await;
    gw.say("m3", "owner", "model?").await;
    let replies = gw.wait_replies(3).await;
    assert_eq!(
        replies[1],
        "[zephyr] restarting the agent on b (other/m3) now; the conversation carries on"
    );
    assert_eq!(replies[2], "model: other/m3");
    let starts = log(&home, "start");
    assert_eq!(starts.len(), 2);
    assert!(starts.iter().all(|s| s["sessionId"] == "main"));
    assert_eq!(accounts_in_file(&home)[0].0, "b", "b moved to the front");
    gw.shutdown().await;
}

#[tokio::test]
async fn unknown_models_and_bad_edits_change_nothing() {
    let home = home("model-bad");
    let gw = start(on_file(&home, ""));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/model fake/nope").await;
    gw.say("m3", "owner", "/model third/x").await;
    gw.wait_replies(3).await;
    std::fs::write(home.join("zephyr.toml"), "rpi = [").unwrap();
    gw.say("m4", "owner", "/reload").await;
    gw.say("m5", "owner", "still there?").await;
    let replies = gw.wait_replies(5).await;
    assert_eq!(replies[1], "[zephyr] rpi has no model fake/nope");
    assert_eq!(
        replies[2],
        "[zephyr] model not changed: no account is signed in to third; run `zephyr login third`"
    );
    assert!(
        replies[3].starts_with("[zephyr] zephyr.toml not applied: "),
        "{replies:?}"
    );
    assert_eq!(replies[4], "echo: [chat #5] still there?");
    assert_eq!(log(&home, "start").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn hand_edits_apply_live_on_the_next_tick() {
    let home = home("hand-edit");
    let gw = start(on_file(&home, ""));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    std::fs::write(
        home.join("zephyr.toml"),
        file_text(&home, "[budget]\ndaily_tokens = 5"),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await; // the 5 s tick
    gw.say("m2", "owner", "/status").await;
    let status = gw.wait_replies(2).await.remove(1);
    assert!(status.contains("(limit 5)"), "{status}");
    assert!(
        status.starts_with("agent: account a on fake/m1"),
        "{status}"
    );
    assert_eq!(log(&home, "start").len(), 1, "no restart");
    gw.say("m3", "owner", "/reload").await;
    assert_eq!(gw.wait_replies(3).await[2], "[zephyr] nothing changed");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_channel_change_ends_the_run_for_a_reexec_once_idle() {
    let home = home("reexec");
    let gw = start(on_file(&home, ""));
    std::fs::write(
        home.join("zephyr.toml"),
        file_text(&home, "[discord]\ntoken_env = \"ZEPHYR_T\""),
    )
    .unwrap();
    gw.say("m1", "owner", "/reload").await;
    assert_eq!(
        gw.wait_replies(1).await,
        ["[zephyr] restarting zephyr's channels now"]
    );
    let exit = tokio::time::timeout(Duration::from_secs(10), gw.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(exit, zephyr::router::Exit::Reexec);
}

#[tokio::test]
async fn a_run_shows_activity_on_the_channel_it_serves() {
    let home = home("activity");
    let mut cfg = config(&home);
    // The fake child answers nothing while its tool runs; an auth check's call would stall the router.
    cfg.auth.enabled = false;
    let gw = start(cfg);
    // The tool runs until `tool_wait` goes, so the run can't be coalesced away.
    std::fs::write(home.join("tool_wait"), "").unwrap();
    gw.say_on("discord", "d1", "downer", "[tool] look around")
        .await;
    let acts = |c: &str| gw.acts[c].lock().unwrap().clone();
    wait(|| acts("discord").iter().any(|a| a == "progress downer")).await;
    assert_eq!(acts("discord")[0], "start downer");
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    gw.wait_replies_on("discord", 1).await;
    wait(|| acts("discord").last().map(String::as_str) == Some("stop downer")).await;
    assert!(acts("chat").is_empty(), "only the serving channel shows it");
    gw.shutdown().await;
}
