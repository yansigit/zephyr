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
        assistant(
            T + 182_000,
            json!([
            {"type": "text", "text": "Three files: the image recipe, the launcher, and a one-time cleanup script."}]),
        ),
    ]
}

fn snapshot(scene: &str) -> Value {
    let now = zephyr::unix_millis() as i64;
    let (busy, paused, cost, failed) = match scene {
        "paused" => (false, json!("user"), 4.2, 1),
        "down" => (false, Value::Null, 0.38, 0),
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
        "thinking": "medium", "paused": paused,
        "channels": [{"name": "discord", "state": "connected"}, {"name": "imessage", "state": "connected"},
                     {"name": "chat", "state": "off"}],
        "spend": {"tokens": 41_200, "cost": cost, "daily_tokens_limit": null, "daily_cost_limit": 5.0},
        "queue": if busy { json!([{"channel": "discord", "preview": "are you around later?", "since_unix": now / 1000 - 30}]) } else { json!([]) },
        "failed_replies": failed,
        "run": run,
    })
}

/// The live events after `ready` and the first status.
fn live(scene: &str) -> Vec<Value> {
    let mut events = vec![json!({"type": "zephyr_reply", "command": "/status",
        "text": "agent: claude-code-1 on claude-code/claude-opus-5-5\nchannels: discord connected, imessage connected\ntoday: 41.2k tokens, $0.38 of $5"})];
    if scene == "paused" || scene == "down" {
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
        .map(|l| {
            let live = ["esc to stop", "retry ", "● ", "✗ "];
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
        // Each screen its own dir (config, token) and tmux server; the name is part of a
        // unix socket path, so it stays short.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "zt-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, sent) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(listener, scene, tx));
        if scene == "connecting" {
            // Let the listener close, so the port refuses.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let cfg = config(&dir, port);
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
        Screen {
            tmux,
            dir,
            server,
            sent,
        }
    }

    /// The plain screen once it shows `text`.
    async fn wait_for(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let screen = self.tmux.capture(false);
            if screen.contains(text) {
                return screen;
            }
            assert!(
                !screen.contains("zephyr exited"),
                "zephyr tui exited:\n{screen}"
            );
            assert!(
                Instant::now() < deadline,
                "no {text:?} on screen:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
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

/// Starts `zephyr tui` for `scene` and returns the plain and escaped screens once `ready` shows.
async fn shoot(
    scene: &'static str,
    cols: u16,
    rows: u16,
    theme: &str,
    ready: &str,
) -> (String, String) {
    let screen = Screen::start(scene, cols, rows, theme).await;
    screen.wait_for(ready).await;
    if scene == "down" {
        // A draft typed while away stays in the box.
        screen.keys(&["=half-typed draft"]);
    }
    // One more frame for anything that arrived with the marker.
    tokio::time::sleep(Duration::from_millis(400)).await;
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
async fn screens_match_the_goldens() {
    if tmux_missing() {
        return;
    }
    let update = std::env::var_os("ZEPHYR_UPDATE_SCREENS").is_some();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/screens");
    std::fs::create_dir_all(&dir).unwrap();
    let shots: [(&'static str, u16, u16, &str, &str); 10] = [
        ("busy", 80, 24, "dark", "esc to stop"),
        ("busy", 80, 24, "light", "esc to stop"),
        ("busy", 140, 40, "dark", "esc to stop"),
        ("busy", 140, 40, "light", "esc to stop"),
        ("retry", 80, 24, "dark", "retry 2/10"),
        ("paused", 80, 24, "dark", "paused:"),
        ("down", 80, 24, "dark", "reconnecting"),
        ("connecting", 80, 24, "light", "not answering"),
        ("busy", 80, 24, "ascii", "esc to stop"),
        ("busy", 80, 24, "nocolor", "esc to stop"),
    ];
    let mut failures = vec![];
    for (scene, cols, rows, theme, ready) in shots {
        let (plain, escaped) = shoot(scene, cols, rows, theme, ready).await;
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
        let name = format!("{scene}-{cols}x{rows}-{theme}");
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
