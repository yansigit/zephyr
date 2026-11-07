//! Scheduled jobs (DESIGN §4.5). A tick claims due runs; runs go one at a time, each in a
//! fresh `job-<job id>-<run id>` child, so `main` never waits on a job. While the gateway is
//! paused nothing is claimed or started, and a running job is aborted.

use crate::channel::Inbound;
use crate::config::{Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child};
use crate::store::Store;
use croner::parser::{CronParser, Seconds, Year};
use jiff::{tz::TimeZone, Timestamp};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};

const TICK: Duration = Duration::from_secs(30);
/// How often background work's old leftovers are swept (`retention`).
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// How often the pause flag (set by `/pause` or a budget trip, in the store) is read.
const PAUSE_POLL: Duration = Duration::from_secs(1);
/// The DESIGN §4.4 per-run wall clock cap; `Config::job_timeout` defaults to it.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A run whose whole reply is this is not delivered.
const SILENT: &str = "[SILENT]";
const USAGE: &str = "[zephyr] usage: /job add <name> | <cron or RFC3339 time> | <prompt>, /job list, /job ok <id>, /job rm <id>, /job run <id>";

/// Wakeups between the router and the scheduler.
#[derive(Default)]
pub struct Wake {
    /// A run was queued by `/job run`.
    pub runs: Notify,
    /// The scheduler wrote to the outbox.
    pub outbox: Notify,
    /// A job run is in progress, so the daemon does not re-exec under it.
    pub running: AtomicBool,
    /// A task was queued or asked to stop (DESIGN §4.12).
    pub tasks: Notify,
    /// A task wrote its report to the inbox: the router dispatches it.
    pub inbox: Notify,
}

/// One unattended prompt in a fresh child: a job run or a background task.
pub struct Spec<'a> {
    pub prompt: &'a str,
    /// What the run is about, for its checkpoint's label: a job's prompt, a task's goal.
    pub goal: &'a str,
    /// `[zephyr] job #1 (name) run 2`: how failures name the run.
    pub label: String,
    /// The first attempt's session id; a retry on another account adds `-<n>`.
    pub session: String,
    /// The `(channel, recipient)` its `send_file`, `schedule` and notices go to.
    pub to: (String, String),
    /// What its `gateway` and `task` calls get: neither belongs to an unattended run.
    pub refuse: &'static str,
    /// The task whose stop flag also ends it.
    pub task: Option<i64>,
    /// Reopen `session` and carry on (DESIGN §4.12) instead of sending `prompt` to a fresh one.
    pub resume: bool,
    /// `provider/model` to run on instead of the account's own (a task's `model`, DESIGN
    /// §4.12): only that provider's accounts are tried.
    pub model: Option<&'a str>,
}

/// What a run resumed after a restart is told (OpenClaw's restart-recovery wording): rpi
/// records a tool call cut mid-flight as of unknown outcome, so the model checks before redoing.
pub const RESUME_NOTE: &str = "Zephyr restarted while you were working. Check the current \
    state of anything that was in progress (files, processes, tasks, external actions) before \
    repeating it, then continue.";

/// `cfg.timezone`, else the system's.
pub fn zone(cfg: &Config) -> anyhow::Result<TimeZone> {
    match &cfg.timezone {
        Some(name) => TimeZone::get(name).map_err(|e| anyhow::anyhow!("timezone {name:?}: {e}")),
        None => Ok(TimeZone::system()),
    }
}

/// The first time after `after` (unix seconds) that `schedule` fires: a five-field cron in
/// `tz`, or a one-shot RFC3339 time.
pub fn next_after(schedule: &str, after: i64, tz: &TimeZone) -> Option<i64> {
    if let Ok(t) = schedule.parse::<Timestamp>() {
        return (t.as_second() > after).then_some(t.as_second());
    }
    let after = Timestamp::from_second(after).ok()?.to_zoned(tz.clone());
    let next = parse_cron(schedule)
        .ok()?
        .find_next_occurrence(&after, false);
    next.ok().map(|t| t.timestamp().as_second())
}

/// Fails closed: a pause flag that cannot be read counts as paused.
fn paused(store: &Store) -> bool {
    store
        .pause_reason()
        .unwrap_or_else(|e| {
            eprintln!("zephyr: reading the pause flag failed: {e}");
            Some(String::new())
        })
        .is_some()
}

