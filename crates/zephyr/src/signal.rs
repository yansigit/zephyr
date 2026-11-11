//! Signal channel adapter: a `signal-cli jsonRpc` child of the daemon's, on a number of zephyr's
//! own. See DESIGN.md section 4.3.

use crate::channel::{chunks, Activity, Channel, Inbound, Outbound};
use anyhow::Context as _;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// `[signal]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Zephyr's own number, never the owner's: `+821012345678`. Not a secret.
    pub account: String,
}

/// Whether the daemon's signal-cli is up and holds `[signal] account` (the adapter's `.ok`).
pub fn holds(cfg: &crate::config::Config) -> bool {
    let Some(signal) = &cfg.signal else {
        return false;
    };
    std::fs::read_to_string(data_dir(cfg).join(OK_FILE)).is_ok_and(|a| a == signal.account)
}

/// signal-cli's data dir: the account's keys, password and sessions. Root's, 0700, in `<state>`
/// and so out of the agent's reach (DESIGN §4.4); not in the daily backup.
pub fn data_dir(cfg: &crate::config::Config) -> PathBuf {
    cfg.state_dir().join("signal")
}

/// Signal's text limit is larger; 2000 UTF-16 units keeps a reply readable on a phone.
pub const MAX_TEXT: usize = 2000;
/// A restart waits this long, doubling up to `BACKOFF_MAX`.
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// signal-cli's JVM takes seconds to start, and a send waits for the server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Signal's typing indicator lapses after 15 s.
const TYPING_EVERY: Duration = Duration::from_secs(10);
/// How often `D/.trust` (dropped by `zephyr signal-setup trust`) is looked for.
const TRUST_EVERY: Duration = Duration::from_secs(2);
/// Names remembered for `known_as`.
const NAMES: usize = 256;
/// Root's PATH for signal-cli: nothing the agent can write.
const ROOT_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// Written in the data dir while the child is up and holds `account`; doctor reads it.
const OK_FILE: &str = ".ok";
/// Written by `zephyr signal-setup` while it registers or links, holding its pid: the daemon's
/// signal-cli stops and lets the account go meanwhile.
const HOLD_FILE: &str = ".hold";
/// A hold not refreshed for this long is a setup that is gone or stuck: it holds nothing.
const HOLD_LEASE: Duration = Duration::from_secs(90);
const HOLD_REFRESH: Duration = Duration::from_secs(30);
/// How long setup waits for the SMS or call code.
const CODE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The most a setup step's signal-cli may take; `link` waits for a scan, so it gets 10 min.
const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
const LINK_TIMEOUT: Duration = Duration::from_secs(600);
const TRUST_FILE: &str = ".trust";

/// signal-cli by full path, its environment cleared to what the JVM needs, on `dir`. Its
/// temporary files go in `dir/tmp` (root's, 0700), never `/tmp`, which the agent can write to,
/// and the JVM keeps no `hsperfdata` file at all.
fn command(cli: &Path, dir: &Path) -> tokio::process::Command {
    let jre = cli
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"))
        .join("jre");
    let tmp = dir.join("tmp");
    if let Err(e) = private_dir(&tmp) {
        eprintln!("zephyr: signal: making {}: {e}", tmp.display());
    }
    let opts = format!(
        "-Xmx256m -XX:+UseSerialGC -XX:-UsePerfData -Djava.io.tmpdir={}",
        tmp.display()
    );
    let mut cmd = tokio::process::Command::new(cli);
    cmd.env_clear()
        .env("PATH", ROOT_PATH)
        .env("JAVA_HOME", jre)
        .env("JAVA_TOOL_OPTIONS", opts)
        .arg("--data-dir")
        .arg(dir)
        .kill_on_drop(true);
    cmd
}

/// Creates `dir` as the daemon's (root's in the container), 0700, and makes an existing one 0700.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// A Signal ACI: a UUID, `8-4-4-4-12` hex digits.
pub fn is_aci(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// An attachment id signal-cli gave a file in `D/attachments`: a plain file name, nothing that
/// could lead out of that folder.
fn plain(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// The row for `env`, with its attachments: only a direct `dataMessage` from another account.
/// Sync messages (all a device linked to the owner's own account would hear), edits, typing,
/// receipts, stories, reactions, remote deletes and groups are none.
fn to_inbound(env: &Value, account: &str, own: Option<&str>) -> Option<(Inbound, Vec<Value>)> {
    let others = [
        "syncMessage",
        "editMessage",
        "typingMessage",
        "receiptMessage",
        "storyMessage",
        "callMessage",
    ];
    if others.iter().any(|k| !env[k].is_null()) {
        return None;
    }
    let dm = env["dataMessage"].as_object()?;
    let skip = ["groupInfo", "groupV2", "reaction", "remoteDelete"];
    if skip
        .iter()
        .any(|k| dm.get(*k).is_some_and(|v| !v.is_null()))
    {
        return None;
    }
    let from = env["sourceUuid"].as_str().filter(|u| is_aci(u))?;
    if own == Some(from) || env["sourceNumber"].as_str() == Some(account) {
        return None;
    }
    let ts = env["timestamp"].as_u64()?;
    let text = dm.get("message").and_then(Value::as_str).unwrap_or("");
    let files = dm
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if text.is_empty() && files.is_empty() {
        return None;
    }
    let row = Inbound {
        channel: "signal".into(),
        platform_msg_id: format!("{from}-{ts}"),
        sender: from.into(),
        text: text.into(),
    };
    Some((row, files))
}

/// Every attachment id in `v` (a data message's, a sync's, a quote's thumbnail, a preview's
/// image): signal-cli saved each one, whoever sent it.
fn attachment_ids(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            if let (Some(_), Some(Value::String(id))) = (o.get("contentType"), o.get("id")) {
                out.push(id.clone());
            }
            o.values().for_each(|v| attachment_ids(v, out));
        }
        Value::Array(a) => a.iter().for_each(|v| attachment_ids(v, out)),
        _ => {}
    }
}

