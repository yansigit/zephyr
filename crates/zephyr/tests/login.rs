//! `/login` and `zephyr.login_*`: signing a provider in on the live gateway (DESIGN §4.3.1,
//! §4.2). `crate::auth` runs `rpi auth login --provider P` under `script` against the account's
//! own `RPI_CODING_AGENT_DIR`, reports what it printed, and records the account in `zephyr.toml`
//! once `rpi auth check --json` says the credential is stored. Nothing here restarts the daemon.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;

/// The key a test types in. It must never come back out of a poll, a reply or a log line.
const KEY: &str = "sk-test-4f0a9c";

struct Feed {
    rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Inbound>>,
    sent: Sent,
}

type Sent = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

impl Channel for Feed {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: tokio::sync::mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let mut rx = self.rx.lock().await;
        while let Some(m) = rx.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg.text.clone());
        Ok(())
    }
}

/// Removes the temp roots of earlier runs: a test that panics never gets to clean up after itself.
fn prune() {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let old = Duration::from_secs(3600);
    for e in dir.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| m.elapsed().unwrap_or_default() > old)
            .unwrap_or(false);
        if name.starts_with("zephyr-login-") && stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// A fresh home with a `zephyr.toml` that already holds `extra`, loaded as the gateway would.
fn setup(name: &str, extra: &str) -> (Config, PathBuf) {
    prune();
    let root = std::env::temp_dir().join(format!(
        "zephyr-login-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let toml = format!(
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\n{extra}",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        root.join("agent"),
    );
    let path = root.join("zephyr.toml");
    std::fs::write(&path, &toml).unwrap();
    let mut cfg = Config::load(&path).unwrap();
    // A sign-in's notices and the pool it changes are picked up on the router's tick.
    cfg.tick = Duration::from_millis(200);
    (cfg, home)
}

fn toml_at(cfg: &Config) -> String {
    std::fs::read_to_string(cfg.path.as_ref().expect("a config file")).unwrap()
}

/// Polls `auth::poll` until `done`, returning the last answer.
async fn until(id: &str, mut done: impl FnMut(&Value) -> bool) -> Value {
    for _ in 0..400 {
        let Some(login) = zephyr::auth::poll(id) else {
            panic!("login {id} vanished");
        };
        if done(&login) {
            return login;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("login {id} never settled: {:?}", zephyr::auth::poll(id));
}

/// Polls a condition on the filesystem for up to 20 s, then fails with `what`.
async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The `start` lines `fake-rpi.py` logged for the children it ran on `dir`.
fn starts(home: &Path, dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["type"] == "start" && same_dir(v["agentDir"].as_str(), dir))
        .collect()
}

/// Whether the agent dir fake-rpi.py logged is `dir`.
fn same_dir(logged: Option<&str>, dir: &Path) -> bool {
    let Some(logged) = logged else { return false };
    let logged = Path::new(logged);
    logged == dir
        || (logged.canonicalize().ok().is_some()
            && logged.canonicalize().ok() == dir.canonicalize().ok())
}

/// The pid of the rpi child the fake wrote to the home.
fn pid(home: &Path) -> u32 {
    std::fs::read_to_string(home.join("pid"))
        .expect("the fake logged no pid")
        .trim()
        .parse()
        .expect("a pid")
}

/// A process's command line, as one line.
fn cmdline_of(pid: u32) -> String {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    String::from_utf8_lossy(&bytes).replace('\0', " ")
}

/// The live processes running this login: the `script` wrapper, the shell it runs the command in,
/// and the rpi child itself all carry `auth login` on their command line and the account's
/// `RPI_CODING_AGENT_DIR` in their environment.
fn login_processes(dir: &Path) -> Vec<u32> {
    let want = format!("RPI_CODING_AGENT_DIR={}", dir.display());
    let mut pids: Vec<u32> = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let cmd = cmdline_of(pid);
        if !cmd.contains("auth login --provider") {
            continue;
        }
        let environ = std::fs::read(path.join("environ")).unwrap_or_default();
        let ours = String::from_utf8_lossy(&environ)
            .split('\0')
            .any(|v| v == want || same_dir(v.strip_prefix("RPI_CODING_AGENT_DIR="), dir));
        if ours {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Whether the process is gone or dead: a killed child can linger as a zombie until it is reaped.
fn dead(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    // `pid (comm) state ...`; the command name may itself hold a `)`.
    let state = stat.rsplit(')').next().unwrap_or_default();
    matches!(
        state.trim_start().chars().next(),
        Some('Z' | 'X' | 'x') | None
    )
}

/// Every regular file under `dir`.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => todo.push(path),
                // A symlink points out of the tree (the pool links the shared agent dir's files).
                Ok(t) if t.is_file() => found.push(path),
                _ => {}
            }
        }
    }
    found.sort();
    found
}

