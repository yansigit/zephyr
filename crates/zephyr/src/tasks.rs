//! Background tasks (DESIGN §4.12). The main agent starts one with its `task` tool; it runs in a
//! fresh `task-<id>` child beside `main`, at most [`Config::parallel_tasks`] at once, and its
//! report comes back to the main session as an inbox row, so the main agent tells the owner. A task a restart
//! cut resumes in its rpi session, at most `store::MAX_TASK_RESUMES` times; then it is `unknown`.

use crate::config::{Config, BUDGET_NOTICE};
use crate::scheduler::{execute, Spec, Wake};
use crate::store::{Store, Task};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// The most of a report that goes into the main session; the rest is left in a file.
const MAX_REPORT: usize = 8 * 1024;
/// How often queued tasks are looked at without a wake-up, e.g. after a `/resume`.
const TICK: Duration = Duration::from_secs(5);

/// What the child is told first: it works alone and its last message is the report. A
/// subagent's prompt comes next, then `Goal: ` and the goal.
pub const BRIEF: &str = "You are a background task the main agent started. Nobody reads your \
messages while you work, and nobody can answer a question, so do the work on your own. When \
you finish, your last message is handed to the main agent as your report: say what you did, \
what you found, and anything left undone.\n\n";

/// Runs until an error opening the store; per-task errors are logged and the loop goes on.
pub async fn run(mut cfgs: watch::Receiver<Arc<Config>>, wake: &Wake) -> anyhow::Result<()> {
    let mut cfg = cfgs.borrow_and_update().clone();
    let mut store = Store::open(&cfg.db_path())?;
    // Tasks a restart cut go back to the queue and resume below (DESIGN §4.12).
    if store.requeue_interrupted_tasks(crate::now())? > 0 {
        wake.inbox.notify_one();
    }
    let mut running = FuturesUnordered::new();
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = wake.tasks.notified() => {}
            _ = tick.tick() => {}
            Some(()) = running.next(), if !running.is_empty() => {}
        }
        if cfgs.has_changed().unwrap_or(false) {
            cfg = cfgs.borrow_and_update().clone();
        }
        // While paused nothing starts; the runner aborts what was running (scheduler::until_cut).
        while running.len() < cfg.parallel_tasks() && store.pause_reason().ok() == Some(None) {
            match store.start_next_task(crate::now()) {
                // Its own connection: a task awaits for minutes while others write. One that
                // cannot open ends failed here, never left `running` with nothing running it.
                Ok(Some(task)) => match Store::open(&cfg.db_path()) {
                    Ok(own) => running.push(one(cfg.clone(), own, task, wake)),
                    Err(e) => {
                        let why =
                            format!("[zephyr] task #{} could not open the store: {e}", task.id);
                        eprintln!("zephyr: {why}");
                        log(store.finish_task(&task, "failed", &why, crate::now()));
                        wake.inbox.notify_one();
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    eprintln!("zephyr: starting a task failed: {e}");
                    break;
                }
            }
        }
    }
}

/// Runs one task to its end and hands the report to the main session.
async fn one(cfg: Arc<Config>, mut store: Store, task: Task, wake: &Wake) {
    let role = (task.preamble.as_ref()).map_or(String::new(), |p| format!("{p}\n\n"));
    let prompt = format!("{BRIEF}{role}Goal: {}", task.goal);
    // A task a restart cut reopens the session it ran in.
    let resume = task.resumes > 0 && task.session.is_some();
    let spec = Spec {
        prompt: &prompt,
        goal: &task.goal,
        label: format!("[zephyr] task #{}", task.id),
        session: match (&task.session, resume) {
            (Some(s), true) => s.clone(),
            _ => format!("task-{}", task.id),
        },
        to: (task.channel.clone(), task.recipient.clone()),
        refuse: "not available in background tasks",
        task: Some(task.id),
        resume,
        model: task.model.as_deref(),
        steers: &wake.steers,
    };
    let (status, output, crossed) = execute(&cfg, &mut store, &spec).await;
    let report = cut(&cfg, task.id, &output);
    let (channel, recipient) = (&task.channel, &task.recipient);
    // The task's memory edits are committed as main's settle would commit them.
    let origin = crate::memory::Origin::Task(task.id);
    let done =
        crate::memory::commit(&cfg.home, cfg.state_dir(), origin, cfg.digest(), || false).await;
    if let Some(notice) = crate::memory::recorded(&store, done) {
        log(store.notify(channel, recipient, &notice));
    }
    for notice in crate::drafts::scan(&cfg) {
        log(store.notify(channel, recipient, &notice));
    }
    log(store.finish_task(&task, status, &report, crate::now()));
    if crossed {
        log(store.notify(channel, recipient, BUDGET_NOTICE));
    }
    wake.inbox.notify_one();
    wake.outbox.notify_one();
}

