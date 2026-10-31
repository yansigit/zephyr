//! Inbound → inbox → `main` child → outbox → channel, with crash recovery (DESIGN §4.2).

use crate::channel::{Activity, Channel, Inbound, Outbound};
use crate::config::{Account, Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child, MAIN_SESSION};
use crate::store::{InboxRow, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;

/// rpi's own cap (`MAX_RESUME_ATTEMPTS`); after that we tell the sender.
const MAX_RESUMES: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A channel send that takes longer fails, and that channel is skipped for `STUCK_PAUSE`, so
/// one stuck channel can't hold up the router or the other channels.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const STUCK_PAUSE: Duration = Duration::from_secs(30);

/// The prompt text for a row. The `#id` tag is what recovery looks for in `get_messages`.
pub fn tagged(row: &InboxRow) -> String {
    format!("[{} #{}] {}", row.channel, row.id, row.text)
}

/// The reverse of `tagged`: `(channel, inbox id, text)`, or `None` for untagged text.
pub fn untag(text: &str) -> Option<(&str, i64, &str)> {
    let (channel, rest) = text.strip_prefix('[')?.split_once(" #")?;
    let (id, body) = rest.split_once("] ")?;
    Some((channel, id.parse().ok()?, body))
}

/// What attached clients see of the daemon: `zephyr.hello` and `zephyr_status` (DESIGN §4.3.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub version: String,
    pub session: String,
    pub account: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub paused: Option<String>,
    pub channels: Vec<ChannelState>,
    pub spend: Spend,
    pub queue: Vec<Waiting>,
    pub failed_replies: i64,
    pub run: Run,
    /// The `[[account]]` pool, in order; empty without one.
    pub accounts: Vec<AccountState>,
    /// The soonest scheduled job runs, soonest first.
    pub jobs: Vec<NextJob>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    pub name: String,
    pub model: String,
    /// `ready` | `cooling` | `needs login`
    pub state: String,
    /// When a rate-limit cooldown ends.
    pub until_unix: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NextJob {
    pub id: i64,
    pub name: String,
    pub next_unix: i64,
}

/// At most this many jobs in `Snapshot.jobs`.
const NEXT_JOBS: usize = 5;

/// At most this many `/login` sign-ins wait for their URL at once: one whose URL is never posted
/// can never be finished, so a further `/login` is refused rather than dropping one.
const PENDING_LOGINS: usize = 8;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelState {
    pub name: String,
    /// `connected` | `off` | `reconnecting`
    pub state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spend {
    pub tokens: i64,
    pub cost: f64,
    pub daily_tokens_limit: Option<i64>,
    pub daily_cost_limit: Option<f64>,
}

/// An inbox row waiting to be dispatched.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Waiting {
    pub channel: String,
    pub preview: String,
    pub since_unix: i64,
}

/// The `main` run, from the child's events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub busy: bool,
    pub since_unix_ms: Option<i64>,
    pub origin: Option<Origin>,
    pub current_tool: Option<String>,
    pub tools_done: u32,
}

/// The user message a run is serving: its inbox row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Origin {
    pub channel: String,
    pub id: i64,
    pub sender: String,
}

/// Applies `f` to the snapshot and wakes receivers only if that changed it.
fn update(tx: &watch::Sender<Snapshot>, f: impl FnOnce(&mut Snapshot)) -> bool {
    tx.send_if_modified(|cur| {
        let mut next = cur.clone();
        f(&mut next);
        let changed = next != *cur;
        if changed {
            *cur = next;
        }
        changed
    })
}

/// Whitespace folded, at most 60 chars, `…` when cut.
fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// How long a reply may be retried: only as long as the platform still dedupes the client id.
fn retry_window(channel: &str) -> i64 {
    match channel {
        "discord" => 5 * 60,
        // tempGuid dedupes only while a send is in flight, and a timed-out send may duplicate. Accept at-least-once.
        "imessage" => 5 * 60,
        _ => 60 * 60,
    }
}

/// Why `run` returned.
#[derive(Debug, PartialEq)]
pub enum Exit {
    Shutdown,
    /// A reload changed what only a fresh daemon picks up; the caller execs itself.
    Reexec,
}

/// Runs the gateway until `shutdown` resolves, then drains. Every channel feeds one main session.
pub async fn run<C: Channel>(
    cfg: &Config,
    channels: &[C],
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<Exit> {
    std::fs::create_dir_all(cfg.state_dir())?;
    let (tx, rx) = mpsc::channel(64);
    let (attach_tx, attaches) = mpsc::channel(4);
    let jobs = crate::scheduler::Wake::default();
    let shared = Arc::new(cfg.clone());
    let (cfg_tx, cfg_rx) = watch::channel(shared.clone());
    let (snap, snap_rx) = watch::channel(Snapshot::default());
    let activity_rx = snap_rx.clone();
    let attach_cfgs = cfg_rx.clone();
    let mut router = Router {
        cfg: shared,
        cfg_tx,
        edits: edits(cfg),
        secrets: crate::dotenv::read(&cfg.secrets_path()).unwrap_or_default(),
        pending_model: None,
        reexec: false,
        auth_stamp: None,
        logins: Vec::new(),
        starting: Vec::new(),
        store: Store::open(&cfg.db_path())?,
        child: None,
        busy: false,
        resumes: 0,
        backoff: FIRST_BACKOFF,
        respawn_at: Instant::now(),
        unrouted: HashSet::new(),
        stuck: HashMap::new(),
        tap: broadcast::channel(1024).0,
        attaches,
        jobs: &jobs,
        auth_at: Instant::now(),
        restart: false,
        account: None,
        tally: Default::default(),
        snap,
        names: channels.iter().map(|c| c.name().to_string()).collect(),
        thinking: None,
        shown: 0,
        run_start: None,
        cut: None,
    };
    let runs = futures_util::future::try_join_all(channels.iter().map(|c| c.run(tx.clone())));
    tokio::select! {
        r = runs => r.map(|_| Exit::Shutdown),
        r = crate::attach::serve(cfg, tx, attach_tx, snap_rx, attach_cfgs) => r.map(|()| Exit::Shutdown),
        r = crate::scheduler::run(cfg_rx, &jobs) => r.map(|()| Exit::Shutdown),
        r = show_activity(channels, activity_rx) => r.map(|()| Exit::Shutdown),
        r = router.main(rx, channels, shutdown) => r,
    }
}

/// Shows the run on the channel it serves (typing, a progress note), from `Snapshot.run`.
/// A watch may skip states; diffing what it shows keeps Start and Stop paired anyway.
async fn show_activity<C: Channel>(
    channels: &[C],
    mut rx: watch::Receiver<Snapshot>,
) -> anyhow::Result<()> {
    let (mut prev, mut tools) = (Run::default(), Vec::new());
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            r = rx.changed() => if r.is_err() { return std::future::pending().await },
            _ = tick.tick() => {}
        }
        let next = rx.borrow_and_update().run.clone();
        let now = crate::unix_millis() as i64;
        for (o, a) in activities(&prev, &next, &mut tools, now) {
            if let Some(c) = channels.iter().find(|c| c.name() == o.channel) {
                if let Err(e) = c.activity(&o.sender, &a).await {
                    eprintln!("zephyr: {} activity failed: {e:#}", o.channel);
                }
            }
        }
        prev = next;
    }
}

