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
//! Zephyr's TUI authenticates with `zephyr.challenge` and `zephyr.prove` instead, so it never
//! sends the token (DESIGN §4.3.1).
//! `zephyr.hello`, `zephyr.subscribe` and `zephyr.models` are Zephyr's own additions
//! (DESIGN §4.3.1), as are `zephyr.login_providers`/`_start`/`_poll`/`_input`/`_cancel`, which
//! sign a provider in on the live gateway (`crate::auth`), and `zephyr.vault_list`/`_put`/`_rm`/
//! `_revoke`, the owner's credential vault (`crate::vault`), `zephyr.facts_list`/`_put`/`_rm`, the
//! owner's private facts (`crate::facts`), `zephyr.sessions`, the saved main
//! sessions with their `/rename` names, and `zephyr.turns`, the exchanges `/undo N` counts, newest
//! first, for the TUI's rewind picker, and `zephyr.dequeue`, which takes back the `tui` messages
//! still waiting (a `send` prompt with `followUp: true` waits for the settle), and
//! `zephyr.activity`, the daemon's activity record paged by cursor or its standing grants
//! (`crate::activity`). `zephyr.login_start` answers once rpi's
//! child has started, not at once: rpi may be asked for a model first, so the `loginId` can take a
//! few seconds, and the sign-in URL follows on the daemon's 5 s tick rather than in the reply.

use crate::channel::Inbound;
use crate::config::{Attach, Config};
use crate::router::{untag, Snapshot};
use crate::rpc::{message_text, MAIN_SESSION};
use crate::web::passkey::{self, Signed};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// The inbox channel name for messages typed in an attached client.
pub const CHANNEL: &str = "tui";
/// The inbox channel name for messages typed in the web app (DESIGN §4.3.3).
pub const WEB: &str = "web";

/// Whether `channel` is a client on one of the daemon's own ports (the TUI's or the web app's),
/// which reads the router's replies from the event stream rather than from an adapter.
pub fn attached(channel: &str) -> bool {
    channel == CHANNEL || channel == WEB
}

/// The methods open to the web app. The rest answer `-32601`: sign-ins, the vault's and the
/// facts' values and the saved sessions stay on the TUI.
pub const WEB_METHODS: &[&str] = &[
    "zephyr.hello",
    "zephyr.subscribe",
    "send",
    "zephyr.models",
    "zephyr.dequeue",
    "zephyr.turns",
    "zephyr.settings",
    "zephyr.settings_set",
    "zephyr.proposals",
    "zephyr.decide",
    "zephyr.facts_list",
    "zephyr.vault_list",
    "zephyr.activity",
    "zephyr.goals",
    "ping",
    "zephyr.web_challenge",
    "zephyr.web_devices",
    "zephyr.desktop",
];
/// One subscription per connection, so the id is fixed.
const SUB_ID: u64 = 1;
pub(crate) const MAX_LINE: u64 = 1 << 20;
const UNAUTHORIZED: i64 = -32001;
/// Connections served at once; one more is closed as it arrives.
const MAX_CONNS: usize = 32;
/// How long a connection has to authenticate before it is closed.
const AUTH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What each side of `zephyr.challenge`/`zephyr.prove` signs before the two nonces (DESIGN
/// §4.3.1), so neither proof can stand for the other.
pub const BY_DAEMON: &[u8] = b"zephyr attach v1: the daemon\0";
pub const BY_CLIENT: &[u8] = b"zephyr attach v1: the client\0";
/// A nonce's length, in bytes.
pub const NONCE: usize = 32;

/// The client's nonce and the daemon's.
pub type Nonces<'a> = (&'a [u8], &'a [u8]);

/// A connection's two ends as each side sees them: the client's address and the daemon's.
/// The daemon reads them as its peer and its local address, the TUI as its local and its
/// peer, so a proof made on one connection never verifies on another (a relay's).
pub type Ends = (SocketAddr, SocketAddr);

/// What a proof signs: `by`, both nonces and both ends, an IPv4-mapped address as IPv4.
fn signed(by: &[u8], (client, daemon): Nonces, ends: Ends) -> Vec<u8> {
    let end = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    let ends = format!("{}\0{}", end(ends.0), end(ends.1));
    [by, client, daemon, ends.as_bytes()].concat()
}

/// HMAC-SHA256, keyed by the attach token, over `by`, both nonces and the connection's `ends`:
/// the proof that `by` holds the token on this connection, which never itself crosses it.
pub fn proof(token: &str, by: &[u8], nonces: Nonces, ends: Ends) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let tag = ring::hmac::sign(&key, &signed(by, nonces, ends));
    data_encoding::HEXLOWER.encode(tag.as_ref())
}

