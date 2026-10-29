//! Lets stock `rpi --connect 127.0.0.1:<port>` attach to the live `main` session (DESIGN §4.3).
//!
//! Wire shape, from upstream `crates/rpi-cli/src/remote/client.rs` and `session_driver.rs`
//! (identical in v0.12.0 and main): one JSON-RPC 2.0 object per line. The client calls
//! `authenticate {token}` (only when it has a token), `start_session {}`, `subscribe
//! {sessionId}`, then `send {sessionId, command}` per prompt and `stop_session` on exit, and
//! renders `{"method":"event","params":{"subscriptionId","event":<--mode rpc line>}}`.
//! Here prompts go through the inbox as channel `tui`, and `stop_session` only detaches.

use crate::channel::Inbound;
use crate::config::{Attach, Config};
use crate::rpc::{message_text, MAIN_SESSION};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};

/// The inbox channel name for messages typed in an attached client.
pub const CHANNEL: &str = "tui";
/// One subscription per connection, so the id is fixed.
const SUB_ID: u64 = 1;
const MAX_LINE: u64 = 1 << 20;
const UNAUTHORIZED: i64 = -32001;

/// Asks the router for the session history plus a receiver for every child event after it.
pub type AttachRequest = oneshot::Sender<(Value, broadcast::Receiver<Value>)>;

/// Reads the attach token. Never log the result.
pub fn token(a: &Attach) -> anyhow::Result<String> {
    let token = match (&a.token_env, &a.token_file) {
        (Some(var), None) => std::env::var(var).with_context(|| format!("${var} is not set"))?,
        (None, Some(path)) => {
            let mode = std::fs::metadata(path)
                .with_context(|| format!("reading {}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                bail!("{} must be mode 0600", path.display());
            }
            std::fs::read_to_string(path)?
        }
        _ => bail!("[attach] needs exactly one of token_env or token_file"),
    };
    let token = token.trim();
    if token.is_empty() {
        bail!("the attach token is empty");
    }
    Ok(token.to_string())
}

/// Serves attach clients until dropped. Pending forever when `[attach]` is not configured.
pub async fn serve(
    cfg: &Config,
    inbound: mpsc::Sender<Inbound>,
    router: mpsc::Sender<AttachRequest>,
) -> anyhow::Result<()> {
    let Some(a) = &cfg.attach else {
        return std::future::pending().await;
    };
    let token: Arc<str> = token(a).context("attach token")?.into();
    let listener = TcpListener::bind(("127.0.0.1", a.port))
        .await
        .with_context(|| format!("binding attach port {}", a.port))?;
    eprintln!("zephyr: attach listening on {}", listener.local_addr()?);
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let conn = Conn {
                    token: token.clone(),
                    model: cfg.model.clone(),
                    inbound: inbound.clone(),
                    router: router.clone(),
                    authed: false,
                    live: None,
                };
                tokio::spawn(conn.run(stream));
            }
            Err(e) => eprintln!("zephyr: attach accept failed: {e}"),
        }
    }
}

struct Conn {
    token: Arc<str>,
    model: Option<String>,
    inbound: mpsc::Sender<Inbound>,
    router: mpsc::Sender<AttachRequest>,
    authed: bool,
    live: Option<broadcast::Receiver<Value>>,
}

/// What to do after a request: write these frames, then maybe hang up.
type Reply = (Vec<Value>, bool);

impl Conn {
    async fn run(mut self, stream: TcpStream) {
        let (read, mut write) = stream.into_split();
        let (tx, mut lines) = mpsc::channel(16);
        tokio::spawn(read_lines(read, tx));
        loop {
            let (frames, close) = tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => self.request(&line).await,
                    None => return, // client gone; the run carries on
                },
                ev = next_live(&mut self.live) => match ev {
                    Ok(ev) => (live(ev), false),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("zephyr: attach client lagged, {n} events skipped");
                        (vec![], false)
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            };
            for frame in frames {
                if write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if close {
                return;
            }
        }
    }

    async fn request(&mut self, line: &[u8]) -> Reply {
        let Ok(req) = serde_json::from_slice::<Value>(line) else {
            return (vec![error(&Value::Null, -32700, "parse error")], false);
        };
        let id = &req["id"];
        let params = &req["params"];
        let method = req["method"].as_str().unwrap_or_default();
        if method == "authenticate" {
            let given = params["token"].as_str().unwrap_or_default();
            if !same(given, &self.token) {
                return (vec![error(id, UNAUTHORIZED, "invalid token")], true);
            }
            self.authed = true;
            return (vec![ok(id, json!({"authenticated": true}))], false);
        }
        if !self.authed {
            return (
                vec![error(id, UNAUTHORIZED, "authentication required")],
                false,
            );
        }
        let session = json!({"sessionId": MAIN_SESSION});
        match method {
            "start_session" => (vec![ok(id, session)], false),
            "list_sessions" => (vec![ok(id, json!({"sessions": [session]}))], false),
            "ping" => (vec![ok(id, json!({"type": "pong"}))], false),
            // The agent outlives every client: stopping only detaches.
            "stop_session" => (
                vec![ok(
                    id,
                    json!({"sessionId": MAIN_SESSION, "status": "detached"}),
                )],
                false,
            ),
            "subscribe" => self.subscribe(id).await,
            "send" => self.send(id, &params["command"]).await,
            _ => (vec![error(id, -32601, "method not found")], false),
        }
    }