/// What to show when the run goes from `prev` to `next`; `tools` collects the run's tool names.
/// While busy every call repeats Start before Progress: a mid-run notice ends a channel's typing,
/// and Start is a no-op while it is still up.
fn activities(
    prev: &Run,
    next: &Run,
    tools: &mut Vec<String>,
    now_ms: i64,
) -> Vec<(Origin, Activity)> {
    let mut out = Vec::new();
    if prev.origin != next.origin {
        out.extend(prev.origin.clone().map(|o| (o, Activity::Stop)));
        tools.clear();
    }
    let (true, Some(o)) = (next.busy, &next.origin) else {
        return out;
    };
    let same = prev.origin == next.origin;
    if let Some(t) = &next.current_tool {
        if !same || prev.current_tool.as_ref() != Some(t) || prev.tools_done != next.tools_done {
            tools.push(t.clone());
        }
    }
    let since = next.since_unix_ms.unwrap_or(now_ms);
    out.push((o.clone(), Activity::Start));
    out.push((
        o.clone(),
        Activity::Progress {
            elapsed: Duration::from_millis(now_ms.saturating_sub(since).max(0) as u64),
            tools: tools.clone(),
            current: next.current_tool.clone(),
        },
    ));
    out
}

/// The mtimes a hand edit changes: `zephyr.toml` and `secrets.env`.
type Edits = (Option<SystemTime>, Option<SystemTime>);

fn edits(cfg: &Config) -> Edits {
    let config = cfg.path.as_deref().and_then(crate::reload::stamp);
    (config, crate::reload::stamp(&cfg.secrets_path()))
}

/// A `/login` the tick still has to announce. rpi prints the URL a second or two after the child
/// starts, and waiting for it in the loop that reads the child would stop it from reading events,
/// capping a run or flushing a channel, so the tick posts the notice instead. `from` is the
/// channel and sender that asked: an attached client is reached by `show` alone.
struct Login {
    id: String,
    provider: &'static str,
    from: Option<(String, String)>,
}

/// A `/login` whose `crate::auth::start` is still running. Before it spawns anything, rpi may be
/// asked for its models, which takes seconds; that wait belongs to a task of its own, because the
/// loop that reads the child also owns a run's events, the 5 s tick, `cap_run`, the re-exec gate
/// and the child's stdout pump. The tick drains what has finished and posts its account, or why
/// it failed; `from` is the channel and sender that asked, so the notice is addressed to them.
struct Starting {
    provider: &'static str,
    from: Option<(String, String)>,
    task: tokio::task::JoinHandle<anyhow::Result<crate::auth::Start>>,
}

struct Router<'a> {
    /// The live config; `reload` swaps it and sends it to the scheduler.
    cfg: Arc<Config>,
    cfg_tx: watch::Sender<Arc<Config>>,
    /// Config file mtimes as of the last reload, so a hand edit is applied once.
    edits: Edits,
    /// `secrets.env` as this process loaded it; a change needs a re-exec.
    secrets: Vec<(String, String)>,
    /// A same-provider model switch, sent with `set_model` once the child is idle.
    pending_model: Option<String>,
    /// Exit with `Exit::Reexec` once main and the jobs are idle.
    reexec: bool,
    /// The child's `auth.json` mtime at spawn; a newer one (a re-login) restarts the child.
    auth_stamp: Option<SystemTime>,
    /// Sign-ins `/login` started whose URL is not out yet.
    logins: Vec<Login>,
    /// `/login` starts still running: the tick drains them, so this loop never waits on rpi.
    starting: Vec<Starting>,
    store: Store,
    child: Option<Child>,
    /// A prompt or resume is running in the child.
    busy: bool,
    resumes: u32,
    backoff: Duration,
    respawn_at: Instant,
    /// Outbox channels with no configured adapter, so each is logged once.
    unrouted: HashSet<String>,
    /// Channels whose last send timed out, and when to try them again.
    stuck: HashMap<String, Instant>,
    /// Every child event, for attached clients.
    tap: broadcast::Sender<Value>,
    attaches: mpsc::Receiver<crate::attach::AttachRequest>,
    /// Shared with the scheduler, which runs jobs beside `main`.
    jobs: &'a crate::scheduler::Wake,
    /// The next AuthKeeper check (DESIGN §4.7).
    auth_at: Instant,
    /// A refresh or a failover needs the child restarted; done once it is idle.
    restart: bool,
    /// Usage and outbound commands since the last settle.
    tally: crate::rpc::Tally,
    /// When the current run started, for `budget.run_minutes`.
    run_start: Option<Instant>,
    /// Why the current run was stopped by a run limit; told to the owner at its settle.
    cut: Option<String>,
    /// The pooled account the child runs on (DESIGN §4.9). `None` without a pool.
    account: Option<Account>,
    /// What attached clients see; `publish` refreshes it, `tapped` keeps `run` current.
    snap: watch::Sender<Snapshot>,
    /// The channel adapters this daemon runs.
    names: Vec<String>,
    /// rpi's thinking level, read from `get_state` when the child starts.
    thinking: Option<String>,
    /// The last inbox row whose user turn went to attached clients. Rows go out in id order,
    /// so one sent again (after `/stop` cleared rpi's queue, or a fail-over) is not shown twice.
    shown: i64,
}

async fn next_event(child: &mut Option<Child>) -> Option<Value> {
    match child {
        Some(c) => c.events.recv().await,
        None => std::future::pending().await,
    }
}