/// Waits until the gateway is paused (`": paused"`) or `task`'s stop flag is set (`""`).
async fn until_cut(store: &mut Store, task: Option<i64>) -> &'static str {
    loop {
        tokio::time::sleep(PAUSE_POLL).await;
        if paused(store) {
            return ": paused";
        }
        // Fails closed like the pause flag: an unreadable flag stops the task.
        if task.is_some_and(|id| store.stop_requested(id).unwrap_or(true)) {
            return "";
        }
    }
}

fn parse_cron(schedule: &str) -> Result<croner::Cron, String> {
    if schedule.split_whitespace().count() != 5 {
        return Err("a cron schedule has five fields".into());
    }
    let parser = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build();
    parser.parse(schedule).map_err(|e| e.to_string())
}

/// Runs until an error opening the store; per-run errors are logged and the loop goes on.
/// `cfgs` carries each reloaded config; a run started before a reload finishes on the old one.
pub async fn run(mut cfgs: watch::Receiver<Arc<Config>>, wake: &Wake) -> anyhow::Result<()> {
    let mut cfg = cfgs.borrow_and_update().clone();
    let mut tz = zone(&cfg)?;
    let mut store = Store::open(&cfg.db_path())?;
    if store.interrupt_running_runs(crate::now())? > 0 {
        wake.outbox.notify_one();
    }
    let mut tick = tokio::time::interval(TICK);
    let mut poll = tokio::time::interval(PAUSE_POLL);
    let mut was_paused = false;
    let mut swept: Option<Instant> = None;
    let mut sweeping: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let woke = tokio::select! {
            _ = tick.tick() => true,
            _ = wake.runs.notified() => true,
            _ = poll.tick() => false,
        };
        if cfgs.has_changed().unwrap_or(false) {
            cfg = cfgs.borrow_and_update().clone();
            tz = zone(&cfg)?; // checked by Config::validate
        }
        // Hourly, from the first tick on: leftovers older than 30 days (DESIGN §4.1).
        // On a blocking thread with its own connection, one at a time: it walks dirs and runs
        // checkpoint git, which must never stall the daemon's one future.
        let idle = sweeping.as_ref().is_none_or(|h| h.is_finished());
        if woke && idle && swept.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
            swept = Some(Instant::now());
            let cfg = cfg.clone();
            sweeping = Some(tokio::task::spawn_blocking(move || {
                let store = Store::open(&cfg.db_path());
                match store.and_then(|s| crate::retention::sweep(&cfg, &s, crate::retention::AGE)) {
                    Ok(0) => {}
                    Ok(n) => {
                        eprintln!("zephyr: removed {n} files of background work older than 30 days")
                    }
                    Err(e) => eprintln!("zephyr: sweeping old background files failed: {e}"),
                }
            }));
        }
        let now_paused = paused(&store);
        // A resume works through what waited without waiting for the next tick.
        let resumed = std::mem::replace(&mut was_paused, now_paused) && !now_paused;
        if now_paused || !(woke || resumed) {
            continue;
        }
        if let Err(e) = store.claim_due(crate::now(), |s, t| next_after(s, t, &tz)) {
            eprintln!("zephyr: claiming job runs failed: {e}");
        }
        while !paused(&store) {
            let run = match store.start_next_run(crate::now()) {
                Ok(Some(run)) => run,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("zephyr: starting a job run failed: {e}");
                    break;
                }
            };
            wake.running.store(true, Ordering::SeqCst);
            let spec = Spec {
                prompt: &run.job.prompt,
                goal: &run.job.prompt,
                label: format!(
                    "[zephyr] job #{} ({}) run {}",
                    run.job.id, run.job.name, run.id
                ),
                session: format!("job-{}-{}", run.job.id, run.id),
                to: (run.job.channel.clone(), run.job.recipient.clone()),
                refuse: "not available in scheduled runs",
                task: None,
                resume: false,
                model: None,
            };
            let (status, output, crossed) = execute(&cfg, &mut store, &spec).await;
            wake.running.store(false, Ordering::SeqCst);
            // The run's memory edits are committed as main's settle would commit them.
            let memory = crate::memory::commit(&cfg.home, cfg.state_dir())
                .await
                .unwrap_or_else(|e| {
                    eprintln!("zephyr: memory commit failed: {e:#}");
                    None
                });
            let delivery = (output.trim() != SILENT).then_some(output.as_str());
            if let Err(e) = store.finish_run(&run, status, &output, delivery, crate::now()) {
                eprintln!("zephyr: recording job run {} failed: {e}", run.id);
            }
            if let Some(notice) = memory {
                if let Err(e) = store.notify(&run.job.channel, &run.job.recipient, &notice) {
                    eprintln!("zephyr: memory notice failed: {e}");
                }
            }
            // Skill and connector drafts the run wrote (DESIGN §4.6).
            for notice in crate::drafts::scan(&cfg) {
                if let Err(e) = store.notify(&run.job.channel, &run.job.recipient, &notice) {
                    eprintln!("zephyr: draft notice failed: {e}");
                }
            }
            if crossed {
                let (channel, recipient) = (&run.job.channel, &run.job.recipient);
                if let Err(e) = store.notify(channel, recipient, BUDGET_NOTICE) {
                    eprintln!("zephyr: budget notice failed: {e}");
                }
            }
            wake.outbox.notify_one();
        }
    }
}

