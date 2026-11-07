//! `zephyr tui` against a scripted attach server, drawn in tmux at fixed sizes and compared with
//! the goldens in tests/screens/ (`ZEPHYR_UPDATE_SCREENS=1` rewrites them). Skipped without tmux.

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const TOKEN: &str = "screen-token";
/// 2026-10-30 14:02 UTC.
const T: i64 = 1_793_368_920_000;

fn user(channel: &str, id: u64, text: &str, at: i64) -> Value {
    json!({"kind": "user", "role": "user", "content": format!("[{channel} #{id}] {text}"), "timestamp": at,
           "origin": {"channel": channel, "id": id}, "text": text})
}

fn assistant(at: i64, content: Value) -> Value {
    json!({"kind": "assistant", "role": "assistant", "timestamp": at, "content": content})
}

fn result(id: &str, name: &str, text: &str, error: bool, at: i64) -> Value {
    json!({"kind": "tool_result", "role": "toolResult", "toolCallId": id, "toolName": name,
           "content": [{"type": "text", "text": text}], "isError": error, "timestamp": at})
}

fn history() -> Vec<Value> {
    vec![
        user("discord", 7, "remind me to renew the domain on friday", T),
        assistant(
            T + 1_000,
            json!([
            {"type": "text", "text": "Done. I scheduled **renew kimchigoose.com** for Fri 09:00 KST."},
            {"type": "toolCall", "id": "c1", "name": "schedule", "arguments": {"prompt": "add \"renew domain\" fri 09:00"}}]),
        ),
        result("c1", "schedule", "job 3 added", false, T + 1_200),
        user("tui", 8, "what's in ~/Projects/zephyr/deploy?", T + 180_000),
        assistant(
            T + 181_000,
            json!([
            {"type": "toolCall", "id": "c2", "name": "bash", "arguments": {"command": "ls ~/Projects/zephyr/deploy"}}]),
        ),
        result(
            "c2",
            "bash",
            "Dockerfile  run.sh  remove-account.sh",
            false,
            T + 181_100,
        ),
        // The footer's `ctx 42%` comes from this reply's usage.
        json!({"kind": "assistant", "role": "assistant", "timestamp": T + 182_000,
               "usage": {"input": 1_200, "cacheRead": 82_000, "cacheWrite": 400, "output": 400},
               "content": [{"type": "text", "text": "Three files: the image recipe, the launcher, and a one-time cleanup script."}]}),
    ]
}

/// The scenes with nothing running: "waiting" is "idle" with a proposal waiting for the owner.
fn idle(scene: &str) -> bool {
    matches!(scene, "paused" | "down" | "idle" | "waiting")
}

fn snapshot(scene: &str) -> Value {
    let now = zephyr::unix_millis() as i64;
    let (busy, paused, cost, failed) = match scene {
        "paused" => (false, json!("user"), 4.2, 1),
        _ if idle(scene) => (false, Value::Null, 0.38, 0),
        _ => (true, Value::Null, 0.38, 0),
    };
    let run = if busy {
        json!({"busy": true, "since_unix_ms": now - 12_000, "origin": {"channel": "imessage", "id": 9, "sender": "me"},
               "current_tool": "bash", "tools_done": 1})
    } else {
        json!({"busy": false, "since_unix_ms": null, "origin": null, "current_tool": null, "tools_done": 0})
    };
    json!({
        "version": "0.1.0", "session": "main", "account": "claude-code-1", "model": "claude-code/claude-opus-5-5",
        "thinking": "medium", "context_window": 200_000, "paused": paused,
        "channels": [{"name": "discord", "state": "connected"}, {"name": "imessage", "state": "connected"},
                     {"name": "chat", "state": "off"}],
        "spend": {"tokens": 41_200, "cost": cost, "daily_tokens_limit": null, "daily_cost_limit": 5.0},
        "queue": if busy { json!([{"channel": "discord", "preview": "are you around later?", "since_unix": now / 1000 - 30}]) } else { json!([]) },
        "failed_replies": failed,
        "run": run,
        // Fixed times far ahead, so the panel reads the same on any day.
        "accounts": [
            {"name": "claude-code-1", "model": "claude-code/claude-opus-5-5", "state": "ready", "until_unix": null},
            {"name": "codex-2", "model": "openai/gpt-6-astra", "state": "cooling", "until_unix": COOL_UNTIL},
        ],
        "jobs": [
            {"id": 3, "name": "renew domain", "next_unix": COOL_UNTIL + 3_600},
            {"id": 1, "name": "daily-brief", "next_unix": COOL_UNTIL + 86_400},
        ],
        "proposals": if scene == "waiting" { 1 } else { 0 },
    })
}

/// 2030-03-01 09:00 UTC.
const COOL_UNTIL: i64 = 1_898_586_000;

