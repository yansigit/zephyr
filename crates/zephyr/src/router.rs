//! Inbound → inbox → `main` child → outbox → channel, with crash recovery (DESIGN §4.2).

use crate::channel::{Channel, Inbound, Outbound};
use crate::config::Config;
use crate::rpc::{message_text, Child, MAIN_SESSION};
use crate::store::{InboxRow, Store};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;
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
    let mut router = Router {
        cfg,
        store: Store::open(&cfg.db_path())?,
        child: None,
        busy: false,
        resumes: 0,
        backoff: FIRST_BACKOFF,
        respawn_at: Instant::now(),
        unrouted: HashSet::new(),
    };
    let (tx, rx) = mpsc::channel(64);
    let runs = futures_util::future::try_join_all(channels.iter().map(|c| c.run(tx.clone())));
    tokio::select! {
        r = runs => r.map(drop),
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
}

async fn next_event(child: &mut Option<Child>) -> Option<Value> {
    match child {
        Some(c) => c.events.recv().await,
        None => std::future::pending().await,
    }
}

impl Router<'_> {
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
                ev = next_event(&mut self.child) => match ev {
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

    async fn spawn(&mut self) {
        match Child::spawn(self.cfg, MAIN_SESSION).await {
            Ok(child) => {
                eprintln!("zephyr: rpi child {:?} ready", child.pid());
                self.child = Some(child);
                self.busy = false;
                log(self.reconcile(None).await);
            }
            Err(e) => {
                eprintln!("zephyr: spawning rpi failed: {e:#}");
                self.schedule_respawn();
            }
        }
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
        if m.text.trim() == "/stop" {
            let r = self.call(json!({"type": "abort"})).await.map(drop);
            log(r);
            let row = InboxRow {
                id,
                channel: m.channel,
                sender: m.sender,
                text: m.text,
                tries: 0,
            };
            log(self
                .store
                .reply(&row, "[zephyr] stopped", "done")
                .map_err(Into::into));
            return;
        }
        log(self.dispatch_new().await);
    }

    /// Sends every `new` row: `prompt` when idle, rpi's own `queue_message` when busy.
    async fn dispatch_new(&mut self) -> anyhow::Result<()> {
        if self.child.is_none() {
            return Ok(()); // rows stay `new` until the child is back
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
        let fallback = match ev["error"].as_str() {
            Some(e) => format!(
                "[zephyr] run {}: {e}",
                ev["outcome"].as_str().unwrap_or("failed")
            ),
            None => "[zephyr] the run ended without a reply".into(),
        };
        log(self.reconcile(Some(fallback)).await);
    }

    /// Matches `dispatched` rows against the session: replies to the ones rpi has
    /// answered, resumes an interrupted run, and re-sends rows rpi never got (once).
    /// Runs after every settle and after every (re)spawn, so it is also crash recovery.
    async fn reconcile(&mut self, fallback: Option<String>) -> anyhow::Result<()> {
        let snap = self.call(json!({"type": "get_messages"})).await?;
        let rows = self.store.inbox_with_status("dispatched")?;
        if snap["pendingResume"] == true {
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
        self.dispatch_new().await
    }

    async fn flush<C: Channel>(&mut self, channels: &[C]) {
        let rows = match self.store.pending_outbox() {
            Ok(rows) => rows,
            Err(e) => return eprintln!("zephyr: outbox read failed: {e}"),
        };
        for row in rows {
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
                    log(self.reconcile(None).await);
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
