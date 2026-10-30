//! Scheduled jobs (DESIGN §4.5). A tick claims due runs; runs go one at a time, each in a
//! fresh `job-<job id>-<run id>` child, so `main` never waits on a job. While the gateway is
//! paused nothing is claimed or started, and a running job is aborted.

use crate::channel::Inbound;
use crate::config::{Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child};
use crate::store::{JobRun, Store};
use croner::parser::{CronParser, Seconds, Year};
use jiff::{tz::TimeZone, Timestamp};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};

const TICK: Duration = Duration::from_secs(30);
/// How often the pause flag (set by `/pause` or a budget trip, in the store) is read.
const PAUSE_POLL: Duration = Duration::from_secs(1);
/// The DESIGN §4.4 per-run wall clock cap; `Config::job_timeout` defaults to it.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A run whose whole reply is this is not delivered.
const SILENT: &str = "[SILENT]";
const USAGE: &str = "[zephyr] usage: /job add <name> | <cron or RFC3339 time> | <prompt>, /job list, /job rm <id>, /job run <id>";

/// Wakeups between the router and the scheduler.
#[derive(Default)]
pub struct Wake {
    /// A run was queued by `/job run`.
    pub runs: Notify,
    /// The scheduler wrote to the outbox.
    pub outbox: Notify,
    /// A job run is in progress, so the daemon does not re-exec under it.
    pub running: AtomicBool,
}

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