impl<'a> Router<'a> {
    async fn main<C: Channel>(
        &mut self,
        mut rx: mpsc::Receiver<Inbound>,
        channels: &[C],
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<Exit> {
        tokio::pin!(shutdown);
        let mut exit = Exit::Shutdown;
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        self.publish();
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep_until(self.respawn_at), if self.child.is_none() => {
                    self.spawn().await;
                }
                Some(m) = rx.recv() => self.inbound(m).await,
                Some(req) = self.attaches.recv() => self.attach(req).await,
                _ = self.jobs.outbox.notified() => {} // a job's output: flush below
                _ = tokio::time::sleep_until(self.auth_at), if (self.child.is_some() || !self.cfg.accounts.is_empty()) && self.cfg.auth.enabled => {
                    self.auth_check().await
                }
                ev = next_event(&mut self.child) => match self.tapped(ev) {
                    Some(ev) if ev["type"] == "agent_settled" => self.settled(&ev).await,
                    Some(_) => {
                        self.cap_run().await;
                        continue;
                    }
                    None => self.child_exited().await,
                },
                _ = tick.tick() => {
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
                    self.cap_run().await;
                    self.login_starts().await;
                    self.login_notices();
                    if self.cfg.path.is_some() && edits(&self.cfg) != self.edits {
                        let text = self.reload().await;
                        eprintln!("zephyr: zephyr.toml or secrets.env changed: {text}");
                    }
                }
            }
            self.flush(channels).await;
            self.publish();
            if self.reexec && !self.busy && !self.jobs.running.load(Ordering::SeqCst) {
                eprintln!("zephyr: restarting itself to apply the new settings");
                exit = Exit::Reexec;
                break;
            }
        }
        self.drain(channels).await;
        Ok(exit)
    }

    fn tapped(&mut self, ev: Option<Value>) -> Option<Value> {
        if let Some(ev) = &ev {
            let _ = self.tap.send(ev.clone()); // no receivers is fine
            self.tally.add(ev);
            self.track(ev);
        }
        ev
    }

    /// Keeps `Snapshot.run` in step with the child's run events. The origin comes from
    /// `user_turn`, set before the run starts.
    fn track(&self, ev: &Value) {
        match ev["type"].as_str().unwrap_or_default() {
            "agent_start" => update(&self.snap, |s| {
                s.run = Run {
                    busy: true,
                    since_unix_ms: Some(crate::unix_millis() as i64),
                    origin: s.run.origin.take(),
                    ..Run::default()
                }
            }),
            "tool_execution_start" => update(&self.snap, |s| {
                s.run.current_tool = ev["toolName"].as_str().map(Into::into)
            }),
            "tool_execution_end" => update(&self.snap, |s| {
                s.run.current_tool = None;
                s.run.tools_done += 1;
            }),
            "agent_settled" => update(&self.snap, |s| s.run = Run::default()),
            _ => false,
        };
    }

    /// Rebuilds the snapshot from the store and config, keeping `run`. Pushed only on change.
    fn publish(&self) {
        let built = (|| -> anyhow::Result<Snapshot> {
            let day = self.store.today()?;
            let (tokens, cost) = self.store.spend(&day)?;
            let budget = self.cfg.budget.clone().unwrap_or_default();
            let (now, cooling) = (crate::now(), self.store.cooldowns()?);
            let mut channels: Vec<_> = self
                .names
                .iter()
                .map(|n| (n.as_str(), "connected"))
                .collect();
            for known in ["chat", "discord", "imessage"] {
                if !self.names.iter().any(|n| n == known) {
                    channels.push((known, "off"));
                }
            }
            // shortcut: adapters report no connection state, so a running one counts as
            // connected; add `reconnecting` once they do.
            Ok(Snapshot {
                version: env!("CARGO_PKG_VERSION").into(),
                session: MAIN_SESSION.into(),
                account: self.account.as_ref().map(|a| a.name.clone()),
                model: (self.account.as_ref().map(|a| a.model.clone()))
                    .or_else(|| self.cfg.model.clone()),
                thinking: self.thinking.clone(),
                paused: self.store.pause_reason()?,
                channels: channels
                    .into_iter()
                    .map(|(name, state)| ChannelState {
                        name: name.into(),
                        state: state.into(),
                    })
                    .collect(),
                spend: Spend {
                    tokens,
                    cost,
                    daily_tokens_limit: budget.daily_tokens,
                    daily_cost_limit: budget.daily_cost_usd,
                },
                queue: (self.store.waiting()?.into_iter())
                    .map(|(channel, text, since_unix)| Waiting {
                        channel,
                        preview: preview(&text),
                        since_unix,
                    })
                    .collect(),
                failed_replies: self.store.outbox_failed()?,
                run: self.snap.borrow().run.clone(),
                accounts: (self.cfg.accounts.iter())
                    .map(|a| {
                        let (state, c) = crate::pool::state(&self.cfg, &cooling, a, now);
                        AccountState {
                            name: a.name.clone(),
                            model: a.model.clone(),
                            state: state.into(),
                            until_unix: c.map(|c| c.until),
                        }
                    })
                    .collect(),
                jobs: {
                    let tz = crate::scheduler::zone(&self.cfg)?;
                    let mut jobs: Vec<NextJob> = (self.store.jobs()?.into_iter())
                        .filter_map(|j| {
                            let next_unix = crate::scheduler::next_after(&j.schedule, now, &tz)?;
                            Some(NextJob {
                                id: j.id,
                                name: j.name,
                                next_unix,
                            })
                        })
                        .collect();
                    jobs.sort_by_key(|j| j.next_unix);
                    jobs.truncate(NEXT_JOBS);
                    jobs
                },
            })
        })();
        match built {
            Ok(new) => drop(update(&self.snap, |s| *s = new)),
            Err(e) => eprintln!("zephyr: snapshot failed: {e:#}"),
        }
    }

    /// History and the live stream start at the same point: events the child sends during
    /// `get_messages` are still queued and reach the new receiver.
    async fn attach(&mut self, req: crate::attach::AttachRequest) {
        let history = self.call(json!({"type": "get_messages"})).await;
        let history = history.unwrap_or_else(|e| {
            eprintln!("zephyr: no history for attach: {e:#}");
            Value::Null
        });
        let _ = req.send((history, self.tap.subscribe()));
    }

    async fn spawn(&mut self) {
        let account = match self.pick() {
            Ok(account) => account,
            Err(until) => {
                // Every account is cooling down: rows wait in the inbox until the first reset.
                eprintln!(
                    "zephyr: every account is rate-limited until {}",
                    crate::pool::iso(until)
                );
                let wait = Duration::from_secs((until - crate::now()).max(1) as u64);
                self.respawn_at = Instant::now() + wait;
                return;
            }
        };
        let session = self.store.main_session().unwrap_or_else(|e| {
            eprintln!("zephyr: reading the main session id failed: {e}");
            MAIN_SESSION.into()
        });
        match Child::spawn_as(&self.cfg, &session, account.as_ref()).await {
            Ok(child) => {
                eprintln!("zephyr: rpi child {:?} ready", child.pid());
                if let Some(a) = &account {
                    eprintln!("zephyr: on account {} ({})", a.name, a.model);
                }
                self.child = Some(child);
                self.account = account;
                self.pending_model = None;
                self.auth_stamp = self.auth_mtime();
                self.busy = false;
                let state = self.call(json!({"type": "get_state"})).await;
                self.thinking = state
                    .ok()
                    .and_then(|s| s["thinkingLevel"].as_str().map(Into::into));
                log(self.reconcile(None, false).await);
            }
            Err(e) => {
                eprintln!("zephyr: spawning rpi failed: {e:#}");
                self.schedule_respawn();
            }
        }
    }

    /// The account for the next child: `Ok(None)` without a pool, `Err(until)` when every
    /// account is cooling down.
    fn pick(&self) -> Result<Option<Account>, i64> {
        pick_in(&self.cfg, &self.store)
    }

    /// `auth.json` of the child's account, or of the agent dir without a pool.
    /// When the child's `auth.json` last changed; a newer one is a re-login.
    fn auth_mtime(&self) -> Option<SystemTime> {
        let file = match &self.account {
            Some(a) => self.cfg.account_dir(a),
            None => crate::auth_keeper::agent_dir(&self.cfg),
        }
        .join("auth.json");
        let _fs = crate::agent::fs();
        crate::reload::stamp(&file)
    }

    async fn child_exited(&mut self) {
        eprintln!("zephyr: rpi child exited; respawning in {:?}", self.backoff);
        self.child = None;
        self.busy = false;
        update(&self.snap, |s| s.run = Run::default());
        self.run_start = None;
        self.cut = None;
        self.schedule_respawn();
    }

    fn schedule_respawn(&mut self) {
        self.respawn_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    async fn inbound(&mut self, m: Inbound) {
        if m.text.trim().is_empty() {
            return; // attachment-only messages are not handled in v1
        }
        let id = match self.store.insert_inbox(&m) {
            Ok(Some(id)) => id,
            Ok(None) => return, // duplicate platform id
            Err(e) => return eprintln!("zephyr: inbox write failed: {e}"),
        };
        if !self.cfg.is_owner(&m.channel, &m.sender) {
            eprintln!(
                "zephyr: dropped message {id} from unknown sender {:?} on {}",
                m.sender, m.channel
            );
            log(self.store.set_status(id, "dropped").map_err(Into::into));
            return;
        }
        let text = m.text.trim();
        let reply = if let Some((reply, rules_changed)) =
            crate::memory::command(&self.cfg.home, self.cfg.state_dir(), text)
        {
            // rpi reads AGENTS.md once, at start: a fresh child on the same session loads the rules.
            self.restart |= rules_changed;
            reply
        } else if let Some(reply) = crate::scheduler::command(&mut self.store, &self.cfg, &m) {
            self.jobs.runs.notify_one();
            reply
        } else if let Some(reply) = self.command(text, (&m.channel, &m.sender)).await {
            reply
        } else if matches!(text, "/stop" | "/pause" | "/resume") {
            self.control(text).await.into()
        } else {
            return log(self.dispatch_new().await);
        };
        let resume = text == "/resume";
        let row = InboxRow {
            id,
            channel: m.channel,
            sender: m.sender,
            text: m.text,
            tries: 0,
        };
        log(self.store.reply(&row, &reply, "done").map_err(Into::into));
        if row.channel == crate::attach::CHANNEL {
            self.show(&reply, Some(row.text.trim()));
        }
        self.restart_if_idle().await;
        // A run in progress settles first; its settle reconciles and dispatches.
        if resume && !self.busy {
            log(self.reconcile(None, false).await);
        }
    }

    /// Stops the run once it passes `budget.run_turns` or `run_minutes` (DESIGN §4.4).
    async fn cap_run(&mut self) {
        let (Some(start), None, Some(budget)) = (self.run_start, &self.cut, &self.cfg.budget)
        else {
            return;
        };
        let Some(why) = budget.run_over(self.tally.turns, start.elapsed()) else {
            return;
        };
        eprintln!("zephyr: stopping the run after {why}");
        self.cut = Some(why);
        log(self.call(json!({"type": "abort"})).await.map(drop));
    }

    /// The kill switch (DESIGN §4.4), run outside the queue. Returns the reply text.
    async fn control(&mut self, cmd: &str) -> &'static str {
        match cmd {
            "/pause" => {
                log(self.store.set_paused(Some("user")).map_err(Into::into));
                if self.busy {
                    log(self.call(json!({"type": "abort"})).await.map(drop));
                }
                "[zephyr] paused: messages wait in the inbox until /resume"
            }
            "/resume" => {
                log(self.store.set_paused(None).map_err(Into::into));
                "[zephyr] resumed"
            }
            _ => {
                log(self.call(json!({"type": "abort"})).await.map(drop));
                "[zephyr] stopped"
            }
        }
    }

    /// Adds what the replies used since the last settle to today's spend. Returns true when
    /// this settle crossed a budget limit, and pauses the gateway for the rest of the UTC day.
    fn record_spend(&mut self, used: (i64, f64)) -> anyhow::Result<bool> {
        if let Some(a) = &self.account {
            self.store.add_account_spend(&a.name, used)?;
        }
        Ok(self.store.charge(self.cfg.budget.as_ref(), used)?)
    }

    /// Sends every `new` row: `prompt` when idle, rpi's own `queue_message` when busy.
    async fn dispatch_new(&mut self) -> anyhow::Result<()> {
        // Rows stay `new` while the child is down or the gateway is paused.
        if self.child.is_none() || self.store.pause_reason()?.is_some() {
            return Ok(());
        }
        for row in self.store.inbox_with_status("new")? {
            // Durable before the child sees it, so recovery knows to look for it.
            self.store.mark_dispatched(row.id)?;
            let text = tagged(&row);
            if self.busy {
                self.call(json!({"type": "queue_message", "message": text}))
                    .await?;
                self.user_turn(&row, &text, false);
            } else {
                self.call(json!({"type": "prompt", "message": text}))
                    .await?;
                self.busy = true;
                self.user_turn(&row, &text, true);
                self.run_start = Some(Instant::now());
            }
        }
        Ok(())
    }

    /// rpi 0.15 sends no message events for a prompt it accepts, so the router reports the user
    /// turn itself: on the tap for attached clients and, when it starts a run, as its origin.
    fn user_turn(&mut self, row: &InboxRow, text: &str, starts_run: bool) {
        if row.id > self.shown {
            self.shown = row.id;
            let m = json!({"role": "user", "kind": "user", "content": [{"type": "text", "text": text}],
                           "timestamp": crate::unix_millis() as i64});
            for kind in ["message_start", "message_end"] {
                let _ = self.tap.send(json!({"type": kind, "message": m}));
            }
        }
        if starts_run {
            let origin = Origin {
                channel: row.channel.clone(),
                id: row.id,
                sender: row.sender.clone(),
            };
            update(&self.snap, |s| s.run.origin = Some(origin));
        }
    }

    async fn settled(&mut self, ev: &Value) {
        self.busy = false;
        self.run_start = None;
        let cut = self
            .cut
            .take()
            .map(|why| format!("[zephyr] stopped the run after {why}"));
        // Before reconcile dispatches the next row, so that row runs on the new model.
        self.apply_model().await;
        self.backoff = FIRST_BACKOFF;
        let memory = crate::memory::commit_if_changed(&self.cfg.home, self.cfg.state_dir())
            .unwrap_or_else(|e| {
                eprintln!("zephyr: memory commit failed: {e:#}");
                None
            });
        let tally = std::mem::take(&mut self.tally);
        let crossed = self.record_spend(tally.used).unwrap_or_else(|e| {
            eprintln!("zephyr: recording spend failed: {e:#}");
            false
        });
        let fallback = match ev["error"].as_str() {
            Some(e) => format!(
                "[zephyr] run {}: {e}",
                ev["outcome"].as_str().unwrap_or("failed")
            ),
            None => "[zephyr] the run ended without a reply".into(),
        };
        let limited = self
            .account
            .clone()
            .zip(ev["error"].as_str())
            .and_then(|(a, e)| crate::pool::rate_limit(e).map(|resets| (a, e, resets)));
        match limited {
            Some((account, error, resets_at)) => {
                log(self.fail_over(&account, error, resets_at).await)
            }
            None => {
                if let (Some(a), "completed") =
                    (&self.account, ev["outcome"].as_str().unwrap_or(""))
                {
                    log(self.store.clear_cooldown(&a.name).map_err(Into::into));
                }
                log(self.reconcile(Some(fallback), false).await);
            }
        }
        // After the reply, so the owner reads what was sent out next to what it was for.
        for notice in [cut, tally.alert(), memory].into_iter().flatten() {
            log(self.store.notify_last_owner(&notice).map_err(Into::into));
        }
        if crossed {
            log(self
                .store
                .notify_last_owner(BUDGET_NOTICE)
                .map_err(Into::into));
        }
        self.restart_if_idle().await;
    }

    /// The turn hit `account`'s rate limit: cool it down, put the turn's unanswered rows back,
    /// tell the owner, and restart on the next account (or wait for the first reset). The new
    /// child continues the same session, and the rows are sent again as prompts.
    async fn fail_over(
        &mut self,
        account: &Account,
        error: &str,
        resets_at: Option<i64>,
    ) -> anyhow::Result<()> {
        let now = crate::now();
        let until = crate::pool::cool_down(&self.store, &account.name, error, resets_at, now)?;
        let rows = self.store.inbox_with_status("dispatched")?;
        self.reconcile(None, true).await?;
        let text = match crate::pool::pick(&self.cfg.accounts, &self.store.cooldowns()?, now) {
            Ok(next) => format!(
                "[zephyr] account {} is rate-limited until {}; switched to {}",
                account.name,
                crate::pool::iso(until),
                next.name
            ),
            Err(at) => format!(
                "[zephyr] every account is rate-limited; messages wait in the inbox until {}",
                crate::pool::iso(at)
            ),
        };
        eprintln!("zephyr: {text}");
        match rows.first() {
            Some(row) => self.store.notify(&row.channel, &row.sender, &text)?,
            None => self.store.notify_last_owner(&text)?,
        }
        self.restart = true;
        Ok(())
    }

    /// `/reload`, `/status` and `/model`, run outside the queue. `None` for anything else.
    async fn command(&mut self, text: &str, from: (&str, &str)) -> Option<String> {
        let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
        Some(match (cmd, arg.trim()) {
            ("/reload", "") => self.reload().await,
            ("/status", "") => self.status(),
            ("/model", "") => self.models(),
            ("/model", model) => self.switch_model(model).await,
            ("/new", "") => self.new_session().await,
            ("/login", arg) => self.login(arg, from).await,
            _ => return None,
        })
    }

    /// `/login [PROVIDER [ACCOUNT [MODEL]]]`: signs a provider in from the live gateway, with the
    /// same child `zephyr login` starts on the host (`crate::auth`). Nothing restarts here: a new
    /// `auth.json` for the account the child runs on is a restart-on-idle (`Plan::RestartChild`),
    /// and an account added to the pool only changes the pool.
    ///
    /// This runs on the loop that reads the child, which also owns the 5 s tick, `cap_run`, the
    /// re-exec gate and the child's stdout pump, so it never waits on rpi: `auth::start` can
    /// spend 20 s on a model listing, so it goes to a task of its own and the tick posts what it
    /// found (`login_starts`). Refusals that need no subprocess are settled here, at once.
    ///
    /// `/login cancel [ID|PROVIDER]` is the way back out: a chat has no other handle on a sign-in
    /// whose URL was never posted, and the pending cap refuses a ninth rather than drop one.
    async fn login(&mut self, arg: &str, from: (&str, &str)) -> String {
        let ids = || -> String {
            crate::auth::PROVIDERS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ")
        };
        if arg.is_empty() {
            return format!(
                "[zephyr] /login PROVIDER [ACCOUNT] [MODEL]\nproviders: {}\n\
                 /login cancel [ID|PROVIDER] stops a sign-in, so a refused one can be retried.\n\
                 A provider that takes a key or a setup token is signed in on the host: \
                 `zephyr login PROVIDER`. Never paste a token here.",
                ids()
            );
        }
        // `cancel` is not a provider id, so nothing is shadowed.
        let (verb, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        if verb == "cancel" {
            return self.login_cancel(rest).await;
        }
        let mut words = arg.split_whitespace();
        let provider = words.next().unwrap_or_default();
        let account = words.next().map(str::to_string);
        let model = words.next().map(str::to_string);
        if let Some(extra) = words.next() {
            return format!("[zephyr] unexpected {extra:?}: /login PROVIDER [ACCOUNT] [MODEL]");
        }
        // Both checks are free — a table lookup, no child — so a refusal costs no login to
        // start and then cancel.
        let Some(p) = crate::auth::provider(provider) else {
            return format!("[zephyr] unknown provider {provider}; one of {}.", ids());
        };
        if p.kind != crate::auth::Kind::Oauth {
            // A chat is not a place to type a key: the other end is another vendor.
            return format!(
                "[zephyr] {provider} takes {}: run `zephyr login {provider}` on the host. \
                 Never paste a token here.",
                match p.kind {
                    crate::auth::Kind::Key => "an API key",
                    _ => "a setup token",
                }
            );
        }
        // A login whose URL is never posted can never be finished, and a live sign-in is never
        // dropped to make room: refuse instead, so the channel user is not left waiting forever.
        if self.logins.len() + self.starting.len() >= PENDING_LOGINS {
            return format!(
                "[zephyr] {PENDING_LOGINS} sign-ins are waiting for their URL; finish one or \
                 `/login cancel`"
            );
        }
        let cfg = self.cfg.clone();
        let (wanted, model_arg) = (account.clone(), model.clone());
        let wanted_provider = provider.to_string();
        let task = tokio::spawn(async move {
            crate::auth::start(&cfg, &wanted_provider, wanted, model_arg).await
        });
        self.starting.push(Starting {
            provider: p.id,
            from: Some((from.0.to_string(), from.1.to_string())),
            task,
        });
        // Never a name rpi has not given us: the account is resolved inside `start` (the first
        // free `<provider>-<n>`, a whole `zephyr.toml` read away), so the reply names one only
        // when the command did, and the tick posts the line that has the real one.
        match &account {
            Some(name) => {
                format!("[zephyr] signing in to {provider} as {name}; the URL follows here.")
            }
            None => format!("[zephyr] signing in to {provider}; the URL follows here."),
        }
    }

    /// `/login cancel [ID|PROVIDER]`: stops sign-ins this daemon holds, so a chat that abandoned
    /// one can start another — nothing else a channel can send reaches them. With an `ID`, that
    /// login; with a `PROVIDER` id, the ones waiting for that provider (the notices still to be
    /// posted and the starts still resolving); with neither, every login the daemon holds, the
    /// attached TUI's included. A start that has not resolved has no id to have been told, so it
    /// is aborted and named by its provider. A login that already recorded its account is
    /// refused by `auth::cancel` and stays `done`: its row is in the file.
    async fn login_cancel(&mut self, what: &str) -> String {
        let what = what.trim();
        let provider = crate::auth::provider(what).map(|p| p.id);
        let mut lines = Vec::new();
        if what.is_empty() || provider.is_some() {
            let starting = std::mem::take(&mut self.starting);
            for Starting {
                provider: p,
                from,
                task,
            } in starting
            {
                if !what.is_empty() && provider != Some(p) {
                    self.starting.push(Starting {
                        provider: p,
                        from,
                        task,
                    });
                    continue;
                }
                if !task.is_finished() {
                    task.abort();
                    lines.push(format!(
                        "[zephyr] cancelled the sign-in to {p}; it had not resolved yet"
                    ));
                    continue;
                }
                // Resolved since the last tick: it registered a login nobody has an id for yet,
                // and a failed start left nothing to stop.
                if let Ok(Ok(start)) = task.await {
                    if crate::auth::cancel(&start.login_id) {
                        lines.push(format!(
                            "[zephyr] cancelled the sign-in to {p} as {}",
                            start.account
                        ));
                    }
                }
            }
        }
        let ids: Vec<String> = match provider {
            // Only `/login` knows a login's provider for as long as it waits, so the ids of the
            // ones it already announced are gone with the notice; `auth::logins` has them all.
            // But only this router's own logins are cancelled: the registry is process-wide,
            // and another daemon (or an attached TUI's `zephyr.login_*`) may hold a login with
            // the same provider. A bare cancel takes every login this router started.
            Some(p) => self
                .logins
                .iter()
                .filter(|l| l.provider == p)
                .map(|l| l.id.clone())
                .collect(),
            None if what.is_empty() => self.logins.iter().map(|l| l.id.clone()).collect(),
            // An id names one login; only this router's own ids are honoured, so one channel
            // cannot stop another daemon's (or an attached TUI's) sign-in by guessing its id.
            None => vec![what.to_string()]
                .into_iter()
                .filter(|id| self.logins.iter().any(|l| &l.id == id))
                .collect(),
        };
        for id in ids {
            let provider = self.logins.iter().find(|l| l.id == id).map(|l| l.provider);
            // Read before the cancel: `cancel` takes the entry out of the registry.
            let account = crate::auth::poll(&id)
                .and_then(|l| l["account"].as_str().map(str::to_string))
                .filter(|a| !a.is_empty());
            // `false`: already `done` or unknown, and a `done` login stays `done`.
            if !crate::auth::cancel(&id) {
                continue;
            }
            self.logins.retain(|l| l.id != id);
            lines.push(match (provider, account) {
                (Some(p), Some(a)) => format!("[zephyr] cancelled the sign-in to {p} as {a}"),
                (Some(p), None) => format!("[zephyr] cancelled the sign-in to {p}"),
                (None, Some(a)) => format!("[zephyr] cancelled the sign-in as {a}"),
                (None, None) => "[zephyr] cancelled a sign-in".to_string(),
            });
        }
        if lines.is_empty() {
            return "[zephyr] no sign-in to cancel".into();
        }
        lines.join("\n")
    }

    /// Drains the `/login` starts that have finished: the account and model they resolved, or why
    /// they failed, as a notice. On the 5 s tick, since the start runs in its own task.
    async fn login_starts(&mut self) {
        let starting = std::mem::take(&mut self.starting);
        for Starting {
            provider,
            from,
            task,
        } in starting
        {
            if !task.is_finished() {
                self.starting.push(Starting {
                    provider,
                    from,
                    task,
                });
                continue;
            }
            let text = match task.await {
                Ok(Ok(start)) => {
                    // Registered in `crate::auth`, so `login_notices` follows it for its URL.
                    self.logins.push(Login {
                        id: start.login_id,
                        provider: start.provider,
                        from: from.clone(),
                    });
                    format!(
                        "[zephyr] signing in to {} as {}{}",
                        start.provider,
                        start.account,
                        match start.model.as_str() {
                            "" => String::new(),
                            model => format!(" ({model})"),
                        }
                    )
                }
                // `start` checks the cheap things first — an unknown provider, no `zephyr.toml`
                // to record the account in, the registry's own cap — before it spawns anything.
                Ok(Err(e)) => format!("[zephyr] signing in to {provider} failed: {e:#}"),
                Err(e) => format!("[zephyr] signing in to {provider} failed: {e}"),
            };
            self.login_notice(&text, &from);
        }
    }

    /// Posts what each `/login` sign-in has to say — the URL, a failure, or the account once it
    /// is recorded — and forgets it. On the 5 s config tick, so nothing here waits on a login and
    /// a login that died without a word is dropped by the registry and forgotten with it.
    fn login_notices(&mut self) {
        let logins = std::mem::take(&mut self.logins);
        for Login { id, provider, from } in logins {
            let Some(login) = crate::auth::poll(&id) else {
                continue; // finished long ago, or evicted: nothing left to say
            };
            let phase = login["phase"].as_str().unwrap_or_default();
            let text = match phase {
                "done" => format!(
                    "[zephyr] signed in to {provider} as {}{}",
                    login["account"].as_str().unwrap_or_default(),
                    match login["model"].as_str() {
                        Some(model) => format!(" ({model})"),
                        None => String::new(),
                    }
                ),
                "failed" => format!(
                    "[zephyr] signing in to {provider} failed: {}",
                    login["error"].as_str().unwrap_or("no reason given")
                ),
                _ => match login["url"].as_str() {
                    Some(url) => format!(
                        "[zephyr] sign in to {provider} as {} at {url}",
                        login["account"].as_str().unwrap_or_default()
                    ),
                    None => {
                        // Still waiting for the URL.
                        self.logins.push(Login { id, provider, from });
                        continue;
                    }
                },
            };
            self.login_notice(&text, &from);
        }
    }

    /// One `/login` notice, in both places a `/login` can come from: an attached client sees it
    /// live, and the channel that asked gets it as an outbox row. The row is addressed to that
    /// sender, not to whoever spoke last in the inbox: a second owner must not see the first
    /// one's sign-in URL. Both writes, neither waits.
    fn login_notice(&self, text: &str, from: &Option<(String, String)>) {
        self.show(text, None);
        let Some((channel, recipient)) = from else {
            return;
        };
        if channel == crate::attach::CHANNEL {
            return; // that sender is the attached client `show` has just reached
        }
        let _ = self.store.notify(channel, recipient, text);
    }

    /// `/new`: a fresh main session from the next message. The old one stays on disk; memory
    /// and the rules carry over, since they are files. Long sessions are easier to steer
    /// (ClawSafety Table 3), so the owner can cut one off.
    async fn new_session(&mut self) -> String {
        let id = format!("{MAIN_SESSION}-{}", crate::now());
        if let Err(e) = self.store.set_main_session(&id) {
            return format!("[zephyr] no new conversation: {e}");
        }
        self.restart = true;
        let when = if self.busy {
            "after the current reply"
        } else {
            "from the next message"
        };
        self.restart_if_idle().await;
        format!("[zephyr] new conversation {when}; memory and rules are kept")
    }

    fn status(&self) -> String {
        let agent = match (&self.child, &self.account, &self.cfg.model) {
            (None, _, _) => "agent: not running".to_string(),
            (Some(_), Some(a), _) => format!("agent: account {} on {}", a.name, a.model),
            (Some(_), None, model) => {
                format!(
                    "agent: on {}",
                    model.as_deref().unwrap_or("rpi's default model")
                )
            }
        };
        let rest = crate::reload::status_lines(&self.cfg, &self.store)
            .unwrap_or_else(|e| vec![format!("status failed: {e:#}")]);
        std::iter::once(agent)
            .chain(rest)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn models(&self) -> String {
        let mut lines = vec![self.status().lines().next().unwrap_or_default().to_string()];
        lines.extend(
            self.cfg
                .accounts
                .iter()
                .map(|a| format!("account {}: {}", a.name, a.model)),
        );
        lines.push("/model PROVIDER/MODEL switches; it applies after the current reply".into());
        lines.join("\n")
    }

    /// `/model PROVIDER/MODEL`: sets it on the account for that provider in `zephyr.toml`
    /// (`reload::set_model`), checks that account's rpi lists it, saves, then reloads.
    ///
    /// `set_model` derives the account from the file it writes, so the check names the derivation
    /// on the file as it is now, and the write re-derives: a `/login` that records an account in
    /// between must not make the write land on an account no rpi was asked about, so it writes
    /// nothing and the new pool goes through the check first.
    async fn switch_model(&mut self, model: &str) -> String {
        let Some(path) = self.cfg.path.clone() else {
            return "[zephyr] no zephyr.toml to change".into();
        };
        let active = self.account.as_ref().map(|a| a.name.clone());
        for _ in 0..3 {
            let derived = crate::host::conf::load(&path).and_then(|mut doc| {
                crate::reload::set_model(&mut doc, active.as_deref(), model)?;
                Ok(model_account(&doc, model))
            });
            let name = match derived {
                Ok(name) => name,
                Err(e) => return format!("[zephyr] model not changed: {e:#}"),
            };
            let target = name.clone().map(|name| Account {
                name,
                model: model.to_string(),
            });
            if !lists(&self.cfg.clone(), target.as_ref(), model).await {
                return format!("[zephyr] rpi has no model {model}");
            }
            // The check and the write must name the same account: the closure that writes
            // re-derives, and a pool that moved under the check leaves the file untouched.
            let mut moved = false;
            let saved = crate::host::conf::update(&path, |doc| {
                crate::reload::set_model(doc, active.as_deref(), model)?;
                if model_account(doc, model) != name {
                    moved = true;
                    anyhow::bail!("the account pool changed while rpi listed the model");
                }
                Ok(())
            });
            if moved {
                continue;
            }
            return match saved {
                Ok(()) => self.reload().await,
                Err(e) => format!("[zephyr] model not changed: {e:#}"),
            };
        }
        "[zephyr] model not changed: the account pool kept changing while rpi listed the model"
            .into()
    }

    /// Reads `zephyr.toml` and `secrets.env` again and applies what changed (`reload::diff`).
    /// A file that does not load leaves everything as it was. Returns the reply text.
    async fn reload(&mut self) -> String {
        use crate::reload::Plan;
        let Some(path) = self.cfg.path.clone() else {
            return "[zephyr] no zephyr.toml to reload".into();
        };
        self.edits = edits(&self.cfg);
        let new = match Config::load(&path) {
            Ok(new) => new,
            Err(e) => return format!("[zephyr] zephyr.toml not applied: {e:#}"),
        };
        let secrets = crate::dotenv::read(&new.secrets_path()).unwrap_or_default();
        let next = pick_in(&new, &self.store).ok().flatten();
        let mut plan = crate::reload::diff(&self.cfg, &new, self.account.as_ref(), next.as_ref());
        if secrets != self.secrets {
            plan = Plan::Reexec;
        } else if self.child.is_some()
            && self.auth_mtime() != self.auth_stamp
            && !matches!(plan, Plan::Reexec)
        {
            plan = Plan::RestartChild; // a re-login: rpi reads credentials at start
        }
        let new = Arc::new(new);
        self.cfg = new.clone();
        let _ = self.cfg_tx.send(new);
        let after = if self.busy {
            "after the current reply"
        } else {
            "now"
        };
        match plan {
            Plan::Same => "[zephyr] nothing changed".into(),
            Plan::Live => {
                self.account = next.or(self.account.take());
                "[zephyr] applied".into()
            }
            Plan::SetModel(model) => {
                self.account = next;
                self.pending_model = Some(model.clone());
                if self.busy {
                    return format!("[zephyr] switching to {model} {after}");
                }
                match self.apply_model().await {
                    None => format!("[zephyr] now on {model}"),
                    Some(e) => e,
                }
            }
            Plan::RestartChild => {
                self.restart = true;
                self.restart_if_idle().await;
                let on = next.map_or(String::new(), |a| format!(" on {} ({})", a.name, a.model));
                format!("[zephyr] restarting the agent{on} {after}; the conversation carries on")
            }
            Plan::Reexec => {
                self.reexec = true;
                format!("[zephyr] restarting zephyr's channels {after}")
            }
        }
    }

    /// Sends a pending `set_model`. A refusal goes to the owner, and the error text is returned.
    async fn apply_model(&mut self) -> Option<String> {
        let model = self.pending_model.take()?;
        self.child.as_ref()?; // a child that is down starts on the new model anyway
        let (provider, id) = model.split_once('/')?;
        let cmd = json!({"type": "set_model", "provider": provider, "modelId": id});
        let e = self.call(cmd).await.err()?;
        let text = format!("[zephyr] rpi refused {model}: {e:#}");
        eprintln!("zephyr: {text}");
        log(self.store.notify_last_owner(&text).map_err(Into::into));
        Some(text)
    }

    /// Refreshes near-expiry OAuth credentials, then restarts the child once it is idle.
    async fn auth_check(&mut self) {
        let every = Duration::from_secs_f64((self.cfg.auth.refresh_minutes * 60.0).max(1.0));
        self.auth_at = Instant::now() + every;
        let now = crate::now();
        if !self.cfg.accounts.is_empty() {
            self.restart |=
                crate::auth_keeper::refresh_accounts(&self.cfg, self.account.as_ref(), now).await;
        } else if let Some(child) = self.child.as_mut() {
            match crate::auth_keeper::refresh(child, &self.cfg, now).await {
                Ok(refreshed) => self.restart |= refreshed,
                Err(e) => eprintln!("zephyr: auth check failed: {e:#}"),
            }
        }
        self.restart_if_idle().await;
    }

    /// rpi binds credentials and the model when a session starts, so a refreshed token or a
    /// failover needs a fresh child.
    async fn restart_if_idle(&mut self) {
        if !self.restart || self.busy {
            return;
        }
        self.restart = false;
        if let Some(child) = self.child.take() {
            child.stop().await;
            eprintln!("zephyr: restarting rpi");
        }
        self.respawn_at = Instant::now();
    }

    /// Matches `dispatched` rows against the session: replies to the ones rpi has
    /// answered, resumes an interrupted run, and re-sends rows rpi never got (once).
    /// Runs after every settle and after every (re)spawn, so it is also crash recovery.
    /// `requeue` (a failover) puts unanswered rows back to `new` and dispatches nothing.
    async fn reconcile(&mut self, fallback: Option<String>, requeue: bool) -> anyhow::Result<()> {
        let snap = self.call(json!({"type": "get_messages"})).await?;
        let rows = self.store.inbox_with_status("dispatched")?;
        if snap["pendingResume"] == true {
            if self.store.pause_reason()?.is_some() {
                return Ok(()); // `/resume` continues the interrupted run
            }
            if self.resumes < MAX_RESUMES {
                self.resumes += 1;
                self.call(json!({"type": "resume_pending"})).await?;
                self.busy = true;
                self.run_start = Some(Instant::now());
                return Ok(());
            }
            for row in &rows {
                let text = "[zephyr] the run was interrupted and could not be resumed";
                self.store.reply(row, text, "failed")?;
            }
            self.resumes = 0;
            return Ok(());
        }
        self.resumes = 0;

        // tagged user text -> last assistant text that followed it
        let mut replies: HashMap<String, Option<String>> = HashMap::new();
        let mut current = None;
        for m in snap["messages"].as_array().into_iter().flatten() {
            match m["kind"].as_str() {
                Some("user") => {
                    let text = message_text(m);
                    replies.insert(text.clone(), None);
                    current = Some(text);
                }
                Some("assistant") => {
                    let text = message_text(m);
                    if let (Some(c), false) = (&current, text.is_empty()) {
                        replies.insert(c.clone(), Some(text));
                    }
                }
                _ => {}
            }
        }

        let mut missing = Vec::new();
        for row in rows {
            match replies.get(&tagged(&row)) {
                Some(Some(reply)) => self.store.reply(&row, reply, "done")?,
                Some(None) if requeue => self.store.set_status(row.id, "new")?,
                Some(None) => {
                    let text = fallback
                        .as_deref()
                        .unwrap_or("[zephyr] the run ended without a reply");
                    self.store.reply(&row, text, "done")?
                }
                None => missing.push(row),
            }
        }
        if !missing.is_empty() {
            // A follow-up queued as the run ended stays in rpi's queue; take it back.
            self.call(json!({"type": "clear_queue"})).await?;
        }
        for row in missing {
            if row.tries >= 2 {
                let text = "[zephyr] this message could not be delivered to the agent";
                self.store.reply(&row, text, "failed")?;
            } else {
                self.store.set_status(row.id, "new")?;
            }
        }
        if requeue {
            return Ok(());
        }
        self.dispatch_new().await
    }

    async fn flush<C: Channel>(&mut self, channels: &[C]) {
        let rows = match self.store.pending_outbox() {
            Ok(rows) => rows,
            Err(e) => return eprintln!("zephyr: outbox read failed: {e}"),
        };
        // An attached client saw the agent's replies live (a later attach sees them in the
        // history), and command replies when they were made: both are `z…` rows. Notices and
        // job output go to the clients attached now, or wait for the next one; so do the
        // router's own `[zephyr]` answers to a turn (a failed or empty run), which no agent
        // event carries.
        for row in rows.iter().filter(|r| r.channel == crate::attach::CHANNEL) {
            let command =
                (row.prompt.as_deref().map(str::trim)).is_some_and(|p| p.starts_with('/'));
            if !row.client_id.starts_with('z') || (!command && row.text.starts_with("[zephyr]")) {
                if self.tap.receiver_count() == 0 {
                    continue;
                }
                self.show(&row.text, None);
            }
            log(self
                .store
                .set_outbox_status(row.id, "sent")
                .map_err(Into::into));
        }
        for row in rows
            .into_iter()
            .filter(|r| r.channel != crate::attach::CHANNEL)
        {
            if self
                .stuck
                .get(&row.channel)
                .is_some_and(|t| Instant::now() < *t)
            {
                continue;
            }
            let Some(channel) = channels.iter().find(|c| c.name() == row.channel) else {
                if self.unrouted.insert(row.channel.clone()) {
                    eprintln!(
                        "zephyr: no {} channel is configured; its replies stay pending",
                        row.channel
                    );
                }
                continue;
            };
            let msg = Outbound {
                channel: row.channel.clone(),
                recipient: row.recipient,
                client_id: row.client_id,
                text: row.text,
            };
            let sent = match tokio::time::timeout(SEND_TIMEOUT, channel.send(&msg)).await {
                Ok(sent) => sent,
                Err(_) => {
                    eprintln!("zephyr: {} send timed out; trying again later", row.channel);
                    self.stuck
                        .insert(row.channel.clone(), Instant::now() + STUCK_PAUSE);
                    Err(anyhow::anyhow!("timed out"))
                }
            };
            let status = match sent {
                Ok(()) => "sent",
                Err(e) if crate::now() - row.created_at > retry_window(&row.channel) => {
                    eprintln!(
                        "zephyr: reply {} to {} failed for good: {e:#}",
                        row.id, row.channel
                    );
                    "failed"
                }
                Err(_) => continue, // retried on the next pass
            };
            log(self
                .store
                .set_outbox_status(row.id, status)
                .map_err(Into::into));
        }
    }

    /// Shows `text`, the router's own (a command reply, a notice), to attached clients:
    /// `attach` draws it for stock clients as an assistant message, the TUI as a reply card.
    fn show(&self, text: &str, command: Option<&str>) {
        let reply = json!({"type": "zephyr_reply", "text": text, "command": command});
        let _ = self.tap.send(reply); // no receivers is fine
    }

    /// SIGTERM: stop intake, let a running turn settle, flush, then stop the child.
    async fn drain<C: Channel>(&mut self, channels: &[C]) {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.busy {
            match tokio::time::timeout_at(deadline, next_event(&mut self.child)).await {
                // As in `main`, so the run's spend and outbound list are booked.
                Ok(Some(ev)) => match self.tapped(Some(ev)) {
                    Some(ev) if ev["type"] == "agent_settled" => self.settled(&ev).await,
                    _ => {}
                },
                _ => break, // child gone or out of time: recovery picks it up next start
            }
        }
        if self.busy {
            // Cut off: book what the run did so far.
            let tally = std::mem::take(&mut self.tally);
            log(self.record_spend(tally.used).map(drop));
            if let Some(alert) = tally.alert() {
                log(self.store.notify_last_owner(&alert).map_err(Into::into));
            }
        }
        self.flush(channels).await;
        if let Some(child) = self.child.take() {
            child.stop().await;
        }
    }

    async fn call(&mut self, cmd: Value) -> anyhow::Result<Value> {
        match &mut self.child {
            Some(c) => c.call(cmd).await,
            None => anyhow::bail!("rpi child is not running"),
        }
    }
}

/// The account `reload::set_model` names for `model` in `doc`, read back out of the document it
/// just wrote: the write's own derivation, so a check and a write talk about one account.
fn model_account(doc: &toml_edit::DocumentMut, model: &str) -> Option<String> {
    crate::host::conf::accounts(doc)
        .into_iter()
        .find(|(_, m)| m == model)
        .map(|(name, _)| name)
}

/// Whether `rpi --list-models`, signed in as `account`, lists `model`. A listing that fails
/// says nothing, so it counts as yes. The running child lists only its own sign-in's models.
async fn lists(cfg: &Config, account: Option<&Account>, model: &str) -> bool {
    listed(cfg, account)
        .await
        .is_none_or(|models| models.iter().any(|m| m == model))
}

/// `zephyr.models`: each pooled account with its model and the models rpi lists on its
/// provider; without a pool, one unnamed entry with all of them (DESIGN §4.3.1).
pub async fn models(cfg: &Config) -> Value {
    let accounts: Vec<Option<&Account>> = match cfg.accounts.is_empty() {
        true => vec![None],
        false => cfg.accounts.iter().map(Some).collect(),
    };
    let lists = futures_util::future::join_all(accounts.iter().map(|a| listed(cfg, *a))).await;
    let entries: Vec<Value> = accounts
        .iter()
        .zip(lists)
        .map(|(a, listed)| {
            let model = a.map(|a| a.model.clone()).or_else(|| cfg.model.clone());
            let mut models: Vec<String> = (listed.unwrap_or_default().into_iter())
                .filter(|m| a.is_none_or(|a| m.split('/').next() == Some(a.provider())))
                .collect();
            if let Some(m) = model.as_ref().filter(|m| !models.contains(m)) {
                models.insert(0, m.clone());
            }
            json!({"name": a.map(|a| &a.name), "model": model, "models": models})
        })
        .collect();
    json!({"accounts": entries})
}

/// The `provider/model` ids rpi lists for `account` (rpi's agent dir without a pool), or
/// `None` when rpi could not say.
async fn listed(cfg: &Config, account: Option<&Account>) -> Option<Vec<String>> {
    let dir = match account {
        Some(a) => crate::pool::prepare(cfg, a).ok(),
        None => Some(crate::auth_keeper::agent_dir(cfg)),
    };
    let mut cmd = tokio::process::Command::new(&cfg.rpi);
    crate::agent::cmd(cmd.as_std_mut());
    cmd.arg("--list-models")
        .current_dir(&cfg.home)
        .kill_on_drop(true);
    for ext in &cfg.extensions {
        cmd.arg("--extension").arg(ext);
    }
    if let Some(dir) = dir {
        cmd.env("RPI_CODING_AGENT_DIR", dir);
    }
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await;
    let out = out.ok()?.ok().filter(|o| o.status.success())?;
    let listed = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            Some(format!("{}/{}", cols.next()?, cols.next()?))
        })
        .collect();
    Some(listed)
}