/// `"Aiden", +821012345678`: how a sender named themselves, never an identity.
fn named(env: &Value) -> Option<String> {
    let name = env["sourceName"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(|n| format!("{n:?}"));
    let number = env["sourceNumber"].as_str().map(str::to_string);
    let both: Vec<String> = name.into_iter().chain(number).collect();
    (!both.is_empty()).then(|| both.join(", "))
}

/// The child's JSON-RPC: requests go to its stdin through `writer`, answers come back by id.
#[derive(Default)]
struct Rpc {
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    next: AtomicU64,
}

impl Rpc {
    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = format!("z{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let sent = (self.writer.lock().unwrap().as_ref())
            .is_some_and(|w| w.send(format!("{line}\n")).is_ok());
        let answer = match sent {
            false => Err(anyhow::anyhow!("signal-cli is not running")),
            true => match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
                Ok(Ok(Ok(v))) => Ok(v),
                Ok(Ok(Err(e))) => Err(anyhow::anyhow!("signal-cli {method}: {e}")),
                Ok(Err(_)) => Err(anyhow::anyhow!("signal-cli exited during {method}")),
                Err(_) => Err(anyhow::anyhow!("signal-cli did not answer {method}")),
            },
        };
        self.pending.lock().unwrap().remove(&id);
        answer
    }

    /// A response line from the child.
    fn answer(&self, v: &Value) {
        let Some(id) = v["id"].as_str() else { return };
        let Some(tx) = self.pending.lock().unwrap().remove(id) else {
            return;
        };
        let r = match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
            None => Ok(v["result"].clone()),
        };
        let _ = tx.send(r);
    }
}

/// While a child runs; dropped when it exits (or `run` is dropped): requests fail at once and
/// doctor no longer reads the account as up.
struct Up<'a>(&'a Signal);

impl Drop for Up<'_> {
    fn drop(&mut self) {
        *self.0.rpc.writer.lock().unwrap() = None;
        self.0.rpc.pending.lock().unwrap().clear();
        let _ = std::fs::remove_file(self.0.dir.join(OK_FILE));
    }
}

pub struct Signal {
    cli: PathBuf,
    account: String,
    /// signal-cli's data dir, `data_dir`.
    dir: PathBuf,
    rpc: Arc<Rpc>,
    live: crate::inbox::Live,
    /// `named` per sender, for `known_as`.
    names: Mutex<HashMap<String, String>>,
    /// The chunks of each reply signal-cli sent: Signal has no nonce to drop a duplicate.
    delivered: Arc<crate::channel::Delivered>,
    /// ACIs whose changed safety number the owners were told of in this daemon run, or are
    /// being told of now: claimed before the write, so two failed sends tell once.
    told: Mutex<HashSet<String>>,
    /// For an ACI whose notice reached only some owners: who, so a retry tells only the rest.
    reached: Mutex<HashMap<String, HashSet<(String, String)>>>,
    /// The typing refresher per recipient.
    typing: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

impl Signal {
    pub fn new(cfg: &crate::config::Config) -> anyhow::Result<Self> {
        let account = cfg
            .signal
            .as_ref()
            .context("[signal] is not set")?
            .account
            .clone();
        Ok(Self {
            cli: cfg.signal_cli.clone(),
            account,
            dir: data_dir(cfg),
            rpc: Arc::default(),
            live: Default::default(),
            names: Mutex::default(),
            delivered: Arc::default(),
            told: Mutex::default(),
            reached: Mutex::default(),
            typing: Mutex::default(),
        })
    }