async fn until_paused(store: &mut Store) {
    loop {
        tokio::time::sleep(PAUSE_POLL).await;
        if paused(store) {
            return;
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
            let (status, output, crossed) = execute(&cfg, &mut store, &run).await;
            wake.running.store(false, Ordering::SeqCst);
            // The run's memory edits are committed as main's settle would commit them.
            log_err(crate::memory::commit_if_changed(&cfg.home));
            let delivery = (output.trim() != SILENT).then_some(output.as_str());
            if let Err(e) = store.finish_run(&run, status, &output, delivery, crate::now()) {
                eprintln!("zephyr: recording job run {} failed: {e}", run.id);
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

fn log_err(r: anyhow::Result<bool>) {
    if let Err(e) = r {
        eprintln!("zephyr: memory commit failed: {e:#}");
    }
}

/// One prompt in a fresh child, bounded by `cfg.job_timeout` and aborted by a pause. The
/// run's usage is charged to today's budget. Returns `(status, output, crossed the budget)`.
async fn execute(cfg: &Config, store: &mut Store, run: &JobRun) -> (&'static str, String, bool) {
    let label = format!(
        "[zephyr] job #{} ({}) run {}",
        run.job.id, run.job.name, run.id
    );
    // A job runs on the first account not cooling down. A rate-limited attempt cools that
    // account and retries on the next one, in a fresh session; each account is tried once.
    let mut crossed = false;
    for attempt in 0.. {
        let session = match attempt {
            0 => format!("job-{}-{}", run.job.id, run.id),
            n => format!("job-{}-{}-{n}", run.job.id, run.id),
        };
        let account = if cfg.accounts.is_empty() {
            None
        } else {
            let cooling = store.cooldowns().unwrap_or_default();
            match crate::pool::pick(&cfg.accounts, &cooling, crate::now()) {
                Ok(a) => Some(a),
                Err(until) => {
                    let why = format!(
                        "every account is rate-limited until {}",
                        crate::pool::iso(until)
                    );
                    return ("failed", format!("{label} could not start: {why}"), crossed);
                }
            }
        };
        let (status, text, over) = attempt_once(cfg, store, run, &label, &session, account).await;
        crossed |= over;
        let limited = (status == "failed")
            .then(|| crate::pool::rate_limit(&text))
            .flatten();
        let (Some(a), Some(resets_at)) = (account, limited) else {
            return (status, text, crossed);
        };
        if let Err(e) = crate::pool::cool_down(store, &a.name, &text, resets_at, crate::now()) {
            eprintln!("zephyr: recording a cooldown failed: {e}");
            return (status, text, crossed);
        }
        if attempt + 1 >= cfg.accounts.len() {
            return (status, text, crossed);
        }
    }
    unreachable!("the attempt loop always returns")
}

/// One attempt of a job run on `account`, in its own fresh child.
async fn attempt_once(
    cfg: &Config,
    store: &mut Store,
    run: &JobRun,
    label: &str,
    session: &str,
    account: Option<&crate::config::Account>,
) -> (&'static str, String, bool) {
    let mut child = match Child::spawn_as(cfg, session, account).await {
        Ok(child) => child,
        Err(e) => return ("failed", format!("{label} could not start: {e:#}"), false),
    };
    let limit = cfg.job_timeout;
    let ended = tokio::select! {
        r = tokio::time::timeout(limit, prompt(&mut child, &run.job.prompt)) => Some(r),
        _ = until_paused(store) => None,
    };
    let out = match ended {
        Some(Ok(Ok(Ok(text)))) => ("completed", text),
        Some(Ok(Ok(Err(why)) | Err(why))) => ("failed", format!("{label} failed: {why:#}")),
        cut => {
            let abort = child.call(json!({"type": "abort"}));
            let _ = tokio::time::timeout(Duration::from_secs(5), abort).await;
            let why = match cut {
                Some(_) => format!(" after {limit:?}"),
                None => ": paused".into(),
            };
            ("failed", format!("{label} was aborted{why}"))
        }
    };
    // A fresh session, so its totals are this run's usage.
    let stats = child.call(json!({"type": "get_stats"}));
    let crossed = match tokio::time::timeout(Duration::from_secs(5), stats).await {
        Ok(Ok(stats)) => {
            let used = (
                stats["totalTokens"].as_i64().unwrap_or(0),
                stats["costTotal"].as_f64().unwrap_or(0.0),
            );
            if let Some(a) = account {
                let _ = store.add_account_spend(&a.name, used);
            }
            store.charge(cfg.budget.as_ref(), used).unwrap_or_else(|e| {
                eprintln!("zephyr: recording job spend failed: {e}");
                false
            })
        }
        _ => {
            eprintln!("zephyr: no get_stats from {label}; its spend is not counted");
            false
        }
    };
    child.stop().await;
    (out.0, out.1, crossed)
}

/// Sends the prompt and waits for the run to settle. The inner result is the final
/// assistant text, or why the run ended without one.
async fn prompt(child: &mut Child, text: &str) -> anyhow::Result<anyhow::Result<String>> {
    child
        .call(json!({"type": "prompt", "message": text}))
        .await?;
    let settled = loop {
        match child.events.recv().await {
            Some(ev) if ev["type"] == "agent_settled" => break ev,
            Some(_) => {}
            None => anyhow::bail!("rpi exited mid-run"),
        }
    };
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
    if schedule.parse::<Timestamp>().is_err() {
        if let Err(e) = parse_cron(schedule) {
            return Ok(format!("[zephyr] bad schedule {schedule:?}: {e}"));
        }
    }
    let Some(next) = next_after(schedule, crate::now(), tz) else {
        return Ok(format!("[zephyr] {schedule} is in the past"));
    };
    let id = store.add_job(name, schedule, prompt, &m.channel, &m.sender)?;
    let next = Timestamp::from_second(next).map(|t| t.to_zoned(tz.clone()).to_string());
    Ok(format!(
        "[zephyr] job #{id} ({name}) added; next run {}",
        next.unwrap_or_default()
    ))
}

fn list(store: &Store) -> rusqlite::Result<String> {
    let jobs = store.jobs()?;
    if jobs.is_empty() {
        return Ok("[zephyr] no jobs".into());
    }
    let lines: Vec<String> = jobs
        .iter()
        .map(|j| {
            format!(
                "#{} {} | {} | {} | to {}",
                j.id, j.name, j.schedule, j.prompt, j.channel
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
        let back = crate::now() + 3 * 24 * 3600; // three days of missed runs
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
