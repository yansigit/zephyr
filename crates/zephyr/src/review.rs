//! The built-in daily review (DESIGN §4.5): one job row with `builtin = 'review'`, kept in step
//! with `review.at` by `sync`, whose run sends a block the daemon composes (the goals, the
//! activity since the last review, the memory digest) before an optional check-in by the model.

use crate::config::Config;
use crate::store::{JobRun, Store};

/// The model's part of a review run, after the daemon's block.
pub const PROMPT: &str = "This is the owner's daily review; the record above is zephyr's. In at most 10 lines: for each goal, what moved today and one next step you could take. Do not start work and do not change memory. If there is nothing useful to say, reply exactly [SILENT].";

const BUILTIN: &str = "review";
/// The activity part's longest text.
const ACTIVITY_CHARS: usize = 3000;

/// Brings the built-in job in step with `review.at`: made (counted from now), moved or removed.
/// Run at the scheduler's start and at each config change.
pub fn sync(store: &mut Store, cfg: &Config) -> anyhow::Result<()> {
    let job = store.builtin_job(BUILTIN)?;
    let at = cfg.review_at().and_then(crate::config::hh_mm);
    match (job, at) {
        (Some(job), None) => drop(store.remove_job(job.id)?),
        (job, Some((h, m))) => {
            let schedule = format!("{m} {h} * * *");
            match job {
                Some(job) if job.schedule == schedule => {}
                Some(job) => store.set_job_schedule(job.id, &schedule)?,
                None => {
                    let (channel, to) = store.last_owner()?.unwrap_or_default();
                    store.add_builtin_job(BUILTIN, BUILTIN, &schedule, PROMPT, &channel, &to)?;
                }
            }
        }
        (None, None) => {}
    }
    Ok(())
}

/// Where this review counts from: the last completed review's due time, else a day ago.
pub fn since(store: &Store, now: i64) -> i64 {
    match store.last_builtin_run(BUILTIN) {
        Ok(Some(due)) => due,
        _ => now - 86400,
    }
}

/// `text` cut at `ACTIVITY_CHARS`, saying so.
fn cut(text: &str) -> String {
    match text.chars().count() > ACTIVITY_CHARS {
        true => {
            let kept: String = text.chars().take(ACTIVITY_CHARS).collect();
            format!("{kept}\n(cut; /activity shows it all)")
        }
        false => text.to_string(),
    }
}

/// The activity since `since`, one line a record, newest first.
// shortcut: A4's Store::activity is not merged yet, the rebase onto claude/activity fills this in
// with `store.activity(since, &[], None, 50)` rendered by its line renderer.
fn activity_block(_store: &Store, _since: i64) -> Result<String, String> {
    Ok(String::new())
}

/// The daemon's part of the review, composed with no model. `None` when there is nothing to
/// say: no goals, no activity and no memory change.
pub fn block(cfg: &Config, store: &Store, since: i64, now: i64) -> Option<String> {
    let tz = crate::scheduler::zone(cfg).unwrap_or(jiff::tz::TimeZone::UTC);
    let clock = |at| crate::settings::clock(at, &tz);
    let goals = crate::memory::read_goals(cfg.state_dir()).unwrap_or_default();
    let activity = activity_block(store, since);
    let digest = crate::memory::digest(&cfg.home, cfg.state_dir(), store, since, &tz);
    let quiet = matches!(&activity, Ok(a) if a.is_empty()) && matches!(digest, Ok(None));
    if goals.is_empty() && quiet {
        return None;
    }
    let date = jiff::Timestamp::from_second(now).map(|t| t.to_zoned(tz.clone()));
    let date = date.map_or(String::new(), |d| d.strftime("%a %-d %b").to_string());
    let mut out = vec![format!("[zephyr] daily review, {date}"), "Goals".into()];
    match goals.is_empty() {
        true => out.push("no goals; add one with /goal add <text>".into()),
        false => out.extend(
            goals
                .iter()
                .enumerate()
                .map(|(i, g)| format!("{}. {g}", i + 1)),
        ),
    }
    let done = store.goals_done_since(since).unwrap_or_default();
    if !done.is_empty() {
        let done: Vec<String> = (done.iter())
            .map(|d| {
                let who = match (d.actor.as_str(), d.proposal) {
                    ("owner", _) => "you".to_string(),
                    (_, Some(n)) => format!("proposed as #{n}"),
                    (actor, None) => actor.to_string(),
                };
                format!("{} ({who}, {})", d.text, clock(d.at))
            })
            .collect();
        out.push(format!("Done since the last review: {}", done.join(", ")));
    }
    match activity {
        Ok(a) => {
            out.push(format!("Activity since {}", clock(since)));
            out.push(match a.is_empty() {
                true => "nothing recorded".into(),
                false => cut(&a),
            });
        }
        Err(e) => out.push(format!("Activity: unavailable ({e})")),
    }
    out.push("Memory".into());
    out.push(match digest {
        Ok(Some(d)) => d,
        Ok(None) => "no changes".into(),
        Err(e) => format!("unavailable ({e:#})"),
    });
    Some(out.join("\n"))
}