    /// Replays the session from `get_messages`, then streams live events.
    async fn subscribe(&mut self, id: &Value) -> Reply {
        let (tx, rx) = oneshot::channel();
        if self.router.send(tx).await.is_err() {
            return (vec![error(id, -32000, "gateway is shutting down")], true);
        }
        let Ok((history, live)) = rx.await else {
            return (vec![error(id, -32000, "gateway is shutting down")], true);
        };
        self.live = Some(live);
        let result =
            json!({"subscriptionId": SUB_ID, "sessionId": MAIN_SESSION, "status": "subscribed"});
        let mut frames = vec![ok(id, result), event(json!({"type": "ready"}))];
        for m in history["messages"].as_array().into_iter().flatten() {
            frames.extend(shown(m).into_iter().map(event));
        }
        (frames, false)
    }

    /// Maps the client's `--mode rpc` command onto the gateway.
    async fn send(&mut self, id: &Value, cmd: &Value) -> Reply {
        let text = match cmd["type"].as_str() {
            Some("prompt") => cmd["content"].as_str().unwrap_or_default(),
            Some("abort") => "/stop",
            Some("get_state") => {
                let state =
                    json!({"sessionId": MAIN_SESSION, "model": self.model, "activeTools": []});
                return (
                    vec![ok(id, json!({})), event(response(cmd, Ok(state)))],
                    false,
                );
            }
            Some("ping") | Some("stop") => return (vec![ok(id, json!({}))], false),
            other => {
                let e = format!(
                    "{} is not available when attached to zephyr",
                    other.unwrap_or("?")
                );
                return (vec![ok(id, json!({})), event(response(cmd, Err(e)))], false);
            }
        };
        let m = Inbound {
            channel: CHANNEL.into(),
            platform_msg_id: next_msg_id(),
            sender: "token".into(),
            text: text.into(),
        };
        if self.inbound.send(m).await.is_err() {
            return (vec![error(id, -32000, "gateway is shutting down")], true);
        }
        (
            vec![ok(id, json!({"sessionId": MAIN_SESSION, "status": "sent"}))],
            false,
        )
    }
}

async fn next_live(
    live: &mut Option<broadcast::Receiver<Value>>,
) -> Result<Value, broadcast::error::RecvError> {
    match live {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Frames for one live child event.
fn live(ev: Value) -> Vec<Value> {
    let m = &ev["message"];
    if ev["type"] == "response" {
        return vec![]; // answers to the router's own commands
    }
    if role(m) != Some("user") {
        return vec![event(ev)];
    }
    // The client already drew its own prompt; other channels' turns are quoted once, at the end.
    let own = message_text(m).starts_with(&format!("[{CHANNEL} #"));
    if ev["type"] != "message_end" || own {
        return vec![];
    }
    shown(m).into_iter().map(event).collect()
}

/// `message_start`/`message_end` for a session message as the stock client draws it. It
/// renders assistant messages only, so a user turn becomes a quoted assistant block.
fn shown(m: &Value) -> Vec<Value> {
    let m = match role(m) {
        Some("assistant") => m.clone(),
        Some("user") => {
            let quoted = format!("> {}", message_text(m));
            json!({"role": "assistant", "content": [{"type": "text", "text": quoted}]})
        }
        _ => return vec![],
    };
    vec![
        json!({"type": "message_start", "message": m}),
        json!({"type": "message_end", "message": m}),
    ]
}

fn role(m: &Value) -> Option<&str> {
    m["kind"].as_str().or(m["role"].as_str())
}

/// Reads newline-delimited requests; a line over `MAX_LINE` ends the connection.
async fn read_lines(read: OwnedReadHalf, tx: mpsc::Sender<Vec<u8>>) {
    let mut read = BufReader::new(read);
    loop {
        let mut line = Vec::new();
        match (&mut read)
            .take(MAX_LINE)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(n) if n as u64 == MAX_LINE && line.last() != Some(&b'\n') => return,
            Ok(_) if tx.send(line).await.is_err() => return,
            Ok(_) => {}
        }
    }
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn event(inner: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "event", "params": {"subscriptionId": SUB_ID, "event": inner}})
}

/// A `RemoteResponse` line for an inner command, as the client's session fold expects.
fn response(cmd: &Value, r: Result<Value, String>) -> Value {
    match r {
        Ok(result) => {
            json!({"type": "response", "id": cmd["id"], "status": "ok", "result": result})
        }
        Err(e) => json!({"type": "response", "id": cmd["id"], "status": "error", "error": e}),
    }
}

/// Unique across restarts, so the inbox never dedupes two different prompts.
fn next_msg_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        crate::unix_millis(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Constant-time compare, so the token can't be guessed byte by byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}
