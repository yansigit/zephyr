//! Inbound → inbox → `main` child → outbox → channel, with crash recovery (DESIGN §4.2).

use crate::channel::{Channel, Inbound, Outbound};
use crate::config::{Account, Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child, MAIN_SESSION};
use crate::store::{InboxRow, Store};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

/// rpi's own cap (`MAX_RESUME_ATTEMPTS`); after that we tell the sender.
const MAX_RESUMES: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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

/// Runs the gateway until `shutdown` resolves, then drains. Every channel feeds one main session.
pub async fn run<C: Channel>(
    cfg: &Config,
    channels: &[C],
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.home)?;
    let (tx, rx) = mpsc::channel(64);
    let (attach_tx, attaches) = mpsc::channel(4);
    let jobs = crate::scheduler::Wake::default();
    let mut router = Router {
        cfg,
        store: Store::open(&cfg.db_path())?,
        child: None,
        busy: false,
        resumes: 0,
        backoff: FIRST_BACKOFF,
        respawn_at: Instant::now(),
        unrouted: HashSet::new(),
        tap: broadcast::channel(1024).0,
        attaches,
        jobs: &jobs,
        auth_at: Instant::now(),
        restart: false,
        account: None,
    };
    let runs = futures_util::future::try_join_all(channels.iter().map(|c| c.run(tx.clone())));
    tokio::select! {
        r = runs => r.map(drop),
        r = crate::attach::serve(cfg, tx, attach_tx) => r,
        r = crate::scheduler::run(cfg, &jobs) => r,
        r = router.main(rx, channels, shutdown) => r,
    }
}