    /// One child, from its start to its exit.
    async fn child(&self, tx: &mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        private_dir(&self.dir)?;
        let _ = std::fs::remove_file(self.dir.join(OK_FILE));
        let own = self.own_aci();
        let mut child = command(&self.cli, &self.dir)
            .args(["-a", &self.account, "jsonRpc"])
            .args(["--ignore-stories", "--ignore-avatars", "--ignore-stickers"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", self.cli.display()))?;
        let (mut stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
        *self.rpc.writer.lock().unwrap() = Some(lines_tx);
        let _up = Up(self);
        let writer = async {
            while let Some(line) = lines.recv().await {
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
            anyhow::Ok(())
        };
        // Envelopes are taken one at a time, in order, while answers keep coming.
        let (env_tx, mut envs) = mpsc::unbounded_channel::<Value>();
        let reader = async move {
            let mut out = BufReader::new(stdout).lines();
            while let Some(line) = out.next_line().await? {
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) if v["method"] == "receive" => {
                        let _ = env_tx.send(v["params"]["envelope"].clone());
                    }
                    Ok(v) if v.get("id").is_some() => self.rpc.answer(&v),
                    Ok(_) => {}
                    Err(_) => eprintln!("zephyr: signal-cli wrote a line that is not JSON"),
                }
            }
            anyhow::Ok(())
        };
        let taker = async {
            while let Some(env) = envs.recv().await {
                self.take(env, own.as_deref(), tx).await;
            }
        };
        let side = async {
            self.started().await;
            let mut every = tokio::time::interval(TRUST_EVERY);
            // Until a setup holds the account: then this signal-cli goes.
            while !held(&self.dir) {
                every.tick().await;
                self.trust().await;
            }
        };
        tokio::select! {
            (r, ()) = async { tokio::join!(reader, taker) } => r?,
            r = writer => r?,
            () = side => {
                eprintln!("zephyr: signal-cli stops while zephyr signal-setup holds the account");
                let _ = child.start_kill();
            }
        }
        let status = child.wait().await?;
        eprintln!("zephyr: signal-cli exited ({status})");
        Ok(())
    }

    /// Zephyr's own ACI, from signal-cli's record of the accounts it holds.
    fn own_aci(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join("data/accounts.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        (v["accounts"].as_array()?.iter())
            .find(|a| a["number"] == self.account.as_str())
            .and_then(|a| a["uuid"].as_str().map(str::to_string))
    }

    /// `listAccounts` at each start: `D/.ok` names the account once signal-cli holds it.
    async fn started(&self) {
        match self.rpc.call("listAccounts", json!({})).await {
            Ok(list)
                if (list.as_array().into_iter().flatten())
                    .any(|a| a["number"] == self.account.as_str()) =>
            {
                let ok = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(self.dir.join(OK_FILE))
                    .and_then(|mut f| std::io::Write::write_all(&mut f, self.account.as_bytes()));
                if let Err(e) = ok {
                    eprintln!("zephyr: signal: writing {OK_FILE}: {e}");
                }
            }
            Ok(_) => eprintln!(
                "zephyr: signal-cli does not hold {}; register it with zephyr channel add signal",
                self.account
            ),
            Err(e) => eprintln!("zephyr: {e:#}"),
        }
    }

    /// An envelope from the child: a row for the router, or nothing. Every file signal-cli saved
    /// for it is deleted, a stranger's never read.
    async fn take(&self, env: Value, own: Option<&str>, tx: &mpsc::Sender<Inbound>) {
        let mut ids = Vec::new();
        attachment_ids(&env, &mut ids);
        let row = to_inbound(&env, &self.account, own);
        let mut taken = None;
        if let Some((mut row, files)) = row {
            if let Some(name) = named(&env) {
                let mut names = self.names.lock().unwrap();
                // shortcut: forgets every name when full, enough for a pairing notice; an LRU if names matter more.
                if names.len() >= NAMES && !names.contains_key(&row.sender) {
                    names.clear();
                }
                names.insert(row.sender.clone(), name);
            }
            let files = files.iter().map(|a| {
                let id = a["id"].as_str().unwrap_or_default();
                crate::inbox::File {
                    name: a["filename"].as_str().unwrap_or(id).to_string(),
                    mime: a["contentType"].as_str().map(str::to_string),
                    size: a["size"].as_u64().unwrap_or(0),
                    src: match plain(id) {
                        true => crate::inbox::Src::Path(self.dir.join("attachments").join(id)),
                        false => crate::inbox::Src::Skip("not downloaded"),
                    },
                }
            });
            crate::inbox::append(&self.live, &mut row, files.collect()).await;
            taken = Some(row);
        }
        for id in ids.iter().filter(|id| plain(id)) {
            let _ = std::fs::remove_file(self.dir.join("attachments").join(id));
        }
        if let Some(row) = taken {
            // A closed receiver means run() is returning.
            let _ = tx.send(row).await;
        }
    }

    /// Takes `D/.trust` (one ACI, from `zephyr signal-setup trust`) and trusts that ACI's keys.
    async fn trust(&self) {
        let file = self.dir.join(TRUST_FILE);
        let Ok(text) = std::fs::read_to_string(&file) else {
            return;
        };
        let _ = std::fs::remove_file(&file);
        let aci = text.trim();
        if !is_aci(aci) {
            eprintln!("zephyr: signal: {TRUST_FILE} does not hold an ACI; ignored");
            return;
        }
        let params = json!({"recipient": aci, "trustAllKnownKeys": true});
        match self.rpc.call("trust", params).await {
            Ok(_) => eprintln!("zephyr: signal: trusting the new safety number of {aci}"),
            Err(e) => eprintln!("zephyr: {e:#}"),
        }
    }

    /// A send to `aci` failed on a changed identity key: the owner is told once per daemon run.
    /// It counts as told only once the notice is stored, so a failed write is tried again at the
    /// next failed send; the write is a blocking task's, off the send.
    async fn changed_identity(&self, aci: &str) {
        // Claimed before the write: a second failed send meanwhile tells nobody again.
        if !self.told.lock().unwrap().insert(aci.to_string()) {
            return;
        }
        let Some(cfg) = self.live.get().map(|rx| rx.borrow().clone()) else {
            self.told.lock().unwrap().remove(aci);
            return;
        };
        let text = format!(
            "[zephyr] the Signal safety number of {aci} changed; compare it, then run zephyr channel signal trust {aci}"
        );
        let mut reached = (self.reached.lock().unwrap().get(aci).cloned()).unwrap_or_default();
        let who = aci.to_string();
        let told = tokio::task::spawn_blocking(move || {
            let store = crate::store::Store::open(&cfg.db_path())?;
            let mut all = true;
            for target in crate::router::owner_targets(&cfg, Some(("signal", &who))) {
                if reached.contains(&target) {
                    continue;
                }
                match store.notify(&target.0, &target.1, &text) {
                    Ok(()) => drop(reached.insert(target)),
                    Err(e) => {
                        eprintln!("zephyr: signal: telling {} on {}: {e}", target.1, target.0);
                        all = false;
                    }
                }
            }
            anyhow::Ok((reached, all))
        })
        .await;
        match told {
            // Every owner told: the claim stays, for this daemon run.
            Ok(Ok((_, true))) => drop(self.reached.lock().unwrap().remove(aci)),
            // Some: they are recorded, and the next failed send tells only the rest.
            Ok(Ok((reached, false))) => {
                self.reached
                    .lock()
                    .unwrap()
                    .insert(aci.to_string(), reached);
                self.told.lock().unwrap().remove(aci);
            }
            Ok(Err(e)) => {
                eprintln!("zephyr: signal: telling the owner of {aci}'s safety number: {e:#}");
                self.told.lock().unwrap().remove(aci);
            }
            Err(e) => {
                eprintln!("zephyr: signal: telling the owner of {aci}'s safety number: {e}");
                self.told.lock().unwrap().remove(aci);
            }
        }
    }
}

impl Channel for Signal {
    fn name(&self) -> &str {
        "signal"
    }

    /// Never fails for the child: one that exits is started again, 5 s later doubling to 5 min.
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let mut backoff = BACKOFF_MIN;
        loop {
            // Before every start, the first after a daemon start or re-exec too.
            while held(&self.dir) {
                tokio::select! {
                    _ = tokio::time::sleep(TRUST_EVERY) => {},
                    _ = tx.closed() => return Ok(()),
                }
            }
            let began = Instant::now();
            tokio::select! {
                r = self.child(&tx) => if let Err(e) = r {
                    eprintln!("zephyr: signal-cli: {e:#}");
                },
                _ = tx.closed() => return Ok(()),
            }
            if began.elapsed() > BACKOFF_MAX {
                backoff = BACKOFF_MIN;
            }
            eprintln!(
                "zephyr: signal-cli is down; starting it again in {} s",
                backoff.as_secs()
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        anyhow::ensure!(is_aci(&msg.recipient), "recipient is not a Signal ACI");
        // Setup holds the account: the reply waits for it, as long as its lease runs.
        if held(&self.dir) {
            return Err(crate::channel::Paused.into());
        }
        let mut parts = chunks(&msg.text, MAX_TEXT);
        if parts.is_empty() && msg.attachment.is_some() {
            parts.push(String::new());
        }
        let last = parts.len().saturating_sub(1);
        let mut sending = (self.delivered).begin("signal", &msg.recipient, &msg.client_id)?;
        sending.total = parts.len();
        // In a task of its own: the router stops waiting after its timeout, but signal-cli sends
        // what it was handed (up to its 60 s), and that is recorded, so no retry sends it again.
        let (rpc, to, file) = (
            self.rpc.clone(),
            msg.recipient.clone(),
            msg.attachment.clone(),
        );
        let send = move |sending: crate::channel::Sending| async move {
            for (i, text) in parts.into_iter().enumerate().skip(sending.before) {
                let mut params = json!({"recipient": [to], "message": text});
                if let (true, Some(file)) = (i == last, &file) {
                    // Root's copy (`<state>/files/<id>/<name>`), which signal-cli reads as root.
                    params["attachments"] = json!([file]);
                }
                let r = rpc.call("send", params).await?;
                let results = r["results"].as_array().cloned().unwrap_or_default();
                anyhow::ensure!(!results.is_empty(), "signal-cli send gave no result");
                for kind in results.iter().map(|r| r["type"].as_str().unwrap_or("?")) {
                    if kind == "IDENTITY_FAILURE" {
                        return Ok(false);
                    }
                    anyhow::ensure!(kind == "SUCCESS", "signal send failed: {kind}");
                }
                sending.sent(i + 1);
            }
            Ok(true)
        };
        if !crate::channel::detach(sending, crate::channel::SEND_CAP, send).await? {
            self.changed_identity(&msg.recipient).await;
            anyhow::bail!("signal send failed: IDENTITY_FAILURE");
        }
        Ok(())
    }

    /// Typing every 10 s while a run goes; no progress note (shortcut: Signal can edit a message, so a note is possible later).
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        if !is_aci(to) {
            return Ok(());
        }
        match a {
            Activity::Start => {
                let mut typing = self.typing.lock().unwrap();
                if typing.contains_key(to) {
                    return Ok(());
                }
                let (rpc, recipient) = (self.rpc.clone(), json!([to]));
                let task = tokio::spawn(async move {
                    loop {
                        let _ = rpc
                            .call("sendTyping", json!({"recipient": recipient}))
                            .await;
                        tokio::time::sleep(TYPING_EVERY).await;
                    }
                });
                typing.insert(to.to_string(), task.abort_handle());
            }
            Activity::Progress { .. } => {}
            Activity::Stop => {
                let Some(task) = self.typing.lock().unwrap().remove(to) else {
                    return Ok(());
                };
                task.abort();
                let params = json!({"recipient": [to], "stop": true});
                let rpc = self.rpc.clone();
                tokio::spawn(async move { rpc.call("sendTyping", params).await });
            }
        }
        Ok(())
    }

    fn live(&self, cfg: tokio::sync::watch::Receiver<Arc<crate::config::Config>>) {
        let _ = self.live.set(cfg);
    }

    fn known_as(&self, sender: &str) -> Option<String> {
        self.names.lock().unwrap().get(sender).cloned()
    }
}

const WARNING: &str = "This must be a number zephyr will own, never your own Signal account: a device linked to yours receives every conversation you have, and its keys speak as you to all of your contacts.";
const USAGE: &str =
    "usage: zephyr signal-setup register NUMBER [--voice] | link | trust ACI | remove [ACCOUNT]";

/// signal-cli as root on the data dir, on the terminal: setup's runs.
/// signal-cli's multi-account JSON-RPC on the data dir, for the registration's secrets: the
/// captcha and the code go over its stdin, since any uid can read an argv in `/proc`.
struct Registrar {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next: u64,
}

impl Registrar {
    fn start(cfg: &crate::config::Config) -> anyhow::Result<Self> {
        let mut child = command(&cfg.signal_cli, &data_dir(cfg))
            .arg("jsonRpc")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().context("signal-cli's stdin")?;
        let stdout = child.stdout.take().context("signal-cli's stdout")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next: 0,
        })
    }

    /// `method` for `account`, its result or signal-cli's own error.
    async fn call(
        &mut self,
        method: &str,
        account: &str,
        mut params: Value,
    ) -> anyhow::Result<Value> {
        self.next += 1;
        let id = format!("setup{}", self.next);
        params["account"] = account.into();
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin.write_all(format!("{line}\n").as_bytes()).await?;
        let answer = async {
            while let Some(line) = self.lines.next_line().await? {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v["id"] != id.as_str() {
                    continue;
                }
                if let Some(e) = v.get("error") {
                    let why = e["message"].as_str().unwrap_or("refused");
                    anyhow::bail!("signal-cli {method}: {why}");
                }
                return Ok(v["result"].clone());
            }
            anyhow::bail!("signal-cli exited during {method}")
        };
        tokio::time::timeout(Duration::from_secs(120), answer)
            .await
            .map_err(|_| anyhow::anyhow!("signal-cli {method} did not answer in 120 s"))?
    }
}

impl Drop for Registrar {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn setup_run(
    cfg: &crate::config::Config,
    account: Option<&str>,
    args: &[&str],
) -> anyhow::Result<()> {
    let mut cmd = command(&cfg.signal_cli, &data_dir(cfg));
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    // Bounded: a signal-cli waiting on the account's lock would otherwise wait for good.
    let status = tokio::time::timeout(SETUP_TIMEOUT, cmd.args(args).status())
        .await
        .map_err(|_| anyhow::anyhow!("signal-cli {} did not finish in 2 min", args[0]))??;
    anyhow::ensure!(status.success(), "signal-cli {} failed ({status})", args[0]);
    Ok(())
}

/// Whether a live setup holds the account (`HOLD_FILE`). A hold whose setup is gone (killed with
/// Ctrl-C) holds nothing and is removed, so it can never keep Signal down.
fn held(dir: &Path) -> bool {
    let file = dir.join(HOLD_FILE);
    let Ok(line) = std::fs::read_to_string(&file) else {
        return false;
    };
    // The setup that wrote it, by pid and kernel start time (a reused pid is another process),
    // and a lease it refreshes: a hung setup holds nothing either.
    let mut words = line.split_whitespace().map(str::parse::<i64>);
    let alive = match (words.next(), words.next()) {
        (Some(Ok(pid)), Some(Ok(ticks))) => {
            i32::try_from(pid).ok().and_then(crate::procs::start_ticks) == Some(ticks)
        }
        _ => false,
    };
    let fresh = (std::fs::metadata(&file).and_then(|m| m.modified()))
        .is_ok_and(|at| at.elapsed().is_ok_and(|age| age < HOLD_LEASE));
    if !(alive && fresh) {
        let _ = std::fs::remove_file(&file);
    }
    alive && fresh
}

/// Setup's hold on the account, from taking it until it is dropped: its lease is refreshed
/// every `HOLD_REFRESH` meanwhile.
struct Hold(PathBuf, tokio::task::AbortHandle);

impl Hold {
    fn take(dir: &Path) -> anyhow::Result<Self> {
        let me = std::process::id();
        let ticks = crate::procs::start_ticks(me as i32).context("this process's start time")?;
        let (file, part) = (dir.join(HOLD_FILE), dir.join(".hold.part"));
        // Written aside and renamed: the adapter never reads half a hold.
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&part)
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, format!("{me} {ticks}").as_bytes())
            })?;
        std::fs::rename(&part, &file)?;
        let lease = file.clone();
        let refresh = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HOLD_REFRESH).await;
                let touched = std::fs::File::options()
                    .write(true)
                    .open(&lease)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()));
                if let Err(e) = touched {
                    eprintln!("zephyr: refreshing the hold on the Signal account: {e}");
                }
            }
        });
        Ok(Self(file, refresh.abort_handle()))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.1.abort();
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A prompt of setup's, answered within `limit`: a setup left at a prompt must not hold the
/// account for good.
async fn ask(limit: Duration, prompt: String) -> anyhow::Result<String> {
    let asked = tokio::task::spawn_blocking(move || {
        dialoguer::Input::<String>::new()
            .with_prompt(prompt)
            .interact_text()
    });
    let answer = tokio::time::timeout(limit, asked)
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {} min", limit.as_secs() / 60))???;
    Ok(answer)
}