/// What a review run does once its block is out.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// The model's check-in, with this prompt.
    Run(String),
    /// Nothing for the model: recorded `completed`, `[SILENT]`.
    Done,
    /// Recorded `failed` with this output; nothing was sent.
    Failed(String),
}

/// Before a review run: the job goes where the owner last wrote (the row too, so its output and
/// an interrupted run's report follow), the block is sent there, and the model is asked only
/// when there are goals.
pub fn prepare(cfg: &Config, store: &mut Store, run: &mut JobRun, now: i64) -> Step {
    let Ok(Some((channel, to))) = store.last_owner() else {
        return Step::Failed("no owner to send the review to".into());
    };
    if let Err(e) = store.retarget_job(run.job.id, &channel, &to) {
        eprintln!("zephyr: pointing the review at {channel} failed: {e}");
    }
    (run.job.channel, run.job.recipient) = (channel, to);
    let block = block(cfg, store, since(store, now), now);
    if let Some(text) = &block {
        if let Err(e) = store.notify(&run.job.channel, &run.job.recipient, text) {
            eprintln!("zephyr: sending the daily review failed: {e}");
        }
    }
    let goals = crate::memory::read_goals(cfg.state_dir()).unwrap_or_default();
    match (goals.is_empty(), block) {
        (false, Some(block)) => Step::Run(format!("{block}\n\n{PROMPT}")),
        _ => Step::Done,
    }
}

