//! The daemon's half of the agent's `send_file`, `schedule`, `gateway` and `task` tools (DESIGN §4.11, §4.12).
//! The policy plugin registers them and checks their arguments in the child. `send_file` and
//! `schedule` calls reach the daemon in the run's tally, read from rpi's event stream, and are
//! carried out here when it settles; a `gateway` or `task` call is answered as soon as it starts.

use crate::config::Config;
use crate::store::Store;
use anyhow::{bail, Context};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zephyr_policy::tools::MAX_FILE;

/// Where the child's `gateway` tool waits for its answers (`ZEPHYR_GATEWAY_DIR`).
pub fn gateway_dir(cfg: &Config) -> PathBuf {
    cfg.home.join(".gateway")
}

/// `(tool call id, command)` when `ev` starts a `gateway` call. From rpi's event stream, so
/// the agent cannot fake one; the command is still checked by whoever answers.
pub fn gateway_call(ev: &Value) -> Option<(&str, &str)> {
    if ev["type"] != "tool_execution_start" || ev["toolName"] != "gateway" {
        return None;
    }
    let id = ev["toolCallId"].as_str()?;
    Some((id, ev["args"]["command"].as_str().unwrap_or("").trim()))
}

/// `(tool call id, args)` when `ev` starts a `task` call (DESIGN §4.12). It waits for its
/// answer the way a `gateway` call does; the args are checked again by whoever answers.
pub fn task_call(ev: &Value) -> Option<(&str, &Value)> {
    if ev["type"] != "tool_execution_start" || ev["toolName"] != "task" {
        return None;
    }
    Some((ev["toolCallId"].as_str()?, &ev["args"]))
}

/// Hands `text` to the waiting `gateway` or `task` call `call_id`: written as the agent, then renamed into
/// place so the plugin never reads half of it.
pub fn answer_gateway(cfg: &Config, call_id: &str, text: &str) {
    let Some(path) = zephyr_policy::tools::gateway_file(&gateway_dir(cfg), call_id) else {
        return eprintln!("zephyr: gateway call id {call_id:?} is not a plain name; not answered");
    };
    // The id has no '.', so this never collides with another call's answer.
    let tmp = path.with_extension("tmp");
    let _fs = crate::agent::fs();
    if let Err(e) = crate::host::conf::replace_durably(&path, &tmp, text.as_bytes()) {
        eprintln!("zephyr: answering gateway call {call_id}: {e}");
    }
    sweep(&gateway_dir(cfg), GATEWAY_STALE);
}

/// Far past the plugin's 10 s wait: an answer this old was written after its call gave up.
const GATEWAY_STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// Removes the files in `dir` older than `age`, so late answers do not pile up.
fn sweep(dir: &Path, age: std::time::Duration) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > age));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Carries out a run's requests for `to`, the `(channel, recipient)` it answers: a file goes to
/// the outbox, a proposed job waits for `/job ok`. A failure is told to `to`.
pub fn apply(
    store: &Store,
    cfg: &Config,
    requests: &[(String, Value)],
    to: Option<(String, String)>,
) {
    let Some((channel, recipient)) = to else {
        return;
    };
    for (tool, args) in requests {
        let done = match tool.as_str() {
            "send_file" => send_file(store, cfg, args, &channel, &recipient),
            "schedule" => propose(store, cfg, args, &channel, &recipient),
            _ => continue,
        };
        if let Err(e) = done {
            let text = format!("[zephyr] the agent's {tool} failed: {e:#}");
            if let Err(e) = store.notify(&channel, &recipient, &text) {
                eprintln!("zephyr: {text} (and telling the owner failed: {e})");
            }
        }
    }
}

