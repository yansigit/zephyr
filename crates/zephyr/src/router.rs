//! Inbound → inbox → `main` child → outbox → channel, with crash recovery (DESIGN §4.2).

use crate::channel::{Channel, Inbound, Outbound};
use crate::config::{Account, Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child, MAIN_SESSION};
use crate::store::{InboxRow, Store};
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
    let mut router = Router {
        cfg: shared,
        cfg_tx,
        edits: edits(cfg),
        secrets: crate::dotenv::read(&cfg.secrets_path()).unwrap_or_default(),
        pending_model: None,
        reexec: false,
        auth_stamp: None,
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
    };
    let runs = futures_util::future::try_join_all(channels.iter().map(|c| c.run(tx.clone())));
    tokio::select! {
        r = runs => r.map(|_| Exit::Shutdown),
        r = crate::attach::serve(cfg, tx, attach_tx) => r.map(|()| Exit::Shutdown),
        r = crate::scheduler::run(cfg_rx, &jobs) => r.map(|()| Exit::Shutdown),
        r = router.main(rx, channels, shutdown) => r,
    }
}

/// The mtimes a hand edit changes: `zephyr.toml` and `secrets.env`.
type Edits = (Option<SystemTime>, Option<SystemTime>);

fn edits(cfg: &Config) -> Edits {
    let config = cfg.path.as_deref().and_then(crate::reload::stamp);
    (config, crate::reload::stamp(&cfg.secrets_path()))
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
    /// The pooled account the child runs on (DESIGN §4.9). `None` without a pool.
    account: Option<Account>,
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
                    Some(_) => continue,
                    None => self.child_exited().await,
                },
                _ = tick.tick() => {
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
                    if self.cfg.path.is_some() && edits(&self.cfg) != self.edits {
                        let text = self.reload().await;
                        eprintln!("zephyr: zephyr.toml or secrets.env changed: {text}");
                    }
                }
            }
            self.flush(channels).await;
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
        }
        ev
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
                let wait = Duration::from_secs((until - now()).max(1) as u64);
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
        } else if let Some(reply) = self.command(text).await {
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
            self.show(&reply);
        }
        self.restart_if_idle().await;
        // A run in progress settles first; its settle reconciles and dispatches.
        if resume && !self.busy {
            log(self.reconcile(None, false).await);
        }
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
            } else {
                self.call(json!({"type": "prompt", "message": text}))
                    .await?;
                self.busy = true;
            }
        }
        Ok(())
    }

    async fn settled(&mut self, ev: &Value) {
        self.busy = false;
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
        for notice in [tally.alert(), memory].into_iter().flatten() {
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
        let now = now();
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
    async fn command(&mut self, text: &str) -> Option<String> {
        let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
        Some(match (cmd, arg.trim()) {
            ("/reload", "") => self.reload().await,
            ("/status", "") => self.status(),
            ("/model", "") => self.models(),
            ("/model", model) => self.switch_model(model).await,
            ("/new", "") => self.new_session().await,
            _ => return None,
        })
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
    async fn switch_model(&mut self, model: &str) -> String {
        let Some(path) = self.cfg.path.clone() else {
            return "[zephyr] no zephyr.toml to change".into();
        };
        let active = self.account.as_ref().map(|a| a.name.clone());
        let doc = match crate::host::conf::load(&path).and_then(|mut doc| {
            crate::reload::set_model(&mut doc, active.as_deref(), model)?;
            Ok(doc)
        }) {
            Ok(doc) => doc,
            Err(e) => return format!("[zephyr] model not changed: {e:#}"),
        };
        let target = crate::host::conf::accounts(&doc)
            .into_iter()
            .find(|(_, m)| m == model)
            .and_then(|(name, _)| self.cfg.accounts.iter().find(|a| a.name == name).cloned());
        if !lists(&self.cfg.clone(), target.as_ref(), model).await {
            return format!("[zephyr] rpi has no model {model}");
        }
        match crate::host::conf::save(&path, &doc) {
            Ok(()) => self.reload().await,
            Err(e) => format!("[zephyr] model not changed: {e:#}"),
        }
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
        let now = crate::auth_keeper::now_secs();
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
        // job output go to the clients attached now, or wait for the next one.
        for row in rows.iter().filter(|r| r.channel == crate::attach::CHANNEL) {
            if !row.client_id.starts_with('z') {
                if self.tap.receiver_count() == 0 {
                    continue;
                }
                self.show(&row.text);
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
                Err(e) if now() - row.created_at > retry_window(&row.channel) => {
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

    /// Shows `text` to attached clients as an assistant message, as `attach` draws history.
    fn show(&self, text: &str) {
        let m = json!({"role": "assistant", "content": [{"type": "text", "text": text}]});
        for kind in ["message_start", "message_end"] {
            let _ = self.tap.send(json!({"type": kind, "message": m})); // no receivers is fine
        }
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

/// Whether `rpi --list-models`, signed in as `account`, lists `model`. A listing that fails
/// says nothing, so it counts as yes. The running child lists only its own sign-in's models.
async fn lists(cfg: &Config, account: Option<&Account>, model: &str) -> bool {
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
    let Ok(Ok(out)) = out.map_err(drop) else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .any(|l| {
            let mut cols = l.split_whitespace();
            matches!((cols.next(), cols.next()), (Some(p), Some(m)) if format!("{p}/{m}") == model)
        })
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
    crate::pool::pick(&cfg.accounts, &cooling, now()).map(|a| Some(a.clone()))
}

fn log(r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("zephyr: {e:#}");
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
