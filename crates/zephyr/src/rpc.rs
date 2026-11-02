//! Drives one `rpi --mode rpc` child: JSONL on stdio, frames as in upstream
//! `crates/rpi-cli/src/rpc/protocol.rs` and `modes.rs` (v0.15.0).

use crate::config::{Account, Config};
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// The session every channel shares (DESIGN §4.1), until `/new` starts `main-<unix secs>`
/// (`Store::main_session`). Attach always shows this name.
pub const MAIN_SESSION: &str = "main";

/// The main sessions on disk, newest first, as (id, mtime in unix secs). rpi 0.15 files them as
/// `<dir>/<cwd folder>/<ISO>_<id>.jsonl`, fake-rpi as `<dir>/<id>.json`. Only `main` and the
/// `main-<secs>` ids `/new` makes count, so `/switch` never hands rpi a job's or a stray name.
/// Ends the rpi children a crashed daemon left behind (DESIGN §4.4): `zephyr supervise` starts a
/// new daemon in the same container, and an old child still holds its session open, so the new
/// one could not resume it. Their pipes to the old daemon are gone, so they have no work left.
/// Matched exactly: this `rpi` binary, `--mode rpc`, this `--session-dir`, and the agent's uid
/// (ours without one). A group is killed only when the process leads it (children since this
/// change do); one from an older build shares the supervisor's group, so only it is killed.
/// Returns how many. Called before anything is spawned, so none of these are ours.
pub fn kill_leftovers(rpi: &std::path::Path, session_dir: &std::path::Path) -> usize {
    let uid = crate::agent::ids().map_or_else(|| unsafe { libc::getuid() }, |(u, _)| u);
    let (rpi, dir) = (rpi.to_string_lossy(), session_dir.to_string_lossy());
    let me = std::process::id();
    let mut killed = 0;
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        // argv[0] is the binary, or argv[1] when it is a script run through its interpreter.
        let ours = args.iter().take(2).any(|a| *a == rpi)
            && args.windows(2).any(|w| w[0] == "--mode" && w[1] == "rpc")
            && args
                .windows(2)
                .any(|w| w[0] == "--session-dir" && w[1] == dir);
        if !ours || proc_uid(pid) != Some(uid) {
            continue;
        }
        let pid = pid as i32;
        let target = if proc_pgid(pid) == Some(pid) {
            -pid
        } else {
            pid
        };
        // SAFETY: a plain syscall; the target was matched just above.
        if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
            killed += 1;
        }
    }
    if killed > 0 {
        eprintln!("zephyr: ended {killed} rpi process(es) a previous daemon left behind");
    }
    killed
}

fn proc_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Field 5 of `/proc/<pid>/stat`, read after the command name's last `)`.
fn proc_pgid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

pub fn saved_sessions(dir: &std::path::Path) -> Vec<(String, i64)> {
    let entries = |d: &std::path::Path| {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>()
    };
    let mut found: HashMap<String, i64> = HashMap::new();
    for e in entries(dir) {
        let files = if e.path().is_dir() {
            entries(&e.path())
        } else {
            vec![e]
        };
        for f in files {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(stem) = name
                .strip_suffix(".jsonl")
                .or_else(|| name.strip_suffix(".json"))
            else {
                continue;
            };
            let id = stem.rsplit_once('_').map_or(stem, |(_, id)| id);
            let numbered = id
                .strip_prefix("main-")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if id != MAIN_SESSION && !numbered {
                continue;
            }
            let mtime = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            let at = found.entry(id.to_string()).or_insert(mtime);
            *at = (*at).max(mtime);
        }
    }
    let mut list: Vec<_> = found.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list
}

/// The slash command the policy plugin registers as proof that it loaded.
pub const POLICY_BEACON: &str = "zephyr-policy";

/// Commands zephyr sends; `doctor` checks them against `ready.supportedCommands`.
pub const USED_COMMANDS: &[&str] = &[
    "prompt",
    "get_commands",
    "queue_message",
    "clear_queue",
    "abort",
    "get_state",
    "get_messages",
    "resume_pending",
    "get_auth_status",
    "set_model",
    "stop",
];

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Child {
    pub ready: Value,
    /// Every frame that is not a command response. Closes when the child exits.
    pub events: mpsc::UnboundedReceiver<Value>,
    stdin: ChildStdin,
    pending: Pending,
    next_id: u64,
    process: tokio::process::Child,
}