/// `zephyr.settings` as the daemon answers it for a config with one connector (settings.rs).
fn settings() -> Value {
    let row = |key: &str,
               label: &str,
               help: &str,
               kind: Value,
               value: Value,
               source: &str,
               access: &str| {
        json!({"key": key, "label": label, "help": help, "kind": kind, "value": value, "source": source,
               "access": access, "strong": key.starts_with("budget.") || key == "settings.strong_channels",
               "until": if source.starts_with("until") { json!(COOL_UNTIL) } else { Value::Null }})
    };
    let int = |min: i64, max: i64| json!({"type": "int", "min": min, "max": max});
    let optional = |inner: Value| json!({"type": "optional", "inner": inner});
    let fixed = |key: &str, label: &str, help: &str| {
        row(
            key,
            label,
            help,
            json!({"type": "text", "max": 0}),
            Value::Null,
            "you",
            "read-only",
        )
    };
    let thinking = json!({"type": "choice", "options": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]});
    json!({"sections": [
        {"name": "Model", "settings": [
            row("model", "Model", "The model the active account runs, PROVIDER/MODEL.", json!({"type": "model"}),
                json!("claude-code/claude-opus-5-5"), "you", "propose"),
            row("thinking", "Thinking level", "How much the model thinks before it answers (this session only).",
                thinking, json!("medium"), "default", "propose"),
        ]},
        {"name": "Background tasks", "settings": [
            row("tasks.parallel", "Tasks at once", "How many background tasks run at the same time.", int(1, 32),
                json!("2"), "default", "tighten"),
            row("tasks.model", "Task model", "The model background tasks use unless they name one; none follows the account.",
                optional(json!({"type": "model"})), Value::Null, "default", "propose"),
            row("subagent", "Subagents", "Named helpers the agent starts tasks as: add, prompt, model, rm.",
                json!({"type": "text", "max": 4000}), json!("reviewer"), "you", "propose"),
        ]},
        {"name": "Budget", "settings": [
            row("budget.daily_tokens", "Daily tokens", "Tokens per UTC day before the agent pauses; none is no limit.",
                optional(int(1, i64::MAX)), Value::Null, "default", "tighten"),
            row("budget.daily_cost_usd", "Daily cost (USD)", "Dollars per UTC day before the agent pauses; none is no limit.",
                optional(json!({"type": "float", "min": 0.0, "max": 1.0e9})), json!("5"), "until 18:00", "tighten"),
            row("budget.run_turns", "Turns per run", "Model turns one run may take; none is no limit.",
                optional(int(1, 100_000)), json!("40"), "agent (#7)", "tighten"),
            row("budget.run_minutes", "Minutes per run", "Minutes one main-session run may take; none is no limit.",
                optional(json!({"type": "float", "min": 0.1, "max": 100_000.0})), Value::Null, "default", "tighten"),
        ]},
        {"name": "General", "settings": [
            row("timezone", "Time zone", "IANA name for job schedules, e.g. Asia/Seoul; none follows the system.",
                optional(json!({"type": "text", "max": 64})), json!("UTC"), "you", "propose"),
        ]},
        {"name": "Accounts", "settings": [
            row("auth.enabled", "Token refresh", "Refresh expiring sign-ins in the background.", json!({"type": "bool"}),
                json!("true"), "default", "propose"),
            row("auth.refresh_minutes", "Refresh check (minutes)", "How often to look for expiring sign-ins.", int(1, 60),
                json!("5"), "default", "propose"),
        ]},
        {"name": "Connectors", "settings": [
            row("mcp.github.enabled", "Connector enabled", "Whether the agent may call this connector.", json!({"type": "bool"}),
                json!("true"), "you", "tighten"),
        ]},
        {"name": "Rules", "settings": [
            row("rule", "Standing rules", "Rules the agent follows in every run: add, rm.", json!({"type": "text", "max": 4000}),
                json!("2 rules"), "you", "propose"),
        ]},
        {"name": "Security", "settings": [
            row("settings.strong_channels", "Strong channels",
                "Channels that may loosen a protected setting; the agent may only remove entries.",
                json!({"type": "list", "options": ["tui", "chat", "discord", "imessage"]}), json!("tui, chat"), "default", "tighten"),
        ]},
        {"name": "Read-only", "settings": [
            fixed("owners", "Owners", "Who may talk to the agent, per channel."),
            fixed("discord", "Discord", "The Discord adapter."),
            fixed("imessage", "iMessage", "The BlueBubbles adapter."),
            fixed("attach", "Attach", "The rpi --connect listener."),
            fixed("agent.env", "Agent environment", "Variables the agent's processes get."),
            fixed("extensions", "Extensions", "Extra rpi extensions."),
            fixed("policy_plugin", "Policy plugin", "The zephyr-policy library."),
            fixed("rpi", "rpi", "The rpi binary."),
            fixed("home", "Home", "The agent's home folder."),
            fixed("state", "State", "The daemon's state folder."),
            fixed("account", "Accounts", "The [[account]] pool, in failover order."),
        ]},
    ], "strong_channels": ["tui", "chat"]})
}

/// `zephyr.proposals` in the "waiting" scene: the agent asks for a bigger budget for a while.
fn proposals(scene: &str) -> Value {
    if scene != "waiting" {
        return json!({"proposals": []});
    }
    json!({"proposals": [{"id": 12, "card": "/set budget.daily_cost_usd 8 for 2h", "kind": "setting",
        "key": "budget.daily_cost_usd", "age_secs": 180, "expires_in_secs": 86_220, "strong": true}]})
}

/// What the router answers `zephyr.settings_set` with: `tasks.parallel` takes 1 to 32.
fn settings_set(params: &Value) -> Value {
    let (key, value) = (
        params["key"].as_str().unwrap_or_default(),
        params["value"].as_str().unwrap_or_default(),
    );
    match value.parse::<i64>() {
        Ok(n) if key == "tasks.parallel" && !(1..=32).contains(&n) => {
            json!({"outcome": "refused", "reason": "not changed: [tasks] parallel must be 1 to 32"})
        }
        _ => {
            json!({"outcome": "changed", "key": key, "old": "2", "new": value, "applied": "[zephyr] applied",
                    "until": null})
        }
    }
}