/// Whether `given` (hex) is [`proof`]'s, compared in constant time.
pub fn proven(token: &str, by: &[u8], nonces: Nonces, ends: Ends, given: &str) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let signed = signed(by, nonces, ends);
    let given = data_encoding::HEXLOWER.decode(given.as_bytes());
    given.is_ok_and(|g| ring::hmac::verify(&key, &signed, &g).is_ok())
}

/// A fresh nonce, and the hex a side sends of it.
pub fn nonce() -> ([u8; NONCE], String) {
    let mut n = [0; NONCE];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut n)
        .expect("the system's random source");
    (n, data_encoding::HEXLOWER.encode(&n))
}

/// What a connection asks of the router.
pub enum AttachRequest {
    /// The session history plus a receiver for every child event after it.
    History(oneshot::Sender<(Value, broadcast::Receiver<Value>)>),
    /// A prompt sent with `followUp: true`: held until the run settles (DESIGN §4.1).
    FollowUp(Inbound),
    /// `zephyr.dequeue`: the texts of this channel's messages still waiting, withdrawn.
    Dequeue {
        channel: &'static str,
        reply: oneshot::Sender<Vec<String>>,
    },
    /// `zephyr.settings`: every setting as it stands now.
    Settings(oneshot::Sender<Value>),
    /// `zephyr.settings_set`: a change by the owner on `channel`, answered as the outcome.
    /// `expect` is a passkey-signed change's: the value the owner saw, which must still stand.
    SettingsSet {
        key: String,
        value: String,
        until: Option<String>,
        channel: &'static str,
        expect: Option<Option<String>>,
        reply: oneshot::Sender<Value>,
    },
    /// `zephyr.proposals`: the proposals still waiting.
    Proposals(oneshot::Sender<Value>),
    /// A note of zephyr's own for main (the vault's new entry): an inbox row that is no owner's.
    Notice(Inbound),
    /// `zephyr.activity`: a page of the activity record, or the grants (DESIGN §4.19).
    Activity {
        query: crate::activity::Query,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// `zephyr.goals`: the open goals, then those done since the last review (read-only).
    Goals(oneshot::Sender<Result<Value, String>>),
    /// A waiting proposal's card as `zephyr.proposals` gives it now, and its digest
    /// (`Store::proposal_digest`): what a passkey is about to sign. `None` when none waits.
    Card {
        id: i64,
        reply: oneshot::Sender<Option<(String, [u8; 32])>>,
    },
    /// A setting's value now, as a signed change records it; `Err` for a key that is not one.
    Current {
        key: String,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// A notice from the web app's listener (a passkey was added, sign-in was closed), for
    /// every attached client and the owner last answered.
    WebNotice(String),
    /// `zephyr.decide`: `/ok N` (`ok`, `once`), `/no N` (`no`) or `/cancel N` (`cancel`) as the
    /// owner on `channel`. `expect` is a passkey-signed decision's: what was signed, which the
    /// proposal must still be.
    Decide {
        id: i64,
        verb: String,
        channel: &'static str,
        expect: Option<Expect>,
        reply: oneshot::Sender<Value>,
    },
}

/// What a passkey signature committed to: the proposal as stored (`Store::proposal_digest`) and
/// the hex SHA-256 of the card the owner was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    pub digest: [u8; 32],
    pub card_sha256: String,
}

/// Where a connection comes from: the attach port with its token, or the web app's socket,
/// whose cookie the listener already checked.
pub enum Origin {
    Tui {
        token: Arc<str>,
        /// The connection's ends, its peer's and ours, which the TUI's proofs are over.
        ends: Option<Ends>,
    },
    Web {
        session: [u8; 32],
        passkey: i64,
        hub: Arc<crate::web::Hub>,
    },
}

/// Serves one client on `stream` until it goes.
pub async fn serve_stream<S>(stream: S, origin: Origin, handles: Handles)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (token, ends, web) = match origin {
        Origin::Tui { token, ends } => (token, ends, None),
        Origin::Web {
            session,
            passkey,
            hub,
        } => ("".into(), None, Some((session, passkey, hub))),
    };
    let conn = Conn {
        token,
        ends,
        channel: if web.is_some() { WEB } else { CHANNEL },
        // The cookie was checked at the upgrade.
        authed: web.is_some(),
        challenge: None,
        web,
        snap: handles.snap,
        cfgs: handles.cfgs,
        inbound: handles.inbound,
        router: handles.router,
        live: None,
        zephyr: false,
        models: None,
        late: mpsc::channel(4),
        // shortcut: counted per socket, so a session's four sockets could ask for 40 a minute.
        signatures: crate::web::Window::new(10, std::time::Duration::from_secs(60)),
    };
    conn.run(stream).await;
}

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