/// Whether `needle` appears anywhere in `hay`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A running gateway on `cfg` with one `chat` channel, and what it replied.
struct Gateway {
    feed: tokio::sync::mpsc::Sender<Inbound>,
    sent: Sent,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

impl Gateway {
    fn start(cfg: &Config) -> Gateway {
        let (feed, rx) = tokio::sync::mpsc::channel(4);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let sent: Sent = Default::default();
        let task = tokio::spawn({
            let cfg = cfg.clone();
            let sent = sent.clone();
            async move {
                zephyr::router::run(
                    &cfg,
                    &[Feed {
                        rx: tokio::sync::Mutex::new(rx),
                        sent,
                    }],
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
            }
        });
        Gateway {
            feed,
            sent,
            stop,
            task,
        }
    }

    async fn say(&self, text: &str, id: usize) {
        self.feed
            .send(Inbound {
                channel: "chat".into(),
                platform_msg_id: format!("m{id}"),
                sender: "owner".into(),
                text: text.into(),
            })
            .await
            .unwrap();
    }

    /// Every reply so far, once `n` have arrived.
    async fn replies(&self, n: usize) -> Vec<String> {
        for _ in 0..400 {
            if self.sent.lock().unwrap().len() >= n {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return self.sent.lock().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no reply: {:?}", self.sent.lock().unwrap());
    }

    /// Stops the daemon, and says why it exited.
    async fn stop(self) -> zephyr::router::Exit {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap()
    }
}

#[tokio::test]
async fn a_key_login_records_the_account_and_never_reports_the_key() {
    let (cfg, home) = setup("key", "");
    let start = zephyr::auth::start(&cfg, "opencode-go", None, None)
        .await
        .unwrap();
    assert_eq!(start.account, "opencode-go-1");
    // No `--model`: the first model rpi lists for the provider's prefix.
    assert_eq!(start.model, "opencode-go/alpha");
    assert_eq!(start.kind, zephyr::auth::Kind::Key);
    assert_eq!(
        start.to_json(),
        serde_json::json!({
            "loginId": start.login_id,
            "provider": "opencode-go",
            "account": "opencode-go-1",
            "kind": "key",
        })
    );
    // A key provider is ready for the token at once, and the prompt is passed on verbatim.
    let waiting = until(&start.login_id, |l| {
        l["phase"] == "need_input" && l["lines"].to_string().contains("Paste the opencode-go")
    })
    .await;
    assert_eq!(waiting["url"], Value::Null);

    assert!(zephyr::auth::input(&start.login_id, KEY));
    let done = until(&start.login_id, |l| l["phase"] == "done").await;
    assert_eq!(done["account"], "opencode-go-1");
    assert_eq!(done["model"], "opencode-go/alpha");
    assert_eq!(done["error"], Value::Null);
    // The pty echoes what is typed: it must not come back out.
    assert!(
        !done.to_string().contains(KEY),
        "the key came back in a poll: {done}"
    );

    // The credential went into the account's own agent dir, and the account into the pool.
    let auth: Value = serde_json::from_str(
        &std::fs::read_to_string(cfg.home.join("accounts/opencode-go-1/auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(auth["opencode-go"]["type"], "oauth");
    let toml = toml_at(&cfg);
    assert!(toml.contains("[[account]]"), "{toml}");
    assert!(toml.contains("name = \"opencode-go-1\""), "{toml}");
    assert!(toml.contains("model = \"opencode-go/alpha\""), "{toml}");

    // The dir holds a credential: owner-only.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(cfg.home.join("accounts/opencode-go-1"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    // The fake logged the login, and nothing logged the key.
    let log = std::fs::read_to_string(home.join("log.jsonl")).unwrap();
    assert!(log.contains("\"auth-login\""), "{log}");
    assert!(!log.contains(KEY));

    // Nothing on disk holds the key either: not `state.db` (opened here so the file exists, as
    // the daemon's own handle leaves it), not a session file, not `argv.json`, not the config.
    // The database crate is not in the tests' dev-dependencies, so there is no handle on the
    // tables; the scan is over the file's bytes and its WAL, which covers every page of every
    // table all the same.
    drop(zephyr::store::Store::open(&cfg.db_path()).unwrap());
    assert!(
        !waiting.to_string().contains(KEY),
        "the key came back before it was typed: {waiting}"
    );
    let own = cfg.home.join("accounts/opencode-go-1/auth.json");
    for file in files(&cfg.home) {
        if file == own {
            continue; // rpi's own credential store: the one place the key belongs
        }
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !contains(&bytes, KEY.as_bytes()),
            "the key reached {}",
            file.display()
        );
    }
}

#[tokio::test]
async fn a_login_that_finished_is_not_cancelled_by_a_late_cancel() {
    // The window is real: `pump` records the account and reports `done`, and a cancel that lands
    // just after must not turn that into "cancelled" — the row is in the config and the daemon may
    // already be running on it (DESIGN §4.7.1). A cancel that arrives in time is refused too, so
    // the answer never lies about an account that stays.
    let (cfg, _home) = setup("late-cancel", "");
    let start = zephyr::auth::start(&cfg, "opencode-go", None, None)
        .await
        .unwrap();
    until(&start.login_id, |l| l["phase"] == "need_input").await;
    assert!(zephyr::auth::input(&start.login_id, KEY));
    until(&start.login_id, |l| l["phase"] == "done").await;

    assert!(
        !zephyr::auth::cancel(&start.login_id),
        "a finished login cannot be cancelled"
    );
    let after = zephyr::auth::poll(&start.login_id).expect("a finished login stays readable");
    assert_eq!(after["phase"], "done", "{after}");
    assert_eq!(after["error"], Value::Null, "{after}");
    let toml = toml_at(&cfg);
    assert!(toml.contains("name = \"opencode-go-1\""), "{toml}");
}

#[tokio::test]
async fn an_oauth_login_reports_its_url_then_finishes_when_the_browser_returns() {
    let (cfg, home) = setup("oauth", "[[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n");
    std::fs::write(home.join("login_url"), "").unwrap();
    // No model given and rpi lists none for antigravity, before the sign-in or after it: the
    // login still starts — the URL has to reach the user — and fails at the end, asking for one.
    let none = zephyr::auth::start(&cfg, "antigravity", None, None)
        .await
        .unwrap();
    assert_eq!(none.model, "");
    std::fs::write(home.join("login_go"), "").unwrap();
    let failed = until(&none.login_id, |l| l["phase"] == "failed").await;
    assert!(
        failed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("rpi lists no antigravity model"),
        "{failed}"
    );
    assert!(
        !toml_at(&cfg).contains("antigravity-1"),
        "{}",
        toml_at(&cfg)
    );

    // Given one, the same sign-in records the account.
    std::fs::remove_file(home.join("login_go")).unwrap();
    let start = zephyr::auth::start(
        &cfg,
        "antigravity",
        None,
        Some("antigravity/gemini-3".into()),
    )
    .await
    .unwrap();
    assert_eq!(start.account, "antigravity-1");
    assert_eq!(start.kind, zephyr::auth::Kind::Oauth);

    let waiting = until(&start.login_id, |l| !l["url"].is_null()).await;
    assert_eq!(waiting["phase"], "waiting");
    assert_eq!(waiting["url"], "https://example.test/login/antigravity");

    assert!(!home.join("login_go").exists());
    std::fs::write(home.join("login_go"), "").unwrap();
    let done = until(&start.login_id, |l| l["phase"] == "done").await;
    assert_eq!(done["account"], "antigravity-1");
    let toml = toml_at(&cfg);
    assert!(toml.contains("name = \"antigravity-1\""), "{toml}");
    assert!(toml.contains("model = \"antigravity/gemini-3\""), "{toml}");
    // The account already there is untouched.
    assert!(toml.contains("name = \"a\""), "{toml}");
}

#[tokio::test]
async fn a_login_that_stores_nothing_fails_and_records_nothing() {
    let (cfg, home) = setup("fail", "");
    std::fs::write(home.join("login_fail"), "").unwrap();
    let start = zephyr::auth::start(&cfg, "anthropic", None, Some("anthropic/claude".into()))
        .await
        .unwrap();
    let failed = until(&start.login_id, |l| l["phase"] == "failed").await;
    let error = failed["error"].as_str().unwrap();
    assert!(error.contains("stored no credential"), "{error}");
    assert!(
        !toml_at(&cfg).contains("[[account]]"),
        "a failed login must not add an account"
    );
}

#[tokio::test]
async fn an_unknown_login_id_is_an_error_and_cancel_forgets_the_login() {
    assert!(zephyr::auth::poll("l99999").is_none());
    assert!(!zephyr::auth::input("l99999", "x"));
    assert!(!zephyr::auth::cancel("l99999"));

    let (cfg, home) = setup("cancel", "");
    std::fs::write(home.join("login_url"), "").unwrap();
    let start = zephyr::auth::start(
        &cfg,
        "openai-codex",
        None,
        Some("openai-codex/gpt-5".into()),
    )
    .await
    .unwrap();
    until(&start.login_id, |l| !l["url"].is_null()).await;
    assert!(zephyr::auth::logins().contains(&start.login_id));
    assert!(zephyr::auth::cancel(&start.login_id));
    assert!(zephyr::auth::poll(&start.login_id).is_none());
    assert!(!zephyr::auth::cancel(&start.login_id));
    // The killed child never recorded the account, and a login that finishes after the cancel is
    // not recorded late either: the account stays out of the config.
    std::fs::write(home.join("login_go"), "").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));
}

/// Cancelling a login that waits on stdin kills its `script` child (and the rpi under it), so the
/// pump never goes on to `rpi auth check`, and the account never reaches `zephyr.toml`. This is
/// the cancel-versus-record race: a child that outlived the cancel would still be able to store a
/// credential and be recorded.
#[tokio::test]
async fn cancelling_a_waiting_login_kills_its_child_and_records_nothing() {
    let (cfg, _home) = setup("kill", "");
    let start = zephyr::auth::start(&cfg, "opencode-go", None, None)
        .await
        .unwrap();
    let dir = cfg.home.join("accounts").join(&start.account);
    // The prompt is out: the rpi child is up, with the `script` wrapper over it.
    until(&start.login_id, |l| l["phase"] == "need_input").await;
    let rpi = cfg.rpi.display().to_string();
    wait_for(
        || {
            login_processes(&dir)
                .iter()
                .any(|p| cmdline_of(*p).contains(&rpi))
        },
        "the login's rpi child to run",
    )
    .await;
    let children = login_processes(&dir);
    assert!(
        children
            .iter()
            .any(|p| cmdline_of(*p).starts_with("script")),
        "a login runs under a pty: {children:?}"
    );

    assert!(zephyr::auth::cancel(&start.login_id));
    wait_for(
        || login_processes(&dir).is_empty(),
        "the cancelled login's processes to be gone",
    )
    .await;
    for p in &children {
        assert!(
            dead(*p),
            "process {p} outlived the cancel: {}",
            cmdline_of(*p)
        );
    }
    // The child died before rpi stored anything, and nothing was recorded.
    assert!(!dir.join("auth.json").exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));
}

/// A login restarts the child only when it has to (DESIGN §4.7.1, §4.10). A `/login` a chat refuses
/// writes nothing, and a login that appends an account only changes the pool: neither restarts the
/// child, and the new account stays inactive until something explicitly picks it. Signing the
/// account the child runs on in again is the one case that does: a newer `auth.json` restarts it on
/// the same session, once idle.
#[tokio::test]
async fn a_login_restarts_the_child_only_for_a_re_sign_in_of_the_running_account() {
    let (cfg, home) = setup(
        "restart",
        "[[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n",
    );
    let a = cfg.account_dir(&cfg.accounts[0]);
    let gw = Gateway::start(&cfg);
    // The pool's child comes up on the only account.
    wait_for(
        || starts(&home, &a).len() == 1,
        "the child to start on account a",
    )
    .await;
    let first = pid(&home);

    // A chat cannot type a key: `/login` for a key provider is refused and the login stopped, so
    // nothing is written for the account the child is already running on.
    gw.say("/login opencode-go a", 1).await;
    let got = gw.replies(1).await;
    assert!(got[0].contains("zephyr login opencode-go"), "{}", got[0]);

    // A login that appends an account adds it to the pool, and stops there.
    let start = zephyr::auth::start(&cfg, "opencode-go", None, None)
        .await
        .unwrap();
    assert_eq!(start.account, "opencode-go-1");
    assert_eq!(start.model, "opencode-go/alpha");
    until(&start.login_id, |l| l["phase"] == "need_input").await;
    assert!(zephyr::auth::input(&start.login_id, KEY));
    until(&start.login_id, |l| l["phase"] == "done").await;
    let b = cfg.home.join("accounts").join(&start.account);
    // The router reads the pool on its tick: once `/model` lists the new account the tick has
    // been, and the child is the same one, still on account a.
    let mut n = 1;
    loop {
        n += 1;
        gw.say("/model", n).await;
        let listed = gw.replies(n).await[n - 1].contains("account opencode-go-1:");
        if listed || n == 20 {
            assert!(listed, "the tick never read the pool");
            break;
        }
    }
    assert_eq!(
        starts(&home, &a).len(),
        1,
        "a login must not restart the running child: {:?}",
        starts(&home, &a)
    );
    assert!(
        starts(&home, &b).is_empty(),
        "a new account only starts once it is picked"
    );
    assert_eq!(pid(&home), first, "the child was restarted");
    assert!(
        !gw.task.is_finished(),
        "a login must never re-exec the daemon"
    );

    // It becomes active on an explicit `/model`, and only then.
    gw.say("/model opencode-go/alpha", n + 1).await;
    gw.replies(n + 1).await;
    wait_for(
        || !starts(&home, &b).is_empty(),
        "the child to start on opencode-go-1",
    )
    .await;
    assert_ne!(pid(&home), first);
    assert_eq!(starts(&home, &b)[0]["sessionId"], "main");

    // Signing that account in again is a re-login: a newer `auth.json` restarts the child on the
    // same session, once idle.
    let again = zephyr::auth::start(
        &cfg,
        "opencode-go",
        Some(start.account.clone()),
        Some("opencode-go/alpha".into()),
    )
    .await
    .unwrap();
    until(&again.login_id, |l| l["phase"] == "need_input").await;
    assert!(zephyr::auth::input(&again.login_id, KEY));
    until(&again.login_id, |l| l["phase"] == "done").await;
    wait_for(
        || starts(&home, &b).len() >= 2,
        "the re-login to restart the child",
    )
    .await;
    let restarts = starts(&home, &b);
    assert_eq!(restarts[1]["sessionId"], "main");
    assert_eq!(restarts[1]["model"], "opencode-go/alpha");
    assert_eq!(gw.stop().await, zephyr::router::Exit::Shutdown);
}

#[tokio::test]
async fn start_says_no_to_an_unknown_provider_and_a_config_with_no_file() {
    let (cfg, _) = setup("bad", "");
    let e = zephyr::auth::start(&cfg, "nope", None, None)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("unknown provider nope"), "{e:#}");
    assert!(format!("{e:#}").contains("opencode-go"), "{e:#}");

    let mut memory = cfg.clone();
    memory.path = None;
    let e = zephyr::auth::start(&memory, "opencode-go", None, Some("x/y".into()))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("no zephyr.toml"), "{e:#}");
}

/// `/login` from a channel, through the router: the reply carries the URL, `zephyr.toml` gains
/// the account, and no reply ever carries a credential.
#[tokio::test]
async fn login_from_a_channel() {
    let (cfg, home) = setup(
        "channel",
        "[[account]]\nname = \"a\"\nmodel = \"fake/m1\"\n",
    );
    let path = cfg.path.clone().unwrap();
    let (feed, rx) = tokio::sync::mpsc::channel(4);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let sent: Sent = Default::default();
    let task = tokio::spawn({
        let sent = sent.clone();
        async move {
            zephyr::router::run(
                &cfg,
                &[Feed {
                    rx: tokio::sync::Mutex::new(rx),
                    sent,
                }],
                async {
                    let _ = stopped.await;
                },
            )
            .await
        }
    });
    let say = |feed: tokio::sync::mpsc::Sender<Inbound>, text: &'static str, id: usize| async move {
        feed.send(Inbound {
            channel: "chat".into(),
            platform_msg_id: format!("m{id}"),
            sender: "owner".into(),
            text: text.into(),
        })
        .await
        .unwrap();
    };
    async fn wait_replies(sent: &Sent, n: usize) -> Vec<String> {
        for _ in 0..400 {
            if sent.lock().unwrap().len() >= n {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return sent.lock().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no reply: {:?}", sent.lock().unwrap());
    }

    // Bare `/login`: the provider ids, for every channel.
    say(feed.clone(), "/login", 1).await;
    let got = wait_replies(&sent, 1).await;
    assert!(got[0].contains("opencode-go"), "{}", got[0]);
    assert!(got[0].contains("anthropic"), "{}", got[0]);

    // A browser sign-in: the reply says what was started, and the URL follows as its own notice
    // on the router's tick — the one loop that reads the child never waits on a login. That
    // notice goes to the attach bus (`zephyr_reply`, §4.3.1), so here the login itself is asked.
    std::fs::write(home.join("login_url"), "").unwrap();
    say(
        feed.clone(),
        "/login antigravity antigravity-1 antigravity/gemini-3",
        2,
    )
    .await;
    let got = wait_replies(&sent, 2).await;
    let reply = &got[1];
    assert!(reply.contains("the URL follows here"), "{reply}");
    assert!(reply.contains("antigravity-1"), "{reply}");
    assert!(
        !reply.contains("https://example.test/login/antigravity"),
        "the URL is not in the reply: {reply}"
    );
    let id = zephyr::auth::logins()
        .into_iter()
        .find(|id| zephyr::auth::poll(id).is_some_and(|l| l["account"] == "antigravity-1"))
        .expect("the channel's login is in the registry");
    let sign_in = until(&id, |l| !l["url"].is_null()).await;
    assert_eq!(sign_in["url"], "https://example.test/login/antigravity");

    // The browser comes back: the account joins the pool with no restart, and no prompt.
    std::fs::write(home.join("login_go"), "").unwrap();
    for _ in 0..400 {
        if std::fs::read_to_string(&path)
            .unwrap()
            .contains("name = \"antigravity-1\"")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let toml = std::fs::read_to_string(&path).unwrap();
    assert!(toml.contains("name = \"antigravity-1\""), "{toml}");
    assert!(toml.contains("model = \"antigravity/gemini-3\""), "{toml}");

    // A key provider is signed in on the host, not in a chat: the reply says so and holds no
    // credential, and no account is added.
    // Found by content: the sign-in's own notice, posted on a tick, may come before it.
    say(feed.clone(), "/login opencode-go", 3).await;
    let host = |x: &String| x.contains("zephyr login opencode-go");
    let got = replies_until(&sent, |r| r.iter().any(host), "the key provider's reply").await;
    let refused = got.iter().find(|x| host(x)).unwrap();
    assert!(!refused.contains(KEY), "{refused}");
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("opencode-go"));

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

/// DESIGN §4.7.1: a sign-in that ends without a `[[account]]` row also drops the directory it
/// made. rpi writes `auth.json` into that directory *before* this process decides whether the
/// attempt counts, so a directory left behind would make `rpi auth check` pass for that name
/// later — the next `/login` takes a fresh name, and a name given explicitly would be "signed in"
/// without anyone signing in.
#[tokio::test]
async fn a_failed_login_leaves_no_account_directory() {
    let (cfg, home) = setup("nodir", "");
    // rpi fails on its own: the child exits before it stores anything.
    std::fs::write(home.join("login_fail"), "").unwrap();
    let start = zephyr::auth::start(&cfg, "anthropic", None, Some("anthropic/claude-x".into()))
        .await
        .unwrap();
    let dir = cfg.home.join("accounts").join(&start.account);
    let failed = until(&start.login_id, |l| l["phase"] == "failed").await;
    assert!(
        failed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stored no credential"),
        "{failed}"
    );
    // `fail` reports the phase and then drops the directory: both, and no row either way.
    wait_for(
        || !dir.exists(),
        "the failed sign-in's account directory to go",
    )
    .await;
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));

    // The same when rpi did store a credential and the sign-in fails after that: the directory it
    // wrote into goes with it, so a later `rpi auth check` for that name cannot pass.
    std::fs::remove_file(home.join("login_fail")).unwrap();
    std::fs::write(home.join("login_url"), "").unwrap();
    let none = zephyr::auth::start(&cfg, "antigravity", None, None)
        .await
        .unwrap();
    let dir = cfg.home.join("accounts").join(&none.account);
    until(&none.login_id, |l| !l["url"].is_null()).await;
    assert!(dir.is_dir(), "{} ran without an account dir", none.account);
    // fake-rpi.py stores `auth.json` once the browser comes back, then lists no antigravity
    // model, which is the failure DESIGN §4.7.1 is about.
    std::fs::write(home.join("login_go"), "").unwrap();
    let failed = until(&none.login_id, |l| l["phase"] == "failed").await;
    assert!(
        failed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("lists no antigravity model"),
        "{failed}"
    );
    wait_for(|| !dir.exists(), "the stored-then-failed directory to go").await;
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));
}

