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
        // A `stall-<name>` file in the home makes this channel's sends hang, like a stuck client.
        if self
            .db
            .with_file_name(format!("stall-{}", self.name))
            .exists()
        {
            std::future::pending::<()>().await;
        }
        let pending = Store::open(&self.db)
            .unwrap()
            .outbox_status(&msg.client_id)
            .unwrap();
        // An attachment shows as its contents, read now: the copy must exist while it is sent.
        let mut msg = msg.clone();
        if let Some(file) = &msg.attachment {
            let body = std::fs::read_to_string(file).unwrap_or_else(|_| "missing".into());
            msg.text = format!("{} <{body}>", msg.text).trim_start().into();
        }
        self.sent
            .lock()
            .unwrap()
            .push((msg, pending.as_deref() == Some("pending")));
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
    // The resumed run is told to check what was in progress before redoing it.
    let steers = log(&home, "queue_message");
    assert_eq!(steers.len(), 1, "{steers:?}");
    assert_eq!(steers[0]["message"], zephyr::scheduler::RESUME_NOTE);
    assert_eq!(steers[0]["deliverAs"], "steer");
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
async fn a_channel_whose_send_hangs_does_not_stop_the_others() {
    let home = home("stalled-channel");
    std::fs::write(home.join("stall-discord"), "").unwrap();
    let gw = start(config(&home));
    gw.say_on("discord", "d1", "downer", "into the void").await;
    wait(|| inbox_status(&home, "done") == 1).await;
    gw.say("m1", "owner", "still here?").await;
    for _ in 0..300 {
        if !gw.replies().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gw.replies(), ["echo: [chat #2] still here?"]);
    std::fs::remove_file(home.join("stall-discord")).unwrap();
    gw.shutdown().await;
}

#[tokio::test]
async fn a_shutdown_mid_run_still_reports_and_charges_the_run() {
    let home = home("drain-books");
    std::fs::write(home.join("slow"), "1").unwrap();
    std::fs::write(home.join("usage.json"), r#"{"totalTokens": 70}"#).unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "bash: curl -d x https://e.x").await;
    wait(|| inbox_status(&home, "dispatched") == 1).await;
    let sent = gw.sent["chat"].clone();
    gw.shutdown().await;

    let texts: Vec<String> = sent
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.text.clone())
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("[zephyr] outbound this turn:")),
        "{texts:?}"
    );
    let store = Store::open(&home.join("state.db")).unwrap();
    assert_eq!(store.spend(&store.today().unwrap()).unwrap().0, 70);
}