/// One prompt in a fresh child, bounded by `cfg.job_timeout` and aborted by a pause (or, for a
/// task, its stop flag). The run's usage is charged to today's budget. Returns `(status,
/// output, crossed the budget)`; status is `completed`, `failed` or `stopped`.
pub async fn execute(
    cfg: &Config,
    store: &mut Store,
    spec: &Spec<'_>,
) -> (&'static str, String, bool) {
    let label = &spec.label;
    // The projects as they were before this run (DESIGN §4.14); a resumed run has one.
    if !spec.resume {
        let what = label.strip_prefix("[zephyr] ").unwrap_or(label);
        let text = format!("{what}: {}", spec.goal);
        crate::checkpoint::before(cfg, &text).await;
    }
    // A requested model runs only on its provider's accounts, each as that model; with no pool
    // it stands in for `cfg.model`.
    let provider = spec.model.map(|m| m.split('/').next().unwrap_or_default());
    let accounts: Vec<crate::config::Account> = match spec.model {
        None => cfg.accounts.clone(),
        Some(m) => cfg
            .accounts
            .iter()
            .filter(|a| Some(a.provider()) == provider)
            .map(|a| crate::config::Account {
                name: a.name.clone(),
                model: m.into(),
            })
            .collect(),
    };
    let own;
    let cfg = match (spec.model, cfg.accounts.is_empty()) {
        (Some(m), true) => {
            own = Config {
                model: Some(m.into()),
                ..cfg.clone()
            };
            &own
        }
        (Some(_), false) if accounts.is_empty() => {
            let why = format!("no account on {}", provider.unwrap_or_default());
            return ("failed", format!("{label} could not start: {why}"), false);
        }
        _ => cfg,
    };
    // A job runs on the first account not cooling down. A rate-limited attempt cools that
    // account and retries on the next one, in a fresh session; each account is tried once.
    let mut crossed = false;
    for attempt in 0.. {
        let session = match attempt {
            0 => spec.session.clone(),
            n => format!("{}-{n}", spec.session),
        };
        let account = if accounts.is_empty() {
            None
        } else {
            let cooling = store.cooldowns().unwrap_or_default();
            match crate::pool::pick(&accounts, &cooling, crate::now()) {
                Ok(a) => Some(a),
                Err(until) => {
                    let which = provider.map_or(String::new(), |p| format!("{p} "));
                    let why = format!(
                        "every {which}account is rate-limited until {}",
                        crate::pool::iso(until)
                    );
                    return ("failed", format!("{label} could not start: {why}"), crossed);
                }
            }
        };
        // Only the first attempt reopens the stored session; a failover starts fresh.
        let resume = spec.resume && attempt == 0;
        let (status, text, over, error) =
            attempt_once(cfg, store, spec, &session, account, resume).await;
        crossed |= over;
        // Only the run's own error: the label holds ids ("task #429") and a cut-short report
        // holds the agent's words.
        let limited = error.as_deref().and_then(crate::pool::rate_limit);
        let (Some(a), Some(resets_at)) = (account, limited) else {
            return (status, text, crossed);
        };
        if let Err(e) = crate::pool::cool_down(store, &a.name, &text, resets_at, crate::now()) {
            eprintln!("zephyr: recording a cooldown failed: {e}");
            return (status, text, crossed);
        }
        if attempt + 1 >= accounts.len() {
            return (status, text, crossed);
        }
    }
    unreachable!("the attempt loop always returns")
}