/// Waits until the daemon's signal-cli let the account go (`.ok` gone), for at most 30 s.
async fn released(dir: &Path) -> anyhow::Result<()> {
    for _ in 0..60 {
        if !dir.join(OK_FILE).exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("zephyr's signal-cli still holds the account; zephyr channel remove signal first")
}

/// `zephyr signal-setup`, run as root in the container by the host's `zephyr channel add|remove
/// signal` and `zephyr channel signal trust`: registers or links zephyr's own number, trusts a
/// changed safety number through the running daemon, or removes the account (DESIGN §4.3).
pub async fn setup(cfg: &crate::config::Config, args: &[String]) -> anyhow::Result<()> {
    let image = cfg.signal_cli.parent().and_then(Path::parent);
    anyhow::ensure!(
        image.is_some_and(Path::is_dir),
        "signal: not in this image (built with SIGNAL=0)"
    );
    let dir = data_dir(cfg);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["register", number, ref rest @ ..] => {
            let digits = number.strip_prefix('+').unwrap_or("");
            anyhow::ensure!(
                (7..=15).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit()),
                "{number} is not a phone number in international form (+821012345678)"
            );
            let voice = match rest {
                [] => false,
                ["--voice"] => true,
                _ => anyhow::bail!(USAGE),
            };
            println!("{WARNING}");
            println!(
                "Open https://signalcaptchas.org/registration/generate.html, solve the captcha, \
                 and copy the signalcaptcha:// link behind \"Open Signal\"."
            );
            let captcha: String = dialoguer::Input::new()
                .with_prompt("captcha link")
                .validate_with(
                    |s: &String| match s.trim().starts_with("signalcaptcha://") {
                        true => Ok(()),
                        false => Err("it starts with signalcaptcha://"),
                    },
                )
                .interact_text()?;
            private_dir(&dir)?;
            // After every prompt but the code's: the daemon's signal-cli lets the account go
            // for as long as this runs, and comes back when it ends, however it ends.
            let _hold = Hold::take(&dir)?;
            released(&dir).await?;
            let mut rpc = Registrar::start(cfg)?;
            let asked = json!({"captcha": captcha.trim(), "voice": voice});
            rpc.call("register", number, asked).await?;
            let how = if voice { "call" } else { "SMS" };
            let code = ask(CODE_TIMEOUT, format!("the code from Signal's {how}")).await?;
            let code = json!({"verificationCode": code.trim()});
            rpc.call("verify", number, code).await?;
            drop(rpc);
            setup_run(
                cfg,
                Some(number),
                &["updateProfile", "--given-name", "zephyr"],
            )
            .await?;
            println!("{number} is registered for zephyr");
        }
        ["link"] => {
            println!("{WARNING}");
            private_dir(&dir)?;
            let _hold = Hold::take(&dir)?;
            released(&dir).await?;
            // `link` takes no account: signal-cli learns it from the phone.
            let mut child = command(&cfg.signal_cli, &dir)
                .args(["link", "-n", "zephyr"])
                .stdout(std::process::Stdio::piped())
                .spawn()?;
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let first = async {
                loop {
                    match lines.next_line().await? {
                        Some(l) if l.trim().starts_with("sgnl://") => {
                            break anyhow::Ok(l.trim().to_string())
                        }
                        Some(l) => println!("{l}"),
                        None => anyhow::bail!("signal-cli link printed no sgnl:// link"),
                    }
                }
            };
            let uri = tokio::time::timeout(SETUP_TIMEOUT, first)
                .await
                .map_err(|_| anyhow::anyhow!("signal-cli link gave no link in 2 min"))??;
            println!("Scan this on zephyr's own phone: Signal, Settings, Linked devices.");
            let shown = std::process::Command::new("/usr/bin/qrencode")
                .env_clear()
                .args(["-t", "ansiutf8", &uri])
                .status()
                .is_ok_and(|s| s.success());
            if !shown {
                println!("{uri}");
            }
            let rest = async {
                while let Some(l) = lines.next_line().await? {
                    println!("{l}");
                }
                anyhow::Ok(child.wait().await?)
            };
            let status = tokio::time::timeout(LINK_TIMEOUT, rest)
                .await
                .map_err(|_| anyhow::anyhow!("the link was not scanned within 10 min"))??;
            anyhow::ensure!(status.success(), "signal-cli link failed ({status})");
        }
        ["trust", aci] => {
            anyhow::ensure!(is_aci(aci), "{aci} is not a Signal ACI (a UUID)");
            anyhow::ensure!(
                dir.is_dir(),
                "signal is not set up: {} is missing",
                dir.display()
            );
            let file = dir.join(TRUST_FILE);
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&file)
                .and_then(|mut f| std::io::Write::write_all(&mut f, aci.as_bytes()))?;
            // The daemon's signal-cli holds the account's lock: it trusts the key itself.
            for _ in 0..60 {
                if !file.exists() {
                    println!("the daemon trusts the new safety number of {aci}; replies to them go out again");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let _ = std::fs::remove_file(&file);
            anyhow::bail!(
                "the daemon did not take it within 30 s; is it running with [signal] set?"
            );
        }
        ["remove", ref rest @ ..] if rest.len() <= 1 => {
            let account = rest
                .first()
                .map(|a| a.to_string())
                .or_else(|| cfg.signal.as_ref().map(|s| s.account.clone()));
            if let (Some(account), true) = (account, dir.is_dir()) {
                // Best effort: an account Signal already dropped cannot be unregistered.
                let gone = tokio::time::timeout(
                    Duration::from_secs(60),
                    setup_run(cfg, Some(account.as_str()), &["unregister"]),
                );
                match gone.await {
                    Ok(Ok(())) => println!("{account} is unregistered"),
                    Ok(Err(e)) => println!("warn {e:#}; removing its keys anyway"),
                    Err(_) => println!("warn signal-cli unregister did not finish in 60 s; removing its keys anyway"),
                }
            }
            match std::fs::remove_dir_all(&dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => println!("removed {}", dir.display()),
            }
        }
        _ => anyhow::bail!(USAGE),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "+15550001111";
    const OWNER: &str = "7f3c0000-aaaa-4bbb-8ccc-dddddddddddd";

    #[test]
    fn the_jvm_keeps_its_temporary_files_in_roots_folder() {
        let home = crate::test_dir("signal-tmp");
        let dir = home.join("signal");
        let cmd = command(Path::new("/opt/zephyr/signal/bin/signal-cli"), &dir);
        let env: HashMap<String, String> = (cmd.as_std().get_envs())
            .filter_map(|(k, v)| Some((k.to_str()?.into(), v?.to_str()?.into())))
            .collect();
        let opts = &env["JAVA_TOOL_OPTIONS"];
        let tmp = dir.join("tmp");
        assert!(
            opts.contains(&format!("-Djava.io.tmpdir={}", tmp.display())),
            "{opts}"
        );
        assert!(opts.contains("-XX:-UsePerfData"), "{opts}");
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "made private, the agent cannot plant files there"
        );
    }

    /// A Signal adapter on `home` whose owners are `owners` on chat, its live config handed over.
    fn told_adapter(home: &Path, owners: &str) -> (Signal, crate::config::Config) {
        let cfg: crate::config::Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {home:?}\n[signal]\naccount = '{ACCOUNT}'\n[owners]\nchat = {owners}\n"
        ))
        .unwrap();
        let signal = Signal::new(&cfg).unwrap();
        signal.live(tokio::sync::watch::channel(Arc::new(cfg.clone())).1);
        (signal, cfg)
    }

    #[tokio::test]
    async fn two_failed_sends_at_once_tell_the_owner_once() {
        let home = crate::test_dir("signal-told-race");
        let (signal, cfg) = told_adapter(&home, "['owner']");
        tokio::join!(
            signal.changed_identity(OWNER),
            signal.changed_identity(OWNER)
        );
        let store = crate::store::Store::open(&cfg.db_path()).unwrap();
        assert_eq!(store.pending_outbox().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn after_a_partial_failure_only_the_owners_not_told_are_told() {
        let home = crate::test_dir("signal-told-partial");
        let (signal, cfg) = told_adapter(&home, "['owner', 'o2']");
        let store = crate::store::Store::open(&cfg.db_path()).unwrap();
        let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER no_o2 BEFORE INSERT ON outbox WHEN NEW.recipient = 'o2'
             BEGIN SELECT RAISE(ABORT, 'no'); END;",
        )
        .unwrap();
        signal.changed_identity(OWNER).await;
        db.execute_batch("DROP TRIGGER no_o2;").unwrap();
        signal.changed_identity(OWNER).await;
        signal.changed_identity(OWNER).await;
        let mut to: Vec<String> = (store.pending_outbox().unwrap().into_iter())
            .map(|r| r.recipient)
            .collect();
        to.sort();
        assert_eq!(to, ["o2", "owner"], "each owner once");
    }

    #[tokio::test]
    async fn a_safety_number_counts_as_told_only_once_the_notice_is_stored() {
        let home = crate::test_dir("signal-told");
        let cfg: crate::config::Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {home:?}\n[signal]\naccount = '{ACCOUNT}'\n[owners]\nchat = ['owner']\n"
        ))
        .unwrap();
        let signal = Signal::new(&cfg).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(Arc::new(cfg.clone()));
        signal.live(rx);
        // state.db cannot be opened: nothing is stored, so nothing counts as told.
        std::fs::create_dir_all(cfg.db_path()).unwrap();
        signal.changed_identity(OWNER).await;
        assert!(!signal.told.lock().unwrap().contains(OWNER));
        std::fs::remove_dir(cfg.db_path()).unwrap();
        signal.changed_identity(OWNER).await;
        signal.changed_identity(OWNER).await;
        assert!(signal.told.lock().unwrap().contains(OWNER));
        let store = crate::store::Store::open(&cfg.db_path()).unwrap();
        let told = store.pending_outbox().unwrap();
        assert_eq!(told.len(), 1, "once");
        assert!(
            told[0].text.contains("safety number of 7f3c"),
            "{}",
            told[0].text
        );
    }

    #[tokio::test]
    async fn the_captcha_and_the_code_never_go_on_signal_clis_command_line() {
        let home = crate::test_dir("signal-register");
        let mut cfg: crate::config::Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        cfg.signal_cli = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-signal-cli.py").into();
        private_dir(&data_dir(&cfg)).unwrap();
        let mut rpc = Registrar::start(&cfg).unwrap();
        let captcha = json!({"captcha": "signalcaptcha://secret-captcha", "voice": false});
        rpc.call("register", ACCOUNT, captcha).await.unwrap();
        let code = json!({"verificationCode": "123-456"});
        rpc.call("verify", ACCOUNT, code).await.unwrap();
        assert!(rpc.call("nope", ACCOUNT, json!({})).await.is_err());
        drop(rpc);
        let log = std::fs::read_to_string(data_dir(&cfg).join("log.jsonl")).unwrap();
        let start = log.lines().next().unwrap();
        assert!(
            start.contains("jsonRpc") && !start.contains("\"-a\""),
            "{start}"
        );
        assert!(
            !start.contains("secret-captcha") && !start.contains("123-456"),
            "{start}"
        );
        assert!(log.contains("\"verificationCode\": \"123-456\""), "{log}");
        assert!(
            log.contains(&format!("\"account\": \"{ACCOUNT}\"")),
            "{log}"
        );
    }

    fn env(extra: Value) -> Value {
        let mut v = json!({"sourceUuid": OWNER, "sourceNumber": "+15559990000",
            "sourceName": "Aiden", "timestamp": 7, "dataMessage": {"timestamp": 7, "message": "hi"}});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }

    #[test]
    fn a_direct_message_is_a_row_keyed_by_aci_and_timestamp() {
        let (row, files) = to_inbound(&env(json!({})), ACCOUNT, None).unwrap();
        assert_eq!(row.platform_msg_id, format!("{OWNER}-7"));
        assert_eq!((row.sender.as_str(), row.text.as_str()), (OWNER, "hi"));
        assert!(files.is_empty());
        assert_eq!(named(&env(json!({}))).unwrap(), "\"Aiden\", +15559990000");
    }

    #[test]
    fn only_a_direct_message_from_another_account_is_taken() {
        let none = |extra: Value| to_inbound(&env(extra), ACCOUNT, Some(OWNER)).is_none();
        assert!(none(json!({})), "our own ACI");
        let drop = |extra: Value| to_inbound(&env(extra), ACCOUNT, None).is_none();
        assert!(drop(json!({"sourceNumber": ACCOUNT})), "our own number");
        assert!(drop(json!({"sourceUuid": "not-a-uuid"})));
        assert!(drop(
            json!({"syncMessage": {"sentMessage": {"message": "x"}}})
        ));
        assert!(drop(
            json!({"dataMessage": {"message": "x", "groupInfo": {"groupId": "g"}}})
        ));
        assert!(drop(
            json!({"dataMessage": {"message": "", "reaction": {"emoji": "x"}}})
        ));
        assert!(drop(json!({"dataMessage": {"message": ""}})), "empty");
        assert!(!drop(
            json!({"dataMessage": {"attachments": [{"id": "a.png"}]}})
        ));
    }

    #[tokio::test]
    async fn setup_refuses_without_signal_in_the_image_and_a_bad_aci() {
        let home = crate::test_dir("signal-setup");
        let mut cfg: crate::config::Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        cfg.signal_cli = "/nonexistent/signal/bin/signal-cli".into();
        let err = setup(&cfg, &["link".into()]).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "signal: not in this image (built with SIGNAL=0)"
        );
        cfg.signal_cli = home.join("signal/bin/signal-cli");
        std::fs::create_dir_all(home.join("signal/bin")).unwrap();
        let err = setup(&cfg, &["trust".into(), "7f3c".into()]).await;
        assert!(err.unwrap_err().to_string().contains("is not a Signal ACI"));
        assert!(setup(&cfg, &["nope".into()]).await.is_err());
    }

    #[test]
    fn attachment_ids_are_plain_file_names() {
        assert!(plain("Xy_1-2.png") && !plain("../x") && !plain(".ok") && !plain("a/b"));
        let mut ids = Vec::new();
        let v = json!({"syncMessage": {"sentMessage": {"attachments": [{"contentType": "image/png", "id": "a.png"}],
            "quote": {"attachments": [{"thumbnail": {"contentType": "image/jpeg", "id": "t.jpg"}}]}}}});
        attachment_ids(&v, &mut ids);
        assert_eq!(ids, ["a.png", "t.jpg"]);
        assert!(
            is_aci(OWNER) && !is_aci("7f3c") && !is_aci("7f3c0000-aaaa-4bbb-8ccc-ddddddddddd!")
        );
    }
}