/// A report short enough for the main session: past [`MAX_REPORT`], the whole of it goes to
/// `<home>/tasks/<id>.md`, written as the agent so it can `read` the rest.
fn cut(cfg: &Config, id: i64, output: &str) -> String {
    if output.len() <= MAX_REPORT {
        return output.to_string();
    }
    let end = (0..=MAX_REPORT)
        .rev()
        .find(|&i| output.is_char_boundary(i))
        .unwrap_or(0);
    let name = format!("tasks/{id}.md");
    let saved = {
        let _fs = crate::agent::fs();
        let dir = cfg.home.join("tasks");
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join(format!("{id}.md")), output))
    };
    let rest = match saved {
        Ok(()) => format!("the whole report is in {name}"),
        Err(e) => format!("saving the rest to {name} failed: {e}"),
    };
    format!("{}\n[cut at {MAX_REPORT} bytes; {rest}]", &output[..end])
}

/// `/task list` and the agent's `task list`: open tasks, then the last few that ended.
pub fn list(store: &Store) -> String {
    let tasks = match store.tasks(5) {
        Ok(t) => t,
        Err(e) => return format!("[zephyr] listing tasks failed: {e}"),
    };
    if tasks.is_empty() {
        return "[zephyr] no tasks".into();
    }
    let now = crate::now();
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let goal: String = t.goal.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = goal.chars().take(60).collect();
            let cut = if short.len() < goal.len() { "…" } else { "" };
            let age = (now - t.created_at).max(0) / 60;
            let on = t
                .model
                .as_ref()
                .map_or(String::new(), |m| format!(" on {m}"));
            let as_ = (t.subagent.as_ref()).map_or(String::new(), |s| format!(" as {s}"));
            format!(
                "#{} {} ({age} min ago){as_}{on}: {short}{cut}",
                t.id, t.status
            )
        })
        .collect();
    format!("[zephyr] tasks:\n{}", lines.join("\n"))
}

/// `/subagent` and `zephyr subagent list`: the owner's subagents, then the task settings.
pub fn settings(cfg: &Config) -> String {
    let mut lines: Vec<String> = cfg
        .subagents
        .iter()
        .map(|s| {
            let model = s.model.as_deref().unwrap_or("default");
            let first = s.prompt.trim().lines().next().unwrap_or_default();
            format!("{} ({model}): {first}", s.name)
        })
        .collect();
    if lines.is_empty() {
        lines.push("no subagents".into());
    }
    lines.push(format!(
        "tasks: {} at once, on {}",
        cfg.parallel_tasks(),
        cfg.task_model().unwrap_or("the account's own model")
    ));
    lines.join("\n")
}

/// `/task stop <id>` and the agent's `task stop`; `None` stops every open task (`/stop`).
pub fn stop(store: &mut Store, wake: &Wake, id: Option<i64>) -> String {
    match (store.request_stop(id, crate::now()), id) {
        (Err(e), _) => format!("[zephyr] stopping tasks failed: {e}"),
        (Ok(0), Some(id)) => format!("[zephyr] task #{id} is not queued or running"),
        (Ok(n), id) => {
            wake.tasks.notify_one();
            match id {
                Some(id) => format!("[zephyr] stopping task #{id}"),
                None => format!("[zephyr] stopping {n} task(s)"),
            }
        }
    }
}

fn log(r: rusqlite::Result<()>) {
    if let Err(e) = r {
        eprintln!("zephyr: recording a task failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_report_is_cut_on_a_char_boundary_and_kept_whole_in_a_file() {
        let home = crate::test_dir("task-cut");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        assert_eq!(cut(&cfg, 1, "short"), "short");
        // A 3-byte char straddles the limit: the cut falls before it.
        let long = format!("{}€{}", "a".repeat(MAX_REPORT - 1), "b".repeat(100));
        let report = cut(&cfg, 7, &long);
        let (head, tail) = report.split_once('\n').unwrap();
        assert_eq!(head, "a".repeat(MAX_REPORT - 1));
        assert_eq!(
            tail,
            "[cut at 8192 bytes; the whole report is in tasks/7.md]"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("tasks/7.md")).unwrap(),
            long
        );
    }
}