/// One attempt of a run on `account`, in its own fresh child.
async fn attempt_once(
    cfg: &Config,
    store: &mut Store,
    spec: &Spec<'_>,
    session: &str,
    account: Option<&crate::config::Account>,
    resume: bool,
) -> (&'static str, String, bool, Option<String>) {
    let label = &spec.label;
    if let Some(id) = spec.task {
        // Recorded first, so a restart during this attempt resumes this session.
        if let Err(e) = store.set_task_session(id, session) {
            eprintln!("zephyr: recording task #{id}'s session failed: {e}");
        }
    }
    let mut child = match Child::spawn_as(cfg, Some(session), account).await {
        Ok(child) => child,
        Err(e) => {
            return (
                "failed",
                format!("{label} could not start: {e:#}"),
                false,
                None,
            )
        }
    };
    let limit = cfg.job_timeout;
    let max_turns = cfg.budget.as_ref().and_then(|b| b.run_turns);
    let mut tally = crate::rpc::Tally::default();
    let ended = tokio::select! {
        r = tokio::time::timeout(limit, start(cfg, &mut child, spec, resume, &mut tally, max_turns)) => Ok(r),
        why = until_cut(store, spec.task) => Err(why),
    };
    // `cut_short`: stopped, paused, timed out or stalled, so it may have done some of the work.
    let mut error = None;
    let (status, text, cut_short) = match ended {
        Ok(Ok(Ok(Ok(text)))) => ("completed", text, false),
        Ok(Ok(Ok(Err(why)))) => {
            let stalled = why.to_string().starts_with("stalled");
            error = Some(format!("{why:#}"));
            ("failed", format!("{label} failed: {why:#}"), stalled)
        }
        Ok(Ok(Err(why))) => {
            error = Some(format!("{why:#}"));
            ("failed", format!("{label} failed: {why:#}"), false)
        }
        cut => {
            let abort = child.call(json!({"type": "abort"}));
            let _ = tokio::time::timeout(Duration::from_secs(5), abort).await;
            match cut {
                Err("") => ("stopped", format!("{label} was stopped"), true),
                Err(why) => ("failed", format!("{label} was aborted{why}"), true),
                Ok(_) => (
                    "failed",
                    format!("{label} was aborted after {limit:?}"),
                    true,
                ),
            }
        }
    };
    // As Hermes's interrupted children do: the report keeps what the run had got to.
    let out = match cut_short {
        true => match last_words(&mut child).await {
            Some(words) => (
                status,
                format!("{text}\nIts last message before that:\n{words}"),
            ),
            None => (status, text),
        },
        false => (status, text),
    };
    if let Some(alert) = tally.alert().map(|a| crate::facts::scrubbed(cfg, a)) {
        if let Err(e) = store.notify(&spec.to.0, &spec.to.1, &alert) {
            eprintln!("zephyr: outbound notice failed: {e}");
        }
    }
    crate::tools::apply(store, cfg, &tally.requests, Some(spec.to.clone()));
    let crossed = book(store, cfg, account, tally.used);
    child.stop().await;
    (out.0, out.1, crossed, error)
}

/// The most a cut-short run's last message adds to its report, as `tasks::MAX_REPORT` is for a
/// whole report.
const LAST_WORDS: usize = 8 * 1024;