#[tokio::test]
async fn a_run_past_its_turn_cap_is_stopped_and_the_owner_told() {
    let home = home("run-turns");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("run_turns = 3").unwrap());
    let gw = start(cfg);
    gw.say("m1", "owner", "[turns 5] loop").await;
    assert_eq!(
        gw.wait_replies(2).await,
        [
            "[zephyr] the run ended without a reply",
            "[zephyr] stopped the run after 3 turns (budget.run_turns)"
        ]
    );
    assert_eq!(log(&home, "abort").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_run_past_its_time_cap_is_stopped() {
    let home = home("run-minutes");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("run_minutes = 0.01").unwrap()); // 0.6 s
    let gw = start(cfg);
    gw.say("m1", "owner", "[hang] forever").await;
    assert_eq!(
        gw.wait_replies(2).await[1],
        "[zephyr] stopped the run after 0.01 minutes (budget.run_minutes)"
    );
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
async fn a_job_run_past_its_turn_cap_is_stopped() {
    let home = home("job-turns");
    let mut cfg = config(&home);
    cfg.budget = Some(toml::from_str("run_turns = 2").unwrap());
    let gw = start(cfg);
    gw.say(
        "m1",
        "owner",
        &format!("/job add loop | {NEVER} | [turns 5] go"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    assert_eq!(
        gw.wait_replies(3).await[2],
        "[zephyr] job #1 (loop) run 1 failed: stopped after 2 turns (budget.run_turns)"
    );
    assert_eq!(log(&home, "abort").len(), 1);
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

#[tokio::test]
async fn switch_goes_back_to_an_earlier_conversation_and_keeps_it() {
    let home = home("switch");
    let gw = start(config(&home));
    gw.say("m1", "owner", "hi").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/new").await;
    gw.say("m3", "owner", "again").await;
    gw.wait_replies(3).await;
    let fresh = log(&home, "start").last().unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    gw.say("m4", "owner", "/switch").await;
    let list = gw.wait_replies(4).await[3].clone();
    assert!(list.starts_with("[zephyr] conversations"), "{list}");
    assert!(
        list.contains(&format!("* {fresh}  ")) && list.contains("(current)"),
        "{list}"
    );
    assert!(list.contains("\n  main  "), "{list}");

    gw.say("m5", "owner", "/switch nope").await;
    assert_eq!(
        gw.wait_replies(5).await[4],
        "[zephyr] no conversation nope; /switch lists them"
    );
    gw.say("m6", "owner", "/switch main").await;
    assert_eq!(
        gw.wait_replies(6).await[5],
        "[zephyr] back to main from the next message"
    );
    gw.say("m7", "owner", "back").await;
    assert_eq!(gw.wait_replies(7).await[6], "echo: [chat #7] back");
    assert_eq!(log(&home, "start").last().unwrap()["sessionId"], "main");
    let held: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("sessions/main.json")).unwrap())
            .unwrap();
    let texts: Vec<&str> = held["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(texts, ["[chat #1] hi", "[chat #7] back"]);
    gw.say("m8", "owner", "/switch main").await;
    assert_eq!(gw.wait_replies(8).await[7], "[zephyr] already on main");
    gw.shutdown().await;

    let gw = start(config(&home));
    gw.say("m9", "owner", "later").await;
    gw.wait_replies(1).await;
    assert_eq!(log(&home, "start").last().unwrap()["sessionId"], "main");
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

#[tokio::test]
async fn send_file_goes_to_the_owner_after_the_reply_and_its_copy_is_dropped() {
    let home = home("send-file");
    std::fs::write(home.join("shot.png"), "PNG").unwrap();
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        r#"look [call send_file {"path": "shot.png", "caption": "the page"}]"#,
    )
    .await;
    let replies = gw.wait_replies(2).await;
    assert!(
        replies[0].starts_with("echo: [chat #1] look"),
        "{replies:?}"
    );
    assert_eq!(replies[1], "the page <PNG>");
    let copy = gw.sent["chat"].lock().unwrap()[1]
        .0
        .attachment
        .clone()
        .unwrap();
    assert!(copy.ends_with("shot.png") && copy.starts_with(home.join("files")));
    wait(|| !copy.exists()).await;
    std::fs::write(home.join("shot.png"), "changed").unwrap();

    // A missing file or a credential is not sent; the owner is told why.
    gw.say(
        "m2",
        "owner",
        r#"[call send_file {"path": "gone.png"}] [call send_file {"path": "accounts/a/auth.json"}]"#,
    )
    .await;
    let replies = gw.wait_replies(5).await;
    assert!(
        replies[3].starts_with("[zephyr] the agent's send_file failed: opening gone.png"),
        "{replies:?}"
    );
    assert!(replies[4].contains("is a credential file"), "{replies:?}");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_the_agent_proposes_waits_for_the_owners_ok() {
    let home = home("propose-job");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        r#"[call schedule {"name": "stretch", "schedule": "0 9 * * *", "prompt": "say: stretch"}] remind me"#,
    )
    .await;
    let replies = gw.wait_replies(2).await;
    assert!(
        replies[1]
            .starts_with("[zephyr] the agent proposed job #1 (stretch, 0 9 * * *, first run ")
            && replies[1].ends_with("/job ok 1 turns it on; /job rm 1 drops it"),
        "{replies:?}"
    );
    let jobs = || Store::open(&home.join("state.db")).unwrap().jobs().unwrap();
    assert!(!jobs()[0].enabled, "off until the owner says so");
    gw.say("m2", "owner", "/job list").await;
    assert!(gw.wait_replies(3).await[2].ends_with("| waiting for /job ok"));
    gw.say("m3", "owner", "/job ok 1").await;
    assert_eq!(gw.wait_replies(4).await[3], "[zephyr] job #1 is on");
    assert!(jobs()[0].enabled);

    gw.say(
        "m4",
        "owner",
        r#"[call schedule {"name": "x", "schedule": "every day", "prompt": "p"}]"#,
    )
    .await;
    let replies = gw.wait_replies(6).await;
    assert!(
        replies[5].starts_with("[zephyr] the agent's schedule failed: bad schedule \"every day\""),
        "{replies:?}"
    );
    assert_eq!(jobs().len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn the_agent_reads_status_through_gateway_within_its_turn() {
    let home = home("gateway");
    let gw = start(config(&home));
    // The very first run: the router's start-up calls are answered while the call waits.
    gw.say(
        "m1",
        "owner",
        r#"[call gateway {"command": "/status"}] how are you?"#,
    )
    .await;
    let reply = gw.wait_replies(1).await[0].clone();
    // The daemon's answer came back as the tool result, so the same turn could use it.
    assert!(reply.contains("\ngateway: agent: on "), "{reply}");
    assert_eq!(
        std::fs::read_dir(home.join(".gateway")).unwrap().count(),
        0,
        "the answer was consumed"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn gateway_never_approves_for_the_owner() {
    let home = home("gateway-approve");
    let gw = start(config(&home));
    gw.say("m1", "owner", "WRITE_MEMORY likes tea").await;
    gw.wait_replies(2).await; // the reply and the "memory change waiting" notice
    gw.say(
        "m2",
        "owner",
        r#"[call gateway {"command": "/memory ok"}] [call gateway {"command": "/memory diff"}]"#,
    )
    .await;
    let reply = gw.wait_replies(3).await[2].clone();
    assert!(
        reply.contains("\ngateway: tool error: /memory ok is not available to the agent"),
        "{reply}"
    );
    assert!(
        reply.contains("remembered: [chat #1] WRITE_MEMORY likes tea"),
        "{reply}"
    );
    gw.say("m3", "owner", "/memory diff").await;
    let diff = gw.wait_replies(4).await[3].clone();
    assert!(
        diff.contains("likes tea"),
        "still waiting for the owner: {diff}"
    );
    // Only the listed call got an answer, and it was consumed: none is left for the refused one.
    assert_eq!(std::fs::read_dir(home.join(".gateway")).unwrap().count(), 0);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_gets_no_gateway() {
    let home = home("gateway-job");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!(r#"/job add gw | {NEVER} | [call gateway {{"command": "/status"}}] check"#),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| run_status(&home, "completed") == 1).await;
    let replies = gw.wait_replies(3).await;
    assert!(
        replies[2].ends_with("\ngateway: not available in scheduled runs"),
        "{replies:?}"
    );
    gw.shutdown().await;
}

const NO_PROPOSAL: &str = "[zephyr] no proposal #1 is waiting";

#[tokio::test]
async fn the_agent_proposes_a_model_and_only_the_owners_ok_switches_it() {
    let home = home("gateway-propose");
    let gw = start(on_file(&home, ""));
    gw.say(
        "m1",
        "owner",
        r#"[call gateway {"command": "/model fake/m2"}] try m2"#,
    )
    .await;
    let replies = gw.wait_replies(2).await;
    assert_eq!(
        replies[0],
        "[zephyr] the agent asks to run /model fake/m2 (#1); /ok 1 runs it, /no 1 drops it"
    );
    assert!(
        replies[1]
            .ends_with("\ngateway: proposed as #1; it runs only if the owner approves with /ok 1"),
        "{replies:?}"
    );
    assert!(log(&home, "set_model").is_empty(), "nothing ran yet");
    gw.say("m2", "owner", "/ok 1").await;
    gw.say("m3", "owner", "model?").await;
    let replies = gw.wait_replies(4).await;
    // The owner's own `/model fake/m2` reply, from the same code path.
    assert_eq!(replies[2..], ["[zephyr] now on fake/m2", "model: fake/m2"]);
    assert_eq!(log(&home, "set_model").len(), 1);
    gw.say("m4", "owner", "/ok 1").await;
    let again = gw.wait_replies(5).await[4].clone();
    assert!(again.starts_with(NO_PROPOSAL), "{again}");
    assert_eq!(log(&home, "set_model").len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn no_drops_a_proposal() {
    let home = home("gateway-no");
    let gw = start(config(&home));
    gw.say("m1", "owner", r#"[call gateway {"command": "/pause"}] hi"#)
        .await;
    gw.wait_replies(2).await;
    gw.say("m2", "owner", "/no 1").await;
    gw.say("m3", "owner", "/ok 1").await;
    let replies = gw.wait_replies(4).await;
    assert_eq!(replies[2], "[zephyr] dropped #1 (/pause)");
    assert!(replies[3].starts_with(NO_PROPOSAL), "{replies:?}");
    gw.say("m4", "owner", "still here?").await;
    assert_eq!(gw.wait_replies(5).await[4], "echo: [chat #4] still here?");
    gw.shutdown().await;
}

#[tokio::test]
async fn approvals_are_never_proposed() {
    let home = home("gateway-no-approve");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        r#"[call gateway {"command": "/login"}] [call gateway {"command": "/ok 1"}]"#,
    )
    .await;
    let replies = gw.wait_replies(1).await;
    assert_eq!(replies.len(), 1, "no notice: {replies:?}");
    for refused in ["/login", "/ok 1"] {
        let line = format!("\ngateway: tool error: {refused} is not available to the agent");
        assert!(replies[0].contains(&line), "{replies:?}");
    }
    gw.say("m2", "owner", "/ok 1").await;
    let none = gw.wait_replies(2).await[1].clone();
    assert!(none.starts_with(NO_PROPOSAL), "nothing stored: {none}");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_cannot_propose() {
    let home = home("gateway-job-propose");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!(r#"/job add gw | {NEVER} | [call gateway {{"command": "/new"}}] check"#),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    wait(|| run_status(&home, "completed") == 1).await;
    let replies = gw.wait_replies(3).await;
    assert!(
        replies[2].ends_with("\ngateway: not available in scheduled runs"),
        "{replies:?}"
    );
    gw.say("m3", "owner", "/ok 1").await;
    let none = gw.wait_replies(4).await[3].clone();
    assert!(none.starts_with(NO_PROPOSAL), "nothing stored: {none}");
    gw.shutdown().await;
}

fn main_session(home: &Path) -> String {
    std::fs::read_to_string(home.join("sessions").join("main.json")).unwrap_or_default()
}

#[tokio::test]
async fn btw_answers_from_a_snapshot_and_leaves_main_as_is() {
    let home = home("btw");
    let gw = start(config(&home));
    gw.say("m1", "owner", "hi").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #1] hi"]);
    let before = main_session(&home);
    std::fs::write(home.join("usage.json"), r#"{"totalTokens": 40}"#).unwrap();

    gw.say("m2", "owner", "/btw what did I say?").await;
    let replies = gw.wait_replies(2).await;
    let store = Store::open(&home.join("state.db")).unwrap();
    assert_eq!(
        store.spend(&store.today().unwrap()).unwrap().0,
        40,
        "booked"
    );
    std::fs::remove_file(home.join("usage.json")).unwrap();
    let answer = &replies[1];
    assert!(
        answer.starts_with("[btw] echo: You are answering a side question"),
        "{answer}"
    );
    assert!(
        answer.contains("user: hi\n\nassistant: echo: [chat #1] hi"),
        "{answer}"
    );
    assert!(answer.ends_with("Question: what did I say?"), "{answer}");
    assert_eq!(main_session(&home), before, "main's session is left as is");
    // A throwaway child: no session, no tools; main is not restarted.
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(home.join("argv.json")).unwrap()).unwrap();
    for flag in [
        "--no-session",
        "--no-tools",
        "--approve",
        "--no-skills",
        "--local-only",
    ] {
        assert!(argv.contains(&flag.to_string()), "{flag} in {argv:?}");
    }
    assert!(!argv.contains(&"--session-id".to_string()), "{argv:?}");
    let starts = log(&home, "start");
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[1]["noSession"], true);
    assert_eq!(starts[1]["noTools"], true);

    gw.say("m3", "owner", "/btw").await;
    assert!(gw.wait_replies(3).await[2].starts_with("[zephyr] usage: /btw <question>"));

    // One at a time.
    std::fs::write(home.join("slow"), "1").unwrap();
    gw.say("m4", "owner", "/btw one").await;
    gw.say("m5", "owner", "/btw two").await;
    let replies = gw.wait_replies(5).await;
    assert_eq!(replies[3], "[zephyr] a /btw is already running");
    assert!(replies[4].starts_with("[btw] "), "{replies:?}");
    assert!(replies[4].ends_with("Question: one"), "{replies:?}");
    std::fs::remove_file(home.join("slow")).unwrap();

    // The conversation goes on without the side questions in it.
    gw.say("m6", "owner", "again").await;
    assert_eq!(gw.wait_replies(6).await[5], "echo: [chat #6] again");
    assert!(!main_session(&home).contains("/btw"));
    assert!(!main_session(&home).contains("side question"));
    gw.shutdown().await;
    assert_eq!(inbox_status(&home, "dispatched"), 0);
}

#[tokio::test]
async fn btw_answers_while_main_is_busy_without_disturbing_the_run() {
    let home = home("btw-busy");
    let gw = start(config(&home));
    std::fs::write(home.join("tool_wait"), "").unwrap();
    gw.say("m1", "owner", "[tool] look around").await;
    wait(|| main_session(&home).contains("toolCall")).await; // main is mid-run, in its tool
    let before = main_session(&home);

    gw.say("m2", "owner", "/btw what are you doing?").await;
    let replies = gw.wait_replies(1).await;
    assert!(replies[0].starts_with("[btw] "), "{replies:?}");
    assert!(
        replies[0].contains("user: [tool] look around\n\n[tool: bash]"),
        "{replies:?}"
    );
    assert_eq!(main_session(&home), before, "the run is left as is");

    std::fs::remove_file(home.join("tool_wait")).unwrap();
    let replies = gw.wait_replies(2).await;
    assert_eq!(replies[1], "echo: [chat #1] [tool] look around");
    assert!(log(&home, "abort").is_empty());
    assert!(log(&home, "queue_message").is_empty());
    gw.shutdown().await;
}

fn task_status(home: &Path, status: &str) -> i64 {
    let counts = Store::open(&home.join("state.db"))
        .unwrap()
        .counts()
        .unwrap();
    counts
        .iter()
        .find(|(t, s, _)| t == "tasks" && s == status)
        .map_or(0, |c| c.2)
}

/// The newest prompt any fake child got, once there are `n`.
async fn last_prompt(home: &Path, n: usize) -> String {
    wait(|| log(home, "prompt").len() >= n).await;
    log(home, "prompt").last().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The newest prompt any fake child got that holds `needle`.
fn prompt_with(home: &Path, needle: &str) -> Option<String> {
    log(home, "prompt")
        .iter()
        .rev()
        .filter_map(|p| p["message"].as_str())
        .find(|m| m.contains(needle))
        .map(str::to_string)
}

/// Task reports written to the inbox for main.
fn inbox_reports(home: &Path) -> usize {
    let store = Store::open(&home.join("state.db")).unwrap();
    ["new", "dispatched", "done"]
        .iter()
        .flat_map(|s| store.inbox_with_status(s).unwrap())
        .filter(|r| r.text.starts_with("[task #"))
        .count()
}

/// A task goal that hangs the task's child: `\u005b` is `[`, so the main prompt carrying the
/// call never contains `[hang]` itself and answers at once.
const HANGING_GOAL: &str = r"wait \u005bhang]";

#[tokio::test]
async fn a_task_runs_beside_main_and_the_agent_stops_it() {
    let home = home("task-stop");
    let gw = start(config(&home));
    let start = format!(r#"[call task {{"action": "start", "goal": "{HANGING_GOAL}"}}] on it"#);
    gw.say("m1", "owner", &start).await;
    let first = gw.wait_replies(1).await[0].clone();
    assert!(first.contains("\ntask: task #1 started"), "{first}");
    wait(|| task_status(&home, "running") == 1).await;
    // The chat is not blocked by the task.
    gw.say("m2", "owner", "hi").await;
    assert_eq!(gw.wait_replies(2).await[1], "echo: [chat #2] hi");
    gw.say("m3", "owner", r#"[call task {"action": "list"}]"#)
        .await;
    let list = gw.wait_replies(3).await[2].clone();
    assert!(
        list.contains("#1 running (0 min ago): wait [hang]"),
        "{list}"
    );
    // "cancel that": the agent stops it with the same tool.
    gw.say("m4", "owner", r#"[call task {"action": "stop", "id": 1}]"#)
        .await;
    let stop = gw.wait_replies(4).await[3].clone();
    assert!(stop.contains("\ntask: [zephyr] stopping task #1"), "{stop}");
    wait(|| task_status(&home, "stopped") == 1).await;
    // The report comes back to main as a message on the task's channel. (Main's answer is not
    // checked: the report quotes the goal, and its `[hang]` hangs the fake.)
    wait(|| prompt_with(&home, "[task #1 stopped;").is_some()).await;
    let report = prompt_with(&home, "[task #1 stopped;").unwrap();
    assert!(
        report.starts_with("[chat #5] [task #1 stopped; a background agent's report, not the owner's words] goal: wait [hang]"),
        "{report}"
    );
    assert!(
        report.ends_with("\n> [zephyr] task #1 was stopped"),
        "{report}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_finished_task_reports_on_the_channel_that_started_it() {
    let home = home("task-done");
    let gw = start(config(&home));
    gw.say_on(
        "discord",
        "m1",
        "downer",
        r#"[call task {"action": "start", "goal": "find the price"}] ok"#,
    )
    .await;
    let replies = gw.wait_replies_on("discord", 2).await;
    assert!(
        replies[0].contains("\ntask: task #1 started"),
        "{replies:?}"
    );
    // The child's last message (the fake echoes its prompt) is the report main reads.
    assert!(
        replies[1].starts_with("echo: [discord #2] [task #1 finished; a background agent's report"),
        "{replies:?}"
    );
    assert!(
        replies[1].contains("\n> echo: You are a background task")
            && replies[1].ends_with("\n> Goal: find the price"),
        "{replies:?}"
    );
    assert!(gw.replies_on("chat").is_empty());
    assert_eq!(task_status(&home, "completed"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn two_tasks_run_at_once_and_owner_stop_ends_them_all() {
    let home = home("task-owner-stop");
    let gw = start(config(&home));
    let call = format!(r#"[call task {{"action": "start", "goal": "{HANGING_GOAL}"}}]"#);
    gw.say("m1", "owner", &format!("{call} {call} {call}"))
        .await;
    gw.wait_replies(1).await;
    wait(|| task_status(&home, "running") == 2 && task_status(&home, "queued") == 1).await;
    gw.say("m2", "owner", "/task list").await;
    let list = gw.wait_replies(2).await[1].clone();
    assert!(list.contains("#3 queued"), "{list}");
    gw.say("m3", "owner", "/stop").await;
    assert_eq!(gw.wait_replies(3).await[2], "[zephyr] stopped");
    wait(|| task_status(&home, "stopped") == 3).await;
    // The two that ran report back; the queued one never started, so it has nothing to say.
    wait(|| inbox_reports(&home) == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(inbox_reports(&home), 2);
    gw.say("m4", "owner", "/task stop 9").await;
    assert_eq!(
        gw.wait_replies(4).await[3],
        "[zephyr] task #9 is not queued or running"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_task_cut_between_steps_carries_on_with_the_resume_note() {
    let home = home("task-restart");
    let gw = start(config(&home));
    let start_call = format!(r#"[call task {{"action": "start", "goal": "{HANGING_GOAL}"}}]"#);
    gw.say("m1", "owner", &start_call).await;
    gw.wait_replies(1).await;
    wait(|| task_prompts(&home) == 1).await;
    gw.shutdown().await;
    // Nothing for rpi to resume: its session ends at the prompt. The task is told to go on.
    let gw = start(config(&home));
    wait(|| task_status(&home, "completed") == 1).await;
    assert!(log(&home, "resume_pending").is_empty());
    let prompts = log(&home, "prompt");
    assert!(
        prompts
            .iter()
            .any(|p| p["message"] == zephyr::scheduler::RESUME_NOTE),
        "{prompts:?}"
    );
    wait(|| inbox_reports(&home) == 1).await;
    let store = Store::open(&home.join("state.db")).unwrap();
    let report = ["new", "dispatched", "done"]
        .iter()
        .flat_map(|st| store.inbox_with_status(st).unwrap())
        .find(|r| r.text.starts_with("[task #"))
        .unwrap()
        .text;
    assert!(
        report.starts_with("[task #1 finished;")
            && report.ends_with("> echo: Zephyr restarted while you were working. Check the current state of anything that was in progress (files, processes, tasks, external actions) before repeating it, then continue."),
        "{report}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_task_cannot_start_tasks_or_use_gateway() {
    let home = home("task-nested");
    let gw = start(config(&home));
    // `\u007b` and `\u007d` are braces: the calls only appear once the goal is decoded.
    let goal = r#"\u005bcall task \u007b\"action\": \"list\"\u007d] \u005bcall gateway \u007b\"command\": \"/status\"\u007d]"#;
    gw.say(
        "m1",
        "owner",
        &format!(r#"[call task {{"action": "start", "goal": "{goal}"}}]"#),
    )
    .await;
    let report = gw.wait_replies(2).await[1].clone();
    assert!(
        report.contains(
            "\n> task: not available in background tasks\n> gateway: not available in background tasks"
        ),
        "{report}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn pause_aborts_a_running_task() {
    let home = home("task-pause");
    let gw = start(config(&home));
    let call = format!(r#"[call task {{"action": "start", "goal": "{HANGING_GOAL}"}}]"#);
    gw.say("m1", "owner", &call).await;
    gw.wait_replies(1).await;
    wait(|| task_status(&home, "running") == 1).await;
    gw.say("m2", "owner", "/pause").await;
    wait(|| task_status(&home, "failed") == 1).await;
    let report = last_prompt(&home, 1).await;
    let rows = Store::open(&home.join("state.db"))
        .unwrap()
        .inbox_with_status("new")
        .unwrap();
    // Paused, so the report waits in the inbox for /resume like any message.
    assert!(
        rows.iter()
            .any(|r| r.text.ends_with("\n> [zephyr] task #1 was aborted: paused")),
        "{rows:?} {report}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_message_sent_mid_tool_is_steered_in_and_answered_before_the_run_ends() {
    let home = home("steer");
    let gw = start(config(&home));
    std::fs::write(home.join("tool_wait"), "").unwrap();
    gw.say("m1", "owner", "[tool] look around").await;
    wait(|| main_session(&home).contains("toolCall")).await; // main is mid-run, in its tool
    gw.say("m2", "owner", "what are you doing?").await;
    wait(|| !log(&home, "queue_message").is_empty()).await;
    assert_eq!(log(&home, "queue_message")[0]["deliverAs"], "steer");
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    // m2 was answered inside the run, so its reply goes out before the run's own. It went in
    // with the note that tells the model to answer it and carry on.
    let m2 = InboxRow {
        id: 2,
        channel: "chat".into(),
        sender: "owner".into(),
        text: "what are you doing?".into(),
        tries: 1,
    };
    assert_eq!(
        gw.wait_replies(2).await,
        [
            format!("echo: {}", zephyr::router::steered(&m2)),
            "echo: [chat #1] [tool] look around".to_string()
        ]
    );
    assert!(
        log(&home, "clear_queue").is_empty(),
        "delivered, nothing taken back"
    );
    assert_eq!(log(&home, "prompt").len(), 1, "no run of its own");
    gw.shutdown().await;
}

#[tokio::test]
async fn the_one_text_after_a_steer_goes_out_once() {
    let home = home("steer-once");
    let gw = start(config(&home));
    std::fs::write(home.join("tool_wait"), "").unwrap();
    gw.say("m1", "owner", "[tool] look around").await;
    wait(|| main_session(&home).contains("toolCall")).await;
    // `[quiet]`: the fake writes nothing for it, so the run's final text is the only answer.
    gw.say("m2", "owner", "[quiet] thanks").await;
    wait(|| !log(&home, "queue_message").is_empty()).await;
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    wait(|| inbox_status(&home, "done") == 2 && log(&home, "prompt").len() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The run's own row cannot tell "one text for both" from "answered the steer and stopped",
    // so it goes once more as a continuation; a model that had finished answers NO_REPLY,
    // which is not sent.
    assert_eq!(gw.replies(), ["echo: [chat #1] [tool] look around"]);
    let again = log(&home, "prompt")[1]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(again.contains("reply with exactly NO_REPLY"), "{again}");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_run_that_stops_at_a_steered_answer_carries_on_with_its_own_request() {
    let home = home("steer-stop");
    let gw = start(config(&home));
    std::fs::write(home.join("tool_wait"), "").unwrap();
    // As Haiku did live: it answered the steered question and ended, the research unfinished.
    gw.say(
        "m1",
        "owner",
        "[stop-after-steer] [unfinished] [tool] research",
    )
    .await;
    wait(|| main_session(&home).contains("toolCall")).await;
    gw.say("m2", "owner", "what is 3+5?").await;
    wait(|| !log(&home, "queue_message").is_empty()).await;
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    let replies = gw.wait_replies(2).await;
    assert!(
        replies[0].starts_with("echo: [chat #2] what is 3+5?"),
        "{replies:?}"
    );
    // The research was not closed silently: it went again and got its own answer.
    assert!(
        replies[1].starts_with("echo: [chat #1] [stop-after-steer] [unfinished] [tool] research"),
        "{replies:?}"
    );
    assert!(replies[1].contains("carry on with it"), "{replies:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.replies().len(), 2, "each message answered once");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_steer_racing_the_end_of_a_run_is_taken_back_and_sent_again() {
    let home = home("steer-race");
    // The fake reads nothing while it is slow, so the steer lands after the run has ended.
    std::fs::write(home.join("slow"), "1").unwrap();
    let mut cfg = config(&home);
    // The auth check's `get_auth_status` would wait out the slow run (the fake reads nothing
    // then; real rpi answers mid-run) and hold "second" until the run had ended.
    cfg.auth.enabled = false;
    let gw = start(cfg);
    gw.say("m1", "owner", "first").await;
    wait(|| !log(&home, "prompt").is_empty()).await;
    gw.say("m2", "owner", "second").await;
    assert_eq!(
        gw.wait_replies(2).await,
        ["echo: [chat #1] first", "echo: [chat #2] second"]
    );
    assert_eq!(log(&home, "queue_message")[0]["deliverAs"], "steer");
    assert_eq!(
        log(&home, "clear_queue").len(),
        1,
        "the late steer is taken back"
    );
    assert_eq!(
        log(&home, "prompt").len(),
        2,
        "and sent again as a prompt, once"
    );
    gw.shutdown().await;
}

fn proc_status(home: &Path, status: &str) -> i64 {
    let counts = Store::open(&home.join("state.db"))
        .unwrap()
        .counts()
        .unwrap();
    counts
        .iter()
        .find(|(t, s, _)| t == "procs" && s == status)
        .map_or(0, |c| c.2)
}

#[tokio::test]
async fn exec_answers_in_the_turn_when_its_command_ends() {
    let home = home("exec-quick");
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        r#"[call exec {"command": "echo hi; exit 3"}] run it"#,
    )
    .await;
    let reply = gw.wait_replies(1).await[0].clone();
    assert!(reply.ends_with("\nexec: exited 3\nhi"), "{reply}");
    assert_eq!(proc_status(&home, "exited"), 1);
    // It ended in the turn, so the model has its output: no report comes after.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.replies().len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn an_owner_message_moves_a_running_command_to_the_background() {
    let home = home("exec-yield");
    let gw = start(config(&home));
    let call = r#"[call exec {"command": "sleep 2; echo done"}] go"#;
    gw.say("m1", "owner", call).await;
    wait(|| proc_status(&home, "running") == 1).await;
    gw.say("m2", "owner", "hi").await;
    let replies = gw.wait_replies(2).await;
    let has = |r: &[String], s: &str| r.iter().any(|x| x.contains(s));
    assert!(
        has(
            &replies,
            "\nexec: Moved to the background (a message for you arrived)"
        ),
        "{replies:?}"
    );
    assert!(
        has(&replies, "echo: [chat #2] hi"),
        "answered while it runs: {replies:?}"
    );
    // Its end comes back to main as a quoted report.
    let replies = gw.wait_replies(3).await;
    let report = replies
        .iter()
        .find(|r| r.contains("[process #1 exited 0 after"));
    assert!(
        report.is_some_and(|r| r.ends_with("\n> done")),
        "{replies:?}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn background_commands_report_when_stopped_by_the_agent_or_the_owner() {
    let home = home("exec-stop");
    let gw = start(config(&home));
    let call = r#"[call exec {"command": "sleep 30", "background": true}]"#;
    gw.say("m1", "owner", &format!("{call} {call}")).await;
    let first = gw.wait_replies(1).await[0].clone();
    assert!(
        first.contains("exec: Moved to the background (you asked for the background)"),
        "{first}"
    );
    assert_eq!(proc_status(&home, "running"), 2);
    gw.say("m2", "owner", "/process list").await;
    assert!(gw.wait_replies(2).await[1].contains("#1 running"));
    gw.say(
        "m3",
        "owner",
        r#"[call process {"action": "stop", "id": 1}]"#,
    )
    .await;
    wait(|| proc_status(&home, "stopped") == 1).await;
    gw.say("m4", "owner", "/stop").await;
    wait(|| proc_status(&home, "stopped") == 2).await;
    // The process answer, /stop's, and one report per stopped command (order varies).
    let replies = gw.wait_replies(6).await;
    let count = |s: &str| replies.iter().filter(|r| r.contains(s)).count();
    assert_eq!(
        count("process: [zephyr] stopping process #1"),
        1,
        "{replies:?}"
    );
    assert_eq!(count("was stopped after"), 2, "{replies:?}");
    gw.say("m5", "owner", "/process stop 1").await;
    let last = gw.wait_replies(7).await;
    assert!(
        last.contains(&"[zephyr] process #1 is not running".to_string()),
        "{last:?}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn the_backstop_moves_a_long_command_to_the_background() {
    let home = home("exec-backstop");
    let mut cfg = config(&home);
    cfg.exec_backstop = Duration::from_secs(1);
    let gw = start(cfg);
    gw.say(
        "m1",
        "owner",
        r#"[call exec {"command": "sleep 3; echo late"}]"#,
    )
    .await;
    let first = gw.wait_replies(1).await[0].clone();
    assert!(
        first.contains("Moved to the background (it ran past 1s)"),
        "{first}"
    );
    let report = gw.wait_replies(2).await[1].clone();
    assert!(report.ends_with("\n> late"), "{report}");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_crash_after_a_steered_answer_still_gives_the_run_its_own_reply() {
    let home = home("steer-crash");
    std::fs::write(home.join("tool_wait"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "[pend-after-steer] [tool] look around")
        .await;
    wait(|| main_session(&home).contains("toolCall")).await;
    gw.say("m2", "owner", "what are you doing?").await;
    wait(|| !log(&home, "queue_message").is_empty()).await;
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    // m2 is answered mid-run; then the gateway dies before the run settles.
    wait(|| gw.replies().len() == 1).await;
    assert!(gw.replies()[0].starts_with("echo: [chat #2] what are you doing?"));
    gw.task.abort(); // like kill -9
    let _ = gw.task.await;

    // The steer state comes back from the store: m1 gets the resumed run's text, not the
    // fallback, and m2 is not answered again.
    let gw = start(config(&home));
    let replies = gw.wait_replies(1).await;
    assert!(replies[0].starts_with("resumed: "), "{replies:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.replies().len(), 1, "{:?}", gw.replies());
    assert_eq!(inbox_status(&home, "done"), 2);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_continuation_waiting_at_a_restart_still_goes_as_one() {
    let home = home("continue-restart");
    let cfg = config(&home);
    let store = Store::open(&cfg.db_path()).unwrap();
    let id = store
        .insert_inbox(&Inbound {
            channel: "chat".into(),
            platform_msg_id: "m1".into(),
            sender: "owner".into(),
            text: "[unfinished] research".into(),
        })
        .unwrap()
        .unwrap();
    // As `reconcile` leaves a row whose run stopped at a steered answer.
    store.requeue_as_continue(id).unwrap();
    drop(store);
    let gw = start(cfg);
    let reply = gw.wait_replies(1).await[0].clone();
    assert!(
        reply.starts_with("echo: [chat #1] [unfinished] research"),
        "{reply}"
    );
    assert!(reply.contains("carry on with it"), "{reply}");
    gw.shutdown().await;
}

/// Prompts a fake child got that started or resumed a background task.
fn task_prompts(home: &Path) -> usize {
    log(home, "prompt")
        .iter()
        .filter(|p| {
            let m = p["message"].as_str().unwrap_or("");
            // `starts_with`: main's prompt for the report quotes the brief too.
            m.starts_with("You are a background task") || m == zephyr::scheduler::RESUME_NOTE
        })
        .count()
}

#[tokio::test]
async fn a_task_a_restart_cut_resumes_in_its_session() {
    let home = home("task-resume");
    let gw = start(config(&home));
    // As HANGING_GOAL, but the fake marks the task's session resumable, as a crash leaves it.
    let goal = HANGING_GOAL.replace("hang]", "hang-pending]");
    let call = format!(r#"[call task {{"action": "start", "goal": "{goal}"}}]"#);
    gw.say("m1", "owner", &call).await;
    gw.wait_replies(1).await;
    wait(|| task_prompts(&home) == 1).await;
    gw.task.abort(); // like kill -9: the task's child goes with it, its row stays `running`
    let _ = gw.task.await;

    let gw = start(config(&home));
    wait(|| task_status(&home, "completed") == 1).await;
    assert_eq!(log(&home, "resume_pending").len(), 1);
    assert_eq!(task_prompts(&home), 1, "resumed, not started again");
    let note = log(&home, "queue_message");
    assert!(
        note.iter()
            .any(|q| q["message"] == zephyr::scheduler::RESUME_NOTE && q["deliverAs"] == "steer"),
        "{note:?}"
    );
    let task = Store::open(&home.join("state.db"))
        .unwrap()
        .tasks(1)
        .unwrap()[0]
        .clone();
    assert_eq!((task.resumes, task.session.as_deref()), (1, Some("task-1")));
    wait(|| inbox_reports(&home) == 1).await;
    // Main's prompt for the report, not just the newest prompt so far (under load it comes later).
    wait(|| prompt_with(&home, "[task #1 finished;").is_some()).await;
    let report = prompt_with(&home, "[task #1 finished;").unwrap();
    assert!(
        report.contains("> resumed: You are a background task"),
        "{report}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_task_that_finished_before_the_restart_reports_without_running_again() {
    let home = home("task-finished-unreported");
    let cfg = config(&home);
    {
        let store = Store::open(&cfg.db_path()).unwrap();
        store.add_task("count the files", "chat", "owner").unwrap();
        store.start_next_task(1).unwrap().unwrap();
        store.set_task_session(1, "task-1").unwrap();
    }
    // Its session already ends with the agent's answer: the gateway died before the report.
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let session = serde_json::json!({"pending": false, "messages": [
        {"kind": "user", "role": "user", "content": "You are a background task …", "timestamp": 0},
        {"kind": "assistant", "role": "assistant", "stopReason": "stop", "timestamp": 0,
         "content": [{"type": "text", "text": "there are 3 files"}]}
    ]});
    std::fs::write(home.join("sessions/task-1.json"), session.to_string()).unwrap();
    let gw = start(cfg);
    wait(|| task_status(&home, "completed") == 1).await;
    wait(|| prompt_with(&home, "[task #1 finished;").is_some()).await;
    let report = prompt_with(&home, "[task #1 finished;").unwrap();
    assert!(report.ends_with("> there are 3 files"), "{report}");
    assert_eq!(task_prompts(&home), 0, "no model call for the task");
    assert!(log(&home, "resume_pending").is_empty());
    gw.shutdown().await;
}

#[tokio::test]
async fn a_task_that_stalls_is_aborted() {
    let home = home("task-stall");
    let mut cfg = config(&home);
    cfg.stall_limit = Duration::from_secs(1);
    let gw = start(cfg);
    let call = format!(r#"[call task {{"action": "start", "goal": "{HANGING_GOAL}"}}]"#);
    gw.say("m1", "owner", &call).await;
    wait(|| task_status(&home, "failed") == 1).await;
    wait(|| prompt_with(&home, "[task #1 failed;").is_some()).await;
    let report = prompt_with(&home, "[task #1 failed;").unwrap();
    assert!(
        report.contains("> [zephyr] task #1 failed: stalled: no progress for 1s"),
        "{report}"
    );
    assert!(!log(&home, "abort").is_empty());
    gw.shutdown().await;
}

#[tokio::test]
async fn a_main_run_that_stalls_is_stopped_and_the_owner_told() {
    let home = home("main-stall");
    let mut cfg = config(&home);
    cfg.stall_limit = Duration::from_secs(1);
    let gw = start(cfg);
    gw.say("m1", "owner", "[hang] think hard").await;
    let replies = gw.wait_replies(2).await;
    assert!(
        replies
            .iter()
            .any(|r| r == "[zephyr] stopped a run that made no progress for 1 s"),
        "{replies:?}"
    );
    assert_eq!(log(&home, "abort").len(), 1);
    gw.shutdown().await;
}

/// A task goal line that writes `progress: halfway` first (see HANGING_GOAL for the escape).
const PROGRESS: &str = r"\u005bprogress]";

#[tokio::test]
async fn a_stopped_task_reports_what_it_had_got_to() {
    let home = home("task-partial");
    let gw = start(config(&home));
    let goal = format!("{PROGRESS} {HANGING_GOAL}");
    let call = format!(r#"[call task {{"action": "start", "goal": "{goal}"}}]"#);
    gw.say("m1", "owner", &call).await;
    gw.wait_replies(1).await;
    wait(|| task_status(&home, "running") == 1).await;
    gw.say("m2", "owner", "/task stop 1").await;
    wait(|| task_status(&home, "stopped") == 1 && inbox_reports(&home) == 1).await;
    let store = Store::open(&home.join("state.db")).unwrap();
    let report = ["new", "dispatched", "done"]
        .iter()
        .flat_map(|st| store.inbox_with_status(st).unwrap())
        .find(|r| r.text.starts_with("[task #1 stopped"))
        .unwrap();
    // As Hermes's interrupted children: the report keeps what the run had got to.
    assert!(
        report.text.ends_with(
            "> [zephyr] task #1 was stopped\n> Its last message before that:\n> progress: halfway"
        ),
        "{}",
        report.text
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_resume_note_the_run_never_reached_does_not_open_the_next_run() {
    let home = home("note-leftover");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await;
    gw.task.abort(); // like kill -9
    let _ = gw.task.await;
    // The fake's resume settles before any step, so the note steered after it is never reached.
    let gw = start(config(&home));
    assert_eq!(gw.wait_replies(1).await, ["resumed: [chat #1] long job"]);
    wait(|| !log(&home, "clear_queue").is_empty()).await; // taken back at the settle
    gw.say("m2", "owner", "next").await;
    assert_eq!(gw.wait_replies(2).await[1], "echo: [chat #2] next");
    assert!(
        !main_session(&home).contains("Zephyr restarted while you were working"),
        "the note did not open the next run"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn a_steer_the_resumed_run_never_reached_is_sent_again_once() {
    let home = home("note-steer");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await;
    gw.task.abort();
    let _ = gw.task.await;
    // The resumed run stays open: the note and then the owner's steer are both held.
    std::fs::write(home.join("resume_wait"), "").unwrap();
    let gw = start(config(&home));
    wait(|| log(&home, "queue_message").len() == 1).await;
    gw.say("m2", "owner", "next").await;
    wait(|| log(&home, "queue_message").len() == 2).await;
    std::fs::remove_file(home.join("resume_wait")).unwrap();
    // Neither was delivered: the queue is cleared and the steer goes again, as a prompt.
    assert_eq!(
        gw.wait_replies(2).await,
        ["resumed: [chat #1] long job", "echo: [chat #2] next"]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.replies().len(), 2, "each message answered once");
    assert!(!log(&home, "clear_queue").is_empty());
    assert!(!main_session(&home).contains("Zephyr restarted while you were working"));
    gw.shutdown().await;
}

/// Five identical failing `read` calls, under a `tool_fail` file.
const FAILING_CALLS: &str = r#"[call read {"path": "x"}] [call read {"path": "x"}] [call read {"path": "x"}] [call read {"path": "x"}] [call read {"path": "x"}] go"#;
const GUARD_NOTE: &str = "[zephyr] read failed 3 times in a row with the same error (tool error: \
                          no such file: x). Don't retry it unchanged: change approach, or stop \
                          and tell the owner.";

#[tokio::test]
async fn the_same_failure_is_steered_at_three_and_stopped_at_five() {
    let home = home("guard");
    std::fs::write(home.join("tool_fail"), "no such file: x").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", FAILING_CALLS).await;
    let replies = gw.wait_replies(2).await;
    assert_eq!(
        replies[1],
        "[zephyr] stopped a run that repeated a failing read call 5 times"
    );
    let steers = log(&home, "queue_message");
    assert_eq!(steers.len(), 1, "{steers:?}");
    assert_eq!(steers[0]["message"], GUARD_NOTE);
    assert_eq!(steers[0]["deliverAs"], "steer");
    assert_eq!(log(&home, "abort").len(), 1);
    // A note the run never reached is taken back, so it cannot open the next run.
    assert!(!log(&home, "clear_queue").is_empty());
    std::fs::remove_file(home.join("tool_fail")).unwrap();
    gw.say("m2", "owner", "hi").await;
    assert_eq!(gw.wait_replies(3).await[2], "echo: [chat #2] hi");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_job_repeating_a_failing_call_is_stopped() {
    let home = home("guard-job");
    std::fs::write(home.join("tool_fail"), "no such file: x").unwrap();
    let gw = start(config(&home));
    gw.say(
        "m1",
        "owner",
        &format!("/job add loop | {NEVER} | {FAILING_CALLS}"),
    )
    .await;
    gw.say("m2", "owner", "/job run 1").await;
    assert_eq!(
        gw.wait_replies(3).await[2],
        "[zephyr] job #1 (loop) run 1 failed: stopped: repeated a failing read call 5 times"
    );
    let steers = log(&home, "queue_message");
    assert_eq!(steers.len(), 1, "{steers:?}");
    assert_eq!(steers[0]["message"], GUARD_NOTE);
    assert_eq!(log(&home, "abort").len(), 1);
    gw.shutdown().await;
}

const UNDONE: &str = "[zephyr] undone: the agent no longer sees your last message or its reply; \
                      files it changed stay changed (see /rollback)";

/// The main session's messages, as the fake keeps them.
fn messages(home: &Path) -> Vec<Value> {
    let session: Value = serde_json::from_str(&main_session(home)).unwrap();
    session["messages"].as_array().unwrap().clone()
}

/// The text of each message in the main session.
fn texts(home: &Path) -> Vec<String> {
    messages(home)
        .iter()
        .map(zephyr::rpc::message_text)
        .collect()
}

#[tokio::test]
async fn undo_takes_the_last_exchange_and_its_steers_out_of_the_context() {
    let home = home("undo");
    let gw = start(config(&home));
    gw.say("m1", "owner", "one").await;
    gw.wait_replies(1).await;
    // The second run has a steered message in it: both rows are undone together.
    std::fs::write(home.join("tool_wait"), "").unwrap();
    gw.say("m2", "owner", "[tool] look around").await;
    wait(|| main_session(&home).contains("toolCall")).await;
    gw.say("m3", "owner", "what are you doing?").await;
    wait(|| !log(&home, "queue_message").is_empty()).await;
    std::fs::remove_file(home.join("tool_wait")).unwrap();
    gw.wait_replies(3).await;

    gw.say("m4", "owner", "/undo").await;
    assert_eq!(gw.wait_replies(4).await[3], UNDONE);
    // The leaf goes on the entry before the run's own message: "one", which stays.
    assert_eq!(log(&home, "navigate")[0]["targetId"], "m1");
    assert_eq!(inbox_status(&home, "undone"), 2);
    assert_eq!(texts(&home), ["[chat #1] one", "echo: [chat #1] one"]);

    // The first message has nothing before it: the root, an empty context.
    gw.say("m5", "owner", "/undo").await;
    assert_eq!(gw.wait_replies(5).await[4], UNDONE);
    assert!(log(&home, "navigate")[1].get("targetId").is_none());
    assert!(messages(&home).is_empty());
    assert_eq!(inbox_status(&home, "undone"), 3);
    gw.say("m6", "owner", "/undo").await;
    assert_eq!(gw.wait_replies(6).await[5], "[zephyr] nothing to undo");

    // The next message carries on from there.
    gw.say("m7", "owner", "fresh").await;
    assert_eq!(gw.wait_replies(7).await[6], "echo: [chat #7] fresh");
    assert_eq!(log(&home, "prompt").len(), 3);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_gateway_crash_after_undo_sends_nothing_again() {
    let home = home("undo-crash");
    let gw = start(config(&home));
    gw.say("m1", "owner", "one").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/undo").await;
    assert_eq!(gw.wait_replies(2).await, ["echo: [chat #1] one", UNDONE]);
    gw.task.abort();
    let _ = gw.task.await;

    let gw = start(config(&home));
    gw.say("m3", "owner", "two").await;
    assert_eq!(gw.wait_replies(1).await, ["echo: [chat #3] two"]);
    assert_eq!(
        log(&home, "prompt").len(),
        2,
        "the undone row never went again"
    );
    assert_eq!(inbox_status(&home, "undone"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn retry_undoes_and_sends_the_same_text_once() {
    let home = home("retry");
    let gw = start(config(&home));
    gw.say("m1", "owner", "one").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/retry").await;
    assert_eq!(
        gw.wait_replies(3).await,
        [
            "echo: [chat #1] one",
            "[zephyr] undone; sending your last message again as #3",
            "echo: [chat #3] one"
        ]
    );
    let prompts = log(&home, "prompt");
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert_eq!(prompts[1]["message"], "[chat #3] one");
    assert_eq!(texts(&home), ["[chat #3] one", "echo: [chat #3] one"]);
    let db = rusqlite::Connection::open(home.join("state.db")).unwrap();
    let row: (String, String, String) = db
        .query_row(
            "SELECT platform_msg_id, sender, status FROM inbox WHERE id = 3",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("retry-1-1".into(), "owner".into(), "done".into()));
    assert_eq!(inbox_status(&home, "undone"), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn compact_replies_with_rpis_summary() {
    let home = home("compact");
    let gw = start(config(&home));
    gw.say("m1", "owner", "one").await;
    gw.wait_replies(1).await;
    gw.say("m2", "owner", "/compact keep the names").await;
    let summary = gw.wait_replies(2).await[1].clone();
    assert!(
        summary.starts_with("[zephyr] Conversation compacted · ")
            && summary.ends_with(" tokens before, 0 messages kept."),
        "{summary}"
    );
    assert_eq!(
        log(&home, "compact")[0]["customInstructions"],
        "keep the names"
    );
    gw.say("m3", "owner", "/compact").await;
    gw.wait_replies(3).await;
    assert!(log(&home, "compact")[1].get("customInstructions").is_none());
    // Idle again afterwards.
    gw.say("m4", "owner", "two").await;
    assert_eq!(gw.wait_replies(4).await[3], "echo: [chat #4] two");
    gw.shutdown().await;
}

#[tokio::test]
async fn usage_lists_each_day_and_account() {
    let home = home("usage");
    let cfg = config(&home);
    let store = Store::open(&cfg.db_path()).unwrap();
    let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
    let (today, yesterday, old): (String, String, String) = db
        .query_row(
            "SELECT date('now'), date('now', '-1 day'), date('now', '-30 days')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    store.add_spend(&today, 100, 0.5).unwrap();
    store.add_spend(&yesterday, 7, 0.01).unwrap();
    store.add_spend(&old, 999, 9.0).unwrap();
    store.add_account_spend("a", (60, 0.3)).unwrap();
    store.add_account_spend("b", (40, 0.2)).unwrap();
    drop(store);
    let gw = start(cfg);
    gw.say("m1", "owner", "/usage").await;
    gw.say("m2", "owner", "/usage 1").await;
    gw.say("m3", "owner", "/usage soon").await;
    let replies = gw.wait_replies(3).await;
    assert_eq!(
        replies[0],
        format!(
            "[zephyr] usage over the last 7 days (UTC):\n{today}: 100 tokens, $0.5000\n  \
             a: 60 tokens, $0.3000\n  b: 40 tokens, $0.2000\n{yesterday}: 7 tokens, $0.0100\n\
             total: 107 tokens, $0.5100"
        )
    );
    assert_eq!(
        replies[1],
        format!(
            "[zephyr] usage over the last 1 day (UTC):\n{today}: 100 tokens, $0.5000\n  \
             a: 60 tokens, $0.3000\n  b: 40 tokens, $0.2000"
        )
    );
    assert_eq!(
        replies[2],
        "[zephyr] usage: /usage [days], 1 to 366 (default 7)"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn undo_retry_and_compact_wait_for_the_run() {
    let home = home("undo-busy");
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(config(&home));
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await;
    for (id, command) in [("m2", "/undo"), ("m3", "/retry"), ("m4", "/compact")] {
        gw.say(id, "owner", command).await;
    }
    let busy = "[zephyr] the agent is busy: wait for its reply, or /stop it first";
    assert_eq!(gw.wait_replies(3).await, [busy, busy, busy]);
    assert!(log(&home, "navigate").is_empty());
    assert!(log(&home, "compact").is_empty());
    gw.say("m5", "owner", "/stop").await;
    gw.wait_replies(5).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn a_rollback_waits_until_nothing_runs_then_restores_the_projects() {
    let home = home("rollback");
    let app = home.join("projects/app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("a.txt"), "before").unwrap();
    let mut cfg = config(&home);
    cfg.projects = Some(home.join("projects"));
    std::fs::write(home.join("hang"), "").unwrap();
    let gw = start(cfg);
    gw.say("m1", "owner", "long job").await;
    wait(|| !home.join("hang").exists()).await; // mid-run, with its checkpoint taken
    std::fs::write(app.join("a.txt"), "the run's edit").unwrap();

    gw.say("m2", "owner", "/rollback").await;
    let list = gw.wait_replies(1).await.remove(0);
    assert!(
        list.contains(", before \"long job\", app (1 files changed since)"),
        "{list}"
    );
    gw.say("m3", "owner", "/rollback 1").await;
    assert_eq!(
        gw.wait_replies(2).await[1],
        "[zephyr] not rolled back: a run still running; /stop them first"
    );
    let a = || std::fs::read_to_string(app.join("a.txt")).unwrap();
    assert_eq!(a(), "the run's edit");

    gw.say("m4", "owner", "/stop").await;
    gw.wait_replies(4).await; // the stop notice, then the aborted run's fallback reply
    gw.say("m5", "owner", "/rollback 1").await;
    let reply = gw.wait_replies(5).await.remove(4);
    assert!(
        reply.starts_with("[zephyr] rolled back app to #1"),
        "{reply}"
    );
    assert_eq!(a(), "before");
    gw.shutdown().await;
}