struct Router<'a> {
    cfg: &'a Config,
    store: Store,
    child: Option<Child>,
    /// A prompt or resume is running in the child.
    busy: bool,
    resumes: u32,
    backoff: Duration,
    respawn_at: Instant,
    /// Outbox channels with no configured adapter, so each is logged once.
    unrouted: HashSet<String>,
    /// Every child event, for attached clients.
    tap: broadcast::Sender<Value>,
    attaches: mpsc::Receiver<crate::attach::AttachRequest>,
    /// Shared with the scheduler, which runs jobs beside `main`.
    jobs: &'a crate::scheduler::Wake,
    /// The next AuthKeeper check (DESIGN §4.7).
    auth_at: Instant,
    /// A refresh or a failover needs the child restarted; done once it is idle.
    restart: bool,
    /// The pooled account the child runs on (DESIGN §4.9). `None` without a pool.
    account: Option<&'a Account>,
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
    ) -> anyhow::Result<()> {
        tokio::pin!(shutdown);
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
                }
            }
            self.flush(channels).await;
        }
        self.drain(channels).await;
        Ok(())
    }

    fn tapped(&self, ev: Option<Value>) -> Option<Value> {
        if let Some(ev) = &ev {
            let _ = self.tap.send(ev.clone()); // no receivers is fine
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
        match Child::spawn_as(self.cfg, MAIN_SESSION, account).await {
            Ok(child) => {
                eprintln!("zephyr: rpi child {:?} ready", child.pid());
                if let Some(a) = account {
                    eprintln!("zephyr: on account {} ({})", a.name, a.model);
                }
                self.child = Some(child);
                self.account = account;
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
    fn pick(&self) -> Result<Option<&'a Account>, i64> {
        if self.cfg.accounts.is_empty() {
            return Ok(None);
        }
        let cooling = self.store.cooldowns().unwrap_or_else(|e| {
            eprintln!("zephyr: reading cooldowns failed: {e}");
            Default::default()
        });
        crate::pool::pick(&self.cfg.accounts, &cooling, now()).map(Some)
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
        if let Some(reply) = crate::memory::command(&self.cfg.home, m.text.trim()) {
            let row = InboxRow {
                id,
                channel: m.channel,
                sender: m.sender,
                text: m.text,
                tries: 0,
            };
            log(self.store.reply(&row, &reply, "done").map_err(Into::into));
            return;
        }
        if let Some(reply) = crate::scheduler::command(&mut self.store, self.cfg, &m) {
            let row = InboxRow {
                id,
                channel: m.channel,
                sender: m.sender,
                text: m.text,
                tries: 0,
            };
            log(self.store.reply(&row, &reply, "done").map_err(Into::into));
            self.jobs.runs.notify_one();
            return;
        }
        let cmd = m.text.trim().to_string();
        if matches!(cmd.as_str(), "/stop" | "/pause" | "/resume") {
            let reply = self.control(&cmd).await;
            let row = InboxRow {
                id,
                channel: m.channel,
                sender: m.sender,
                text: m.text,
                tries: 0,
            };
            log(self.store.reply(&row, reply, "done").map_err(Into::into));
            // A run in progress settles first; its settle reconciles and dispatches.
            if cmd == "/resume" && !self.busy {
                log(self.reconcile(None, false).await);
            }
            return;
        }
        log(self.dispatch_new().await);
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

    /// Adds what rpi used since the last settle to today's spend. Returns true when this
    /// settle crossed a budget limit, and pauses the gateway for the rest of the UTC day.
    async fn record_spend(&mut self) -> anyhow::Result<bool> {
        let stats = self.call(json!({"type": "get_stats"})).await?;
        let total = (
            stats["totalTokens"].as_i64().unwrap_or(0),
            stats["costTotal"].as_f64().unwrap_or(0.0),
        );
        let last = self.store.last_stats()?;
        // rpi's totals are per session: a drop means a fresh session, so the whole total is new.
        let delta = if total.0 >= last.0 {
            (total.0 - last.0, total.1 - last.1)
        } else {
            total
        };
        self.store.set_last_stats(total)?;
        if let Some(a) = self.account {
            self.store.add_account_spend(&a.name, delta)?;
        }
        Ok(self.store.charge(self.cfg.budget.as_ref(), delta)?)
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
        self.backoff = FIRST_BACKOFF;
        log(crate::memory::commit_if_changed(&self.cfg.home).map(drop));
        let crossed = self.record_spend().await.unwrap_or_else(|e| {
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
            .zip(ev["error"].as_str())
            .and_then(|(a, e)| crate::pool::rate_limit(e).map(|resets| (a, e, resets)));
        match limited {
            Some((account, error, resets_at)) => {
                log(self.fail_over(account, error, resets_at).await)
            }
            None => {
                if let (Some(a), "completed") = (self.account, ev["outcome"].as_str().unwrap_or(""))
                {
                    log(self.store.clear_cooldown(&a.name).map_err(Into::into));
                }
                log(self.reconcile(Some(fallback), false).await);
            }
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

    /// Refreshes near-expiry OAuth credentials, then restarts the child once it is idle.
    async fn auth_check(&mut self) {
        let every = Duration::from_secs_f64((self.cfg.auth.refresh_minutes * 60.0).max(1.0));
        self.auth_at = Instant::now() + every;
        let now = crate::auth_keeper::now_secs();
        if !self.cfg.accounts.is_empty() {
            self.restart |= crate::auth_keeper::refresh_accounts(self.cfg, self.account, now).await;
        } else if let Some(child) = self.child.as_mut() {
            match crate::auth_keeper::refresh(child, self.cfg, now).await {
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
        // An attached client saw the reply live; a later attach sees it in the history.
        for row in rows.iter().filter(|r| r.channel == crate::attach::CHANNEL) {
            log(self
                .store
                .set_outbox_status(row.id, "sent")
                .map_err(Into::into));
        }
        for row in rows
            .into_iter()
            .filter(|r| r.channel != crate::attach::CHANNEL)
        {
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
            let status = match channel.send(&msg).await {
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

    /// SIGTERM: stop intake, let a running turn settle, flush, then stop the child.
    async fn drain<C: Channel>(&mut self, channels: &[C]) {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.busy {
            match tokio::time::timeout_at(deadline, next_event(&mut self.child)).await {
                Ok(Some(ev)) if ev["type"] == "agent_settled" => {
                    self.busy = false;
                    log(self.reconcile(None, false).await);
                }
                Ok(Some(_)) => {}
                _ => break, // child gone or out of time: recovery picks it up next start
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