impl Child {
    /// A child on `agent_dir` and `model`.
    pub async fn spawn(cfg: &Config, session_id: &str) -> anyhow::Result<Self> {
        Self::spawn_as(cfg, Some(session_id), None).await
    }

    /// A child on a pooled account: its own dir as `RPI_CODING_AGENT_DIR`, and its model.
    /// With no `session_id` it is a `/btw` child (`crate::btw`): no session file, no tools.
    pub async fn spawn_as(
        cfg: &Config,
        session_id: Option<&str>,
        account: Option<&Account>,
    ) -> anyhow::Result<Self> {
        {
            let _fs = crate::agent::fs();
            std::fs::create_dir_all(cfg.session_dir())?;
            std::fs::create_dir_all(crate::tools::gateway_dir(cfg))?;
        }
        let (model, agent_dir) = match account {
            Some(a) => (Some(&a.model), Some(crate::pool::prepare(cfg, a)?)),
            None => (cfg.model.as_ref(), cfg.agent_dir.clone()),
        };
        // rpi reads the context files (AGENTS.md) once, at start, so regenerate them first.
        let context_dir = agent_dir
            .clone()
            .unwrap_or_else(|| crate::auth_keeper::agent_dir(cfg));
        crate::memory::prepare(&cfg.home, cfg.state_dir(), &context_dir)?;
        let mut cmd = Command::new(&cfg.rpi);
        crate::agent::cmd(cmd.as_std_mut());
        // AGENTS.md is the agent's only standing instruction (memory::prepare): no skills it
        // could plant, and a trust.json it wrote can't switch AGENTS.md off. `--local-only`
        // loads only the `--extension` files below, none from a dir or setting the agent can write.
        // rpi 0.15 defaults to compact deltas; the TUI streams from each update's `message`.
        cmd.args(["--mode", "rpc", "--approve", "--no-skills", "--local-only"])
            .args(["--rpc-message-updates", "snapshot"])
            // rpi files sessions under a per-cwd folder, so the cwd must never change.
            .current_dir(&cfg.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a daemon that finds it left over from a crash can end it and
        // what it started (the provider's runtime) together: `kill_leftovers`.
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        match session_id {
            Some(id) => cmd
                .args(["--session-id", id, "--session-dir"])
                .arg(cfg.session_dir()),
            None => cmd.args(["--no-session", "--no-tools"]),
        };
        // The main session (`main`, `main-<secs>`) runs commands through zephyr's `exec`, which can
        // move to the background when the owner speaks (DESIGN §4.13); jobs and tasks keep `bash`.
        if session_id.is_some_and(|id| id == MAIN_SESSION || id.starts_with("main-")) {
            cmd.args(["--exclude-tools", "bash"])
                .env(zephyr_policy::tools::EXEC_ENV, "1");
        }
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if let Some(plugin) = &cfg.policy_plugin {
            cmd.arg("--extension").arg(plugin);
        }
        for ext in &cfg.extensions {
            cmd.arg("--extension").arg(ext);
        }
        if let Some(dir) = &agent_dir {
            cmd.env("RPI_CODING_AGENT_DIR", dir);
        }
        cmd.env(
            zephyr_policy::tools::GATEWAY_DIR_ENV,
            crate::tools::gateway_dir(cfg),
        );
        // `recall` reads the session files itself (DESIGN §4.11).
        cmd.env(zephyr_policy::tools::SESSIONS_DIR_ENV, cfg.session_dir());
        if let Some(display) = crate::desktop::ensure() {
            cmd.env("DISPLAY", display);
            // The desktop takes only clients holding its cookie (`desktop` module docs).
            if let Some(auth) = crate::desktop::xauthority() {
                cmd.env("XAUTHORITY", auth);
            }
        }
        let mut process = cmd
            .spawn()
            .with_context(|| format!("spawning {}", cfg.rpi.display()))?;
        let stdin = process.stdin.take().expect("piped stdin");
        let mut lines = BufReader::new(process.stdout.take().expect("piped stdout")).lines();

        let ready = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["type"] == "ready" {
                        return Ok(v);
                    }
                }
            }
            bail!("rpi exited before ready")
        })
        .await
        .map_err(|_| anyhow!("no ready frame from rpi within 30s"))??;

        let pending: Pending = Arc::default();
        let (tx, events) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let waiter = (v["type"] == "response")
                    .then(|| v["id"].as_u64())
                    .flatten()
                    .and_then(|id| reader_pending.lock().unwrap().remove(&id));
                match waiter {
                    Some(w) => drop(w.send(v)),
                    None => drop(tx.send(v)),
                }
            }
            // Dropping `tx` and the waiters tells the router the child is gone.
            reader_pending.lock().unwrap().clear();
        });
        let mut child = Self {
            ready,
            events,
            stdin,
            pending,
            next_id: 0,
            process,
        };
        if cfg.policy_plugin.is_some() {
            let listed = child.call(json!({"type": "get_commands"})).await?;
            let loaded = listed["commands"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| {
                    c["name"].as_str().map(|n| n.trim_start_matches('/')) == Some(POLICY_BEACON)
                });
            if !loaded {
                child.stop().await;
                bail!("policy plugin did not load: `{POLICY_BEACON}` is not in get_commands");
            }
        }
        Ok(child)
    }

    /// Sends one command and returns its `data`, or the child's error string.
    pub async fn call(&mut self, mut cmd: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&cmd)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        let Ok(resp) = tokio::time::timeout(Duration::from_secs(60), rx).await else {
            // A late answer then finds no waiter and goes out as an event, which the router ignores.
            self.pending.lock().unwrap().remove(&id);
            bail!("{} timed out", cmd["type"]);
        };
        let resp = resp.map_err(|_| anyhow!("rpi exited during {}", cmd["type"]))?;
        if resp["success"] == true {
            Ok(resp["data"].clone())
        } else {
            bail!("{}: {}", cmd["type"], resp["error"])
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.id()
    }

    /// `stop` lets rpi drain its session; then wait for the process to exit.
    pub async fn stop(mut self) {
        let _ =
            tokio::time::timeout(Duration::from_secs(5), self.call(json!({"type": "stop"}))).await;
        drop(self.stdin);
        if tokio::time::timeout(Duration::from_secs(10), self.process.wait())
            .await
            .is_err()
        {
            let _ = self.process.kill().await;
        }
    }
}