fn models() -> Value {
    json!({"accounts": [
        {"name": "claude-code-1", "model": "claude-code/claude-opus-5-5",
         "models": ["claude-code/claude-opus-5-5", "claude-code/claude-sonnet-5", "claude-code/claude-haiku-4-5"]},
        {"name": "codex-2", "model": "openai/gpt-6-astra", "models": ["openai/gpt-6-astra", "openai/gpt-6-mini"]},
    ]})
}

/// The live events after `ready` and the first status.
fn live(scene: &str) -> Vec<Value> {
    let mut events = vec![json!({"type": "zephyr_reply", "command": "/status",
        "text": "agent: claude-code-1 on claude-code/claude-opus-5-5\nchannels: discord connected, imessage connected\ntoday: 41.2k tokens, $0.38 of $5"})];
    if idle(scene) {
        return events;
    }
    events.extend([
        json!({"type": "message_start", "message": user("imessage", 9, "why did the nightly backup fail?", T + 400_000)}),
        json!({"type": "message_end", "message": assistant(T + 401_000, json!([
            {"type": "text", "text": "Checking the backup log first."},
            {"type": "toolCall", "id": "c3", "name": "read", "arguments": {"path": "/var/log/backup.log"}},
            {"type": "toolCall", "id": "c4", "name": "bash", "arguments": {"command": "journalctl -u backup --since today"}}]))}),
        json!({"type": "tool_execution_start", "toolCallId": "c3", "toolName": "read", "args": {"path": "/var/log/backup.log"}}),
        json!({"type": "tool_execution_end", "toolCallId": "c3", "toolName": "read", "isError": true,
               "result": {"content": [{"type": "text", "text": "EACCES: permission denied, open '/var/log/backup.log'"}]}}),
        json!({"type": "tool_execution_start", "toolCallId": "c4", "toolName": "bash", "args": {"command": "journalctl -u backup --since today"}}),
    ]);
    if scene == "retry" {
        events.push(
            json!({"type": "retry_scheduled", "attempt": 2, "maxRetries": 10, "delayMs": 8_000,
                           "error": "429 rate limited: too many requests"}),
        );
    }
    events
}

/// Serves `scene` to every connection and passes on each prompt sent. "down" serves the first
/// connection for a moment, then closes it and every later one: a daemon that went away.
/// "connecting" never listens.
async fn serve(listener: TcpListener, scene: &'static str, sent: mpsc::UnboundedSender<String>) {
    if scene == "connecting" {
        return;
    }
    for n in 0.. {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let sent = sent.clone();
        tokio::spawn(async move {
            if scene == "down" && n > 0 {
                return;
            }
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let frame = |v: Value| format!("{v}\n");
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req["id"].clone();
                let result = match req["method"].as_str().unwrap() {
                    "authenticate" if req["params"]["token"] == TOKEN => {
                        json!({"authenticated": true})
                    }
                    "zephyr.hello" => snapshot(scene),
                    "zephyr.subscribe" => {
                        json!({"subscriptionId": 1, "sessionId": "main", "status": "subscribed", "messages": history()})
                    }
                    "zephyr.models" => models(),
                    "zephyr.vault_list" => json!({"entries": []}),
                    // The daemon answers what anyone may see: never the value.
                    "zephyr.vault_put" => json!({"entries": [{
                        "kind": req["params"]["kind"], "name": req["params"]["name"],
                        "granted": false, "programs": [],
                    }]}),
                    "zephyr.turns" => json!({"turns": [
                        {"channel": "tui", "text": "what's in ~/Projects/zephyr/deploy?"},
                        {"channel": "discord", "text": "remind me to renew the domain on friday"},
                    ]}),
                    "zephyr.settings" => settings(),
                    "zephyr.proposals" => proposals(scene),
                    // The changes and decisions are passed on like prompts, for the test to read.
                    "zephyr.settings_set" => {
                        let p = &req["params"];
                        let until = p["until"]
                            .as_str()
                            .map_or(String::new(), |u| format!(" for {u}"));
                        let _ = sent.send(format!(
                            "zephyr.settings_set {} {}{until}",
                            p["key"].as_str().unwrap(),
                            p["value"].as_str().unwrap()
                        ));
                        settings_set(p)
                    }
                    "zephyr.decide" => {
                        let p = &req["params"];
                        let _ = sent.send(format!(
                            "zephyr.decide {} {}",
                            p["id"],
                            p["verb"].as_str().unwrap()
                        ));
                        json!({"ok": true, "text": "[zephyr] /ok 12 approved /set budget.daily_cost_usd 8 for 2h\n\
                               [zephyr] budget.daily_cost_usd 5 → 8 until 16:02"})
                    }
                    "send" => {
                        let text = req["params"]["command"]["content"]
                            .as_str()
                            .unwrap_or_default();
                        let _ = sent.send(text.to_string());
                        json!({"sessionId": "main", "status": "sent"})
                    }
                    _ => json!({"error": "unexpected"}),
                };
                let ok = json!({"jsonrpc": "2.0", "id": id, "result": result});
                write.write_all(frame(ok).as_bytes()).await.unwrap();
                // What the router answers to the commands the tests send.
                let text = req["params"]["command"]["content"]
                    .as_str()
                    .unwrap_or_default();
                let reply = match text {
                    "/stop" => Some("[zephyr] stopped".to_string()),
                    t if t.starts_with("/model ") => Some(format!("[zephyr] now on {}", &t[7..])),
                    t if t.starts_with("/undo") => Some("[zephyr] undone".to_string()),
                    _ => None,
                };
                if let Some(reply) = reply {
                    let ev = json!({"type": "zephyr_reply", "command": text, "text": reply});
                    let ev = json!({"jsonrpc": "2.0", "method": "event", "params": {"subscriptionId": 1, "event": ev}});
                    write.write_all(frame(ev).as_bytes()).await.unwrap();
                }
                if req["method"] == "zephyr.subscribe" {
                    let mut events = vec![
                        json!({"type": "ready"}),
                        json!({"type": "zephyr_status", "snapshot": snapshot(scene)}),
                    ];
                    events.extend(live(scene));
                    for ev in events {
                        let ev = json!({"jsonrpc": "2.0", "method": "event", "params": {"subscriptionId": 1, "event": ev}});
                        write.write_all(frame(ev).as_bytes()).await.unwrap();
                    }
                    if scene == "down" {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        return;
                    }
                }
            }
        });
    }
}

