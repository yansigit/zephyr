//! Lets stock `rpi --connect 127.0.0.1:<port>` attach to the live `main` session (DESIGN §4.3).
//!
//! Wire shape, from upstream `crates/rpi-cli/src/remote/client.rs` and `session_driver.rs`
//! (the same from v0.12.0 to v0.18.0, which only adds optional event fields): one JSON-RPC 2.0
//! object per line. The client calls `authenticate {token}` (only when it has a token),
//! `start_session {}`, `subscribe
//! {sessionId}`, then a `send {sessionId, command:{type:"get_state"}}`, one `send
//! {sessionId, command}` per prompt (the text is the command's `content`) and `stop_session` on
//! exit, and renders `{"method":"event","params":{"subscriptionId","event":<--mode rpc line>}}`.
//! Here prompts go through the inbox as channel `tui`, and `stop_session` only detaches.
//! `zephyr.hello`, `zephyr.subscribe` and `zephyr.models` are Zephyr's own additions
//! (DESIGN §4.3.1), as are `zephyr.login_providers`/`_start`/`_poll`/`_input`/`_cancel`, which
//! sign a provider in on the live gateway (`crate::auth`), and `zephyr.vault_list`/`_put`/`_rm`/
//! `_revoke`, the owner's credential vault (`crate::vault`), `zephyr.sessions`, the saved main
//! sessions with their `/rename` names, and `zephyr.turns`, the exchanges `/undo N` counts, newest
//! first, for the TUI's rewind picker. `zephyr.login_start` answers once rpi's
//! child has started, not at once: rpi may be asked for a model first, so the `loginId` can take a
//! few seconds, and the sign-in URL follows on the daemon's 5 s tick rather than in the reply.

use crate::channel::Inbound;
use crate::config::{Attach, Config};
use crate::router::{untag, Snapshot};
use crate::rpc::{message_text, MAIN_SESSION};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

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
    snap: watch::Receiver<Snapshot>,
    cfgs: watch::Receiver<Arc<Config>>,
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
                    snap: snap.clone(),
                    cfgs: cfgs.clone(),
                    inbound: inbound.clone(),
                    router: router.clone(),
                    authed: false,
                    live: None,
                    zephyr: false,
                    models: None,
                    late: mpsc::channel(4),
                };
                tokio::spawn(conn.run(stream));
            }
            Err(e) => eprintln!("zephyr: attach accept failed: {e}"),
        }
    }
}