/// `doctor`'s: the review is on but its job is not there.
pub fn warning(cfg: &Config, store: &Store) -> Option<String> {
    cfg.review_at()?;
    matches!(store.builtin_job(BUILTIN), Ok(None)).then(|| {
        "review.at is set but the daily review job is missing; it is made again at the next \
         start or config change"
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn cfg(review: Option<&str>) -> Config {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let home = crate::test_dir(&format!("review-{n}"));
        let review = review.map_or(String::new(), |at| format!("[review]\nat = \"{at}\"\n"));
        let text = format!(
            "rpi = '/r'\nhome = {:?}\nstate = {:?}\ntimezone = 'UTC'\n{review}",
            home,
            home.join("state")
        );
        std::fs::create_dir_all(home.join("state")).unwrap();
        toml::from_str(&text).unwrap()
    }

    fn ready(cfg: &Config) -> Store {
        crate::memory::prepare(&cfg.home, cfg.state_dir(), &cfg.home.join("agent")).unwrap();
        Store::open(&cfg.db_path()).unwrap()
    }

    #[test]
    fn the_block_lists_goals_done_activity_and_memory() {
        let cfg = cfg(Some("21:30"));
        let store = ready(&cfg);
        crate::memory::goal(cfg.state_dir(), "add Ship zephyr 0.8").unwrap();
        store
            .log_setting(
                "goal",
                Some("Renew passport"),
                Some("done"),
                "owner",
                Some("chat"),
                None,
            )
            .unwrap();
        store
            .log_setting(
                "goal",
                Some("Run"),
                Some("done"),
                "agent",
                Some("chat"),
                Some(4),
            )
            .unwrap();
        std::fs::write(
            cfg.home.join("memory/MEMORY.md"),
            "# MEMORY.md\nlikes tea\n",
        )
        .unwrap();
        let origin = crate::memory::Origin::Task(1);
        let done = crate::memory::commit_if_changed(&cfg.home, cfg.state_dir(), &origin);
        crate::memory::recorded(&store, done);
        let now = crate::now();
        let since = now - 86400;
        let utc = jiff::tz::TimeZone::UTC;
        let clock = |at| crate::settings::clock(at, &utc);
        let date = jiff::Timestamp::from_second(now)
            .unwrap()
            .to_zoned(utc.clone());
        let at = store.goals_done_since(0).unwrap()[0].at;
        let sha = store.memory_commits_since(0).unwrap()[0].sha.clone();
        let commit_at = store.memory_commits_since(0).unwrap()[0].at;
        assert_eq!(
            block(&cfg, &store, since, now).unwrap(),
            format!(
                "[zephyr] daily review, {}\nGoals\n1. Ship zephyr 0.8\nDone since the last review: \
                 Renew passport (you, {}), Run (proposed as #4, {})\nActivity since {}\nnothing \
                 recorded\nMemory\n- {} {} from task #1\nWaiting (/memory ok keeps it, /memory undo \
                 drops it):\nMEMORY.md\n+ likes tea",
                date.strftime("%a %-d %b"),
                clock(at),
                clock(at),
                clock(since),
                clock(commit_at),
                &sha[..7],
            )
        );
    }

    #[test]
    fn nothing_to_say_is_none() {
        let cfg = cfg(Some("21:30"));
        let store = ready(&cfg);
        assert_eq!(
            block(&cfg, &store, crate::now() - 86400, crate::now()),
            None
        );
        crate::memory::goal(cfg.state_dir(), "add Rest").unwrap();
        let text = block(&cfg, &store, crate::now() - 86400, crate::now()).unwrap();
        assert!(text.contains("\nGoals\n1. Rest\nActivity since "), "{text}");
        assert!(text.ends_with("\nMemory\nno changes"), "{text}");
    }

    #[test]
    fn activity_is_cut_at_3000_characters() {
        assert_eq!(cut("a\nb"), "a\nb");
        let long = "x".repeat(3500);
        assert_eq!(
            cut(&long),
            format!("{}\n(cut; /activity shows it all)", "x".repeat(3000))
        );
    }

    #[test]
    fn warning_names_a_missing_review_job() {
        let off = cfg(None);
        let mut store = ready(&off);
        assert_eq!(warning(&off, &store), None);
        let on = Config {
            review: Some(crate::config::Review {
                at: Some("08:05".into()),
            }),
            ..off.clone()
        };
        assert_eq!(
            warning(&on, &store).as_deref(),
            Some("review.at is set but the daily review job is missing; it is made again at the next start or config change")
        );
        sync(&mut store, &on).unwrap();
        assert_eq!(warning(&on, &store), None);
        let job = store.builtin_job("review").unwrap().unwrap();
        assert_eq!(
            (job.name.as_str(), job.schedule.as_str()),
            ("review", "5 8 * * *")
        );
        assert_eq!(job.prompt, PROMPT);
        let later = Config {
            review: Some(crate::config::Review {
                at: Some("21:30".into()),
            }),
            ..off.clone()
        };
        sync(&mut store, &later).unwrap();
        let moved = store.builtin_job("review").unwrap().unwrap();
        assert_eq!((moved.id, moved.schedule.as_str()), (job.id, "30 21 * * *"));
        sync(&mut store, &off).unwrap();
        assert_eq!(store.builtin_job("review").unwrap(), None);
    }
}
