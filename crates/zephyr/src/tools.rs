//! The daemon's half of the agent's `send_file` and `schedule` tools (DESIGN §4.11). The policy
//! plugin registers them and checks their arguments in the child. Their calls reach the daemon
//! in the run's tally, read from rpi's event stream, and are carried out here when it settles.

use crate::config::Config;
use crate::store::Store;
use anyhow::{bail, Context};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use zephyr_policy::tools::MAX_FILE;

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

/// Drops a sent (or given up) file's copy: its whole per-file dir under `<state>/files`.
pub fn discard(copy: &str) {
    let dir = Path::new(copy).parent();
    if let Some(dir) =
        dir.filter(|d| d.parent().and_then(Path::file_name) == Some("files".as_ref()))
    {
        let _ = std::fs::remove_dir_all(dir);
    }
}