struct Tmux(String);

impl Tmux {
    fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-L", &self.0, "-f", "/dev/null"]);
        c
    }

    fn capture(&self, escapes: bool) -> String {
        let mut c = self.cmd();
        c.args(["capture-pane", "-p", "-t", "screen"]);
        if escapes {
            c.arg("-e");
        }
        String::from_utf8_lossy(&c.output().unwrap().stdout).into_owned()
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = self.cmd().arg("kill-server").output();
    }
}

fn config(dir: &Path, port: u16) -> PathBuf {
    let token = dir.join("tui.token");
    std::fs::write(&token, TOKEN).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.join("zephyr.toml");
    let toml = format!(
        "rpi = \"/nonexistent/rpi\"\nhome = {dir:?}\ntimezone = \"UTC\"\n[attach]\nport = {port}\ntoken_file = {token:?}\n"
    );
    std::fs::write(&path, toml).unwrap();
    path
}

/// Live timers (the run line's glyph and clock, a live tool card's time) move between frames;
/// everything else must match.
fn normalize(screen: &str) -> String {
    screen
        .lines()
        .enumerate()
        .map(|(row, l)| {
            // The mascot (the header's first 14 columns) moves with the clock and greets on
            // connecting; chrome.rs tests its frames. Masking keeps its width checked.
            // shortcut: masks rows 0-2 at any width; a golden under 60 columns (no mascot) would
            // hide header text here, so narrow the mask by width before adding one.
            let l = &if row < 3 {
                "#".repeat(14) + &l.chars().skip(14).collect::<String>()
            } else {
                l.to_string()
            };
            // The "down" server drops each reconnect: EOF or a reset by timing, so the reason varies.
            let l = &match l.find("reconnecting…  ") {
                Some(i) if l.contains("your draft is kept") => {
                    format!("{}reconnecting… your draft is kept", &l[..i])
                }
                _ => l.to_string(),
            };
            // `* ` is `● ` under LANG=C: a running tool's clock.
            let live = ["esc to stop", "retry ", "● ", "✗ ", "* "];
            if !live.iter().any(|m| l.contains(m)) {
                return l.trim_end().to_string();
            }
            let mut out = String::new();
            let mut digits = false;
            for (i, c) in l.trim_end().chars().enumerate() {
                match c {
                    '∼' | '≈' | '≋' if i == 1 => out.push('~'),
                    '0'..='9' if !digits => {
                        digits = true;
                        out.push('N')
                    }
                    '0'..='9' => {}
                    c => {
                        digits = false;
                        out.push(c)
                    }
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tiny PNG, for `@` references.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Each screen its own dir (config, token) and tmux server; the name is part of a unix socket
/// path, so it stays short. The dir holds a projects folder for `@` and an editor for Ctrl+G
/// that appends ` edited`.
fn screen_dir() -> (String, PathBuf) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
        "zt-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(&name);
    std::fs::create_dir_all(dir.join("projects/deploy")).unwrap();
    std::fs::write(dir.join("projects/notes.md"), "x").unwrap();
    std::fs::write(dir.join("projects/shot.png"), PNG).unwrap();
    std::fs::write(dir.join("ed.sh"), "printf ' edited' >> \"$1\"\n").unwrap();
    (name, dir)
}

/// `zephyr tui` on `cfg` in a new tmux server. `theme`: `dark` or `light` (by COLORFGBG),
/// `ascii` (dark, C locale) or `nocolor`.
fn tui_in_tmux(name: String, cfg: &Path, dir: &Path, cols: u16, rows: u16, theme: &str) -> Tmux {
    let tmux = Tmux(name);
    let command = format!(
        "{} tui; echo zephyr exited $?; sleep 30",
        env!("CARGO_BIN_EXE_zephyr")
    );
    let (cols, rows) = (cols.to_string(), rows.to_string());
    let status = tmux
        .cmd()
        .args([
            "new-session",
            "-d",
            "-s",
            "screen",
            "-x",
            &cols,
            "-y",
            &rows,
        ])
        .args(["-e", &format!("ZEPHYR_CONFIG={}", cfg.display())])
        .args(["-e", &format!("HOME={}", dir.display())])
        .args([
            "-e",
            &format!("ZEPHYR_PROJECTS={}", dir.join("projects").display()),
        ])
        .args(["-e", &format!("EDITOR=sh {}", dir.join("ed.sh").display())])
        .args(["-e", "VISUAL="])
        .args([
            "-e",
            if theme == "light" {
                "COLORFGBG=0;15"
            } else {
                "COLORFGBG=15;0"
            },
        ])
        .args([
            "-e",
            if theme == "nocolor" {
                "NO_COLOR=1"
            } else {
                "NO_COLOR="
            },
        ])
        .args(["-e", "TERM=xterm-256color", "-e", "RPI_SERVER_TOKEN="])
        .args(match theme {
            "ascii" => ["-e", "LANG=C", "-e", "LC_ALL=C"],
            _ => ["-e", "LANG=C.UTF-8", "-e", "LC_ALL=C.UTF-8"],
        })
        .arg(command)
        .status()
        .unwrap();
    assert!(status.success());
    tmux
}

/// `zephyr tui` running in tmux against the fake server.
struct Screen {
    tmux: Tmux,
    dir: PathBuf,
    server: tokio::task::JoinHandle<()>,
    sent: mpsc::UnboundedReceiver<String>,
}

impl Screen {
    /// `theme`: `dark` or `light` (by COLORFGBG), `ascii` (dark, C locale) or `nocolor`.
    async fn start(scene: &'static str, cols: u16, rows: u16, theme: &str) -> Screen {
        let (name, dir) = screen_dir();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, sent) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(listener, scene, tx));
        if scene == "connecting" {
            // Let the listener close, so the port refuses.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let cfg = config(&dir, port);
        let tmux = tui_in_tmux(name, &cfg, &dir, cols, rows, theme);
        Screen {
            tmux,
            dir,
            server,
            sent,
        }
    }

    /// The plain screen once it shows `text`. A read while the TUI is still writing a frame is
    /// torn (half a panel over the last frame), so the screen counts once two reads agree,
    /// apart from what moves with the clock (`normalize` masks it).
    async fn wait_for(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last: Option<String> = None;
        loop {
            let screen = self.tmux.capture(false);
            if screen.contains(text) {
                let now = normalize(&screen);
                if last.as_ref() == Some(&now) {
                    return screen;
                }
                last = Some(now);
            } else {
                last = None;
                assert!(
                    !screen.contains("zephyr exited"),
                    "zephyr tui exited:\n{screen}"
                );
            }
            assert!(
                Instant::now() < deadline,
                "no {text:?} on screen:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// tmux key names (`Enter`, `C-o`), or literal text with a leading `=`.
    fn keys(&self, keys: &[&str]) {
        for k in keys {
            let mut c = self.tmux.cmd();
            c.args(["send-keys", "-t", "screen"]);
            match k.strip_prefix('=') {
                Some(text) => c.args(["-l", text]),
                None => c.arg(k),
            };
            assert!(c.status().unwrap().success());
        }
    }

    async fn next_sent(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(10), self.sent.recv())
            .await
            .expect("nothing sent within 10s")
            .unwrap()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tmux_missing() -> bool {
    let missing = Command::new("tmux").arg("-V").output().is_err();
    if missing {
        eprintln!("skipping: tmux is not installed");
    }
    missing
}

/// Starts `zephyr tui` for `scene`, presses `keys` once `ready` shows, and returns the plain
/// and escaped screens once `shown` shows. A key `~text` waits for `text` before the next one,
/// for a panel whose answer must arrive before its keys mean anything.
async fn shoot(
    scene: &'static str,
    cols: u16,
    rows: u16,
    theme: &str,
    (ready, keys, shown): (&str, &[&str], &str),
) -> (String, String) {
    let screen = Screen::start(scene, cols, rows, theme).await;
    screen.wait_for(ready).await;
    if scene == "down" {
        // A draft typed while away stays in the box.
        screen.keys(&["=half-typed draft"]);
    }
    for k in keys {
        match k.strip_prefix('~') {
            Some(text) => drop(screen.wait_for(text).await),
            None => screen.keys(&[k]),
        }
    }
    screen.wait_for(shown).await;
    // Anything that arrived with the marker is drawn once the screen stops changing, apart from
    // what moves with the clock (`normalize` masks it).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = normalize(&screen.tmux.capture(false));
    let mut same = 0;
    while same < 3 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = normalize(&screen.tmux.capture(false));
        same = if now == last { same + 1 } else { 0 };
        last = now;
        assert!(
            Instant::now() < deadline,
            "the screen never settled:\n{last}"
        );
    }
    (screen.tmux.capture(false), screen.tmux.capture(true))
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_send_stop_expand_warn_and_detach() {
    if tmux_missing() {
        return;
    }
    let mut screen = Screen::start("busy", 80, 24, "dark").await;
    screen.wait_for("esc to stop").await;
    screen.keys(&["=hello zephyr", "Enter"]);
    assert_eq!(screen.next_sent().await, "hello zephyr");
    screen.keys(&["Escape"]);
    assert_eq!(screen.next_sent().await, "/stop");
    screen.wait_for("stopping…").await;
    // The router's `[zephyr] stopped` is a notice, not a card.
    let stopped = screen.wait_for("· stopped").await;
    assert!(!stopped.contains("─ /stop"), "{stopped}");
    screen.keys(&["C-o"]);
    let expanded = screen.wait_for("▾ bash").await;
    assert!(expanded.contains("▾ read"), "{expanded}");
    screen.keys(&["C-c"]);
    screen.wait_for("press ^C again to detach").await;
    screen.keys(&["C-d"]);
    let gone = screen.wait_for("zephyr exited 0").await;
    assert!(
        gone.contains("detached — zephyr keeps running (zephyr attach to return)"),
        "{gone}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn panels_pick_a_model_list_commands_and_close() {
    if tmux_missing() {
        return;
    }
    let mut screen = Screen::start("idle", 80, 24, "dark").await;
    screen.wait_for("^L model").await;
    // ^L: the picker starts on the current model; typing filters; Enter sends the pick.
    screen.keys(&["C-l"]);
    let picker = screen.wait_for("claude-sonnet-5").await;
    assert!(picker.contains("› ● claude-opus-5-5"), "{picker}");
    assert!(picker.contains("esc close"), "{picker}");
    screen.keys(&["=mini", "Enter"]);
    assert_eq!(screen.next_sent().await, "/model openai/gpt-6-mini");
    let after = screen.wait_for("· now on openai/gpt-6-mini").await;
    assert!(!after.contains("╭─ model"), "{after}");
    // `?` on an empty composer: help, grouped; Esc closes it.
    screen.keys(&["?"]);
    let help = screen.wait_for("╭─ commands").await;
    assert!(help.contains("Chat") && help.contains("/new"), "{help}");
    screen.keys(&["Escape"]);
    // Slash autocomplete: `/sta` then Enter runs /status, which opens the panel here.
    screen.keys(&["=/sta"]);
    let menu = screen.wait_for("agent, accounts, channels").await;
    assert!(menu.contains("tab complete"), "{menu}");
    screen.keys(&["Enter"]);
    let status = screen.wait_for("╭─ status").await;
    assert!(status.contains("cooling until 1 Mar 09:00"), "{status}");
    assert!(status.contains("renew domain"), "{status}");
    screen.keys(&["Escape"]);
    // ^S opens it too; ^C closes like Esc; then a plain message still goes out.
    screen.keys(&["C-s"]);
    screen.wait_for("╭─ status").await;
    screen.keys(&["C-c", "=hi", "Enter"]);
    assert_eq!(screen.next_sent().await, "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_composer_recalls_completes_stashes_and_edits() {
    if tmux_missing() {
        return;
    }
    let mut screen = Screen::start("idle", 80, 24, "dark").await;
    let projects = screen.dir.join("projects");
    std::fs::create_dir_all(screen.dir.join("prompts")).unwrap();
    std::fs::write(screen.dir.join("prompts/hi.md"), "Say hi to $1\n").unwrap();
    screen.wait_for("Three files").await;
    // Up brings back your own message from the replayed conversation.
    screen.keys(&["Up", "Enter"]);
    assert_eq!(
        screen.next_sent().await,
        "what's in ~/Projects/zephyr/deploy?"
    );
    // `@` completes in the projects folder, and an image goes with its attachment line.
    screen.keys(&["=look @sh", "Tab", "Enter"]);
    let shot = projects.join("shot.png");
    assert_eq!(
        screen.next_sent().await,
        format!(
            "look @{0}\n[attached image: {0} (shot.png, image/png, 1 KB); open it with read]",
            shot.display()
        )
    );
    // ^C clears; Up on the empty box brings it back.
    screen.keys(&["=half done", "C-c", "Up", "Enter"]);
    assert_eq!(screen.next_sent().await, "half done");
    // Alt+S puts a draft aside for another message, and brings it back.
    screen.keys(&["=aside", "M-s", "=other", "Enter"]);
    assert_eq!(screen.next_sent().await, "other");
    screen.keys(&["M-s", "Enter"]);
    assert_eq!(screen.next_sent().await, "aside");
    // ^R finds an earlier message and puts it in the box, unsent.
    screen.keys(&["C-r"]);
    screen.wait_for("╭─ history").await;
    screen.keys(&["=deploy", "Enter", "=!", "Enter"]);
    assert_eq!(
        screen.next_sent().await,
        "what's in ~/Projects/zephyr/deploy?!"
    );
    // A saved prompt fills the box; Enter sends what you read.
    screen.keys(&["=/p hi bob", "Enter"]);
    screen.wait_for("Say hi to bob").await;
    screen.keys(&["Enter"]);
    assert_eq!(screen.next_sent().await, "Say hi to bob");
    // ^G hands the draft to $EDITOR and takes back what it wrote.
    screen.keys(&["=draft", "C-g"]);
    screen.wait_for("draft edited").await;
    screen.keys(&["Enter"]);
    assert_eq!(screen.next_sent().await, "draft edited");
}

/// A channel that never says anything: the router runs as long as its channels do.
struct Quiet;

impl zephyr::channel::Channel for Quiet {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, _: mpsc::Sender<zephyr::channel::Inbound>) -> anyhow::Result<()> {
        std::future::pending().await
    }
    async fn send(&self, _: &zephyr::channel::Outbound) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An `@image` typed in the TUI reaches the agent with the line a Discord photo gets: the real
/// router and attach server, with fake-rpi as the child.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_reference_reaches_the_agent_with_its_attachment_line() {
    if tmux_missing() {
        return;
    }
    let (name, dir) = screen_dir();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg_path = config(&dir, port);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py");
    let toml = std::fs::read_to_string(&cfg_path)
        .unwrap()
        .replace("\"/nonexistent/rpi\"", &format!("{fake:?}"))
        .replace(
            "timezone",
            &format!(
                "agent_dir = {:?}\nmodel = \"fake/m1\"\ntimezone",
                dir.join("agent")
            ),
        );
    std::fs::write(&cfg_path, toml).unwrap();
    let cfg = zephyr::config::Config::load(&cfg_path).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = tokio::spawn(async move {
        zephyr::router::run(&cfg, &[Quiet], async {
            let _ = stopped.await;
        })
        .await
    });
    let screen = Screen {
        tmux: tui_in_tmux(name, &cfg_path, &dir, 80, 24, "dark"),
        dir: dir.clone(),
        server: tokio::spawn(async {}),
        sent: mpsc::unbounded_channel().1,
    };
    screen.wait_for("main · m1").await;
    screen.keys(&["=what is in @shot.png", "Enter"]);
    let shot = dir.join("projects/shot.png");
    let want = format!(
        "[tui #1] what is in @{0}\n[attached image: {0} (shot.png, image/png, 1 KB); open it with read]",
        shot.display()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(dir.join("log.jsonl")).unwrap_or_default();
        let prompts: Vec<Value> = (log.lines())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["type"] == "prompt")
            .collect();
        if let Some(p) = prompts.first() {
            assert_eq!(p["message"], want.as_str());
            break;
        }
        assert!(!router.is_finished(), "the router stopped");
        let shown = screen.tmux.capture(false);
        assert!(Instant::now() < deadline, "no prompt:\n{log}\n{shown}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop.send(());
    router.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_esc_rewinds_to_a_picked_message() {
    if tmux_missing() {
        return;
    }
    let mut screen = Screen::start("idle", 80, 24, "dark").await;
    screen.wait_for("^L model").await;
    screen.keys(&["Escape", "Escape"]);
    screen.wait_for("╭─ rewind").await;
    let picker = screen.wait_for("renew the domain on friday  discord").await;
    assert!(
        picker.contains("› what's in ~/Projects/zephyr/deploy?  you"),
        "{picker}"
    );
    // The pick takes it back and puts it in the composer to edit.
    screen.keys(&["Enter"]);
    assert_eq!(screen.next_sent().await, "/undo 1");
    let after = screen.wait_for("· undone").await;
    assert!(!after.contains("╭─ rewind"), "{after}");
    assert!(
        after.contains("│ what's in ~/Projects/zephyr/deploy?"),
        "{after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_a_vault_entry_shows_its_card_without_the_value() {
    if tmux_missing() {
        return;
    }
    let screen = Screen::start("idle", 80, 24, "dark").await;
    screen.wait_for("^L model").await;
    screen.keys(&["=/vault add token GH_TOKEN", "Enter"]);
    screen.wait_for("value").await;
    screen.keys(&["=hunter2", "Enter"]);
    let card = screen.wait_for("saved the token GH_TOKEN").await;
    assert!(card.contains("each program needs your OK once"), "{card}");
    assert!(
        card.contains("the agent was told it can use it now"),
        "{card}"
    );
    assert!(!card.contains("hunter2"), "{card}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_settings_panel_opens_from_slash_settings() {
    if tmux_missing() {
        return;
    }
    let mut screen = Screen::start("waiting", 100, 30, "dark").await;
    // The header says something waits for the owner.
    let first = screen.wait_for("^L model").await;
    assert!(
        first
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .ends_with("1 waiting"),
        "{first}"
    );
    screen.keys(&["=/settings", "Enter"]);
    let panel = screen.wait_for("Waiting (1)").await;
    assert!(panel.contains("╭─ settings"), "{panel}");
    assert!(
        panel.contains("› Model                Model           claude-code/claude-opus-5-5 · you"),
        "{panel}"
    );
    assert!(panel.contains("? keys · esc close"), "{panel}");
    // A number out of bounds is refused, and the reason shows in the editor.
    screen.keys(&["Down", "Right"]);
    screen
        .wait_for("How many background tasks run at the same time.")
        .await;
    screen.keys(&["Enter"]);
    screen.wait_for("a whole number, 1 to 32").await;
    screen.keys(&["BSpace", "=40", "Enter"]);
    assert_eq!(
        screen.next_sent().await,
        "zephyr.settings_set tasks.parallel 40"
    );
    let refused = screen.wait_for("[tasks] parallel must be 1 to 32").await;
    assert!(refused.contains("› value  40"), "{refused}");
    // Fixed, it goes through; the editor closes on the change.
    screen.keys(&["BSpace", "BSpace", "=4", "Tab", "=2h", "Enter"]);
    assert_eq!(
        screen.next_sent().await,
        "zephyr.settings_set tasks.parallel 4 for 2h"
    );
    let changed = screen.wait_for("tasks.parallel 2 → 4").await;
    assert!(!changed.contains("› value"), "{changed}");
    // Waiting: the card in full, then `o` approves it.
    screen.keys(&["Left", "End"]);
    screen
        .wait_for("#12  /set budget.daily_cost_usd 8 for 2h")
        .await;
    screen.keys(&["Right", "Enter"]);
    let card = screen.wait_for("proposal #12").await;
    assert!(card.contains("waited 3m · expires in 23h 57m"), "{card}");
    screen.keys(&["o"]);
    assert_eq!(screen.next_sent().await, "zephyr.decide 12 ok");
    screen
        .wait_for("budget.daily_cost_usd 5 → 8 until 16:02")
        .await;
    // Esc closes the panel; the composer is the composer again.
    screen.keys(&["Escape"]);
    let closed = screen.wait_for("^L model").await;
    assert!(!closed.contains("╭─ settings"), "{closed}");
    screen.keys(&["=hi", "Enter"]);
    assert_eq!(screen.next_sent().await, "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn screens_match_the_goldens() {
    if tmux_missing() {
        return;
    }
    let update = std::env::var_os("ZEPHYR_UPDATE_SCREENS").is_some();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/screens");
    std::fs::create_dir_all(&dir).unwrap();
    let busy = ("esc to stop", &[][..], "esc to stop");
    let model = ("^L model", &["C-l"][..], "claude-sonnet-5");
    let status = ("^L model", &["C-s"][..], "╭─ status");
    let help = ("^L model", &["?"][..], "╭─ commands");
    let complete = ("^L model", &["=/"][..], "tab complete");
    let search = ("^L model", &["C-f", "=renew"][..], "3 of 3");
    let focus = ("^L model", &["=/focus", "Enter"][..], "focus ·");
    let rewind = ("^L model", &["Escape", "Escape"][..], "friday  discord");
    let settings = (
        "^L model",
        &[
            "=/settings",
            "Enter",
            "~Waiting (1)",
            "Down",
            "Down",
            "Right",
        ][..],
        "loosening it needs tui or chat",
    );
    let card = (
        "^L model",
        &[
            "=/settings",
            "Enter",
            "~Waiting (1)",
            "End",
            "~#12",
            "Right",
            "Enter",
        ][..],
        "proposal #12",
    );
    // (golden name, scene, size, theme, (ready, keys, shown))
    #[allow(clippy::type_complexity)]
    let shots: [(&str, &'static str, u16, u16, &str, (&str, &[&str], &str)); 29] = [
        ("busy", "busy", 80, 24, "dark", busy),
        ("busy", "busy", 80, 24, "light", busy),
        ("busy", "busy", 140, 40, "dark", busy),
        ("busy", "busy", 140, 40, "light", busy),
        (
            "retry",
            "retry",
            80,
            24,
            "dark",
            ("retry 2/10", &[], "retry 2/10"),
        ),
        (
            "paused",
            "paused",
            80,
            24,
            "dark",
            ("paused:", &[], "paused:"),
        ),
        (
            "down",
            "down",
            80,
            24,
            "dark",
            ("reconnecting", &[], "draft"),
        ),
        (
            "connecting",
            "connecting",
            80,
            24,
            "light",
            ("not answering", &[], "not answering"),
        ),
        ("busy", "busy", 80, 24, "ascii", busy),
        ("busy", "busy", 80, 24, "nocolor", busy),
        ("model", "idle", 80, 24, "dark", model),
        ("model", "idle", 80, 24, "light", model),
        ("model", "idle", 140, 40, "dark", model),
        ("status", "idle", 80, 24, "dark", status),
        ("status", "idle", 80, 24, "light", status),
        ("status", "idle", 140, 40, "dark", status),
        ("help", "idle", 80, 24, "dark", help),
        ("help", "idle", 80, 24, "light", help),
        ("help", "idle", 140, 40, "dark", help),
        ("complete", "idle", 80, 24, "dark", complete),
        ("search", "idle", 80, 24, "dark", search),
        ("focus", "idle", 80, 24, "dark", focus),
        (
            "files",
            "idle",
            80,
            24,
            "dark",
            ("^L model", &["=look at @"], "tab complete"),
        ),
        (
            "history",
            "idle",
            80,
            24,
            "dark",
            ("^L model", &["C-r"], "╭─ history"),
        ),
        ("rewind", "idle", 80, 24, "dark", rewind),
        (
            "model",
            "idle",
            80,
            24,
            "ascii",
            ("^L model", &["C-l"], "claude-sonnet-5"),
        ),
        ("status", "idle", 80, 24, "nocolor", status),
        ("settings", "waiting", 100, 30, "dark", settings),
        ("waiting", "waiting", 100, 30, "dark", card),
    ];
    let mut failures = vec![];
    for (name, scene, cols, rows, theme, steps) in shots {
        let (plain, escaped) = shoot(scene, cols, rows, theme, steps).await;
        // The palette follows COLORFGBG: the accent's exact RGB is in the escapes.
        match theme {
            "light" => assert!(
                escaped.contains("38;2;11;111;145"),
                "{scene}: no light accent"
            ),
            "nocolor" => assert!(!escaped.contains("38;2;"), "NO_COLOR still colours"),
            _ => assert!(
                escaped.contains("38;2;94;200;229"),
                "{scene} {theme}: no dark accent"
            ),
        }
        if theme == "ascii" {
            assert!(plain.is_ascii(), "non-ASCII under LANG=C:\n{plain}");
        }
        let name = format!("{name}-{cols}x{rows}-{theme}");
        let golden = dir.join(format!("{name}.txt"));
        if update {
            std::fs::write(&golden, &plain).unwrap();
            std::fs::write(dir.join(format!("{name}.ansi")), &escaped).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&golden).unwrap_or_default();
        if normalize(&want) != normalize(&plain) {
            failures.push(format!("{name}:\n{plain}"));
        }
    }
    assert!(
        failures.is_empty(),
        "screens differ (ZEPHYR_UPDATE_SCREENS=1 rewrites):\n{}",
        failures.join("\n")
    );
}