/// The run's last assistant text with something in it, cut to `LAST_WORDS` on a char boundary.
/// `None` when it wrote nothing, or the child cannot say within 5 s (a stalled one may not).
async fn last_words(child: &mut Child) -> Option<String> {
    let snap = child.call(json!({"type": "get_messages"}));
    let snap = tokio::time::timeout(Duration::from_secs(5), snap)
        .await
        .ok()?
        .ok()?;
    let text = snap["messages"]
        .as_array()?
        .iter()
        .rev()
        .filter(|m| m["kind"] == "assistant")
        .map(message_text)
        .find(|t| !t.trim().is_empty())?;
    let end = (0..=LAST_WORDS.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    Some(text[..end].to_string())
}

/// Adds a side run's usage (a job's, a `/btw`'s) to today's spend and `account`'s. Returns true
/// when it crossed a budget limit; the caller sends `BUDGET_NOTICE`.
pub(crate) fn book(
    store: &Store,
    cfg: &Config,
    account: Option<&crate::config::Account>,
    used: (i64, f64),
) -> bool {
    if let Some(a) = account {
        let _ = store.add_account_spend(&a.name, used);
    }
    store.charge(cfg.budget.as_ref(), used).unwrap_or_else(|e| {
        eprintln!("zephyr: recording spend failed: {e}");
        false
    })
}

/// A run's first move: a fresh prompt, or a resume in the session it ran in.
async fn start(
    cfg: &Config,
    child: &mut Child,
    spec: &Spec<'_>,
    resume_it: bool,
    tally: &mut crate::rpc::Tally,
    max_turns: Option<u32>,
) -> anyhow::Result<anyhow::Result<String>> {
    match resume_it {
        true => resume(cfg, child, spec, tally, max_turns).await,
        false => prompt(cfg, child, spec.prompt, spec.refuse, tally, max_turns).await,
    }
}

/// Carries on a run a restart cut (DESIGN §4.12), with rpi's session as the checkpoint, as
/// Absurd's durable-agent pattern does: a pending run resumes where it stopped; one whose last
/// message is the agent's had finished, so that is the reply; anything else is told to go on.
async fn resume(
    cfg: &Config,
    child: &mut Child,
    spec: &Spec<'_>,
    tally: &mut crate::rpc::Tally,
    max_turns: Option<u32>,
) -> anyhow::Result<anyhow::Result<String>> {
    let snap = child.call(json!({"type": "get_messages"})).await?;
    if snap["pendingResume"] == true {
        child.call(json!({"type": "resume_pending"})).await?;
        // Delivered at the resumed run's next step.
        let note = json!({"type": "queue_message", "message": RESUME_NOTE, "deliverAs": "steer"});
        child.call(note).await?;
        return follow(cfg, child, spec.refuse, tally, max_turns).await;
    }
    let messages = snap["messages"].as_array().cloned().unwrap_or_default();
    let Some(last) = messages.last() else {
        // Cut before the prompt was committed: start it now.
        return prompt(cfg, child, spec.prompt, spec.refuse, tally, max_turns).await;
    };
    if let Some(text) = final_reply(last) {
        return Ok(Ok(text));
    }
    prompt(cfg, child, RESUME_NOTE, spec.refuse, tally, max_turns).await
}

/// The text of `last` when it is a finished reply: the agent's, with text, and no tool call
/// still waiting on its result.
fn final_reply(last: &serde_json::Value) -> Option<String> {
    let blocks = last["content"].as_array().into_iter().flatten();
    let calling = blocks.clone().any(|b| b["type"] == "toolCall");
    let text = message_text(last);
    (last["kind"] == "assistant" && !calling && !text.is_empty()).then_some(text)
}

/// The most of a tool call's arguments an audit line keeps.
const AUDIT_ARGS: usize = 500;

/// Records a tool call's start (or the policy's refusal of it) and its failure in the audit
/// (DESIGN §4.4), from rpi's own events, in root's store: the agent can write neither.
/// Arguments are cut to `AUDIT_ARGS` and the vault's values scrubbed out.
pub(crate) fn audit(store: &Store, cfg: &Config, session: &str, ev: &serde_json::Value) {
    let Some(tool) = ev["toolName"].as_str() else {
        return;
    };
    let (args, outcome) = match ev["type"].as_str() {
        Some("tool_execution_start") => {
            let refused = matches!(
                zephyr_policy::policy::check(tool, &ev["args"]),
                zephyr_policy::policy::Decision::Deny(_)
            );
            let outcome = if refused { "refused" } else { "started" };
            // A fact's value is saved only after this runs: redact the `/facts add` itself.
            let mut args = ev["args"].clone();
            if tool == "gateway" {
                if let Some(c) = args["command"]
                    .as_str()
                    .filter(|c| c.starts_with("/facts add"))
                {
                    args["command"] = crate::facts::redact(c).into();
                }
            }
            let text = crate::vault::scrubbed(cfg, args.to_string());
            (crate::facts::scrubbed(cfg, text), outcome)
        }
        Some("tool_execution_end") if ev["isError"] == true => (String::new(), "error"),
        _ => return,
    };
    let end = (0..=AUDIT_ARGS.min(args.len()))
        .rev()
        .find(|&i| args.is_char_boundary(i))
        .unwrap_or(0);
    if let Err(e) = store.audit(session, tool, &args[..end], outcome) {
        eprintln!("zephyr: writing the audit failed: {e}");
    }
}

/// Sends the prompt and waits for the run to settle (`follow`).
pub(crate) async fn prompt(
    cfg: &Config,
    child: &mut Child,
    text: &str,
    refuse: &str,
    tally: &mut crate::rpc::Tally,
    max_turns: Option<u32>,
) -> anyhow::Result<anyhow::Result<String>> {
    child
        .call(json!({"type": "prompt", "message": text}))
        .await?;
    follow(cfg, child, refuse, tally, max_turns).await
}

/// Waits for the run to settle, counting its events into `tally`, and aborts it at `max_turns`
/// or when it stalls: no event for `cfg.stall_limit`, twice that while a tool runs, since rpi
/// reports nothing between a silent command's start and end (DESIGN §4.4). The inner result is
/// the final assistant text, or why the run ended without one.
async fn follow(
    cfg: &Config,
    child: &mut Child,
    refuse: &str,
    tally: &mut crate::rpc::Tally,
    max_turns: Option<u32>,
) -> anyhow::Result<anyhow::Result<String>> {
    let mut cut = None;
    let mut guarded: Option<String> = None;
    let mut in_tool = 0usize;
    // Its own connection: `follow` is not handed the run's store.
    let audited = child.session.clone().zip(Store::open(&cfg.db_path()).ok());
    let settled = loop {
        let limit = cfg.stall_limit * if in_tool > 0 { 2 } else { 1 };
        let Ok(ev) = tokio::time::timeout(limit, child.events.recv()).await else {
            let _ = child.call(json!({"type": "abort"})).await;
            return Ok(Err(anyhow::anyhow!("stalled: no progress for {limit:?}")));
        };
        match ev {
            Some(ev) if ev["type"] == "agent_settled" => break ev,
            Some(ev) => {
                tally.add(&ev);
                if let Some((session, store)) = &audited {
                    audit(store, cfg, session, &ev);
                }
                match ev["type"].as_str() {
                    Some("tool_execution_start") => in_tool += 1,
                    Some("tool_execution_end") => in_tool = in_tool.saturating_sub(1),
                    _ => {}
                }
                // A job or task is not the owner's conversation; answered at once so it does not
                // wait. Off the list, the plugin has refused the call already and nothing waits. A
                // proposal is not stored either: no one is there to approve it.
                if let Some((id, command)) = crate::tools::gateway_call(&ev) {
                    if zephyr_policy::tools::gateway_kind(command).is_some() {
                        crate::tools::answer_gateway(cfg, id, refuse);
                    }
                }
                // Tasks stay one level deep: a task cannot start, list or stop tasks.
                if let Some((id, _)) = crate::tools::task_call(&ev) {
                    crate::tools::answer_gateway(cfg, id, refuse);
                }
                if cut.is_none() && max_turns.is_some_and(|max| tally.turns >= max) {
                    cut = max_turns;
                    child.call(json!({"type": "abort"})).await?;
                }
                // The repeated-failure guard, as for main (DESIGN §4.4).
                match tally.guard() {
                    Some(crate::rpc::Guard::Steer(note)) => {
                        let steer =
                            json!({"type": "queue_message", "message": note, "deliverAs": "steer"});
                        let _ = child.call(steer).await;
                    }
                    Some(crate::rpc::Guard::Stop(why)) if guarded.is_none() => {
                        guarded = Some(why);
                        child.call(json!({"type": "abort"})).await?;
                    }
                    _ => {}
                }
            }
            None => anyhow::bail!("rpi exited mid-run"),
        }
    };
    if let Some(max) = cut {
        return Ok(Err(anyhow::anyhow!(
            "stopped after {max} turns (budget.run_turns)"
        )));
    }
    if let Some(why) = guarded {
        return Ok(Err(anyhow::anyhow!("stopped: {why}")));
    }
    let snap = child.call(json!({"type": "get_messages"})).await?;
    let reply = snap["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .filter(|m| m["kind"] == "assistant")
        .map(message_text)
        .find(|t| !t.is_empty());
    Ok(match (settled["outcome"].as_str(), reply) {
        (Some("completed"), Some(reply)) => Ok(reply),
        (outcome, _) => Err(anyhow::anyhow!(
            "{} without a reply: {}",
            outcome.unwrap_or("ended"),
            settled["error"].as_str().unwrap_or("no error given")
        )),
    })
}

/// An owner's `/job …` command. `None` when the text is not one; otherwise the reply.
pub fn command(store: &mut Store, cfg: &Config, m: &Inbound) -> Option<String> {
    let rest = m.text.trim().strip_prefix("/job")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let rest = rest.trim();
    let (verb, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let arg = arg.trim();
    let id = arg.parse::<i64>();
    let reply = match (verb, id) {
        ("add", _) => match zone(cfg) {
            Ok(tz) => add(store, &tz, m, arg),
            Err(e) => Ok(format!("[zephyr] {e:#}")),
        },
        ("list", _) if arg.is_empty() => list(store),
        ("ok", Ok(id)) => store.enable_job(id).map(|found| match found {
            true => format!("[zephyr] job #{id} is on"),
            false => format!("[zephyr] no job #{id}"),
        }),
        ("rm", Ok(id)) => store.remove_job(id).map(|found| match found {
            true => format!("[zephyr] job #{id} removed"),
            false => format!("[zephyr] no job #{id}"),
        }),
        ("run", Ok(id)) => store.queue_run(id, crate::now()).map(|run| match run {
            Some(run) => format!("[zephyr] job #{id} queued as run {run}"),
            None => format!("[zephyr] no job #{id}"),
        }),
        _ => Ok(USAGE.into()),
    };
    Some(reply.unwrap_or_else(|e| format!("[zephyr] job store error: {e}")))
}

fn add(store: &Store, tz: &TimeZone, m: &Inbound, arg: &str) -> rusqlite::Result<String> {
    let parts: Vec<&str> = arg.splitn(3, '|').map(str::trim).collect();
    let [name, schedule, prompt] = parts[..] else {
        return Ok(USAGE.into());
    };
    if name.is_empty() || prompt.is_empty() {
        return Ok(USAGE.into());
    }
    let next = match check(schedule, tz) {
        Ok(next) => next,
        Err(e) => return Ok(format!("[zephyr] {e}")),
    };
    let id = store.add_job(name, schedule, prompt, &m.channel, &m.sender)?;
    Ok(format!(
        "[zephyr] job #{id} ({name}) added; next run {next}"
    ))
}

/// Checks `schedule` (five-field cron or an RFC 3339 time) and returns its next run, in `tz`.
pub fn check(schedule: &str, tz: &TimeZone) -> Result<String, String> {
    if schedule.parse::<Timestamp>().is_err() {
        if let Err(e) = parse_cron(schedule) {
            return Err(format!("bad schedule {schedule:?}: {e}"));
        }
    }
    let Some(next) = next_after(schedule, crate::now(), tz) else {
        return Err(format!("{schedule} is in the past"));
    };
    let next = Timestamp::from_second(next).map(|t| t.to_zoned(tz.clone()).to_string());
    Ok(next.unwrap_or_default())
}

pub(crate) fn list(store: &Store) -> rusqlite::Result<String> {
    let jobs = store.jobs()?;
    if jobs.is_empty() {
        return Ok("[zephyr] no jobs".into());
    }
    let lines: Vec<String> = jobs
        .iter()
        .map(|j| {
            format!(
                "#{} {} | {} | {} | to {}{}",
                j.id,
                j.name,
                j.schedule,
                j.prompt,
                j.channel,
                if j.enabled {
                    ""
                } else {
                    " | waiting for /job ok"
                }
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(schedule: &str, after: i64) -> Option<i64> {
        next_after(schedule, after, &TimeZone::UTC)
    }

    #[test]
    fn a_reply_still_calling_a_tool_is_not_the_finished_report() {
        let calling = json!({"kind": "assistant", "content": [
            {"type": "text", "text": "Let me build it."},
            {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "make"}}
        ]});
        assert_eq!(final_reply(&calling), None);
        let done = json!({"kind": "assistant", "content": [{"type": "text", "text": "Built."}]});
        assert_eq!(final_reply(&done).as_deref(), Some("Built."));
        let user = json!({"kind": "user", "content": "go"});
        assert_eq!(final_reply(&user), None);
    }

    fn cfg(timezone: Option<&str>) -> Config {
        let mut cfg: Config = toml::from_str("rpi = 'rpi'\nhome = 'home'").unwrap();
        cfg.timezone = timezone.map(Into::into);
        cfg
    }

    fn store(name: &str) -> Store {
        Store::open(&crate::test_dir(name).join("state.db")).unwrap()
    }

    fn say(store: &mut Store, text: &str) -> String {
        let m = Inbound {
            channel: "discord".into(),
            platform_msg_id: "x".into(),
            sender: "owner".into(),
            text: text.into(),
        };
        command(store, &cfg(None), &m).expect("a /job command")
    }

    #[test]
    fn job_commands_parse_and_validate() {
        let mut s = store("job-commands");
        let not_job = Inbound {
            channel: "chat".into(),
            platform_msg_id: "x".into(),
            sender: "o".into(),
            text: "/jobs please".into(),
        };
        assert_eq!(command(&mut s, &cfg(None), &not_job), None);
        assert!(say(&mut s, "/job add a | 61 * * * * | hi").contains("bad schedule"));
        assert!(say(&mut s, "/job add a | 0 * * * * * | hi").contains("five fields"));
        assert!(say(&mut s, "/job add a | not cron | hi").contains("bad schedule"));
        assert!(say(&mut s, "/job add a | 2001-01-01T00:00:00Z | hi").contains("in the past"));
        assert_eq!(say(&mut s, "/job add a | * * * * *"), USAGE);
        assert_eq!(say(&mut s, "/job add  | * * * * * | hi"), USAGE);
        assert_eq!(say(&mut s, "/job frob"), USAGE);
        assert_eq!(say(&mut s, "/job rm x"), USAGE);
        assert!(s.jobs().unwrap().is_empty());

        assert!(say(
            &mut s,
            "/job add digest | 0 8 * * 1-5 | summarise | my inbox"
        )
        .starts_with("[zephyr] job #1 (digest) added; next run "));
        assert!(say(&mut s, "/job add once | 2999-01-01T00:00:00+09:00 | hi").contains("#2"));
        let job = &s.jobs().unwrap()[0];
        assert_eq!(
            (
                job.prompt.as_str(),
                job.channel.as_str(),
                job.recipient.as_str()
            ),
            ("summarise | my inbox", "discord", "owner")
        );
        assert_eq!(
            say(&mut s, "/job list"),
            "#1 digest | 0 8 * * 1-5 | summarise | my inbox | to discord\n\
             #2 once | 2999-01-01T00:00:00+09:00 | hi | to discord"
        );
        assert_eq!(say(&mut s, "/job run 1"), "[zephyr] job #1 queued as run 1");
        assert_eq!(say(&mut s, "/job run 9"), "[zephyr] no job #9");
        assert_eq!(say(&mut s, "/job rm 1"), "[zephyr] job #1 removed");
        assert_eq!(say(&mut s, "/job rm 1"), "[zephyr] no job #1");
        assert_eq!(s.start_next_run(0).unwrap(), None, "rm drops queued runs");
    }

    #[test]
    fn a_claim_is_taken_once_even_by_concurrent_ticks() {
        let dir = crate::test_dir("job-claim");
        let s = Store::open(&dir.join("state.db")).unwrap();
        s.add_job("a", "* * * * *", "p", "chat", "o").unwrap();
        let later = crate::now() + 120;
        let ticks: Vec<_> = (0..4)
            .map(|_| {
                let db = dir.join("state.db");
                std::thread::spawn(move || {
                    Store::open(&db)
                        .unwrap()
                        .claim_due(later, utc)
                        .unwrap()
                        .len()
                })
            })
            .collect();
        let claimed: usize = ticks.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(claimed, 1);
    }

    #[test]
    fn missed_runs_catch_up_once() {
        let mut s = store("job-catch-up");
        s.add_job("a", "*/5 * * * *", "p", "chat", "o").unwrap();
        // Three days of missed runs, on a slot: one second before a slot, the next one is due.
        let back = (crate::now() + 3 * 24 * 3600) / 300 * 300;
        assert_eq!(s.claim_due(back, utc).unwrap().len(), 1);
        assert!(s.claim_due(back + 1, utc).unwrap().is_empty());
        assert_eq!(s.claim_due(back + 301, utc).unwrap().len(), 1);
    }

    #[test]
    fn a_one_shot_fires_once() {
        let mut s = store("job-one-shot");
        let at = Timestamp::from_second(crate::now() + 60).unwrap();
        s.add_job("a", &at.to_string(), "p", "chat", "o").unwrap();
        assert!(s.claim_due(crate::now(), utc).unwrap().is_empty());
        assert_eq!(s.claim_due(crate::now() + 61, utc).unwrap().len(), 1);
        assert!(s.claim_due(crate::now() + 9999, utc).unwrap().is_empty());
    }

    #[test]
    fn cron_follows_the_configured_timezone() {
        let seoul = zone(&cfg(Some("Asia/Seoul"))).unwrap();
        let noon_utc: Timestamp = "2026-10-28T12:00:00Z".parse().unwrap();
        let next = next_after("0 8 * * *", noon_utc.as_second(), &seoul).unwrap();
        // 08:00 in Seoul is 23:00 UTC the day before.
        assert_eq!(
            Timestamp::from_second(next).unwrap().to_string(),
            "2026-10-28T23:00:00Z"
        );
        assert!(zone(&cfg(Some("Mars/Olympus"))).is_err());

        let mut s = store("job-tz");
        let m = Inbound {
            channel: "chat".into(),
            platform_msg_id: "x".into(),
            sender: "o".into(),
            text: "/job add d | 0 8 * * * | hi".into(),
        };
        let reply = command(&mut s, &cfg(Some("Asia/Seoul")), &m).unwrap();
        assert!(reply.contains("T08:00:00+09:00[Asia/Seoul]"), "{reply}");
        let bad = command(&mut s, &cfg(Some("Mars/Olympus")), &m).unwrap();
        assert!(bad.contains("Mars/Olympus"), "{bad}");
    }
}
