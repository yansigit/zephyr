//! AuthKeeper (DESIGN §4.7) against the scripted fake child (tests/fake-rpi.py). The fake's
//! `$RPI_CODING_AGENT_DIR/auth.json` is the credential store, and `rpi auth refresh` (run as a
//! subprocess by the keeper) rewrites it.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;

/// A channel fed by the test. Its name is `chat`, so `owners.chat` applies.
struct Feed(tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Inbound>>);

impl Channel for Feed {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: tokio::sync::mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let mut rx = self.0.lock().await;
        while let Some(m) = rx.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, _msg: &Outbound) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Gateway {
    feed: tokio::sync::mpsc::Sender<Inbound>,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
    home: PathBuf,
    agent: PathBuf,
}

fn now() -> i64 {
    (zephyr::unix_millis() / 1000) as i64
}

/// A fresh home and agent dir, with `auth` written as `auth.json` when given.
fn setup(name: &str, auth: Option<Value>, extra: &str) -> (Config, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "zephyr-auth-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let home = root.join("home");
    let agent = root.join("agent");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    if let Some(auth) = auth {
        std::fs::write(agent.join("auth.json"), auth.to_string()).unwrap();
    }
    let cfg = toml::from_str(&format!(
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\n{extra}",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        agent,
    ))
    .unwrap();
    (cfg, home, agent)
}

fn start(name: &str, auth: Option<Value>, extra: &str) -> Gateway {
    let (cfg, home, agent) = setup(name, auth, extra);
    run(cfg, home, agent)
}