/// Two `/login antigravity` at once (or the TUI and a chat) must not resolve the same account
/// name: nothing on disk holds a generated name until its directory exists, so both would pick
/// `antigravity-1`, write one `auth.json`, and a cancel of one would discard the directory under
/// the other's live child. `auth` reserves the name before the model listing.
#[tokio::test]
async fn two_logins_at_once_do_not_share_an_account_name() {
    let (cfg, home) = setup("names", "");
    // Both children print a URL and wait for the browser, so both stay live until they are
    // cancelled here.
    std::fs::write(home.join("login_url"), "").unwrap();
    let (a, b) = tokio::join!(
        zephyr::auth::start(
            &cfg,
            "antigravity",
            None,
            Some("antigravity/gemini-3".into())
        ),
        zephyr::auth::start(
            &cfg,
            "antigravity",
            None,
            Some("antigravity/gemini-3".into())
        ),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.account, b.account, "two sign-ins took one name");
    let mut names = [a.account.clone(), b.account.clone()];
    names.sort();
    assert_eq!(names, ["antigravity-1", "antigravity-2"]);
    for start in [&a, &b] {
        until(&start.login_id, |l| !l["url"].is_null()).await;
        let dir = cfg.home.join("accounts").join(&start.account);
        assert!(dir.is_dir(), "{} has no account dir", start.account);
    }
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));

    // Both are cancelled, and each sign-in drops the directory it made.
    for start in [&a, &b] {
        assert!(zephyr::auth::cancel(&start.login_id), "{}", start.account);
        let dir = cfg.home.join("accounts").join(&start.account);
        wait_for(|| !dir.exists(), "the cancelled sign-in's directory to go").await;
    }
}