/// What one run did that zephyr answers for, read from its events.
#[derive(Debug, Default)]
pub struct Tally {
    /// Tokens and cost of the replies. rpi's `get_stats` sums only usage that extensions
    /// record, never the replies' own, so spend is summed from each assistant `message_end`.
    pub used: (i64, f64),
    /// Bash commands that sent data off the machine (`zephyr_policy::policy::outbound`).
    pub outbound: Vec<String>,
    /// Model turns taken (`turn_end`), for `budget.run_turns`.
    pub turns: u32,
    /// `send_file` and `schedule` calls that succeeded, as `(tool, args)`: the daemon carries
    /// them out when the run settles (`crate::tools`, DESIGN §4.11).
    pub requests: Vec<(String, Value)>,
    /// Those calls between their start and end, by tool call id.
    started: std::collections::HashMap<String, (String, Value)>,
}

impl Tally {
    pub fn add(&mut self, ev: &Value) {
        let m = &ev["message"];
        if ev["type"] == "message_end" && m["role"] == "assistant" {
            self.used.0 += m["usage"]["totalTokens"].as_i64().unwrap_or(0);
            self.used.1 += m["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
        }
        if ev["type"] == "turn_end" {
            self.turns += 1;
        }
        if let (Some(id), Some(tool @ ("send_file" | "schedule"))) =
            (ev["toolCallId"].as_str(), ev["toolName"].as_str())
        {
            if ev["type"] == "tool_execution_start" {
                self.started
                    .insert(id.into(), (tool.into(), ev["args"].clone()));
            }
            if ev["type"] == "tool_execution_end" {
                let call = self.started.remove(id);
                self.requests.extend(call.filter(|_| ev["isError"] != true));
            }
        }
        // From rpi, not from anything the agent writes, so the agent cannot hide a send.
        let shell = matches!(ev["toolName"].as_str(), Some("bash" | "exec"));
        if ev["type"] == "tool_execution_start" && shell {
            if let Some(command) = ev["args"]["command"].as_str() {
                self.outbound
                    .extend(zephyr_policy::policy::outbound(command));
            }
        }
    }

    /// The owner's notice of what the run sent out, if it sent anything.
    pub fn alert(&self) -> Option<String> {
        const SHOWN: usize = 10;
        if self.outbound.is_empty() {
            return None;
        }
        let mut text = String::from("[zephyr] outbound this turn:");
        for line in self.outbound.iter().take(SHOWN) {
            text.push_str("\n- ");
            text.push_str(line);
        }
        if self.outbound.len() > SHOWN {
            text.push_str(&format!("\n(+{} more)", self.outbound.len() - SHOWN));
        }
        Some(text)
    }
}

/// The text of a wire message: a plain string, or the `text` blocks of a content list.
pub fn message_text(msg: &Value) -> String {
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crashed_daemons_rpi_is_ended_and_no_other() {
        use std::os::unix::process::CommandExt;
        let fake = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"));
        let dir = crate::test_dir("rpc-leftovers");
        let (ours, other) = (dir.join("sessions"), dir.join("elsewhere"));
        let spawn = |sessions: &std::path::Path| {
            std::process::Command::new(fake)
                .args(["--mode", "rpc", "--session-id", "main", "--session-dir"])
                .arg(sessions)
                .env("ZEPHYR_HOME_FOR_FAKE", &dir)
                .current_dir(&dir)
                .stdin(std::process::Stdio::piped()) // held open: it waits on it like a real child
                .stdout(std::process::Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let (mut left, mut kept) = (spawn(&ours), spawn(&other));
        std::thread::sleep(Duration::from_millis(300)); // past the interpreter's exec
        assert_eq!(kill_leftovers(fake, &ours), 1);
        let status = left.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            kept.try_wait().unwrap().is_none(),
            "another session dir is not ours"
        );
        assert_eq!(kill_leftovers(fake, &dir.join("none")), 0);
        let _ = kept.kill();
        let _ = kept.wait();
    }

    #[test]
    fn tally_keeps_only_daemon_tool_calls_that_succeeded() {
        let mut tally = Tally::default();
        let ev = |kind: &str, id: &str, tool: &str, error: bool| {
            serde_json::json!({"type": kind, "toolCallId": id, "toolName": tool,
                               "args": {"path": id}, "isError": error})
        };
        for (id, tool, error) in [
            ("a", "send_file", false),
            ("b", "send_file", true),
            ("c", "read", false),
        ] {
            tally.add(&ev("tool_execution_start", id, tool, false));
            tally.add(&ev("tool_execution_end", id, tool, error));
        }
        tally.add(&ev(
            "tool_execution_end",
            "never-started",
            "schedule",
            false,
        ));
        assert_eq!(
            tally.requests,
            [("send_file".to_string(), serde_json::json!({"path": "a"}))]
        );
    }

    #[test]
    fn saved_sessions_reads_both_layouts_and_only_main_ids() {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-rpc-sessions-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        let real = dir.join("--zephyr--");
        std::fs::create_dir_all(&real).unwrap();
        for f in [
            "2026-10-30T03-32-24-353Z_main.jsonl",
            "2026-10-30T03-32-24-353Z_main.jsonl.lock",
            "2026-11-01T05-30-57-396Z_main-1793511057.jsonl",
            "2026-11-01T06-00-00-000Z_job-3-7.jsonl",
            "2026-11-01T06-00-00-000Z_main-x.jsonl",
        ] {
            std::fs::write(real.join(f), "").unwrap();
        }
        std::fs::write(dir.join("main-42.json"), "").unwrap();
        let mut ids: Vec<String> = saved_sessions(&dir).into_iter().map(|s| s.0).collect();
        ids.sort();
        assert_eq!(ids, ["main", "main-1793511057", "main-42"]);
        assert!(saved_sessions(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn extensions_follow_the_policy_plugin_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-rpc-ext-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let (policy, a, b) = (dir.join("policy.so"), dir.join("a.so"), dir.join("b.so"));
        for f in [&policy, &a, &b] {
            std::fs::write(f, "").unwrap();
        }
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py");
        let cfg: Config = toml::from_str(&format!(
            "rpi = {fake:?}\nhome = {:?}\npolicy_plugin = {:?}\nextensions = [{:?}, {:?}]\n",
            home.display(),
            policy.display(),
            a.display(),
            b.display(),
        ))
        .unwrap();
        Child::spawn(&cfg, "argv").await.unwrap().stop().await;

        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(home.join("argv.json")).unwrap())
                .unwrap();
        for flag in ["--approve", "--no-skills", "--local-only", "snapshot"] {
            assert!(argv.contains(&flag.to_string()), "{flag} in {argv:?}");
        }
        let at = argv.iter().position(|x| x == "--extension").unwrap();
        let want: Vec<String> = [&policy, &a, &b]
            .iter()
            .flat_map(|p| ["--extension".to_string(), p.display().to_string()])
            .collect();
        assert_eq!(argv[at..], want[..]);
    }
}