/// What a listener for attached clients (this one, and the web app's) needs of the router.
#[derive(Clone)]
pub struct Handles {
    pub inbound: mpsc::Sender<Inbound>,
    pub router: mpsc::Sender<AttachRequest>,
    pub snap: watch::Receiver<Snapshot>,
    pub cfgs: watch::Receiver<Arc<Config>>,
}

/// Serves attach clients until dropped. Pending forever when `[attach]` is not configured.
pub async fn serve(cfg: &Config, handles: Handles) -> anyhow::Result<()> {
    let Some(a) = &cfg.attach else {
        return std::future::pending().await;
    };
    let token: Arc<str> = token(a).context("attach token")?.into();
    let addr = crate::egress::listen_addr(([127, 0, 0, 1], a.port).into())?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding attach port {}", a.port))?;
    eprintln!("zephyr: attach listening on {}", listener.local_addr()?);
    // Anything on the host can connect (the agent too, under host networking): a cap, so idle
    // sockets cannot use up the daemon's file descriptors, and `AUTH_WAIT` to authenticate.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue; // dropped, so closed at once
                };
                let origin = Origin::Tui {
                    token: token.clone(),
                    ends: stream.local_addr().ok().map(|ours| (peer, ours)),
                };
                let handles = handles.clone();
                tokio::spawn(async move {
                    serve_stream(stream, origin, handles).await;
                    drop(slot);
                });
            }
            Err(e) => eprintln!("zephyr: attach accept failed: {e}"),
        }
    }
}

struct Conn {
    token: Arc<str>,
    /// A TUI connection's ends ([`Ends`]); `None` for the web app's, which has no challenge.
    ends: Option<Ends>,
    /// The inbox channel this client's messages and decisions are on: `tui` or `web`.
    channel: &'static str,
    /// A web app's socket: its session's hash, its passkey's row and the listener's hub.
    web: Option<([u8; 32], i64, Arc<crate::web::Hub>)>,
    snap: watch::Receiver<Snapshot>,
    /// The live config, for `zephyr.models`.
    cfgs: watch::Receiver<Arc<Config>>,
    inbound: mpsc::Sender<Inbound>,
    router: mpsc::Sender<AttachRequest>,
    authed: bool,
    /// The nonces of a `zephyr.challenge` answered on this connection: the client's and ours.
    challenge: Option<([u8; NONCE], [u8; NONCE])>,
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
    /// `zephyr.web_challenge` calls this minute: at most 10.
    signatures: crate::web::Window,
}

const OTHER_REQUEST: &str = "that signature is for another request";
const NOT_VERIFIED: &str = "the passkey signature did not verify";

