//! Drives one `rpi --mode rpc` child: JSONL on stdio, frames as in upstream
//! `crates/rpi-cli/src/rpc/protocol.rs` and `modes.rs` (v0.12.0).

use crate::config::Config;
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// The session every channel shares (DESIGN §4.1).
pub const MAIN_SESSION: &str = "main";

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
    "get_stats",
    "resume_pending",
    "get_auth_status",
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
    pub async fn spawn(cfg: &Config, session_id: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cfg.session_dir())?;
        // rpi reads the context files (AGENTS.md) once, at start, so regenerate them first.
        crate::memory::prepare(&cfg.home)?;
        let mut cmd = Command::new(&cfg.rpi);
        cmd.args(["--mode", "rpc", "--session-id", session_id, "--session-dir"])
            .arg(cfg.session_dir())
            // rpi files sessions under a per-cwd folder, so the cwd must never change.
            .current_dir(&cfg.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(model) = &cfg.model {
            cmd.args(["--model", model]);
        }
        if let Some(plugin) = &cfg.policy_plugin {
            cmd.arg("--extension").arg(plugin);
        }
        for ext in &cfg.extensions {
            cmd.arg("--extension").arg(ext);
        }
        if let Some(dir) = &cfg.agent_dir {
            cmd.env("RPI_CODING_AGENT_DIR", dir);
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
        let resp = tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .map_err(|_| anyhow!("{} timed out", cmd["type"]))?
            .map_err(|_| anyhow!("rpi exited during {}", cmd["type"]))?;
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
        let at = argv.iter().position(|x| x == "--extension").unwrap();
        let want: Vec<String> = [&policy, &a, &b]
            .iter()
            .flat_map(|p| ["--extension".to_string(), p.display().to_string()])
            .collect();
        assert_eq!(argv[at..], want[..]);
    }
}