fn run(cfg: Config, home: PathBuf, agent: PathBuf) -> Gateway {
    let (feed, rx) = tokio::sync::mpsc::channel(4);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, &[Feed(tokio::sync::Mutex::new(rx))], async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        feed,
        stop,
        task,
        home,
        agent,
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
        self.feed.send(m).await.unwrap();
    }
    async fn shutdown(self) {
        let _ = self.stop.send(());
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

/// The fake's pid: changes each time a child starts.
fn pid(home: &Path) -> Option<String> {
    std::fs::read_to_string(home.join("pid")).ok()
}

/// Every line the fake logged in the home, in order: child commands and `auth-refresh` runs.
fn log(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_type(home: &Path, kind: &str) -> Vec<Value> {
    log(home)
        .into_iter()
        .filter(|c| c["type"] == kind)
        .collect()
}

/// Sends a message and waits for the reconcile after its run settled. The router checks
/// credentials inline in its loop, so a check under way when the message came in has finished
/// by then, and over the run's events one that was due has had its turn.
async fn round_trip(gw: &Gateway) {
    gw.say("probe", "hi").await;
    wait(|| {
        let commands = log(&gw.home);
        let prompt = commands.iter().position(|c| c["type"] == "prompt");
        prompt.is_some_and(|at| commands[at..].iter().any(|c| c["type"] == "get_messages"))
    })
    .await;
}

fn refreshes(home: &Path) -> Vec<Value> {
    of_type(home, "auth-refresh")
}

fn stored(agent: &Path, provider: &str) -> Value {
    let store: Value =
        serde_json::from_str(&std::fs::read_to_string(agent.join("auth.json")).unwrap()).unwrap();
    store[provider].clone()
}

fn expiring_in(secs: i64) -> Value {
    json!({"anthropic": {"type": "oauth", "access": "old", "refresh": "r", "expires": now() + secs}})
}

#[tokio::test]
async fn expiring_oauth_is_refreshed_then_the_child_restarts() {
    let gw = start("refresh", Some(expiring_in(300)), "");
    wait(|| pid(&gw.home).is_some()).await;
    let first = pid(&gw.home);
    wait(|| pid(&gw.home) != first).await;

    let refreshed = refreshes(&gw.home);
    assert_eq!(refreshed.len(), 1, "one refresh, no retries");
    assert_eq!(refreshed[0]["provider"], "anthropic");
    let status_at = log(&gw.home)
        .iter()
        .position(|c| c["type"] == "get_auth_status")
        .unwrap();
    let refresh_at = log(&gw.home)
        .iter()
        .position(|c| c["type"] == "auth-refresh")
        .unwrap();
    assert!(
        status_at < refresh_at,
        "the status is read before the refresh"
    );
    assert!(stored(&gw.agent, "anthropic")["expires"].as_i64().unwrap() > now() + 15 * 60);
    gw.shutdown().await;
}

/// The refresh lands while a run is in progress. The restart must wait for the settle, so the
/// run's reply is reconciled from the old child before the new one starts.
#[tokio::test]
async fn refresh_while_busy_restarts_only_after_the_settle() {
    let gw = start("busy", None, "[auth]\nrefresh_minutes = 0.02\n");
    std::fs::write(gw.home.join("slow"), "4").unwrap(); // each prompt takes 4s
    wait(|| pid(&gw.home).is_some()).await;
    let first = pid(&gw.home);

    gw.say("m1", "hello").await;
    wait(|| !of_type(&gw.home, "prompt").is_empty()).await;
    // Expires inside the window, so the next check (every 1.2s) refreshes during the run.
    std::fs::write(gw.agent.join("auth.json"), expiring_in(60).to_string()).unwrap();
    wait(|| !refreshes(&gw.home).is_empty()).await;
    wait(|| pid(&gw.home) != first).await;

    let commands: Vec<String> = log(&gw.home)
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_string())
        .collect();
    let refresh_at = commands.iter().position(|c| c == "auth-refresh").unwrap();
    let stop_at = commands.iter().rposition(|c| c == "stop").unwrap();
    let settle_read = commands[refresh_at..stop_at]
        .iter()
        .any(|c| c == "get_messages");
    assert!(
        settle_read,
        "the old child was reconciled (get_messages after the settle) before its stop: {commands:?}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn no_oauth_credentials_means_no_refresh_and_no_restart() {
    // An API key is a credential too, but not an OAuth one.
    let gw = start(
        "apikey",
        Some(json!({"openai": {"type": "api_key", "key": "sk-test"}})),
        "",
    );
    wait(|| !of_type(&gw.home, "get_auth_status").is_empty()).await;
    let first = pid(&gw.home);
    round_trip(&gw).await;
    assert!(refreshes(&gw.home).is_empty());
    assert_eq!(pid(&gw.home), first, "no restart");
    gw.shutdown().await;
}

#[tokio::test]
async fn an_absent_auth_file_means_no_refresh() {
    let gw = start("nofile", None, "");
    wait(|| !of_type(&gw.home, "get_auth_status").is_empty()).await;
    assert!(refreshes(&gw.home).is_empty());
    gw.shutdown().await;
}

#[tokio::test]
async fn a_credential_with_time_left_is_left_alone() {
    let gw = start("fresh", Some(expiring_in(7200)), "");
    wait(|| !of_type(&gw.home, "get_auth_status").is_empty()).await;
    let first = pid(&gw.home);
    round_trip(&gw).await;
    assert!(refreshes(&gw.home).is_empty());
    assert_eq!(pid(&gw.home), first);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_failed_refresh_leaves_the_child_running() {
    let gw = start("failing", Some(expiring_in(60)), "");
    std::fs::write(gw.home.join("oauth_fail"), "").unwrap();
    wait(|| !refreshes(&gw.home).is_empty()).await;
    let first = pid(&gw.home);
    round_trip(&gw).await;
    assert_eq!(refreshes(&gw.home).len(), 1);
    assert_eq!(pid(&gw.home), first, "nothing refreshed, so no restart");
    assert_eq!(stored(&gw.agent, "anthropic")["access"], "old");
    gw.shutdown().await;
}

#[tokio::test]
async fn disabled_means_the_child_is_never_asked() {
    let gw = start(
        "disabled",
        Some(expiring_in(60)),
        "[auth]\nenabled = false\n",
    );
    wait(|| pid(&gw.home).is_some()).await;
    round_trip(&gw).await;
    assert!(of_type(&gw.home, "get_auth_status").is_empty());
    assert!(refreshes(&gw.home).is_empty());
    gw.shutdown().await;
}

#[tokio::test]
async fn every_pooled_account_is_refreshed_with_its_own_dir() {
    let accounts = "[[account]]\nname = 'a'\nmodel = 'anthropic/m'\n[[account]]\nname = 'b'\nmodel = 'openai-codex/m'\n";
    let (cfg, home, agent) = setup("pool", None, accounts);
    let soon = |provider: &str| json!({provider: {"type": "oauth", "access": "old", "refresh": "r", "expires": now() + 60}});
    for (name, provider) in [("a", "anthropic"), ("b", "openai-codex")] {
        let dir = home.join("accounts").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), soon(provider).to_string()).unwrap();
    }
    let gw = run(cfg, home.clone(), agent);
    wait(|| refreshes(&home).len() >= 2).await;
    let mut providers: Vec<_> = refreshes(&home)
        .iter()
        .map(|r| r["provider"].as_str().unwrap().to_string())
        .collect();
    providers.sort();
    assert_eq!(providers, ["anthropic", "openai-codex"]);
    for (name, provider) in [("a", "anthropic"), ("b", "openai-codex")] {
        let dir = home.join("accounts").join(name);
        assert!(stored(&dir, provider)["expires"].as_i64().unwrap() > now() + 15 * 60);
    }
    gw.shutdown().await;
}