/// Copies the file into the daemon's state, so the outbox can retry it whatever the agent does
/// to the original, and queues it.
fn send_file(
    store: &Store,
    cfg: &Config,
    args: &Value,
    channel: &str,
    recipient: &str,
) -> anyhow::Result<()> {
    let given = args["path"].as_str().context("no path")?;
    if let zephyr_policy::policy::Decision::Deny(why) =
        zephyr_policy::policy::check("send_file", args)
    {
        bail!("{given} is {why}");
    }
    // The child's cwd is the home, so a relative path is the agent's.
    let path = cfg.home.join(given);
    let name = path.file_name().context("no file name")?.to_owned();
    let bytes = {
        // As the agent: it can send only what it can read.
        let _fs = crate::agent::fs();
        let file = std::fs::File::open(&path).with_context(|| format!("opening {given}"))?;
        if !file.metadata()?.is_file() {
            bail!("{given} is not a file");
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        bytes
    };
    if bytes.len() as u64 > MAX_FILE {
        bail!("{given} is larger than {MAX_FILE} bytes");
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = cfg.state_dir().join("files").join(format!(
        "{}-{}",
        crate::unix_millis(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(name);
    std::fs::write(&copy, bytes)?;
    let caption = args["caption"].as_str().unwrap_or("").trim();
    store.notify_file(channel, recipient, caption, copy.to_str())?;
    Ok(())
}

/// Adds the job switched off and asks the owner.
fn propose(
    store: &Store,
    cfg: &Config,
    args: &Value,
    channel: &str,
    recipient: &str,
) -> anyhow::Result<()> {
    let [name, schedule, prompt] =
        ["name", "schedule", "prompt"].map(|k| args[k].as_str().unwrap_or("").trim());
    let next = crate::scheduler::check(schedule, &crate::scheduler::zone(cfg)?)
        .map_err(anyhow::Error::msg)?;
    let id = store.propose_job(name, schedule, prompt, channel, recipient)?;
    let text = format!(
        "[zephyr] the agent proposed job #{id} ({name}, {schedule}, first run {next}): {prompt}\n\
         /job ok {id} turns it on; /job rm {id} drops it"
    );
    store.notify(channel, recipient, &text)?;
    Ok(())
}

/// Stores an action the agent proposed through `gateway` and asks the owner, on the channel the
/// run answers. Returns the tool's answer. Nothing runs until `/ok`.
pub fn propose_command(store: &Store, command: &str) -> String {
    match ask_owner(store, command, &format!("to run {command}"), "runs it") {
        Ok(id) => format!("proposed as #{id}; it runs only if the owner approves with /ok {id}"),
        Err(e) => format!("[zephyr] proposing {command} failed: {e}"),
    }
}

/// Stores `command` for the owner's `/ok`, telling them what it is in plain words (`ask`, e.g.
/// "to use the token GH_TOKEN with gh") and what `/ok` does (`ok`). Returns the proposal's number.
pub fn ask_owner(store: &Store, command: &str, ask: &str, ok: &str) -> rusqlite::Result<i64> {
    let id = store.propose_command(command)?;
    let text = format!("[zephyr] the agent asks {ask} (#{id}); /ok {id} {ok}, /no {id} drops it");
    store.notify_last_owner(&text)?;
    Ok(id)
}

/// Why `/ok N` does not run a proposal now (`decide`'s `check`).
pub enum NotNow {
    /// Its setting changed since the owner was shown it: it is dropped, with this answer.
    Stale(String),
    /// Approving it needs a strong channel: it stays waiting, with this answer.
    Weak(String),
}

/// What the owner's `/ok N` approved: a proposed slash command, for the router to run as the
/// owner's own, or a held action (approvals spec §4.6), for `actions::approve` to carry out.
/// `once` (`/ok N once`) approves an action without remembering its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    Command(String),
    Action { id: i64, once: bool },
}

/// The owner's `/ok N [once]` or `/no N`. `Some(Ok(_))` is what was approved; `Some(Err(reply))`
/// is the whole answer (a `/no`, or why nothing was approved); `None` for any other text. Before
/// an `/ok` settles a proposed command, `check` sees it and may hold it back. A held action is
/// only looked up here: `actions::approve` takes it, once, and `/no` drops it through
/// `actions::decline`. `once` on a proposed command is a plain `/ok`.
pub fn decide(
    store: &Store,
    text: &str,
    check: impl FnOnce(&crate::store::PendingProposal) -> Option<NotNow>,
) -> Option<Result<Decided, String>> {
    let (verb, rest) = text.split_once(' ').unwrap_or((text, ""));
    let status = match verb {
        "/ok" => "done",
        "/no" => "dropped",
        _ => return None,
    };
    let mut words = rest.split_whitespace();
    let (n, extra) = (words.next().unwrap_or(""), words.collect::<Vec<_>>());
    let once = match (status, extra.as_slice()) {
        (_, []) => false,
        ("done", ["once"]) => true,
        _ => return Some(Err("[zephyr] usage: /ok N, /ok N once, /no N".into())),
    };
    let Ok(id) = n.trim_start_matches('#').parse::<i64>() else {
        return Some(Err(format!(
            "[zephyr] {verb} takes a proposal number, e.g. {verb} 3"
        )));
    };
    let failed =
        |e: rusqlite::Error| Some(Err(format!("[zephyr] reading proposal #{id} failed: {e}")));
    let waiting = match store.pending_proposals() {
        Ok(waiting) => waiting,
        Err(e) => return failed(e),
    };
    let found = waiting.iter().find(|p| p.id == id);
    if found.is_some_and(|p| p.kind != "command") {
        return Some(match status {
            "done" => Ok(Decided::Action { id, once }),
            _ => Err(crate::actions::decline(store, id)),
        });
    }
    if status == "done" {
        match found.and_then(check) {
            Some(NotNow::Weak(reply)) => return Some(Err(reply)),
            Some(NotNow::Stale(reply)) => {
                return match store.settle_proposal(id, "dropped") {
                    Ok(_) => Some(Err(reply)),
                    Err(e) => failed(e),
                }
            }
            None => {}
        }
    }
    Some(match store.settle_proposal(id, status) {
        Ok(Some(command)) if status == "done" => Ok(Decided::Command(command)),
        Ok(Some(command)) => Err(format!("[zephyr] dropped #{id} ({command})")),
        Ok(None) => Err(not_waiting(id)),
        Err(e) => return failed(e),
    })
}

/// Why `/ok N` or `/no N` found nothing to settle.
pub fn not_waiting(id: i64) -> String {
    format!(
        "[zephyr] no proposal #{id} is waiting: it was never made, is already handled, or is \
         older than 24 h"
    )
}

/// Drops a sent (or given up) file's copy: its whole per-file dir under `<state>/files`.
pub fn discard(copy: &str) {
    let dir = Path::new(copy).parent();
    if let Some(dir) =
        dir.filter(|d| d.parent().and_then(Path::file_name) == Some("files".as_ref()))
    {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweep_drops_only_stale_answers() {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-gw-sweep-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("c1"), dir.join("c2"));
        std::fs::write(&old, "late").unwrap();
        std::fs::write(&new, "fresh").unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
        super::sweep(&dir, super::GATEWAY_STALE);
        assert!(!old.exists(), "a late answer is dropped");
        assert!(new.exists(), "a waiting call's answer stays");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
