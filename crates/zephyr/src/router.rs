//! Inbound → inbox → `main` child → outbox → channel, with crash recovery (DESIGN §4.2).

use crate::channel::{Activity, Channel, Inbound, Outbound};
use crate::config::{Account, Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child, MAIN_SESSION};
use crate::settings::{Actor, Apply, Outcome, Setting};
use crate::store::{InboxRow, SessionOrigin, Store};
use crate::tools::Decided;
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
/// How long the router waits for rpi to answer an `abort`: the settle is what counts.
const ABORT_WAIT: Duration = Duration::from_secs(5);
/// A run cut this long ago (at most `stall_limit`) that never settled: the child is wedged and
/// is killed, and the run resumes as after a crash.
const WEDGED: Duration = Duration::from_secs(60);
/// After this many failed starts in a row the owner is told the agent cannot start.
const SPAWN_FAILURES: u32 = 3;

/// The prompt text for a row. The `#id` tag is what recovery looks for in `get_messages`.
pub fn tagged(row: &InboxRow) -> String {
    format!("[{} #{}] {}", row.channel, row.id, row.text)
}

/// What a row steered into a running turn says (DESIGN §4.1): its tagged text, and a note so
/// the model answers it and carries on, instead of taking it for a new request and stopping.
pub fn steered(row: &InboxRow) -> String {
    format!(
        "{}\n\n(This arrived while you were working on the request above: answer it in a line or \
         two, then carry on with that work.)",
        tagged(row)
    )
}

/// What a row says when it goes again because its run stopped after answering a steered
/// message. `NO_REPLY` (dropped, never sent) lets a model that had finished say so.
pub fn continued(row: &InboxRow) -> String {
    format!(
        "{}\n\n(The work on this request stopped after you answered a message that came in \
         meanwhile. If it is not finished, carry on with it. If you already answered it in full, \
         reply with exactly {NO_REPLY}.)",
        tagged(row)
    )
}

/// The reply a continued row gives when it had nothing left to say; it is not sent.
pub const NO_REPLY: &str = "NO_REPLY";

/// The reverse of `tagged`: `(channel, inbox id, text)`, or `None` for untagged text.
pub fn untag(text: &str) -> Option<(&str, i64, &str)> {
    let (channel, rest) = text.strip_prefix('[')?.split_once(" #")?;
    let (id, body) = rest.split_once("] ")?;
    Some((channel, id.parse().ok()?, body))
}

/// Where each exchange `/undo N` counts starts in `messages` (`get_messages`), oldest first: a
/// tagged user message, unless it follows a tool result (steered into the run, so part of its
/// exchange) or repeats an earlier row (a continuation). The TUI's rewind picker lists the same.
pub fn turns(messages: &[Value]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    (0..messages.len())
        .filter(|&i| {
            let m = &messages[i];
            let steered = i > 0 && matches!(messages[i - 1]["kind"].as_str(), Some("toolResult"));
            m["kind"] == "user"
                && !steered
                && untag(&message_text(m)).is_some_and(|(_, id, _)| seen.insert(id))
        })
        .collect()
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
    /// The model's context window in tokens, from rpi's `get_state`; `None` when rpi does not know it.
    pub context_window: Option<i64>,
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
    /// Proposals waiting for the owner's `/ok` or `/no`.
    pub proposals: u32,
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
pub(crate) fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// `/export`'s markdown of `get_messages`: each user and assistant message's text under who
/// said it, and a tool call as one line, its name and arguments.
fn markdown(session: &str, messages: &[Value]) -> String {
    let mut out = format!("# zephyr conversation {session}\n");
    for m in messages {
        match m["kind"].as_str() {
            Some("user") => {
                let text = message_text(m);
                let (who, body) =
                    untag(&text).map_or(("zephyr", text.as_str()), |(c, _, b)| (c, b));
                out.push_str(&format!("\n## {who}\n\n{body}\n"));
            }
            Some("assistant") => {
                let mut said = String::new();
                for b in m["content"].as_array().into_iter().flatten() {
                    match (b["type"].as_str(), b["text"].as_str()) {
                        (Some("text"), Some(text)) => said.push_str(&format!("\n{text}\n")),
                        (Some("toolCall"), _) => said.push_str(&format!(
                            "\n- `{}` {}\n",
                            b["name"].as_str().unwrap_or("tool"),
                            preview(&b["arguments"].to_string())
                        )),
                        _ => {}
                    }
                }
                if !said.is_empty() {
                    out.push_str(&format!("\n## agent\n{said}"));
                }
            }
            _ => {}
        }
    }
    out
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
    // Before main or a task resumes: a crashed daemon's children would hold their sessions open.
    crate::rpc::kill_leftovers(&cfg.rpi, &cfg.session_dir());
    let (tx, rx) = mpsc::channel(64);
    let (attach_tx, attaches) = mpsc::channel(4);
    let jobs = crate::scheduler::Wake::default();
    let (cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg.clone()));
    let (snap, snap_rx) = watch::channel(Snapshot::default());
    let activity_rx = snap_rx.clone();
    let (attach_cfgs, egress_cfgs) = (cfg_rx.clone(), cfg_rx.clone());
    let (gate_cfgs, gate_snap) = (cfg_rx.clone(), snap_rx.clone());
    let names = channels.iter().map(|c| c.name().to_string()).collect();
    let mut router = Router::new(cfg, &jobs, attaches, cfg_tx, snap, names)?;
    // The gate's held notes for main's run, steered in before the plugin is answered (R51).
    let (gate_notes, notes_rx) = mpsc::channel(16);
    router.gate_notes = Some(notes_rx);
    router.keep_upgraded_install_as_it_was().await;
    router.taint_cut_off();
    // Commands a re-exec handed over alive are adopted; the rest a dead gateway left running
    // are reported, never re-run (DESIGN §4.13).
    router.procs.adopt(&router.cfg, &mut router.store)?;
    // After the adoption, so an approved action whose command is still running stays running;
    // a delay that came due while down waits for the owner (approvals spec §4.9).
    crate::actions::restore(&router.cfg, &router.store, crate::now());
    for c in channels {
        c.live(cfg_rx.clone()); // owners for file downloads, kept current by reload
    }
    let waits = router.waits.clone();
    let runs = futures_util::future::try_join_all(channels.iter().map(|c| c.run(tx.clone())));
    tokio::select! {
        r = runs => r.map(|_| Exit::Shutdown),
        // Tasks' and jobs' sends, every connector call, the browser's clicks (approvals §4.2).
        r = crate::gate::serve(gate_cfgs, gate_snap, waits, &jobs, gate_notes) => r.map(|()| Exit::Shutdown),
        // The agent's only way out, in bridge mode (DESIGN §4.20).
        r = crate::egress::serve(egress_cfgs, &jobs) => r.map(|()| Exit::Shutdown),
        r = crate::attach::serve(cfg, tx, attach_tx, snap_rx, attach_cfgs) => r.map(|()| Exit::Shutdown),
        r = crate::scheduler::run(cfg_rx.clone(), &jobs) => r.map(|()| Exit::Shutdown),
        r = crate::tasks::run(cfg_rx, &jobs) => r.map(|()| Exit::Shutdown),
        r = show_activity(channels, activity_rx, cfg.tick) => r.map(|()| Exit::Shutdown),
        r = router.main(rx, channels, shutdown) => r,
    }
}