/// Every reply `Feed` has taken, once `done` holds for them: the content, not the count, so a
/// notice a start posts on the tick cannot shift what a test is looking at.
async fn replies_until(
    sent: &Sent,
    mut done: impl FnMut(&[String]) -> bool,
    what: &str,
) -> Vec<String> {
    for _ in 0..400 {
        {
            let replies = sent.lock().unwrap();
            if done(&replies) {
                return replies.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}: {:?}", sent.lock().unwrap());
}

/// The router's `PENDING_LOGINS` cap: at most eight `/login` sign-ins wait for their URL at once,
/// and a ninth is refused rather than dropping a live one — a sign-in whose URL was never posted
/// can never be finished, so `/login cancel PROVIDER` is the way back out.
///
/// Nothing here writes `login_url`: each child waits at the paste prompt and never prints a URL,
/// so all eight stay in the pending set the cap counts (a login whose URL went out is no longer
/// waiting for one) for as long as the test needs them. The one thing this does not reach is a
/// pending sign-in that is also *serving* a URL: the fake prints one URL for the whole file, so
/// eight with URLs would mean eight markers, and the cap only counts those still waiting.
#[tokio::test]
async fn the_eight_pending_cap_refuses_a_ninth_login() {
    let (cfg, _home) = setup("cap", "");
    let gw = Gateway::start(&cfg);
    // A `/login` the router accepted, as opposed to one it refused (and as opposed to a start's
    // own failure, which arrives later as a notice).
    fn accepted(reply: &str) -> bool {
        reply.contains("the URL follows here")
    }
    for n in 1..=8 {
        gw.say("/login antigravity", n).await;
    }
    let got = replies_until(
        &gw.sent,
        |r| r.iter().filter(|x| accepted(x)).count() >= 8,
        "eight accepted sign-ins",
    )
    .await;
    assert_eq!(got.iter().filter(|x| accepted(x)).count(), 8, "{got:?}");

    // The ninth is refused, and the refusal says what to do about it.
    gw.say("/login antigravity", 9).await;
    let got = replies_until(
        &gw.sent,
        |r| {
            r.iter()
                .any(|x| x.contains("sign-ins are waiting for their URL"))
        },
        "the cap's refusal",
    )
    .await;
    let refusal = got
        .iter()
        .find(|x| x.contains("sign-ins are waiting for their URL"))
        .unwrap();
    assert!(refusal.contains("8 sign-ins are waiting"), "{refusal}");
    assert!(
        refusal.contains("finish one or `/login cancel`"),
        "{refusal}"
    );

    // Cancelling the provider frees the eight it held, so a `/login` is accepted again.
    gw.say("/login cancel antigravity", 10).await;
    let got = replies_until(
        &gw.sent,
        |r| {
            r.iter()
                .filter(|x| x.contains("cancelled the sign-in to antigravity"))
                .count()
                >= 1
        },
        "the cancel of the eight",
    )
    .await;
    assert!(
        got.iter()
            .any(|x| x.contains("cancelled the sign-in to antigravity")),
        "{got:?}"
    );
    gw.say("/login antigravity", 11).await;
    replies_until(
        &gw.sent,
        |r| r.iter().filter(|x| accepted(x)).count() >= 9,
        "a sign-in accepted again after the cancel",
    )
    .await;

    // The ninth start resolves on a later tick; then a second `/login cancel antigravity` stops
    // it too, so nothing this test started is left running. Cancelled by provider, never with a
    // bare `/login cancel`: that one stops every login in the process (`router.rs` `login_cancel`
    // reads `auth::logins`, which a second daemon in this test binary shares).
    replies_until(
        &gw.sent,
        |r| {
            after(r, "cancelled the sign-in to antigravity")
                .iter()
                .any(|x| x.contains("signing in to antigravity as "))
        },
        "the sign-in started after the cancel to resolve",
    )
    .await;
    gw.say("/login cancel antigravity", 12).await;
    replies_until(
        &gw.sent,
        |r| {
            r.iter()
                .filter(|x| x.contains("cancelled the sign-in to antigravity"))
                .count()
                >= 2
        },
        "the last sign-in to be cancelled",
    )
    .await;
    assert_eq!(gw.stop().await, zephyr::router::Exit::Shutdown);
}

/// The replies posted after the first one containing `needle`.
fn after<'a>(replies: &'a [String], needle: &str) -> &'a [String] {
    match replies.iter().position(|r| r.contains(needle)) {
        Some(i) => &replies[i + 1..],
        None => &[],
    }
}