/// A setting's value as a client sends it: text, or a number or a flag as such.
fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        v @ (Value::Number(_) | Value::Bool(_)) => Some(v.to_string()),
        _ => None,
    }
}

/// A change's time limit. A number would read as "no limit": refused rather than loosen for good.
fn until_text(v: &Value) -> Result<Option<String>, &'static str> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err("until must be text like 30m, 2h or 1d"),
    }
}

/// What to do after a request: write these frames, then maybe hang up.
type Reply = (Vec<Value>, bool);

impl Conn {
    async fn run<S: AsyncRead + AsyncWrite + Send + 'static>(self, stream: S) {
        let (read, write) = tokio::io::split(stream);
        let (tx, lines) = mpsc::channel(16);
        let reader = tokio::spawn(read_lines(read, tx));
        self.talk(write, lines).await;
        reader.abort(); // its half holds the socket open until the client sends again
    }

    async fn talk(
        mut self,
        mut write: impl AsyncWrite + Unpin,
        mut lines: mpsc::Receiver<Vec<u8>>,
    ) {
        let deadline = tokio::time::Instant::now() + AUTH_WAIT;
        loop {
            let (frames, close) = tokio::select! {
                () = tokio::time::sleep_until(deadline), if !self.authed => return,
                line = lines.recv() => match line {
                    Some(line) => self.request(&line).await,
                    None => return, // client gone; the run carries on
                },
                ev = next_live(&mut self.live) => match ev {
                    // A command's reply is drawn where it was typed; a notice (no channel) everywhere.
                    Ok(ev) if ev["type"] == "zephyr_reply"
                        && ev["channel"].as_str().is_some_and(|c| c != self.channel) => (vec![], false),
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
        if self.channel == WEB && !WEB_METHODS.contains(&method) {
            return (
                vec![error(id, -32601, "not available in the web app")],
                false,
            );
        }
        if method == "authenticate" {
            let given = params["token"].as_str().unwrap_or_default();
            // Constant time, so the token can't be guessed byte by byte.
            if !bool::from(given.as_bytes().ct_eq(self.token.as_bytes())) {
                return (vec![error(id, UNAUTHORIZED, "invalid token")], true);
            }
            self.authed = true;
            return (vec![ok(id, json!({"authenticated": true}))], false);
        }
        // The TUI's way in (DESIGN §4.3.1): the daemon proves it holds the token first, over
        // the client's nonce and its own, and the client then proves it does; the token never
        // crosses the socket, so a program holding the port while the daemon is down learns
        // nothing from a TUI that connects to it.
        if method == "zephyr.challenge" {
            let client = params["nonce"].as_str().unwrap_or_default();
            let client = data_encoding::HEXLOWER.decode(client.as_bytes());
            let Some(client) = client.ok().and_then(|c| <[u8; NONCE]>::try_from(c).ok()) else {
                return (
                    vec![error(id, -32602, "nonce must be 32 bytes of hex")],
                    true,
                );
            };
            let Some(ends) = self.ends else {
                return (vec![error(id, UNAUTHORIZED, "no attach token here")], true);
            };
            let (daemon, hex) = nonce();
            self.challenge = Some((client, daemon));
            let proof = proof(&self.token, BY_DAEMON, (&client, &daemon), ends);
            return (vec![ok(id, json!({"nonce": hex, "proof": proof}))], false);
        }
        if method == "zephyr.prove" {
            let given = params["proof"].as_str().unwrap_or_default();
            let proven = (self.challenge.take().zip(self.ends))
                .is_some_and(|((c, d), e)| proven(&self.token, BY_CLIENT, (&c, &d), e, given));
            if !proven {
                return (vec![error(id, UNAUTHORIZED, "invalid proof")], true);
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
            "zephyr.dequeue" => {
                let (tx, rx) = oneshot::channel();
                let req = AttachRequest::Dequeue {
                    channel: self.channel,
                    reply: tx,
                };
                let asked = self.router.send(req).await;
                match (asked, rx.await) {
                    (Ok(()), Ok(texts)) => (vec![ok(id, json!({ "texts": texts }))], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.settings" | "zephyr.proposals" => {
                let (tx, rx) = oneshot::channel();
                let req = if method == "zephyr.settings" {
                    AttachRequest::Settings(tx)
                } else {
                    AttachRequest::Proposals(tx)
                };
                match (self.router.send(req).await, rx.await) {
                    (Ok(()), Ok(value)) => (vec![ok(id, value)], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.activity" => {
                let query = match crate::activity::Query::params(params) {
                    Ok(q) => q,
                    Err(why) => return (vec![error(id, -32602, &why)], false),
                };
                let (tx, rx) = oneshot::channel();
                let req = AttachRequest::Activity { query, reply: tx };
                match (self.router.send(req).await, rx.await) {
                    (Ok(()), Ok(Ok(value))) => (vec![ok(id, value)], false),
                    (Ok(()), Ok(Err(why))) => (vec![error(id, -32000, &why)], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.goals" => {
                let (tx, rx) = oneshot::channel();
                match (self.router.send(AttachRequest::Goals(tx)).await, rx.await) {
                    (Ok(()), Ok(Ok(value))) => (vec![ok(id, value)], false),
                    (Ok(()), Ok(Err(why))) => (vec![error(id, -32000, &why)], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.web_challenge" if self.web.is_some() => self.web_challenge(id, params).await,
            "zephyr.settings_set" => {
                // A client may send a number or a flag as such.
                let (Some(key), Some(value)) =
                    (params["key"].as_str(), value_text(&params["value"]))
                else {
                    return (vec![error(id, -32602, "key and value are required")], false);
                };
                let until = match until_text(&params["until"]) {
                    Ok(until) => until,
                    Err(why) => return (vec![error(id, -32602, why)], false),
                };
                // The web app signs a change to a strong setting: the signature must be for
                // this change, and then acts as `passkey`. Unsigned, the change is `web`'s.
                let mut by = (self.channel, None);
                if self.web.is_some() && !params["assertion"].is_null() {
                    let same = |s: &Signed| {
                        matches!(s, Signed::Set { key: k, value: v, until: u, .. }
                            if k == key && *v == value && *u == until)
                    };
                    match self.signed(params, same) {
                        Ok(Signed::Set { current, .. }) => by = ("passkey", Some(current)),
                        Ok(_) => return (vec![error(id, -32602, OTHER_REQUEST)], false),
                        Err(why) => return (vec![error(id, -32602, why)], false),
                    }
                }
                let (tx, rx) = oneshot::channel();
                let req = AttachRequest::SettingsSet {
                    key: key.into(),
                    value,
                    until,
                    channel: by.0,
                    expect: by.1,
                    reply: tx,
                };
                match (self.router.send(req).await, rx.await) {
                    (Ok(()), Ok(value)) => (vec![ok(id, value)], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.decide" => {
                let (Some(n), Some(verb)) = (params["id"].as_i64(), params["verb"].as_str()) else {
                    return (vec![error(id, -32602, "id and verb are required")], false);
                };
                if !matches!(verb, "ok" | "once" | "no" | "cancel") {
                    let why = "verb is ok, once, no or cancel";
                    return (vec![error(id, -32602, why)], false);
                }
                // Every approval made in the web app is signed with a passkey, over this
                // proposal as stored and as shown; it then acts as `passkey`. Refusing and
                // cancelling only take away, so the cookie (`web`) is enough.
                let mut by = (self.channel, None);
                if self.web.is_some() && matches!(verb, "ok" | "once") {
                    if params["assertion"].is_null() {
                        return (vec![error(id, -32602, "approve with your passkey")], false);
                    }
                    let same = |s: &Signed| matches!(s, Signed::Decide { id: i, verb: v, .. } if *i == n && v == verb);
                    match self.signed(params, same) {
                        Ok(Signed::Decide {
                            digest,
                            card_sha256,
                            ..
                        }) => {
                            let expect = Expect {
                                digest,
                                card_sha256,
                            };
                            by = ("passkey", Some(expect));
                        }
                        Ok(_) => return (vec![error(id, -32602, OTHER_REQUEST)], false),
                        Err(why) => return (vec![error(id, -32602, why)], false),
                    }
                }
                let (tx, rx) = oneshot::channel();
                let req = AttachRequest::Decide {
                    id: n,
                    verb: verb.into(),
                    channel: by.0,
                    expect: by.1,
                    reply: tx,
                };
                match (self.router.send(req).await, rx.await) {
                    (Ok(()), Ok(value)) => (vec![ok(id, value)], false),
                    _ => (vec![error(id, -32000, "gateway is shutting down")], true),
                }
            }
            "zephyr.vault_list"
            | "zephyr.vault_put"
            | "zephyr.vault_rm"
            | "zephyr.vault_revoke" => self.vault(id, method, params).await,
            "zephyr.facts_list" | "zephyr.facts_put" | "zephyr.facts_rm" => {
                self.facts(id, method, params)
            }
            "zephyr.desktop" if self.web.is_some() => self.desktop(id, params).await,
            "zephyr.web_devices" if self.web.is_some() => {
                let cfg = self.cfgs.borrow().clone();
                let session = self.web.as_ref().map_or([0; 32], |w| w.0);
                (vec![ok(id, crate::web::devices(&cfg, &session))], false)
            }
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
                let history = match self.router.send(AttachRequest::History(tx)).await {
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
                derived: None,
                sent_at: None,
            };
            // zephyr's words, not the owner's: never through `inbound`, which marks the owner's.
            let _ = self.router.send(AttachRequest::Notice(m)).await;
        }
        let entries: Vec<Value> = vault.entries().iter().map(Entry::public).collect();
        (vec![ok(id, json!({ "entries": entries }))], false)
    }

    /// The owner's private facts (approvals spec §4.5): every call answers with the facts as they
    /// are now, names, kinds and allowed targets, never a value. `facts_put` takes the value, and
    /// it goes nowhere but `facts.toml`: not into an answer, a log or an error. An empty `value`
    /// for a saved name keeps the saved one. The agent is not told of a fact.
    fn facts(&mut self, id: &Value, method: &str, params: &Value) -> Reply {
        use crate::facts::{edit, load, put, remove, Fact, FactKind};
        let path = crate::facts::path(&self.cfgs.borrow());
        let name = params["name"].as_str().unwrap_or_default();
        let done: anyhow::Result<()> = match method {
            "zephyr.facts_put" => {
                let text = |k: &str| params[k].as_str().map(str::to_string);
                // Not serde's message: it can quote the value it did not like.
                match (
                    text("name"),
                    FactKind::parse(&text("kind").unwrap_or_default()),
                ) {
                    (Some(name), Some(kind)) => {
                        let allow: Vec<String> = (params["allow"].as_array().into_iter().flatten())
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        let value = text("value").unwrap_or_default();
                        edit(&path, |facts| {
                            let kept = facts.iter().find(|f| f.name == name);
                            let value = match (value.is_empty(), kept) {
                                (true, Some(old)) => old.value.clone(),
                                _ => value.trim().to_string(),
                            };
                            put(
                                facts,
                                Fact {
                                    name,
                                    kind,
                                    value,
                                    allow,
                                },
                                true,
                            )
                            .map(drop)
                        })
                    }
                    _ => Err(anyhow::anyhow!("the fact is malformed")),
                }
            }
            "zephyr.facts_rm" => edit(&path, |facts| match remove(facts, name) {
                true => Ok(()),
                false => bail!("no fact named {name}"),
            }),
            _ => Ok(()),
        };
        let listed = done.and_then(|()| load(&path));
        match listed {
            Ok(facts) => {
                let facts: Vec<Value> = facts.iter().map(Fact::public).collect();
                (vec![ok(id, json!({ "facts": facts }))], false)
            }
            Err(e) => (vec![error(id, -32602, &format!("{e:#}"))], false),
        }
    }

    /// `zephyr.web_challenge` (DESIGN §4.3.3): what the owner is about to sign in the web app,
    /// `{"op": "decide", "id", "verb": "ok" | "once", "card_sha256"}` or `{"op": "set", "key",
    /// "value", "until"?}`, as a WebAuthn challenge that commits to it: the SHA-256 of
    /// `passkey::preimage`, which holds the proposal's digest and the hash of the card on
    /// screen (which must be the card the router would show now), or the setting's current
    /// value, and a nonce. The challenge is kept for this session, 2 minutes, one use.
    async fn web_challenge(&mut self, id: &Value, params: &Value) -> Reply {
        let refuse = |why: &str| (vec![error(id, -32602, why)], false);
        let gone = || (vec![error(id, -32000, "gateway is shutting down")], true);
        let Some((session, key, hub)) = self.web.clone() else {
            return refuse("not available here");
        };
        if !self.signatures.hit(std::time::Instant::now()) {
            return refuse("too many signatures; wait a minute");
        }
        let signed = match params["op"].as_str() {
            Some("decide") => {
                let (Some(n), Some(verb @ ("ok" | "once")), Some(shown)) = (
                    params["id"].as_i64(),
                    params["verb"].as_str(),
                    params["card_sha256"].as_str(),
                ) else {
                    return refuse("id, verb (ok or once) and card_sha256 are required");
                };
                let (tx, rx) = oneshot::channel();
                let asked = AttachRequest::Card { id: n, reply: tx };
                let card = match (self.router.send(asked).await, rx.await) {
                    (Ok(()), Ok(card)) => card,
                    _ => return gone(),
                };
                let Some((card, digest)) = card else {
                    return refuse(&format!("no proposal #{n} is waiting"));
                };
                // The page and the daemon must agree on what is signed.
                if passkey::card_sha256(&card) != shown {
                    return refuse("the card changed; look at it again");
                }
                Signed::Decide {
                    id: n,
                    verb: verb.into(),
                    digest,
                    card_sha256: shown.into(),
                }
            }
            Some("set") => {
                let (Some(key), Some(value)) =
                    (params["key"].as_str(), value_text(&params["value"]))
                else {
                    return refuse("key and value are required");
                };
                let until = match until_text(&params["until"]) {
                    Ok(until) => until,
                    Err(why) => return refuse(why),
                };
                let (tx, rx) = oneshot::channel();
                let asked = AttachRequest::Current {
                    key: key.into(),
                    reply: tx,
                };
                let current = match (self.router.send(asked).await, rx.await) {
                    (Ok(()), Ok(Ok(current))) => current,
                    (Ok(()), Ok(Err(why))) => return refuse(&why),
                    _ => return gone(),
                };
                Signed::Set {
                    key: key.into(),
                    value,
                    until,
                    current,
                }
            }
            Some("desktop") if params["verb"] == "take" => Signed::Desktop,
            _ => return refuse("op is decide, set or desktop"),
        };
        let cfg = self.cfgs.borrow().clone();
        // Only this session's own passkey may sign for it.
        let own = (crate::store::Store::open(&cfg.db_path()).ok())
            .and_then(|store| store.passkey(key).ok().flatten());
        let (Some(own), Some(web)) = (own, &cfg.web) else {
            return refuse("this session's passkey is gone; sign in again");
        };
        let challenge = passkey::sha256(&passkey::preimage(&signed, &crate::web::random()));
        let b64 = |bytes: &[u8]| data_encoding::BASE64URL_NOPAD.encode(bytes);
        let result = json!({"challengeId": hub.sign(session, signed, challenge), "publicKey": {
            "challenge": b64(&challenge), "rpId": web.host, "timeout": 120_000,
            "userVerification": "required",
            "allowCredentials": [{"type": "public-key", "id": b64(&own.cred_id)}]}});
        (vec![ok(id, result)], false)
    }

    /// `zephyr.desktop` (DESIGN §4.3.3), the web app's alone: `{"verb": "take", challengeId,
    /// assertion}` takes the agent's desktop over and answers the ticket that opens `/api/vnc`.
    /// It is signed with the passkey, over a preimage of its own: the desktop holds the agent's
    /// signed-in browser. `{"verb": "give"}` hands it back, unsigned: that only takes away.
    async fn desktop(&mut self, id: &Value, params: &Value) -> Reply {
        let refuse = |why: &str| (vec![error(id, -32602, why)], false);
        let Some((session, _, hub)) = self.web.clone() else {
            return refuse("not available here");
        };
        match params["verb"].as_str() {
            Some("take") => {
                if params["assertion"].is_null() {
                    return refuse("approve with your passkey");
                }
                if let Err(why) = self.signed(params, |s| *s == Signed::Desktop) {
                    return refuse(why);
                }
                if self.cfgs.borrow().desktop.is_none() {
                    return refuse("there is no desktop");
                }
                match crate::web::vnc::take(&hub, session).await {
                    Ok(ticket) => (vec![ok(id, json!({ "ticket": ticket }))], false),
                    Err(why) => refuse(why),
                }
            }
            Some("give") => {
                hub.desktop.give(crate::now());
                (vec![ok(id, json!({}))], false)
            }
            _ => refuse("verb is take or give"),
        }
    }

    /// What a signed request's `challengeId` was issued for, once its `assertion` verifies
    /// under this session's own passkey. The challenge is used up either way; `same` says
    /// whether it was issued for this request.
    fn signed(
        &self,
        params: &Value,
        same: impl Fn(&Signed) -> bool,
    ) -> Result<Signed, &'static str> {
        let (session, key, hub) = self.web.as_ref().ok_or(NOT_VERIFIED)?;
        let challenge = params["challengeId"].as_str().unwrap_or_default();
        let (signed, challenge) = (hub.signed(challenge, *session))
            .ok_or("that signature expired or was used; tap again")?;
        if !same(&signed) {
            return Err(OTHER_REQUEST);
        }
        let cfg = self.cfgs.borrow().clone();
        let host = cfg
            .web
            .as_ref()
            .map(|w| w.host.as_str())
            .ok_or(NOT_VERIFIED)?;
        let store = crate::store::Store::open(&cfg.db_path()).map_err(|_| NOT_VERIFIED)?;
        let own = (store.passkey(*key).ok().flatten()).ok_or(NOT_VERIFIED)?;
        let a = passkey::assertion(&params["assertion"]).map_err(|_| NOT_VERIFIED)?;
        if a.id != own.cred_id {
            return Err(NOT_VERIFIED);
        }
        let count = passkey::verify(host, &challenge, &own.public_key, own.sign_count, &a)
            .inspect_err(|reason| eprintln!("zephyr: a web app signature was refused: {reason}"))
            .map_err(|_| NOT_VERIFIED)?;
        (store.passkey_used(own.id, count, crate::now())).map_err(|_| NOT_VERIFIED)?;
        Ok(signed)
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
            .send(AttachRequest::History(tx))
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
            channel: self.channel.into(),
            platform_msg_id: next_msg_id(),
            sender: sender(self.channel).into(),
            text: text.into(),
            derived: None,
            sent_at: None,
        };
        let sent = match cmd["followUp"] == true {
            true => self.router.send(AttachRequest::FollowUp(m)).await.is_ok(),
            false => self.inbound.send(m).await.is_ok(),
        };
        if !sent {
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

/// The channel whose rows a decision made on `channel` belongs to: `passkey`, a signed decision
/// in the web app, has no inbox or outbox rows of its own, so its notices are `web`'s.
pub fn rows(channel: &str) -> &str {
    if channel == "passkey" {
        WEB
    } else {
        channel
    }
}

/// Who an attached client's messages are from: the attach token's holder, or the web session's.
pub fn sender(channel: &str) -> &'static str {
    if channel == WEB {
        WEB
    } else {
        "token"
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
async fn read_lines(read: impl AsyncRead + Unpin, tx: mpsc::Sender<Vec<u8>>) {
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