/// Shows the run on the channel it serves (typing, a progress note), from `Snapshot.run`.
/// A watch may skip states; diffing what it shows keeps Start and Stop paired anyway.
async fn show_activity<C: Channel>(
    channels: &[C],
    mut rx: watch::Receiver<Snapshot>,
    every: Duration,
) -> anyhow::Result<()> {
    let (mut prev, mut tools) = (Run::default(), Vec::new());
    let mut tick = tokio::time::interval(every);
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
/// How long a port stays unpublished before the owner is told (`Router::unpublished_notice`).
const UNPUBLISHED_WAIT: Duration = Duration::from_secs(60);

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
    /// The `/login` sign-ins whose URL was posted and that have not ended: `/login paste` feeds
    /// the sender's own, and the tick posts how each ended.
    posted: Vec<Login>,
    /// The keys whose port the container does not publish, since when, and whether the owner
    /// was told (`unpublished_notice`).
    unpublished: Option<(Vec<&'static str>, Instant, bool)>,
    /// `/login` starts still running: the tick drains them, so this loop never waits on rpi.
    starting: Vec<Starting>,
    /// The `/btw` side question being answered (`crate::btw::run`), one at a time.
    btw: Option<tokio::task::JoinHandle<()>>,
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
    /// Whether the current run of main is the owner's alone (DESIGN §4.6), decided at its start
    /// and lost at the first text that is not the owner's.
    trust: Trust,
    /// The run's conversation was tainted already: one `session_origin` write per run.
    run_taint: bool,
    /// Taints `session_origin` could not take, by session: they hold for this process's life.
    tainted: HashMap<String, String>,
    /// When the child last said anything, for stall detection (`cap_run`).
    last_event: Instant,
    /// `RESUME_NOTE` was steered into a resumed run and not delivered yet: taken back at the
    /// settle, so it never reaches the next run (DESIGN §4.2).
    note_pending: bool,
    /// Why the current run was stopped by a run limit; told to the owner at its settle.
    cut: Option<String>,
    /// When `cut` was set: a cut run that never settles is a wedged child (`cap_run`).
    cut_at: Option<Instant>,
    /// Failed child starts in a row, so the owner is told once (`SPAWN_FAILURES`).
    spawn_failures: u32,
    /// Draining for a shutdown: rows are written but never dispatched.
    stopping: bool,
    /// The main agent's `exec` commands (DESIGN §4.13).
    procs: crate::procs::Procs,
    /// Browser actions waiting in place for the owner's answer (approvals spec §4.4).
    waits: crate::actions::Waits,
    /// The last tick found the gateway paused: the next one holds what came due (I1).
    due_held: bool,
    /// The gate's held notes for main's run (R51), from `gate::serve`.
    gate_notes: Option<mpsc::Receiver<GateNote>>,
    /// Actions whose held note is in this run already, so the call's end steers it in no more.
    held_steered: HashSet<i64>,
    /// The newest note about an action main was woken for (R49).
    action_woken: i64,
    /// The pooled account the child runs on (DESIGN §4.9). `None` without a pool.
    account: Option<Account>,
    /// What attached clients see; `publish` refreshes it, `tapped` keeps `run` current.
    snap: watch::Sender<Snapshot>,
    /// The channel adapters this daemon runs.
    names: Vec<String>,
    /// rpi's thinking level and context window, read from `get_state` (`read_state`).
    thinking: Option<String>,
    context_window: Option<i64>,
    /// The owner's `/thinking` choice: rpi keeps the level neither in the session nor its
    /// settings, so each new child is set to it again.
    /// shortcut: lost when the daemon restarts; save it in zephyr.toml if that matters.
    thinking_choice: Option<String>,
    /// The last inbox row whose user turn went to attached clients. Rows go out in id order,
    /// so one sent again (after `/stop` cleared rpi's queue, or a fail-over) is not shown twice.
    shown: i64,
    /// Messages steered into the running turn (DESIGN §4.1), oldest first, each with whether
    /// rpi has delivered it yet. The agent's first text after a delivered one answers it.
    steered: Vec<(InboxRow, bool)>,
    /// This run's steered texts, so `reconcile` keeps the run's last text for the turn that
    /// started it, and the replies already sent for them, so none goes out twice.
    steered_texts: HashSet<String>,
    steer_sent: Vec<String>,
    /// A steered message was answered mid-run: flush now, not at the settle.
    steer_replied: bool,
    /// The latest thing the agent said in this run: the reason it gives for a setting change.
    last_say: String,
    /// The run that just settled ended `completed`: only such a run's rows may be continued.
    /// Set by `settled` for the one `reconcile` that follows; never after an abort or a failure.
    run_completed: bool,
    /// A `/compact` is running: `(channel, sender)` of who asked, told its summary at the settle.
    compacting: Option<(String, String)>,
    /// The agent's proposal the owner's `/ok` is running, so its setting change is logged as it.
    approving: Option<i64>,
}

/// The reply to `/undo`, `/retry` and `/compact` while a run is going: they need the agent idle.
const BUSY: &str = "[zephyr] the agent is busy: wait for its reply, or /stop it first";

/// A reload that applied live: `/set` leaves it out of its reply.
const APPLIED: &str = "[zephyr] applied";

const SET_USAGE: &str = "[zephyr] usage: /set KEY VALUE [for 2h]; /settings lists the keys";

const APPROVALS_USAGE: &str = "[zephyr] usage: /approvals, /approvals manual|semi|full|on|off \
    [for 2h], /approvals messages|posts|money|facts MODE [for 2h], /approvals forget N";

const SUBAGENT_USAGE: &str = "usage: /subagent, /subagent add NAME [PROVIDER/MODEL] PROMPT, \
    /subagent model NAME PROVIDER/MODEL|default, /subagent prompt NAME PROMPT, /subagent rm NAME";

/// How a saved setting took effect: the reload's answer (or the command's own reply), and
/// whether the reload failed, which leaves the file saved but the change not in effect.
struct Reloaded {
    text: String,
    failed: bool,
}

impl Reloaded {
    fn done(text: String) -> Self {
        Self {
            text,
            failed: false,
        }
    }

    fn failed(text: String) -> Self {
        Self { text, failed: true }
    }
}

/// A note the gate hands main's run (`Router::gate_note`): the session it is for, and the note.
pub type GateNote = (String, crate::scheduler::Note);

async fn next_gate_note(rx: &mut Option<mpsc::Receiver<GateNote>>) -> Option<GateNote> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_event(child: &mut Option<Child>) -> Option<Value> {
    match child {
        Some(c) => c.events.recv().await,
        None => std::future::pending().await,
    }
}

impl<'a> Router<'a> {
    /// A router for `cfg` with no child yet. `cfg_tx` holds the config it starts on.
    fn new(
        cfg: &Config,
        jobs: &'a crate::scheduler::Wake,
        attaches: mpsc::Receiver<crate::attach::AttachRequest>,
        cfg_tx: watch::Sender<Arc<Config>>,
        snap: watch::Sender<Snapshot>,
        names: Vec<String>,
    ) -> anyhow::Result<Self> {
        let shared = cfg_tx.borrow().clone();
        Ok(Router {
            cfg: shared,
            edits: edits(cfg),
            secrets: crate::dotenv::read(&cfg.secrets_path()).unwrap_or_default(),
            pending_model: None,
            reexec: false,
            auth_stamp: None,
            logins: Vec::new(),
            posted: Vec::new(),
            unpublished: None,
            starting: Vec::new(),
            btw: None,
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
            jobs,
            auth_at: Instant::now(),
            restart: false,
            account: None,
            tally: crate::rpc::Tally::main(&cfg.home),
            snap,
            names,
            thinking: None,
            context_window: None,
            thinking_choice: None,
            shown: 0,
            steered: Vec::new(),
            steered_texts: HashSet::new(),
            steer_sent: Vec::new(),
            steer_replied: false,
            last_say: String::new(),
            run_completed: false,
            run_start: None,
            trust: Trust::Not(crate::memory::LEGACY.into()),
            run_taint: false,
            tainted: HashMap::new(),
            last_event: Instant::now(),
            note_pending: false,
            cut: None,
            cut_at: None,
            spawn_failures: 0,
            stopping: false,
            procs: Default::default(),
            waits: Default::default(),
            due_held: false,
            gate_notes: None,
            held_steered: HashSet::new(),
            action_woken: 0,
            compacting: None,
            approving: None,
            cfg_tx,
        })
    }

    async fn main<C: Channel>(
        &mut self,
        mut rx: mpsc::Receiver<Inbound>,
        channels: &[C],
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<Exit> {
        tokio::pin!(shutdown);
        let mut exit = Exit::Shutdown;
        let mut tick = tokio::time::interval(self.cfg.tick);
        let paused = |store: &Store| store.pause_reason().ok().flatten().is_some();
        let mut was_paused = paused(&self.store);
        // Time limits that ended while the daemon was down, before anything is dispatched.
        self.revert_due().await;
        self.publish();
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep_until(self.respawn_at), if self.child.is_none() => {
                    self.spawn().await;
                }
                Some(m) = rx.recv() => self.inbound(m, false).await,
                Some(req) = self.attaches.recv() => self.attach(req).await,
                Some((session, note)) = next_gate_note(&mut self.gate_notes) => {
                    self.gate_note(&session, note).await
                }
                _ = self.jobs.outbox.notified() => {} // a job's output: flush below
                // The `/btw` answer is in the outbox: flush below.
                r = async { self.btw.as_mut().expect("guarded").await }, if self.btw.is_some() => {
                    if let Err(e) = r {
                        eprintln!("zephyr: /btw failed: {e}");
                    }
                    self.btw = None;
                }
                // A report for main (a task's, a background command's): a command still in the
                // turn moves to the background so the turn can read it.
                _ = self.jobs.inbox.notified() => {
                    self.procs.yield_all();
                    log(self.dispatch_new().await)
                }
                _ = self.procs.reported.notified() => {
                    self.procs.yield_all();
                    log(self.dispatch_new().await)
                }
                _ = tokio::time::sleep_until(self.auth_at), if (self.child.is_some() || !self.cfg.accounts.is_empty()) && self.cfg.auth.enabled => {
                    self.auth_check().await
                }
                ev = next_event(&mut self.child) => match self.tapped(ev).await {
                    Some(ev) if ev["type"] == "agent_settled" => self.settled(&ev).await,
                    Some(_) => {
                        self.cap_run().await;
                        // A steered message's reply goes out now, not at the settle.
                        if !std::mem::take(&mut self.steer_replied) {
                            continue;
                        }
                    }
                    None => self.child_exited().await,
                },
                _ = tick.tick() => {
                    // Liveness for `zephyr supervise` and `zephyr status`: a plain overwrite of
                    // `<unix time> <main child up: 0|1> <failed child starts in a row>`.
                    let beat = self.cfg.state_dir().join(crate::supervise::HEARTBEAT);
                    let up = self.child.is_some() as u8;
                    let line = format!("{} {up} {}\n", crate::now(), self.spawn_failures);
                    let _ = std::fs::write(beat, line);
                    self.cap_run().await;
                    self.login_starts().await;
                    self.login_notices();
                    self.unpublished_notice();
                    if self.cfg.path.is_some() && edits(&self.cfg) != self.edits {
                        let text = self.reload().await;
                        eprintln!("zephyr: zephyr.toml or secrets.env changed: {text}");
                    }
                    self.revert_due().await;
                    self.upgrade_notice();
                    // Delays that came due, and held actions nobody answered (approvals §4.6).
                    self.tick_actions();
                    // A budget pause ends at UTC midnight with nothing else to wake the router;
                    // a run in progress reconciles at its settle, a new child when it starts.
                    let ended = std::mem::replace(&mut was_paused, paused(&self.store));
                    if ended && !was_paused && !self.busy && self.child.is_some() {
                        log(self.reconcile(None, false).await);
                    }
                }
            }
            self.flush(channels).await;
            self.publish();
            // Tasks are not waited for: they resume after the re-exec (DESIGN §4.12).
            let background = self.jobs.running.load(Ordering::SeqCst);
            if self.reexec && !self.busy && self.btw.is_none() && !background {
                eprintln!("zephyr: restarting itself to apply the new settings");
                exit = Exit::Reexec;
                break;
            }
        }
        // A re-exec keeps the pid, so its commands stay our children: the new image adopts them.
        if exit != Exit::Reexec {
            self.procs.stop_all();
        }
        self.drain(channels, &mut rx).await;
        Ok(exit)
    }

    async fn tapped(&mut self, ev: Option<Value>) -> Option<Value> {
        if let Some(ev) = &ev {
            let _ = self.tap.send(ev.clone()); // no receivers is fine
            self.last_event = Instant::now();
            self.tally.add(ev);
            if let Some(reason) = self.tally.taint.clone() {
                self.taint_session(&reason);
            }
            let session = self.child.as_ref().and_then(|c| c.session.clone());
            if let Some(session) = &session {
                crate::scheduler::audit(&self.store, &self.cfg, session, ev);
                // The plugin refused a call the gate held, showing the model no reason. The gate
                // put the note in the run before it answered (R51); if that did not get through
                // in time, it goes in now, once (approvals spec §4.2), as the guard's note does.
                match crate::scheduler::held_note(&self.store, session, ev) {
                    Some((id, note)) if self.held_steered.insert(id) => {
                        self.steer_note(note).await;
                    }
                    _ => {}
                }
            }
            self.track(ev);
            match (ev["type"].as_str(), ev["message"]["role"].as_str()) {
                (Some("agent_start"), _) => self.last_say.clear(),
                (Some("message_end"), Some("assistant")) => {
                    let say = message_text(&ev["message"]);
                    if !say.trim().is_empty() {
                        self.last_say = say;
                    }
                }
                _ => {}
            }
            self.steer_event(ev);
            if let Some((id, command)) = crate::tools::gateway_call(ev) {
                // Off the list, the plugin has already refused the call: nobody waits for it.
                if let Some(text) = self.gateway_reply(command).await {
                    crate::tools::answer_gateway(&self.cfg, id, &text);
                }
            }
            if let Some((id, args)) = crate::procs::call(ev, "exec") {
                // Refused by the plugin's check or its veto: the call waits for nothing.
                let vetoed = matches!(
                    zephyr_policy::policy::check("exec", args),
                    zephyr_policy::policy::Decision::Deny(_)
                );
                if let (Ok(call), false) = (zephyr_policy::tools::exec_call(args), vetoed) {
                    // Vault tokens only with the owner's grant for every program it runs.
                    let env = match call.secrets.is_empty() {
                        true => Ok(Vec::new()),
                        false => crate::vault::exec_env(
                            &self.cfg,
                            &self.store,
                            &call.command,
                            &call.secrets,
                        ),
                    };
                    match (self.origin(), env) {
                        (Some(to), Ok(mut env)) => {
                            // What it runs asks the gate as main (the browser wrapper).
                            let session = session.as_deref().unwrap_or_default();
                            env.extend(crate::gate::agent_env(&self.cfg, session));
                            self.exec(id, args, call, to, env)
                        }
                        (None, _) => {
                            crate::scheduler::outbound_settled(&self.store, id, "error");
                            crate::tools::answer_gateway(&self.cfg, id, "no owner; not run")
                        }
                        (_, Err(text)) => {
                            crate::scheduler::outbound_settled(&self.store, id, "error");
                            crate::tools::answer_gateway(&self.cfg, id, &text)
                        }
                    }
                }
            }
            if let Some((id, args)) = crate::procs::call(ev, "vault") {
                // Main only, like `exec`; a call the plugin refused waits for nothing.
                if let Ok(action) = zephyr_policy::tools::vault_action(args) {
                    crate::vault::agent(&self.cfg, &self.store, id, action);
                }
            }
            if let Some((id, args)) = crate::procs::call(ev, "process") {
                if let Ok(action) = zephyr_policy::tools::process_action(args) {
                    let text = self.procs.reply(&self.cfg, &self.store, action);
                    crate::tools::answer_gateway(&self.cfg, id, &text);
                }
            }
            if let Some((id, args)) = crate::tools::task_call(ev) {
                // A call the plugin refused waits for nothing, so it gets no answer.
                if let Ok(action) = zephyr_policy::tools::task_action(args) {
                    let text = self.task_reply(action);
                    crate::tools::answer_gateway(&self.cfg, id, &text);
                }
            }
        }
        ev
    }

    /// `(channel, sender)` of the message this run serves: where a task or command reports.
    fn origin(&self) -> Option<(String, String)> {
        match self.snap.borrow().run.origin.clone() {
            Some(o) => Some((o.channel, o.sender)),
            None => self.store.last_owner().ok().flatten(),
        }
    }

    /// Main's `exec` call `id` once the veto and the vault passed: the gate decides it
    /// (approvals spec §4.2 path 1). Allowed, it starts; held or refused, the call is answered
    /// with the daemon's words and nothing starts. The gate sends the owner's card itself.
    fn exec(
        &self,
        id: &str,
        args: &Value,
        call: zephyr_policy::tools::ExecCall,
        to: (String, String),
        env: Vec<(String, String)>,
    ) {
        use crate::gate::Verdict;
        let session = self.child.as_ref().and_then(|c| c.session.clone());
        let since = self.snap.borrow().run.since_unix_ms.unwrap_or_default();
        let req = crate::gate::Request {
            session: session.unwrap_or_default(),
            call_id: Some(id.into()),
            tool: "exec".into(),
            args: args.clone(),
            browser: None,
            // R3: main's run key.
            run: format!("main:{since}"),
            // Cut to 200 by the gate, after its scrub: a cut here could split a value it hides.
            reason: Some(self.last_say.clone()).filter(|r| !r.trim().is_empty()),
        };
        // A held or refused call never ran: its outbound rows say so, not `sent` at its end.
        let settled = |outcome| crate::scheduler::outbound_settled(&self.store, id, outcome);
        match crate::gate::decide(&self.cfg, &self.store, &req, to.clone(), crate::now()) {
            Verdict::Allow => self.procs.start(&self.cfg, &self.store, id, call, to, env),
            Verdict::Hold { text, .. } => {
                settled("held");
                crate::tools::answer_gateway(&self.cfg, id, &text)
            }
            Verdict::Deny(text) => {
                settled("refused");
                crate::tools::answer_gateway(&self.cfg, id, &text)
            }
            // Only the browser wrapper's requests wait in place.
            Verdict::Wait { id: n } => {
                settled("held");
                crate::tools::answer_gateway(
                    &self.cfg,
                    id,
                    &format!("not run: #{n} waits for the owner"),
                )
            }
        }
    }

    /// What the held actions' lifecycle needs from the router (`crate::actions`).
    fn actions(&self) -> crate::actions::Ctx<'_> {
        crate::actions::Ctx {
            cfg: &self.cfg,
            store: &self.store,
            procs: &self.procs,
            waits: &self.waits,
            steers: &self.jobs.steers,
        }
    }

    /// The owner's `/ok N [once]` on `channel` for held action `id` (`actions::approve`).
    fn approve(&self, id: i64, once: bool, channel: &str) -> Result<String, String> {
        let by = crate::actions::By::Owner { channel };
        crate::actions::approve(&self.actions(), id, once, by, crate::now())
    }

    /// After a held action was settled: a waiting browser action whose action ended is denied,
    /// and a new note for main (a decline, a cancel, an expiry, a pass) is dispatched as a report
    /// is. Main is woken once per new note, not on every tick while one waits undelivered: each
    /// wake moves its running command to the background (R49). Returns whether it woke main.
    fn settled_actions(&mut self) -> bool {
        self.waits.deny_settled(&self.store);
        let newest = self.store.newest_action_row().unwrap_or(0);
        if newest <= self.action_woken {
            return false;
        }
        self.action_woken = newest;
        self.jobs.inbox.notify_one();
        true
    }

    /// Tells the owner, once, when the daemon listens on a port its container does not publish
    /// (DESIGN §4.20): zephyr.toml was edited by hand, and only `zephyr up` publishes a port. Not
    /// before `UNPUBLISHED_WAIT`: the host's `config set` and `channel` commands make the new
    /// container themselves, and that takes this daemon down before it has said anything.
    fn unpublished_notice(&mut self) {
        if !crate::egress::bridge() {
            return;
        }
        let made_with = std::env::var("ZEPHYR_PUBLISHED").ok();
        let keys = crate::egress::unpublished(&self.cfg, made_with.as_deref());
        if self.unpublished.as_ref().map(|u| &u.0) != Some(&keys) {
            self.unpublished = Some((keys, Instant::now(), false));
        }
        let Some((keys, since, told)) = &mut self.unpublished else {
            return;
        };
        if *told || since.elapsed() < UNPUBLISHED_WAIT {
            return;
        }
        *told = true;
        for key in keys.iter() {
            let text = format!("[zephyr] {key} changed; run zephyr up on the host to publish it");
            log(self.store.notify_last_owner(&text).map_err(Into::into));
        }
    }

    /// The tick's part for held actions (`actions::tick`): `held` while the gateway is paused,
    /// and on the first tick after a pause ended by itself, so nothing that came due meanwhile
    /// goes late without the owner (I1).
    fn tick_actions(&mut self) {
        let paused = self.store.pause_reason().map_or(true, |p| p.is_some());
        crate::actions::tick(&self.actions(), crate::now(), paused || self.due_held);
        self.due_held = paused;
        self.settled_actions();
    }

    /// The agent's `task` call (DESIGN §4.12), answered mid-turn. A started task reports back
    /// to the channel and sender this run serves.
    fn task_reply(&mut self, action: zephyr_policy::tools::TaskAction) -> String {
        use zephyr_policy::tools::TaskAction;
        match action {
            TaskAction::Start {
                goal,
                model,
                subagent,
            } => {
                let Some((channel, sender)) = self.origin() else {
                    return "no owner to report to; not started".into();
                };
                let cfg = self.cfg.clone();
                let sub = match subagent {
                    None => None,
                    Some(name) => match cfg.subagents.iter().find(|s| s.name == name) {
                        Some(s) => Some(s),
                        None => {
                            let names: Vec<&str> =
                                cfg.subagents.iter().map(|s| s.name.as_str()).collect();
                            return match names.is_empty() {
                                true => format!("not started: no subagent {name}; none are set up"),
                                false => format!(
                                    "not started: no subagent {name}; the subagents are {}",
                                    names.join(", ")
                                ),
                            };
                        }
                    },
                };
                // The agent's model, else the subagent's, else `[tasks] model`, else the account's.
                let from_sub = sub.and_then(|s| Some((s.model.as_deref()?, s.name.as_str())));
                let (model, from) = match (model.as_deref(), from_sub, cfg.task_model()) {
                    (Some(m), _, _) => (Some(m), String::new()),
                    (None, Some((m, name)), _) => (
                        Some(m),
                        format!(" (subagent {name}'s model in zephyr.toml)"),
                    ),
                    (None, None, Some(m)) => (Some(m), " ([tasks] model in zephyr.toml)".into()),
                    (None, None, None) => (None, String::new()),
                };
                // shortcut: no rpi --list-models check mid-turn; an unknown model fails the task
                // at start and its report says so. Add a check if agents keep guessing wrong.
                if let Some(m) = model {
                    let provider = m.split('/').next().unwrap_or_default();
                    let pool = &cfg.accounts;
                    if !pool.is_empty() && !pool.iter().any(|a| a.provider() == provider) {
                        let have: Vec<String> = pool
                            .iter()
                            .map(|a| format!("{} ({})", a.name, a.model))
                            .collect();
                        return format!(
                            "not started: no account on {provider}{from}; the accounts are {}",
                            have.join(", ")
                        );
                    }
                }
                let sub = sub.map(|s| (s.name.as_str(), s.prompt.as_str()));
                match self.store.add_task(&goal, model, sub, &channel, &sender) {
                    Ok(Some(id)) => {
                        self.jobs.tasks.notify_one();
                        format!(
                            "task #{id} started; its report comes back to you as a message when \
                             it ends. Tell the owner it is under way and end your turn."
                        )
                    }
                    Ok(None) => format!(
                        "not started: {} tasks are queued or running; stop one or wait",
                        crate::store::MAX_OPEN_TASKS
                    ),
                    Err(e) => format!("not started: {e}"),
                }
            }
            TaskAction::List => crate::tasks::list(&self.store),
            TaskAction::Stop(id) => crate::tasks::stop(&mut self.store, self.jobs, Some(id)),
        }
    }

    /// The answer to a command the agent sent with `gateway` (DESIGN §4.11), checked again here:
    /// the plugin's check is the agent's side of the boundary, this is the daemon's. A read is
    /// the same text the owner would get; none restarts or queues anything mid-turn. An action is
    /// only stored, for the owner's `/ok`. `None`, and nothing run, for anything else.
    async fn gateway_reply(&mut self, command: &str) -> Option<String> {
        use zephyr_policy::tools::Gateway;
        match zephyr_policy::tools::gateway_kind(command) {
            None => {
                // A malformed `/facts add` holds a value: never in the log.
                let shown = crate::facts::redact(command);
                eprintln!("zephyr: gateway {shown:?} is not on the list; not run");
                return None;
            }
            // Subagents, rules, goals and the persona are settings: proposed through
            // `set_setting`, so the card and the owner's `/ok` check them by content (settings
            // spec §4.4).
            Some(Gateway::Propose)
                if ["/subagent ", "/rule ", "/goal ", "/persona "]
                    .iter()
                    .any(|p| command.starts_with(p)) =>
            {
                return Some(self.agent_set(command).await)
            }
            Some(Gateway::Propose) if command.starts_with("/facts ") => {
                return Some(self.agent_facts(command))
            }
            Some(Gateway::Propose) => {
                return Some(crate::tools::propose_command(&self.store, command))
            }
            Some(Gateway::Set) => return Some(self.agent_set(command).await),
            Some(Gateway::Read) => {}
        }
        Some(match command {
            "/status" => self.status(),
            "/model" => self.models(),
            "/switch" => self.list_sessions(),
            "/facts" => (crate::facts::load(&crate::facts::path(&self.cfg)))
                .map_or_else(|e| format!("{e:#}"), |f| crate::facts::names(&f)),
            "/approvals" => format!("[zephyr] {}", crate::approvals::effective(&self.cfg).line()),
            settings if settings.starts_with("/settings") => {
                self.settings_listing(settings["/settings".len()..].trim())
            }
            // The owner's text, the daemon's words; `gateway_kind` keeps `grants` out.
            activity if activity.starts_with("/activity") => crate::activity::chat(
                &self.cfg,
                &self.store,
                activity["/activity".len()..].trim(),
                crate::now(),
            ),
            "/job list" => crate::scheduler::list(&self.store)
                .unwrap_or_else(|e| format!("[zephyr] listing jobs failed: {e}")),
            memory => crate::memory::command(&self.cfg.home, self.cfg.state_dir(), memory)
                .unwrap_or_default(),
        })
    }

    /// The agent's `/facts …`: an `add` is saved at once (a new fact only makes calls stricter)
    /// and the owner is told its name, never its value; `rm` and `allow` loosen the check, so
    /// they are stored for the owner's `/ok` from a strong channel (`not_now`).
    fn agent_facts(&mut self, command: &str) -> String {
        let arg = command.strip_prefix("/facts ").unwrap_or_default();
        let Some(add) = arg.strip_prefix("add ") else {
            return crate::tools::propose_command(&self.store, command);
        };
        match crate::facts::agent_add(&crate::facts::path(&self.cfg), add) {
            Ok(said) => {
                let name = add.split(' ').next().unwrap_or_default();
                self.tell_owner(&format!(
                    "[zephyr] the agent saved a private fact named {name}; calls holding it \
                     follow approvals.facts. /facts lists the facts, /facts rm {name} removes it"
                ));
                said
            }
            Err(why) => format!("refused: {why}"),
        }
    }

    /// The agent's `/set KEY VALUE [for D]`, `/subagent …` and `/rule …`: through `set_setting`
    /// as the agent, which applies a tightening at once and stores everything else for the
    /// owner. The reason is what the agent said last in this run.
    async fn agent_set(&mut self, command: &str) -> String {
        let (cmd, arg) = command.split_once(' ').unwrap_or((command, ""));
        let (key, raw, until) = match cmd {
            "/set" => match crate::settings::set_args(arg) {
                Some(parts) => parts,
                None => return "refused: usage /set KEY VALUE [for D]".into(),
            },
            // `/rule rm 2` and `/subagent add …` are the list settings' own values, shown whole
            // on the owner's card.
            _ if command.chars().count() > zephyr_policy::tools::CARD_CHARS => {
                return format!(
                    "refused: the text is longer than the owner's card shows ({} characters)",
                    zephyr_policy::tools::CARD_CHARS
                )
            }
            other => (other.trim_start_matches('/'), arg, None),
        };
        let until = match until
            .map(|d| crate::settings::parse_until(d, self.cfg.min_until))
            .transpose()
        {
            Ok(until) => until,
            Err(e) => return format!("refused: {e}"),
        };
        let session = self.child.as_ref().and_then(|c| c.session.clone());
        let session = session.unwrap_or_default();
        let reason = self.last_say.clone();
        let reason = (!reason.is_empty()).then_some(reason);
        let actor = Actor::Agent { session: &session };
        match self
            .set_setting(key, raw, actor, until, reason.as_deref())
            .await
        {
            Outcome::Changed { old, new, .. } => {
                let (old, new) = (
                    crate::settings::shown(old.as_deref()),
                    crate::settings::shown(new.as_deref()),
                );
                format!("changed {key} {old} → {new}")
            }
            Outcome::Proposed { id } => {
                format!("proposed as #{id}; it runs only if the owner approves with /ok {id}")
            }
            Outcome::Refused(why) => format!("refused: {why}"),
        }
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

    /// Follows steered messages through the run: rpi's user message marks one delivered, and
    /// the agent's next text answers it at once instead of at the settle.
    fn steer_event(&mut self, ev: &Value) {
        if ev["type"] != "message_end" {
            return;
        }
        let m = &ev["message"];
        let text = message_text(m);
        if m["role"] == "user" && text == crate::scheduler::RESUME_NOTE {
            self.note_pending = false;
        }
        if self.steered.is_empty() {
            return;
        }
        match m["role"].as_str() {
            Some("user") => {
                let pending = self.steered.iter_mut();
                if let Some(s) = pending
                    .filter(|(_, done)| !done)
                    .find(|(r, _)| steered(r) == text)
                {
                    s.1 = true;
                }
            }
            Some("assistant") if !text.is_empty() => {
                if let Some(i) = self.steered.iter().position(|(_, delivered)| *delivered) {
                    let (row, _) = self.steered.remove(i);
                    log(self
                        .store
                        .reply(&row, &unspoof(&text), "done")
                        .map_err(Into::into));
                    self.steer_sent.push(text);
                    self.steer_replied = true;
                }
            }
            _ => {}
        }
    }

    /// A restart loses the steer state: rebuilt from the store (`inbox.sent_as`), so the run's
    /// own row is not given a steered answer again (DESIGN §4.1). A no-op while it is held.
    fn restore_steers(&mut self) -> anyhow::Result<()> {
        if !self.steered_texts.is_empty() {
            return Ok(());
        }
        for (row, reply) in self.store.steered_into_run()? {
            self.steered_texts.insert(steered(&row));
            match reply {
                Some(reply) => self.steer_sent.push(reply),
                None => self.steered.push((row, false)),
            }
        }
        Ok(())
    }

    fn forget_steers(&mut self) {
        self.steered.clear();
        self.steered_texts.clear();
        self.steer_sent.clear();
        self.held_steered.clear();
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
                // Its `/rename` name, which the TUI's header shows.
                session: {
                    let id = self.store.main_session()?;
                    self.store.session_names()?.remove(&id).unwrap_or(id)
                },
                account: self.account.as_ref().map(|a| a.name.clone()),
                model: (self.account.as_ref().map(|a| a.model.clone()))
                    .or_else(|| self.cfg.model.clone()),
                thinking: self.thinking.clone(),
                context_window: self.context_window,
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
                proposals: self.store.pending_proposals()?.len() as u32,
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
        use crate::attach::AttachRequest;
        match req {
            AttachRequest::History(tx) => {
                let history = self.call(json!({"type": "get_messages"})).await;
                let history = history.unwrap_or_else(|e| {
                    eprintln!("zephyr: no history for attach: {e:#}");
                    Value::Null
                });
                let _ = tx.send((history, self.tap.subscribe()));
            }
            AttachRequest::FollowUp(m) => self.inbound(m, true).await,
            AttachRequest::Dequeue(tx) => {
                let _ = tx.send(self.dequeue().unwrap_or_else(|e| {
                    eprintln!("zephyr: dequeue failed: {e}");
                    vec![]
                }));
            }
            AttachRequest::Settings(tx) => {
                let _ = tx.send(self.settings_json());
            }
            AttachRequest::Goals(tx) => {
                let _ = tx.send(self.goals_json());
            }
            AttachRequest::SettingsSet {
                key,
                value,
                until,
                reply,
            } => {
                let _ = reply.send(self.settings_set(&key, &value, until.as_deref()).await);
                self.publish();
            }
            AttachRequest::Proposals(tx) => {
                let _ = tx.send(self.proposals_json());
            }
            AttachRequest::Activity { query, reply } => {
                let answer = crate::activity::answer(&self.cfg, &self.store, &query, crate::now());
                let _ = reply.send(answer);
            }
            AttachRequest::Decide { id, verb, reply } => {
                let _ = reply.send(self.decide(id, &verb).await);
                self.publish();
            }
        }
    }

    /// `zephyr.settings`: every setting by section, as `/settings` shows them.
    fn settings_json(&self) -> Value {
        let mut names: Vec<&str> = vec![];
        for s in crate::settings::SETTINGS {
            if !names.contains(&s.section) {
                names.push(s.section);
            }
        }
        let strong = &self.cfg.settings.strong_channels;
        // `semi (modified)` (approvals spec §4.3): a hand-set class differs from the preset.
        let modified = crate::approvals::effective(&self.cfg).modified;
        let sections: Vec<Value> = (names.into_iter())
            .map(|name| {
                let settings: Vec<Value> = (crate::settings::SETTINGS.iter())
                    .filter(|s| s.section == name)
                    .flat_map(|s| {
                        self.setting_rows(s).into_iter().map(move |r| {
                            let until = self.store.revert(&r.key).ok().flatten().map(|v| v.at);
                            json!({
                                "key": r.key,
                                "label": crate::settings::label(s, &r.key),
                                "help": s.help,
                                "kind": crate::settings::kind_json(&s.kind),
                                "value": self.shown(&r.key),
                                "source": r.source,
                                "access": crate::settings::access_name(s),
                                "strong": s.strong,
                                "until": until,
                                "modified": modified && r.key == "approvals.preset",
                            })
                        })
                    })
                    .collect();
                json!({"name": name, "settings": settings})
            })
            .collect();
        json!({"sections": sections, "strong_channels": strong})
    }

    /// `zephyr.goals`: the open goals in file order, then those done since the last review,
    /// newest first. Changes go through the `/goal` commands, as text.
    fn goals_json(&self) -> Result<Value, String> {
        let failed = |e: String| format!("reading the goals failed: {e}");
        let open = crate::memory::read_goals(self.cfg.state_dir())
            .map_err(|e| failed(format!("{e:#}")))?;
        let since = crate::review::since(&self.store, crate::now());
        let done = self
            .store
            .goals_done_since(since)
            .map_err(|e| failed(e.to_string()))?;
        let open = (open.iter().enumerate())
            .map(|(i, text)| json!({"n": i + 1, "text": text, "done": false}));
        let done = (done.iter().rev()).map(|d| json!({"n": null, "text": d.text, "done": true}));
        Ok(json!({"goals": open.chain(done).collect::<Vec<_>>()}))
    }

    /// `zephyr.settings_set`: `/set KEY VALUE [for D]` as the owner on the TUI, answered as data.
    async fn settings_set(&mut self, key: &str, value: &str, until: Option<&str>) -> Value {
        let refused = |why: String| json!({"outcome": "refused", "reason": why});
        if key == "facts" {
            return refused("change private facts with /facts".into());
        }
        if matches!(key, "subagent" | "rule") {
            return refused("change subagents with /subagent and rules with /rule".into());
        }
        if matches!(key, "goal" | "persona") {
            return refused("change goals with /goal and the persona with /persona".into());
        }
        let min = self.cfg.min_until;
        let until = match until
            .map(|d| crate::settings::parse_until(d, min))
            .transpose()
        {
            Ok(until) => until,
            Err(e) => return refused(e),
        };
        let owner = Actor::Owner {
            channel: crate::attach::CHANNEL,
        };
        match self.set_setting(key, value, owner, until, None).await {
            Outcome::Changed { old, new, applied } => {
                let at = until
                    .and(self.store.revert(key).ok().flatten())
                    .map(|r| r.at);
                json!({"outcome": "changed", "key": key, "old": old, "new": new,
                       "applied": applied, "until": at})
            }
            Outcome::Proposed { id } => json!({"outcome": "proposed", "id": id}),
            Outcome::Refused(why) => refused(why),
        }
    }

    /// `zephyr.proposals`: what waits for the owner, newest first. A held action carries its
    /// class, its summary and the card the chat was sent, in the gate's words (`gate::card`);
    /// `once` says whether `/ok N once` differs from `/ok N` for it now (its class asks only
    /// about new targets), and `strong` whether only a strong channel may approve it.
    fn proposals_json(&self) -> Value {
        let now = crate::now();
        let list = self.store.pending_proposals().unwrap_or_else(|e| {
            eprintln!("zephyr: reading proposals failed: {e}");
            vec![]
        });
        let e = crate::approvals::effective(&self.cfg);
        let money_strong = (crate::approvals::strong_only(&self.cfg).iter()).any(|s| s == "money");
        let proposals: Vec<Value> = (list.into_iter())
            .map(|p| {
                // An action's stored summary, until its card is read below (R70).
                let card = match p.kind.as_str() {
                    "command" => p.command.clone(),
                    _ => crate::facts::scrubbed(&self.cfg, p.command.clone()),
                };
                let mut v = json!({
                    "id": p.id,
                    "card": card,
                    "kind": if p.kind != "command" {
                        p.kind.as_str()
                    } else if p.key.is_some() {
                        "setting"
                    } else {
                        "command"
                    },
                    "status": p.status,
                    "due_at": p.due_at,
                    "key": p.key,
                    "age_secs": (now - p.created_at).max(0),
                    "expires_in_secs": (p.created_at + 86_400 - now).max(0),
                    "strong": p.strong,
                });
                let action = (p.kind != "command")
                    .then(|| self.store.action(p.id).ok().flatten())
                    .flatten();
                if let Some(a) = action {
                    // A browser action waits 10 minutes, not a day (spec §4.6).
                    if a.kind == crate::store::ActionKind::Wait {
                        let wait = 60 * i64::from(crate::gate::WAIT_MINUTES);
                        v["expires_in_secs"] = json!((a.created_at + wait - now).max(0));
                    }
                    v["card"] = json!(crate::gate::card(&a, &self.cfg, &self.store));
                    // R70: the summary as the private facts are now.
                    v["summary"] = json!(crate::facts::scrubbed(&self.cfg, a.summary.clone()));
                    v["strong"] = json!(a.class == "money" && money_strong);
                    v["once"] = json!(crate::gate::asks_new(&e, &a.class));
                    v["class"] = json!(a.class);
                }
                v
            })
            .collect();
        json!({"proposals": proposals})
    }

    /// `zephyr.decide`: `/ok N`, `/no N` or `/cancel N` from the TUI, through the checks the chat
    /// commands make (stale, strong channel). `once` is `ok` for a setting.
    async fn decide(&mut self, id: i64, verb: &str) -> Value {
        if verb == "cancel" {
            let done = crate::actions::cancel(&self.cfg, &self.store, &self.waits, &id.to_string());
            self.settled_actions();
            let (ok, text) = match done {
                Ok(text) => (true, text),
                Err(text) => (false, text),
            };
            return json!({"ok": ok, "text": text});
        }
        let text = match verb {
            "no" => format!("/no {id}"),
            "once" => format!("/ok {id} once"),
            _ => format!("/ok {id}"),
        };
        let m = Inbound {
            channel: crate::attach::CHANNEL.into(),
            platform_msg_id: String::new(),
            sender: "token".into(),
            text: text.clone(),
        };
        let mut approving = None;
        let decided = crate::tools::decide(&self.cfg, &self.store, &text, |p| {
            approving = Some(p.id);
            self.not_now(p, &m.channel)
        });
        let (ok, reply) = match decided {
            Some(Ok(Decided::Action { id, once })) => {
                let done = self.approve(id, once, &m.channel);
                self.settled_actions();
                match done {
                    Ok(reply) => (true, reply),
                    Err(reply) => (false, reply),
                }
            }
            Some(Ok(Decided::Command(command))) => {
                self.approving = approving;
                let run = Inbound {
                    text: command.clone(),
                    ..m.clone()
                };
                let reply = self.owner_command(&command, &run).await;
                self.approving = None;
                let reply = reply.unwrap_or_default();
                self.restart_if_idle().await;
                (true, format!("[zephyr] {text} approved {command}\n{reply}"))
            }
            Some(Err(reply)) => {
                self.settled_actions();
                (reply.starts_with("[zephyr] dropped #"), reply)
            }
            None => (false, format!("[zephyr] {text} is not a decision")),
        };
        json!({"ok": ok, "text": reply.trim_end()})
    }

    /// An approved or typed command that the daemon answers itself; `None` is a message for the agent.
    async fn owner_command(&mut self, text: &str, m: &Inbound) -> Option<String> {
        if let Some(reply) = crate::memory::command(&self.cfg.home, self.cfg.state_dir(), text) {
            return Some(reply);
        }
        if let Some(reply) = crate::scheduler::command(&mut self.store, &self.cfg, m) {
            self.jobs.runs.notify_one();
            return Some(reply);
        }
        if let Some(reply) = self.command(text, (&m.channel, &m.sender)).await {
            return Some(reply);
        }
        if matches!(text, "/stop" | "/pause" | "/resume") {
            return Some(self.control(text).await);
        }
        None
    }

    /// `zephyr.dequeue` (DESIGN §4.1): the attached client's messages rpi has not been sent yet,
    /// oldest first, withdrawn so the owner can edit them. Steered ones are already rpi's.
    fn dequeue(&mut self) -> rusqlite::Result<Vec<String>> {
        let mut texts = vec![];
        for row in self.store.inbox_with_status("new")? {
            if row.channel == crate::attach::CHANNEL && row.tries == 0 {
                self.store.set_status(row.id, "withdrawn")?;
                texts.push(row.text);
            }
        }
        Ok(texts)
    }

    /// Reads the thinking level and context window from rpi's `get_state` (0 is unknown).
    async fn read_state(&mut self) {
        let state = self.call(json!({"type": "get_state"})).await.ok();
        let state = state.as_ref();
        self.thinking = state.and_then(|s| s["thinkingLevel"].as_str().map(Into::into));
        self.context_window = state
            .and_then(|s| s["context"]["window"].as_i64())
            .filter(|&w| w > 0);
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
        match Child::spawn_as(&self.cfg, Some(&session), account.as_ref()).await {
            Ok(child) => {
                eprintln!("zephyr: rpi child {:?} ready", child.pid());
                self.spawn_failures = 0;
                if let Some(a) = &account {
                    eprintln!("zephyr: on account {} ({})", a.name, a.model);
                }
                self.child = Some(child);
                self.account = account;
                self.pending_model = None;
                self.auth_stamp = self.auth_mtime();
                self.busy = false;
                self.read_state().await;
                if let Some(level) = self.thinking_choice.clone() {
                    if self.thinking.as_ref() != Some(&level) {
                        log(self.set_thinking(&level).await.map(drop));
                    }
                }
                log(self.reconcile(None, false).await);
            }
            Err(e) => {
                eprintln!("zephyr: spawning rpi failed: {e:#}");
                self.spawn_failures += 1;
                if self.spawn_failures == SPAWN_FAILURES {
                    let text = format!("[zephyr] the agent cannot start: {e:#}; messages wait");
                    // Whoever waits for an answer, else whoever had the last one.
                    let waiting = self.store.inbox_with_status("new").ok();
                    let told = match waiting.as_ref().and_then(|w| w.first()) {
                        Some(row) => self.store.notify(&row.channel, &row.sender, &text),
                        None => self.store.notify_last_owner(&text),
                    };
                    log(told.map_err(Into::into));
                }
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
        self.cut_at = None;
        if let Some((channel, to)) = self.compacting.take() {
            let text = "[zephyr] /compact was cut off: the agent restarted";
            log(self.store.notify(&channel, &to, text).map_err(Into::into));
        }
        self.schedule_respawn();
    }

    fn schedule_respawn(&mut self) {
        self.respawn_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    /// `after`: a follow-up, held until the run settles rather than steered into it.
    async fn inbound(&mut self, m: Inbound, after: bool) {
        if m.text.trim().is_empty() {
            return; // attachment-only messages are not handled in v1
        }
        // Decided before the write: a crash in between must not leave a stranger's row `new`.
        let owner = self.cfg.is_owner(&m.channel, &m.sender);
        let status = if owner { "new" } else { "dropped" };
        let id = match self.store.insert_inbox_as(&m, status) {
            Ok(Some(id)) => id,
            Ok(None) => return, // duplicate platform id
            Err(e) => return eprintln!("zephyr: inbox write failed: {e}"),
        };
        if !owner {
            eprintln!(
                "zephyr: dropped message {id} from unknown sender {:?} on {}",
                m.sender, m.channel
            );
            return;
        }
        if after {
            log(self.store.deliver_after(id).map_err(Into::into));
        }
        let mut approving = None;
        let decided = crate::tools::decide(&self.cfg, &self.store, m.text.trim(), |p| {
            approving = Some(p.id);
            self.not_now(p, &m.channel)
        });
        // A held action is carried out here, its answer the whole reply.
        let decided = decided.map(|d| match d {
            Ok(Decided::Command(command)) => Ok(command),
            Ok(Decided::Action { id, once }) => {
                Err(self.approve(id, once, &m.channel).unwrap_or_else(|e| e))
            }
            Err(reply) => Err(reply),
        });
        if decided.is_some() {
            self.settled_actions();
        }
        // An approved proposal runs exactly as if the owner had typed it, logged as the agent's.
        self.approving = approving.filter(|_| matches!(decided, Some(Ok(_))));
        let text = match &decided {
            Some(Ok(command)) => command.as_str(),
            _ => m.text.trim(),
        };
        let reply = if let Some(Err(reply)) = &decided {
            reply.clone()
        } else if let Some(reply) = self.owner_command(text, &m).await {
            reply
        } else {
            self.approving = None;
            // The owner spoke: a command still in the turn moves to the background (DESIGN §4.13).
            self.procs.yield_all();
            return log(self.dispatch_new().await);
        };
        self.approving = None;
        // What `/ok N` ran, in the daemon's words: the agent's request may have said otherwise.
        let reply = match &decided {
            Some(Ok(command)) => format!("[zephyr] {} approved {command}\n{reply}", m.text.trim()),
            _ => reply,
        };
        let resume = text == "/resume";
        let row = InboxRow {
            id,
            channel: m.channel,
            sender: m.sender,
            text: m.text,
            tries: 0,
            platform_id: m.platform_msg_id,
        };
        if reply.is_empty() {
            // A `/btw` whose answer its task posts.
            log(self.store.set_status(id, "done").map_err(Into::into));
        } else {
            log(self.store.reply(&row, &reply, "done").map_err(Into::into));
        }
        if row.channel == crate::attach::CHANNEL && !reply.is_empty() {
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
        let Some(start) = self.run_start else {
            return;
        };
        if let Some(at) = self.cut_at {
            if at.elapsed() >= WEDGED.min(self.cfg.stall_limit) {
                eprintln!("zephyr: rpi never settled the stopped run; killing it");
                self.child = None; // dropping it kills it
                self.child_exited().await;
            }
            return;
        }
        if self.guard_run().await {
            return;
        }
        let budget = self.cfg.budget.as_ref();
        let over = budget.and_then(|b| b.run_over(self.tally.turns, start.elapsed()));
        let over = over.map(|why| format!("[zephyr] stopped the run after {why}"));
        let Some(notice) = over.or_else(|| self.stalled(start)) else {
            return;
        };
        eprintln!("zephyr: {notice}");
        self.cut = Some(notice);
        self.cut_at = Some(Instant::now());
        self.abort().await;
    }

    /// Asks rpi to abort the run, waiting at most `ABORT_WAIT`: a wedged child never answers.
    async fn abort(&mut self) {
        match tokio::time::timeout(ABORT_WAIT, self.call(json!({"type": "abort"}))).await {
            Ok(r) => log(r.map(drop)),
            Err(_) => eprintln!("zephyr: rpi did not answer the abort"),
        }
    }

    /// The owner's notice when the run counts as stuck: no rpi event for `cfg.stall_limit`, twice
    /// that while a tool runs, since rpi reports nothing between a silent command's start and
    /// end (DESIGN §4.4).
    fn stalled(&self, run_start: Instant) -> Option<String> {
        let in_tool = self.snap.borrow().run.current_tool.is_some();
        let limit = self.cfg.stall_limit * if in_tool { 2 } else { 1 };
        // The run's start counts as progress: the last event may predate it by hours.
        (self.last_event.max(run_start).elapsed() >= limit).then(|| {
            let took = match limit.as_secs() / 60 {
                0 => format!("{} s", limit.as_secs()),
                m => format!("{m} min"),
            };
            format!("[zephyr] stopped a run that made no progress for {took}")
        })
    }

    /// The kill switch (DESIGN §4.4), run outside the queue. Returns the reply text.
    async fn control(&mut self, cmd: &str) -> String {
        let reply = match cmd {
            "/pause" => {
                log(self.store.set_paused(Some("user")).map_err(Into::into));
                if self.busy {
                    self.abort().await;
                }
                self.procs.stop_all();
                "[zephyr] paused: messages wait in the inbox until /resume"
            }
            "/resume" => {
                // What came due during the pause waits for the owner, before a tick runs it.
                if self.store.pause_reason().ok().flatten().is_some() {
                    crate::actions::resume(&self.cfg, &self.store, crate::now());
                    self.due_held = false;
                }
                log(self.store.set_paused(None).map_err(Into::into));
                return "[zephyr] resumed".into();
            }
            _ => {
                self.abort().await;
                // Background tasks too (DESIGN §4.12); each still reports back as stopped.
                crate::tasks::stop(&mut self.store, self.jobs, None);
                // Approved actions running are `procs` commands; held ones stay held, and
                // delayed ones wait for the owner again (R47).
                self.procs.stop_all();
                crate::actions::stop(&self.cfg, &self.store, &self.waits);
                "[zephyr] stopped"
            }
        };
        match self.procs.kill_strays() {
            0 => reply.into(),
            1 => format!("{reply}, and 1 stray process"),
            n => format!("{reply}, and {n} stray processes"),
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

    /// Sends every `new` row: `prompt` when idle; when busy, steered into the running turn,
    /// which rpi delivers after the current tool as a user message of its own. A follow-up
    /// waits for the settle, and each one then starts a run of its own, in order.
    async fn dispatch_new(&mut self) -> anyhow::Result<()> {
        // Rows stay `new` while the child is down, the gateway is paused or stopping.
        if self.stopping || self.child.is_none() || self.store.pause_reason()?.is_some() {
            return Ok(());
        }
        for row in self.store.inbox_with_status("new")? {
            if self.busy && self.store.is_follow_up(row.id)? {
                continue;
            }
            let again = self.store.sent_as(row.id)?.as_deref() == Some("continue");
            let sent_as = match (self.busy, again) {
                (true, _) => "steer",
                (false, true) => "continue",
                (false, false) => "prompt",
            };
            // The projects as they were before this run (DESIGN §4.14); a steer's run has one.
            if !self.busy {
                crate::checkpoint::before(&self.cfg, &row.text).await;
            }
            if !self.busy {
                self.begin_run().await;
            }
            // A task's, a process's or an action's report is the daemon's, not the owner's text:
            // the conversation holds it from now on (DESIGN §4.6).
            if let Some(reason) = daemon_row(&row.platform_id) {
                self.taint_session(&reason);
            }
            // Durable before the child sees it, so recovery knows to look for it, and how.
            self.store.mark_dispatched_as(row.id, sent_as)?;
            let text = match again {
                true => continued(&row),
                false => tagged(&row),
            };
            if self.busy {
                let text = steered(&row);
                let steer = json!({"type": "queue_message", "message": text, "deliverAs": "steer"});
                self.call(steer).await?;
                // No user turn on the tap: rpi's own user message shows it when delivered.
                self.steered_texts.insert(text);
                self.steered.push((row, false));
            } else {
                self.call(json!({"type": "prompt", "message": text}))
                    .await?;
                self.busy = true;
                self.user_turn(&row, &text, true);
            }
        }
        Ok(())
    }

    /// A run of main starts: whether it is the owner's alone is decided now (DESIGN §4.6), from
    /// what the daemon saw, never from what the agent says. Every doubt is `Trust::Not`.
    async fn begin_run(&mut self) {
        self.run_start = Some(Instant::now());
        self.run_taint = false;
        let now = crate::now();
        let session = self.child.as_ref().and_then(|c| c.session.clone());
        let failed = session.as_ref().and_then(|s| self.tainted.get(s)).cloned();
        let origin = session.map(|s| self.store.session_origin(&s));
        self.trust = match origin {
            _ if failed.is_some() => Trust::Not(format!(
                "this conversation, which {}",
                failed.unwrap_or_default()
            )),
            Some(Ok(SessionOrigin::Clean)) if self.writers_since(now) => Trust::Not(BESIDE.into()),
            Some(Ok(SessionOrigin::Clean)) => {
                match crate::memory::base(&self.cfg.home, self.cfg.state_dir()).await {
                    Some(base) => Trust::Owner { base, since: now },
                    None => Trust::Not(ON_WAITING.into()),
                }
            }
            Some(Ok(SessionOrigin::Tainted(why))) => {
                Trust::Not(format!("this conversation, which {why}"))
            }
            _ => Trust::Not(crate::memory::LEGACY.into()),
        };
    }

    /// Whether other work of the agent's ran, or runs, since `since`, or a stray of its lives:
    /// any of it could write memory. A failed read counts as yes.
    fn writers_since(&self, since: i64) -> bool {
        self.store.writers_since(since).unwrap_or(true) || self.procs.strays_alive()
    }

    /// The run's conversation took in text that is not the owner's: it is tainted for good, in
    /// `session_origin` before the run goes on (once per run), and this run is not the owner's
    /// even if that write fails.
    fn taint_session(&mut self, reason: &str) {
        if self.run_taint {
            return;
        }
        self.run_taint = true;
        let why = format!("{reason} at {}", self.clock(crate::now()));
        if let Some(session) = self.child.as_ref().and_then(|c| c.session.clone()) {
            self.record_taint(session, &why);
        }
        if let Trust::Owner { .. } = self.trust {
            self.trust = Trust::Not(format!("this conversation, which {why}"));
        }
    }

    /// `session_origin` takes the taint; when it cannot, this process remembers it.
    fn record_taint(&mut self, session: String, why: &str) {
        if let Err(e) = self.store.taint(&session, why) {
            eprintln!("zephyr: recording the conversation's taint failed: {e}");
            self.tainted
                .entry(session)
                .or_insert_with(|| why.to_string());
        }
    }

    /// At start: a run of main in flight when the daemon stopped may have read text whose events
    /// were never read, so its conversation is not the owner's alone (DESIGN §4.6).
    fn taint_cut_off(&mut self) {
        let rows = self.store.inbox_with_status("dispatched");
        if rows.as_ref().is_ok_and(|r| r.is_empty()) {
            return;
        }
        let session = (self.store.main_session()).unwrap_or_else(|_| MAIN_SESSION.into());
        let why = format!("was cut off by a restart at {}", self.clock(crate::now()));
        self.record_taint(session, &why);
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
        let cut = self.cut.take();
        self.cut_at = None;
        self.compacted(ev);
        // Before reconcile dispatches the next row, so that row runs on the new model.
        self.apply_model().await;
        self.backoff = FIRST_BACKOFF;
        let trust = std::mem::replace(&mut self.trust, Trust::Not(crate::memory::LEGACY.into()));
        let origin = match trust {
            // Anything that ran beside the run, or a stray, could have written memory too.
            Trust::Owner { base, since } => match self.writers_since(since) {
                false => crate::memory::Origin::Owner { base },
                true => crate::memory::Origin::Main(BESIDE.into()),
            },
            Trust::Not(label) => crate::memory::Origin::Main(label),
        };
        let (home, state, digest) = (&self.cfg.home, self.cfg.state_dir(), self.cfg.digest());
        let done = crate::memory::commit(home, state, origin, digest).await;
        // A change saved at once is in the next child's context, as a rule is.
        if done.as_ref().is_ok_and(|c| c.auto && c.sha.is_some()) {
            self.restart = true;
        }
        let memory = crate::memory::recorded(&self.store, done);
        let main = crate::rpc::Tally::main(&self.cfg.home);
        let tally = std::mem::replace(&mut self.tally, main);
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
        // Who this run served, read before reconcile dispatches the next row.
        let to = self.store.last_owner().unwrap_or_else(|e| {
            eprintln!("zephyr: reading the last owner failed: {e}");
            None
        });
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
                self.run_completed = ev["outcome"] == "completed" && cut.is_none();
                log(self.reconcile(Some(fallback), false).await);
            }
        }
        // The agent's files and proposed jobs, after the reply they belong to.
        crate::tools::apply(&self.store, &self.cfg, &tally.requests, to.clone());
        // After the reply, so the owner reads what was sent out next to what it was for.
        let mut notices: Vec<String> = [
            cut,
            tally.alert().map(|a| crate::facts::scrubbed(&self.cfg, a)),
            memory,
        ]
        .into_iter()
        .flatten()
        .collect();
        // Skill and connector drafts the run wrote, each shown once (DESIGN §4.6).
        notices.extend(crate::drafts::scan(&self.cfg));
        if crossed {
            notices.push(BUDGET_NOTICE.into());
        }
        for notice in notices {
            if let Some((channel, recipient)) = &to {
                log(self
                    .store
                    .notify(channel, recipient, &notice)
                    .map_err(Into::into));
            }
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
            ("/model", model) => self.owner_set("model", model, from.0, |a| a).await,
            ("/thinking", level) => self.thinking_level(level, from.0).await,
            ("/set", arg) => self.set_command(arg, from.0).await,
            ("/settings", arg) => self.settings_listing(arg),
            ("/approvals", arg) => self.approvals(arg, from.0).await,
            // Held messages, posts and payments (approvals spec §4.6).
            ("/held", "") => crate::actions::held(&self.cfg, &self.store),
            // The daemon's own record (DESIGN §4.19), answered outside the queue.
            ("/activity", arg) => crate::activity::chat(&self.cfg, &self.store, arg, crate::now()),
            ("/cancel", n) => {
                let reply = crate::actions::cancel(&self.cfg, &self.store, &self.waits, n);
                self.settled_actions();
                reply.unwrap_or_else(|refused| refused)
            }
            // `/rule` alone and `/rules` are `memory::command`'s.
            ("/rule", rule) if !rule.is_empty() => {
                self.owner_set("rule", rule, from.0, |a| a).await
            }
            // `/goals`, `/persona` alone and the usages are `memory::command`'s.
            ("/goal", arg) if matches!(word(arg).0, "add" | "done" | "rm") => {
                self.owner_set("goal", arg, from.0, |a| a).await
            }
            ("/persona", arg) if matches!(word(arg).0, "set" | "clear") => {
                self.owner_set("persona", arg, from.0, |a| a).await
            }
            ("/forget", text) => self.forget(text).await,
            ("/memory", "squash yes") => {
                let squashed = crate::memory::squash(&self.cfg.home, self.cfg.state_dir()).await;
                if squashed.is_ok() {
                    log(self
                        .store
                        .clear_memory_commits()
                        .map(drop)
                        .map_err(Into::into));
                }
                squashed.unwrap_or_else(|refused| refused)
            }
            ("/new" | "/clear", "") => self.new_session().await,
            ("/switch", "") => self.list_sessions(),
            ("/switch", id) => self.switch(id).await,
            ("/login", arg) => self.login(arg, from).await,
            ("/btw", "") => crate::btw::USAGE.into(),
            ("/btw", question) => self.btw(question, from).await,
            ("/process", arg) => {
                use zephyr_policy::tools::ProcessAction;
                let action = match arg.split_once(' ').unwrap_or((arg, "")) {
                    ("list", "") => Some(ProcessAction::List),
                    ("stop", id) => id.trim().parse().ok().map(ProcessAction::Stop),
                    _ => None,
                };
                match action {
                    Some(a) => self.procs.reply(&self.cfg, &self.store, a),
                    None => "[zephyr] usage: /process list, /process stop <id>".into(),
                }
            }
            ("/task", "list") => crate::tasks::list(&self.store),
            ("/task", arg) => match word(arg) {
                ("stop", id) if id.parse::<i64>().is_ok() => {
                    let id = id.parse().ok();
                    crate::tasks::stop(&mut self.store, self.jobs, id)
                }
                ("parallel", n) if n.parse::<usize>().is_ok() => {
                    let done = |a| format!("[zephyr] up to {n} tasks run at once\n{a}");
                    self.owner_set("tasks.parallel", n, from.0, done).await
                }
                ("model", m) if !m.is_empty() && word(m).1.is_empty() => {
                    let done = |a| match m {
                        "default" => format!("[zephyr] tasks run on the account's own model\n{a}"),
                        m => format!("[zephyr] tasks run on {m} unless they name a model\n{a}"),
                    };
                    self.owner_set("tasks.model", m, from.0, done).await
                }
                _ => "[zephyr] usage: /task list, /task stop <id>, /task parallel <1-32>, \
                      /task model PROVIDER/MODEL|default"
                    .into(),
            },
            ("/subagent", arg) => match word(arg) {
                ("" | "list", "") => format!("[zephyr] {}", crate::tasks::settings(&self.cfg)),
                _ => self.owner_set("subagent", arg, from.0, |a| a).await,
            },
            ("/undo", n) => match if n.is_empty() { Ok(1) } else { n.parse() } {
                Ok(n @ 1..=20) => match self.undo(n).await {
                    Ok(_) if n == 1 => "[zephyr] undone: the agent no longer sees your last \
                                        message or its reply; files it changed stay changed \
                                        (see /rollback)"
                        .into(),
                    Ok(_) => format!(
                        "[zephyr] undone: the agent no longer sees your last {n} messages or \
                         their replies; files it changed stay changed (see /rollback)"
                    ),
                    Err(reply) => reply,
                },
                _ => "[zephyr] usage: /undo [n], 1 to 20 (default 1)".into(),
            },
            ("/rename", "") => "[zephyr] usage: /rename <name>".into(),
            ("/rename", name) => self.rename(name),
            ("/fork", "") => self.fork().await,
            ("/export", "") => self.export().await,
            ("/retry", "") => self.retry().await,
            ("/compact", instructions) => self.compact(instructions, from).await,
            ("/usage", days) => self.usage(days),
            ("/rollback", arg) => {
                let busy = self.rollback_busy();
                crate::checkpoint::command(&self.cfg, arg, busy.as_deref()).await
            }
            // The owner's credentials (DESIGN §4.15); also an approved `/vault grant` proposal.
            ("/vault", arg) => crate::vault::command(&crate::vault::path(&self.cfg), arg),
            // The owner's private facts (approvals spec §4.5): names only, never a value.
            ("/facts", arg) => {
                let strong = self
                    .cfg
                    .settings
                    .strong_channels
                    .iter()
                    .any(|c| c == from.0);
                crate::facts::command(&crate::facts::path(&self.cfg), arg, strong)
            }
            // Skills and connectors the agent drafted (DESIGN §4.6).
            ("/skills" | "/skill" | "/mcp", _) => {
                use crate::drafts::After;
                let (reply, after) = crate::drafts::command(&self.cfg, text)?;
                match after {
                    After::Nothing => reply,
                    // rpi reads AGENTS.md once, at start.
                    After::Restart => {
                        self.restart = true;
                        reply
                    }
                    After::Reload => format!("{reply}\n{}", self.reload().await),
                }
            }
            _ => return None,
        })
    }

    /// `/subagent add|model|prompt|rm ...`: a change to the owner's named subagents (DESIGN
    /// §4.12), through `set_setting`. The reply, or why nothing changed.
    async fn subagent(&mut self, arg: &str) -> Result<Reloaded, String> {
        use crate::host::conf::{remove_subagent, set_subagent};
        type Doc = toml_edit::DocumentMut;
        let (sub, rest) = word(arg);
        let (name, rest) = word(rest);
        let name = name.to_string();
        let (done, applied) = match (sub, name.as_str(), rest) {
            ("add", n, rest) if !n.is_empty() && !rest.is_empty() => {
                // The second word is a model when it has a '/'; the rest is the prompt.
                let (model, prompt) = match word(rest) {
                    (m, prompt) if m.contains('/') => (Some(m), prompt),
                    _ => (None, rest),
                };
                if self.cfg.subagents.iter().any(|s| s.name == name) {
                    return Err(format!(
                        "there is a subagent named {name} already; change it with \
                         /subagent model {name} ... or /subagent prompt {name} ..., or remove it \
                         with /subagent rm {name}"
                    ));
                }
                let set = |doc: &mut Doc| set_subagent(doc, &name, true, model, Some(prompt));
                let done = format!("added the subagent {name}");
                (done, self.edit_config(model, set).await)
            }
            ("model", n, m) if !n.is_empty() && !m.is_empty() && word(m).1.is_empty() => {
                let set = |doc: &mut Doc| set_subagent(doc, &name, false, Some(m), None);
                let done = format!("subagent {name} runs on {m}");
                (done, self.edit_config(Some(m), set).await)
            }
            ("prompt", n, prompt) if !n.is_empty() && !prompt.is_empty() => {
                let set = |doc: &mut Doc| set_subagent(doc, &name, false, None, Some(prompt));
                let done = format!("changed the prompt of subagent {name}");
                (done, self.edit_config(None, set).await)
            }
            ("rm", n, "") if !n.is_empty() => {
                let rm = |doc: &mut Doc| match remove_subagent(doc, &name) {
                    true => Ok(()),
                    false => Err(anyhow::anyhow!("no subagent named {name}")),
                };
                let done = format!("removed the subagent {name}");
                (done, self.edit_config(None, rm).await)
            }
            _ => return Err(SUBAGENT_USAGE.into()),
        };
        let applied = applied?;
        Ok(Reloaded {
            text: format!("[zephyr] {done}\n{}", applied.text),
            failed: applied.failed,
        })
    }

    /// Edits `zephyr.toml` for the owner: checks `model` (unless `default`) as `/model` does,
    /// saves the file only if it still loads, then reloads. How the reload went, or why nothing
    /// changed.
    async fn edit_config(
        &mut self,
        model: Option<&str>,
        edit: impl FnOnce(&mut toml_edit::DocumentMut) -> anyhow::Result<()>,
    ) -> Result<Reloaded, String> {
        let Some(path) = self.cfg.path.clone() else {
            return Err("no zephyr.toml to change".into());
        };
        if let Some(m) = model.filter(|m| *m != "default") {
            check_model(&self.cfg, m).await?;
        }
        crate::host::conf::update(&path, |doc| {
            edit(doc)?;
            toml::from_str::<Config>(&doc.to_string())?.validate()
        })
        .map_err(|e| format!("not changed: {e:#}"))?;
        Ok(self.reloaded().await)
    }

    /// Approvals spec §4.10: an install made before the gate keeps today's behaviour. The first
    /// start after the v16 migration of an existing `state.db`, with no `[approvals]` table in
    /// `zephyr.toml`, writes `preset = "full"` (logged like any change) and tells the owner once.
    /// The mark is cleared once dealt with; if the file cannot be written it stays for the next
    /// start.
    async fn keep_upgraded_install_as_it_was(&mut self) {
        let Some(from) = self.store.upgraded_from() else {
            return;
        };
        if self.cfg.approvals.is_none() && self.cfg.path.is_some() {
            let write = |doc: &mut toml_edit::DocumentMut| {
                crate::settings::write(doc, "approvals.preset", Some(&"full".into()))?;
                let value = (doc.get_mut("approvals"))
                    .and_then(|a| a.get_mut("preset"))
                    .and_then(|v| v.as_value_mut());
                if let Some(v) = value {
                    v.decor_mut()
                        .set_suffix(" # kept as before 0.7; /approvals manual turns approvals on");
                }
                Ok(())
            };
            if let Err(why) = self.edit_config(None, write).await {
                return eprintln!("zephyr: upgrade from schema {from}: {why}");
            }
            let logged = self.store.log_setting(
                "approvals.preset",
                Some("manual"),
                Some("full"),
                "zephyr",
                None,
                None,
            );
            log(logged.map_err(Into::into));
            log(self
                .store
                .set_upgrade_notice_pending(true)
                .map_err(Into::into));
        }
        log(self.store.clear_upgraded().map_err(Into::into));
        self.upgrade_notice();
    }

    /// The once-only notice that an upgrade kept approvals off. It waits, across restarts, until
    /// there is an owner to tell (an install whose agent has only run jobs has none yet).
    fn upgrade_notice(&self) {
        if !self.store.upgrade_notice_pending() || self.store.last_owner().ok().flatten().is_none()
        {
            return;
        }
        let told = self.store.notify_last_owner(
            "[zephyr] approvals are off, as before this version; \
             /approvals manual or /settings turns them on",
        );
        match told {
            Ok(()) => log(self
                .store
                .set_upgrade_notice_pending(false)
                .map_err(Into::into)),
            Err(e) => log(Err(e.into())),
        }
    }

    /// The one way a setting changes (settings spec §4.3), whoever asks: the key is looked up,
    /// `raw` checked against its kind, the change decided (an owner on a channel that is not
    /// strong cannot loosen a strong setting; the agent tightens at once and proposes anything
    /// else), applied, logged and, with `until`, given its revert. `reason` is the agent's,
    /// quoted on the owner's notice or card.
    async fn set_setting(
        &mut self,
        key: &str,
        raw: &str,
        actor: Actor<'_>,
        until: Option<Duration>,
        reason: Option<&str>,
    ) -> Outcome {
        use crate::settings::{self as reg, Kind, Verdict};
        let Some(s) = reg::find(key) else {
            return Outcome::Refused(format!("no setting {key}; /settings lists them"));
        };
        if s.apply == Apply::Facts {
            return Outcome::Refused("change private facts with /facts".into());
        }
        let list = matches!(
            s.apply,
            Apply::Subagent | Apply::Rule | Apply::Goal | Apply::Persona
        );
        let raw = raw.trim();
        if until.is_some() && list {
            return Outcome::Refused(format!("a change to {key} cannot be timed"));
        }
        let (old, seen) = (self.shown(key), self.current(s, key));
        // The end of a timed preset puts back the whole `[approvals]` it replaced, which its
        // revert holds (`approvals::snapshot`); nobody else writes one.
        let restore = (s.apply == Apply::Preset && actor == Actor::Internal).then_some(raw);
        // `None` clears the key. A thinking level, a subagent or a rule is checked as it applies.
        let value = match s.apply {
            Apply::Preset if restore.is_some() => None,
            Apply::Toml | Apply::Model | Apply::Preset if !reg::clears(s, raw) => {
                match reg::parse(s, raw) {
                    Ok(v) => Some(v),
                    Err(e) if s.apply == Apply::Model => {
                        return Outcome::Refused(format!("model not changed: {e}"))
                    }
                    Err(e) => return Outcome::Refused(format!("not changed: {e}")),
                }
            }
            _ => None,
        };
        let new = match (&value, s.apply) {
            (Some(v), _) => Some(reg::text(v)),
            // An approval class cleared follows the preset.
            (None, Apply::Toml) => reg::cleared(&self.cfg, key),
            (None, Apply::Preset) => raw.split(';').next().map(|p| p.trim().to_string()),
            (None, _) => Some(raw.to_string()),
        };
        let loosens = reg::loosens(s, old.as_deref(), new.as_deref());
        let strong = self.cfg.settings.strong_channels.clone();
        match reg::decide(s, &actor, loosens, &strong) {
            Verdict::Apply => {}
            Verdict::Refuse(why) => {
                if let (Actor::Owner { channel }, true) = (actor, s.strong && loosens) {
                    let (old, new) = (old.as_deref(), new.as_deref());
                    let refused =
                        self.store
                            .log_setting(key, old, new, "refused", Some(channel), None);
                    log(refused.map_err(Into::into));
                }
                // The key asked for, not the entry it shares (`mcp.<name>.classes.<tool>`).
                return Outcome::Refused(why.replace(s.key, key));
            }
            Verdict::Propose => {
                let span = until.map(reg::span);
                let command = match s.apply {
                    Apply::Subagent => format!("/subagent {raw}"),
                    Apply::Rule => format!("/rule {raw}"),
                    Apply::Goal => format!("/goal {raw}"),
                    Apply::Persona => format!("/persona {raw}"),
                    _ => {
                        let span = span.as_ref().map_or(String::new(), |d| format!(" for {d}"));
                        format!("/set {key} {raw}{span}")
                    }
                };
                let needs = s.strong && loosens;
                let id = match self
                    .store
                    .propose_setting(&command, key, seen.as_deref(), needs)
                {
                    Ok(id) => id,
                    Err(e) => return Outcome::Refused(format!("proposing {command} failed: {e}")),
                };
                let what = match list {
                    true => command,
                    false => format!(
                        "{key} {} → {}",
                        reg::shown(old.as_deref()),
                        reg::shown(new.as_deref())
                    ),
                };
                let needs = needs.then(|| strong.join(" or "));
                let card = reg::card(id, &what, span.as_deref(), reason, needs.as_deref());
                self.tell_owner(&card);
                return Outcome::Proposed { id };
            }
        }
        // What a time limit on a preset goes back to: all of `[approvals]` before the change.
        let mut before = None;
        // R31: the class time limits an owner's preset ends, as it changes or clears them.
        let mut ended: Vec<String> = Vec::new();
        // A goal's or the persona's change logs what `memory::Change` says, never the persona.
        let mut logged = None;
        let applied = match s.apply {
            Apply::Toml => {
                let model =
                    matches!(s.kind, Kind::Model | Kind::Optional(Kind::Model)) && value.is_some();
                let write = |doc: &mut toml_edit::DocumentMut| reg::write(doc, key, value.as_ref());
                self.edit_config(model.then_some(raw), write).await
            }
            // The preset and the hand-set values it drops (or a timed one's whole set put back)
            // are one update, worked out from the file as it is under the writer's lock.
            Apply::Preset => {
                let preset = value.as_ref().map(reg::text);
                // R31: as any later owner change of a key does, an owner's preset ends the time
                // limit of each class it changes or clears, so that limit's revert can never
                // loosen what the preset chose. The agent's never does (R12); the preset's own
                // limit is set below as usual.
                let limits: Vec<(String, Option<String>)> = match actor {
                    Actor::Owner { .. } => (crate::approvals::CLASS_KEYS.iter())
                        .filter_map(|k| Some((k.to_string(), self.store.revert(k).ok()??.value)))
                        .collect(),
                    _ => Vec::new(),
                };
                let write = |doc: &mut toml_edit::DocumentMut| {
                    let now: Config = toml::from_str(&doc.to_string())?;
                    let edits = match (restore, preset.as_deref()) {
                        (Some(snapshot), _) => crate::approvals::restore_edits(&now, snapshot)
                            .map_err(|e| anyhow::anyhow!(e))?,
                        (None, Some(p)) => {
                            let p =
                                crate::approvals::Preset::parse(p).map_err(anyhow::Error::msg)?;
                            crate::approvals::preset_edits(&now, p)
                        }
                        (None, None) => anyhow::bail!("no preset to set"),
                    };
                    let ends: Vec<(String, Option<String>)> = (limits.into_iter())
                        .filter(|(k, _)| edits.iter().any(|(e, _)| e == k))
                        .collect();
                    // A timed preset saves such a class as what its limit would have put back.
                    before = Some(crate::approvals::snapshot_with(&now, &ends));
                    ended = ends.into_iter().map(|(k, _)| k).collect();
                    for (key, v) in edits {
                        reg::write(doc, &key, v.map(toml_edit::Value::from).as_ref())?;
                    }
                    Ok(())
                };
                self.edit_config(None, write).await
            }
            Apply::Model => self.switch_model(raw).await,
            Apply::Thinking => self.apply_thinking(raw).await,
            Apply::Subagent => self.subagent(raw).await,
            // rpi reads AGENTS.md once, at start: a fresh child on the same session loads the rules.
            Apply::Rule => crate::memory::rule(self.cfg.state_dir(), raw).map(|reply| {
                self.restart = true;
                Reloaded::done(reply)
            }),
            Apply::Goal | Apply::Persona => {
                let state = self.cfg.state_dir();
                let change = match s.apply {
                    Apply::Goal => crate::memory::goal(state, raw),
                    _ => crate::memory::persona(state, raw),
                };
                change.map(|c| {
                    self.restart = true;
                    logged = Some((c.old, c.new));
                    Reloaded::done(c.reply)
                })
            }
            Apply::Facts => Err("change private facts with /facts".into()),
        };
        let applied = match applied {
            Ok(applied) => applied,
            Err(why) => return Outcome::Refused(why),
        };
        // What is in effect now; a model asked for may still be on its way to the child, and
        // after a failed reload the file holds what was asked, not what is in effect.
        let new = match (s.apply, applied.failed) {
            (Apply::Toml | Apply::Thinking | Apply::Preset, false) => self.shown(key),
            _ => new,
        };
        let (who, channel, proposal) = match actor {
            Actor::Owner { channel } if self.approving.is_some() => {
                ("agent", Some(channel), self.approving)
            }
            Actor::Owner { channel } => ("owner", Some(channel), None),
            Actor::Agent { .. } => ("agent", None, None),
            Actor::Internal => ("zephyr", None, None),
        };
        // Spec §4.9: the file stays saved, and the log says the reload failed.
        let who = match applied.failed {
            true => format!("{who} (reload failed)"),
            false => who.to_string(),
        };
        let (old, new) = logged.unwrap_or((old, new));
        let (o, n) = (old.as_deref(), new.as_deref());
        log((self.store.log_setting(key, o, n, &who, channel, proposal)).map_err(Into::into));
        // A change that did not take effect leaves the time limits as they were.
        if !applied.failed {
            for key in &ended {
                log(self.store.drop_revert(key).map_err(Into::into));
            }
            let back = match (s.apply, &s.kind) {
                // Back to what the file held: unset, or for a setting that has no "none", the
                // value it had by default.
                (Apply::Toml, Kind::Optional(_)) => seen,
                (Apply::Toml, _) => seen.or_else(|| old.clone()),
                (Apply::Preset, _) => before,
                _ => old.clone(),
            };
            self.time_limit(key, actor, until, back, channel);
        }
        if let Actor::Agent { .. } = actor {
            self.tell_owner(&reg::tightened(key, o, n, reason));
        }
        Outcome::Changed {
            old,
            new,
            applied: applied.text,
        }
    }

    /// The time limit after a change to `key`: with `until`, it goes `back` then; without, a
    /// pending one is cancelled. The agent never cancels or replaces one that is pending:
    /// its own `for D` counts only when none is.
    fn time_limit(
        &self,
        key: &str,
        actor: Actor<'_>,
        until: Option<Duration>,
        back: Option<String>,
        channel: Option<&str>,
    ) {
        // A revert that cannot be read counts as pending: the agent then leaves it alone.
        let pending = self.store.revert(key).map_or(true, |r| r.is_some());
        let done = match (actor, until) {
            (Actor::Agent { .. }, _) if pending => return,
            (_, None) => self.store.drop_revert(key),
            (_, Some(d)) => {
                let since = crate::now();
                self.store.set_revert(&crate::store::Revert {
                    key: key.into(),
                    value: back,
                    at: since + d.as_secs() as i64,
                    since,
                    channel: channel.map(Into::into),
                })
            }
        };
        log(done.map_err(Into::into));
    }

    /// An owner's command that changes `key` through `set_setting`: `done` words a change from
    /// how it applied; a refusal is `[zephyr] ` and its reason.
    async fn owner_set(
        &mut self,
        key: &str,
        raw: &str,
        channel: &str,
        done: impl FnOnce(String) -> String,
    ) -> String {
        let owner = Actor::Owner { channel };
        match self.set_setting(key, raw, owner, None, None).await {
            Outcome::Changed { applied, .. } => done(applied),
            Outcome::Proposed { id } => format!("[zephyr] proposed as #{id}"),
            Outcome::Refused(why) => format!("[zephyr] {why}"),
        }
    }

    /// `/set KEY VALUE [for D]`, for any registered setting.
    async fn set_command(&mut self, arg: &str, channel: &str) -> String {
        let Some((key, value, until)) = crate::settings::set_args(arg) else {
            return SET_USAGE.into();
        };
        if key == "facts" {
            return "[zephyr] change private facts with /facts".into();
        }
        if matches!(key, "subagent" | "rule") {
            return "[zephyr] change subagents with /subagent and rules with /rule".into();
        }
        if matches!(key, "goal" | "persona") {
            return "[zephyr] change goals with /goal and the persona with /persona".into();
        }
        let min = self.cfg.min_until;
        let until = match until
            .map(|d| crate::settings::parse_until(d, min))
            .transpose()
        {
            Ok(until) => until,
            Err(e) => return format!("[zephyr] {e}"),
        };
        let owner = Actor::Owner { channel };
        match self.set_setting(key, value, owner, until, None).await {
            Outcome::Changed { old, new, applied } => {
                let at = until.and(self.store.revert(key).ok().flatten());
                let at = at.map(|r| self.clock(r.at));
                let (old, new) = (old.as_deref(), new.as_deref());
                let line = crate::settings::changed(key, old, new, at.as_deref());
                match applied.as_str() {
                    "" | APPLIED => line,
                    applied => format!("{line}\n{applied}"),
                }
            }
            Outcome::Proposed { id } => format!("[zephyr] proposed as #{id}"),
            Outcome::Refused(why) => format!("[zephyr] {why}"),
        }
    }

    /// `/approvals` (approvals spec §4.3): the modes in effect; `manual|semi|full` (`off` is
    /// full, `on` manual) or `messages|posts|money|facts MODE`, each `[for D]`, through
    /// `set_setting` as `/set` would, answered with the change and the modes after it.
    async fn approvals(&mut self, arg: &str, channel: &str) -> String {
        let shown = |cfg: &Config| format!("[zephyr] {}", crate::approvals::effective(cfg).line());
        let (what, until) = match arg.rsplit_once(" for ") {
            Some((what, d)) if !what.trim().is_empty() => (what.trim(), format!(" for {d}")),
            _ => (arg.trim(), String::new()),
        };
        let (key, value) = match word(what) {
            ("", "") => return shown(&self.cfg),
            (p @ ("manual" | "semi" | "full"), "") => ("approvals.preset".to_string(), p),
            ("off", "") => ("approvals.preset".into(), "full"),
            ("on", "") => ("approvals.preset".into(), "manual"),
            ("forget", n) if until.is_empty() => return self.forget_target(n, channel),
            (class @ ("messages" | "posts" | "money" | "facts"), mode) if !mode.is_empty() => {
                (format!("approvals.{class}"), mode)
            }
            _ => return APPROVALS_USAGE.into(),
        };
        let reply = self
            .set_command(&format!("{key} {value}{until}"), channel)
            .await;
        match crate::settings::is_change(&reply, &key) {
            true => format!("{reply}\n{}", shown(&self.cfg)),
            false => reply,
        }
    }

    /// `/approvals forget N` (DESIGN §4.16): forgets remembered target `N`, so under `ask-new`
    /// a call to it waits again. It only makes the gate stricter, so any owner channel may.
    fn forget_target(&self, n: &str, channel: &str) -> String {
        let Some(n) = n.parse::<i64>().ok().filter(|n| *n > 0) else {
            return APPROVALS_USAGE.into();
        };
        let (class, target) = match self.store.forget_target(n) {
            Ok(Some(row)) => row,
            Ok(None) => {
                return format!("[zephyr] no remembered target #{n}; /activity grants lists them")
            }
            Err(e) => return format!("[zephyr] forgetting #{n} failed: {e}"),
        };
        let what = match class.as_str() {
            "messages" => "a message",
            "posts" => "a post",
            _ => "a payment",
        };
        // The target came from the agent's call: scrubbed and on one line, as the record shows it.
        let shown =
            crate::settings::one_line(&crate::gate::Scrub::load(&self.cfg).text(&target), 200);
        let mut reply = format!(
            "[zephyr] forgot {shown} for {class}; under ask-new {what} to it waits for you again"
        );
        let key = format!("approvals.known.{class}");
        let logged =
            self.store
                .log_setting(&key, Some(&target), None, "owner", Some(channel), None);
        if let Err(e) = logged {
            reply.push_str(&format!(
                "\n[zephyr] recording it in the activity failed: {e}"
            ));
        }
        reply
    }

    /// `/settings [SECTION|KEY]`: the values in effect and where each came from.
    fn settings_listing(&self, arg: &str) -> String {
        let strong = &self.cfg.settings.strong_channels;
        crate::settings::listing(arg, strong, |s| self.setting_rows(s))
    }

    /// `s`'s rows for `/settings`: one per connector for a connector's setting.
    fn setting_rows(&self, s: &Setting) -> Vec<crate::settings::Row> {
        let keys: Vec<String> = match s.key {
            "mcp.<name>.enabled" => (self.cfg.mcp.iter())
                .map(|m| format!("mcp.{}.enabled", m.name))
                .collect(),
            // The tools the owner gave a class.
            "mcp.<name>.classes.<tool>" => (self.cfg.mcp.iter())
                .flat_map(|m| (m.classes.keys()).map(|t| format!("mcp.{}.classes.{t}", m.name)))
                .collect(),
            key => vec![key.to_string()],
        };
        (keys.into_iter())
            .map(|key| crate::settings::Row {
                value: crate::settings::shown(self.shown(&key).as_deref()).to_string(),
                source: self.source(s, &key),
                key,
            })
            .collect()
    }

    /// Where `key`'s value came from: `until HH:MM` while a time limit runs, `default` when
    /// nothing sets it (`preset` for an approval class), else who changed it last.
    fn source(&self, s: &Setting, key: &str) -> String {
        if let Ok(Some(r)) = self.store.revert(key) {
            return format!("until {}", self.clock(r.at));
        }
        let change = self.store.last_setting_change(key).ok().flatten();
        let set = match s.apply {
            Apply::Toml | Apply::Preset => self.current(s, key).is_some(),
            // rpi's own level until one is chosen.
            Apply::Thinking => change.is_some(),
            _ => self.shown(key).is_some(),
        };
        match (set, change) {
            // Settings spec §4.6: an approval class nobody set follows the preset.
            (false, _) if crate::settings::cleared(&self.cfg, key).is_some() => "preset".into(),
            (false, _) => "default".into(),
            (true, Some(c)) => crate::settings::source(&c),
            // In zephyr.toml, written by hand or by the host CLI.
            (true, None) => "you".into(),
        }
    }

    /// `key`'s value in effect, as replies show it (`None`: unset): the model the child runs on
    /// and rpi's thinking level, the subagents' names and how many rules there are, the rest
    /// from the config.
    fn shown(&self, key: &str) -> Option<String> {
        match key {
            "model" => (self.account.as_ref().map(|a| a.model.clone()))
                .or_else(|| crate::settings::value(&self.cfg, key)),
            "thinking" => self.thinking.clone(),
            "subagent" => {
                let names: Vec<&str> = self.cfg.subagents.iter().map(|s| s.name.as_str()).collect();
                (!names.is_empty()).then(|| names.join(", "))
            }
            // Names only: the values never leave facts.toml.
            "facts" => (crate::facts::load(&crate::facts::path(&self.cfg)).ok())
                .filter(|f| !f.is_empty())
                .map(|f| {
                    f.iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }),
            "rule" => match crate::memory::read_rules(self.cfg.state_dir()).map(|r| r.len()) {
                Ok(0) | Err(_) => None,
                Ok(1) => Some("1 rule".into()),
                Ok(n) => Some(format!("{n} rules")),
            },
            "goal" => match crate::memory::read_goals(self.cfg.state_dir()).map(|g| g.len()) {
                Ok(0) | Err(_) => None,
                Ok(1) => Some("1 goal".into()),
                Ok(n) => Some(format!("{n} goals")),
            },
            // Never the text: replies, the panel and settings_log say only whether there is one.
            "persona" => (crate::memory::read_persona(self.cfg.state_dir()).ok())
                .filter(|p| !p.is_empty())
                .map(|_| "set".into()),
            _ => crate::settings::value(&self.cfg, key),
        }
    }

    /// `key` as it stands, which a proposal records as the value the owner saw and `/ok` checks
    /// again: the file's text for a `zephyr.toml` setting (`None`: absent); for the subagents and
    /// the rules, a SHA-256 of their whole text, so any edit makes a waiting `/subagent …` or
    /// `/rule rm N` stale; else `shown`.
    fn current(&self, s: &Setting, key: &str) -> Option<String> {
        let doc = || crate::host::conf::load(self.cfg.path.as_deref()?).ok();
        match s.apply {
            Apply::Toml | Apply::Preset => crate::settings::read(&doc()?, key),
            Apply::Subagent => {
                let table = doc()?.get("subagent").map(|t| t.to_string());
                Some(crate::drafts::digest([table
                    .unwrap_or_default()
                    .as_bytes()]))
            }
            Apply::Rule => {
                let rules = crate::memory::read_rules(self.cfg.state_dir()).ok()?;
                Some(crate::drafts::digest(rules.iter().map(|r| r.as_bytes())))
            }
            Apply::Goal | Apply::Persona => {
                let name = if s.apply == Apply::Goal {
                    "GOALS.md"
                } else {
                    "PERSONA.md"
                };
                let text = std::fs::read(self.cfg.state_dir().join(name)).unwrap_or_default();
                Some(crate::drafts::digest([text.as_slice()]))
            }
            _ => self.shown(key),
        }
    }

    /// Why the owner's `/ok` on `channel` must not run proposal `p` now (`tools::decide`): its
    /// setting moved on since the owner was shown it, or it loosens a strong setting and
    /// `channel` is not a strong one.
    fn not_now(
        &self,
        p: &crate::store::PendingProposal,
        channel: &str,
    ) -> Option<crate::tools::NotNow> {
        use crate::tools::NotNow;
        // A fact's removal or new allowed target loosens the check: from a strong channel only.
        let facts = p.command.starts_with("/facts rm ") || p.command.starts_with("/facts allow ");
        let strong = &self.cfg.settings.strong_channels;
        if p.key.is_none() && facts && !strong.iter().any(|c| c == channel) {
            return Some(NotNow::Weak(format!(
                "[zephyr] #{} loosens the check for a private fact: approve it from {}",
                p.id,
                strong.join(" or ")
            )));
        }
        let key = p.key.as_deref()?;
        let s = crate::settings::find(key)?;
        if self.current(s, key) != p.seen {
            let now = self.shown(key);
            return Some(NotNow::Stale(crate::settings::stale(key, now.as_deref())));
        }
        let strong = &self.cfg.settings.strong_channels;
        match crate::settings::decide(s, &Actor::Owner { channel }, p.strong, strong) {
            crate::settings::Verdict::Refuse(why) => Some(NotNow::Weak(format!(
                "[zephyr] {}",
                why.replace(s.key, key)
            ))),
            _ => None,
        }
    }

    /// A notice for the owner, on the channel the run answers (else the one last answered).
    fn tell_owner(&self, text: &str) {
        if let Some((channel, to)) = self.origin() {
            log(self.store.notify(&channel, &to, text).map_err(Into::into));
        }
    }

    /// `at` as `HH:MM` in the owner's time zone.
    fn clock(&self, at: i64) -> String {
        let tz = crate::scheduler::zone(&self.cfg).unwrap_or(jiff::tz::TimeZone::UTC);
        crate::settings::clock(at, &tz)
    }

    /// Applies the time limits that ended (settings spec §4.5), and tells whoever set each, on
    /// the channel they set it from (else the owner last answered). A revert that cannot apply
    /// is not tried again: the owner is told why instead.
    async fn revert_due(&mut self) {
        let due = match self.store.due_reverts(crate::now()) {
            Ok(due) => due,
            Err(e) => return eprintln!("zephyr: reading setting reverts failed: {e}"),
        };
        for r in due {
            let span = crate::settings::span(Duration::from_secs((r.at - r.since).max(0) as u64));
            let (key, raw) = (r.key.as_str(), r.value.as_deref().unwrap_or("none"));
            let text = match self
                .set_setting(key, raw, Actor::Internal, None, None)
                .await
            {
                Outcome::Changed { new, .. } => {
                    crate::settings::reverted(key, new.as_deref(), &span)
                }
                failed => {
                    log(self.store.drop_revert(key).map_err(Into::into));
                    let why = match failed {
                        Outcome::Refused(why) => why,
                        _ => "it was not applied".into(),
                    };
                    format!(
                        "[zephyr] {key} could not go back to {raw} when the {span} you set \
                         ended: {why}"
                    )
                }
            };
            let told = match r.channel.and_then(|channel| self.owner_on(&channel)) {
                Some((channel, owner)) => self.store.notify(&channel, &owner, &text),
                None => self.store.notify_last_owner(&text),
            };
            log(told.map_err(Into::into));
        }
    }

    /// The owner on `channel`, as a notice there is addressed. On the TUI's channel the
    /// attached client is the owner (`attach` sends as `token`).
    fn owner_on(&self, channel: &str) -> Option<(String, String)> {
        if channel == crate::attach::CHANNEL {
            return Some((channel.into(), "token".into()));
        }
        let owner = self.cfg.owners.get(channel)?.first()?;
        Some((channel.into(), owner.clone()))
    }

    /// What a `/rollback N` waits for: anything that may still be writing to the projects.
    /// A store error counts as busy.
    fn rollback_busy(&self) -> Option<String> {
        let tasks = self.store.tasks(0).map_or(true, |t| !t.is_empty());
        let procs = self.store.running_procs().map_or(true, |p| !p.is_empty());
        let what: Vec<&str> = [
            (self.busy, "a run"),
            (self.jobs.running.load(Ordering::SeqCst), "a job run"),
            (tasks, "a background task"),
            (procs, "an exec process"),
        ]
        .into_iter()
        .filter_map(|(on, what)| on.then_some(what))
        .collect();
        (!what.is_empty()).then(|| format!("{} still running", what.join(", ")))
    }

    /// `/btw <question>`: reads `main`'s messages (rpi answers `get_messages` mid-run too) and
    /// hands the question to a task of its own (`crate::btw::run`) on the main child's account,
    /// so nothing here waits on it and `main`'s session, queue and run are untouched. Returns ""
    /// when the task will answer.
    async fn btw(&mut self, question: &str, from: (&str, &str)) -> String {
        if self.btw.is_some() {
            return "[zephyr] a /btw is already running".into();
        }
        if self.store.pause_reason().map_or(true, |p| p.is_some()) {
            return "[zephyr] paused: /btw waits for /resume".into();
        }
        let account = match (&self.account, self.pick()) {
            (Some(a), _) => Some(a.clone()),
            (None, Ok(a)) => a,
            (None, Err(until)) => {
                let until = crate::pool::iso(until);
                return format!(
                    "[zephyr] /btw failed: every account is rate-limited until {until}"
                );
            }
        };
        let transcript = match self.child {
            Some(_) => match self.call(json!({"type": "get_messages"})).await {
                Ok(snap) => Some(crate::btw::transcript(&snap)),
                Err(e) => {
                    eprintln!("zephyr: /btw could not read main: {e:#}");
                    None
                }
            },
            None => None,
        };
        let prompt = crate::btw::prompt(transcript, question);
        let to = (from.0.to_string(), from.1.to_string());
        self.btw = Some(tokio::spawn(crate::btw::run(
            self.cfg.clone(),
            account,
            prompt,
            to,
        )));
        String::new()
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
                 /login paste ADDRESS finishes one whose page could not open 127.0.0.1.\n\
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
        if verb == "paste" {
            return self.login_paste(rest, from);
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

    /// `/login paste ADDRESS`: the address the owner's browser could not open goes to the stdin
    /// of the sender's own newest sign-in, where rpi reads the code from it (DESIGN §4.7.1). Only
    /// something that looks like that address is taken, so a token pasted by mistake goes nowhere.
    fn login_paste(&mut self, text: &str, from: (&str, &str)) -> String {
        // Discord wraps a link in angle brackets when its preview is off.
        let text = text.trim();
        let text = (text.strip_prefix('<').and_then(|t| t.strip_suffix('>'))).unwrap_or(text);
        let address = ["http://127.0.0.1", "http://localhost"]
            .iter()
            .any(|start| text.starts_with(start))
            || text.contains("code=");
        if !address {
            return "[zephyr] /login paste takes the address the browser could not open \
                    (http://127.0.0.1:…?code=…). Never paste a token here."
                .into();
        }
        let theirs = |l: &&Login| {
            (l.from.as_ref())
                .is_some_and(|(channel, sender)| (channel.as_str(), sender.as_str()) == from)
        };
        match self.posted.iter().rev().find(theirs) {
            Some(login) if crate::auth::input(&login.id, text) => {
                format!("[zephyr] sent to the {} sign-in", login.provider)
            }
            _ => "[zephyr] no sign-in of yours is waiting for a pasted address".into(),
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
        // Only this router's own logins, the ones still waiting for their URL and the ones
        // whose URL was posted: the registry is process-wide, and an attached TUI's
        // `zephyr.login_*` may hold a login with the same provider, or an id a channel guessed.
        let own = |l: &&Login| match provider {
            Some(p) => l.provider == p,
            None => what.is_empty() || l.id == what,
        };
        let mine: Vec<(String, &'static str)> = (self.logins.iter().chain(&self.posted))
            .filter(own)
            .map(|l| (l.id.clone(), l.provider))
            .collect();
        for (id, provider) in mine {
            // Read before the cancel: `cancel` takes the entry out of the registry.
            let account = crate::auth::poll(&id)
                .and_then(|l| l["account"].as_str().map(str::to_string))
                .filter(|a| !a.is_empty());
            // `false`: already `done` or unknown, and a `done` login stays `done`.
            if !crate::auth::cancel(&id) {
                continue;
            }
            self.logins.retain(|l| l.id != id);
            self.posted.retain(|l| l.id != id);
            lines.push(match account {
                Some(a) => format!("[zephyr] cancelled the sign-in to {provider} as {a}"),
                None => format!("[zephyr] cancelled the sign-in to {provider}"),
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

    /// Posts what each `/login` sign-in has to say: the URL once, then a failure or the account
    /// once it is recorded, after which it is forgotten. On the 5 s config tick, so nothing here
    /// waits on a login and a login that died without a word is dropped by the registry and
    /// forgotten with it.
    fn login_notices(&mut self) {
        let waiting = std::mem::take(&mut self.logins).into_iter();
        let posted = std::mem::take(&mut self.posted).into_iter();
        let all = waiting.map(|l| (l, false)).chain(posted.map(|l| (l, true)));
        for (Login { id, provider, from }, announced) in all {
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
                // Its URL is out; the owner is signing in.
                _ if announced => {
                    self.posted.push(Login { id, provider, from });
                    continue;
                }
                _ => match login["url"].as_str() {
                    Some(url) => {
                        // Only where the browser's redirect cannot reach the login.
                        let paste = match phase {
                            "need_input" => "\nIf the page then cannot open 127.0.0.1, send /login paste <its address>.",
                            _ => "",
                        };
                        let text = format!(
                            "[zephyr] sign in to {provider} as {} at {url}{paste}",
                            login["account"].as_str().unwrap_or_default()
                        );
                        self.login_notice(&text, &from);
                        self.posted.push(Login { id, provider, from });
                        continue;
                    }
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

    /// `/forget TEXT`: its commit is the owner's, saved at once, and the child restarts at idle so
    /// its context drops the lines.
    async fn forget(&mut self, text: &str) -> String {
        match crate::memory::forget(&self.cfg.home, self.cfg.state_dir(), text).await {
            Ok(forgot) => {
                if let Some(sha) = &forgot.sha {
                    let recorded = self.store.record_memory_commit(sha, "you (/forget)", true);
                    log(recorded.map_err(Into::into));
                    self.restart = true;
                    self.restart_if_idle().await;
                }
                forgot.reply
            }
            Err(refused) => refused,
        }
    }

    /// `/new`: a fresh main session from the next message. The old one stays on disk; memory
    /// and the rules carry over, since they are files. Long sessions are easier to steer
    /// (ClawSafety Table 3), so the owner can cut one off.
    async fn new_session(&mut self) -> String {
        let id = format!("{MAIN_SESSION}-{}", crate::now());
        // Clean from its first message; `/switch` back to an older one never cleans it.
        if let Err(e) = self.store.mark_clean(&id) {
            eprintln!("zephyr: marking {id} clean failed: {e}");
        }
        match self.switch_main(&id).await {
            Ok(when) => format!("[zephyr] new conversation {when}; memory and rules are kept"),
            Err(e) => format!("[zephyr] no new conversation: {e}"),
        }
    }

    /// Points `main` at `id` and restarts the child on it once idle, never mid-turn.
    /// Returns when that happens, for the reply.
    async fn switch_main(&mut self, id: &str) -> rusqlite::Result<&'static str> {
        self.store.set_main_session(id)?;
        self.restart = true;
        let when = if self.busy {
            "after the current reply"
        } else {
            "from the next message"
        };
        self.restart_if_idle().await;
        Ok(when)
    }

    /// `/switch`: the saved conversations, newest first, the current one marked, each with its
    /// `/rename` name and its first message's opening words.
    fn list_sessions(&self) -> String {
        let current = self.store.main_session().unwrap_or_default();
        let names = self.store.session_names().unwrap_or_default();
        let dir = self.cfg.session_dir();
        let mut lines = vec!["[zephyr] conversations (/switch <id> goes back):".to_string()];
        for (id, at) in crate::rpc::saved_sessions(&dir).into_iter().take(10) {
            let mark = if id == current { "*" } else { " " };
            let mut line = format!("{mark} {id}  {}", crate::pool::iso(at));
            if let Some(name) = names.get(&id) {
                line.push_str(&format!("  \"{name}\""));
            }
            if let Some(words) = crate::rpc::opening(&dir, &id) {
                line.push_str(&format!("  {words}"));
            }
            if id == current {
                line.push_str("  (current)");
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// `/rename <name>`: names the current conversation, for `/switch` and the TUI's header.
    fn rename(&self, name: &str) -> String {
        let name: String = name.chars().filter(|c| !c.is_control()).take(60).collect();
        let named = (self.store.main_session())
            .and_then(|id| self.store.set_session_name(&id, &name).map(|()| id));
        match named {
            Ok(id) => format!("[zephyr] {id} is now called \"{name}\""),
            Err(e) => format!("[zephyr] /rename failed: {e}"),
        }
    }

    /// `/fork`, idle only: copies the conversation as it stands (`rpc::copy_session`) to a new
    /// `main-<secs>` and carries on in the copy, as `/switch` would; the original stays as it was.
    async fn fork(&mut self) -> String {
        if self.busy {
            return BUSY.into();
        }
        let from = match self.store.main_session() {
            Ok(id) => id,
            Err(e) => return format!("[zephyr] /fork failed: {e}"),
        };
        let dir = self.cfg.session_dir();
        let saved = crate::rpc::saved_sessions(&dir);
        let to = (crate::now()..)
            .map(|t| format!("{MAIN_SESSION}-{t}"))
            .find(|id| !saved.iter().any(|(s, _)| s == id))
            .expect("an unused second");
        if let Err(e) = crate::rpc::copy_session(&dir, &from, &to) {
            return format!("[zephyr] /fork failed: {e:#}");
        }
        match self.switch_main(&to).await {
            Ok(_) => format!(
                "[zephyr] forked {from} into {to}, where you are now; /switch {from} goes back"
            ),
            Err(e) => format!("[zephyr] forked {from} into {to}, but could not switch to it: {e}"),
        }
    }

    /// `/export`: the conversation as markdown (`markdown`) in the projects folder, written as
    /// the agent (`agent::fs`), so the file is the owner's on the host.
    async fn export(&mut self) -> String {
        let Some(projects) = self.cfg.projects.clone() else {
            return "[zephyr] /export writes to the projects folder, and this container was \
                    started without ZEPHYR_PROJECTS; recreate it with `zephyr up`"
                .into();
        };
        let snap = match self.call(json!({"type": "get_messages"})).await {
            Ok(snap) => snap,
            Err(e) => return format!("[zephyr] /export failed: {e:#}"),
        };
        let messages = snap["messages"].as_array().map_or(&[][..], Vec::as_slice);
        let session = self
            .store
            .main_session()
            .unwrap_or_else(|_| MAIN_SESSION.into());
        let tz = crate::scheduler::zone(&self.cfg).unwrap_or(jiff::tz::TimeZone::UTC);
        let stamp = jiff::Timestamp::now().to_zoned(tz).strftime("%Y%m%d-%H%M");
        let dir = projects.join("zephyr-exports");
        let path = dir.join(format!("{session}-{stamp}.md"));
        let written = {
            let _fs = crate::agent::fs();
            let text = markdown(&session, messages);
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, text))
        };
        match written {
            Ok(()) => format!("[zephyr] exported to {}", path.display()),
            Err(e) => format!("[zephyr] /export failed: {e}"),
        }
    }

    /// `/switch <id>`: back to a conversation `/new` left on disk. Only ids found there, since
    /// the id goes to rpi as `--session-id`.
    async fn switch(&mut self, id: &str) -> String {
        if self.store.main_session().is_ok_and(|c| c == id) {
            return format!("[zephyr] already on {id}");
        }
        let saved = crate::rpc::saved_sessions(&self.cfg.session_dir());
        if !saved.iter().any(|(s, _)| s == id) {
            return format!("[zephyr] no conversation {id}; /switch lists them");
        }
        match self.switch_main(id).await {
            Ok(when) => format!("[zephyr] back to {id} {when}"),
            Err(e) => format!("[zephyr] could not switch to {id}: {e}"),
        }
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
    async fn switch_model(&mut self, model: &str) -> Result<Reloaded, String> {
        let Some(path) = self.cfg.path.clone() else {
            return Err("no zephyr.toml to change".into());
        };
        let active = self.account.as_ref().map(|a| a.name.clone());
        for _ in 0..3 {
            let derived = crate::host::conf::load(&path).and_then(|mut doc| {
                crate::reload::set_model(&mut doc, active.as_deref(), model)?;
                Ok(model_account(&doc, model))
            });
            let name = derived.map_err(|e| format!("model not changed: {e:#}"))?;
            let target = name.clone().map(|name| Account {
                name,
                model: model.to_string(),
            });
            if !lists(&self.cfg.clone(), target.as_ref(), model).await {
                return Err(format!("rpi has no model {model}"));
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
                Ok(()) => Ok(self.reloaded().await),
                Err(e) => Err(format!("model not changed: {e:#}")),
            };
        }
        Err("model not changed: the account pool kept changing while rpi listed the model".into())
    }

    /// Reads `zephyr.toml` and `secrets.env` again and applies what changed (`reload::diff`).
    /// A file that does not load leaves everything as it was. Returns the reply text.
    async fn reload(&mut self) -> String {
        self.reloaded().await.text
    }

    /// `reload`, saying whether it failed: the file did not load, or rpi refused the new model.
    async fn reloaded(&mut self) -> Reloaded {
        use crate::reload::Plan;
        let Some(path) = self.cfg.path.clone() else {
            return Reloaded::failed("[zephyr] no zephyr.toml to reload".into());
        };
        self.edits = edits(&self.cfg);
        let mut new = match Config::load(&path) {
            Ok(new) => new,
            Err(e) => return Reloaded::failed(format!("[zephyr] zephyr.toml not applied: {e:#}")),
        };
        new.keep_hidden(&self.cfg);
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
        Reloaded::done(match plan {
            Plan::Same => "[zephyr] nothing changed".into(),
            Plan::Live => {
                self.account = next.or(self.account.take());
                APPLIED.into()
            }
            Plan::SetModel(model) => {
                self.account = next;
                self.pending_model = Some(model.clone());
                if self.busy {
                    return Reloaded::done(format!("[zephyr] switching to {model} {after}"));
                }
                match self.apply_model().await {
                    None => format!("[zephyr] now on {model}"),
                    Some(e) => return Reloaded::failed(e),
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
        })
    }

    /// `/thinking [level|next]`: how hard the model thinks, from the next request on, through
    /// `set_setting`. Without an argument, the level and the ones this model offers.
    async fn thinking_level(&mut self, arg: &str, channel: &str) -> String {
        if self.child.is_none() {
            return "[zephyr] the agent is not running".into();
        }
        let level = match arg {
            "" | "next" => {
                let levels = match self.thinking_levels().await {
                    Ok(levels) => levels,
                    Err(e) => return format!("[zephyr] {e}"),
                };
                let current = self.thinking.clone().unwrap_or_default();
                if arg.is_empty() {
                    return format!(
                        "[zephyr] thinking {current}; /thinking {} or next sets it",
                        levels.join("|")
                    );
                }
                let at = levels.iter().position(|l| *l == current);
                match levels.get(at.map_or(0, |i| (i + 1) % levels.len())) {
                    Some(l) => l.clone(),
                    None => return "[zephyr] this model has no thinking levels".into(),
                }
            }
            l => l.to_string(),
        };
        let done = |_| format!("[zephyr] thinking {level}");
        self.owner_set("thinking", &level, channel, done).await
    }

    /// The levels the child's model offers: rpi's list is the one that counts.
    async fn thinking_levels(&mut self) -> Result<Vec<String>, String> {
        let levels = (self
            .call(json!({"type": "get_available_thinking_levels"}))
            .await)
            .map_err(|e| format!("no thinking levels: {e:#}"))?;
        Ok((levels["levels"].as_array().into_iter().flatten())
            .filter_map(|l| l.as_str().map(String::from))
            .collect())
    }

    /// Sets rpi's thinking level, kept for each new child (`thinking_choice`), if the model
    /// offers it.
    async fn apply_thinking(&mut self, level: &str) -> Result<Reloaded, String> {
        if self.child.is_none() {
            return Err("the agent is not running".into());
        }
        let levels = self.thinking_levels().await?;
        if !levels.iter().any(|l| l == level) {
            return Err(format!(
                "no thinking level {level}; one of {}",
                levels.join(", ")
            ));
        }
        (self.set_thinking(level).await)
            .map_err(|e| format!("rpi refused thinking {level}: {e:#}"))?;
        self.thinking_choice = Some(level.to_string());
        Ok(Reloaded::done(String::new()))
    }

    async fn set_thinking(&mut self, level: &str) -> anyhow::Result<()> {
        let set = json!({"type": "set_thinking_level", "level": level});
        let v = self.call(set).await?;
        self.thinking = Some(v["thinkingLevel"].as_str().unwrap_or(level).into());
        Ok(())
    }

    /// Sends a pending `set_model`. A refusal goes to the owner, and the error text is returned.
    async fn apply_model(&mut self) -> Option<String> {
        let model = self.pending_model.take()?;
        self.child.as_ref()?; // a child that is down starts on the new model anyway
        let (provider, id) = model.split_once('/')?;
        let cmd = json!({"type": "set_model", "provider": provider, "modelId": id});
        let Err(e) = self.call(cmd).await else {
            self.read_state().await; // the new model's context window
            return None;
        };
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
        self.restore_steers()?;
        let snap = self.call(json!({"type": "get_messages"})).await?;
        let rows = self.store.inbox_with_status("dispatched")?;
        if snap["pendingResume"] == true {
            if self.store.pause_reason()?.is_some() {
                return Ok(()); // `/resume` continues the interrupted run
            }
            if self.resumes < MAX_RESUMES {
                self.resumes += 1;
                self.call(json!({"type": "resume_pending"})).await?;
                // Delivered at the resumed run's next step; `reconcile` never takes it for a row.
                let note = crate::scheduler::RESUME_NOTE;
                let note = json!({"type": "queue_message", "message": note, "deliverAs": "steer"});
                self.call(note).await?;
                self.note_pending = true;
                self.busy = true;
                self.begin_run().await;
                return Ok(());
            }
            for row in &rows {
                let text = "[zephyr] the run was interrupted and could not be resumed";
                self.store.reply(row, text, "failed")?;
            }
            self.resumes = 0;
            self.forget_steers();
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
                    // A steered message is answered on its own (`steer_event`); the run's last
                    // text still belongs to the turn that started it.
                    // Nor is the resume note (E4) anyone's message.
                    if !self.steered_texts.contains(&text) && text != crate::scheduler::RESUME_NOTE
                    {
                        current = Some(text);
                    }
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
        // A model can answer a steered message and stop, leaving the run's own request without
        // an answer: that row goes once more, so the work carries on (DESIGN §4.1).
        let steers = !self.steered_texts.is_empty() && std::mem::take(&mut self.run_completed);
        for row in rows {
            // The form it was last sent in first: history holds its earlier forms too.
            let reply = replies
                .get(&continued(&row))
                .or_else(|| replies.get(&steered(&row)))
                .or_else(|| replies.get(&tagged(&row)));
            match reply {
                Some(Some(reply)) if reply.trim() == NO_REPLY => {
                    self.store.set_status(row.id, "done")?
                }
                Some(Some(reply)) if self.steer_sent.contains(reply) && row.tries < 2 => {
                    self.store.requeue_as_continue(row.id)?
                }
                // Already sent to a steered message: once is enough.
                Some(Some(reply)) if self.steer_sent.contains(reply) => {
                    self.store.set_status(row.id, "done")?
                }
                Some(Some(reply)) => self.store.reply(&row, &unspoof(reply), "done")?,
                Some(None) if steers && !requeue && row.tries < 2 => {
                    self.store.requeue_as_continue(row.id)?
                }
                // A failover sends it again, but not forever.
                Some(None) if requeue && row.tries < 5 => self.store.set_status(row.id, "new")?,
                Some(None) => {
                    let text = fallback
                        .as_deref()
                        .unwrap_or("[zephyr] the run ended without a reply");
                    self.store.reply(&row, text, "done")?
                }
                None => missing.push(row),
            }
        }
        // A follow-up queued as the run ended stays in rpi's queue, and so does a resume note
        // the resumed run never reached: take them back, or the note opens the next run.
        if !missing.is_empty() || std::mem::take(&mut self.note_pending) {
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
        self.forget_steers();
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
                attachment: row.attachment.clone().map(Into::into),
                reply_to: row.reply_to,
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
            if let Some(file) = &row.attachment {
                crate::tools::discard(file);
            }
        }
    }

    /// Shows `text`, the router's own (a command reply, a notice), to attached clients:
    /// `attach` draws it for stock clients as an assistant message, the TUI as a reply card.
    fn show(&self, text: &str, command: Option<&str>) {
        let reply = json!({"type": "zephyr_reply", "text": text, "command": command});
        let _ = self.tap.send(reply); // no receivers is fine
    }

    /// SIGTERM: let a running turn settle, flush, then stop the child. Nothing more is
    /// dispatched; messages that come in meanwhile get their rows, for the next start.
    async fn drain<C: Channel>(&mut self, channels: &[C], rx: &mut mpsc::Receiver<Inbound>) {
        self.stopping = true;
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.busy {
            match tokio::time::timeout_at(deadline, next_event(&mut self.child)).await {
                // As in `main`, so the run's spend and outbound list are booked.
                Ok(Some(ev)) => match self.tapped(Some(ev)).await {
                    Some(ev) if ev["type"] == "agent_settled" => self.settled(&ev).await,
                    _ => {}
                },
                _ => break, // child gone or out of time: recovery picks it up next start
            }
        }
        if self.busy {
            // Cut off: book what the run did so far.
            let main = crate::rpc::Tally::main(&self.cfg.home);
            let tally = std::mem::replace(&mut self.tally, main);
            log(self.record_spend(tally.used).map(drop));
            let to = self.store.last_owner().ok().flatten();
            crate::tools::apply(&self.store, &self.cfg, &tally.requests, to);
            if let Some(alert) = tally.alert().map(|a| crate::facts::scrubbed(&self.cfg, a)) {
                log(self.store.notify_last_owner(&alert).map_err(Into::into));
            }
        }
        self.take_in(rx).await;
        self.flush(channels).await;
        if let Some(child) = self.child.take() {
            child.stop().await;
        }
        // Last, and nothing awaits a channel after it: a closed intake ends the adapters' runs,
        // which ends the gateway.
        rx.close();
        self.attaches.close();
        self.take_in(rx).await;
    }

    /// Writes the waiting messages' rows (and answers commands) while draining.
    async fn take_in(&mut self, rx: &mut mpsc::Receiver<Inbound>) {
        while let Ok(m) = rx.try_recv() {
            self.inbound(m, false).await;
        }
        while let Ok(req) = self.attaches.try_recv() {
            self.attach(req).await;
        }
    }

    async fn call(&mut self, cmd: Value) -> anyhow::Result<Value> {
        match &mut self.child {
            Some(c) => c.call(cmd).await,
            None => anyhow::bail!("rpi child is not running"),
        }
    }

    /// The repeated-failure guard (DESIGN §4.4): at `GUARD_STEER` identical failures in a row a
    /// note is steered in, at `GUARD_STOP` the run is stopped. True when it stopped the run.
    async fn guard_run(&mut self) -> bool {
        match self.tally.guard() {
            Some(crate::rpc::Guard::Steer(note)) => {
                eprintln!("zephyr: {note}");
                self.steer_note(note).await;
                false
            }
            Some(crate::rpc::Guard::Stop(why)) => {
                let notice = format!("[zephyr] stopped a run that {why}");
                eprintln!("zephyr: {notice}");
                self.cut = Some(notice);
                self.cut_at = Some(Instant::now());
                self.abort().await;
                true
            }
            None => false,
        }
    }

    /// Steers the daemon's own `note` into the running turn: the repeated-failure guard's, or
    /// what a held call means. Returns whether rpi took it.
    async fn steer_note(&mut self, note: String) -> bool {
        let steer = json!({"type": "queue_message", "message": note, "deliverAs": "steer"});
        let queued = self.call(steer).await.map(drop);
        let took = queued.is_ok();
        log(queued);
        // Not the owner's message: `reconcile` never takes it for the run's request.
        self.steered_texts.insert(note);
        // A run that ends before reaching it would leave it to open the next run: the settle
        // takes it back, as it does the resume note.
        self.note_pending = true;
        took
    }

    /// The gate's held note for main's run (R51), before it answers the plugin `hold`: steered
    /// in now, while rpi still waits on the call, so the step after the refused call reads it
    /// (rpi takes its steers as the tool batch ends). `note.queued` is told once rpi has it; a
    /// note for a session or a run that is not this one is dropped, which tells the gate at once.
    async fn gate_note(&mut self, session: &str, mut note: crate::scheduler::Note) {
        let ours = self.child.as_ref().and_then(|c| c.session.as_deref()) == Some(session);
        if !(self.busy && ours) || self.held_steered.contains(&note.action) {
            return;
        }
        if self.steer_note(note.text).await {
            self.held_steered.insert(note.action);
            if let Some(queued) = note.queued.take() {
                let _ = queued.send(());
            }
        }
    }

    /// `/undo [n]` (DESIGN §4.1), idle only: takes the last `n` exchanges (`turns`) out of the
    /// agent's context. rpi's `navigate` puts the session leaf on its target, which stays, so the
    /// target is the entry just before the oldest one's message (none: the root, an empty
    /// context). Every row in what was cut becomes `undone`, so recovery never sends it again.
    /// Returns that oldest exchange's row, or the reply saying why nothing was undone.
    async fn undo(&mut self, n: usize) -> Result<InboxRow, String> {
        if self.busy {
            return Err(BUSY.into());
        }
        let failed = |e: &dyn std::fmt::Display| format!("[zephyr] /undo failed: {e:#}");
        let snap = (self.call(json!({"type": "get_messages"})).await).map_err(|e| failed(&e))?;
        let messages = snap["messages"].as_array().map_or(&[][..], Vec::as_slice);
        let turns = turns(messages);
        let Some(&at) = turns.len().checked_sub(n).map(|i| &turns[i]) else {
            return Err(match turns.len() {
                0 => "[zephyr] nothing to undo".into(),
                k => format!("[zephyr] only {k} of your messages to undo"),
            });
        };
        let rows: Vec<i64> = (messages[at..].iter())
            .filter(|m| m["kind"] == "user")
            .filter_map(|m| untag(&message_text(m)).map(|(_, id, _)| id))
            .collect();
        let row = match self.store.inbox_row(rows[0]) {
            Ok(Some(row)) => row,
            Ok(None) => return Err(failed(&format!("no inbox row #{}", rows[0]))),
            Err(e) => return Err(failed(&e)),
        };
        let mut navigate = json!({"type": "navigate"});
        if let Some(id) = messages[..at]
            .iter()
            .rev()
            .find_map(|m| m["entryId"].as_str())
        {
            navigate["targetId"] = json!(id);
        }
        let moved = self.call(navigate).await.map_err(|e| failed(&e))?;
        if moved["outcome"] != "completed" {
            return Err(failed(&format!("rpi answered {moved}")));
        }
        for id in rows {
            self.store.mark_undone(id).map_err(|e| failed(&e))?;
        }
        Ok(row)
    }

    /// `/retry`: `/undo`, then the same text again as a new row on the same channel and sender,
    /// platform id `retry-<row>-<n>`, dispatched like any message.
    async fn retry(&mut self) -> String {
        let row = match self.undo(1).await {
            Ok(row) => row,
            Err(reply) => return reply,
        };
        let mut n = 1;
        let id = loop {
            let again = Inbound {
                channel: row.channel.clone(),
                platform_msg_id: format!("retry-{}-{n}", row.id),
                sender: row.sender.clone(),
                text: row.text.clone(),
            };
            match self.store.insert_inbox(&again) {
                Ok(Some(id)) => break id,
                Ok(None) => n += 1,
                Err(e) => return format!("[zephyr] undone, but not sent again: {e}"),
            }
        };
        // Dispatched from the loop, as a task's report is: this `/retry` row is still `new`.
        self.jobs.inbox.notify_one();
        format!("[zephyr] undone; sending your last message again as #{id}")
    }

    /// `/compact [instructions]`, idle only: rpi summarises the conversation in a run of its own,
    /// and its settle (`compacted`) tells the asker the summary. Returns "" when it will.
    async fn compact(&mut self, instructions: &str, from: (&str, &str)) -> String {
        if self.busy {
            return BUSY.into();
        }
        let mut cmd = json!({"type": "compact"});
        if !instructions.is_empty() {
            cmd["customInstructions"] = json!(instructions);
        }
        if let Err(e) = self.call(cmd).await {
            return format!("[zephyr] /compact failed: {e:#}");
        }
        self.busy = true;
        self.begin_run().await;
        self.compacting = Some((from.0.into(), from.1.into()));
        String::new()
    }

    /// At a settle: the summary of a `/compact`, to whoever asked for it.
    fn compacted(&mut self, ev: &Value) {
        let Some((channel, to)) = self.compacting.take() else {
            return;
        };
        let text = match (ev["outcome"].as_str(), ev["summary"].as_str()) {
            (Some("compacted"), Some(summary)) => format!("[zephyr] {summary}"),
            (Some("compacted"), None) => "[zephyr] compacted".into(),
            (outcome, _) => format!(
                "[zephyr] /compact {}: {}",
                outcome.unwrap_or("ended"),
                ev["error"].as_str().unwrap_or("no reason given")
            ),
        };
        log(self.store.notify(&channel, &to, &text).map_err(Into::into));
    }

    /// `/usage [days]` (default 7): spend per UTC day, newest first, with each account's share.
    fn usage(&self, days: &str) -> String {
        let days = match days {
            "" => 7,
            days => match days.parse::<u32>() {
                Ok(n @ 1..=366) => n,
                _ => return "[zephyr] usage: /usage [days], 1 to 366 (default 7)".into(),
            },
        };
        let (totals, accounts) = match self.store.usage(days) {
            Ok(usage) => usage,
            Err(e) => return format!("[zephyr] /usage failed: {e}"),
        };
        let plural = if days == 1 { "" } else { "s" };
        let mut lines = vec![format!(
            "[zephyr] usage over the last {days} day{plural} (UTC):"
        )];
        if totals.is_empty() {
            lines.push("nothing spent".into());
        }
        let (mut tokens, mut cost) = (0, 0.0);
        for (day, t, c) in &totals {
            lines.push(format!("{day}: {t} tokens, ${c:.4}"));
            for (_, account, t, c) in accounts.iter().filter(|a| &a.0 == day) {
                lines.push(format!("  {account}: {t} tokens, ${c:.4}"));
            }
            (tokens, cost) = (tokens + t, cost + c);
        }
        if totals.len() > 1 {
            lines.push(format!("total: {tokens} tokens, ${cost:.4}"));
        }
        lines.join("\n")
    }
}

/// The first word of `s` and the rest, both trimmed; a line break ends a word too.
/// Whether the current run of main is the owner's alone (DESIGN §4.6).
#[derive(Debug)]
enum Trust {
    /// Started on a clean conversation, a clean memory base and nothing else running.
    Owner { base: String, since: i64 },
    /// Why it is not: the waiting notice's `(from …)`.
    Not(String),
}

/// `Trust::Not`'s labels for work beside the run and for a change already waiting.
const BESIDE: &str = "your message, while other work of the agent's was running";
const ON_WAITING: &str = "your message, on top of a change still waiting";

/// What taking in an inbox row with this platform id means for the conversation, when the row
/// is the daemon's (a task's or a process's report, a held action's result) and not the owner's.
fn daemon_row(platform_id: &str) -> Option<String> {
    let (what, n) = platform_id.split_once('-')?;
    match what {
        "task" => Some(format!("took in task #{n}'s report")),
        "proc" => Some(format!("took in process #{n}'s report")),
        "action" => Some(format!("took in action #{n}'s result")),
        _ => None,
    }
}

fn word(s: &str) -> (&str, &str) {
    let s = s.trim();
    s.split_once(char::is_whitespace)
        .map_or((s, ""), |(w, rest)| (w, rest.trim()))
}

/// The account `reload::set_model` names for `model` in `doc`, read back out of the document it
/// just wrote: the write's own derivation, so a check and a write talk about one account.
fn model_account(doc: &toml_edit::DocumentMut, model: &str) -> Option<String> {
    crate::host::conf::accounts(doc)
        .into_iter()
        .find(|(_, m)| m == model)
        .map(|(name, _)| name)
}

/// `m` checked as `/model` checks a model: PROVIDER/MODEL, an account on its provider, and that
/// account's rpi lists it.
async fn check_model(cfg: &Config, m: &str) -> Result<(), String> {
    if !zephyr_policy::tools::model_shape(m) {
        return Err(format!("not changed: {m} is not PROVIDER/MODEL"));
    }
    let provider = m.split('/').next().unwrap_or_default();
    let account = cfg.accounts.iter().find(|a| a.provider() == provider);
    if !cfg.accounts.is_empty() && account.is_none() {
        return Err(format!(
            "not changed: no account is signed in to {provider}; run `zephyr login {provider}`"
        ));
    }
    if !lists(cfg, account, m).await {
        return Err(format!("rpi has no model {m}"));
    }
    Ok(())
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

/// Agent text as it goes out: a line that starts `[zephyr]` is shown as `[agent]`, so the agent
/// cannot pass for a daemon notice (to bait an `/ok`, say). Never applied to the daemon's own.
fn unspoof(text: &str) -> String {
    let lines = text.split('\n').map(|line| {
        let body = line.trim_start();
        match body.strip_prefix("[zephyr]") {
            Some(rest) => format!("{}[agent]{rest}", &line[..line.len() - body.len()]),
            None => line.to_string(),
        }
    });
    lines.collect::<Vec<_>>().join("\n")
}

fn log(r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("zephyr: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config on `<dir>/zephyr.toml` holding `extra`, `owner` the owner on `chat`.
    fn on_file(name: &str, extra: &str) -> Config {
        let dir = crate::test_dir(name);
        let path = dir.join("zephyr.toml");
        let text = format!(
            "rpi = '/nonexistent/rpi'\nhome = {:?}\n{extra}\n[owners]\nchat = ['owner']\n",
            dir.display().to_string()
        );
        std::fs::write(&path, text).unwrap();
        Config::load(&path).unwrap()
    }

    /// A router on `cfg` with no child, so `set_setting` can be driven as any actor.
    fn router<'a>(cfg: &Config, jobs: &'a crate::scheduler::Wake) -> Router<'a> {
        let (_, attaches) = mpsc::channel(1);
        let (cfg_tx, _) = watch::channel(Arc::new(cfg.clone()));
        let (snap, _) = watch::channel(Snapshot::default());
        Router::new(cfg, jobs, attaches, cfg_tx, snap, vec!["chat".into()]).unwrap()
    }

    const OWNER: Actor = Actor::Owner { channel: "chat" };
    const AGENT: Actor = Actor::Agent { session: "main" };

    #[tokio::test]
    async fn the_agent_never_cancels_or_replaces_the_owners_time_limit() {
        let cfg = on_file("set-agent-revert", "[budget]\ndaily_cost_usd = 5.0");
        let jobs = Default::default();
        let mut r = router(&cfg, &jobs);
        let key = "budget.daily_cost_usd";
        let hour = Some(Duration::from_secs(3600));
        let short = Some(Duration::from_secs(30));
        let set = r.set_setting(key, "8", OWNER, hour, None).await;
        assert!(matches!(set, Outcome::Changed { .. }), "{set:?}");
        let limit = r.store.revert(key).unwrap().unwrap();
        assert_eq!(limit.value.as_deref(), Some("5.0"));
        assert_eq!(limit.channel.as_deref(), Some("chat"));
        // A tightening, then a timed tightening: both apply, and the owner's limit stays.
        let tight = r.set_setting(key, "7.99", AGENT, None, Some("asked")).await;
        let (old, new) = (Some("8".into()), Some("7.99".into()));
        let applied = APPLIED.into();
        assert_eq!(tight, Outcome::Changed { old, new, applied });
        assert_eq!(r.store.revert(key).unwrap().as_ref(), Some(&limit));
        let timed = r.set_setting(key, "7.5", AGENT, short, None).await;
        assert!(matches!(timed, Outcome::Changed { .. }), "{timed:?}");
        assert_eq!(r.store.revert(key).unwrap().as_ref(), Some(&limit));
        // The owner's own change cancels it.
        r.set_setting(key, "6", OWNER, None, None).await;
        assert_eq!(r.store.revert(key).unwrap(), None);
        // With none pending, the agent's `for D` goes back to the value before its change.
        r.set_setting(key, "4", AGENT, short, None).await;
        let own = r.store.revert(key).unwrap().unwrap();
        let got = (own.value.as_deref(), own.at - own.since, own.channel);
        assert_eq!(got, (Some("6.0"), 30, None));
    }

    /// R30: a reload reads the file again, but what no file sets (tests shorten these) stays.
    #[tokio::test]
    async fn a_reload_keeps_what_no_file_sets() {
        let mut cfg = on_file("reload-hidden", "");
        cfg.tick = Duration::from_millis(200);
        cfg.stall_limit = Duration::from_secs(3);
        cfg.min_until = Duration::from_secs(1);
        cfg.job_timeout = Duration::from_secs(4);
        cfg.exec_backstop = Duration::from_secs(5);
        cfg.projects = Some("/p".into());
        cfg.agent_browser = "/fake/agent-browser".into();
        let jobs = Default::default();
        let mut r = router(&cfg, &jobs);
        let set = r
            .set_setting("tasks.parallel", "3", OWNER, None, None)
            .await;
        assert!(matches!(set, Outcome::Changed { .. }), "{set:?}");
        assert_eq!(r.cfg.tasks.as_ref().map(|t| t.parallel), Some(3));
        let c = &r.cfg;
        assert_eq!(
            (
                c.tick,
                c.stall_limit,
                c.min_until,
                c.job_timeout,
                c.exec_backstop
            ),
            (
                cfg.tick,
                cfg.stall_limit,
                cfg.min_until,
                cfg.job_timeout,
                cfg.exec_backstop
            )
        );
        assert_eq!((&c.path, &c.projects), (&cfg.path, &cfg.projects));
        assert_eq!(c.agent_browser, cfg.agent_browser);
        assert_eq!(r.cfg_tx.borrow().tick, cfg.tick, "and what the others read");
    }

    /// R49: main is woken once for each new note about an action, not on every tick while it
    /// waits undelivered (each wake moves main's running command to the background).
    #[tokio::test]
    async fn main_is_woken_once_per_action_note() {
        let cfg = on_file("actions-wake", "");
        let jobs = Default::default();
        let mut r = router(&cfg, &jobs);
        let note = |r: &Router, id| {
            r.store
                .action_note(id, "declined", "the owner declined it")
                .unwrap()
        };
        let held = |r: &Router| r.store.hold_action(&held_action(None)).unwrap();
        assert!(!r.settled_actions(), "nothing new");
        let (a, b) = (held(&r), held(&r));
        assert!(note(&r, a));
        assert!(r.settled_actions());
        assert!(!r.settled_actions(), "the same row, still undelivered");
        assert!(note(&r, b));
        assert!(r.settled_actions(), "a new one");
    }

    /// I1: a delay that came due during a pause is asked again on the first tick after the
    /// pause lifted by itself (a budget pause at midnight), never run.
    #[tokio::test]
    async fn a_delay_due_when_a_pause_lifts_is_asked_again() {
        let cfg = on_file(
            "actions-lift",
            "timezone = 'UTC'\n[approvals]\nmessages = 'delay 5'",
        );
        let jobs = Default::default();
        let mut r = router(&cfg, &jobs);
        r.store.set_paused(Some("user")).unwrap();
        r.tick_actions();
        // Due now, held five minutes ago as `delay 5` holds one (R63 reads how long), and the
        // pause gone with no /resume: as a budget pause ends.
        let delayed = |r: &Router<'_>| {
            let due = crate::now() - 1;
            let id = r.store.hold_action(&held_action(Some(due))).unwrap();
            let db = r.store.db_for_test();
            let set = "UPDATE proposals SET created_at = ?2 WHERE id = ?1";
            db.execute(set, rusqlite::params![id, due - 300]).unwrap();
            id
        };
        let id = delayed(&r);
        r.store.set_paused(None).unwrap();
        r.tick_actions();
        assert_eq!(r.store.action(id).unwrap().unwrap().status, "pending");
        let card = r.store.pending_outbox().unwrap().pop().unwrap().text;
        assert!(
            card.starts_with(&format!("[zephyr] approve #{id}:")),
            "{card}"
        );
        // The tick after that runs delays again.
        let next = delayed(&r);
        r.tick_actions();
        assert_eq!(r.store.action(next).unwrap().unwrap().status, "done");
    }

    /// A connector message held from main on `chat`, delayed to `due` when given.
    fn held_action(due: Option<i64>) -> crate::store::NewAction {
        crate::store::NewAction {
            kind: crate::store::ActionKind::Pass,
            class: "messages".into(),
            targets: vec!["mail/send".into(), "a@x.org".into()],
            summary: "mail/send".into(),
            reason: None,
            payload: r#"{"server": "mail", "tool": "send", "arguments": {}}"#.into(),
            secrets: vec![],
            origin: crate::store::Origin {
                channel: "chat".into(),
                sender: "owner".into(),
                session: "main".into(),
                run: "main:1".into(),
                call_id: None,
            },
            due_at: due,
        }
    }

    /// The owner's `/ok N` on `chat`, and the reply it got.
    async fn ok(r: &mut Router<'_>, id: i64) -> String {
        let m = Inbound {
            channel: "chat".into(),
            platform_msg_id: format!("ok-{id}"),
            sender: "owner".into(),
            text: format!("/ok {id}"),
        };
        r.inbound(m, false).await;
        let sent = r.store.pending_outbox().unwrap();
        sent.last().unwrap().text.clone()
    }

    #[tokio::test]
    async fn a_waiting_rule_or_subagent_change_goes_stale_after_any_edit() {
        let cfg = on_file("set-list-stale", "");
        let jobs = Default::default();
        let mut r = router(&cfg, &jobs);
        let proposed = |o| match o {
            Outcome::Proposed { id } => id,
            other => panic!("{other:?}"),
        };
        for rule in ["be brief", "reply in Korean"] {
            r.set_setting("rule", rule, OWNER, None, None).await;
        }
        // Two rules before and after: `rm 2` would now remove another rule than the one shown.
        let rm = proposed(r.set_setting("rule", "rm 2", AGENT, None, None).await);
        r.set_setting("rule", "rm 1", OWNER, None, None).await;
        r.set_setting("rule", "never email bob", OWNER, None, None)
            .await;
        let stale = "[zephyr] rule is now 2 rules; propose again";
        assert_eq!(ok(&mut r, rm).await, stale);
        let rules = crate::memory::read_rules(r.cfg.state_dir()).unwrap();
        assert_eq!(rules, ["reply in Korean", "never email bob"]);
        // An edit that adds one makes it stale too; one unchanged since it was shown runs.
        let rm = proposed(r.set_setting("rule", "rm 2", AGENT, None, None).await);
        r.set_setting("rule", "be kind", OWNER, None, None).await;
        let stale = "[zephyr] rule is now 3 rules; propose again";
        assert_eq!(ok(&mut r, rm).await, stale);
        let rm = proposed(r.set_setting("rule", "rm 3", AGENT, None, None).await);
        assert_eq!(
            ok(&mut r, rm).await,
            format!("[zephyr] /ok {rm} approved /rule rm 3\n[zephyr] removed rule 3: be kind")
        );

        // A subagent whose prompt changed keeps its name, but the proposal is stale all the same.
        r.set_setting("subagent", "add y Build.", OWNER, None, None)
            .await;
        let edit =
            proposed((r.set_setting("subagent", "prompt y Explore.", AGENT, None, None)).await);
        let own = r
            .set_setting("subagent", "prompt y Build well.", OWNER, None, None)
            .await;
        assert!(matches!(own, Outcome::Changed { .. }), "{own:?}");
        assert_eq!(
            ok(&mut r, edit).await,
            "[zephyr] subagent is now y; propose again"
        );
        assert_eq!(r.cfg.subagents[0].prompt, "Build well.");
    }

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
            platform_id: "p".into(),
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
    fn turns_skip_steers_continuations_and_untagged_notes() {
        let user = |text: &str| json!({"kind": "user", "content": text});
        let messages = [
            user("[chat #1] one"),
            json!({"kind": "assistant", "content": [{"type": "toolCall", "name": "bash"}]}),
            json!({"kind": "toolResult", "content": "ok"}),
            user("[tui #2] steered in"),
            json!({"kind": "assistant", "content": [{"type": "text", "text": "done"}]}),
            user("[chat #1] one\n\n(The work on this request stopped ...)"),
            user(crate::scheduler::RESUME_NOTE),
            user("[discord #3] two"),
        ];
        assert_eq!(turns(&messages), [0, 7]);
        let md = markdown("main", &messages);
        assert!(
            md.starts_with("# zephyr conversation main\n\n## chat\n\none\n"),
            "{md}"
        );
        assert!(md.contains("\n## agent\n\n- `bash` null\n"), "{md}");
        assert!(md.contains("\n## tui\n\nsteered in\n\n## agent\n"), "{md}");
        assert!(md.ends_with("\n## discord\n\ntwo\n"), "{md}");
    }

    #[test]
    fn previews_fold_whitespace_and_cut_at_60_chars() {
        assert_eq!(preview("a\n\n b"), "a b");
        assert_eq!(preview(&"é".repeat(61)), format!("{}…", "é".repeat(60)));
        assert_eq!(preview(&"é".repeat(60)), "é".repeat(60));
    }
}