struct Conn {
    token: Arc<str>,
    snap: watch::Receiver<Snapshot>,
    /// The live config, for `zephyr.models`.
    cfgs: watch::Receiver<Arc<Config>>,
    inbound: mpsc::Sender<Inbound>,
    router: mpsc::Sender<AttachRequest>,
    authed: bool,
    live: Option<broadcast::Receiver<Value>>,
    /// Subscribed with `zephyr.subscribe`: raw events plus `zephyr_status`.
    zephyr: bool,
    /// A `zephyr.models` answer being built: `(id, result)`. It runs rpi once per account, so
    /// it is off the loop, which keeps streaming meanwhile.
    models: Option<tokio::task::JoinHandle<(Value, Value)>>,
    /// Answers from work this conn spawned and cannot wait on in the loop — `zephyr.login_start`
    /// asks rpi for its models — as frames to write. The sender lives with the conn, so a task
    /// may outlive one reply without ending the connection.
    late: (mpsc::Sender<Reply>, mpsc::Receiver<Reply>),
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
                    Ok(ev) if self.zephyr => (raw(ev).into_iter().map(event).collect(), false),
                    Ok(ev) => (live(ev), false),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("zephyr: attach client lagged, {n} events skipped");
                        (vec![], false)
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                done = async { self.models.as_mut().expect("pending").await }, if self.models.is_some() => {
                    self.models = None;
                    match done {
                        Ok((id, result)) => (vec![ok(&id, result)], false),
                        Err(_) => (vec![], false), // a panic in the listing; the client asks again
                    }
                }
                reply = self.late.1.recv() => match reply {
                    Some(reply) => reply,
                    None => return, // unreachable: this conn holds the sender
                },
                changed = self.snap.changed(), if self.zephyr => match changed {
                    Ok(()) => (vec![self.status()], false),
                    Err(_) => return, // the router is gone
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
            "zephyr.subscribe" => self.zephyr_subscribe(id).await,
            "zephyr.hello" => {
                let snap = serde_json::to_value(&*self.snap.borrow()).unwrap_or_default();
                (vec![ok(id, snap)], false)
            }
            "zephyr.models" => {
                let (cfg, id) = (self.cfgs.borrow().clone(), id.clone());
                // A newer ask replaces one still pending.
                let task = tokio::spawn(async move { (id, crate::router::models(&cfg).await) });
                if let Some(old) = self.models.replace(task) {
                    old.abort();
                }
                (vec![], false)
            }
            "send" => self.send(id, &params["command"]).await,
            "zephyr.login_providers" => (vec![ok(id, crate::auth::providers())], false),
            "zephyr.login_start" => {
                let (cfg, provider) = (
                    self.cfgs.borrow().clone(),
                    params["provider"].as_str().unwrap_or_default().to_string(),
                );
                let (account, model) = (
                    params["account"].as_str().map(str::to_string),
                    params["model"].as_str().map(str::to_string),
                );
                let (asked, late) = (json!(id), self.late.0.clone());
                // Loading the config and asking rpi for its models is not quick, so this loop —
                // the one that streams events — does not wait: the answer goes back over `late`,
                // as `zephyr.models` does, once the login's child has started. A provider
                // `auth::start` does not know still answers `-32602` here.
                tokio::spawn(async move {
                    let started = crate::auth::start(&cfg, &provider, account, model).await;
                    let (reply, login) = match started {
                        Ok(start) => (
                            (vec![ok(&asked, start.to_json())], false),
                            Some(start.login_id),
                        ),
                        Err(e) => (
                            (vec![error(&asked, -32602, &format!("{e:#}"))], false),
                            None,
                        ),
                    };
                    // Nobody left to answer: the connection is gone. No client ever learned this
                    // login's id, so it could never be polled, typed into or cancelled, and a
                    // live `script` and rpi would sit in `waiting` for the daemon's life. Stop
                    // it there; the cancel records nothing and logs nothing about credentials.
                    // A client that walks away from a login without closing the connection is
                    // not this code's business: it can still cancel by id.
                    if late.send(reply).await.is_err() {
                        if let Some(login) = login {
                            crate::auth::cancel(&login);
                        }
                    }
                });
                (vec![], false)
            }
            "zephyr.login_poll" => {
                match crate::auth::poll(params["loginId"].as_str().unwrap_or_default()) {
                    Some(login) => (vec![ok(id, login)], false),
                    None => (vec![error(id, -32602, "unknown login")], false),
                }
            }
            "zephyr.login_input" => {
                let (login, text) = (
                    params["loginId"].as_str().unwrap_or_default(),
                    params["text"].as_str().unwrap_or_default(),
                );
                if !crate::auth::input(login, text) {
                    return (vec![error(id, -32602, "unknown login")], false);
                }
                (vec![ok(id, json!({}))], false)
            }
            "zephyr.login_cancel" => {
                // `false`: already finished, or evicted — the client asked to cancel nothing.
                if crate::auth::cancel(params["loginId"].as_str().unwrap_or_default()) {
                    (vec![ok(id, json!({}))], false)
                } else {
                    (vec![error(id, -32602, "unknown login")], false)
                }
            }
            "zephyr.vault_list"
            | "zephyr.vault_put"
            | "zephyr.vault_rm"
            | "zephyr.vault_revoke" => self.vault(id, method, params).await,
            "zephyr.sessions" => {
                let cfg = self.cfgs.borrow().clone();
                let names = (crate::store::Store::open(&cfg.db_path()))
                    .and_then(|s| s.session_names())
                    .unwrap_or_default();
                let sessions: Vec<Value> = (crate::rpc::saved_sessions(&cfg.session_dir()))
                    .into_iter()
                    .map(|(s, at)| json!({"name": names.get(&s), "id": s, "at": at}))
                    .collect();
                (vec![ok(id, json!({ "sessions": sessions }))], false)
            }
            "zephyr.turns" => {
                // The router's own `get_messages`, so the picker counts what `/undo N` will.
                let (tx, rx) = oneshot::channel();
                let history = match self.router.send(tx).await {
                    Ok(()) => rx.await.map(|(history, _)| history).unwrap_or_default(),
                    Err(_) => Value::Null,
                };
                let messages = history["messages"]
                    .as_array()
                    .map_or(&[][..], Vec::as_slice);
                let turns: Vec<Value> = (crate::router::turns(messages).into_iter().rev())
                    .filter_map(|i| {
                        let text = message_text(&messages[i]);
                        let (channel, _, body) = untag(&text)?;
                        Some(json!({"channel": channel, "text": body}))
                    })
                    .collect();
                (vec![ok(id, json!({ "turns": turns }))], false)
            }
            _ => (vec![error(id, -32601, "method not found")], false),
        }
    }

    /// The owner's vault (DESIGN §4.15): every call answers with the entries as they are now,
    /// never a value. `vault_put` takes the values, and they go nowhere but `vault.toml`: not into
    /// an answer, a log or an error. A new entry is told to the agent as a message of its own, so
    /// it can carry on with what it asked for.
    async fn vault(&mut self, id: &Value, method: &str, params: &Value) -> Reply {
        use crate::vault::{edit, Entry, Vault};
        let path = crate::vault::path(&self.cfgs.borrow());
        let name = params["name"].as_str().unwrap_or_default();
        let done: anyhow::Result<Option<Entry>> = match method {
            "zephyr.vault_put" => match serde_json::from_value::<Entry>(params.clone()) {
                // Not serde's message: it can quote the value it did not like.
                Err(_) => Err(anyhow::anyhow!("the entry is malformed")),
                Ok(entry) => edit(&path, |v| v.put(entry.clone())).map(|new| new.then_some(entry)),
            },
            "zephyr.vault_rm" => edit(&path, |v| match v.remove(name) {
                true => Ok(None),
                false => bail!("no entry named {name} in the vault"),
            }),
            "zephyr.vault_revoke" => edit(&path, |v| {
                v.revoke(name, params["program"].as_str()).map(|_| None)
            }),
            _ => Ok(None),
        };
        let listed = done.and_then(|added| Ok((Vault::load(&path)?, added)));
        let (vault, added) = match listed {
            Ok(v) => v,
            Err(e) => return (vec![error(id, -32602, &format!("{e:#}"))], false),
        };
        if let Some(e) = added {
            let kind = serde_json::to_value(e.kind).unwrap_or_default();
            let text = format!(
                "[zephyr] the owner saved the {} {} in the vault; you can use it now",
                kind.as_str().unwrap_or_default(),
                e.name
            );
            let m = Inbound {
                channel: CHANNEL.into(),
                platform_msg_id: next_msg_id(),
                sender: "token".into(),
                text,
            };
            let _ = self.inbound.send(m).await;
        }
        let entries: Vec<Value> = vault.entries().iter().map(Entry::public).collect();
        (vec![ok(id, json!({ "entries": entries }))], false)
    }

    /// Replays the session from `get_messages`, then streams live events.
    async fn subscribe(&mut self, id: &Value) -> Reply {
        let history = match self.history().await {
            Ok(history) => history,
            Err(e) => return (vec![error(id, -32000, e)], true),
        };
        let result =
            json!({"subscriptionId": SUB_ID, "sessionId": MAIN_SESSION, "status": "subscribed"});
        let mut frames = vec![ok(id, result), event(json!({"type": "ready"}))];
        for m in history["messages"].as_array().into_iter().flatten() {
            frames.extend(shown(m).into_iter().map(event));
        }
        (frames, false)
    }

    /// Like `subscribe`, but the history is raw and comes in the result, and live events are
    /// passed on unquoted, with `zephyr_status` on every snapshot change.
    async fn zephyr_subscribe(&mut self, id: &Value) -> Reply {
        let mut history = match self.history().await {
            Ok(history) => history,
            Err(e) => return (vec![error(id, -32000, e)], true),
        };
        self.zephyr = true;
        // `[]` when the router could not read the history.
        let mut messages = (history.get_mut("messages").map(Value::take)).unwrap_or(json!([]));
        messages
            .as_array_mut()
            .into_iter()
            .flatten()
            .for_each(annotate);
        let result = json!({"subscriptionId": SUB_ID, "sessionId": MAIN_SESSION, "status": "subscribed", "messages": messages});
        let frames = vec![
            ok(id, result),
            event(json!({"type": "ready"})),
            self.status(),
        ];
        (frames, false)
    }

    /// The `get_messages` result, and the live stream from that point on.
    async fn history(&mut self) -> Result<Value, &'static str> {
        let (tx, rx) = oneshot::channel();
        self.router
            .send(tx)
            .await
            .map_err(|_| "gateway is shutting down")?;
        let (history, live) = rx.await.map_err(|_| "gateway is shutting down")?;
        self.live = Some(live);
        Ok(history)
    }

    /// A `zephyr_status` event with the current snapshot, marked seen.
    fn status(&mut self) -> Value {
        let snap = serde_json::to_value(&*self.snap.borrow_and_update()).unwrap_or_default();
        event(json!({"type": "zephyr_status", "snapshot": snap}))
    }

    /// Maps the client's `--mode rpc` command onto the gateway.
    async fn send(&mut self, id: &Value, cmd: &Value) -> Reply {
        let text = match cmd["type"].as_str() {
            Some("prompt") => cmd["content"].as_str().unwrap_or_default(),
            Some("abort") => "/stop",
            Some("get_state") => {
                let (model, thinking) = {
                    let s = self.snap.borrow();
                    (s.model.clone(), s.thinking.clone())
                };
                let state = json!({"sessionId": MAIN_SESSION, "model": model, "thinkingLevel": thinking, "activeTools": []});
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
    if ev["type"] == "zephyr_reply" {
        let m = json!({"role": "assistant", "content": [{"type": "text", "text": ev["text"]}]});
        return shown(&m).into_iter().map(event).collect();
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

/// A live child event for a `zephyr.subscribe` client: unchanged but annotated, minus the
/// answers to the router's own commands.
fn raw(mut ev: Value) -> Option<Value> {
    if ev["type"] == "response" {
        return None;
    }
    if let Some(m) = ev.get_mut("message") {
        annotate(m);
    }
    Some(ev)
}

/// Adds `origin: {channel, id}` (null when untagged) and the untagged `text` to a user message.
fn annotate(m: &mut Value) {
    if role(m) != Some("user") {
        return;
    }
    let text = message_text(m);
    let (origin, body) = match untag(&text) {
        Some((channel, id, body)) => (json!({"channel": channel, "id": id}), body),
        None => (Value::Null, text.as_str()),
    };
    let body = body.to_string();
    if let Some(m) = m.as_object_mut() {
        m.insert("origin".into(), origin);
        m.insert("text".into(), body.into());
    }
}

/// `message_start`/`message_end` for a session message as the stock client draws it. It
/// renders assistant messages only, so a user turn becomes an assistant block `**channel** · text`.
fn shown(m: &Value) -> Vec<Value> {
    let m = match role(m) {
        Some("assistant") => m.clone(),
        Some("user") => {
            let text = message_text(m);
            let quoted = match untag(&text) {
                Some((channel, _, body)) => format!("**{channel}** · {body}"),
                None => format!("> {text}"),
            };
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