/// The account a child on `cfg` would run on: `Ok(None)` without a pool, `Err(until)` when
/// every account is cooling down.
fn pick_in(cfg: &Config, store: &Store) -> Result<Option<Account>, i64> {
    if cfg.accounts.is_empty() {
        return Ok(None);
    }
    let cooling = store.cooldowns().unwrap_or_else(|e| {
        eprintln!("zephyr: reading cooldowns failed: {e}");
        Default::default()
    });
    crate::pool::pick(&cfg.accounts, &cooling, crate::now()).map(|a| Some(a.clone()))
}

fn log(r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("zephyr: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(channel: &str) -> Origin {
        Origin {
            channel: channel.into(),
            id: 1,
            sender: "u".into(),
        }
    }

    fn busy(o: &Origin, tool: Option<&str>, done: u32) -> Run {
        Run {
            busy: true,
            since_unix_ms: Some(1_000),
            origin: Some(o.clone()),
            current_tool: tool.map(Into::into),
            tools_done: done,
        }
    }

    #[test]
    fn activities_follow_the_run() {
        let (d, i) = (origin("discord"), origin("imessage"));
        let mut tools = Vec::new();
        let progress = |tools: &[&str], current: Option<&str>| Activity::Progress {
            elapsed: Duration::from_secs(2),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            current: current.map(Into::into),
        };
        // Idle stays quiet, even on a tick.
        assert!(activities(&Run::default(), &Run::default(), &mut tools, 3_000).is_empty());
        // A run starts: Start, then a Progress tick (each tick repeats Start, which channels ignore while up).
        let a = busy(&d, None, 0);
        assert_eq!(
            activities(&Run::default(), &a, &mut tools, 3_000),
            vec![
                (d.clone(), Activity::Start),
                (d.clone(), progress(&[], None))
            ]
        );
        // Tools are collected in order, the same tool twice counts twice.
        let b = busy(&d, Some("bash"), 0);
        let c = busy(&d, Some("bash"), 1);
        activities(&a, &b, &mut tools, 3_000);
        assert_eq!(
            activities(&b, &c, &mut tools, 3_000)[1],
            (d.clone(), progress(&["bash", "bash"], Some("bash")))
        );
        // The next run serves another channel: the old one stops, the new one starts afresh.
        let e = busy(&i, None, 0);
        let out = activities(&c, &e, &mut tools, 3_000);
        assert_eq!(out[0], (d.clone(), Activity::Stop));
        assert_eq!(out[1], (i.clone(), Activity::Start));
        assert_eq!(out[2], (i.clone(), progress(&[], None)));
        // Settle clears the run.
        assert_eq!(
            activities(&e, &Run::default(), &mut tools, 3_000),
            vec![(i, Activity::Stop)]
        );
    }

    #[test]
    fn untag_reverses_tagged() {
        let row = |text: &str| InboxRow {
            id: 42,
            channel: "discord".into(),
            sender: "s".into(),
            text: text.into(),
            tries: 0,
        };
        for text in ["hi", "", "[a #1] nested\nlines"] {
            assert_eq!(
                untag(&tagged(&row(text))),
                Some(("discord", 42, text)),
                "{text:?}"
            );
        }
        assert_eq!(untag("plain text"), None);
        assert_eq!(untag("[chat #x] not a number"), None);
    }

    #[test]
    fn a_snapshot_is_pushed_only_when_it_changes() {
        let (tx, mut rx) = watch::channel(Snapshot::default());
        rx.mark_unchanged();
        assert!(!update(&tx, |s| s.run.tools_done = 0));
        assert!(!rx.has_changed().unwrap());
        assert!(update(&tx, |s| s.run.tools_done = 1));
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().run.tools_done, 1);
        assert!(!update(&tx, |s| s.run.tools_done = 1));
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn previews_fold_whitespace_and_cut_at_60_chars() {
        assert_eq!(preview("a\n\n b"), "a b");
        assert_eq!(preview(&"é".repeat(61)), format!("{}…", "é".repeat(60)));
        assert_eq!(preview(&"é".repeat(60)), "é".repeat(60));
    }
}