/// The model a `/login` names must be one the provider being signed in can serve: a credential
/// for `antigravity` cannot serve `anthropic/claude-x`, whether the model comes from the command
/// or from the account's existing row.
#[tokio::test]
async fn an_explicit_model_from_another_provider_is_refused() {
    let (cfg, _home) = setup("model-provider", "");
    let gw = Gateway::start(&cfg);
    // antigravity is a browser sign-in, so the command gets past the check a chat makes for a key
    // provider and reaches the model rule inside `auth::start`. A refusal there is the start's
    // notice, posted on the router's tick.
    gw.say("/login antigravity new anthropic/claude-x", 1).await;
    let got = gw.replies(1).await;
    assert!(
        got[0].contains("signing in to antigravity as new"),
        "{}",
        got[0]
    );
    let got = replies_until(
        &gw.sent,
        |r| r.iter().any(|x| x.contains("is not a antigravity model")),
        "the model's refusal",
    )
    .await;
    let refusal = got
        .iter()
        .find(|x| x.contains("is not a antigravity model"))
        .unwrap();
    assert!(refusal.contains("anthropic/claude-x"), "{refusal}");
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));
    assert!(!cfg.home.join("accounts/new").exists());

    // The same command with a key provider never reaches the model rule from a chat: a chat
    // refuses a key provider first (that has its own test above). Where the host's own
    // `zephyr login` does reach it, the rule holds there too — and no account is recorded either
    // way.
    gw.say("/login opencode-go new anthropic/claude-x", 2).await;
    let got = gw.replies(2).await;
    assert!(
        got.iter().any(|r| r.contains("zephyr login opencode-go")),
        "{got:?}"
    );
    let err = zephyr::auth::start(
        &cfg,
        "opencode-go",
        Some("new".into()),
        Some("anthropic/claude-x".into()),
    )
    .await
    .expect_err("a cross-provider model must be refused");
    assert!(
        format!("{err:#}").contains("is not a opencode-go model"),
        "{err:#}"
    );
    assert!(!toml_at(&cfg).contains("[[account]]"), "{}", toml_at(&cfg));
    assert!(!cfg.home.join("accounts/new").exists());
    assert_eq!(gw.stop().await, zephyr::router::Exit::Shutdown);
}
